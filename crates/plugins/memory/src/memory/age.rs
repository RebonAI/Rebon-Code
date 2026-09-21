//! Memory age and freshness utilities.
//!
//! Provides human-readable age
//! strings and staleness caveats for recalled memories.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days elapsed since mtime. Floor-rounded — 0 for today, 1 for
/// yesterday, 2+ for older. Negative inputs (future mtime, clock skew)
/// clamp to 0.
pub fn memory_age_days(mtime_ms: i64) -> u64 {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff = now_ms - mtime_ms;
    if diff <= 0 {
        return 0;
    }
    (diff / 86_400_000) as u64
}

/// Human-readable age string. Models are poor at date arithmetic —
/// a raw ISO timestamp doesn't trigger staleness reasoning the way
/// "47 days ago" does.
pub fn memory_age(mtime_ms: i64) -> String {
    let d = memory_age_days(mtime_ms);
    match d {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{n} days ago"),
    }
}

/// Plain-text staleness caveat for memories >1 day old. Returns empty
/// string for fresh (today/yesterday) memories — warning there is noise.
///
/// Motivated by user reimplements of stale code-state memories being asserted
/// as fact — file:line citations to code that has since changed.
pub fn memory_freshness_text(mtime_ms: i64) -> String {
    let d = memory_age_days(mtime_ms);
    if d <= 1 {
        return String::new();
    }
    format!(
        "This memory is {d} days old. \
         Memories are point-in-time observations, not live state \u{2014} \
         claims about code behavior or file:line citations may be outdated. \
         Verify against current code before asserting as fact."
    )
}

/// Per-memory staleness note wrapped in `<system-reminder>` tags.
/// Returns empty string for memories <= 1 day old.
pub fn memory_freshness_note(mtime_ms: i64) -> String {
    let text = memory_freshness_text(mtime_ms);
    if text.is_empty() {
        return String::new();
    }
    format!("<system-reminder>{text}</system-reminder>\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn age_days_today() {
        assert_eq!(memory_age_days(now_ms()), 0);
    }

    #[test]
    fn age_days_yesterday() {
        let yesterday = now_ms() - 86_400_000;
        assert_eq!(memory_age_days(yesterday), 1);
    }

    #[test]
    fn age_days_future_clamps_to_zero() {
        let future = now_ms() + 86_400_000;
        assert_eq!(memory_age_days(future), 0);
    }

    #[test]
    fn age_days_three_days_ago() {
        let three_days = now_ms() - 3 * 86_400_000;
        assert_eq!(memory_age_days(three_days), 3);
    }

    #[test]
    fn memory_age_strings() {
        assert_eq!(memory_age(now_ms()), "today");
        assert_eq!(memory_age(now_ms() - 86_400_000), "yesterday");
        assert_eq!(memory_age(now_ms() - 5 * 86_400_000), "5 days ago");
    }

    #[test]
    fn freshness_text_empty_for_recent() {
        assert!(memory_freshness_text(now_ms()).is_empty());
        assert!(memory_freshness_text(now_ms() - 86_400_000).is_empty());
    }

    #[test]
    fn freshness_text_non_empty_for_old() {
        let old = now_ms() - 10 * 86_400_000;
        let text = memory_freshness_text(old);
        assert!(text.contains("10 days old"));
        assert!(text.contains("Verify against current code"));
    }

    #[test]
    fn freshness_note_wraps_in_system_reminder() {
        let old = now_ms() - 5 * 86_400_000;
        let note = memory_freshness_note(old);
        assert!(note.starts_with("<system-reminder>"));
        assert!(note.contains("</system-reminder>"));
    }

    #[test]
    fn freshness_note_empty_for_recent() {
        assert!(memory_freshness_note(now_ms()).is_empty());
    }
}
