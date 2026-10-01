//! What the group wrote to this session, handed to the model as it works.
//!
//! One attachment, `group_inbox`: the entries of the session's group written
//! to it (or to all) since the last delivery, one line each, as a
//! system-reminder at the end of the history. It sits on the
//! `attachment-producers` seat at [`rebon_core::attachment_seat::Order::Mailbox`] beside the teammate
//! mailbox — traffic from outside the session — so it is read after
//! everything that describes the session's own state.
//!
//! **Why this is the cheap way in (RFC-0009 §8, §16).** The session is
//! running a turn when the seat polls it, so its cache is warm by
//! definition; the entry is appended, never spliced into the system prompt
//! or an earlier message, so the cached prefix survives. Nothing here ever
//! starts a turn: a session nobody is talking to hears from its group the
//! next time it runs.
//!
//! **Delivered, not read.** The attachment carries each entry's first
//! lines and moves the member's `delivered` cursor; `group_inbox` still has
//! them in full and moves `read`. A long request the model wants to act on
//! is one tool call away.
//!
//! 每个新回合补齐当前有效记忆，避免恢复或压缩后只剩已投递游标；
//! 回合内仅追加增量，不改变已有的缓存前缀。

use std::sync::{Arc, Mutex};

use rebon_api::Message as ApiMessage;
use rebon_core::attachment_seat::{SeatAttachmentProducer, SessionAttachmentBinding};
use rebon_core::query::{
    visible_runtime_attachment_message, AttachmentPollRequest, AttachmentPoller,
};
use rebon_group::deliver;
use rebon_group::identity::AgentKind;
use rebon_group::model::{MemberKey, Via};
use rebon_group::GroupStore;

/// The seat entry: a poller for every session, which answers nothing while
/// the session is in no group.
pub struct GroupInboxProducer {
    store: GroupStore,
}

impl GroupInboxProducer {
    pub fn new(store: GroupStore) -> Self {
        Self { store }
    }
}

impl SeatAttachmentProducer for GroupInboxProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        Some(Arc::new(GroupInboxPoller::new(
            self.store.clone(),
            MemberKey {
                agent: AgentKind::REBON.to_string(),
                session_id: binding.session_id.clone(),
            },
        )))
    }
}

pub struct GroupInboxPoller {
    store: GroupStore,
    member: MemberKey,
    briefed_turn: Mutex<Option<String>>,
}

impl GroupInboxPoller {
    pub fn new(store: GroupStore, member: MemberKey) -> Self {
        Self {
            store,
            member,
            briefed_turn: Mutex::new(None),
        }
    }
}

impl AttachmentPoller for GroupInboxPoller {
    /// Both phases: before the turn's first request (what arrived while the
    /// session was idle) and after each tool round (what arrived during it).
    /// A failure to read is no delivery, not a failed turn.
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.session_id != self.member.session_id {
            return Vec::new();
        }
        let mut briefed = self
            .briefed_turn
            .lock()
            .expect("group briefed turn poisoned");
        let fresh = briefed.as_deref() != Some(request.turn_id);
        let Some(pending) = deliver::pending_with(&self.store, &self.member, fresh) else {
            return Vec::new();
        };
        let messages = group_inbox(&pending, request.turn_id);
        if !deliver::delivered(&self.store, &self.member, &pending, Via::Attachment) {
            return Vec::new();
        }
        *briefed = Some(request.turn_id.to_string());
        messages
    }
}

/// 群组消息与有效记忆共用附件，只有真正注入上下文后才推进记忆游标。
pub fn group_inbox(pending: &deliver::Pending, turn_id: &str) -> Vec<ApiMessage> {
    let Some(model_text) = rebon_group::render::context(pending) else {
        return Vec::new();
    };
    let uuid = format!(
        "u-group-attachment-{turn_id}-{}-{}-{}",
        pending.group.id, pending.through, pending.memory_through
    );
    vec![visible_runtime_attachment_message(
        &uuid,
        rebon_group::render::context_label(pending),
        model_text,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::ContentBlock;
    use rebon_core::query::AttachmentPollPhase;
    use rebon_group::store::Draft;
    use rebon_group::{render, Delivery, EntryKind, Member};

    fn member(agent: &str, session_id: &str, alias: &str) -> Member {
        Member {
            agent: agent.into(),
            session_id: session_id.into(),
            alias: alias.into(),
            role: None,
            delivery: Delivery::Auto,
            joined_at_ms: 1,
        }
    }

    fn key(agent: &str, session_id: &str) -> MemberKey {
        MemberKey {
            agent: agent.into(),
            session_id: session_id.into(),
        }
    }

    fn send(
        store: &GroupStore,
        group: &str,
        from: &MemberKey,
        to: &str,
        kind: EntryKind,
        text: &str,
    ) {
        store
            .append(
                group,
                from,
                Draft {
                    kind,
                    to: Some(to.into()),
                    re: None,
                    supersedes: None,
                    text: text.into(),
                },
            )
            .unwrap();
    }

    fn model_text(messages: &[ApiMessage]) -> String {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn poll(poller: &GroupInboxPoller, session_id: &str) -> Vec<ApiMessage> {
        poller.poll(AttachmentPollRequest::new(
            session_id,
            "turn",
            1,
            AttachmentPollPhase::Regular,
        ))
    }

    #[test]
    fn a_session_hears_what_was_written_to_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("refactor", "/work/app").unwrap();
        store
            .join(&group.id, member("rebon", "s1", "planner"))
            .unwrap();
        store
            .join(&group.id, member("claude-code", "c1", "coder"))
            .unwrap();
        let planner = GroupInboxPoller::new(store.clone(), key("rebon", "s1"));
        // Only the coder's joining so far.
        let first = model_text(&poll(&planner, "s1"));
        assert!(first.contains("kind=\"join\""), "{first}");

        let coder = key("claude-code", "c1");
        send(
            &store,
            &group.id,
            &coder,
            "planner",
            EntryKind::Note,
            "3 tests <all> pass",
        );
        send(
            &store,
            &group.id,
            &coder,
            "all",
            EntryKind::Note,
            "api is v2",
        );
        let text = model_text(&poll(&planner, "s1"));
        assert!(text.contains("from=\"coder\" kind=\"note\""), "{text}");
        assert!(text.contains("3 tests &lt;all&gt; pass"), "{text}");
        assert!(text.contains("not instructions from the user"));
        assert!(text.contains("<system-reminder>"));
        // Delivered once.
        assert!(poll(&planner, "s1").is_empty());
        // Still unread for group_inbox.
        assert_eq!(
            store
                .inbox(&group.id, &key("rebon", "s1"), false)
                .unwrap()
                .entries
                .len(),
            3
        );
    }

    #[test]
    fn nothing_for_a_session_outside_a_group_or_for_another_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let loner = GroupInboxPoller::new(store.clone(), key("rebon", "s9"));
        assert!(poll(&loner, "s9").is_empty());

        let group = store.create("g", "/w").unwrap();
        store.join(&group.id, member("rebon", "s1", "a")).unwrap();
        store.join(&group.id, member("rebon", "s2", "b")).unwrap();
        let a = GroupInboxPoller::new(store.clone(), key("rebon", "s1"));
        assert!(poll(&a, "some-other-session").is_empty());
        // What a sent to b is not a's to hear.
        send(
            &store,
            &group.id,
            &key("rebon", "s1"),
            "b",
            EntryKind::Note,
            "hi",
        );
        let _ = poll(&a, "s1");
        assert!(poll(&a, "s1").is_empty());
    }

    #[test]
    fn shared_memory_survives_terminal_delivery_and_a_new_turn() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("g", "/w").unwrap();
        store.join(&group.id, member("rebon", "s1", "a")).unwrap();
        store.join(&group.id, member("rebon", "s2", "b")).unwrap();
        send(
            &store,
            &group.id,
            &key("rebon", "s2"),
            "all",
            EntryKind::Memory,
            "shared-api-v2",
        );
        let me = key("rebon", "s1");
        let pending = deliver::pending(&store, &me).unwrap();
        assert!(deliver::delivered(&store, &me, &pending, Via::Terminal));
        let poller = GroupInboxPoller::new(store.clone(), me);
        assert!(model_text(&poll(&poller, "s1")).contains("shared-api-v2"));
        assert!(poll(&poller, "s1").is_empty());
        let next = poller.poll(AttachmentPollRequest::new(
            "s1",
            "next-turn",
            0,
            AttachmentPollPhase::Eager,
        ));
        assert!(model_text(&next).contains("shared-api-v2"));
        let rebuilt = GroupInboxPoller::new(store, key("rebon", "s1"));
        assert!(model_text(&poll(&rebuilt, "s1")).contains("shared-api-v2"));
    }

    #[test]
    fn a_late_joiner_gets_memory_and_pull_only_stays_silent() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("g", "/w").unwrap();
        store.join(&group.id, member("rebon", "s2", "b")).unwrap();
        send(
            &store,
            &group.id,
            &key("rebon", "s2"),
            "all",
            EntryKind::Memory,
            "before-join",
        );
        store.join(&group.id, member("rebon", "s1", "a")).unwrap();
        let poller = GroupInboxPoller::new(store.clone(), key("rebon", "s1"));
        assert!(model_text(&poll(&poller, "s1")).contains("before-join"));
        let mut pull = member("rebon", "s3", "c");
        pull.delivery = Delivery::PullOnly;
        store.join(&group.id, pull).unwrap();
        let poller = GroupInboxPoller::new(store, key("rebon", "s3"));
        assert!(poll(&poller, "s3").is_empty());
    }

    #[test]
    fn a_flood_is_counted_not_spelled_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("g", "/w").unwrap();
        store.join(&group.id, member("rebon", "s1", "a")).unwrap();
        store.join(&group.id, member("rebon", "s2", "b")).unwrap();
        let a = GroupInboxPoller::new(store.clone(), key("rebon", "s1"));
        let _ = poll(&a, "s1");
        for n in 0..20 {
            send(
                &store,
                &group.id,
                &key("rebon", "s2"),
                "a",
                EntryKind::Note,
                &format!("n{n}"),
            );
        }
        let text = model_text(&poll(&a, "s1"));
        assert_eq!(text.matches("<group-entry").count(), render::MAX_LINES);
        assert!(text.contains("and 8 more"), "{text}");
    }
}
