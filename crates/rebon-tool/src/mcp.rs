//! Inward MCP contract shared by tool contexts and runtime consumers.
//! Concrete tools, transports and connection ownership live in the MCP plugin.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use thiserror::Error;

/// MCP state that [`crate::ToolContext`] carries in its extension bag.
///
/// Storage only — read and written through the unchanged `mcp_client()` /
/// `mcp_tool_definitions()` accessors.
#[derive(Clone, Default)]
pub struct McpContext {
    pub client: Option<Arc<dyn McpClient>>,
    pub tool_definitions: Option<Arc<Vec<(String, McpToolDefinition)>>>,
}

/// Registered name of the MCP tool.
pub const MCP_TOOL_NAME: &str = "Mcp";

/// Input passed to [`McpClient::call_tool`].
#[derive(Debug, Clone)]
pub struct McpToolCall {
    /// Logical MCP server name (matches the registered key).
    pub server: String,
    /// Tool name exposed by that server.
    pub name: String,
    /// JSON arguments forwarded to the server.
    pub arguments: Value,
}

/// Result returned from [`McpClient::call_tool`].
#[derive(Debug, Clone)]
pub struct McpToolResult {
    /// Server name (echoed for observability).
    pub server: String,
    /// Tool name (echoed).
    pub name: String,
    /// Structured tool output. The server's exact shape is opaque
    /// — on the wire MCP returns a typed content list, but the
    /// tool layer only needs to round-trip the JSON.
    pub content: Value,
    /// Whether the server flagged the call as an error.
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
    pub destructive: bool,
    pub open_world: bool,
    pub search_hint: Option<String>,
    pub always_load: bool,
}

impl McpToolDefinition {
    pub fn should_defer(&self) -> bool {
        !self.always_load
    }

    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            input_schema: default_mcp_input_schema(),
            read_only: false,
            destructive: false,
            open_world: false,
            search_hint: None,
            always_load: true,
        }
    }
}

pub fn default_mcp_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": true
    })
}

pub fn normalize_name_for_mcp(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn build_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        normalize_name_for_mcp(server_name),
        normalize_name_for_mcp(tool_name)
    )
}

/// Error type surfaced from an [`McpClient`].
#[derive(Debug, Clone, Error)]
pub enum McpClientError {
    /// The requested server is not registered on this client.
    #[error("unknown MCP server: {0}")]
    UnknownServer(String),
    /// The requested tool does not exist on that server.
    #[error("unknown MCP tool `{tool}` on server `{server}`")]
    UnknownTool {
        /// Server name.
        server: String,
        /// Tool name.
        tool: String,
    },
    /// The server returned a permanent / semantic error.
    #[error("MCP call failed: {0}")]
    CallFailed(String),
    /// Transport / connectivity failure.
    #[error("MCP transport error: {0}")]
    Transport(String),
}

/// What a bounded transport close actually managed to finish.
///
/// Naming the servers, rather than returning a bare deadline error, is the
/// difference between an operator knowing which MCP server is wedged and
/// knowing only that something was. The names go into the caller's log or job
/// event; nothing branches on them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpShutdownReport {
    /// Servers whose teardown was observed to finish inside the budget.
    pub closed: Vec<String>,
    /// Servers still being torn down when the budget ran out.
    ///
    /// Their *cancel* half has happened regardless — waiters released,
    /// child processes killed. What overran is only the join, so a name here
    /// means a handle was left falling, never a process left running.
    pub timed_out: Vec<String>,
}

impl McpShutdownReport {
    /// Whether every server closed inside the budget.
    pub fn is_complete(&self) -> bool {
        self.timed_out.is_empty()
    }

    /// Fold another transport's report into this one, for clients that own
    /// several. Both sides stay sorted so a caller's log line is stable.
    pub fn absorb(&mut self, other: McpShutdownReport) {
        self.closed.extend(other.closed);
        self.timed_out.extend(other.timed_out);
        self.closed.sort();
        self.timed_out.sort();
    }
}

/// Wait out a set of already-cancelled server teardowns under one deadline.
///
/// Every transport's bounded close ends the same way: the servers have been
/// cancelled — waiters released, processes killed — and what is left is
/// waiting for the pieces to fall. Two things here are deliberate. The
/// deadline covers the whole set rather than each server in turn, so a budget
/// means what the caller thinks it means instead of being multiplied by the
/// server count. And the joins are polled together, so one server that never
/// falls cannot make the others look like they timed out — sequentially, a
/// single wedged server would report every server behind it as overrun when
/// they would have finished instantly.
pub async fn join_closed_servers<F>(
    servers: Vec<(String, F)>,
    budget: std::time::Duration,
) -> McpShutdownReport
where
    F: std::future::Future<Output = ()>,
{
    use futures_util::stream::{FuturesUnordered, StreamExt};

    let mut names: Vec<Option<String>> = Vec::with_capacity(servers.len());
    let mut pending = FuturesUnordered::new();
    for (index, (name, close)) in servers.into_iter().enumerate() {
        names.push(Some(name));
        pending.push(async move {
            close.await;
            index
        });
    }

    let deadline = tokio::time::Instant::now() + budget;
    let mut report = McpShutdownReport::default();
    while !pending.is_empty() {
        match tokio::time::timeout_at(deadline, pending.next()).await {
            Ok(Some(index)) => {
                if let Some(name) = names[index].take() {
                    report.closed.push(name);
                }
            }
            // The set drained; `is_empty` will end the loop.
            Ok(None) => break,
            Err(_) => break,
        }
    }

    report.timed_out = names.into_iter().flatten().collect();
    report.closed.sort();
    report.timed_out.sort();
    report
}

/// Trait every MCP client implementation satisfies.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// Dispatch a tool call to the named server.
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError>;

    /// List tool names available on the given server. Used by the
    /// tool validation path so invalid inputs fail fast instead of
    /// round-tripping to the server. Implementations that don't
    /// have a cached listing may return `None`.
    async fn list_tools(&self, _server: &str) -> Option<Vec<String>> {
        None
    }

    async fn list_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        self.list_tools(server).await.map(|tools| {
            tools
                .into_iter()
                .map(McpToolDefinition::new)
                .collect::<Vec<_>>()
        })
    }

    fn cached_tool_definitions(&self, _server: &str) -> Option<Vec<McpToolDefinition>> {
        None
    }

    /// Return the logical names of currently-connected MCP servers.
    ///
    /// Used by coordinator-mode prompt generation so worker tool
    /// context can mention the real MCP backends available during
    /// the session.
    fn server_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Drain queued model-facing channel notification XML payloads.
    ///
    /// Transport implementations that support `notifications/claude/channel`
    /// return any pending wrapped XML messages here. The default keeps
    /// existing MCP clients source-compatible and reports no pending work.
    fn drain_channel_notifications(&self) -> Vec<String> {
        Vec::new()
    }

    /// Deterministically tear down transport resources (spawned server
    /// processes, sockets). Sessions that outlive their MCP runtime —
    /// e.g. background workers that build one runtime per prompt —
    /// call this instead of relying on `Drop`, which cannot reach
    /// server-side helper processes. Idempotent; the default is a
    /// no-op for in-memory clients.
    async fn shutdown_transport(&self) {}

    /// [`Self::shutdown_transport`] with a deadline the transport enforces
    /// itself.
    ///
    /// A caller that cannot wait forever used to wrap the unbounded form in
    /// `tokio::time::timeout`. That is what leaves a half-closed transport
    /// behind: dropping the future abandons the teardown wherever it happened
    /// to be, so servers the loop had not reached keep their child processes
    /// and whoever was waiting on an in-flight call is never woken. Only the
    /// owner of the transport can cancel that work instead of abandoning it,
    /// which is why the deadline belongs here and not at the call site.
    ///
    /// The contract is a pair, and the halves are not equally optional.
    /// *Cancel* — releasing every waiter with a transport error and killing
    /// whatever was spawned — must happen inside the budget, always. *Close*
    /// is the join that follows, and is the only part the budget may cut
    /// short. So a name in [`McpShutdownReport::timed_out`] says the cancel
    /// ran and the join did not finish; it never says the work was dropped.
    ///
    /// The default is the unbounded form, which is exactly right for a client
    /// that owns no process or socket and therefore cannot exceed any budget.
    /// An implementation that owns either must override this.
    async fn close_with_timeout(&self, _budget: std::time::Duration) -> McpShutdownReport {
        self.shutdown_transport().await;
        McpShutdownReport::default()
    }

    /// Disconnect one server, tearing its transport down and failing whatever
    /// it still owed. Returns whether this client had a server by that name.
    ///
    /// The per-server counterpart to [`Self::shutdown_transport`], which takes
    /// the whole client down. A session needs this one because one server can
    /// go bad — wedged, or reconfigured — without the others being any less
    /// useful. It is on the trait rather than only on the transports because
    /// that is where it was unreachable: `remove_server` has always existed on
    /// the concrete clients, and every consumer holds an `Arc<dyn McpClient>`.
    async fn disconnect_server(&self, _name: &str) -> bool {
        false
    }

    /// Reconnect one server with the configuration it was started with.
    ///
    /// The old connection goes first, so a reconnect never leaves two of the
    /// same server running. A failure to come back therefore leaves the name
    /// *disconnected* rather than half-attached — the honest outcome, and the
    /// one a caller can retry.
    ///
    /// `Ok(false)` means this client has no such server, which is how an
    /// aggregate knows to keep looking.
    async fn reconnect_server(&self, _name: &str) -> Result<bool, McpClientError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// What a transport hands the bounded join: a name and the teardown left
    /// to wait on. Boxed only so one test can mix a pending future with
    /// finished ones in the same list.
    type ClosingServers = Vec<(
        String,
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    )>;

    /// The bounded join is what makes a budget mean the caller's total wait.
    /// A transport with one server that never finishes closing must still
    /// report the others as closed — sequentially, every server queued behind
    /// the wedged one would be blamed for a timeout it never had a chance to
    /// avoid.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_server_that_never_closes_does_not_make_the_others_look_stuck() {
        let servers: ClosingServers = vec![
            (
                // First in the list, so a sequential join would stop here and
                // never reach the two behind it.
                "wedged".to_string(),
                Box::pin(std::future::pending::<()>()),
            ),
            ("fast-a".to_string(), Box::pin(async {})),
            ("fast-b".to_string(), Box::pin(async {})),
        ];

        let started = std::time::Instant::now();
        let report = join_closed_servers(servers, Duration::from_millis(120)).await;
        let waited = started.elapsed();

        assert_eq!(report.closed, ["fast-a", "fast-b"]);
        assert_eq!(report.timed_out, ["wedged"]);
        assert!(!report.is_complete());
        // The budget bounds the whole set, not each server in turn.
        assert!(
            waited < Duration::from_millis(600),
            "join outran its budget: {waited:?}"
        );
    }

    /// Nothing outstanding is the ordinary case, and it must not wait out the
    /// budget to say so.
    #[tokio::test]
    async fn every_server_closing_reports_complete_without_spending_the_budget() {
        let servers: ClosingServers = vec![
            ("b".to_string(), Box::pin(async {})),
            ("a".to_string(), Box::pin(async {})),
        ];

        let started = std::time::Instant::now();
        let report = join_closed_servers(servers, Duration::from_secs(30)).await;

        assert_eq!(report.closed, ["a", "b"]);
        assert!(report.timed_out.is_empty());
        assert!(report.is_complete());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Absorbing keeps both halves sorted, so a client that owns several
    /// transports logs a stable line rather than one ordered by whichever
    /// transport happened to answer first.
    #[test]
    fn absorbing_another_transports_report_keeps_both_halves_sorted() {
        let mut merged = McpShutdownReport {
            closed: vec!["m".into()],
            timed_out: vec!["z".into()],
        };
        merged.absorb(McpShutdownReport {
            closed: vec!["a".into()],
            timed_out: vec!["b".into()],
        });

        assert_eq!(merged.closed, ["a", "m"]);
        assert_eq!(merged.timed_out, ["b", "z"]);
    }

    /// A client that owns no process cannot exceed a budget, so the default
    /// paired close is the unbounded one and reports nothing outstanding.
    #[tokio::test]
    async fn the_default_paired_close_runs_the_unbounded_shutdown() {
        struct InMemory(std::sync::Arc<std::sync::atomic::AtomicUsize>);

        #[async_trait]
        impl McpClient for InMemory {
            async fn call_tool(&self, _: McpToolCall) -> Result<McpToolResult, McpClientError> {
                unreachable!("the shutdown test never calls a tool")
            }
            async fn shutdown_transport(&self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let shutdowns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let report = InMemory(shutdowns.clone())
            .close_with_timeout(Duration::from_millis(1))
            .await;

        assert_eq!(shutdowns.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(report.is_complete());
    }
}
