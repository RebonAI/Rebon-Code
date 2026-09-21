//! What a permission prompt answers with, and handing that answer back.
//!
//! The broker gives the front end one [`oneshot::Sender`] per query and
//! waits on the other end. Answering means building a [`PermissionAnswer`]
//! and moving that sender exactly once; there is no second chance and no
//! way to take it back. Keeping the construction and the send in one
//! place is what makes "exactly once" checkable by reading a single
//! function, and it is why the sender arrives here by value rather than
//! behind a borrow.
//!
//! Nothing here draws anything. The caller decides which option was
//! picked and what note was typed; this module turns that into the
//! answer the engine expects, persists an "allow always" rule when the
//! pick was one, and reports whether the receiver was still listening.

use rebon_core::permission::{OutboundPermissionQuery, PermissionAnswer};
use rebon_core::policy::PolicyStore;

use crate::permission_policy::{
    canonical_permission_option_id, is_allow_always_option_id, persist_allow_always_rule,
};

pub(crate) fn build_permission_answer(option_id: Option<String>) -> PermissionAnswer {
    build_permission_answer_with_input(option_id, None)
}

pub fn build_permission_answer_with_extra_text(
    option_id: Option<String>,
    extra_text: Option<String>,
) -> PermissionAnswer {
    build_permission_answer(option_id).with_extra_text(extra_text)
}

/// Build a permission answer that also carries an `updated_input`
/// payload (used by AskUserQuestion to transport collected answers).
pub fn build_permission_answer_with_input(
    option_id: Option<String>,
    updated_input: Option<serde_json::Value>,
) -> PermissionAnswer {
    match option_id {
        Some(option_id) => PermissionAnswer::Selected {
            option_id,
            updated_input,
            extra_text: None,
        },
        None => PermissionAnswer::Cancelled,
    }
}

/// What the front end decided, in the two shapes a permission prompt can
/// end in.
pub enum PermissionChoice {
    /// The user picked an option. `prepared` carries an answer that was
    /// already built because acting on the approval produced a payload
    /// the tool needs back (an applied profile proposal); when it is
    /// `None` the answer is built from `option_id` and `extra_text`.
    Confirm {
        option_id: Option<String>,
        extra_text: Option<String>,
        prepared: Option<PermissionAnswer>,
        overlay_blocks: usize,
    },
    /// The user dismissed the prompt without picking anything.
    Cancelled,
}

/// Whether the broker was still waiting when the answer went out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDelivery {
    Delivered,
    ReceiverGone,
}

/// Send the one answer this query gets.
///
/// Takes the query by value: the sender inside it is moved once, here,
/// and the caller cannot hold a second copy to send again.
pub fn answer_permission(
    outbound: OutboundPermissionQuery,
    choice: PermissionChoice,
    policy_store: &PolicyStore,
    cwd: &str,
) -> PermissionDelivery {
    match choice {
        PermissionChoice::Confirm {
            option_id,
            extra_text,
            prepared,
            overlay_blocks,
        } => {
            // When the user selects an allow-always candidate, persist
            // the selected local rule(s) while sending the canonical
            // engine option id back to the broker.
            if option_id.as_deref().is_some_and(is_allow_always_option_id) {
                persist_allow_always_rule(
                    &outbound.tool_name,
                    outbound.tool_input.as_ref(),
                    policy_store,
                    cwd,
                    option_id.as_deref(),
                );
            }
            let answer = prepared.unwrap_or_else(|| {
                build_permission_answer_with_extra_text(
                    canonical_permission_option_id(option_id.clone()),
                    extra_text,
                )
            });
            tracing::info!(
                target: "stream_dbg",
                call_id = %outbound.tool_call_id,
                tool = %outbound.tool_name,
                option = ?option_id,
                overlay_blocks,
                "tui: permission Confirm -> sending answer"
            );
            let send_ok = outbound.response_tx.send(answer).is_ok();
            tracing::info!(target: "stream_dbg", send_ok, "tui: permission answer sent");
            delivery(send_ok)
        }
        PermissionChoice::Cancelled => {
            let answer = build_permission_answer(None);
            tracing::info!(
                target: "stream_dbg",
                call_id = %outbound.tool_call_id,
                tool = %outbound.tool_name,
                "tui: permission Cancel -> sending None answer"
            );
            delivery(outbound.response_tx.send(answer).is_ok())
        }
    }
}

fn delivery(send_ok: bool) -> PermissionDelivery {
    if send_ok {
        PermissionDelivery::Delivered
    } else {
        PermissionDelivery::ReceiverGone
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_policy::ULTRAPLAN_CEO_OPTION_ID;
    use rebon_core::permission::{PermissionOptionKind, PermissionQueryOption};
    use tokio::sync::oneshot;

    fn query() -> (OutboundPermissionQuery, oneshot::Receiver<PermissionAnswer>) {
        let (response_tx, response_rx) = oneshot::channel();
        (
            OutboundPermissionQuery {
                id: 1,
                tool_name: "Read".into(),
                tool_call_id: "tool-1".into(),
                session_id: "sess-1".into(),
                title: "Allow Read?".into(),
                message: "Read(path=\"Cargo.toml\")".into(),
                tool_input: None,
                metadata: None,
                options: vec![PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                }],
                response_tx,
            },
            response_rx,
        )
    }

    fn store() -> PolicyStore {
        PolicyStore::new()
    }

    #[test]
    fn build_permission_answer_selected() {
        let answer = build_permission_answer(Some("allow_once".into()));
        assert!(
            matches!(answer, PermissionAnswer::Selected { option_id, .. } if option_id == "allow_once")
        );
    }

    #[test]
    fn build_permission_answer_cancelled() {
        let answer = build_permission_answer(None);
        assert!(matches!(answer, PermissionAnswer::Cancelled));
    }

    #[test]
    fn confirm_delivers_exactly_one_answer() {
        let (outbound, mut rx) = query();
        let delivery = answer_permission(
            outbound,
            PermissionChoice::Confirm {
                option_id: Some("allow_once".into()),
                extra_text: Some("a note".into()),
                prepared: None,
                overlay_blocks: 3,
            },
            &store(),
            ".",
        );

        assert_eq!(delivery, PermissionDelivery::Delivered);
        match rx.try_recv().expect("one answer") {
            PermissionAnswer::Selected {
                option_id,
                extra_text,
                ..
            } => {
                assert_eq!(option_id, "allow_once");
                assert_eq!(extra_text.as_deref(), Some("a note"));
            }
            PermissionAnswer::Cancelled => panic!("expected a selected answer"),
        }
        // The sender moved into the call, so the channel is closed and
        // there is no second answer to read.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_prepared_answer_wins_over_the_option_id() {
        let (outbound, mut rx) = query();
        let prepared = build_permission_answer_with_input(
            Some("allow_once".into()),
            Some(serde_json::json!({"applied": true})),
        );

        answer_permission(
            outbound,
            PermissionChoice::Confirm {
                option_id: Some("allow_once".into()),
                extra_text: Some("ignored".into()),
                prepared: Some(prepared),
                overlay_blocks: 0,
            },
            &store(),
            ".",
        );

        match rx.try_recv().expect("one answer") {
            PermissionAnswer::Selected {
                updated_input,
                extra_text,
                ..
            } => {
                assert_eq!(updated_input, Some(serde_json::json!({"applied": true})));
                assert!(extra_text.is_none());
            }
            PermissionAnswer::Cancelled => panic!("expected a selected answer"),
        }
    }

    #[test]
    fn an_ultraplan_option_id_is_canonicalized_for_the_broker() {
        let (outbound, mut rx) = query();
        answer_permission(
            outbound,
            PermissionChoice::Confirm {
                option_id: Some(ULTRAPLAN_CEO_OPTION_ID.into()),
                extra_text: None,
                prepared: None,
                overlay_blocks: 0,
            },
            &store(),
            ".",
        );

        match rx.try_recv().expect("one answer") {
            PermissionAnswer::Selected { option_id, .. } => {
                assert_eq!(option_id, "yes_default");
            }
            PermissionAnswer::Cancelled => panic!("expected a selected answer"),
        }
    }

    #[test]
    fn an_allow_always_pick_persists_a_rule_before_the_answer_goes_out() {
        // The rule is written under the working directory it is given,
        // so this one gets a throwaway directory rather than the repo.
        let cwd = tempfile::tempdir().expect("tempdir");
        let (mut outbound, mut rx) = query();
        outbound.tool_input = Some(serde_json::json!({"path": "Cargo.toml"}));
        let policy_store = store();

        answer_permission(
            outbound,
            PermissionChoice::Confirm {
                option_id: Some("allow_always".into()),
                extra_text: None,
                prepared: None,
                overlay_blocks: 0,
            },
            &policy_store,
            &cwd.path().to_string_lossy(),
        );

        match rx.try_recv().expect("one answer") {
            PermissionAnswer::Selected { option_id, .. } => {
                assert_eq!(option_id, "allow_always");
            }
            PermissionAnswer::Cancelled => panic!("expected a selected answer"),
        }
        assert!(
            cwd.path().join(".rebon/settings.json").exists(),
            "the allow-always rule should have been written to the project settings"
        );
    }

    #[test]
    fn cancel_sends_the_cancelled_answer() {
        let (outbound, mut rx) = query();
        let delivery = answer_permission(outbound, PermissionChoice::Cancelled, &store(), ".");

        assert_eq!(delivery, PermissionDelivery::Delivered);
        assert!(matches!(
            rx.try_recv().expect("one answer"),
            PermissionAnswer::Cancelled
        ));
    }

    #[test]
    fn a_dropped_receiver_reports_receiver_gone_without_panicking() {
        let (outbound, rx) = query();
        drop(rx);

        let delivery = answer_permission(
            outbound,
            PermissionChoice::Confirm {
                option_id: Some("allow_once".into()),
                extra_text: None,
                prepared: None,
                overlay_blocks: 0,
            },
            &store(),
            ".",
        );

        assert_eq!(delivery, PermissionDelivery::ReceiverGone);
    }
}
