// Package usage fetches the per-model quota meters Gemini CLI shows in its
// /stats view, from Google's Code Assist API, using the Gemini CLI's stored
// OAuth credentials.
//
// It is READ-ONLY: it reads ~/.gemini/oauth_creds.json but never refreshes or
// writes it. The access token is short-lived and is refreshed only by running
// the `gemini` CLI; once it has expired, Fetch returns ErrTokenExpired until the
// next refresh.
//
// # Why not gemini.google.com/usage
//
// The consumer usage page at https://gemini.google.com/usage has no
// token-authenticated API. It is served by the Gemini web app and authenticated
// with whole-account Google session cookies (__Secure-1PSID / __Secure-1PSIDTS
// plus a SAPISIDHASH), which rotate every few minutes and would have to be
// lifted out of the browser's cookie jar and kept alive by this app. That is a
// whole-Google-account credential and it would mean refreshing someone else's
// session token in the background, so this app deliberately does not go there.
//
// What it reads instead is the quota the same account has on Gemini Code
// Assist — the meters behind `gemini` CLI usage. These are real, they come from
// a documented-shaped internal endpoint over a scoped OAuth token the user
// already owns, and they are NOT the same numbers as the consumer app's 5-hour
// and weekly meters. The README says so too; do not quietly relabel them.
//
// There is no officially documented API for this data — the endpoint is the same
// internal one the Gemini CLI calls (v1internal:retrieveUserQuota).
package usage

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	// endpoint and apiVersion mirror the Gemini CLI's CODE_ASSIST_ENDPOINT and
	// CODE_ASSIST_API_VERSION, including the environment overrides, so a build
	// can be pointed at a fixture or a staging host without a recompile.
	defaultEndpoint   = "https://cloudcode-pa.googleapis.com"
	defaultAPIVersion = "v1internal"

	methodLoadCodeAssist    = "loadCodeAssist"
	methodRetrieveUserQuota = "retrieveUserQuota"
)

// ErrNoCredentials means the Gemini CLI has never been logged in on this
// machine, so there is nothing to read.
var ErrNoCredentials = errors.New("no Gemini CLI credentials")

// ErrTokenExpired means the stored OAuth token is expired or was rejected.
// Callers should keep the last known meters and prompt the user to refresh (run
// any `gemini` command), rather than treat it as a hard error.
var ErrTokenExpired = errors.New("oauth token expired")

// ErrNotEligible means the account authenticated fine but has no Code Assist
// project behind it — a Google account that has never onboarded to Gemini Code
// Assist. Retrying will not help until the user runs `gemini` once.
var ErrNotEligible = errors.New("no Code Assist project for this account")

// ErrRateLimited means the endpoint refused the request because it was called
// too often. Unlike Anthropic's usage endpoint — which refuses roughly anything
// faster than one request per two minutes — this is genuinely rare, so it is
// treated as a transient fault: keep the last known meters and back off.
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

// Meter is one tier's quota window.
type Meter struct {
	Name     string    // display label: "Pro", "Flash", "Flash Lite"
	ModelID  string    // the bucket this came from, for the tooltip
	Percent  int       // 0–100 used, rounded
	ResetsAt time.Time // when the bucket resets; zero if unknown
	HasReset bool
}

// Usage is the set of tier meters the menu bar renders, worst tier first within
// the canonical Pro → Flash → Flash Lite ordering.
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

// Headline picks the two meters shown in the menu-bar title. Pro and Flash when
// both are present, otherwise simply the first two tiers, so an account with an
// unexpected model lineup still renders something truthful.
func (u Usage) Headline() []Meter {
	var out []Meter
	for _, want := range []string{"Pro", "Flash"} {
		if m, ok := u.Tier(want); ok {
			out = append(out, m)
		}
	}
	if len(out) == 2 {
		return out
	}
	out = nil
	for i, m := range u.Tiers {
		if i == 2 {
			break
		}
		out = append(out, m)
	}
	return out
}

var httpClient = &http.Client{Timeout: 20 * time.Second}

// projectCache holds the Code Assist project id between polls. Resolving it
// costs a second round trip and it does not change for the life of a login, so
// it is resolved once and reused; a 401 clears it along with everything else.
var projectCache struct {
	sync.Mutex
	id string
}

// Fetch reads the stored OAuth token and queries the quota endpoint.
func Fetch(ctx context.Context) (Usage, error) {
	cred, err := loadCredentials()
	if err != nil {
		return Usage{}, err
	}
	// Don't hammer the endpoint with a known-dead token: if it has already
	// expired locally, report it without a doomed request. A live 401 is still
	// handled in post(), in case expiry_date is missing or wrong.
	if !cred.expiresAt.IsZero() && time.Now().After(cred.expiresAt) {
		return Usage{}, ErrTokenExpired
	}

	project, err := resolveProject(ctx, cred.accessToken)
	if err != nil {
		return Usage{}, err
	}

	body, err := post(ctx, cred.accessToken, methodRetrieveUserQuota,
		map[string]any{"project": project})
	if err != nil {
		// A stale cached project id outlives its login; drop it so the next poll
		// resolves a fresh one rather than repeating a doomed request forever.
		if errors.Is(err, ErrTokenExpired) || errors.Is(err, ErrNotEligible) {
			forgetProject()
		}
		return Usage{}, err
	}
	return parseQuota(body)
}

// resolveProject returns the Code Assist project id for the signed-in account,
// preferring an explicit GOOGLE_CLOUD_PROJECT the way the Gemini CLI does, then
// the cached value, and finally loadCodeAssist.
func resolveProject(ctx context.Context, token string) (string, error) {
	if v := strings.TrimSpace(os.Getenv("GOOGLE_CLOUD_PROJECT")); v != "" {
		return v, nil
	}

	projectCache.Lock()
	cached := projectCache.id
	projectCache.Unlock()
	if cached != "" {
		return cached, nil
	}

	// The CLI sends its own client metadata here; the fields are required but
	// their values only affect Google's telemetry, so the neutral "unspecified"
	// constants are used rather than claiming to be an IDE we are not.
	body, err := post(ctx, token, methodLoadCodeAssist, map[string]any{
		"metadata": map[string]any{
			"ideType":    "IDE_UNSPECIFIED",
			"platform":   "PLATFORM_UNSPECIFIED",
			"pluginType": "GEMINI",
		},
	})
	if err != nil {
		return "", err
	}

	var r struct {
		CloudaicompanionProject string `json:"cloudaicompanionProject"`
	}
	if err := json.Unmarshal(body, &r); err != nil {
		return "", fmt.Errorf("parsing loadCodeAssist JSON: %w", err)
	}
	id := strings.TrimSpace(r.CloudaicompanionProject)
	if id == "" {
		return "", ErrNotEligible
	}

	projectCache.Lock()
	projectCache.id = id
	projectCache.Unlock()
	return id, nil
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
		// 403 is overloaded here: an expired token and an account with no Code
		// Assist entitlement both land on it, and the two need different advice
		// in the menu, so the body decides.
		if isIneligible(body) {
			return nil, ErrNotEligible
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
	endpoint := envOr("CODE_ASSIST_ENDPOINT", defaultEndpoint)
	version := envOr("CODE_ASSIST_API_VERSION", defaultAPIVersion)
	return strings.TrimSuffix(endpoint, "/") + "/" + version + ":" + method
}

func envOr(key, def string) string {
	if v := strings.TrimSpace(os.Getenv(key)); v != "" {
		return v
	}
	return def
}

// isIneligible spots the "the API is not enabled / no project" family of 403s,
// which mean the account cannot use Code Assist at all rather than that the
// token went stale.
func isIneligible(body []byte) bool {
	var r struct {
		Error struct {
			Status  string `json:"status"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(body, &r); err != nil {
		return false
	}
	if r.Error.Status == "PERMISSION_DENIED" {
		msg := strings.ToLower(r.Error.Message)
		for _, hint := range []string{"has not been used", "is disabled", "cloud code", "not eligible"} {
			if strings.Contains(msg, hint) {
				return true
			}
		}
	}
	return false
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

// ---- quota parsing ----

type bucket struct {
	ModelID           string   `json:"modelId"`
	RemainingFraction *float64 `json:"remainingFraction"`
	RemainingAmount   string   `json:"remainingAmount"` // stringified int, often absent
	ResetTime         string   `json:"resetTime"`
}

type quotaResponse struct {
	Buckets []bucket `json:"buckets"`
	Error   *struct {
		Status  string `json:"status"`
		Message string `json:"message"`
	} `json:"error"`
}

func parseQuota(body []byte) (Usage, error) {
	var r quotaResponse
	if err := json.Unmarshal(body, &r); err != nil {
		return Usage{}, fmt.Errorf("parsing quota JSON: %w", err)
	}
	if r.Error != nil && r.Error.Message != "" {
		return Usage{}, errors.New(r.Error.Message)
	}

	// Group by tier and keep the worst bucket in each, which is what the Gemini
	// CLI's own quota display does: several model ids can share a tier's pool and
	// the binding constraint is whichever has least left.
	worst := map[string]Meter{}
	for _, b := range r.Buckets {
		if b.ModelID == "" || b.RemainingFraction == nil {
			continue
		}
		name := tierName(b.ModelID)
		m := toMeter(name, b)
		if cur, ok := worst[name]; !ok || m.Percent > cur.Percent {
			worst[name] = m
		}
	}
	if len(worst) == 0 {
		return Usage{}, errors.New("quota response carried no usable buckets")
	}

	u := Usage{Tiers: make([]Meter, 0, len(worst))}
	for _, m := range worst {
		u.Tiers = append(u.Tiers, m)
	}
	sort.SliceStable(u.Tiers, func(i, j int) bool {
		ri, rj := tierRank(u.Tiers[i].Name), tierRank(u.Tiers[j].Name)
		if ri != rj {
			return ri < rj
		}
		return u.Tiers[i].Name < u.Tiers[j].Name
	})
	return u, nil
}

// tierName maps a model id onto the tier label the Gemini CLI shows. The CLI
// looks the tier up in its model-definition service, which is not reachable from
// here, so the id is matched instead — "flash-lite" before "flash", since the
// former contains the latter.
func tierName(modelID string) string {
	id := strings.ToLower(modelID)
	switch {
	case strings.Contains(id, "flash-lite"), strings.Contains(id, "flash_lite"):
		return "Flash Lite"
	case strings.Contains(id, "flash"):
		return "Flash"
	case strings.Contains(id, "pro"):
		return "Pro"
	default:
		// An unrecognised model still deserves a row rather than being dropped:
		// Google adds model ids faster than this mapping can be updated.
		return modelID
	}
}

func tierRank(name string) int {
	switch name {
	case "Pro":
		return 0
	case "Flash":
		return 1
	case "Flash Lite":
		return 2
	default:
		return 3
	}
}

// toMeter converts a bucket's *remaining* fraction into a *used* percentage,
// which is the direction every other meter in this app reads.
func toMeter(name string, b bucket) Meter {
	m := Meter{Name: name, ModelID: b.ModelID}

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

// credBlob is the on-disk shape of ~/.gemini/oauth_creds.json; we read only what
// we need, and deliberately not refresh_token.
type credBlob struct {
	AccessToken string `json:"access_token"`
	ExpiryDate  int64  `json:"expiry_date"` // epoch milliseconds
}

// CredentialsPath is where the Gemini CLI keeps its OAuth credentials.
func CredentialsPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".gemini", "oauth_creds.json")
}

// loadCredentials reads the Gemini CLI's OAuth credentials from disk. Unlike
// Claude Code there is no Keychain copy to prefer — the CLI writes this file and
// nothing else.
func loadCredentials() (credentials, error) {
	path := CredentialsPath()
	if path == "" {
		return credentials{}, ErrNoCredentials
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return credentials{}, fmt.Errorf("%w: %s not readable; log in with `gemini` first",
			ErrNoCredentials, path)
	}
	c, ok := parseBlob(data)
	if !ok {
		return credentials{}, fmt.Errorf("%w: %s has no access_token", ErrNoCredentials, path)
	}
	return c, nil
}

func parseBlob(data []byte) (credentials, bool) {
	var b credBlob
	if err := json.Unmarshal(data, &b); err != nil {
		return credentials{}, false
	}
	if b.AccessToken == "" {
		return credentials{}, false
	}
	c := credentials{accessToken: b.AccessToken}
	if b.ExpiryDate > 0 {
		c.expiresAt = time.UnixMilli(b.ExpiryDate)
	}
	return c, true
}
