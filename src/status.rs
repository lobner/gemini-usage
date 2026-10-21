//! Reads Google's public Workspace Status Dashboard
//! (<https://www.google.com/appsstatus/dashboard/>) through its JSON incident
//! feed, and reports whether anything is going on with Gemini.
//!
//! The feed is a flat array of the most recent incidents across every Workspace
//! product, open and closed. So this module does two things that the Statuspage
//! equivalent got for free: it filters to the Gemini products, and it decides
//! which incidents are still running. An incident is ongoing exactly when it has
//! no "end" timestamp. There is no per-incident resolved/investigating state
//! machine to read (every update in the feed is stamped "AVAILABLE").
//!
//! No authentication is necessary. This is the same JSON that the public
//! dashboard itself renders.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

use crate::http;
use crate::json::nullable;

/// The Workspace Status Dashboard's incident feed.
pub const DEFAULT_URL: &str = "https://www.google.com/appsstatus/dashboard/incidents.json";

/// Resolves the relative "uri" of an incident ("incidents/<id>") to a page that
/// a browser can open.
const DASHBOARD_BASE: &str = "https://www.google.com/appsstatus/dashboard/";

const USER_AGENT: &str = "gemini-usage/1.0 (+https://github.com/lobner/gemini-usage)";

const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);
// The feed carries the recent history of every product and is a few hundred
// kilobytes, so the cap is larger than a Statuspage summary needs.
const BODY_LIMIT: u64 = 8 << 20; // 8 MiB

/// An ongoing disruption that affects a Gemini product.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // updated_at and maintenance are parsed for parity, not shown
pub struct Incident {
    pub id: String,
    pub name: String,
    /// The impact class, for example "SERVICE_DISRUPTION".
    pub status: String,
    /// "none", "minor", "major", "critical".
    pub impact: String,
    /// The permalink on the dashboard.
    pub url: String,
    /// The body of the newest update, if any.
    pub latest_update: String,
    pub updated_at: Option<DateTime<FixedOffset>>,
    /// Informational notices, not a disruption.
    pub maintenance: bool,
}

/// A single affected Gemini product (for example "Gemini", "Gemini Notebook").
#[derive(Debug, Clone, Default)]
pub struct Component {
    pub name: String,
    /// "SERVICE_DISRUPTION", "SERVICE_OUTAGE", ...
    pub status: String,
}

impl Component {
    /// Reports whether this component is in its normal state. The feed only
    /// lists a product because something is wrong with it, so anything named
    /// here is by definition not operational.
    #[cfg(test)]
    pub fn operational(&self) -> bool {
        self.status.is_empty()
    }
}

/// The parsed dashboard: the headline indicator plus the detail that the
/// dropdown shows.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// "none", "minor", "major", "critical".
    pub indicator: String,
    /// For example "All Systems Operational".
    pub description: String,
    pub incidents: Vec<Incident>,
    pub affected: Vec<Component>,
}

impl Summary {
    /// Reports whether the dashboard says Gemini is fine. An empty indicator
    /// counts as fine, so an unexpected payload never raises a false alarm.
    pub fn all_operational(&self) -> bool {
        self.indicator.is_empty() || self.indicator == "none"
    }
}

/// Gets and parses the incident feed from `url` (use [`DEFAULT_URL`]).
pub fn fetch(url: &str, deadline: Instant) -> Result<Summary, String> {
    let mut resp = http::agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .config()
        .timeout_global(Some(http::timeout(CLIENT_TIMEOUT, deadline)))
        .build()
        .call()
        .map_err(|e| format!("status request: {e}"))?;

    if resp.status().as_u16() != 200 {
        return Err(format!("status dashboard HTTP {}", resp.status().as_u16()));
    }
    let body = http::read_limited(resp.body_mut(), BODY_LIMIT)
        .map_err(|e| format!("reading status dashboard: {e}"))?;
    parse(&body)
}

// Every field is optional and may be null, as with Go's encoding/json.

#[derive(Deserialize, Default)]
#[serde(default)]
struct Update {
    #[serde(deserialize_with = "nullable")]
    text: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Product {
    #[serde(deserialize_with = "nullable")]
    title: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IncidentJson {
    #[serde(deserialize_with = "nullable")]
    id: String,
    #[serde(deserialize_with = "nullable")]
    service_name: String,
    #[serde(deserialize_with = "nullable")]
    affected_products: Vec<Product>,
    /// SERVICE_OUTAGE / _DISRUPTION / _INFORMATION.
    #[serde(deserialize_with = "nullable")]
    status_impact: String,
    #[serde(deserialize_with = "nullable")]
    begin: String,
    /// Empty while the incident is running.
    #[serde(deserialize_with = "nullable")]
    end: String,
    #[serde(deserialize_with = "nullable")]
    modified: String,
    #[serde(deserialize_with = "nullable")]
    uri: String,
    #[serde(deserialize_with = "nullable")]
    external_desc: String,
    most_recent_update: Option<Update>,
}

fn parse(body: &[u8]) -> Result<Summary, String> {
    // Go reads a JSON null into a nil slice without an error.
    let raw: Option<Vec<IncidentJson>> =
        serde_json::from_slice(body).map_err(|e| format!("parsing status JSON: {e}"))?;
    let raw = raw.unwrap_or_default();

    let mut s = Summary {
        indicator: "none".into(),
        description: "All Systems Operational".into(),
        ..Default::default()
    };

    // Deduplicate the affected products across incidents, and keep the worst
    // impact of each, so a product hit by two incidents is listed once.
    let mut affected: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for inc in &raw {
        if !is_ongoing(inc) {
            continue;
        }
        let names = gemini_products(inc);
        if names.is_empty() {
            continue;
        }

        let i = to_incident(inc);
        if rank(&i.impact) > rank(&s.indicator) {
            s.indicator = i.impact.clone();
        }
        s.incidents.push(i);

        for n in names {
            let prev = affected.get(&n).cloned();
            if prev.is_none() {
                order.push(n.clone());
            }
            if prev
                .as_deref()
                .is_none_or(|p| impact_rank(&inc.status_impact) > impact_rank(p))
            {
                affected.insert(n, inc.status_impact.clone());
            }
        }
    }

    if !s.all_operational() {
        s.description = describe(&order, &s.indicator);
    }
    for n in order {
        let status = affected.get(&n).cloned().unwrap_or_default();
        s.affected.push(Component { name: n, status });
    }
    Ok(s)
}

/// Reports whether an incident is still running. The feed marks the end of an
/// incident by filling in "end". While it is live, that field is absent or
/// empty.
fn is_ongoing(inc: &IncidentJson) -> bool {
    inc.end.trim().is_empty()
}

/// Returns the Gemini-branded products that an incident affects. The match is
/// on the product title, not the service key, so "Gemini", "Gemini Notebook"
/// and whatever Google brands next are all found, and a "Multiple Products"
/// incident is caught by its product list, not by its service name.
fn gemini_products(inc: &IncidentJson) -> Vec<String> {
    let mut out: Vec<String> = inc
        .affected_products
        .iter()
        .map(|p| p.title.trim())
        .filter(|t| is_gemini(t))
        .map(String::from)
        .collect();
    // Some records carry only a service name. Do not lose those.
    if out.is_empty() && is_gemini(&inc.service_name) {
        out.push(inc.service_name.trim().to_string());
    }
    out
}

fn is_gemini(name: &str) -> bool {
    name.to_lowercase().contains("gemini")
}

fn to_incident(inc: &IncidentJson) -> Incident {
    let mut i = Incident {
        id: inc.id.trim().to_string(),
        name: headline(&inc.external_desc),
        status: inc.status_impact.trim().to_string(),
        impact: impact(&inc.status_impact).to_string(),
        maintenance: inc.status_impact.trim() == "SERVICE_INFORMATION",
        ..Default::default()
    };
    if i.name.is_empty() {
        i.name = format!("{} incident", inc.service_name.trim());
    }
    let uri = inc.uri.trim();
    if !uri.is_empty() {
        i.url = format!("{DASHBOARD_BASE}{}", uri.strip_prefix('/').unwrap_or(uri));
    }
    if let Some(u) = &inc.most_recent_update {
        i.latest_update = collapse_space(&strip_markdown(&u.text));
    }
    i.updated_at = [&inc.modified, &inc.begin]
        .iter()
        .find_map(|ts| DateTime::parse_from_rfc3339(ts.trim()).ok());
    i
}

/// Takes a one-line name out of external_desc, which is a small Markdown
/// document that starts with a "**Title**" or "**Summary**" heading followed by
/// the sentence we want.
fn headline(desc: &str) -> String {
    let first = desc
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // Skip the bold section headings ("**Title**", "**Description**", ...).
        .filter(|line| !(line.starts_with("**") && line.ends_with("**")))
        .find(|line| !line.starts_with('#'))
        .unwrap_or_default();
    collapse_space(&strip_markdown(first))
}

/// Removes the little formatting the feed uses, so a menu row shows text, not
/// asterisks and hashes.
fn strip_markdown(s: &str) -> String {
    let s = s.replace("**", "").replace("##", "");
    let s = s.trim();
    s.strip_prefix('#').unwrap_or(s).trim().to_string()
}

/// Maps the dashboard's status_impact onto the four-level scale that the menu
/// already speaks.
fn impact(status_impact: &str) -> &'static str {
    match status_impact.trim() {
        "SERVICE_OUTAGE" => "critical",
        "SERVICE_DISRUPTION" => "major",
        "SERVICE_INFORMATION" => "minor",
        // An unknown impact still counts as something, not nothing: a new
        // class that Google invents must not read as "all clear".
        _ => "minor",
    }
}

fn rank(indicator: &str) -> u8 {
    match indicator {
        "critical" => 3,
        "major" => 2,
        "minor" => 1,
        _ => 0,
    }
}

fn impact_rank(status_impact: &str) -> u8 {
    rank(impact(status_impact))
}

/// Writes the headline that the menu shows next to the dot. It names the
/// products, not a generic phrase.
fn describe(products: &[String], indicator: &str) -> String {
    let noun = match indicator {
        "critical" => "service outage",
        "minor" => "service notice",
        _ => "service disruption",
    };
    match products.len() {
        0 => format!("Gemini {noun}"),
        _ => format!("{} {noun}", products.join(", ")),
    }
}

/// Flattens an update body to a single line, so that it fits a menu row or a
/// tooltip.
fn collapse_space(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turns a dashboard machine value ("SERVICE_DISRUPTION") into a display label
/// ("Service disruption").
pub fn human(s: &str) -> String {
    let s = s.replace('_', " ");
    let mut s = s.trim().to_string();
    if s.is_empty() {
        return String::new();
    }
    // The feed shouts its enums in upper case. Lower them before the first
    // letter is made capital, or the menu row shouts too.
    if s == s.to_uppercase() {
        s = s.to_lowercase();
    }
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Shaped after the real feed: a mix of ongoing and ended incidents, Gemini
    /// and non-Gemini products, and a "Multiple Products" record that shows its
    /// Gemini involvement only through affected_products.
    const FEED_SAMPLE: &str = r###"[
  {
    "id": "ongoing-outage",
    "service_name": "Gemini",
    "affected_products": [{"title": "Gemini", "id": "npdyhgECDJ6tB66MxXyo"}],
    "severity": "high",
    "status_impact": "SERVICE_OUTAGE",
    "begin": "2026-08-17T06:00:00+00:00",
    "modified": "2026-08-17T07:15:00+00:00",
    "uri": "incidents/ongoing-outage",
    "external_desc": "**Title**\nGemini is unavailable for most users.\n**Description**\nWe are investigating.",
    "most_recent_update": {"status": "AVAILABLE", "when": "2026-08-17T07:15:00+00:00", "text": "## Summary\nWe are still\ninvestigating the issue."}
  },
  {
    "id": "ongoing-notebook",
    "service_name": "Gemini Notebook",
    "affected_products": [{"title": "Gemini Notebook", "id": "x"}],
    "severity": "low",
    "status_impact": "SERVICE_INFORMATION",
    "begin": "2026-08-17T05:00:00+00:00",
    "modified": "2026-08-17T05:30:00+00:00",
    "uri": "incidents/ongoing-notebook",
    "external_desc": "**Summary**\nSome NotebookLM exports are delayed."
  },
  {
    "id": "ended-gemini",
    "service_name": "Gemini",
    "affected_products": [{"title": "Gemini", "id": "npdyhgECDJ6tB66MxXyo"}],
    "severity": "medium",
    "status_impact": "SERVICE_DISRUPTION",
    "begin": "2026-06-10T10:20:00+00:00",
    "end": "2026-06-10T17:30:00+00:00",
    "uri": "incidents/ended-gemini",
    "external_desc": "**Title**\nAn old, resolved problem."
  },
  {
    "id": "ongoing-gmail",
    "service_name": "Gmail",
    "affected_products": [{"title": "Gmail", "id": "y"}],
    "severity": "high",
    "status_impact": "SERVICE_OUTAGE",
    "begin": "2026-08-17T04:00:00+00:00",
    "uri": "incidents/ongoing-gmail",
    "external_desc": "**Title**\nGmail is down."
  },
  {
    "id": "ongoing-multi",
    "service_name": "Multiple Products",
    "affected_products": [{"title": "Google Drive", "id": "z"}, {"title": "Gemini", "id": "npdyhgECDJ6tB66MxXyo"}],
    "severity": "medium",
    "status_impact": "SERVICE_DISRUPTION",
    "begin": "2026-08-17T03:00:00+00:00",
    "uri": "incidents/ongoing-multi",
    "external_desc": "**Title**\nSign-in is slow across several products."
  }
]"###;

    fn sample() -> Summary {
        parse(FEED_SAMPLE.as_bytes()).expect("parse")
    }

    #[test]
    fn parse_keeps_only_ongoing_gemini_incidents() {
        let s = sample();
        let got: HashSet<&str> = s.incidents.iter().map(|i| i.id.as_str()).collect();
        for want in ["ongoing-outage", "ongoing-notebook", "ongoing-multi"] {
            assert!(
                got.contains(want),
                "expected incident {want:?} to be listed"
            );
        }
        for unwanted in ["ended-gemini", "ongoing-gmail"] {
            assert!(
                !got.contains(unwanted),
                "{unwanted:?} should be filtered out"
            );
        }
    }

    #[test]
    fn parse_takes_the_worst_indicator() {
        let s = sample();
        assert_eq!(s.indicator, "critical", "the outage wins");
        assert!(!s.all_operational());
        assert!(s.description.contains("outage"), "{:?}", s.description);
    }

    #[test]
    fn parse_reports_all_clear_when_nothing_is_running() {
        let body = r#"[
	  {"id":"a","service_name":"Gemini","affected_products":[{"title":"Gemini"}],
	   "status_impact":"SERVICE_OUTAGE","begin":"2026-01-01T00:00:00+00:00",
	   "end":"2026-01-01T01:00:00+00:00","uri":"incidents/a","external_desc":"**Title**\nOver."}
	]"#;
        let s = parse(body.as_bytes()).expect("parse");
        assert!(s.all_operational(), "indicator = {:?}", s.indicator);
        assert_eq!(s.description, "All Systems Operational");
        assert!(s.incidents.is_empty() && s.affected.is_empty());
    }

    #[test]
    fn parse_deduplicates_affected_products() {
        let s = sample();
        let gemini = s.affected.iter().filter(|c| c.name == "Gemini").count();
        assert_eq!(gemini, 1, "Gemini listed {gemini} times");
        // Gemini is in an outage and a disruption. The worse one must stick.
        for c in &s.affected {
            if c.name == "Gemini" {
                assert_eq!(c.status, "SERVICE_OUTAGE");
            }
            assert!(!c.operational(), "{:?} is affected but operational", c.name);
        }
        assert!(!s.affected.iter().any(|c| c.name == "Google Drive"));
    }

    #[test]
    fn incident_fields_are_renderable() {
        let s = sample();
        let outage = s
            .incidents
            .iter()
            .find(|i| i.id == "ongoing-outage")
            .expect("ongoing-outage");
        assert_eq!(outage.name, "Gemini is unavailable for most users.");
        assert_eq!(
            outage.url,
            format!("{DASHBOARD_BASE}incidents/ongoing-outage")
        );
        assert_eq!(outage.impact, "critical");
        assert!(outage.updated_at.is_some(), "expected modified to parse");
        // The update body is multi-line Markdown. It must collapse to one line.
        assert!(
            !outage.latest_update.contains(['\n', '#']),
            "{:?}",
            outage.latest_update
        );
        assert!(outage.latest_update.contains("still investigating"));
    }

    #[test]
    fn information_only_incident_is_marked_maintenance() {
        let s = sample();
        let n = s
            .incidents
            .iter()
            .find(|i| i.id == "ongoing-notebook")
            .expect("ongoing-notebook");
        assert!(n.maintenance, "SERVICE_INFORMATION is a notice");
        assert_eq!(n.impact, "minor");
    }

    #[test]
    fn headline_skips_markdown_headings() {
        let cases = [
            (
                "**Title**\nThe real headline.\n**Description**\nMore.",
                "The real headline.",
            ),
            ("**Summary**\nAnother   headline.", "Another headline."),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(headline(input), want, "headline({input:?})");
        }
    }

    #[test]
    fn unknown_impact_is_never_all_clear() {
        assert_ne!(impact("SERVICE_SOMETHING_NEW"), "none");
    }

    #[test]
    fn human_lowercases_shouted_enums() {
        let cases = [
            ("SERVICE_DISRUPTION", "Service disruption"),
            ("SERVICE_OUTAGE", "Service outage"),
            ("in_progress", "In progress"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(human(input), want);
        }
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse(br#"{"not":"an array"}"#).is_err());
    }

    /// Go reads null into the zero value. One null field must not fail the
    /// feed.
    #[test]
    fn parse_tolerates_nulls() {
        let body = r#"[{"id":"n","service_name":"Gemini","affected_products":null,
            "status_impact":"SERVICE_DISRUPTION","end":null,"uri":null,
            "external_desc":null,"most_recent_update":null}]"#;
        let s = parse(body.as_bytes()).expect("parse");
        assert_eq!(s.incidents.len(), 1);
        assert_eq!(s.incidents[0].name, "Gemini incident");
        assert_eq!(s.incidents[0].url, "");
        assert!(parse(b"null").expect("null").all_operational());
    }

    /// testdata/incidents.json is a trimmed capture of the real feed. One
    /// genuine Gemini incident has had its "end" stamp removed, so the file
    /// exercises the ongoing path. Everything else - the Markdown in
    /// external_desc, the affected_products shape, the shouted enums - is
    /// untouched.
    ///
    /// The hand-written sample above pins the behaviour. This pins it against
    /// the payload that Google actually sends, which is what breaks when the
    /// shape of the feed drifts.
    #[test]
    fn parse_real_feed_capture() {
        let body = include_bytes!("../testdata/incidents.json");
        let s = parse(body).expect("parse");

        assert!(
            !s.all_operational(),
            "the doctored incident must register, indicator {:?}",
            s.indicator
        );
        assert_eq!(s.incidents.len(), 1, "exactly the one with no end stamp");

        let inc = &s.incidents[0];
        assert!(
            !inc.name.is_empty(),
            "no headline from the real external_desc"
        );
        // The real external_desc is Markdown with "**Title**" headings. None of
        // that may reach a menu row.
        assert!(!inc.name.contains(['*', '#', '\n']), "{:?}", inc.name);
        assert!(inc.url.starts_with(DASHBOARD_BASE), "{:?}", inc.url);
        assert!(inc.updated_at.is_some());
        assert!(!inc.impact.is_empty());

        // Only Gemini products may be listed, and each exactly once.
        assert!(!s.affected.is_empty());
        let mut seen = HashSet::new();
        for c in &s.affected {
            assert!(is_gemini(&c.name), "non-Gemini product {:?}", c.name);
            assert!(seen.insert(c.name.clone()), "{:?} listed twice", c.name);
        }
    }

    /// Diagnostic: queries the live feed and prints what the menu would show.
    /// Opt-in, because it needs network access. STATUS_URL points it at a
    /// doctored feed served locally:
    ///
    ///     cargo test live_status -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_status() {
        let url = std::env::var("STATUS_URL").unwrap_or_else(|_| DEFAULT_URL.into());
        let s = fetch(&url, Instant::now() + Duration::from_secs(20)).expect("fetch");
        println!(
            "indicator = {:?} ({}), all_operational = {}",
            s.indicator,
            s.description,
            s.all_operational()
        );
        for inc in &s.incidents {
            println!(
                "incident: [{}] {} - {}",
                human(&inc.status),
                inc.name,
                inc.url
            );
        }
        for c in &s.affected {
            println!("affected: {} ({})", c.name, human(&c.status));
        }
        assert!(!s.description.is_empty(), "expected a status description");
    }
}
