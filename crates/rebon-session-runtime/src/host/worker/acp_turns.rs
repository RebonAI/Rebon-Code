//! What a hosted session's turn needs when an ACP agent runs it rather than
//! Rebon's own engine: answers to the agent's permission requests, and —
//! for a member of an agent group — its turn handed on as its answer.
//!
//! Rebon's engine asks its permission broker, which applies the session's
//! mode and leaves the rest to a person. An ACP agent brings its own tools
//! and asks with `session/request_permission` instead, which before this
//! had no one to answer it in a background session and failed. The answer
//! here comes from the session's mode and the kind of tool call the agent
//! names (`rebon_acp_client::permission_tool_kind`):
//!
//! | mode                                  | allowed                      |
//! |---------------------------------------|------------------------------|
//! | `plan`, `default`, `dontAsk`, `bubble`| read, search, think, fetch   |
//! | `acceptEdits`, `auto`                 | and edit, delete, move       |
//! | `bypassPermissions`                   | everything                   |
//!
//! Anything else is refused: commands under `auto` too, since Rebon's
//! classifier never sees an ACP agent's calls, and whatever a person would
//! be asked about, since no one is asked here. A member Rebon started is
//! told to answer `Blocked:` with what it needed when that happens. A grant
//! that stands (`allow_always`) is never chosen: that is a person's to give.
//!
//! An ACP agent has no group tools — `rebon mcp serve` is not among the
//! servers it is given — so a member whose turns one runs cannot call
//! `group_send`. Its turn's last message is its answer instead
//! ([`rebon_group::assign::answer_with_turn`]).

use std::sync::{Arc, Mutex};

use rebon_agent_core::publisher::{
    make_permission_result_response, ChannelPermissionRequestPublisher, OutboundPermissionRequest,
};
use rebon_permissions::PermissionMode;
use rebon_proto::types::{PermissionOptionKind, PermissionOutcome, RequestPermissionResult};

/// The ACP agent the session's turns go to now, when they go to one the
/// config declares; `None` for Rebon's own engine and for a kernel loop.
pub(crate) fn acp_agent_of(session: &crate::EngineSession) -> Option<String> {
    let current = session.engine_half.runtime.session_agents.current_id();
    crate::rebon_config::load_acp_agents()
        .ok()?
        .into_iter()
        .any(|agent| agent.id.eq_ignore_ascii_case(&current))
        .then_some(current)
}

/// Whether a request for a tool call of `kind` is let through in `mode`.
fn allows(mode: PermissionMode, kind: Option<&str>) -> bool {
    let reads = matches!(kind, Some("read" | "search" | "think" | "fetch"));
    let edits = matches!(kind, Some("edit" | "delete" | "move"));
    match mode {
        PermissionMode::BypassPermissions => true,
        PermissionMode::AcceptEdits | PermissionMode::Auto => reads || edits,
        PermissionMode::Plan
        | PermissionMode::Default
        | PermissionMode::DontAsk
        | PermissionMode::Bubble => reads,
    }
}

/// The answer to one request: the option that allows it once, or the one
/// that refuses it once; cancelled when the agent offered neither.
fn answer(mode: PermissionMode, request: &OutboundPermissionRequest) -> RequestPermissionResult {
    let kind = rebon_acp_client::permission_tool_kind(&request.params);
    let wanted = if allows(mode, kind) {
        PermissionOptionKind::AllowOnce
    } else {
        PermissionOptionKind::RejectOnce
    };
    let option = request
        .params
        .options
        .iter()
        .find(|option| option.kind == wanted);
    tracing::info!(
        kind = kind.unwrap_or("?"),
        title = request.params.title.as_deref().unwrap_or(""),
        mode = mode.as_wire(),
        decision = ?wanted,
        "acp turn: permission answered by the session's mode"
    );
    match option {
        Some(option) => RequestPermissionResult {
            outcome: PermissionOutcome::Selected,
            option_id: Some(option.option_id.clone()),
            updated_input: None,
        },
        None => RequestPermissionResult {
            outcome: PermissionOutcome::Cancelled,
            option_id: None,
            updated_input: None,
        },
    }
}

/// A publisher for this turn's permission requests, answered by the
/// session's mode as it stands when each arrives (`/mode` between two
/// requests counts). The answering task ends with the turn, when the last
/// copy of the publisher goes.
pub(crate) fn permission_publisher(
    mode: Arc<Mutex<PermissionMode>>,
) -> ChannelPermissionRequestPublisher {
    let (publisher, mut requests) = ChannelPermissionRequestPublisher::new();
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let mode = *mode.lock().expect("permission mode cell poisoned");
            let result = answer(mode, &request);
            let _ = request
                .response_tx
                .send(make_permission_result_response(request.request_id, result));
        }
    });
    publisher
}

/// After a turn an ACP agent ran: when the session is a member of an agent
/// group, the turn's last message (read from the session's transcript)
/// answers what it had been asked and had not answered.
pub(crate) fn answer_group_with_turn(session_id: &str, last_message: Option<String>) {
    let Some(text) = last_message else {
        return;
    };
    let store = rebon_group::GroupStore::new(rebon_group::default_root(
        &crate::rebon_config::config_home_dir(),
    ));
    let member = rebon_group::model::MemberKey {
        agent: rebon_group::AgentKind::REBON.to_string(),
        session_id: session_id.to_string(),
    };
    match rebon_group::assign::answer_with_turn(&store, &member, &text) {
        Ok(replies) if !replies.is_empty() => tracing::info!(
            session_id,
            replies = replies.len(),
            "acp turn: the turn answered its group"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            session_id,
            error = %format!("{error:#}"),
            "acp turn: the turn's answer could not be posted to its group"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_proto::types::{PermissionOption, RequestPermissionParams};

    fn request(kind: Option<&str>, options: &[PermissionOptionKind]) -> RequestPermissionParams {
        serde_json::from_value(serde_json::json!({
            "sessionId": "s-1",
            "toolCall": { "toolCallId": "call-1" },
            "options": options
                .iter()
                .enumerate()
                .map(|(index, kind)| serde_json::to_value(PermissionOption {
                    option_id: format!("o{index}"),
                    name: format!("{kind:?}"),
                    kind: *kind,
                })
                .unwrap())
                .collect::<Vec<_>>(),
            "metadata": kind.map(|kind| serde_json::json!({ "toolKind": kind })),
        }))
        .unwrap()
    }

    const ALL: [PermissionOptionKind; 4] = [
        PermissionOptionKind::AllowAlways,
        PermissionOptionKind::AllowOnce,
        PermissionOptionKind::RejectOnce,
        PermissionOptionKind::RejectAlways,
    ];

    fn decide(mode: PermissionMode, kind: Option<&str>) -> Option<String> {
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        let outbound = OutboundPermissionRequest {
            request_id: rebon_proto::types::RequestId::Number(1),
            params: request(kind, &ALL),
            response_tx,
        };
        answer(mode, &outbound).option_id
    }

    #[test]
    fn each_mode_lets_through_its_kinds_of_call_and_refuses_the_rest() {
        const ALLOW: Option<&str> = Some("o1");
        const REJECT: Option<&str> = Some("o2");
        let cases: &[(PermissionMode, &[(&str, Option<&str>)])] = &[
            (
                PermissionMode::Plan,
                &[
                    ("read", ALLOW),
                    ("search", ALLOW),
                    ("fetch", ALLOW),
                    ("think", ALLOW),
                    ("edit", REJECT),
                    ("delete", REJECT),
                    ("execute", REJECT),
                    ("other", REJECT),
                ],
            ),
            (
                PermissionMode::AcceptEdits,
                &[
                    ("read", ALLOW),
                    ("edit", ALLOW),
                    ("move", ALLOW),
                    ("delete", ALLOW),
                    ("execute", REJECT),
                    ("other", REJECT),
                ],
            ),
            (
                PermissionMode::Auto,
                &[("edit", ALLOW), ("execute", REJECT)],
            ),
            (
                PermissionMode::Default,
                &[("read", ALLOW), ("edit", REJECT), ("execute", REJECT)],
            ),
            (
                PermissionMode::BypassPermissions,
                &[("execute", ALLOW), ("other", ALLOW), ("edit", ALLOW)],
            ),
        ];
        for (mode, kinds) in cases {
            for (kind, expected) in *kinds {
                assert_eq!(
                    decide(*mode, Some(kind)).as_deref(),
                    *expected,
                    "{mode:?} / {kind}"
                );
            }
        }
    }

    #[test]
    fn a_call_of_no_named_kind_is_a_persons_to_allow() {
        assert_eq!(
            decide(PermissionMode::AcceptEdits, None).as_deref(),
            Some("o2")
        );
        assert_eq!(
            decide(PermissionMode::BypassPermissions, None).as_deref(),
            Some("o1")
        );
    }

    #[test]
    fn a_standing_grant_is_never_chosen_and_a_missing_option_cancels() {
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        let only_always = OutboundPermissionRequest {
            request_id: rebon_proto::types::RequestId::Number(1),
            params: request(
                Some("read"),
                &[
                    PermissionOptionKind::AllowAlways,
                    PermissionOptionKind::RejectOnce,
                ],
            ),
            response_tx,
        };
        let result = answer(PermissionMode::BypassPermissions, &only_always);
        assert_eq!(result.outcome, PermissionOutcome::Cancelled);
        assert_eq!(result.option_id, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_publisher_answers_by_the_mode_as_it_is_when_asked() {
        let mode = Arc::new(Mutex::new(PermissionMode::Plan));
        let publisher = permission_publisher(Arc::clone(&mode));
        let refused = publisher
            .request_permission(request(Some("edit"), &ALL))
            .await
            .unwrap();
        assert_eq!(refused.option_id.as_deref(), Some("o2"));

        *mode.lock().unwrap() = PermissionMode::AcceptEdits;
        let allowed = publisher
            .request_permission(request(Some("edit"), &ALL))
            .await
            .unwrap();
        assert_eq!(allowed.option_id.as_deref(), Some("o1"), "/mode counts");
    }
}
