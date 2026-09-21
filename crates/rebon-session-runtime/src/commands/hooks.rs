//! `/hooks`: the hooks configured for this session, and where each
//! one is configured.
//!
//! Both the textual answer and the browser's rows are collected here,
//! because both need the same three things a reducer cannot reach: the
//! tool list, the agent registry, and the settings files on disk. The
//! panel itself is `rebon_dialog::hooks_dialog`.

use std::path::Path;

use rebon_dialog::hooks_dialog::{HookEventRow, HooksDialogInput};
use rebon_hooks::{
    build_hook_event_metadata, display_text, load_all_editable, matcher_metadata_for_event,
    parse_hook_event, IndividualHookConfig, MetadataInputs, HOOK_EVENTS,
};

use crate::EngineSession;
use rebon_slash_commands::strip_command_prefix;

/// Parsed payload of a `/hooks` slash command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HooksCommand {
    /// Optional event name to show focused metadata for.
    pub event_name: Option<String>,
}

/// Recognize `/hooks`, optionally followed by a hook event name.
pub fn parse_hooks_command(text: &str) -> Option<HooksCommand> {
    let rest = strip_command_prefix(text.trim_end(), "hooks")?;
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let event_name = rest.trim();
    Some(HooksCommand {
        event_name: if event_name.is_empty() {
            None
        } else {
            Some(event_name.to_string())
        },
    })
}

pub fn execute_hooks_command(session: &EngineSession, cmd: HooksCommand) -> String {
    let (configured_hooks, settings_warnings) = load_settings_hooks(Path::new(&session.cwd));
    format_hooks_command(
        session.engine_half.engine.tool_names(),
        active_agent_types(session),
        cmd,
        &configured_hooks,
        &settings_warnings,
    )
}

/// Read the settings-backed hooks for `cwd`, turning a load failure into
/// a warning rather than an error: a broken settings file must not hide
/// the events that are fine.
fn load_settings_hooks(cwd: &Path) -> (Vec<IndividualHookConfig>, Vec<String>) {
    match load_all_editable(&hook_settings_paths(cwd)) {
        Ok(loaded) => (
            loaded.hooks,
            loaded
                .warnings
                .into_iter()
                .map(|warning| format!("{}: {}", warning.path.display(), warning.reason))
                .collect(),
        ),
        Err(error) => (Vec::new(), vec![format!("settings load failed: {error}")]),
    }
}

fn active_agent_types(session: &EngineSession) -> Vec<String> {
    session
        .engine_half
        .agent_registry
        .active()
        .map(|agent| agent.agent_type.clone())
        .collect()
}

/// Collect what the `/hooks` browser is opened with: every event with its
/// metadata and the hooks configured for it, plus any settings warnings.
pub fn collect_hooks_dialog_input(
    session: &EngineSession,
    initial_event: Option<&str>,
) -> HooksDialogInput {
    let metadata = build_hook_event_metadata(&MetadataInputs {
        tool_names: session.engine_half.engine.tool_names(),
        agent_types: active_agent_types(session),
        elicitation_servers: Vec::new(),
    });
    let (configured, warnings) = load_settings_hooks(Path::new(&session.cwd));
    let events = HOOK_EVENTS
        .iter()
        .map(|event| {
            let event_metadata = metadata.get(event);
            HookEventRow {
                name: event.name().to_string(),
                summary: event_metadata
                    .map(|item| item.summary.clone())
                    .unwrap_or_else(|| "Metadata unavailable".into()),
                description: event_metadata
                    .map(|item| item.description.clone())
                    .unwrap_or_default(),
                configured: configured
                    .iter()
                    .filter(|hook| hook.event == *event)
                    .map(|hook| {
                        let matcher = hook.matcher.as_deref().unwrap_or("all");
                        format!(
                            "[{}] {} · matcher: {} · {}",
                            hook.config.type_str(),
                            display_text(&hook.config),
                            matcher,
                            hook.source.header()
                        )
                    })
                    .collect(),
            }
        })
        .collect();
    HooksDialogInput {
        events,
        warnings,
        initial_event: initial_event.map(str::to_string),
    }
}

fn format_hooks_command(
    tool_names: Vec<String>,
    agent_types: Vec<String>,
    cmd: HooksCommand,
    configured_hooks: &[IndividualHookConfig],
    settings_warnings: &[String],
) -> String {
    let metadata = build_hook_event_metadata(&MetadataInputs {
        tool_names,
        agent_types,
        elicitation_servers: Vec::new(),
    });
    let selected_event = cmd.event_name.as_deref().map(|name| {
        parse_hook_event(name)
            .map(|event| event.name().to_string())
            .unwrap_or_else(|| name.to_string())
    });
    let events = HOOK_EVENTS
        .iter()
        .map(|event| {
            let event_metadata = metadata.get(event);
            let matcher = matcher_metadata_for_event(&metadata, *event);
            let configs = configured_hooks
                .iter()
                .filter(|hook| hook.event == *event)
                .map(|hook| rebon_slash_commands::formatters::HookConfigDto {
                    kind: hook.config.type_str().to_string(),
                    display: display_text(&hook.config).to_string(),
                    matcher: hook.matcher.as_deref().unwrap_or("all").to_string(),
                    source: hook.source.header().to_string(),
                })
                .collect();
            rebon_slash_commands::formatters::HookEventDto {
                name: event.name().to_string(),
                summary: event_metadata.map(|value| value.summary.clone()),
                description: event_metadata.map(|value| value.description.clone()),
                matcher: matcher.map(|value| rebon_slash_commands::formatters::HookMatcherDto {
                    field: value.field_to_match.clone(),
                    values: value.values.clone(),
                }),
                configs,
            }
        })
        .collect();
    rebon_slash_commands::formatters::format_hooks_command(
        rebon_slash_commands::formatters::HooksCommandDto {
            selected_event,
            events,
            warnings: settings_warnings.to_vec(),
        },
    )
}

/// The three settings files `/hooks` reports on. The user file lives in the
/// config home, resolved the way the rest of the process resolves it.
pub(crate) fn hook_settings_paths(cwd: &Path) -> rebon_hooks::SettingsPaths {
    rebon_hooks::SettingsPaths::from_config_dir(&crate::rebon_config::config_home_dir(), cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_command_parses_bare_event_and_rejects_adjacent_names() {
        assert_eq!(
            parse_hooks_command("/hooks"),
            Some(HooksCommand { event_name: None })
        );
        assert_eq!(
            parse_hooks_command("/hooks PreToolUse"),
            Some(HooksCommand {
                event_name: Some("PreToolUse".into())
            })
        );
        assert_eq!(parse_hooks_command("/hooksfoo"), None);
    }

    #[test]
    fn hooks_formatter_lists_summary_and_focused_event() {
        let summary = format_hooks_command(
            vec!["Read".into(), "Write".into()],
            vec!["Explore".into()],
            HooksCommand { event_name: None },
            &[],
            &[],
        );
        assert!(summary.starts_with("Hooks\nRead-only hook event summary."));
        assert!(summary.contains(&format!("Known hook events: {}", HOOK_EVENTS.len())));
        let first_event = HOOK_EVENTS[0].name();
        assert!(summary.contains(&format!("- {first_event} — matcher metadata: yes")));
        assert!(summary.contains("Before tool execution"));
        assert!(summary.contains("not configured"));
        assert!(!summary.contains("This build does not include a local hook config browser."));

        let focused = format_hooks_command(
            vec!["Read".into(), "Write".into()],
            vec!["Explore".into()],
            HooksCommand {
                event_name: Some(first_event.into()),
            },
            &[],
            &[],
        );
        assert!(focused.starts_with(&format!("Hooks: {first_event}")));
        assert!(focused.contains("summary: Before tool execution"));
        assert!(focused.contains("matcher metadata: yes"));
        assert!(focused.contains("field: tool_name"));
    }

    #[test]
    fn hooks_formatter_reports_unknown_event_friendly_error() {
        let output = format_hooks_command(
            Vec::new(),
            Vec::new(),
            HooksCommand {
                event_name: Some("NoSuchEvent".into()),
            },
            &[],
            &[],
        );
        assert!(output.contains("Unknown hook event: NoSuchEvent"));
        assert!(output.contains(HOOK_EVENTS[0].name()));
        assert!(output.contains("Run `/hooks`"));
    }
}
