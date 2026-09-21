//! `escalation`: interactive questions and worker escalations on the process
//! tool seat.
//!
//! `AskUserQuestion` gathers user decisions through the permission broker. A
//! worker that cannot decide something on its own asks the coordinator with
//! `EscalateQuestion` and blocks; whoever can answer replies with
//! `ResolveEscalation`. All three tool implementations live here.
//!
//! The shared contracts stay in `rebon-tool`: `PermissionBroker` carries the
//! user dialog, while `EscalationRegistry`, `WorkerEscalationClient` and
//! `EscalationResolver` carry worker questions. `ToolContext` and the engine
//! use those contracts without depending on this plugin.
//!
//! Turning the plugin off removes all three tools from the seat. An already
//! pending question remains owned by its broker or registry and can still be
//! answered through the surface that owns it; no new tool calls can start.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod ask_user_question;
pub mod escalate_question;
pub mod resolve_escalation;

pub use ask_user_question::{AskUserQuestionTool, ASK_USER_QUESTION_TOOL_NAME};
pub use escalate_question::{EscalateQuestionTool, ESCALATE_QUESTION_TOOL_NAME};
pub use resolve_escalation::{ResolveEscalationTool, RESOLVE_ESCALATION_TOOL_NAME};

/// Stable id: the config key `plugins.escalation.enabled`.
pub const PLUGIN_ID: &str = "escalation";

const PROVIDER_ID: &str = "escalation";

/// The three tools, in registration order.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register them without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![
        Arc::new(EscalateQuestionTool) as Arc<dyn rebon_tool::Tool>,
        Arc::new(ResolveEscalationTool),
        Arc::new(AskUserQuestionTool),
    ]
}

pub struct EscalationPlugin;

impl Plugin for EscalationPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(EscalationPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Questions and escalations (AskUserQuestion, EscalateQuestion, ResolveEscalation)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::permission::{ChannelPermissionBroker, PermissionAnswer};
    use rebon_core::tool_seat::ToolSeat;
    use rebon_core::Engine;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{ToolContext, ToolResolver};
    use rebon_tools_core::ToolError;
    use serde_json::{json, Value};

    /// Stands in for the real `core-tools` seat plugin, which this crate does
    /// not depend on. All this plugin needs is the root seat.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[TOOL_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<ToolSeatService>(ToolSeat::new())
        }
    }

    fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(SeatPlugin))
    }

    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "test-seat",
            title: "Test seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_seat,
        },
        PLUGIN,
    ];

    fn plugin_engine(desired: DesiredSet) -> (Arc<Kernel>, Arc<PluginRegistry>, Engine) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&desired);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let engine = Engine::with_builtin_tools();
        assert!(engine.attach_upstream_tool_context(kernel.context().clone()));
        (kernel, registry, engine)
    }

    fn question_input() -> Value {
        json!({
            "questions": [{
                "question": "Continue?", "header": "Continue", "multiSelect": false,
                "options": [
                    {"label": "Yes", "description": "Continue", "preview": "Next step"},
                    {"label": "No", "description": "Stop"}
                ]
            }],
            "metadata": {"source": "escalation-regression"}
        })
    }

    /// The switch is the whole contract: enabled means the model can resolve
    /// all three tools, disabled means it cannot, and flipping back restores them.
    #[tokio::test]
    async fn the_switch_takes_the_escalation_tools_off_the_seat_and_puts_them_back() {
        let (kernel, registry, engine) = plugin_engine(DesiredSet::new());
        let seat: Arc<ToolSeat> = kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root");
        for enabled in [true, false, true] {
            let report = registry
                .set_enabled(PLUGIN_ID, enabled)
                .expect("feature switch");
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            for name in [
                ESCALATE_QUESTION_TOOL_NAME,
                RESOLVE_ESCALATION_TOOL_NAME,
                ASK_USER_QUESTION_TOOL_NAME,
            ] {
                assert_eq!(
                    seat.resolve(name, None).unwrap().is_some(),
                    enabled,
                    "{name}"
                );
                assert_eq!(engine.find_tool(name).is_some(), enabled, "{name}");
                assert_eq!(
                    engine
                        .tool_names()
                        .iter()
                        .filter(|candidate| candidate.as_str() == name)
                        .count(),
                    usize::from(enabled),
                    "{name} must not survive unload or duplicate on reload"
                );
            }
            let error = engine
                .invoke_tool(
                    ASK_USER_QUESTION_TOOL_NAME,
                    question_input(),
                    &ToolContext::new(),
                )
                .await
                .unwrap_err();
            if enabled {
                assert!(
                    matches!(error, ToolError::PermissionDenied { .. }),
                    "{error:?}"
                );
            } else {
                assert!(matches!(error, ToolError::UnknownTool { .. }), "{error:?}");
            }
        }
    }

    #[tokio::test]
    async fn bare_engines_and_disabled_startup_have_no_question_fallback() {
        let (_kernel, registry, engine) = plugin_engine(DesiredSet::new().with(PLUGIN_ID, false));
        for bare in [Engine::new(), Engine::with_builtin_tools(), engine] {
            assert!(bare.find_tool(ASK_USER_QUESTION_TOOL_NAME).is_none());
            assert!(!bare
                .tool_names()
                .iter()
                .any(|name| name == ASK_USER_QUESTION_TOOL_NAME));
            let error = bare
                .invoke_tool(
                    ASK_USER_QUESTION_TOOL_NAME,
                    question_input(),
                    &ToolContext::new(),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ToolError::UnknownTool { .. }), "{error:?}");
        }
        let report = registry
            .set_enabled(PLUGIN_ID, true)
            .expect("enable from disabled startup");
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(_kernel
            .context()
            .get::<ToolSeatService>()
            .unwrap()
            .resolve(ASK_USER_QUESTION_TOOL_NAME, None)
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn registered_question_preserves_broker_answers_errors_and_permission_denials() {
        let (_kernel, _registry, engine) = plugin_engine(DesiredSet::new());
        for case in [
            "answered",
            "rejected",
            "cancelled",
            "missing answers",
            "invalid answers",
        ] {
            let (broker, mut rx) = ChannelPermissionBroker::new("escalation-regression");
            let context = ToolContext::new().with_permission_broker(Arc::new(broker));
            let input = question_input();
            let response = async {
                let query = rx.recv().await.expect("the question must reach the broker");
                assert_eq!(query.tool_name, ASK_USER_QUESTION_TOOL_NAME);
                assert_eq!(query.tool_input.as_ref(), Some(&input));
                let mut answered = input.clone();
                answered["answers"] = json!({"Continue?": "Yes"});
                answered["annotations"] =
                    json!({"Continue?": {"notes": "Proceed", "preview": "Next step"}});
                let answer = match case {
                    "cancelled" => PermissionAnswer::Cancelled,
                    _ => PermissionAnswer::Selected {
                        option_id: if case == "rejected" {
                            "reject_once"
                        } else {
                            "allow_once"
                        }
                        .into(),
                        updated_input: Some(match case {
                            "missing answers" => input.clone(),
                            "invalid answers" => {
                                answered["answers"]["Continue?"] = json!(42);
                                answered
                            }
                            _ => answered,
                        }),
                        extra_text: None,
                    },
                };
                query
                    .response_tx
                    .send(answer)
                    .expect("invocation is waiting");
            };
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(
                    engine.invoke_tool(ASK_USER_QUESTION_TOOL_NAME, input.clone(), &context),
                    response
                )
            })
            .await
            .expect("question dispatch must finish");
            match case {
                "answered" => {
                    let output = result.unwrap();
                    assert_eq!(output["questions"], input["questions"]);
                    assert_eq!(output["answers"], json!({"Continue?": "Yes"}));
                    assert_eq!(
                        output["annotations"],
                        json!({"Continue?": {"notes": "Proceed", "preview": "Next step"}})
                    );
                }
                "rejected" | "cancelled" => assert!(
                    matches!(result, Err(ToolError::PermissionDenied { .. })),
                    "{case}: {result:?}"
                ),
                "missing answers" => assert!(
                    matches!(result, Err(ToolError::Execution { .. })),
                    "{result:?}"
                ),
                "invalid answers" => assert!(
                    matches!(
                        result,
                        Err(ToolError::InvalidInput {
                            error_code: Some(400),
                            ..
                        })
                    ),
                    "{result:?}"
                ),
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn invalid_question_arguments_fail_before_prompting() {
        let (_kernel, _registry, engine) = plugin_engine(DesiredSet::new());
        let (broker, mut rx) = ChannelPermissionBroker::new("invalid-questions");
        let context = ToolContext::new().with_permission_broker(Arc::new(broker));
        let mut duplicate = question_input();
        duplicate["questions"][0]["options"][1]["label"] = json!("Yes");
        let mut invalid_answers = question_input();
        invalid_answers["answers"] = json!({"Continue?": false});
        for input in [
            Value::Null,
            json!({}),
            json!({"questions": []}),
            json!({"questions": "invalid"}),
            duplicate,
            invalid_answers,
        ] {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                engine.invoke_tool(ASK_USER_QUESTION_TOOL_NAME, input, &context),
            )
            .await
            .expect("invalid input must fail without waiting for a broker answer");
            assert!(
                matches!(
                    result,
                    Err(ToolError::InvalidInput {
                        error_code: Some(400),
                        ..
                    })
                ),
                "{result:?}"
            );
            assert!(
                rx.try_recv().is_err(),
                "invalid input must not reach the broker"
            );
        }
    }
}
