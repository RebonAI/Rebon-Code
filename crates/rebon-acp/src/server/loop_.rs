use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver},
    oneshot, watch,
};

use super::handler::RequestHandler;
use rebon_agent_core::publisher::{serialize_permission_request, OutboundPermissionRequest};
use rebon_proto::framing::FramingMode;
use rebon_proto::transport::{StdioReader, StdioWriter};
use rebon_proto::types::{
    JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcParseError, JsonRpcRequest,
    JsonRpcResponse, JsonRpcVersion, PermissionOption, PermissionOptionKind, PermissionOutcome,
    RequestId, RequestPermissionParams, RequestPermissionWireResult, SessionUpdateParams,
};

#[derive(Debug)]
struct AllowAlwaysCandidate {
    option_id: String,
    label: String,
    rules: Vec<rebon_permissions::PermissionRuleValue>,
}

struct PendingPermissionRequest {
    session_id: String,
    response_tx: oneshot::Sender<JsonRpcResponse>,
    cwd: Option<String>,
    candidates: Vec<AllowAlwaysCandidate>,
}

type PendingPermissionRequests = HashMap<RequestId, PendingPermissionRequest>;

fn expand_permission_options(
    params: &mut RequestPermissionParams,
    cwd: Option<&str>,
) -> Vec<AllowAlwaysCandidate> {
    let candidates = params
        .tool_name
        .as_deref()
        .filter(|_| {
            params.options.iter().any(|option| {
                option.option_id == "allow_always"
                    || option.kind == PermissionOptionKind::AllowAlways
            })
        })
        .map(|tool_name| allow_always_candidates(tool_name, params.tool_input.as_ref(), cwd))
        .unwrap_or_default();

    // A generalized option belongs directly after the allow-always option
    // it widens; appending would push it past the reject options and split
    // the two allow tiers apart in the client's list.
    let mut insert_at = params
        .options
        .iter()
        .position(|option| {
            option.option_id == "allow_always" || option.kind == PermissionOptionKind::AllowAlways
        })
        .map(|index| index + 1);
    for candidate in &candidates {
        if candidate.option_id == "allow_always"
            || params
                .options
                .iter()
                .any(|option| option.option_id == candidate.option_id)
        {
            continue;
        }
        let option = PermissionOption {
            option_id: candidate.option_id.clone(),
            name: candidate.label.clone(),
            kind: PermissionOptionKind::AllowAlways,
        };
        match insert_at {
            Some(index) => {
                params.options.insert(index, option);
                insert_at = Some(index + 1);
            }
            None => params.options.push(option),
        }
    }
    candidates
}

fn resolve_permission_response<H: RequestHandler + ?Sized>(
    handler: &H,
    pending: &mut PendingPermissionRequests,
    mut response: JsonRpcResponse,
) -> bool {
    let Some(id) = response.id.clone() else {
        return false;
    };
    let Some(pending) = pending.remove(&id) else {
        return false;
    };

    if let Some(result) = response.result.clone() {
        if let Ok(mut wire) = serde_json::from_value::<RequestPermissionWireResult>(result) {
            if wire.outcome.outcome == PermissionOutcome::Selected {
                if let Some(original_option_id) = wire.outcome.option_id.as_deref() {
                    if matches!(
                        original_option_id,
                        "allow_always" | "allow_always_generalized"
                    ) {
                        wire.outcome.option_id = Some(
                            match accept_allow_always(
                                handler,
                                &pending.session_id,
                                pending.cwd.as_deref(),
                                original_option_id,
                                &pending.candidates,
                            ) {
                                Ok(()) => canonical_permission_option_id(original_option_id),
                                Err(error) => {
                                    tracing::warn!(session_id = %pending.session_id, %error, "ACP allow-always rejected by live policy");
                                    "reject_once".to_string()
                                }
                            },
                        );
                        response.result = serde_json::to_value(wire).ok();
                    }
                }
            }
        }
    }

    if pending.response_tx.send(response).is_err() {
        tracing::debug!(?id, "permission requester dropped before response arrived");
    }
    true
}

fn canonical_permission_option_id(option_id: &str) -> String {
    match option_id {
        "allow_always_generalized" => "allow_always".to_string(),
        other => other.to_string(),
    }
}

/// Apply an accepted allow-always selection.
///
/// The session's live policy must accept the hand-off first. Once accepted,
/// a project whose `.rebon/settings.json` cannot be written must still stop
/// re-prompting for this session. Persistence never rolls back a live grant.
fn accept_allow_always<H: RequestHandler + ?Sized>(
    handler: &H,
    session_id: &str,
    cwd: Option<&str>,
    selected_option_id: &str,
    candidates: &[AllowAlwaysCandidate],
) -> Result<(), String> {
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.option_id == selected_option_id)
        .ok_or("ACP allow-always option had no safe rule candidate")?;

    handler.apply_allow_always_rules(session_id, &candidate.rules)?;
    if let Some(cwd) = cwd {
        persist_allow_always_candidate(cwd, &candidate.rules);
    }
    Ok(())
}

fn persist_allow_always_candidate(cwd: &str, rules: &[rebon_permissions::PermissionRuleValue]) {
    let path = Path::new(cwd).join(".rebon").join("settings.json");
    let mut settings = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "failed to read ACP permission settings");
            return;
        }
    };

    let permissions = settings
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    if !permissions.is_object() {
        *permissions = serde_json::json!({});
    }
    let permissions = permissions
        .as_object_mut()
        .expect("permissions normalized to an object");
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    if !allow.is_array() {
        *allow = serde_json::json!([]);
    }
    let allow = allow.as_array_mut().expect("allow normalized to an array");
    for rule in rules {
        let encoded = rebon_permissions::permission_rule_value_to_string(rule);
        if !allow.iter().any(|value| value.as_str() == Some(&encoded)) {
            allow.push(Value::String(encoded));
        }
    }

    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %err, path = %parent.display(), "failed to create ACP permission settings directory");
            return;
        }
    }
    match serde_json::to_string_pretty(&settings) {
        Ok(payload) => {
            if let Err(err) = std::fs::write(&path, format!("{payload}\n")) {
                tracing::warn!(error = %err, path = %path.display(), "failed to persist ACP allow-always rule");
            }
        }
        Err(err) => tracing::warn!(error = %err, "failed to serialize ACP permission settings"),
    }
}

fn allow_always_candidates(
    tool_name: &str,
    tool_input: Option<&Value>,
    cwd: Option<&str>,
) -> Vec<AllowAlwaysCandidate> {
    if let Some(candidates) = mcp_allow_always_candidates(tool_name, tool_input) {
        return candidates;
    }

    // A file tool's "allow always" is scoped to the project directory, so the
    // question is only whether the call names a file — which the tool says.
    if rebon_tools_core::tool_kind_for_name(tool_name).touches_a_file() {
        let Some(cwd) = cwd else {
            return Vec::new();
        };
        return vec![AllowAlwaysCandidate {
            option_id: "allow_always".to_string(),
            label: "Allow always exact command".to_string(),
            rules: vec![rebon_permissions::PermissionRuleValue::new(
                tool_name,
                Some(format!("{}/**", cwd.replace(char::from(92), "/"))),
            )],
        }];
    }

    let exact_content = match tool_name {
        "Bash" | "PowerShell" => shell_command(tool_name, tool_input, cwd).map(str::to_string),
        "WebFetch" => tool_input
            .and_then(|input| input.get("url"))
            .and_then(Value::as_str)
            .and_then(rebon_permissions::web_fetch_hostname)
            .map(|host| format!("domain:{host}")),
        _ => None,
    };
    let Some(exact_content) = exact_content else {
        return Vec::new();
    };

    let mut candidates = vec![AllowAlwaysCandidate {
        option_id: "allow_always".to_string(),
        label: "Allow always exact command".to_string(),
        rules: vec![rebon_permissions::PermissionRuleValue::new(
            tool_name,
            Some(exact_content),
        )],
    }];
    if tool_name == "Bash" {
        if let Some(generalized) = generalized_shell_candidate(tool_name, tool_input, cwd) {
            candidates.push(generalized);
        }
    }
    candidates
}

/// The command text a permission rule should be written against.
///
/// The engine strips the session's working-directory wrapper before
/// evaluating rules, so a rule recorded from the raw
/// `cd "<cwd>" && cargo test` text would never match — and its generalized
/// form would widen to `Bash(cd:*)`. Strip the same wrapper here.
fn shell_command<'a>(
    tool_name: &str,
    tool_input: Option<&'a Value>,
    cwd: Option<&str>,
) -> Option<&'a str> {
    let raw = tool_input?.get("command")?.as_str()?;
    let command =
        rebon_permissions::command_without_cwd_prefix(tool_name, raw, cwd.unwrap_or_default())
            .trim();
    let first = rebon_permissions::shell_runtime::first_shell_word(command).unwrap_or("");
    if command.is_empty()
        || (!first.is_empty()
            && command[first.len()..].trim().is_empty()
            && rebon_permissions::shell_runtime::is_bare_shell_prefix(first))
    {
        None
    } else {
        Some(command)
    }
}

fn generalized_shell_candidate(
    tool_name: &str,
    tool_input: Option<&Value>,
    cwd: Option<&str>,
) -> Option<AllowAlwaysCandidate> {
    use rebon_permissions::shell_runtime::BashShape;

    let command = shell_command(tool_name, tool_input, cwd)?;
    let (segments, allow_first_word_prefix) =
        match rebon_permissions::shell_runtime::parse_bash_shape(command) {
            BashShape::Simple(argv) => (vec![argv], true),
            BashShape::SafeAndChain(segments) => (segments, false),
            BashShape::UnsafeComplex => return None,
        };
    let empty = std::collections::BTreeSet::new();
    let mut rules = Vec::new();
    let mut labels = Vec::new();
    for segment in segments {
        let segment = segment.join(" ");
        let prefix = rebon_permissions::shell_runtime::get_simple_command_prefix(
            &segment, &empty, &empty, false,
        )
        .or_else(|| {
            allow_first_word_prefix.then(|| {
                rebon_permissions::shell_runtime::get_first_word_prefix(
                    &segment, &empty, &empty, false,
                )
            })?
        })?;
        if rebon_permissions::shell_runtime::first_shell_word(&prefix)
            .is_some_and(rebon_permissions::shell_runtime::is_bare_shell_prefix)
        {
            return None;
        }
        let content = format!("{prefix}:*");
        if !labels.contains(&prefix) {
            labels.push(prefix);
            rules.push(rebon_permissions::PermissionRuleValue::new(
                tool_name,
                Some(content),
            ));
        }
    }
    (!rules.is_empty()).then(|| AllowAlwaysCandidate {
        option_id: "allow_always_generalized".to_string(),
        label: format!("Allow always {} commands", labels.join(" + ")),
        rules,
    })
}

fn mcp_allow_always_candidates(
    tool_name: &str,
    tool_input: Option<&Value>,
) -> Option<Vec<AllowAlwaysCandidate>> {
    if let Some(rest) = tool_name.strip_prefix("mcp__") {
        if rest.is_empty() {
            return Some(Vec::new());
        }
        let mut candidates = vec![AllowAlwaysCandidate {
            option_id: "allow_always".to_string(),
            label: format!("Allow always {tool_name}"),
            rules: vec![rebon_permissions::PermissionRuleValue::new(
                tool_name,
                None::<String>,
            )],
        }];
        if let Some((server, tool)) = rest.rsplit_once("__") {
            if !server.is_empty() && !tool.is_empty() {
                candidates.push(AllowAlwaysCandidate {
                    option_id: "allow_always_generalized".to_string(),
                    label: format!("Allow always all {server} tools"),
                    rules: vec![rebon_permissions::PermissionRuleValue::new(
                        format!("mcp__{server}"),
                        None::<String>,
                    )],
                });
            }
        }
        return Some(candidates);
    }

    if matches!(tool_name, "Mcp" | "McpTool" | "MCPTool") {
        let server = tool_input?.get("server")?.as_str()?.trim();
        if server.is_empty() {
            return Some(Vec::new());
        }
        let name = tool_input
            .and_then(|input| input.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let mut candidates = Vec::new();
        if let Some(name) = name {
            candidates.push(AllowAlwaysCandidate {
                option_id: "allow_always".to_string(),
                label: format!("Allow always {server}: {name}"),
                rules: vec![rebon_permissions::PermissionRuleValue::new(
                    "Mcp",
                    Some(format!("{server}:{name}")),
                )],
            });
        }
        candidates.push(AllowAlwaysCandidate {
            option_id: if name.is_some() {
                "allow_always_generalized".to_string()
            } else {
                "allow_always".to_string()
            },
            label: format!("Allow always all {server} tools"),
            rules: vec![rebon_permissions::PermissionRuleValue::new(
                "Mcp",
                Some(format!("{server}:*")),
            )],
        });
        return Some(candidates);
    }
    None
}

struct PromptBarrierKey {
    session_id: String,
    id: u64,
    dispatch: Arc<PromptDispatch>,
}

struct PromptStartBarrier {
    id: u64,
    started: watch::Receiver<bool>,
    dispatch: Arc<PromptDispatch>,
}

#[derive(Default)]
struct PromptDispatchState {
    completed: bool,
    pending_cancels: usize,
}

#[derive(Default)]
struct PromptDispatch {
    state: Mutex<PromptDispatchState>,
    cancels_drained: tokio::sync::Notify,
}

impl PromptDispatch {
    fn register_cancel(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.completed {
            return false;
        }
        state.pending_cancels += 1;
        true
    }

    async fn complete(&self) {
        loop {
            let notified = self.cancels_drained.notified();
            let drained = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.completed = true;
                state.pending_cancels == 0
            };
            if drained {
                return;
            }
            notified.await;
        }
    }

    fn cancel_dispatched(&self) {
        let drained = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            debug_assert!(state.pending_cancels > 0);
            state.pending_cancels = state.pending_cancels.saturating_sub(1);
            state.pending_cancels == 0
        };
        if drained {
            self.cancels_drained.notify_one();
        }
    }
}

enum RequestWork {
    Request {
        request: JsonRpcRequest,
        started: Option<watch::Sender<bool>>,
        prompt_barrier: Option<PromptBarrierKey>,
    },
    ImmediateResponse(Vec<u8>),
}

struct RequestResponse {
    bytes: Vec<u8>,
    completed_prompt: Option<PromptBarrierKey>,
}

struct NotificationWork {
    notification: JsonRpcNotification,
    preceding_prompt: Option<(watch::Receiver<bool>, Arc<PromptDispatch>)>,
}

fn session_id(params: Option<&Value>) -> Option<&str> {
    params?.get("sessionId")?.as_str()
}

async fn handle_request_and_signal_start<H: RequestHandler>(
    handler: &H,
    request: &JsonRpcRequest,
    started: Option<watch::Sender<bool>>,
) -> Result<Value, JsonRpcError> {
    let mut started = started;
    let mut future =
        std::pin::pin!(handler.handle_request(&request.method, request.params.clone()));
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(result) => {
            if let Some(started) = started.take() {
                let _ = started.send(true);
            }
            Poll::Ready(result)
        }
        Poll::Pending => {
            if let Some(started) = started.take() {
                let _ = started.send(true);
            }
            Poll::Pending
        }
    })
    .await
}

async fn wait_for_preceding_prompt(mut started: watch::Receiver<bool>) {
    if !*started.borrow() {
        let _ = started.changed().await;
    }
}

pub async fn serve<R, W, H>(reader: R, writer: W, handler: H) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    H: RequestHandler + 'static,
{
    serve_with_publishers(reader, writer, handler, None, None).await
}

/// Back-compat wrapper around [`serve_with_publishers`] for callers that only
/// need outbound `session/update` notifications.
pub async fn serve_with_publisher<R, W, H>(
    reader: R,
    writer: W,
    handler: H,
    outbound_notifications: Option<UnboundedReceiver<SessionUpdateParams>>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    H: RequestHandler + 'static,
{
    serve_with_publishers(reader, writer, handler, outbound_notifications, None).await
}

/// Run the ACP server loop, optionally also draining outbound
/// `session/update` notifications and `session/request_permission` reverse
/// requests produced by publishers.
pub async fn serve_with_publishers<R, W, H>(
    reader: R,
    writer: W,
    handler: H,
    outbound_notifications: Option<UnboundedReceiver<SessionUpdateParams>>,
    outbound_permission_requests: Option<UnboundedReceiver<OutboundPermissionRequest>>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    H: RequestHandler + 'static,
{
    let mut reader = StdioReader::new(reader);
    let mut writer = StdioWriter::new(writer);
    let mut outbound_notifications = outbound_notifications;
    let mut outbound_permission_requests = outbound_permission_requests;
    let mut pending_permission_requests = PendingPermissionRequests::new();
    let handler = std::sync::Arc::new(handler);

    // Requests have one FIFO worker so their responses cannot overtake one
    // another. Notifications have a distinct FIFO worker: ingestion never puts
    // a later request in front of session/cancel, and a pending prompt cannot
    // prevent subsequent notifications from being dispatched. `_session/steering`
    // requests get a third worker: the whole point of steering is to reach the
    // handler *while* a session/prompt is still occupying the request worker,
    // so it must bypass that FIFO — its responses may legally overtake a
    // pending prompt's response.
    let (request_tx, request_rx) = mpsc::unbounded_channel::<RequestWork>();
    let (steering_tx, steering_rx) = mpsc::unbounded_channel::<JsonRpcRequest>();
    let (notification_tx, notification_rx) = mpsc::unbounded_channel::<NotificationWork>();
    let (response_tx, mut response_rx) = mpsc::unbounded_channel::<RequestResponse>();
    // Each session retains every accepted prompt until that prompt finishes.
    // Cancels bind to the oldest unfinished prompt that preceded them, so a
    // queued prompt can never hide an active prompt's already-ready barrier.
    let mut preceding_prompts = HashMap::<String, VecDeque<PromptStartBarrier>>::new();
    let mut next_prompt_barrier_id = 0_u64;

    let AcpWorkers {
        steering: mut steering_worker,
        request: mut request_worker,
        notification: mut notification_worker,
    } = spawn_acp_workers(
        &handler,
        steering_rx,
        request_rx,
        notification_rx,
        // Moved, not cloned: the request worker owns the last sender, so the
        // response channel closes when the workers stop. A copy kept here
        // would hold it open and the loop would never see the end of input.
        response_tx,
    );

    enum ServeEvent {
        Notification(Option<SessionUpdateParams>),
        PermissionRequest(Option<OutboundPermissionRequest>),
        RequestResponse(Option<RequestResponse>),
        Input(std::io::Result<Option<Vec<u8>>>),
        RequestWorker(Result<(), tokio::task::JoinError>),
        SteeringWorker(Result<(), tokio::task::JoinError>),
        NotificationWorker(Result<(), tokio::task::JoinError>),
    }

    let mut accepting_input = true;
    let mut request_tx = Some(request_tx);
    let mut steering_tx = Some(steering_tx);
    let mut notification_tx = Some(notification_tx);
    let mut response_channel_closed = false;

    let result: anyhow::Result<()> = async {
        loop {
            if !accepting_input
                && request_worker.is_none()
                && steering_worker.is_none()
                && notification_worker.is_none()
                && response_channel_closed
                && outbound_permission_requests.is_none()
            {
                if let Some(receiver) = outbound_notifications.as_mut() {
                    // The handler intentionally retains its publisher sender for
                    // its whole lifetime, so the channel cannot close naturally.
                    // Once every accepted producer is done, close the receiver and
                    // let the select loop drain notifications already buffered.
                    receiver.close();
                }
            }
            if !accepting_input
                && request_worker.is_none()
                && steering_worker.is_none()
                && notification_worker.is_none()
                && response_channel_closed
                && outbound_notifications.is_none()
                && outbound_permission_requests.is_none()
            {
                break Ok(());
            }

            let event = tokio::select! {
                biased;
                response = response_rx.recv(), if !response_channel_closed => {
                    ServeEvent::RequestResponse(response)
                }
                note = async {
                    outbound_notifications
                        .as_mut()
                        .expect("notification receiver guarded by select condition")
                        .recv()
                        .await
                }, if outbound_notifications.is_some() => ServeEvent::Notification(note),
                request = async {
                    outbound_permission_requests
                        .as_mut()
                        .expect("permission receiver guarded by select condition")
                        .recv()
                        .await
                }, if outbound_permission_requests.is_some() => {
                    ServeEvent::PermissionRequest(request)
                }
                worker = async {
                    request_worker
                        .as_mut()
                        .expect("request worker guarded by select condition")
                        .await
                }, if request_worker.is_some() => ServeEvent::RequestWorker(worker),
                worker = async {
                    steering_worker
                        .as_mut()
                        .expect("steering worker guarded by select condition")
                        .await
                }, if steering_worker.is_some() => ServeEvent::SteeringWorker(worker),
                worker = async {
                    notification_worker
                        .as_mut()
                        .expect("notification worker guarded by select condition")
                        .await
                }, if notification_worker.is_some() => ServeEvent::NotificationWorker(worker),
                body = reader.read_message(), if accepting_input => ServeEvent::Input(body),
            };

            match event {
                ServeEvent::Notification(Some(params)) => {
                    let bytes = serialize_notification("session/update", &params)?;
                    write_framed(&mut writer, reader.framing(), &bytes).await?;
                }
                ServeEvent::Notification(None) => outbound_notifications = None,
                ServeEvent::PermissionRequest(Some(mut outbound)) => {
                    let cwd = handler.permission_session_cwd(&outbound.params.session_id);
                    let candidates =
                        expand_permission_options(&mut outbound.params, cwd.as_deref());
                    let bytes = serialize_permission_request(
                        outbound.request_id.clone(),
                        &outbound.params,
                    )?;
                    pending_permission_requests.insert(
                        outbound.request_id,
                        PendingPermissionRequest {
                            session_id: outbound.params.session_id.clone(),
                            response_tx: outbound.response_tx,
                            cwd,
                            candidates,
                        },
                    );
                    write_framed(&mut writer, reader.framing(), &bytes).await?;
                }
                ServeEvent::PermissionRequest(None) => outbound_permission_requests = None,
                ServeEvent::RequestResponse(Some(response)) => {
                    if let Some(completed) = response.completed_prompt {
                        let remove_session = preceding_prompts
                            .get_mut(&completed.session_id)
                            .map(|barriers| {
                                let barrier = barriers
                                    .pop_front()
                                    .expect("completed prompt has a start barrier");
                                debug_assert_eq!(barrier.id, completed.id);
                                barriers.is_empty()
                            })
                            .unwrap_or(false);
                        if remove_session {
                            preceding_prompts.remove(&completed.session_id);
                        }
                    }
                    write_framed(&mut writer, reader.framing(), &response.bytes).await?;
                }
                ServeEvent::RequestResponse(None) => {
                    response_channel_closed = true;
                    if accepting_input {
                        break Err(anyhow::anyhow!("ACP request worker stopped unexpectedly"));
                    }
                }
                ServeEvent::RequestWorker(worker_result) => {
                    request_worker = None;
                    if let Err(err) = worker_result {
                        break Err(anyhow::anyhow!("ACP request worker failed: {err}"));
                    }
                    if accepting_input {
                        break Err(anyhow::anyhow!("ACP request worker stopped unexpectedly"));
                    }
                }
                ServeEvent::SteeringWorker(worker_result) => {
                    steering_worker = None;
                    if let Err(err) = worker_result {
                        break Err(anyhow::anyhow!("ACP steering worker failed: {err}"));
                    }
                    if accepting_input {
                        break Err(anyhow::anyhow!("ACP steering worker stopped unexpectedly"));
                    }
                }
                ServeEvent::NotificationWorker(worker_result) => {
                    notification_worker = None;
                    if let Err(err) = worker_result {
                        break Err(anyhow::anyhow!("ACP notification worker failed: {err}"));
                    }
                    if accepting_input {
                        break Err(anyhow::anyhow!(
                            "ACP notification worker stopped unexpectedly"
                        ));
                    }
                }
                ServeEvent::Input(Err(err)) => break Err(err.into()),
                ServeEvent::Input(Ok(None)) => {
                    // Stop accepting commands. No pending or future permission
                    // reverse request can receive a response after inbound EOF,
                    // so release those waiters before draining accepted work.
                    accepting_input = false;
                    request_tx.take();
                    steering_tx.take();
                    notification_tx.take();
                    pending_permission_requests.clear();
                    outbound_permission_requests = None;
                }
                ServeEvent::Input(Ok(Some(body))) => dispatch_incoming_message(
                    &body,
                    &handler,
                    &request_tx,
                    &steering_tx,
                    &notification_tx,
                    &mut preceding_prompts,
                    &mut next_prompt_barrier_id,
                    &mut pending_permission_requests,
                )?,
            }
        }
    }
    .await;

    // Every non-graceful exit must synchronously drop handler futures before
    // returning. In particular, aborting the request worker drops a pending
    // session/prompt future and its PromptLifecycleGuard.
    request_tx.take();
    steering_tx.take();
    notification_tx.take();
    if let Some(worker) = request_worker {
        worker.abort();
        let _ = worker.await;
    }
    if let Some(worker) = steering_worker {
        worker.abort();
        let _ = worker.await;
    }
    if let Some(worker) = notification_worker {
        worker.abort();
        let _ = worker.await;
    }

    result
}

/// Serialize a JSON-RPC 2.0 notification to wire bytes.
///
/// `params` is anything `Serialize` — typically [`SessionUpdateParams`]
/// for `session/update`. The output has no `id` field, matching the
/// JSON-RPC 2.0 spec for notifications.
fn serialize_notification<P: serde::Serialize>(
    method: &str,
    params: &P,
) -> anyhow::Result<Vec<u8>> {
    let value = serde_json::to_value(params)
        .map_err(|e| anyhow::anyhow!("failed to serialize notification params: {e}"))?;
    let note = JsonRpcNotification {
        jsonrpc: JsonRpcVersion,
        method: method.to_string(),
        params: Some(value),
    };
    serde_json::to_vec(&note)
        .map_err(|e| anyhow::anyhow!("failed to serialize JsonRpcNotification: {e}"))
}

fn success_response(id: RequestId, value: Value) -> Vec<u8> {
    let resp = JsonRpcResponse {
        jsonrpc: JsonRpcVersion,
        id: Some(id),
        result: Some(value),
        error: None,
    };
    serde_json::to_vec(&resp).expect("JsonRpcResponse always serializes")
}

fn error_response(id: Option<RequestId>, error: JsonRpcError) -> Vec<u8> {
    let resp = JsonRpcResponse {
        jsonrpc: JsonRpcVersion,
        id,
        result: None,
        error: Some(error),
    };
    serde_json::to_vec(&resp).expect("JsonRpcResponse always serializes")
}

/// Best-effort attempt to recover the `id` field from a structurally
/// invalid JSON-RPC body, so the error response we send back can echo it.
/// JSON-RPC 2.0 says: when the id can't be determined, send `null`.
fn recover_request_id(body: &[u8]) -> Option<RequestId> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let id = v.get("id")?.clone();
    serde_json::from_value(id).ok()
}

async fn write_framed<W>(
    writer: &mut StdioWriter<W>,
    framing: FramingMode,
    body: &[u8],
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match framing {
        FramingMode::ContentLength => writer.write_content_length(body).await,
        // Default to NDJSON when the reader hasn't yet pinned a framing mode
        // (which can only happen on an empty stream). NDJSON is the safest
        // default because it needs no length header state.
        FramingMode::Ndjson | FramingMode::Auto => writer.write_ndjson(body).await,
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use async_trait::async_trait;
    use rebon_permissions::PermissionRuleValue;
    use rebon_proto::types::{RequestPermissionResult, ToolCallReference};
    use serde_json::json;

    /// Records what the host would have applied to its live policy store.
    #[derive(Default)]
    struct RecordingHandler {
        applied: Mutex<Vec<Vec<PermissionRuleValue>>>,
        fail_live: bool,
    }

    impl RecordingHandler {
        fn applied(&self) -> Vec<Vec<PermissionRuleValue>> {
            self.applied.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RequestHandler for RecordingHandler {
        async fn handle_request(
            &self,
            _method: &str,
            _params: Option<Value>,
        ) -> Result<Value, JsonRpcError> {
            Ok(Value::Null)
        }

        fn apply_allow_always_rules(
            &self,
            session_id: &str,
            rules: &[PermissionRuleValue],
        ) -> Result<(), String> {
            assert_eq!(
                session_id, "sess-1",
                "response must retain the requesting session"
            );
            if self.fail_live {
                return Err("session policy unavailable".into());
            }
            self.applied.lock().unwrap().push(rules.to_vec());
            Ok(())
        }
    }

    fn option(option_id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption {
            option_id: option_id.to_string(),
            name: option_id.to_string(),
            kind,
        }
    }

    fn bash_params(command: &str) -> RequestPermissionParams {
        RequestPermissionParams {
            session_id: "sess-1".into(),
            tool_call: ToolCallReference {
                tool_call_id: "toolu_01".into(),
            },
            options: vec![
                option("allow_once", PermissionOptionKind::AllowOnce),
                option("allow_always", PermissionOptionKind::AllowAlways),
                option("reject_once", PermissionOptionKind::RejectOnce),
            ],
            title: None,
            message: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({ "command": command })),
            metadata: None,
        }
    }

    fn option_ids(params: &RequestPermissionParams) -> Vec<&str> {
        params
            .options
            .iter()
            .map(|option| option.option_id.as_str())
            .collect()
    }

    fn rule_strings(rules: &[PermissionRuleValue]) -> Vec<String> {
        rules
            .iter()
            .map(rebon_permissions::permission_rule_value_to_string)
            .collect()
    }

    fn exact_rules(candidates: &[AllowAlwaysCandidate], option_id: &str) -> Vec<String> {
        candidates
            .iter()
            .find(|candidate| candidate.option_id == option_id)
            .map(|candidate| rule_strings(&candidate.rules))
            .unwrap_or_default()
    }

    #[test]
    fn exact_shell_candidate_retains_the_whole_command() {
        let mut params = bash_params("ls -la");
        let candidates = expand_permission_options(&mut params, Some("/project"));
        let handler = RecordingHandler::default();
        accept_allow_always(&handler, "sess-1", None, "allow_always", &candidates).unwrap();
        assert_eq!(
            handler.applied(),
            vec![vec![PermissionRuleValue::new("Bash", Some("ls -la"))]],
            "candidate.rules={candidates:?}"
        );
    }

    #[test]
    fn unoffered_allow_always_has_no_candidate_or_live_grant() {
        let mut params = bash_params("ls -la");
        params
            .options
            .retain(|option| option.option_id != "allow_always");
        let candidates = expand_permission_options(&mut params, Some("/project"));
        assert!(candidates.is_empty(), "candidate.rules={candidates:?}");
        let handler = RecordingHandler::default();
        assert!(
            accept_allow_always(&handler, "sess-1", None, "allow_always", &candidates).is_err()
        );
        assert!(handler.applied().is_empty());
    }

    #[test]
    fn generalized_option_sits_directly_after_allow_always() {
        let mut params = bash_params("cargo test -p rebon-acp");
        let candidates = expand_permission_options(&mut params, Some("C:/projects/example"));

        assert!(candidates
            .iter()
            .any(|candidate| candidate.option_id == "allow_always_generalized"));
        assert_eq!(
            option_ids(&params),
            vec![
                "allow_once",
                "allow_always",
                "allow_always_generalized",
                "reject_once"
            ],
            "a reject option must not separate the two allow tiers"
        );
    }

    #[test]
    fn expanding_twice_does_not_duplicate_the_generalized_option() {
        let mut params = bash_params("cargo test -p rebon-acp");
        expand_permission_options(&mut params, Some("C:/projects/example"));
        expand_permission_options(&mut params, Some("C:/projects/example"));

        assert_eq!(
            option_ids(&params),
            vec![
                "allow_once",
                "allow_always",
                "allow_always_generalized",
                "reject_once"
            ]
        );
    }

    #[test]
    fn cwd_prefix_is_stripped_from_both_rule_shapes() {
        let candidates = allow_always_candidates(
            "Bash",
            Some(&json!({ "command": r#"cd "C:\projects\example" && cargo test"# })),
            Some("C:/projects/example"),
        );

        assert_eq!(
            exact_rules(&candidates, "allow_always"),
            ["Bash(cargo test)"]
        );
        assert_eq!(
            exact_rules(&candidates, "allow_always_generalized"),
            ["Bash(cargo test:*)"],
            "the cd wrapper must not become its own generalized rule"
        );
    }

    #[test]
    fn powershell_cwd_prefix_is_stripped() {
        let candidates = allow_always_candidates(
            "PowerShell",
            Some(&json!({ "command": r#"Set-Location "C:\projects\example"; cargo test"# })),
            Some("C:/projects/example"),
        );

        assert_eq!(
            exact_rules(&candidates, "allow_always"),
            ["PowerShell(cargo test)"]
        );
    }

    #[test]
    fn cd_to_another_directory_is_left_intact() {
        let candidates = allow_always_candidates(
            "Bash",
            Some(&json!({ "command": "cd /tmp && ls" })),
            Some("C:/projects/example"),
        );

        assert_eq!(
            exact_rules(&candidates, "allow_always"),
            ["Bash(cd /tmp && ls)"]
        );
    }

    #[test]
    fn unsafe_chain_offers_no_generalized_candidate() {
        let candidates = allow_always_candidates(
            "Bash",
            Some(&json!({ "command": "cargo test | tee out.txt" })),
            Some("C:/projects/example"),
        );

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.option_id.as_str())
                .collect::<Vec<_>>(),
            ["allow_always"]
        );
    }

    #[test]
    fn generalized_selection_applies_its_own_rules_and_reports_allow_always() {
        let handler = RecordingHandler::default();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_string = cwd.path().to_string_lossy().replace('\\', "/");

        let mut params = bash_params(&format!(r#"cd "{cwd_string}" && cargo test"#));
        let candidates = expand_permission_options(&mut params, Some(&cwd_string));

        let (response_tx, mut response_rx) = oneshot::channel();
        let mut pending = PendingPermissionRequests::new();
        let id = RequestId::Number(7);
        pending.insert(
            id.clone(),
            PendingPermissionRequest {
                session_id: params.session_id.clone(),
                response_tx,
                cwd: Some(cwd_string.clone()),
                candidates,
            },
        );

        let resolved = resolve_permission_response(
            &handler,
            &mut pending,
            JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: Some(id),
                result: Some(
                    serde_json::to_value(RequestPermissionWireResult {
                        outcome: RequestPermissionResult {
                            outcome: PermissionOutcome::Selected,
                            option_id: Some("allow_always_generalized".into()),
                            updated_input: None,
                        },
                        updated_input: None,
                    })
                    .unwrap(),
                ),
                error: None,
            },
        );
        assert!(resolved);

        assert_eq!(
            handler.applied(),
            vec![vec![PermissionRuleValue::new("Bash", Some("cargo test:*"))]],
            "the rules must come from the option the client actually picked"
        );

        let response = response_rx.try_recv().unwrap();
        let wire: RequestPermissionWireResult =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(
            wire.outcome.option_id.as_deref(),
            Some("allow_always"),
            "the engine only knows the canonical option ids"
        );

        let settings: Value = serde_json::from_slice(
            &std::fs::read(cwd.path().join(".rebon").join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["permissions"]["allow"][0], "Bash(cargo test:*)");
    }

    #[test]
    fn allow_always_live_failure_rejects_but_persistence_failure_keeps_grant() {
        for fail_live in [false, true] {
            let handler = RecordingHandler {
                fail_live,
                ..Default::default()
            };
            let project = tempfile::tempdir().unwrap();
            let cwd = project.path().to_string_lossy().into_owned();
            // A directory at the settings file path deterministically fails persistence.
            let settings = project.path().join(".rebon/settings.json");
            if !fail_live {
                std::fs::create_dir_all(&settings).unwrap();
            }
            let mut params = bash_params("cargo test");
            let candidates = expand_permission_options(&mut params, Some(&cwd));
            let (response_tx, mut response_rx) = oneshot::channel();
            let mut pending = PendingPermissionRequests::new();
            let id = RequestId::Number(13);
            pending.insert(
                id.clone(),
                PendingPermissionRequest {
                    session_id: params.session_id,
                    response_tx,
                    cwd: Some(cwd),
                    candidates,
                },
            );
            let response: JsonRpcResponse = serde_json::from_value(json!({
                "jsonrpc":"2.0", "id":13,
                "result":{"outcome":{"outcome":"selected","optionId":"allow_always"}}
            }))
            .unwrap();
            assert!(resolve_permission_response(
                &handler,
                &mut pending,
                response
            ));
            let wire: RequestPermissionWireResult =
                serde_json::from_value(response_rx.try_recv().unwrap().result.unwrap()).unwrap();
            if fail_live {
                assert_eq!(wire.outcome.option_id.as_deref(), Some("reject_once"));
                assert!(handler.applied().is_empty());
                assert!(
                    !settings.exists(),
                    "failed live grant must not be persisted"
                );
            } else {
                assert_eq!(wire.outcome.option_id.as_deref(), Some("allow_always"));
                assert_eq!(
                    handler.applied(),
                    vec![vec![PermissionRuleValue::new("Bash", Some("cargo test"))]]
                );
                assert!(settings.is_dir());
            }
        }
    }

    #[test]
    fn allow_always_takes_effect_in_memory_without_a_writable_project() {
        let handler = RecordingHandler::default();
        let mut params = bash_params("cargo test");
        // No session cwd: nothing can be persisted, but the grant must still
        // stop this session from re-prompting.
        let candidates = expand_permission_options(&mut params, None);

        let (response_tx, _response_rx) = oneshot::channel();
        let mut pending = PendingPermissionRequests::new();
        let id = RequestId::Number(3);
        pending.insert(
            id.clone(),
            PendingPermissionRequest {
                session_id: params.session_id.clone(),
                response_tx,
                cwd: None,
                candidates,
            },
        );

        resolve_permission_response(
            &handler,
            &mut pending,
            JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: Some(id),
                result: Some(
                    serde_json::to_value(RequestPermissionWireResult {
                        outcome: RequestPermissionResult {
                            outcome: PermissionOutcome::Selected,
                            option_id: Some("allow_always".into()),
                            updated_input: None,
                        },
                        updated_input: None,
                    })
                    .unwrap(),
                ),
                error: None,
            },
        );

        assert_eq!(
            handler.applied(),
            vec![vec![PermissionRuleValue::new("Bash", Some("cargo test"))]]
        );
    }
}

/// Route one decoded stdio message to the worker that owns it.
///
/// `_session/steering` bypasses the FIFO request worker on purpose: steering
/// has to reach the handler while a `session/prompt` is still occupying that
/// worker, so a steering response overtaking a pending prompt's response is
/// the intended ordering, not a race.
#[allow(clippy::too_many_arguments)]
fn dispatch_incoming_message<H>(
    body: &[u8],
    handler: &std::sync::Arc<H>,
    request_tx: &Option<mpsc::UnboundedSender<RequestWork>>,
    steering_tx: &Option<mpsc::UnboundedSender<JsonRpcRequest>>,
    notification_tx: &Option<mpsc::UnboundedSender<NotificationWork>>,
    preceding_prompts: &mut HashMap<String, VecDeque<PromptStartBarrier>>,
    next_prompt_barrier_id: &mut u64,
    pending_permission_requests: &mut PendingPermissionRequests,
) -> anyhow::Result<()>
where
    H: RequestHandler + 'static,
{
    match JsonRpcMessage::from_bytes(body) {
        Ok(JsonRpcMessage::Request(req)) => {
            tracing::debug!(method = %req.method, id = ?req.id, "rebon-acp request");
            if req.method == "_session/steering" {
                // Bypass the FIFO request worker: steering must
                // reach the handler while a session/prompt is
                // still executing there. No prompt barrier — a
                // steering response overtaking a pending prompt
                // response is the intended ordering.
                steering_tx
                    .as_ref()
                    .expect("steering sender exists while accepting input")
                    .send(req)
                    .map_err(|_| {
                        anyhow::anyhow!("ACP steering worker stopped before accepting request")
                    })?;
                return Ok(());
            }
            let (started, prompt_barrier) = if req.method == "session/prompt" {
                if let Some(session_id) = session_id(req.params.as_ref()) {
                    let barrier_id = *next_prompt_barrier_id;
                    *next_prompt_barrier_id = next_prompt_barrier_id.wrapping_add(1);
                    let (started_tx, started_rx) = watch::channel(false);
                    let dispatch = Arc::new(PromptDispatch::default());
                    preceding_prompts
                        .entry(session_id.to_string())
                        .or_default()
                        .push_back(PromptStartBarrier {
                            id: barrier_id,
                            started: started_rx,
                            dispatch: dispatch.clone(),
                        });
                    (
                        Some(started_tx),
                        Some(PromptBarrierKey {
                            session_id: session_id.to_string(),
                            id: barrier_id,
                            dispatch,
                        }),
                    )
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };
            request_tx
                .as_ref()
                .expect("request sender exists while accepting input")
                .send(RequestWork::Request {
                    request: req,
                    started,
                    prompt_barrier,
                })
                .map_err(|_| {
                    anyhow::anyhow!("ACP request worker stopped before accepting request")
                })?;
        }
        Ok(JsonRpcMessage::Notification(note)) => {
            tracing::debug!(method = %note.method, "rebon-acp notification");
            let preceding_prompt = if note.method == "session/cancel" {
                session_id(note.params.as_ref()).and_then(|session_id| {
                    preceding_prompts.get(session_id).and_then(|barriers| {
                        barriers.iter().find_map(|barrier| {
                            barrier
                                .dispatch
                                .register_cancel()
                                .then(|| (barrier.started.clone(), barrier.dispatch.clone()))
                        })
                    })
                })
            } else {
                None
            };
            notification_tx
                .as_ref()
                .expect("notification sender exists while accepting input")
                .send(NotificationWork {
                    notification: note,
                    preceding_prompt,
                })
                .map_err(|_| {
                    anyhow::anyhow!("ACP notification worker stopped before accepting notification")
                })?;
        }
        Ok(JsonRpcMessage::Response(resp)) => {
            if !resolve_permission_response(
                handler.as_ref(),
                pending_permission_requests,
                resp.clone(),
            ) {
                tracing::debug!(
                    id = ?resp.id,
                    "rebon-acp ignoring unsolicited JSON-RPC response"
                );
            }
        }
        Err(JsonRpcParseError::InvalidJson(e)) => {
            tracing::warn!(error = %e, "rebon-acp parse error");
            let response_bytes =
                error_response(None, JsonRpcError::parse_error(format!("Parse error: {e}")));
            request_tx
                .as_ref()
                .expect("request sender exists while accepting input")
                .send(RequestWork::ImmediateResponse(response_bytes))
                .map_err(|_| {
                    anyhow::anyhow!("ACP request worker stopped before accepting error response")
                })?;
        }
        Err(JsonRpcParseError::InvalidRequest) => {
            let id = recover_request_id(body);
            tracing::warn!(?id, "rebon-acp invalid JSON-RPC request");
            let response_bytes = error_response(
                id,
                JsonRpcError::invalid_request("Invalid JSON-RPC 2.0 message"),
            );
            request_tx
                .as_ref()
                .expect("request sender exists while accepting input")
                .send(RequestWork::ImmediateResponse(response_bytes))
                .map_err(|_| {
                    anyhow::anyhow!("ACP request worker stopped before accepting error response")
                })?;
        }
    }
    Ok(())
}

/// The three FIFO workers the serve loop dispatches into.
struct AcpWorkers {
    steering: Option<tokio::task::JoinHandle<()>>,
    request: Option<tokio::task::JoinHandle<()>>,
    notification: Option<tokio::task::JoinHandle<()>>,
}

/// Start the three workers that run handler calls off the read loop.
///
/// Requests get one FIFO worker so their responses cannot overtake one
/// another. Notifications get a distinct one: ingestion never puts a later
/// request in front of `session/cancel`, and a pending prompt cannot stop
/// subsequent notifications from being dispatched. `_session/steering` gets a
/// third, because the whole point of steering is to reach the handler *while*
/// a `session/prompt` still occupies the request worker -- so it must bypass
/// that FIFO, and its responses may legally overtake a pending prompt's.
fn spawn_acp_workers<H>(
    handler: &std::sync::Arc<H>,
    mut steering_rx: mpsc::UnboundedReceiver<JsonRpcRequest>,
    mut request_rx: mpsc::UnboundedReceiver<RequestWork>,
    mut notification_rx: mpsc::UnboundedReceiver<NotificationWork>,
    response_tx: mpsc::UnboundedSender<RequestResponse>,
) -> AcpWorkers
where
    H: RequestHandler + 'static,
{
    let steering_handler = handler.clone();
    let steering_response_tx = response_tx.clone();
    let steering_worker = Some(tokio::spawn(async move {
        while let Some(request) = steering_rx.recv().await {
            let result = steering_handler
                .handle_request(&request.method, request.params.clone())
                .await;
            let bytes = match result {
                Ok(value) => success_response(request.id, value),
                Err(err) => error_response(Some(request.id), err),
            };
            if steering_response_tx
                .send(RequestResponse {
                    bytes,
                    completed_prompt: None,
                })
                .is_err()
            {
                break;
            }
        }
    }));

    let request_handler = handler.clone();
    let request_worker = Some(tokio::spawn(async move {
        while let Some(work) = request_rx.recv().await {
            let response = match work {
                RequestWork::Request {
                    request,
                    started,
                    prompt_barrier,
                } => {
                    let result = handle_request_and_signal_start(
                        request_handler.as_ref(),
                        &request,
                        started,
                    )
                    .await;
                    let response_bytes = match result {
                        Ok(value) => success_response(request.id, value),
                        Err(err) => error_response(Some(request.id), err),
                    };
                    if let Some(barrier) = prompt_barrier.as_ref() {
                        // Do not advance the per-request FIFO to a later prompt
                        // until every cancel already bound to this exact prompt
                        // has been dispatched. This makes the binding non-sticky
                        // without exposing transport generations to the handler.
                        barrier.dispatch.complete().await;
                    }
                    RequestResponse {
                        bytes: response_bytes,
                        completed_prompt: prompt_barrier,
                    }
                }
                RequestWork::ImmediateResponse(bytes) => RequestResponse {
                    bytes,
                    completed_prompt: None,
                },
            };
            if response_tx.send(response).is_err() {
                break;
            }
        }
    }));

    let notification_handler = handler.clone();
    let notification_worker = Some(tokio::spawn(async move {
        while let Some(work) = notification_rx.recv().await {
            if let Some((started, dispatch)) = work.preceding_prompt {
                wait_for_preceding_prompt(started).await;
                notification_handler
                    .handle_notification(&work.notification.method, work.notification.params)
                    .await;
                dispatch.cancel_dispatched();
            } else {
                notification_handler
                    .handle_notification(&work.notification.method, work.notification.params)
                    .await;
            }
        }
    }));
    AcpWorkers {
        steering: steering_worker,
        request: request_worker,
        notification: notification_worker,
    }
}
