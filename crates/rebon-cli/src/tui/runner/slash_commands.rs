//! Slash-command selection helpers shared by the runner loop,
//! prompt history, and render argument hints.

use crate::tui::app::AppState;

use super::prompt_history::slash_command_requires_input;

pub(super) fn selected_slash_command_needs_input(app: &AppState, input: &str) -> bool {
    let Some(command) = selected_slash_command(app, input) else {
        return false;
    };
    slash_command_requires_input(command)
}

pub(super) fn selected_slash_command<'a>(
    app: &'a AppState,
    input: &str,
) -> Option<&'a rebon_types::SlashCommand> {
    let name = input.trim().strip_prefix('/')?;
    let command_name = name.split_whitespace().next()?;
    app.slash_commands
        .iter()
        .find(|command| command.matches_name_or_alias(command_name))
}

/// Resolve the argument hint for the current input by matching
/// the leading slash-command against the session's slash commands.
pub(super) fn resolve_argument_hint(
    input: &str,
    commands: &[rebon_types::SlashCommand],
) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let cmd_token = trimmed.split_whitespace().next()?;
    let cmd_name = cmd_token.strip_prefix('/')?;
    commands
        .iter()
        .find(|c| c.name == cmd_name)
        .and_then(|c| c.input.as_ref())
        .and_then(|i| i.hint.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_slash_command_needs_input_only_for_required_hints() {
        let mut app = AppState::new();
        app.slash_commands = vec![
            rebon_types::SlashCommand {
                name: "tasks".into(),
                description: "Show background tasks".into(),
                input: None,
                category: None,
                aliases: Vec::new(),
            },
            rebon_types::SlashCommand {
                name: "compact".into(),
                description: "Compact context".into(),
                input: Some(rebon_types::SlashCommandInput {
                    hint: Some("[instructions]".into()),
                }),
                category: None,
                aliases: Vec::new(),
            },
            rebon_types::SlashCommand {
                name: "run".into(),
                description: "Run a background shell task".into(),
                input: Some(rebon_types::SlashCommandInput {
                    hint: Some("<command>".into()),
                }),
                category: None,
                aliases: Vec::new(),
            },
        ];

        assert!(!selected_slash_command_needs_input(&app, "/tasks "));
        assert!(!selected_slash_command_needs_input(&app, "/compact "));
        assert!(selected_slash_command_needs_input(&app, "/run "));
    }

    #[test]
    fn resolve_argument_hint_matches_command_name_only() {
        let commands = vec![rebon_types::SlashCommand {
            name: "run".into(),
            description: "Run a background shell task".into(),
            input: Some(rebon_types::SlashCommandInput {
                hint: Some("<command>".into()),
            }),
            category: None,
            aliases: vec!["r".into()],
        }];

        assert_eq!(
            resolve_argument_hint("/run ", &commands),
            Some("<command>".into())
        );
        assert_eq!(resolve_argument_hint("run", &commands), None);
        assert_eq!(resolve_argument_hint("/r ", &commands), None);
    }
}
