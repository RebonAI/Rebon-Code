//! `rebon serve` — the ACP server behind a local web page.
//!
//! The third way to run the same session machinery: `--acp` hands the
//! handler an editor over stdio, the TUI drives it from a terminal, and
//! this serves it to a browser. Nothing here knows how a turn runs. It
//! builds the ACP server every transport shares ([`crate::acp::build_acp_server`]),
//! connects it to an in-memory pipe, and puts a multiplexer on the other
//! end so any number of tabs can be the server's one peer ([`mux`]). The
//! page is `rebon-web-ui`, an ACP client built into the binary
//! ([`assets`]); the HTTP routes ([`http`], [`api`]) carry what ACP has no
//! method for — the reads and levers that make the page the TUI's peer
//! rather than an editor's.
//!
//! The plugin plane comes with the server: the handler boots `kernelPlugins`
//! and wires the plugin tool seat exactly as it does for an editor, and the
//! per-session agent router lets a tab switch a session onto a kernel loop
//! (`kernel:dsh`) through the `agent` config option. A dsh plugin the user
//! composed is therefore reachable from the page without the page knowing
//! it exists.
//!
//! Binding is loopback by default, and every request that is not the page
//! itself needs the per-run token: a WebSocket is not subject to the
//! same-origin policy, so without the token any page the user visits could
//! drive their agent.

mod api;
mod assets;
mod files;
mod hosted;
#[cfg(test)]
mod hosted_tests;
mod http;
mod ipc;
mod mux;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::rebon_config::RuntimeOverride;
use crate::session_agent_router::SessionAgentRouter;
use assets::WebAssets;
use hosted::HostedSessions;
use http::{Guard, HeadError, RequestHead, Response};
use mux::AcpMux;

pub struct ServeArgs {
    pub host: String,
    pub port: u16,
    /// A token to require instead of a generated one.
    pub token: Option<String>,
    /// Open the page in the default browser once listening.
    pub open: bool,
    /// Serve the page from this directory instead of the built-in build.
    pub web_ui: Option<PathBuf>,
    /// Also listen on a local IPC endpoint: `None` not to, `Some(None)` for
    /// the default endpoint, `Some(Some(path))` for an explicit one.
    pub ipc: Option<Option<PathBuf>>,
}

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 7700;

/// The most body a `POST /api/*` may carry.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How often idle sessions are checked for resident transcripts to release.
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How long a session must go untouched before its resident raw transcript is
/// released back to disk. A tab left open keeps its session record alive for
/// the life of the process and every turn appends to that resident copy, so
/// without this a long-running server grows for as long as it runs.
const DEFAULT_SESSION_IDLE_SECS: u64 = 300;

/// The idle threshold, or `None` when sweeping is switched off.
///
/// `REBON_SERVE_SESSION_IDLE_SECS=0` disables it; an unparseable value falls
/// back to the default rather than failing a server that is otherwise fine.
fn session_idle_after() -> Option<Duration> {
    let secs = std::env::var("REBON_SERVE_SESSION_IDLE_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SESSION_IDLE_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// The content security policy the app shell is served with: its own
/// assets and socket only. Inline styles are allowed because the page sets
/// a few (`style=` on a preview swatch); inline scripts are not.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; connect-src 'self' ws: wss:; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; script-src 'self'; font-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// The MCP runtime this server started, and what it was started from.
pub(crate) struct McpRuntime {
    pub client: Option<Arc<dyn rebon_tool::McpClient>>,
    pub warnings: Vec<String>,
    pub runtime_configs: Vec<String>,
    pub strict: bool,
    pub plugin_configs: Vec<crate::mcp_config::PluginMcpConfig>,
}

pub(crate) struct ServeContext {
    pub cwd: PathBuf,
    pub projects_root: PathBuf,
    pub agents: Arc<SessionAgentRouter>,
    pub mux: AcpMux,
    /// The sessions the page has open, and the workers they live in. Every
    /// session a tab opens goes through here; this process hosts none of
    /// them.
    pub hosted: Arc<HostedSessions>,
    pub guard: Guard,
    pub assets: WebAssets,
    pub files: files::FileIndexCache,
    pub handler_state: Arc<rebon_acp::ServerState>,
    pub skill_registry: Arc<rebon_plugin_skill::SkillRegistry>,
    pub agent_registry: Arc<rebon_tool::AgentRegistry>,
    pub task_registry_resolver: rebon_plugin_tasks::TaskRegistryResolver,
    pub mcp: McpRuntime,
    pub runtime_model: rebon_core::query::SharedRuntimeModel,
    pub prune_level: rebon_api::PruneLevelHandle,
}

pub async fn run(overrides: RuntimeOverride, args: ServeArgs) -> anyhow::Result<()> {
    let token = match args.token {
        Some(token) if !token.trim().is_empty() => token,
        Some(_) => anyhow::bail!("--token must not be empty; omit it to have one generated"),
        None => rebon_types::secure_random_hex_token().context("could not generate a token")?,
    };
    let web_ui_dir = args.web_ui.or_else(|| {
        std::env::var_os("REBON_WEB_UI_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let assets = match web_ui_dir {
        Some(dir) => WebAssets::directory(&dir)?,
        None => WebAssets::Embedded,
    };
    if matches!(assets, WebAssets::Embedded) && WebAssets::embedded_is_placeholder() {
        eprintln!(
            "rebon serve: this binary carries no rebon-web-ui build; the page will say how to build one (or pass --web-ui <dir>)"
        );
    }

    let listener = TcpListener::bind((args.host.as_str(), args.port))
        .await
        .with_context(|| format!("failed to bind {}:{}", args.host, args.port))?;
    let local_addr = listener.local_addr()?;
    if !local_addr.ip().is_loopback() {
        tracing::warn!(
            address = %local_addr,
            "rebon serve: listening on a non-loopback address; the token is the only thing between the network and this agent"
        );
    }

    // `rebon serve` is a client of every session it shows, so the handler
    // behind it must not take an active lock: the worker that hosts the
    // session takes it, and a second holder is the thing invariant I1 is
    // there to prevent. `--acp` keeps its ownership — it is a host.
    let runtime_fields = {
        use crate::background::RuntimeFieldsExt;
        crate::background::BackgroundRuntimeFields::from_runtime_override(&overrides)
    };
    let parts = crate::acp::build_acp_server_without_session_ownership(overrides).await?;
    let crate::acp::AcpServerParts {
        handler,
        update_rx,
        permission_rx,
        engine,
        cwd,
        agents,
        skill_registry,
        agent_registry,
        task_registry_resolver,
        mcp_client,
        mcp_warnings,
        mcp_runtime_configs,
        mcp_strict,
        mcp_plugin_configs,
        runtime_model,
        prune_level,
        _cron_scheduler,
    } = parts;
    let handler_state = handler.state().clone();
    let config_options = handler.config_options.clone();
    let config_option_applier = handler.config_option_applier.clone();

    // The server's one peer is the mux, over an in-memory pipe.
    let (mux_side, server_side) = tokio::io::duplex(1 << 20);
    let (server_reader, server_writer) = tokio::io::split(server_side);
    let mut server_task = tokio::spawn(rebon_acp::serve_with_publishers(
        server_reader,
        server_writer,
        handler,
        Some(update_rx),
        Some(permission_rx),
    ));
    let (mux_reader, mux_writer) = tokio::io::split(mux_side);
    let mux = AcpMux::start(mux_reader, mux_writer);
    // The router and the mux hold each other: a client message is offered to
    // the router first, and everything the router learns from an owner's
    // stream is fanned out by the mux.
    let hosted = HostedSessions::new(
        rebon_harness::projects_root(),
        cwd.to_string_lossy().into_owned(),
        runtime_fields,
        config_options,
        config_option_applier,
        handler_state.clone(),
        tokio::runtime::Handle::current(),
    );
    hosted.attach_mux(mux.clone());
    mux.attach_hosted(hosted.clone());

    let ipc_listener = match args.ipc {
        None => None,
        Some(explicit) => {
            let endpoint = explicit.unwrap_or_else(|| ipc::default_endpoint(local_addr.port()));
            Some(ipc::IpcListener::bind(&endpoint)?)
        }
    };

    let guard = Guard::new(&args.host, local_addr.port(), token);
    let url = format!(
        "http://{}:{}/?token={}",
        display_host(&args.host),
        local_addr.port(),
        guard.token()
    );
    eprintln!("rebon serve: listening on http://{local_addr}");
    eprintln!("rebon serve: open {url}");
    eprintln!("rebon serve: workspace {}", cwd.display());
    if let Some(listener) = &ipc_listener {
        eprintln!("rebon serve: ipc {}", listener.endpoint().display());
    }
    if let WebAssets::Directory(dir) = &assets {
        eprintln!("rebon serve: page from {}", dir.display());
    }
    tracing::info!(
        tools = engine.tool_count(),
        address = %local_addr,
        "rebon serve: ACP server behind the web page"
    );
    if args.open {
        if let Err(err) = webbrowser::open(&url) {
            tracing::warn!(error = %err, "rebon serve: could not open a browser; use the URL above");
        }
    }

    let context = Arc::new(ServeContext {
        cwd,
        projects_root: rebon_harness::projects_root(),
        agents,
        mux,
        hosted,
        guard,
        assets,
        files: files::FileIndexCache::default(),
        handler_state,
        skill_registry,
        agent_registry,
        task_registry_resolver,
        mcp: McpRuntime {
            client: mcp_client,
            warnings: mcp_warnings,
            runtime_configs: mcp_runtime_configs,
            strict: mcp_strict,
            plugin_configs: mcp_plugin_configs,
        },
        runtime_model,
        prune_level,
    });

    if let Some(listener) = ipc_listener {
        tokio::spawn(ipc::accept_loop(listener, context.clone()));
    }

    let idle_after = session_idle_after();
    let mut idle_sweep = tokio::time::interval(SESSION_SWEEP_INTERVAL);
    idle_sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    idle_sweep.tick().await;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let context = context.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, context).await {
                                tracing::debug!(%peer, error = %err, "rebon serve: connection ended");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "rebon serve: accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            outcome = &mut server_task => {
                return match outcome {
                    Ok(Ok(())) => anyhow::bail!("rebon serve: the ACP server stopped"),
                    Ok(Err(err)) => Err(err.context("rebon serve: the ACP server failed")),
                    Err(err) => Err(anyhow::anyhow!("rebon serve: the ACP server task panicked: {err}")),
                };
            }
            _ = idle_sweep.tick() => {
                if let Some(idle_after) = idle_after {
                    let swept = context
                        .handler_state
                        .sweep_idle_transcripts(&context.projects_root, idle_after);
                    if !swept.is_empty() {
                        tracing::debug!(
                            sessions = swept.sessions,
                            entries = swept.entries,
                            "rebon serve: released resident transcripts for idle sessions"
                        );
                    }
                    // The second half of this sweep used to give idle sessions
                    // up entirely, because the server held each open session's
                    // active lock and a session it touched once stayed
                    // unresumable in a terminal for the life of the process.
                    // Since stage 4 it holds no locks — the worker does, and
                    // the lease this process drops when the last tab of a
                    // session closes is what starts that worker's linger — so
                    // there is nothing left to release but the memory above.
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("rebon serve: stopping");
                return Ok(());
            }
        }
    }
}

fn display_host(host: &str) -> String {
    match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        other if other.contains(':') && !other.starts_with('[') => format!("[{other}]"),
        other => other.to_string(),
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    context: Arc<ServeContext>,
) -> anyhow::Result<()> {
    // Read the head without consuming it: a WebSocket upgrade has to be
    // finished by the library, which wants to read the request itself.
    let mut buffer = vec![0u8; http::MAX_HEAD_BYTES];
    let head = loop {
        let peeked = tokio::time::timeout(Duration::from_secs(10), stream.peek(&mut buffer))
            .await
            .context("request head did not arrive in time")??;
        if peeked == 0 {
            return Ok(());
        }
        match RequestHead::parse(&buffer[..peeked]) {
            Ok(head) => break head,
            Err(HeadError::Incomplete) if peeked < buffer.len() => {
                // `peek` returns what has arrived; wait for more.
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(HeadError::Incomplete) => {
                return respond_and_close(
                    stream,
                    Response::error(431, "request head too large"),
                    0,
                )
                .await;
            }
            Err(HeadError::Malformed(reason)) => {
                return respond_and_close(stream, Response::error(400, reason), 0).await;
            }
        }
    };

    if !context.guard.host_allowed(head.header("host")) {
        return respond_and_close(
            stream,
            Response::error(403, "this server answers only the address it was opened on"),
            head.head_len,
        )
        .await;
    }

    if head.is_websocket_upgrade() {
        if head.path != "/ws" {
            return respond_and_close(
                stream,
                Response::error(404, "no such socket"),
                head.head_len,
            )
            .await;
        }
        if !context.guard.origin_allowed(head.header("origin")) {
            return respond_and_close(
                stream,
                Response::error(403, "origin not allowed"),
                head.head_len,
            )
            .await;
        }
        if !context.guard.token_presented(&head) {
            return respond_and_close(
                stream,
                Response::error(401, "token required"),
                head.head_len,
            )
            .await;
        }
        return serve_websocket(stream, context).await;
    }

    // A body is read only for an API write; everything else discards it.
    let body = if head.method == "POST" && head.path.starts_with("/api/") {
        let length = head.content_length();
        if length > MAX_BODY_BYTES {
            return respond_and_close(stream, Response::error(413, "body too large"), 0).await;
        }
        let mut raw = vec![0u8; head.head_len + length];
        tokio::time::timeout(Duration::from_secs(30), stream.read_exact(&mut raw))
            .await
            .context("request body did not arrive in time")??;
        Some(raw.split_off(head.head_len))
    } else {
        None
    };
    let consumed = body.as_ref().map(|body| body.len() + head.head_len);

    let response = route(&head, body.as_deref(), &context).await;
    respond_and_close(
        stream,
        response,
        consumed
            .map(|_| 0)
            .unwrap_or(head.head_len + head.content_length()),
    )
    .await
}

/// Consume what the request occupied, write the response, close.
async fn respond_and_close(
    mut stream: TcpStream,
    response: Response,
    consume: usize,
) -> anyhow::Result<()> {
    let mut remaining = consume.min(1 << 20);
    let mut scratch = [0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(scratch.len());
        let read = stream.read(&mut scratch[..want]).await?;
        if read == 0 {
            break;
        }
        remaining -= read;
    }
    stream.write_all(&response.to_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn route(head: &RequestHead, body: Option<&[u8]>, context: &ServeContext) -> Response {
    match head.method.as_str() {
        "GET" | "HEAD" => {}
        "POST" if head.path.starts_with("/api/") => {}
        _ => return Response::error(405, "method not allowed"),
    }
    if head.path == "/healthz" {
        return Response::text(200, "ok");
    }
    if head.path.starts_with("/api/") {
        if !context.guard.token_presented(head) {
            return Response::error(401, "token required");
        }
        return api_route(head, body, context).await;
    }
    match context.assets.get(&head.path) {
        Some(asset) => {
            let response = Response::new(200, asset.content_type, asset.body.into_owned());
            if asset.is_document {
                response.with_header("Content-Security-Policy", CONTENT_SECURITY_POLICY)
            } else {
                response
            }
        }
        None => Response::error(404, "not found"),
    }
}

fn parse_body(body: Option<&[u8]>) -> Result<Value, Response> {
    let body = body.unwrap_or(&[]);
    if body.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_slice::<Value>(body)
        .map_err(|err| Response::error(400, format!("body is not JSON: {err}")))
        .and_then(|value| {
            if value.is_object() {
                Ok(value)
            } else {
                Err(Response::error(400, "body must be a JSON object"))
            }
        })
}

fn action_result<T: serde::Serialize>(result: Result<T, String>) -> Response {
    match result {
        Ok(value) => Response::json(200, &value),
        Err(reason) => Response::error(400, reason),
    }
}

async fn api_route(head: &RequestHead, body: Option<&[u8]>, context: &ServeContext) -> Response {
    let is_post = head.method == "POST";
    let query = |name: &str| head.query.get(name).cloned();
    let cwd = || {
        query("cwd")
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or_else(|| context.cwd.to_string_lossy().into_owned())
    };
    match (head.path.as_str(), is_post) {
        ("/api/info", false) => Response::json(200, &api::server_info(context)),
        ("/api/plugins", false) => Response::json(200, &api::plugin_status(&context.agents).await),
        ("/api/commands", false) => Response::json(200, &api::commands(context)),
        ("/api/skills", false) => Response::json(200, &api::skills(context)),
        ("/api/agents", false) => Response::json(200, &api::agent_definitions(context)),
        ("/api/models", false) => Response::json(200, &api::models(context)),
        ("/api/tasks", false) => {
            let Some(session_id) = query("session") else {
                return Response::error(400, "session is required");
            };
            action_result(api::tasks(context, &session_id))
        }
        ("/api/tasks", true) => match parse_body(body) {
            Ok(body) => {
                let session_id = body["sessionId"]
                    .as_str()
                    .or_else(|| body["session"].as_str());
                match session_id {
                    Some(session_id) if !session_id.is_empty() => {
                        action_result(api::task_action(context, session_id, &body))
                    }
                    _ => Response::error(400, "sessionId is required"),
                }
            }
            Err(response) => response,
        },
        ("/api/mcp", false) => Response::json(200, &api::mcp_status(context).await),
        ("/api/mcp", true) => match parse_body(body) {
            Ok(body) => action_result(api::mcp_action(context, &body).await),
            Err(response) => response,
        },
        ("/api/files", false) => {
            let limit = query("limit")
                .and_then(|limit| limit.parse::<usize>().ok())
                .unwrap_or(40);
            Response::json(
                200,
                &context
                    .files
                    .search(&cwd(), &query("q").unwrap_or_default(), limit),
            )
        }
        ("/api/history", false) => {
            let Some(session_id) = query("session") else {
                return Response::error(400, "session is required");
            };
            match api::session_history(&context.projects_root, &cwd(), &session_id) {
                Ok(history) => Response::json(200, &history),
                Err(reason) => Response::error(404, reason),
            }
        }
        ("/api/usage", false) => {
            let Some(session_id) = query("session") else {
                return Response::error(400, "session is required");
            };
            if !api::valid_session_id(&session_id) {
                return Response::error(400, "not a session id");
            }
            Response::json(200, &api::usage(context, &session_id, &cwd()))
        }
        ("/api/rewind", false) => {
            let Some(session_id) = query("session") else {
                return Response::error(400, "session is required");
            };
            match api::rewind_list(context, &session_id, &cwd()) {
                Ok(list) => Response::json(200, &list),
                Err(reason) => Response::error(404, reason),
            }
        }
        ("/api/rewind", true) => match parse_body(body) {
            Ok(body) => action_result(api::rewind_apply(context, &body)),
            Err(response) => response,
        },
        ("/api/compact", true) => match parse_body(body) {
            Ok(body) => Response::json(200, &api::compact(context, &body)),
            Err(response) => response,
        },
        (_, true) => Response::error(404, "no such endpoint"),
        _ => Response::error(404, "no such endpoint"),
    }
}

async fn serve_websocket(stream: TcpStream, context: Arc<ServeContext>) -> anyhow::Result<()> {
    let socket = tokio_tungstenite::accept_async(stream)
        .await
        .context("WebSocket handshake failed")?;
    let (mut sink, mut source) = socket.split();
    let (client, mut outbound) = context.mux.connect();
    tracing::info!(
        client,
        clients = context.mux.client_count(),
        "rebon serve: page connected"
    );

    let writer = tokio::spawn(async move {
        while let Some(text) = outbound.recv().await {
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(message) = source.next().await {
        match message {
            Ok(Message::Text(text)) => context.mux.on_client_message(client, &text),
            Ok(Message::Binary(bytes)) => match std::str::from_utf8(&bytes) {
                Ok(text) => context.mux.on_client_message(client, text),
                Err(_) => tracing::debug!(client, "rebon serve: ignoring a non-UTF-8 frame"),
            },
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Err(err) => {
                tracing::debug!(client, error = %err, "rebon serve: socket read failed");
                break;
            }
        }
    }

    context.mux.disconnect(client);
    writer.abort();
    tracing::info!(
        client,
        clients = context.mux.client_count(),
        "rebon serve: page disconnected"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_printed_host_is_one_a_browser_can_open() {
        assert_eq!(display_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(display_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(display_host("::1"), "[::1]");
        assert_eq!(display_host("[::1]"), "[::1]");
        assert_eq!(display_host("localhost"), "localhost");
    }

    #[test]
    fn a_post_body_must_be_a_json_object() {
        assert!(parse_body(None).unwrap().is_object());
        assert!(parse_body(Some(b"{\"a\":1}")).unwrap()["a"] == 1);
        assert_eq!(parse_body(Some(b"[1]")).unwrap_err().status, 400);
        assert_eq!(parse_body(Some(b"nope")).unwrap_err().status, 400);
    }

    #[test]
    fn the_policy_keeps_scripts_to_the_page_itself() {
        assert!(CONTENT_SECURITY_POLICY.contains("script-src 'self'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("script-src 'self' 'unsafe"));
        assert!(CONTENT_SECURITY_POLICY.contains("frame-ancestors 'none'"));
    }
}
