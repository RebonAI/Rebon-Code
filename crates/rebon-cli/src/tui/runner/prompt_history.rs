use crate::session::input_history::{append_history, load_history, HistoryEntry};
use crate::tui::app::AppState;
use crate::tui::dispatch::save_to_history;
use crate::tui::wiring::TuiEngineSession;

use super::slash_commands::selected_slash_command;

/// The channel a background history load reports on.
pub(crate) type InputHistoryRx = std::sync::mpsc::Receiver<Vec<HistoryEntry>>;

/// Load the prompt history on a thread of its own.
///
/// `history.jsonl` is every prompt ever typed — megabytes, parsed line by
/// line — and the first frame used to wait ~150 ms for it. Up-arrow has no
/// history until the load lands; [`apply_loaded_input_history_if_idle`]
/// merges it in behind whatever was typed meanwhile.
pub(super) fn spawn_input_history_load(cwd: String, session_id: String) -> InputHistoryRx {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("rebon-input-history".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let entries = load_input_history(&cwd, &session_id);
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                entries = entries.len(),
                "rebon startup: input history loaded"
            );
            // The loop may be gone already; nothing to report to then.
            let _ = tx.send(entries);
        });
    if let Err(err) = spawned {
        tracing::warn!(error = %err, "rebon-cli: could not start the prompt history loader");
    }
    rx
}

fn load_input_history(cwd: &str, session_id: &str) -> Vec<HistoryEntry> {
    let config_home = crate::rebon_config::config_home_dir();
    match load_history(&config_home, cwd, session_id) {
        Ok(entries) => entries.into_iter().rev().collect(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                path = %crate::session::input_history::history_path(&config_home).display(),
                "rebon-cli: failed to load prompt history"
            );
            Vec::new()
        }
    }
}

/// Put a finished history load behind the prompts typed since startup, but
/// not while the user is walking the history: an index into a list that
/// just grew underneath would land on the wrong entry. Returns whether the
/// load was applied (and is therefore consumed).
pub(super) fn apply_loaded_input_history_if_idle(
    app: &mut AppState,
    history_rx: &mut Option<InputHistoryRx>,
) -> bool {
    let Some(rx) = history_rx.as_ref() else {
        return false;
    };
    if app.history_index != 0 {
        return false;
    }
    match rx.try_recv() {
        Ok(loaded) => {
            *history_rx = None;
            merge_loaded_input_history(app, loaded);
            true
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => false,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            *history_rx = None;
            false
        }
    }
}

fn merge_loaded_input_history(app: &mut AppState, loaded: Vec<HistoryEntry>) {
    let typed_since_startup = std::mem::replace(&mut app.history, loaded);
    app.history.extend(typed_since_startup);
}

fn save_history_entry_to_disk(cwd: &str, session_id: &str, entry: &HistoryEntry) {
    let config_home = crate::rebon_config::config_home_dir();
    if let Err(err) = append_history(&config_home, cwd, session_id, entry) {
        tracing::warn!(
            error = %err,
            path = %crate::session::input_history::history_path(&config_home).display(),
            "rebon-cli: failed to write prompt history"
        );
    }
}

fn save_to_history_for_project(
    app: &mut AppState,
    cwd: &str,
    session_id: &str,
    text: &str,
) -> Option<HistoryEntry> {
    let entry = save_to_history(app, text)?;
    save_history_entry_to_disk(cwd, session_id, &entry);
    Some(entry)
}

pub(super) fn save_to_history_for_session_if_needed(
    app: &mut AppState,
    session: &TuiEngineSession,
    text: &str,
) -> Option<HistoryEntry> {
    save_to_history_for_project_if_needed(app, &session.cwd, &session.session_id, text)
}

/// The history write for a prompt that has a project and a session id but
/// no session yet — the hosted default names the session before it builds
/// it, and a prompt typed in that window is history like any other.
pub(super) fn save_to_history_for_project_if_needed(
    app: &mut AppState,
    cwd: &str,
    session_id: &str,
    text: &str,
) -> Option<HistoryEntry> {
    if should_record_prompt_history(app, text) {
        save_to_history_for_project(app, cwd, session_id, text)
    } else {
        skip_history_for_prompt(app, text);
        None
    }
}

fn skip_history_for_prompt(app: &mut AppState, text: &str) {
    if !text.trim().is_empty() {
        app.history_index = 0;
        app.saved_draft = None;
        app.saved_draft_pasted_contents = None;
    }
}

fn should_record_prompt_history(app: &AppState, text: &str) -> bool {
    !is_sensitive_provider_command(text) && !is_enter_executable_slash_command(app, text)
}

/// Whether this line is a `/provider add …` carrying an API key.
///
/// Matched the way the dispatcher matches it — case and all. `/Provider` runs
/// the command, so `/Provider add openai sk-…` has to be kept out of the
/// history file too, and a literal comparison here would have written the key
/// to disk. Leading whitespace is trimmed rather than disqualifying: a line the
/// dispatcher declines still reached the model with the key in it, and there is
/// no reason to also keep it.
fn is_sensitive_provider_command(text: &str) -> bool {
    let trimmed = text.trim();
    let Some(rest) = crate::tui::runner::commands::strip_command_prefix(trimmed, "provider") else {
        return false;
    };
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with(':') {
        return false;
    }

    let mut parts = rest
        .trim_start_matches(|ch: char| ch == ' ' || ch == ':')
        .split_whitespace();
    parts
        .next()
        .is_some_and(|command| command.eq_ignore_ascii_case("add"))
        && parts.next().is_some()
}

fn is_enter_executable_slash_command(app: &AppState, text: &str) -> bool {
    let Some(command_text) = text.trim_end().strip_prefix('/') else {
        return false;
    };
    let mut parts = command_text.splitn(2, char::is_whitespace);
    if parts.next().unwrap_or("").is_empty() || parts.next().unwrap_or("").trim().len() > 0 {
        return false;
    }

    selected_slash_command(app, text).is_some_and(|command| !slash_command_requires_input(command))
}

pub(super) fn slash_command_requires_input(command: &rebon_types::SlashCommand) -> bool {
    command
        .input
        .as_ref()
        .and_then(|input| input.hint.as_deref())
        .is_some_and(|hint| hint.trim_start().starts_with('<'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_executable_slash_commands_are_not_prompt_history() {
        let mut app = AppState::new();
        app.slash_commands = vec![
            rebon_types::SlashCommand {
                name: "new".into(),
                description: "Start a fresh conversation".into(),
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

        assert!(!should_record_prompt_history(&app, "/new"));
        assert!(!should_record_prompt_history(&app, "/compact"));
        assert!(should_record_prompt_history(
            &app,
            "/compact preserve latest"
        ));
        assert!(should_record_prompt_history(&app, "/run cargo test"));
        assert!(should_record_prompt_history(&app, "/unknown"));
        assert!(should_record_prompt_history(&app, "normal prompt"));
    }

    #[test]
    fn provider_add_commands_with_inline_credentials_are_not_prompt_history() {
        let app = AppState::new();

        assert!(!should_record_prompt_history(
            &app,
            "/provider add deepseek sk-secret"
        ));
        assert!(!should_record_prompt_history(
            &app,
            "/provider:add custom openai https://example.com sk-secret model"
        ));
        assert!(should_record_prompt_history(&app, "/provider add"));
        assert!(should_record_prompt_history(&app, "/provider use deepseek"));
        assert!(should_record_prompt_history(
            &app,
            "/providerish add secret"
        ));
    }

    fn entry(text: &str) -> HistoryEntry {
        HistoryEntry {
            display: text.into(),
            pasted_contents: Vec::new(),
            timestamp: 0,
        }
    }

    /// The load lands behind what was typed while it ran, in the order
    /// Up-arrow expects (oldest first, the newest prompt last).
    #[test]
    fn a_finished_history_load_goes_behind_the_prompts_typed_meanwhile() {
        let mut app = AppState::new();
        app.history = vec![entry("typed while loading")];
        let (tx, rx) = std::sync::mpsc::channel();
        let mut history_rx = Some(rx);

        assert!(
            !apply_loaded_input_history_if_idle(&mut app, &mut history_rx),
            "nothing has arrived"
        );
        tx.send(vec![entry("old one"), entry("old two")]).unwrap();
        assert!(apply_loaded_input_history_if_idle(
            &mut app,
            &mut history_rx
        ));
        assert!(history_rx.is_none(), "consumed");
        let texts: Vec<_> = app.history.iter().map(|e| e.display.as_str()).collect();
        assert_eq!(texts, vec!["old one", "old two", "typed while loading"]);
    }

    /// While the user is walking the history the load waits: the index
    /// points into the list, and the list must not move under it.
    #[test]
    fn a_history_load_waits_while_the_user_is_walking_the_history() {
        let mut app = AppState::new();
        app.history = vec![entry("typed")];
        app.history_index = 1;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut history_rx = Some(rx);
        tx.send(vec![entry("old")]).unwrap();

        assert!(!apply_loaded_input_history_if_idle(
            &mut app,
            &mut history_rx
        ));
        assert!(history_rx.is_some(), "still pending");
        assert_eq!(app.history.len(), 1);

        app.history_index = 0;
        assert!(apply_loaded_input_history_if_idle(
            &mut app,
            &mut history_rx
        ));
        assert_eq!(app.history.len(), 2);
    }

    /// A loader that went away (the thread failed to start, or panicked)
    /// is forgotten rather than polled forever.
    #[test]
    fn a_vanished_history_loader_is_forgotten() {
        let mut app = AppState::new();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<HistoryEntry>>();
        let mut history_rx = Some(rx);
        drop(tx);
        assert!(!apply_loaded_input_history_if_idle(
            &mut app,
            &mut history_rx
        ));
        assert!(history_rx.is_none());
    }

    #[test]
    fn skipped_prompt_history_resets_navigation_state() {
        let mut app = AppState::new();
        app.history_index = 2;
        app.saved_draft = Some("draft".into());
        app.saved_draft_pasted_contents = Some(Vec::new());

        skip_history_for_prompt(&mut app, "/new");

        assert_eq!(app.history_index, 0);
        assert_eq!(app.saved_draft, None);
        assert_eq!(app.saved_draft_pasted_contents, None);
    }
}
