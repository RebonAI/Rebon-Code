//! Calendar-day bucketing for token activity.
//!
//! Buckets follow the **local** wall clock, so a UTC+8 user's 01:00 usage lands on
//! their own "today" instead of the previous UTC day. The offset is an explicit
//! parameter on the pure math so callers (and tests) never depend on the host's
//! timezone; [`local_day_number`] is the production entry point.
//!
//! Both the stats collector and the app's activity charts derive day numbers here
//! so the buckets and the dates they are keyed by can't drift apart.

const DAY_SECONDS: i64 = 86_400;

/// Days since the Unix epoch for `ms`, shifted east by `offset_seconds`.
pub fn day_number_from_ms(ms: u64, offset_seconds: i32) -> i64 {
    ((ms / 1000) as i64 + offset_seconds as i64).div_euclid(DAY_SECONDS)
}

/// The local UTC offset in effect at `ms`, east positive (UTC+8 → `28_800`).
/// Resolved per instant so timestamps from the other side of a DST switch bucket
/// by the rules that applied then.
pub fn local_utc_offset_seconds(ms: u64) -> i32 {
    use chrono::{Local, TimeZone};

    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|local| local.offset().local_minus_utc())
        .unwrap_or(0)
}

/// [`day_number_from_ms`] at the local offset — how production buckets usage.
pub fn local_day_number(ms: u64) -> i64 {
    day_number_from_ms(ms, local_utc_offset_seconds(ms))
}

/// The local day the wall clock is on right now.
pub fn current_local_day_number() -> i64 {
    local_day_number(crate::now_ms())
}

/// `YYYY-MM-DD` for a day number from this module.
pub fn date_from_day_number(day: i64) -> String {
    let (year, month, day) = civil_from_days(day);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Inverse of [`date_from_day_number`]; `None` for malformed dates.
pub fn day_number_from_date(date: &str) -> Option<i64> {
    let mut parts = date.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    days_from_civil(year, month, day)
}

pub fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let d = doy - (153 * mp + 2).div_euclid(5) + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + (m <= 2) as i64, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC_PLUS_8: i32 = 8 * 3_600;
    const UTC_MINUS_5: i32 = -5 * 3_600;

    fn ms(date: &str, hour: u64, minute: u64) -> u64 {
        let day = day_number_from_date(date).expect("valid date");
        (day as u64 * 86_400 + hour * 3_600 + minute * 60) * 1000
    }

    #[test]
    fn day_numbers_round_trip_through_dates() {
        assert_eq!(date_from_day_number(0), "1970-01-01");
        assert_eq!(day_number_from_date("1970-01-01"), Some(0));
        for date in ["2024-02-29", "2026-07-25", "2026-12-31"] {
            let day = day_number_from_date(date).expect("valid date");
            assert_eq!(date_from_day_number(day), date);
        }
        assert_eq!(
            day_number_from_date("2026-01-02").map(|day| day - 1),
            day_number_from_date("2026-01-01")
        );
        assert_eq!(day_number_from_date("2026-13-01"), None);
        assert_eq!(day_number_from_date("nope"), None);
    }

    #[test]
    fn an_eastern_offset_moves_after_midnight_usage_into_the_local_day() {
        // 2026-07-05T01:00 in UTC+8 is still 2026-07-04T17:00 UTC.
        let after_local_midnight = ms("2026-07-04", 17, 0);
        assert_eq!(
            date_from_day_number(day_number_from_ms(after_local_midnight, 0)),
            "2026-07-04"
        );
        assert_eq!(
            date_from_day_number(day_number_from_ms(after_local_midnight, UTC_PLUS_8)),
            "2026-07-05"
        );
    }

    #[test]
    fn a_western_offset_keeps_late_evening_usage_on_the_local_day() {
        // 2026-07-05T02:00 UTC is 2026-07-04T21:00 in UTC-5.
        let before_local_midnight = ms("2026-07-05", 2, 0);
        assert_eq!(
            date_from_day_number(day_number_from_ms(before_local_midnight, 0)),
            "2026-07-05"
        );
        assert_eq!(
            date_from_day_number(day_number_from_ms(before_local_midnight, UTC_MINUS_5)),
            "2026-07-04"
        );
    }

    #[test]
    fn local_day_number_agrees_with_the_offset_it_resolves() {
        let now = crate::now_ms();
        let offset = local_utc_offset_seconds(now);

        assert_eq!(local_day_number(now), day_number_from_ms(now, offset));
        assert_eq!(
            current_local_day_number(),
            local_day_number(crate::now_ms())
        );
    }
}
