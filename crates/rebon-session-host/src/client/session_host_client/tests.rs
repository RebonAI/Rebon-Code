//! The client's half of the wire contract, against a real loopback owner.
//!
//! A concrete fake rather than a trait: endpoints depend on the
//! concrete connection and tests use a real socket, because the behaviour under
//! test here — a deadline expiring, a `CancelCall` going out, a waiter being
//! released — only exists on a real transport. A mock that returned
//! `Err(HostUnanswered)` would be asserting the test's own opinion.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::{
    BackgroundIpcEndpoint, BackgroundIpcEnvelope, BackgroundIpcRequest, BackgroundIpcResponse,
    BackgroundJobStatus, BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot,
    SessionStatusSnapshot,
};

/// How a fake owner answers one request.
enum Answer {
    /// Reply with this response.
    Reply(BackgroundIpcResponse),
    /// Read the request, then never answer. The client's read timeout is what
    /// ends it.
    Stall,
}

/// An owner that answers on loopback, and remembers what it was asked.
struct FakeOwner {
    port: u16,
    token: String,
    seen: Arc<Mutex<Vec<BackgroundIpcRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeOwner {
    /// Serve requests with `answer`, which is given each decoded request.
    fn serve<F>(answer: F) -> Self
    where
        F: Fn(&BackgroundIpcRequest) -> Answer + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
        let port = listener.local_addr().expect("bound").port();
        listener
            .set_nonblocking(true)
            .expect("a listener that can be stopped");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_seen = Arc::clone(&seen);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("fake-session-host".to_string())
            .spawn(move || {
                // Held open for the life of a stalled request, so the socket
                // stays connected while the client waits out its deadline.
                let mut stalled: Vec<TcpStream> = Vec::new();
                while !thread_stop.load(Ordering::Relaxed) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).expect("blocking");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("read timeout");
                    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        continue;
                    }
                    let Ok(envelope) = serde_json::from_str::<BackgroundIpcEnvelope>(&line) else {
                        continue;
                    };
                    let reply = answer(&envelope.request);
                    thread_seen.lock().expect("poisoned").push(envelope.request);
                    match reply {
                        Answer::Reply(response) => {
                            let mut stream = stream;
                            let _ = serde_json::to_writer(&mut stream, &response);
                            let _ = stream.write_all(b"\n");
                            let _ = stream.flush();
                        }
                        Answer::Stall => stalled.push(stream),
                    }
                }
            })
            .expect("the fake owner starts");
        Self {
            port,
            token: "fake-token".to_string(),
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn handle(&self) -> OwnerHandle {
        OwnerHandle::for_worker(
            "sess-fake",
            Some("job-fake"),
            &BackgroundIpcEndpoint {
                pid: std::process::id(),
                port: self.port,
                token: self.token.clone(),
            },
        )
    }

    fn requests(&self) -> Vec<BackgroundIpcRequest> {
        self.seen.lock().expect("poisoned").clone()
    }
}

impl Drop for FakeOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn ok() -> Answer {
    Answer::Reply(BackgroundIpcResponse::ok())
}

/// Deliberately unspawnable, for the reason `lib.rs`'s own fixture spells out:
/// a bare `rebon` resolves against the working directory, and a test binary
/// runs beside a real `rebon` build artifact. A path shape is rejected outright
/// by `resolve_supervisor_exe` instead.
fn rebon_exe() -> std::path::PathBuf {
    std::path::PathBuf::from("./__rebon-test-supervisor-must-not-spawn__")
}

/// A roster owned by this live process, so the supervisor wake-up on the queue
/// path short-circuits rather than trying to launch [`rebon_exe`].
fn seed_live_supervisor(store: &BackgroundStore) {
    store
        .write_roster(&crate::BackgroundRoster {
            supervisor_pid: std::process::id(),
            supervisor_pid_identity: crate::process_identity(std::process::id()),
            updated_at_ms: crate::now_ms(),
            jobs: Vec::new(),
        })
        .expect("the roster is written");
}

// ---------------------------------------------------------------------------
// Paired timeout, CancelCall, waiter release
// ---------------------------------------------------------------------------

/// A deadline that passes releases the waiter and tells the owner to stop.
///
/// The three assertions are the three halves that used to be missing: the call
/// comes back rather than hanging, it comes back as a value rather than as an
/// I/O message, and a `CancelCall` naming the same id actually goes out.
#[test]
fn a_call_that_times_out_sends_cancel_and_releases_its_waiter() {
    let owner = FakeOwner::serve(|request| match request {
        BackgroundIpcRequest::CancelCall { .. } => ok(),
        _ => Answer::Stall,
    });
    let connection = SessionHostConnection::new(owner.handle());

    let started = Instant::now();
    let result = connection.call_with_timeout(
        BackgroundIpcRequest::Reply {
            message: "hello".to_string(),
            images: Vec::new(),
        },
        Duration::from_millis(300),
    );

    assert_eq!(result, Err(HostCallError::HostUnanswered));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the call returned on its own deadline, not on the socket's"
    );
    assert_eq!(
        connection.calls().in_flight(),
        0,
        "the waiter is released on the timeout path too"
    );

    let requests = owner.requests();
    let cancelled: Vec<&String> = requests
        .iter()
        .filter_map(|request| match request {
            BackgroundIpcRequest::CancelCall { command_id } => Some(command_id),
            _ => None,
        })
        .collect();
    assert_eq!(cancelled.len(), 1, "exactly one cancel, for {requests:?}");
    assert!(
        cancelled[0].starts_with("cmd-"),
        "the cancel names a call id: {}",
        cancelled[0]
    );
}

/// An owner too old to know `CancelCall` refuses it, and the client carries on.
///
/// The failure this guards against is a client that treats compatibility as a
/// reason to keep waiting: the waiter is already gone by the time the cancel is
/// sent, so a refusal changes nothing the caller can see.
#[test]
fn an_owner_that_does_not_know_cancel_call_still_lets_the_client_go() {
    let owner = FakeOwner::serve(|request| match request {
        BackgroundIpcRequest::CancelCall { .. } => {
            Answer::Reply(BackgroundIpcResponse::failed("unknown request"))
        }
        _ => Answer::Stall,
    });
    let connection = SessionHostConnection::new(owner.handle());

    let started = Instant::now();
    let result =
        connection.call_with_timeout(BackgroundIpcRequest::Status, Duration::from_millis(300));

    assert_eq!(result, Err(HostCallError::HostUnanswered));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(connection.calls().in_flight(), 0);
    assert!(
        owner
            .requests()
            .iter()
            .any(|request| matches!(request, BackgroundIpcRequest::CancelCall { .. })),
        "the cancel was still attempted"
    );
}

/// Every call gets a new id, and the ids only ever go up.
///
/// A repeated id is how a discarded answer gets taken for the current one, and
/// how two clients that started in the same millisecond end up sharing an
/// owner's idempotency history.
#[test]
fn call_ids_are_unique_and_strictly_increasing() {
    let ids = CallIds::new();
    let first = ids.begin();
    let second = ids.begin();
    assert_ne!(first.wire, second.wire);
    assert!(second.sequence > first.sequence);
    assert_eq!(ids.in_flight(), 2);
    drop(first);
    assert_eq!(ids.in_flight(), 1);
    drop(second);
    assert_eq!(ids.in_flight(), 0);

    // Two allocators in one process — two surfaces — do not share a sequence,
    // so the salt is what keeps their ids apart.
    let other = CallIds::new();
    assert_ne!(ids.begin().wire, other.begin().wire);
}

/// A successful call releases its waiter too.
#[test]
fn a_call_that_is_answered_leaves_no_waiter_behind() {
    let owner = FakeOwner::serve(|_| ok());
    let connection = SessionHostConnection::new(owner.handle());
    assert_eq!(
        connection.call(BackgroundIpcRequest::Ping),
        Ok(HostReply { data: None })
    );
    assert_eq!(connection.calls().in_flight(), 0);
}

/// A refusal comes back as a value, not as a sentence to match on.
#[test]
fn a_refusal_is_typed_rather_than_a_string_to_parse() {
    let owner = FakeOwner::serve(|_| {
        Answer::Reply(BackgroundIpcResponse::failed(
            "background turn is no longer cancellable",
        ))
    });
    let connection = SessionHostConnection::new(owner.handle());
    assert_eq!(
        connection.call(BackgroundIpcRequest::Ping),
        Err(HostCallError::StaleGeneration)
    );

    // A sentence this does not recognise stays whole rather than being forced
    // into a category, because showing the owner's own words is the only right
    // answer when the category is unknown.
    let owner = FakeOwner::serve(|_| Answer::Reply(BackgroundIpcResponse::failed("disk is full")));
    let connection = SessionHostConnection::new(owner.handle());
    assert_eq!(
        connection.call(BackgroundIpcRequest::Ping),
        Err(HostCallError::Refused("disk is full".to_string()))
    );
}

/// A timeout is retryable with the same id; a fence failure is not.
#[test]
fn only_an_unanswered_call_is_worth_retrying() {
    assert!(HostCallError::HostUnanswered.is_retryable());
    assert!(HostCallError::Transport("reset".to_string()).is_retryable());
    assert!(!HostCallError::StaleGeneration.is_retryable());
    assert!(!HostCallError::Unauthenticated.is_retryable());
    assert!(!HostCallError::Cancelled.is_retryable());
}

/// `call` and `call_with_timeout` are the same call with the deadline named.
#[test]
fn the_default_budget_is_the_documented_one_per_request_kind() {
    assert_eq!(
        default_budget(&BackgroundIpcRequest::Ping),
        Duration::from_secs(2)
    );
    assert_eq!(
        default_budget(&BackgroundIpcRequest::SetSessionOption {
            key: "model".to_string(),
            value: "opus".to_string(),
        }),
        Duration::from_secs(30)
    );
    // A slash command runs on the owner's turn loop, so it gets the store's own
    // per-command budget rather than a round-trip one.
    assert!(
        default_budget(&BackgroundIpcRequest::RunCommand {
            name: "compact".to_string(),
            args: Vec::new(),
        }) > Duration::from_secs(30)
    );
}

// ---------------------------------------------------------------------------
// Permission fail-closed
// ---------------------------------------------------------------------------

fn option(option_id: &str, kind: &str) -> BackgroundPermissionOptionSnapshot {
    BackgroundPermissionOptionSnapshot {
        option_id: option_id.to_string(),
        label: option_id.to_string(),
        kind: kind.to_string(),
    }
}

fn query(options: Vec<BackgroundPermissionOptionSnapshot>) -> BackgroundPermissionQuerySnapshot {
    BackgroundPermissionQuerySnapshot {
        query_id: 5,
        turn_generation: 9,
        endpoint: None,
        tool: Some("Bash".to_string()),
        tool_call_id: None,
        session_id: Some("sess-fake".to_string()),
        title: None,
        message: None,
        tool_input: None,
        metadata: None,
        options,
    }
}

/// An option the owner offered is sent as it stands.
#[test]
fn an_offered_option_is_sent_unchanged() {
    let query = query(vec![
        option("allow-once", "allow_once"),
        option("reject-once", "reject_once"),
    ]);
    assert_eq!(
        SessionHostConnection::permission_answer_for(&query, Some("allow-once")),
        PermissionAnswer::Option("allow-once".to_string())
    );
}

/// An option the owner never offered becomes the owner's own `RejectOnce`.
///
/// Never an allow, never the choice this client made last time: the option came
/// from somewhere that is not this query — a stale render, a replaced prompt, a
/// client from another build — and the only safe reading of "I do not know what
/// you picked" is no.
#[test]
fn an_unknown_option_becomes_the_owners_reject_once() {
    let query = query(vec![
        option("allow-once", "allow_once"),
        option("reject-once", "reject_once"),
    ]);
    assert_eq!(
        SessionHostConnection::permission_answer_for(&query, Some("allow-always")),
        PermissionAnswer::Option("reject-once".to_string())
    );
}

/// With no `RejectOnce` to fall back to, the answer is a cancellation.
#[test]
fn an_unknown_option_with_nothing_to_reject_with_cancels() {
    let query = query(vec![option("allow-once", "allow_once")]);
    assert_eq!(
        SessionHostConnection::permission_answer_for(&query, Some("allow-always")),
        PermissionAnswer::Cancelled
    );
    // And a client that sends no option at all is cancelling on purpose.
    assert_eq!(
        SessionHostConnection::permission_answer_for(&query, None),
        PermissionAnswer::Cancelled
    );
}

/// An answer aimed at a prompt the owner has moved past is a race somebody else
/// won, not a failure to retry.
#[test]
fn answering_a_prompt_the_owner_moved_past_is_not_an_error() {
    let owner = FakeOwner::serve(|request| match request {
        BackgroundIpcRequest::Status => {
            Answer::Reply(BackgroundIpcResponse::with_data(&status(None)))
        }
        _ => ok(),
    });
    let connection = SessionHostConnection::new(owner.handle());
    assert_eq!(
        connection.answer_permission(5, Some("allow-once"), None),
        Ok(PermissionOutcome::AlreadyResolved)
    );
    assert!(
        !owner
            .requests()
            .iter()
            .any(|request| matches!(request, BackgroundIpcRequest::PermissionAnswer { .. })),
        "nothing was sent at the new turn"
    );
}

/// The answer carries the generation the owner is parked on, read from the
/// owner rather than remembered by the caller.
#[test]
fn an_answer_carries_the_generation_the_owner_is_parked_on() {
    let pending = query(vec![
        option("allow-once", "allow_once"),
        option("reject-once", "reject_once"),
    ]);
    let owner = FakeOwner::serve(move |request| match request {
        BackgroundIpcRequest::Status => Answer::Reply(BackgroundIpcResponse::with_data(&status(
            Some(query(vec![
                option("allow-once", "allow_once"),
                option("reject-once", "reject_once"),
            ])),
        ))),
        _ => ok(),
    });
    let connection = SessionHostConnection::new(owner.handle());

    assert_eq!(
        connection.answer_permission(5, Some("nonsense"), None),
        Ok(PermissionOutcome::Applied(PermissionAnswer::Option(
            "reject-once".to_string()
        )))
    );

    let sent = owner
        .requests()
        .into_iter()
        .find_map(|request| match request {
            BackgroundIpcRequest::PermissionAnswer {
                query_id,
                turn_generation,
                option_id,
                ..
            } => Some((query_id, turn_generation, option_id)),
            _ => None,
        })
        .expect("an answer was sent");
    assert_eq!(
        sent,
        (5, pending.turn_generation, Some("reject-once".to_string()))
    );
    assert_eq!(
        connection.last_turn_generation(),
        9,
        "the connection remembers the generation it last saw"
    );
}

fn status(pending: Option<BackgroundPermissionQuerySnapshot>) -> SessionStatusSnapshot {
    SessionStatusSnapshot {
        job_id: "job-fake".to_string(),
        session_id: Some("sess-fake".to_string()),
        cwd: "/work".to_string(),
        status: BackgroundJobStatus::Running,
        busy: true,
        turn_generation: 9,
        permission_mode: None,
        plan_mode: false,
        model: None,
        effort: None,
        agent: None,
        pending_permission: pending,
        ask_user_questions: None,
        usage: None,
        mcp: None,
        client_leases: Vec::new(),
        last_command_id: None,
        last_command_at_ms: 0,
        last_command_error: None,
        updated_at_ms: 1,
    }
}

// ---------------------------------------------------------------------------
// Cursor, lease and close
// ---------------------------------------------------------------------------

/// The cursor only moves forward, so an out-of-order observation cannot make a
/// reconnect replay events it has already rendered.
#[test]
fn the_cursor_only_moves_forward() {
    let owner = FakeOwner::serve(|_| ok());
    let connection = SessionHostConnection::new(owner.handle());
    assert_eq!(connection.last_cursor(), None);
    connection.observe_cursor(7);
    connection.observe_cursor(3);
    assert_eq!(connection.last_cursor(), Some(7));
}

/// Closing ends the subscription and gives the lease up, and does it once.
#[test]
fn closing_releases_the_lease_and_is_idempotent() {
    let owner = FakeOwner::serve(|_| ok());
    let connection = SessionHostConnection::new(owner.handle());
    connection.hold_lease("client-1", crate::ClientLeaseKind::Serve);
    assert!(connection.holds_lease());

    connection.close();
    assert!(!connection.holds_lease(), "the lease went with the close");
    assert!(connection.is_closed());
    connection.close();
    assert!(!connection.holds_lease());

    assert!(
        owner
            .requests()
            .iter()
            .any(|request| matches!(request, BackgroundIpcRequest::ReleaseLease { .. })),
        "the owner was told, rather than left to wait out the TTL"
    );
}

/// Dropping the connection takes the same path as closing it, so an endpoint
/// that was torn down leaves the owner in the state a clean exit would.
#[test]
fn dropping_the_connection_releases_the_lease_too() {
    let owner = FakeOwner::serve(|_| ok());
    {
        let connection = SessionHostConnection::new(owner.handle());
        connection.hold_lease("client-1", crate::ClientLeaseKind::Tui);
    }
    assert!(
        owner
            .requests()
            .iter()
            .any(|request| matches!(request, BackgroundIpcRequest::ReleaseLease { .. })),
        "a dropped connection still says goodbye"
    );
}

// ---------------------------------------------------------------------------
// Owner routing
// ---------------------------------------------------------------------------

/// An unreachable owner is refused and nothing is written.
///
/// It is alive — the lock says so — so appending a pending prompt would queue
/// work for a worker that is already running and will never claim it.
#[test]
fn a_prompt_for_an_unreachable_owner_is_refused_without_a_write() {
    let dir = tempfile::Builder::new()
        .prefix("rebon-client-unreachable-")
        .tempdir()
        .expect("tempdir");
    let store = BackgroundStore::new(dir.path().join("store"));
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects root");
    let cwd = "/work/unreachable";

    let _lock = rebon_session::try_acquire_session_active_lock(&projects, cwd, "sess-unreachable")
        .expect("probe")
        .expect("free");
    // A descriptor naming a port nothing listens on: alive, published, and not
    // answering.
    rebon_session::write_session_owner(
        &projects,
        cwd,
        "sess-unreachable",
        &rebon_session::SessionOwnerDescriptor {
            version: rebon_session::SESSION_OWNER_VERSION,
            pid: std::process::id(),
            pid_identity: None,
            surface: rebon_session::SessionOwnerSurface::Worker,
            job_id: Some("job-unreachable".to_string()),
            ipc_port: Some(dead_port()),
            ipc_token: Some("token".to_string()),
            started_at_ms: 1,
        },
    )
    .expect("descriptor");

    let client = SessionHostClient::new(store, &projects, rebon_exe());
    assert_eq!(
        client.send_prompt(
            cwd,
            "sess-unreachable",
            Some("job-unreachable"),
            "hello".to_string(),
            Vec::new(),
        ),
        Err(HostCallError::HostUnanswered)
    );
}

/// A port that was bound and released, so connecting to it fails fast rather
/// than waiting out a firewall.
fn dead_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
    let port = listener.local_addr().expect("bound").port();
    drop(listener);
    port
}

/// A free session with a job record gets the prompt queued durably — the one
/// write a client is allowed (invariant I8).
#[test]
fn a_prompt_for_a_session_with_no_owner_is_queued_for_the_worker() {
    let dir = tempfile::Builder::new()
        .prefix("rebon-client-queue-")
        .tempdir()
        .expect("tempdir");
    let store = BackgroundStore::new(dir.path().join("store"));
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects root");

    let mut state = crate::BackgroundJobState::new(
        "first".to_string(),
        "/work/queue".to_string(),
        crate::BackgroundRuntimeFields {
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
        None,
    );
    state.identity.session_id = Some("sess-queue".to_string());
    state.process.status = BackgroundJobStatus::Idle;
    let job_id = state.identity.job_id.clone();
    store
        .write_state(&state)
        .expect("the job record is written");
    seed_live_supervisor(&store);

    let client = SessionHostClient::new(store.clone(), &projects, rebon_exe());
    assert_eq!(
        client.send_prompt(
            "/work/queue",
            "sess-queue",
            Some(&job_id),
            "and then this".to_string(),
            Vec::new(),
        ),
        Ok(PromptDelivery::Queued)
    );

    let reloaded = store.read_state(&job_id).expect("the job is still there");
    assert_eq!(
        reloaded
            .identity
            .pending_prompts
            .iter()
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>(),
        vec!["and then this"],
        "the prompt is where the worker will look for it"
    );
}
