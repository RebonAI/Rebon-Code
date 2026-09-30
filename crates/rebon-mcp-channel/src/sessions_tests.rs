//! The session tools against real files in a temp directory: a Rebon config
//! home and a Claude Code config directory, each laid out the way its agent
//! lays it out, with modification times set outright so "newest first" does
//! not depend on how fast the test writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{json, Value};

use super::*;
use crate::jobs::canonical_dir;

const CLAUDE_ID: &str = "7d3f2a10-5c4e-4b8a-9f61-0a2b3c4d5e6f";
const OTHER_CLAUDE_ID: &str = "11111111-2222-4333-8444-555555555555";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    rebon_projects: PathBuf,
    claude_home: PathBuf,
    reader: SessionReader,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("sub")).unwrap();
    let root = canonical_dir(&project).unwrap();
    let rebon_projects = dir.path().join("rebon").join("projects");
    let claude_home = dir.path().join("claude");
    let reader = SessionReader::new(
        root.clone(),
        rebon_projects.clone(),
        Some(claude_home.clone()),
    );
    Fixture {
        _dir: dir,
        root,
        rebon_projects,
        claude_home,
        reader,
    }
}

fn jsonl(lines: &[Value]) -> String {
    let mut body = lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    body.push('\n');
    body
}

fn set_mtime(path: &Path, ms: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap();
}

impl Fixture {
    fn cwd(&self) -> String {
        self.root.to_string_lossy().to_string()
    }

    fn rebon_at(&self, cwd: &str, id: &str, body: &str, mtime_ms: u64) -> PathBuf {
        let path = rebon_session::ensure_session_file_path(&self.rebon_projects, cwd, id).unwrap();
        std::fs::write(&path, body).unwrap();
        set_mtime(&path, mtime_ms);
        path
    }

    fn rebon(&self, id: &str, body: &str, mtime_ms: u64) -> PathBuf {
        self.rebon_at(&self.cwd(), id, body, mtime_ms)
    }

    fn claude_in(&self, dir_name: &str, id: &str, body: &str, mtime_ms: u64) -> PathBuf {
        let dir = self.claude_home.join("projects").join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, body).unwrap();
        set_mtime(&path, mtime_ms);
        path
    }

    fn claude(&self, id: &str, body: &str, mtime_ms: u64) -> PathBuf {
        self.claude_in(&claude_project_dir_name(&self.cwd()), id, body, mtime_ms)
    }

    fn list(&self, arguments: Value) -> anyhow::Result<Value> {
        self.reader.list(serde_json::from_value(arguments)?)
    }

    fn read(&self, arguments: Value) -> anyhow::Result<Value> {
        self.reader.read(serde_json::from_value(arguments)?)
    }
}

fn err(result: anyhow::Result<Value>) -> String {
    format!("{:#}", result.expect_err("expected a refusal"))
}

fn rows(listed: &Value) -> Vec<(String, String, String)> {
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["session_id"].as_str().unwrap().to_string(),
                row["agent"].as_str().unwrap().to_string(),
                row["title"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn user(uuid: &str, parent: Option<&str>, stamp: u32, content: Value) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent,
        "timestamp": format!("2026-09-01T00:{:02}:{:02}.000Z", stamp / 60, stamp % 60),
        "message": { "role": "user", "content": content },
    })
}

fn assistant(uuid: &str, parent: Option<&str>, stamp: u32, content: Value) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent,
        "timestamp": format!("2026-09-01T00:{:02}:{:02}.000Z", stamp / 60, stamp % 60),
        "message": { "role": "assistant", "content": content },
    })
}

/// A transcript in the shape Claude Code writes one: rows that are not
/// messages at all (no uuid), attachment and system rows on the chain, a meta
/// prompt, every content block in an assistant turn as an entry of its own,
/// thinking, and the title rows appended at the end.
fn claude_transcript() -> String {
    let session = json!(CLAUDE_ID);
    let stamp = |n: u32| format!("2026-09-02T10:00:{n:02}.000Z");
    jsonl(&[
        json!({"type":"permission-mode","permissionMode":"default","sessionId":session}),
        json!({"type":"file-history-snapshot","messageId":"m0","snapshot":{"messageId":"m0","trackedFileBackups":{},"timestamp":stamp(0)},"isSnapshotUpdate":false}),
        json!({"type":"user","uuid":"c1","parentUuid":null,"isSidechain":false,"userType":"external","cwd":"F:\\dev\\x","sessionId":session,"version":"2.1.0","gitBranch":"main","timestamp":stamp(1),
               "message":{"role":"user","content":"Why does `cargo test` fail?"}}),
        json!({"type":"attachment","uuid":"c1a","parentUuid":"c1","timestamp":stamp(2),"attachment":{"type":"hook_success","content":"hook said hello"}}),
        json!({"type":"user","uuid":"c1m","parentUuid":"c1a","isMeta":true,"timestamp":stamp(3),
               "message":{"role":"user","content":"<local-command-caveat>meta words</local-command-caveat>"}}),
        json!({"type":"assistant","uuid":"c2","parentUuid":"c1m","timestamp":stamp(4),"requestId":"req_1",
               "message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus","content":[{"type":"thinking","thinking":"private reasoning","signature":"sig"}],"stop_reason":null}}),
        json!({"type":"assistant","uuid":"c3","parentUuid":"c2","timestamp":stamp(5),
               "message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"Let me run it."}]}}),
        json!({"type":"assistant","uuid":"c4","parentUuid":"c3","timestamp":stamp(6),
               "message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test -p foo","description":"Run tests"}}],"stop_reason":"tool_use"}}),
        json!({"type":"user","uuid":"c5","parentUuid":"c4","timestamp":stamp(7),"sourceToolAssistantUUID":"c4",
               "toolUseResult":{"stdout":"","stderr":"error[E0425]","interrupted":false},
               "message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"error[E0425]: cannot find value `x`","is_error":true}]}}),
        json!({"type":"assistant","uuid":"c6","parentUuid":"c5","timestamp":stamp(8),
               "message":{"id":"msg_2","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"src/lib.rs"}}]}}),
        json!({"type":"user","uuid":"c7","parentUuid":"c6","timestamp":stamp(9),
               "message":{"role":"user","content":[{"tool_use_id":"toolu_2","type":"tool_result","content":[{"type":"text","text":"fn main() {}"}]}]}}),
        json!({"type":"system","uuid":"c7s","parentUuid":"c7","timestamp":stamp(10),"subtype":"informational","content":"a system note","level":"info"}),
        json!({"type":"assistant","uuid":"c8","parentUuid":"c7s","timestamp":stamp(11),
               "message":{"id":"msg_3","role":"assistant","content":[{"type":"text","text":"`x` is never defined; I defined it."}]}}),
        json!({"type":"user","uuid":"c9","parentUuid":"c8","timestamp":stamp(12),
               "message":{"role":"user","content":[{"type":"text","text":"thanks"},{"type":"text","text":"<system-reminder>internal</system-reminder>"}]}}),
        json!({"type":"ai-title","aiTitle":"Fixing cargo test","sessionId":session}),
        json!({"type":"last-prompt","lastPrompt":"thanks","sessionId":session}),
        json!({"type":"mode","mode":"normal","sessionId":session}),
    ])
}

const CLAUDE_TEXT: &str = "### user\nWhy does `cargo test` fail?\n\n### assistant\n\
Let me run it.\n→ Bash: cargo test -p foo\n← Bash error: error[E0425]: cannot find value `x`\n\
→ Read: src/lib.rs\n← Read: fn main() {}\n`x` is never defined; I defined it.\n\n### user\nthanks";

/// A transcript in the shape Rebon writes one: a turn's blocks in one
/// assistant entry, its tool results in one user entry.
fn rebon_transcript() -> String {
    jsonl(&[
        user("r1", None, 1, json!("tidy the imports")),
        assistant(
            "r2",
            Some("r1"),
            2,
            json!([
                {"type":"thinking","thinking":"hidden plan","signature":"s"},
                {"type":"text","text":"Checking both files."},
                {"type":"tool_use","id":"t1","name":"Grep","input":{"pattern":"^use ","path":"src"}},
                {"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/a.rs","old_string":"x","new_string":"y"}},
            ]),
        ),
        user(
            "r3",
            Some("r2"),
            3,
            json!([
                {"type":"tool_result","tool_use_id":"t1","content":"src/a.rs:1:use std::fs;"},
                {"type":"tool_result","tool_use_id":"t2","content":"","is_error":false},
            ]),
        ),
        json!({"type":"user","uuid":"r3m","parentUuid":"r3","timestamp":"2026-09-01T00:00:04.000Z",
               "message":{"role":"user","content":"runtime context","isMeta":true}}),
        assistant(
            "r4",
            Some("r3m"),
            5,
            json!([{"type":"text","text":"Done."}]),
        ),
    ])
}

const REBON_TEXT: &str = "### user\ntidy the imports\n\n### assistant\nChecking both files.\n\
→ Grep: ^use\n→ Edit: src/a.rs\n← Grep: src/a.rs:1:use std::fs;\n← Edit: (no output)\nDone.";

// ── layout ───────────────────────────────────────────────────────────

#[test]
fn claude_project_dirs_keep_case_and_dash_everything_else() {
    assert_eq!(
        claude_project_dir_name(r"F:\dev\sandbox-v2\rebon"),
        "F--dev-sandbox-v2-rebon"
    );
    assert_eq!(
        claude_project_dir_name(r"C:\Users\bon\.config\opencode"),
        "C--Users-bon--config-opencode"
    );
    assert_eq!(claude_project_dir_name("/home/me/a_b"), "-home-me-a-b");
    assert_eq!(claude_project_dir_name("/tmp/проект"), "-tmp-------");
}

#[test]
fn a_hashed_claude_dir_is_found_by_its_prefix_but_never_guessed() {
    let f = fixture();
    let cwd = format!("/work/{}", "deep/".repeat(60));
    let name = claude_project_dir_name(&cwd);
    assert!(name.len() > rebon_session::MAX_SANITIZED_LENGTH);
    let prefix = &name[..rebon_session::MAX_SANITIZED_LENGTH];
    // Claude Code's own hash of the same path, spelled another way.
    let theirs = f
        .claude_home
        .join("projects")
        .join(format!("{prefix}-otherhash"));
    std::fs::create_dir_all(&theirs).unwrap();
    assert_eq!(claude_project_dir(&f.claude_home, &cwd), Some(theirs));

    std::fs::create_dir_all(
        f.claude_home
            .join("projects")
            .join(format!("{prefix}-secondhash")),
    )
    .unwrap();
    assert_eq!(
        claude_project_dir(&f.claude_home, &cwd),
        None,
        "two directories share the prefix: neither is guessed"
    );
    // A short name is never matched by prefix.
    std::fs::create_dir_all(f.claude_home.join("projects").join("-work-a-more")).unwrap();
    assert_eq!(claude_project_dir(&f.claude_home, "/work/a"), None);
}

#[cfg(windows)]
#[test]
fn a_claude_project_opened_under_another_case_is_found() {
    let f = fixture();
    let lower = claude_project_dir_name(&f.cwd()).to_lowercase();
    f.claude_in(
        &lower,
        CLAUDE_ID,
        &jsonl(&[json!({"type":"last-prompt","lastPrompt":"do it"})]),
        1_000,
    );
    let listed = f.list(json!({ "agent": "claude-code" })).unwrap();
    assert_eq!(
        rows(&listed),
        vec![(CLAUDE_ID.into(), "claude-code".into(), "do it".into())]
    );
    let read = f.read(json!({ "session_id": CLAUDE_ID })).unwrap();
    assert_eq!(read["agent"], "claude-code");
}

#[test]
fn claude_code_config_follows_its_variable_then_the_home() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        let map: HashMap<&str, OsString> = pairs
            .iter()
            .map(|(key, value)| (*key, OsString::from(value)))
            .collect();
        claude_config_dir_with_env(move |name| map.get(name).cloned())
    };
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    assert_eq!(
        env(&[
            ("CLAUDE_CONFIG_DIR", "/elsewhere/claude"),
            ("HOME", "/h"),
            ("USERPROFILE", "/h")
        ]),
        Some(PathBuf::from("/elsewhere/claude"))
    );
    let blank = match home {
        "HOME" => env(&[("CLAUDE_CONFIG_DIR", "  "), ("HOME", "/h")]),
        _ => env(&[("CLAUDE_CONFIG_DIR", "  "), ("USERPROFILE", "/h")]),
    };
    assert_eq!(blank, Some(PathBuf::from("/h").join(".claude")));
    assert_eq!(env(&[]), None, "no variable and no home: nowhere to look");
}

// ── sessions_list ────────────────────────────────────────────────────

#[test]
fn both_agents_are_listed_together_newest_first() {
    let f = fixture();
    let newest = f.claude(
        CLAUDE_ID,
        &jsonl(&[json!({"type":"ai-title","aiTitle":"Newest"})]),
        4_000,
    );
    f.rebon(
        "aaaaa-aaaaa-aaaaa-aaaaa",
        &jsonl(&[user("u", None, 1, json!("third"))]),
        3_000,
    );
    f.claude(
        OTHER_CLAUDE_ID,
        &jsonl(&[json!({"type":"last-prompt","lastPrompt":"second"})]),
        2_000,
    );
    f.rebon(
        "bbbbb-bbbbb-bbbbb-bbbbb",
        &jsonl(&[user("u", None, 1, json!("oldest"))]),
        1_000,
    );

    let listed = f.list(json!({})).unwrap();
    assert_eq!(
        rows(&listed),
        vec![
            (CLAUDE_ID.into(), "claude-code".into(), "Newest".into()),
            (
                "aaaaa-aaaaa-aaaaa-aaaaa".into(),
                "rebon".into(),
                "third".into()
            ),
            (
                OTHER_CLAUDE_ID.into(),
                "claude-code".into(),
                "second".into()
            ),
            (
                "bbbbb-bbbbb-bbbbb-bbbbb".into(),
                "rebon".into(),
                "oldest".into()
            ),
        ]
    );
    let first = &listed["sessions"][0];
    assert_eq!(first["cwd"], f.cwd());
    assert_eq!(first["updated_at"], "1970-01-01T00:00:04.000Z");
    assert_eq!(
        first["size_bytes"],
        std::fs::metadata(&newest).unwrap().len()
    );
    assert_eq!(listed["more"], false);

    let only_rebon = f.list(json!({ "agent": "rebon" })).unwrap();
    assert!(rows(&only_rebon).iter().all(|row| row.1 == "rebon"));
    assert_eq!(rows(&only_rebon).len(), 2);
    let only_claude = f.list(json!({ "agent": "claude-code" })).unwrap();
    assert!(rows(&only_claude).iter().all(|row| row.1 == "claude-code"));
    assert_eq!(rows(&only_claude).len(), 2);

    let page = f.list(json!({ "limit": 2 })).unwrap();
    assert_eq!(rows(&page).len(), 2);
    assert_eq!(page["more"], true);
    let clamped = f.list(json!({ "limit": 0 })).unwrap();
    assert_eq!(rows(&clamped).len(), 1, "a limit of 0 is read as 1");
    assert_eq!(
        rows(&f.list(json!({ "limit": 1000 })).unwrap()).len(),
        4,
        "a limit past the cap is the cap, not an error"
    );
}

#[test]
fn listing_skips_what_is_not_a_conversation() {
    let f = fixture();
    let said = jsonl(&[json!({"type":"last-prompt","lastPrompt":"hi"})]);
    // Claude Code: only UUID-named files with something said in them.
    f.claude("notes", &said, 1_000);
    f.claude(OTHER_CLAUDE_ID, "", 1_000);
    f.claude(
        "22222222-2222-4222-8222-222222222222",
        &jsonl(&[
            json!({"type":"mode","mode":"normal"}),
            json!({"type":"permission-mode","permissionMode":"default"}),
        ]),
        1_000,
    );
    f.claude(CLAUDE_ID, &said, 1_000);
    // Rebon: sidecars, an empty file, a hidden session, a transcript with
    // nothing said, and a name that is not an id.
    let kept = f.rebon(
        "kept1-kept1-kept1-kept1",
        &jsonl(&[user("u", None, 1, json!("real"))]),
        1_000,
    );
    let project = kept.parent().unwrap().to_path_buf();
    std::fs::write(project.join("kept1-kept1-kept1-kept1.meta.json"), "{}").unwrap();
    std::fs::write(project.join("kept1-kept1-kept1-kept1.compact.json"), "{}").unwrap();
    f.rebon("empty-empty-empty-empty", "", 1_000);
    f.rebon(
        "hide1-hide1-hide1-hide1",
        &jsonl(&[user("u", None, 1, json!("internal"))]),
        1_000,
    );
    rebon_session::save_session_hidden_from_chats(
        &f.rebon_projects,
        &f.cwd(),
        "hide1-hide1-hide1-hide1",
        true,
    )
    .unwrap();
    f.rebon("mute1-mute1-mute1-mute1", "{}\n", 1_000);
    std::fs::write(project.join("not.an.id.jsonl"), "{}\n").unwrap();

    let listed = f.list(json!({})).unwrap();
    let mut ids: Vec<String> = rows(&listed).into_iter().map(|row| row.0).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![CLAUDE_ID.to_string(), "kept1-kept1-kept1-kept1".to_string()]
    );
}

#[test]
fn a_project_neither_agent_has_seen_lists_nothing() {
    let f = fixture();
    let listed = f.list(json!({})).unwrap();
    assert_eq!(listed["sessions"], json!([]));
    assert_eq!(listed["more"], false);
}

#[test]
fn claude_titles_fall_back_from_name_to_generated_to_prompt() {
    assert_eq!(
        title_from_lines(r#"{"type":"last-prompt","lastPrompt":"fix it"}"#),
        Some("fix it".into())
    );
    assert_eq!(
        title_from_lines(
            "{\"type\":\"ai-title\",\"aiTitle\":\"First\"}\n{\"type\":\"last-prompt\",\"lastPrompt\":\"fix it\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Settled\"}"
        ),
        Some("Settled".into()),
        "a generated title beats the prompt, and the latest one wins"
    );
    assert_eq!(
        title_from_lines(
            "{\"type\":\"agent-name\",\"agentName\":\"Mine\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Generated\"}"
        ),
        Some("Mine".into()),
        "the user's name beats a later generated title"
    );
    assert_eq!(
        title_from_lines(
            "{\"type\":\"agent-name\",\"agentName\":\"  \"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Kept\"}"
        ),
        Some("Kept".into()),
        "a blank name is no name"
    );
    assert_eq!(
        title_from_lines("not json \"ai-title\"\n{\"type\":\"user\"}"),
        None
    );
}

#[test]
fn a_claude_title_is_read_from_the_tail_alone() {
    let f = fixture();
    let filler = json!({"type":"assistant","message":{"content":"x".repeat(1024)}}).to_string();
    let mut lines = vec![r#"{"type":"ai-title","aiTitle":"Early"}"#.to_string()];
    lines.extend(std::iter::repeat_n(filler.clone(), 80));
    lines.push(r#"{"type":"ai-title","aiTitle":"Late"}"#.to_string());
    f.claude(CLAUDE_ID, &lines.join("\n"), 2_000);
    // Its only title is more than the tail window from the end.
    let mut buried = vec![r#"{"type":"ai-title","aiTitle":"Buried"}"#.to_string()];
    buried.extend(std::iter::repeat_n(filler, 80));
    f.claude(OTHER_CLAUDE_ID, &buried.join("\n"), 1_000);

    let listed = f.list(json!({ "agent": "claude-code" })).unwrap();
    assert_eq!(
        rows(&listed),
        vec![(CLAUDE_ID.into(), "claude-code".into(), "Late".into())]
    );
}

#[test]
fn a_rebon_title_is_its_sidecar_then_its_first_prompt_cut_short() {
    let f = fixture();
    f.rebon(
        "named-named-named-named",
        &jsonl(&[user("u", None, 1, json!("the prompt"))]),
        2_000,
    );
    rebon_session::save_session_title(
        &f.rebon_projects,
        &f.cwd(),
        "named-named-named-named",
        "Named session",
    )
    .unwrap();
    let long = "word ".repeat(100);
    f.rebon(
        "plain-plain-plain-plain",
        &jsonl(&[user(
            "u",
            None,
            1,
            json!([{"type":"text","text": long.clone()}]),
        )]),
        1_000,
    );
    let listed = f.list(json!({})).unwrap();
    let rows = rows(&listed);
    assert_eq!(rows[0].2, "Named session");
    assert!(rows[1].2.starts_with("word word"));
    assert!(rows[1].2.chars().count() <= TITLE_MAX_CHARS);
    assert!(rows[1].2.ends_with('…'));
}

/// Rebon writes the turn's runtime context as a meta user row ahead of the
/// prompt; the title is the prompt, not the context.
#[test]
fn a_rebon_title_passes_over_the_context_the_harness_injected() {
    let f = fixture();
    f.rebon(
        "ctx01-ctx01-ctx01-ctx01",
        &jsonl(&[
            json!({"type":"user","uuid":"rc","parentUuid":null,"isMeta":true,"runtimeContext":true,
                   "timestamp":"2026-09-01T00:00:00.000Z",
                   "message":{"role":"user","content":"<system-reminder>\n<runtime_context>\ngitStatus: clean"}}),
            user(
                "u1",
                Some("rc"),
                1,
                json!([
                    {"type":"text","text":"<system-reminder>an injected note</system-reminder>"},
                    {"type":"text","text":"/theme dark"},
                ]),
            ),
        ]),
        1_000,
    );
    let listed = f.list(json!({})).unwrap();
    assert_eq!(
        rows(&listed),
        vec![(
            "ctx01-ctx01-ctx01-ctx01".into(),
            "rebon".into(),
            "/theme dark".into()
        )]
    );
    let only_context = jsonl(&[
        json!({"type":"user","uuid":"rc","parentUuid":null,"isMeta":true,
        "timestamp":"2026-09-01T00:00:00.000Z","message":{"role":"user","content":"context only"}}),
    ]);
    f.rebon("ctx02-ctx02-ctx02-ctx02", &only_context, 2_000);
    assert_eq!(
        rows(&f.list(json!({})).unwrap()).len(),
        1,
        "a session where only the harness spoke has nothing said in it"
    );
}

#[test]
fn claude_code_is_optional_until_it_is_asked_for() {
    let f = fixture();
    f.rebon(
        "aaaaa-aaaaa-aaaaa-aaaaa",
        &jsonl(&[user("u", None, 1, json!("hi"))]),
        1_000,
    );
    let homeless = SessionReader::new(f.root.clone(), f.rebon_projects.clone(), None);
    let all = homeless
        .list(serde_json::from_value(json!({})).unwrap())
        .unwrap();
    assert_eq!(rows(&all).len(), 1, "Rebon's sessions still list");
    for refused in [
        homeless.list(serde_json::from_value(json!({ "agent": "claude-code" })).unwrap()),
        homeless.read(
            serde_json::from_value(json!({ "session_id": CLAUDE_ID, "agent": "claude-code" }))
                .unwrap(),
        ),
    ] {
        assert!(err(refused).contains("CLAUDE_CONFIG_DIR"));
    }
}

// ── scope and ids ────────────────────────────────────────────────────

#[test]
fn a_cwd_outside_the_root_is_refused_by_both_tools() {
    let f = fixture();
    let elsewhere = tempfile::tempdir().unwrap();
    for cwd in [
        elsewhere.path().to_string_lossy().to_string(),
        "..".to_string(),
        "sub/../..".to_string(),
    ] {
        let listed = err(f.list(json!({ "cwd": cwd })));
        assert!(listed.contains("outside"), "{cwd}: {listed}");
        let read = err(f.read(json!({ "session_id": CLAUDE_ID, "cwd": cwd })));
        assert!(read.contains("outside"), "{cwd}: {read}");
    }
    assert!(err(f.list(json!({ "cwd": "no-such-dir" }))).contains("not a directory"));

    // A directory inside the root is its own project to both agents.
    let sub = f.root.join("sub").to_string_lossy().to_string();
    f.rebon_at(
        &sub,
        "inner-inner-inner-inner",
        &jsonl(&[user("u", None, 1, json!("in sub"))]),
        1_000,
    );
    f.rebon(
        "outer-outer-outer-outer",
        &jsonl(&[user("u", None, 1, json!("at root"))]),
        1_000,
    );
    let listed = f.list(json!({ "cwd": "sub" })).unwrap();
    assert_eq!(
        rows(&listed),
        vec![(
            "inner-inner-inner-inner".into(),
            "rebon".into(),
            "in sub".into()
        )]
    );
    assert_eq!(listed["cwd"], sub);
    let read = f
        .read(json!({ "session_id": "inner-inner-inner-inner", "cwd": "sub" }))
        .unwrap();
    assert_eq!(read["text"], "### user\nin sub");
}

#[test]
fn ids_are_checked_before_they_touch_the_disk() {
    let f = fixture();
    let long = "a".repeat(MAX_SESSION_ID_LEN + 1);
    for bad in [
        "../x",
        "a/b",
        r"a\b",
        "",
        "  ",
        "a.b",
        "a b",
        "C:x",
        long.as_str(),
    ] {
        let refused = err(f.read(json!({ "session_id": bad })));
        assert!(refused.contains("not a session id"), "{bad:?}: {refused}");
    }
    assert!(
        err(f.read(json!({ "session_id": "abc", "agent": "claude-code" })))
            .contains("not a Claude Code session id")
    );
    assert!(
        serde_json::from_value::<ReadRequest>(json!({ "session_id": "a", "agent": "codex" }))
            .is_err()
    );
    assert!(serde_json::from_value::<ListRequest>(json!({ "agent": "codex" })).is_err());
    assert!(
        serde_json::from_value::<ReadRequest>(json!({ "session_id": "a", "path": "/etc/passwd" }))
            .is_err(),
        "a read takes an id, never a path"
    );
    assert!(is_uuid(CLAUDE_ID));
    assert!(is_uuid("7D3F2A10-5C4E-4B8A-9F61-0A2B3C4D5E6F"));
    assert!(!is_uuid("7d3f2a10-5c4e-4b8a-9f61-0a2b3c4d5e6"));
    assert!(!is_uuid("7d3f2a10x5c4e-4b8a-9f61-0a2b3c4d5e6f"));
    assert!(!is_uuid("aaaaa-aaaaa-aaaaa-aaaaa"));
}

#[test]
fn an_unknown_session_is_reported_not_guessed() {
    let f = fixture();
    let missing = err(f.read(json!({ "session_id": CLAUDE_ID })));
    assert!(
        missing.contains("no Rebon or Claude Code session"),
        "{missing}"
    );
    let missing = err(f.read(json!({ "session_id": "zzzzz-zzzzz-zzzzz-zzzzz" })));
    assert!(missing.contains("no Rebon session"), "{missing}");
    // Claude Code's session asked for as Rebon's is not Rebon's.
    f.claude(CLAUDE_ID, &claude_transcript(), 1_000);
    let missing = err(f.read(json!({ "session_id": CLAUDE_ID, "agent": "rebon" })));
    assert!(missing.contains("no Rebon session"), "{missing}");
}

#[test]
fn a_uuid_claude_code_does_not_have_is_looked_up_as_rebons() {
    let f = fixture();
    f.rebon(CLAUDE_ID, &rebon_transcript(), 1_000);
    let read = f.read(json!({ "session_id": CLAUDE_ID })).unwrap();
    assert_eq!(read["agent"], "rebon");
    assert_eq!(read["text"], REBON_TEXT);
}

// ── session_read ─────────────────────────────────────────────────────

#[test]
fn a_claude_code_transcript_reads_as_its_conversation() {
    let f = fixture();
    f.claude(CLAUDE_ID, &claude_transcript(), 1_000);
    let read = f.read(json!({ "session_id": CLAUDE_ID })).unwrap();
    assert_eq!(read["agent"], "claude-code");
    assert_eq!(read["title"], "Fixing cargo test");
    assert_eq!(read["text"], CLAUDE_TEXT);
    let text = read["text"].as_str().unwrap();
    for hidden in [
        "private reasoning",
        "meta words",
        "hook said hello",
        "a system note",
        "internal",
    ] {
        assert!(!text.contains(hidden), "{hidden} leaked into {text}");
    }
    assert_eq!(
        read["entries"], 8,
        "every message entry, not the rows around them"
    );
    assert_eq!(read["truncated"], false);
    assert_eq!(read["cursor_reset"], false);
    assert_eq!(read["next_cursor"], "c9");
    assert!(read.get("note").is_none());
}

#[test]
fn a_rebon_transcript_reads_the_same_way() {
    let f = fixture();
    f.rebon("aaaaa-aaaaa-aaaaa-aaaaa", &rebon_transcript(), 1_000);
    rebon_session::save_session_title(
        &f.rebon_projects,
        &f.cwd(),
        "aaaaa-aaaaa-aaaaa-aaaaa",
        "Import tidy",
    )
    .unwrap();
    let read = f
        .read(json!({ "session_id": "aaaaa-aaaaa-aaaaa-aaaaa" }))
        .unwrap();
    assert_eq!(read["agent"], "rebon");
    assert_eq!(read["title"], "Import tidy");
    assert_eq!(read["text"], REBON_TEXT);
    assert!(!read["text"].as_str().unwrap().contains("hidden plan"));
    assert!(!read["text"].as_str().unwrap().contains("runtime context"));
    assert_eq!(read["next_cursor"], "r4");
}

#[test]
fn a_compacted_claude_conversation_opens_with_its_summary() {
    let f = fixture();
    f.claude(
        CLAUDE_ID,
        &jsonl(&[
            user("old", None, 1, json!("before the compaction")),
            json!({"type":"system","subtype":"compact_boundary","uuid":"b","parentUuid":null,"logicalParentUuid":"old",
                   "timestamp":"2026-09-01T00:00:02.000Z","content":"Conversation compacted"}),
            json!({"type":"user","uuid":"s","parentUuid":"b","isCompactSummary":true,"isVisibleInTranscriptOnly":true,
                   "timestamp":"2026-09-01T00:00:03.000Z",
                   "message":{"role":"user","content":"We were fixing the build."}}),
            assistant("a", Some("s"), 4, json!([{"type":"text","text":"Continuing."}])),
        ]),
        1_000,
    );
    let read = f.read(json!({ "session_id": CLAUDE_ID })).unwrap();
    assert_eq!(
        read["text"],
        "### summary of the earlier conversation\nWe were fixing the build.\n\n\
         ### assistant\nContinuing."
    );
}

#[test]
fn a_conversation_with_nothing_said_reads_as_such() {
    let f = fixture();
    f.claude(
        CLAUDE_ID,
        &jsonl(&[json!({"type":"ai-title","aiTitle":"Empty"})]),
        1_000,
    );
    let read = f.read(json!({ "session_id": CLAUDE_ID })).unwrap();
    assert_eq!(read["entries"], 0);
    assert_eq!(read["next_cursor"], Value::Null);
    assert!(read["text"]
        .as_str()
        .unwrap()
        .contains("Nothing has been said"));
}

#[test]
fn tool_lines_show_one_argument_and_a_short_result() {
    let names: HashMap<String, String> = [("t1".to_string(), "Bash".to_string())].into();
    assert_eq!(
        tool_call_line(
            &json!({"type":"tool_use","name":"mcp__db__query","input":{"sql":"select 1"}})
        ),
        r#"→ mcp__db__query: {"sql":"select 1"}"#,
        "a tool with no recorded argument shows its input"
    );
    assert_eq!(
        tool_call_line(&json!({"type":"tool_use","name":"EnterPlanMode","input":{}})),
        "→ EnterPlanMode"
    );
    assert_eq!(
        tool_call_line(
            &json!({"type":"tool_use","name":"Task","input":{"prompt":"look around","description":"x"}})
        ),
        "→ Task: look around",
        "an alias finds its tool's argument"
    );
    let long = tool_call_line(
        &json!({"type":"tool_use","name":"Bash","input":{"command":"x".repeat(1000)}}),
    );
    assert!(long.chars().count() <= "→ Bash: ".chars().count() + TOOL_INPUT_MAX_CHARS);
    assert!(long.ends_with('…'));

    let result = |content: Value, is_error: bool| {
        tool_result_line(
            &json!({"type":"tool_result","tool_use_id":"t1","content":content,"is_error":is_error}),
            &names,
        )
    };
    assert_eq!(result(json!("ok\n\nfine"), false), "← Bash: ok fine");
    assert_eq!(result(json!("boom"), true), "← Bash error: boom");
    assert_eq!(result(json!([]), false), "← Bash: (no output)");
    assert_eq!(
        result(
            json!([{"type":"text","text":"shot"},{"type":"image","source":{}}]),
            false
        ),
        "← Bash: shot [an image]"
    );
    let long = result(json!("y".repeat(5000)), false);
    assert!(long.chars().count() <= "← Bash: ".chars().count() + TOOL_RESULT_MAX_CHARS);
    assert_eq!(
        tool_result_line(
            &json!({"type":"tool_result","tool_use_id":"unknown","content":"z"}),
            &names
        ),
        "← tool: z",
        "a result whose call is not on the chain still reads"
    );
}

// ── paging ───────────────────────────────────────────────────────────

/// `turns` user/assistant exchanges, each message `size` characters and
/// numbered so a test can tell which ones came back.
fn long_conversation(turns: usize, size: usize) -> String {
    let mut lines = Vec::new();
    let mut parent: Option<String> = None;
    for turn in 0..turns {
        let question = format!("u{turn}");
        let answer = format!("a{turn}");
        lines.push(user(
            &question,
            parent.as_deref(),
            (turn * 2) as u32,
            json!(format!("question {turn} {}", "q".repeat(size))),
        ));
        lines.push(assistant(
            &answer,
            Some(&question),
            (turn * 2 + 1) as u32,
            json!([{"type":"text","text":format!("answer {turn} {}", "a".repeat(size))}]),
        ));
        parent = Some(answer);
    }
    jsonl(&lines)
}

#[test]
fn without_a_cursor_the_newest_part_that_fits_comes_back() {
    let f = fixture();
    f.claude(CLAUDE_ID, &long_conversation(10, 400), 1_000);
    let read = f
        .read(json!({ "session_id": CLAUDE_ID, "max_chars": 2000 }))
        .unwrap();
    let text = read["text"].as_str().unwrap();
    assert_eq!(read["truncated"], true);
    assert!(read["note"].as_str().unwrap().contains("latest part"));
    assert!(text.ends_with(&format!("answer 9 {}", "a".repeat(400))));
    assert!(!text.contains("question 0 "));
    assert!(text.chars().count() <= 2000, "{}", text.chars().count());
    assert_eq!(
        read["next_cursor"], "a9",
        "the tail is always read to the end"
    );
    let entries = read["entries"].as_u64().unwrap();
    assert!((2..20).contains(&entries), "{entries}");

    let whole = f
        .read(json!({ "session_id": CLAUDE_ID, "max_chars": 100_000 }))
        .unwrap();
    assert_eq!(whole["truncated"], false);
    assert_eq!(whole["entries"], 20);
    assert!(whole["text"]
        .as_str()
        .unwrap()
        .starts_with("### user\nquestion 0 "));
    // Past the cap is the cap; below the floor is the floor.
    let floor = f
        .read(json!({ "session_id": CLAUDE_ID, "max_chars": 1 }))
        .unwrap();
    assert_eq!(
        floor["entries"], 2,
        "max_chars 1 is read as {MIN_READ_CHARS}"
    );
}

#[test]
fn a_cursor_pages_forward_through_everything_exactly_once() {
    let f = fixture();
    f.claude(CLAUDE_ID, &long_conversation(10, 400), 1_000);
    let mut after = "u0".to_string();
    let mut seen = Vec::new();
    for _ in 0..40 {
        let read = f
            .read(json!({ "session_id": CLAUDE_ID, "after": after, "max_chars": 1000 }))
            .unwrap();
        assert_eq!(read["cursor_reset"], false);
        let text = read["text"].as_str().unwrap();
        for line in text.lines() {
            if let Some(rest) = line
                .strip_prefix("question ")
                .or_else(|| line.strip_prefix("answer "))
            {
                seen.push(format!(
                    "{} {}",
                    &line[..1],
                    rest.split(' ').next().unwrap()
                ));
            }
        }
        after = read["next_cursor"].as_str().unwrap().to_string();
        if read["truncated"] == false {
            break;
        }
        assert!(read["note"].as_str().unwrap().contains("next_cursor"));
    }
    let mut expected = vec!["a 0".to_string()];
    for turn in 1..10 {
        expected.push(format!("q {turn}"));
        expected.push(format!("a {turn}"));
    }
    assert_eq!(seen, expected, "in order, none twice, none skipped");
    assert_eq!(after, "a9");

    let nothing = f
        .read(json!({ "session_id": CLAUDE_ID, "after": "a9" }))
        .unwrap();
    assert_eq!(nothing["entries"], 0);
    assert_eq!(nothing["truncated"], false);
    assert_eq!(nothing["next_cursor"], "a9");
    assert!(nothing["text"].as_str().unwrap().contains("Nothing new"));
}

#[test]
fn a_cursor_over_skipped_rows_moves_past_them() {
    let f = fixture();
    f.claude(CLAUDE_ID, &claude_transcript(), 1_000);
    // After the first prompt come an attachment and a meta prompt; the
    // cursor lands on the last row read, and those are not read again.
    let read = f
        .read(json!({ "session_id": CLAUDE_ID, "after": "c1" }))
        .unwrap();
    assert!(read["text"]
        .as_str()
        .unwrap()
        .starts_with("### assistant\nLet me run it."));
    assert_eq!(read["next_cursor"], "c9");
    let tail = f
        .read(json!({ "session_id": CLAUDE_ID, "after": "c8" }))
        .unwrap();
    assert_eq!(tail["text"], "### user\nthanks");
}

#[test]
fn a_cursor_the_conversation_was_rewound_past_resets_to_the_tail() {
    let f = fixture();
    // u1 → a1 → u2 → a2, then rewound to a1 and continued: u2b → a2b.
    f.claude(
        CLAUDE_ID,
        &jsonl(&[
            user("u1", None, 1, json!("first")),
            assistant("a1", Some("u1"), 2, json!([{"type":"text","text":"one"}])),
            user("u2", Some("a1"), 3, json!("abandoned")),
            assistant(
                "a2",
                Some("u2"),
                4,
                json!([{"type":"text","text":"old answer"}]),
            ),
            user("u2b", Some("a1"), 5, json!("instead")),
            assistant(
                "a2b",
                Some("u2b"),
                6,
                json!([{"type":"text","text":"new answer"}]),
            ),
        ]),
        1_000,
    );
    for stale in ["a2", "never-was"] {
        let read = f
            .read(json!({ "session_id": CLAUDE_ID, "after": stale }))
            .unwrap();
        assert_eq!(read["cursor_reset"], true, "{stale}");
        assert!(read["note"].as_str().unwrap().contains("rewound"));
        let text = read["text"].as_str().unwrap();
        assert_eq!(
            text,
            "### user\nfirst\n\n### assistant\none\n\n### user\ninstead\n\n### assistant\nnew answer"
        );
        assert!(!text.contains("abandoned"));
        assert_eq!(read["next_cursor"], "a2b");
    }
    let on_chain = f
        .read(json!({ "session_id": CLAUDE_ID, "after": "a1" }))
        .unwrap();
    assert_eq!(on_chain["cursor_reset"], false);
    assert_eq!(
        on_chain["text"],
        "### user\ninstead\n\n### assistant\nnew answer"
    );
}

#[test]
fn a_message_bigger_than_the_budget_is_cut_not_dropped() {
    let f = fixture();
    let big = format!("START{}END", "m".repeat(5000));
    f.claude(
        CLAUDE_ID,
        &jsonl(&[
            user("u1", None, 1, json!("go")),
            assistant("a1", Some("u1"), 2, json!([{"type":"text","text":big}])),
        ]),
        1_000,
    );
    let tail = f
        .read(json!({ "session_id": CLAUDE_ID, "max_chars": 1000 }))
        .unwrap();
    let text = tail["text"].as_str().unwrap();
    assert_eq!(tail["truncated"], true);
    assert_eq!(tail["entries"], 1);
    assert!(text.starts_with("### assistant\n[… "), "{text}");
    assert!(text.ends_with("END"), "the newest message keeps its end");
    assert!(text.chars().count() <= 1000, "{}", text.chars().count());

    let forward = f
        .read(json!({ "session_id": CLAUDE_ID, "after": "u1", "max_chars": 1000 }))
        .unwrap();
    let text = forward["text"].as_str().unwrap();
    assert!(
        text.starts_with("### assistant\nSTART"),
        "read forward, it keeps its start"
    );
    assert!(text.contains("characters of this message left out"));
    assert!(text.chars().count() <= 1000, "{}", text.chars().count());
    assert_eq!(forward["next_cursor"], "a1", "and the cursor moves past it");
    assert!(
        forward.get("note").is_none(),
        "nothing comes after it, so there is no next page to point to"
    );
}
