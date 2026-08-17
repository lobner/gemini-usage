package usage

import (
	"errors"
	"net/http"
	"strings"
	"testing"
	"time"
)

// A trimmed retrieveUserQuota response. Two pro buckets are present on purpose,
// to prove the worst one wins, and an unknown model id to prove it still gets a
// row rather than being dropped.
const quotaSample = `{
  "buckets": [
    {"modelId": "gemini-3-pro-preview",   "remainingFraction": 0.93, "remainingAmount": "93", "resetTime": "2026-08-17T16:10:00Z"},
    {"modelId": "gemini-3-pro",           "remainingFraction": 0.20, "remainingAmount": "20", "resetTime": "2026-08-17T16:10:00Z"},
    {"modelId": "gemini-3-flash",         "remainingFraction": 0.955},
    {"modelId": "gemini-2.5-flash-lite",  "remainingFraction": 1.0},
    {"modelId": "gemini-experimental-xyz","remainingFraction": 0.5}
  ]
}`

func TestParseQuotaGroupsByTierAndKeepsWorst(t *testing.T) {
	u, err := parseQuota([]byte(quotaSample))
	if err != nil {
		t.Fatalf("parseQuota: %v", err)
	}

	pro, ok := u.Tier("Pro")
	if !ok {
		t.Fatal("no Pro tier parsed")
	}
	// 1 - 0.20 = 80% used; the 0.93 bucket must not win.
	if pro.Percent != 80 {
		t.Errorf("pro = %d%%, want 80 (worst of the two pro buckets)", pro.Percent)
	}
	if pro.ModelID != "gemini-3-pro" {
		t.Errorf("pro model = %q, want gemini-3-pro", pro.ModelID)
	}
	if !pro.HasReset {
		t.Error("expected pro reset time to parse")
	}

	flash, ok := u.Tier("Flash")
	if !ok {
		t.Fatal("no Flash tier parsed")
	}
	if flash.Percent != 5 { // 1 - 0.955 = 0.045 -> 5% after rounding
		t.Errorf("flash = %d%%, want 5", flash.Percent)
	}
	if flash.HasReset {
		t.Error("flash bucket has no resetTime; HasReset should be false")
	}

	if lite, ok := u.Tier("Flash Lite"); !ok || lite.Percent != 0 {
		t.Errorf("flash lite = %v/%d%%, want present at 0%%", ok, lite.Percent)
	}
	if _, ok := u.Tier("gemini-experimental-xyz"); !ok {
		t.Error("unknown model id should still get its own row")
	}
}

func TestParseQuotaOrdersProFlashLiteThenRest(t *testing.T) {
	u, err := parseQuota([]byte(quotaSample))
	if err != nil {
		t.Fatalf("parseQuota: %v", err)
	}
	want := []string{"Pro", "Flash", "Flash Lite", "gemini-experimental-xyz"}
	if len(u.Tiers) != len(want) {
		t.Fatalf("got %d tiers, want %d", len(u.Tiers), len(want))
	}
	for i, n := range want {
		if u.Tiers[i].Name != n {
			t.Errorf("tier %d = %q, want %q", i, u.Tiers[i].Name, n)
		}
	}
}

func TestHeadlinePrefersProAndFlash(t *testing.T) {
	u, err := parseQuota([]byte(quotaSample))
	if err != nil {
		t.Fatalf("parseQuota: %v", err)
	}
	h := u.Headline()
	if len(h) != 2 || h[0].Name != "Pro" || h[1].Name != "Flash" {
		t.Fatalf("headline = %+v, want Pro then Flash", h)
	}
}

func TestHeadlineFallsBackWhenTiersAreUnfamiliar(t *testing.T) {
	u, err := parseQuota([]byte(`{"buckets":[
	  {"modelId":"aardvark","remainingFraction":0.5},
	  {"modelId":"bison","remainingFraction":0.25},
	  {"modelId":"civet","remainingFraction":0.1}]}`))
	if err != nil {
		t.Fatalf("parseQuota: %v", err)
	}
	h := u.Headline()
	if len(h) != 2 {
		t.Fatalf("headline = %d meters, want 2", len(h))
	}
}

func TestTierName(t *testing.T) {
	cases := map[string]string{
		"gemini-3-pro":              "Pro",
		"gemini-3-PRO-preview":      "Pro",
		"gemini-3-flash":            "Flash",
		"gemini-2.5-flash-lite":     "Flash Lite",
		"gemini-2.5-flash_lite-001": "Flash Lite",
		"something-else":            "something-else",
	}
	for id, want := range cases {
		if got := tierName(id); got != want {
			t.Errorf("tierName(%q) = %q, want %q", id, got, want)
		}
	}
}

func TestToMeterInvertsRoundsAndClamps(t *testing.T) {
	f := func(v float64) *float64 { return &v }
	cases := []struct {
		remaining *float64
		want      int
	}{
		{f(1.0), 0},   // nothing used
		{f(0.0), 100}, // all used
		{f(0.5), 50},
		{f(0.004), 100}, // 99.6 rounds up
		{f(1.5), 0},     // nonsense over 1 clamps at 0 used
		{f(-0.5), 100},  // nonsense under 0 clamps at 100 used
	}
	for _, c := range cases {
		got := toMeter("Pro", bucket{ModelID: "m", RemainingFraction: c.remaining}).Percent
		if got != c.want {
			t.Errorf("remainingFraction %v -> %d%%, want %d%%", *c.remaining, got, c.want)
		}
	}
}

func TestParseQuotaRejectsEmptyAndErrorPayloads(t *testing.T) {
	if _, err := parseQuota([]byte(`{"buckets":[]}`)); err == nil {
		t.Error("expected an error for a response with no buckets")
	}
	// Buckets present but all unusable is the same situation.
	if _, err := parseQuota([]byte(`{"buckets":[{"modelId":"x"}]}`)); err == nil {
		t.Error("expected an error when no bucket carries remainingFraction")
	}
	_, err := parseQuota([]byte(`{"error":{"status":"PERMISSION_DENIED","message":"nope"}}`))
	if err == nil || !strings.Contains(err.Error(), "nope") {
		t.Errorf("expected the server message to surface, got %v", err)
	}
}

func TestParseBlobReadsGeminiCredentialShape(t *testing.T) {
	c, ok := parseBlob([]byte(`{"access_token":"tok","expiry_date":1779282948419,"refresh_token":"secret"}`))
	if !ok {
		t.Fatal("expected the credential blob to parse")
	}
	if c.accessToken != "tok" {
		t.Errorf("accessToken = %q", c.accessToken)
	}
	if c.expiresAt.IsZero() {
		t.Error("expected expiry_date to become a time")
	}

	if _, ok := parseBlob([]byte(`{"refresh_token":"only"}`)); ok {
		t.Error("a blob with no access_token must not parse")
	}
	if _, ok := parseBlob([]byte(`not json`)); ok {
		t.Error("garbage must not parse")
	}
}

func TestIsIneligibleDistinguishes403s(t *testing.T) {
	disabled := []byte(`{"error":{"status":"PERMISSION_DENIED","message":"Cloud Code Private API has not been used in project 1234"}}`)
	if !isIneligible(disabled) {
		t.Error("a disabled-API 403 should read as ineligible, not as an expired token")
	}
	stale := []byte(`{"error":{"status":"UNAUTHENTICATED","message":"Invalid Credentials"}}`)
	if isIneligible(stale) {
		t.Error("a credentials 403 must not read as ineligible")
	}
	if isIneligible([]byte(`garbage`)) {
		t.Error("an unparseable body must not read as ineligible")
	}
}

func TestRetryAfter(t *testing.T) {
	if got := retryAfter("30"); got != 30*time.Second {
		t.Errorf("retryAfter(30) = %v", got)
	}
	if got := retryAfter("0"); got != 0 {
		t.Errorf("retryAfter(0) = %v, want 0", got)
	}
	if got := retryAfter(""); got != 0 {
		t.Errorf("retryAfter(empty) = %v, want 0", got)
	}
	future := time.Now().Add(90 * time.Second).UTC().Format(http.TimeFormat)
	if got := retryAfter(future); got <= 0 {
		t.Errorf("retryAfter(HTTP date) = %v, want positive", got)
	}
	past := time.Now().Add(-time.Hour).UTC().Format(http.TimeFormat)
	if got := retryAfter(past); got != 0 {
		t.Errorf("retryAfter(past date) = %v, want 0", got)
	}
}

func TestRateLimitedUnwraps(t *testing.T) {
	var err error = RateLimited{RetryAfter: 5 * time.Second}
	if !errors.Is(err, ErrRateLimited) {
		t.Error("RateLimited should satisfy errors.Is(ErrRateLimited)")
	}
	if !strings.Contains(err.Error(), "5s") {
		t.Errorf("error text should mention the delay, got %q", err.Error())
	}
	if got := (RateLimited{}).Error(); got != ErrRateLimited.Error() {
		t.Errorf("bare RateLimited = %q", got)
	}
}

func TestMethodURLHonoursEnvOverrides(t *testing.T) {
	if got := methodURL("retrieveUserQuota"); got != defaultEndpoint+"/v1internal:retrieveUserQuota" {
		t.Errorf("default methodURL = %q", got)
	}
	t.Setenv("CODE_ASSIST_ENDPOINT", "http://127.0.0.1:8080/")
	t.Setenv("CODE_ASSIST_API_VERSION", "v1test")
	if got := methodURL("loadCodeAssist"); got != "http://127.0.0.1:8080/v1test:loadCodeAssist" {
		t.Errorf("overridden methodURL = %q", got)
	}
}
