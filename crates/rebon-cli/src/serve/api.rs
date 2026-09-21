//! The `/api/*` routes beside the ACP socket: what this server is, what a
//! session said before the page was opened, what the plugin plane, the
//! task registry, the skill and agent registries and the MCP servers hold,
//! and the few actions ACP has no method for (rewind, compaction, task
//! and MCP control).
//!
//! Everything a turn *does* goes over ACP on the WebSocket. These are the
//! facts and levers ACP has no method for — `session/load` does not
//! replay a transcript to the client, the plane's state is not a
//! session's, and `/rewind` is a local surface's command — read straight
//! from where they live and projected as JSON. The page is the TUI's
//! peer here, not an editor's: what `/tasks`, `/skills`, `/agents`,
//! `/mcp`, `/rewind`, `/compact` and `/model` show in the terminal is
//! what these answer.

use std::path::Path;

use rebon_api::{ContentBlock, Role};
use rebon_plugin_skill::SkillSource;
use rebon_proto::web_api::{
    ActionResult, AgentEntry, AgentOption, AgentsResponse, CommandsResponse, HistorySnapshot,
    KernelLoopOption, McpServerEntry, McpStatus, ModelOption, ModelsResponse, OutOfRangeRuntime,
    PluginEntry, PluginPlaneStatus, PluginRuntimeStatus, PluginStatus, ResolvedRuntime,
    RewindCheckpoint, RewindResponse, ServerInfo, SkillEntry, SkillsResponse, TaskEntry,
    TasksResponse, UsageResponse,
};
use rebon_slash_commands::{for_surface, Surface};
use rebon_tool::{AgentRegistrySettingSource as Setting, AgentRegistrySource as Source};
use serde_json::Value;

use super::ServeContext;

/// A session id as it appears in a transcript file name. Anything else is
/// not a session id and must not become part of a path.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// A session's conversation so far.
///
/// Two projections of the same transcript, for one release only.
///
/// `rows` is the answer: the display row stream
/// [`rebon_render::transcript_replay::replayed_rows`] produces, which is
/// what the terminal draws. Injected reminders are taken back out of the
/// user's own words rather than deleted whole, tools that render in their
/// own surface are gone, each persisted tool result is folded onto the
/// `tool_use` block it belongs to, and an approved `ExitPlanMode` leaves its
/// plan card. A page reading these rows shows what the terminal shows.
///
/// `messages` is the old answer and **nothing reads it any more** — the
/// page reads `rows` now. It is the model's view —
/// `transcript_to_api_messages`, the same projection a resume replays —
/// with whole injected blocks dropped by [`is_injected_context`]. It shows
/// hidden tools, it deletes a user block outright where the row stream
/// keeps the half the person typed, and it makes a client fold the
/// transcript a second, different way.
///
/// It is served one more release for a client built against it, and is
/// marked Deprecated in the cli changelog. **Delete it, [`is_injected_context`],
/// and the `messages` half of `HistoryResponse` in
/// `assets/web-ui/acp/types.generated.ts` in the release after the one that carries
/// this comment** — there is no condition to wait for beyond that release
/// shipping, because the only first-party reader is already gone.
pub fn session_history(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Result<HistorySnapshot, String> {
    if !valid_session_id(session_id) {
        return Err("not a session id".to_string());
    }
    let path = rebon_session::session_storage::transcript_file_path(projects_root, cwd, session_id);
    let loaded = rebon_session::session_storage::load_transcript_from_file(&path)
        .map_err(|err| format!("transcript unreadable: {err}"))?
        .ok_or_else(|| format!("no transcript for session {session_id} in {cwd}"))?;
    let lines = crate::session::transcript_replay::transcript_lines(&loaded.messages);
    let (rows, _stats) = rebon_render::transcript_replay::replayed_rows(&lines);
    let rows = serde_json::to_value(&rows).map_err(|err| format!("rows unserializable: {err}"))?;
    let mut messages = rebon_core::query::transcript_to_api_messages(&loaded.messages);
    for message in &mut messages {
        if message.role == Role::User {
            message.content.retain(|block| !is_injected_context(block));
        }
    }
    messages.retain(|message| !message.content.is_empty());
    Ok(HistorySnapshot {
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        title: loaded.title,
        rows,
        messages: serde_json::to_value(&messages)
            .map_err(|err| format!("messages unserializable: {err}"))?,
    })
}

/// Text the engine injected beside the user's own words — runtime context,
/// attachment reminders — which the TUI never shows as the user's message
/// and the page should not either. The model still sees it on resume.
///
/// A prefix test that deletes the whole block, which is not what the
/// terminal does: `replayed_rows` takes the reminder out and keeps the rest
/// of the row, so a prompt that opened with a reminder still shows the words
/// the person typed. This is the third of the three copies of this rule in
/// the tree, and it dies with the `messages` field it serves —
/// next release, per the note on [`session_history`].
fn is_injected_context(block: &ContentBlock) -> bool {
    block
        .as_text()
        .map(|text| {
            let text = text.trim_start();
            text.starts_with("<system-reminder>")
                || text.starts_with("<runtime_context>")
                || text.starts_with("<attachment")
        })
        .unwrap_or(false)
}

/// What this server is: version, workspace, the provider and model turns
/// run on, the agents a session can be switched to, the sessions with a
/// turn running, where the page came from, and the ACP capabilities
/// negotiated with the engine.
pub fn server_info(context: &ServeContext) -> ServerInfo {
    let runtime = context.runtime_model.get();
    ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        cwd: context.cwd.to_string_lossy().into_owned(),
        provider: runtime.provider_name,
        model: runtime.model,
        agents: context
            .agents
            .option_values()
            .into_iter()
            .map(|(id, label)| AgentOption { id, label })
            .collect(),
        agent_config_option: crate::acp::AGENT_CONFIG_OPTION.to_string(),
        active_turns: context.mux.active_turns(),
        web_ui: context.assets.describe(),
        steering: true,
        initialize: context.mux.initialize_result(),
    }
}

// ---- slash commands ----------------------------------------------------------------

/// The `/` menu: the commands the page runs, then every enabled
/// user-invocable skill — the registry's, so a plugin's or a project's
/// commands appear exactly as they do in the terminal.
///
/// Which commands the page runs is the catalog's `WEB` surface bit, not a
/// list here. A list of twenty-seven names used to be, and it said the same
/// thing the bit says — the bit was added to replace it and the list stayed —
/// so a plugin registering a command for the page had to be added in two
/// places, and adding it in one produced a command the page offers and does
/// not run, or runs and does not offer.
pub fn commands(context: &ServeContext) -> CommandsResponse {
    let mut commands: Vec<Value> = for_surface(Surface::Web)
        .iter()
        .map(CommandsResponse::catalog_entry)
        .collect();
    for skill in context.skill_registry.user_invocable_entries() {
        commands.push(CommandsResponse::skill_entry(
            &skill.id,
            &skill.description,
            skill.argument_hint.as_deref(),
        ));
    }
    CommandsResponse { commands }
}

// ---- skills / agents ----------------------------------------------------------------

pub fn skills(context: &ServeContext) -> SkillsResponse {
    let disabled = context.skill_registry.disabled_skills();
    SkillsResponse {
        skills: context
            .skill_registry
            .all_entries()
            .into_iter()
            .map(|skill| SkillEntry {
                enabled: !disabled.contains(&skill.id),
                name: skill.id,
                title: skill.title,
                description: skill.description,
                source: skill_source_label(skill.source).to_string(),
                path: skill.skill_root,
                user_invocable: skill.user_invocable,
                hint: skill.argument_hint,
            })
            .collect(),
    }
}

fn skill_source_label(source: SkillSource) -> &'static str {
    match source {
        SkillSource::BuiltIn => "builtin",
        SkillSource::User => "user",
        SkillSource::Project => "project",
        SkillSource::Managed => "managed",
        SkillSource::Local => "local",
        SkillSource::Flag => "flag",
        SkillSource::Plugin => "plugin",
        SkillSource::Mcp => "mcp",
    }
}

pub fn agent_definitions(context: &ServeContext) -> AgentsResponse {
    AgentsResponse {
        agents: context
            .agent_registry
            .active_snapshot()
            .into_iter()
            .map(|def| AgentEntry {
                source: agent_source_label(&def.source),
                runtime: def.runtime.as_str().to_string(),
                tools: def.tool_filter.allow_list(),
                name: def.agent_type,
                description: def.when_to_use,
                model: def.model,
                provider: def.provider,
                effort: def.effort,
                background: def.background,
            })
            .collect(),
    }
}

fn agent_source_label(source: &Source) -> String {
    match source {
        Source::BuiltIn => "builtin".to_string(),
        Source::Plugin(name) => format!("plugin:{name}"),
        Source::Settings(Setting::UserSettings) => "user".to_string(),
        Source::Settings(Setting::ProjectSettings) => "project".to_string(),
        Source::Settings(Setting::LocalSettings) => "local".to_string(),
        Source::Settings(Setting::FlagSettings) => "flag".to_string(),
        Source::Settings(Setting::PolicySettings) => "policy".to_string(),
    }
}

// ---- tasks ------------------------------------------------------------------------------

pub fn tasks(context: &ServeContext, session_id: &str) -> Result<TasksResponse, String> {
    let registry = context.task_registry_resolver.resolve(session_id)?;
    let mut snapshots = registry.snapshots();
    snapshots.sort_by(|a, b| b.start_time_ms.cmp(&a.start_time_ms));
    Ok(TasksResponse {
        tasks: snapshots
            .into_iter()
            .map(|task| TaskEntry {
                id: task.id.0,
                kind: task.kind.as_str().to_string(),
                status: task.status.as_str().to_string(),
                title: task.title,
                description: task.last_progress,
                error: task.error,
                output: task.result.as_ref().map(|result| match result {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                }),
                backgrounded: task.is_backgrounded,
                created_at: iso_ms(task.start_time_ms),
                updated_at: task.end_time_ms.and_then(iso_ms),
            })
            .collect(),
    })
}

pub fn task_action(
    context: &ServeContext,
    session_id: &str,
    body: &Value,
) -> Result<ActionResult, String> {
    let registry = context.task_registry_resolver.resolve(session_id)?;
    let id = body["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("id is required")?;
    let task_id = rebon_plugin_tasks::runtime::TaskId(id.to_string());
    match body["action"].as_str() {
        Some("cancel") => {
            if registry.cancel(&task_id) {
                Ok(ActionResult::new(true, format!("Cancelled {id}")))
            } else {
                Err(format!("{id} is not a running task"))
            }
        }
        Some("dismiss") => {
            if registry.remove(&task_id).is_some() {
                Ok(ActionResult::new(true, format!("Removed {id}")))
            } else {
                Err(format!("no task {id}"))
            }
        }
        _ => Err("action must be cancel or dismiss".to_string()),
    }
}

fn iso_ms(ms: u64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|when| when.to_rfc3339())
}

// ---- MCP --------------------------------------------------------------------------------

pub async fn mcp_status(context: &ServeContext) -> McpStatus {
    let configured = crate::mcp_config::collect_default_mcp_configs_with_plugin_overrides(
        &context.cwd,
        &context.mcp.runtime_configs,
        context.mcp.strict,
        &context.mcp.plugin_configs,
    );
    let (configured, problem) = match configured {
        Ok(list) => (list, None),
        Err(err) => (Vec::new(), Some(err.to_string())),
    };
    let connected: Vec<String> = context
        .mcp
        .client
        .as_ref()
        .map(|client| client.server_names())
        .unwrap_or_default();
    let mut servers = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in &configured {
        let name = item.config.name().to_string();
        seen.insert(name.clone());
        servers.push(server_entry(context, &name, Some(&item.config), &connected));
    }
    // A server the client runs that no config names (an env-declared one).
    for name in &connected {
        if seen.insert(name.clone()) {
            servers.push(server_entry(context, name, None, &connected));
        }
    }
    McpStatus {
        servers,
        problem,
        warnings: context.mcp.warnings.clone(),
    }
}

fn server_entry(
    context: &ServeContext,
    name: &str,
    config: Option<&crate::mcp_config::McpServerConfig>,
    connected: &[String],
) -> McpServerEntry {
    use crate::mcp_config::McpServerConfig;
    let transport = config.map(|config| {
        match config {
            McpServerConfig::Stdio(_) => "stdio",
            McpServerConfig::Http(_) => "http",
            McpServerConfig::Sse(_) => "sse",
        }
        .to_string()
    });
    let is_connected = connected.iter().any(|candidate| candidate == name);
    let warning = context
        .mcp
        .warnings
        .iter()
        .find(|warning| warning.contains(name))
        .cloned();
    let tools = if is_connected {
        context
            .mcp
            .client
            .as_ref()
            .and_then(|client| client.cached_tool_definitions(name))
            .map(|definitions| {
                definitions
                    .iter()
                    .map(|definition| rebon_tool::build_mcp_tool_name(name, &definition.name))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let status = if is_connected {
        "connected"
    } else if warning.is_some() {
        "failed"
    } else if context.mcp.client.is_none() {
        "not started"
    } else {
        "disconnected"
    };
    McpServerEntry {
        name: name.to_string(),
        status: status.to_string(),
        transport,
        tools,
        error: warning,
    }
}

pub async fn mcp_action(context: &ServeContext, body: &Value) -> Result<ActionResult, String> {
    let name = body["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or("name is required")?;
    let client = context
        .mcp
        .client
        .as_ref()
        .ok_or("no MCP client is running in this server")?;
    match body["action"].as_str() {
        Some("reconnect") => match client.reconnect_server(name).await {
            Ok(true) => Ok(ActionResult::new(true, format!("Reconnected {name}"))),
            Ok(false) => Err(format!("{name} is not a server this client knows")),
            Err(err) => Err(format!("{name}: {err}")),
        },
        Some("disconnect") => {
            if client.disconnect_server(name).await {
                Ok(ActionResult::new(true, format!("Disconnected {name}")))
            } else {
                Err(format!("{name} is not connected"))
            }
        }
        _ => Err("action must be reconnect or disconnect".to_string()),
    }
}

// ---- models -------------------------------------------------------------------------------

pub fn models(context: &ServeContext) -> ModelsResponse {
    let runtime = context.runtime_model.get();
    let provider_name = rebon_config::get_active_custom_provider_name();
    let provider = provider_name.as_ref().and_then(|name| {
        rebon_config::list_custom_providers()
            .into_iter()
            .find(|candidate| candidate.name.eq_ignore_ascii_case(name))
    });
    let mut options = provider
        .as_ref()
        .map(rebon_config::provider_model_options)
        .unwrap_or_default();
    if !runtime.model.is_empty() && !options.iter().any(|model| *model == runtime.model) {
        options.insert(0, runtime.model.clone());
    }
    ModelsResponse {
        provider: provider_name.or(Some(runtime.provider_name.clone())),
        current: runtime.model,
        models: options.into_iter().map(|id| ModelOption { id }).collect(),
    }
}

// ---- usage ---------------------------------------------------------------------------------

/// The token ledger for a session.
///
/// The owner counts it: a worker sees every turn of its session, including
/// the ones that ran before this server started or in another client. The
/// ledger the mux keeps from the `token_usage` updates passing through is
/// the same number a moment fresher, and it is what answers for a session
/// whose owner is not reachable right now.
pub fn usage(context: &ServeContext, session_id: &str, cwd: &str) -> UsageResponse {
    let owner_usage = context
        .hosted
        .reachable_owner(session_id, cwd)
        .and_then(|owner| owner.status().ok())
        .and_then(|status| status.usage);
    let usage = context.mux.usage(session_id);
    let runtime = context.runtime_model.get();
    let created = context
        .handler_state
        .get_session(session_id)
        .map(|record| record.created_at)
        .and_then(|created| created.elapsed().ok())
        .map(|elapsed| elapsed.as_millis() as u64);
    let messages = rebon_session::session_storage::load_transcript_from_file(
        &rebon_session::session_storage::transcript_file_path(
            &context.projects_root,
            cwd,
            session_id,
        ),
    )
    .ok()
    .flatten()
    .map(|loaded| loaded.messages.len());
    let (input_tokens, output_tokens) = match &owner_usage {
        Some(owner) => (
            owner.input_tokens.max(usage.input_tokens) + usage.current_input,
            owner.output_tokens.max(usage.output_tokens) + usage.current_output,
        ),
        None => (
            usage.input_tokens + usage.current_input,
            usage.output_tokens + usage.current_output,
        ),
    };
    UsageResponse {
        session_id: session_id.to_string(),
        input_tokens,
        output_tokens,
        turns: usage.turns,
        messages,
        duration_ms: created,
        model: runtime.model,
        cache_read_tokens: owner_usage.as_ref().map(|usage| usage.cache_read_tokens),
        cache_creation_tokens: owner_usage
            .as_ref()
            .map(|usage| usage.cache_creation_tokens),
        problem: if owner_usage.is_some() {
            "Counted by the session's own host, which sees every turn."
        } else {
            "Counted from this server's own turns; earlier turns are not on record."
        }
        .to_string(),
    }
}

// ---- rewind ----------------------------------------------------------------------------------

/// The points a session can be rewound to: its user turns, newest last.
pub fn rewind_list(
    context: &ServeContext,
    session_id: &str,
    cwd: &str,
) -> Result<RewindResponse, String> {
    if !valid_session_id(session_id) {
        return Err("not a session id".to_string());
    }
    let path = rebon_session::transcript_file_path(&context.projects_root, cwd, session_id);
    let history = rebon_session::load_session_history(&path)
        .map_err(|err| format!("transcript unreadable: {err}"))?;
    let checkpoints = history
        .turns
        .iter()
        .map(|turn| {
            let mut preview: String = turn.prompt.chars().take(240).collect();
            if preview.len() < turn.prompt.len() {
                preview.push('…');
            }
            RewindCheckpoint {
                index: turn.turn_number,
                uuid: turn.user_uuid.clone(),
                timestamp: turn.timestamp.clone(),
                preview,
                complete: matches!(
                    turn.completion_state,
                    rebon_session::HistoryTurnCompletion::Complete
                ),
            }
        })
        .collect::<Vec<_>>();
    // A rewind is the owner's to perform, and a worker keeps file-history
    // snapshots exactly as a terminal does — so the files half is available
    // here for as long as this session has a reachable host.
    let hosted = context.hosted.reachable_owner(session_id, cwd).is_some();
    Ok(RewindResponse {
        checkpoints,
        supported: true,
        files_supported: hosted,
        problem: (!hosted).then(|| {
            "File restore is not available for a session with no reachable host: the snapshots \
             are the host's. The conversation can be rewound; use `rebon` in the terminal to \
             restore files."
                .to_string()
        }),
    })
}

/// Rewind a session's conversation to a user turn. The transcript on disk
/// is rewritten with the same compare-and-swap the TUI uses, and the
/// session is dropped from memory so its next `session/load` reads the
/// rewound transcript back.
pub fn rewind_apply(context: &ServeContext, body: &Value) -> Result<ActionResult, String> {
    let session_id = body["sessionId"].as_str().ok_or("sessionId is required")?;
    if !valid_session_id(session_id) {
        return Err("not a session id".to_string());
    }
    let cwd = body["cwd"]
        .as_str()
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| context.cwd.to_string_lossy().into_owned());
    let uuid = body["uuid"].as_str().ok_or("uuid is required")?;
    let mode = body["mode"].as_str().unwrap_or("conversation");
    // The owner rewrites its own transcript. It is the only process that
    // can: the rewrite is a compare-and-swap against the conversation chain
    // it holds in memory, and it is the one holding the file-history
    // snapshots the `code` and `both` scopes restore from.
    if let Some(owner) = context.hosted.reachable_owner(session_id, &cwd) {
        let scope = match mode {
            "conversation" => rebon_session_host::RewindScopeWire::Conversation,
            "code" => rebon_session_host::RewindScopeWire::Code,
            "both" => rebon_session_host::RewindScopeWire::Both,
            other => return Err(format!("unknown rewind mode {other}")),
        };
        let response = owner
            .send(
                rebon_session_host::BackgroundIpcRequest::Rewind {
                    user_message_uuid: uuid.to_string(),
                    scope,
                },
                Some(rebon_session_host::generate_command_id()),
            )
            .map_err(|err| format!("rewind failed: {err}"))?;
        let message = response
            .data
            .as_ref()
            .and_then(|data| data.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("Rewound")
            .to_string();
        return Ok(ActionResult::new(true, message));
    }
    if mode != "conversation" {
        return Ok(ActionResult::new(
            false,
            "File restore needs the session's host: the snapshots are its. Choose \
             `conversation`, or open the session so its worker starts.",
        ));
    }
    if context.mux.is_turn_running(session_id) {
        return Err("a turn is running in this session; stop it first".to_string());
    }
    // A rewind rewrites the transcript, so it belongs to whoever owns the
    // session. When the server is that owner (a page opened the session here,
    // and the state took its active lock) it rewrites in place. Otherwise it
    // has to prove nobody else is writing, and it proves it the way every
    // other surface does: by holding the lock across the whole read-CAS-write,
    // acquired *before* the history stamp this compares against is read.
    let witness = if context.handler_state.owns_session(session_id) {
        None
    } else {
        Some(
            rebon_session::try_acquire_session_active_lock(
                &context.projects_root,
                &cwd,
                session_id,
            )
            .map_err(|err| format!("could not evaluate the session lock: {err}"))?
            .ok_or("that session is open in another Rebon process; close it there first")?,
        )
    };
    let path = rebon_session::transcript_file_path(&context.projects_root, &cwd, session_id);
    let history = rebon_session::load_session_history(&path)
        .map_err(|err| format!("transcript unreadable: {err}"))?;
    let turn = history
        .turns
        .iter()
        .find(|turn| turn.user_uuid == uuid)
        .ok_or("that turn is no longer in the transcript")?;
    let request = rebon_session::RewindConversationRequest {
        mutation_id: format!("serve-{uuid}-{}", now_ms()),
        session_id: session_id.to_string(),
        cwd: cwd.clone(),
        selected_user_uuid: turn.user_uuid.clone(),
        boundary_parent_uuid: turn.parent_uuid.clone(),
        selected_prompt: turn.prompt.clone(),
        expected_sha256: history.stamp.sha256,
        expected_source_head_uuid: history.source_head_uuid.clone(),
    };
    let receipt = match witness.as_ref() {
        Some(lock) => {
            rebon_session::rewind_conversation_locked(&context.projects_root, &path, &request, lock)
        }
        None => rebon_session::rewind_conversation(&context.projects_root, &path, &request),
    }
    .map_err(|err| format!("rewind failed: {err}"))?;
    // The in-memory session still holds the longer conversation; drop it so
    // the page's reload reads the rewound transcript.
    context.handler_state.evict_session(session_id);
    Ok(ActionResult {
        ok: true,
        message: format!("Rewound to turn {}", turn.turn_number),
        prefill_prompt: Some(receipt.prefill_prompt),
        turns: Some(receipt.canonical_history.turns.len()),
    })
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---- compaction ---------------------------------------------------------------------------------

/// Arm a one-shot compaction, as `/compact` does in the terminal: the
/// budget middleware compacts at the start of the next model request.
///
/// Which budget, though, is the question: the session's
/// turns run in its worker, so arming this process's middleware would
/// compact a context nobody is using. A session with a host is asked; only
/// a session without one falls back to the local handle.
pub fn compact(context: &ServeContext, body: &Value) -> ActionResult {
    let instructions = body["instructions"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    if let Some(session_id) = body["sessionId"].as_str() {
        let cwd = body["cwd"]
            .as_str()
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| context.cwd.to_string_lossy().into_owned());
        if let Some(owner) = context.hosted.reachable_owner(session_id, &cwd) {
            return match owner.send(
                rebon_session_host::BackgroundIpcRequest::Compact {
                    instructions: instructions.clone(),
                },
                Some(rebon_session_host::generate_command_id()),
            ) {
                Ok(response) => ActionResult::new(
                    true,
                    response
                        .data
                        .as_ref()
                        .and_then(|data| data.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("Compaction armed in the session's host."),
                ),
                Err(err) => ActionResult::new(false, err.to_string()),
            };
        }
    }
    let handle = &context.prune_level;
    handle
        .budget
        .force_compact_once_with_instructions(instructions.clone());
    let note = instructions
        .map(|text| format!("\n\nCompact instructions:\n{text}"))
        .unwrap_or_default();
    ActionResult::new(
        true,
        format!(
            "Compaction armed: it runs at the start of the next model request.{note}\n\nBefore compact:\n{}",
            handle.budget.context_info()
        ),
    )
}

/// The plugin plane as this process sees it: the Node runtime it would
/// run on, the composition `kernelPlugins` asked for and what of it is
/// loaded, why the rest is not, and the kernel loop vendors a session can
/// be switched to.
///
/// This is the page's answer to "is my dsh plugin running": a plugin that
/// starts and is then refused at every registration is a worse failure than
/// one that does not start, so the skipped list carries the reason each
/// entry was left out.
pub async fn plugin_status(
    agents: &crate::session_agent_router::SessionAgentRouter,
) -> PluginStatus {
    use rebon_plugin_host::plugin_boot;
    use rebon_plugin_host::plugin_composition::{
        payload_root, plane_composition, CompositionRoots,
    };

    let config_dir = rebon_config::config_home_dir();
    let runtime = plugin_boot::plane_runtime_status();
    let plane = plugin_boot::ensure_process_plugin_plane(
        &rebon_harness::kernel_bootstrap::process_plugin_registry(),
    )
    .await;
    let refusal = plugin_boot::composition_refusal().map(|refusal| refusal.message());
    let loaded = plane
        .as_ref()
        .map(|plane| plane.loaded())
        .unwrap_or_default();
    let generation = plane.as_ref().map(|plane| plane.generation());

    let (entries, skipped, composition_problem) = match plugin_boot::plane_scripts() {
        Ok(scripts) => {
            let roots = CompositionRoots {
                payload: payload_root(&scripts.compose_root),
                runtime: scripts.compose_root.clone(),
            };
            // The tool list only expands the `$rebon/tools` sentinel, which
            // this listing does not show.
            match plane_composition(&config_dir, &roots, &[]) {
                Ok(composition) => (
                    composition
                        .entries
                        .iter()
                        .map(|entry| PluginEntry {
                            id: entry.id.clone(),
                            root: entry.root.clone(),
                            entry: entry.entry.clone(),
                            loaded: loaded.contains(&entry.id),
                            services: entry.services.clone(),
                            tools: entry.tools.clone(),
                            llm_providers: entry.llm_providers.clone(),
                        })
                        .collect::<Vec<_>>(),
                    composition.skipped,
                    None,
                ),
                Err(problem) => (Vec::new(), Vec::new(), Some(problem)),
            }
        }
        Err(problem) => (Vec::new(), Vec::new(), Some(problem)),
    };

    let kernel_backends = agents.kernel_backend_ids();
    let loops = rebon_plugin_host::loop_host::KNOWN_LOOP_VENDORS
        .iter()
        .map(|vendor| {
            let backend_id = rebon_plugin_host::loop_host::loop_backend_id(vendor.id);
            let config = rebon_plugin_host::kernel_loop_backend::KernelLoopConfig::for_vendor(
                &config_dir,
                vendor.id,
            );
            KernelLoopOption {
                id: vendor.id.to_string(),
                label: vendor.label.to_string(),
                bundled: vendor.bundled,
                configured: config.is_some(),
                switchable: kernel_backends.contains(&backend_id),
                provider: config.as_ref().map(|config| config.provider.clone()),
                model: config.as_ref().map(|config| config.model.clone()),
                backend_id,
            }
        })
        .collect::<Vec<_>>();

    PluginStatus {
        host: rebon_plugin_host::loop_host::describe_loop_host().to_string(),
        runtime: PluginRuntimeStatus {
            configured_plugins: runtime.configured_plugins,
            supported: runtime.supported,
            resolved: runtime.runtime.as_ref().map(|resolved| ResolvedRuntime {
                version: resolved.version.clone(),
                executable: resolved.executable.to_string_lossy().into_owned(),
                origin: resolved.origin.clone(),
            }),
            problem: runtime.problem,
            out_of_range: runtime
                .out_of_range
                .as_ref()
                .map(|out_of_range| OutOfRangeRuntime {
                    version: out_of_range.version.clone(),
                    executable: out_of_range.executable.to_string_lossy().into_owned(),
                }),
            installable: runtime.installable,
        },
        plane: PluginPlaneStatus {
            running: plane.is_some(),
            generation,
            refusal,
            loaded,
            problem: composition_problem,
        },
        entries,
        skipped,
        loops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_ids_are_file_name_safe_or_nothing() {
        assert!(valid_session_id("session_2026-08-27_abc"));
        // Both shapes a session on disk can have: the current random id
        // and the timestamped one sessions made before it still carry.
        assert!(valid_session_id(&rebon_types::new_session_id()));
        assert!(valid_session_id("sess-18d1fce237971d90-0"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../../etc/passwd"));
        assert!(!valid_session_id("a b"));
        assert!(!valid_session_id(&"x".repeat(200)));
    }

    #[test]
    fn history_refuses_a_bad_id_and_reports_a_missing_transcript() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            session_history(dir.path(), "/work", "../x").unwrap_err(),
            "not a session id"
        );
        let err = session_history(dir.path(), "/work", "missing").unwrap_err();
        assert!(err.contains("no transcript"), "{err}");
    }

    #[test]
    fn history_projects_a_transcript_the_way_a_resume_would() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = "/work";
        let path = rebon_session::session_storage::transcript_file_path(dir.path(), cwd, "s1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-08-27T00:00:00Z",
                   "message":{"role":"user","content":[
                       {"type":"text","text":"<system-reminder>\n<runtime_context>git status…</runtime_context>\n</system-reminder>"},
                       {"type":"text","text":"hi"}]}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-08-27T00:00:01Z",
                   "message":{"role":"assistant","content":[
                       {"type":"text","text":"hello"},
                       {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"x"}}]}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2026-08-27T00:00:02Z",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"contents"}]}}),
            json!({"type":"user","uuid":"u3","parentUuid":"u2","isVisibleInTranscriptOnly":true,
                   "message":{"role":"user","content":[{"type":"text","text":"transcript-only"}]}}),
        ];
        let body = rows
            .iter()
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body + "\n").unwrap();

        let history =
            serde_json::to_value(session_history(dir.path(), cwd, "s1").unwrap()).unwrap();
        assert_eq!(history["sessionId"], "s1");
        let messages = history["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3, "{history}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            messages[0]["content"].as_array().unwrap().len(),
            1,
            "injected context is stripped"
        );
        assert_eq!(messages[0]["content"][0]["text"], "hi");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert!(
            !history.to_string().contains("transcript-only"),
            "transcript-only rows stay out"
        );
    }

    /// The page and the terminal read the same rows.
    ///
    /// `/api/history` used to fold the transcript its own way — a prefix
    /// test that deleted whole user blocks, no hidden-tool gate, no
    /// tool-result folding — so the browser and the terminal disagreed about
    /// the same session. Both now go through
    /// `rebon_render::transcript_replay::replayed_rows`, and this compares
    /// the served `rows` against what the terminal's own entry point
    /// produces for the same file, byte for byte.
    #[test]
    fn served_rows_are_the_rows_the_terminal_replays() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = "/work";
        let path = rebon_session::session_storage::transcript_file_path(dir.path(), cwd, "s1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let entries = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-09-06T00:00:00Z",
                   "message":{"role":"user","content":[
                       {"type":"text","text":"<system-reminder>be brief</system-reminder>\nread x"}]}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-09-06T00:00:01Z",
                   "message":{"role":"assistant","content":[
                       {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"x"}},
                       {"type":"tool_use","id":"t2","name":"TodoWrite","input":{}}]}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2026-09-06T00:00:02Z",
                   "toolUseResults":{"t1":"contents"},
                   "message":{"role":"user","content":[
                       {"type":"tool_result","tool_use_id":"t1","content":"contents"}]}}),
        ];
        let body = entries
            .iter()
            .map(|entry| entry.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body + "\n").unwrap();

        let loaded = rebon_session::session_storage::load_transcript_from_file(&path)
            .unwrap()
            .unwrap();
        let (terminal_rows, _) =
            crate::session::transcript_replay::replayed_messages(loaded.messages);
        let served = serde_json::to_value(session_history(dir.path(), cwd, "s1").unwrap()).unwrap();

        assert_eq!(
            served["rows"],
            serde_json::to_value(&terminal_rows).unwrap(),
            "the page and the terminal must see the same rows"
        );

        let rows = served["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{served}");
        assert_eq!(rows[0]["message"]["content"][0]["text"], "read x");
        let tools = rows[1]["message"]["content"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "the hidden TodoWrite call is gone");
        assert_eq!(tools[0]["name"], "Read");
        // The output and the status the terminal shows are on the wire too.
        // Without them a page reading `rows` would draw every tool card
        // empty, which is why `messages` could not be removed before.
        assert_eq!(tools[0]["status"], "completed");
        assert_eq!(tools[0]["raw_output"], json!("contents"));

        // The other half of the answer, kept one more release for the
        // checked-in web bundle, still shows both of those.
        let messages = served["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|message| {
                message["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["name"] == "TodoWrite"))
            }),
            "`messages` is the model's view and still carries hidden tools"
        );
    }

    /// The page's menu is the `WEB` surface bit, so a command a plugin
    /// registers for the page is on it without anyone editing this file.
    ///
    /// The kernel is booted first because half the menu is registered by a
    /// plugin rather than compiled into the built-in table — `/tasks`,
    /// `/workflows`, `/agents`, `/skills` and `/memory` all live in plugins —
    /// and the catalog answers from the table until
    /// `core-commands` installs the seat's source.
    #[test]
    fn the_page_menu_is_the_web_surface_bit() {
        let seat = rebon_kernel_seats::kernel_core_commands::command_seat()
            .or_else(|| {
                rebon_harness::kernel_bootstrap::process_kernel();
                rebon_kernel_seats::kernel_core_commands::command_seat()
            })
            .expect("core-commands is a Core plugin and always loads");
        let offered = || -> Vec<String> {
            rebon_slash_commands::for_surface(Surface::Web)
                .into_iter()
                .map(|command| command.name)
                .collect()
        };
        for required in ["help", "status", "tasks", "memory"] {
            assert!(offered().contains(&required.to_string()), "/{required}");
        }
        assert!(
            !offered().contains(&"vim".to_string()),
            "a terminal-only command is not on the page's menu"
        );

        let kernel = rebon_harness::kernel_bootstrap::process_kernel();
        let plugin = kernel.context().fork("test-plugin-web-menu");
        seat.register(
            &plugin,
            rebon_slash_commands::CommandSpec::new("web-menu-fixture", "A fixture")
                .surfaces(rebon_slash_commands::Surfaces::WEB),
            rebon_kernel_seats::kernel_core_commands::CommandHandler::Explain("nothing".into()),
        )
        .expect("the name is free");
        assert!(offered().contains(&"web-menu-fixture".to_string()));
        plugin.dispose();
        assert!(!offered().contains(&"web-menu-fixture".to_string()));
    }
}
