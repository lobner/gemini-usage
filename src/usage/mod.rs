//! Gets the Gemini quota windows that Antigravity CLI (`agy`) shows in its
//! /usage panel - a 5-hour and a weekly limit for each model group - from
//! Google's Cloud Code API, with agy's stored OAuth token.
//!
//! It is READ-ONLY: it reads the token from the macOS Keychain (or the token
//! file of the Antigravity app) but never refreshes or writes it. The access
//! token lasts approximately one hour, and only running `agy` refreshes it.
//! After it has expired, `fetch` returns `Kind::TokenExpired` until the next
//! refresh.
//!
//! # Why Antigravity
//!
//! Google moved Gemini CLI to Antigravity CLI on 2026-06-18. Since then the
//! free tier ("Gemini Code Assist for individuals") and Google AI Pro/Ultra no
//! longer serve Gemini CLI: loadCodeAssist answers UNSUPPORTED_CLIENT. The
//! quota of the same Google account is now behind agy, on daily-cloudcode-pa.
//! The older cloudcode-pa host is reported to always say 100% for the 5-hour
//! window, so do not change hosts without a check.
//!
//! # Why not gemini.google.com/usage
//!
//! The consumer usage page has no token-authenticated API. The Gemini web app
//! serves it, with whole-account Google session cookies for authentication.
//! Those cookies rotate every few minutes, and this app would have to lift them
//! out of the browser's cookie jar and keep them alive. This app deliberately
//! does not go there.
//!
//! There is no officially documented API for this data. The endpoint is the
//! same internal one that agy calls (v1internal:retrieveUserQuotaSummary).

mod apikey;
mod monitoring;

use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use wait_timeout::ChildExt;

use crate::duration;
use crate::env;
use crate::http;
use crate::json::nullable;

pub use apikey::{KeyUsage, fetch_api_key, forget_api_key};

/// The host that agy uses. CLOUDCODE_ENDPOINT overrides it, so a build can
/// point at a fixture without a new compile.
const DEFAULT_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
const API_VERSION: &str = "v1internal";

const METHOD_LOAD_CODE_ASSIST: &str = "loadCodeAssist";
const METHOD_QUOTA_SUMMARY: &str = "retrieveUserQuotaSummary";

/// agy's Keychain item, as its Go keyring library writes it.
const KEYCHAIN_SERVICE: &str = "gemini";
const KEYCHAIN_ACCOUNT: &str = "antigravity";

/// Goes in the User-Agent when the installed agy does not answer --version.
/// Google answers loadCodeAssist without a project unless the User-Agent names
/// Antigravity.
const FALLBACK_AGY_VERSION: &str = "1.3.5";

const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
const BODY_LIMIT: u64 = 1 << 20; // 1 MiB

/// What kind of failure an [`Error`] is. It survives wrapping, as Go's
/// `errors.Is` sees through `%w`.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// agy has never been logged in on this Mac, so there is nothing to read.
    NoCredentials,
    /// The stored OAuth token is expired or was rejected. Callers keep the last
    /// known meters and tell the user to refresh it (run `agy`). It is not a
    /// hard error.
    TokenExpired,
    /// The account has no Antigravity quota to read. The detail is Google's
    /// reason (from loadCodeAssist's ineligibleTiers or a 403's ErrorInfo
    /// reason), when it gave one.
    NotEligible(Option<NotEligible>),
    /// The endpoint refused the request because it was called too often. This
    /// is rare, so it is a transient fault: keep the last known meters and back
    /// off. `retry_after` is zero when the server said nothing useful.
    RateLimited { retry_after: Duration },
    /// GEMINI_API_KEY is in neither the app's environment nor the login shell.
    NoApiKey,
    /// Google rejected the API key (API_KEY_INVALID).
    KeyInvalid,
    /// There are no usable gcloud Application Default Credentials, which the
    /// Cloud Monitoring query needs.
    NoAdc,
    /// Any other failure.
    Other,
}

/// Google's reason for offering the account no quota.
#[derive(Debug, Clone, PartialEq)]
pub struct NotEligible {
    /// For example UNSUPPORTED_CLIENT, SUBSCRIPTION_REQUIRED.
    pub code: String,
    pub message: String,
}

/// A failure, with its kind and the text Go's error chain would print.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub kind: Kind,
    msg: String,
}

impl Error {
    fn new(kind: Kind, msg: impl Into<String>) -> Self {
        Error {
            kind,
            msg: msg.into(),
        }
    }

    fn other(msg: impl Into<String>) -> Self {
        Error::new(Kind::Other, msg)
    }

    fn token_expired() -> Self {
        Error::new(Kind::TokenExpired, "oauth token expired")
    }

    fn not_eligible(detail: Option<NotEligible>) -> Self {
        let msg = match &detail {
            Some(ne) => format!("{NOT_ELIGIBLE}: {} ({})", ne.message, ne.code),
            None => NOT_ELIGIBLE.to_string(),
        };
        Error::new(Kind::NotEligible(detail), msg)
    }

    fn rate_limited(retry_after: Duration) -> Self {
        let msg = if retry_after.is_zero() {
            "rate limited".to_string()
        } else {
            format!(
                "rate limited, retry after {}",
                duration::go_string(retry_after)
            )
        };
        Error::new(Kind::RateLimited { retry_after }, msg)
    }

    fn no_adc(detail: &str) -> Self {
        Error::new(Kind::NoAdc, format!("{NO_ADC}: {detail}"))
    }

    /// Puts `prefix: ` in front of the text and keeps the kind, as Go's
    /// `fmt.Errorf("prefix: %w", err)` does.
    fn wrap(self, prefix: &str) -> Self {
        Error::new(self.kind, format!("{prefix}: {}", self.msg))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

const NOT_ELIGIBLE: &str = "no Antigravity quota for this account";
const NO_ADC: &str = "no gcloud application-default credentials";

/// One quota window of one model group.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meter {
    /// The display label: "Gemini 5h", "Gemini weekly", ...
    pub name: String,
    /// The model group: "Gemini", "Claude and GPT".
    pub group: String,
    /// "5h" or "weekly".
    pub window: String,
    /// The bucketId it came from, for the tooltip.
    pub bucket: String,
    /// 0-100 used, rounded.
    pub percent: i64,
    /// When the window resets, if known.
    pub resets_at: Option<DateTime<Utc>>,
    /// The window has not started: nothing is used, and the reset is one
    /// window length from now. Google moves such a reset forward on every call,
    /// so it is not a real time to show.
    pub idle: bool,
}

/// The meters that the menu bar shows: the model groups in the order Google
/// returns them (Gemini first), and the 5-hour window before the weekly one in
/// each.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub tiers: Vec<Meter>,
}

impl Usage {
    /// Returns the meter for a display name, if the response had one.
    #[cfg(test)]
    pub fn tier(&self, name: &str) -> Option<&Meter> {
        self.tiers
            .iter()
            .find(|m| m.name.to_lowercase() == name.to_lowercase())
    }

    /// Picks the two meters in the menu-bar title: the 5-hour and weekly
    /// windows of the Gemini group, or else of the first group.
    pub fn headline(&self) -> Vec<&Meter> {
        let Some(first) = self.tiers.first() else {
            return Vec::new();
        };
        let group = self
            .tiers
            .iter()
            .find(|m| m.bucket.starts_with("gemini-"))
            .map_or(&first.group, |m| &m.group);
        self.tiers
            .iter()
            .filter(|m| &m.group == group)
            .take(2)
            .collect()
    }
}

/// The project id that loadCodeAssist returns, kept between polls. It costs a
/// second round trip and does not change for the life of a login, so it is
/// resolved once and used again. A 401 clears it, with everything else.
static PROJECT_CACHE: Mutex<String> = Mutex::new(String::new());

/// Reads agy's stored OAuth token and queries the quota summary. All the work
/// stops at `deadline`.
pub fn fetch(deadline: Instant) -> Result<Usage, Error> {
    let cred = load_credentials(deadline)?;
    // Do not hammer the endpoint with a known-dead token: if it has already
    // expired locally, report it without a doomed request. A live 401 is still
    // handled in post, in case the expiry is missing or wrong.
    if cred.expires_at.is_some_and(|t| Utc::now() > t) {
        return Err(Error::token_expired());
    }

    let project = resolve_project(&cred.access_token, deadline)?;

    match post(
        &cred.access_token,
        METHOD_QUOTA_SUMMARY,
        &json!({ "project": project }),
        deadline,
    ) {
        Ok(body) => parse_summary(&body),
        Err(err) => {
            // A stale cached project id outlives its login. Drop it, so the
            // next poll resolves a new one, not the same doomed request.
            if matches!(err.kind, Kind::TokenExpired | Kind::NotEligible(_)) {
                forget_project();
            }
            Err(err)
        }
    }
}

/// Returns the project id of the signed-in account, from the cache or from
/// loadCodeAssist.
fn resolve_project(token: &str, deadline: Instant) -> Result<String, Error> {
    let cached = PROJECT_CACHE.lock().unwrap().clone();
    if !cached.is_empty() {
        return Ok(cached);
    }

    // ideType makes this an Antigravity call. Together with the User-Agent it
    // decides which tiers Google offers.
    let body = post(
        token,
        METHOD_LOAD_CODE_ASSIST,
        &json!({
            "metadata": {
                "ideType": "ANTIGRAVITY",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
            }
        }),
        deadline,
    )?;
    let id = parse_load_code_assist(&body)?;
    *PROJECT_CACHE.lock().unwrap() = id.clone();
    Ok(id)
}

/// Returns the project, or NotEligible with Google's reason when there is none.
fn parse_load_code_assist(body: &[u8]) -> Result<String, Error> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Tier {
        #[serde(rename = "reasonCode", deserialize_with = "nullable")]
        reason_code: String,
        #[serde(rename = "reasonMessage", deserialize_with = "nullable")]
        reason_message: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Response {
        #[serde(rename = "cloudaicompanionProject", deserialize_with = "nullable")]
        project: String,
        #[serde(rename = "ineligibleTiers", deserialize_with = "nullable")]
        ineligible: Vec<Tier>,
    }

    let r: Response = serde_json::from_slice(body)
        .map_err(|e| Error::other(format!("parsing loadCodeAssist JSON: {e}")))?;
    let id = r.project.trim();
    if !id.is_empty() {
        return Ok(id.to_string());
    }
    if let Some(t) = r.ineligible.into_iter().next() {
        return Err(Error::not_eligible(Some(NotEligible {
            code: t.reason_code,
            message: t.reason_message,
        })));
    }
    Err(Error::not_eligible(None))
}

fn forget_project() {
    PROJECT_CACHE.lock().unwrap().clear();
}

fn post(
    token: &str,
    method: &str,
    payload: &serde_json::Value,
    deadline: Instant,
) -> Result<Vec<u8>, Error> {
    // serde_json sorts object keys, as Go's json.Marshal does for a map.
    let buf = serde_json::to_vec(payload).map_err(|e| Error::other(e.to_string()))?;
    let mut resp = http::agent()
        .post(method_url(method))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent())
        .config()
        .timeout_global(Some(http::timeout(CLIENT_TIMEOUT, deadline)))
        .build()
        .send(&buf[..])
        .map_err(|e| Error::other(format!("{method} request: {e}")))?;

    let code = resp.status().as_u16();
    let retry = header(&resp, "Retry-After");
    let body = http::read_limited(resp.body_mut(), BODY_LIMIT).unwrap_or_default();

    match code {
        401 => Err(Error::token_expired()),
        // 403 has two meanings: an expired token and an account with no quota
        // both land on it, and the two need different advice in the menu, so
        // the body decides.
        403 => match not_eligible(&body) {
            Some(ne) => Err(Error::not_eligible(Some(ne))),
            None => Err(Error::token_expired()),
        },
        429 => Err(Error::rate_limited(retry_after(&retry))),
        200 => Ok(body),
        _ => Err(Error::other(format!(
            "{method} HTTP {code}: {}",
            snippet(&body)
        ))),
    }
}

fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn method_url(method: &str) -> String {
    let endpoint = env::trimmed_or("CLOUDCODE_ENDPOINT", DEFAULT_ENDPOINT);
    format!(
        "{}/{API_VERSION}:{method}",
        endpoint.strip_suffix('/').unwrap_or(&endpoint)
    )
}

/// Finds the 403s that mean the account has no quota here - no licence, a
/// disabled API, no project - and not a stale token.
fn not_eligible(body: &[u8]) -> Option<NotEligible> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Detail {
        #[serde(deserialize_with = "nullable")]
        reason: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Inner {
        #[serde(deserialize_with = "nullable")]
        status: String,
        #[serde(deserialize_with = "nullable")]
        message: String,
        #[serde(deserialize_with = "nullable")]
        details: Vec<Detail>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Response {
        #[serde(deserialize_with = "nullable")]
        error: Inner,
    }

    let r: Response = serde_json::from_slice(body).ok()?;
    if r.error.status != "PERMISSION_DENIED" {
        return None;
    }
    if let Some(d) = r
        .error
        .details
        .iter()
        .find(|d| d.reason == "SUBSCRIPTION_REQUIRED")
    {
        return Some(NotEligible {
            code: d.reason.clone(),
            message: r.error.message,
        });
    }
    let msg = r.error.message.to_lowercase();
    let hints = [
        "has not been used",
        "is disabled",
        "not eligible",
        "valid license",
    ];
    if hints.iter().any(|h| msg.contains(h)) {
        return Some(NotEligible {
            code: r.error.status,
            message: r.error.message,
        });
    }
    None
}

/// Reads the header in either of its forms - a delay in seconds, or an HTTP
/// date - and returns zero for anything else.
fn retry_after(h: &str) -> Duration {
    let h = h.trim();
    if h.is_empty() {
        return Duration::ZERO;
    }
    if let Ok(secs) = h.parse::<i64>() {
        return if secs <= 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(secs as u64)
        };
    }
    if let Ok(t) = httpdate::parse_http_date(h)
        && let Ok(d) = t.duration_since(SystemTime::now())
    {
        return d;
    }
    Duration::ZERO
}

// ---- the User-Agent ----

/// Names Antigravity CLI and its installed version, as agy itself does.
fn user_agent() -> String {
    static VERSION: OnceLock<String> = OnceLock::new();
    let v = VERSION.get_or_init(installed_agy_version);
    format!("antigravity/{v} {}", crate::platform())
}

/// Asks agy for its version once. A GUI app gets a minimal PATH, so
/// ~/.local/bin (where agy's installer puts it) is tried explicitly. AGY_BIN
/// overrides both.
fn installed_agy_version() -> String {
    let mut candidates = vec![env::var("AGY_BIN")];
    let home = env::var("HOME");
    if !home.is_empty() {
        candidates.push(format!("{home}/.local/bin/agy"));
    }
    if let Some(p) = look_path("agy") {
        candidates.push(p);
    }
    for bin in candidates.iter().filter(|b| !b.is_empty()) {
        let mut cmd = Command::new(bin);
        cmd.arg("--version");
        if let Some(out) = output_by(cmd, Instant::now() + Duration::from_secs(5)) {
            let v = parse_agy_version(&String::from_utf8_lossy(&out));
            if !v.is_empty() {
                return v;
            }
        }
    }
    FALLBACK_AGY_VERSION.into()
}

/// Finds an executable in PATH, as Go's exec.LookPath does. Relative entries
/// are skipped, as Go refuses a result in the current directory.
fn look_path(name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    env::var("PATH")
        .split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| format!("{dir}/{name}"))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Takes the first word that looks like a version number.
fn parse_agy_version(out: &str) -> String {
    out.split_whitespace()
        .map(|f| f.strip_prefix('v').unwrap_or(f))
        .find(|f| f.starts_with(|c: char| c.is_ascii_digit()) && f.contains('.'))
        .unwrap_or_default()
        .to_string()
}

// ---- quota parsing ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct SummaryBucket {
    #[serde(rename = "bucketId", deserialize_with = "nullable")]
    bucket_id: String,
    #[serde(deserialize_with = "nullable")]
    window: String,
    #[serde(rename = "resetTime", deserialize_with = "nullable")]
    reset_time: String,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SummaryGroup {
    #[serde(rename = "displayName", deserialize_with = "nullable")]
    display_name: String,
    #[serde(deserialize_with = "nullable")]
    buckets: Vec<SummaryBucket>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SummaryResponse {
    #[serde(deserialize_with = "nullable")]
    groups: Vec<SummaryGroup>,
}

fn parse_summary(body: &[u8]) -> Result<Usage, Error> {
    parse_summary_at(body, Utc::now())
}

fn parse_summary_at(body: &[u8], now: DateTime<Utc>) -> Result<Usage, Error> {
    let r: SummaryResponse = serde_json::from_slice(body)
        .map_err(|e| Error::other(format!("parsing quota JSON: {e}")))?;

    let mut u = Usage::default();
    for g in &r.groups {
        let group = group_name(&g.display_name);
        let mut meters: Vec<Meter> = g
            .buckets
            .iter()
            .filter_map(|b| Some(to_meter(&group, b, b.remaining_fraction?, now)))
            .collect();
        // The API lists weekly first. The shorter window is the one that runs
        // out first, so it leads. sort_by_key is stable, as Go's SliceStable.
        meters.sort_by_key(|m| window_rank(&m.window));
        u.tiers.extend(meters);
    }
    if u.tiers.is_empty() {
        return Err(Error::other("quota response carried no usable buckets"));
    }
    Ok(u)
}

/// Shortens "Gemini Models" to "Gemini" and "Claude and GPT models" to
/// "Claude and GPT".
fn group_name(display: &str) -> String {
    let mut s = display.trim();
    for suffix in [" models", " Models"] {
        s = s.strip_suffix(suffix).unwrap_or(s);
    }
    if s.is_empty() {
        "Models".into()
    } else {
        s.to_string()
    }
}

/// How long a window runs after it has started, or zero for an unknown window.
pub fn window_length(w: &str) -> Duration {
    match w {
        "5h" => Duration::from_secs(5 * 3600),
        "weekly" => Duration::from_secs(7 * 24 * 3600),
        _ => Duration::ZERO,
    }
}

/// How far the reset of an idle window can be from "now + length": Google
/// stamps it when it answers, a moment before now.
const IDLE_SLACK: Duration = Duration::from_secs(120);

fn window_rank(w: &str) -> u8 {
    match w {
        "5h" => 0,
        "weekly" => 1,
        _ => 2,
    }
}

/// Converts the *remaining* fraction of a bucket into a *used* percentage,
/// which is the direction every other meter in this app reads.
fn to_meter(group: &str, b: &SummaryBucket, remaining: f64, now: DateTime<Utc>) -> Meter {
    let window = if b.window.is_empty() {
        b.bucket_id.clone()
    } else {
        b.window.clone()
    };
    // f64::round rounds halfway values away from zero, as Go's math.Round.
    let percent = (((1.0 - remaining) * 100.0).round() as i64).clamp(0, 100);
    let resets_at = if b.reset_time.is_empty() {
        None
    } else {
        DateTime::parse_from_rfc3339(&b.reset_time)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    };

    let mut idle = false;
    let len = window_length(&window);
    if percent == 0
        && let Some(reset) = resets_at
        && !len.is_zero()
    {
        let expected = now + chrono::TimeDelta::from_std(len).unwrap_or_default();
        let slack = chrono::TimeDelta::from_std(IDLE_SLACK).unwrap_or_default();
        let d = reset - expected;
        idle = d > -slack && d < slack;
    }

    Meter {
        name: format!("{group} {window}"),
        group: group.to_string(),
        window,
        bucket: b.bucket_id.clone(),
        percent,
        resets_at,
        idle,
    }
}

fn snippet(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    let s = s.trim();
    if s.len() <= 200 {
        return s.to_string();
    }
    let mut end = 200;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ---- processes ----

/// Runs `cmd` and returns its stdout when it exits with success before
/// `deadline`. It is stopped at the deadline.
fn output_by(cmd: Command, deadline: Instant) -> Option<Vec<u8>> {
    match run_by(cmd, deadline) {
        Some((true, out)) => Some(out),
        _ => None,
    }
}

/// Runs `cmd`, stops it at `deadline`, and returns whether it exited with
/// success together with all of its stdout, as Go's `Output()` does. Stdout is
/// read on a second thread while the process runs: a process with more output
/// than the pipe holds otherwise blocks on its write, and never exits. Stdin
/// and stderr go to /dev/null.
fn run_by(mut cmd: Command, deadline: Instant) -> Option<(bool, Vec<u8>)> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });

    let success = match child.wait_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status.success(),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
    };
    // The pipe closes when the process ends, so this returns.
    let out = reader.join().ok()?;
    Some((success, out))
}

// ---- credentials (read-only) ----

struct Credentials {
    access_token: String,
    expires_at: Option<DateTime<Utc>>,
}

/// The shape agy keeps in the Keychain and the Antigravity app in its token
/// file. Only the necessary fields are read, and deliberately not
/// refresh_token.
#[derive(Deserialize, Default)]
#[serde(default)]
struct CredBlob {
    #[serde(deserialize_with = "nullable")]
    token: TokenBlob,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenBlob {
    #[serde(deserialize_with = "nullable")]
    access_token: String,
    /// RFC 3339.
    #[serde(deserialize_with = "nullable")]
    expiry: String,
}

/// The token file of the Antigravity app, the fallback when the Keychain has
/// nothing usable. Empty when there is no home directory.
pub fn token_file_path() -> String {
    let home = env::var("HOME");
    if home.is_empty() {
        return String::new();
    }
    format!("{home}/.gemini/jetski-standalone-oauth-token")
}

/// Reads agy's OAuth token from the Keychain and the token file of the
/// Antigravity app, and keeps the one that lasts longer: both apps share one
/// Google login, and the one that ran last has the new token.
fn load_credentials(deadline: Instant) -> Result<Credentials, Error> {
    let mut best: Option<Credentials> = None;
    let mut consider = |c: Option<Credentials>| {
        let Some(c) = c else { return };
        // A missing expiry sorts before every real one, as Go's zero time.
        if best.as_ref().is_none_or(|b| c.expires_at > b.expires_at) {
            best = Some(c);
        }
    };
    if let Some(raw) = keychain_read(deadline) {
        consider(parse_blob(&decode_keyring(&raw)));
    }
    let path = token_file_path();
    if !path.is_empty()
        && let Ok(data) = std::fs::read(&path)
    {
        consider(parse_blob(&data));
    }
    best.ok_or_else(|| {
        Error::new(
            Kind::NoCredentials,
            format!(
                "no Antigravity CLI credentials (Keychain item {KEYCHAIN_SERVICE:?} / {KEYCHAIN_ACCOUNT:?} or {}); log in with `agy` first",
                token_file_path()
            ),
        )
    })
}

/// Runs the system `security` tool. The absolute path is used because an app
/// started from Finder or at login gets a minimal PATH.
fn keychain_read(deadline: Instant) -> Option<Vec<u8>> {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args([
        "find-generic-password",
        "-s",
        KEYCHAIN_SERVICE,
        "-a",
        KEYCHAIN_ACCOUNT,
        "-w",
    ]);
    output_by(cmd, deadline)
}

/// Undoes the encodings of the Go keyring library: a "go-keyring-base64:" or
/// "go-keyring-encoded:" (hex) prefix, or none.
fn decode_keyring(raw: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(raw);
    let s = s.trim();
    if let Some(v) = s.strip_prefix("go-keyring-base64:")
        && let Ok(b) = base64::engine::general_purpose::STANDARD.decode(v)
    {
        return b;
    }
    if let Some(v) = s.strip_prefix("go-keyring-encoded:")
        && let Some(b) = decode_hex(v)
    {
        return b;
    }
    s.as_bytes().to_vec()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn parse_blob(data: &[u8]) -> Option<Credentials> {
    let b: CredBlob = serde_json::from_slice(data).ok()?;
    if b.token.access_token.is_empty() {
        return None;
    }
    Some(Credentials {
        access_token: b.token.access_token,
        expires_at: DateTime::parse_from_rfc3339(&b.token.expiry)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
    })
}

/// A lock for the tests that share the process-wide caches in this module.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    /// A trimmed live retrieveUserQuotaSummary answer, with some use added.
    /// Google lists the weekly window before the 5-hour one.
    const SUMMARY_SAMPLE: &str = r#"{
  "groups": [
    {"displayName": "Gemini Models", "description": "Models within this group: Gemini Flash, Gemini Pro",
     "buckets": [
       {"bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly", "resetTime": "2026-10-22T14:46:35Z", "remainingFraction": 0.9},
       {"bucketId": "gemini-5h", "displayName": "Five Hour Limit Remaining", "window": "5h", "resetTime": "2026-10-15T19:46:35Z", "remainingFraction": 0.375}
     ]},
    {"displayName": "Claude and GPT models",
     "buckets": [
       {"bucketId": "3p-weekly", "window": "weekly", "resetTime": "2026-10-22T14:46:35Z", "remainingFraction": 1},
       {"bucketId": "3p-5h", "window": "5h", "resetTime": "2026-10-15T19:46:35Z", "remainingFraction": 1}
     ]}
  ],
  "description": "Within each group, models share a weekly limit and a 5-hour limit."
}"#;

    fn bucket(window: &str, reset: &str) -> SummaryBucket {
        SummaryBucket {
            bucket_id: "gemini-5h".into(),
            window: window.into(),
            reset_time: reset.into(),
            remaining_fraction: None,
        }
    }

    #[test]
    fn parse_summary_orders_groups_and_windows() {
        let u = parse_summary(SUMMARY_SAMPLE.as_bytes()).expect("parse");
        let names: Vec<&str> = u.tiers.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Gemini 5h",
                "Gemini weekly",
                "Claude and GPT 5h",
                "Claude and GPT weekly"
            ]
        );
        let m = u.tier("Gemini 5h").unwrap();
        assert!(
            m.percent == 63 && m.bucket == "gemini-5h" && m.resets_at.is_some(),
            "{m:?}"
        );
        assert_eq!(u.tier("Gemini weekly").unwrap().percent, 10);
    }

    #[test]
    fn headline_is_gemini_windows() {
        let u = parse_summary(SUMMARY_SAMPLE.as_bytes()).unwrap();
        let h: Vec<&str> = u.headline().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(h, ["Gemini 5h", "Gemini weekly"]);

        // Without a Gemini group, the first group stands in.
        let meter = |name: &str, group: &str, bucket: &str| Meter {
            name: name.into(),
            group: group.into(),
            bucket: bucket.into(),
            ..Default::default()
        };
        let other = Usage {
            tiers: vec![
                meter("X 5h", "X", "x-5h"),
                meter("X weekly", "X", "x-weekly"),
                meter("Y 5h", "Y", "y-5h"),
            ],
        };
        let h = other.headline();
        assert!(h.len() == 2 && h.iter().all(|m| m.group == "X"), "{h:?}");
        assert!(Usage::default().headline().is_empty());
    }

    #[test]
    fn to_meter_inverts_rounds_and_clamps() {
        for (remaining, want) in [(1.0, 0), (0.0, 100), (0.554, 45), (-0.2, 100), (1.3, 0)] {
            let b = bucket("5h", "");
            assert_eq!(
                to_meter("Gemini", &b, remaining, Utc::now()).percent,
                want,
                "remaining {remaining}"
            );
        }
    }

    #[test]
    fn idle_windows_are_spotted() {
        let now = DateTime::parse_from_rfc3339("2026-10-15T14:46:36Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = |secs: i64| {
            (now + chrono::TimeDelta::seconds(secs))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        };
        let cases = [
            ("unused 5h", "5h", 1.0, at(5 * 3600 - 1), true),
            ("unused weekly", "weekly", 1.0, at(7 * 24 * 3600), true),
            (
                "used: the reset is real",
                "5h",
                0.9,
                at(5 * 3600 - 1),
                false,
            ),
            ("0% but started earlier", "5h", 1.0, at(3 * 3600), false),
            ("unknown window", "daily", 1.0, at(24 * 3600), false),
        ];
        for (name, window, remaining, reset, want) in cases {
            let b = bucket(window, &reset);
            assert_eq!(to_meter("Gemini", &b, remaining, now).idle, want, "{name}");
        }
    }

    #[test]
    fn parse_summary_rejects_empty() {
        assert!(parse_summary(br#"{"groups":[]}"#).is_err());
        assert!(parse_summary(b"nope").is_err());
    }

    #[test]
    fn group_names() {
        assert_eq!(group_name("Gemini Models"), "Gemini");
        assert_eq!(group_name("Claude and GPT models"), "Claude and GPT");
        assert_eq!(group_name(""), "Models");
    }

    #[test]
    fn decode_keyring_and_parse_blob() {
        let blob = r#"{"auth_method":"consumer","id_token":"x","token":{"access_token":"ya29.test","expiry":"2026-10-15T17:42:51.28174+02:00","refresh_token":"r","token_type":"Bearer"}}"#;
        let hex: String = blob.bytes().map(|b| format!("{b:02x}")).collect();
        let cases = [
            (
                "base64",
                format!("go-keyring-base64:{}", STANDARD.encode(blob)),
            ),
            ("hex", format!("go-keyring-encoded:{hex}")),
            ("plain", format!("{blob}\n")),
        ];
        let want = DateTime::parse_from_rfc3339("2026-10-15T15:42:51.28174Z")
            .unwrap()
            .with_timezone(&Utc);
        for (name, raw) in cases {
            let c = parse_blob(&decode_keyring(raw.as_bytes())).expect(name);
            assert_eq!(c.access_token, "ya29.test", "{name}");
            assert_eq!(c.expires_at, Some(want), "{name}");
        }
        assert!(
            parse_blob(br#"{"token":{}}"#).is_none(),
            "no access token must not parse"
        );
    }

    #[test]
    fn not_eligible_spots_licence_403s() {
        let licence = br#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"You do not have a valid license of this product.",
	  "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SUBSCRIPTION_REQUIRED"}]}}"#;
        let ne = not_eligible(licence).expect("licence 403");
        assert_eq!(ne.code, "SUBSCRIPTION_REQUIRED");
        let disabled = br#"{"error":{"status":"PERMISSION_DENIED","message":"Cloud Code Private API has not been used in project 1234"}}"#;
        assert!(
            not_eligible(disabled).is_some(),
            "a disabled API is not eligible"
        );
        let stale = br#"{"error":{"status":"PERMISSION_DENIED","message":"The caller does not have permission"}}"#;
        assert!(not_eligible(stale).is_none(), "a stale token");
    }

    #[test]
    fn parse_agy_versions() {
        assert_eq!(parse_agy_version("1.3.5\n"), "1.3.5");
        assert_eq!(parse_agy_version("agy version v1.4.0 (x)"), "1.4.0");
        assert_eq!(parse_agy_version("no version here"), "");
    }

    #[test]
    fn method_url_honours_env_override() {
        env::overrides::set("CLOUDCODE_ENDPOINT", "http://127.0.0.1:9/");
        assert_eq!(
            method_url(METHOD_QUOTA_SUMMARY),
            "http://127.0.0.1:9/v1internal:retrieveUserQuotaSummary"
        );
    }

    #[test]
    fn rate_limited_text() {
        let err = Error::rate_limited(Duration::from_secs(5));
        assert!(matches!(err.kind, Kind::RateLimited { .. }));
        assert!(err.to_string().contains("5s"), "{err}");
        assert_eq!(
            Error::rate_limited(Duration::ZERO).to_string(),
            "rate limited"
        );
        // Wrapping keeps the kind, as errors.Is sees through %w.
        let wrapped = err.wrap("reading the request count");
        assert!(matches!(wrapped.kind, Kind::RateLimited { .. }));
    }

    #[test]
    fn parse_load_code_assist_reports_why_not_eligible() {
        let body = br#"{"allowedTiers":[{"id":"standard-tier","userDefinedCloudaicompanionProject":true}],
	  "ineligibleTiers":[{"reasonCode":"UNSUPPORTED_CLIENT","reasonMessage":"This client is no longer supported for Gemini Code Assist for individuals.","tierId":"free-tier"}]}"#;
        let err = parse_load_code_assist(body).unwrap_err();
        let Kind::NotEligible(Some(ne)) = &err.kind else {
            panic!("err = {err}, want NotEligible UNSUPPORTED_CLIENT");
        };
        assert_eq!(ne.code, "UNSUPPORTED_CLIENT");
        assert!(err.to_string().starts_with(NOT_ELIGIBLE));
        assert_eq!(
            parse_load_code_assist(br#"{"cloudaicompanionProject":"p-1"}"#),
            Ok("p-1".into())
        );
        assert_eq!(
            parse_load_code_assist(b"{}").unwrap_err().kind,
            Kind::NotEligible(None)
        );
    }

    #[test]
    fn retry_after_forms() {
        assert_eq!(retry_after("30"), Duration::from_secs(30));
        assert_eq!(retry_after("0"), Duration::ZERO);
        assert_eq!(retry_after(""), Duration::ZERO);
        let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(90));
        assert!(retry_after(&future) > Duration::ZERO);
        let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(3600));
        assert_eq!(retry_after(&past), Duration::ZERO);
    }

    /// More output than a pipe holds (64 KiB) must not block the process until
    /// the deadline.
    #[test]
    fn output_by_drains_large_output() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "head -c 300000 /dev/zero"]);
        let start = Instant::now();
        let out = output_by(cmd, start + Duration::from_secs(5)).expect("output");
        assert_eq!(out.len(), 300_000);
        assert!(start.elapsed() < Duration::from_secs(4));
    }

    /// Pins the two requests against a local server: the paths, the headers
    /// and the exact JSON bodies that Go's json.Marshal sent, and the 401/403
    /// answers.
    #[test]
    fn quota_requests_match_go() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let srv = crate::testserver::serve(|req| {
            assert_eq!(req.header("Authorization").as_deref(), Some("Bearer tok"));
            assert_eq!(
                req.header("Content-Type").as_deref(),
                Some("application/json")
            );
            let ua = req.header("User-Agent").unwrap_or_default();
            assert!(ua.starts_with("antigravity/"), "User-Agent {ua:?}");
            let body = String::from_utf8_lossy(&req.body).into_owned();
            match req.target.as_str() {
                "/v1internal:loadCodeAssist" => {
                    assert_eq!(
                        body,
                        r#"{"metadata":{"ideType":"ANTIGRAVITY","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#
                    );
                    (200, r#"{"cloudaicompanionProject":"p-1"}"#.into())
                }
                "/v1internal:retrieveUserQuotaSummary" => {
                    assert_eq!(body, r#"{"project":"p-1"}"#);
                    (200, SUMMARY_SAMPLE.into())
                }
                other => panic!("unexpected request {other}"),
            }
        });
        env::overrides::set("CLOUDCODE_ENDPOINT", &format!("{srv}/"));
        forget_project();

        let deadline = Instant::now() + Duration::from_secs(10);
        let project = resolve_project("tok", deadline).expect("project");
        assert_eq!(project, "p-1");
        assert_eq!(
            *PROJECT_CACHE.lock().unwrap(),
            "p-1",
            "the project is cached"
        );
        let body = post(
            "tok",
            METHOD_QUOTA_SUMMARY,
            &json!({ "project": project }),
            deadline,
        )
        .expect("summary");
        assert_eq!(parse_summary(&body).unwrap().tiers.len(), 4);
        forget_project();
    }

    #[test]
    fn post_reads_401_and_403() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let srv = crate::testserver::serve(|req| {
            match req.target.as_str() {
            "/v1internal:a" => (401, "{}".into()),
            "/v1internal:b" => (
                403,
                r#"{"error":{"status":"PERMISSION_DENIED","message":"You do not have a valid license.","details":[{"reason":"SUBSCRIPTION_REQUIRED"}]}}"#.into(),
            ),
            "/v1internal:c" => (403, r#"{"error":{"status":"PERMISSION_DENIED","message":"no"}}"#.into()),
            _ => (500, "  boom  ".into()),
        }
        });
        env::overrides::set("CLOUDCODE_ENDPOINT", &srv);
        let deadline = Instant::now() + Duration::from_secs(10);
        let kind = |m: &str| post("t", m, &json!({}), deadline).unwrap_err();

        assert_eq!(kind("a").kind, Kind::TokenExpired);
        let b = kind("b");
        assert!(
            matches!(&b.kind, Kind::NotEligible(Some(ne)) if ne.code == "SUBSCRIPTION_REQUIRED"),
            "{b:?}"
        );
        assert_eq!(
            b.to_string(),
            "no Antigravity quota for this account: You do not have a valid license. (SUBSCRIPTION_REQUIRED)"
        );
        assert_eq!(
            kind("c").kind,
            Kind::TokenExpired,
            "a plain 403 is a stale token"
        );
        assert_eq!(kind("d").to_string(), "d HTTP 500: boom");
    }

    #[test]
    fn output_by_stops_at_the_deadline() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("10");
        let start = Instant::now();
        assert!(output_by(cmd, start + Duration::from_millis(300)).is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// Diagnostic: queries the live quota summary with agy's stored OAuth token
    /// and prints every meter. Opt-in, because it needs a valid `agy` login:
    ///
    ///     cargo test live_fetch -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_fetch() {
        let u = match fetch(Instant::now() + Duration::from_secs(25)) {
            Ok(u) => u,
            Err(e) if !matches!(e.kind, Kind::Other | Kind::RateLimited { .. }) => {
                println!("skipped: {e}");
                return;
            }
            Err(e) => panic!("fetch: {e}"),
        };
        assert!(!u.tiers.is_empty(), "no tiers");
        for m in &u.tiers {
            println!(
                "{:<22} = {:>3}% used  (bucket {}, resets {:?}, idle {})",
                m.name, m.percent, m.bucket, m.resets_at, m.idle
            );
            assert!((0..=100).contains(&m.percent));
        }
        println!(
            "headline: {:?}",
            u.headline().iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }
}
