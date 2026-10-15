package usage

import (
	"encoding/base64"
	"errors"
	"net/http"
	"strings"
	"testing"
	"time"
)

// A trimmed live retrieveUserQuotaSummary answer, with some use added. Google
// lists the weekly window before the 5-hour one.
const summarySample = `{
  "groups": [
    {"displayName": "Gemini Models", "description": "Models within this group: Gemini Flash, Gemini Pro",
     "buckets": [
       {"bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly", "resetTime": "2026-10-22T14:46:35Z", "remainingFraction": 0.9},
       {"bucketId": "gemini-5h", "displayName": "Five Hour Limit Remaining", "window": "5h", "resetTime": "2026-10-15T19:46:35Z", "remainingFraction": 0.375}
     ]},
    {"displayName": "Claude and GPT models",
     "buckets": [
       {"bucketId": "3p-weekly", "window": "weekly", "resetTime": "2026-10-22T14:46:35Z", "remainingFraction": 1},
       {"bucketId": "3p-5h", "window": "5h", "resetTime": "2026-10-15T19:46:35Z", "remainingFraction": 1}
     ]}
  ],
  "description": "Within each group, models share a weekly limit and a 5-hour limit."
}`

func TestParseSummaryOrdersGroupsAndWindows(t *testing.T) {
	u, err := parseSummary([]byte(summarySample))
	if err != nil {
		t.Fatalf("parseSummary: %v", err)
	}
	want := []string{"Gemini 5h", "Gemini weekly", "Claude and GPT 5h", "Claude and GPT weekly"}
	if len(u.Tiers) != len(want) {
		t.Fatalf("got %d meters, want %d", len(u.Tiers), len(want))
	}
	for i, name := range want {
		if u.Tiers[i].Name != name {
			t.Errorf("meter %d = %q, want %q", i, u.Tiers[i].Name, name)
		}
	}
	m, _ := u.Tier("Gemini 5h")
	if m.Percent != 63 || m.Bucket != "gemini-5h" || !m.HasReset {
		t.Errorf("Gemini 5h = %+v, want 63%% used of gemini-5h with a reset", m)
	}
	if m, _ := u.Tier("Gemini weekly"); m.Percent != 10 {
		t.Errorf("Gemini weekly = %d%%, want 10", m.Percent)
	}
}

func TestHeadlineIsGeminiWindows(t *testing.T) {
	u, _ := parseSummary([]byte(summarySample))
	h := u.Headline()
	if len(h) != 2 || h[0].Name != "Gemini 5h" || h[1].Name != "Gemini weekly" {
		t.Errorf("headline = %+v, want Gemini 5h and weekly", h)
	}

	// Without a Gemini group, the first group stands in.
	other := Usage{Tiers: []Meter{
		{Name: "X 5h", Group: "X", Bucket: "x-5h"},
		{Name: "X weekly", Group: "X", Bucket: "x-weekly"},
		{Name: "Y 5h", Group: "Y", Bucket: "y-5h"},
	}}
	if h := other.Headline(); len(h) != 2 || h[0].Group != "X" || h[1].Group != "X" {
		t.Errorf("fallback headline = %+v, want the X group", h)
	}
	if h := (Usage{}).Headline(); h != nil {
		t.Errorf("empty headline = %+v, want nil", h)
	}
}

func TestToMeterInvertsRoundsAndClamps(t *testing.T) {
	for _, c := range []struct {
		remaining float64
		want      int
	}{{1, 0}, {0, 100}, {0.554, 45}, {-0.2, 100}, {1.3, 0}} {
		r := c.remaining
		if got := toMeter("Gemini", summaryBucket{BucketID: "gemini-5h", Window: "5h", RemainingFraction: &r}, time.Now()).Percent; got != c.want {
			t.Errorf("remaining %v → %d%%, want %d%%", c.remaining, got, c.want)
		}
	}
}

func TestIdleWindowsAreSpotted(t *testing.T) {
	now := time.Date(2026, 10, 15, 14, 46, 36, 0, time.UTC)
	one, used := 1.0, 0.9
	for _, c := range []struct {
		name      string
		window    string
		remaining *float64
		reset     time.Time
		want      bool
	}{
		{"unused 5h", "5h", &one, now.Add(5*time.Hour - time.Second), true},
		{"unused weekly", "weekly", &one, now.Add(7 * 24 * time.Hour), true},
		{"used: the reset is real", "5h", &used, now.Add(5*time.Hour - time.Second), false},
		{"0% but started earlier", "5h", &one, now.Add(3 * time.Hour), false},
		{"unknown window", "daily", &one, now.Add(24 * time.Hour), false},
	} {
		b := summaryBucket{Window: c.window, RemainingFraction: c.remaining, ResetTime: c.reset.Format(time.RFC3339)}
		if got := toMeter("Gemini", b, now).Idle; got != c.want {
			t.Errorf("%s: Idle = %v, want %v", c.name, got, c.want)
		}
	}
}

func TestParseSummaryRejectsEmpty(t *testing.T) {
	if _, err := parseSummary([]byte(`{"groups":[]}`)); err == nil {
		t.Error("an empty summary must be an error")
	}
	if _, err := parseSummary([]byte(`nope`)); err == nil {
		t.Error("bad JSON must be an error")
	}
}

func TestGroupName(t *testing.T) {
	for in, want := range map[string]string{
		"Gemini Models":         "Gemini",
		"Claude and GPT models": "Claude and GPT",
		"":                      "Models",
	} {
		if got := groupName(in); got != want {
			t.Errorf("groupName(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestDecodeKeyringAndParseBlob(t *testing.T) {
	blob := `{"auth_method":"consumer","id_token":"x","token":{"access_token":"ya29.test","expiry":"2026-10-15T17:42:51.28174+02:00","refresh_token":"r","token_type":"Bearer"}}`
	for name, raw := range map[string]string{
		"base64": "go-keyring-base64:" + base64.StdEncoding.EncodeToString([]byte(blob)),
		"hex":    "go-keyring-encoded:" + hexOf(blob),
		"plain":  blob + "\n",
	} {
		c, ok := parseBlob(decodeKeyring([]byte(raw)))
		if !ok || c.accessToken != "ya29.test" {
			t.Errorf("%s: got %+v, %v", name, c, ok)
			continue
		}
		if want := time.Date(2026, 10, 15, 15, 42, 51, 281740000, time.UTC); !c.expiresAt.Equal(want) {
			t.Errorf("%s: expiry = %s, want %s", name, c.expiresAt.UTC(), want)
		}
	}
	if _, ok := parseBlob([]byte(`{"token":{}}`)); ok {
		t.Error("a blob without an access token must not parse")
	}
}

func hexOf(s string) string {
	const digits = "0123456789abcdef"
	out := make([]byte, 0, 2*len(s))
	for i := 0; i < len(s); i++ {
		out = append(out, digits[s[i]>>4], digits[s[i]&0xf])
	}
	return string(out)
}

func TestNotEligibleSpotsLicence403s(t *testing.T) {
	licence := []byte(`{"error":{"code":403,"status":"PERMISSION_DENIED","message":"You do not have a valid license of this product.",
	  "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SUBSCRIPTION_REQUIRED"}]}}`)
	ne, ok := notEligible(licence)
	if !ok || ne.Code != "SUBSCRIPTION_REQUIRED" {
		t.Errorf("licence 403 = %+v, %v; want SUBSCRIPTION_REQUIRED", ne, ok)
	}
	disabled := []byte(`{"error":{"status":"PERMISSION_DENIED","message":"Cloud Code Private API has not been used in project 1234"}}`)
	if _, ok := notEligible(disabled); !ok {
		t.Error("a disabled API is not eligible")
	}
	stale := []byte(`{"error":{"status":"PERMISSION_DENIED","message":"The caller does not have permission"}}`)
	if _, ok := notEligible(stale); ok {
		t.Error("a plain permission error reads as a stale token")
	}
}

func TestParseAgyVersion(t *testing.T) {
	for in, want := range map[string]string{
		"1.3.5\n":                "1.3.5",
		"agy version v1.4.0 (x)": "1.4.0",
		"no version here":        "",
	} {
		if got := parseAgyVersion(in); got != want {
			t.Errorf("parseAgyVersion(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestMethodURLHonoursEnvOverride(t *testing.T) {
	t.Setenv("CLOUDCODE_ENDPOINT", "http://127.0.0.1:9/")
	if got := methodURL(methodQuotaSummary); got != "http://127.0.0.1:9/v1internal:retrieveUserQuotaSummary" {
		t.Errorf("methodURL = %q", got)
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
func TestParseLoadCodeAssistReportsWhyNotEligible(t *testing.T) {
	body := []byte(`{"allowedTiers":[{"id":"standard-tier","userDefinedCloudaicompanionProject":true}],
	  "ineligibleTiers":[{"reasonCode":"UNSUPPORTED_CLIENT","reasonMessage":"This client is no longer supported for Gemini Code Assist for individuals.","tierId":"free-tier"}]}`)
	_, err := parseLoadCodeAssist(body)
	var ne NotEligible
	if !errors.As(err, &ne) || ne.Code != "UNSUPPORTED_CLIENT" {
		t.Fatalf("err = %v, want NotEligible UNSUPPORTED_CLIENT", err)
	}
	if !errors.Is(err, ErrNotEligible) {
		t.Error("NotEligible must match ErrNotEligible")
	}
	if id, err := parseLoadCodeAssist([]byte(`{"cloudaicompanionProject":"p-1"}`)); err != nil || id != "p-1" {
		t.Errorf("parseLoadCodeAssist = %q, %v; want p-1", id, err)
	}
	if _, err := parseLoadCodeAssist([]byte(`{}`)); err != ErrNotEligible {
		t.Errorf("no project and no reason: err = %v, want ErrNotEligible", err)
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
