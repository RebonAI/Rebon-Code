//! Shared harness bootstrap helpers used by both run modes.
//!
//! Both the `--acp` fast-path (`acp::run_acp_server`) and the
//! local TUI path (`tui::run`) need to build the same set of
//! harness primitives:
//!
//! 1. a provider-specific [`ModelClient`] wrapped in the standard
//!    `RetryMiddleware` + `LoggingMiddleware` stack,
//! 2. a default model string with provider-specific fallback,
//! 3. a projects-root `PathBuf` used for transcript storage,
//! 4. a default env-driven [`PolicyStore`] preloaded with
//!    `REBON_ALLOW_RULES` / `REBON_DENY_RULES`,
//! 5. a default env-driven [`ToolFilter`] built from
//!    `REBON_ALLOW_TOOLS` / `REBON_DENY_TOOLS`,
//! 6. a default env/project-driven MCP client built from supported MCP
//!    config sources, assembled in `rebon-session-runtime`.
//!
//! Keeping these in one module guarantees the two entrypoints stay
//! behaviourally identical on the "which provider, which model, where
//! do transcripts live, which rules/tools/MCP servers are loaded"
//! axis — only the IO surface (stdio JSON-RPC vs. local ratatui event
//! loop) differs.
//!
//! This is the shared bootstrap that runs before either the interactive
//! TUI or the `--acp` fast-path's server starts.
//!
//! The provider / model / compaction / policy / environment helpers below
//! are shared with the TUI wiring, which
//! imports them instead of carrying copies — that is why a number of
//! otherwise-internal helpers here are `pub`.

/// The kernel itself, for a front end that has to name a `Context` but has no
/// business assembling one.
///
/// The harness is what boots the process kernel and hands out its scopes, so
/// it is also where a front end should reach for the type. A second direct
/// edge to `rebon-kernel` from a binary says the binary assembles kernels,
/// which it does not.
///
/// The same rule, said the other way round now that the kernel holds the
/// process slot (`rebon_kernel::process`): **a binary calls
/// [`kernel_bootstrap::process_plugin_registry`] / [`kernel_bootstrap::process_kernel`]
/// and never `rebon_kernel::process_registry()` directly.** Booting is the
/// harness's job — it is the only place that has the definition table — and
/// the kernel's `Option`-returning readers answer "has this process booted",
/// which is a question a front end has no business having. Crates *below* the
/// assembly layer read them; binaries ask the harness, which boots.
pub use rebon_kernel;

/// Installed plugin packages: what is on disk, and what a manifest declared.
///
/// Re-exported here because reading the package store is half of assembling a
/// session -- the harness already does the other half with the same crate --
/// and a front end that only wants to list what is installed should not need
/// its own edge to the package format.
pub use rebon_plugin_package;

pub mod agent_assembly;
pub mod kernel_bootstrap;
pub mod session_assembly;
pub mod session_model;
pub use session_model::SessionModel;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rebon_agent_core::{
    AgentBackend, BackendPromptExecutor, ChannelSessionUpdatePublisher, LocalAgentBackend,
    PromptExecutor, SessionUpdatePublisher,
};
use rebon_api::{
    anthropic_client, openai_compatible_client, AnthropicClientConfig, ChatCompletionsCompat,
    ContextPruneConfig, ContextPruneMiddleware, LoggingMiddleware, ModelClient,
    OpenAiCompatibleClientConfig, OpenAiRequestOptions, OpenAiResponsesClientConfig,
    OpenAiResponsesProvider, PruneLevel, PruneLevelHandle, ReasoningEffort, ReasoningMode,
    RetryConfig, RetryMiddleware, ServiceTierHandle, TokenRefresher, UniversalModelClient,
};
use rebon_core::permission::{OutboundPermissionQuery, SharedChannelPermissionBroker};
use rebon_types::env::env_flag;
use rebon_types::{AgentCapabilityMode, SessionUpdateParams};

use rebon_agent_core::model_router::{
    ConfigurableModelRouter, ProviderModelRuntime, ProviderRuntimeResolver,
};
use rebon_config::{check_startup_oauth, OAuthStartupState, ProviderFormat, ResolvedProvider};
use rebon_core::{
    policy::PolicyStore,
    query::{EngineQueryExecutor, QueryEventObserver, RuntimeModelConfig, SharedRuntimeModel},
    Engine,
};
use rebon_permissions::types::{PermissionMode, PermissionRuleSource};
use rebon_tool::ToolFilter;
use tokio::sync::mpsc::UnboundedReceiver;

use rebon_plugin_host::provider_registry::{ProviderBuildContext, ProviderRegistry};
use rebon_provider::model_provider_plugin::PluginModelProviderContribution;
use rebon_provider::provider_runtime_cache::ProviderRuntimeCache;

/// Build a raw [`ModelClient`] from environment variables **without**
/// middleware wrapping. Used by [`resolve_runtime_model`]'s fallback
/// path so it can build the middleware stack in the same order as
/// the rebon-config path (ContextPrune → Retry → Logging).
pub fn build_raw_model_client(
    service_tier: Option<ServiceTierHandle>,
) -> anyhow::Result<Arc<dyn ModelClient>> {
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        if !api_key.is_empty() {
            return Ok(Arc::new(anthropic_client(
                AnthropicClientConfig::with_api_key(api_key),
            )));
        }
    }
    if let Ok(api_key) = std::env::var("DEEPSEEK_API_KEY") {
        if !api_key.is_empty() {
            let mut config = OpenAiCompatibleClientConfig::with_base_url(
                std::env::var("DEEPSEEK_BASE_URL")
                    .unwrap_or_else(|_| "https://api.deepseek.com".to_string()),
                api_key,
            );
            config.request_options = deepseek_request_options_from_env();
            // DeepSeek thinking mode: replay reasoning_content on tool-call
            // turns and keep tool-only content as a non-null empty string so
            // historical assistant turns match the provider's cached prefix
            // unit byte-for-byte. Stripping these caused observed cache hit
            // rates to collapse to the ~20-55% band.
            if deepseek_thinking_enabled_from_env() {
                config.compat = ChatCompletionsCompat::DEEPSEEK_THINKING;
            }
            return Ok(Arc::new(openai_compatible_client(config)));
        }
    }
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        if !api_key.is_empty() {
            let mut config = if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
                OpenAiCompatibleClientConfig::with_base_url(base_url, api_key)
            } else {
                OpenAiCompatibleClientConfig::with_api_key(api_key)
            };
            config.service_tier = service_tier;
            return Ok(Arc::new(openai_compatible_client(config)));
        }
    }
    anyhow::bail!(
        "no model credentials found — set ANTHROPIC_API_KEY, DEEPSEEK_API_KEY, or OPENAI_API_KEY \
         (with optional DEEPSEEK_BASE_URL / OPENAI_BASE_URL)"
    )
}

/// The vendor the env fallback dials: the Anthropic key means Anthropic,
/// a DeepSeek key means DeepSeek unless `DEEPSEEK_BASE_URL` points
/// elsewhere, and `OPENAI_BASE_URL` decides the rest.
pub fn env_fallback_vendor(format: &ProviderFormat) -> rebon_api::ProviderVendor {
    use rebon_api::ProviderVendor;
    if matches!(format, ProviderFormat::Anthropic) {
        return ProviderVendor::Anthropic;
    }
    if std::env::var("DEEPSEEK_API_KEY").is_ok_and(|key| !key.is_empty()) {
        return std::env::var("DEEPSEEK_BASE_URL")
            .ok()
            .map(|url| ProviderVendor::detect(&url))
            .filter(|vendor| *vendor != ProviderVendor::Unknown)
            .unwrap_or(ProviderVendor::DeepSeek);
    }
    std::env::var("OPENAI_BASE_URL")
        .ok()
        .map(|url| ProviderVendor::detect(&url))
        .unwrap_or(ProviderVendor::OpenAi)
}

fn deepseek_thinking_enabled_from_env() -> bool {
    // Unset or unrecognised means "on"; only an explicit falsy value turns it off.
    env_flag("DEEPSEEK_THINKING_ENABLED").unwrap_or(true)
}

fn deepseek_request_options_from_env() -> OpenAiRequestOptions {
    let enabled = deepseek_thinking_enabled_from_env();
    let mut options = OpenAiRequestOptions::default();
    options.extra_body.insert(
        "thinking".to_string(),
        serde_json::json!({ "type": if enabled { "enabled" } else { "disabled" } }),
    );
    if enabled {
        options.omit_body_fields.push("temperature".to_string());
        options.body.insert(
            "reasoning_effort".to_string(),
            serde_json::json!(deepseek_reasoning_effort_wire_from_env()),
        );
    }
    options
}

fn deepseek_reasoning_effort_wire_from_env() -> &'static str {
    std::env::var("DEEPSEEK_REASONING_EFFORT")
        .or_else(|_| std::env::var("DEEPSEEK_THINKING_EFFORT"))
        .ok()
        .as_deref()
        .map(parse_deepseek_reasoning_effort_wire)
        .unwrap_or("high")
}

fn parse_deepseek_reasoning_effort_wire(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "xhigh" => "xhigh",
        _ => "high",
    }
}

// ---------------------------------------------------------------------------
// rebon-first resolution (primary path used by both run modes)
// ---------------------------------------------------------------------------

/// The fully-resolved model client + default model string that
/// both `acp::run_acp_server` and
/// `rebon_session_runtime::build_tui_session` feed into their
/// `EngineQueryExecutor` construction.
pub struct RuntimeModel {
    /// Middleware-wrapped model client ready to hand to the
    /// executor.
    pub client: Arc<dyn ModelClient>,
    /// Default model name to pass to `EngineQueryExecutor::new`.
    /// For rebon-backed providers this is the `model` field from
    /// `config.json::customProviders`; for the env-var fallback
    /// it comes from [`default_model`].
    pub model: String,
    /// Human-readable provider name for the status bar.
    /// `"openai"` / `"anthropic"` / custom provider name, or
    /// `"env"` for the env-var fallback.
    pub provider_name: String,
    /// Resolved provider transport format. Consumers use this to
    /// gate features whose backend support differs per transport.
    pub provider_format: ProviderFormat,
    /// Whether the selected provider is a first-party OpenAI route that may
    /// receive the local ComputerUse tool.
    pub computer_use: bool,
    /// The Images API behind the `ImageGen` tool: `Some` exactly when the
    /// selected provider is a first-party OpenAI route
    /// ([`rebon_config::is_first_party_openai_route`]).
    pub images_endpoint: Option<Arc<rebon_plugin_image_gen::ImagesEndpoint>>,
    /// Shared retry-state handle that the [`RetryMiddleware`] writes
    /// to and the TUI reads each frame to display "Retry 2/10".
    pub retry_notifier: rebon_api::RetryNotifier,
    /// Shared handle the TUI uses to toggle context-prune level at
    /// runtime via `/settings`.
    pub prune_level: PruneLevelHandle,
    /// Shared runtime switch for OpenAI fast service tier.
    pub service_tier: ServiceTierHandle,
    /// Whether fast service tier can affect the current main provider.
    pub service_tier_available: bool,
    /// Whether the provider runtime cache may share this runtime across
    /// sub-agent spawns.
    ///
    /// External plugin clients have no `fork_for_sub_agent`, and their
    /// conversation-level signals (`endTurn` / `reset` / `invalidate`) are
    /// scope-scoped: one client, one plane scope, and an adapter that keeps
    /// state keys it by that scope. Sharing the *client* would therefore share
    /// the scope, and one agent's turn end would clear another's state — which
    /// is the same hazard the child-process transport had, at the same place,
    /// for a different reason. What changed is that the isolation is now a
    /// scope rather than a process, so it costs a `scope/open` instead of a
    /// spawn.
    ///
    /// OAuth providers outside the Responses format have no 401-refresh
    /// middleware, so a cached client could never rotate an expired token.
    ///
    /// Never written by hand: [`provider_runtime_cacheable`] is the rule, and
    /// the three places that build a `RuntimeModel` all ask it. A `true` and a
    /// `false` written at two of those call sites is how the two halves of one
    /// rule drift apart without either half looking wrong.
    pub provider_runtime_cacheable: bool,
    /// Active provider model profile map used by sub-agent resolution.
    pub model_profiles: rebon_types::ModelProfileMap,
    /// Concrete model used for lightweight title generation.
    pub title_model: String,
    /// Last fully-resolved system prompt captured by the executor.
    pub system_prompt_snapshot: rebon_core::query::SystemPromptSnapshot,
    /// Shared runtime model config used by the executor so /provider and /model
    /// changes take effect on the next turn without rebuilding the whole session.
    pub runtime_model: SharedRuntimeModel,
}

impl RuntimeModel {
    /// Hand this runtime's provider verdicts to the plugins whose tools the
    /// provider gates: the desktop verdict to `ComputerUse`, the Images
    /// endpoint to `ImageGen`.
    ///
    /// The rules themselves are
    /// [`rebon_plugin_host::provider_registry::computer_use_enabled_for_provider`]
    /// and [`rebon_config::is_first_party_openai_route`], asked once while the
    /// provider is resolved and carried here in [`Self::computer_use`] and
    /// [`Self::images_endpoint`]; this is only their publication. Every
    /// surface that adopts a runtime for a session calls it — session
    /// assembly, the TUI's `/provider` refresh, and a background worker
    /// switching model — because the cells are what the tools read at call
    /// time. Resolving a runtime without adopting it (a sub-agent's provider,
    /// say) deliberately does not publish.
    pub fn publish_provider_capabilities(&self) {
        rebon_plugin_computer_use::set_provider_capability(self.computer_use);
        rebon_plugin_image_gen::set_provider_endpoint(self.images_endpoint.clone());
    }
}

/// The Images API a resolved provider offers `ImageGen`: its own base URL and
/// credentials, on a first-party OpenAI route only. An OAuth route carries
/// the refresher the rest of the runtime rotates its token with, because an
/// image request made hours into a session is when the startup token has
/// expired.
fn images_endpoint_for(
    config_dir: &Path,
    resolved: &ResolvedProvider,
) -> Option<Arc<rebon_plugin_image_gen::ImagesEndpoint>> {
    rebon_config::is_first_party_openai_route(
        resolved.format,
        &resolved.base_url,
        resolved.oauth.is_some(),
        resolved.provider_selection.is_external(),
    )
    .then(|| {
        Arc::new(rebon_plugin_image_gen::ImagesEndpoint::new(
            resolved.base_url.clone(),
            resolved.api_key.clone(),
            compact_token_refresher(config_dir, resolved),
        ))
    })
}

/// Whether the fast service tier can affect this provider. External plugin
/// providers never take it; `rebon_config` owns the endpoint rule.
pub fn openai_service_tier_available(resolved: &ResolvedProvider) -> bool {
    rebon_config::openai_service_tier_available(
        resolved.format,
        &resolved.base_url,
        resolved.oauth.is_some(),
        resolved.provider_selection.is_external(),
    )
}

pub fn env_openai_service_tier_available() -> bool {
    if std::env::var("ANTHROPIC_API_KEY").is_ok_and(|key| !key.is_empty())
        || std::env::var("DEEPSEEK_API_KEY").is_ok_and(|key| !key.is_empty())
    {
        return false;
    }
    if !std::env::var("OPENAI_API_KEY").is_ok_and(|key| !key.is_empty()) {
        return false;
    }
    std::env::var("OPENAI_BASE_URL")
        .map(|base_url| rebon_config::is_openai_api_endpoint(ProviderFormat::Openai, &base_url))
        .unwrap_or(true)
}

/// Deliberately not `Clone`: the resolver is shared behind an `Arc`, and
/// cloning it would split the runtime cache into per-clone copies — which is
/// exactly the duplicate-child-process problem the cache exists to prevent.
struct RuntimeProviderResolver {
    runtime_model: SharedRuntimeModel,
    service_tier: ServiceTierHandle,
    /// Keyed by provider id. Without this, every cross-provider sub-agent
    /// spawn re-read config, re-ran the OAuth refresh, rebuilt the middleware
    /// stack, and — for plugin providers — spawned a brand new child process.
    runtime_cache: ProviderRuntimeCache<RuntimeModel>,
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

/// Build this session's sub-agent spawner off the `sub-agent-spawner` seat.
///
/// `None` is the ordinary answer when `plugins.agents.enabled` is false:
/// nothing provides the seat, the executor gets no spawner, and the `Agent`
/// tool is off the tool seat in the same breath. A provider that refuses is a
/// wiring fault and is logged rather than silently swallowed.
pub fn sub_agent_spawner_for_session(
    request: rebon_tool::SubAgentSpawnerRequest,
) -> Option<Arc<dyn rebon_tool::SubAgentSpawner>> {
    let source = kernel_bootstrap::process_kernel()
        .context()
        .get::<rebon_tool::SubAgentSpawnerService>()?;
    match source.for_session(request) {
        Ok(spawner) => Some(spawner),
        Err(error) => {
            tracing::warn!(%error, "sub-agent spawner seat refused this session");
            None
        }
    }
}

/// Build this session's team manager off the `team-manager` seat.
///
/// `None` on the same terms as [`sub_agent_spawner_for_session`]; the tools
/// that need one (`TeamCreate`, `SendMessage`, `TeamDelete`) already answer
/// "no team runtime here" when `ToolContext` carries none.
pub fn team_manager_for_session(
    request: rebon_tool::TeamManagerRequest,
) -> Option<Arc<dyn rebon_tool::TeamManager>> {
    let source = kernel_bootstrap::process_kernel()
        .context()
        .get::<rebon_tool::TeamManagerService>()?;
    match source.for_session(request) {
        Ok(manager) => Some(manager),
        Err(error) => {
            tracing::warn!(%error, "team manager seat refused this session");
            None
        }
    }
}

struct AutomaticRuntimeModelRouter {
    inner: ConfigurableModelRouter,
    runtime: SharedRuntimeModel,
}

#[async_trait::async_trait]
impl rebon_agent_core::model_router::AgentModelRouter for AutomaticRuntimeModelRouter {
    async fn resolve(
        &self,
        request: rebon_agent_core::model_router::ModelRouteRequest,
    ) -> anyhow::Result<rebon_agent_core::model_router::ResolvedModelRuntime> {
        self.inner.resolve(request).await
    }

    async fn resolve_automatic(
        &self,
        request: rebon_agent_core::model_router::ModelRouteRequest,
    ) -> anyhow::Result<rebon_agent_core::model_router::ResolvedModelRuntime> {
        let mut resolved = self.inner.resolve(request).await?;
        let rebuilt = self
            .runtime
            .rebuild_model(resolved.provider_name.clone(), resolved.model.clone())
            .await?;
        anyhow::ensure!(
            rebuilt.provider_name == resolved.provider_name,
            "runtime rebuild returned a different provider than the route resolved"
        );
        resolved.client = rebuilt.client;
        resolved.model = rebuilt.model;
        Ok(resolved)
    }
}

pub fn model_router_for_runtime(
    runtime_model: SharedRuntimeModel,
    service_tier: ServiceTierHandle,
    model_config: rebon_types::SubAgentModelConfig,
) -> Arc<dyn rebon_agent_core::model_router::AgentModelRouter> {
    // Plugin-registered provider routes participate in resolution
    // (builtin providers keep precedence; see kernel_model_router).
    let resolver = rebon_provider::kernel_model_router::KernelAwareProviderResolver::new(
        kernel_bootstrap::process_kernel().context().clone(),
        Arc::new(RuntimeProviderResolver {
            runtime_model: runtime_model.clone(),
            service_tier,
            runtime_cache: ProviderRuntimeCache::new(&rebon_config::config_home_dir()),
        }),
    );
    Arc::new(AutomaticRuntimeModelRouter {
        inner: ConfigurableModelRouter::new(resolver).with_model_config(model_config),
        runtime: runtime_model,
    })
}

/// Main-session seam for kernel plugin model routes. Precedence holds:
/// native resolution runs first, so this is only consulted after
/// `customProviders` failed to know the requested name. When the kernel
/// route table (fed by composed plugins like llm-deepseek) knows it, a
/// full [`RuntimeModel`] is built over the plugin's stream host.
pub async fn kernel_route_runtime_model(
    provider: &str,
    overrides: &HarnessOverrides,
) -> Option<RuntimeModel> {
    let ctx = kernel_bootstrap::process_kernel().context().clone();
    let lookup = |ctx: &rebon_kernel::Context| -> Option<serde_json::Value> {
        match ctx.call_json(
            rebon_provider::kernel_model_router::MODEL_ROUTER_SERVICE,
            "resolve",
            serde_json::json!({
                "provider": provider,
                "model": overrides.model.as_deref(),
            }),
        ) {
            Ok(serde_json::Value::Null) | Err(_) => None,
            Ok(route) => Some(route),
        }
    };

    // Steady state: the route is already announced. Otherwise boot the
    // configured composition (idempotent) and ask once — only reached when
    // native resolution already failed, so well-known provider names never
    // pay even that.
    //
    // Once rather than polling, because there is no window to wait out.
    // `register_published` announces a route inside the boot, and the boot
    // hands the plane back before `ensure_plane` makes it visible, so a
    // visible plane already has its routes. A second caller arriving mid-boot
    // blocks on the boot lock and re-checks the slot inside it, so it sees the
    // same finished state. And a boot that failed leaves a standing refusal
    // rather than a plane, so nothing is coming later. A `None` here means no
    // loaded entry claims this provider name, which three more seconds cannot
    // change.
    let route = match lookup(&ctx) {
        Some(route) => route,
        None => {
            rebon_plugin_host::plugin_boot::ensure_process_composition(
                &kernel_bootstrap::process_plugin_registry(),
            )
            .await;
            lookup(&ctx)?
        }
    };

    let model = route.get("model")?.as_str()?.to_string();
    let context_window = route
        .get("contextWindow")
        .and_then(|w| w.as_u64())
        .and_then(|w| u32::try_from(w).ok());

    // Adapter host bound → live client, with no wait for it.
    //
    // `rebon_plugin_host::plugin_plane::register_published` registers the host *before* it
    // announces the route, and `withdraw` removes the route before the host,
    // so a visible route always has a host behind it — on this thread or any
    // other. A `None` here is therefore a metadata-only route, which is not an
    // executable provider and fails resolution rather than yielding a
    // placeholder client. This used to sleep up to half a second first, for an
    // "event wave" that the one synchronous publish had already ruled out.
    let host = rebon_provider::kernel_llm_dispatch::llm_host_for(provider)?;
    let base: Arc<dyn ModelClient> = Arc::new(
        rebon_provider::kernel_llm_dispatch::DshLlmClient::new(provider, host),
    );

    // dsh chat-completions wire semantics sit closest to the OpenAI chat
    // format, and that format gates no extra transport features.
    let provider_format = ProviderFormat::Openai;
    let notifier = rebon_api::RetryNotifier::new();
    let service_tier = ServiceTierHandle::new(rebon_config::saved_fast_mode_enabled());
    let prune_handle = prune_handle_from_env(
        &provider_format,
        rebon_api::ProviderVendor::Unknown,
        &model,
        context_window,
        None,
        std::iter::empty::<(String, u32)>(),
        std::iter::empty::<(String, u32)>(),
    );
    let prune_config = prune_config_from_env(&provider_format);
    let pruned: Arc<dyn ModelClient> = Arc::new(ContextPruneMiddleware::wrap(
        base,
        prune_config,
        prune_handle.clone(),
    ));
    let retried: Arc<dyn ModelClient> = Arc::new(RetryMiddleware::wrap_with_notifier(
        pruned,
        RetryConfig::default(),
        notifier.clone(),
    ));
    let wrapped: Arc<dyn ModelClient> = Arc::new(LoggingMiddleware::wrap(retried));
    let title_model = model.clone();
    // Compaction rides the same adapter client (the external-plugin
    // precedent): a plugin route has no separate small-model endpoint.
    let compact_provider = Some(external_model_compact_provider(
        wrapped.clone(),
        &title_model,
        &model,
        ReasoningEffort::Medium,
    ));
    let runtime_model = SharedRuntimeModel::new(RuntimeModelConfig {
        provider_name: provider.to_string(),
        client: wrapped.clone(),
        model: model.clone(),
        model_profiles: rebon_types::ModelProfileMap::default(),
        title_model: title_model.clone(),
        model_marketing_name: model_marketing_name(&model),
        knowledge_cutoff: knowledge_cutoff(&model),
        prune_level: Some(prune_handle.clone()),
        compact_provider,
        compact_fallback_provider: None,
        context_management: None,
        reasoning_mode: None,
    });
    tracing::info!(provider, model = %model, "main-session runtime served by kernel plugin route");
    Some(RuntimeModel {
        client: wrapped,
        model,
        provider_name: provider.to_string(),
        provider_format,
        computer_use: false,
        // An external provider, like the fast tier below.
        images_endpoint: None,
        retry_notifier: notifier,
        prune_level: prune_handle,
        service_tier,
        service_tier_available: false,
        // A plugin route is an external provider, which is the first
        // disqualification. Asked rather than written, so it answers `false`
        // for the reason the rule gives and not because someone typed it.
        provider_runtime_cacheable: provider_runtime_cacheable(true, false, provider_format),
        model_profiles: rebon_types::ModelProfileMap::default(),
        title_model,
        system_prompt_snapshot: Arc::new(std::sync::RwLock::new(None)),
        runtime_model,
    })
}

/// Slim, framework-agnostic overrides an in-process embedder feeds to the
/// harness. Replaces `RuntimeOverride` (which couples to
/// binary-internal `UiMode`/MCP/status types) — the harness only ever needs
/// runtime and session construction inputs.
#[derive(Clone, Debug, Default)]
pub struct HarnessOverrides {
    /// `customProviders[]` entry to use instead of the stored active provider.
    pub provider: Option<String>,
    /// Model name to use instead of the provider's configured model.
    pub model: Option<String>,
    /// Fast-tier toggle. `None` reads the saved setting, which is what every
    /// embedder did before the flag existed; the CLI passes `--fast` through.
    pub fast_mode: Option<bool>,
    /// Working directory for session construction.
    pub cwd: Option<String>,
    /// Session id to resume (loads the on-disk transcript) instead of new.
    pub resume_session_id: Option<String>,
    /// Optional cap for the agentic model/tool loop.
    pub max_iterations: Option<usize>,
    /// Permission mode applied to a newly-created or resumed session.
    pub permission_mode: Option<PermissionMode>,
    /// Coordinator (worktree/sub-agent) mode.
    pub coordinator_mode: bool,
    /// Agent capability mode for the session. `Minimal` is what the desktop
    /// app's new-chat picker sets; on a provider that opts into Anchored
    /// Minimal it also selects the anchored bootstrap request.
    pub capability_mode: AgentCapabilityMode,
    /// Whether the Agent / Workflow / Team tools are enabled.
    pub sub_agents_enabled: bool,
    /// Optional model provider plugin contributions already materialized by the
    /// embedding binary's plugin runtime.
    pub plugin_model_providers: Vec<PluginModelProviderContribution>,
    /// Hooks contributed by installed plugins, already materialized by the
    /// embedding binary's plugin runtime. The harness cannot resolve these
    /// itself — reading `--plugin-dirs` and expanding a manifest's
    /// placeholders is the binary's job — so they arrive here alongside the
    /// model providers, and join the user's `settings.json` hooks on the
    /// session's policy handle.
    pub plugin_hooks: Vec<rebon_hooks::IndividualHookConfig>,
    /// Optional observer wired into the executor to receive every raw
    /// [`rebon_core::query::QueryEvent`] as the turn runs. `rebon exec
    /// --json` sets this to stream a JSONL event feed for eval harnesses;
    /// interactive embedders leave it `None`.
    pub query_event_observer: Option<QueryEventObserver>,
}

/// Build a provider's runtime from scratch. The caller owns caching and is
/// responsible for re-applying live session state (fast tier) to the result.
async fn resolve_provider_runtime(provider: Option<&str>) -> anyhow::Result<RuntimeModel> {
    let overrides = HarnessOverrides {
        provider: provider.map(str::to_string),
        ..Default::default()
    };
    resolve_runtime_model(&overrides).await
}

fn build_provider_registry_from_contributions(
    contributions: &[PluginModelProviderContribution],
) -> ProviderRegistry {
    let mut registry = ProviderRegistry::with_builtins();
    for contribution in contributions {
        if let Err(warning) = registry.register_plugin_provider(contribution.clone()) {
            tracing::warn!(warning = %warning, "rebon harness provider registry: plugin provider ignored");
        }
    }
    registry
}

/// Whether a runtime built for this provider may be shared across sub-agent
/// spawns -- see [`RuntimeModel::provider_runtime_cacheable`] for what sharing
/// would cost.
///
/// Two disqualifications, and the reason each one still holds:
///
/// * An **external** (plugin) provider. Its client owns one plane scope, opened
///   in `PlaneModelProviderClient::bind` and closed when the client drops, and
///   the adapter keys whatever it remembers by that scope. Sharing the client
///   shares the scope. Collapsing the two dialects onto one transport did not
///   change this: `LlmRoute` moved the *wire*, and the scope is still per
///   client.
/// * **OAuth outside the Responses format**, which has no 401-refresh
///   middleware, so a cached client could never rotate an expired token.
fn provider_runtime_cacheable(
    external_provider: bool,
    oauth: bool,
    format: ProviderFormat,
) -> bool {
    !external_provider && (!oauth || format == ProviderFormat::OpenaiResponses)
}

/// Stop the process-wide plugin plane, if this process started one.
///
/// Call it on the way out of a command. A process that booted a plane cannot
/// exit while the plane is up: the plane holds a Node child and a task reading
/// that child's stdout, and dropping the tokio runtime waits for the task. The
/// wait happens *before* the `Termination` impl prints `main`'s error, so a
/// rebon that failed to resolve a provider parks with the explanation already
/// in hand and never gets to say it. That is not a hypothetical -- it is what
/// `rebon exec --plugin-dir <a package with a model provider>` did.
///
/// Deliberately not called from a failed resolution. The plane is process-wide
/// and a resolution is not: a provider switch inside a live session fails the
/// same way, and tearing the plane down there would take the plugins out from
/// under every session that is still running on them.
///
/// Idempotent, and cheap when no plane ever started.
pub async fn shutdown_process_plugin_plane() {
    // Native MCP disposal is synchronous revocation only. Its plugin keeps
    // retired generations and their JoinHandles until this async host boundary.
    rebon_plugin_mcp::shutdown_process_runtimes().await;
    rebon_plugin_host::plugin_boot::shutdown_process_plane().await;
}

/// Resolve the model client + default model name used by both run
/// modes, preferring the rebon `.rebon/config.json` active
/// provider when present and falling back to the env var path
/// otherwise.
///
/// The rebon path:
///
/// 1. Loads `config.json` and `.credentials.json` via
///    [`rebon_config::resolve_from_dir_with_external_provider_ids`], honouring any
///    `--provider` override in `HarnessOverrides::provider`.
/// 2. If the active provider is OAuth-backed and its cached
///    access token is within the 5-minute expiry buffer, runs a
///    preemptive refresh so the very first request does not have
///    to round-trip a 401. A refresh that fails is logged, never
///    fatal — an invalidated refresh token would otherwise take the
///    whole process down before any surface exists to say `/login`
///    on. The session starts on the cached token; the per-request
///    401 refresher and the `/login` prompt do the recovering.
/// 3. Looks up the resolved provider selection in the shared provider
///    registry. Built-ins still dispatch on `format`; external plugin
///    providers build stdio JSON-RPC clients through the registry.
/// 4. Applies `HarnessOverrides::model` if set — replaces the
///    provider's `model` field with the CLI value.
/// 5. Wraps the client in the standard [`RetryMiddleware`] +
///    [`LoggingMiddleware`] stack.
///
/// The fallback path runs the original env-var-based construction
/// via [`build_raw_model_client`] and [`default_model`], so
/// CI and fresh environments without a `.rebon` dir still work.
/// The model override applies in both branches.
pub async fn resolve_runtime_model(overrides: &HarnessOverrides) -> anyhow::Result<RuntimeModel> {
    let runtime = resolve_runtime_model_inner(overrides).await?;
    let base = overrides.clone();
    runtime
        .runtime_model
        .set_runtime_resolver(Arc::new(move |provider, model| {
            let mut overrides = base.clone();
            overrides.provider = Some(provider);
            overrides.model = Some(model);
            Box::pin(async move {
                Ok(resolve_runtime_model_inner(&overrides)
                    .await?
                    .runtime_model
                    .get())
            })
        }));
    Ok(runtime)
}

async fn resolve_runtime_model_inner(overrides: &HarnessOverrides) -> anyhow::Result<RuntimeModel> {
    let config_dir = rebon_config::config_home_dir();
    let provider_registry =
        build_provider_registry_from_contributions(&overrides.plugin_model_providers);
    let external_provider_ids = provider_registry.external_provider_ids();

    let resolved_provider = rebon_config::resolve_from_dir_with_external_provider_ids(
        &config_dir,
        overrides.provider.as_deref(),
        external_provider_ids.iter().map(String::as_str),
    );
    // A provider name no customProviders entry satisfies may be a kernel
    // plugin route (native first, the route only on native
    // failure). The native error surfaces unchanged when neither knows it.
    let resolved_provider = match resolved_provider {
        Err(native_err) => {
            let requested = overrides
                .provider
                .clone()
                .or_else(rebon_config::get_active_custom_provider_name);
            if let Some(name) = requested {
                if let Some(runtime) = kernel_route_runtime_model(&name, overrides).await {
                    return Ok(runtime);
                }
            }
            Err(native_err)
        }
        ok => ok,
    };
    if let Some(mut resolved) = resolved_provider? {
        // Preemptive refresh: if the cached OAuth access token is
        // within the 5-minute expiry buffer, rotate it now so the
        // very first request does not take the 401 → retry slow
        // path.
        if let Some(oauth) = resolved.oauth.clone() {
            match check_startup_oauth(&config_dir, &oauth).await {
                OAuthStartupState::Refreshed(new_access_token) => {
                    tracing::info!(
                        provider = %resolved.name,
                        "rebon-cli: refreshed OpenAI OAuth access token preemptively"
                    );
                    resolved.api_key = new_access_token;
                }
                OAuthStartupState::Cached => {
                    // Still fresh — use the cached token.
                }
                // Not fatal. A refresh token the server has invalidated used
                // to abort startup before the first frame; the session is
                // still startable on the cached token, and the per-request
                // 401 refresher plus the `/login` prompt are the recovery
                // path the user can actually act on.
                state => tracing::warn!(
                    provider = %resolved.name,
                    ?state,
                    "rebon-cli: preemptive OAuth refresh did not run; starting with the cached token"
                ),
            }
        }

        // Apply the explicit --model override after resolution so the log
        // line reflects the effective model, not the on-disk one. The
        // provider's own `model` field is otherwise canonical — the global
        // `settings.json` model key must never shadow a provider selection
        // (it is only consulted on the env fallback path, in
        // `resolve_env_runtime_model`).
        if let Some(override_model) = overrides.model.as_deref() {
            resolved.model = override_model.to_string();
        } else if resolved.model.trim().is_empty() {
            if let Some(default_model) =
                provider_registry.default_model_for(&resolved.provider_selection)
            {
                resolved.model = default_model.to_string();
            }
        }
        if let Some(profiles) = provider_registry.model_profiles_for(&resolved.provider_selection) {
            resolved.model_profiles.fill_missing_from(&profiles);
        }
        let registry_context_windows =
            provider_registry.model_context_windows_for(&resolved.provider_selection);
        for (model, window) in registry_context_windows {
            resolved
                .model_context_windows
                .entry(model)
                .or_insert(window);
        }

        let title_model = rebon_config::resolve_model_profile(
            Some(&resolved),
            rebon_types::MODEL_PROFILE_SMALL,
            &resolved.model,
        );

        tracing::info!(
            provider = %resolved.name,
            format = ?resolved.format,
            model = %resolved.model,
            base_url = %resolved.base_url,
            oauth = resolved.oauth.is_some(),
            "rebon-cli: resolved active provider from rebon config"
        );

        let model_profiles = resolved.model_profiles.clone();
        let model = resolved.model.clone();
        let provider_name = resolved.name.clone();
        let provider_base_url = resolved.base_url.clone();
        let provider_format = resolved.format;
        let provider_selection = resolved.provider_selection.clone();
        let external_provider = provider_selection.is_external();
        let provider_runtime_cacheable = provider_runtime_cacheable(
            external_provider,
            resolved.oauth.is_some(),
            provider_format,
        );
        // The fast-mode gate reads the model table, so a refreshed one has
        // to be in place before the first request is built.
        rebon_config::ensure_model_table_installed();
        let service_tier = ServiceTierHandle::new(
            overrides
                .fast_mode
                .unwrap_or_else(rebon_config::saved_fast_mode_enabled),
        );
        let service_tier_available = !external_provider && openai_service_tier_available(&resolved);
        let active_service_tier = service_tier_available.then(|| service_tier.clone());
        let compact_provider = build_compact_provider(
            &config_dir,
            &resolved,
            &title_model,
            active_service_tier.clone(),
        );
        let compact_fallback_provider = build_compact_fallback_provider(
            &config_dir,
            &resolved,
            &title_model,
            active_service_tier.clone(),
        );
        let external_compact_effort = compact_reasoning_effort_for_provider(&resolved);
        let system_prompt_snapshot = Arc::new(std::sync::RwLock::new(None));
        let notifier = rebon_api::RetryNotifier::new();
        let configured_context_window =
            rebon_config::resolve_model_context_window(Some(&resolved), &model);
        let configured_output_token_limit =
            rebon_config::resolve_model_output_token_limit(Some(&resolved), &model);
        let model_context_windows = resolved.model_context_windows.clone();
        let model_output_token_limits = resolved.model_output_token_limits.clone();
        let provider_vendor = resolved.vendor;
        let reasoning_mode = parse_provider_reasoning_mode(resolved.reasoning_mode.as_deref());
        let computer_use = rebon_plugin_host::provider_registry::computer_use_enabled_for_provider(
            &resolved,
            &provider_registry.capabilities_for(&resolved.provider_selection),
        );
        let images_endpoint = images_endpoint_for(&config_dir, &resolved);
        let built_client = provider_registry
            .build_client(
                &kernel_bootstrap::process_plugin_registry(),
                resolved,
                ProviderBuildContext {
                    config_dir: config_dir.clone(),
                    retry_notifier: Some(notifier.clone()),
                    service_tier: active_service_tier,
                },
            )
            .await?;
        let client = if external_provider {
            rebon_provider::kernel_model_router::route_model_provider_client(
                &provider_name,
                &model,
                built_client,
            )?
        } else {
            built_client
        };
        let compact_provider = if external_provider {
            Some(external_model_compact_provider(
                client.clone(),
                &title_model,
                &model,
                external_compact_effort,
            ))
        } else {
            compact_provider
        };
        let prune_handle = prune_handle_from_env(
            &provider_format,
            provider_vendor,
            &model,
            configured_context_window,
            configured_output_token_limit,
            model_context_windows,
            model_output_token_limits,
        );
        let prune_config = prune_config_from_env(&provider_format);
        let pruned: Arc<dyn ModelClient> = Arc::new(ContextPruneMiddleware::wrap(
            client,
            prune_config,
            prune_handle.clone(),
        ));
        let retried: Arc<dyn ModelClient> = Arc::new(RetryMiddleware::wrap_with_notifier(
            pruned,
            RetryConfig::default(),
            notifier.clone(),
        ));
        let wrapped: Arc<dyn ModelClient> = Arc::new(LoggingMiddleware::wrap(retried));
        let runtime_model = SharedRuntimeModel::new(RuntimeModelConfig {
            provider_name: provider_name.clone(),
            client: wrapped.clone(),
            model: model.clone(),
            model_profiles: model_profiles.clone(),
            title_model: title_model.clone(),
            model_marketing_name: model_marketing_name(&model),
            knowledge_cutoff: knowledge_cutoff(&model),
            prune_level: Some(prune_handle.clone()),
            compact_provider: compact_provider.clone(),
            compact_fallback_provider: compact_fallback_provider.clone(),
            context_management: if external_provider {
                None
            } else {
                anthropic_context_management_config(&provider_format, &provider_base_url)
            },
            reasoning_mode,
        });
        return Ok(RuntimeModel {
            client: wrapped,
            model,
            provider_name,
            provider_format,
            computer_use,
            images_endpoint,
            retry_notifier: notifier,
            prune_level: prune_handle,
            service_tier,
            service_tier_available,
            provider_runtime_cacheable,
            model_profiles,
            title_model,
            system_prompt_snapshot,
            runtime_model,
        });
    }

    resolve_env_runtime_model(overrides)
}

/// Resolve the runtime from environment variables alone.
///
/// The env-var path, kept intact for CI and fresh environments. The stack is built
/// from scratch so the middleware order matches the config path: context
/// prune, then retry, then logging, innermost to outermost.
fn resolve_env_runtime_model(overrides: &HarnessOverrides) -> anyhow::Result<RuntimeModel> {
    // Fallback: env-var path (kept intact for CI / fresh envs).
    // Build the stack from scratch so the middleware order matches
    // the rebon-config path: ContextPrune → Retry → Logging
    // (innermost to outermost).
    tracing::debug!(
    "rebon-cli: no rebon config found, falling back to env vars (ANTHROPIC_API_KEY / DEEPSEEK_API_KEY / OPENAI_API_KEY)"
);
    let fallback_format = fallback_provider_format_from_env();
    rebon_config::ensure_model_table_installed();
    let service_tier = ServiceTierHandle::new(
        overrides
            .fast_mode
            .unwrap_or_else(rebon_config::saved_fast_mode_enabled),
    );
    let service_tier_available = env_openai_service_tier_available();
    let active_service_tier = service_tier_available.then(|| service_tier.clone());
    // The same first-party verdict: an `OPENAI_API_KEY` aimed at OpenAI.
    let images_endpoint = service_tier_available.then(|| {
        Arc::new(rebon_plugin_image_gen::ImagesEndpoint::new(
            std::env::var("OPENAI_BASE_URL").unwrap_or_default(),
            std::env::var("OPENAI_API_KEY").unwrap_or_default(),
            None,
        ))
    });
    let base = build_raw_model_client(active_service_tier.clone())?;
    let model = overrides
        .model
        .clone()
        .or_else(rebon_config::saved_user_model)
        .unwrap_or_else(default_model);
    let title_model = model.clone();
    let compact_provider =
        build_env_compact_provider(&fallback_format, &title_model, active_service_tier.clone());
    let compact_fallback_provider = build_env_compact_fallback_provider(
        &fallback_format,
        &title_model,
        active_service_tier.clone(),
    );
    let notifier = rebon_api::RetryNotifier::new();
    let system_prompt_snapshot = Arc::new(std::sync::RwLock::new(None));
    // Infer format from env vars: ANTHROPIC_API_KEY → Anthropic, else OpenAI.
    let prune_handle = prune_handle_from_env(
        &fallback_format,
        env_fallback_vendor(&fallback_format),
        &model,
        None,
        None,
        std::iter::empty::<(String, u32)>(),
        std::iter::empty::<(String, u32)>(),
    );
    let prune_config = prune_config_from_env(&fallback_format);
    let pruned: Arc<dyn ModelClient> = Arc::new(ContextPruneMiddleware::wrap(
        base,
        prune_config,
        prune_handle.clone(),
    ));
    let retried: Arc<dyn ModelClient> = Arc::new(RetryMiddleware::wrap_with_notifier(
        pruned,
        RetryConfig::default(),
        notifier.clone(),
    ));
    let wrapped: Arc<dyn ModelClient> = Arc::new(LoggingMiddleware::wrap(retried));
    let runtime_model = SharedRuntimeModel::new(RuntimeModelConfig {
        provider_name: String::from("env"),
        client: wrapped.clone(),
        model: model.clone(),
        model_profiles: rebon_types::ModelProfileMap::default(),
        title_model: title_model.clone(),
        model_marketing_name: model_marketing_name(&model),
        knowledge_cutoff: knowledge_cutoff(&model),
        prune_level: Some(prune_handle.clone()),
        compact_provider: compact_provider.clone(),
        compact_fallback_provider: compact_fallback_provider.clone(),
        context_management: anthropic_context_management_config(
            &fallback_format,
            "https://api.anthropic.com",
        ),
        reasoning_mode: None,
    });
    Ok(RuntimeModel {
        client: wrapped,
        model,
        provider_name: String::from("env"),
        provider_format: fallback_format,
        // The env path has no resolved OAuth-sentinel identity, so it must not
        // inherit Computer Use from generic OpenAI API-key support.
        computer_use: false,
        images_endpoint,
        retry_notifier: notifier,
        prune_level: prune_handle,
        service_tier,
        service_tier_available,
        // The env path resolves no provider entry and carries no OAuth
        // identity, so both disqualifications miss it.
        provider_runtime_cacheable: provider_runtime_cacheable(false, false, fallback_format),
        model_profiles: rebon_types::ModelProfileMap::default(),
        title_model,
        system_prompt_snapshot,
        runtime_model,
    })
}

/// Build a [`PruneLevelHandle`] with optional context-window
/// override. Set `REBON_CONTEXT_WINDOW` to a low value (e.g.
/// `50000`) to make auto-compact trigger after a few turns.
///
/// Resolution order:
/// 1. `REBON_CONTEXT_WINDOW`
/// 2. `customProviders[].models[model].contextWindow`
/// 3. Inference from provider format and model name
pub fn prune_handle_from_env(
    format: &ProviderFormat,
    vendor: rebon_api::ProviderVendor,
    model: &str,
    configured_context_window: Option<u32>,
    configured_output_token_limit: Option<u32>,
    model_context_windows: impl IntoIterator<Item = (String, u32)>,
    model_output_token_limits: impl IntoIterator<Item = (String, u32)>,
) -> PruneLevelHandle {
    let handle = if let Ok(v) = std::env::var("REBON_CONTEXT_WINDOW") {
        if let Ok(n) = v.parse::<u32>() {
            tracing::info!(context_window = n, "context window override from env");
            PruneLevelHandle::with_context_window(PruneLevel::Conservative, n)
        } else {
            prune_handle_from_model_config(
                format,
                vendor,
                model,
                configured_context_window,
                configured_output_token_limit,
                model_context_windows,
                model_output_token_limits,
            )
        }
    } else {
        prune_handle_from_model_config(
            format,
            vendor,
            model,
            configured_context_window,
            configured_output_token_limit,
            model_context_windows,
            model_output_token_limits,
        )
    };

    if matches!(format, ProviderFormat::Anthropic) {
        handle.use_full_history_replay_microcompact_profile();
    }

    handle
}

fn prune_handle_from_model_config(
    format: &ProviderFormat,
    vendor: rebon_api::ProviderVendor,
    model: &str,
    configured_context_window: Option<u32>,
    configured_output_token_limit: Option<u32>,
    model_context_windows: impl IntoIterator<Item = (String, u32)>,
    model_output_token_limits: impl IntoIterator<Item = (String, u32)>,
) -> PruneLevelHandle {
    let output_token_reserve = configured_output_token_limit.unwrap_or(0);
    if let Some(window) = configured_context_window {
        tracing::info!(
            context_window = window,
            output_token_reserve,
            model = model,
            "context limits resolved from provider model config"
        );
        return PruneLevelHandle::with_model_context_limits(
            PruneLevel::Conservative,
            window,
            output_token_reserve,
            model_context_windows,
            model_output_token_limits,
        );
    }
    // The vendor's documented catalogue answers before any name-based
    // guess: it knows that Haiku is 200k where the Anthropic default
    // below says 1M, and that Kimi K3 is 1M where the OpenAI table would
    // say 128k.
    if let Some(known) = vendor.known_model(model) {
        let output_token_reserve = if output_token_reserve == 0 {
            known.max_output_tokens.unwrap_or(0)
        } else {
            output_token_reserve
        };
        tracing::info!(
            context_window = known.context_window,
            output_token_reserve,
            model = model,
            vendor = vendor.id(),
            "context limits taken from the vendor's documented catalogue"
        );
        return PruneLevelHandle::with_model_context_limits(
            PruneLevel::Conservative,
            known.context_window,
            output_token_reserve,
            model_context_windows,
            model_output_token_limits,
        );
    }
    let window = match format {
        ProviderFormat::Anthropic => 1_000_000,
        ProviderFormat::Openai | ProviderFormat::OpenaiResponses => {
            infer_openai_context_window(model)
        }
    };
    tracing::info!(
        context_window = window,
        output_token_reserve,
        model = model,
        "context window inferred from model"
    );
    PruneLevelHandle::with_model_context_limits(
        PruneLevel::Conservative,
        window,
        output_token_reserve,
        model_context_windows,
        model_output_token_limits,
    )
}

/// Map an OpenAI model name to its approximate context window size.
/// Conservative defaults — better to compact early than to hit a
/// `context_length_exceeded` error.
fn infer_openai_context_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    if m == "deepseek-flash" {
        200_000
    } else if m == "deepseek-v4-pro" {
        1_000_000
    } else if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        200_000
    } else if m.starts_with("gpt-4o") {
        128_000
    } else if m.starts_with("gpt-4.1") || m.starts_with("gpt-5") {
        // GPT-4.1 and GPT-5.x (5.3, 5.3-codex, 5.4, …) all support 1M context.
        1_000_000
    } else if m.starts_with("gpt-4") {
        128_000
    } else if m.starts_with("grok-4") {
        // xAI Grok 4 family (grok-4, grok-4.x, grok-4-fast) is 256k+.
        // The old 128k unknown-model default halved the real window and
        // the hard context guard's fixed 64k reserve then squeezed the
        // usable history to ~32k, causing constant pruning + re-reads.
        256_000
    } else if m.starts_with("grok") {
        // Grok 3 and earlier: 131k.
        131_072
    } else {
        // Unknown model — default to 128k to be safe.
        128_000
    }
}

pub fn anthropic_context_management_config(
    format: &ProviderFormat,
    base_url: &str,
) -> Option<rebon_api::ContextManagementConfig> {
    if !matches!(format, ProviderFormat::Anthropic) {
        return None;
    }
    if env_flag_disabled("REBON_ANTHROPIC_CONTEXT_MANAGEMENT") {
        return None;
    }
    let tool_trigger = env_u32("REBON_ANTHROPIC_TOOL_CLEAR_TRIGGER").unwrap_or(50_000);
    let compact_trigger = anthropic_compaction_enabled(base_url)
        .then(|| env_u32("REBON_ANTHROPIC_COMPACT_TRIGGER").unwrap_or(100_000));
    Some(
        rebon_api::ContextManagementConfig::anthropic_context_management_with_thresholds(
            tool_trigger,
            compact_trigger,
        ),
    )
}

fn anthropic_compaction_enabled(base_url: &str) -> bool {
    anthropic_compaction_enabled_with_override(base_url, env_flag("REBON_ANTHROPIC_COMPACTION"))
}

fn anthropic_compaction_enabled_with_override(base_url: &str, enabled: Option<bool>) -> bool {
    enabled.unwrap_or_else(|| rebon_api::anthropic::is_official_anthropic_base_url(base_url))
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_flag_disabled(name: &str) -> bool {
    matches!(env_flag(name), Some(false))
}

/// Build a [`ContextPruneConfig`] with optional env-var overrides
/// for testing. Set `REBON_PRUNE_PROTECTED_TURNS` and/or
/// `REBON_PRUNE_MAX_AGE_TURNS` to low values (e.g. `1`) to make
/// pruning kick in after fewer turns — useful for verifying that
/// the `previous_response_id` invalidation works in practice.
pub fn prune_config_from_env(format: &ProviderFormat) -> ContextPruneConfig {
    let mut cfg = match format {
        ProviderFormat::Anthropic => ContextPruneConfig::full_history_replay(),
        ProviderFormat::Openai | ProviderFormat::OpenaiResponses => ContextPruneConfig::default(),
    };
    if let Ok(v) = std::env::var("REBON_PRUNE_PROTECTED_TURNS") {
        if let Ok(n) = v.parse::<usize>() {
            tracing::info!(protected_recent_turns = n, "prune config override from env");
            cfg.protected_recent_turns = n;
        }
    }
    if let Ok(v) = std::env::var("REBON_PRUNE_MAX_AGE_TURNS") {
        if let Ok(n) = v.parse::<usize>() {
            tracing::info!(
                tool_result_max_age_turns = n,
                "prune config override from env"
            );
            cfg.tool_result_max_age_turns = n;
            cfg.error_purge_age_turns = n;
        }
    }
    cfg
}

fn compact_model_for_provider(small_model: &str, provider_model: &str) -> String {
    let small_model = small_model.trim();
    if small_model.is_empty() {
        provider_model.trim().to_string()
    } else {
        small_model.to_string()
    }
}

/// Parse `customProviders[].reasoningMode` into the wire enum. Only
/// `"pro"` (gpt-5.6+ pro mode) is defined; anything else means
/// standard mode.
pub fn parse_provider_reasoning_mode(raw: Option<&str>) -> Option<ReasoningMode> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "pro" => Some(ReasoningMode::Pro),
        _ => None,
    }
}

fn parse_profile_reasoning_effort(raw: &str) -> Option<ReasoningEffort> {
    raw.parse().ok()
}

pub fn compact_reasoning_effort_for_provider(resolved: &ResolvedProvider) -> ReasoningEffort {
    resolved
        .model_profiles
        .resolve_profile_selection(
            rebon_types::MODEL_PROFILE_SMALL,
            Some(&resolved.model),
            &resolved.model,
        )
        .reasoning_effort
        .as_deref()
        .and_then(parse_profile_reasoning_effort)
        .unwrap_or(ReasoningEffort::Low)
}

/// Build a [`ModelCompactProvider`] for an external plugin provider on top of
/// its already-connected client: compaction is a plain model request against
/// the `small` profile model routed through the same plugin process — never a
/// provider-specific compact endpoint the plugin may not support.
pub fn external_model_compact_provider(
    client: Arc<dyn ModelClient>,
    small_model: &str,
    provider_model: &str,
    reasoning_effort: ReasoningEffort,
) -> Arc<dyn rebon_api::CompactProvider> {
    let compact_model = compact_model_for_provider(small_model, provider_model);
    Arc::new(
        rebon_api::ModelCompactProvider::new(client, compact_model)
            .with_reasoning_effort(reasoning_effort),
    )
}

/// The OAuth refresher for a resolved provider, or `None` when it carries a
/// literal key. Every compaction rung needs one: compaction fires hours into a
/// session, which is exactly when the access token minted at startup has
/// expired.
fn compact_token_refresher(
    config_dir: &Path,
    resolved: &ResolvedProvider,
) -> Option<Arc<dyn TokenRefresher>> {
    let oauth = resolved.oauth.as_ref()?;
    Some(Arc::new(
        rebon_provider::oauth_refresher::RebonOAuthRefresher::new(
            config_dir.to_path_buf(),
            oauth.refresh_token.clone(),
        ),
    ))
}

/// Build a [`rebon_api::ModelCompactProvider`] that talks the **Responses**
/// dialect.
///
/// The summariser has to reach the same API the session itself reaches. Routing
/// it through [`openai_compatible_client`] instead — as this code did until
/// 2026-08-31 — appends `/v1/chat/completions` to a base URL that already ends
/// in `/responses`, producing
/// `https://chatgpt.com/backend-api/codex/responses/v1/chat/completions`. No
/// Responses backend serves that path, so the call could only ever 404 and the
/// whole compaction ladder collapsed to blind truncation.
///
/// HTTP, never WebSocket: this is one detached summarisation request rather
/// than a session, so it has nothing to gain from `previous_response_id` and no
/// reason to open a second socket alongside the live one.
pub fn responses_model_compact_provider(
    base_url: &str,
    api_key: &str,
    compact_model: &str,
    reasoning_effort: ReasoningEffort,
    service_tier: Option<ServiceTierHandle>,
    refresher: Option<Arc<dyn TokenRefresher>>,
) -> Arc<dyn rebon_api::CompactProvider> {
    let mut config = OpenAiResponsesClientConfig::with_api_key(api_key);
    if !base_url.is_empty() {
        config.base_url = base_url.to_string();
    }
    config.service_tier = service_tier;
    let provider = OpenAiResponsesProvider::new(config);
    let provider = match refresher {
        Some(refresher) => provider.with_refresher(refresher),
        None => provider,
    };
    let client: Arc<dyn ModelClient> = Arc::new(UniversalModelClient::new(Arc::new(provider)));
    Arc::new(
        rebon_api::ModelCompactProvider::new(client, compact_model)
            .with_reasoning_effort(reasoning_effort),
    )
}

/// Build a [`CompactProvider`] matching the provider format.
///
/// - **OpenAI Responses** → [`RemoteCompactProvider`] calling
///   `/responses/compact`.
/// - **Anthropic** / **OpenAI Compatible** → [`ModelCompactProvider`] calling
///   the configured `small` profile model for summarisation.
/// - **External plugin** → `None` here; the runtime wires
///   [`external_model_compact_provider`] once the plugin client exists.
pub fn build_compact_provider(
    config_dir: &Path,
    resolved: &ResolvedProvider,
    small_model: &str,
    service_tier: Option<ServiceTierHandle>,
) -> Option<Arc<dyn rebon_api::CompactProvider>> {
    if resolved.provider_selection.is_external() {
        return None;
    }
    match resolved.format {
        ProviderFormat::OpenaiResponses => {
            let compact_model = compact_model_for_provider(small_model, &resolved.model);
            let provider = rebon_api::RemoteCompactProvider::new(
                reqwest::Client::new(),
                &resolved.base_url,
                &resolved.api_key,
                &compact_model,
            );
            let provider = if let Some(handle) = service_tier.clone() {
                provider.with_service_tier(handle)
            } else {
                provider
            };
            let provider = match compact_token_refresher(config_dir, resolved) {
                Some(refresher) => provider.with_refresher(refresher),
                None => provider,
            };
            Some(Arc::new(provider))
        }
        ProviderFormat::Anthropic | ProviderFormat::Openai => {
            build_model_compact_provider(config_dir, resolved, small_model, service_tier)
        }
    }
}

fn build_model_compact_provider(
    config_dir: &Path,
    resolved: &ResolvedProvider,
    small_model: &str,
    service_tier: Option<ServiceTierHandle>,
) -> Option<Arc<dyn rebon_api::CompactProvider>> {
    if resolved.provider_selection.is_external() {
        return None;
    }
    let compact_model = compact_model_for_provider(small_model, &resolved.model);
    let reasoning_effort = compact_reasoning_effort_for_provider(resolved);
    match resolved.format {
        ProviderFormat::Anthropic => {
            let mut config = AnthropicClientConfig::with_api_key(resolved.api_key.clone());
            if !resolved.base_url.is_empty() {
                config.base_url = resolved.base_url.clone();
            }
            let client: Arc<dyn ModelClient> = Arc::new(anthropic_client(config));
            Some(Arc::new(
                rebon_api::ModelCompactProvider::new(client, &compact_model)
                    .with_reasoning_effort(reasoning_effort),
            ))
        }
        ProviderFormat::OpenaiResponses => Some(responses_model_compact_provider(
            &resolved.base_url,
            &resolved.api_key,
            &compact_model,
            reasoning_effort,
            service_tier,
            compact_token_refresher(config_dir, resolved),
        )),
        ProviderFormat::Openai => {
            let mut config = OpenAiCompatibleClientConfig::with_api_key(resolved.api_key.clone());
            if !resolved.base_url.is_empty() {
                config.base_url = resolved.base_url.clone();
            }
            config.extra_headers = resolved.extra_headers.clone();
            config.request_options = resolved.request_options.clone();
            config.model_request_options = resolved.model_request_options.clone();
            config.service_tier = service_tier;
            let client: Arc<dyn ModelClient> = Arc::new(openai_compatible_client(config));
            Some(Arc::new(
                rebon_api::ModelCompactProvider::new(client, &compact_model)
                    .with_reasoning_effort(reasoning_effort),
            ))
        }
    }
}

/// The rung below [`build_compact_provider`]: what runs when the server-side
/// `/responses/compact` endpoint is missing or refuses. The ChatGPT Codex
/// backend does not serve `/compact` at all, so on an OAuth session this rung
/// is not a fallback but *the* compaction path — if it fails too, the engine
/// blind-truncates the history down to the protected tail and the turn loses
/// everything it had learned.
pub fn build_compact_fallback_provider(
    config_dir: &Path,
    resolved: &ResolvedProvider,
    small_model: &str,
    service_tier: Option<ServiceTierHandle>,
) -> Option<Arc<dyn rebon_api::CompactProvider>> {
    if resolved.provider_selection.is_external() {
        return None;
    }
    match resolved.format {
        ProviderFormat::OpenaiResponses => {
            build_model_compact_provider(config_dir, resolved, small_model, service_tier)
        }
        ProviderFormat::Anthropic | ProviderFormat::Openai => None,
    }
}

fn build_env_model_compact_provider(
    format: &ProviderFormat,
    small_model: &str,
    service_tier: Option<ServiceTierHandle>,
) -> Option<Arc<dyn rebon_api::CompactProvider>> {
    match format {
        ProviderFormat::Anthropic => {
            let api_key = std::env::var("ANTHROPIC_API_KEY").ok()?;
            if api_key.is_empty() {
                return None;
            }
            let client: Arc<dyn ModelClient> = Arc::new(anthropic_client(
                AnthropicClientConfig::with_api_key(api_key),
            ));
            Some(Arc::new(rebon_api::ModelCompactProvider::new(
                client,
                small_model,
            )))
        }
        ProviderFormat::Openai | ProviderFormat::OpenaiResponses => {
            let mut config = match std::env::var("DEEPSEEK_API_KEY") {
                Ok(api_key) if !api_key.is_empty() => {
                    let mut config = OpenAiCompatibleClientConfig::with_base_url(
                        std::env::var("DEEPSEEK_BASE_URL")
                            .unwrap_or_else(|_| "https://api.deepseek.com".to_string()),
                        api_key,
                    );
                    config.request_options = deepseek_request_options_from_env();
                    config
                }
                _ => {
                    let api_key = std::env::var("OPENAI_API_KEY").ok()?;
                    if api_key.is_empty() {
                        return None;
                    }
                    if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
                        OpenAiCompatibleClientConfig::with_base_url(base_url, api_key)
                    } else {
                        OpenAiCompatibleClientConfig::with_api_key(api_key)
                    }
                }
            };
            config.model_request_options = Default::default();
            config.service_tier = service_tier;
            let client: Arc<dyn ModelClient> = Arc::new(openai_compatible_client(config));
            Some(Arc::new(rebon_api::ModelCompactProvider::new(
                client,
                small_model,
            )))
        }
    }
}

pub fn build_env_compact_fallback_provider(
    format: &ProviderFormat,
    small_model: &str,
    service_tier: Option<ServiceTierHandle>,
) -> Option<Arc<dyn rebon_api::CompactProvider>> {
    match format {
        ProviderFormat::OpenaiResponses => {
            build_env_model_compact_provider(format, small_model, service_tier)
        }
        ProviderFormat::Anthropic | ProviderFormat::Openai => None,
    }
}

pub fn build_env_compact_provider(
    format: &ProviderFormat,
    small_model: &str,
    service_tier: Option<ServiceTierHandle>,
) -> Option<Arc<dyn rebon_api::CompactProvider>> {
    match format {
        ProviderFormat::OpenaiResponses => None,
        ProviderFormat::Anthropic | ProviderFormat::Openai => {
            build_env_model_compact_provider(format, small_model, service_tier)
        }
    }
}

// ---------------------------------------------------------------------------
// Env-var fallback path (kept for CI / fresh environments)
// ---------------------------------------------------------------------------

/// Resolve the default model string from env vars.
///
/// Provider-specific fallback:
/// - Anthropic (`ANTHROPIC_API_KEY` set) → `claude-sonnet-4-6`
/// - DeepSeek (`DEEPSEEK_API_KEY` set) → `deepseek-flash`
/// - otherwise → `gpt-4o`
///
/// `REBON_MODEL` overrides the fallback in every case; the DeepSeek
/// branch falls back to `DEEPSEEK_MODEL` before its own default.
pub fn default_model() -> String {
    if std::env::var("ANTHROPIC_API_KEY")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        std::env::var("REBON_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string())
    } else if std::env::var("DEEPSEEK_API_KEY")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        std::env::var("REBON_MODEL")
            .or_else(|_| std::env::var("DEEPSEEK_MODEL"))
            .unwrap_or_else(|_| "deepseek-flash".to_string())
    } else {
        std::env::var("REBON_MODEL").unwrap_or_else(|_| "gpt-4o".to_string())
    }
}

pub fn fallback_provider_format_from_env() -> ProviderFormat {
    if std::env::var("ANTHROPIC_API_KEY")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        ProviderFormat::Anthropic
    } else {
        ProviderFormat::Openai
    }
}

/// Projects root directory used for transcript storage.
pub fn projects_root() -> std::path::PathBuf {
    rebon_session::default_projects_root()
}

/// Resolve the sandbox this session's shell commands run under.
///
/// Shared by every entrypoint that builds an executor — the TUI, the ACP
/// server, and the headless harness — because an editor-driven session runs
/// the same tools against the same workspace, and a `sandbox` block that only
/// bound in one of them would be a setting the user could not tell was off.
///
/// The answer comes off the `session-sandbox` seat rather than from a direct
/// call, so `plugins.sandbox.enabled = false` means what it says: no
/// provider, no sandbox, and `Bash` spawns the argv it built. Every caller
/// here runs after the kernel has booted, so the seat is a fair question to
/// ask — unlike `rebon remote …`, whose subcommands are parsed before a
/// registry exists.
///
/// # The one combination that refuses
///
/// `sandbox.enabled = true` in settings with this plugin switched off is a
/// contradiction, and the safe reading is not "run unconfined". That would be
/// the exact outcome the setting exists to prevent, and it would be
/// invisible: every command would succeed. So the session gets a sandbox that
/// refuses everything and says why, and `/doctor` carries the same sentence.
pub fn resolve_command_sandbox(
    cwd: &std::path::Path,
) -> Option<Arc<dyn rebon_tool::CommandSandbox>> {
    let ctx = kernel_bootstrap::process_kernel().context().clone();
    command_sandbox_from(
        ctx.get::<rebon_tool::SessionSandboxService>().as_deref(),
        cwd,
    )
}

/// The decision, with the kernel and the settings files supplied.
///
/// Split out because the branch worth testing is the one where the seat is
/// empty, and a process kernel is booted once for the whole test binary —
/// whichever test ran first would decide what every later one saw.
fn command_sandbox_from(
    source: Option<&dyn rebon_tool::SessionSandboxSource>,
    cwd: &std::path::Path,
) -> Option<Arc<dyn rebon_tool::CommandSandbox>> {
    if let Some(source) = source {
        return source.for_session(cwd);
    }
    if !rebon_config::sandbox_enabled_in_settings(&rebon_config::config_home_dir(), cwd) {
        return None;
    }
    tracing::warn!(
        "sandbox.enabled is true but the sandbox plugin is disabled; shell commands will be \
         refused rather than run unconfined"
    );
    Some(Arc::new(rebon_tool::RefusingSandbox::new(
        SANDBOX_PLUGIN_DISABLED,
    )))
}

/// Said in exactly two places — the tool refusal and the `/doctor` row — and
/// written once so they cannot drift apart.
pub const SANDBOX_PLUGIN_DISABLED: &str =
    "sandbox is enabled in settings but the sandbox plugin is disabled \
     (plugins.sandbox.enabled = false); enable the plugin or set sandbox.enabled = false";

/// The sandbox section of `/doctor`, off the same seat.
///
/// With no provider there is nothing to probe, so the report is one line: the
/// contradiction above if the settings ask for a sandbox, and otherwise the
/// unremarkable fact that a feature is switched off.
pub fn sandbox_doctor(
    cwd: &std::path::Path,
    settings_overrides: &[String],
) -> rebon_tool::SandboxDoctor {
    let ctx = kernel_bootstrap::process_kernel().context().clone();
    sandbox_doctor_from(
        ctx.get::<rebon_tool::SessionSandboxService>().as_deref(),
        cwd,
        settings_overrides,
    )
}

fn sandbox_doctor_from(
    source: Option<&dyn rebon_tool::SessionSandboxSource>,
    cwd: &std::path::Path,
    settings_overrides: &[String],
) -> rebon_tool::SandboxDoctor {
    if let Some(source) = source {
        return source.doctor(cwd, settings_overrides);
    }
    let enabled_in_settings =
        rebon_config::sandbox_enabled_in_settings(&rebon_config::config_home_dir(), cwd);
    let line = if enabled_in_settings {
        rebon_tool::DoctorLine::warning(
            "sandbox",
            "plugin disabled (plugins.sandbox.enabled = false) while sandbox.enabled is true; \
             Bash / PowerShell will refuse to run",
        )
    } else {
        rebon_tool::DoctorLine::ok("sandbox", "plugin disabled")
    };
    rebon_tool::SandboxDoctor {
        enabled_in_settings,
        lines: vec![line],
    }
}

/// Build a default [`PolicyStore`] from user, project, local and env rules.
///
/// Rules come from `REBON_ALLOW_RULES` / `REBON_DENY_RULES` as
/// comma-separated lists (`Bash(ls:*),Read(/tmp/**)`). The store is
/// handed to the `EngineQueryExecutor`, which wires it into the
/// per-call permission broker chain so the rules broker sits
/// **outside** the ACP reverse-RPC broker (matching allow rules
/// short-circuit the client prompt entirely).
///
/// Missing settings files are optional; unreadable or malformed policy files
/// fail session construction rather than silently dropping deny rules.
pub fn build_default_policy_store(cwd: &Path) -> anyhow::Result<PolicyStore> {
    let policy_store = PolicyStore::new();
    if let Ok(raw) = std::env::var("REBON_ALLOW_RULES") {
        for piece in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            policy_store.allow(piece, PermissionRuleSource::CliArg);
        }
    }
    if let Ok(raw) = std::env::var("REBON_DENY_RULES") {
        for piece in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            policy_store.deny(piece, PermissionRuleSource::CliArg);
        }
    }
    for (path, source) in [
        (
            rebon_session::config_home::default_config_home_dir().join("settings.json"),
            PermissionRuleSource::UserSettings,
        ),
        (
            cwd.join(".rebon").join("settings.json"),
            PermissionRuleSource::ProjectSettings,
        ),
        (
            cwd.join(".rebon").join("settings.local.json"),
            PermissionRuleSource::LocalSettings,
        ),
    ] {
        let rules = rebon_config::permission_rules_from_settings(&path)?;
        for rule in rules.allow {
            policy_store.allow(&rule, source);
        }
        for rule in rules.deny {
            policy_store.deny(&rule, source);
        }
    }
    tracing::info!(
        rules = policy_store.len(),
        "rebon: loaded stored permission rules"
    );
    Ok(policy_store)
}

/// Bind ACP's two live-policy consumers to the same session-owned store.
/// Sessions snapshot settings at activation, before an idle peer can inherit a
/// newly persisted grant. Restored external-agent records can also initialize
/// here without first entering the local engine. Each record owns one policy
/// (including load failure); turn brokers retain clones for same-turn grants.
/// No store is keyed by cwd alone and no accepted grant is shared across ids.
pub fn with_acp_session_policies(
    handler: rebon_acp::DefaultHandler,
) -> (
    rebon_acp::DefaultHandler,
    rebon_core::policy::PolicyStoreResolver,
) {
    let state = handler.state().clone();
    let resolve: rebon_core::policy::PolicyStoreResolver = Arc::new(move |sid, cwd| {
        state
            .session_permission_policy(sid, cwd, || {
                build_default_policy_store(Path::new(cwd)).map_err(|error| format!("{error:#}"))
            })
            .map(|store| store.as_ref().clone())
    });
    let init_resolver = resolve.clone();
    let mut handler = handler;
    handler.session_policy_initializer = Some(Arc::new(move |sid, cwd| {
        init_resolver(sid, cwd).map(|_| ())
    }));
    let apply_resolver = resolve.clone();
    let handler = handler.with_allow_always_applier(Arc::new(move |sid, cwd, rules| {
        let store = apply_resolver(sid, cwd)?;
        for rule in rules {
            store.try_push_rule(rebon_permissions::types::PermissionRule {
                source: PermissionRuleSource::ProjectSettings,
                rule_behavior: rebon_permissions::types::PermissionBehavior::Allow,
                rule_value: rule.clone(),
            })?;
        }
        Ok(())
    }));
    (handler, resolve)
}

/// Build a default [`ToolFilter`] from env vars.
///
/// `REBON_ALLOW_TOOLS` — comma-separated allow list (e.g.
/// `Read,Grep,Glob`).
/// `REBON_DENY_TOOLS` — comma-separated deny list (e.g.
/// `Bash,PowerShell`).
/// `REBON_COORDINATOR_MODE` — read by the caller and passed in as
/// `coordinator_mode`, not read here: when truthy, the base filter is the
/// coordinator-safe allow list, with the env-supplied allow/deny
/// intersected on top. When unset, the base filter is the normal-session
/// filter, which hides queue and coordinator-only tools.
///
/// All three are optional; any combination composes cleanly via
/// [`ToolFilter::intersect`].
pub fn build_default_tool_filter(coordinator_mode: bool) -> ToolFilter {
    build_default_tool_filter_for_context(coordinator_mode, false)
}

/// [`build_default_tool_filter`] for a session that may be a queue session,
/// whose base filter admits the queue tools a normal session hides.
pub fn build_default_tool_filter_for_context(
    coordinator_mode: bool,
    queue_session: bool,
) -> ToolFilter {
    let mut filter = rebon_core::coordinator_mode::session_filter(coordinator_mode, queue_session);
    if let Ok(raw) = std::env::var("REBON_ALLOW_TOOLS") {
        let entries: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !entries.is_empty() {
            filter = filter.intersect(&ToolFilter::allow_only(entries));
        }
    }
    if let Ok(raw) = std::env::var("REBON_DENY_TOOLS") {
        let entries: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !entries.is_empty() {
            filter = filter.with_deny(entries);
        }
    }
    filter
}

/// Detect the active shell name for the system prompt.
pub fn detect_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|s| {
            if s.contains("zsh") {
                Some("zsh".to_string())
            } else if s.contains("bash") {
                Some("bash".to_string())
            } else {
                Some(s)
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Best-effort OS version string.
///
/// Detected once per process: the answer does not change, and every
/// session build (a `/new`, every worker) used to pay for it again.
pub fn detect_os_version() -> String {
    static OS_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OS_VERSION.get_or_init(detect_os_version_uncached).clone()
}

fn detect_os_version_uncached() -> String {
    // On Windows, std::env::consts::OS is "windows" — use the
    // OS_VERSION env var or a Command fallback.
    if cfg!(target_os = "windows") {
        // Try `ver` or read from registry-set env vars
        if let Ok(v) = std::env::var("OS") {
            // Windows typically sets OS=Windows_NT. Combine with
            // version info if available.
            let version = sys_info_windows();
            if version.is_empty() {
                return v;
            }
            return version;
        }
    }
    // Unix: equivalent of `uname -sr`
    std::process::Command::new("uname")
        .args(["-sr"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

#[cfg(target_os = "windows")]
fn sys_info_windows() -> String {
    // Ask the kernel first: `RtlGetVersion` answers in microseconds and,
    // unlike `GetVersionExW`, is not shimmed by the manifest's
    // compatibility settings. Spawning `cmd /C ver` for the same digits
    // cost 40–60 ms per session build.
    if let Some(version) = windows_version_from_ntdll() {
        return format!("Microsoft Windows [Version {version}]");
    }
    // `cmd /C ver` gives e.g. `Microsoft Windows [Version 10.0.19044.5737]`.
    std::process::Command::new("cmd")
        .args(["/C", "ver"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// `major.minor.build` from `ntdll!RtlGetVersion`, the one version API
/// Windows does not lie to depending on the application manifest.
#[cfg(target_os = "windows")]
fn windows_version_from_ntdll() -> Option<String> {
    #[repr(C)]
    struct OsVersionInfoW {
        os_version_info_size: u32,
        major_version: u32,
        minor_version: u32,
        build_number: u32,
        platform_id: u32,
        csd_version: [u16; 128],
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(version_information: *mut OsVersionInfoW) -> i32;
    }
    let mut info = OsVersionInfoW {
        os_version_info_size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major_version: 0,
        minor_version: 0,
        build_number: 0,
        platform_id: 0,
        csd_version: [0; 128],
    };
    // SAFETY: `info` is a correctly sized, writable OSVERSIONINFOW with its
    // size field set, which is the whole contract of `RtlGetVersion`.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status != 0 || info.major_version == 0 {
        return None;
    }
    Some(format!(
        "{}.{}.{}",
        info.major_version, info.minor_version, info.build_number
    ))
}

#[cfg(not(target_os = "windows"))]
fn sys_info_windows() -> String {
    String::new()
}

/// Map a model ID to its marketing name.
pub fn model_marketing_name(model: &str) -> Option<String> {
    if model.contains("claude-opus-4-6") {
        Some("Claude Opus 4.6 (1M context)".to_string())
    } else if model.contains("claude-sonnet-4-6") {
        Some("Claude Sonnet 4.6".to_string())
    } else if model.contains("claude-opus-4-5") {
        Some("Claude Opus 4.5".to_string())
    } else if model.contains("claude-haiku-4") {
        Some("Claude Haiku 4.5".to_string())
    } else if let Some(base) = rebon_api::split_pro_model_alias(model) {
        Some(match base {
            "gpt-5.6" | "gpt-5.6-sol" => "GPT-5.6 Pro".to_string(),
            "gpt-5.6-terra" => "GPT-5.6 Terra Pro".to_string(),
            "gpt-5.6-luna" => "GPT-5.6 Luna Pro".to_string(),
            other => format!("{other} (pro)"),
        })
    } else {
        None
    }
}

pub fn knowledge_cutoff(model: &str) -> Option<String> {
    if model.contains("claude-sonnet-4-6") {
        Some("August 2025".to_string())
    } else if model.contains("claude-opus-4-6") || model.contains("claude-opus-4-5") {
        Some("May 2025".to_string())
    } else if model.contains("claude-haiku-4") {
        Some("February 2025".to_string())
    } else if model.contains("claude-opus-4") || model.contains("claude-sonnet-4") {
        Some("January 2025".to_string())
    } else {
        None
    }
}

// ==============
// build_headless_session — the in-process embedder constructor (v1: lean)
// ==============

/// A fully-wired, ready-to-drive in-process agent session. The embedder spawns
/// turns on `executor.execute(PromptRequest)`, drains `update_rx` for streaming
/// `SessionUpdate`s, and answers `permission_rx` queries via their
/// `response_tx`.
///
/// The embedder MUST hold [`Self::server_state`] for the whole session
/// lifetime: it owns the session's active lock, and dropping it lets a resume
/// collide or a second writer attach. (The lock used to be a field here; it
/// moved onto the state so that one object answers "who owns this session" for
/// every surface.)
pub struct HeadlessSession {
    pub engine: Arc<Engine>,
    /// Turn entry point. Forwards to [`Self::backend`] — hold this
    /// when all you need is "run a turn".
    pub executor: Arc<dyn PromptExecutor>,
    /// The agent behind this session. Hold this to ask what the agent
    /// can do ([`AgentBackend::capabilities`]) or to reach it out of
    /// band (cancel, close).
    pub backend: Arc<dyn AgentBackend>,
    pub client: Arc<dyn ModelClient>,
    pub update_publisher: Arc<dyn SessionUpdatePublisher>,
    pub update_rx: UnboundedReceiver<SessionUpdateParams>,
    pub permission_rx: UnboundedReceiver<OutboundPermissionQuery>,
    pub permission_broker: SharedChannelPermissionBroker,
    pub server_state: Arc<rebon_acp::ServerState>,
    /// Session-scoped kernel context: session services (tool-registry, …)
    /// live on it. The matching lease below keeps this exact generation
    /// admitted while callers use the compatibility context handle.
    pub kernel_ctx: rebon_kernel::Context,
    /// Whole-session holder for the exact scope generation exposed through
    /// `kernel_ctx`. Per-turn brokers acquire their own logical holders; this
    /// one prevents the raw compatibility context from being evicted while the
    /// headless session remains alive.
    pub kernel_scope: rebon_kernel_seats::kernel_services::SessionKernelScopeLease,
    pub session_id: String,
    pub projects_root: PathBuf,
    pub cwd: String,
    /// The resolved model and provider this session runs on. Shared shape
    /// with the CLI's session handle; see [`SessionModel`].
    pub model: SessionModel,
    /// The policy-event handle this session's turns raise events on: the
    /// process seat plus this session's own settings + plugin hooks.
    ///
    /// The same handle the executor gates tool calls with and the sub-agent
    /// spawner passes down, exposed because the events an embedder owns —
    /// `UserPromptSubmit` before it submits, `Stop` when it stops — have no
    /// trigger point inside the engine. Mirrors the CLI's
    /// `SessionRuntime::policy`.
    pub policy: rebon_core::policy_seat::PolicySources,
}

/// Construct an in-process agent session: resolve the provider + model client,
/// build the engine + executor, mint (or resume) a session on disk, and return
/// the typed permission broker + update publisher the embedder bridges to its UI.
///
/// v1 is deliberately lean: teams / MCP / skills / cron are OFF. Sub-agents
/// follow `overrides.sub_agents_enabled`, and the session's turns run the
/// hooks the user configured — both were once off here for want of a wire,
/// not by policy.
pub async fn build_headless_session(
    overrides: HarnessOverrides,
) -> anyhow::Result<HeadlessSession> {
    // 0-3. Permission policy, tool filter, merged agent registry, engine and
    //      the model client: one implementation, in `session_assembly`. The
    //      registry reaches its process-wide cell before the engine lists its
    //      tools, which `fix_tools()` makes a compile-time fact rather than a
    //      comment; the process-wide plugin kernel is booted there too.
    //
    //      This surface declares no plugin agent directories and cannot spawn
    //      external agents -- it has no sub-agent pool to spawn them into --
    //      and v1 keeps the coordinator worktree off.
    let sub_agents_enabled = overrides.sub_agents_enabled;
    let coordinator_use_worktree = false;
    let (assembly, engine, runtime, runtime_resolved) = session_assembly::SessionAssembly::begin(
        overrides,
        session_assembly::AssemblyInputs {
            policy_loading: session_assembly::PolicyLoading::SessionCwd,
            queue_session: false,
            agent_dirs: &[],
            external_agents_spawnable: false,
            coordinator_use_worktree,
        },
    )?
    .fix_tools()
    .resolve_runtime()
    .await?
    .into_parts();
    // The sub-agent switch stays here rather than in `fix_tools()`: this
    // surface reads it from the caller's flag, while the TUI and the ACP
    // server read the saved setting much later, and moving any of them would
    // change when the switch takes effect.
    rebon_tool::set_sub_agents_enabled(sub_agents_enabled);
    let session_assembly::SessionAssembly {
        overrides,
        policy_store,
        tool_filter,
        ..
    } = assembly;
    let policy_store = policy_store.expect("headless assembly loads session policy");
    let cwd = overrides.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let session_model = SessionModel::from_runtime(&runtime);
    let client = runtime.client;
    let model = runtime.model;
    let prune_level = runtime.prune_level;
    let runtime_model = runtime.runtime_model;
    let title_model = runtime.title_model;
    let system_prompt_snapshot = runtime.system_prompt_snapshot;
    let model_profiles = runtime.model_profiles;
    let service_tier = runtime.service_tier;

    // 4. System prompt config (static now; dynamic per-turn inside execute()).
    let deferred_tool_names = if rebon_tool::is_tool_search_enabled() {
        engine.deferred_tool_names()
    } else {
        Vec::new()
    };
    let tool_names_for_prompt: Vec<String> = engine
        .eager_tool_snapshots()
        .into_iter()
        .map(|s| s.name)
        .collect();
    let system_prompt_config = rebon_core::system_prompt::SystemPromptConfig {
        model: model.clone(),
        model_marketing_name: model_marketing_name(&model),
        knowledge_cutoff: knowledge_cutoff(&model),
        tool_names: tool_names_for_prompt,
        deferred_tool_names,
        platform: std::env::consts::OS.to_string(),
        shell: detect_shell(),
        os_version: detect_os_version(),
        language: rebon_config::saved_language(),
        normal_system_prompt_override: rebon_config::saved_system_prompt_overrides().normal,
        minimal_system_prompt_override: rebon_config::saved_system_prompt_overrides().minimal,
        chat_system_prompt_override: None,
        // Headless sessions have no idle loop that could start a turn when
        // a background agent completes; completion notifications ride along
        // with the next user-initiated turn instead.
        auto_continue_background_agents: false,
    };

    // 5. Server state + session (new or resume). The state owns the session:
    //    it takes the active lock when it mints or loads one, and refuses a
    //    session another process is writing.
    let server_state = Arc::new(rebon_acp::ServerState::new());
    server_state.enable_session_ownership(projects_root());
    let session_id = if let Some(resume_id) = overrides.resume_session_id.as_deref() {
        server_state
            .load_session(&projects_root(), resume_id, &cwd, Some(&cwd), Vec::new())
            .map_err(|e| anyhow::anyhow!("failed to resume session {resume_id}: {}", e.message))?
            .id
    } else {
        server_state.create_session(cwd.clone(), Vec::new()).id
    };
    if !server_state.owns_session(&session_id) {
        anyhow::bail!(
            "session {session_id} could not be claimed; its active lock was not acquired"
        );
    }
    apply_session_permission_mode_override(&server_state, &session_id, overrides.permission_mode);
    let _ = server_state.set_session_mode(&session_id, "normal");
    let _ = rebon_session::save_session_mode(&projects_root(), &cwd, &session_id, "normal");
    // NOTE: process-global; v1 supports a single live session at a time.
    std::env::set_var("REBON_SESSION_ID", &session_id);

    // 6. Kernel composition + session scopes, then the typed permission broker
    //    (bypasses ACP JSON-RPC) and the update publisher.
    //
    //    Ungated: the plane needs no embedded runtime, and a build without one
    //    still has kernel plugins to boot. A headless session has nobody
    //    standing in front of it, so a refusal goes to the log the app
    //    collects — the desktop has no surface of its own for this yet.
    //
    //    The broker is now built after the scopes rather than before them.
    //    Nothing between the two used to touch it -- only the auto-mode hooks
    //    and the classifier were installed there, and both are installed here
    //    instead, still before the broker is shared behind an `Arc`.
    let (bound, composition_refusal) = runtime_resolved
        .bind_session(&engine, runtime_model.clone(), session_id.clone(), None)
        .await;
    if let Some(refusal) = composition_refusal {
        tracing::warn!(detail = %refusal.message(), "kernel plugins unavailable");
    }
    // Keep a whole-session lease because `HeadlessSession::kernel_ctx` is a
    // public compatibility handle; each prompt also gets one independently
    // acquired turn lease through the broker resolver, so provider calls and
    // detached work cannot outlive or cross a scope reincarnation.
    let kernel_scope = bound.kernel_scopes.acquire(&session_id);
    let kernel_ctx = kernel_scope.context().clone();
    let session_plugin_tools = bound.kernel_scopes.plugin_tools();
    // ExitPlanMode's Auto choice lands in the session record
    // (`notify_plan_mode_tool` → `ServerState::set_permission_mode`).
    // Give the broker a mode provider that reads that record live, so the
    // selected mode actually drives permission resolution instead of only
    // being displayed. Without hooks the broker's `is_auto()` is
    // permanently false and every tool keeps prompting under the old mode.
    let auto_mode_hooks = {
        use rebon_permissions::denial_sink::{AutoModeHooks, NullDenialSink};
        AutoModeHooks::new(
            Arc::new(NullDenialSink),
            session_record_permission_mode_provider(server_state.clone(), session_id.clone()),
        )
    };
    let (permission_broker, permission_rx) = bound.build_permissions(Some(auto_mode_hooks));
    // The policy-event handle, built here rather than left at its empty
    // default. `rebon exec` and the desktop app used to be the two surfaces
    // that ran none of the user's hooks — not by policy, but because neither
    // ever built a handle to run them on — while the same `settings.json`
    // gated every TUI turn. Built before the spawner, because a sub-agent
    // inherits this session's subscribers.
    let policy = bound.build_policy_sources(&cwd, overrides.plugin_hooks.clone());
    let (update_publisher, update_rx) = ChannelSessionUpdatePublisher::new();
    let update_publisher: Arc<dyn SessionUpdatePublisher> = Arc::new(update_publisher);

    // 7. The executor (minimal extension set for v1).
    let executor = EngineQueryExecutor::new(
        engine.clone(),
        client.clone(),
        projects_root(),
        model.clone(),
    )
    .with_system_prompt_config(system_prompt_config)
    .with_system_prompt_snapshot(system_prompt_snapshot)
    .with_title_model(title_model)
    .with_policy_store(policy_store)
    .with_tool_filter(tool_filter)
    .with_server_state(server_state.clone())
    .with_prune_level(prune_level)
    .with_shared_runtime_model(runtime_model.clone())
    .with_capability_mode(overrides.capability_mode)
    .with_policy(policy.clone())
    .with_permission_broker(permission_broker.clone() as Arc<dyn rebon_tool::PermissionBroker>);
    // Sandbox, on the same terms as the TUI and ACP paths. A headless
    // run executes the same `Bash` / `PowerShell` tools against the
    // same workspace, so a `sandbox` block that bound only in the
    // interactive paths would leave the one surface most likely to be
    // scripted as the one that is not confined.
    let executor = match resolve_command_sandbox(Path::new(&cwd)) {
        Some(sandbox) => executor.with_command_sandbox(sandbox),
        None => executor,
    };
    // Sub-agent delegation, on the same terms as every other surface.
    //
    // This seam was simply never connected here. `rebon exec` passed
    // `sub_agents_enabled: false` from the day it was written with no
    // comment, and the flag was telling the truth:
    // with no spawner in the context, `Agent` would have failed at call
    // time. But the flag is a *user setting* that defaults to on
    // ("users opt out via the settings toggle, not opt in" —
    // `rebon_tool::agent`), so what looked like a policy for headless
    // runs was a missing wire. On Terminal-Bench 4.0 the model spent 82 `ToolSearch`
    // calls hunting for `Agent`, `Explore` and `TeamCreate` — it was
    // looking for something that should have been there.
    //
    // Everything the spawner needs was already in scope. The base filter
    // is the same mode-aware one the ACP path builds, so a sub-agent
    // spawned from a headless turn sees exactly the tools it would see
    // from an interactive one.
    let sub_agent_model_config = rebon_config::saved_sub_agent_model_config();
    let executor = match sub_agent_spawner_for_session(rebon_tool::SubAgentSpawnerRequest {
        engine: rebon_tool::SubAgentRuntimeHandle::new(Arc::downgrade(&engine)),
        task_runtime: None,
        client: client.clone(),
        default_model: model.clone(),
        model_config: sub_agent_model_config.clone(),
        model_profiles,
        model_router: model_router_for_runtime(
            runtime_model.clone(),
            service_tier,
            sub_agent_model_config,
        ),
        base_filter: Some(rebon_tool::SharedToolFilter::new(
            rebon_core::coordinator_mode::default_subagent_filter(overrides.coordinator_mode),
        )),
        coordinator_mode: None,
        coordinator_use_worktree: false,
        file_history_tracker: None,
        external_runner: None,
        // A sub-agent's first turn runs this session's hooks, tagged with the
        // agent it runs as — the same inheritance the TUI's spawner gets.
        policy: Some(rebon_tool::SubAgentRuntimeHandle::new(policy.clone())),
    }) {
        Some(spawner) => executor.with_sub_agent_spawner(spawner),
        None => executor,
    };
    // Session plugin tools: model-visible + dispatchable in
    // real sessions once a JS plugin registers them.
    let executor =
        executor.with_plugin_tools(session_plugin_tools as Arc<dyn rebon_tool::PluginToolProvider>);
    // Web provider router: stateless — consults the process
    // web seat at call time, so an unconfigured deployment stays on the
    // builtin WebSearch/WebFetch path bit for bit.
    let executor = executor
        .with_web_provider_router(rebon_kernel_seats::kernel_web_seat::KernelWebRouter::shared());
    let executor = match overrides.max_iterations {
        Some(max_iterations) => executor.with_max_iterations(max_iterations),
        None => executor,
    };
    let executor = match overrides.query_event_observer.clone() {
        Some(observer) => executor.with_query_event_observer(observer),
        None => executor,
    };
    // The engine is one agent backend among (eventually) several. Every
    // turn is dispatched through the backend so swapping in a
    // third-party agent is a swap of this one value, not a rewrite of
    // the call sites downstream.
    let engine_executor: Arc<dyn PromptExecutor> = Arc::new(executor);
    let backend: Arc<dyn AgentBackend> = Arc::new(LocalAgentBackend::new(engine_executor));
    let executor: Arc<dyn PromptExecutor> = Arc::new(BackendPromptExecutor::new(backend.clone()));

    Ok(HeadlessSession {
        engine,
        executor,
        backend,
        client,
        update_publisher,
        update_rx,
        permission_rx,
        permission_broker,
        server_state,
        kernel_ctx,
        kernel_scope,
        session_id,
        projects_root: projects_root(),
        cwd,
        model: session_model,
        policy,
    })
}

fn apply_session_permission_mode_override(
    server_state: &rebon_acp::ServerState,
    session_id: &str,
    permission_mode: Option<PermissionMode>,
) {
    if let Some(mode) = permission_mode {
        let updated = server_state.set_permission_mode(session_id, mode.as_wire());
        debug_assert!(updated, "headless session record must exist");
    }
}

/// Mode provider backed by the session record: reads
/// `ServerState::session_permission_mode` on every query so mid-session
/// changes (ExitPlanMode choosing Auto, `session/set_config_option`) are
/// visible to the broker immediately. Unknown sessions resolve to
/// `Default` — never more permissive.
fn session_record_permission_mode_provider(
    server_state: Arc<rebon_acp::ServerState>,
    session_id: String,
) -> Arc<dyn rebon_permissions::denial_sink::PermissionModeProvider> {
    Arc::new(move || {
        server_state
            .session_permission_mode(&session_id)
            .map(|mode| rebon_permissions::PermissionMode::from_wire(&mode))
            .unwrap_or(rebon_permissions::types::PermissionMode::Default)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ── The `session-sandbox` seat, when nobody is on it ──────────
    //
    // The interesting case is the empty one: `plugins.sandbox.enabled =
    // false`. What the plugin does when it *is* loaded is tested in the
    // plugin, where the decision table lives.

    /// Settings ask for a sandbox, the thing that provides one is switched
    /// off. Running the commands unconfined is the outcome the setting exists
    /// to prevent, so they are refused instead — and the refusal names both
    /// halves, because the person reading it has two knobs to choose between.
    #[test]
    fn a_disabled_plugin_with_the_setting_on_refuses_rather_than_runs_free() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sandbox-fail-closed");
        std::fs::write(
            home.path().join("settings.json"),
            r#"{"sandbox":{"enabled":true}}"#,
        )
        .unwrap();

        let sandbox = command_sandbox_from(None, home.path())
            .expect("an enabled sandbox with no provider must not be silently absent");

        let error = sandbox
            .check(&rebon_tools_core::ToolId::new("Bash"), "echo hi", false)
            .expect_err("every command is refused");
        match error {
            rebon_tools_core::ToolError::PermissionDenied { reason, .. } => {
                assert_eq!(reason, SANDBOX_PLUGIN_DISABLED);
                assert!(reason.contains("plugins.sandbox.enabled = false"));
                assert!(reason.contains("sandbox.enabled = false"));
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }

        let doctor = sandbox_doctor_from(None, home.path(), &[]);
        assert!(doctor.enabled_in_settings);
        assert_eq!(doctor.lines.len(), 1);
        assert_eq!(doctor.lines[0].level, rebon_tool::DoctorLevel::Warning);
        assert!(doctor.lines[0].message.contains("refuse to run"));
    }

    /// Both switched off is not a contradiction, it is the machine nobody
    /// configured a sandbox on: no sandbox, and `/doctor` says so plainly.
    #[test]
    fn a_disabled_plugin_with_the_setting_off_attaches_nothing() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sandbox-both-off");
        std::fs::write(
            home.path().join("settings.json"),
            r#"{"sandbox":{"enabled":false}}"#,
        )
        .unwrap();

        assert!(command_sandbox_from(None, home.path()).is_none());

        let doctor = sandbox_doctor_from(None, home.path(), &[]);
        assert!(!doctor.enabled_in_settings);
        assert_eq!(doctor.lines[0].level, rebon_tool::DoctorLevel::Ok);
        assert_eq!(doctor.lines[0].message, "plugin disabled");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_session_policy_isolated_by_real_session_cwd() {
        use rebon_acp::RequestHandler;
        use rebon_core::policy::{PolicyEvaluator, PolicyOutcome};
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-policy-cwd");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::create_dir(a.path().join(".rebon")).unwrap();
        std::fs::write(
            a.path().join(".rebon/settings.json"),
            r#"{"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();
        let (handler, resolve) = with_acp_session_policies(rebon_acp::DefaultHandler::default());
        handler
            .handle_request(
                "initialize",
                Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
            )
            .await
            .unwrap();
        let mut ids = Vec::new();
        for cwd in [a.path(), b.path()] {
            let response = handler
                .handle_request(
                    "session/new",
                    Some(serde_json::json!({"cwd":cwd,"mcpServers":[]})),
                )
                .await
                .unwrap();
            ids.push(response["sessionId"].as_str().unwrap().to_owned());
        }
        for (index, cwd) in [a.path(), b.path()].into_iter().enumerate() {
            let store = resolve(&ids[index], cwd.to_str().unwrap()).unwrap();
            let outcome =
                PolicyEvaluator::new(store).evaluate("Read", &serde_json::json!({}), None);
            if index == 0 {
                assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
            } else {
                assert_eq!(
                    outcome,
                    PolicyOutcome::PassThrough,
                    "startup project allow leaked into session B"
                );
            }
        }
    }

    async fn new_policy_session(handler: &rebon_acp::DefaultHandler, cwd: &Path) -> String {
        use rebon_acp::RequestHandler;
        handler
            .handle_request(
                "session/new",
                Some(serde_json::json!({"cwd":cwd,"mcpServers":[]})),
            )
            .await
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_reverse_rpc_grant_matches_live_and_persisted_policy() {
        use rebon_acp::RequestHandler;
        use rebon_agent_core::publisher::ChannelPermissionRequestPublisher;
        use rebon_core::policy::{PolicyEvaluator, PolicyOutcome};
        use serde_json::json;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-wire-policy");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        for (offered, persistence_fails) in [(false, false), (true, false), (true, true)] {
            let project = tempfile::tempdir().unwrap();
            let cwd = project.path().to_str().unwrap();
            let (handler, resolve) =
                with_acp_session_policies(rebon_acp::DefaultHandler::default());
            handler
                .handle_request(
                    "initialize",
                    Some(json!({"protocolVersion":1,"clientCapabilities":{}})),
                )
                .await
                .unwrap();
            let a = new_policy_session(&handler, project.path()).await;
            let b = new_policy_session(&handler, project.path()).await;
            let live = resolve(&a, cwd).unwrap();
            let evaluator = PolicyEvaluator::new(live.clone());
            let settings = project.path().join(".rebon/settings.json");
            if persistence_fails {
                std::fs::create_dir_all(&settings).unwrap();
            }
            let (publisher, permission_rx) = ChannelPermissionRequestPublisher::new();
            let (mut writer, server_in) = tokio::io::duplex(8192);
            let (server_out, reader) = tokio::io::duplex(8192);
            let mut reader = BufReader::new(reader);
            let server = tokio::spawn(rebon_acp::serve_with_publishers(
                server_in,
                server_out,
                handler,
                None,
                Some(permission_rx),
            ));
            let mut params = json!({
                "sessionId":a, "toolCall":{"toolCallId":"asker"},
                "toolName":"Bash", "toolInput":{"command":"ls -la"},
                "options":[
                    {"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
                    {"optionId":"reject_once","name":"Reject once","kind":"reject_once"}
                ]
            });
            if offered {
                params["options"].as_array_mut().unwrap().insert(
                    1,
                    json!({
                        "optionId":"allow_always","name":"Allow always","kind":"allow_always"
                    }),
                );
            }
            let params = serde_json::from_value(params).unwrap();
            let request = tokio::spawn(async move { publisher.request_permission(params).await });
            let mut line = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                reader.read_line(&mut line),
            )
            .await
            .unwrap()
            .unwrap();
            let wire: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(wire["method"], "session/request_permission");
            let response = json!({"jsonrpc":"2.0","id":wire["id"],
                "result":{"outcome":{"outcome":"selected","optionId":"allow_always"}}});
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            drop(writer);
            tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if !offered {
                assert_eq!(result.option_id.as_deref(), Some("reject_once"));
                assert!(live.is_empty(), "unoffered option created a live grant");
                assert!(!settings.exists());
                continue;
            }
            assert_eq!(
                result.option_id.as_deref(),
                Some("allow_always"),
                "wire={wire}, live snapshot={:?}",
                live.snapshot()
            );
            let expected = rebon_permissions::PermissionRuleValue::new("Bash", Some("ls -la"));
            assert_eq!(live.snapshot().len(), 1);
            assert_eq!(live.snapshot()[0].rule_value, expected);
            assert_eq!(
                evaluator.evaluate("Bash", &json!({"command":"ls -la"}), Some(cwd)),
                PolicyOutcome::AutoAllow { matched: expected }
            );
            assert_eq!(
                evaluator.evaluate("Bash", &json!({"command":"ls -l"}), Some(cwd)),
                PolicyOutcome::PassThrough
            );
            assert!(resolve(&b, cwd).unwrap().is_empty());
            if persistence_fails {
                assert!(settings.is_dir());
            } else {
                let persisted = build_default_policy_store(project.path()).unwrap();
                assert_eq!(persisted.snapshot(), live.snapshot());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_assembly_ignores_unrelated_startup_policy_but_activation_rejects_it() {
        use rebon_acp::RequestHandler;
        use serde_json::json;
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-startup-policy");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        for filename in ["settings.json", "settings.local.json"] {
            let startup = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            std::fs::create_dir(startup.path().join(".rebon")).unwrap();
            let malformed = startup.path().join(".rebon").join(filename);
            std::fs::write(&malformed, "{malformed").unwrap();
            let assembly = session_assembly::SessionAssembly::begin(
                HarnessOverrides {
                    cwd: Some(startup.path().to_string_lossy().into_owned()),
                    ..Default::default()
                },
                session_assembly::AssemblyInputs {
                    policy_loading: session_assembly::PolicyLoading::AcpSessionActivation,
                    ..Default::default()
                },
            );
            assert!(
                assembly.is_ok(),
                "unrelated startup policy blocked ACP: {:?}",
                assembly.as_ref().err()
            );
            assert!(assembly.unwrap().policy_store.is_none());
            let (handler, resolve) =
                with_acp_session_policies(rebon_acp::DefaultHandler::default());
            handler
                .handle_request(
                    "initialize",
                    Some(json!({
                        "protocolVersion":1,"clientCapabilities":{}
                    })),
                )
                .await
                .unwrap();
            let sid = new_policy_session(&handler, project.path()).await;
            assert!(resolve(&sid, project.path().to_str().unwrap())
                .unwrap()
                .is_empty());
            let rejected = handler
                .handle_request(
                    "session/new",
                    Some(json!({
                        "cwd":startup.path(), "mcpServers":[]
                    })),
                )
                .await
                .unwrap_err();
            assert!(rejected.message.contains(filename), "{rejected:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_live_policy_is_owned_by_session_even_before_local_execution() {
        use rebon_acp::RequestHandler;
        use rebon_core::policy::{PolicyEvaluator, PolicyOutcome};
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-live-policy");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().to_str().unwrap();
        let (handler, resolve) = with_acp_session_policies(rebon_acp::DefaultHandler::default());
        handler
            .handle_request(
                "initialize",
                Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
            )
            .await
            .unwrap();
        let a = new_policy_session(&handler, project.path()).await;
        let b = new_policy_session(&handler, project.path()).await;
        let grant = rebon_permissions::PermissionRuleValue {
            tool_name: "Read".into(),
            rule_content: None,
        };
        // External backends use this hook without ever executing the local engine.
        handler
            .apply_allow_always_rules(&a, &[grant.clone()])
            .unwrap();
        let first_turn = PolicyEvaluator::new(resolve(&a, cwd).unwrap());
        assert!(matches!(
            first_turn.evaluate("Read", &serde_json::json!({}), None),
            PolicyOutcome::AutoAllow { .. }
        ));
        std::fs::create_dir(project.path().join(".rebon")).unwrap();
        std::fs::write(
            project.path().join(".rebon/settings.json"),
            r#"{"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();
        // B was activated before persistence and has not yet used a local broker.
        assert_eq!(
            PolicyEvaluator::new(resolve(&b, cwd).unwrap()).evaluate(
                "Read",
                &serde_json::json!({}),
                None
            ),
            PolicyOutcome::PassThrough
        );
        assert!(resolve(&a, "wrong-cwd").is_err());
        assert!(handler
            .apply_allow_always_rules("missing", &[grant.clone()])
            .is_err());
        std::fs::remove_file(project.path().join(".rebon/settings.json")).unwrap();
        assert!(handler.state().close_session(&a));
        assert!(resolve(&a, cwd).is_err());
        // Reusing an id for a restored external session must not resurrect grants.
        handler
            .state()
            .restore_empty_session(a.clone(), cwd.into(), Vec::new(), "default");
        assert_eq!(
            PolicyEvaluator::new(resolve(&a, cwd).unwrap()).evaluate(
                "Read",
                &serde_json::json!({}),
                None
            ),
            PolicyOutcome::PassThrough
        );
        // A restored external record also accepts grants without a local turn.
        handler
            .state()
            .restore_empty_session("external".into(), cwd.into(), Vec::new(), "default");
        handler
            .apply_allow_always_rules("external", &[grant])
            .unwrap();
        assert!(matches!(
            PolicyEvaluator::new(resolve("external", cwd).unwrap()).evaluate(
                "Read",
                &serde_json::json!({}),
                None
            ),
            PolicyOutcome::AutoAllow { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_policy_denies_win_across_all_settings_sources_and_live_grants() {
        use rebon_acp::RequestHandler;
        use rebon_core::policy::{PolicyEvaluator, PolicyOutcome};
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-policy-denies");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        let project = tempfile::tempdir().unwrap();
        let settings = project.path().join(".rebon");
        std::fs::create_dir(&settings).unwrap();
        for (path, allow, deny) in [
            (home.path().join("settings.json"), "Read", "Write"),
            (settings.join("settings.json"), "Edit", "Read"),
            (settings.join("settings.local.json"), "Write", "Edit"),
        ] {
            std::fs::write(
                path,
                serde_json::json!({"permissions":{"allow":[allow],"deny":[deny]}}).to_string(),
            )
            .unwrap();
        }
        let (handler, resolve) = with_acp_session_policies(rebon_acp::DefaultHandler::default());
        handler
            .handle_request(
                "initialize",
                Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
            )
            .await
            .unwrap();
        let sid = new_policy_session(&handler, project.path()).await;
        let evaluator =
            PolicyEvaluator::new(resolve(&sid, project.path().to_str().unwrap()).unwrap());
        for name in ["Read", "Write", "Edit"] {
            handler
                .apply_allow_always_rules(
                    &sid,
                    &[rebon_permissions::PermissionRuleValue {
                        tool_name: name.into(),
                        rule_content: None,
                    }],
                )
                .unwrap();
            assert!(
                matches!(
                    evaluator.evaluate(name, &serde_json::json!({}), None),
                    PolicyOutcome::AutoDeny { .. }
                ),
                "{name}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_policy_malformed_scope_fails_closed_on_activation_and_external_grant() {
        use rebon_acp::RequestHandler;
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-policy-malformed");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().to_str().unwrap();
        let settings = project.path().join(".rebon");
        std::fs::create_dir(&settings).unwrap();
        for path in [
            home.path().join("settings.json"),
            settings.join("settings.json"),
            settings.join("settings.local.json"),
        ] {
            std::fs::write(
                &path,
                r#"{"permissions":{"allow":["Read"],"deny":[false]}}"#,
            )
            .unwrap();
            let (handler, resolve) =
                with_acp_session_policies(rebon_acp::DefaultHandler::default());
            handler
                .handle_request(
                    "initialize",
                    Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
                )
                .await
                .unwrap();
            assert!(handler
                .handle_request(
                    "session/new",
                    Some(serde_json::json!({"cwd":cwd,"mcpServers":[]}))
                )
                .await
                .is_err());
            handler.state().restore_empty_session(
                "external".into(),
                cwd.into(),
                Vec::new(),
                "default",
            );
            let grant = rebon_permissions::PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: None,
            };
            assert!(handler
                .apply_allow_always_rules("external", &[grant.clone()])
                .is_err());
            std::fs::remove_file(&path).unwrap();
            // A failed authoritative slot never silently retries into a weaker policy.
            assert!(resolve("external", cwd).is_err());
            assert!(handler
                .apply_allow_always_rules("external", &[grant])
                .is_err());
        }
    }

    #[test]
    fn session_policy_loads_user_allow_rules() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("policy-user-allow");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        std::fs::write(
            home.path().join("settings.json"),
            r#"{"permissions":{"allow":["Bash(ls:*)"]}}"#,
        )
        .expect("write user settings");
        let project = tempfile::tempdir().expect("project directory");
        let assembly = session_assembly::SessionAssembly::begin(
            HarnessOverrides {
                cwd: Some(project.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
            session_assembly::AssemblyInputs::default(),
        )
        .expect("assemble session");
        assert!(assembly
            .policy_store
            .as_ref()
            .unwrap()
            .snapshot()
            .iter()
            .any(|rule| {
                rule.source == PermissionRuleSource::UserSettings
                    && rule.rule_value.tool_name == "Bash"
            }));
        let evaluator = rebon_core::policy::PolicyEvaluator::new(assembly.policy_store.unwrap());
        assert!(matches!(
            evaluator.evaluate("Bash", &serde_json::json!({"command":"ls -la"}), None),
            rebon_core::policy::PolicyOutcome::AutoAllow { .. }
        ));
    }

    #[test]
    fn session_policy_accumulates_rules_with_each_settings_source() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("policy-scopes");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        let project = tempfile::tempdir().expect("project directory");
        let settings_dir = project.path().join(".rebon");
        std::fs::create_dir(&settings_dir).expect("project settings directory");
        for (path, allow, deny) in [
            (home.path().join("settings.json"), "Read", "Write"),
            (settings_dir.join("settings.json"), "Glob", "Edit"),
            (
                settings_dir.join("settings.local.json"),
                "Grep",
                "Bash(git push:*)",
            ),
        ] {
            std::fs::write(
                path,
                serde_json::json!({"permissions":{"allow":[allow],"deny":[deny]}}).to_string(),
            )
            .expect("write settings");
        }
        let policy = build_default_policy_store(project.path()).expect("load scoped policy");
        let rules = policy.snapshot();
        assert_eq!(rules.len(), 6);
        for (name, source) in [
            ("Read", PermissionRuleSource::UserSettings),
            ("Write", PermissionRuleSource::UserSettings),
            ("Glob", PermissionRuleSource::ProjectSettings),
            ("Edit", PermissionRuleSource::ProjectSettings),
            ("Grep", PermissionRuleSource::LocalSettings),
            ("Bash", PermissionRuleSource::LocalSettings),
        ] {
            assert!(rules
                .iter()
                .any(|rule| rule.rule_value.tool_name == name && rule.source == source));
        }
        let evaluator = rebon_core::policy::PolicyEvaluator::new(policy);
        for name in ["Read", "Glob", "Grep"] {
            assert!(matches!(
                evaluator.evaluate(name, &serde_json::json!({}), None),
                rebon_core::policy::PolicyOutcome::AutoAllow { .. }
            ));
        }
        for name in ["Write", "Edit"] {
            assert!(matches!(
                evaluator.evaluate(name, &serde_json::json!({}), None),
                rebon_core::policy::PolicyOutcome::AutoDeny { .. }
            ));
        }
        assert!(matches!(
            evaluator.evaluate(
                "Bash",
                &serde_json::json!({"command":"git push origin main"}),
                None
            ),
            rebon_core::policy::PolicyOutcome::AutoDeny { .. }
        ));
    }

    #[test]
    fn session_policy_denies_win_across_settings_and_environment() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("policy-deny-precedence");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::set_var("REBON_ALLOW_RULES", "Edit");
        std::env::set_var("REBON_DENY_RULES", "Glob");
        let project = tempfile::tempdir().expect("project directory");
        let settings_dir = project.path().join(".rebon");
        std::fs::create_dir(&settings_dir).expect("project settings directory");
        for (path, contents) in [
            (
                home.path().join("settings.json"),
                r#"{"permissions":{"allow":["Read"],"deny":["Write"]}}"#,
            ),
            (
                settings_dir.join("settings.json"),
                r#"{"permissions":{"deny":["Edit"]}}"#,
            ),
            (
                settings_dir.join("settings.local.json"),
                r#"{"permissions":{"allow":["Write","Glob"],"deny":["Read"]}}"#,
            ),
        ] {
            std::fs::write(path, contents).expect("write settings");
        }
        let policy = build_default_policy_store(project.path()).expect("load scoped policy");
        let evaluator = rebon_core::policy::PolicyEvaluator::new(policy);
        for name in ["Read", "Write", "Edit", "Glob"] {
            assert!(
                matches!(
                    evaluator.evaluate(name, &serde_json::json!({}), None),
                    rebon_core::policy::PolicyOutcome::AutoDeny { .. }
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn session_policy_keeps_environment_rules_and_ignores_empty_pieces() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("policy-env");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::set_var("REBON_ALLOW_RULES", " , Read, Bash(ls:*), , ");
        std::env::set_var("REBON_DENY_RULES", " Write, , ");
        let project = tempfile::tempdir().expect("project directory");
        let policy = build_default_policy_store(project.path()).expect("load env policy");
        let rules = policy.snapshot();
        assert_eq!(rules.len(), 3);
        assert!(rules
            .iter()
            .all(|rule| rule.source == PermissionRuleSource::CliArg));
        let evaluator = rebon_core::policy::PolicyEvaluator::new(policy);
        assert!(matches!(
            evaluator.evaluate("Bash", &serde_json::json!({"command":"ls -la"}), None),
            rebon_core::policy::PolicyOutcome::AutoAllow { .. }
        ));
        assert!(matches!(
            evaluator.evaluate("Write", &serde_json::json!({}), None),
            rebon_core::policy::PolicyOutcome::AutoDeny { .. }
        ));
    }

    #[test]
    fn session_policy_missing_settings_leave_unknown_tools_for_the_broker() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("policy-missing");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        let project = tempfile::tempdir().expect("project directory");
        let policy =
            build_default_policy_store(project.path()).expect("missing settings are optional");
        assert!(policy.is_empty());
        let evaluator = rebon_core::policy::PolicyEvaluator::new(policy);
        assert!(matches!(
            evaluator.evaluate("Read", &serde_json::json!({}), None),
            rebon_core::policy::PolicyOutcome::PassThrough
        ));
        assert!(!project.path().join(".rebon").exists());
    }

    #[test]
    fn session_policy_uses_each_explicit_session_directory() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("policy-cwd");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        for name in ["Read", "Glob"] {
            let project = tempfile::tempdir().expect("project directory");
            let settings_dir = project.path().join(".rebon");
            std::fs::create_dir(&settings_dir).expect("project settings directory");
            std::fs::write(
                settings_dir.join("settings.json"),
                serde_json::json!({"permissions":{"allow":[name]}}).to_string(),
            )
            .expect("write project settings");
            let assembly = session_assembly::SessionAssembly::begin(
                HarnessOverrides {
                    cwd: Some(project.path().to_string_lossy().into_owned()),
                    ..Default::default()
                },
                session_assembly::AssemblyInputs::default(),
            )
            .expect("assemble project session");
            assert_eq!(assembly.cwd, project.path());
            let rules = assembly.policy_store.as_ref().unwrap().snapshot();
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].rule_value.tool_name, name);
            assert_eq!(rules[0].source, PermissionRuleSource::ProjectSettings);
        }
    }

    #[test]
    fn session_policy_rejects_malformed_settings_in_every_scope() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("policy-invalid");
        let _env = EnvRestore::new(&["REBON_ALLOW_RULES", "REBON_DENY_RULES"]);
        std::env::remove_var("REBON_ALLOW_RULES");
        std::env::remove_var("REBON_DENY_RULES");
        let project = tempfile::tempdir().expect("project directory");
        let settings_dir = project.path().join(".rebon");
        std::fs::create_dir(&settings_dir).expect("project settings directory");
        for path in [
            home.path().join("settings.json"),
            settings_dir.join("settings.json"),
            settings_dir.join("settings.local.json"),
        ] {
            for contents in ["{", r#"{"permissions":{"allow":["Read"],"deny":[false]}}"#] {
                std::fs::write(&path, contents).expect("write invalid settings");
                let result = session_assembly::SessionAssembly::begin(
                    HarnessOverrides {
                        cwd: Some(project.path().to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                    session_assembly::AssemblyInputs::default(),
                );
                let error = match result {
                    Err(error) => error,
                    Ok(_) => panic!("invalid policy must fail session construction"),
                };
                assert!(
                    error.to_string().contains(
                        path.file_name()
                            .expect("settings filename")
                            .to_str()
                            .expect("UTF-8 filename")
                    ),
                    "{error}"
                );
            }
            std::fs::write(path, "{}").expect("restore valid settings before testing next scope");
        }
    }

    #[tokio::test]
    async fn external_compact_provider_summarises_through_plugin_client() {
        use rebon_api::{ContentBlock, Message, Role, TextBlock};

        struct StubClient {
            seen_model: Mutex<Option<String>>,
        }

        #[async_trait::async_trait]
        impl rebon_api::ModelClient for StubClient {
            fn provider_name(&self) -> &'static str {
                "stub-external"
            }

            async fn create_message_stream(
                &self,
                request: rebon_api::CreateMessageRequest,
            ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
                *self.seen_model.lock().unwrap() = Some(request.model.clone());
                let events: Vec<rebon_api::ModelResult<rebon_api::StreamEvent>> = vec![
                    Ok(rebon_api::StreamEvent::MessageStart {
                        message_id: "m1".into(),
                        model: request.model.clone(),
                        usage: Default::default(),
                    }),
                    Ok(rebon_api::StreamEvent::ContentBlockStart {
                        index: 0,
                        content_block: rebon_api::ContentBlockStart::Text {
                            text: String::new(),
                        },
                    }),
                    Ok(rebon_api::StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: rebon_api::ContentBlockDelta::TextDelta {
                            text: "<analysis>a</analysis>\n<summary>compact summary</summary>"
                                .into(),
                        },
                    }),
                    Ok(rebon_api::StreamEvent::ContentBlockStop { index: 0 }),
                    Ok(rebon_api::StreamEvent::MessageStop),
                ];
                Ok(Box::pin(futures_util::stream::iter(events)))
            }
        }

        let stub = Arc::new(StubClient {
            seen_model: Mutex::new(None),
        });
        let provider = external_model_compact_provider(
            stub.clone(),
            "fake-small",
            "fake-large",
            ReasoningEffort::Low,
        );

        let turn = |text: &str, role| Message {
            role,
            content: vec![ContentBlock::Text(TextBlock { text: text.into() })],
        };
        let messages = vec![
            turn("q1", Role::User),
            turn("a1", Role::Assistant),
            turn("q2", Role::User),
            turn("a2", Role::Assistant),
        ];
        let result = provider
            .compact(
                &messages,
                None,
                1,
                None,
                &rebon_api::CompactSummaryOptions::default(),
            )
            .await
            .expect("compact succeeds");

        // Compaction must route through the plugin's own client with the
        // `small` profile model — no separate HTTP client, no compact endpoint.
        assert_eq!(
            stub.seen_model.lock().unwrap().as_deref(),
            Some("fake-small")
        );
        assert!(!result.messages.is_empty());
    }

    #[test]
    fn external_compact_model_falls_back_to_provider_model() {
        assert_eq!(compact_model_for_provider("", "fake-large"), "fake-large");
        assert_eq!(
            compact_model_for_provider("fake-small", "fake-large"),
            "fake-small"
        );
    }

    fn responses_provider(base_url: String) -> ResolvedProvider {
        ResolvedProvider {
            name: "codex".into(),
            base_url,
            api_key: "sk-test".into(),
            model: "gpt-5.6-luna".into(),
            model_profiles: Default::default(),
            model_context_windows: BTreeMap::new(),
            model_output_token_limits: BTreeMap::new(),
            format: ProviderFormat::OpenaiResponses,
            vendor: Default::default(),
            provider_selection: rebon_config::ProviderSelection::BuiltIn(
                ProviderFormat::OpenaiResponses,
            ),
            oauth: None,
            use_websocket: false,
            extra_headers: Vec::new(),
            request_options: Default::default(),
            model_request_options: BTreeMap::new(),
            request_scoped_transient_context: false,
            reasoning_mode: None,
        }
    }

    /// Accept exactly one request, hand back its request line, and answer
    /// `400` — a permanent status, so `compact_with_retry` does not re-send
    /// and one accept is enough.
    async fn record_one_request_line(listener: tokio::net::TcpListener) -> String {
        let (mut stream, _peer) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 4096];
        let mut used = 0usize;
        loop {
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer[used..])
                .await
                .unwrap();
            if read == 0 {
                break;
            }
            used += read;
            if used >= 4 && buffer[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if used == buffer.len() {
                buffer.resize(buffer.len() * 2, 0);
            }
        }
        let request = String::from_utf8_lossy(&buffer[..used]).to_string();
        let _ = tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        request.lines().next().unwrap_or_default().to_string()
    }

    /// The summariser must reach the same API the session reaches. Built on
    /// the chat-completions client instead — as this rung was until
    /// 2026-08-31 — it appends `/v1/chat/completions` to a base URL destined
    /// for `/responses`, a path no backend serves. That mattered because on a
    /// ChatGPT Codex OAuth session the rung above it (`/responses/compact`)
    /// 404s as well, so a guaranteed-404 fallback left blind truncation as the
    /// only outcome: one 8-hour benchmark trial had its 690-message history
    /// cut to 11 mid-task and started over.
    #[tokio::test]
    async fn responses_compact_fallback_posts_to_the_responses_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(record_one_request_line(listener));

        let resolved = responses_provider(format!("http://{addr}/backend-api/codex"));
        let provider = build_compact_fallback_provider(Path::new("."), &resolved, "", None)
            .expect("a Responses provider has a compaction fallback");

        let messages = vec![
            rebon_api::Message::user_text("q1"),
            rebon_api::Message::assistant_text("a1"),
            rebon_api::Message::user_text("q2"),
            rebon_api::Message::assistant_text("a2"),
        ];
        let _ = provider
            .compact(
                &messages,
                None,
                1,
                None,
                &rebon_api::CompactSummaryOptions::default(),
            )
            .await;

        assert_eq!(
            server.await.unwrap(),
            "POST /backend-api/codex/responses HTTP/1.1"
        );
    }

    #[test]
    fn harness_anthropic_compaction_defaults_on_for_official_endpoint() {
        assert!(anthropic_compaction_enabled_with_override(
            "https://api.anthropic.com/v1/",
            None,
        ));
    }

    #[test]
    fn harness_anthropic_compaction_defaults_off_for_compatible_endpoint() {
        assert!(!anthropic_compaction_enabled_with_override(
            "https://relay.example.com/v1",
            None,
        ));
    }

    #[test]
    fn harness_anthropic_compaction_explicit_override_wins() {
        assert!(!anthropic_compaction_enabled_with_override(
            "https://api.anthropic.com",
            Some(false),
        ));
        assert!(anthropic_compaction_enabled_with_override(
            "https://relay.example.com",
            Some(true),
        ));
    }

    #[test]
    fn headless_permission_override_updates_session_record_for_every_runtime_mode() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
            PermissionMode::Auto,
        ] {
            let state = rebon_acp::ServerState::new();
            let record = state.create_session("/tmp/w".into(), Vec::new());

            apply_session_permission_mode_override(&state, &record.id, Some(mode));

            assert_eq!(
                state.get_session(&record.id).unwrap().permission_mode,
                mode.as_wire(),
                "mode={mode:?}"
            );
        }

        let state = rebon_acp::ServerState::new();
        let record = state.create_session("/tmp/w".into(), Vec::new());
        apply_session_permission_mode_override(&state, &record.id, None);
        assert_eq!(
            state.get_session(&record.id).unwrap().permission_mode,
            "default"
        );
    }

    #[test]
    fn session_record_mode_provider_tracks_live_permission_mode() {
        use rebon_permissions::types::PermissionMode;

        let state = Arc::new(rebon_acp::ServerState::new());
        let record = state.create_session("/tmp/w".into(), Vec::new());
        let provider = session_record_permission_mode_provider(state.clone(), record.id.clone());

        assert_eq!(provider.current_mode(), PermissionMode::Default);
        // ExitPlanMode's Auto choice writes the session record; the broker's
        // provider must observe it immediately.
        assert!(state.set_permission_mode(&record.id, "auto"));
        assert_eq!(provider.current_mode(), PermissionMode::Auto);
        assert!(state.set_permission_mode(&record.id, "acceptEdits"));
        assert_eq!(provider.current_mode(), PermissionMode::AcceptEdits);

        let unknown = session_record_permission_mode_provider(state, "missing-session".into());
        assert_eq!(unknown.current_mode(), PermissionMode::Default);
    }

    struct EnvRestore(Vec<(String, Option<OsString>)>);

    impl EnvRestore {
        fn new(names: &[&str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| ((*name).to_string(), std::env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn headless_tool_filter_honors_deny_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["REBON_ALLOW_TOOLS", "REBON_DENY_TOOLS"]);
        std::env::remove_var("REBON_ALLOW_TOOLS");
        std::env::set_var("REBON_DENY_TOOLS", "EnterPlanMode, ExitPlanMode");

        let filter = build_default_tool_filter(false);

        assert!(!filter.allows("EnterPlanMode", &[]));
        assert!(!filter.allows("ExitPlanMode", &[]));
        assert!(filter.allows("Read", &[]));
    }

    #[test]
    fn headless_tool_filter_composes_allow_and_deny_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["REBON_ALLOW_TOOLS", "REBON_DENY_TOOLS"]);
        std::env::set_var("REBON_ALLOW_TOOLS", "Read,EnterPlanMode");
        std::env::set_var("REBON_DENY_TOOLS", "EnterPlanMode");

        let filter = build_default_tool_filter(false);

        assert!(filter.allows("Read", &[]));
        assert!(!filter.allows("EnterPlanMode", &[]));
        assert!(!filter.allows("Grep", &[]));
    }

    fn clear_model_env() {
        for name in [
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_MODEL",
            "OPENAI_API_KEY",
            "REBON_MODEL",
            "REBON_MCP_SERVERS_JSON",
            "REBON_MCP_SERVERS",
        ] {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn infer_openai_context_window_covers_common_model_families() {
        assert_eq!(infer_openai_context_window("gpt-4o"), 128_000);
        assert_eq!(infer_openai_context_window("o4-mini"), 200_000);
        assert_eq!(infer_openai_context_window("gpt-5.3-codex"), 1_000_000);
        assert_eq!(infer_openai_context_window("deepseek-flash"), 200_000);
        assert_eq!(infer_openai_context_window("deepseek-v4-pro"), 1_000_000);
        assert_eq!(infer_openai_context_window("unknown-model"), 128_000);
    }

    #[test]
    fn prune_handle_uses_configured_context_limits_and_model_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["REBON_CONTEXT_WINDOW"]);
        std::env::remove_var("REBON_CONTEXT_WINDOW");
        let handle = prune_handle_from_env(
            &ProviderFormat::OpenaiResponses,
            rebon_api::ProviderVendor::OpenAi,
            "gpt-5.6-sol",
            Some(500_000),
            Some(128_000),
            [
                ("gpt-5.6-sol".to_string(), 500_000),
                ("deepseek-v4-pro[1m]".to_string(), 1_000_000),
            ],
            [("gpt-5.6-sol".to_string(), 128_000)],
        );
        assert_eq!(handle.budget.context_window(), 500_000);
        assert_eq!(handle.budget.output_token_reserve(), 128_000);
        assert_eq!(handle.budget.auto_compact_threshold(), 353_400);

        handle.set_context_window_for_model("deepseek-v4-pro[1m]");
        assert_eq!(handle.budget.context_window(), 1_000_000);
        assert_eq!(handle.budget.output_token_reserve(), 0);
        handle.set_context_window_for_model("gpt-5.6-sol");
        assert_eq!(handle.budget.context_window(), 500_000);
        assert_eq!(handle.budget.output_token_reserve(), 128_000);
    }

    #[test]
    fn anthropic_prune_profile_is_optimized_for_full_history_replay() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&[
            "REBON_CONTEXT_WINDOW",
            "REBON_PRUNE_PROTECTED_TURNS",
            "REBON_PRUNE_MAX_AGE_TURNS",
        ]);
        std::env::remove_var("REBON_CONTEXT_WINDOW");
        std::env::remove_var("REBON_PRUNE_PROTECTED_TURNS");
        std::env::remove_var("REBON_PRUNE_MAX_AGE_TURNS");

        let handle = prune_handle_from_env(
            &ProviderFormat::Anthropic,
            rebon_api::ProviderVendor::Anthropic,
            "claude-sonnet-4-6",
            Some(1_000_000),
            None,
            std::iter::empty::<(String, u32)>(),
            std::iter::empty::<(String, u32)>(),
        );
        assert_eq!(handle.budget.context_window(), 1_000_000);
        assert_eq!(handle.budget.microcompact_trigger(), 48_000);
        assert_eq!(handle.budget.microcompact_target(), 32_000);

        let cfg = prune_config_from_env(&ProviderFormat::Anthropic);
        assert_eq!(cfg.protected_recent_turns, 2);
        assert_eq!(cfg.tool_result_max_age_turns, 4);
        assert_eq!(cfg.error_purge_age_turns, 2);

        let openai_cfg = prune_config_from_env(&ProviderFormat::OpenaiResponses);
        assert_eq!(openai_cfg.protected_recent_turns, 4);
        assert_eq!(openai_cfg.tool_result_max_age_turns, 8);
        assert_eq!(openai_cfg.error_purge_age_turns, 4);
    }

    #[test]
    fn default_model_uses_deepseek_env_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&[
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_MODEL",
            "OPENAI_API_KEY",
            "REBON_MODEL",
        ]);
        clear_model_env();
        std::env::set_var("DEEPSEEK_API_KEY", "sk-deepseek");
        assert_eq!(default_model(), "deepseek-flash");
        std::env::set_var("DEEPSEEK_MODEL", "deepseek-v4-pro");
        assert_eq!(default_model(), "deepseek-v4-pro");
    }

    #[test]
    fn parses_deepseek_env_values_into_request_options() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&[
            "DEEPSEEK_THINKING_ENABLED",
            "DEEPSEEK_REASONING_EFFORT",
            "DEEPSEEK_THINKING_EFFORT",
        ]);
        std::env::remove_var("DEEPSEEK_THINKING_ENABLED");
        std::env::remove_var("DEEPSEEK_REASONING_EFFORT");
        std::env::remove_var("DEEPSEEK_THINKING_EFFORT");
        let options = deepseek_request_options_from_env();
        assert_eq!(options.extra_body["thinking"]["type"], "enabled");
        assert_eq!(options.body["reasoning_effort"], "high");
        assert!(options
            .omit_body_fields
            .contains(&"temperature".to_string()));

        std::env::set_var("DEEPSEEK_THINKING_ENABLED", "off");
        let options = deepseek_request_options_from_env();
        assert_eq!(options.extra_body["thinking"]["type"], "disabled");
        assert!(options.body.get("reasoning_effort").is_none());
        assert!(options.omit_body_fields.is_empty());

        std::env::set_var("DEEPSEEK_THINKING_ENABLED", "yes");
        std::env::set_var("DEEPSEEK_REASONING_EFFORT", "xhigh");
        let options = deepseek_request_options_from_env();
        assert_eq!(options.extra_body["thinking"]["type"], "enabled");
        assert_eq!(options.body["reasoning_effort"], "xhigh");
    }
}
