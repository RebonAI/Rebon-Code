//! Minimal 5-field cron expression parsing + next-run calculation.
//!
//! Pure implementation for cron expressions:
//!
//! - Fields: minute, hour, day-of-month, month, day-of-week.
//! - Syntax: wildcard, `N`, `*/N` (step), `N-M[/S]` (range with optional
//!   step), comma-separated lists of any of the above.
//! - Day-of-week: `0` = Sunday, `7` accepted as Sunday alias.
//! - DoM + DoW both constrained → OR semantics (standard cron).
//! - Time math uses local timezone arithmetic.
//! - [`compute_next_cron_run`] walks minute-by-minute, bounded at 366 days.
//!
//! Only the arithmetic pieces live here. Task IO, jitter, and missed-task
//! detection live in [`super::tasks`].

use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike};

/// Expanded matching values for each of the five cron fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFields {
    pub minute: Vec<u8>,
    pub hour: Vec<u8>,
    pub day_of_month: Vec<u8>,
    pub month: Vec<u8>,
    pub day_of_week: Vec<u8>,
}

struct FieldRange {
    min: u8,
    max: u8,
}

const FIELD_RANGES: [FieldRange; 5] = [
    FieldRange { min: 0, max: 59 }, // minute
    FieldRange { min: 0, max: 23 }, // hour
    FieldRange { min: 1, max: 31 }, // day-of-month
    FieldRange { min: 1, max: 12 }, // month
    FieldRange { min: 0, max: 6 },  // day-of-week (7 = Sunday alias)
];

/// Parse a single cron field into a sorted list of matching values. Returns
/// `None` on invalid syntax, out-of-range numbers, or empty match sets.
fn expand_field(field: &str, range: &FieldRange) -> Option<Vec<u8>> {
    let min = range.min;
    let max = range.max;
    let is_dow = min == 0 && max == 6;
    let mut out: Vec<u8> = Vec::new();
    let mut push = |v: u8| {
        if !out.contains(&v) {
            out.push(v);
        }
    };

    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }

        // Wildcard or `*/N`.
        if let Some(rest) = part.strip_prefix('*') {
            let step: u32 = if rest.is_empty() {
                1
            } else if let Some(n) = rest.strip_prefix('/') {
                n.parse().ok().filter(|&n: &u32| n >= 1)?
            } else {
                return None;
            };
            let mut i = min as u32;
            while i <= max as u32 {
                push(i as u8);
                i += step;
            }
            continue;
        }

        // `N-M` or `N-M/S`.
        if let Some((lo_raw, hi_step)) = part.split_once('-') {
            let (hi_raw, step) = match hi_step.split_once('/') {
                Some((h, s)) => (h, s.parse::<u32>().ok().filter(|&n| n >= 1)?),
                None => (hi_step, 1u32),
            };
            let lo: u32 = lo_raw.parse().ok()?;
            let hi: u32 = hi_raw.parse().ok()?;
            let eff_max: u32 = if is_dow { 7 } else { max as u32 };
            if lo > hi || lo < min as u32 || hi > eff_max {
                return None;
            }
            let mut i = lo;
            while i <= hi {
                let v = if is_dow && i == 7 { 0 } else { i as u8 };
                push(v);
                i += step;
            }
            continue;
        }

        // Plain integer.
        let n: u32 = part.parse().ok()?;
        let n_norm = if is_dow && n == 7 { 0 } else { n as u8 };
        if is_dow {
            if n > 7 {
                return None;
            }
        } else if n < min as u32 || n > max as u32 {
            return None;
        }
        push(n_norm);
    }

    if out.is_empty() {
        return None;
    }
    out.sort();
    Some(out)
}

/// Parse a 5-field cron string. Returns `None` on any invalid or
/// unsupported syntax.
pub fn parse_cron_expression(expr: &str) -> Option<CronFields> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    let minute = expand_field(parts[0], &FIELD_RANGES[0])?;
    let hour = expand_field(parts[1], &FIELD_RANGES[1])?;
    let day_of_month = expand_field(parts[2], &FIELD_RANGES[2])?;
    let month = expand_field(parts[3], &FIELD_RANGES[3])?;
    let day_of_week = expand_field(parts[4], &FIELD_RANGES[4])?;
    Some(CronFields {
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
    })
}

/// Next local-time match strictly after `from`. Bounded at 366 days; returns
/// `None` when no match inside that window (impossible for a valid cron, but
/// the type must admit it).
///
/// DoM + DoW OR semantics when both are constrained — same as vixie-cron.
pub fn compute_next_cron_run(
    fields: &CronFields,
    from: DateTime<Local>,
) -> Option<DateTime<Local>> {
    let dom_wild = fields.day_of_month.len() == 31;
    let dow_wild = fields.day_of_week.len() == 7;

    // Round up to the next whole minute (strictly after `from`).
    let mut t = from.with_second(0).and_then(|d| d.with_nanosecond(0))? + Duration::minutes(1);

    let max_iter = 366 * 24 * 60;
    for _ in 0..max_iter {
        let month = t.month() as u8;
        if !fields.month.contains(&month) {
            // Jump to start of next month, 00:00 local.
            let (y, m) = if t.month() == 12 {
                (t.year() + 1, 1)
            } else {
                (t.year(), t.month() + 1)
            };
            let candidate = Local.with_ymd_and_hms(y, m, 1, 0, 0, 0).single();
            match candidate {
                Some(next) => {
                    t = next;
                    continue;
                }
                None => return None,
            }
        }

        let dom = t.day() as u8;
        let dow = t.weekday().num_days_from_sunday() as u8;
        let day_matches = if dom_wild && dow_wild {
            true
        } else if dom_wild {
            fields.day_of_week.contains(&dow)
        } else if dow_wild {
            fields.day_of_month.contains(&dom)
        } else {
            fields.day_of_month.contains(&dom) || fields.day_of_week.contains(&dow)
        };

        if !day_matches {
            // Jump to start of next day, 00:00 local.
            let midnight = t
                .with_hour(0)
                .and_then(|d| d.with_minute(0))
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))?;
            t = midnight + Duration::days(1);
            continue;
        }

        if !fields.hour.contains(&(t.hour() as u8)) {
            let next_hour = t
                .with_minute(0)
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))?;
            t = next_hour + Duration::hours(1);
            continue;
        }

        if !fields.minute.contains(&(t.minute() as u8)) {
            t += Duration::minutes(1);
            continue;
        }

        return Some(t);
    }

    None
}

// --- cron_to_human ---------------------------------------------------------

const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn format_local_time(minute: u32, hour: u32) -> String {
    let (h12, suffix) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    if minute == 0 {
        format!("{}:00 {}", h12, suffix)
    } else {
        format!("{}:{:02} {}", h12, minute, suffix)
    }
}

/// Render a cron expression as a short English phrase. Narrow pattern set —
/// anything exotic falls back to the raw cron string. Hours and minutes are
/// rendered as written; cron fields are local time.
pub fn cron_to_human(cron: &str) -> String {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() != 5 {
        return cron.to_string();
    }
    let (minute_s, hour_s, dom_s, month_s, dow_s) =
        (parts[0], parts[1], parts[2], parts[3], parts[4]);

    let is_num = |s: &str| -> Option<u32> { s.parse::<u32>().ok() };
    let every_step_minute = minute_s
        .strip_prefix("*/")
        .and_then(|n| n.parse::<u32>().ok());
    let every_step_hour = hour_s
        .strip_prefix("*/")
        .and_then(|n| n.parse::<u32>().ok());

    // `*/N * * * *`
    if let Some(n) = every_step_minute {
        if hour_s == "*" && dom_s == "*" && month_s == "*" && dow_s == "*" {
            return if n == 1 {
                "Every minute".into()
            } else {
                format!("Every {} minutes", n)
            };
        }
    }

    // `M * * * *`
    if let Some(m) = is_num(minute_s) {
        if hour_s == "*" && dom_s == "*" && month_s == "*" && dow_s == "*" {
            return if m == 0 {
                "Every hour".into()
            } else {
                format!("Every hour at :{:02}", m)
            };
        }

        // `M */N * * *`
        if let Some(n) = every_step_hour {
            if dom_s == "*" && month_s == "*" && dow_s == "*" {
                let suffix = if m == 0 {
                    String::new()
                } else {
                    format!(" at :{:02}", m)
                };
                return if n == 1 {
                    format!("Every hour{}", suffix)
                } else {
                    format!("Every {} hours{}", n, suffix)
                };
            }
        }
    }

    // Remaining cases require both minute and hour to be integers.
    let (Some(m), Some(h)) = (is_num(minute_s), is_num(hour_s)) else {
        return cron.to_string();
    };

    // `M H * * *`
    if dom_s == "*" && month_s == "*" && dow_s == "*" {
        return format!("Every day at {}", format_local_time(m, h));
    }

    // `M H * * D` (single digit)
    if dom_s == "*" && month_s == "*" && dow_s.len() == 1 {
        if let Ok(d) = dow_s.parse::<u32>() {
            let idx = (d % 7) as usize;
            if let Some(name) = DAY_NAMES.get(idx) {
                return format!("Every {} at {}", name, format_local_time(m, h));
            }
        }
    }

    // `M H * * 1-5`
    if dom_s == "*" && month_s == "*" && dow_s == "1-5" {
        return format!("Weekdays at {}", format_local_time(m, h));
    }

    cron.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn parse_wildcard_fields() {
        let fields = parse_cron_expression("* * * * *").unwrap();
        assert_eq!(fields.minute.len(), 60);
        assert_eq!(fields.hour.len(), 24);
        assert_eq!(fields.day_of_month.len(), 31);
        assert_eq!(fields.month.len(), 12);
        assert_eq!(fields.day_of_week.len(), 7);
    }

    #[test]
    fn parse_steps_lists_ranges() {
        let f = parse_cron_expression("*/15 9-17 1,15 * 1-5").unwrap();
        assert_eq!(f.minute, vec![0, 15, 30, 45]);
        assert_eq!(f.hour, vec![9, 10, 11, 12, 13, 14, 15, 16, 17]);
        assert_eq!(f.day_of_month, vec![1, 15]);
        assert_eq!(f.day_of_week, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_sunday_alias_seven() {
        let f = parse_cron_expression("0 9 * * 7").unwrap();
        assert_eq!(f.day_of_week, vec![0]);
        let f2 = parse_cron_expression("0 9 * * 5-7").unwrap();
        assert_eq!(f2.day_of_week, vec![0, 5, 6]);
    }

    #[test]
    fn reject_bad_syntax() {
        assert!(parse_cron_expression("").is_none());
        assert!(parse_cron_expression("* * * *").is_none()); // 4 fields
        assert!(parse_cron_expression("60 * * * *").is_none()); // minute overflow
        assert!(parse_cron_expression("* 24 * * *").is_none()); // hour overflow
        assert!(parse_cron_expression("* * 0 * *").is_none()); // DoM underflow
        assert!(parse_cron_expression("* * * 13 *").is_none()); // month overflow
        assert!(parse_cron_expression("* * * * 8").is_none()); // DoW overflow
        assert!(parse_cron_expression("*/0 * * * *").is_none());
        assert!(parse_cron_expression("5-3 * * * *").is_none()); // reversed range
        assert!(parse_cron_expression("a * * * *").is_none());
    }

    #[test]
    fn next_run_minute_wildcard_advances_one_minute() {
        let f = parse_cron_expression("* * * * *").unwrap();
        let next = compute_next_cron_run(&f, at(2024, 1, 1, 12, 0)).unwrap();
        assert_eq!(next, at(2024, 1, 1, 12, 1));
    }

    #[test]
    fn next_run_on_specific_day_of_week() {
        // 0 9 * * 1 = Every Monday at 9am. 2024-01-01 is a Monday.
        let f = parse_cron_expression("0 9 * * 1").unwrap();
        let next = compute_next_cron_run(&f, at(2024, 1, 1, 8, 0)).unwrap();
        assert_eq!(next, at(2024, 1, 1, 9, 0));
        let after = compute_next_cron_run(&f, at(2024, 1, 1, 9, 0)).unwrap();
        assert_eq!(after, at(2024, 1, 8, 9, 0));
    }

    #[test]
    fn next_run_dom_or_dow_semantics() {
        // 0 9 1 * 1 = 9am on the 1st OR on Mondays. 2024-01-02 Tue → next match 2024-01-08 Mon (or 1st of Feb if sooner).
        let f = parse_cron_expression("0 9 1 * 1").unwrap();
        let next = compute_next_cron_run(&f, at(2024, 1, 2, 12, 0)).unwrap();
        // 2024-01-08 is Monday, which is before 2024-02-01.
        assert_eq!(next, at(2024, 1, 8, 9, 0));
    }

    #[test]
    fn cron_to_human_common_patterns() {
        // Bare `* * * * *` falls through — only `*/N` renders as "Every N minutes".
        assert_eq!(cron_to_human("* * * * *"), "* * * * *");
        assert_eq!(cron_to_human("*/1 * * * *"), "Every minute");
        assert_eq!(cron_to_human("*/5 * * * *"), "Every 5 minutes");
        assert_eq!(cron_to_human("0 * * * *"), "Every hour");
        assert_eq!(cron_to_human("15 * * * *"), "Every hour at :15");
        assert_eq!(cron_to_human("0 */2 * * *"), "Every 2 hours");
        assert_eq!(cron_to_human("15 */2 * * *"), "Every 2 hours at :15");
        assert_eq!(cron_to_human("0 9 * * *"), "Every day at 9:00 AM");
        assert_eq!(cron_to_human("30 14 * * *"), "Every day at 2:30 PM");
        assert_eq!(cron_to_human("0 9 * * 1"), "Every Monday at 9:00 AM");
        assert_eq!(cron_to_human("30 17 * * 1-5"), "Weekdays at 5:30 PM");
        // Fallback for unrecognized patterns.
        assert_eq!(cron_to_human("5 3 1 6 *"), "5 3 1 6 *");
    }

    #[test]
    fn next_run_month_skip() {
        // Only April, at 30 14 27 4 * — April 27 14:30.
        let f = parse_cron_expression("30 14 27 4 *").unwrap();
        let next = compute_next_cron_run(&f, at(2024, 5, 1, 0, 0)).unwrap();
        assert_eq!(next, at(2025, 4, 27, 14, 30));
    }
}
