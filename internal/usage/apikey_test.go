package usage

import (
	"context"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

const testKey = "AIzaTEST-not-a-real-key"

func TestIsKeyInvalid(t *testing.T) {
	invalid := `{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT",
	  "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com"}]}}`
	if !isKeyInvalid([]byte(invalid)) {
		t.Error("API_KEY_INVALID must read as an invalid key")
	}
	if !isKeyInvalid([]byte(`{"error":{"message":"API key not valid. Please pass a valid API key."}}`)) {
		t.Error("the message alone must also read as an invalid key")
	}
	disabled := `{"error":{"code":403,"message":"Generative Language API has not been used in project 1 before or it is disabled.","status":"PERMISSION_DENIED"}}`
	if isKeyInvalid([]byte(disabled)) {
		t.Error("a disabled API is not an invalid key")
	}
}

func TestBetweenMarks(t *testing.T) {
	out := "welcome from .zshrc\n" + shellMark + " " + testKey + "\n" + shellMark + "trailing"
	if got := betweenMarks(out); got != testKey {
		t.Errorf("betweenMarks = %q, want %q", got, testKey)
	}
	if got := betweenMarks("no marks at all"); got != "" {
		t.Errorf("betweenMarks without marks = %q, want empty", got)
	}
	if got := betweenMarks(shellMark + shellMark); got != "" {
		t.Errorf("betweenMarks of an unset var = %q, want empty", got)
	}
}

func TestParseLookupKey(t *testing.T) {
	got, err := parseLookupKey([]byte(`{"parent":"projects/123456789/locations/global","name":"projects/123456789/locations/global/keys/abc"}`))
	if err != nil || got != "123456789" {
		t.Errorf("parseLookupKey = %q, %v; want 123456789", got, err)
	}
	if _, err := parseLookupKey([]byte(`{}`)); err == nil {
		t.Error("an empty lookupKey answer must be an error")
	}
}

func TestParseADC(t *testing.T) {
	if _, err := parseADC([]byte(`{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r"}`)); err != nil {
		t.Errorf("valid authorized_user: %v", err)
	}
	for name, in := range map[string]string{
		"service account": `{"type":"service_account","client_email":"x"}`,
		"no token":        `{"type":"authorized_user","client_id":"c"}`,
		"not json":        `nope`,
	} {
		if _, err := parseADC([]byte(in)); !errors.Is(err, ErrNoADC) {
			t.Errorf("%s: err = %v, want ErrNoADC", name, err)
		}
	}
}

func TestCheckKeyOnceReportsInvalidKey(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("x-goog-api-key") != testKey {
			t.Errorf("key not sent in the x-goog-api-key header")
		}
		if strings.Contains(r.URL.RawQuery, testKey) {
			t.Errorf("key must not be in the URL")
		}
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, `{"error":{"message":"API key not valid.","details":[{"reason":"API_KEY_INVALID"}]}}`)
	}))
	defer srv.Close()
	withVar(t, &geminiBaseURL, srv.URL)
	ForgetAPIKey()
	keyState.key = testKey

	err := checkKeyOnce(context.Background(), testKey)
	if !errors.Is(err, ErrKeyInvalid) {
		t.Fatalf("err = %v, want ErrKeyInvalid", err)
	}
	if keyState.key != "" {
		t.Error("a rejected key must be dropped, so a fixed key is read again")
	}
}

func TestADCAccessTokenExchangesAndNeverWrites(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "adc.json")
	content := `{"type":"authorized_user","client_id":"cid","client_secret":"cs","refresh_token":"rt","quota_project_id":"qp"}`
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GOOGLE_APPLICATION_CREDENTIALS", path)

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		r.ParseForm()
		if r.Form.Get("grant_type") != "refresh_token" || r.Form.Get("refresh_token") != "rt" {
			t.Errorf("unexpected token form: %v", r.Form)
		}
		io.WriteString(w, `{"access_token":"at","expires_in":3599}`)
	}))
	defer srv.Close()
	withVar(t, &tokenURL, srv.URL)
	forgetADC()

	tok, err := adcAccessToken(context.Background())
	if err != nil {
		t.Fatalf("adcAccessToken: %v", err)
	}
	if tok.access != "at" || tok.quotaProject != "qp" {
		t.Errorf("token = %+v", tok)
	}
	after, _ := os.ReadFile(path)
	if string(after) != content {
		t.Error("the ADC file must never be written")
	}
}

func TestADCAccessTokenMissingFile(t *testing.T) {
	t.Setenv("GOOGLE_APPLICATION_CREDENTIALS", filepath.Join(t.TempDir(), "missing.json"))
	forgetADC()
	if _, err := adcAccessToken(context.Background()); !errors.Is(err, ErrNoADC) {
		t.Errorf("err = %v, want ErrNoADC", err)
	}
}

func TestKeyNeverInTransportError(t *testing.T) {
	// Nothing listens on this port, so the request fails in the transport,
	// whose error normally quotes the whole URL - query string and key too.
	withVar(t, &apiKeysBaseURL, "http://127.0.0.1:1")
	t.Setenv("GOOGLE_CLOUD_PROJECT", "")
	ForgetAPIKey()

	_, err := keyProject(context.Background(), testKey, adcToken{access: "at"})
	if err == nil {
		t.Fatal("expected a transport error")
	}
	if strings.Contains(err.Error(), testKey) {
		t.Errorf("error text leaks the API key: %v", err)
	}
}

func withVar(t *testing.T, v *string, val string) {
	t.Helper()
	old := *v
	*v = val
	t.Cleanup(func() { *v = old })
}

func TestSumTimeSeries(t *testing.T) {
	// The shape of a live answer: one series per method, each with points.
	body := `{"timeSeries":[
	  {"metric":{"labels":{"method":"google.ai.generativelanguage.v1beta.GenerativeService.GenerateContent","quota_metric":"generativelanguage.googleapis.com/generate_content_requests"}},
	   "points":[{"value":{"int64Value":"1"}},{"value":{"int64Value":"1"}}]},
	  {"metric":{"labels":{"method":"google.ai.generativelanguage.v1beta.GenerativeService.StreamGenerateContent","quota_metric":"generativelanguage.googleapis.com/generate_content_requests"}},
	   "points":[{"value":{"int64Value":"3"}},{"value":{"int64Value":"1"}}]}
	],"unit":"1"}`
	if got, err := sumTimeSeries([]byte(body)); err != nil || got != 6 {
		t.Errorf("sumTimeSeries = %d, %v; want 6", got, err)
	}
	// No requests yet today: Monitoring leaves timeSeries out.
	if got, err := sumTimeSeries([]byte(`{"unit":"1"}`)); err != nil || got != 0 {
		t.Errorf("sumTimeSeries of an empty answer = %d, %v; want 0", got, err)
	}
}

func TestMissingKeyIsRemembered(t *testing.T) {
	ForgetAPIKey()
	keyState.missingUntil = time.Now().Add(time.Hour)
	t.Setenv("GEMINI_API_KEY", testKey) // would be found, if the app looked
	if _, err := apiKey(context.Background()); !errors.Is(err, ErrNoAPIKey) {
		t.Errorf("err = %v; a recent miss must not start a new lookup", err)
	}
	ForgetAPIKey()
	if key, err := apiKey(context.Background()); err != nil || key != testKey {
		t.Errorf("after ForgetAPIKey: %q, %v; want the key", key, err)
	}
	ForgetAPIKey()
}

func TestRejectedEnvKeyFallsBackToLoginShell(t *testing.T) {
	ForgetAPIKey()
	t.Cleanup(func() {
		ForgetAPIKey()
		keyState.envRejected = false
	})
	t.Setenv("GEMINI_API_KEY", testKey)
	if _, err := apiKey(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !rejectEnvKey() {
		t.Fatal("a rejected environment key must allow a second try")
	}
	if rejectEnvKey() {
		t.Error("only one second try: the environment key is already skipped")
	}
	ForgetAPIKey()
	t.Setenv("SHELL", "/usr/bin/false") // the login shell has no key
	if _, err := apiKey(context.Background()); !errors.Is(err, ErrNoAPIKey) {
		t.Errorf("err = %v; the rejected environment key must be skipped", err)
	}
}

func TestQuotaDayStartIsPacificMidnight(t *testing.T) {
	now := time.Date(2026, 10, 15, 14, 0, 0, 0, time.UTC) // 07:00 PDT
	got := quotaDayStart(now)
	if want := time.Date(2026, 10, 15, 7, 0, 0, 0, time.UTC); !got.Equal(want) {
		t.Errorf("quotaDayStart = %s, want %s", got.UTC(), want)
	}
}
