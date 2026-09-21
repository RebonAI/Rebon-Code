//! Per-session agent choice for the surfaces that host many sessions in one
//! process — the ACP server and `rebon serve`.
//!
//! The TUI builds one [`SessionAgents`] for its one session, so `/agent` can
//! swap the backend behind that session's `AgentBackendSwitch`. An ACP server
//! opens a session per `session/new`, and a web page opens as many as it has
//! tabs, so the same thing has to happen per session id: a switch and an
//! agent set of its own, built the first time the session is seen and kept
//! until the process exits. The router *is* the handler's prompt executor —
//! every turn goes through it, which is what makes the choice stick without
//! the handler learning what an agent is.
//!
//! What a session can be switched to is the same set the TUI offers: Rebon's
//! own engine (`local`), every third-party agent CLI both declaration surfaces
//! name, and one kernel loop backend per vendor the user configured under
//! `kernelPlugins.loops` (`kernel:dsh` runs the dsh agent loop on the plugin
//! plane). A kernel backend journals its turns into the session's transcript
//! exactly as the TUI's does, so switching back to `local` replays the real
//! conversation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rebon_agent_core::routing::{DeclaredAgent, SessionAgents};
use rebon_agent_core::{
    AgentBackend, AgentBackendSwitch, PromptExecutor, PromptExecutorError, PromptOutcome,
    PromptRequest,
};

/// Everything a router needs to build a session's agent set on demand.
pub struct SessionAgentRouterConfig {
    /// Rebon's own engine, shared by every session (the executor keys on the
    /// request's session id).
    pub local: Arc<dyn AgentBackend>,
    /// Third-party agent CLIs from `acpAgents` and `runtime: acp` definitions.
    pub declared: Vec<DeclaredAgent>,
    /// Why the `acpAgents` list could not be read, if it could not.
    pub config_error: Option<String>,
    pub projects_root: PathBuf,
    /// Where `config.json` lives — the kernel loop vendors are read from it.
    pub config_dir: PathBuf,
    /// Mirrors journalled rows into the live transcript projection.
    pub transcript_sink: Option<rebon_acp_client::TranscriptSink>,
    /// Where a kernel loop runs.
    pub kernel_spawner: Arc<dyn rebon_plugin_host::loop_host::LoopHostSpawner>,
}

struct SessionSlot {
    switch: Arc<AgentBackendSwitch>,
    agents: Arc<SessionAgents<rebon_acp_client::AcpAgentBackend>>,
}

/// The reasoning effort levels a session can be set to, as
/// `(value, label)`. The values are the TUI's `/effort` levels; `auto`
/// leaves the provider's default in place. The ordinal a level becomes
/// on the request is the same one the TUI derives
/// (`crate::session::commands::effort::resolve_thinking_from_effort`).
pub const EFFORT_LEVELS: &[(&str, &str)] = &[
    ("auto", "Auto (provider default)"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Max"),
];

/// The request ordinal for an effort value; `None` for `auto`.
pub fn effort_ordinal(value: &str) -> Option<u8> {
    match value {
        "low" => Some(0),
        "medium" => Some(1),
        "high" => Some(2),
        "xhigh" => Some(3),
        "max" => Some(4),
        _ => None,
    }
}

/// The agent choice of every session this process hosts.
pub struct SessionAgentRouter {
    config: SessionAgentRouterConfig,
    /// The kernel loop vendors configured when the router was built, as
    /// `(backend id, label, config)`. Read once: the option list a client is
    /// shown has to match what a switch will accept.
    kernel_vendors: Vec<(
        String,
        &'static str,
        rebon_plugin_host::kernel_loop_backend::KernelLoopConfig,
    )>,
    slots: Mutex<HashMap<String, Arc<SessionSlot>>>,
    /// The effort a session's turns run with, by session id. A session not
    /// in here runs with [`Self::default_effort`].
    efforts: Mutex<HashMap<String, String>>,
    /// The persisted `/effort` choice at startup, or `auto`.
    default_effort: String,
}

/// A tracker for surfaces that keep no per-session file history. Routed
/// writes from an external agent go direct and are honestly not
/// rewind-covered, which is what the ACP server has always done.
struct NoHistoryTracker;

impl rebon_agent_core::file_history::FileHistoryTracker for NoHistoryTracker {
    fn track_before_write(&self, _file_path: &Path) -> anyhow::Result<()> {
        Ok(())
    }
}

impl SessionAgentRouter {
    pub fn new(config: SessionAgentRouterConfig) -> Self {
        let kernel_vendors = rebon_plugin_host::loop_host::KNOWN_LOOP_VENDORS
            .iter()
            .filter_map(|vendor| {
                let loop_config =
                    rebon_plugin_host::kernel_loop_backend::KernelLoopConfig::for_vendor(
                        &config.config_dir,
                        vendor.id,
                    )?;
                Some((
                    rebon_plugin_host::loop_host::loop_backend_id(vendor.id),
                    vendor.label,
                    loop_config,
                ))
            })
            .collect();
        let default_effort = rebon_config::saved_effort_level()
            .filter(|level| effort_ordinal(level).is_some())
            .unwrap_or_else(|| "auto".to_string());
        Self {
            config,
            kernel_vendors,
            slots: Mutex::new(HashMap::new()),
            efforts: Mutex::new(HashMap::new()),
            default_effort,
        }
    }

    /// The effort a session runs with until it is set: the persisted
    /// `/effort` choice, or `auto`.
    pub fn default_effort(&self) -> String {
        self.default_effort.clone()
    }

    /// The effort value of a session.
    pub fn effort(&self, session_id: &str) -> String {
        self.efforts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| self.default_effort.clone())
    }

    /// Set a session's effort to one of [`EFFORT_LEVELS`].
    pub fn set_effort(&self, session_id: &str, cwd: &str, value: &str) -> Result<(), String> {
        if !EFFORT_LEVELS
            .iter()
            .any(|(candidate, _)| *candidate == value)
        {
            return Err(format!(
                "unknown effort `{value}`; expected one of {}",
                EFFORT_LEVELS
                    .iter()
                    .map(|(candidate, _)| *candidate)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        rebon_session::model_selection::save_manual_effort(
            &self.config.projects_root,
            cwd,
            session_id,
            rebon_types::ReasoningEffort::from_wire_exact(value),
        )
        .map_err(|error| format!("cannot persist session effort: {error}"))?;
        self.efforts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string(), value.to_string());
        Ok(())
    }

    /// Every agent a session here can be switched to, as `(id, label)`.
    ///
    /// The same for every session, so a client can be shown the list before
    /// it has opened one.
    pub fn option_values(&self) -> Vec<(String, String)> {
        let mut values = vec![(
            rebon_config::LOCAL_AGENT_ID.to_string(),
            "Rebon (local engine)".to_string(),
        )];
        values.extend(
            self.config
                .declared
                .iter()
                .map(|agent| (agent.id.clone(), agent.label.clone())),
        );
        values.extend(
            self.kernel_vendors
                .iter()
                .map(|(id, label, _)| (id.clone(), (*label).to_string())),
        );
        values
    }

    /// Ids of the kernel loop backends this router can run.
    pub fn kernel_backend_ids(&self) -> Vec<String> {
        self.kernel_vendors
            .iter()
            .map(|(id, _, _)| id.clone())
            .collect()
    }

    /// The agent a session's next turn runs on. A session this router has
    /// not seen is on the local engine.
    pub fn current(&self, session_id: &str) -> String {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map(|slot| slot.agents.current_id())
            .unwrap_or_else(|| rebon_config::LOCAL_AGENT_ID.to_string())
    }

    /// The choices a session sees, with the current one marked.
    /// Only the tests reach the router this way; production asks the slot.
    #[cfg(test)]
    pub fn choices(
        &self,
        session_id: &str,
        cwd: &str,
    ) -> Vec<rebon_agent_core::routing::AgentChoice> {
        self.slot(session_id, cwd).agents.choices()
    }

    /// Point a session's next turn at `agent_id`. The receipt is the same
    /// text `/agent` prints; the error names what could not be done.
    pub fn switch(&self, session_id: &str, cwd: &str, agent_id: &str) -> Result<String, String> {
        self.slot(session_id, cwd).agents.switch_to(agent_id)
    }

    fn slot(&self, session_id: &str, cwd: &str) -> Arc<SessionSlot> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = slots.get(session_id) {
            return slot.clone();
        }
        let slot = Arc::new(self.build_slot(session_id, cwd));
        slots.insert(session_id.to_string(), slot.clone());
        slot
    }

    fn build_slot(&self, session_id: &str, cwd: &str) -> SessionSlot {
        let switch = Arc::new(AgentBackendSwitch::new(self.config.local.clone()));
        let mut agents = rebon_harness::agent_assembly::build_session_agents(
            self.config.declared.clone(),
            switch.clone(),
            self.config.local.clone(),
            self.config.projects_root.clone(),
            cwd.to_string(),
            session_id.to_string(),
            Arc::new(NoHistoryTracker)
                as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
            vec![PathBuf::from(cwd)],
            self.config.transcript_sink.clone(),
        )
        .with_config_error(self.config.config_error.clone());
        for (backend_id, label, loop_config) in &self.kernel_vendors {
            let mut journal = rebon_acp_client::TranscriptJournal::new(
                self.config.projects_root.clone(),
                cwd.to_string(),
                session_id.to_string(),
                backend_id.clone(),
            );
            if let Some(sink) = &self.config.transcript_sink {
                journal = journal.with_transcript_sink(sink.clone());
            }
            let backend = rebon_plugin_host::kernel_loop_backend::KernelLoopBackend::new(
                backend_id.clone(),
                self.config.kernel_spawner.clone(),
                loop_config.clone(),
                Arc::new(journal),
            );
            agents = agents.with_kernel_backend(backend_id.clone(), *label, Arc::new(backend));
        }
        SessionSlot {
            switch,
            agents: Arc::new(agents),
        }
    }
}

#[async_trait]
impl PromptExecutor for SessionAgentRouter {
    async fn execute(
        &self,
        mut request: PromptRequest,
    ) -> Result<PromptOutcome, PromptExecutorError> {
        // Resolved before the await so the slot map is never locked across
        // a turn; the turn stays on the backend the session was on when it
        // started.
        let slot = self.slot(&request.session_id, &request.cwd);
        // The handler leaves effort to the executor; the session's choice
        // is applied here, where the session is known.
        if request.reasoning_effort_ordinal.is_none()
            && request.thinking_budget.is_none()
            && request.max_tokens.is_none()
        {
            let explicit = self
                .efforts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&request.session_id)
                .cloned();
            request.effort_is_session_default = explicit.is_none();
            request.reasoning_effort_ordinal =
                effort_ordinal(&explicit.unwrap_or_else(|| self.default_effort.clone()));
        }
        slot.switch.execute(request).await
    }
}

impl std::fmt::Debug for SessionAgentRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAgentRouter")
            .field(
                "declared",
                &self
                    .config
                    .declared
                    .iter()
                    .map(|agent| agent.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("kernel", &self.kernel_backend_ids())
            .field(
                "sessions",
                &self
                    .slots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_agent_core::{LocalAgentBackend, StubPromptExecutor};

    struct NoLoops;

    #[async_trait]
    impl rebon_plugin_host::loop_host::LoopHostSpawner for NoLoops {
        async fn spawn(
            &self,
            _spec: rebon_plugin_host::loop_host::LoopAgentSpec,
        ) -> Result<Arc<dyn rebon_plugin_host::loop_host::LoopHost>, String> {
            Err("no loops in this test".to_string())
        }
    }

    fn router(config_dir: &Path) -> SessionAgentRouter {
        SessionAgentRouter::new(SessionAgentRouterConfig {
            local: Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)))
                as Arc<dyn AgentBackend>,
            declared: Vec::new(),
            config_error: None,
            projects_root: config_dir.join("projects"),
            config_dir: config_dir.to_path_buf(),
            transcript_sink: None,
            kernel_spawner: Arc::new(NoLoops),
        })
    }

    #[test]
    fn a_session_starts_on_the_local_engine_and_remembers_a_switch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"kernelPlugins":{"loops":{"dsh":{"provider":"deepseek","model":"deepseek-chat"}}}}"#,
        )
        .unwrap();
        let router = router(dir.path());

        assert_eq!(router.current("s1"), "local");
        assert_eq!(
            router.option_values(),
            vec![
                ("local".to_string(), "Rebon (local engine)".to_string()),
                (
                    "kernel:dsh".to_string(),
                    "dsh loop (deepseek-harness)".to_string()
                ),
            ]
        );

        let receipt = router.switch("s1", "/work", "kernel:dsh").unwrap();
        assert!(receipt.contains("dsh"), "receipt names the loop: {receipt}");
        assert_eq!(router.current("s1"), "kernel:dsh");
        // Another session is untouched.
        assert_eq!(router.current("s2"), "local");
        let choices = router.choices("s1", "/work");
        assert!(choices.iter().any(|c| c.id == "kernel:dsh" && c.current));
        assert!(choices.iter().any(|c| c.id == "local" && !c.current));
    }

    #[test]
    fn an_unconfigured_loop_is_not_offered_and_not_switchable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        let router = router(dir.path());
        assert_eq!(router.kernel_backend_ids(), Vec::<String>::new());
        assert_eq!(router.option_values().len(), 1);
        let err = router.switch("s1", "/work", "kernel:dsh").unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(router.current("s1"), "local");
    }

    #[tokio::test]
    async fn a_turn_runs_on_whatever_the_session_is_switched_to() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        let router = router(dir.path());
        let request = PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: "s1".to_string(),
            cwd: "/work".to_string(),
            prompt: Vec::new(),
            mcp_servers: Vec::new(),
            update_publisher: None,
            permission_publisher: None,
            cancel: rebon_agent_core::PromptCancel::new(),
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
        };
        let outcome = router.execute(request).await.unwrap();
        assert_eq!(outcome.stop_reason, rebon_types::StopReason::EndTurn);
        assert_eq!(router.current("s1"), "local");
    }
}
