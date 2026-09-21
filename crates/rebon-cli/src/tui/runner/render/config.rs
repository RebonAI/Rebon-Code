use super::*;

use rebon_dialog::settings_dialog::SettingsDialogState;

pub(in crate::tui::runner) fn current_config_options(
    session: &TuiEngineSession,
) -> Vec<rebon_types::ConfigOption> {
    let mut options = session
        .engine_half
        .handler
        .config_options_for_session(&session.session_id);
    ensure_ui_mode_config_option(&mut options, session.ui_mode, session.configured_ui_mode);
    ensure_math_rendering_config_option(&mut options, session.math_rendering_mode);
    options
}

pub(in crate::tui::runner) fn ensure_ui_mode_config_option(
    options: &mut Vec<rebon_types::ConfigOption>,
    runtime_mode: crate::ui_config::UiMode,
    configured_mode: crate::ui_config::UiMode,
) {
    if options.iter().any(|option| option.id == "uiMode") {
        return;
    }
    let description = if runtime_mode == configured_mode {
        "Choose screen mode (fullscreen terminal UI) or inline mode (prompt-adjacent UI)"
            .to_string()
    } else {
        format!(
            "Saved as {configured_mode} for the next launch. The current session remains in {runtime_mode} mode until rebon restarts."
        )
    };
    options.insert(
        0,
        rebon_types::ConfigOption {
            id: "uiMode".to_string(),
            name: "UI mode".to_string(),
            description: Some(description),
            category: Some("ui".to_string()),
            option_type: rebon_types::ConfigOptionType::Select,
            current_value: configured_mode.to_string(),
            options: vec![
                rebon_types::ConfigOptionValue {
                    value: "screen".to_string(),
                    name: "Screen".to_string(),
                    description: Some("Fullscreen terminal UI".to_string()),
                },
                rebon_types::ConfigOptionValue {
                    value: "inline".to_string(),
                    name: "Inline".to_string(),
                    description: Some("Prompt-adjacent inline UI".to_string()),
                },
            ],
        },
    );
}

pub(in crate::tui::runner) fn ensure_math_rendering_config_option(
    options: &mut Vec<rebon_types::ConfigOption>,
    current_mode: crate::rebon_config::MathRenderingMode,
) {
    if options.iter().any(|option| option.id == "mathRendering") {
        return;
    }
    let insert_at = options
        .iter()
        .position(|option| option.id == "uiMode")
        .map(|index| index + 1)
        .unwrap_or(0);
    options.insert(
        insert_at,
        rebon_types::ConfigOption {
            id: "mathRendering".to_string(),
            name: "LaTeX formulas".to_string(),
            description: Some("Choose how formulas are displayed in the terminal UI".to_string()),
            category: Some("ui".to_string()),
            option_type: rebon_types::ConfigOptionType::Select,
            current_value: current_mode.to_string(),
            options: vec![
                rebon_types::ConfigOptionValue {
                    value: "off".to_string(),
                    name: "Off".to_string(),
                    description: Some("Show formula source without conversion".to_string()),
                },
                rebon_types::ConfigOptionValue {
                    value: "unicode".to_string(),
                    name: "Unicode".to_string(),
                    description: Some(
                        "Render formulas offline with portable Unicode half-blocks".to_string(),
                    ),
                },
                rebon_types::ConfigOptionValue {
                    value: "graphics-auto".to_string(),
                    name: "Graphics (auto)".to_string(),
                    description: Some(
                        "Use terminal graphics when supported, with automatic fallback".to_string(),
                    ),
                },
            ],
        },
    );
}

pub(in crate::tui::runner) fn build_settings_dialog_view(
    app: &AppState,
    session: &TuiEngineSession,
) -> SettingsDialogView {
    let session_record = session
        .engine_half
        .handler
        .state()
        .get_session(&session.session_id);
    let usage = app.usage();
    SettingsDialogView {
        version: env!("CARGO_PKG_VERSION").to_string(),
        session_id: session.session_id.clone(),
        session_title: session_record
            .as_ref()
            .and_then(|record| record.title.clone()),
        cwd: session.cwd.clone(),
        provider_name: session.model.provider_name.clone(),
        model_name: session.model.name.clone(),
        permission_mode: session_record
            .map(|record| record.permission_mode)
            .unwrap_or_else(|| "default".to_string()),
        config_options: current_config_options(session),
        context_usage: session.model.prune_level.budget.usage_snapshot(),
        context_window: session.model.prune_level.budget.context_window(),
        last_turn_usage: usage.last_turn,
        total_usage: usage.total,
    }
}

/// Hand the open settings panel this frame's projection of live session
/// state. Per frame rather than per action, so a value changed
/// elsewhere shows up while the panel is open; the projection was built
/// once per frame already, this only moved where.
///
/// Which tab's rows to build is the panel's answer, not ours: it owns
/// the tab, so it is asked before the rows are made.
pub(in crate::tui::runner) fn refresh_settings_projection(
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
) {
    // Without a session there is nothing to project, and the panel keeps
    // whatever it last had.
    let Some(session) =
        session.filter(|_| app.dialogs.top_id() == Some(rebon_ui_seat::ids::dialog::SETTINGS))
    else {
        return;
    };
    let Some(active_tab) = app
        .dialogs
        .top_as::<SettingsDialogState>()
        .map(SettingsDialogState::active_tab)
    else {
        return;
    };
    let view = build_settings_dialog_view(app, session);
    let projection = crate::session::settings_rows::projection(&view, active_tab);
    if let Some(dialog) = app.dialogs.top_as_mut::<SettingsDialogState>() {
        dialog.set_projection(projection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_view_projects_live_usage_state() {
        let mut app = AppState::new();
        app.usage_ledger = std::sync::Arc::new(std::sync::Mutex::new(
            crate::session::usage::UsageLedger::seeded(
                0,
                rebon_types::Usage {
                    output_tokens: 567,
                    ..Default::default()
                },
                rebon_types::Usage {
                    input_tokens: 1_234,
                    ..Default::default()
                },
            ),
        ));
        let session = crate::tui::runner::test_support::make_test_tui_session();
        session.model.prune_level.budget.set_context_window(200_000);
        session.model.prune_level.budget.report_usage(50_000);

        let view = build_settings_dialog_view(&app, &session);

        assert_eq!(view.context_usage.tokens, 50_000);
        assert_eq!(
            view.context_usage.source,
            rebon_api::ContextUsageSource::Server
        );
        assert_eq!(view.context_window, 200_000);
        assert_eq!(view.last_turn_usage.input_tokens, 1_234);
        assert_eq!(view.total_usage.output_tokens, 567);
    }

    #[test]
    fn ui_mode_dialog_option_shows_saved_value_pending_restart() {
        let mut options = Vec::new();

        ensure_ui_mode_config_option(
            &mut options,
            crate::ui_config::UiMode::Screen,
            crate::ui_config::UiMode::Inline,
        );

        assert_eq!(options[0].current_value, "inline");
        let description = options[0].description.as_deref().unwrap_or_default();
        assert!(description.contains("next launch"));
        assert!(description.contains("current session remains in screen mode"));
    }

    #[test]
    fn math_rendering_dialog_option_has_all_modes_and_current_value() {
        let mut options = Vec::new();
        ensure_ui_mode_config_option(
            &mut options,
            crate::ui_config::UiMode::Screen,
            crate::ui_config::UiMode::Screen,
        );
        ensure_math_rendering_config_option(
            &mut options,
            crate::rebon_config::MathRenderingMode::GraphicsAuto,
        );

        assert_eq!(options[0].id, "uiMode");
        let math = &options[1];
        assert_eq!(math.id, "mathRendering");
        assert_eq!(math.name, "LaTeX formulas");
        assert_eq!(math.category.as_deref(), Some("ui"));
        assert_eq!(math.option_type, rebon_types::ConfigOptionType::Select);
        assert_eq!(math.current_value, "graphics-auto");
        assert_eq!(
            math.options
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            vec!["off", "unicode", "graphics-auto"]
        );
    }

    #[test]
    fn math_rendering_dialog_option_does_not_replace_engine_option() {
        let mut options = vec![rebon_types::ConfigOption {
            id: "mathRendering".into(),
            name: "Engine math".into(),
            description: None,
            category: None,
            option_type: rebon_types::ConfigOptionType::Select,
            current_value: "unicode".into(),
            options: Vec::new(),
        }];

        ensure_math_rendering_config_option(
            &mut options,
            crate::rebon_config::MathRenderingMode::Off,
        );

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].name, "Engine math");
        assert_eq!(options[0].current_value, "unicode");
    }
}
