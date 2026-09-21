//! The teammate mailbox the team tools speak into the model's history.
//!
//! One attachment, `teammate_mailbox`: the unread messages a teammate sent
//! this agent, surfaced *between tool rounds* so a running turn sees
//! inter-agent traffic instead of waiting for turn-end. It is the read half of
//! [`SendMessage`](crate::SendMessageTool) — the write half is a tool in this
//! same plugin, and the team the messages travel through is
//! [`TeamCreate`](crate::TeamCreateTool)'s.
//!
//! It arrives after everything that describes the session's own state and
//! before the task nudge: the producer sits on the kernel's
//! `attachment-producers` seat at [`Order::Mailbox`], between the engine's own
//! session poller and this plugin's [`Order::Reminder`] nudge.
//!
//! **Two surfaces, one drain.** The lead reads its inbox through the seat
//! ([`MailboxAttachmentProducer`]); a teammate worker reads its own through
//! [`TeammateMailboxPoller`], which the coordinator hands to `run_query`
//! directly because a teammate has no session record for the seat to bind to.
//! Both go through [`drain_teammate_mailbox_for`], so both emit the same
//! shape.
//!
//! **What it reads from outside the record.** One handle off the binding:
//! [`TurnMailbox`] answers which inbox this turn drains, or `None` for a solo
//! session. The executor resolves it from the in-process `TeamManager` first
//! and the team files second, per poll, so a `TeamCreate` in this same turn
//! becomes visible without touching process-global state.
//!
//! **What stayed behind.** The mailbox files and
//! `drain_unread_mailbox_matching` are `rebon-tool`'s — the TUI and the
//! teammate loop read them without this plugin. The XML escapers were on loan
//! from `rebon_core::attachments` while the roster was still there; the
//! roster is [`crate::roster`]'s now and the escapers came with it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::roster::{escape_xml_attr, escape_xml_text};
use rebon_api::Message as ApiMessage;
use rebon_core::attachment_seat::{
    MailboxIdentity, SeatAttachmentProducer, SessionAttachmentBinding,
};
use rebon_core::query::{
    visible_runtime_attachment_message, AttachmentPollPhase, AttachmentPollRequest,
    AttachmentPoller,
};

/// One teammate mailbox message ready to be rendered into the
/// `teammate_mailbox` attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateMailboxMessage {
    pub from: String,
    pub text: String,
    pub timestamp: String,
    pub color: Option<String>,
    pub summary: Option<String>,
}

/// `teammate_mailbox` attachment.
///
/// Emits unread teammate messages as a single system-reminder user
/// message so the model sees mid-turn inter-agent traffic without
/// waiting for turn-end. Deduplication, structured-protocol-message
/// filtering, idle-notification collapse, and the mark-as-read side
/// effect all happen in the caller that fills `messages`; this is a pure
/// renderer over that snapshot.
pub fn teammate_mailbox(messages: &[TeammateMailboxMessage]) -> Vec<ApiMessage> {
    if messages.is_empty() {
        return Vec::new();
    }

    let rendered = messages
        .iter()
        .map(render_teammate_mailbox_entry)
        .collect::<Vec<_>>()
        .join("\n");

    let content = format!(
        "You have received the following message(s) from your teammate(s). \
         Read them, and respond if needed — you can use the SendMessage tool \
         to reply or forward. Do not mention this reminder.\n\n{rendered}"
    );

    let model_text = format!("<system-reminder>\n{content}\n</system-reminder>");
    let uuid = teammate_mailbox_attachment_uuid(messages);

    vec![visible_runtime_attachment_message(
        &uuid, rendered, model_text,
    )]
}

fn teammate_mailbox_attachment_uuid(messages: &[TeammateMailboxMessage]) -> String {
    let mut hasher = DefaultHasher::new();
    for message in messages {
        message.from.hash(&mut hasher);
        message.text.hash(&mut hasher);
        message.timestamp.hash(&mut hasher);
    }
    format!("u-teammate-attachment-{:016x}", hasher.finish())
}

fn render_teammate_mailbox_entry(msg: &TeammateMailboxMessage) -> String {
    let mut attrs = format!(" teammate_id=\"{}\"", escape_xml_attr(&msg.from));
    if let Some(summary) = msg.summary.as_deref() {
        attrs.push_str(&format!(" summary=\"{}\"", escape_xml_attr(summary)));
    }
    if let Some(color) = msg.color.as_deref() {
        attrs.push_str(&format!(" color=\"{}\"", escape_xml_attr(color)));
    }
    if !msg.timestamp.is_empty() {
        attrs.push_str(&format!(
            " timestamp=\"{}\"",
            escape_xml_attr(&msg.timestamp)
        ));
    }
    format!(
        "<teammate-message{attrs}>{}</teammate-message>",
        escape_xml_text(&msg.text)
    )
}

/// Drain + filter + collapse the file-backed team mailbox for one
/// identity. Shared by the seat producer (lead path) and
/// [`TeammateMailboxPoller::poll`] (teammate path) so both surfaces emit the
/// same shape of attachment data.
///
/// `drain_unread_mailbox` is a read → mutate → write cycle. Callers
/// must serialise with any other drain site in the same process —
/// for the lead, the TUI drain is guarded on `active_prompt.is_none()`;
/// for a teammate, the between-
/// worker drain in `run_teammate_loop::try_read_mailbox_prompt` runs
/// before a new worker is spawned, so the per-iteration drain inside
/// the running worker is strictly serialised by `worker.wait().await`.
pub fn drain_teammate_mailbox_for(identity: &MailboxIdentity) -> Vec<TeammateMailboxMessage> {
    // Predicate-based drain: skip AND leave unread any structured
    // protocol envelope (plan approval, shutdown, …) so the out-of-
    // band handlers retain ownership:
    // * Lead: TUI `classify_team_message` → auto-approve / lifecycle.
    // * Teammate: `run_teammate_loop::try_read_mailbox_prompt` parses
    //   `plan_approval_response` into a fresh worker prompt. Marking
    //   these as read here would starve that path and leave a
    //   teammate stuck in `awaiting_plan_approval` forever.
    let unread = match rebon_tool::drain_unread_mailbox_matching(
        &identity.team_name,
        &identity.agent_name,
        |msg| !is_structured_protocol_message(&msg.text),
    ) {
        Ok(messages) => messages,
        Err(err) => {
            tracing::warn!(
                error = %err,
                team_name = %identity.team_name,
                agent_name = %identity.agent_name,
                "teammate_mailbox: failed to drain mailbox"
            );
            return Vec::new();
        }
    };
    if unread.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<TeammateMailboxMessage> = unread
        .into_iter()
        .map(|msg| TeammateMailboxMessage {
            from: msg.from,
            text: msg.text,
            timestamp: msg.timestamp,
            color: msg.color,
            summary: msg.summary,
        })
        .collect();

    // Collapse per-agent idle notifications — keep only the most
    // recent so long idle streaks don't balloon the prompt.
    collapse_idle_notifications(&mut out);
    out
}

/// Detects routed JSON envelopes
/// the TUI handles out-of-band; we must not surface them to the model.
fn is_structured_protocol_message(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    matches!(
        value.get("type").and_then(|v| v.as_str()),
        Some(
            "plan_approval_request"
                | "plan_approval_response"
                | "shutdown_approved"
                | "shutdown_response"
                | "teammate_terminated"
                | "permission_request"
                | "permission_response"
        )
    )
}

/// Parses `text` as an `idle_notification` envelope and returns the
/// agent name if so.
fn idle_notification_agent(text: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("idle_notification") {
        return None;
    }
    value
        .get("from")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn collapse_idle_notifications(messages: &mut Vec<TeammateMailboxMessage>) {
    if messages.is_empty() {
        return;
    }
    let mut latest_idle: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        if let Some(agent) = idle_notification_agent(&msg.text) {
            latest_idle.insert(agent, i);
        }
    }
    if latest_idle.is_empty() {
        return;
    }
    let keep: std::collections::HashSet<usize> = latest_idle.values().copied().collect();
    let mut i = 0;
    messages.retain(|msg| {
        let drop_if_idle = match idle_notification_agent(&msg.text) {
            Some(_) => !keep.contains(&i),
            None => false,
        };
        i += 1;
        !drop_if_idle
    });
}

/// [`AttachmentPoller`] for the lead's own inbox, bound to one turn.
///
/// It holds the identity resolved for the turn rather than the session
/// record: the mailbox lives in files, not in `ServerState`, and nothing
/// about it is throttled or recorded there.
pub struct MailboxAttachmentPoller {
    identity: Option<MailboxIdentity>,
}

impl MailboxAttachmentPoller {
    pub fn new(identity: Option<MailboxIdentity>) -> Self {
        Self { identity }
    }
}

impl AttachmentPoller for MailboxAttachmentPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let Some(identity) = self.identity.as_ref() else {
            return Vec::new();
        };
        teammate_mailbox(&drain_teammate_mailbox_for(identity))
    }
}

/// The seat entry for the lead's inbox.
///
/// A session with no team declines the turn outright: draining is a file
/// read → write cycle, and a solo session has nothing to read.
pub struct MailboxAttachmentProducer;

impl SeatAttachmentProducer for MailboxAttachmentProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        let identity = binding.mailbox_identity()?;
        Some(Arc::new(MailboxAttachmentPoller::new(Some(identity))))
    }
}

/// Minimal [`AttachmentPoller`] for teammate workers — drains the
/// file-backed team mailbox and emits only the `teammate_mailbox`
/// attachment, skipping every session-state-backed producer (plan
/// mode, date change, skill listing, task reminder, nested memory).
/// Teammates don't have an ACP session record, so those producers
/// have no input to read from; this poller stays deliberately narrow.
///
/// Lifetime: built per-worker in `run_teammate_loop`, dropped when
/// the worker finishes. Between-worker traffic (plan approval,
/// idle notifications) is still handled by
/// `run_teammate_loop::try_read_mailbox_prompt`, which runs before
/// the next worker is spawned — structured protocol messages are
/// filtered out of the mid-turn drain so that path remains
/// authoritative.
pub struct TeammateMailboxPoller {
    identity: MailboxIdentity,
}

impl TeammateMailboxPoller {
    pub fn new(identity: MailboxIdentity) -> Self {
        Self { identity }
    }
}

impl AttachmentPoller for TeammateMailboxPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        teammate_mailbox(&drain_teammate_mailbox_for(&self.identity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::ContentBlock as ApiContentBlock;

    fn request(next_iteration: u64) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new(
            "session",
            "turn",
            next_iteration,
            AttachmentPollPhase::Regular,
        )
    }

    fn only_text(messages: &[ApiMessage]) -> String {
        messages
            .iter()
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ApiContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    fn mailbox_msg(from: &str, text: &str) -> TeammateMailboxMessage {
        TeammateMailboxMessage {
            from: from.to_string(),
            text: text.to_string(),
            timestamp: "1700000000000".into(),
            color: None,
            summary: None,
        }
    }

    #[test]
    fn teammate_mailbox_no_op_when_no_messages() {
        assert!(teammate_mailbox(&[]).is_empty());
    }

    #[test]
    fn teammate_mailbox_emits_single_attachment_with_all_messages() {
        let messages = teammate_mailbox(&[
            mailbox_msg("alice", "hello lead"),
            mailbox_msg("bob", "I'm done with task X"),
        ]);
        assert_eq!(messages.len(), 1);
        let text = only_text(&messages);
        assert!(text.contains("<rebon-visible-runtime-attachment"));
        assert!(text.contains("<rebon-visible-runtime-model-text>"));
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("teammate_id=\"alice\""));
        assert!(text.contains("hello lead"));
        assert!(text.contains("teammate_id=\"bob\""));
        assert!(text.contains("I'm done with task X"));
    }

    #[test]
    fn teammate_mailbox_escapes_xml_attrs_and_text() {
        let messages = teammate_mailbox(&[TeammateMailboxMessage {
            from: "a\"b".into(),
            text: "look at <tag>".into(),
            timestamp: "t".into(),
            color: Some("red".into()),
            summary: Some("<hi>".into()),
        }]);
        let text = only_text(&messages);
        assert!(text.contains("teammate_id=\"a&quot;b\""));
        assert!(text.contains("look at &lt;tag&gt;"));
        assert!(text.contains("summary=\"&lt;hi&gt;\""));
        assert!(text.contains("color=\"red\""));
    }

    #[test]
    fn is_structured_protocol_message_detects_known_types() {
        assert!(is_structured_protocol_message(
            r#"{"type":"plan_approval_request"}"#
        ));
        assert!(is_structured_protocol_message(
            r#"{"type":"shutdown_approved","from":"bob"}"#
        ));
        assert!(is_structured_protocol_message(
            r#"{"type":"teammate_terminated"}"#
        ));
        assert!(!is_structured_protocol_message("hello"));
        assert!(!is_structured_protocol_message(
            r#"{"type":"idle_notification"}"#
        ));
        assert!(!is_structured_protocol_message(r#"{"other":"shape"}"#));
    }

    #[test]
    fn collapse_idle_notifications_keeps_latest_per_agent_and_non_idle() {
        let mut msgs = vec![
            TeammateMailboxMessage {
                from: "alice".into(),
                text: r#"{"type":"idle_notification","from":"alice"}"#.into(),
                timestamp: "1".into(),
                color: None,
                summary: None,
            },
            TeammateMailboxMessage {
                from: "bob".into(),
                text: "real message".into(),
                timestamp: "2".into(),
                color: None,
                summary: None,
            },
            TeammateMailboxMessage {
                from: "alice".into(),
                text: r#"{"type":"idle_notification","from":"alice"}"#.into(),
                timestamp: "3".into(),
                color: None,
                summary: None,
            },
            TeammateMailboxMessage {
                from: "carol".into(),
                text: r#"{"type":"idle_notification","from":"carol"}"#.into(),
                timestamp: "4".into(),
                color: None,
                summary: None,
            },
        ];
        collapse_idle_notifications(&mut msgs);
        // 4 → 3: alice's first idle is dropped, latest kept; bob's
        // regular message kept; carol's one idle kept.
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].timestamp, "2"); // bob's real message
        assert_eq!(msgs[1].from, "alice");
        assert_eq!(msgs[1].timestamp, "3"); // alice's latest idle
        assert_eq!(msgs[2].from, "carol");
    }

    /// A session with no team is not in the turn at all — no poller, no
    /// file read.
    #[test]
    fn a_session_without_a_team_declines_the_turn() {
        let state = std::sync::Arc::new(rebon_session_state::ServerState::new());
        let record = state.create_session("/tmp/mailbox".into(), Vec::new());
        let binding = SessionAttachmentBinding::new(state, record.id);
        assert!(MailboxAttachmentProducer
            .poller_for_session(&binding)
            .is_none());
    }

    #[test]
    fn teammate_mailbox_poller_emits_attachment_for_unread_regular_messages() {
        use rebon_tool::TeamMailboxMessage as WireMsg;

        // The crate-wide fixture: points the team stores at a temp config
        // home and serialises with every other env-mutating test here.
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("mailbox-teammate");

        let team = "teammate-poller-test";
        let agent = "worker-bee";

        rebon_tool::write_mailbox_message(
            team,
            agent,
            WireMsg {
                from: "lead".into(),
                text: "please triage the failing build".into(),
                timestamp: "1".into(),
                read: false,
                color: None,
                summary: None,
            },
        )
        .expect("write regular");
        rebon_tool::write_mailbox_message(
            team,
            agent,
            WireMsg {
                from: "lead".into(),
                text: r#"{"type":"plan_approval_response","approved":true}"#.into(),
                timestamp: "2".into(),
                read: false,
                color: None,
                summary: None,
            },
        )
        .expect("write structured");

        let poller = TeammateMailboxPoller::new(MailboxIdentity {
            team_name: team.into(),
            agent_name: agent.into(),
        });

        let messages = poller.poll(request(0));
        assert_eq!(messages.len(), 1, "one system-reminder attachment");
        let text = only_text(&messages);
        assert!(text.contains("teammate_id=\"lead\""));
        assert!(text.contains("please triage the failing build"));

        // The structured plan_approval_response must stay unread so
        // `try_read_mailbox_prompt` can consume it between workers.
        // Only the regular message gets marked as read.
        let after = rebon_tool::read_mailbox(team, agent).expect("read mailbox");
        assert_eq!(after.len(), 2);
        let regular = after.iter().find(|m| m.timestamp == "1").expect("regular");
        let structured = after
            .iter()
            .find(|m| m.timestamp == "2")
            .expect("structured");
        assert!(regular.read, "regular message marked read");
        assert!(
            !structured.read,
            "structured envelope left unread for out-of-band handler"
        );

        // Second poll yields nothing (regular already drained).
        assert!(poller.poll(request(1)).is_empty());
    }

    #[test]
    fn the_lead_drain_filters_structured_protocol_messages() {
        use rebon_tool::TeamMailboxMessage as WireMsg;

        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("mailbox-lead-drain");

        let team = "teammate-mailbox-test-team";
        let agent = "team-lead";
        rebon_tool::write_mailbox_message(
            team,
            agent,
            WireMsg {
                from: "alice".into(),
                text: "hello lead".into(),
                timestamp: "100".into(),
                read: false,
                color: None,
                summary: None,
            },
        )
        .expect("write regular message");
        rebon_tool::write_mailbox_message(
            team,
            agent,
            WireMsg {
                from: "bob".into(),
                text: r#"{"type":"shutdown_approved","from":"bob"}"#.into(),
                timestamp: "200".into(),
                read: false,
                color: None,
                summary: None,
            },
        )
        .expect("write structured message");

        let identity = MailboxIdentity {
            team_name: team.into(),
            agent_name: agent.into(),
        };
        let drained = drain_teammate_mailbox_for(&identity);
        // Regular message surfaces; structured protocol message is
        // filtered out AND left unread (so TUI
        // classify_team_message / auto-approve can still process it).
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].from, "alice");
        assert_eq!(drained[0].text, "hello lead");

        let after = rebon_tool::read_mailbox(team, agent).expect("read mailbox");
        let regular = after
            .iter()
            .find(|m| m.timestamp == "100")
            .expect("regular");
        let structured = after
            .iter()
            .find(|m| m.timestamp == "200")
            .expect("structured");
        assert!(regular.read, "regular message marked read");
        assert!(
            !structured.read,
            "structured protocol envelope stays unread"
        );

        // Second drain returns nothing — the first marked only the regular
        // message as read and the structured one is filtered by predicate.
        assert!(drain_teammate_mailbox_for(&identity).is_empty());
    }
}
