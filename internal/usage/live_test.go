//go:build livetest

// Diagnostic: queries the live Antigravity quota summary with agy's stored
// OAuth token and prints every meter. Build-tagged, opt-in (needs a valid,
// unexpired `agy` login).
//
//	go test -tags livetest -run TestLiveFetch -v ./internal/usage
package usage

import (
	"context"
	"errors"
	"testing"
	"time"
)

func TestLiveFetch(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()

	u, err := Fetch(ctx)
	switch {
	case errors.Is(err, ErrNoCredentials):
		t.Skip("no Antigravity credentials - run `agy` and log in first")
	case errors.Is(err, ErrTokenExpired):
		t.Skip("OAuth token expired - run `agy`, then retry")
	case errors.Is(err, ErrNotEligible):
		t.Skipf("this account has no Antigravity quota: %v", err)
	case err != nil:
		t.Fatalf("Fetch: %v", err)
	}

	if len(u.Tiers) == 0 {
		t.Fatal("Fetch returned no tiers")
	}
	for _, m := range u.Tiers {
		reset := "unknown"
		if m.HasReset {
			reset = m.ResetsAt.Local().Format("Mon 15:04")
		}
		t.Logf("%-22s = %3d%% used  (bucket %s, resets %s)", m.Name, m.Percent, m.Bucket, reset)
		if m.Percent < 0 || m.Percent > 100 {
			t.Errorf("percent out of range for %s: %d", m.Name, m.Percent)
		}
	}
	t.Logf("headline: %+v", u.Headline())
}

// Diagnostic for the API-key row: checks GEMINI_API_KEY and prints today's
// request count for its project. Needs a valid key and
// `gcloud auth application-default login`.
//
//	go test -tags livetest -run TestLiveFetchAPIKey -v ./internal/usage
func TestLiveFetchAPIKey(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()

	k, err := FetchAPIKey(ctx)
	switch {
	case errors.Is(err, ErrNoAPIKey):
		t.Skip("no GEMINI_API_KEY in the environment or the login shell")
	case errors.Is(err, ErrNoADC):
		t.Skipf("no gcloud login: %v", err)
	case err != nil:
		t.Fatalf("FetchAPIKey: %v", err)
	}
	t.Logf("project %s: %d requests since %s", k.Project, k.Requests, k.Since.Local().Format("Mon 15:04"))
	if k.Requests < 0 {
		t.Errorf("negative request count: %d", k.Requests)
	}
}
