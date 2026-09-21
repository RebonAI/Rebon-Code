//! The desk against a real store in a temp directory. No worker runs: a job
//! is put into each state by writing its record the way the host would, and
//! answers that need a live worker are shown to reach the host (its "no live
//! worker endpoint" refusal) only after every check of this surface passed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rebon_session_host::{
    BackgroundIpcEndpoint, BackgroundJobState, BackgroundJobStatus,
    BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot, BackgroundRoster,
    BackgroundStore,
};
use serde_json::json;

use super::*;
use crate::push::{Pending, Update};

struct Fixture {
    _dir: tempfile::TempDir,
    store: BackgroundStore,
    projects_root: PathBuf,
    root: PathBuf,
    desk: Desk,
    gate_calls: Arc<AtomicUsize>,
}

fn fixture() -> Fixture {
    fixture_with(LedgerOwner::this_process(), Ok(()))
}

fn fixture_with(owner: LedgerOwner, gate: Result<(), &'static str>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = BackgroundStore::new(dir.path().join("home"));
    let projects_root = dir.path().join("home").join("projects");
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let root = canonical_dir(&root).unwrap();
    seed_live_supervisor(&store);
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&gate_calls);
    let desk = Desk::new(DeskConfig {
        store: store.clone(),
        projects_root: projects_root.clone(),
        root: root.clone(),
        // Deliberately unspawnable; the seeded roster keeps the launch path
        // from ever trying.
        rebon_exe: PathBuf::from("./__rebon-mcp-test-must-not-spawn__"),
        launch_gate: Arc::new(move |_runtime: &BackgroundRuntimeFields| {
            calls.fetch_add(1, Ordering::SeqCst);
            gate.map_err(|reason| anyhow::anyhow!(reason))
        }),
        owner,
        channel: true,
    });
    Fixture {
        _dir: dir,
        store,
        projects_root,
        root,
        desk,
        gate_calls,
    }
}

/// A roster naming this (live) process, so `ensure_supervisor_running`
/// returns without spawning anything.
fn seed_live_supervisor(store: &BackgroundStore) {
    store
        .write_roster(&BackgroundRoster {
            supervisor_pid: std::process::id(),
            supervisor_pid_identity: rebon_session_host::process_identity(std::process::id()),
            updated_at_ms: rebon_types::wall_clock_ms(),
            jobs: Vec::new(),
        })
        .unwrap();
}

fn start(fixture: &Fixture, prompt: &str) -> String {
    let started = fixture
        .desk
        .start(
            StartRequest {
                prompt: prompt.into(),
                agent: None,
                provider: None,
                model: None,
                cwd: None,
                name: None,
                permission_mode: None,
            },
            1_000,
        )
        .unwrap();
    started["job_id"].as_str().unwrap().to_string()
}

fn set(fixture: &Fixture, job_id: &str, change: impl FnOnce(&mut BackgroundJobState)) {
    fixture
        .store
        .update_state(job_id, |state| {
            change(state);
            Ok(())
        })
        .unwrap();
}

/// What a worker leaves behind when a turn completes.
fn finish(fixture: &Fixture, job_id: &str, status: BackgroundJobStatus, answer: Option<&str>) {
    let session_id = format!("sess-{job_id}");
    set(fixture, job_id, |state| {
        state.identity.session_id = Some(session_id.clone());
        state.process.status = status;
        state.process.turn_generation = 1;
        state.process.started_at_ms = Some(10_000);
        state.process.completed_at_ms = Some(12_500);
        state.outcome.exit_code = Some(if status == BackgroundJobStatus::Succeeded {
            0
        } else {
            1
        });
        if status == BackgroundJobStatus::Failed {
            state.outcome.error = Some("provider returned 529".into());
        }
    });
    if let Some(answer) = answer {
        write_transcript(fixture, &fixture.root, &session_id, answer);
    }
}

fn write_transcript(fixture: &Fixture, cwd: &Path, session_id: &str, answer: &str) {
    let path = rebon_session::ensure_session_file_path(
        &fixture.projects_root,
        &cwd.to_string_lossy(),
        session_id,
    )
    .unwrap();
    let lines = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"do the task"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[{"type":"text","text":answer}]}}),
    ];
    let body = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, body).unwrap();
}

fn option(option_id: &str, kind: &str) -> BackgroundPermissionOptionSnapshot {
    BackgroundPermissionOptionSnapshot {
        option_id: option_id.into(),
        label: option_id.replace('_', " "),
        kind: kind.into(),
    }
}

fn permission(query_id: u64) -> BackgroundPermissionQuerySnapshot {
    BackgroundPermissionQuerySnapshot {
        query_id,
        turn_generation: 1,
        endpoint: None,
        tool: Some("Bash".into()),
        tool_call_id: Some("toolu_1".into()),
        session_id: Some("sess".into()),
        title: Some("Run a command".into()),
        message: Some("Allow `cargo test`?".into()),
        tool_input: Some(json!({ "command": "cargo test" })),
        metadata: None,
        options: vec![
            option("allow_once", "AllowOnce"),
            option("allow_always", "AllowAlways"),
            option("reject_once", "reject_once"),
            option("mystery", "GrantForever"),
        ],
    }
}

fn question(query_id: u64, questions: usize) -> BackgroundPermissionQuerySnapshot {
    let questions: Vec<_> = (0..questions)
        .map(|n| {
            json!({
                "header": format!("Q{n}"),
                "question": format!("Which way, {n}?"),
                "multiSelect": false,
                "options": [
                    { "label": "Left", "description": "go left" },
                    { "label": "Right", "description": "go right" },
                ],
            })
        })
        .collect();
    BackgroundPermissionQuerySnapshot {
        query_id,
        turn_generation: 1,
        endpoint: Some(BackgroundIpcEndpoint {
            pid: std::process::id(),
            port: 1,
            token: "t".into(),
        }),
        tool: Some("AskUserQuestion".into()),
        tool_call_id: Some("toolu_q".into()),
        session_id: Some("sess".into()),
        title: None,
        message: None,
        tool_input: Some(json!({ "questions": questions })),
        metadata: None,
        options: vec![
            option("allow_once", "AllowOnce"),
            option("reject_once", "RejectOnce"),
        ],
    }
}

fn park(fixture: &Fixture, job_id: &str, pending: BackgroundPermissionQuerySnapshot) {
    set(fixture, job_id, |state| {
        state.identity.session_id = Some("sess".into());
        state.process.status = BackgroundJobStatus::NeedsInput;
        state.process.turn_generation = 1;
        state.outcome.pending_permission = Some(pending);
    });
}

fn err(result: anyhow::Result<serde_json::Value>) -> String {
    format!("{:#}", result.expect_err("the desk should refuse this"))
}

// ── exec_start ───────────────────────────────────────────────────────

#[test]
fn start_launches_a_background_job_and_records_it_for_this_server() {
    let f = fixture();
    let started = f
        .desk
        .start(
            StartRequest {
                prompt: "  write the report  ".into(),
                agent: Some(" reviewer ".into()),
                provider: Some("deepseek".into()),
                model: Some("deepseek-v4".into()),
                cwd: None,
                name: Some("report".into()),
                permission_mode: Some("acceptEdits".into()),
            },
            1_000,
        )
        .unwrap();
    let job_id = started["job_id"].as_str().unwrap();
    assert_eq!(started["state"], "queued");
    assert_eq!(started["channel"], "declared");
    assert_eq!(
        f.gate_calls.load(Ordering::SeqCst),
        1,
        "the gate saw the launch"
    );

    let state = f.store.read_state(job_id).unwrap();
    assert_eq!(state.prompt(), "write the report");
    assert_eq!(state.name(), "report");
    assert_eq!(state.agent_type(), Some("reviewer"));
    assert_eq!(state.runtime().provider.as_deref(), Some("deepseek"));
    assert_eq!(state.runtime().model.as_deref(), Some("deepseek-v4"));
    assert_eq!(
        state.runtime().permission_mode.as_deref(),
        Some("acceptEdits")
    );
    assert!(state.isolate_in_worktree(), "the same options as `--bg`");
    assert_eq!(Path::new(state.cwd()), f.root);

    let ledger = ledger::read(&f.store, job_id).unwrap().expect("a ledger");
    assert_eq!(ledger.owner, LedgerOwner::this_process());
    assert_eq!(Path::new(&ledger.root), f.root);
    assert_eq!(f.desk.tracked(), vec![job_id.to_string()]);
}

#[test]
fn start_runs_in_a_directory_inside_the_root_and_nowhere_else() {
    let f = fixture();
    let inside = f
        .desk
        .start(
            StartRequest {
                prompt: "x".into(),
                agent: None,
                provider: None,
                model: None,
                cwd: Some("sub".into()),
                name: None,
                permission_mode: None,
            },
            0,
        )
        .unwrap();
    assert_eq!(
        Path::new(inside["cwd"].as_str().unwrap()),
        f.root.join("sub"),
        "a relative cwd is taken under the root"
    );

    let elsewhere = tempfile::tempdir().unwrap();
    for cwd in [
        elsewhere.path().to_string_lossy().to_string(),
        "..".to_string(),
        "sub/../..".to_string(),
    ] {
        let before = std::fs::read_dir(f.store.jobs_dir()).unwrap().count();
        let refused = err(f.desk.start(
            StartRequest {
                prompt: "x".into(),
                agent: None,
                provider: None,
                model: None,
                cwd: Some(cwd.clone()),
                name: None,
                permission_mode: None,
            },
            0,
        ));
        assert!(refused.contains("outside"), "{cwd}: {refused}");
        assert_eq!(
            std::fs::read_dir(f.store.jobs_dir()).unwrap().count(),
            before,
            "{cwd}: a refused start creates no job"
        );
    }
    let missing = err(f.desk.start(
        StartRequest {
            prompt: "x".into(),
            agent: None,
            provider: None,
            model: None,
            cwd: Some("no-such-dir".into()),
            name: None,
            permission_mode: None,
        },
        0,
    ));
    assert!(missing.contains("not a directory"), "{missing}");
}

#[test]
fn a_closed_gate_or_an_empty_prompt_starts_nothing() {
    let f = fixture_with(LedgerOwner::this_process(), Err("agent view is disabled"));
    let refused = err(f.desk.start(
        StartRequest {
            prompt: "x".into(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            name: None,
            permission_mode: Some("bypassPermissions".into()),
        },
        0,
    ));
    assert!(refused.contains("agent view is disabled"), "{refused}");
    assert!(
        !f.store.jobs_dir().exists() || std::fs::read_dir(f.store.jobs_dir()).unwrap().count() == 0
    );

    let f = fixture();
    let empty = err(f.desk.start(
        StartRequest {
            prompt: "   ".into(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            name: None,
            permission_mode: None,
        },
        0,
    ));
    assert!(empty.contains("empty"), "{empty}");
    assert_eq!(f.gate_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn requests_refuse_fields_they_do_not_have() {
    // `job_result` takes an id, never a path.
    assert!(serde_json::from_value::<ResultRequest>(json!({
        "job_id": "bg-1", "path": "/etc/passwd"
    }))
    .is_err());
    assert!(serde_json::from_value::<StartRequest>(json!({
        "prompt": "x", "isolate": false
    }))
    .is_err());
    assert!(
        serde_json::from_value::<PermitRequest>(json!({
            "job_id": "bg-1", "option_id": "allow_once"
        }))
        .is_err(),
        "a permit names the query it answers"
    );
}

// ── visibility ───────────────────────────────────────────────────────

#[test]
fn only_jobs_started_through_this_surface_are_visible() {
    let f = fixture();
    let foreign = f
        .store
        .create_job(
            "someone else's".into(),
            f.root.clone(),
            runtime_fields(None, None, None),
        )
        .unwrap();
    let refused = err(f.desk.status(foreign.job_id()));
    assert!(
        refused.contains("not started through `rebon mcp serve`"),
        "{refused}"
    );
    assert!(err(f.desk.status("bg-nope")).contains("no job bg-nope"));
    for bad in ["../../etc", "bg/1", ""] {
        assert!(err(f.desk.status(bad)).contains("not a job id"), "{bad:?}");
    }
}

// ── job_status ───────────────────────────────────────────────────────

#[test]
fn status_reports_state_timing_and_the_heartbeat() {
    let f = fixture();
    let job_id = start(&f, "task");
    let queued = f.desk.status(&job_id).unwrap();
    assert_eq!(queued["state"], "queued");
    assert_eq!(queued["pending"], serde_json::Value::Null);
    assert!(queued["last_heartbeat_ms"].as_u64().unwrap() > 0);

    finish(&f, &job_id, BackgroundJobStatus::Succeeded, None);
    let done = f.desk.status(&job_id).unwrap();
    assert_eq!(done["state"], "succeeded");
    assert_eq!(done["turn"], 1);
    assert_eq!(done["duration_ms"], 2_500);
    assert_eq!(done["exit_code"], 0);
}

#[test]
fn a_pending_permission_shows_only_the_answers_this_surface_may_give() {
    let f = fixture();
    let job_id = start(&f, "task");
    park(&f, &job_id, permission(7));
    let status = f.desk.status(&job_id).unwrap();
    let pending = &status["pending"];
    assert_eq!(pending["kind"], "permission");
    assert_eq!(pending["query_id"], 7);
    assert_eq!(pending["tool"], "Bash");
    assert_eq!(pending["answer_with"], "job_permit");
    let offered: Vec<&str> = pending["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["option_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        offered,
        vec!["allow_once", "reject_once"],
        "no lasting grant, and no kind this build cannot read"
    );
}

#[test]
fn a_pending_question_is_numbered_for_job_reply() {
    let f = fixture();
    let job_id = start(&f, "task");
    park(&f, &job_id, question(3, 2));
    let pending = f.desk.status(&job_id).unwrap()["pending"].clone();
    assert_eq!(pending["kind"], "question");
    assert_eq!(pending["answer_with"], "job_reply");
    assert_eq!(pending["questions"].as_array().unwrap().len(), 2);
    assert_eq!(pending["questions"][1]["options"][1]["number"], 2);
    assert_eq!(pending["questions"][1]["options"][1]["label"], "Right");
}

#[test]
fn a_huge_tool_input_is_previewed_not_dumped() {
    let f = fixture();
    let job_id = start(&f, "task");
    let mut pending = permission(1);
    pending.tool_input = Some(json!({ "content": "x".repeat(50_000) }));
    park(&f, &job_id, pending);
    let input = f.desk.status(&job_id).unwrap()["pending"]["input"].clone();
    let text = input.as_str().expect("a cut preview is a string");
    assert!(text.chars().count() <= MAX_INPUT_PREVIEW_CHARS + 1);
    assert!(text.ends_with('…'));
}

// ── job_result ───────────────────────────────────────────────────────

#[test]
fn a_running_job_has_no_result_yet() {
    let f = fixture();
    let job_id = start(&f, "task");
    for status in [
        BackgroundJobStatus::Queued,
        BackgroundJobStatus::Running,
        BackgroundJobStatus::NeedsInput,
    ] {
        set(&f, &job_id, |state| state.process.status = status);
        let refused = err(f.desk.result(ResultRequest {
            job_id: job_id.clone(),
            max_chars: None,
        }));
        assert!(refused.contains("not ready"), "{status:?}: {refused}");
    }
    assert!(!result::result_path(&f.store, &job_id).exists());
}

#[test]
fn a_finished_job_writes_its_result_file_and_previews_the_end() {
    let f = fixture();
    let job_id = start(&f, "task");
    let answer = format!("{}\n\nConclusion: shipped.", "detail ".repeat(200));
    finish(&f, &job_id, BackgroundJobStatus::Succeeded, Some(&answer));

    let result = f
        .desk
        .result(ResultRequest {
            job_id: job_id.clone(),
            max_chars: Some(250),
        })
        .unwrap();
    let path = PathBuf::from(result["result_path"].as_str().unwrap());
    assert_eq!(path, f.store.job_dir(&job_id).join(result::RESULT_FILE));
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written, format!("{}\n", answer.trim()));
    assert_eq!(result["truncated"], true);
    let summary = result["summary"].as_str().unwrap();
    assert!(summary.ends_with("Conclusion: shipped.\n"), "{summary}");
    assert_eq!(summary.chars().count(), 251, "the cut plus its marker");

    let whole = f
        .desk
        .result(ResultRequest {
            job_id,
            max_chars: Some(1_000_000),
        })
        .unwrap();
    assert_eq!(
        whole["truncated"], false,
        "max_chars is clamped, not refused"
    );
}

#[test]
fn a_failed_job_says_why_in_its_result() {
    let f = fixture();
    let job_id = start(&f, "task");
    finish(
        &f,
        &job_id,
        BackgroundJobStatus::Failed,
        Some("Got halfway."),
    );
    let result = f
        .desk
        .result(ResultRequest {
            job_id,
            max_chars: None,
        })
        .unwrap();
    let summary = result["summary"].as_str().unwrap();
    assert!(summary.starts_with("Got halfway."), "{summary}");
    assert!(
        summary.contains("The job failed: provider returned 529"),
        "{summary}"
    );
}

// ── job_cancel ───────────────────────────────────────────────────────

#[test]
fn cancel_stops_the_job_without_pushing_the_stop_back() {
    let f = fixture();
    let job_id = start(&f, "task");
    let cancelled = f.desk.cancel(&job_id, 5).unwrap();
    assert_eq!(cancelled["ok"], true);
    assert_eq!(
        f.store.read_state(&job_id).unwrap().status(),
        BackgroundJobStatus::Stopped
    );
    assert!(
        f.desk.observe(&job_id).update.is_none(),
        "the client that asked for the stop is not told about it"
    );
    let again = f.desk.cancel(&job_id, 6).unwrap();
    assert_eq!(again["already"], "stopped");
}

// ── job_reply ────────────────────────────────────────────────────────

#[test]
fn a_reply_to_a_finished_job_queues_its_next_turn() {
    let f = fixture();
    let job_id = start(&f, "task");
    finish(&f, &job_id, BackgroundJobStatus::Succeeded, Some("done"));
    let replied = f
        .desk
        .reply(ReplyRequest {
            job_id: job_id.clone(),
            text: Some("  now add tests  ".into()),
            query_id: None,
            answers: None,
        })
        .unwrap();
    assert_eq!(replied["delivered"], "follow_up_queued");
    let state = f.store.read_state(&job_id).unwrap();
    assert_eq!(state.status(), BackgroundJobStatus::Queued);
    assert_eq!(state.identity.pending_prompts[0].text, "now add tests");

    let refused = err(f.desk.reply(ReplyRequest {
        job_id: job_id.clone(),
        text: None,
        query_id: None,
        answers: None,
    }));
    assert!(refused.contains("`text` is required"), "{refused}");
    let refused = err(f.desk.reply(ReplyRequest {
        job_id,
        text: Some("x".into()),
        query_id: Some(4),
        answers: None,
    }));
    assert!(refused.contains("not waiting on a question"), "{refused}");
}

#[test]
fn a_question_is_answered_only_with_its_own_query_id_and_the_right_shape() {
    let f = fixture();
    let job_id = start(&f, "task");
    park(&f, &job_id, question(3, 1));

    let stale = err(f.desk.reply(ReplyRequest {
        job_id: job_id.clone(),
        text: Some("left".into()),
        query_id: Some(2),
        answers: None,
    }));
    assert!(stale.contains("no longer pending"), "{stale}");
    let unnamed = err(f.desk.reply(ReplyRequest {
        job_id: job_id.clone(),
        text: Some("left".into()),
        query_id: None,
        answers: None,
    }));
    assert!(unnamed.contains("pass it as `query_id`"), "{unnamed}");
    let zero = err(f.desk.reply(ReplyRequest {
        job_id: job_id.clone(),
        text: None,
        query_id: Some(3),
        answers: Some(vec![AnswerRequest {
            selected: vec![0],
            text: None,
        }]),
    }));
    assert!(zero.contains("start at 1"), "{zero}");

    // Every check here passed: the answer reached the host, which has no
    // live worker to hand it to in this test.
    let forwarded = err(f.desk.reply(ReplyRequest {
        job_id: job_id.clone(),
        text: Some("left, please".into()),
        query_id: Some(3),
        answers: None,
    }));
    assert!(
        forwarded.contains("endpoint") || forwarded.contains("IPC"),
        "{forwarded}"
    );

    park(&f, &job_id, question(4, 2));
    let ambiguous = err(f.desk.reply(ReplyRequest {
        job_id,
        text: Some("left".into()),
        query_id: Some(4),
        answers: None,
    }));
    assert!(ambiguous.contains("asked 2 questions"), "{ambiguous}");
}

#[test]
fn a_permission_prompt_is_not_a_question() {
    let f = fixture();
    let job_id = start(&f, "task");
    park(&f, &job_id, permission(7));
    let refused = err(f.desk.reply(ReplyRequest {
        job_id,
        text: Some("yes".into()),
        query_id: Some(7),
        answers: None,
    }));
    assert!(refused.contains("job_permit"), "{refused}");
}

// ── job_permit ───────────────────────────────────────────────────────

#[test]
fn permit_is_fail_closed_on_the_query_the_option_and_lasting_grants() {
    let f = fixture();
    let job_id = start(&f, "task");
    let permit = |query_id, option_id: &str| {
        f.desk.permit(PermitRequest {
            job_id: job_id.clone(),
            query_id,
            option_id: option_id.into(),
        })
    };
    assert!(err(permit(7, "allow_once")).contains("not waiting on a permission"));

    park(&f, &job_id, permission(7));
    assert!(err(permit(6, "allow_once")).contains("no longer pending"));
    let unknown = err(permit(7, "sudo"));
    assert!(unknown.contains("allow_once, reject_once"), "{unknown}");
    assert!(err(permit(7, "allow_always")).contains("persist"));
    assert!(
        err(permit(7, "mystery")).contains("persist"),
        "unknown kinds are refused"
    );

    let forwarded = err(permit(7, "allow_once"));
    assert!(
        forwarded.contains("no live worker endpoint"),
        "every check passed and the host was asked: {forwarded}"
    );

    park(&f, &job_id, question(8, 1));
    assert!(err(permit(8, "allow_once")).contains("job_reply"));
}

// ── what the watcher sees ────────────────────────────────────────────

#[test]
fn a_finished_job_is_observed_once_with_its_result_file_ready() {
    let f = fixture();
    let job_id = start(&f, "task");
    let running = f.desk.observe(&job_id);
    assert!(running.active && running.update.is_none() && !running.gone);

    finish(
        &f,
        &job_id,
        BackgroundJobStatus::Succeeded,
        Some("All green."),
    );
    let observed = f.desk.observe(&job_id);
    assert!(!observed.active);
    let Some(Update::Settled {
        state,
        turn,
        duration_ms,
        result_path: Some(path),
        ..
    }) = observed.update.clone()
    else {
        panic!("expected a settled update: {observed:?}");
    };
    assert_eq!(
        (state, turn, duration_ms),
        (BackgroundJobStatus::Succeeded, 1, Some(2_500))
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "All green.\n");

    let claimed = f.desk.claim(vec![observed.update.unwrap()], 1);
    assert_eq!(claimed.len(), 1);
    assert!(
        f.desk.observe(&job_id).update.is_none(),
        "pushed once, then quiet"
    );
}

#[test]
fn a_parked_job_is_observed_per_query_and_an_idle_job_only_after_a_turn() {
    let f = fixture();
    let job_id = start(&f, "task");
    park(&f, &job_id, permission(7));
    let observed = f.desk.observe(&job_id);
    assert!(observed.active);
    assert_eq!(
        observed.update,
        Some(Update::NeedsInput {
            job_id: job_id.clone(),
            turn: 1,
            query_id: 7,
            pending: Pending::Permission {
                tool: "Bash".into()
            },
        })
    );
    f.desk.claim(vec![observed.update.unwrap()], 1);
    park(&f, &job_id, permission(8));
    assert!(
        matches!(
            f.desk.observe(&job_id).update,
            Some(Update::NeedsInput { query_id: 8, .. })
        ),
        "a second prompt in the same turn is news"
    );

    let idle = start(&f, "never ran");
    set(&f, &idle, |state| {
        state.process.status = BackgroundJobStatus::Idle
    });
    assert!(
        f.desk.observe(&idle).update.is_none(),
        "idle before any turn"
    );
    set(&f, &idle, |state| state.process.turn_generation = 1);
    assert!(matches!(
        f.desk.observe(&idle).update,
        Some(Update::Settled {
            state: BackgroundJobStatus::Idle,
            result_path: None,
            ..
        })
    ));
}

#[test]
fn a_removed_job_is_gone_and_leaves_the_tracked_set() {
    let f = fixture();
    let job_id = start(&f, "task");
    std::fs::remove_dir_all(f.store.job_dir(&job_id)).unwrap();
    assert!(f.desk.observe(&job_id).gone);
    let tick = f.desk.observe_tracked();
    assert!(tick.updates.is_empty());
    assert!(f.desk.tracked().is_empty());
}

#[test]
fn two_servers_never_push_the_same_update() {
    let first = fixture();
    let job_id = start(&first, "task");
    finish(&first, &job_id, BackgroundJobStatus::Succeeded, Some("ok"));
    let second = Desk::new(DeskConfig {
        store: first.store.clone(),
        projects_root: first.projects_root.clone(),
        root: first.root.clone(),
        rebon_exe: PathBuf::from("./unused"),
        launch_gate: Arc::new(|_: &BackgroundRuntimeFields| Ok(())),
        owner: LedgerOwner {
            pid: 1,
            pid_identity: Some("another server".into()),
        },
        channel: true,
    });
    let update = first.desk.observe(&job_id).update.unwrap();
    assert_eq!(second.observe(&job_id).update, Some(update.clone()));
    let pushed_by_first = first.desk.claim(vec![update.clone()], 1);
    let pushed_by_second = second.claim(vec![update], 2);
    assert_eq!((pushed_by_first.len(), pushed_by_second.len()), (1, 0));
}

#[test]
fn a_restarted_server_takes_over_what_a_dead_one_left() {
    let dead = LedgerOwner {
        pid: finished_pid(),
        pid_identity: Some("gone".into()),
    };
    let before = fixture_with(dead, Ok(()));
    let job_id = start(&before, "task");
    let after = Desk::new(DeskConfig {
        store: before.store.clone(),
        projects_root: before.projects_root.clone(),
        root: before.root.clone(),
        rebon_exe: PathBuf::from("./unused"),
        launch_gate: Arc::new(|_: &BackgroundRuntimeFields| Ok(())),
        owner: LedgerOwner::this_process(),
        channel: true,
    });
    assert!(after.tracked().is_empty());
    assert_eq!(after.adopt_orphans(2_000), vec![job_id.clone()]);
    assert_eq!(after.tracked(), vec![job_id.clone()]);
    assert_eq!(
        ledger::read(&before.store, &job_id).unwrap().unwrap().owner,
        LedgerOwner::this_process()
    );
}

#[test]
fn replying_through_a_server_makes_it_the_one_that_reports_back() {
    let dead = LedgerOwner {
        pid: finished_pid(),
        pid_identity: Some("gone".into()),
    };
    let before = fixture_with(dead, Ok(()));
    let job_id = start(&before, "task");
    finish(
        &before,
        &job_id,
        BackgroundJobStatus::Succeeded,
        Some("done"),
    );
    let after = Desk::new(DeskConfig {
        store: before.store.clone(),
        projects_root: before.projects_root.clone(),
        root: before.root.clone(),
        rebon_exe: PathBuf::from("./unused"),
        launch_gate: Arc::new(|_: &BackgroundRuntimeFields| Ok(())),
        owner: LedgerOwner::this_process(),
        channel: true,
    });
    after
        .reply(ReplyRequest {
            job_id: job_id.clone(),
            text: Some("more".into()),
            query_id: None,
            answers: None,
        })
        .unwrap();
    assert_eq!(after.tracked(), vec![job_id]);
}

/// The pid of a process that has exited.
fn finished_pid() -> u32 {
    let mut child = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .unwrap()
    } else {
        std::process::Command::new("true").spawn().unwrap()
    };
    let pid = child.id();
    child.wait().unwrap();
    pid
}
