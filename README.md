# Gemini Usage — macOS menu-bar meter

A tiny menu-bar app that shows your **Antigravity Gemini quota** as percentages
in the menu-bar title - `<5h>% · <weekly>%`, the Gemini group's two limits:

- **First number** → the **5-hour** limit used.
- **Second number** → the **weekly** limit used.

These are the same meters that Antigravity CLI (`agy`) shows in `/usage`. The
dropdown shows every model group's windows with their reset times (Gemini, and
Claude and GPT), the [API key row](#api-key-row), the Gemini service status,
*Open usage page*, *Refresh now*, the last-checked time, *Launch at Login*,
*About*, and *Quit*. When a window first crosses a threshold (default 80%) you get
a Notification Centre banner.

It is a sibling of [claude-usage](https://github.com/lobner/claude-usage) and
[github-usage](https://github.com/lobner/github-usage), and deliberately keeps
the same menu structure.

## Why Antigravity, not Gemini CLI

Google [moved Gemini CLI to Antigravity
CLI](https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/)
on 2026-06-18. Since then, the free tier ("Gemini Code Assist for individuals")
and Google AI Pro/Ultra no longer serve Gemini CLI. Google's `loadCodeAssist`
answers `UNSUPPORTED_CLIENT` for them. Gemini CLI still works with paid API keys
and with Code Assist Standard or Enterprise licences.

Earlier versions of this app read Gemini CLI's Code Assist quota. This version
reads the Antigravity quota of the same Google account.

## ⚠️ These are not the numbers on gemini.google.com/usage

**Read this before trusting the percentages.**

The consumer usage page at <https://gemini.google.com/usage> — the 5-hour and
weekly meters for the Gemini app — has **no token-authenticated API**. It is
served by the Gemini web app and authenticated with whole-account Google session
cookies (`__Secure-1PSID` / `__Secure-1PSIDTS` plus a `SAPISIDHASH`). Reaching it
from a background app would mean:

1. decrypting the browser's cookie jar (Chrome's needs the *Chrome Safe Storage*
   Keychain key; Safari's needs Full Disk Access), and
2. **refreshing** `__Secure-1PSIDTS` against `accounts.google.com/RotateCookies`
   every few minutes to stop the session going stale.

That is a whole-Google-account credential — the same cookie opens Gmail, Drive
and Photos — and step 2 means writing and rotating someone's session token in the
background. Both are the opposite of what this family of apps does (see
[read-only credentials](#design-decision-read-only-credentials-no-auto-refresh)),
so this app does not go there.

**What it reads instead** is the quota the same Google account has in
**Antigravity** - the meters behind `agy`, which `agy` itself shows in `/usage`.
These come from a scoped OAuth token you already own, and they are real.
Antigravity's windows are also 5-hour and weekly, but they are **not** the Gemini
app's meters. The *Open usage page* row still opens `gemini.google.com/usage` for
convenience, and its tooltip says which is which.

If you want the consumer app's meters specifically, this app is the wrong tool —
open that page, or use a browser extension that runs inside the logged-in page.

## Service status

Every poll also reads Google's [Workspace Status
Dashboard](https://www.google.com/appsstatus/dashboard/). While a Gemini product
has a **running** incident, a **red dot** appears in front of the percentages
(`🔴 12% · 34%`) and the dropdown grows an outage section:

- a headline row — `●  Gemini service outage` — which opens the dashboard;
- one row per ongoing incident, labelled `Service disruption — <incident>`;
  hovering shows the newest update, clicking opens that incident;
- an *Affected:* row listing the Gemini products involved.

The dot **blinks** while the outage is unacknowledged, and goes **solid** once you
open the menu — so a new disruption catches your eye, then stops nagging. A later,
different incident starts it blinking again. While blinking, the off phase is a
transparent image of the same size rather than no image at all, so the
percentages stay put instead of shifting twice a second.

When everything is fine the dot disappears entirely and the section collapses to a
single `✓  All Gemini services operational` row. The dashboard needs no
credentials, so the dot keeps working even while the OAuth token is expired.

Two details the feed forces, which the Statuspage-based siblings got for free:

- The feed is a flat array of **all** Workspace products' recent incidents, open
  and closed. An incident counts as ongoing exactly when it has no `end`
  timestamp — there is no per-incident state machine to read (every update in the
  feed is stamped `AVAILABLE`).
- Gemini incidents are matched on the **product title** rather than the service
  key, so `Gemini`, `Gemini Notebook` and whatever Google brands next are all
  picked up — including cross-product incidents filed under *Multiple Products*.

## Polling rates

Unlike claude-usage - whose usage endpoint refuses more than about one request per
two minutes and forced a 120 s floor - the Cloud Code quota endpoint has been
comfortable at a normal poll rate. **Do not port claude-usage's floor over**; it is
not the same endpoint and not the same limit.

- **The status dashboard** - no credentials, no limit - every `POLL_SECONDS`
  (60 s), so the outage dot stays responsive.
- **The quota endpoint** - every `USAGE_SECONDS` (60 s, floor 30 s).
- **The API key row** - every `USAGE_SECONDS`, on its own schedule, so a quota
  back-off does not freeze it.

If a `429` ever does arrive it is treated as expected rather than as a failure:
the percentages stay on screen, the menu says *"Rate limited; retrying 12:34:56"*,
and the wait doubles up to 15 minutes until a call succeeds. The server's
`Retry-After` is honoured when it asks for longer. Genuine failures name their
reason instead of just "failed".

*Refresh now* ignores the pacing and calls the endpoint immediately.

## How it works

Every `USAGE_SECONDS` it makes up to two calls against
`https://daily-cloudcode-pa.googleapis.com/v1internal` - the same internal
endpoint `agy` uses - with `agy`'s stored OAuth token:

1. `:loadCodeAssist` with `ideType: ANTIGRAVITY` → the account's project id. The
   result is cached for the life of the process, so this is normally paid once.
2. `:retrieveUserQuotaSummary` with `{"project": "<id>"}` → model groups
   (*Gemini Models*, *Claude and GPT models*), each with a `5h` and a `weekly`
   bucket: `bucketId`, `remainingFraction` and `resetTime`.

`remainingFraction` is inverted into a *used* percentage, which is the direction
every other meter in this app reads. The 5-hour window comes before the weekly one,
because it is the one that runs out first.

A window starts at your first request in it. Until then, Google reports its reset
as "now + 5 hours" or "now + 7 days", which moves forward on every call. The app
spots this (0% used, and the reset one window length from now) and shows
*"resets 5h after first use"* or *"resets 7 days after first use"* in place of a
moving date.

Three details matter:

- **The User-Agent.** Google returns no project unless the User-Agent names
  Antigravity, as `antigravity/<version> darwin/arm64`. The app asks the installed
  `agy --version` once (`~/.local/bin/agy`, or `AGY_BIN`), and falls back to a
  fixed version.
- **The host.** The older `cloudcode-pa.googleapis.com` host is reported to always
  say 100% for the 5-hour window. Use `daily-cloudcode-pa`.
- **The token.** `agy` keeps its login in the macOS Keychain (service `gemini`,
  account `antigravity`), as a `go-keyring-base64:` JSON blob. The Antigravity app
  keeps the same login in `~/.gemini/jetski-standalone-oauth-token`. The app reads
  both and uses the one that expires last. The first Keychain read shows a macOS
  prompt; choose *Always Allow* so it does not come back.

There is no officially documented API for this data.

## ⚠️ OAuth token & refresh - read this

The app is **read-only**: it uses whatever OAuth token `agy` has stored and
**never refreshes or writes it**. That token lasts about an hour, and only running
`agy` (or the Antigravity app) refreshes it. So:

- While the token is valid, the numbers refresh every `USAGE_SECONDS` as expected.
- Once it expires, every poll returns "token expired", the title shows **"⚠"**,
  the numbers stop updating, and the menu shows **"⚠ OAuth token expired - Run
  `agy` to refresh"** until you next use `agy`.
- If you have never logged in, the menu says **"⚠ Antigravity CLI not logged
  in"**.
- If the account has no Antigravity quota, it says **"⚠ No Antigravity quota"**
  and backs right off, since nothing about that resolves on its own. Google names
  its reason in the tooltip, for example `SUBSCRIPTION_REQUIRED`.

## API key row

When a `GEMINI_API_KEY` is found, the dropdown has one more row under the
quota windows: `API key: 6 requests today`. It counts the Gemini API
`generateContent` requests in the key's Cloud project since midnight Pacific,
when Google resets the daily quotas. Click the row to open AI Studio usage. The
row works next to the Antigravity meters and does not depend on `agy` or Gemini
CLI. When there are no Antigravity meters, the title shows the count (`6 req`) in
place of `⚠`.

The row shows a count, not a percentage, for these reasons:

- Google has no quota endpoint that accepts an API key.
- Cloud Monitoring has the key's project usage, but only as project-wide request
  counts. No series names the model, so the per-model daily limits (in the
  Cloud Quotas API) cannot be joined to usage.
- With billing on, the paid-tier limits are high (for example 10,000 Flash
  requests a day). The real limit is then the prepaid credit, which no API
  shows.

The row uses two credentials, and writes neither:

1. **The API key.** The app reads `GEMINI_API_KEY` (then `GOOGLE_API_KEY`) from
   its environment. A bundle started from Finder or at login does not get
   `~/.zshrc`, so the app then reads the variable from a login shell
   (`$SHELL -lic`). A miss is remembered for 10 minutes. The key stays in memory,
   and the app never logs it.
2. **gcloud Application Default Credentials (ADC).** The app reads
   `~/.config/gcloud/application_default_credentials.json` (or
   `GOOGLE_APPLICATION_CREDENTIALS`) and gets an access token from it in memory.

Each poll, the app checks the key once with `models.list`, finds the key's
project with the API Keys API `keys:lookupKey`, and reads
`quota/rate/net_usage` for `generate_content_requests` from Cloud Monitoring.

### One-time setup

1. Install the Google Cloud CLI: `brew install --cask gcloud-cli`.
2. Run `gcloud auth application-default login`. Tick the Google Cloud box on the
   consent page.
3. Run `gcloud auth application-default set-quota-project <key project>`.
   `keys:lookupKey` needs a quota project. `GOOGLE_CLOUD_QUOTA_PROJECT` also
   works, as in Google's client libraries.
4. Turn on the **API Keys API** in that project.

The row then shows one of these states:

- **`API key: N requests today`** - all is well.
- **"⚠ API key not valid"** - Google rejected the key. Replace it from
  <https://aistudio.google.com/apikey>. The app reads the key again on the next
  poll, so a fix in `~/.zshrc` needs no restart.
- **"⚠ API key: run gcloud auth application-default login"** - there are no
  usable ADC.
- No row - no key was found.

## Design decision: read-only credentials, no auto-refresh

This app **only reads** the OAuth token `agy` already stored, and **never writes
credentials, refreshes tokens, or runs its own auth flow**. That is a deliberate
choice, not an oversight - and it is why the consumer usage page is out of scope,
since reaching that *requires* rotating a session cookie.

The refresh token is in the same Keychain item, and Google's token endpoint is a
standard OAuth `grant_type=refresh_token` exchange. But the exchange needs
Antigravity's OAuth client id and secret, which belong to `agy`, and the
credentials belong to `agy` too. A meter that shows `⚠` until you next run `agy` is
a far better failure than a meter that borrows another app's identity or corrupts
your login.

The [API key row](#api-key-row) does exchange a refresh token, but only the gcloud
ADC one, with gcloud's own client, and only in memory. The app never writes the ADC
file, and gcloud does not rotate that refresh token, so there is no owner to race.
The rule stays the same: the app writes no credentials.

## Build & run

Requires Rust (this repo pins `1.98.1` via `rust-toolchain.toml`, which `rustup`
picks up by itself).

```sh
# Run in the foreground (Ctrl-C to stop):
cargo run

# Build the .app, and let it offer to install:
./build/make-app.sh
```

`make-app.sh` produces `Gemini Usage.app` with `LSUIElement=true` (menu-bar-only,
no Dock icon). After building it asks *"Install to /Applications and relaunch?"*,
which quits whichever copy is running (the installed one or one launched from this
repo — they share a bundle id), replaces the bundle, and reopens it from
`/Applications`. Pass `--install` or `--no-install` to answer up front; with no
terminal on stdin it builds and stops rather than touching `/Applications`.

Installing this way is worth preferring, because *Launch at Login* records the
bundle by the path it was registered from — see below.

## Configuration

| Env var                   | Default | Meaning                                                       |
| ------------------------- | ------- | ------------------------------------------------------------- |
| `POLL_SECONDS`            | `60`    | Status-dashboard interval in seconds (min 10).                |
| `USAGE_SECONDS`           | `60`    | Quota-endpoint interval in seconds (min 30).                  |
| `ALERT_PERCENT`           | `80`    | Banner when a window first reaches this %. `0` disables alerts. |
| `STATUS_URL`              | `https://www.google.com/appsstatus/dashboard/incidents.json` | Incident feed to poll for the outage dot. |
| `CLOUDCODE_ENDPOINT`      | `https://daily-cloudcode-pa.googleapis.com` | Quota host. |
| `AGY_BIN`                 | `~/.local/bin/agy` | The `agy` binary to ask for its version (User-Agent). |
| `GOOGLE_CLOUD_PROJECT`    | —       | API key row: skip the key lookup and use this project id.     |
| `GEMINI_API_KEY`          | —       | The key for the API key row. Read from the login shell when not set. |
| `GOOGLE_APPLICATION_CREDENTIALS` | `~/.config/gcloud/application_default_credentials.json` | The ADC file for the API key row. |
| `GOOGLE_CLOUD_QUOTA_PROJECT` | quota project in the ADC file | The project Google charges the API key row's calls to. |

## Launch at login

The app offers this itself: the first time you run it from the `.app` bundle it
asks *"Launch Gemini Usage at login?"*, and choosing **Launch at Login**
registers the bundle as a login item via `SMAppService` (macOS 13+). It then
appears under System Settings → General → Login Items → **Open at Login**, and
you can switch it off there.

Register the copy that is going to stay put — macOS remembers the bundle by the
path it was registered from, so install to `/Applications` *first* and answer the
prompt from that copy. Answering it from the repo build means the next
`make-app.sh` (which does `rm -rf` on the bundle) leaves a dangling login item.

The offer is made once, either way. The answer is remembered in
`~/Library/Application Support/dk.biq.geminiusage/launch-at-login`; delete that
file to be asked again. It is also skipped when the binary isn't running from a
bundle (`cargo run`), since login items are bundles.

Each launch also *reconciles*: if the record says registered, the app registers
again. That is a no-op when it already is, and it repairs the case where the login
item was dropped — which happens whenever the bundle is replaced, as
`make-app.sh --install` does. If macOS instead reports the item as switched off,
that is left alone: it means someone turned it off in System Settings, and the
checkbox will show unticked until you tick it again.

Three notes on the mechanics:

- **The bundle is ad-hoc signed with its own identifier.** The linker already
  ad-hoc signs, but not with the bundle id (the Go linker, which built this app
  before the Rust port, calls every binary `a.out`), and macOS keys login items
  by signing identity — so two such apps can look like one item, and registering
  either one evicts the other's *Open at Login* entry. `make-app.sh`
  runs `codesign --sign - --identifier <bundle id>` to give each build a distinct,
  stable identity. That matters more here than usual, because this app is designed
  to sit alongside its two siblings. It buys identity, not trust: these builds are
  still not Developer ID signed and not notarised.
- A launchd job in `~/Library/LaunchAgents` also starts the app at login, but
  macOS classifies it as a *background item*, so it lands under **App Background
  Activity** instead — labelled "Item from unidentified developer", since these
  builds are unsigned. `build/dk.biq.geminiusage.plist` is kept as that manual
  alternative.
- `SMAppService.mainApp.status` cannot be used to decide whether to ask: it
  reports `enabled` for a bundle that has *never* been registered, and only says
  `notRegistered` once something has explicitly unregistered it. Hence the local
  state file above.

To set it up by hand instead:

- **Login Items** — System Settings → General → Login Items → add
  `Gemini Usage.app`; or
- **LaunchAgent** — copy `build/dk.biq.geminiusage.plist` to
  `~/Library/LaunchAgents/`, fix the path inside if the app isn't in
  `/Applications`, then
  `launchctl load ~/Library/LaunchAgents/dk.biq.geminiusage.plist`.

## Project layout

```
src/main.rs             poll loop, icon blink, menu contents, threshold banners
src/tray.rs             the status item and its menu, directly on AppKit via objc2
src/usage/mod.rs        Antigravity quota → window meters; agy credentials; the User-Agent
src/usage/apikey.rs     API key → its project, via gcloud ADC (read-only)
src/usage/monitoring.rs the API key's request count from Cloud Monitoring
src/status.rs           poll the Workspace Status Dashboard feed → indicator, incidents, products
src/icon.rs             the red status dot, drawn programmatically (no binary assets)
src/login.rs            open-at-login registration via SMAppService
src/notify.rs           Notification Centre banner + confirm dialog via osascript
src/http.rs             the shared HTTP agent (macOS trust store)
src/env.rs              environment variables, read as Go's os.Getenv reads them
src/duration.rs         Go-style durations ("2m30s") for the menu text
src/json.rs             null-tolerant JSON fields
testdata/               a trimmed capture of the real incident feed
build/                  Info.plist, make-app.sh, LaunchAgent plist
```

`src/icon.rs`, `src/login.rs`, `src/notify.rs` and `src/tray.rs` are
**duplicated** from claude-usage rather than shared through a crate. Fix a bug in
one, port it to the others.

The app was written in Go and ported to Rust without changing its behaviour.
`src/tray.rs` replaces the fork of `fyne.io/systray` that the Go build vendored.
It keeps that package's model - calls from any thread, rows that are hidden and
shown, never removed - and attaches the menu to the status item, so AppKit opens
the dropdown directly below the menu bar. (That placement was the one patch the
Go build made to its systray fork: upstream popped the menu at the button's zero
origin, which drew it over the menu-bar icons.)

## Testing

```sh
cargo test      # unit tests (offline)

# Opt-in diagnostics against the live endpoints:
cargo test usage::tests::live_fetch -- --ignored --exact --nocapture   # needs a valid `agy` login
cargo test live_fetch_api_key -- --ignored --nocapture                 # needs GEMINI_API_KEY + ADC
cargo test live_status -- --ignored --nocapture                        # needs network only
```

The gates before a push are `cargo fmt --check`, `cargo clippy --all-targets --
-D warnings`, `cargo build` and `cargo test`.

To see the outage UI without waiting for a real incident, point `STATUS_URL` at a
saved incident feed served locally (e.g. `python3 -m http.server`) with an entry
whose `end` field is removed and whose `affected_products` names Gemini. Note that
`STATUS_URL` goes through the HTTP client, so a `file://` URL will **not** work.

To exercise the quota path against a fixture, point `CLOUDCODE_ENDPOINT` at a
local server that answers both `:loadCodeAssist` and `:retrieveUserQuotaSummary`.

## Notes & possible extensions

- **Consumer app meters** — see the caveat at the top. Reaching
  `gemini.google.com/usage` needs browser session cookies and cookie rotation;
  that is a deliberate non-goal, not a missing feature.
- **Two-line display** — MenuMeters-style stacked numbers need a custom view in
  the `NSStatusItem` button, so the title is a single line. `src/tray.rs` owns the
  status item directly, so that is where a stacked variant would go.
- **More windows in the title** - the dropdown shows up to four windows (two per
  model group); the title keeps to the Gemini pair to keep the menu-bar footprint
  small.
- **Per-model quotas** - `:retrieveUserQuota` returns one bucket per model id
  (`gemini-3.5-flash-low`, `claude-sonnet-5-5-high`, ...). The summary's groups
  are what `agy`'s `/usage` panel shows, so the app reads those.

## Disclaimer

This is an independent, unofficial project. It is not affiliated with, endorsed
by or supported by Google. Gemini, Antigravity and Google Cloud are trademarks of
Google LLC.

The quota data comes from Google's internal, undocumented `v1internal` endpoints,
which the app calls the way Antigravity CLI does. Google can change or close them
at any time, and the app can then stop working. The app only reads your own
account's quota with credentials that are already on your Mac.

## License

This project is licensed under the [MIT License](LICENSE).
