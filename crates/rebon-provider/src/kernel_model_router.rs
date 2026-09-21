//! Plugin request routing: the `model-router` kernel service plus the
//! resolver wrapper that lets plugin-registered provider routes participate
//! in model resolution.
//!
//! Routing semantics follow dsh's: the exact `{provider, model}` pair is the
//! only routing key, the model catalog is **advisory** (adapters may accept
//! model ids outside their `listModels` output, so `resolve` never rejects an
//! un-cataloged id — it only withholds capacity facts), and a builtin
//! provider always wins over a plugin route with the same name.
//!
//! Streaming model dispatch is consumed through a typed `model-router` seat.
//! Native `modelProvider` clients are registered with a scoped generation and
//! routed clients revalidate that generation before opening each stream.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use rebon_agent_core::model_router::{ProviderModelRuntime, ProviderRuntimeResolver};
use rebon_api::{CreateMessageRequest, ModelClient, ModelError, ModelResult, StreamEventStream};
use rebon_kernel::{Context, JsonService, KernelError, Plugin, PluginMeta, Service};
use rebon_types::ModelProfileMap;

/// JSON-plane name of the route-table service.
pub const MODEL_ROUTER_SERVICE: &str = "model-router";

#[derive(Debug, Clone)]
struct RouteModel {
    id: String,
    context_window: Option<u32>,
}

struct RouteEntry {
    /// Registration token: a scoped registration's disposer removes the
    /// route only while this still matches, so a later JSON-plane
    /// re-registration (replace semantics) survives a stale fork disposal.
    token: u64,
    models: Vec<RouteModel>,
    default_model: Option<String>,
    capabilities: serde_json::Value,
    /// Live typed provider for native `modelProvider` consumption. JSON-only
    /// plugin routes leave this empty and bind their transport separately.
    client: Option<Arc<dyn ModelClient>>,
}

impl RouteEntry {
    /// The model a bare `{provider}` resolve lands on.
    fn fallback_model(&self) -> Option<&str> {
        self.default_model
            .as_deref()
            .or_else(|| self.models.first().map(|m| m.id.as_str()))
    }

    fn context_window_for(&self, model: &str) -> Option<u32> {
        self.models
            .iter()
            .find(|entry| entry.id == model)
            .and_then(|entry| entry.context_window)
    }
}

/// Process-global route table, registered on the kernel's JSON plane so JS
/// and Rust plugins share one registration surface. The kernel table is the
/// only source of truth for plugin routes — nothing is mirrored into
/// rebon-config.
#[derive(Default)]
pub struct ModelRouterService {
    routes: RwLock<HashMap<String, RouteEntry>>,
    next_token: std::sync::atomic::AtomicU64,
}

impl ModelRouterService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of registered routes (conformance/diagnostics).
    pub fn route_count(&self) -> usize {
        self.routes
            .read()
            .expect("model-router table poisoned")
            .len()
    }

    /// Rust-plane registration conforming to the seat spec's registration
    /// primitive: the route is tied to `ctx`'s scope and unregisters when
    /// that fork disposes. Replace semantics still apply while live; the
    /// scoped disposer is token-guarded so it never removes a successor
    /// registered after a replace.
    pub fn register_scoped(
        self: &Arc<Self>,
        ctx: &rebon_kernel::Context,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        let (provider, token, replaced) = self.register_entry(params, None)?;
        let weak = Arc::downgrade(self);
        let for_disposer = provider.clone();
        ctx.effect_labeled(&format!("model-router route({provider})"), || {
            rebon_kernel::Disposer::new(move || {
                if let Some(service) = weak.upgrade() {
                    let mut routes = service.routes.write().expect("model-router table poisoned");
                    if routes.get(&for_disposer).is_some_and(|e| e.token == token) {
                        routes.remove(&for_disposer);
                    }
                }
            })
        });
        Ok(serde_json::json!({ "provider": provider, "replaced": replaced }))
    }

    /// Register a live Rust model provider into the same keyed-route seat used
    /// by JSON plugin routes. The returned token is captured by the routed
    /// client so a held client cannot call a successor registration.
    pub fn register_client_scoped(
        self: &Arc<Self>,
        ctx: &Context,
        params: &serde_json::Value,
        client: Arc<dyn ModelClient>,
    ) -> Result<(String, u64), KernelError> {
        let (provider, token, _) = self.register_entry(params, Some(client))?;
        let weak = Arc::downgrade(self);
        let for_disposer = provider.clone();
        ctx.effect_labeled(&format!("model provider({provider})"), || {
            rebon_kernel::Disposer::new(move || {
                if let Some(service) = weak.upgrade() {
                    let mut routes = service.routes.write().expect("model-router table poisoned");
                    if routes
                        .get(&for_disposer)
                        .is_some_and(|entry| entry.token == token)
                    {
                        routes.remove(&for_disposer);
                    }
                }
            })
        });
        Ok((provider, token))
    }

    fn client_for_token(&self, provider: &str, token: u64) -> Option<Arc<dyn ModelClient>> {
        self.routes
            .read()
            .expect("model-router table poisoned")
            .get(provider)
            .filter(|entry| entry.token == token)
            .and_then(|entry| entry.client.clone())
    }

    fn routed_client(self: &Arc<Self>, provider: &str) -> Option<Arc<dyn ModelClient>> {
        let routes = self.routes.read().expect("model-router table poisoned");
        let entry = routes.get(provider)?;
        let initial = entry.client.clone()?;
        Some(Arc::new(KernelRoutedModelClient::new(
            self,
            provider,
            entry.token,
            initial,
        )))
    }

    fn register(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let (provider, token, replaced) = self.register_entry(params, None)?;
        // The token is the JSON-plane disposal guard: pass it back to
        // `unregister` so a stale teardown never removes a successor route.
        Ok(serde_json::json!({ "provider": provider, "replaced": replaced, "token": token }))
    }

    fn register_entry(
        &self,
        params: &serde_json::Value,
        client: Option<Arc<dyn ModelClient>>,
    ) -> Result<(String, u64, bool), KernelError> {
        let provider = params
            .get("provider")
            .and_then(|p| p.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                KernelError::Other("model-router register requires a non-empty `provider`".into())
            })?;
        let models: Vec<RouteModel> = params
            .get("models")
            .and_then(|m| m.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let id = entry.get("id")?.as_str()?.trim();
                        (!id.is_empty()).then(|| RouteModel {
                            id: id.to_string(),
                            context_window: entry
                                .get("contextWindow")
                                .and_then(|w| w.as_u64())
                                .and_then(|w| u32::try_from(w).ok()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let default_model = params
            .get("defaultModel")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        if default_model.is_none() && models.is_empty() {
            return Err(KernelError::Other(format!(
                "model-router route `{provider}` needs a `defaultModel` or at least one model"
            )));
        }
        let token = self
            .next_token
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = RouteEntry {
            token,
            models,
            default_model,
            capabilities: params
                .get("capabilities")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            client,
        };
        let replaced = self
            .routes
            .write()
            .expect("model-router table poisoned")
            .insert(provider.to_string(), entry)
            .is_some();
        Ok((provider.to_string(), token, replaced))
    }

    fn resolve(&self, params: &serde_json::Value) -> serde_json::Value {
        let Some(provider) = params
            .get("provider")
            .and_then(|p| p.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            // Routes only exist for named providers; the unnamed case is the
            // main session runtime and never consults the table.
            return serde_json::Value::Null;
        };
        let routes = self.routes.read().expect("model-router table poisoned");
        let Some(entry) = routes.get(provider) else {
            return serde_json::Value::Null;
        };
        let requested = params
            .get("model")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty());
        let Some(model) = requested.or_else(|| entry.fallback_model()) else {
            return serde_json::Value::Null;
        };
        serde_json::json!({
            "provider": provider,
            "model": model,
            "contextWindow": entry.context_window_for(model),
            "capabilities": entry.capabilities,
        })
    }

    fn unregister(&self, params: &serde_json::Value) -> serde_json::Value {
        // Optional token guard (seat-spec §2): when the caller passes the
        // token its register returned, removal only happens while the live
        // entry still carries it — a replace in between wins.
        let token_guard = params.get("token").and_then(|t| t.as_u64());
        let removed = params
            .get("provider")
            .and_then(|p| p.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .is_some_and(|provider| {
                let mut routes = self.routes.write().expect("model-router table poisoned");
                match routes.get(provider) {
                    Some(entry) if token_guard.map_or(true, |t| entry.token == t) => {
                        routes.remove(provider).is_some()
                    }
                    _ => false,
                }
            });
        serde_json::json!({ "removed": removed })
    }

    fn list(&self) -> serde_json::Value {
        let routes = self.routes.read().expect("model-router table poisoned");
        let mut providers: Vec<serde_json::Value> = routes
            .iter()
            .map(|(provider, entry)| {
                serde_json::json!({
                    "provider": provider,
                    "models": entry.models.iter().map(|m| serde_json::json!({
                        "id": m.id,
                        "contextWindow": m.context_window,
                    })).collect::<Vec<_>>(),
                    "defaultModel": entry.default_model,
                    "capabilities": entry.capabilities,
                })
            })
            .collect();
        providers.sort_by(|a, b| a["provider"].as_str().cmp(&b["provider"].as_str()));
        serde_json::json!({ "providers": providers })
    }
}

impl JsonService for ModelRouterService {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        match method {
            "register" => self.register(&params),
            "resolve" => Ok(self.resolve(&params)),
            "unregister" => Ok(self.unregister(&params)),
            "list" => Ok(self.list()),
            other => Err(KernelError::Other(format!(
                "model-router has no method `{other}`"
            ))),
        }
    }
}

/// Typed definition for the model provider/router seat.
pub struct ModelRouterSeat;

impl Service for ModelRouterSeat {
    type Interface = ModelRouterService;
    const NAME: &'static str = MODEL_ROUTER_SERVICE;
}

/// Kernel plugin providing the global `model-router` seam.
pub struct ModelRouterPlugin;

impl Plugin for ModelRouterPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("model-router").provides(&[MODEL_ROUTER_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let service = ModelRouterService::new();
        ctx.provide_dual::<ModelRouterSeat>(service.clone(), service)
    }
}

struct ModelRouteOwner {
    scope: Context,
    _kernel: Arc<rebon_kernel::Kernel>,
}

impl Drop for ModelRouteOwner {
    fn drop(&mut self) {
        self.scope.dispose();
    }
}

/// Generation-bound streaming client resolved from the typed model seat.
/// Every stream start re-enters the seat and verifies the registration token;
/// a provider unloaded after resolution fails with `[STALE_PROVIDER]`.
pub struct KernelRoutedModelClient {
    service: std::sync::Weak<ModelRouterService>,
    provider: String,
    token: u64,
    initial: Arc<dyn ModelClient>,
    owner: Option<ModelRouteOwner>,
}

impl KernelRoutedModelClient {
    fn new(
        service: &Arc<ModelRouterService>,
        provider: impl Into<String>,
        token: u64,
        initial: Arc<dyn ModelClient>,
    ) -> Self {
        Self {
            service: Arc::downgrade(service),
            provider: provider.into(),
            token,
            initial,
            owner: None,
        }
    }

    fn with_owner(mut self, owner: ModelRouteOwner) -> Self {
        self.owner = Some(owner);
        self
    }

    fn current(&self) -> ModelResult<Arc<dyn ModelClient>> {
        self.service
            .upgrade()
            .and_then(|service| service.client_for_token(&self.provider, self.token))
            .ok_or_else(|| {
                ModelError::Permanent(format!(
                    "[STALE_PROVIDER] model provider `{}` is unloaded or was replaced",
                    self.provider
                ))
            })
    }
}

#[async_trait]
impl ModelClient for KernelRoutedModelClient {
    fn provider_name(&self) -> &'static str {
        self.initial.provider_name()
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        self.current()?.create_message_stream(request).await
    }

    fn context_prune_handle(&self) -> Option<rebon_api::PruneLevelHandle> {
        self.initial.context_prune_handle()
    }

    fn supports_codex_oauth_web_search(&self) -> bool {
        self.initial.supports_codex_oauth_web_search()
    }

    fn supports_request_scoped_transient_context(&self) -> bool {
        self.initial.supports_request_scoped_transient_context()
    }

    fn supports_forced_tool_choice(&self) -> bool {
        self.initial.supports_forced_tool_choice()
    }

    fn supports_anchored_minimal(&self) -> bool {
        self.initial.supports_anchored_minimal()
    }

    fn output_budget_includes_reasoning(&self) -> bool {
        self.initial.output_budget_includes_reasoning()
    }

    fn thinking_replay_requires_signature(&self) -> bool {
        self.initial.thinking_replay_requires_signature()
    }

    fn prefix_cache_is_byte_exact(&self) -> bool {
        self.initial.prefix_cache_is_byte_exact()
    }

    fn supports_remote_compaction_v2(&self) -> bool {
        self.initial.supports_remote_compaction_v2()
    }

    fn reset_session_state(&self) {
        if let Ok(client) = self.current() {
            client.reset_session_state();
        }
    }

    fn end_turn(&self) {
        if let Ok(client) = self.current() {
            client.end_turn();
        }
    }

    fn invalidate_previous_response_id(&self) {
        if let Ok(client) = self.current() {
            client.invalidate_previous_response_id();
        }
    }
}

/// Put a native `modelProvider` protocol client behind a private kernel seat
/// and return the routed streaming consumer. A private seat preserves the
/// existing per-runtime process lifecycle while still making the kernel the
/// sole consumption path; concurrent sessions using the same provider id do
/// not replace one another.
pub fn route_model_provider_client(
    provider: &str,
    model: &str,
    client: Arc<dyn ModelClient>,
) -> anyhow::Result<Arc<dyn ModelClient>> {
    let kernel = rebon_kernel::Kernel::new();
    kernel.load(vec![Box::new(ModelRouterPlugin)])?;
    let scope = kernel.context().fork(&format!("model-provider/{provider}"));
    let service = kernel.context().require::<ModelRouterSeat>()?;
    let (provider, token) = service.register_client_scoped(
        &scope,
        &serde_json::json!({
            "provider": provider,
            "defaultModel": model,
        }),
        client.clone(),
    )?;
    Ok(Arc::new(
        KernelRoutedModelClient::new(&service, provider, token, client).with_owner(
            ModelRouteOwner {
                scope,
                _kernel: kernel,
            },
        ),
    ))
}

/// [`ProviderRuntimeResolver`] wrapper that consults the kernel route table.
///
/// Precedence: the native resolver is tried first, so a builtin provider
/// always shadows a plugin route with the same name (with a warning). Only
/// when native resolution fails does a kernel route satisfy the request; if
/// neither knows the provider, the native error surfaces unchanged.
pub struct KernelAwareProviderResolver {
    ctx: Context,
    inner: Arc<dyn ProviderRuntimeResolver>,
}

impl KernelAwareProviderResolver {
    pub fn new(ctx: Context, inner: Arc<dyn ProviderRuntimeResolver>) -> Arc<Self> {
        Arc::new(Self { ctx, inner })
    }

    /// Look the provider up in the kernel route table. A missing service is a
    /// normal miss. ACL, plane, and provider errors are preserved so a plugin
    /// cannot turn an unauthorized resolve into ambient native fallback.
    fn kernel_route(&self, provider: &str) -> Result<Option<serde_json::Value>, KernelError> {
        match self.ctx.call_json(
            MODEL_ROUTER_SERVICE,
            "resolve",
            serde_json::json!({ "provider": provider }),
        ) {
            Ok(serde_json::Value::Null) | Err(KernelError::ServiceNotFound(_)) => Ok(None),
            Ok(route) => Ok(Some(route)),
            Err(err) => Err(err),
        }
    }
}

#[async_trait]
impl ProviderRuntimeResolver for KernelAwareProviderResolver {
    async fn resolve_provider(
        &self,
        provider: Option<&str>,
    ) -> anyhow::Result<ProviderModelRuntime> {
        let Some(name) = provider.map(str::trim).filter(|value| !value.is_empty()) else {
            return self.inner.resolve_provider(None).await;
        };
        match self.inner.resolve_provider(Some(name)).await {
            Ok(runtime) => {
                // The inner resolver may itself have satisfied the request
                // FROM the kernel route (the main-session runtime path does);
                // only a genuinely different backend is a shadowing builtin.
                let plugin_served = matches!(
                    runtime.client.provider_name(),
                    "plugin-llm" | "kernel-model-route"
                );
                if !plugin_served && self.kernel_route(name).ok().flatten().is_some() {
                    tracing::warn!(
                        provider = name,
                        "builtin provider shadows a plugin model route with the same name"
                    );
                }
                Ok(runtime)
            }
            Err(native_err) => {
                let route = match self.kernel_route(name) {
                    Ok(Some(route)) => route,
                    Ok(None) => return Err(native_err),
                    Err(err) => return Err(anyhow::Error::new(err)),
                };
                let Some(model) = route
                    .get("model")
                    .and_then(|model| model.as_str())
                    .map(str::to_string)
                else {
                    return Err(native_err);
                };
                tracing::debug!(
                    provider = name,
                    model,
                    "provider resolved via kernel model-router route"
                );

                // Native model providers are held on the typed seat. The
                // routed client captures the registration generation so an
                // unload/replace after resolve cannot dispatch stale code.
                match self.ctx.require::<ModelRouterSeat>() {
                    Ok(service) => {
                        if let Some(client) = service.routed_client(name) {
                            return Ok(ProviderModelRuntime {
                                provider_name: name.to_string(),
                                client,
                                default_model: model,
                                model_profiles: ModelProfileMap::default(),
                            });
                        }
                    }
                    Err(KernelError::ServiceNotFound(_)) => {}
                    Err(err) => return Err(anyhow::Error::new(err)),
                }

                // An adapter host is another live transport bound to the route,
                // outside the native typed registration. The plane registers one
                // for every `llmProviders` entry a plugin declares.
                //
                // This used to be gated on the embedded runtime, from when the
                // host trait lived in that crate — which meant a build without
                // it, which is the one that ships, could not resolve any route a
                // plugin had registered.
                if let Some(host) = crate::kernel_llm_dispatch::llm_host_for(name) {
                    return Ok(ProviderModelRuntime {
                        provider_name: name.to_string(),
                        client: Arc::new(crate::kernel_llm_dispatch::DshLlmClient::new(name, host)),
                        default_model: model,
                        model_profiles: ModelProfileMap::default(),
                    });
                }

                Err(anyhow::anyhow!(
                    "[MODEL_PROVIDER_UNAVAILABLE] model provider route `{name}` has no live streaming provider"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use futures_util::StreamExt;
    use rebon_agent_core::model_router::{
        AgentModelRouter, ConfigurableModelRouter, ModelRouteRequest,
    };
    use rebon_kernel::Kernel;
    use std::sync::Mutex;

    fn kernel_with_router() -> Arc<Kernel> {
        let kernel = Kernel::new();
        kernel
            .load(vec![Box::new(ModelRouterPlugin)])
            .expect("model-router plugin loads");
        kernel
    }

    fn register(ctx: &Context, params: serde_json::Value) {
        ctx.call_json(MODEL_ROUTER_SERVICE, "register", params)
            .expect("route registers");
    }

    /// Inner resolver that knows exactly one builtin provider.
    struct BuiltinOnly {
        provider: String,
    }

    #[async_trait]
    impl ProviderRuntimeResolver for BuiltinOnly {
        async fn resolve_provider(
            &self,
            provider: Option<&str>,
        ) -> anyhow::Result<ProviderModelRuntime> {
            let name = provider.unwrap_or(&self.provider);
            if name != self.provider {
                return Err(anyhow!("unknown provider `{name}`"));
            }
            Ok(ProviderModelRuntime {
                provider_name: self.provider.clone(),
                client: Arc::new(rebon_api::MockModelClient::new()),
                default_model: "builtin-default".into(),
                model_profiles: ModelProfileMap::default(),
            })
        }
    }

    fn wrapped(kernel: &Kernel) -> Arc<KernelAwareProviderResolver> {
        KernelAwareProviderResolver::new(
            kernel.context().clone(),
            Arc::new(BuiltinOnly {
                provider: "openai".into(),
            }),
        )
    }

    #[test]
    fn register_then_resolve_uses_default_and_catalog_facts() {
        let kernel = kernel_with_router();
        let ctx = kernel.context();
        register(
            ctx,
            serde_json::json!({
                "provider": "dsh-fake",
                "models": [
                    { "id": "fake-large", "contextWindow": 64000 },
                    { "id": "fake-mini" },
                ],
                "defaultModel": "fake-large",
            }),
        );

        let hit = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "resolve",
                serde_json::json!({ "provider": "dsh-fake" }),
            )
            .unwrap();
        assert_eq!(hit["model"], "fake-large");
        assert_eq!(hit["contextWindow"], 64000);

        // Cataloged model: capacity fact comes back. Un-cataloged model: the
        // catalog is advisory (dsh semantics), so the id passes through with
        // no capacity fact — never a rejection.
        let mini = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "resolve",
                serde_json::json!({ "provider": "dsh-fake", "model": "fake-mini" }),
            )
            .unwrap();
        assert_eq!(mini["model"], "fake-mini");
        assert!(mini["contextWindow"].is_null());
        let dynamic = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "resolve",
                serde_json::json!({ "provider": "dsh-fake", "model": "fake-2026-preview" }),
            )
            .unwrap();
        assert_eq!(dynamic["model"], "fake-2026-preview");
        assert!(dynamic["contextWindow"].is_null());

        // Unknown provider and the unnamed case both miss.
        let miss = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "resolve",
                serde_json::json!({ "provider": "nope" }),
            )
            .unwrap();
        assert!(miss.is_null());
        let unnamed = ctx
            .call_json(MODEL_ROUTER_SERVICE, "resolve", serde_json::json!({}))
            .unwrap();
        assert!(unnamed.is_null());
    }

    #[test]
    fn register_validates_and_unregister_removes() {
        let kernel = kernel_with_router();
        let ctx = kernel.context();

        let err = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "register",
                serde_json::json!({ "provider": "empty-route" }),
            )
            .expect_err("route without models or default must be rejected");
        assert!(err.to_string().contains("defaultModel"), "{err}");

        register(
            ctx,
            serde_json::json!({ "provider": "dsh-fake", "defaultModel": "fake-large" }),
        );
        let removed = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "unregister",
                serde_json::json!({ "provider": "dsh-fake" }),
            )
            .unwrap();
        assert_eq!(removed["removed"], true);
        let miss = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "resolve",
                serde_json::json!({ "provider": "dsh-fake" }),
            )
            .unwrap();
        assert!(miss.is_null());
    }

    /// JSON-plane token guard: unregistering with
    /// the token a superseded registration returned must spare the
    /// replacement; the replacement's own token still removes it.
    #[test]
    fn json_unregister_with_stale_token_spares_replacement() {
        let kernel = kernel_with_router();
        let ctx = kernel.context();

        let first = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "register",
                serde_json::json!({ "provider": "guarded", "defaultModel": "m1" }),
            )
            .unwrap();
        let second = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "register",
                serde_json::json!({ "provider": "guarded", "defaultModel": "m2" }),
            )
            .unwrap();
        assert_eq!(second["replaced"], true);
        assert_ne!(
            first["token"], second["token"],
            "tokens are per-registration"
        );

        let stale = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "unregister",
                serde_json::json!({ "provider": "guarded", "token": first["token"] }),
            )
            .unwrap();
        assert_eq!(
            stale["removed"], false,
            "stale token must not remove the successor"
        );
        let live = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "resolve",
                serde_json::json!({ "provider": "guarded" }),
            )
            .unwrap();
        assert_eq!(
            live["model"], "m2",
            "successor survives the stale unregister"
        );

        let current = ctx
            .call_json(
                MODEL_ROUTER_SERVICE,
                "unregister",
                serde_json::json!({ "provider": "guarded", "token": second["token"] }),
            )
            .unwrap();
        assert_eq!(current["removed"], true);
    }

    /// Seat-spec conformance (exemplar): a scoped
    /// route registration is a Context registration like any other — mount,
    /// enumerate, dispose, no residue in any registry. The arbitration
    /// template does not apply: model-router is a keyed-route seat (exact
    /// provider name, miss = Null so native resolution chains), not an
    /// arbitrated single-slot seat.
    #[test]
    fn seat_conformance_scoped_registration_leaves_no_residue() {
        let kernel = rebon_kernel::Kernel::new();
        let root = kernel.context();
        let service = ModelRouterService::new();
        root.provide_json(MODEL_ROUTER_SERVICE, service.clone())
            .expect("router provides");

        let for_count = service.clone();
        rebon_kernel::testing::assert_mount_leaves_no_residue(
            root,
            move || for_count.route_count(),
            |ctx| {
                service
                    .register_scoped(
                        ctx,
                        &serde_json::json!({
                            "provider": "probe-route",
                            "defaultModel": "probe-model",
                        }),
                    )
                    .expect("scoped route registers");
            },
        );
    }

    /// Seat-spec conformance: replace semantics + scoped disposal compose
    /// safely — a fork's stale disposer never removes the JSON-plane
    /// replacement that superseded its registration.
    #[test]
    fn seat_conformance_stale_scoped_disposer_spares_replacement() {
        let kernel = rebon_kernel::Kernel::new();
        let root = kernel.context();
        let service = ModelRouterService::new();

        let fork = root.fork("plugin-a");
        service
            .register_scoped(
                &fork,
                &serde_json::json!({ "provider": "x", "defaultModel": "m1" }),
            )
            .expect("scoped route registers");

        // A JSON-plane re-registration replaces the route (the JS shim's
        // config-update path).
        let replaced = service
            .call(
                "register",
                serde_json::json!({ "provider": "x", "defaultModel": "m2" }),
            )
            .expect("replace registers");
        assert_eq!(replaced["replaced"], true);

        // Disposing the original fork must not remove the successor.
        fork.dispose();
        assert_eq!(service.route_count(), 1, "successor route survives");
        let resolved = service
            .call("resolve", serde_json::json!({ "provider": "x" }))
            .expect("resolve runs");
        assert_eq!(resolved["model"], "m2");
    }

    #[tokio::test]
    async fn coverage_matrix_metadata_only_route_fails_at_resolution() {
        let kernel = kernel_with_router();
        register(
            kernel.context(),
            serde_json::json!({
                "provider": "dsh-fake",
                "models": [{ "id": "fake-large", "contextWindow": 64000 }],
                "defaultModel": "fake-large",
            }),
        );

        let router = ConfigurableModelRouter::new(wrapped(&kernel));
        let err = router
            .resolve(ModelRouteRequest {
                provider: Some("dsh-fake".into()),
                ..ModelRouteRequest::default()
            })
            .await
            .expect_err("metadata without a live provider must not resolve");
        assert!(
            err.to_string().contains("[MODEL_PROVIDER_UNAVAILABLE]"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn coverage_matrix_live_model_provider_routes_streaming() {
        let kernel = kernel_with_router();
        let service = kernel
            .context()
            .require::<ModelRouterSeat>()
            .expect("typed model seat is provided");
        let provider_scope = kernel.context().fork("live-model-provider");
        let mock = Arc::new(rebon_api::MockModelClient::new());
        mock.push_turn(vec![rebon_api::StreamEvent::MessageStop]);
        service
            .register_client_scoped(
                &provider_scope,
                &serde_json::json!({
                    "provider": "live-fake",
                    "defaultModel": "fake-large",
                }),
                mock.clone(),
            )
            .expect("live provider registers");

        let resolved = ConfigurableModelRouter::new(wrapped(&kernel))
            .resolve(ModelRouteRequest {
                provider: Some("live-fake".into()),
                ..ModelRouteRequest::default()
            })
            .await
            .expect("typed provider route resolves");
        assert_eq!(resolved.provider_name, "live-fake");
        assert_eq!(resolved.model, "fake-large");
        let mut stream = resolved
            .client
            .create_message_stream(CreateMessageRequest::simple("fake-large", "ping"))
            .await
            .expect("stream starts through the typed seat");
        assert!(matches!(
            stream.next().await,
            Some(Ok(rebon_api::StreamEvent::MessageStop))
        ));
        assert_eq!(mock.captured_requests().len(), 1);
    }

    #[tokio::test]
    async fn coverage_matrix_native_model_provider_helper_streams_through_private_seat() {
        let mock = Arc::new(rebon_api::MockModelClient::new());
        mock.push_turn(vec![rebon_api::StreamEvent::MessageStop]);
        let routed = route_model_provider_client("external-fake", "fake-large", mock.clone())
            .expect("native modelProvider client routes through a private seat");

        let mut stream = routed
            .create_message_stream(CreateMessageRequest::simple("fake-large", "ping"))
            .await
            .expect("routed external provider starts a stream");
        assert!(matches!(
            stream.next().await,
            Some(Ok(rebon_api::StreamEvent::MessageStop))
        ));
        assert_eq!(mock.captured_requests().len(), 1);
    }

    #[tokio::test]
    async fn coverage_matrix_replaced_model_provider_rejects_held_client() {
        let kernel = kernel_with_router();
        let service = kernel
            .context()
            .require::<ModelRouterSeat>()
            .expect("typed model seat is provided");
        let first_scope = kernel.context().fork("first-model-provider");
        let second_scope = kernel.context().fork("second-model-provider");
        let first = Arc::new(rebon_api::MockModelClient::new());
        service
            .register_client_scoped(
                &first_scope,
                &serde_json::json!({
                    "provider": "replaceable",
                    "defaultModel": "m1",
                }),
                first,
            )
            .unwrap();
        let held = service
            .routed_client("replaceable")
            .expect("first generation resolves");

        let second = Arc::new(rebon_api::MockModelClient::new());
        second.push_turn(vec![rebon_api::StreamEvent::MessageStop]);
        service
            .register_client_scoped(
                &second_scope,
                &serde_json::json!({
                    "provider": "replaceable",
                    "defaultModel": "m2",
                }),
                second.clone(),
            )
            .unwrap();
        first_scope.dispose();

        let err = match held
            .create_message_stream(CreateMessageRequest::simple("m1", "stale"))
            .await
        {
            Ok(_) => panic!("held client must not dispatch into a replacement"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("[STALE_PROVIDER]"), "{err}");

        let mut current_stream = service
            .routed_client("replaceable")
            .unwrap()
            .create_message_stream(CreateMessageRequest::simple("m2", "current"))
            .await
            .expect("replacement remains live");
        assert!(matches!(
            current_stream.next().await,
            Some(Ok(rebon_api::StreamEvent::MessageStop))
        ));
        assert_eq!(second.captured_requests().len(), 1);
    }

    struct CaptureUnauthorizedContext(Arc<Mutex<Option<Context>>>);

    impl Plugin for CaptureUnauthorizedContext {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("unauthorized-model-consumer")
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            *self.0.lock().unwrap() = Some(ctx.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn coverage_matrix_unauthorized_model_consumer_fails_loudly() {
        let kernel = Kernel::new();
        let captured = Arc::new(Mutex::new(None));
        kernel
            .load(vec![
                Box::new(ModelRouterPlugin),
                Box::new(CaptureUnauthorizedContext(captured.clone())),
            ])
            .unwrap();
        register(
            kernel.context(),
            serde_json::json!({
                "provider": "guarded-model",
                "defaultModel": "m1",
            }),
        );
        let resolver = KernelAwareProviderResolver::new(
            captured.lock().unwrap().clone().unwrap(),
            Arc::new(BuiltinOnly {
                provider: "openai".into(),
            }),
        );

        let err = resolver
            .resolve_provider(Some("guarded-model"))
            .await
            .expect_err("undeclared model-router access must fail");
        assert!(err.to_string().contains("[UNAUTHORIZED_RESOLVE]"), "{err}");
    }

    #[tokio::test]
    async fn builtin_provider_wins_over_plugin_route_with_same_name() {
        let kernel = kernel_with_router();
        register(
            kernel.context(),
            serde_json::json!({ "provider": "openai", "defaultModel": "impostor" }),
        );

        let resolved = wrapped(&kernel)
            .resolve_provider(Some("openai"))
            .await
            .expect("builtin resolution succeeds");
        assert_eq!(resolved.default_model, "builtin-default");
        assert_eq!(resolved.client.provider_name(), "mock");
    }

    #[tokio::test]
    async fn unknown_everywhere_surfaces_the_native_error() {
        let kernel = kernel_with_router();
        let err = wrapped(&kernel)
            .resolve_provider(Some("ghost"))
            .await
            .expect_err("no builtin, no route: native error surfaces");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[tokio::test]
    async fn unnamed_provider_never_consults_the_route_table() {
        let kernel = kernel_with_router();
        register(
            kernel.context(),
            serde_json::json!({ "provider": "dsh-fake", "defaultModel": "fake-large" }),
        );
        let resolved = wrapped(&kernel)
            .resolve_provider(None)
            .await
            .expect("unnamed resolves to the main runtime");
        assert_eq!(resolved.provider_name, "openai");
    }
}
