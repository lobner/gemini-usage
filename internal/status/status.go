// Package status reads Google's public Workspace Status Dashboard
// (https://www.google.com/appsstatus/dashboard/) through its JSON incident feed
// and reports whether anything is going on with Gemini.
//
// The feed is a flat array of the most recent incidents across every Workspace
// product, open and closed alike, so this package does two things the Statuspage
// equivalent got for free: it filters to the Gemini products, and it decides
// which incidents are still running. An incident is ongoing exactly when it has
// no "end" timestamp — there is no per-incident resolved/investigating state
// machine to read (every update in the feed is stamped "AVAILABLE").
//
// No authentication is required — this is the same JSON the public dashboard
// itself renders.
package status

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// DefaultURL is the Workspace Status Dashboard's incident feed.
const DefaultURL = "https://www.google.com/appsstatus/dashboard/incidents.json"

// dashboardBase resolves an incident's relative "uri" ("incidents/<id>") to a
// page a browser can open.
const dashboardBase = "https://www.google.com/appsstatus/dashboard/"

const userAgent = "gemini-usage/1.0 (+https://github.com/lobner/gemini-usage)"

// Incident is an ongoing disruption affecting a Gemini product.
type Incident struct {
	ID           string
	Name         string
	Status       string    // the impact class, e.g. "SERVICE_DISRUPTION"
	Impact       string    // "none", "minor", "major", "critical"
	URL          string    // permalink on the dashboard
	LatestUpdate string    // body of the newest update, if any
	UpdatedAt    time.Time // zero if unknown
	Maintenance  bool      // informational notices rather than a disruption
}

// Component is a single affected Gemini product (e.g. "Gemini", "Gemini
// Notebook").
type Component struct {
	Name   string
	Status string // "SERVICE_DISRUPTION", "SERVICE_OUTAGE", …
}

// Operational reports whether this component is in its normal state. The feed
// only ever lists a product because something is wrong with it, so anything
// named here is by definition not operational.
func (c Component) Operational() bool { return c.Status == "" }

// Summary is the parsed dashboard: the headline indicator plus the detail the
// dropdown shows.
type Summary struct {
	Indicator   string // "none", "minor", "major", "critical"
	Description string // e.g. "All Systems Operational"
	Incidents   []Incident
	Affected    []Component
}

// AllOperational reports whether the dashboard says Gemini is fine. An empty
// indicator is treated as fine so an unexpected payload never raises a false
// alarm.
func (s Summary) AllOperational() bool {
	return s.Indicator == "" || s.Indicator == "none"
}

var httpClient = &http.Client{Timeout: 15 * time.Second}

// Fetch retrieves and parses the incident feed from rawURL (use DefaultURL).
func Fetch(ctx context.Context, rawURL string) (Summary, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return Summary{}, err
	}
	req.Header.Set("User-Agent", userAgent)
	req.Header.Set("Accept", "application/json")

	resp, err := httpClient.Do(req)
	if err != nil {
		return Summary{}, fmt.Errorf("status request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return Summary{}, fmt.Errorf("status dashboard HTTP %d", resp.StatusCode)
	}
	// The feed carries every product's recent history and runs to a few hundred
	// kilobytes, so the cap is roomier than a Statuspage summary would need.
	body, err := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	if err != nil {
		return Summary{}, fmt.Errorf("reading status dashboard: %w", err)
	}
	return parse(body)
}

type update struct {
	Status   string `json:"status"`
	Text     string `json:"text"`
	When     string `json:"when"`
	Modified string `json:"modified"`
}

type product struct {
	Title string `json:"title"`
	ID    string `json:"id"`
}

type incidentJSON struct {
	ID               string    `json:"id"`
	ServiceName      string    `json:"service_name"`
	AffectedProducts []product `json:"affected_products"`
	Severity         string    `json:"severity"`      // "low", "medium", "high"
	StatusImpact     string    `json:"status_impact"` // SERVICE_OUTAGE / _DISRUPTION / _INFORMATION
	Begin            string    `json:"begin"`
	End              string    `json:"end"` // empty while the incident is running
	Modified         string    `json:"modified"`
	URI              string    `json:"uri"`
	ExternalDesc     string    `json:"external_desc"`
	MostRecentUpdate *update   `json:"most_recent_update"`
}

func parse(body []byte) (Summary, error) {
	var raw []incidentJSON
	if err := json.Unmarshal(body, &raw); err != nil {
		return Summary{}, fmt.Errorf("parsing status JSON: %w", err)
	}

	s := Summary{Indicator: "none", Description: "All Systems Operational"}

	// Deduplicate affected products across incidents, keeping the worst impact
	// seen for each, so a product hit by two incidents is listed once.
	affected := map[string]string{}
	var order []string

	for _, in := range raw {
		if !isOngoing(in) {
			continue
		}
		names := geminiProducts(in)
		if len(names) == 0 {
			continue
		}

		inc := toIncident(in)
		s.Incidents = append(s.Incidents, inc)
		if rank(inc.Impact) > rank(s.Indicator) {
			s.Indicator = inc.Impact
		}

		for _, n := range names {
			prev, seen := affected[n]
			if !seen {
				order = append(order, n)
			}
			if !seen || impactRank(in.StatusImpact) > impactRank(prev) {
				affected[n] = in.StatusImpact
			}
		}
	}

	if !s.AllOperational() {
		s.Description = describe(order, s.Indicator)
	}
	for _, n := range order {
		s.Affected = append(s.Affected, Component{Name: n, Status: affected[n]})
	}
	return s, nil
}

// isOngoing reports whether an incident is still running. The feed marks the end
// of an incident by filling in "end"; while it is live that field is absent or
// empty.
func isOngoing(in incidentJSON) bool {
	return strings.TrimSpace(in.End) == ""
}

// geminiProducts returns the Gemini-branded products an incident affects. The
// match is on the product title rather than the service key so that "Gemini",
// "Gemini Notebook" and whatever Google brands next are all picked up, and so
// that a "Multiple Products" incident is caught by its product list rather than
// its service name.
func geminiProducts(in incidentJSON) []string {
	var out []string
	for _, p := range in.AffectedProducts {
		if t := strings.TrimSpace(p.Title); isGemini(t) {
			out = append(out, t)
		}
	}
	// Some records carry only a service name; don't lose those.
	if len(out) == 0 && isGemini(in.ServiceName) {
		out = append(out, strings.TrimSpace(in.ServiceName))
	}
	return out
}

func isGemini(name string) bool {
	return strings.Contains(strings.ToLower(name), "gemini")
}

func toIncident(in incidentJSON) Incident {
	inc := Incident{
		ID:          strings.TrimSpace(in.ID),
		Name:        headline(in.ExternalDesc),
		Status:      strings.TrimSpace(in.StatusImpact),
		Impact:      impact(in.StatusImpact),
		Maintenance: strings.TrimSpace(in.StatusImpact) == "SERVICE_INFORMATION",
	}
	if inc.Name == "" {
		inc.Name = strings.TrimSpace(in.ServiceName) + " incident"
	}
	if uri := strings.TrimSpace(in.URI); uri != "" {
		inc.URL = dashboardBase + strings.TrimPrefix(uri, "/")
	}
	if in.MostRecentUpdate != nil {
		inc.LatestUpdate = collapseSpace(stripMarkdown(in.MostRecentUpdate.Text))
	}
	for _, ts := range []string{in.Modified, in.Begin} {
		if t, err := time.Parse(time.RFC3339, strings.TrimSpace(ts)); err == nil {
			inc.UpdatedAt = t
			break
		}
	}
	return inc
}

// headline pulls a one-line name out of external_desc, which is a small Markdown
// document beginning with a "**Title**" or "**Summary**" heading followed by the
// sentence we want.
func headline(desc string) string {
	var first string
	for _, line := range strings.Split(desc, "\n") {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		// Skip the bold section headings ("**Title**", "**Description**", …).
		if strings.HasPrefix(line, "**") && strings.HasSuffix(line, "**") {
			continue
		}
		if strings.HasPrefix(line, "#") {
			continue
		}
		first = line
		break
	}
	return collapseSpace(stripMarkdown(first))
}

// stripMarkdown removes the little formatting the feed uses, so a menu row shows
// text rather than asterisks and hashes.
func stripMarkdown(s string) string {
	s = strings.ReplaceAll(s, "**", "")
	s = strings.ReplaceAll(s, "##", "")
	return strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(s), "#"))
}

// impact maps the dashboard's status_impact onto the same four-level scale the
// menu already speaks.
func impact(statusImpact string) string {
	switch strings.TrimSpace(statusImpact) {
	case "SERVICE_OUTAGE":
		return "critical"
	case "SERVICE_DISRUPTION":
		return "major"
	case "SERVICE_INFORMATION":
		return "minor"
	default:
		// Unknown impact still counts as something rather than nothing: a new
		// class Google invents must not read as "all clear".
		return "minor"
	}
}

func rank(indicator string) int {
	switch indicator {
	case "critical":
		return 3
	case "major":
		return 2
	case "minor":
		return 1
	default:
		return 0
	}
}

func impactRank(statusImpact string) int { return rank(impact(statusImpact)) }

// describe writes the headline the menu shows next to the dot, naming the
// products rather than repeating a generic phrase.
func describe(products []string, indicator string) string {
	noun := "service disruption"
	switch indicator {
	case "critical":
		noun = "service outage"
	case "minor":
		noun = "service notice"
	}
	switch len(products) {
	case 0:
		return "Gemini " + noun
	case 1:
		return products[0] + " " + noun
	default:
		return strings.Join(products, ", ") + " " + noun
	}
}

// collapseSpace flattens an update body to a single line so it fits a menu row
// or a tooltip.
func collapseSpace(s string) string {
	return strings.Join(strings.Fields(s), " ")
}

// Human turns a dashboard machine value ("SERVICE_DISRUPTION") into a display
// label ("Service disruption").
func Human(s string) string {
	s = strings.TrimSpace(strings.ReplaceAll(s, "_", " "))
	if s == "" {
		return ""
	}
	// The feed shouts its enums in upper case; lower them before capitalising, or
	// the menu row shouts too.
	if s == strings.ToUpper(s) {
		s = strings.ToLower(s)
	}
	r := []rune(s)
	return strings.ToUpper(string(r[0])) + string(r[1:])
}
