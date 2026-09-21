use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rebon_command_seat::{CommandSeatService, Surface, TypedLine};
use rebon_permissions::types::PermissionMode;
use rebon_types::ExecutionPolicy;
use serde_json::{json, Value};

use super::commands::acp_advertised_slash_commands;
use super::config::{default_config_options, has_config_option_value, update_config_options};
use super::ACP_PROTOCOL_VERSION;
use crate::session::{
    resolve_session_cwd, PromptGeneration, PromptSessionSnapshot, ServerState, SessionRecord,
};
use rebon_agent_core::prompt_executor::SkillInvocationRequest;
use rebon_agent_core::prompt_executor::{
    PromptCancel, PromptExecutor, PromptExecutorError, PromptRequest, StubPromptExecutor,
};
use rebon_agent_core::publisher::{ChannelPermissionRequestPublisher, SessionUpdatePublisher};
use rebon_proto::types::{
    error_code, AgentCapabilities, AuthMethod, ConfigOption, ContentBlock, ImplementationInfo,
    InitializeParams, InitializeResult, JsonRpcError, McpCapabilities, McpServerConfig,
    PromptCapabilities, ProtocolVersion, SessionCancelParams, SessionCapabilities, SessionInfo,
    SessionListCapability, SessionListParams, SessionListResult, SessionLoadParams,
    SessionLoadResult, SessionNewParams, SessionNewResult, SessionPromptParams,
    SessionPromptResult, SessionSetConfigOptionParams, SessionSetConfigOptionResult,
    SessionSteeringParams, SessionSteeringResult, SessionUpdate, SteeringOutcome, StopReason,
    TextContent,
};
use rebon_session::session_storage::{default_projects_root, format_system_time_iso_ms};

/// Return whether `text` starts with a complete ultrawork slash-command token.
/// Leading whitespace is ignored for detection, while the original text remains
/// untouched. ACP and resumed background turns share one parser contract.
///
/// Which spellings those are is the catalog's, not a pair written here: `/ulw`
/// is an alias `rebon-slash-commands` already lists, and a second copy is a
/// second answer to "is this the ultrawork command".
pub fn starts_with_ultrawork_command(text: &str) -> bool {
    rebon_slash_commands::leading_command_token(text)
        .and_then(rebon_slash_commands::find)
        .is_some_and(|spec| spec.name == "ultrawork")
}

/// Detect an ultrawork opt-in (`/ultrawork` or `/ulw`, with or without a task
/// argument) in the first text block of `prompt`. When found, prepend the
/// ultrawork workflow-orchestration system-reminder to that block and return
/// `true` so the caller can attach [`rebon_types::ExecutionPolicy::workflow_controller`]
/// to the turn. Shared by the ACP `session/prompt` handler and the background
/// worker turn, so both surfaces wire ultrawork identically and the
/// reminder text has a single source of truth.
pub fn acp_prompt_with_ultrawork_reminder(
    mut prompt: Vec<rebon_types::ContentBlock>,
) -> (Vec<rebon_types::ContentBlock>, bool) {
    let Some(first_text) = prompt.iter_mut().find_map(|block| match block {
        rebon_types::ContentBlock::Text(text) => Some(text),
        _ => None,
    }) else {
        return (prompt, false);
    };
    if !starts_with_ultrawork_command(&first_text.text) {
        return (prompt, false);
    }
    first_text.text = format!(
        "<system-reminder>\nThe user explicitly requested ultrawork workflow orchestration. Use the Workflow tool to fulfill this request. This opt-in is a quality dial, not just permission: aim for the most exhaustive, correct result the task allows, and lean toward adversarially verifying findings rather than trusting a single pass. For research/review/audit-style requests, include a verification stage (adversarial refuters, a multi-lens judge vote, or a completeness critic) scaled to the request.\n</system-reminder>\n\n{}",
        first_text.text
    );
    (prompt, true)
}

/// The error every handler returns for an id no session answers to.
fn session_not_found(session_id: &str) -> JsonRpcError {
    JsonRpcError::invalid_params(format!("Session not found: {session_id}"))
}

/// The one text block a typed slash command arrives as.
///
/// A prompt carrying an image, a resource, or a second block is a turn for
/// the model whatever its first line says.
fn single_text_line(prompt: &[ContentBlock]) -> Option<&str> {
    let [ContentBlock::Text(text)] = prompt else {
        return None;
    };
    Some(text.text.trim())
}

/// What the `command-registry` seat says a typed line is, on this surface.
///
/// With no seat — a bare `DefaultHandler` in a test, an embedder that boots
/// no kernel — this falls back to the compiled-in built-in table, the same
/// way `rebon_slash_commands::{all, find}` do, and every row there is
/// natively implemented under its own name. So a host without a kernel keeps
/// answering the reports it always answered; what it cannot see is a
/// plugin's command, which only exists on a seat anyway.
///
/// The expansion runs on a blocking thread: a plugin's handler round-trips
/// to the plugin host process, and holding a runtime worker for the length of
/// that would stall every other session on this server. A handler that panics
/// takes the join with it, and that is a sentence for the person rather than
/// prompt text — a failure sent to the model reads as a question about the
/// failure.
async fn resolve_typed_command(
    kernel_scope: Option<&rebon_kernel::Context>,
    line: &str,
) -> TypedLine {
    // Ordinary prose is every turn, and a thread hop for every turn is a
    // thread hop for nothing: only a line that opens with a command token can
    // name one.
    if rebon_slash_commands::leading_command_token(line).is_none() {
        return TypedLine::NotACommand;
    }
    let Some(seat) = kernel_scope.and_then(|scope| scope.get::<CommandSeatService>()) else {
        return rebon_slash_commands::leading_command_token(line)
            .and_then(rebon_slash_commands::find)
            .map(|spec| TypedLine::Native {
                args: rebon_command_seat::CommandArgs::from_line(line, &spec, Surface::Acp),
                id: spec.name.to_string(),
            })
            .unwrap_or(TypedLine::NotACommand);
    };
    let line = line.to_string();
    match tokio::task::spawn_blocking(move || {
        rebon_command_seat::resolve_typed_line(&seat, &line, Surface::Acp)
    })
    .await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::error!(%error, "a slash command handler did not return");
            TypedLine::Say("That command could not be run.".to_string())
        }
    }
}

/// The skill invocations a client attached to a prompt, from
/// `_meta.rebon.skillInvocations`.
///
/// A local surface turns a typed `/skill args` into a Skill tool call the
/// engine dispatches ahead of the model's first turn, with the typed line
/// as the user's message. ACP's `session/prompt` has no field for that, so
/// a client that resolved the name against the skill registry (the web
/// page does, from `/api/commands`) says so in `_meta`; a client that did
/// not sends the line as text, which is what every ACP client did before.
fn skill_invocations_from_meta(
    meta: Option<&HashMap<String, Value>>,
) -> Result<Vec<SkillInvocationRequest>, JsonRpcError> {
    let Some(raw) = meta
        .and_then(|meta| meta.get("rebon"))
        .and_then(|rebon| rebon.get("skillInvocations"))
    else {
        return Ok(Vec::new());
    };
    let invocations: Vec<SkillInvocationRequest> =
        serde_json::from_value(raw.clone()).map_err(|err| {
            JsonRpcError::invalid_params(format!("_meta.rebon.skillInvocations: {err}"))
        })?;
    if invocations.iter().any(|invocation| {
        invocation.skill.trim().is_empty()
            || !invocation
                .skill
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
    }) {
        return Err(JsonRpcError::invalid_params(
            "_meta.rebon.skillInvocations: a skill name is letters, digits, `:`, `_` and `-`",
        ));
    }
    Ok(invocations)
}

/// Stand-in for any value the ACP server genuinely cannot measure.
const UNAVAILABLE: &str = "unavailable";

/// `format_status_command` types the agent/task counters as `usize`, but the
/// ACP server owns no agent or task registry to count. Rewrite the two lines
/// so a structural zero does not read as an observed zero.
fn mark_unmeasured_status_counters(status: String) -> String {
    status
        .lines()
        .map(|line| match line {
            "active agents: 0" => format!("active agents: {UNAVAILABLE}"),
            "active background tasks: 0" => format!("active background tasks: {UNAVAILABLE}"),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolve the three settings files `/hooks` reports on, the same way the
/// TUI hooks dialog does: the user file lives in the config home.
fn acp_hook_settings_paths(cwd: &std::path::Path) -> rebon_hooks::SettingsPaths {
    rebon_hooks::SettingsPaths::from_config_dir(&rebon_session::default_config_home_dir(), cwd)
}

/// Build the `/hooks` report for an ACP session.
///
/// The event list comes from [`rebon_hooks::HOOK_EVENTS`] rather than a
/// hand-copied subset, and the configured entries are read from the user,
/// project, and local `settings.json` under the session cwd — reporting an
/// empty configuration when hooks *are* configured is a false negative the
/// caller cannot detect.
///
/// Matcher value lists stay empty: they enumerate the live tool and agent
/// registries, which belong to the prompt executor, not this server.
fn format_acp_hooks_command(cwd: &str, args: &str) -> String {
    let metadata = rebon_hooks::build_hook_event_metadata(&rebon_hooks::MetadataInputs {
        tool_names: Vec::new(),
        agent_types: Vec::new(),
        elicitation_servers: Vec::new(),
    });
    let paths = acp_hook_settings_paths(std::path::Path::new(cwd));
    let (configured, warnings) = match rebon_hooks::load_all_editable(&paths) {
        Ok(loaded) => (
            loaded.hooks,
            loaded
                .warnings
                .into_iter()
                .map(|warning| format!("{}: {}", warning.path.display(), warning.reason))
                .collect(),
        ),
        Err(error) => (Vec::new(), vec![format!("settings load failed: {error}")]),
    };

    let events = rebon_hooks::HOOK_EVENTS
        .iter()
        .map(|event| {
            let event_metadata = metadata.get(event);
            let configs = configured
                .iter()
                .filter(|hook| hook.event == *event)
                .map(|hook| rebon_slash_commands::formatters::HookConfigDto {
                    kind: hook.config.type_str().to_string(),
                    display: rebon_hooks::display_text(&hook.config).to_string(),
                    matcher: hook.matcher.as_deref().unwrap_or("all").to_string(),
                    source: hook.source.header().to_string(),
                })
                .collect();
            rebon_slash_commands::formatters::HookEventDto {
                name: event.name().to_string(),
                summary: event_metadata.map(|value| value.summary.clone()),
                description: event_metadata.map(|value| value.description.clone()),
                matcher: rebon_hooks::matcher_metadata_for_event(&metadata, *event).map(|value| {
                    rebon_slash_commands::formatters::HookMatcherDto {
                        field: value.field_to_match.clone(),
                        values: value.values.clone(),
                    }
                }),
                configs,
            }
        })
        .collect();

    rebon_slash_commands::formatters::format_hooks_command(
        rebon_slash_commands::formatters::HooksCommandDto {
            selected_event: (!args.is_empty()).then(|| {
                rebon_hooks::parse_hook_event(args)
                    .map(|event| event.name().to_string())
                    .unwrap_or_else(|| args.to_string())
            }),
            events,
            warnings,
        },
    )
}

/// The read-only reports this server answers itself, keyed by the stable id a
/// [`CommandHandler::Native`] registration carries.
///
/// This match *is* the list. There used to be a second one — seven names in
/// `read_only_slash_command`, checked before this was reached — and a report
/// added here without being added there was a command the editor advertised
/// and the server sent to the model. `None` for an id with no report, which
/// leaves the line to go on as prompt text.
pub(super) fn format_acp_read_only_command(
    record: &SessionRecord,
    command: &str,
    args: &str,
    kernel_scope: Option<&rebon_kernel::Context>,
) -> Option<String> {
    Some(match command {
        "status" => mark_unmeasured_status_counters(rebon_slash_commands::formatters::format_status_command(rebon_slash_commands::formatters::StatusCommandDto {
            provider: "ACP host".into(), model: "managed by prompt executor".into(), cwd: record.cwd.clone(),
            ui_mode: "ACP".into(), vim_mode: "n/a (ACP has no editor mode)".into(), mcp_client: if record.mcp_servers.is_empty() { "not connected" } else { "present" }.into(),
            active_agents: 0, active_tasks: 0, session_title: record.title.clone(), ultraplan_phase: None,
            update_status: Some(format!("session id: {}", record.id)),
        })),
        // Every token counter here is unknown to the ACP server: usage lives
        // in the prompt executor, which does not report it back. Printing
        // "0" would read as a measured zero.
        "cost" => rebon_slash_commands::formatters::format_cost_command(rebon_slash_commands::formatters::CostCommandDto {
            duration: record.created_at.elapsed().map(|d| format!("{}s", d.as_secs())).unwrap_or_else(|_| "unknown".into()),
            context_tokens: UNAVAILABLE.into(), context_window: "unknown".into(), context_source: "ACP does not expose token usage".into(), streaming_tokens: UNAVAILABLE.into(),
            total_input: UNAVAILABLE.into(), total_output: UNAVAILABLE.into(), total_cache_read: UNAVAILABLE.into(), total_cache_write: UNAVAILABLE.into(), last_input: UNAVAILABLE.into(), last_output: UNAVAILABLE.into(),
            usd_summary: Some("USD estimate\n  ACP usage: unpriced (excluded from total)\n  total: unavailable (no token usage to price)".into()),
        }),
        "context" => rebon_slash_commands::formatters::format_context_command(
            rebon_slash_commands::formatters::ContextCommandDto::Unavailable(
                rebon_slash_commands::formatters::ContextUnavailableDto {
                    session_kind: "ACP session".into(),
                    live_messages: record.messages.len(),
                    loaded_records: record.loaded_transcript.len(),
                    reason: "unavailable from ACP prompt executor".into(),
                },
            ),
        ),
        // `/memory` reports what the session loaded, so it has to resolve the
        // same set the TUI does — ancestor `REBON.md`, `.rebon/rules/**`, `@`
        // includes, `REBON.local.md`, the user-level instructions, and the auto
        // `MEMORY.md`. That set spans the plugin boundary: only the `memory`
        // plugin knows the per-project switch that decides whether the auto
        // `MEMORY.md` counts, so the whole list comes off the
        // `loaded-documents` seam rather than being assembled here. With no
        // provider — the plugin off, or a host with no kernel — the list is
        // empty rather than partial: reporting the instruction files alone
        // would claim the session loaded less than it did.
        "memory" => rebon_slash_commands::formatters::format_memory_command(
            kernel_scope
                .map(|scope| {
                    rebon_instructions::loaded_documents::loaded_files(scope, &record.cwd)
                })
                .unwrap_or_default()
                .into_iter()
                .map(|file| rebon_slash_commands::formatters::MemoryFileDto {
                    path: file.path.to_string_lossy().into_owned(),
                    tokens: file.approx_tokens().to_string(),
                })
                .collect(),
        ),
        "mcp" => rebon_slash_commands::formatters::format_mcp_command(rebon_slash_commands::formatters::McpCommandDto {
            loader: if record.mcp_servers.is_empty() { "not configured" } else { "ready" }.into(), warnings: Vec::new(), error: None,
            client: if record.mcp_servers.is_empty() { "not connected" } else { "ready" }.into(), tools: Vec::new(),
        }),
        "hooks" => format_acp_hooks_command(&record.cwd, args),
        "doctor" => rebon_slash_commands::formatters::format_doctor_command(rebon_slash_commands::formatters::DoctorCommandDto {
            version: env!("CARGO_PKG_VERSION").into(), provider: "ACP host".into(), model: "managed by prompt executor".into(), cwd: record.cwd.clone(), session_id: record.id.clone(), permission_mode: record.permission_mode.clone(),
            sections: vec![rebon_slash_commands::formatters::DoctorSectionDto { title: "ACP session".into(), rows: vec![rebon_slash_commands::formatters::DoctorRowDto { status: rebon_slash_commands::formatters::DoctorStatus::Ok, label: "session".into(), message: "session is loaded and accepting commands".into(), suggestion: None }] }],
        }),
        _ => return None,
    })
}

/// One steering message queued for injection into a running prompt turn.
///
/// `user_message_uuid` becomes the transcript uuid of the steered user
/// line when the host's attachment poller injects the message, so the
/// same identity flows through persistence and the `QueuedUserMessage`
/// echo the client receives.
#[derive(Debug, Clone)]
pub struct SteeringMessage {
    pub prompt: Vec<rebon_types::ContentBlock>,
    pub user_message_uuid: String,
}

/// Host-side mailbox for `_session/steering`.
///
/// The handler owns *when* a steering message is queued or taken back;
/// the host owns *how* queued messages reach the model — its
/// implementation doubles as an engine `AttachmentPoller` that drains
/// the per-session queue between tool rounds. The split keeps this crate
/// free of any engine dependency.
///
/// Delivery contract: a message removed by the poller is delivered (the
/// engine persists and echoes it); a message still queued when the turn
/// ends is returned by [`SteeringSink::drain_pending`] so the handler
/// can hand it to a fresh turn instead of losing it.
pub trait SteeringSink: Send + Sync {
    /// Queue steering messages for `session_id`.
    fn enqueue(&self, session_id: &str, messages: Vec<SteeringMessage>);

    /// Remove and return every not-yet-delivered steering message for
    /// `session_id`, oldest first.
    fn drain_pending(&self, session_id: &str) -> Vec<SteeringMessage>;

    /// Give a drained batch back at the *front* of the queue, so a
    /// message that lost a slot race keeps its place ahead of anything
    /// enqueued in the meantime. Defaults to plain `enqueue` for
    /// implementations that don't track ordering.
    fn requeue_front(&self, session_id: &str, messages: Vec<SteeringMessage>) {
        self.enqueue(session_id, messages);
    }
}

static STEERING_UUID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint a process-unique uuid for a steered user line, following the
/// same timestamp+counter recipe as session ids in [`ServerState`].
fn next_steering_uuid() -> String {
    let n = STEERING_UUID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("u-steer-{ts_nanos:x}-{n:x}")
}

/// Method-routing abstraction for the ACP server.
///
/// One implementation handles the whole request surface: the server loop
/// does not know which method names are supported, it simply forwards the
/// method name + raw params to [`RequestHandler::handle_request`]. The
/// default implementation of [`RequestHandler::handle_notification`]
/// ignores notifications, per spec's "ignore unrecognized notifications"
/// behavior.
///
/// [`DefaultHandler`] can be replaced with a richer handler
/// that owns sessions, prompt turns, and tool execution.
#[async_trait]
pub trait RequestHandler: Send + Sync {
    /// Handle a JSON-RPC request. Returning `Ok(value)` produces a
    /// successful response carrying `value` as `result`. Returning an
    /// error produces a JSON-RPC error response with the embedded
    /// [`JsonRpcError`].
    async fn handle_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, JsonRpcError>;

    /// Return the working directory for a session whose permission request
    /// is being answered. Custom handlers without session state may omit it.
    fn permission_session_cwd(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// Hand rules the client just accepted through an allow-always option to
    /// the host's live permission policy.
    ///
    /// The server loop writes those rules to `.rebon/settings.json` itself,
    /// but a rule that only exists on disk does not take effect until the
    /// next start — the same tool call keeps prompting for the rest of the
    /// session. Applying it in memory needs the host's policy store, which
    /// this crate deliberately does not depend on, so it arrives through
    /// this seam. A host that cannot apply it must refuse; the server will not
    /// persist or forward a successful allow-always selection in that case.
    fn apply_allow_always_rules(
        &self,
        _session_id: &str,
        _rules: &[rebon_permissions::PermissionRuleValue],
    ) -> Result<(), String> {
        Err("session live permission policy is unavailable".into())
    }

    /// Handle a JSON-RPC notification. Default: ignore.
    async fn handle_notification(&self, _method: &str, _params: Option<Value>) {}
}

#[derive(Clone)]
pub(super) struct ActivePromptCancel {
    pub(super) generation: PromptGeneration,
    pub(super) cancel: PromptCancel,
}

/// Synchronous cleanup for one acquired prompt turn. During setup the guard is
/// created immediately after ownership becomes visible; it clears state without
/// relocking the cancellation map if setup unwinds while that map is held. Once
/// registration completes it also owns cancellation-entry cleanup. Generation
/// checks make a delayed Drop harmless if a newer turn has acquired the session.
pub(super) struct PromptLifecycleGuard {
    state: Arc<ServerState>,
    active_cancels: Arc<Mutex<HashMap<String, ActivePromptCancel>>>,
    session_id: String,
    generation: PromptGeneration,
    cancel_registered: bool,
}

impl PromptLifecycleGuard {
    #[cfg(test)]
    pub(super) fn new(
        state: Arc<ServerState>,
        active_cancels: Arc<Mutex<HashMap<String, ActivePromptCancel>>>,
        session_id: String,
        generation: PromptGeneration,
    ) -> Self {
        Self {
            state,
            active_cancels,
            session_id,
            generation,
            cancel_registered: true,
        }
    }

    fn new_during_registration(
        state: Arc<ServerState>,
        active_cancels: Arc<Mutex<HashMap<String, ActivePromptCancel>>>,
        session_id: String,
        generation: PromptGeneration,
    ) -> Self {
        Self {
            state,
            active_cancels,
            session_id,
            generation,
            cancel_registered: false,
        }
    }

    fn mark_cancel_registered(&mut self) {
        self.cancel_registered = true;
    }
}

impl Drop for PromptLifecycleGuard {
    fn drop(&mut self) {
        // Never hold this lock while acquiring a ServerState lock. Cancel RPCs
        // only take this lock, and ServerState cleanup uses its own fixed
        // active-prompts -> sessions order.
        if self.cancel_registered {
            let mut cancels = self
                .active_cancels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cancels
                .get(&self.session_id)
                .is_some_and(|entry| entry.generation == self.generation)
            {
                cancels.remove(&self.session_id);
            }
        }
        self.state.finish_prompt(&self.session_id, self.generation);
    }
}

/// One acquired active-prompt slot, ready to be driven. Bundles the
/// session snapshot, the RAII lifecycle guard, and the cancel handle
/// registered under the turn's generation.
pub(super) struct AcquiredPromptTurn {
    record: PromptSessionSnapshot,
    lifecycle: PromptLifecycleGuard,
    cancel: PromptCancel,
}

/// Config options whose value is a fact about one session, not the server.
///
/// The shared option list answers "what can be set"; these answer "what is
/// set *here*". The host that owns the per-session state supplies both
/// directions: `apply` changes it and may refuse (the refusal reaches the
/// client as invalid params), `current` reads it back as `(config_id,
/// value)` pairs so `session/new`, `session/load` and
/// `session/set_config_option` report the session's own value rather than
/// whatever the last session set.
pub struct SessionConfigOptions {
    /// The options themselves; appended to the shared list once.
    pub options: Vec<ConfigOption>,
    /// `(session_id, config_id, value)` → applied, or a reason it was not.
    pub apply: SessionConfigApply,
    /// `session_id` → the session's current `(config_id, value)` pairs.
    pub current: SessionConfigCurrent,
}

/// `(session_id, config_id, value)` → applied, or the reason it was not.
pub type SessionConfigApply = Arc<dyn Fn(&str, &str, &str) -> Result<(), String> + Send + Sync>;
/// `session_id` → the session's current `(config_id, value)` pairs.
pub type SessionConfigCurrent = Arc<dyn Fn(&str) -> Vec<(String, String)> + Send + Sync>;

impl std::fmt::Debug for SessionConfigOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConfigOptions")
            .field(
                "options",
                &self
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The default [`RequestHandler`]: it answers `initialize`, `session/new`,
/// `session/load`, `session/list`, `session/prompt` and
/// `session/set_config_option` (plus the internal `_session/steering`), and
/// refuses every other method as method-not-found. `run_server()` wires this
/// one up; tests construct it directly to drive the dispatcher in isolation.
///
/// Clones share the underlying [`ServerState`] via `Arc`, so any dispatcher
/// that fans out concurrent requests will observe the same `initialized`
/// flag and session map. The serve loop does exactly that — requests,
/// notifications and steering each get a worker of their own — which is why
/// the state is shared rather than owned per task.
#[derive(Clone)]
pub struct DefaultHandler {
    /// Largest ACP protocol version this handler will negotiate.
    pub max_protocol_version: ProtocolVersion,
    pub agent_info: ImplementationInfo,
    pub agent_capabilities: AgentCapabilities,
    /// Auth methods advertised in the initialize response. Empty by default.
    pub auth_methods: Vec<AuthMethod>,
    /// ACP config options returned by `session/new` and `session/set_config_option`.
    pub config_options: Arc<Mutex<Vec<ConfigOption>>>,
    /// Optional local side-effect hook for accepted config-option changes.
    pub config_option_applier: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
    /// Config options whose value belongs to one session rather than the
    /// server. See [`SessionConfigOptions`].
    pub session_config_options: Option<Arc<SessionConfigOptions>>,
    /// Snapshot the host's policy when a session is activated, before any
    /// other session can persist a grant that an idle session would inherit.
    pub session_policy_initializer:
        Option<Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>>,
    /// Optional sink for allow-always rules the client accepted, so the
    /// host's live policy store honours them without a restart.
    pub allow_always_applier: Option<
        Arc<
            dyn Fn(&str, &str, &[rebon_permissions::PermissionRuleValue]) -> Result<(), String>
                + Send
                + Sync,
        >,
    >,
    /// Fallback cwd for `session/new` when the request params omit one; if
    /// unset, `session/new` falls back again to the current process cwd.
    pub default_cwd: Option<String>,
    /// The kernel scope this server resolves optional plugin seams through
    /// — today only `loaded-documents`, behind `/memory`. Held as a scope,
    /// not as a resolved provider, so a plugin that unloads stops answering.
    /// `None` for a server built without a kernel, which every test that
    /// does not need one gets.
    pub(super) kernel_scope: Option<rebon_kernel::Context>,
    /// Root directory that holds on-disk session transcripts — the
    /// `projects` subdirectory of the config home. `session/load` resolves
    /// `${projects_root}/${sanitize(cwd)}/${sessionId}.jsonl` and reads
    /// from it. Tests override this to a temp dir; production leaves it
    /// `None`, which lazily resolves to [`default_projects_root`] at
    /// lookup time (so `REBON_CONFIG_DIR` changes still take effect
    /// between requests instead of being cached once).
    pub projects_root: Option<PathBuf>,
    /// Hide sessions that are currently locked by a running TUI instance.
    pub filter_active_sessions: bool,
    /// Shared server state (initialized flag + session map).
    pub(super) state: Arc<ServerState>,
    /// Optional streaming update publisher. When set, `session/prompt`
    /// forwards updates through this sink so the client sees partial
    /// results while the model is streaming.
    pub update_publisher: Option<Arc<dyn SessionUpdatePublisher>>,
    /// Optional outbound permission request publisher. Plumbed into
    /// the prompt executor so tool permission prompts can round-trip
    /// through the ACP client.
    pub permission_publisher: Option<ChannelPermissionRequestPublisher>,
    /// Prompt executor that owns the agentic turn. Defaults to a
    /// [`StubPromptExecutor`] that returns `end_turn` immediately;
    /// production callers swap in a real engine-backed executor
    /// (or an equivalent) via [`Self::with_prompt_executor`].
    pub(super) prompt_executor: Arc<dyn PromptExecutor>,
    /// Map of session_id → cancel handle held while a prompt is in
    /// flight. `session/cancel` trips the handle to abort the turn.
    pub(super) active_cancels: Arc<Mutex<HashMap<String, ActivePromptCancel>>>,
    /// Optional `_session/steering` mailbox. When wired, `initialize`
    /// advertises `_meta.steering.supported` and `_session/steering`
    /// injects into the active turn (or starts a fresh one). When
    /// absent the method stays method-not-found, matching the missing
    /// advertisement.
    pub(super) steering_sink: Option<Arc<dyn SteeringSink>>,
    /// Deterministic test gate for the formerly racy interval after prompt
    /// ownership is visible but before its cancellation handle is published.
    #[cfg(test)]
    pub(super) prompt_setup_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for DefaultHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultHandler")
            .field("max_protocol_version", &self.max_protocol_version)
            .field("agent_info", &self.agent_info)
            .field("auth_methods", &self.auth_methods)
            .field("default_cwd", &self.default_cwd)
            .field("has_kernel_scope", &self.kernel_scope.is_some())
            .field("projects_root", &self.projects_root)
            .field("filter_active_sessions", &self.filter_active_sessions)
            .field("has_update_publisher", &self.update_publisher.is_some())
            .field(
                "has_permission_publisher",
                &self.permission_publisher.is_some(),
            )
            .field("has_custom_prompt_executor", &true)
            .field("has_steering_sink", &self.steering_sink.is_some())
            .finish()
    }
}

impl Default for DefaultHandler {
    fn default() -> Self {
        Self {
            max_protocol_version: ACP_PROTOCOL_VERSION,
            agent_info: ImplementationInfo {
                name: "rebon".to_string(),
                title: Some("Rebon".to_string()),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            },
            agent_capabilities: AgentCapabilities {
                load_session: Some(true),
                session_capabilities: Some(SessionCapabilities {
                    list: Some(SessionListCapability {}),
                }),
                prompt_capabilities: Some(PromptCapabilities {
                    image: Some(true),
                    audio: None,
                    embedded_context: Some(true),
                }),
                mcp_capabilities: Some(McpCapabilities {
                    http: Some(true),
                    sse: Some(true),
                }),
                meta: None,
            },
            auth_methods: Vec::new(),
            config_options: Arc::new(Mutex::new(default_config_options())),
            config_option_applier: None,
            session_config_options: None,
            allow_always_applier: None,
            session_policy_initializer: None,
            default_cwd: None,
            kernel_scope: None,
            projects_root: None,
            filter_active_sessions: false,
            state: Arc::new(ServerState::new()),
            update_publisher: None,
            permission_publisher: None,
            prompt_executor: Arc::new(StubPromptExecutor::default()),
            active_cancels: Arc::new(Mutex::new(HashMap::new())),
            steering_sink: None,
            #[cfg(test)]
            prompt_setup_hook: None,
        }
    }
}

impl DefaultHandler {
    /// Replace the default [`StubPromptExecutor`] with a real
    /// [`PromptExecutor`]. Typically wired from a higher layer that owns the
    /// real engine-backed implementation.
    pub fn with_prompt_executor(mut self, executor: Arc<dyn PromptExecutor>) -> Self {
        self.prompt_executor = executor;
        self
    }

    /// Resolve this server's optional plugin seams through `scope`.
    pub fn with_kernel_scope(mut self, scope: rebon_kernel::Context) -> Self {
        self.kernel_scope = Some(scope);
        self
    }

    /// Attach a streaming update publisher.
    pub fn with_update_publisher(mut self, publisher: Arc<dyn SessionUpdatePublisher>) -> Self {
        self.update_publisher = Some(publisher);
        self
    }

    /// Attach an outbound permission request publisher.
    pub fn with_permission_publisher(
        mut self,
        publisher: ChannelPermissionRequestPublisher,
    ) -> Self {
        self.permission_publisher = Some(publisher);
        self
    }

    /// Hide sessions locked by active TUI instances from `session/list`.
    pub fn with_filter_active_sessions(mut self, filter: bool) -> Self {
        self.filter_active_sessions = filter;
        self
    }

    /// Make this handler's sessions owned: the shared [`ServerState`] takes
    /// each session's active lock when it mints or loads it, and `session/load`
    /// refuses a session another process holds instead of opening a second
    /// writer on the same transcript.
    ///
    /// Off by default because a surface that still takes the lock itself would
    /// otherwise race its own state through the in-process lock registry. Call
    /// it *after* setting `projects_root`; the root is resolved here, once.
    pub fn with_session_ownership(self) -> Self {
        let root = self
            .projects_root
            .clone()
            .unwrap_or_else(default_projects_root);
        if !self.state.enable_session_ownership(root) {
            tracing::error!(
                "rebon: session ownership was already enabled under a different projects root"
            );
        }
        self
    }

    /// Attach the `_session/steering` mailbox. Wiring this both enables
    /// the method and makes `initialize` advertise
    /// `_meta.steering.supported`.
    pub fn with_steering_sink(mut self, sink: Arc<dyn SteeringSink>) -> Self {
        self.steering_sink = Some(sink);
        self
    }

    /// Attach a callback invoked after an accepted config-option update.
    pub fn with_config_option_applier(
        mut self,
        applier: Arc<dyn Fn(&str, &str) + Send + Sync>,
    ) -> Self {
        self.config_option_applier = Some(applier);
        self
    }

    /// Attach config options whose value is a fact about one session.
    ///
    /// The options are appended to the shared list so every client sees
    /// them in `session/new`, `session/load`, and
    /// `session/set_config_option` results; their `current_value` is read
    /// back per session through [`SessionConfigOptions::current`], and a
    /// change goes through [`SessionConfigOptions::apply`] instead of the
    /// shared store. An option id that is already in the shared list is
    /// refused, because a value cannot be both server-wide and
    /// per-session.
    pub fn with_session_config_options(self, options: SessionConfigOptions) -> Self {
        {
            let mut shared = self
                .config_options
                .lock()
                .expect("config options mutex poisoned");
            for option in &options.options {
                assert!(
                    !shared.iter().any(|existing| existing.id == option.id),
                    "config option {:?} is already a server-wide option",
                    option.id
                );
                shared.push(option.clone());
            }
        }
        Self {
            session_config_options: Some(Arc::new(options)),
            ..self
        }
    }

    /// Attach the sink that feeds accepted allow-always rules into the
    /// host's live policy store. See
    /// [`RequestHandler::apply_allow_always_rules`].
    pub fn with_allow_always_applier(
        mut self,
        applier: Arc<
            dyn Fn(&str, &str, &[rebon_permissions::PermissionRuleValue]) -> Result<(), String>
                + Send
                + Sync,
        >,
    ) -> Self {
        self.allow_always_applier = Some(applier);
        self
    }

    /// Apply one config-option change through the same in-memory path
    /// used by `session/set_config_option`.
    ///
    /// The local TUI bypasses ACP transport, so it calls this helper
    /// directly to keep config-option and permission-mode state in sync.
    pub fn apply_config_option_local(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Vec<ConfigOption> {
        self.apply_config_option_update(session_id, config_id, value)
    }

    /// Seed the process-local permission mode used by sessions opened after
    /// startup, without invoking persistence side effects.
    ///
    /// Unlike [`Self::seed_config_option_value`], this accepts session-scoped
    /// modes such as `plan` and `bypassPermissions`. Hosts use it only for an
    /// explicit startup override; persisted defaults must continue through
    /// [`Self::seed_config_option_value`] so they cannot become session-scoped.
    pub fn seed_startup_permission_mode(&self, value: &str) {
        let _ = self.update_config_options_shared("permissions", value);
    }

    /// Seed a config option's `current_value` from persisted storage
    /// without running the side effects `apply_config_option_local`
    /// triggers (permission mode, etc.). Used at startup to reflect
    /// `~/.rebon/config.json` in the option list before the user
    /// opens the Settings dialog.
    ///
    /// Silently ignored if `config_id` or `value` are not recognised.
    pub fn seed_config_option_value(&self, config_id: &str, value: &str) {
        if config_id == "permissions" && PermissionMode::from_wire(value).is_session_scoped() {
            return;
        }
        // A seat row needs no seeding. Seeding exists because this handler's
        // list held a copy of the value that startup had to fill in; a seat
        // row holds none, so writing one back would be the stale copy this
        // seat was built to remove.
        if Self::seat_owns(config_id) {
            return;
        }
        let _ = self.update_config_options_shared(config_id, value);
    }
}

impl DefaultHandler {
    pub(super) fn config_options_snapshot(&self) -> Vec<ConfigOption> {
        self.config_options_with_seat_rows(None)
    }

    /// The session rows this handler holds, followed by every row registered
    /// on the `config-options` seat.
    ///
    /// The seat is read on every call rather than copied in at startup: a row
    /// reports what its owner reads *now*, so a value changed by a slash
    /// command or by an edit to the config file is already right, and a plugin
    /// that loaded or unloaded since the last call is reflected without anyone
    /// being told. No seat — before the kernel boots, or a composition that
    /// leaves the Core plugin out — means the session rows alone, which is
    /// what this handler could offer before the seat existed.
    fn config_options_with_seat_rows(&self, session_id: Option<&str>) -> Vec<ConfigOption> {
        let mut options = self
            .config_options
            .lock()
            .expect("config options mutex poisoned")
            .clone();
        if let Some(seat) = rebon_config_seat::process_config_seat() {
            // A session row wins a clash: it is the one that can answer for
            // *this* session, and the seat's would quietly report another's.
            let taken: Vec<String> = options.iter().map(|option| option.id.clone()).collect();
            options.extend(
                seat.options(session_id)
                    .into_iter()
                    .filter(|option| !taken.contains(&option.id)),
            );
        }
        options
    }

    /// Whether the seat owns `config_id`, in which case applying it is the
    /// registrar's business and not this handler's.
    fn seat_owns(config_id: &str) -> bool {
        rebon_config_seat::process_config_seat()
            .is_some_and(|seat| seat.has(config_id))
    }

    pub fn config_options_for_session(&self, session_id: &str) -> Vec<ConfigOption> {
        let options = self.config_options_with_seat_rows(Some(session_id));
        let Some(session) = self.state.get_session(session_id) else {
            return options;
        };
        let mut options = update_config_options(&options, "permissions", &session.permission_mode);
        if let Some(session_options) = &self.session_config_options {
            for (config_id, value) in (session_options.current)(session_id) {
                if let Some(option) = options.iter_mut().find(|option| option.id == config_id) {
                    option.current_value = value;
                }
            }
        }
        options
    }

    /// Whether `config_id` is one of the per-session options.
    fn is_session_config_option(&self, config_id: &str) -> bool {
        self.session_config_options
            .as_ref()
            .map(|options| options.options.iter().any(|option| option.id == config_id))
            .unwrap_or(false)
    }

    fn apply_config_option_update(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Vec<ConfigOption> {
        // A seat row is applied by whoever registered it. Going through the
        // seat is what keeps one setting's persistence in one place: the same
        // row reached from the terminal, from `serve` and over ACP used to be
        // three copies of the same write.
        if Self::seat_owns(config_id) {
            if let Some(seat) = rebon_config_seat::process_config_seat() {
                match seat.apply(Some(session_id), config_id, value) {
                    // Persisted. The surface is still told, because some of
                    // these have live state a file cannot reach: the service
                    // tier a running session sends, the shell tools a session
                    // offers. The seat owns *what the setting is*; reacting to
                    // it is the surface's.
                    Ok(()) => {
                        if let Some(apply) = &self.config_option_applier {
                            apply(config_id, value);
                        }
                    }
                    // Refused: nothing was written, so nothing reacts either.
                    // Telling the surface here would leave the live state
                    // saying one thing and the file another.
                    Err(reason) => tracing::warn!(
                        config_id, value, %reason,
                        "a settings row refused the value it was given"
                    ),
                }
            }
            return self.config_options_for_session(session_id);
        }

        let current = self.config_options_snapshot();
        if !has_config_option_value(&current, config_id, value) {
            // Said out loud rather than returning the unchanged list. A value
            // this list does not offer is a client sending something no
            // surface can have shown it; silence here reads to the caller as
            // a setting that refuses to change for no reason.
            tracing::warn!(
                config_id,
                value,
                "no settings row offers this value; nothing applied"
            );
            return self.config_options_for_session(session_id);
        }

        let session_scoped_permission_mode =
            config_id == "permissions" && PermissionMode::from_wire(value).is_session_scoped();
        if !session_scoped_permission_mode {
            let _ = self.update_config_options_shared(config_id, value);
        }
        if config_id == "permissions" {
            let _ = self.state.set_permission_mode(session_id, value);
        }
        if !session_scoped_permission_mode {
            if let Some(apply) = &self.config_option_applier {
                apply(config_id, value);
            }
        }

        self.config_options_for_session(session_id)
    }

    fn update_config_options_shared(&self, config_id: &str, value: &str) -> Vec<ConfigOption> {
        let mut config_options = self
            .config_options
            .lock()
            .expect("config options mutex poisoned");
        if has_config_option_value(&config_options, config_id, value) {
            *config_options = update_config_options(&config_options, config_id, value);
        }
        config_options.clone()
    }

    fn config_option_current_value(&self, config_id: &str, fallback: &str) -> String {
        self.config_options
            .lock()
            .expect("config options mutex poisoned")
            .iter()
            .find(|option| option.id == config_id)
            .map(|option| option.current_value.clone())
            .unwrap_or_else(|| fallback.to_string())
    }

    fn validate_mcp_servers(
        &self,
        method: &str,
        servers: &[McpServerConfig],
    ) -> Result<(), JsonRpcError> {
        let mut names = HashSet::new();
        for server in servers {
            let name = server.name().trim();
            if name.is_empty() {
                return Err(JsonRpcError::invalid_params(format!(
                    "{method} mcpServers entries require non-blank name"
                )));
            }
            if !names.insert(name.to_string()) {
                return Err(JsonRpcError::invalid_params(format!(
                    "{method} mcpServers contains duplicate server name: {name}"
                )));
            }
            match server {
                McpServerConfig::Stdio { command, .. } if command.trim().is_empty() => {
                    return Err(JsonRpcError::invalid_params(format!(
                        "{method} mcpServers stdio server `{name}` requires non-blank command"
                    )));
                }
                McpServerConfig::Http { url, .. } if url.trim().is_empty() => {
                    return Err(JsonRpcError::invalid_params(format!(
                        "{method} mcpServers http server `{name}` requires non-blank url"
                    )));
                }
                McpServerConfig::Sse { url, .. } if url.trim().is_empty() => {
                    return Err(JsonRpcError::invalid_params(format!(
                        "{method} mcpServers sse server `{name}` requires non-blank url"
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Access the shared server state. Intended for tests and diagnostics;
    /// the dispatch path itself touches state only through the helper
    /// methods below.
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    fn initialize(&self, params: Option<Value>) -> Result<Value, JsonRpcError> {
        let params: InitializeParams = match params {
            Some(v) => serde_json::from_value(v)
                .map_err(|e| JsonRpcError::invalid_params(format!("initialize params: {e}")))?,
            None => return Err(JsonRpcError::invalid_params("initialize requires params")),
        };

        // Enforce single-initialize semantics *before* we compute the
        // response, so a duplicate `initialize` hits the "Already initialized" path.
        self.state.mark_initialized()?;

        let protocol_version = std::cmp::min(params.protocol_version, self.max_protocol_version);
        let result = InitializeResult {
            protocol_version,
            agent_capabilities: self.agent_capabilities.clone(),
            auth_methods: self.auth_methods.clone(),
            agent_info: Some(self.agent_info.clone()),
            // Top-level `_meta`, a sibling of agentCapabilities — the
            // spelling both claude-agent-acp and codex-acp use, which
            // Rebon's own ACP client already reads. Advertised only
            // when a steering sink is wired, so `_session/steering`
            // support and its advertisement can never disagree.
            meta: self.steering_sink.is_some().then(|| {
                HashMap::from([(
                    "steering".to_string(),
                    serde_json::json!({ "supported": true }),
                )])
            }),
        };
        serde_json::to_value(result).map_err(|e| {
            JsonRpcError::internal_error(format!("failed to serialize InitializeResult: {e}"))
        })
    }

    fn session_new(&self, params: Option<Value>) -> Result<Value, JsonRpcError> {
        // Order matters: reject "session/new before initialize" with
        // INVALID_REQUEST rather than leaking a params-parse error first.
        self.state.require_initialized()?;

        let params: SessionNewParams = match params {
            Some(v) => serde_json::from_value(v)
                .map_err(|e| JsonRpcError::invalid_params(format!("session/new params: {e}")))?,
            None => return Err(JsonRpcError::invalid_params("session/new requires params")),
        };

        self.validate_mcp_servers("session/new", &params.mcp_servers)?;

        let cwd = resolve_session_cwd(&params.cwd, self.default_cwd.as_deref())?;
        let permission_mode = self.config_option_current_value("permissions", "default");
        let record = self.state.create_session_with_permission_mode(
            cwd,
            params.mcp_servers,
            &permission_mode,
        );
        if let Some(init) = &self.session_policy_initializer {
            if let Err(error) = init(&record.id, &record.cwd) {
                self.state.close_session(&record.id);
                return Err(JsonRpcError::internal_error(error));
            }
        }
        // An owner that could not take the lock is a defect, not a degraded
        // mode: nothing would stop a second writer from opening the same
        // session. A freshly minted id is never contended, so reaching this
        // means the lock file could not be written at all.
        if self.state.session_ownership_root().is_some() && !self.state.owns_session(&record.id) {
            self.state.close_session(&record.id);
            return Err(JsonRpcError::internal_error(format!(
                "could not take the active lock for new session {}",
                record.id
            )));
        }

        let slash_commands = acp_advertised_slash_commands();
        let _ = self
            .state
            .set_slash_commands(&record.id, slash_commands.clone());

        let config_options = self.config_options_for_session(&record.id);
        let result = SessionNewResult {
            session_id: record.id,
            config_options: Some(config_options),
            slash_commands: Some(slash_commands),
        };
        serde_json::to_value(result).map_err(|e| {
            JsonRpcError::internal_error(format!("failed to serialize SessionNewResult: {e}"))
        })
    }

    /// Handle a `session/prompt` request.
    ///
    /// Ordering:
    ///
    /// 1. `require_initialized` → INVALID_REQUEST if no `initialize` yet.
    /// 2. Parse params → INVALID_PARAMS on parse failure or missing params.
    /// 3. Look up the session → INVALID_PARAMS `Session not found: …` if
    ///    the id is unknown.
    /// 4. Reject a concurrent prompt on the same session → INVALID_REQUEST
    ///    `Session already has an active prompt turn`.
    /// 5. Execute the prompt turn: kick off an async query
    ///    stream that publishes `session/update` notifications until the
    ///    model is done, records the prompt content on the session, and
    ///    returns an `end_turn` stop reason once the turn completes so
    ///    clients see a structurally complete round-trip.
    ///
    /// The active-prompt slot is acquired in step (4) via
    /// [`ServerState::begin_prompt`] and released before the function
    /// returns.
    async fn session_prompt(&self, params: Option<Value>) -> Result<Value, JsonRpcError> {
        self.state.require_initialized()?;

        let mut params: SessionPromptParams = match params {
            Some(v) => serde_json::from_value(v)
                .map_err(|e| JsonRpcError::invalid_params(format!("session/prompt params: {e}")))?,
            None => {
                return Err(JsonRpcError::invalid_params(
                    "session/prompt requires params",
                ))
            }
        };

        // Slash commands, by the handler the `command-registry` seat carries
        // rather than a list this server keeps. Only
        // `Native` reaches this server's own table of read-only reports;
        // `Prompt` — a skill, or a plugin's prompt-shaped command — expands
        // into the turn, which is what a terminal has always done with it and
        // what this surface used to send to the model with the `/` still on
        // it. Anything this server cannot run goes on as prompt text.
        if let Some(line) = single_text_line(&params.prompt).map(str::to_string) {
            let answer = match resolve_typed_command(self.kernel_scope.as_ref(), &line).await {
                TypedLine::NotACommand => None,
                TypedLine::Native { id, args } => {
                    let record = self
                        .state
                        .get_session(&params.session_id)
                        .ok_or_else(|| session_not_found(&params.session_id))?;
                    format_acp_read_only_command(
                        &record,
                        &id,
                        &args.rest,
                        self.kernel_scope.as_ref(),
                    )
                }
                TypedLine::Say(text) => Some(text),
                // The `ui-registry` seat's panels are drawn by a front end.
                // An editor driving this server has none, and saying so beats
                // an end_turn with nothing in it.
                TypedLine::Panel(dialog) => Some(format!(
                    "That command opens the `{dialog}` panel, which the ACP server has no surface for."
                )),
                TypedLine::Expanded(expanded) => {
                    if let Some(ContentBlock::Text(text)) = params.prompt.first_mut() {
                        text.text = expanded;
                    }
                    None
                }
            };
            if let Some(text) = answer {
                // The session has to exist before an answer is published for
                // it, exactly as the read-only reports required.
                if self.state.get_session(&params.session_id).is_none() {
                    return Err(session_not_found(&params.session_id));
                }
                if let Some(publisher) = &self.update_publisher {
                    publisher
                        .publish_to(
                            &params.session_id,
                            SessionUpdate::AgentMessageChunk {
                                content: ContentBlock::Text(TextContent {
                                    text,
                                    annotations: None,
                                }),
                            },
                        )
                        .await;
                }
                return serde_json::to_value(SessionPromptResult {
                    stop_reason: StopReason::EndTurn,
                })
                .map_err(|e| {
                    JsonRpcError::internal_error(format!(
                        "failed to serialize SessionPromptResult: {e}"
                    ))
                });
            }
        }

        let skill_invocations = skill_invocations_from_meta(params.meta.as_ref())?;
        let turn = self
            .acquire_prompt_turn_queued(&params.session_id, params.prompt.clone())
            .await?;
        let result = self
            .drive_prompt_turn(
                params.session_id,
                params.prompt,
                None,
                turn,
                skill_invocations,
            )
            .await?;
        serde_json::to_value(result).map_err(|e| {
            JsonRpcError::internal_error(format!("failed to serialize SessionPromptResult: {e}"))
        })
    }

    /// Acquire the active-prompt slot for one turn.
    ///
    /// Serializes ownership acquisition with cancellation registration. A
    /// concurrent cancel either linearizes before this critical section (no
    /// prompt exists yet) or after the generation-tagged handle is present.
    /// Pre-clones every guard input before ownership becomes visible so guard
    /// construction itself is infallible once the state transition succeeds.
    fn acquire_prompt_turn(
        &self,
        session_id: &str,
        prompt: Vec<rebon_types::ContentBlock>,
    ) -> Result<AcquiredPromptTurn, JsonRpcError> {
        let lifecycle_state = self.state.clone();
        let lifecycle_cancels = self.active_cancels.clone();
        let lifecycle_session_id = session_id.to_string();
        let cancel = PromptCancel::new();
        let mut map = self
            .active_cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = self.state.begin_prompt_lifecycle(session_id, prompt)?;
        let mut lifecycle = PromptLifecycleGuard::new_during_registration(
            lifecycle_state,
            lifecycle_cancels,
            lifecycle_session_id,
            record.generation,
        );

        #[cfg(test)]
        if let Some(hook) = &self.prompt_setup_hook {
            hook();
        }

        map.insert(
            session_id.to_string(),
            ActivePromptCancel {
                generation: record.generation,
                cancel: cancel.clone(),
            },
        );
        lifecycle.mark_cancel_registered();
        Ok(AcquiredPromptTurn {
            record,
            lifecycle,
            cancel,
        })
    }

    /// Acquire the active-prompt slot, waiting out a steering-spawned turn.
    ///
    /// `session/prompt` requests are serialized by the FIFO request worker,
    /// so a slot collision here means the current owner is a turn that
    /// `_session/steering` spawned off-queue. Rejecting would surface that
    /// implementation detail as an INVALID_REQUEST the client never
    /// provoked; waiting queues the prompt exactly as if the steering turn
    /// had arrived as a `session/prompt` ahead of it. A session that
    /// vanishes while waiting surfaces as `invalid_params` on the retry.
    async fn acquire_prompt_turn_queued(
        &self,
        session_id: &str,
        prompt: Vec<rebon_types::ContentBlock>,
    ) -> Result<AcquiredPromptTurn, JsonRpcError> {
        loop {
            let released = self.state.prompt_slot_released();
            tokio::pin!(released);
            // Register before re-checking the slot so a release between the
            // failed acquire and the await cannot be missed.
            released.as_mut().enable();
            match self.acquire_prompt_turn(session_id, prompt.clone()) {
                Ok(turn) => return Ok(turn),
                // The only INVALID_REQUEST this path produces is the
                // occupied-slot rejection; unknown sessions are
                // invalid_params and initialization is checked upstream.
                Err(err) if err.code == error_code::INVALID_REQUEST => {
                    released.await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Drive one acquired prompt turn through the executor and release
    /// the slot. Shared by `session/prompt` and steering-spawned turns,
    /// so both run the same permission/update wiring and both hand
    /// end-of-turn steering leftovers to a fresh turn.
    ///
    /// `user_message_uuid` carries a steered message's minted identity into
    /// the engine so persistence and the `QueuedUserMessage` echo agree
    /// with what `_session/steering` promised; plain prompts pass `None`
    /// and let the engine mint one.
    async fn drive_prompt_turn(
        &self,
        session_id: String,
        prompt: Vec<rebon_types::ContentBlock>,
        user_message_uuid: Option<String>,
        turn: AcquiredPromptTurn,
        skill_invocations: Vec<SkillInvocationRequest>,
    ) -> Result<SessionPromptResult, JsonRpcError> {
        let AcquiredPromptTurn {
            record,
            lifecycle,
            cancel,
        } = turn;
        let executor = self.prompt_executor.clone();
        let user_prompt = Some(
            prompt
                .iter()
                .filter_map(|block| match block {
                    rebon_types::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let (prompt, workflow_requested) = acp_prompt_with_ultrawork_reminder(prompt);
        let request = PromptRequest {
            user_prompt,
            effort_is_session_default: false,
            session_id: session_id.clone(),
            cwd: record.cwd.clone(),
            prompt,
            mcp_servers: record.mcp_servers,
            update_publisher: self.update_publisher.clone(),
            permission_publisher: self.permission_publisher.clone(),
            cancel: cancel.clone(),
            thinking_budget: None,
            max_tokens: None,
            reasoning_effort_ordinal: None,
            additional_working_directories: Vec::new(),
            coordinator_mode: None,
            coordinator_report_paths: Vec::new(),
            user_message_uuid,
            background_agent_system: None,
            background_agent_tool_filter: None,
            execution_policy: workflow_requested.then(ExecutionPolicy::workflow_controller),
            replay_requests: Vec::new(),
            skill_invocations,
        };

        // Drive the executor. Explicitly dropping the same guard used by every
        // unwind/cancellation path keeps normal completion cleanup identical.
        let outcome = executor.execute(request).await;
        drop(lifecycle);

        // Steering messages that raced this turn's ending never reached
        // the attachment poller; settle them onto a fresh turn now that
        // the slot is free, so an accepted `injected` can't be lost.
        self.settle_steering_leftovers(&session_id);

        let stop_reason = match outcome {
            Ok(outcome) => outcome.stop_reason,
            Err(PromptExecutorError::Cancelled) => StopReason::Cancelled,
            Err(err) => {
                return Err(JsonRpcError::internal_error(format!(
                    "prompt executor failed: {err}"
                )));
            }
        };

        Ok(SessionPromptResult { stop_reason })
    }

    /// Handle a `session/load` request.
    ///
    /// Steps:
    ///
    /// 1. `require_initialized` → `INVALID_REQUEST` if no `initialize` yet.
    /// 2. Parse `SessionLoadParams` → `INVALID_PARAMS` on parse failure
    ///    or missing params.
    /// 3. Delegate to [`ServerState::load_session`], which branches:
    ///    - existing in-memory session → cwd updated if the request
    ///      supplied a non-empty cwd, record returned
    ///    - on-disk transcript present → restored into the session map,
    ///      with the cwd resolved through the same fallback chain
    ///      (`params.cwd || options.cwd || the current process directory`) that
    ///      `session/new` uses
    ///    - anything else → `INVALID_PARAMS: "Session not found: …"`
    /// 4. Restore and return the same ACP slash-command and config-option metadata
    ///    that `session/new` returns, so a resumed client keeps its command menu
    ///    and current permission-mode selection.
    fn session_load(&self, params: Option<Value>) -> Result<Value, JsonRpcError> {
        self.state.require_initialized()?;

        let params: SessionLoadParams = match params {
            Some(v) => serde_json::from_value(v)
                .map_err(|e| JsonRpcError::invalid_params(format!("session/load params: {e}")))?,
            None => return Err(JsonRpcError::invalid_params("session/load requires params")),
        };

        self.validate_mcp_servers("session/load", &params.mcp_servers)?;

        // Resolve the projects root. If the handler was constructed with
        // an explicit override (tests), use that unchanged; otherwise fall
        // back to `${REBON_CONFIG_DIR:-$HOME/.rebon}/projects`. The
        // value is cheap to compute, so we re-resolve on every call
        // rather than cache it at handler construction time.
        let projects_root = self
            .projects_root
            .clone()
            .unwrap_or_else(default_projects_root);

        // `load_session` takes two cwd values: the *raw* request cwd
        // (used unchanged for the "update existing session" short-circuit
        // so that an empty string leaves the live record alone) and the
        // handler-level fallback (used only on the disk path, as the
        // middle link of the `params.cwd || options.cwd || the current process directory`
        // chain that `session/new` also uses).
        let session_already_loaded = self.state.get_session(&params.session_id).is_some();
        let record = self.state.load_session(
            &projects_root,
            &params.session_id,
            &params.cwd,
            self.default_cwd.as_deref(),
            params.mcp_servers,
        )?;
        if let Some(init) = &self.session_policy_initializer {
            init(&record.id, &record.cwd).map_err(JsonRpcError::internal_error)?;
        }
        if !session_already_loaded {
            let permission_mode = self.config_option_current_value("permissions", "default");
            let _ = self.state.set_permission_mode(&record.id, &permission_mode);
        }

        let slash_commands = acp_advertised_slash_commands();
        let _ = self
            .state
            .set_slash_commands(&record.id, slash_commands.clone());
        let config_options = self.config_options_for_session(&record.id);
        let result = SessionLoadResult {
            session_id: record.id,
            config_options: Some(config_options),
            slash_commands: Some(slash_commands),
        };
        serde_json::to_value(result).map_err(|e| {
            JsonRpcError::internal_error(format!("failed to serialize SessionLoadResult: {e}"))
        })
    }

    fn session_set_config_option(&self, params: Option<Value>) -> Result<Value, JsonRpcError> {
        self.state.require_initialized()?;

        let params: SessionSetConfigOptionParams = match params {
            Some(v) => serde_json::from_value(v).map_err(|e| {
                JsonRpcError::invalid_params(format!("session/set_config_option params: {e}"))
            })?,
            None => {
                return Err(JsonRpcError::invalid_params(
                    "session/set_config_option requires params",
                ))
            }
        };

        let updated = if self.is_session_config_option(&params.config_id) {
            if self.state.get_session(&params.session_id).is_none() {
                return Err(JsonRpcError::invalid_params(format!(
                    "session/set_config_option: unknown session {}",
                    params.session_id
                )));
            }
            let session_options = self
                .session_config_options
                .as_ref()
                .expect("session config options exist when one of their ids matched");
            (session_options.apply)(&params.session_id, &params.config_id, &params.value)
                .map_err(JsonRpcError::invalid_params)?;
            self.config_options_for_session(&params.session_id)
        } else {
            self.apply_config_option_update(&params.session_id, &params.config_id, &params.value)
        };

        let result = SessionSetConfigOptionResult {
            config_options: updated,
        };
        serde_json::to_value(result).map_err(|e| {
            JsonRpcError::internal_error(format!(
                "failed to serialize SessionSetConfigOptionResult: {e}"
            ))
        })
    }

    /// `_meta.rebon.owner` for one `session/list` entry.
    ///
    /// `active` answers "is some process writing this session"; `heldHere`
    /// narrows that to "and it is this ACP server", which is the distinction a
    /// client needs between *this is already open in your other window* and
    /// *this is the session you have open right here*. `heldHere` is false on
    /// a surface that has not taken session ownership yet (the terminal still
    /// holds its own lock), so read it as "owned by this server", not as
    /// "owned by this process". Costs one `try_lock` per listed session — the
    /// same probe `filter_active_sessions` already pays — and short-circuits
    /// on the sessions this server holds.
    fn session_owner_meta(
        &self,
        projects_root: &Path,
        cwd: &str,
        session_id: &str,
    ) -> (Option<HashMap<String, Value>>, bool) {
        let held_here = self.state.owns_session(session_id);
        let active = held_here
            || rebon_session::session_storage::is_session_active(projects_root, cwd, session_id);
        let owner = json!({ "active": active, "heldHere": held_here });
        let meta = HashMap::from([("rebon".to_string(), json!({ "owner": owner }))]);
        (Some(meta), active)
    }

    /// Handle a `session/list` request.
    ///
    /// Steps:
    ///
    /// 1. `require_initialized` → `INVALID_REQUEST` if no `initialize` yet.
    /// 2. Params are optional; `None` and `{}` are both valid and are
    ///    normalized to an empty [`SessionListParams`]. A structurally
    ///    malformed shape (e.g. `{ "cwd": 42 }`) returns
    ///    `INVALID_PARAMS` with a useful prefix, matching the crate's
    ///    existing defensive serde pattern.
    /// 3. `cursor` is accepted and ignored; pagination is not yet
    ///    implemented.
    /// 4. Delegate to [`ServerState::list_sessions`], including the
    ///    asymmetry where an omitted/blank cwd returns every
    ///    in-memory session but only disk placeholders under the
    ///    effective cwd's project directory.
    /// 5. Build `SessionInfo` entries from the returned records. The
    ///    `updatedAt` field is derived from the record's in-memory
    ///    `created_at` via [`format_system_time_iso_ms`], an ISO-8601
    ///    millisecond timestamp, so disk-placeholder mtimes and in-memory
    ///    timestamps land on the same wire shape.
    /// 6. Omit `nextCursor` from the wire (pagination deferred). `_meta`
    ///    carries `rebon.owner` — see [`Self::session_owner_meta`].
    fn session_list(&self, params: Option<Value>) -> Result<Value, JsonRpcError> {
        // Order matters: reject "session/list before initialize" before
        // doing anything else.
        self.state.require_initialized()?;

        // Normalize the params into a `SessionListParams`. `None` and
        // `{}` are both valid; a malformed shape (wrong types on
        // optional fields) returns INVALID_PARAMS with a useful prefix.
        let params: SessionListParams = match params {
            None => SessionListParams::default(),
            Some(v) => serde_json::from_value(v)
                .map_err(|e| JsonRpcError::invalid_params(format!("session/list params: {e}")))?,
        };

        // `cursor` is accepted and ignored — binding to `_` so clippy
        // doesn't flag the unused field and so the intent is explicit.
        let _ = params.cursor;

        let projects_root = self
            .projects_root
            .clone()
            .unwrap_or_else(default_projects_root);

        let records = self.state.list_sessions(
            &projects_root,
            params.cwd.as_deref(),
            self.default_cwd.as_deref(),
        )?;

        // One ownership probe per session, whose answer serves both the
        // `_meta` payload and the active-session filter — probing twice would
        // double the `try_lock` cost of every list on the surface that filters.
        let mut sessions: Vec<(SessionInfo, bool)> = records
            .into_iter()
            .map(|r| {
                let (meta, active) = self.session_owner_meta(&projects_root, &r.cwd, &r.id);
                (
                    SessionInfo {
                        session_id: r.id,
                        cwd: r.cwd,
                        title: r.title,
                        updated_at: Some(format_system_time_iso_ms(r.created_at)),
                        meta,
                    },
                    active,
                )
            })
            .collect();

        if self.filter_active_sessions {
            sessions.retain(|(_, active)| !active);
        }
        let sessions: Vec<SessionInfo> = sessions.into_iter().map(|(info, _)| info).collect();

        // `next_cursor` stays `None` so the wire format omits it
        // entirely, giving the usual bare `{ sessions: [...] }` shape.
        let result = SessionListResult {
            sessions,
            next_cursor: None,
        };
        serde_json::to_value(result).map_err(|e| {
            JsonRpcError::internal_error(format!("failed to serialize SessionListResult: {e}"))
        })
    }

    /// Handle a `_session/steering` extension request: inject a user
    /// message into the session's running prompt turn instead of
    /// queueing a separate `session/prompt`.
    ///
    /// The request reaches this handler through the serve loop's
    /// steering bypass worker, so it is processed while a
    /// `session/prompt` for the same session is still executing on the
    /// FIFO request worker. Outcomes mirror the adapter contract Rebon's
    /// own ACP client consumes:
    ///
    /// - `injected` — the message is queued for the active turn; the
    ///   host's attachment poller delivers it between tool rounds (and
    ///   once more before a would-be-final response), persisting it and
    ///   echoing a `QueuedUserMessage` update.
    /// - `startedNewTurn` — no turn was running, so the message became
    ///   the prompt of a fresh fire-and-forget turn whose output flows
    ///   through `session/update` with nobody awaiting a prompt
    ///   response.
    async fn session_steering(&self, params: Option<Value>) -> Result<Value, JsonRpcError> {
        // No sink wired means initialize never advertised steering;
        // keep the method surface consistent with the advertisement.
        let Some(sink) = self.steering_sink.clone() else {
            return Err(JsonRpcError::method_not_found("_session/steering"));
        };
        self.state.require_initialized()?;

        let params: SessionSteeringParams = match params {
            Some(v) => serde_json::from_value(v).map_err(|e| {
                JsonRpcError::invalid_params(format!("_session/steering params: {e}"))
            })?,
            None => {
                return Err(JsonRpcError::invalid_params(
                    "_session/steering requires params",
                ))
            }
        };
        if self.state.get_session(&params.session_id).is_none() {
            return Err(JsonRpcError::invalid_params(format!(
                "Session not found: {}",
                params.session_id
            )));
        }
        if params.prompt.is_empty() {
            return Err(JsonRpcError::invalid_params(
                "_session/steering requires a non-empty prompt",
            ));
        }

        sink.enqueue(
            &params.session_id,
            vec![SteeringMessage {
                prompt: params.prompt,
                user_message_uuid: next_steering_uuid(),
            }],
        );
        let outcome = self.settle_steering(&params.session_id, &sink)?;

        serde_json::to_value(SessionSteeringResult { outcome }).map_err(|e| {
            JsonRpcError::internal_error(format!("failed to serialize SessionSteeringResult: {e}"))
        })
    }

    /// Converge queued steering messages onto a turn.
    ///
    /// The invariant this loop maintains: whenever the session's
    /// steering queue is non-empty, either an active turn exists (its
    /// poller — or its end-of-turn leftover settle — owns delivery), or
    /// some caller is inside this loop about to spawn a turn. Losing the
    /// slot race re-queues the messages and retries, so a message can
    /// never strand in an empty-slot/non-empty-queue limbo.
    fn settle_steering(
        &self,
        session_id: &str,
        sink: &Arc<dyn SteeringSink>,
    ) -> Result<SteeringOutcome, JsonRpcError> {
        loop {
            if self.state.is_prompt_active(session_id) {
                return Ok(SteeringOutcome::Injected);
            }
            let pending = sink.drain_pending(session_id);
            if pending.is_empty() {
                // A live turn's poller (or a concurrent settle) already
                // took responsibility for every queued message.
                return Ok(SteeringOutcome::Injected);
            }
            let prompt: Vec<rebon_types::ContentBlock> = pending
                .iter()
                .flat_map(|message| message.prompt.iter().cloned())
                .collect();
            // Messages merged into one turn can carry only one identity;
            // the first is the message whose uuid the earliest caller was
            // promised.
            let user_message_uuid = pending
                .first()
                .map(|message| message.user_message_uuid.clone());
            match self.acquire_prompt_turn(session_id, prompt.clone()) {
                Ok(turn) => {
                    let handler = self.clone();
                    let spawn_session_id = session_id.to_string();
                    tokio::spawn(async move {
                        if let Err(err) = handler
                            .drive_prompt_turn(
                                spawn_session_id.clone(),
                                prompt,
                                user_message_uuid,
                                turn,
                                Vec::new(),
                            )
                            .await
                        {
                            tracing::warn!(
                                session_id = %spawn_session_id,
                                error = ?err,
                                "rebon-acp steering-spawned prompt turn failed"
                            );
                        }
                    });
                    return Ok(SteeringOutcome::StartedNewTurn);
                }
                Err(err) if err.code == error_code::INVALID_REQUEST => {
                    // A prompt won the slot between the active check and
                    // the acquire; give the messages back so that turn's
                    // poller (or its leftover settle) delivers them.
                    sink.requeue_front(session_id, pending);
                    continue;
                }
                Err(err) => {
                    // Session vanished mid-request (not a state this
                    // server currently produces). Surface the error
                    // rather than pretending the message was delivered.
                    return Err(err);
                }
            }
        }
    }

    /// End-of-turn half of the steering delivery contract: settle any
    /// messages the finished turn's poller never picked up. Errors are
    /// logged, not returned — the finished turn's own response must not
    /// change because a follow-up turn could not be spawned.
    fn settle_steering_leftovers(&self, session_id: &str) {
        let Some(sink) = self.steering_sink.clone() else {
            return;
        };
        if let Err(err) = self.settle_steering(session_id, &sink) {
            tracing::warn!(
                session_id = %session_id,
                error = ?err,
                "rebon-acp failed to settle leftover steering messages"
            );
        }
    }

    /// Handle a `session/cancel` notification.
    ///
    /// Records the cancel on the shared [`ServerState`] so tests can
    /// observe that the notification was delivered, and trips the
    /// active prompt's cancel handle (if any) so the executor can abort.
    ///
    /// Malformed params are logged and dropped — notifications have no
    /// response path, so there's no error to send back; this follows the
    /// "log and ignore" behavior used for unrecognized notifications.
    fn handle_session_cancel(&self, params: Option<Value>) {
        let params: SessionCancelParams = match params {
            Some(v) => match serde_json::from_value(v) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "rebon-acp ignoring malformed session/cancel params"
                    );
                    return;
                }
            },
            None => {
                tracing::warn!("rebon-acp ignoring session/cancel with no params");
                return;
            }
        };
        self.state.record_cancel(&params.session_id);
        // If a prompt is in flight for this session, trip its cancel
        // handle so the executor can abort.
        let maybe_cancel = self
            .active_cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&params.session_id)
            .map(|entry| entry.cancel.clone());
        if let Some(cancel) = maybe_cancel {
            cancel.cancel();
        }
    }
}

#[async_trait]
impl RequestHandler for DefaultHandler {
    async fn handle_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, JsonRpcError> {
        match method {
            "initialize" => self.initialize(params),
            "session/new" => self.session_new(params),
            "session/load" => self.session_load(params),
            "session/list" => self.session_list(params),
            "session/prompt" => self.session_prompt(params).await,
            "session/set_config_option" => self.session_set_config_option(params),
            "_session/steering" => self.session_steering(params).await,
            _ => Err(JsonRpcError::method_not_found(method)),
        }
    }

    fn permission_session_cwd(&self, session_id: &str) -> Option<String> {
        self.state.session_storage_cwd(session_id)
    }

    fn apply_allow_always_rules(
        &self,
        session_id: &str,
        rules: &[rebon_permissions::PermissionRuleValue],
    ) -> Result<(), String> {
        let cwd = self
            .permission_session_cwd(session_id)
            .ok_or("permission session not found")?;
        let apply = self
            .allow_always_applier
            .as_ref()
            .ok_or("session live permission policy is unavailable")?;
        apply(session_id, &cwd, rules)
    }

    async fn handle_notification(&self, method: &str, params: Option<Value>) {
        match method {
            "session/cancel" => self.handle_session_cancel(params),
            // Unknown notifications are silently ignored per JSON-RPC 2.0
            // ("log and ignore" for unrecognized notifications).
            _ => {
                tracing::debug!(
                    method = %method,
                    "rebon-acp ignoring unrecognized notification"
                );
            }
        }
    }
}
