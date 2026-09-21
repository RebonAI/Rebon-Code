//! Gather the data feeds that `/context` surfaces through the
//! [`crate::commands::context_visualization`] plan.
//!
//! `execute_context_command` in [`super::context`] gathers the context
//! sources that are reachable from an `EngineSession` today:
//!
//! * **Built-in tools (eager)** — from [`rebon_core::Engine::eager_tool_snapshots`].
//! * **Deferred built-in tools** — from [`rebon_core::Engine::deferred_tool_names`]
//!   joined with [`rebon_core::Engine::tool_snapshots`] for descriptions.
//! * **MCP tools** — detected by the `mcp__<server>__<tool>` name convention
//!   used when registering foreign tools. All such tools are reported
//!   as `is_loaded = true` because their
//!   presence in the engine registry means the MCP client successfully
//!   handshook at startup.
//! * **Skills** — populated by `/context` from actual `Skill` tool results
//!   in the transcript; registered-but-uninvoked skills are not counted here.
//! * **Memory files** — from `rebon-plugin-memory` canonical instruction discovery
//!   plus `~/.rebon/projects/<slug>/memory/MEMORY.md` (when present).
//! * **Agents** — from the session's already-loaded [`rebon_tool::AgentRegistry`],
//!   preserving registry provenance without reloading agent directories.
//! * **System prompt sections** — from the latest resolved system prompt
//!   snapshot exposed by the running session.

use crate::commands::context_visualization as cv;

use crate::EngineSession;

/// Approximate token count for a string (chars / 4). Duplicated from
/// [`crate::commands::approx_tokens`] to keep this module self-contained.
fn approx_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Plain-data bundle that maps one-to-one onto the corresponding fields
/// of [`cv::ContextVisualizationInput`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ContextSources {
    pub mcp_tools: Vec<cv::McpTool>,
    pub deferred_builtin_tools: Vec<cv::DeferredBuiltinTool>,
    pub system_tools: Vec<cv::BuiltinToolDetail>,
    pub system_prompt_sections: Vec<cv::SystemPromptSectionDetail>,
    pub agents: Vec<cv::AgentDetail>,
    pub skills: Option<cv::SkillInfo>,
    pub memory_files: Vec<cv::MemoryFileDetail>,
}

impl ContextSources {
    /// Gather every source we can reach from the current session. Safe
    /// to call on a hot path — no network, no tool execution; at most
    /// a handful of `fs::read_to_string` calls for the memory files.
    pub(crate) fn gather(session: &EngineSession) -> Self {
        let (mcp_tools, system_tools) = split_builtin_and_mcp_tools(session);
        let deferred_builtin_tools = gather_deferred_builtin_tools(session);
        let skills = None;
        let agents = gather_loaded_agents(session);
        let memory_files = gather_memory_files(&session.cwd);
        let system_prompt_sections = gather_system_prompt_sections(session);

        Self {
            mcp_tools,
            deferred_builtin_tools,
            system_tools,
            system_prompt_sections,
            agents,
            skills,
            memory_files,
        }
    }
}

fn gather_system_prompt_sections(session: &EngineSession) -> Vec<cv::SystemPromptSectionDetail> {
    let prompt = session
        .engine_half
        .system_prompt_snapshot
        .read()
        .ok()
        .and_then(|guard| guard.clone());
    let Some(prompt) = prompt else {
        return Vec::new();
    };
    if prompt.is_empty() {
        return Vec::new();
    }
    vec![cv::SystemPromptSectionDetail {
        name: "Resolved system prompt".into(),
        tokens: approx_tokens(&prompt),
    }]
}

/// Pattern `mcp__<server>__<tool>`. Every registered foreign tool is named
/// this way, so the `mcp__` prefix plus the double-underscore separator is
/// enough to tell them apart from first-party tools, whose names never
/// contain `__`.
fn looks_like_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__") && name.matches("__").count() >= 2
}

fn split_builtin_and_mcp_tools(
    session: &EngineSession,
) -> (Vec<cv::McpTool>, Vec<cv::BuiltinToolDetail>) {
    let mut mcp = if let Some(client) = session.engine_half.mcp.as_ref().map(|mcp| &mcp.client) {
        client
            .server_names()
            .into_iter()
            .flat_map(|server| {
                client
                    .cached_tool_definitions(&server)
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |definition| cv::McpTool {
                        name: rebon_tool::build_mcp_tool_name(&server, &definition.name),
                        tokens: approx_tokens(&definition.description)
                            + approx_tokens(&definition.input_schema.to_string()),
                        is_loaded: true,
                    })
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut builtin = Vec::new();
    for snap in session.engine_half.engine.eager_tool_snapshots() {
        let tokens =
            approx_tokens(&snap.description) + approx_tokens(&snap.input_schema.to_string());
        if looks_like_mcp_tool(&snap.name) {
            mcp.push(cv::McpTool {
                name: snap.name,
                tokens,
                // Presence in the engine registry means the MCP proxy
                // successfully registered the tool — treat as loaded.
                is_loaded: true,
            });
        } else {
            builtin.push(cv::BuiltinToolDetail {
                name: snap.name,
                tokens,
            });
        }
    }
    (mcp, builtin)
}

fn gather_deferred_builtin_tools(session: &EngineSession) -> Vec<cv::DeferredBuiltinTool> {
    let snapshots = session.engine_half.engine.tool_snapshots();
    let descriptions: std::collections::HashMap<String, (String, serde_json::Value)> = snapshots
        .into_iter()
        .map(|snap| (snap.name.clone(), (snap.description, snap.input_schema)))
        .collect();
    session
        .engine_half
        .engine
        .deferred_tool_names()
        .into_iter()
        .map(|name| {
            let tokens = descriptions
                .get(&name)
                .map(|(desc, schema)| approx_tokens(desc) + approx_tokens(&schema.to_string()))
                .unwrap_or_default();
            cv::DeferredBuiltinTool {
                name,
                tokens,
                is_loaded: false,
            }
        })
        .collect()
}

fn agent_source_kind(source: &rebon_tool::AgentSource) -> cv::SourceKind {
    match source {
        rebon_tool::AgentSource::BuiltIn => cv::SourceKind::BuiltIn,
        rebon_tool::AgentSource::Plugin(_) => cv::SourceKind::Plugin,
        rebon_tool::AgentSource::Settings(setting) => match setting {
            rebon_tool::SettingSource::UserSettings => {
                cv::SourceKind::Setting(cv::SettingSource::User)
            }
            rebon_tool::SettingSource::ProjectSettings => {
                cv::SourceKind::Setting(cv::SettingSource::Project)
            }
            rebon_tool::SettingSource::LocalSettings => {
                cv::SourceKind::Setting(cv::SettingSource::Local)
            }
            rebon_tool::SettingSource::FlagSettings => {
                cv::SourceKind::Setting(cv::SettingSource::Flag)
            }
            rebon_tool::SettingSource::PolicySettings => {
                cv::SourceKind::Setting(cv::SettingSource::Managed)
            }
        },
    }
}

fn gather_loaded_agents(session: &EngineSession) -> Vec<cv::AgentDetail> {
    gather_agent_details_from_registry(&session.engine_half.agent_registry)
}

fn gather_agent_details_from_registry(
    registry: &rebon_tool::AgentRegistry,
) -> Vec<cv::AgentDetail> {
    registry
        .active_snapshot()
        .into_iter()
        .map(|def| cv::AgentDetail {
            agent_type: def.agent_type,
            source: agent_source_kind(&def.source),
            tokens: approx_tokens(&def.when_to_use) + approx_tokens(&def.system_prompt),
        })
        .collect()
}

pub(crate) fn skill_source_kind(source: rebon_plugin_skill::SkillSource) -> cv::SourceKind {
    match source {
        rebon_plugin_skill::SkillSource::BuiltIn => cv::SourceKind::BuiltIn,
        rebon_plugin_skill::SkillSource::User => cv::SourceKind::Setting(cv::SettingSource::User),
        rebon_plugin_skill::SkillSource::Project => {
            cv::SourceKind::Setting(cv::SettingSource::Project)
        }
        rebon_plugin_skill::SkillSource::Managed => {
            cv::SourceKind::Setting(cv::SettingSource::Managed)
        }
        rebon_plugin_skill::SkillSource::Local => cv::SourceKind::Setting(cv::SettingSource::Local),
        rebon_plugin_skill::SkillSource::Flag => cv::SourceKind::Setting(cv::SettingSource::Flag),
        rebon_plugin_skill::SkillSource::Plugin => cv::SourceKind::Plugin,
        rebon_plugin_skill::SkillSource::Mcp => cv::SourceKind::Plugin,
    }
}

pub(crate) fn gather_memory_files(cwd: &str) -> Vec<cv::MemoryFileDetail> {
    rebon_plugin_memory::memory::loaded_files::gather_memory_files(cwd)
        .into_iter()
        .map(|file| cv::MemoryFileDetail {
            path: file.path.to_string_lossy().into_owned(),
            tokens: file.approx_tokens(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The candidate list itself now lives in `rebon-plugin-memory`; these tests keep
    // asserting it from the TUI side so a wiring regression here still fails.
    use rebon_plugin_memory::memory::loaded_files::memory_file_candidates;

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env::lock_env()
    }

    struct EnvGuard {
        prev_disable_auto_memory: Option<std::ffi::OsString>,
        prev_simple: Option<std::ffi::OsString>,
        prev_config_dir: Option<std::ffi::OsString>,
        config_home_path: Option<std::path::PathBuf>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let _lock = env_test_lock();
            let prev_disable_auto_memory = std::env::var_os("REBON_DISABLE_AUTO_MEMORY");
            let prev_simple = std::env::var_os("REBON_SIMPLE");
            let prev_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            unsafe {
                std::env::remove_var("REBON_DISABLE_AUTO_MEMORY");
                std::env::remove_var("REBON_SIMPLE");
                std::env::remove_var("REBON_CONFIG_DIR");
            }
            Self {
                prev_disable_auto_memory,
                prev_simple,
                prev_config_dir,
                config_home_path: None,
                _lock,
            }
        }

        fn with_config_dir(config_home_path: std::path::PathBuf) -> Self {
            let mut guard = Self::new();
            unsafe {
                std::env::set_var("REBON_CONFIG_DIR", &config_home_path);
            }
            guard.config_home_path = Some(config_home_path);
            guard
        }

        fn config_home(&self) -> &std::path::Path {
            self.config_home_path
                .as_deref()
                .expect("config home path set")
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev_disable_auto_memory.take() {
                    Some(v) => std::env::set_var("REBON_DISABLE_AUTO_MEMORY", v),
                    None => std::env::remove_var("REBON_DISABLE_AUTO_MEMORY"),
                }
                match self.prev_simple.take() {
                    Some(v) => std::env::set_var("REBON_SIMPLE", v),
                    None => std::env::remove_var("REBON_SIMPLE"),
                }
                match self.prev_config_dir.take() {
                    Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                    None => std::env::remove_var("REBON_CONFIG_DIR"),
                }
            }
        }
    }

    #[test]
    fn looks_like_mcp_tool_matches_namespaced_tools() {
        assert!(looks_like_mcp_tool("mcp__linear__list_issues"));
        assert!(looks_like_mcp_tool("mcp__github__gh_pr_view"));
        // Three-segment namespace variants still match.
        assert!(looks_like_mcp_tool("mcp__corp__team__thing"));
    }

    #[test]
    fn looks_like_mcp_tool_rejects_first_party_tools() {
        assert!(!looks_like_mcp_tool("Read"));
        assert!(!looks_like_mcp_tool("BashTool"));
        assert!(!looks_like_mcp_tool(""));
        // `mcp__foo` has only one `__` and no tool segment.
        assert!(!looks_like_mcp_tool("mcp__foo"));
    }

    #[test]
    fn skill_source_kind_maps_registry_sources() {
        assert_eq!(
            skill_source_kind(rebon_plugin_skill::SkillSource::BuiltIn),
            cv::SourceKind::BuiltIn
        );
        assert_eq!(
            skill_source_kind(rebon_plugin_skill::SkillSource::User),
            cv::SourceKind::Setting(cv::SettingSource::User)
        );
        assert_eq!(
            skill_source_kind(rebon_plugin_skill::SkillSource::Project),
            cv::SourceKind::Setting(cv::SettingSource::Project)
        );
        assert_eq!(
            skill_source_kind(rebon_plugin_skill::SkillSource::Plugin),
            cv::SourceKind::Plugin
        );
    }

    #[test]
    fn gather_agent_details_uses_registry_provenance() {
        let registry = rebon_tool::AgentRegistry::from_groups(rebon_tool::AgentGroups {
            built_in: vec![agent_def(
                "builtin-agent",
                "builtin prompt",
                rebon_tool::AgentSource::BuiltIn,
            )],
            user: vec![agent_def(
                "user-agent",
                "user prompt",
                rebon_tool::AgentSource::Settings(rebon_tool::SettingSource::UserSettings),
            )],
            project: vec![agent_def(
                "project-agent",
                "project prompt",
                rebon_tool::AgentSource::Settings(rebon_tool::SettingSource::ProjectSettings),
            )],
            ..Default::default()
        });

        let agents = gather_agent_details_from_registry(&registry);

        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].agent_type, "builtin-agent");
        assert_eq!(agents[0].source, cv::SourceKind::BuiltIn);
        assert_eq!(
            agents[1].source,
            cv::SourceKind::Setting(cv::SettingSource::User)
        );
        assert_eq!(
            agents[2].source,
            cv::SourceKind::Setting(cv::SettingSource::Project)
        );
    }

    fn agent_def(
        name: &str,
        prompt: &str,
        source: rebon_tool::AgentSource,
    ) -> rebon_tool::ResolvedAgentDef {
        rebon_tool::ResolvedAgentDef {
            agent_type: name.to_string(),
            when_to_use: format!("when to use {name}"),
            system_prompt: prompt.to_string(),
            tool_filter: rebon_tool::ToolFilter::unrestricted(),
            model: None,
            model_profile: None,
            provider: None,
            effort: None,
            background: false,
            isolation: None,
            memory: None,
            permission_mode: None,
            runtime: rebon_tool::AgentRuntime::Local,
            source,
            file_stem: None,
        }
    }

    #[test]
    fn memory_file_candidates_is_deduped_and_nonempty() {
        let _guard = EnvGuard::new();
        let paths = memory_file_candidates("/tmp/project");
        assert!(paths.iter().any(|p| p.ends_with("MEMORY.md")));
        // No duplicate paths.
        let mut sorted: Vec<_> = paths.iter().collect();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), paths.len(), "expected deduped candidate list");
    }

    #[test]
    fn memory_file_candidates_honor_rebon_config_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config_home = tmp.path().join("config-home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        // macOS tempdirs live behind the /var -> /private/var symlink, and the
        // candidate path and the expected path below must derive the project
        // key from the same spelling of the cwd. Unix-only: on Windows,
        // canonicalize would introduce a \\?\ prefix this test never had.
        #[cfg(unix)]
        let project = project.canonicalize().unwrap();
        let global = config_home.join("REBON.md");
        std::fs::write(&global, "config global").unwrap();
        let _guard = EnvGuard::with_config_dir(config_home.clone());
        let cwd = project.to_string_lossy().into_owned();

        let paths = memory_file_candidates(&cwd);
        let memory_dir = rebon_plugin_memory::memory::prompt::get_auto_mem_path(&cwd).unwrap();

        assert!(paths.iter().any(|p| p == &global));
        assert!(paths.iter().any(|p| p == &memory_dir.join("MEMORY.md")));
        assert!(memory_dir.starts_with(_guard.config_home()));
    }

    #[test]
    fn context_sources_omits_memory_md_when_auto_memory_disabled() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_string_lossy().into_owned();
        unsafe {
            std::env::set_var("REBON_DISABLE_AUTO_MEMORY", "1");
        }

        let paths = memory_file_candidates(&cwd);

        assert!(
            !paths.iter().any(|p| p.ends_with("MEMORY.md")),
            "disabled auto-memory should omit MEMORY.md candidates: {paths:?}"
        );
    }

    #[test]
    fn context_sources_keeps_rebon_md_when_auto_memory_disabled() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let rebon = project.join("REBON.md");
        std::fs::write(&rebon, "project instructions").unwrap();
        let rebon = std::fs::canonicalize(rebon).unwrap();
        let cwd = std::fs::canonicalize(project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        unsafe {
            std::env::set_var("REBON_DISABLE_AUTO_MEMORY", "1");
        }

        let paths = memory_file_candidates(&cwd);

        assert!(
            paths.iter().any(|p| p == &rebon),
            "REBON.md should remain in context candidates when only auto-memory is disabled"
        );
        assert!(!paths.iter().any(|p| p.ends_with("MEMORY.md")));
    }

    #[test]
    fn gather_memory_files_reads_existing_fixture() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let file = project.join("REBON.md");
        std::fs::write(&file, "hello ".repeat(20)).unwrap();

        let cwd = std::fs::canonicalize(&project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mems = gather_memory_files(&cwd);
        assert!(
            mems.iter().any(|m| m.path.ends_with("REBON.md")),
            "expected to pick up the fixture REBON.md: {:?}",
            mems
        );
        // Token estimate = len / 4 = 120 / 4 = 30.
        let detail = mems
            .iter()
            .find(|m| m.path.ends_with("REBON.md") && m.path.contains("project"))
            .expect("fixture row");
        assert_eq!(detail.tokens, 30);
    }

    #[test]
    fn gather_memory_files_includes_rules_and_includes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join(".rebon/rules")).unwrap();
        let included = project.join("included.md");
        let rebon = project.join("REBON.md");
        let rule = project.join(".rebon/rules/rule.md");
        std::fs::write(&included, "included content").unwrap();
        std::fs::write(&rebon, "@./included.md\nparent").unwrap();
        std::fs::write(&rule, "rule content").unwrap();
        let included = std::fs::canonicalize(&included).unwrap();
        let rebon = std::fs::canonicalize(&rebon).unwrap();
        let rule = std::fs::canonicalize(&rule).unwrap();

        let cwd = std::fs::canonicalize(&project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mems = gather_memory_files(&cwd);
        // Discovery records paths unchanged; on platforms where joining
        // `./included.md` keeps the `./` segment (e.g. macOS) the raw
        // string won't equal the canonicalized expectation. Resolve both
        // through canonicalize for an apples-to-apples compare.
        let canon =
            |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
        assert!(mems.iter().any(|m| canon(&m.path) == included));
        assert!(mems.iter().any(|m| canon(&m.path) == rebon));
        assert!(mems.iter().any(|m| canon(&m.path) == rule));
    }

    #[test]
    fn gather_memory_files_skips_missing_candidates() {
        // Point at a cwd that deliberately has no REBON.md so only
        // user-level / auto-memory paths (possibly absent too) are considered.
        let tmp = tempfile::tempdir().expect("tempdir");
        let empty_cwd = tmp.path().to_string_lossy().into_owned();
        let mems = gather_memory_files(&empty_cwd);
        for m in &mems {
            assert!(
                !m.path.contains(tmp.path().to_string_lossy().as_ref())
                    || m.path.ends_with("MEMORY.md"),
                "no project-level REBON.md should have been discovered: {}",
                m.path
            );
        }
    }
}
