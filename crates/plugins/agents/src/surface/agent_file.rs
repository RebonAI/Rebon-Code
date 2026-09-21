//! Agent-file path computation, markdown formatting, and the
//! [`AgentFs`] write/delete seam.
//!
//! The pure helpers (path computation, YAML/markdown serialization)
//! are standalone functions; the IO lives behind the [`AgentFs`]
//! trait so this crate never touches the filesystem directly.

use rebon_tool::agent_registry::escape_yaml_double_quoted;

use crate::surface::types::{
    AgentMemoryScope, AgentSource, AgentSummary, EffortValue, SettingSource,
};

/// The agents-dir folder name. The project config dir name is a
/// value parameter on [`AgentPathContext`] so this crate never reads
/// process state.
pub const AGENTS_DIR: &str = "agents";

/// Static path inputs needed to compute agent file locations.
///
/// The consumer computes each path upstream and hands it to this
/// crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPathContext {
    /// Current working directory.
    pub cwd: String,
    /// Config home dir (`~/.rebon`).
    pub rebon_config_home_dir: String,
    /// Managed-settings prefix.
    pub managed_file_path: String,
    /// Project config dir name (e.g. `.rebon`).
    pub project_config_dir_name: String,
}

impl AgentPathContext {
    /// Convenience constructor.
    pub fn new(
        cwd: impl Into<String>,
        rebon_config_home_dir: impl Into<String>,
        managed_file_path: impl Into<String>,
        project_config_dir_name: impl Into<String>,
    ) -> Self {
        AgentPathContext {
            cwd: cwd.into(),
            rebon_config_home_dir: rebon_config_home_dir.into(),
            managed_file_path: managed_file_path.into(),
            project_config_dir_name: project_config_dir_name.into(),
        }
    }
}

/// Possible errors when computing an agent file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFileError {
    /// `flagSettings` agents have no on-disk location.
    FlagSettingsHasNoPath,
    /// Plugin agents have no editable on-disk path.
    PluginHasNoPath,
    /// Built-in agents have no on-disk path.
    BuiltInHasNoPath,
}

/// Format an agent definition as the YAML-frontmatter markdown body
/// that gets written to disk.
///
/// The YAML escape rules are pinned exactly:
///
/// 1. Backslashes are doubled FIRST: `\` → `\\`.
/// 2. Then double quotes are escaped: `"` → `\"`.
/// 3. Then newlines are escaped as `\\n` (two backslashes + `n`) so
///    the YAML reader stores them as a literal `\n`.
///
/// The frontmatter omits the `tools:` line entirely when `tools` is
/// `None` or `Some(["*"])` (behavioral).
pub fn format_agent_as_markdown(
    agent_type: &str,
    when_to_use: &str,
    tools: Option<&[String]>,
    system_prompt: &str,
    color: Option<&str>,
    model: Option<&str>,
    memory: Option<AgentMemoryScope>,
    effort: Option<&EffortValue>,
) -> String {
    let escaped_when = escape_yaml_double_quoted(when_to_use);

    let is_all_tools = match tools {
        None => true,
        Some(t) => t.len() == 1 && t[0] == "*",
    };
    let tools_line = if is_all_tools {
        String::new()
    } else {
        format!("\ntools: {}", tools.unwrap().join(", "))
    };
    let model_line = model.map(|m| format!("\nmodel: {}", m)).unwrap_or_default();
    let effort_line = effort
        .map(|e| format!("\neffort: {}", e.as_str()))
        .unwrap_or_default();
    let color_line = color.map(|c| format!("\ncolor: {}", c)).unwrap_or_default();
    let memory_line = memory
        .map(|m| format!("\nmemory: {}", m.as_str()))
        .unwrap_or_default();

    format!(
        "---\nname: {agent_type}\ndescription: \"{escaped_when}\"{tools_line}{model_line}{effort_line}{color_line}{memory_line}\n---\n\n{system_prompt}\n"
    )
}

/// Compute the on-disk directory path for an agent location.
///
/// `flagSettings` is rejected because flag-supplied agents have no
/// on-disk file.
pub fn agent_directory_path(
    ctx: &AgentPathContext,
    location: SettingSource,
) -> Result<String, AgentFileError> {
    match location {
        SettingSource::FlagSettings => Err(AgentFileError::FlagSettingsHasNoPath),
        SettingSource::UserSettings => Ok(join_path(&[&ctx.rebon_config_home_dir, AGENTS_DIR])),
        SettingSource::ProjectSettings => Ok(join_path(&[
            &ctx.cwd,
            &ctx.project_config_dir_name,
            AGENTS_DIR,
        ])),
        SettingSource::PolicySettings => Ok(join_path(&[
            &ctx.managed_file_path,
            &ctx.project_config_dir_name,
            AGENTS_DIR,
        ])),
        SettingSource::LocalSettings => Ok(join_path(&[
            &ctx.cwd,
            &ctx.project_config_dir_name,
            AGENTS_DIR,
        ])),
    }
}

/// Relative variant used by display labels — `projectSettings`
/// flattens to `./<dir>/agents`, every other source falls through to
/// the absolute path.
pub fn relative_agent_directory_path(
    ctx: &AgentPathContext,
    location: SettingSource,
) -> Result<String, AgentFileError> {
    if location == SettingSource::ProjectSettings {
        return Ok(join_path(&[".", &ctx.project_config_dir_name, AGENTS_DIR]));
    }
    agent_directory_path(ctx, location)
}

/// Compute the file path for a NEW agent (uses agent_type as the
/// filename). Used when creating new agent files.
pub fn new_agent_file_path(
    ctx: &AgentPathContext,
    source: SettingSource,
    agent_type: &str,
) -> Result<String, AgentFileError> {
    let dir = agent_directory_path(ctx, source)?;
    Ok(join_path(&[&dir, &format!("{agent_type}.md")]))
}

/// Compute the actual on-disk file path for an existing agent.
/// Honours the `filename` override (behavioral for the rare case where
/// the on-disk filename doesn't match the agent's type).
///
/// Returns the special string `"Built-in"` for built-in agents and
/// `AgentFileError::PluginHasNoPath` for plugin agents, which have no
/// file of their own.
pub fn actual_agent_file_path(
    ctx: &AgentPathContext,
    agent: &AgentSummary,
) -> Result<String, AgentFileError> {
    match &agent.source {
        AgentSource::BuiltIn => Ok("Built-in".to_string()),
        AgentSource::Plugin { .. } => Err(AgentFileError::PluginHasNoPath),
        AgentSource::Settings(ss) => {
            let dir = agent_directory_path(ctx, *ss)?;
            let filename = agent.filename.as_deref().unwrap_or(&agent.agent_type);
            Ok(join_path(&[&dir, &format!("{filename}.md")]))
        }
    }
}

/// Compute the relative file path for a NEW agent.
///
/// Returns `"Built-in"` for built-in agents.
pub fn new_relative_agent_file_path(
    ctx: &AgentPathContext,
    source: AgentSource,
    agent_type: &str,
) -> Result<String, AgentFileError> {
    match source {
        AgentSource::BuiltIn => Ok("Built-in".to_string()),
        AgentSource::Plugin { .. } => Err(AgentFileError::PluginHasNoPath),
        AgentSource::Settings(ss) => {
            let dir = relative_agent_directory_path(ctx, ss)?;
            Ok(join_path(&[&dir, &format!("{agent_type}.md")]))
        }
    }
}

/// Compute the actual relative file path for an existing agent.
///
/// Built-ins return `"Built-in"`, plugin agents return
/// `"Plugin: <name>"`, flagSettings agents return `"CLI argument"`.
pub fn actual_relative_agent_file_path(
    ctx: &AgentPathContext,
    agent: &AgentSummary,
) -> Result<String, AgentFileError> {
    match &agent.source {
        AgentSource::BuiltIn => Ok("Built-in".to_string()),
        AgentSource::Plugin { plugin } => {
            let name = if plugin.is_empty() { "Unknown" } else { plugin };
            Ok(format!("Plugin: {name}"))
        }
        AgentSource::Settings(SettingSource::FlagSettings) => Ok("CLI argument".to_string()),
        AgentSource::Settings(ss) => {
            let dir = relative_agent_directory_path(ctx, *ss)?;
            let filename = agent.filename.as_deref().unwrap_or(&agent.agent_type);
            Ok(join_path(&[&dir, &format!("{filename}.md")]))
        }
    }
}

/// Filesystem seam used by save / update / delete. This crate never
/// touches `fs::File` directly.
pub trait AgentFs {
    /// Ensure the directory exists (recursive mkdir).
    fn ensure_dir(&mut self, dir: &str) -> Result<(), AgentFsError>;
    /// Write a file. If `exclusive` is `true`, the underlying open
    /// must use `wx` (fail if file exists).
    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        exclusive: bool,
    ) -> Result<(), AgentFsError>;
    /// Delete a file. Missing files (`ENOENT`) are NOT an error
    /// (behavioral: ENOENT is swallowed, not surfaced as an error).
    fn delete_file(&mut self, path: &str) -> Result<(), AgentFsError>;
}

/// IO error returned by [`AgentFs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFsError {
    /// File already exists (corresponds to `EEXIST`).
    FileExists(String),
    /// Other IO error.
    Other(String),
}

/// Save an agent to disk.
///
/// Refuses to save `built-in` agents (`SaveError::CannotSaveBuiltIn`).
#[allow(clippy::too_many_arguments)]
pub fn save_agent_to_file<F: AgentFs>(
    fs: &mut F,
    ctx: &AgentPathContext,
    source: AgentSource,
    agent_type: &str,
    when_to_use: &str,
    tools: Option<&[String]>,
    system_prompt: &str,
    check_exists: bool,
    color: Option<&str>,
    model: Option<&str>,
    memory: Option<AgentMemoryScope>,
    effort: Option<&EffortValue>,
) -> Result<String, SaveError> {
    let settings_source = match source {
        AgentSource::BuiltIn => return Err(SaveError::CannotSaveBuiltIn),
        AgentSource::Plugin { .. } => return Err(SaveError::PluginHasNoPath),
        AgentSource::Settings(s) => s,
    };

    let dir = agent_directory_path(ctx, settings_source).map_err(SaveError::Path)?;
    fs.ensure_dir(&dir).map_err(SaveError::Io)?;
    let file_path =
        new_agent_file_path(ctx, settings_source, agent_type).map_err(SaveError::Path)?;
    let content = format_agent_as_markdown(
        agent_type,
        when_to_use,
        tools,
        system_prompt,
        color,
        model,
        memory,
        effort,
    );
    match fs.write_file(&file_path, &content, check_exists) {
        Ok(()) => Ok(file_path),
        Err(AgentFsError::FileExists(_)) => Err(SaveError::AlreadyExists(file_path)),
        Err(other) => Err(SaveError::Io(other)),
    }
}

/// Errors returned by [`save_agent_to_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// Cannot save `built-in` agents.
    CannotSaveBuiltIn,
    /// Plugin agents have no editable file path.
    PluginHasNoPath,
    /// Path computation failed.
    Path(AgentFileError),
    /// Filesystem error.
    Io(AgentFsError),
    /// File already exists (when `check_exists` is `true`).
    AlreadyExists(String),
}

/// Update an existing agent file.
#[allow(clippy::too_many_arguments)]
pub fn update_agent_file<F: AgentFs>(
    fs: &mut F,
    ctx: &AgentPathContext,
    agent: &AgentSummary,
    new_when_to_use: &str,
    new_tools: Option<&[String]>,
    new_system_prompt: &str,
    new_color: Option<&str>,
    new_model: Option<&str>,
    new_memory: Option<AgentMemoryScope>,
    new_effort: Option<&EffortValue>,
) -> Result<String, SaveError> {
    if matches!(agent.source, AgentSource::BuiltIn) {
        return Err(SaveError::CannotSaveBuiltIn);
    }
    let file_path = actual_agent_file_path(ctx, agent).map_err(SaveError::Path)?;
    let content = format_agent_as_markdown(
        &agent.agent_type,
        new_when_to_use,
        new_tools,
        new_system_prompt,
        new_color,
        new_model,
        new_memory,
        new_effort,
    );
    fs.write_file(&file_path, &content, false)
        .map_err(SaveError::Io)?;
    Ok(file_path)
}

/// Delete an agent file from disk.
pub fn delete_agent_from_file<F: AgentFs>(
    fs: &mut F,
    ctx: &AgentPathContext,
    agent: &AgentSummary,
) -> Result<String, SaveError> {
    if matches!(agent.source, AgentSource::BuiltIn) {
        return Err(SaveError::CannotSaveBuiltIn);
    }
    let file_path = actual_agent_file_path(ctx, agent).map_err(SaveError::Path)?;
    fs.delete_file(&file_path).map_err(SaveError::Io)?;
    Ok(file_path)
}

// ----- Path helpers -----

/// Joins path components with `/`. Cross-platform — this module
/// produces strings the consumer renders or hands to its own path
/// library. Always joins with `/` (not the platform separator) so
/// output stays platform-stable in tests.
fn join_path(parts: &[&str]) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            // Avoid double-slashes.
            if !out.ends_with('/') && !p.starts_with('/') {
                out.push('/');
            } else if out.ends_with('/') && p.starts_with('/') {
                out.pop();
            }
        }
        out.push_str(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> AgentPathContext {
        AgentPathContext::new("/repo", "/home/.rebon", "/etc/managed", ".rebon")
    }

    // ---- format_agent_as_markdown ----

    #[test]
    fn format_minimal_no_tools() {
        let out = format_agent_as_markdown(
            "code-reviewer",
            "Use this when reviewing code",
            None,
            "You are a reviewer",
            None,
            None,
            None,
            None,
        );
        // No tools/model/effort/color/memory line.
        assert!(out.contains("name: code-reviewer"));
        assert!(out.contains(r#"description: "Use this when reviewing code""#));
        assert!(!out.contains("tools:"));
        assert!(out.ends_with("You are a reviewer\n"));
    }

    #[test]
    fn format_with_specific_tools() {
        let tools = vec!["Read".to_string(), "Write".to_string()];
        let out = format_agent_as_markdown("x", "y", Some(&tools), "z", None, None, None, None);
        assert!(out.contains("tools: Read, Write"));
    }

    #[test]
    fn format_omits_star_tools() {
        let tools = vec!["*".to_string()];
        let out = format_agent_as_markdown("x", "y", Some(&tools), "z", None, None, None, None);
        assert!(!out.contains("tools:"));
    }

    #[test]
    fn format_includes_color_model_effort_memory() {
        let effort = EffortValue::new("high");
        let out = format_agent_as_markdown(
            "x",
            "y",
            None,
            "z prompt",
            Some("blue"),
            Some("opus"),
            Some(AgentMemoryScope::User),
            Some(&effort),
        );
        assert!(out.contains("model: opus"));
        assert!(out.contains("effort: high"));
        assert!(out.contains("color: blue"));
        assert!(out.contains("memory: user"));
    }

    #[test]
    fn format_escapes_when_to_use_quotes() {
        let out =
            format_agent_as_markdown("x", "say \"hi\"", None, "z prompt", None, None, None, None);
        assert!(out.contains(r#"description: "say \"hi\"""#));
    }

    // ---- agent_directory_path ----

    #[test]
    fn dir_user_settings() {
        let p = agent_directory_path(&ctx(), SettingSource::UserSettings).unwrap();
        assert_eq!(p, "/home/.rebon/agents");
    }

    #[test]
    fn dir_project_settings() {
        let p = agent_directory_path(&ctx(), SettingSource::ProjectSettings).unwrap();
        assert_eq!(p, "/repo/.rebon/agents");
    }

    #[test]
    fn dir_local_settings() {
        let p = agent_directory_path(&ctx(), SettingSource::LocalSettings).unwrap();
        assert_eq!(p, "/repo/.rebon/agents");
    }

    #[test]
    fn dir_policy_settings() {
        let p = agent_directory_path(&ctx(), SettingSource::PolicySettings).unwrap();
        assert_eq!(p, "/etc/managed/.rebon/agents");
    }

    #[test]
    fn dir_flag_settings_errors() {
        let err = agent_directory_path(&ctx(), SettingSource::FlagSettings).unwrap_err();
        assert_eq!(err, AgentFileError::FlagSettingsHasNoPath);
    }

    // ---- relative_agent_directory_path ----

    #[test]
    fn relative_dir_project_is_dot_relative() {
        let p = relative_agent_directory_path(&ctx(), SettingSource::ProjectSettings).unwrap();
        assert_eq!(p, "./.rebon/agents");
    }

    #[test]
    fn relative_dir_user_falls_through_to_absolute() {
        let p = relative_agent_directory_path(&ctx(), SettingSource::UserSettings).unwrap();
        assert_eq!(p, "/home/.rebon/agents");
    }

    // ---- new_agent_file_path ----

    #[test]
    fn new_path_user() {
        let p = new_agent_file_path(&ctx(), SettingSource::UserSettings, "code-reviewer").unwrap();
        assert_eq!(p, "/home/.rebon/agents/code-reviewer.md");
    }

    // ---- actual_agent_file_path ----

    #[test]
    fn actual_path_built_in_returns_label() {
        let mut a =
            AgentSummary::minimal("a", "b", "system prompt long enough", AgentSource::BuiltIn);
        a.agent_type = "general-purpose".into();
        let p = actual_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "Built-in");
    }

    #[test]
    fn actual_path_plugin_errors() {
        let a = AgentSummary::minimal(
            "a",
            "b",
            "system prompt long enough",
            AgentSource::Plugin { plugin: "p".into() },
        );
        let err = actual_agent_file_path(&ctx(), &a).unwrap_err();
        assert_eq!(err, AgentFileError::PluginHasNoPath);
    }

    #[test]
    fn actual_path_settings_uses_filename_override() {
        let mut a = AgentSummary::minimal(
            "code-reviewer",
            "b",
            "system prompt long enough",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        a.filename = Some("legacy-name".into());
        let p = actual_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "/home/.rebon/agents/legacy-name.md");
    }

    #[test]
    fn actual_path_settings_falls_back_to_agent_type() {
        let a = AgentSummary::minimal(
            "code-reviewer",
            "b",
            "system prompt long enough",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        let p = actual_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "/home/.rebon/agents/code-reviewer.md");
    }

    // ---- new_relative_agent_file_path ----

    #[test]
    fn new_relative_built_in() {
        let p = new_relative_agent_file_path(&ctx(), AgentSource::BuiltIn, "x").unwrap();
        assert_eq!(p, "Built-in");
    }

    #[test]
    fn new_relative_project() {
        let p = new_relative_agent_file_path(
            &ctx(),
            AgentSource::Settings(SettingSource::ProjectSettings),
            "code-reviewer",
        )
        .unwrap();
        assert_eq!(p, "./.rebon/agents/code-reviewer.md");
    }

    // ---- actual_relative_agent_file_path ----

    #[test]
    fn actual_relative_built_in() {
        let a = AgentSummary::minimal("x", "y", "system prompt long enough", AgentSource::BuiltIn);
        let p = actual_relative_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "Built-in");
    }

    #[test]
    fn actual_relative_plugin_known_name() {
        let a = AgentSummary::minimal(
            "x",
            "y",
            "system prompt long enough",
            AgentSource::Plugin {
                plugin: "my-plugin".into(),
            },
        );
        let p = actual_relative_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "Plugin: my-plugin");
    }

    #[test]
    fn actual_relative_plugin_unknown_name() {
        let a = AgentSummary::minimal(
            "x",
            "y",
            "system prompt long enough",
            AgentSource::Plugin { plugin: "".into() },
        );
        let p = actual_relative_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "Plugin: Unknown");
    }

    #[test]
    fn actual_relative_flag_settings_label() {
        let a = AgentSummary::minimal(
            "x",
            "y",
            "system prompt long enough",
            AgentSource::Settings(SettingSource::FlagSettings),
        );
        let p = actual_relative_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "CLI argument");
    }

    #[test]
    fn actual_relative_project_settings_uses_dot_path() {
        let a = AgentSummary::minimal(
            "code-reviewer",
            "y",
            "system prompt long enough",
            AgentSource::Settings(SettingSource::ProjectSettings),
        );
        let p = actual_relative_agent_file_path(&ctx(), &a).unwrap();
        assert_eq!(p, "./.rebon/agents/code-reviewer.md");
    }

    // ---- AgentFs seam ----

    #[derive(Default)]
    struct MockFs {
        existing: Vec<String>,
        writes: Vec<(String, String, bool)>,
        deletes: Vec<String>,
        dirs_created: Vec<String>,
    }

    impl AgentFs for MockFs {
        fn ensure_dir(&mut self, dir: &str) -> Result<(), AgentFsError> {
            self.dirs_created.push(dir.to_string());
            Ok(())
        }
        fn write_file(
            &mut self,
            path: &str,
            content: &str,
            exclusive: bool,
        ) -> Result<(), AgentFsError> {
            if exclusive && self.existing.iter().any(|p| p == path) {
                return Err(AgentFsError::FileExists(path.to_string()));
            }
            self.writes
                .push((path.to_string(), content.to_string(), exclusive));
            Ok(())
        }
        fn delete_file(&mut self, path: &str) -> Result<(), AgentFsError> {
            self.deletes.push(path.to_string());
            Ok(())
        }
    }

    #[test]
    fn save_agent_round_trip() {
        let mut fs = MockFs::default();
        let path = save_agent_to_file(
            &mut fs,
            &ctx(),
            AgentSource::Settings(SettingSource::UserSettings),
            "code-reviewer",
            "Use this when reviewing code",
            None,
            "You are a reviewer that knows things",
            true,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(path, "/home/.rebon/agents/code-reviewer.md");
        assert_eq!(fs.dirs_created, vec!["/home/.rebon/agents".to_string()]);
        assert_eq!(fs.writes.len(), 1);
        assert!(fs.writes[0].1.contains("name: code-reviewer"));
        assert!(fs.writes[0].2);
    }

    #[test]
    fn save_agent_built_in_rejected() {
        let mut fs = MockFs::default();
        let err = save_agent_to_file(
            &mut fs,
            &ctx(),
            AgentSource::BuiltIn,
            "x",
            "y",
            None,
            "z is a prompt that's long enough",
            true,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, SaveError::CannotSaveBuiltIn);
    }

    #[test]
    fn save_agent_eexist_translated() {
        let mut fs = MockFs {
            existing: vec!["/home/.rebon/agents/code-reviewer.md".into()],
            ..Default::default()
        };
        let err = save_agent_to_file(
            &mut fs,
            &ctx(),
            AgentSource::Settings(SettingSource::UserSettings),
            "code-reviewer",
            "y",
            None,
            "z is a prompt that's long enough",
            true,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::AlreadyExists(_)));
    }

    #[test]
    fn update_agent_writes_to_actual_path() {
        let mut fs = MockFs::default();
        let agent = AgentSummary::minimal(
            "code-reviewer",
            "old",
            "old prompt long enough",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        let path = update_agent_file(
            &mut fs,
            &ctx(),
            &agent,
            "new desc",
            None,
            "new prompt long enough",
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(path, "/home/.rebon/agents/code-reviewer.md");
        // Update never goes through ensure_dir.
        assert!(fs.dirs_created.is_empty());
        assert_eq!(fs.writes.len(), 1);
        // Update is non-exclusive.
        assert!(!fs.writes[0].2);
    }

    #[test]
    fn delete_agent_calls_fs() {
        let mut fs = MockFs::default();
        let agent = AgentSummary::minimal(
            "code-reviewer",
            "old",
            "old prompt long enough",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        let path = delete_agent_from_file(&mut fs, &ctx(), &agent).unwrap();
        assert_eq!(path, "/home/.rebon/agents/code-reviewer.md");
        assert_eq!(
            fs.deletes,
            vec!["/home/.rebon/agents/code-reviewer.md".to_string()]
        );
    }

    #[test]
    fn delete_built_in_rejected() {
        let mut fs = MockFs::default();
        let agent = AgentSummary::minimal(
            "general-purpose",
            "x",
            "system prompt long enough",
            AgentSource::BuiltIn,
        );
        let err = delete_agent_from_file(&mut fs, &ctx(), &agent).unwrap_err();
        assert_eq!(err, SaveError::CannotSaveBuiltIn);
    }

    #[test]
    fn join_path_handles_trailing_and_leading_slashes() {
        assert_eq!(join_path(&["a", "b"]), "a/b");
        assert_eq!(join_path(&["a/", "b"]), "a/b");
        assert_eq!(join_path(&["a", "/b"]), "a/b");
        assert_eq!(join_path(&["a/", "/b"]), "a/b");
    }
}
