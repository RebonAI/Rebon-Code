//! The wizard's import step: Claude Code / Codex assets into rebon.
//!
//! Scans the user's `~/.claude` and `~/.codex` directories for skills,
//! commands, and agents/prompts, and copies them into the rebon config
//! home (`~/.rebon/skills/`, `~/.rebon/commands/`, `~/.rebon/agents/`).
//!
//! This is the single source of truth for the import step, and everything
//! that runs it goes through here: the wizard's own state machine in
//! [`crate::dialog`], which holds the snapshot and the checklist, and its
//! write path in [`crate::apply`]; the terminal, which draws that step;
//! and the desktop app's two surfaces, its onboarding wizard and the
//! settings window's import panel. The layout normalization below is the
//! reason it must be shared: a plain recursive directory copy produces
//! files rebon's loaders silently ignore.
//!
//! # Not behind the switch
//!
//! Unlike the rest of this plugin, nothing here is gated on
//! `plugins.onboarding.enabled`. It is a library rather than a seat
//! contribution, and the app's import panel and the terminal wizard call
//! it directly. Turning the plugin off takes away `/onboarding` and the
//! first-run gate, not the ability to import.
//!
//! Rebon's on-disk layouts (see `crates/plugins/skill/src/loader.rs`
//! and `crates/rebon-tool/src/agent_registry.rs`):
//!
//! * Skills — `<config_home>/skills/<name>/SKILL.md`. The skills
//!   loader accepts the **directory format only**; a flat
//!   `skills/<name>.md` is skipped.
//! * Commands — `<config_home>/commands/<name>.md` (legacy flat
//!   markdown format).
//! * Agents — `<config_home>/agents/<name>.md`. The agent registry
//!   reads that directory **non-recursively**, so nested source packs
//!   have to be flattened.
//!
//! Source layouts we know how to import:
//!
//! * Claude Code — `~/.claude/skills/*.md` (single-file skills) and
//!   `~/.claude/skills/<name>/SKILL.md` (directory skills),
//!   `~/.claude/commands/**/*.md` (legacy commands), and
//!   `~/.claude/agents/**/*.md` (possibly nested by pack name).
//! * Codex — `~/.codex/skills/*.md` (may be empty) and
//!   `~/.codex/prompts/*.md` (prompt-as-agent).
//!
//! Every source kind is mapped to an [`ImportCategory`]. The UI shows a
//! checklist of these with their discovery counts; on submit,
//! [`perform_migration`] copies the selected categories into place,
//! skipping items that would overwrite an existing entry so re-running
//! the import is idempotent.

#![deny(missing_docs)]

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

// ── Categories ─────────────────────────────────────────────────────

/// One row in the migration checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImportCategory {
    /// Markdown files under `~/.claude/skills/**`.
    ClaudeSkills,
    /// Markdown files under `~/.claude/agents/**`.
    ClaudeAgents,
    /// Markdown files under `~/.claude/commands/**`.
    ClaudeCommands,
    /// Markdown files under `~/.codex/skills/**`.
    CodexSkills,
    /// Markdown files under `~/.codex/prompts/**` — codex prompts
    /// map onto rebon agents because both describe a named,
    /// invocable operator.
    CodexPrompts,
}

impl ImportCategory {
    /// Every category, in checklist order.
    pub const ALL: [Self; 5] = [
        Self::ClaudeSkills,
        Self::ClaudeAgents,
        Self::ClaudeCommands,
        Self::CodexSkills,
        Self::CodexPrompts,
    ];

    /// Full English label including the source path, for surfaces that
    /// render one line per category.
    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeSkills => "Claude skills (~/.claude/skills)",
            Self::ClaudeAgents => "Claude agents (~/.claude/agents)",
            Self::ClaudeCommands => "Claude commands (~/.claude/commands)",
            Self::CodexSkills => "Codex skills (~/.codex/skills)",
            Self::CodexPrompts => "Codex prompts (~/.codex/prompts)",
        }
    }

    /// Source directory as a display string, for surfaces that render
    /// the source and destination in separate columns.
    pub fn source_display(self) -> &'static str {
        match self {
            Self::ClaudeSkills => "~/.claude/skills",
            Self::ClaudeAgents => "~/.claude/agents",
            Self::ClaudeCommands => "~/.claude/commands",
            Self::CodexSkills => "~/.codex/skills",
            Self::CodexPrompts => "~/.codex/prompts",
        }
    }

    /// Where the category's files land in rebon's config home.
    pub fn dest_label(self) -> &'static str {
        match self {
            Self::ClaudeSkills | Self::CodexSkills => "skills",
            Self::ClaudeAgents | Self::CodexPrompts => "agents",
            Self::ClaudeCommands => "commands",
        }
    }

    /// Destination directory as a display string (`~/.rebon/<kind>`).
    pub fn dest_display(self) -> &'static str {
        match self {
            Self::ClaudeSkills | Self::CodexSkills => "~/.rebon/skills",
            Self::ClaudeAgents | Self::CodexPrompts => "~/.rebon/agents",
            Self::ClaudeCommands => "~/.rebon/commands",
        }
    }
}

// ── Discovery ──────────────────────────────────────────────────────

/// A single discovered markdown file that can be imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredItem {
    /// Logical name (used for the destination filename / dir).
    pub name: String,
    /// Absolute source path.
    pub source: PathBuf,
}

/// Scan a source root for one category, returning the discovered
/// files. Missing directories produce an empty vec — the migration
/// step must degrade gracefully on fresh machines.
pub fn scan_category(home: &Path, category: ImportCategory) -> Vec<DiscoveredItem> {
    let dir = source_dir(home, category);
    if !dir.exists() {
        return Vec::new();
    }
    let mut items = Vec::new();
    walk_md_files(&dir, &dir, &mut items);
    items.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.source.cmp(&b.source)));

    let mut used_names = HashSet::new();
    for item in &mut items {
        let base = item.name.clone();
        let mut candidate = base.clone();
        let mut suffix = 2usize;
        while !used_names.insert(candidate.clone()) {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
        item.name = candidate;
    }
    items
}

fn source_dir(home: &Path, category: ImportCategory) -> PathBuf {
    match category {
        ImportCategory::ClaudeSkills => home.join(".claude").join("skills"),
        ImportCategory::ClaudeAgents => home.join(".claude").join("agents"),
        ImportCategory::ClaudeCommands => home.join(".claude").join("commands"),
        ImportCategory::CodexSkills => home.join(".codex").join("skills"),
        ImportCategory::CodexPrompts => home.join(".codex").join("prompts"),
    }
}

fn walk_md_files(root: &Path, dir: &Path, out: &mut Vec<DiscoveredItem>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk_md_files(root, &path, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        // Skip SKILL.md when we're about to walk into the containing
        // directory (directory-format skills): use the directory name
        // instead so nested SKILL.md from Claude Code's newer format
        // round-trips cleanly.
        let name = if stem.eq_ignore_ascii_case("SKILL") {
            match path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
            {
                Some(n) => n.to_string(),
                None => stem,
            }
        } else {
            stem
        };
        out.push(DiscoveredItem {
            name: sanitize_name(&name),
            source: path,
        });
    }
}

/// Strip filesystem-unfriendly characters while preserving Unicode
/// letters and numbers. Separator runs collapse to a single `-`.
fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "imported".to_string()
    } else {
        trimmed
    }
}

// ── Snapshot ───────────────────────────────────────────────────────

/// Summary of what is available to import, produced once when the
/// migration step opens so the UI has stable per-category counts.
#[derive(Debug, Clone, Default)]
pub struct DiscoverySnapshot {
    /// Items discovered under `~/.claude/skills`.
    pub claude_skills: Vec<DiscoveredItem>,
    /// Items discovered under `~/.claude/agents`.
    pub claude_agents: Vec<DiscoveredItem>,
    /// Items discovered under `~/.claude/commands`.
    pub claude_commands: Vec<DiscoveredItem>,
    /// Items discovered under `~/.codex/skills`.
    pub codex_skills: Vec<DiscoveredItem>,
    /// Items discovered under `~/.codex/prompts`.
    pub codex_prompts: Vec<DiscoveredItem>,
}

impl DiscoverySnapshot {
    /// The discovered items for one category.
    pub fn for_items(&self, category: ImportCategory) -> &[DiscoveredItem] {
        match category {
            ImportCategory::ClaudeSkills => &self.claude_skills,
            ImportCategory::ClaudeAgents => &self.claude_agents,
            ImportCategory::ClaudeCommands => &self.claude_commands,
            ImportCategory::CodexSkills => &self.codex_skills,
            ImportCategory::CodexPrompts => &self.codex_prompts,
        }
    }

    /// How many items one category holds.
    pub fn count(&self, category: ImportCategory) -> usize {
        self.for_items(category).len()
    }

    /// How many items every category holds together.
    pub fn total(&self) -> usize {
        ImportCategory::ALL.iter().map(|c| self.count(*c)).sum()
    }

    /// Categories that actually discovered something, in checklist order.
    pub fn non_empty_categories(&self) -> Vec<ImportCategory> {
        ImportCategory::ALL
            .iter()
            .copied()
            .filter(|category| self.count(*category) > 0)
            .collect()
    }

    /// Scan every category against the given home directory.
    pub fn discover(home: &Path) -> Self {
        Self {
            claude_skills: scan_category(home, ImportCategory::ClaudeSkills),
            claude_agents: scan_category(home, ImportCategory::ClaudeAgents),
            claude_commands: scan_category(home, ImportCategory::ClaudeCommands),
            codex_skills: scan_category(home, ImportCategory::CodexSkills),
            codex_prompts: scan_category(home, ImportCategory::CodexPrompts),
        }
    }

    /// Scan every category against the current user's home directory.
    /// Returns an empty snapshot when no home directory is set.
    pub fn discover_home() -> Self {
        home_dir()
            .map(|home| Self::discover(&home))
            .unwrap_or_default()
    }
}

// ── Copying ────────────────────────────────────────────────────────

/// Per-category import result — what was copied and what was
/// skipped. The UI surfaces this as a status banner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    /// Files newly written into the config home.
    pub copied: usize,
    /// Files left alone because the destination already existed.
    pub skipped_existing: usize,
    /// Files that could not be read, created, or copied.
    pub failed: usize,
}

impl ImportSummary {
    /// Fold another summary's counts into this one.
    pub fn merge(&mut self, other: &Self) {
        self.copied += other.copied;
        self.skipped_existing += other.skipped_existing;
        self.failed += other.failed;
    }
}

/// Copy the selected categories into `config_home`. Skills go to
/// `config_home/skills/<name>/SKILL.md`, commands to
/// `config_home/commands/<name>.md`, and agents to
/// `config_home/agents/<name>.md`. Existing destinations are left
/// alone so re-running the import is idempotent.
pub fn perform_migration(
    config_home: &Path,
    snapshot: &DiscoverySnapshot,
    selected: &[ImportCategory],
) -> ImportSummary {
    let mut total = ImportSummary::default();
    for category in selected {
        let items = snapshot.for_items(*category);
        if items.is_empty() {
            continue;
        }
        let summary = match category {
            ImportCategory::ClaudeSkills | ImportCategory::CodexSkills => {
                copy_skills(config_home, items)
            }
            ImportCategory::ClaudeAgents | ImportCategory::CodexPrompts => {
                copy_agents(config_home, items)
            }
            ImportCategory::ClaudeCommands => copy_commands(config_home, items),
        };
        total.merge(&summary);
    }
    total
}

fn copy_skills(config_home: &Path, items: &[DiscoveredItem]) -> ImportSummary {
    let skills_dir = config_home.join("skills");
    if let Err(err) = fs::create_dir_all(&skills_dir) {
        tracing::warn!(
            dir = %skills_dir.display(),
            error = %err,
            "migration: failed to create skills dir"
        );
        return ImportSummary {
            failed: items.len(),
            ..Default::default()
        };
    }
    let mut summary = ImportSummary::default();
    for item in items {
        let dest_dir = skills_dir.join(&item.name);
        let dest_file = dest_dir.join("SKILL.md");
        if dest_file.exists() {
            summary.skipped_existing += 1;
            continue;
        }
        if let Err(err) = fs::create_dir_all(&dest_dir) {
            tracing::warn!(
                dir = %dest_dir.display(),
                error = %err,
                "migration: failed to create skill dir"
            );
            summary.failed += 1;
            continue;
        }
        match fs::copy(&item.source, &dest_file) {
            Ok(_) => summary.copied += 1,
            Err(err) => {
                tracing::warn!(
                    source = %item.source.display(),
                    dest = %dest_file.display(),
                    error = %err,
                    "migration: failed to copy skill"
                );
                summary.failed += 1;
            }
        }
    }
    summary
}

fn copy_agents(config_home: &Path, items: &[DiscoveredItem]) -> ImportSummary {
    let agents_dir = config_home.join("agents");
    if let Err(err) = fs::create_dir_all(&agents_dir) {
        tracing::warn!(
            dir = %agents_dir.display(),
            error = %err,
            "migration: failed to create agents dir"
        );
        return ImportSummary {
            failed: items.len(),
            ..Default::default()
        };
    }
    let mut summary = ImportSummary::default();
    for item in items {
        let dest = agents_dir.join(format!("{}.md", item.name));
        if dest.exists() {
            summary.skipped_existing += 1;
            continue;
        }
        match fs::copy(&item.source, &dest) {
            Ok(_) => summary.copied += 1,
            Err(err) => {
                tracing::warn!(
                    source = %item.source.display(),
                    dest = %dest.display(),
                    error = %err,
                    "migration: failed to copy agent"
                );
                summary.failed += 1;
            }
        }
    }
    summary
}

fn copy_commands(config_home: &Path, items: &[DiscoveredItem]) -> ImportSummary {
    let commands_dir = config_home.join("commands");
    if let Err(err) = fs::create_dir_all(&commands_dir) {
        tracing::warn!(
            dir = %commands_dir.display(),
            error = %err,
            "migration: failed to create commands dir"
        );
        return ImportSummary {
            failed: items.len(),
            ..Default::default()
        };
    }
    let mut summary = ImportSummary::default();
    for item in items {
        let dest = commands_dir.join(format!("{}.md", item.name));
        if dest.exists() {
            summary.skipped_existing += 1;
            continue;
        }
        match fs::copy(&item.source, &dest) {
            Ok(_) => summary.copied += 1,
            Err(err) => {
                tracing::warn!(
                    source = %item.source.display(),
                    dest = %dest.display(),
                    error = %err,
                    "migration: failed to copy command"
                );
                summary.failed += 1;
            }
        }
    }
    summary
}

// ── Home dir lookup ────────────────────────────────────────────────

/// Cross-platform home directory lookup, resolved the way every other `~`
/// in the process is (see [`rebon_session::platform_home_dir`]).
pub fn home_dir() -> Option<PathBuf> {
    rebon_session::platform_home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn temp_home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn scans_claude_skills_directory() {
        let home = temp_home();
        let root = home.path();
        write(
            &root.join(".claude/skills/review.md"),
            "---\nname: review\n---\n",
        );
        write(
            &root.join(".claude/skills/other-pack/SKILL.md"),
            "---\nname: other-pack\n---\n",
        );
        let items = scan_category(root, ImportCategory::ClaudeSkills);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["other-pack", "review"]);
    }

    #[test]
    fn scans_claude_agents_recursively() {
        let home = temp_home();
        let root = home.path();
        write(&root.join(".claude/agents/planner.md"), "x");
        write(
            &root.join(".claude/agents/zcf/common/get-current-datetime.md"),
            "x",
        );
        let items = scan_category(root, ImportCategory::ClaudeAgents);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"planner"));
        assert!(names.contains(&"get-current-datetime"));
    }

    #[test]
    fn scans_claude_commands_recursively() {
        let home = temp_home();
        let root = home.path();
        write(&root.join(".claude/commands/review.md"), "review");
        write(&root.join(".claude/commands/git/commit.md"), "commit");

        let items = scan_category(root, ImportCategory::ClaudeCommands);
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();

        assert_eq!(names, vec!["commit", "review"]);
    }

    #[test]
    fn scan_preserves_unicode_names_and_disambiguates_collisions() {
        let home = temp_home();
        let root = home.path();
        write(&root.join(".claude/agents/a/代码审查.md"), "a");
        write(&root.join(".claude/agents/b/代码审查.md"), "b");

        let items = scan_category(root, ImportCategory::ClaudeAgents);
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();

        assert_eq!(names, vec!["代码审查", "代码审查-2"]);
    }

    #[test]
    fn missing_directory_yields_empty() {
        let home = temp_home();
        let items = scan_category(home.path(), ImportCategory::CodexSkills);
        assert!(items.is_empty());
    }

    #[test]
    fn snapshot_counts_each_category() {
        let home = temp_home();
        let root = home.path();
        write(&root.join(".claude/skills/a.md"), "a");
        write(&root.join(".claude/agents/b.md"), "b");
        write(&root.join(".claude/commands/d.md"), "d");
        write(&root.join(".codex/prompts/c.md"), "c");
        let snap = DiscoverySnapshot::discover(root);
        assert_eq!(snap.count(ImportCategory::ClaudeSkills), 1);
        assert_eq!(snap.count(ImportCategory::ClaudeAgents), 1);
        assert_eq!(snap.count(ImportCategory::ClaudeCommands), 1);
        assert_eq!(snap.count(ImportCategory::CodexSkills), 0);
        assert_eq!(snap.count(ImportCategory::CodexPrompts), 1);
        assert_eq!(snap.total(), 4);
    }

    #[test]
    fn snapshot_lists_only_non_empty_categories_in_checklist_order() {
        let home = temp_home();
        let root = home.path();
        write(&root.join(".claude/skills/a.md"), "a");
        write(&root.join(".codex/prompts/c.md"), "c");

        let snap = DiscoverySnapshot::discover(root);

        assert_eq!(
            snap.non_empty_categories(),
            vec![ImportCategory::ClaudeSkills, ImportCategory::CodexPrompts]
        );
        assert!(DiscoverySnapshot::default()
            .non_empty_categories()
            .is_empty());
    }

    #[test]
    fn migration_copies_skills_into_skill_md() {
        let home = temp_home();
        let src = home.path();
        write(&src.join(".claude/skills/review.md"), "body");
        let snap = DiscoverySnapshot::discover(src);

        let config = tempfile::tempdir().unwrap();
        let summary = perform_migration(config.path(), &snap, &[ImportCategory::ClaudeSkills]);
        assert_eq!(summary.copied, 1);
        let skill_md = config.path().join("skills/review/SKILL.md");
        assert_eq!(fs::read_to_string(&skill_md).unwrap(), "body");
    }

    #[test]
    fn migration_copies_agents_flat() {
        let home = temp_home();
        let src = home.path();
        write(&src.join(".claude/agents/zcf/planner.md"), "plan");
        let snap = DiscoverySnapshot::discover(src);

        let config = tempfile::tempdir().unwrap();
        let summary = perform_migration(config.path(), &snap, &[ImportCategory::ClaudeAgents]);
        assert_eq!(summary.copied, 1);
        let agent = config.path().join("agents/planner.md");
        assert_eq!(fs::read_to_string(&agent).unwrap(), "plan");
    }

    #[test]
    fn migration_copies_commands_flat() {
        let home = temp_home();
        let src = home.path();
        write(&src.join(".claude/commands/git/commit.md"), "commit");
        let snap = DiscoverySnapshot::discover(src);

        let config = tempfile::tempdir().unwrap();
        let first = perform_migration(config.path(), &snap, &[ImportCategory::ClaudeCommands]);
        let second = perform_migration(config.path(), &snap, &[ImportCategory::ClaudeCommands]);
        assert_eq!(first.copied, 1);
        assert_eq!(second.skipped_existing, 1);
        let command = config.path().join("commands/commit.md");
        assert_eq!(fs::read_to_string(&command).unwrap(), "commit");
    }

    #[test]
    fn migration_is_idempotent() {
        let home = temp_home();
        let src = home.path();
        write(&src.join(".codex/prompts/commit.md"), "v1");
        let snap = DiscoverySnapshot::discover(src);

        let config = tempfile::tempdir().unwrap();
        let first = perform_migration(config.path(), &snap, &[ImportCategory::CodexPrompts]);
        let second = perform_migration(config.path(), &snap, &[ImportCategory::CodexPrompts]);
        assert_eq!(first.copied, 1);
        assert_eq!(second.copied, 0);
        assert_eq!(second.skipped_existing, 1);
    }

    #[test]
    fn migration_of_an_unselected_category_copies_nothing() {
        let home = temp_home();
        let src = home.path();
        write(&src.join(".claude/skills/review.md"), "body");
        write(&src.join(".claude/agents/planner.md"), "plan");
        let snap = DiscoverySnapshot::discover(src);

        let config = tempfile::tempdir().unwrap();
        let summary = perform_migration(config.path(), &snap, &[ImportCategory::ClaudeSkills]);

        assert_eq!(summary.copied, 1);
        assert!(!config.path().join("agents/planner.md").exists());
    }

    #[test]
    fn category_display_strings_pair_source_with_destination() {
        for category in ImportCategory::ALL {
            assert!(category.label().contains(category.source_display()));
            assert!(category.dest_display().ends_with(category.dest_label()));
        }
        assert_eq!(
            ImportCategory::CodexPrompts.dest_display(),
            "~/.rebon/agents"
        );
    }

    #[test]
    fn sanitize_name_strips_unsafe_chars_without_losing_unicode() {
        assert_eq!(sanitize_name("foo bar"), "foo-bar");
        assert_eq!(sanitize_name("safe_name-1"), "safe_name-1");
        assert_eq!(sanitize_name("代码 审查"), "代码-审查");
        assert_eq!(sanitize_name("??"), "imported");
    }
}
