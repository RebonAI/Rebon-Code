//! Progressive skill discovery — I/O layer and session state.
//!
//! Bridges the state-machine primitives in [`crate::skills`] with the
//! runtime: reads `SKILL.md` files from disk, populates the
//! [`SkillRegistry`], and performs
//! dynamic discovery when file-operation tools touch new paths.
//!
//! # Lifecycle
//!
//! 1. **Startup** — [`load_startup_skills`] registers compile-time
//!    bundled skills, then scans standard directories (user, project)
//!    and registers unconditional disk-based skills.
//! 2. **Per-tool-round** — [`SkillState::on_files_touched`] checks
//!    conditional activation and discovers new `.rebon/skills/` dirs.
//! 3. **First invocation** — bundled skill SKILL.md files are lazily
//!    extracted to a per-process cache dir via
//!    [`bundled_cache`](crate::bundled_cache).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::skill::{Skill, SkillRegistry, SkillSource};
use crate::skills::{
    activate_conditional_skills, candidate_skill_dirs, create_skill_command,
    deduplicate_by_file_identity, parse_skill_frontmatter_fields, parse_skill_paths,
    skill_command_to_entry, split_conditional_skills, transform_skill_files, CommandSource,
    FrontmatterValue, LoadedFrom, MarkdownFileEntry, RawFrontmatter, SkillCommandDef,
    SkillWithPath,
};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Parameters for skill loading.
#[derive(Debug, Clone)]
pub struct SkillLoaderConfig {
    /// `~/.rebon` (or `$REBON_CONFIG_DIR`).
    pub config_home: String,
    /// Working directory of the session.
    pub cwd: String,
    /// Session ID for `${CLAUDE_SESSION_ID}` expansion.
    pub session_id: String,
    /// Whether `.claude` and `.codex` compatibility directories are scanned.
    pub claude_codex_fallback_enabled: bool,
    /// Additional plugin-provided skill directories.
    pub plugin_skill_dirs: Vec<String>,
    /// Additional plugin-provided command/workflow directories.
    pub plugin_command_dirs: Vec<String>,
    /// Skills compiled plugins registered on the `skill-bundles` seat. Put
    /// on disk under `config_home` and loaded as plugin skills.
    pub skill_bundles: Vec<rebon_core::skill_seat::SkillBundle>,
}

// ---------------------------------------------------------------------------
// SkillState — mutable per-session discovery state
// ---------------------------------------------------------------------------

/// Mutable state for progressive skill discovery, shared between
/// the query executor (startup) and the tool-dispatch hook
/// (dynamic).
pub struct SkillState {
    /// Conditional skills waiting for path-based activation.
    conditional: Vec<SkillCommandDef>,
    /// Names of skills already activated this session.
    already_activated: Vec<String>,
    /// Directories already scanned (canonical paths).
    known_dirs: HashSet<String>,
    /// Session ID for prompt expansion.
    session_id: String,
    /// Working directory.
    cwd: String,
    /// Whether dynamic discovery may scan `.claude` and `.codex` directories.
    claude_codex_fallback_enabled: bool,
}

impl SkillState {
    /// Create an empty per-session discovery state. Runtime loaders populate
    /// the same fields after scanning; lightweight hosts and tests can use
    /// this without inventing a shared mutable placeholder.
    pub fn empty(session_id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            conditional: Vec::new(),
            already_activated: Vec::new(),
            known_dirs: HashSet::new(),
            session_id: session_id.into(),
            cwd: cwd.into(),
            claude_codex_fallback_enabled: false,
        }
    }

    /// Immutable session metadata used for prompt expansion and relative paths.
    pub fn session_binding(&self) -> (&str, &str) {
        (&self.session_id, &self.cwd)
    }

    /// Handle file paths touched by tool calls.
    ///
    /// 1. Activates conditional skills whose `paths` patterns match.
    /// 2. Discovers new `.rebon/skills/` directories by walking up
    ///    from each touched path.
    ///
    /// Returns `true` if any new skills were registered.
    pub async fn on_files_touched(
        state: &Arc<Mutex<Self>>,
        touched_paths: &[String],
        registry: &SkillRegistry,
    ) -> bool {
        if touched_paths.is_empty() {
            return false;
        }

        let mut changed = false;

        // 1. Conditional activation (synchronous — pure logic).
        {
            let mut guard = state.lock().expect("skill state");
            let cwd = guard.cwd.clone();
            let session_id = guard.session_id.clone();

            let relative: Vec<String> = touched_paths
                .iter()
                .filter_map(|p| compute_relative_path(p, &cwd))
                .collect();
            let refs: Vec<&str> = relative.iter().map(String::as_str).collect();

            let activated = activate_conditional_skills(&mut guard.conditional, &refs);
            for skill in &activated {
                let entry = skill_command_to_entry(skill, &session_id);
                registry.register(entry_to_skill(
                    entry,
                    skill_source_from_command(skill.source),
                    skill,
                ));
                guard.already_activated.push(skill.name.clone());
                changed = true;
            }
        }

        // 2. Directory discovery (async I/O — release lock first).
        let (cwd, known_dirs, session_id, claude_codex_fallback_enabled) = {
            let guard = state.lock().expect("skill state");
            (
                guard.cwd.clone(),
                guard.known_dirs.clone(),
                guard.session_id.clone(),
                guard.claude_codex_fallback_enabled,
            )
        };

        let mut new_dirs = Vec::new();
        for path in touched_paths {
            let normalized = path.replace('\\', "/");
            let cwd_norm = cwd.replace('\\', "/");
            for dir in candidate_skill_dirs(&normalized, &cwd_norm) {
                if known_dirs.contains(&dir) {
                    continue;
                }
                let disk_path = PathBuf::from(dir.replace('/', std::path::MAIN_SEPARATOR_STR));
                if tokio::fs::metadata(&disk_path).await.is_ok() {
                    new_dirs.push(dir.clone());
                }
                if claude_codex_fallback_enabled {
                    for compat_dir in compat_skill_dirs_for_rebon_dir(&dir) {
                        if known_dirs.contains(&compat_dir) || new_dirs.contains(&compat_dir) {
                            continue;
                        }
                        let disk_path =
                            PathBuf::from(compat_dir.replace('/', std::path::MAIN_SEPARATOR_STR));
                        if tokio::fs::metadata(&disk_path).await.is_ok() {
                            new_dirs.push(compat_dir);
                        }
                    }
                }
            }
        }

        if !new_dirs.is_empty() {
            for dir in &new_dirs {
                let disk_path = dir.replace('/', std::path::MAIN_SEPARATOR_STR);
                let skills =
                    load_skills_from_skills_dir(&disk_path, CommandSource::ProjectSettings).await;
                if !skills.is_empty() {
                    let identities = resolve_identities(&skills).await;
                    let deduped = deduplicate_by_file_identity(&skills, &identities);
                    let split = split_conditional_skills(deduped.skills, &[]);

                    let mut guard = state.lock().expect("skill state");
                    for skill in split.unconditional {
                        let entry = skill_command_to_entry(&skill, &session_id);
                        registry.register(entry_to_skill(
                            entry,
                            skill_source_from_command(skill.source),
                            &skill,
                        ));
                        changed = true;
                    }
                    guard.conditional.extend(split.conditional);
                    guard.known_dirs.insert(dir.clone());
                }
            }

            // Mark new dirs as known even if empty.
            let mut guard = state.lock().expect("skill state");
            for dir in &new_dirs {
                guard.known_dirs.insert(dir.clone());
            }
        }

        changed
    }
}

// ---------------------------------------------------------------------------
// Startup loading
// ---------------------------------------------------------------------------

fn standard_startup_dirs(
    config: &SkillLoaderConfig,
) -> Vec<(String, CommandSource, StartupDirKind)> {
    let mut dirs = Vec::new();

    dirs.push((
        format!("{}/skills", normalize_separators(&config.config_home)),
        CommandSource::UserSettings,
        StartupDirKind::Skills,
    ));
    if config.claude_codex_fallback_enabled {
        if let Some(dir) = user_home_compat_dir(".claude", "skills") {
            dirs.push((dir, CommandSource::UserSettings, StartupDirKind::Skills));
        }
        if let Some(dir) = user_home_compat_dir(".codex", "skills") {
            dirs.push((dir, CommandSource::UserSettings, StartupDirKind::Skills));
        }
    }

    dirs.push((
        format!("{}/.rebon/skills", normalize_separators(&config.cwd)),
        CommandSource::ProjectSettings,
        StartupDirKind::Skills,
    ));
    if config.claude_codex_fallback_enabled {
        dirs.push((
            format!("{}/.claude/skills", normalize_separators(&config.cwd)),
            CommandSource::ProjectSettings,
            StartupDirKind::Skills,
        ));
        dirs.push((
            format!("{}/.codex/skills", normalize_separators(&config.cwd)),
            CommandSource::ProjectSettings,
            StartupDirKind::Skills,
        ));
    }

    dirs.push((
        format!("{}/commands", normalize_separators(&config.config_home)),
        CommandSource::UserSettings,
        StartupDirKind::Commands,
    ));
    if config.claude_codex_fallback_enabled {
        if let Some(dir) = user_home_compat_dir(".claude", "commands") {
            dirs.push((dir, CommandSource::UserSettings, StartupDirKind::Commands));
        }
        if let Some(dir) = user_home_compat_dir(".codex", "commands") {
            dirs.push((dir, CommandSource::UserSettings, StartupDirKind::Commands));
        }
    }

    dirs.push((
        format!("{}/.rebon/commands", normalize_separators(&config.cwd)),
        CommandSource::ProjectSettings,
        StartupDirKind::Commands,
    ));
    if config.claude_codex_fallback_enabled {
        dirs.push((
            format!("{}/.claude/commands", normalize_separators(&config.cwd)),
            CommandSource::ProjectSettings,
            StartupDirKind::Commands,
        ));
        dirs.push((
            format!("{}/.codex/commands", normalize_separators(&config.cwd)),
            CommandSource::ProjectSettings,
            StartupDirKind::Commands,
        ));
    }

    for dir in &config.plugin_skill_dirs {
        dirs.push((
            normalize_separators(dir),
            CommandSource::Plugin,
            StartupDirKind::Skills,
        ));
    }
    for dir in &config.plugin_command_dirs {
        dirs.push((
            normalize_separators(dir),
            CommandSource::Plugin,
            StartupDirKind::Commands,
        ));
    }

    dirs
}

#[derive(Clone, Copy)]
enum StartupDirKind {
    Skills,
    Commands,
}

/// Load skills from standard directories and register them.
///
/// Returns an `Arc<Mutex<SkillState>>` holding the conditional
/// skills pool and discovery bookkeeping.
pub async fn load_startup_skills(
    config: &SkillLoaderConfig,
    registry: &SkillRegistry,
) -> Arc<Mutex<SkillState>> {
    // Register compile-time bundled skills first so they are
    // immediately available even before disk scanning completes.
    crate::bundled_cache::register_built_in_skills(registry);

    let mut all_skills: Vec<SkillWithPath> = Vec::new();
    let mut known_dirs = HashSet::new();

    let bundle_dirs = crate::skill_bundles::materialize_skill_bundles(
        Path::new(&config.config_home),
        &config.skill_bundles,
    )
    .into_iter()
    .map(|dir| {
        (
            normalize_separators(&dir.to_string_lossy()),
            CommandSource::Plugin,
            StartupDirKind::Skills,
        )
    });
    for (dir, source, kind) in standard_startup_dirs(config).into_iter().chain(bundle_dirs) {
        let loaded = match kind {
            StartupDirKind::Skills => try_load_dir(&dir, source).await,
            StartupDirKind::Commands => try_load_commands_dir(&dir, source).await,
        };
        if let Some(skills) = loaded {
            known_dirs.insert(dir);
            all_skills.extend(skills);
        }
    }

    // Deduplicate by canonical path.
    let identities = resolve_identities(&all_skills).await;
    let deduped = deduplicate_by_file_identity(&all_skills, &identities);

    // Split conditional / unconditional.
    let split = split_conditional_skills(deduped.skills, &[]);

    // Register unconditional skills.
    for skill in &split.unconditional {
        let entry = skill_command_to_entry(skill, &config.session_id);
        registry.register(entry_to_skill(
            entry,
            skill_source_from_command(skill.source),
            skill,
        ));
    }

    tracing::debug!(
        unconditional = split.unconditional.len(),
        conditional = split.conditional.len(),
        dirs = known_dirs.len(),
        "skill loader: startup complete"
    );

    Arc::new(Mutex::new(SkillState {
        conditional: split.conditional,
        already_activated: Vec::new(),
        known_dirs,
        session_id: config.session_id.clone(),
        cwd: config.cwd.clone(),
        claude_codex_fallback_enabled: config.claude_codex_fallback_enabled,
    }))
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Try loading a skills directory; returns `None` if the directory
/// doesn't exist or can't be read.
async fn try_load_dir(dir: &str, source: CommandSource) -> Option<Vec<SkillWithPath>> {
    if tokio::fs::metadata(dir).await.is_err() {
        return None;
    }
    let skills = load_skills_from_skills_dir(dir, source).await;
    Some(skills)
}

/// Load skills from a `/skills/` directory (directory format:
/// `skill-name/SKILL.md`).
async fn load_skills_from_skills_dir(base_path: &str, source: CommandSource) -> Vec<SkillWithPath> {
    let Ok(mut entries) = tokio::fs::read_dir(base_path).await else {
        return Vec::new();
    };

    let mut results = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();

        // Only directory format: skill-name/SKILL.md
        let is_dir = match tokio::fs::metadata(&path).await {
            Ok(m) => m.is_dir(),
            Err(_) => continue,
        };
        if !is_dir {
            continue;
        }

        let skill_file = path.join("SKILL.md");
        let Ok(content) = tokio::fs::read_to_string(&skill_file).await else {
            continue;
        };

        let skill_name = entry.file_name().to_string_lossy().into_owned();
        let (frontmatter, markdown_content) = split_frontmatter(&content);
        let parsed =
            parse_skill_frontmatter_fields(&frontmatter, &markdown_content, &skill_name, "Skill");

        if let Some(ref warning) = parsed.effort_parse_warning {
            tracing::debug!("{}", warning);
        }

        let base_dir = path.to_string_lossy().into_owned();
        let paths = parse_skill_paths(frontmatter.get("paths"));
        let cmd = create_skill_command(
            &skill_name,
            &parsed,
            &markdown_content,
            source,
            LoadedFrom::Skills,
            Some(&base_dir),
            paths,
        );

        results.push(SkillWithPath {
            skill: cmd,
            file_path: skill_file.to_string_lossy().into_owned(),
        });
    }

    results
}

/// Try loading a legacy `/commands/` directory.
async fn try_load_commands_dir(dir: &str, source: CommandSource) -> Option<Vec<SkillWithPath>> {
    if tokio::fs::metadata(dir).await.is_err() {
        return None;
    }
    let skills = load_skills_from_commands_dir(dir, source).await;
    Some(skills)
}

/// Load from a legacy `/commands/` directory (supports both
/// `name.md` and `name/SKILL.md`).
async fn load_skills_from_commands_dir(
    base_path: &str,
    source: CommandSource,
) -> Vec<SkillWithPath> {
    let Ok(mut entries) = tokio::fs::read_dir(base_path).await else {
        return Vec::new();
    };

    // Collect all .md files.
    let mut md_files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_file() {
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                md_files.push(MarkdownFileEntry {
                    base_dir: normalize_separators(base_path),
                    file_path: normalize_separators(&path.to_string_lossy()),
                });
            }
        } else if meta.is_dir() {
            // Check for SKILL.md inside
            let skill_file = path.join("SKILL.md");
            if tokio::fs::metadata(&skill_file).await.is_ok() {
                md_files.push(MarkdownFileEntry {
                    base_dir: normalize_separators(base_path),
                    file_path: normalize_separators(&skill_file.to_string_lossy()),
                });
            }
        }
    }

    // Group and filter (SKILL.md takes priority).
    let filtered = transform_skill_files(md_files);

    let mut results = Vec::new();
    for entry in filtered {
        let Ok(content) = tokio::fs::read_to_string(&entry.file_path).await else {
            continue;
        };

        let skill_name = crate::skills::get_command_name(&entry.file_path, &entry.base_dir);
        let (frontmatter, markdown_content) = split_frontmatter(&content);
        let parsed = parse_skill_frontmatter_fields(
            &frontmatter,
            &markdown_content,
            &skill_name,
            "Custom command",
        );

        let base_dir = Path::new(&entry.file_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned());
        let paths = parse_skill_paths(frontmatter.get("paths"));
        let cmd = create_skill_command(
            &skill_name,
            &parsed,
            &markdown_content,
            source,
            LoadedFrom::CommandsDeprecated,
            base_dir.as_deref(),
            paths,
        );

        results.push(SkillWithPath {
            skill: cmd,
            file_path: entry.file_path,
        });
    }

    results
}

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Split markdown into YAML frontmatter and body.
fn split_frontmatter(content: &str) -> (RawFrontmatter, String) {
    let trimmed = content.trim_start_matches('\u{feff}'); // strip BOM
    if !trimmed.starts_with("---") {
        return (RawFrontmatter::new(), content.to_string());
    }

    let after_opening = &trimmed[3..];
    let after_opening = after_opening.strip_prefix('\r').unwrap_or(after_opening);
    let after_opening = after_opening.strip_prefix('\n').unwrap_or(after_opening);

    let closing_pos = after_opening.find("\n---").map(|p| p + 1);
    let (yaml_block, body) = match closing_pos {
        Some(pos) => {
            let yaml = &after_opening[..pos];
            let rest = &after_opening[pos + 3..];
            let rest = rest.strip_prefix('\r').unwrap_or(rest);
            let rest = rest.strip_prefix('\n').unwrap_or(rest);
            (yaml, rest.to_string())
        }
        None => return (RawFrontmatter::new(), content.to_string()),
    };

    let fm = parse_skill_yaml(yaml_block);
    (fm, body)
}

/// Parse a YAML-like frontmatter block into [`RawFrontmatter`].
///
/// Handles the subset used by skill frontmatter:
/// - `key: value` (string, bool, integer)
/// - `key:` + indented `- item` lines (string list)
/// - `key:` + indented `subkey: value` lines (map)
fn parse_skill_yaml(yaml: &str) -> RawFrontmatter {
    let mut result = RawFrontmatter::new();
    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        // Top-level key: value
        if let Some((key, rest)) = trimmed.split_once(':') {
            let key = key.trim().to_string();
            if key.is_empty() {
                i += 1;
                continue;
            }

            let value_part = rest.trim();

            if value_part.is_empty() {
                // Look ahead for indented lines (list or map).
                let mut list_items = Vec::new();
                let mut map_entries = std::collections::HashMap::new();
                let mut is_list = false;
                let mut is_map = false;

                let mut j = i + 1;
                while j < lines.len() {
                    let next = lines[j];
                    let next_trimmed = next.trim();
                    if next_trimmed.is_empty() {
                        j += 1;
                        continue;
                    }
                    let indent = next.len() - next.trim_start().len();
                    if indent == 0 {
                        break;
                    }
                    if next_trimmed.starts_with("- ") {
                        is_list = true;
                        let item = strip_yaml_quotes(next_trimmed[2..].trim());
                        list_items.push(item.to_string());
                    } else if let Some((sub_key, sub_val)) = next_trimmed.split_once(':') {
                        is_map = true;
                        let sub_key = sub_key.trim().to_string();
                        let sub_val = strip_yaml_quotes(sub_val.trim());
                        map_entries.insert(sub_key, parse_scalar(sub_val));
                    }
                    j += 1;
                }

                if is_list {
                    result.insert(key, FrontmatterValue::StringList(list_items));
                } else if is_map {
                    result.insert(key, FrontmatterValue::Map(map_entries));
                } else {
                    result.insert(key, FrontmatterValue::Null);
                }
                i = j;
                continue;
            }

            // Inline value
            result.insert(key, parse_scalar(value_part));
        }

        i += 1;
    }

    result
}

fn parse_scalar(s: &str) -> FrontmatterValue {
    let s = strip_yaml_quotes(s);
    match s.to_lowercase().as_str() {
        "true" | "yes" => return FrontmatterValue::Bool(true),
        "false" | "no" => return FrontmatterValue::Bool(false),
        "null" | "~" => return FrontmatterValue::Null,
        _ => {}
    }
    if s.is_empty() {
        return FrontmatterValue::Null;
    }
    if let Ok(n) = s.parse::<i64>() {
        return FrontmatterValue::Integer(n);
    }
    FrontmatterValue::String(s.to_string())
}

fn strip_yaml_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract file paths from tool-use blocks that perform file
/// operations (Read, Write, Edit, Glob, Grep).
pub fn extract_file_paths(tool_uses: &[(&str, &serde_json::Value)]) -> Vec<String> {
    let mut paths = Vec::new();
    for &(name, input) in tool_uses {
        let obj = match input.as_object() {
            Some(o) => o,
            None => continue,
        };
        // The file tools name their target in a field they each declare;
        // Glob and Grep take a directory to search under `path`.
        if let Some(field) = rebon_tools_core::file_target_field_for_name(name) {
            if let Some(p) = obj.get(field).and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
        } else if rebon_tools_core::tool_kind_for_name(name) == rebon_tools_core::ToolKind::Search {
            if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
        }
    }
    paths
}

/// Compute a path relative to `cwd`. Returns `None` for paths
/// outside or equal to `cwd`.
pub fn compute_relative_path(file_path: &str, cwd: &str) -> Option<String> {
    // Normalize separators.
    let fp = file_path.replace('\\', "/");
    let cwd = cwd.replace('\\', "/");
    let cwd = cwd.trim_end_matches('/');
    let prefix = format!("{cwd}/");
    fp.strip_prefix(&prefix).map(String::from)
}

fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

fn compat_skill_dirs_for_rebon_dir(dir: &str) -> Vec<String> {
    match dir.strip_suffix("/.rebon/skills") {
        Some(base) => vec![
            format!("{base}/.claude/skills"),
            format!("{base}/.codex/skills"),
        ],
        None => Vec::new(),
    }
}

fn user_home_compat_dir(config_dir: &str, child: &str) -> Option<String> {
    let home = rebon_session::platform_home_dir()?;
    Some(
        home.join(config_dir)
            .join(child)
            .to_string_lossy()
            .replace('\\', "/"),
    )
}

/// Resolve canonical paths for deduplication.
async fn resolve_identities(skills: &[SkillWithPath]) -> Vec<Option<String>> {
    let mut ids = Vec::with_capacity(skills.len());
    for skill in skills {
        let canonical = tokio::fs::canonicalize(&skill.file_path)
            .await
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        ids.push(canonical);
    }
    ids
}

/// Convert a [`CommandSource`] to the runtime context bucket.
fn skill_source_from_command(source: CommandSource) -> SkillSource {
    match source {
        CommandSource::ProjectSettings => SkillSource::Project,
        CommandSource::UserSettings => SkillSource::User,
        CommandSource::PolicySettings => SkillSource::Managed,
        CommandSource::LocalSettings => SkillSource::Local,
        CommandSource::FlagSettings => SkillSource::Flag,
        CommandSource::Builtin | CommandSource::Bundled => SkillSource::BuiltIn,
        CommandSource::Plugin => SkillSource::Plugin,
        CommandSource::Mcp => SkillSource::Mcp,
    }
}

/// Convert a [`crate::skills::SkillRegistryEntry`] to a [`Skill`].
fn entry_to_skill(
    entry: crate::skills::SkillRegistryEntry,
    source: SkillSource,
    command: &SkillCommandDef,
) -> Skill {
    Skill {
        id: entry.id,
        title: entry.title,
        description: entry.description,
        prompt_template: entry.prompt_template,
        suggested_tools: entry.suggested_tools,
        source,
        argument_hint: command.argument_hint.clone(),
        argument_names: command.arg_names.clone(),
        skill_root: command.skill_root.clone(),
        user_invocable: command.user_invocable,
        disable_model_invocation: command.disable_model_invocation,
        required_tools: command.required_tools.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_startup_skills_scans_project_codex_skills_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let cwd = temp.path();
        write_skill(
            &cwd.join(".codex").join("skills"),
            "patent-search",
            "Patent search workflow",
            "Run a patent search.",
        );

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: cwd.join("config").to_string_lossy().into_owned(),
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: true,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;

        assert!(registry.get("patent-search").is_some());
    }

    /// A bundle a compiled plugin registered is written under the config
    /// home and loads as a plugin skill: base-directory line pointing at the
    /// written files, `required-tools` carried onto the registry entry.
    #[tokio::test]
    async fn load_startup_skills_loads_registered_skill_bundles() {
        const BUNDLE: rebon_core::skill_seat::SkillBundle = rebon_core::skill_seat::SkillBundle {
            name: "painter",
            files: &[
                (
                    "SKILL.md",
                    "---\nname: \"painter\"\ndescription: \"Paint things\"\nrequired-tools: ImageGen\n---\nSee references/tips.md.",
                ),
                ("references/tips.md", "tips"),
            ],
        };
        let temp = tempfile::TempDir::new().unwrap();
        let home = temp.path().join("config");

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: home.to_string_lossy().into_owned(),
                cwd: temp.path().join("project").to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: false,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: vec![BUNDLE],
            },
            &registry,
        )
        .await;

        let skill = registry.get("painter").expect("the bundle loads");
        assert_eq!(skill.source, SkillSource::Plugin);
        assert_eq!(skill.description, "Paint things");
        assert_eq!(skill.required_tools, vec!["ImageGen".to_string()]);
        let skill_dir = crate::skill_bundles::skill_bundles_root(&home)
            .join("painter")
            .join("painter");
        assert!(skill_dir.join("references").join("tips.md").is_file());
        let base_line = skill.prompt_template.lines().next().unwrap_or_default();
        assert!(
            base_line.starts_with("Base directory for this skill: ")
                && base_line
                    .replace('\\', "/")
                    .ends_with("skill-bundles/painter/painter"),
            "{base_line}"
        );

        // Without the registration the written directory is not loaded.
        let fresh = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: home.to_string_lossy().into_owned(),
                cwd: temp.path().join("project").to_string_lossy().into_owned(),
                session_id: "sess-2".into(),
                claude_codex_fallback_enabled: false,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &fresh,
        )
        .await;
        assert!(fresh.get("painter").is_none());
    }

    #[tokio::test]
    async fn load_startup_skills_scans_project_claude_skills_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let cwd = temp.path();
        write_skill(
            &cwd.join(".claude").join("skills"),
            "claude-review",
            "Claude review workflow",
            "Run a Claude review.",
        );

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: cwd.join("config").to_string_lossy().into_owned(),
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: true,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;

        assert!(registry.get("claude-review").is_some());
    }

    #[tokio::test]
    async fn load_startup_skills_scans_user_home_compat_dirs() {
        let temp = tempfile::TempDir::new().unwrap();
        let home = temp.path().join("home");
        write_skill(
            &home.join(".codex").join("skills"),
            "patent-search",
            "Patent search workflow",
            "Run a patent search.",
        );
        write_skill(
            &home.join(".claude").join("skills"),
            "claude-review",
            "Claude review workflow",
            "Run a Claude review.",
        );
        let commands_dir = home.join(".claude").join("commands");
        std::fs::create_dir_all(&commands_dir).unwrap();
        std::fs::write(
            commands_dir.join("triage.md"),
            "---\ndescription: Triage workflow\n---\nRun triage.",
        )
        .unwrap();
        // The compat directories hang off the platform home, which Windows
        // reads from `USERPROFILE` first; point both variables at the fixture.
        let _home_guard = EnvVarGuard::set("HOME", home.to_string_lossy().into_owned());
        let _profile_guard = EnvVarGuard::set("USERPROFILE", home.to_string_lossy().into_owned());

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: temp.path().join("config").to_string_lossy().into_owned(),
                cwd: temp.path().join("cwd").to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: true,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;

        assert!(registry.get("patent-search").is_some());
        assert!(registry.get("claude-review").is_some());
        assert!(registry.get("triage").is_some());
    }

    #[tokio::test]
    async fn load_startup_skills_scans_project_claude_commands_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let cwd = temp.path();
        let commands_dir = cwd.join(".claude").join("commands");
        std::fs::create_dir_all(&commands_dir).unwrap();
        std::fs::write(
            commands_dir.join("triage.md"),
            "---\ndescription: Triage workflow\n---\nRun triage.",
        )
        .unwrap();

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: cwd.join("config").to_string_lossy().into_owned(),
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: true,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;

        assert!(registry.get("triage").is_some());
    }

    #[tokio::test]
    async fn load_startup_skills_scans_project_codex_commands_dir_when_enabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let cwd = temp.path();
        write_command(
            &cwd.join(".codex").join("commands"),
            "codex-triage",
            "Codex triage workflow",
        );

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: cwd.join("config").to_string_lossy().into_owned(),
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: true,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;

        assert!(registry.get("codex-triage").is_some());
    }

    #[tokio::test]
    async fn load_startup_skills_ignores_claude_codex_dirs_when_disabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let cwd = temp.path();
        write_skill(
            &cwd.join(".claude").join("skills"),
            "claude-review",
            "Claude review workflow",
            "Run a Claude review.",
        );
        write_skill(
            &cwd.join(".codex").join("skills"),
            "codex-review",
            "Codex review workflow",
            "Run a Codex review.",
        );
        write_command(
            &cwd.join(".claude").join("commands"),
            "claude-triage",
            "Claude triage workflow",
        );
        write_command(
            &cwd.join(".codex").join("commands"),
            "codex-triage",
            "Codex triage workflow",
        );
        write_skill(
            &cwd.join(".rebon").join("skills"),
            "rebon-review",
            "Rebon review workflow",
            "Run a Rebon review.",
        );
        write_command(
            &cwd.join(".rebon").join("commands"),
            "rebon-triage",
            "Rebon triage workflow",
        );

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: cwd.join("config").to_string_lossy().into_owned(),
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: false,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;

        assert!(registry.get("claude-review").is_none());
        assert!(registry.get("codex-review").is_none());
        assert!(registry.get("claude-triage").is_none());
        assert!(registry.get("codex-triage").is_none());
        assert!(registry.get("rebon-review").is_some());
        assert!(registry.get("rebon-triage").is_some());
    }

    #[tokio::test]
    async fn dynamic_discovery_ignores_claude_codex_dirs_when_disabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let cwd = temp.path();
        let registry = SkillRegistry::new();
        let state = load_startup_skills(
            &SkillLoaderConfig {
                config_home: cwd.join("config").to_string_lossy().into_owned(),
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: false,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;
        let project = cwd.join("src");
        std::fs::create_dir_all(project.join(".rebon").join("skills")).unwrap();
        write_skill(
            &project.join(".claude").join("skills"),
            "late-claude-skill",
            "Late Claude skill",
            "Run the late skill.",
        );

        let touched = project.join("main.rs").to_string_lossy().into_owned();
        let changed = SkillState::on_files_touched(&state, &[touched], &registry).await;

        assert!(!changed);
        assert!(registry.get("late-claude-skill").is_none());
    }

    #[tokio::test]
    async fn dynamic_discovery_loads_claude_codex_dirs_when_enabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let cwd = temp.path();
        let registry = SkillRegistry::new();
        let state = load_startup_skills(
            &SkillLoaderConfig {
                config_home: cwd.join("config").to_string_lossy().into_owned(),
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: true,
                plugin_skill_dirs: Vec::new(),
                plugin_command_dirs: Vec::new(),
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;
        let project = cwd.join("src");
        std::fs::create_dir_all(project.join(".rebon").join("skills")).unwrap();
        write_skill(
            &project.join(".codex").join("skills"),
            "late-codex-skill",
            "Late Codex skill",
            "Run the late skill.",
        );

        let touched = project.join("main.rs").to_string_lossy().into_owned();
        let changed = SkillState::on_files_touched(&state, &[touched], &registry).await;

        assert!(changed);
        assert!(registry.get("late-codex-skill").is_some());
    }

    #[tokio::test]
    async fn load_startup_skills_scans_plugin_skill_and_command_dirs() {
        let temp = tempfile::TempDir::new().unwrap();
        let plugin = temp.path().join("plugin");
        write_skill(
            &plugin.join("skills"),
            "plugin-skill",
            "Plugin skill workflow",
            "Run plugin skill.",
        );
        let commands_dir = plugin.join("commands");
        std::fs::create_dir_all(&commands_dir).unwrap();
        std::fs::write(
            commands_dir.join("plugin-command.md"),
            "---\ndescription: Plugin command workflow\n---\nRun plugin command.",
        )
        .unwrap();

        let registry = SkillRegistry::new();
        load_startup_skills(
            &SkillLoaderConfig {
                config_home: temp.path().join("config").to_string_lossy().into_owned(),
                cwd: temp.path().join("cwd").to_string_lossy().into_owned(),
                session_id: "sess-1".into(),
                claude_codex_fallback_enabled: false,
                plugin_skill_dirs: vec![plugin.join("skills").to_string_lossy().into_owned()],
                plugin_command_dirs: vec![plugin.join("commands").to_string_lossy().into_owned()],
                skill_bundles: Vec::new(),
            },
            &registry,
        )
        .await;

        let skill = registry.get("plugin-skill").unwrap();
        assert!(matches!(skill.source, SkillSource::Plugin));
        let command = registry.get("plugin-command").unwrap();
        assert!(matches!(command.source, SkillSource::Plugin));
    }

    fn write_skill(base: &Path, name: &str, description: &str, body: &str) {
        let skill_dir = base.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
    }

    fn write_command(base: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(base).unwrap();
        std::fs::write(
            base.join(format!("{name}.md")),
            format!("---\ndescription: {description}\n---\nRun {name}."),
        )
        .unwrap();
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: String) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = self.previous.take() {
                    std::env::set_var(self.key, value);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    // -- YAML parsing --

    #[test]
    fn parse_scalar_types() {
        assert_eq!(parse_scalar("true"), FrontmatterValue::Bool(true));
        assert_eq!(parse_scalar("false"), FrontmatterValue::Bool(false));
        assert_eq!(parse_scalar("yes"), FrontmatterValue::Bool(true));
        assert_eq!(parse_scalar("no"), FrontmatterValue::Bool(false));
        assert_eq!(parse_scalar("42"), FrontmatterValue::Integer(42));
        assert_eq!(
            parse_scalar("hello"),
            FrontmatterValue::String("hello".into())
        );
        assert_eq!(parse_scalar("null"), FrontmatterValue::Null);
    }

    #[test]
    fn parse_yaml_flat_key_value() {
        let yaml = "name: My Skill\ndescription: Does things\nuser-invocable: false";
        let fm = parse_skill_yaml(yaml);
        assert_eq!(
            fm.get("name"),
            Some(&FrontmatterValue::String("My Skill".into()))
        );
        assert_eq!(
            fm.get("description"),
            Some(&FrontmatterValue::String("Does things".into()))
        );
        assert_eq!(
            fm.get("user-invocable"),
            Some(&FrontmatterValue::Bool(false))
        );
    }

    #[test]
    fn parse_yaml_list() {
        let yaml = "allowed-tools:\n  - Read\n  - Write\n  - Bash";
        let fm = parse_skill_yaml(yaml);
        assert_eq!(
            fm.get("allowed-tools"),
            Some(&FrontmatterValue::StringList(vec![
                "Read".into(),
                "Write".into(),
                "Bash".into()
            ]))
        );
    }

    #[test]
    fn parse_yaml_map() {
        let yaml = "hooks:\n  PreToolUse: test\n  PostToolUse: validate";
        let fm = parse_skill_yaml(yaml);
        let map = match fm.get("hooks") {
            Some(FrontmatterValue::Map(m)) => m,
            other => panic!("expected Map, got: {:?}", other),
        };
        assert_eq!(
            map.get("PreToolUse"),
            Some(&FrontmatterValue::String("test".into()))
        );
    }

    #[test]
    fn parse_yaml_quoted_values() {
        let yaml = "name: \"Quoted Name\"\nversion: '1.0'";
        let fm = parse_skill_yaml(yaml);
        assert_eq!(
            fm.get("name"),
            Some(&FrontmatterValue::String("Quoted Name".into()))
        );
        assert_eq!(
            fm.get("version"),
            Some(&FrontmatterValue::String("1.0".into()))
        );
    }

    // -- frontmatter splitting --

    #[test]
    fn split_frontmatter_with_yaml() {
        let content = "---\nname: test\n---\n# Body\n\nContent here.";
        let (fm, body) = split_frontmatter(content);
        assert_eq!(
            fm.get("name"),
            Some(&FrontmatterValue::String("test".into()))
        );
        assert_eq!(body, "# Body\n\nContent here.");
    }

    #[test]
    fn split_frontmatter_no_yaml() {
        let content = "# Just markdown\n\nNo frontmatter.";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    // -- file path extraction --

    #[test]
    fn extract_file_paths_from_tool_uses() {
        let read_input = serde_json::json!({"file_path": "/tmp/foo.rs"});
        let write_input = serde_json::json!({"file_path": "/tmp/bar.rs"});
        let glob_input = serde_json::json!({"path": "/tmp", "pattern": "*.rs"});
        let other_input = serde_json::json!({"command": "ls"});

        let tool_uses: Vec<(&str, &serde_json::Value)> = vec![
            ("Read", &read_input),
            ("Write", &write_input),
            ("Glob", &glob_input),
            ("Bash", &other_input),
        ];

        let paths = extract_file_paths(&tool_uses);
        assert_eq!(paths, vec!["/tmp/foo.rs", "/tmp/bar.rs", "/tmp"]);
    }

    // -- relative path --

    #[test]
    fn compute_relative_path_strips_cwd() {
        assert_eq!(
            compute_relative_path("/project/src/main.rs", "/project"),
            Some("src/main.rs".into())
        );
    }

    #[test]
    fn compute_relative_path_outside_cwd() {
        assert_eq!(compute_relative_path("/other/file.rs", "/project"), None);
    }

    #[test]
    fn compute_relative_path_windows() {
        assert_eq!(
            compute_relative_path("C:\\project\\src\\main.rs", "C:\\project"),
            Some("src/main.rs".into())
        );
    }
}
