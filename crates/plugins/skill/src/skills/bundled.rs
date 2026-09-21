//! Bundled skill definitions that ship with the CLI binary.
//!
//! Bundled skills are registered at startup and available to all
//! users. This module provides the definition type, an in-memory
//! registry, and helpers for embedded reference-file path
//! validation.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use super::frontmatter::FrontmatterValue;

// ---------------------------------------------------------------------------
// Execution context
// ---------------------------------------------------------------------------

/// How a skill runs relative to the current conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionContext {
    /// Content expands into the current conversation (default).
    Inline,
    /// Runs in a sub-agent with its own context & token budget.
    Fork,
}

// ---------------------------------------------------------------------------
// BundledSkillDefinition
// ---------------------------------------------------------------------------

/// Static definition for a skill that ships with the CLI.
///
/// No prompt-generating closure is stored here — the Rust integration layer
/// provides prompt generation as a trait implementation or callback on the
/// consuming side. This struct captures the *data* portion of a
/// bundled skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledSkillDefinition {
    /// Skill name (used as `/name` in CLI).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Alternate names that also resolve to this skill.
    pub aliases: Vec<String>,
    /// Hint shown after the skill name in completion UIs.
    pub argument_hint: Option<String>,
    /// When the model should consider invoking this skill.
    pub when_to_use: Option<String>,
    /// Tools the skill is allowed to call.
    pub allowed_tools: Vec<String>,
    /// Model override (e.g. `"haiku"`).
    pub model: Option<String>,
    /// If `true`, only user can invoke via `/name`; model cannot.
    pub disable_model_invocation: bool,
    /// Whether the user can type `/name` to invoke.
    pub user_invocable: bool,
    /// Execution context (inline / fork).
    pub context: Option<ExecutionContext>,
    /// Agent type when forked (e.g. `"batch-worker"`).
    pub agent: Option<String>,
    /// Runtime-checked enabled predicate. When `false` the skill
    /// is hidden from the model and from typeahead. This is the evaluated
    /// form of the optional `enabled` predicate in the skill definition.
    pub is_enabled: bool,
    /// Optional hooks configuration, kept opaque.
    pub hooks: Option<HashMap<String, FrontmatterValue>>,
    /// Embedded reference files: relative path → content.
    ///
    /// When non-empty, files are extracted to disk on first
    /// invocation and a "Base directory for this skill" prefix is
    /// prepended to the prompt.
    pub files: Vec<(String, String)>,
    /// The prompt template body (markdown).
    pub prompt_body: String,
}

impl Default for BundledSkillDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            aliases: Vec::new(),
            argument_hint: None,
            when_to_use: None,
            allowed_tools: Vec::new(),
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            context: None,
            agent: None,
            is_enabled: true,
            hooks: None,
            files: Vec::new(),
            prompt_body: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// In-memory registry of bundled skill definitions, in registration
/// order.
#[derive(Debug, Clone, Default)]
pub struct BundledSkillRegistry {
    skills: Vec<BundledSkillDefinition>,
}

impl BundledSkillRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a bundled skill. Appends to the end of the list.
    pub fn register(&mut self, definition: BundledSkillDefinition) {
        self.skills.push(definition);
    }

    /// Get all registered bundled skills (returns a clone to
    /// prevent external mutation).
    pub fn get_all(&self) -> Vec<BundledSkillDefinition> {
        self.skills.clone()
    }

    /// Look up a skill by name or alias.
    pub fn get_by_name(&self, name: &str) -> Option<&BundledSkillDefinition> {
        self.skills
            .iter()
            .find(|s| s.name == name || s.aliases.iter().any(|a| a == name))
    }

    /// Clear the registry (for testing).
    pub fn clear(&mut self) {
        self.skills.clear();
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Embedded file path validation
// ---------------------------------------------------------------------------

/// Normalize and validate a skill-relative path; returns `Err` on
/// traversal attempts (absolute paths or `..` components).
pub fn resolve_skill_file_path(base_dir: &Path, rel_path: &str) -> Result<PathBuf, SkillPathError> {
    let normalized = Path::new(rel_path);

    // Reject absolute paths (on Windows, `/foo` is not considered
    // absolute by `Path::is_absolute`, so also check for leading `/`).
    if normalized.is_absolute() || rel_path.starts_with('/') {
        return Err(SkillPathError::Traversal(rel_path.to_string()));
    }

    // Reject `..` components (both OS separator and `/`).
    for component in normalized.components() {
        if matches!(component, Component::ParentDir) {
            return Err(SkillPathError::Traversal(rel_path.to_string()));
        }
    }
    // Belt-and-suspenders: also check raw string for `/..` on
    // platforms where Component parsing might differ.
    if rel_path.split('/').any(|seg| seg == "..") {
        return Err(SkillPathError::Traversal(rel_path.to_string()));
    }

    Ok(base_dir.join(normalized))
}

/// Error returned by [`resolve_skill_file_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillPathError {
    /// The relative path escapes the skill directory.
    Traversal(String),
}

impl std::fmt::Display for SkillPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Traversal(path) => {
                write!(f, "bundled skill file path escapes skill dir: {path}")
            }
        }
    }
}

impl std::error::Error for SkillPathError {}

// ---------------------------------------------------------------------------
// Base-directory prefix helper
// ---------------------------------------------------------------------------

/// Prepend `"Base directory for this skill: {dir}\n\n"` to a prompt string.
pub fn prepend_base_dir(prompt: &str, base_dir: &str) -> String {
    format!("Base directory for this skill: {base_dir}\n\n{prompt}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolve_valid_relative_path() {
        let base = Path::new("/tmp/skills/verify");
        let result = resolve_skill_file_path(base, "examples/cli.md");
        assert_eq!(
            result.unwrap(),
            Path::new("/tmp/skills/verify/examples/cli.md"),
        );
    }

    #[test]
    fn resolve_rejects_parent_traversal() {
        let base = Path::new("/tmp/skills/verify");
        assert!(resolve_skill_file_path(base, "../etc/passwd").is_err());
        assert!(resolve_skill_file_path(base, "foo/../../etc/passwd").is_err());
    }

    #[test]
    fn resolve_rejects_absolute_path() {
        let base = Path::new("/tmp/skills/verify");
        assert!(resolve_skill_file_path(base, "/etc/passwd").is_err());
    }

    #[test]
    fn prepend_base_dir_adds_prefix() {
        let result = prepend_base_dir("Hello world", "/tmp/skill");
        assert_eq!(
            result,
            "Base directory for this skill: /tmp/skill\n\nHello world"
        );
    }

    #[test]
    fn registry_round_trip() {
        let mut reg = BundledSkillRegistry::new();
        assert!(reg.is_empty());

        reg.register(BundledSkillDefinition {
            name: "simplify".into(),
            description: "Code review".into(),
            user_invocable: true,
            ..Default::default()
        });

        assert_eq!(reg.len(), 1);
        assert!(reg.get_by_name("simplify").is_some());
        assert!(reg.get_by_name("missing").is_none());

        let all = reg.get_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "simplify");

        reg.clear();
        assert!(reg.is_empty());
    }

    #[test]
    fn default_definition_has_sane_values() {
        let def = BundledSkillDefinition::default();
        assert!(def.user_invocable);
        assert!(!def.disable_model_invocation);
        assert!(def.context.is_none());
        assert!(def.files.is_empty());
        assert!(def.is_enabled);
        assert!(def.hooks.is_none());
    }

    #[test]
    fn get_by_name_finds_by_alias() {
        let mut reg = BundledSkillRegistry::new();
        reg.register(BundledSkillDefinition {
            name: "simplify".into(),
            description: "Code review".into(),
            aliases: vec!["review".into(), "cleanup".into()],
            ..Default::default()
        });
        assert!(reg.get_by_name("simplify").is_some());
        assert!(reg.get_by_name("review").is_some());
        assert!(reg.get_by_name("cleanup").is_some());
        assert!(reg.get_by_name("missing").is_none());
    }
}
