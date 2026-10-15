// Command geminiusage is a macOS menu-bar app that shows your Antigravity
// Gemini quota as percentages in the menu-bar title - "<5h>% · <weekly>%", the
// Gemini group's two limits - and lists every model group's windows with their
// reset times in the dropdown. It reads the quota summary that Antigravity CLI
// (`agy`) shows in /usage, directly from Google with agy's stored OAuth token,
// and posts a Notification Centre banner when a window crosses a threshold.
//
// It also polls Google's Workspace Status Dashboard: whenever a Gemini product
// has a running incident, a red dot appears in front of the percentages and the
// dropdown lists what is going on.
//
// # What these numbers are, and are not
//
// These are Antigravity quotas - the meters behind `agy`. They are NOT the
// Gemini app's meters shown at https://gemini.google.com/usage, which has no
// token-authenticated API and is reachable only with whole-account Google
// session cookies. See internal/usage and the README for why this app
// deliberately does not go there.
//
// The two sources are polled at different rates. The status dashboard needs no
// credentials and is not rate limited, so it is read every POLL_SECONDS. The
// quota endpoint is cheap but not free, so it gets its own USAGE_SECONDS cadence
// and backs off if it is ever refused.
//
// Environment overrides:
//
//	POLL_SECONDS            status-dashboard interval in seconds (default 60, minimum 10)
//	USAGE_SECONDS           quota-endpoint interval in seconds (default 60, minimum 30)
//	ALERT_PERCENT           notify when a tier first reaches this % (default 80, 0=off)
//	STATUS_URL              incident feed to poll (default the Workspace dashboard)
//	CODE_ASSIST_ENDPOINT    quota host (default https://cloudcode-pa.googleapis.com)
//	CODE_ASSIST_API_VERSION quota API version (default v1internal)
//	GOOGLE_CLOUD_PROJECT    skip project discovery and use this project id
package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"fyne.io/systray"

	"geminiusage/internal/icon"
	"geminiusage/internal/login"
	"geminiusage/internal/notify"
	"geminiusage/internal/status"
	"geminiusage/internal/usage"
)

const (
	defaultPollInterval = 60 * time.Second

	blinkInterval = 700 * time.Millisecond

	// Unlike Anthropic's usage endpoint - which allows roughly one request per
	// two minutes per token and forced claude-usage's 120 s floor - the Cloud
	// Code quota endpoint has been comfortable at this rate. Do not copy
	// claude-usage's floor over; it is not the same endpoint and not the same
	// limit. A 429 still backs off.
	defaultUsageInterval = 60 * time.Second
	minUsageInterval     = 30 * time.Second
	maxUsageBackoff      = 15 * time.Minute

	// The Gemini app's usage page. It is NOT where these numbers come from - see
	// the package comment - but it is the page people mean by "Gemini usage", so
	// the menu offers it with a tooltip that says which is which.
	usagePageURL = "https://gemini.google.com/usage"
	// The API key row opens AI Studio usage, which shows the same project's
	// requests and spend.
	aiStudioUsageURL = "https://aistudio.google.com/usage"
	statusPageURL    = "https://www.google.com/appsstatus/dashboard/"

	appName         = "Gemini Usage"
	repoURL         = "https://github.com/lobner/gemini-usage"
	maxTierRows     = 4  // fixed pool of tier rows (systray can't remove items)
	maxStatusItems  = 5  // fixed pool of incident rows
	statusRowMaxLen = 64 // characters per incident row
)

// Build stamps, set by build/make-app.sh with -ldflags. A plain `go build` or
// `go run .` leaves them at these defaults, which is what "dev" in the About row
// means.
var (
	version   = "dev"
	commit    = ""
	buildDate = ""
)

// noIcon is deliberately undecodable image data: systray has no "remove the
// icon" call, but AppKit turns data it cannot decode into a nil NSImage, which
// drops the image from the status item (leaving the title text alone).
var noIcon = []byte{0}

var (
	pollInterval  = pollIntervalFromEnv()
	usageInterval = usageIntervalFromEnv()
	alertPercent  = alertPercentFromEnv()
	statusURL     = envOr("STATUS_URL", status.DefaultURL)

	// Tier rows come in pairs — the meter and its reset line — because the number
	// of tiers the endpoint returns is Google's to decide, not ours.
	mTiers       []*systray.MenuItem
	mTierResets  []*systray.MenuItem
	mStatus      *systray.MenuItem
	mStatusItems []*systray.MenuItem
	mAffected    *systray.MenuItem
	mLastCheck   *systray.MenuItem
	mLogin       *systray.MenuItem
	mKey         *systray.MenuItem

	// The API key's last good reading, poll-goroutine-only. It stands in for
	// the title when there are no quota meters.
	lastKey *usage.KeyUsage

	refreshNow = make(chan struct{}, 1)

	// Icon state, owned by iconLoop. checkStatus reports what the dashboard
	// says; opening the menu acknowledges an outage.
	iconUpdates = make(chan iconUpdate, 1)
	iconAck     = make(chan struct{}, 1)

	// Poll-goroutine-only state.
	firstRun      = true
	alerted       = map[string]bool{}
	haveStatus    bool
	lastIncidents = map[string]bool{}

	// Quota-endpoint pacing, also poll-goroutine-only. nextUsage is when the
	// endpoint may be called again; backoff grows while it keeps saying no.
	nextUsage    time.Time
	usageBackoff time.Duration
	nextKey      time.Time // when the API key row may be read again

	// Shared between the poll goroutine (writer) and click goroutines (readers).
	mu           sync.Mutex
	incidentURLs = make([]string, maxStatusItems)
)

func main() {
	systray.Run(onReady, onExit)
}

func onReady() {
	systray.SetTitle("…")
	systray.SetTooltip("Gemini usage — checking…")

	// Pre-allocate the tier rows; they are relabelled and shown/hidden per poll.
	for i := 0; i < maxTierRows; i++ {
		mi := systray.AddMenuItem("", "")
		mi.Disable()
		mi.Hide()
		mTiers = append(mTiers, mi)

		reset := systray.AddMenuItem("", "")
		reset.Disable()
		reset.Hide()
		mTierResets = append(mTierResets, reset)
	}
	mTiers[0].SetTitle("Antigravity quota: …")
	mTiers[0].Show()

	systray.AddSeparator()

	// The Gemini API key's row, hidden until a key is found. It sits below the
	// separator rather than above one of its own: systray cannot hide a
	// separator, so a row-own separator would leave a double line with no key.
	mKey = systray.AddMenuItem("", "Open AI Studio usage for the API key's project")
	mKey.Hide()

	mStatus = systray.AddMenuItem("Service status: checking…", "Open the Google Workspace Status Dashboard")

	// Pre-allocate a fixed pool of incident rows, shown/hidden and relabelled on
	// each poll.
	for i := 0; i < maxStatusItems; i++ {
		mi := systray.AddMenuItem("", "")
		mi.Hide()
		mStatusItems = append(mStatusItems, mi)
		go func(idx int) {
			for range mi.ClickedCh {
				openURL(incidentURL(idx))
			}
		}(i)
	}
	mAffected = systray.AddMenuItem("", "")
	mAffected.Disable()
	mAffected.Hide()

	systray.AddSeparator()
	mOpenPage := systray.AddMenuItem("Open usage page",
		"Open gemini.google.com/usage - the Gemini app's own meters, which are not the Antigravity quota shown above")
	mRefresh := systray.AddMenuItem("Refresh now", "Fetch quota immediately")
	mLastCheck = systray.AddMenuItem("", "")
	mLastCheck.Disable()
	systray.AddSeparator()

	mLogin = systray.AddMenuItemCheckbox("Launch at Login",
		"Open Gemini Usage automatically when you log in", login.Enabled())
	if login.BundlePath() == "" || !login.Supported() {
		mLogin.Hide() // nothing to register: not a bundle, or macOS 12 or earlier
	}
	systray.AddSeparator()

	mAbout := systray.AddMenuItem(aboutTitle(), aboutTooltip())
	mQuit := systray.AddMenuItem("Quit", "Quit Gemini Usage")

	go func() {
		for range mKey.ClickedCh {
			openURL(aiStudioUsageURL)
		}
	}()
	go func() {
		for range mOpenPage.ClickedCh {
			openURL(usagePageURL)
		}
	}()
	go func() {
		for range mStatus.ClickedCh {
			openURL(statusPageURL)
		}
	}()
	go func() {
		for range mRefresh.ClickedCh {
			triggerRefresh()
		}
	}()
	go func() {
		for range mLogin.ClickedCh {
			toggleLaunchAtLogin()
		}
	}()
	go func() {
		for range mAbout.ClickedCh {
			openURL(releaseNotesURL())
		}
	}()
	go func() {
		<-mQuit.ClickedCh
		systray.Quit()
	}()

	// Opening the menu means the user has seen the outage: stop the blinking.
	go func() {
		for range systray.TrayOpenedCh {
			select {
			case iconAck <- struct{}{}:
			default: // an acknowledgement is already queued
			}
		}
	}()

	go iconLoop()
	go pollLoop()
	go offerLaunchAtLogin()
}

func onExit() {}

// aboutTitle names the app and the build in one line, so the version is readable
// without clicking anything.
func aboutTitle() string { return "About " + appName + " " + version }

// aboutTooltip carries what is needed to identify a build in a bug report. The
// architecture is included because the released archives are Apple silicon only,
// and the commit because these builds are unsigned — "which build is that?" is
// otherwise unanswerable.
func aboutTooltip() string {
	parts := make([]string, 0, 4)
	if commit != "" {
		parts = append(parts, "commit "+commit)
	}
	if buildDate != "" {
		parts = append(parts, "built "+buildDate)
	}
	parts = append(parts, runtime.GOOS+"/"+runtime.GOARCH)
	return strings.Join(parts, " · ") + " — click for the release notes"
}

// releaseNotesURL points at this exact version's notes when the build came from a
// clean tag, and at the release list otherwise, since there is nothing to link a
// dev build to.
func releaseNotesURL() string {
	if strings.HasPrefix(version, "v") && !strings.ContainsAny(version, "-+") {
		return repoURL + "/releases/tag/" + version
	}
	return repoURL + "/releases"
}

// offerLaunchAtLogin asks — once — whether the app should open at login, and
// registers it as a login item if so. It stays quiet when there is nothing to
// offer: when the user has already answered either way, when the binary is not
// running from its .app bundle, or on a macOS without SMAppService.
func offerLaunchAtLogin() {
	if login.BundlePath() == "" || !login.Supported() {
		return
	}

	// Already answered: don't ask again, but do make the system match what we
	// recorded — see login.Reconcile for why that can't be read back instead.
	if login.Answered() {
		if login.Answer() {
			if login.Reconcile() {
				mLogin.Check()
			} else {
				mLogin.Uncheck()
			}
		}
		return
	}

	yes, err := notify.Confirm(
		"Launch Gemini Usage at login?",
		"Gemini Usage can start automatically when you log in, so your model quota is always in the menu bar.",
		"Launch at Login", "Not Now")
	if err != nil {
		return // the dialog itself failed; try again next launch rather than guess
	}
	if !yes {
		_ = login.RecordAnswer(false)
		return
	}

	if err := login.Enable(); err != nil {
		// Don't record the answer, so the offer is made again next launch.
		_ = notify.Banner("Gemini Usage", "Could not open at login: "+err.Error())
		return
	}
	_ = login.RecordAnswer(true)
	mLogin.Check()
	warnIfNeedsApproval()
}

// toggleLaunchAtLogin flips the Launch at Login checkbox. The checkbox is only
// ticked once the change has actually gone through, so a failure leaves the menu
// showing what is really the case.
func toggleLaunchAtLogin() {
	if mLogin.Checked() {
		if err := login.Disable(); err != nil {
			_ = notify.Banner("Gemini Usage", "Could not stop opening at login: "+err.Error())
			return
		}
		_ = login.RecordAnswer(false)
		mLogin.Uncheck()
		return
	}

	if err := login.Enable(); err != nil {
		_ = notify.Banner("Gemini Usage", "Could not open at login: "+err.Error())
		return
	}
	_ = login.RecordAnswer(true)
	mLogin.Check()
	warnIfNeedsApproval()
}

// warnIfNeedsApproval covers the case where the user has previously switched the
// login item off in System Settings: registering succeeds, but macOS keeps it
// held back until they switch it on there.
func warnIfNeedsApproval() {
	if login.NeedsApproval() {
		_ = notify.Banner("Gemini Usage",
			"Switch Gemini Usage on under System Settings → General → Login Items to finish enabling it.")
	}
}

func pollLoop() {
	check() // immediate first check
	t := time.NewTicker(pollInterval)
	defer t.Stop()
	for {
		select {
		case <-t.C:
			check()
		case <-refreshNow:
			// "Refresh now" means now: drop the pacing so the quota endpoint is
			// actually called. If it refuses, the menu says so.
			nextUsage = time.Time{}
			usageBackoff = 0
			usage.ForgetAPIKey()
			nextKey = time.Time{}
			check()
		}
	}
}

func check() {
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()

	// The status dashboard needs no credentials and isn't rate limited, so check
	// it first and on every poll — the dot must keep working while the quota
	// endpoint is unavailable for any reason.
	checkStatus(ctx)

	// The key row has its own pacing, so a long quota back-off does not freeze
	// it.
	if !time.Now().Before(nextKey) {
		checkAPIKey(ctx)
		nextKey = time.Now().Add(usageInterval)
	}

	// Respect the pacing rather than calling the endpoint on every poll.
	if time.Now().Before(nextUsage) {
		return
	}

	u, err := usage.Fetch(ctx)
	now := time.Now()
	stamp := now.Format("15:04:05")

	switch {
	case errors.Is(err, usage.ErrRateLimited):
		// Not a fault: keep the numbers on screen and space out.
		var rl usage.RateLimited
		errors.As(err, &rl)
		wait := backOffUsage(rl.RetryAfter)
		nextUsage = now.Add(wait)
		mLastCheck.SetTitle("Rate limited; retrying " + nextUsage.Format("15:04:05"))
		mLastCheck.SetTooltip(fmt.Sprintf("The quota endpoint refused the request. Backing off %s.",
			wait.Round(time.Second)))
		return

	case errors.Is(err, usage.ErrNoCredentials):
		showProblem("⚠ Antigravity CLI not logged in", "Run `agy` once to sign in",
			"Gemini usage - not signed in to Antigravity", "Not signed in "+stamp, err)
		keyTitle()
		nextUsage = now.Add(usageInterval)
		return

	case errors.Is(err, usage.ErrTokenExpired):
		// Keep the last known numbers; just flag that the token needs refreshing.
		showProblem("⚠ OAuth token expired", "Run `agy` to refresh",
			"Gemini usage - token expired; refresh by running agy", "Token expired "+stamp, nil)
		nextUsage = now.Add(usageInterval)
		return

	case errors.Is(err, usage.ErrNotEligible):
		detail := "This account has no Antigravity quota"
		var ne usage.NotEligible
		if errors.As(err, &ne) && ne.Code == "SUBSCRIPTION_REQUIRED" {
			detail = "Google wants a licence for this account"
		}
		showProblem("⚠ No Antigravity quota", detail,
			"Gemini usage - no Antigravity quota for this account", "Not eligible "+stamp, err)
		if ne.Message != "" {
			mTierResets[0].SetTooltip(ne.Message)
		}
		keyTitle()
		// Nothing about this resolves on its own, so back right off rather than
		// asking the same doomed question every minute.
		nextUsage = now.Add(maxUsageBackoff)
		return

	case err != nil:
		// Say what went wrong: a bare "failed" is indistinguishable from the
		// rate limiting above, which is what made this hard to diagnose.
		systray.SetTooltip("Gemini usage — update failed")
		mLastCheck.SetTitle("Last check failed " + stamp + " — " + truncate(reason(err), 48))
		mLastCheck.SetTooltip(err.Error())
		nextUsage = now.Add(usageInterval)
		return
	}

	usageBackoff = 0
	nextUsage = now.Add(usageInterval)

	updateUI(u)
	notifyThresholds(u)
	mLastCheck.SetTitle("Last checked " + stamp)
	mLastCheck.SetTooltip("")
}

// checkAPIKey reads the Gemini API key's request count into the key row. No
// key is normal and hides the row; other problems show in the row itself, so
// they never hide the quota meters.
func checkAPIKey(ctx context.Context) {
	k, err := usage.FetchAPIKey(ctx)
	switch {
	case err == nil:
		lastKey = &k
		mKey.SetTitle(fmt.Sprintf("API key: %s today", plural(k.Requests, "request")))
		mKey.SetTooltip(fmt.Sprintf("Gemini API generateContent requests in project %s since %s (midnight Pacific). Click for AI Studio usage.",
			k.Project, k.Since.Local().Format("15:04")))
	case errors.Is(err, usage.ErrNoAPIKey):
		lastKey = nil
		mKey.Hide()
		return
	case errors.Is(err, usage.ErrKeyInvalid):
		lastKey = nil
		mKey.SetTitle("⚠ API key not valid - replace GEMINI_API_KEY")
		mKey.SetTooltip("Google rejected the key. Get a new one at aistudio.google.com/apikey.")
	case errors.Is(err, usage.ErrNoADC):
		mKey.SetTitle("⚠ API key: run gcloud auth application-default login")
		mKey.SetTooltip("The request count comes from Cloud Monitoring, which needs a gcloud login. " + err.Error())
	case errors.Is(err, usage.ErrRateLimited):
		return // keep the last reading on screen
	default:
		mKey.SetTitle("API key: count failed - " + truncate(reason(err), 40))
		mKey.SetTooltip(err.Error())
	}
	mKey.Show()
}

// keyTitle puts the API key's request count in the title when there are no
// quota meters, so an API-key-only setup does not show "⚠" for ever.
func keyTitle() {
	if lastKey == nil {
		return
	}
	systray.SetTitle(fmt.Sprintf("%d req", lastKey.Requests))
}

func plural(n int64, word string) string {
	if n == 1 {
		return "1 " + word
	}
	return fmt.Sprintf("%d %ss", n, word)
}

// showProblem puts a credential-shaped problem in the first two tier rows and
// hides the rest, so the menu explains itself instead of showing stale numbers
// with no hint of why they stopped moving.
func showProblem(headline, detail, tooltip, lastCheck string, err error) {
	systray.SetTitle("⚠")
	systray.SetTooltip(tooltip)

	mTiers[0].SetTitle(headline)
	mTiers[0].Show()
	mTierResets[0].SetTitle("    " + detail)
	mTierResets[0].Show()
	for i := 1; i < maxTierRows; i++ {
		mTiers[i].Hide()
		mTierResets[i].Hide()
	}

	mLastCheck.SetTitle(lastCheck)
	if err != nil {
		mLastCheck.SetTooltip(err.Error())
	} else {
		mLastCheck.SetTooltip("")
	}
}

// backOffUsage doubles the wait each time the endpoint refuses, from one usage
// interval up to maxUsageBackoff, and takes the server's Retry-After instead when
// that asks for longer.
func backOffUsage(retryAfter time.Duration) time.Duration {
	if usageBackoff <= 0 {
		usageBackoff = usageInterval
	} else if usageBackoff < maxUsageBackoff {
		usageBackoff *= 2
	}
	if usageBackoff > maxUsageBackoff {
		usageBackoff = maxUsageBackoff
	}
	if retryAfter > usageBackoff {
		return retryAfter
	}
	return usageBackoff
}

// reason trims an error down to something that fits in a menu row: the first
// line, without the multi-line JSON body some failures carry.
func reason(err error) string {
	s := err.Error()
	if i := strings.IndexAny(s, "\n\r"); i >= 0 {
		s = s[:i]
	}
	return strings.TrimSpace(strings.TrimSuffix(strings.TrimSpace(s), ":"))
}

func updateUI(u usage.Usage) {
	systray.SetTitle(title(u))
	systray.SetTooltip("Antigravity quota - " + tooltipSummary(u))

	for i := 0; i < maxTierRows; i++ {
		if i >= len(u.Tiers) {
			mTiers[i].Hide()
			mTierResets[i].Hide()
			continue
		}
		m := u.Tiers[i]
		mTiers[i].SetTitle(fmt.Sprintf("%s: %d%%", m.Name, m.Percent))
		mTiers[i].SetTooltip(fmt.Sprintf("%d%% of the %s limit used (bucket %s)", m.Percent, m.Name, m.Bucket))
		mTiers[i].Show()
		setReset(mTierResets[i], m)
	}
}

// title renders the menu-bar text: the headline tiers' percentages, matching
// claude-usage's "<a>% · <b>%" shape.
func title(u usage.Usage) string {
	h := u.Headline()
	if len(h) == 0 {
		return "…"
	}
	parts := make([]string, 0, len(h))
	for _, m := range h {
		parts = append(parts, fmt.Sprintf("%d%%", m.Percent))
	}
	return strings.Join(parts, " · ")
}

func tooltipSummary(u usage.Usage) string {
	parts := make([]string, 0, len(u.Tiers))
	for _, m := range u.Tiers {
		parts = append(parts, fmt.Sprintf("%s %d%%", m.Name, m.Percent))
	}
	if len(parts) == 0 {
		return "no windows reported"
	}
	return strings.Join(parts, " · ")
}

func setReset(mi *systray.MenuItem, m usage.Meter) {
	if !m.HasReset {
		mi.Hide()
		return
	}
	if m.Idle {
		// Google reports "now + window" until the first request, so a date
		// here would move on every poll.
		mi.SetTitle("    resets " + windowWords(usage.WindowLength(m.Window)) + " after first use")
	} else {
		mi.SetTitle("    resets " + humanizeReset(m.ResetsAt))
	}
	mi.Show()
}

// humanizeReset formats a reset time as a short relative string for windows
// resetting within a day ("in 1h 59m"), or an absolute weekday/time otherwise
// ("Wed 22:59").
// windowWords says a window length the way a person would: "5h", "7 days".
func windowWords(d time.Duration) string {
	if d >= 24*time.Hour && d%(24*time.Hour) == 0 {
		return fmt.Sprintf("%d days", int(d/(24*time.Hour)))
	}
	return fmt.Sprintf("%dh", int(d.Hours()))
}

func humanizeReset(t time.Time) string {
	d := time.Until(t)
	switch {
	case d <= 0:
		return "now"
	case d < time.Hour:
		return fmt.Sprintf("in %dm", int(d.Minutes()))
	case d < 24*time.Hour:
		return fmt.Sprintf("in %dh %dm", int(d.Hours()), int(d.Minutes())%60)
	default:
		// With the date: a weekly window resets on the same weekday, so
		// "Thu 16:52" alone reads as today.
		return "on " + t.Local().Format("Mon 2/1 15:04")
	}
}

// notifyThresholds fires a banner the first time a tier reaches alertPercent,
// re-arming once it drops back below (e.g. after a reset). The first poll only
// establishes a baseline so launching while already high doesn't spam.
func notifyThresholds(u usage.Usage) {
	if alertPercent <= 0 {
		return
	}
	for _, m := range u.Tiers {
		high := m.Percent >= alertPercent
		if !firstRun && high && !alerted[m.Name] {
			_ = notify.Banner(m.Name+" quota high",
				fmt.Sprintf("%s at %d%%", m.Name, m.Percent))
		}
		alerted[m.Name] = high
	}
	firstRun = false
}

// checkStatus polls the Workspace dashboard and refreshes the dot and the
// incident rows. A failed poll keeps whatever was last known — a flaky network
// must not silently claim everything is fine.
func checkStatus(ctx context.Context) {
	s, err := status.Fetch(ctx, statusURL)
	if err != nil {
		if !haveStatus {
			mStatus.SetTitle("Service status unavailable")
		}
		return
	}
	haveStatus = true
	updateStatusUI(s)
	// Outside updateStatusUI on purpose: that holds mu, and this send can block
	// while a menu is open, which would stall the click handlers that need mu.
	reportStatusToIcon(s)
}

func updateStatusUI(s status.Summary) {
	mu.Lock()
	defer mu.Unlock()

	for i := range incidentURLs {
		incidentURLs[i] = ""
	}

	if s.AllOperational() {
		mStatus.SetTitle("✓  All Gemini services operational")
		mStatus.SetTooltip("The Workspace Status Dashboard reports no running Gemini incidents")
		for _, mi := range mStatusItems {
			mi.Hide()
		}
		mAffected.Hide()
		return
	}

	desc := s.Description
	if desc == "" {
		desc = status.Human(s.Indicator) + " service disruption"
	}
	mStatus.SetTitle("●  " + desc)
	mStatus.SetTooltip("Google Workspace Status Dashboard — click to open it")

	if extra := len(s.Incidents) - maxStatusItems; extra > 0 {
		mStatus.SetTitle(fmt.Sprintf("●  %s  (showing %d of %d)", desc, maxStatusItems, len(s.Incidents)))
	}

	for i, mi := range mStatusItems {
		if i >= len(s.Incidents) {
			mi.Hide()
			continue
		}
		inc := s.Incidents[i]
		label := inc.Name
		if inc.Status != "" {
			label = status.Human(inc.Status) + " — " + inc.Name
		}
		mi.SetTitle("    " + truncate(label, statusRowMaxLen))
		mi.SetTooltip(incidentTooltip(inc))
		incidentURLs[i] = inc.URL
		mi.Show()
	}

	if len(s.Affected) == 0 {
		mAffected.Hide()
		return
	}
	names := make([]string, 0, len(s.Affected))
	for _, c := range s.Affected {
		names = append(names, fmt.Sprintf("%s (%s)", c.Name, status.Human(c.Status)))
	}
	list := strings.Join(names, ", ")
	mAffected.SetTitle("    Affected: " + truncate(list, statusRowMaxLen))
	mAffected.SetTooltip("Affected: " + list)
	mAffected.Show()
}

func incidentTooltip(inc status.Incident) string {
	tip := inc.Name
	if inc.LatestUpdate != "" {
		tip += " — " + inc.LatestUpdate
	}
	return truncate(tip, 300)
}

// reportStatusToIcon tells iconLoop what the dashboard says: whether anything is
// wrong, and whether any of it is new since the last poll. An outage with no
// listed incidents still counts as ongoing — the indicator alone is enough.
func reportStatusToIcon(s status.Summary) {
	cur := make(map[string]bool, len(s.Incidents))
	isNew := false
	for _, inc := range s.Incidents {
		cur[inc.ID] = true
		if !lastIncidents[inc.ID] {
			isNew = true
		}
	}
	lastIncidents = cur
	iconUpdates <- iconUpdate{ongoing: !s.AllOperational(), isNew: isNew}
}

// iconUpdate is what the latest status poll saw.
type iconUpdate struct{ ongoing, isNew bool }

type iconState int

const (
	stateOK    iconState = iota // all operational: no icon at all, text only
	stateAlert                  // unacknowledged outage: blinking red dot
	stateAck                    // seen by the user: steady red dot
)

// iconLoop owns the menu-bar icon. It is the only writer, so the blink can never
// race a state change and leave the wrong icon on screen. The dot blinks until
// the user opens the menu, then stays solid for as long as the outage lasts.
func iconLoop() {
	st := stateOK
	red := false

	setRed := func(on bool) {
		red = on
		if on {
			systray.SetIcon(icon.RedDotPNG())
		} else {
			// Blink off, not outage over: a transparent image of the same size
			// holds the icon slot so the percentages don't shift.
			systray.SetIcon(icon.BlankPNG())
		}
	}
	apply := func(next iconState) {
		st = next
		switch next {
		case stateOK:
			// All clear: drop the icon entirely, so it's text only again.
			red = false
			systray.SetIcon(noIcon)
		case stateAlert:
			setRed(true) // start the blink lit, so it is seen immediately
		case stateAck:
			red = true
			systray.SetIcon(icon.RedDotPNG())
		}
	}

	t := time.NewTicker(blinkInterval)
	defer t.Stop()

	for {
		select {
		case u := <-iconUpdates:
			switch {
			case !u.ongoing:
				if st != stateOK {
					apply(stateOK)
				}
			case u.isNew || st == stateOK:
				// A fresh incident re-arms the blink even if an earlier one has
				// already been acknowledged.
				apply(stateAlert)
			}
		case <-iconAck:
			if st == stateAlert {
				apply(stateAck)
			}
		case <-t.C:
			if st == stateAlert {
				setRed(!red)
			}
		}
	}
}

func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	if n < 1 {
		return ""
	}
	return string(r[:n-1]) + "…"
}

func incidentURL(i int) string {
	mu.Lock()
	defer mu.Unlock()
	if i < 0 || i >= len(incidentURLs) {
		return ""
	}
	return incidentURLs[i]
}

func triggerRefresh() {
	select {
	case refreshNow <- struct{}{}:
	default: // a refresh is already queued
	}
}

func openURL(u string) {
	if u == "" {
		return
	}
	_ = exec.Command("open", u).Start()
}

// usageIntervalFromEnv reads USAGE_SECONDS, refusing anything below
// minUsageInterval: the endpoint is cheap but not free, and the windows are
// hours long, so polling faster buys nothing.
func usageIntervalFromEnv() time.Duration {
	if v := os.Getenv("USAGE_SECONDS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			if d := time.Duration(n) * time.Second; d >= minUsageInterval {
				return d
			}
		}
	}
	return defaultUsageInterval
}

func pollIntervalFromEnv() time.Duration {
	if v := os.Getenv("POLL_SECONDS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n >= 10 {
			return time.Duration(n) * time.Second
		}
	}
	return defaultPollInterval
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func alertPercentFromEnv() int {
	if v := os.Getenv("ALERT_PERCENT"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n >= 0 && n <= 100 {
			return n
		}
	}
	return 80
}
