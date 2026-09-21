//! One connection, spoken as ACP.
//!
//! [`super::wire_probe`] decided that this connection is JSON-RPC rather than a
//! legacy envelope; [`super::acp_gate`] decides what it is allowed to do. This
//! is the loop between them: read frames, route methods, write answers, and
//! stop when the peer goes away or authentication fails.
//!
//! What is here is the ACP skeleton — `initialize`, `_session/ping`,
//! `_session/status` — plus the rest of the `_session/*` methods the control
//! plane answers. `_session/subscribe` is the one still declared (in
//! `session_ext::method`) and answered `-32601`, because it takes over the
//! connection rather than answering on it. The skeleton went first on purpose:
//! it is the part that can be wrong in ways tests do not catch.
//!
//! **Notifications are dropped, not answered.** JSON-RPC says a notification
//! has no reply, and answering one would put a response with a null id on a
//! wire where the peer is not reading for it.

use std::io::BufRead;
use std::net::TcpStream;

use rebon_proto::framing::{FrameDecodeStep, FrameDecoder, FramingMode};
use rebon_proto::types::{
    AgentCapabilities, InitializeResult, JsonRpcError, JsonRpcMessage, JsonRpcRequest,
    JsonRpcResponse, JsonRpcVersion,
};
use rebon_session_host::session_ext::{self, method, RebonMeta};
use rebon_session_host::BackgroundIpcRequest;

use super::acp_gate::{AcpGate, Admission};
use super::acp_permission::permission_params;
use super::acp_stream::{outbound_for, Outbound, Wire};
use super::server::{
    execute_background_request, restated_pending_permission, session_command_for_request,
    session_status_snapshot, RequestContext, WireResponse,
};
use rebon_session_host::wire_errors;

/// The protocol version this control plane answers with.
///
/// The same number `rebon-acp` advertises. One wire, one version: a worker
/// that claimed a different one would be telling a client it is a different
/// kind of agent.
pub const ACP_PROTOCOL_VERSION: i32 = 1;

/// Serve one connection until the peer hangs up or the gate closes it.
///
/// `reader` owns the read half because the probe may have buffered past the
/// first frame; `writer` is a separate handle on the same socket, so answers
/// can be written without borrowing the reader.
pub(crate) fn serve_acp_connection(
    mut reader: impl BufRead,
    writer: TcpStream,
    framing: FramingMode,
    first_body: Vec<u8>,
    token: &str,
    context: &RequestContext<'_>,
) {
    // From here the socket's write half has exactly one owner. Everything that
    // wants to say something -- this loop's answers, and the event forwarder a
    // subscription starts -- queues a framed message for it. Two threads each
    // holding a `try_clone`d handle would tear a frame in half, and a torn
    // frame is not something a peer resynchronises from.
    let wire = match Wire::spawn(writer, framing) {
        Ok(wire) => std::sync::Arc::new(wire),
        Err(error) => {
            tracing::warn!(%error, "could not start ACP connection writer");
            return;
        }
    };
    struct Cleanup<'a, 'b>(&'a std::sync::Arc<Wire>, &'a RequestContext<'b>);
    impl Drop for Cleanup<'_, '_> {
        fn drop(&mut self) {
            self.1
                .recent_command_results
                .lock()
                .expect("poisoned")
                .prompts
                .disconnect(self.0);
            self.0.forget_stream();
            if let Some(id) = self.0.subscription.lock().expect("poisoned").take() {
                self.1.events.unsubscribe(id);
            }
        }
    }
    let _cleanup = Cleanup(&wire, context);
    let mut gate = AcpGate::new(token);
    let mut decoder = FrameDecoder::with_framing(framing);
    // The probe consumed the first NDJSON line to classify it. Feeding it back
    // in is what makes the probe non-destructive: the loop below sees the
    // stream as though nothing had read it.
    if !first_body.is_empty() {
        decoder.push(&first_body);
    }

    loop {
        while let FrameDecodeStep::Message(body) = decoder.next_message() {
            if !handle_one(&body, &mut gate, &wire, context) {
                return;
            }
        }
        let filled = match reader.fill_buf() {
            Ok(bytes) if bytes.is_empty() => return,
            Ok(bytes) => bytes.to_vec(),
            // A peer that vanished is not an error worth a log line: half the
            // clients of this port are terminals that close when a user does.
            Err(_) => return,
        };
        reader.consume(filled.len());
        decoder.push(&filled);
    }
}

/// Handle one decoded frame. Returns whether the connection survives it.
fn handle_one(
    body: &[u8],
    gate: &mut AcpGate,
    wire: &std::sync::Arc<Wire>,
    context: &RequestContext<'_>,
) -> bool {
    let request = match JsonRpcMessage::from_bytes(body) {
        Ok(JsonRpcMessage::Request(request)) => request,
        // A response is the client answering something *this* side asked --
        // the only such question is a permission, and the answer is merged by
        // the query it names rather than by which connection carried it.
        Ok(JsonRpcMessage::Response(response)) => {
            if gate.is_ready() {
                answer_permission(&response, wire, context);
            }
            return true;
        }
        Ok(JsonRpcMessage::Notification(notification)) => {
            // A notification is run and not answered: JSON-RPC gives it no id,
            // so there is nowhere to put either a result or an error. That is
            // also why it is dropped when the connection is not ready -- a
            // refusal the peer cannot receive is not a refusal, and
            // `session/cancel` is the only notification this side routes.
            if gate.is_ready() {
                let _ = run_notification(&notification, context);
            }
            return true;
        }
        Err(error) => {
            // No id to answer against, so this is the one case that writes an
            // error with a null id — which is exactly what JSON-RPC says to do
            // when a parse failure means the id could not be read.
            return wire.send_response(&JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: None,
                result: None,
                error: Some(JsonRpcError::parse_error(error.to_string())),
            });
        }
    };

    let meta = request
        .params
        .as_ref()
        .and_then(|params| params.get("_meta"));
    match gate.admit(&request.method, meta) {
        Admission::Initialized => answer(wire, &request, Ok(initialize_result())),
        Admission::Reject(error) => answer(wire, &request, Err(error)),
        Admission::RejectAndClose(error) => {
            answer(wire, &request, Err(error));
            false
        }
        // The one request that does not simply answer: it answers *and* then
        // keeps sending, for as long as the session runs.
        Admission::Dispatch if request.method == method::SUBSCRIBE => {
            subscribe(&request, wire, context)
        }
        Admission::Dispatch => match dispatch(&request, wire, context) {
            Ok(None) => true,
            Ok(Some(result)) => answer(wire, &request, Ok(result)),
            Err(error) => answer(wire, &request, Err(error)),
        },
    }
}

/// Open the event stream on this connection.
///
/// The request is answered first, so a client knows it is attached before the
/// first event arrives, and only then does anything start flowing. Then the
/// same three things the legacy subscriber gets, in the same order and for the
/// same reason: `hello` carries the snapshot a client must agree with before
/// it can interpret a single delta, the replay carries what it missed, and a
/// permission still pending is restated because a fresh subscriber is given no
/// replay and would otherwise never hear a question that is blocking a tool.
fn subscribe(request: &JsonRpcRequest, wire: &Wire, context: &RequestContext<'_>) -> bool {
    let params: session_ext::SubscribeParams =
        match decode(&request.method, request.params.as_ref()) {
            Ok(params) => params,
            Err(error) => return answer(wire, request, Err(error)),
        };
    match start_stream(wire, context, params.since, Some(request)) {
        Ok(()) => true,
        Err(error) => answer(wire, request, Err(error)),
    }
}

/// Standard prompt clients receive updates and permission calls without having
/// to discover a private subscribe extension. There is one stream per socket.
fn start_stream(
    wire: &Wire,
    context: &RequestContext<'_>,
    since: Option<u64>,
    request: Option<&JsonRpcRequest>,
) -> Result<(), JsonRpcError> {
    if request.is_none() && wire.subscription.lock().expect("poisoned").is_some() {
        return Ok(());
    }
    let state = context
        .store
        .read_state(context.job_id)
        .map_err(|error| JsonRpcError::internal_error(error.to_string()))?;
    let session_id = state.identity.session_id.clone().unwrap_or_default();
    if let Some(old) = wire.subscription.lock().expect("poisoned").take() {
        context.events.unsubscribe(old);
    }
    let subscription = context.events.subscribe(since);
    *wire.subscription.lock().expect("poisoned") = Some(subscription.id);
    if !wire.begin_stream(subscription.id, subscription.cursor) {
        return Err(JsonRpcError::internal_error(
            "previous prompt updates were not delivered",
        ));
    }
    if let Some(request) = request {
        wire.extensions
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = session_status_snapshot(
            &state,
            context.live_permission_mode,
            context.live_mcp_status,
            context.live_agent,
            context.recent_command_results,
        );
        answer(wire, request, Ok(serde_json::json!({})));
        forward(
            wire,
            &session_id,
            rebon_session_host::SessionEvent::Hello {
                cursor: subscription.cursor,
                turn_generation: snapshot.turn_generation,
                status: Box::new(snapshot),
                epoch: context.events.epoch(),
            },
        );
    }
    for line in &subscription.replay {
        let event = serde_json::from_str::<rebon_session_host::SessionEvent>(line)
            .expect("event stream serializes SessionEvent");
        if !forward_stream_event(wire, subscription.id, &session_id, event) {
            wire.close();
            return Err(JsonRpcError::internal_error("prompt event replay failed"));
        }
    }
    if let Some(event) =
        restated_pending_permission(&state, &subscription.replay, subscription.cursor)
    {
        forward(wire, &session_id, event);
    }

    // Clone the inner Wire, not the reader's Arc used by deferred reply sinks.
    let forwarder = wire.clone();
    let events = subscription.events;
    let id = subscription.id;
    let from_cursor = subscription.cursor;
    let ring_cursor = context.events.clone();
    std::thread::Builder::new()
        .spawn(move || {
            forward_live_stream(
                &forwarder,
                &session_id,
                events,
                id,
                from_cursor,
                &ring_cursor,
            );
        })
        .map_err(|error| {
            wire.close();
            JsonRpcError::internal_error(format!("could not start ACP event forwarder: {error}"))
        })?;
    Ok(())
}

pub(super) fn forward_live_stream(
    wire: &Wire,
    session_id: &str,
    events: std::sync::mpsc::Receiver<String>,
    id: u64,
    from_cursor: u64,
    ring: &super::events::SessionEventStream,
) {
    while let Ok(line) = events.recv() {
        let event = serde_json::from_str::<rebon_session_host::SessionEvent>(&line)
            .expect("event stream serializes SessionEvent");
        if !forward_stream_event(wire, id, session_id, event) {
            break;
        }
    }
    // Disconnect/re-subscribe unsubscribes explicitly; neither is a gap.
    let mut active = wire.subscription.lock().expect("poisoned");
    if *active == Some(id) {
        *active = None;
        drop(active);
        forward(
            wire,
            session_id,
            rebon_session_host::SessionEvent::Gap {
                from: from_cursor,
                to: ring.cursor(),
            },
        );
        // A live gap can never satisfy an update watermark. Closing also
        // wakes the read loop, which releases every pending reply sink.
        wire.close();
    }
    ring.unsubscribe(id);
}

// Kept at IPC visibility so socket tests can hold the actual event receiver
// and deterministically exercise a delayed forwarder without scheduling hooks.
pub(super) fn forward_stream_event(
    wire: &Wire,
    subscription: u64,
    session_id: &str,
    event: rebon_session_host::SessionEvent,
) -> bool {
    let cursor = event.cursor();
    forward(wire, session_id, event)
        && cursor.is_none_or(|cursor| wire.forwarded(subscription, cursor))
}

/// Put one session event on the wire in the shape ACP gives it.
fn forward(wire: &Wire, session_id: &str, event: rebon_session_host::SessionEvent) -> bool {
    match outbound_for(event) {
        Some(Outbound::Notify { method, params }) => {
            if method != "session/update"
                && !wire.extensions.load(std::sync::atomic::Ordering::Relaxed)
            {
                return true;
            }
            wire.send_notification(&method, params)
        }
        Some(Outbound::AskPermission {
            asked,
            cursor,
            query,
        }) => wire.ask_permission(
            session_id,
            asked,
            cursor,
            permission_params(session_id, &query),
        ),
        None => true,
    }
}

/// Merge the client's answer to a permission this side asked about.
///
/// A response naming an id this connection never issued is ignored: it is the
/// client answering something it invented, and acting on it would let any
/// response shape a permission decision.
fn answer_permission(response: &JsonRpcResponse, wire: &Wire, context: &RequestContext<'_>) {
    let Some(id) = response.id.as_ref() else {
        return;
    };
    let Some(asked) = wire.answered_query(id) else {
        return;
    };
    let meta = RebonMeta::from_meta(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("_meta")),
    );
    // A refusal to answer is a cancellation of the question, which is what an
    // `error` on this response means and what the outcome field spells when
    // the client says it outright.
    let outcome = response
        .result
        .as_ref()
        .and_then(|result| result.get("outcome"));
    let option_id = outcome
        .and_then(|outcome| outcome.get("optionId"))
        .and_then(|option| option.as_str())
        .map(|option| option.to_string());
    let updated_input = outcome
        .and_then(|outcome| outcome.get("updatedInput"))
        .cloned()
        .or_else(|| {
            response
                .result
                .as_ref()
                .and_then(|result| result.get("updatedInput"))
                .cloned()
        });
    // A late answer to a query the owner already decided is accepted and does
    // nothing, which is what the legacy path does: the client is told its
    // answer arrived, and the first one still wins.
    execute_background_request(
        BackgroundIpcRequest::PermissionAnswer {
            query_id: asked.query_id,
            // The turn this side asked about, not one the client echoed back.
            turn_generation: asked.turn_generation,
            option_id,
            extra_text: meta.extra_text,
            updated_input,
        },
        None,
        context,
    );
}

/// Route one authenticated request to what answers it.
///
/// Every `_session/*` method is translated into the `BackgroundIpcRequest` it
/// has always been and run through `execute_background_request`, the same
/// function the legacy envelope path calls. Nothing about what a method *does*
/// is written twice: a compatibility period in which each protocol had its own
/// copy of `_session/rewind` would be a period in which the answer depended on
/// which client you happened to use.
fn dispatch(
    request: &JsonRpcRequest,
    wire: &std::sync::Arc<Wire>,
    context: &RequestContext<'_>,
) -> Result<Option<serde_json::Value>, JsonRpcError> {
    let params = request.params.as_ref();
    let meta = RebonMeta::from_meta(params.and_then(|params| params.get("_meta")));
    let ipc = translate(&request.method, params)?;

    // The job and session fences, per request, through the same ladder the
    // legacy path runs. Not authentication -- that happened once, at
    // `initialize` -- but "did you mean this worker", which any client can get
    // wrong on any single message after a worker was replaced.
    // The job is *not* defaulted to this worker. An earlier version did, and it
    // quietly disabled the check below it: a request that names a session and
    // no job is routed by that session, and pretending it had named this job
    // made "is that session even open here" unreachable. What the connection
    // stands in for is narrower -- only the requirement to name *something* --
    // and that is what `must_name_a_target: false` says.
    let addressed_job = meta.job_id.as_deref();
    // A plain ACP client names the session in the standard field rather than
    // in `_meta`. Reading it here is what makes that client's `sessionId`
    // actually fence instead of being decoration.
    let standard_session = standard_session_id(&request.method, params);
    let addressed_session = meta.session_id.as_deref().or(standard_session.as_deref());
    if let Some((kind, sentence)) =
        context.refusal_for(addressed_job, addressed_session, &ipc, false, || true)
    {
        // The kind comes from the fence itself. Reading it back out of the
        // sentence would throw away what this side already knows, and
        // `HostCallError::from_wire` is a reader for owners too old to say it
        // any other way -- not for this one.
        let mut error = wire_errors::to_json_rpc(&kind);
        error.message = sentence;
        return Err(error);
    }

    // A retry after a lost connection is answered from memory rather than run
    // again -- from the same memory the legacy path writes, so the guarantee
    // holds across protocols rather than within each one.
    let command_id = meta.command_id.as_deref();
    if request.method == SESSION_PROMPT {
        let BackgroundIpcRequest::Reply { message, images } = ipc else {
            unreachable!()
        };
        let prompt = rebon_session_host::PendingPrompt::new(
            rebon_session_host::generate_pending_prompt_id(),
            message,
            images,
            rebon_session_host::now_ms(),
        )
        .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
        start_stream(wire, context, None, None)?;
        super::acp_prompt::enqueue(context, wire, request.id.clone(), command_id, prompt)?;
        return Ok(None);
    }
    let carries_command_output = session_command_for_request(&ipc).is_some()
        || matches!(ipc, BackgroundIpcRequest::RunCommand { .. });
    if let Some(answer) = context.remembered_answer(command_id) {
        return result_for(&request.method, carries_command_output, &answer).map(Some);
    }

    let answer = execute_background_request(ipc, command_id, context);
    context.record_answer(command_id, &answer);
    result_for(&request.method, carries_command_output, &answer).map(Some)
}

/// Run a notification. Nothing is written back, whatever happens.
///
/// `session/cancel` is the only one: it trips the running turn's cancel, and
/// the fence it carries in `_meta.rebon.fence` is what stops a cancel meant
/// for a finished turn from landing on the one that replaced it. Standard
/// `session/cancel` is a bare notification with nowhere to put that, which is
/// why the fence rides in `_meta`.
fn run_notification(
    notification: &rebon_proto::types::JsonRpcNotification,
    context: &RequestContext<'_>,
) -> Result<(), JsonRpcError> {
    if notification.method != SESSION_CANCEL {
        return Ok(());
    }
    let params = notification.params.as_ref();
    let meta = RebonMeta::from_meta(params.and_then(|params| params.get("_meta")));
    let standard: rebon_proto::types::SessionCancelParams = decode(SESSION_CANCEL, params)?;
    // Without a fence there is nothing to compare against, and cancelling
    // whatever happens to be running is the older clients' behaviour rather
    // than a new hazard: the request is built from the job's own state, which
    // is what `BackgroundIpcRequest::cancel_for` has always done.
    let request = match meta.fence {
        Some(fence) => BackgroundIpcRequest::Cancel { fence },
        None => match context.store.read_state(context.job_id) {
            Ok(state) => BackgroundIpcRequest::cancel_for(&state),
            Err(error) => return Err(JsonRpcError::internal_error(error.to_string())),
        },
    };
    let addressed_session = meta
        .session_id
        .as_deref()
        .or(Some(standard.session_id.as_str()));
    if context
        .refusal_for(
            meta.job_id.as_deref(),
            addressed_session,
            &request,
            false,
            || true,
        )
        .is_some()
    {
        return Ok(());
    }
    execute_background_request(request, meta.command_id.as_deref(), context);
    Ok(())
}

/// The `BackgroundIpcRequest` a `_session/*` call has always been.
///
/// `-32602` for params that will not decode and `-32601` for a method this
/// build does not route. The two are different questions and a client acts on
/// them differently: one is worth fixing and resending, the other is worth
/// falling back over.
fn translate(
    request_method: &str,
    params: Option<&serde_json::Value>,
) -> Result<BackgroundIpcRequest, JsonRpcError> {
    use rebon_session_host::session_ext as ext;
    Ok(match request_method {
        method::PING => BackgroundIpcRequest::Ping,
        method::STATUS => BackgroundIpcRequest::Status,
        method::RECONCILE_PLUGINS => BackgroundIpcRequest::ReconcilePlugins,
        method::RUN_COMMAND => {
            let params: ext::RunCommandParams = decode(request_method, params)?;
            BackgroundIpcRequest::RunCommand {
                name: params.name,
                args: params.args,
            }
        }
        method::SET_PERMISSION_MODE => {
            let params: ext::SetPermissionModeParams = decode(request_method, params)?;
            BackgroundIpcRequest::SetPermissionMode { mode: params.mode }
        }
        method::SET_OPTION => {
            let params: ext::SetOptionParams = decode(request_method, params)?;
            BackgroundIpcRequest::SetSessionOption {
                key: params.key,
                value: params.value,
            }
        }
        method::REWIND => {
            let params: ext::RewindParams = decode(request_method, params)?;
            BackgroundIpcRequest::Rewind {
                user_message_uuid: params.user_message_uuid,
                scope: params.scope,
            }
        }
        method::COMPACT => {
            let params: ext::CompactParams = decode(request_method, params)?;
            BackgroundIpcRequest::Compact {
                instructions: params.instructions,
            }
        }
        method::ANSWER_QUESTIONS => {
            let params: ext::AnswerQuestionsParams = decode(request_method, params)?;
            BackgroundIpcRequest::AnswerQuestions {
                query_id: params.query_id,
                turn_generation: params.turn_generation,
                answers: params.answers,
            }
        }
        method::TASK_REPLY => {
            let params: ext::TaskReplyParams = decode(request_method, params)?;
            BackgroundIpcRequest::ReplyTask {
                task_id: params.task_id,
                message: params.message,
            }
        }
        method::CANCEL_TASKS => {
            let params: ext::CancelTasksParams = decode(request_method, params)?;
            BackgroundIpcRequest::CancelTasks {
                task_ids: params.task_ids,
            }
        }
        method::LEASE => {
            let params: ext::LeaseParams = decode(request_method, params)?;
            BackgroundIpcRequest::Lease {
                client_id: params.client_id,
                kind: params.kind,
            }
        }
        method::RELEASE_LEASE => {
            let params: ext::ReleaseLeaseParams = decode(request_method, params)?;
            BackgroundIpcRequest::ReleaseLease {
                client_id: params.client_id,
                deliberate: params.deliberate,
            }
        }
        method::CANCEL_CALL => {
            let params: ext::CancelCallParams = decode(request_method, params)?;
            BackgroundIpcRequest::CancelCall {
                command_id: params.command_id,
            }
        }
        // The standard methods. A prompt is a prompt whichever protocol
        // carried it, so these translate rather than getting a `_session/*`
        // spelling of their own.
        SESSION_PROMPT | method::ENQUEUE => {
            let params: rebon_proto::types::SessionPromptParams = decode(request_method, params)?;
            let (message, images) = prompt_content(params.prompt);
            BackgroundIpcRequest::Reply { message, images }
        }
        SESSION_STEERING => {
            let params: rebon_proto::types::SessionSteeringParams = decode(request_method, params)?;
            let (message, images) = prompt_content(params.prompt);
            BackgroundIpcRequest::Steer { message, images }
        }
        // Declared and not yet routed: `_session/subscribe` takes over the
        // connection rather than answering on it, so it lands with the event
        // stream.
        other => return Err(JsonRpcError::method_not_found(other)),
    })
}

/// The standard method names this control plane also answers.
///
/// Spelled here rather than in `session_ext::method` because they are not
/// extensions: a client that speaks plain ACP calls these and gets what it
/// expects.
pub const SESSION_PROMPT: &str = "session/prompt";
pub const SESSION_CANCEL: &str = "session/cancel";
pub const SESSION_STEERING: &str = "_session/steering";

/// The session a standard method names in its own params.
///
/// Only the standard methods have one; a `_session/*` call says it in
/// `_meta.rebon.sessionId` or not at all.
fn standard_session_id(request_method: &str, params: Option<&serde_json::Value>) -> Option<String> {
    if !matches!(
        request_method,
        SESSION_PROMPT | SESSION_STEERING | method::ENQUEUE
    ) {
        return None;
    }
    params?
        .get("sessionId")?
        .as_str()
        .map(|session| session.to_string())
}

/// A prompt's content blocks as the text and images a session takes.
///
/// Text blocks are joined with newlines, which is what a client that sent
/// several of them meant: ACP has no separator of its own, and a session's
/// prompt is one string.
///
/// Only images survive the other block kinds. Audio, embedded resources and
/// resource links have no representation in a `Reply`, and inventing one here
/// would put content into a turn that the rest of rebon cannot read back --
/// worse than dropping it, because it would look like it worked. A client that
/// sends them gets its text and its images, and nothing silently reshaped.
fn prompt_content(
    blocks: Vec<rebon_types::ContentBlock>,
) -> (String, Vec<rebon_session_host::BackgroundImageAttachment>) {
    let mut text: Vec<String> = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            rebon_types::ContentBlock::Text(content) => text.push(content.text),
            rebon_types::ContentBlock::Image(content) => {
                images.push(rebon_session_host::BackgroundImageAttachment {
                    // The id numbers the images within this one prompt, which
                    // is all it is ever compared within. ACP carries no id of
                    // its own, and borrowing one from anywhere else would make
                    // two prompts' images collide.
                    id: images.len() as u32,
                    data: content.data,
                    media_type: content.mime_type,
                    filename: None,
                    source_path: content.uri,
                });
            }
            _ => {}
        }
    }
    (text.join("\n"), images)
}

/// Decode one method's params, treating absent params as an empty object.
///
/// Absent rather than rejected because a method whose every field is optional
/// is correctly called with no params at all, and refusing that would make
/// `_session/compact` need a `{}` nobody should have to type.
fn decode<T: serde::de::DeserializeOwned>(
    request_method: &str,
    params: Option<&serde_json::Value>,
) -> Result<T, JsonRpcError> {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    serde_json::from_value(params)
        .map_err(|error| JsonRpcError::invalid_params(format!("{request_method} params: {error}")))
}

/// The JSON-RPC result for an answer the shared path produced.
///
/// Three methods have a declared result type and get it; the rest answer with
/// whatever payload the request has always carried, or `{}` when it carries
/// none. Failures, including replayed ones, retain the category chosen by the
/// business path. Only the legacy encoder reduces them to a sentence.
fn result_for(
    request_method: &str,
    carries_command_output: bool,
    response: &WireResponse,
) -> Result<serde_json::Value, JsonRpcError> {
    let data = match response {
        WireResponse::Standard(result) => result.as_ref().map(|reply| reply.data.clone()),
        WireResponse::Command(result) => result
            .as_ref()
            .map(|output| Some(serde_json::to_value(output).expect("CommandOutput serializes"))),
    }
    .map_err(|(kind, message)| {
        let mut error = wire_errors::to_json_rpc(kind);
        error.message = message.clone();
        error
    })?
    .unwrap_or_else(|| serde_json::json!({}));
    Ok(match request_method {
        // The declared result types. Wrapping here rather than at the source
        // keeps the legacy payload byte-identical for the clients still
        // reading it.
        method::STATUS => serde_json::json!({ "snapshot": data }),
        _ if carries_command_output => serde_json::json!({ "output": data }),
        _ => data,
    })
}

/// What this control plane says it is.
///
/// The extension methods are advertised under `_meta.rebon.methods` the same
/// way steering advertises itself under `_meta.steering`: a client that does
/// not find them there is talking to a worker that predates them and should
/// fall back rather than probe method by method.
fn initialize_result() -> serde_json::Value {
    let mut meta = std::collections::HashMap::new();
    meta.insert(
        "rebon".to_string(),
        serde_json::json!({ "methods": method::ALL }),
    );
    serde_json::to_value(InitializeResult {
        protocol_version: ACP_PROTOCOL_VERSION,
        agent_capabilities: AgentCapabilities::default(),
        auth_methods: Vec::new(),
        agent_info: None,
        meta: Some(meta),
    })
    .expect("InitializeResult serializes")
}

/// Write one response. Returns whether the connection survives the write.
fn answer(
    wire: &Wire,
    request: &JsonRpcRequest,
    outcome: Result<serde_json::Value, JsonRpcError>,
) -> bool {
    let (result, error) = match outcome {
        Ok(result) => (Some(result), None),
        Err(error) => (None, Some(error)),
    };
    wire.send_response(&JsonRpcResponse {
        jsonrpc: JsonRpcVersion,
        id: Some(request.id.clone()),
        result,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::{ClientLeaseKind, ForegroundQuestionAnswer, RewindScopeWire};
    use serde_json::json;

    /// Every method this build routes, with params that exercise each field
    /// and the request it must become.
    ///
    /// One entry per variant, checked against `method::ALL` below, so a method
    /// added to the extension without a translation fails here rather than at
    /// runtime with a `-32601` nobody expected.
    fn routed() -> Vec<(&'static str, serde_json::Value, BackgroundIpcRequest)> {
        vec![
            (method::PING, json!({}), BackgroundIpcRequest::Ping),
            (method::STATUS, json!({}), BackgroundIpcRequest::Status),
            (
                method::ENQUEUE,
                json!({"sessionId": "s", "prompt": [{"type": "text", "text": "queued"}]}),
                BackgroundIpcRequest::Reply {
                    message: "queued".into(),
                    images: vec![],
                },
            ),
            (
                method::RECONCILE_PLUGINS,
                json!({}),
                BackgroundIpcRequest::ReconcilePlugins,
            ),
            (
                method::RUN_COMMAND,
                json!({"name": "hooks", "args": ["list"]}),
                BackgroundIpcRequest::RunCommand {
                    name: "hooks".into(),
                    args: vec!["list".into()],
                },
            ),
            (
                method::SET_PERMISSION_MODE,
                json!({"mode": "plan"}),
                BackgroundIpcRequest::SetPermissionMode {
                    mode: "plan".into(),
                },
            ),
            (
                method::SET_OPTION,
                json!({"key": "model", "value": "gpt-6"}),
                BackgroundIpcRequest::SetSessionOption {
                    key: "model".into(),
                    value: "gpt-6".into(),
                },
            ),
            (
                method::REWIND,
                json!({"userMessageUuid": "u-1", "scope": "both"}),
                BackgroundIpcRequest::Rewind {
                    user_message_uuid: "u-1".into(),
                    scope: RewindScopeWire::Both,
                },
            ),
            (
                method::COMPACT,
                json!({"instructions": "keep the decisions"}),
                BackgroundIpcRequest::Compact {
                    instructions: Some("keep the decisions".into()),
                },
            ),
            (
                method::ANSWER_QUESTIONS,
                json!({
                    "queryId": 4,
                    "turnGeneration": 9,
                    "answers": [{"selectedOptions": [1], "otherText": "and this"}],
                }),
                BackgroundIpcRequest::AnswerQuestions {
                    query_id: 4,
                    turn_generation: 9,
                    answers: vec![ForegroundQuestionAnswer {
                        selected_options: vec![1],
                        other_text: Some("and this".into()),
                    }],
                },
            ),
            (
                method::TASK_REPLY,
                json!({"taskId": "t-1", "message": "carry on"}),
                BackgroundIpcRequest::ReplyTask {
                    task_id: "t-1".into(),
                    message: "carry on".into(),
                },
            ),
            (
                method::CANCEL_TASKS,
                json!({"taskIds": ["t-1", "t-2"]}),
                BackgroundIpcRequest::CancelTasks {
                    task_ids: vec!["t-1".into(), "t-2".into()],
                },
            ),
            (
                method::LEASE,
                json!({"clientId": "app-1", "kind": "app"}),
                BackgroundIpcRequest::Lease {
                    client_id: "app-1".into(),
                    kind: ClientLeaseKind::App,
                },
            ),
            (
                method::RELEASE_LEASE,
                json!({"clientId": "app-1", "deliberate": true}),
                BackgroundIpcRequest::ReleaseLease {
                    client_id: "app-1".into(),
                    deliberate: true,
                },
            ),
            (
                method::CANCEL_CALL,
                json!({"commandId": "c-1"}),
                BackgroundIpcRequest::CancelCall {
                    command_id: "c-1".into(),
                },
            ),
        ]
    }

    /// Declared, and deliberately not routed yet: `_session/subscribe` takes
    /// over the connection rather than answering on it, so it lands with the
    /// event stream.
    const DEFERRED: &[&str] = &[method::SUBSCRIBE];

    /// The standard methods this side also translates. Not in `method::ALL`,
    /// which declares only the extension, so they get their own guard.
    fn standard() -> Vec<(&'static str, serde_json::Value, BackgroundIpcRequest)> {
        let prompt = json!({
            "sessionId": "s-1",
            "prompt": [
                {"type": "text", "text": "first"},
                {"type": "image", "mimeType": "image/png", "data": "AAAA", "uri": "/tmp/a.png"},
                {"type": "text", "text": "second"},
            ],
        });
        vec![
            (
                SESSION_PROMPT,
                prompt.clone(),
                BackgroundIpcRequest::Reply {
                    message: "first\nsecond".into(),
                    images: vec![rebon_session_host::BackgroundImageAttachment {
                        id: 0,
                        data: "AAAA".into(),
                        media_type: "image/png".into(),
                        filename: None,
                        source_path: Some("/tmp/a.png".into()),
                    }],
                },
            ),
            (
                SESSION_STEERING,
                prompt,
                BackgroundIpcRequest::Steer {
                    message: "first\nsecond".into(),
                    images: vec![rebon_session_host::BackgroundImageAttachment {
                        id: 0,
                        data: "AAAA".into(),
                        media_type: "image/png".into(),
                        filename: None,
                        source_path: Some("/tmp/a.png".into()),
                    }],
                },
            ),
        ]
    }

    /// Declared, and never dispatched: these travel owner to client.
    const OWNER_TO_CLIENT: &[&str] = &[
        method::HELLO,
        method::TURN,
        method::STATUS_CHANGED,
        method::GAP,
    ];

    /// The standard methods translate too, and to the same requests a legacy
    /// client would have sent: several text blocks become one prompt, an image
    /// block becomes the attachment a session takes.
    #[test]
    fn the_standard_methods_become_the_requests_they_have_always_been() {
        for (name, params, expected) in standard() {
            let translated = translate(name, Some(&params))
                .unwrap_or_else(|error| panic!("{name} did not translate: {error:?}"));
            assert_eq!(
                translated, expected,
                "{name} translated to the wrong request"
            );
        }
    }

    /// `session/cancel` is a notification in the standard, so a *request* by
    /// that name is method-not-found rather than a cancel. It is routed, but
    /// through `run_notification`, which has no reply to give.
    #[test]
    fn cancel_as_a_request_is_not_a_cancel() {
        let error = translate(SESSION_CANCEL, Some(&json!({"sessionId": "s-1"})))
            .expect_err("cancel is a notification, not a request");
        assert_eq!(error.code, rebon_proto::types::error_code::METHOD_NOT_FOUND);
    }

    /// Blocks a `Reply` has no room for are dropped rather than reshaped.
    /// Inventing a representation would put content into a turn that the rest
    /// of rebon cannot read back, which looks like it worked and did not.
    #[test]
    fn content_a_reply_cannot_carry_is_dropped_not_reshaped() {
        let (message, images) = prompt_content(vec![
            rebon_types::ContentBlock::Text(rebon_types::TextContent {
                text: "kept".into(),
                annotations: None,
            }),
            rebon_types::ContentBlock::Audio(rebon_types::AudioContent {
                mime_type: "audio/wav".into(),
                data: "AAAA".into(),
                annotations: None,
            }),
        ]);
        assert_eq!(message, "kept");
        assert!(images.is_empty());
    }

    /// Images are numbered within the prompt that carried them, which is the
    /// only place the number is ever compared.
    #[test]
    fn images_are_numbered_within_their_own_prompt() {
        let image = |data: &str| {
            rebon_types::ContentBlock::Image(rebon_types::ImageContent {
                mime_type: "image/png".into(),
                data: data.into(),
                uri: None,
                annotations: None,
            })
        };
        let (_, images) = prompt_content(vec![image("A"), image("B")]);
        assert_eq!(
            images.iter().map(|image| image.id).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    /// What the client sends and what the owner reads are the same protocol.
    ///
    /// The client spells a request with `session_ext::method_and_params`; this
    /// side reads it back with `translate`. Those are inverses, and nothing
    /// but a test makes them stay inverses -- they are written in two crates,
    /// by two people, months apart. Every variant that has a `_session/*`
    /// spelling goes out and comes back as itself.
    #[test]
    fn what_the_client_spells_is_what_this_side_reads() {
        let requests = vec![
            BackgroundIpcRequest::Ping,
            BackgroundIpcRequest::Status,
            BackgroundIpcRequest::ReconcilePlugins,
            BackgroundIpcRequest::RunCommand {
                name: "hooks".into(),
                args: vec!["list".into()],
            },
            BackgroundIpcRequest::SetPermissionMode {
                mode: "plan".into(),
            },
            BackgroundIpcRequest::SetSessionOption {
                key: "model".into(),
                value: "gpt-6".into(),
            },
            BackgroundIpcRequest::Rewind {
                user_message_uuid: "u-1".into(),
                scope: RewindScopeWire::Both,
            },
            BackgroundIpcRequest::Compact {
                instructions: Some("keep the decisions".into()),
            },
            BackgroundIpcRequest::AnswerQuestions {
                query_id: 4,
                turn_generation: 9,
                answers: vec![ForegroundQuestionAnswer {
                    selected_options: vec![1],
                    other_text: Some("and this".into()),
                }],
            },
            BackgroundIpcRequest::ReplyTask {
                task_id: "t-1".into(),
                message: "carry on".into(),
            },
            BackgroundIpcRequest::CancelTasks {
                task_ids: vec!["t-1".into()],
            },
            BackgroundIpcRequest::Lease {
                client_id: "app-1".into(),
                kind: ClientLeaseKind::App,
            },
            BackgroundIpcRequest::ReleaseLease {
                client_id: "app-1".into(),
                deliberate: true,
            },
            BackgroundIpcRequest::CancelCall {
                command_id: "c-1".into(),
            },
        ];
        for request in requests {
            let (name, params) = rebon_session_host::session_ext::method_and_params(&request)
                .unwrap_or_else(|| panic!("{request:?} has no `_session/*` spelling"));
            let read_back = translate(name, Some(&params))
                .unwrap_or_else(|error| panic!("{name} did not translate back: {error:?}"));
            assert_eq!(read_back, request, "{name} did not survive the round trip");
        }
    }

    /// `_session/subscribe` is spelled by the client but not routed here: it
    /// takes over the connection instead of answering on it, so it is handled
    /// before `translate` is reached. Pinned so that "the client can spell it"
    /// and "this function routes it" do not get confused for each other.
    #[test]
    fn subscribe_is_spelled_by_the_client_but_not_routed_by_translate() {
        let (name, _) =
            rebon_session_host::session_ext::method_and_params(&BackgroundIpcRequest::Subscribe {
                since: Some(3),
            })
            .expect("the client can spell it");
        assert_eq!(name, method::SUBSCRIBE);
        assert!(DEFERRED.contains(&name));
    }

    /// These spellings need connection/session context supplied by OwnerHandle.
    #[test]
    fn context_dependent_requests_are_shaped_by_the_shared_client() {
        for request in [
            BackgroundIpcRequest::Reply {
                message: "hi".into(),
                images: Vec::new(),
            },
            BackgroundIpcRequest::Steer {
                message: "hi".into(),
                images: Vec::new(),
            },
            BackgroundIpcRequest::PermissionAnswer {
                query_id: 1,
                turn_generation: 1,
                option_id: None,
                extra_text: None,
                updated_input: None,
            },
        ] {
            assert!(
                rebon_session_host::session_ext::method_and_params(&request).is_none(),
                "{request:?} should travel as the standard method, not an extension"
            );
        }
    }

    #[test]
    fn every_routed_method_becomes_the_request_it_has_always_been() {
        for (name, params, expected) in routed() {
            let translated = translate(name, Some(&params))
                .unwrap_or_else(|error| panic!("{name} did not translate: {error:?}"));
            assert_eq!(
                translated, expected,
                "{name} translated to the wrong request"
            );
        }
    }

    /// The guard that makes the table above worth having: every declared name
    /// is in exactly one of the three lists, so a method added to the
    /// extension cannot quietly go unrouted.
    #[test]
    fn every_declared_method_is_placed() {
        let routed: Vec<&str> = routed().into_iter().map(|(name, _, _)| name).collect();
        for name in method::ALL {
            let places = usize::from(routed.contains(name))
                + usize::from(DEFERRED.contains(name))
                + usize::from(OWNER_TO_CLIENT.contains(name));
            assert_eq!(
                places, 1,
                "{name} is in {places} of the three lists, and must be in exactly one"
            );
        }
        assert_eq!(
            routed.len() + DEFERRED.len() + OWNER_TO_CLIENT.len(),
            method::ALL.len(),
            "a list names something `method::ALL` does not declare"
        );
    }

    /// A method that is declared but not yet routed answers method-not-found,
    /// which is what tells a client to fall back rather than wait.
    #[test]
    fn a_declared_but_unrouted_method_is_method_not_found() {
        for name in DEFERRED {
            let error = translate(name, Some(&json!({}))).expect_err("should not route yet");
            assert_eq!(error.code, rebon_proto::types::error_code::METHOD_NOT_FOUND);
        }
    }

    /// Params that will not decode are `-32602`, not `-32601`. The method
    /// exists; the call was wrong, and a client acts on those differently.
    #[test]
    fn params_that_do_not_decode_are_invalid_params() {
        let error = translate(method::LEASE, Some(&json!({"clientId": "app-1"})))
            .expect_err("a lease without a kind cannot translate");
        assert_eq!(error.code, rebon_proto::types::error_code::INVALID_PARAMS);
        assert!(
            error.message.contains(method::LEASE),
            "the message should name the method: {}",
            error.message
        );
    }

    /// A method whose fields are all optional is callable with no params.
    #[test]
    fn absent_params_read_as_an_empty_object() {
        assert_eq!(
            translate(method::COMPACT, None).unwrap(),
            BackgroundIpcRequest::Compact { instructions: None }
        );
        assert_eq!(
            translate(method::CANCEL_TASKS, None).unwrap(),
            BackgroundIpcRequest::CancelTasks {
                task_ids: Vec::new()
            }
        );
    }
}
