//! One session assembly, shared by every surface.
//!
//! The TUI, the ACP server and the headless harness each used to build the
//! same session out of the same pieces in the same order, in three separate
//! functions. What kept them apart was not the pieces but the shape: each
//! surface threaded fifty-odd values through one long function and dropped
//! them into its own final struct at the end. Extracting phases that *return*
//! their outputs costs one line of interface per output, which is why the
//! obvious split never paid for itself.
//!
//! So the phases here take `&mut` this accumulator instead. A phase reads what
//! earlier phases left and writes what later ones need; a call site is one
//! line whatever the phase produces.
//!
//! # Order
//!
//! Three constraints in this assembly are real -- getting them wrong produces
//! a session that builds and then behaves wrongly -- so they are types rather
//! than comments:
//!
//! * [`SessionAssembly::fix_tools`] must run before the engine lists its tools
//!   for the first time. The `Agent` tool renders the agent list into its own
//!   description off a process-wide cell; publishing it late leaves the first
//!   turn describing the compiled-in built-ins only.
//! * [`ToolsFixed::resolve_runtime`] must run before anything builds an
//!   executor, which needs the resolved client and model.
//! * [`RuntimeResolved::bind_session`] must run before the permission broker and
//!   the kernel scope, both of which are keyed by the session id.
//!
//! Each gate consumes the previous stage, so the compiler enforces the order
//! and no surface can rediscover it by reading comments.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rebon_core::auto_mode_classifier::ModelAutoModeClassifier;
use rebon_core::permission::{ChannelPermissionBroker, SharedChannelPermissionBroker};
use rebon_core::policy::PolicyStore;
use rebon_core::Engine;
use rebon_permissions::denial_sink::AutoModeHooks;
use rebon_tool::{AgentRegistry, ToolFilter};

use crate::{kernel_bootstrap, projects_root};
use rebon_kernel_seats::kernel_services::SessionKernelScopes;

/// The receiving half of the permission channel, handed to whichever surface
/// answers the prompts.
type OutboundPermissionRx =
    tokio::sync::mpsc::UnboundedReceiver<rebon_core::permission::OutboundPermissionQuery>;

use crate::{
    build_default_policy_store, build_default_tool_filter_for_context, resolve_runtime_model,
    HarnessOverrides, RuntimeModel,
};

/// Where the assembly runs and what it may touch.
///
/// Everything here is settled before any tool list is taken, because the
/// registry and the sub-agent switch are process-wide cells the engine reads
/// on its first listing.
pub struct SessionAssembly {
    /// The caller's inputs, kept whole: later phases read fields this stage
    /// has no opinion about (`max_iterations`, `capability_mode`, ...).
    pub overrides: HarnessOverrides,
    /// Working directory the session runs in.
    pub cwd: PathBuf,
    /// The store this session's files go to. Settled here rather than at each
    /// call site because three later phases derive paths from it — the kernel
    /// scopes, the executor, and the hook context's transcript path — and a
    /// second resolution is how one of them keeps writing to the user's store
    /// while the run claims otherwise.
    pub projects_root: PathBuf,
    /// Coordinator (worktree / sub-agent) mode for this session.
    pub coordinator_mode: bool,
    /// Whether coordinator workers get their own worktree.
    pub coordinator_use_worktree: bool,
    /// Permission rules from settings and env. None means ACP owns loading at
    /// session activation, not at this server's unrelated startup cwd.
    pub policy_store: Option<PolicyStore>,
    /// The tool set this session starts with.
    ///
    /// `REBON_ALLOW_TOOLS` is a comma-separated allow list and
    /// `REBON_DENY_TOOLS` a deny list; both are optional, and the
    /// mode-aware base filter still hides queue and coordinator-only tools
    /// outside the contexts that own them.
    pub tool_filter: ToolFilter,
    /// Built-ins plus the user, project and plugin agent definitions.
    ///
    /// User override files shadow the compiled-in definitions -- a
    /// `~/.rebon/agents/rebon-code-guide.md` replaces the built-in of that
    /// name -- and later entries in the override search order win over
    /// earlier ones.
    pub agent_registry: Arc<AgentRegistry>,
}

/// The owner of permission-policy loading for this assembly.
#[derive(Default)]
pub enum PolicyLoading {
    #[default]
    SessionCwd,
    AcpSessionActivation,
}

/// What a surface contributes that the harness cannot discover for itself.
///
/// The plugin runtime lives in the embedding binary -- it reads that binary's
/// `--plugin-dirs` and spawns its processes -- so its results arrive here
/// already materialised rather than being rebuilt per surface.
#[derive(Default)]
pub struct AssemblyInputs<'a> {
    pub policy_loading: PolicyLoading,
    /// Whether this session drives the task queue. Queue sessions see a
    /// different default tool set.
    pub queue_session: bool,
    /// Directories holding agent definition files, from the plugin runtime,
    /// each tagged with the plugin that contributed it.
    pub agent_dirs: &'a [(String, PathBuf)],
    /// Whether agents declared by external CLIs may be spawned. The TUI and
    /// the ACP server say yes; the headless harness has no pool to spawn them
    /// into and says no.
    pub external_agents_spawnable: bool,
    /// Coordinator worktree preference. The headless harness pins this off.
    pub coordinator_use_worktree: bool,
}

impl SessionAssembly {
    /// Settle the inputs no tool listing may precede.
    pub fn begin(overrides: HarnessOverrides, inputs: AssemblyInputs<'_>) -> anyhow::Result<Self> {
        let cwd = match overrides.cwd.as_deref() {
            Some(cwd) => PathBuf::from(cwd),
            None => std::env::current_dir()?,
        };
        // `None` is the user's `projects/`, which is what every embedder got
        // before the field existed. A caller that names one (`rebon exec
        // --ephemeral`) gets a run whose session files land there instead.
        let store_root = overrides
            .projects_root
            .clone()
            .unwrap_or_else(projects_root);
        // Each surface settles this differently -- the headless harness from
        // the caller's flag, the TUI from the env default and then whatever mode
        // a resumed session was saved under -- so it arrives decided.
        let coordinator_mode = overrides.coordinator_mode;
        let policy_store = match inputs.policy_loading {
            PolicyLoading::SessionCwd => Some(build_default_policy_store(&cwd)?),
            PolicyLoading::AcpSessionActivation => None,
        };
        let tool_filter =
            build_default_tool_filter_for_context(coordinator_mode, inputs.queue_session);
        if !tool_filter.is_unrestricted() {
            tracing::info!(
                allow = ?tool_filter.allow_list(),
                deny = ?tool_filter.deny_list(),
                "rebon: loaded default tool filter"
            );
        }
        let mut registry = AgentRegistry::load_with_plugin_dirs(
            Path::new(&cwd),
            &rebon_config::config_home_dir(),
            inputs.agent_dirs,
        );
        if inputs.external_agents_spawnable {
            registry = registry.with_external_agents_spawnable();
        }
        Ok(Self {
            overrides,
            cwd,
            projects_root: store_root,
            coordinator_mode,
            coordinator_use_worktree: inputs.coordinator_use_worktree,
            policy_store,
            tool_filter,
            agent_registry: Arc::new(registry),
        })
    }

    /// Publish the agent registry, then build the engine.
    ///
    /// The registry is a process-wide cell the `Agent` tool reads when it
    /// renders its own description, so it is published *before* the engine
    /// exists rather than after: an engine that lists its tools first would
    /// describe the compiled-in built-ins and nothing else for the whole first
    /// turn.
    ///
    /// The sub-agent switch is deliberately *not* set here. Each surface
    /// already sets it at its own point with its own source -- the headless
    /// harness from the caller's flag right after this, the TUI and the ACP
    /// server from the saved setting much later -- and moving those calls
    /// earlier would change when the switch takes effect.
    pub fn fix_tools(self) -> ToolsFixed {
        rebon_tool::set_agent_registry_selection(
            self.agent_registry.clone(),
            self.coordinator_use_worktree,
        );
        let engine = Arc::new(Engine::with_builtin_tools());
        ToolsFixed {
            assembly: self,
            engine,
        }
    }
}

/// The tool surface is fixed and the engine exists.
pub struct ToolsFixed {
    /// Everything settled before the engine.
    pub assembly: SessionAssembly,
    /// The engine every executor on this session runs against.
    ///
    /// It keeps its `DenyAsk` default permission broker: the executor
    /// overrides that per call once a prompt is flowing.
    pub engine: Arc<Engine>,
}

impl ToolsFixed {
    /// Resolve the provider, the model and the middleware stack.
    ///
    /// This is the one network-shaped step in the assembly: it may refresh an
    /// OAuth token and it may reach a kernel plugin route, so it is `async`
    /// as well as fallible.
    pub async fn resolve_runtime(self) -> anyhow::Result<RuntimeReady> {
        let runtime = resolve_runtime_model(&self.assembly.overrides).await?;
        runtime.publish_provider_capabilities();
        Ok(RuntimeReady {
            tools: self,
            runtime,
        })
    }
}

/// The model client is resolved; an executor can be built.
pub struct RuntimeReady {
    /// The engine and everything before it.
    pub tools: ToolsFixed,
    /// The resolved client, model and the handles the surfaces read at
    /// runtime (retry state, prune level, fast tier).
    pub runtime: RuntimeModel,
}

impl RuntimeReady {
    /// Take the assembly apart into the pieces a surface wires up.
    ///
    /// The fourth value is proof that the runtime was resolved. A surface
    /// spends the pieces immediately but binds its session hundreds of lines
    /// later, once it knows the session id, so the gate travels as a token it
    /// cannot mint rather than as a value it would have to keep whole.
    pub fn into_parts(self) -> (SessionAssembly, Arc<Engine>, RuntimeModel, RuntimeResolved) {
        let store_root = self.tools.assembly.projects_root.clone();
        (
            self.tools.assembly,
            self.tools.engine,
            self.runtime,
            RuntimeResolved(store_root),
        )
    }

    /// The engine, for the phases that only need that.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.tools.engine
    }

    /// Everything settled before the engine.
    pub fn assembly(&self) -> &SessionAssembly {
        &self.tools.assembly
    }
}

/// The session id is known; the kernel scope and the permission broker can be
/// keyed by it.
pub struct SessionBound {
    /// The session these scopes and brokers belong to.
    pub session_id: String,
    /// One bounded table owning this session's kernel scopes. Each turn takes
    /// its own lease through the broker's resolver.
    pub kernel_scopes: Arc<SessionKernelScopes>,
    /// The runtime switch the auto-mode classifier reads.
    pub runtime_model: rebon_core::query::SharedRuntimeModel,
    /// The store this session's files go to, the one the assembly settled.
    pub projects_root: PathBuf,
}

/// Proof that [`ToolsFixed::resolve_runtime`] ran.
///
/// Only [`RuntimeReady::into_parts`] makes one, and only a
/// [`RuntimeResolved`] binds a session, so no surface can open a kernel scope
/// or a permission broker against a runtime it never resolved.
///
/// It carries the assembly's store root for the same reason it exists: the
/// phases that bind a session run hundreds of lines later, and one thing they
/// must not do is resolve a second root behind the caller's back.
pub struct RuntimeResolved(PathBuf);

impl RuntimeResolved {
    /// Boot the kernel composition and open this session's scope table.
    ///
    /// The composition boots before the scopes because a scope is opened
    /// against it. Booting it also boots (or reuses) the process-wide plugin
    /// kernel: the built-in plugin list rides along, and session-scoped
    /// services fork off it here. A refusal is returned rather than reported: a headless run
    /// has nobody in front of it and logs, while an interactive one shows the
    /// message as a startup notice, and that choice is the surface's.
    pub async fn bind_session(
        self,
        engine: &Arc<Engine>,
        runtime_model: rebon_core::query::SharedRuntimeModel,
        session_id: String,
        existing_kernel_scopes: Option<Arc<SessionKernelScopes>>,
    ) -> (
        SessionBound,
        Option<rebon_plugin_host::plugin_boot::CompositionRefusal>,
    ) {
        let store_root = self.0;
        let refusal = rebon_plugin_host::plugin_boot::ensure_process_composition(
            &kernel_bootstrap::process_plugin_registry(),
        )
        .await;
        let kernel_scopes = existing_kernel_scopes.unwrap_or_else(|| {
            SessionKernelScopes::new(
                kernel_bootstrap::process_kernel(),
                engine.clone(),
                store_root.clone(),
            )
        });
        // Bind all session services before handing the runtime to consumers.
        drop(kernel_scopes.acquire(&session_id));
        (
            SessionBound {
                session_id,
                kernel_scopes,
                runtime_model,
                projects_root: store_root,
            },
            refusal,
        )
    }
}

impl SessionBound {
    /// Build the typed permission broker this session answers tool requests
    /// through.
    ///
    /// `auto_mode` is the surface's own short-circuit wiring -- the mode cell
    /// a TUI mirrors from its own state, the denial store its `/permissions`
    /// view reads. It is installed before the broker is shared, because after
    /// that the broker is behind an `Arc` and nobody may reach in.
    ///
    /// The kernel resolver goes on last for the same reason, and the classifier
    /// needs the resolved runtime model, which is why this stage sits behind
    /// [`RuntimeReady`]. That classifier judges every approval request nothing
    /// else resolved; without it a surface fails closed and every such call is
    /// refused for want of an opinion.
    pub fn build_permissions(
        &self,
        auto_mode: Option<AutoModeHooks>,
    ) -> (SharedChannelPermissionBroker, OutboundPermissionRx) {
        let (broker, permission_rx) = ChannelPermissionBroker::new(self.session_id.clone());
        if let Some(hooks) = auto_mode {
            broker.set_auto_mode_hooks(Some(hooks));
        }
        broker.set_auto_mode_classifier(Some(Arc::new(ModelAutoModeClassifier::new(
            self.runtime_model.clone(),
        ))));
        broker.set_kernel_context_resolver(self.kernel_scopes.resolver());
        let broker = Arc::new(broker);
        // A5: the plane's ask surface gets its front end here, where the
        // channel a user actually watches is born.
        //
        // Here rather than in each front end because this *is* the one seam
        // every front end shares: the TUI drains this receiver into its
        // permission modal, a background worker forwards it over IPC to the
        // desktop's permission pane, `rebon exec` answers it from its own
        // policy. Installing once, at the broker, is what makes a Node
        // plugin's ungranted write a question somebody sees instead of a
        // hundred-and-twenty-second wait that ends in deny.
        //
        // Installing replaces: a session build is a new foreground session,
        // and the previous one's outstanding asks are denied with it.
        rebon_kernel_seats::kernel_tool_asks::install_process_ask_front_end(
            kernel_bootstrap::process_kernel().context(),
            &broker,
            &self.session_id,
        );
        (broker, permission_rx)
    }

    /// Build the policy-event handle this session's turns raise events on.
    ///
    /// Two halves, as [`rebon_core::policy_seat::PolicySources`] describes:
    /// the process seat, where a plugin subscribed once, and this session's
    /// own subscriber for the hooks the user configured.
    ///
    /// Plugin hooks arrive already materialised: they come from the embedding
    /// binary's plugin runtime, which the harness has no way to run itself.
    /// The transcript path is derived here rather than passed in because it is
    /// a function of the session this stage is already bound to.
    pub fn build_policy_sources(
        &self,
        cwd: &str,
        plugin_hooks: Vec<rebon_hooks::IndividualHookConfig>,
    ) -> rebon_core::policy_seat::PolicySources {
        policy_sources_for_session(&self.session_id, cwd, plugin_hooks, &self.projects_root)
    }
}

/// The policy-event handle a session's turns raise events on, for a caller
/// that has no [`SessionBound`] to ask.
///
/// The one constructor. Every surface that runs turns — the TUI's session
/// builder, `build_headless_session` (the desktop app and `rebon exec`), and
/// the per-session resolver `--acp` and `serve` hand the executor — has to
/// end up with the same three subscribers, and a second copy of this list is
/// how one of them silently stops running the user's hooks.
///
/// The store root is passed rather than resolved: a hook reads the transcript
/// path off this context, and an ephemeral run's hooks have to be handed the
/// path their own session writes to, not the one the user's store would use.
pub fn policy_sources_for_session(
    session_id: &str,
    cwd: &str,
    plugin_hooks: Vec<rebon_hooks::IndividualHookConfig>,
    projects_root: &Path,
) -> rebon_core::policy_seat::PolicySources {
    let mut sources = rebon_core::policy_seat::PolicySources::default().with_context(
        rebon_core::policy_seat::PolicyContext {
            cwd: cwd.to_string(),
            transcript_path: rebon_session::transcript_file_path(projects_root, cwd, session_id)
                .to_string_lossy()
                .to_string(),
            session_id: session_id.to_string(),
            ..Default::default()
        },
    );
    if let Some(seat) = rebon_kernel_seats::kernel_core_tools::process_policy_event_seat(
        &kernel_bootstrap::process_kernel(),
    ) {
        sources = sources.with_seat(seat);
    }
    sources.with_subscriber(
        rebon_core::policy_seat::SETTINGS_HOOKS_SUBSCRIBER_ID,
        rebon_core::turn_hook::Order::NORMAL,
        Arc::new(rebon_core::hooks::SettingsHookSubscriber::new().with_plugin_hooks(plugin_hooks)),
    )
}
