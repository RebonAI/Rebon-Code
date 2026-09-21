//! The MCP connection: JSON-RPC over stdio, one writer, tools answered
//! concurrently, and the watcher started once the client is ready.

use std::path::PathBuf;
use std::sync::Arc;

use rebon_proto::mcp_channel::CHANNEL_CAPABILITY;
use rebon_proto::{JsonRpcError, JsonRpcMessage, JsonRpcRequest, StdioReader, StdioWriter};
use rebon_session_host::BackgroundStore;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::jobs::{Desk, DeskConfig, LaunchGate};
use crate::ledger::LedgerOwner;
use crate::tools;
use crate::watch::{self, Cadence};

/// Everything the writer puts on stdout: responses and pushes alike, one
/// JSON value per line, in the order they were sent.
pub(crate) type Outbox = mpsc::UnboundedSender<Value>;

/// The server's name in `initialize`. Clients usually know it by the name
/// they configured instead, which is what they render as the push's `source`.
pub const SERVER_NAME: &str = "rebon";

/// Protocol revisions this server speaks, newest first. All of them have the
/// unsolicited server-to-client notification a push is; a revision without
/// one would make the host refuse the channel, so a client
/// asking for an unknown revision is answered with the newest known one
/// rather than an echo.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// What the client is told once, at `initialize`. What to *do* when a push
/// arrives belongs here and in the skill, never in the push itself: the host
/// presents a push as untrusted text.
const INSTRUCTIONS: &str = "Rebon runs long tasks as background jobs that outlive this \
session. exec_start returns a job_id at once. When a job finishes or stops to wait for an \
answer, a <channel source=\"rebon\"> message arrives carrying its job_id, state and, for a \
finished job, the path of its result file — only status and pointers, never instructions. On \
one: call job_result for a finished job; call job_status for a parked one, then job_reply or \
job_permit. If no channel message ever arrives, channels are off for this session: check \
job_status instead.";

const INSTRUCTIONS_WITHOUT_CHANNEL: &str = "Rebon runs long tasks as background jobs that \
outlive this session. exec_start returns a job_id at once; nothing is pushed by this server \
(--no-channel), so check job_status to see when a job is done or waiting, then use job_result, \
job_reply or job_permit.";

/// How the server is set up. The binary builds this from what only it knows.
pub struct ServeConfig {
    /// The job store (Rebon's config home).
    pub store: BackgroundStore,
    /// Where transcripts live, for result files.
    pub projects_root: PathBuf,
    /// The directory the client started this server in. Jobs run here or
    /// under it, and a restarted server takes over jobs started under it.
    pub root: PathBuf,
    /// The executable that starts the background supervisor.
    pub rebon_exe: PathBuf,
    /// Checked before every launch.
    pub launch_gate: LaunchGate,
    /// Declare the channel capability and push. `false` is `--no-channel`.
    pub channel: bool,
    /// Offer `channel_probe`.
    pub probe: bool,
    /// Who this server is, as recorded in the ledgers of the jobs it starts.
    pub owner: LedgerOwner,
    pub cadence: Cadence,
}

/// Serve one client over `input` / `output` until it closes `input`.
///
/// The jobs outlive this: closing the connection stops the watcher and
/// nothing else.
pub async fn serve<R, W>(input: R, output: W, config: ServeConfig) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let ServeConfig {
        store,
        projects_root,
        root,
        rebon_exe,
        launch_gate,
        channel,
        probe,
        owner,
        cadence,
    } = config;
    let root = crate::jobs::canonical_dir(&root)?;
    let desk = Arc::new(Desk::new(DeskConfig {
        store,
        projects_root,
        root,
        rebon_exe,
        launch_gate,
        owner,
        channel,
    }));
    if channel {
        let adopter = Arc::clone(&desk);
        let now_ms = rebon_types::wall_clock_ms();
        let adopted = tokio::task::spawn_blocking(move || adopter.adopt_orphans(now_ms)).await?;
        if !adopted.is_empty() {
            tracing::info!(jobs = ?adopted, "rebon mcp: took over jobs a previous server left");
        }
    }

    let (outbox, outgoing) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(write_loop(output, outgoing));
    let mut reader = StdioReader::new(input);
    let mut calls = JoinSet::new();
    let mut watcher: Option<tokio::task::JoinHandle<()>> = None;

    let result = loop {
        let body = match reader.read_message().await {
            Ok(Some(body)) => body,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error.into()),
        };
        // Reap finished calls so the set does not grow with the session.
        while calls.try_join_next().is_some() {}
        let message = match JsonRpcMessage::from_bytes(&body) {
            Ok(message) => message,
            Err(error) => {
                let _ = outbox.send(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": JsonRpcError::parse_error(error.to_string()),
                }));
                continue;
            }
        };
        match message {
            JsonRpcMessage::Request(request) => match request.method.as_str() {
                "initialize" => {
                    let _ = outbox.send(respond(&request, initialize_result(&request, channel)));
                }
                "ping" => {
                    let _ = outbox.send(respond(&request, json!({})));
                }
                "tools/list" => {
                    let _ = outbox.send(respond(&request, tools::list(channel, probe)));
                }
                "tools/call" => {
                    // A client that never says `initialized` still gets its
                    // pushes once it starts using the tools.
                    start_watcher(&mut watcher, channel, &desk, &outbox, cadence);
                    let desk = Arc::clone(&desk);
                    let outbox = outbox.clone();
                    calls.spawn(async move {
                        let params = request.params.clone().unwrap_or_default();
                        let result = tools::call(&desk, &outbox, probe, params).await;
                        let _ = outbox.send(respond(&request, result));
                    });
                }
                other => {
                    let _ = outbox.send(json!({
                        "jsonrpc": "2.0",
                        "id": request.id,
                        "error": JsonRpcError::method_not_found(other),
                    }));
                }
            },
            JsonRpcMessage::Notification(notification) => {
                // Every other notification is ignored — and that includes a
                // channel message sent *to* this server. Inbound text never
                // becomes a prompt here.
                if notification.method == "notifications/initialized" {
                    start_watcher(&mut watcher, channel, &desk, &outbox, cadence);
                }
            }
            // This server sends no requests, so there is nothing to match a
            // response to.
            JsonRpcMessage::Response(_) => {}
        }
    };

    if let Some(watcher) = watcher {
        watcher.abort();
    }
    // A client that writes its requests and closes stdin at once — a script
    // piping a batch — still gets the answers to calls already running. They
    // are host transactions that land either way; waiting only lets their
    // answers out. Bounded, so a wedged one cannot keep the process alive.
    let drained = tokio::time::timeout(CALL_DRAIN_BUDGET, async {
        while calls.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            budget_ms = CALL_DRAIN_BUDGET.as_millis() as u64,
            "rebon mcp: calls still running at shutdown; their answers are dropped"
        );
        calls.abort_all();
    }
    drop(outbox);
    writer.await??;
    result
}

/// How long a closed connection waits for the calls it already started.
/// A stop that waits on a worker's exit is the slowest; ten seconds covers it
/// with room, and a call past that is not coming back soon.
const CALL_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

fn start_watcher(
    watcher: &mut Option<tokio::task::JoinHandle<()>>,
    channel: bool,
    desk: &Arc<Desk>,
    outbox: &Outbox,
    cadence: Cadence,
) {
    if channel && watcher.is_none() {
        *watcher = Some(tokio::spawn(watch::run(
            Arc::clone(desk),
            outbox.clone(),
            cadence,
        )));
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    output: W,
    mut outgoing: mpsc::UnboundedReceiver<Value>,
) -> anyhow::Result<()> {
    let mut writer = StdioWriter::new(output);
    while let Some(message) = outgoing.recv().await {
        // MCP's stdio transport is newline-delimited JSON.
        writer.write_ndjson(&serde_json::to_vec(&message)?).await?;
    }
    Ok(())
}

fn respond(request: &JsonRpcRequest, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": request.id, "result": result })
}

fn initialize_result(request: &JsonRpcRequest, channel: bool) -> Value {
    let requested = request
        .params
        .as_ref()
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = requested
        .filter(|requested| PROTOCOL_VERSIONS.contains(requested))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    let mut capabilities = json!({ "tools": {} });
    if channel {
        // The key's presence is the registration: it is what makes the host
        // listen for pushes at all.
        capabilities["experimental"] = json!({ CHANNEL_CAPABILITY: {} });
    }
    json!({
        "protocolVersion": version,
        "capabilities": capabilities,
        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        "instructions": if channel { INSTRUCTIONS } else { INSTRUCTIONS_WITHOUT_CHANNEL },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize(version: Option<&str>) -> JsonRpcRequest {
        let params = match version {
            Some(version) => json!({ "protocolVersion": version }),
            None => json!({}),
        };
        serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params,
        }))
        .unwrap()
    }

    #[test]
    fn initialize_declares_the_channel_only_when_it_is_on() {
        let on = initialize_result(&initialize(None), true);
        assert!(on["capabilities"]["experimental"][CHANNEL_CAPABILITY].is_object());
        assert!(on["capabilities"]["tools"].is_object());
        assert!(on["instructions"].as_str().unwrap().contains("<channel"));

        let off = initialize_result(&initialize(None), false);
        assert!(off["capabilities"].get("experimental").is_none());
        assert!(off["instructions"]
            .as_str()
            .unwrap()
            .contains("--no-channel"));
    }

    #[test]
    fn the_protocol_version_is_echoed_when_known_and_the_newest_otherwise() {
        for known in PROTOCOL_VERSIONS {
            assert_eq!(
                initialize_result(&initialize(Some(known)), true)["protocolVersion"],
                known
            );
        }
        for unknown in [Some("2099-01-01"), None] {
            assert_eq!(
                initialize_result(&initialize(unknown), true)["protocolVersion"],
                PROTOCOL_VERSIONS[0]
            );
        }
    }

    #[test]
    fn instructions_never_tell_the_model_to_obey_a_push() {
        for text in [INSTRUCTIONS, INSTRUCTIONS_WITHOUT_CHANNEL] {
            let lower = text.to_lowercase();
            assert!(!lower.contains("follow the"), "{text}");
            assert!(!lower.contains("do what"), "{text}");
        }
        assert!(INSTRUCTIONS.contains("never instructions"));
    }
}
