//! The shell time display: turning an optional elapsed time and an optional
//! timeout into one short dim line of text, plus the duration/file-size format
//! helpers used elsewhere in this crate.
//!
//! Depending on which of the two inputs is present the text is nothing,
//! `(timeout X)`, `(Y)`, or `(Y · timeout X)`.

/// A shell time display value: just the text to show, produced by
/// `project_shell_time_display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellTimeDisplay {
    /// The final dim text the consumer should render.
    pub text: String,
}

/// The subset of `format_duration` options this shell slice needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DurationFormatOptions {
    /// Drop trailing components that are zero (e.g. `1h` instead of `1h 0m 0s`).
    pub hide_trailing_zeros: bool,
    /// Emit only the largest non-zero unit — days, then hours, then minutes,
    /// otherwise seconds.
    pub most_significant_only: bool,
}

/// Format a byte count as `N bytes`, `NKB`, `NMB` or `NGB`, with one decimal
/// place and no trailing `.0`.
pub fn format_file_size(size_in_bytes: u64) -> String {
    let kb = size_in_bytes as f64 / 1024.0;
    if kb < 1.0 {
        return format!("{size_in_bytes} bytes");
    }
    if kb < 1024.0 {
        return format_unit(kb, "KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format_unit(mb, "MB");
    }
    let gb = mb / 1024.0;
    format_unit(gb, "GB")
}

/// Format a millisecond count as `s`, `m s`, `h m s` or `d h m`, carrying
/// rounded-up seconds into minutes, minutes into hours and hours into days,
/// then applying `options`.
pub fn format_duration(ms: u64, options: DurationFormatOptions) -> String {
    if ms < 60_000 {
        if ms == 0 {
            return "0s".to_string();
        }
        if ms < 1 {
            return format!("{:.1}s", ms as f64 / 1000.0);
        }
        return format!("{}s", ms / 1000);
    }

    let mut days = ms / 86_400_000;
    let mut hours = (ms % 86_400_000) / 3_600_000;
    let mut minutes = (ms % 3_600_000) / 60_000;
    let mut seconds = ((ms % 60_000) as f64 / 1000.0).round() as u64;

    // Rounding can push a component to exactly its ceiling; carry it upward
    // instead of printing `60s` / `60m` / `24h`.
    if seconds == 60 {
        seconds = 0;
        minutes += 1;
    }
    if minutes == 60 {
        minutes = 0;
        hours += 1;
    }
    if hours == 24 {
        hours = 0;
        days += 1;
    }

    if options.most_significant_only {
        if days > 0 {
            return format!("{days}d");
        }
        if hours > 0 {
            return format!("{hours}h");
        }
        if minutes > 0 {
            return format!("{minutes}m");
        }
        return format!("{seconds}s");
    }

    if days > 0 {
        if options.hide_trailing_zeros && hours == 0 && minutes == 0 {
            return format!("{days}d");
        }
        if options.hide_trailing_zeros && minutes == 0 {
            return format!("{days}d {hours}h");
        }
        return format!("{days}d {hours}h {minutes}m");
    }
    if hours > 0 {
        if options.hide_trailing_zeros && minutes == 0 && seconds == 0 {
            return format!("{hours}h");
        }
        if options.hide_trailing_zeros && seconds == 0 {
            return format!("{hours}h {minutes}m");
        }
        return format!("{hours}h {minutes}m {seconds}s");
    }
    if minutes > 0 {
        if options.hide_trailing_zeros && seconds == 0 {
            return format!("{minutes}m");
        }
        return format!("{minutes}m {seconds}s");
    }
    format!("{seconds}s")
}

/// Build the shell time display text from an optional elapsed time in seconds
/// and an optional timeout in milliseconds. Returns `None` when both are
/// absent; the timeout is formatted with trailing zero components hidden.
pub fn project_shell_time_display(
    elapsed_time_seconds: Option<u64>,
    timeout_ms: Option<u64>,
) -> Option<ShellTimeDisplay> {
    if elapsed_time_seconds.is_none() && timeout_ms.is_none() {
        return None;
    }

    let timeout = timeout_ms.map(|value| {
        format_duration(
            value,
            DurationFormatOptions {
                hide_trailing_zeros: true,
                most_significant_only: false,
            },
        )
    });

    let text = match (elapsed_time_seconds, timeout) {
        (None, Some(timeout)) => format!("(timeout {timeout})"),
        (Some(elapsed_seconds), Some(timeout)) => {
            let elapsed = format_duration(elapsed_seconds * 1000, DurationFormatOptions::default());
            format!("({elapsed} \u{00b7} timeout {timeout})")
        }
        (Some(elapsed_seconds), None) => {
            let elapsed = format_duration(elapsed_seconds * 1000, DurationFormatOptions::default());
            format!("({elapsed})")
        }
        (None, None) => unreachable!("guarded above"),
    };

    Some(ShellTimeDisplay { text })
}

fn format_unit(value: f64, suffix: &str) -> String {
    let mut text = format!("{value:.1}");
    if text.ends_with(".0") {
        text.truncate(text.len() - 2);
    }
    text.push_str(suffix);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_file_size_bytes() {
        assert_eq!(format_file_size(500), "500 bytes");
    }

    #[test]
    fn format_file_size_kb_boundary() {
        assert_eq!(format_file_size(1024), "1KB");
        assert_eq!(format_file_size(1536), "1.5KB");
    }

    #[test]
    fn format_file_size_mb_boundary() {
        assert_eq!(format_file_size(1024 * 1024), "1MB");
        assert_eq!(format_file_size(3 * 1024 * 1024 / 2), "1.5MB");
    }

    #[test]
    fn format_file_size_gb_boundary() {
        assert_eq!(format_file_size(1024 * 1024 * 1024), "1GB");
    }

    #[test]
    fn format_duration_under_minute() {
        assert_eq!(format_duration(0, DurationFormatOptions::default()), "0s");
        assert_eq!(format_duration(999, DurationFormatOptions::default()), "0s");
        assert_eq!(
            format_duration(12_345, DurationFormatOptions::default()),
            "12s"
        );
    }

    #[test]
    fn format_duration_minutes_hours_days() {
        assert_eq!(
            format_duration(61_000, DurationFormatOptions::default()),
            "1m 1s"
        );
        assert_eq!(
            format_duration(3_661_000, DurationFormatOptions::default()),
            "1h 1m 1s"
        );
        assert_eq!(
            format_duration(176_400_000, DurationFormatOptions::default()),
            "2d 1h 0m"
        );
    }

    #[test]
    fn format_duration_hide_trailing_zeros() {
        assert_eq!(
            format_duration(
                3_600_000,
                DurationFormatOptions {
                    hide_trailing_zeros: true,
                    most_significant_only: false,
                }
            ),
            "1h"
        );
        assert_eq!(
            format_duration(
                120_000,
                DurationFormatOptions {
                    hide_trailing_zeros: true,
                    most_significant_only: false,
                }
            ),
            "2m"
        );
    }

    #[test]
    fn format_duration_most_significant_only() {
        assert_eq!(
            format_duration(
                3_661_000,
                DurationFormatOptions {
                    hide_trailing_zeros: false,
                    most_significant_only: true,
                }
            ),
            "1h"
        );
    }

    #[test]
    fn format_duration_rounding_carries_into_next_minute() {
        assert_eq!(
            format_duration(119_500, DurationFormatOptions::default()),
            "2m 0s"
        );
    }

    #[test]
    fn shell_time_display_hidden_when_both_absent() {
        assert_eq!(project_shell_time_display(None, None), None);
    }

    #[test]
    fn shell_time_display_timeout_only() {
        let display = project_shell_time_display(None, Some(120_000)).unwrap();
        assert_eq!(display.text, "(timeout 2m)");
    }

    #[test]
    fn shell_time_display_elapsed_only() {
        let display = project_shell_time_display(Some(12), None).unwrap();
        assert_eq!(display.text, "(12s)");
    }

    #[test]
    fn shell_time_display_elapsed_and_timeout() {
        let display = project_shell_time_display(Some(12), Some(120_000)).unwrap();
        assert_eq!(display.text, "(12s \u{00b7} timeout 2m)");
    }
}
