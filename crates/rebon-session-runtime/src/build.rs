//! Assembling a session: from the flags a run started with to a live
//! [`crate::EngineSession`].
//!
//! This is the one place that knows the order — resolve the model, publish
//! the tool registry, fix the filter, bind the kernel scope, build the
//! permission broker and the hook runtime, load the transcript, install the
//! runtime — and it is shared by every surface that opens a session. The TUI
//! wraps what comes out in its own shell; the background worker and `serve`
//! take it as it is.

use crate::mcp::{DelayedTuiMcpClient, McpLoadRequest, SessionMcp};
use crate::rebon_config::{self, config_home_dir, RuntimeOverride};
use crate::runtime::{
    DeferredEngineCore, DeferredEnginePromptExecutor, EngineCoreInputs, SessionEngineHalf,
    SessionRuntime, SessionRuntimeFactory,
};
use crate::session_handoff::{HandedBack, HeldKernelScopes};
use crate::startup::SessionStartupParams;
use rebon_acp::{DefaultHandler, ServerState};
use rebon_acp_client::{AcpAgentBackend, TranscriptJournal, TranscriptSink};
use rebon_agent_core::model_router::{
    AgentModelRouter, ConfigurableModelRouter, ProviderModelRuntime, ProviderRuntimeResolver,
};
use rebon_agent_core::routing::{DeclaredAgent, SessionAgents};
use rebon_agent_core::{
    AgentBackend, AgentBackendSwitch, ChannelSessionUpdatePublisher, LocalAgentBackend,
    PromptExecutor, SessionUpdatePublisher,
};
use rebon_api::{ModelClient, RetryNotifier, ServiceTierHandle};
use rebon_core::coordinator_mode::{
    coordinator_mode_from_env_default, default_subagent_filter, match_session_mode,
};
use rebon_core::cron::{CompositePoller, CronPoller};
use rebon_core::policy::PolicyStore;
use rebon_core::query::{AttachmentPoller, SharedRuntimeModel, SystemPromptSnapshot};
use rebon_core::Engine;
use rebon_harness::session_assembly::{RuntimeResolved, SessionBound};
use rebon_harness::{detect_os_version, detect_shell, knowledge_cutoff, model_marketing_name};
use rebon_harness::{projects_root, RuntimeModel, SessionModel};
use rebon_harness::{sub_agent_spawner_for_session, team_manager_for_session};
use rebon_permissions::AutoModeVerdictCache;
use rebon_permissions::{
    auto_mode_denials::AutoModeDenialStore,
    denial_sink::{AutoModeHooks, PermissionModeProvider, SharedDenialSink},
    types::PermissionMode,
};
use rebon_plugin_host::provider_registry::ProviderRegistry;
use rebon_plugin_skill::{load_startup_skills, SkillLoaderConfig, SkillRegistry};
use rebon_plugin_tasks::runtime::{TaskRegistry, TaskRegistryRuntimeController};
use rebon_provider::provider_runtime_cache::ProviderRuntimeCache;
use rebon_session::{HeldSessionLock, SessionActiveLock, TranscriptEntry};
use rebon_tool::{
    AgentRegistry, ExternalSubAgentRunner, McpClient, SessionCronStore, SharedCoordinatorMode,
    SharedToolFilter, SubAgentRuntimeHandle, SubAgentSpawner, SubAgentSpawnerRequest,
    TaskRuntimeController, TeamManager, TeamManagerRequest, ToolFilter, UnavailableSubAgentSpawner,
};
use rebon_types::{AgentCapabilityMode, ModelProfileMap, SubAgentModelConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// rebon-first resolution (primary path used by both run modes)
// ---------------------------------------------------------------------------

/// Deliberately not `Clone`: the resolver is shared behind an `Arc`, and
/// cloning it would split the runtime cache into per-clone copies — which is
/// exactly the duplicate-child-process problem the cache exists to prevent.
struct RuntimeProviderResolver {
    runtime_model: SharedRuntimeModel,
    service_tier: ServiceTierHandle,
    /// Keyed by provider id. Without this, every cross-provider sub-agent
    /// spawn re-read config, re-scanned the plugin dirs, re-ran the OAuth
    /// refresh, rebuilt the middleware stack, and — for plugin providers —
    /// spawned a brand new child process.
    /// Shared, so whoever built this resolver can still reach the cache —
    /// `/provider reconnect` has to drop what is in it, and the resolver is
    /// several `Arc<dyn …>` layers deep by the time anything holds it.
    runtime_cache: Arc<ProviderRuntimeCache<RuntimeModel>>,
}

#[async_trait::async_trait]
impl ProviderRuntimeResolver for RuntimeProviderResolver {
    async fn resolve_provider(
        &self,
        provider: Option<&str>,
    ) -> anyhow::Result<ProviderModelRuntime> {
        let provider = provider.map(str::trim).filter(|value| !value.is_empty());
        // No provider named: the main session's runtime, which `/provider`
        // hot-swaps in place. Never cached here — that would pin a stale one.
        let Some(provider) = provider else {
            let current = self.runtime_model.get();
            return Ok(ProviderModelRuntime {
                provider_name: current.provider_name,
                client: current.client,
                default_model: current.model,
                model_profiles: current.model_profiles,
            });
        };
        let resolved = self
            .runtime_cache
            .get_or_try_init(
                provider,
                || resolve_provider_runtime(Some(provider)),
                |runtime| runtime.provider_runtime_cacheable,
            )
            .await?;
        // Re-applied on every hit, not just on build: the fast-tier toggle is
        // live session state and the cached runtime must follow it.
        if resolved.service_tier_available {
            resolved.service_tier.set_fast(self.service_tier.is_fast());
        }
        Ok(ProviderModelRuntime {
            provider_name: resolved.provider_name.clone(),
            client: resolved.client.clone(),
            default_model: resolved.model.clone(),
            model_profiles: resolved.model_profiles.clone(),
        })
    }
}

pub fn model_router_for_runtime(
    runtime_model: SharedRuntimeModel,
    service_tier: ServiceTierHandle,
    model_config: rebon_types::SubAgentModelConfig,
) -> Arc<dyn AgentModelRouter> {
    model_router_and_cache_for_runtime(runtime_model, service_tier, model_config).0
}

/// The same router, plus the provider-runtime cache behind it.
///
/// A caller that can reconnect providers needs the cache; one that only routes
/// does not, and takes the shorter name above.
pub(crate) fn model_router_and_cache_for_runtime(
    runtime_model: SharedRuntimeModel,
    service_tier: ServiceTierHandle,
    model_config: rebon_types::SubAgentModelConfig,
) -> (
    Arc<dyn AgentModelRouter>,
    Arc<ProviderRuntimeCache<RuntimeModel>>,
) {
    let cache = Arc::new(ProviderRuntimeCache::new(&config_home_dir()));
    // Provider seam: plugin-registered provider routes participate in resolution
    // (builtin providers keep precedence; see harness kernel_model_router).
    let resolver = rebon_provider::kernel_model_router::KernelAwareProviderResolver::new(
        rebon_harness::kernel_bootstrap::process_kernel()
            .context()
            .clone(),
        Arc::new(RuntimeProviderResolver {
            runtime_model,
            service_tier,
            runtime_cache: Arc::clone(&cache),
        }),
    );
    (
        Arc::new(ConfigurableModelRouter::new(resolver).with_model_config(model_config)),
        cache,
    )
}

/// Build a provider's runtime from scratch. The caller owns caching and is
/// responsible for re-applying live session state (fast tier) to the result.
async fn resolve_provider_runtime(provider: Option<&str>) -> anyhow::Result<RuntimeModel> {
    let mut overrides = RuntimeOverride::default();
    overrides.provider = provider.map(str::to_string);
    resolve_runtime_model(overrides).await
}

fn build_provider_registry_for_overrides(
    overrides: &RuntimeOverride,
    runtime_cwd: &Path,
) -> anyhow::Result<ProviderRegistry> {
    let plugin_runtime =
        crate::plugin::resolve_runtime_contributions(&crate::plugin::PluginRuntimeOptions {
            cwd: runtime_cwd.to_path_buf(),
            config_home: config_home_dir(),
            plugin_dirs: overrides.plugin_dirs.iter().map(PathBuf::from).collect(),
            rebon_exe: std::env::current_exe().ok(),
        })?;
    for warning in &plugin_runtime.warnings {
        tracing::warn!(warning = %warning, "rebon provider registry: plugin warning");
    }
    let mut registry = ProviderRegistry::with_builtins();
    for contribution in plugin_runtime.model_providers {
        if let Err(warning) = registry.register_plugin_provider(contribution) {
            tracing::warn!(warning = %warning, "rebon provider registry: plugin provider ignored");
        }
    }
    Ok(registry)
}

/// What the SessionStart event sends back: how long it took, and the
/// verdict its subscribers reached.
pub(crate) type SessionStartHookRx =
    std::sync::mpsc::Receiver<(std::time::Duration, rebon_core::policy_seat::Verdict)>;

/// The model `resolve_runtime_model` fills in for a provider entry that
/// names none: the registry's default for the selection. For the first
/// frame's status bar, which otherwise showed an empty model until the
/// session arrived and corrected it. `None` when the registry has no
/// default either, or could not be built.
pub fn registry_default_model(
    overrides: &RuntimeOverride,
    resolved: &rebon_config::ResolvedProvider,
) -> Option<String> {
    let started = std::time::Instant::now();
    let registry =
        build_provider_registry_for_overrides(overrides, &runtime_cwd_path(overrides)).ok()?;
    let model = registry
        .default_model_for(&resolved.provider_selection)
        .map(str::to_string);
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        model = ?model,
        "rebon startup: registry default model resolved for the preview"
    );
    model
}

/// Resolve the model client + default model name used by both run
/// modes, preferring the rebon `.rebon/config.json` active
/// provider when present and falling back to the env var path
/// otherwise.
///
/// The rebon path:
///
/// 1. Loads `config.json` and `.credentials.json` via
///    [`rebon_config::resolve_from_dir_with`], honouring any
///    `--provider` override in [`RuntimeOverride::provider`].
/// 2. If the active provider is OAuth-backed and its cached
///    access token is within the 5-minute expiry buffer, runs a
///    preemptive refresh so the very first request does not have
///    to round-trip a 401. A refresh that fails is logged, never
///    fatal — an invalidated refresh token would otherwise take the
///    whole process down before any surface exists to say `/login`
///    on. The session starts on the cached token; the per-request
///    401 refresher and the `/login` prompt do the recovering.
/// 3. Dispatches on `format` to build the matching rebon-api
///    client. For the OpenAI Responses path (the ChatGPT Codex
///    backend rebon ships with by default), also attaches a
///    [`rebon_provider::oauth_refresher::RebonOAuthRefresher`] so the provider can recover from mid-
///    session 401s by re-running the refresh POST and retrying
///    the request once.
/// 4. Applies [`RuntimeOverride::model`] if set — replaces the
///    provider's `model` field with the CLI value.
/// 5. Wraps the client in the standard [`RetryMiddleware`] +
///    [`LoggingMiddleware`] stack.
///
/// The fallback path runs the original env-var-based construction
/// via [`build_raw_model_client`] and [`default_model`], so
/// CI and fresh environments without a `.rebon` dir still work.
/// The model override applies in both branches.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub async fn resolve_runtime_model(overrides: RuntimeOverride) -> anyhow::Result<RuntimeModel> {
    // One implementation, in the harness. This used to be a second copy that
    // drifted: the harness grew the kernel-plugin-route fallback and this one
    // did not, so a provider only a kernel plugin declared resolved headless
    // and failed in the TUI. What the TUI still owns is its own inputs -- the
    // `--fast` flag, and the plugin contributions its own plugin runtime
    // materialises from `--plugin-dirs`.
    let runtime_cwd = runtime_cwd_path(&overrides);
    let plugin_runtime =
        crate::plugin::resolve_runtime_contributions(&crate::plugin::PluginRuntimeOptions {
            cwd: runtime_cwd,
            config_home: config_home_dir(),
            plugin_dirs: overrides.plugin_dirs.iter().map(PathBuf::from).collect(),
            rebon_exe: std::env::current_exe().ok(),
        })?;
    for warning in &plugin_runtime.warnings {
        tracing::warn!(warning = %warning, "rebon provider registry: plugin warning");
    }
    rebon_harness::resolve_runtime_model(&rebon_harness::HarnessOverrides {
        provider: overrides.provider,
        model: overrides.model,
        cwd: overrides.cwd,
        fast_mode: overrides.fast_mode,
        plugin_model_providers: plugin_runtime.model_providers,
        ..Default::default()
    })
    .await
}

fn runtime_cwd_path(overrides: &RuntimeOverride) -> PathBuf {
    overrides
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Runs for every session build, including background workers and
            // `/new` mid-session; a vanished cwd falls back to the relative
            // root rather than failing the build.
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

/// Where a kernel loop runs: on the plugin plane, which is the only runtime
/// there is.
///
/// A loop's model routes come from the composition's adapters, so a loop runs
/// wherever the composition runs. There used to be two other answers — an
/// isolate on a thread here, or the same isolate in a spawned
/// `rebon-kernel-host` — and both hosted that composition on V8.
pub fn kernel_loop_spawner() -> Arc<dyn rebon_plugin_host::loop_host::LoopHostSpawner> {
    let kernel = rebon_harness::kernel_bootstrap::process_kernel();
    Arc::new(rebon_plugin_host::kernel_loop_plane::PlaneLoopSpawner::new(
        kernel.context().clone(),
    ))
}

// ---------------------------------------------------------------------------
// Full harness session — used by the local TUI run mode.
// ---------------------------------------------------------------------------

/// Read `shellTool` from `config.json` and install it as the process-wide
/// preference, returning what was applied so callers can seed the settings UI
/// with the same value.
///
/// Shared by the TUI and ACP entry points — both have to apply it before the
/// first tool snapshot, and neither should own its own copy of the fallback.
pub fn apply_persisted_shell_tool() -> rebon_tool::ShellToolPreference {
    let preference = crate::rebon_config::saved_shell_tool()
        .as_deref()
        .map(rebon_tool::ShellToolPreference::from_wire_or_default)
        .unwrap_or_default();
    rebon_tool::set_shell_tool_preference(preference);
    preference
}

fn tui_config_option_applier() -> Arc<dyn Fn(&str, &str) + Send + Sync> {
    Arc::new(move |config_id, value| {
        if config_id == "permissions" {
            if let Err(err) = crate::rebon_config::save_default_permission_mode_wire(value) {
                tracing::warn!(error = %err, mode = value, "failed to persist permission mode from TUI config option");
            }
        }
        if config_id == "model" {
            if let Err(err) = crate::rebon_config::persist_model_config_choice(value) {
                tracing::warn!(error = %err, model = value, "failed to persist model from TUI config option");
            }
        }
        if config_id == "claude_codex_fallback" {
            if let Err(err) = crate::rebon_config::save_claude_codex_fallback_enabled_in_dir(
                &config_home_dir(),
                value == "on",
            ) {
                tracing::warn!(error = %err, "failed to persist Claude/Codex fallback from TUI config option");
            }
        }
        if config_id == "shell_tool" {
            apply_shell_tool_choice(value);
        }
    })
}

/// Apply a shell-tool choice made in a settings surface: flip the
/// process-global preference so the next turn's tool list reflects it, then
/// persist it so the choice survives a restart.
pub fn apply_shell_tool_choice(value: &str) {
    let preference = rebon_tool::ShellToolPreference::from_wire_or_default(value);
    rebon_tool::set_shell_tool_preference(preference);
    if let Err(err) =
        crate::rebon_config::save_shell_tool_in_dir(&config_home_dir(), preference.as_wire())
    {
        tracing::warn!(error = %err, shell_tool = value, "failed to persist shell tool preference");
    }
}

#[derive(Clone)]
pub struct SharedAutoModeState {
    pub denials: Arc<std::sync::Mutex<AutoModeDenialStore>>,
    pub verdicts: Arc<rebon_permissions::AutoModeVerdictCache>,
}

impl Default for SharedAutoModeState {
    fn default() -> Self {
        Self {
            denials: Arc::new(std::sync::Mutex::new(AutoModeDenialStore::default())),
            verdicts: Arc::new(rebon_permissions::AutoModeVerdictCache::default()),
        }
    }
}

/// What assembling a session produced: the session itself, plus the two
/// things only the assembly could know that a terminal wants to show.
///
/// A surface with no screen takes `session` and drops the rest. The TUI
/// wraps it with the binary's `tui::wiring::into_tui_session`, which fills the
/// view fields from the flags the run started with.
pub struct SessionBuild {
    pub session: crate::EngineSession,
    /// When a resumed transcript was written, so a terminal can say how
    /// stale it is. `None` for a fresh session or an empty transcript.
    pub resumed_at: Option<std::time::SystemTime>,
    /// Notices the assembly itself raised — an unusable `acpAgents` block,
    /// for one — which a terminal shows on boot.
    pub notices: Vec<String>,
}

/// `resolve_remote` is the `declared-agent-source` seat's answer to "what
/// agent id does this host name mean". It is separate from the switch
/// because the two failures are different problems: `None` is the feature
/// being off, while a switch that fails is a host that cannot be reached,
/// and a person reading either message has to be able to tell which they
/// have.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn activate_initial_session_agent<R, F>(
    demanded_remote: Option<&str>,
    preferred_agent: Option<String>,
    resolve_remote: R,
    mut switch_to: F,
) -> anyhow::Result<()>
where
    R: FnOnce(&str) -> Option<String>,
    F: FnMut(&str) -> Result<String, String>,
{
    if let Some(name) = demanded_remote {
        let wanted = resolve_remote(name).ok_or_else(|| {
            anyhow::anyhow!(
                "`--remote {name}` needs the `remote` plugin, and it is disabled; \
                 re-enable it with `plugins.remote.enabled` or drop `--remote`"
            )
        })?;
        switch_to(&wanted).map_err(|err| {
            anyhow::anyhow!("could not start this session on remote `{name}`: {err}")
        })?;
    } else if let Some(wanted) = preferred_agent {
        if let Err(err) = switch_to(&wanted) {
            tracing::warn!(
                agent = %wanted,
                error = %err,
                "rebon: staying on the local engine"
            );
        }
    }
    Ok(())
}

pub async fn build_tui_session(overrides: RuntimeOverride) -> anyhow::Result<SessionBuild> {
    build_tui_session_with_extra_attachment_poller(
        overrides,
        None,
        None,
        rebon_types::AgentCapabilityMode::Normal,
    )
    .await
}

pub(crate) async fn build_tui_session_with_extra_attachment_poller(
    overrides: RuntimeOverride,
    additional_attachment_poller: Option<Arc<dyn rebon_core::query::AttachmentPoller>>,
    shared_auto_mode_state: Option<SharedAutoModeState>,
    capability_mode: rebon_types::AgentCapabilityMode,
) -> anyhow::Result<SessionBuild> {
    build_session_with_runtime_controller(
        overrides,
        additional_attachment_poller,
        shared_auto_mode_state,
        capability_mode,
        None,
        HandedBack::default(),
    )
    .await
}

/// The third-party agent CLIs this session can run on, from both
/// surfaces that declare them: `acpAgents` in config.json and agent
/// definitions whose frontmatter says `runtime: acp`. Their writes go
/// through this session's file-history tracker and their turns into this
/// session's transcript, so `/rewind` and `--resume` keep working when a
/// session is running on one.
fn build_tui_session_agents(
    declared_acp_agents: &[DeclaredAgent],
    agent_switch: &Arc<AgentBackendSwitch>,
    backend: &Arc<dyn AgentBackend>,
    cwd: &str,
    session_id: &str,
    file_history_tracker: &crate::SharedFileHistoryTracker,
    runtime_add_dirs: &[String],
    server_state: &Arc<ServerState>,
    acp_config_error: Option<String>,
    remote_host: Option<&str>,
) -> anyhow::Result<Arc<SessionAgents<AcpAgentBackend>>> {
    // The config error is surfaced twice, not just logged: the
    // config layer refuses a malformed list wholesale rather than
    // dropping one entry, so staying quiet would show a `/agent`
    // list that is silently missing the agent the user just
    // configured. The startup notice (pushed where the list was
    // loaded, above) covers session open; carrying the error into
    // `SessionAgents` keeps `/agent list` honest later, after the
    // notice has scrolled away.
    let declared = declared_acp_agents.to_vec();
    let mut write_roots = vec![std::path::PathBuf::from(cwd)];
    write_roots.extend(runtime_add_dirs.iter().map(std::path::PathBuf::from));
    let transcript_sink = transcript_sink_for_state(server_state);
    #[allow(unused_mut)]
    let mut agents = rebon_harness::agent_assembly::build_session_agents(
        declared,
        agent_switch.clone(),
        backend.clone(),
        projects_root(),
        cwd.to_string(),
        session_id.to_string(),
        Arc::new(file_history_tracker.clone())
            as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
        write_roots,
        // Mirror every journalled row into the in-memory
        // transcript projection. This is what bumps the
        // replay revision — without it, switching back to the
        // local engine can replay a cached history that is
        // missing the ACP turns.
        Some(transcript_sink.clone()),
    )
    .with_config_error(acp_config_error);
    // The embedded kernel loop vendors (`/kernel dsh|pi|opencode`),
    // one backend per vendor the user configured
    // (`kernelPlugins.loops.<vendor>`, with `loopAgent` as the dsh
    // legacy alias). Where the V8 lives is a build fact the spawner
    // hides: kernel-js builds host the isolate in-process, V8-free
    // builds drive the rebon-kernel-host sidecar (located at spawn
    // time — a missing sidecar is a loud prompt-time error carrying
    // the install hint, never a silent absence from this list).
    // Registered host-side under the `kernel:` prefix — never a
    // config `acpAgents` surface.
    {
        let config_dir = config_home_dir();
        for vendor in rebon_plugin_host::loop_host::KNOWN_LOOP_VENDORS {
            let Some(loop_config) =
                rebon_plugin_host::kernel_loop_backend::KernelLoopConfig::for_vendor(
                    &config_dir,
                    vendor.id,
                )
            else {
                continue;
            };
            let backend_id = rebon_plugin_host::loop_host::loop_backend_id(vendor.id);
            let journal = TranscriptJournal::new(
                projects_root(),
                cwd.to_string(),
                session_id.to_string(),
                backend_id.clone(),
            )
            .with_transcript_sink(transcript_sink.clone());
            let journal = Arc::new(journal);
            let loop_backend = rebon_plugin_host::kernel_loop_backend::KernelLoopBackend::new(
                backend_id.clone(),
                kernel_loop_spawner(),
                loop_config,
                journal,
            );
            agents = agents.with_kernel_backend(backend_id, vendor.label, Arc::new(loop_backend));
        }
    }
    let agents = Arc::new(agents);
    // `--remote` is a demand, not a preference: the user named the
    // machine this session is supposed to run on, and silently
    // starting on the local engine would run their prompts against
    // the wrong filesystem. A saved/configured initial agent remains
    // a preference and may warn and stay local when unavailable.
    activate_initial_session_agent(
        remote_host,
        rebon_agent_core::routing::initial_agent(
            &projects_root(),
            cwd,
            session_id,
            crate::rebon_config::active_acp_agent(),
        ),
        |name| {
            rebon_agent_core::declared_source::demanded_agent_id(
                rebon_harness::kernel_bootstrap::process_kernel().context(),
                name,
            )
        },
        |wanted| agents.switch_to(wanted),
    )?;
    Ok(agents)
}

/// Mirror a journalled ACP row into the live session record, so the
/// in-memory transcript projection sees the same turns the journal
/// wrote.
fn transcript_sink_for_state(server_state: &Arc<ServerState>) -> TranscriptSink {
    let state = server_state.clone();
    Arc::new(move |session_id: &str, entry: TranscriptEntry| {
        if !state.push_transcript_entries(session_id, vec![entry]) {
            tracing::debug!(
                session_id,
                "rebon: no live session record for a journalled ACP row"
            );
        }
    }) as TranscriptSink
}

/// The session this build is for: resumed from disk, or minted here.
///
/// A resume also decides where the transcript lives (a duplicate id in
/// another project is an error, not a guess) and takes the session's
/// active lock, moving the transcript to this cwd when the two differ.
/// A mirror takes no lock and moves nothing: the worker owns both.
struct SessionRecordBinding {
    session_id: String,
    loaded_transcript: Vec<TranscriptEntry>,
    /// When the transcript was created, for the "N days old" line a
    /// terminal draws. `None` for a transcript with nothing in it.
    resumed_at: Option<std::time::SystemTime>,
    session_active_lock: Option<SessionActiveLock>,
    /// The cwd the session record settled on, which a resume can change.
    cwd: String,
}

fn bind_session_record(
    startup_cwd: String,
    resume_session_id: Option<&str>,
    mirror: bool,
    tui_server_state: &Arc<ServerState>,
    startup_permission_mode: Option<PermissionMode>,
    handed_back_session_lock: Option<HeldSessionLock>,
) -> anyhow::Result<SessionRecordBinding> {
    let mut cwd = startup_cwd;
    let (session_id, loaded_transcript, resumed_at, session_active_lock) = if let Some(resume_id) =
        resume_session_id
    {
        let projects_root = projects_root();
        // A mirror opens the transcript where its owner keeps it — for a
        // session born in this terminal, right here — and skips the
        // search across every project and job a resume makes to tell
        // duplicates apart. That search is most of a mirror's build.
        let transcript_here = mirror
            && rebon_session::ensure_session_file_path(&projects_root, &cwd, resume_id)
                .map(|path| path.is_file())
                .unwrap_or(false);
        let transcript_cwd = if transcript_here {
            cwd.clone()
        } else {
            match crate::resume_resolution::resolve_resume_transcript_cwd(
                &projects_root,
                &cwd,
                resume_id,
            ) {
                crate::resume_resolution::ResumeTranscriptResolution::Missing => cwd.clone(),
                crate::resume_resolution::ResumeTranscriptResolution::Unique(transcript_cwd) => {
                    transcript_cwd
                }
                crate::resume_resolution::ResumeTranscriptResolution::Ambiguous => {
                    anyhow::bail!(
                            "cannot resume session {resume_id}: multiple distinct transcripts were found"
                        );
                }
            }
        };
        // A lock this process already holds for this session is reused
        // rather than taken again: the registry inside
        // `try_acquire_session_active_lock` would answer "held" to its own
        // holder, and a worker resuming its own session between turns is
        // exactly that caller.
        //
        // A mirror takes none, and leaves the transcript where it is: the
        // worker owns both.
        let active_lock = if mirror {
            None
        } else {
            let held_lock = HeldSessionLock::take_for(handed_back_session_lock, resume_id);
            let source_lock = match held_lock {
                    Some(lock) => lock,
                    None => rebon_session::try_acquire_session_active_lock(
                        &projects_root,
                        &transcript_cwd,
                        resume_id,
                    )?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "session {resume_id} is still active; resume only supports stopped sessions"
                        )
                    })?,
                };
            if rebon_session::same_cwd(&transcript_cwd, &cwd) {
                Some(source_lock)
            } else {
                let target_lock = rebon_session::try_acquire_session_active_lock(
                        &projects_root,
                        &cwd,
                        resume_id,
                    )?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "session {resume_id} is still active; resume only supports stopped sessions"
                        )
                    })?;
                rebon_session::move_session_to_cwd(
                    &projects_root,
                    &transcript_cwd,
                    &cwd,
                    resume_id,
                )
                .map_err(|e| {
                    anyhow::anyhow!("failed to move transcript for session {resume_id}: {e}")
                })?;
                drop(source_lock);
                Some(target_lock)
            }
        };
        let record = load_resumed_session_record(
            tui_server_state,
            &projects_root,
            resume_id,
            &cwd,
            startup_permission_mode,
        )?;
        cwd = record.cwd.clone();
        // A transcript with nothing in it has no age: it was created just
        // now, however the epoch reads.
        // The facts, not the sentence: a terminal turns these into the
        // "this transcript is N days old" line, and a surface without one
        // has no use for it.
        let resumed_at = if record.loaded_transcript.is_empty() {
            None
        } else {
            Some(record.created_at)
        };
        let transcript = record.loaded_transcript.clone();
        tracing::info!(
            session_id = %record.id,
            transcript_len = transcript.len(),
            "rebon: resumed session from disk"
        );
        (record.id, transcript, resumed_at, active_lock)
    } else {
        let record = tui_server_state.create_session_with_permission_mode(
            cwd.clone(),
            Vec::new(),
            startup_permission_mode
                .unwrap_or(PermissionMode::Default)
                .as_wire(),
        );
        let active_lock = if mirror {
            None
        } else {
            Some(
                rebon_session::try_acquire_session_active_lock(&projects_root(), &cwd, &record.id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "session {} is still active; resume only supports stopped sessions",
                            record.id
                        )
                    })?,
            )
        };
        (record.id, Vec::new(), None, active_lock)
    };
    Ok(SessionRecordBinding {
        session_id,
        loaded_transcript,
        resumed_at,
        session_active_lock,
        cwd,
    })
}

/// Load a transcript back into a record that runs in the mode this session
/// starts in.
///
/// `load_session` has no mode to give the record and says `default`, while
/// the gate's cell starts in `startup_permission_mode`. The two readers then
/// disagree: a background job, which resumes its own session every turn,
/// enforced `plan` while the plan-mode reminders — which read the record —
/// said nothing, so the model went on editing and every edit prompted. The
/// write goes through `set_permission_mode`, as a mode set on a live session
/// does, so the plan-mode bookkeeping sees the session enter the mode.
fn load_resumed_session_record(
    state: &ServerState,
    projects_root: &Path,
    resume_id: &str,
    cwd: &str,
    startup_permission_mode: Option<PermissionMode>,
) -> anyhow::Result<rebon_acp::SessionRecord> {
    let record = state
        .load_session(projects_root, resume_id, cwd, Some(cwd), Vec::new())
        .map_err(|e| anyhow::anyhow!("failed to resume session {resume_id}: {}", e.message))?;
    let mode = startup_permission_mode.unwrap_or(PermissionMode::Default);
    state.set_permission_mode(&record.id, mode.as_wire());
    Ok(record)
}

/// Every third-party agent this session may reach, and the warm pool
/// that runs the ones addressed as `<agentId>:<model>` sub-agents.
struct DeclaredAgentSet {
    declared_acp_agents: Vec<DeclaredAgent>,
    /// A malformed `acpAgents` list is refused wholesale, so the reason
    /// travels with the (empty) list rather than only being logged.
    acp_config_error: Option<String>,
    local_declared_agents: Vec<DeclaredAgent>,
    acp_subagent_pool: Option<Arc<crate::acp_subagent_pool::AcpSubAgentPool>>,
}

fn resolve_declared_agents(
    plugin_acp_agents: &[crate::plugin::acp_agent_manifest::PluginAcpAgentContribution],
    agent_registry: &rebon_tool::AgentRegistry,
    remote_host: Option<&str>,
    remote_path: Option<&str>,
    startup_cwd: &Path,
    runtime_add_dirs: &[String],
    worker_file_history_tracker: &crate::DeferredFileHistoryTracker,
    overrides_startup_notices: &mut Vec<String>,
) -> DeclaredAgentSet {
    // Declared ACP agents, resolved once: the sub-agent pool here and
    // the session-level `/agent` surface further down must agree on
    // what an id means, so both consume this one list.
    let (acp_configured_agents, acp_config_error) = match crate::rebon_config::load_acp_agents() {
        Ok(agents) => (agents, None),
        Err(err) => {
            tracing::warn!(error = %err, "rebon: ignoring an unusable `acpAgents` config");
            overrides_startup_notices
                .push(format!("`acpAgents` in config.json was ignored: {err}"));
            (Vec::new(), Some(err.to_string()))
        }
    };
    let mut declared_acp_agents = rebon_harness::agent_assembly::declared_agents(
        &acp_configured_agents,
        plugin_acp_agents,
        agent_registry,
    );
    // Whatever the `declared-agent-source` seat declares joins the same
    // list — today the `remote` plugin's ssh hosts. Their ids carry a
    // `remote:` prefix that the other surfaces cannot spell (agent names
    // reject `:`), so they extend the list rather than competing for names
    // in it — and `/agent`, the agent list, and the session sidecar all
    // work on a remote without knowing what ssh is. With the plugin off
    // there is no source and the list is unchanged.
    declared_acp_agents.extend(rebon_agent_core::declared_source::declared_agents(
        rebon_harness::kernel_bootstrap::process_kernel().context(),
        remote_host,
        remote_path,
    ));
    let declared_acp_agents = declared_acp_agents;
    // Resident backends for `<agentId>:<model>` sub-agent specs: one
    // process per agent kept warm across tasks, a fresh agent session
    // per task. Routed writes snapshot into the same deferred tracker
    // the local worker path uses, so `/rewind` covers them.
    // Remotes are deliberately not offered to the sub-agent pool. The
    // pool routes a sub-agent's writes into this session's file
    // history against local write roots, which is exactly the wiring a
    // remote must not have — and `<agentId>:<model>` on a remote would
    // name a model resolved by the far end's config, not this one's.
    // Running sub-agents remotely is a feature, not a side effect of
    // sharing the declaration list.
    let local_declared_agents: Vec<_> = declared_acp_agents
        .iter()
        .filter(|agent| agent.workspace_cwd.is_none())
        .cloned()
        .collect();
    let acp_subagent_pool = (!local_declared_agents.is_empty()).then(|| {
        crate::acp_subagent_pool::pool_from_declared(
            local_declared_agents.clone(),
            startup_cwd.to_string_lossy().into_owned(),
            runtime_add_dirs
                .iter()
                .map(std::path::PathBuf::from)
                .collect(),
            Some(Arc::new(worker_file_history_tracker.clone())
                as Arc<
                    dyn rebon_agent_core::file_history::FileHistoryTracker,
                >),
        )
    });
    DeclaredAgentSet {
        declared_acp_agents,
        acp_config_error,
        local_declared_agents,
        acp_subagent_pool,
    }
}

/// The MCP stack this session serves, or `None` for a mirror: a mirror
/// hosts no servers because the worker's are the session's,
/// and a stack lent to one is let go rather than
/// parked in a field nothing reads.
async fn attach_session_mcp(
    handed_back_mcp: Option<SessionMcp>,
    mirror: bool,
    startup_cwd: &Path,
    allowed_channels: &[rebon_plugin_mcp::runtime::ChannelEntry],
    runtime_mcp_configs: &[String],
    runtime_strict_mcp_config: bool,
    plugin_mcp_configs: &[crate::mcp_config::PluginMcpConfig],
    startup_started: std::time::Instant,
) -> Option<SessionMcp> {
    // 6. MCP runtime — optional, populated from `config.json#mcpServers`,
    //    REBON_MCP_SERVERS_JSON, and approved project .mcp.json servers.
    if !allowed_channels.is_empty() {
        tracing::info!(
            channels = ?allowed_channels.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "rebon: loaded MCP channel session opt-in entries"
        );
    }
    // A session opened for a job is another process's; this one shows it.
    // A mirror takes no lock — the worker holds it — and hosts no MCP
    // servers — the worker's are the session's. Nothing
    // else about the build changes: the engine half stays, because `/new`
    // and a local resume still need it, and because the runner cannot run
    // without one yet.
    let mcp = if mirror {
        // A stack lent to a mirror has nobody to serve; let it go rather
        // than park it in a field nothing reads.
        if let Some(stale) = handed_back_mcp {
            stale.shutdown().await;
        }
        None
    } else {
        Some(
            SessionMcp::take_or_spawn(
                handed_back_mcp,
                McpLoadRequest {
                    cwd: startup_cwd.to_path_buf(),
                    allowed_channels: allowed_channels.to_vec(),
                    mcp_configs: runtime_mcp_configs.to_vec(),
                    strict_mcp_config: runtime_strict_mcp_config,
                    plugin_mcp_configs: plugin_mcp_configs.to_vec(),
                },
            )
            .await,
        )
    };
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        mcp_channels = allowed_channels.len(),
        cli_mcp_configs = runtime_mcp_configs.len(),
        plugin_mcp_configs = plugin_mcp_configs.len(),
        mcp_load_status = ?mcp.as_ref().map(|mcp| &mcp.load_status),
        mirror,
        "rebon startup: tui mcp stack attached"
    );
    mcp
}

/// What the Settings dialog has to show for this session before anyone
/// opens it: each row's `current_value` is seeded from the persisted
/// toggle the build already read.
struct TuiHandlerSeeds {
    sub_agents: bool,
    shell_tool: rebon_tool::ShellToolPreference,
    claude_codex_fallback: bool,
    fast_mode: bool,
    permission_mode: Option<PermissionMode>,
    model: String,
    startup_started: std::time::Instant,
}

fn build_tui_handler(seeds: TuiHandlerSeeds) -> DefaultHandler {
    // 7. Build the handler first so we can borrow its shared
    //    ServerState, then build the executor with a clone of the
    //    same state (matches `run_acp_server` — session/prompt
    //    replay needs the executor and handler to share the state
    //    map). The TUI path additionally keeps its own clone of the state to
    //    mint the TUI session id via `create_session` below.
    let handler = DefaultHandler::default()
        .with_filter_active_sessions(true)
        .with_config_option_applier(tui_config_option_applier());
    tracing::info!(
        elapsed_ms = seeds.startup_started.elapsed().as_millis() as u64,
        "rebon startup: acp handler built"
    );
    // Reflect the persisted toggle in the Settings-dialog option list
    // so the row shows the correct `current_value` when opened.
    handler.seed_config_option_value("sub_agents", if seeds.sub_agents { "on" } else { "off" });
    handler.seed_config_option_value("shell_tool", seeds.shell_tool.as_wire());
    handler.seed_config_option_value(
        "claude_codex_fallback",
        if seeds.claude_codex_fallback {
            "on"
        } else {
            "off"
        },
    );
    handler.seed_config_option_value("fast_mode", if seeds.fast_mode { "on" } else { "off" });
    if let Some(default_permission_mode) = seeds.permission_mode {
        handler.seed_config_option_value("permissions", default_permission_mode.as_wire());
    }
    handler.seed_config_option_value("model", &seeds.model);
    let update_auto_install = crate::rebon_config::load_update_preferences()
        .map(|prefs| prefs.auto_install)
        .unwrap_or(false);
    handler.seed_config_option_value(
        "update_auto_install",
        if update_auto_install { "on" } else { "off" },
    );
    handler
}

/// Everything the session is wired from before it has an identity: the
/// plugin contributions, the coordinator mode this run settles on, and
/// the engine, model runtime and runtime-switchable handles the harness
/// assembled.
struct SessionWiring {
    startup_cwd: PathBuf,
    plugin_runtime: crate::plugin::runtime::PluginRuntimeContributions,
    startup_coordinator_mode: bool,
    coordinator_use_worktree: bool,
    engine: Arc<Engine>,
    /// The session's model and the handles `/model` and `/provider` write
    /// through, already in the shape the session carries them.
    model: SessionModel,
    client: Arc<dyn ModelClient>,
    retry_notifier: RetryNotifier,
    model_profiles: ModelProfileMap,
    system_prompt_snapshot: SystemPromptSnapshot,
    service_tier_enabled: bool,
    sub_agent_model_config: SubAgentModelConfig,
    sub_agent_model_router: Arc<dyn AgentModelRouter>,
    provider_runtimes: Arc<ProviderRuntimeCache<RuntimeModel>>,
    live_policy_store: PolicyStore,
    agent_registry: Arc<AgentRegistry>,
    /// The filter the session is built with; the handle beside it is what
    /// `/ceo` swaps on the running executor.
    startup_session_filter: ToolFilter,
    session_filter_handle: SharedToolFilter,
    subagent_filter_handle: SharedToolFilter,
    coordinator_mode_handle: SharedCoordinatorMode,
}

/// Resolve the plugin contributions, settle the coordinator mode, and run
/// the harness assembly. Nothing here knows which session this will be.
async fn resolve_session_wiring(
    overrides: &RuntimeOverride,
    startup_started: std::time::Instant,
) -> anyhow::Result<(SessionWiring, RuntimeResolved)> {
    let startup_cwd = runtime_cwd_path(&overrides);
    let mut plugin_runtime =
        crate::plugin::resolve_runtime_contributions(&crate::plugin::PluginRuntimeOptions {
            cwd: startup_cwd.clone(),
            config_home: config_home_dir(),
            plugin_dirs: overrides.plugin_dirs.iter().map(PathBuf::from).collect(),
            rebon_exe: std::env::current_exe().ok(),
        })?;
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        mcp_configs = plugin_runtime.mcp_configs.len(),
        skill_dirs = plugin_runtime.skill_dirs.len(),
        command_dirs = plugin_runtime.command_dirs.len(),
        agent_dirs = plugin_runtime.agent_dirs.len(),
        hooks = plugin_runtime.hooks.len(),
        warnings = plugin_runtime.warnings.len(),
        "rebon startup: plugin runtime resolved"
    );
    for warning in &plugin_runtime.warnings {
        tracing::warn!(warning = %warning, "rebon startup: plugin warning");
    }
    let mut startup_coordinator_mode = coordinator_mode_from_env_default();
    let coordinator_use_worktree = crate::rebon_config::saved_coordinator_use_worktree();
    if let Some(resume_id) = overrides.resume.as_deref() {
        if let Some(mode) = rebon_session::load_session_mode(
            &projects_root(),
            &startup_cwd.to_string_lossy(),
            resume_id,
        ) {
            let (matched_mode, warning) = match_session_mode(startup_coordinator_mode, Some(&mode));
            startup_coordinator_mode = matched_mode;
            if let Some(warning) = warning {
                tracing::info!(
                    session_id = %resume_id,
                    mode = %mode,
                    warning = %warning,
                    "rebon: matched coordinator mode for resumed session before wiring"
                );
            }
        }
    }

    // 1-3. Policy store, tool filter, merged agent registry and the engine.
    //
    // One implementation, in the harness: the ACP server and the headless
    // harness assemble the same four things in the same order, and the order
    // is load-bearing -- the registry has to reach its process-wide cell
    // before the engine lists its tools, or the first turn describes the
    // compiled-in built-ins only. `fix_tools()` is that constraint as a type.
    //
    // What stays here is what only this surface knows: the coordinator mode
    // it derived from the environment and any resumed session, and the
    // runtime-switchable handles `/ceo` and `/settings` write to.
    let assembled = rebon_harness::session_assembly::SessionAssembly::begin(
        rebon_harness::HarnessOverrides {
            provider: overrides.provider.clone(),
            model: overrides.model.clone(),
            cwd: overrides.cwd.clone(),
            fast_mode: overrides.fast_mode,
            coordinator_mode: startup_coordinator_mode,
            plugin_model_providers: std::mem::take(&mut plugin_runtime.model_providers),
            ..Default::default()
        },
        rebon_harness::session_assembly::AssemblyInputs {
            policy_loading: rebon_harness::session_assembly::PolicyLoading::SessionCwd,
            queue_session: overrides.queue_session,
            agent_dirs: &plugin_runtime.agent_dirs,
            external_agents_spawnable: true,
            coordinator_use_worktree,
        },
    )?
    .fix_tools()
    .resolve_runtime()
    .await?;
    let (assembly, engine, runtime, runtime_resolved) = assembled.into_parts();
    let rebon_harness::session_assembly::SessionAssembly {
        policy_store: live_policy_store,
        tool_filter,
        agent_registry,
        ..
    } = assembly;
    let live_policy_store = live_policy_store.expect("TUI assembly loads session policy");
    // The startup value and the live handle are the same filter: `/ceo` swaps
    // the handle on the running executor without rebuilding the session, while
    // the startup copy is what the session is built with. A clone of the
    // sub-agent handle is plumbed into the spawner too, so coordinator-mode
    // workers get the matching async-agent filter at spawn time even when the
    // toggle fires after boot.
    let startup_session_filter = tool_filter.clone();
    let session_filter_handle = SharedToolFilter::new(tool_filter);
    let startup_subagent_filter = default_subagent_filter(startup_coordinator_mode);
    let subagent_filter_handle = SharedToolFilter::new(startup_subagent_filter.clone());
    let coordinator_mode_handle = SharedCoordinatorMode::new(startup_coordinator_mode);
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        tools = engine.tool_count(),
        "rebon startup: agent registry and tool engine ready"
    );

    let client = runtime.client;
    let retry_notifier = runtime.retry_notifier;
    let model_profiles = runtime.model_profiles;
    let system_prompt_snapshot = runtime.system_prompt_snapshot;
    let service_tier_enabled = runtime.service_tier.is_fast();
    // The session's model, in the shape the session carries it. `/model`
    // and `/provider` write through these same handles, so it is built
    // once here rather than assembled again at the end of the wiring.
    let model = SessionModel {
        provider_name: runtime.provider_name,
        provider_format: runtime.provider_format,
        name: runtime.model.clone(),
        default_name: runtime.model,
        title_name: runtime.title_model,
        prune_level: runtime.prune_level,
        service_tier: runtime.service_tier,
        service_tier_available: runtime.service_tier_available,
        runtime_model: runtime.runtime_model,
    };
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        provider = %model.provider_name,
        model = %model.name,
        provider_format = ?model.provider_format,
        "rebon startup: runtime model resolved"
    );

    let sub_agent_model_config = crate::rebon_config::saved_sub_agent_model_config();
    // The cache comes back too: `/provider reconnect` drops what is in it, and
    // it is otherwise several `Arc<dyn …>` layers inside the router.
    let (sub_agent_model_router, provider_runtimes) = model_router_and_cache_for_runtime(
        model.runtime_model.clone(),
        model.service_tier.clone(),
        sub_agent_model_config.clone(),
    );

    Ok((
        SessionWiring {
            startup_cwd,
            plugin_runtime,
            startup_coordinator_mode,
            coordinator_use_worktree,
            engine,
            model,
            client,
            retry_notifier,
            model_profiles,
            system_prompt_snapshot,
            service_tier_enabled,
            sub_agent_model_config,
            sub_agent_model_router,
            provider_runtimes,
            live_policy_store,
            agent_registry,
            startup_session_filter,
            session_filter_handle,
            subagent_filter_handle,
            coordinator_mode_handle,
        },
        runtime_resolved,
    ))
}

/// What binding this session to the kernel needs. A request struct
/// rather than twenty parameters, matching `SubAgentSpawnerRequest` and
/// `TeamManagerRequest` beside it -- most of these are the same
/// sub-agent resources those two already take.
struct SessionKernelRequest<'a> {
    runtime_resolved: RuntimeResolved,
    engine: &'a Arc<Engine>,
    runtime_model: SharedRuntimeModel,
    session_id: &'a str,
    cwd: &'a str,
    existing_kernel_scopes: Option<HeldKernelScopes>,
    task_runtime_controller: Option<Arc<dyn TaskRuntimeController>>,
    client: Arc<dyn ModelClient>,
    default_model: String,
    sub_agent_model_config: SubAgentModelConfig,
    model_profiles: ModelProfileMap,
    sub_agent_model_router: Arc<dyn AgentModelRouter>,
    subagent_filter_handle: SharedToolFilter,
    coordinator_mode_handle: SharedCoordinatorMode,
    coordinator_use_worktree: bool,
    startup_coordinator_mode: bool,
    plugin_hooks: Vec<rebon_hooks::IndividualHookConfig>,
    worker_file_history_tracker: &'a crate::DeferredFileHistoryTracker,
    acp_subagent_pool: Option<Arc<crate::acp_subagent_pool::AcpSubAgentPool>>,
    user_attachment_poller: Arc<dyn AttachmentPoller>,
    notices: &'a mut Vec<String>,
}

/// What the binding leaves behind: the scope table, the task registry
/// hanging off it, and everything built against that registry -- the
/// team manager, the sub-agent spawner, the notification poller, and
/// this session's file-history store.
struct SessionKernelBinding {
    bound: SessionBound,
    task_registry_resolver: rebon_plugin_tasks::TaskRegistryResolver,
    tasks: Arc<TaskRegistry>,
    task_runtime_controller: Arc<dyn TaskRuntimeController>,
    team_manager: Option<Arc<dyn TeamManager>>,
    policy: rebon_core::policy_seat::PolicySources,
    spawner: Arc<dyn SubAgentSpawner>,
    task_notification_poller: Arc<crate::task_notification_poller::TaskNotificationPoller>,
    extra_attachment_poller: Arc<dyn AttachmentPoller>,
    file_history_tracker: crate::SharedFileHistoryTracker,
}

async fn bind_session_kernel(req: SessionKernelRequest<'_>) -> SessionKernelBinding {
    // Kernel session wiring, matching ACP and headless: the configured
    // composition boots once per process (idempotent, config-driven, and a
    // no-op when nothing is configured), and installs the process-wide
    // system-prompt sections and web seat.
    //
    // One bounded scope table backs all TUI turns. `for_session` acquires an
    // exact generation lease, so `/new`, resume, attach, and detached turns do
    // not dispose or retarget a scope that an older turn still owns. The table
    // also owns the one plugin-tool registry handed to this executor.
    // A configured composition that did not start is shown, not logged: the
    // person asked for kernel plugins and would otherwise get a session that
    // silently lacks them. The startup gate already offers the fix for the
    // common cause, so what usually reaches here is the other kind.
    let existing_kernel_scopes =
        HeldKernelScopes::take_for(req.existing_kernel_scopes, Some(req.session_id));
    let (bound, composition_refusal) = req
        .runtime_resolved
        .bind_session(
            req.engine,
            req.runtime_model,
            req.session_id.to_string(),
            existing_kernel_scopes,
        )
        .await;
    if let Some(refusal) = composition_refusal {
        req.notices.push(refusal.message());
    }

    let task_registry_resolver = bound.kernel_scopes.task_registry_resolver();
    // The host/UI Arc retains state while the feature is disabled. Runtime
    // consumers below still resolve the typed seat on each operation.
    let tasks = bound.kernel_scopes.host_task_registry(req.session_id);
    let task_runtime_controller = req.task_runtime_controller.unwrap_or_else(|| {
        Arc::new(TaskRegistryRuntimeController::new(
            task_registry_resolver.clone(),
        )) as Arc<dyn TaskRuntimeController>
    });
    let team_manager = team_manager_for_session(TeamManagerRequest {
        engine: SubAgentRuntimeHandle::new(req.engine.clone()),
        task_runtime: SubAgentRuntimeHandle::new(task_registry_resolver.clone()),
        client: req.client.clone(),
        default_model: req.default_model.clone(),
        model_config: req.sub_agent_model_config.clone(),
        model_profiles: req.model_profiles.clone(),
        model_router: req.sub_agent_model_router.clone(),
        base_filter: Some(req.subagent_filter_handle.clone()),
    });
    if req.startup_coordinator_mode {
        tracing::info!(
            "rebon: coordinator mode active — applying async_agent_filter to sub-agents (tui)"
        );
    }
    // Before the spawner: a sub-agent inherits this session's subscribers,
    // so the handle has to exist by the time the spawner is built.
    let policy = bound.build_policy_sources(req.cwd, req.plugin_hooks);
    let spawner = sub_agent_spawner_for_session(SubAgentSpawnerRequest {
        engine: SubAgentRuntimeHandle::new(Arc::downgrade(req.engine)),
        task_runtime: Some(SubAgentRuntimeHandle::new(task_registry_resolver.clone())),
        policy: Some(SubAgentRuntimeHandle::new(policy.clone())),
        client: req.client,
        default_model: req.default_model,
        model_config: req.sub_agent_model_config,
        model_profiles: req.model_profiles,
        model_router: req.sub_agent_model_router,
        base_filter: Some(req.subagent_filter_handle),
        coordinator_mode: Some(req.coordinator_mode_handle),
        coordinator_use_worktree: req.coordinator_use_worktree,
        file_history_tracker: Some(Arc::new(req.worker_file_history_tracker.clone())
            as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>),
        external_runner: req
            .acp_subagent_pool
            .map(|pool| pool as Arc<dyn ExternalSubAgentRunner>),
    })
    .unwrap_or_else(|| Arc::new(UnavailableSubAgentSpawner));
    let task_notification_poller =
        crate::task_notification_poller::TaskNotificationPoller::new_session_resolving(
            task_registry_resolver.clone(),
            tasks.subscribe_notification_revision(),
        );
    let extra_attachment_poller: Arc<dyn AttachmentPoller> = Arc::new(CompositePoller::new(
        req.user_attachment_poller,
        task_notification_poller.clone(),
    ));

    let file_history_tracker =
        crate::SharedFileHistoryTracker::new(rebon_session::FileHistoryStore::new(
            projects_root(),
            std::path::PathBuf::from(req.cwd),
            req.session_id,
        ));
    // Bind the deferred handle the sub-agent spawner already holds so
    // worker Write/Edit snapshots land in this session's store.
    req.worker_file_history_tracker
        .bind(file_history_tracker.clone());

    SessionKernelBinding {
        bound,
        task_registry_resolver,
        tasks,
        task_runtime_controller,
        team_manager,
        policy,
        spawner,
        task_notification_poller,
        extra_attachment_poller,
        file_history_tracker,
    }
}

/// The four records this session is: the engine core, the agent switch
/// and the routing table beside it, the runtime plus the factory that
/// rebuilds it, and the `SessionBuild` the caller gets back.
///
/// They read the same values, which is why the phases above hand them
/// over as one `SessionAssemblyParts` instead of as sixty parameters.
/// Nothing here resolves anything: every field was decided by a phase
/// with a name, and this is where they are written down as records.
struct SessionAssemblyParts {
    wiring: SessionWiring,
    declared: DeclaredAgentSet,
    record: SessionRecordBinding,
    kernel: SessionKernelBinding,
    startup_started: std::time::Instant,
    capability_mode: AgentCapabilityMode,
    mirror: bool,
    startup: SessionStartupParams,
    startup_permission_mode: Option<PermissionMode>,
    shared_auto_mode_state: Option<SharedAutoModeState>,
    engine_for_tasks: Arc<Engine>,
    client_for_tasks: Arc<dyn ModelClient>,
    default_model_for_tasks: String,
    attached_background_job_id: Option<String>,
    remote_host: Option<String>,
    handler: DefaultHandler,
    tui_server_state: Arc<ServerState>,
    executor_server_state: Arc<ServerState>,
    mcp: Option<SessionMcp>,
    mcp_client: Arc<dyn McpClient>,
    skill_registry: Arc<SkillRegistry>,
    skill_state: Arc<std::sync::Mutex<rebon_plugin_skill::SkillState>>,
    skill_loader_template: SkillLoaderConfig,
    cron_poller: Arc<CronPoller>,
    session_cron_store: Arc<SessionCronStore>,
    mid_turn_queue: Arc<crate::mid_turn_queue::MidTurnQueuedSubmitPoller>,
    additional_attachment_poller: Option<Arc<dyn AttachmentPoller>>,
    overrides_startup_notices: Vec<String>,
}

/// The records the first half built, handed to the second.
struct SessionRecords {
    engine_core: Arc<DeferredEngineCore>,
    backend: Arc<dyn AgentBackend>,
    initial_runtime: Arc<SessionRuntime>,
    runtime_factory: Arc<SessionRuntimeFactory>,
    tui_update_publisher: Arc<dyn SessionUpdatePublisher>,
    update_rx: tokio::sync::mpsc::UnboundedReceiver<rebon_types::SessionUpdateParams>,
    permission_rx:
        tokio::sync::mpsc::UnboundedReceiver<rebon_core::permission::OutboundPermissionQuery>,
    auto_mode_denials: Arc<std::sync::Mutex<AutoModeDenialStore>>,
    auto_mode_verdicts: Arc<AutoModeVerdictCache>,
    permission_mode_cell: Arc<std::sync::Mutex<PermissionMode>>,
    startup_started: std::time::Instant,
    mirror: bool,
    model: SessionModel,
    startup: SessionStartupParams,
    engine: Arc<Engine>,
    engine_for_tasks: Arc<Engine>,
    client_for_tasks: Arc<dyn ModelClient>,
    retry_notifier: RetryNotifier,
    system_prompt_snapshot: SystemPromptSnapshot,
    session_filter_handle: SharedToolFilter,
    subagent_filter_handle: SharedToolFilter,
    coordinator_mode_handle: SharedCoordinatorMode,
    live_policy_store: PolicyStore,
    agent_registry: Arc<AgentRegistry>,
    session_id: String,
    cwd: String,
    loaded_transcript: Vec<TranscriptEntry>,
    resumed_at: Option<std::time::SystemTime>,
    session_active_lock: Option<SessionActiveLock>,
    attached_background_job_id: Option<String>,
    handler: DefaultHandler,
    tui_server_state: Arc<ServerState>,
    mcp: Option<SessionMcp>,
    cron_poller: Arc<CronPoller>,
    session_cron_store: Arc<SessionCronStore>,
    bound: SessionBound,
    tasks: Arc<TaskRegistry>,
    task_notification_poller: Arc<crate::task_notification_poller::TaskNotificationPoller>,
    team_manager: Option<Arc<dyn TeamManager>>,
    overrides_startup_notices: Vec<String>,
}

/// Wire the handler to the installed runtime, start this session's cron
/// scheduler, and write down the `SessionBuild` the caller gets.
fn finish_session_build(records: SessionRecords) -> anyhow::Result<SessionBuild> {
    let SessionRecords {
        engine_core,
        backend,
        initial_runtime,
        runtime_factory,
        tui_update_publisher,
        update_rx,
        permission_rx,
        auto_mode_denials,
        auto_mode_verdicts,
        permission_mode_cell,
        startup_started,
        mirror,
        model,
        startup,
        engine,
        engine_for_tasks,
        client_for_tasks,
        retry_notifier,
        system_prompt_snapshot,
        session_filter_handle,
        subagent_filter_handle,
        coordinator_mode_handle,
        live_policy_store,
        agent_registry,
        session_id,
        cwd,
        loaded_transcript,
        resumed_at,
        session_active_lock,
        attached_background_job_id,
        handler,
        tui_server_state,
        mcp,
        cron_poller,
        session_cron_store,
        bound,
        tasks,
        task_notification_poller,
        team_manager,
        overrides_startup_notices,
    } = records;

    // The handler is retained for its state/config-option surface, but TUI
    // prompt dispatch always captures the installed runtime Arc directly.
    let handler = handler
        .with_prompt_executor(initial_runtime.executor.clone())
        .with_update_publisher(initial_runtime.update_publisher.clone());

    // Wire rebon-hooks: log the number of supported hook events so
    // the startup diagnostic shows hook system readiness.
    let hook_event_count = rebon_hooks::HOOK_EVENTS.len();

    // MCP servers start Pending and transition via the plugin's status FSM.
    let initial_mcp_status = rebon_plugin_mcp::runtime::status::McpServerStatus::Pending;

    tracing::info!(
        tools = engine.tool_count(),
        session_id = %session_id,
        cwd = %cwd,
        os = std::env::consts::OS,
        hook_event_count,
        initial_mcp_status = ?initial_mcp_status,
        agent_backend = backend.name(),
        agent_backend_kind = backend.kind().as_str(),
        "rebon: built local TUI engine session (skipping ACP stdio loop)"
    );

    // Spawn the cron scheduler rooted at the session cwd. The
    // scheduler owns `<cwd>/.rebon/scheduled_tasks.json` and the
    // cross-process `.scheduler.lock`; only one rebon process per cwd
    // actually fires, the rest sit in the probe loop. The returned
    // handle lives on the `TuiEngineSession` so it drops (and releases
    // the owner lock) when the TUI exits.
    //
    // A mirror starts none. A fired task is a prompt fed to the local query
    // loop, and a mirror never runs one: had it won the lock — a coin toss
    // against the worker starting beside it — every task for this cwd would
    // have been fed to a loop that is not there, and the one-shots deleted
    // as fired. The lock belongs to whichever process runs the session;
    // `TuiEngineSession::start_cron_scheduler` is for the session that
    // becomes this process's after all.
    let cron_scheduler = if mirror {
        None
    } else {
        let config = rebon_core::cron::SchedulerConfig::new(std::path::PathBuf::from(&cwd));
        Some(rebon_core::cron::start_scheduler_with_session_store(
            config,
            cron_poller.clone(),
            Some(session_cron_store.clone()),
        ))
    };
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        session_id = %session_id,
        "rebon startup: tui wiring completed"
    );

    // The gate that decides whether a tool call prompts reads
    // `permission_mode_cell`, not the session record — it runs below the
    // layer that can poll one. Register the cell so every write to the
    // record reaches it, whoever made the write: the TUI already wrote
    // both, but a tool-driven move (`EnterPlanMode` succeeding,
    // `ExitPlanMode` handing a mode back) goes through the record alone,
    // and without this the gate kept enforcing the mode the session had
    // left — a plan the model entered was never actually in plan mode, so
    // its `ExitPlanMode` resolved under the old mode and skipped the
    // approval that is the whole point of the tool.
    handler
        .state()
        .attach_permission_mode_cell(&session_id, permission_mode_cell.clone());

    // Mint the shared background-task registry. Empty at startup —
    // the TUI loop polls it every frame and the moment any future
    // spawn point (async Bash, `/run` slash command, sub-agent
    // queries, remote review sessions) inserts a task, the "/tasks"
    // dialog + footer subtitle pick it up automatically.
    Ok(SessionBuild {
        session: crate::EngineSession {
            engine_half: SessionEngineHalf {
                runtime: initial_runtime.clone(),
                runtime_factory: Some(runtime_factory),
                handler,
                update_publisher: tui_update_publisher,
                engine_core,
                compact_runtime: crate::compact::CompactRuntime::new(),
                projection_invalid: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                update_rx,
                tasks,
                engine: engine_for_tasks,
                sub_agent_spawner: initial_runtime.sub_agent_spawner.clone(),
                skill_registry: initial_runtime.skill_registry.clone(),
                client: client_for_tasks,
                system_prompt_snapshot,
                session_filter_handle,
                coordinator_mode_handle,
                permission_mode_cell,
                cron_scheduler,
                cron_poller,
                session_cron_store,
                task_notification_poller,
                kernel_scopes: bound.kernel_scopes.clone(),
                executor: initial_runtime.executor.clone(),
                mcp,
                permission_rx,
                live_policy_store,
                team_manager,
                agent_registry,
                retry_notifier,
                subagent_filter_handle,
                auto_mode_denials,
                auto_mode_verdicts,
                usage_ledger: Arc::new(std::sync::Mutex::new(crate::usage::UsageLedger::new(
                    rebon_types::wall_clock_ms(),
                ))),
            },
            session_id: initial_runtime.session_id.clone(),
            projects_root: initial_runtime.projects_root.clone(),
            server_state: tui_server_state,
            cwd: initial_runtime.cwd.clone(),
            model,
            loaded_transcript,
            session_active_lock,
            attached_background_job_id,
            session_start_hook: None,
            startup,
        },
        resumed_at,
        notices: overrides_startup_notices,
    })
}

fn assemble_session_build(parts: SessionAssemblyParts) -> anyhow::Result<SessionRecords> {
    let SessionAssemblyParts {
        wiring,
        declared,
        record,
        kernel,
        startup_started,
        capability_mode,
        mirror,
        startup,
        startup_permission_mode,
        shared_auto_mode_state,
        engine_for_tasks,
        client_for_tasks,
        default_model_for_tasks,
        attached_background_job_id,
        remote_host,
        handler,
        tui_server_state,
        executor_server_state,
        mcp,
        mcp_client,
        skill_registry,
        skill_state,
        skill_loader_template,
        cron_poller,
        session_cron_store,
        mid_turn_queue,
        additional_attachment_poller,
        overrides_startup_notices,
    } = parts;
    let SessionWiring {
        startup_cwd,
        plugin_runtime,
        coordinator_use_worktree,
        engine,
        live_policy_store,
        agent_registry,
        startup_session_filter,
        session_filter_handle,
        subagent_filter_handle,
        coordinator_mode_handle,
        model,
        client,
        retry_notifier,
        model_profiles,
        system_prompt_snapshot,
        sub_agent_model_config,
        sub_agent_model_router,
        provider_runtimes,
        ..
    } = wiring;
    let DeclaredAgentSet {
        declared_acp_agents,
        local_declared_agents,
        acp_config_error,
        acp_subagent_pool,
        ..
    } = declared;
    let SessionRecordBinding {
        session_id,
        cwd,
        loaded_transcript,
        resumed_at,
        session_active_lock,
        ..
    } = record;
    let SessionKernelBinding {
        bound,
        tasks,
        task_registry_resolver,
        task_runtime_controller,
        task_notification_poller,
        team_manager,
        spawner,
        policy,
        file_history_tracker,
        extra_attachment_poller,
        ..
    } = kernel;

    // 9c. The typed permission broker (bypasses ACP JSON-RPC).
    //
    // Auto-mode wiring is this surface's own: the TUI owns both the mode cell
    // (mirrored from `AppState::permission_mode` by the cycle-mode handler)
    // and the denial store (visible through the `/permissions` slash command).
    // Both are built now so the broker can short-circuit `Ask→Allow` and
    // record denials as soon as it starts answering tool requests, and they
    // are handed over before the broker is shared -- once it is behind an
    // `Arc` nobody may reach in.
    let shared_auto_mode_state = shared_auto_mode_state.unwrap_or_default();
    let auto_mode_denials = shared_auto_mode_state.denials;
    let auto_mode_verdicts = shared_auto_mode_state.verdicts;
    let permission_mode_cell: Arc<std::sync::Mutex<PermissionMode>> = Arc::new(
        std::sync::Mutex::new(startup_permission_mode.unwrap_or(PermissionMode::Default)),
    );
    let auto_mode_hooks = {
        let sink = SharedDenialSink::new(Arc::clone(&auto_mode_denials));
        let provider_cell = Arc::clone(&permission_mode_cell);
        let provider: Arc<dyn PermissionModeProvider> =
            Arc::new(move || *provider_cell.lock().expect("mode cell poisoned"));
        AutoModeHooks::new(Arc::new(sink), provider).with_verdicts(Arc::clone(&auto_mode_verdicts))
    };
    let (permission_broker, permission_rx) = bound.build_permissions(Some(auto_mode_hooks));

    // 9. The engine core: everything the executor needs is now in hand,
    // collected into one bundle. A session built to run turns builds the
    // core here — same products, same checkpoints as when this was inline.
    // A mirror carries the bundle instead: it shows a session the worker
    // runs, and the ≈250 ms core would serve nobody.
    // The recipe stays reachable through the factory and the executor
    // seam below for the day the session becomes this process's after all.
    let engine_core = Arc::new(DeferredEngineCore::new(EngineCoreInputs {
        startup_started,
        engine: engine.clone(),
        client,
        projects_root: projects_root(),
        model: model.name.clone(),
        capability_mode,
        startup_session_filter,
        system_prompt_snapshot: system_prompt_snapshot.clone(),
        title_model: model.title_name.clone(),
        sub_agent_spawner: spawner.clone(),
        skill_registry: skill_registry.clone(),
        team_manager: team_manager.clone(),
        task_runtime_controller,
        tasks: tasks.clone(),
        task_registry_resolver: task_registry_resolver.clone(),
        live_policy_store: live_policy_store.clone(),
        session_filter_handle: session_filter_handle.clone(),
        coordinator_mode_handle: coordinator_mode_handle.clone(),
        coordinator_use_worktree,
        server_state: executor_server_state,
        prune_level: model.prune_level.clone(),
        cron_poller: cron_poller.clone(),
        extra_attachment_poller,
        session_cron_store: session_cron_store.clone(),
        runtime_model: model.runtime_model.clone(),
        sandbox_cwd: startup_cwd.clone(),
        mcp_client,
        provider_name: model.provider_name.clone(),
        skill_state: skill_state.clone(),
        cwd: cwd.clone(),
        model_profiles: model_profiles.clone(),
        kernel_context_resolver: bound.kernel_scopes.resolver(),
        kernel_plugin_tools: bound.kernel_scopes.plugin_tools()
            as Arc<dyn rebon_tool::PluginToolProvider>,
        file_history_tracker: file_history_tracker.clone(),
        permission_broker: permission_broker.clone(),
        policy: policy.clone(),
    }));
    // The engine is this session's default agent backend. Turns go
    // through a switch rather than straight to the engine executor, so
    // `/agent` can point the session at a third-party CLI without
    // rebuilding everything that already holds the executor.
    let engine_executor: Arc<dyn PromptExecutor> = if mirror {
        Arc::new(DeferredEnginePromptExecutor {
            core: engine_core.clone(),
        })
    } else {
        engine_core.build_at_wiring().executor.clone() as Arc<dyn PromptExecutor>
    };
    let backend: Arc<dyn AgentBackend> = Arc::new(LocalAgentBackend::new(engine_executor));
    let agent_switch = Arc::new(rebon_agent_core::AgentBackendSwitch::new(backend.clone()));
    let executor_arc: Arc<dyn PromptExecutor> = agent_switch.clone();

    // Third-party agent CLIs this session can run on, from both
    // surfaces that declare them: `acpAgents` in config.json and agent
    // definitions whose frontmatter says `runtime: acp`. Their writes
    // go through this session's file-history tracker and their turns
    // into this session's transcript, so `/rewind` and `--resume` keep
    // working when a session is running on one.
    let session_agents = build_tui_session_agents(
        &declared_acp_agents,
        &agent_switch,
        &backend,
        &cwd,
        &session_id,
        &file_history_tracker,
        &startup.add_dirs,
        &tui_server_state,
        acp_config_error.clone(),
        remote_host.as_deref(),
    )?;

    let transcript_sink_for_runtime = transcript_sink_for_state(&tui_server_state);
    let runtime_handle = tokio::runtime::Handle::current();
    let runtime_factory = Arc::new(SessionRuntimeFactory {
        engine_core: engine_core.clone(),
        engine: engine_for_tasks.clone(),
        client: client_for_tasks.clone(),
        default_model: default_model_for_tasks.clone(),
        model_profiles: model_profiles.clone(),
        sub_agent_model_config,
        sub_agent_model_router,
        subagent_filter_handle: subagent_filter_handle.clone(),
        coordinator_mode_handle: coordinator_mode_handle.clone(),
        coordinator_use_worktree,
        kernel_scopes: bound.kernel_scopes.clone(),
        task_registry_resolver: task_registry_resolver.clone(),
        local_declared_agents,
        declared_agents: declared_acp_agents,
        acp_config_error,
        runtime_add_dirs: startup.add_dirs.clone(),
        projects_root: projects_root(),
        transcript_sink: transcript_sink_for_runtime,
        skill_loader: skill_loader_template,
        plugin_hooks: plugin_runtime.hooks.clone(),
        auto_mode_denials: auto_mode_denials.clone(),
        auto_mode_verdicts: auto_mode_verdicts.clone(),
        permission_mode_cell: permission_mode_cell.clone(),
        runtime_model: model.runtime_model.clone(),
        kernel_context_resolver: bound.kernel_scopes.resolver(),
        session_cron_store: session_cron_store.clone(),
        additional_attachment_poller,
        runtime_handle: runtime_handle.clone(),
        provider_runtimes,
    });
    let (tui_update_publisher, update_rx) = ChannelSessionUpdatePublisher::new();
    let tui_update_publisher: Arc<dyn SessionUpdatePublisher> = Arc::new(tui_update_publisher);
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: session runtime factory ready"
    );
    let initial_runtime = Arc::new(SessionRuntime {
        session_id: session_id.clone(),
        projects_root: projects_root(),
        cwd: cwd.clone(),
        executor: executor_arc,
        backend: backend.clone(),
        session_agents,
        acp_subagent_pool,
        sub_agent_spawner: spawner,
        file_history_tracker,
        policy,
        skill_registry,
        skill_state,
        update_publisher: tui_update_publisher.clone(),
        permission_broker,
        mid_turn_queue,
        tasks: tasks.clone(),
        task_notification_poller: task_notification_poller.clone(),
        runtime_handle: Some(runtime_handle.clone()),
    });

    Ok(SessionRecords {
        engine_core,
        backend,
        initial_runtime,
        runtime_factory,
        tui_update_publisher,
        update_rx,
        permission_rx,
        auto_mode_denials,
        auto_mode_verdicts,
        permission_mode_cell,
        startup_started,
        mirror,
        model,
        startup,
        engine,
        engine_for_tasks,
        client_for_tasks,
        retry_notifier,
        system_prompt_snapshot,
        session_filter_handle,
        subagent_filter_handle,
        coordinator_mode_handle,
        live_policy_store,
        agent_registry,
        session_id,
        cwd,
        loaded_transcript,
        resumed_at,
        session_active_lock,
        attached_background_job_id,
        handler,
        tui_server_state,
        mcp,
        cron_poller,
        session_cron_store,
        bound,
        tasks,
        task_notification_poller,
        team_manager,
        overrides_startup_notices,
    })
}

/// Build a session, screen or no screen.
///
/// Named without `tui` on purpose: what comes back is a
/// [`SessionBuild`] holding a [`crate::EngineSession`], which is the
/// UI-neutral handle split out — the terminal wraps it in a
/// the binary's `session_shell::TuiEngineSession` afterwards, and a background worker,
/// which has no screen at all, uses it as it stands. The old name said
/// otherwise and the doc that went with it said it built a `TuiEngineSession`,
/// which had not been true since that split.
///
/// The phases are named functions rather than inline blocks, and the first and
/// fifth of them run inside `rebon_harness::session_assembly::SessionAssembly`:
/// tool fixing, runtime resolution, session binding, permissions and policy
/// sources are the harness's, not the terminal's. What is left here is the
/// surface's own: reading `RuntimeOverride`, the ACP server record, the MCP
/// attach, and assembling the handle.
///
/// It matches `rebon_cli::acp::run_acp_server`'s bootstrap step for step, so the
/// two run modes build behaviourally identical engines and executors; that one
/// then calls `serve_with_publishers(...)` to drive the JSON-RPC loop, while
/// this stops before that call and hands the handler back to the caller.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub async fn build_session_with_runtime_controller(
    overrides: RuntimeOverride,
    additional_attachment_poller: Option<Arc<dyn AttachmentPoller>>,
    shared_auto_mode_state: Option<SharedAutoModeState>,
    capability_mode: AgentCapabilityMode,
    task_runtime_controller: Option<Arc<dyn TaskRuntimeController>>,
    mut handed_back: HandedBack,
) -> anyhow::Result<SessionBuild> {
    let startup_started = std::time::Instant::now();
    tracing::info!("rebon startup: tui wiring started");
    let (wiring, runtime_resolved) = resolve_session_wiring(&overrides, startup_started).await?;
    let startup_cwd_str = wiring.startup_cwd.to_string_lossy().to_string();

    // 4. Model client + default model. Prefer rebon's
    //    `.rebon/config.json` active provider (with OAuth token
    //    refresh) and fall back to env vars when no config dir
    //    exists. CLI `--provider` / `--model` overrides are
    //    applied by `resolve_runtime_model`.
    let resume_session_id = overrides.resume.clone();
    let attached_background_job_id = overrides.attached_background_job_id.clone();
    let mut overrides_startup_notices = overrides.startup_notices.clone();
    let startup_permission_mode = overrides
        .permission_mode
        .or_else(crate::rebon_config::saved_default_permission_mode);
    // The launch arguments a detached or dispatched background session
    // inherits, in the shape the session carries them. Built here rather
    // than at the end so the wiring below reads one value per argument.
    let startup = SessionStartupParams {
        effort_level: overrides.effort_level,
        permission_mode: startup_permission_mode,
        queue_session: overrides.queue_session,
        channels: overrides.channels.clone(),
        development_channels: overrides.development_channels.clone(),
        settings: overrides.settings.clone(),
        add_dirs: overrides.add_dirs.clone(),
        plugin_dirs: overrides.plugin_dirs.clone(),
        mcp_configs: overrides.mcp_configs.clone(),
        strict_mcp_config: overrides.strict_mcp_config,
    };
    let remote_host = overrides.remote.clone();
    let remote_path = overrides.remote_path.clone();
    // Keep dedicated clones for the spawn points on `TuiEngineSession`
    // — independent of the executor so the runner can mint background
    // tasks without reaching through the executor type.
    let engine_for_tasks = wiring.engine.clone();
    let client_for_tasks = wiring.client.clone();
    let default_model_for_tasks = wiring.model.default_name.clone();

    // 5. Sub-agent resources are completed after the exact session scope is
    // bound below. This prevents any consumer from observing an unbound or
    // independently-created task registry.
    //    base filter so sub-agents inherit the coordinator-safe tool
    //    allow list. The filter is handed in as a shared handle so
    //    runtime `/ceo` toggles flip the filter every subsequent spawn
    //    sees — no need to rebuild the spawner.
    //
    //    The worker file-history tracker is deferred: the spawner is
    //    built before the session id (and its real tracker) exist, so
    //    it takes an empty handle now that wiring binds below once the
    //    session tracker is constructed. This makes sub-agent Write/Edit
    //    snapshot into the same store as the main session so `/rewind`
    //    can undo worker changes.
    let worker_file_history_tracker = crate::DeferredFileHistoryTracker::default();

    let declared = resolve_declared_agents(
        &wiring.plugin_runtime.acp_agents,
        &wiring.agent_registry,
        remote_host.as_deref(),
        remote_path.as_deref(),
        &wiring.startup_cwd,
        &startup.add_dirs,
        &worker_file_history_tracker,
        &mut overrides_startup_notices,
    );

    // 6. MCP runtime — optional, populated from `config.json#mcpServers`,
    //    REBON_MCP_SERVERS_JSON, and approved project .mcp.json servers.
    let mirror = attached_background_job_id.is_some();
    let mcp = attach_session_mcp(
        handed_back.mcp.take(),
        mirror,
        &wiring.startup_cwd,
        &startup.channels,
        &startup.mcp_configs,
        startup.strict_mcp_config,
        &wiring.plugin_runtime.mcp_configs,
        startup_started,
    )
    .await;
    let skill_registry = Arc::new(SkillRegistry::new());
    match crate::rebon_config::load_disabled_skills() {
        Ok(disabled_skills) => skill_registry.set_disabled_skills(disabled_skills),
        Err(err) => {
            tracing::warn!(error = %err, "rebon startup: failed to load disabled skills");
        }
    }

    // 6b. Apply the persisted sub-agent toggle *before* snapshotting
    //     tools into the system prompt: `Engine::eager_tool_snapshots`
    //     filters by `Tool::is_enabled()`, so if the user previously
    //     disabled sub-agents we want AgentTool gone from both the
    //     API tool list and the `agent_tool_section` of the system
    //     prompt on this very first turn.
    let persisted_sub_agents = crate::rebon_config::saved_sub_agents_enabled();
    rebon_tool::set_sub_agents_enabled(persisted_sub_agents);
    // Same reason, same timing: `BashTool::is_enabled` /
    // `PowerShellTool::is_enabled` read this, so it has to be in place before
    // the first tool snapshot reaches the system prompt.
    let persisted_shell_tool = apply_persisted_shell_tool();
    let persisted_claude_codex_fallback =
        crate::rebon_config::saved_claude_codex_fallback_enabled();

    let handler = build_tui_handler(TuiHandlerSeeds {
        sub_agents: persisted_sub_agents,
        shell_tool: persisted_shell_tool,
        claude_codex_fallback: persisted_claude_codex_fallback,
        fast_mode: wiring.service_tier_enabled,
        permission_mode: startup_permission_mode,
        model: wiring.model.name.clone(),
        startup_started,
    });
    let tui_server_state = handler.state().clone();
    let executor_server_state = tui_server_state.clone();

    // 8/9. What used to be built here — the system prompt config, the
    // sandbox policy and the prompt executor itself — is the engine core,
    // now built by `build_engine_core` from inputs collected below:
    // immediately for a session that runs turns,
    // lazily (or never) for a mirror.
    //
    // Cron poller: the model-facing queue the scheduler pushes fired
    // prompts into. Handed to the executor's composite attachment poller
    // and cloned into `start_scheduler` below once the cwd is known.
    let cron_poller = CronPoller::new();
    let session_cron_store = SessionCronStore::new();
    let mid_turn_queue = crate::mid_turn_queue::MidTurnQueuedSubmitPoller::new("");
    let mut user_attachment_poller: Arc<dyn AttachmentPoller> = mid_turn_queue.clone();
    if let Some(additional_attachment_poller) = additional_attachment_poller.clone() {
        user_attachment_poller = Arc::new(CompositePoller::new(
            user_attachment_poller,
            additional_attachment_poller,
        ));
    }
    // A mirror's executor gets an empty seam: it never runs a turn here,
    // and the servers behind a real one belong to the owner.
    let mcp_client: Arc<dyn McpClient> = match mcp.as_ref() {
        Some(mcp) => mcp.client.clone(),
        None => Arc::new(DelayedTuiMcpClient::new()),
    };

    // 9b. Progressive skill discovery: load startup skills from
    //     user (~/.rebon/skills) and project (.rebon/skills) dirs.
    let skill_loader_template = SkillLoaderConfig {
        config_home: config_home_dir().to_string_lossy().into_owned(),
        cwd: startup_cwd_str.clone(),
        session_id: String::new(),
        claude_codex_fallback_enabled: persisted_claude_codex_fallback,
        plugin_skill_dirs: wiring
            .plugin_runtime
            .skill_dirs
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect(),
        plugin_command_dirs: wiring
            .plugin_runtime
            .command_dirs
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect(),
        skill_bundles: rebon_harness::kernel_bootstrap::skill_bundles(),
    };

    // 11. Capture cwd and mint (or resume) a session on our own
    //     ServerState clone. The executor will see the same session
    //     because both halves of the state map share the inner `Arc`
    //     via `handler.state().clone()` on step 7.
    let record = bind_session_record(
        startup_cwd_str,
        resume_session_id.as_deref(),
        mirror,
        &tui_server_state,
        startup_permission_mode,
        handed_back.session_lock.take(),
    )?;
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        record.session_id = %record.session_id,
        resumed = resume_session_id.is_some(),
        transcript_entries = record.loaded_transcript.len(),
        "rebon startup: session record ready"
    );
    let current_session_mode = if wiring.startup_coordinator_mode {
        "coordinator"
    } else {
        "normal"
    };
    let _ = tui_server_state.set_session_mode(&record.session_id, current_session_mode);
    if let Err(err) = rebon_session::save_session_mode(
        &projects_root(),
        &record.cwd,
        &record.session_id,
        current_session_mode,
    ) {
        tracing::warn!(
            error = %err,
            record.session_id = %record.session_id,
            mode = %current_session_mode,
            "rebon: failed to persist initial session mode"
        );
    }
    std::env::set_var("REBON_SESSION_ID", &record.session_id);
    // Skill expansion includes `${CLAUDE_SESSION_ID}` and resolves relative
    // paths against cwd, so discovery must happen only after the final startup
    // session/cwd binding is known (resume may have changed both).
    let mut initial_skill_loader = skill_loader_template.clone();
    initial_skill_loader.cwd = record.cwd.clone();
    initial_skill_loader.session_id = record.session_id.clone();
    let skill_state = load_startup_skills(&initial_skill_loader, &skill_registry).await;
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: startup skills loaded"
    );
    let kernel = bind_session_kernel(SessionKernelRequest {
        runtime_resolved,
        engine: &wiring.engine,
        runtime_model: wiring.model.runtime_model.clone(),
        session_id: &record.session_id,
        cwd: &record.cwd,
        existing_kernel_scopes: handed_back.kernel_scopes.take(),
        task_runtime_controller,
        client: client_for_tasks.clone(),
        default_model: default_model_for_tasks.clone(),
        sub_agent_model_config: wiring.sub_agent_model_config.clone(),
        model_profiles: wiring.model_profiles.clone(),
        sub_agent_model_router: wiring.sub_agent_model_router.clone(),
        subagent_filter_handle: wiring.subagent_filter_handle.clone(),
        coordinator_mode_handle: wiring.coordinator_mode_handle.clone(),
        coordinator_use_worktree: wiring.coordinator_use_worktree,
        startup_coordinator_mode: wiring.startup_coordinator_mode,
        plugin_hooks: wiring.plugin_runtime.hooks.clone(),
        worker_file_history_tracker: &worker_file_history_tracker,
        acp_subagent_pool: declared.acp_subagent_pool.clone(),
        user_attachment_poller,
        notices: &mut overrides_startup_notices,
    })
    .await;

    let records = assemble_session_build(SessionAssemblyParts {
        wiring,
        declared,
        record,
        kernel,
        startup_started,
        capability_mode,
        mirror,
        startup,
        startup_permission_mode,
        shared_auto_mode_state,
        engine_for_tasks,
        client_for_tasks,
        default_model_for_tasks,
        attached_background_job_id,
        remote_host,
        handler,
        tui_server_state,
        executor_server_state,
        mcp,
        mcp_client,
        skill_registry,
        skill_state,
        skill_loader_template,
        cron_poller,
        session_cron_store,
        mid_turn_queue,
        additional_attachment_poller,
        overrides_startup_notices,
    })?;
    finish_session_build(records)
}

pub fn system_prompt_config_for_engine(
    engine: &rebon_core::Engine,
    session_filter: &rebon_tool::ToolFilter,
    model: &str,
    auto_continue_background_agents: bool,
) -> rebon_core::system_prompt::SystemPromptConfig {
    let started = std::time::Instant::now();
    let deferred_tool_names = if rebon_tool::is_tool_search_enabled() {
        engine.deferred_tool_names()
    } else {
        Vec::new()
    };
    // Names only: a full snapshot serialises every tool's input schema,
    // which was 300–400 ms of a debug startup for a list of strings.
    let tool_names = engine
        .eager_tool_name_snapshots()
        .into_iter()
        .filter(|snapshot| {
            session_filter.is_unrestricted()
                || session_filter.allows(&snapshot.name, snapshot.aliases)
        })
        .map(|snapshot| snapshot.name)
        .collect();
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "rebon startup: system prompt tool names collected"
    );
    let prompt_overrides = crate::rebon_config::saved_system_prompt_overrides();
    let shell = detect_shell();
    let os_version = detect_os_version();
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "rebon startup: system prompt environment detected"
    );

    rebon_core::system_prompt::SystemPromptConfig {
        model: model.to_string(),
        model_marketing_name: model_marketing_name(model),
        knowledge_cutoff: knowledge_cutoff(model),
        tool_names,
        deferred_tool_names,
        platform: std::env::consts::OS.to_string(),
        shell,
        os_version,
        language: crate::rebon_config::saved_language(),
        normal_system_prompt_override: prompt_overrides.normal,
        minimal_system_prompt_override: prompt_overrides.minimal,
        chat_system_prompt_override: prompt_overrides.chat,
        auto_continue_background_agents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript_on_disk(projects_root: &Path, cwd: &str, session_id: &str) {
        let path = rebon_session::ensure_session_file_path(projects_root, cwd, session_id).unwrap();
        std::fs::write(
            path,
            "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\
             \"timestamp\":\"2026-09-25T00:00:00.000Z\",\"message\":null}\n",
        )
        .unwrap();
    }

    /// A resumed record says the mode the session starts in, whatever it is:
    /// the plan-mode reminders read the record, the gate reads a cell that
    /// starts in this mode, and the two have to agree from the first call.
    #[test]
    fn a_resumed_record_takes_the_mode_the_session_starts_in() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/repo/resume-mode";
        for (index, (startup, expected)) in [
            (Some(PermissionMode::Plan), "plan"),
            (Some(PermissionMode::AcceptEdits), "acceptEdits"),
            (Some(PermissionMode::Auto), "auto"),
            (None, "default"),
        ]
        .into_iter()
        .enumerate()
        {
            let session_id = format!("sess-resume-mode-{index}");
            transcript_on_disk(root.path(), cwd, &session_id);
            let state = ServerState::new();

            let record =
                load_resumed_session_record(&state, root.path(), &session_id, cwd, startup)
                    .unwrap();

            assert_eq!(record.id, session_id);
            assert_eq!(
                state.session_permission_mode(&session_id).as_deref(),
                Some(expected),
                "startup mode {startup:?}"
            );
        }
    }

    /// Resuming straight into plan mode is entering it: the snapshot the
    /// plan-mode producer reads must not carry an exit notice from nowhere.
    #[test]
    fn resuming_into_plan_leaves_no_pending_exit_notice() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/repo/resume-plan";
        transcript_on_disk(root.path(), cwd, "sess-resume-plan");
        let state = ServerState::new();

        load_resumed_session_record(
            &state,
            root.path(),
            "sess-resume-plan",
            cwd,
            Some(PermissionMode::Plan),
        )
        .unwrap();

        let snapshot = state
            .attachment_session_snapshot("sess-resume-plan")
            .unwrap();
        assert_eq!(snapshot.permission_mode, "plan");
        assert!(!snapshot.attachment_state.needs_plan_mode_exit_attachment);
        assert!(!snapshot.attachment_state.has_exited_plan_mode);
    }

    #[test]
    fn a_missing_transcript_is_still_an_error() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();

        let error = load_resumed_session_record(
            &state,
            root.path(),
            "sess-not-there",
            "/repo/none",
            Some(PermissionMode::Plan),
        )
        .unwrap_err();

        assert!(error.to_string().contains("failed to resume session"));
        assert!(state.session_permission_mode("sess-not-there").is_none());
    }
}
