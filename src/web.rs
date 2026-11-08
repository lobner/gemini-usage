//! Receives the Gemini app's usage windows - the meters on
//! https://gemini.google.com/usage - from the browser extension in
//! `extension/`.
//!
//! The usage page has no token-authenticated API: only the browser's Google
//! login reaches it. So the extension reads it inside the browser, and posts
//! the windows here, to 127.0.0.1. This app never touches Google cookies.
//!
//! The listener takes one request shape - `POST /gemini-web-usage` with a JSON
//! body - and only from a browser extension: a browser sets the Origin header
//! itself, and a web page cannot claim a `chrome-extension://` origin.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;

use crate::env;
use crate::json::nullable;

/// The port the extension posts to. Keep in step with APP_URL in
/// extension/background.js. GEMINI_WEB_PORT overrides it.
pub const DEFAULT_PORT: u16 = 47614;
const PATH: &str = "/gemini-web-usage";
const BODY_LIMIT: usize = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The extension posts once a minute. A reading older than this means the
/// browser is closed, or the extension stopped.
pub const STALE_AFTER: TimeDelta = TimeDelta::minutes(10);

/// One usage window of the Gemini app.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// 0-100 used, rounded, as the usage page shows it.
    pub percent: i64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl Window {
    /// The percentage to show at `now`. A window that has reset since the
    /// reading starts again from 0, so the old number would be wrong.
    pub fn percent_at(&self, now: DateTime<Utc>) -> i64 {
        match self.resets_at {
            Some(t) if t <= now => 0,
            _ => self.percent,
        }
    }
}

/// A good reading: the two windows that the usage page shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// "Current usage": the 5-hour window (GXU_FIVE_HOURLY).
    pub five_hour: Window,
    /// "Weekly limit" (GXU_WEEKLY).
    pub weekly: Window,
    /// The plan as the page names it: "PRO", "ULTRA", "PLUS", or "".
    pub plan: String,
    /// When the extension read it.
    pub read_at: DateTime<Utc>,
}

impl Reading {
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        now - self.read_at > STALE_AFTER
    }
}

/// What the extension has said so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// The last good reading.
    pub reading: Option<Reading>,
    /// The last failure, when the newest report was one.
    pub error: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    reading: None,
    error: None,
});

/// Returns what the extension has said so far.
pub fn state() -> State {
    STATE.lock().unwrap().clone()
}

/// A report: the windows, or why the extension could not read them.
#[derive(Debug, Clone, PartialEq)]
pub enum Report {
    Usage(Reading),
    Failed(String),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Pool {
    #[serde(rename = "poolType", deserialize_with = "nullable")]
    pool_type: String,
    #[serde(rename = "percentageUsed")]
    percentage_used: Option<f64>,
    #[serde(rename = "windowRefreshesAt", deserialize_with = "nullable")]
    window_refreshes_at: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Body {
    #[serde(rename = "fetchedAt", deserialize_with = "nullable")]
    fetched_at: String,
    #[serde(deserialize_with = "nullable")]
    tier: String,
    #[serde(deserialize_with = "nullable")]
    pools: Vec<Pool>,
    #[serde(deserialize_with = "nullable")]
    error: String,
}

/// Reads the extension's JSON. `now` stands in for a missing or bad fetchedAt.
pub fn parse_report(body: &[u8], now: DateTime<Utc>) -> Result<Report, String> {
    let b: Body = serde_json::from_slice(body).map_err(|e| format!("bad JSON: {e}"))?;
    if !b.error.is_empty() {
        return Ok(Report::Failed(b.error));
    }
    let window = |kind: &str| -> Result<Window, String> {
        let p = b
            .pools
            .iter()
            .find(|p| p.pool_type == kind)
            .ok_or_else(|| format!("no {kind} pool"))?;
        let used = p
            .percentage_used
            .filter(|u| u.is_finite() && *u >= 0.0)
            .ok_or_else(|| format!("{kind} has no percentageUsed"))?;
        Ok(Window {
            // The page rounds the fraction the same way: Math.round(n*100).
            percent: ((used * 100.0).round() as i64).clamp(0, 100),
            resets_at: rfc3339(&p.window_refreshes_at),
        })
    };
    Ok(Report::Usage(Reading {
        five_hour: window("GXU_FIVE_HOURLY")?,
        weekly: window("GXU_WEEKLY")?,
        plan: plan_label(&b.tier).to_string(),
        read_at: rfc3339(&b.fetched_at).unwrap_or(now),
    }))
}

fn rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Names the plan as the usage page does.
fn plan_label(tier: &str) -> &'static str {
    match tier {
        "TIER1" => "PRO",
        "TIER2" | "TIER_NEON" => "ULTRA",
        "TIER_HELIUM" => "PLUS",
        _ => "",
    }
}

/// Keeps a report: a good reading replaces the last one, a failure is kept
/// next to it, so the last numbers stay on screen.
fn record(r: Report) {
    let mut st = STATE.lock().unwrap();
    match r {
        Report::Usage(reading) => {
            st.reading = Some(reading);
            st.error = None;
        }
        Report::Failed(e) => st.error = Some(e),
    }
}

/// Starts the listener on its own thread, and calls `on_report` after each
/// report. Returns the error when the port cannot be bound, for example when a
/// second copy of the app already has it.
pub fn start(on_report: impl Fn(&Report) + Send + 'static) -> std::io::Result<()> {
    let port = match env::var("GEMINI_WEB_PORT").trim().parse::<u16>() {
        Ok(p) if p > 0 => p,
        _ => DEFAULT_PORT,
    };
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    thread::spawn(move || serve(listener, on_report));
    Ok(())
}

fn serve(listener: TcpListener, on_report: impl Fn(&Report)) {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        if let Some(r) = handle(stream) {
            record(r.clone());
            on_report(&r);
        }
    }
}

/// Answers one connection. Returns the report it carried, if any.
fn handle(mut stream: TcpStream) -> Option<Report> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let req = read_request(&mut stream);
    let (status, report) = match &req {
        Some(r) => answer(r, Utc::now()),
        None => (400, None),
    };
    let origin = req
        .as_ref()
        .and_then(|r| r.origin.clone())
        .filter(|o| is_extension(o));
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    if let Some(o) = origin {
        head += &format!(
            "Access-Control-Allow-Origin: {o}\r\nAccess-Control-Allow-Methods: POST\r\nAccess-Control-Allow-Headers: Content-Type\r\n"
        );
    }
    head += "Content-Length: 0\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(head.as_bytes());
    report
}

struct Request {
    method: String,
    path: String,
    origin: Option<String>,
    body: Vec<u8>,
}

fn is_extension(origin: &str) -> bool {
    origin.starts_with("chrome-extension://")
}

/// Decides the status code, and reads the report out of a good request.
fn answer(r: &Request, now: DateTime<Utc>) -> (u16, Option<Report>) {
    if r.path != PATH {
        return (404, None);
    }
    if !r.origin.as_deref().is_some_and(is_extension) {
        return (403, None);
    }
    match r.method.as_str() {
        "OPTIONS" => (204, None),
        "POST" => match parse_report(&r.body, now) {
            Ok(report) => (204, Some(report)),
            Err(_) => (400, None),
        },
        _ => (405, None),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

/// Reads a small HTTP/1.1 request. Anything malformed or too big is None.
fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut origin = None;
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        let v = v.trim();
        if k.eq_ignore_ascii_case("content-length") {
            length = v.parse().ok()?;
        } else if k.eq_ignore_ascii_case("origin") {
            origin = Some(v.to_string());
        }
    }
    if length > BODY_LIMIT {
        return None;
    }
    let mut body = vec![0; length];
    r.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        origin,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"fetchedAt":"2026-11-07T17:54:51.000Z","tier":"TIER1","pools":[
        {"poolType":"GXU_FIVE_HOURLY","percentageUsed":0.4249,"windowRefreshesAt":"2026-11-07T21:45:00Z"},
        {"poolType":"GXU_WEEKLY","percentageUsed":0.1,"windowRefreshesAt":"2026-11-10T04:45:00Z"},
        {"poolType":"OVERAGE_CREDITS","percentageUsed":null,"windowRefreshesAt":null}]}"#;

    fn now() -> DateTime<Utc> {
        rfc3339("2026-11-07T18:00:00Z").unwrap()
    }

    #[test]
    fn parses_a_good_report() {
        let Ok(Report::Usage(r)) = parse_report(GOOD.as_bytes(), now()) else {
            panic!("not a reading");
        };
        assert_eq!(r.five_hour.percent, 42);
        assert_eq!(r.weekly.percent, 10);
        assert_eq!(r.five_hour.resets_at, rfc3339("2026-11-07T21:45:00Z"));
        assert_eq!(r.plan, "PRO");
        assert_eq!(r.read_at, rfc3339("2026-11-07T17:54:51Z").unwrap());
        assert!(!r.is_stale(now()));
        assert!(r.is_stale(now() + TimeDelta::minutes(10)));
    }

    #[test]
    fn a_window_that_has_reset_shows_zero() {
        let Ok(Report::Usage(r)) = parse_report(GOOD.as_bytes(), now()) else {
            panic!("not a reading");
        };
        // Stale, but neither window has reset yet: keep the last numbers.
        let later = rfc3339("2026-11-07T21:44:00Z").unwrap();
        assert_eq!(r.five_hour.percent_at(later), 42);
        assert_eq!(r.weekly.percent_at(later), 10);
        // The 5-hour window reset at 21:45; the weekly one did not.
        let after = rfc3339("2026-11-07T21:45:00Z").unwrap();
        assert_eq!(r.five_hour.percent_at(after), 0);
        assert_eq!(r.weekly.percent_at(after), 10);
        let no_reset = Window {
            percent: 7,
            resets_at: None,
        };
        assert_eq!(no_reset.percent_at(after), 7);
    }

    #[test]
    fn parses_failures_and_rejects_partial_reports() {
        assert_eq!(
            parse_report(br#"{"error":"not signed in"}"#, now()),
            Ok(Report::Failed("not signed in".into()))
        );
        let no_weekly = r#"{"pools":[{"poolType":"GXU_FIVE_HOURLY","percentageUsed":0.5}]}"#;
        assert!(parse_report(no_weekly.as_bytes(), now()).is_err());
        let negative = r#"{"pools":[{"poolType":"GXU_FIVE_HOURLY","percentageUsed":-1},{"poolType":"GXU_WEEKLY","percentageUsed":0}]}"#;
        assert!(parse_report(negative.as_bytes(), now()).is_err());
        assert!(parse_report(b"nope", now()).is_err());
    }

    #[test]
    fn answers_only_extension_posts_to_the_path() {
        let req = |method: &str, path: &str, origin: Option<&str>, body: &str| Request {
            method: method.into(),
            path: path.into(),
            origin: origin.map(String::from),
            body: body.as_bytes().to_vec(),
        };
        let ext = Some("chrome-extension://abcdef");
        assert!(matches!(
            answer(&req("POST", PATH, ext, GOOD), now()),
            (204, Some(Report::Usage(_)))
        ));
        assert_eq!(answer(&req("OPTIONS", PATH, ext, ""), now()), (204, None));
        assert_eq!(
            answer(
                &req("POST", PATH, Some("https://evil.example"), GOOD),
                now()
            ),
            (403, None)
        );
        assert_eq!(answer(&req("POST", PATH, None, GOOD), now()), (403, None));
        assert_eq!(
            answer(&req("POST", "/other", ext, GOOD), now()),
            (404, None)
        );
        assert_eq!(answer(&req("GET", PATH, ext, ""), now()), (405, None));
        assert_eq!(answer(&req("POST", PATH, ext, "{"), now()), (400, None));
    }

    /// A real POST over TCP, as the extension sends it.
    #[test]
    fn serves_a_post_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || handle(listener.accept().unwrap().0));

        let mut c = TcpStream::connect(addr).unwrap();
        write!(
            c,
            "POST {PATH} HTTP/1.1\r\nHost: {addr}\r\nOrigin: chrome-extension://abc\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{GOOD}",
            GOOD.len()
        )
        .unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 204 "), "{resp}");
        assert!(
            resp.contains("Access-Control-Allow-Origin: chrome-extension://abc"),
            "{resp}"
        );
        assert!(matches!(server.join().unwrap(), Some(Report::Usage(_))));
    }
}
