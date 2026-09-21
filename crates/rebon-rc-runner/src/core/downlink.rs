//! From controller frames to what the runner does, and back to answers.
//!
//! | Frame | Action | Answer |
//! |---|---|---|
//! | `prompt` | deliver it by the session host's prompt ladder | `session_state` detail if it could not be delivered |
//! | `cancel` | interrupt the running turn (never stop the worker) | — |
//! | `permission_response` | `parse_remote_decision` + `map_remote_decision` under the one-shot policy, then answer the prompt it names ([`plan_permission_answer`]; a `deny` on a question declines it) | `control_response` error when refused; the prompt stays pending |
//! | `question_response` | check the answers against the question the owner holds ([`plan_question_answer`]), then answer it | `control_response` error when refused; the prompt stays pending |
//! | `control_request` `initialize` | — | the planner's capabilities body |
//! | `control_request` `set_model` | `set_session_option("model")` | `applies` from the owner's answer |
//! | `control_request` `set_max_thinking_tokens` | — (Rebon has no such setting) | error, from the planner |
//! | `control_request` `set_permission_mode` | the unattended-launch authorization gate, then `set_permission_mode` | `applies: now`, or the gate's refusal |
//! | `control_request` `interrupt` | interrupt the running turn | `applies: now` |
//! | anything else | ignored | — |
//!
//! Every answer to a request that changes something is built from what
//! actually happened ([`ControlVerdict`]); nothing here answers success on
//! its own.

use rebon_bridge::config::PermissionResponseBody;
use rebon_bridge::control_request::{
    plan_server_control_response, ControlEffect, ControlVerdict, ServerControlRequestPlanInput,
    ServerControlRequestSubtype,
};
use rebon_bridge::remote_permission::{
    map_remote_decision, parse_remote_decision, RebonPermissionOption, RemotePermissionAnswer,
    RemotePermissionPolicy,
};
use rebon_bridge::session_stream::{ControlResponseBody, QuestionAnswer, SessionFrame};
use rebon_proto::PermissionOptionKind;
use rebon_session_host::{
    BackgroundImageAttachment, BackgroundPermissionQuerySnapshot, ForegroundQuestionAnswer,
    SessionOptionAppliesFrom,
};

use crate::core::ids;
use serde_json::Value;

/// What a controller frame asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Prompt {
        text: String,
        images: Vec<BackgroundImageAttachment>,
        /// Attachments that were not images and so could not be passed on.
        skipped_attachments: usize,
    },
    Cancel,
    Permission(PermissionCommand),
    Question(QuestionCommand),
    Control(ControlCommand),
    /// Nothing for the runner to do, and why.
    Ignored(&'static str),
}

/// A controller's decision on a permission prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionCommand {
    pub request_id: String,
    /// The option to answer with, or why the decision cannot be applied.
    pub decision: Result<RemotePermissionAnswer, String>,
}

/// A controller's answers to a question prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionCommand {
    pub request_id: String,
    /// One answer per question, or why the frame cannot be applied.
    pub answers: Result<Vec<ForegroundQuestionAnswer>, String>,
}

/// One of the RFC-0008 §4 control requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    Initialize {
        request_id: String,
    },
    SetModel {
        request_id: String,
        model: Option<String>,
    },
    SetMaxThinkingTokens {
        request_id: String,
    },
    SetPermissionMode {
        request_id: String,
        mode: Option<String>,
    },
    Interrupt {
        request_id: String,
    },
    Unsupported {
        request_id: String,
        subtype: String,
    },
}

impl ControlCommand {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Initialize { request_id }
            | Self::SetModel { request_id, .. }
            | Self::SetMaxThinkingTokens { request_id }
            | Self::SetPermissionMode { request_id, .. }
            | Self::Interrupt { request_id }
            | Self::Unsupported { request_id, .. } => request_id,
        }
    }

    pub fn subtype(&self) -> ServerControlRequestSubtype {
        match self {
            Self::Initialize { .. } => ServerControlRequestSubtype::Initialize,
            Self::SetModel { .. } => ServerControlRequestSubtype::SetModel,
            Self::SetMaxThinkingTokens { .. } => ServerControlRequestSubtype::SetMaxThinkingTokens,
            Self::SetPermissionMode { .. } => ServerControlRequestSubtype::SetPermissionMode,
            Self::Interrupt { .. } => ServerControlRequestSubtype::Interrupt,
            Self::Unsupported { subtype, .. } => {
                ServerControlRequestSubtype::Other(subtype.clone())
            }
        }
    }
}

/// Read one frame from the stream.
pub fn classify(frame: SessionFrame, policy: RemotePermissionPolicy) -> Inbound {
    match frame {
        SessionFrame::Prompt { text, attachments } => {
            let mut images = Vec::new();
            let mut skipped_attachments = 0;
            for attachment in &attachments {
                match image_attachment(attachment, images.len()) {
                    Some(image) => images.push(image),
                    None => skipped_attachments += 1,
                }
            }
            Inbound::Prompt {
                text,
                images,
                skipped_attachments,
            }
        }
        SessionFrame::Cancel => Inbound::Cancel,
        SessionFrame::PermissionResponse { response } => {
            Inbound::Permission(permission_command(response, policy))
        }
        SessionFrame::QuestionResponse {
            request_id,
            answers,
        } => Inbound::Question(question_command(request_id, answers)),
        SessionFrame::ControlRequest {
            request_id,
            subtype,
            params,
        } => Inbound::Control(control_command(request_id, &subtype, &params)),
        SessionFrame::StreamError { .. } => Inbound::Ignored("the server could not route a frame"),
        SessionFrame::SessionMessage { .. }
        | SessionFrame::SessionState { .. }
        | SessionFrame::PermissionRequest { .. }
        | SessionFrame::ControlResponse { .. }
        | SessionFrame::SessionBound { .. } => Inbound::Ignored("a worker frame"),
        SessionFrame::Other(_) => Inbound::Ignored("a frame type this build does not know"),
    }
}

fn permission_command(
    response: PermissionResponseBody,
    policy: RemotePermissionPolicy,
) -> PermissionCommand {
    let decision = if response.subtype != "success" {
        Err(format!(
            "a `{}` permission response carries no decision",
            response.subtype
        ))
    } else {
        parse_remote_decision(&response.response)
            .map_err(|error| error.to_string())
            .and_then(|decision| {
                map_remote_decision(&decision, policy).map_err(|error| error.to_string())
            })
    };
    PermissionCommand {
        request_id: response.request_id,
        decision,
    }
}

fn question_command(request_id: String, answers: Vec<QuestionAnswer>) -> QuestionCommand {
    let answers = if answers.is_empty() {
        Err("a question response needs one answer per question".to_string())
    } else {
        Ok(answers
            .into_iter()
            .map(|answer| ForegroundQuestionAnswer {
                selected_options: answer.selected_options,
                other_text: answer.other_text,
            })
            .collect())
    };
    QuestionCommand {
        request_id,
        answers,
    }
}

fn control_command(request_id: String, subtype: &str, params: &Value) -> ControlCommand {
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    match ServerControlRequestSubtype::parse(subtype) {
        ServerControlRequestSubtype::Initialize => ControlCommand::Initialize { request_id },
        ServerControlRequestSubtype::SetModel => ControlCommand::SetModel {
            request_id,
            model: text("model"),
        },
        ServerControlRequestSubtype::SetMaxThinkingTokens => {
            ControlCommand::SetMaxThinkingTokens { request_id }
        }
        ServerControlRequestSubtype::SetPermissionMode => ControlCommand::SetPermissionMode {
            request_id,
            mode: text("mode").or_else(|| text("permission_mode")),
        },
        ServerControlRequestSubtype::Interrupt => ControlCommand::Interrupt { request_id },
        ServerControlRequestSubtype::Other(subtype) => ControlCommand::Unsupported {
            request_id,
            subtype,
        },
    }
}

/// An ACP image content block, as a job attachment.
fn image_attachment(block: &Value, index: usize) -> Option<BackgroundImageAttachment> {
    if block.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let data = block.get("data")?.as_str()?.to_string();
    let media_type = block
        .get("mimeType")
        .or_else(|| block.get("mediaType"))?
        .as_str()?
        .to_string();
    Some(BackgroundImageAttachment {
        id: u32::try_from(index + 1).unwrap_or(u32::MAX),
        data,
        media_type,
        filename: block
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        source_path: None,
    })
}

/// The `control_response` for `command`, from the verdict the runner
/// reached by trying it. `None` means it was not tried because it cannot
/// be (`set_max_thinking_tokens`), and the planner answers with an error.
pub fn control_response(
    command: &ControlCommand,
    verdict: Option<ControlVerdict>,
    pid: u32,
) -> SessionFrame {
    let plan = plan_server_control_response(ServerControlRequestPlanInput {
        request_id: command.request_id(),
        subtype: command.subtype(),
        outbound_only: false,
        verdict,
        pid,
    });
    SessionFrame::ControlResponse {
        response: ControlResponseBody::from(plan),
    }
}

/// The verdict for a session option the owner accepted.
///
/// A worker builds its session for every turn, so an option that lands
/// "when the session is next built" lands on the next turn.
pub fn option_verdict(applies: SessionOptionAppliesFrom) -> ControlVerdict {
    ControlVerdict::Applied(match applies {
        SessionOptionAppliesFrom::Immediately => ControlEffect::Now,
        SessionOptionAppliesFrom::NextTurn | SessionOptionAppliesFrom::NextSession => {
            ControlEffect::NextTurn
        }
    })
}

/// The verdict for an interrupt. Nothing to interrupt is not a failure:
/// the session is in the state the controller asked for.
pub fn interrupt_verdict(result: Result<bool, String>) -> ControlVerdict {
    match result {
        Ok(_) => ControlVerdict::Applied(ControlEffect::Now),
        Err(error) => ControlVerdict::Rejected(error),
    }
}

/// A missing required parameter, as a verdict.
pub fn missing_parameter(subtype: &str, key: &str) -> ControlVerdict {
    ControlVerdict::Rejected(format!("{subtype} needs params.{key}"))
}

/// Tell the controller its permission decision was not applied. The
/// prompt, if still pending, stays pending.
pub fn permission_refusal(request_id: &str, reason: &str) -> SessionFrame {
    SessionFrame::ControlResponse {
        response: ControlResponseBody::error(request_id, reason),
    }
}

/// What to send the owner for a remote answer, decided against the prompt
/// it is parked on now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerPlan {
    /// The owner is not parked on the prompt the controller answered.
    NotPending,
    /// Answer a permission prompt with `option_id`. `None` declines it
    /// without an option: the owner's cancellation.
    Permission {
        query_id: u64,
        option_id: Option<String>,
    },
    /// Answer the question prompt with the controller's answers.
    Questions { query_id: u64, turn_generation: u64 },
}

/// The prompt `pending` if it is the one `request_id` names.
fn named<'a>(
    pending: Option<&'a BackgroundPermissionQuerySnapshot>,
    request_id: &str,
) -> Option<&'a BackgroundPermissionQuerySnapshot> {
    pending.filter(|pending| ids::permission_request_id(pending) == request_id)
}

/// How to apply a permission decision to the prompt the owner holds.
///
/// A question takes answers, not an allow: an `allow` on it is refused.
/// A `deny` declines it, the way dismissing the question on the machine
/// does — with the prompt's own reject option when it offers one, and
/// as a cancellation when it does not.
pub fn plan_permission_answer(
    pending: Option<&BackgroundPermissionQuerySnapshot>,
    request_id: &str,
    answer: RebonPermissionOption,
) -> Result<AnswerPlan, String> {
    let Some(pending) = named(pending, request_id) else {
        return Ok(AnswerPlan::NotPending);
    };
    if rebon_session_host::ask_user_questions_from_permission(pending).is_some() {
        if answer != RebonPermissionOption::RejectOnce {
            return Err(
                "this prompt is a question; answer it with question_response, or deny it"
                    .to_string(),
            );
        }
        return Ok(AnswerPlan::Permission {
            query_id: pending.query_id,
            option_id: option_for(pending, RebonPermissionOption::RejectOnce).ok(),
        });
    }
    Ok(AnswerPlan::Permission {
        query_id: pending.query_id,
        option_id: Some(option_for(pending, answer)?),
    })
}

/// How to apply a controller's answers to the prompt the owner holds.
///
/// The answers are checked here, against the same rules the owner applies
/// (one per question, options in range, one option for a single-select
/// question, never empty), so a misfit is refused with a reason before
/// anything is sent and the prompt stays pending.
pub fn plan_question_answer(
    pending: Option<&BackgroundPermissionQuerySnapshot>,
    request_id: &str,
    answers: &[ForegroundQuestionAnswer],
) -> Result<AnswerPlan, String> {
    let Some(pending) = named(pending, request_id) else {
        return Ok(AnswerPlan::NotPending);
    };
    if rebon_session_host::ask_user_questions_from_permission(pending).is_none() {
        return Err(
            "this prompt is a permission request, not a question; answer it with \
             permission_response"
                .to_string(),
        );
    }
    rebon_session_host::build_ask_user_question_updated_input(pending, answers)
        .map_err(|error| format!("the answers do not fit the questions: {error:#}"))?;
    Ok(AnswerPlan::Questions {
        query_id: pending.query_id,
        turn_generation: pending.turn_generation,
    })
}

/// The option of `query` that carries `answer`, by kind. Kinds are what
/// the tool contract fixes; option ids are generated per prompt.
pub fn option_for(
    query: &BackgroundPermissionQuerySnapshot,
    answer: RebonPermissionOption,
) -> Result<String, String> {
    let wanted = match answer {
        RebonPermissionOption::AllowOnce => PermissionOptionKind::AllowOnce,
        RebonPermissionOption::AllowAlways => PermissionOptionKind::AllowAlways,
        RebonPermissionOption::RejectOnce => PermissionOptionKind::RejectOnce,
        // Never chosen by the mapping; see `remote_permission`.
        RebonPermissionOption::AllowAlwaysGeneralized => {
            return Err("a generalized rule cannot be granted remotely".to_string())
        }
    };
    query
        .options
        .iter()
        .find(|option| {
            rebon_session_host::parse_permission_option_kind(&option.kind) == Some(wanted)
        })
        .map(|option| option.option_id.clone())
        .ok_or_else(|| format!("this prompt offers no `{answer}` option"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::uplink::tests::{query, question};
    use serde_json::json;

    fn one_shot() -> RemotePermissionPolicy {
        RemotePermissionPolicy::one_shot()
    }

    fn decision(value: Value) -> SessionFrame {
        SessionFrame::PermissionResponse {
            response: PermissionResponseBody::success("perm-1", value),
        }
    }

    fn request(subtype: &str, params: Value) -> SessionFrame {
        SessionFrame::ControlRequest {
            request_id: "r1".into(),
            subtype: subtype.into(),
            params,
        }
    }

    fn response_of(frame: SessionFrame) -> ControlResponseBody {
        match frame {
            SessionFrame::ControlResponse { response } => response,
            other => panic!("expected a control response, got {other:?}"),
        }
    }

    #[test]
    fn a_prompt_keeps_its_text_and_its_images() {
        let frame = SessionFrame::Prompt {
            text: "look".into(),
            attachments: vec![
                json!({"type": "image", "mimeType": "image/png", "data": "AAA", "name": "a.png"}),
                json!({"type": "resource_link", "uri": "file:///x"}),
                json!({"type": "image", "mediaType": "image/jpeg", "data": "BBB"}),
                json!({"type": "image", "data": "no type"}),
            ],
        };
        let Inbound::Prompt {
            text,
            images,
            skipped_attachments,
        } = classify(frame, one_shot())
        else {
            panic!("expected a prompt");
        };
        assert_eq!(text, "look");
        assert_eq!(skipped_attachments, 2);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].filename.as_deref(), Some("a.png"));
        assert_eq!(images[1].data, "BBB");
        assert_ne!(images[0].id, images[1].id);
    }

    #[test]
    fn cancel_and_worker_frames_classify() {
        assert_eq!(classify(SessionFrame::Cancel, one_shot()), Inbound::Cancel);
        for frame in [
            SessionFrame::message(json!({})),
            SessionFrame::bound("x"),
            SessionFrame::stream_error("no_worker", "x"),
            serde_json::from_value::<SessionFrame>(json!({"type": "future"})).unwrap(),
        ] {
            assert!(
                matches!(classify(frame.clone(), one_shot()), Inbound::Ignored(_)),
                "{frame:?}"
            );
        }
    }

    #[test]
    fn remote_decisions_map_one_shot() {
        let allow = classify(decision(json!({"behavior": "allow"})), one_shot());
        let Inbound::Permission(PermissionCommand {
            request_id,
            decision: Ok(answer),
        }) = allow
        else {
            panic!("expected an applicable decision");
        };
        assert_eq!(request_id, "perm-1");
        assert_eq!(answer.option, RebonPermissionOption::AllowOnce);

        let deny = classify(
            decision(json!({"behavior": "deny", "message": "no"})),
            one_shot(),
        );
        let Inbound::Permission(PermissionCommand {
            decision: Ok(answer),
            ..
        }) = deny
        else {
            panic!("expected an applicable decision");
        };
        assert_eq!(answer.option, RebonPermissionOption::RejectOnce);
        assert_eq!(answer.message.as_deref(), Some("no"));
    }

    #[test]
    fn decisions_the_policy_refuses_are_refused_not_downgraded() {
        for value in [
            json!({"behavior": "allow", "updated_permissions": [{"rule": "x"}]}),
            json!({"behavior": "allow", "updatedInput": {"command": "rm"}}),
            json!({"behavior": "deny", "updated_input": {}}),
            json!({"behavior": "maybe"}),
            json!("allow"),
        ] {
            let Inbound::Permission(command) = classify(decision(value.clone()), one_shot()) else {
                panic!("expected a permission command");
            };
            assert!(command.decision.is_err(), "{value}");
        }
        let error = SessionFrame::PermissionResponse {
            response: PermissionResponseBody {
                subtype: "error".into(),
                request_id: "perm-1".into(),
                response: json!({"behavior": "allow"}),
            },
        };
        let Inbound::Permission(command) = classify(error, one_shot()) else {
            panic!("expected a permission command");
        };
        assert!(command.decision.unwrap_err().contains("error"));
    }

    #[test]
    fn control_requests_classify_with_their_parameters() {
        let cases = [
            (
                request("initialize", Value::Null),
                ControlCommand::Initialize {
                    request_id: "r1".into(),
                },
            ),
            (
                request("set_model", json!({"model": " opus "})),
                ControlCommand::SetModel {
                    request_id: "r1".into(),
                    model: Some("opus".into()),
                },
            ),
            (
                request("set_model", json!({})),
                ControlCommand::SetModel {
                    request_id: "r1".into(),
                    model: None,
                },
            ),
            (
                request("set_max_thinking_tokens", json!({"max_thinking_tokens": 1})),
                ControlCommand::SetMaxThinkingTokens {
                    request_id: "r1".into(),
                },
            ),
            (
                request("set_permission_mode", json!({"mode": "plan"})),
                ControlCommand::SetPermissionMode {
                    request_id: "r1".into(),
                    mode: Some("plan".into()),
                },
            ),
            (
                request(
                    "set_permission_mode",
                    json!({"permission_mode": "acceptEdits"}),
                ),
                ControlCommand::SetPermissionMode {
                    request_id: "r1".into(),
                    mode: Some("acceptEdits".into()),
                },
            ),
            (
                request("interrupt", Value::Null),
                ControlCommand::Interrupt {
                    request_id: "r1".into(),
                },
            ),
            (
                request("teleport", Value::Null),
                ControlCommand::Unsupported {
                    request_id: "r1".into(),
                    subtype: "teleport".into(),
                },
            ),
        ];
        for (frame, expected) in cases {
            assert_eq!(classify(frame, one_shot()), Inbound::Control(expected));
        }
    }

    #[test]
    fn verdicts_become_the_answers_the_protocol_defines() {
        let set_model = ControlCommand::SetModel {
            request_id: "r1".into(),
            model: Some("m".into()),
        };
        let applied = response_of(control_response(
            &set_model,
            Some(option_verdict(SessionOptionAppliesFrom::NextTurn)),
            7,
        ));
        assert_eq!(applied.subtype, "success");
        assert_eq!(applied.applied(), Some(ControlEffect::NextTurn));
        assert_eq!(
            option_verdict(SessionOptionAppliesFrom::NextSession),
            ControlVerdict::Applied(ControlEffect::NextTurn)
        );
        assert_eq!(
            option_verdict(SessionOptionAppliesFrom::Immediately),
            ControlVerdict::Applied(ControlEffect::Now)
        );

        // Rebon has no thinking-token setting: no verdict, an error.
        let thinking = response_of(control_response(
            &ControlCommand::SetMaxThinkingTokens {
                request_id: "r2".into(),
            },
            None,
            7,
        ));
        assert_eq!(thinking.subtype, "error");
        assert_eq!(thinking.request_id, "r2");
        assert!(thinking
            .error
            .unwrap()
            .contains("set_max_thinking_tokens is not supported"));

        // The permission-mode gate's refusal reaches the controller as is.
        let refused = response_of(control_response(
            &ControlCommand::SetPermissionMode {
                request_id: "r3".into(),
                mode: Some("bypassPermissions".into()),
            },
            Some(ControlVerdict::Rejected(
                "bypassPermissions has not been accepted for unattended sessions".into(),
            )),
            7,
        ));
        assert_eq!(refused.subtype, "error");
        assert!(refused.error.unwrap().contains("not been accepted"));

        let init = response_of(control_response(
            &ControlCommand::Initialize {
                request_id: "r4".into(),
            },
            None,
            7,
        ));
        assert_eq!(init.subtype, "success");
        assert_eq!(init.response.unwrap()["pid"], 7);

        let unknown = response_of(control_response(
            &ControlCommand::Unsupported {
                request_id: "r5".into(),
                subtype: "teleport".into(),
            },
            None,
            7,
        ));
        assert_eq!(unknown.subtype, "error");

        assert_eq!(
            missing_parameter("set_model", "model"),
            ControlVerdict::Rejected("set_model needs params.model".into())
        );
    }

    #[test]
    fn an_interrupt_with_nothing_running_is_still_applied() {
        assert_eq!(
            interrupt_verdict(Ok(true)),
            ControlVerdict::Applied(ControlEffect::Now)
        );
        assert_eq!(
            interrupt_verdict(Ok(false)),
            ControlVerdict::Applied(ControlEffect::Now)
        );
        assert_eq!(
            interrupt_verdict(Err("no host".into())),
            ControlVerdict::Rejected("no host".into())
        );
    }

    #[test]
    fn an_answer_picks_the_option_by_kind() {
        let prompt = query(1, 1);
        assert_eq!(
            option_for(&prompt, RebonPermissionOption::AllowOnce),
            Ok("allow".into())
        );
        assert_eq!(
            option_for(&prompt, RebonPermissionOption::RejectOnce),
            Ok("deny".into())
        );
        assert!(option_for(&prompt, RebonPermissionOption::AllowAlwaysGeneralized).is_err());
        let mut bare = query(1, 1);
        bare.options.retain(|option| option.kind != "reject_once");
        assert!(option_for(&bare, RebonPermissionOption::RejectOnce)
            .unwrap_err()
            .contains("reject_once"));
        // A legacy kind spelling is read the same way.
        bare.options[0].kind = "AllowOnce".into();
        assert_eq!(
            option_for(&bare, RebonPermissionOption::AllowOnce),
            Ok("allow".into())
        );
    }

    fn answer(selected: &[usize], text: Option<&str>) -> ForegroundQuestionAnswer {
        ForegroundQuestionAnswer {
            selected_options: selected.to_vec(),
            other_text: text.map(str::to_string),
        }
    }

    #[test]
    fn a_question_response_classifies_into_session_host_answers() {
        let frame = SessionFrame::QuestionResponse {
            request_id: "perm-q".into(),
            answers: vec![QuestionAnswer::options([2]), QuestionAnswer::text("later")],
        };
        assert_eq!(
            classify(frame, one_shot()),
            Inbound::Question(QuestionCommand {
                request_id: "perm-q".into(),
                answers: Ok(vec![answer(&[2], None), answer(&[], Some("later"))]),
            })
        );
        let Inbound::Question(empty) = classify(
            SessionFrame::QuestionResponse {
                request_id: "perm-q".into(),
                answers: Vec::new(),
            },
            one_shot(),
        ) else {
            panic!("expected a question command");
        };
        assert!(empty
            .answers
            .unwrap_err()
            .contains("one answer per question"));
    }

    #[test]
    fn answers_are_checked_against_the_question_the_owner_holds() {
        let asked = question(6, 2);
        let id = ids::permission_request_id(&asked);
        let good = [answer(&[1], None), answer(&[0, 2], Some("both"))];
        assert_eq!(
            plan_question_answer(Some(&asked), &id, &good),
            Ok(AnswerPlan::Questions {
                query_id: 6,
                turn_generation: 2
            })
        );
        // Free text alone answers a question.
        assert!(plan_question_answer(
            Some(&asked),
            &id,
            &[answer(&[], Some("D")), answer(&[], Some("never"))]
        )
        .is_ok());

        for (answers, why) in [
            (vec![answer(&[0], None)], "count"),
            (
                vec![answer(&[0], None), answer(&[0], None), answer(&[0], None)],
                "count",
            ),
            (vec![answer(&[3], None), answer(&[0], None)], "out-of-range"),
            (
                vec![answer(&[0, 1], None), answer(&[0], None)],
                "single-select",
            ),
            (vec![answer(&[0], None), answer(&[1, 1], None)], "duplicate"),
            (
                vec![answer(&[], Some("  ")), answer(&[0], None)],
                "select an option",
            ),
        ] {
            let error = plan_question_answer(Some(&asked), &id, &answers).unwrap_err();
            assert!(error.contains("do not fit"), "{error}");
            assert!(error.contains(why), "{why}: {error}");
        }

        // Not the prompt the controller answered, or no prompt at all.
        assert_eq!(
            plan_question_answer(Some(&asked), "perm-other", &good),
            Ok(AnswerPlan::NotPending)
        );
        assert_eq!(
            plan_question_answer(None, &id, &good),
            Ok(AnswerPlan::NotPending)
        );
        let replaced = question(6, 3);
        assert_eq!(
            plan_question_answer(Some(&replaced), &id, &good),
            Ok(AnswerPlan::NotPending)
        );

        // A tool approval is not answered with answers.
        let approval = query(6, 2);
        let error = plan_question_answer(
            Some(&approval),
            &ids::permission_request_id(&approval),
            &good,
        )
        .unwrap_err();
        assert!(error.contains("permission_response"), "{error}");
    }

    #[test]
    fn a_question_can_be_declined_but_not_allowed() {
        let asked = question(6, 2);
        let id = ids::permission_request_id(&asked);
        let error = plan_permission_answer(Some(&asked), &id, RebonPermissionOption::AllowOnce)
            .unwrap_err();
        assert!(error.contains("question_response"), "{error}");
        // It offers no reject option: declined as a cancellation.
        assert_eq!(
            plan_permission_answer(Some(&asked), &id, RebonPermissionOption::RejectOnce),
            Ok(AnswerPlan::Permission {
                query_id: 6,
                option_id: None
            })
        );
        // With one, declined with it.
        let mut rejectable = asked.clone();
        rejectable.options.push(query(6, 2).options.remove(2));
        assert_eq!(
            plan_permission_answer(Some(&rejectable), &id, RebonPermissionOption::RejectOnce),
            Ok(AnswerPlan::Permission {
                query_id: 6,
                option_id: Some("deny".into())
            })
        );
        assert_eq!(
            plan_permission_answer(None, &id, RebonPermissionOption::RejectOnce),
            Ok(AnswerPlan::NotPending)
        );
    }

    #[test]
    fn a_tool_approval_is_planned_by_option_kind() {
        let prompt = query(3, 1);
        let id = ids::permission_request_id(&prompt);
        assert_eq!(
            plan_permission_answer(Some(&prompt), &id, RebonPermissionOption::AllowOnce),
            Ok(AnswerPlan::Permission {
                query_id: 3,
                option_id: Some("allow".into())
            })
        );
        assert_eq!(
            plan_permission_answer(
                Some(&prompt),
                "perm-stale",
                RebonPermissionOption::AllowOnce
            ),
            Ok(AnswerPlan::NotPending)
        );
        let mut bare = query(3, 1);
        bare.options.retain(|option| option.kind != "reject_once");
        assert!(
            plan_permission_answer(Some(&bare), &id, RebonPermissionOption::RejectOnce)
                .unwrap_err()
                .contains("reject_once")
        );
    }

    #[test]
    fn a_refusal_is_an_error_response_to_the_request_id() {
        let body = response_of(permission_refusal("perm-9", "nope"));
        assert_eq!(body, ControlResponseBody::error("perm-9", "nope"));
    }
}
