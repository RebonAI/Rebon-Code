//! Per-agent memory primitives.
//!
//! The module provides:
//!
//! - [`AgentMemoryScope`] — `user` / `project` / `local`
//! - [`agent_memory_dir`] — resolve on-disk dir per scope
//! - [`load_agent_memory_prompt`] — build the per-agent memory prompt
//! (reuses the main memory prompt builder with a scoped-display name
//! and scope-specific guideline)
//!
//! The `agents` plugin is what wires this in: a spawn whose frontmatter
//! names a `memory:` scope gets its per-agent memory prompt through
//! `rebon_instructions::agent_documents::agent_memory_prompt`, which
//! reaches [`load_agent_memory_prompt`] over the `agent-memory-prompt` seam.

use std::path::{Path, PathBuf};

use crate::memory::prompt::{ensure_memory_dir_exists, get_rebon_home};

/// Scope of per-agent memory persistence, parsed from the `user` /
/// `project` / `local` strings by [`AgentMemoryScope::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMemoryScope {
    /// Cross-project: `~/.rebon/agent-memory/<agent_type>/`.
    User,
    /// Project-shared (VCS-checked): `<cwd>/.rebon/agent-memory/<agent_type>/`.
    Project,
    /// Project-local (not VCS): `<cwd>/.rebon/agent-memory-local/<agent_type>/`.
    Local,
}

impl AgentMemoryScope {
    /// Parse a frontmatter string into a scope. Only `user`, `project`
    /// and `local` are accepted; anything else returns `None`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    /// Canonical scope name used in wire formats. Does not change.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// Replace `:` (invalid on Windows, used by plugin-namespaced types
/// like `my-plugin:my-agent`) with `-`.
fn sanitize_agent_type(agent_type: &str) -> String {
    agent_type.replace(':', "-")
}

/// Resolve the on-disk directory for a given agent's memory.
pub fn agent_memory_dir(agent_type: &str, scope: AgentMemoryScope, cwd: &Path) -> PathBuf {
    let dir_name = sanitize_agent_type(agent_type);
    match scope {
        AgentMemoryScope::User => get_rebon_home()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("agent-memory")
            .join(dir_name),
        AgentMemoryScope::Project => cwd.join(".rebon").join("agent-memory").join(dir_name),
        AgentMemoryScope::Local => cwd.join(".rebon").join("agent-memory-local").join(dir_name),
    }
}

/// Path of the `MEMORY.md` entrypoint for an agent's memory.
pub fn agent_memory_entrypoint(agent_type: &str, scope: AgentMemoryScope, cwd: &Path) -> PathBuf {
    agent_memory_dir(agent_type, scope, cwd).join("MEMORY.md")
}

/// Build the full per-agent memory prompt: behavioural instructions
/// (headered `# Persistent Agent Memory`) + the scope-specific
/// guideline + MEMORY.md contents. Creates the directory if missing
/// so the child agent's first `Write` call doesn't have to mkdir.
///
/// Renders the `# Persistent Agent Memory` header and appends the scope note for `scope`.
pub fn load_agent_memory_prompt(agent_type: &str, scope: AgentMemoryScope, cwd: &Path) -> String {
    let dir = agent_memory_dir(agent_type, scope, cwd);
    ensure_memory_dir_exists(&dir);
    let entrypoint_path = dir.join("MEMORY.md");
    let entrypoint_content = std::fs::read_to_string(&entrypoint_path).ok();

    let scope_note = match scope {
        AgentMemoryScope::User => "- Since this memory is user-scope, keep learnings general since they apply across all projects",
        AgentMemoryScope::Project => "- Since this memory is project-scope and shared with your team via version control, tailor your memories to this project",
        AgentMemoryScope::Local => "- Since this memory is local-scope (not checked into version control), tailor your memories to this project and machine",
    };

    // Render the directory path using forward slashes so the prompt
    // looks consistent across platforms (matches the main
    // `build_memory_prompt` convention).
    let dir_display = dir.to_string_lossy().replace('\\', "/");

    build_scoped_memory_prompt(
        "Persistent Agent Memory",
        &dir_display,
        entrypoint_content.as_deref(),
        &[scope_note],
    )
}

/// Build a memory prompt with a custom display name and extra
/// guidelines. Wraps the same behavioural lines as `build_memory_prompt` but with a parameterised
/// header and optional per-memory-type hints appended before the
/// persistence-vs-tasks-vs-plan block.
fn build_scoped_memory_prompt(
    display_name: &str,
    memory_dir_display: &str,
    entrypoint_content: Option<&str>,
    extra_guidelines: &[&str],
) -> String {
    // Delegate the boilerplate body to the main prompt builder and
    // then patch the header + guidelines inline. Duplicating the
    // full body text would mean keeping two copies in sync — we avoid
    // it here by string-replacing the only two places that differ.
    let base = crate::memory::prompt::build_memory_prompt(memory_dir_display, entrypoint_content);

    // Swap the "# auto memory" header for the caller-supplied one.
    let mut rewritten = base.replacen("# auto memory", &format!("# {display_name}"), 1);

    // Append extra guidelines (scope note). Place them immediately
    // after the "## Memory and other forms of persistence" block so
    // they sit above the "## Before recommending from memory" and
    // `## MEMORY.md` sections. If that anchor isn't found (future
    // refactor), fall through to the end so the scope note is at
    // least still present in the prompt.
    if !extra_guidelines.is_empty() {
        let anchor = "- When to use or update tasks instead of memory";
        let rendered = extra_guidelines.join("\n");
        if let Some(anchor_pos) = rewritten.find(anchor) {
            // Insert after the line containing the anchor.
            if let Some(eol) = rewritten[anchor_pos..].find('\n') {
                let insert_at = anchor_pos + eol + 1;
                rewritten.insert_str(insert_at, &format!("\n{rendered}\n"));
            } else {
                rewritten.push('\n');
                rewritten.push_str(&rendered);
            }
        } else {
            rewritten.push('\n');
            rewritten.push_str(&rendered);
        }
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parse_round_trip() {
        for s in ["user", "project", "local"] {
            let scope = AgentMemoryScope::parse(s).unwrap();
            assert_eq!(scope.as_str(), s);
        }
        assert!(AgentMemoryScope::parse("global").is_none());
    }

    #[test]
    fn sanitize_agent_type_replaces_colon() {
        assert_eq!(sanitize_agent_type("my-plugin:agent"), "my-plugin-agent");
        assert_eq!(sanitize_agent_type("my-agent"), "my-agent");
    }

    #[test]
    fn project_scope_dir_lives_under_cwd() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-agent-mem-test-")
            .tempdir()
            .unwrap();
        let tmp = tmp_dir.path();
        let dir = agent_memory_dir("explorer", AgentMemoryScope::Project, tmp);
        assert!(dir.starts_with(tmp));
        assert!(dir.ends_with(PathBuf::from(".rebon/agent-memory/explorer")));
    }

    #[test]
    fn local_scope_dir_lives_under_cwd_local() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-agent-mem-test-local-")
            .tempdir()
            .unwrap();
        let dir = agent_memory_dir("explorer", AgentMemoryScope::Local, tmp_dir.path());
        assert!(dir.ends_with(PathBuf::from(".rebon/agent-memory-local/explorer")));
    }

    #[test]
    fn user_scope_dir_sits_under_rebon_home() {
        // Sanity: the path contains "agent-memory" regardless of
        // whether $HOME is set in the test env.
        let tmp = std::env::temp_dir();
        let dir = agent_memory_dir("explorer", AgentMemoryScope::User, &tmp);
        let s = dir.to_string_lossy();
        assert!(s.contains("agent-memory"));
        assert!(s.ends_with("explorer"));
    }

    #[test]
    fn load_prompt_renders_header_and_scope_note() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-agent-mem-load-")
            .tempdir()
            .unwrap();
        let tmp = tmp_dir.path();
        let prompt = load_agent_memory_prompt("explorer", AgentMemoryScope::Project, tmp);
        assert!(prompt.contains("# Persistent Agent Memory"));
        assert!(prompt.contains("Since this memory is project-scope"));
        // Memory dir created.
        assert!(tmp.join(".rebon/agent-memory/explorer").exists());
    }

    #[test]
    fn load_prompt_includes_entrypoint_when_present() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-agent-mem-entry-")
            .tempdir()
            .unwrap();
        let tmp = tmp_dir.path();
        let dir = tmp.join(".rebon/agent-memory/coder");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MEMORY.md"), "- [alpha](a.md) — hello\n").unwrap();

        let prompt = load_agent_memory_prompt("coder", AgentMemoryScope::Project, tmp);
        assert!(prompt.contains("[alpha](a.md)"));
        assert!(prompt.contains("hello"));
    }
}
