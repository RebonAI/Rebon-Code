use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;

/// What Claude Code 2.1.293 wrote to its PermissionRequest hook, asking to
/// write a file.
fn claude_input() -> Value {
    json!({
        "session_id": "c4a80c0b-61a9-49a8-9adc-26a563d44cde",
        "transcript_path": "C:\\Users\\me\\.claude\\projects\\p\\c4a80c0b.jsonl",
        "cwd": "C:\\work\\probe-dir",
        "permission_mode": "default",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Write",
        "tool_input": {"file_path": "C:\\work\\probe-dir\\hooked.txt", "content": "hi"},
        "permission_suggestions": [
            {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
        ]
    })
}

/// What Codex 0.159.2 wrote to its PermissionRequest hook, asking to run a
/// command.
fn codex_input() -> Value {
    json!({
        "session_id": "01a11977-6803-7f40-9d55-a7cabd1da75f",
        "turn_id": "01a11977-6966-7470-9736-75ccec957c61",
        "transcript_path": "C:\\Users\\me\\.codex\\sessions\\rollout.jsonl",
        "cwd": "C:\\work\\probe-codex",
        "hook_event_name": "PermissionRequest",
        "model": "gpt-6-astra",
        "permission_mode": "default",
        "tool_name": "Bash",
        "tool_input": {
            "command": "Set-Content -LiteralPath 'hooked5.txt' -Value 'hi'",
            "description": "当前环境为只读沙箱。是否允许创建 hooked5.txt？"
        }
    })
}

fn decision(output: &Value) -> &Value {
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "PermissionRequest"
    );
    &output["hookSpecificOutput"]["decision"]
}

#[test]
fn agents_are_named_as_the_hook_command_names_them() {
    for agent in [Agent::ClaudeCode, Agent::Codex] {
        assert_eq!(Agent::from_name(agent.name()), Some(agent));
    }
    assert_eq!(Agent::from_name("rebon"), None);
}

#[test]
fn a_request_carries_the_tool_and_whether_it_can_be_remembered() {
    let claude = request_for(Agent::ClaudeCode, "conv".into(), &claude_input()).expect("request");
    assert_eq!(claude.tool_name, "Write");
    assert_eq!(claude.tool_input["content"], "hi");
    assert_eq!(claude.owner, "conv");
    assert!(claude.can_remember, "Claude Code proposed a rule to keep");

    let codex = request_for(Agent::Codex, "conv".into(), &codex_input()).expect("request");
    assert_eq!(codex.tool_name, "Bash");
    assert!(!codex.can_remember, "Codex takes no rules back");

    let mut no_rules = claude_input();
    no_rules["permission_suggestions"] = json!([]);
    let claude = request_for(Agent::ClaudeCode, "conv".into(), &no_rules).expect("request");
    assert!(!claude.can_remember);

    assert_eq!(
        request_for(Agent::ClaudeCode, "conv".into(), &json!({"cwd": "/"})),
        None,
        "no tool, no request"
    );
}

#[test]
fn each_answer_is_printed_in_the_shape_the_cli_reads() {
    let allow = hook_output(Agent::ClaudeCode, &claude_input(), Answer::Allow).expect("output");
    assert_eq!(decision(&allow), &json!({"behavior": "allow"}));

    let deny = hook_output(Agent::Codex, &codex_input(), Answer::Deny).expect("output");
    assert_eq!(decision(&deny)["behavior"], "deny");
    assert_eq!(decision(&deny)["message"], DENIED);

    assert_eq!(
        hook_output(Agent::ClaudeCode, &claude_input(), Answer::Defer),
        None,
        "deferring prints nothing, so the CLI asks itself"
    );
}

#[test]
fn always_allow_hands_claude_code_back_its_own_rules() {
    let output =
        hook_output(Agent::ClaudeCode, &claude_input(), Answer::AllowAlways).expect("output");
    assert_eq!(
        decision(&output),
        &json!({
            "behavior": "allow",
            "updatedPermissions": [
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
            ]
        })
    );
}

#[test]
fn always_allow_is_a_plain_allow_where_rules_cannot_go() {
    // Codex fails closed on `updatedPermissions`: the whole answer would be
    // thrown away.
    let codex = hook_output(Agent::Codex, &codex_input(), Answer::AllowAlways).expect("output");
    assert_eq!(decision(&codex), &json!({"behavior": "allow"}));
    let mut no_rules = claude_input();
    no_rules
        .as_object_mut()
        .expect("object")
        .remove("permission_suggestions");
    let claude = hook_output(Agent::ClaudeCode, &no_rules, Answer::AllowAlways).expect("output");
    assert_eq!(decision(&claude), &json!({"behavior": "allow"}));
}

#[test]
fn answers_and_requests_round_trip_through_their_files() {
    for value in [
        Answer::Allow,
        Answer::AllowAlways,
        Answer::Deny,
        Answer::Defer,
    ] {
        let text = serde_json::to_string(&value).expect("serialize");
        assert_eq!(serde_json::from_str::<Answer>(&text).expect("parse"), value);
    }
    assert_eq!(
        serde_json::to_string(&Answer::AllowAlways).unwrap(),
        "\"allow-always\""
    );
}

fn env_for(inbox: &Path, host: u32) -> impl Fn(&str) -> Option<String> {
    let inbox = inbox.to_string_lossy().into_owned();
    move |name| match name {
        INBOX_ENV => Some(inbox.clone()),
        OWNER_ENV => Some("conv-1".into()),
        HOST_PID_ENV => Some(host.to_string()),
        _ => None,
    }
}

/// The app's half, played by a thread: wait for the request, check it, and
/// answer it.
fn answer_when_asked(inbox: PathBuf, reply: Answer) -> std::thread::JoinHandle<Request> {
    std::thread::spawn(move || loop {
        if let Some(request) = pending(&inbox).into_iter().next() {
            answer(&inbox, &request.id, reply).expect("answer");
            return request;
        }
        std::thread::sleep(Duration::from_millis(10));
    })
}

#[test]
fn the_hook_asks_the_app_and_prints_its_answer() {
    let inbox = tempfile::tempdir().expect("inbox");
    let app = answer_when_asked(inbox.path().to_path_buf(), Answer::Deny);
    let output = run_hook(
        "claude-code",
        &claude_input().to_string(),
        env_for(inbox.path(), 42),
        |_| true,
    )
    .expect("an answer");
    assert_eq!(decision(&output)["behavior"], "deny");

    let asked = app.join().expect("app thread");
    assert_eq!(asked.owner, "conv-1");
    assert_eq!(asked.agent, Agent::ClaudeCode);
    assert_eq!(asked.tool_name, "Write");
    assert_eq!(
        std::fs::read_dir(inbox.path()).expect("inbox").count(),
        0,
        "both files are cleaned up"
    );
}

#[test]
fn a_deferred_request_prints_nothing() {
    let inbox = tempfile::tempdir().expect("inbox");
    let app = answer_when_asked(inbox.path().to_path_buf(), Answer::Defer);
    let output = run_hook(
        "codex",
        &codex_input().to_string(),
        env_for(inbox.path(), 42),
        |_| true,
    );
    app.join().expect("app thread");
    assert_eq!(output, None);
}

#[test]
fn without_the_apps_environment_the_hook_says_nothing() {
    let inbox = tempfile::tempdir().expect("inbox");
    let input = claude_input().to_string();
    let full = env_for(inbox.path(), 42);
    for missing in [INBOX_ENV, OWNER_ENV, HOST_PID_ENV] {
        let env = |name: &str| (name != missing).then(|| full(name)).flatten();
        assert_eq!(
            run_hook("claude-code", &input, env, |_| true),
            None,
            "{missing}"
        );
    }
    assert_eq!(
        run_hook("claude-code", "not json", env_for(inbox.path(), 42), |_| {
            true
        }),
        None
    );
    assert_eq!(
        run_hook("rebon", &input, env_for(inbox.path(), 42), |_| true),
        None,
        "an agent this does not relay"
    );
    assert_eq!(std::fs::read_dir(inbox.path()).expect("inbox").count(), 0);
}

#[test]
fn a_hook_whose_app_has_gone_stops_waiting() {
    let inbox = tempfile::tempdir().expect("inbox");
    let request = request_for(Agent::ClaudeCode, "conv".into(), &claude_input()).expect("request");
    let alive = Arc::new(AtomicBool::new(true));
    let gone = {
        let alive = alive.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            alive.store(false, Ordering::SeqCst);
        })
    };
    let started = Instant::now();
    let answer = ask(
        inbox.path(),
        &request,
        &Wait {
            timeout: Duration::from_secs(30),
            poll: Duration::from_millis(10),
            host_alive: &|| alive.load(Ordering::SeqCst),
        },
    );
    gone.join().expect("thread");
    assert_eq!(answer, None);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        pending(inbox.path()).is_empty(),
        "the request is taken back"
    );
}

#[test]
fn a_hook_gives_up_at_its_timeout() {
    let inbox = tempfile::tempdir().expect("inbox");
    let request = request_for(Agent::Codex, "conv".into(), &codex_input()).expect("request");
    let answer = ask(
        inbox.path(),
        &request,
        &Wait {
            timeout: Duration::from_millis(50),
            poll: Duration::from_millis(10),
            host_alive: &|| true,
        },
    );
    assert_eq!(answer, None);
    assert_eq!(std::fs::read_dir(inbox.path()).expect("inbox").count(), 0);
}

#[test]
fn an_answer_left_by_a_dead_hook_is_not_taken_for_this_one() {
    let inbox = tempfile::tempdir().expect("inbox");
    let request = request_for(Agent::ClaudeCode, "conv".into(), &claude_input()).expect("request");
    answer(inbox.path(), &request.id, Answer::Allow).expect("stale answer");
    let answer = ask(
        inbox.path(),
        &request,
        &Wait {
            timeout: Duration::from_millis(50),
            poll: Duration::from_millis(10),
            host_alive: &|| true,
        },
    );
    assert_eq!(answer, None, "the stale allow was cleared before asking");
}

#[test]
fn pending_lists_unanswered_requests_only() {
    let inbox = tempfile::tempdir().expect("inbox");
    let write = |id: &str| {
        let request = Request {
            id: id.into(),
            owner: "conv".into(),
            agent: Agent::Codex,
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            can_remember: false,
        };
        std::fs::write(
            request_path(inbox.path(), id),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
    };
    write("hook-2");
    write("hook-1");
    write("hook-3");
    answer(inbox.path(), "hook-3", Answer::Allow).expect("answer");
    std::fs::write(request_path(inbox.path(), "hook-4"), b"{ half").unwrap();
    std::fs::write(inbox.path().join("notes.txt"), b"x").unwrap();
    let ids: Vec<String> = pending(inbox.path())
        .into_iter()
        .map(|request| request.id)
        .collect();
    assert_eq!(ids, ["hook-1", "hook-2"]);
    assert!(pending(&inbox.path().join("missing")).is_empty());
}
