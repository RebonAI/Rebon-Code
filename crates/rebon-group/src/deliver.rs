//! Handing a member what the group wrote to it.
//!
//! [`pending`] is the one read every channel starts from: the entries after
//! the member's `delivered` cursor that are its to hear. The channel shows
//! them ([`crate::render`]) and then moves the cursor past everything it
//! looked at, so the next delivery, by whatever channel, starts after it.
//! What was delivered stays unread: `group_inbox` still has it in full.
//!
//! [`hook_output`] is the hook channel end to end, for `rebon group hook`:
//! Claude Code and Codex run a command on their `UserPromptSubmit`,
//! `SessionStart` and `PostToolUse` events, hand it the session on stdin,
//! and add what it prints as `additionalContext` — at the start of the next
//! turn, or beside a tool result mid-turn. Neither starts a turn, so the
//! hook is never the thing that wakes a member.

use serde_json::{json, Value};

use crate::model::{Delivery, Entry, Group, MemberKey};
use crate::store::GroupStore;

/// What a member has not been handed yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub group: Group,
    /// The entries for this member, oldest first.
    pub entries: Vec<Entry>,
    /// The last seq looked at, the member's or not: where `delivered` moves.
    pub through: u64,
}

/// The member's undelivered entries, or `None` when it is in no group, is
/// pull-only, or has nothing new.
pub fn pending(store: &GroupStore, member: &MemberKey) -> Option<Pending> {
    let group = store.group_of(member).ok()??;
    let me = group.member(member)?;
    if me.delivery == Delivery::PullOnly {
        return None;
    }
    let alias = me.alias.clone();
    let cursor = store.cursor(&group.id, member).ok()?;
    let after = store.entries_after(&group.id, cursor.delivered).ok()?;
    let through = after.last()?.seq;
    let entries = after
        .into_iter()
        .filter(|entry| entry.is_for(&alias))
        .collect();
    Some(Pending {
        group,
        entries,
        through,
    })
}

/// Marks `pending` delivered to `member`.
pub fn delivered(store: &GroupStore, member: &MemberKey, pending: &Pending) -> bool {
    store
        .mark_delivered(&pending.group.id, member, pending.through)
        .is_ok()
}

/// The hook events that can carry context into the model.
const CONTEXT_EVENTS: &[&str] = &["UserPromptSubmit", "SessionStart", "PostToolUse"];

/// What `rebon group hook --agent <agent>` prints for one hook `input` (the
/// JSON the agent passes on stdin), or `None` to print nothing. The shape is
/// the one both Claude Code and Codex read:
/// `{"hookSpecificOutput": {"hookEventName": …, "additionalContext": …}}`.
pub fn hook_output(store: &GroupStore, agent: &str, input: &Value) -> Option<Value> {
    let event = input.get("hook_event_name").and_then(Value::as_str)?;
    if !CONTEXT_EVENTS.contains(&event) {
        return None;
    }
    let session_id = input
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let member = MemberKey {
        agent: agent.to_string(),
        session_id: session_id.to_string(),
    };
    let pending = pending(store, &member)?;
    let text = crate::render::reminder(&pending.group, &pending.entries);
    // Moved past what was looked at even when none of it was ours, so the
    // next event does not read it again.
    if !delivered(store, &member, &pending) {
        return None;
    }
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": text?,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntryKind, Member};
    use crate::store::Draft;

    fn member(agent: &str, session_id: &str, alias: &str, delivery: Delivery) -> Member {
        Member {
            agent: agent.into(),
            session_id: session_id.into(),
            alias: alias.into(),
            role: None,
            delivery,
            joined_at_ms: 1,
        }
    }

    fn setup(coder_delivery: Delivery) -> (tempfile::TempDir, GroupStore, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("refactor", "/w").unwrap();
        store
            .join(&group.id, member("rebon", "s1", "planner", Delivery::Auto))
            .unwrap();
        store
            .join(
                &group.id,
                member("claude-code", "aa38", "coder", coder_delivery),
            )
            .unwrap();
        store
            .append(
                &group.id,
                &MemberKey {
                    agent: "rebon".into(),
                    session_id: "s1".into(),
                },
                Draft {
                    kind: EntryKind::Request,
                    to: Some("coder".into()),
                    re: None,
                    supersedes: None,
                    text: "add the tests".into(),
                },
            )
            .unwrap();
        (dir, store, group.id)
    }

    fn input(event: &str) -> Value {
        json!({ "session_id": "aa38", "hook_event_name": event, "cwd": "/w", "prompt": "hi" })
    }

    #[test]
    fn a_prompt_carries_what_the_group_wrote_once() {
        let (_dir, store, _) = setup(Delivery::Auto);
        let output = hook_output(&store, "claude-code", &input("UserPromptSubmit")).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        let context = output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("kind=\"request\"") && context.contains("add the tests"));
        // Handed over once.
        assert_eq!(
            hook_output(&store, "claude-code", &input("PostToolUse")),
            None
        );
    }

    #[test]
    fn nothing_for_other_events_strangers_or_pull_only_members() {
        let (_dir, store, _) = setup(Delivery::Auto);
        assert_eq!(hook_output(&store, "claude-code", &input("Stop")), None);
        assert_eq!(
            hook_output(&store, "codex", &input("UserPromptSubmit")),
            None
        );
        assert_eq!(
            hook_output(
                &store,
                "claude-code",
                &json!({ "hook_event_name": "PostToolUse" })
            ),
            None
        );

        let (_dir, store, _) = setup(Delivery::PullOnly);
        assert_eq!(
            hook_output(&store, "claude-code", &input("UserPromptSubmit")),
            None
        );
    }
}
