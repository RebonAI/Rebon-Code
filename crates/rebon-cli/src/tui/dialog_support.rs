//! Shared helper functions for dialog hosts.
//!
//! The display-width truncation these hosts use lives in `rebon-tui`
//! alongside the painter that also needs it — one implementation, so a
//! change to how a line is cut shows up everywhere it is cut. It is
//! re-exported here because the dialog hosts name it through this
//! module.

pub use rebon_tui::dialog_view::truncate_to_width;

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Read up to `max_lines` starting at `start_line` (0-based). Returns
/// an empty vector when the file cannot be opened; a line that fails
/// to decode as UTF-8 is skipped.
pub fn read_preview_lines(path: &Path, start_line: usize, max_lines: usize) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    reader
        .lines()
        .skip(start_line)
        .take(max_lines)
        .filter_map(Result::ok)
        .collect()
}

/// Compact relative-age formatter used by the history picker.
pub fn format_relative_age(timestamp_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(timestamp_ms);
    let delta_secs = now_ms.saturating_sub(timestamp_ms) / 1_000;
    if delta_secs < 60 {
        return format!("{delta_secs}s");
    }
    let delta_mins = delta_secs / 60;
    if delta_mins < 60 {
        return format!("{delta_mins}m");
    }
    let delta_hours = delta_mins / 60;
    if delta_hours < 24 {
        return format!("{delta_hours}h");
    }
    let delta_days = delta_hours / 24;
    format!("{delta_days}d")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_relative_age_uses_short_suffixes() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        assert!(format_relative_age(now_ms.saturating_sub(30_000)).ends_with('s'));
        assert!(format_relative_age(now_ms.saturating_sub(5 * 60_000)).ends_with('m'));
        assert!(format_relative_age(now_ms.saturating_sub(3 * 60 * 60_000)).ends_with('h'));
    }
}
