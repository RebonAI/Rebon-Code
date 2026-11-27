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

pub const ROUTER_CAPTURE_SYSTEM_PROMPT: &str = r#"You keep the long-term memory of Rebon's router: the one assistant the person talks to about everything they have going, which dispatches background workers into the person's projects and follows their work. The router's conversation is compacted as it grows and starts over when the router restarts; this memory is what carries over. You are shown the memory indexes as they stand and the router's latest turn: what the person said, what was delivered to the router (worker digests, events, progress checks), the router's own notes, and what it did and told the person.

Keep, most important first:
1. scope "user" — the person, in every session they have: preferences, standing instructions, corrections, how they want to be kept posted, decisions and rulings they made. Types feedback or user.
2. scope "router" — how the router should work for this person, across projects: which projects there are and what each is for, standing arrangements (how a project releases, what needs the person's approval), goals the person keeps pursuing, lessons about running workers (a kind of task that needs a stronger model or a fresh worker, a check that keeps failing for the same reason). Types project, feedback or reference.
3. scope "project", with "project" set to one of the project directories listed under "Projects in this turn" — what work there taught about that project: how a change is verified, a pitfall a failed check or a rework exposed, a decision, where things live. The project's workers read it.

Do not keep:
- task status, task or worker ids, progress, or what a single task needed: the router's ledger holds those;
- worker chatter, or a report's details that matter only to that task;
- what the router assumed or proposed that the person did not confirm;
- anything an index already covers: update that memory instead (give its file name as "target_file" and write the merged content);
- secrets, credentials, tokens, or contact details.

Most turns hold nothing worth keeping: then answer {"memories": []}. Keep at most 3.

Answer with JSON only, no prose:
{"memories": [{"scope": "user" or "router" or "project", "project": "the project directory, only for scope project", "type": "feedback" or "user" or "project" or "reference", "title": "short title", "description": "one line saying when this matters", "content": "the fact or rule; for feedback and project add **Why:** and **How to apply:** lines when known", "target_file": "existing-file.md, only when updating one"}]}"#;

/// How many projects' indexes a router question carries.
const MAX_ROUTER_PROJECTS: usize = 3;
const MAX_PROJECT_INDEX_CHARS: usize = 4_000;

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

    fn ask_router(&self, cwd: &Path, work: &str, projects: &[String]) -> Option<CaptureAsk> {
        if work.trim().is_empty() || !is_auto_memory_enabled(cwd) {
            return None;
        }
        Some(ask_router_for(cwd, work, projects))
    }

    fn keep_router(&self, cwd: &Path, answer: &str, projects: &[String]) -> Vec<String> {
        keep_router_answer(cwd, answer, projects)
    }
}

fn ask_router_for(cwd: &Path, work: &str, projects: &[String]) -> CaptureAsk {
    let cwd_text = cwd.to_string_lossy();
    let user_index = memory_paths::user_memory_dir()
        .map(|dir| read_index(&dir))
        .unwrap_or_default();
    let router_index = memory_paths::repo_memory_dir(&cwd_text)
        .map(|dir| read_index(&dir))
        .unwrap_or_default();
    let mut prompt = format!(
        "## Memory index, user scope\n{}\n\n## Memory index, router scope\n{}",
        or_empty(&user_index),
        or_empty(&router_index),
    );
    let projects: Vec<&String> = projects.iter().take(MAX_ROUTER_PROJECTS).collect();
    if projects.is_empty() {
        prompt.push_str("\n\n## Projects in this turn\n(none: keep no project memory)");
    } else {
        prompt.push_str("\n\n## Projects in this turn");
        for project in projects {
            let index = memory_paths::repo_memory_dir(project)
                .map(|dir| clip(read_index(&dir).trim(), MAX_PROJECT_INDEX_CHARS))
                .unwrap_or_default();
            prompt.push_str(&format!(
                "\n\n### {project}\nMemory index:\n{}",
                or_empty(&index)
            ));
        }
    }
    prompt.push_str(&format!("\n\n## The router's latest turn\n{}", work.trim()));
    CaptureAsk {
        system: ROUTER_CAPTURE_SYSTEM_PROMPT.to_string(),
        prompt,
        max_tokens: 1_500,
    }
}

/// [`keep_answer`] for the router: `router` is its own workspace's repo
/// memory, and `project` a project the turn named.
pub fn keep_router_answer(cwd: &Path, answer: &str, projects: &[String]) -> Vec<String> {
    let cwd_text = cwd.to_string_lossy().into_owned();
    let mut kept = Vec::new();
    for proposal in proposals(answer) {
        let (scope, place) = match proposal.scope.trim().to_ascii_lowercase().as_str() {
            "user" => (MemoryScope::User, cwd_text.clone()),
            "router" => (MemoryScope::Repo, cwd_text.clone()),
            "project" => {
                let Some(project) = proposal
                    .project
                    .as_deref()
                    .and_then(|named| projects.iter().find(|project| same_dir(project, named)))
                else {
                    continue;
                };
                (MemoryScope::Repo, project.clone())
            }
            _ => continue,
        };
        let Some(request) = build_request(&place, scope, proposal) else {
            continue;
        };
        let title = request.title.clone().unwrap_or_default();
        if save_memory(Some(&place), request).is_ok() {
            kept.push(title);
        }
    }
    kept
}

/// The same directory, however its separators and case were written.
fn same_dir(a: &str, b: &str) -> bool {
    let normal = |path: &str| {
        path.trim()
            .trim_end_matches(['/', '\\'])
            .replace('\\', "/")
            .to_lowercase()
    };
    !a.trim().is_empty() && normal(a) == normal(b)
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
    /// The router's answers name the project a memory is about.
    project: Option<String>,
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
            project: text("project"),
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
    let cwd_text = cwd.to_string_lossy().into_owned();
    let mut kept = Vec::new();
    for proposal in proposals(answer) {
        let Some(request) = request_for(&cwd_text, proposal) else {
            continue;
        };
        let title = request.title.clone().unwrap_or_default();
        if save_memory(Some(&cwd_text), request).is_ok() {
            kept.push(title);
        }
    }
    kept
}

/// The answer's first [`MAX_KEPT`] proposals in the asked-for shape.
fn proposals(answer: &str) -> Vec<Proposal> {
    let Some(json) = first_json_object(answer) else {
        return Vec::new();
    };
    let Ok(answer) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(memories) = answer.get("memories").and_then(Value::as_array) else {
        return Vec::new();
    };
    memories
        .iter()
        .take(MAX_KEPT)
        .filter_map(Proposal::from_value)
        .collect()
}

fn request_for(cwd: &str, proposal: Proposal) -> Option<SaveMemoryRequest> {
    let scope = match proposal.scope.trim().to_ascii_lowercase().as_str() {
        "user" => MemoryScope::User,
        "repo" => MemoryScope::Repo,
        _ => return None,
    };
    build_request(cwd, scope, proposal)
}

/// The write a proposal asks for, in `scope` (repo scope: of `cwd`).
fn build_request(cwd: &str, scope: MemoryScope, proposal: Proposal) -> Option<SaveMemoryRequest> {
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
    fn the_router_keeps_for_the_person_itself_and_the_projects_it_named() {
        let _home = TestConfigHome::new("capture-router");
        let router = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let project_text = project.path().to_string_lossy().to_string();
        let projects = vec![project_text.clone()];
        let named = project_text.replace('\\', "/").to_uppercase() + "/";
        let kept = keep_router_answer(
            router.path(),
            &answer(&format!(
                r#"{{"scope": "user", "type": "feedback", "title": "Report in one line",
                    "description": "How the person wants updates", "content": "One line per task."}},
                   {{"scope": "router", "type": "project", "title": "Projects",
                    "description": "What each project is", "content": "app is the desktop client."}},
                   {{"scope": "project", "project": {named:?}, "type": "project",
                    "title": "Verify with the app suite", "description": "How a change is checked",
                    "content": "cargo test --bin Rebon, hosted_sessions alone."}}"#
            )),
            &projects,
        );
        assert_eq!(
            kept,
            [
                "Report in one line",
                "Projects",
                "Verify with the app suite"
            ]
        );
        let index = |dir: std::path::PathBuf| {
            std::fs::read_to_string(dir.join(ENTRYPOINT_NAME)).unwrap_or_default()
        };
        assert!(index(memory_paths::user_memory_dir().unwrap()).contains("Report in one line"));
        let router_dir = memory_paths::repo_memory_dir(&router.path().to_string_lossy()).unwrap();
        assert!(index(router_dir).contains("Projects"));
        let project_dir = memory_paths::repo_memory_dir(&project_text).unwrap();
        assert!(index(project_dir).contains("Verify with the app suite"));

        // A project the turn did not name, or none, keeps nothing.
        let elsewhere_text = elsewhere.path().to_string_lossy().to_string();
        let stray = keep_router_answer(
            router.path(),
            &answer(&format!(
                r#"{{"scope": "project", "project": {elsewhere_text:?}, "type": "project",
                    "title": "Stray", "description": "d", "content": "c"}},
                   {{"scope": "project", "type": "project", "title": "Nowhere",
                    "description": "d", "content": "c"}},
                   {{"scope": "repo", "type": "project", "title": "Old scope",
                    "description": "d", "content": "c"}}"#
            )),
            &projects,
        );
        assert!(stray.is_empty(), "{stray:?}");

        let ask = MemoryCaptureProvider
            .ask_router(router.path(), "Delivered: digest", &projects)
            .unwrap();
        assert!(ask.system.contains("scope \"router\""));
        assert!(ask.prompt.contains("Report in one line"), "{}", ask.prompt);
        assert!(ask.prompt.contains(&format!("### {project_text}")));
        assert!(ask.prompt.contains("Verify with the app suite"));
        assert!(ask
            .prompt
            .ends_with("## The router's latest turn\nDelivered: digest"));
        let alone = MemoryCaptureProvider
            .ask_router(router.path(), "Person: hi", &[])
            .unwrap();
        assert!(alone.prompt.contains("(none: keep no project memory)"));
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
