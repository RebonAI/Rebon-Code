//! Agent files on disk, as the `/agents` surface sees them.
//!
//! [`surface`](crate::surface) is deliberately free of IO: it computes
//! paths, formats markdown and validates drafts, and takes the
//! filesystem as the injected [`AgentFs`] seam. This module is the
//! other side of that seam — the one place in the plugin that actually
//! reads a directory or writes a file, and the one place that turns
//! [`rebon_tool`]'s resolved registry into the [`AgentSummary`] rows
//! the surface reducers work on.
//!
//! It reads through [`AgentRegistry`] rather than parsing agent files
//! itself. The terminal used to carry a second frontmatter parser for
//! exactly one answer — which file did this agent come from — and the
//! two parsers disagreed: the registry strips quotes off `name:` and
//! that one did not, so an agent declared as `name: "reviewer"` never
//! matched and its file could not be opened or deleted from the
//! dialog. The registry records the stem it read
//! ([`ResolvedAgentDef::file_stem`]) and this module reads it back.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use rebon_tool::{
    AgentRegistry, AgentRegistrySettingSource, AgentRegistrySource, ResolvedAgentDef, ToolFilter,
};

use crate::surface::agent_file::{AgentFs, AgentFsError, AgentPathContext};
use crate::surface::types::{
    AgentMemoryScope, AgentRuntimeLabel, AgentSource, AgentSummary, EffortValue, SettingSource,
};

/// The path inputs the surface needs, resolved against this process.
///
/// The surface takes them as values precisely so it never reads process
/// state; resolving them is this side's job. Managed settings share the
/// config home because Rebon has no separate managed-settings prefix.
pub fn path_context(cwd: &Path) -> AgentPathContext {
    let config_home = rebon_session::config_home::default_config_home_dir();
    AgentPathContext::new(
        normalize_path(cwd),
        normalize_path(&config_home),
        normalize_path(&config_home),
        rebon_session::config_home::DEFAULT_CONFIG_DIR_NAME,
    )
}

/// Load every active agent definition and project it onto the display
/// shape the surface reducers take.
pub fn load_agents(path_ctx: &AgentPathContext) -> Vec<AgentSummary> {
    let cwd = Path::new(&path_ctx.cwd);
    let home_config_dir = Path::new(&path_ctx.rebon_config_home_dir);
    let registry = AgentRegistry::load(cwd, home_config_dir);
    registry.active().map(agent_summary_from_def).collect()
}

/// Project one resolved definition onto an [`AgentSummary`].
pub fn agent_summary_from_def(def: &ResolvedAgentDef) -> AgentSummary {
    AgentSummary {
        agent_type: def.agent_type.clone(),
        when_to_use: def.when_to_use.clone(),
        tools: registry_tools(&def.tool_filter),
        system_prompt: def.system_prompt.clone(),
        color: None,
        model: def.model.clone(),
        effort: def.effort.clone().map(EffortValue::new),
        memory: def.memory.as_deref().and_then(parse_memory),
        runtime: runtime_label(&def.runtime),
        source: agent_source_from_registry(&def.source),
        // Only settings-backed agents have a file to name; the registry
        // already dropped a stem that matches the agent type, which is
        // the path the surface computes on its own.
        filename: match def.source {
            AgentRegistrySource::Settings(_) => def.file_stem.clone(),
            AgentRegistrySource::BuiltIn | AgentRegistrySource::Plugin(_) => None,
        },
    }
}

/// Project the registry's executable runtime onto the display shape the
/// agents surface uses. The registry is the single source of truth; the
/// surface carries only the label.
fn runtime_label(runtime: &rebon_tool::AgentRuntime) -> AgentRuntimeLabel {
    match runtime {
        rebon_tool::AgentRuntime::Local => AgentRuntimeLabel::Local,
        rebon_tool::AgentRuntime::Acp { .. } => AgentRuntimeLabel::Acp {
            command: runtime.command_line(),
        },
    }
}

fn agent_source_from_registry(source: &AgentRegistrySource) -> AgentSource {
    match source {
        AgentRegistrySource::BuiltIn => AgentSource::BuiltIn,
        AgentRegistrySource::Plugin(plugin) => AgentSource::Plugin {
            plugin: plugin.clone(),
        },
        AgentRegistrySource::Settings(setting_source) => {
            AgentSource::Settings(setting_source_from_registry(*setting_source))
        }
    }
}

fn setting_source_from_registry(source: AgentRegistrySettingSource) -> SettingSource {
    match source {
        AgentRegistrySettingSource::UserSettings => SettingSource::UserSettings,
        AgentRegistrySettingSource::ProjectSettings => SettingSource::ProjectSettings,
        AgentRegistrySettingSource::LocalSettings => SettingSource::LocalSettings,
        AgentRegistrySettingSource::FlagSettings => SettingSource::FlagSettings,
        AgentRegistrySettingSource::PolicySettings => SettingSource::PolicySettings,
    }
}

fn registry_tools(tool_filter: &ToolFilter) -> Option<Vec<String>> {
    tool_filter.allow_list().and_then(|tools| {
        if tools.is_empty() {
            Some(Vec::new())
        } else if tools.iter().any(|tool| tool == "*") {
            None
        } else {
            Some(tools)
        }
    })
}

/// Read the `memory:` frontmatter scope. `local` and anything else the
/// registry accepts but the surface has no row for read as no scope.
pub fn parse_memory(raw: &str) -> Option<AgentMemoryScope> {
    match raw.trim() {
        "user" => Some(AgentMemoryScope::User),
        "project" => Some(AgentMemoryScope::Project),
        _ => None,
    }
}

/// Paths reach the surface with forward slashes so the strings it
/// joins and compares are the same on every platform.
pub fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The real filesystem behind the surface's [`AgentFs`] seam.
#[derive(Default)]
pub struct RealAgentFs;

impl AgentFs for RealAgentFs {
    fn ensure_dir(&mut self, dir: &str) -> Result<(), AgentFsError> {
        fs::create_dir_all(dir).map_err(map_io_error)
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        exclusive: bool,
    ) -> Result<(), AgentFsError> {
        let mut options = OpenOptions::new();
        options.write(true);
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        let mut file = options.open(path).map_err(map_io_error)?;
        file.write_all(content.as_bytes()).map_err(map_io_error)?;
        file.sync_data().map_err(map_io_error)
    }

    fn delete_file(&mut self, path: &str) -> Result<(), AgentFsError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(map_io_error(err)),
        }
    }
}

/// One line naming why a save or a delete did not happen.
///
/// Next to the seam that produces the error rather than next to the
/// surface that shows it: what an [`AgentFsError`] means is this
/// module's, and every front end wants the same sentence.
pub fn format_save_error(err: &crate::surface::agent_file::SaveError) -> String {
    use crate::surface::agent_file::SaveError;
    match err {
        SaveError::CannotSaveBuiltIn => "built-in agent cannot be deleted".into(),
        SaveError::PluginHasNoPath => "plugin agent has no editable file".into(),
        SaveError::Path(path_err) => format!("{path_err:?}"),
        SaveError::Io(io_err) => format!("{io_err:?}"),
        SaveError::AlreadyExists(path) => format!("file already exists: {path}"),
    }
}

fn map_io_error(err: std::io::Error) -> AgentFsError {
    if err.kind() == std::io::ErrorKind::AlreadyExists {
        AgentFsError::FileExists(err.to_string())
    } else {
        AgentFsError::Other(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::tasks::test_support::TestConfigHome;
    use tempfile::TempDir;

    fn write_agent(dir: &Path, file_name: &str, body: &str) {
        fs::create_dir_all(dir).expect("create agents dir");
        fs::write(dir.join(file_name), body).expect("write agent file");
    }

    /// The fields the terminal's own frontmatter parser used to fill in,
    /// now read through the registry that already parsed the file.
    #[test]
    fn a_user_agent_projects_every_displayed_field() {
        let home = TestConfigHome::new("agent-files-projects-fields");
        let project = TempDir::new().expect("temp project");
        write_agent(
            &home.path().join("agents"),
            "code-reviewer.md",
            "---\nname: code-reviewer\ndescription: \"Review \\\\n code\"\ntools: Read, Grep\nmodel: sonnet\nmemory: user\neffort: high\n---\n\nYou are a reviewer.\n",
        );

        let ctx = path_context(project.path());
        let agents = load_agents(&ctx);
        let agent = agents
            .iter()
            .find(|agent| agent.agent_type == "code-reviewer")
            .expect("the user agent is listed");

        assert_eq!(agent.when_to_use, "Review \n code");
        // The registry's allow list is sorted, not in file order. That
        // was already true of what the dialog displayed, since the tools
        // column always came from `ToolFilter`.
        assert_eq!(
            agent.tools,
            Some(vec!["Grep".to_string(), "Read".to_string()])
        );
        assert_eq!(agent.model.as_deref(), Some("sonnet"));
        assert_eq!(agent.memory, Some(AgentMemoryScope::User));
        assert_eq!(agent.effort.as_ref().map(EffortValue::as_str), Some("high"));
        assert_eq!(agent.system_prompt, "You are a reviewer.");
        assert_eq!(
            agent.source,
            AgentSource::Settings(SettingSource::UserSettings)
        );
        assert_eq!(agent.runtime, AgentRuntimeLabel::Local);
        assert_eq!(agent.filename, None, "the stem already is the agent type");
    }

    /// A file whose stem differs from the agent's name is the whole
    /// reason the surface carries a filename at all.
    #[test]
    fn a_file_named_differently_from_its_agent_keeps_its_stem() {
        let home = TestConfigHome::new("agent-files-keeps-stem");
        let project = TempDir::new().expect("temp project");
        write_agent(
            &home.path().join("agents"),
            "reviewer-v2.md",
            "---\nname: code-reviewer\ndescription: reviews\n---\n\nbody\n",
        );

        let agents = load_agents(&path_context(project.path()));
        let agent = agents
            .iter()
            .find(|agent| agent.agent_type == "code-reviewer")
            .expect("the user agent is listed");
        assert_eq!(agent.filename.as_deref(), Some("reviewer-v2"));
    }

    /// The bug the second parser had: it compared the raw `name:` value,
    /// quotes included, so a quoted name matched nothing and the file
    /// could not be opened or deleted from the dialog.
    #[test]
    fn a_quoted_agent_name_still_finds_its_file() {
        let home = TestConfigHome::new("agent-files-quoted-name");
        let project = TempDir::new().expect("temp project");
        write_agent(
            &home.path().join("agents"),
            "quoted-file.md",
            "---\nname: \"code-reviewer\"\ndescription: reviews\n---\n\nbody\n",
        );

        let agents = load_agents(&path_context(project.path()));
        let agent = agents
            .iter()
            .find(|agent| agent.agent_type == "code-reviewer")
            .expect("a quoted name resolves to the unquoted agent type");
        assert_eq!(agent.filename.as_deref(), Some("quoted-file"));
    }

    /// Built-ins have no file, so they never name one.
    #[test]
    fn a_builtin_agent_names_no_file() {
        let _home = TestConfigHome::new("agent-files-builtin");
        let project = TempDir::new().expect("temp project");
        let agents = load_agents(&path_context(project.path()));
        let builtin = agents
            .iter()
            .find(|agent| agent.source == AgentSource::BuiltIn)
            .expect("built-ins are always listed");
        assert_eq!(builtin.filename, None);
    }

    #[test]
    fn the_memory_scope_reads_only_the_two_the_surface_shows() {
        assert_eq!(parse_memory(" user "), Some(AgentMemoryScope::User));
        assert_eq!(parse_memory("project"), Some(AgentMemoryScope::Project));
        assert_eq!(parse_memory("local"), None);
        assert_eq!(parse_memory(""), None);
    }
}
