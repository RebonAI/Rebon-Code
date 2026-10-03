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

/// How Claude Code records a `/compact` typed while idle: the command's own
/// rows land after the boundary, and no reply follows.
fn claude_code_idle_compaction(boundary: Value) -> Vec<Value> {
    vec![
        json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{
            "input_tokens":1,"cache_read_input_tokens":310000,"cache_creation_input_tokens":1000,
            "cache_creation":{"ephemeral_1h_input_tokens":1000,"ephemeral_5m_input_tokens":0}}}}),
        json!({"type":"system","subtype":"turn_duration"}),
        json!({"type":"user","promptId":"p1","message":{"content":"/compact"}}),
        boundary,
        json!({"type":"user","promptId":"p1","isCompactSummary":true,
            "message":{"content":"This session is being continued from a previous conversation."}}),
        json!({"type":"user","promptId":"p1","isMeta":true,
            "message":{"content":"<local-command-caveat>The command below was run directly in Claude Code.</local-command-caveat>"}}),
        json!({"type":"user","promptId":"p1",
            "message":{"content":"<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args></command-args>"}}),
        json!({"type":"user","promptId":"p1",
            "message":{"content":"<local-command-stdout>Compacted (ctrl+o to see full summary)</local-command-stdout>"}}),
        json!({"type":"attachment","attachment":{"type":"hook_additional_context"}}),
    ]
}

#[test]
fn a_claude_code_session_compacted_while_idle_is_warm_on_its_compacted_context() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("claude/projects/C--work-app/aa38.jsonl"),
        &lines(&claude_code_idle_compaction(json!({
            "type":"system","subtype":"compact_boundary",
            "compactMetadata":{"trigger":"manual","preTokens":311000,"postTokens":9913}}))),
    );
    let activity = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(activity.busy, Some(false));
    assert_eq!(activity.context_tokens, Some(9913));
    // The cache the old reply wrote holds a context that is gone.
    assert_eq!(activity.cache_ttl_minutes, None);
    assert_eq!(state(&activity, &Warmth::default(), now()), State::Warm);
    assert!(!compact_due(
        &activity,
        &Warmth::default(),
        now() + 10 * 60_000
    ));
    // Short now: the hour no longer sends it cold.
    assert_eq!(
        state(&activity, &Warmth::default(), now() + 5 * 60 * 60_000),
        State::Warm
    );
}

#[test]
fn a_compaction_that_does_not_record_its_size_leaves_the_context_unknown() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("claude/projects/C--work-app/aa38.jsonl"),
        &lines(&claude_code_idle_compaction(json!({
            "type":"system","subtype":"compact_boundary",
            "compactMetadata":{"trigger":"auto","preTokens":228918}}))),
    );
    let activity = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(activity.busy, Some(false));
    assert_eq!(activity.context_tokens, None);
    assert_eq!(state(&activity, &Warmth::default(), now()), State::Warm);
    // Of no known size, it is not short: the hour still applies.
    assert_eq!(
        state(&activity, &Warmth::default(), now() + 2 * 60 * 60_000),
        State::Cold
    );
}

#[test]
fn a_prompt_after_a_compaction_opens_a_turn_and_its_reply_measures_the_context() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude/projects/C--work-app/aa38.jsonl");
    let mut rows = claude_code_idle_compaction(json!({
        "type":"system","subtype":"compact_boundary",
        "compactMetadata":{"trigger":"manual","postTokens":9913}}));
    rows.push(json!({"type":"user","promptId":"p2","message":{"content":"go on"}}));
    write(&path, &lines(&rows));
    let opened = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(opened.busy, Some(true));
    assert_eq!(opened.context_tokens, Some(9913));

    rows.push(
        json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{
        "input_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":12000,
        "cache_creation":{"ephemeral_1h_input_tokens":12000,"ephemeral_5m_input_tokens":0}}}}),
    );
    write(&path, &lines(&rows));
    let answered = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(answered.busy, Some(false));
    assert_eq!(answered.context_tokens, Some(12002));
    assert_eq!(answered.cache_ttl_minutes, Some(60));
}

#[test]
fn a_compaction_inside_a_turn_leaves_the_turn_open() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("claude/projects/C--work-app/aa38.jsonl"),
        &lines(&[
            json!({"type":"user","message":{"content":"go"}}),
            json!({"type":"assistant","message":{"stop_reason":"tool_use","usage":{"input_tokens":190000}}}),
            json!({"type":"system","subtype":"compact_boundary",
                "compactMetadata":{"trigger":"auto","postTokens":8000}}),
            json!({"type":"user","isCompactSummary":true,"message":{"content":"Summary"}}),
            json!({"type":"assistant","message":{"stop_reason":"tool_use","usage":{"input_tokens":9000}}}),
        ]),
    );
    let activity = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(activity.busy, Some(true));
    assert_eq!(activity.context_tokens, Some(9000));
}

#[test]
fn a_local_command_run_while_idle_leaves_the_turn_ended() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("claude/projects/C--work-app/aa38.jsonl"),
        &lines(&[
            json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{"input_tokens":30000}}}),
            json!({"type":"user","isMeta":true,
                "message":{"content":"<local-command-caveat>Caveat</local-command-caveat>"}}),
            json!({"type":"user","message":{"content":"<command-name>/model</command-name>"}}),
            json!({"type":"user","message":{"content":"<local-command-stdout>Set model</local-command-stdout>"}}),
            json!({"type":"user","message":{"content":"<local-command-stderr>warning</local-command-stderr>"}}),
        ]),
    );
    let activity = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(activity.busy, Some(false));
    assert_eq!(activity.context_tokens, Some(30000));
}

#[test]
fn a_prompt_command_opens_a_turn_until_it_is_answered() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("claude/projects/C--work-app/aa38.jsonl"),
        &lines(&[
            json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{"input_tokens":30000}}}),
            // An earlier local command's output must not hide the prompt
            // command that followed it.
            json!({"type":"user","message":{"content":"<local-command-stdout>Set model</local-command-stdout>"}}),
            json!({"type":"user","message":{"content":"<command-name>/review</command-name>"}}),
            json!({"type":"user","isMeta":true,"message":{"content":"Review the current diff."}}),
        ]),
    );
    let activity = activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(activity.busy, Some(true));
}

fn set_modified(path: &Path, ms: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + std::time::Duration::from_millis(ms))
        .unwrap();
}

#[test]
fn a_rebon_session_compacted_after_its_last_row_is_measured_by_its_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("rebon/projects/F--app/k7m2q.jsonl");
    let baseline = dir.path().join("rebon/projects/F--app/k7m2q.compact.json");
    write(
        &transcript,
        &lines(&[
            json!({"type":"user","message":{"content":"go"}}),
            json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{"input_tokens":150000}}}),
        ]),
    );
    write(&baseline, &"x".repeat(80_000));
    let at = now() - 60_000;
    set_modified(&transcript, at - 6 * 60_000);
    set_modified(&baseline, at);

    let compacted = activity(&homes(dir.path()), AgentKind::REBON, "k7m2q");
    assert_eq!(compacted.busy, Some(false));
    assert_eq!(compacted.context_tokens, Some(20_000));
    assert_eq!(compacted.last_activity_ms, Some(at));
    assert_eq!(state(&compacted, &Warmth::default(), now()), State::Warm);
    assert!(!compact_due(&compacted, &Warmth::default(), now()));
    // Short: still wakeable long after the group's hour.
    assert_eq!(
        state(&compacted, &Warmth::default(), now() + 5 * 60 * 60_000),
        State::Warm
    );

    // A turn after the compaction writes the transcript again, and its reply
    // measures the context.
    set_modified(&transcript, at + 1_000);
    let resumed = activity(&homes(dir.path()), AgentKind::REBON, "k7m2q");
    assert_eq!(resumed.context_tokens, Some(150000));
    assert_eq!(resumed.last_activity_ms, Some(at + 1_000));
}

#[test]
fn another_sessions_compaction_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("rebon/projects/F--app/k7m2q.jsonl");
    write(
        &transcript,
        &lines(&[
            json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{"input_tokens":150000}}}),
        ]),
    );
    let other = dir.path().join("rebon/projects/F--app/z9x8w.compact.json");
    write(&other, "{}");
    let at = now() - 60_000;
    set_modified(&transcript, at - 60_000);
    set_modified(&other, at);
    let activity = activity(&homes(dir.path()), AgentKind::REBON, "k7m2q");
    assert_eq!(activity.context_tokens, Some(150000));
    // A Claude Code transcript is not read for Rebon's baseline either.
    let claude = dir.path().join("claude/projects/C--work-app/aa38.jsonl");
    write(
        &claude,
        &lines(&[
            json!({"type":"assistant","message":{"stop_reason":"end_turn","usage":{"input_tokens":150000}}}),
        ]),
    );
    write(&claude.with_file_name("aa38.compact.json"), "{}");
    set_modified(&claude, at - 60_000);
    set_modified(&claude.with_file_name("aa38.compact.json"), at);
    let claude_activity = super::activity(&homes(dir.path()), AgentKind::CLAUDE_CODE, "aa38");
    assert_eq!(claude_activity.context_tokens, Some(150000));
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
        // Past short, short of long: the cache decides.
        context_tokens: Some(90_000),
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
fn a_finished_turn_on_a_short_context_stays_warm_past_the_cache_and_the_hour() {
    let short = Activity {
        last_activity_ms: Some(now() - 5 * 60 * 60_000),
        busy: Some(false),
        context_tokens: Some(55_000),
        context_window: None,
        cache_ttl_minutes: Some(5),
    };
    assert_eq!(state(&short, &Warmth::default(), now()), State::Warm);
    // Two thirds of the default 120k threshold is the edge.
    for (tokens, expected) in [
        (80_000, State::Warm),
        (80_001, State::Cold),
        (119_000, State::Cold),
    ] {
        let activity = Activity {
            context_tokens: Some(tokens),
            ..short.clone()
        };
        assert_eq!(
            state(&activity, &Warmth::default(), now()),
            expected,
            "{tokens}"
        );
    }
    // Within its cache a context short of long is warm either way.
    let cached = Activity {
        last_activity_ms: Some(now() - 60_000),
        context_tokens: Some(119_000),
        ..short
    };
    assert_eq!(state(&cached, &Warmth::default(), now()), State::Warm);
}

#[test]
fn short_follows_the_groups_threshold_and_the_models_window() {
    let activity = Activity {
        last_activity_ms: Some(now() - 5 * 60 * 60_000),
        busy: Some(false),
        context_tokens: Some(300_000),
        context_window: Some(1_000_000),
        cache_ttl_minutes: None,
    };
    // 60% of a million is long, and two thirds of that short.
    assert_eq!(state(&activity, &Warmth::default(), now()), State::Warm);
    let tight = Warmth {
        context_ratio: 0.3,
        ..Warmth::default()
    };
    assert_eq!(state(&activity, &tight, now()), State::Cold);
}

#[test]
fn only_a_finished_turn_of_known_size_is_short() {
    let hours_ago = Activity {
        last_activity_ms: Some(now() - 5 * 60 * 60_000),
        busy: Some(false),
        context_tokens: None,
        context_window: None,
        cache_ttl_minutes: None,
    };
    assert_eq!(state(&hours_ago, &Warmth::default(), now()), State::Cold);
    // A turn left open, or a member that does not say, may still be working
    // or dead with its process.
    for busy in [Some(true), None] {
        let activity = Activity {
            busy,
            context_tokens: Some(10_000),
            ..hours_ago.clone()
        };
        assert_eq!(
            state(&activity, &Warmth::default(), now()),
            State::Cold,
            "{busy:?}"
        );
    }
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
