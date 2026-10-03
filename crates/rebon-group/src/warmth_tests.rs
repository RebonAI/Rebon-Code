use super::*;
use serde_json::json;

fn lines(values: &[Value]) -> String {
    values
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn homes(root: &Path) -> Homes {
    Homes {
        rebon_projects: Some(root.join("rebon/projects")),
        claude: Some(root.join("claude")),
        codex: Some(root.join("codex")),
        grok: Some(root.join("grok")),
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn now() -> u64 {
    crate::store::now_ms()
}

#[test]
fn a_claude_code_turn_that_ended_is_idle_and_names_its_cache() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("claude/projects/C--work-app/aa38.jsonl"),
        &lines(&[
            json!({"type":"user","message":{"content":"go"}}),
            json!({"type":"assistant","message":{"stop_reason":"tool_use","usage":{"input_tokens":5}}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result"}]}}),
            json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{
                "input_tokens":10,"cache_read_input_tokens":40000,"cache_creation_input_tokens":2000,
                "cache_creation":{"ephemeral_1h_input_tokens":2000,"ephemeral_5m_input_tokens":0}}}}),
            json!({"type":"system","subtype":"turn_duration"}),
        ]),
    );
    let activity = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(activity.busy, Some(false));
    assert_eq!(activity.context_tokens, Some(42010));
    assert_eq!(activity.cache_ttl_minutes, Some(60));
    assert!(activity.last_activity_ms.is_some());
    assert_eq!(state(&activity, &Warmth::default(), now()), State::Warm);
}

#[test]
fn a_turn_waiting_on_a_tool_is_busy_until_it_goes_stale() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("rebon/projects/F--app/k7m2q.jsonl"),
        &lines(&[
            json!({"type":"user","message":{"content":"go"}}),
            json!({"type":"assistant","message":{"stop_reason":"tool_use","usage":{"input_tokens":100}}}),
        ]),
    );
    let mut activity = activity(&homes(dir.path()), AgentKind::REBON, "k7m2q");
    assert_eq!(activity.busy, Some(true));
    assert_eq!(activity.cache_ttl_minutes, None);
    assert_eq!(state(&activity, &Warmth::default(), now()), State::Busy);
    // Left open for half an hour: the process died with it, and the cache.
    activity.last_activity_ms = Some(now() - 30 * 60_000);
    let warmth = Warmth {
        idle_minutes: 20,
        ..Warmth::default()
    };
    assert_eq!(state(&activity, &warmth, now()), State::Cold);
}

#[test]
fn the_shorter_of_the_cache_and_the_groups_limit_decides() {
    let base = Activity {
        last_activity_ms: Some(now() - 6 * 60_000),
        busy: Some(false),
        context_tokens: Some(1000),
        context_window: None,
        cache_ttl_minutes: Some(5),
    };
    assert_eq!(state(&base, &Warmth::default(), now()), State::Cold);
    let hour_cache = Activity {
        cache_ttl_minutes: Some(60),
        ..base.clone()
    };
    assert_eq!(state(&hour_cache, &Warmth::default(), now()), State::Warm);
    // No TTL of its own: the group's hour.
    let unknown_ttl = Activity {
        cache_ttl_minutes: None,
        ..base
    };
    assert_eq!(state(&unknown_ttl, &Warmth::default(), now()), State::Warm);
}

#[test]
fn a_long_context_is_cold_however_recent() {
    let long = Activity {
        last_activity_ms: Some(now()),
        busy: Some(false),
        context_tokens: Some(130_000),
        context_window: None,
        cache_ttl_minutes: None,
    };
    assert_eq!(state(&long, &Warmth::default(), now()), State::Cold);
    let big_window = Activity {
        context_window: Some(1_000_000),
        ..long
    };
    assert_eq!(state(&big_window, &Warmth::default(), now()), State::Warm);
}

#[test]
fn codex_is_read_from_its_rollout() {
    let dir = tempfile::tempdir().unwrap();
    let rollout = dir
        .path()
        .join("codex/sessions/2026/09/30/rollout-2026-09-30T10-00-00-019a-77.jsonl");
    write(
        &rollout,
        &lines(&[
            json!({"timestamp":"t","type":"event_msg","payload":{"type":"task_started"}}),
            json!({"timestamp":"t","type":"event_msg","payload":{"type":"token_count","info":{
                "last_token_usage":{"input_tokens":51000},"model_context_window":272000}}}),
            json!({"timestamp":"t","type":"event_msg","payload":{"type":"task_complete"}}),
        ]),
    );
    // An older day holds nothing of it.
    write(
        &dir.path()
            .join("codex/sessions/2026/09/29/rollout-x-other.jsonl"),
        "",
    );
    let activity = activity(&homes(dir.path()), AgentKind::CODEX, "019a-77");
    assert_eq!(activity.busy, Some(false));
    assert_eq!(activity.context_tokens, Some(51000));
    assert_eq!(activity.context_window, Some(272000));
    assert_eq!(state(&activity, &Warmth::default(), now()), State::Warm);
}

#[test]
fn grok_is_read_from_its_signals() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("grok/sessions/enc-cwd/g-1");
    write(
        &session.join("signals.json"),
        r#"{"context_tokens_used": 90000, "context_window_tokens": 128000}"#,
    );
    let activity = activity(&homes(dir.path()), AgentKind::GROK, "g-1");
    assert_eq!(activity.busy, None);
    assert_eq!(activity.context_window, Some(128000));
    // 70% of its window: cold.
    assert_eq!(state(&activity, &Warmth::default(), now()), State::Cold);
}

#[test]
fn an_agent_whose_files_are_unknown_or_missing_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    for (agent, id) in [
        ("opencode", "x"),
        (AgentKind::CLAUDE_CODE, "missing"),
        (AgentKind::CLAUDE_CODE, "../escape"),
    ] {
        let activity = activity(&homes(dir.path()), agent, id);
        assert_eq!(activity, Activity::default(), "{agent} {id}");
        assert_eq!(state(&activity, &Warmth::default(), now()), State::Unknown);
    }
}

#[test]
fn session_path_finds_each_agents_own_file() {
    let dir = tempfile::tempdir().unwrap();
    let rebon = dir.path().join("rebon/projects/F--app/k7m2q.jsonl");
    let claude = dir.path().join("claude/projects/C--work-app/aa38.jsonl");
    let codex = dir
        .path()
        .join("codex/sessions/2026/09/30/rollout-2026-09-30T10-00-00-019a-77.jsonl");
    let grok = dir.path().join("grok/sessions/enc-cwd/g-1");
    for file in [&rebon, &claude, &codex] {
        write(file, "");
    }
    write(&grok.join("signals.json"), "{}");

    let homes = homes(dir.path());
    assert_eq!(session_path(&homes, AgentKind::REBON, "k7m2q"), Some(rebon));
    assert_eq!(
        session_path(&homes, AgentKind::CLAUDE_CODE, "aa38"),
        Some(claude)
    );
    assert_eq!(
        session_path(&homes, AgentKind::CODEX, "019a-77"),
        Some(codex)
    );
    assert_eq!(session_path(&homes, AgentKind::GROK, "g-1"), Some(grok));
}

#[test]
fn session_path_finds_nothing_unknown_missing_or_escaping() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("claude/projects/C--work-app/aa38.jsonl"),
        "",
    );
    let homes = homes(dir.path());
    for (agent, id) in [
        ("opencode", "aa38"),
        (AgentKind::CLAUDE_CODE, "missing"),
        (AgentKind::CLAUDE_CODE, "../escape"),
        // Claude Code's transcript is not Codex's, nor Rebon's.
        (AgentKind::CODEX, "aa38"),
        (AgentKind::REBON, "aa38"),
    ] {
        assert_eq!(session_path(&homes, agent, id), None, "{agent} {id}");
    }
    assert_eq!(
        session_path(&Homes::default(), AgentKind::CLAUDE_CODE, "aa38"),
        None
    );
}

#[test]
fn idle_compaction_waits_for_five_minutes_and_a_long_context() {
    let activity = Activity {
        last_activity_ms: Some(1_000),
        busy: Some(false),
        context_tokens: Some(130_000),
        ..Activity::default()
    };
    assert!(!compact_due(&activity, &Warmth::default(), 300_999));
    assert!(compact_due(&activity, &Warmth::default(), 301_000));
    assert!(!compact_due(&activity, &Warmth::default(), 999));
}

#[test]
fn idle_compaction_never_treats_stale_work_or_unknown_state_as_idle() {
    for busy in [Some(true), None] {
        let activity = Activity {
            last_activity_ms: Some(1_000),
            busy,
            context_tokens: Some(190_000),
            ..Activity::default()
        };
        assert!(!compact_due(&activity, &Warmth::default(), 3_601_000));
    }
    assert!(!compact_due(
        &Activity::default(),
        &Warmth::default(),
        3_601_000
    ));
}

#[test]
fn idle_compaction_uses_the_groups_context_threshold_and_model_window() {
    let activity = Activity {
        last_activity_ms: Some(1_000),
        busy: Some(false),
        context_tokens: Some(130_000),
        context_window: Some(272_000),
        ..Activity::default()
    };
    assert!(!compact_due(&activity, &Warmth::default(), 301_000));
    let warmth = Warmth {
        context_ratio: 0.4,
        ..Warmth::default()
    };
    assert!(compact_due(&activity, &warmth, 301_000));
}

#[test]
fn idle_compaction_leaves_short_missing_or_invalid_context_alone() {
    let mut activity = Activity {
        last_activity_ms: Some(1_000),
        busy: Some(false),
        ..Activity::default()
    };
    for tokens in [None, Some(0), Some(10_000), Some(120_000)] {
        activity.context_tokens = tokens;
        assert!(!compact_due(&activity, &Warmth::default(), 301_000));
    }
    activity.context_tokens = Some(190_000);
    activity.context_window = Some(0);
    assert!(!compact_due(&activity, &Warmth::default(), 301_000));
}

#[test]
fn knows_exactly_the_agents_session_path_looks_for() {
    let dir = tempfile::tempdir().unwrap();
    for (agent, file) in [
        (AgentKind::REBON, "rebon/projects/p/s.jsonl"),
        (AgentKind::CLAUDE_CODE, "claude/projects/p/s.jsonl"),
        (
            AgentKind::CODEX,
            "codex/sessions/2026/10/02/rollout-t-s.jsonl",
        ),
        (AgentKind::GROK, "grok/sessions/p/s/signals.json"),
    ] {
        write(&dir.path().join(file), "");
        assert!(knows(agent), "{agent}");
        assert!(
            session_path(&homes(dir.path()), agent, "s").is_some(),
            "{agent}"
        );
    }
    for agent in [AgentKind::OPENCODE, AgentKind::DSH, "some-new-cli"] {
        assert!(!knows(agent), "{agent}");
    }
}
