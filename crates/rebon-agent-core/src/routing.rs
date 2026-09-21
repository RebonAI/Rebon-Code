//! Which agent runs a session's turns.
//!
//! Rebon's own engine, one of the ACP agent CLIs configured under `acpAgents`,
//! or an in-process kernel loop. This crate owns the set of backends a session
//! can reach, the switch between them, the file-history tracker they write
//! through, and the handoff document carried across a switch.
//!
//! Nothing here draws anything. [`handle_agent_command`] returns the text to
//! show and whether it is an error; how that reaches a person is the front
//! end's business.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::registry::BackendRegistry;
use crate::{AgentBackend, AgentBackendSwitch, AgentCapabilities};
use rebon_session::SESSION_AGENT_LOCAL;

/// Where an agent CLI was declared.
///
/// Both surfaces are legitimate and they answer different questions —
/// "this machine has these agent CLIs" versus "this agent definition is
/// an external one" — so neither replaces the other. What they must not
/// do is describe *how to run* an agent twice, which is why both
/// collapse into [`DeclaredAgent`] before anything spawns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOrigin {
    /// `acpAgents[]` in `config.json`.
    Config,
    /// `capabilities.acpAgents` in an installed plugin's manifest.
    Plugin,
    /// `runtime: acp` in an agent definition's frontmatter.
    Definition,
    /// A host in `remotes.json`, reached over ssh.
    ///
    /// The one origin whose agent does not share this machine's
    /// filesystem, which is why [`DeclaredAgent::workspace_cwd`] is
    /// set for it and the host-side write plumbing is not.
    Remote,
    /// An agent loop running inside Rebon's embedded kernel runtime —
    /// in-process, registered by the host itself rather than declared
    /// in any config surface (the `kernel:` id prefix mirrors
    /// `remote:`, keeping it out of the `acpAgents` namespace).
    Kernel,
}

impl AgentOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config.json",
            Self::Plugin => "plugin",
            Self::Definition => "agent definition",
            Self::Remote => "remote",
            Self::Kernel => "embedded kernel",
        }
    }
}

/// One agent CLI, normalised out of whichever surface declared it.
///
/// The two surfaces do not carry the same detail: `config.json` can set
/// `env` and `cwd`, an agent definition's frontmatter deliberately
/// cannot. Normalising here rather than teaching the spawn path about
/// both is what keeps "how to start an agent CLI" a single description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredAgent {
    pub id: String,
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub origin: AgentOrigin,
    /// Whether Rebon's `write_file`/`edit_file` MCP tools ride into
    /// this agent's sessions. On by default — the injection is what
    /// pulls an agent's own writes back through the snapshot pipeline.
    pub inject_fs_tools: bool,
    /// `_meta` for `session/new`/`session/load` — adapter options the
    /// spec has no field for (e.g. `claude-agent-acp`'s
    /// `claudeCode.options.disallowedTools`).
    pub session_meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Appended to a spawn failure — how to install the CLI this
    /// declaration points at. Only plugins carry one today.
    pub install_hint: Option<String>,
    /// The directory this agent's sessions open in, when it is not the
    /// host session's.
    ///
    /// `None` for every agent running on this machine — they share the
    /// session's cwd, and overriding it would point them at a
    /// directory the user did not open. `Some` only for a remote,
    /// where the session cwd names a path on the wrong machine.
    /// Doubles as the "this agent is not local" flag where the host builds
    /// its agent registry.
    pub workspace_cwd: Option<String>,
}

/// One row of `/agent list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentChoice {
    pub id: String,
    pub label: String,
    pub current: bool,
    /// Where it was declared. `None` for Rebon's own engine.
    pub origin: Option<AgentOrigin>,
    /// What this agent cannot promise, when there is something to say.
    pub caveat: Option<String>,
}

/// The agents this session can run on, and the one it is running on.
pub struct SessionAgents<B: AgentBackend + 'static> {
    registry: BackendRegistry<B>,
    origins: std::collections::BTreeMap<String, AgentOrigin>,
    switch: Arc<AgentBackendSwitch>,
    local: Arc<dyn AgentBackend>,
    projects_root: PathBuf,
    cwd: String,
    /// Immutable identity of the runtime these adapters belong to.
    session_id: String,
    /// [`SESSION_AGENT_LOCAL`] or a configured agent id. Shared so the deferred
    /// close of a replaced kernel backend can re-read it before acting.
    current: Arc<Mutex<String>>,
    /// Why the `acpAgents` config contributed nothing, when it failed
    /// to read. Kept so `/agent` can say what actually happened —
    /// `"your config is broken: <why>"` — instead of showing the
    /// "nothing is configured" hint to a user who configured something.
    config_error: Option<String>,
    /// The host half of the injected fs tools, held so it lives — and
    /// its port stays bound — exactly as long as this session.
    session_resource: Option<Box<dyn Send + Sync>>,
    /// In-process kernel backends (`kernel:` ids), registered by the
    /// host rather than declared in config. A parallel table because
    /// the declared registry's `backend()` returns its concrete type;
    /// kernel hosts keep their independent lifecycle.
    kernel_backends: std::collections::BTreeMap<String, (String, Arc<dyn AgentBackend>)>,
}

impl<B: AgentBackend + 'static> SessionAgents<B> {
    /// Whether this token names an agent the session can switch to.
    ///
    /// What tells `/agent codex` (switch) apart from `/agent write the tests`
    /// (spawn a sub-agent). The two meanings shared one command and the spawn
    /// branch took every input, so the switch was unreachable — `/agent list`
    /// spawned a sub-agent whose prompt was the word "list".
    ///
    /// Case-insensitive because [`Self::switch_to`] is: `local`, the kernel
    /// table and the ACP registry all fold before comparing. Answering `false`
    /// for a spelling the switch would have accepted does not make it a prompt,
    /// it makes `/agent Codex` spawn a sub-agent while `/agent codex` switches.
    pub fn knows_agent(&self, id: &str) -> bool {
        let id = id.trim();
        !id.is_empty()
            && self
                .choices()
                .iter()
                .any(|choice| choice.id.eq_ignore_ascii_case(id))
    }

    /// Whether an in-process kernel backend was registered under this id.
    ///
    /// `/kernel` asks this. It runs in the front end rather than here because it
    /// reaches into the host for loop vendors, so the command stayed behind
    /// while the agent set moved out.
    pub fn has_kernel_backend(&self, backend_id: &str) -> bool {
        self.kernel_backends.contains_key(backend_id)
    }

    /// A session with no agent CLIs available: every turn stays on the
    /// local engine.
    ///
    /// Only tests reach this today — every production surface goes
    /// through the harness assembly, which starts from the declared agents
    /// even when there are none. Kept because "this host offers no
    /// agents" is a real state, not a test artifact.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn local_only(
        switch: Arc<AgentBackendSwitch>,
        local: Arc<dyn AgentBackend>,
        projects_root: impl Into<PathBuf>,
        cwd: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self::new(
            BackendRegistry::empty(),
            std::collections::BTreeMap::new(),
            switch,
            local,
            projects_root,
            cwd,
            session_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: BackendRegistry<B>,
        origins: std::collections::BTreeMap<String, AgentOrigin>,
        switch: Arc<AgentBackendSwitch>,
        local: Arc<dyn AgentBackend>,
        projects_root: impl Into<PathBuf>,
        cwd: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            origins,
            switch,
            local,
            projects_root: projects_root.into(),
            cwd: cwd.into(),
            session_id: session_id.into(),
            current: Arc::new(Mutex::new(SESSION_AGENT_LOCAL.to_string())),
            config_error: None,
            session_resource: None,
            kernel_backends: std::collections::BTreeMap::new(),
        }
    }

    /// Keep a host-owned resource alive until this immutable session is dropped.
    /// The router owns the lifetime, not the resource's transport or implementation.
    pub fn with_session_resource(mut self, resource: Box<dyn Send + Sync>) -> Self {
        self.session_resource = Some(resource);
        self
    }

    /// The session's lazy backend registry, including its cached connections.
    pub fn registry(&self) -> &BackendRegistry<B> {
        &self.registry
    }

    /// Register an in-process kernel backend under a `kernel:`-prefixed
    /// id. Host-owned: these never come from `acpAgents`, so the prefix
    /// keeps the namespaces from colliding (the `remote:` precedent).
    pub fn with_kernel_backend(
        mut self,
        id: impl Into<String>,
        label: impl Into<String>,
        backend: Arc<dyn AgentBackend>,
    ) -> Self {
        self.kernel_backends
            .insert(id.into(), (label.into(), backend));
        self
    }

    /// Record that the `acpAgents` config could not be read.
    ///
    /// The config layer fails the whole read rather than dropping the
    /// broken entry, precisely so the user is not shown a list that is
    /// quietly missing the agent they just configured. This is the
    /// other half of that contract: the host that caught the error
    /// hands it here, and `/agent` repeats it instead of pretending
    /// nothing was configured.
    pub fn with_config_error(mut self, error: Option<String>) -> Self {
        self.config_error = error;
        self
    }

    /// The session these agents serve for their entire lifetime.
    pub fn session(&self) -> String {
        self.session_id.clone()
    }

    /// Root/cwd/session triple used by transcripts, handoff sidecars, and
    /// file-history adapters owned by this registry.
    pub fn session_binding(&self) -> (&Path, &str, &str) {
        (&self.projects_root, &self.cwd, &self.session_id)
    }

    /// Retire every backend session owned by this immutable runtime.
    ///
    /// Called only when the last `Arc<SessionRuntime>` releases this registry,
    /// so in-flight and detached turns have already finished. The close is
    /// asynchronous because ACP clients and kernel hosts both use async
    /// transports; dropping the host-fs handle itself remains synchronous.
    pub fn retire(&self, runtime: &tokio::runtime::Handle) {
        let session_id = self.session_id.clone();
        let mut backends: Vec<Arc<dyn AgentBackend>> = self
            .registry
            .ids()
            .into_iter()
            .filter_map(|id| self.registry.backend(&id).ok())
            .map(|backend| backend as Arc<dyn AgentBackend>)
            .collect();
        backends.extend(
            self.kernel_backends
                .values()
                .map(|(_, backend)| backend.clone()),
        );
        for backend in backends {
            let session_id = session_id.clone();
            runtime.spawn(async move {
                if let Err(err) = backend.close_session(&session_id).await {
                    tracing::warn!(
                        error = %err,
                        session_id = %session_id,
                        "session runtime: backend retirement failed"
                    );
                }
            });
        }
    }

    /// The agent this session's turns currently go to.
    pub fn current_id(&self) -> String {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// What the current agent can deliver.
    pub fn capabilities(&self) -> AgentCapabilities {
        self.switch.capabilities()
    }

    /// Push a message into the turn the current agent is running.
    ///
    /// The caller keeps the message until this succeeds: every error
    /// here means "not delivered", and the queue that holds it will
    /// send it as an ordinary prompt once the turn ends.
    pub async fn steer(
        &self,
        session_id: &str,
        blocks: Vec<rebon_types::ContentBlock>,
        user_message_uuid: &str,
    ) -> Result<crate::SteerOutcome, crate::AgentBackendError> {
        self.switch
            .steer(session_id, blocks, user_message_uuid)
            .await
    }

    /// Whether the current agent's sessions carry Rebon's injected
    /// `write_file`/`edit_file` tools. `false` on the local engine —
    /// its writes already go through the tracker.
    pub fn current_agent_has_fs_tools(&self) -> bool {
        let current = self.current_id();
        if current == SESSION_AGENT_LOCAL {
            return false;
        }
        self.registry
            .backend(&current)
            .map(|backend| !backend.injected_mcp_servers().is_empty())
            .unwrap_or(false)
    }

    /// Every choice, local first, with the current one marked.
    pub fn choices(&self) -> Vec<AgentChoice> {
        let current = self.current_id();
        let mut choices = vec![AgentChoice {
            id: SESSION_AGENT_LOCAL.to_string(),
            label: "Rebon (local engine)".to_string(),
            current: current == SESSION_AGENT_LOCAL,
            origin: None,
            caveat: None,
        }];
        for id in self.registry.ids() {
            let label = self
                .registry
                .resolve(&id)
                .map(|(_, label)| label)
                .unwrap_or_else(|| id.clone());
            choices.push(AgentChoice {
                current: current == id,
                origin: self.origins.get(&id).copied(),
                caveat: Some(ACP_CAVEAT.to_string()),
                id,
                label,
            });
        }
        for (id, (label, _backend)) in &self.kernel_backends {
            choices.push(AgentChoice {
                id: id.clone(),
                label: label.clone(),
                current: current == *id,
                origin: Some(AgentOrigin::Kernel),
                caveat: Some(KERNEL_CAVEAT.to_string()),
            });
        }
        choices
    }

    /// Stops one agent's process so the next turn dials it again.
    ///
    /// For an agent that went bad without going away — a CLI that stopped
    /// answering, one whose own configuration changed on disk, one left over
    /// from a machine that slept. Reconnecting is not switching: the session
    /// stays on whichever agent it was on, and nothing about the conversation
    /// moves.
    ///
    /// The old backend is stopped *after* the registry has let go of it, so a
    /// process on its way out never holds another agent's lookup.
    pub fn reconnect(
        &self,
        id: Option<&str>,
        runtime: &tokio::runtime::Handle,
    ) -> Result<String, String> {
        let id = match id {
            Some(id) => id.to_string(),
            None => self.current_id(),
        };
        if id == SESSION_AGENT_LOCAL {
            return Err(
                "the local engine is in this process; there is no connection to remake".to_string(),
            );
        }
        let (label, _) = self
            .registry
            .resolve(&id)
            .ok_or_else(|| format!("no agent named `{id}`"))?;
        let dropped = self.registry.reconnect(&id).map_err(|e| e.to_string())?;
        match dropped {
            Some(backend) => {
                // `Handle::block_on` panics on a runtime worker thread, and
                // this command reaches one through the host's IPC path (a front
                // end dispatches it from a `spawn_blocking` worker, where it
                // happens to be legal). A dedicated thread may block on the
                // handle from every dispatch surface.
                let runtime = runtime.clone();
                let _ = std::thread::spawn(move || runtime.block_on(backend.shutdown())).join();
                Ok(format!(
                    "{label} disconnected. The next turn on it starts a fresh session."
                ))
            }
            // Nothing had been dialed, so there is nothing to redo — and saying
            // "reconnected" would claim an act that did not happen.
            None => Ok(format!("{label} was not running; the next turn starts it.")),
        }
    }

    /// Point this session's next turn at `id`.
    ///
    /// Switching mid-turn is not blocked here — the in-flight turn
    /// stays on the backend it started on, and the change takes effect
    /// on the next one.
    pub fn switch_to(&self, id: &str) -> Result<String, String> {
        let requested = id.trim();
        if requested.is_empty() {
            return Err("name an agent, or `local` for rebon's own engine".to_string());
        }
        let previous = self.current_id();

        if requested.eq_ignore_ascii_case(SESSION_AGENT_LOCAL) {
            self.switch.switch(self.local.clone());
            self.set_current(SESSION_AGENT_LOCAL);
            self.close_replaced_kernel(&previous, SESSION_AGENT_LOCAL);
            return Ok("Rebon (local engine)".to_string());
        }

        // In-process kernel backends first: their ids live in a prefixed
        // namespace (`kernel:`), so a hit here can never shadow a
        // configured ACP agent.
        if let Some((id, (label, backend))) = self
            .kernel_backends
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(requested))
        {
            self.switch.switch(backend.clone());
            let (id, label) = (id.clone(), label.clone());
            self.set_current(&id);
            self.close_replaced_kernel(&previous, &id);
            return Ok(label);
        }

        let (canonical, label) = self.registry.resolve(requested).ok_or_else(|| {
            let configured = self.registry.ids();
            if configured.is_empty() {
                if let Some(error) = &self.config_error {
                    format!(
                        "no agent named `{requested}` — the `acpAgents` entries in \
                         config.json could not be read: {error}"
                    )
                } else {
                    format!(
                        "no agent named `{requested}`, and no agents are configured — \
                         add one under `acpAgents` in config.json"
                    )
                }
            } else {
                format!(
                    "no agent named `{requested}` (configured: {})",
                    configured.join(", ")
                )
            }
        })?;

        let backend = self
            .registry
            .backend(&canonical)
            .map_err(|err| err.to_string())?;

        // Hand back the agent's own session id from a previous run,
        // read *before* `set_current` rewrites the sidecar. Without
        // this the agent is asked for a fresh session on every restart
        // and the conversation on screen has no counterpart on its
        // side. Only when the sidecar records this same agent — an id
        // minted by a different one means nothing to this one.
        let stored =
            rebon_session::load_session_agent(&self.projects_root, &self.cwd, &self.session())
                .filter(|recorded| recorded.eq_ignore_ascii_case(&canonical))
                .and_then(|_| {
                    rebon_session::load_agent_session_id(
                        &self.projects_root,
                        &self.cwd,
                        &self.session(),
                    )
                });
        if let Some(agent_session_id) = stored {
            backend.remember_agent_session(&self.session(), &agent_session_id);
        }

        self.switch.switch(backend);
        self.set_current(&canonical);
        self.close_replaced_kernel(&previous, &canonical);
        Ok(label)
    }

    /// Wind down the kernel loop a switch just walked away from.
    ///
    /// A kernel backend's session is a live isolate (in-process) or a
    /// sidecar process; no other cleanup point on the switch path calls
    /// `close_session`, so without this every vendor switch leaked one
    /// until process exit. ACP backends are exempt on purpose: their
    /// pooled processes are shared and reconnectable.
    ///
    /// The close is asynchronous, so it re-reads which agent the session is
    /// on immediately before acting: `dsh → local → dsh` must not have the
    /// first close land on the backend the second switch just returned to.
    /// A switch that happens inside that last instant still races, and the
    /// outcome is deliberately survivable rather than guarded further — the
    /// kernel backend refuses to close a session with a turn in flight
    /// (`close_pending`) and refuses to start a turn on a closed one
    /// (`claim_turn`), so the worst case is an idle host torn down and
    /// rebuilt by the next prompt. Loop sessions mount no persistence
    /// backend, so there is no conversation state to lose.
    fn close_replaced_kernel(&self, previous: &str, next: &str) {
        if previous.eq_ignore_ascii_case(next) {
            return;
        }
        let Some((_, backend)) = self.kernel_backends.get(previous) else {
            return;
        };
        let backend = backend.clone();
        let session_id = self.session();
        let previous = previous.to_string();
        let current = self.current.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let switched_back = current
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .eq_ignore_ascii_case(&previous);
                if switched_back {
                    tracing::debug!(
                        agent = %previous,
                        "kernel loop: close skipped — the session switched back before it ran"
                    );
                    return;
                }
                if let Err(err) = backend.close_session(&session_id).await {
                    tracing::warn!(error = %err, "kernel loop: close on switch failed");
                }
            });
        }
        // Without a runtime (sync-only tests) the process-exit teardown
        // remains the backstop, as before this fix.
    }

    fn set_current(&self, id: &str) {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = id.to_string();
        // Persisted immediately rather than at the end of the turn: a
        // session reopened after a crash should come back on the agent
        // the user chose, not the one the config defaults to.
        if let Err(err) =
            rebon_session::save_session_agent(&self.projects_root, &self.cwd, &self.session(), id)
        {
            tracing::warn!(
                error = %err,
                session_id = %self.session(),
                agent = id,
                "rebon: failed to remember which agent runs this session"
            );
        }
    }
}

impl<B: AgentBackend + 'static> std::fmt::Debug for SessionAgents<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAgents")
            .field("current", &self.current_id())
            .field("configured", &self.registry.ids())
            .finish()
    }
}

/// What an ACP agent cannot promise, in one line the UI can show.
///
/// Both halves are real: ACP's prompt result carries a stop reason and
/// nothing else, and rewind only covers writes the agent routed back
/// through Rebon. An agent that writes to disk itself leaves nothing to
/// restore, and the honest thing is to say so up front.
pub const ACP_CAVEAT: &str = "no token usage; rewind covers only writes routed through Rebon";

/// What the embedded kernel loop cannot promise yet, in one line.
///
/// Its tools run through Rebon's engine pipeline (validation +
/// permission grants), but the kernel tool seat does not arm file-history
/// snapshots, non-granted writes fail closed instead of asking, and no
/// dsh persistence backend is mounted — so rewind, interactive
/// permission prompts, and resume are all off the table for now.
pub const KERNEL_CAVEAT: &str =
    "no rewind; ungranted writes fail closed (kernelPlugins.toolGrants); sessions do not resume";

/// Which agent this session should open on.
///
/// The session's own record wins over the configured default: a user
/// who switched a session to an agent — or explicitly back to local —
/// expects to reopen it where they left it, not where the config points
/// today. `None` means the local engine.
pub fn initial_agent(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    configured_default: Option<String>,
) -> Option<String> {
    match rebon_session::load_session_agent(projects_root, cwd, session_id) {
        Some(stored) if stored.eq_ignore_ascii_case(SESSION_AGENT_LOCAL) => None,
        Some(stored) => Some(stored),
        None => configured_default,
    }
}

// ── the `/agent` and `/backend` commands ──────────────────────────

/// Strip `/name` off a line whatever case it was typed in, and return the rest.
///
/// The catalog's promise is that case never decides what a command is, and the
/// terminal's other parsers keep it. These two are reached from that same
/// submit path and have to agree with them, or `/Backend list` goes to the
/// model as prompt text while `/Compact` runs.
fn strip_command_prefix<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let rest = text.strip_prefix('/')?;
    // `get` rather than indexing: a line opening with a multi-byte character
    // would split one mid-way and panic.
    rest.get(..name.len())
        .is_some_and(|typed| typed.eq_ignore_ascii_case(name))
        .then(|| &rest[name.len()..])
}

/// Returns `Some(args)` when `text` is the `/agent` command.
///
/// `/agents` (the agent-definition browser) is a different command and
/// must not be swallowed here.
pub fn parse_agent_command(text: &str) -> Option<&str> {
    let rest = strip_command_prefix(text.trim_end(), "agent")?;
    if rest.starts_with(['s', 'S']) {
        return None;
    }
    let args = rest.trim_start_matches([' ', ':']).trim();
    if rest.is_empty() || rest.starts_with(' ') || rest.starts_with(':') {
        Some(args)
    } else {
        None
    }
}

/// Returns `Some(args)` when `text` is the `/backend` command.
///
/// The unambiguous name for the switch. `/agent` reaches it too, but also
/// means "spawn a sub-agent with this prompt", so which one a line meant
/// depends on whether it named an agent; this one never has to be guessed at.
/// Space and colon forms both work (`/backend codex`, `/backend:codex`).
pub fn parse_backend_command(text: &str) -> Option<&str> {
    let rest = strip_command_prefix(text.trim_end(), "backend")?;
    let args = rest.trim_start_matches([' ', ':']).trim();
    if rest.is_empty() || rest.starts_with(' ') || rest.starts_with(':') {
        Some(args)
    } else {
        None
    }
}

/// `Some(target)` when these arguments are the `reconnect` subcommand:
/// `Some(None)` for the current agent, `Some(Some(id))` for a named one.
///
/// Strict about what follows the word on purpose. `reconnect the parser` is a
/// sub-agent prompt that happens to open with it, and the terminal decides from
/// this same answer whether `/agent …` is a command or a prompt — so a loose
/// match here would start swallowing prompts.
fn reconnect_target(args: &str) -> Option<Option<&str>> {
    const WORD: &str = "reconnect";
    args.get(..WORD.len())
        .filter(|head| head.eq_ignore_ascii_case(WORD))?;
    let rest = &args[WORD.len()..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let named = rest.trim();
    if named.is_empty() {
        return Some(None);
    }
    (!named.contains(char::is_whitespace)).then_some(Some(named))
}

/// Whether these arguments ask this command for something, rather than being a
/// sub-agent prompt that landed on `/agent`.
///
/// `/agent` carries two meanings and the terminal has to pick one before it
/// acts. The predicate lives next to [`handle_agent_command`] because it has to
/// agree with it in both directions: a line this rejects is spawned as a prompt
/// and never reaches the switch, and a line it accepts had better be something
/// the switch understands. They read the same three tests to keep that true.
pub fn agent_args_name_the_switch<B: AgentBackend + 'static>(
    agents: &SessionAgents<B>,
    args: &str,
) -> bool {
    let args = args.trim();
    args.is_empty()
        || args.eq_ignore_ascii_case("list")
        || reconnect_target(args).is_some()
        || agents.knows_agent(args)
}

/// Whether these arguments would change what runs the session, rather than
/// just describe it.
///
/// What a front end asks before refusing the command mid-turn: listing is
/// always safe, and moving the session onto another backend while a turn is in
/// flight is not. `/kernel` asks the same question of its own vendor names,
/// which is why this takes arguments rather than a [`SessionAgents`].
pub fn args_change_the_backend(args: &str) -> bool {
    let args = args.trim();
    !args.is_empty() && !args.eq_ignore_ascii_case("list")
}

pub struct AgentCommandResult {
    pub text: String,
    pub is_err: bool,
}

/// Run `/agent` or `/backend`, returning what to show the user.
pub fn handle_agent_command<B: AgentBackend + 'static>(
    agents: &SessionAgents<B>,
    args: &str,
    runtime: Option<&tokio::runtime::Handle>,
) -> AgentCommandResult {
    let args = args.trim();
    if args.is_empty() || args.eq_ignore_ascii_case("list") {
        return AgentCommandResult {
            text: render_list(agents),
            is_err: false,
        };
    }

    if let Some(target) = reconnect_target(args) {
        let Some(runtime) = runtime else {
            return AgentCommandResult {
                text: "This surface cannot reconnect an agent — it has no runtime to stop one on."
                    .to_string(),
                is_err: true,
            };
        };
        return match agents.reconnect(target, runtime) {
            Ok(text) => AgentCommandResult {
                text,
                is_err: false,
            },
            Err(text) => AgentCommandResult { text, is_err: true },
        };
    }

    match agents.switch_to(args) {
        Ok(label) => {
            let mut text = format!("Switched to {label}.");
            let capabilities = agents.capabilities();
            if !capabilities.supports_rewind() {
                text.push_str(
                    "\n  /rewind cannot cover this agent's code changes: it decides \
                     whether to route file writes through Rebon, and anything it \
                     writes directly leaves no snapshot to restore.",
                );
                if agents.current_agent_has_fs_tools() {
                    text.push_str(
                        "\n  Rebon injects write_file/edit_file tools into this agent \
                         to route its edits back through the snapshot pipeline; \
                         coverage depends on the agent using them.",
                    );
                }
            }
            if !capabilities.reports_usage {
                text.push_str("\n  Token usage is not reported by this agent.");
            }
            AgentCommandResult {
                text,
                is_err: false,
            }
        }
        Err(message) => AgentCommandResult {
            text: message,
            is_err: true,
        },
    }
}

fn render_list<B: AgentBackend + 'static>(agents: &SessionAgents<B>) -> String {
    let mut out = String::from("Agents for this session:\n");
    for choice in agents.choices() {
        let marker = if choice.current { "*" } else { " " };
        let origin = choice
            .origin
            .map(|origin| format!("  ({})", origin.as_str()))
            .unwrap_or_default();
        out.push_str(&format!(
            "{marker} {} — {}{origin}\n",
            choice.id, choice.label
        ));
        if let Some(caveat) = &choice.caveat {
            out.push_str(&format!("    {caveat}\n"));
        }
    }
    if let Some(error) = &agents.config_error {
        out.push_str(&format!(
            "\nWarning: the `acpAgents` entries in config.json were ignored: {error}\n"
        ));
    }
    // `/backend`, not `/agent`: this list is rendered by front ends where
    // `/agent` is prompt text that reaches the model. The one name that means
    // the switch on every front end is the one to print.
    out.push_str("\nUse /backend <id> to switch, /backend local to go back to rebon's engine.");
    // The how-to hint is for a user who configured nothing — not for
    // one whose configuration failed to read, who needs the warning
    // above, not a suggestion to add more entries to a broken list.
    if agents.choices().len() == 1 && agents.config_error.is_none() {
        out.push_str(
            "\nNo agent CLIs are configured. Add one under `acpAgents` in config.json, e.g.\n  \
             { \"id\": \"claude-code\", \"command\": \"claude\", \"args\": [\"--acp\"] }",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::BackendEntry;
    use crate::{
        AgentBackendError, AgentBackendKind, AgentSessionSpec, AgentSessionStart,
        LocalAgentBackend, PromptExecutorError, PromptOutcome, PromptRequest, StubPromptExecutor,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `reconnect` reads the same way for the terminal deciding what a line is
    /// and for the command running it, because both call this.
    #[test]
    fn reconnect_names_at_most_one_agent() {
        assert_eq!(reconnect_target("reconnect"), Some(None));
        assert_eq!(reconnect_target("reconnect codex"), Some(Some("codex")));
        assert_eq!(reconnect_target("RECONNECT  codex "), Some(Some("codex")));
        assert_eq!(reconnect_target("reconnect the parser"), None);
        assert_eq!(reconnect_target("reconnectfoo"), None);
        assert_eq!(reconnect_target("recon"), None);
        assert_eq!(reconnect_target("重连"), None, "a short multi-byte arg");
    }

    // This backend has no ACP client, engine, config, tool or plugin dependency.
    // Its only contract with the router is the neutral turn/lifecycle seam.
    struct ProbeBackend {
        local: LocalAgentBackend,
        remembered: Mutex<Vec<(String, String)>>,
        shutdowns: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl AgentBackend for ProbeBackend {
        fn kind(&self) -> AgentBackendKind {
            AgentBackendKind::Acp
        }
        fn name(&self) -> &str {
            "probe"
        }
        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities::MINIMAL
        }
        async fn start_session(
            &self,
            spec: AgentSessionSpec,
        ) -> Result<AgentSessionStart, AgentBackendError> {
            self.local.start_session(spec).await
        }
        async fn prompt(
            &self,
            request: PromptRequest,
        ) -> Result<PromptOutcome, PromptExecutorError> {
            self.local.prompt(request).await
        }
        fn remember_agent_session(&self, host: &str, agent: &str) {
            self.remembered
                .lock()
                .unwrap()
                .push((host.into(), agent.into()));
        }
        async fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn probe_agents(root: &Path, shutdowns: Arc<AtomicUsize>) -> SessionAgents<ProbeBackend> {
        let local: Arc<dyn AgentBackend> =
            Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)));
        SessionAgents::new(
            BackendRegistry::new([BackendEntry::new("probe", "Probe", move || ProbeBackend {
                local: LocalAgentBackend::new(Arc::new(StubPromptExecutor)),
                remembered: Mutex::new(Vec::new()),
                shutdowns: shutdowns.clone(),
            })]),
            [("probe".into(), AgentOrigin::Config)].into(),
            Arc::new(AgentBackendSwitch::new(local.clone())),
            local,
            root,
            "/work",
            "host",
        )
    }

    #[test]
    fn generic_routing_success_failure_and_boundary_matrix() {
        let root = tempfile::tempdir().unwrap();
        let agents = probe_agents(root.path(), Arc::new(AtomicUsize::new(0)));
        assert!(agents.registry().live_backends().is_empty());
        for (requested, succeeds, current, caps) in [
            ("", false, "local", AgentCapabilities::LOCAL),
            ("missing", false, "local", AgentCapabilities::LOCAL),
            (" PROBE ", true, "probe", AgentCapabilities::MINIMAL),
            ("重连", false, "probe", AgentCapabilities::MINIMAL),
            ("probe", true, "probe", AgentCapabilities::MINIMAL),
            (" LoCaL ", true, "local", AgentCapabilities::LOCAL),
        ] {
            assert_eq!(agents.switch_to(requested).is_ok(), succeeds, "{requested}");
            assert_eq!(agents.current_id(), current);
            assert_eq!(agents.capabilities(), caps);
            assert!(!agents.current_agent_has_fs_tools());
            assert_eq!(
                agents
                    .choices()
                    .iter()
                    .filter(|choice| choice.current)
                    .count(),
                1
            );
        }
        let cached = agents.registry().backend("probe").unwrap();
        agents.switch_to("probe").unwrap();
        assert!(Arc::ptr_eq(
            &cached,
            &agents.registry().backend("PROBE").unwrap()
        ));
        assert_eq!(agents.session_binding(), (root.path(), "/work", "host"));
    }

    #[test]
    fn resume_sidecar_is_offered_only_to_the_matching_agent_and_binding() {
        for (stored_agent, stored_cwd, stored_id, expected) in [
            ("PROBE", "/work", Some("external"), true),
            ("other", "/work", Some("external"), false),
            ("probe", "/other", Some("external"), false),
            ("probe", "/work", None, false),
        ] {
            let root = tempfile::tempdir().unwrap();
            rebon_session::save_session_agent(root.path(), stored_cwd, "host", stored_agent)
                .unwrap();
            if let Some(id) = stored_id {
                rebon_session::save_agent_session_id(root.path(), stored_cwd, "host", id).unwrap();
            }
            let agents = probe_agents(root.path(), Arc::new(AtomicUsize::new(0)));
            agents.switch_to("probe").unwrap();
            let backend = agents.registry().backend("probe").unwrap();
            let offers = backend.remembered.lock().unwrap();
            assert_eq!(
                offers.len(),
                usize::from(expected),
                "{stored_agent}/{stored_cwd}/{stored_id:?}"
            );
            if expected {
                assert_eq!(offers[0], ("host".into(), "external".into()));
            }
            assert_eq!(
                rebon_session::load_session_agent(root.path(), "/work", "host").as_deref(),
                Some("probe")
            );
        }
    }

    #[test]
    fn reconnect_cold_warm_unknown_and_local_preserves_the_selected_agent() {
        let root = tempfile::tempdir().unwrap();
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let agents = probe_agents(root.path(), shutdowns.clone());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(agents
            .reconnect(Some("probe"), runtime.handle())
            .unwrap()
            .contains("not running"));
        agents.switch_to("probe").unwrap();
        let first = agents.registry().backend("probe").unwrap();
        assert!(agents
            .reconnect(None, runtime.handle())
            .unwrap()
            .contains("disconnected"));
        assert_eq!(agents.current_id(), "probe");
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        assert!(agents.registry().live_backends().is_empty());
        assert!(agents.reconnect(Some("missing"), runtime.handle()).is_err());
        assert!(agents.reconnect(Some("local"), runtime.handle()).is_err());
        agents.switch_to("probe").unwrap();
        assert!(!Arc::ptr_eq(
            &first,
            &agents.registry().backend("probe").unwrap()
        ));
    }

    #[test]
    fn host_resource_lives_until_the_last_session_owner_drops() {
        struct Resource(Arc<AtomicUsize>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let root = tempfile::tempdir().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let agents = Arc::new(
            probe_agents(root.path(), Arc::new(AtomicUsize::new(0)))
                .with_session_resource(Box::new(Resource(drops.clone()))),
        );
        let held = agents.clone();
        drop(agents);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(held.session(), "host");
        drop(held);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
