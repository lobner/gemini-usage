//! The request count of the Gemini API key, from Cloud Monitoring. Google
//! publishes the consumer-quota usage of a project as quota/rate/net_usage on
//! the consumer_quota resource. For the Gemini API it has project-wide request
//! counts only: no series carries the model, so the per-model daily limits
//! cannot become percentages. This reads the generateContent request count for
//! the current quota day instead.

use std::time::Instant;

use chrono::{DateTime, NaiveDate, TimeDelta, TimeZone, Utc};
use objc2_foundation::{NSDate, NSString, NSTimeZone};
use serde::Deserialize;

use super::Error;
use super::apikey::{AdcToken, google_get, monitoring_base_url, path_escape, query_escape};
use crate::json::nullable;

const GEMINI_SERVICE: &str = "generativelanguage.googleapis.com";
const METRIC_QUOTA_USAGE: &str = "serviceruntime.googleapis.com/quota/rate/net_usage";
/// Counts GenerateContent and StreamGenerateContent calls.
const QUOTA_GENERATE: &str = "generativelanguage.googleapis.com/generate_content_requests";

/// Adds up the generateContent requests of the project since `start`.
/// Monitoring does the sum (ALIGN_SUM over the whole window, then REDUCE_SUM
/// across methods and regions), so the answer stays small on a busy day.
pub(super) fn requests_today(
    tok: &AdcToken,
    project: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    deadline: Instant,
) -> Result<i64, Error> {
    let minutes = (end - start).num_milliseconds() as f64 / 60_000.0;
    let period = ((minutes.ceil() as i64) * 60).max(60);

    let filter = format!(
        "metric.type={METRIC_QUOTA_USAGE:?} AND resource.type=\"consumer_quota\" AND resource.labels.service={GEMINI_SERVICE:?} AND metric.labels.quota_metric={QUOTA_GENERATE:?}"
    );
    // Go's url.Values.Encode sorts the keys.
    let query = [
        ("aggregation.alignmentPeriod", format!("{period}s")),
        ("aggregation.crossSeriesReducer", "REDUCE_SUM".into()),
        ("aggregation.perSeriesAligner", "ALIGN_SUM".into()),
        ("filter", filter),
        ("interval.endTime", rfc3339(end)),
        ("interval.startTime", rfc3339(start)),
    ]
    .iter()
    .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
    .collect::<Vec<_>>()
    .join("&");

    let url = format!(
        "{}/v3/projects/{}/timeSeries?{query}",
        monitoring_base_url(),
        path_escape(project)
    );
    let body =
        google_get(tok, &url, &[], deadline).map_err(|e| e.wrap("reading the request count"))?;
    sum_time_series(&body)
}

/// Formats a time as Go's time.RFC3339 does in UTC: whole seconds and "Z".
fn rfc3339(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Adds up every point of every series. An answer with no series means no
/// requests yet in the window.
fn sum_time_series(body: &[u8]) -> Result<i64, Error> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Value {
        #[serde(rename = "int64Value", deserialize_with = "nullable")]
        int64_value: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Point {
        #[serde(deserialize_with = "nullable")]
        value: Value,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Series {
        #[serde(deserialize_with = "nullable")]
        points: Vec<Point>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Response {
        #[serde(rename = "timeSeries", deserialize_with = "nullable")]
        time_series: Vec<Series>,
    }

    let r: Response = serde_json::from_slice(body)
        .map_err(|e| Error::other(format!("parsing timeSeries JSON: {e}")))?;
    let mut total: i64 = 0;
    for p in r.time_series.iter().flat_map(|s| &s.points) {
        let v = &p.value.int64_value;
        let n: i64 = v.parse().map_err(|_| {
            Error::other(format!(
                "parsing timeSeries value {v:?}: strconv.ParseInt: parsing {v:?}: invalid syntax"
            ))
        })?;
        total += n;
    }
    Ok(total)
}

/// The last midnight in Pacific time, when Google resets the per-day quotas of
/// the Gemini API. The offset comes from the macOS time zone database, as Go's
/// time.LoadLocation reads the system one. Without it, PST (-8) is used.
pub(super) fn quota_day_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let offset = |t: DateTime<Utc>| -> i64 { pacific_offset_secs(t).unwrap_or(-8 * 3600) };

    let local = now + TimeDelta::seconds(offset(now));
    let midnight = NaiveDate::from(local.naive_utc())
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let naive = Utc.from_utc_datetime(&midnight);
    // The offset at midnight can differ from the offset now (on a day with a
    // daylight saving change), so ask again at the first guess.
    naive - TimeDelta::seconds(offset(naive - TimeDelta::seconds(offset(now))))
}

/// The offset of America/Los_Angeles from UTC at `t`, in seconds.
fn pacific_offset_secs(t: DateTime<Utc>) -> Option<i64> {
    let tz = NSTimeZone::timeZoneWithName(&NSString::from_str("America/Los_Angeles"))?;
    let date = NSDate::dateWithTimeIntervalSince1970(t.timestamp() as f64);
    Some(tz.secondsFromGMTForDate(&date) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_time_series_cases() {
        // The shape of a live answer: one series per method, each with points.
        let body = br#"{"timeSeries":[
	  {"metric":{"labels":{"method":"google.ai.generativelanguage.v1beta.GenerativeService.GenerateContent","quota_metric":"generativelanguage.googleapis.com/generate_content_requests"}},
	   "points":[{"value":{"int64Value":"1"}},{"value":{"int64Value":"1"}}]},
	  {"metric":{"labels":{"method":"google.ai.generativelanguage.v1beta.GenerativeService.StreamGenerateContent","quota_metric":"generativelanguage.googleapis.com/generate_content_requests"}},
	   "points":[{"value":{"int64Value":"3"}},{"value":{"int64Value":"1"}}]}
	],"unit":"1"}"#;
        assert_eq!(sum_time_series(body), Ok(6));
        // No requests yet today: Monitoring leaves timeSeries out.
        assert_eq!(sum_time_series(br#"{"unit":"1"}"#), Ok(0));
        assert!(
            sum_time_series(br#"{"timeSeries":[{"points":[{"value":{"doubleValue":1}}]}]}"#)
                .is_err()
        );
    }

    #[test]
    fn quota_day_start_is_pacific_midnight() {
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        // 07:00 PDT.
        assert_eq!(
            quota_day_start(at("2026-10-15T14:00:00Z")),
            at("2026-10-15T07:00:00Z")
        );
        // 23:30 PST, in winter.
        assert_eq!(
            quota_day_start(at("2026-01-16T07:30:00Z")),
            at("2026-01-15T08:00:00Z")
        );
        // The day after the change to daylight saving time (2026-03-08).
        assert_eq!(
            quota_day_start(at("2026-03-08T20:00:00Z")),
            at("2026-03-08T08:00:00Z")
        );
    }

    #[test]
    fn rfc3339_drops_fractions() {
        let t = DateTime::parse_from_rfc3339("2026-10-15T07:00:00.123Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(rfc3339(t), "2026-10-15T07:00:00Z");
    }
}
