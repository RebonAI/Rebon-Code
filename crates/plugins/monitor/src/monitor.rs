use async_trait::async_trait;
use rebon_tool::bash::{
    command_shell, configured_shell_command, shell_permission_decision, CommandShell,
};
use rebon_tool::monitor::{execution_error, invalid_input, WebSocketTarget};
use rebon_tool::{MonitorTaskSource, Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use tokio::net::lookup_host;
use url::Url;

#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
pub use rebon_tool::MONITOR_TOOL_NAME;
#[cfg(test)]
use rebon_tool::{
    MonitorEventDisposition, MonitorRegistry, MonitorTaskCompletion, MonitorTaskCompletionStatus,
    MonitorTaskSpec, TaskRuntimeController,
};
#[cfg(test)]
use rebon_types::PromptCancel;
#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(test)]
use tokio::time::{sleep, Duration};
#[cfg(test)]
use tokio_tungstenite::tungstenite::{
    http::header::SEC_WEBSOCKET_PROTOCOL, http::HeaderValue, Message,
};
const INVALID_INPUT_CODE: i64 = 400;
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

const DESCRIPTION_INTRO: &str = "Start a background monitor that streams events from a long-running command or WebSocket. Each stdout line or WebSocket text frame is an event; you keep working and notifications arrive in the chat. Events are asynchronous task notifications, not user replies.\n\
\n\
Use Monitor for a selective stream of events that could change what you do next, or to wait on an external condition (a port answering, a file appearing, a CI status changing). To wait for a process you started yourself to finish, use Bash run_in_background instead. For recurring full prompts on a coarse schedule, use /loop. Agent completion is already delivered automatically: never use Monitor, Sleep, or TaskList to poll an Agent.";

// The command section is chosen per interpreter: a model driving a
// PowerShell session on Windows otherwise cannot tell which grammar
// `command` takes, and falls back to writing a script file and running
// that instead of an inline watcher.
const POSIX_COMMAND_GUIDE: &str = "Put the whole watcher inline in `command`: pipes, loops and multi-line scripts all work, so never write a script file first. Patterns:\n\
- Filter a log: tail -n 0 -F app.log | grep --line-buffered -E 'ERROR|FAIL|panicked'\n\
- Wait for a condition, then end: until curl -sf http://localhost:3000/health >/dev/null; do sleep 2; done; echo ready\n\
- Report changes from a poll: prev=; while :; do cur=$(<status command> 2>&1 || true); if [ \"$cur\" != \"$prev\" ]; then echo \"$cur\"; prev=$cur; fi; sleep 30; done\n\
\n\
Rules: stdout is the event stream (add 2>&1 when stderr matters). grep, sed and awk buffer inside a pipe and delay events by minutes, so use grep --line-buffered, sed -u or awk with fflush(). Match failure signatures as well as success, or a crash looks like silence. In poll loops, tolerate transient failures with || true and sleep 1-5 s for local checks, 30 s or more for remote APIs. Process exit ends the monitor, so an until-loop that echoes once is a one-shot wait.";

const GIT_BASH_NOTE: &str = "`command` runs in Git Bash (bash -c), not PowerShell, in the session's working directory. Use POSIX syntax and forward-slash paths (C:/dir or /c/dir); for a cmdlet, call powershell.exe -NoProfile -Command '...' from inside the script.";

const POSIX_NOTE: &str =
    "`command` runs in sh -lc (POSIX shell) in the session's working directory.";

const POWERSHELL_COMMAND_GUIDE: &str = "`command` runs in Windows PowerShell (powershell.exe -NoProfile -Command) in the session's working directory; no POSIX shell is installed. Put the whole watcher inline in `command`: pipelines, loops and multi-line scripts all work, so never write a script file first. Patterns:\n\
- Filter a log: Get-Content app.log -Tail 0 -Wait | Select-String -Pattern 'ERROR|FAIL' | ForEach-Object { $_.Line }\n\
- Wait for a condition, then end: while (-not (Test-Path out/done.flag)) { Start-Sleep -Seconds 2 }; 'ready'\n\
- Report changes from a poll: $prev = $null; while ($true) { $cur = try { <status command> 2>&1 | Out-String } catch { \"$_\" }; if ($cur -ne $prev) { $cur.Trim(); $prev = $cur }; Start-Sleep -Seconds 30 }\n\
\n\
Rules: the success output stream is the event stream (merge errors with 2>&1 when they matter). Emit plain strings, one per event. Match failure signatures as well as success, or a crash looks like silence. In poll loops, catch transient failures and sleep 1-5 s for local checks, 30 s or more for remote APIs. Process exit ends the monitor, so a loop that prints once and exits is a one-shot wait.";

const DESCRIPTION_OUTRO: &str = "Every emitted line becomes a notification, so emit only actionable lines. Noisy monitors are suppressed and may be stopped automatically; restart with a tighter filter. The default deadline is 10 minutes. persistent=true removes that deadline for the current session; stop it with TaskStop.";

fn description_for(shell: CommandShell) -> String {
    let command = match shell {
        CommandShell::Posix => format!("{POSIX_NOTE} {POSIX_COMMAND_GUIDE}"),
        CommandShell::GitBash => format!("{GIT_BASH_NOTE} {POSIX_COMMAND_GUIDE}"),
        CommandShell::WindowsPowerShell => POWERSHELL_COMMAND_GUIDE.to_string(),
    };
    format!("{DESCRIPTION_INTRO}\n\n{command}\n\n{DESCRIPTION_OUTRO}")
}

fn tool_description() -> &'static str {
    static DESCRIPTION: OnceLock<String> = OnceLock::new();
    DESCRIPTION.get_or_init(|| description_for(command_shell()))
}

#[derive(Debug, Clone, Default)]
pub struct MonitorTool;

#[derive(Debug, Clone)]
struct MonitorInput {
    description: String,
    source: MonitorSource,
    timeout_ms: Option<u64>,
    persistent: bool,
}

#[derive(Debug, Clone)]
enum MonitorSource {
    Command(String),
    WebSocket(WebSocketTarget),
}

#[async_trait]
impl Tool for MonitorTool {
    fn id(&self) -> ToolId {
        ToolId::new(MONITOR_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["MonitorTool"]
    }

    fn description(&self) -> &str {
        tool_description()
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Inline shell script (may span several lines) whose stdout is the event stream. Each line is one event; process exit ends the monitor. Cannot be combined with ws."
                },
                "ws": {
                    "type": "string",
                    "description": "WebSocket to open. Each text frame is one event; binary frames are reported as placeholders. Socket close ends the monitor. Cannot be combined with command."
                },
                "description": {
                    "type": "string",
                    "description": "Short, specific human-readable description shown in notifications."
                },
                "subprotocols": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional RFC 6455 subprotocols for a WebSocket monitor."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "default": DEFAULT_TIMEOUT_MS,
                    "description": "Stop the monitor after this deadline. Defaults to 600000 ms and is ignored when persistent is true."
                },
                "persistent": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run for the lifetime of this session without a timeout. Stop with TaskStop."
                }
            },
            "required": ["description"],
            "additionalProperties": false
        })
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("watch streaming logs websocket events wait until condition poll status background notifications")
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match parse_input(input) {
            Ok(_) => Ok(ValidationOutcome::valid()),
            Err(reason) => Ok(ValidationOutcome::invalid(reason, INVALID_INPUT_CODE)),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let parsed = parse_input(input).map_err(|reason| invalid_input(reason))?;
        match parsed.source {
            MonitorSource::Command(command) => Ok(shell_permission_decision(
                input,
                &command,
                context,
                false,
                MONITOR_TOOL_NAME,
            )),
            MonitorSource::WebSocket(target) => Ok(PermissionDecision::ask(
                PermissionRequest::new(
                    format!("Monitor {}", target.host),
                    format!(
                        "Monitor wants to open a WebSocket to {}",
                        target.redacted_target
                    ),
                )
                .with_options(["allow_once", "allow_always", "reject_once"])
                .with_metadata(json!({
                    "kind": "monitor_websocket",
                    "permissionRule": format!("domain:{}", target.host),
                    "host": target.host,
                })),
                Some(input.clone()),
            )),
        }
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input).map_err(invalid_input)?;
        let controller = context
            .task_runtime_controller()
            .cloned()
            .ok_or_else(|| execution_error("task runtime controller is unavailable"))?;

        match parsed.source {
            MonitorSource::Command(command) => {
                if let Some(sandbox) = context.command_sandbox() {
                    sandbox.check(&self.id(), &command, false)?;
                }
                let shell_registry = context
                    .shell_process_registry()
                    .ok_or_else(|| execution_error("background shell registry is unavailable"))?;
                let process = configured_shell_command(&command, context.cwd());
                let task_id = shell_registry
                    .spawn_monitor(
                        context,
                        process,
                        command,
                        parsed.description.clone(),
                        "shell command (redacted)".to_string(),
                        parsed.timeout_ms,
                    )
                    .await
                    .map_err(|_| {
                        execution_error(
                            "Monitor: pre-spawn error (cwd/argv redacted); command was not started",
                        )
                    })?;
                Ok(started_result(
                    task_id,
                    MonitorTaskSource::Command,
                    parsed.description,
                    parsed.persistent,
                    parsed.timeout_ms,
                ))
            }
            MonitorSource::WebSocket(target) => {
                let addresses = resolve_and_validate_target(&target).await?;
                let registry = context
                    .monitor_registry()
                    .ok_or_else(|| execution_error("monitor registry is unavailable"))?;
                let task_id = registry.spawn_websocket(
                    context,
                    controller,
                    target,
                    addresses,
                    parsed.description.clone(),
                    parsed.timeout_ms,
                )?;
                Ok(started_result(
                    task_id,
                    MonitorTaskSource::WebSocket,
                    parsed.description,
                    parsed.persistent,
                    parsed.timeout_ms,
                ))
            }
        }
    }
}

fn started_result(
    task_id: String,
    source: MonitorTaskSource,
    description: String,
    persistent: bool,
    timeout_ms: Option<u64>,
) -> Value {
    json!({
        "taskId": task_id,
        "status": "started",
        "source": source.as_str(),
        "description": description,
        "persistent": persistent,
        "timeout_ms": timeout_ms,
        "note": "Events and the monitor's exit arrive as task notifications. Keep working; do not poll this task with ShellOutput or Sleep. Stop it with TaskStop.",
    })
}

fn parse_input(input: &Value) -> Result<MonitorInput, String> {
    let object = input
        .as_object()
        .ok_or_else(|| "Monitor input must be an object".to_string())?;
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .ok_or_else(|| "description must be a non-empty string".to_string())?
        .to_string();
    let command = optional_non_empty_string(object.get("command"), "command")?;
    let ws = optional_non_empty_string(object.get("ws"), "ws")?;
    let source = match (command, ws) {
        (Some(command), None) => {
            if command.contains("${user_config.") {
                return Err("monitor command must not reference ${user_config.*}".to_string());
            }
            if object.contains_key("subprotocols") {
                return Err("subprotocols can only be used with ws".to_string());
            }
            MonitorSource::Command(command)
        }
        (None, Some(ws)) => MonitorSource::WebSocket(parse_websocket_target(
            &ws,
            parse_subprotocols(object.get("subprotocols"))?,
        )?),
        _ => return Err("provide exactly one of command or ws".to_string()),
    };
    let persistent = optional_bool(object.get("persistent"), "persistent")?.unwrap_or(false);
    let requested_timeout = match object.get("timeout_ms") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|value| *value > 0)
                .ok_or_else(|| "timeout_ms must be an integer >= 1".to_string())?,
        ),
    };
    let timeout_ms = if persistent {
        None
    } else {
        Some(requested_timeout.unwrap_or(DEFAULT_TIMEOUT_MS))
    };
    Ok(MonitorInput {
        description,
        source,
        timeout_ms,
        persistent,
    })
}

fn optional_non_empty_string(value: Option<&Value>, field: &str) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() {
                Err(format!("{field} must be non-empty when provided"))
            } else {
                Ok(Some(value.to_string()))
            }
        }
        Some(_) => Err(format!("{field} must be a string when provided")),
    }
}

fn optional_bool(value: Option<&Value>, field: &str) -> Result<Option<bool>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{field} must be a boolean when provided")),
    }
}

fn parse_subprotocols(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let protocols = value
        .as_array()
        .ok_or_else(|| "subprotocols must be an array of RFC 6455 tokens".to_string())?;
    let mut parsed = Vec::with_capacity(protocols.len());
    let mut unique = HashSet::with_capacity(protocols.len());
    for value in protocols {
        let protocol = value
            .as_str()
            .ok_or_else(|| "each subprotocol must be a string".to_string())?;
        if !is_rfc6455_token(protocol) {
            return Err(format!(
                "subprotocol `{protocol}` must be an RFC 6455 token"
            ));
        }
        if !unique.insert(protocol.to_string()) {
            return Err("subprotocols must be unique".to_string());
        }
        parsed.push(protocol.to_string());
    }
    Ok(parsed)
}

fn is_rfc6455_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn parse_websocket_target(raw: &str, subprotocols: Vec<String>) -> Result<WebSocketTarget, String> {
    if !raw.is_ascii()
        || raw
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(
            "ws must be a valid ASCII ws:// or wss:// URL with no whitespace or control characters"
                .to_string(),
        );
    }
    let url = Url::parse(raw).map_err(|error| format!("invalid WebSocket URL: {error}"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err("ws URL must use ws:// or wss://".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("ws URL must not contain userinfo".to_string());
    }
    if url.fragment().is_some() {
        return Err("ws URL must not contain a fragment".to_string());
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "ws URL must include a host".to_string())?
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "ws URL must include a valid port".to_string())?;
    let host_display = match url.host() {
        Some(url::Host::Ipv6(address)) => format!("[{address}]"),
        _ => host.clone(),
    };
    let default_port = if url.scheme() == "wss" { 443 } else { 80 };
    let redacted_target = if port == default_port {
        format!("{}://{host_display}", url.scheme())
    } else {
        format!("{}://{host_display}:{port}", url.scheme())
    };
    Ok(WebSocketTarget {
        url,
        host,
        port,
        subprotocols,
        redacted_target,
    })
}

async fn resolve_and_validate_target(target: &WebSocketTarget) -> ToolResult<Vec<SocketAddr>> {
    let mut addresses = lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|_| invalid_input("ws host could not resolve"))?
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(invalid_input("ws host could not resolve"));
    }
    if let Some(address) = addresses
        .iter()
        .find(|address| !websocket_address_is_allowed(address.ip()))
    {
        return Err(invalid_input(format!(
            "ws resolves to an SSRF-blocked address range ({})",
            address.ip()
        )));
    }
    Ok(addresses)
}

fn websocket_address_is_allowed(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_address_is_allowed(address),
        IpAddr::V6(address) => ipv6_address_is_allowed(address),
    }
}

fn ipv4_address_is_allowed(address: Ipv4Addr) -> bool {
    if address.is_loopback() {
        return true;
    }
    let value = u32::from(address);
    ![
        (0x0000_0000, 8),
        (0x0a00_0000, 8),
        (0x6440_0000, 10),
        (0xa9fe_0000, 16),
        (0xac10_0000, 12),
        (0xc000_0000, 24),
        (0xc000_0200, 24),
        (0xc058_6300, 24),
        (0xc0a8_0000, 16),
        (0xc612_0000, 15),
        (0xc633_6400, 24),
        (0xcb00_7100, 24),
        (0xe000_0000, 4),
        (0xf000_0000, 4),
    ]
    .into_iter()
    .any(|(network, prefix)| ipv4_in_prefix(value, network, prefix))
}

fn ipv4_in_prefix(address: u32, network: u32, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    address & mask == network & mask
}

fn ipv6_address_is_allowed(address: Ipv6Addr) -> bool {
    if address.is_loopback() {
        return true;
    }
    if address.is_unspecified() || address.is_multicast() {
        return false;
    }
    let segments = address.segments();
    if segments[..5] == [0, 0, 0, 0, 0] && matches!(segments[5], 0 | 0xffff) {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return ipv4_address_is_allowed(embedded);
    }
    if segments[..6] == [0, 0, 0, 0, 0xffff, 0] {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return ipv4_address_is_allowed(embedded);
    }
    if segments[0] == 0x2002 {
        let embedded = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return ipv4_address_is_allowed(embedded);
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return ipv4_address_is_allowed(embedded);
    }

    let value = u128::from(address);
    if !ipv6_in_prefix(
        value,
        u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)),
        3,
    ) {
        return false;
    }

    ![
        (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
        (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2620, 0x004f, 0x8000, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    ]
    .into_iter()
    .any(|(network, prefix)| ipv6_in_prefix(value, u128::from(network), prefix))
}

fn ipv6_in_prefix(address: u128, network: u128, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    address & mask == network & mask
}

#[cfg(test)]
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tools_core::{PermissionBehavior, ToolErrorPresentation};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, accept_hdr_async};

    #[derive(Default)]
    struct RecordingController {
        started: Mutex<Vec<MonitorTaskSpec>>,
        cancels: Mutex<Vec<PromptCancel>>,
        events: Mutex<Vec<(String, String)>>,
        finished: Mutex<Vec<MonitorTaskCompletion>>,
        disposition: Mutex<Option<MonitorEventDisposition>>,
    }

    #[async_trait]
    impl TaskRuntimeController for RecordingController {
        async fn stop_task(
            &self,
            _session_id: &str,
            _task_id: &str,
        ) -> Result<rebon_tool::StopTaskOutcome, String> {
            Ok(rebon_tool::StopTaskOutcome::NotFound)
        }

        async fn send_message_to_task(
            &self,
            _session_id: &str,
            _task_id: &str,
            _message: String,
        ) -> Result<(), ToolErrorPresentation> {
            Ok(())
        }

        fn monitor_started(&self, _session_id: &str, spec: MonitorTaskSpec, cancel: PromptCancel) {
            lock(&self.started).push(spec);
            lock(&self.cancels).push(cancel);
        }

        fn monitor_event(
            &self,
            _session_id: &str,
            task_id: &str,
            event: String,
        ) -> MonitorEventDisposition {
            lock(&self.events).push((task_id.to_string(), event));
            (*lock(&self.disposition)).unwrap_or(MonitorEventDisposition::Queued)
        }

        fn monitor_finished(&self, _session_id: &str, completion: MonitorTaskCompletion) {
            lock(&self.finished).push(completion);
        }
    }

    fn websocket_context(controller: Arc<RecordingController>) -> ToolContext {
        ToolContext::new()
            .with_session_id("monitor-test")
            .with_task_runtime_controller(controller)
            .with_monitor_registry(Arc::new(MonitorRegistry::new()))
    }

    async fn wait_for_finished(controller: &RecordingController) -> MonitorTaskCompletion {
        for _ in 0..250 {
            if let Some(completion) = lock(&controller.finished).first().cloned() {
                return completion;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("WebSocket monitor did not finish");
    }

    fn websocket_input(address: SocketAddr) -> Value {
        json!({
            "ws": format!("ws://{address}/events"),
            "description": "test events",
            "timeout_ms": 5_000
        })
    }

    fn valid_command() -> Value {
        json!({
            "command": "tail -f app.log",
            "description": "application errors"
        })
    }

    #[test]
    fn description_names_the_shell_that_runs_the_command() {
        let git_bash = description_for(CommandShell::GitBash);
        assert!(git_bash.contains("runs in Git Bash"));
        assert!(git_bash.contains("not PowerShell"));
        assert!(git_bash.contains("grep --line-buffered"));

        let posix = description_for(CommandShell::Posix);
        assert!(posix.contains("sh -lc"));
        assert!(!posix.contains("PowerShell"));

        let powershell = description_for(CommandShell::WindowsPowerShell);
        assert!(powershell.contains("runs in Windows PowerShell"));
        assert!(powershell.contains("Get-Content app.log -Tail 0 -Wait"));
        assert!(!powershell.contains("grep --line-buffered"));

        for description in [git_bash, posix, powershell] {
            assert!(description.contains("never write a script file first"));
            assert!(description.contains("Wait for a condition, then end"));
            assert!(description.contains("never use Monitor, Sleep, or TaskList to poll an Agent"));
        }
    }

    #[test]
    fn started_result_says_events_are_pushed_not_polled() {
        let result = started_result(
            "m1".into(),
            MonitorTaskSource::Command,
            "events".into(),
            false,
            Some(DEFAULT_TIMEOUT_MS),
        );
        let note = result["note"].as_str().unwrap();
        assert!(note.contains("task notifications"));
        assert!(note.contains("do not poll"));
        assert!(note.contains("TaskStop"));
    }

    #[test]
    fn description_matches_this_machine() {
        assert_eq!(
            MonitorTool.description(),
            description_for(command_shell()).as_str()
        );
    }

    #[test]
    fn requires_exactly_one_source() {
        for input in [
            json!({"description": "events"}),
            json!({"command": "echo hi", "ws": "ws://localhost", "description": "events"}),
        ] {
            assert!(parse_input(&input).unwrap_err().contains("exactly one"));
        }
        assert!(parse_input(&valid_command()).is_ok());
    }

    #[test]
    fn requires_non_empty_description() {
        for description in [Value::Null, Value::String("  ".to_string())] {
            let input = json!({"command": "echo hi", "description": description});
            assert!(parse_input(&input).is_err());
        }
    }

    #[test]
    fn defaults_to_ten_minutes_and_persistent_ignores_timeout() {
        let defaulted = parse_input(&valid_command()).unwrap();
        assert_eq!(defaulted.timeout_ms, Some(DEFAULT_TIMEOUT_MS));
        let persistent = parse_input(&json!({
            "command": "tail -f app.log",
            "description": "errors",
            "timeout_ms": 5,
            "persistent": true
        }))
        .unwrap();
        assert_eq!(persistent.timeout_ms, None);
        assert!(persistent.persistent);
    }

    #[test]
    fn rejects_user_config_expansion() {
        let input = json!({
            "command": "echo ${user_config.secret}",
            "description": "events"
        });
        assert!(parse_input(&input).unwrap_err().contains("user_config"));
    }

    #[test]
    fn subprotocols_are_websocket_only_tokens_and_unique() {
        assert!(parse_input(&json!({
            "command": "echo hi",
            "description": "events",
            "subprotocols": []
        }))
        .is_err());
        for protocols in [json!(["chat protocol"]), json!(["chat", "chat"])] {
            let input = json!({
                "ws": "ws://localhost/events",
                "description": "events",
                "subprotocols": protocols
            });
            assert!(parse_input(&input).is_err());
        }
        assert!(parse_input(&json!({
            "ws": "ws://localhost/events",
            "description": "events",
            "subprotocols": ["chat.v2", "json"]
        }))
        .is_ok());
    }

    #[test]
    fn websocket_url_rejects_non_ascii_userinfo_whitespace_and_wrong_scheme() {
        for ws in [
            "https://example.com/events",
            "ws://user:secret@example.com/events",
            "ws://example.com/a b",
            "ws://例子.测试/events",
            "ws://example.com/events#fragment",
        ] {
            let input = json!({"ws": ws, "description": "events"});
            assert!(parse_input(&input).is_err(), "{ws}");
        }
    }

    #[test]
    fn redacted_target_omits_path_query_and_user_data() {
        let parsed = parse_input(&json!({
            "ws": "wss://Example.COM:8443/private?token=secret",
            "description": "events"
        }))
        .unwrap();
        let MonitorSource::WebSocket(target) = parsed.source else {
            panic!("expected WebSocket")
        };
        assert_eq!(target.redacted_target, "wss://example.com:8443");
        assert!(!target.redacted_target.contains("secret"));
        assert!(!target.redacted_target.contains("private"));
    }

    #[test]
    fn loopback_is_allowed_for_local_development() {
        assert!(websocket_address_is_allowed(IpAddr::V4(
            Ipv4Addr::LOCALHOST
        )));
        assert!(websocket_address_is_allowed(IpAddr::V6(
            Ipv6Addr::LOCALHOST
        )));
    }

    #[test]
    fn private_link_local_metadata_and_multicast_are_blocked() {
        for address in [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
            IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
            IpAddr::V6("fe80::1".parse().unwrap()),
            IpAddr::V6("fc00::1".parse().unwrap()),
            IpAddr::V6("ff02::1".parse().unwrap()),
        ] {
            assert!(!websocket_address_is_allowed(address), "{address}");
        }
    }

    #[test]
    fn embedded_private_ipv4_is_blocked() {
        for address in [
            "::ffff:192.168.1.1",
            "::ffff:0:169.254.169.254",
            "64:ff9b::a00:1",
            "2002:0a00:0001::",
        ] {
            let address = IpAddr::V6(address.parse().unwrap());
            assert!(!websocket_address_is_allowed(address), "{address}");
        }
    }

    #[test]
    fn special_ipv6_ranges_are_blocked() {
        for address in [
            "100::1",
            "2001::1",
            "2001:20::1",
            "2001:db8::1",
            "2620:4f:8000::1",
            "3fff::1",
            "5f00::1",
            "fec0::1",
        ] {
            let address = IpAddr::V6(address.parse().unwrap());
            assert!(!websocket_address_is_allowed(address), "{address}");
        }
    }

    #[test]
    fn public_embedded_ipv4_is_allowed() {
        for address in [
            "::ffff:8.8.8.8",
            "::ffff:0:8.8.8.8",
            "64:ff9b::808:808",
            "2002:0808:0808::",
        ] {
            let address = IpAddr::V6(address.parse().unwrap());
            assert!(websocket_address_is_allowed(address), "{address}");
        }
    }

    #[test]
    fn ordinary_public_addresses_are_allowed() {
        assert!(websocket_address_is_allowed(IpAddr::V4(Ipv4Addr::new(
            8, 8, 8, 8
        ))));
        assert!(websocket_address_is_allowed(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        )));
    }

    #[tokio::test]
    async fn websocket_runtime_streams_text_binary_ping_and_close() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            socket.send(Message::Text("ready".into())).await.unwrap();
            socket
                .send(Message::Binary(vec![1, 2, 3].into()))
                .await
                .unwrap();
            socket.send(Message::Ping(vec![9].into())).await.unwrap();
            let pong = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(matches!(pong, Message::Pong(_)));
            socket.send(Message::Close(None)).await.unwrap();
        });
        let controller = Arc::new(RecordingController::default());
        let context = websocket_context(controller.clone());
        let result = MonitorTool
            .call(websocket_input(address), &context)
            .await
            .unwrap();
        let completion = wait_for_finished(&controller).await;
        server.await.unwrap();

        assert_eq!(result["status"], "started");
        assert_eq!(result["source"], "websocket");
        assert_eq!(completion.status, MonitorTaskCompletionStatus::Closed);
        let events = lock(&controller.events)
            .iter()
            .map(|(_, event)| event.clone())
            .collect::<Vec<_>>();
        assert_eq!(events, vec!["ready", "[3-byte binary WebSocket frame]"]);
    }

    #[tokio::test]
    async fn websocket_runtime_sends_requested_subprotocols() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(stream, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(
                    request.headers().get(SEC_WEBSOCKET_PROTOCOL).unwrap(),
                    "chat.v2, json"
                );
                response.headers_mut().insert(
                    SEC_WEBSOCKET_PROTOCOL,
                    HeaderValue::from_static("chat.v2"),
                );
                Ok(response)
            })
            .await
            .unwrap();
            socket.send(Message::Close(None)).await.unwrap();
        });
        let controller = Arc::new(RecordingController::default());
        let context = websocket_context(controller.clone());
        let mut input = websocket_input(address);
        input["subprotocols"] = json!(["chat.v2", "json"]);
        MonitorTool.call(input, &context).await.unwrap();
        let completion = wait_for_finished(&controller).await;
        server.await.unwrap();
        assert_eq!(completion.status, MonitorTaskCompletionStatus::Closed);
    }

    #[tokio::test]
    async fn websocket_runtime_times_out_and_can_be_cancelled() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (timeout_ready_tx, timeout_ready_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let _ = timeout_ready_tx.send(());
            let _ = socket.next().await;
        });
        let timeout_controller = Arc::new(RecordingController::default());
        let timeout_context = websocket_context(timeout_controller.clone());
        let input = json!({
            "ws": format!("ws://{address}/events"),
            "description": "timeout events",
            "timeout_ms": 1_000
        });
        MonitorTool.call(input, &timeout_context).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), timeout_ready_rx)
            .await
            .unwrap()
            .unwrap();
        let completion = wait_for_finished(&timeout_controller).await;
        assert_eq!(completion.status, MonitorTaskCompletionStatus::TimedOut);
        server.await.unwrap();

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (cancel_ready_tx, cancel_ready_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let _ = cancel_ready_tx.send(());
            let _ = socket.next().await;
        });
        let cancel_controller = Arc::new(RecordingController::default());
        let cancel_context = websocket_context(cancel_controller.clone());
        let input = json!({
            "ws": format!("ws://{address}/events"),
            "description": "persistent events",
            "persistent": true
        });
        MonitorTool.call(input, &cancel_context).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), cancel_ready_rx)
            .await
            .unwrap()
            .unwrap();
        lock(&cancel_controller.cancels)[0].cancel();
        let completion = wait_for_finished(&cancel_controller).await;
        assert_eq!(completion.status, MonitorTaskCompletionStatus::Stopped);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn websocket_runtime_reports_handshake_failure_as_terminal_task() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });
        let controller = Arc::new(RecordingController::default());
        let context = websocket_context(controller.clone());
        let started = MonitorTool
            .call(websocket_input(address), &context)
            .await
            .unwrap();
        let completion = wait_for_finished(&controller).await;
        server.await.unwrap();
        assert_eq!(started["status"], "started");
        assert_eq!(completion.status, MonitorTaskCompletionStatus::Failed);
        assert!(completion.error.unwrap().contains("handshake"));
    }

    #[tokio::test]
    async fn command_permission_uses_shell_approval_shape() {
        let decision = MonitorTool
            .check_permissions(&valid_command(), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        assert_eq!(decision.request.unwrap().title, "Run shell command");
    }

    #[tokio::test]
    async fn websocket_permission_is_scoped_to_redacted_host() {
        let input = json!({
            "ws": "wss://Example.com/private?token=secret",
            "description": "events"
        });
        let decision = MonitorTool
            .check_permissions(&input, &ToolContext::new())
            .await
            .unwrap();
        let request = decision.request.unwrap();
        assert_eq!(
            request.metadata.unwrap()["permissionRule"],
            "domain:example.com"
        );
        assert!(!request.message.contains("secret"));
        assert!(!request.message.contains("private"));
    }
}
