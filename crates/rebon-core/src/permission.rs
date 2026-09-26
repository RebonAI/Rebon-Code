//! Typed permission channel for the TUI path.
//!
//! Replaces the JSON-RPC round-trip through [`rebon_agent_core::ChannelPermissionRequestPublisher`]
//! with direct Rust types. The ACP external path still uses `AcpPermissionBroker`; the TUI
//! path uses `ChannelPermissionBroker` which works with [`OutboundPermissionQuery`] /
//! [`PermissionAnswer`] — no serialization.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use rebon_permissions::{
    auto_mode_denials::AutoModeDenialInput, denial_sink::AutoModeHooks, types::PermissionMode,
};
use rebon_tool::{PermissionBroker, Tool, ToolContext};
use rebon_tools_core::{
    PermissionBehavior, PermissionDecision, PermissionRequest, ToolError, ToolKind, ToolResult,
};
use rebon_types::AutoModeAllowSource;

use crate::auto_mode_classifier::{
    AutoModeClassifier, AutoModeClassifierError, AutoModeClassifierFailureKind,
    AutoModeClassifierOutcome, AutoModeClassifierRequest,
};
use crate::deferred_question::{self, DeferredQuestionSink};
use crate::permission_seat::{DecisionScope, PermissionRules, RejectionNote};

/// How long one auto-mode classification may take: both stages, and the
/// fallback model behind them if the first one fails.
///
/// It was 30s, which is a budget for a classifier that answers a question
/// rather than thinks about it. A provider that reasons first does not fit:
/// DeepSeek spends about 7s on the fast stage and 28s on the thinking one, so
/// every call that reached the second stage was cut off here and refused —
/// and a refusal for running out of time is indistinguishable, to whoever is
/// reading it, from the classifier having found something. The bound is now
/// long enough that a slow model, a long transcript and a fallback attempt
/// all finish inside it, because the cheap mistake is waiting and the
/// expensive one is throwing away a verdict that was on its way.
const AUTO_MODE_CLASSIFIER_TIMEOUT: Duration = Duration::from_secs(300);

// ── Typed channel types ─────────────────────────────────────────

/// The option kind is the wire shape, re-exported so the typed channel
/// and the ACP path classify an option id into the same four values. It
/// used to be a second, identical enum here; the TUI already depends on
/// `rebon-proto` through the engine, so the copy bought nothing and let
/// the two paths drift.
pub use rebon_proto::types::PermissionOptionKind;

/// One option in a permission query.
#[derive(Debug, Clone)]
pub struct PermissionQueryOption {
    pub option_id: String,
    pub label: String,
    pub kind: PermissionOptionKind,
}

/// Typed permission query sent from the broker to the TUI.
#[derive(Debug)]
pub struct OutboundPermissionQuery {
    pub id: u64,
    pub tool_name: String,
    pub tool_call_id: String,
    pub session_id: String,
    pub title: String,
    pub message: String,
    pub tool_input: Option<Value>,
    pub metadata: Option<Value>,
    pub options: Vec<PermissionQueryOption>,
    pub response_tx: oneshot::Sender<PermissionAnswer>,
}

/// Typed response from the TUI to the broker.
#[derive(Debug)]
pub enum PermissionAnswer {
    /// User selected an option by id.
    Selected {
        option_id: String,
        updated_input: Option<Value>,
        extra_text: Option<String>,
    },
    /// User cancelled / dismissed the dialog.
    Cancelled,
}

impl PermissionAnswer {
    pub fn with_extra_text(self, extra_text: Option<String>) -> Self {
        match self {
            Self::Selected {
                option_id,
                updated_input,
                ..
            } => Self::Selected {
                option_id,
                updated_input,
                extra_text,
            },
            Self::Cancelled => Self::Cancelled,
        }
    }
}

/// A kernel context paired with the owner that keeps its scope generation
/// admitted. Clones share one keepalive, so a per-turn broker can retain the
/// exact generation without depending on the host that minted it.
#[derive(Clone)]
pub struct KernelContextLease {
    inner: Arc<KernelContextLeaseInner>,
}

struct KernelContextLeaseInner {
    context: rebon_kernel::Context,
    _keepalive: Box<dyn Send + Sync>,
}

impl std::fmt::Debug for KernelContextLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelContextLease")
            .field("context", &self.inner.context)
            .finish_non_exhaustive()
    }
}

impl KernelContextLease {
    /// Pair a context with the generation lease or runtime holder that owns it.
    pub fn managed(context: rebon_kernel::Context, keepalive: impl Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(KernelContextLeaseInner {
                context,
                _keepalive: Box::new(keepalive),
            }),
        }
    }

    /// Compatibility for fixed-lifetime scopes that are already owned by the
    /// surrounding runtime rather than by a bounded session-scope table.
    pub fn unmanaged(context: rebon_kernel::Context) -> Self {
        Self::managed(context, ())
    }

    pub fn context(&self) -> &rebon_kernel::Context {
        &self.inner.context
    }
}

/// Resolve and acquire the exact scope generation for one prompt turn.
pub type KernelContextLeaseResolver = Arc<dyn Fn(&str) -> Option<KernelContextLease> + Send + Sync>;

/// Run the session-scoped `permission/ask` waterfall before an interactive
/// permission transport.
///
/// `Some(answer)` means a kernel listener answered (or cancelled) it;
/// `None` means the ask falls through to whatever interactive surface the
/// caller owns — including when a listener's output is malformed. The
/// fall-through direction is deliberate: fail open **to a human**, never
/// to a silent approval.
///
/// Shared by both brokers so the seam behaves the same whichever transport
/// a session's permissions ride on (`ChannelPermissionBroker` for the TUI
/// and headless sessions, `AcpPermissionBroker` over JSON-RPC). The managed
/// lease keeps the exact scope generation admitted for the whole turn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn kernel_ask_waterfall(
    lease: &KernelContextLease,
    session_id: &str,
    tool_name: &str,
    tool_call_id: Option<&str>,
    title: &str,
    message: &str,
    tool_input: Option<&Value>,
    options: &[(String, String)],
) -> Option<PermissionAnswer> {
    let payload = serde_json::json!({
        "sessionId": session_id,
        "toolName": tool_name,
        "toolCallId": tool_call_id,
        "title": title,
        "message": message,
        "toolInput": tool_input,
        "options": options
            .iter()
            .map(|(option_id, label)| {
                serde_json::json!({ "optionId": option_id, "label": label })
            })
            .collect::<Vec<_>>(),
    });
    let outcome = lease.context().waterfall_json_scoped(
        "permission/ask",
        payload,
        |query| serde_json::json!({ "pass": true, "query": query }),
    );
    if outcome
        .get("pass")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    if outcome
        .get("cancel")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some(PermissionAnswer::Cancelled);
    }
    let answer = outcome.get("answer")?;
    let Some(option_id) = answer.get("optionId").and_then(Value::as_str) else {
        tracing::warn!(
            tool = %tool_name,
            "permission/ask listener returned a malformed answer; falling through to UI"
        );
        return None;
    };
    Some(PermissionAnswer::Selected {
        option_id: option_id.to_string(),
        updated_input: answer
            .get("updatedInput")
            .cloned()
            .filter(|value| !value.is_null()),
        extra_text: answer
            .get("extraText")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

// ── ChannelPermissionBroker ─────────────────────────────────────

/// Permission broker backed by a typed `mpsc` channel.
///
/// Created via [`ChannelPermissionBroker::new`] which returns both the
/// broker (for the engine) and the receiver (for the TUI).
///
/// When a `query_event_tx` is set (via [`set_query_event_tx`]), permission
/// queries are routed through the query-event channel instead of being
/// sent directly. This guarantees FIFO ordering with `ToolDispatchStart`
/// events — the executor processes them sequentially, so the TUI always
/// sees the tool-call event before the permission dialog.
#[derive(Clone)]
pub struct ChannelPermissionBroker {
    /// Direct sender — used as fallback when no query-event tx is set
    /// (e.g. sub-agent workers, tests).
    sender: Arc<std::sync::Mutex<mpsc::UnboundedSender<OutboundPermissionQuery>>>,
    session_id: Arc<std::sync::Mutex<String>>,
    next_id: Arc<AtomicU64>,
    /// When set, permission queries are sent through this channel as
    /// `QueryEvent::PermissionQuery` instead of through `sender`.
    query_event_tx: Arc<std::sync::Mutex<Option<crate::turn_hook::QueryEventSender>>>,
    /// Optional auto-mode state: live mode, denial sink, verdict cache, and the
    /// model classifier installed separately below. Kept optional so workers and
    /// tests that do not use auto mode can construct the broker unchanged.
    ///
    /// The mode provider inside is also the only channel through which the
    /// broker learns the session is in `PermissionMode::BypassPermissions`, so
    /// a caller that wants bypass honored must install these hooks. Without
    /// them every mode behaves like `Default`.
    auto_mode_hooks: Arc<std::sync::Mutex<Option<AutoModeHooks>>>,
    auto_mode_classifier: Arc<std::sync::Mutex<Option<Arc<dyn AutoModeClassifier>>>>,
    /// Fixed scope snapshot used by an already-created per-turn broker.
    kernel_ctx: Arc<std::sync::Mutex<Option<KernelContextLease>>>,
    /// Runtime resolver used only by the long-lived broker. `for_session`
    /// acquires once and stores the resulting fixed snapshot above.
    kernel_ctx_resolver: Arc<std::sync::Mutex<Option<KernelContextLeaseResolver>>>,
    /// Where deferred `AskUserQuestion` answers go. Only a surface that can
    /// take a user message at any time installs one (the local TUI); without
    /// it every question holds its turn as before. Shared by every per-turn
    /// view, so a sink installed mid-session reaches the next turn.
    /// See [`Self::question_deferral`].
    deferred_question_sink: Arc<std::sync::Mutex<Option<Arc<dyn DeferredQuestionSink>>>>,
}

pub type SharedChannelPermissionBroker = Arc<ChannelPermissionBroker>;

impl std::fmt::Debug for ChannelPermissionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelPermissionBroker")
            .field("session_id", &self.session_id.lock().expect("poisoned"))
            .field(
                "has_kernel_context",
                &self.kernel_ctx.lock().expect("poisoned").is_some(),
            )
            .field(
                "has_kernel_context_resolver",
                &self.kernel_ctx_resolver.lock().expect("poisoned").is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl ChannelPermissionBroker {
    /// Create a broker + receiver pair. The TUI drains the receiver
    /// to show permission dialogs.
    pub fn new(
        session_id: impl Into<String>,
    ) -> (Self, mpsc::UnboundedReceiver<OutboundPermissionQuery>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                sender: Arc::new(std::sync::Mutex::new(sender)),
                session_id: Arc::new(std::sync::Mutex::new(session_id.into())),
                next_id: Arc::new(AtomicU64::new(1)),
                query_event_tx: Arc::new(std::sync::Mutex::new(None)),
                auto_mode_hooks: Arc::new(std::sync::Mutex::new(None)),
                auto_mode_classifier: Arc::new(std::sync::Mutex::new(None)),
                kernel_ctx: Arc::new(std::sync::Mutex::new(None)),
                kernel_ctx_resolver: Arc::new(std::sync::Mutex::new(None)),
                deferred_question_sink: Arc::new(std::sync::Mutex::new(None)),
            },
            receiver,
        )
    }

    pub fn set_session_id(&self, session_id: impl Into<String>) {
        *self.session_id.lock().expect("poisoned") = session_id.into();
    }

    /// Attach a fixed kernel context: `Ask` decisions then run the
    /// `permission/ask` JSON waterfall before reaching the UI.
    ///
    /// This compatibility API is for scopes whose lifetime is owned by the
    /// surrounding runtime. Bounded session-scope tables should install a
    /// managed resolver with [`Self::set_kernel_context_resolver`] instead.
    /// Per-turn views snapshot the resulting handle, so detached work keeps
    /// asking on the exact session generation where it started.
    pub fn set_kernel_context(&self, ctx: rebon_kernel::Context) {
        self.set_kernel_context_lease(KernelContextLease::unmanaged(ctx));
    }

    /// Attach a fixed managed context, preserving its owner until the last
    /// per-turn snapshot drops.
    pub fn set_kernel_context_lease(&self, lease: KernelContextLease) {
        *self.kernel_ctx.lock().expect("poisoned") = Some(lease);
        *self.kernel_ctx_resolver.lock().expect("poisoned") = None;
    }

    /// Attach a runtime resolver. Each `for_session` call acquires exactly one
    /// generation and holds it for that broker's whole turn.
    pub fn set_kernel_context_resolver(&self, resolver: KernelContextLeaseResolver) {
        *self.kernel_ctx_resolver.lock().expect("poisoned") = Some(resolver);
        *self.kernel_ctx.lock().expect("poisoned") = None;
    }

    /// Run the `permission/ask` waterfall. `Some(answer)` means a kernel
    /// listener answered (or cancelled) the ask; `None` falls through to the
    /// interactive query. Malformed listener output falls through too —
    /// fail-open to the UI, never to silent approval.
    fn kernel_waterfall_answer(
        &self,
        tool_name: &str,
        tool_call_id: Option<&str>,
        title: &str,
        message: &str,
        tool_input: Option<&Value>,
        options: &[PermissionQueryOption],
    ) -> Option<PermissionAnswer> {
        let lease = self.kernel_ctx.lock().expect("poisoned").clone()?;
        let session_id = self.session_id.lock().expect("poisoned").clone();
        let options: Vec<(String, String)> = options
            .iter()
            .map(|option| (option.option_id.clone(), option.label.clone()))
            .collect();
        kernel_ask_waterfall(
            &lease,
            &session_id,
            tool_name,
            tool_call_id,
            title,
            message,
            tool_input,
            &options,
        )
    }

    /// Build a per-turn broker view with a fixed session id. It snapshots the
    /// current receiver sender and shares the id counter/auto-mode hooks, while
    /// keeping its query-event route independent so concurrent foreground and
    /// detached turns do not overwrite each other's permission FIFO channel.
    ///
    /// The kernel resolver is called exactly once here. Its managed generation
    /// lease is stored on this view for the complete prompt, including turns
    /// with no permission calls and detached turns that outlive a TUI session
    /// change. The long-lived broker never retargets an already-created view.
    pub fn for_session(&self, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let resolver = self.kernel_ctx_resolver.lock().expect("poisoned").clone();
        let kernel_ctx = resolver
            .and_then(|resolver| resolver(&session_id))
            .or_else(|| self.kernel_ctx.lock().expect("poisoned").clone());
        Self {
            sender: Arc::new(std::sync::Mutex::new(
                self.sender.lock().expect("poisoned").clone(),
            )),
            session_id: Arc::new(std::sync::Mutex::new(session_id)),
            next_id: self.next_id.clone(),
            query_event_tx: Arc::new(std::sync::Mutex::new(None)),
            auto_mode_hooks: self.auto_mode_hooks.clone(),
            auto_mode_classifier: self.auto_mode_classifier.clone(),
            kernel_ctx: Arc::new(std::sync::Mutex::new(kernel_ctx)),
            kernel_ctx_resolver: Arc::new(std::sync::Mutex::new(None)),
            deferred_question_sink: self.deferred_question_sink.clone(),
        }
    }

    /// Attach a query-event sender so permission queries are routed
    /// through the executor event loop (guaranteeing ordering with
    /// `ToolDispatchStart`). Called by the executor at the start of
    /// each prompt turn; cleared when the turn ends.
    pub fn set_query_event_tx(&self, tx: Option<mpsc::UnboundedSender<crate::query::QueryEvent>>) {
        self.set_query_event_sender(tx.map(crate::turn_hook::QueryEventSender::without_hooks));
    }

    pub(crate) fn set_query_event_sender(
        &self,
        sender: Option<crate::turn_hook::QueryEventSender>,
    ) {
        *self.query_event_tx.lock().expect("poisoned") = sender;
    }

    /// Announce that the auto-mode gate let a tool call through without a
    /// permission dialog. Routed through the query-event channel so it lands
    /// after the call's `ToolDispatchStart`, exactly like a permission query.
    ///
    /// Best effort by design: a sub-agent broker with no query-event tx (or a
    /// call the model never gave an id) simply renders without the note.
    fn emit_auto_mode_allowed(&self, context: &ToolContext, source: AutoModeAllowSource) {
        let Some(tool_use_id) = context.tool_use_id() else {
            return;
        };
        let tx = self.query_event_tx.lock().expect("poisoned").clone();
        let Some(tx) = tx else {
            return;
        };
        let _ = tx.send(crate::query::QueryEvent::ToolAutoModeAllowed {
            tool_use_id: tool_use_id.to_owned(),
            source,
        });
    }

    /// Send a permission query directly via the `sender` channel.
    /// Used by the executor event loop to forward
    /// `QueryEvent::PermissionQuery` to the TUI's `permission_rx`.
    pub fn forward_direct(&self, query: OutboundPermissionQuery) {
        if let Err(err) = self.sender.lock().expect("poisoned").send(query) {
            tracing::warn!("ChannelPermissionBroker::forward_direct: receiver gone: {err}");
        }
    }

    /// Install auto-mode hooks (denial sink + mode provider). Pass
    /// `None` to clear them. The broker clones the hooks out under a
    /// short lock on each dispatch so the caller can rotate the
    /// mode provider without racing the executor.
    pub fn set_auto_mode_hooks(&self, hooks: Option<AutoModeHooks>) {
        *self.auto_mode_hooks.lock().expect("poisoned") = hooks;
    }

    pub fn set_auto_mode_classifier(&self, classifier: Option<Arc<dyn AutoModeClassifier>>) {
        *self.auto_mode_classifier.lock().expect("poisoned") = classifier;
    }

    fn current_classifier(&self) -> Option<Arc<dyn AutoModeClassifier>> {
        self.auto_mode_classifier.lock().expect("poisoned").clone()
    }

    /// Install (or with `None`, remove) the sink deferred questions answer
    /// into. See [`crate::deferred_question`].
    pub fn set_deferred_question_sink(&self, sink: Option<Arc<dyn DeferredQuestionSink>>) {
        *self.deferred_question_sink.lock().expect("poisoned") = sink;
    }

    pub fn has_deferred_question_sink(&self) -> bool {
        self.deferred_question_sink
            .lock()
            .expect("poisoned")
            .is_some()
    }

    /// Whether this ask may return before the user answers, and if so what
    /// the answer needs to be delivered later.
    ///
    /// Only a plain `AskUserQuestion` from the main thread qualifies. A
    /// grill or ultraplan question gates what happens next — its
    /// `metadata.intent` or the run it belongs to reads the answer
    /// synchronously — and a sub-agent's or teammate's question has no user
    /// message to come back as. The tool is re-resolved as an owned handle
    /// because the call that finally records the answer outlives this
    /// borrow; with no resolver on the context the question simply waits.
    fn question_deferral(
        &self,
        tool: &dyn Tool,
        input: &Value,
        context: &ToolContext,
    ) -> Option<QuestionDeferral> {
        if tool.id().as_str() != deferred_question::ASK_USER_QUESTION_TOOL
            || !deferred_question::deferred_questions_enabled()
            || context.agent_id().is_some()
            || context.ultraplan_context().is_some()
            || input
                .get("metadata")
                .and_then(|metadata| metadata.get("intent"))
                .is_some()
        {
            return None;
        }
        let sink = self
            .deferred_question_sink
            .lock()
            .expect("poisoned")
            .clone()?;
        let tool_use_id = context.tool_use_id()?.to_owned();
        let tool = context
            .tool_resolver()?
            .resolve(tool.id().as_str(), context.tool_filter())
            .ok()
            .flatten()?;
        Some(QuestionDeferral {
            sink,
            tool,
            session_id: self.session_id.lock().expect("poisoned").clone(),
            tool_use_id,
        })
    }

    /// Hand `query` to whoever shows it.
    ///
    /// Routes through the query-event channel when available so the
    /// permission query arrives at the TUI AFTER the ToolDispatchStart event
    /// (FIFO ordering guarantee). Falls back to the direct sender for
    /// sub-agents, tests, or when no query-event tx is wired.
    fn send_query(&self, tool: &dyn Tool, query: OutboundPermissionQuery) -> ToolResult<()> {
        let maybe_tx = self.query_event_tx.lock().expect("poisoned").clone();
        if let Some(tx) = maybe_tx {
            // Attempt to route through the executor event loop.
            // On failure (channel closed), recover the query and
            // fall back to direct send.
            match tx.send(crate::query::QueryEvent::PermissionQuery(query)) {
                Ok(()) => {}
                Err(mpsc::error::SendError(crate::query::QueryEvent::PermissionQuery(
                    recovered,
                ))) => {
                    self.sender
                        .lock()
                        .expect("poisoned")
                        .send(recovered)
                        .map_err(|_| ToolError::Execution {
                            tool: tool.id(),
                            source: anyhow::anyhow!("permission request receiver gone"),
                        })?;
                }
                Err(_) => unreachable!("we sent PermissionQuery"),
            }
        } else {
            self.sender
                .lock()
                .expect("poisoned")
                .send(query)
                .map_err(|_| ToolError::Execution {
                    tool: tool.id(),
                    source: anyhow::anyhow!("permission request receiver gone"),
                })?;
        }
        Ok(())
    }

    /// The `permission-rules` seat as this broker's scope sees it.
    ///
    /// Empty without a kernel scope, which is the engine's own behaviour: a
    /// rule can only add a prompt, never remove one, so a broker that cannot
    /// see the seat fails closed by construction.
    fn permission_rules(&self) -> crate::permission_seat::PermissionRules {
        self.kernel_ctx
            .lock()
            .expect("poisoned")
            .as_ref()
            .map(|lease| crate::permission_seat::rules_for(lease.context()))
            .unwrap_or_default()
    }

    /// Snapshot the current hooks, if any. Kept cheap — the lock is
    /// only held long enough to clone the `Arc`s inside.
    fn current_hooks(&self) -> Option<AutoModeHooks> {
        self.auto_mode_hooks.lock().expect("poisoned").clone()
    }

    /// Auto-mode gate decision for a tool call that would otherwise ask.
    ///
    /// Permission rules have already had their chance to allow or deny before
    /// this runs. Every remaining action is classified with the conversation
    /// transcript; only tools whose permission response is itself user data or
    /// a product decision stay on the interactive path.
    async fn auto_mode_gate(
        tool_name: &str,
        input: &Value,
        context: &ToolContext,
        hooks: &AutoModeHooks,
        classifier: Option<Arc<dyn AutoModeClassifier>>,
        rules: &PermissionRules,
    ) -> AutoModeGateOutcome {
        if requires_permission_broker_response(tool_name)
            || requires_workflow_review(tool_name)
            || rules.requires_user_decision(tool_name, context).is_some()
            || edits_git_metadata(tool_name, input, context)
        {
            return AutoModeGateOutcome::Ask;
        }

        let verdicts = hooks.verdicts();
        let input_json = input.to_string();
        let transcript = context
            .auto_mode_classifier_transcript()
            .unwrap_or_default();
        let classifier_key = auto_mode_classifier_cache_key(&input_json, transcript, context);
        let denial_key = auto_mode_denial_cache_key(&input_json, transcript, context);
        if verdicts.take_exemption(tool_name, &input_json) {
            verdicts.forget_verdict(tool_name, &classifier_key);
            verdicts.forget_verdict(tool_name, &denial_key);
            tracing::info!(
                tool = tool_name,
                "auto-mode gate consumed a user-approved one-shot exemption"
            );
            return AutoModeGateOutcome::Run(AutoModeAllowSource::UserExemption);
        }

        if let Some(reason) = verdicts.denied_reason(tool_name, &denial_key) {
            tracing::debug!(
                tool = tool_name,
                "auto-mode gate replayed a classifier denial for unchanged user authorization context"
            );
            return AutoModeGateOutcome::Deny {
                reason,
                fresh: false,
            };
        }
        if verdicts.is_allowed(tool_name, &classifier_key) {
            tracing::debug!(
                tool = tool_name,
                "auto-mode gate replayed a classifier allow for unchanged context"
            );
            return AutoModeGateOutcome::Run(AutoModeAllowSource::CachedVerdict);
        }

        let Some(classifier) = classifier else {
            tracing::warn!(
                tool = tool_name,
                "auto-mode classifier is not installed; blocking for safety"
            );
            return AutoModeGateOutcome::Deny {
                reason: auto_mode_classifier_failure_reason(
                    "Classifier unavailable - blocking for safety",
                ),
                fresh: true,
            };
        };
        let request = AutoModeClassifierRequest {
            tool_name: tool_name.to_owned(),
            tool_input: input.clone(),
            tool_use_id: context.tool_use_id().map(str::to_owned),
            transcript: transcript.to_owned(),
            cwd: context.cwd().map(str::to_owned),
            isolated_worktree: context.is_isolated_worktree(),
            workflow_nesting_depth: context.workflow_nesting_depth(),
        };
        let result =
            tokio::time::timeout(AUTO_MODE_CLASSIFIER_TIMEOUT, classifier.classify(request)).await;

        if !hooks.auto_gates(hooks.current_mode()) {
            tracing::debug!(
                tool = tool_name,
                "auto-mode classifier verdict ignored after permission mode changed"
            );
            return AutoModeGateOutcome::Ask;
        }

        match result {
            Ok(Ok(AutoModeClassifierOutcome::Allow { reason, stage })) => {
                tracing::debug!(
                    tool = tool_name,
                    %reason,
                    ?stage,
                    "auto-mode classifier allowed tool call"
                );
                verdicts.mark_allowed(tool_name, &classifier_key);
                AutoModeGateOutcome::Run(AutoModeAllowSource::Classifier)
            }
            Ok(Ok(AutoModeClassifierOutcome::Block { reason, category })) => {
                tracing::info!(
                    tool = tool_name,
                    %reason,
                    category = category.as_deref().unwrap_or("unknown"),
                    "auto-mode classifier blocked tool call"
                );
                let detail = category
                    .as_deref()
                    .filter(|category| !reason.starts_with(&format!("[{category}]")))
                    .map(|category| format!("[{category}] {reason}"))
                    .unwrap_or(reason);
                deny_via_cache(
                    verdicts,
                    tool_name,
                    &denial_key,
                    auto_mode_deny_reason(&sanitize_classifier_reason(&detail)),
                )
            }
            Ok(Err(error)) => {
                tracing::warn!(tool = tool_name, %error, "auto-mode classifier failed; blocking for safety");
                AutoModeGateOutcome::Deny {
                    reason: auto_mode_classifier_failure_reason(
                        &auto_mode_classifier_failure_detail(&error),
                    ),
                    fresh: true,
                }
            }
            Err(_) => {
                tracing::warn!(
                    tool = tool_name,
                    "auto-mode classifier request timed out; blocking for safety"
                );
                AutoModeGateOutcome::Deny {
                    reason: auto_mode_classifier_failure_reason(
                        "Classifier request timed out - blocking for safety",
                    ),
                    fresh: true,
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Magic prefix the permission UI puts in front of workflow-revision
/// feedback. Shared with the CLI producer so the two sides cannot
/// drift apart: feedback that does not start with this exact prefix is
/// treated as a plain denial note, never as a revision request.
pub const WORKFLOW_REVISION_FEEDBACK_PREFIX: &str = "Please revise the workflow before running it.";
pub const ULTRAPLAN_REJECTION_FEEDBACK_PREFIX: &str = "ULTRAPLAN_PLAN_REJECTED";

pub(crate) fn workflow_revision_requested_result(
    tool_name: &str,
    feedback: Option<&str>,
) -> Option<Value> {
    if !matches!(tool_name, "Workflow" | "RunWorkflow") {
        return None;
    }
    let feedback = feedback.map(str::trim).filter(|text| !text.is_empty())?;
    if !feedback.starts_with(WORKFLOW_REVISION_FEEDBACK_PREFIX) {
        return None;
    }
    Some(serde_json::json!({
        "status": "revision_requested",
        "action": "regenerate_workflow",
        "message": "The current Workflow call was not approved and was not run. Regenerate a new Workflow call that incorporates the requested changes before running it.",
        "feedback": feedback,
    }))
}

/// Construct an [`AutoModeDenialInput`] from a tool invocation + the
/// reason string the `PermissionDecision::deny(...)` carried. Kept as
/// a standalone helper so the broker's `resolve()` body stays flat.
/// Outcome of the auto-mode gate for a would-be `Ask` dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoModeGateOutcome {
    /// Run the tool without a dialog (auto-mode short circuit), naming
    /// which part of the gate decided so the tool row can attribute it.
    Run(AutoModeAllowSource),
    /// Fall through to the interactive approval dialog.
    Ask,
    /// Reject the call with `reason` returned to the model. `fresh` is
    /// false when this replays a remembered denial for the same invocation and
    /// user authorization context — the denial store already holds its record.
    Deny { reason: String, fresh: bool },
}

fn auto_mode_classifier_cache_key(
    input_json: &str,
    transcript: &str,
    context: &ToolContext,
) -> String {
    let cwd = context.cwd().unwrap_or_default();
    let mut key = String::with_capacity(input_json.len() + transcript.len() + cwd.len() + 32);
    key.push_str(input_json);
    key.push('\0');
    key.push_str(transcript);
    key.push('\0');
    key.push_str(cwd);
    key.push('\0');
    key.push_str(if context.is_isolated_worktree() {
        "1"
    } else {
        "0"
    });
    key.push('\0');
    key.push_str(&context.workflow_nesting_depth().to_string());
    rebon_api::stable_hash_str(&key)
}

fn auto_mode_denial_cache_key(input_json: &str, transcript: &str, context: &ToolContext) -> String {
    auto_mode_classifier_cache_key(
        input_json,
        transcript_through_latest_user_turn(transcript),
        context,
    )
}

fn transcript_through_latest_user_turn(transcript: &str) -> &str {
    let mut offset = 0;
    let mut in_user_turn = false;
    let mut last_user_end = None;

    for line in transcript.split_inclusive('\n') {
        let line_end = offset + line.len();
        let content = line.strip_suffix('\n').unwrap_or(line);
        if content.starts_with("User: ") {
            in_user_turn = true;
            last_user_end = Some(line_end);
        } else if in_user_turn && content.starts_with("  ") {
            last_user_end = Some(line_end);
        } else {
            in_user_turn = false;
        }
        offset = line_end;
    }

    &transcript[..last_user_end.unwrap_or(0)]
}

/// Model- and user-facing description of a classifier failure.
///
/// The headline stays fixed per kind — `tool_outcome_code` and the
/// `/permissions` surface both match on it. What follows is the part
/// that used to exist only in a log line nobody reads: which stage and
/// which model. Without it a misbehaving classifier model is
/// indistinguishable from a dead API key, and both read as "retry
/// later" forever.
///
/// What deliberately does *not* follow is the classifier's own reply.
/// This text lands in the agent's transcript, and the agent is what
/// the classifier is judging — quoting its thinking back both spends
/// tokens and shows the watched model how it is being watched. The
/// reply stays in the log, where `parse_failure` puts it.
fn auto_mode_classifier_failure_detail(error: &anyhow::Error) -> String {
    let classifier_error = error.downcast_ref::<AutoModeClassifierError>();
    let headline = match classifier_error.map(AutoModeClassifierError::kind) {
        Some(AutoModeClassifierFailureKind::Parsing) => {
            "Classifier response could not be parsed - blocking for safety"
        }
        Some(AutoModeClassifierFailureKind::Refusal) => {
            "Classifier request was refused - blocking for safety"
        }
        Some(AutoModeClassifierFailureKind::TranscriptTooLong) => {
            "Classifier transcript exceeded the model context window - blocking for safety"
        }
        Some(AutoModeClassifierFailureKind::OutputBudgetExhausted) => {
            "Classifier ran out of output budget - blocking for safety"
        }
        Some(AutoModeClassifierFailureKind::Unavailable) | None => {
            "Classifier unavailable - blocking for safety"
        }
    };
    // Classifier output is model text, so it goes through the same
    // sanitizer as a block reason before it is quoted back.
    match classifier_error.and_then(AutoModeClassifierError::detail) {
        Some(detail) => format!("{headline} ({})", sanitize_classifier_reason(detail)),
        None => headline.to_owned(),
    }
}

/// Record the denial authorization fingerprint and produce the gate outcome.
/// Only a fresh fingerprint appends a `/permissions` record, so autonomous
/// retries cannot flood the 20-slot ring buffer or re-roll the classifier.
fn deny_via_cache(
    verdicts: &std::sync::Arc<rebon_permissions::AutoModeVerdictCache>,
    tool_name: &str,
    denial_key: &str,
    reason: String,
) -> AutoModeGateOutcome {
    let fresh = verdicts.mark_denied(tool_name, denial_key, &reason);
    AutoModeGateOutcome::Deny { reason, fresh }
}

fn auto_mode_classifier_failure_reason(detail: &str) -> String {
    format!(
        "auto mode blocked this call without asking the user: {detail}. \
         The tool was not run. Retry after the classifier is available, or \
         switch out of auto mode if manual approval is required."
    )
}

/// Model-facing denial message. Names the flagged behavior and the
/// legitimate ways forward so the agent adjusts instead of blindly
/// retrying: rewrite, skip, or escalate to the user.
fn auto_mode_deny_reason(detail: &str) -> String {
    format!(
        "auto mode denied this call without asking the user: {detail}. \
         Rewrite the command to avoid the flagged behavior, skip it if it \
         is not essential, or ask the user for approval; the denied call \
         is also listed in /permissions for approve or retry. Resubmitting \
         it unchanged will be denied again."
    )
}

/// Model-facing denial for `dontAsk`. That mode suppresses approval
/// dialogs instead of granting them, so the way forward is a rule that
/// already covers the call — not a retry, which is denied identically.
/// Deliberately does not point at `/permissions`: its approve/retry flow
/// installs auto-mode exemptions, which this mode never consults.
fn dont_ask_deny_reason(tool_name: &str) -> String {
    format!(
        "`dontAsk` permission mode denied this {tool_name} call: it required \
         approval and this session never prompts. Take an approach already \
         covered by an allow rule, skip the step if it is not essential, or \
         tell the user which permission rule to add. Resubmitting the same \
         call will be denied again."
    )
}

/// The workflow review is an approval prompt, so `dontAsk` refuses it like
/// any other Ask — but the way out is a mode switch, not an allow rule, so
/// the reason spells the modes out instead of pointing at `/permissions`.
fn dont_ask_workflow_deny_reason(tool_name: &str) -> String {
    format!(
        "`dontAsk` permission mode denied this {tool_name} call: launching a \
         workflow opens a review prompt and this session never prompts. Ask \
         the user to switch to a prompting permission mode (default, \
         acceptEdits, or auto) to review the workflow, or to \
         bypassPermissions to run it unreviewed. Resubmitting the same call \
         will be denied again."
    )
}

/// What the active permission mode decides about a call that would otherwise
/// open an approval prompt.
pub(crate) enum ModeAskOutcome {
    /// Run the tool now, with no prompt. `Some` only through auto mode's
    /// gate, the one short circuit that is a *verdict* worth attributing;
    /// `bypassPermissions` and `acceptEdits` just run.
    Run(Option<AutoModeAllowSource>),
    /// Refuse. `reason` goes back to the model as the tool error.
    Deny(String),
    /// The mode has no opinion — the caller prompts over its own transport.
    Ask,
}

/// The single implementation of permission-mode semantics.
///
/// [`ChannelPermissionBroker`] (TUI and the desktop worker) and
/// [`crate::AcpPermissionBroker`] (third-party ACP clients) differ only in how
/// they *ask* a human. What each mode means must not differ at all, so neither
/// is allowed to re-derive it — a second copy is what drifts.
///
/// Carve-outs that apply to every non-default mode:
///
/// * `AskUserQuestion` and `Agent` always reach the prompt. Their response is
///   the tool's data (the user's answer, an Agent call's authorized roots), not
///   an approval, so resolving it here would delete a data path rather than a
///   confirmation.
/// * `Workflow`/`RunWorkflow` reach the prompt in every prompting mode: the
///   review is a plan-approval step whose answer can carry revision feedback
///   (an edited workflow), so auto-resolving it would delete the user's only
///   chance to reshape the run before it starts. The two "never prompt" modes
///   keep their contracts instead: `bypassPermissions` runs the workflow
///   unreviewed, and `dontAsk` denies it with a reason that names the modes
///   that can review it — its settings copy promises "no prompts", and
///   non-interactive surfaces would hang on a prompt it forced anyway.
/// * Anything a feature claimed on the `permission-rules` seat reaches the
///   prompt: a tool whose entire output is a user decision has nothing left
///   when a mode resolves it. A claim of
///   [`DecisionScope::EvenUnderBypass`](crate::permission_seat::DecisionScope)
///   outranks even `bypassPermissions`, for a gate whose absence would delete
///   a workflow's only checkpoint rather than skip a confirmation. Plan mode
///   makes both kinds of claim and the profile feature makes the stronger one;
///   the engine no longer knows any of their tools by name. Under `dontAsk`
///   the claim still resolves as a refusal — that mode never prompts — but a
///   rule may word the refusal itself, which is how a tool with another road
///   to the same outcome names it.
///
/// `Deny` decisions never reach here: a deny rule or hook resolves the call
/// before any mode is consulted.
pub(crate) async fn resolve_ask_under_mode(
    mode: PermissionMode,
    tool_name: &str,
    input: &Value,
    context: &ToolContext,
    hooks: Option<&AutoModeHooks>,
    classifier: Option<Arc<dyn AutoModeClassifier>>,
    rules: &PermissionRules,
) -> ModeAskOutcome {
    // What the features on the `permission-rules` seat say about this call.
    // A rule can only ever *add* a prompt, so an empty seat is the engine's
    // own behaviour and a plugin that unloaded mid-turn cannot open a gate.
    let seat_decision = rules.requires_user_decision(tool_name, context);
    let seat_requires_ask = seat_decision.is_some();
    let seat_outranks_bypass = seat_decision == Some(DecisionScope::EvenUnderBypass);
    let needs_response = requires_permission_broker_response(tool_name);
    let workflow_review = requires_workflow_review(tool_name);

    match mode {
        // "Bypass prompts": the call runs instead of being confirmed. No
        // classifier and no denial record — nothing is being judged. The
        // workflow review is NOT exempt from this contract: bypass runs the
        // workflow unreviewed, exactly like every other call.
        PermissionMode::BypassPermissions => {
            if needs_response || seat_outranks_bypass {
                return ModeAskOutcome::Ask;
            }
            tracing::debug!(
                tool = tool_name,
                "bypassPermissions: running without a prompt"
            );
            ModeAskOutcome::Run(None)
        }
        // Bypass's opposite: also never prompts, but resolves by refusing, with
        // the reason handed back so the model can adapt.
        //
        // No ultraplan carve-out here. That exemption exists to *force* a
        // final approval, and a mode contracted never to prompt has nothing to
        // fall back to — so an ultraplan plan cannot be approved under
        // `dontAsk`, which fails closed rather than quietly prompting.
        //
        // The workflow review fails closed the same way. Unlike
        // `AskUserQuestion` its response is an approval, not tool data, and a
        // mode whose settings copy promises "no prompts" cannot spring a
        // review modal (or park a headless surface on one). The deny reason
        // names the modes that can review the workflow instead.
        PermissionMode::DontAsk => {
            if needs_response {
                return ModeAskOutcome::Ask;
            }
            if workflow_review {
                tracing::debug!(
                    tool = tool_name,
                    "dontAsk: refusing a workflow review that would prompt"
                );
                return ModeAskOutcome::Deny(dont_ask_workflow_deny_reason(tool_name));
            }
            // A feature on the seat can word its own refusal — the decision is
            // the same either way, but a tool the user has another road to
            // (`/profile`) says so, and the model stops retrying.
            if let Some(reason) = rules.deny_reason_when_never_prompting(tool_name) {
                tracing::debug!(
                    tool = tool_name,
                    "dontAsk: refusing a call whose feature words its own refusal"
                );
                return ModeAskOutcome::Deny(reason);
            }
            tracing::debug!(
                tool = tool_name,
                "dontAsk: refusing a call that would prompt"
            );
            ModeAskOutcome::Deny(dont_ask_deny_reason(tool_name))
        }
        // File edits only. Everything else falls through exactly as in
        // `default` — the "other operations follow policy" half of the promise.
        PermissionMode::AcceptEdits => {
            if is_accept_edits_tool(tool_name) && !edits_git_metadata(tool_name, input, context) {
                tracing::debug!(
                    tool = tool_name,
                    "acceptEdits: running a file edit without a prompt"
                );
                ModeAskOutcome::Run(None)
            } else {
                ModeAskOutcome::Ask
            }
        }
        PermissionMode::Auto => {
            let Some(hooks) = hooks else {
                if needs_response || workflow_review || seat_requires_ask {
                    return ModeAskOutcome::Ask;
                }
                tracing::warn!(
                    tool = tool_name,
                    "auto mode has no classifier state installed; blocking for safety"
                );
                return ModeAskOutcome::Deny(auto_mode_classifier_failure_reason(
                    "Classifier state unavailable - blocking for safety",
                ));
            };
            if seat_requires_ask {
                return ModeAskOutcome::Ask;
            }
            ask_through_auto_gate(tool_name, input, context, hooks, classifier, rules).await
        }
        // Plan mode entered from auto keeps auto's gate (see
        // `AutoModeHooks::auto_gates`). What plan forbids still comes from the
        // prompt, not from here. Without hooks nothing recorded where plan was
        // entered from, and it prompts like any other plan.
        PermissionMode::Plan => match hooks.filter(|hooks| hooks.auto_gates(mode)) {
            Some(hooks) if !seat_requires_ask => {
                ask_through_auto_gate(tool_name, input, context, hooks, classifier, rules).await
            }
            _ => ModeAskOutcome::Ask,
        },
        PermissionMode::Default | PermissionMode::Bubble => ModeAskOutcome::Ask,
    }
}

/// A would-be prompt put to auto mode's gate: run on its verdict, refuse with
/// a `/permissions` record, or fall through to the prompt.
async fn ask_through_auto_gate(
    tool_name: &str,
    input: &Value,
    context: &ToolContext,
    hooks: &AutoModeHooks,
    classifier: Option<Arc<dyn AutoModeClassifier>>,
    rules: &PermissionRules,
) -> ModeAskOutcome {
    match ChannelPermissionBroker::auto_mode_gate(
        tool_name, input, context, hooks, classifier, rules,
    )
    .await
    {
        AutoModeGateOutcome::Run(source) => ModeAskOutcome::Run(Some(source)),
        AutoModeGateOutcome::Deny { reason, fresh } => {
            // Only a first-time denial appends a `/permissions` record;
            // a replayed one is already in the store.
            if fresh {
                let record = build_denial_input(tool_name, context, input, Some(&reason));
                let _ = hooks.sink.record(record);
            }
            ModeAskOutcome::Deny(reason)
        }
        AutoModeGateOutcome::Ask => ModeAskOutcome::Ask,
    }
}

/// The classifier reason is model output derived from untrusted transcript and
/// tool input: bound its length and strip control characters before it enters
/// tool results and the `/permissions` list.
fn sanitize_classifier_reason(reason: &str) -> String {
    const MAX_CHARS: usize = 280;
    let mut cleaned: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() > MAX_CHARS {
        cleaned = cleaned.chars().take(MAX_CHARS).collect();
        cleaned.push('…');
    }
    cleaned
}

fn build_denial_input(
    tool_name: &str,
    context: &ToolContext,
    input: &Value,
    reason: Option<&str>,
) -> AutoModeDenialInput {
    // One-line display — full payload lives in `tool_input`.
    let display = match input.as_object().and_then(|o| o.get("command")) {
        Some(Value::String(cmd)) if !cmd.is_empty() => {
            format!("{tool_name}: {}", truncate(cmd, 80))
        }
        _ => tool_name.to_string(),
    };
    AutoModeDenialInput {
        tool_use_id: context
            .tool_use_id()
            .map(str::to_owned)
            .unwrap_or_else(|| "tool-call".into()),
        tool_name: tool_name.to_string(),
        tool_input: input.to_string(),
        reason: reason.unwrap_or("auto-mode denial").to_string(),
        display,
        timestamp_ms: now_ms(),
        task_id: None,
        conversation_id: None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

fn inject_add_dir_allow_option(decision: &mut PermissionDecision, context: &ToolContext) {
    if context.additional_working_directories().is_empty() {
        return;
    }
    let Some(input) = decision.updated_input.as_ref() else {
        return;
    };
    let Some(path) = input.get("file_path").and_then(Value::as_str) else {
        return;
    };
    if !is_under_add_dir(path, context.additional_working_directories()) {
        return;
    }
    let Some(request) = decision.request.as_mut() else {
        return;
    };
    if !request.options.iter().any(|option| option == "allow_once") {
        request.options.insert(0, "allow_once".to_string());
    }
}

fn is_under_add_dir(path: &str, dirs: &[String]) -> bool {
    let Some(path) = canonical_path_for_compare(path) else {
        return false;
    };
    dirs.iter().any(|dir| {
        canonical_path_for_compare(dir)
            .as_ref()
            .is_some_and(|dir| path == *dir || path.starts_with(dir))
    })
}

fn canonical_path_for_compare(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let canonical = std::fs::canonicalize(trimmed).ok()?;
    Some(normalize_canonical_path_for_compare(&canonical))
}

fn normalize_canonical_path_for_compare(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_ascii_lowercase())
    } else {
        path.to_path_buf()
    }
}

#[async_trait]
impl PermissionBroker for ChannelPermissionBroker {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        let hooks = self.current_hooks();
        // Read once per dispatch, never snapshotted at construction: switching
        // a live session's mode has to take effect on the next tool call.
        let mode = hooks
            .as_ref()
            .map(|h| h.current_mode())
            .unwrap_or(PermissionMode::Default);
        // Auto mode, or plan mode entered from it: the modes whose prompts the
        // classifier answers, and so the ones that owe the user a record of
        // what it refused and a note on what it let through.
        let in_auto = hooks.as_ref().is_some_and(|h| h.auto_gates(mode));

        match decision.behavior {
            PermissionBehavior::Allow => {
                let effective_input = decision.updated_input.unwrap_or(input);
                tool.call(effective_input, context).await
            }
            PermissionBehavior::Deny => {
                let reason = decision
                    .reason
                    .unwrap_or_else(|| "tool permission denied".into());
                // Auto mode: capture every denial into the sink so the
                // user can review + retry from /permissions.
                if in_auto {
                    if let Some(h) = hooks.as_ref() {
                        let record =
                            build_denial_input(tool.id().as_str(), context, &input, Some(&reason));
                        let _ = h.sink.record(record);
                    }
                }
                Err(ToolError::PermissionDenied {
                    tool: tool.id(),
                    reason,
                })
            }
            PermissionBehavior::Ask => {
                let mut decision = decision;
                inject_add_dir_allow_option(&mut decision, context);
                // One snapshot of the `permission-rules` seat for the whole
                // decision, so a plugin unloading mid-ask cannot change the
                // answer between the mode check and the dialog.
                let rules = self.permission_rules();
                let check_input = decision.updated_input.as_ref().unwrap_or(&input);
                match resolve_ask_under_mode(
                    mode,
                    tool.id().as_str(),
                    check_input,
                    context,
                    hooks.as_ref(),
                    self.current_classifier(),
                    &rules,
                )
                .await
                {
                    ModeAskOutcome::Run(source) => {
                        // Auto mode (and plan mode entered from it) is the
                        // only mode that runs a would-be dialog on a
                        // classifier verdict, so it is the only one that owes
                        // the user a visible "this was allowed
                        // for you" note on the tool row — and the note names
                        // which part of the gate decided, because an
                        // exemption the user granted is not auto mode's call.
                        if in_auto {
                            self.emit_auto_mode_allowed(
                                context,
                                source.unwrap_or(AutoModeAllowSource::Unspecified),
                            );
                        }
                        let effective_input = decision.updated_input.unwrap_or(input);
                        return tool.call(effective_input, context).await;
                    }
                    ModeAskOutcome::Deny(reason) => {
                        return Err(ToolError::PermissionDenied {
                            tool: tool.id(),
                            reason,
                        });
                    }
                    ModeAskOutcome::Ask => {}
                }
                let request = decision
                    .request
                    .ok_or_else(|| ToolError::PermissionDenied {
                        tool: tool.id(),
                        reason: decision
                            .reason
                            .unwrap_or_else(|| "permission request missing payload".into()),
                    })?;

                let title = request.title.clone();
                let message = request.message.clone();
                let metadata = request.metadata.clone();
                let permission_authorized_context =
                    rebon_tool::context_with_permission_authorized_paths(
                        tool.id().as_str(),
                        context,
                        &request,
                    );
                let options = build_options(request, &rules);
                // Kernel plugins listening on the `permission/ask` waterfall
                // may answer (or cancel) the ask before it reaches the UI.
                // Without a kernel context or listeners this is a no-op.
                let kernel_answer = self.kernel_waterfall_answer(
                    tool.id().as_str(),
                    context.tool_use_id(),
                    &title,
                    &message,
                    decision.updated_input.as_ref(),
                    &options,
                );
                let answer = if let Some(answer) = kernel_answer {
                    answer
                } else {
                    let deferral = self.question_deferral(
                        tool,
                        decision.updated_input.as_ref().unwrap_or(&input),
                        context,
                    );
                    let (response_tx, response_rx) = oneshot::channel();
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);

                    let query = OutboundPermissionQuery {
                        id,
                        tool_name: tool.id().as_str().to_owned(),
                        tool_call_id: context
                            .tool_use_id()
                            .map(str::to_owned)
                            .unwrap_or_else(|| "tool-call".into()),
                        session_id: self.session_id.lock().expect("poisoned").clone(),
                        title,
                        message,
                        tool_input: decision.updated_input.clone(),
                        metadata,
                        options,
                        response_tx,
                    };

                    self.send_query(tool, query)?;

                    // The dialog is up either way; a deferred question just
                    // stops waiting for it here.
                    if let Some(deferral) = deferral {
                        let pending = deferred_question::pending_result(&deferral.tool_use_id);
                        deferral.await_answer(
                            response_rx,
                            decision.updated_input.unwrap_or(input),
                            permission_authorized_context.unwrap_or_else(|| context.clone()),
                            rules,
                        );
                        return Ok(pending);
                    }

                    response_rx.await.map_err(|_| ToolError::Execution {
                        tool: tool.id(),
                        source: anyhow::anyhow!("permission response dropped"),
                    })?
                };

                match answer {
                    PermissionAnswer::Selected {
                        option_id,
                        updated_input,
                        extra_text,
                    } => {
                        if is_reject_option(&option_id) {
                            // Revision requests only make sense for a
                            // one-shot rejection; "reject always" means
                            // the user wants the tool stopped, not a
                            // regenerate-and-retry loop.
                            if !is_reject_always_option(&option_id) {
                                if let Some(output) = workflow_revision_requested_result(
                                    tool.id().as_str(),
                                    extra_text.as_deref(),
                                ) {
                                    return Ok(output);
                                }
                            }
                            return Err(ToolError::PermissionDenied {
                                tool: tool.id(),
                                reason: permission_denial_reason(
                                    &option_id,
                                    extra_text.as_deref(),
                                    rules.rejection_note(tool.id().as_str()).as_ref(),
                                ),
                            });
                        }
                        call_approved(
                            tool,
                            &option_id,
                            updated_input.or(decision.updated_input).unwrap_or(input),
                            permission_authorized_context.unwrap_or_else(|| context.clone()),
                            &rules,
                            &extra_text,
                        )
                        .await
                    }
                    PermissionAnswer::Cancelled => Err(ToolError::PermissionDenied {
                        tool: tool.id(),
                        reason: "permission request cancelled".into(),
                    }),
                }
            }
        }
    }
}

/// Run a call the user approved, the way both a waiting and a deferred ask
/// finish.
async fn call_approved(
    tool: &dyn Tool,
    option_id: &str,
    mut effective_input: Value,
    mut call_context: ToolContext,
    rules: &PermissionRules,
    extra_text: &Option<String>,
) -> ToolResult<Value> {
    // An approved option means whatever the feature that
    // offered it says: it may rewrite the input it
    // authorised and mark the context the call runs under.
    if let Some(approved) = rules.on_approved(
        tool.id().as_str(),
        option_id,
        &mut effective_input,
        &call_context,
    ) {
        call_context = approved;
    }
    let mut output = tool.call(effective_input, &call_context).await?;
    append_permission_extra_text(&mut output, extra_text);
    Ok(output)
}

/// What a deferred question needs once its answer arrives, after the call
/// that asked has returned. See [`ChannelPermissionBroker::question_deferral`].
struct QuestionDeferral {
    sink: Arc<dyn DeferredQuestionSink>,
    tool: Arc<dyn Tool>,
    session_id: String,
    tool_use_id: String,
}

impl QuestionDeferral {
    /// Wait for the dialog in the background and deliver what the user did.
    ///
    /// An answer goes through the tool exactly as a waiting ask's would, so
    /// the model reads the same content either way. A dialog closed without
    /// an answer (the surface dropped it, say on exit) delivers nothing:
    /// nobody is there to have said anything.
    fn await_answer(
        self,
        response_rx: oneshot::Receiver<PermissionAnswer>,
        input: Value,
        call_context: ToolContext,
        rules: PermissionRules,
    ) {
        tokio::spawn(async move {
            let Ok(answer) = response_rx.await else {
                tracing::info!(
                    session_id = %self.session_id,
                    tool_use_id = %self.tool_use_id,
                    "deferred question closed without an answer; nothing to deliver"
                );
                return;
            };
            let delivery = match answer {
                PermissionAnswer::Selected {
                    option_id,
                    extra_text,
                    ..
                } if is_reject_option(&option_id) => deferred_question::dismissed(
                    &self.session_id,
                    &self.tool_use_id,
                    extra_text.as_deref(),
                ),
                PermissionAnswer::Selected {
                    option_id,
                    updated_input,
                    extra_text,
                } => match call_approved(
                    self.tool.as_ref(),
                    &option_id,
                    updated_input.unwrap_or(input),
                    call_context,
                    &rules,
                    &extra_text,
                )
                .await
                {
                    Ok(output) => {
                        deferred_question::answered(&self.session_id, &self.tool_use_id, &output)
                    }
                    Err(error) => deferred_question::unreadable(
                        &self.session_id,
                        &self.tool_use_id,
                        &error.to_string(),
                    ),
                },
                PermissionAnswer::Cancelled => {
                    deferred_question::dismissed(&self.session_id, &self.tool_use_id, None)
                }
            };
            self.sink.deliver(delivery);
        });
    }
}

pub fn user_prompt_permission_broker_from(
    broker: &Arc<dyn PermissionBroker>,
) -> Option<Arc<dyn PermissionBroker>> {
    user_prompt_permission_broker_from_inner(broker, false)
}

pub fn workflow_user_prompt_permission_broker_from(
    broker: &Arc<dyn PermissionBroker>,
) -> Option<Arc<dyn PermissionBroker>> {
    user_prompt_permission_broker_from_inner(broker, true)
}

/// Build the sensitive-delegation broker for a sub-agent from the
/// parent turn's broker chain. Digs out the interactive prompt broker
/// like the unwrap helpers above, but keeps the parent's stored
/// permission rules in front of it: `allow` rules (settings allowlists,
/// interactive "always allow") resolve without prompting and `deny`
/// rules stay enforced inside sub-agents. Without this re-wrap a
/// sub-agent chain is `AutoApproveExceptSensitive(Channel)` and every
/// stored rule is silently skipped.
pub fn sub_agent_sensitive_permission_broker_from(
    broker: &Arc<dyn PermissionBroker>,
    unwrap_sub_agent_auto_broker: bool,
) -> Option<Arc<dyn PermissionBroker>> {
    let base = user_prompt_permission_broker_from_inner(broker, unwrap_sub_agent_auto_broker)?;
    match find_rules_policy_store(broker) {
        Some(store) => Some(Arc::new(crate::policy::RulesBasedPermissionBroker::new(
            store, base,
        ))),
        None => Some(base),
    }
}

fn find_rules_policy_store(
    broker: &Arc<dyn PermissionBroker>,
) -> Option<crate::policy::PolicyStore> {
    if let Some(rules) = broker
        .as_any()
        .downcast_ref::<crate::policy::RulesBasedPermissionBroker>()
    {
        return Some(rules.store().clone());
    }
    if let Some(hooked) = broker
        .as_any()
        .downcast_ref::<crate::hooks::HookedPermissionBroker>()
    {
        return find_rules_policy_store(hooked.inner());
    }
    if let Some(auto_broker) = broker
        .as_any()
        .downcast_ref::<rebon_tool::AutoApproveExceptSensitivePermissionBroker>()
    {
        return find_rules_policy_store(auto_broker.delegate());
    }
    None
}

fn user_prompt_permission_broker_from_inner(
    broker: &Arc<dyn PermissionBroker>,
    unwrap_sub_agent_auto_broker: bool,
) -> Option<Arc<dyn PermissionBroker>> {
    if broker.as_any().is::<ChannelPermissionBroker>()
        || broker.as_any().is::<crate::AcpPermissionBroker>()
    {
        return Some(broker.clone());
    }
    if unwrap_sub_agent_auto_broker {
        if let Some(auto_broker) = broker
            .as_any()
            .downcast_ref::<rebon_tool::AutoApproveExceptSensitivePermissionBroker>(
        ) {
            return user_prompt_permission_broker_from_inner(
                auto_broker.delegate(),
                unwrap_sub_agent_auto_broker,
            );
        }
    }
    if let Some(hooked_broker) = broker
        .as_any()
        .downcast_ref::<crate::hooks::HookedPermissionBroker>()
    {
        return user_prompt_permission_broker_from_inner(
            hooked_broker.inner(),
            unwrap_sub_agent_auto_broker,
        );
    }
    if let Some(rules_broker) = broker
        .as_any()
        .downcast_ref::<crate::policy::RulesBasedPermissionBroker>()
    {
        return user_prompt_permission_broker_from_inner(
            rules_broker.delegate(),
            unwrap_sub_agent_auto_broker,
        );
    }
    None
}

// ── Helpers ──────────────────────────────────────────────────────

fn requires_permission_broker_response(tool_name: &str) -> bool {
    matches!(tool_name, "AskUserQuestion" | "Agent")
}

/// The pre-run workflow review — a plan-approval step whose answer can carry
/// revision feedback. Every mode surfaces it except `bypassPermissions`,
/// whose "no gates" contract wins (see [`resolve_ask_under_mode`]).
fn requires_workflow_review(tool_name: &str) -> bool {
    matches!(tool_name, "Workflow" | "RunWorkflow")
}

/// The tools `acceptEdits` auto-approves — the closed set behind "File edits
/// are accepted; other operations follow policy".
///
/// Membership is by built-in tool identity, never by matching substrings of a
/// name: an MCP or plugin tool is free to call itself `EditDatabase`, and a
/// mode that silently swallowed its approval prompt would be a permission hole
/// named after a naming convention.
///
/// Kept deliberately narrow: exactly the tools whose entire purpose is to
/// write a file the user named — the ones that declare
/// [`ToolKind::FileEdit`]. The rule matcher folds the same class under a
/// single `Edit(...)` rule, so the two definitions of "file-edit permission
/// class" agree by construction rather than by two lists being kept in step.
/// Anything that merely *touches* a file as a side effect (`Bash`, `Agent`,
/// the MCP proxies) declares another kind and stays out; `SaveMemory` and
/// `TodoWrite` never reach here because they resolve to `Allow` on their own.
fn is_accept_edits_tool(tool_name: &str) -> bool {
    rebon_tool::tool_kind_for_name(tool_name) == ToolKind::FileEdit
}

/// A file edit aimed at a repository's git metadata — `.git/config`, a hook,
/// a worktree's `.git` pointer.
///
/// Git runs what that metadata names (`core.fsmonitor`, `diff.external`,
/// filter and textconv drivers, hooks) from commands that only read, and a
/// read-only `git status` runs without a prompt. So a mode that accepts file
/// edits on the user's behalf — `acceptEdits`, or auto mode's classifier —
/// would turn "edit a file" into "run a program" with nobody asked. Such an
/// edit always reaches the prompt instead; `bypassPermissions` keeps its
/// contract and runs it. A file edit with no target to read is left to the
/// tool, whose validation refuses it before any of this is asked.
fn edits_git_metadata(tool_name: &str, input: &Value, context: &ToolContext) -> bool {
    if !is_accept_edits_tool(tool_name) {
        return false;
    }
    let Some(target) = rebon_tool::file_target_field_for_name(tool_name)
        .and_then(|field| input.get(field))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let target = std::path::Path::new(target);
    let target = match context.cwd() {
        Some(cwd) if target.is_relative() => std::path::Path::new(cwd).join(target),
        _ => target.to_path_buf(),
    };
    rebon_tool::path_scope::is_git_metadata_path(&target)
}

fn append_permission_extra_text(value: &mut Value, extra_text: &Option<String>) {
    let Some(extra_text) = extra_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        return;
    };

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "permissionExtraText".into(),
            serde_json::Value::String(extra_text.to_string()),
        );
    }
}

/// Why a call the user turned down was denied.
///
/// `note` is whatever the `permission-rules` seat had to say about rejecting
/// this particular tool — a feature knows what its own rejection means for
/// the model's next move. Without one the generic wording stands.
pub(crate) fn permission_denial_reason(
    option_id: &str,
    extra_text: Option<&str>,
    note: Option<&RejectionNote>,
) -> String {
    let extra_text = extra_text.map(str::trim).filter(|text| !text.is_empty());
    if option_id == "reject_once" {
        let mut reason = "The user chose \"No, chat with this\".".to_owned();
        if let Some(note) = note {
            reason.push(' ');
            reason.push_str(&note.guidance);
            if let Some(extra_text) = extra_text {
                reason.push(' ');
                reason.push_str(&note.feedback_label);
                reason.push_str(": ");
                reason.push_str(extra_text);
            }
        } else {
            reason.push_str(
                " Discuss the requested action with the user instead of retrying the tool immediately.",
            );
            if let Some(extra_text) = extra_text {
                reason.push_str(" User note: ");
                reason.push_str(extra_text);
            }
        }
        return reason;
    }

    let mut reason = format!("permission denied (option: {option_id})");
    if let Some(extra_text) = extra_text {
        reason.push_str(". User note: ");
        reason.push_str(extra_text);
    }
    reason
}

/// Classify an option id into its kind. The one classifier: the ACP path
/// in `lib.rs` calls this too, so a new option id cannot mean "allow" on
/// one path and "reject" on the other.
pub(crate) fn option_kind(option_id: &str) -> PermissionOptionKind {
    match option_id {
        "allow_once"
        | "yes_clear_context_auto"
        | "yes_auto"
        | "yes_accept_edits"
        | "yes_default" => PermissionOptionKind::AllowOnce,
        "allow_always" => PermissionOptionKind::AllowAlways,
        "reject_always" | "deny_always" => PermissionOptionKind::RejectAlways,
        _ => PermissionOptionKind::RejectOnce,
    }
}

/// The label an option carries in the dialog.
///
/// The generic ids are the engine's. Anything else is offered by a feature,
/// so the `permission-rules` seat gets first refusal before the fallback
/// prettifier turns `some_id` into `some id`.
fn option_label(option_id: &str, rules: &PermissionRules) -> String {
    if let Some(label) = rules.option_label(option_id) {
        return label;
    }
    match option_id {
        "allow_once" => "Allow once".into(),
        "allow_always" => "Allow always".into(),
        "reject_once" => "No, chat with this".into(),
        other => other.replace('_', " "),
    }
}

fn build_options(
    request: PermissionRequest,
    rules: &PermissionRules,
) -> Vec<PermissionQueryOption> {
    if request.options.is_empty() {
        return vec![
            PermissionQueryOption {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionQueryOption {
                option_id: "reject_once".into(),
                label: "Reject once".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ];
    }

    request
        .options
        .into_iter()
        .map(|id| PermissionQueryOption {
            label: option_label(&id, rules),
            kind: option_kind(&id),
            option_id: id,
        })
        .collect()
}

fn is_reject_option(option_id: &str) -> bool {
    matches!(
        option_kind(option_id),
        PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
    )
}

fn is_reject_always_option(option_id: &str) -> bool {
    matches!(option_kind(option_id), PermissionOptionKind::RejectAlways)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_mode_classifier::AutoModeClassifierStage;
    use rebon_plugin_escalation::AskUserQuestionTool;
    use rebon_tool::EchoTool;
    use rebon_tools_core::{ToolId, ToolInputSchema};
    use serde_json::json;
    use tempfile::tempdir;

    struct WorkflowReviewTool;
    struct ContextRootsTool;
    struct BashEchoTool;
    struct PowerShellEchoTool;

    /// Echoes its input under a caller-chosen tool id, so a test can drive the
    /// broker as `Edit` / `Write` without pulling in the real tools' input
    /// validation and filesystem side effects.
    struct NamedEchoTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedEchoTool {
        fn id(&self) -> ToolId {
            ToolId::new(self.0)
        }

        fn description(&self) -> &str {
            "Returns its input without touching the filesystem."
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(input)
        }
    }

    #[async_trait::async_trait]
    impl Tool for BashEchoTool {
        fn id(&self) -> ToolId {
            ToolId::new("Bash")
        }

        fn description(&self) -> &str {
            "Returns Bash input without executing it."
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(input)
        }
    }

    #[async_trait::async_trait]
    impl Tool for PowerShellEchoTool {
        fn id(&self) -> ToolId {
            ToolId::new("PowerShell")
        }

        fn description(&self) -> &str {
            "Returns PowerShell input without executing it."
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(input)
        }
    }

    #[async_trait::async_trait]
    impl Tool for ContextRootsTool {
        fn id(&self) -> ToolId {
            ToolId::new("Agent")
        }

        fn description(&self) -> &str {
            "Agent context roots test tool."
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }

        async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
            Ok(json!(context.additional_working_directories()))
        }
    }

    #[async_trait::async_trait]
    impl Tool for WorkflowReviewTool {
        fn id(&self) -> ToolId {
            ToolId::new("Workflow")
        }

        fn description(&self) -> &str {
            "Workflow review test tool."
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }

        async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
            panic!("workflow revision feedback must not run the original tool")
        }
    }

    /// A stand-in for a feature's rule.
    ///
    /// The engine deliberately does **not** drive the shipped plan-mode rule
    /// here: `rebon-plugin-plan-mode` is a dev-dependency, so the copy of
    /// `rebon-core` it was compiled against is a different crate instance
    /// and its `PermissionRule` is a different trait. What these tests own is
    /// the *seam* — that a claim on the seat forces an ask, that an approved
    /// option's rewrite reaches the tool, that a rejection note reaches the
    /// model. What the plan-mode pair specifically claims, and what each of
    /// its options writes, is asserted in that plugin against the real rule.
    struct SeamRule;

    impl crate::permission_seat::PermissionRule for SeamRule {
        fn requires_user_decision(
            &self,
            tool_name: &str,
            _context: &ToolContext,
        ) -> Option<DecisionScope> {
            (tool_name == "SeamTool").then_some(DecisionScope::WhenPrompting)
        }

        fn option_label(&self, option_id: &str) -> Option<String> {
            (option_id == "seam_yes").then(|| "Yes, the feature's words".to_string())
        }

        fn on_approved(
            &self,
            tool_name: &str,
            option_id: &str,
            input: &mut Value,
            context: &ToolContext,
        ) -> Option<ToolContext> {
            // Keyed on the option alone, and on a *generic* allow id: option
            // kind is still the engine's table, so an id it has never heard
            // of classifies as a rejection and never reaches an approval.
            // Whether a rule declines another feature's tool is that rule's
            // own test, not this seam's.
            let _ = tool_name;
            if option_id != "allow_once" {
                return None;
            }
            if let Some(object) = input.as_object_mut() {
                object.insert("seam".into(), Value::String("applied".into()));
            }
            Some(context.clone())
        }

        fn rejection_note(&self, tool_name: &str) -> Option<RejectionNote> {
            (tool_name == "SeamTool").then(|| RejectionNote {
                guidance: "Seam guidance.".into(),
                feedback_label: "Seam label".into(),
            })
        }
    }

    fn seam_rules() -> (Arc<rebon_kernel::Kernel>, PermissionRules) {
        let kernel = rebon_kernel::Kernel::new();
        let seat = crate::permission_seat::PermissionRuleSeat::new();
        kernel
            .context()
            .provide::<crate::permission_seat::PermissionRuleSeatService>(seat.clone())
            .expect("the seat goes on the root");
        seat.register(kernel.context(), "seam", Arc::new(SeamRule))
            .expect("the rule goes on the seat");
        let rules = seat.rules();
        (kernel, rules)
    }

    /// A rule on the seat renames an option the engine has never heard of;
    /// the generic ids keep the engine's own words.
    #[test]
    fn a_rule_labels_the_options_its_own_feature_offers() {
        let (_kernel, rules) = seam_rules();
        assert_eq!(option_label("seam_yes", &rules), "Yes, the feature's words");
        assert_eq!(option_label("allow_once", &rules), "Allow once");
        assert_eq!(option_label("unknown_id", &rules), "unknown id");
    }

    /// Auto mode hands nothing the seat claimed to the classifier.
    #[tokio::test]
    async fn auto_mode_never_resolves_a_claimed_call_itself() {
        let (_kernel, rules) = seam_rules();
        let outcome = resolve_ask_under_mode(
            PermissionMode::Auto,
            "SeamTool",
            &json!({}),
            &ToolContext::new(),
            None,
            None,
            &rules,
        )
        .await;
        assert!(matches!(outcome, ModeAskOutcome::Ask));
    }

    /// An unclaimed call is judged as before — the seat only ever adds a
    /// prompt, so an empty claim must not change the answer.
    #[tokio::test]
    async fn an_unclaimed_call_is_left_to_the_mode() {
        let (_kernel, rules) = seam_rules();
        let outcome = resolve_ask_under_mode(
            PermissionMode::BypassPermissions,
            "SeamTool",
            &json!({}),
            &ToolContext::new(),
            None,
            None,
            &rules,
        )
        .await;
        assert!(
            matches!(outcome, ModeAskOutcome::Run(_)),
            "a WhenPrompting claim does not reach bypass"
        );
    }

    /// `dontAsk` refuses a claimed call along with everything else that would
    /// prompt — the seat adds a reason to prompt, never an exemption from the
    /// mode that refuses to.
    #[tokio::test]
    async fn dont_ask_still_refuses_a_claimed_call() {
        let (_kernel, rules) = seam_rules();
        let outcome = resolve_ask_under_mode(
            PermissionMode::DontAsk,
            "SeamTool",
            &json!({}),
            &ToolContext::new(),
            None,
            None,
            &rules,
        )
        .await;
        assert!(matches!(outcome, ModeAskOutcome::Deny(_)));
    }

    /// `dontAsk` refuses a claimed call either way; a rule with its own
    /// wording gets to say why, and an unclaimed tool keeps the generic
    /// sentence.
    #[tokio::test]
    async fn a_rules_own_wording_replaces_the_generic_dont_ask_refusal() {
        struct OwnWords;
        impl crate::permission_seat::PermissionRule for OwnWords {
            fn requires_user_decision(
                &self,
                tool_name: &str,
                _context: &ToolContext,
            ) -> Option<DecisionScope> {
                (tool_name == "SeamTool").then_some(DecisionScope::EvenUnderBypass)
            }

            fn deny_reason_when_never_prompting(&self, tool_name: &str) -> Option<String> {
                (tool_name == "SeamTool").then(|| "The feature's own refusal.".to_string())
            }
        }
        let kernel = rebon_kernel::Kernel::new();
        let seat = crate::permission_seat::PermissionRuleSeat::new();
        seat.register(kernel.context(), "own-words", Arc::new(OwnWords))
            .unwrap();
        let rules = seat.rules();

        let outcome = resolve_ask_under_mode(
            PermissionMode::DontAsk,
            "SeamTool",
            &json!({}),
            &ToolContext::new(),
            None,
            None,
            &rules,
        )
        .await;
        let ModeAskOutcome::Deny(reason) = outcome else {
            panic!("dontAsk refuses a claimed call");
        };
        assert_eq!(reason, "The feature's own refusal.");

        let outcome = resolve_ask_under_mode(
            PermissionMode::DontAsk,
            "Bash",
            &json!({}),
            &ToolContext::new(),
            None,
            None,
            &rules,
        )
        .await;
        let ModeAskOutcome::Deny(reason) = outcome else {
            panic!("dontAsk refuses everything that would prompt");
        };
        assert_eq!(reason, dont_ask_deny_reason("Bash"));
    }

    /// The stronger claim is the one that reaches into `bypassPermissions`.
    #[tokio::test]
    async fn an_even_under_bypass_claim_reaches_bypass() {
        struct Checkpoint;
        impl crate::permission_seat::PermissionRule for Checkpoint {
            fn requires_user_decision(
                &self,
                tool_name: &str,
                _context: &ToolContext,
            ) -> Option<DecisionScope> {
                (tool_name == "SeamTool").then_some(DecisionScope::EvenUnderBypass)
            }
        }
        let kernel = rebon_kernel::Kernel::new();
        let seat = crate::permission_seat::PermissionRuleSeat::new();
        seat.register(kernel.context(), "checkpoint", Arc::new(Checkpoint))
            .unwrap();
        let rules = seat.rules();

        let outcome = resolve_ask_under_mode(
            PermissionMode::BypassPermissions,
            "SeamTool",
            &json!({}),
            &ToolContext::new(),
            None,
            None,
            &rules,
        )
        .await;
        assert!(matches!(outcome, ModeAskOutcome::Ask));
    }

    /// A rejection carries the feature's sentence and its own label for the
    /// user's words; an unclaimed tool keeps the generic wording.
    #[test]
    fn a_rejection_note_replaces_the_generic_wording() {
        let (_kernel, rules) = seam_rules();
        let claimed = permission_denial_reason(
            "reject_once",
            Some("try the other thing"),
            rules.rejection_note("SeamTool").as_ref(),
        );
        assert_eq!(
            claimed,
            "The user chose \"No, chat with this\". Seam guidance. Seam label: try the other thing"
        );

        let generic = permission_denial_reason(
            "reject_once",
            Some("try the other thing"),
            rules.rejection_note("Bash").as_ref(),
        );
        assert_eq!(
            generic,
            "The user chose \"No, chat with this\". Discuss the requested action with the user instead of retrying the tool immediately. User note: try the other thing"
        );
    }

    #[test]
    fn option_labels_use_sentence_case() {
        let none = PermissionRules::default();
        assert_eq!(option_label("allow_once", &none), "Allow once");
        assert_eq!(option_label("allow_always", &none), "Allow always");
        assert_eq!(option_label("reject_once", &none), "No, chat with this");
    }

    #[tokio::test]
    async fn kernel_waterfall_listener_answers_ask_before_ui() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-kernel-1");
        let kernel = rebon_kernel::Kernel::new();
        let session_ctx = kernel.context().fork_scoped("session");
        session_ctx.wrap_json("permission/ask", |query, _next| {
            assert_eq!(query["toolName"], "Echo");
            assert_eq!(query["title"], "Test");
            serde_json::json!({ "answer": { "optionId": "allow_once" } })
        });
        broker.set_kernel_context(session_ctx);

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t-k1");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            Some(json!({"x": 1})),
        );

        let output = broker
            .resolve(tool.as_ref(), json!({"x": 1}), &context, decision)
            .await
            .expect("kernel listener approved the ask");
        assert_eq!(output, json!({"x": 1}));
        assert!(
            rx.try_recv().is_err(),
            "an intercepted ask must never reach the UI channel"
        );
    }

    #[tokio::test]
    async fn kernel_waterfall_pass_through_reaches_ui() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-kernel-2");
        let kernel = rebon_kernel::Kernel::new();
        let session_ctx = kernel.context().fork_scoped("session");
        session_ctx.wrap_json("permission/ask", |query, next| next.call(query));
        broker.set_kernel_context(session_ctx);

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t-k2");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            Some(json!({"x": 2})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({"x": 2}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.expect("pass-through must reach the UI");
        assert_eq!(query.tool_name, "Echo");
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();
        handle.await.unwrap().expect("tool ran after UI approval");
    }

    /// A turn detached from the front end outlives the session it started
    /// in: the TUI can `/new` or resume underneath it and rebind its
    /// scope. That turn must keep asking on the scope it began with, or a
    /// session it never ran in decides its tool calls.
    #[tokio::test]
    async fn a_turn_keeps_the_scope_it_started_on_when_the_session_moves() {
        let (broker, _rx) = ChannelPermissionBroker::new("sess-before");
        let kernel = rebon_kernel::Kernel::new();
        broker.set_kernel_context(kernel.context().fork_scoped("session/before"));

        let detached = broker.for_session("sess-before");

        // The front end moves on: `/new` rebinds the shared broker.
        broker.set_kernel_context(kernel.context().fork_scoped("session/after"));

        let held = detached
            .kernel_ctx
            .lock()
            .expect("poisoned")
            .clone()
            .expect("the detached view kept a context");
        assert_eq!(
            held.context().label(),
            "session/before",
            "a detached turn must not be re-pointed at the session that replaced it"
        );
    }

    #[test]
    fn per_turn_resolver_acquires_once_and_keeps_lease_without_permission_calls() {
        struct DropProbe(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let (broker, _rx) = ChannelPermissionBroker::new("long-lived");
        let kernel = rebon_kernel::Kernel::new();
        let context = kernel.context().fork_scoped("session/exact");
        let acquisitions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        broker.set_kernel_context_resolver({
            let acquisitions = acquisitions.clone();
            let drops = drops.clone();
            Arc::new(move |session_id| {
                assert_eq!(session_id, "sess-exact");
                acquisitions.fetch_add(1, Ordering::SeqCst);
                Some(KernelContextLease::managed(
                    context.clone(),
                    DropProbe(drops.clone()),
                ))
            })
        });

        let turn = broker.for_session("sess-exact");
        assert_eq!(acquisitions.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // No permission call is made. The fixed per-turn broker itself keeps
        // the managed owner alive even after the long-lived broker is gone.
        drop(broker);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(turn);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// The waterfall is a decision plane, and a decision belongs to the
    /// session that asked. A middleware another session registered must
    /// never see — let alone answer — this session's query.
    #[tokio::test]
    async fn kernel_waterfall_ignores_another_sessions_middleware() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-kernel-scope");
        let kernel = rebon_kernel::Kernel::new();
        let compose = kernel.context().fork("compose");
        let other_session = compose.fork_scoped("session-b");
        other_session.wrap_json(
            "permission/ask",
            |_query, _next| serde_json::json!({ "answer": { "optionId": "allow_once" } }),
        );
        broker.set_kernel_context(compose.fork_scoped("session-a"));

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t-k-scope");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            Some(json!({"x": 9})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({"x": 9}), &context, decision)
                    .await
            }
        });

        // The sibling's approval must not apply: the ask reaches the UI.
        let query = rx
            .recv()
            .await
            .expect("the ask must reach this session's UI");
        assert_eq!(query.tool_name, "Echo");
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();
        handle.await.unwrap().expect("tool ran after UI approval");
    }

    #[tokio::test]
    async fn kernel_waterfall_cancel_denies_without_ui() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-kernel-3");
        let kernel = rebon_kernel::Kernel::new();
        let session_ctx = kernel.context().fork_scoped("session");
        session_ctx.wrap_json(
            "permission/ask",
            |_query, _next| serde_json::json!({ "cancel": true }),
        );
        broker.set_kernel_context(session_ctx);

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t-k3");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            None,
        );

        let err = broker
            .resolve(tool.as_ref(), json!({}), &context, decision)
            .await
            .expect_err("cancel must deny");
        assert!(matches!(err, ToolError::PermissionDenied { .. }), "{err}");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn broker_calls_tool_on_allow_option() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            Some(json!({"x": 1})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({"x": 1}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        assert_eq!(query.tool_name, "Echo");
        assert_eq!(query.title, "Test");
        assert_eq!(query.message, "Approve?");
        assert_eq!(query.options.len(), 2);
        assert_eq!(query.options[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(query.options[1].kind, PermissionOptionKind::RejectOnce);

        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn broker_applies_authorized_paths_after_approval() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-path");
        let tool = Arc::new(ContextRootsTool);
        let context = ToolContext::new()
            .with_tool_use_id("t-path")
            .with_additional_working_directories(["F:/existing"]);
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Authorize agent directory", "Approve F:/other?")
                .with_options(["allow_once", "reject_once"])
                .with_metadata(json!({
                    "kind": rebon_tool::AGENT_EXTERNAL_PATH_AUTHORIZATION_KIND,
                    "authorized_path_roots": ["F:/other"]
                })),
            Some(json!({})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();

        assert_eq!(
            handle.await.unwrap().unwrap(),
            json!(["F:/existing", "F:/other"])
        );
        assert_eq!(context.additional_working_directories(), ["F:/existing"]);
    }

    #[tokio::test]
    async fn broker_denies_on_reject_option() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            None,
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "reject_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();

        let error = handle.await.unwrap().unwrap_err();
        assert_eq!(
            error.to_string(),
            "permission denied for tool `Echo`: The user chose \"No, chat with this\". Discuss the requested action with the user instead of retrying the tool immediately."
        );
        assert!(matches!(error, ToolError::PermissionDenied { .. }));
    }

    #[tokio::test]
    async fn broker_returns_workflow_revision_request_on_reject_with_feedback() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(WorkflowReviewTool);
        let context = ToolContext::new().with_tool_use_id("workflow-1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Review workflow", "Approve?")
                .with_options(["allow_once", "reject_once"]),
            Some(json!({"script": "export const meta = { name: 'demo' };"})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({"script": "old"}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "reject_once".into(),
                updated_input: None,
                extra_text: Some(
                    "Please revise the workflow before running it. Split verification into its own phase"
                        .into(),
                ),
            })
            .unwrap();

        let result = handle
            .await
            .unwrap()
            .expect("workflow edit feedback should be model-visible output");
        assert_eq!(result["status"], "revision_requested");
        assert_eq!(result["action"], "regenerate_workflow");
        assert_eq!(
            result["feedback"],
            "Please revise the workflow before running it. Split verification into its own phase"
        );
    }

    #[tokio::test]
    async fn broker_denies_workflow_reject_always_even_with_revision_prefix() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(WorkflowReviewTool);
        let context = ToolContext::new().with_tool_use_id("workflow-1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Review workflow", "Approve?").with_options([
                "allow_once",
                "reject_once",
                "reject_always",
            ]),
            Some(json!({"script": "export const meta = { name: 'demo' };"})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({"script": "old"}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "reject_always".into(),
                updated_input: None,
                extra_text: Some(format!(
                    "{WORKFLOW_REVISION_FEEDBACK_PREFIX} Split verification into its own phase"
                )),
            })
            .unwrap();

        let result = handle.await.unwrap();
        match result.unwrap_err() {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(reason.contains("reject_always"));
            }
            other => panic!("expected PermissionDenied, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn broker_denies_workflow_reject_note_without_revision_prefix() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(WorkflowReviewTool);
        let context = ToolContext::new().with_tool_use_id("workflow-1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Review workflow", "Approve?")
                .with_options(["allow_once", "reject_once"]),
            Some(json!({"script": "export const meta = { name: 'demo' };"})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({"script": "old"}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "reject_once".into(),
                updated_input: None,
                extra_text: Some("no, stop running workflows".into()),
            })
            .unwrap();

        let result = handle.await.unwrap();
        match result.unwrap_err() {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(!reason.contains("reject_once"));
                assert!(reason.contains("No, chat with this"));
                assert!(reason.contains("no, stop running workflows"));
            }
            other => panic!("expected PermissionDenied, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn broker_denial_includes_extra_text() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            None,
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "reject_once".into(),
                updated_input: None,
                extra_text: Some("try a safer command".into()),
            })
            .unwrap();

        let result = handle.await.unwrap();
        match result.unwrap_err() {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(!reason.contains("reject_once"));
                assert!(reason.contains("No, chat with this"));
                assert!(reason.contains("try a safer command"));
            }
            other => panic!("expected PermissionDenied, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn broker_allow_injects_extra_text_into_input() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            None,
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        json!({"command": "npm test"}),
                        &context,
                        decision,
                    )
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: Some("only run unit tests".into()),
            })
            .unwrap();

        let result = handle.await.unwrap().expect("allow should call tool");
        assert_eq!(result["permissionExtraText"], "only run unit tests");
    }

    /// An approved option's rewrite reaches the tool the call runs. This is
    /// the whole of what the broker owes the seat; *which* fields a given
    /// feature writes is that plugin's test.
    #[tokio::test]
    async fn an_approved_option_rewrites_the_input_the_tool_receives() {
        let kernel = rebon_kernel::Kernel::new();
        let seat = crate::permission_seat::PermissionRuleSeat::new();
        kernel
            .context()
            .provide::<crate::permission_seat::PermissionRuleSeatService>(seat.clone())
            .unwrap();
        seat.register(kernel.context(), "seam", Arc::new(SeamRule))
            .unwrap();
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-seam");
        broker.set_kernel_context(kernel.context().clone());

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("seam-1");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Seam", "Review").with_options(["allow_once", "reject_once"]),
            Some(json!({"value": 1})),
        );
        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({"value": 1}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();

        let output = handle.await.unwrap().unwrap();
        assert_eq!(output["seam"], "applied");
    }

    #[tokio::test]
    async fn broker_injects_allow_once_for_add_dir_file_prompt() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared");
        let src = shared.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file_path = src.join("lib.rs");
        std::fs::write(&file_path, "").unwrap();
        let context = ToolContext::new()
            .with_tool_use_id("t1")
            .with_additional_working_directories([shared.to_string_lossy().to_string()]);

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Write file", "Write wants to create or overwrite a file")
                .with_options(["allow_always", "reject_once"]),
            Some(json!({"file_path": file_path.to_string_lossy()})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            let file_path = file_path.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        json!({"file_path": file_path.to_string_lossy()}),
                        &context,
                        decision,
                    )
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        assert_eq!(
            query
                .options
                .iter()
                .map(|option| option.option_id.as_str())
                .collect::<Vec<_>>(),
            vec!["allow_once", "allow_always", "reject_once"]
        );
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();

        let result = handle.await.unwrap().expect("allow should call tool");
        assert_eq!(result["file_path"], file_path.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn broker_does_not_inject_allow_once_for_nonexistent_add_dir_path() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let missing_path = shared.join("missing.rs");
        let context = ToolContext::new()
            .with_tool_use_id("t1")
            .with_additional_working_directories([shared.to_string_lossy().to_string()]);

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Write file", "Write wants to create or overwrite a file")
                .with_options(["allow_always", "reject_once"]),
            Some(json!({"file_path": missing_path.to_string_lossy()})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            let missing_path = missing_path.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        json!({"file_path": missing_path.to_string_lossy()}),
                        &context,
                        decision,
                    )
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        assert_eq!(
            query
                .options
                .iter()
                .map(|option| option.option_id.as_str())
                .collect::<Vec<_>>(),
            vec!["allow_always", "reject_once"]
        );
        query.response_tx.send(PermissionAnswer::Cancelled).unwrap();
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn broker_does_not_inject_allow_once_outside_add_dir() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared");
        let other = dir.path().join("other").join("src");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let file_path = other.join("lib.rs");
        std::fs::write(&file_path, "").unwrap();
        let context = ToolContext::new()
            .with_tool_use_id("t1")
            .with_additional_working_directories([shared.to_string_lossy().to_string()]);

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Write file", "Write wants to create or overwrite a file")
                .with_options(["allow_always", "reject_once"]),
            Some(json!({"file_path": file_path.to_string_lossy()})),
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            let file_path = file_path.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        json!({"file_path": file_path.to_string_lossy()}),
                        &context,
                        decision,
                    )
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        assert_eq!(
            query
                .options
                .iter()
                .map(|option| option.option_id.as_str())
                .collect::<Vec<_>>(),
            vec!["allow_always", "reject_once"]
        );
        query.response_tx.send(PermissionAnswer::Cancelled).unwrap();
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn broker_denies_on_cancel() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-1");
        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t1");

        let decision = PermissionDecision::ask(PermissionRequest::new("Test", "Approve?"), None);

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.unwrap();
        query.response_tx.send(PermissionAnswer::Cancelled).unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_err());
    }

    // ── Auto-mode integration ─────────────────────────────────────

    use rebon_permissions::{
        auto_mode_denials::AutoModeDenialStore,
        denial_sink::{AutoModeHooks, PermissionModeProvider, SharedDenialSink},
        types::PermissionMode,
    };
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    fn hooks_with_mode(mode: PermissionMode) -> (AutoModeHooks, SharedDenialSink) {
        let store = Arc::new(StdMutex::new(AutoModeDenialStore::default()));
        let sink = SharedDenialSink::new(Arc::clone(&store));
        let provider: Arc<dyn PermissionModeProvider> = Arc::new(move || mode);
        let hooks = AutoModeHooks::new(Arc::new(sink.clone()), provider);
        (hooks, sink)
    }

    fn hooks_with_mode_cell(
        mode: PermissionMode,
    ) -> (
        AutoModeHooks,
        SharedDenialSink,
        Arc<StdMutex<PermissionMode>>,
    ) {
        let store = Arc::new(StdMutex::new(AutoModeDenialStore::default()));
        let sink = SharedDenialSink::new(Arc::clone(&store));
        let mode_cell = Arc::new(StdMutex::new(mode));
        let provider_cell = Arc::clone(&mode_cell);
        let provider: Arc<dyn PermissionModeProvider> =
            Arc::new(move || *provider_cell.lock().expect("mode cell poisoned"));
        let hooks = AutoModeHooks::new(Arc::new(sink.clone()), provider);
        (hooks, sink, mode_cell)
    }

    #[derive(Debug, Clone)]
    enum FixedClassifierResult {
        Outcome(AutoModeClassifierOutcome),
        Error,
        /// A real typed parse failure, as a classifier model that will
        /// not hold to the grammar produces on every call.
        ParseError,
    }

    #[derive(Debug)]
    struct RecordingClassifier {
        result: FixedClassifierResult,
        calls: AtomicUsize,
        requests: StdMutex<Vec<AutoModeClassifierRequest>>,
    }

    impl RecordingClassifier {
        fn new(result: FixedClassifierResult) -> Self {
            Self {
                result,
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl AutoModeClassifier for RecordingClassifier {
        async fn classify(
            &self,
            request: AutoModeClassifierRequest,
        ) -> anyhow::Result<AutoModeClassifierOutcome> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.requests
                .lock()
                .expect("requests poisoned")
                .push(request);
            match &self.result {
                FixedClassifierResult::Outcome(outcome) => Ok(outcome.clone()),
                FixedClassifierResult::Error => Err(anyhow::anyhow!("classifier failed")),
                FixedClassifierResult::ParseError => {
                    Err(anyhow::Error::new(AutoModeClassifierError::parsing(
                        crate::auto_mode_classifier::AutoModeClassifierStage::Fast,
                        "chatty-model",
                        "<block>no</block>\nThis only reads repository state.",
                        "unexpected content after </block>",
                    )))
                }
            }
        }
    }

    struct BlockingAllowClassifier {
        started: Notify,
        release: Notify,
    }

    impl std::fmt::Debug for BlockingAllowClassifier {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("BlockingAllowClassifier")
        }
    }

    #[async_trait::async_trait]
    impl AutoModeClassifier for BlockingAllowClassifier {
        async fn classify(
            &self,
            _request: AutoModeClassifierRequest,
        ) -> anyhow::Result<AutoModeClassifierOutcome> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(AutoModeClassifierOutcome::Allow {
                reason: "read-only".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            })
        }
    }

    struct PendingClassifier {
        started: Notify,
    }

    impl std::fmt::Debug for PendingClassifier {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("PendingClassifier")
        }
    }

    #[async_trait::async_trait]
    impl AutoModeClassifier for PendingClassifier {
        async fn classify(
            &self,
            _request: AutoModeClassifierRequest,
        ) -> anyhow::Result<AutoModeClassifierOutcome> {
            self.started.notify_one();
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    fn embedded_python_input(source: &str) -> Value {
        json!({
            "command": format!("python - <<'PY'\n{source}\nPY")
        })
    }

    fn edit_ask_decision(input: &Value) -> PermissionDecision {
        PermissionDecision::ask(
            PermissionRequest::new("Edit file", "Edit wants to modify: src/lib.rs")
                .with_options(["allow_once", "reject_once"]),
            Some(input.clone()),
        )
    }

    fn bash_ask_decision(input: Value) -> PermissionDecision {
        PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run a command")
                .with_options(["allow_once", "reject_once"]),
            Some(input),
        )
    }

    #[test]
    fn user_prompt_broker_unwraps_rules_based_policy_layer() {
        let (channel, _rx) = ChannelPermissionBroker::new("sess-parent");
        let inner = Arc::new(channel) as Arc<dyn PermissionBroker>;
        let wrapped = Arc::new(crate::policy::RulesBasedPermissionBroker::new(
            crate::policy::PolicyStore::new(),
            inner,
        )) as Arc<dyn PermissionBroker>;

        let prompt_broker = user_prompt_permission_broker_from(&wrapped)
            .expect("rules layer should unwrap to prompt broker");
        assert!(prompt_broker.as_any().is::<ChannelPermissionBroker>());
    }

    #[test]
    fn workflow_user_prompt_broker_unwraps_sub_agent_auto_approval_layer() {
        let (channel, _rx) = ChannelPermissionBroker::new("sess-parent");
        let inner = Arc::new(channel) as Arc<dyn PermissionBroker>;
        let wrapped = Arc::new(rebon_tool::AutoApproveExceptSensitivePermissionBroker::new(
            inner,
        )) as Arc<dyn PermissionBroker>;

        assert!(user_prompt_permission_broker_from(&wrapped).is_none());
        let prompt_broker = workflow_user_prompt_permission_broker_from(&wrapped)
            .expect("workflow lineage should unwrap the sub-agent auto approval layer");
        assert!(prompt_broker.as_any().is::<ChannelPermissionBroker>());
    }

    #[test]
    fn sub_agent_sensitive_broker_keeps_parent_rules_layer() {
        let (channel, _rx) = ChannelPermissionBroker::new("sess-parent");
        let store = crate::policy::PolicyStore::new();
        let rules = Arc::new(crate::policy::RulesBasedPermissionBroker::new(
            store,
            Arc::new(channel) as Arc<dyn PermissionBroker>,
        )) as Arc<dyn PermissionBroker>;

        let sensitive = sub_agent_sensitive_permission_broker_from(&rules, false)
            .expect("rules layer should unwrap to a prompt broker");
        let rebuilt = sensitive
            .as_any()
            .downcast_ref::<crate::policy::RulesBasedPermissionBroker>()
            .expect("sub-agent sensitive broker must keep the rules layer");
        assert!(rebuilt.delegate().as_any().is::<ChannelPermissionBroker>());
    }

    #[test]
    fn sub_agent_sensitive_broker_finds_rules_behind_auto_approval_layer() {
        let (channel, _rx) = ChannelPermissionBroker::new("sess-parent");
        let store = crate::policy::PolicyStore::new();
        let rules = Arc::new(crate::policy::RulesBasedPermissionBroker::new(
            store,
            Arc::new(channel) as Arc<dyn PermissionBroker>,
        )) as Arc<dyn PermissionBroker>;
        let wrapped = Arc::new(rebon_tool::AutoApproveExceptSensitivePermissionBroker::new(
            rules,
        )) as Arc<dyn PermissionBroker>;

        let sensitive = sub_agent_sensitive_permission_broker_from(&wrapped, true)
            .expect("workflow lineage should unwrap to a prompt broker");
        let rebuilt = sensitive
            .as_any()
            .downcast_ref::<crate::policy::RulesBasedPermissionBroker>()
            .expect("sub-agent sensitive broker must keep the rules layer");
        assert!(rebuilt.delegate().as_any().is::<ChannelPermissionBroker>());
    }

    #[test]
    fn sub_agent_sensitive_broker_without_rules_returns_prompt_broker() {
        let (channel, _rx) = ChannelPermissionBroker::new("sess-parent");
        let inner = Arc::new(channel) as Arc<dyn PermissionBroker>;

        let sensitive = sub_agent_sensitive_permission_broker_from(&inner, false)
            .expect("bare channel broker should pass through");
        assert!(sensitive.as_any().is::<ChannelPermissionBroker>());
    }

    // ── bypassPermissions ─────────────────────────────────────────

    /// The mode's whole contract: an `Ask` runs instead of prompting. It
    /// takes no classifier, so a command auto mode would stop and hand to
    /// the classifier still runs here.
    #[tokio::test]
    async fn bypass_mode_runs_ask_decisions_without_a_dialog() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-bypass");
        let (hooks, sink) = hooks_with_mode(PermissionMode::BypassPermissions);
        broker.set_auto_mode_hooks(Some(hooks));
        // Installed but never consulted: bypass judges nothing.
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "would deny".to_owned(),
                category: None,
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let context = ToolContext::new().with_tool_use_id("t-bypass");
        let input = json!({ "command": "rm -rf ./build" });
        // Bounded: without the bypass branch this falls through to the dialog
        // and waits on an answer nobody sends, which would hang the suite
        // instead of reporting the regression.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            broker.resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            ),
        )
        .await
        .expect("bypass mode must not wait on an approval dialog")
        .expect("bypass mode should run the tool");

        assert_eq!(result, input);
        assert!(rx.try_recv().is_err(), "bypass must not emit a dialog");
        assert_eq!(classifier.call_count(), 0);
        assert_eq!(sink.store().lock().unwrap().len(), 0);
    }

    /// `AskUserQuestion` and `Agent` read their answer out of the broker
    /// response, so bypassing the dialog would erase the data path — not
    /// just the approval. Same carve-out auto mode makes.
    #[tokio::test]
    async fn bypass_mode_still_prompts_for_tools_that_need_the_response() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-bypass");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::BypassPermissions);
        broker.set_auto_mode_hooks(Some(hooks));

        let tool = Arc::new(AskUserQuestionTool);
        let context = ToolContext::new().with_tool_use_id("t-question");
        let input = json!({
            "questions": [{
                "question": "Continue?",
                "header": "Continue",
                "options": [
                    { "label": "Yes", "description": "Continue" },
                    { "label": "No", "description": "Stop" }
                ]
            }]
        });
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        let call_input = input.clone();

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), call_input, &context, decision)
                    .await
            }
        });

        let query = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("AskUserQuestion should still emit a dialog under bypass")
            .expect("permission receiver should stay open");
        assert_eq!(query.tool_name, "AskUserQuestion");

        let mut updated_input = input;
        updated_input
            .as_object_mut()
            .unwrap()
            .insert("answers".into(), json!({ "Continue?": "Yes" }));
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: Some(updated_input),
                extra_text: None,
            })
            .unwrap();

        let result = handle.await.unwrap().expect("answered question should run");
        assert_eq!(result["answers"]["Continue?"], "Yes");
    }

    /// The mode provider is read per dispatch, not captured at session build,
    /// so switching a live session into (and back out of) bypass takes effect
    /// on the very next tool call. This is what makes the desktop composer's
    /// mid-session mode switch and the TUI settings dialog work without a
    /// restart — both mutate the shared mode cell this provider reads.
    #[tokio::test]
    async fn bypass_toggles_mid_session_without_rebuilding_the_broker() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-flip");
        let (hooks, _sink, mode_cell) = hooks_with_mode_cell(PermissionMode::BypassPermissions);
        broker.set_auto_mode_hooks(Some(hooks));

        let context = ToolContext::new().with_tool_use_id("t-flip");
        let input = json!({ "command": "cargo build" });

        // In bypass: runs straight through.
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            broker.resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            ),
        )
        .await
        .expect("bypass must not wait on a dialog")
        .expect("bypass should run the tool");
        assert!(rx.try_recv().is_err());

        // Switched back to default mid-session: the same call now prompts.
        *mode_cell.lock().expect("mode cell poisoned") = PermissionMode::Default;
        let pending = tokio::spawn({
            let input = input.clone();
            async move {
                broker
                    .resolve(
                        &BashEchoTool,
                        input.clone(),
                        &context,
                        bash_ask_decision(input),
                    )
                    .await
            }
        });
        let query = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("leaving bypass must restore the approval dialog")
            .expect("permission receiver should stay open");
        assert_eq!(query.tool_name, "Bash");
        drop(query.response_tx);
        assert!(pending.await.unwrap().is_err());
    }

    /// Bypass skips *prompts*, not deny rules: a `Deny` decision — which is
    /// what a stored deny rule or a hook produces — still fails the call.
    #[tokio::test]
    async fn bypass_mode_still_honors_deny_decisions() {
        let (broker, _rx) = ChannelPermissionBroker::new("sess-bypass");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::BypassPermissions);
        broker.set_auto_mode_hooks(Some(hooks));

        let context = ToolContext::new().with_tool_use_id("t-denied");
        let decision = PermissionDecision::deny("denied by stored rule: Bash(rm:*)");
        let error = broker
            .resolve(
                &BashEchoTool,
                json!({ "command": "rm -rf /" }),
                &context,
                decision,
            )
            .await
            .expect_err("deny must survive bypass mode");

        assert!(matches!(error, ToolError::PermissionDenied { .. }));
    }

    // ── the plan-mode pair ────────────────────────────────────────

    // ── acceptEdits ───────────────────────────────────────────────

    /// The closed set is the entire safety story for this mode, so pin its
    /// membership rather than only its effects.
    #[test]
    fn accept_edits_closed_set_is_the_file_edit_tools_only() {
        assert!(is_accept_edits_tool("Edit"));
        assert!(is_accept_edits_tool("Write"));
        assert!(is_accept_edits_tool("MultiEdit"));
        assert!(is_accept_edits_tool("NotebookEdit"));
        for tool in [
            "Bash",
            "PowerShell",
            "Agent",
            "AskUserQuestion",
            "ExitPlanMode",
            "WebFetch",
            "mcp__server__edit_file",
            // Naming is not identity: a plugin tool cannot opt itself in.
            "EditDatabase",
            "edit",
            "MultiEditDatabase",
            "multiedit",
        ] {
            assert!(!is_accept_edits_tool(tool), "tool={tool}");
        }
    }

    #[tokio::test]
    async fn accept_edits_runs_file_edits_without_a_dialog() {
        for tool_name in ["Edit", "Write", "MultiEdit", "NotebookEdit"] {
            let (broker, mut rx) = ChannelPermissionBroker::new("sess-accept");
            let (hooks, _sink) = hooks_with_mode(PermissionMode::AcceptEdits);
            broker.set_auto_mode_hooks(Some(hooks));

            let tool = NamedEchoTool(tool_name);
            let context = ToolContext::new().with_tool_use_id("t-edit");
            let input = json!({ "file_path": "src/main.rs", "content": "fn main() {}" });
            let decision = PermissionDecision::ask(
                PermissionRequest::new("Edit file", "Edit wants to modify: src/main.rs")
                    .with_options(["allow_once", "reject_once"]),
                Some(input.clone()),
            );

            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                broker.resolve(&tool, input.clone(), &context, decision),
            )
            .await
            .expect("acceptEdits must not wait on a dialog")
            .expect("acceptEdits should run the edit");

            assert_eq!(result, input);
            assert!(rx.try_recv().is_err(), "tool={tool_name}");
        }
    }

    /// A file edit into git metadata is a way to run a program: git runs
    /// what `.git/config` and the hooks name from a `git status` that no
    /// longer asks. `acceptEdits` does not accept it on the user's behalf.
    #[tokio::test]
    async fn accept_edits_prompts_for_an_edit_into_git_metadata() {
        for (tool_name, field, target) in [
            ("Write", "file_path", ".git/config"),
            ("Edit", "file_path", "/repo/.git/hooks/pre-commit"),
            ("MultiEdit", "file_path", "sub/.GIT/info/attributes"),
            ("Write", "file_path", "worktree/.git"),
            ("NotebookEdit", "notebook_path", "/repo/.git/x.ipynb"),
        ] {
            let (broker, mut rx) = ChannelPermissionBroker::new("sess-accept");
            let (hooks, _sink) = hooks_with_mode(PermissionMode::AcceptEdits);
            broker.set_auto_mode_hooks(Some(hooks));
            let mut input = json!({ "content": "[core]" });
            input[field] = json!(target);
            let decision = PermissionDecision::ask(
                PermissionRequest::new("Edit file", format!("Edit wants to modify: {target}"))
                    .with_options(["allow_once", "reject_once"]),
                Some(input.clone()),
            );
            let pending = tokio::spawn({
                let input = input.clone();
                async move {
                    broker
                        .resolve(
                            &NamedEchoTool(tool_name),
                            input,
                            &ToolContext::new().with_cwd("/repo"),
                            decision,
                        )
                        .await
                }
            });

            let query = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{tool_name} {target} must prompt"))
                .expect("permission receiver should stay open");
            drop(query.response_tx);
            assert!(pending.await.unwrap().is_err(), "{tool_name} {target}");
        }
    }

    /// Paths that only look like git's are ordinary project files.
    #[tokio::test]
    async fn accept_edits_still_runs_edits_beside_git_metadata() {
        for target in [
            ".gitignore",
            ".gitattributes",
            ".github/workflows/ci.yml",
            "src/git.rs",
        ] {
            let (broker, mut rx) = ChannelPermissionBroker::new("sess-accept");
            let (hooks, _sink) = hooks_with_mode(PermissionMode::AcceptEdits);
            broker.set_auto_mode_hooks(Some(hooks));
            let input = json!({ "file_path": target, "content": "x" });
            let decision = PermissionDecision::ask(
                PermissionRequest::new("Edit file", "Edit wants to modify a file"),
                Some(input.clone()),
            );

            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                broker.resolve(
                    &NamedEchoTool("Write"),
                    input.clone(),
                    &ToolContext::new().with_cwd("/repo"),
                    decision,
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("{target} must not wait on a dialog"))
            .expect("acceptEdits should run the edit");

            assert_eq!(result, input);
            assert!(rx.try_recv().is_err(), "{target}");
        }
    }

    /// Auto mode's classifier does not get to answer for it either: the
    /// edit goes to the user, and the classifier is never asked.
    #[tokio::test]
    async fn auto_mode_prompts_for_an_edit_into_git_metadata() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Allow {
                reason: "Only edits a config file.".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));
        let input = json!({ "file_path": "/repo/.git/config", "content": "[core]" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Write file", "Write wants to overwrite /repo/.git/config"),
            Some(input.clone()),
        );
        let pending = tokio::spawn(async move {
            broker
                .resolve(
                    &NamedEchoTool("Write"),
                    input,
                    &ToolContext::new().with_cwd("/repo"),
                    decision,
                )
                .await
        });

        let query = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("an edit into .git must prompt under auto mode")
            .expect("permission receiver should stay open");
        drop(query.response_tx);
        assert!(pending.await.unwrap().is_err());
        assert_eq!(classifier.call_count(), 0);
        assert_eq!(sink.store().lock().unwrap().len(), 0);
    }

    /// "Other operations follow policy": a shell call in this mode behaves
    /// exactly as it does in `default`, dialog and all.
    #[tokio::test]
    async fn accept_edits_still_prompts_for_tools_outside_the_closed_set() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-accept");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::AcceptEdits);
        broker.set_auto_mode_hooks(Some(hooks));

        let context = ToolContext::new().with_tool_use_id("t-bash");
        let input = json!({ "command": "rm -rf ./build" });
        let pending = tokio::spawn({
            let input = input.clone();
            async move {
                broker
                    .resolve(
                        &BashEchoTool,
                        input.clone(),
                        &context,
                        bash_ask_decision(input),
                    )
                    .await
            }
        });

        let query = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("Bash must still prompt under acceptEdits")
            .expect("permission receiver should stay open");
        assert_eq!(query.tool_name, "Bash");
        drop(query.response_tx);
        assert!(pending.await.unwrap().is_err());
    }

    /// The mode resolves an *Ask*. A deny rule resolves to `Deny` one layer up
    /// and must still win, even for a tool inside the closed set.
    #[tokio::test]
    async fn accept_edits_still_honors_deny_decisions_for_edits() {
        let (broker, _rx) = ChannelPermissionBroker::new("sess-accept");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::AcceptEdits);
        broker.set_auto_mode_hooks(Some(hooks));

        let context = ToolContext::new().with_tool_use_id("t-denied-edit");
        let error = broker
            .resolve(
                &NamedEchoTool("Edit"),
                json!({ "file_path": "/etc/passwd" }),
                &context,
                PermissionDecision::deny("denied by stored rule: Edit(/etc/**)"),
            )
            .await
            .expect_err("a deny rule must outrank acceptEdits");

        assert!(matches!(error, ToolError::PermissionDenied { .. }));
    }

    /// Switching in and out mid-session takes effect on the next dispatch,
    /// and `default` is the contrast: the identical Edit prompts there.
    #[tokio::test]
    async fn accept_edits_toggles_mid_session_and_default_still_prompts() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-accept-flip");
        let (hooks, _sink, mode_cell) = hooks_with_mode_cell(PermissionMode::Default);
        broker.set_auto_mode_hooks(Some(hooks));

        let context = ToolContext::new().with_tool_use_id("t-flip-edit");
        let input = json!({ "file_path": "src/lib.rs" });

        // Default: the same edit opens a dialog.
        let pending = tokio::spawn({
            let broker = broker.clone();
            let context = context.clone();
            let input = input.clone();
            async move {
                let decision = edit_ask_decision(&input);
                broker
                    .resolve(&NamedEchoTool("Edit"), input, &context, decision)
                    .await
            }
        });
        let query = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("default must prompt for an edit")
            .expect("permission receiver should stay open");
        drop(query.response_tx);
        assert!(pending.await.unwrap().is_err());

        // Switched to acceptEdits mid-session: it runs.
        *mode_cell.lock().expect("mode cell poisoned") = PermissionMode::AcceptEdits;
        let decision = edit_ask_decision(&input);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            broker.resolve(&NamedEchoTool("Edit"), input.clone(), &context, decision),
        )
        .await
        .expect("acceptEdits must not block")
        .expect("acceptEdits should run the edit");
        assert_eq!(result, input);
        assert!(rx.try_recv().is_err());
    }

    // ── dontAsk ───────────────────────────────────────────────────

    /// `dontAsk` resolves an Ask by refusing, and the reason rides back on
    /// the tool error so the model can pick another approach.
    #[tokio::test]
    async fn dont_ask_denies_ask_decisions_with_a_reason() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-dont-ask");
        let (hooks, sink) = hooks_with_mode(PermissionMode::DontAsk);
        broker.set_auto_mode_hooks(Some(hooks));

        let context = ToolContext::new().with_tool_use_id("t-dont-ask");
        let input = json!({ "command": "cargo build" });
        let error = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            broker.resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input),
            ),
        )
        .await
        .expect("dontAsk must not wait on an approval dialog")
        .expect_err("dontAsk should refuse an Ask decision");

        let ToolError::PermissionDenied { reason, .. } = error else {
            panic!("expected PermissionDenied, got {error:?}");
        };
        assert!(reason.contains("dontAsk"), "{reason}");
        assert!(reason.contains("Bash"), "{reason}");
        // The model needs to know a retry is pointless, or it will loop.
        assert!(reason.contains("denied again"), "{reason}");
        assert!(rx.try_recv().is_err(), "dontAsk must not emit a dialog");
        // `/permissions` approve/retry only feeds the auto-mode gate, so a
        // dontAsk denial must not land there advertising a dead button.
        assert_eq!(sink.store().lock().unwrap().len(), 0);
    }

    /// The mode only suppresses the prompt. A decision that already resolved
    /// to Allow — which is what a matching allow rule produces one layer up —
    /// still runs the tool.
    #[tokio::test]
    async fn dont_ask_still_runs_allowed_decisions() {
        let (broker, _rx) = ChannelPermissionBroker::new("sess-dont-ask");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::DontAsk);
        broker.set_auto_mode_hooks(Some(hooks));

        let context = ToolContext::new().with_tool_use_id("t-allowed");
        let input = json!({ "command": "cargo build" });
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                PermissionDecision::allow(input.clone()),
            )
            .await
            .expect("an allowed decision must still run under dontAsk");

        assert_eq!(result, input);
    }

    /// Same carve-out as auto and bypass: these dialogs are the tool's data
    /// path, so `dontAsk` leaves them alone rather than removing the model's
    /// ability to ask.
    #[tokio::test]
    async fn dont_ask_still_prompts_for_tools_that_need_the_response() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-dont-ask");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::DontAsk);
        broker.set_auto_mode_hooks(Some(hooks));

        let tool = Arc::new(AskUserQuestionTool);
        let context = ToolContext::new().with_tool_use_id("t-question");
        let input = json!({
            "questions": [{
                "question": "Continue?",
                "header": "Continue",
                "options": [
                    { "label": "Yes", "description": "Continue" },
                    { "label": "No", "description": "Stop" }
                ]
            }]
        });
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        let call_input = input.clone();

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), call_input, &context, decision)
                    .await
            }
        });

        let query = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("AskUserQuestion should still emit a dialog under dontAsk")
            .expect("permission receiver should stay open");
        assert_eq!(query.tool_name, "AskUserQuestion");

        let mut updated_input = input;
        updated_input
            .as_object_mut()
            .unwrap()
            .insert("answers".into(), json!({ "Continue?": "Yes" }));
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: Some(updated_input),
                extra_text: None,
            })
            .unwrap();

        let result = handle.await.unwrap().expect("answered question should run");
        assert_eq!(result["answers"]["Continue?"], "Yes");
    }

    /// The pair users confuse: identical input, identical Ask decision, and
    /// the two "never prompt" modes land on opposite outcomes. Neither one
    /// opens a dialog — that is the only thing they share.
    #[tokio::test]
    async fn dont_ask_and_bypass_are_opposites_on_the_same_call() {
        let input = json!({ "command": "rm -rf ./build" });

        let (bypass_broker, mut bypass_rx) = ChannelPermissionBroker::new("sess-bypass");
        let (bypass_hooks, _) = hooks_with_mode(PermissionMode::BypassPermissions);
        bypass_broker.set_auto_mode_hooks(Some(bypass_hooks));

        let (deny_broker, mut deny_rx) = ChannelPermissionBroker::new("sess-dont-ask");
        let (deny_hooks, _) = hooks_with_mode(PermissionMode::DontAsk);
        deny_broker.set_auto_mode_hooks(Some(deny_hooks));

        let context = ToolContext::new().with_tool_use_id("t-contrast");
        let bypassed = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            bypass_broker.resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            ),
        )
        .await
        .expect("bypass must not block");
        let refused = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            deny_broker.resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            ),
        )
        .await
        .expect("dontAsk must not block");

        assert_eq!(bypassed.expect("bypass runs the call"), input);
        assert!(matches!(refused, Err(ToolError::PermissionDenied { .. })));
        assert!(bypass_rx.try_recv().is_err());
        assert!(deny_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn auto_mode_classifies_every_ask_before_allowing() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Allow {
                reason: "Requested operation is safe.".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new()
            .with_tool_use_id("t-auto")
            .with_auto_mode_classifier_transcript("User: run the requested operation\n");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            Some(json!({"x": 42})),
        );

        let result = broker
            .resolve(tool.as_ref(), json!({"x": 42}), &context, decision)
            .await
            .expect("classifier allow should run the tool");
        assert_eq!(result, json!({"x": 42}));
        assert!(rx.try_recv().is_err(), "auto mode must not emit a dialog");
        assert_eq!(classifier.call_count(), 1);
        let requests = classifier.requests.lock().expect("requests poisoned");
        assert_eq!(
            requests[0].transcript,
            "User: run the requested operation\n"
        );
    }

    /// The note the front-ends render has to be tied to the *auto-mode* gate,
    /// not to "ran without a dialog": `bypassPermissions` also skips the dialog
    /// and would otherwise claim a classifier looked at the call.
    #[tokio::test]
    async fn only_auto_mode_announces_a_dialog_free_run_to_the_ui() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<crate::query::QueryEvent>();

        let (broker, _rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        broker.set_auto_mode_classifier(Some(Arc::new(RecordingClassifier::new(
            FixedClassifierResult::Outcome(AutoModeClassifierOutcome::Allow {
                reason: "Requested operation is safe.".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            }),
        ))));
        broker.set_query_event_tx(Some(event_tx.clone()));

        broker
            .resolve(
                &EchoTool,
                json!({"x": 1}),
                &ToolContext::new().with_tool_use_id("t-auto"),
                PermissionDecision::ask(
                    PermissionRequest::new("Test", "Approve?")
                        .with_options(["allow_once", "reject_once"]),
                    Some(json!({"x": 1})),
                ),
            )
            .await
            .expect("classifier allow should run the tool");

        match event_rx
            .try_recv()
            .expect("auto mode must announce the run")
        {
            crate::query::QueryEvent::ToolAutoModeAllowed {
                tool_use_id,
                source,
            } => {
                assert_eq!(tool_use_id, "t-auto");
                assert_eq!(source, AutoModeAllowSource::Classifier);
            }
            other => panic!("expected ToolAutoModeAllowed, got {other:?}"),
        }

        let (bypass_broker, _bypass_rx) = ChannelPermissionBroker::new("sess-bypass");
        let (bypass_hooks, _) = hooks_with_mode(PermissionMode::BypassPermissions);
        bypass_broker.set_auto_mode_hooks(Some(bypass_hooks));
        bypass_broker.set_query_event_tx(Some(event_tx));
        bypass_broker
            .resolve(
                &EchoTool,
                json!({"x": 2}),
                &ToolContext::new().with_tool_use_id("t-bypass"),
                PermissionDecision::ask(
                    PermissionRequest::new("Test", "Approve?")
                        .with_options(["allow_once", "reject_once"]),
                    Some(json!({"x": 2})),
                ),
            )
            .await
            .expect("bypass runs the call");
        assert!(
            event_rx.try_recv().is_err(),
            "bypassPermissions skips the dialog without any classifier verdict to report"
        );
    }

    #[tokio::test]
    async fn auto_mode_classifier_allows_read_only_embedded_script() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Allow {
                reason: "Reads a public package archive in memory.".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let tool = BashEchoTool;
        let context = ToolContext::new().with_tool_use_id("t-script");
        let updated_input = embedded_python_input(
            "import io, tarfile, urllib.request\nwith urllib.request.urlopen('https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz') as response:\n    archive = tarfile.open(fileobj=io.BytesIO(response.read()), mode='r:gz')\nprint(archive.getnames())",
        );
        let result = broker
            .resolve(
                &tool,
                json!({ "command": "rm should-not-be-classified" }),
                &context,
                bash_ask_decision(updated_input.clone()),
            )
            .await
            .unwrap();

        assert_eq!(result, updated_input);
        assert!(
            rx.try_recv().is_err(),
            "classifier allow must skip the dialog"
        );
        assert_eq!(classifier.call_count(), 1);
        let requests = classifier.requests.lock().expect("requests poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tool_name, "Bash");
        assert_eq!(requests[0].tool_input, updated_input);
        assert_eq!(requests[0].tool_use_id.as_deref(), Some("t-script"));
        assert!(requests[0].transcript.is_empty());
    }

    #[tokio::test]
    async fn continuity_auto_mode_reviews_visible_heredocs() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Allow {
                reason: "Runs bounded project tooling.".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));
        let context = ToolContext::new()
            .with_tool_use_id("t-visible-script")
            .with_workflow_nesting_depth(1)
            .with_execution_policy(rebon_types::ExecutionPolicy::workflow_controller());

        for command in [
            "python - <<'PY'\nimport subprocess\nsubprocess.run(['cargo', 'test'], check=True)\nPY",
            "bash <<'SH'\nprintf '%s\\n' checking\ncargo test -p rebon-core\nSH",
            "pwsh -NoProfile -Command - <<'PS'\nGet-Date\nPS",
        ] {
            let input = json!({ "command": command });
            broker
                .resolve(
                    &BashEchoTool,
                    input.clone(),
                    &context,
                    bash_ask_decision(input),
                )
                .await
                .unwrap();
        }

        assert!(rx.try_recv().is_err());
        let requests = classifier.requests.lock().expect("requests poisoned");
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|request| request.workflow_nesting_depth == 1));
        assert!(requests.iter().all(|request| request.tool_name == "Bash"));
        assert!(requests.iter().all(|request| request
            .tool_input
            .get("command")
            .and_then(Value::as_str)
            .is_some()));
    }

    #[tokio::test]
    async fn continuity_auto_mode_classifies_external_script_files() {
        for command in ["python scripts/check.py", "pwsh -File scripts/check.ps1"] {
            let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
            let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
            broker.set_auto_mode_hooks(Some(hooks));
            let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
                AutoModeClassifierOutcome::Block {
                    reason: "[External Script] executes code whose contents are not visible"
                        .to_owned(),
                    category: Some("External Script".to_owned()),
                },
            )));
            broker.set_auto_mode_classifier(Some(classifier.clone()));
            let context = ToolContext::new()
                .with_tool_use_id("t-external-script")
                .with_workflow_nesting_depth(1)
                .with_auto_mode_classifier_transcript("User: run project checks\n");
            let input = json!({ "command": command });

            let result = broker
                .resolve(
                    &BashEchoTool,
                    input.clone(),
                    &context,
                    bash_ask_decision(input),
                )
                .await;

            let Err(ToolError::PermissionDenied { reason, .. }) = result else {
                panic!("{command} should be blocked, got {result:?}");
            };
            assert!(reason.contains("External Script"), "{reason}");
            assert!(rx.try_recv().is_err(), "auto mode must not prompt");
            assert_eq!(classifier.call_count(), 1, "command: {command}");
            let requests = classifier.requests.lock().expect("requests poisoned");
            assert_eq!(requests[0].tool_input, json!({ "command": command }));
            assert_eq!(requests[0].workflow_nesting_depth, 1);
            assert_eq!(sink.store().lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn auto_mode_classifier_deny_sticks_until_input_or_user_intent_changes() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "Uploads local SSH keys to a remote host.".to_owned(),
                category: Some("Data Exfiltration".to_owned()),
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));
        let context = ToolContext::new()
            .with_tool_use_id("t-deny")
            .with_auto_mode_classifier_transcript("User: inspect the upload attempt\n");
        let input = embedded_python_input("import urllib.request\nprint('hidden upload')");

        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            )
            .await;
        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("classifier deny must fail the call, got {result:?}");
        };
        assert!(reason.contains("auto mode denied"));
        assert!(reason.contains("Uploads local SSH keys"));
        assert!(rx.try_recv().is_err(), "deny must not open a dialog");
        let store = sink.store();
        assert_eq!(store.lock().unwrap().len(), 1);

        // Appending only the agent's denied call, outcome, and retry narration
        // does not create a new authorization context or a classifier re-roll.
        let retry_context = ToolContext::new()
            .with_tool_use_id("t-deny-retry")
            .with_auto_mode_classifier_transcript(concat!(
                "User: inspect the upload attempt\n",
                "[Bash] {\"id\":\"old\",\"input\":{}}\n",
                "{\"outcome\":\"automode-blocked\",\"id\":\"old\"}\n",
                "Assistant: I will retry the upload.\n",
            ));
        let retry = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &retry_context,
                bash_ask_decision(input.clone()),
            )
            .await;
        assert!(matches!(retry, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(
            classifier.call_count(),
            1,
            "autonomous resubmission must not re-roll the classifier"
        );
        assert_eq!(store.lock().unwrap().len(), 1);

        // A new user turn earns a fresh classification even for the same input.
        let user_reasserted_context = ToolContext::new()
            .with_tool_use_id("t-deny-user-reasserted")
            .with_auto_mode_classifier_transcript(concat!(
                "User: inspect the upload attempt\n",
                "[Bash] {\"id\":\"old\",\"input\":{}}\n",
                "{\"outcome\":\"automode-blocked\",\"id\":\"old\"}\n",
                "Assistant: I will retry the upload.\n",
                "User: yes, upload those exact SSH keys now\n",
            ));
        let user_reasserted = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &user_reasserted_context,
                bash_ask_decision(input.clone()),
            )
            .await;
        assert!(matches!(
            user_reasserted,
            Err(ToolError::PermissionDenied { .. })
        ));
        assert_eq!(classifier.call_count(), 2);
        assert_eq!(store.lock().unwrap().len(), 2);

        // A changed invocation also earns a fresh verdict.
        let changed = embedded_python_input("print('completely different')");
        let _ = broker
            .resolve(
                &BashEchoTool,
                changed.clone(),
                &user_reasserted_context,
                bash_ask_decision(changed),
            )
            .await;
        assert_eq!(classifier.call_count(), 3);
    }

    #[tokio::test]
    async fn auto_mode_approve_exemption_admits_exactly_one_identical_call() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        let verdicts = Arc::clone(hooks.verdicts());
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "Exfiltrates data.".to_owned(),
                category: Some("Data Exfiltration".to_owned()),
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));
        let context = ToolContext::new().with_tool_use_id("t-exempt");
        let input = embedded_python_input("print('to be approved')");

        let denied = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            )
            .await;
        assert!(matches!(denied, Err(ToolError::PermissionDenied { .. })));

        // `/permissions approve` installs the exemption from the stored
        // record's exact tool name + input payload.
        let (tool_name, tool_input) = {
            let store = sink.store();
            let guard = store.lock().unwrap();
            let record = guard.iter().next().expect("denial recorded");
            (record.tool_name.clone(), record.tool_input.clone())
        };
        verdicts.exempt_once(&tool_name, &tool_input);

        let approved = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            )
            .await
            .expect("the approved identical call must run once");
        assert_eq!(approved, input);
        assert!(rx.try_recv().is_err());

        // The exemption is one-shot: the next identical call goes back
        // through the classifier (the lifted denial is re-evaluated).
        let after = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input.clone()),
            )
            .await;
        assert!(matches!(after, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(classifier.call_count(), 2);
    }

    /// A mode source that knows where plan mode was entered from, as the
    /// session record does.
    struct PlanOrigin {
        mode: PermissionMode,
        entered_from: Option<PermissionMode>,
    }

    impl PermissionModeProvider for PlanOrigin {
        fn current_mode(&self) -> PermissionMode {
            self.mode
        }

        fn plan_entered_from(&self) -> Option<PermissionMode> {
            self.entered_from
        }
    }

    fn plan_hooks(entered_from: Option<PermissionMode>) -> (AutoModeHooks, SharedDenialSink) {
        let sink = SharedDenialSink::new(Arc::new(StdMutex::new(AutoModeDenialStore::default())));
        let provider: Arc<dyn PermissionModeProvider> = Arc::new(PlanOrigin {
            mode: PermissionMode::Plan,
            entered_from,
        });
        (AutoModeHooks::new(Arc::new(sink.clone()), provider), sink)
    }

    /// The user already let the classifier answer for them in auto mode;
    /// planning does not take that back, so a prompt a plan needs goes to the
    /// classifier rather than the user.
    #[tokio::test]
    async fn plan_entered_from_auto_asks_the_classifier_instead_of_the_user() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-plan-auto");
        let (hooks, sink) = plan_hooks(Some(PermissionMode::Auto));
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Allow {
                reason: "Inspects the repository.".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = embedded_python_input("print('inspect')");
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-plan-auto"),
                bash_ask_decision(input.clone()),
            )
            .await
            .unwrap();

        assert_eq!(result, input);
        assert!(
            rx.try_recv().is_err(),
            "the classifier answered, not a dialog"
        );
        assert_eq!(classifier.call_count(), 1);
        assert_eq!(sink.store().lock().unwrap().len(), 0);
    }

    /// A refusal under plan-from-auto is auto mode's refusal: the model is
    /// told, and `/permissions` keeps a record to approve or retry from.
    #[tokio::test]
    async fn plan_entered_from_auto_records_what_the_classifier_refuses() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-plan-auto");
        let (hooks, sink) = plan_hooks(Some(PermissionMode::Auto));
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "Pushes to a shared branch.".to_owned(),
                category: None,
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = embedded_python_input("print('push')");
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-plan-block"),
                bash_ask_decision(input),
            )
            .await;

        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("the classifier's block must refuse the call, got {result:?}");
        };
        assert!(reason.contains("Pushes to a shared branch"), "{reason}");
        assert!(rx.try_recv().is_err(), "a refusal is not a dialog");
        assert_eq!(sink.store().lock().unwrap().len(), 1);
    }

    /// Plan entered from anywhere but auto — or from nowhere anyone
    /// recorded — prompts the way plan mode always has, and the classifier
    /// is never asked.
    #[tokio::test]
    async fn plan_entered_from_anywhere_else_still_asks_the_user() {
        for entered_from in [
            Some(PermissionMode::Default),
            Some(PermissionMode::AcceptEdits),
            None,
        ] {
            let (broker, mut rx) = ChannelPermissionBroker::new("sess-plan");
            let (hooks, _sink) = plan_hooks(entered_from);
            broker.set_auto_mode_hooks(Some(hooks));
            let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
                AutoModeClassifierOutcome::Allow {
                    reason: "unused".to_owned(),
                    stage: AutoModeClassifierStage::Fast,
                },
            )));
            broker.set_auto_mode_classifier(Some(classifier.clone()));

            let input = embedded_python_input("print('inspect')");
            let handle = tokio::spawn({
                let input = input.clone();
                async move {
                    broker
                        .resolve(
                            &BashEchoTool,
                            input.clone(),
                            &ToolContext::new().with_tool_use_id("t-plan"),
                            bash_ask_decision(input),
                        )
                        .await
                }
            });

            let query = rx
                .recv()
                .await
                .unwrap_or_else(|| panic!("plan from {entered_from:?} must prompt"));
            query.response_tx.send(PermissionAnswer::Cancelled).unwrap();
            assert!(matches!(
                handle.await.unwrap(),
                Err(ToolError::PermissionDenied { .. })
            ));
            assert_eq!(classifier.call_count(), 0, "from {entered_from:?}");
        }
    }

    /// The mode function alone, without a broker: plan from auto routes
    /// through the gate, plan from default does not.
    #[tokio::test]
    async fn resolve_ask_under_plan_follows_where_plan_was_entered_from() {
        let rules = PermissionRules::default();
        let input = embedded_python_input("print('inspect')");
        let context = ToolContext::new();
        let allow: Arc<dyn AutoModeClassifier> = Arc::new(RecordingClassifier::new(
            FixedClassifierResult::Outcome(AutoModeClassifierOutcome::Allow {
                reason: "fine".to_owned(),
                stage: AutoModeClassifierStage::Fast,
            }),
        ));

        let (from_auto, _) = plan_hooks(Some(PermissionMode::Auto));
        let outcome = resolve_ask_under_mode(
            PermissionMode::Plan,
            "Bash",
            &input,
            &context,
            Some(&from_auto),
            Some(allow.clone()),
            &rules,
        )
        .await;
        assert!(matches!(outcome, ModeAskOutcome::Run(Some(_))));

        let (from_default, _) = plan_hooks(Some(PermissionMode::Default));
        let outcome = resolve_ask_under_mode(
            PermissionMode::Plan,
            "Bash",
            &input,
            &context,
            Some(&from_default),
            Some(allow.clone()),
            &rules,
        )
        .await;
        assert!(matches!(outcome, ModeAskOutcome::Ask));

        let outcome = resolve_ask_under_mode(
            PermissionMode::Plan,
            "Bash",
            &input,
            &context,
            None,
            Some(allow),
            &rules,
        )
        .await;
        assert!(matches!(outcome, ModeAskOutcome::Ask));
    }

    #[tokio::test]
    async fn auto_mode_classifier_errors_fail_closed_without_dialog() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Error));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = embedded_python_input("print('inspect')");
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-script"),
                bash_ask_decision(input),
            )
            .await;
        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("classifier failure must block the call, got {result:?}");
        };
        assert!(reason.contains("Classifier unavailable"), "{reason}");
        assert!(rx.try_recv().is_err(), "classifier failure must not prompt");
        assert_eq!(classifier.call_count(), 1);
        assert_eq!(sink.store().lock().unwrap().len(), 1);
    }

    /// The denial the agent (and `/permissions`) sees must say *which*
    /// classifier failed, not just that it "failed" — the same wording
    /// covers a dead API key and a model that will not follow the
    /// grammar, and only one of those is worth retrying.
    ///
    /// It must stop there. This reason lands in the transcript of the
    /// agent the classifier is judging, so the classifier's own reply —
    /// its `<thinking>` above all — stays out of it. Three denials on
    /// Terminal-Bench 4.0 r1 carried 284, 504 and 560 characters of monitor
    /// reasoning back into the monitored context.
    #[tokio::test]
    async fn a_classifier_parse_failure_names_the_stage_and_model_only() {
        let (broker, _rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        broker.set_auto_mode_classifier(Some(Arc::new(RecordingClassifier::new(
            FixedClassifierResult::ParseError,
        ))));

        let input = embedded_python_input("print('inspect')");
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-script"),
                bash_ask_decision(input),
            )
            .await;
        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("a parse failure must block the call, got {result:?}");
        };

        // Headline kept: `tool_outcome_code` classifies on it.
        assert!(
            reason.contains("Classifier response could not be parsed"),
            "{reason}"
        );
        assert!(reason.contains("Fast stage"), "{reason}");
        assert!(reason.contains("chatty-model"), "{reason}");
        // Neither the grammar rule it broke nor a word of what it said.
        assert!(
            !reason.contains("unexpected content after </block>"),
            "{reason}"
        );
        assert!(
            !reason.contains("This only reads repository state."),
            "{reason}"
        );
        assert_eq!(sink.store().lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_mode_without_classifier_fails_closed_without_dialog() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));

        let input = embedded_python_input("print('inspect')");
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-script"),
                bash_ask_decision(input),
            )
            .await;
        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("missing classifier must block the call, got {result:?}");
        };
        assert!(reason.contains("Classifier unavailable"), "{reason}");
        assert!(rx.try_recv().is_err(), "missing classifier must not prompt");
        assert_eq!(sink.store().lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_mode_classifies_commands_previously_caught_by_static_rules() {
        for command in [
            "PYTHON=python\n$PYTHON - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('data')\nPY",
            ":\np\\ython - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('data')\nPY",
            "# << decoy\nPYTHON=python\n$PYTHON - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('data')\nPY",
            "bash <<'SH'\ntouch x\nSH",
            "cat <<'EOF' > $OUT\ndata\nEOF",
        ] {
            let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
            let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
            broker.set_auto_mode_hooks(Some(hooks));
            let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
                AutoModeClassifierOutcome::Allow {
                    reason: "Authorized by user intent.".to_owned(),
                    stage: AutoModeClassifierStage::Fast,
                },
            )));
            broker.set_auto_mode_classifier(Some(classifier.clone()));

            let input = json!({ "command": command });
            let result = broker
                .resolve(
                    &BashEchoTool,
                    input.clone(),
                    &ToolContext::new().with_tool_use_id("t-script"),
                    bash_ask_decision(input.clone()),
                )
                .await
                .expect("classifier allow should run the command");
            assert_eq!(result, input);
            assert!(rx.try_recv().is_err(), "auto mode must not prompt");
            assert_eq!(classifier.call_count(), 1, "command: {command}");
        }
    }

    #[tokio::test]
    async fn auto_mode_classifier_allow_is_ignored_after_mode_change() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink, mode_cell) = hooks_with_mode_cell(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(BlockingAllowClassifier {
            started: Notify::new(),
            release: Notify::new(),
        });
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let tool = Arc::new(BashEchoTool);
        let context = ToolContext::new().with_tool_use_id("t-script");
        let input = embedded_python_input("print('inspect')");
        let handle = tokio::spawn({
            let input = input.clone();
            let context = context.clone();
            let tool = tool.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        input.clone(),
                        &context,
                        bash_ask_decision(input),
                    )
                    .await
            }
        });

        classifier.started.notified().await;
        *mode_cell.lock().expect("mode cell poisoned") = PermissionMode::Default;
        classifier.release.notify_one();
        let query = rx.recv().await.expect("mode change should force a dialog");
        query.response_tx.send(PermissionAnswer::Cancelled).unwrap();
        assert!(matches!(
            handle.await.unwrap(),
            Err(ToolError::PermissionDenied { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn auto_mode_classifier_timeout_fails_closed_without_dialog() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(PendingClassifier {
            started: Notify::new(),
        });
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = embedded_python_input("print('inspect')");
        let handle = tokio::spawn({
            let input = input.clone();
            async move {
                broker
                    .resolve(
                        &BashEchoTool,
                        input.clone(),
                        &ToolContext::new().with_tool_use_id("t-script"),
                        bash_ask_decision(input),
                    )
                    .await
            }
        });

        classifier.started.notified().await;
        // Past the bound, whatever it currently is: the number moved once
        // already, when a reasoning provider turned out not to fit inside it.
        tokio::time::advance(AUTO_MODE_CLASSIFIER_TIMEOUT + Duration::from_secs(1)).await;
        let result = handle.await.unwrap();
        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("classifier timeout must block, got {result:?}");
        };
        assert!(reason.contains("timed out"), "{reason}");
        assert!(rx.try_recv().is_err(), "classifier timeout must not prompt");
        assert_eq!(sink.store().lock().unwrap().len(), 1);
    }

    #[test]
    fn auto_mode_requires_agent_authorization_response() {
        assert!(requires_permission_broker_response("Agent"));
        assert!(requires_permission_broker_response("AskUserQuestion"));
    }

    /// The workflow review is a plan-approval step: every prompting mode
    /// surfaces it instead of resolving it silently. The two "never prompt"
    /// modes keep their own contracts — `bypassPermissions` runs the
    /// workflow unreviewed, and `dontAsk` denies it with a reason that
    /// points at the modes that can review it.
    #[tokio::test]
    async fn workflow_review_reaches_the_prompt_except_under_no_prompt_modes() {
        let context = ToolContext::new();
        let input = json!({"script": "export const meta = { name: 'demo' };"});
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        for tool_name in ["Workflow", "RunWorkflow"] {
            for mode in [PermissionMode::AcceptEdits, PermissionMode::Auto] {
                let outcome = resolve_ask_under_mode(
                    mode,
                    tool_name,
                    &input,
                    &context,
                    Some(&hooks),
                    None,
                    &PermissionRules::default(),
                )
                .await;
                assert!(
                    matches!(outcome, ModeAskOutcome::Ask),
                    "{tool_name} under {mode:?} must ask"
                );
            }
            let outcome = resolve_ask_under_mode(
                PermissionMode::BypassPermissions,
                tool_name,
                &input,
                &context,
                Some(&hooks),
                None,
                &PermissionRules::default(),
            )
            .await;
            assert!(
                matches!(outcome, ModeAskOutcome::Run(None)),
                "{tool_name} under bypassPermissions must run without a review"
            );
            let outcome = resolve_ask_under_mode(
                PermissionMode::DontAsk,
                tool_name,
                &input,
                &context,
                Some(&hooks),
                None,
                &PermissionRules::default(),
            )
            .await;
            let ModeAskOutcome::Deny(reason) = outcome else {
                panic!("{tool_name} under dontAsk must deny instead of prompting");
            };
            assert!(reason.contains("dontAsk"), "{reason}");
            assert!(reason.contains(tool_name), "{reason}");
            // The reason must route the model to a mode switch, not a
            // permission rule, and must mark retries as pointless.
            assert!(reason.contains("bypassPermissions"), "{reason}");
            assert!(reason.contains("denied again"), "{reason}");
        }
    }

    #[test]
    fn auto_mode_cache_key_tracks_transcript_and_execution_context() {
        let input = json!({ "command": "cargo test" }).to_string();
        let base = ToolContext::new().with_cwd("/repo");
        let base_key = auto_mode_classifier_cache_key(&input, "User: test it\n", &base);
        assert_eq!(base_key.len(), 64);
        assert!(!base_key.contains("User: test it"));

        assert_ne!(
            base_key,
            auto_mode_classifier_cache_key(&input, "User: delete it\n", &base)
        );
        assert_ne!(
            base_key,
            auto_mode_classifier_cache_key(
                &input,
                "User: test it\n",
                &base.clone().with_workflow_nesting_depth(1),
            )
        );
        assert_ne!(
            base_key,
            auto_mode_classifier_cache_key(
                &input,
                "User: test it\n",
                &base.clone().with_isolated_worktree(true),
            )
        );
        assert_ne!(
            base_key,
            auto_mode_classifier_cache_key(
                &input,
                "User: test it\n",
                &ToolContext::new().with_cwd("/other"),
            )
        );
    }

    #[test]
    fn denial_cache_sticks_across_autonomous_retries_but_not_new_user_intent() {
        let input = json!({ "command": "git push --force origin main" }).to_string();
        let context = ToolContext::new().with_cwd("/repo");
        let original_context =
            "Assistant: I propose pushing forcefully to main.\nUser: no, do not do that\n";
        let autonomous_retry = concat!(
            "Assistant: I propose pushing forcefully to main.\n",
            "User: no, do not do that\n",
            "[Bash] {\"id\":\"old\",\"input\":{}}\n",
            "{\"outcome\":\"automode-blocked\",\"id\":\"old\"}\n",
            "Assistant: I will try the same action again.\n",
        );
        let user_reassertion = concat!(
            "Assistant: I propose pushing forcefully to main.\n",
            "User: no, do not do that\n",
            "[Bash] {\"id\":\"old\",\"input\":{}}\n",
            "{\"outcome\":\"automode-blocked\",\"id\":\"old\"}\n",
            "Assistant: I will try the same action again.\n",
            "User: actually, force push to main now\n",
        );

        assert_eq!(
            auto_mode_denial_cache_key(&input, original_context, &context),
            auto_mode_denial_cache_key(&input, autonomous_retry, &context),
        );
        assert_ne!(
            auto_mode_classifier_cache_key(&input, original_context, &context),
            auto_mode_classifier_cache_key(&input, autonomous_retry, &context),
            "allows must still be re-evaluated when autonomous context changes",
        );
        assert_ne!(
            auto_mode_denial_cache_key(&input, original_context, &context),
            auto_mode_denial_cache_key(&input, user_reassertion, &context),
            "new user intent must earn a fresh classifier verdict",
        );
    }

    #[tokio::test]
    async fn auto_mode_classifies_workflow_sensitive_git_in_every_context() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "[Git Destructive] force push can rewrite shared history".to_owned(),
                category: Some("Git Destructive".to_owned()),
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = json!({"command": "git push --force-with-lease origin main"});
        for context in [
            ToolContext::new().with_tool_use_id("t-ordinary-force"),
            ToolContext::new()
                .with_tool_use_id("t-workflow-force")
                .with_workflow_nesting_depth(1),
        ] {
            let result = broker
                .resolve(
                    &BashEchoTool,
                    input.clone(),
                    &context,
                    bash_ask_decision(input.clone()),
                )
                .await;
            assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        }

        assert!(rx.try_recv().is_err(), "auto mode must not prompt");
        assert_eq!(classifier.call_count(), 2);
        let requests = classifier.requests.lock().expect("requests poisoned");
        assert_eq!(
            requests
                .iter()
                .map(|request| request.workflow_nesting_depth)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[tokio::test]
    async fn auto_mode_classifies_workflow_powershell_git_risks() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "[Git Destructive] deletes another agent's branch".to_owned(),
                category: Some("Git Destructive".to_owned()),
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let context = ToolContext::new()
            .with_tool_use_id("t-workflow-powershell")
            .with_workflow_nesting_depth(1);
        let input = json!({"command": "Set-Location repo; git branch -D other-agent"});
        let result = broker
            .resolve(
                &PowerShellEchoTool,
                input.clone(),
                &context,
                bash_ask_decision(input),
            )
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert!(rx.try_recv().is_err(), "auto mode must not prompt");
        assert_eq!(classifier.call_count(), 1);
    }

    #[tokio::test]
    async fn auto_mode_classifier_blocks_sensitive_git_stash_commands() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "[Git Destructive] stash pop can overwrite local changes".to_owned(),
                category: Some("Git Destructive".to_owned()),
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = json!({"command": "git stash pop"});
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-stash"),
                bash_ask_decision(input),
            )
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert!(rx.try_recv().is_err(), "auto mode must not prompt");
        assert_eq!(classifier.call_count(), 1);
    }

    #[tokio::test]
    async fn auto_mode_classifier_blocks_sensitive_git_checkout_commands() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "[Git Destructive] checkout would discard local changes".to_owned(),
                category: Some("Git Destructive".to_owned()),
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = json!({"command": "git checkout -- src/lib.rs"});
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-checkout"),
                bash_ask_decision(input),
            )
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert!(rx.try_recv().is_err(), "auto mode must not prompt");
        assert_eq!(classifier.call_count(), 1);
    }

    #[tokio::test]
    async fn auto_mode_classifier_blocks_sensitive_file_deletion_commands() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));
        let classifier = Arc::new(RecordingClassifier::new(FixedClassifierResult::Outcome(
            AutoModeClassifierOutcome::Block {
                reason: "[File Deletion] deletes a user file without authorization".to_owned(),
                category: Some("File Deletion".to_owned()),
            },
        )));
        broker.set_auto_mode_classifier(Some(classifier.clone()));

        let input = json!({"command": "rm old.log"});
        let result = broker
            .resolve(
                &BashEchoTool,
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-rm"),
                bash_ask_decision(input),
            )
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert!(rx.try_recv().is_err(), "auto mode must not prompt");
        assert_eq!(classifier.call_count(), 1);
    }

    #[tokio::test]
    async fn auto_mode_still_prompts_for_ask_user_question_answers() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));

        let tool = Arc::new(AskUserQuestionTool);
        let context = ToolContext::new().with_tool_use_id("t-question");
        let input = json!({
            "questions": [{
                "question": "Continue?",
                "header": "Continue",
                "options": [
                    { "label": "Yes", "description": "Continue" },
                    { "label": "No", "description": "Stop" }
                ]
            }]
        });
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        let call_input = input.clone();

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), call_input, &context, decision)
                    .await
            }
        });

        let query = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("AskUserQuestion should still emit a dialog in auto mode")
            .expect("permission receiver should stay open");
        assert_eq!(query.tool_name, "AskUserQuestion");
        assert!(!query.message.is_empty());

        let mut updated_input = input;
        updated_input
            .as_object_mut()
            .unwrap()
            .insert("answers".into(), json!({ "Continue?": "Yes" }));
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: Some(updated_input),
                extra_text: None,
            })
            .unwrap();

        let result = handle.await.unwrap().expect("answered question should run");
        assert_eq!(result["answers"]["Continue?"], "Yes");
    }

    #[tokio::test]
    async fn auto_mode_records_deny_into_sink() {
        let (broker, _rx) = ChannelPermissionBroker::new("sess-auto");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Auto);
        broker.set_auto_mode_hooks(Some(hooks));

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t-deny");
        let decision = PermissionDecision::deny("classifier blocked");

        let result = broker
            .resolve(
                tool.as_ref(),
                json!({"command": "rm -rf /"}),
                &context,
                decision,
            )
            .await;
        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));

        let store = sink.store();
        let guard = store.lock().unwrap();
        assert_eq!(guard.len(), 1, "expected one recorded denial");
        let denial = guard.iter().next().unwrap();
        assert_eq!(denial.tool_name, "Echo");
        assert_eq!(denial.tool_use_id, "t-deny");
        assert!(denial.display.contains("rm -rf"));
        assert_eq!(denial.reason, "classifier blocked");
    }

    #[tokio::test]
    async fn non_auto_mode_does_not_record_deny() {
        let (broker, _rx) = ChannelPermissionBroker::new("sess-default");
        let (hooks, sink) = hooks_with_mode(PermissionMode::Default);
        broker.set_auto_mode_hooks(Some(hooks));

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t-noauto");
        let decision = PermissionDecision::deny("classifier blocked");

        let _ = broker
            .resolve(tool.as_ref(), json!({}), &context, decision)
            .await;
        assert_eq!(sink.store().lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn non_auto_mode_still_asks() {
        let (broker, mut rx) = ChannelPermissionBroker::new("sess-default");
        let (hooks, _sink) = hooks_with_mode(PermissionMode::Default);
        broker.set_auto_mode_hooks(Some(hooks));

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("t-ask");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            None,
        );

        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(tool.as_ref(), json!({}), &context, decision)
                    .await
            }
        });

        let query = rx.recv().await.expect("default mode should send dialog");
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();
        let _ = handle.await.unwrap();
    }

    /// Deferred `AskUserQuestion`: the broker's half. The sink stands in for
    /// the surface; what the surface does with a delivery is its own test.
    mod deferred_questions {
        use super::*;
        use crate::deferred_question::{DeferredQuestionAnswer, DEFERRED_QUESTIONS_ENV};

        struct ChannelSink(mpsc::UnboundedSender<DeferredQuestionAnswer>);

        impl DeferredQuestionSink for ChannelSink {
            fn deliver(&self, answer: DeferredQuestionAnswer) {
                self.0.send(answer).expect("test holds the receiver");
            }
        }

        struct OneTool(Arc<dyn Tool>);

        impl rebon_tool::ToolResolver for OneTool {
            fn resolve(
                &self,
                name: &str,
                _filter: Option<&rebon_tool::ToolFilter>,
            ) -> ToolResult<Option<Arc<dyn Tool>>> {
                Ok((self.0.id().as_str() == name).then(|| self.0.clone()))
            }

            fn tools(
                &self,
                _filter: Option<&rebon_tool::ToolFilter>,
            ) -> ToolResult<Vec<Arc<dyn Tool>>> {
                Ok(vec![self.0.clone()])
            }
        }

        const QUESTION: &str = "Which database?";

        fn question_input() -> Value {
            json!({
                "questions": [{
                    "question": QUESTION,
                    "header": "DB",
                    "options": [
                        { "label": "Postgres", "description": "Relational" },
                        { "label": "SQLite", "description": "Embedded" }
                    ]
                }]
            })
        }

        fn answered_input() -> Value {
            let mut input = question_input();
            input["answers"] = json!({ QUESTION: "Postgres" });
            input
        }

        fn question_context() -> ToolContext {
            ToolContext::new()
                .with_tool_use_id("toolu_q")
                .with_tool_resolver(Arc::new(OneTool(Arc::new(AskUserQuestionTool))))
        }

        fn broker_with_sink() -> (
            ChannelPermissionBroker,
            mpsc::UnboundedReceiver<OutboundPermissionQuery>,
            mpsc::UnboundedReceiver<DeferredQuestionAnswer>,
        ) {
            let (broker, rx) = ChannelPermissionBroker::new("sess-deferred");
            let (tx, deliveries) = mpsc::unbounded_channel();
            broker.set_deferred_question_sink(Some(Arc::new(ChannelSink(tx))));
            (broker, rx, deliveries)
        }

        async fn ask(
            broker: &ChannelPermissionBroker,
            input: Value,
            context: &ToolContext,
        ) -> ToolResult<Value> {
            let tool = AskUserQuestionTool;
            let decision = tool.check_permissions(&input, context).await.unwrap();
            broker.resolve(&tool, input, context, decision).await
        }

        fn answer_with(query: OutboundPermissionQuery, answer: PermissionAnswer) {
            query.response_tx.send(answer).expect("broker still waits");
        }

        fn selected(updated_input: Value) -> PermissionAnswer {
            PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: Some(updated_input),
                extra_text: None,
            }
        }

        async fn next_delivery(
            deliveries: &mut mpsc::UnboundedReceiver<DeferredQuestionAnswer>,
        ) -> DeferredQuestionAnswer {
            tokio::time::timeout(std::time::Duration::from_secs(5), deliveries.recv())
                .await
                .expect("the answer is delivered")
                .expect("sink stays open")
        }

        /// A question that waits resolves to the answers only after the
        /// dialog is answered, and nothing reaches the sink.
        async fn assert_waits_for_the_dialog(
            broker: ChannelPermissionBroker,
            mut rx: mpsc::UnboundedReceiver<OutboundPermissionQuery>,
            mut deliveries: Option<mpsc::UnboundedReceiver<DeferredQuestionAnswer>>,
            input: Value,
            context: ToolContext,
        ) {
            let handle = tokio::spawn(async move { ask(&broker, input, &context).await });
            let query = rx.recv().await.expect("the dialog is shown");
            assert!(!handle.is_finished(), "a waiting ask holds the call");
            let mut answered = query.tool_input.clone().expect("question input");
            answered["answers"] = json!({ QUESTION: "Postgres" });
            answer_with(query, selected(answered));
            let output = handle.await.unwrap().expect("answered question runs");
            assert_eq!(output["answers"][QUESTION], "Postgres");
            if let Some(deliveries) = deliveries.as_mut() {
                assert!(deliveries.try_recv().is_err(), "nothing was deferred");
            }
        }

        #[tokio::test]
        async fn an_eligible_question_returns_pending_and_the_answer_reaches_the_sink() {
            let _env = crate::test_env_lock();
            let (broker, mut rx, mut deliveries) = broker_with_sink();

            let output = ask(&broker, question_input(), &question_context())
                .await
                .expect("a deferred question does not fail");
            assert_eq!(output["status"], "pending");
            assert_eq!(output["toolUseId"], "toolu_q");
            let pending = crate::deferred_question::pending_model_text(&output).unwrap();
            assert!(pending.contains("<question-answer tool_use_id=\"toolu_q\">"));

            // The dialog went out exactly as a waiting ask's would.
            let query = rx.try_recv().expect("the dialog is shown");
            assert_eq!(query.tool_name, "AskUserQuestion");
            assert_eq!(query.tool_call_id, "toolu_q");
            assert_eq!(query.session_id, "sess-deferred");

            answer_with(
                query,
                PermissionAnswer::Selected {
                    option_id: "allow_once".into(),
                    updated_input: Some(answered_input()),
                    extra_text: Some("go fast".into()),
                },
            );
            let delivery = next_delivery(&mut deliveries).await;
            assert_eq!(delivery.session_id, "sess-deferred");
            assert_eq!(delivery.tool_use_id, "toolu_q");
            assert!(delivery.start_turn_if_idle);
            assert_eq!(
                delivery.display_text,
                "Answered questions:\n- Which database?\n  Answer: Postgres"
            );
            assert_eq!(
                delivery.model_text,
                "<question-answer tool_use_id=\"toolu_q\">\n\
                 Answered questions:\n- Which database?\n  Answer: Postgres\n\
                 User note: go fast\n\
                 </question-answer>"
            );
        }

        /// The per-turn view the executor builds shares the sink the surface
        /// installed on the long-lived broker.
        #[tokio::test]
        async fn a_per_turn_view_defers_through_the_installed_sink() {
            let _env = crate::test_env_lock();
            let (broker, mut rx, mut deliveries) = broker_with_sink();
            let turn = broker.for_session("sess-turn");
            assert!(turn.has_deferred_question_sink());

            let output = ask(&turn, question_input(), &question_context())
                .await
                .unwrap();
            assert_eq!(output["status"], "pending");
            answer_with(rx.try_recv().unwrap(), selected(answered_input()));
            assert_eq!(next_delivery(&mut deliveries).await.session_id, "sess-turn");
        }

        #[tokio::test]
        async fn an_answer_after_the_asking_turn_is_gone_still_arrives() {
            let _env = crate::test_env_lock();
            let (broker, mut rx, mut deliveries) = broker_with_sink();
            ask(&broker, question_input(), &question_context())
                .await
                .unwrap();
            let query = rx.try_recv().unwrap();
            // Nothing of the turn survives but the dialog.
            drop(broker);
            answer_with(query, selected(answered_input()));
            assert_eq!(next_delivery(&mut deliveries).await.tool_use_id, "toolu_q");
        }

        #[tokio::test]
        async fn a_rejection_or_cancel_delivers_a_dismissal() {
            let _env = crate::test_env_lock();
            let (broker, mut rx, mut deliveries) = broker_with_sink();

            ask(&broker, question_input(), &question_context())
                .await
                .unwrap();
            answer_with(rx.try_recv().unwrap(), PermissionAnswer::Cancelled);
            let dismissal = next_delivery(&mut deliveries).await;
            assert!(!dismissal.start_turn_if_idle);
            assert!(dismissal.model_text.contains("status=\"dismissed\""));

            ask(&broker, question_input(), &question_context())
                .await
                .unwrap();
            answer_with(
                rx.try_recv().unwrap(),
                PermissionAnswer::Selected {
                    option_id: "reject_once".into(),
                    updated_input: None,
                    extra_text: Some("neither, explain first".into()),
                },
            );
            let rejection = next_delivery(&mut deliveries).await;
            assert!(rejection.model_text.contains("status=\"dismissed\""));
            assert!(rejection
                .model_text
                .contains("User note: neither, explain first"));
            assert!(
                rejection.start_turn_if_idle,
                "a note the user wrote is something they said"
            );
        }

        #[tokio::test]
        async fn a_dialog_dropped_unanswered_delivers_nothing() {
            let _env = crate::test_env_lock();
            let (broker, mut rx, mut deliveries) = broker_with_sink();
            ask(&broker, question_input(), &question_context())
                .await
                .unwrap();
            drop(rx.try_recv().unwrap());
            drop(broker);
            // The task ends and takes the only other sender with it.
            let closed = tokio::time::timeout(std::time::Duration::from_secs(5), deliveries.recv())
                .await
                .expect("the waiting task finishes");
            assert!(closed.is_none(), "nothing delivered: {closed:?}");
        }

        #[tokio::test]
        async fn without_a_sink_the_question_waits() {
            let _env = crate::test_env_lock();
            let (broker, rx) = ChannelPermissionBroker::new("sess-no-sink");
            assert!(!broker.has_deferred_question_sink());
            assert_waits_for_the_dialog(broker, rx, None, question_input(), question_context())
                .await;
        }

        #[tokio::test]
        async fn a_grill_intent_question_waits() {
            let _env = crate::test_env_lock();
            let (broker, rx, deliveries) = broker_with_sink();
            let mut input = question_input();
            input["metadata"] = json!({ "intent": "confirm_understanding" });
            assert_waits_for_the_dialog(broker, rx, Some(deliveries), input, question_context())
                .await;
        }

        #[tokio::test]
        async fn an_ultraplan_question_waits() {
            let _env = crate::test_env_lock();
            let (broker, rx, deliveries) = broker_with_sink();
            let ultraplan = rebon_types::UltraplanContext::planning_turn(
                "run",
                "plan",
                rebon_types::PolicyMode::Enforce,
            );
            let context = question_context()
                .with_execution_policy(rebon_types::ExecutionPolicy::ultraplan(ultraplan));
            assert_waits_for_the_dialog(broker, rx, Some(deliveries), question_input(), context)
                .await;
        }

        #[tokio::test]
        async fn a_sub_agent_question_waits() {
            let _env = crate::test_env_lock();
            let (broker, rx, deliveries) = broker_with_sink();
            let context = question_context().with_agent_id("worker-1");
            assert_waits_for_the_dialog(broker, rx, Some(deliveries), question_input(), context)
                .await;
        }

        #[tokio::test]
        async fn a_context_without_a_tool_resolver_waits() {
            let _env = crate::test_env_lock();
            let (broker, rx, deliveries) = broker_with_sink();
            let context = ToolContext::new().with_tool_use_id("toolu_q");
            assert_waits_for_the_dialog(broker, rx, Some(deliveries), question_input(), context)
                .await;
        }

        #[tokio::test]
        async fn switching_the_feature_off_makes_questions_wait() {
            let _env = crate::test_env_lock();
            let previous = std::env::var_os(DEFERRED_QUESTIONS_ENV);
            std::env::set_var(DEFERRED_QUESTIONS_ENV, "0");
            let (broker, rx, deliveries) = broker_with_sink();
            assert_waits_for_the_dialog(
                broker,
                rx,
                Some(deliveries),
                question_input(),
                question_context(),
            )
            .await;
            match previous {
                Some(value) => std::env::set_var(DEFERRED_QUESTIONS_ENV, value),
                None => std::env::remove_var(DEFERRED_QUESTIONS_ENV),
            }
        }

        /// Only `AskUserQuestion` defers; any other ask holds its call even
        /// with a sink installed.
        #[tokio::test]
        async fn other_tools_still_wait_with_a_sink_installed() {
            let _env = crate::test_env_lock();
            let (broker, mut rx, mut deliveries) = broker_with_sink();
            let context = ToolContext::new()
                .with_tool_use_id("t-echo")
                .with_tool_resolver(Arc::new(OneTool(Arc::new(EchoTool))));
            let decision = PermissionDecision::ask(
                PermissionRequest::new("Test", "Approve?")
                    .with_options(["allow_once", "reject_once"]),
                None,
            );
            let handle = tokio::spawn(async move {
                broker
                    .resolve(&EchoTool, json!({ "x": 1 }), &context, decision)
                    .await
            });
            let query = rx.recv().await.unwrap();
            assert!(!handle.is_finished());
            answer_with(
                query,
                PermissionAnswer::Selected {
                    option_id: "allow_once".into(),
                    updated_input: None,
                    extra_text: None,
                },
            );
            assert_eq!(handle.await.unwrap().unwrap(), json!({ "x": 1 }));
            assert!(deliveries.try_recv().is_err());
        }
    }
}
