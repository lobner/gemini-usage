// Package usage fetches the Gemini quota windows that Antigravity CLI (`agy`)
// shows in its /usage panel - a 5-hour and a weekly limit per model group -
// from Google's Cloud Code API, using agy's stored OAuth token.
//
// It is READ-ONLY: it reads the token from the macOS Keychain (or the
// Antigravity app's token file) but never refreshes or writes it. The access
// token lasts about an hour and is refreshed only by running `agy`; once it
// has expired, Fetch returns ErrTokenExpired until the next refresh.
//
// # Why Antigravity
//
// Google moved Gemini CLI to Antigravity CLI on 2026-06-18. Since then the
// free tier ("Gemini Code Assist for individuals") and Google AI Pro/Ultra no
// longer serve Gemini CLI: loadCodeAssist answers UNSUPPORTED_CLIENT. The same
// Google account's quota now lives behind agy, on daily-cloudcode-pa. The older
// cloudcode-pa host is reported to always say 100% for the 5-hour window, so
// do not switch hosts without checking.
//
// # Why not gemini.google.com/usage
//
// The consumer usage page has no token-authenticated API. It is served by the
// Gemini web app and authenticated with whole-account Google session cookies,
// which rotate every few minutes and would have to be lifted out of the
// browser's cookie jar and kept alive by this app. This app deliberately does
// not go there.
//
// There is no officially documented API for this data - the endpoint is the
// same internal one agy calls (v1internal:retrieveUserQuotaSummary).
package usage

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	// defaultEndpoint is the host agy uses. CLOUDCODE_ENDPOINT overrides it,
	// so a build can be pointed at a fixture without a recompile.
	defaultEndpoint = "https://daily-cloudcode-pa.googleapis.com"
	apiVersion      = "v1internal"

	methodLoadCodeAssist = "loadCodeAssist"
	methodQuotaSummary   = "retrieveUserQuotaSummary"

	// agy's Keychain item, as written by its Go keyring library.
	keychainService = "gemini"
	keychainAccount = "antigravity"

	// fallbackAgyVersion goes in the User-Agent when the installed agy does
	// not answer --version. Google answers loadCodeAssist without a project
	// unless the User-Agent names Antigravity.
	fallbackAgyVersion = "1.3.5"
)

// ErrNoCredentials means agy has never been logged in on this machine, so
// there is nothing to read.
var ErrNoCredentials = errors.New("no Antigravity CLI credentials")

// ErrTokenExpired means the stored OAuth token is expired or was rejected.
// Callers should keep the last known meters and prompt the user to refresh
// (run `agy`), rather than treat it as a hard error.
var ErrTokenExpired = errors.New("oauth token expired")

// ErrNotEligible means the account has no Antigravity quota to read.
var ErrNotEligible = errors.New("no Antigravity quota for this account")

// NotEligible carries Google's reason when it offers the account no quota,
// from loadCodeAssist's ineligibleTiers or a 403's ErrorInfo reason. It
// matches ErrNotEligible.
type NotEligible struct {
	Code    string // e.g. UNSUPPORTED_CLIENT, SUBSCRIPTION_REQUIRED
	Message string
}

func (e NotEligible) Error() string {
	return fmt.Sprintf("%s: %s (%s)", ErrNotEligible, e.Message, e.Code)
}

func (e NotEligible) Unwrap() error { return ErrNotEligible }

// ErrRateLimited means the endpoint refused the request because it was called
// too often. This is rare, so it is treated as a transient fault: keep the
// last known meters and back off.
var ErrRateLimited = errors.New("rate limited")

// RateLimited carries how long the server asked us to wait.
type RateLimited struct {
	RetryAfter time.Duration // zero when the server didn't say anything useful
}

func (e RateLimited) Error() string {
	if e.RetryAfter > 0 {
		return fmt.Sprintf("rate limited, retry after %s", e.RetryAfter)
	}
	return ErrRateLimited.Error()
}

// Unwrap makes errors.Is(err, ErrRateLimited) work on a RateLimited value.
func (e RateLimited) Unwrap() error { return ErrRateLimited }

// Meter is one quota window of one model group.
type Meter struct {
	Name     string    // display label: "Gemini 5h", "Gemini weekly", ...
	Group    string    // the model group: "Gemini", "Claude and GPT"
	Window   string    // "5h" or "weekly"
	Bucket   string    // the bucketId it came from, for the tooltip
	Percent  int       // 0–100 used, rounded
	ResetsAt time.Time // when the window resets; zero if unknown
	HasReset bool
	// Idle means the window has not started: nothing used, and the reset is
	// one window length from now. Google moves such a reset forward on every
	// call, so it is not a real time to show.
	Idle bool
}

// Usage is the set of meters the menu bar renders: the model groups in the
// order Google returns them (Gemini first), the 5-hour window before the
// weekly one in each.
type Usage struct {
	Tiers []Meter
}

// Tier returns the meter for a display name, if the response carried one.
func (u Usage) Tier(name string) (Meter, bool) {
	for _, m := range u.Tiers {
		if strings.EqualFold(m.Name, name) {
			return m, true
		}
	}
	return Meter{}, false
}

// Headline picks the two meters shown in the menu-bar title: the Gemini
// group's 5-hour and weekly windows, or else the first group's.
func (u Usage) Headline() []Meter {
	if len(u.Tiers) == 0 {
		return nil
	}
	group := u.Tiers[0].Group
	for _, m := range u.Tiers {
		if strings.HasPrefix(m.Bucket, "gemini-") {
			group = m.Group
			break
		}
	}
	var out []Meter
	for _, m := range u.Tiers {
		if m.Group == group && len(out) < 2 {
			out = append(out, m)
		}
	}
	return out
}

var httpClient = &http.Client{Timeout: 20 * time.Second}

// projectCache holds the project id loadCodeAssist returns between polls. It
// costs a second round trip and does not change for the life of a login, so
// it is resolved once and reused; a 401 clears it along with everything else.
var projectCache struct {
	sync.Mutex
	id string
}

// Fetch reads agy's stored OAuth token and queries the quota summary.
func Fetch(ctx context.Context) (Usage, error) {
	cred, err := loadCredentials(ctx)
	if err != nil {
		return Usage{}, err
	}
	// Don't hammer the endpoint with a known-dead token: if it has already
	// expired locally, report it without a doomed request. A live 401 is still
	// handled in post(), in case the expiry is missing or wrong.
	if !cred.expiresAt.IsZero() && time.Now().After(cred.expiresAt) {
		return Usage{}, ErrTokenExpired
	}

	project, err := resolveProject(ctx, cred.accessToken)
	if err != nil {
		return Usage{}, err
	}

	body, err := post(ctx, cred.accessToken, methodQuotaSummary, map[string]any{"project": project})
	if err != nil {
		// A stale cached project id outlives its login; drop it so the next poll
		// resolves a fresh one rather than repeating a doomed request forever.
		if errors.Is(err, ErrTokenExpired) || errors.Is(err, ErrNotEligible) {
			forgetProject()
		}
		return Usage{}, err
	}
	return parseSummary(body)
}

// resolveProject returns the project id for the signed-in account, from the
// cache or from loadCodeAssist.
func resolveProject(ctx context.Context, token string) (string, error) {
	projectCache.Lock()
	cached := projectCache.id
	projectCache.Unlock()
	if cached != "" {
		return cached, nil
	}

	// ideType is what makes this an Antigravity call; together with the
	// User-Agent it decides which tiers Google offers.
	body, err := post(ctx, token, methodLoadCodeAssist, map[string]any{
		"metadata": map[string]any{
			"ideType":    "ANTIGRAVITY",
			"platform":   "PLATFORM_UNSPECIFIED",
			"pluginType": "GEMINI",
		},
	})
	if err != nil {
		return "", err
	}
	id, err := parseLoadCodeAssist(body)
	if err != nil {
		return "", err
	}

	projectCache.Lock()
	projectCache.id = id
	projectCache.Unlock()
	return id, nil
}

// parseLoadCodeAssist returns the project, or NotEligible with Google's
// reason when there is none.
func parseLoadCodeAssist(body []byte) (string, error) {
	var r struct {
		CloudaicompanionProject string `json:"cloudaicompanionProject"`
		IneligibleTiers         []struct {
			ReasonCode    string `json:"reasonCode"`
			ReasonMessage string `json:"reasonMessage"`
		} `json:"ineligibleTiers"`
	}
	if err := json.Unmarshal(body, &r); err != nil {
		return "", fmt.Errorf("parsing loadCodeAssist JSON: %w", err)
	}
	if id := strings.TrimSpace(r.CloudaicompanionProject); id != "" {
		return id, nil
	}
	if len(r.IneligibleTiers) > 0 {
		t := r.IneligibleTiers[0]
		return "", NotEligible{Code: t.ReasonCode, Message: t.ReasonMessage}
	}
	return "", ErrNotEligible
}

func forgetProject() {
	projectCache.Lock()
	projectCache.id = ""
	projectCache.Unlock()
}

func post(ctx context.Context, token, method string, payload any) ([]byte, error) {
	buf, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, methodURL(method), bytes.NewReader(buf))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", userAgent())

	resp, err := httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("%s request: %w", method, err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	switch {
	case resp.StatusCode == http.StatusUnauthorized:
		return nil, ErrTokenExpired
	case resp.StatusCode == http.StatusForbidden:
		// 403 is overloaded: an expired token and an account with no quota
		// both land on it, and the two need different advice in the menu, so
		// the body decides.
		if ne, ok := notEligible(body); ok {
			return nil, ne
		}
		return nil, ErrTokenExpired
	case resp.StatusCode == http.StatusTooManyRequests:
		return nil, RateLimited{RetryAfter: retryAfter(resp.Header.Get("Retry-After"))}
	case resp.StatusCode != http.StatusOK:
		return nil, fmt.Errorf("%s HTTP %d: %s", method, resp.StatusCode, snippet(body))
	}
	return body, nil
}

func methodURL(method string) string {
	endpoint := envOr("CLOUDCODE_ENDPOINT", defaultEndpoint)
	return strings.TrimSuffix(endpoint, "/") + "/" + apiVersion + ":" + method
}

func envOr(key, def string) string {
	if v := strings.TrimSpace(os.Getenv(key)); v != "" {
		return v
	}
	return def
}

// notEligible spots the 403s that mean the account has no quota here - no
// licence, a disabled API, no project - rather than a stale token.
func notEligible(body []byte) (NotEligible, bool) {
	var r struct {
		Error struct {
			Status  string `json:"status"`
			Message string `json:"message"`
			Details []struct {
				Reason string `json:"reason"`
			} `json:"details"`
		} `json:"error"`
	}
	if err := json.Unmarshal(body, &r); err != nil || r.Error.Status != "PERMISSION_DENIED" {
		return NotEligible{}, false
	}
	for _, d := range r.Error.Details {
		if d.Reason == "SUBSCRIPTION_REQUIRED" {
			return NotEligible{Code: d.Reason, Message: r.Error.Message}, true
		}
	}
	msg := strings.ToLower(r.Error.Message)
	for _, hint := range []string{"has not been used", "is disabled", "not eligible", "valid license"} {
		if strings.Contains(msg, hint) {
			return NotEligible{Code: r.Error.Status, Message: r.Error.Message}, true
		}
	}
	return NotEligible{}, false
}

// retryAfter reads the header in either of its forms — delay in seconds, or an
// HTTP date — and returns zero for anything else.
func retryAfter(h string) time.Duration {
	h = strings.TrimSpace(h)
	if h == "" {
		return 0
	}
	if secs, err := strconv.Atoi(h); err == nil {
		if secs <= 0 {
			return 0
		}
		return time.Duration(secs) * time.Second
	}
	if t, err := http.ParseTime(h); err == nil {
		if d := time.Until(t); d > 0 {
			return d
		}
	}
	return 0
}

// ---- the User-Agent ----

var agyVersion struct {
	sync.Once
	v string
}

// userAgent names Antigravity CLI and its installed version, the way agy
// itself does.
func userAgent() string {
	agyVersion.Do(func() { agyVersion.v = installedAgyVersion() })
	return "antigravity/" + agyVersion.v + " " + runtime.GOOS + "/" + runtime.GOARCH
}

// installedAgyVersion asks agy for its version once. A GUI app inherits a
// minimal PATH, so ~/.local/bin (where agy's installer puts it) is tried
// explicitly. AGY_BIN overrides both.
func installedAgyVersion() string {
	candidates := []string{os.Getenv("AGY_BIN")}
	if home, err := os.UserHomeDir(); err == nil {
		candidates = append(candidates, filepath.Join(home, ".local", "bin", "agy"))
	}
	if p, err := exec.LookPath("agy"); err == nil {
		candidates = append(candidates, p)
	}
	for _, bin := range candidates {
		if bin == "" {
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		out, err := exec.CommandContext(ctx, bin, "--version").Output()
		cancel()
		if v := parseAgyVersion(string(out)); err == nil && v != "" {
			return v
		}
	}
	return fallbackAgyVersion
}

// parseAgyVersion takes the first word that looks like a version number.
func parseAgyVersion(out string) string {
	for _, f := range strings.Fields(out) {
		f = strings.TrimPrefix(f, "v")
		if len(f) > 0 && f[0] >= '0' && f[0] <= '9' && strings.Contains(f, ".") {
			return f
		}
	}
	return ""
}

// ---- quota parsing ----

type summaryBucket struct {
	BucketID          string   `json:"bucketId"`
	DisplayName       string   `json:"displayName"`
	Window            string   `json:"window"`
	ResetTime         string   `json:"resetTime"`
	RemainingFraction *float64 `json:"remainingFraction"`
}

type summaryResponse struct {
	Groups []struct {
		DisplayName string          `json:"displayName"`
		Buckets     []summaryBucket `json:"buckets"`
	} `json:"groups"`
}

func parseSummary(body []byte) (Usage, error) {
	return parseSummaryAt(body, time.Now())
}

func parseSummaryAt(body []byte, now time.Time) (Usage, error) {
	var r summaryResponse
	if err := json.Unmarshal(body, &r); err != nil {
		return Usage{}, fmt.Errorf("parsing quota JSON: %w", err)
	}

	var u Usage
	for _, g := range r.Groups {
		group := groupName(g.DisplayName)
		var meters []Meter
		for _, b := range g.Buckets {
			if b.RemainingFraction == nil {
				continue
			}
			meters = append(meters, toMeter(group, b, now))
		}
		// The API lists weekly first; the shorter window is the one that bites
		// first, so it leads.
		sort.SliceStable(meters, func(i, j int) bool {
			return windowRank(meters[i].Window) < windowRank(meters[j].Window)
		})
		u.Tiers = append(u.Tiers, meters...)
	}
	if len(u.Tiers) == 0 {
		return Usage{}, errors.New("quota response carried no usable buckets")
	}
	return u, nil
}

// groupName shortens "Gemini Models" to "Gemini" and "Claude and GPT models"
// to "Claude and GPT".
func groupName(display string) string {
	s := strings.TrimSpace(display)
	for _, suffix := range []string{" models", " Models"} {
		s = strings.TrimSuffix(s, suffix)
	}
	if s == "" {
		return "Models"
	}
	return s
}

// WindowLength is how long a window runs once it has started, or zero for an
// unknown window.
func WindowLength(w string) time.Duration {
	switch w {
	case "5h":
		return 5 * time.Hour
	case "weekly":
		return 7 * 24 * time.Hour
	default:
		return 0
	}
}

// idleSlack is how far an idle window's reset may sit from "now + length":
// Google stamps it when it answers, a moment before now.
const idleSlack = 2 * time.Minute

func windowRank(w string) int {
	switch w {
	case "5h":
		return 0
	case "weekly":
		return 1
	default:
		return 2
	}
}

// toMeter converts a bucket's *remaining* fraction into a *used* percentage,
// which is the direction every other meter in this app reads.
func toMeter(group string, b summaryBucket, now time.Time) Meter {
	window := b.Window
	if window == "" {
		window = b.BucketID
	}
	m := Meter{Name: group + " " + window, Group: group, Window: window, Bucket: b.BucketID}

	p := int(math.Round((1 - *b.RemainingFraction) * 100))
	switch {
	case p < 0:
		p = 0
	case p > 100:
		p = 100
	}
	m.Percent = p

	if b.ResetTime != "" {
		if t, err := time.Parse(time.RFC3339, b.ResetTime); err == nil {
			m.ResetsAt = t
			m.HasReset = true
		}
	}
	if l := WindowLength(window); m.Percent == 0 && m.HasReset && l > 0 {
		if d := m.ResetsAt.Sub(now.Add(l)); d > -idleSlack && d < idleSlack {
			m.Idle = true
		}
	}
	return m
}

func snippet(b []byte) string {
	s := strings.TrimSpace(string(b))
	if len(s) > 200 {
		return s[:200] + "…"
	}
	return s
}

// ---- credentials (read-only) ----

type credentials struct {
	accessToken string
	expiresAt   time.Time // zero if unknown
}

// credBlob is the shape agy keeps in the Keychain and the Antigravity app in
// its token file; we read only what we need, and deliberately not
// refresh_token.
type credBlob struct {
	Token struct {
		AccessToken string `json:"access_token"`
		Expiry      string `json:"expiry"` // RFC 3339
	} `json:"token"`
}

// TokenFilePath is the Antigravity app's token file, the fallback when the
// Keychain has nothing usable.
func TokenFilePath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".gemini", "jetski-standalone-oauth-token")
}

// loadCredentials reads agy's OAuth token from the Keychain and the
// Antigravity app's token file, and keeps whichever lasts longer: both apps
// share one Google login, and whichever ran last holds the fresh token.
func loadCredentials(ctx context.Context) (credentials, error) {
	var best credentials
	found := false
	consider := func(c credentials, ok bool) {
		if ok && (!found || c.expiresAt.After(best.expiresAt)) {
			best, found = c, true
		}
	}
	if raw, err := keychainRead(ctx); err == nil {
		consider(parseBlob(decodeKeyring(raw)))
	}
	if path := TokenFilePath(); path != "" {
		if data, err := os.ReadFile(path); err == nil {
			consider(parseBlob(data))
		}
	}
	if !found {
		return credentials{}, fmt.Errorf("%w (Keychain item %q / %q or %s); log in with `agy` first",
			ErrNoCredentials, keychainService, keychainAccount, TokenFilePath())
	}
	return best, nil
}

// keychainRead shells out to the system `security` tool. Its absolute path is
// used because a GUI/launchd-started app inherits a minimal PATH (which still
// includes /usr/bin, where `security` always lives).
func keychainRead(ctx context.Context) ([]byte, error) {
	return exec.CommandContext(ctx, "/usr/bin/security",
		"find-generic-password", "-s", keychainService, "-a", keychainAccount, "-w").Output()
}

// decodeKeyring undoes the Go keyring library's encodings: a
// "go-keyring-base64:" or "go-keyring-encoded:" (hex) prefix, or none.
func decodeKeyring(raw []byte) []byte {
	s := strings.TrimSpace(string(raw))
	if v, ok := strings.CutPrefix(s, "go-keyring-base64:"); ok {
		if b, err := base64.StdEncoding.DecodeString(v); err == nil {
			return b
		}
	}
	if v, ok := strings.CutPrefix(s, "go-keyring-encoded:"); ok {
		if b, err := hex.DecodeString(v); err == nil {
			return b
		}
	}
	return []byte(s)
}

func parseBlob(data []byte) (credentials, bool) {
	var b credBlob
	if err := json.Unmarshal(data, &b); err != nil || b.Token.AccessToken == "" {
		return credentials{}, false
	}
	c := credentials{accessToken: b.Token.AccessToken}
	if t, err := time.Parse(time.RFC3339Nano, b.Token.Expiry); err == nil {
		c.expiresAt = t
	}
	return c, true
}
