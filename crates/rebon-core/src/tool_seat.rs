//! Kernel-backed tool consumption seat.
//!
//! The Engine sees one [`ToolResolver`] regardless of where a tool came from.
//! Providers are registered with explicit priorities (`Core` / `Feature` <
//! `Mcp` < `Plugin` < upstream), so precedence is data in the seat instead
//! of control flow in `Engine::invoke_tool`.
//!
//! There are two seats of the same type. The **process seat** lives on the
//! kernel root for the life of the process; the `core-tools` plugin provides
//! it and registers the primitives there, and feature plugins add theirs.
//! The **turn seat** is forked under a session for one turn and holds what
//! that turn brings — the engine's host tools, MCP definitions, plugin
//! tools — with the process seat registered last as its upstream, so a
//! plugin's registration on the root is visible to every turn without the
//! engine holding a list of its own.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use rebon_kernel::{Context, Disposer, KernelError, Service};
use rebon_tool::{
    McpToolDefinition, PluginToolProvider, Tool, ToolContext, ToolFilter, ToolResolver,
};
use rebon_tools_core::{
    tool_matches_name, PermissionDecision, ToolError, ToolId, ToolInputSchema, ToolKind,
    ToolResult, ValidationOutcome,
};
use serde_json::Value;

use crate::permission::KernelContextLease;
use crate::Engine;

pub const TOOL_SEAT_SERVICE: &str = "tool-registry";

/// Where a provider stands when two of them know the same name: the lowest
/// value answers first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Priority {
    /// The primitives; `core-tools` registers at this level.
    Core = 0,
    /// Built-in feature tools, and the engine's host tools on a turn seat.
    Feature = 50,
    /// MCP server proxies.
    Mcp = 100,
    /// Plugin-plane tools (Code Mode, composed Node plugins).
    Plugin = 200,
}

impl Priority {
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// The turn seat's view of the process seat: registered after everything
/// the turn brought, so a turn-level name wins over a process-level one, the
/// same way a session fork shadows the root.
const UPSTREAM_PRIORITY: u16 = 300;

/// Typed definition for the kernel's `tool-registry` seat.
///
/// The interface is the seat itself, not `dyn ToolResolver`, because the
/// seat has two faces and both are needed through the kernel: a consumer
/// resolves tools from it, and a feature plugin registers its own onto it
/// (`ToolSeat::register_tools`). `Arc<ToolSeat>` coerces to
/// `Arc<dyn ToolResolver>` wherever only the consuming half matters.
pub struct ToolSeatService;

impl Service for ToolSeatService {
    type Interface = ToolSeat;
    const NAME: &'static str = TOOL_SEAT_SERVICE;
}

/// One source of tools behind a seat. A provider answers by name and can
/// list what it has; the seat does the precedence and the liveness.
pub trait SeatToolProvider: Send + Sync {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>>;
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

struct ProviderEntry {
    id: String,
    priority: u16,
    token: u64,
    active: Arc<AtomicBool>,
    provider: Arc<dyn SeatToolProvider>,
}

/// Provider registry behind the typed `tool-registry` service.
pub struct ToolSeat {
    entries: RwLock<Vec<ProviderEntry>>,
    next_token: AtomicU64,
}

impl ToolSeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(1),
        })
    }

    /// Register a provider on `ctx`. The registration is an effect of the
    /// context: when `ctx` is disposed (the plugin unloads, the turn ends)
    /// the provider leaves the seat and tools already handed out from it
    /// refuse to run. `id` must be unique within the seat.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        priority: Priority,
        provider: Arc<dyn SeatToolProvider>,
    ) -> Result<(), KernelError> {
        self.register_scoped(ctx, id, priority.as_u16(), provider)
    }

    /// [`register`](Self::register) for a fixed list of tools.
    pub fn register_tools(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        priority: Priority,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<(), KernelError> {
        self.register(ctx, id, priority, Arc::new(LocalToolProvider::new(tools)))
    }

    /// Canonical names of every live, enabled tool of `kind` — the derived
    /// replacement for a hand-written list such as "the file-edit tools".
    pub fn names_of_kind(&self, kind: ToolKind) -> Vec<String> {
        self.tools(None)
            .unwrap_or_default()
            .into_iter()
            .filter(|tool| tool.kind() == kind)
            .map(|tool| tool.id().as_str().to_string())
            .collect()
    }

    fn register_scoped(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        priority: u16,
        provider: Arc<dyn SeatToolProvider>,
    ) -> Result<(), KernelError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(KernelError::Other(
                "tool-registry provider id must be non-empty".into(),
            ));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let active = Arc::new(AtomicBool::new(true));
        {
            let mut entries = self.entries.write().expect("tool seat poisoned");
            if entries.iter().any(|entry| entry.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: format!("{TOOL_SEAT_SERVICE}:{id}"),
                });
            }
            entries.push(ProviderEntry {
                id: id.to_string(),
                priority,
                token,
                active: active.clone(),
                provider,
            });
            entries.sort_by(|left, right| {
                left.priority
                    .cmp(&right.priority)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }

        let weak = Arc::downgrade(self);
        let id_for_dispose = id.to_string();
        ctx.effect_labeled(&format!("tool provider({id})"), move || {
            Disposer::new(move || {
                active.store(false, Ordering::Release);
                if let Some(seat) = weak.upgrade() {
                    seat.entries
                        .write()
                        .expect("tool seat poisoned")
                        .retain(|entry| !(entry.id == id_for_dispose && entry.token == token));
                }
            })
        });
        Ok(())
    }

    fn entries(&self) -> Vec<(Arc<AtomicBool>, Arc<dyn SeatToolProvider>)> {
        self.entries
            .read()
            .expect("tool seat poisoned")
            .iter()
            .map(|entry| (entry.active.clone(), entry.provider.clone()))
            .collect()
    }
}

impl ToolResolver for ToolSeat {
    fn resolve(
        &self,
        name: &str,
        filter: Option<&ToolFilter>,
    ) -> ToolResult<Option<Arc<dyn Tool>>> {
        for (active, provider) in self.entries() {
            if !active.load(Ordering::Acquire) {
                continue;
            }
            let Some(tool) = provider.resolve(name) else {
                continue;
            };
            if !tool.is_enabled()
                || filter.is_some_and(|filter| !filter.allows(tool.id().as_str(), tool.aliases()))
            {
                return Ok(None);
            }
            return Ok(Some(Arc::new(ProviderGuardedTool { tool, active })));
        }
        Ok(None)
    }

    fn tools(&self, filter: Option<&ToolFilter>) -> ToolResult<Vec<Arc<dyn Tool>>> {
        let mut seen = HashSet::new();
        let mut tools = Vec::new();
        for (active, provider) in self.entries() {
            if !active.load(Ordering::Acquire) {
                continue;
            }
            for tool in provider.tools() {
                let name = tool.id().as_str().to_string();
                if !tool.is_enabled()
                    || seen.contains(&name)
                    || filter.is_some_and(|filter| !filter.allows(&name, tool.aliases()))
                {
                    continue;
                }
                seen.insert(name);
                seen.extend(tool.aliases().iter().map(|alias| (*alias).to_string()));
                tools.push(Arc::new(ProviderGuardedTool {
                    tool,
                    active: active.clone(),
                }) as Arc<dyn Tool>);
            }
        }
        Ok(tools)
    }
}

struct ProviderGuardedTool {
    tool: Arc<dyn Tool>,
    active: Arc<AtomicBool>,
}

impl ProviderGuardedTool {
    fn ensure_live(&self) -> ToolResult<()> {
        if self.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(ToolError::Execution {
                tool: self.tool.id(),
                source: anyhow::anyhow!(
                    "[STALE_PROVIDER] tool provider for `{}` is unloaded",
                    self.tool.id()
                ),
            })
        }
    }
}

#[async_trait]
impl Tool for ProviderGuardedTool {
    fn id(&self) -> ToolId {
        self.tool.id()
    }

    fn aliases(&self) -> &'static [&'static str] {
        self.tool.aliases()
    }

    fn description(&self) -> &str {
        self.tool.description()
    }

    fn model_description(&self) -> &str {
        self.tool.model_description()
    }

    fn input_schema(&self) -> ToolInputSchema {
        self.tool.input_schema()
    }

    fn is_enabled(&self) -> bool {
        self.active.load(Ordering::Acquire) && self.tool.is_enabled()
    }

    fn should_defer(&self) -> bool {
        self.tool.should_defer()
    }

    fn kind(&self) -> ToolKind {
        self.tool.kind()
    }

    fn file_target_field(&self) -> Option<&'static str> {
        self.tool.file_target_field()
    }

    fn search_hint(&self) -> Option<&str> {
        self.tool.search_hint()
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        self.tool.is_concurrency_safe(input)
    }

    fn is_read_only(&self, input: &Value) -> bool {
        self.tool.is_read_only(input)
    }

    fn is_destructive(&self, input: &Value) -> bool {
        self.tool.is_destructive(input)
    }

    fn needs_permission(&self, input: &Value) -> bool {
        self.tool.needs_permission(input)
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        self.ensure_live()?;
        self.tool.validate_input(input, context).await
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        self.ensure_live()?;
        self.tool.check_permissions(input, context).await
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        self.ensure_live()?;
        self.tool.call(input, context).await
    }
}

/// A fixed list of tools, matched by canonical name or alias.
pub struct LocalToolProvider {
    tools: Vec<Arc<dyn Tool>>,
}

impl LocalToolProvider {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }
}

impl SeatToolProvider for LocalToolProvider {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| {
                tool.is_enabled() && tool_matches_name(tool.id().as_str(), tool.aliases(), name)
            })
            .cloned()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// The process seat seen from a turn seat. Resolution goes through the
/// upstream resolver on every call, so a provider unloaded from the root
/// between two turns — or during one — is not answered for from a copy.
struct UpstreamSeatProvider {
    seat: Arc<dyn ToolResolver>,
}

impl SeatToolProvider for UpstreamSeatProvider {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.seat.resolve(name, None).ok().flatten()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.seat.tools(None).unwrap_or_default()
    }
}

struct PluginToolProviderAdapter {
    provider: Arc<dyn PluginToolProvider>,
}

impl SeatToolProvider for PluginToolProviderAdapter {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.provider.tool(name)
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.provider
            .tool_names()
            .into_iter()
            .filter_map(|name| self.provider.tool(&name))
            .collect()
    }
}

/// Resolver that looks the seat up through a kernel context on every call.
/// This preserves ACL errors and prevents a cached typed service from bypassing
/// a later scope replacement.
struct KernelToolResolver {
    context: Context,
    _keepalive: Option<KernelContextLease>,
    /// Direct Engine callers have no process/session kernel owner, so the
    /// resolver owns the temporary kernel for exactly as long as the seat.
    _kernel: Option<Arc<rebon_kernel::Kernel>>,
    dispose_on_drop: bool,
}

impl KernelToolResolver {
    fn owned(
        context: Context,
        keepalive: Option<KernelContextLease>,
        kernel: Option<Arc<rebon_kernel::Kernel>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            context,
            _keepalive: keepalive,
            _kernel: kernel,
            dispose_on_drop: true,
        })
    }

    #[cfg(test)]
    fn borrowed(context: Context) -> Arc<Self> {
        Arc::new(Self {
            context,
            _keepalive: None,
            _kernel: None,
            dispose_on_drop: false,
        })
    }

    fn seat(&self, tool: &str) -> ToolResult<Arc<ToolSeat>> {
        self.context
            .require::<ToolSeatService>()
            .map_err(|error| ToolError::Execution {
                tool: ToolId::new(tool),
                source: anyhow::anyhow!(error),
            })
    }
}

impl Drop for KernelToolResolver {
    fn drop(&mut self) {
        if self.dispose_on_drop {
            self.context.dispose();
        }
    }
}

impl ToolResolver for KernelToolResolver {
    fn resolve(
        &self,
        name: &str,
        filter: Option<&ToolFilter>,
    ) -> ToolResult<Option<Arc<dyn Tool>>> {
        self.seat(name)?.resolve(name, filter)
    }

    fn tools(&self, filter: Option<&ToolFilter>) -> ToolResult<Vec<Arc<dyn Tool>>> {
        self.seat("tool-registry")?.tools(filter)
    }
}

impl Engine {
    /// The process seat this engine consults for the tools it does not own:
    /// the one the turn's own context resolves, else the one on the kernel
    /// context a host attached. `None` for an engine that runs without a
    /// kernel, which then falls back to its own copy of the core set.
    pub(crate) fn upstream_tool_seat(
        &self,
        turn: Option<&Context>,
    ) -> Option<Arc<dyn ToolResolver>> {
        turn.and_then(|ctx| ctx.get::<ToolSeatService>())
            .or_else(|| {
                self.upstream_tool_context()
                    .and_then(|ctx| ctx.get::<ToolSeatService>())
            })
            .map(|seat| seat as Arc<dyn ToolResolver>)
    }

    /// Bind one turn's providers into a scoped typed kernel seat.
    pub(crate) fn scoped_tool_resolver(
        &self,
        parent: Option<KernelContextLease>,
        mcp_definitions: &[(String, McpToolDefinition)],
        plugin_tools: Option<Arc<dyn PluginToolProvider>>,
    ) -> Arc<dyn ToolResolver> {
        let (context, kernel) = match parent.as_ref() {
            Some(lease) => (lease.context().fork_scoped("tool-consumer"), None),
            None => {
                let kernel = rebon_kernel::Kernel::new();
                let context = kernel.context().fork_scoped("tool-consumer");
                (context, Some(kernel))
            }
        };
        let seat = ToolSeat::new();
        context
            .provide::<ToolSeatService>(seat.clone())
            .expect("fresh tool consumer scope has one tool-registry seat");
        seat.register_tools(
            &context,
            "local",
            Priority::Feature,
            self.host_tools().to_vec(),
        )
        .expect("fresh tool seat accepts local provider");
        let contribution = parent
            .as_ref()
            .map(KernelContextLease::context)
            .or_else(|| self.upstream_tool_context())
            .and_then(|ctx| ctx.get::<crate::mcp_runtime::McpTurnToolsService>());
        if let Some(provider) =
            contribution.and_then(|contribute| contribute(mcp_definitions.to_vec()))
        {
            seat.register(&context, "mcp", Priority::Mcp, provider)
                .expect("fresh tool seat accepts MCP contribution");
        }
        if let Some(provider) = plugin_tools {
            seat.register(
                &context,
                "plugin",
                Priority::Plugin,
                Arc::new(PluginToolProviderAdapter { provider }),
            )
            .expect("fresh tool seat accepts plugin provider");
        }
        // The primitives come from the process seat, where `core-tools` put
        // them. Without a kernel to ask, the engine's own copy of the same
        // set stands in, at the same rank, so the two paths resolve alike.
        match self.upstream_tool_seat(parent.as_ref().map(KernelContextLease::context)) {
            Some(upstream) => seat
                .register_scoped(
                    &context,
                    "upstream",
                    UPSTREAM_PRIORITY,
                    Arc::new(UpstreamSeatProvider { seat: upstream }),
                )
                .expect("fresh tool seat accepts upstream provider"),
            None => {
                if let Some(core) = self.core_fallback_tools() {
                    seat.register_scoped(
                        &context,
                        "core",
                        UPSTREAM_PRIORITY,
                        Arc::new(LocalToolProvider::new(core)),
                    )
                    .expect("fresh tool seat accepts core fallback provider");
                }
            }
        }
        KernelToolResolver::owned(context, parent, kernel)
    }

    /// Compatibility assembly for direct Engine callers that construct a
    /// `ToolContext` without the executor. Consumption still goes through the
    /// same seat; only the scope owner is synthesized locally.
    pub(crate) fn tool_resolver_for_context(&self, context: &ToolContext) -> Arc<dyn ToolResolver> {
        context.tool_resolver().cloned().unwrap_or_else(|| {
            self.scoped_tool_resolver(
                None,
                context.mcp_tool_definitions().unwrap_or_default(),
                context.plugin_tools().cloned(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::{Kernel, Plugin, PluginMeta};
    use rebon_tools_core::PermissionBehavior;
    use std::sync::Mutex;

    struct MarkerTool {
        id: &'static str,
        marker: &'static str,
        ask: bool,
    }

    #[async_trait]
    impl Tool for MarkerTool {
        fn id(&self) -> ToolId {
            ToolId::new(self.id)
        }

        fn description(&self) -> &str {
            self.marker
        }

        fn input_schema(&self) -> ToolInputSchema {
            serde_json::json!({ "type": "object" })
        }

        fn needs_permission(&self, _input: &Value) -> bool {
            self.ask
        }

        async fn check_permissions(
            &self,
            input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(if self.ask {
                PermissionDecision::ask(
                    rebon_tools_core::PermissionRequest::new("ask", self.marker),
                    Some(input.clone()),
                )
            } else {
                PermissionDecision::allow(input.clone())
            })
        }

        async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(Value::String(self.marker.to_string()))
        }
    }

    struct StaticProvider(Vec<Arc<dyn Tool>>);

    impl SeatToolProvider for StaticProvider {
        fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
            self.0
                .iter()
                .find(|tool| tool.id().as_str() == name)
                .cloned()
        }

        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.0.clone()
        }
    }

    fn marker(id: &'static str, value: &'static str, ask: bool) -> Arc<dyn Tool> {
        Arc::new(MarkerTool {
            id,
            marker: value,
            ask,
        })
    }

    #[tokio::test]
    async fn coverage_matrix_precedence_absence_and_permission_delegation() {
        let kernel = Kernel::new();
        let scope = kernel.context().fork("providers");
        let seat = ToolSeat::new();
        seat.register(
            &scope,
            "plugin",
            Priority::Plugin,
            Arc::new(StaticProvider(vec![marker("shared", "plugin", true)])),
        )
        .unwrap();
        seat.register(
            &scope,
            "local",
            Priority::Feature,
            Arc::new(StaticProvider(vec![marker("shared", "local", false)])),
        )
        .unwrap();
        seat.register(
            &scope,
            "mcp",
            Priority::Mcp,
            Arc::new(StaticProvider(vec![
                marker("shared", "mcp", true),
                marker("mcp-only", "mcp", true),
            ])),
        )
        .unwrap();

        let shared = seat.resolve("shared", None).unwrap().unwrap();
        assert_eq!(
            shared.call(Value::Null, &ToolContext::new()).await.unwrap(),
            "local"
        );
        assert!(!shared.needs_permission(&Value::Null));

        let mcp = seat.resolve("mcp-only", None).unwrap().unwrap();
        assert!(mcp.needs_permission(&Value::Null));
        assert_eq!(
            mcp.check_permissions(&Value::Null, &ToolContext::new())
                .await
                .unwrap()
                .behavior,
            PermissionBehavior::Ask
        );
        assert!(seat.resolve("absent", None).unwrap().is_none());
    }

    #[tokio::test]
    async fn coverage_matrix_held_tool_rejects_stale_provider() {
        let kernel = Kernel::new();
        let scope = kernel.context().fork("provider");
        let seat = ToolSeat::new();
        seat.register(
            &scope,
            "ephemeral",
            Priority::Mcp,
            Arc::new(StaticProvider(vec![marker("ephemeral", "old", false)])),
        )
        .unwrap();
        let held = seat.resolve("ephemeral", None).unwrap().unwrap();
        scope.dispose();

        let error = held
            .call(Value::Null, &ToolContext::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("[STALE_PROVIDER]"), "{error}");
        assert!(seat.resolve("ephemeral", None).unwrap().is_none());
    }

    struct CaptureContext(Arc<Mutex<Option<Context>>>);

    impl Plugin for CaptureContext {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("unauthorized-tool-consumer")
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            *self.0.lock().unwrap() = Some(ctx.clone());
            Ok(())
        }
    }

    #[test]
    fn coverage_matrix_unauthorized_consumer_cannot_resolve_tool_seat() {
        let kernel = Kernel::new();
        kernel
            .context()
            .provide::<ToolSeatService>(ToolSeat::new())
            .unwrap();
        let captured = Arc::new(Mutex::new(None));
        kernel
            .load(vec![Box::new(CaptureContext(captured.clone()))])
            .unwrap();
        let context = captured.lock().unwrap().clone().unwrap();
        let resolver = KernelToolResolver::borrowed(context);
        let error = match resolver.resolve("anything", None) {
            Ok(_) => panic!("undeclared tool-registry resolution must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("[UNAUTHORIZED_RESOLVE]"),
            "{error}"
        );
    }

    /// A tool registered on the process seat is visible to a turn that never
    /// heard of it, and gone from the next turn once its scope is disposed;
    /// a copy held across the unload refuses to run.
    #[tokio::test]
    async fn process_seat_registrations_reach_the_turn_and_leave_with_their_scope() {
        let kernel = Kernel::new();
        let process_seat = ToolSeat::new();
        kernel
            .context()
            .provide::<ToolSeatService>(process_seat.clone())
            .unwrap();
        let plugin_scope = kernel.context().fork_scoped("core-tools");
        process_seat
            .register_tools(
                &plugin_scope,
                "core",
                Priority::Core,
                vec![marker("Read", "process", false)],
            )
            .unwrap();

        let engine = Engine::new();
        assert!(engine.attach_upstream_tool_context(kernel.context().clone()));
        let turn = engine.scoped_tool_resolver(None, &[], None);
        let held = turn
            .resolve("Read", None)
            .unwrap()
            .expect("Read via upstream");
        assert_eq!(
            held.call(Value::Null, &ToolContext::new()).await.unwrap(),
            "process"
        );
        assert!(turn
            .tools(None)
            .unwrap()
            .iter()
            .any(|tool| tool.id().as_str() == "Read"));

        plugin_scope.dispose();
        let next_turn = engine.scoped_tool_resolver(None, &[], None);
        assert!(next_turn.resolve("Read", None).unwrap().is_none());
        let error = held
            .call(Value::Null, &ToolContext::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("[STALE_PROVIDER]"), "{error}");
    }

    /// The turn's own lease context is asked first: a session fork under the
    /// root reaches the root's seat without the engine holding anything.
    #[test]
    fn a_turn_lease_under_the_root_finds_the_process_seat_by_itself() {
        let kernel = Kernel::new();
        let process_seat = ToolSeat::new();
        kernel
            .context()
            .provide::<ToolSeatService>(process_seat.clone())
            .unwrap();
        process_seat
            .register_tools(
                kernel.context(),
                "core",
                Priority::Core,
                vec![marker("Read", "process", false)],
            )
            .unwrap();
        let session = kernel.context().fork_scoped("session/s1");
        let lease = KernelContextLease::unmanaged(session);

        let engine = Engine::new();
        let turn = engine.scoped_tool_resolver(Some(lease), &[], None);
        assert!(turn.resolve("Read", None).unwrap().is_some());
    }

    /// A turn-level name shadows the process-level one: the upstream seat is
    /// the last provider asked.
    #[tokio::test]
    async fn the_turn_seat_shadows_the_process_seat() {
        let kernel = Kernel::new();
        let process_seat = ToolSeat::new();
        kernel
            .context()
            .provide::<ToolSeatService>(process_seat.clone())
            .unwrap();
        process_seat
            .register_tools(
                kernel.context(),
                "core",
                Priority::Core,
                vec![marker("Shared", "process", false)],
            )
            .unwrap();
        let mut engine = Engine::new();
        engine.register_tool(marker("Shared", "host", false));
        engine.attach_upstream_tool_context(kernel.context().clone());
        let turn = engine.scoped_tool_resolver(None, &[], None);
        let shared = turn.resolve("Shared", None).unwrap().unwrap();
        assert_eq!(
            shared.call(Value::Null, &ToolContext::new()).await.unwrap(),
            "host"
        );
        assert_eq!(
            turn.tools(None)
                .unwrap()
                .iter()
                .filter(|tool| tool.id().as_str() == "Shared")
                .count(),
            1,
            "one entry per name in the listing"
        );
    }

    /// Without any kernel, the engine's own copy of the core set stands in
    /// for the process seat: headless callers keep `Read`, and what the
    /// engine snapshots is exactly what the turn seat lists.
    #[test]
    fn kernel_less_engine_keeps_the_primitives_and_snapshots_match_the_seat() {
        let engine = Engine::with_builtin_tools();
        let turn = engine.scoped_tool_resolver(None, &[], None);
        assert!(turn.resolve("Read", None).unwrap().is_some());
        assert!(turn.resolve("Bash", None).unwrap().is_some());

        let mut seat_names: Vec<String> = turn
            .tools(None)
            .unwrap()
            .iter()
            .map(|tool| tool.id().as_str().to_string())
            .collect();
        seat_names.sort();
        let mut snapshot_names: Vec<String> = engine
            .tool_snapshots()
            .into_iter()
            .map(|snapshot| snapshot.name)
            .collect();
        snapshot_names.sort();
        assert_eq!(seat_names, snapshot_names);
        for primitive in [
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "ToolSearch",
            "Sleep",
        ] {
            assert!(
                snapshot_names.iter().any(|name| name == primitive),
                "{primitive}"
            );
        }
    }

    /// The derived name list for a kind, from a seat holding the core set.
    #[test]
    fn names_of_kind_are_derived_from_the_tools_themselves() {
        let kernel = Kernel::new();
        let seat = ToolSeat::new();
        seat.register_tools(
            kernel.context(),
            "core",
            Priority::Core,
            rebon_tool::core_tool_set(rebon_tool::BashTool::new()),
        )
        .unwrap();
        let mut edits = seat.names_of_kind(ToolKind::FileEdit);
        edits.sort();
        assert_eq!(edits, vec!["Edit", "MultiEdit", "Write"]);
        // Which shells are enabled follows the host's shell preference;
        // whatever is enabled is a shell and nothing else is.
        let shells = seat.names_of_kind(ToolKind::Shell);
        assert!(!shells.is_empty());
        assert!(shells
            .iter()
            .all(|name| name == "Bash" || name == "PowerShell"));
        assert_eq!(seat.names_of_kind(ToolKind::FileRead), vec!["Read"]);
    }
}
