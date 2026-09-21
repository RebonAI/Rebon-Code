//! The compatibility contract that must not break while the session host is
//! assembled: the flat job JSON on disk, the IPC request/response/event wire,
//! the four owner states, the lease state machine, permission fencing and
//! `command_id` idempotency.
//!
//! These are goldens, not behaviour experiments. Each one records what a build
//! shipped today writes and reads, so that a refactor which reorganises the
//! Rust side — notably the five-block `BackgroundJobState` split — is
//! forced to prove it changed no byte a peer or an on-disk record depends on.
//!
//! # What "byte identical" means here
//!
//! JSON object key order carries no meaning: every reader in this repository is
//! serde, and both `serde_json` configurations in the tree (the root workspace's
//! sorted `BTreeMap` and the desktop app's `preserve_order` `IndexMap`) compare
//! objects as unordered maps. So the byte golden below is taken over the
//! *canonical* encoding — keys sorted recursively — which is exactly the set of
//! bytes a peer can observe, and is invariant under the block reordering the
//! split performs. [`the_flat_job_json_carries_exactly_these_keys`] separately pins
//! that no field is added, renamed or dropped.
//!
//! # Old worker × new client compatibility matrix
//!
//! The wire is frozen; the only permitted increment is an additive
//! variant inside [`crate::BackgroundIpcRequest`] (the `CancelCall`). That
//! yields four combinations, and each row names where it is proven:
//!
//! | client | worker | expected | proven by |
//! |---|---|---|---|
//! | old | old | today's behaviour | the whole existing suite |
//! | new | old | every request the old worker already knows is answered unchanged; a request variant the old worker does not know fails *that one* request with a decode error and never bumps the envelope version | `the_envelope_version_does_not_move_when_a_request_variant_is_added`, [`crate::BACKGROUND_IPC_PROTOCOL_VERSION`] |
//! | old | new | an old client sends no `protocolVersion`, no `commandId`, and addresses by job id; all three still decode | `an_envelope_from_before_the_new_fields_still_decodes` in `protocol.rs`, `an_old_client_envelope_without_optional_fields_still_decodes` here |
//! | new | new | the full semantics | the newer suites |
//!
//! Two rows cannot be automated inside this crate and belong to the runtime
//! checklist instead, because they need two real processes built from two
//! different commits:
//!
//! 1. a worker from before `CancelCall` exists, receiving one from a new client:
//!    the client must release its waiter on schedule rather than block;
//! 2. a job state file written by an old build, opened by a new worker, and
//!    written back — the disk round trip is pinned here field-for-field by
//!    `a_flat_job_json_written_by_an_older_build_round_trips_field_for_field`,
//!    but that the *file* survives a real worker turn is a runtime item.

use std::time::Duration;

use crate::owner::{resolve_owner, OwnerState};
use crate::{
    validate_permission_target, BackgroundImageAttachment, BackgroundIpcCancelFence,
    BackgroundIpcEndpoint, BackgroundIpcEnvelope, BackgroundIpcRequest, BackgroundIpcResponse,
    BackgroundJobState, BackgroundJobStatus, BackgroundPermissionOptionSnapshot,
    BackgroundPermissionQuerySnapshot, BackgroundPullRequestDotStatus, BackgroundPullRequestStatus,
    BackgroundRetryProgress, BackgroundRuntimeFields, ClientLease, ClientLeaseKind,
    ForegroundQuestionAnswer, JobIdentityIntent, JobPlacement, LeaseLifecycle, OutcomeUsage,
    PendingPrompt, ProcessOwnership, RewindScopeWire, SessionEvent, SessionStatusSnapshot,
    SessionUsageSnapshot, TurnStreamState, WorkspaceIsolation, BACKGROUND_IPC_PROTOCOL_VERSION,
};

/// Every JSON value re-emitted with object keys in sorted order.
///
/// The two workspaces in this tree disagree about whether `serde_json::Value`
/// remembers insertion order, so a golden taken over `to_string` alone would
/// pass in one and fail in the other for a reason that is not a wire change.
/// Sorting first makes the golden say what it means: these keys, these values.
fn canonical(value: &serde_json::Value) -> String {
    fn write(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(key).expect("a string key"));
                    out.push(':');
                    write(&map[key], out);
                }
                out.push('}');
            }
            serde_json::Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&serde_json::to_string(other).expect("a scalar")),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// The canonical encoding of anything that serializes.
fn canonical_of<T: serde::Serialize>(value: &T) -> String {
    canonical(&serde_json::to_value(value).expect("serializable"))
}

/// A job state with every one of its fields set to a distinct, non-default
/// value.
///
/// Deliberately not built through [`BackgroundJobState::new`]: the point is to
/// hold every field at once, including the ones only a worker ever writes, so
/// that a block split which forgets one is caught by the golden rather than by
/// a user whose job record lost its worktree path.
fn fully_populated_job() -> BackgroundJobState {
    BackgroundJobState {
        identity: JobIdentityIntent {
            job_id: "job-contract".to_string(),
            session_id: Some("sess-contract".to_string()),
            parent_job_id: Some("job-parent".to_string()),
            respawned_job_id: Some("job-respawn".to_string()),
            name: "contract job".to_string(),
            agent_type: Some("reviewer".to_string()),
            cwd: "/repo/work".to_string(),
            prompt: "do the thing".to_string(),
            prompt_images: vec![BackgroundImageAttachment {
                id: 7,
                data: "aGk=".to_string(),
                media_type: "image/png".to_string(),
                filename: Some("shot.png".to_string()),
                source_path: Some("/tmp/shot.png".to_string()),
            }],
            pending_prompts: vec![PendingPrompt {
                id: "0000000000000001".to_string(),
                text: "and then this".to_string(),
                images: Vec::new(),
                coordinator_report_paths: vec!["/reports/one.md".to_string()],
                enqueued_at_ms: 1_700_000_000_100,
                claimed_turn_generation: Some(3),
                completed_turn_generation: Some(4),
            }],
            coordinator_report_grants: vec!["/reports/one.md".to_string()],
            runtime: BackgroundRuntimeFields {
                provider: Some("anthropic".to_string()),
                model: Some("claude-fable-5-1".to_string()),
                fast_mode: Some(true),
                channels: vec!["stable".to_string()],
                development_channels: vec!["nightly".to_string()],
                provider_format: Some("messages".to_string()),
                ui_mode: Some("tui".to_string()),
                effort_level: Some("high".to_string()),
                permission_mode: Some("auto".to_string()),
                capability_mode: rebon_types::AgentCapabilityMode::Minimal,
                settings: vec!["/settings.json".to_string()],
                add_dirs: vec!["/extra".to_string()],
                plugin_dirs: vec!["/plugins".to_string()],
                mcp_configs: vec!["/mcp.json".to_string()],
                strict_mcp_config: true,
            },
            resume_only: true,
            queue_session: true,
            pinned: true,
            sort_order: -3,
        },
        process: ProcessOwnership {
            status: BackgroundJobStatus::NeedsInput,
            pid: Some(4242),
            pid_identity: Some("4242:1700000000".to_string()),
            owner_detached_group: true,
            spawn_admitted: true,
            process_owner_fenced: true,
            removal_reserved: true,
            ipc_port: Some(45_201),
            ipc_token: Some("f0e1d2c3b4a596877869504132231405".to_string()),
            turn_generation: 9,
            process_path: Some("/usr/local/bin/rebon".to_string()),
            node_runtime_path: Some("/runtime/node".to_string()),
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_500,
            started_at_ms: Some(1_700_000_000_050),
            completed_at_ms: Some(1_700_000_000_900),
        },
        workspace: WorkspaceIsolation {
            isolate_in_worktree: true,
            require_worktree: true,
            preserve_worktree_on_success: true,
            worktree_path: Some("/repo/.worktrees/job".to_string()),
        },
        lease: LeaseLifecycle {
            placement: JobPlacement::Foreground,
            client_leases: vec![ClientLease {
                client_id: "tui-1".to_string(),
                kind: ClientLeaseKind::Tui,
                pid: Some(999),
                updated_at_ms: 1_700_000_000_400,
            }],
            linger_ms: Some(30_000),
            exit_when_idle: true,
        },
        outcome: OutcomeUsage {
            retry: Some(BackgroundRetryProgress {
                attempt: 2,
                max_retries: 10,
            }),
            exit_code: Some(3),
            error: Some("it went wrong".to_string()),
            summary: Some("a summary".to_string()),
            summary_updated_at_ms: Some(1_700_000_000_600),
            pull_requests: vec![BackgroundPullRequestStatus {
                url: "https://example.invalid/pr/1".to_string(),
                owner: "acme".to_string(),
                repo: "widgets".to_string(),
                number: 1,
                dot: Some(BackgroundPullRequestDotStatus::Ready),
                state: Some("OPEN".to_string()),
                merge_state: Some("CLEAN".to_string()),
                review_decision: Some("APPROVED".to_string()),
                checks_summary: Some("3/3".to_string()),
                error: Some("checks flaked once".to_string()),
                updated_at_ms: 1_700_000_000_700,
            }],
            pending_permission: Some(permission_snapshot()),
            event_count: 12,
            usage: Some(SessionUsageSnapshot {
                input_tokens: 11,
                output_tokens: 22,
                cache_read_tokens: 33,
                cache_creation_tokens: 44,
            }),
        },
    }
}

fn permission_snapshot() -> BackgroundPermissionQuerySnapshot {
    BackgroundPermissionQuerySnapshot {
        query_id: 5,
        turn_generation: 9,
        endpoint: Some(BackgroundIpcEndpoint {
            pid: 4242,
            port: 45_201,
            token: "f0e1d2c3b4a596877869504132231405".to_string(),
        }),
        tool: Some("Bash".to_string()),
        tool_call_id: Some("call-1".to_string()),
        session_id: Some("sess-contract".to_string()),
        title: Some("Run a command".to_string()),
        message: Some("rm -rf nothing".to_string()),
        tool_input: Some(serde_json::json!({"command": "ls"})),
        metadata: Some(serde_json::json!({"origin": "test"})),
        options: vec![
            BackgroundPermissionOptionSnapshot {
                option_id: "allow-once".to_string(),
                label: "Allow once".to_string(),
                kind: "allow_once".to_string(),
            },
            BackgroundPermissionOptionSnapshot {
                option_id: "reject-once".to_string(),
                label: "Reject".to_string(),
                kind: "reject_once".to_string(),
            },
        ],
    }
}

/// The canonical flat JSON a fully populated job writes.
///
/// Regenerate deliberately, never to make a red test green: a diff here is a
/// change to what every other build of Rebon reads out of `~/.rebon`.
const FULL_JOB_GOLDEN: &str = include_str!("wire_contract_goldens/full_job.json");

/// The same job with only the fields a fresh record actually carries, which is
/// what pins every `skip_serializing_if`.
const MINIMAL_JOB_GOLDEN: &str = include_str!("wire_contract_goldens/minimal_job.json");

fn minimal_job() -> BackgroundJobState {
    BackgroundJobState {
        identity: JobIdentityIntent {
            job_id: "job-min".to_string(),
            session_id: None,
            parent_job_id: None,
            respawned_job_id: None,
            name: "job-min".to_string(),
            agent_type: None,
            cwd: "/repo".to_string(),
            prompt: "hello".to_string(),
            prompt_images: Vec::new(),
            pending_prompts: Vec::new(),
            coordinator_report_grants: Vec::new(),
            runtime: BackgroundRuntimeFields {
                provider: None,
                model: None,
                fast_mode: None,
                channels: Vec::new(),
                development_channels: Vec::new(),
                provider_format: None,
                ui_mode: None,
                effort_level: None,
                permission_mode: None,
                capability_mode: rebon_types::AgentCapabilityMode::Normal,
                settings: Vec::new(),
                add_dirs: Vec::new(),
                plugin_dirs: Vec::new(),
                mcp_configs: Vec::new(),
                strict_mcp_config: false,
            },
            resume_only: false,
            queue_session: false,
            pinned: false,
            sort_order: 0,
        },
        process: ProcessOwnership {
            status: BackgroundJobStatus::Queued,
            pid: None,
            pid_identity: None,
            owner_detached_group: false,
            spawn_admitted: false,
            process_owner_fenced: false,
            removal_reserved: false,
            ipc_port: None,
            ipc_token: None,
            turn_generation: 0,
            process_path: None,
            node_runtime_path: None,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_000,
            started_at_ms: None,
            completed_at_ms: None,
        },
        workspace: WorkspaceIsolation {
            isolate_in_worktree: false,
            require_worktree: false,
            preserve_worktree_on_success: false,
            worktree_path: None,
        },
        lease: LeaseLifecycle {
            placement: JobPlacement::Background,
            client_leases: Vec::new(),
            linger_ms: None,
            exit_when_idle: false,
        },
        outcome: OutcomeUsage {
            retry: None,
            exit_code: None,
            error: None,
            summary: None,
            summary_updated_at_ms: None,
            pull_requests: Vec::new(),
            pending_permission: None,
            event_count: 0,
            usage: None,
        },
    }
}

/// Rewrites the two goldens from the fixtures above.
///
/// Ignored, so it only runs when somebody asks for it by name:
///
/// ```text
/// cargo test -p rebon-session-host --lib regenerate_the_job_state_goldens -- --ignored --exact
/// ```
///
/// Reach for it when a field is *deliberately* added to or removed from the job
/// record, and put the resulting diff in the commit that made the change. Never
/// to make a red test green: the diff is the wire change, and it is the only
/// place a reviewer will see it.
#[test]
#[ignore = "rewrites the goldens; run it only when the job record deliberately changed"]
fn regenerate_the_job_state_goldens() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/wire_contract_goldens");
    std::fs::write(
        dir.join("full_job.json"),
        canonical_of(&fully_populated_job()),
    )
    .expect("the goldens directory is writable");
    std::fs::write(dir.join("minimal_job.json"), canonical_of(&minimal_job()))
        .expect("the goldens directory is writable");
}

#[test]
fn a_fully_populated_job_state_writes_the_same_canonical_bytes() {
    assert_eq!(canonical_of(&fully_populated_job()), FULL_JOB_GOLDEN.trim());
}

#[test]
fn a_minimal_job_state_skips_every_defaulted_field() {
    assert_eq!(canonical_of(&minimal_job()), MINIMAL_JOB_GOLDEN.trim());
}

/// No field added, renamed or dropped by a refactor.
///
/// Separate from the value golden because this is the assertion a reviewer can
/// read at a glance: 49 names, spelled the way they are spelled on disk.
#[test]
fn the_flat_job_json_carries_exactly_these_keys() {
    let value = serde_json::to_value(fully_populated_job()).expect("serializable");
    let mut keys: Vec<String> = value
        .as_object()
        .expect("a job state is a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "agentType",
            "clientLeases",
            "completedAtMs",
            "coordinatorReportGrants",
            "createdAtMs",
            "cwd",
            "error",
            "eventCount",
            "exitCode",
            "exitWhenIdle",
            "ipcPort",
            "ipcToken",
            "isolateInWorktree",
            "jobId",
            "lingerMs",
            "name",
            "nodeRuntimePath",
            "ownerDetachedGroup",
            "parentJobId",
            "pendingPermission",
            "pendingPrompts",
            "pid",
            "pidIdentity",
            "pinned",
            "placement",
            "preserveWorktreeOnSuccess",
            "processOwnerFenced",
            "processPath",
            "prompt",
            "promptImages",
            "pullRequests",
            "queueSession",
            "removalReserved",
            "requireWorktree",
            "respawnedJobId",
            "resumeOnly",
            "retry",
            "runtime",
            "sessionId",
            "sortOrder",
            "spawnAdmitted",
            "startedAtMs",
            "status",
            "summary",
            "summaryUpdatedAtMs",
            "turnGeneration",
            "updatedAtMs",
            "usage",
            "worktreePath",
        ]
    );
    assert_eq!(keys.len(), 49);
}

/// A record an older build left on disk, read and written back unchanged.
///
/// This is the disk half of the compatibility matrix: the block split may
/// reorganise the Rust struct however it likes, but a file that was already
/// there has to come back out with the same fields carrying the same values.
#[test]
fn a_flat_job_json_written_by_an_older_build_round_trips_field_for_field() {
    let on_disk: serde_json::Value =
        serde_json::from_str(FULL_JOB_GOLDEN).expect("the golden is valid JSON");
    let state: BackgroundJobState =
        serde_json::from_value(on_disk.clone()).expect("the golden deserializes");
    let written = serde_json::to_value(&state).expect("serializable");
    assert_eq!(written, on_disk);
    assert_eq!(state, fully_populated_job());
}

/// Fields an older build never wrote are absent, not null, and read as their
/// documented defaults rather than failing the whole record.
#[test]
fn a_job_json_missing_every_optional_field_still_reads() {
    let bare = serde_json::json!({
        "jobId": "job-bare",
        "cwd": "/repo",
        "prompt": "hello",
        "name": "job-bare",
        "status": "queued",
        "createdAtMs": 1_700_000_000_000u64,
        "updatedAtMs": 1_700_000_000_000u64,
        "runtime": {},
    });
    let state: BackgroundJobState = serde_json::from_value(bare).expect("a pre-C3 record reads");
    assert_eq!(state, minimal_job_named("job-bare"));
}

fn minimal_job_named(job_id: &str) -> BackgroundJobState {
    let mut state = minimal_job();
    state.identity.job_id = job_id.to_string();
    state.identity.name = job_id.to_string();
    state
}

/// An unknown field is ignored rather than rejected, which is what lets a newer
/// build write a record an older one still opens.
#[test]
fn an_unknown_field_does_not_reject_the_record() {
    let mut value: serde_json::Value =
        serde_json::from_str(MINIMAL_JOB_GOLDEN).expect("valid JSON");
    value
        .as_object_mut()
        .expect("object")
        .insert("somethingFromTheFuture".to_string(), serde_json::json!(1));
    let state: BackgroundJobState = serde_json::from_value(value).expect("unknown fields ignored");
    assert_eq!(state, minimal_job());
}

// ---------------------------------------------------------------------------
// Request / response / event wire
// ---------------------------------------------------------------------------

/// Every request variant, in the encoding a peer parses.
///
/// Listed one per line rather than derived from the enum so that adding a
/// variant is a visible edit here: exactly one addition is permitted
/// (`CancelCall`) and nothing else.
///
/// Note the case: `#[serde(rename_all = "camelCase")]` on an enum renames the
/// *variant* tags, not the fields inside a struct variant, so `taskId` is
/// `task_id` and `optionId` is `option_id` on this wire. Named types reached
/// from a variant — the cancel fence, a question answer — carry their own
/// `rename_all` and so stay camelCase. Nothing about that is deliberate design,
/// but it is what every shipped peer parses, and it may not quietly be fixed.
#[test]
fn every_request_variant_has_the_encoding_it_has_today() {
    let cases: Vec<(BackgroundIpcRequest, &str)> = vec![
        (BackgroundIpcRequest::Ping, r#""ping""#),
        (
            BackgroundIpcRequest::RunCommand {
                name: "model".to_string(),
                args: vec!["opus".to_string()],
            },
            r#"{"runCommand":{"args":["opus"],"name":"model"}}"#,
        ),
        (
            BackgroundIpcRequest::Reply {
                message: "go on".to_string(),
                images: Vec::new(),
            },
            r#"{"reply":{"message":"go on"}}"#,
        ),
        (
            BackgroundIpcRequest::ReplyTask {
                task_id: "t1".to_string(),
                message: "carry on".to_string(),
            },
            r#"{"replyTask":{"message":"carry on","task_id":"t1"}}"#,
        ),
        (
            BackgroundIpcRequest::SetPermissionMode {
                mode: "auto".to_string(),
            },
            r#"{"setPermissionMode":{"mode":"auto"}}"#,
        ),
        (
            BackgroundIpcRequest::PermissionAnswer {
                query_id: 5,
                turn_generation: 9,
                option_id: Some("allow-once".to_string()),
                extra_text: None,
                updated_input: None,
            },
            r#"{"permissionAnswer":{"extra_text":null,"option_id":"allow-once","query_id":5,"turn_generation":9}}"#,
        ),
        (
            BackgroundIpcRequest::AnswerQuestions {
                query_id: 5,
                turn_generation: 9,
                answers: vec![ForegroundQuestionAnswer {
                    selected_options: vec![0],
                    other_text: Some("or this".to_string()),
                }],
            },
            r#"{"answerQuestions":{"answers":[{"otherText":"or this","selectedOptions":[0]}],"query_id":5,"turn_generation":9}}"#,
        ),
        (
            BackgroundIpcRequest::CancelTasks {
                task_ids: vec!["t1".to_string()],
            },
            r#"{"cancelTasks":{"task_ids":["t1"]}}"#,
        ),
        (
            BackgroundIpcRequest::Cancel {
                fence: BackgroundIpcCancelFence {
                    status: BackgroundJobStatus::Running,
                    turn_generation: 9,
                    updated_at_ms: 1_700_000_000_500,
                    pending_permission_query_id: Some(5),
                },
            },
            r#"{"cancel":{"fence":{"pendingPermissionQueryId":5,"status":"running","turnGeneration":9,"updatedAtMs":1700000000500}}}"#,
        ),
        (BackgroundIpcRequest::Status, r#""status""#),
        (
            BackgroundIpcRequest::SetSessionOption {
                key: "model".to_string(),
                value: "opus".to_string(),
            },
            r#"{"setSessionOption":{"key":"model","value":"opus"}}"#,
        ),
        (
            BackgroundIpcRequest::Steer {
                message: "actually".to_string(),
                images: Vec::new(),
            },
            r#"{"steer":{"message":"actually"}}"#,
        ),
        (
            BackgroundIpcRequest::Rewind {
                user_message_uuid: "uuid-1".to_string(),
                scope: RewindScopeWire::Conversation,
            },
            r#"{"rewind":{"scope":"conversation","user_message_uuid":"uuid-1"}}"#,
        ),
        (
            BackgroundIpcRequest::Compact { instructions: None },
            r#"{"compact":{}}"#,
        ),
        (
            BackgroundIpcRequest::ReconcilePlugins,
            r#""reconcilePlugins""#,
        ),
        (
            BackgroundIpcRequest::Lease {
                client_id: "tui-1".to_string(),
                kind: ClientLeaseKind::Tui,
            },
            r#"{"lease":{"client_id":"tui-1","kind":"tui"}}"#,
        ),
        (
            BackgroundIpcRequest::Subscribe { since: Some(12) },
            r#"{"subscribe":{"since":12}}"#,
        ),
        (
            BackgroundIpcRequest::ReleaseLease {
                client_id: "tui-1".to_string(),
                deliberate: true,
            },
            r#"{"releaseLease":{"client_id":"tui-1","deliberate":true}}"#,
        ),
        // The one permitted addition to this enum, landed by the client.
        // Additive inside this enum: an owner that does not know it fails this
        // one request and the envelope version does not move.
        (
            BackgroundIpcRequest::CancelCall {
                command_id: "cmd-7".to_string(),
            },
            r#"{"cancelCall":{"command_id":"cmd-7"}}"#,
        ),
    ];
    assert_eq!(cases.len(), 19, "one case per request variant");
    for (request, expected) in cases {
        assert_eq!(canonical_of(&request), expected, "{request:?}");
        let decoded: BackgroundIpcRequest =
            serde_json::from_str(expected).expect("the golden decodes");
        assert_eq!(decoded, request);
    }
}

#[test]
fn every_session_event_variant_has_the_encoding_it_has_today() {
    let status = Box::new(status_snapshot());
    let cases: Vec<(SessionEvent, &str)> = vec![
        (
            SessionEvent::SessionUpdate {
                cursor: 3,
                update: serde_json::json!({"type": "text"}),
            },
            r#"{"cursor":3,"kind":"sessionUpdate","update":{"type":"text"}}"#,
        ),
        (
            SessionEvent::Turn {
                cursor: 4,
                state: TurnStreamState::Running,
                stop_reason: None,
                stop_refused: None,
            },
            r#"{"cursor":4,"kind":"turn","state":"running"}"#,
        ),
        (
            SessionEvent::Turn {
                cursor: 5,
                state: TurnStreamState::Idle,
                stop_reason: Some("end_turn".to_string()),
                stop_refused: None,
            },
            r#"{"cursor":5,"kind":"turn","state":"idle","stop_reason":"end_turn"}"#,
        ),
        (
            SessionEvent::Permission {
                cursor: 6,
                query: Box::new(permission_snapshot()),
            },
            PERMISSION_EVENT_GOLDEN,
        ),
        (
            SessionEvent::Gap { from: 7, to: 9 },
            r#"{"from":7,"kind":"gap","to":9}"#,
        ),
    ];
    for (event, expected) in cases {
        assert_eq!(canonical_of(&event), expected, "{event:?}");
        let decoded: SessionEvent = serde_json::from_str(expected).expect("the golden decodes");
        assert_eq!(decoded, event);
    }
    // Hello and Status carry the whole status snapshot; asserted separately so
    // the snapshot's own shape is pinned in one place.
    let hello = SessionEvent::Hello {
        cursor: 1,
        turn_generation: 9,
        status: status.clone(),
        epoch: 2,
    };
    let encoded = canonical_of(&hello);
    assert!(
        encoded.starts_with(r#"{"cursor":1,"epoch":2,"kind":"hello","status":{"#),
        "{encoded}"
    );
    let decoded: SessionEvent = serde_json::from_str(&encoded).expect("hello round trips");
    assert_eq!(decoded, hello);
    let status_event = SessionEvent::Status {
        cursor: 2,
        snapshot: status,
    };
    let decoded: SessionEvent =
        serde_json::from_str(&canonical_of(&status_event)).expect("status round trips");
    assert_eq!(decoded, status_event);
}

const PERMISSION_EVENT_GOLDEN: &str = r#"{"cursor":6,"kind":"permission","query":{"endpoint":{"pid":4242,"port":45201,"token":"f0e1d2c3b4a596877869504132231405"},"message":"rm -rf nothing","metadata":{"origin":"test"},"options":[{"kind":"allow_once","label":"Allow once","optionId":"allow-once"},{"kind":"reject_once","label":"Reject","optionId":"reject-once"}],"queryId":5,"sessionId":"sess-contract","title":"Run a command","tool":"Bash","toolCallId":"call-1","toolInput":{"command":"ls"},"turnGeneration":9}}"#;

fn status_snapshot() -> SessionStatusSnapshot {
    SessionStatusSnapshot {
        job_id: "job-contract".to_string(),
        session_id: Some("sess-contract".to_string()),
        cwd: "/repo/work".to_string(),
        status: BackgroundJobStatus::Running,
        busy: true,
        turn_generation: 9,
        permission_mode: Some("auto".to_string()),
        plan_mode: false,
        model: Some("claude-fable-5-1".to_string()),
        effort: Some("high".to_string()),
        agent: Some("reviewer".to_string()),
        pending_permission: Some(permission_snapshot()),
        ask_user_questions: None,
        usage: Some(SessionUsageSnapshot {
            input_tokens: 11,
            output_tokens: 22,
            cache_read_tokens: 33,
            cache_creation_tokens: 44,
        }),
        mcp: None,
        client_leases: Vec::new(),
        last_command_id: Some("cmd-1".to_string()),
        last_command_at_ms: 1_700_000_000_400,
        last_command_error: None,
        updated_at_ms: 1_700_000_000_500,
    }
}

#[test]
fn the_three_response_shapes_are_what_a_peer_reads() {
    assert_eq!(canonical_of(&BackgroundIpcResponse::ok()), r#"{"ok":true}"#);
    assert_eq!(
        canonical_of(&BackgroundIpcResponse::failed("no")),
        r#"{"error":"no","ok":false}"#
    );
    assert_eq!(
        canonical_of(&BackgroundIpcResponse::with_data(&serde_json::json!({
            "generation": 2
        }))),
        r#"{"data":{"generation":2},"ok":true}"#
    );
    // `ok` and `error` are readable without understanding `data`, which is what
    // a version skew needs.
    let decoded: BackgroundIpcResponse =
        serde_json::from_str(r#"{"ok":true,"data":{"anything":[1,2]}}"#).expect("decodes");
    assert!(decoded.ok);
    assert!(decoded.error.is_none());
}

// ---------------------------------------------------------------------------
// Envelope, addressing and command_id
// ---------------------------------------------------------------------------

#[test]
fn an_old_client_envelope_without_optional_fields_still_decodes() {
    let envelope: BackgroundIpcEnvelope =
        serde_json::from_str(r#"{"jobId":"job-1","token":"tok","request":"ping"}"#)
            .expect("a pre-session-ownership envelope decodes");
    assert_eq!(envelope.protocol_version, 0);
    assert_eq!(envelope.job_id.as_deref(), Some("job-1"));
    assert!(envelope.session_id.is_none());
    assert!(envelope.command_id.is_none());
}

/// The envelope version is about the *envelope*. A new request variant must not
/// move it, or every added command would force the CLI and the desktop app to
/// ship in lockstep.
#[test]
fn the_envelope_version_does_not_move_when_a_request_variant_is_added() {
    assert_eq!(BACKGROUND_IPC_PROTOCOL_VERSION, 1);
    let envelope = BackgroundIpcEnvelope {
        protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
        job_id: None,
        session_id: Some("sess-contract".to_string()),
        command_id: Some("cmd-1".to_string()),
        token: "tok".to_string(),
        request: BackgroundIpcRequest::Ping,
    };
    assert_eq!(
        canonical_of(&envelope),
        r#"{"commandId":"cmd-1","protocolVersion":1,"request":"ping","sessionId":"sess-contract","token":"tok"}"#
    );
}

/// A retry carries the id the first attempt carried; that identity is the whole
/// idempotency contract on this side of the wire. The owner's replay of the
/// remembered result is asserted where the server lives.
#[test]
fn a_retry_is_the_same_bytes_as_the_attempt_it_repeats() {
    let make = || BackgroundIpcEnvelope {
        protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
        job_id: Some("job-1".to_string()),
        session_id: Some("sess-1".to_string()),
        command_id: Some("cmd-7".to_string()),
        token: "tok".to_string(),
        request: BackgroundIpcRequest::Reply {
            message: "again".to_string(),
            images: Vec::new(),
        },
    };
    assert_eq!(canonical_of(&make()), canonical_of(&make()));
    // And a fresh id is never reused for a different call.
    assert_ne!(crate::generate_command_id(), crate::generate_command_id());
}

// ---------------------------------------------------------------------------
// Permission fencing
// ---------------------------------------------------------------------------

#[test]
fn a_permission_answer_is_bound_to_the_turn_and_the_endpoint_that_asked() {
    let state = fully_populated_job();
    let snapshot = permission_snapshot();
    let endpoint = BackgroundIpcEndpoint {
        pid: 4242,
        port: 45_201,
        token: "f0e1d2c3b4a596877869504132231405".to_string(),
    };

    validate_permission_target(&state, &snapshot, Some(9), Some(&endpoint))
        .expect("the matching turn and endpoint are accepted");

    let err = validate_permission_target(&state, &snapshot, Some(10), Some(&endpoint))
        .expect_err("a different turn generation is refused");
    assert!(err.to_string().contains("turn generation"), "{err}");

    let other_endpoint = BackgroundIpcEndpoint {
        port: 45_202,
        ..endpoint.clone()
    };
    let err = validate_permission_target(&state, &snapshot, Some(9), Some(&other_endpoint))
        .expect_err("a different endpoint generation is refused");
    assert!(err.to_string().contains("endpoint generation"), "{err}");

    // The snapshot names the endpoint it was asked from, but the job has since
    // been claimed by a replacement owner: fail closed rather than letting the
    // answer reach the new one.
    let mut replaced = state.clone();
    replaced.process.ipc_port = Some(45_203);
    let err = validate_permission_target(&replaced, &snapshot, Some(9), Some(&endpoint))
        .expect_err("a replaced endpoint is refused");
    assert!(err.to_string().contains("endpoint changed"), "{err}");
}

// ---------------------------------------------------------------------------
// Owner four states and lease
// ---------------------------------------------------------------------------

/// A fifth `OwnerState` variant stops this compiling, which is the half of
/// the four-state contract a value-by-value list cannot express.
fn assert_owner_state_is_exhaustive(state: &OwnerState) {
    match state {
        OwnerState::Free
        | OwnerState::OwnedOpaque { .. }
        | OwnerState::OwnedUnreachable { .. }
        | OwnerState::OwnedReachable { .. } => {}
    }
}

/// The four states in one place, so that a client cannot quietly grow a
/// fifth or collapse two. Three are asserted by value below;
/// `assert_owner_state_is_exhaustive` covers all four at compile time,
/// because `OwnedUnreachable` needs a descriptor this test has no
/// listener to build.
///
/// `Free`, `OwnedOpaque` and `OwnedUnreachable` are also covered individually in
/// `owner.rs`; this asserts they are the same four a client can observe.
#[test]
fn an_owner_is_always_one_of_exactly_four_states() {
    let dir = tempfile::Builder::new()
        .prefix("rebon-contract-owner-")
        .tempdir()
        .expect("tempdir");
    let projects = dir.path();
    let cwd = "/work/contract";

    assert_eq!(
        resolve_owner(projects, cwd, "sess-free"),
        OwnerState::Free,
        "an unheld session is free"
    );

    let lock = rebon_session::try_acquire_session_active_lock(projects, cwd, "sess-opaque")
        .expect("the lock probe succeeds")
        .expect("a fresh session is free");
    assert!(
        matches!(
            resolve_owner(projects, cwd, "sess-opaque"),
            OwnerState::OwnedOpaque { .. }
        ),
        "a lock without a descriptor is opaque"
    );
    drop(lock);

    // `OwnedReachable` needs an endpoint, and `OwnedUnreachable` a descriptor
    // beside the lock — `owner.rs` builds that one with a real listener. Here
    // the discriminants a client matches on are asserted.
    let all = [
        OwnerState::Free,
        OwnerState::OwnedOpaque { descriptor: None },
        OwnerState::OwnedReachable {
            owner: crate::owner::OwnerHandle::for_worker(
                "sess-contract",
                Some("job-contract"),
                &BackgroundIpcEndpoint {
                    pid: 1,
                    port: 2,
                    token: "t".to_string(),
                },
            ),
        },
    ];
    for state in &all {
        assert_owner_state_is_exhaustive(state);
    }
    assert!(!all[0].is_owned());
    assert!(all[1].is_owned());
    assert!(all[2].handle().is_some());
    assert!(all[1].handle().is_none());
}

/// The lease TTL, the renewal cadence and the linger they feed are one policy.
/// A client that renews on time keeps its host up; one that stops does not.
#[test]
fn the_lease_state_machine_keeps_its_ttl_renewal_and_linger() {
    assert_eq!(crate::CLIENT_LEASE_TTL_MS, 15_000);
    assert_eq!(crate::CLIENT_LEASE_RENEW_INTERVAL_MS, 5_000);
    assert!(
        Duration::from_millis(crate::CLIENT_LEASE_RENEW_INTERVAL_MS) * 2
            < Duration::from_millis(crate::CLIENT_LEASE_TTL_MS),
        "a client must survive two missed renewals"
    );
    assert_eq!(JobPlacement::Foreground.default_linger_ms(), 10 * 60 * 1000);
    assert_eq!(JobPlacement::Background.default_linger_ms(), 60 * 60 * 1000);

    let mut state = minimal_job();
    let now = 1_000_000;
    state.touch_client_lease(
        ClientLease {
            client_id: "a".to_string(),
            kind: ClientLeaseKind::Serve,
            pid: None,
            updated_at_ms: now,
        },
        now,
    );
    assert!(state.has_live_client_lease(now));
    // A second tab is the same process-level lease, not a second one.
    state.touch_client_lease(
        ClientLease {
            client_id: "a".to_string(),
            kind: ClientLeaseKind::Serve,
            pid: None,
            updated_at_ms: now + 1,
        },
        now + 1,
    );
    assert_eq!(state.lease.client_leases.len(), 1);
    assert!(state.release_client_lease("a", now + 2));
    assert!(!state.release_client_lease("a", now + 2));
    assert!(!state.has_live_client_lease(now + 2));
}
