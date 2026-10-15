package usage

// The Gemini API key: a second source, next to the Antigravity quota. When a
// GEMINI_API_KEY is found, FetchAPIKey checks it and reads its project's
// request count for the day from Cloud Monitoring. Monitoring needs an OAuth
// token, so this also reads the user's gcloud Application Default Credentials
// (ADC).
//
// It stays READ-ONLY: the key is held in memory and never logged, and the ADC
// refresh token is exchanged for an access token in memory only. Nothing is
// ever written back. Unlike agy's Keychain token, nothing else rotates
// the ADC refresh token, so an in-memory exchange cannot race an owner.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// ErrNoAPIKey means GEMINI_API_KEY is in neither the app's environment nor
// the login shell.
var ErrNoAPIKey = errors.New("GEMINI_API_KEY not found")

// ErrKeyInvalid means Google rejected the API key (API_KEY_INVALID).
var ErrKeyInvalid = errors.New("gemini API key not valid")

// ErrNoADC means there are no usable gcloud Application Default Credentials,
// which the Cloud Monitoring quota query needs.
var ErrNoADC = errors.New("no gcloud application-default credentials")

// Endpoints, as variables so tests can point them at a local server.
var (
	geminiBaseURL     = envOr("GOOGLE_GEMINI_BASE_URL", "https://generativelanguage.googleapis.com")
	tokenURL          = "https://oauth2.googleapis.com/token"
	apiKeysBaseURL    = "https://apikeys.googleapis.com"
	monitoringBaseURL = "https://monitoring.googleapis.com"
)

const (
	// shellTimeout bounds the login-shell lookup of GEMINI_API_KEY.
	shellTimeout = 5 * time.Second
	// missingKeyRecheck is how long "no key" is believed before the login
	// shell is asked again, so a machine with no key does not start a shell
	// on every poll.
	missingKeyRecheck = 10 * time.Minute
)

// KeyUsage is what is known about the Gemini API key's use today.
type KeyUsage struct {
	Project  string    // the Cloud project that owns the key
	Requests int64     // generateContent requests since Since
	Since    time.Time // the start of the quota day, midnight Pacific
}

// keyState holds the API key and what is known about it between polls. A
// rejected key is dropped, so the next poll reads it again and picks up a fix
// in ~/.zshrc without a restart.
var keyState struct {
	sync.Mutex
	key          string
	checked      bool      // Google accepted key at least once
	project      string    // the key's project, once resolved
	missingUntil time.Time // no key was found; do not look again before this
	lastFromEnv  bool      // the last key read came from the process environment
	envRejected  bool      // Google rejected the environment's key; skip it
}

// ForgetAPIKey drops everything known about the key, so the next
// FetchAPIKey reads it again. "Refresh now" calls it.
func ForgetAPIKey() {
	keyState.Lock()
	keyState.key, keyState.checked, keyState.project = "", false, ""
	keyState.missingUntil = time.Time{}
	keyState.Unlock()
}

// FetchAPIKey checks the Gemini API key and reads today's request count for
// its project. It returns ErrNoAPIKey when there is no key, which is normal:
// the caller then hides the key row.
func FetchAPIKey(ctx context.Context) (KeyUsage, error) {
	key, err := apiKey(ctx)
	if err != nil {
		return KeyUsage{}, err
	}
	if err := checkKeyOnce(ctx, key); err != nil {
		// An app started from a terminal inherits that terminal's
		// environment, which can hold an old key long after ~/.zshrc got a
		// new one. Before giving up, try the login shell's key.
		if !errors.Is(err, ErrKeyInvalid) || !rejectEnvKey() {
			return KeyUsage{}, err
		}
		if key, err = apiKey(ctx); err != nil {
			return KeyUsage{}, err
		}
		if err := checkKeyOnce(ctx, key); err != nil {
			return KeyUsage{}, err
		}
	}
	tok, err := adcAccessToken(ctx)
	if err != nil {
		return KeyUsage{}, err
	}
	project, err := keyProject(ctx, key, tok)
	if err != nil {
		return KeyUsage{}, err
	}
	now := time.Now()
	since := quotaDayStart(now)
	n, err := requestsToday(ctx, tok, project, since, now)
	if err != nil {
		return KeyUsage{}, err
	}
	return KeyUsage{Project: project, Requests: n, Since: since}, nil
}

// ---- the key ----

// rejectEnvKey marks the environment's key as rejected, so apiKey asks the
// login shell from now on. It reports whether the rejected key came from the
// environment, which is when a second try can help.
func rejectEnvKey() bool {
	keyState.Lock()
	defer keyState.Unlock()
	if !keyState.lastFromEnv || keyState.envRejected {
		return false
	}
	keyState.envRejected = true
	return true
}

// apiKey returns the Gemini API key: from the environment first, the way the
// Gemini CLI reads it, then from the login shell. A bundle started from Finder
// or at login does not inherit ~/.zshrc, which is where the key usually is.
func apiKey(ctx context.Context) (string, error) {
	keyState.Lock()
	key, missingUntil := keyState.key, keyState.missingUntil
	keyState.Unlock()
	if key != "" {
		return key, nil
	}
	if time.Now().Before(missingUntil) {
		return "", ErrNoAPIKey
	}

	keyState.Lock()
	skipEnv := keyState.envRejected
	keyState.Unlock()

	fromEnv := false
	if !skipEnv {
		for _, name := range []string{"GEMINI_API_KEY", "GOOGLE_API_KEY"} {
			if v := strings.TrimSpace(os.Getenv(name)); v != "" {
				key, fromEnv = v, true
				break
			}
		}
	}
	if key == "" {
		key = loginShellVar(ctx, "GEMINI_API_KEY")
	}
	if key == "" {
		keyState.Lock()
		keyState.missingUntil = time.Now().Add(missingKeyRecheck)
		keyState.Unlock()
		return "", ErrNoAPIKey
	}

	keyState.Lock()
	keyState.key, keyState.lastFromEnv = key, fromEnv
	keyState.Unlock()
	return key, nil
}

// shellMark fences the value in the login shell's output, so text that
// ~/.zshrc prints on its own does not get mixed into the key.
const shellMark = "\x1fgemini-usage\x1f"

// loginShellVar reads one variable from an interactive login shell, or
// returns "" when it is not set or the shell does not answer in time.
func loginShellVar(ctx context.Context, name string) string {
	shell := os.Getenv("SHELL")
	if shell == "" {
		shell = "/bin/zsh"
	}
	ctx, cancel := context.WithTimeout(ctx, shellTimeout)
	defer cancel()

	// The name goes in through the environment, not the script text.
	script := `eval "printf '%s%s%s' \"$GU_MARK\" \"\${$GU_NAME}\" \"$GU_MARK\""`
	cmd := exec.CommandContext(ctx, shell, "-lic", script)
	cmd.Env = append(os.Environ(), "GU_MARK="+shellMark, "GU_NAME="+name)
	out, _ := cmd.Output() // stdin is /dev/null; stderr is dropped
	return betweenMarks(string(out))
}

func betweenMarks(out string) string {
	_, rest, ok := strings.Cut(out, shellMark)
	if !ok {
		return ""
	}
	v, _, ok := strings.Cut(rest, shellMark)
	if !ok {
		return ""
	}
	return strings.TrimSpace(v)
}

// checkKeyOnce asks Google whether it accepts the key, with the cheapest call
// that needs one, until it has said yes once.
func checkKeyOnce(ctx context.Context, key string) error {
	keyState.Lock()
	checked := keyState.checked
	keyState.Unlock()
	if checked {
		return nil
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet,
		strings.TrimSuffix(geminiBaseURL, "/")+"/v1beta/models?pageSize=1", nil)
	if err != nil {
		return err
	}
	req.Header.Set("x-goog-api-key", key) // a header keeps the key out of URLs and errors
	resp, err := httpClient.Do(req)
	if err != nil {
		return fmt.Errorf("checking the API key: %w", stripURL(err))
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	switch {
	case resp.StatusCode == http.StatusOK:
		keyState.Lock()
		keyState.checked = true
		keyState.Unlock()
		return nil
	case isKeyInvalid(body):
		ForgetAPIKey()
		return ErrKeyInvalid
	case resp.StatusCode == http.StatusTooManyRequests:
		return RateLimited{RetryAfter: retryAfter(resp.Header.Get("Retry-After"))}
	default:
		return fmt.Errorf("checking the API key: HTTP %d: %s", resp.StatusCode, snippet(body))
	}
}

// isKeyInvalid spots Google's "API key not valid" error, by its reason code or,
// failing that, its message.
func isKeyInvalid(body []byte) bool {
	var r struct {
		Error struct {
			Message string `json:"message"`
			Details []struct {
				Reason string `json:"reason"`
			} `json:"details"`
		} `json:"error"`
	}
	if err := json.Unmarshal(body, &r); err != nil {
		return false
	}
	for _, d := range r.Error.Details {
		if d.Reason == "API_KEY_INVALID" {
			return true
		}
	}
	return strings.Contains(strings.ToLower(r.Error.Message), "api key not valid")
}

// keyProject returns the Cloud project that owns the key:
// GOOGLE_CLOUD_PROJECT when set, else the API Keys API's lookupKey.
func keyProject(ctx context.Context, key string, tok adcToken) (string, error) {
	if v := strings.TrimSpace(os.Getenv("GOOGLE_CLOUD_PROJECT")); v != "" {
		return v, nil
	}
	keyState.Lock()
	cached := keyState.project
	keyState.Unlock()
	if cached != "" {
		return cached, nil
	}

	body, err := googleGet(ctx, tok, apiKeysBaseURL+"/v2/keys:lookupKey?keyString="+url.QueryEscape(key))
	if err != nil {
		return "", fmt.Errorf("finding the key's project (set GOOGLE_CLOUD_PROJECT to skip): %w", err)
	}
	project, err := parseLookupKey(body)
	if err != nil {
		return "", err
	}
	keyState.Lock()
	keyState.project = project
	keyState.Unlock()
	return project, nil
}

// parseLookupKey takes the project from lookupKey's
// {"parent": "projects/<number>/locations/global", ...}.
func parseLookupKey(body []byte) (string, error) {
	var r struct {
		Parent string `json:"parent"`
	}
	if err := json.Unmarshal(body, &r); err != nil {
		return "", fmt.Errorf("parsing lookupKey JSON: %w", err)
	}
	parts := strings.Split(r.Parent, "/")
	if len(parts) < 2 || parts[0] != "projects" || parts[1] == "" {
		return "", fmt.Errorf("lookupKey gave no project: %q", r.Parent)
	}
	return parts[1], nil
}

// ---- gcloud Application Default Credentials (read-only) ----

// adcToken is an access token from ADC, plus the quota project that Google
// bills the API calls to.
type adcToken struct {
	access       string
	quotaProject string
	expiresAt    time.Time
}

var adcCache struct {
	sync.Mutex
	tok adcToken
}

func forgetADC() {
	adcCache.Lock()
	adcCache.tok = adcToken{}
	adcCache.Unlock()
}

// adcFile is the shape gcloud writes for `auth application-default login`.
type adcFile struct {
	Type           string `json:"type"`
	ClientID       string `json:"client_id"`
	ClientSecret   string `json:"client_secret"`
	RefreshToken   string `json:"refresh_token"`
	QuotaProjectID string `json:"quota_project_id"`
}

// ADCPath is where the ADC file is: GOOGLE_APPLICATION_CREDENTIALS, else
// gcloud's default location.
func ADCPath() string {
	if v := strings.TrimSpace(os.Getenv("GOOGLE_APPLICATION_CREDENTIALS")); v != "" {
		return v
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".config", "gcloud", "application_default_credentials.json")
}

func parseADC(data []byte) (adcFile, error) {
	var f adcFile
	if err := json.Unmarshal(data, &f); err != nil {
		return adcFile{}, fmt.Errorf("%w: file is not JSON", ErrNoADC)
	}
	if f.Type != "authorized_user" {
		return adcFile{}, fmt.Errorf("%w: type %q is not supported, only authorized_user", ErrNoADC, f.Type)
	}
	if f.ClientID == "" || f.RefreshToken == "" {
		return adcFile{}, fmt.Errorf("%w: file has no refresh token", ErrNoADC)
	}
	return f, nil
}

// adcAccessToken returns a cached access token, or exchanges the ADC refresh
// token for a new one. The file is read, never written.
func adcAccessToken(ctx context.Context) (adcToken, error) {
	adcCache.Lock()
	tok := adcCache.tok
	adcCache.Unlock()
	if tok.access != "" && time.Until(tok.expiresAt) > time.Minute {
		return tok, nil
	}

	path := ADCPath()
	data, err := os.ReadFile(path)
	if err != nil {
		return adcToken{}, fmt.Errorf("%w: %s not readable", ErrNoADC, path)
	}
	f, err := parseADC(data)
	if err != nil {
		return adcToken{}, err
	}

	form := url.Values{
		"grant_type":    {"refresh_token"},
		"client_id":     {f.ClientID},
		"client_secret": {f.ClientSecret},
		"refresh_token": {f.RefreshToken},
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, tokenURL, strings.NewReader(form.Encode()))
	if err != nil {
		return adcToken{}, err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	resp, err := httpClient.Do(req)
	if err != nil {
		return adcToken{}, fmt.Errorf("ADC token request: %w", stripURL(err))
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	switch {
	case resp.StatusCode == http.StatusBadRequest || resp.StatusCode == http.StatusUnauthorized:
		// invalid_grant: the login was revoked or expired.
		return adcToken{}, fmt.Errorf("%w: Google rejected the refresh token", ErrNoADC)
	case resp.StatusCode != http.StatusOK:
		return adcToken{}, fmt.Errorf("ADC token HTTP %d: %s", resp.StatusCode, snippet(body))
	}

	var r struct {
		AccessToken string `json:"access_token"`
		ExpiresIn   int    `json:"expires_in"`
	}
	if err := json.Unmarshal(body, &r); err != nil || r.AccessToken == "" {
		return adcToken{}, fmt.Errorf("ADC token response has no access_token")
	}
	// GOOGLE_CLOUD_QUOTA_PROJECT wins over the file, as in Google's own client
	// libraries, so a quota project can be set without editing the ADC file.
	tok = adcToken{
		access:       r.AccessToken,
		quotaProject: envOr("GOOGLE_CLOUD_QUOTA_PROJECT", f.QuotaProjectID),
		expiresAt:    time.Now().Add(time.Duration(r.ExpiresIn) * time.Second),
	}
	adcCache.Lock()
	adcCache.tok = tok
	adcCache.Unlock()
	return tok, nil
}

// googleGet calls a Google Cloud API with the ADC token.
func googleGet(ctx context.Context, tok adcToken, rawURL string) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+tok.access)
	if tok.quotaProject != "" {
		// User credentials have no project of their own; this names the one
		// Google charges the API call to.
		req.Header.Set("x-goog-user-project", tok.quotaProject)
	}
	resp, err := httpClient.Do(req)
	if err != nil {
		return nil, stripURL(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	switch {
	case resp.StatusCode == http.StatusUnauthorized:
		forgetADC()
		return nil, fmt.Errorf("%w: Google rejected the access token", ErrNoADC)
	case resp.StatusCode == http.StatusTooManyRequests:
		return nil, RateLimited{RetryAfter: retryAfter(resp.Header.Get("Retry-After"))}
	case resp.StatusCode != http.StatusOK:
		// 403 lands here: a disabled API or a missing role. Google's message
		// names which, so pass it on.
		return nil, fmt.Errorf("HTTP %d: %s", resp.StatusCode, snippet(body))
	}
	return body, nil
}

// stripURL drops the request URL from a transport error. lookupKey has the
// API key in its query string, and that must never reach the menu or a log.
func stripURL(err error) error {
	var ue *url.Error
	if errors.As(err, &ue) {
		return ue.Err
	}
	return err
}
