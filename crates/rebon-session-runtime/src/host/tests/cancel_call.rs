//! The server half of `CancelCall` (design §5.3 and §12.2 items 2, 3, 6).
//!
//! The client half is tested against a fake owner in
//! `rebon-session-host`. What is proven here is the other end: that a real
//! server registers a call it is working on, that a cancel arriving on a second
//! connection actually reaches the waiter, that the registry is empty again
//! afterwards, and that cancelling does not throw away the idempotency history
//! a retry depends on.

use super::super::*;
use super::support::*;

/// A job with a live IPC server, and a client that can address it.
fn hosted_job(
    session_id: &str,
) -> (
    tempfile::TempDir,
    BackgroundStore,
    BackgroundJobState,
    BackgroundIpcServer,
) {
    let (dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some(session_id.into());
    let ipc = start_background_ipc_server(&store, state.job_id()).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    (dir, store, state, ipc)
}

fn send(
    ipc: &BackgroundIpcServer,
    state: &BackgroundJobState,
    request: rebon_session_host::BackgroundIpcRequest,
    command_id: Option<String>,
) -> anyhow::Result<rebon_session_host::BackgroundIpcResponse> {
    rebon_session_host::OwnerHandle::for_worker(
        state.session_id().unwrap_or_default(),
        Some(state.job_id()),
        &ipc.owner().endpoint,
    )
    .send(request, command_id)
}

fn wait_until(deadline_ms: u64, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(deadline_ms);
    while std::time::Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ready()
}

/// A call the owner is working on is registered under the id that asked for it,
/// and a cancel on another connection releases it.
///
/// The waiter used to be a bare `recv_timeout`: a client that gave up left the
/// owner working for the whole ceiling — minutes, for `/compact` — on an answer
/// nobody would read.
#[test]
fn a_cancel_releases_the_waiter_the_owner_is_holding() {
    let (_dir, _store, state, ipc) = hosted_job("sess-cancel");
    assert_eq!(ipc.calls_in_flight(), 0, "nothing in flight to begin with");

    // A command nobody will answer: the turn loop is never drained here, so
    // the server sits in the waiter until it is cancelled.
    let client_state = state.clone();
    let client_ipc = ipc.owner().endpoint.clone();
    let caller = std::thread::spawn(move || {
        rebon_session_host::OwnerHandle::for_worker(
            "sess-cancel",
            Some(client_state.job_id()),
            &client_ipc,
        )
        .run_command_with_id("hooks", Vec::new(), "cmd-parked".into())
    });

    assert!(
        wait_until(2_000, || ipc.calls_in_flight() == 1),
        "the call the owner is working on is registered"
    );

    let response = send(
        &ipc,
        &state,
        rebon_session_host::BackgroundIpcRequest::CancelCall {
            command_id: "cmd-parked".into(),
        },
        None,
    )
    .expect("the owner answers a cancel");
    assert_eq!(
        response.data.as_ref().and_then(|d| d.get("released")),
        Some(&serde_json::json!(true)),
        "the cancel found the call it named"
    );

    let answer = caller.join().expect("the caller thread finished");
    let error = answer.expect_err("a cancelled command is not a success");
    assert!(
        error.to_string().contains("cancelled"),
        "the caller is told why: {error}"
    );
    assert!(
        wait_until(2_000, || ipc.calls_in_flight() == 0),
        "the registry is empty again"
    );
}

/// Cancelling a call nobody is running is not an error.
///
/// "There was nothing to cancel" is the state the caller wanted. Answering with
/// a refusal would read to a client as an owner too old to know the request,
/// which is the one thing it must not be confused with — that is the signal the
/// client uses to stop expecting cancellation to work at all.
#[test]
fn cancelling_a_call_that_is_not_running_is_still_an_answer() {
    let (_dir, _store, state, ipc) = hosted_job("sess-cancel-none");
    let response = send(
        &ipc,
        &state,
        rebon_session_host::BackgroundIpcRequest::CancelCall {
            command_id: "cmd-never-existed".into(),
        },
        None,
    )
    .expect("the owner answers");
    assert!(response.ok);
    assert_eq!(
        response.data.as_ref().and_then(|d| d.get("released")),
        Some(&serde_json::json!(false)),
        "it says plainly that nothing was waiting"
    );
}

/// A cancel does not erase the answer a retry is entitled to.
///
/// Design §5.3 item 4: work that reached a point it cannot be rolled back from
/// still finishes, and the retry carrying that id gets that one result rather
/// than running the command a second time. Cancelling clears the *waiter*, not
/// the history.
#[test]
fn a_cancel_leaves_the_idempotency_history_alone() {
    let (_dir, _store, state, ipc) = hosted_job("sess-cancel-idem");

    // One command that actually completes, remembered under its id.
    let client_state = state.clone();
    let endpoint = ipc.owner().endpoint.clone();
    let caller = std::thread::spawn(move || {
        rebon_session_host::OwnerHandle::for_worker(
            "sess-cancel-idem",
            Some(client_state.job_id()),
            &endpoint,
        )
        .run_command_with_id("hooks", Vec::new(), "cmd-done".into())
    });
    assert!(
        wait_until(2_000, || {
            ipc.drain_commands(|_, _| {
                Ok(rebon_session_host::CommandOutput {
                    text: "ran once".into(),
                    tone: "info".into(),
                })
            }) == 1
        }),
        "the command reached the turn loop"
    );
    caller
        .join()
        .expect("thread")
        .expect("the command answered");
    assert_eq!(ipc.calls_in_flight(), 0);

    // Cancelling that id now finds nothing in flight...
    send(
        &ipc,
        &state,
        rebon_session_host::BackgroundIpcRequest::CancelCall {
            command_id: "cmd-done".into(),
        },
        None,
    )
    .expect("the owner answers");

    // ...and the retry still gets the remembered answer, without the turn loop
    // being asked to run anything a second time.
    let replay = rebon_session_host::OwnerHandle::for_worker(
        "sess-cancel-idem",
        Some(state.job_id()),
        &ipc.owner().endpoint,
    )
    .run_command_with_id("hooks", Vec::new(), "cmd-done".into())
    .expect("the retry is answered from memory");
    let body = serde_json::to_string(&replay).unwrap();
    assert!(
        body.contains("ran once"),
        "the retry replayed the first answer: {body}"
    );
    assert_eq!(
        ipc.drain_commands(|_, _| Ok(rebon_session_host::CommandOutput {
            text: "ran twice".into(),
            tone: "info".into(),
        })),
        0,
        "the turn loop was never asked to run it again"
    );
}

/// Shutdown releases every waiter, by the same path a single cancel takes.
///
/// Design §5.3 item 6. A client parked on a command should learn the same way
/// whether it gave up or the worker did, rather than discovering that the
/// endpoint simply stopped answering.
#[test]
fn shutting_the_host_down_releases_every_call_in_flight() {
    let (_dir, _store, state, ipc) = hosted_job("sess-cancel-shutdown");

    let client_state = state.clone();
    let endpoint = ipc.owner().endpoint.clone();
    let caller = std::thread::spawn(move || {
        rebon_session_host::OwnerHandle::for_worker(
            "sess-cancel-shutdown",
            Some(client_state.job_id()),
            &endpoint,
        )
        .run_command_with_id("hooks", Vec::new(), "cmd-at-shutdown".into())
    });
    assert!(
        wait_until(2_000, || ipc.calls_in_flight() == 1),
        "the call is registered"
    );

    assert_eq!(
        ipc.cancel_calls_in_flight(),
        1,
        "shutdown reports what it released"
    );
    let error = caller
        .join()
        .expect("thread")
        .expect_err("the parked call did not succeed");
    assert!(
        error.to_string().contains("cancelled"),
        "the caller is told, not left to time out: {error}"
    );
    assert_eq!(ipc.calls_in_flight(), 0);
    assert_eq!(
        ipc.cancel_calls_in_flight(),
        0,
        "a second shutdown has nothing left to do"
    );
}

/// A call that carries no `command_id` is not registered.
///
/// It could not be cancelled by name and could not be retried idempotently
/// either; those are the same property, and pretending otherwise would put an
/// entry in the map that nothing could ever remove.
#[test]
fn a_call_without_an_id_is_not_registered() {
    let (_dir, _store, state, ipc) = hosted_job("sess-cancel-anon");
    send(
        &ipc,
        &state,
        rebon_session_host::BackgroundIpcRequest::Ping,
        None,
    )
    .expect("ping is answered");
    assert_eq!(ipc.calls_in_flight(), 0);
}

/// The order inside one host: waiters are released before the transports they
/// were waiting on are closed.
///
/// This is design §12.2 item 9 at the level a test can
/// watch. The wider claim those make — every host closed before the process
/// plugin plane — is structural: every host lives inside `route_main`, and
/// `async_main` closes the plane only after it returns. What is not structural,
/// and is what this pins, is the order inside one host. Backwards, a parked
/// caller would be woken by a transport that had already gone.
///
/// The observation is the in-flight count read at the moment the transport is
/// asked to close, rather than a race between two threads' log writes: if the
/// release had not happened yet, the count would still be 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_releases_its_waiters_before_it_closes_its_transports() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let (_dir, store, state, ipc) = hosted_job("sess-exit-order");
    // Shared so the probe can read the in-flight count from inside the close.
    let ipc = Arc::new(ipc);

    /// Reads how many calls are still parked, at the moment it is closed.
    struct ClosingProbe {
        in_flight_at_close: Arc<AtomicUsize>,
        read: Arc<dyn Fn() -> usize + Send + Sync>,
    }

    #[async_trait::async_trait]
    impl rebon_tool::McpClient for ClosingProbe {
        async fn call_tool(
            &self,
            _: rebon_tool::McpToolCall,
        ) -> Result<rebon_tool::McpToolResult, rebon_tool::McpClientError> {
            unreachable!("the exit-order test never calls a tool")
        }
        async fn shutdown_transport(&self) {
            self.in_flight_at_close
                .store((self.read)(), Ordering::SeqCst);
        }
    }

    let in_flight_at_close = Arc::new(AtomicUsize::new(usize::MAX));
    let reader = {
        let ipc = Arc::clone(&ipc);
        Arc::new(move || ipc.calls_in_flight()) as Arc<dyn Fn() -> usize + Send + Sync>
    };
    let mcp = crate::mcp::SessionMcp::with_probe_client(Arc::new(ClosingProbe {
        in_flight_at_close: in_flight_at_close.clone(),
        read: reader,
    }));

    let endpoint = ipc.owner().endpoint.clone();
    let job_id = state.job_id().to_string();
    let caller = std::thread::spawn(move || {
        rebon_session_host::OwnerHandle::for_worker("sess-exit-order", Some(&job_id), &endpoint)
            .run_command_with_id("hooks", Vec::new(), "cmd-at-exit".into())
    });
    assert!(
        wait_until(2_000, || ipc.calls_in_flight() == 1),
        "the call is registered before the host starts closing"
    );

    super::super::worker::execution::close_host_resources(&ipc, Some(mcp), &store, state.job_id())
        .await;

    assert_eq!(
        in_flight_at_close.load(Ordering::SeqCst),
        0,
        "the transport was closed while a call was still parked on the host"
    );
    caller
        .join()
        .expect("thread")
        .expect_err("a released call does not succeed");
}
