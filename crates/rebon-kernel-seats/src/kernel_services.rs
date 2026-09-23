//! Session-scoped kernel services: the first real seams carved out of
//! the legacy stack. Each session forks a scoped kernel context; services
//! registered on it are visible to that session's kernel consumers (JS
//! plugins, diagnostics) and vanish with the fork — concurrent sessions
//! never collide.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_core::Engine;
use rebon_kernel::{Context, Kernel, SessionClosed, SessionOpened};

/// 返回会话 context 与共享插件工具表。Code Mode 单独挂在 context 的
/// `session-tools` 服务上，由持有会话 lease 的工具解析器读取。
pub fn bootstrap_session_context_with_tools(
    kernel: &Kernel,
    session_id: &str,
    engine: &Arc<Engine>,
) -> (
    Context,
    Arc<crate::kernel_tool_dispatch::SessionPluginTools>,
) {
    let scope = bind_session_scope(kernel, session_id, engine, None, None);
    // The seat is owned by the scope's registry; callers on this legacy
    // entry point keep only what they asked for.
    let SessionKernelScope { ctx, tools, .. } = scope;
    (ctx, tools)
}

/// The session's plugin-tool registry.
pub type SessionPluginToolsHandle = Arc<crate::kernel_tool_dispatch::SessionPluginTools>;

/// One generation of a session's kernel scope: the fork every
/// session-scoped service hangs on, plus the handles a host has to keep.
pub struct SessionKernelScope {
    /// Disposing this fork revokes everything registered for the generation.
    pub ctx: Context,
    /// The plugin-tool registry registered as `tool-registry`.
    pub tools: SessionPluginToolsHandle,
    /// The `session` seat, held so it lives as long as the scope.
    pub seat: Arc<crate::kernel_session_seat::SessionSeat>,
    /// The session's single task runtime identity. The typed service on `ctx`
    /// is the only consumer-facing path; this host handle preserves state
    /// across a generation replacement and while the feature is disabled.
    pub task_registry: Arc<rebon_plugin_tasks::runtime::TaskRegistry>,
}

/// Build one unbounded session scope: the scoped fork, `tool-registry`, Code
/// Mode's `run_code`, and the `session` seat.
///
/// This compatibility helper can replace a previously owned scope while
/// preserving its plugin-tool registry. Long-lived or multi-session hosts
/// should use [`SessionKernelScopes`] so active turns own generation leases.
pub fn bind_session_scope(
    kernel: &Kernel,
    session_id: &str,
    engine: &Arc<Engine>,
    projects_root: Option<std::path::PathBuf>,
    replacing: Option<SessionKernelScope>,
) -> SessionKernelScope {
    let reuse_tools = replacing.as_ref().map(|previous| previous.tools.clone());
    let reuse_task_registry = replacing
        .as_ref()
        .map(|previous| previous.task_registry.clone());
    if let Some(previous) = replacing {
        kernel.context().emit(&SessionClosed {
            session_id: session_id.to_string(),
        });
        previous.ctx.dispose();
    }
    bind_session_scope_with_label(
        kernel,
        session_id,
        &format!("session/{session_id}"),
        engine,
        projects_root,
        reuse_tools,
        reuse_task_registry,
    )
}

/// [`bind_session_scope`] with an explicit registry to register on this
/// session's fork.
///
/// A host that serves several sessions from one executor needs them all on
/// the registry that executor was given: plugins register tools into the
/// composition, not into a session, so the instance is shared while each
/// session gets its own fork and its own `session` seat.
pub fn bind_session_scope_reusing_tools(
    kernel: &Kernel,
    session_id: &str,
    engine: &Arc<Engine>,
    projects_root: Option<std::path::PathBuf>,
    reuse_tools: Option<SessionPluginToolsHandle>,
) -> SessionKernelScope {
    bind_session_scope_with_label(
        kernel,
        session_id,
        &format!("session/{session_id}"),
        engine,
        projects_root,
        reuse_tools,
        None,
    )
}

fn bind_session_scope_with_label(
    kernel: &Kernel,
    session_id: &str,
    scope_label: &str,
    engine: &Arc<Engine>,
    projects_root: Option<std::path::PathBuf>,
    reuse_tools: Option<SessionPluginToolsHandle>,
    reuse_task_registry: Option<Arc<rebon_plugin_tasks::runtime::TaskRegistry>>,
) -> SessionKernelScope {
    let ctx = kernel.context().fork_scoped(scope_label);
    let tools = {
        let tools = reuse_tools.unwrap_or_else(|| {
            crate::kernel_tool_dispatch::SessionPluginTools::new(engine.clone())
        });
        // Code Mode 是会话私有工具，不能注册到多个会话复用的 tools。
        let default_on = crate::kernel_code_mode::default_on_in(
            &rebon_config::config_home_dir(),
            &std::env::current_dir().expect("session working directory must exist"),
        );
        let run_code =
            crate::kernel_code_mode::RunCodeTool::new(engine.clone(), ctx.clone(), default_on);
        let session_tools = crate::kernel_tool_dispatch::SessionPluginTools::new(engine.clone());
        session_tools.set_run_code(run_code.clone());
        ctx.provide::<crate::kernel_code_mode::CodeModeSessionService>(run_code)
            .expect("会话 Code Mode 服务注册失败");
        ctx.provide::<rebon_core::tool_seat::SessionToolsService>(session_tools)
            .expect("会话工具服务注册失败");
        // The service registry keys both planes by one name, so a JSON-only
        // `tool-registry` on this fork would hide the root's typed seat from
        // every lookup made under it. Re-provide the process seat here as
        // the typed half, and the engine learns where that seat lives so
        // its catalog reads it too.
        let process_seat = kernel
            .context()
            .get::<rebon_core::tool_seat::ToolSeatService>();
        engine.attach_upstream_tool_context(kernel.context().clone());
        let registered = match process_seat {
            Some(seat) => {
                ctx.provide_dual::<rebon_core::tool_seat::ToolSeatService>(seat, tools.clone())
            }
            None => ctx.provide_json("tool-registry", tools.clone()),
        };
        if let Err(err) = registered {
            // A failure here must not take the session down — the service is
            // additive; the legacy paths keep working without it.
            tracing::warn!(%err, session = %session_id, "tool-registry registration failed");
        }
        tools
    };
    // The session seat — dsh-session's append/deriveMessages face over
    // the authoritative transcript. It is bound to this session id within the
    // exact generation retained by the turn lease.
    let seat = crate::kernel_session_seat::SessionSeat::provide(
        &ctx,
        session_id,
        projects_root.unwrap_or_else(rebon_session::default_projects_root),
    );
    // Which session this scope is. Provided before the announcement so a
    // `SessionOpened` handler that asks already gets an answer, and on this
    // generation's own service layer so disposing the generation takes it
    // out of the namespace. Additive: a scope that cannot take it still
    // gets every tool, which is what the model sees.
    if let Err(err) = rebon_core::session_scope::provide(&ctx, session_id) {
        tracing::warn!(%err, session = %session_id, "session scope not armed");
    }
    let task_registry = reuse_task_registry
        .unwrap_or_else(|| Arc::new(rebon_plugin_tasks::runtime::TaskRegistry::new()));
    rebon_plugin_tasks::provide_task_registry(&ctx, task_registry.clone())
        .expect("a fresh session scope must accept its task-registry binding");
    // Session-aware plugins fork under this scope; it goes with the session.
    kernel.context().emit(&SessionOpened {
        session_id: session_id.to_string(),
        ctx: ctx.clone(),
    });
    SessionKernelScope {
        ctx,
        tools,
        seat,
        task_registry,
    }
}

/// Per-session kernel-scope generations for a host that serves many sessions
/// from one executor.
///
/// One shared scope would put every session's `permission/ask` middleware on
/// the same label, so one session's plugins could answer another's request,
/// and would serve the `session` seat over whichever session happened to be
/// bound first. Each prompt therefore acquires an exact generation holder.
///
/// The table is bounded. Eviction, acquisition, closing, drain, and disposal
/// are coordinated so active or detached turns keep their generation intact.
struct SessionKernelScopeEntry {
    session_id: String,
    generation: u64,
    /// Keeps the parent root scope alive until this generation is disposed.
    _kernel: Arc<Kernel>,
    holders: AtomicUsize,
    closing: AtomicBool,
    disposed: AtomicBool,
    scope: SessionKernelScope,
}

impl SessionKernelScopeEntry {
    fn mark_closing(&self) {
        self.closing.store(true, Ordering::Release);
    }

    fn close_drain_dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self._kernel.context().emit(&SessionClosed {
            session_id: self.session_id.clone(),
        });
        self.scope.ctx.close_leases();
        while self.scope.ctx.lease_inflight() != 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.scope.ctx.dispose();
    }
}

struct SessionKernelScopeLeaseInner {
    entry: Arc<SessionKernelScopeEntry>,
}

impl Drop for SessionKernelScopeLeaseInner {
    fn drop(&mut self) {
        let previous = self.entry.holders.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "kernel scope holder count underflow");
        if previous == 1 && self.entry.closing.load(Ordering::Acquire) {
            self.entry.close_drain_dispose();
        }
    }
}

/// Whole-turn ownership of one exact session-scope generation.
///
/// Cloning this value clones the same logical holder. The table's holder count
/// reaches zero only when the final clone drops, so detached work can retain a
/// generation independently of the foreground session moving elsewhere.
#[derive(Clone)]
pub struct SessionKernelScopeLease {
    inner: Arc<SessionKernelScopeLeaseInner>,
}

impl std::fmt::Debug for SessionKernelScopeLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKernelScopeLease")
            .field("session_id", &self.inner.entry.session_id)
            .field("generation", &self.inner.entry.generation)
            .finish_non_exhaustive()
    }
}

impl SessionKernelScopeLease {
    fn acquired(entry: Arc<SessionKernelScopeEntry>) -> Self {
        Self {
            inner: Arc::new(SessionKernelScopeLeaseInner { entry }),
        }
    }

    pub fn context(&self) -> &Context {
        &self.inner.entry.scope.ctx
    }

    pub fn generation(&self) -> u64 {
        self.inner.entry.generation
    }

    pub fn into_engine_lease(self) -> rebon_core::permission::KernelContextLease {
        let context = self.context().clone();
        rebon_core::permission::KernelContextLease::managed(context, self)
    }
}

struct SessionKernelScopeTable {
    /// Least-recently-used first.
    entries: Vec<Arc<SessionKernelScopeEntry>>,
    /// Registry identity survives a scope-generation replacement whenever a
    /// legitimate Arc holder is still running. Weak entries neither create a
    /// process-global singleton nor keep closed sessions alive.
    task_registry_owners: std::collections::HashMap<
        String,
        std::sync::Weak<rebon_plugin_tasks::runtime::TaskRegistry>,
    >,
}

// Scope labels live on the kernel's shared event bus, not inside one table.
// A per-table counter can therefore recreate `session-generation/1` after a
// host rebuild while an old lease still dispatches on that same bus. Process
// uniqueness is stronger than the required per-kernel uniqueness and keeps
// sibling table instances from ever aliasing one another.
static NEXT_SESSION_SCOPE_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_session_scope_generation() -> u64 {
    NEXT_SESSION_SCOPE_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            generation.checked_add(1)
        })
        .expect("kernel session-scope generation exhausted")
}

/// Bounded generation table for hosts that serve many sessions through one
/// executor. Acquisition and eviction admission share one mutex, so an entry
/// cannot pass a zero-holder check while another thread is acquiring it.
pub struct SessionKernelScopes {
    kernel: Arc<Kernel>,
    engine: Arc<Engine>,
    projects_root: std::path::PathBuf,
    /// Shared by every generation here, because it is what the one executor
    /// was handed. Plugins register tools into the composition rather than
    /// into a session, so a per-generation registry would leave the executor
    /// dispatching from a table nobody writes to.
    tools: SessionPluginToolsHandle,
    table: Mutex<SessionKernelScopeTable>,
    limit: usize,
}

impl SessionKernelScopes {
    /// Default cap on live session scopes. Well above any realistic number
    /// of concurrent ACP sessions; it exists so a client that mints sessions
    /// forever cannot grow the kernel tree without bound.
    pub const DEFAULT_LIMIT: usize = 32;

    pub fn new(
        kernel: Arc<Kernel>,
        engine: Arc<Engine>,
        projects_root: std::path::PathBuf,
    ) -> Arc<Self> {
        Self::with_limit(kernel, engine, projects_root, Self::DEFAULT_LIMIT)
    }

    pub fn with_limit(
        kernel: Arc<Kernel>,
        engine: Arc<Engine>,
        projects_root: std::path::PathBuf,
        limit: usize,
    ) -> Arc<Self> {
        let tools = crate::kernel_tool_dispatch::SessionPluginTools::new(engine.clone());
        Arc::new(Self {
            kernel,
            engine,
            projects_root,
            tools,
            table: Mutex::new(SessionKernelScopeTable {
                entries: Vec::new(),
                task_registry_owners: std::collections::HashMap::new(),
            }),
            limit: limit.max(1),
        })
    }

    /// The plugin-tool registry every session generation registers, and the
    /// one a host must hand to its executor.
    pub fn plugin_tools(&self) -> SessionPluginToolsHandle {
        self.tools.clone()
    }

    /// Acquire one logical holder for the exact generation currently mapped
    /// to `session_id`. Capacity eviction is admitted under the same lock.
    pub fn acquire(&self, session_id: &str) -> SessionKernelScopeLease {
        let (entry, evicted) = {
            let mut table = self
                .table
                .lock()
                .expect("kernel session-scope table poisoned");
            let entry = if let Some(index) = table
                .entries
                .iter()
                .position(|entry| entry.session_id == session_id)
            {
                let entry = table.entries.remove(index);
                entry.holders.fetch_add(1, Ordering::AcqRel);
                table.entries.push(entry.clone());
                entry
            } else {
                let generation = next_session_scope_generation();
                let reuse_task_registry = table
                    .task_registry_owners
                    .get(session_id)
                    .and_then(std::sync::Weak::upgrade);
                let scope = bind_session_scope_with_label(
                    &self.kernel,
                    session_id,
                    &format!("session-generation/{generation}"),
                    &self.engine,
                    Some(self.projects_root.clone()),
                    Some(self.tools.clone()),
                    reuse_task_registry,
                );
                table
                    .task_registry_owners
                    .insert(session_id.to_string(), Arc::downgrade(&scope.task_registry));
                let entry = Arc::new(SessionKernelScopeEntry {
                    session_id: session_id.to_string(),
                    generation,
                    _kernel: self.kernel.clone(),
                    holders: AtomicUsize::new(1),
                    closing: AtomicBool::new(false),
                    disposed: AtomicBool::new(false),
                    scope,
                });
                table.entries.push(entry.clone());
                entry
            };

            let mut evicted = Vec::new();
            while table.entries.len() > self.limit {
                let Some(index) = table
                    .entries
                    .iter()
                    .position(|candidate| candidate.holders.load(Ordering::Acquire) == 0)
                else {
                    tracing::warn!(
                        live = table.entries.len(),
                        limit = self.limit,
                        "every kernel session scope has a holder; exceeding the cap"
                    );
                    break;
                };
                let victim = table.entries.remove(index);
                // Mark and remove while acquisition is excluded. From this
                // point the same session label can only acquire a fresh entry.
                victim.mark_closing();
                evicted.push(victim);
            }
            (entry, evicted)
        };

        for victim in evicted {
            tracing::debug!(
                session = %victim.session_id,
                generation = victim.generation,
                "evicting idle kernel session-scope generation"
            );
            victim.close_drain_dispose();
        }
        SessionKernelScopeLease::acquired(entry)
    }

    /// Acquire a holder only when the host has already bound this session.
    /// Consumer-side resolution must never mint a scope (and therefore a new
    /// registry) from an arbitrary session id.
    fn acquire_existing(&self, session_id: &str) -> Option<SessionKernelScopeLease> {
        let entry = {
            let mut table = self
                .table
                .lock()
                .expect("kernel session-scope table poisoned");
            let index = table
                .entries
                .iter()
                .position(|entry| entry.session_id == session_id)?;
            let entry = table.entries.remove(index);
            entry.holders.fetch_add(1, Ordering::AcqRel);
            table.entries.push(entry.clone());
            entry
        };
        Some(SessionKernelScopeLease::acquired(entry))
    }

    /// Host ownership handle for wiring UI/storage lifetimes. It is the same
    /// Arc bound on an already-created typed seat, not a second registry
    /// source or an implicit session binder.
    pub fn host_task_registry(
        &self,
        session_id: &str,
    ) -> Arc<rebon_plugin_tasks::runtime::TaskRegistry> {
        self.acquire_existing(session_id)
            .expect("session must be bound before host task access")
            .context()
            .require::<rebon_plugin_tasks::TaskRegistryService>()
            .expect("session scope must carry task-registry")
            .host_registry()
    }

    pub fn task_registry_resolver(self: &Arc<Self>) -> rebon_plugin_tasks::TaskRegistryResolver {
        let scopes = self.clone();
        let resolver: rebon_core::permission::KernelContextLeaseResolver =
            Arc::new(move |session_id| {
                scopes
                    .acquire_existing(session_id)
                    .map(SessionKernelScopeLease::into_engine_lease)
            });
        rebon_plugin_tasks::TaskRegistryResolver::new(resolver)
    }

    pub fn resolver(self: &Arc<Self>) -> rebon_core::permission::KernelContextLeaseResolver {
        let scopes = self.clone();
        Arc::new(move |session_id| Some(scopes.acquire(session_id).into_engine_lease()))
    }

    #[cfg(test)]
    fn live_entries(&self) -> Vec<(String, u64, usize)> {
        self.table
            .lock()
            .expect("kernel session-scope table poisoned")
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.session_id.clone(),
                    entry.generation,
                    entry.holders.load(Ordering::Acquire),
                )
            })
            .collect()
    }
}

impl Drop for SessionKernelScopes {
    fn drop(&mut self) {
        let table = self
            .table
            .get_mut()
            .expect("kernel session-scope table poisoned");
        let entries = std::mem::take(&mut table.entries);
        for entry in &entries {
            entry.mark_closing();
        }
        for entry in entries {
            if entry.holders.load(Ordering::Acquire) == 0 {
                entry.close_drain_dispose();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::{JsonService, KernelError};
    use std::path::Path;

    fn engine_with_builtin_tools() -> Arc<Engine> {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(rebon_tool::AgentRegistry::load_with_plugin_dirs(
            dir.path(),
            &dir.path().join("config"),
            &[],
        ));
        let _ = Path::new("."); // keep the import obviously used on all cfgs
        rebon_tool::set_agent_registry_selection(registry, false);
        Arc::new(Engine::with_builtin_tools())
    }

    #[test]
    fn tool_registry_lists_builtin_tools_per_session() {
        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        let (ctx, _tools) = bootstrap_session_context_with_tools(&kernel, "sess-a", &engine);

        let out = ctx
            .call_json("tool-registry", "list", serde_json::Value::Null)
            .expect("list resolves");
        let tools = out["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty(), "builtin engine must expose eager tools");
        for tool in tools {
            assert!(tool["name"].is_string());
            assert!(tool["inputSchema"].is_object() || tool["inputSchema"].is_boolean());
        }
        assert!(out["deferred"].is_array());

        // Session isolation: the root namespace must stay clean, and a
        // sibling session gets its own instance.
        assert!(kernel.context().get_json("tool-registry").is_none());
        let (sibling, _sibling_tools) =
            bootstrap_session_context_with_tools(&kernel, "sess-b", &engine);
        assert!(sibling.get_json("tool-registry").is_some());

        // Disposal removes the service with the fork.
        ctx.dispose();
        assert!(ctx.get_json("tool-registry").is_none());
    }

    #[test]
    fn a_session_scope_announces_itself_opening_and_closing() {
        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let opened = seen.clone();
        kernel.context().on::<SessionOpened>(move |event| {
            opened.lock().unwrap().push(format!(
                "open {} at {}",
                event.session_id,
                event.ctx.label()
            ));
        });
        let closed = seen.clone();
        kernel.context().on::<SessionClosed>(move |event| {
            closed
                .lock()
                .unwrap()
                .push(format!("close {}", event.session_id));
        });

        let first = bind_session_scope(&kernel, "sess-x", &engine, None, None);
        // A plugin listening to SessionOpened forks under the session and
        // registers there; the fork must vanish with the session.
        struct Echo;
        impl JsonService for Echo {
            fn call(
                &self,
                _method: &str,
                params: serde_json::Value,
            ) -> Result<serde_json::Value, KernelError> {
                Ok(params)
            }
        }
        let plugin_fork = first.ctx.fork("probe-plugin");
        plugin_fork
            .provide_json("probe-svc", Arc::new(Echo))
            .expect("plugin registers under the session");
        assert!(first.ctx.has_service("probe-svc"));

        let second = bind_session_scope(&kernel, "sess-x", &engine, None, Some(first));
        assert!(
            !second.ctx.has_service("probe-svc"),
            "rebinding disposes the previous scope and everything forked under it"
        );
        second.ctx.dispose();

        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "open sess-x at session/sess-x".to_string(),
                "close sess-x".to_string(),
                "open sess-x at session/sess-x".to_string(),
            ]
        );
    }

    #[test]
    fn code_mode_is_not_registered_in_shared_session_tools_by_default() {
        use rebon_tool::PluginToolProvider;
        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        for session_id in ["code-mode-first", "code-mode-second"] {
            let scope = bind_session_scope(&kernel, session_id, &engine, None, None);
            assert!(!scope.tools.tool_names().contains(&"run_code".to_string()));
        }
    }

    #[test]
    fn a_new_session_uses_code_mode_default_only_while_experiment_is_open() {
        use crate::kernel_code_mode::{command, CodeModePlugin};
        use rebon_kernel::Plugin;
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("code-mode-default");
        std::fs::write(
            home.path().join("settings.json"),
            r#"{"plugins":{"code-mode":{"enabled":true,"defaultOn":true}}}"#,
        )
        .unwrap();
        let kernel = Kernel::new();
        let engine = Arc::new(Engine::new());
        let closed = bind_session_scope(&kernel, "closed", &engine, None, None);
        assert!(command(&closed.ctx, &[]).unwrap().contains("off"));
        let experiment = kernel.context().fork("experiment");
        CodeModePlugin.apply(&experiment).unwrap();
        let first = bind_session_scope(&kernel, "first", &engine, None, None);
        assert!(command(&first.ctx, &[]).unwrap().contains("on"));
        command(&first.ctx, &["off".into()]).unwrap();
        let second = bind_session_scope(&kernel, "second", &engine, None, None);
        assert!(command(&first.ctx, &[]).unwrap().contains("off"));
        assert!(command(&second.ctx, &[]).unwrap().contains("on"));
        experiment.dispose();
        assert!(command(&second.ctx, &[]).unwrap().contains("off"));
    }

    #[test]
    fn code_mode_session_command_persists_between_leases_and_isolates_sessions() {
        use crate::kernel_code_mode::{command, CodeModePlugin};
        use rebon_kernel::Plugin;
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("code-mode-sessions");
        let (_projects, scopes) = scope_table(4);
        let first = scopes.acquire("first");
        assert!(command(first.context(), &[]).unwrap().contains("off"));
        assert!(command(first.context(), &["on".into()])
            .unwrap_err()
            .contains("code-mode"));
        let experiment = scopes.kernel.context().fork("experiment");
        CodeModePlugin.apply(&experiment).unwrap();
        assert!(command(first.context(), &[]).unwrap().contains("off"));
        command(first.context(), &["on".into()]).unwrap();
        drop(first);
        let next_turn = scopes.acquire("first");
        assert!(command(next_turn.context(), &[]).unwrap().contains("on"));
        let second = scopes.acquire("second");
        assert!(command(second.context(), &[]).unwrap().contains("off"));
        command(next_turn.context(), &["off".into()]).unwrap();
        assert!(command(next_turn.context(), &[]).unwrap().contains("off"));
        command(next_turn.context(), &["on".into()]).unwrap();
        experiment.dispose();
        assert!(command(next_turn.context(), &[]).unwrap().contains("off"));
    }

    /// Every session the host binds carries the session-scope seat, and it
    /// is there before the announcement — a `SessionOpened` handler that
    /// asks gets an answer rather than a race.
    ///
    /// Four feature plugins used to each fork and provide a marker of their
    /// own to answer this; the seat is the one the host provides for all of
    /// them (`rebon_core::session_scope`).
    #[test]
    fn a_bound_session_carries_its_scope_before_it_is_announced() {
        use rebon_core::session_scope::{session_scope_is, SESSION_SCOPE_SERVICE};

        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        let answered: Arc<std::sync::Mutex<Option<bool>>> = Arc::new(std::sync::Mutex::new(None));
        let seen = answered.clone();
        kernel.context().on::<SessionOpened>(move |event| {
            *seen.lock().unwrap() = Some(session_scope_is(&event.ctx, &event.session_id));
        });

        let scope = bind_session_scope(&kernel, "sess-scope", &engine, None, None);

        assert_eq!(
            *answered.lock().unwrap(),
            Some(true),
            "the scope is provided before SessionOpened goes out"
        );
        assert!(session_scope_is(&scope.ctx, "sess-scope"));
        assert!(session_scope_is(
            &scope.ctx.fork("some-plugin"),
            "sess-scope"
        ));
        assert!(!session_scope_is(&scope.ctx, "sess-other"));

        scope.ctx.dispose();
        assert!(
            !scope.ctx.has_service(SESSION_SCOPE_SERVICE),
            "disposing the generation takes the scope out of the namespace"
        );
    }

    /// Two live sessions each answer for themselves. This is what the four
    /// plugin copies of the pilot test were pinning, once.
    #[test]
    fn two_bound_sessions_do_not_cross() {
        use rebon_core::session_scope::session_scope_is;

        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        let first = bind_session_scope(&kernel, "sess-a", &engine, None, None);
        let second = bind_session_scope(&kernel, "sess-b", &engine, None, None);

        assert!(session_scope_is(&first.ctx, "sess-a"));
        assert!(session_scope_is(&second.ctx, "sess-b"));
        assert!(!session_scope_is(&first.ctx, "sess-b"));

        first.ctx.dispose();
        assert!(!session_scope_is(&first.ctx, "sess-a"));
        assert!(
            session_scope_is(&second.ctx, "sess-b"),
            "one session closing must not disarm the others"
        );
        second.ctx.dispose();
    }

    #[test]
    fn task_registry_is_session_scoped_and_rebind_preserves_its_identity() {
        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        let first = bind_session_scope(&kernel, "sess-a", &engine, None, None);
        let sibling = bind_session_scope(&kernel, "sess-b", &engine, None, None);
        let first_registry = first.task_registry.clone();
        let sibling_registry = sibling.task_registry.clone();
        assert!(!Arc::ptr_eq(&first_registry, &sibling_registry));

        let task_id = rebon_plugin_tasks::runtime::TaskId::new("only-a");
        first_registry.insert(
            task_id.clone(),
            rebon_plugin_tasks::runtime::TaskSnapshot::new_pending(
                task_id.clone(),
                "session A".into(),
                rebon_plugin_tasks::runtime::TaskData::MonitorMcp(
                    rebon_plugin_tasks::runtime::MonitorMcpData {
                        server_name: "test".into(),
                        description: "session A".into(),
                    },
                ),
            ),
            rebon_types::PromptCancel::new(),
        );
        assert!(sibling_registry.snapshot(&task_id).is_none());

        let stale_ctx = first.ctx.clone();
        let rebound = bind_session_scope(&kernel, "sess-a", &engine, None, Some(first));
        assert!(stale_ctx
            .get::<rebon_plugin_tasks::TaskRegistryService>()
            .is_none());
        assert!(Arc::ptr_eq(&rebound.task_registry, &first_registry));
        assert!(rebound.task_registry.snapshot(&task_id).is_some());

        rebound.ctx.dispose();
        assert!(rebound
            .ctx
            .get::<rebon_plugin_tasks::TaskRegistryService>()
            .is_none());
        assert!(first_registry.snapshot(&task_id).is_some());
        sibling.ctx.dispose();
    }

    #[test]
    fn an_evicted_generation_reuses_a_registry_held_by_running_work() {
        let (_projects, scopes) = scope_table(1);
        let old = scopes.acquire("same");
        let held_registry = old
            .context()
            .require::<rebon_plugin_tasks::TaskRegistryService>()
            .expect("task registry seat")
            .host_registry();
        let old_ctx = old.context().clone();
        drop(old);
        let other = scopes.acquire("other");
        drop(other);

        let fresh = scopes.acquire("same");
        let fresh_registry = fresh
            .context()
            .require::<rebon_plugin_tasks::TaskRegistryService>()
            .expect("replacement task registry seat")
            .host_registry();
        assert!(old_ctx
            .get::<rebon_plugin_tasks::TaskRegistryService>()
            .is_none());
        assert!(Arc::ptr_eq(&held_registry, &fresh_registry));
    }

    #[test]
    fn task_registry_resolver_never_creates_an_unbound_or_evicted_scope() {
        let (_projects, scopes) = scope_table(1);
        let resolver = scopes.task_registry_resolver();

        assert!(resolver.resolve("missing").is_err());
        assert!(scopes.live_entries().is_empty());

        let bound = scopes.acquire("bound");
        scopes
            .kernel
            .context()
            .emit(&rebon_kernel::PluginStateChanged {
                id: rebon_plugin_tasks::PLUGIN_ID.to_string(),
                from: rebon_kernel::PluginState::Disabled,
                to: rebon_kernel::PluginState::Loaded,
                generation: 1,
            });
        let held_registry = resolver.resolve("bound").expect("bound registry resolves");
        drop(bound);
        let replacement_trigger = scopes.acquire("other");
        drop(replacement_trigger);

        assert!(resolver.resolve("bound").is_err());
        let live = scopes.live_entries();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, "other");
        assert_eq!(live[0].2, 0);

        let rebound = scopes.acquire("bound");
        scopes
            .kernel
            .context()
            .emit(&rebon_kernel::PluginStateChanged {
                id: rebon_plugin_tasks::PLUGIN_ID.to_string(),
                from: rebon_kernel::PluginState::Disabled,
                to: rebon_kernel::PluginState::Loaded,
                generation: 2,
            });
        let rebound_registry = resolver
            .resolve("bound")
            .expect("rebound registry resolves");
        assert!(Arc::ptr_eq(&held_registry, &rebound_registry));
        drop(rebound);
    }

    #[test]
    fn tool_registry_rejects_unknown_methods() {
        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        let (ctx, _tools) = bootstrap_session_context_with_tools(&kernel, "sess-err", &engine);
        let err = ctx
            .call_json("tool-registry", "mutate", serde_json::Value::Null)
            .expect_err("unknown method must error");
        assert!(err.to_string().contains("no method"), "{err}");
    }

    fn scope_table(limit: usize) -> (tempfile::TempDir, Arc<SessionKernelScopes>) {
        let projects = tempfile::tempdir().expect("projects root");
        let scopes = SessionKernelScopes::with_limit(
            Kernel::new(),
            engine_with_builtin_tools(),
            projects.path().to_path_buf(),
            limit,
        );
        (projects, scopes)
    }

    #[test]
    fn active_holder_is_not_evicted_when_capacity_is_exceeded() {
        let (_projects, scopes) = scope_table(1);
        let active = scopes.acquire("active");
        let active_ctx = active.context().clone();
        let idle = scopes.acquire("idle");
        let idle_ctx = idle.context().clone();
        assert_eq!(scopes.live_entries().len(), 2);

        drop(idle);
        let newcomer = scopes.acquire("newcomer");
        let entries = scopes.live_entries();
        assert!(entries
            .iter()
            .any(|(id, _, holders)| id == "active" && *holders == 1));
        assert!(entries.iter().any(|(id, _, _)| id == "newcomer"));
        assert!(!entries.iter().any(|(id, _, _)| id == "idle"));
        assert!(active_ctx.get_json("tool-registry").is_some());
        assert!(idle_ctx.get_json("tool-registry").is_none());

        drop(newcomer);
        drop(active);
    }

    #[test]
    fn dropping_holder_allows_next_admission_to_evict_it() {
        let (_projects, scopes) = scope_table(1);
        let dropped = scopes.acquire("dropped");
        let dropped_ctx = dropped.context().clone();
        let kept = scopes.acquire("kept");
        drop(dropped);

        let kept_again = scopes.acquire("kept");
        assert_eq!(
            scopes
                .live_entries()
                .iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["kept"]
        );
        assert!(dropped_ctx.get_json("tool-registry").is_none());
        drop(kept_again);
        drop(kept);
    }

    struct BlockingJson {
        entered: AtomicBool,
        release: AtomicBool,
    }

    impl JsonService for BlockingJson {
        fn call(
            &self,
            _method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, KernelError> {
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(serde_json::json!("done"))
        }
    }

    struct ConstantJson(&'static str);

    impl JsonService for ConstantJson {
        fn call(
            &self,
            _method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, KernelError> {
            Ok(serde_json::json!(self.0))
        }
    }

    #[test]
    fn eviction_closes_new_provider_admission_before_drain() {
        let (_projects, scopes) = scope_table(1);
        let lease = scopes.acquire("old");
        let blocker = Arc::new(BlockingJson {
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
        });
        lease
            .context()
            .provide_json("blocking", blocker.clone())
            .unwrap();
        lease
            .context()
            .provide_json("probe", Arc::new(ConstantJson("old")))
            .unwrap();
        let blocking_ref = lease.context().get_json("blocking").unwrap();
        let probe_ref = lease.context().get_json("probe").unwrap();

        let caller = std::thread::spawn(move || blocking_ref.call("run", serde_json::Value::Null));
        while !blocker.entered.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        drop(lease);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let evict_scopes = scopes.clone();
        let evictor = std::thread::spawn(move || {
            let fresh = evict_scopes.acquire("fresh");
            done_tx.send(()).unwrap();
            fresh
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match probe_ref.call("read", serde_json::Value::Null) {
                Err(KernelError::ServiceClosed { .. }) => break,
                Ok(_) if std::time::Instant::now() < deadline => std::thread::yield_now(),
                other => panic!("provider did not enter Closing: {other:?}"),
            }
        }
        assert!(
            done_rx.try_recv().is_err(),
            "eviction must wait for the already-running provider call"
        );
        assert!(matches!(
            probe_ref.call("read", serde_json::Value::Null),
            Err(KernelError::ServiceClosed { .. })
        ));

        blocker.release.store(true, Ordering::Release);
        assert_eq!(caller.join().unwrap().unwrap(), serde_json::json!("done"));
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(evictor.join().unwrap());
    }

    #[test]
    fn separate_tables_on_same_kernel_never_reuse_generation_scope() {
        let kernel = Kernel::new();
        let engine = engine_with_builtin_tools();
        let old_projects = tempfile::tempdir().expect("old projects root");
        let old_scopes = SessionKernelScopes::with_limit(
            kernel.clone(),
            engine.clone(),
            old_projects.path().to_path_buf(),
            1,
        );
        let old = old_scopes.acquire("same");
        let old_generation = old.generation();
        let old_ctx = old.context().clone();
        let old_label = old_ctx.label().to_string();
        old_ctx.wrap_json("permission/ask", |mut value, next| {
            value["oldTable"] = serde_json::json!(true);
            next.call(value)
        });

        // Rebuild the host table while the prior table's exact generation is
        // still leased. Both tables share this kernel's event bus.
        drop(old_scopes);
        let fresh_projects = tempfile::tempdir().expect("fresh projects root");
        let fresh_scopes =
            SessionKernelScopes::with_limit(kernel, engine, fresh_projects.path().to_path_buf(), 1);
        let fresh = fresh_scopes.acquire("same");
        let fresh_ctx = fresh.context().clone();
        fresh_ctx.wrap_json("permission/ask", |mut value, next| {
            value["freshTable"] = serde_json::json!(true);
            next.call(value)
        });

        assert_ne!(old.generation(), fresh.generation());
        assert_eq!(old.generation(), old_generation);
        assert_ne!(old_label, fresh_ctx.label());

        let old_out =
            old_ctx.waterfall_json_scoped("permission/ask", serde_json::json!({}), |value| value);
        assert_eq!(old_out["oldTable"], true);
        assert!(old_out.get("freshTable").is_none());

        let fresh_out =
            fresh_ctx.waterfall_json_scoped("permission/ask", serde_json::json!({}), |value| value);
        assert_eq!(fresh_out["freshTable"], true);
        assert!(fresh_out.get("oldTable").is_none());
    }

    #[test]
    fn same_session_label_rebinds_as_new_generation_without_aba() {
        let (_projects, scopes) = scope_table(1);
        let old = scopes.acquire("same");
        let old_generation = old.generation();
        let old_ctx = old.context().clone();
        old_ctx
            .provide_json("aba", Arc::new(ConstantJson("old")))
            .unwrap();
        let stale_service = old_ctx.get_json("aba").unwrap();
        old_ctx.wrap_json(
            "permission/ask",
            |_value, _next| serde_json::json!({ "answer": { "optionId": "old" } }),
        );
        drop(old);

        let other = scopes.acquire("other");
        drop(other);
        let fresh = scopes.acquire("same");
        assert_ne!(fresh.generation(), old_generation);
        fresh
            .context()
            .provide_json("aba", Arc::new(ConstantJson("new")))
            .unwrap();
        fresh.context().wrap_json(
            "permission/ask",
            |_value, _next| serde_json::json!({ "answer": { "optionId": "new" } }),
        );

        assert!(matches!(
            stale_service.call("read", serde_json::Value::Null),
            Err(KernelError::ServiceClosed { .. })
        ));
        assert_eq!(
            fresh
                .context()
                .call_json("aba", "read", serde_json::Value::Null)
                .unwrap(),
            serde_json::json!("new")
        );
        let stale_waterfall = old_ctx.waterfall_json_scoped(
            "permission/ask",
            serde_json::json!({}),
            |_| serde_json::json!({ "pass": true }),
        );
        assert_eq!(stale_waterfall, serde_json::json!({ "pass": true }));
        let fresh_waterfall = fresh.context().waterfall_json_scoped(
            "permission/ask",
            serde_json::json!({}),
            |_| serde_json::json!({ "pass": true }),
        );
        assert_eq!(fresh_waterfall["answer"]["optionId"], "new");
    }

    #[tokio::test]
    async fn detached_broker_snapshot_survives_session_move_and_table_disposal() {
        use rebon_core::permission::ChannelPermissionBroker;
        use rebon_tool::{EchoTool, PermissionBroker, ToolContext};
        use rebon_tools_core::{PermissionDecision, PermissionRequest};

        let (_projects, scopes) = scope_table(1);
        let setup = scopes.acquire("detached");
        let detached_ctx = setup.context().clone();
        detached_ctx.wrap_json(
            "permission/ask",
            |_value, _next| serde_json::json!({ "answer": { "optionId": "allow_once" } }),
        );
        drop(setup);

        let (broker, mut ui_rx) = ChannelPermissionBroker::new("foreground");
        broker.set_kernel_context_resolver(scopes.resolver());
        let detached = broker.for_session("detached");
        let foreground = broker.for_session("foreground");
        assert!(scopes
            .live_entries()
            .iter()
            .any(|(id, _, holders)| id == "detached" && *holders == 1));

        // Release the table owner as a TUI rebuild/disposal would. Both fixed
        // per-turn broker snapshots still own their entries.
        broker.set_kernel_context_resolver(Arc::new(|_| None));
        drop(scopes);
        let direct = detached_ctx.waterfall_json_scoped(
            "permission/ask",
            serde_json::json!({}),
            |_| serde_json::json!({ "pass": true }),
        );
        assert_eq!(direct["answer"]["optionId"], "allow_once");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            Some(serde_json::json!({ "value": 1 })),
        );
        let output = match tokio::time::timeout(
            Duration::from_secs(2),
            detached.resolve(
                &EchoTool,
                serde_json::json!({ "value": 1 }),
                &ToolContext::new().with_tool_use_id("detached-call"),
                decision,
            ),
        )
        .await
        {
            Ok(result) => {
                result.expect("detached snapshot still invokes its generation middleware")
            }
            Err(_) => {
                let reached_ui = ui_rx.try_recv().is_ok();
                panic!("detached broker timed out; reached_ui={reached_ui}");
            }
        };
        assert_eq!(output, serde_json::json!({ "value": 1 }));
        assert!(ui_rx.try_recv().is_err());

        drop(detached);
        assert!(detached_ctx.registration_labels().is_empty());
        drop(foreground);
    }
}
