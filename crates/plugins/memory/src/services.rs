//! What this plugin answers for other people's questions.
//!
//! Two seams in `rebon-instructions`, both with a consumer on the far side of
//! the plugin boundary. `loaded-documents` is what `/memory` and `/context`
//! print: the project instruction files plus the auto-`MEMORY.md` entrypoint,
//! and only this crate knows the per-project switch that decides whether the
//! entrypoint is loaded at all. `agent-memory-prompt` is what the `agents`
//! plugin appends to a sub-agent's system prompt when its definition declares
//! a memory scope.
//!
//! Both go away with the plugin. A surface with no provider reports no
//! documents and a sub-agent gets no memory, which is the same answer either
//! would give for a project that has none.

use std::path::Path;

use rebon_instructions::agent_documents::AgentMemoryPrompt;

/// The memory feature's answer on the `agent-memory-prompt` seam.
pub struct MemoryAgentPrompts;

impl AgentMemoryPrompt for MemoryAgentPrompts {
    fn agent_memory_prompt(&self, agent_type: &str, scope: &str, cwd: &Path) -> Option<String> {
        let scope = crate::memory::agent_memory::AgentMemoryScope::parse(scope)?;
        Some(crate::memory::agent_memory::load_agent_memory_prompt(
            agent_type, scope, cwd,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolving a scope creates the directory it names, under the process
    /// home and under the cwd it is handed. Both have to be this test's own:
    /// the home because it is process-global and every other env-mutating
    /// test in this crate takes the same lock, the cwd because `.` would be
    /// the crate directory.
    struct TempHome {
        _temp: tempfile::TempDir,
        cwd: std::path::PathBuf,
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TempHome {
        fn new() -> Self {
            let _lock = crate::memory::test_env::env_test_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let temp = tempfile::tempdir().expect("temp home");
            let home = temp.path().to_path_buf();
            let cwd = home.join("project");
            std::fs::create_dir_all(&cwd).expect("project dir");
            let prev = ["HOME", "USERPROFILE", "REBON_CONFIG_DIR"]
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
            std::env::set_var("REBON_CONFIG_DIR", home.join(".rebon"));
            Self {
                _temp: temp,
                cwd,
                prev,
                _lock,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            for (name, value) in std::mem::take(&mut self.prev) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    /// The scope vocabulary is this crate's; an unrecognised one is "no
    /// agent memory" rather than an error the spawn has to handle.
    #[test]
    fn an_unknown_scope_answers_none() {
        let home = TempHome::new();
        assert_eq!(
            MemoryAgentPrompts.agent_memory_prompt("coder", "global", &home.cwd),
            None
        );
        assert_eq!(
            MemoryAgentPrompts.agent_memory_prompt("coder", "", &home.cwd),
            None
        );
    }

    /// Every scope the store recognises produces a prompt.
    #[test]
    fn every_known_scope_answers_a_prompt() {
        let home = TempHome::new();
        for scope in ["user", "project", "local"] {
            let prompt = MemoryAgentPrompts
                .agent_memory_prompt("coder", scope, &home.cwd)
                .unwrap_or_else(|| panic!("{scope} is a known scope"));
            assert!(!prompt.is_empty(), "{scope}");
        }
    }
}
