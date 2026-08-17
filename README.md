# Gemini Usage — macOS menu-bar meter

A tiny menu-bar app that shows your **Gemini Code Assist quota** as percentages
in the menu-bar title — `<pro>% · <flash>%`, the two headline model tiers:

- **First number** → the **Pro** tier's quota used.
- **Second number** → the **Flash** tier's quota used.

The dropdown shows every tier the endpoint reports with its reset time, the
Gemini service status, *Open usage page*, *Refresh now*, the last-checked time,
*Launch at Login*, *About*, and *Quit*. When a tier first crosses a threshold
(default 80%) you get a Notification Centre banner.

It is a sibling of [claude-usage](https://github.com/lobner/claude-usage) and
[github-usage](https://github.com/lobner/github-usage), and deliberately keeps
the same menu structure.

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

**What it reads instead** is the quota the same Google account has on **Gemini
Code Assist** — the meters behind the `gemini` CLI, which the CLI itself renders
in its usage view. These come from a scoped OAuth token you already own, they are
real, and they are **not** the consumer app's 5-hour and weekly windows. The
*Open usage page* row still opens `gemini.google.com/usage` for convenience, and
its tooltip says which is which.

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

Unlike claude-usage — whose usage endpoint refuses more than about one request per
two minutes and forced a 120 s floor — the Code Assist quota endpoint is
comfortable at a normal poll rate. The Gemini CLI itself refreshes its cached copy
whenever it is more than 30 s old. **Do not port claude-usage's floor over**; it is
not the same endpoint and not the same limit.

- **The status dashboard** — no credentials, no limit — every `POLL_SECONDS`
  (60 s), so the outage dot stays responsive.
- **The quota endpoint** — every `USAGE_SECONDS` (60 s, floor 30 s).

If a `429` ever does arrive it is treated as expected rather than as a failure:
the percentages stay on screen, the menu says *"Rate limited; retrying 12:34:56"*,
and the wait doubles up to 15 minutes until a call succeeds. The server's
`Retry-After` is honoured when it asks for longer. Genuine failures name their
reason instead of just "failed".

*Refresh now* ignores the pacing and calls the endpoint immediately.

## How it works

Every `USAGE_SECONDS` it makes up to two calls against
`https://cloudcode-pa.googleapis.com/v1internal` — the same internal endpoint the
Gemini CLI uses — with the Gemini CLI's stored OAuth token from
`~/.gemini/oauth_creds.json`:

1. `:loadCodeAssist` → resolves the account's Code Assist project id. The result
   is cached for the life of the process, so this is normally paid once; setting
   `GOOGLE_CLOUD_PROJECT` skips it entirely.
2. `:retrieveUserQuota` with `{"project": "<id>"}` → a list of buckets, each with
   a `modelId`, a `remainingFraction`, an optional `remainingAmount` and a
   `resetTime`.

Buckets are grouped into **tiers** (Pro / Flash / Flash Lite) by matching the
model id, and **the worst bucket in each tier wins** — several model ids can share
a tier's pool and the binding constraint is whichever has least left. This is what
the Gemini CLI's own quota display does. `remainingFraction` is inverted into a
*used* percentage, which is the direction every other meter in this app reads.

A model id that matches no known tier still gets its own row rather than being
dropped: Google adds model ids faster than any mapping can be updated.

There is no officially documented API for this data.

## ⚠️ OAuth token & refresh — read this

The app is **read-only**: it uses whatever OAuth token the Gemini CLI has stored
and **never refreshes or writes it**. That token is short-lived, and only running
the `gemini` CLI refreshes it. So:

- While the token is valid, the numbers refresh every `USAGE_SECONDS` as expected.
- Once it expires, every poll returns "token expired", the title shows **"⚠"**,
  the numbers stop updating, and the menu shows **"⚠ OAuth token expired — run a
  `gemini` command to refresh"** until you next use the CLI.
- If you have never logged in, the menu says **"⚠ Gemini CLI not logged in"**.
- If the account has no Code Assist project at all, it says **"⚠ No Code Assist
  quota"** and backs right off, since nothing about that resolves on its own.

## Design decision: read-only credentials, no auto-refresh

This app **only reads** the OAuth token the Gemini CLI already stored, and **never
writes credentials, refreshes tokens, or runs its own auth flow**. That is a
deliberate choice, not an oversight — and it is why the consumer usage page is out
of scope, since reaching that *requires* rotating a session cookie.

The refresh token is sitting right there in `~/.gemini/oauth_creds.json`, and
Google's token endpoint is a standard OAuth `grant_type=refresh_token` exchange,
so auto-refresh would be less exotic here than it is in claude-usage. It is still
off, for the same reason: the credentials belong to the Gemini CLI. Writing them
back risks racing with the CLI's own refresh — last writer wins, so either side can
clobber the other's fresh token and break the session. A meter silently corrupting
your login is a far worse failure than a meter that shows `⚠` until you next touch
the CLI.

If you add it later, gate it behind an env var (e.g. `AUTO_REFRESH=1`) so the
default install stays read-only, refresh only when actually expired, re-read
immediately before writing, and treat a `401` after a fresh write as "the CLI
already rotated it — re-read and retry once".

## Build & run

Requires Go 1.22+ (this repo pins `golang 1.25.11` via `.tool-versions`).

```sh
# Run in the foreground (Ctrl-C to stop):
go run .

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
| `ALERT_PERCENT`           | `80`    | Banner when a tier first reaches this %. `0` disables alerts. |
| `STATUS_URL`              | `https://www.google.com/appsstatus/dashboard/incidents.json` | Incident feed to poll for the outage dot. |
| `CODE_ASSIST_ENDPOINT`    | `https://cloudcode-pa.googleapis.com` | Quota host. Same name the Gemini CLI uses. |
| `CODE_ASSIST_API_VERSION` | `v1internal` | Quota API version. |
| `GOOGLE_CLOUD_PROJECT`    | —       | Skip project discovery and use this project id.               |

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
bundle (`go run .`), since login items are bundles.

Each launch also *reconciles*: if the record says registered, the app registers
again. That is a no-op when it already is, and it repairs the case where the login
item was dropped — which happens whenever the bundle is replaced, as
`make-app.sh --install` does. If macOS instead reports the item as switched off,
that is left alone: it means someone turned it off in System Settings, and the
checkbox will show unticked until you tick it again.

Three notes on the mechanics:

- **The bundle is ad-hoc signed with its own identifier.** The Go linker already
  ad-hoc signs, but it calls every binary it produces `a.out`, and macOS keys
  login items by signing identity — so two unsigned Go apps look like one item and
  registering either one evicts the other's *Open at Login* entry. `make-app.sh`
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
main.go                 systray wiring, poll loop, menu, threshold banners
internal/usage/         read the Gemini CLI's OAuth token + query Code Assist quota → tier meters
internal/status/        poll the Workspace Status Dashboard feed → indicator, incidents, products
internal/icon/          the red status dot, drawn programmatically (no binary assets)
internal/login/         open-at-login registration via SMAppService (cgo)
internal/notify/        Notification Centre banner + confirm dialog via osascript
build/                  Info.plist, make-app.sh, LaunchAgent plist
third_party/systray/    vendored fork of fyne.io/systray (see below)
```

`internal/icon`, `internal/login` and `internal/notify` are **duplicated** from
the sibling apps rather than shared through a module. Fix a bug in one, port it to
the others.

The app pins a **local fork of `fyne.io/systray`** under `third_party/systray`,
wired in via a `replace` directive in `go.mod`. The only change is a one-method
patch to `show_menu` in `systray_darwin.m`: it attaches the menu to the status
item and triggers it via the button so AppKit positions the dropdown directly
below the menu bar. Upstream pops the menu at the button's zero origin, which
renders it *over* the menu-bar icons until a mouse move forces a relayout. To
re-sync the fork to a newer upstream, re-copy it from the module cache and
re-apply that patch.

## Testing

```sh
go test ./...   # unit tests (offline)

# Opt-in diagnostics against the live endpoints:
go test -tags livetest -run TestLiveFetch  -v ./internal/usage   # needs a valid `gemini` login
go test -tags livetest -run TestLiveStatus -v ./internal/status  # needs network only
```

To see the outage UI without waiting for a real incident, point `STATUS_URL` at a
saved incident feed served locally (e.g. `python3 -m http.server`) with an entry
whose `end` field is removed and whose `affected_products` names Gemini. Note that
`STATUS_URL` goes through `http.Client`, so a `file://` URL will **not** work.

To exercise the quota path against a fixture, point `CODE_ASSIST_ENDPOINT` at a
local server that answers both `:loadCodeAssist` and `:retrieveUserQuota`.

## Notes & possible extensions

- **Consumer app meters** — see the caveat at the top. Reaching
  `gemini.google.com/usage` needs browser session cookies and cookie rotation;
  that is a deliberate non-goal, not a missing feature.
- **Two-line display** — MenuMeters-style stacked numbers aren't possible through
  stock `fyne.io/systray` (it force-sizes icon images to 16×16), so the title is a
  single line. A stacked variant would need a custom `NSStatusItem` view — an
  extension of the fork already vendored under `third_party/systray`.
- **More tiers in the title** — the dropdown shows up to four tiers; the title
  keeps to two to keep the menu-bar footprint small.

## License

This project is licensed under the [MIT License](LICENSE).

It vendors a fork of [`fyne.io/systray`](https://github.com/fyne-io/systray)
under `third_party/systray/`, which is licensed under the Apache License 2.0 and
retains its own [`LICENSE`](third_party/systray/LICENSE).
