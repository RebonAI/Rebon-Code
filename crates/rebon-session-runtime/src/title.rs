//! A session's title: where it is kept, and the sentence `MarkSessionComplete`
//! turns it into.
//!
//! Both hosts that apply a hook's `SetSessionTitle` -- the terminal and a
//! worker -- come through here; the update that tells the viewers is the
//! caller's, because the two publish from different threads.

use std::time::SystemTime;

use rebon_session::format_system_time_iso_ms;
use rebon_types::SessionUpdate;

const COMPLETED_TITLE_PREFIX: &str = "✓ ";
const COMPLETED_TITLE_FALLBACK: &str = "Completed session";

/// Record the title where the session keeps it: the in-process record and
/// the sidecar on disk. Both hosts that apply a hook's `SetSessionTitle` —
/// the TUI and a worker — come through here; the update that tells the
/// viewers is the caller's, because the two publish from different threads.
pub fn persist_session_title(session: &crate::EngineSession, title: &str) {
    session
        .server_state
        .set_session_title(&session.session_id, title.to_string());
    if let Err(err) = rebon_session::save_session_title(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
        title,
    ) {
        tracing::warn!(
            error = %err,
            session_id = %session.session_id,
            "rebon-cli: failed to persist session title"
        );
    }
}

/// The update that carries a new title to whoever is watching the session.
pub fn session_title_update(title: String) -> SessionUpdate {
    SessionUpdate::SessionInfoUpdate {
        title: Some(title),
        updated_at: Some(format_system_time_iso_ms(SystemTime::now())),
        meta: None,
    }
}

/// What `MarkSessionComplete` turns the session's title into, read from the
/// session record — for a host with no view of its own to read it from.
pub fn completed_session_title_for(session: &crate::EngineSession) -> String {
    let current_title = session
        .server_state
        .get_session(&session.session_id)
        .and_then(|record| record.title);
    completed_session_title(current_title.as_deref().unwrap_or(COMPLETED_TITLE_FALLBACK))
}

pub fn completed_session_title(title: &str) -> String {
    let title = title.trim();
    let title = strip_completed_title_marker(title).unwrap_or(title);
    if title.is_empty() {
        "✓".to_string()
    } else {
        format!("{COMPLETED_TITLE_PREFIX}{title}")
    }
}

fn strip_completed_title_marker(title: &str) -> Option<&str> {
    let rest = title
        .strip_prefix('✓')
        .or_else(|| title.strip_prefix('☑'))
        .or_else(|| title.strip_prefix('✅'))?;
    Some(rest.strip_prefix('\u{fe0f}').unwrap_or(rest).trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_session_title_prefixes_unmarked_title() {
        assert_eq!(completed_session_title("Build feature"), "✓ Build feature");
    }

    #[test]
    fn completed_session_title_normalizes_existing_marker() {
        assert_eq!(
            completed_session_title("✓ Build feature"),
            "✓ Build feature"
        );
        assert_eq!(
            completed_session_title("☑ Build feature"),
            "✓ Build feature"
        );
        assert_eq!(
            completed_session_title("☑️ Build feature"),
            "✓ Build feature"
        );
        assert_eq!(
            completed_session_title("✅ Build feature"),
            "✓ Build feature"
        );
    }

    #[test]
    fn completed_session_title_handles_marker_only_title() {
        assert_eq!(completed_session_title("☑️"), "✓");
    }
}
