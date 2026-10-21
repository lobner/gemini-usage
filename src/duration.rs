//! Go's `time.Duration` text form ("1m30s", "15m0s", "250ms"), so the menu and
//! the error messages read as they did in the Go build.

use std::time::Duration;

/// Formats `d` as Go's `Duration.String` does.
pub fn go_string(d: Duration) -> String {
    let n = d.as_nanos();
    if n == 0 {
        return "0s".into();
    }
    if n < 1_000 {
        return format!("{n}ns");
    }
    if n < 1_000_000 {
        return fraction(n, 1_000, 3) + "µs";
    }
    if n < 1_000_000_000 {
        return fraction(n, 1_000_000, 6) + "ms";
    }

    let secs = n / 1_000_000_000;
    let h = secs / 3600;
    let m = secs / 60 % 60;
    let s = fraction(n % 60_000_000_000, 1_000_000_000, 9);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Rounds `d` to the nearest whole second, halfway values away from zero, as
/// Go's `d.Round(time.Second)` does.
pub fn round_to_second(d: Duration) -> Duration {
    let n = d.as_nanos() + 500_000_000;
    Duration::from_secs((n / 1_000_000_000) as u64)
}

/// Writes `n / unit` with its fractional digits, without trailing zeros.
fn fraction(n: u128, unit: u128, digits: usize) -> String {
    let whole = n / unit;
    let rest = n % unit;
    if rest == 0 {
        return whole.to_string();
    }
    let frac = format!("{rest:0digits$}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go() {
        let cases = [
            (Duration::ZERO, "0s"),
            (Duration::from_nanos(15), "15ns"),
            (Duration::from_micros(1500), "1.5ms"),
            (Duration::from_millis(250), "250ms"),
            (Duration::from_secs(45), "45s"),
            (Duration::from_secs(90), "1m30s"),
            (Duration::from_secs(150), "2m30s"),
            (Duration::from_secs(900), "15m0s"),
            (Duration::from_secs(3600), "1h0m0s"),
            (Duration::from_millis(179_500), "2m59.5s"),
        ];
        for (d, want) in cases {
            assert_eq!(go_string(d), want, "{d:?}");
        }
    }

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(
            round_to_second(Duration::from_millis(1499)),
            Duration::from_secs(1)
        );
        assert_eq!(
            round_to_second(Duration::from_millis(1500)),
            Duration::from_secs(2)
        );
        assert_eq!(round_to_second(Duration::from_millis(400)), Duration::ZERO);
    }
}
