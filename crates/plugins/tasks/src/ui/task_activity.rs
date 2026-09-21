//! Shared formatting for live task activity previews shown in TUI surfaces.

use crate::runtime::TaskSnapshot;

/// Compact a snapshot progress payload for one-line previews.
///
/// The registry writes short activity strings such as `reading src/lib.rs`,
/// `running cargo test`, or the latest assistant text into `last_progress`.
/// This helper trims multiline payloads to the newest non-empty line and caps
/// the preview so callers stay visually tidy and consistent.
pub fn format_snapshot_activity(snapshot: &TaskSnapshot) -> Option<String> {
    let raw = snapshot.last_progress.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let line = raw
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(rebon_types::truncate_chars(line, 160))
}
