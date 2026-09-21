//! The Rust half of the `ctx.web` seat — an arbitrated single-slot registry
//! of web search/fetch providers.
//!
//! The composition's `ctx.web` runtime mirrors every plugin provider here
//! via `web/provider-registered` / `-unregistered` events; execution
//! dispatches back into the compose isolate over the shared serve plane
//! under the reserved target `web:<kind>:<id>`. The rebon builtin path is
//! not an entry in these registries — it is the DEPLOYMENT DEFAULT of the
//! configured id (`"rebon"`), so unconfigured deployments keep today's
//! WebSearch/WebFetch behavior bit for bit, and a configured plugin
//! provider that is missing or broken fails LOUDLY with the seat's
//! three-state codes instead of silently degrading (seat-spec §3).
//!
//! `available` is a registration-time snapshot from the provider's own
//! `available()` probe — a static composition's config cannot change under
//! it (documented v1 semantics).

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use rebon_kernel::seat::{SeatError, SeatRegistry};
use rebon_kernel::Context;
use rebon_tool::WebProviderRouter;
use rebon_tools_core::{ToolError, ToolId, ToolResult};
use serde_json::Value;

/// The configured-id value (and deployment default) that routes to rebon's
/// builtin WebSearch/WebFetch implementations.
pub const BUILTIN_WEB_PROVIDER: &str = "rebon";

/// Wall-clock budget for one plugin web call.
const WEB_PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);

/// One mirrored plugin provider: its registration-time availability
/// snapshot. Dispatch identity is the serve target derived from (kind, id).
#[derive(Clone)]
pub struct WebProviderStub {
    pub available: bool,
}

/// Explicit provider selection from config.json's `web` section.
#[derive(Debug, Clone, Default)]
pub struct WebSeatConfig {
    pub search_provider: Option<String>,
    pub fetch_provider: Option<String>,
}

impl WebSeatConfig {
    /// Parse the `web` section value (`{searchProvider?, fetchProvider?}`).
    pub fn from_section(section: &Value) -> Self {
        let field = |name: &str| {
            section
                .get(name)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        Self {
            search_provider: field("searchProvider"),
            fetch_provider: field("fetchProvider"),
        }
    }
}

/// Kind of web capability a provider serves; also the serve-target segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebCapability {
    Search,
    Fetch,
}

impl WebCapability {
    fn as_str(self) -> &'static str {
        match self {
            WebCapability::Search => "search",
            WebCapability::Fetch => "fetch",
        }
    }
}

/// The process-level web seat: arbitrated provider registries plus the
/// serve plane the winning plugin provider executes on.
pub struct WebSeat {
    search: Arc<SeatRegistry<WebProviderStub>>,
    fetch: Arc<SeatRegistry<WebProviderStub>>,
    config: WebSeatConfig,
    plane: RwLock<Option<Arc<dyn crate::kernel_compose_tools::ComposeToolDispatch>>>,
    /// Registration disposers for mirrored providers, keyed by
    /// (capability, id) so an unregister event removes exactly its entry.
    mirrors:
        std::sync::Mutex<std::collections::HashMap<(&'static str, String), rebon_kernel::Disposer>>,
}

/// Outcome of seat routing for one capability.
#[derive(Debug)]
pub enum WebRoute {
    /// Run the builtin path (configured id is `rebon`, the default).
    Builtin,
    /// Dispatch to this plugin provider.
    Plugin(String),
}

impl WebSeat {
    pub fn new(config: WebSeatConfig) -> Arc<Self> {
        Arc::new(Self {
            search: SeatRegistry::new("web-search"),
            fetch: SeatRegistry::new("web-fetch"),
            config,
            plane: RwLock::new(None),
            mirrors: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Hand the seat the dispatch its plugin providers execute on.
    ///
    /// Which runtime that is — the embedded isolate's serve plane or the plugin
    /// plane's `service/call` — is not the seat's business: arbitration, the
    /// three-state routing and the builtin default are the same either way.
    pub fn bind_plane(&self, plane: Arc<dyn crate::kernel_compose_tools::ComposeToolDispatch>) {
        *self.plane.write().expect("web seat plane poisoned") = Some(plane);
    }

    fn registry(&self, capability: WebCapability) -> &Arc<SeatRegistry<WebProviderStub>> {
        match capability {
            WebCapability::Search => &self.search,
            WebCapability::Fetch => &self.fetch,
        }
    }

    fn configured(&self, capability: WebCapability) -> &str {
        let configured = match capability {
            WebCapability::Search => self.config.search_provider.as_deref(),
            WebCapability::Fetch => self.config.fetch_provider.as_deref(),
        };
        configured.unwrap_or(BUILTIN_WEB_PROVIDER)
    }

    /// Mirror one provider registration (binding path). The registration
    /// disposer is parked in `mirrors` so the matching unregister event
    /// removes exactly this entry; leftovers die with the seat (the process
    /// slot's fork effect). Duplicates only guard event replays — the JS
    /// seat already rejects them plugin-side.
    pub fn mirror_registration(&self, capability: WebCapability, id: &str, available: bool) {
        match self
            .registry(capability)
            .register(id, WebProviderStub { available })
        {
            Ok(disposer) => {
                self.mirrors
                    .lock()
                    .expect("web seat mirrors poisoned")
                    .insert((capability.as_str(), id.to_string()), disposer);
            }
            Err(err) => tracing::warn!(%err, provider = id, "web provider mirror rejected"),
        }
    }

    pub fn mirror_unregistration(&self, capability: WebCapability, id: &str) {
        let disposer = self
            .mirrors
            .lock()
            .expect("web seat mirrors poisoned")
            .remove(&(capability.as_str(), id.to_string()));
        if let Some(disposer) = disposer {
            disposer.dispose();
        }
    }

    /// Registered plugin provider ids for one capability (diagnostics).
    pub fn provider_ids(&self, capability: WebCapability) -> Vec<String> {
        self.registry(capability).ids()
    }

    /// Route one capability: `Builtin` for the default id, otherwise the
    /// arbitrated plugin provider (loud three-state errors).
    pub fn route(&self, capability: WebCapability) -> Result<WebRoute, SeatError> {
        let configured = self.configured(capability);
        if configured == BUILTIN_WEB_PROVIDER {
            return Ok(WebRoute::Builtin);
        }
        let (id, _stub) = self
            .registry(capability)
            .resolve_with(Some(configured), |stub| stub.available)?;
        Ok(WebRoute::Plugin(id))
    }

    /// Dispatch one call to a routed plugin provider over the serve plane.
    pub async fn dispatch(
        &self,
        capability: WebCapability,
        id: &str,
        request: Value,
        tool: &str,
    ) -> ToolResult<Value> {
        let plane = self
            .plane
            .read()
            .expect("web seat plane poisoned")
            .clone()
            .filter(|plane| !plane.has_exited())
            .ok_or_else(|| ToolError::Execution {
                tool: ToolId::new(tool),
                source: anyhow::anyhow!(
                    "the plugin composition serving web provider `{id}` is not running"
                ),
            })?;
        let target = format!("web:{}:{id}", capability.as_str());
        let envelope = plane
            .serve(&target, request, WEB_PROVIDER_TIMEOUT)
            .await
            .map_err(|err| ToolError::Execution {
                tool: ToolId::new(tool),
                source: anyhow::anyhow!("web provider `{id}` failed: {err}"),
            })?;
        if let Some(err) = envelope.get("err").and_then(|e| e.as_str()) {
            let code = envelope.get("code").and_then(|c| c.as_str());
            return Err(ToolError::Execution {
                tool: ToolId::new(tool),
                source: match code {
                    Some(code) => anyhow::anyhow!("[{code}] {err}"),
                    None => anyhow::anyhow!("{err}"),
                },
            });
        }
        envelope
            .get("ok")
            .cloned()
            .ok_or_else(|| ToolError::Execution {
                tool: ToolId::new(tool),
                source: anyhow::anyhow!("web provider `{id}` returned no result envelope"),
            })
    }
}

/// Subscribe the seat to the composition's provider lifecycle events on
/// `ctx` (the fork the composition runs on). Call **before** spawning the
/// host so no early registration is missed.
pub fn attach_compose_web_binding(ctx: &Context, seat: Arc<WebSeat>) {
    let capability_of = |payload: &Value| match payload.get("kind").and_then(|k| k.as_str()) {
        Some("search") => Some(WebCapability::Search),
        Some("fetch") => Some(WebCapability::Fetch),
        _ => None,
    };
    {
        let seat = seat.clone();
        ctx.on_json("web/provider-registered", move |payload| {
            let (Some(capability), Some(id)) = (
                capability_of(payload),
                payload.get("id").and_then(|i| i.as_str()),
            ) else {
                return;
            };
            let available = payload
                .get("available")
                .and_then(|a| a.as_bool())
                .unwrap_or(false);
            seat.mirror_registration(capability, id, available);
            tracing::info!(
                provider = id,
                kind = capability.as_str(),
                available,
                "web provider mirrored"
            );
        });
    }
    ctx.on_json("web/provider-unregistered", move |payload| {
        let (Some(capability), Some(id)) = (
            capability_of(payload),
            payload.get("id").and_then(|i| i.as_str()),
        ) else {
            return;
        };
        seat.mirror_unregistration(capability, id);
        tracing::info!(
            provider = id,
            kind = capability.as_str(),
            "web provider unmirrored"
        );
    });
}

/// Process slot for the active composition's web seat — F1 discipline:
/// set at compose boot, cleared by an identity-guarded effect on the
/// compose fork, so every teardown path collects it.
fn slot() -> &'static RwLock<Option<Arc<WebSeat>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<WebSeat>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

pub fn set_process_web_seat(seat: Arc<WebSeat>) {
    *slot().write().expect("web seat slot poisoned") = Some(seat);
}

pub fn clear_process_web_seat(seat: &Arc<WebSeat>) {
    let mut guard = slot().write().expect("web seat slot poisoned");
    if guard
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, seat))
    {
        *guard = None;
    }
}

pub fn process_web_seat() -> Option<Arc<WebSeat>> {
    slot().read().expect("web seat slot poisoned").clone()
}

/// The [`WebProviderRouter`] installed on every session's ToolContext.
/// Stateless: consults the process slot at CALL time, so a composition that
/// boots later (or is torn down) is picked up without re-wiring sessions.
pub struct KernelWebRouter;

impl KernelWebRouter {
    pub fn shared() -> Arc<dyn WebProviderRouter> {
        Arc::new(Self)
    }

    async fn route_and_dispatch(
        &self,
        capability: WebCapability,
        tool: &str,
        request: Value,
        input: &Value,
    ) -> ToolResult<Option<Value>> {
        let Some(seat) = process_web_seat() else {
            return Ok(None);
        };
        let id = match seat.route(capability) {
            Ok(WebRoute::Builtin) => return Ok(None),
            Ok(WebRoute::Plugin(id)) => id,
            Err(err) => {
                // Arbitration failures are LOUD: a configured provider that
                // is missing or unavailable never silently falls back.
                return Err(ToolError::Execution {
                    tool: ToolId::new(tool),
                    source: anyhow::anyhow!("{err}"),
                });
            }
        };
        let result = seat.dispatch(capability, &id, request, tool).await?;
        Ok(Some(match capability {
            WebCapability::Search => map_search_result(input, &id, &result),
            WebCapability::Fetch => map_fetch_result(input, &id, &result),
        }))
    }
}

#[async_trait]
impl WebProviderRouter for KernelWebRouter {
    async fn search(&self, input: &Value) -> ToolResult<Option<Value>> {
        let query = input.get("query").and_then(|q| q.as_str()).unwrap_or("");
        let max_results = match input.get("search_context_size").and_then(Value::as_str) {
            None | Some("medium") => 10u64,
            Some("low") => 5,
            Some("high") => 20,
            Some(_) => 10,
        };
        let request = serde_json::json!({ "query": query, "maxResults": max_results });
        self.route_and_dispatch(WebCapability::Search, "WebSearch", request, input)
            .await
    }

    async fn fetch(&self, input: &Value) -> ToolResult<Option<Value>> {
        let url = input.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let request = serde_json::json!({ "url": url });
        self.route_and_dispatch(WebCapability::Fetch, "WebFetch", request, input)
            .await
    }
}

/// dsh `WebSearchResult` → the WebSearch tool's `{query, answer, results}`
/// contract (transcript renderers depend on this shape).
fn map_search_result(input: &Value, provider: &str, result: &Value) -> Value {
    let query = input.get("query").and_then(|q| q.as_str()).unwrap_or("");
    let sources = result
        .get("sources")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let results: Vec<Value> = sources
        .iter()
        .map(|source| {
            let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let title = source
                .get("title")
                .and_then(|t| t.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| hostname_of(url).to_string());
            serde_json::json!({
                "title": title,
                "url": url,
                "snippet": source.get("snippet").and_then(|s| s.as_str()).unwrap_or(""),
            })
        })
        .collect();
    let answer = match result.get("content").and_then(|c| c.as_str()) {
        Some(content) if !content.trim().is_empty() => content.to_string(),
        _ => format_sources_answer(query, provider, &results),
    };
    serde_json::json!({
        "query": query,
        "answer": answer,
        "results": results,
        "source": "plugin",
        "provider": provider,
    })
}

/// dsh `WebFetchResult` → the WebFetch tool's output shape.
fn map_fetch_result(input: &Value, provider: &str, result: &Value) -> Value {
    let url = input.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let body = result.get("body");
    let kind = body
        .and_then(|b| b.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or("text");
    let content = body
        .and_then(|b| b.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    serde_json::json!({
        "url": result.get("url").and_then(|u| u.as_str()).unwrap_or(url),
        "status": result.get("statusCode").and_then(|s| s.as_u64()).unwrap_or(0),
        "content_type": if kind == "html" { "text/html" } else { "text/plain" },
        "content": content,
        "offset": 0,
        "total_chars": content.chars().count(),
        "truncated": result.get("truncated").and_then(|t| t.as_bool()).unwrap_or(false),
        "source": "plugin",
        "provider": provider,
    })
}

fn hostname_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
}

/// The local-answer digest, plugin flavor: same layout the builtin path
/// renders so transcript consumers see one format.
fn format_sources_answer(query: &str, provider: &str, results: &[Value]) -> String {
    if results.is_empty() {
        return format!("No web results found for \"{query}\" (searched via {provider}).");
    }
    let mut answer = format!("Top web results for \"{query}\" (via {provider}):\n");
    for (index, result) in results.iter().enumerate() {
        let title = result.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let url = result.get("url").and_then(|u| u.as_str()).unwrap_or("");
        answer.push_str(&format!("\n{}. {}\n   {}", index + 1, title, url));
        let snippet = result.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
        if !snippet.is_empty() {
            answer.push_str(&format!("\n   {snippet}"));
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_defaults_to_builtin_and_arbitrates_configured_ids() {
        // Default (no config) → builtin.
        let seat = WebSeat::new(WebSeatConfig::default());
        assert!(matches!(
            seat.route(WebCapability::Search),
            Ok(WebRoute::Builtin)
        ));
        assert!(matches!(
            seat.route(WebCapability::Fetch),
            Ok(WebRoute::Builtin)
        ));

        // Explicit "rebon" is the builtin too.
        let seat = WebSeat::new(WebSeatConfig {
            search_provider: Some(BUILTIN_WEB_PROVIDER.into()),
            fetch_provider: None,
        });
        assert!(matches!(
            seat.route(WebCapability::Search),
            Ok(WebRoute::Builtin)
        ));

        // Configured plugin id: unknown → UNKNOWN; registered-but-
        // unavailable → UNAVAILABLE (never a fallback); available → routed.
        let seat = WebSeat::new(WebSeatConfig {
            search_provider: Some("exa".into()),
            fetch_provider: None,
        });
        let err = seat.route(WebCapability::Search).unwrap_err();
        assert_eq!(err.code(), rebon_kernel::seat::SEAT_PROVIDER_UNKNOWN);

        seat.mirror_registration(WebCapability::Search, "exa", false);
        let err = seat.route(WebCapability::Search).unwrap_err();
        assert_eq!(err.code(), rebon_kernel::seat::SEAT_PROVIDER_UNAVAILABLE);

        seat.mirror_unregistration(WebCapability::Search, "exa");
        seat.mirror_registration(WebCapability::Search, "exa", true);
        assert!(matches!(
            seat.route(WebCapability::Search),
            Ok(WebRoute::Plugin(id)) if id == "exa"
        ));

        // Unregistration returns the id to UNKNOWN.
        seat.mirror_unregistration(WebCapability::Search, "exa");
        let err = seat.route(WebCapability::Search).unwrap_err();
        assert_eq!(err.code(), rebon_kernel::seat::SEAT_PROVIDER_UNKNOWN);
    }

    #[test]
    fn binding_mirrors_registration_events() {
        let kernel = rebon_kernel::Kernel::new();
        let ctx = kernel.context().fork("compose-web");
        let seat = WebSeat::new(WebSeatConfig {
            search_provider: Some("probe".into()),
            fetch_provider: None,
        });
        attach_compose_web_binding(&ctx, seat.clone());

        kernel.context().emit_json(
            "web/provider-registered",
            &serde_json::json!({ "kind": "search", "id": "probe", "available": true }),
        );
        assert!(matches!(
            seat.route(WebCapability::Search),
            Ok(WebRoute::Plugin(id)) if id == "probe"
        ));

        kernel.context().emit_json(
            "web/provider-unregistered",
            &serde_json::json!({ "kind": "search", "id": "probe" }),
        );
        assert!(seat.route(WebCapability::Search).is_err());

        // Fork disposal removes the listeners; later events change nothing.
        ctx.dispose();
        kernel.context().emit_json(
            "web/provider-registered",
            &serde_json::json!({ "kind": "search", "id": "probe", "available": true }),
        );
        assert!(seat.route(WebCapability::Search).is_err());
    }

    #[test]
    fn process_slot_clear_is_identity_guarded() {
        let a = WebSeat::new(WebSeatConfig::default());
        let b = WebSeat::new(WebSeatConfig::default());
        set_process_web_seat(a.clone());
        set_process_web_seat(b.clone());
        clear_process_web_seat(&a);
        assert!(
            process_web_seat().is_some_and(|current| Arc::ptr_eq(&current, &b)),
            "successor survives a stale clear"
        );
        clear_process_web_seat(&b);
        assert!(process_web_seat().is_none());
    }

    #[test]
    fn search_mapping_keeps_the_tool_contract() {
        let input = serde_json::json!({ "query": "rust async" });
        let dsh = serde_json::json!({
            "sources": [
                { "url": "https://docs.rs/tokio", "title": "tokio", "snippet": "async runtime" },
                { "url": "https://example.com/page" },
            ],
            "truncated": false,
        });
        let out = map_search_result(&input, "exa", &dsh);
        assert_eq!(out["query"], "rust async");
        assert_eq!(out["source"], "plugin");
        assert_eq!(out["provider"], "exa");
        assert_eq!(out["results"][0]["title"], "tokio");
        assert_eq!(
            out["results"][1]["title"], "example.com",
            "missing titles render as the hostname"
        );
        let answer = out["answer"].as_str().unwrap();
        assert!(answer.contains("1. tokio"), "{answer}");
        assert!(answer.contains("via exa"), "{answer}");

        // A provider-generated answer (Perplexity shape) passes through.
        let with_content = serde_json::json!({ "content": "Generated summary.", "sources": [], "truncated": false });
        let out = map_search_result(&input, "pplx", &with_content);
        assert_eq!(out["answer"], "Generated summary.");
    }

    #[test]
    fn fetch_mapping_keeps_the_tool_contract() {
        let input = serde_json::json!({ "url": "https://example.com" });
        let dsh = serde_json::json!({
            "url": "https://example.com/final",
            "statusCode": 200,
            "body": { "kind": "html", "content": "<h1>hi</h1>" },
            "truncated": false,
        });
        let out = map_fetch_result(&input, "http", &dsh);
        assert_eq!(out["url"], "https://example.com/final");
        assert_eq!(out["status"], 200);
        assert_eq!(out["content_type"], "text/html");
        assert_eq!(out["content"], "<h1>hi</h1>");
        assert_eq!(out["source"], "plugin");
    }
}
