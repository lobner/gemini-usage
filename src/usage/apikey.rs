//! The Gemini API key: a second source, next to the Antigravity quota. When a
//! GEMINI_API_KEY is found, `fetch_api_key` checks it and reads the request
//! count of its project for the day from Cloud Monitoring. Monitoring needs an
//! OAuth token, so this also reads the user's gcloud Application Default
//! Credentials (ADC).
//!
//! It stays READ-ONLY: the key stays in memory and is never logged, and the
//! ADC refresh token is exchanged for an access token in memory only. Nothing
//! is ever written back. Unlike agy's Keychain token, nothing else rotates the
//! ADC refresh token, so an in-memory exchange cannot race an owner.

use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::monitoring::{quota_day_start, requests_today};
use super::{BODY_LIMIT, CLIENT_TIMEOUT, Error, Kind, header, retry_after, run_by, snippet};
use crate::env;
use crate::http;
use crate::json::nullable;

/// Bounds the login-shell lookup of GEMINI_API_KEY.
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long "no key" is believed before the login shell is asked again, so a
/// Mac with no key does not start a shell on every poll.
const MISSING_KEY_RECHECK: Duration = Duration::from_secs(10 * 60);

/// What is known about the use of the Gemini API key today.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyUsage {
    /// The Cloud project that owns the key.
    pub project: String,
    /// generateContent requests since `since`.
    pub requests: i64,
    /// The start of the quota day, midnight Pacific.
    pub since: DateTime<Utc>,
}

/// The API key and what is known about it between polls. A rejected key is
/// dropped, so the next poll reads it again and finds a fix in ~/.zshrc
/// without a restart.
struct KeyState {
    key: String,
    /// Google accepted the key at least once.
    checked: bool,
    /// The project of the key, after it is resolved.
    project: String,
    /// No key was found. Do not look again before this.
    missing_until: Option<Instant>,
    /// The last key read came from the process environment.
    last_from_env: bool,
    /// Google rejected the key from the environment. Skip it.
    env_rejected: bool,
}

static KEY_STATE: Mutex<KeyState> = Mutex::new(KeyState {
    key: String::new(),
    checked: false,
    project: String::new(),
    missing_until: None,
    last_from_env: false,
    env_rejected: false,
});

/// The endpoints, so tests can point them at a local server.
#[derive(Clone, Copy)]
pub(super) enum Endpoint {
    GeminiBase,
    Token,
    ApiKeys,
    Monitoring,
}

fn endpoint(e: Endpoint) -> String {
    #[cfg(test)]
    if let Some(v) = tests::endpoint_override(e) {
        return v;
    }
    match e {
        Endpoint::GeminiBase => env::trimmed_or(
            "GOOGLE_GEMINI_BASE_URL",
            "https://generativelanguage.googleapis.com",
        ),
        Endpoint::Token => "https://oauth2.googleapis.com/token".into(),
        Endpoint::ApiKeys => "https://apikeys.googleapis.com".into(),
        Endpoint::Monitoring => "https://monitoring.googleapis.com".into(),
    }
}

pub(super) fn monitoring_base_url() -> String {
    endpoint(Endpoint::Monitoring)
}

/// Drops everything known about the key, so the next `fetch_api_key` reads it
/// again. "Refresh now" calls it.
pub fn forget_api_key() {
    let mut st = KEY_STATE.lock().unwrap();
    st.key.clear();
    st.checked = false;
    st.project.clear();
    st.missing_until = None;
}

/// Checks the Gemini API key and reads the request count of its project for
/// today. It returns `Kind::NoApiKey` when there is no key, which is normal:
/// the caller then hides the key row.
pub fn fetch_api_key(deadline: Instant) -> Result<KeyUsage, Error> {
    let mut key = api_key(deadline)?;
    if let Err(err) = check_key_once(&key, deadline) {
        // An app started from a terminal gets the environment of that
        // terminal, which can hold an old key long after ~/.zshrc got a new
        // one. Before giving up, try the key of the login shell.
        if err.kind != Kind::KeyInvalid || !reject_env_key() {
            return Err(err);
        }
        key = api_key(deadline)?;
        check_key_once(&key, deadline)?;
    }
    let tok = adc_access_token(deadline)?;
    let project = key_project(&key, &tok, deadline)?;
    let now = Utc::now();
    let since = quota_day_start(now);
    let requests = requests_today(&tok, &project, since, now, deadline)?;
    Ok(KeyUsage {
        project,
        requests,
        since,
    })
}

// ---- the key ----

/// Marks the key from the environment as rejected, so `api_key` asks the login
/// shell from now on. It reports whether the rejected key came from the
/// environment, which is when a second try can help.
fn reject_env_key() -> bool {
    let mut st = KEY_STATE.lock().unwrap();
    if !st.last_from_env || st.env_rejected {
        return false;
    }
    st.env_rejected = true;
    true
}

/// Returns the Gemini API key: from the environment first, as Gemini CLI reads
/// it, then from the login shell. A bundle started from Finder or at login
/// does not get ~/.zshrc, which is where the key usually is.
fn api_key(deadline: Instant) -> Result<String, Error> {
    let (key, missing_until, skip_env) = {
        let st = KEY_STATE.lock().unwrap();
        (st.key.clone(), st.missing_until, st.env_rejected)
    };
    if !key.is_empty() {
        return Ok(key);
    }
    if missing_until.is_some_and(|t| Instant::now() < t) {
        return Err(no_api_key());
    }

    let mut key = String::new();
    let mut from_env = false;
    if !skip_env {
        for name in ["GEMINI_API_KEY", "GOOGLE_API_KEY"] {
            let v = env::var(name);
            let v = v.trim();
            if !v.is_empty() {
                key = v.to_string();
                from_env = true;
                break;
            }
        }
    }
    if key.is_empty() {
        key = login_shell_var("GEMINI_API_KEY", deadline);
    }
    if key.is_empty() {
        KEY_STATE.lock().unwrap().missing_until = Some(Instant::now() + MISSING_KEY_RECHECK);
        return Err(no_api_key());
    }

    let mut st = KEY_STATE.lock().unwrap();
    st.key = key.clone();
    st.last_from_env = from_env;
    Ok(key)
}

fn no_api_key() -> Error {
    Error::new(Kind::NoApiKey, "GEMINI_API_KEY not found")
}

/// Fences the value in the output of the login shell, so text that ~/.zshrc
/// prints by itself does not get mixed into the key.
const SHELL_MARK: &str = "\x1fgemini-usage\x1f";

/// Reads one variable from an interactive login shell, or returns "" when it
/// is not set or the shell does not answer in time.
fn login_shell_var(name: &str, deadline: Instant) -> String {
    let mut shell = env::var("SHELL");
    if shell.is_empty() {
        shell = "/bin/zsh".into();
    }
    let deadline = deadline.min(Instant::now() + SHELL_TIMEOUT);

    // The name goes in through the environment, not the script text.
    let script = r#"eval "printf '%s%s%s' \"$GU_MARK\" \"\${$GU_NAME}\" \"$GU_MARK\"""#;
    let mut cmd = Command::new(&shell);
    cmd.args(["-lic", script])
        .env("GU_MARK", SHELL_MARK)
        .env("GU_NAME", name);
    // Stdin is /dev/null and stderr is dropped. The exit status does not
    // matter: the marks decide.
    match run_by(cmd, deadline) {
        Some((_, out)) => between_marks(&String::from_utf8_lossy(&out)),
        None => String::new(),
    }
}

fn between_marks(out: &str) -> String {
    let Some((_, rest)) = out.split_once(SHELL_MARK) else {
        return String::new();
    };
    let Some((v, _)) = rest.split_once(SHELL_MARK) else {
        return String::new();
    };
    v.trim().to_string()
}

/// Asks Google whether it accepts the key, with the cheapest call that needs
/// one, until it has said yes once.
fn check_key_once(key: &str, deadline: Instant) -> Result<(), Error> {
    if KEY_STATE.lock().unwrap().checked {
        return Ok(());
    }

    let base = endpoint(Endpoint::GeminiBase);
    let url = format!(
        "{}/v1beta/models?pageSize=1",
        base.strip_suffix('/').unwrap_or(&base)
    );
    let mut resp = http::agent()
        .get(&url)
        // A header keeps the key out of URLs and errors.
        .header("x-goog-api-key", key)
        .config()
        .timeout_global(Some(http::timeout(CLIENT_TIMEOUT, deadline)))
        .build()
        .call()
        .map_err(|e| {
            Error::other(format!(
                "checking the API key: {}",
                transport_error(&e, &[key])
            ))
        })?;
    let code = resp.status().as_u16();
    let retry = header(&resp, "Retry-After");
    let body = http::read_limited(resp.body_mut(), BODY_LIMIT).unwrap_or_default();

    if code == 200 {
        KEY_STATE.lock().unwrap().checked = true;
        return Ok(());
    }
    if is_key_invalid(&body) {
        forget_api_key();
        return Err(Error::new(Kind::KeyInvalid, "gemini API key not valid"));
    }
    if code == 429 {
        return Err(Error::rate_limited(retry_after(&retry)));
    }
    Err(Error::other(format!(
        "checking the API key: HTTP {code}: {}",
        snippet(&body)
    )))
}

/// Finds Google's "API key not valid" error, by its reason code or, if that
/// is missing, its message.
fn is_key_invalid(body: &[u8]) -> bool {
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
    let Ok(r) = serde_json::from_slice::<Response>(body) else {
        return false;
    };
    r.error
        .details
        .iter()
        .any(|d| d.reason == "API_KEY_INVALID")
        || r.error.message.to_lowercase().contains("api key not valid")
}

/// Returns the Cloud project that owns the key: GOOGLE_CLOUD_PROJECT when set,
/// else the lookupKey method of the API Keys API.
fn key_project(key: &str, tok: &AdcToken, deadline: Instant) -> Result<String, Error> {
    let v = env::var("GOOGLE_CLOUD_PROJECT");
    if !v.trim().is_empty() {
        return Ok(v.trim().to_string());
    }
    let cached = KEY_STATE.lock().unwrap().project.clone();
    if !cached.is_empty() {
        return Ok(cached);
    }

    let url = format!(
        "{}/v2/keys:lookupKey?keyString={}",
        endpoint(Endpoint::ApiKeys),
        query_escape(key)
    );
    let body = google_get(tok, &url, &[key], deadline)
        .map_err(|e| e.wrap("finding the key's project (set GOOGLE_CLOUD_PROJECT to skip)"))?;
    let project = parse_lookup_key(&body)?;
    KEY_STATE.lock().unwrap().project = project.clone();
    Ok(project)
}

/// Takes the project from lookupKey's
/// {"parent": "projects/<number>/locations/global", ...}.
fn parse_lookup_key(body: &[u8]) -> Result<String, Error> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Response {
        #[serde(deserialize_with = "nullable")]
        parent: String,
    }
    let r: Response = serde_json::from_slice(body)
        .map_err(|e| Error::other(format!("parsing lookupKey JSON: {e}")))?;
    let parts: Vec<&str> = r.parent.split('/').collect();
    if parts.len() < 2 || parts[0] != "projects" || parts[1].is_empty() {
        return Err(Error::other(format!(
            "lookupKey gave no project: {:?}",
            r.parent
        )));
    }
    Ok(parts[1].to_string())
}

// ---- gcloud Application Default Credentials (read-only) ----

/// An access token from ADC, plus the quota project that Google bills the API
/// calls to.
#[derive(Clone, Default)]
pub(super) struct AdcToken {
    access: String,
    quota_project: String,
    expires_at: Option<Instant>,
}

static ADC_CACHE: Mutex<Option<AdcToken>> = Mutex::new(None);

fn forget_adc() {
    *ADC_CACHE.lock().unwrap() = None;
}

/// The shape that gcloud writes for `auth application-default login`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct AdcFile {
    #[serde(rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(deserialize_with = "nullable")]
    client_id: String,
    #[serde(deserialize_with = "nullable")]
    client_secret: String,
    #[serde(deserialize_with = "nullable")]
    refresh_token: String,
    #[serde(deserialize_with = "nullable")]
    quota_project_id: String,
}

/// Where the ADC file is: GOOGLE_APPLICATION_CREDENTIALS, else the default
/// location of gcloud.
fn adc_path() -> String {
    let v = env::var("GOOGLE_APPLICATION_CREDENTIALS");
    if !v.trim().is_empty() {
        return v.trim().to_string();
    }
    let home = env::var("HOME");
    if home.is_empty() {
        return String::new();
    }
    format!("{home}/.config/gcloud/application_default_credentials.json")
}

fn parse_adc(data: &[u8]) -> Result<AdcFile, Error> {
    let f: AdcFile = serde_json::from_slice(data).map_err(|_| Error::no_adc("file is not JSON"))?;
    if f.kind != "authorized_user" {
        return Err(Error::no_adc(&format!(
            "type {:?} is not supported, only authorized_user",
            f.kind
        )));
    }
    if f.client_id.is_empty() || f.refresh_token.is_empty() {
        return Err(Error::no_adc("file has no refresh token"));
    }
    Ok(f)
}

/// Returns a cached access token, or exchanges the ADC refresh token for a new
/// one. The file is read, never written.
pub(super) fn adc_access_token(deadline: Instant) -> Result<AdcToken, Error> {
    if let Some(tok) = ADC_CACHE.lock().unwrap().clone()
        && !tok.access.is_empty()
        && tok
            .expires_at
            .is_some_and(|t| t.saturating_duration_since(Instant::now()) > Duration::from_secs(60))
    {
        return Ok(tok);
    }

    let path = adc_path();
    let data = std::fs::read(&path).map_err(|_| Error::no_adc(&format!("{path} not readable")))?;
    let f = parse_adc(&data)?;

    // Go's url.Values.Encode sorts the keys.
    let form = [
        ("client_id", f.client_id.as_str()),
        ("client_secret", f.client_secret.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", f.refresh_token.as_str()),
    ]
    .iter()
    .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
    .collect::<Vec<_>>()
    .join("&");

    let secrets = [f.refresh_token.as_str(), f.client_secret.as_str()];
    let mut resp = http::agent()
        .post(endpoint(Endpoint::Token))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .config()
        .timeout_global(Some(http::timeout(CLIENT_TIMEOUT, deadline)))
        .build()
        .send(form.as_bytes())
        .map_err(|e| {
            Error::other(format!(
                "ADC token request: {}",
                transport_error(&e, &secrets)
            ))
        })?;
    let code = resp.status().as_u16();
    let body = http::read_limited(resp.body_mut(), BODY_LIMIT).unwrap_or_default();

    match code {
        // invalid_grant: the login was revoked or has expired.
        400 | 401 => return Err(Error::no_adc("Google rejected the refresh token")),
        200 => {}
        _ => {
            return Err(Error::other(format!(
                "ADC token HTTP {code}: {}",
                snippet(&body)
            )));
        }
    }

    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct TokenResponse {
        #[serde(deserialize_with = "nullable")]
        access_token: String,
        #[serde(deserialize_with = "nullable")]
        expires_in: i64,
    }
    let r = serde_json::from_slice::<TokenResponse>(&body)
        .ok()
        .filter(|r| !r.access_token.is_empty())
        .ok_or_else(|| Error::other("ADC token response has no access_token"))?;
    // GOOGLE_CLOUD_QUOTA_PROJECT wins over the file, as in Google's own client
    // libraries, so a quota project can be set without an edit to the ADC
    // file.
    let tok = AdcToken {
        access: r.access_token,
        quota_project: env::trimmed_or("GOOGLE_CLOUD_QUOTA_PROJECT", &f.quota_project_id),
        expires_at: Some(Instant::now() + Duration::from_secs(r.expires_in.max(0) as u64)),
    };
    *ADC_CACHE.lock().unwrap() = Some(tok.clone());
    Ok(tok)
}

/// Calls a Google Cloud API with the ADC token. `secrets` are removed from a
/// transport error, as Go's stripURL drops the URL: lookupKey has the API key
/// in its query string, and that must never reach the menu or a log.
pub(super) fn google_get(
    tok: &AdcToken,
    url: &str,
    secrets: &[&str],
    deadline: Instant,
) -> Result<Vec<u8>, Error> {
    let mut req = http::agent()
        .get(url)
        .header("Authorization", &format!("Bearer {}", tok.access));
    if !tok.quota_project.is_empty() {
        // User credentials have no project of their own. This names the one
        // that Google charges the API call to.
        req = req.header("x-goog-user-project", &tok.quota_project);
    }
    let mut resp = req
        .config()
        .timeout_global(Some(http::timeout(CLIENT_TIMEOUT, deadline)))
        .build()
        .call()
        .map_err(|e| Error::other(transport_error(&e, secrets)))?;
    let code = resp.status().as_u16();
    let retry = header(&resp, "Retry-After");
    let body = http::read_limited(resp.body_mut(), BODY_LIMIT).unwrap_or_default();

    match code {
        401 => {
            forget_adc();
            Err(Error::no_adc("Google rejected the access token"))
        }
        429 => Err(Error::rate_limited(retry_after(&retry))),
        200 => Ok(body),
        // 403 lands here: a disabled API or a missing role. Google's message
        // names which, so pass it on.
        _ => Err(Error::other(format!("HTTP {code}: {}", snippet(&body)))),
    }
}

/// The text of a transport error, with every secret removed.
fn transport_error(e: &ureq::Error, secrets: &[&str]) -> String {
    let mut msg = e.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        msg = msg.replace(s, "").replace(&query_escape(s), "");
    }
    msg
}

/// Escapes `s` for a URL query, as Go's url.QueryEscape does: unreserved
/// characters stay, a space becomes "+", and every other byte becomes %XX.
pub(super) fn query_escape(s: &str) -> String {
    escape(s, |b| matches!(b, b'-' | b'_' | b'.' | b'~'), true)
}

/// Escapes `s` for one URL path segment, as Go's url.PathEscape does.
pub(super) fn path_escape(s: &str) -> String {
    escape(
        s,
        |b| {
            matches!(
                b,
                b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b':' | b'=' | b'@'
            )
        },
        false,
    )
}

fn escape(s: &str, keep: impl Fn(u8) -> bool, space_plus: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || keep(b) {
            out.push(b as char);
        } else if b == b' ' && space_plus {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::testserver;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const TEST_KEY: &str = "AIzaTEST-not-a-real-key";

    thread_local! {
        static ENDPOINTS: RefCell<HashMap<u8, String>> = RefCell::new(HashMap::new());
    }

    pub(in crate::usage) fn endpoint_override(e: Endpoint) -> Option<String> {
        ENDPOINTS.with(|m| m.borrow().get(&(e as u8)).cloned())
    }

    fn set_endpoint(e: Endpoint, url: &str) {
        ENDPOINTS.with(|m| m.borrow_mut().insert(e as u8, url.into()));
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        super::super::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[test]
    fn is_key_invalid_cases() {
        let invalid = br#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT",
	  "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com"}]}}"#;
        assert!(is_key_invalid(invalid), "API_KEY_INVALID");
        assert!(
            is_key_invalid(
                br#"{"error":{"message":"API key not valid. Please pass a valid API key."}}"#
            ),
            "the message alone"
        );
        let disabled = br#"{"error":{"code":403,"message":"Generative Language API has not been used in project 1 before or it is disabled.","status":"PERMISSION_DENIED"}}"#;
        assert!(
            !is_key_invalid(disabled),
            "a disabled API is not an invalid key"
        );
    }

    #[test]
    fn between_marks_cases() {
        let out = format!("welcome from .zshrc\n{SHELL_MARK} {TEST_KEY}\n{SHELL_MARK}trailing");
        assert_eq!(between_marks(&out), TEST_KEY);
        assert_eq!(between_marks("no marks at all"), "");
        assert_eq!(between_marks(&format!("{SHELL_MARK}{SHELL_MARK}")), "");
    }

    #[test]
    fn parse_lookup_key_cases() {
        let got = parse_lookup_key(
            br#"{"parent":"projects/123456789/locations/global","name":"projects/123456789/locations/global/keys/abc"}"#,
        );
        assert_eq!(got, Ok("123456789".into()));
        assert!(parse_lookup_key(b"{}").is_err());
    }

    #[test]
    fn parse_adc_cases() {
        assert!(
            parse_adc(br#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r"}"#)
                .is_ok()
        );
        for (name, input) in [
            (
                "service account",
                r#"{"type":"service_account","client_email":"x"}"#,
            ),
            ("no token", r#"{"type":"authorized_user","client_id":"c"}"#),
            ("not json", "nope"),
        ] {
            let err = parse_adc(input.as_bytes()).err().expect(name);
            assert_eq!(err.kind, Kind::NoAdc, "{name}");
        }
    }

    #[test]
    fn check_key_once_reports_invalid_key() {
        let _g = lock();
        let srv = testserver::serve(|req| {
            assert_eq!(
                req.header("x-goog-api-key").as_deref(),
                Some(TEST_KEY),
                "key not sent in the x-goog-api-key header"
            );
            assert!(!req.target.contains(TEST_KEY), "key must not be in the URL");
            (
                400,
                r#"{"error":{"message":"API key not valid.","details":[{"reason":"API_KEY_INVALID"}]}}"#
                    .into(),
            )
        });
        set_endpoint(Endpoint::GeminiBase, &srv);
        forget_api_key();
        KEY_STATE.lock().unwrap().key = TEST_KEY.into();

        let err = check_key_once(TEST_KEY, soon()).unwrap_err();
        assert_eq!(err.kind, Kind::KeyInvalid);
        assert!(
            KEY_STATE.lock().unwrap().key.is_empty(),
            "a rejected key must be dropped, so a fixed key is read again"
        );
    }

    #[test]
    fn adc_access_token_exchanges_and_never_writes() {
        let _g = lock();
        let dir = std::env::temp_dir().join(format!("geminiusage-adc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("adc.json");
        let content = r#"{"type":"authorized_user","client_id":"cid","client_secret":"cs","refresh_token":"rt","quota_project_id":"qp"}"#;
        std::fs::write(&path, content).unwrap();
        env::overrides::set("GOOGLE_APPLICATION_CREDENTIALS", path.to_str().unwrap());

        let srv = testserver::serve(|req| {
            let form = req.form();
            assert_eq!(
                form.get("grant_type").map(String::as_str),
                Some("refresh_token")
            );
            assert_eq!(form.get("refresh_token").map(String::as_str), Some("rt"));
            (200, r#"{"access_token":"at","expires_in":3599}"#.into())
        });
        set_endpoint(Endpoint::Token, &srv);
        forget_adc();

        let tok = adc_access_token(soon()).expect("adc_access_token");
        assert_eq!(tok.access, "at");
        assert_eq!(tok.quota_project, "qp");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            content,
            "the ADC file must never be written"
        );
        forget_adc();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adc_access_token_missing_file() {
        let _g = lock();
        env::overrides::set(
            "GOOGLE_APPLICATION_CREDENTIALS",
            "/nonexistent/missing.json",
        );
        forget_adc();
        let err = adc_access_token(soon()).err().expect("an error");
        assert_eq!(err.kind, Kind::NoAdc, "{err}");
    }

    #[test]
    fn key_never_in_transport_error() {
        let _g = lock();
        // Nothing listens on this port, so the request fails in the transport.
        set_endpoint(Endpoint::ApiKeys, "http://127.0.0.1:1");
        env::overrides::set("GOOGLE_CLOUD_PROJECT", "");
        forget_api_key();

        let tok = AdcToken {
            access: "at".into(),
            ..Default::default()
        };
        let err = key_project(TEST_KEY, &tok, soon()).unwrap_err();
        assert!(
            !err.to_string().contains(TEST_KEY),
            "error text leaks the API key: {err}"
        );
    }

    #[test]
    fn missing_key_is_remembered() {
        let _g = lock();
        forget_api_key();
        KEY_STATE.lock().unwrap().missing_until = Some(Instant::now() + Duration::from_secs(3600));
        env::overrides::set("GEMINI_API_KEY", TEST_KEY); // found, if the app looked
        let err = api_key(soon()).unwrap_err();
        assert_eq!(
            err.kind,
            Kind::NoApiKey,
            "a recent miss must not look again"
        );
        forget_api_key();
        assert_eq!(api_key(soon()), Ok(TEST_KEY.into()));
        forget_api_key();
    }

    #[test]
    fn rejected_env_key_falls_back_to_login_shell() {
        let _g = lock();
        forget_api_key();
        env::overrides::set("GEMINI_API_KEY", TEST_KEY);
        api_key(soon()).expect("the environment key");
        assert!(
            reject_env_key(),
            "a rejected environment key allows a second try"
        );
        assert!(!reject_env_key(), "only one second try");
        forget_api_key();
        env::overrides::set("SHELL", "/usr/bin/false"); // the login shell has no key
        let err = api_key(soon()).unwrap_err();
        assert_eq!(err.kind, Kind::NoApiKey, "the rejected key must be skipped");
        forget_api_key();
        KEY_STATE.lock().unwrap().env_rejected = false;
    }

    #[test]
    fn escapes_match_go() {
        assert_eq!(query_escape("a b/c?d=e&f~g"), "a+b%2Fc%3Fd%3De%26f~g");
        assert_eq!(path_escape("my project/1"), "my%20project%2F1");
        assert_eq!(path_escape("p-1:a@b"), "p-1:a@b");
    }

    /// Diagnostic for the API-key row: checks GEMINI_API_KEY and prints the
    /// request count of its project for today. Opt-in, because it needs a
    /// valid key and `gcloud auth application-default login`:
    ///
    ///     cargo test live_fetch_api_key -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_fetch_api_key() {
        let _g = lock();
        match fetch_api_key(Instant::now() + Duration::from_secs(25)) {
            Ok(k) => {
                println!(
                    "project {}: {} requests since {}",
                    k.project, k.requests, k.since
                );
                assert!(k.requests >= 0);
            }
            Err(e) if matches!(e.kind, Kind::NoApiKey | Kind::NoAdc) => println!("skipped: {e}"),
            Err(e) => panic!("fetch_api_key: {e}"),
        }
    }
}
