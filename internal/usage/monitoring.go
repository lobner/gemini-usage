package usage

// The Gemini API key's request count, from Cloud Monitoring. Google publishes
// a project's consumer-quota usage as quota/rate/net_usage on the
// consumer_quota resource. For the Gemini API it has project-wide request
// counts only: no series carries the model, so the per-model daily limits
// cannot be turned into percentages. This reads the generateContent request
// count for the current quota day instead.

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"net/url"
	"strconv"
	"time"
)

const (
	geminiService    = "generativelanguage.googleapis.com"
	metricQuotaUsage = "serviceruntime.googleapis.com/quota/rate/net_usage"
	// quotaGenerate counts GenerateContent and StreamGenerateContent calls.
	quotaGenerate = geminiService + "/generate_content_requests"
)

// requestsToday sums the project's generateContent requests since start.
// Monitoring does the sum (ALIGN_SUM over the whole window, then REDUCE_SUM
// across methods and regions), so the answer stays small on a busy day.
func requestsToday(ctx context.Context, tok adcToken, project string, start, end time.Time) (int64, error) {
	period := int64(math.Ceil(end.Sub(start).Minutes())) * 60
	if period < 60 {
		period = 60
	}
	q := url.Values{
		"filter": {fmt.Sprintf(`metric.type=%q AND resource.type="consumer_quota" AND resource.labels.service=%q AND metric.labels.quota_metric=%q`,
			metricQuotaUsage, geminiService, quotaGenerate)},
		"interval.startTime":             {start.UTC().Format(time.RFC3339)},
		"interval.endTime":               {end.UTC().Format(time.RFC3339)},
		"aggregation.alignmentPeriod":    {strconv.FormatInt(period, 10) + "s"},
		"aggregation.perSeriesAligner":   {"ALIGN_SUM"},
		"aggregation.crossSeriesReducer": {"REDUCE_SUM"},
	}
	body, err := googleGet(ctx, tok, monitoringBaseURL+"/v3/projects/"+url.PathEscape(project)+"/timeSeries?"+q.Encode())
	if err != nil {
		return 0, fmt.Errorf("reading the request count: %w", err)
	}
	return sumTimeSeries(body)
}

// sumTimeSeries adds up every point of every series. An answer with no series
// means no requests yet in the window.
func sumTimeSeries(body []byte) (int64, error) {
	var r struct {
		TimeSeries []struct {
			Points []struct {
				Value struct {
					Int64Value string `json:"int64Value"`
				} `json:"value"`
			} `json:"points"`
		} `json:"timeSeries"`
	}
	if err := json.Unmarshal(body, &r); err != nil {
		return 0, fmt.Errorf("parsing timeSeries JSON: %w", err)
	}
	var total int64
	for _, s := range r.TimeSeries {
		for _, p := range s.Points {
			n, err := strconv.ParseInt(p.Value.Int64Value, 10, 64)
			if err != nil {
				return 0, fmt.Errorf("parsing timeSeries value %q: %w", p.Value.Int64Value, err)
			}
			total += n
		}
	}
	return total, nil
}

// quotaDayStart is the last midnight in Pacific time, when Google resets the
// Gemini API's per-day quotas.
func quotaDayStart(now time.Time) time.Time {
	loc, err := time.LoadLocation("America/Los_Angeles")
	if err != nil {
		loc = time.FixedZone("PST", -8*60*60)
	}
	t := now.In(loc)
	return time.Date(t.Year(), t.Month(), t.Day(), 0, 0, 0, 0, loc)
}
