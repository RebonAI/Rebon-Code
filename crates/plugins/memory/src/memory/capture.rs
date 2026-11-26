//! What the store asks about a stretch of conversation, and how it keeps the
//! answer ([`rebon_instructions::memory_capture`]).
//!
//! The question puts the store's two indexes beside the conversation so the
//! answer can update a memory instead of adding its near-twin; the answer is
//! a small JSON document, each entry of which goes through the same writer
//! `SaveMemory` uses, marked `source: auto_capture`. What the person says
//! about how they want to work comes first: that is what a model is least
//! likely to save on its own and what costs the person most to repeat.

use std::path::Path;

use rebon_instructions::memory_capture::{
    first_json_object, looks_like_a_secret, CaptureAsk, MemoryCapture,
};
use rebon_session::memory_paths::{self, MemoryScope, ENTRYPOINT_NAME};
use serde_json::Value;

use crate::memory::save::{
    save_memory, MemoryContentType, MemorySource, SaveMemoryAction, SaveMemoryRequest,
};
use crate::memory::settings::is_auto_memory_enabled;

/// At most this many memories from one stretch: a pass that wants to keep
/// more is summarising the work, not remembering.
pub const MAX_KEPT: usize = 3;
const MAX_INDEX_CHARS: usize = 8_000;
const MAX_TITLE_CHARS: usize = 80;
const MAX_DESCRIPTION_CHARS: usize = 240;
const MAX_CONTENT_CHARS: usize = 2_000;

pub const CAPTURE_SYSTEM_PROMPT: &str = r#"You keep a coding assistant's long-term memory. You are shown the memory as it stands and a stretch of conversation between a person and the assistant. Decide what in the conversation is worth remembering in future, separate conversations.

Keep, most important first:
1. feedback: how the person wants the assistant to work. Corrections ("don't do X", "stop doing Y"), approaches they confirmed, standing instructions ("from now on", "always", "never"), language and style preferences. Write the rule, then why when they said why.
2. user: lasting facts about the person: their role, expertise, the tools and setup they use, how they like to collaborate.
3. project: decisions, constraints, deadlines and goals in this project that the code and its history do not show.
4. reference: where things live outside the code: URLs, dashboards, tickets, documents.

Do not keep:
- the task in progress, its steps or its outcome, which files changed, or anything the code, git history or the project's own docs already record;
- an instruction meant only for the current conversation;
- what the assistant assumed or proposed that the person did not confirm;
- text the person pasted from elsewhere, tool output, or messages relayed from another agent: only what the person said or agreed to counts;
- secrets, credentials, tokens, or contact details.

Scope: "user" for what holds across all the person's projects (feedback and user memories usually), "repo" for what is about this project.

If a memory in an index already covers the point, update it: give its file name as "target_file" and write the merged content. Never add a near-duplicate. If the person contradicted a memory, update it to what they now want.

Most stretches hold nothing worth keeping: then answer {"memories": []}. Keep at most 3.

Answer with JSON only, no prose:
{"memories": [{"scope": "user" or "repo", "type": "feedback" or "user" or "project" or "reference", "title": "short title", "description": "one line saying when this matters", "content": "the fact or rule; for feedback and project add **Why:** and **How to apply:** lines when known", "target_file": "existing-file.md, only when updating one"}]}"#;

/// The provider the plugin puts on the seat.
pub struct MemoryCaptureProvider;

impl MemoryCapture for MemoryCaptureProvider {
    fn ask(&self, cwd: &Path, conversation: &str) -> Option<CaptureAsk> {
        if conversation.trim().is_empty() || !is_auto_memory_enabled(cwd) {
            return None;
        }
        Some(ask_for(cwd, conversation))
    }

    fn keep(&self, cwd: &Path, answer: &str) -> Vec<String> {
        keep_answer(cwd, answer)
    }
}

fn ask_for(cwd: &Path, conversation: &str) -> CaptureAsk {
    let cwd_text = cwd.to_string_lossy();
    let user_index = memory_paths::user_memory_dir()
        .map(|dir| read_index(&dir))
        .unwrap_or_default();
    let repo_index = memory_paths::repo_memory_dir(&cwd_text)
        .map(|dir| read_index(&dir))
        .unwrap_or_default();
    let prompt = format!(
        "## Memory index, user scope\n{}\n\n## Memory index, repo scope ({cwd_text})\n{}\n\n## Conversation\n{}",
        or_empty(&user_index),
        or_empty(&repo_index),
        conversation.trim()
    );
    CaptureAsk {
        system: CAPTURE_SYSTEM_PROMPT.to_string(),
        prompt,
        max_tokens: 1_500,
    }
}

fn read_index(dir: &Path) -> String {
    let text = std::fs::read_to_string(dir.join(ENTRYPOINT_NAME)).unwrap_or_default();
    clip(text.trim(), MAX_INDEX_CHARS)
}

fn or_empty(index: &str) -> &str {
    if index.is_empty() {
        "(empty)"
    } else {
        index
    }
}

/// One entry of the answer's `memories`.
struct Proposal {
    scope: String,
    kind: String,
    title: String,
    description: String,
    content: String,
    target_file: Option<String>,
}

impl Proposal {
    fn from_value(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            scope: text("scope")?,
            kind: text("type")?,
            title: text("title")?,
            description: text("description")?,
            content: text("content")?,
            target_file: text("target_file"),
        })
    }
}

/// Write what the answer proposes; a proposal that does not hold up is
/// dropped on its own, the rest still kept.
pub fn keep_answer(cwd: &Path, answer: &str) -> Vec<String> {
    let Some(json) = first_json_object(answer) else {
        return Vec::new();
    };
    let Ok(answer) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(memories) = answer.get("memories").and_then(Value::as_array) else {
        return Vec::new();
    };
    let cwd_text = cwd.to_string_lossy().into_owned();
    let mut kept = Vec::new();
    for proposal in memories.iter().take(MAX_KEPT) {
        let Some(request) = Proposal::from_value(proposal).and_then(|p| request_for(&cwd_text, p))
        else {
            continue;
        };
        let title = request.title.clone().unwrap_or_default();
        if save_memory(Some(&cwd_text), request).is_ok() {
            kept.push(title);
        }
    }
    kept
}

fn request_for(cwd: &str, proposal: Proposal) -> Option<SaveMemoryRequest> {
    let scope = match proposal.scope.trim().to_ascii_lowercase().as_str() {
        "user" => MemoryScope::User,
        "repo" => MemoryScope::Repo,
        _ => return None,
    };
    let memory_type = MemoryContentType::parse(&proposal.kind).ok()?;
    let title = bounded(&proposal.title, MAX_TITLE_CHARS)?;
    let description = bounded(&proposal.description, MAX_DESCRIPTION_CHARS)?;
    let content = bounded(&proposal.content, MAX_CONTENT_CHARS)?;
    if [&title, &description, &content]
        .iter()
        .any(|text| looks_like_a_secret(text))
    {
        return None;
    }
    // An update names a file the scope already has; any other name would
    // make a new file under a name the model made up.
    let target_file = proposal
        .target_file
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .filter(|name| {
            memory_paths::memory_dir_for_scope(scope, Some(cwd))
                .is_some_and(|dir| dir.join(name).is_file())
        });
    Some(SaveMemoryRequest {
        action: SaveMemoryAction::Upsert,
        scope,
        memory_type: Some(memory_type),
        title: Some(title),
        description: Some(description),
        content: Some(content),
        source: Some(MemorySource::AutoCapture),
        reason: Some("kept automatically from the conversation".to_string()),
        dedupe_key: None,
        target_file,
    })
}

fn bounded(text: &str, max: usize) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| clip(text, max))
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config home of its own, under the crate's env lock: user and repo
    /// memory both resolve from it.
    struct TestConfigHome {
        _dir: tempfile::TempDir,
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestConfigHome {
        fn new(_name: &str) -> Self {
            let lock = crate::memory::test_env::env_test_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let names = [
                "HOME",
                "USERPROFILE",
                "REBON_CONFIG_DIR",
                "REBON_DISABLE_AUTO_MEMORY",
                "REBON_SIMPLE",
            ];
            let previous = names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            for name in ["HOME", "USERPROFILE", "REBON_CONFIG_DIR"] {
                std::env::set_var(name, dir.path());
            }
            std::env::remove_var("REBON_DISABLE_AUTO_MEMORY");
            std::env::remove_var("REBON_SIMPLE");
            Self {
                _dir: dir,
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for TestConfigHome {
        fn drop(&mut self) {
            for (name, value) in self.previous.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn answer(memories: &str) -> String {
        format!("```json\n{{\"memories\": [{memories}]}}\n```")
    }

    #[test]
    fn a_preference_the_person_stated_is_kept_in_user_memory() {
        let _home = TestConfigHome::new("capture-keep");
        let cwd = tempfile::tempdir().unwrap();
        let kept = keep_answer(
            cwd.path(),
            &answer(
                r#"{"scope": "user", "type": "feedback", "title": "Answer in Chinese",
                    "description": "Reply language for this person",
                    "content": "Reply in Chinese.\n**Why:** they read it faster."}"#,
            ),
        );
        assert_eq!(kept, ["Answer in Chinese"]);
        let dir = memory_paths::user_memory_dir().unwrap();
        let index = std::fs::read_to_string(dir.join(ENTRYPOINT_NAME)).unwrap();
        assert!(index.contains("Answer in Chinese"), "{index}");
        let file = std::fs::read_to_string(dir.join("feedback-answer-in-chinese.md")).unwrap();
        assert!(file.contains("source: auto_capture"), "{file}");

        // The next pass sees it in the index it is asked with.
        let ask = MemoryCaptureProvider.ask(cwd.path(), "Person: hi").unwrap();
        assert!(ask.prompt.contains("Answer in Chinese"), "{}", ask.prompt);
        assert!(ask.prompt.ends_with("## Conversation\nPerson: hi"));
    }

    #[test]
    fn an_update_names_an_existing_file_or_falls_back_to_the_title() {
        let _home = TestConfigHome::new("capture-update");
        let cwd = tempfile::tempdir().unwrap();
        keep_answer(
            cwd.path(),
            &answer(
                r#"{"scope": "repo", "type": "project", "title": "Release freeze",
                    "description": "When releases stop", "content": "No releases on Fridays."}"#,
            ),
        );
        let dir = memory_paths::repo_memory_dir(&cwd.path().to_string_lossy()).unwrap();
        assert!(dir.join("project-release-freeze.md").is_file());

        keep_answer(
            cwd.path(),
            &answer(
                r#"{"scope": "repo", "type": "project", "title": "Release freeze, updated",
                    "description": "When releases stop",
                    "content": "No releases on Fridays or holidays.",
                    "target_file": "project-release-freeze.md"}"#,
            ),
        );
        let file = std::fs::read_to_string(dir.join("project-release-freeze.md")).unwrap();
        assert!(file.contains("holidays"), "{file}");

        // A target the store does not have is not a file name to invent.
        keep_answer(
            cwd.path(),
            &answer(
                r#"{"scope": "repo", "type": "reference", "title": "Dashboard",
                    "description": "Where the metrics are", "content": "grafana.internal/d/app",
                    "target_file": "made-up.md"}"#,
            ),
        );
        assert!(!dir.join("made-up.md").exists());
        assert!(dir.join("reference-dashboard.md").is_file());
    }

    #[test]
    fn nothing_odd_is_kept() {
        let _home = TestConfigHome::new("capture-odd");
        let cwd = tempfile::tempdir().unwrap();
        assert!(keep_answer(cwd.path(), "no json here").is_empty());
        assert!(keep_answer(cwd.path(), r#"{"memories": []}"#).is_empty());
        assert!(keep_answer(
            cwd.path(),
            &answer(
                r#"{"scope": "task", "type": "feedback", "title": "t", "description": "d", "content": "c"},
                   {"scope": "user", "type": "mood", "title": "t", "description": "d", "content": "c"},
                   {"scope": "user", "type": "reference", "title": "Key", "description": "the key",
                    "content": "sk-abcdefghijklmnopqrstuvwxyz123456"}"#
            ),
        )
        .is_empty());
        let many = (0..5)
            .map(|n| {
                format!(
                    r#"{{"scope": "user", "type": "user", "title": "Fact {n}", "description": "d", "content": "c{n}"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(keep_answer(cwd.path(), &answer(&many)).len(), MAX_KEPT);
    }

    #[test]
    fn memory_turned_off_asks_nothing() {
        let _home = TestConfigHome::new("capture-off");
        let cwd = tempfile::tempdir().unwrap();
        assert!(MemoryCaptureProvider.ask(cwd.path(), "  ").is_none());
        std::env::set_var("REBON_DISABLE_AUTO_MEMORY", "1");
        let ask = MemoryCaptureProvider.ask(cwd.path(), "Person: always use tabs");
        std::env::remove_var("REBON_DISABLE_AUTO_MEMORY");
        assert!(ask.is_none());
    }

    #[test]
    fn json_is_found_inside_prose_and_braces_in_strings_do_not_confuse_it() {
        assert_eq!(
            first_json_object(r#"Sure: {"a": "}{", "b": {"c": 1}} trailing"#),
            Some(r#"{"a": "}{", "b": {"c": 1}}"#)
        );
        assert_eq!(first_json_object("{ unbalanced"), None);
    }
}
