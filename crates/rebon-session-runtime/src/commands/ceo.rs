//! `/ceo`: enter or leave coordinator mode, optionally with a task.
//!
//! Parsed here because both the terminal and the background worker read
//! it off a prompt before the turn starts; what each does with the answer
//! stays with them.

use rebon_slash_commands::strip_command_prefix;

/// Parsed result of a `/ceo` command.
pub enum CeoCommand {
    /// `/ceo on` — explicitly enter coordinator mode.
    On,
    /// `/ceo off` — explicitly exit coordinator mode.
    Off,
    /// `/ceo <task>` — enter coordinator mode and submit the task.
    Task(String),
}

/// Recognize `/ceo`, optionally followed by `on`, `off`, or a task prompt.
pub fn parse_ceo_command(text: &str) -> Option<CeoCommand> {
    let rest = strip_command_prefix(text, "ceo")?;
    if rest.is_empty() {
        return Some(CeoCommand::On);
    }
    if !rest.starts_with(' ') {
        return None;
    }
    let arg = rest.trim();
    Some(match arg.to_ascii_lowercase().as_str() {
        "on" => CeoCommand::On,
        "off" => CeoCommand::Off,
        _ => CeoCommand::Task(arg.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ceo_accepts_on_off_and_task_forms() {
        assert!(matches!(parse_ceo_command("/ceo"), Some(CeoCommand::On)));
        assert!(matches!(parse_ceo_command("/ceo on"), Some(CeoCommand::On)));
        assert!(matches!(
            parse_ceo_command("/ceo off"),
            Some(CeoCommand::Off)
        ));
        assert!(matches!(
            parse_ceo_command("/ceo ship the refactor"),
            Some(CeoCommand::Task(task)) if task == "ship the refactor"
        ));
    }

    #[test]
    fn parse_ceo_rejects_ultrawork_aliases_and_prefix_collisions() {
        assert!(parse_ceo_command("/ceofoo").is_none());
        assert!(parse_ceo_command("/ultrawork").is_none());
        assert!(parse_ceo_command("/ulw do work").is_none());
    }
}
