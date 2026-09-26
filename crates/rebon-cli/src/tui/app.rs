//! Top-level app state for the local TUI run mode.
//!
//! [`AppState`] holds the authoritative copies of the prompt input
//! buffer, cursor, and mode. The `tui::runner` event loop mutates
//! this struct via [`crate::tui::dispatch`], and each frame the
//! renderer derives its view state from it via
//! [`rebon_tui::promptinput::derive_prompt_input_runtime_state`] plus
//! [`rebon_tui::render_prompt_input`].
//!
//! The layering: `AppState` owns the mutable state as plain owned
//! fields, the prompt-input surface is a pure view that receives
//! them as inputs, and edits flow back through a
//! [`crate::tui::dispatch`] step rather than direct mutation from
//! the render path.
//!
//! ## Core scope
//!
//! * Prompt buffer (`input`), cursor (`cursor_offset`), mode
//!   (`mode`), optional placeholder.
//! * A [`Self::build_runtime_input`] that assembles a full
//!   [`PromptInputRuntimeInput`] by filling the many trigger /
//!   suggestion / history fields with neutral defaults so the
//!   prompt-input state machine can be driven even when the richer
//!   surfaces (history search, slash command finder, @member mention
//!   resolver, ultrathink toggle, etc.) are not wired.
//!
//! Beyond that core, the struct also carries the transcript store,
//! streaming overlay, engine query executor handle, permissions
//! modal state, etc.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::mpsc::UnboundedSender;

use rebon_permissions::auto_mode_denials::AutoModeDenialStore;
use rebon_permissions::PermissionMode;
#[cfg(test)]
use rebon_plugin_tasks::runtime::TaskRegistry;
use rebon_plugin_tasks::runtime::{TaskId, TaskSnapshot};
use rebon_tui::input::VimMode;
use rebon_tui::layout::unseen_divider::UnseenDividerState;
use rebon_tui::promptinput::footer_navigation::FooterItem;
use rebon_tui::promptinput::{
    resolve_prompt_input_placeholder, OptionMetaHint, PromptInputRuntimeInput,
    PromptPlaceholderInput, PromptSuggestionState, QueuedCommand, QueuedCommandValue,
    TaskListStatus, TextRange,
};
use rebon_tui::ToolOutputVerbosity;
use rebon_types::PromptPasteContent;
use rebon_types::{effort_indicator::EffortProviderKind, ReasoningEffort};
use rebon_types::{ConfigOption, PlanEntry, SlashCommand};

pub type GoalState = crate::goal::GoalState;
pub type PendingGoalClarification = crate::goal::PendingGoalClarification;

use crate::file_scanner::FileIndex;
use crate::session::input_history::HistoryEntry;
use crate::session::submit_payload::SubmitPayload;
use crate::session::transcript_replay::BackgroundAgentTaskRef;
use crate::session::ultraplan_run::UltraplanStatus;
use crate::tui::agent_view::AgentViewState;
use crate::tui::at_mention_picker::AtMentionPickerState;
use crate::tui::bridge_dialog::RcStatusState;
use crate::tui::global_search_dialog::GlobalSearchDialogState;
use crate::tui::goal_confirm_dialog::GoalConfirmDialogState;
use crate::tui::mcp_dialog::McpDialogState;
use crate::tui::permission_modal::PermissionModalView;
use crate::tui::prompt_tips::{
    PromptTopHint, PromptTopHintInput, PromptTopHintRegistry, PromptTopHintTone,
    MCP_PROMPT_TOP_HINT_ID, UPDATE_PROMPT_TOP_HINT_ID,
};
use crate::tui::resume_dialog::ResumeDialogState;
use crate::tui::rewind_dialog::RewindDialogState;
use crate::tui::slash_picker::SlashPickerState;
use rebon_plugin_onboarding::OnboardingDialogState;
use rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogState;
use rebon_plugin_tasks::ui::teams_dialog::TeamsDialogState;
use rebon_plugin_updater::UpdateNoticeState;

#[cfg(test)]
type AppTaskProjection = Arc<TaskRegistry>;
#[cfg(not(test))]
type AppTaskProjection = Vec<TaskSnapshot>;

#[cfg(test)]
fn empty_task_projection() -> AppTaskProjection {
    Arc::new(TaskRegistry::new())
}

#[cfg(not(test))]
fn empty_task_projection() -> AppTaskProjection {
    Vec::new()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptCompletionStatus {
    Succeeded,
    Failed,
}

#[derive(Debug)]
pub struct StoredTranscriptView {
    pub tui: rebon_tui::AppState,
    pub measure_cache: rebon_tui::TranscriptMeasureCache,
}

impl StoredTranscriptView {
    pub fn from_tui(tui: rebon_tui::AppState) -> Self {
        Self {
            tui,
            measure_cache: rebon_tui::TranscriptMeasureCache::new(),
        }
    }
}

impl Default for StoredTranscriptView {
    fn default() -> Self {
        Self::from_tui(rebon_tui::AppState::default())
    }
}

impl Clone for StoredTranscriptView {
    fn clone(&self) -> Self {
        // Measurement data is derived; cloning app snapshots must not duplicate large warm caches.
        Self::from_tui(self.tui.clone())
    }
}

/// A slash command whose expansion is in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandingCommand {
    /// Which wait this is. An abandoned expansion is not cancelled -- only
    /// whoever registered the command decides when the call returns -- so its
    /// answer can still arrive after the person has given up and typed a
    /// different command. The id travels with the answer, and one that does
    /// not match the wait now in flight is dropped rather than submitted as
    /// that other command's expansion.
    pub id: u64,
    /// The command's name, for the notice on screen.
    pub name: String,
    /// The line as typed. The input is cleared while the expansion runs, so
    /// this is what a failed expansion leaves in history for the person to
    /// recall and retype.
    pub line: String,
}

/// Minimal authoritative state for the local TUI session.
///
/// The fields live at this level so the runner can mutate them
/// without having to reach through submodules: prompt input and its
/// pickers, the [`Self::rebon_tui`] transcript store + streaming
/// overlay maintained by `rebon-tui::reducer`, and the permission,
/// dialog and task surfaces the runner drives.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Working directory for resolving prompt attachments.
    pub cwd: String,
    /// What this session's turns have cost, and since when.
    ///
    /// A handle on the session's ledger (`session::usage::UsageLedger`), not a
    /// copy: the turns of a hosted session run in a worker, and a count kept
    /// per screen answered `/cost` with the zeros of a screen no turn had ever
    /// passed through. A state built before a session has its own ledger,
    /// which the session's replaces when one is installed.
    pub usage_ledger: Arc<std::sync::Mutex<crate::session::usage::UsageLedger>>,
    /// Current raw prompt buffer.
    pub input: String,
    /// Current cursor byte-offset inside [`Self::input`].
    pub cursor_offset: usize,
    /// Current prompt mode — matches `rebon_tui::promptinput::HistoryMode`
    /// string form (`"prompt"` / `"bash"`; `rebon_tui`'s mode cycle
    /// also names `"plan"` / `"auto"`).
    /// Stored as a `String` because the state-machine crate's
    /// [`PromptInputRuntimeInput::mode`] field is a `String`.
    pub mode: String,
    /// Default placeholder. Currently a hard-coded friendly ASCII
    /// hint; a real placeholder resolver can be plumbed in later.
    pub default_placeholder: Option<String>,
    /// Context-sensitive prompt tip selected by the runner from stable
    /// frame timing and current UI context.
    pub contextual_tip: Option<String>,
    /// Stable rotation key for contextual prompt tips.
    pub contextual_tip_key: String,
    /// Current index within the selected contextual-tip candidate list.
    pub contextual_tip_index: usize,
    /// Epoch-millisecond deadline for the next contextual-tip rotation.
    pub contextual_tip_next_rotate_at_ms: u64,
    /// Transcript + streaming overlay owned by `rebon-tui`.
    ///
    /// Wires the engine update stream here via
    /// [`crate::tui::update::translate_session_update`] → the
    /// `rebon-tui` reducer.
    pub rebon_tui: rebon_tui::AppState,
    /// Memoized measurement of the inline live tail. Three probes ask the
    /// same layout question every frame — overflow, viewport sizing, and
    /// the paint — and each used to deep-copy the tail and run the full
    /// renderer to answer it. Interior-mutable because the overflow probe
    /// measures through a shared reference. Empty between turns.
    pub(crate) inline_tail_measure: crate::tui::runner::InlineTailMeasureSlot,
    /// How many times the memo above was rebuilt; telemetry and tests.
    pub(crate) inline_tail_measure_builds: std::cell::Cell<u64>,
    /// Active UI mode for update-time streaming flush policy. Inline
    /// mode commits every sealed block to terminal scrollback; screen
    /// mode preserves historical trailing-tool grouping in the overlay.
    pub ui_mode: crate::ui_config::UiMode,
    /// Active formula display mode. Rendering code consumes this TUI-only
    /// runtime preference; changing it invalidates transcript measurements.
    pub math_rendering_mode: crate::rebon_config::MathRenderingMode,
    /// Inline-mode session switches need to clear only the live ratatui viewport, not terminal scrollback.
    pub pending_inline_viewport_reset: bool,
    /// Inline-mode user-created sessions reuse the startup banner as the visible scrollback boundary.
    pub pending_inline_startup_banner: bool,
    pub pending_inline_banner_refresh: bool,
    /// Page transition requested after switching between Main and live agent views.
    /// Screen mode clears the viewport once; inline mode appends a page banner and
    /// the selected transcript after the existing terminal scrollback.
    pub pending_page_hard_refresh: Option<String>,
    /// Cloneable view state for the currently-open permission modal.
    ///
    /// The corresponding `oneshot::Sender` stays runner-local because
    /// it is not `Clone`; `AppState` only carries the renderable
    /// snapshot that promptinput / ratatui need.
    pub pending_permission_view: Option<PermissionModalView>,
    /// Provider/model change that landed on disk but whose runtime has not
    /// been re-resolved yet.
    ///
    /// Set when an approved `ProfileSwitch` moves the provider or model:
    /// applying reaches the session through shared handles, but re-resolving
    /// the runtime needs `&mut` on the session plus a tokio handle, and a
    /// permission callback holds neither. `drain_ui_channels` performs it on
    /// the next pass, where it has both.
    pub pending_profile_runtime_refresh:
        Option<crate::session::commands::provider::ProviderRuntimeUpdate>,
    /// Where a slash command's expansion is delivered when it runs off the
    /// event loop.
    ///
    /// A `Prompt` command on the `command-registry` seat may be a plugin's,
    /// and asking a plugin what a line expands to is a call to another
    /// process: run on the event loop it stops the terminal drawing until the
    /// plugin answers. The loop arms the sender and drains the receiver every
    /// pass; a surface with no loop behind it leaves the sender `None` and
    /// expands inline, as every surface used to.
    pub command_expansion_tx: Option<UnboundedSender<(u64, Result<String, String>)>>,
    /// `Some` while an expansion is in flight: its presence is what refuses a
    /// second submit. Cleared by the drain, and by an Esc that abandons the
    /// wait.
    pub expanding_command: Option<ExpandingCommand>,
    /// Hands out [`ExpandingCommand::id`]. Monotonic for the life of the
    /// process, so an abandoned expansion's late answer can never carry the
    /// id of a later one.
    pub next_command_expansion_id: u64,
    /// Asynchronous local shells and completed feedback awaiting durable replay.
    pub(crate) inline_shell_commands: crate::tui::runner::task_runtime::InlineShellCommands,
    /// Answers to deferred `AskUserQuestion`s, waiting for the runner to
    /// turn them into user messages.
    pub(crate) deferred_question_inbox:
        crate::tui::runner::deferred_questions::DeferredQuestionInbox,
    /// FIFO submit queue used when Enter is pressed while a prompt is
    /// already in flight.
    pub queued_commands: Vec<QueuedCommand>,
    /// Full submit payloads paired with editable text queued commands.
    ///
    /// `queued_commands` is the prompt-input-facing lightweight view;
    /// this queue preserves attachments, execution policy, model text,
    /// and generated user-message UUIDs until the queued prompt is
    /// either auto-drained or restored for editing.
    pub queued_submit_payloads: Vec<SubmitPayload>,
    /// Goal continuation payloads that should auto-run after the active turn
    /// finishes and can be canceled by `/goal clear`, `/goal off`, or `/goal stop`.
    pub deferred_goal_submit_payloads: Vec<SubmitPayload>,
    /// Internal submit payloads that should auto-run after the active turn
    /// finishes without appearing as editable queued user messages.
    pub deferred_internal_submit_payloads: Vec<SubmitPayload>,
    /// Queued-submit poller used by the active prompt to inject plain
    /// queued user input between tool rounds before the next model request.
    pub mid_turn_queued_submit_poller:
        Option<Arc<crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller>>,
    /// Set after withdrawing an auto-drained queued prompt so the
    /// requeued head item is not immediately auto-submitted again before
    /// the user has a chance to retrieve it with Up-arrow.
    pub queued_auto_drain_paused_after_withdrawal: bool,
    /// Minimal undo history for `push_previous_to_buffer` / Ctrl-Z.
    /// Stores the previous input and cursor offset.
    pub undo_stack: Vec<(String, usize)>,
    /// Line-based scroll offset from the top of the transcript
    /// content. Unlike the previous message-index approach, this
    /// counts terminal lines, giving smooth per-line scrolling.
    pub scroll_offset: usize,
    /// Whether the transcript should keep following the tail as new
    /// messages arrive.
    pub follow_transcript_tail: bool,
    /// Last bare Down key press that occurred while the transcript was scrolled away from bottom.
    pub last_transcript_down_press_ms: u64,
    /// Total content lines from the last render pass. Updated each
    /// frame by `render_transcript_area` so scroll math can compute
    /// `max_scroll_offset = total_content_lines - viewport_height`.
    pub total_content_lines: usize,
    /// Current top sticky transcript anchor, populated by the last screen-mode transcript render.
    pub transcript_sticky_anchor: Option<rebon_tui::TranscriptStickyAnchor>,
    pub transcript_sticky_anchor_label: Option<String>,
    pub transcript_sticky_anchor_area: Option<ratatui::layout::Rect>,
    pub scroll_to_bottom_area: Option<ratatui::layout::Rect>,
    /// Available slash commands received from the engine via
    /// `SessionUpdate::SlashCommands`. Populated once at session
    /// start and updated whenever the engine pushes a new set.
    pub slash_commands: Vec<SlashCommand>,
    /// Slash command picker state backed by `rebon-customselect`'s
    /// `NavigationState`. `None` means the picker is not active.
    pub slash_picker: Option<SlashPickerState>,
    /// File index populated asynchronously by the file scanner.
    /// Used by the `@` mention picker for fuzzy file search.
    pub file_index: FileIndex,
    /// Current async file scanner status. Used by `@` mention empty-state rendering.
    pub file_scan_status: crate::file_scanner::FileScanStatus,
    /// `@` mention picker state backed by `rebon-customselect`'s
    /// `NavigationState`. `None` means the picker is not active.
    pub at_mention_picker: Option<AtMentionPickerState>,
    /// Tool output verbosity level. Default is `Compact` (header only).
    /// Ctrl+O/Ctrl+E toggle to `Verbose` (full details).
    pub tool_output_verbosity: ToolOutputVerbosity,
    /// Non-modal update banner populated by the async startup npm check.
    pub update_notice: Option<UpdateNoticeState>,
    /// Prompt-frame top hints shown only when the prompt is empty and output is idle.
    pub prompt_top_hints: PromptTopHintRegistry,
    /// Next paste id to allocate when collapsing pasted text to a
    /// `[Pasted text #N]` reference. Starts at 1 and increments.
    pub next_paste_id: u32,
    /// Stored pasted content payloads keyed by paste id. When the user
    /// pastes multi-line text, it collapses to a `[Pasted text #N]`
    /// chip in the prompt; the full text is stored here and expanded
    /// back on submit.
    pub pasted_contents: Vec<PromptPasteContent>,
    /// In-memory input history for arrow-key navigation. Stored in
    /// chronological order (oldest first); newest is `history.last()`.
    pub history: Vec<HistoryEntry>,
    /// Current position in history. `0` = live draft (no history shown),
    /// `1` = most recent entry, `2` = second most recent, etc.
    pub history_index: usize,
    /// Saved draft input when the user enters history navigation.
    /// Restored when they navigate back past the newest entry.
    pub saved_draft: Option<String>,
    /// Pasted payloads belonging to [`Self::saved_draft`].
    pub saved_draft_pasted_contents: Option<Vec<PromptPasteContent>>,
    /// Session task rows projected for rendering.
    ///
    /// The production projection is snapshot data only; the registry belongs
    /// to `SessionEngineHalf`. Tests substitute an isolated registry so view
    /// fixtures can seed task state without constructing a full session.
    pub(crate) tasks: AppTaskProjection,
    /// Local-agent task currently promoted into the live foreground view.
    /// `None` means the main session is the active view. When this is
    /// set, `rebon_tui` holds the agent transcript and `main_agent_view`
    /// receives main-session stream updates off-screen.
    pub foregrounded_task_id: Option<String>,
    /// Main-session transcript and measurements preserved while a local agent owns the foreground.
    pub main_agent_view: Option<StoredTranscriptView>,
    /// Per-local-agent live transcript views and measurements keyed by task id.
    pub local_agent_views: HashMap<String, StoredTranscriptView>,
    /// Background tasks dialog state, `Some` while the "/tasks" dialog
    /// is open.
    ///
    /// When set, the runner:
    /// * Routes keyboard events to
    ///   [`BackgroundTasksDialogState::handle_key`] instead of the
    ///   prompt surface.
    /// * Replaces the prompt-surface render with the dialog overlay.
    /// * Calls [`BackgroundTasksDialogState::refresh`] each frame so
    ///   the layout reflects in-flight registry changes.
    pub background_tasks_dialog: Option<BackgroundTasksDialogState>,
    /// Full-screen Agent View for persistent background jobs and in-process agents.
    pub agent_view: Option<AgentViewState>,
    /// Full-screen global-search dialog state.
    pub global_search_dialog: Option<GlobalSearchDialogState>,
    /// Read-only MCP server and tool browser.
    pub mcp_dialog: Option<McpDialogState>,
    /// Latest local Remote Control runner snapshot.
    pub rc_status: RcStatusState,
    /// Inline confirmation dialog for replacing a completed goal.
    pub goal_confirm_dialog: Option<GoalConfirmDialogState>,
    /// Full-screen teams dialog state.
    pub teams_dialog: Option<TeamsDialogState>,
    /// Full-screen resume-session dialog state.
    pub resume_dialog: Option<ResumeDialogState>,
    /// Full-screen rewind dialog state.
    pub rewind_dialog: Option<RewindDialogState>,
    /// Full-screen onboarding dialog state.
    pub onboarding_dialog: Option<OnboardingDialogState>,
    /// Stack of the dialogs implemented as `DialogModel`: `/effort`,
    /// `/model`, `/memory`, `/provider`, `/doctor`, `/skills`,
    /// `/plugin`, `/hooks`. It answers on its own the four questions
    /// every `Option` field above still has to be listed for by hand.
    pub dialogs: crate::tui::dialog_host::DialogHost,
    /// Tracks when tasks transitioned to completed so the task list
    /// panel keeps recently-completed items visible for 30 seconds.
    pub task_completion_timestamps: Vec<TaskCompletionEntry>,
    /// Previous task IDs and statuses from the last frame, used to
    /// detect completion transitions.
    pub prev_task_snapshot: Vec<(String, TaskListStatus)>,
    /// When all tasks first become completed, we record `now + 5 000 ms`
    /// here.  Once the deadline passes (and tasks are still all
    /// completed), `reset_task_list` runs and the panel disappears.
    pub task_hide_deadline_ms: Option<u64>,
    /// Whether the task list panel is collapsed (header-only).
    /// Auto-set to `true` when total tasks exceed 6.
    pub task_list_collapsed: bool,
    /// Previous task count used for one-shot auto-collapse.
    /// Auto-collapse only triggers when count transitions from ≤6 to >6.
    pub task_list_prev_count: usize,
    /// Unseen-divider state machine from `rebon_tui::layout`. Tracks the
    /// divider position when the user scrolls away from the tail,
    /// enabling the "N new messages" pill and the visual divider line
    /// between seen/unseen messages in the transcript.
    pub unseen_divider: UnseenDividerState,
    /// Tool-call IDs of TodoWrite invocations that were suppressed
    /// from the streaming overlay. Tracked so follow-up
    /// `ToolCallUpdate` messages for the same call can also be
    /// silently dropped (they only carry the `tool_call_id`, not the
    /// tool name).
    pub hidden_tool_call_ids: std::collections::HashSet<String>,
    /// After an optimistic no-reply withdrawal, late visible stream
    /// updates may still arrive for the cancelled turn. Current ACP
    /// session updates do not carry a turn/user-message id, so this
    /// guard suppresses visible updates while the app is idle; it is
    /// cleared before submitting a new prompt so the next turn can
    /// stream normally.
    pub suppress_late_visible_updates_after_withdrawal: bool,
    /// Tool-call IDs of in-flight EnterPlanMode / ExitPlanMode tool
    /// invocations. Maps `tool_call_id → target_mode` so the
    /// `ToolCallUpdate(Completed)` handler can flip
    /// `permission_mode` when the tool finishes.
    pub pending_plan_mode_tool_ids: std::collections::HashMap<String, PermissionMode>,
    /// Session-local bridge from committed Agent tool-call ids to their
    /// background task registry identities.
    pub background_agent_tool_tasks: std::collections::HashMap<String, BackgroundAgentTaskRef>,
    /// Task snapshots projected from the currently attached cross-process background worker.
    pub remote_background_tasks:
        std::collections::HashMap<String, rebon_session_host::BackgroundTaskSnapshot>,
    /// Render-time live status for background Agent tool cards.
    pub live_agent_tool_activity:
        std::collections::HashMap<String, rebon_tui::LiveAgentToolActivity>,
    /// Incremented whenever `live_agent_tool_activity` changes so
    /// transcript render caches can invalidate activity-bearing rows.
    pub live_agent_tool_activity_revision: u64,
    /// Whether a bracketed-paste is currently in progress. Used by
    /// [`rebon_tui::input::should_swallow_event`] to prevent pasted newlines from
    /// triggering submit. Crossterm delivers pastes atomically via
    /// `Event::Paste`, so this is a defensive guard for edge cases.
    pub is_pasting: bool,
    /// Armed after a clipboard adoption (full clipboard applied
    /// instantly while the terminal is still streaming the paste) or
    /// a direct Ctrl+V clipboard paste. Matches the terminal's replay
    /// of the already-applied text so it can be dropped instead of
    /// pasted a second time. See `runner::paste_echo`.
    pub paste_echo: Option<crate::tui::runner::paste_echo::PasteEchoSuppressor>,
    /// Active permission mode. Cycled via Shift+Tab. `Default` means
    /// no special mode is active and the indicator is hidden.
    pub permission_mode: PermissionMode,
    /// Shared mode cell the engine's permission broker reads on every
    /// tool dispatch (via a `PermissionModeProvider` closure installed
    /// at wiring time). Kept in lock-step with `permission_mode` by
    /// [`Self::cycle_permission_mode`] and direct assignments through
    /// [`Self::set_permission_mode`]. The engine does NOT watch
    /// `AppState::permission_mode` directly because that would force
    /// `rebon-core` to depend on TUI state.
    pub permission_mode_cell: Arc<std::sync::Mutex<PermissionMode>>,
    /// Session-scoped denial store populated by the engine's auto-mode
    /// path. The `/permissions` slash command reads from this to
    /// render the captured denials and drive approve/retry selection.
    pub auto_mode_denials: Arc<std::sync::Mutex<AutoModeDenialStore>>,
    /// Deny-fingerprint cache shared with the engine's auto-mode gate.
    /// `/permissions approve` installs a one-shot exemption here so the
    /// model's own retry of the approved invocation slides through once.
    pub auto_mode_verdicts: Arc<rebon_permissions::AutoModeVerdictCache>,
    /// Tool call ids the engine's auto-mode gate let through without a
    /// permission dialog, mapped to which part of the gate decided, so their
    /// cards can say so and say *who*.
    ///
    /// Session-scoped and display-only. Live turns fill it from
    /// `SessionUpdate::ToolCallAutoModeAllowed`; a resumed session fills it
    /// from the transcript's `autoModeAllowed` sidecar. Nothing here is ever
    /// fed back to the model.
    pub auto_mode_allowed_tool_ids:
        std::collections::HashMap<String, rebon_types::AutoModeAllowSource>,
    /// Current vim mode. `None` when vim keybindings are not active.
    /// `Some(Insert)` / `Some(Normal)` when the vim state machine is
    /// engaged.
    pub vim_mode: Option<VimMode>,
    /// Initial vim mode to sync to on startup (from engine/session
    /// config).
    pub initial_vim_mode: Option<VimMode>,
    pub custom_status_line: crate::tui::runner::custom_status_line::CustomStatusLineState,
    /// The current random spinner verb shown in the prompt border while
    /// loading (e.g. "Thinking…", "Cooking…", "Pondering…"). Picked
    /// randomly each time `is_loading` transitions to `true`.
    pub spinner_verb: String,
    /// Spinner verb to restore after a temporary compaction override.
    /// Captures the in-flight random verb so compact can display
    /// `"Compacting"` and then return to the same wording.
    pub spinner_verb_before_compacting: Option<String>,
    /// Whether the previous frame was in loading state. Used to detect
    /// the `false → true` transition and pick a new spinner verb.
    pub was_loading: bool,
    /// Whether the help overlay is currently open.
    /// Toggled by `?` via `InputChangePlan::ToggleHelp` and closed
    /// by `close_help` from both event plan and normalized change.
    pub help_open: bool,
    /// Currently selected help tab index within the visible help tab list.
    pub help_tab_index: usize,
    /// Whether prompt speculation is currently active. Set to `true`
    /// when a speculation run starts; cleared by `AbortSpeculation`
    /// or `abort_speculation` from the normalized change path.
    pub speculation_active: bool,
    /// Whether the side-question ("/btw") modal is visible. Cleared
    /// by `DismissSideQuestion` from the event plan path.
    pub side_question_visible: bool,
    /// Whether a prompt suggestion is currently being generated.
    /// Cleared by `abort_prompt_suggestion` from the normalized
    /// change path.
    pub prompt_suggestion_active: bool,
    /// Whether the stash hint notification has been dismissed this
    /// session. Once set, the hint is never re-shown.
    pub stash_hint_dismissed: bool,
    /// Latest macOS Option-as-Meta hint for status-bar toast display.
    /// Cleared after the renderer consumes it or after a timeout.
    pub option_meta_hint_toast: Option<OptionMetaHint>,
    /// Epoch-millisecond timestamp of the last bare-Esc press in the
    /// double-press-Esc handler. Used to detect the second press
    /// within a 400ms window to open the message selector.
    pub last_esc_press_ms: u64,
    /// When `Left` was last pressed on an empty prompt. Two of them inside
    /// the double-press window hand this session to a background worker
    /// (`/hosted` without typing it); zero means "no gesture in progress".
    pub last_left_press_ms: u64,
    /// Epoch-millisecond timestamp of the last Ctrl+C/Ctrl+D exit-confirmation press.
    /// Used to require a second press before exiting an idle session.
    pub last_ctrl_c_exit_press_ms: u64,
    /// Plan entries received from the engine via
    /// `SessionUpdate::Plan`. Updated each time the engine pushes
    /// a new plan snapshot.
    pub plan_entries: Vec<PlanEntry>,
    /// Config options received from the engine via
    /// `SessionUpdate::ConfigOptionUpdate`. Updated whenever the
    /// engine pushes new config state.
    pub config_options: Vec<ConfigOption>,
    /// Session title from `SessionUpdate::SessionInfoUpdate`. Shown
    /// in the status bar or terminal window title.
    pub session_title: Option<String>,
    /// A first-prompt routing decision the engine made and the runner has
    /// not yet applied to the terminal's copy of the session model.
    pub pending_routed_model: Option<rebon_core::model_routing::RoutedSelection>,
    /// Whether the assistant is currently generating a response.
    /// Set to `true` by the runner when `active_prompt.is_some()`,
    /// cleared when the prompt completes or is cancelled.
    pub is_loading: bool,
    /// Outcome of the most recently completed prompt, shown in the terminal title.
    pub(crate) prompt_completion_status: Option<PromptCompletionStatus>,
    /// Current retry progress from the [`rebon_api::RetryMiddleware`].
    /// Set each frame by reading the shared [`rebon_api::RetryNotifier`].
    /// `None` when no retry is in progress.
    pub retry_info: Option<rebon_api::RetryProgress>,
    /// Running token count reported by the model during streaming.
    /// Updated on every [`SessionUpdate::TokenUsage`] event so the
    /// prompt chrome can display it in real time.
    pub streaming_token_count: u32,
    /// How many times the "Press up to edit queued messages" hint has
    /// been shown. Once it reaches `NUM_TIMES_QUEUE_HINT_SHOWN` the
    /// hint is suppressed for the rest of the session.
    pub queued_command_up_hint_count: u64,
    /// Whether coordinator mode is active. Set by `/ceo`,
    /// cleared by `/ceo off`. Drives the header
    /// indicator, coordinator system prompt injection, and footer hint
    /// suppression while background coordinator agents are active.
    pub coordinator_mode: bool,
    /// Current effort level. Set by `/effort`. `None` means auto
    /// (the engine decides based on model defaults).
    pub effort_level: Option<ReasoningEffort>,
    /// Provider kind for effort labelling. Determines whether the UI
    /// says "effort" (Anthropic) or "thinking" (OpenAI-compatible).
    pub effort_provider_kind: EffortProviderKind,
    /// Pending teammate messages (as `<teammate-message>` XML) to
    /// inject into the next prompt the model sees. Drained from the
    /// team mailbox by `drain_team_mailbox`, consumed by
    /// `spawn_prompt_turn` so the model receives teammate context.
    pub pending_teammate_prompts: Vec<String>,
    /// Pending MCP channel notifications (as `<channel>` XML) to inject into
    /// the next prompt the model sees. Drained from the MCP stdio runtime and
    /// consumed by `spawn_prompt_turn`, bypassing slash-command parsing.
    pub pending_channel_prompts: Vec<String>,
    /// In-app text selection state. Replaces terminal-native selection
    /// which drifts when the TUI scrolls (content changes under fixed
    /// screen coordinates). Selection coordinates are in screen-space
    /// with scroll compensation applied each frame.
    pub selection: rebon_tui::SelectionState,
    /// Which surface currently owns [`Self::selection`].
    pub selection_owner: SelectionOwner,
    /// Previous frame's scroll_offset, used to compute the delta for
    /// selection scroll compensation each frame.
    pub prev_scroll_offset: usize,
    /// When true, the next render frame should extract selected text
    /// from the buffer and copy it to clipboard via OSC 52, then
    /// clear the selection.
    pub pending_copy: bool,
    /// Last rendered prompt-input text area. Mouse hit-testing uses
    /// this so prompt selection matches the actual frame layout.
    pub last_prompt_input_area: Option<ratatui::layout::Rect>,
    /// Cached committed-transcript measurements reused across frames.
    pub transcript_measure_cache: rebon_tui::TranscriptMeasureCache,
    /// Text snapshot of the transcript area from the previous frame.
    /// Indexed by screen row (absolute). Used by `capture_scrolled_rows`
    /// to read content from rows that have since scrolled out of the
    /// viewport — ratatui clears the buffer before each frame so the
    /// old content is otherwise lost.
    pub prev_frame_lines: Vec<String>,
    /// The Rect of the transcript area in the previous frame, needed
    /// to interpret `prev_frame_lines` indices correctly.
    pub prev_frame_area: Option<ratatui::layout::Rect>,
    /// Currently-focused footer pill. `None` means prompt/transcript owns arrows.
    pub footer_selection: Option<FooterItem>,
    /// Currently-selected agent pill in the bottom `@main / @agent` row.
    /// Index 0 is always `main`; non-zero indices map to live local-agent
    /// or in-process teammate task pills in render order.
    pub teammate_footer_index: usize,
    /// Session-local status for the local `/ultraplan` workflow.
    /// This is presentation-only state; it does not persist mode,
    /// alter the global system prompt, or affect remote workflows.
    pub ultraplan_status: Option<UltraplanStatus>,
    /// Completed reviewer task ids already ingested into the active ultraplan run.
    pub ingested_ultraplan_reviewer_task_ids: HashSet<String>,
    /// Brief `/goal` input waiting for one clarifying user reply.
    pub pending_goal_clarification: Option<PendingGoalClarification>,
    /// Persistent local goal. When set, a SessionEnd hook/check runs
    /// after each completed turn and may continue in a fresh session.
    pub goal: Option<GoalState>,
    /// In-flight immediate `/compact`. Drives the progress widget and is
    /// cleared when the run finishes (either outcome).
    pub compact_run: Option<CompactRunState>,
}

/// Which part of an immediate `/compact` is running.
///
/// The summariser call is one long request with no intermediate signal, so
/// these are the only honest boundaries there are — the widget shows real
/// phases plus elapsed time rather than a fabricated per-percent trickle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactPhase {
    /// Reading the session transcript off disk.
    Reading,
    /// Waiting on the compact provider. Installing the result afterwards is
    /// in-memory bookkeeping that finishes inside one frame, so it gets no
    /// phase of its own — there would be nothing to see.
    Summarizing,
}

impl CompactPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reading => "Reading session history",
            Self::Summarizing => "Summarizing conversation",
        }
    }

    /// Fraction of the run this phase has completed by, as a 0-100 floor and
    /// ceiling. Summarizing owns nearly the whole bar because it owns nearly
    /// the whole wall clock.
    pub fn progress_bounds(self) -> (u16, u16) {
        match self {
            Self::Reading => (0, 8),
            Self::Summarizing => (8, 95),
        }
    }
}

/// An immediate `/compact` in flight.
#[derive(Debug, Clone)]
pub struct CompactRunState {
    pub phase: CompactPhase,
    pub started_at: std::time::Instant,
    /// Transcript rows the session had when the run started — the "N messages"
    /// the widget names while it waits.
    pub messages_before: usize,
    /// Foreground mailbox command awaiting this run's report, if the desktop
    /// app asked for it rather than the local user.
    pub respond_to_command_id: Option<String>,
}

/// Nominal duration of a compaction, used only to pace the progress bar
/// inside [`CompactPhase::Summarizing`]. Measured, not guessed: a
/// haiku-class summariser over a full context lands near here. The bar
/// never reaches the phase ceiling on time alone, so a slow run reads as
/// "still working" instead of "stuck at 100%".
pub const COMPACT_NOMINAL_DURATION: std::time::Duration = std::time::Duration::from_secs(30);

impl CompactRunState {
    /// Progress percentage to paint, 0-99. Never 100 — only a finished run
    /// is finished, and it clears this state rather than filling the bar.
    pub fn progress_percent(&self) -> u16 {
        let (floor, ceiling) = self.phase.progress_bounds();
        if self.phase != CompactPhase::Summarizing {
            return floor;
        }
        let elapsed = self.started_at.elapsed().as_millis() as u64;
        let nominal = COMPACT_NOMINAL_DURATION.as_millis() as u64;
        // Asymptotic: half the remaining span per nominal duration, so the
        // bar keeps creeping without ever claiming the phase is done.
        let span = u64::from(ceiling - floor);
        let advanced = span.saturating_mul(elapsed) / nominal.saturating_add(elapsed);
        floor.saturating_add(advanced.min(span) as u16)
    }
}

/// Records when a task was marked completed.
#[derive(Debug, Clone)]
pub struct TaskCompletionEntry {
    pub id: String,
    pub completed_at_ms: u64,
}

/// Which surface currently owns the in-app text selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOwner {
    /// The transcript viewport owns the selection.
    Transcript,
    /// The prompt input field owns the selection.
    PromptInput,
}

fn now_ms_for_app_state() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl AppState {
    /// A reading of this session's usage ledger.
    ///
    /// One lock, one owned copy: a caller that wants two of the numbers must
    /// not take the lock twice and risk showing halves of two different
    /// moments in one line.
    pub(crate) fn usage(&self) -> crate::session::usage::UsageSnapshot {
        self.usage_mut().snapshot()
    }

    /// The ledger, locked for writing. One place names the poison, so every
    /// reader and writer treats it the same way.
    pub(crate) fn usage_mut(
        &self,
    ) -> std::sync::MutexGuard<'_, crate::session::usage::UsageLedger> {
        self.usage_ledger.lock().expect("usage ledger poisoned")
    }

    pub(crate) fn replace_active_transcript_view(
        &mut self,
        next: StoredTranscriptView,
    ) -> StoredTranscriptView {
        let StoredTranscriptView { tui, measure_cache } = next;
        StoredTranscriptView {
            tui: std::mem::replace(&mut self.rebon_tui, tui),
            measure_cache: std::mem::replace(&mut self.transcript_measure_cache, measure_cache),
        }
    }

    pub(crate) fn startup_banner_is_empty(&self) -> bool {
        !self.is_loading
            && self.foregrounded_task_id.is_none()
            && self.rebon_tui.transcript.is_empty()
            && self.rebon_tui.overlay.is_empty()
    }

    pub(crate) fn refresh_empty_startup_banner(&mut self) -> bool {
        if !self.startup_banner_is_empty() {
            return false;
        }
        self.pending_inline_banner_refresh = true;
        true
    }

    pub(crate) fn reset_transcript_views(&mut self) {
        self.pending_inline_banner_refresh = false;
        drop(self.replace_active_transcript_view(StoredTranscriptView::default()));
        self.foregrounded_task_id = None;
        self.main_agent_view = None;
        self.local_agent_views.clear();
    }

    pub(crate) fn clear_all_transcript_measure_caches(&mut self) {
        self.transcript_measure_cache.clear();
        if let Some(main) = self.main_agent_view.as_mut() {
            main.measure_cache.clear();
        }
        for view in self.local_agent_views.values_mut() {
            view.measure_cache.clear();
        }
    }

    /// Fresh session state with an empty prompt and the default
    /// `"prompt"` mode.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::new_with_coordinator_mode(false)
    }

    pub fn new_with_coordinator_mode(coordinator_mode: bool) -> Self {
        Self {
            cwd: String::new(),
            usage_ledger: Arc::new(std::sync::Mutex::new(
                crate::session::usage::UsageLedger::new(now_ms_for_app_state()),
            )),
            input: String::new(),
            cursor_offset: 0,
            mode: String::from("prompt"),
            default_placeholder: Some(String::from("Type to start a session, Enter to submit")),
            contextual_tip: None,
            contextual_tip_key: String::new(),
            contextual_tip_index: 0,
            contextual_tip_next_rotate_at_ms: 0,
            rebon_tui: rebon_tui::AppState::default(),
            inline_tail_measure: Default::default(),
            inline_tail_measure_builds: std::cell::Cell::new(0),
            ui_mode: crate::ui_config::UiMode::Screen,
            math_rendering_mode: crate::rebon_config::MathRenderingMode::Off,
            pending_inline_viewport_reset: false,
            pending_inline_startup_banner: false,
            pending_inline_banner_refresh: false,
            pending_page_hard_refresh: None,
            pending_permission_view: None,
            command_expansion_tx: None,
            expanding_command: None,
            next_command_expansion_id: 0,
            inline_shell_commands: Default::default(),
            deferred_question_inbox: Default::default(),
            queued_commands: Vec::new(),
            queued_submit_payloads: Vec::new(),
            deferred_goal_submit_payloads: Vec::new(),
            deferred_internal_submit_payloads: Vec::new(),
            mid_turn_queued_submit_poller: None,
            queued_auto_drain_paused_after_withdrawal: false,
            undo_stack: Vec::new(),
            scroll_offset: 0,
            follow_transcript_tail: true,
            last_transcript_down_press_ms: 0,
            total_content_lines: 0,
            transcript_sticky_anchor: None,
            transcript_sticky_anchor_label: None,
            transcript_sticky_anchor_area: None,
            scroll_to_bottom_area: None,
            slash_commands: Vec::new(),
            slash_picker: None,
            file_index: FileIndex::default(),
            file_scan_status: crate::file_scanner::FileScanStatus::Scanning,
            at_mention_picker: None,
            tool_output_verbosity: ToolOutputVerbosity::default(),
            update_notice: None,
            prompt_top_hints: PromptTopHintRegistry::default(),
            next_paste_id: 1,
            pasted_contents: Vec::new(),
            history: Vec::new(),
            history_index: 0,
            saved_draft: None,
            saved_draft_pasted_contents: None,
            tasks: empty_task_projection(),
            foregrounded_task_id: None,
            main_agent_view: None,
            local_agent_views: HashMap::new(),
            background_tasks_dialog: None,
            agent_view: None,
            global_search_dialog: None,
            mcp_dialog: None,
            rc_status: RcStatusState::default(),
            goal_confirm_dialog: None,
            teams_dialog: None,
            resume_dialog: None,
            rewind_dialog: None,
            onboarding_dialog: None,
            dialogs: crate::tui::dialog_host::DialogHost::default(),
            task_completion_timestamps: Vec::new(),
            prev_task_snapshot: Vec::new(),
            task_hide_deadline_ms: None,
            task_list_collapsed: false,
            task_list_prev_count: 0,
            unseen_divider: UnseenDividerState::new(0),
            hidden_tool_call_ids: std::collections::HashSet::new(),
            suppress_late_visible_updates_after_withdrawal: false,
            pending_plan_mode_tool_ids: std::collections::HashMap::new(),
            background_agent_tool_tasks: std::collections::HashMap::new(),
            remote_background_tasks: std::collections::HashMap::new(),
            live_agent_tool_activity: std::collections::HashMap::new(),
            live_agent_tool_activity_revision: 0,
            is_pasting: false,
            paste_echo: None,
            permission_mode: PermissionMode::Default,
            permission_mode_cell: Arc::new(std::sync::Mutex::new(PermissionMode::Default)),
            pending_profile_runtime_refresh: None,
            auto_mode_denials: Arc::new(std::sync::Mutex::new(AutoModeDenialStore::default())),
            auto_mode_verdicts: Arc::new(rebon_permissions::AutoModeVerdictCache::default()),
            auto_mode_allowed_tool_ids: std::collections::HashMap::new(),
            vim_mode: None,
            initial_vim_mode: None,
            custom_status_line:
                crate::tui::runner::custom_status_line::CustomStatusLineState::default(),
            spinner_verb: String::from("Thinking"),
            spinner_verb_before_compacting: None,
            was_loading: false,
            help_open: false,
            help_tab_index: 0,
            speculation_active: false,
            side_question_visible: false,
            prompt_suggestion_active: false,
            stash_hint_dismissed: false,
            option_meta_hint_toast: None,
            last_esc_press_ms: 0,
            last_left_press_ms: 0,
            last_ctrl_c_exit_press_ms: 0,
            plan_entries: Vec::new(),
            config_options: Vec::new(),
            session_title: None,
            pending_routed_model: None,
            is_loading: false,
            prompt_completion_status: None,
            retry_info: None,
            streaming_token_count: 0,
            queued_command_up_hint_count: 0,
            coordinator_mode,
            effort_level: None,
            effort_provider_kind: EffortProviderKind::Anthropic,
            pending_teammate_prompts: Vec::new(),
            pending_channel_prompts: Vec::new(),
            selection: rebon_tui::SelectionState::new(),
            selection_owner: SelectionOwner::Transcript,
            prev_scroll_offset: 0,
            pending_copy: false,
            last_prompt_input_area: None,
            transcript_measure_cache: rebon_tui::TranscriptMeasureCache::new(),
            prev_frame_lines: Vec::new(),
            prev_frame_area: None,
            footer_selection: None,
            teammate_footer_index: 0,
            ultraplan_status: None,
            ingested_ultraplan_reviewer_task_ids: HashSet::new(),
            pending_goal_clarification: None,
            goal: None,
            compact_run: None,
        }
    }

    pub fn set_update_notice(&mut self, notice: UpdateNoticeState) {
        let text = format!(
            "Update available: rebon {} — /update",
            notice.latest_version
        );
        self.update_notice = Some(notice);
        // An available update is a notice, not a problem: Info renders in the
        // brand slate instead of the warning yellow reserved for real faults.
        self.prompt_top_hints.register(PromptTopHint::new(
            UPDATE_PROMPT_TOP_HINT_ID,
            text,
            100,
            PromptTopHintTone::Info,
        ));
    }

    pub fn clear_update_notice(&mut self) -> Option<UpdateNoticeState> {
        self.prompt_top_hints.remove(UPDATE_PROMPT_TOP_HINT_ID);
        self.update_notice.take()
    }

    pub fn set_mcp_load_hint(&mut self, text: impl Into<String>) {
        self.prompt_top_hints.register(PromptTopHint::new(
            MCP_PROMPT_TOP_HINT_ID,
            text,
            90,
            PromptTopHintTone::Warning,
        ));
    }

    pub fn clear_mcp_load_hint(&mut self) -> Option<PromptTopHint> {
        self.prompt_top_hints.remove(MCP_PROMPT_TOP_HINT_ID)
    }

    pub fn idle_prompt_top_hint(&self) -> Option<&PromptTopHint> {
        self.prompt_top_hints.resolve(PromptTopHintInput {
            prompt_empty: self.input.is_empty(),
            output_complete: !self.is_loading,
            modal_open: self.has_modal_overlay(),
        })
    }

    /// Cycle permission mode: Default → Plan → Accept edits → Auto → Default.
    ///
    /// `BypassPermissions` remains a valid mode for explicit
    /// configuration, but Shift+Tab does not enter it — bypassing all
    /// permission checks should never be one keypress away.
    ///
    /// The catch-all falls back to `Default`, so legacy entries like
    /// `Bubble`/`DontAsk` and off-cycle modes reset cleanly.
    pub fn cycle_permission_mode(&mut self) {
        let next = match self.permission_mode {
            PermissionMode::Default => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Default,
            _ => PermissionMode::Default,
        };
        self.set_permission_mode(next);
    }

    /// Replace the task rows rendered by this terminal with one exact-session
    /// registry snapshot. This copies view data only; runtime operations keep
    /// using the registry owned by `SessionEngineHalf`.
    pub fn sync_task_snapshots(&mut self, snapshots: Vec<TaskSnapshot>) {
        #[cfg(test)]
        {
            let _ = snapshots;
        }
        #[cfg(not(test))]
        {
            self.tasks = snapshots;
        }
    }

    /// Current task rows for UI projection.
    pub fn task_snapshots(&self) -> Vec<TaskSnapshot> {
        #[cfg(test)]
        {
            self.tasks.snapshots()
        }
        #[cfg(not(test))]
        {
            self.tasks.clone()
        }
    }

    /// Look up one in-process task in the current UI projection.
    pub fn task_snapshot(&self, task_id: &TaskId) -> Option<TaskSnapshot> {
        #[cfg(test)]
        {
            self.tasks.snapshot(task_id)
        }
        #[cfg(not(test))]
        {
            self.tasks
                .iter()
                .find(|snapshot| snapshot.id == *task_id)
                .cloned()
        }
    }

    /// Task snapshots driving the session-scoped agent UI (bottom switcher,
    /// footer status, live-agent views): the exact session projection plus,
    /// when this TUI proxies a cross-process background worker, the tasks
    /// projected from its persisted live events.
    pub fn agent_task_snapshots(&self) -> Vec<TaskSnapshot> {
        let mut snapshots = self.task_snapshots();
        for remote in self.remote_background_tasks.values() {
            if snapshots
                .iter()
                .any(|snapshot| snapshot.id.as_str() == remote.task.task_id)
            {
                continue;
            }
            snapshots.push(crate::tui::agent_switcher::remote_background_task_snapshot(
                remote,
            ));
        }
        snapshots
    }

    /// Look up one agent task for the view/switch surfaces: the in-process
    /// registry first, then the attached remote worker's projection.
    pub fn agent_task_snapshot(&self, task_id: &str) -> Option<TaskSnapshot> {
        self.task_snapshot(&TaskId::new(task_id)).or_else(|| {
            self.remote_background_tasks
                .get(task_id)
                .map(crate::tui::agent_switcher::remote_background_task_snapshot)
        })
    }

    /// True when `task_id` is only known through the attached remote worker
    /// (no in-process registry entry). Such agents are viewed read-only.
    pub fn is_remote_agent_task(&self, task_id: &str) -> bool {
        self.remote_background_tasks.contains_key(task_id)
            && self.task_snapshot(&TaskId::new(task_id)).is_none()
    }

    /// Single write point for `permission_mode`. Also matches the value
    /// into `permission_mode_cell` so the engine's broker observes the
    /// change on the next tool dispatch — the broker cannot read
    /// `AppState` directly (see field docs). Prefer this setter over
    /// assigning `permission_mode` directly so the mirror never drifts.
    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        if self.permission_mode != mode {
            self.refresh_empty_startup_banner();
        }
        self.permission_mode = mode;
        {
            let mut guard = self
                .permission_mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = mode;
        }
    }

    /// Whether a full-screen dialog currently owns input focus.
    ///
    /// When `true`, `plan_input_event` short-circuits to its default
    /// no-op — keyboard events should not reach the prompt surface.
    /// Does **not** include prompt-level overlays (`help_open`,
    /// `side_question_visible`) because those still allow partial
    /// prompt interaction (e.g. Esc to close).
    pub fn has_fullscreen_dialog(&self) -> bool {
        self.pending_permission_view.is_some()
            || self.background_tasks_dialog.is_some()
            || self.agent_view.is_some()
            || self.global_search_dialog.is_some()
            || self.goal_confirm_dialog.is_some()
            || self.teams_dialog.is_some()
            || self.resume_dialog.is_some()
            || self.rewind_dialog.is_some()
            || self.mcp_dialog.is_some()
            || self.dialogs.top_is_fullscreen()
            || self.onboarding_dialog.is_some()
    }

    /// Whether any full-screen/modal overlay currently owns input focus.
    pub fn has_modal_overlay(&self) -> bool {
        self.has_fullscreen_dialog() || self.side_question_visible
    }

    /// Build a [`PromptInputRuntimeInput`] for this frame.
    ///
    /// The optional fields this frame has no value for are left at
    /// their well-defined "off" values per `rebon_tui::promptinput`'s
    /// state-machine contracts, so the derived runtime state is
    /// meaningful with only the fields copied below filled in.
    pub fn build_runtime_input(&self) -> PromptInputRuntimeInput {
        let has_editable = self
            .queued_commands
            .iter()
            .any(|cmd| matches!(cmd.value, QueuedCommandValue::Text(_)));
        let resolved_placeholder = resolve_prompt_input_placeholder(&PromptPlaceholderInput {
            input: self.input.clone(),
            submit_count: self.history.len() as u64,
            viewing_agent_name: None,
            has_editable_queued_commands: has_editable,
            queued_command_up_hint_count: self.queued_command_up_hint_count,
            prompt_suggestion_enabled: false,
            proactive_active: false,
            contextual_tip: self.contextual_tip.clone(),
            example_command: None,
        });
        let (slash_command_triggers, special_slash_command_triggers) =
            prompt_slash_command_highlights(&self.input, &self.slash_commands);
        PromptInputRuntimeInput {
            mode: self.mode.clone(),
            input: self.input.clone(),
            history_match_display: None,
            is_searching_history: false,
            is_modal_overlay_active: self.has_modal_overlay(),
            footer_item_selected: self.slash_picker.is_some() || self.at_mention_picker.is_some(),
            cursor_offset: self.cursor_offset,
            suggestion_count: 0,
            default_placeholder: resolved_placeholder.or_else(|| self.default_placeholder.clone()),
            prompt_suggestion: None,
            prompt_suggestion_state: PromptSuggestionState {
                text: None,
                shown_at: 0,
            },
            viewing_agent_task_id_present: self.foregrounded_task_id.is_some(),
            can_undo: !self.undo_stack.is_empty(),
            history_failed_match: false,
            history_query_length: 0,
            btw_triggers: vec![],
            slash_command_triggers,
            token_budget_triggers: vec![],
            slack_channel_triggers: vec![],
            member_mention_highlights: vec![],
            voice_interim_range: None,
            think_triggers: vec![],
            ultraplan_triggers: special_slash_command_triggers,
            ultrareview_triggers: vec![],
            ultrathink_enabled: false,
            ultraplan_enabled: true,
        }
    }

    pub fn update_contextual_tip(&mut self, now_ms: u64) {
        let candidates = crate::tui::prompt_tips::candidates_for_app(self);
        self.contextual_tip = crate::tui::prompt_tips::rotate_tip(
            &mut self.contextual_tip_key,
            &mut self.contextual_tip_index,
            &mut self.contextual_tip_next_rotate_at_ms,
            now_ms,
            &candidates,
        );
    }
}

#[cfg(test)]
impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Pure-logic helpers wired from UI crates ────────────────────

fn prompt_slash_command_highlights(
    input: &str,
    commands: &[SlashCommand],
) -> (Vec<TextRange>, Vec<TextRange>) {
    let Some((command_name, range)) = leading_slash_command(input) else {
        return (Vec::new(), Vec::new());
    };
    if is_special_slash_command_name(command_name) {
        return (Vec::new(), vec![range]);
    }

    let Some(command) = commands
        .iter()
        .find(|command| command.matches_name_or_alias(command_name))
    else {
        return (vec![range], Vec::new());
    };

    if is_special_slash_command(command) {
        (Vec::new(), vec![range])
    } else {
        (vec![range], Vec::new())
    }
}

fn leading_slash_command(input: &str) -> Option<(&str, TextRange)> {
    let start = input.find(|ch: char| !ch.is_whitespace())?;
    let rest = &input[start..];
    let command_start = rest.strip_prefix('/')?;
    let name_len = command_start
        .find(char::is_whitespace)
        .unwrap_or(command_start.len());
    if name_len == 0 {
        return None;
    }
    let end = start + 1 + name_len;
    Some((&input[start + 1..end], TextRange { start, end }))
}

fn is_special_slash_command(command: &SlashCommand) -> bool {
    is_special_slash_command_name(&command.name)
}

fn is_special_slash_command_name(name: &str) -> bool {
    matches!(name, "ultraplan" | "grill" | "ultrawork" | "ulw" | "ceo")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warm_measure_cache(
        state: &mut rebon_tui::AppState,
        cache: &mut rebon_tui::TranscriptMeasureCache,
        uuid: &str,
    ) {
        state
            .transcript
            .push(rebon_tui::Message::System(rebon_tui::SystemMessage {
                uuid: uuid.into(),
                timestamp: "t".into(),
                subtype: "cache".into(),
                content: Some("cache row".into()),
                level: Some(rebon_tui::SystemLevel::Info),
                is_meta: None,
            }));
        let area = ratatui::layout::Rect::new(0, 0, 40, 8);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        rebon_tui::render_transcript_cached_with_running_hints(
            state,
            area,
            &mut buffer,
            &rebon_tui::RenderTheme::default(),
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            cache,
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        assert!(!cache.is_empty());
    }

    #[test]
    fn stored_transcript_view_clone_starts_with_cold_measurements() {
        let mut view = StoredTranscriptView::default();
        warm_measure_cache(&mut view.tui, &mut view.measure_cache, "stored");

        let cloned = view.clone();

        assert!(!view.measure_cache.is_empty());
        assert!(cloned.measure_cache.is_empty());
        assert_eq!(cloned.tui.transcript.rows(), view.tui.transcript.rows());
    }

    #[test]
    fn clear_all_transcript_measure_caches_clears_active_main_and_agents() {
        let mut app = AppState::new();
        warm_measure_cache(
            &mut app.rebon_tui,
            &mut app.transcript_measure_cache,
            "active",
        );
        let mut main = StoredTranscriptView::default();
        warm_measure_cache(&mut main.tui, &mut main.measure_cache, "main");
        app.main_agent_view = Some(main);
        let mut agent = StoredTranscriptView::default();
        warm_measure_cache(&mut agent.tui, &mut agent.measure_cache, "agent");
        app.local_agent_views.insert("agent-1".into(), agent);

        app.clear_all_transcript_measure_caches();

        assert!(app.transcript_measure_cache.is_empty());
        assert!(app
            .main_agent_view
            .as_ref()
            .expect("main view")
            .measure_cache
            .is_empty());
        assert!(app
            .local_agent_views
            .get("agent-1")
            .expect("agent view")
            .measure_cache
            .is_empty());
    }

    /// The Shift+Tab cycle walks Default → Plan → Accept edits → Auto.
    /// `BypassPermissions` remains an explicit mode but is intentionally
    /// skipped by the keyboard cycle.
    #[test]
    fn cycle_permission_mode_walks_through_plan_accept_auto_then_default() {
        let mut app = AppState::new();
        assert_eq!(app.permission_mode, PermissionMode::Default);
        let sequence = [
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::Default,
        ];
        for expected in sequence {
            app.cycle_permission_mode();
            assert_eq!(app.permission_mode, expected);
        }
    }

    #[test]
    fn cycle_permission_mode_refreshes_empty_startup_banner() {
        let mut app = AppState::new();
        app.ui_mode = crate::ui_config::UiMode::Inline;
        app.cycle_permission_mode();
        assert!(app.pending_inline_banner_refresh);
        assert!(app.rebon_tui.transcript.is_empty());
    }

    #[test]
    fn startup_banner_refresh_survives_dialog_screen_mode() {
        for ui_mode in [
            crate::ui_config::UiMode::Inline,
            crate::ui_config::UiMode::Screen,
        ] {
            let mut app = AppState::new();
            app.ui_mode = ui_mode;
            assert!(app.refresh_empty_startup_banner());
            assert!(app.pending_inline_banner_refresh);
            assert!(app.refresh_empty_startup_banner());
            assert!(app.rebon_tui.transcript.is_empty());
        }
    }

    #[test]
    fn startup_banner_refresh_does_not_replace_existing_content() {
        let mut app = AppState::new();
        warm_measure_cache(
            &mut app.rebon_tui,
            &mut app.transcript_measure_cache,
            "existing",
        );
        app.cycle_permission_mode();
        assert!(!app.pending_inline_banner_refresh);
        assert!(!app.refresh_empty_startup_banner());
        assert_eq!(app.rebon_tui.transcript.rows().len(), 1);
    }

    #[test]
    fn startup_banner_refresh_does_not_replace_running_or_agent_view() {
        let mut app = AppState::new();
        app.is_loading = true;
        assert!(!app.refresh_empty_startup_banner());
        app.is_loading = false;
        app.foregrounded_task_id = Some("agent-1".into());
        assert!(!app.refresh_empty_startup_banner());
        assert!(!app.pending_inline_banner_refresh);
    }

    #[test]
    fn startup_banner_refresh_ignores_unchanged_mode_and_resets_for_new_session() {
        let mut app = AppState::new();
        app.set_permission_mode(app.permission_mode);
        assert!(!app.pending_inline_banner_refresh);
        app.cycle_permission_mode();
        assert!(app.pending_inline_banner_refresh);
        app.reset_transcript_views();
        assert!(!app.pending_inline_banner_refresh);
    }

    #[test]
    fn cycle_permission_mode_resets_off_cycle_modes() {
        for mode in [
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
            PermissionMode::Bubble,
        ] {
            let mut app = AppState::new();
            app.permission_mode = mode;
            app.cycle_permission_mode();
            assert_eq!(
                app.permission_mode,
                PermissionMode::Default,
                "mode={mode:?}"
            );
        }
    }

    /// Shift+Tab cycling must mirror the mode into `permission_mode_cell`
    /// so the engine broker's `PermissionModeProvider` observes the new
    /// mode. Without this mirror the TUI's visible mode drifts from the
    /// mode the broker acts on — Shift+Tab would appear to do nothing
    /// as far as auto-mode short-circuiting is concerned.
    #[test]
    fn cycle_mode_mirrors_into_shared_cell() {
        let mut app = AppState::new();
        let cell = app.permission_mode_cell.clone();
        assert_eq!(*cell.lock().unwrap(), PermissionMode::Default);
        app.cycle_permission_mode();
        assert_eq!(*cell.lock().unwrap(), app.permission_mode);
        for _ in 0..2 {
            app.cycle_permission_mode();
            assert_eq!(*cell.lock().unwrap(), app.permission_mode);
        }
    }

    #[test]
    fn contextual_tip_reaches_runtime_placeholder() {
        let mut app = AppState::new();
        app.contextual_tip = Some("Tip: context".into());
        let runtime = app.build_runtime_input();
        assert_eq!(runtime.default_placeholder, Some("Tip: context".into()));
    }

    #[test]
    fn contextual_tip_does_not_override_non_empty_input() {
        let mut app = AppState::new();
        app.input = "hello".into();
        app.cursor_offset = app.input.len();
        app.contextual_tip = Some("Tip: context".into());
        let runtime = app.build_runtime_input();
        assert_eq!(
            runtime.default_placeholder,
            Some("Type to start a session, Enter to submit".into())
        );
    }

    #[test]
    fn set_permission_mode_updates_cell_directly() {
        let mut app = AppState::new();
        let cell = app.permission_mode_cell.clone();
        app.set_permission_mode(PermissionMode::Auto);
        assert_eq!(app.permission_mode, PermissionMode::Auto);
        assert_eq!(*cell.lock().unwrap(), PermissionMode::Auto);
    }

    #[test]
    fn help_overlay_does_not_steal_prompt_focus() {
        let mut app = AppState::new();
        app.help_open = true;

        assert!(!app.has_fullscreen_dialog());
        assert!(!app.has_modal_overlay());
        assert!(!app.build_runtime_input().is_modal_overlay_active);
    }

    #[test]
    fn slash_commands_are_highlighted_in_prompt_runtime_input() {
        let mut app = AppState::new();
        app.slash_commands = vec![SlashCommand {
            name: "help".into(),
            description: "Show help".into(),
            input: None,
            category: None,
            aliases: Vec::new(),
        }];
        app.input = "/help now".into();
        app.cursor_offset = app.input.len();

        let runtime = app.build_runtime_input();

        assert_eq!(
            runtime.slash_command_triggers,
            vec![TextRange { start: 0, end: 5 }]
        );
        assert!(runtime.ultraplan_triggers.is_empty());
    }

    #[test]
    fn leading_slash_words_are_highlighted_without_command_metadata() {
        let mut app = AppState::new();
        app.slash_commands.clear();
        app.input = "/help now".into();
        app.cursor_offset = app.input.len();

        let runtime = app.build_runtime_input();

        assert_eq!(
            runtime.slash_command_triggers,
            vec![TextRange { start: 0, end: 5 }]
        );
        assert!(runtime.ultraplan_triggers.is_empty());
    }

    #[test]
    fn special_slash_commands_use_rainbow_highlights() {
        let mut app = AppState::new();
        app.slash_commands = vec![
            SlashCommand {
                name: "ultraplan".into(),
                description: "Plan".into(),
                input: None,
                category: None,
                aliases: Vec::new(),
            },
            SlashCommand {
                name: "ultrawork".into(),
                description: "Work".into(),
                input: None,
                category: None,
                aliases: vec!["ulw".into()],
            },
        ];

        app.input = "  /ultraplan build it".into();
        app.cursor_offset = app.input.len();
        let runtime = app.build_runtime_input();
        assert!(runtime.slash_command_triggers.is_empty());
        assert_eq!(
            runtime.ultraplan_triggers,
            vec![TextRange { start: 2, end: 12 }]
        );
        assert!(runtime.ultraplan_enabled);

        app.input = "/grill build it".into();
        app.cursor_offset = app.input.len();
        let runtime = app.build_runtime_input();
        assert!(runtime.slash_command_triggers.is_empty());
        assert_eq!(
            runtime.ultraplan_triggers,
            vec![TextRange { start: 0, end: 6 }]
        );

        app.input = "/ulw build it".into();
        app.cursor_offset = app.input.len();
        let runtime = app.build_runtime_input();
        assert_eq!(
            runtime.ultraplan_triggers,
            vec![TextRange { start: 0, end: 4 }]
        );
    }

    #[test]
    fn special_slash_commands_use_rainbow_highlights_without_metadata() {
        let mut app = AppState::new();
        app.slash_commands.clear();

        app.input = "/ultrawork build it".into();
        app.cursor_offset = app.input.len();
        let runtime = app.build_runtime_input();
        assert!(runtime.slash_command_triggers.is_empty());
        assert_eq!(
            runtime.ultraplan_triggers,
            vec![TextRange { start: 0, end: 10 }]
        );

        app.input = "  /ultraplan build it".into();
        app.cursor_offset = app.input.len();
        let runtime = app.build_runtime_input();
        assert!(runtime.slash_command_triggers.is_empty());
        assert_eq!(
            runtime.ultraplan_triggers,
            vec![TextRange { start: 2, end: 12 }]
        );

        app.input = "/ulw build it".into();
        app.cursor_offset = app.input.len();
        let runtime = app.build_runtime_input();
        assert!(runtime.slash_command_triggers.is_empty());
        assert_eq!(
            runtime.ultraplan_triggers,
            vec![TextRange { start: 0, end: 4 }]
        );
    }

    #[test]
    fn context_dialog_counts_as_fullscreen_modal() {
        use rebon_dialog::context_dialog::ContextDialogState;

        let mut app = AppState::new();
        app.dialogs
            .push(ContextDialogState::open("context usage", Vec::new()));

        assert!(app.has_fullscreen_dialog());
        assert!(app.has_modal_overlay());
    }

    #[test]
    fn a_fullscreen_dialog_gates_prompt_input() {
        let mut app = AppState::new();
        app.dialogs
            .push(rebon_dialog::doctor_dialog::DoctorDialogState::open(
                &rebon_dialog::doctor_dialog::DoctorReport {
                    summary: Vec::new(),
                    sections: Vec::new(),
                },
            ));

        assert!(app.has_fullscreen_dialog());
        assert!(app.has_modal_overlay());
        assert!(app.build_runtime_input().is_modal_overlay_active);
    }

    #[test]
    fn effort_dialog_counts_as_fullscreen_modal() {
        let mut app = AppState::new();
        app.dialogs
            .push(rebon_dialog::effort_dialog::EffortDialogState::open(
                "model", None,
            ));

        assert!(app.has_fullscreen_dialog());
        assert!(app.has_modal_overlay());
        assert!(app.build_runtime_input().is_modal_overlay_active);
    }

    #[test]
    fn closing_the_hosted_dialog_clears_the_fullscreen_flag() {
        let mut app = AppState::new();
        app.dialogs
            .push(rebon_dialog::hooks_dialog::HooksDialogState::open(
                &rebon_dialog::hooks_dialog::HooksDialogInput::default(),
            ));
        assert!(app.has_fullscreen_dialog());
        // The predicate reads the stack, so popping it answers `false`
        // without a chain entry to remember to remove.
        app.dialogs.close_all();
        assert!(!app.has_fullscreen_dialog());
    }
}
