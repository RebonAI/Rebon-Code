use std::time::{Duration, SystemTime};

use rebon_types::format_system_time_iso_ms;

use crate::session::ultraplan_run::UltraplanPhase;
use crate::tui::app::AppState;

pub(crate) fn apply_context_reset_to_tui(app: &mut AppState, plan: Option<String>) {
    app.reset_transcript_views();
    app.background_agent_tool_tasks.clear();
    app.remote_background_tasks.clear();
    app.live_agent_tool_activity.clear();
    app.live_agent_tool_activity_revision = app.live_agent_tool_activity_revision.wrapping_add(1);
    app.follow_transcript_tail = true;
    if let Some(status) = app.ultraplan_status.as_mut() {
        status.phase = UltraplanPhase::Executing;
        if let Some(context) = status.context.as_mut() {
            context.phase = "executing".into();
        }
    }
    if let Some(plan) = plan {
        inject_plan_message(app, &plan);
    }
}

/// Inject the approved plan as the fresh user prompt after plan-mode reset.
pub(crate) fn inject_plan_message(app: &mut AppState, plan: &str) {
    inject_plan_user_message(
        app,
        format!("Implement the following plan:\n\n{plan}"),
        plan,
    );
}

pub(crate) fn inject_plan_card(app: &mut AppState, plan: &str) {
    inject_plan_user_message(app, format!("Plan:\n\n{plan}"), plan);
}

fn inject_plan_user_message(app: &mut AppState, text: String, plan: &str) {
    let uuid = format!("u-plan-{}", rebon_types::wall_clock_ms_u128());
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_tui::Message::User(rebon_tui::UserMessage {
            uuid,
            timestamp,
            message: rebon_tui::UserMessageInner {
                role: rebon_tui::UserRole::User,
                content: vec![rebon_tui::UserContentBlock::Text(
                    rebon_tui::UserTextBlock { text },
                )],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: Some(true),
            image_paste_ids: None,
            plan_content: Some(plan.to_string()),
        })),
    );
}

/// Inject a system message into the TUI transcript.
pub(crate) fn inject_system_message(app: &mut AppState, subtype: &str, content: &str) {
    let uuid = format!("s-{subtype}-{}", rebon_types::wall_clock_ms_u128());
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    let level = match subtype {
        "warning" => rebon_tui::SystemLevel::Warning,
        "error" => rebon_tui::SystemLevel::Error,
        _ => rebon_tui::SystemLevel::Info,
    };
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid,
            timestamp,
            subtype: subtype.into(),
            content: Some(content.to_string()),
            level: Some(level),
            is_meta: None,
        })),
    );
}

pub(crate) fn inject_worked_message(app: &mut AppState, elapsed: Duration) {
    inject_system_message(app, "turn_duration", &worked_summary(elapsed));
}

fn worked_summary(elapsed: Duration) -> String {
    let total_seconds = elapsed.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("Worked {hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("Worked {minutes}m {seconds:02}s")
    } else {
        format!("Worked {seconds}s")
    }
}

/// Inject local command feedback using the local-command rendering path.
///
/// This keeps the feedback local to the TUI transcript while rendering through
/// the user-prompt gutter (`❯`) instead of the generic system-text path.
pub(crate) fn inject_local_command_feedback(app: &mut AppState, label: &str, content: &str) {
    let uuid = format!("s-{label}-{}", rebon_types::wall_clock_ms_u128());
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid,
            timestamp,
            subtype: "local_command".into(),
            content: Some(content.to_string()),
            level: Some(rebon_tui::SystemLevel::Info),
            is_meta: None,
        })),
    );
}

pub(crate) fn inject_local_command_feedback_with_command(
    app: &mut AppState,
    label: &str,
    command_line: &str,
    content: &str,
) {
    let text = format_command_feedback_text(command_line, content);
    inject_local_command_feedback(app, label, &text);
}

pub(super) fn inject_fast_command_result(
    app: &mut AppState,
    result: Result<String, String>,
    is_update: bool,
) {
    match result {
        Ok(_) if is_update && app.refresh_empty_startup_banner() => {}
        Ok(text) | Err(text) => {
            inject_local_command_feedback(app, "fast", &text);
            app.follow_transcript_tail = true;
        }
    }
}

pub(super) fn inject_provider_switch(app: &mut AppState, provider: &str, model: &str) {
    if app.refresh_empty_startup_banner() {
        return;
    }
    inject_system_message(
        app,
        "provider_switch",
        &format!("Switched to provider {provider}\nUsing model {model}"),
    );
    app.follow_transcript_tail = true;
}

pub(super) fn format_command_feedback_text(command_line: &str, content: &str) -> String {
    let content = content.trim_matches(['\r', '\n']);
    let mut text = format!("{command_line}\n⎿");
    let mut lines = content.lines();
    if let Some(first) = lines.next() {
        if !first.is_empty() {
            text.push(' ');
            text.push_str(first);
        }
        for line in lines {
            text.push('\n');
            text.push_str("  ");
            text.push_str(line);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::tui::app::{AppState, StoredTranscriptView};

    use super::{
        apply_context_reset_to_tui, inject_local_command_feedback,
        inject_local_command_feedback_with_command, inject_plan_card, inject_plan_message,
        inject_system_message, inject_worked_message, worked_summary,
    };

    fn warm_active_measure_cache(app: &mut AppState, text: &str) {
        inject_system_message(app, "cache", text);
        let area = ratatui::layout::Rect::new(0, 0, 40, 8);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        rebon_tui::render_transcript_cached_with_running_hints(
            &app.rebon_tui,
            area,
            &mut buffer,
            &rebon_tui::RenderTheme::default(),
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            &mut app.transcript_measure_cache,
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        assert!(!app.transcript_measure_cache.is_empty());
    }

    #[test]
    fn context_reset_replaces_foreground_and_saved_views_with_fresh_main() {
        let mut app = AppState::new();
        warm_active_measure_cache(&mut app, "main cache");
        let main = app.replace_active_transcript_view(StoredTranscriptView::default());
        app.main_agent_view = Some(main);
        app.foregrounded_task_id = Some("agent-1".into());
        warm_active_measure_cache(&mut app, "agent cache");
        app.local_agent_views
            .insert("agent-2".into(), StoredTranscriptView::default());

        apply_context_reset_to_tui(&mut app, Some("execute plan".into()));

        assert_eq!(app.foregrounded_task_id, None);
        assert!(app.main_agent_view.is_none());
        assert!(app.local_agent_views.is_empty());
        assert!(app.transcript_measure_cache.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        let rebon_tui::Message::User(user) = &app.rebon_tui.transcript.rows()[0] else {
            panic!("expected plan user message");
        };
        assert_eq!(user.plan_content.as_deref(), Some("execute plan"));
    }

    #[test]
    fn inject_plan_message_marks_plan_content() {
        let mut app = AppState::new();
        inject_plan_message(&mut app, "do the thing");

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::User(user) = &rows[0] else {
            panic!("expected user message");
        };
        assert_eq!(user.plan_content.as_deref(), Some("do the thing"));
        assert_eq!(user.is_visible_in_transcript_only, Some(true));
        assert!(user.uuid.starts_with("u-plan-"));
    }

    #[test]
    fn inject_plan_card_marks_discussion_plan_without_execution_prompt() {
        let mut app = AppState::new();
        inject_plan_card(&mut app, "discuss this");

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::User(user) = &rows[0] else {
            panic!("expected user message");
        };
        assert_eq!(user.plan_content.as_deref(), Some("discuss this"));
        assert_eq!(user.is_visible_in_transcript_only, Some(true));
        let text = match &user.message.content[0] {
            rebon_tui::UserContentBlock::Text(text) => &text.text,
            other => panic!("expected text block, got {other:?}"),
        };
        assert_eq!(text, "Plan:\n\ndiscuss this");
    }

    #[test]
    fn provider_switch_refreshes_empty_banner_without_feedback() {
        for ui_mode in [
            crate::ui_config::UiMode::Inline,
            crate::ui_config::UiMode::Screen,
        ] {
            let mut app = AppState::new();
            app.ui_mode = ui_mode;
            super::inject_provider_switch(&mut app, "first", "model-a");
            super::inject_provider_switch(&mut app, "second", "model-b");
            assert!(app.pending_inline_banner_refresh);
            assert!(app.rebon_tui.transcript.is_empty());
        }
    }

    #[test]
    fn provider_switch_after_content_commits_only_the_two_line_notice() {
        let mut app = AppState::new();
        inject_system_message(&mut app, "local_command", "existing content");
        super::inject_provider_switch(&mut app, "本地 gateway", "org/model-v2");
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 2);
        let rebon_tui::Message::System(system) = &rows[1] else {
            panic!("expected system message");
        };
        assert_eq!(system.subtype, "provider_switch");
        assert_eq!(
            system.content.as_deref(),
            Some("Switched to provider 本地 gateway\nUsing model org/model-v2")
        );
        assert!(!app.pending_inline_banner_refresh);
    }

    #[test]
    fn provider_switch_is_visible_in_compact_tui_transcript() {
        let mut app = AppState::new();
        inject_system_message(&mut app, "local_command", "existing content");
        super::inject_provider_switch(&mut app, "gateway", "org/model-v2");
        let area = ratatui::layout::Rect::new(0, 0, 80, 12);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        rebon_tui::render_transcript_cached_with_running_hints(
            &app.rebon_tui,
            area,
            &mut buffer,
            &rebon_tui::RenderTheme::plain(),
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            &mut rebon_tui::TranscriptMeasureCache::new(),
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("● Switched to provider gateway"), "{text}");
        assert!(text.contains("⎿ Using model org/model-v2"), "{text}");
    }

    #[test]
    fn fast_success_only_refreshes_an_empty_banner() {
        let mut app = AppState::new();
        super::inject_fast_command_result(&mut app, Ok("enabled".into()), true);
        super::inject_fast_command_result(&mut app, Ok("disabled".into()), true);
        assert!(app.pending_inline_banner_refresh);
        assert!(app.rebon_tui.transcript.is_empty());
    }

    #[test]
    fn fast_status_errors_and_existing_history_keep_feedback() {
        for (result, is_update, has_history) in [
            (Ok("status".to_string()), false, false),
            (Err("persist warning".to_string()), true, false),
            (Err("unavailable".to_string()), true, false),
            (Ok("enabled".to_string()), true, true),
        ] {
            let mut app = AppState::new();
            if has_history {
                inject_system_message(&mut app, "local_command", "history");
            }
            let expected = result.clone().unwrap_or_else(|text| text);
            super::inject_fast_command_result(&mut app, result, is_update);
            let rows = app.rebon_tui.transcript.rows();
            assert_eq!(rows.len(), 1 + usize::from(has_history));
            let rebon_tui::Message::System(system) = rows.last().unwrap() else {
                panic!("expected system message");
            };
            assert_eq!(system.content.as_deref(), Some(expected.as_str()));
            assert!(!app.pending_inline_banner_refresh);
        }
    }

    #[test]
    fn inject_system_message_uses_requested_subtype() {
        let mut app = AppState::new();
        inject_system_message(&mut app, "notice", "hello");

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected system message");
        };
        assert_eq!(system.subtype, "notice");
        assert_eq!(system.content.as_deref(), Some("hello"));
        assert_eq!(system.level, Some(rebon_tui::SystemLevel::Info));
    }

    #[test]
    fn inject_system_message_preserves_standard_warning_and_error_levels() {
        for (subtype, expected) in [
            ("warning", rebon_tui::SystemLevel::Warning),
            ("error", rebon_tui::SystemLevel::Error),
        ] {
            let mut app = AppState::new();
            inject_system_message(&mut app, subtype, "message");

            let rebon_tui::Message::System(system) = &app.rebon_tui.transcript.rows()[0] else {
                panic!("expected system message");
            };
            assert_eq!(system.level, Some(expected));
        }
    }

    #[test]
    fn worked_summary_formats_seconds_minutes_and_hours() {
        assert_eq!(worked_summary(Duration::ZERO), "Worked 0s");
        assert_eq!(worked_summary(Duration::from_secs(59)), "Worked 59s");
        assert_eq!(worked_summary(Duration::from_secs(61)), "Worked 1m 01s");
        assert_eq!(
            worked_summary(Duration::from_secs(3_723)),
            "Worked 1h 02m 03s"
        );
    }

    #[test]
    fn inject_worked_message_is_a_tui_only_system_row() {
        let mut app = AppState::new();
        inject_worked_message(&mut app, Duration::from_secs(62));

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected system message");
        };
        assert_eq!(system.subtype, "turn_duration");
        assert_eq!(system.content.as_deref(), Some("Worked 1m 02s"));
        assert_eq!(system.level, Some(rebon_tui::SystemLevel::Info));

        let area = ratatui::layout::Rect::new(0, 0, 40, 3);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        let mut cache = rebon_tui::TranscriptMeasureCache::new();
        rebon_tui::render_transcript_cached_with_running_hints(
            &app.rebon_tui,
            area,
            &mut buffer,
            &rebon_tui::RenderTheme::plain(),
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        let rendered = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter_map(|position| buffer.cell(position))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("● Worked 1m 02s"), "{rendered}");
    }

    #[test]
    fn inject_local_command_feedback_uses_local_command_subtype() {
        let mut app = AppState::new();
        inject_local_command_feedback(&mut app, "status", "ready");

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected system message");
        };
        assert_eq!(system.subtype, "local_command");
        assert!(system.uuid.starts_with("s-status-"));
        assert_eq!(system.content.as_deref(), Some("ready"));
    }

    #[test]
    fn inject_local_command_feedback_with_command_adds_requested_shape() {
        let mut app = AppState::new();
        inject_local_command_feedback_with_command(
            &mut app,
            "goal",
            "/goal ship it",
            "goal set: ship it",
        );

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected system message");
        };
        assert_eq!(system.subtype, "local_command");
        assert!(system.uuid.starts_with("s-goal-"));
        assert_eq!(
            system.content.as_deref(),
            Some("/goal ship it\n⎿ goal set: ship it")
        );
    }

    #[test]
    fn inject_local_command_feedback_with_command_indents_multiline_output() {
        let mut app = AppState::new();
        inject_local_command_feedback_with_command(
            &mut app,
            "shell",
            "!pwd",
            "\nPath\n----\nC:\\projects\\example\n",
        );

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected system message");
        };
        assert_eq!(system.subtype, "local_command");
        assert!(system.uuid.starts_with("s-shell-"));
        assert_eq!(
            system.content.as_deref(),
            Some("!pwd\n⎿ Path\n  ----\n  C:\\projects\\example")
        );
    }
}
