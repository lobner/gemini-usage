package status

import (
	"strings"
	"testing"
)

// Shaped after the real feed: a mix of ongoing and ended incidents, Gemini and
// non-Gemini products, and a "Multiple Products" record that only reveals its
// Gemini involvement through affected_products.
const feedSample = `[
  {
    "id": "ongoing-outage",
    "service_name": "Gemini",
    "affected_products": [{"title": "Gemini", "id": "npdyhgECDJ6tB66MxXyo"}],
    "severity": "high",
    "status_impact": "SERVICE_OUTAGE",
    "begin": "2026-08-17T06:00:00+00:00",
    "modified": "2026-08-17T07:15:00+00:00",
    "uri": "incidents/ongoing-outage",
    "external_desc": "**Title**\nGemini is unavailable for most users.\n**Description**\nWe are investigating.",
    "most_recent_update": {"status": "AVAILABLE", "when": "2026-08-17T07:15:00+00:00", "text": "## Summary\nWe are still\ninvestigating the issue."}
  },
  {
    "id": "ongoing-notebook",
    "service_name": "Gemini Notebook",
    "affected_products": [{"title": "Gemini Notebook", "id": "x"}],
    "severity": "low",
    "status_impact": "SERVICE_INFORMATION",
    "begin": "2026-08-17T05:00:00+00:00",
    "modified": "2026-08-17T05:30:00+00:00",
    "uri": "incidents/ongoing-notebook",
    "external_desc": "**Summary**\nSome NotebookLM exports are delayed."
  },
  {
    "id": "ended-gemini",
    "service_name": "Gemini",
    "affected_products": [{"title": "Gemini", "id": "npdyhgECDJ6tB66MxXyo"}],
    "severity": "medium",
    "status_impact": "SERVICE_DISRUPTION",
    "begin": "2026-06-10T10:20:00+00:00",
    "end": "2026-06-10T17:30:00+00:00",
    "uri": "incidents/ended-gemini",
    "external_desc": "**Title**\nAn old, resolved problem."
  },
  {
    "id": "ongoing-gmail",
    "service_name": "Gmail",
    "affected_products": [{"title": "Gmail", "id": "y"}],
    "severity": "high",
    "status_impact": "SERVICE_OUTAGE",
    "begin": "2026-08-17T04:00:00+00:00",
    "uri": "incidents/ongoing-gmail",
    "external_desc": "**Title**\nGmail is down."
  },
  {
    "id": "ongoing-multi",
    "service_name": "Multiple Products",
    "affected_products": [{"title": "Google Drive", "id": "z"}, {"title": "Gemini", "id": "npdyhgECDJ6tB66MxXyo"}],
    "severity": "medium",
    "status_impact": "SERVICE_DISRUPTION",
    "begin": "2026-08-17T03:00:00+00:00",
    "uri": "incidents/ongoing-multi",
    "external_desc": "**Title**\nSign-in is slow across several products."
  }
]`

func TestParseKeepsOnlyOngoingGeminiIncidents(t *testing.T) {
	s, err := parse([]byte(feedSample))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	got := map[string]bool{}
	for _, in := range s.Incidents {
		got[in.ID] = true
	}
	for _, want := range []string{"ongoing-outage", "ongoing-notebook", "ongoing-multi"} {
		if !got[want] {
			t.Errorf("expected incident %q to be listed", want)
		}
	}
	for _, unwanted := range []string{"ended-gemini", "ongoing-gmail"} {
		if got[unwanted] {
			t.Errorf("incident %q should have been filtered out", unwanted)
		}
	}
}

func TestParseTakesTheWorstIndicator(t *testing.T) {
	s, err := parse([]byte(feedSample))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if s.Indicator != "critical" {
		t.Errorf("indicator = %q, want critical (the outage wins)", s.Indicator)
	}
	if s.AllOperational() {
		t.Error("AllOperational should be false with a live outage")
	}
	if !strings.Contains(s.Description, "outage") {
		t.Errorf("description = %q, want it to mention an outage", s.Description)
	}
}

func TestParseReportsAllClearWhenNothingIsRunning(t *testing.T) {
	s, err := parse([]byte(`[
	  {"id":"a","service_name":"Gemini","affected_products":[{"title":"Gemini"}],
	   "status_impact":"SERVICE_OUTAGE","begin":"2026-01-01T00:00:00+00:00",
	   "end":"2026-01-01T01:00:00+00:00","uri":"incidents/a","external_desc":"**Title**\nOver."}
	]`))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if !s.AllOperational() {
		t.Errorf("indicator = %q, want none", s.Indicator)
	}
	if s.Description != "All Systems Operational" {
		t.Errorf("description = %q", s.Description)
	}
	if len(s.Incidents) != 0 || len(s.Affected) != 0 {
		t.Errorf("expected nothing listed, got %d incidents / %d components", len(s.Incidents), len(s.Affected))
	}
}

func TestParseDeduplicatesAffectedProducts(t *testing.T) {
	s, err := parse([]byte(feedSample))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	seen := map[string]int{}
	for _, c := range s.Affected {
		seen[c.Name]++
	}
	if seen["Gemini"] != 1 {
		t.Errorf("Gemini listed %d times, want exactly 1", seen["Gemini"])
	}
	// Gemini appears in both an outage and a disruption; the worse one must stick.
	for _, c := range s.Affected {
		if c.Name == "Gemini" && c.Status != "SERVICE_OUTAGE" {
			t.Errorf("Gemini status = %q, want SERVICE_OUTAGE (the worse of its two incidents)", c.Status)
		}
		if c.Operational() {
			t.Errorf("%q is listed as affected but reports operational", c.Name)
		}
	}
	if seen["Google Drive"] != 0 {
		t.Error("non-Gemini products must not be listed as affected")
	}
}

func TestIncidentFieldsAreRenderable(t *testing.T) {
	s, err := parse([]byte(feedSample))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	var outage Incident
	for _, in := range s.Incidents {
		if in.ID == "ongoing-outage" {
			outage = in
		}
	}
	if outage.Name != "Gemini is unavailable for most users." {
		t.Errorf("name = %q", outage.Name)
	}
	if outage.URL != dashboardBase+"incidents/ongoing-outage" {
		t.Errorf("url = %q", outage.URL)
	}
	if outage.Impact != "critical" {
		t.Errorf("impact = %q", outage.Impact)
	}
	if outage.UpdatedAt.IsZero() {
		t.Error("expected modified to parse into UpdatedAt")
	}
	// The update body is multi-line Markdown; it has to collapse to one line.
	if strings.ContainsAny(outage.LatestUpdate, "\n#") {
		t.Errorf("latest update not flattened: %q", outage.LatestUpdate)
	}
	if !strings.Contains(outage.LatestUpdate, "still investigating") {
		t.Errorf("latest update = %q", outage.LatestUpdate)
	}
}

func TestInformationOnlyIncidentIsMarkedMaintenance(t *testing.T) {
	s, err := parse([]byte(feedSample))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	for _, in := range s.Incidents {
		if in.ID == "ongoing-notebook" {
			if !in.Maintenance {
				t.Error("SERVICE_INFORMATION should be flagged as maintenance/notice")
			}
			if in.Impact != "minor" {
				t.Errorf("impact = %q, want minor", in.Impact)
			}
			return
		}
	}
	t.Fatal("ongoing-notebook not found")
}

func TestHeadlineSkipsMarkdownHeadings(t *testing.T) {
	cases := map[string]string{
		"**Title**\nThe real headline.\n**Description**\nMore.": "The real headline.",
		"**Summary**\nAnother   headline.":                      "Another headline.",
		"":                                                      "",
	}
	for in, want := range cases {
		if got := headline(in); got != want {
			t.Errorf("headline(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestUnknownImpactIsNeverAllClear(t *testing.T) {
	if impact("SERVICE_SOMETHING_NEW") == "none" {
		t.Error("an unrecognised impact must not read as all-clear")
	}
}

func TestHumanLowercasesShoutedEnums(t *testing.T) {
	cases := map[string]string{
		"SERVICE_DISRUPTION": "Service disruption",
		"SERVICE_OUTAGE":     "Service outage",
		"in_progress":        "In progress",
		"":                   "",
	}
	for in, want := range cases {
		if got := Human(in); got != want {
			t.Errorf("Human(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestParseRejectsGarbage(t *testing.T) {
	if _, err := parse([]byte(`{"not":"an array"}`)); err == nil {
		t.Error("expected an error for a non-array payload")
	}
}
