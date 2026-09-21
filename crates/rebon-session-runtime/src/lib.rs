//! The session as every surface drives it.
//!
//! `rebon-cli` used to hold this as `crate` plus a dozen modules
//! beside it. It came out because three surfaces need it and only one of them
//! has a screen: the terminal draws it, the background worker drives it with
//! no screen, and `serve` reports on it. What is here is the half every
//! surface needs.
//!
//! The layering, settled after measuring it:
//!
//! ```text
//! rebon-cli               the terminal, and the session shell it wraps this in
//!   -> rebon-session-runtime   this crate: the session, its runtime, its commands
//!   -> rebon-session-host      job state, store, protocol, owner/lease, client
//!   -> rebon-harness -> rebon-core -> seats -> kernel
//! ```
//!
//! It is two crates rather than one because the host half must not depend on
//! the runtime half: `session-runtime` reads `BackgroundStore` and the job
//! state, so folding the worker into `session-host` would close a cycle.
//!
//! Nothing here draws. The terminal-only half — the session shell, the
//! handover, the startup dialogs — stayed behind in `rebon_cli::session_shell`.

/// The sibling executable that serves MCP over a language server.
///
/// Lives with its writer: `plugin::builtin` materializes the config that names
/// it, and that materializer is in this crate. `rebon-cli` re-exports it so
/// its own `crate::LSP_MCP_SIBLING` keeps resolving.
pub const LSP_MCP_SIBLING: &str = "rebon-lsp-mcp";

/// The MCP server id `--lsp rust` and the `rust-lsp` builtin alias write.
///
/// `rebon-lsp-mcp` naming itself the same thing on the wire is a separate fact
/// kept in that crate — sharing one constant would mean keeping a dependency
/// edge for a string.
pub const RUST_LSP_SERVER_NAME: &str = "rust_lsp";

pub mod acp_subagent_pool;
pub mod host;
pub mod mcp_config;
pub mod mcp_status;
pub mod plugin;
pub mod project_settings;
pub mod queue_controller;
pub mod rebon_config;
pub mod ripgrep;
pub mod session_handoff;
pub mod task_notification_poller;
pub mod test_env;
pub mod ui_config;

pub mod build;
pub mod commands;
pub mod compact;
pub mod input_history;
pub mod mcp;
pub mod mid_turn_queue;
pub mod permission_answer;
pub mod permission_policy;
pub mod profile_proposal;
pub mod resume_listing;
pub mod resume_resolution;
pub mod runtime;
pub mod runtime_refresh;
pub mod settings_rows;
pub mod startup;
pub mod submit_payload;
pub mod title;
pub mod transcript_replay;
pub mod ultraplan_gate;
pub mod ultraplan_preflight;
pub mod ultraplan_review;
pub mod ultraplan_run;
pub mod usage;
pub mod workflow_review;

use std::sync::Arc;

#[cfg(any(test, feature = "test-support"))]
use tokio::sync::mpsc::UnboundedReceiver;

use rebon_acp::ServerState;
pub use rebon_agent_core::file_history::{DeferredFileHistoryTracker, SharedFileHistoryTracker};
#[cfg(any(test, feature = "test-support"))]
use std::path::PathBuf;

#[cfg(any(test, feature = "test-support"))]
use rebon_agent_core::ChannelSessionUpdatePublisher;
#[cfg(any(test, feature = "test-support"))]
use rebon_agent_core::PromptExecutor;
#[cfg(any(test, feature = "test-support"))]
use rebon_core::permission::ChannelPermissionBroker;
#[cfg(any(test, feature = "test-support"))]
use rebon_core::permission::OutboundPermissionQuery;
#[cfg(any(test, feature = "test-support"))]
use rebon_types::SessionUpdateParams;

use crate::build::SessionStartHookRx;
#[cfg(any(test, feature = "test-support"))]
use crate::runtime::SessionRuntime;
use crate::runtime::{SessionEngineHalf, SessionRuntimeInstall};
use crate::startup::SessionStartupParams;

/// Everything a session is once the terminal is taken away.
pub struct EngineSession {
    /// Everything only the host of this session has. See [`SessionEngineHalf`].
    pub engine_half: SessionEngineHalf,
    // permission_publisher removed — the TUI now uses a typed
    // ChannelPermissionBroker wired into the executor, bypassing
    // the ACP JSON-RPC round-trip. The PromptRequest no longer
    // carries a permission_publisher for the TUI path.
    /// Pre-minted session id used for every submit in this TUI
    /// invocation. `ServerState::create_session` hands us a
    /// `SessionRecord`; we store only the id because the rest of
    /// the record is looked up by the executor on every call.
    pub session_id: String,
    /// Projects root used for transcript and sidecar metadata persistence.
    pub projects_root: std::path::PathBuf,
    /// The in-process session registry: records, transcripts as loaded,
    /// titles, modes. The client half keeps it because a mirror reads it too
    /// — the transcript it shows is loaded through it — and every process
    /// has its own; nothing in it is the owner's alone. The executor and
    /// the handler hold clones of the same `Arc`.
    pub server_state: Arc<ServerState>,
    /// Current working directory at startup, used as the cwd for
    /// every `PromptRequest` this session sends. Captured via
    /// `std::env::current_dir()` — no `--cwd` flag.
    pub cwd: String,
    /// The resolved model and provider this session runs on: names the
    /// status bar shows, and the prune / service-tier knobs `/settings`
    /// writes. One shape with the headless handle's — see
    /// [`rebon_harness::SessionModel`].
    pub model: rebon_harness::SessionModel,
    /// Transcript entries loaded from disk when `--resume` was used.
    /// Empty for fresh sessions. The runner replays these into the
    /// TUI transcript on startup.
    pub loaded_transcript: Vec<rebon_session::TranscriptEntry>,
    /// Session-scoped active marker. Holding this lock means this
    /// session is currently open and must not be offered by /resume.
    pub session_active_lock: Option<rebon_session::SessionActiveLock>,
    /// Background Agent View job currently represented by this foreground session.
    /// Set when attaching a background job; reused by `/background` so
    /// attach/detach cycles keep one stable job row instead of creating
    /// duplicate background-job metadata for the same session.
    pub attached_background_job_id: Option<String>,
    /// The SessionStart hook, running on the runtime since the session was
    /// installed. Its messages go into the transcript when they land
    /// (`drain_session_start_hook`); nothing waits for it — it fed only
    /// what is shown, never the first turn. `None` once drained.
    pub session_start_hook: Option<SessionStartHookRx>,
    /// What this session was launched with. Written once while the session is
    /// built and never by a turn; see [`SessionStartupParams`].
    pub startup: SessionStartupParams,
}

impl EngineSession {
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn restore_runtime(
        &mut self,
        runtime: Arc<SessionRuntime>,
        update_rx: UnboundedReceiver<SessionUpdateParams>,
        permission_rx: UnboundedReceiver<OutboundPermissionQuery>,
        active_prompt_running: bool,
    ) -> anyhow::Result<bool> {
        if active_prompt_running {
            anyhow::bail!("cannot adopt a session runtime while a prompt is running");
        }
        Ok(self.install_runtime(SessionRuntimeInstall {
            runtime,
            update_rx,
            permission_rx,
        }))
    }

    /// Build and atomically install a fresh immutable runtime for future turns.
    ///
    /// The old Arc is not explicitly torn down: active/detached prompts keep it
    /// alive, and `SessionRuntime::drop` retires its backends only after the last
    /// such turn releases it.
    pub fn swap_runtime(
        &mut self,
        session_id: &str,
        cwd: &str,
        active_prompt_running: bool,
    ) -> anyhow::Result<bool> {
        if active_prompt_running {
            anyhow::bail!("cannot replace the session runtime while a prompt is running");
        }
        let preferred_agent = self.engine_half.runtime.session_agents.current_id();
        let Some(factory) = self.engine_half.runtime_factory.as_ref().cloned() else {
            // The binary's own tests drive this path, and a dependency is not
            // compiled with `cfg(test)` when its dependent is under test —
            // so the gate has to be the feature, not `test`, or every
            // `/new` test in `rebon-cli` silently takes the error branch.
            #[cfg(any(test, feature = "test-support"))]
            {
                self.swap_test_runtime(session_id, cwd);
                return Ok(true);
            }
            #[cfg(not(any(test, feature = "test-support")))]
            return Err(anyhow::anyhow!("session runtime factory is unavailable"));
        };
        let install =
            factory
                .runtime_handle
                .block_on(factory.build(session_id, cwd, &preferred_agent))?;
        Ok(self.install_runtime(install))
    }

    /// Drops every cached provider runtime, so the next turn dials its provider
    /// again.
    ///
    /// For a provider that went bad without going away — most often a plugin
    /// provider, which is a child process and can wedge. The cache is keyed by
    /// provider, but this drops all of them: rebuilding is lazy, so an untouched
    /// provider costs nothing to have dropped, and "reconnect the one I mean"
    /// would need the user to know which key the cache used.
    ///
    /// A runtime a turn is still holding survives — reference counting decides
    /// when the old one actually goes, exactly as it does for an idle eviction.
    pub(crate) fn reconnect_provider_runtimes(&self) -> Result<usize, String> {
        let factory = self
            .engine_half
            .runtime_factory
            .as_ref()
            .ok_or("this session has no runtime factory to reconnect through")?;
        Ok(factory
            .runtime_handle
            .block_on(factory.provider_runtimes.drop_all()))
    }

    /// The runtime this session's blocking UI thread hands async work to.
    ///
    /// The event loop runs on a `spawn_blocking` worker, so a command that has
    /// to await something — reconnecting an MCP server, say — needs the handle
    /// rather than a nested runtime. `None` only in tests, which build a
    /// session without a factory.
    pub fn runtime_handle(&self) -> Option<tokio::runtime::Handle> {
        self.engine_half
            .runtime_factory
            .as_ref()
            .map(|factory| factory.runtime_handle.clone())
    }

    /// Let the MCP servers go: this terminal no longer hosts the session,
    /// and a second set of servers beside the owner's has nothing to do.
    ///
    /// The teardown runs on the runtime rather than here. The caller is the
    /// UI thread, and a server slow to die must not hold the screen.
    pub fn release_mcp(&mut self) {
        let Some(mcp) = self.engine_half.mcp.take() else {
            return;
        };
        match self.runtime_handle() {
            Some(handle) => {
                handle.spawn(mcp.shutdown());
            }
            // Nothing to run the teardown on: dropping the stack drops its
            // transports with it.
            None => drop(mcp),
        }
    }

    /// Run the cron scheduler for this session's cwd, if none is running.
    ///
    /// Only the process that runs the session's turns may hold the cwd's
    /// scheduler lock: a fired task is a prompt for the local query loop, and
    /// a mirror has no loop to feed — it would take the lock the worker needs
    /// and swallow whatever fires. A session built as a mirror starts none;
    /// this is for the one place it becomes this process's after all.
    pub fn start_cron_scheduler(&mut self) {
        if self.engine_half.cron_scheduler.is_some() {
            return;
        }
        // The scheduler is a tokio task: it needs a runtime to be spawned
        // on, and the UI thread is not one. A test session may have none.
        let handle = self
            .runtime_handle()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        let Some(handle) = handle else {
            tracing::warn!(
                session_id = %self.session_id,
                "rebon: no runtime to start the cron scheduler on; scheduled tasks stay with whoever holds the lock"
            );
            return;
        };
        let _entered = handle.enter();
        let config = rebon_core::cron::SchedulerConfig::new(std::path::PathBuf::from(&self.cwd));
        self.engine_half.cron_scheduler =
            Some(rebon_core::cron::start_scheduler_with_session_store(
                config,
                self.engine_half.cron_poller.clone(),
                Some(self.engine_half.session_cron_store.clone()),
            ));
    }

    /// Stop the scheduler, handing the cwd's lock to whichever process runs
    /// the session now. Dropping the handle aborts the task and releases
    /// the lock with it; there is nothing to wait for.
    pub fn stop_cron_scheduler(&mut self) {
        drop(self.engine_half.cron_scheduler.take());
    }

    /// Make sure this session exists on disk before another process is asked
    /// to resume it.
    ///
    /// A session's transcript file is written on the first append, so a
    /// conversation nobody has spoken in yet exists only in memory — and a
    /// worker resumes *from disk*. Handing over before the first turn
    /// (`rebon --hosted`, or `/hosted` as the first thing typed) asked the
    /// worker to resume a session it could not find, and the job failed with
    /// "Session not found" seconds after starting; a worker whose first
    /// prompt a hook refused left the same nothing behind for the next one.
    /// An empty transcript file is what makes "this session, from the
    /// beginning" a thing a process can open.
    pub fn ensure_transcript_on_disk(&self) -> std::io::Result<()> {
        rebon_session::create_session_on_disk(&self.projects_root, &self.cwd, &self.session_id)
            .map(|_| ())
    }

    #[cfg(any(test, feature = "test-support"))]
    fn swap_test_runtime(&mut self, session_id: &str, cwd: &str) {
        let backend = self.engine_half.runtime.backend.clone();
        let switch = Arc::new(rebon_agent_core::AgentBackendSwitch::new(backend.clone()));
        let session_agents = Arc::new(rebon_agent_core::routing::SessionAgents::local_only(
            switch,
            backend.clone(),
            self.projects_root.clone(),
            cwd,
            session_id,
        ));
        let (update_publisher, update_rx) = ChannelSessionUpdatePublisher::new();
        let (permission_broker, permission_rx) =
            ChannelPermissionBroker::new(session_id.to_string());
        let permission_broker = Arc::new(permission_broker);
        let runtime = Arc::new(SessionRuntime {
            session_id: session_id.to_string(),
            projects_root: self.projects_root.clone(),
            cwd: cwd.to_string(),
            executor: self.engine_half.executor.clone(),
            backend,
            session_agents,
            acp_subagent_pool: None,
            sub_agent_spawner: self.engine_half.sub_agent_spawner.clone(),
            file_history_tracker: SharedFileHistoryTracker::new(
                rebon_session::FileHistoryStore::new(
                    self.projects_root.clone(),
                    PathBuf::from(cwd),
                    session_id,
                ),
            ),
            policy: rebon_core::policy_seat::PolicySources::default().with_context(
                rebon_core::policy_seat::PolicyContext {
                    cwd: cwd.to_string(),
                    transcript_path: rebon_session::transcript_file_path(
                        &self.projects_root,
                        cwd,
                        session_id,
                    )
                    .to_string_lossy()
                    .to_string(),
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
            ),
            skill_registry: self.engine_half.skill_registry.clone(),
            skill_state: Arc::new(std::sync::Mutex::new(
                rebon_plugin_skill::SkillState::empty(session_id, cwd),
            )),
            update_publisher: Arc::new(update_publisher),
            permission_broker,
            mid_turn_queue: crate::mid_turn_queue::MidTurnQueuedSubmitPoller::new(session_id),
            tasks: self.engine_half.tasks.clone(),
            task_notification_poller: self.engine_half.task_notification_poller.clone(),
            runtime_handle: None,
        });
        self.install_runtime(SessionRuntimeInstall {
            runtime,
            update_rx,
            permission_rx,
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn set_test_executor(&mut self, executor: Arc<dyn PromptExecutor>) {
        self.engine_half.executor = executor.clone();
        Arc::get_mut(&mut self.engine_half.runtime)
            .expect("test runtime must not be captured while replacing its executor")
            .executor = executor;
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn set_test_cwd(&mut self, cwd: impl Into<String>) {
        let cwd = cwd.into();
        self.cwd = cwd.clone();
        Arc::get_mut(&mut self.engine_half.runtime)
            .expect("test runtime must not be captured while replacing its cwd")
            .cwd = cwd;
    }

    /// Install a freshly minted runtime and re-point what reads it.
    ///
    /// The seven fields the engine half used to mirror from the runtime
    /// (backend, session agents, ACP sub-agent pool, file-history tracker,
    /// policy sources, permission broker, mid-turn queued-submit poller) are
    /// gone: readers reach them through `engine_half.runtime` instead, so
    /// there is nothing to re-point. What is copied here is the six handles a
    /// surface *re-points on its own* between installs — `/skill` rebuilds
    /// the registry, a foregrounded agent swaps the task registry — plus the
    /// two channel receivers, which are new objects and not the runtime's.
    fn install_runtime(&mut self, install: SessionRuntimeInstall) -> bool {
        let runtime = install.runtime;
        let replaced = !Arc::ptr_eq(&self.engine_half.runtime, &runtime);
        self.session_id = runtime.session_id.clone();
        self.projects_root = runtime.projects_root.clone();
        self.cwd = runtime.cwd.clone();
        self.engine_half.executor = runtime.executor.clone();
        self.engine_half.sub_agent_spawner = runtime.sub_agent_spawner.clone();
        self.engine_half.skill_registry = runtime.skill_registry.clone();
        self.engine_half.update_publisher = runtime.update_publisher.clone();
        self.engine_half.update_rx = install.update_rx;
        self.engine_half.permission_rx = install.permission_rx;
        self.engine_half.tasks = runtime.tasks.clone();
        self.engine_half.task_notification_poller = runtime.task_notification_poller.clone();
        self.engine_half.runtime = runtime;
        if replaced {
            self.attached_background_job_id = None;
        }
        replaced
    }

    /// Apply coordinator (`/ceo`) mode on/off to the session's shared handles and
    /// persist the session mode, without touching any TUI `AppState`.
    ///
    /// Flips the coordinator-mode flag + swaps the session and sub-agent tool
    /// filters the executor and spawner read each turn, then records the mode both
    /// in the in-memory server state and on disk (`save_session_mode`) so the next
    /// fresh session build — including the app's per-turn background worker, which
    /// resumes via `load_session_mode` — inherits it. Shared by the interactive
    /// `enter/exit_coordinator_mode` and the headless background worker so both
    /// surfaces flip identical state.
    pub fn apply_coordinator_mode(&self, on: bool) {
        self.engine_half.coordinator_mode_handle.set(on);
        let mode = if on { "coordinator" } else { "normal" };
        let _ = self.server_state.set_session_mode(&self.session_id, mode);
        if let Err(err) =
            rebon_session::save_session_mode(&self.projects_root, &self.cwd, &self.session_id, mode)
        {
            tracing::warn!(
                error = %err,
                session_id = %self.session_id,
                mode,
                "failed to persist session mode"
            );
        }
        if on {
            self.engine_half.session_filter_handle.set(
                rebon_core::coordinator_mode::coordinator_session_filter_for_queue(
                    self.startup.queue_session,
                ),
            );
            self.engine_half
                .subagent_filter_handle
                .set(rebon_core::coordinator_mode::async_agent_filter());
        } else {
            self.engine_half.session_filter_handle.set(
                rebon_harness::build_default_tool_filter_for_context(
                    false,
                    self.startup.queue_session,
                ),
            );
            self.engine_half
                .subagent_filter_handle
                .set(rebon_core::coordinator_mode::default_subagent_filter(false));
        }
    }
}

#[cfg(test)]
mod layering {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The repo root, from this crate's manifest directory.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/rebon-session-runtime sits two levels below the repo root")
            .to_path_buf()
    }

    /// Every `Cargo.toml` under `crates/` and `crates/plugins/`. `cargo tree`
    /// cannot answer this question about the whole tree, so a scan is the only
    /// way.
    fn manifests(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for group in [root.join("crates"), root.join("crates").join("plugins")] {
            let Ok(entries) = fs::read_dir(&group) else {
                continue;
            };
            for entry in entries.flatten() {
                let manifest = entry.path().join("Cargo.toml");
                if manifest.is_file() {
                    found.push(manifest);
                }
            }
        }
        found
    }

    /// Whether a manifest declares a dependency on `name`.
    ///
    /// Matches the dependency key at the start of a line, so
    /// `rebon-session-host` does not answer for `rebon-session-runtime`, and
    /// a name inside a comment or a path string does not count.
    fn declares(text: &str, name: &str) -> bool {
        text.lines().any(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                return false;
            }
            trimmed
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(['.', ' ', '=']))
        })
    }

    /// This crate carries a session's runtime, and a session runs with no
    /// screen attached: `rebon exec`, a background worker, `serve`. It must
    /// not be able to reach the terminal.
    ///
    /// The edge this pins is not hypothetical. A past change declared `rebon-tui`
    /// here and used it nowhere; nothing failed, and it sat in `main` for two
    /// days until `rebon-tui`'s own canary happened to be run. That canary
    /// can only see its own dependants — this one sees what this crate
    /// reaches for, which is the half that was missing.
    ///
    /// `rebon-dialog` and `rebon-picker` are deliberately **not** listed. The
    /// criterion is "does it pull in ratatui", and those two do not: they are
    /// dependency-free projections that plugins, the kernel seats and the
    /// GPUI app also depend on. Banning them here would forbid an edge that
    /// breaks nothing.
    #[test]
    fn the_session_runtime_cannot_reach_the_terminal() {
        let manifest = repo_root()
            .join("crates")
            .join("rebon-session-runtime")
            .join("Cargo.toml");
        let text = fs::read_to_string(&manifest).expect("read this crate's manifest");
        let forbidden: Vec<&str> = ["rebon-tui", "ratatui", "crossterm"]
            .into_iter()
            .filter(|name| declares(&text, name))
            .collect();
        assert!(
            forbidden.is_empty(),
            "rebon-session-runtime must run without a screen; it declares {forbidden:?}. \
             `rebon-tui` is the only crate that pulls in ratatui, and ratatui and crossterm \
             are the terminal itself. A session runs headless under `rebon exec`, a \
             background worker and `serve`."
        );
    }

    /// `cli -> rebon-session-runtime -> rebon-session-host`, upper half: only
    /// the terminal binary assembles a session runtime.
    ///
    /// Everything else is a session *client* and depends on
    /// `rebon-session-host`, the layer below this one. A second edge into this
    /// crate would mean some other surface had started assembling its own
    /// runtime instead of talking to an owner, which is the arrangement
    /// RFC-0004 exists to prevent.
    #[test]
    fn only_rebon_cli_may_depend_on_the_session_runtime() {
        let root = repo_root();
        let mut dependants = Vec::new();
        for manifest in manifests(&root) {
            if manifest
                .parent()
                .map(|dir| dir.ends_with("rebon-session-runtime"))
                == Some(true)
            {
                continue;
            }
            let text = fs::read_to_string(&manifest).expect("read a workspace manifest");
            if declares(&text, "rebon-session-runtime") {
                dependants.push(manifest);
            }
        }
        let expected = root.join("crates").join("rebon-cli").join("Cargo.toml");
        assert_eq!(
            dependants,
            vec![expected],
            "only rebon-cli may depend on rebon-session-runtime: every other \
             surface is a session client and depends on rebon-session-host instead"
        );
    }
}
