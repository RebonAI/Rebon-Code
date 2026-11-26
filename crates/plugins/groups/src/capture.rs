//! What a member's turn gave its group to keep, asked of a model at the end
//! of the turn ([`rebon_instructions::memory_capture::GroupCapture`]).
//!
//! A member keeps a fact when it thinks to call `group_remember`, which an
//! agent busy with its task does rarely, and the finding it does not keep is
//! found again by the next member, or by the next worker the router sends.
//! So the engine shows the turn to a model beside the group's memory as it
//! stands, and the facts that come back are recorded as the member's own,
//! exactly as `group_remember` records them: a correction names the fact it
//! replaces.

use rebon_group::model::{EntryKind, MemberKey};
use rebon_group::store::Draft;
use rebon_group::GroupStore;
use rebon_instructions::memory_capture::{
    first_json_object, looks_like_a_secret, CaptureAsk, GroupCapture,
};
use serde_json::Value;

/// At most this many facts from one turn.
pub const MAX_KEPT: usize = 3;
const MAX_FACT_CHARS: usize = 400;
/// How much of the memory goes into the question, newest kept.
const MAX_MEMORY_CHARS: usize = 8_000;

pub const GROUP_CAPTURE_SYSTEM_PROMPT: &str = r#"You keep the shared memory of an agent group: several coding agents working in one project. Every member reads this memory when it starts and again after its context is compacted, so what is in it is what the group knows. You are shown the memory as it stands and one member's latest work: the task or the person's words, and what the member said as it worked.

Keep what another member, or the next agent to work in this project, would otherwise get wrong or have to find out again:
- a decision or ruling, and its reason;
- a convention of the project: how it builds, tests, releases, names things;
- a finding about the code, the build or the environment others will run into: a test that fails on one platform, a command that needs a flag, a file that must not be touched;
- who owns which piece of work, or a plan that changed.

Do not keep:
- progress reports, what was done in this task, or its outcome;
- what holds only for this one task;
- what the repository already records: the code itself, commit messages, the project's docs;
- guesses, or anything the member was not sure of;
- secrets, credentials, tokens, or contact details;
- anything the memory already says. When a fact in the memory is no longer true, give the correction with "replaces" set to that fact's number.

Each fact stands on its own, in one or two sentences. Most turns hold nothing new: then answer {"facts": []}. Keep at most 3.

Answer with JSON only, no prose:
{"facts": [{"text": "the fact", "replaces": null or the number of the fact it corrects}]}"#;

/// The provider the plugin puts on the seat.
pub struct GroupCaptureProvider {
    store: GroupStore,
}

impl GroupCaptureProvider {
    pub fn new(store: GroupStore) -> Self {
        Self { store }
    }

    fn key(session_id: &str) -> MemberKey {
        MemberKey {
            agent: rebon_group::identity::AgentKind::REBON.to_string(),
            session_id: session_id.to_string(),
        }
    }
}

impl GroupCapture for GroupCaptureProvider {
    fn in_group(&self, session_id: &str) -> bool {
        self.store
            .group_of(&Self::key(session_id))
            .is_ok_and(|group| group.is_some())
    }

    fn ask(&self, session_id: &str, conversation: &str) -> Option<CaptureAsk> {
        if conversation.trim().is_empty() {
            return None;
        }
        let key = Self::key(session_id);
        let group = self.store.group_of(&key).ok()??;
        let me = group.member(&key)?;
        let memory = self.store.memory(&group.id).ok()?;
        let mut lines: Vec<String> = memory
            .iter()
            .map(|fact| format!("[{}] {}", fact.seq, fact.text.trim()))
            .collect();
        let mut total: usize = lines.iter().map(|line| line.chars().count() + 1).sum();
        while total > MAX_MEMORY_CHARS && !lines.is_empty() {
            total -= lines.remove(0).chars().count() + 1;
        }
        let memory_text = if lines.is_empty() {
            "(empty)".to_string()
        } else {
            lines.join("\n")
        };
        let role = me
            .role
            .as_deref()
            .map(|role| format!(", {role}"))
            .unwrap_or_default();
        let prompt = format!(
            "## Group \"{}\", project {}\n\n## The group memory\n{memory_text}\n\n## The latest work of member \"{}\"{role}\n{}",
            group.name,
            group.cwd,
            me.alias,
            conversation.trim()
        );
        Some(CaptureAsk {
            system: GROUP_CAPTURE_SYSTEM_PROMPT.to_string(),
            prompt,
            max_tokens: 800,
        })
    }

    fn keep(&self, session_id: &str, answer: &str) -> Vec<String> {
        let key = Self::key(session_id);
        let Some(group) = self.store.group_of(&key).ok().flatten() else {
            return Vec::new();
        };
        let Ok(memory) = self.store.memory(&group.id) else {
            return Vec::new();
        };
        let mut kept = Vec::new();
        for (text, replaces) in proposals(answer) {
            // A correction names a fact the memory has; a fact already there
            // word for word is not news.
            if replaces.is_some_and(|seq| !memory.iter().any(|fact| fact.seq == seq)) {
                continue;
            }
            if memory.iter().any(|fact| same_words(&fact.text, &text)) {
                continue;
            }
            let draft = Draft {
                kind: EntryKind::Memory,
                to: None,
                re: None,
                supersedes: replaces,
                text: text.clone(),
            };
            if self.store.append(&group.id, &key, draft).is_ok() {
                kept.push(text);
            }
        }
        kept
    }
}

/// The answer's facts that hold up: text, bounded and free of secrets, and
/// the fact each replaces.
fn proposals(answer: &str) -> Vec<(String, Option<u64>)> {
    let Some(json) = first_json_object(answer) else {
        return Vec::new();
    };
    let Ok(answer) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(facts) = answer.get("facts").and_then(Value::as_array) else {
        return Vec::new();
    };
    facts
        .iter()
        .take(MAX_KEPT)
        .filter_map(|fact| {
            let text = fact.get("text").and_then(Value::as_str)?.trim();
            if text.is_empty() || looks_like_a_secret(text) {
                return None;
            }
            let text: String = text.chars().take(MAX_FACT_CHARS).collect();
            let replaces = fact.get("replaces").and_then(Value::as_u64);
            Some((text, replaces))
        })
        .collect()
}

fn same_words(a: &str, b: &str) -> bool {
    let words = |text: &str| {
        text.split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    words(a) == words(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_group::model::{Delivery, Member};

    fn setup() -> (tempfile::TempDir, GroupStore, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path().join("groups"));
        let group = store.create("router", "/w/app").unwrap();
        store
            .join(
                &group.id,
                Member {
                    agent: "rebon".into(),
                    session_id: "s1".into(),
                    alias: "fix-build".into(),
                    role: Some("router worker: Fix the build".into()),
                    delivery: Delivery::Auto,
                    joined_at_ms: 1,
                },
            )
            .unwrap();
        (dir, store, group.id)
    }

    fn remember(store: &GroupStore, group: &str, text: &str) -> u64 {
        store
            .append(
                group,
                &GroupCaptureProvider::key("s1"),
                Draft {
                    kind: EntryKind::Memory,
                    to: None,
                    re: None,
                    supersedes: None,
                    text: text.into(),
                },
            )
            .unwrap()
            .seq
    }

    #[test]
    fn the_question_shows_the_memory_and_who_did_the_work() {
        let (_dir, store, group) = setup();
        let seq = remember(&store, &group, "CI runs on macOS only");
        let provider = GroupCaptureProvider::new(store);
        let ask = provider
            .ask("s1", "Task: fix the build\nAssistant: fixed it")
            .unwrap();
        assert!(
            ask.prompt
                .contains(&format!("[{seq}] CI runs on macOS only")),
            "{}",
            ask.prompt
        );
        assert!(ask
            .prompt
            .contains("member \"fix-build\", router worker: Fix the build"));
        assert!(ask
            .prompt
            .ends_with("Task: fix the build\nAssistant: fixed it"));
        // A session in no group is not asked about.
        assert!(provider.in_group("s1") && !provider.in_group("elsewhere"));
        assert!(provider.ask("elsewhere", "Assistant: hi").is_none());
        assert!(provider.ask("s1", "  ").is_none());
    }

    #[test]
    fn facts_are_kept_as_the_members_own_and_corrections_replace() {
        let (_dir, store, group) = setup();
        let old = remember(&store, &group, "Tests run with cargo test --all");
        let provider = GroupCaptureProvider::new(store.clone());
        let answer = format!(
            r#"Here: {{"facts": [
                {{"text": "The app suite needs --skip hosted_sessions", "replaces": null}},
                {{"text": "Tests run with cargo nextest", "replaces": {old}}},
                {{"text": "tests run with  CARGO TEST --all"}},
                {{"text": "Something", "replaces": 999}},
                {{"text": "token sk-ant-abc"}}
            ]}}"#
        );
        let kept = provider.keep("s1", &answer);
        assert_eq!(
            kept,
            [
                "The app suite needs --skip hosted_sessions",
                "Tests run with cargo nextest"
            ]
        );
        let memory = store.memory(&group).unwrap();
        let texts: Vec<&str> = memory.iter().map(|fact| fact.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "The app suite needs --skip hosted_sessions",
                "Tests run with cargo nextest"
            ]
        );
        assert!(memory.iter().all(|fact| fact.from == "fix-build"));
        assert_eq!(memory[1].supersedes, Some(old));
    }

    #[test]
    fn an_answer_out_of_shape_keeps_nothing() {
        let (_dir, store, group) = setup();
        let provider = GroupCaptureProvider::new(store.clone());
        assert!(provider.keep("s1", "nothing to add").is_empty());
        assert!(provider.keep("s1", r#"{"facts": []}"#).is_empty());
        assert!(provider
            .keep("s1", r#"{"memories": [{"text": "x"}]}"#)
            .is_empty());
        assert!(provider
            .keep("other", r#"{"facts": [{"text": "x"}]}"#)
            .is_empty());
        assert!(store.memory(&group).unwrap().is_empty());
    }
}
