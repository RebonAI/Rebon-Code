//! `core-tools`: the Core plugin that owns the process seats a feature
//! plugin registers onto.
//!
//! It provides the typed `tool-registry` seat on the kernel root and
//! registers the thirteen primitives on it (`rebon_tool::core_tool_set`).
//! Every turn's consumer seat registers the root seat as its upstream
//! (`Engine::scoped_tool_resolver`), so a tool a plugin registers here is
//! visible to every session without the engine keeping a list. Both
//! registrations are effects of the plugin's context: unloading the plugin
//! removes the seat and its tools, and reloading it builds fresh ones.
//!
//! It provides the `attachment-producers` seat the same way, and for the same
//! reason: a plugin that speaks into the model's history between tool rounds
//! (plan mode's reminders, the teammate mailbox, the task nudge, the skill
//! listing) registers a producer there, and a turn reads them off it through
//! its own kernel scope.
//!
//! It provides the `turn-hooks` seat too, and that one is what lets a plugin
//! watch a turn at all: the engine looks the seat up on the session's kernel
//! scope and builds its own only when it finds none, so a subscriber
//! registered on the kernel would never have been seen. The seat arrives with
//! the engine's own builtin subscribers already on it; a plugin adds its own
//! and takes it away again when it unloads.
//!
//! It provides the `permission-rules` seat on the same terms. A feature whose
//! tool must not have its permission decided for it — plan mode's pair —
//! registers a rule there, and both brokers read the seat per decision. The
//! seat starts empty, and an empty seat is the engine's own behaviour: a rule
//! can only ever add a prompt.
//!
//! It registers the two producers that are nobody's feature: `date_change`
//! and `runtime_prompts`. A calendar that rolled over and a prompt something
//! queued for this session belong to no plugin a user would switch, so the
//! producers stay in `rebon_core::attachments` and only their seat entries
//! live here. They are on the seat rather than wired into the query loop
//! because the seat is now the only path an attachment takes — the engine's
//! own session poller is gone.

use std::sync::Arc;

use rebon_core::attachment_seat::{
    AttachmentSeat, AttachmentSeatService, Order, ATTACHMENT_SEAT_SERVICE,
};
use rebon_core::permission_seat::{
    PermissionRuleSeat, PermissionRuleSeatService, PERMISSION_RULE_SEAT_SERVICE,
};
use rebon_core::policy_seat::{PolicyEventSeat, PolicyEventSeatService, POLICY_EVENT_SEAT_SERVICE};
use rebon_core::prompt_seat::{PromptSeat, PromptSeatService, PROMPT_SEAT_SERVICE};
use rebon_core::skill_seat::{SkillBundle, SkillSeat, SkillSeatService, SKILL_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeat, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_core::turn_hook::{TurnHookSeat, TurnHookSeatService, TURN_HOOK_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginMeta};

pub const CORE_TOOLS_PLUGIN_ID: &str = "core-tools";

/// The provider id the primitives are registered under on the process seat.
pub const CORE_PROVIDER_ID: &str = "core";

/// Attachment-seat provider id for the `date_change` producer.
pub const DATE_ROLL_PROVIDER_ID: &str = "core/date-roll";

/// Attachment-seat provider id for the `runtime_prompts` producer.
pub const RUNTIME_PROMPTS_PROVIDER_ID: &str = "core/runtime-prompts";

pub struct CoreToolsPlugin;

impl Plugin for CoreToolsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(CORE_TOOLS_PLUGIN_ID).provides(&[
            TOOL_SEAT_SERVICE,
            ATTACHMENT_SEAT_SERVICE,
            PERMISSION_RULE_SEAT_SERVICE,
            PROMPT_SEAT_SERVICE,
            TURN_HOOK_SEAT_SERVICE,
            POLICY_EVENT_SEAT_SERVICE,
            SKILL_SEAT_SERVICE,
        ])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        // The prompt-section seat: empty here, because the engine's own
        // sections are its table's and nobody's plugin. Feature plugins and
        // the Node composition register onto it.
        ctx.provide::<PromptSeatService>(PromptSeat::new())?;
        let attachments = AttachmentSeat::new();
        ctx.provide::<AttachmentSeatService>(attachments.clone())?;
        attachments.register(
            ctx,
            DATE_ROLL_PROVIDER_ID,
            Order::DayRoll,
            Arc::new(rebon_core::attachments::DateChangeProducer),
        )?;
        attachments.register(
            ctx,
            RUNTIME_PROMPTS_PROVIDER_ID,
            Order::Prompt,
            Arc::new(rebon_core::attachments::RuntimePromptsProducer),
        )?;
        ctx.provide::<PermissionRuleSeatService>(PermissionRuleSeat::new())?;
        // The turn-hook seat, which has to be one process-level registry and
        // not one per turn: a plugin subscribes once, and the executor
        // snapshots whatever is on it when a query starts. Without this
        // provision the executor falls back to a fresh seat per turn, which
        // carries the builtin subscribers and can never carry a plugin's.
        ctx.provide::<TurnHookSeatService>(TurnHookSeat::new())?;
        // The policy-event seat, process-level for the same reason: a plugin
        // subscribes once, and a session's emit handle merges whatever is on
        // it with the session's own subscribers when an event is raised.
        ctx.provide::<PolicyEventSeatService>(PolicyEventSeat::new())?;
        // The skill-bundle seat: empty here. A compiled feature plugin that
        // ships a skill registers its files on it, and a session's skill
        // index reads it when it loads.
        ctx.provide::<SkillSeatService>(SkillSeat::new())?;
        let seat = ToolSeat::new();
        ctx.provide::<ToolSeatService>(seat.clone())?;
        seat.register_tools(
            ctx,
            CORE_PROVIDER_ID,
            Priority::Core,
            rebon_tool::core_tool_set(rebon_tool::BashTool::new()),
        )
    }
}

/// The process attachment seat, when `core-tools` is loaded on `kernel`.
pub fn process_attachment_seat(
    kernel: &rebon_kernel::Kernel,
) -> Option<Arc<rebon_core::attachment_seat::AttachmentSeat>> {
    kernel.context().get::<AttachmentSeatService>()
}

/// The process policy-event seat, when `core-tools` is loaded on `kernel`.
pub fn process_policy_event_seat(
    kernel: &rebon_kernel::Kernel,
) -> Option<Arc<rebon_core::policy_seat::PolicyEventSeat>> {
    kernel.context().get::<PolicyEventSeatService>()
}

/// The skills compiled plugins registered on the process seat. Empty when
/// `core-tools` is not loaded — nothing could have registered one.
pub fn process_skill_bundles(kernel: &rebon_kernel::Kernel) -> Vec<SkillBundle> {
    kernel
        .context()
        .get::<SkillSeatService>()
        .map(|seat| seat.bundles())
        .unwrap_or_default()
}

/// The process turn-hook seat, when `core-tools` is loaded on `kernel`.
pub fn process_turn_hook_seat(kernel: &rebon_kernel::Kernel) -> Option<Arc<TurnHookSeat>> {
    kernel.context().get::<TurnHookSeatService>()
}

/// The process seat, when `core-tools` is loaded on `kernel`.
pub fn process_tool_seat(
    kernel: &rebon_kernel::Kernel,
) -> Option<Arc<dyn rebon_tool::ToolResolver>> {
    kernel
        .context()
        .get::<ToolSeatService>()
        .map(|seat| seat as Arc<dyn rebon_tool::ToolResolver>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::{DesiredSet, Kernel, PluginDef, PluginHost, PluginKind, PluginRegistry};

    fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(CoreToolsPlugin))
    }

    static DEFS: &[PluginDef] = &[PluginDef {
        id: CORE_TOOLS_PLUGIN_ID,
        title: "Core tools",
        kind: PluginKind::Core,
        default_enabled: true,
        factory: make,
    }];

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    /// Every root seat a feature plugin may `inject` is on the process
    /// kernel once `core-tools` has loaded.
    ///
    /// This is the list plugins declare against, so a seat missing from it
    /// is not a degraded feature: the plugin that injects it fails to load
    /// outright, and everything else it registers — its tools, its prompt
    /// sections, its dialogs — goes with it. `turn-hooks` was missing when
    /// the `memory` plugin first subscribed to it, and the symptom was
    /// `/memory` opening no browser.
    #[test]
    fn every_root_seat_is_on_the_process_kernel() {
        let (kernel, _registry) = boot();
        let ctx = kernel.context();
        assert!(
            ctx.get::<ToolSeatService>().is_some(),
            "{TOOL_SEAT_SERVICE}"
        );
        assert!(
            ctx.get::<AttachmentSeatService>().is_some(),
            "{ATTACHMENT_SEAT_SERVICE}"
        );
        assert!(
            ctx.get::<PermissionRuleSeatService>().is_some(),
            "{PERMISSION_RULE_SEAT_SERVICE}"
        );
        assert!(
            ctx.get::<PromptSeatService>().is_some(),
            "{PROMPT_SEAT_SERVICE}"
        );
        assert!(
            process_turn_hook_seat(&kernel).is_some(),
            "{TURN_HOOK_SEAT_SERVICE}"
        );
        assert!(
            process_policy_event_seat(&kernel).is_some(),
            "{POLICY_EVENT_SEAT_SERVICE}"
        );
        assert!(
            ctx.get::<SkillSeatService>().is_some(),
            "{SKILL_SEAT_SERVICE}"
        );
        assert!(
            process_skill_bundles(&kernel).is_empty(),
            "the seat starts empty"
        );
    }

    /// The turn-hook seat is one registry for the process, not one per
    /// turn: a subscription made once has to still be there on the seat the
    /// next turn snapshots.
    #[test]
    fn the_turn_hook_seat_keeps_a_subscription_across_lookups() {
        let (kernel, _registry) = boot();
        let seat = process_turn_hook_seat(&kernel).expect("core-tools provided the seat");
        struct Probe;
        impl rebon_core::turn_hook::TurnHook for Probe {
            fn on_event(
                &self,
                _event: &rebon_core::query::QueryEvent,
                _context: &mut rebon_core::turn_hook::TurnHookContext,
            ) {
            }
        }
        let disposer = seat
            .subscribe(
                "test/probe",
                rebon_core::turn_hook::Order::NORMAL,
                Arc::new(Probe),
            )
            .expect("a fresh id");
        assert!(process_turn_hook_seat(&kernel)
            .expect("still there")
            .subscriber_ids()
            .iter()
            .any(|id| id == "test/probe"));
        disposer.dispose();
        assert!(!process_turn_hook_seat(&kernel)
            .expect("still there")
            .subscriber_ids()
            .iter()
            .any(|id| id == "test/probe"));
    }

    /// The policy-event seat is one registry for the process too, and a
    /// session's emit handle merges whatever is on it when an event is
    /// raised — so a plugin's guard registered once has to still be there.
    #[tokio::test]
    async fn the_policy_event_seat_keeps_a_subscription_and_a_session_handle_reads_it() {
        use rebon_core::policy_seat::{
            HookEventPayload, PolicyFuture, PolicyRequest, PolicySources, PolicySubscriber, Verdict,
        };

        let (kernel, _registry) = boot();
        let seat = process_policy_event_seat(&kernel).expect("core-tools provided the seat");

        struct Refuses;
        impl PolicySubscriber for Refuses {
            fn decide<'a>(&'a self, _request: &'a PolicyRequest) -> PolicyFuture<'a> {
                Box::pin(async {
                    Verdict::Deny {
                        reason: "a plugin said no".into(),
                    }
                })
            }
        }

        let disposer = seat
            .subscribe(
                "test/refuses",
                rebon_core::turn_hook::Order::NORMAL,
                Arc::new(Refuses),
            )
            .expect("a fresh id");
        assert!(process_policy_event_seat(&kernel)
            .expect("still there")
            .subscriber_ids()
            .iter()
            .any(|id| id == "test/refuses"));

        // A session's handle, built the way a front end builds one.
        let session = PolicySources::default().with_seat(seat.clone());
        let verdict = session
            .emit(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "t1".into(),
            })
            .await;
        assert_eq!(verdict.denial(), Some("a plugin said no"));

        disposer.dispose();
        assert!(!process_policy_event_seat(&kernel)
            .expect("still there")
            .subscriber_ids()
            .iter()
            .any(|id| id == "test/refuses"));
        assert_eq!(
            session
                .emit(HookEventPayload::PreToolUse {
                    tool_name: "Bash".into(),
                    tool_input: serde_json::json!({}),
                    tool_use_id: "t1".into(),
                })
                .await,
            Verdict::Allow,
            "an unsubscribed guard stops being asked"
        );
    }

    /// Loaded: `Read` resolves through the root seat. Unloaded: the seat
    /// is gone from the root, and a resolver held across the unload refuses.
    #[test]
    fn read_resolves_while_loaded_and_not_after_unload() {
        let (kernel, _registry) = boot();
        let seat = process_tool_seat(&kernel).expect("core-tools provided the seat");
        let read = seat
            .resolve("Read", None)
            .unwrap()
            .expect("Read is a core tool");
        assert_eq!(read.id().as_str(), "Read");
        let names: Vec<String> = seat
            .tools(None)
            .unwrap()
            .iter()
            .map(|tool| tool.id().as_str().to_string())
            .collect();
        for primitive in [
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "ToolSearch",
            "Sleep",
        ] {
            assert!(names.iter().any(|name| name == primitive), "{primitive}");
        }

        assert!(
            kernel.unload(CORE_TOOLS_PLUGIN_ID),
            "core-tools unloads when nothing depends on it"
        );
        assert!(
            process_tool_seat(&kernel).is_none(),
            "the seat leaves the root with its plugin"
        );
        assert!(seat.resolve("Read", None).unwrap().is_none());
    }

    /// The prompt seat is core-tools', and a Rust plugin's provider reaches
    /// the engine's per-turn lookup through it with no Node plane anywhere
    /// — and leaves with the plugin's scope.
    #[test]
    fn a_rust_provider_reaches_the_prompt_seat_without_a_node_plane() {
        use rebon_core::prompt_seat::{
            sections_for, PluginPromptSection, PromptSeatService, PromptSectionProvider,
            PromptSubject, Rung,
        };
        let (kernel, _registry) = boot();
        let seat = kernel
            .context()
            .get::<PromptSeatService>()
            .expect("core-tools provided the seat");
        assert!(
            seat.provider_ids().is_empty(),
            "core-tools contributes no section"
        );

        let plugin = kernel.context().fork_scoped("plugin/probe");
        let provider: Arc<dyn PromptSectionProvider> = Arc::new(|subject: &PromptSubject| {
            vec![PluginPromptSection::new(
                "probe",
                Rung::Style,
                format!("style for {}", subject.model),
            )]
        });
        seat.register(&plugin, "probe", provider).unwrap();

        let session = kernel.context().fork_scoped("session/abc");
        let sections = sections_for(&session, &PromptSubject::new("claude-opus-5"));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rung, Rung::Style);
        assert_eq!(sections[0].text, "style for claude-opus-5");

        plugin.dispose();
        assert!(sections_for(&session, &PromptSubject::new("claude-opus-5")).is_empty());
    }

    /// An engine attached to this kernel sees the primitives through the
    /// seat, not through a list of its own.
    #[test]
    fn an_attached_engine_lists_the_primitives_from_the_seat() {
        let (kernel, _registry) = boot();
        let engine = rebon_core::Engine::new();
        engine.attach_upstream_tool_context(kernel.context().clone());
        let names = engine.tool_names();
        assert!(names.iter().any(|name| name == "Read"), "{names:?}");
        assert!(names.iter().any(|name| name == "Bash"), "{names:?}");
        assert_eq!(names.len(), seat_size(&kernel));
    }

    /// The two feature-less producers are core-tools', in rung order, and
    /// they leave the seat with their plugin like anything registered on a
    /// context.
    #[test]
    fn the_core_producers_are_on_the_attachment_seat_and_leave_with_the_plugin() {
        let (kernel, _registry) = boot();
        let seat = process_attachment_seat(&kernel).expect("core-tools provided the seat");
        assert_eq!(
            seat.provider_ids(),
            vec![
                DATE_ROLL_PROVIDER_ID.to_string(),
                RUNTIME_PROMPTS_PROVIDER_ID.to_string(),
            ]
        );

        // The rungs are the places these two held in the engine's original
        // fixed order: the date roll second of eight, the queued prompts
        // fifth.
        assert!(Order::DayRoll < Order::Listing);
        assert!(Order::Context < Order::Prompt);
        assert!(Order::Prompt < Order::Mailbox);

        assert!(kernel.unload(CORE_TOOLS_PLUGIN_ID));
        assert!(seat.provider_ids().is_empty());
    }

    fn seat_size(kernel: &Kernel) -> usize {
        process_tool_seat(kernel)
            .expect("seat")
            .tools(None)
            .unwrap()
            .len()
    }
}
