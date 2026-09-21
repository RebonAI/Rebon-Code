use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rebon_width::WidthStr;
use serde_json::json;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::{StatusLineConfig, StatusLineKind, STATUS_LINE_MAX_PADDING};

use super::status_bar::wall_clock_ms;
use super::StatusBarInfo;
use crate::tui::app::AppState;
use rebon_permissions::{permission_mode_short_title, permission_mode_symbol, PermissionMode};

pub const STATUS_LINE_MAX_LINES: usize = rebon_hooks::STATUS_LINE_MAX_LINES;
const STATUS_LINE_MIN_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Default)]
pub struct CustomStatusLineState {
    pub config: Option<StatusLineConfig>,
    pub output: Vec<String>,
    pub sequence: u64,
    pub in_flight: bool,
    pub last_started_at: Option<Instant>,
    pub last_completed_at: Option<Instant>,
    last_result: Option<StatusLineRunResult>,
    cancellation: Option<Arc<AtomicBool>>,
}

impl CustomStatusLineState {
    pub fn configured(config: Option<StatusLineConfig>) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub(in crate::tui::runner) fn force_refresh(&mut self) {
        if let Some(cancelled) = self.cancellation.take() {
            cancelled.store(true, Ordering::Release);
        }
        self.sequence = self.sequence.saturating_add(1);
        self.in_flight = false;
        self.last_started_at = None;
        self.last_completed_at = None;
        self.last_result = None;
    }

    pub(in crate::tui::runner) fn last_result(&self) -> Option<StatusLineRunResult> {
        self.last_result
    }

    pub fn should_hide_vim_mode_indicator(&self) -> bool {
        self.config
            .as_ref()
            .is_some_and(StatusLineConfig::hide_vim_mode_indicator)
    }
}

pub struct CustomStatusLineRuntime {
    rx: UnboundedReceiver<StatusLineResult>,
    tx: UnboundedSender<StatusLineResult>,
}

#[derive(Debug)]
struct StatusLineResult {
    sequence: u64,
    result: Result<Vec<String>, StatusLineError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui::runner) enum StatusLineRunResult {
    Rendered,
    Failed(StatusLineError),
}

pub(in crate::tui::runner) type StatusLineError = rebon_hooks::StatusLineCommandErrorKind;

impl Default for CustomStatusLineRuntime {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self { rx, tx }
    }
}

impl CustomStatusLineRuntime {
    pub(in crate::tui::runner) fn drain_and_maybe_spawn(
        &mut self,
        app: &mut AppState,
        session: &TuiEngineSession,
        status: &StatusBarInfo<'_>,
        terminal_columns: u16,
        terminal_lines: u16,
    ) -> bool {
        let mut applied_result = false;
        while let Ok(result) = self.rx.try_recv() {
            if result.sequence != app.custom_status_line.sequence {
                tracing::debug!(
                    sequence = result.sequence,
                    current = app.custom_status_line.sequence,
                    "ignoring stale statusLine result"
                );
                continue;
            }
            applied_result = true;
            app.custom_status_line.in_flight = false;
            app.custom_status_line.cancellation = None;
            app.custom_status_line.last_completed_at = Some(Instant::now());
            match result.result {
                Ok(lines) => {
                    app.custom_status_line.output = lines;
                    app.custom_status_line.last_result = Some(StatusLineRunResult::Rendered);
                }
                Err(err) => {
                    app.custom_status_line.last_result = Some(StatusLineRunResult::Failed(err));
                    if app.custom_status_line.output.is_empty() {
                        tracing::debug!(?err, "statusLine command produced no renderable output");
                    } else {
                        tracing::debug!(
                            ?err,
                            "statusLine command failed; keeping previous rendered output"
                        );
                    }
                }
            }
        }

        let Some(config) = app.custom_status_line.config.clone() else {
            return applied_result;
        };
        if app.custom_status_line.in_flight {
            return applied_result;
        }
        let now = Instant::now();
        if app
            .custom_status_line
            .last_started_at
            .is_some_and(|last| now.duration_since(last) < STATUS_LINE_MIN_DEBOUNCE)
        {
            return applied_result;
        }
        if app
            .custom_status_line
            .last_completed_at
            .is_some_and(|last| {
                now.duration_since(last) < Duration::from_secs(config.refresh_interval_secs())
            })
        {
            return applied_result;
        }

        app.custom_status_line.sequence = app.custom_status_line.sequence.saturating_add(1);
        app.custom_status_line.in_flight = true;
        app.custom_status_line.last_started_at = Some(now);
        let sequence = app.custom_status_line.sequence;
        let payload = build_status_line_payload(app, session, status);
        let tx = self.tx.clone();
        let cwd = session.cwd.clone();
        let cancellation = Arc::new(AtomicBool::new(false));
        app.custom_status_line.cancellation = Some(Arc::clone(&cancellation));
        let command_columns =
            terminal_columns.saturating_sub(status_line_reserved_prefix_width(app));
        tokio::spawn(async move {
            let result = run_status_line(
                &config,
                payload,
                &cwd,
                command_columns,
                terminal_lines,
                cancellation,
            )
            .await;
            let _ = tx.send(StatusLineResult { sequence, result });
        });
        applied_result
    }
}

fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Plan => "Plan mode",
        PermissionMode::BypassPermissions => "Bypass permissions on",
        PermissionMode::AcceptEdits => "Accept edits",
        PermissionMode::DontAsk => "Don't ask",
        PermissionMode::Auto => "Auto",
        PermissionMode::Default | PermissionMode::Bubble => permission_mode_short_title(mode),
    }
}

pub(in crate::tui::runner) fn status_line_permission_prefix(app: &AppState) -> Option<String> {
    if rebon_permissions::is_default_mode(Some(app.permission_mode)) {
        return None;
    }
    let symbol = permission_mode_symbol(app.permission_mode);
    Some(format!(
        " {symbol} {} ",
        permission_mode_label(app.permission_mode)
    ))
}

pub(in crate::tui::runner) fn status_line_background_tasks_prefix(
    app: &AppState,
) -> Option<String> {
    rebon_plugin_tasks::ui::tasks_view::background_tasks_footer_label(&app.task_snapshots())
        .map(|label| format!(" {label} "))
}

pub(in crate::tui::runner) fn status_line_reserved_prefix_width(app: &AppState) -> u16 {
    status_line_permission_prefix(app)
        .into_iter()
        .chain(status_line_background_tasks_prefix(app))
        .map(|prefix| WidthStr::width(prefix.as_str()) as u16)
        .sum()
}

fn cwd_basename(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.replace('\\', "/"))
}

fn shorten_cwd_for_status_line(cwd: &str) -> String {
    let path = std::path::Path::new(cwd);
    let components: Vec<_> = path.components().collect();
    if components.len() <= 2 {
        return cwd.replace('\\', "/");
    }
    components[components.len() - 2..]
        .iter()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn status_line_usage_json(usage: rebon_types::Usage) -> serde_json::Value {
    let cache_read_hit_tokens = usage
        .cache_read_input_tokens
        .saturating_add(usage.prompt_cache_hit_tokens);
    let cache_write_miss_tokens = usage
        .cache_creation_input_tokens
        .saturating_add(usage.prompt_cache_miss_tokens);
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_input_tokens": usage.cache_read_input_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        "cache_read_hit_input_tokens": cache_read_hit_tokens,
        "cache_write_miss_input_tokens": cache_write_miss_tokens,
        "cache_total_input_tokens": cache_read_hit_tokens.saturating_add(cache_write_miss_tokens),
        "prompt_cache_hit_tokens": usage.prompt_cache_hit_tokens,
        "prompt_cache_miss_tokens": usage.prompt_cache_miss_tokens,
        "total_input_tokens": usage.total_input_tokens,
        "total_output_tokens": usage.total_output_tokens,
    })
}

pub(in crate::tui::runner) fn build_status_line_payload(
    app: &AppState,
    session: &TuiEngineSession,
    status: &StatusBarInfo<'_>,
) -> serde_json::Value {
    let mut root = serde_json::Map::new();
    let mut base_hook_input = BTreeMap::new();
    base_hook_input.insert("hook_event_name", "Status");
    for (key, value) in base_hook_input {
        root.insert(key.into(), json!(value));
    }
    root.insert("cwd".into(), json!(session.cwd));
    root.insert("session_id".into(), json!(session.session_id));
    root.insert(
        "permission".into(),
        json!({
            "mode": format!("{:?}", app.permission_mode).to_ascii_lowercase(),
            "label": permission_mode_label(app.permission_mode),
            "symbol": permission_mode_symbol(app.permission_mode),
            "is_default": rebon_permissions::is_default_mode(Some(app.permission_mode)),
        }),
    );
    if let Some(name) = app.session_title.as_deref().filter(|s| !s.is_empty()) {
        root.insert("session_name".into(), json!(name));
    }
    // The owner's answer, not this terminal's.
    //
    // A mirrored session runs in somebody else's process, so the model this
    // one resolved at start-up is a guess about a session it does not run —
    // and a status line that states a guess as fact is the failure mode the
    // whole shared-state rule exists to stop. It read `gpt-5.6-sol` for a
    // worker that had been switched to `gpt-5.6-luna`, and nothing said so.
    //
    // Falls back to the local values only when there is no owner (a `--local`
    // session, where this process *is* the owner) or before the owner's first
    // snapshot lands, which is a brief unknown rather than a lasting lie.
    let owner_model = session
        .remote_background_attachment
        .as_ref()
        .and_then(|remote| remote.owner_model());
    root.insert(
        "model".into(),
        json!({
            "id": owner_model.unwrap_or(session.model.name.as_str()),
            "display_name": owner_model.unwrap_or(status.model),
        }),
    );
    root.insert(
        "workspace".into(),
        json!({
            "current_dir": session.cwd,
            "current_dir_basename": cwd_basename(&session.cwd),
            "current_dir_short": shorten_cwd_for_status_line(&session.cwd),
            "project_dir": session.cwd,
            "project_dir_basename": cwd_basename(&session.cwd),
            "project_dir_short": shorten_cwd_for_status_line(&session.cwd),
            "added_dirs": session.startup.add_dirs,
        }),
    );
    root.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    root.insert("output_style".into(), json!({ "name": "default" }));
    // One reading for the whole payload: two locks would let the totals
    // and the last turn come from different moments in one line.
    let usage = app.usage();
    let total_duration_ms = wall_clock_ms().saturating_sub(usage.started_at_ms);
    root.insert(
        "cost".into(),
        json!({
            "total_cost_usd": 0.0,
            "total_duration_ms": total_duration_ms,
            "total_api_duration_ms": 0,
            "total_lines_added": 0,
            "total_lines_removed": 0,
        }),
    );
    let usage_snapshot = session.model.prune_level.budget.usage_snapshot();
    let current_input_tokens = usage_snapshot.tokens;
    let context_window_size = session.model.prune_level.budget.context_window();
    let used_percentage = if context_window_size > 0 {
        (current_input_tokens as f64 / context_window_size as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let last_turn_usage = status_line_usage_json(usage.last_turn);
    let total_usage = status_line_usage_json(usage.total);
    root.insert("last_turn_usage".into(), last_turn_usage.clone());
    root.insert("total_usage".into(), total_usage.clone());
    let current_usage = match usage_snapshot.source {
        rebon_api::ContextUsageSource::Unknown => serde_json::Value::Null,
        rebon_api::ContextUsageSource::Server | rebon_api::ContextUsageSource::Estimated => json!({
            "input_tokens": current_input_tokens,
            "output_tokens": usage.last_turn.output_tokens,
            "cache_creation_input_tokens": usage.last_turn.cache_creation_input_tokens,
            "cache_read_input_tokens": usage.last_turn.cache_read_input_tokens,
            "cache_read_hit_input_tokens": usage
                .last_turn
                .cache_read_input_tokens
                .saturating_add(usage.last_turn.prompt_cache_hit_tokens),
            "cache_write_miss_input_tokens": usage
                .last_turn
                .cache_creation_input_tokens
                .saturating_add(usage.last_turn.prompt_cache_miss_tokens),
            "cache_total_input_tokens": usage
                .last_turn
                .cache_read_input_tokens
                .saturating_add(usage.last_turn.prompt_cache_hit_tokens)
                .saturating_add(usage.last_turn.cache_creation_input_tokens)
                .saturating_add(usage.last_turn.prompt_cache_miss_tokens),
            "prompt_cache_hit_tokens": usage.last_turn.prompt_cache_hit_tokens,
            "prompt_cache_miss_tokens": usage.last_turn.prompt_cache_miss_tokens,
        }),
    };
    root.insert(
        "context_window".into(),
        json!({
            "total_input_tokens": current_input_tokens,
            "total_output_tokens": usage.last_turn.output_tokens,
            "context_window_size": context_window_size,
            "used_percentage": used_percentage,
            "remaining_percentage": (100.0 - used_percentage).max(0.0),
            "current_usage": current_usage,
            "last_turn_usage": last_turn_usage,
            "total_usage": total_usage,
        }),
    );
    root.insert(
        "exceeds_200k_tokens".into(),
        json!(current_input_tokens > 200_000),
    );
    if let Some(level) = app.effort_level {
        root.insert("effort".into(), json!({ "level": level.as_str() }));
    }
    root.insert(
        "thinking".into(),
        json!({ "enabled": app.effort_level.is_some() }),
    );
    if !app.custom_status_line.should_hide_vim_mode_indicator() {
        if let Some(mode) = app.vim_mode {
            root.insert(
                "vim".into(),
                json!({ "mode": format!("{mode:?}").to_lowercase() }),
            );
        }
    }
    serde_json::Value::Object(root)
}

async fn run_status_line(
    config: &StatusLineConfig,
    payload: serde_json::Value,
    cwd: &str,
    columns: u16,
    lines: u16,
    cancelled: Arc<AtomicBool>,
) -> Result<Vec<String>, StatusLineError> {
    match config.kind {
        StatusLineKind::Command => {
            run_status_line_command(&config.command, payload, cwd, columns, lines).await
        }
        StatusLineKind::Script => {
            let Some(script) = config.script.clone() else {
                return Err(StatusLineError::Spawn);
            };
            let cwd = std::path::PathBuf::from(cwd);
            let script_label = script.clone();
            let result = tokio::task::spawn_blocking(move || {
                let runner = rebon_boa_runner::StatusLineScriptRunner::from_current_executable()?;
                runner.run_file(
                    &script,
                    payload,
                    &cwd,
                    columns,
                    lines,
                    Some(cancelled.as_ref()),
                )
            })
            .await
            .map_err(|error| {
                tracing::debug!(%error, "statusLine script worker failed");
                StatusLineError::Io
            })?;
            match result {
                Ok(output) => {
                    for log in output.logs {
                        tracing::debug!(script = %script_label.display(), log = %log, "statusLine script console");
                    }
                    Ok(output.lines)
                }
                Err(error) => {
                    let kind = status_script_error_kind(&error);
                    tracing::debug!(?kind, %error, script = %script_label.display(), "statusLine script failed");
                    Err(kind)
                }
            }
        }
    }
}

fn status_script_error_kind(error: &rebon_boa_runner::StatusLineScriptError) -> StatusLineError {
    use rebon_boa_runner::{RunnerError, StatusLineScriptError};
    match error {
        StatusLineScriptError::Empty => StatusLineError::Empty,
        StatusLineScriptError::Runner(
            RunnerError::Timeout { .. }
            | RunnerError::Cancelled { .. }
            | RunnerError::CancelledBeforeStart,
        ) => StatusLineError::Timeout,
        StatusLineScriptError::HelperNotFound { .. }
        | StatusLineScriptError::CurrentExecutable(_)
        | StatusLineScriptError::ScriptIo { .. } => StatusLineError::Spawn,
        StatusLineScriptError::Runner(
            RunnerError::Io(_)
            | RunnerError::InputTooLarge { .. }
            | RunnerError::OutputTooLarge { .. }
            | RunnerError::Protocol(_)
            | RunnerError::ChildExit { .. }
            | RunnerError::InvalidLimits(_)
            | RunnerError::ContainmentUnsupported(_)
            | RunnerError::ContainmentSetup(_)
            | RunnerError::CleanupIntegrity { .. },
        )
        | StatusLineScriptError::SourceTooLarge { .. } => StatusLineError::Io,
        StatusLineScriptError::InvalidModule(_)
        | StatusLineScriptError::NonStringResult
        | StatusLineScriptError::Runner(RunnerError::Script { .. }) => StatusLineError::NonZero,
    }
}

async fn run_status_line_command(
    command: &str,
    payload: serde_json::Value,
    cwd: &str,
    columns: u16,
    lines: u16,
) -> Result<Vec<String>, StatusLineError> {
    run_status_line_command_with_budget(
        command,
        payload,
        cwd,
        columns,
        lines,
        rebon_hooks::STATUS_LINE_TIMEOUT,
    )
    .await
}

async fn run_status_line_command_with_budget(
    command: &str,
    payload: serde_json::Value,
    cwd: &str,
    columns: u16,
    lines: u16,
    budget: std::time::Duration,
) -> Result<Vec<String>, StatusLineError> {
    match rebon_hooks::run_status_line_command_with_timeout(
        command,
        payload,
        std::path::Path::new(cwd),
        columns,
        lines,
        budget,
    )
    .await
    {
        Ok(lines) => Ok(lines),
        Err(failure) => {
            tracing::debug!(
                error = ?failure.kind,
                status = ?failure.exit_code,
                stderr = %failure.stderr,
                "statusLine command failed"
            );
            Err(failure.kind)
        }
    }
}

pub fn bounded_status_line_padding(padding: u32) -> u32 {
    padding.min(STATUS_LINE_MAX_PADDING)
}

pub fn padded_status_line_lines(lines: &[String], padding: u32) -> Vec<String> {
    let pad = " ".repeat(bounded_status_line_padding(padding) as usize);
    lines
        .iter()
        .take(STATUS_LINE_MAX_LINES)
        .map(|line| format!("{pad}{line}{pad}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> String {
        std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    async fn status_line_shell_available() -> bool {
        !matches!(
            run_status_line_command("exit 0", json!({}), &cwd(), 80, 24).await,
            Err(StatusLineError::Spawn)
        )
    }

    fn test_status_bar_info<'a>() -> StatusBarInfo<'a> {
        StatusBarInfo {
            provider: "test",
            model: "Test Model",
            cwd: ".",
            elapsed_ms: 7,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        }
    }

    /// A mirrored terminal runs none of the turns, so the model it resolved at
    /// start-up says nothing about the session on screen. The owner's answer
    /// wins; before the owner has given one, the honest report is that this
    /// terminal does not know yet, not the local guess dressed up as fact.
    #[test]
    fn a_mirrored_status_line_reports_the_owners_model_not_its_own() {
        let app = AppState::new();
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.model.name = "gpt-5.6-sol".into();

        // Local session: this process is the owner, so its own answer is right.
        let local = build_status_line_payload(&app, &session, &test_status_bar_info());
        assert_eq!(local["model"]["id"], json!("gpt-5.6-sol"));
        assert_eq!(local["model"]["display_name"], json!("Test Model"));

        // Mirrored, owner has not spoken: still the local value, because there
        // is nothing better yet — but the moment it speaks, it wins.
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "job-1".into(),
                "sess-1".into(),
                ".".into(),
                rebon_session_host::BackgroundJobStatus::Idle,
                0,
                rebon_session_host::BackgroundIpcEndpoint {
                    pid: std::process::id(),
                    port: 0,
                    token: "test-token".into(),
                },
            ));
        let silent = build_status_line_payload(&app, &session, &test_status_bar_info());
        assert_eq!(silent["model"]["id"], json!("gpt-5.6-sol"));

        // Built from the wire shape on purpose: an owner that omits everything
        // optional is exactly what an older worker sends, and the model has to
        // survive that trip to be worth reading.
        let snapshot: rebon_session_host::SessionStatusSnapshot = serde_json::from_value(json!({
            "jobId": "job-1",
            "cwd": ".",
            "status": "idle",
            "busy": false,
            "turnGeneration": 0,
            "updatedAtMs": 0,
            "model": "gpt-5.6-luna",
        }))
        .expect("the owner's snapshot parses from its own wire form");
        session
            .remote_background_attachment
            .as_mut()
            .expect("attached")
            .owner = Some(snapshot);

        let mirrored = build_status_line_payload(&app, &session, &test_status_bar_info());
        assert_eq!(
            mirrored["model"]["id"],
            json!("gpt-5.6-luna"),
            "the owner runs the turns, so the owner names the model"
        );
        assert_eq!(
            mirrored["model"]["display_name"],
            json!("gpt-5.6-luna"),
            "a true name beats a prettier one that is wrong"
        );
    }

    #[test]
    fn payload_context_window_uses_live_context_and_session_duration() {
        let mut app = AppState::new();
        app.usage_ledger = std::sync::Arc::new(std::sync::Mutex::new(
            crate::session::usage::UsageLedger::seeded(
                wall_clock_ms().saturating_sub(1_500),
                rebon_types::Usage {
                    input_tokens: 999_999,
                    output_tokens: 888_888,
                    cache_creation_input_tokens: 44,
                    cache_read_input_tokens: 55,
                    prompt_cache_hit_tokens: 66,
                    prompt_cache_miss_tokens: 77,
                    total_input_tokens: 123_456,
                    total_output_tokens: 654,
                    ..Default::default()
                },
                rebon_types::Usage {
                    output_tokens: 321,
                    cache_creation_input_tokens: 22,
                    cache_read_input_tokens: 33,
                    prompt_cache_hit_tokens: 11,
                    prompt_cache_miss_tokens: 12,
                    ..Default::default()
                },
            ),
        ));

        let session = crate::tui::runner::test_support::make_test_tui_session();
        session.model.prune_level.budget.set_context_window(50_000);
        session.model.prune_level.budget.report_usage(10_000);

        let payload = build_status_line_payload(&app, &session, &test_status_bar_info());
        assert_eq!(payload["hook_event_name"], json!("Status"));
        assert!(payload["cost"]["total_duration_ms"].as_u64().unwrap() >= 1_500);

        let context = &payload["context_window"];
        assert_eq!(
            context["context_window_size"],
            json!(session.model.prune_level.budget.context_window())
        );
        assert_eq!(context["total_input_tokens"], json!(10_000));
        assert_eq!(context["total_output_tokens"], json!(321));
        assert_eq!(context["used_percentage"].as_f64().unwrap(), 20.0);
        assert_eq!(context["remaining_percentage"].as_f64().unwrap(), 80.0);
        assert_eq!(
            context["current_usage"],
            json!({
                "input_tokens": 10_000,
                "output_tokens": 321,
                "cache_creation_input_tokens": 22,
                "cache_read_input_tokens": 33,
                "cache_read_hit_input_tokens": 44,
                "cache_write_miss_input_tokens": 34,
                "cache_total_input_tokens": 78,
                "prompt_cache_hit_tokens": 11,
                "prompt_cache_miss_tokens": 12,
            })
        );
        let expected_total_usage = json!({
            "input_tokens": 999_999,
            "output_tokens": 888_888,
            "cache_read_input_tokens": 55,
            "cache_creation_input_tokens": 44,
            "cache_read_hit_input_tokens": 121,
            "cache_write_miss_input_tokens": 121,
            "cache_total_input_tokens": 242,
            "prompt_cache_hit_tokens": 66,
            "prompt_cache_miss_tokens": 77,
            "total_input_tokens": 123_456,
            "total_output_tokens": 654,
        });
        assert_eq!(payload["total_usage"], expected_total_usage);
        assert_eq!(context["total_usage"], expected_total_usage);
    }

    #[test]
    fn payload_current_usage_is_null_before_usage_is_known() {
        let app = AppState::new();
        let session = crate::tui::runner::test_support::make_test_tui_session();

        let payload = build_status_line_payload(&app, &session, &test_status_bar_info());

        assert!(payload["context_window"]["current_usage"].is_null());
    }

    #[test]
    fn failed_status_line_result_preserves_previous_output() {
        let mut app = AppState::new();
        app.custom_status_line.output = vec!["previous".into()];
        app.custom_status_line.sequence = 7;
        app.custom_status_line.in_flight = true;

        let mut runtime = CustomStatusLineRuntime::default();
        runtime
            .tx
            .send(StatusLineResult {
                sequence: 7,
                result: Err(StatusLineError::Timeout),
            })
            .unwrap();
        let session = crate::tui::runner::test_support::make_test_tui_session();

        assert!(runtime.drain_and_maybe_spawn(&mut app, &session, &test_status_bar_info(), 80, 24));
        assert_eq!(app.custom_status_line.output, vec!["previous"]);
        assert_eq!(
            app.custom_status_line.last_result(),
            Some(StatusLineRunResult::Failed(StatusLineError::Timeout))
        );
        assert!(!app.custom_status_line.in_flight);
    }

    #[test]
    fn stale_status_line_result_preserves_current_state() {
        let mut app = AppState::new();
        app.custom_status_line.output = vec!["current".into()];
        app.custom_status_line.sequence = 8;
        app.custom_status_line.in_flight = true;

        let mut runtime = CustomStatusLineRuntime::default();
        runtime
            .tx
            .send(StatusLineResult {
                sequence: 7,
                result: Ok(vec!["stale".into()]),
            })
            .unwrap();
        let session = crate::tui::runner::test_support::make_test_tui_session();

        assert!(!runtime.drain_and_maybe_spawn(
            &mut app,
            &session,
            &test_status_bar_info(),
            80,
            24
        ));
        assert_eq!(app.custom_status_line.output, vec!["current"]);
        assert_eq!(app.custom_status_line.sequence, 8);
        assert!(app.custom_status_line.in_flight);
    }

    #[test]
    fn force_refresh_cancels_the_previous_script_run() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut state = CustomStatusLineState {
            sequence: 4,
            in_flight: true,
            cancellation: Some(Arc::clone(&cancelled)),
            ..CustomStatusLineState::default()
        };

        state.force_refresh();

        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(state.sequence, 5);
        assert!(!state.in_flight);
        assert!(state.cancellation.is_none());
    }

    #[test]
    fn padding_preserves_multiline_allocation() {
        let lines = vec![String::from("one"), String::from("two")];
        assert_eq!(
            padded_status_line_lines(&lines, 2),
            vec!["  one  ", "  two  "]
        );
    }

    #[test]
    fn padding_is_bounded() {
        let lines = vec![String::from("x")];
        let rendered = padded_status_line_lines(&lines, STATUS_LINE_MAX_PADDING + 1);
        assert_eq!(
            rendered[0].len(),
            (STATUS_LINE_MAX_PADDING as usize * 2) + 1
        );
    }

    #[cfg(not(windows))]
    fn stdin_env_command() -> &'static str {
        "read input; printf '%s|%s:%s' \"$input\" \"$COLUMNS\" \"$LINES\""
    }

    #[cfg(windows)]
    fn stdin_env_command() -> &'static str {
        "$inputText = [Console]::In.ReadToEnd(); [Console]::Out.Write($inputText + '|' + $env:COLUMNS + ':' + $env:LINES)"
    }

    #[cfg(not(windows))]
    fn stderr_ok_command() -> &'static str {
        "echo err >&2; printf ok"
    }

    #[cfg(windows)]
    fn stderr_ok_command() -> &'static str {
        "[Console]::Error.WriteLine('err'); [Console]::Out.Write('ok')"
    }

    #[cfg(not(windows))]
    fn nonzero_command() -> &'static str {
        "echo nope; exit 7"
    }

    #[cfg(windows)]
    fn nonzero_command() -> &'static str {
        "Write-Output nope; exit 7"
    }

    #[cfg(not(windows))]
    fn empty_command() -> &'static str {
        "printf '  \\n'"
    }

    #[cfg(windows)]
    fn empty_command() -> &'static str {
        "Write-Output '  '"
    }

    #[cfg(not(windows))]
    fn timeout_command() -> &'static str {
        "sleep 2; printf late"
    }

    #[cfg(windows)]
    fn timeout_command() -> &'static str {
        "Start-Sleep -Seconds 2; Write-Output -NoNewline late"
    }

    #[cfg(not(windows))]
    fn ansi_multiline_command() -> &'static str {
        "printf '\\033[31mred\\033[0m\\nsecond\\nthird\\nfourth\\nfifth\\nsixth'"
    }

    #[cfg(windows)]
    fn ansi_multiline_command() -> &'static str {
        "$esc = [char]27; [Console]::Out.Write($esc + '[31mred' + $esc + '[0m' + \"`nsecond`nthird`nfourth`nfifth`nsixth\")"
    }

    /// Budget for tests that check what a command's output turns into.
    /// Generous on purpose: on a loaded machine simply starting a shell can
    /// outlast the production budget, and these tests are not about that.
    /// `command_timeout_hides_output` keeps the real budget — it is the one
    /// testing the timeout.
    const TEST_OUTPUT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

    #[tokio::test]
    async fn command_receives_json_stdin_and_env() {
        if !status_line_shell_available().await {
            return;
        }

        let output = run_status_line_command_with_budget(
            stdin_env_command(),
            json!({ "hook_event_name": "Status", "value": 1 }),
            &cwd(),
            123,
            45,
            TEST_OUTPUT_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("\"hook_event_name\":\"Status\""));
        assert!(output[0].ends_with("|123:45"));
    }

    #[tokio::test]
    async fn command_stderr_is_ignored_when_stdout_is_valid() {
        let output = run_status_line_command_with_budget(
            stderr_ok_command(),
            json!({}),
            &cwd(),
            80,
            24,
            TEST_OUTPUT_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(output, vec!["ok"]);
    }

    #[tokio::test]
    async fn command_nonzero_and_empty_hide_output() {
        if !status_line_shell_available().await {
            return;
        }

        let nonzero = run_status_line_command_with_budget(
            nonzero_command(),
            json!({}),
            &cwd(),
            80,
            24,
            TEST_OUTPUT_BUDGET,
        )
        .await;
        assert_eq!(nonzero, Err(StatusLineError::NonZero));

        let empty = run_status_line_command_with_budget(
            empty_command(),
            json!({}),
            &cwd(),
            80,
            24,
            TEST_OUTPUT_BUDGET,
        )
        .await;
        assert_eq!(empty, Err(StatusLineError::Empty));
    }

    #[tokio::test]
    async fn command_timeout_hides_output() {
        if !status_line_shell_available().await {
            return;
        }

        let timed_out = run_status_line_command(timeout_command(), json!({}), &cwd(), 80, 24).await;
        assert_eq!(timed_out, Err(StatusLineError::Timeout));
    }

    #[tokio::test]
    async fn command_output_is_capped_and_ansi_preserved() {
        let output = run_status_line_command_with_budget(
            ansi_multiline_command(),
            json!({}),
            &cwd(),
            80,
            24,
            TEST_OUTPUT_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(
            output,
            vec![
                "\u{1b}[31mred\u{1b}[0m",
                "second",
                "third",
                "fourth",
                "fifth"
            ]
        );
    }
}
