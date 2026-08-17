//go:build livetest

// Diagnostic: queries the live Workspace Status Dashboard feed and prints what
// the menu would show. Build-tagged, opt-in (needs network access).
//
//	go test -tags livetest -run TestLiveStatus -v ./internal/status
package status

import (
	"context"
	"os"
	"testing"
	"time"
)

func TestLiveStatus(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	// Honour the same override the app uses, so a doctored feed served locally
	// can be pointed at to exercise the outage path without waiting for a real
	// Gemini incident.
	url := DefaultURL
	if v := os.Getenv("STATUS_URL"); v != "" {
		url = v
		t.Logf("using STATUS_URL override: %s", url)
	}

	s, err := Fetch(ctx, url)
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	t.Logf("indicator = %q (%s), allOperational = %v", s.Indicator, s.Description, s.AllOperational())
	for _, inc := range s.Incidents {
		t.Logf("incident: [%s] %s — %s", Human(inc.Status), inc.Name, inc.URL)
	}
	for _, c := range s.Affected {
		t.Logf("affected: %s (%s)", c.Name, Human(c.Status))
	}
	if s.Description == "" {
		t.Errorf("expected a status description")
	}
}
