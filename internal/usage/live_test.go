//go:build livetest

// Diagnostic: queries the live Code Assist quota endpoint with the Gemini CLI's
// stored OAuth token and prints every tier meter. Build-tagged, opt-in (needs a
// valid, unexpired `gemini` login).
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
		t.Skipf("no Gemini CLI credentials at %s — run `gemini` first", CredentialsPath())
	case errors.Is(err, ErrTokenExpired):
		t.Skip("OAuth token expired — run any `gemini` command, then retry")
	case errors.Is(err, ErrNotEligible):
		t.Skip("this account has no Code Assist project — run `gemini` once to onboard")
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
		t.Logf("%-12s = %3d%% used  (model %s, resets %s)", m.Name, m.Percent, m.ModelID, reset)
		if m.Percent < 0 || m.Percent > 100 {
			t.Errorf("percent out of range for %s: %d", m.Name, m.Percent)
		}
	}
	t.Logf("headline: %+v", u.Headline())
}
