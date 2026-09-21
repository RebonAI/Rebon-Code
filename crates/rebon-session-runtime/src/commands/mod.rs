//! The slash commands a session can answer with no terminal attached.
//!
//! Twelve commands report on the session itself, and three surfaces run
//! them: the terminal the user typed into, the background worker
//! answering over IPC, and `serve` on behalf of a page. Only the first
//! has a screen, so what each command needs from one is named in
//! [`SessionCommandInputs`] and filled by whoever has it.
//!
//! The terminal's half of that -- reading the inputs off an `AppState`,
//! and deciding a command was typed at all -- stays in
//! the binary's `tui::runner::commands`.

use rebon_agent_core::DenialReplayRequest;

pub mod ceo;
pub mod context;
pub mod context_sources;
pub mod context_visualization;
pub mod control;
pub mod cost;
pub mod doctor;
pub mod effort;
pub mod hooks;
pub mod kernel;
pub mod mcp;
pub mod model;
pub mod permissions;
pub mod plugin;
pub mod provider;
pub mod rewind;
pub mod status;
pub mod ultraplan_prompt;

pub struct SessionControlCommandResult {
    pub output: rebon_session_host::CommandOutput,
    pub replay_requests: Vec<DenialReplayRequest>,
}

/// Everything a session-control command reads that is not the session.
///
/// A terminal fills this from its `AppState`; a surface with no screen
/// fills the same fields from what it holds. Naming them is what lets one
/// implementation answer both, instead of the worker assembling a whole
/// off-screen `AppState` to be read for a dozen values.
pub struct SessionCommandInputs<'a> {
    /// The rendered transcript, which `/context` counts and `/rewind`
    /// searches. Rendered rows rather than engine entries is the one thing
    /// here that still ties a worker to a screenless `AppState`.
    pub rows: &'a [rebon_render::transcript_row::Message],
    /// Engine state, mirrored on `AppState` for rendering. A worker reads
    /// the same value from `engine_half.permission_mode_cell`.
    pub permission_mode: rebon_permissions::types::PermissionMode,
    /// One reading of the session's usage ledger (`session::usage`), taken
    /// under one lock so the totals, the last turn and the per-model buckets
    /// are all the same moment.
    ///
    /// Owned rather than borrowed: the ledger lives behind a mutex, and a
    /// borrow of the map inside it cannot outlive the guard.
    pub usage: crate::usage::UsageSnapshot,
    /// Tokens seen so far in the answer being streamed right now. A UI
    /// counter with no meaning between turns: it is zero in a terminal
    /// that is not streaming, and zero in a worker always.
    pub streaming_token_count: u32,
    /// The next three are shared with the engine, not copied: a worker
    /// holds the same `Arc`s.
    pub auto_mode_denials: &'a std::sync::Arc<
        std::sync::Mutex<rebon_permissions::auto_mode_denials::AutoModeDenialStore>,
    >,
    pub auto_mode_verdicts: &'a std::sync::Arc<rebon_permissions::AutoModeVerdictCache>,
    pub task_snapshots: Vec<rebon_plugin_tasks::runtime::TaskSnapshot>,
    pub session_title: Option<&'a str>,
    /// Which way the terminal draws. A worker has no screen and reports
    /// the mode its session was configured with.
    pub ui_mode: crate::ui_config::UiMode,
    /// The last three are terminal-only: a worker leaves them empty, and
    /// that is the fact rather than a gap to fill.
    ///
    /// The mode's label rather than the terminal's enum, for the same reason
    /// the field below takes a label: carrying a type only the terminal
    /// builds would tie every surface to it, and `/status` prints the word.
    pub vim_mode: Option<&'static str>,
    /// The ultraplan phase as a label, because that is all `/status`
    /// shows; carrying the terminal's status object would tie every
    /// surface to a type only the terminal builds.
    pub ultraplan_phase: Option<String>,
    pub update_notice: Option<&'a rebon_plugin_updater::UpdateNoticeState>,
}

/// Whether `rest` — what [`rebon_slash_commands::strip_command_prefix`] returned — ends the command
/// name rather than continuing it, so `/modelling` is not `/model`.
pub fn name_ends_here(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(' ') || rest.starts_with(':')
}

/// The arguments after a recognized command, matched the way the recognizer
/// matches it — case, aliases, longest spelling and all. A literal
/// `strip_prefix` here read `/Provider add …` as an empty argument list (and
/// ran `list`) while the recognizer had accepted the spelling.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn command_args<'a>(text: &'a str, name: &str) -> &'a str {
    rebon_slash_commands::strip_command_prefix(text, name)
        .filter(|rest| name_ends_here(rest))
        .unwrap_or("")
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim()
}

/// Split a command's arguments on whitespace, honouring single and double
/// quotes so a path with a space in it stays one argument. `command_name`
/// names the command in the one error this can return, so the reader is told
/// which line has the unterminated quote.
pub(crate) fn tokenize_command_args(
    input: &str,
    command_name: &str,
) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut token_started = false;
    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                token_started = true;
            }
            Some(_) => {
                current.push(ch);
                token_started = true;
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                token_started = true;
            }
            None if ch.is_whitespace() => {
                if token_started {
                    tokens.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            None => {
                current.push(ch);
                token_started = true;
            }
        }
    }
    if let Some(q) = quote {
        return Err(format!("{command_name} has an unterminated {q} quote"));
    }
    if token_started {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Approximate token count for a string (chars / 4).
pub(crate) fn approx_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Format a token count as a human-readable string (e.g. "145.2k").
pub fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_config_dir {
    use std::ffi::OsString;
    use std::path::Path;

    /// Point `config_home_dir()` at a throwaway directory for the duration of
    /// one test, holding the process-wide env lock until the value is restored.
    pub struct ConfigDirGuard {
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ConfigDirGuard {
        pub fn set(dir: &Path) -> Self {
            let lock = crate::test_env::lock_env();
            let previous = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("REBON_CONFIG_DIR", dir);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for ConfigDirGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_env_restore {
    pub struct EnvRestore {
        values: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        pub fn new(keys: &[&'static str]) -> Self {
            Self {
                values: keys
                    .iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in self.values.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use test_config_dir::ConfigDirGuard;
#[cfg(any(test, feature = "test-support"))]
pub use test_env_restore::EnvRestore;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_tokens_small() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
    }

    #[test]
    fn fmt_tokens_thousands() {
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(145_200), "145.2k");
        assert_eq!(fmt_tokens(145_200), "145.2k");
        assert_eq!(fmt_tokens(999_999), "1000.0k");
    }

    #[test]
    fn fmt_tokens_millions() {
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn approx_tokens_basic() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("hello world!"), 3);
    }

    #[test]
    fn command_args_matches_the_recognizer_not_a_literal_prefix() {
        assert_eq!(
            command_args("/Provider add deepseek", "provider"),
            "add deepseek"
        );
        assert_eq!(command_args("/provider", "provider"), "");
        assert_eq!(command_args("/providers list", "provider"), "");
    }

    #[test]
    fn name_ends_here_refuses_a_longer_name() {
        assert!(name_ends_here(""));
        assert!(name_ends_here(" add"));
        assert!(name_ends_here(":add"));
        assert!(!name_ends_here("ling"));
    }
}
