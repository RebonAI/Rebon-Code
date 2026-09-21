//! Kernel-backed seat for a feature's say in a permission decision.
//!
//! Most of what decides whether a tool call runs is general: the rule store,
//! the permission mode, the auto-mode classifier. A few tools are exceptions
//! that only their own feature can justify — a tool whose *entire output is a
//! user decision* has nothing left when the decision is skipped, so no mode
//! may resolve it on the user's behalf, and the options such a tool offers
//! mean something to the feature and nothing to the engine.
//!
//! Those exceptions used to be `matches!(tool_name, ...)` arms in
//! [`crate::permission`], which meant the engine knew the plan-mode pair by
//! name and carried its option truth table. This seat lets the feature say it
//! instead.
//!
//! Shape. A process-level registry like the tool and attachment seats: a
//! plugin registers once on its own context, and the rules are read per
//! decision. Registration is an effect of the registering context, so
//! unloading the plugin takes its rules off the seat and a session mid-turn
//! stops honouring them rather than outliving its provider.
//!
//! **Fail-closed on read.** A permission seat that goes quiet must not open a
//! gate. Every method here is *additive*: a rule can insist a call reaches a
//! human, never that one skips a prompt it would otherwise get. So the worst a
//! missing seat can do is ask more often than it needs to — never less. The
//! `dontAsk` mode's refusal is unchanged by this seat for the same reason: it
//! denies everything that would prompt, and a rule claiming a tool "must reach
//! a human" is another reason it prompts, so `dontAsk` still refuses it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebon_kernel::{Context, Disposer, KernelError, Service};
use rebon_tool::ToolContext;
use serde_json::Value;

pub const PERMISSION_RULE_SEAT_SERVICE: &str = "permission-rules";

/// How far a rule's "this call must reach a human" claim reaches.
///
/// Both variants are about *prompting* modes; neither touches `dontAsk`,
/// which refuses anything that would prompt and refuses these too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DecisionScope {
    /// Every mode that prompts at all — `default`, `acceptEdits`, and `auto`,
    /// whose classifier must not judge this call. `bypassPermissions` opted
    /// out of gates and still runs it.
    WhenPrompting,
    /// The above, and `bypassPermissions` too: a gate this call must not lose
    /// even under "no prompts". Reserve it for a decision whose absence would
    /// silently delete a workflow's only checkpoint.
    EvenUnderBypass,
}

/// What one rejected call should tell the model, past the shared opening
/// sentence the engine writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectionNote {
    /// Sentence appended after `The user chose "No, chat with this".`
    pub guidance: String,
    /// Label introducing the user's free text, when they typed any.
    pub feedback_label: String,
}

/// One feature's say in how its own tools' permissions are decided.
///
/// Every method defaults to "no opinion", so a rule states only the
/// exceptions it owns.
pub trait PermissionRule: Send + Sync {
    /// Whether this call must reach a human rather than be resolved by a
    /// mode, and how far that claim reaches. `None` is no opinion.
    ///
    /// The context is passed because the answer can depend on the run: the
    /// same tool may be an ordinary prompt normally and a workflow's only
    /// checkpoint inside a particular flow.
    fn requires_user_decision(
        &self,
        _tool_name: &str,
        _context: &ToolContext,
    ) -> Option<DecisionScope> {
        None
    }

    /// The label for an option this rule's tools offer. `None` leaves the
    /// engine's generic labelling alone.
    fn option_label(&self, _option_id: &str) -> Option<String> {
        None
    }

    /// Apply an approved option's effect to the call it authorised: rewrite
    /// the input, and hand back a context carrying whatever the approval
    /// means to the feature. `None` means the option is not this rule's, and
    /// the call proceeds untouched.
    fn on_approved(
        &self,
        _tool_name: &str,
        _option_id: &str,
        _input: &mut Value,
        _context: &ToolContext,
    ) -> Option<ToolContext> {
        None
    }

    /// What a rejection of this tool should tell the model. `None` leaves the
    /// engine's generic wording alone.
    fn rejection_note(&self, _tool_name: &str) -> Option<RejectionNote> {
        None
    }

    /// What the `dontAsk` refusal of this tool should say, past the generic
    /// "this mode never prompts". `None` leaves the engine's wording alone.
    ///
    /// This does not change the decision — a mode that never prompts refuses
    /// every call a rule claims, with or without an opinion here — only the
    /// sentence the model reads. A feature whose tool has a *different* thing
    /// the user can still do says so, so the model stops retrying a call the
    /// mode will refuse forever.
    fn deny_reason_when_never_prompting(&self, _tool_name: &str) -> Option<String> {
        None
    }
}

/// The rules in force for one decision.
///
/// Resolved off the seat once per ask and passed down, so the deep helpers
/// stay free functions over data rather than reaching into a kernel scope.
/// An empty set is the engine's own behaviour with no plugin loaded.
#[derive(Clone, Default)]
pub struct PermissionRules {
    rules: Vec<Arc<dyn PermissionRule>>,
}

impl PermissionRules {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The strongest claim any rule makes about this call.
    ///
    /// Strongest wins: one rule saying [`DecisionScope::EvenUnderBypass`]
    /// outranks another saying [`DecisionScope::WhenPrompting`], because the
    /// two are the same claim at different strengths and the safe fold of
    /// "must reach a human" is the wider one.
    pub fn requires_user_decision(
        &self,
        tool_name: &str,
        context: &ToolContext,
    ) -> Option<DecisionScope> {
        self.rules
            .iter()
            .filter_map(|rule| rule.requires_user_decision(tool_name, context))
            .max()
    }

    /// Whether this call must reach a human even in a mode that prompts for
    /// nothing else.
    pub fn requires_user_decision_under_bypass(
        &self,
        tool_name: &str,
        context: &ToolContext,
    ) -> bool {
        self.requires_user_decision(tool_name, context) == Some(DecisionScope::EvenUnderBypass)
    }

    /// The first rule that labels this option, if any.
    pub fn option_label(&self, option_id: &str) -> Option<String> {
        self.rules
            .iter()
            .find_map(|rule| rule.option_label(option_id))
    }

    /// Apply every rule's reading of an approved option. Returns the context
    /// the call should run under — the one handed in, when no rule claims the
    /// option.
    pub fn on_approved(
        &self,
        tool_name: &str,
        option_id: &str,
        input: &mut Value,
        context: &ToolContext,
    ) -> Option<ToolContext> {
        let mut current: Option<ToolContext> = None;
        for rule in &self.rules {
            let base = current.as_ref().unwrap_or(context);
            if let Some(next) = rule.on_approved(tool_name, option_id, input, base) {
                current = Some(next);
            }
        }
        current
    }

    /// The first rule with something to say about rejecting this tool.
    pub fn rejection_note(&self, tool_name: &str) -> Option<RejectionNote> {
        self.rules
            .iter()
            .find_map(|rule| rule.rejection_note(tool_name))
    }

    /// The first rule with its own wording for a `dontAsk` refusal of this
    /// tool.
    pub fn deny_reason_when_never_prompting(&self, tool_name: &str) -> Option<String> {
        self.rules
            .iter()
            .find_map(|rule| rule.deny_reason_when_never_prompting(tool_name))
    }
}

struct RuleEntry {
    id: String,
    token: u64,
    active: Arc<AtomicBool>,
    rule: Arc<dyn PermissionRule>,
}

/// Typed definition for the kernel's `permission-rules` seat.
pub struct PermissionRuleSeatService;

impl Service for PermissionRuleSeatService {
    type Interface = PermissionRuleSeat;
    const NAME: &'static str = PERMISSION_RULE_SEAT_SERVICE;
}

/// Rule registry behind the typed `permission-rules` service.
pub struct PermissionRuleSeat {
    entries: RwLock<Vec<RuleEntry>>,
    next_token: AtomicU64,
}

impl PermissionRuleSeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(1),
        })
    }

    /// Register `rule` on `ctx`. Disposing `ctx` takes it off the seat.
    /// `id` must be unique within the seat.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        rule: Arc<dyn PermissionRule>,
    ) -> Result<(), KernelError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(KernelError::Other(
                "permission-rules provider id must be non-empty".into(),
            ));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let active = Arc::new(AtomicBool::new(true));
        {
            let mut entries = self.entries.write().expect("permission rule seat poisoned");
            if entries.iter().any(|entry| entry.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: format!("{PERMISSION_RULE_SEAT_SERVICE}:{id}"),
                });
            }
            entries.push(RuleEntry {
                id: id.to_string(),
                token,
                active: active.clone(),
                rule,
            });
            entries.sort_by(|left, right| left.id.cmp(&right.id));
        }

        let weak = Arc::downgrade(self);
        let id_for_dispose = id.to_string();
        ctx.effect_labeled(&format!("permission rule({id})"), move || {
            Disposer::new(move || {
                active.store(false, Ordering::Release);
                if let Some(seat) = weak.upgrade() {
                    seat.entries
                        .write()
                        .expect("permission rule seat poisoned")
                        .retain(|entry| !(entry.id == id_for_dispose && entry.token == token));
                }
            })
        });
        Ok(())
    }

    /// Provider ids currently on the seat, in read order.
    pub fn provider_ids(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("permission rule seat poisoned")
            .iter()
            .filter(|entry| entry.active.load(Ordering::Acquire))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// The rules in force right now.
    pub fn rules(&self) -> PermissionRules {
        PermissionRules {
            rules: self
                .entries
                .read()
                .expect("permission rule seat poisoned")
                .iter()
                .filter(|entry| entry.active.load(Ordering::Acquire))
                .map(|entry| entry.rule.clone())
                .collect(),
        }
    }
}

/// The rules in force for `ctx`. A scope with no seat above it — a host with
/// no kernel, a test engine — has none, which is the engine's own behaviour.
pub fn rules_for(ctx: &Context) -> PermissionRules {
    match ctx.get::<PermissionRuleSeatService>() {
        Some(seat) => seat.rules(),
        None => PermissionRules::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    struct MustAsk {
        tool: &'static str,
        scope: DecisionScope,
    }

    impl PermissionRule for MustAsk {
        fn requires_user_decision(
            &self,
            tool_name: &str,
            _context: &ToolContext,
        ) -> Option<DecisionScope> {
            (tool_name == self.tool).then_some(self.scope)
        }
    }

    struct Rewriter;

    impl PermissionRule for Rewriter {
        fn on_approved(
            &self,
            tool_name: &str,
            option_id: &str,
            input: &mut Value,
            context: &ToolContext,
        ) -> Option<ToolContext> {
            if tool_name != "Thing" || option_id != "yes" {
                return None;
            }
            if let Some(object) = input.as_object_mut() {
                object.insert("marked".into(), Value::Bool(true));
            }
            Some(context.clone())
        }
    }

    fn context() -> ToolContext {
        ToolContext::default()
    }

    #[test]
    fn an_empty_set_has_no_opinion_about_anything() {
        let rules = PermissionRules::default();
        assert!(rules.is_empty());
        assert_eq!(rules.requires_user_decision("Thing", &context()), None);
        assert!(!rules.requires_user_decision_under_bypass("Thing", &context()));
        assert_eq!(rules.option_label("yes"), None);
        assert_eq!(rules.rejection_note("Thing"), None);
    }

    /// Two rules claiming the same tool at different strengths fold to the
    /// wider claim: "must reach a human" is safe to widen, never to narrow.
    #[test]
    fn the_strongest_claim_about_a_tool_wins() {
        let kernel = Kernel::new();
        let seat = PermissionRuleSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "narrow",
            Arc::new(MustAsk {
                tool: "Thing",
                scope: DecisionScope::WhenPrompting,
            }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "wide",
            Arc::new(MustAsk {
                tool: "Thing",
                scope: DecisionScope::EvenUnderBypass,
            }),
        )
        .unwrap();

        let rules = seat.rules();
        assert_eq!(
            rules.requires_user_decision("Thing", &context()),
            Some(DecisionScope::EvenUnderBypass)
        );
        assert!(rules.requires_user_decision_under_bypass("Thing", &context()));
        assert_eq!(rules.requires_user_decision("Other", &context()), None);
    }

    /// A rule that only prompts does not reach into `bypassPermissions`.
    #[test]
    fn a_when_prompting_claim_leaves_bypass_alone() {
        let kernel = Kernel::new();
        let seat = PermissionRuleSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "narrow",
            Arc::new(MustAsk {
                tool: "Thing",
                scope: DecisionScope::WhenPrompting,
            }),
        )
        .unwrap();

        let rules = seat.rules();
        assert_eq!(
            rules.requires_user_decision("Thing", &context()),
            Some(DecisionScope::WhenPrompting)
        );
        assert!(!rules.requires_user_decision_under_bypass("Thing", &context()));
    }

    #[test]
    fn an_approved_option_rewrites_the_input_of_the_rule_that_claims_it() {
        let kernel = Kernel::new();
        let seat = PermissionRuleSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(&ctx, "rewriter", Arc::new(Rewriter)).unwrap();
        let rules = seat.rules();

        let mut input = serde_json::json!({"plan": "x"});
        assert!(rules
            .on_approved("Thing", "yes", &mut input, &context())
            .is_some());
        assert_eq!(input["marked"], Value::Bool(true));

        let mut untouched = serde_json::json!({"plan": "x"});
        assert!(rules
            .on_approved("Other", "yes", &mut untouched, &context())
            .is_none());
        assert_eq!(untouched, serde_json::json!({"plan": "x"}));
    }

    /// Unloading the registering scope takes the rule off the seat, and a
    /// set already resolved for a decision in flight keeps what it had —
    /// which is safe, because every claim only adds a prompt.
    #[test]
    fn disposing_the_scope_takes_the_rule_off_the_seat() {
        let kernel = Kernel::new();
        let seat = PermissionRuleSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "plan-mode",
            Arc::new(MustAsk {
                tool: "Thing",
                scope: DecisionScope::WhenPrompting,
            }),
        )
        .unwrap();
        assert_eq!(seat.provider_ids(), vec!["plan-mode".to_string()]);

        ctx.dispose();

        assert!(seat.provider_ids().is_empty());
        assert!(seat.rules().is_empty());
    }

    #[test]
    fn a_duplicate_provider_id_is_refused() {
        let kernel = Kernel::new();
        let seat = PermissionRuleSeat::new();
        let ctx = kernel.context().fork("test");
        let rule = || {
            Arc::new(MustAsk {
                tool: "Thing",
                scope: DecisionScope::WhenPrompting,
            })
        };
        seat.register(&ctx, "dup", rule()).unwrap();
        assert!(seat.register(&ctx, "dup", rule()).is_err());
    }

    /// A scope with no seat above it answers "no rules" rather than failing
    /// the decision.
    #[test]
    fn a_scope_without_a_seat_contributes_no_rules() {
        let kernel = Kernel::new();
        assert!(rules_for(kernel.context()).is_empty());
    }

    /// The seat is found from a session scope forked under the one that
    /// provides it, both fork kinds — the shape the hosts hand the brokers.
    #[test]
    fn a_session_scope_under_the_providing_scope_sees_the_seat() {
        let kernel = Kernel::new();
        let seat = PermissionRuleSeat::new();
        kernel
            .context()
            .provide::<PermissionRuleSeatService>(seat.clone())
            .unwrap();
        seat.register(
            kernel.context(),
            "plan-mode",
            Arc::new(MustAsk {
                tool: "Thing",
                scope: DecisionScope::WhenPrompting,
            }),
        )
        .unwrap();

        let plain = kernel.context().fork("session/abc");
        assert!(!rules_for(&plain).is_empty());
        let scoped = kernel.context().fork_scoped("session/def");
        assert!(!rules_for(&scoped).is_empty());
    }
}
