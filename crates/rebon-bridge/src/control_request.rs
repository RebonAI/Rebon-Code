//! Pure-logic planner for server-initiated control requests.
//!
//! Handling a control request is two jobs:
//!
//! 1. Decide what `control_response` shape answers a given request subtype —
//!    possibly shadowed by outbound-only mode — from a verdict the caller has
//!    already reached.
//! 2. Apply the change, write the response to the transport and log the
//!    outcome.
//!
//! This module owns **only** the first. The caller routes the plan it returns
//! to its transport, its logger and its `session_id` wrapper; the I/O side is
//! caller-owned.
//!
//! ## Nothing answers success on its own
//!
//! Every request that changes something — `set_model`,
//! `set_max_thinking_tokens`, `set_permission_mode`, `interrupt` — is
//! answered from a [`ControlVerdict`] the caller supplies after it has
//! actually tried the change. No verdict means the caller cannot do it,
//! and the answer is an error. A success says **when** the change takes
//! effect, because for a running agent that is part of the truth:
//!
//! ```json
//! {"type": "control_response",
//!  "response": {"subtype": "success", "request_id": "r1",
//!               "response": {"applies": "next_turn"}}}
//! ```
//!
//! | `applies` | Meaning |
//! |---|---|
//! | `now` | already in force — an interrupt that stopped the turn, a permission mode the next tool call will see |
//! | `next_turn` | accepted and stored, used from the next turn on — Rebon's model and effort |
//!
//! A Rebon runner has no thinking-token setting, so it passes no verdict for
//! `set_max_thinking_tokens` and the controller is told as much. `initialize`
//! is not a change and keeps its capabilities body.
//!
//! Every caller-supplied verdict arrives as an *input* to the planner. That
//! keeps a closure out of the state-machine layer and makes the test matrix
//! exhaustive over the boolean and verdict states.

use serde::{Deserialize, Serialize};

use crate::constants::OUTBOUND_ONLY_ERROR;

/// Parsed `request.subtype` from an inbound control_request.
///
/// Five well-known subtypes are recognized; any other value is
/// treated as an error, which [`ServerControlRequestSubtype::Other`]
/// preserves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerControlRequestSubtype {
    /// `initialize` — reply with the minimal capabilities block.
    Initialize,
    /// `set_model` — change the model; answered from the caller's verdict.
    SetModel,
    /// `set_max_thinking_tokens` — change the thinking budget; answered
    /// from the caller's verdict.
    SetMaxThinkingTokens,
    /// `set_permission_mode` — change the permission mode; answered from
    /// the caller's verdict.
    SetPermissionMode,
    /// `interrupt` — stop the running turn; answered from the caller's
    /// verdict.
    Interrupt,
    /// Anything else — reply with the "bridge doesn't handle subtype"
    /// error string.
    Other(String),
}

impl ServerControlRequestSubtype {
    /// Parse a wire subtype string into the enum variant.
    pub fn parse(subtype: &str) -> Self {
        match subtype {
            "initialize" => Self::Initialize,
            "set_model" => Self::SetModel,
            "set_max_thinking_tokens" => Self::SetMaxThinkingTokens,
            "set_permission_mode" => Self::SetPermissionMode,
            "interrupt" => Self::Interrupt,
            other => Self::Other(other.to_string()),
        }
    }

    /// Wire-form name, used for error messages and log lines.
    pub fn wire_name(&self) -> &str {
        match self {
            Self::Initialize => "initialize",
            Self::SetModel => "set_model",
            Self::SetMaxThinkingTokens => "set_max_thinking_tokens",
            Self::SetPermissionMode => "set_permission_mode",
            Self::Interrupt => "interrupt",
            Self::Other(name) => name,
        }
    }

    /// Whether this subtype changes something and so needs a
    /// [`ControlVerdict`].
    pub fn needs_verdict(&self) -> bool {
        matches!(
            self,
            Self::SetModel | Self::SetMaxThinkingTokens | Self::SetPermissionMode | Self::Interrupt
        )
    }
}

/// When an accepted change takes effect. Wire values `now` and
/// `next_turn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEffect {
    /// Already in force.
    Now,
    /// Accepted; used from the next turn on.
    NextTurn,
}

/// What the caller made of a request that changes something, reached by
/// actually trying the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlVerdict {
    /// The change was accepted and takes effect as stated.
    Applied(ControlEffect),
    /// The change was refused — forward this error string.
    Rejected(String),
}

/// Success body of a response to a request that changed something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlApplied {
    /// When the change takes effect.
    pub applies: ControlEffect,
}

/// Inputs the caller assembles before asking for a response plan.
///
/// Every caller-side decision arrives here as a pre-computed value
/// so the planner stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerControlRequestPlanInput<'a> {
    /// Wire request id (echoed unchanged in the response).
    pub request_id: &'a str,
    /// Parsed subtype.
    pub subtype: ServerControlRequestSubtype,
    /// When true, the session is outbound-only and must reject all
    /// mutable requests before switching on subtype. `initialize` is
    /// still handled normally (the server kills the connection
    /// otherwise).
    pub outbound_only: bool,
    /// The caller's verdict, consulted for every subtype that
    /// [needs one](ServerControlRequestSubtype::needs_verdict). `None`
    /// means the caller cannot perform the request at all, which is
    /// answered with [`unsupported_control_error`].
    pub verdict: Option<ControlVerdict>,
    /// Only consulted for [`ServerControlRequestSubtype::Initialize`].
    /// This process's id — embedded in the initialize response.
    pub pid: u32,
}

/// The error text for a request the caller gave no verdict for.
pub fn unsupported_control_error(subtype: &str) -> String {
    format!("{subtype} is not supported by this bridge")
}

/// Minimal body sent with an `initialize` success response.
///
/// `Serialize` only: this is the body a bridge *emits*, and
/// [`crate::session_stream::ControlResponseBody`] carries it as opaque
/// JSON in the `response` slot it shares with every other success
/// payload. Giving it a `Deserialize` would mean owning its strings for
/// the sake of a direction nothing reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitializeResponseBody {
    /// Slash commands known to this bridge — always empty (the REPL
    /// owns its own command registry).
    pub commands: Vec<String>,
    /// Current output style; always `"normal"`.
    pub output_style: &'static str,
    /// Available output styles; always `["normal"]`.
    pub available_output_styles: Vec<&'static str>,
    /// Models known to this bridge — always empty.
    pub models: Vec<String>,
    /// Process identifier from the caller.
    pub pid: u32,
}

impl InitializeResponseBody {
    fn new(pid: u32) -> Self {
        Self {
            commands: Vec::new(),
            output_style: "normal",
            available_output_styles: vec!["normal"],
            models: Vec::new(),
            pid,
        }
    }
}

/// Response the planner chose.
///
/// Callers translate this to wire format (`{ type: 'control_response',
/// response: { subtype, request_id, ... } }`) — see
/// [`crate::session_stream::ControlResponseBody`]'s `From` impl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerControlResponsePlan {
    /// A change was made; the body is a [`ControlApplied`].
    SuccessApplied {
        /// Echoed `request_id`.
        request_id: String,
        /// When the change takes effect.
        effect: ControlEffect,
    },
    /// Success for the `initialize` subtype, which carries a body.
    SuccessInitialize {
        /// Echoed `request_id`.
        request_id: String,
        /// The minimal capabilities block.
        body: InitializeResponseBody,
    },
    /// Error response.
    Error {
        /// Echoed `request_id`.
        request_id: String,
        /// Human-visible error string.
        error: String,
    },
}

impl ServerControlResponsePlan {
    /// The subtype (`"success"` or `"error"`) the wire response will
    /// have. Convenience for log lines that report the outcome as a
    /// `result=...` suffix.
    pub fn outcome_label(&self) -> &'static str {
        match self {
            Self::SuccessApplied { .. } | Self::SuccessInitialize { .. } => "success",
            Self::Error { .. } => "error",
        }
    }
}

/// Decide what to reply to a server-initiated control_request.
///
/// Pure function — no I/O, no hidden state.
pub fn plan_server_control_response(
    input: ServerControlRequestPlanInput<'_>,
) -> ServerControlResponsePlan {
    let ServerControlRequestPlanInput {
        request_id,
        subtype,
        outbound_only,
        verdict,
        pid,
    } = input;
    let request_id = request_id.to_string();

    // Outbound-only gate: every mutable request rejects with the
    // shared error string. `initialize` is the one exception — the
    // server kills the WS if it doesn't see a success reply.
    if outbound_only && !matches!(subtype, ServerControlRequestSubtype::Initialize) {
        return ServerControlResponsePlan::Error {
            request_id,
            error: OUTBOUND_ONLY_ERROR.to_string(),
        };
    }

    match subtype {
        ServerControlRequestSubtype::Initialize => ServerControlResponsePlan::SuccessInitialize {
            request_id,
            body: InitializeResponseBody::new(pid),
        },
        ServerControlRequestSubtype::Other(name) => ServerControlResponsePlan::Error {
            request_id,
            error: format!("REPL bridge does not handle control_request subtype: {name}"),
        },
        changing => match verdict {
            Some(ControlVerdict::Applied(effect)) => {
                ServerControlResponsePlan::SuccessApplied { request_id, effect }
            }
            Some(ControlVerdict::Rejected(error)) => {
                ServerControlResponsePlan::Error { request_id, error }
            }
            None => ServerControlResponsePlan::Error {
                request_id,
                error: unsupported_control_error(changing.wire_name()),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGING: [ServerControlRequestSubtype; 4] = [
        ServerControlRequestSubtype::SetModel,
        ServerControlRequestSubtype::SetMaxThinkingTokens,
        ServerControlRequestSubtype::SetPermissionMode,
        ServerControlRequestSubtype::Interrupt,
    ];

    fn base_input<'a>(
        request_id: &'a str,
        subtype: ServerControlRequestSubtype,
    ) -> ServerControlRequestPlanInput<'a> {
        ServerControlRequestPlanInput {
            request_id,
            subtype,
            outbound_only: false,
            verdict: None,
            pid: 4242,
        }
    }

    fn verdicts() -> [Option<ControlVerdict>; 4] {
        [
            None,
            Some(ControlVerdict::Applied(ControlEffect::Now)),
            Some(ControlVerdict::Applied(ControlEffect::NextTurn)),
            Some(ControlVerdict::Rejected("no such model".to_string())),
        ]
    }

    #[test]
    fn subtype_parse_covers_known_values() {
        assert_eq!(
            ServerControlRequestSubtype::parse("initialize"),
            ServerControlRequestSubtype::Initialize
        );
        assert_eq!(
            ServerControlRequestSubtype::parse("set_model"),
            ServerControlRequestSubtype::SetModel
        );
        assert_eq!(
            ServerControlRequestSubtype::parse("set_max_thinking_tokens"),
            ServerControlRequestSubtype::SetMaxThinkingTokens
        );
        assert_eq!(
            ServerControlRequestSubtype::parse("set_permission_mode"),
            ServerControlRequestSubtype::SetPermissionMode
        );
        assert_eq!(
            ServerControlRequestSubtype::parse("interrupt"),
            ServerControlRequestSubtype::Interrupt
        );
    }

    #[test]
    fn subtype_parse_preserves_unknown_in_other() {
        match ServerControlRequestSubtype::parse("eject_warp_core") {
            ServerControlRequestSubtype::Other(name) => assert_eq!(name, "eject_warp_core"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn wire_name_roundtrips_for_known_variants() {
        for raw in [
            "initialize",
            "set_model",
            "set_max_thinking_tokens",
            "set_permission_mode",
            "interrupt",
        ] {
            assert_eq!(ServerControlRequestSubtype::parse(raw).wire_name(), raw);
        }
    }

    #[test]
    fn wire_name_roundtrips_for_other_variant() {
        let s = ServerControlRequestSubtype::parse("custom");
        assert_eq!(s.wire_name(), "custom");
    }

    #[test]
    fn exactly_the_changing_subtypes_need_a_verdict() {
        for subtype in CHANGING {
            assert!(subtype.needs_verdict(), "{subtype:?}");
        }
        assert!(!ServerControlRequestSubtype::Initialize.needs_verdict());
        assert!(!ServerControlRequestSubtype::Other("x".into()).needs_verdict());
    }

    #[test]
    fn initialize_returns_success_with_body_including_pid() {
        // Whatever verdict is lying around, initialize ignores it.
        for verdict in verdicts() {
            let plan = plan_server_control_response(ServerControlRequestPlanInput {
                pid: 1234,
                verdict,
                ..base_input("req-1", ServerControlRequestSubtype::Initialize)
            });
            match plan {
                ServerControlResponsePlan::SuccessInitialize { request_id, body } => {
                    assert_eq!(request_id, "req-1");
                    assert_eq!(body.pid, 1234);
                    assert!(body.commands.is_empty());
                    assert_eq!(body.output_style, "normal");
                    assert_eq!(body.available_output_styles, vec!["normal"]);
                    assert!(body.models.is_empty());
                }
                other => panic!("expected SuccessInitialize, got {other:?}"),
            }
        }
    }

    /// The whole matrix for the four changing subtypes: the answer is
    /// exactly what the verdict says, and no verdict is an error.
    #[test]
    fn a_changing_request_is_answered_from_its_verdict_and_only_from_it() {
        for subtype in CHANGING {
            for verdict in verdicts() {
                let plan = plan_server_control_response(ServerControlRequestPlanInput {
                    verdict: verdict.clone(),
                    ..base_input("req", subtype.clone())
                });
                let expected = match verdict {
                    None => ServerControlResponsePlan::Error {
                        request_id: "req".into(),
                        error: unsupported_control_error(subtype.wire_name()),
                    },
                    Some(ControlVerdict::Applied(effect)) => {
                        ServerControlResponsePlan::SuccessApplied {
                            request_id: "req".into(),
                            effect,
                        }
                    }
                    Some(ControlVerdict::Rejected(error)) => ServerControlResponsePlan::Error {
                        request_id: "req".into(),
                        error,
                    },
                };
                assert_eq!(plan, expected, "{subtype:?}");
            }
        }
    }

    #[test]
    fn no_verdict_names_the_unsupported_subtype() {
        let plan = plan_server_control_response(base_input(
            "req-7",
            ServerControlRequestSubtype::SetMaxThinkingTokens,
        ));
        assert_eq!(
            plan,
            ServerControlResponsePlan::Error {
                request_id: "req-7".to_string(),
                error: "set_max_thinking_tokens is not supported by this bridge".to_string(),
            }
        );
    }

    #[test]
    fn other_subtype_returns_bridge_does_not_handle_error() {
        for verdict in verdicts() {
            let plan = plan_server_control_response(ServerControlRequestPlanInput {
                verdict,
                ..base_input(
                    "req-8",
                    ServerControlRequestSubtype::Other("custom_action".to_string()),
                )
            });
            match plan {
                ServerControlResponsePlan::Error { request_id, error } => {
                    assert_eq!(request_id, "req-8");
                    assert!(error.contains("REPL bridge does not handle"));
                    assert!(error.contains("custom_action"));
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }

    #[test]
    fn outbound_only_rejects_every_change_whatever_the_verdict() {
        // The gate runs before the subtype switch, so the verdict is not
        // consulted.
        for subtype in CHANGING
            .into_iter()
            .chain([ServerControlRequestSubtype::Other("anything".into())])
        {
            for verdict in verdicts() {
                let plan = plan_server_control_response(ServerControlRequestPlanInput {
                    outbound_only: true,
                    verdict,
                    ..base_input("req-9", subtype.clone())
                });
                assert_eq!(
                    plan,
                    ServerControlResponsePlan::Error {
                        request_id: "req-9".to_string(),
                        error: OUTBOUND_ONLY_ERROR.to_string()
                    },
                    "{subtype:?}"
                );
            }
        }
    }

    #[test]
    fn outbound_only_still_handles_initialize_with_success() {
        // initialize must still succeed even in outbound-only mode —
        // otherwise the server kills the WS.
        let plan = plan_server_control_response(ServerControlRequestPlanInput {
            outbound_only: true,
            pid: 9,
            ..base_input("req-init", ServerControlRequestSubtype::Initialize)
        });
        match plan {
            ServerControlResponsePlan::SuccessInitialize { request_id, body } => {
                assert_eq!(request_id, "req-init");
                assert_eq!(body.pid, 9);
            }
            other => panic!("expected SuccessInitialize, got {other:?}"),
        }
    }

    #[test]
    fn outcome_label_maps_variant_to_wire_subtype() {
        assert_eq!(
            ServerControlResponsePlan::SuccessApplied {
                request_id: "x".to_string(),
                effect: ControlEffect::Now,
            }
            .outcome_label(),
            "success"
        );
        assert_eq!(
            ServerControlResponsePlan::SuccessInitialize {
                request_id: "x".to_string(),
                body: InitializeResponseBody::new(1)
            }
            .outcome_label(),
            "success"
        );
        assert_eq!(
            ServerControlResponsePlan::Error {
                request_id: "x".to_string(),
                error: "e".to_string()
            }
            .outcome_label(),
            "error"
        );
    }

    #[test]
    fn effects_have_stable_wire_names() {
        assert_eq!(
            serde_json::to_value(ControlApplied {
                applies: ControlEffect::Now
            })
            .unwrap(),
            serde_json::json!({"applies": "now"})
        );
        assert_eq!(
            serde_json::to_value(ControlApplied {
                applies: ControlEffect::NextTurn
            })
            .unwrap(),
            serde_json::json!({"applies": "next_turn"})
        );
        assert_eq!(
            serde_json::from_str::<ControlApplied>(r#"{"applies":"next_turn"}"#).unwrap(),
            ControlApplied {
                applies: ControlEffect::NextTurn
            }
        );
        assert!(serde_json::from_str::<ControlApplied>(r#"{"applies":"later"}"#).is_err());
    }
}
