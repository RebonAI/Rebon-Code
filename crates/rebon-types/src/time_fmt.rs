//! ISO-8601 millisecond timestamp formatter (no chrono/time dependency).

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch on the wall clock. A clock set
/// before the epoch reads as `0` rather than panicking.
pub fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// The same reading as [`wall_clock_ms`], widened.
///
/// Timestamps that are compared against `Duration::as_millis` or stored
/// beside one want the wider type, and writing the cast at every call site
/// invites someone to reach for `SystemTime::now()` again instead.
pub fn wall_clock_ms_u128() -> u128 {
    wall_clock_ms() as u128
}

/// Format a [`SystemTime`] as an ISO-8601 UTC string with millisecond
/// precision, e.g. `"2025-01-15T10:30:45.123Z"`.
///
/// Matches the timestamp wire format used by transcript records exactly,
/// so transcript timestamps remain compatible.
///
/// Sub-epoch times are clamped to `UNIX_EPOCH` rather than panicking.
pub fn format_system_time_iso_ms(t: SystemTime) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let total_secs = dur.as_secs();
    let millis = dur.subsec_millis();

    let secs_per_day: u64 = 86_400;
    let days = (total_secs / secs_per_day) as i64;
    let sod = total_secs % secs_per_day;
    let hour = (sod / 3600) as u32;
    let minute = ((sod / 60) % 60) as u32;
    let second = (sod % 60) as u32;

    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

/// Howard Hinnant's `civil_from_days` (public-domain reference algorithm):
/// map days since 1970-01-01 into a Gregorian `(year, month, day)` triple.
fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m as u32, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_ms_is_after_the_epoch_and_monotonic_enough() {
        let first = wall_clock_ms();
        let second = wall_clock_ms();
        assert!(first > 1_600_000_000_000, "clock reads {first}");
        assert!(second >= first);
    }

    #[test]
    fn epoch_is_canonical_string() {
        assert_eq!(
            format_system_time_iso_ms(UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn clamps_pre_epoch_to_epoch() {
        let before = UNIX_EPOCH - std::time::Duration::from_secs(10);
        assert_eq!(
            format_system_time_iso_ms(before),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn known_fixtures() {
        let cases: &[(u64, &str)] = &[
            (1, "1970-01-01T00:00:00.001Z"),
            (1000, "1970-01-01T00:00:01.000Z"),
            (1_700_000_000_000, "2023-11-14T22:13:20.000Z"),
        ];
        for &(ms, expected) in cases {
            let t = UNIX_EPOCH + std::time::Duration::from_millis(ms);
            assert_eq!(
                format_system_time_iso_ms(t),
                expected,
                "formatter mismatch for {ms}ms since epoch",
            );
        }
    }

    #[test]
    fn handles_leap_year_feb_29() {
        let ms = 1_709_210_096_789u64;
        let t = UNIX_EPOCH + std::time::Duration::from_millis(ms);
        assert_eq!(format_system_time_iso_ms(t), "2024-02-29T12:34:56.789Z");
    }
}
