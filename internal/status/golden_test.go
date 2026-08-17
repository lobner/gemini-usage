package status

import (
	"os"
	"strings"
	"testing"
)

// testdata/incidents.json is a trimmed capture of the real Workspace Status
// Dashboard feed. One genuine Gemini incident has had its "end" stamp removed so
// the file exercises the ongoing path; everything else — the Markdown in
// external_desc, the affected_products shape, the shouted enums — is untouched.
//
// The hand-written sample in status_test.go pins the behaviour; this pins the
// behaviour against the payload Google actually sends, which is what breaks when
// the feed's shape drifts.
func TestParseRealFeedCapture(t *testing.T) {
	body, err := os.ReadFile("testdata/incidents.json")
	if err != nil {
		t.Fatalf("reading fixture: %v", err)
	}

	s, err := parse(body)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}

	if s.AllOperational() {
		t.Fatalf("expected the doctored ongoing incident to register, got indicator %q", s.Indicator)
	}
	if len(s.Incidents) != 1 {
		t.Fatalf("got %d incidents, want exactly the one with no end stamp", len(s.Incidents))
	}

	inc := s.Incidents[0]
	if inc.Name == "" {
		t.Error("no headline extracted from the real external_desc")
	}
	// The real external_desc is Markdown with "**Title**" headings; none of that
	// may reach a menu row.
	if strings.ContainsAny(inc.Name, "*#\n") {
		t.Errorf("headline still carries Markdown: %q", inc.Name)
	}
	if !strings.HasPrefix(inc.URL, dashboardBase) {
		t.Errorf("incident URL = %q, want it resolved against the dashboard", inc.URL)
	}
	if inc.UpdatedAt.IsZero() {
		t.Error("expected a timestamp to parse from the real record")
	}
	if inc.Impact == "" {
		t.Error("expected an impact class")
	}

	// Only Gemini products may be listed, and each exactly once.
	if len(s.Affected) == 0 {
		t.Fatal("expected at least one affected Gemini product")
	}
	seen := map[string]bool{}
	for _, c := range s.Affected {
		if !isGemini(c.Name) {
			t.Errorf("non-Gemini product %q leaked into Affected", c.Name)
		}
		if seen[c.Name] {
			t.Errorf("product %q listed twice", c.Name)
		}
		seen[c.Name] = true
	}

	t.Logf("indicator=%q description=%q", s.Indicator, s.Description)
	t.Logf("incident: [%s] %s", Human(inc.Status), inc.Name)
}
