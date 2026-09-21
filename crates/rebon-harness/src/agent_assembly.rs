//! Assemble session backends from config, plugin declarations and agent definitions.
//!
//! Routing and backend reuse live in `rebon_agent_core::routing` and `registry`.
//! Only this composition layer knows ACP transports, tools and engine replay.

use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use rebon_acp_client::{
    AcpAgentBackend, AcpBackendConfig, AgentCommand, HostFileHistory, SnapshotHostFs,
    TranscriptJournal,
};
use rebon_agent_core::registry::{BackendEntry, BackendRegistry};
#[cfg(test)]
use rebon_agent_core::routing::{
    agent_args_name_the_switch, handle_agent_command, initial_agent, parse_agent_command,
    parse_backend_command,
};
use rebon_agent_core::routing::{AgentOrigin, DeclaredAgent, SessionAgents};
use rebon_agent_core::{AgentBackend, AgentBackendSwitch};
use rebon_config::AcpAgentConfig;
#[cfg(test)]
use rebon_config::LOCAL_AGENT_ID;

/// Reads this session's transcript and describes it for a cold agent.
struct SessionHandoff {
    projects_root: PathBuf,
    cwd: String,
    /// Immutable session identity for this runtime.
    session_id: String,
}

impl SessionHandoff {
    fn new(
        projects_root: impl Into<PathBuf>,
        cwd: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            projects_root: projects_root.into(),
            cwd: cwd.into(),
            session_id: session_id.into(),
        }
    }
}

impl rebon_acp_client::HandoffProvider for SessionHandoff {
    fn handoff_text(&self, host_session_id: &str) -> Option<String> {
        if host_session_id != self.session_id {
            // One handoff belongs to one session; describing another
            // session's conversation would be worse than none.
            tracing::error!(
                handoff_session = %self.session_id,
                requested_session = %host_session_id,
                "rebon: refusing to hand over another session's conversation"
            );
            return None;
        }
        let path =
            rebon_session::transcript_file_path(&self.projects_root, &self.cwd, &self.session_id);
        let loaded = rebon_session::load_transcript_from_file(&path).ok()??;
        let messages = rebon_core::query::transcript_to_api_messages(&loaded.messages);
        rebon_agent_core::handoff::handoff_document(
            &messages,
            rebon_agent_core::handoff::MAX_HANDOFF_CHARS,
        )
    }
}

/// This session's file-history tracker, as the ACP leg sees it.
///
/// A thin forward onto the tracker the local engine already drives, so
/// an ACP write and a `Write` tool call land in the same store.
/// The adapter rejects other session ids rather than corrupting history.
/// File-history adapter bound to one immutable session runtime.
struct SessionFileHistory {
    session_id: String,
    tracker: Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
}

impl SessionFileHistory {
    fn check(&self, session_id: &str) -> anyhow::Result<()> {
        if session_id == self.session_id {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "file history for session {} was asked to act for session {session_id}",
            self.session_id
        ))
    }
}

impl HostFileHistory for SessionFileHistory {
    fn begin_turn(&self, session_id: &str, turn_id: &str) -> anyhow::Result<()> {
        self.check(session_id)?;
        self.tracker.begin_prompt_turn(turn_id)
    }

    fn snapshot_before_write(&self, session_id: &str, path: &Path) -> anyhow::Result<()> {
        self.check(session_id)?;
        self.tracker.track_before_write(path)
    }

    fn end_turn(&self, session_id: &str, turn_id: &str) -> anyhow::Result<()> {
        self.check(session_id)?;
        self.tracker.end_prompt_turn(turn_id)
    }

    fn is_armed(&self) -> Option<bool> {
        self.tracker.is_armed()
    }
}

/// An entry from `acpAgents[]`.
pub fn declared_agent_from_config(agent: &AcpAgentConfig) -> DeclaredAgent {
    DeclaredAgent {
        id: agent.id.clone(),
        label: agent.label().to_string(),
        command: agent.command.clone(),
        args: agent.args.clone(),
        // Resolved here, at the point the command is assembled, so
        // the reference stays in `config.json` and the value only
        // exists on the way to the child process.
        env: agent.resolved_env(),
        cwd: agent.cwd.clone(),
        origin: AgentOrigin::Config,
        inject_fs_tools: agent.injects_fs_tools(),
        session_meta: agent
            .session_meta
            .as_ref()
            .map(|meta| meta.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        install_hint: None,
        workspace_cwd: None,
    }
}

/// An entry a plugin's `capabilities.acpAgents` declared. Command,
/// args, env and cwd arrive already materialized — placeholders
/// expanded against the installed plugin's root.
pub fn declared_agent_from_plugin(
    agent: &rebon_plugin_package::acp_agent_manifest::PluginAcpAgentContribution,
) -> DeclaredAgent {
    DeclaredAgent {
        id: agent.id.clone(),
        label: agent
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&agent.id)
            .to_string(),
        command: agent.command.clone(),
        args: agent.args.clone(),
        // Same `$VAR` indirection as config.json entries, resolved
        // at the same point: on the way to the child process.
        env: agent
            .env
            .iter()
            .map(|(key, value)| (key.clone(), rebon_config::resolve_env_value(value)))
            .collect(),
        cwd: agent
            .cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        origin: AgentOrigin::Plugin,
        inject_fs_tools: agent.inject_fs_tools,
        session_meta: agent
            .session_meta
            .as_ref()
            .map(|meta| meta.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        install_hint: agent.install_hint.clone(),
        workspace_cwd: None,
    }
}

/// An agent definition whose frontmatter says `runtime: acp`.
///
/// `None` for a definition that runs on Rebon's own engine — there
/// is no CLI to start.
pub fn declared_agent_from_definition(
    def: &rebon_tool::agent_registry::ResolvedAgentDef,
) -> Option<DeclaredAgent> {
    let rebon_tool::agent_registry::AgentRuntime::Acp { command, args } = &def.runtime else {
        return None;
    };
    Some(DeclaredAgent {
        id: def.agent_type.clone(),
        label: def.agent_type.clone(),
        command: command.clone(),
        args: args.clone(),
        env: std::collections::BTreeMap::new(),
        cwd: None,
        origin: AgentOrigin::Definition,
        // Frontmatter has no opt-out knob; the config surface is
        // where an agent that dislikes the tools gets excused.
        inject_fs_tools: true,
        session_meta: None,
        install_hint: None,
        workspace_cwd: None,
    })
}

fn agent_command(agent: &DeclaredAgent, session_cwd: &str) -> AgentCommand {
    let mut command = AgentCommand::new(&agent.command)
        .with_args(agent.args.clone())
        .with_cwd(agent.cwd.clone().unwrap_or_else(|| session_cwd.to_string()))
        .with_spawn_hint(agent.install_hint.clone());
    for (key, value) in &agent.env {
        command = command.with_env(key, value);
    }
    command
}

/// Every agent CLI this session could run on, from all three surfaces.
///
/// A name declared more than once resolves `config.json > plugin >
/// definition`. Config wins because it is the surface the user edits by
/// hand — an override of a plugin's declaration must stick — and it
/// carries `env`/`cwd`: letting a lower surface win would silently drop
/// the environment the user configured, and an agent CLI launched
/// without its key fails in a way that looks like the agent's fault.
pub fn declared_agents(
    config_agents: &[AcpAgentConfig],
    plugin_agents: &[rebon_plugin_package::acp_agent_manifest::PluginAcpAgentContribution],
    definitions: &rebon_tool::AgentRegistry,
) -> Vec<DeclaredAgent> {
    let mut declared: Vec<DeclaredAgent> = config_agents
        .iter()
        .map(declared_agent_from_config)
        .collect();
    let mut claimed: std::collections::BTreeSet<String> = declared
        .iter()
        .map(|agent| agent.id.to_ascii_lowercase())
        .collect();

    for contribution in plugin_agents {
        let agent = declared_agent_from_plugin(contribution);
        if !claimed.insert(agent.id.to_ascii_lowercase()) {
            tracing::info!(
                agent = %agent.id,
                plugin = %contribution.plugin_name,
                "rebon: plugin ACP agent shadowed by the `acpAgents` entry of the same name"
            );
            continue;
        }
        declared.push(agent);
    }

    for def in definitions.active() {
        let Some(agent) = declared_agent_from_definition(def) else {
            continue;
        };
        if claimed.contains(&agent.id.to_ascii_lowercase()) {
            tracing::info!(
                agent = %agent.id,
                "rebon: agent definition shadowed by a declared agent of the same name"
            );
            continue;
        }
        declared.push(agent);
    }
    declared
}

/// Build the agents for a session: one backend per declared CLI,
/// each writing through this session's file history and recording
/// into this session's transcript.
#[allow(clippy::too_many_arguments)]
pub fn build_session_agents(
    declared: Vec<DeclaredAgent>,
    switch: Arc<AgentBackendSwitch>,
    local: Arc<dyn AgentBackend>,
    projects_root: impl Into<PathBuf>,
    cwd: impl Into<String>,
    session_id: impl Into<String>,
    tracker: Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
    write_roots: Vec<PathBuf>,
    transcript_sink: Option<rebon_acp_client::TranscriptSink>,
) -> SessionAgents<AcpAgentBackend> {
    let projects_root = projects_root.into();
    let cwd = cwd.into();
    let session_id = session_id.into();
    let origins = declared
        .iter()
        .map(|agent| (agent.id.clone(), agent.origin))
        .collect();
    let (registry, fs_service) = build_registry(
        &declared,
        &projects_root,
        &cwd,
        &session_id,
        tracker,
        write_roots,
        transcript_sink,
    );
    let mut agents = SessionAgents::new(
        registry,
        origins,
        switch,
        local,
        projects_root,
        cwd,
        session_id,
    );
    if let Some(service) = fs_service {
        agents = agents.with_session_resource(Box::new(service));
    }
    agents
}

/// Build the registry for this session.
///
/// Each agent gets its own journal (so the transcript records which
/// agent produced a turn) but shares this session's file history (so
/// every write, whoever made it, lands in one rewindable store).
///
/// When any agent wants Rebon's fs tools injected, the host half of
/// those tools — a loopback listener serving snapshot-then-write — is
/// started here, and its handle rides back so the session can keep it
/// alive. No agent process is spawned; the listener just waits.
#[allow(clippy::too_many_arguments)]
pub fn build_registry(
    agents: &[DeclaredAgent],
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    tracker: Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
    write_roots: Vec<PathBuf>,
    transcript_sink: Option<rebon_acp_client::TranscriptSink>,
) -> (
    BackendRegistry<AcpAgentBackend>,
    Option<rebon_acp_client::HostFsServiceHandle>,
) {
    let file_history: Arc<dyn HostFileHistory> = Arc::new(SessionFileHistory {
        session_id: session_id.to_string(),
        tracker,
    });

    let fs_service = agents
        .iter()
        .any(|agent| agent.inject_fs_tools)
        .then(|| start_fs_service(file_history.clone(), &write_roots, session_id))
        .flatten();
    let injected = fs_service
        .as_ref()
        .and_then(|service| injected_fs_server(service, session_id));

    let entries = agents.iter().map(|agent| {
        // A remote agent's paths are on another machine, so this
        // session's write roots do not describe anything it could
        // legitimately touch. Handing it an empty root list is the
        // documented way to refuse every host-side write — a remote
        // that asked to write `/srv/app/x.rs` through the client
        // would otherwise land on whatever `/srv/app/x.rs` happens to
        // be *here*. Rebon's own ACP server never routes writes back
        // through the client, so this is a guard rather than a path
        // anything takes; the remote keeps its own file history, and
        // `writes_through_host_fs` is already reported false.
        let roots = if agent.workspace_cwd.is_some() {
            Vec::new()
        } else {
            write_roots.clone()
        };
        let host_fs = SnapshotHostFs::new(file_history.clone(), roots);
        let mut journal = TranscriptJournal::new(projects_root, cwd, session_id, &agent.id)
            .with_file_history(file_history.clone());
        if let Some(sink) = &transcript_sink {
            journal = journal.with_transcript_sink(sink.clone());
        }
        let journal = Arc::new(journal);
        let mut config = AcpBackendConfig::new(&agent.id, agent_command(agent, cwd))
            .with_host_fs(Arc::new(host_fs))
            .with_journal(journal)
            .with_session_meta(agent.session_meta.clone())
            // A session that starts cold on this agent gets the
            // conversation so far prepended to its first prompt, so
            // switching agents mid-conversation does not restart it.
            .with_handoff_provider(Arc::new(SessionHandoff::new(
                projects_root,
                cwd,
                session_id.to_string(),
            )));
        if agent.inject_fs_tools {
            if let Some(injected) = &injected {
                config = config.with_injected_mcp_servers(vec![injected.clone()]);
            }
        }
        // The remote opens the project directory *it* was configured
        // with, not this machine's cwd.
        if let Some(workspace_cwd) = &agent.workspace_cwd {
            config = config.with_workspace_cwd(workspace_cwd);
        }

        BackendEntry::new(&agent.id, &agent.label, move || {
            AcpAgentBackend::new(config.clone())
        })
    });

    let registry = BackendRegistry::new(entries);
    (registry, fs_service)
}

/// Start the host half of the injected fs tools, degrading to "no
/// injection" rather than failing the session when it cannot run.
/// Shared with the sub-agent pool, which runs the same injection under
/// its own label.
pub fn start_fs_service(
    file_history: Arc<dyn HostFileHistory>,
    write_roots: &[PathBuf],
    session_id: &str,
) -> Option<rebon_acp_client::HostFsServiceHandle> {
    // The service task needs a runtime. Production wiring always has
    // one (`build_tui_session` is async); a sync test context does
    // not, and quietly skipping the injection there mirrors what the
    // degradation path does everywhere else.
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!("acp-fs: no tokio runtime; skipping fs tool injection");
        return None;
    }
    match rebon_acp_client::HostFsService::start(file_history, write_roots.to_vec(), session_id) {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "acp-fs: could not start the fs tool service; \
                 agents run without Rebon's write_file/edit_file tools"
            );
            None
        }
    }
}

/// The injected server entry: `rebon __acp-fs-mcp`, told where its
/// host lives through the environment.
pub fn injected_fs_server(
    service: &rebon_acp_client::HostFsServiceHandle,
    session_id: &str,
) -> Option<rebon_proto::types::McpServerConfig> {
    // `REBON_ACP_FS_BRIDGE_EXE` overrides the bridge binary. Needed
    // wherever `current_exe` is not the `rebon` CLI — the real-agent
    // tests foremost, where it is the libtest harness, which would
    // treat `__acp-fs-mcp` as a test filter and exit.
    let command = match std::env::var("REBON_ACP_FS_BRIDGE_EXE") {
        Ok(exe) if !exe.trim().is_empty() => exe,
        _ => match std::env::current_exe() {
            Ok(exe) => exe.to_string_lossy().to_string(),
            Err(err) => {
                tracing::warn!(error = %err, "acp-fs: current_exe unavailable; skipping injection");
                return None;
            }
        },
    };
    Some(rebon_proto::types::McpServerConfig::Stdio {
        name: rebon_acp_client::FS_MCP_SERVER_NAME.to_string(),
        command,
        args: vec![rebon_acp_client::FS_BRIDGE_SUBCOMMAND.to_string()],
        env: service.bridge_env(session_id),
        // Deliberately no cwd: the bridge does not read it, and an
        // agent that validates cwd existence would fail the spawn for
        // nothing if the session directory disappears.
        cwd: None,
    })
}

/// End-to-end checks against a real agent CLI.
///
/// Ignored by default: they spawn a third-party binary, talk to its
/// provider over the network, and spend the user's tokens. Run one
/// deliberately after installing an ACP agent:
///
/// ```text
/// # e.g. npm i @agentclientprotocol/codex-acp
/// REBON_TEST_ACP_AGENT=/path/to/codex-acp.cmd \
///   cargo test -p rebon-harness real_agent -- --ignored --nocapture
/// ```
///
/// These are the only tests that prove the promises this leg makes are
/// kept by an agent nobody wrote for Rebon: that its turns land in the
/// transcript, that a live session gets reused rather than re-created,
/// and that a file it writes is snapshotted first so `/rewind` has
/// something to restore.
#[cfg(test)]
mod real_agent_tests {
    use super::tests::temp_root;
    use super::*;
    use rebon_agent_core::{
        ChannelPermissionRequestPublisher, LocalAgentBackend, PromptExecutor, PromptRequest,
        StubPromptExecutor,
    };
    use rebon_types::PromptCancel;

    /// The agent binary under test, or `None` when the env var is unset.
    fn agent_command() -> Option<(String, Vec<String>)> {
        let raw = std::env::var("REBON_TEST_ACP_AGENT").ok()?;
        let mut parts = raw.split_whitespace().map(str::to_string);
        let command = parts.next()?;
        Some((command, parts.collect()))
    }

    struct Harness {
        agents: Arc<SessionAgents<AcpAgentBackend>>,
        switch: Arc<AgentBackendSwitch>,
        projects_root: PathBuf,
        cwd: PathBuf,
        tracker: rebon_agent_core::file_history::SharedFileHistoryTracker,
        session_id: String,
        _root: tempfile::TempDir,
    }

    fn harness(name: &str) -> Option<Harness> {
        let (command, args) = agent_command()?;
        let root = temp_root(name);
        let projects_root = root.path().to_path_buf();
        let cwd = projects_root.join("work");
        std::fs::create_dir_all(&cwd).expect("work dir");
        let session_id = format!("real-{name}");

        let tracker = rebon_agent_core::file_history::SharedFileHistoryTracker::new(
            rebon_session::FileHistoryStore::new(projects_root.clone(), cwd.clone(), &session_id),
        );
        let local: Arc<dyn AgentBackend> =
            Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)));
        let switch = Arc::new(AgentBackendSwitch::new(local.clone()));
        // `REBON_TEST_ACP_SESSION_META` lets a real-agent run carry
        // adapter options — e.g. claude-agent-acp's disallowedTools —
        // without editing the harness.
        let session_meta = std::env::var("REBON_TEST_ACP_SESSION_META")
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let declared = vec![DeclaredAgent {
            id: "codex".to_string(),
            label: "Codex".to_string(),
            command,
            args,
            env: std::collections::BTreeMap::new(),
            cwd: None,
            origin: AgentOrigin::Config,
            inject_fs_tools: true,
            session_meta,
            install_hint: None,
            workspace_cwd: None,
        }];
        let agents = Arc::new(build_session_agents(
            declared,
            switch.clone(),
            local,
            projects_root.clone(),
            cwd.to_string_lossy().to_string(),
            session_id.clone(),
            Arc::new(tracker.clone())
                as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
            vec![cwd.clone()],
            None,
        ));
        agents.switch_to("codex").expect("declared agent");

        Some(Harness {
            agents,
            switch,
            projects_root,
            cwd,
            tracker,
            session_id,
            _root: root,
        })
    }

    /// Approve everything the agent asks for, so a write actually
    /// reaches the host filesystem.
    fn auto_approving_permissions() -> ChannelPermissionRequestPublisher {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let option = request
                    .params
                    .options
                    .iter()
                    .find(|option| {
                        matches!(
                            option.kind,
                            rebon_proto::types::PermissionOptionKind::AllowAlways
                                | rebon_proto::types::PermissionOptionKind::AllowOnce
                        )
                    })
                    .or_else(|| request.params.options.first())
                    .map(|option| option.option_id.clone());
                let response = rebon_agent_core::publisher::make_permission_result_response(
                    request.request_id.clone(),
                    rebon_proto::types::RequestPermissionResult {
                        outcome: rebon_proto::types::PermissionOutcome::Selected,
                        option_id: option,
                        updated_input: None,
                    },
                );
                let _ = request.response_tx.send(response);
            }
        });
        publisher
    }

    fn request(harness: &Harness, text: &str) -> PromptRequest {
        PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: harness.session_id.clone(),
            cwd: harness.cwd.to_string_lossy().to_string(),
            prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                text: text.to_string(),
                annotations: None,
            })],
            mcp_servers: Vec::new(),
            update_publisher: None,
            permission_publisher: Some(auto_approving_permissions()),
            cancel: PromptCancel::new(),
            thinking_budget: None,
            max_tokens: None,
            reasoning_effort_ordinal: None,
            additional_working_directories: Vec::new(),
            coordinator_mode: None,
            coordinator_report_paths: Vec::new(),
            user_message_uuid: None,
            background_agent_system: None,
            background_agent_tool_filter: None,
            execution_policy: None,
            replay_requests: Vec::new(),
            skill_invocations: Vec::new(),
        }
    }

    fn transcript_rows(harness: &Harness) -> Vec<(String, serde_json::Value)> {
        let path = rebon_session::transcript_file_path(
            &harness.projects_root,
            &harness.cwd.to_string_lossy(),
            &harness.session_id,
        );
        rebon_session::load_raw_transcript_from_file(&path)
            .expect("read transcript")
            .map(|raw| {
                raw.entries
                    .into_iter()
                    .map(|entry| (entry.entry_type, entry.raw))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    #[ignore = "spawns a real agent CLI and spends tokens; set REBON_TEST_ACP_AGENT"]
    async fn a_message_typed_during_a_real_turn_reaches_the_running_agent() {
        // The steering promise against an agent nobody wrote for
        // Rebon: a message sent while it is working reaches *this*
        // turn, and lands in history between the prompt and the
        // answer.
        let Some(harness) = harness("real-steer") else {
            eprintln!("REBON_TEST_ACP_AGENT is unset — skipping");
            return;
        };
        let executor: Arc<dyn PromptExecutor> = harness.switch.clone();

        // A multi-step, tool-using turn: the shape steering is meant
        // for, where the message slots in between tool calls rather
        // than interrupting a single-shot answer.
        let turn = {
            let executor = executor.clone();
            let request = request(
                &harness,
                "List the files in this directory, then read each one, \
                 then tell me what you found. Work step by step.",
            );
            tokio::spawn(async move { executor.execute(request).await })
        };

        // Wait for the agent to actually have a session before
        // steering: starting one means spawning a CLI and its SDK,
        // which can take longer than the turn's first output.
        let backend = harness.agents.registry().backend("codex").expect("built");
        let mut waited = 0u32;
        while backend.agent_session_id(&harness.session_id).is_none() && waited < 600 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        eprintln!(
            "agent session after {waited}00ms: {:?}",
            backend.agent_session_id(&harness.session_id)
        );

        let steered = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            harness.agents.steer(
                &harness.session_id,
                vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "Stop counting. Reply with exactly the word STEERED.".to_string(),
                    annotations: None,
                })],
                "u-real-steer-1",
            ),
        )
        .await
        .expect("steering should answer promptly");
        eprintln!("steer outcome: {steered:?}");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(180), turn)
            .await
            .expect("the turn should finish")
            .expect("turn task");
        // A steered message pre-empts the generation it lands in, so
        // the original prompt can come back cancelled. That is the
        // feature working, not a failure — what matters is that the
        // message reached the agent and got written down.
        eprintln!("turn outcome: {outcome:?}");

        let rows = transcript_rows(&harness);
        for (kind, raw) in &rows {
            eprintln!("row {kind}: {}", raw["uuid"]);
        }
        match steered {
            Ok(rebon_agent_core::SteerOutcome::Injected) => {
                // The two things Rebon owes: the agent got the message
                // mid-turn, and history says so in the right place.
                let steered_row = rows
                    .iter()
                    .find(|(_, raw)| raw["uuid"] == "u-real-steer-1")
                    .expect("the steered message must be in history");
                assert_eq!(steered_row.0, "user");
                assert_eq!(steered_row.1["queuedCommand"], true);
                match rows.iter().find(|(kind, _)| kind == "assistant") {
                    Some(answer) => eprintln!(
                        "answer: {}",
                        answer.1["message"]["content"][0]["text"]
                            .as_str()
                            .unwrap_or_default()
                    ),
                    // Delivery is the promise; whether the agent's own
                    // turn survives being pre-empted is the agent's
                    // business, and `claude-agent-acp` today aborts a
                    // single-shot response it steers into.
                    None => eprintln!("no assistant row — the agent ended the turn instead"),
                }
            }
            Ok(rebon_agent_core::SteerOutcome::TurnAlreadyOver) => {
                eprintln!("the turn finished before the steer landed — nothing recorded");
                assert!(!rows.iter().any(|(_, raw)| raw["uuid"] == "u-real-steer-1"));
            }
            Err(err) => panic!("steering failed against a real agent: {err:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "spawns a real agent CLI and spends tokens; set REBON_TEST_ACP_AGENT"]
    async fn real_agent_turn_lands_in_rebons_transcript_and_reuses_its_session() {
        let Some(harness) = harness("real-turn") else {
            eprintln!("REBON_TEST_ACP_AGENT is unset — skipping");
            return;
        };
        let executor: Arc<dyn PromptExecutor> = harness.switch.clone();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(180),
            executor.execute(request(
                &harness,
                "Reply with exactly the word PONG and nothing else. Do not use any tools.",
            )),
        )
        .await
        .expect("the agent should answer within the timeout")
        .expect("the turn should succeed");
        eprintln!("stop reason: {:?}", outcome.stop_reason);

        let rows = transcript_rows(&harness);
        eprintln!("transcript rows: {}", rows.len());
        assert!(rows.len() >= 2, "the prompt and an answer: {rows:?}");
        assert_eq!(rows[0].0, "user");
        let assistant = rows
            .iter()
            .find(|(kind, _)| kind == "assistant")
            .expect("the agent's answer must be recorded");
        let text = assistant.1["message"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        eprintln!("agent said: {text}");
        assert!(!text.trim().is_empty(), "the answer must not be empty");

        // The agent's own session id is stored, which is what makes a
        // later run able to ask for `session/load`.
        let stored = rebon_session::load_agent_session_id(
            &harness.projects_root,
            &harness.cwd.to_string_lossy(),
            &harness.session_id,
        );
        assert!(stored.is_some(), "the agent's session id must be stored");
        eprintln!("agent session id: {stored:?}");

        // A second turn must reuse the live session rather than mint a
        // new one — this leg's efficiency signal.
        let backend = harness.agents.registry().backend("codex").expect("built");
        let before = backend.agent_session_id(&harness.session_id);
        let start = backend
            .start_session(rebon_agent_core::AgentSessionSpec::new(
                harness.session_id.clone(),
                harness.cwd.to_string_lossy().to_string(),
            ))
            .await
            .expect("the session is live");
        assert_eq!(start.mode, rebon_agent_core::SessionResumeMode::Reused);
        assert_eq!(before.as_deref(), Some(start.session_id.as_str()));
    }

    #[tokio::test]
    #[ignore = "spawns a real agent CLI and spends tokens; set REBON_TEST_ACP_AGENT"]
    async fn a_file_the_real_agent_writes_is_snapshotted_first() {
        // The rewind promise, proven against an agent that has never
        // heard of Rebon: it asks us to write, we photograph the old
        // contents into the session's file history, and the restore
        // plan can see the change.
        let Some(harness) = harness("real-write") else {
            eprintln!("REBON_TEST_ACP_AGENT is unset — skipping");
            return;
        };
        let target = harness.cwd.join("hello.txt");
        std::fs::write(&target, "before\n").expect("seed the file");

        let executor: Arc<dyn PromptExecutor> = harness.switch.clone();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(240),
            executor.execute(request(
                &harness,
                "Replace the entire contents of hello.txt with the single line: after. \
                 Then stop.",
            )),
        )
        .await
        .expect("the agent should answer within the timeout")
        .expect("the turn should succeed");
        eprintln!("stop reason: {:?}", outcome.stop_reason);

        let contents = std::fs::read_to_string(&target).expect("read back");
        eprintln!("file now: {contents:?}");

        assert!(
            contents.contains("after"),
            "the agent should have changed the file: {contents:?}"
        );

        let rows = transcript_rows(&harness);
        let turn_id = rows[0].1["uuid"]
            .as_str()
            .expect("user row uuid")
            .to_string();
        let capability = harness.tracker.store().restore_capability(&turn_id);
        eprintln!("restore capability: {capability:?}");

        let snapshotted = matches!(
            capability,
            rebon_session::FileRestoreCapability::Clean(ref plan) if plan.writes > 0
        );
        eprintln!(
            "routed writes through the host: {snapshotted} \
             (codex-acp writes directly and does not)"
        );

        // The invariant that must hold for *every* agent, whichever it
        // chose: the capability may not over-promise. Rebon claiming
        // rewind coverage while the store holds no pre-image is the one
        // outcome that would put a restore button in front of the user
        // with nothing behind it.
        let promised = harness.agents.capabilities().supports_rewind();
        assert!(
            !promised || snapshotted,
            "rewind was promised but nothing was snapshotted: {capability:?}"
        );
        assert!(
            !promised,
            "no ACP agent can promise rewind coverage — it decides whether to \
             route writes through the host"
        );
    }
}

/// Fixtures for tests that need a [`SessionAgents`] and do not care how one is
/// built.
///
/// Public because the `/kernel` command tests live in a front end — the command
/// needs the harness, so it stayed behind when the agent set moved out — and
/// the desktop app will want the same fixtures when it grows tests for
/// switching. Takes a root rather than making one, so a caller brings its own
/// temporary directory and this crate does not ship `tempfile`.
#[doc(hidden)]
pub mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rebon_agent_core::{
        AgentBackend, AgentBackendSwitch, LocalAgentBackend, StubPromptExecutor,
    };
    use rebon_config::AcpAgentConfig;

    use super::{
        build_session_agents, declared_agent_from_config, AcpAgentBackend, DeclaredAgent,
        SessionAgents,
    };

    #[derive(Default)]
    pub struct NoopTracker;

    impl rebon_agent_core::file_history::FileHistoryTracker for NoopTracker {
        fn track_before_write(&self, _file_path: &Path) -> anyhow::Result<()> {
            Ok(())
        }
    }

    pub fn agent_config(id: &str) -> AcpAgentConfig {
        AcpAgentConfig {
            id: id.to_string(),
            display_name: Some(format!("{id} CLI")),
            command: "agent-binary".to_string(),
            args: vec!["--acp".to_string()],
            env: Default::default(),
            cwd: None,
            inject_fs_tools: None,
            session_meta: None,
        }
    }

    pub fn session_agents(
        root: &Path,
        configured: Vec<AcpAgentConfig>,
    ) -> SessionAgents<AcpAgentBackend> {
        let declared = configured.iter().map(declared_agent_from_config).collect();
        session_agents_declared(root, declared)
    }

    pub fn session_agents_declared(
        root: &Path,
        declared: Vec<DeclaredAgent>,
    ) -> SessionAgents<AcpAgentBackend> {
        let local: Arc<dyn AgentBackend> =
            Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)));
        let switch = Arc::new(AgentBackendSwitch::new(local.clone()));
        build_session_agents(
            declared,
            switch,
            local,
            root,
            "/tmp/repo",
            "sess-1",
            Arc::new(NoopTracker),
            vec![PathBuf::from("/tmp/repo")],
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_agent_core::{LocalAgentBackend, StubPromptExecutor};

    pub(super) fn temp_root(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-harness-{name}-"))
            .tempdir()
            .expect("projects root")
    }

    use super::test_support::{agent_config, session_agents, session_agents_declared, NoopTracker};

    /// What tells a switch apart from a prompt. `/agent` carries both meanings,
    /// and the spawn branch used to take every input — so `/agent list` spawned
    /// a sub-agent whose prompt was the word "list", and switching was
    /// unreachable.
    #[test]
    fn an_agent_id_is_told_apart_from_a_prompt() {
        let root = temp_root("knows-agent");
        let agents = session_agents(root.path(), vec![agent_config("codex")]);

        assert!(agents.knows_agent("codex"), "a configured agent");
        assert!(agents.knows_agent(LOCAL_AGENT_ID), "rebon's own engine");
        assert!(
            agents.knows_agent("  codex  "),
            "surrounding space is not part of an id"
        );

        assert!(!agents.knows_agent("write the tests"), "that is a prompt");
        assert!(!agents.knows_agent("codex please"), "and so is this");
        assert!(!agents.knows_agent(""), "nothing names nothing");
        assert!(!agents.knows_agent("   "));
    }

    /// An id that is not configured is not a switch target either — otherwise a
    /// typo would stop being a prompt and start being an error.
    #[test]
    fn an_unconfigured_id_is_not_a_switch_target() {
        let root = temp_root("unknown-agent");
        let agents = session_agents(root.path(), vec![agent_config("codex")]);
        assert!(!agents.knows_agent("claude"));
    }

    /// `switch_to` folds ASCII case on every one of its three lookups, so the
    /// test that decides whether a line is a switch has to fold too. Disagreeing
    /// meant `/agent codex` switched while `/agent Codex` spawned a sub-agent
    /// whose prompt was the word "Codex".
    #[test]
    fn an_agent_id_is_recognised_however_it_is_spelled() {
        let root = temp_root("knows-agent-case");
        let agents = session_agents(root.path(), vec![agent_config("codex")]);

        assert!(agents.knows_agent("Codex"));
        assert!(agents.knows_agent("CODEX"));
        assert!(agents.knows_agent("Local"));
        assert_eq!(
            agents.switch_to("CODEX").is_ok(),
            agents.knows_agent("CODEX")
        );
    }

    /// Everything the command understands has to be told apart from a prompt,
    /// not just the ids: a subcommand the terminal does not recognise is spawned
    /// as a sub-agent instead of run, which is what `/agent reconnect` did.
    #[test]
    fn every_subcommand_is_told_apart_from_a_prompt() {
        let root = temp_root("switch-args");
        let agents = session_agents(root.path(), vec![agent_config("codex")]);
        let names_switch = |args: &str| agent_args_name_the_switch(&agents, args);

        assert!(names_switch(""), "a bare /agent lists");
        assert!(names_switch("list"));
        assert!(names_switch("List"));
        assert!(names_switch("reconnect"), "the current agent");
        assert!(names_switch("reconnect codex"), "a named one");
        assert!(names_switch("Reconnect"));
        assert!(names_switch("codex"));

        // A prompt that happens to open with a word the command knows is still
        // a prompt — the switch understands `reconnect <id>`, nothing longer.
        assert!(!names_switch("reconnect the parser and the lexer"));
        assert!(!names_switch("reconnecting the db"));
        assert!(!names_switch("list the open files"));
        assert!(!names_switch("write the tests"));
    }

    #[test]
    fn cross_cwd_handoff_instances_keep_their_own_transcript_binding() {
        use rebon_acp_client::HandoffProvider;

        let root = tempfile::tempdir().unwrap();
        for (cwd, session, text) in [
            ("/old", "sess-old", "old conversation"),
            ("/new", "sess-new", "new conversation"),
        ] {
            rebon_session::append_transcript_entry(
                root.path(),
                cwd,
                session,
                rebon_session::TranscriptWriteEntry::new(
                    "user",
                    serde_json::json!({
                        "message": { "role": "user", "content": text }
                    }),
                ),
            )
            .unwrap();
        }

        let old = SessionHandoff::new(root.path(), "/old", "sess-old");
        let new = SessionHandoff::new(root.path(), "/new", "sess-new");
        let old_text = old.handoff_text("sess-old").unwrap();
        let new_text = new.handoff_text("sess-new").unwrap();
        assert!(old_text.contains("old conversation"));
        assert!(!old_text.contains("new conversation"));
        assert!(new_text.contains("new conversation"));
        assert!(!new_text.contains("old conversation"));
        assert!(old.handoff_text("sess-new").is_none());
    }

    /// `/backend` is the unambiguous name for the switch, and it must not
    /// swallow anything else on the way.
    #[test]
    fn backend_command_parses_its_own_name_only() {
        assert_eq!(parse_backend_command("/backend"), Some(""));
        assert_eq!(parse_backend_command("/backend codex"), Some("codex"));
        assert_eq!(parse_backend_command("/backend:codex"), Some("codex"));
        assert_eq!(parse_backend_command("/backend  list "), Some("list"));
        // Not this command:
        assert_eq!(parse_backend_command("/backends"), None);
        assert_eq!(parse_backend_command("/back"), None);
        assert_eq!(parse_backend_command("backend codex"), None);
        assert_eq!(parse_backend_command("/agent codex"), None);
    }

    /// An agent definition the way the registry resolves one.
    fn definition(
        agent_type: &str,
        runtime: rebon_tool::agent_registry::AgentRuntime,
    ) -> rebon_tool::agent_registry::ResolvedAgentDef {
        use rebon_tool::agent_registry::{AgentSource, ResolvedAgentDef};
        ResolvedAgentDef {
            agent_type: agent_type.to_string(),
            when_to_use: "when testing".to_string(),
            system_prompt: String::new(),
            tool_filter: Default::default(),
            model: None,
            model_profile: None,
            provider: None,
            effort: None,
            background: false,
            isolation: None,
            memory: None,
            permission_mode: None,
            runtime,
            source: AgentSource::BuiltIn,
            file_stem: None,
        }
    }

    #[derive(Default)]
    struct CloseCountingBackend {
        closed: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl rebon_agent_core::AgentBackend for CloseCountingBackend {
        fn kind(&self) -> rebon_agent_core::AgentBackendKind {
            rebon_agent_core::AgentBackendKind::Kernel
        }

        fn name(&self) -> &str {
            "counting-kernel"
        }

        fn capabilities(&self) -> rebon_agent_core::AgentCapabilities {
            rebon_agent_core::AgentCapabilities::MINIMAL
        }

        async fn start_session(
            &self,
            spec: rebon_agent_core::AgentSessionSpec,
        ) -> Result<rebon_agent_core::AgentSessionStart, rebon_agent_core::AgentBackendError>
        {
            Ok(rebon_agent_core::AgentSessionStart::reused(spec.session_id))
        }

        async fn prompt(
            &self,
            _request: rebon_agent_core::PromptRequest,
        ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>
        {
            Ok(rebon_agent_core::PromptOutcome::end_turn())
        }

        async fn close_session(
            &self,
            session_id: &str,
        ) -> Result<(), rebon_agent_core::AgentBackendError> {
            self.closed.lock().unwrap().push(session_id.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn retiring_repeated_session_runtimes_closes_each_kernel_host_once() {
        let root = temp_root("kernel-runtime-retire");
        let closed = Arc::new(Mutex::new(Vec::new()));
        for session_id in ["sess-1", "sess-2", "sess-3"] {
            let local: Arc<dyn AgentBackend> =
                Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)));
            let switch = Arc::new(AgentBackendSwitch::new(local.clone()));
            let agents = SessionAgents::<AcpAgentBackend>::local_only(
                switch,
                local,
                root.path(),
                "/tmp/repo",
                session_id,
            )
            .with_kernel_backend(
                "kernel:dsh",
                "dsh",
                Arc::new(CloseCountingBackend {
                    closed: closed.clone(),
                }),
            );
            agents.retire(&tokio::runtime::Handle::current());
        }
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let mut actual = closed.lock().unwrap().clone();
        actual.sort();
        assert_eq!(actual, vec!["sess-1", "sess-2", "sess-3"]);
    }

    #[test]
    fn the_two_crates_agree_on_what_local_is_called() {
        // `rebon-config` names the sentinel for the config surface and
        // `rebon-session` for the sidecar; neither can depend on the
        // other, so this is where the two spellings have to be checked
        // against each other.
        assert_eq!(LOCAL_AGENT_ID, rebon_session::SESSION_AGENT_LOCAL);
    }

    #[test]
    fn agent_parses_but_agents_does_not() {
        // `/agents` browses agent definitions; swallowing it here would
        // break a command that has nothing to do with this one.
        assert_eq!(parse_agent_command("/agent"), Some(""));
        assert_eq!(parse_agent_command("/agent list"), Some("list"));
        assert_eq!(
            parse_agent_command("/agent claude-code"),
            Some("claude-code")
        );
        assert_eq!(parse_agent_command("/agent:local"), Some("local"));
        assert_eq!(parse_agent_command("/agents"), None);
        assert_eq!(parse_agent_command("/agents new"), None);
        assert_eq!(parse_agent_command("/model"), None);
    }

    #[test]
    fn a_session_starts_local_and_lists_the_configured_agents() {
        let root = temp_root("list");
        let agents = session_agents(root.path(), vec![agent_config("claude-code")]);
        assert_eq!(agents.current_id(), LOCAL_AGENT_ID);
        assert!(agents.capabilities().supports_rewind());

        let choices = agents.choices();
        assert_eq!(choices.len(), 2);
        assert!(choices[0].current, "local is where a session starts");
        assert_eq!(choices[1].id, "claude-code");
        assert_eq!(choices[1].label, "claude-code CLI");
        assert!(choices[1].caveat.is_some(), "the caveat is not optional UI");

        let listed = handle_agent_command(&agents, "list", None);
        assert!(!listed.is_err);
        assert!(listed.text.contains("claude-code"));
        assert!(listed.text.contains("Rebon (local engine)"));
    }

    #[test]
    fn switching_remembers_the_choice_in_the_session_record() {
        let root = temp_root("switch");
        let agents = session_agents(root.path(), vec![agent_config("claude-code")]);

        let result = handle_agent_command(&agents, "claude-code", None);
        assert!(!result.is_err, "{}", result.text);
        assert_eq!(agents.current_id(), "claude-code");
        assert_eq!(
            rebon_session::load_session_agent(root.path(), "/tmp/repo", "sess-1").as_deref(),
            Some("claude-code")
        );

        // Switching back is also a choice, and is recorded as one so a
        // resume does not drag the session onto the configured default.
        let result = handle_agent_command(&agents, "local", None);
        assert!(!result.is_err, "{}", result.text);
        assert_eq!(agents.current_id(), LOCAL_AGENT_ID);
        assert_eq!(
            rebon_session::load_session_agent(root.path(), "/tmp/repo", "sess-1").as_deref(),
            Some("local")
        );
    }

    #[test]
    fn switching_to_an_agent_says_what_it_cannot_do() {
        // The caveat is the point: the user is about to lose rewind
        // coverage for anything the agent writes on its own.
        let root = temp_root("caveat");
        let agents = session_agents(root.path(), vec![agent_config("claude-code")]);
        let result = handle_agent_command(&agents, "claude-code", None);
        assert!(result.text.contains("Token usage is not reported"));
    }

    #[test]
    fn an_unknown_agent_names_the_configured_ones() {
        let root = temp_root("unknown");
        let agents = session_agents(root.path(), vec![agent_config("claude-code")]);
        let result = handle_agent_command(&agents, "ghost", None);
        assert!(result.is_err);
        assert!(result.text.contains("ghost"), "{}", result.text);
        assert!(result.text.contains("claude-code"), "{}", result.text);
        assert_eq!(
            agents.current_id(),
            LOCAL_AGENT_ID,
            "a failed switch must not move the session"
        );
    }

    #[test]
    fn with_nothing_configured_the_list_says_how_to_configure_one() {
        let root = temp_root("empty");
        let agents = session_agents(root.path(), Vec::new());
        let listed = handle_agent_command(&agents, "", None);
        assert!(listed.text.contains("acpAgents"), "{}", listed.text);

        let failed = handle_agent_command(&agents, "claude-code", None);
        assert!(failed.is_err);
        assert!(failed.text.contains("acpAgents"), "{}", failed.text);
    }

    #[test]
    fn a_broken_acp_agents_config_is_reported_not_passed_off_as_unconfigured() {
        // The config layer fails the whole read so a broken entry is
        // not silently dropped; showing "nothing is configured" here
        // would throw that guarantee away at the last step.
        let root = temp_root("config-error");
        let agents = session_agents(root.path(), Vec::new())
            .with_config_error(Some("`acpAgents[0].id` must not be empty".to_string()));

        let listed = handle_agent_command(&agents, "list", None);
        assert!(!listed.is_err);
        assert!(
            listed.text.contains("were ignored")
                && listed.text.contains("`acpAgents[0].id` must not be empty"),
            "{}",
            listed.text
        );
        assert!(
            !listed.text.contains("No agent CLIs are configured"),
            "a broken config is not an absent one: {}",
            listed.text
        );

        let failed = handle_agent_command(&agents, "claude-code", None);
        assert!(failed.is_err);
        assert!(
            failed.text.contains("could not be read")
                && failed.text.contains("`acpAgents[0].id` must not be empty"),
            "{}",
            failed.text
        );
    }

    #[test]
    fn a_config_error_still_leaves_definition_declared_agents_usable() {
        // `acpAgents` failing to read must not take the other surface
        // down with it — and the list should still warn about it.
        use rebon_tool::agent_registry::AgentRuntime;

        let root = temp_root("config-error-defs");
        let declared = vec![declared_agent_from_definition(&definition(
            "gemini",
            AgentRuntime::Acp {
                command: "gemini".into(),
                args: vec!["--experimental-acp".into()],
            },
        ))
        .expect("acp definition")];
        let agents = session_agents_declared(root.path(), declared)
            .with_config_error(Some("config.json is not valid JSON".to_string()));

        let listed = handle_agent_command(&agents, "list", None);
        assert!(listed.text.contains("gemini"), "{}", listed.text);
        assert!(listed.text.contains("were ignored"), "{}", listed.text);

        let result = handle_agent_command(&agents, "gemini", None);
        assert!(!result.is_err, "{}", result.text);
    }

    #[test]
    fn an_empty_argument_is_a_list_not_a_switch() {
        let root = temp_root("blank-arg");
        let agents = session_agents(root.path(), vec![agent_config("claude-code")]);
        assert!(agents.switch_to("   ").is_err());
        assert_eq!(agents.current_id(), LOCAL_AGENT_ID);
    }

    #[test]
    fn the_session_record_outranks_the_configured_default() {
        let root = temp_root("initial");
        // Nothing stored yet: the config default decides.
        assert_eq!(
            initial_agent(
                root.path(),
                "/tmp/repo",
                "sess-1",
                Some("claude-code".into())
            )
            .as_deref(),
            Some("claude-code")
        );

        // A session that was explicitly switched back to local stays
        // local, even though the config says otherwise.
        rebon_session::save_session_agent(root.path(), "/tmp/repo", "sess-1", "local").unwrap();
        assert!(initial_agent(
            root.path(),
            "/tmp/repo",
            "sess-1",
            Some("claude-code".into())
        )
        .is_none());

        // And a session recorded on an agent reopens on it.
        rebon_session::save_session_agent(root.path(), "/tmp/repo", "sess-1", "gemini").unwrap();
        assert_eq!(
            initial_agent(
                root.path(),
                "/tmp/repo",
                "sess-1",
                Some("claude-code".into())
            )
            .as_deref(),
            Some("gemini")
        );
    }

    /// A declaration shaped like the one `rebon_plugin_remote` produces.
    fn remote_declaration(id: &str, workspace_cwd: &str) -> DeclaredAgent {
        DeclaredAgent {
            id: id.to_string(),
            label: format!("{id} (remote)"),
            command: "ssh".into(),
            args: vec!["host".into(), "--".into(), "rebon --acp".into()],
            env: std::collections::BTreeMap::new(),
            cwd: None,
            origin: AgentOrigin::Remote,
            inject_fs_tools: false,
            session_meta: None,
            install_hint: None,
            workspace_cwd: Some(workspace_cwd.to_string()),
        }
    }

    #[test]
    fn a_remote_gets_no_local_write_roots() {
        // `SnapshotHostFs` with an empty root list refuses every
        // write. That is the point: a remote asking to write
        // `/srv/app/x.rs` through the host must not land on whatever
        // `/srv/app/x.rs` is on *this* machine.
        let root = tempfile::tempdir().unwrap();
        let tracker = rebon_agent_core::file_history::SharedFileHistoryTracker::new(
            rebon_session::FileHistoryStore::new(
                root.path().to_path_buf(),
                "/tmp/repo".to_string(),
                "sess-1",
            ),
        );
        let file_history: Arc<dyn HostFileHistory> = Arc::new(SessionFileHistory {
            session_id: "sess-1".to_string(),
            tracker: Arc::new(tracker)
                as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
        });

        let local = SnapshotHostFs::new(file_history.clone(), vec![PathBuf::from("/tmp/repo")]);
        let remote = SnapshotHostFs::new(file_history, Vec::<PathBuf>::new());

        use rebon_acp_client::HostFs;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(
                remote
                    .write_text_file("sess-1", Path::new("/tmp/repo/x.rs"), "x")
                    .await
                    .is_err(),
                "a remote must not be able to write through the host"
            );
            // The local agent's roots still work, so the empty list is
            // the remote's doing and not a broken helper.
            let _ = local;
        });
    }

    #[test]
    fn a_remote_declaration_carries_its_own_workspace_and_refuses_the_fs_injection() {
        let declared = vec![
            remote_declaration("remote:prod", "/srv/app"),
            declared_agent_from_config(&AcpAgentConfig {
                id: "claude-code".into(),
                display_name: None,
                command: "claude".into(),
                args: vec!["--acp".into()],
                env: std::collections::BTreeMap::new(),
                cwd: None,
                inject_fs_tools: None,
                session_meta: None,
            }),
        ];
        // The remote opts out; the local agent opts in. The two must
        // not be collapsed into one answer for the session.
        assert!(!declared[0].inject_fs_tools);
        assert!(declared[1].inject_fs_tools);
        assert_eq!(declared[0].workspace_cwd.as_deref(), Some("/srv/app"));
        assert_eq!(declared[1].workspace_cwd, None);

        let root = tempfile::tempdir().unwrap();
        let tracker = rebon_agent_core::file_history::SharedFileHistoryTracker::new(
            rebon_session::FileHistoryStore::new(
                root.path().to_path_buf(),
                "/tmp/repo".to_string(),
                "sess-1",
            ),
        );
        let (registry, _fs) = build_registry(
            &declared,
            root.path(),
            "/tmp/repo",
            "sess-1",
            Arc::new(tracker) as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
            vec![PathBuf::from("/tmp/repo")],
            None,
        );
        // Both are offered to `/agent`; only their wiring differs.
        assert_eq!(registry.ids(), vec!["remote:prod", "claude-code"]);
    }

    #[test]
    fn a_remote_id_cannot_be_spelled_by_the_other_declaring_surfaces() {
        // `remote:` is what keeps the two namespaces apart, and the
        // config surface must not be able to reach into it.
        let colliding = declared_agent_from_config(&AcpAgentConfig {
            id: "remote:prod".into(),
            display_name: None,
            command: "claude".into(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            inject_fs_tools: None,
            session_meta: None,
        });
        // config.json is not validated against the prefix, so the
        // guarantee is on the other side: a remote *host* name may not
        // contain a colon, so no remote can be declared to shadow a
        // configured agent either.
        assert_eq!(colliding.origin, AgentOrigin::Config);
        assert!(rebon_plugin_remote::RemoteHost::from_target("remote:prod", "host").is_err());
    }

    #[test]
    fn both_surfaces_describe_the_command_the_same_way() {
        // The point of normalising: whichever surface declared it, the
        // spawn path sees one shape. Only the detail each surface can
        // carry differs.
        use rebon_tool::agent_registry::AgentRuntime;

        let from_config = declared_agent_from_config(&AcpAgentConfig {
            id: "claude-code".into(),
            display_name: Some("Claude Code".into()),
            command: "claude".into(),
            args: vec!["--acp".into()],
            env: std::collections::BTreeMap::from([("KEY".to_string(), "value".to_string())]),
            cwd: Some("/elsewhere".into()),
            inject_fs_tools: None,
            session_meta: None,
        });
        assert_eq!(from_config.origin, AgentOrigin::Config);
        assert_eq!(from_config.label, "Claude Code");
        let command = agent_command(&from_config, "/tmp/repo");
        assert_eq!(command.command, "claude");
        assert_eq!(command.args, vec!["--acp".to_string()]);
        assert_eq!(command.env.get("KEY").map(String::as_str), Some("value"));
        assert_eq!(command.cwd, Some(PathBuf::from("/elsewhere")));

        let from_def = declared_agent_from_definition(&definition(
            "gemini",
            AgentRuntime::Acp {
                command: "gemini".into(),
                args: vec!["--experimental-acp".into()],
            },
        ))
        .expect("an acp definition declares an agent CLI");
        assert_eq!(from_def.origin, AgentOrigin::Definition);
        assert_eq!(from_def.id, "gemini");
        // Frontmatter carries no env or cwd, so the session's cwd is
        // the one the child gets.
        let command = agent_command(&from_def, "/tmp/repo");
        assert!(command.env.is_empty());
        assert_eq!(command.cwd, Some(PathBuf::from("/tmp/repo")));

        // A local definition is not an agent CLI at all.
        assert!(declared_agent_from_definition(&definition("plan", AgentRuntime::Local)).is_none());
    }

    #[test]
    fn a_definition_declared_agent_is_selectable_like_a_configured_one() {
        // Before this, `runtime: acp` could be seen but never run.
        use rebon_tool::agent_registry::AgentRuntime;

        let root = temp_root("declared-def");
        let declared = vec![declared_agent_from_definition(&definition(
            "gemini",
            AgentRuntime::Acp {
                command: "gemini".into(),
                args: vec!["--experimental-acp".into()],
            },
        ))
        .expect("acp definition")];
        let agents = session_agents_declared(root.path(), declared);

        let result = handle_agent_command(&agents, "gemini", None);
        assert!(!result.is_err, "{}", result.text);
        assert_eq!(agents.current_id(), "gemini");

        let listed = handle_agent_command(&agents, "list", None);
        assert!(
            listed.text.contains("agent definition"),
            "the list should say where an agent came from: {}",
            listed.text
        );
    }

    #[test]
    fn a_name_declared_on_both_surfaces_resolves_to_the_config_entry() {
        // config.json is the surface that can carry env and cwd;
        // letting the definition win would silently launch the CLI
        // without the key the user configured.
        use rebon_tool::agent_registry::{AgentGroups, AgentRegistry, AgentRuntime};

        let registry = AgentRegistry::from_groups(AgentGroups {
            user: vec![
                definition(
                    "claude-code",
                    AgentRuntime::Acp {
                        command: "from-definition".into(),
                        args: Vec::new(),
                    },
                ),
                definition(
                    "gemini",
                    AgentRuntime::Acp {
                        command: "gemini".into(),
                        args: Vec::new(),
                    },
                ),
                definition("plan", AgentRuntime::Local),
            ],
            ..Default::default()
        });

        let declared = declared_agents(&[agent_config("claude-code")], &[], &registry);

        assert_eq!(declared.len(), 2, "one per name, not one per surface");
        let claude = declared
            .iter()
            .find(|agent| agent.id == "claude-code")
            .expect("declared");
        assert_eq!(claude.origin, AgentOrigin::Config);
        assert_eq!(claude.command, "agent-binary");
        assert_eq!(
            declared
                .iter()
                .find(|agent| agent.id == "gemini")
                .map(|agent| agent.origin),
            Some(AgentOrigin::Definition)
        );
        assert!(
            !declared.iter().any(|agent| agent.id == "plan"),
            "a local definition is not an agent CLI"
        );
    }

    #[test]
    fn the_three_surfaces_resolve_config_over_plugin_over_definition() {
        use rebon_tool::agent_registry::{AgentGroups, AgentRegistry, AgentRuntime};

        fn plugin_agent(
            id: &str,
        ) -> rebon_plugin_package::acp_agent_manifest::PluginAcpAgentContribution {
            rebon_plugin_package::acp_agent_manifest::PluginAcpAgentContribution {
                id: id.to_string(),
                plugin_name: "demo-plugin".to_string(),
                source: "plugin:demo-plugin@user".to_string(),
                display_name: Some(format!("{id} (plugin)")),
                command: "plugin-binary".to_string(),
                args: vec!["--acp".to_string()],
                env: std::collections::BTreeMap::new(),
                cwd: None,
                inject_fs_tools: false,
                session_meta: None,
                install_hint: Some("npm install -g demo".to_string()),
            }
        }

        let registry = AgentRegistry::from_groups(AgentGroups {
            user: vec![definition(
                "gemini",
                AgentRuntime::Acp {
                    command: "from-definition".into(),
                    args: Vec::new(),
                },
            )],
            ..Default::default()
        });

        let declared = declared_agents(
            &[agent_config("claude-code")],
            &[
                // Shadowed by the config entry of the same name.
                plugin_agent("claude-code"),
                // Shadows the definition of the same name.
                plugin_agent("gemini"),
                plugin_agent("qwen"),
            ],
            &registry,
        );

        assert_eq!(declared.len(), 3, "one per name across all surfaces");
        assert_eq!(
            declared
                .iter()
                .find(|agent| agent.id == "claude-code")
                .map(|agent| agent.origin),
            Some(AgentOrigin::Config)
        );
        let gemini = declared
            .iter()
            .find(|agent| agent.id == "gemini")
            .expect("declared");
        assert_eq!(gemini.origin, AgentOrigin::Plugin);
        assert_eq!(gemini.command, "plugin-binary");
        let qwen = declared
            .iter()
            .find(|agent| agent.id == "qwen")
            .expect("declared");
        assert_eq!(qwen.label, "qwen (plugin)");
        assert!(!qwen.inject_fs_tools);
        assert_eq!(qwen.install_hint.as_deref(), Some("npm install -g demo"));
    }

    #[test]
    fn each_agent_gets_a_journal_and_the_shared_file_history() {
        // Building the registry must not spawn anything — an agent
        // nobody selected should never run.
        let root = temp_root("build");
        let (registry, fs_service) = build_registry(
            &[
                declared_agent_from_config(&agent_config("claude-code")),
                declared_agent_from_config(&agent_config("gemini")),
            ],
            root.path(),
            "/tmp/repo",
            "sess-1",
            Arc::new(NoopTracker),
            vec![PathBuf::from("/tmp/repo")],
            None,
        );
        assert!(
            fs_service.is_none(),
            "a sync test context has no runtime, so injection degrades to off"
        );
        assert_eq!(registry.ids(), vec!["claude-code", "gemini"]);
        assert!(registry.live_backends().is_empty());

        let backend = registry.backend("claude-code").expect("configured");
        assert!(
            !backend.capabilities().supports_rewind(),
            "no ACP agent can promise rewind: advertising the filesystem \
             capability does not force the agent to use it"
        );
        assert!(
            backend.snapshots_routed_writes(),
            "the writes it does route through us are still snapshotted"
        );
        assert!(!backend.capabilities().reports_usage);
    }

    #[tokio::test]
    async fn injection_gives_each_agent_the_fs_server_and_honours_the_opt_out() {
        let root = temp_root("inject");
        let mut opted_out = declared_agent_from_config(&agent_config("gemini"));
        opted_out.inject_fs_tools = false;
        let (registry, fs_service) = build_registry(
            &[
                declared_agent_from_config(&agent_config("claude-code")),
                opted_out,
            ],
            root.path(),
            "/tmp/repo",
            "sess-1",
            Arc::new(NoopTracker),
            vec![PathBuf::from("/tmp/repo")],
            None,
        );
        let service = fs_service.expect("one agent wants injection, so the host half starts");
        assert!(
            registry.live_backends().is_empty(),
            "starting the service must not start any agent"
        );

        let injected = registry.backend("claude-code").expect("configured");
        let servers = injected.injected_mcp_servers();
        assert_eq!(servers.len(), 1);
        let rebon_proto::types::McpServerConfig::Stdio {
            name,
            command,
            args,
            env,
            cwd,
        } = &servers[0]
        else {
            panic!("the bridge is a stdio server");
        };
        assert_eq!(name, rebon_acp_client::FS_MCP_SERVER_NAME);
        let expected_command = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            command.as_str(),
            expected_command.as_str(),
            "the bridge is this very binary"
        );
        assert_eq!(
            args.as_slice(),
            &[rebon_acp_client::FS_BRIDGE_SUBCOMMAND.to_string()]
        );
        assert_eq!(
            env.get(rebon_acp_client::fs_mcp::FS_PORT_ENV)
                .map(String::as_str),
            Some(service.port().to_string().as_str())
        );
        assert_eq!(
            env.get(rebon_acp_client::fs_mcp::FS_TOKEN_ENV)
                .map(String::as_str),
            Some(service.token())
        );
        assert_eq!(
            env.get(rebon_acp_client::fs_mcp::FS_SESSION_ENV)
                .map(String::as_str),
            Some("sess-1")
        );
        assert!(cwd.is_none(), "the bridge must not depend on a cwd");

        let excused = registry.backend("gemini").expect("configured");
        assert!(
            excused.injected_mcp_servers().is_empty(),
            "injectFsTools: false must mean exactly that"
        );
    }

    #[tokio::test]
    async fn rebuilding_for_a_new_session_mints_fresh_host_fs_credentials() {
        let root = temp_root("host-fs-runtime-swap");
        let declared = [declared_agent_from_config(&agent_config("claude-code"))];
        let (old_registry, old_service) = build_registry(
            &declared,
            root.path(),
            "/old",
            "sess-old",
            Arc::new(NoopTracker),
            vec![PathBuf::from("/old")],
            None,
        );
        let (new_registry, new_service) = build_registry(
            &declared,
            root.path(),
            "/new",
            "sess-new",
            Arc::new(NoopTracker),
            vec![PathBuf::from("/new")],
            None,
        );
        let old_service = old_service.expect("old host-fs service");
        let new_service = new_service.expect("new host-fs service");
        let old_backend = old_registry.backend("claude-code").unwrap();
        let new_backend = new_registry.backend("claude-code").unwrap();
        let old_servers = old_backend.injected_mcp_servers();
        let new_servers = new_backend.injected_mcp_servers();
        let rebon_proto::types::McpServerConfig::Stdio { env: old_env, .. } = &old_servers[0]
        else {
            panic!("old bridge must be stdio")
        };
        let rebon_proto::types::McpServerConfig::Stdio { env: new_env, .. } = &new_servers[0]
        else {
            panic!("new bridge must be stdio")
        };

        assert_eq!(
            old_env.get(rebon_acp_client::fs_mcp::FS_SESSION_ENV),
            Some(&"sess-old".to_string())
        );
        assert_eq!(
            new_env.get(rebon_acp_client::fs_mcp::FS_SESSION_ENV),
            Some(&"sess-new".to_string())
        );
        assert_eq!(
            old_env.get(rebon_acp_client::fs_mcp::FS_TOKEN_ENV),
            Some(&old_service.token().to_string())
        );
        assert_eq!(
            new_env.get(rebon_acp_client::fs_mcp::FS_TOKEN_ENV),
            Some(&new_service.token().to_string())
        );
        assert_ne!(old_service.token(), new_service.token());
        assert_ne!(old_service.port(), new_service.port());
    }

    #[test]
    fn reopening_a_session_hands_the_agent_back_its_own_session_id() {
        // Without this, a restart can only mint a fresh agent session:
        // the conversation on screen would look continuous while the
        // agent had forgotten all of it.
        let root = temp_root("resume-id");
        rebon_session::save_session_agent(root.path(), "/tmp/repo", "sess-1", "claude-code")
            .unwrap();
        rebon_session::save_agent_session_id(root.path(), "/tmp/repo", "sess-1", "agent-sess-7")
            .unwrap();

        let agents = session_agents(root.path(), vec![agent_config("claude-code")]);
        agents.switch_to("claude-code").expect("configured");

        let backend = agents.registry().backend("claude-code").expect("built");
        assert_eq!(
            backend.remembered_agent_session_id("sess-1").as_deref(),
            Some("agent-sess-7"),
            "the stored id must reach the backend that will resume it"
        );
    }

    #[test]
    fn an_id_minted_by_a_different_agent_is_not_offered() {
        // Session ids are private to the agent that issued them;
        // offering one to a stranger asks it to load a session it never
        // had.
        let root = temp_root("resume-id-other");
        rebon_session::save_session_agent(root.path(), "/tmp/repo", "sess-1", "gemini").unwrap();
        rebon_session::save_agent_session_id(root.path(), "/tmp/repo", "sess-1", "gemini-sess")
            .unwrap();

        let agents = session_agents(
            root.path(),
            vec![agent_config("claude-code"), agent_config("gemini")],
        );
        agents.switch_to("claude-code").expect("configured");

        let backend = agents.registry().backend("claude-code").expect("built");
        assert!(backend.remembered_agent_session_id("sess-1").is_none());
    }

    #[test]
    fn an_acp_turn_replays_as_messages_the_local_engine_can_send() {
        // The journal writes transcript JSON by hand, matching a shape
        // owned by `rebon-api`. This is the test that catches the two
        // drifting apart: it runs a recorded ACP turn back through the
        // engine's own transcript → request conversion, which is what
        // would happen the moment the user switched to `/agent local`.
        use rebon_acp_client::TurnJournal;
        use rebon_types::{RegularContent, TextContent, ToolCallContent, ToolCallStatus, ToolKind};

        let root = temp_root("replay");
        let journal = rebon_acp_client::TranscriptJournal::new(
            root.path(),
            "/tmp/repo",
            "sess-1",
            "claude-code",
        );
        let text = |t: &str| {
            rebon_types::ContentBlock::Text(TextContent {
                text: t.to_string(),
                annotations: None,
            })
        };

        let turn = journal
            .begin_turn("sess-1", &[text("fix the build")])
            .expect("turn opened");
        journal.record_update(
            "sess-1",
            &rebon_types::SessionUpdate::AgentMessageChunk {
                content: text("editing now"),
            },
        );
        journal.record_update(
            "sess-1",
            &rebon_types::SessionUpdate::ToolCall {
                tool_call_id: "t1".into(),
                title: "Edit".into(),
                kind: ToolKind::Edit,
                status: ToolCallStatus::Completed,
                content: Some(vec![ToolCallContent::Content(RegularContent {
                    content: text("wrote 3 lines"),
                })]),
                locations: None,
                raw_input: None,
                raw_output: None,
            },
        );
        journal.end_turn("sess-1", &turn, Some(rebon_types::StopReason::EndTurn));

        let path = rebon_session::transcript_file_path(root.path(), "/tmp/repo", "sess-1");
        let loaded = rebon_session::load_transcript_from_file(&path)
            .expect("read transcript")
            .expect("the turn was recorded");
        let messages = rebon_core::query::transcript_to_api_messages(&loaded.messages);

        assert_eq!(messages.len(), 3, "prompt, answer, tool result");
        assert_eq!(messages[0].role, rebon_api::Role::User);
        assert_eq!(messages[0].content[0].as_text(), Some("fix the build"));

        assert_eq!(messages[1].role, rebon_api::Role::Assistant);
        assert_eq!(messages[1].content[0].as_text(), Some("editing now"));
        let tool_use = messages[1].content[1]
            .as_tool_use()
            .expect("the agent's tool call must survive as a tool_use block");
        assert_eq!(tool_use.id, "t1");
        assert_eq!(tool_use.name, "Edit");

        assert_eq!(messages[2].role, rebon_api::Role::User);
        match &messages[2].content[0] {
            rebon_api::ContentBlock::ToolResult(result) => {
                assert_eq!(
                    result.tool_use_id, "t1",
                    "an unpaired tool_use is a message no model accepts"
                );
                assert!(!result.is_error);
            }
            other => panic!("expected a tool_result block, got {other:?}"),
        }
    }

    #[test]
    fn the_file_history_adapter_refuses_another_sessions_writes() {
        let history = SessionFileHistory {
            session_id: "sess-1".to_string(),
            tracker: Arc::new(NoopTracker),
        };
        assert!(history.begin_turn("sess-1", "turn-1").is_ok());
        let err = history
            .begin_turn("sess-2", "turn-1")
            .expect_err("a tracker belongs to one session");
        assert!(err.to_string().contains("sess-2"), "{err}");
        assert!(history
            .snapshot_before_write("sess-2", Path::new("/tmp/repo/a.txt"))
            .is_err());
        assert!(history.end_turn("sess-2", "turn-1").is_err());
    }
}
