//! Kernel-backed seat for a policy event: a request that comes back
//! with a verdict.
//!
//! The engine and its hosts reach a number of points where something is
//! about to happen and a feature outside the engine may have a say in it:
//! a tool is about to run, a prompt is about to be submitted, a turn is
//! about to be stopped. Until this seat those points each called one
//! hard-wired runtime (`rebon-hooks`) directly, in thirteen places, and
//! nothing else could ever answer them.
//!
//! ## What a policy event is
//!
//! A [`PolicyRequest`] carries the same [`HookEventPayload`] the hook
//! runtime has always carried, plus the invocation context and, for a
//! sub-agent, its id. Subscribers answer with a [`Verdict`]:
//!
//! * [`Verdict::Allow`] — no opinion, or approval with nothing to change.
//! * [`Verdict::Deny`] — a terminal refusal. Only a **gated** event has a
//!   place to put one (see [`PolicyClass`]).
//! * [`Verdict::Modify`] — approval, with [`HookEffect`]s to apply.
//!
//! The modification channel is deliberately `Vec<HookEffect>` rather than
//! a new patch type. `HookEffect` is already the full vocabulary of what a
//! subscriber may ask for, and [`crate::hooks`] already owns the
//! per-event projection that says what each effect means where it lands.
//! Reusing both is what makes "a configured hook behaves exactly as
//! before" a fact rather than a claim.
//!
//! ## Composition
//!
//! Subscribers run in [`Order`] (lower first, ties by id). Effects
//! accumulate in that order. An explicit [`Verdict::Deny`] from any
//! subscriber short-circuits: the ones after it are not asked.
//!
//! An effect-level block (`HookEffect::BlockToolCall`, `BlockStop`) does
//! **not** short-circuit here. That is still the emit site's projection to
//! decide, exactly as before this seat existed, so the effect list a
//! single subscriber produces is byte-for-byte what it was.
//!
//! ## Fail-closed
//!
//! A gated event whose subscriber times out or panics is **denied**. A
//! policy gate that goes quiet must not open. A notification event has no
//! refusal branch at its emit site, so pretending it could refuse would be
//! a lie: there a timeout or panic is logged and the remaining subscribers
//! still run.
//!
//! The per-subscriber budget ([`DEFAULT_POLICY_TIMEOUT`]) is a backstop
//! against a subscriber that never returns at all, not a replacement for
//! the timeouts a subscriber applies internally — the hook runtime's own
//! 60-second per-hook budget is unchanged and still reports an ordinary
//! execution error. Only "the future never came back" reaches this layer.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::FutureExt;
use rebon_hooks::{HookEffect, HookEvent, HookInvocationContext};
use rebon_kernel::{Context, Disposer, KernelError, Service};

use crate::turn_hook::Order;

pub const POLICY_EVENT_SEAT_SERVICE: &str = "policy-events";

/// Subscriber id of the settings-driven hook runtime — the first
/// subscriber, and the one that carries every hook a user configured.
pub const SETTINGS_HOOKS_SUBSCRIBER_ID: &str = "core/settings-hooks";

/// Backstop budget for one subscriber's answer.
///
/// Five minutes, not the hook runtime's sixty seconds: one subscriber is
/// the *whole* hook runtime, which may run several hooks in turn, each
/// with its own sixty-second budget. The backstop has to be larger than
/// the worst legitimate configuration, because everything past that point
/// is a subscriber that is never coming back — and hanging forever is
/// worse than refusing.
pub const DEFAULT_POLICY_TIMEOUT: Duration = Duration::from_secs(300);

/// The kind of a policy event — the canonical hook event taxonomy.
///
/// The enum itself stays in `rebon-hooks` (it is a plain data type with no
/// behaviour, and this crate already depends on that one). What decides
/// the taxonomy — which kinds gate, what a terminal answer is, how several
/// answers compose — lives here.
pub use rebon_hooks::HookEvent as PolicyEventKind;

/// What an event carries, what a subscriber may ask for, and where the
/// event happened.
///
/// Re-exported so an emit site or a subscriber names the seat rather than
/// the runtime that happens to define the types. It is not a decoupling —
/// every caller already depends on this crate — it is so one place says
/// what the vocabulary of a policy event is.
pub use rebon_hooks::{
    HookEffect as PolicyEffect, HookEventPayload, HookInvocationContext as PolicyContext,
};

/// Whether a refusal has anywhere to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyClass {
    /// The emit site has a "don't do it" branch. A [`Verdict::Deny`] is
    /// honoured, and a subscriber that times out or panics denies.
    Gated,
    /// The emit site has no refusal branch. [`Verdict::Modify`] still
    /// applies; a `Deny` is logged and ignored, and a timeout or panic
    /// costs only that subscriber's contribution.
    Notification,
}

/// Classify one event.
///
/// The rule is "does the emit site have a branch a refusal can reach".
/// Four events answer yes today. Four more are gated by intent but have
/// no trigger point at all yet (`PreCompact`, `ConfigChange`,
/// `Elicitation`, `WorktreeCreate` — each blocks in the effects model);
/// classifying them here rather than when they are wired keeps one table
/// instead of two, and costs nothing while they never fire.
pub const fn class_of(kind: PolicyEventKind) -> PolicyClass {
    match kind {
        HookEvent::PreToolUse
        | HookEvent::UserPromptSubmit
        | HookEvent::PermissionRequest
        | HookEvent::Stop
        | HookEvent::PreCompact
        | HookEvent::ConfigChange
        | HookEvent::Elicitation
        | HookEvent::WorktreeCreate => PolicyClass::Gated,
        HookEvent::PostToolUse
        | HookEvent::PostToolUseFailure
        | HookEvent::Notification
        | HookEvent::SessionStart
        | HookEvent::SessionEnd
        | HookEvent::StopFailure
        | HookEvent::SubagentStart
        | HookEvent::SubagentStop
        | HookEvent::PostCompact
        | HookEvent::PermissionDenied
        | HookEvent::Setup
        | HookEvent::TeammateIdle
        | HookEvent::TaskCreated
        | HookEvent::TaskCompleted
        | HookEvent::ElicitationResult
        | HookEvent::WorktreeRemove
        | HookEvent::InstructionsLoaded
        | HookEvent::CwdChanged
        | HookEvent::FileChanged
        | HookEvent::Onboarding => PolicyClass::Notification,
    }
}

/// One thing that is about to happen, put to whoever has a say in it.
#[derive(Debug, Clone)]
pub struct PolicyRequest {
    /// The event and everything it carries.
    pub payload: HookEventPayload,
    /// Where it happened: cwd, transcript path, session id.
    pub context: HookInvocationContext,
    /// The sub-agent it happened in. `None` is the session's own turn.
    pub agent: Option<String>,
}

impl PolicyRequest {
    pub fn new(context: HookInvocationContext, payload: HookEventPayload) -> Self {
        Self {
            payload,
            context,
            agent: None,
        }
    }

    /// Mark this request as raised inside a sub-agent's turn.
    pub fn in_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    pub fn kind(&self) -> PolicyEventKind {
        self.payload.event()
    }

    pub fn class(&self) -> PolicyClass {
        class_of(self.kind())
    }

    /// The value a subscriber's matcher is tested against — tool name,
    /// source, reason, phase — derived from the payload itself so the
    /// seat never carries a second copy of it.
    pub fn matcher_value(&self) -> Option<&str> {
        self.payload.matcher_value()
    }
}

/// What one subscriber, or the whole seat, decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// No opinion, or approval with nothing to change.
    Allow,
    /// Terminal refusal. Honoured only on a [`PolicyClass::Gated`] event.
    Deny { reason: String },
    /// Approval, with these applied in subscriber order.
    Modify { effects: Vec<HookEffect> },
}

impl Verdict {
    /// The effects to apply. A denial contributes none: a refused request
    /// is not also modified.
    pub fn effects(&self) -> &[HookEffect] {
        match self {
            Verdict::Modify { effects } => effects,
            Verdict::Allow | Verdict::Deny { .. } => &[],
        }
    }

    /// The refusal reason, when this is one.
    pub fn denial(&self) -> Option<&str> {
        match self {
            Verdict::Deny { reason } => Some(reason.as_str()),
            _ => None,
        }
    }

    /// Build the terminal verdict for an accumulated effect list.
    fn from_effects(effects: Vec<HookEffect>) -> Self {
        if effects.is_empty() {
            Verdict::Allow
        } else {
            Verdict::Modify { effects }
        }
    }
}

pub type PolicyFuture<'a> = Pin<Box<dyn Future<Output = Verdict> + Send + 'a>>;

/// One party's say in a policy event.
pub trait PolicySubscriber: Send + Sync {
    /// Which kinds this subscriber wants to be asked about. A subscriber
    /// that says no here is not called at all — cheaper than deciding
    /// `Allow` inside a future, and it keeps a slow subscriber off events
    /// it has no opinion on.
    fn interest(&self, _kind: PolicyEventKind) -> bool {
        true
    }

    /// This subscriber's answer budget. `None` takes
    /// [`DEFAULT_POLICY_TIMEOUT`].
    fn budget(&self) -> Option<Duration> {
        None
    }

    fn decide<'a>(&'a self, request: &'a PolicyRequest) -> PolicyFuture<'a>;
}

#[derive(Clone)]
struct PolicyEntry {
    id: String,
    order: Order,
    active: Arc<AtomicBool>,
    subscriber: Arc<dyn PolicySubscriber>,
}

/// Typed definition for the kernel's `policy-events` seat.
pub struct PolicyEventSeatService;

impl Service for PolicyEventSeatService {
    type Interface = PolicyEventSeat;
    const NAME: &'static str = POLICY_EVENT_SEAT_SERVICE;
}

/// Process-level registry of ordered policy subscribers.
///
/// Plugins register here once, on their own context, and stay registered
/// for as long as the plugin is loaded. Per-session subscribers — the
/// settings hook runtime, whose configuration is a session's cwd —
/// belong in [`PolicySources::local`] instead, so two sessions sharing a
/// process never answer each other's events.
#[derive(Default)]
pub struct PolicyEventSeat {
    entries: RwLock<Vec<PolicyEntry>>,
}

impl PolicyEventSeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a subscriber and return its explicit unsubscription
    /// handle. Ids are unique within one seat.
    pub fn subscribe(
        self: &Arc<Self>,
        id: &str,
        order: Order,
        subscriber: Arc<dyn PolicySubscriber>,
    ) -> Result<Disposer, KernelError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(KernelError::Other(
                "policy-events subscriber id must be non-empty".into(),
            ));
        }
        let active = Arc::new(AtomicBool::new(true));
        {
            let mut entries = self.entries.write().expect("policy event seat poisoned");
            if entries.iter().any(|entry| entry.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: format!("{POLICY_EVENT_SEAT_SERVICE}:{id}"),
                });
            }
            entries.push(PolicyEntry {
                id: id.to_string(),
                order,
                active: active.clone(),
                subscriber,
            });
            entries.sort_by(|left, right| {
                left.order
                    .cmp(&right.order)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }

        let weak = Arc::downgrade(self);
        let id_for_dispose = id.to_string();
        let registered = active.clone();
        Ok(Disposer::new(move || {
            active.store(false, Ordering::Release);
            if let Some(seat) = weak.upgrade() {
                seat.entries
                    .write()
                    .expect("policy event seat poisoned")
                    .retain(|entry| {
                        entry.id != id_for_dispose || !Arc::ptr_eq(&entry.active, &registered)
                    });
            }
        }))
    }

    /// Register a subscriber as an effect of `ctx`, so unloading the
    /// plugin that registered it takes it off the seat.
    pub fn subscribe_scoped(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        order: Order,
        subscriber: Arc<dyn PolicySubscriber>,
    ) -> Result<(), KernelError> {
        let disposer = self.subscribe(id, order, subscriber)?;
        ctx.effect_labeled(&format!("policy event({id})"), || disposer);
        Ok(())
    }

    pub fn subscriber_ids(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("policy event seat poisoned")
            .iter()
            .filter(|entry| entry.active.load(Ordering::Acquire))
            .map(|entry| entry.id.clone())
            .collect()
    }

    fn snapshot(&self) -> Vec<PolicyEntry> {
        self.entries
            .read()
            .expect("policy event seat poisoned")
            .clone()
    }
}

/// The subscribers in force for one query: the process seat plus this
/// session's own.
///
/// Shaped after [`crate::turn_hook::TurnHooks`] for the same reason: the
/// seat is where a plugin registers once for the process, and `local` is
/// where a session puts a subscriber whose configuration is that session's
/// (its cwd, its transcript, its plugin hooks). Without the split, two
/// sessions in one `serve` process would answer each other's events.
#[derive(Clone, Default)]
pub struct PolicySources {
    seat: Option<Arc<PolicyEventSeat>>,
    local: Vec<PolicyEntry>,
    context: HookInvocationContext,
    agent: Option<String>,
}

/// Host-owned lookup of the handle a given session's turns raise events on,
/// keyed by session id and the request's immutable cwd.
///
/// One executor serving many sessions — `--acp` and the `serve` page behind
/// it — cannot hold a single [`PolicySources`], because the handle carries
/// *whose* session it is (cwd, transcript path, session id) and two sessions
/// would answer each other's events. The subscribers are the same for all of
/// them; only the context differs, and only the host knows it.
pub type PolicySourcesResolver = Arc<dyn Fn(&str, &str) -> PolicySources + Send + Sync>;

impl std::fmt::Debug for PolicySources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicySources")
            .field("seat", &self.seat.is_some())
            .field(
                "local",
                &self.local.iter().map(|e| &e.id).collect::<Vec<_>>(),
            )
            .field("session_id", &self.context.session_id)
            .field("agent", &self.agent)
            .finish()
    }
}

impl PolicySources {
    pub fn with_seat(mut self, seat: Arc<PolicyEventSeat>) -> Self {
        self.seat = Some(seat);
        self
    }

    /// Where events raised through this handle happen: cwd, transcript
    /// path, session id.
    ///
    /// The handle is the one place this is written down. A subscriber
    /// reads it off the request rather than keeping its own copy, so a
    /// session that rebinds to another directory cannot end up with two
    /// answers about where it is.
    pub fn with_context(mut self, context: HookInvocationContext) -> Self {
        self.context = context;
        self
    }

    pub fn context(&self) -> &HookInvocationContext {
        &self.context
    }

    /// Derive the handle a sub-agent's turn uses: the same subscribers
    /// and the same session, tagged with the agent it runs as.
    pub fn in_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    pub fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }

    /// Add a subscriber that lives exactly as long as this handle.
    pub fn with_subscriber(
        mut self,
        id: impl Into<String>,
        order: Order,
        subscriber: Arc<dyn PolicySubscriber>,
    ) -> Self {
        let id = id.into();
        assert!(
            !id.trim().is_empty(),
            "policy subscriber id must be non-empty"
        );
        self.local.push(PolicyEntry {
            id,
            order,
            active: Arc::new(AtomicBool::new(true)),
            subscriber,
        });
        self
    }

    pub fn has_seat(&self) -> bool {
        self.seat.is_some()
    }

    /// Whether anything at all would be asked. Emit sites use it to skip
    /// building a payload nobody will read.
    pub fn is_empty(&self) -> bool {
        self.local.is_empty()
            && self
                .seat
                .as_ref()
                .is_none_or(|seat| seat.snapshot().is_empty())
    }

    pub fn subscriber_ids(&self) -> Vec<String> {
        self.ordered().into_iter().map(|entry| entry.id).collect()
    }

    fn ordered(&self) -> Vec<PolicyEntry> {
        let mut entries = self
            .seat
            .as_ref()
            .map(|seat| seat.snapshot())
            .unwrap_or_default();
        entries.extend(self.local.iter().cloned());
        entries.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.id.cmp(&right.id))
        });
        entries
    }

    /// Put one event to every interested subscriber and return the
    /// terminal verdict.
    ///
    /// This is the one entry point. Every trigger point in the engine and
    /// its hosts goes through it; what a particular event means is the
    /// caller's projection of the effects that come back.
    pub async fn emit(&self, payload: HookEventPayload) -> Verdict {
        let mut request = PolicyRequest::new(self.context.clone(), payload);
        request.agent = self.agent.clone();
        self.emit_request(request).await
    }

    async fn emit_request(&self, request: PolicyRequest) -> Verdict {
        let kind = request.kind();
        let class = class_of(kind);
        let mut effects: Vec<HookEffect> = Vec::new();

        for entry in self.ordered() {
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            if !Self::interested(&entry, kind) {
                continue;
            }
            let budget = entry.subscriber.budget().unwrap_or(DEFAULT_POLICY_TIMEOUT);
            match Self::ask(&entry, &request, budget).await {
                Ok(Verdict::Allow) => {}
                Ok(Verdict::Modify {
                    effects: contributed,
                }) => effects.extend(contributed),
                Ok(Verdict::Deny { reason }) => match class {
                    PolicyClass::Gated => return Verdict::Deny { reason },
                    PolicyClass::Notification => tracing::warn!(
                        subscriber = %entry.id,
                        event = %kind.name(),
                        reason = %reason,
                        "policy subscriber denied a notification event; \
                         there is no refusal branch at this emit site, so it is ignored"
                    ),
                },
                Err(failure) => {
                    let reason = failure.deny_reason(&entry.id, kind, budget);
                    match class {
                        PolicyClass::Gated => {
                            tracing::error!(
                                subscriber = %entry.id,
                                event = %kind.name(),
                                "policy subscriber {failure} on a gated event; denying"
                            );
                            return Verdict::Deny { reason };
                        }
                        PolicyClass::Notification => tracing::warn!(
                            subscriber = %entry.id,
                            event = %kind.name(),
                            "policy subscriber {failure} on a notification event; \
                             its contribution is dropped and the rest still run"
                        ),
                    }
                }
            }
        }

        Verdict::from_effects(effects)
    }

    fn interested(entry: &PolicyEntry, kind: PolicyEventKind) -> bool {
        // `interest` is a subscriber's own code and may panic like any
        // other. A subscriber that cannot say whether it is interested is
        // not asked — on a gated event that is the safe direction only
        // because it removes an opinion, never a gate.
        catch_unwind(AssertUnwindSafe(|| entry.subscriber.interest(kind))).unwrap_or_else(|_| {
            tracing::error!(
                subscriber = %entry.id,
                "policy subscriber panicked deciding interest; skipped"
            );
            false
        })
    }

    async fn ask(
        entry: &PolicyEntry,
        request: &PolicyRequest,
        budget: Duration,
    ) -> Result<Verdict, SubscriberFailure> {
        // Two places a subscriber can panic: building the future, and
        // polling it. Both are isolated, and neither poisons the seat.
        let future = catch_unwind(AssertUnwindSafe(|| entry.subscriber.decide(request)))
            .map_err(|_| SubscriberFailure::Panicked)?;
        match tokio::time::timeout(budget, AssertUnwindSafe(future).catch_unwind()).await {
            Ok(Ok(verdict)) => Ok(verdict),
            Ok(Err(_)) => Err(SubscriberFailure::Panicked),
            Err(_) => Err(SubscriberFailure::TimedOut),
        }
    }
}

/// Why a subscriber produced no answer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriberFailure {
    TimedOut,
    Panicked,
}

impl SubscriberFailure {
    fn deny_reason(self, id: &str, kind: PolicyEventKind, budget: Duration) -> String {
        match self {
            SubscriberFailure::TimedOut => format!(
                "Policy subscriber `{id}` did not answer {} within {}s. \
                 A gate that goes quiet is refused, not opened.",
                kind.name(),
                budget.as_secs()
            ),
            SubscriberFailure::Panicked => format!(
                "Policy subscriber `{id}` panicked deciding {}. \
                 A gate that fails is refused, not opened.",
                kind.name()
            ),
        }
    }
}

impl std::fmt::Display for SubscriberFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriberFailure::TimedOut => f.write_str("timed out"),
            SubscriberFailure::Panicked => f.write_str("panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    fn pre_tool_use(tool: &str) -> HookEventPayload {
        HookEventPayload::PreToolUse {
            tool_name: tool.to_string(),
            tool_input: json!({}),
            tool_use_id: "t1".into(),
        }
    }

    fn session_start() -> HookEventPayload {
        HookEventPayload::SessionStart {
            source: "tui".into(),
            model: "m".into(),
        }
    }

    fn message(text: &str) -> HookEffect {
        HookEffect::SystemMessage { text: text.into() }
    }

    /// Answers with a fixed verdict and counts how many times it was asked.
    struct Fixed {
        verdict: Verdict,
        calls: Arc<AtomicUsize>,
        only: Option<PolicyEventKind>,
    }

    impl Fixed {
        fn new(verdict: Verdict) -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    verdict,
                    calls: calls.clone(),
                    only: None,
                }),
                calls,
            )
        }

        fn only(mut self: Arc<Self>, kind: PolicyEventKind) -> Arc<Self> {
            Arc::get_mut(&mut self).expect("unique").only = Some(kind);
            self
        }
    }

    impl PolicySubscriber for Fixed {
        fn interest(&self, kind: PolicyEventKind) -> bool {
            self.only.is_none_or(|only| only == kind)
        }

        fn decide<'a>(&'a self, _request: &'a PolicyRequest) -> PolicyFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let verdict = self.verdict.clone();
            Box::pin(async move { verdict })
        }
    }

    struct Hangs;

    impl PolicySubscriber for Hangs {
        fn budget(&self) -> Option<Duration> {
            Some(Duration::from_millis(20))
        }

        fn decide<'a>(&'a self, _request: &'a PolicyRequest) -> PolicyFuture<'a> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(3_600)).await;
                Verdict::Allow
            })
        }
    }

    /// Panics while being polled, not while building the future.
    struct PanicsInPoll;

    impl PolicySubscriber for PanicsInPoll {
        fn decide<'a>(&'a self, _request: &'a PolicyRequest) -> PolicyFuture<'a> {
            Box::pin(async { panic!("subscriber exploded") })
        }
    }

    /// Panics before it ever produces a future.
    struct PanicsUpFront;

    impl PolicySubscriber for PanicsUpFront {
        fn decide<'a>(&'a self, _request: &'a PolicyRequest) -> PolicyFuture<'a> {
            panic!("subscriber exploded up front")
        }
    }

    #[test]
    fn the_four_live_gated_events_are_the_ones_with_a_refusal_branch() {
        for kind in [
            HookEvent::PreToolUse,
            HookEvent::UserPromptSubmit,
            HookEvent::PermissionRequest,
            HookEvent::Stop,
        ] {
            assert_eq!(class_of(kind), PolicyClass::Gated, "{kind:?}");
        }
        for kind in [
            HookEvent::PostToolUse,
            HookEvent::PostToolUseFailure,
            HookEvent::PermissionDenied,
            HookEvent::SessionStart,
            HookEvent::SessionEnd,
            HookEvent::Onboarding,
            HookEvent::SubagentStart,
            HookEvent::SubagentStop,
        ] {
            assert_eq!(class_of(kind), PolicyClass::Notification, "{kind:?}");
        }
    }

    #[test]
    fn every_event_is_classified() {
        // The match in `class_of` is exhaustive, so this only guards the
        // count against a variant being added without a decision.
        assert_eq!(rebon_hooks::HOOK_EVENTS.len(), 28);
        for kind in rebon_hooks::HOOK_EVENTS {
            let _ = class_of(kind);
        }
    }

    #[tokio::test]
    async fn no_subscribers_allows() {
        let sources = PolicySources::default();
        assert!(sources.is_empty());
        assert_eq!(sources.emit(pre_tool_use("Bash")).await, Verdict::Allow);
    }

    #[tokio::test]
    async fn a_subscriber_that_allows_leaves_the_request_alone() {
        let (subscriber, calls) = Fixed::new(Verdict::Allow);
        let sources = PolicySources::default().with_subscriber("a", Order::NORMAL, subscriber);
        assert_eq!(sources.emit(pre_tool_use("Bash")).await, Verdict::Allow);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_subscriber_that_denies_ends_it_and_the_rest_are_not_asked() {
        let (denier, denier_calls) = Fixed::new(Verdict::Deny {
            reason: "no bash".into(),
        });
        let (later, later_calls) = Fixed::new(Verdict::Modify {
            effects: vec![message("too late")],
        });
        let sources = PolicySources::default()
            .with_subscriber("a-denier", Order::FIRST, denier)
            .with_subscriber("b-later", Order::LAST, later);

        let verdict = sources.emit(pre_tool_use("Bash")).await;
        assert_eq!(verdict.denial(), Some("no bash"));
        assert!(verdict.effects().is_empty(), "a denial carries no effects");
        assert_eq!(denier_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            later_calls.load(Ordering::SeqCst),
            0,
            "subscribers after a denial must not be asked"
        );
    }

    #[tokio::test]
    async fn modify_effects_accumulate_in_subscriber_order() {
        let (first, _) = Fixed::new(Verdict::Modify {
            effects: vec![message("first")],
        });
        let (second, _) = Fixed::new(Verdict::Modify {
            effects: vec![message("second-a"), message("second-b")],
        });
        // Registered in the reverse of the order they must run in, so the
        // assertion is about `Order` and not about insertion.
        let sources = PolicySources::default()
            .with_subscriber("z-second", Order::new(10), second)
            .with_subscriber("a-first", Order::new(-10), first);

        let verdict = sources.emit(pre_tool_use("Bash")).await;
        assert_eq!(
            verdict.effects(),
            &[message("first"), message("second-a"), message("second-b")]
        );
    }

    #[tokio::test]
    async fn equal_order_falls_back_to_the_subscriber_id() {
        let (b, _) = Fixed::new(Verdict::Modify {
            effects: vec![message("b")],
        });
        let (a, _) = Fixed::new(Verdict::Modify {
            effects: vec![message("a")],
        });
        let sources = PolicySources::default()
            .with_subscriber("b", Order::NORMAL, b)
            .with_subscriber("a", Order::NORMAL, a);
        assert_eq!(
            sources.emit(pre_tool_use("Bash")).await.effects(),
            &[message("a"), message("b")]
        );
    }

    #[tokio::test]
    async fn an_uninterested_subscriber_is_never_asked() {
        let (subscriber, calls) = Fixed::new(Verdict::Deny {
            reason: "would have denied".into(),
        });
        let sources = PolicySources::default().with_subscriber(
            "only-stop",
            Order::NORMAL,
            subscriber.only(HookEvent::Stop),
        );
        assert_eq!(sources.emit(pre_tool_use("Bash")).await, Verdict::Allow);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_gated_event_denies_when_a_subscriber_never_answers() {
        let sources = PolicySources::default().with_subscriber(
            "hangs",
            Order::NORMAL,
            Arc::new(Hangs) as Arc<dyn PolicySubscriber>,
        );
        let verdict = sources.emit(pre_tool_use("Bash")).await;
        let reason = verdict.denial().expect("a gate that goes quiet is refused");
        assert!(reason.contains("hangs"), "{reason}");
        assert!(reason.contains("PreToolUse"), "{reason}");
    }

    #[tokio::test]
    async fn a_notification_event_survives_a_subscriber_that_never_answers() {
        let (healthy, calls) = Fixed::new(Verdict::Modify {
            effects: vec![message("still here")],
        });
        let sources = PolicySources::default()
            .with_subscriber(
                "a-hangs",
                Order::FIRST,
                Arc::new(Hangs) as Arc<dyn PolicySubscriber>,
            )
            .with_subscriber("b-healthy", Order::LAST, healthy);

        let verdict = sources.emit(session_start()).await;
        assert_eq!(verdict.effects(), &[message("still here")]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_gated_event_denies_when_a_subscriber_panics_while_polled() {
        let sources = PolicySources::default().with_subscriber(
            "boom",
            Order::NORMAL,
            Arc::new(PanicsInPoll) as Arc<dyn PolicySubscriber>,
        );
        let reason = sources
            .emit(pre_tool_use("Bash"))
            .await
            .denial()
            .expect("a gate that fails is refused")
            .to_string();
        assert!(reason.contains("panicked"), "{reason}");
    }

    #[tokio::test]
    async fn a_gated_event_denies_when_a_subscriber_panics_before_returning_a_future() {
        let sources = PolicySources::default().with_subscriber(
            "boom",
            Order::NORMAL,
            Arc::new(PanicsUpFront) as Arc<dyn PolicySubscriber>,
        );
        assert!(sources.emit(pre_tool_use("Bash")).await.denial().is_some());
    }

    #[tokio::test]
    async fn a_panicking_subscriber_isolates_the_others_and_leaves_the_seat_usable() {
        let (healthy, calls) = Fixed::new(Verdict::Modify {
            effects: vec![message("unaffected")],
        });
        let sources = PolicySources::default()
            .with_subscriber(
                "a-boom",
                Order::FIRST,
                Arc::new(PanicsInPoll) as Arc<dyn PolicySubscriber>,
            )
            .with_subscriber("b-healthy", Order::LAST, healthy);

        assert_eq!(
            sources.emit(session_start()).await.effects(),
            &[message("unaffected")]
        );
        // The seat is not poisoned: a second emit behaves the same.
        assert_eq!(
            sources.emit(session_start()).await.effects(),
            &[message("unaffected")]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_denial_on_a_notification_event_is_logged_and_ignored() {
        let (denier, _) = Fixed::new(Verdict::Deny {
            reason: "nowhere to land".into(),
        });
        let (later, calls) = Fixed::new(Verdict::Modify {
            effects: vec![message("ran anyway")],
        });
        let sources = PolicySources::default()
            .with_subscriber("a-denier", Order::FIRST, denier)
            .with_subscriber("b-later", Order::LAST, later);

        let verdict = sources.emit(session_start()).await;
        assert_eq!(verdict.denial(), None);
        assert_eq!(verdict.effects(), &[message("ran anyway")]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_process_seat_and_the_session_subscribers_are_merged_by_order() {
        let seat = PolicyEventSeat::new();
        let (plugin, _) = Fixed::new(Verdict::Modify {
            effects: vec![message("plugin")],
        });
        let disposer = seat
            .subscribe("plugin/guard", Order::FIRST, plugin)
            .expect("fresh seat accepts a subscriber");
        let (session, _) = Fixed::new(Verdict::Modify {
            effects: vec![message("session")],
        });
        let sources = PolicySources::default()
            .with_seat(seat.clone())
            .with_subscriber(SETTINGS_HOOKS_SUBSCRIBER_ID, Order::NORMAL, session);

        assert_eq!(
            sources.subscriber_ids(),
            vec![
                "plugin/guard".to_string(),
                SETTINGS_HOOKS_SUBSCRIBER_ID.to_string()
            ]
        );
        assert_eq!(
            sources.emit(session_start()).await.effects(),
            &[message("plugin"), message("session")]
        );

        // Unsubscribing takes the plugin off; the session subscriber stays.
        disposer.dispose();
        assert!(seat.subscriber_ids().is_empty());
        assert_eq!(
            sources.emit(session_start()).await.effects(),
            &[message("session")]
        );
    }

    #[test]
    fn a_duplicate_subscriber_id_is_refused() {
        let seat = PolicyEventSeat::new();
        let (first, _) = Fixed::new(Verdict::Allow);
        let (second, _) = Fixed::new(Verdict::Allow);
        let _kept = seat.subscribe("same", Order::NORMAL, first).expect("first");
        assert!(seat.subscribe("same", Order::NORMAL, second).is_err());
    }

    #[test]
    fn an_empty_subscriber_id_is_refused() {
        let seat = PolicyEventSeat::new();
        let (subscriber, _) = Fixed::new(Verdict::Allow);
        assert!(seat.subscribe("   ", Order::NORMAL, subscriber).is_err());
    }

    #[test]
    fn a_scoped_subscription_leaves_when_its_context_is_disposed() {
        let kernel = rebon_kernel::Kernel::new();
        let ctx = kernel.context().fork_scoped("policy-test");
        let seat = PolicyEventSeat::new();
        let (subscriber, _) = Fixed::new(Verdict::Allow);
        seat.subscribe_scoped(&ctx, "scoped", Order::NORMAL, subscriber)
            .expect("scoped subscription");
        assert_eq!(seat.subscriber_ids(), vec!["scoped".to_string()]);
        ctx.dispose();
        assert!(seat.subscriber_ids().is_empty());
    }

    #[test]
    fn the_matcher_value_comes_from_the_payload() {
        let context = HookInvocationContext::default();
        assert_eq!(
            PolicyRequest::new(context.clone(), pre_tool_use("Bash")).matcher_value(),
            Some("Bash")
        );
        assert_eq!(
            PolicyRequest::new(context, session_start()).matcher_value(),
            Some("tui")
        );
    }

    /// A subscriber reads where it is off the request, so the handle's
    /// context and agent have to reach it.
    #[tokio::test]
    async fn the_handle_stamps_its_context_and_agent_onto_every_request() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<(String, Option<String>)>>);

        impl PolicySubscriber for Recorder {
            fn decide<'a>(&'a self, request: &'a PolicyRequest) -> PolicyFuture<'a> {
                self.0
                    .lock()
                    .expect("recorder poisoned")
                    .push((request.context.session_id.clone(), request.agent.clone()));
                Box::pin(async { Verdict::Allow })
            }
        }

        let recorder = Arc::new(Recorder::default());
        let session = PolicySources::default()
            .with_context(HookInvocationContext {
                session_id: "s-1".into(),
                ..Default::default()
            })
            .with_subscriber("rec", Order::NORMAL, recorder.clone());
        session.emit(pre_tool_use("Bash")).await;
        session
            .in_agent("explorer-1")
            .emit(pre_tool_use("Bash"))
            .await;

        assert_eq!(
            *recorder.0.lock().expect("recorder poisoned"),
            vec![
                ("s-1".to_string(), None),
                ("s-1".to_string(), Some("explorer-1".to_string()))
            ]
        );
    }
}
