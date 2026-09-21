//! The suspend-answer permission pipeline for the tool seat.
//!
//! When a composed plugin's tool call needs authorization that no config
//! grant covers, the seat broker suspends the call here instead of failing
//! closed: a pending ask is registered and announced on the kernel JSON
//! plane (`tool-asks/pending`, metadata + truncated input preview — never
//! the full payload), and the invoking future parks on a oneshot until a
//! front end answers through the `tool-asks` service. The suspension is
//! plain async, so the composition isolate keeps running everything else.
//!
//! Contract (cordis two-phase approval, bounded for the bg-ask lesson):
//! - **metadata first**: the announcement carries tool name, the
//!   tool-authored request (title/message/options), and a preview whose
//!   long strings are truncated;
//! - **first answer wins**: an ask resolves exactly once; answering an
//!   unknown or already-resolved id is a loud error, never a re-apply;
//! - **bounded**: every ask carries a deadline; an unanswered ask resolves
//!   to deny with `timeout` — a surface nobody renders can delay a call,
//!   never deadlock it;
//! - **drain on close**: tearing the surface down denies everything
//!   pending; a dropped service does the same via closed channels.
//!
//! The service is deliberately a **global surface** on the kernel plane —
//! any front end (app, TUI, tests) lists and answers through the same two
//! methods, so an approval is never reachable only inside a view nobody
//! has open (the ui-cordis lesson).
//!
//! Global to the **host**, not to the composition: the JSON plane cannot
//! tell one caller from another, so a plugin able to call `answer` would
//! approve the very call of its own that is parked here and walk through
//! `kernelPlugins.toolGrants`. The JS bridge therefore refuses this
//! service outright; answers
//! arrive on the Rust plane, which no isolate can reach.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use rebon_core::permission::{
    ChannelPermissionBroker, OutboundPermissionQuery, PermissionAnswer, PermissionOptionKind,
    PermissionQueryOption,
};
use rebon_kernel::{Context, JsonService, KernelError};
use tokio::sync::oneshot;

/// JSON-plane name of the ask surface.
pub const TOOL_ASKS_SERVICE: &str = "tool-asks";
/// Emitted when an ask is created: `{id, tool, request, preview}`.
pub const TOOL_ASK_PENDING_EVENT: &str = "tool-asks/pending";
/// Emitted when an ask resolves: `{id, tool, verdict, reason?}`.
pub const TOOL_ASK_RESOLVED_EVENT: &str = "tool-asks/resolved";

/// Default deadline for an unanswered ask.
pub const DEFAULT_ASK_TIMEOUT: Duration = Duration::from_secs(120);

/// How one suspended ask ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOutcome {
    Allow,
    Deny,
    /// Nobody answered within the deadline (resolved as deny).
    Timeout,
}

struct PendingAsk {
    tool: String,
    request: serde_json::Value,
    preview: serde_json::Value,
    responder: oneshot::Sender<AskOutcome>,
}

/// The global suspend-answer surface.
pub struct ToolAskService {
    /// Event channel to observers; label path scopes diagnostics.
    ctx: Context,
    pending: Mutex<HashMap<u64, PendingAsk>>,
    next_id: AtomicU64,
    timeout: Duration,
}

/// Truncate long strings so announcements carry a preview, not payloads.
fn preview_of(value: &serde_json::Value) -> serde_json::Value {
    const MAX_STR: usize = 512;
    match value {
        serde_json::Value::String(s) if s.chars().count() > MAX_STR => {
            let head: String = s.chars().take(MAX_STR).collect();
            serde_json::Value::String(format!(
                "{head}… [truncated, {} chars total]",
                s.chars().count()
            ))
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(preview_of).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), preview_of(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

impl ToolAskService {
    pub fn new(ctx: Context, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            ctx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            timeout,
        })
    }

    /// Suspend one authorization question until a front end answers, the
    /// deadline passes, or the surface drains. Never hangs forever.
    pub async fn ask(
        &self,
        tool: &str,
        request: serde_json::Value,
        input: &serde_json::Value,
    ) -> AskOutcome {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let preview = preview_of(input);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id,
            PendingAsk {
                tool: tool.to_string(),
                request: request.clone(),
                preview: preview.clone(),
                responder: tx,
            },
        );
        self.ctx.emit_json(
            TOOL_ASK_PENDING_EVENT,
            &serde_json::json!({
                "id": id,
                "tool": tool,
                "request": request,
                "preview": preview,
            }),
        );

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            // Sender dropped without an answer: surface closed → deny.
            Ok(Err(_)) => AskOutcome::Deny,
            Err(_elapsed) => {
                // Deadline: resolve ourselves (the answer path may race us;
                // whoever removes the entry first wins).
                if self.pending.lock().unwrap().remove(&id).is_some() {
                    self.emit_resolved(id, tool, "deny", Some("timeout"));
                }
                AskOutcome::Timeout
            }
        }
    }

    /// Answer one pending ask. First answer wins; an unknown or resolved
    /// id is refused loudly.
    pub fn answer(&self, id: u64, allow: bool) -> Result<(), KernelError> {
        let Some(entry) = self.pending.lock().unwrap().remove(&id) else {
            return Err(KernelError::Other(format!(
                "tool ask {id} is unknown or already resolved (first answer wins)"
            )));
        };
        let outcome = if allow {
            AskOutcome::Allow
        } else {
            AskOutcome::Deny
        };
        // The waiter may have timed out concurrently; a failed send means
        // the outcome no longer matters, but the resolution event still
        // reflects what the surface decided.
        let _ = entry.responder.send(outcome);
        self.emit_resolved(id, &entry.tool, if allow { "allow" } else { "deny" }, None);
        Ok(())
    }

    /// Pending asks, oldest first: `[{id, tool, request, preview}]`.
    pub fn list(&self) -> serde_json::Value {
        let pending = self.pending.lock().unwrap();
        let mut items: Vec<(u64, serde_json::Value)> = pending
            .iter()
            .map(|(id, ask)| {
                (
                    *id,
                    serde_json::json!({
                        "id": id,
                        "tool": ask.tool,
                        "request": ask.request,
                        "preview": ask.preview,
                    }),
                )
            })
            .collect();
        items.sort_by_key(|(id, _)| *id);
        serde_json::Value::Array(items.into_iter().map(|(_, v)| v).collect())
    }

    /// Deny everything pending (surface teardown, composition shutdown).
    pub fn drain(&self, reason: &str) -> usize {
        let drained: Vec<(u64, PendingAsk)> = self.pending.lock().unwrap().drain().collect();
        let count = drained.len();
        for (id, ask) in drained {
            let _ = ask.responder.send(AskOutcome::Deny);
            self.emit_resolved(id, &ask.tool, "deny", Some(reason));
        }
        count
    }

    fn emit_resolved(&self, id: u64, tool: &str, verdict: &str, reason: Option<&str>) {
        let mut payload = serde_json::json!({ "id": id, "tool": tool, "verdict": verdict });
        if let Some(reason) = reason {
            payload["reason"] = serde_json::json!(reason);
        }
        self.ctx.emit_json(TOOL_ASK_RESOLVED_EVENT, &payload);
    }
}

impl Drop for ToolAskService {
    fn drop(&mut self) {
        // Closing the surface must not strand suspended calls.
        self.drain("ask surface closed");
    }
}

impl JsonService for ToolAskService {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        match method {
            "list" => Ok(self.list()),
            "answer" => {
                let id = params
                    .get("id")
                    .and_then(|i| i.as_u64())
                    .ok_or_else(|| KernelError::Other("answer requires a numeric `id`".into()))?;
                let allow = match params.get("verdict").and_then(|v| v.as_str()) {
                    Some("allow") => true,
                    Some("deny") => false,
                    other => {
                        return Err(KernelError::Other(format!(
                            "answer verdict must be \"allow\" or \"deny\", got {other:?}"
                        )))
                    }
                };
                self.answer(id, allow)?;
                Ok(serde_json::json!({ "id": id, "resolved": true }))
            }
            "drain" => {
                let reason = params
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("drained");
                Ok(serde_json::json!({ "denied": self.drain(reason) }))
            }
            other => Err(KernelError::Other(format!(
                "tool-asks has no method `{other}`"
            ))),
        }
    }
}

// ---------------------------------------------------------------------
// The process slot: where a front end finds the surface to answer on
// ---------------------------------------------------------------------

/// Process slot for the running plane's ask surface — the same F1 discipline
/// as the web seat: set at plane boot, cleared by an identity-guarded effect
/// on the plane's fork, so every teardown path collects it.
///
/// A slot rather than a kernel lookup because the plane publishes `tool-asks`
/// on a *scoped* fork, whose service layer is invisible from the root: a front
/// end asking the process kernel for it would never find it. The announcement
/// events are global (`emit_json` with no scope), so a front end can hear an
/// ask from anywhere; this is how it answers one.
fn ask_slot() -> &'static RwLock<Option<Arc<ToolAskService>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<ToolAskService>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

pub fn set_process_tool_asks(asks: Arc<ToolAskService>) {
    *ask_slot().write().expect("ask slot poisoned") = Some(asks);
}

/// Identity-guarded: a plane being torn down must not clear its successor's
/// surface (a stop and a boot overlap, see `take_running_plane`).
pub fn clear_process_tool_asks(asks: &Arc<ToolAskService>) {
    let mut guard = ask_slot().write().expect("ask slot poisoned");
    if guard
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, asks))
    {
        *guard = None;
    }
}

/// The ask surface of the plane this process is running, if any.
pub fn process_tool_asks() -> Option<Arc<ToolAskService>> {
    ask_slot().read().expect("ask slot poisoned").clone()
}

/// Answer one ask on this process's surface. `Err` when no plane is running
/// or the ask already resolved (first answer wins).
pub fn answer_process_ask(id: u64, allow: bool) -> Result<(), KernelError> {
    answer_on(None, id, allow)
}

/// Answer on `bound` when a watcher was handed one surface outright, and on
/// the process slot otherwise.
///
/// The fallback is not a convenience: a front end installs before the plane
/// boots (it must, or it would miss the first ask), so at install time there
/// is nothing to bind to and the surface has to be looked up when the answer
/// arrives.
fn answer_on(bound: Option<&Arc<ToolAskService>>, id: u64, allow: bool) -> Result<(), KernelError> {
    match bound.cloned().or_else(process_tool_asks) {
        Some(asks) => asks.answer(id, allow),
        None => Err(KernelError::Other(format!(
            "tool ask {id} cannot be answered: no plugin plane is running"
        ))),
    }
}

// ---------------------------------------------------------------------
// The front end half: one question, and the permission query it becomes
// ---------------------------------------------------------------------

/// One suspended question, as a front end receives it off the event plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskPrompt {
    pub id: u64,
    pub tool: String,
    /// The tool-authored request (`title` / `message` / `options`), or null.
    pub request: serde_json::Value,
    /// The truncated input preview — never the full payload.
    pub preview: serde_json::Value,
}

/// Marker a permission query carries so a front end can tell a plugin's
/// question from a session's own. Lives under `metadata.rebon` because that
/// is where every other rebon-authored permission annotation lives.
pub const PLUGIN_ASK_METADATA_KEY: &str = "kernelPluginAsk";

impl AskPrompt {
    /// Read one off a [`TOOL_ASK_PENDING_EVENT`] payload. `None` when the
    /// payload is not one (a foreign emitter on the same name).
    pub fn from_event(event: &serde_json::Value) -> Option<Self> {
        Some(Self {
            id: event.get("id")?.as_u64()?,
            tool: event.get("tool")?.as_str()?.to_owned(),
            request: event
                .get("request")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            preview: event
                .get("preview")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
    }

    /// The one-line heading a front end shows. Says a *plugin* is asking,
    /// because the tool name alone reads like the session's own call.
    pub fn title(&self) -> String {
        format!("A kernel plugin is requesting {}", self.tool)
    }

    /// The body, which repeats the heading on purpose.
    ///
    /// Front ends that title a permission prompt from the tool name alone
    /// (the TUI's generic modal says "Allow Write?") would otherwise show a
    /// plugin's request as if the session had made it. Leading the body with
    /// who is asking is what keeps the two apart on every surface, including
    /// the ones that never read [`AskPrompt::title`].
    pub fn message(&self) -> String {
        let authored = self
            .request
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| compact_preview(&self.preview));
        format!("{}: {authored}", self.title())
    }

    /// The metadata marker this ask's permission query carries.
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({ "rebon": { PLUGIN_ASK_METADATA_KEY: { "id": self.id, "tool": self.tool } } })
    }
}

/// `{"file_path":"x.txt"}` → `file_path: "x.txt"`, bounded.
fn compact_preview(preview: &serde_json::Value) -> String {
    const MAX: usize = 200;
    let mut rendered = match preview.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(", "),
        None => preview.to_string(),
    };
    if rendered.chars().count() > MAX {
        rendered = rendered.chars().take(MAX).collect::<String>() + "…";
    }
    rendered
}

/// The plugin-ask marker on a permission query's metadata: `Some(ask id)`
/// when this query came from the plane's ask surface.
///
/// A front end uses it to keep persisted-rule affordances off a plugin's
/// question: the surface has two verdicts and no memory, so an "allow always"
/// there would promise a rule nobody stores.
pub fn plugin_ask_id(metadata: Option<&serde_json::Value>) -> Option<u64> {
    metadata?
        .get("rebon")?
        .get(PLUGIN_ASK_METADATA_KEY)?
        .get("id")?
        .as_u64()
}

/// Query ids for plugin asks live above every id a broker hands out, so a
/// front end that keys anything by query id cannot confuse the two. The
/// broker counts up from 1 and a session that reached 2^48 prompts has
/// other problems.
const PLUGIN_ASK_QUERY_ID_BASE: u64 = 1 << 48;

/// Turn one ask into the permission query the front ends already render,
/// paired with the receiver its answer arrives on.
///
/// Two options and no third: the surface knows `allow` and `deny`, and
/// nothing persists. An `AllowAlways` here would be a rule the ask surface
/// has no way to remember.
pub fn permission_query_for(
    prompt: &AskPrompt,
    session_id: &str,
) -> (OutboundPermissionQuery, oneshot::Receiver<PermissionAnswer>) {
    let (response_tx, response_rx) = oneshot::channel();
    let query = OutboundPermissionQuery {
        id: PLUGIN_ASK_QUERY_ID_BASE + prompt.id,
        tool_name: prompt.tool.clone(),
        tool_call_id: format!("kernel-plugin-ask-{}", prompt.id),
        session_id: session_id.to_owned(),
        title: prompt.title(),
        message: prompt.message(),
        // Deliberately none: the preview is already in `message`, and a
        // front end that renders `tool_input` would render a truncated
        // payload as if it were the call's real arguments.
        tool_input: None,
        metadata: Some(prompt.metadata()),
        options: vec![
            PermissionQueryOption {
                option_id: "allow_once".to_owned(),
                label: "Yes, allow this call".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionQueryOption {
                option_id: "reject_once".to_owned(),
                label: "No, deny it".to_owned(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
        response_tx,
    };
    (query, response_rx)
}

/// Whether one answer means "let the call run".
///
/// Anything that is not an explicit allow is a deny: a cancelled dialog, a
/// rejected option, an answer that names an option nobody offered. The
/// surface is fail-closed and this is the last place that decides.
pub fn answer_allows(answer: &PermissionAnswer) -> bool {
    matches!(
        answer,
        PermissionAnswer::Selected { option_id, .. }
            if option_id == "allow_once" || option_id == "allow"
    )
}

// ---------------------------------------------------------------------
// The watcher: one front end, one subscription
// ---------------------------------------------------------------------

/// A front end's subscription to the ask surface.
///
/// Dropping it stops the subscription and **denies** every ask this watcher
/// had already put in front of a user — closing the surface must not leave a
/// plugin parked until the deadline. Asks it never saw are untouched: they
/// belong to whoever else is watching, or to the deadline.
pub struct AskWatch {
    ctx: Context,
    inflight: Arc<Mutex<HashSet<u64>>>,
    bound: Option<Arc<ToolAskService>>,
}

impl Drop for AskWatch {
    fn drop(&mut self) {
        self.ctx.dispose();
        let inflight: Vec<u64> = self
            .inflight
            .lock()
            .expect("ask watch inflight poisoned")
            .drain()
            .collect();
        for id in inflight {
            let _ = answer_on(self.bound.as_ref(), id, false);
        }
    }
}

/// Watch this process's ask surface and put every question in front of a
/// user through `broker` — the same channel the session's own permission
/// prompts travel, so the ask lands in whatever front end is draining it:
/// the TUI's modal, or a background worker's IPC stream and from there the
/// desktop's permission pane.
///
/// `session_id` only labels the query; the ask belongs to the process, not
/// to a turn. `kernel` must be the root the plane publishes its surface on —
/// the process kernel, in production — because the announcement is a global
/// broadcast and the plane may not have booted yet: a watcher installed first
/// still hears the first ask. It is passed in rather than looked up so that
/// the one caller who knows which kernel that is says so, and so a test can
/// pair a surface with a watcher on a bus of its own.
///
/// `asks` binds one surface outright; `None` looks the process one up when
/// an answer arrives, which is what production needs (see [`answer_on`]).
pub fn watch_asks_through_broker(
    kernel: &Context,
    runtime: tokio::runtime::Handle,
    asks: Option<Arc<ToolAskService>>,
    broker: Arc<ChannelPermissionBroker>,
    session_id: String,
) -> AskWatch {
    let ctx = kernel.fork("tool-ask-front-end");
    let inflight = Arc::new(Mutex::new(HashSet::new()));
    {
        let inflight = inflight.clone();
        let bound = asks.clone();
        ctx.on_json(TOOL_ASK_PENDING_EVENT, move |payload| {
            let Some(prompt) = AskPrompt::from_event(payload) else {
                return;
            };
            let (query, response_rx) = permission_query_for(&prompt, &session_id);
            let id = prompt.id;
            inflight
                .lock()
                .expect("ask watch inflight poisoned")
                .insert(id);
            broker.forward_direct(query);
            let inflight = inflight.clone();
            let bound = bound.clone();
            runtime.spawn(async move {
                // A dropped sender is a front end that went away without
                // answering: deny, exactly as a cancelled dialog does.
                let allow = match response_rx.await {
                    Ok(answer) => answer_allows(&answer),
                    Err(_) => false,
                };
                if inflight
                    .lock()
                    .expect("ask watch inflight poisoned")
                    .remove(&id)
                {
                    // Only if we still own it: a watcher torn down between
                    // the prompt and the answer already denied this one.
                    let _ = answer_on(bound.as_ref(), id, allow);
                }
            });
        });
    }
    AskWatch {
        ctx,
        inflight,
        bound: asks,
    }
}

/// The one watcher this process has installed, if any.
fn front_end_slot() -> &'static Mutex<Option<AskWatch>> {
    static SLOT: OnceLock<Mutex<Option<AskWatch>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Make `broker` this process's ask front end, replacing whatever was there.
///
/// One front end at a time, on purpose: a process runs one foreground
/// session, and two watchers would put the same question in front of the
/// user twice and race to answer it (the loser gets a "first answer wins"
/// error). Replacing denies the previous watcher's outstanding asks, which
/// is what the session going away means.
///
/// A no-op off a runtime: the watcher answers from a spawned task, and a
/// caller with no runtime to spawn on has no front end to offer either.
pub fn install_process_ask_front_end(
    kernel: &Context,
    broker: &Arc<ChannelPermissionBroker>,
    session_id: &str,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let watch =
        watch_asks_through_broker(kernel, runtime, None, broker.clone(), session_id.to_owned());
    *front_end_slot().lock().expect("ask front end poisoned") = Some(watch);
}

/// Take the front end down (process shutdown, a surface closing for good).
pub fn clear_process_ask_front_end() {
    *front_end_slot().lock().expect("ask front end poisoned") = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    fn service_with_timeout(timeout: Duration) -> (Arc<Kernel>, Arc<ToolAskService>) {
        let kernel = Kernel::new();
        let svc = ToolAskService::new(kernel.context().fork("tool-asks"), timeout);
        (kernel, svc)
    }

    /// The one kernel these tests share, standing in for the process kernel.
    ///
    /// The surface and the watcher have to be on the *same* bus: a service
    /// forked off one kernel emits where a watcher on another is not
    /// listening, exactly as it would in production if the plane booted on a
    /// kernel of its own. Production passes the process kernel; this is one
    /// bus with nothing else on it, which is the same property without a boot.
    fn shared_bus() -> &'static Arc<Kernel> {
        static BUS: OnceLock<Arc<Kernel>> = OnceLock::new();
        BUS.get_or_init(Kernel::new)
    }

    /// A surface on [`shared_bus`], plus the guard that serialises these
    /// tests: they share one event bus, so two at once would hand each
    /// other's asks to the wrong broker.
    fn surface_on_the_process_bus(
        timeout: Duration,
    ) -> (std::sync::MutexGuard<'static, ()>, Arc<ToolAskService>) {
        static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = SERIAL
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ctx = shared_bus().context().fork("tool-asks-test");
        (guard, ToolAskService::new(ctx, timeout))
    }

    #[tokio::test]
    async fn allow_and_deny_roundtrip_with_events() {
        let (kernel, svc) = service_with_timeout(Duration::from_secs(30));
        let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let events = events.clone();
            kernel.context().on_json(TOOL_ASK_RESOLVED_EVENT, move |p| {
                events.lock().unwrap().push(p.clone());
            });
        }

        let asker = svc.clone();
        let pending = tokio::spawn(async move {
            asker
                .ask(
                    "Write",
                    serde_json::json!({ "title": "Write file" }),
                    &serde_json::json!({ "file_path": "x.txt" }),
                )
                .await
        });

        // Wait until the ask is listed, then answer allow.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let id = loop {
            let list = svc.list();
            if let Some(first) = list.as_array().and_then(|a| a.first()) {
                break first["id"].as_u64().unwrap();
            }
            assert!(tokio::time::Instant::now() < deadline, "ask never listed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        svc.answer(id, true).unwrap();
        assert_eq!(pending.await.unwrap(), AskOutcome::Allow);
        assert!(
            svc.list().as_array().unwrap().is_empty(),
            "resolved ask leaves the list"
        );
        assert_eq!(events.lock().unwrap()[0]["verdict"], "allow");

        // Deny path through the JSON facade.
        let asker = svc.clone();
        let pending = tokio::spawn(async move {
            asker
                .ask("Bash", serde_json::json!({}), &serde_json::json!({}))
                .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let id = loop {
            if let Some(first) = svc.list().as_array().and_then(|a| a.first()) {
                break first["id"].as_u64().unwrap();
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        svc.call("answer", serde_json::json!({ "id": id, "verdict": "deny" }))
            .unwrap();
        assert_eq!(pending.await.unwrap(), AskOutcome::Deny);
    }

    #[tokio::test]
    async fn first_answer_wins_and_stale_answers_are_refused() {
        let (_kernel, svc) = service_with_timeout(Duration::from_secs(30));
        let asker = svc.clone();
        let pending = tokio::spawn(async move {
            asker
                .ask("Write", serde_json::json!({}), &serde_json::json!({}))
                .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let id = loop {
            if let Some(first) = svc.list().as_array().and_then(|a| a.first()) {
                break first["id"].as_u64().unwrap();
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        svc.answer(id, true).unwrap();
        let stale = svc.answer(id, false).unwrap_err();
        assert!(stale.to_string().contains("first answer wins"), "{stale}");
        assert_eq!(
            pending.await.unwrap(),
            AskOutcome::Allow,
            "the first answer stands"
        );

        let unknown = svc.answer(9999, true).unwrap_err();
        assert!(unknown.to_string().contains("unknown"), "{unknown}");
    }

    /// The bg-ask regression shape: a surface nobody renders must delay,
    /// never deadlock — the ask resolves to deny on its own.
    #[tokio::test]
    async fn unanswered_ask_times_out_to_deny_and_leaves_no_residue() {
        let (kernel, svc) = service_with_timeout(Duration::from_millis(50));
        let resolved: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let resolved = resolved.clone();
            kernel.context().on_json(TOOL_ASK_RESOLVED_EVENT, move |p| {
                resolved.lock().unwrap().push(p.clone());
            });
        }
        let outcome = svc
            .ask("Write", serde_json::json!({}), &serde_json::json!({}))
            .await;
        assert_eq!(outcome, AskOutcome::Timeout);
        assert!(
            svc.list().as_array().unwrap().is_empty(),
            "no pending residue"
        );
        let resolved = resolved.lock().unwrap();
        assert_eq!(resolved[0]["reason"], "timeout");
    }

    #[tokio::test]
    async fn drain_denies_everything_pending() {
        let (_kernel, svc) = service_with_timeout(Duration::from_secs(30));
        let (a, b) = (svc.clone(), svc.clone());
        let ask_a = tokio::spawn(async move {
            a.ask("Write", serde_json::json!({}), &serde_json::json!({}))
                .await
        });
        let ask_b = tokio::spawn(async move {
            b.ask("Bash", serde_json::json!({}), &serde_json::json!({}))
                .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while svc.list().as_array().unwrap().len() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "asks never listed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(svc.drain("shutdown"), 2);
        assert_eq!(ask_a.await.unwrap(), AskOutcome::Deny);
        assert_eq!(ask_b.await.unwrap(), AskOutcome::Deny);
        assert!(svc.list().as_array().unwrap().is_empty());
    }

    /// The A5 shape, end to end on this side of the seam: a plugin's
    /// ungranted call suspends, the front end's channel receives a permission
    /// query, the user picks allow, and the suspended call runs.
    #[tokio::test]
    async fn a_broker_front_end_answers_a_plugin_ask() {
        let (_serial, svc) = surface_on_the_process_bus(Duration::from_secs(30));
        let (broker, mut permission_rx) = ChannelPermissionBroker::new("sess-ask-allow");
        let _watch = watch_asks_through_broker(
            shared_bus().context(),
            tokio::runtime::Handle::current(),
            Some(svc.clone()),
            Arc::new(broker),
            "sess-ask-allow".to_owned(),
        );

        let asker = svc.clone();
        let pending = tokio::spawn(async move {
            asker
                .ask(
                    "Write",
                    serde_json::json!({ "message": "overwrite the changelog" }),
                    &serde_json::json!({ "file_path": "CHANGELOG.md" }),
                )
                .await
        });

        let query = tokio::time::timeout(Duration::from_secs(5), permission_rx.recv())
            .await
            .expect("a front end receives the ask")
            .expect("the channel stays open");
        assert_eq!(query.tool_name, "Write");
        assert!(
            query.message.contains("kernel plugin"),
            "the body says who is asking: {}",
            query.message
        );
        assert!(
            query.message.contains("overwrite the changelog"),
            "the tool's own words survive: {}",
            query.message
        );
        assert_eq!(
            plugin_ask_id(query.metadata.as_ref()),
            Some(1),
            "the query is marked as a plugin ask"
        );
        assert!(
            !query
                .options
                .iter()
                .any(|option| option.kind == PermissionOptionKind::AllowAlways
                    || option.kind == PermissionOptionKind::RejectAlways),
            "the surface has no memory, so it offers no persisted rule"
        );
        assert!(
            query.id > PLUGIN_ASK_QUERY_ID_BASE,
            "plugin ask query ids cannot collide with a broker's"
        );

        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".to_owned(),
                updated_input: None,
                extra_text: None,
            })
            .expect("the watcher is still waiting");
        assert_eq!(pending.await.unwrap(), AskOutcome::Allow);
    }

    /// Dismissing the dialog is a deny, not a wait for the deadline.
    #[tokio::test]
    async fn a_cancelled_dialog_denies_the_plugin_ask() {
        let (_serial, svc) = surface_on_the_process_bus(Duration::from_secs(30));
        let (broker, mut permission_rx) = ChannelPermissionBroker::new("sess-ask-cancel");
        let _watch = watch_asks_through_broker(
            shared_bus().context(),
            tokio::runtime::Handle::current(),
            Some(svc.clone()),
            Arc::new(broker),
            "sess-ask-cancel".to_owned(),
        );

        let asker = svc.clone();
        let pending = tokio::spawn(async move {
            asker
                .ask("Bash", serde_json::json!({}), &serde_json::json!({}))
                .await
        });
        let query = tokio::time::timeout(Duration::from_secs(5), permission_rx.recv())
            .await
            .expect("a front end receives the ask")
            .expect("the channel stays open");
        query
            .response_tx
            .send(PermissionAnswer::Cancelled)
            .expect("the watcher is still waiting");
        assert_eq!(pending.await.unwrap(), AskOutcome::Deny);
    }

    /// The front end going away denies what it was holding — it must not
    /// leave the plugin parked until the deadline.
    #[tokio::test]
    async fn dropping_the_watch_denies_what_it_was_showing() {
        let (_serial, svc) = surface_on_the_process_bus(Duration::from_secs(30));
        let (broker, mut permission_rx) = ChannelPermissionBroker::new("sess-ask-drop");
        let watch = watch_asks_through_broker(
            shared_bus().context(),
            tokio::runtime::Handle::current(),
            Some(svc.clone()),
            Arc::new(broker),
            "sess-ask-drop".to_owned(),
        );

        let asker = svc.clone();
        let pending = tokio::spawn(async move {
            asker
                .ask("Write", serde_json::json!({}), &serde_json::json!({}))
                .await
        });
        let query = tokio::time::timeout(Duration::from_secs(5), permission_rx.recv())
            .await
            .expect("a front end receives the ask")
            .expect("the channel stays open");
        // The user never answers; the surface closes under them.
        drop(watch);
        assert_eq!(pending.await.unwrap(), AskOutcome::Deny);
        assert!(svc.list().as_array().unwrap().is_empty());
        drop(query);
    }

    #[test]
    fn the_process_slot_clear_is_identity_guarded() {
        let kernel = Kernel::new();
        let a = ToolAskService::new(kernel.context().fork("a"), Duration::from_secs(1));
        let b = ToolAskService::new(kernel.context().fork("b"), Duration::from_secs(1));
        set_process_tool_asks(a.clone());
        set_process_tool_asks(b.clone());
        clear_process_tool_asks(&a);
        assert!(
            process_tool_asks().is_some_and(|current| Arc::ptr_eq(&current, &b)),
            "a stale clear must not take the successor's surface"
        );
        clear_process_tool_asks(&b);
        assert!(process_tool_asks().is_none());
        assert!(
            answer_process_ask(1, true).is_err(),
            "with no plane there is nothing to answer"
        );
    }

    #[test]
    fn only_an_explicit_allow_allows() {
        assert!(answer_allows(&PermissionAnswer::Selected {
            option_id: "allow_once".into(),
            updated_input: None,
            extra_text: None,
        }));
        assert!(!answer_allows(&PermissionAnswer::Cancelled));
        assert!(!answer_allows(&PermissionAnswer::Selected {
            option_id: "reject_once".into(),
            updated_input: None,
            extra_text: None,
        }));
        assert!(
            !answer_allows(&PermissionAnswer::Selected {
                option_id: "allow_always".into(),
                updated_input: None,
                extra_text: None,
            }),
            "an option the surface never offered is not an allow"
        );
    }

    #[test]
    fn a_prompt_reads_off_the_announcement() {
        let event = serde_json::json!({
            "id": 7, "tool": "Write",
            "request": {},
            "preview": { "file_path": "x.txt" },
        });
        let prompt = AskPrompt::from_event(&event).expect("a well-formed announcement");
        assert_eq!(prompt.id, 7);
        assert!(prompt.message().contains("file_path"));
        assert!(AskPrompt::from_event(&serde_json::json!({ "tool": "Write" })).is_none());
    }

    #[test]
    fn previews_truncate_long_strings_only() {
        let long = "x".repeat(2000);
        let input = serde_json::json!({
            "file_path": "short.txt",
            "content": long,
            "nested": { "list": ["ok", "y".repeat(600)] },
        });
        let preview = preview_of(&input);
        assert_eq!(preview["file_path"], "short.txt");
        let content = preview["content"].as_str().unwrap();
        assert!(content.len() < 700, "long strings must be truncated");
        assert!(content.contains("[truncated, 2000 chars total]"));
        assert!(preview["nested"]["list"][1]
            .as_str()
            .unwrap()
            .contains("truncated"));
    }
}
