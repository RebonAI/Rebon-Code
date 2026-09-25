use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context as _;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use rebon_tui::input::needs_mode_sync;
use rebon_tui::promptinput::paste_flow::normalize_pasted_text;
use rebon_tui::promptinput::TaskListStatus;
use rebon_tui::promptinput::{clamp_cursor_offset, derive_prompt_input_runtime_state};
use rebon_tui::RenderTheme;
use tokio::runtime::Handle;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::file_scanner::{FileListRx, FileListUpdate};
#[cfg(test)]
use crate::session::mcp::TuiMcpLoadEvent;
use crate::session::mcp::TuiMcpLoadStatus;
use crate::tui::app::AppState;
use crate::tui::event::{translate_key, KeyAction};
use crate::tui::permission_modal::{PendingPermission, PermissionKind, PermissionModalView};
use crate::tui::terminal::{
    terminal_title_for_request_state, AltScreenOverlayGuard, TerminalGuard,
};
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;

use super::agent_view::{
    open_agent_view, refresh_agent_view_if_due, should_detach_attached_session_on_ctrl_z,
};
use super::commands::register_registered_skill_slash_commands;
use super::context_prompts::drain_mcp_channel_notifications;
use super::dialog_keys::{
    maybe_handle_active_dialog_key, maybe_handle_agent_view_key, maybe_handle_agent_view_mouse,
    maybe_handle_background_tasks_dialog_key, maybe_handle_dialog_shortcut,
    maybe_handle_resume_dialog_mouse, maybe_handle_teams_dialog_key, refresh_agents_dialog,
    refresh_background_tasks_dialog, refresh_live_agent_tool_activity, refresh_teams_dialog,
    PendingWork,
};
use super::footer_navigation::derive_footer_items;
use super::global_search::{sync_global_search_dialog, GlobalSearchWorker};
use super::inline_banner::{
    emit_inline_page_banner, emit_inline_startup_banner_from, inline_banner_effort_display,
    prepare_inline_viewport_for_page_banner, prepare_inline_viewport_for_startup_banner,
    startup_banner_for_slot, PageBanner,
};
use super::inline_commit_cursor::{
    flush_inline_commits, inline_resize_repaint_budget, measure_inline_commit_batches_for_prefix,
    InlineRuntimeState,
};
use super::key_actions::{handle_key_action, handle_key_action_without_session};
use super::layout_and_scroll::{
    handle_mouse_event, handle_scroll_action, mouse_prompt_area, mouse_transcript_area,
    permission_suffix_scroll_action, reset_transcript_page_state,
    should_double_down_repin_transcript, should_non_empty_prompt_down_press_repin_transcript,
    should_reroute_prompt_down_to_scroll, transcript_area,
};
use super::live_agent_view::sync_foreground_agent_view;
use super::mid_turn_submit_queue::reconcile_mid_turn_consumed_queued_submits;
use super::paste_burst::{
    paste_candidate_prefix_at_cursor, plain_char_from_edit, retro_grab_at_cursor, CharOutcome,
    EnterOutcome, PasteBurst, BURST_BATCH_FAST_COUNT,
};
use super::paste_burst_pipeline::{
    coalesce_adjacent_pastes, detect_paste_batch, detect_paste_batch_after_enter,
    detect_paste_batch_after_mode_shortcut, drain_burst_queue, drain_paste_echo_keys,
    flush_burst_with_merge, poll_crossterm_event, queued_paste_char, read_crossterm_event,
    stash_event_front, summarize_paste_payload, summarize_prompt_tail, CrosstermSource,
};
use super::paste_echo::{
    clipboard_image_path_matches_prefix, finalize_paste_echo, find_adopted_text_repair_target,
    is_main_prompt_paste_target, paste_echo_enabled, paste_echo_key_hook, snapshot_text_chips,
    try_adopt_clipboard_for_first_chunk, ClipboardAdoption, EchoConsume, KeyEchoHook,
    PasteEchoSuppressor, MIN_ADOPTION_PREFIX_BYTES,
};
use super::permission_flow::{maybe_handle_permission_key, sync_permission_view};
use super::prompt_lifecycle::{
    admit_idle_context_prompt, drain_ui_channels, maybe_spawn_task_notification_prompt,
    maybe_update_loading_state,
};
use super::remote_background_attachment::refresh_remote_background_attachment;
use super::render::{self, drain_file_list, render_frame};
use super::resume_runtime::{sync_resume_dialog, ResumeRuntime};
use super::session_detach_attach::detach_current_session_to_agent_view;
use super::slash_commands::selected_slash_command_needs_input;
use super::status_bar::{
    active_agent_footer_status, footer_new_session_hint, has_active_background_agent,
    has_active_background_shell, wall_clock_ms, StatusBarInfo,
};
use super::submit::submit_or_queue;
use super::task_runtime::{drain_agent_generation, drain_ultraplan_reviewer_verdicts};
use super::team_messages::drain_team_mailbox;
use super::updater_ui::drain_update_check;
use super::{ActivePrompt, SessionSlot};

pub(in crate::tui::runner) fn drain_mcp_load_result(
    app: &mut AppState,
    session: &mut TuiEngineSession,
) {
    // A process that does not host the servers has no load to collect.
    let Some(mcp) = session.engine_half.mcp.as_mut() else {
        return;
    };
    let Some(status) = mcp.drain_load_events() else {
        return;
    };
    match status {
        TuiMcpLoadStatus::Ready { warnings } => {
            if warnings.is_empty() {
                app.clear_mcp_load_hint();
            } else {
                let text = warnings
                    .iter()
                    .find_map(|warning| rust_lsp_load_hint(warning))
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if warnings.len() == 1 {
                            "MCP warning: run /mcp for details".to_string()
                        } else {
                            format!("{} MCP warnings: run /mcp for details", warnings.len())
                        }
                    });
                app.set_mcp_load_hint(text);
            }
        }
        TuiMcpLoadStatus::NotConfigured => {
            app.clear_mcp_load_hint();
        }
        TuiMcpLoadStatus::Failed { error } => {
            let hint = rust_lsp_load_hint(error).unwrap_or("MCP unavailable: run /mcp for details");
            app.set_mcp_load_hint(hint);
        }
        // Nothing arrived that leaves a load in flight.
        TuiMcpLoadStatus::Loading => {}
    }
}

fn rust_lsp_load_hint(details: &str) -> Option<&'static str> {
    if !details.contains("MCP server `rust_lsp`") {
        return None;
    }
    if details.contains("rust-analyzer is unavailable") {
        return Some(
            "Rust LSP unavailable: run rustup component add rust-analyzer · /mcp for details",
        );
    }
    Some("Rust LSP unavailable: run /mcp for details")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventLoopOutcome {
    Exit,
    InlineFullscreenRequested,
    InlineFullscreenClosed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InlineBlockedOverlayObservation {
    Text {
        len: usize,
    },
    Thinking {
        is_streaming: bool,
    },
    ToolUse {
        call_id: String,
        status: rebon_types::ToolCallStatus,
    },
}

impl InlineBlockedOverlayObservation {
    fn capture(block: &rebon_tui::StreamingContentBlock) -> Self {
        match block {
            rebon_tui::StreamingContentBlock::Text(text) => Self::Text { len: text.len() },
            rebon_tui::StreamingContentBlock::Thinking(thinking) => Self::Thinking {
                is_streaming: thinking.is_streaming,
            },
            rebon_tui::StreamingContentBlock::ToolUse(tool) => Self::ToolUse {
                call_id: tool.call_id.clone(),
                status: tool.status,
            },
        }
    }

    fn matches(&self, block: Option<&rebon_tui::StreamingContentBlock>) -> bool {
        match (self, block) {
            (Self::Text { len }, Some(rebon_tui::StreamingContentBlock::Text(text))) => {
                *len == text.len()
            }
            (
                Self::Thinking { is_streaming },
                Some(rebon_tui::StreamingContentBlock::Thinking(thinking)),
            ) => *is_streaming == thinking.is_streaming,
            (
                Self::ToolUse { call_id, status },
                Some(rebon_tui::StreamingContentBlock::ToolUse(tool)),
            ) => call_id == &tool.call_id && *status == tool.status,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InlineAskUserInteractionObservation {
    active_question: usize,
    confirmation_active: bool,
    confirmation_selected: usize,
    highlighted_row: usize,
    selected_count: usize,
    selected_signature: u64,
    other_text_len: usize,
    other_cursor_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InlinePermissionInteractionObservation {
    query_id: u64,
    selected: usize,
    extra_text_len: usize,
    extra_text_focused: bool,
    ask_user: Option<InlineAskUserInteractionObservation>,
}

impl InlinePermissionInteractionObservation {
    fn capture(view: &PermissionModalView) -> Self {
        let ask_user = match &view.kind {
            PermissionKind::AskUserQuestion {
                answers,
                active_question,
                confirmation_active,
                confirmation_selected,
                ..
            } => {
                let answer = answers.get(*active_question);
                let selected_signature = answer
                    .into_iter()
                    .flat_map(|answer| answer.selected_options.iter().copied())
                    .fold(0u64, |signature, selected| {
                        signature
                            .wrapping_mul(1_099_511_628_211)
                            .wrapping_add(selected as u64 + 1)
                    });
                Some(InlineAskUserInteractionObservation {
                    active_question: *active_question,
                    confirmation_active: *confirmation_active,
                    confirmation_selected: *confirmation_selected,
                    highlighted_row: answer.map(|answer| answer.highlighted_row).unwrap_or(0),
                    selected_count: answer
                        .map(|answer| answer.selected_options.len())
                        .unwrap_or(0),
                    selected_signature,
                    other_text_len: answer.map(|answer| answer.other_text.len()).unwrap_or(0),
                    other_cursor_offset: answer
                        .map(|answer| answer.other_cursor_offset)
                        .unwrap_or(0),
                })
            }
            _ => None,
        };
        Self {
            query_id: view.query_id,
            selected: view.selected,
            extra_text_len: view.extra_text.len(),
            extra_text_focused: view.extra_text_focused,
            ask_user,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InlineForceDrainStallObservation {
    first_overlay_block: InlineBlockedOverlayObservation,
    permission: Option<InlinePermissionInteractionObservation>,
}

impl InlineForceDrainStallObservation {
    fn capture(app: &AppState) -> Option<Self> {
        Some(Self {
            first_overlay_block: InlineBlockedOverlayObservation::capture(
                app.rebon_tui.overlay.blocks.first()?,
            ),
            permission: app
                .pending_permission_view
                .as_ref()
                .map(InlinePermissionInteractionObservation::capture),
        })
    }

    fn matches(&self, app: &AppState) -> bool {
        self.first_overlay_block
            .matches(app.rebon_tui.overlay.blocks.first())
            && self.permission
                == app
                    .pending_permission_view
                    .as_ref()
                    .map(InlinePermissionInteractionObservation::capture)
    }
}

/// Suppresses a repeated overflow escape-hatch call after it has proven that
/// the first live overlay block cannot drain. The event loop still measures and
/// redraws every frame; it only skips the reducer walk until the blocking block
/// or permission interaction state changes, either of which can make a fresh
/// attempt useful.
#[derive(Debug, Default)]
struct InlineForceDrainStall {
    observation: Option<InlineForceDrainStallObservation>,
}

impl InlineForceDrainStall {
    fn should_attempt(&self, app: &AppState) -> bool {
        !self
            .observation
            .as_ref()
            .is_some_and(|observation| observation.matches(app))
    }

    fn record(&mut self, app: &AppState) {
        self.observation = InlineForceDrainStallObservation::capture(app);
    }

    fn clear(&mut self) {
        self.observation = None;
    }
}

fn should_exit_completed_onboarding_to_inline(
    exit_after_onboarding_for_inline_target: bool,
    selected_ui_mode: UiMode,
    onboarding_dialog_open: bool,
    onboarding_completed: bool,
) -> bool {
    exit_after_onboarding_for_inline_target
        && selected_ui_mode == UiMode::Inline
        && !onboarding_dialog_open
        && onboarding_completed
}

pub(super) fn inline_event_loop(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &mut SessionSlot,
    handle: &Handle,
    file_list_rx: &mut FileListRx,
    prefix_file_list_rx: &mut FileListRx,
    file_list_tx: UnboundedSender<FileListUpdate>,
    initial_inline_runtime: Option<InlineRuntimeState>,
) -> anyhow::Result<EventLoopOutcome> {
    event_loop_with_mode(
        guard,
        app,
        theme,
        slot,
        handle,
        file_list_rx,
        prefix_file_list_rx,
        file_list_tx,
        UiMode::Inline,
        false,
        false,
        initial_inline_runtime,
    )
}

pub(super) fn event_loop(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &mut SessionSlot,
    handle: &Handle,
    file_list_rx: &mut FileListRx,
    prefix_file_list_rx: &mut FileListRx,
    file_list_tx: UnboundedSender<FileListUpdate>,
) -> anyhow::Result<EventLoopOutcome> {
    event_loop_with_mode(
        guard,
        app,
        theme,
        slot,
        handle,
        file_list_rx,
        prefix_file_list_rx,
        file_list_tx,
        UiMode::Screen,
        false,
        false,
        None,
    )
}

/// What one phase of an event-loop pass decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseFlow {
    /// Fall through to the next phase of the same pass.
    Continue,
    /// Consume the pass and start the next one: the old bare `continue`.
    NextIteration,
    /// Leave the loop with this outcome — the `phase!` macro applies it as
    /// an `Ok` exit from the pass.
    Exit(EventLoopOutcome),
}

/// Applies a phase's verdict in the loop body.
macro_rules! phase {
    ($flow:expr) => {
        match $flow {
            PhaseFlow::Continue => {}
            PhaseFlow::NextIteration => continue,
            PhaseFlow::Exit(outcome) => return Ok(outcome),
        }
    };
}

/// Everything one pass of the event loop hands to the next: what used to be
/// the run of `let mut` bindings the single 2,168-line body opened with. The
/// phase functions below take `&mut LoopState` instead of one parameter each.
struct LoopState {
    ui_mode: UiMode,
    exit_after_onboarding_for_inline_target: bool,
    exit_on_inline_fullscreen_close: bool,
    file_list_tx: UnboundedSender<FileListUpdate>,
    active_prompt: Option<ActivePrompt>,
    pending_permission: Option<PendingPermission>,
    /// Foreground command mailbox: lets the desktop app inject prompts, stop the
    /// turn, and answer permissions for this live session. No-op unless this
    /// session holds its on-disk active lock. Drops (and clears its sidecar) when
    /// the loop returns. Built on the first pass that has a session — the
    /// session may arrive after the first frame.
    foreground_control: Option<super::foreground_mailbox::ForegroundControl>,
    global_search_worker: Option<GlobalSearchWorker>,
    resume_runtime: ResumeRuntime,
    task_notification_retry_after: Option<Instant>,
    task_notification_revision_rx: Option<tokio::sync::watch::Receiver<u64>>,
    task_notification_scan_pending: bool,
    agent_gen_rx: Option<
        oneshot::Receiver<Result<rebon_plugin_agents::surface::generate::GeneratedAgent, String>>,
    >,
    /// Expansions of `Prompt` commands that ran off the loop, drained once a
    /// pass by `submit::drain_command_expansion`. The sending end lives on
    /// `AppState`, where the submit path can reach it without every surface
    /// having to carry one.
    command_expansion_rx: tokio::sync::mpsc::UnboundedReceiver<(u64, Result<String, String>)>,
    /// Time-based non-bracketed-paste burst detector. Replaces the
    /// earlier `poll_crossterm_event(2ms)` queue peek which was unreliable on
    /// Windows due to 15.6ms timer granularity. See `PasteBurst`
    /// for the full rationale.
    paste_burst: PasteBurst,
    /// Stash for events read by queue lookahead/drain helpers that must
    /// be replayed by the outer loop without reordering.
    stashed_events: VecDeque<Event>,
    /// Crossterm-backed event source. Held by `&mut` even though it's
    /// stateless so the same call sites work in unit tests with a
    /// queue-backed fake source.
    source: CrosstermSource,
    /// Off unless `REBON_INPUT_PROBE_MS` is set; see
    /// `input_latency_probe` for what it reports and why those fields.
    input_probe: super::input_latency_probe::InputLatencyProbe,
    /// Timestamp of the most recent paste flush (burst or bracketed).
    /// Consumed by `flush_burst_with_merge` to coalesce a second
    /// flush-within-grace into the preceding chip instead of letting
    /// conpty batching jitter produce two adjacent chips.
    last_paste_flush_at: Option<Instant>,
    inline_runtime: InlineRuntimeState,
    custom_status_line_runtime: super::custom_status_line::CustomStatusLineRuntime,
    rc_status_runtime: crate::tui::bridge_dialog::RcStatusRuntime,
    prefix_scan_state: crate::file_scanner::PrefixScanState,
    last_agent_view_left_press_at: Option<Instant>,
    new_session_hint_idle_since: Instant,
    last_registered_skill_slash_count: usize,
    /// Once an overflowing live overlay proves it is blocked by its first open
    /// block, avoid repeating the same reducer walk on every animation frame.
    /// Content or permission interaction changes re-arm the escape hatch.
    stalled_force_drain: InlineForceDrainStall,
    /// Throttle for the "overflow force-drain drained nothing" warning:
    /// the stalled state persists across frames, so an unthrottled warn
    /// would flood the log at frame rate.
    last_stalled_drain_warn_at: Option<Instant>,
    last_rendered_input: String,
    /// The moment the user first sees a frame. Startup is measured against
    /// it, so it is logged once, from the loop rather than from
    /// the render layer, which draws the same frame more than once.
    first_frame_drawn: bool,
    /// How many passes the loop made before the session arrived: a number
    /// for the log, so an idle loop that turned out not to be idle shows.
    passes_before_session: u64,
}

/// The chrome one frame is drawn from: derived once per pass, then read by the
/// status line and by both render arms.
struct FrameInputs<'a> {
    is_loading: bool,
    elapsed_ms: u64,
    runtime_state: rebon_tui::promptinput::PromptInputRuntimeState,
    status: StatusBarInfo<'a>,
}
pub(in crate::tui::runner) fn event_loop_with_mode(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &mut SessionSlot,
    handle: &Handle,
    file_list_rx: &mut crate::file_scanner::FileListRx,
    prefix_file_list_rx: &mut crate::file_scanner::FileListRx,
    file_list_tx: tokio::sync::mpsc::UnboundedSender<crate::file_scanner::FileListUpdate>,
    ui_mode: UiMode,
    exit_after_onboarding_for_inline_target: bool,
    exit_on_inline_fullscreen_close: bool,
    initial_inline_runtime: Option<InlineRuntimeState>,
) -> anyhow::Result<EventLoopOutcome> {
    app.ui_mode = ui_mode;
    // The loop's end of the command-expansion channel: a `Prompt` command
    // registered by a plugin is expanded on a worker thread and delivered
    // here, so the terminal keeps drawing while the plugin answers.
    let (command_expansion_tx, command_expansion_rx) = tokio::sync::mpsc::unbounded_channel();
    app.command_expansion_tx = Some(command_expansion_tx);
    let mut state = LoopState {
        ui_mode,
        exit_after_onboarding_for_inline_target,
        exit_on_inline_fullscreen_close,
        file_list_tx,
        active_prompt: None,
        pending_permission: None,
        foreground_control: None,
        global_search_worker: None,
        resume_runtime: ResumeRuntime::new(),
        task_notification_retry_after: None,
        task_notification_revision_rx: None,
        task_notification_scan_pending: true,
        agent_gen_rx: None,
        command_expansion_rx,
        paste_burst: PasteBurst::new(),
        stashed_events: VecDeque::new(),
        source: CrosstermSource,
        input_probe: super::input_latency_probe::InputLatencyProbe::from_env(),
        last_paste_flush_at: None,
        inline_runtime: initial_inline_runtime.unwrap_or_default(),
        custom_status_line_runtime: super::custom_status_line::CustomStatusLineRuntime::default(),
        rc_status_runtime: crate::tui::bridge_dialog::RcStatusRuntime::default(),
        prefix_scan_state: crate::file_scanner::PrefixScanState::new(),
        last_agent_view_left_press_at: None,
        new_session_hint_idle_since: Instant::now(),
        last_registered_skill_slash_count: slot
            .session()
            .map(|session| session.engine_half.skill_registry.user_invocable_count())
            .unwrap_or(0),
        stalled_force_drain: InlineForceDrainStall::default(),
        last_stalled_drain_warn_at: None,
        last_rendered_input: app.input.clone(),
        first_frame_drawn: false,
        passes_before_session: 0,
    };

    loop {
        if !slot.is_ready() {
            state.passes_before_session += 1;
        }
        settle_paste_buffers(guard, app, &mut state)?;
        phase!(inline_surface_gates(
            guard, app, theme, slot, handle, &mut state
        )?);

        // Are terminal events already queued (paste streaming, or a stashed
        // event from the previous iteration)? If so the whole background
        // pipeline waits — see `background_pipeline` for why that matters.
        let stashed_or_queued =
            !state.stashed_events.is_empty() || poll_crossterm_event(Duration::ZERO)?;
        if !stashed_or_queued && !state.paste_burst.has_pending() {
            phase!(background_pipeline(
                guard,
                app,
                theme,
                slot,
                handle,
                file_list_rx,
                prefix_file_list_rx,
                &mut state,
            )?);
        }

        let Some(evt) = read_next_event(app, &mut state, stashed_or_queued)? else {
            continue;
        };
        if let Event::Paste(text) = evt {
            handle_bracketed_paste(guard, app, &mut state, text)?;
            continue;
        }
        if let Event::Mouse(mouse) = evt {
            handle_mouse_input(guard, app, slot, handle, &mut state, mouse)?;
            continue;
        }
        if swallow_paste_echo_key(app, &mut state, &evt)? {
            continue;
        }
        let Event::Key(key) = evt else {
            continue;
        };
        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
            state.new_session_hint_idle_since = Instant::now();
        }

        phase!(route_key_to_surfaces(
            guard, app, theme, slot, handle, &mut state, key
        )?);
        let action = reroute_prompt_down_to_scroll(guard, app, key)?;
        let Some(action) = route_action_through_paste_burst(guard, app, &mut state, key, action)?
        else {
            continue;
        };
        if !matches!(action, KeyAction::CancelOrExit | KeyAction::Ignored) {
            app.last_ctrl_c_exit_press_ms = 0;
        }
        let Some(action) = drain_batched_scroll(guard, app, &mut state, action)? else {
            continue;
        };
        phase!(dispatch_key_action(
            app, theme, slot, handle, &mut state, action
        ));
    }
}
/// Flushes an idle paste burst, retires a spent echo suppressor, and lets the
/// OS clipboard confirm a raw-key burst before the idle timeout.
fn settle_paste_buffers(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    state: &mut LoopState,
) -> anyhow::Result<()> {
    // Flush a burst that has been idle longer than
    // BURST_IDLE_TIMEOUT. This is the path that turns a
    // fully-received paste into a `apply_paste_to_app` call
    // once the pasted events have stopped arriving — runs on
    // every iteration (including the input-fast-path ones),
    // so flush latency is bounded by poll timeout + idle
    // timeout.
    if let Some(flushed) = state.paste_burst.flush_if_idle(Instant::now()) {
        tracing::debug!(
            len = flushed.len(),
            lines = flushed.lines().count(),
            "paste: burst idle flush"
        );
        flush_burst_with_merge(app, flushed, guard, &mut state.last_paste_flush_at);
    }

    // Disarm an adopted-paste echo suppressor whose stream has
    // gone quiet. If the stream ended short of full confirmation
    // this repairs the over-adopted chip suffix; late stragglers
    // then converge back via the chip-merge grace.
    if app
        .paste_echo
        .as_ref()
        .is_some_and(|s| s.is_idle_expired(Instant::now()))
    {
        finalize_paste_echo(app, "idle");
    }

    if state.paste_burst.has_pending()
        && !state.paste_burst.adoption_attempted()
        && state.paste_burst.pending_len() >= MIN_ADOPTION_PREFIX_BYTES
        && app.paste_echo.is_none()
        && paste_echo_enabled()
        && is_main_prompt_paste_target(app)
    {
        state.paste_burst.mark_adoption_attempted();
        match try_adopt_clipboard_for_first_chunk(state.paste_burst.pending_text()) {
            ClipboardAdoption::Complete => {
                if state.stashed_events.is_empty() && !poll_crossterm_event(Duration::ZERO)? {
                    if let Some(text) = state.paste_burst.force_flush() {
                        tracing::debug!(
                            len = text.len(),
                            "paste: raw-key burst equals clipboard; flushing before idle timeout"
                        );
                        flush_burst_with_merge(app, text, guard, &mut state.last_paste_flush_at);
                    }
                }
            }
            ClipboardAdoption::Adopt {
                clipboard_raw,
                clipboard_norm,
                matched,
            } => {
                tracing::info!(
                    burst_len = state.paste_burst.pending_len(),
                    clipboard_len = clipboard_raw.len(),
                    "paste: adopting clipboard for raw-key paste burst"
                );
                let _ = state.paste_burst.force_flush();
                let input_before = app.input.clone();
                let cursor_before = app.cursor_offset;
                let text_chips_before = snapshot_text_chips(app);
                flush_burst_with_merge(app, clipboard_raw, guard, &mut state.last_paste_flush_at);
                let repair_target = find_adopted_text_repair_target(
                    app,
                    &text_chips_before,
                    &input_before,
                    cursor_before,
                    &clipboard_norm,
                );
                app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
                    clipboard_norm,
                    matched,
                    repair_target,
                    Instant::now(),
                ));
            }
            ClipboardAdoption::None => {}
        }
    }
    Ok(())
}
/// The gates that leave the loop (or restart it) before any work: onboarding
/// handing back to inline, an inline fullscreen surface opening or closing, and
/// the inline background-tasks overlay.
fn inline_surface_gates(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &mut SessionSlot,
    handle: &Handle,
    state: &mut LoopState,
) -> anyhow::Result<PhaseFlow> {
    if should_exit_completed_onboarding_to_inline(
        state.exit_after_onboarding_for_inline_target,
        slot.session()
            .map(|session| session.ui_mode)
            .unwrap_or(state.ui_mode),
        app.onboarding_dialog.is_some(),
        crate::rebon_config::has_completed_onboarding(),
    ) {
        return Ok(PhaseFlow::Exit(EventLoopOutcome::InlineFullscreenClosed));
    }
    if state.exit_on_inline_fullscreen_close
        && state.active_prompt.is_none()
        && !has_inline_fullscreen_surface(app)
    {
        return Ok(PhaseFlow::Exit(EventLoopOutcome::InlineFullscreenClosed));
    }

    if state.ui_mode == UiMode::Inline
        && state.active_prompt.is_none()
        && has_inline_fullscreen_surface(app)
    {
        reset_inline_viewport_if_pending(guard, app, &mut state.inline_runtime)?;
        emit_inline_startup_banner_if_pending(guard, app, slot, &mut state.inline_runtime)?;
        return Ok(PhaseFlow::Exit(EventLoopOutcome::InlineFullscreenRequested));
    }
    if state.ui_mode == UiMode::Inline
        && state.active_prompt.is_some()
        && app.background_tasks_dialog.is_some()
    {
        // A prompt is active, so the session is in.
        if let Some(session) = slot.session_mut() {
            if let Err(err) = run_inline_background_tasks_overlay(
                app,
                theme,
                session,
                handle,
                &mut state.active_prompt,
                &mut state.pending_permission,
                &mut state.task_notification_retry_after,
            ) {
                tracing::warn!(%err, "inline background tasks overlay failed");
            }
        }
        return Ok(PhaseFlow::NextIteration);
    }
    Ok(PhaseFlow::Continue)
}

// ── Input fast path ─────────────────────────────────────
// If terminal events are already queued (paste streaming,
// or a stashed event from the previous iteration), skip
// the entire background-work pipeline — drain_ui_channels,
// drain_team_mailbox (file IO!), dialog refreshes,
// runtime-state derivation, and the render frame all wait
// until input goes idle.
//
// Why: each pass through that pipeline costs 15–30 ms of
// cumulative work, mostly file IO inside drain_team_mailbox
// when a team is configured. Running it between every key
// event during a non-bracketed paste inflates the
// inter-char gap the `PasteBurst` detector measures from
// the terminal's actual ~1–5 ms paste pace up into the
// 15–30 ms range, which sits *inside* fast-typing speed.
// Time-based activation never trips, the detector sees
// every paste char as ordinary typing, pasted Enters
// submit individually, and the user watches their paste
// get re-typed line-by-line.
//
// The fix: do not run the pipeline between hot key events.
// Engine output, mailbox drains, dialog state — none of it
// is *visible* until rendering happens, and rendering is
// already gated on `events_queued`. So delaying the
// bookkeeping by a few hundred ms (the duration of a paste
// burst) is a no-op from the user's perspective. The
// pipeline runs the moment the input queue drains, which
// is exactly when the user could possibly see the result.
#[allow(clippy::too_many_arguments)]
fn background_pipeline(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &mut SessionSlot,
    handle: &Handle,
    file_list_rx: &mut crate::file_scanner::FileListRx,
    prefix_file_list_rx: &mut crate::file_scanner::FileListRx,
    state: &mut LoopState,
) -> anyhow::Result<PhaseFlow> {
    // ── Background pipeline (only when input is idle) ──
    // Drain channel events BEFORE checking completion so that
    // FinalizeTurn sees the full overlay. This prevents the race
    // where FinalizeTurn promotes the overlay to the transcript
    // and then late-arriving events re-populate the cleared
    // overlay, causing the same tool call to render twice.
    // Settle the unseen divider from the previous frame's state
    // before draining new events, so the divider position is
    // cleared exactly once per settled frame.
    app.unseen_divider.settle();
    match session_pipeline(app, slot, handle, file_list_rx, prefix_file_list_rx, state)? {
        PhaseFlow::Continue => {}
        other => return Ok(other),
    }
    sync_input_surfaces(app, slot, handle, state);
    let frame_inputs = derive_frame_inputs(app, slot, state);
    if let (Ok(size), Some(session)) = (guard.terminal().size(), slot.session()) {
        if state.custom_status_line_runtime.drain_and_maybe_spawn(
            app,
            session,
            &frame_inputs.status,
            size.width,
            size.height,
        ) {
            return Ok(PhaseFlow::NextIteration);
        }
    }
    // Render unless the burst detector is actively buffering
    // (skips per-char render overhead during large pastes).
    // We already know the input queue is idle from the
    // `stashed_or_queued` check above.
    if !state.paste_burst.has_pending() {
        render_pass(guard, app, theme, slot, state, &frame_inputs)?;
    }
    Ok(PhaseFlow::Continue)
}
/// Installs the session once it arrives, then runs every drain, refresh and
/// auto-trigger that belongs to a session-bearing idle pass.
fn session_pipeline(
    app: &mut AppState,
    slot: &mut SessionSlot,
    handle: &Handle,
    file_list_rx: &mut crate::file_scanner::FileListRx,
    prefix_file_list_rx: &mut crate::file_scanner::FileListRx,
    state: &mut LoopState,
) -> anyhow::Result<PhaseFlow> {
    // The session may still be building: until it
    // arrives the loop draws, takes input, and polls for it; the
    // moment it does, it is installed and Enter pressed early is
    // replayed through the ordinary submit path.
    if !slot.is_ready() {
        match slot.poll_arrival() {
            Some(Ok(mut session)) => {
                let install_options = super::run_blocking_entry::InstallOptions {
                    math_rendering: slot.math_rendering_mode,
                    ui_mode: state.ui_mode,
                    permission_mode_cycled: slot.permission_mode_cycled(),
                };
                super::run_blocking_entry::install_session(
                    app,
                    &mut session,
                    handle,
                    None,
                    install_options,
                );
                tracing::info!(
                    passes_before_session = state.passes_before_session,
                    first_frame_drawn = state.first_frame_drawn,
                    "rebon startup: session arrived in the event loop"
                );
                slot.install(session);
            }
            Some(Err(err)) => return Err(err),
            None => {}
        }
    }
    let replay_deferred_enter = slot.is_ready() && slot.take_deferred_enter();
    if let Some(session) = slot.session_mut() {
        app.sync_task_snapshots(session.engine_half.tasks.snapshots());
        let foreground_control = state.foreground_control.get_or_insert_with(|| {
            let mut control = super::foreground_mailbox::ForegroundControl::new(session);
            control
                .set_command_handler(super::foreground_mailbox::execute_foreground_session_command);
            control
        });
        if replay_deferred_enter && !app.input.trim().is_empty() {
            let text = app.input.clone();
            tracing::info!("rebon startup: submitting the prompt typed before the session");
            if submit_or_queue(
                app,
                text,
                session,
                handle,
                &mut state.active_prompt,
                &mut state.pending_permission,
                state.ui_mode,
            ) {
                return Ok(PhaseFlow::Exit(EventLoopOutcome::Exit));
            }
        }

        drain_ui_channels(
            app,
            session,
            &mut state.pending_permission,
            &mut state.active_prompt,
        );
        reconcile_mid_turn_consumed_queued_submits(app);
        super::deferred_questions::drain_deferred_question_answers(
            app,
            session,
            handle,
            &mut state.active_prompt,
        );
        // Apply any commands the desktop app queued for this session
        // (inject / stop / answer-permission) right after the inbound
        // channels drain, so an injected prompt rides the same downstream
        // bookkeeping this pass and a remote answer sees the freshly
        // populated permission.
        if app.resume_dialog.is_none() {
            foreground_control.drain(
                app,
                session,
                handle,
                &mut state.active_prompt,
                &mut state.pending_permission,
                state.ui_mode,
            );
        }
        maybe_update_loading_state(
            app,
            session,
            handle,
            &mut state.active_prompt,
            &mut state.task_notification_retry_after,
        );
        if super::task_runtime::task_notification_scan_needed(
            &mut state.task_notification_revision_rx,
            session
                .engine_half
                .task_notification_poller
                .subscribe_notification_revision(),
        ) {
            state.task_notification_scan_pending = true;
        }
        if !session
            .engine_half
            .tasks
            .unnotified_question_escalation_notifications()
            .is_empty()
        {
            state.task_notification_scan_pending = true;
        }
        if state
            .task_notification_retry_after
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            state.task_notification_retry_after = None;
            state.task_notification_scan_pending = true;
        }
        refresh_remote_background_attachment(app, session, &mut state.pending_permission);
        // `--hosted` fires from here rather than at startup because the
        // session it means is not settled yet at startup: a resume dialog
        // is still deciding which conversation this is, and handing over
        // the empty placeholder would host the wrong one. One-shot.
        if super::session_detach_attach::start_hosted_session_if_requested(
            app,
            session,
            &state.active_prompt,
        ) {
            tracing::info!("rebon: --hosted requested, handing this session to a worker");
        }
        // Attaching reads the session record and opens the worker's
        // stream — work the first frame must not wait for.
        if state.first_frame_drawn {
            super::session_detach_attach::poll_pending_hosted_session(
                app,
                session,
                &mut state.pending_permission,
            );
        }
        drain_file_list(app, file_list_rx);
        drain_file_list(app, prefix_file_list_rx);
        drain_update_check(app);
        super::task_runtime::drain_inline_shell_commands(
            app,
            session,
            state.active_prompt.is_none(),
        );
        super::submit::drain_command_expansion(
            app,
            session,
            handle,
            &mut state.active_prompt,
            &mut state.command_expansion_rx,
        );
        drain_mcp_load_result(app, session);
        super::run_blocking_entry::drain_session_start_hook(app, session);
        // Skip the TUI drain while a prompt is running: the tasks
        // plugin's mailbox producer, on the engine's attachment seat,
        // drains the same file-backed mailbox mid-turn (see
        // `rebon_plugin_tasks::drain_teammate_mailbox_for`). Running
        // both in the same process would race on the read-modify-write
        // cycle inside `drain_unread_mailbox` and silently drop one
        // side's messages. The idle-path drain stays here so the
        // auto-submit (pending_teammate_prompts -> new turn) still
        // fires when nothing is running.
        if state.active_prompt.is_none() {
            drain_mcp_channel_notifications(app, session);
            drain_team_mailbox(app, &session.session_id);
        }

        // Auto-trigger: when teammate messages arrived and the leader
        // is idle (no active prompt, no queued commands), auto-submit
        // the pending teammate messages as a new model turn.
        if app.resume_dialog.is_none()
            && (!app.pending_channel_prompts.is_empty() || !app.pending_teammate_prompts.is_empty())
            && state.active_prompt.is_none()
            && app.queued_commands.is_empty()
        {
            if let Some(admitted) =
                admit_idle_context_prompt(app, session, handle, state.active_prompt.as_ref())
            {
                state.active_prompt = Some(admitted);
                app.is_loading = true;
            }
        }

        // Auto-trigger: background tasks report terminal results via
        // task-notification XML. Feed those notifications back into the
        // coordinator as a fresh turn once the UI is idle.
        if app.resume_dialog.is_none()
            && state.active_prompt.is_none()
            && app.queued_commands.is_empty()
            && state.task_notification_retry_after.is_none()
            && state.task_notification_scan_pending
        {
            state.task_notification_scan_pending = false;
            if !maybe_spawn_task_notification_prompt(app, session, handle, &mut state.active_prompt)
                && (!session
                    .engine_half
                    .task_notification_poller
                    .unnotified_notifications_for_session(&session.session_id)
                    .is_empty()
                    || !session
                        .engine_half
                        .tasks
                        .unnotified_question_escalation_notifications()
                        .is_empty())
            {
                state.task_notification_retry_after =
                    Some(Instant::now() + Duration::from_secs(10));
            }
        }

        let skill_count = session.engine_half.skill_registry.user_invocable_count();
        if skill_count != state.last_registered_skill_slash_count {
            register_registered_skill_slash_commands(
                &mut app.slash_commands,
                &session.engine_half.skill_registry,
            );
            state.last_registered_skill_slash_count = skill_count;
        }

        // Update the divider's message count after draining new
        // messages so on_scroll_away snapshots the latest count.
        let msg_count = app.rebon_tui.transcript.rows().len();
        app.unseen_divider.set_message_count(msg_count);
        sync_global_search_dialog(
            app,
            Path::new(&session.cwd),
            &mut state.global_search_worker,
        );
        sync_resume_dialog(app, session, handle, &mut state.resume_runtime);
        super::compact_runtime::sync_compact_run(app, session);
        sync_permission_view(app, &state.pending_permission);
        // Publish this session's live status (busy + pending permission) so
        // the desktop app can render controls for it. Throttled internally.
        foreground_control.publish(&state.active_prompt, &state.pending_permission);
        app.sync_task_snapshots(session.engine_half.tasks.snapshots());
        refresh_teams_dialog(app);
        refresh_agent_view_if_due(app);
        state
            .rc_status_runtime
            .sync(app, Path::new(&session.cwd), Instant::now());
        refresh_background_tasks_dialog(app);
        sync_foreground_agent_view(app, session.engine_half.tasks.as_ref());
        refresh_live_agent_tool_activity(app);
        refresh_agents_dialog(app);
        drain_agent_generation(app, &mut state.agent_gen_rx);
        drain_ultraplan_reviewer_verdicts(app, session);
    } else {
        // No session yet: the local resources the first frame owns.
        drain_file_list(app, file_list_rx);
        drain_file_list(app, prefix_file_list_rx);
        drain_update_check(app);
        let msg_count = app.rebon_tui.transcript.rows().len();
        app.unseen_divider.set_message_count(msg_count);
    }
    Ok(PhaseFlow::Continue)
}
/// Prompt history, cursor clamping, the slash/@ pickers and the vim-mode sync:
/// the input surfaces that settle before the frame is derived.
fn sync_input_surfaces(
    app: &mut AppState,
    slot: &mut SessionSlot,
    handle: &Handle,
    state: &mut LoopState,
) {
    // The prompt history arrives from its loader thread whenever it
    // is done; Up-arrow has nothing to walk until then.
    super::prompt_history::apply_loaded_input_history_if_idle(app, &mut slot.input_history);

    let normalized_cursor = clamp_cursor_offset(&app.input, app.cursor_offset);
    if normalized_cursor != app.cursor_offset {
        tracing::warn!(
            cursor_offset = app.cursor_offset,
            normalized_cursor,
            input_len = app.input.len(),
            "normalized invalid prompt cursor"
        );
        app.cursor_offset = normalized_cursor;
    }

    if app.has_modal_overlay() || app.history_index > 0 {
        app.slash_picker = None;
        app.at_mention_picker = None;
    } else {
        crate::tui::slash_picker::sync(&mut app.slash_picker, &app.input, &app.slash_commands);
        // Only sync the @ picker when the / picker is not active
        // (they are mutually exclusive overlays).
        if app.slash_picker.is_none() {
            if let Some((_, query)) =
                crate::tui::at_mention_picker::find_at_token(&app.input, app.cursor_offset)
            {
                if query.contains(['/', '\\']) {
                    crate::file_scanner::maybe_spawn_prefix_scan(
                        handle,
                        slot.cwd(),
                        &query,
                        &mut state.prefix_scan_state,
                        state.file_list_tx.clone(),
                    );
                }
            }
            crate::tui::at_mention_picker::sync(
                &mut app.at_mention_picker,
                &app.input,
                app.cursor_offset,
                slot.cwd(),
                &app.file_index,
                &app.file_scan_status,
            );
        } else {
            app.at_mention_picker = None;
        }
    }

    // ── Vim mode sync (rebon_tui::input) ─────────────────────────
    // If an initial vim mode was configured and differs from the
    // current mode, sync it to the configured initial mode.
    if let Some(current) = app.vim_mode {
        if needs_mode_sync(app.initial_vim_mode, current) {
            app.vim_mode = app.initial_vim_mode;
        }
    }
}
/// Derives everything the frame is drawn from: the turn clock, the retry
/// notice, the prompt runtime state and the status bar.
fn derive_frame_inputs<'slot>(
    app: &mut AppState,
    slot: &'slot SessionSlot,
    state: &mut LoopState,
) -> FrameInputs<'slot> {
    // The turn in flight: the prompt future this process holds, or
    // the turn the owner of a mirrored session announced. Reading
    // only the former is how a hosted session — the default — ran
    // every turn with the spinner off, the clock at zero and the
    // title saying idle: the mirror learned the turn had started
    // and this line overwrote it a few hundred microseconds later,
    // every frame.
    let hosted_turn_started_at = slot
        .session()
        .and_then(|session| session.remote_background_attachment.as_ref())
        .and_then(|remote| remote.running_turn_started_at());
    let elapsed_ms = state
        .active_prompt
        .as_ref()
        .map(|a| a.started_at)
        .or(hosted_turn_started_at)
        .map(|started_at| started_at.elapsed().as_millis() as u64)
        .unwrap_or(0);
    let is_loading = state.active_prompt.is_some() || hosted_turn_started_at.is_some();
    app.is_loading = is_loading;
    // Pick a new random spinner verb on each loading transition.
    if is_loading && !app.was_loading {
        app.spinner_verb = crate::tui::spinner_verbs::pick_random_verb().to_string();
    }
    app.was_loading = is_loading;
    // Poll the retry notifier for display in prompt chrome. A mirror
    // has no model client to poll; the owner publishes its retries
    // to the job record and the mirror carries the last word.
    app.retry_info = slot
        .session()
        .and_then(|session| session.engine_half.retry_notifier.current())
        .or_else(|| {
            slot.session()
                .and_then(|session| session.remote_background_attachment.as_ref())
                .and_then(|remote| remote.owner_retry)
                .map(|retry| rebon_api::RetryProgress {
                    attempt: retry.attempt,
                    max_retries: retry.max_retries,
                })
        });
    app.update_contextual_tip(wall_clock_ms());
    let runtime_input = app.build_runtime_input();
    let runtime_state = derive_prompt_input_runtime_state(&runtime_input, prompt_rainbow_color);

    let effort_display = inline_banner_effort_display(app);
    let fast_mode_on = match slot.session() {
        Some(session) => {
            session.model.service_tier_available && session.model.service_tier.is_fast()
        }
        None => slot.preview().fast_mode,
    };
    let fast_mode_display = if fast_mode_on {
        String::from("[Fast]")
    } else {
        String::new()
    };
    // Context-budget percentage: how much room is left before
    // auto-compact fires. Only shown after the first API response
    // reports input_tokens (last_input_tokens > 0).
    let last_input_tokens = slot
        .session()
        .map(|session| session.model.prune_level.budget.last_input_tokens())
        .unwrap_or(0);
    let context_left_pct = slot.session().and_then(|session| {
        let budget = &session.model.prune_level.budget;
        // A mirror's budget never sees a request: the model client
        // that would update it runs in the owner. What the owner
        // streams per response is the same input count, so the
        // percentage is derived from that instead of showing nothing.
        let tokens = if session.remote_background_attachment.is_some() {
            app.usage().last_turn.input_tokens
        } else {
            budget.last_input_tokens()
        };
        if tokens > 0 {
            let threshold = budget.auto_compact_threshold();
            let pct = if threshold == 0 || tokens >= threshold {
                0
            } else {
                (((threshold - tokens) as f64 / threshold as f64) * 100.0).round() as u32
            };
            Some(pct)
        } else {
            None
        }
    });
    // Gate the `/new` hint behind an idleness check. Showing it
    // while the user is waiting on the engine, a foreground task, or
    // any coordinator-backed agent invites them to trash in-flight work.
    let task_snapshots = app.agent_task_snapshots();
    let now_ms = wall_clock_ms();
    let agent_activity =
        active_agent_footer_status(&task_snapshots, now_ms, app.foregrounded_task_id.as_deref());
    let has_active_background_agent =
        app.coordinator_mode && has_active_background_agent(&task_snapshots);
    let has_active_background_shell = has_active_background_shell(&task_snapshots);
    let has_foreground_coordinator_task = task_snapshots.iter().any(|s| {
        matches!(
            s.status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
                | rebon_plugin_tasks::runtime::TaskStatus::Pending
        ) && !s.is_backgrounded
    });
    let has_in_progress_tool_task = self::render::collect_task_views()
        .iter()
        .any(|t| t.status == TaskListStatus::InProgress);
    let new_session_hint_eligible = !is_loading
        && agent_activity.is_none()
        && !has_active_background_agent
        && !has_active_background_shell
        && !has_foreground_coordinator_task
        && !has_in_progress_tool_task;
    if !new_session_hint_eligible {
        state.new_session_hint_idle_since = Instant::now();
    }
    let new_session_hint = footer_new_session_hint(
        last_input_tokens,
        is_loading,
        agent_activity.is_some(),
        has_active_background_agent || has_active_background_shell,
        has_foreground_coordinator_task,
        has_in_progress_tool_task,
        state.new_session_hint_idle_since.elapsed(),
    );
    // The ctrl+b background hint is rendered inline under each
    // in-progress Agent tool card (see
    // `rebon_tui::render::render_streaming_tool_use`), not in the
    // footer — showing it next to the running agent is more
    // discoverable than a distant pill.
    let footer_action_hint = pending_footer_action_hint(
        state.last_agent_view_left_press_at,
        app.last_ctrl_c_exit_press_ms,
    )
    .or_else(|| slot.hosted_wait_footer_hint());
    let status = StatusBarInfo {
        provider: slot.provider_name(),
        model: slot.model_name(),
        cwd: slot.cwd(),
        elapsed_ms,
        effort_display,
        fast_mode_display,
        context_left_pct,
        agent_activity,
        goal_activity: app
            .goal
            .as_ref()
            .filter(|goal| goal.is_active() || goal.is_paused() || goal.is_complete())
            .map(|goal| super::status_bar::GoalActivityInfo {
                status: goal.status,
                elapsed_ms: now_ms.saturating_sub(goal.started_at_ms),
            }),
        footer_action_hint,
        new_session_hint,
    };
    FrameInputs {
        is_loading,
        elapsed_ms,
        runtime_state,
        status,
    }
}
/// One drawn frame: the synchronized-output bracket, the window title, the
/// mode-specific draw and the out-of-band caret placement.
fn render_pass(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &SessionSlot,
    state: &mut LoopState,
    frame_inputs: &FrameInputs<'_>,
) -> anyhow::Result<()> {
    // Open a DEC 2026 synchronized frame around the WHOLE frame —
    // title push, viewport resize, `insert_before` commits, the
    // `draw`, and the out-of-band cursor `MoveTo`/`Show`. Without
    // it the terminal repaints each of those terminal writes as it
    // sees them, so the inline composer's per-frame
    // `resize → insert_before → draw` flashes through half-drawn
    // intermediate states (the "stutter" flicker). The guard's drop
    // emits the matching `EndSynchronizedUpdate`, so the frame is
    // closed atomically even when any `?` below bails out early.
    let _sync_frame = super::terminal_capabilities::begin_synchronized_frame();
    let input_changed =
        inline_input_should_scroll_into_view(state.ui_mode, &app.input, &state.last_rendered_input);
    let input_scroll_active = if input_changed {
        guard
            .begin_inline_input_scroll()
            .context("enable inline input scroll-to-bottom")?
    } else {
        false
    };

    // Push the current session title to the terminal-window
    // title bar. `set_title` dedupes against the last-emitted
    // value, so this is cheap to call every frame.
    let base_title = app.session_title.as_deref().unwrap_or("rebon");
    let window_title = terminal_title_for_request_state(
        base_title,
        frame_inputs.is_loading,
        frame_inputs.elapsed_ms,
        state.pending_permission.is_some(),
        app.prompt_completion_status,
    );
    guard.set_title(&window_title);

    // Capture the prompt caret position out-of-band so we can
    // emit `MoveTo + Show` in the correct order AFTER
    // `terminal.draw` returns. See the long comment in
    // `render_prompt_surface` for why we bypass ratatui's
    // built-in cursor management: its `show_cursor` →
    // `set_cursor_position` order causes the cursor to flash
    // visibly at the last-drawn cell every frame, and on
    // Windows that makes the IME candidate window drift
    // across the screen.
    // Hide before the frame flush, not only after it: animated
    // spinner/timer updates repaint cells away from the prompt caret.
    guard
        .terminal()
        .hide_cursor()
        .context("hide terminal cursor before drawing TUI frame")?;
    if !state.first_frame_drawn {
        tracing::info!("rebon startup: first frame render started");
    }

    let mut cursor_hint: Option<(u16, u16)> = None;
    match state.ui_mode {
        UiMode::Screen => {
            if app.pending_page_hard_refresh.take().is_some() {
                reset_page_layout_state(app, &mut state.inline_runtime);
                guard
                    .terminal()
                    .clear()
                    .context("clear alternate-screen TUI for hard refresh")?;
            }
            guard
                .terminal()
                .draw(|frame| {
                    render_frame(
                        frame,
                        app,
                        &frame_inputs.runtime_state,
                        theme,
                        frame_inputs.is_loading,
                        &frame_inputs.status,
                        slot.session(),
                        &mut cursor_hint,
                    );
                })
                .context("draw alternate-screen TUI frame")?;
        }
        UiMode::Inline => {
            render_inline_pass(
                guard,
                app,
                theme,
                slot,
                state,
                frame_inputs,
                &mut cursor_hint,
            )?;
        }
    }
    if !state.first_frame_drawn {
        state.first_frame_drawn = true;
        tracing::info!(ui_mode = ?state.ui_mode, "rebon startup: first frame drawn");
    }
    if let Some((x, y)) = cursor_hint {
        // Move FIRST (cursor still hidden from the end-of-draw
        // `hide_cursor()`), then Show. A single `execute!`
        // with both commands queues them before flushing, so
        // the terminal sees them as one atomic position
        // update and the IME anchors to the correct caret.
        use ratatui::crossterm::cursor::{MoveTo, Show};
        use ratatui::crossterm::execute;
        let _ = execute!(std::io::stdout(), MoveTo(x, y), Show);
        // Keep the terminal's tracked cursor consistent with the
        // caret we just placed out-of-band. The next `resize()`
        // derives the inline viewport's new top from
        // `last_known_cursor_pos - viewport_top`; left at the last
        // buffer-diff cell (a row or two below the caret) that math
        // places the viewport too high and clears into the
        // committed banner above it. The cursor is already at
        // (x, y), so this only updates bookkeeping — no extra write.
        guard.terminal().note_cursor_position((x, y));
    }
    guard
        .end_inline_input_scroll(input_scroll_active)
        .context("disable inline input scroll-to-bottom")?;
    if input_changed {
        state.last_rendered_input.clone_from(&app.input);
    }
    // The frame is on the terminal: every event read before
    // it has now been answered. See `input_latency_probe`.
    state
        .input_probe
        .note_frame(app.input.len(), app.pasted_contents.len());
    Ok(())
}
/// The inline arm of `render_pass`: reconcile scrollback and the live viewport
/// against the terminal, commit what has settled, then draw.
fn render_inline_pass(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &SessionSlot,
    state: &mut LoopState,
    frame_inputs: &FrameInputs<'_>,
    cursor_hint: &mut Option<(u16, u16)>,
) -> anyhow::Result<()> {
    // A terminal WIDTH change reflows the committed rows the
    // inline arm already pushed into real scrollback: the terminal
    // re-wraps every scrollback row to the new width, mangling the
    // startup banner's box-drawing borders into stray `┌───┌───`
    // fragments. We never repaint scrollback, so the incremental
    // autoresize path below — which only touches the live viewport
    // — can't fix them. Instead, still INSIDE the frame's BSU/ESU
    // synchronized-output bracket (`_sync_frame` above), wipe screen
    // + scrollback and rebuild every committed row at the new width.
    // The sync bracket makes the wipe+rebuild swap in atomically, so
    // — unlike a bare clear — it never flashes blank.
    //
    // Two bounds keep that rebuild from BECOMING the flicker on long
    // transcripts (a BSU/ESU frame is only atomic for a BOUNDED
    // paint): a height change — grow OR shrink — no longer triggers
    // it (fixed-width scrollback does not reflow on height; the live
    // viewport follows height on the incremental path), and past
    // `inline_resize_repaint_budget()` the repaint itself falls back
    // to a bounded light path (see `inline_full_repaint_for_resize`).
    let term_size = guard
        .terminal()
        .size()
        .context("query terminal size before inline repaint")?;
    if let Some(page_name) = app.pending_page_hard_refresh.take() {
        let pinned_live_prefix = inline_pinned_live_prefix(
            state.active_prompt.as_ref(),
            app.rebon_tui.transcript.rows(),
            0,
            app.rebon_tui.overlay.is_empty(),
            remote_attachment_live_prefix(
                slot.session()
                    .and_then(|session| session.remote_background_attachment.as_ref()),
                app.rebon_tui.transcript.rows(),
            ),
        )
        .min(super::task_runtime::inline_shell_live_prefix(app));
        rewind_inline_page_switch(
            guard.terminal(),
            app,
            theme,
            &mut state.inline_runtime,
            &page_name,
            pinned_live_prefix,
        )?;
    } else if state
        .inline_runtime
        .resize_needs_full_repaint((term_size.width, term_size.height))
    {
        let pinned_live_prefix = inline_pinned_live_prefix(
            state.active_prompt.as_ref(),
            app.rebon_tui.transcript.rows(),
            state.inline_runtime.commit_cursor.committed_row_count(),
            app.rebon_tui.overlay.is_empty(),
            remote_attachment_live_prefix(
                slot.session()
                    .and_then(|session| session.remote_background_attachment.as_ref()),
                app.rebon_tui.transcript.rows(),
            ),
        )
        .min(super::task_runtime::inline_shell_live_prefix(app));
        inline_full_repaint_for_resize(
            guard,
            app,
            theme,
            slot,
            &mut state.inline_runtime,
            pinned_live_prefix,
        )?;
    }
    reset_inline_viewport_if_pending(guard, app, &mut state.inline_runtime)?;
    emit_inline_startup_banner_if_pending(guard, app, slot, &mut state.inline_runtime)?;
    // Reconcile the terminal to any pending OS resize BEFORE we
    // flush commits or resize the viewport below. A bare
    // `Event::Resize` falls through the loop (`continue`) without
    // redrawing, so by the time we reach here the backend size can
    // already differ from the terminal's `last_known_area`.
    // `flush_inline_commits`' `insert_before` and the
    // `set_viewport_height` / `shrink_inline_viewport_keeping_top`
    // reconciliation all position the viewport relative to
    // `last_known_area`; if it is still the OLD geometry they move
    // against stale bounds, and the `autoresize()` inside the
    // trailing `draw()` then repositions a SECOND time against the
    // new bounds. The two moves disagree and `resize()`'s targeted
    // clear (derived from the stale `previous_viewport_area`) misses
    // the old prompt/notice rows, which survive on screen as the
    // resize ghost ("double image"). Running `autoresize()` up front settles
    // `last_known_area` and clears the old live region once, so every
    // geometry computation below — and the now-no-op autoresize inside
    // `draw()` — sees consistent bounds. Cheap no-op when the size is
    // unchanged (the overwhelmingly common case).
    guard
        .terminal()
        .autoresize()
        .context("autoresize inline terminal before commit")?;
    if let Some(actual_height) = guard.terminal().inline_viewport_height() {
        // A resize onto a shorter screen clamps the inline height
        // down; keep our tracked height in lockstep so the reconcile
        // below measures the real delta instead of chasing a phantom
        // shrink/grow against the pre-resize height.
        state.inline_runtime.current_viewport_height = actual_height;
    }
    // Live content can grow past the available transcript area in
    // two ways: (a) streaming Read/Search tool chains accumulate in
    // the held overlay cluster, (b) dynamic prompt input / queue /
    // picker rows grow and squeeze the transcript area below them.
    // Resolution order:
    //   1. If the live demand exceeds the *whole terminal* even with
    //      the inline viewport grown to its cap, no amount of resizing
    //      helps — force-drain the held sealed prefix into transcript →
    //      scrollback so the overflowing blocks deposit into real
    //      scrollback before draw.
    //   2. After draining, compute the desired inline viewport height
    //      (floored at the user-configured initial height, capped at
    //      terminal height) and call `Terminal::set_viewport_height()`
    //      so the viewport itself grows/shrinks to fit. With ratatui
    //      PR#1964 backported in our vendored fork this is the
    //      load-bearing fix; before it landed the viewport was fixed
    //      and step 1 was the only escape, which silently sacrificed
    //      grouping every time live content got tall.
    let terminal_area_pre_flush = guard
        .terminal()
        .size()
        .context("query terminal size for inline overflow measurement")?;
    let pre_flush_committed_rows = state.inline_runtime.commit_cursor.committed_row_count();
    let viewport_floor = state.inline_runtime.initial_viewport_height.max(1);
    inline_force_drain_overflow(
        app,
        theme,
        state,
        terminal_area_pre_flush,
        viewport_floor,
        pre_flush_committed_rows,
        frame_inputs.status.elapsed_ms,
    );
    let pinned_live_prefix = inline_pinned_live_prefix(
        state.active_prompt.as_ref(),
        app.rebon_tui.transcript.rows(),
        pre_flush_committed_rows,
        app.rebon_tui.overlay.is_empty(),
        remote_attachment_live_prefix(
            slot.session()
                .and_then(|session| session.remote_background_attachment.as_ref()),
            app.rebon_tui.transcript.rows(),
        ),
    )
    .min(super::task_runtime::inline_shell_live_prefix(app));
    inline_shrink_before_commit(
        guard,
        app,
        theme,
        state,
        terminal_area_pre_flush,
        viewport_floor,
        pre_flush_committed_rows,
        pinned_live_prefix,
        frame_inputs.status.elapsed_ms,
    )?;
    flush_inline_commits(
        guard.terminal(),
        app,
        theme,
        &mut state.inline_runtime,
        pinned_live_prefix,
    )
    .context("flush committed inline transcript rows")?;
    let committed_rows = state.inline_runtime.commit_cursor.committed_row_count();
    let terminal_area_for_resize = guard
        .terminal()
        .size()
        .context("query terminal size before inline viewport resize")?;
    inline_reconcile_viewport(
        guard,
        app,
        theme,
        state,
        terminal_area_for_resize,
        viewport_floor,
        committed_rows,
        frame_inputs.status.elapsed_ms,
    )?;
    guard
        .terminal()
        .draw(|frame| {
            render::render_inline_frame(
                frame,
                app,
                &frame_inputs.runtime_state,
                theme,
                frame_inputs.is_loading,
                &frame_inputs.status,
                slot.session(),
                committed_rows,
                terminal_area_for_resize.height,
                cursor_hint,
            );
        })
        .context("draw inline TUI frame")?;
    Ok(())
}
/// Step 1 of the inline overflow resolution: when even a full-height viewport
/// cannot hold the live tail, push the sealed prefix into real scrollback.
fn inline_force_drain_overflow(
    app: &mut AppState,
    theme: &mut RenderTheme,
    state: &mut LoopState,
    terminal_area_pre_flush: ratatui::layout::Size,
    viewport_floor: u16,
    pre_flush_committed_rows: usize,
    elapsed_ms: u64,
) {
    // Do NOT gate this on `current_viewport_height <
    // terminal_height`. `inline_live_content_overflows_viewport`
    // already measures the live demand against the *whole
    // terminal*'s transcript area, so it returns true only when
    // even the viewport grown to its terminal-height cap cannot
    // fit the tail — exactly the case step 1 above describes,
    // where growing is exhausted and draining is the only remedy.
    // The old viewport guard disabled the drain precisely once the
    // streaming-grown viewport reached that cap, stranding the
    // overflowing overlay: its top was clipped by the bottom-
    // anchored render and, being overlay (not transcript) content,
    // never reached scrollback.
    // The cheap gate first: while a recorded stall
    // stands, the full overflow measurement is pure
    // waste — the drain would be skipped anyway.
    if state.stalled_force_drain.should_attempt(app)
        && render::inline_live_content_overflows_viewport(
            app,
            theme,
            render::InlineViewportHeightInput {
                width: terminal_area_pre_flush.width,
                terminal_height: terminal_area_pre_flush.height,
                base_height: viewport_floor,
                committed_rows: pre_flush_committed_rows,
                elapsed_ms: elapsed_ms,
            },
        )
    {
        let transcript_len_before_force_drain = app.rebon_tui.transcript.len();
        crate::tui::update::force_drain_overlay_sealed_prefix(app);
        if app.rebon_tui.transcript.len() > transcript_len_before_force_drain {
            state.stalled_force_drain.clear();
            if let Some(active) = state.active_prompt.as_mut() {
                active.reply_started = true;
            }
        } else if !app.rebon_tui.overlay.is_empty() {
            state.stalled_force_drain.record(app);
            // The escape hatch fired but drained nothing: the
            // sealed-prefix walk is stalled on an open block
            // (a thinking block whose ThinkingEnd was lost, or
            // a tool stuck non-terminal). Everything behind it
            // stays live, gets clipped off the top of the
            // viewport, and cannot reach scrollback until the
            // turn finalizes — surface the stall instead of
            // failing silently every frame.
            let now = Instant::now();
            let should_warn = !state
                .last_stalled_drain_warn_at
                .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(5));
            if should_warn {
                state.last_stalled_drain_warn_at = Some(now);
                let first_open_block =
                    app.rebon_tui
                        .overlay
                        .blocks
                        .iter()
                        .find_map(|block| match block {
                            rebon_tui::StreamingContentBlock::Thinking(t) if t.is_streaming => {
                                Some("Thinking(streaming)".to_string())
                            }
                            rebon_tui::StreamingContentBlock::ToolUse(t)
                                if !matches!(
                                    t.status,
                                    rebon_types::ToolCallStatus::Completed
                                        | rebon_types::ToolCallStatus::Failed
                                ) =>
                            {
                                Some(format!("Tool({}, {:?})", t.tool_name, t.status))
                            }
                            _ => None,
                        });
                tracing::warn!(
                    target: "inline_flush",
                    overlay_blocks = app.rebon_tui.overlay.blocks.len(),
                    first_open_block = first_open_block.as_deref().unwrap_or("none"),
                    "inline overflow force-drain drained nothing; live overlay is stalled behind an open block"
                );
            }
        }
    }
}
/// The commit-driven shrink that has to run *before* `insert_before`, so the
/// rows the commit frees become its landing zone instead of scrollback churn.
#[allow(clippy::too_many_arguments)]
fn inline_shrink_before_commit(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    state: &mut LoopState,
    terminal_area_pre_flush: ratatui::layout::Size,
    viewport_floor: u16,
    pre_flush_committed_rows: usize,
    pinned_live_prefix: usize,
    elapsed_ms: u64,
) -> anyhow::Result<()> {
    // A *commit-driven* shrink must be applied BEFORE the
    // flush, not after. When the live tail is about to lose
    // rows because they are being committed, the viewport
    // has to shrink by that much; doing it first — keeping
    // the viewport's top fixed so the freed rows open up at
    // the bottom — makes the freed rows the exact landing
    // zone for the commit's `insert_before`, which then
    // re-anchors the viewport to the screen bottom with zero
    // scrollback churn. Shrinking *after* the flush instead
    // forces `insert_before` to over-scroll the still-tall
    // viewport, pushing still-visible committed rows
    // irreversibly into scrollback and leaving the
    // mid-history blank band (plus per-frame streaming
    // flicker). `predicted_committed_rows` mirrors the commit
    // cursor's `max_committable` (`pinned_live_prefix` capped
    // by the transcript length): when it advances past the
    // current committed count the flush *will* paint, so the
    // top-anchored shrink is safe; the post-flush reconcile
    // below corrects any residual delta with the real count.
    let predicted_committed_rows = pinned_live_prefix.min(app.rebon_tui.transcript.len());
    if predicted_committed_rows > pre_flush_committed_rows {
        let predicted_desired_height = render::desired_inline_viewport_height(
            app,
            theme,
            render::InlineViewportHeightInput {
                width: terminal_area_pre_flush.width,
                terminal_height: terminal_area_pre_flush.height,
                base_height: viewport_floor,
                committed_rows: predicted_committed_rows,
                elapsed_ms: elapsed_ms,
            },
        );
        if predicted_desired_height > 0
            && predicted_desired_height < state.inline_runtime.current_viewport_height
        {
            let expected_insert_height = measure_inline_commit_batches_for_prefix(
                app,
                theme,
                &state.inline_runtime,
                pinned_live_prefix,
                terminal_area_pre_flush.width,
            );
            let bounded_desired_height = inline_commit_prelude_shrink_height(
                state.inline_runtime.current_viewport_height,
                predicted_desired_height,
                expected_insert_height,
            );
            if bounded_desired_height < state.inline_runtime.current_viewport_height {
                guard
                    .terminal()
                    .shrink_inline_viewport_keeping_top_for_insert_before(
                        bounded_desired_height,
                        expected_insert_height,
                    )
                    .context("shrink inline viewport before committed-row insert")?;
                state.inline_runtime.current_viewport_height = bounded_desired_height;
            }
        }
    }
    Ok(())
}
/// Step 2: reconcile the inline viewport against the real post-flush committed
/// count, holding the streaming high-water mark instead of chasing dips.
#[allow(clippy::too_many_arguments)]
fn inline_reconcile_viewport(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    state: &mut LoopState,
    terminal_area_for_resize: ratatui::layout::Size,
    viewport_floor: u16,
    committed_rows: usize,
    elapsed_ms: u64,
) -> anyhow::Result<()> {
    // Reconcile against the *real* post-flush committed count.
    // Grows (more live content or a picker opening) are applied here;
    // so is any small residual from the pre-flush estimate. For
    // non-fullscreen commit-less shrinks (picker/queue/overlay closing),
    // keep the viewport top fixed and release rows below; otherwise every
    // open/close cycle at the screen bottom pushes the prompt down and
    // leaves another blank band above it. Full-height inline surfaces still
    // use `set_viewport_height` so closing them returns the UI to the bottom.
    let desired_viewport_height = render::desired_inline_viewport_height(
        app,
        theme,
        render::InlineViewportHeightInput {
            width: terminal_area_for_resize.width,
            terminal_height: terminal_area_for_resize.height,
            base_height: viewport_floor,
            committed_rows,
            elapsed_ms: elapsed_ms,
        },
    );
    if desired_viewport_height != state.inline_runtime.current_viewport_height
        && desired_viewport_height > 0
    {
        let shrinking = desired_viewport_height < state.inline_runtime.current_viewport_height;
        let current_is_full_height =
            state.inline_runtime.current_viewport_height >= terminal_area_for_resize.height;
        // Held only while the drop looks like a dip. A drop that is both large
        // and durable is a long command's output tail retreating, not a dip, and
        // holding the peak for it strands the rest of the turn behind a
        // screen-tall blank band — so the release below drops through to the
        // same shrink the idle reconcile would have run at end of turn.
        let hold_streaming_high_water = shrinking
            && app.is_loading
            && !state
                .inline_runtime
                .streaming_shrink_release(desired_viewport_height, Instant::now());
        if !hold_streaming_high_water {
            state.inline_runtime.clear_streaming_slack();
        }
        if hold_streaming_high_water {
            // While the request is still streaming, never chase a measured-height
            // *drop* by shrinking the viewport — at ANY height, not only once it
            // has filled to the terminal bottom. The commit-driven shrink that
            // pairs with `insert_before` already ran *before* the flush
            // (`shrink_inline_viewport_keeping_top_for_insert_before` above), so
            // every drop that reaches here is a transient measurement dip: a
            // closed thinking/tool block draining into scrollback, a tool card
            // collapsing, an animation row dropping. Shrinking for it moves the
            // prompt's bottom anchor up and the next streamed delta grows it right
            // back — the "scroll down then repaint" bottom-edge wobble.
            //
            // This guard used to be gated on `current_is_full_height`. The engine
            // fix that closes the thinking stream on a tool/image/search block
            // start (not just on text) made closed prefix blocks drain — and the
            // measured live tail therefore shrink — at *every tool boundary* of a
            // reasoning turn, well below full height, so the wobble surfaced at
            // mid heights the full-height-only guard never covered. Hold the
            // viewport at its streaming high-water mark; the idle reconcile (this
            // same block with `app.is_loading == false`) shrinks it to fit once
            // the turn ends.
            //
            // The hold is no longer unconditional: `streaming_shrink_release`
            // above lets go once the unused rows are both many and durable —
            // the shape a `cargo test`/`cargo build` tail leaves behind after
            // its rows commit to scrollback, which the peak-until-end-of-turn
            // rule otherwise turned into a screen-tall blank band for the rest
            // of the turn.
        } else if shrinking && !current_is_full_height {
            guard
                .terminal()
                .shrink_inline_viewport_keeping_top(desired_viewport_height)
                .context("shrink inline live viewport")?;
            state.inline_runtime.current_viewport_height = desired_viewport_height;
        } else {
            guard
                .terminal()
                .set_viewport_height(desired_viewport_height)
                .context("resize inline live viewport")?;
            state.inline_runtime.current_viewport_height = desired_viewport_height;
        }
    }
    Ok(())
}
/// Takes the next terminal event: a stashed one, one already queued, or one
/// polled with the timeout the current surface deserves. `None` means the poll
/// timed out and the pass is over.
fn read_next_event(
    app: &mut AppState,
    state: &mut LoopState,
    stashed_or_queued: bool,
) -> anyhow::Result<Option<Event>> {
    // ── Read the next event ─────────────────────────────────
    // Use a shorter poll timeout when a paste burst is actively
    // buffering so `flush_if_idle` at the top of the next
    // iteration fires close to `BURST_IDLE_TIMEOUT` after the
    // last burst char instead of up to 50 ms later. This keeps
    // the post-paste flush latency down to ~60–75 ms on Windows.
    let evt = if let Some(stashed) = state.stashed_events.pop_front() {
        stashed
    } else if stashed_or_queued {
        // poll(0) above already confirmed an event is queued;
        // read it directly without re-polling. This is the
        // tight inner cycle of the input fast path.
        read_crossterm_event()?
    } else {
        let poll_timeout = if state.paste_burst.has_pending() {
            Duration::from_millis(5)
        } else if app.paste_echo.is_some() {
            // Armed echo suppressor: poll fast enough that the
            // idle finalize (and any shortfall repair) lands
            // promptly after the echo stream stops.
            Duration::from_millis(25)
        } else if app.custom_status_line.in_flight {
            Duration::from_millis(25)
        } else if app
            .resume_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_loading_more())
        {
            // Session discovery ships its rows in batches; the idle
            // 250 ms poll would hold each one back for up to a quarter
            // second, which reads as the picker stalling.
            Duration::from_millis(25)
        } else if state.active_prompt.is_some()
            || !app.rebon_tui.overlay.is_empty()
            // A compaction has no turn and no overlay, so the idle 250ms
            // poll would advance its progress bar and elapsed clock four
            // times a second — visibly steppy for the ~30s it runs.
            || app.compact_run.is_some()
        {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(250)
        };
        if !poll_crossterm_event(poll_timeout)? {
            return Ok(None);
        }
        read_crossterm_event()?
    };
    state
        .input_probe
        .note_input(state.paste_burst.has_pending(), app.paste_echo.is_some());
    Ok(Some(evt))
}
// Bracketed paste: crossterm delivers pasted text (possibly
// multi-line) as a single Event::Paste(String) instead of a
// stream of individual Key events. Route through
// apply_text_paste (pure logic in rebon_tui::promptinput) which
// owns the full splice + id-allocation dance — we just hand
// it a snapshot of our state and write back the result.
//
// If we happen to be mid-burst-buffer when a bracketed paste
// arrives, flush the buffer first so the two pastes stay in
// the right order.
fn handle_bracketed_paste(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    state: &mut LoopState,
    text: String,
) -> anyhow::Result<()> {
    state.new_session_hint_idle_since = Instant::now();

    // ── Adopted-paste echo suppression ──────────────────
    // When a clipboard adoption is armed, this chunk is
    // (part of) the terminal's replay of text we already
    // applied. Match it against the expected remainder and
    // drop it instead of paying the blocking coalesce +
    // re-apply cost. The event loop keeps cycling between
    // chunks, so rendering stays live while the echo drains.
    if app.paste_echo.is_some() {
        let chunk_norm = normalize_pasted_text(&text);
        let outcome = app
            .paste_echo
            .as_mut()
            .expect("paste_echo checked is_some above")
            .consume_paste_chunk(&chunk_norm, Instant::now());
        match outcome {
            EchoConsume::Swallowed => {
                tracing::debug!(chunk_len = text.len(), "paste: echo chunk swallowed");
                return Ok(());
            }
            EchoConsume::Done => {
                tracing::debug!(
                    chunk_len = text.len(),
                    "paste: adopted paste fully confirmed by terminal echo"
                );
                app.paste_echo = None;
                return Ok(());
            }
            EchoConsume::Diverged { tail_norm } => {
                tracing::debug!(
                    tail_len = tail_norm.len(),
                    "paste: echo diverged from adopted text; finalizing"
                );
                finalize_paste_echo(app, "diverged");
                // The divergent tail is genuinely-new pasted
                // input; route it through the normal flush —
                // the chip-merge grace attaches it to the
                // adopted chip when the prompt tail still
                // ends with that chip's reference.
                flush_burst_with_merge(app, tail_norm, guard, &mut state.last_paste_flush_at);
                return Ok(());
            }
        }
    }

    let initial_len = text.len();
    let initial_lines = text.lines().count();
    tracing::debug!(
        len = initial_len,
        lines = initial_lines,
        tail = %summarize_prompt_tail(&text, 16),
        "paste: Event::Paste received before coalesce"
    );
    let mut combined = text;

    // ── Clipboard adoption fast path ────────────────────
    // The terminal streams pastes at its own pace; waiting
    // for the stream (coalesce) is what makes large pastes
    // feel slow. If the OS clipboard confirms what this
    // stream is going to deliver, act on the clipboard NOW:
    // * chunk == clipboard → the paste is already complete;
    //   skip the blocking coalesce grace entirely.
    // * chunk is a long proper prefix of the clipboard →
    //   apply the full clipboard immediately (chip appears
    //   now) and arm the echo suppressor to drop the rest
    //   of the stream as it trickles in.
    // Clipboard unreadable or mismatched (e.g. over SSH)
    // → fall through to the legacy coalesce path.
    let main_prompt_paste = is_main_prompt_paste_target(app);
    let mut skip_coalesce = false;
    if main_prompt_paste && paste_echo_enabled() && !state.paste_burst.has_pending() {
        match try_adopt_clipboard_for_first_chunk(&combined) {
            ClipboardAdoption::Complete => {
                skip_coalesce = true;
                tracing::debug!(
                    len = combined.len(),
                    "paste: chunk equals clipboard; skipping coalesce grace"
                );
            }
            ClipboardAdoption::Adopt {
                clipboard_raw,
                clipboard_norm,
                matched,
            } => {
                tracing::info!(
                    chunk_len = combined.len(),
                    clipboard_len = clipboard_raw.len(),
                    "paste: adopting clipboard for split paste stream"
                );
                let input_before = app.input.clone();
                let cursor_before = app.cursor_offset;
                let text_chips_before = snapshot_text_chips(app);
                flush_burst_with_merge(app, clipboard_raw, guard, &mut state.last_paste_flush_at);
                let repair_target = find_adopted_text_repair_target(
                    app,
                    &text_chips_before,
                    &input_before,
                    cursor_before,
                    &clipboard_norm,
                );
                app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
                    clipboard_norm,
                    matched,
                    repair_target,
                    Instant::now(),
                ));
                return Ok(());
            }
            ClipboardAdoption::None => {}
        }
    }

    // Coalesce: large pastes can be split by conpty into
    // multiple Event::Paste deliveries. Treat them as one.
    if !skip_coalesce {
        coalesce_adjacent_pastes(&mut combined, &mut state.stashed_events, &mut state.source)?;
    }
    tracing::debug!(
        len = combined.len(),
        lines = combined.lines().count(),
        added_len = combined.len().saturating_sub(initial_len),
        tail = %summarize_prompt_tail(&combined, 16),
        stashed_after_coalesce = !state.stashed_events.is_empty(),
        "paste: Event::Paste ready after coalesce"
    );
    if let Some(dialog) = app.onboarding_dialog.as_mut() {
        let handled = dialog.handle_paste(&combined);
        tracing::debug!(
            len = combined.len(),
            handled,
            "paste: routed Event::Paste to onboarding dialog"
        );
        return Ok(());
    }
    if app.has_modal_overlay()
        && !app
            .agent_view
            .as_ref()
            .is_some_and(|view| view.is_input_focused())
    {
        tracing::debug!(
            len = combined.len(),
            lines = combined.lines().count(),
            "paste: dropping Event::Paste because modal overlay is active"
        );
        return Ok(());
    }
    if app
        .agent_view
        .as_ref()
        .is_some_and(|view| view.is_input_focused())
    {
        if let Some(image) =
            crate::tui::clipboard_image::read_image_file_from_pasted_text(&combined)
        {
            if let Some(view) = app.agent_view.as_mut() {
                view.paste_image(image);
            }
        } else if let Some(view) = app.agent_view.as_mut() {
            view.paste_text(&combined);
        }
        return Ok(());
    }
    if let Some(pending) = state.paste_burst.force_flush() {
        let (pending_len, pending_lines, pending_tail) = summarize_paste_payload(&pending);
        tracing::debug!(
            pending_len,
            pending_lines,
            pending_tail = %pending_tail,
            "paste: forcing pending non-bracketed burst flush before bracketed paste"
        );
        flush_burst_with_merge(app, pending, guard, &mut state.last_paste_flush_at);
    }
    // Route the bracketed body through the same merge-aware
    // flusher so a bracketed paste arriving right after a
    // non-bracketed burst flush coalesces into one chip.
    flush_burst_with_merge(app, combined, guard, &mut state.last_paste_flush_at);
    Ok(())
}
// ── Mouse event routing ─────────────────────────────────
// DEC 1000+1002+1006 mouse tracking is enabled in terminal.rs.
// Route wheel events to scroll, click/drag to in-app selection.
fn handle_mouse_input(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    slot: &mut SessionSlot,
    handle: &Handle,
    state: &mut LoopState,
    mouse: event::MouseEvent,
) -> anyhow::Result<()> {
    state.new_session_hint_idle_since = Instant::now();
    if maybe_handle_resume_dialog_mouse(app, mouse) {
        return Ok(());
    }
    if let Some(session) = slot.session_mut() {
        if maybe_handle_agent_view_mouse(app, session, handle, &mut state.active_prompt, mouse) {
            return Ok(());
        }
    }
    let size = guard
        .terminal()
        .size()
        .context("query terminal size for mouse input routing")?;
    let transcript = mouse_transcript_area(app, size);
    let prompt = mouse_prompt_area(app, size);
    handle_mouse_event(app, mouse, transcript, prompt);
    Ok(())
}
// ── Adopted-paste echo: key-event stragglers ─────────────
// conpty occasionally delivers paste-body characters as raw
// Key events at chunk boundaries. While a stream-adoption
// suppressor is armed, plain text keys that continue the
// expected echo are swallowed; anything else finalizes the
// suppressor (repairing an over-adopted chip if the stream
// fell short) and then processes normally.
fn swallow_paste_echo_key(
    app: &mut AppState,
    state: &mut LoopState,
    evt: &Event,
) -> anyhow::Result<bool> {
    if app.paste_echo.is_some() {
        if let Event::Key(key) = evt {
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && paste_echo_key_hook(app, key) == KeyEchoHook::Swallow
            {
                // conhost replays a raw-key paste as two events per
                // pasted character. Matching one per outer-loop
                // iteration keeps the prompt frozen for the whole
                // replay even though the chip is already on screen, so
                // swallow the rest of the queued echo right here.
                let drained =
                    drain_paste_echo_keys(app, &mut state.stashed_events, &mut state.source)?;
                if drained > 0 {
                    tracing::debug!(drained, "paste: drained queued echo key stragglers");
                }
                return Ok(true);
            }
        }
    }
    Ok(false)
}
/// Offers the key to every surface that can swallow it: the permission modal
/// and its scroll whitelist, the dialogs, the agents view, the pickers, and the
/// two paste-burst shortcuts that must run before translation.
fn route_key_to_surfaces(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &mut SessionSlot,
    handle: &Handle,
    state: &mut LoopState,
    key: event::KeyEvent,
) -> anyhow::Result<PhaseFlow> {
    // A pending permission view (e.g. a long ExitPlanMode plan) renders as
    // a scrollable suffix below the transcript. The permission modal
    // consumes every key (maybe_handle_permission_key always returns true
    // while pending), so divert the scroll whitelist BEFORE it — otherwise
    // a plan taller than the viewport can never be scrolled into view.
    if state.pending_permission.is_some()
        && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
    {
        if let Some(action) = permission_suffix_scroll_action(&key, app) {
            let size = guard
                .terminal()
                .size()
                .context("query terminal size for permission scroll routing")?;
            let area = mouse_transcript_area(app, size);
            handle_scroll_action(app, action, area);
            return Ok(PhaseFlow::NextIteration);
        }
    }

    if let Some(session) = slot.session_mut() {
        if maybe_handle_permission_key(
            app,
            session,
            &mut state.pending_permission,
            &key,
            &session.engine_half.live_policy_store,
            &session.cwd,
            state.active_prompt.is_some(),
        ) {
            return Ok(PhaseFlow::NextIteration);
        }
    }

    if let Some(session) = slot.session() {
        if maybe_handle_teams_dialog_key(app, session.engine_half.tasks.as_ref(), &key) {
            return Ok(PhaseFlow::NextIteration);
        }
    }

    if let Some(session) = slot.session_mut() {
        if maybe_handle_agent_view_key(app, session, handle, &mut state.active_prompt, &key, guard)
        {
            return Ok(PhaseFlow::NextIteration);
        }

        if should_detach_attached_session_on_ctrl_z(app, session, &key) {
            detach_current_session_to_agent_view(app, session, &mut state.active_prompt);
            return Ok(PhaseFlow::NextIteration);
        }
    }

    if is_agent_view_left_press_candidate(app, &key) {
        let now = Instant::now();
        let is_double_left = state
            .last_agent_view_left_press_at
            .is_some_and(|last| now.saturating_duration_since(last) <= Duration::from_millis(400));
        state.last_agent_view_left_press_at = Some(now);
        if is_double_left {
            state.last_agent_view_left_press_at = None;
            let this_terminal_job_id = slot
                .session()
                .and_then(|session| session.attached_background_job_id.clone());
            open_agent_view(app, this_terminal_job_id.as_deref());
            return Ok(PhaseFlow::NextIteration);
        }
    } else if !matches!(key.kind, KeyEventKind::Release) {
        state.last_agent_view_left_press_at = None;
    }

    if let Some(session) = slot.session() {
        if maybe_handle_background_tasks_dialog_key(
            app,
            session.engine_half.tasks.as_ref(),
            &key,
            &mut state.active_prompt,
        ) {
            return Ok(PhaseFlow::NextIteration);
        }
    }

    if maybe_handle_dialog_shortcut(app, std::path::Path::new(slot.cwd()), &key) {
        return Ok(PhaseFlow::NextIteration);
    }

    let cwd_snapshot = slot.cwd().to_string();
    if let Some(session) = slot.session_mut() {
        if maybe_handle_active_dialog_key(
            app,
            session,
            handle,
            PendingWork {
                active_prompt: &mut state.active_prompt,
                agent_generation: &mut state.agent_gen_rx,
            },
            Path::new(&cwd_snapshot),
            &key,
            theme,
            guard,
        ) {
            return Ok(PhaseFlow::NextIteration);
        }
    }

    if let Some(result) = crate::tui::slash_picker::handle_key(&mut app.slash_picker, &key) {
        use crate::tui::slash_picker::PickerKeyResult;
        match result {
            PickerKeyResult::Selected { input, cursor } => {
                if selected_slash_command_needs_input(app, &input) {
                    app.input = input;
                    app.cursor_offset = cursor;
                } else {
                    let text = input.trim_end().to_string();
                    app.input = text.clone();
                    app.cursor_offset = app.input.len();
                    match slot.session_mut() {
                        Some(session) => {
                            if submit_or_queue(
                                app,
                                text,
                                session,
                                handle,
                                &mut state.active_prompt,
                                &mut state.pending_permission,
                                state.ui_mode,
                            ) {
                                return Ok(PhaseFlow::Exit(EventLoopOutcome::Exit));
                            }
                        }
                        None => {
                            super::startup_submit::submit_before_session(app, text, slot);
                        }
                    }
                }
            }
            PickerKeyResult::Completed { input, cursor } => {
                app.input = input;
                app.cursor_offset = cursor;
            }
            PickerKeyResult::Consumed | PickerKeyResult::Closed => {}
        }
        return Ok(PhaseFlow::NextIteration);
    }

    if let Some(result) =
        crate::tui::at_mention_picker::handle_key(&mut app.at_mention_picker, &key)
    {
        use crate::tui::at_mention_picker::MentionKeyResult;
        match result {
            MentionKeyResult::Selected {
                replacement,
                token_start,
            }
            | MentionKeyResult::Completed {
                replacement,
                token_start,
            } => match crate::tui::at_mention_picker::apply_replacement(
                &app.input,
                app.cursor_offset,
                token_start,
                &replacement,
            ) {
                Ok((input, cursor)) => {
                    app.input = input;
                    app.cursor_offset = cursor;
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        input_len = app.input.len(),
                        cursor_offset = app.cursor_offset,
                        cached_token_start = token_start,
                        "ignored stale @ mention completion"
                    );
                }
            },
            MentionKeyResult::Consumed | MentionKeyResult::Closed => {}
        }
        return Ok(PhaseFlow::NextIteration);
    }

    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && state.paste_burst.has_pending()
        && matches!(key.code, KeyCode::Char('?') | KeyCode::Char('!'))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        let KeyCode::Char(ch) = key.code else {
            unreachable!();
        };
        state.paste_burst.append_char_to_active(ch, Instant::now());
        drain_burst_queue(
            &mut state.paste_burst,
            &mut state.stashed_events,
            &mut state.source,
        )?;
        return Ok(PhaseFlow::NextIteration);
    }

    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && app.input.is_empty()
        && app.cursor_offset == 0
        && matches!(key.code, KeyCode::Char('?') | KeyCode::Char('!'))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        && detect_paste_batch_after_mode_shortcut(&mut state.stashed_events, &mut state.source)?
    {
        let KeyCode::Char(ch) = key.code else {
            unreachable!();
        };
        state
            .paste_burst
            .batch_activate_with_char(ch, Instant::now());
        drain_burst_queue(
            &mut state.paste_burst,
            &mut state.stashed_events,
            &mut state.source,
        )?;
        return Ok(PhaseFlow::NextIteration);
    }
    Ok(PhaseFlow::Continue)
}
/// Translates the key, then reroutes a bare Down onto the transcript scroll
/// engine when the prompt has nothing to do with it.
fn reroute_prompt_down_to_scroll(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    key: event::KeyEvent,
) -> anyhow::Result<KeyAction> {
    let action = translate_key(&key, app);

    // Keyboard Down → scroll rerouting. When no footer item is
    // available, an empty prompt, no picker, and real scrollback
    // means bare Down should scroll the transcript. If footer pills
    // are visible, keep Down in the prompt path so it can focus the
    // first pill instead.
    let action = match action {
        KeyAction::PromptDown
            if app.input.is_empty()
                && app.slash_picker.is_none()
                && app.at_mention_picker.is_none()
                && derive_footer_items(app).is_empty() =>
        {
            let size = guard
                .terminal()
                .size()
                .context("query terminal size for scroll routing")?;
            let area = transcript_area(app, size);
            if should_reroute_prompt_down_to_scroll(app, area) {
                let now_ms = wall_clock_ms();
                if should_double_down_repin_transcript(app, now_ms) {
                    KeyAction::ScrollEnd
                } else {
                    KeyAction::ScrollDown
                }
            } else {
                action
            }
        }
        other => other,
    };

    let action = match action {
        KeyAction::PromptDown
            if should_non_empty_prompt_down_press_repin_transcript(
                app,
                matches!(key.kind, KeyEventKind::Press),
                wall_clock_ms(),
            ) =>
        {
            KeyAction::ScrollEnd
        }
        other => other,
    };
    Ok(action)
}
fn route_action_through_paste_burst(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    state: &mut LoopState,
    key: event::KeyEvent,
    action: KeyAction,
) -> anyhow::Result<Option<KeyAction>> {
    // ── Paste burst routing ─────────────────────────────────
    // Feed the decoded KeyAction through the time-based
    // `PasteBurst` detector before handing it to
    // `handle_key_action`. See the `PasteBurst` docstring for
    // why the old `poll_crossterm_event(2ms)` queue peek was replaced:
    // Windows timer granularity made it unreliable, so pasted
    // newlines escaped into `submit_or_queue` and each line
    // became a separate queued prompt.
    //
    // - Plain char: if activated, divert into the buffer
    //   instead of the prompt. If the burst just ended on a
    //   slow char, flush the buffer first and then insert the
    //   char normally.
    // - Submit (Enter): if the burst is active, append `\n`
    //   to the buffer and swallow the submit. If the burst
    //   ended, flush first, then submit against the
    //   post-flush `app.input`.
    // - Anything else: a force-flush so cursor moves, chip
    //   backspace, scroll, etc. never land on a half-buffered
    //   prompt.
    let burst_now = Instant::now();
    let action = match action {
        KeyAction::TextEdit(edit) => {
            match route_text_edit_through_burst(guard, app, state, key, edit, burst_now)? {
                Some(action) => action,
                None => return Ok(None),
            }
        }
        KeyAction::Submit(text) => match route_submit_through_burst(app, state, text, burst_now)? {
            Some(action) => action,
            None => return Ok(None),
        },
        // Release events and modifier-only keys translate to
        // Ignored. They must NOT touch the burst state —
        // otherwise every Windows Press+Release pair resets
        // `last_event_at` and the consecutive-fast counter
        // never reaches the activation threshold.
        KeyAction::Ignored => action,
        other => {
            if let Some(flushed) = state.paste_burst.force_flush() {
                flush_burst_with_merge(app, flushed, guard, &mut state.last_paste_flush_at);
            }
            other
        }
    };
    Ok(Some(action))
}
/// The `TextEdit` arm of the burst router. `None` means the edit was diverted
/// into the burst buffer and the pass is over.
fn route_text_edit_through_burst(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    state: &mut LoopState,
    key: event::KeyEvent,
    edit: crate::tui::event::TextEdit,
    burst_now: Instant,
) -> anyhow::Result<Option<KeyAction>> {
    let action = if let Some(ch) = plain_char_from_edit(&edit) {
        let outcome = state.paste_burst.on_char(ch, burst_now);
        match &outcome {
            CharOutcome::FlushThenPassThrough(flushed) => {
                let (flush_len, flush_lines, flush_tail) = summarize_paste_payload(flushed);
                tracing::debug!(
                    ch = %ch,
                    outcome = "FlushThenPassThrough",
                    burst_active = state.paste_burst.has_pending(),
                    flushed_len = flush_len,
                    flushed_lines = flush_lines,
                    flushed_tail = %flush_tail,
                    "paste: burst on_char"
                );
            }
            _ => {
                tracing::debug!(
                    ch = %ch,
                    outcome = ?outcome,
                    burst_active = state.paste_burst.has_pending(),
                    "paste: burst on_char"
                );
            }
        }
        match outcome {
            CharOutcome::PassThrough => {
                // Timing said "not a paste". Queue depth may
                // still activate after a strict-speed signal.
                // A clipboard-backed image path is the narrow
                // exception: confirming three stream characters
                // keeps a provisional path prefix out of the
                // rendered prompt.
                let candidate_chars = state.paste_burst.candidate_chars();
                let consecutive_fast = state.paste_burst.consecutive_fast();
                let generic_batch_ready = consecutive_fast >= BURST_BATCH_FAST_COUNT;
                let image_path_probe_ready = candidate_chars >= 2
                    && paste_echo_enabled()
                    && is_main_prompt_paste_target(app)
                    && !state.paste_burst.image_path_probe_attempted();
                let batch_detected = (generic_batch_ready || image_path_probe_ready)
                    && detect_paste_batch(&mut state.stashed_events, &mut state.source)?;
                let image_path_batch = if batch_detected
                    && !generic_batch_ready
                    && image_path_probe_ready
                {
                    state.paste_burst.mark_image_path_probe_attempted();
                    queued_paste_char(&state.stashed_events).is_some_and(|queued_ch| {
                        let prefix =
                            paste_candidate_prefix_at_cursor(app, candidate_chars, ch, queued_ch);
                        clipboard_image_path_matches_prefix(&prefix)
                    })
                } else {
                    false
                };
                if batch_detected && (generic_batch_ready || image_path_batch) {
                    state.paste_burst.batch_activate_with_char(ch, burst_now);
                    let retro_chars = candidate_chars.saturating_sub(1);
                    if retro_chars > 0 {
                        let grabbed = retro_grab_at_cursor(app, retro_chars);
                        state.paste_burst.prepend_retro(&grabbed);
                    }
                    drain_burst_queue(
                        &mut state.paste_burst,
                        &mut state.stashed_events,
                        &mut state.source,
                    )?;
                    return Ok(None);
                }
                KeyAction::TextEdit(edit)
            }
            CharOutcome::Buffered => {
                // Fast path: drain every queued pasted char
                // directly into the burst buffer without
                // re-entering the outer loop's sync work.
                // This is what makes non-bracketed paste
                // feel instant on Windows — see
                // `drain_burst_queue`.
                drain_burst_queue(
                    &mut state.paste_burst,
                    &mut state.stashed_events,
                    &mut state.source,
                )?;
                return Ok(None);
            }
            CharOutcome::ActivatedRetro { retro_chars } => {
                // Retro-grab: lift the just-inserted prefix
                // out of the prompt at the cursor and
                // prepend it to the burst buffer so the
                // eventual flush covers the full pasted
                // text. Cursor-relative — content after the
                // cursor is preserved in place.
                let grabbed = retro_grab_at_cursor(app, retro_chars as usize);
                state.paste_burst.prepend_retro(&grabbed);
                // Burst just activated — the rest of the
                // pasted stream is almost certainly already
                // queued. Drain it in a tight loop.
                drain_burst_queue(
                    &mut state.paste_burst,
                    &mut state.stashed_events,
                    &mut state.source,
                )?;
                return Ok(None);
            }
            CharOutcome::FlushThenPassThrough(flushed) => {
                flush_burst_with_merge(app, flushed, guard, &mut state.last_paste_flush_at);
                // Recompute the edit against the
                // post-flush prompt/cursor so the
                // `next_value` / `intended_cursor`
                // snapshot isn't stale.
                translate_key(&key, app)
            }
        }
    } else if edit.event_input.return_key {
        // Shift/Alt/Ctrl+Enter that translates to a literal
        // `\n` edit (not a Submit). Zed's built-in terminal
        // on Windows delivers every pasted newline as
        // Ctrl+Enter, so this path is the primary
        // paste-multiline signal there. Run the same
        // paste-batch detection as the Submit arm below —
        // retro-grab the first pasted line, activate the
        // burst with a newline, and drain the rest.
        let pre_enter_fast = state.paste_burst.consecutive_fast();
        let pre_enter_candidate = state.paste_burst.candidate_chars();
        let enter_outcome = state.paste_burst.on_enter(burst_now);
        tracing::debug!(
            outcome = ?enter_outcome,
            burst_active = state.paste_burst.has_pending(),
            pre_enter_fast,
            pre_enter_candidate,
            "paste: burst on_enter (TextEdit return_key)"
        );
        match enter_outcome {
            EnterOutcome::Submit => {
                if detect_paste_batch_after_enter(&mut state.stashed_events, &mut state.source)? {
                    let retro_chars = pre_enter_candidate.max(pre_enter_fast as usize);
                    let grabbed = if retro_chars > 0 {
                        retro_grab_at_cursor(app, retro_chars)
                    } else {
                        String::new()
                    };
                    state
                        .paste_burst
                        .force_activate_with_newline(&grabbed, burst_now);
                    drain_burst_queue(
                        &mut state.paste_burst,
                        &mut state.stashed_events,
                        &mut state.source,
                    )?;
                    return Ok(None);
                }
                // Not a paste — fall through to the normal
                // \n-insertion path so the edit lands in
                // the prompt as a literal newline.
                KeyAction::TextEdit(edit)
            }
            EnterOutcome::Buffered => {
                drain_burst_queue(
                    &mut state.paste_burst,
                    &mut state.stashed_events,
                    &mut state.source,
                )?;
                return Ok(None);
            }
            EnterOutcome::ActivatedRetro { retro_chars } => {
                let grabbed = retro_grab_at_cursor(app, retro_chars as usize);
                state.paste_burst.prepend_retro(&grabbed);
                drain_burst_queue(
                    &mut state.paste_burst,
                    &mut state.stashed_events,
                    &mut state.source,
                )?;
                return Ok(None);
            }
        }
    } else {
        if let Some(flushed) = state.paste_burst.force_flush() {
            flush_burst_with_merge(app, flushed, guard, &mut state.last_paste_flush_at);
        }
        KeyAction::TextEdit(edit)
    };
    Ok(Some(action))
}
/// The `Submit` arm of the burst router. `None` means the Enter was a pasted
/// newline and went into the burst buffer instead.
fn route_submit_through_burst(
    app: &mut AppState,
    state: &mut LoopState,
    text: String,
    burst_now: Instant,
) -> anyhow::Result<Option<KeyAction>> {
    let action = {
        // Snapshot the strict consecutive-fast counter *before*
        // calling on_enter. on_enter resets it on the slow
        // (Submit) path, so we have to capture the count here
        // to know how much of the current prompt belongs to the
        // paste stream if the queue-depth signal flips the
        // Enter into a pasted newline below.
        let pre_enter_fast = state.paste_burst.consecutive_fast();
        let pre_enter_candidate = state.paste_burst.candidate_chars();
        let enter_outcome = state.paste_burst.on_enter(burst_now);
        tracing::debug!(
            outcome = ?enter_outcome,
            burst_active = state.paste_burst.has_pending(),
            text_len = text.len(),
            pre_enter_fast,
            pre_enter_candidate,
            "paste: burst on_enter (Submit)"
        );
        match enter_outcome {
            EnterOutcome::Submit => {
                // Timing-based on_enter said "regular Submit",
                // but a multi-line non-bracketed paste whose
                // first line is too short to satisfy
                // `consecutive_fast >= 2` (e.g. `"a\nb\nc"`)
                // also lands here. Use `detect_paste_batch` —
                // which correctly consumes the matched
                // `KeyEventKind::Release` for the Enter key
                // before re-checking the queue — to distinguish
                // a real Submit (queue empties after Release is
                // drained) from a pasted newline (more events
                // still queued behind it).
                //
                // The earlier naive `poll_crossterm_event(Duration::ZERO)`
                // here misfired on every Windows Submit because
                // the matching Release was always queued. With
                // Release-aware detection the false-positive is
                // gone and short-first-line pastes activate
                // properly instead of splitting into N submits.
                if detect_paste_batch_after_enter(&mut state.stashed_events, &mut state.source)? {
                    // Lift the first pasted line out of the prompt.
                    // Use the lenient candidate length, not only the
                    // strict fast counter: Windows can stretch chars
                    // in the first line beyond BURST_CHAR_INTERVAL,
                    // and grabbing only the last fast char splits the
                    // paste chip and looks like content was lost.
                    let retro_chars = pre_enter_candidate.max(pre_enter_fast as usize);
                    let grabbed = if retro_chars > 0 {
                        retro_grab_at_cursor(app, retro_chars)
                    } else {
                        String::new()
                    };
                    state
                        .paste_burst
                        .force_activate_with_newline(&grabbed, burst_now);
                    drain_burst_queue(
                        &mut state.paste_burst,
                        &mut state.stashed_events,
                        &mut state.source,
                    )?;
                    return Ok(None);
                }
                KeyAction::Submit(text)
            }
            EnterOutcome::Buffered => {
                // Same fast-drain path as a buffered char — a
                // pasted newline is almost always followed by
                // more queued paste chars/newlines. If the
                // Enter actually belonged to the user (they
                // pressed it within `BURST_IDLE_TIMEOUT` of the
                // last paste char), the buffer will flush as
                // a paste chip via `flush_if_idle` and the
                // user presses Enter again to submit.
                drain_burst_queue(
                    &mut state.paste_burst,
                    &mut state.stashed_events,
                    &mut state.source,
                )?;
                return Ok(None);
            }
            EnterOutcome::ActivatedRetro { retro_chars } => {
                // Same cursor-relative retro-grab as the
                // CharOutcome path: lift the just-inserted
                // chars out at the cursor and prepend them to
                // the burst buffer.
                let grabbed = retro_grab_at_cursor(app, retro_chars as usize);
                state.paste_burst.prepend_retro(&grabbed);
                drain_burst_queue(
                    &mut state.paste_burst,
                    &mut state.stashed_events,
                    &mut state.source,
                )?;
                return Ok(None);
            }
        }
    };
    Ok(Some(action))
}
/// Handles a scroll action and batch-drains the queued scroll events behind it.
/// `None` means the action was a scroll and the pass is over.
fn drain_batched_scroll(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    state: &mut LoopState,
    action: KeyAction,
) -> anyhow::Result<Option<KeyAction>> {
    match action {
        KeyAction::ScrollUp
        | KeyAction::ScrollDown
        | KeyAction::PageUp
        | KeyAction::PageDown
        | KeyAction::ScrollHome
        | KeyAction::ScrollEnd => {
            let size = guard
                .terminal()
                .size()
                .context("query terminal size for scroll routing")?;
            let area = transcript_area(app, size);
            handle_scroll_action(app, action, area);

            // Batch-drain queued scroll events to prevent event
            // queue backup. On Windows, key repeat generates
            // Press+Release pairs at ~5ms intervals (~200 events/s),
            // but the main loop renders between every event
            // (~50ms/frame). Without batching, the queue grows by
            // ~9 events/frame and scrolling appears to freeze.
            while poll_crossterm_event(Duration::ZERO)? {
                let inner_evt = read_crossterm_event()?;
                // Mouse scroll events can also batch up; route
                // them through the same handler inline.
                if let Event::Mouse(mouse) = &inner_evt {
                    match mouse.kind {
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            let prompt = mouse_prompt_area(
                                app,
                                guard
                                    .terminal()
                                    .size()
                                    .context("query terminal size for batched mouse scroll")?,
                            );
                            handle_mouse_event(app, *mouse, area, prompt);
                            continue;
                        }
                        _ => {
                            stash_event_front(&mut state.stashed_events, inner_evt);
                            break;
                        }
                    }
                }
                let Event::Key(inner_key) = &inner_evt else {
                    stash_event_front(&mut state.stashed_events, inner_evt);
                    break;
                };
                if !matches!(inner_key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    // Release events — skip without stashing.
                    continue;
                }
                let inner_action = translate_key(inner_key, app);
                let inner_action = match inner_action {
                    KeyAction::PromptDown if should_reroute_prompt_down_to_scroll(app, area) => {
                        let now_ms = wall_clock_ms();
                        if should_double_down_repin_transcript(app, now_ms) {
                            KeyAction::ScrollEnd
                        } else {
                            KeyAction::ScrollDown
                        }
                    }
                    other => other,
                };
                match inner_action {
                    KeyAction::ScrollUp
                    | KeyAction::ScrollDown
                    | KeyAction::PageUp
                    | KeyAction::PageDown
                    | KeyAction::ScrollHome
                    | KeyAction::ScrollEnd => {
                        handle_scroll_action(app, inner_action, area);
                    }
                    _ => {
                        // Non-scroll event — stash for next
                        // iteration so it isn't lost.
                        stash_event_front(&mut state.stashed_events, inner_evt);
                        break;
                    }
                }
            }
            return Ok(None);
        }
        _ => {}
    }
    Ok(Some(action))
}
/// The last stop: hand the action to the key-action dispatcher and honour an
/// exit request.
fn dispatch_key_action(
    app: &mut AppState,
    theme: &mut RenderTheme,
    slot: &mut SessionSlot,
    handle: &Handle,
    state: &mut LoopState,
    action: KeyAction,
) -> PhaseFlow {
    let exit = match slot.session_mut() {
        Some(session) => handle_key_action(
            app,
            action,
            session,
            handle,
            &mut state.active_prompt,
            &mut state.pending_permission,
            state.ui_mode,
            theme,
        ),
        None => handle_key_action_without_session(app, action, slot, state.ui_mode),
    };
    if exit {
        // Leaving on purpose, so say so instead of just going quiet. The
        // owner otherwise cannot tell this apart from a terminal that
        // died, waits out the lease and then lingers, and a `/exit` costs
        // ten more minutes of a parked plugin and MCP stack. What that
        // admission is allowed to end is the owner's call, not ours — a
        // job `/bg` handed off carries `Background` and keeps its hour.
        if let Some(remote) = slot
            .session()
            .and_then(|session| session.remote_background_attachment.as_ref())
        {
            remote.mark_exit_deliberate();
        }
        return PhaseFlow::Exit(EventLoopOutcome::Exit);
    }
    PhaseFlow::Continue
}

fn is_agent_view_left_press_candidate(app: &AppState, key: &event::KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press)
        && key.code == KeyCode::Left
        && key.modifiers.is_empty()
        && app.input.is_empty()
        && !app.help_open
        && app.onboarding_dialog.is_none()
}

fn has_inline_fullscreen_surface(app: &AppState) -> bool {
    app.agent_view.is_some()
        || app.background_tasks_dialog.is_some()
        // The `/provider` and `/login` dialogs are hosted inline in the
        // prompt area (like the settings dialog); the multi-step setup
        // wizard takes over the whole terminal via the alternate-screen
        // switch.
        || app
            .onboarding_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.is_inline_hosted())
}

fn inline_commit_prelude_shrink_height(
    current_height: u16,
    desired_after_commit: u16,
    expected_insert_height: u16,
) -> u16 {
    desired_after_commit
        .min(current_height)
        .max(current_height.saturating_sub(expected_insert_height))
}

/// Rows stay live while they can still change presentation: the active
/// prompt's just-submitted user rows remain withdrawable until the first reply,
/// and a running supervisor attachment keeps its mutable transcript suffix
/// repaintable until persisted rows stop changing its grouping or position.
fn inline_pinned_live_prefix(
    active_prompt: Option<&ActivePrompt>,
    rows: &[rebon_tui::Message],
    pre_flush_committed_rows: usize,
    overlay_is_empty: bool,
    remote_live_prefix: Option<usize>,
) -> usize {
    let mut prefix = match active_prompt {
        None => rows.len(),
        Some(active) if active.reply_started || !overlay_is_empty => rows.len(),
        Some(active) => {
            let scan_start = active
                .withdrawable
                .as_ref()
                .map(|withdrawable| withdrawable.transcript_len_before)
                .unwrap_or(pre_flush_committed_rows)
                .min(rows.len());
            rows.iter()
                .enumerate()
                .skip(scan_start)
                .find_map(|(idx, row)| matches!(row, rebon_tui::Message::User(_)).then_some(idx))
                .unwrap_or(rows.len())
        }
    };

    if let Some(remote_live_prefix) = remote_live_prefix {
        prefix = prefix.min(remote_live_prefix);
    }

    prefix
}

pub(super) fn remote_attachment_live_prefix(
    remote: Option<&crate::background::RemoteBackgroundAttachment>,
    rows: &[rebon_tui::Message],
) -> Option<usize> {
    let remote = remote?;
    // A splice fallback can replace an unsettled streaming slab with a
    // persisted row. Keep every such slab out of immutable scrollback even
    // when a later persisted row makes it non-trailing; otherwise the commit
    // cursor sees its `partial-*` key disappear and reprints the suffix.
    let unsettled_streaming_start = rows.iter().position(|row| {
        row.uuid().is_some_and(|uuid| {
            uuid.starts_with("partial-") && !remote.settled_local_row_uuids.contains(uuid)
        })
    });
    let mut persisted_end = rows.len();
    while persisted_end > 0 {
        // A settled `partial-*` row is as scrollback-safe as a persisted
        // one: the merge keeps it in place by design, and its covered
        // persisted twin never splices in to terminate the local run.
        // Treating it as "local" pinned every settled turn in the mutable
        // region forever — the whole session stopped reaching scrollback.
        let row_is_local = rows[persisted_end - 1].uuid().is_some_and(|uuid| {
            !remote.persisted_transcript_uuids.contains(uuid)
                && !remote.settled_local_row_uuids.contains(uuid)
        });
        if !row_is_local {
            break;
        }
        persisted_end -= 1;
    }

    let mutable_suffix_start = if remote.inline_transcript_tail_is_mutable() {
        rebon_tui::trailing_collapsible_tool_run_start(&rows[..persisted_end], true)
            .or((persisted_end < rows.len()).then_some(persisted_end))
    } else {
        // Local-only rows can be repositioned by every future remote replay,
        // including when a follow-up starts another turn on this attachment.
        // Keep them out of immutable scrollback until the attachment ends.
        (persisted_end < rows.len()).then_some(persisted_end)
    };

    match (unsettled_streaming_start, mutable_suffix_start) {
        (Some(streaming), Some(suffix)) => Some(streaming.min(suffix)),
        (Some(streaming), None) => Some(streaming),
        (None, suffix) => suffix,
    }
}

fn inline_input_should_scroll_into_view(
    ui_mode: UiMode,
    input: &str,
    last_rendered_input: &str,
) -> bool {
    ui_mode == UiMode::Inline && input != last_rendered_input
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rebon_tool::McpClient;

    use super::super::active_prompt::{WithdrawDestination, WithdrawableSubmit};
    use super::super::test_support::make_test_tui_session;
    use super::*;

    /// The one visible sign of the default startup: a dot after the cwd
    /// while the worker comes up, words only once it has taken a while,
    /// and nothing at all once the session is attached. Before the session
    /// exists the slot's preview says whether a worker is coming; after,
    /// the session's own wait does.
    #[test]
    fn the_status_bar_shows_a_dot_while_the_worker_comes_up_and_words_after_a_while() {
        let (slot, _tx) = SessionSlot::pending_for_test(
            super::super::StartupPreview::hosted_for_test(".", "sess-startup", "bg-startup"),
        );
        assert_eq!(slot.hosted_wait_footer_hint(), Some("·"));

        let mut session = make_test_tui_session();
        let mut slot = SessionSlot::ready(make_test_tui_session());
        assert_eq!(slot.hosted_wait_footer_hint(), None);

        session.pending_hosted_session = Some(crate::background::PendingHostedSession::startup(
            "bg-startup".into(),
        ));
        slot.install(session);
        assert_eq!(slot.hosted_wait_footer_hint(), Some("·"));

        slot.session_mut()
            .unwrap()
            .pending_hosted_session
            .as_mut()
            .unwrap()
            .started_at = Instant::now()
            .checked_sub(Duration::from_secs(4))
            .unwrap_or_else(Instant::now);
        assert_eq!(
            slot.hosted_wait_footer_hint(),
            Some("· starting session host…")
        );

        slot.session_mut().unwrap().pending_hosted_session = None;
        assert_eq!(slot.hosted_wait_footer_hint(), None);
    }

    #[test]
    fn changed_inline_input_scrolls_into_view() {
        assert!(inline_input_should_scroll_into_view(
            UiMode::Inline,
            "prompt ",
            "prompt",
        ));
    }

    #[test]
    fn unchanged_inline_input_does_not_scroll_into_view() {
        assert!(!inline_input_should_scroll_into_view(
            UiMode::Inline,
            "prompt",
            "prompt",
        ));
    }

    #[test]
    fn screen_input_change_does_not_use_terminal_scrollback_scroll() {
        assert!(!inline_input_should_scroll_into_view(
            UiMode::Screen,
            "prompt ",
            "prompt",
        ));
    }

    #[test]
    fn completed_first_run_onboarding_switches_to_selected_inline_mode() {
        assert!(should_exit_completed_onboarding_to_inline(
            true,
            UiMode::Inline,
            false,
            true,
        ));
    }

    #[test]
    fn completed_first_run_onboarding_stays_on_screen_when_screen_is_selected() {
        assert!(!should_exit_completed_onboarding_to_inline(
            true,
            UiMode::Screen,
            false,
            true,
        ));
    }

    #[test]
    fn stalled_force_drain_is_bounded_and_rearms_after_relevant_changes() {
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .upsert_streaming_tool_use(rebon_tui::StreamingToolUse {
                call_id: "tool-ask".into(),
                tool_name: "AskUserQuestion".into(),
                kind: rebon_types::ToolKind::Other,
                status: rebon_types::ToolCallStatus::Pending,
                title: None,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            });
        app.pending_permission_view = Some(PermissionModalView {
            query_id: 7,
            tool_call_id: "tool-ask".into(),
            title: "Choice".into(),
            summary: String::new(),
            options: Vec::new(),
            selected: 0,
            extra_text: String::new(),
            extra_text_focused: false,
            kind: PermissionKind::AskUserQuestion {
                questions: vec![crate::tui::permission_modal::AskUserQuestionEntry {
                    question: "Choose?".into(),
                    header: "Choice".into(),
                    options: vec![crate::tui::permission_modal::AskUserQuestionOption {
                        label: "A".into(),
                        description: String::new(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
                answers: vec![crate::tui::permission_modal::AskUserQuestionAnswer::new()],
                active_question: 0,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: serde_json::json!({}),
            },
        });

        let mut stall = InlineForceDrainStall::default();
        assert!(stall.should_attempt(&app));
        stall.record(&app);
        for _ in 0..100 {
            assert!(
                !stall.should_attempt(&app),
                "an unchanged open block must not force-drain again at frame rate"
            );
        }

        let PermissionKind::AskUserQuestion { answers, .. } =
            &mut app.pending_permission_view.as_mut().unwrap().kind
        else {
            unreachable!();
        };
        answers[0].highlighted_row = 1;
        assert!(
            stall.should_attempt(&app),
            "permission navigation must re-arm one attempt before the next redraw"
        );
        stall.record(&app);
        assert!(!stall.should_attempt(&app));

        let rebon_tui::StreamingContentBlock::ToolUse(tool) = &mut app.rebon_tui.overlay.blocks[0]
        else {
            unreachable!();
        };
        tool.status = rebon_types::ToolCallStatus::Completed;
        assert!(
            stall.should_attempt(&app),
            "a terminal tool status can unblock the sealed prefix"
        );

        app.rebon_tui.overlay.clear();
        app.rebon_tui.overlay.append_streaming_text("partial");
        stall.record(&app);
        assert!(!stall.should_attempt(&app));
        app.rebon_tui.overlay.append_streaming_text(" line");
        assert!(
            stall.should_attempt(&app),
            "new first-block text can add a drainable line and must re-arm"
        );
    }

    #[test]
    fn slash_onboarding_does_not_hot_switch_to_inline() {
        assert!(!should_exit_completed_onboarding_to_inline(
            false,
            UiMode::Inline,
            false,
            true,
        ));
    }

    fn pin_user_row(uuid: &str) -> rebon_tui::Message {
        pin_user_row_with_text(uuid, "user")
    }

    fn pin_user_row_with_text(uuid: &str, text: &str) -> rebon_tui::Message {
        rebon_tui::Message::User(rebon_tui::UserMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: rebon_tui::UserMessageInner {
                role: rebon_tui::UserRole::User,
                content: vec![rebon_tui::UserContentBlock::Text(
                    rebon_tui::UserTextBlock { text: text.into() },
                )],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    fn pin_assistant_row(uuid: &str) -> rebon_tui::Message {
        rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: rebon_tui::AssistantMessageInner {
                role: rebon_tui::AssistantRole::Assistant,
                content: vec![rebon_tui::AssistantContentBlock::Text(
                    rebon_tui::AssistantTextBlock {
                        text: "reply".into(),
                    },
                )],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn pin_tool_row(uuid: &str, tool_use_id: &str, name: &str) -> rebon_tui::Message {
        let input = match name {
            "Read" => serde_json::json!({ "file_path": "src/lib.rs" }),
            "Bash" | "PowerShell" => serde_json::json!({ "command": "cargo test" }),
            _ => serde_json::json!({ "pattern": "needle" }),
        };
        rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: rebon_tui::AssistantMessageInner {
                role: rebon_tui::AssistantRole::Assistant,
                content: vec![rebon_tui::AssistantContentBlock::ToolUse(
                    rebon_tui::AssistantToolUseBlock {
                        id: tool_use_id.into(),
                        name: name.into(),
                        input,
                        tool_call_content: None,
                        raw_output: None,
                        title: None,
                        locations: None,
                        status: Some(rebon_types::ToolCallStatus::Completed),
                    },
                )],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn pin_tool_result_row(uuid: &str, tool_use_id: &str) -> rebon_tui::Message {
        rebon_tui::Message::User(rebon_tui::UserMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: rebon_tui::UserMessageInner {
                role: rebon_tui::UserRole::User,
                content: vec![rebon_tui::UserContentBlock::ToolResult(
                    rebon_tui::UserToolResultBlock {
                        tool_use_id: tool_use_id.into(),
                        content: rebon_tui::ToolResultContent::Text("result".into()),
                        is_error: None,
                    },
                )],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    fn pin_system_row(uuid: &str, content: &str) -> rebon_tui::Message {
        rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            subtype: "info".into(),
            content: Some(content.into()),
            level: Some(rebon_tui::SystemLevel::Info),
            is_meta: None,
        })
    }

    fn pin_remote_attachment(
        status: crate::background::BackgroundJobStatus,
        persisted_uuids: &[&str],
    ) -> crate::background::RemoteBackgroundAttachment {
        let mut remote = crate::background::RemoteBackgroundAttachment::new(
            "bg-test".into(),
            "session-test".into(),
            ".".into(),
            status,
            0,
            crate::background::BackgroundIpcEndpoint {
                pid: 1,
                port: 1,
                token: "test".into(),
            },
        );
        remote.persisted_transcript_uuids = persisted_uuids
            .iter()
            .map(|uuid| (*uuid).to_string())
            .collect();
        remote
    }

    fn pin_active_prompt() -> ActivePrompt {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        ActivePrompt::new(rx, rebon_types::PromptCancel::new())
    }

    #[test]
    fn pinned_live_prefix_holds_withdrawable_prompt_before_reply() {
        let rows = vec![pin_user_row("u0"), pin_user_row("u1")];
        let active = pin_active_prompt().with_withdrawable(WithdrawableSubmit {
            destination: WithdrawDestination::Discard,
            transcript_len_before: 1,
            transcript_len_after: 2,
            user_message_uuid: Some("u1".into()),
        });

        assert_eq!(
            inline_pinned_live_prefix(Some(&active), &rows, 1, true, None),
            1,
            "the just-submitted prompt row must stay live while it can still be withdrawn"
        );
    }

    #[test]
    fn pinned_live_prefix_releases_everything_once_reply_started() {
        // Mid-turn injected user rows (queued steering messages, task
        // notifications) are already part of the model conversation; pinning
        // one strands the whole rest of the turn in the live region, the
        // overflow escape hatch fires every frame, and streaming text gets
        // chopped into per-frame fragment rows.
        let rows = vec![
            pin_user_row("u1"),
            pin_assistant_row("a1"),
            pin_user_row("u-injected"),
            pin_assistant_row("a2"),
        ];
        let mut active = pin_active_prompt().with_withdrawable(WithdrawableSubmit {
            destination: WithdrawDestination::Discard,
            transcript_len_before: 0,
            transcript_len_after: 1,
            user_message_uuid: Some("u1".into()),
        });
        active.reply_started = true;

        assert_eq!(
            inline_pinned_live_prefix(Some(&active), &rows, 1, true, None),
            rows.len(),
            "once the reply is visible nothing is withdrawable, so every row commits"
        );
        // A live overlay alone (reply_started not yet latched) also counts
        // as a visible reply.
        let unlatched = pin_active_prompt();
        assert_eq!(
            inline_pinned_live_prefix(Some(&unlatched), &rows, 1, false, None),
            rows.len()
        );
    }

    #[test]
    fn pinned_live_prefix_without_active_prompt_commits_all_rows() {
        let rows = vec![pin_user_row("u1"), pin_assistant_row("a1")];
        assert_eq!(
            inline_pinned_live_prefix(None, &rows, 0, true, None),
            rows.len()
        );
    }

    /// A settled `partial-*` row is scrollback-safe: the merge keeps it in
    /// place and its covered persisted twin never splices in to terminate
    /// the trailing local run. Counting it as "local" pinned every settled
    /// turn in the mutable region forever — nothing ever reached
    /// scrollback again.
    #[test]
    fn remote_live_prefix_releases_settled_local_rows() {
        let rows = vec![
            pin_user_row("u1"),
            pin_assistant_row("partial-3-0"),
            pin_assistant_row("partial-final-u1"),
        ];
        let mut remote =
            pin_remote_attachment(crate::background::BackgroundJobStatus::Running, &["u1"]);
        assert_eq!(
            remote_attachment_live_prefix(Some(&remote), &rows),
            Some(1),
            "unsettled slabs stay live: a splice can still drop them"
        );

        remote.settled_local_row_uuids = ["partial-3-0", "partial-final-u1"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            remote_attachment_live_prefix(Some(&remote), &rows),
            None,
            "a fully settled turn commits to scrollback like persisted rows"
        );
    }

    #[test]
    fn remote_live_prefix_tracks_tool_batches_before_local_recap() {
        let recap = pin_system_row("s-local", "attached supervisor recap");
        let initial_rows = vec![pin_user_row("u1"), recap.clone()];
        let mut remote =
            pin_remote_attachment(crate::background::BackgroundJobStatus::Running, &["u1"]);
        assert_eq!(
            remote_attachment_live_prefix(Some(&remote), &initial_rows),
            Some(1),
            "the local recap must stay live before the first transcript refresh"
        );

        let first_batch = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            recap.clone(),
        ];
        remote.persisted_transcript_uuids =
            ["u1", "a1", "r1"].into_iter().map(str::to_string).collect();
        assert_eq!(
            remote_attachment_live_prefix(Some(&remote), &first_batch),
            Some(1)
        );

        let queued_follow_up = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            recap.clone(),
            pin_user_row_with_text("u-follow-up", "check another path"),
        ];
        assert_eq!(
            remote_attachment_live_prefix(Some(&remote), &queued_follow_up),
            Some(1),
            "an optimistic follow-up after the recap must not release the active tool run"
        );

        let second_batch = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            pin_tool_row("a2", "t2", "Read"),
            pin_tool_result_row("r2", "t2"),
            recap,
        ];
        remote.persisted_transcript_uuids = ["u1", "a1", "r1", "a2", "r2"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            remote_attachment_live_prefix(Some(&remote), &second_batch),
            Some(1),
            "later persisted batches must remain in the same repaintable group"
        );

        remote.status = crate::background::BackgroundJobStatus::NeedsInput;
        assert_eq!(
            remote_attachment_live_prefix(Some(&remote), &second_batch),
            Some(1),
            "NeedsInput pauses the turn but does not finalize its transcript"
        );
    }

    #[test]
    fn remote_live_prefix_releases_persisted_rows_but_holds_local_suffix_at_terminal_status() {
        let rows = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            pin_system_row("s-local", "attached supervisor recap"),
        ];
        for status in [
            crate::background::BackgroundJobStatus::Idle,
            crate::background::BackgroundJobStatus::Succeeded,
            crate::background::BackgroundJobStatus::Failed,
            crate::background::BackgroundJobStatus::Stopped,
        ] {
            let remote = pin_remote_attachment(status, &["u1", "a1", "r1"]);
            assert_eq!(remote_attachment_live_prefix(Some(&remote), &rows), Some(3));
            assert_eq!(
                inline_pinned_live_prefix(
                    None,
                    &rows,
                    1,
                    true,
                    remote_attachment_live_prefix(Some(&remote), &rows),
                ),
                3
            );
        }
    }

    #[test]
    fn remote_live_prefix_does_not_hold_non_collapsible_shell_tools() {
        for name in ["Bash", "PowerShell"] {
            let rows = vec![
                pin_user_row("u1"),
                pin_tool_row("a1", "t1", name),
                pin_tool_result_row("r1", "t1"),
                pin_system_row("s-local", "attached supervisor recap"),
            ];
            let remote = pin_remote_attachment(
                crate::background::BackgroundJobStatus::Running,
                &["u1", "a1", "r1"],
            );
            assert_eq!(
                remote_attachment_live_prefix(Some(&remote), &rows),
                Some(3),
                "{name} must commit normally while only the movable recap stays live"
            );
        }
    }

    #[test]
    fn running_remote_page_switch_and_refresh_commit_one_collapsed_group() {
        let backend = ratatui::backend::TestBackend::new(80, 12);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        let recap = pin_system_row("s-local", "attached supervisor recap marker");
        let mut rows = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            recap.clone(),
        ];
        let mut remote = pin_remote_attachment(
            crate::background::BackgroundJobStatus::Running,
            &["u1", "a1", "r1"],
        );
        let mut app = AppState::default();
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows.clone());
        let theme = RenderTheme::plain();
        let mut runtime = InlineRuntimeState::with_initial_viewport_height(4);
        let pinned = inline_pinned_live_prefix(
            None,
            &rows,
            0,
            true,
            remote_attachment_live_prefix(Some(&remote), &rows),
        );
        rewind_inline_page_switch(
            &mut terminal,
            &mut app,
            &theme,
            &mut runtime,
            "Main",
            pinned,
        )
        .expect("append remote page switch");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 1);

        rows = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            recap.clone(),
            pin_user_row_with_text("u-follow-up", "check another path"),
        ];
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows.clone());
        let pinned = inline_pinned_live_prefix(
            None,
            &rows,
            1,
            true,
            remote_attachment_live_prefix(Some(&remote), &rows),
        );
        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, pinned)
            .expect("hold tool batch across an optimistic remote follow-up");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 1);

        rows = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            pin_tool_row("a2", "t2", "Read"),
            pin_tool_result_row("r2", "t2"),
            recap,
        ];
        remote.persisted_transcript_uuids = ["u1", "a1", "r1", "a2", "r2"]
            .into_iter()
            .map(str::to_string)
            .collect();
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows.clone());
        let pinned = inline_pinned_live_prefix(
            None,
            &rows,
            1,
            true,
            remote_attachment_live_prefix(Some(&remote), &rows),
        );
        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, pinned)
            .expect("hold refreshed tool batch and recap");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 1);

        remote.status = crate::background::BackgroundJobStatus::NeedsInput;
        let pinned = inline_pinned_live_prefix(
            None,
            &rows,
            1,
            true,
            remote_attachment_live_prefix(Some(&remote), &rows),
        );
        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, pinned)
            .expect("keep paused turn live");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 1);

        remote.status = crate::background::BackgroundJobStatus::Succeeded;
        let pending_terminal_sync = inline_pinned_live_prefix(
            None,
            &rows,
            1,
            true,
            remote_attachment_live_prefix(Some(&remote), &rows),
        );
        flush_inline_commits(
            &mut terminal,
            &mut app,
            &theme,
            &mut runtime,
            pending_terminal_sync,
        )
        .expect("hold tail until the terminal transcript refresh completes");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 1);

        remote.terminal_transcript_synced = true;
        let released = inline_pinned_live_prefix(
            None,
            &rows,
            1,
            true,
            remote_attachment_live_prefix(Some(&remote), &rows),
        );
        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, released)
            .expect("commit completed tool run while keeping the movable recap live");
        assert_eq!(runtime.commit_cursor.committed_row_count(), rows.len() - 1);

        let detached = inline_pinned_live_prefix(None, &rows, rows.len() - 1, true, None);
        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, detached)
            .expect("commit the recap when the remote attachment ends");
        let output = terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .chain(terminal.backend().buffer().content.iter())
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            output.contains("Searched for 1 pattern, read 1 file"),
            "completed batches should collapse together: {output:?}"
        );
        assert_eq!(output.matches("Searched for").count(), 1, "{output:?}");
        assert_eq!(runtime.commit_cursor.committed_row_count(), rows.len());
    }

    #[test]
    fn terminal_remote_follow_up_can_move_local_suffix_without_recommitting_history() {
        let backend = ratatui::backend::TestBackend::new(80, 12);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        let recap = pin_system_row("s-local", "attached supervisor recap");
        let mut rows = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            recap.clone(),
        ];
        let mut remote = pin_remote_attachment(
            crate::background::BackgroundJobStatus::Succeeded,
            &["u1", "a1", "r1"],
        );
        let mut app = AppState::default();
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows.clone());
        let theme = RenderTheme::plain();
        let mut runtime = InlineRuntimeState::with_initial_viewport_height(4);

        let terminal_prefix = remote_attachment_live_prefix(Some(&remote), &rows)
            .expect("local recap remains movable while attached");
        flush_inline_commits(
            &mut terminal,
            &mut app,
            &theme,
            &mut runtime,
            terminal_prefix,
        )
        .expect("commit completed remote turn");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 3);

        remote.status = crate::background::BackgroundJobStatus::Queued;
        remote.terminal_transcript_synced = false;
        rows.push(pin_user_row_with_text("u-next", "start another turn"));
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows.clone());
        let queued_prefix = remote_attachment_live_prefix(Some(&remote), &rows)
            .expect("optimistic follow-up keeps the suffix live");
        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, queued_prefix)
            .expect("hold optimistic follow-up");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 3);

        rows = vec![
            pin_user_row("u1"),
            pin_tool_row("a1", "t1", "Grep"),
            pin_tool_result_row("r1", "t1"),
            pin_user_row_with_text("u-next", "start another turn"),
            recap,
        ];
        remote.persisted_transcript_uuids = ["u1", "a1", "r1", "u-next"]
            .into_iter()
            .map(str::to_string)
            .collect();
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows.clone());
        let refreshed_prefix = remote_attachment_live_prefix(Some(&remote), &rows)
            .expect("recap remains the only movable suffix");
        flush_inline_commits(
            &mut terminal,
            &mut app,
            &theme,
            &mut runtime,
            refreshed_prefix,
        )
        .expect("commit persisted follow-up without moving committed history");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 4);

        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, rows.len())
            .expect("commit recap after detaching");
        let output = terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .chain(terminal.backend().buffer().content.iter())
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(output.matches("Grep (needle)").count(), 1, "{output:?}");
        assert_eq!(output.matches("[tool result t1]").count(), 1, "{output:?}");
        assert_eq!(runtime.commit_cursor.committed_row_count(), rows.len());
    }

    #[test]
    fn drain_mcp_load_result_records_ready_and_failed_status() {
        let mut app = AppState::default();
        let mut session = make_test_tui_session();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        session.engine_half.mcp = Some(crate::session::mcp::SessionMcp::with_loader(
            rx,
            TuiMcpLoadStatus::Loading,
        ));

        let client = rebon_plugin_mcp::InMemoryMcpClient::new();
        client.register_tool("server", "ping", serde_json::json!({"ok": true}));
        tx.send(TuiMcpLoadEvent::Ready(
            crate::session::mcp::McpClientBuild {
                client: Some(Arc::new(client) as Arc<dyn rebon_tool::McpClient>),
                warnings: vec!["MCP server `bad` from test is unavailable and was skipped".into()],
            },
        ))
        .expect("send ready");

        drain_mcp_load_result(&mut app, &mut session);

        let mcp = session
            .engine_half
            .mcp
            .as_ref()
            .expect("test session hosts MCP");
        assert!(mcp.delayed.is_ready());
        assert_eq!(mcp.delayed.server_names(), vec!["server"]);
        assert!(matches!(
            &mcp.load_status,
            TuiMcpLoadStatus::Ready { warnings } if warnings.len() == 1
        ));
        assert!(app.rebon_tui.transcript.rows().is_empty());
        let hint = app.idle_prompt_top_hint().expect("mcp warning hint");
        assert_eq!(hint.text, "MCP warning: run /mcp for details");

        tx.send(TuiMcpLoadEvent::Failed("broken config".into()))
            .expect("send failed");
        drain_mcp_load_result(&mut app, &mut session);

        let mcp = session
            .engine_half
            .mcp
            .as_ref()
            .expect("test session hosts MCP");
        assert!(matches!(
            &mcp.load_status,
            TuiMcpLoadStatus::Failed { error } if error == "broken config"
        ));
        assert!(app.rebon_tui.transcript.rows().is_empty());
        let hint = app.idle_prompt_top_hint().expect("mcp failure hint");
        assert_eq!(hint.text, "MCP unavailable: run /mcp for details");
    }

    #[test]
    fn drain_mcp_load_result_surfaces_rust_lsp_failures_as_ui_only_hints() {
        let mut app = AppState::default();
        let mut session = make_test_tui_session();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        session.engine_half.mcp = Some(crate::session::mcp::SessionMcp::with_loader(
            rx,
            TuiMcpLoadStatus::Loading,
        ));

        let warning = "MCP server `rust_lsp` from CLI is unavailable and was skipped: Rust LSP initialize failed: rust-analyzer is unavailable: failed to spawn `rust-analyzer`";
        let client = rebon_plugin_mcp::InMemoryMcpClient::new();
        tx.send(TuiMcpLoadEvent::Ready(
            crate::session::mcp::McpClientBuild {
                client: Some(Arc::new(client) as Arc<dyn rebon_tool::McpClient>),
                warnings: vec![warning.into()],
            },
        ))
        .expect("send ready with rust lsp warning");

        drain_mcp_load_result(&mut app, &mut session);

        assert!(matches!(
            &session.engine_half.mcp.as_ref().expect("test session hosts MCP").load_status,
            TuiMcpLoadStatus::Ready { warnings } if warnings == &[warning]
        ));
        assert!(app.rebon_tui.transcript.rows().is_empty());
        let hint = app.idle_prompt_top_hint().expect("rust lsp warning hint");
        assert_eq!(
            hint.text,
            "Rust LSP unavailable: run rustup component add rust-analyzer · /mcp for details"
        );

        let error = format!("{warning}\nMCP server startup failed");
        tx.send(TuiMcpLoadEvent::Failed(error.clone()))
            .expect("send rust lsp failure");
        drain_mcp_load_result(&mut app, &mut session);

        assert!(matches!(
            &session.engine_half.mcp.as_ref().expect("test session hosts MCP").load_status,
            TuiMcpLoadStatus::Failed { error: stored } if stored == &error
        ));
        assert!(app.rebon_tui.transcript.rows().is_empty());
        let hint = app.idle_prompt_top_hint().expect("rust lsp failure hint");
        assert_eq!(
            hint.text,
            "Rust LSP unavailable: run rustup component add rust-analyzer · /mcp for details"
        );
    }

    #[test]
    fn rust_lsp_load_hint_keeps_non_install_failures_generic() {
        assert_eq!(
            rust_lsp_load_hint(
                "MCP server `rust_lsp` from CLI is unavailable and was skipped: initialize failed"
            ),
            Some("Rust LSP unavailable: run /mcp for details")
        );
        assert_eq!(rust_lsp_load_hint("MCP server `other` failed"), None);
    }

    #[test]
    fn drain_mcp_load_result_records_not_configured_status() {
        let mut app = AppState::default();
        let mut session = make_test_tui_session();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        session.engine_half.mcp = Some(crate::session::mcp::SessionMcp::with_loader(
            rx,
            TuiMcpLoadStatus::Loading,
        ));
        app.set_mcp_load_hint("old warning");

        tx.send(TuiMcpLoadEvent::NotConfigured)
            .expect("send not configured");
        drain_mcp_load_result(&mut app, &mut session);

        assert!(matches!(
            session
                .engine_half
                .mcp
                .as_ref()
                .expect("test session hosts MCP")
                .load_status,
            TuiMcpLoadStatus::NotConfigured
        ));
        assert!(app.rebon_tui.transcript.rows().is_empty());
        assert!(app.idle_prompt_top_hint().is_none());
    }

    #[test]
    fn agent_view_left_press_candidate_ignores_help_overlay() {
        let mut app = AppState::default();
        let key = event::KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);

        assert!(is_agent_view_left_press_candidate(&app, &key));

        app.help_open = true;
        assert!(!is_agent_view_left_press_candidate(&app, &key));
    }

    #[test]
    fn resume_dialog_does_not_force_inline_fullscreen_surface() {
        let mut app = AppState::default();
        app.resume_dialog = Some(crate::tui::resume_dialog::ResumeDialogState::open());

        assert!(!has_inline_fullscreen_surface(&app));
    }

    /// `/login` used to bounce an inline session into the alternate
    /// screen just to show a one-row login picker, throwing away the
    /// visible scrollback. It belongs in the inline prompt host, like
    /// `/provider`; only the multi-step wizard still needs the screen.
    #[test]
    fn login_dialog_does_not_force_inline_fullscreen_surface() {
        let mut app = AppState::default();
        app.onboarding_dialog =
            Some(rebon_plugin_onboarding::OnboardingDialogState::open_for_login_pane());
        assert!(!has_inline_fullscreen_surface(&app));

        app.onboarding_dialog = Some(
            rebon_plugin_onboarding::OnboardingDialogState::open_for_command_with_ui_mode(
                UiMode::Inline,
            ),
        );
        assert!(has_inline_fullscreen_surface(&app));
    }

    #[test]
    fn inline_commit_prelude_shrink_is_bounded_by_pending_insert_height() {
        assert_eq!(inline_commit_prelude_shrink_height(12, 3, 4), 8);
        assert_eq!(inline_commit_prelude_shrink_height(12, 9, 4), 9);
        assert_eq!(inline_commit_prelude_shrink_height(12, 3, 0), 12);
    }

    #[test]
    fn page_layout_reset_drops_physical_frame_and_scroll_anchors() {
        let mut app = AppState::default();
        app.scroll_offset = 9;
        app.prev_scroll_offset = 3;
        app.follow_transcript_tail = false;
        app.total_content_lines = 42;
        app.prev_frame_lines = vec!["old page".into()];
        app.prev_frame_area = Some(ratatui::layout::Rect::new(0, 2, 80, 10));
        app.transcript_sticky_anchor = Some(rebon_tui::TranscriptStickyAnchor {
            row_index: 1,
            scroll_offset: 4,
        });
        app.transcript_sticky_anchor_label = Some("old anchor".into());
        app.transcript_sticky_anchor_area = Some(ratatui::layout::Rect::new(0, 0, 20, 1));
        app.scroll_to_bottom_area = Some(ratatui::layout::Rect::new(70, 20, 8, 1));
        app.last_prompt_input_area = Some(ratatui::layout::Rect::new(0, 18, 80, 3));
        app.rebon_tui.transcript.push(pin_user_row("cache-row"));
        let cache_area = ratatui::layout::Rect::new(0, 0, 40, 8);
        let mut cache_buffer = ratatui::buffer::Buffer::empty(cache_area);
        rebon_tui::render_transcript_cached_with_running_hints(
            &app.rebon_tui,
            cache_area,
            &mut cache_buffer,
            &rebon_tui::RenderTheme::default(),
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            &mut app.transcript_measure_cache,
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        assert!(!app.transcript_measure_cache.is_empty());
        let rows = vec![pin_user_row("u0")];
        let mut inline_runtime = InlineRuntimeState::default();
        let batches = inline_runtime
            .commit_cursor
            .prepare_batches_with_pinned_live_prefix(&rows, 1, rows.len());
        inline_runtime
            .commit_cursor
            .mark_committed(&batches[0], &rows, 1);

        reset_page_layout_state(&mut app, &mut inline_runtime);

        assert_eq!(inline_runtime.commit_cursor.committed_row_count(), 0);
        assert_eq!(app.total_content_lines, 0);
        assert!(app.prev_frame_lines.is_empty());
        assert_eq!(app.prev_frame_area, None);
        assert_eq!(app.transcript_sticky_anchor, None);
        assert_eq!(app.transcript_sticky_anchor_label, None);
        assert_eq!(app.transcript_sticky_anchor_area, None);
        assert_eq!(app.scroll_to_bottom_area, None);
        assert_eq!(app.last_prompt_input_area, None);
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.prev_scroll_offset, 0);
        assert!(app.follow_transcript_tail);
        assert!(!app.transcript_measure_cache.is_empty());
    }

    #[test]
    fn inline_page_switch_rewinds_and_repaints_the_complete_selected_transcript() {
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("old-main-page-marker"),
                    frame.area(),
                );
            })
            .expect("seed old page");
        let mut app = AppState::default();
        for index in 0..501 {
            let text = match index {
                0 => "first-agent-transcript-row",
                500 => "last-agent-transcript-row",
                _ => "middle-agent-transcript-row",
            };
            app.rebon_tui
                .transcript
                .push(pin_user_row_with_text(&format!("agent-row-{index}"), text));
        }
        let mut inline_runtime = InlineRuntimeState::with_initial_viewport_height(4);
        let row_count = app.rebon_tui.transcript.len();

        rewind_inline_page_switch(
            &mut terminal,
            &mut app,
            &RenderTheme::default(),
            &mut inline_runtime,
            "Agent: worker",
            row_count,
        )
        .expect("rewind page switch");

        assert_eq!(
            inline_runtime.commit_cursor.committed_row_count(),
            app.rebon_tui.transcript.len()
        );
        let output = terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .chain(terminal.backend().buffer().content.iter())
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            output.contains("first-agent-transcript-row"),
            "the first row must be physically emitted, not only marked committed: {output:?}"
        );
        assert!(
            output.contains("last-agent-transcript-row"),
            "the final row must be physically emitted: {output:?}"
        );
        assert!(
            !output.contains("old-main-page-marker"),
            "the previous page must be purged before the target page is repainted: {output:?}"
        );
    }

    #[test]
    fn inline_page_switch_keeps_short_history_next_to_the_banner() {
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        let mut app = AppState::default();
        app.rebon_tui.transcript.push(pin_user_row_with_text(
            "agent-short-row",
            "short-agent-prompt",
        ));
        let mut inline_runtime = InlineRuntimeState::with_initial_viewport_height(4);

        rewind_inline_page_switch(
            &mut terminal,
            &mut app,
            &RenderTheme::default(),
            &mut inline_runtime,
            "Agent: worker",
            1,
        )
        .expect("rewind short page switch");

        let mut rows = Vec::new();
        for buffer in [terminal.backend().scrollback(), terminal.backend().buffer()] {
            for y in buffer.area.y..buffer.area.y.saturating_add(buffer.area.height) {
                rows.push(
                    (buffer.area.x..buffer.area.x.saturating_add(buffer.area.width))
                        .map(|x| buffer.cell((x, y)).expect("terminal cell").symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string(),
                );
            }
        }
        let banner_row = rows
            .iter()
            .position(|row| row.contains("Agent: worker"))
            .expect("page banner row");
        let prompt_row = rows
            .iter()
            .position(|row| row.contains("short-agent-prompt"))
            .expect("short history row");
        let blank_rows = rows[banner_row + 1..prompt_row]
            .iter()
            .filter(|row| row.trim().is_empty())
            .count();

        assert!(
            blank_rows <= 2,
            "short history must stay adjacent to the page banner: {rows:?}"
        );
    }

    #[test]
    fn prompt_rainbow_color_interpolates_across_keyword_length() {
        assert_eq!(prompt_rainbow_color(0, 10, false), "rgb(235,95,87)");
        assert_eq!(prompt_rainbow_color(2, 10, false), "rgb(247,167,91)");
        assert_eq!(prompt_rainbow_color(9, 10, false), "rgb(200,130,180)");
        assert_ne!(
            prompt_rainbow_color(2, 10, false),
            prompt_rainbow_color(2, 10, true)
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn run_inline_background_tasks_overlay(
    app: &mut AppState,
    theme: &mut RenderTheme,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    task_notification_retry_after: &mut Option<Instant>,
) -> anyhow::Result<()> {
    let mut guard = AltScreenOverlayGuard::enter()?;
    loop {
        drain_ui_channels(app, session, pending_permission, active_prompt);
        maybe_update_loading_state(
            app,
            session,
            handle,
            active_prompt,
            task_notification_retry_after,
        );
        refresh_background_tasks_dialog(app);
        refresh_live_agent_tool_activity(app);

        let hosted_turn_started_at = session
            .remote_background_attachment
            .as_ref()
            .and_then(|remote| remote.running_turn_started_at());
        let is_loading = active_prompt.is_some() || hosted_turn_started_at.is_some();
        let elapsed_ms = active_prompt
            .as_ref()
            .map(|active| active.started_at)
            .or(hosted_turn_started_at)
            .map(|started_at| started_at.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let status = StatusBarInfo {
            provider: &session.model.provider_name,
            model: &session.model.name,
            cwd: &session.cwd,
            elapsed_ms,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let runtime_input = app.build_runtime_input();
        let runtime_state = derive_prompt_input_runtime_state(&runtime_input, prompt_rainbow_color);
        let mut cursor_hint = None;
        guard
            .terminal()
            .draw(|frame| {
                render_frame(
                    frame,
                    app,
                    &runtime_state,
                    theme,
                    is_loading,
                    &status,
                    Some(session),
                    &mut cursor_hint,
                );
            })
            .context("draw inline fullscreen background-tasks overlay")?;

        if app.background_tasks_dialog.is_none() || active_prompt.is_none() {
            break;
        }
        if !poll_crossterm_event(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = read_crossterm_event()? else {
            continue;
        };
        if maybe_handle_background_tasks_dialog_key(
            app,
            session.engine_half.tasks.as_ref(),
            &key,
            active_prompt,
        ) {
            continue;
        }
    }
    Ok(())
}

fn prompt_rainbow_color(index: usize, len: usize, shimmer: bool) -> String {
    const BASE: [(u8, u8, u8); 7] = [
        (235, 95, 87),
        (245, 139, 87),
        (250, 195, 95),
        (145, 200, 130),
        (130, 170, 220),
        (155, 130, 200),
        (200, 130, 180),
    ];
    const SHIMMER: [(u8, u8, u8); 7] = [
        (250, 155, 147),
        (255, 185, 137),
        (255, 225, 155),
        (185, 230, 180),
        (180, 205, 240),
        (195, 180, 230),
        (230, 180, 210),
    ];
    let palette = if shimmer { SHIMMER } else { BASE };
    let len = len.max(1);
    let scaled = if len == 1 {
        0
    } else {
        index.saturating_mul(palette.len() - 1) / (len - 1)
    };
    let next = (scaled + 1).min(palette.len() - 1);
    let segment_count = palette.len() - 1;
    let segment_start = scaled.saturating_mul(len - 1) / segment_count;
    let segment_end = next.saturating_mul(len - 1) / segment_count;
    let segment_width = segment_end.saturating_sub(segment_start).max(1);
    let numerator = index.saturating_sub(segment_start).min(segment_width);
    let (r1, g1, b1) = palette[scaled];
    let (r2, g2, b2) = palette[next];
    let blend = |a: u8, b: u8| -> u8 {
        let a = a as usize;
        let b = b as usize;
        ((a * (segment_width - numerator) + b * numerator) / segment_width) as u8
    };
    format!("rgb({},{},{})", blend(r1, r2), blend(g1, g2), blend(b1, b2))
}

fn reset_inline_viewport_if_pending(
    _guard: &mut TerminalGuard,
    app: &mut AppState,
    inline_runtime: &mut InlineRuntimeState,
) -> anyhow::Result<()> {
    if !app.pending_inline_viewport_reset {
        return Ok(());
    }
    app.pending_inline_viewport_reset = false;
    // Trim — do not reset — the commit cursor. The requesters of this flag
    // differ in what survives in the transcript: /new wipes it (trim ≡ reset),
    // resume replaces it wholesale (no keys match → trim ≡ reset), but a
    // prompt withdraw only truncates the tail. In the withdraw case the
    // untouched prefix is already in terminal scrollback; a full reset would
    // re-commit and visibly duplicate it.
    inline_runtime
        .commit_cursor
        .trim_to_rows(app.rebon_tui.transcript.rows());
    Ok(())
}

fn emit_inline_startup_banner_if_pending(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    slot: &SessionSlot,
    inline_runtime: &mut InlineRuntimeState,
) -> anyhow::Result<()> {
    let refresh =
        std::mem::take(&mut app.pending_inline_banner_refresh) && app.startup_banner_is_empty();
    if !app.pending_inline_startup_banner && !refresh {
        return Ok(());
    }
    let banner = startup_banner_for_slot(slot, app);
    if refresh && !app.pending_inline_startup_banner {
        inline_runtime.current_viewport_height =
            super::inline_banner::refresh_inline_startup_banner(guard.terminal(), &banner)?;
        return Ok(());
    }
    app.pending_inline_startup_banner = false;
    inline_runtime.current_viewport_height =
        prepare_inline_viewport_for_startup_banner(guard.terminal())?;
    emit_inline_startup_banner_from(guard.terminal(), &banner)
}

fn reset_page_layout_state(app: &mut AppState, inline_runtime: &mut InlineRuntimeState) {
    inline_runtime.commit_cursor.reset();
    reset_transcript_page_state(app);
}

fn rewind_inline_page_switch<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut AppState,
    theme: &RenderTheme,
    inline_runtime: &mut InlineRuntimeState,
    page_name: &str,
    pinned_live_prefix: usize,
) -> anyhow::Result<()> {
    use ratatui::crossterm::{
        cursor::MoveTo,
        execute,
        terminal::{Clear, ClearType},
    };

    terminal
        .clear()
        .context("clear inline terminal for page switch")?;
    execute!(
        std::io::stdout(),
        Clear(ClearType::All),
        Clear(ClearType::Purge),
        MoveTo(0, 0),
    )
    .context("purge terminal scrollback for inline page switch")?;
    terminal
        .reseed_inline_viewport_after_clear()
        .context("reseed inline viewport after page-switch clear")?;
    reset_page_layout_state(app, inline_runtime);
    app.pending_inline_startup_banner = false;
    app.pending_inline_viewport_reset = false;

    let banner = PageBanner::new(page_name);
    inline_runtime.current_viewport_height =
        prepare_inline_viewport_for_page_banner(terminal, &banner)
            .context("prepare inline viewport for page banner")?;
    emit_inline_page_banner(terminal, &banner).context("emit inline page banner")?;

    if app.rebon_tui.transcript.rows().is_empty() {
        return Ok(());
    }

    // Keep the viewport prepared for the page banner while rebuilding history.
    // Shrinking it back to the configured launch height here moves the viewport
    // away from the banner before `insert_before` runs; a short Agent transcript
    // then lands near the bottom of that viewport and leaves a large blank band
    // between the page title and its opening prompt. The normal post-rebuild
    // reconcile sizes the live viewport to its content on this same frame.

    // Commit the immutable prefix in BOUNDED chunks. The repo's iron rule
    // (see `inline_full_repaint_for_resize`'s "hard limit" / `# Budget` sections)
    // is that any O(transcript) repaint must carry a budget: a single giant
    // flush both overruns a BSU/ESU frame's implementation-defined size limit
    // AND clips at `measure_inline_commit_batch`'s `u16::MAX` line clamp once the
    // transcript exceeds ~65535 physical lines. Committing at most
    // `inline_resize_repaint_budget()` rows per flush — each in its own
    // synchronized frame — keeps every batch's measure well under the clamp and
    // presents progressively instead of overrunning one frame. Unlike the resize
    // path there is no light fallback: a page switch must physically emit the
    // selected immutable history, so we loop until the pinned prefix is committed.
    let target_row_count = pinned_live_prefix.min(app.rebon_tui.transcript.len());
    let budget = inline_resize_repaint_budget().max(1);
    loop {
        let committed = inline_runtime.commit_cursor.committed_row_count();
        if committed >= target_row_count {
            break;
        }
        let target = committed.saturating_add(budget).min(target_row_count);
        let _batch_sync = super::terminal_capabilities::begin_synchronized_frame();
        flush_inline_commits(terminal, app, theme, inline_runtime, target)
            .context("rebuild inline transcript after page switch")?;
    }
    Ok(())
}

/// Full-repaint the inline surface on a terminal resize: wipe screen +
/// scrollback and rebuild every committed row from the transcript at the new
/// width, in place. This is the fix for the inline resize-ghosting bug (width
/// changes leaving mangled `┌──┌──` box-border fragments in scrollback); the
/// sections below are the full record of why it is shaped this way, including
/// the two regressions that shaped it.
///
/// # Why a full repaint is necessary
///
/// Inline mode pushes every committed transcript row into the terminal's REAL
/// scrollback via `insert_before` and then never repaints it again — only the
/// live bottom viewport is managed. That is fine until the terminal is resized:
///
/// - **Width change**: the terminal reflows all that scrollback we no longer
///   touch. The startup banner's box-drawing borders (and any wrapped row) get
///   re-wrapped by the terminal itself, leaving mangled `┌──┌──` fragments we
///   cannot repaint — they live above the viewport, outside what we redraw.
/// - **Height shrink**: the cursor / viewport anchor drifts relative to the
///   rows already on screen.
///
/// The only robust fix is to discard the reflowed scrollback and rebuild from
/// our own source of truth (the transcript) at the new width: on width-change
/// OR height-shrink, clear scrollback (`ESC[2J ESC[3J ESC[H`) and redraw the
/// whole transcript from (0,0).
///
/// # When it runs — and when it must NOT
///
/// Gated by [`InlineRuntimeState::resize_needs_full_repaint`]: a width change in
/// EITHER direction, or a height *shrink*. A height *grow* deliberately stays on
/// the cheap incremental viewport path — growing only adds blank rows below
/// without reflowing existing scrollback, so there is nothing to rebuild and a
/// full repaint there would clear scrollback for no reason.
///
/// # Why it does not flicker — and the hard limit on that claim
///
/// MUST run inside the frame's `begin_synchronized_frame()` BSU/ESU bracket
/// (DEC 2026 synchronized output, opened above in the event loop). The
/// `ESC[2J ESC[3J` wipe blanks the whole screen; the synchronized-output
/// wrapper buffers the wipe AND the rebuild and presents them as one atomic
/// frame, so the blank intermediate never reaches the glass. Outside the
/// bracket this flickers hard on every width change. The BSU/ESU bracket is
/// what makes this path flicker-free.
///
/// Synchronized output's atomicity **has a ceiling**, though: BSU/ESU must not
/// be misread as "wrapping it makes it atomic". A synchronized update is only
/// meant to protect one **bounded** draw, never to act as a transaction that
/// hides an **unbounded** whole-transcript rebuild. A terminal puts
/// implementation-defined time/size limits on the writes inside a single BSU/ESU
/// frame, so once an O(transcript) rebuild outgrows them, the half-rebuilt
/// intermediate state shows through anyway — that is the root cause of the
/// long-context resize flicker. Hence one iron rule: **any repaint that scales
/// linearly with the transcript's length must carry a budget cap or a
/// fallback**, and a synchronized frame alone is not enough. Where this function
/// lands on that rule is the `# Budget` section below.
///
/// # Budget — when it must NOT run a full rebuild
///
/// Gated additionally by `inline_resize_repaint_budget()`: the work below is
/// O(transcript), so on a transcript longer than the budget (default 500 logical
/// rows, env-tunable) this function early-returns a *light* repaint that does
/// NOT purge scrollback, reset the commit cursor, or replay history. It only
/// drops the width-keyed measure cache; the incremental autoresize path then
/// re-anchors and fully redraws the LIVE viewport at the new width, while the
/// committed scrollback above it is left for the terminal to reflow on its own
/// (mangled banner borders included). That cosmetic reflow is the deliberate
/// price of never flashing on long transcripts; small transcripts keep the
/// full-fidelity rebuild below. This is the concrete application of the
/// budget-or-fallback rule stated in `# Why it does not flicker`.
///
/// # Why it rebuilds everything itself — regression #1: "history got cleared"
///
/// It does NOT re-arm the pending-banner / viewport-reset flags and let the
/// trailing inline steps rebuild. Those are the *incremental* per-frame path,
/// and routing a whole-transcript rebuild through them is wrong twice over:
///
/// 1. They feed the commit-driven
///    `shrink_inline_viewport_keeping_top_for_insert_before` an
///    `expected_insert_height` equal to the ENTIRE history. That step is sized
///    for a handful of streamed rows, so the oversized value mangles the
///    viewport geometry and the rebuilt rows never reach scrollback — this was
///    the "history got cleared on resize" regression.
/// 2. The trailing banner step re-emits a banner that `insert_before` then
///    chops mid-render once the rebuilt content exceeds `viewport_top`.
///
/// Instead we mirror the launch-time [`prime_inline_scrollback_from_transcript`]
/// path directly: commit the whole transcript in one pass with the full row
/// count, leaving the commit cursor at `row_count` so the inline arm's later
/// commit-driven shrink and trailing `flush_inline_commits` both see
/// `pre_flush_committed_rows == row_count` and no-op.
///
/// # The banner, in each branch
///
/// - **Empty transcript** → re-emit the banner, like a fresh start; it is all
///   there is to draw. Note the `ESC[3J` also wipes whatever the shell showed
///   before rebon launched (the command line, earlier output). That is the
///   unavoidable cost of clearing scrollback, and it is NOT a bug — it was once
///   mis-reported as "resize leaves only the banner", but an empty session
///   genuinely has nothing else to redraw.
/// - **Non-empty transcript** → re-emit the banner as history's ROW 0, then
///   rebuild the transcript below it. The launch-time resume path (in
///   `run_blocking_entry`) SKIPS the banner, because there the pre-existing
///   shell content makes `insert_before` chop it mid-render. A resize repaint is
///   different: it begins from a freshly cleared screen and a re-seeded top-row
///   viewport, so the geometry is clean and the banner renders whole. The
///   history flush then scrolls the banner into the top of scrollback (long
///   history) or leaves it visible at the top (short history). Either way the
///   banner ends up as history's row 0 — exactly like a fresh start — so it
///   SURVIVES every resize instead of vanishing the moment the transcript goes
///   non-empty. (Regression #2: before this emit, any resize with history made
///   the banner disappear.)
///
/// # Steps
///
/// 1. `ESC[2J ESC[3J ESC[H` — erase screen, erase scrollback, home the cursor.
/// 2. Re-seed ratatui's inline viewport (a single top row) against the
///    now-empty homed screen so its tracked geometry stops describing the
///    pre-wipe layout.
/// 3. Reset the commit cursor + measure cache and clear the pending-flag steps
///    so `reset_inline_viewport_if_pending` /
///    `emit_inline_startup_banner_if_pending` don't re-fire on top of this
///    rebuild.
/// 4. Empty → banner only. Non-empty → banner as row 0, then seed the viewport
///    to the configured inline height and rebuild the immutable transcript prefix.
fn inline_full_repaint_for_resize(
    guard: &mut TerminalGuard,
    app: &mut AppState,
    theme: &RenderTheme,
    slot: &SessionSlot,
    inline_runtime: &mut InlineRuntimeState,
    pinned_live_prefix: usize,
) -> anyhow::Result<()> {
    use ratatui::crossterm::{
        cursor::MoveTo,
        execute,
        terminal::{Clear, ClearType},
    };
    // Over-budget fallback (option 2). A full repaint is O(transcript): it
    // purges native scrollback and rebuilds the whole history row by row, and
    // the more rows there are, the more writes are packed into one BSU/ESU
    // frame — the easier it is to overrun the terminal's time/size limit for a
    // synchronized update, and so the blank post-clear intermediate state gets
    // drawn = the long-context resize flash. Above the budget we no longer
    // purge/replay: we only drop the width-keyed measurement cache and let the
    // incremental path below (`autoresize` → `resize`) re-anchor and fully
    // redraw the **live region** at the new width; the history above the
    // viewport is left to the terminal to reflow on its own (mangled banner
    // borders included). That visual cost buys never flashing, on purpose. A
    // short transcript still takes the full-fidelity rebuild below.
    if app.rebon_tui.transcript.len() > inline_resize_repaint_budget() {
        app.clear_all_transcript_measure_caches();
        return Ok(());
    }
    // ESC[2J ESC[3J ESC[H — erase screen, erase scrollback, home the cursor.
    execute!(
        std::io::stdout(),
        Clear(ClearType::All),
        Clear(ClearType::Purge),
        MoveTo(0, 0),
    )
    .context("purge terminal scrollback for inline resize repaint")?;
    // ratatui's tracked geometry still describes the pre-wipe layout; re-seed
    // it against the now-empty homed screen (a single top row) so the rebuild
    // below lands at the new width.
    guard
        .terminal()
        .reseed_inline_viewport_after_clear()
        .context("reseed inline viewport after resize clear")?;
    inline_runtime.commit_cursor.reset();
    app.clear_all_transcript_measure_caches();
    // This helper rebuilds everything itself; make sure the trailing
    // pending-flag steps in the inline arm don't re-fire on top of it.
    app.pending_inline_startup_banner = false;
    app.pending_inline_banner_refresh = false;
    app.pending_inline_viewport_reset = false;

    if app.rebon_tui.transcript.rows().is_empty() {
        // No transcript to rebuild — re-emit the banner at the new width,
        // exactly like a fresh inline start. `prepare_…` grows the re-seeded
        // single-row viewport to `H - banner - gap`; `emit_…`'s `insert_before`
        // then pushes the banner and trailing gap to the top and re-anchors the
        // viewport below it.
        // The post-flush reconcile in the inline arm shrinks it to the prompt.
        inline_runtime.current_viewport_height =
            prepare_inline_viewport_for_startup_banner(guard.terminal())
                .context("prepare inline viewport after empty resize repaint")?;
        let banner = startup_banner_for_slot(slot, app);
        emit_inline_startup_banner_from(guard.terminal(), &banner)
            .context("emit inline startup banner after empty resize repaint")?;
        return Ok(());
    }

    // Transcript present: re-emit the banner as history's row 0, then rebuild
    // the whole transcript below it. The launch-time resume path
    // (run_blocking_entry.rs:203) skips the banner because the pre-existing
    // shell content there makes `insert_before` chop it mid-render; a resize
    // repaint instead starts from a freshly cleared screen and a re-seeded
    // single top row, so the geometry is clean. `emit_inline_startup_banner`
    // lands the banner plus its trailing gap at rows [0, banner + gap) off the
    // row-0 viewport; the history flush below then scrolls it into the top of
    // scrollback (long history) or leaves it visible at the top (short history).
    // Either way the banner ends up as history's row 0 — exactly like a fresh
    // start — so it survives every resize instead of vanishing the moment the
    // transcript goes non-empty.
    let banner = startup_banner_for_slot(slot, app);
    emit_inline_startup_banner_from(guard.terminal(), &banner)
        .context("emit inline startup banner before transcript resize rebuild")?;

    // Seed the viewport to the configured inline height (a top-anchored grow off
    // the post-banner viewport, no scroll), then rebuild only the immutable
    // transcript prefix. A running remote attachment may still insert persisted
    // rows before its local recap or extend a trailing collapsed tool group, so
    // that suffix must remain in the repaintable viewport after the purge.
    let initial = inline_runtime.initial_viewport_height.max(1);
    guard
        .terminal()
        .set_viewport_height(initial)
        .context("seed inline viewport before resize transcript rebuild")?;
    inline_runtime.current_viewport_height = initial;
    let target_row_count = pinned_live_prefix.min(app.rebon_tui.transcript.len());
    flush_inline_commits(
        guard.terminal(),
        app,
        theme,
        inline_runtime,
        target_row_count,
    )
    .context("rebuild inline transcript after terminal resize")?;
    Ok(())
}

fn pending_footer_action_hint(
    last_agent_view_left_press_at: Option<Instant>,
    last_ctrl_c_exit_press_ms: u64,
) -> Option<&'static str> {
    let now = Instant::now();
    if last_agent_view_left_press_at
        .is_some_and(|last| now.saturating_duration_since(last) <= Duration::from_millis(400))
    {
        return Some("press ← again for agents view");
    }

    let now_ms = wall_clock_ms();
    if last_ctrl_c_exit_press_ms != 0 && now_ms.saturating_sub(last_ctrl_c_exit_press_ms) <= 2_000 {
        return Some("press Ctrl+C again to exit");
    }

    None
}
