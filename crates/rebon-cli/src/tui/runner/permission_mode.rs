use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

pub(super) fn record_background_permission_mode_acceptance(
    mode: rebon_permissions::PermissionMode,
) {
    #[cfg(not(test))]
    {
        if crate::rebon_config::background_permission_mode_requires_interactive_acceptance(mode) {
            if let Err(err) = crate::rebon_config::mark_background_permission_mode_accepted(mode) {
                tracing::debug!(
                    mode = mode.as_wire(),
                    error = %err,
                    "failed to persist interactive background permission-mode acceptance"
                );
            }
        }
    }

    #[cfg(test)]
    let _ = mode;
}

/// Apply a Shift+Tab permission-mode cycle with full side effects:
///
/// 1. Flip `app.permission_mode` (local UI state used by the footer
///    indicator).
/// 2. Sync the new mode into the ACP session record via
///    `apply_config_option_local`. On the engine side,
///    [`rebon_acp::ServerState::set_permission_mode`] now also
///    runs [`rebon_acp::apply_plan_mode_transition_flags`] so the
///    session's `SessionAttachmentState` picks up
///    `needs_plan_mode_exit_attachment` / `has_exited_plan_mode`.
///    The plan-mode plugin's attachment producer, registered on the
///    engine's attachment seat, then injects the appropriate
///    plan-mode / plan-mode-exit message between the current and next
///    tool round - the stream keeps flowing and the model sees the
///    mode change as a real user turn.
/// 3. When a turn is currently streaming, emit a debug trace for the
///    local operator without adding visible transcript clutter.
pub(super) fn apply_cycle_permission_mode(
    app: &mut AppState,
    session: &TuiEngineSession,
    stream_in_flight: bool,
) {
    let previous = app.permission_mode;
    app.cycle_permission_mode();
    let next = app.permission_mode;
    if previous == next {
        return;
    }
    record_background_permission_mode_acceptance(next);

    let wire = next.as_wire();
    let _ = session.engine_half.handler.apply_config_option_local(
        &session.session_id,
        "permissions",
        wire,
    );
    if !next.is_session_scoped() {
        if let Err(err) = crate::rebon_config::save_default_permission_mode(next) {
            tracing::warn!(error = %err, mode = wire, "failed to persist default permission mode");
        }
    }

    if stream_in_flight {
        let label = rebon_permissions::permission_mode_title(next);
        tracing::debug!(
            mode = next.as_wire(),
            "rebon-cli: permission mode switched to {label} mid-turn"
        );
    }
}

/// Shift+Tab before the session exists: the app's mode
/// cycles and the default is persisted as usual; the session's own cell
/// gets the result when the session is installed
/// (`push_permission_mode_into_session`).
pub(super) fn cycle_permission_mode_before_session(app: &mut AppState) -> bool {
    let previous = app.permission_mode;
    app.cycle_permission_mode();
    let next = app.permission_mode;
    if previous == next {
        return false;
    }
    record_background_permission_mode_acceptance(next);
    if !next.is_session_scoped() {
        if let Err(err) = crate::rebon_config::save_default_permission_mode(next) {
            tracing::warn!(error = %err, mode = next.as_wire(), "failed to persist default permission mode");
        }
    }
    true
}

/// Make a freshly installed session agree with a mode the app already
/// shows — the one Shift+Tab reached before the session existed.
pub(super) fn push_permission_mode_into_session(app: &mut AppState, session: &TuiEngineSession) {
    let mode = app.permission_mode;
    app.set_permission_mode(mode);
    let _ = session.engine_half.handler.apply_config_option_local(
        &session.session_id,
        "permissions",
        mode.as_wire(),
    );
}

/// Commit a system message to the transcript documenting a mid-turn
/// permission-mode transition. Retained for unit test coverage of the
/// message shape; the live path now uses a debug trace instead to
/// avoid cluttering the transcript on accidental Shift+Tab presses.
#[cfg(test)]
fn commit_permission_mode_change_message(
    app: &mut AppState,
    session_id: &str,
    next: rebon_permissions::PermissionMode,
) {
    let wire = next.as_wire();
    let label = rebon_permissions::permission_mode_title(next);
    let subtype = if matches!(next, rebon_permissions::PermissionMode::Plan) {
        "permission_mode_plan"
    } else {
        "permission_mode_change"
    };
    let content = format!(
        "User switched permission mode to {label} ({wire}) mid-turn. Honor the new mode from the next tool call onward."
    );
    let uuid = format!("s-mode-{session_id}-{}", rebon_types::wall_clock_ms_u128());
    let timestamp = rebon_types::format_system_time_iso_ms(std::time::SystemTime::now());
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid,
            timestamp,
            subtype: subtype.into(),
            content: Some(content),
            level: Some(rebon_tui::SystemLevel::Info),
            is_meta: Some(true),
        })),
    );
}

#[cfg(test)]
mod tests {
    use crate::tui::app::AppState;

    use super::commit_permission_mode_change_message;

    #[test]
    fn commit_permission_mode_change_inserts_plan_system_message() {
        use rebon_permissions::PermissionMode;
        let mut app = AppState::new();
        commit_permission_mode_change_message(&mut app, "sess-1", PermissionMode::Plan);
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let row = rows.last().expect("transcript has one row");
        let rebon_tui::Message::System(sys) = row else {
            panic!("expected system message, got {row:?}");
        };
        assert_eq!(sys.subtype, "permission_mode_plan");
        assert_eq!(sys.level, Some(rebon_tui::SystemLevel::Info));
        assert_eq!(sys.is_meta, Some(true));
        let content = sys.content.as_deref().expect("has content");
        assert!(content.contains("Plan Mode"), "{content}");
        assert!(content.contains("plan"), "{content}");
        assert!(content.contains("next tool call"), "{content}");
        assert!(sys.uuid.starts_with("s-mode-sess-1-"));
    }

    #[test]
    fn commit_permission_mode_change_uses_generic_subtype_for_non_plan() {
        use rebon_permissions::PermissionMode;
        let mut app = AppState::new();
        commit_permission_mode_change_message(&mut app, "sess-2", PermissionMode::AcceptEdits);
        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(sys) = rows.last().unwrap() else {
            panic!("expected system message");
        };
        assert_eq!(sys.subtype, "permission_mode_change");
        let content = sys.content.as_deref().unwrap();
        assert!(content.contains("Accept edits"));
        assert!(content.contains("acceptEdits"));
    }
}
