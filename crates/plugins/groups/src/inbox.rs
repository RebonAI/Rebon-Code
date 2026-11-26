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
//! **Briefed once per context.** The briefing and the whole memory go in
//! when the history the session will send holds no briefing for this
//! group: a new session, or one whose briefing a compaction summarised
//! away. Otherwise only what is new goes in. Re-sending the whole memory
//! every turn would cost its tokens on every turn for nothing; the history
//! is the record of what the model still has.

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
    /// Polls of one session do not overlap; this keeps two that might from
    /// both briefing.
    polling: Mutex<()>,
}

impl GroupInboxPoller {
    pub fn new(store: GroupStore, member: MemberKey) -> Self {
        Self {
            store,
            member,
            polling: Mutex::new(()),
        }
    }

    /// Whether the history this poll is for still holds the member's
    /// briefing. Not knowing the group yet is "no": there is nothing to
    /// deliver then anyway.
    fn context_holds_briefing(&self, request: &AttachmentPollRequest<'_>) -> bool {
        let Ok(Some(group)) = self.store.group_of(&self.member) else {
            return false;
        };
        let Some(me) = group.member(&self.member) else {
            return false;
        };
        request.history_contains(&rebon_group::render::briefing_marker(&group, &me.alias))
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
        let _polling = self.polling.lock().expect("group inbox poll poisoned");
        let fresh = !self.context_holds_briefing(&request);
        let Some(pending) = deliver::pending_with(&self.store, &self.member, fresh) else {
            return Vec::new();
        };
        let messages = group_inbox(&pending, request.turn_id);
        if !deliver::delivered(&self.store, &self.member, &pending, Via::Attachment) {
            return Vec::new();
        }
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

    /// A session as the engine runs it: what a poll injects stays in the
    /// history the next poll sees.
    struct Session {
        poller: GroupInboxPoller,
        history: std::cell::RefCell<Vec<ApiMessage>>,
    }

    impl Session {
        fn new(poller: GroupInboxPoller) -> Self {
            Self {
                poller,
                history: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn poll_as(&self, session_id: &str, turn: &str) -> Vec<ApiMessage> {
            let history = self.history.borrow().clone();
            let messages = self.poller.poll(
                AttachmentPollRequest::new(session_id, turn, 1, AttachmentPollPhase::Regular)
                    .with_history(&history),
            );
            self.history.borrow_mut().extend(messages.iter().cloned());
            messages
        }

        /// A compaction: the history becomes a summary that names nothing
        /// of the group's.
        fn compact(&self) {
            *self.history.borrow_mut() = vec![ApiMessage::user_text("summary of the work so far")];
        }
    }

    fn poll(session: &Session, session_id: &str) -> Vec<ApiMessage> {
        session.poll_as(session_id, "turn")
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
        let planner = Session::new(GroupInboxPoller::new(store.clone(), key("rebon", "s1")));
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
        let loner = Session::new(GroupInboxPoller::new(store.clone(), key("rebon", "s9")));
        assert!(poll(&loner, "s9").is_empty());

        let group = store.create("g", "/w").unwrap();
        store.join(&group.id, member("rebon", "s1", "a")).unwrap();
        store.join(&group.id, member("rebon", "s2", "b")).unwrap();
        let a = Session::new(GroupInboxPoller::new(store.clone(), key("rebon", "s1")));
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
        let poller = Session::new(GroupInboxPoller::new(store.clone(), me));
        assert!(model_text(&poll(&poller, "s1")).contains("shared-api-v2"));
        assert!(poll(&poller, "s1").is_empty());
        // A new turn over the same history: the briefing is still there, so
        // the memory is not sent again.
        assert!(poller.poll_as("s1", "next-turn").is_empty());
        // A resumed session replays the history that holds the briefing.
        let resumed = Session::new(GroupInboxPoller::new(store.clone(), key("rebon", "s1")));
        *resumed.history.borrow_mut() = poller.history.borrow().clone();
        assert!(poll(&resumed, "s1").is_empty());
        // A compaction summarised the briefing away: brief again, whole.
        poller.compact();
        let again = model_text(&poller.poll_as("s1", "after-compaction"));
        assert!(again.contains("shared-api-v2"), "{again}");
        assert!(again.contains("in the agent group"), "{again}");
        assert!(poller.poll_as("s1", "after-compaction").is_empty());
        // A fresh context for the same member (cleared) gets it too.
        let rebuilt = Session::new(GroupInboxPoller::new(store, key("rebon", "s1")));
        assert!(model_text(&poll(&rebuilt, "s1")).contains("shared-api-v2"));
    }

    #[test]
    fn memory_learned_mid_session_arrives_once_as_news() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("g", "/w").unwrap();
        store.join(&group.id, member("rebon", "s1", "a")).unwrap();
        store.join(&group.id, member("rebon", "s2", "b")).unwrap();
        let a = Session::new(GroupInboxPoller::new(store.clone(), key("rebon", "s1")));
        let _ = poll(&a, "s1");
        send(
            &store,
            &group.id,
            &key("rebon", "s2"),
            "all",
            EntryKind::Memory,
            "deploys-freeze-friday",
        );
        let news = model_text(&a.poll_as("s1", "turn-2"));
        assert!(news.contains("deploys-freeze-friday"), "{news}");
        assert!(!news.contains("in the agent group"), "{news}");
        assert!(a.poll_as("s1", "turn-3").is_empty());
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
        let poller = Session::new(GroupInboxPoller::new(store.clone(), key("rebon", "s1")));
        assert!(model_text(&poll(&poller, "s1")).contains("before-join"));
        let mut pull = member("rebon", "s3", "c");
        pull.delivery = Delivery::PullOnly;
        store.join(&group.id, pull).unwrap();
        let poller = Session::new(GroupInboxPoller::new(store, key("rebon", "s3")));
        assert!(poll(&poller, "s3").is_empty());
    }

    #[test]
    fn a_flood_is_counted_not_spelled_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("g", "/w").unwrap();
        store.join(&group.id, member("rebon", "s1", "a")).unwrap();
        store.join(&group.id, member("rebon", "s2", "b")).unwrap();
        let a = Session::new(GroupInboxPoller::new(store.clone(), key("rebon", "s1")));
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
