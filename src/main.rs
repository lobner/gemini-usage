//! Gemini Usage is a macOS menu-bar app that shows your Antigravity Gemini
//! quota as percentages in the menu-bar title - "<5h>% · <weekly>%", the two
//! limits of the Gemini group - and lists the windows of every model group with
//! their reset times in the dropdown. It reads the quota summary that
//! Antigravity CLI (`agy`) shows in /usage, directly from Google with agy's
//! stored OAuth token, and posts a Notification Centre banner when a window
//! crosses a threshold.
//!
//! It also polls Google's Workspace Status Dashboard: when a Gemini product has
//! a running incident, a red dot appears in front of the percentages and the
//! dropdown lists what is going on.
//!
//! # What these numbers are, and are not
//!
//! These are Antigravity quotas - the meters behind `agy`. They are NOT the
//! meters of the Gemini app at https://gemini.google.com/usage, which has no
//! token-authenticated API and is reachable only with whole-account Google
//! session cookies. See the usage module and the README for why this app
//! deliberately does not go there.
//!
//! The two sources are polled at different rates. The status dashboard needs
//! no credentials and has no rate limit, so it is read every POLL_SECONDS. The
//! quota endpoint is cheap but not free, so it has its own USAGE_SECONDS cadence
//! and backs off if it refuses.
//!
//! Environment overrides:
//!
//!     POLL_SECONDS          status-dashboard interval in seconds (default 60, minimum 10)
//!     USAGE_SECONDS         quota-endpoint interval in seconds (default 60, minimum 30)
//!     ALERT_PERCENT         notify when a tier first reaches this % (default 80, 0=off)
//!     STATUS_URL            incident feed to poll (default the Workspace dashboard)
//!     CLOUDCODE_ENDPOINT    quota host (default https://daily-cloudcode-pa.googleapis.com)
//!     AGY_BIN               the agy binary to ask for its version (User-Agent)
//!     GOOGLE_CLOUD_PROJECT  API key row: skip the key lookup and use this project id

mod duration;
mod env;
mod http;
mod icon;
mod json;
mod login;
mod notify;
mod status;
#[cfg(test)]
mod testserver;
mod tray;
mod usage;

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeDelta, Utc};
use crossbeam_channel::{Receiver, Sender, bounded, select, tick};

use tray::MenuItem;
use usage::Kind;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

const BLINK_INTERVAL: Duration = Duration::from_millis(700);

// Unlike Anthropic's usage endpoint - which allows approximately one request
// per two minutes per token and forced the 120 s floor of claude-usage - the
// Cloud Code quota endpoint has been comfortable at this rate. Do not copy the
// floor of claude-usage over: it is not the same endpoint and not the same
// limit. A 429 still backs off.
const DEFAULT_USAGE_INTERVAL: Duration = Duration::from_secs(60);
const MIN_USAGE_INTERVAL: Duration = Duration::from_secs(30);
const MAX_USAGE_BACKOFF: Duration = Duration::from_secs(15 * 60);

// The usage page of the Gemini app. These numbers do NOT come from it - see the
// module comment - but it is the page people mean by "Gemini usage", so the
// menu offers it, with a tooltip that says which is which.
const USAGE_PAGE_URL: &str = "https://gemini.google.com/usage";
// The API key row opens AI Studio usage, which shows the requests and spend of
// the same project.
const AI_STUDIO_USAGE_URL: &str = "https://aistudio.google.com/usage";
const STATUS_PAGE_URL: &str = "https://www.google.com/appsstatus/dashboard/";

const APP_NAME: &str = "Gemini Usage";
const REPO_URL: &str = "https://github.com/lobner/gemini-usage";
const MAX_TIER_ROWS: usize = 4; // a fixed pool of tier rows (rows are never removed)
const MAX_STATUS_ITEMS: usize = 5; // a fixed pool of incident rows
const STATUS_ROW_MAX_LEN: usize = 64; // characters for each incident row

// Build stamps, set by build/make-app.sh through the environment of `cargo
// build`. A plain `cargo build` or `cargo run` leaves them at these defaults,
// which is what "dev" in the About row means.
const VERSION: &str = match option_env!("GEMINIUSAGE_VERSION") {
    Some(v) => v,
    None => "dev",
};
const COMMIT: &str = match option_env!("GEMINIUSAGE_COMMIT") {
    Some(v) => v,
    None => "",
};
const BUILD_DATE: &str = match option_env!("GEMINIUSAGE_BUILD_DATE") {
    Some(v) => v,
    None => "",
};

/// The environment overrides, read once at startup.
struct Config {
    poll_interval: Duration,
    usage_interval: Duration,
    alert_percent: i64,
    status_url: String,
}

static CONFIG: LazyLock<Config> = LazyLock::new(|| Config {
    poll_interval: poll_interval_from(&env::var("POLL_SECONDS")),
    usage_interval: usage_interval_from(&env::var("USAGE_SECONDS")),
    alert_percent: alert_percent_from(&env::var("ALERT_PERCENT")),
    status_url: env_or("STATUS_URL", status::DEFAULT_URL),
});

/// The rows that change after startup.
struct Menu {
    // Tier rows come in pairs - the meter and its reset line - because Google
    // decides the number of tiers the endpoint returns, not us.
    tiers: Vec<MenuItem>,
    tier_resets: Vec<MenuItem>,
    status: MenuItem,
    status_items: Vec<MenuItem>,
    affected: MenuItem,
    last_check: MenuItem,
    login: MenuItem,
    key: MenuItem,
}

static MENU: OnceLock<Menu> = OnceLock::new();

fn menu() -> &'static Menu {
    MENU.get().expect("the menu is built in on_ready")
}

type Chan<T> = LazyLock<(Sender<T>, Receiver<T>)>;

static REFRESH_NOW: Chan<()> = LazyLock::new(|| bounded(1));

// The icon state, owned by icon_loop. check_status reports what the dashboard
// says. Opening the menu acknowledges an outage.
static ICON_UPDATES: Chan<IconUpdate> = LazyLock::new(|| bounded(1));
static ICON_ACK: Chan<()> = LazyLock::new(|| bounded(1));

// Shared between the poll thread (writer) and the click threads (readers).
static INCIDENT_URLS: Mutex<[String; MAX_STATUS_ITEMS]> =
    Mutex::new([const { String::new() }; MAX_STATUS_ITEMS]);

fn main() {
    tray::run(on_ready);
}

fn on_ready() {
    tray::set_title("…");
    tray::set_tooltip("Gemini usage — checking…");

    // The tier rows. They are relabelled, shown and hidden on each poll.
    let mut tiers = Vec::with_capacity(MAX_TIER_ROWS);
    let mut tier_resets = Vec::with_capacity(MAX_TIER_ROWS);
    for _ in 0..MAX_TIER_ROWS {
        let mi = tray::add_item("", "");
        mi.disable();
        mi.hide();
        tiers.push(mi);

        let reset = tray::add_item("", "");
        reset.disable();
        reset.hide();
        tier_resets.push(reset);
    }
    tiers[0].set_title("Antigravity quota: …");
    tiers[0].show();

    tray::add_separator();

    // The row of the Gemini API key, hidden until a key is found. It is below
    // the separator, not above one of its own: a separator cannot be hidden,
    // so a separator of its own would leave a double line with no key.
    let key = tray::add_item("", "Open AI Studio usage for the API key's project");
    key.hide();

    let status = tray::add_item(
        "Service status: checking…",
        "Open the Google Workspace Status Dashboard",
    );

    // A fixed pool of incident rows, shown, hidden and relabelled on each poll.
    let mut status_items = Vec::with_capacity(MAX_STATUS_ITEMS);
    for idx in 0..MAX_STATUS_ITEMS {
        let mi = tray::add_item("", "");
        mi.hide();
        on_click(&mi, move || open_url(&incident_url(idx)));
        status_items.push(mi);
    }
    let affected = tray::add_item("", "");
    affected.disable();
    affected.hide();

    tray::add_separator();
    let open_page = tray::add_item(
        "Open usage page",
        "Open gemini.google.com/usage - the Gemini app's own meters, which are not the Antigravity quota shown above",
    );
    let refresh = tray::add_item("Refresh now", "Fetch quota immediately");
    let last_check = tray::add_item("", "");
    last_check.disable();
    tray::add_separator();

    let login = tray::add_checkbox(
        "Launch at Login",
        "Open Gemini Usage automatically when you log in",
        login::enabled(),
    );
    if login::bundle_path().is_none() || !login::supported() {
        login.hide(); // nothing to register: not a bundle, or macOS 12 or earlier
    }
    tray::add_separator();

    let about = tray::add_item(&about_title(), &about_tooltip());
    let quit = tray::add_item("Quit", "Quit Gemini Usage");

    on_click(&key, || open_url(AI_STUDIO_USAGE_URL));
    on_click(&open_page, || open_url(USAGE_PAGE_URL));
    on_click(&status, || open_url(STATUS_PAGE_URL));
    on_click(&refresh, trigger_refresh);
    on_click(&login, toggle_launch_at_login);
    on_click(&about, || open_url(&release_notes_url()));
    thread::spawn(move || {
        let _ = quit.clicked().recv();
        tray::quit();
    });

    // Opening the menu means the user has seen the outage: stop the blinking.
    tray::on_menu_will_open(|| {
        let _ = ICON_ACK.0.try_send(()); // or an acknowledgement is already queued
    });

    let _ = MENU.set(Menu {
        tiers,
        tier_resets,
        status,
        status_items,
        affected,
        last_check,
        login,
        key,
    });

    thread::spawn(icon_loop);
    thread::spawn(|| Poller::default().run());
    thread::spawn(offer_launch_at_login);
}

/// Runs `f` on its own thread for each click on `item`, one click at a time.
fn on_click(item: &MenuItem, f: impl Fn() + Send + 'static) {
    let clicks = item.clicked().clone();
    thread::spawn(move || {
        for () in clicks.iter() {
            f();
        }
    });
}

/// Names the app and the build in one line, so the version is visible without
/// a click.
fn about_title() -> String {
    format!("About {APP_NAME} {VERSION}")
}

/// Carries what is necessary to identify a build in a bug report. The
/// architecture is there because the released archives are for Apple silicon
/// only, and the commit because these builds are unsigned - "which build is
/// that?" otherwise has no answer.
fn about_tooltip() -> String {
    let mut parts = Vec::with_capacity(4);
    if !COMMIT.is_empty() {
        parts.push(format!("commit {COMMIT}"));
    }
    if !BUILD_DATE.is_empty() {
        parts.push(format!("built {BUILD_DATE}"));
    }
    parts.push(platform());
    parts.join(" · ") + " — click for the release notes"
}

/// The platform in Go's GOOS/GOARCH words, which the About tooltip and the
/// User-Agent have always used.
pub(crate) fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!("{os}/{arch}")
}

/// Points at the notes of this exact version when the build came from a clean
/// tag, and at the release list otherwise, since there is nothing to link a
/// dev build to.
fn release_notes_url() -> String {
    release_notes_url_for(VERSION)
}

fn release_notes_url_for(version: &str) -> String {
    if version.starts_with('v') && !version.contains(['-', '+']) {
        format!("{REPO_URL}/releases/tag/{version}")
    } else {
        format!("{REPO_URL}/releases")
    }
}

/// Asks - once - whether the app should open at login, and registers it as a
/// login item if so. It stays quiet when there is nothing to offer: when the
/// user has already answered either way, when the binary is not running from
/// its .app bundle, or on a macOS without SMAppService.
fn offer_launch_at_login() {
    if login::bundle_path().is_none() || !login::supported() {
        return;
    }
    let m = menu();

    // Already answered: do not ask again, but make the system match what we
    // recorded - see login::reconcile for why it cannot be read back instead.
    if login::answered() {
        if login::answer() {
            if login::reconcile() {
                m.login.check();
            } else {
                m.login.uncheck();
            }
        }
        return;
    }

    let Ok(yes) = notify::confirm(
        "Launch Gemini Usage at login?",
        "Gemini Usage can start automatically when you log in, so your model quota is always in the menu bar.",
        "Launch at Login",
        "Not Now",
    ) else {
        return; // the dialog itself failed; try again next launch, do not guess
    };
    if !yes {
        let _ = login::record_answer(false);
        return;
    }

    if let Err(e) = login::enable() {
        // Do not record the answer, so the offer is made again next launch.
        let _ = notify::banner("Gemini Usage", &format!("Could not open at login: {e}"));
        return;
    }
    let _ = login::record_answer(true);
    m.login.check();
    warn_if_needs_approval();
}

/// Flips the Launch at Login checkbox. The checkbox changes only after the
/// change has gone through, so a failure leaves the menu showing the truth.
fn toggle_launch_at_login() {
    let m = menu();
    if m.login.checked() {
        if let Err(e) = login::disable() {
            let _ = notify::banner(
                "Gemini Usage",
                &format!("Could not stop opening at login: {e}"),
            );
            return;
        }
        let _ = login::record_answer(false);
        m.login.uncheck();
        return;
    }

    if let Err(e) = login::enable() {
        let _ = notify::banner("Gemini Usage", &format!("Could not open at login: {e}"));
        return;
    }
    let _ = login::record_answer(true);
    m.login.check();
    warn_if_needs_approval();
}

/// Covers the case where the user has switched the login item off in System
/// Settings before: the registration succeeds, but macOS holds it back until
/// they switch it on there.
fn warn_if_needs_approval() {
    if login::needs_approval() {
        let _ = notify::banner(
            "Gemini Usage",
            "Switch Gemini Usage on under System Settings → General → Login Items to finish enabling it.",
        );
    }
}

/// The state of the poll thread. Nothing else reads or writes it.
#[derive(Default)]
struct Poller {
    first_run_done: bool,
    alerted: HashMap<String, bool>,
    have_status: bool,
    last_incidents: HashSet<String>,

    /// The last good reading of the API key. It takes the place of the title
    /// when there are no quota meters.
    last_key: Option<usage::KeyUsage>,

    // The pacing of the quota endpoint. next_usage is when the endpoint may be
    // called again. The backoff grows while it keeps saying no.
    next_usage: Option<Instant>,
    usage_backoff: Duration,
    /// When the API key row may be read again.
    next_key: Option<Instant>,
}

impl Poller {
    fn run(mut self) {
        self.check(); // the first check is immediate
        let ticker = tick(CONFIG.poll_interval);
        loop {
            select! {
                recv(ticker) -> _ => self.check(),
                recv(REFRESH_NOW.1) -> _ => {
                    // "Refresh now" means now: drop the pacing so that the
                    // quota endpoint is called. If it refuses, the menu says so.
                    self.next_usage = None;
                    self.usage_backoff = Duration::ZERO;
                    usage::forget_api_key();
                    self.next_key = None;
                    self.check();
                }
            }
        }
    }

    fn check(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(25);
        let m = menu();

        // The status dashboard needs no credentials and has no rate limit, so
        // check it first and on every poll: the dot must keep working while the
        // quota endpoint is not available for any reason.
        self.check_status(deadline);

        // The key row has its own pacing, so a long quota backoff does not
        // freeze it.
        if self.next_key.is_none_or(|t| Instant::now() >= t) {
            self.check_api_key(deadline);
            self.next_key = Some(Instant::now() + CONFIG.usage_interval);
        }

        // Respect the pacing instead of calling the endpoint on every poll.
        if self.next_usage.is_some_and(|n| Instant::now() < n) {
            return;
        }

        let result = usage::fetch(deadline);
        let now = Instant::now();
        let wall = Local::now();
        let stamp = wall.format("%H:%M:%S").to_string();

        let err = match result {
            Ok(u) => {
                self.usage_backoff = Duration::ZERO;
                self.next_usage = Some(now + CONFIG.usage_interval);

                update_ui(&u);
                self.notify_thresholds(&u);
                m.last_check.set_title(&format!("Last checked {stamp}"));
                m.last_check.set_tooltip("");
                return;
            }
            Err(err) => err,
        };

        match &err.kind {
            Kind::RateLimited { retry_after } => {
                // Not a fault: keep the numbers on screen and space out.
                let wait = self.back_off_usage(*retry_after);
                self.next_usage = Some(now + wait);
                let retry_at = wall + wait;
                m.last_check.set_title(&format!(
                    "Rate limited; retrying {}",
                    retry_at.format("%H:%M:%S")
                ));
                m.last_check.set_tooltip(&format!(
                    "The quota endpoint refused the request. Backing off {}.",
                    duration::go_string(duration::round_to_second(wait))
                ));
            }
            Kind::NoCredentials => {
                show_problem(
                    "⚠ Antigravity CLI not logged in",
                    "Run `agy` once to sign in",
                    "Gemini usage - not signed in to Antigravity",
                    &format!("Not signed in {stamp}"),
                    Some(&err),
                );
                self.key_title();
                self.next_usage = Some(now + CONFIG.usage_interval);
            }
            Kind::TokenExpired => {
                // Keep the last known numbers. Only flag that the token needs a
                // refresh.
                show_problem(
                    "⚠ OAuth token expired",
                    "Run `agy` to refresh",
                    "Gemini usage - token expired; refresh by running agy",
                    &format!("Token expired {stamp}"),
                    None,
                );
                self.next_usage = Some(now + CONFIG.usage_interval);
            }
            Kind::NotEligible(detail) => {
                let text = match detail {
                    Some(ne) if ne.code == "SUBSCRIPTION_REQUIRED" => {
                        "Google wants a licence for this account"
                    }
                    _ => "This account has no Antigravity quota",
                };
                show_problem(
                    "⚠ No Antigravity quota",
                    text,
                    "Gemini usage - no Antigravity quota for this account",
                    &format!("Not eligible {stamp}"),
                    Some(&err),
                );
                if let Some(ne) = detail
                    && !ne.message.is_empty()
                {
                    m.tier_resets[0].set_tooltip(&ne.message);
                }
                self.key_title();
                // Nothing about this resolves by itself, so back right off
                // instead of asking the same doomed question every minute.
                self.next_usage = Some(now + MAX_USAGE_BACKOFF);
            }
            _ => {
                // Say what went wrong: a bare "failed" looks the same as the
                // rate limiting above, which made this hard to diagnose.
                tray::set_tooltip("Gemini usage — update failed");
                m.last_check.set_title(&format!(
                    "Last check failed {stamp} — {}",
                    truncate(&reason(&err.to_string()), 48)
                ));
                m.last_check.set_tooltip(&err.to_string());
                self.next_usage = Some(now + CONFIG.usage_interval);
            }
        }
    }

    /// Reads the request count of the Gemini API key into the key row. No key
    /// is normal and hides the row. Other problems show in the row itself, so
    /// they never hide the quota meters.
    fn check_api_key(&mut self, deadline: Instant) {
        let key = &menu().key;
        match usage::fetch_api_key(deadline) {
            Ok(k) => {
                key.set_title(&format!("API key: {} today", plural(k.requests, "request")));
                key.set_tooltip(&format!(
                    "Gemini API generateContent requests in project {} since {} (midnight Pacific). Click for AI Studio usage.",
                    k.project,
                    k.since.with_timezone(&Local).format("%H:%M")
                ));
                self.last_key = Some(k);
            }
            Err(err) => match err.kind {
                Kind::NoApiKey => {
                    self.last_key = None;
                    key.hide();
                    return;
                }
                Kind::KeyInvalid => {
                    self.last_key = None;
                    key.set_title("⚠ API key not valid - replace GEMINI_API_KEY");
                    key.set_tooltip(
                        "Google rejected the key. Get a new one at aistudio.google.com/apikey.",
                    );
                }
                Kind::NoAdc => {
                    key.set_title("⚠ API key: run gcloud auth application-default login");
                    key.set_tooltip(&format!(
                        "The request count comes from Cloud Monitoring, which needs a gcloud login. {err}"
                    ));
                }
                Kind::RateLimited { .. } => return, // keep the last reading on screen
                _ => {
                    key.set_title(&format!(
                        "API key: count failed - {}",
                        truncate(&reason(&err.to_string()), 40)
                    ));
                    key.set_tooltip(&err.to_string());
                }
            },
        }
        key.show();
    }

    /// Puts the request count of the API key in the title when there are no
    /// quota meters, so a setup with only an API key does not show "⚠" for
    /// ever.
    fn key_title(&self) {
        if let Some(k) = &self.last_key {
            tray::set_title(&format!("{} req", k.requests));
        }
    }

    /// Doubles the wait each time the endpoint refuses, from one usage interval
    /// up to MAX_USAGE_BACKOFF, and uses the server's Retry-After instead when
    /// that asks for longer.
    fn back_off_usage(&mut self, retry_after: Duration) -> Duration {
        if self.usage_backoff.is_zero() {
            self.usage_backoff = CONFIG.usage_interval;
        } else if self.usage_backoff < MAX_USAGE_BACKOFF {
            self.usage_backoff *= 2;
        }
        if self.usage_backoff > MAX_USAGE_BACKOFF {
            self.usage_backoff = MAX_USAGE_BACKOFF;
        }
        if retry_after > self.usage_backoff {
            return retry_after;
        }
        self.usage_backoff
    }

    /// Sends a banner the first time a tier reaches the alert percentage, and
    /// arms again after it drops back below (for example after a reset). The
    /// first poll only sets a baseline, so a launch while already high does not
    /// spam.
    fn notify_thresholds(&mut self, u: &usage::Usage) {
        let alert = CONFIG.alert_percent;
        if alert <= 0 {
            return;
        }
        for m in &u.tiers {
            let high = m.percent >= alert;
            let alerted = self.alerted.get(&m.name).copied().unwrap_or(false);
            if self.first_run_done && high && !alerted {
                let _ = notify::banner(
                    &format!("{} quota high", m.name),
                    &format!("{} at {}%", m.name, m.percent),
                );
            }
            self.alerted.insert(m.name.clone(), high);
        }
        self.first_run_done = true;
    }

    /// Polls the Workspace dashboard and refreshes the dot and the incident
    /// rows. A failed poll keeps what was last known: a flaky network must not
    /// silently claim that everything is fine.
    fn check_status(&mut self, deadline: Instant) {
        let s = match status::fetch(&CONFIG.status_url, deadline) {
            Ok(s) => s,
            Err(_) => {
                if !self.have_status {
                    menu().status.set_title("Service status unavailable");
                }
                return;
            }
        };
        self.have_status = true;
        update_status_ui(&s);
        // Outside update_status_ui on purpose: that holds INCIDENT_URLS, and this
        // send can block, which would stall the click handlers that need it.
        self.report_status_to_icon(&s);
    }

    /// Tells icon_loop what the dashboard says: whether anything is wrong, and
    /// whether any of it is new since the last poll. An outage with no listed
    /// incidents still counts as ongoing - the indicator alone is enough.
    fn report_status_to_icon(&mut self, s: &status::Summary) {
        let cur: HashSet<String> = s.incidents.iter().map(|i| i.id.clone()).collect();
        let is_new = cur.iter().any(|id| !self.last_incidents.contains(id));
        self.last_incidents = cur;
        let _ = ICON_UPDATES.0.send(IconUpdate {
            ongoing: !s.all_operational(),
            is_new,
        });
    }
}

fn plural(n: i64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Puts a credential-shaped problem in the first two tier rows and hides the
/// rest, so the menu explains itself instead of showing stale numbers with no
/// hint of why they stopped moving.
fn show_problem(
    headline: &str,
    detail: &str,
    tooltip: &str,
    last_check: &str,
    err: Option<&usage::Error>,
) {
    let m = menu();
    tray::set_title("⚠");
    tray::set_tooltip(tooltip);

    m.tiers[0].set_title(headline);
    m.tiers[0].show();
    m.tier_resets[0].set_title(&format!("    {detail}"));
    m.tier_resets[0].show();
    for i in 1..MAX_TIER_ROWS {
        m.tiers[i].hide();
        m.tier_resets[i].hide();
    }

    m.last_check.set_title(last_check);
    match err {
        Some(err) => m.last_check.set_tooltip(&err.to_string()),
        None => m.last_check.set_tooltip(""),
    }
}

/// Reduces an error to something that fits in a menu row: the first line,
/// without the multi-line JSON body that some failures carry.
fn reason(s: &str) -> String {
    let first = s.split(['\n', '\r']).next().unwrap_or_default();
    let first = first.trim();
    first.strip_suffix(':').unwrap_or(first).trim().to_string()
}

fn update_ui(u: &usage::Usage) {
    let m = menu();
    tray::set_title(&title(u));
    tray::set_tooltip(&format!("Antigravity quota - {}", tooltip_summary(u)));

    for i in 0..MAX_TIER_ROWS {
        let Some(meter) = u.tiers.get(i) else {
            m.tiers[i].hide();
            m.tier_resets[i].hide();
            continue;
        };
        m.tiers[i].set_title(&format!("{}: {}%", meter.name, meter.percent));
        m.tiers[i].set_tooltip(&format!(
            "{}% of the {} limit used (bucket {})",
            meter.percent, meter.name, meter.bucket
        ));
        m.tiers[i].show();
        set_reset(&m.tier_resets[i], meter);
    }
}

/// The menu-bar text: the percentages of the headline tiers, in the
/// "<a>% · <b>%" shape of claude-usage.
fn title(u: &usage::Usage) -> String {
    let h = u.headline();
    if h.is_empty() {
        return "…".into();
    }
    h.iter()
        .map(|m| format!("{}%", m.percent))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn tooltip_summary(u: &usage::Usage) -> String {
    if u.tiers.is_empty() {
        return "no windows reported".into();
    }
    u.tiers
        .iter()
        .map(|m| format!("{} {}%", m.name, m.percent))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn set_reset(mi: &MenuItem, meter: &usage::Meter) {
    let Some(t) = meter.resets_at else {
        mi.hide();
        return;
    };
    if meter.idle {
        // Google reports "now + window" until the first request, so a date
        // here would move on every poll.
        mi.set_title(&format!(
            "    resets {} after first use",
            window_words(usage::window_length(&meter.window))
        ));
    } else {
        mi.set_title(&format!("    resets {}", humanize_reset(t, Utc::now())));
    }
    mi.show();
}

/// Says a window length the way a person would: "5h", "7 days".
fn window_words(d: Duration) -> String {
    const DAY: u64 = 24 * 3600;
    let secs = d.as_secs();
    if secs >= DAY && d.as_nanos().is_multiple_of(u128::from(DAY) * 1_000_000_000) {
        format!("{} days", secs / DAY)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Formats a reset time as a short relative string for windows that reset
/// within a day ("in 1h 59m"), or as an absolute weekday, date and time
/// otherwise ("on Thu 22/10 16:52").
fn humanize_reset(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let d = t - now;
    if d <= TimeDelta::zero() {
        "now".into()
    } else if d < TimeDelta::hours(1) {
        format!("in {}m", d.num_minutes())
    } else if d < TimeDelta::hours(24) {
        format!("in {}h {}m", d.num_hours(), d.num_minutes() % 60)
    } else {
        // With the date: a weekly window resets on the same weekday, so
        // "Thu 16:52" alone reads as today.
        format!("on {}", t.with_timezone(&Local).format("%a %-d/%-m %H:%M"))
    }
}

fn update_status_ui(s: &status::Summary) {
    let m = menu();
    let mut urls = INCIDENT_URLS.lock().unwrap();
    for u in urls.iter_mut() {
        u.clear();
    }

    if s.all_operational() {
        m.status.set_title("✓  All Gemini services operational");
        m.status
            .set_tooltip("The Workspace Status Dashboard reports no running Gemini incidents");
        for mi in &m.status_items {
            mi.hide();
        }
        m.affected.hide();
        return;
    }

    let desc = if s.description.is_empty() {
        format!("{} service disruption", status::human(&s.indicator))
    } else {
        s.description.clone()
    };
    m.status.set_title(&format!("●  {desc}"));
    m.status
        .set_tooltip("Google Workspace Status Dashboard — click to open it");

    if s.incidents.len() > MAX_STATUS_ITEMS {
        m.status.set_title(&format!(
            "●  {desc}  (showing {MAX_STATUS_ITEMS} of {})",
            s.incidents.len()
        ));
    }

    for (i, mi) in m.status_items.iter().enumerate() {
        let Some(inc) = s.incidents.get(i) else {
            mi.hide();
            continue;
        };
        let label = if inc.status.is_empty() {
            inc.name.clone()
        } else {
            format!("{} — {}", status::human(&inc.status), inc.name)
        };
        mi.set_title(&format!("    {}", truncate(&label, STATUS_ROW_MAX_LEN)));
        mi.set_tooltip(&incident_tooltip(inc));
        urls[i] = inc.url.clone();
        mi.show();
    }

    if s.affected.is_empty() {
        m.affected.hide();
        return;
    }
    let list = s
        .affected
        .iter()
        .map(|c| format!("{} ({})", c.name, status::human(&c.status)))
        .collect::<Vec<_>>()
        .join(", ");
    m.affected.set_title(&format!(
        "    Affected: {}",
        truncate(&list, STATUS_ROW_MAX_LEN)
    ));
    m.affected.set_tooltip(&format!("Affected: {list}"));
    m.affected.show();
}

fn incident_tooltip(inc: &status::Incident) -> String {
    let mut tip = inc.name.clone();
    if !inc.latest_update.is_empty() {
        tip += " — ";
        tip += &inc.latest_update;
    }
    truncate(&tip, 300)
}

/// What the latest status poll saw.
struct IconUpdate {
    ongoing: bool,
    is_new: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum IconState {
    /// All operational: no icon at all, text only.
    Ok,
    /// An outage the user has not seen yet: a blinking red dot.
    Alert,
    /// Seen by the user: a steady red dot.
    Ack,
}

/// Owns the menu-bar icon. It is the only writer, so the blink can never race a
/// state change and leave the wrong icon on screen. The dot blinks until the
/// user opens the menu, then stays solid for as long as the outage lasts.
fn icon_loop() {
    let mut st = IconState::Ok;
    let mut red = false;

    let set_red = |red: &mut bool, on: bool| {
        *red = on;
        if on {
            tray::set_icon(Some(icon::red_dot_png()));
        } else {
            // The blink is off, the outage is not over: a transparent image of
            // the same size holds the icon slot so the percentages do not move.
            tray::set_icon(Some(icon::blank_png()));
        }
    };
    let apply = |st: &mut IconState, red: &mut bool, next: IconState| {
        *st = next;
        match next {
            IconState::Ok => {
                // All clear: remove the icon, so it is text only again.
                *red = false;
                tray::set_icon(None);
            }
            IconState::Alert => set_red(red, true), // start lit, so it is seen immediately
            IconState::Ack => {
                *red = true;
                tray::set_icon(Some(icon::red_dot_png()));
            }
        }
    };

    let ticker = tick(BLINK_INTERVAL);
    loop {
        select! {
            recv(ICON_UPDATES.1) -> u => {
                let Ok(u) = u else { return };
                if !u.ongoing {
                    if st != IconState::Ok {
                        apply(&mut st, &mut red, IconState::Ok);
                    }
                } else if u.is_new || st == IconState::Ok {
                    // A new incident starts the blink again, even if an earlier
                    // one has already been acknowledged.
                    apply(&mut st, &mut red, IconState::Alert);
                }
            }
            recv(ICON_ACK.1) -> _ => {
                if st == IconState::Alert {
                    apply(&mut st, &mut red, IconState::Ack);
                }
            }
            recv(ticker) -> _ => {
                if st == IconState::Alert {
                    let on = !red;
                    set_red(&mut red, on);
                }
            }
        }
    }
}

/// Cuts `s` to `n` characters, with "…" as the last one when it is cut.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    if n < 1 {
        return String::new();
    }
    s.chars().take(n - 1).chain(std::iter::once('…')).collect()
}

fn incident_url(i: usize) -> String {
    INCIDENT_URLS
        .lock()
        .unwrap()
        .get(i)
        .cloned()
        .unwrap_or_default()
}

fn trigger_refresh() {
    let _ = REFRESH_NOW.0.try_send(()); // or a refresh is already queued
}

/// Opens `u` in the default browser, without waiting for it.
fn open_url(u: &str) {
    if u.is_empty() {
        return;
    }
    let child = Command::new("/usr/bin/open")
        .arg(u)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        thread::spawn(move || child.wait());
    }
}

/// Parses a whole number as Go's strconv.Atoi does: an optional sign, then
/// digits only.
fn atoi(v: &str) -> Option<i64> {
    v.parse().ok()
}

/// Reads USAGE_SECONDS, and refuses anything below MIN_USAGE_INTERVAL: the
/// endpoint is cheap but not free, and the windows are hours long, so a faster
/// poll buys nothing.
fn usage_interval_from(v: &str) -> Duration {
    match atoi(v) {
        Some(n) if n >= MIN_USAGE_INTERVAL.as_secs() as i64 => Duration::from_secs(n as u64),
        _ => DEFAULT_USAGE_INTERVAL,
    }
}

fn poll_interval_from(v: &str) -> Duration {
    match atoi(v) {
        Some(n) if n >= 10 => Duration::from_secs(n as u64),
        _ => DEFAULT_POLL_INTERVAL,
    }
}

fn env_or(key: &str, def: &str) -> String {
    let v = env::var(key);
    if v.is_empty() { def.to_string() } else { v }
}

fn alert_percent_from(v: &str) -> i64 {
    match atoi(v) {
        Some(n) if (0..=100).contains(&n) => n,
        _ => 80,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter(name: &str, group: &str, bucket: &str, percent: i64) -> usage::Meter {
        usage::Meter {
            name: name.into(),
            group: group.into(),
            bucket: bucket.into(),
            percent,
            ..Default::default()
        }
    }

    #[test]
    fn truncate_counts_characters() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("much too long", 5), "much…");
        assert_eq!(truncate("æøå æøå", 4), "æøå…");
        assert_eq!(truncate("x", 0), "");
    }

    #[test]
    fn reason_takes_the_first_line() {
        assert_eq!(
            reason("retrieveUserQuotaSummary HTTP 500: {\n \"a\": 1}"),
            "retrieveUserQuotaSummary HTTP 500: {"
        );
        assert_eq!(reason("  parsing failed:  \nbody"), "parsing failed");
    }

    #[test]
    fn release_notes_url_only_for_clean_tags() {
        assert_eq!(
            release_notes_url_for("v1.2.0"),
            "https://github.com/lobner/gemini-usage/releases/tag/v1.2.0"
        );
        for v in ["dev", "v1.2.0-2-gabc1234", "v1.2.0-dirty", "v1.2.0+meta"] {
            assert_eq!(
                release_notes_url_for(v),
                "https://github.com/lobner/gemini-usage/releases"
            );
        }
    }

    #[test]
    fn env_parsing() {
        assert_eq!(poll_interval_from(""), DEFAULT_POLL_INTERVAL);
        assert_eq!(poll_interval_from("9"), DEFAULT_POLL_INTERVAL);
        assert_eq!(poll_interval_from("10"), Duration::from_secs(10));
        assert_eq!(usage_interval_from("29"), DEFAULT_USAGE_INTERVAL);
        assert_eq!(usage_interval_from("30"), Duration::from_secs(30));
        assert_eq!(usage_interval_from("-500"), DEFAULT_USAGE_INTERVAL);
        assert_eq!(alert_percent_from(""), 80);
        assert_eq!(alert_percent_from("0"), 0);
        assert_eq!(alert_percent_from("101"), 80);
    }

    #[test]
    fn back_off_doubles_up_to_the_cap() {
        let mut p = Poller::default();
        let base = CONFIG.usage_interval;
        assert_eq!(p.back_off_usage(Duration::ZERO), base);
        assert_eq!(p.back_off_usage(Duration::ZERO), base * 2);
        assert_eq!(
            p.back_off_usage(Duration::from_secs(3600)),
            Duration::from_secs(3600)
        );
        for _ in 0..10 {
            p.back_off_usage(Duration::ZERO);
        }
        assert_eq!(p.back_off_usage(Duration::ZERO), MAX_USAGE_BACKOFF);
    }

    #[test]
    fn humanize_reset_forms() {
        let now = Utc::now();
        let mins = |m: i64| now + TimeDelta::minutes(m) + TimeDelta::seconds(30);
        assert_eq!(humanize_reset(now, now), "now");
        assert_eq!(humanize_reset(mins(5), now), "in 5m");
        assert_eq!(humanize_reset(mins(119), now), "in 1h 59m");
        let far = now + TimeDelta::days(3);
        assert_eq!(
            humanize_reset(far, now),
            format!(
                "on {}",
                far.with_timezone(&Local).format("%a %-d/%-m %H:%M")
            )
        );
    }

    #[test]
    fn window_words_cases() {
        assert_eq!(window_words(usage::window_length("5h")), "5h");
        assert_eq!(window_words(usage::window_length("weekly")), "7 days");
        assert_eq!(window_words(Duration::from_secs(36 * 3600)), "36h");
    }

    #[test]
    fn title_and_summary() {
        let u = usage::Usage {
            tiers: vec![
                meter("Gemini 5h", "Gemini", "gemini-5h", 63),
                meter("Gemini weekly", "Gemini", "gemini-weekly", 10),
                meter("Claude and GPT 5h", "Claude and GPT", "3p-5h", 0),
            ],
        };
        assert_eq!(title(&u), "63% · 10%");
        assert_eq!(
            tooltip_summary(&u),
            "Gemini 5h 63% · Gemini weekly 10% · Claude and GPT 5h 0%"
        );
        let empty = usage::Usage::default();
        assert_eq!(title(&empty), "…");
        assert_eq!(tooltip_summary(&empty), "no windows reported");
    }

    #[test]
    fn plural_forms() {
        assert_eq!(plural(1, "request"), "1 request");
        assert_eq!(plural(0, "request"), "0 requests");
        assert_eq!(plural(6, "request"), "6 requests");
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn platform_uses_go_names() {
        assert_eq!(platform(), "darwin/arm64");
    }
}
