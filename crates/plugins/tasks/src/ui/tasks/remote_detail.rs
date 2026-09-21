//! Remote session detail dialog projection.
//!
//! This file contains pure pieces for the remote session detail model:
//!
//! 1. [`format_tool_use_summary`] — compact
//!    one-line summary of a remote-side tool call. Used by both the
//!    Ultraplan and Review session detail panes.
//! 2. [`phase_label`] / [`agent_verb`] — the Ultraplan phase-label and
//!    verb tables (`needs_input` / `plan_ready`).
//! 3. [`stage_label`] — the review-pipeline stage labels
//!    (`finding` → `Find`, `verifying` → `Verify`, `synthesizing` →
//!    `Dedupe`).
//! 4. [`build_stage_pipeline`] — returns a discriminated
//!    list of pipeline cells.
//! 5. [`review_counts_line`] — the review counts line.
//! 6. [`RemoteDetailState`] — the confirm-stop / menu reducer
//!    (the `Menu` / `ConfirmingStop` state machine).
//! 7. [`build_ultraplan_status_text`] — the Ultraplan status text.
//! 8. [`scan_ultraplan_session_log`] — pure fold over a list of
//!    log blocks that counts spawns / tool calls and
//!    captures the last tool-use block. Modeled as a fold over a
//!    pre-built list of `UltraplanLogBlock` values so the model
//!    doesn't need to know the full ACP message shape.
//!
//! The actual widget, elapsed-time helper, dialog, and
//! browser-open calls are the consumer's responsibility.

use crate::ui::tasks::common::{ReviewStage, TaskStatus};
use crate::ui::tasks::dream_detail::plural;
use crate::ui::tasks::remote_progress::{format_review_stage_counts, ReviewProgressInput};

/// Tool name for the ExitPlanMode v2 tool. Pinned here so the
/// model doesn't import the tools-core surface.
pub const EXIT_PLAN_MODE_V2_TOOL_NAME: &str = "ExitPlanMode";

/// Tool name for the AskUserQuestion tool.
pub const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";

/// Agent tool names.
pub const AGENT_TOOL_NAMES: [&str; 2] = ["Agent", "Task"];

/// Phase label table for the Ultraplan progress line.
pub fn phase_label(phase: &str) -> Option<&'static str> {
    match phase {
        "needs_input" => Some("input required"),
        "plan_ready" => Some("ready"),
        _ => None,
    }
}

/// Agent verb table.
pub fn agent_verb(phase: Option<&str>) -> &'static str {
    match phase {
        Some("needs_input") => "waiting",
        Some("plan_ready") => "done",
        _ => "working",
    }
}

/// Stage label table for the review pipeline.
pub fn stage_label(stage: ReviewStage) -> &'static str {
    match stage {
        ReviewStage::Finding => "Find",
        ReviewStage::Verifying => "Verify",
        ReviewStage::Synthesizing => "Dedupe",
    }
}

/// Compact one-line summary of a remote-side tool call.
///
/// Special-cases:
///
/// * `EXIT_PLAN_MODE_V2_TOOL_NAME` → `"Review the plan in Rebon on
///   the web"`.
/// * `ASK_USER_QUESTION_TOOL_NAME` with a `questions[0].question` or
///   `questions[0].header` field → `"Answer in browser: {q (≤50ch)}"`.
/// * Otherwise: pick the first non-empty string field of `input` and
///   render `"{name} {value (≤60ch)}"`.
/// * Fall through: `name`.
///
/// The model doesn't have a JSON value model, so the input is
/// modeled as a `ToolUseInput` enum the consumer constructs.
pub fn format_tool_use_summary(name: &str, input: &ToolUseInput) -> String {
    if name == EXIT_PLAN_MODE_V2_TOOL_NAME {
        return "Review the plan in Rebon on the web".to_owned();
    }
    if name == ASK_USER_QUESTION_TOOL_NAME {
        if let Some(q) = input.first_question_text() {
            let one_line = collapse_whitespace(&q);
            let trimmed = truncate_to_width(&one_line, 50);
            return format!("Answer in browser: {trimmed}");
        }
    }
    if let Some(v) = input.first_string_field() {
        if !v.trim().is_empty() {
            let one_line = collapse_whitespace(v);
            let trimmed = truncate_to_width(&one_line, 60);
            return format!("{name} {trimmed}");
        }
    }
    name.to_owned()
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

fn truncate_to_width(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

/// Pre-built tool-use input. The consumer constructs this from the
/// raw JSON value before calling [`format_tool_use_summary`].
#[derive(Debug, Clone, Default)]
pub struct ToolUseInput {
    /// First non-empty string field of the input object, in the order
    /// the object lists its own fields. `None` when no string field
    /// exists.
    string_fields: Vec<String>,
    /// `input.questions[0].question` or `header`, if present.
    first_question: Option<String>,
}

impl ToolUseInput {
    /// Empty input — [`format_tool_use_summary`] will fall back to the
    /// tool name.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from explicit string fields, preserving their order.
    pub fn from_string_fields(values: Vec<String>) -> Self {
        Self {
            string_fields: values,
            first_question: None,
        }
    }

    /// Set the AskUserQuestion's first-question text.
    pub fn with_first_question(mut self, q: String) -> Self {
        self.first_question = Some(q);
        self
    }

    /// First non-empty string field, used for the generic
    /// `{name} {value}` form.
    pub fn first_string_field(&self) -> Option<&str> {
        self.string_fields
            .iter()
            .map(String::as_str)
            .find(|v| !v.trim().is_empty())
    }

    /// Question text for the AskUserQuestion form.
    pub fn first_question_text(&self) -> Option<String> {
        self.first_question.clone()
    }
}

/// Pre-built input for [`build_stage_pipeline`].
#[derive(Debug, Clone, Copy)]
pub struct StagePipelineInput {
    /// Current stage. `None` when
    /// no progress yet.
    pub stage: Option<ReviewStage>,
    /// True when the session status is `Completed`.
    pub completed: bool,
    /// True when review progress exists at all (used to
    /// distinguish "Setup" from "Find").
    pub has_progress: bool,
}

/// One pipeline cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagePipelineCell {
    /// Cell label (`Setup`, `Find`, `Verify`, `Dedupe`).
    pub label: String,
    /// True when this cell is the *current* stage.
    pub is_current: bool,
}

/// Result of [`build_stage_pipeline`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagePipeline {
    /// Cells in display order: `Setup`, `Find`, `Verify`, `Dedupe`.
    pub cells: Vec<StagePipelineCell>,
    /// True when the trailing green ✓ should be drawn.
    pub completed_check: bool,
}

/// Project the review-pipeline display.
pub fn build_stage_pipeline(input: StagePipelineInput) -> StagePipeline {
    let in_setup = !input.completed && !input.has_progress;
    let mut cells: Vec<StagePipelineCell> = Vec::with_capacity(4);

    cells.push(StagePipelineCell {
        label: "Setup".into(),
        is_current: in_setup,
    });

    let stages = [
        (ReviewStage::Finding, "Find"),
        (ReviewStage::Verifying, "Verify"),
        (ReviewStage::Synthesizing, "Dedupe"),
    ];
    for (stage, label) in stages {
        let is_current = !input.completed && !in_setup && input.stage == Some(stage);
        cells.push(StagePipelineCell {
            label: label.into(),
            is_current,
        });
    }

    StagePipeline {
        cells,
        completed_check: input.completed,
    }
}

/// Pre-built input for [`review_counts_line`].
#[derive(Debug, Clone)]
pub struct ReviewCountsInput {
    /// The session status.
    pub status: TaskStatus,
    /// Pre-built review progress.
    pub review: Option<ReviewProgressInput>,
}

/// Render the counts line for the review session.
///
/// Behaviour:
///
/// * No progress data → `"done"` (when completed) / `"setting up"`.
/// * Completed with progress → `"{verified} finding[s][ · {refuted}
///   refuted]"`.
/// * Otherwise → delegate to
///   [`crate::ui::tasks::remote_progress::format_review_stage_counts`].
pub fn review_counts_line(input: &ReviewCountsInput) -> String {
    let Some(p) = input.review.as_ref() else {
        return if input.status == TaskStatus::Completed {
            "done".to_owned()
        } else {
            "setting up".to_owned()
        };
    };
    let verified = p.bugs_verified;
    let refuted = p.bugs_refuted;
    if input.status == TaskStatus::Completed {
        let mut parts: Vec<String> = vec![format!("{verified} {}", plural(verified, "finding"))];
        if refuted > 0 {
            parts.push(format!("{refuted} refuted"));
        }
        return parts.join(" · ");
    }
    format_review_stage_counts(p.stage, p.bugs_found, verified, refuted)
}

/// Build the Ultraplan status text.
pub fn build_ultraplan_status_text(status: TaskStatus, phase: Option<&str>) -> String {
    let running = matches!(status, TaskStatus::Running | TaskStatus::Pending);
    if running {
        if let Some(p) = phase {
            if let Some(label) = phase_label(p) {
                return label.to_owned();
            }
        }
        return "running".to_owned();
    }
    status.as_str().to_owned()
}

/// One block in an Ultraplan session log message. Used by
/// [`scan_ultraplan_session_log`] without exposing the model to the
/// full ACP message shape.
#[derive(Debug, Clone)]
pub struct UltraplanLogBlock {
    /// Tool name (e.g. `"Bash"`, `"Agent"`).
    pub tool_name: String,
    /// Tool input — already projected to a [`ToolUseInput`].
    pub input: ToolUseInput,
}

/// Result of [`scan_ultraplan_session_log`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltraplanScan {
    /// `1 + spawns` — the +1 accounts for the leader agent.
    pub agents_working: u64,
    /// Total tool-use blocks in the log.
    pub tool_calls: u64,
    /// Pre-projected last-tool-use summary, ready for display.
    pub last_tool_call: Option<String>,
}

/// Pure form of the for-loop.
pub fn scan_ultraplan_session_log(blocks: &[UltraplanLogBlock]) -> UltraplanScan {
    let mut spawns: u64 = 0;
    let mut calls: u64 = 0;
    let mut last_block_idx: Option<usize> = None;
    for (i, block) in blocks.iter().enumerate() {
        calls += 1;
        last_block_idx = Some(i);
        if AGENT_TOOL_NAMES.contains(&block.tool_name.as_str()) {
            spawns += 1;
        }
    }
    let last_tool_call = last_block_idx.map(|i| {
        let block = &blocks[i];
        format_tool_use_summary(&block.tool_name, &block.input)
    });
    UltraplanScan {
        agents_working: 1 + spawns,
        tool_calls: calls,
        last_tool_call,
    }
}

/// Confirm-stop reducer state for the remote session detail dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDetailState {
    /// Showing the main menu (open / stop / back).
    Menu,
    /// Showing the "Stop ultrareview / ultraplan?" confirmation
    /// dialog.
    ConfirmingStop,
}

impl Default for RemoteDetailState {
    fn default() -> Self {
        RemoteDetailState::Menu
    }
}

/// Menu action (the option value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// `'open'` — open the session in Rebon on the web.
    Open,
    /// `'stop'` — show the confirm-stop dialog.
    Stop,
    /// `'back'` — go back to the parent.
    Back,
    /// `'dismiss'` — close the dialog.
    Dismiss,
}

/// Action emitted by the confirm-stop reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDetailAction {
    /// Open the session URL in the browser then finish.
    OpenInBrowser,
    /// Enter `ConfirmingStop` state.
    EnterConfirmingStop,
    /// Confirm-stop accepted: kill the session then go back / close.
    Kill,
    /// Confirm-stop cancelled: leave `ConfirmingStop` state.
    LeaveConfirmingStop,
    /// `back` action: go back to the parent.
    Back,
    /// `dismiss`: finish directly.
    Dismiss,
    /// Event ignored.
    Ignore,
}

/// Reducer for the remote session detail dialog.
pub fn handle_remote_detail_event(
    state: RemoteDetailState,
    action: MenuAction,
    can_kill: bool,
) -> (RemoteDetailState, RemoteDetailAction) {
    match (state, action) {
        (RemoteDetailState::Menu, MenuAction::Open) => {
            (RemoteDetailState::Menu, RemoteDetailAction::OpenInBrowser)
        }
        (RemoteDetailState::Menu, MenuAction::Stop) => {
            if can_kill {
                (
                    RemoteDetailState::ConfirmingStop,
                    RemoteDetailAction::EnterConfirmingStop,
                )
            } else {
                (RemoteDetailState::Menu, RemoteDetailAction::Ignore)
            }
        }
        (RemoteDetailState::Menu, MenuAction::Back) => {
            (RemoteDetailState::Menu, RemoteDetailAction::Back)
        }
        (RemoteDetailState::Menu, MenuAction::Dismiss) => {
            (RemoteDetailState::Menu, RemoteDetailAction::Dismiss)
        }
        (RemoteDetailState::ConfirmingStop, MenuAction::Stop) => {
            // The confirm dialog routes its `stop` value
            // through to the kill action followed by go-back-or-close.
            (RemoteDetailState::Menu, RemoteDetailAction::Kill)
        }
        (RemoteDetailState::ConfirmingStop, MenuAction::Back) => (
            RemoteDetailState::Menu,
            RemoteDetailAction::LeaveConfirmingStop,
        ),
        (RemoteDetailState::ConfirmingStop, _) => {
            // Ignore other menu actions while in the confirm dialog.
            (
                RemoteDetailState::ConfirmingStop,
                RemoteDetailAction::Ignore,
            )
        }
    }
}

/// Pre-built menu options shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDetailMenu {
    /// Available actions in the order they're listed.
    pub options: Vec<MenuAction>,
}

/// Build the menu options for the remote session detail dialog. The
/// exact wording lives in the consumer; we expose the [`MenuAction`]
/// discriminant.
pub fn build_review_menu(completed: bool, running: bool, can_kill: bool) -> RemoteDetailMenu {
    let mut options: Vec<MenuAction> = Vec::new();
    options.push(MenuAction::Open);
    if running && can_kill && !completed {
        options.push(MenuAction::Stop);
    }
    options.push(MenuAction::Back);
    RemoteDetailMenu { options }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_label_table() {
        assert_eq!(phase_label("needs_input"), Some("input required"));
        assert_eq!(phase_label("plan_ready"), Some("ready"));
        assert_eq!(phase_label("nope"), None);
    }

    #[test]
    fn agent_verb_table() {
        assert_eq!(agent_verb(Some("needs_input")), "waiting");
        assert_eq!(agent_verb(Some("plan_ready")), "done");
        assert_eq!(agent_verb(None), "working");
        assert_eq!(agent_verb(Some("other")), "working");
    }

    #[test]
    fn stage_label_table() {
        assert_eq!(stage_label(ReviewStage::Finding), "Find");
        assert_eq!(stage_label(ReviewStage::Verifying), "Verify");
        assert_eq!(stage_label(ReviewStage::Synthesizing), "Dedupe");
    }

    #[test]
    fn collapse_whitespace_handles_runs() {
        assert_eq!(
            collapse_whitespace("hello   world\n  foo"),
            "hello world foo"
        );
        assert_eq!(collapse_whitespace("  trim me  "), "trim me");
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn tool_summary_exit_plan_special_case() {
        let input = ToolUseInput::empty();
        assert_eq!(
            format_tool_use_summary("ExitPlanMode", &input),
            "Review the plan in Rebon on the web"
        );
    }

    #[test]
    fn tool_summary_ask_user_question_with_question() {
        let input = ToolUseInput::empty().with_first_question("What is\nthe answer?".into());
        assert_eq!(
            format_tool_use_summary("AskUserQuestion", &input),
            "Answer in browser: What is the answer?"
        );
    }

    #[test]
    fn tool_summary_ask_user_question_truncates_at_50() {
        let q = "x".repeat(80);
        let input = ToolUseInput::empty().with_first_question(q);
        let out = format_tool_use_summary("AskUserQuestion", &input);
        // "Answer in browser: " (19) + 50 chars = 69
        let body = out.strip_prefix("Answer in browser: ").unwrap();
        assert!(body.chars().count() <= 50);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn tool_summary_first_string_field() {
        let input = ToolUseInput::from_string_fields(vec!["ls -la".into()]);
        assert_eq!(format_tool_use_summary("Bash", &input), "Bash ls -la");
    }

    #[test]
    fn tool_summary_first_string_field_truncates_at_60() {
        let v = "y".repeat(80);
        let input = ToolUseInput::from_string_fields(vec![v]);
        let out = format_tool_use_summary("Bash", &input);
        let body = out.strip_prefix("Bash ").unwrap();
        assert!(body.chars().count() <= 60);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn tool_summary_skips_empty_string_fields() {
        let input = ToolUseInput::from_string_fields(vec!["".into(), "real value".into()]);
        assert_eq!(format_tool_use_summary("Read", &input), "Read real value");
    }

    #[test]
    fn tool_summary_falls_through_to_name() {
        let input = ToolUseInput::empty();
        assert_eq!(format_tool_use_summary("Custom", &input), "Custom");
    }

    #[test]
    fn pipeline_setup_when_no_progress() {
        let p = build_stage_pipeline(StagePipelineInput {
            stage: None,
            completed: false,
            has_progress: false,
        });
        // Setup is current, none of Find/Verify/Dedupe
        assert!(p.cells[0].is_current);
        assert!(!p.cells[1].is_current);
        assert!(!p.cells[2].is_current);
        assert!(!p.cells[3].is_current);
        assert!(!p.completed_check);
    }

    #[test]
    fn pipeline_running_with_finding_stage() {
        let p = build_stage_pipeline(StagePipelineInput {
            stage: Some(ReviewStage::Finding),
            completed: false,
            has_progress: true,
        });
        assert!(!p.cells[0].is_current);
        assert!(p.cells[1].is_current); // Find
    }

    #[test]
    fn pipeline_completed_no_current_with_check() {
        let p = build_stage_pipeline(StagePipelineInput {
            stage: Some(ReviewStage::Synthesizing),
            completed: true,
            has_progress: true,
        });
        for cell in &p.cells {
            assert!(!cell.is_current);
        }
        assert!(p.completed_check);
    }

    #[test]
    fn pipeline_label_order() {
        let p = build_stage_pipeline(StagePipelineInput {
            stage: None,
            completed: false,
            has_progress: false,
        });
        let labels: Vec<&str> = p.cells.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["Setup", "Find", "Verify", "Dedupe"]);
    }

    fn rp(
        stage: Option<ReviewStage>,
        found: u64,
        verified: u64,
        refuted: u64,
    ) -> ReviewProgressInput {
        ReviewProgressInput {
            stage,
            bugs_found: found,
            bugs_verified: verified,
            bugs_refuted: refuted,
        }
    }

    #[test]
    fn review_counts_no_progress_completed() {
        let line = review_counts_line(&ReviewCountsInput {
            status: TaskStatus::Completed,
            review: None,
        });
        assert_eq!(line, "done");
    }

    #[test]
    fn review_counts_no_progress_running() {
        let line = review_counts_line(&ReviewCountsInput {
            status: TaskStatus::Running,
            review: None,
        });
        assert_eq!(line, "setting up");
    }

    #[test]
    fn review_counts_completed_singular_finding() {
        let line = review_counts_line(&ReviewCountsInput {
            status: TaskStatus::Completed,
            review: Some(rp(None, 0, 1, 0)),
        });
        assert_eq!(line, "1 finding");
    }

    #[test]
    fn review_counts_completed_plural_findings() {
        let line = review_counts_line(&ReviewCountsInput {
            status: TaskStatus::Completed,
            review: Some(rp(None, 0, 3, 0)),
        });
        assert_eq!(line, "3 findings");
    }

    #[test]
    fn review_counts_completed_with_refuted() {
        let line = review_counts_line(&ReviewCountsInput {
            status: TaskStatus::Completed,
            review: Some(rp(None, 0, 3, 2)),
        });
        assert_eq!(line, "3 findings · 2 refuted");
    }

    #[test]
    fn review_counts_running_delegates_to_stage_counts() {
        let line = review_counts_line(&ReviewCountsInput {
            status: TaskStatus::Running,
            review: Some(rp(Some(ReviewStage::Verifying), 5, 2, 1)),
        });
        assert_eq!(line, "5 found · 2 verified · 1 refuted");
    }

    #[test]
    fn ultraplan_status_running_with_phase() {
        assert_eq!(
            build_ultraplan_status_text(TaskStatus::Running, Some("plan_ready")),
            "ready"
        );
        assert_eq!(
            build_ultraplan_status_text(TaskStatus::Running, Some("needs_input")),
            "input required"
        );
    }

    #[test]
    fn ultraplan_status_running_without_phase() {
        assert_eq!(
            build_ultraplan_status_text(TaskStatus::Running, None),
            "running"
        );
    }

    #[test]
    fn ultraplan_status_pending_treated_as_running() {
        assert_eq!(
            build_ultraplan_status_text(TaskStatus::Pending, None),
            "running"
        );
    }

    #[test]
    fn ultraplan_status_terminal_uses_status() {
        assert_eq!(
            build_ultraplan_status_text(TaskStatus::Completed, None),
            "completed"
        );
        assert_eq!(
            build_ultraplan_status_text(TaskStatus::Failed, Some("plan_ready")),
            "failed"
        );
    }

    fn block(name: &str) -> UltraplanLogBlock {
        UltraplanLogBlock {
            tool_name: name.into(),
            input: ToolUseInput::empty(),
        }
    }

    #[test]
    fn scan_session_log_empty() {
        let scan = scan_ultraplan_session_log(&[]);
        assert_eq!(scan.agents_working, 1);
        assert_eq!(scan.tool_calls, 0);
        assert!(scan.last_tool_call.is_none());
    }

    #[test]
    fn scan_session_log_counts_spawns() {
        let blocks = vec![block("Bash"), block("Agent"), block("Read"), block("Task")];
        let scan = scan_ultraplan_session_log(&blocks);
        // 1 + 2 spawns
        assert_eq!(scan.agents_working, 3);
        assert_eq!(scan.tool_calls, 4);
        assert_eq!(scan.last_tool_call.as_deref(), Some("Task"));
    }

    #[test]
    fn scan_session_log_last_tool_call_via_summary() {
        let blocks = vec![
            block("Bash"),
            UltraplanLogBlock {
                tool_name: "ExitPlanMode".into(),
                input: ToolUseInput::empty(),
            },
        ];
        let scan = scan_ultraplan_session_log(&blocks);
        assert_eq!(
            scan.last_tool_call.as_deref(),
            Some("Review the plan in Rebon on the web")
        );
    }

    #[test]
    fn detail_state_default_is_menu() {
        assert_eq!(RemoteDetailState::default(), RemoteDetailState::Menu);
    }

    #[test]
    fn detail_event_open_is_browser() {
        let (state, action) =
            handle_remote_detail_event(RemoteDetailState::Menu, MenuAction::Open, true);
        assert_eq!(state, RemoteDetailState::Menu);
        assert_eq!(action, RemoteDetailAction::OpenInBrowser);
    }

    #[test]
    fn detail_event_stop_with_kill_enters_confirm() {
        let (state, action) =
            handle_remote_detail_event(RemoteDetailState::Menu, MenuAction::Stop, true);
        assert_eq!(state, RemoteDetailState::ConfirmingStop);
        assert_eq!(action, RemoteDetailAction::EnterConfirmingStop);
    }

    #[test]
    fn detail_event_stop_without_kill_ignored() {
        let (state, action) =
            handle_remote_detail_event(RemoteDetailState::Menu, MenuAction::Stop, false);
        assert_eq!(state, RemoteDetailState::Menu);
        assert_eq!(action, RemoteDetailAction::Ignore);
    }

    #[test]
    fn detail_confirm_stop_kills_and_returns_to_menu() {
        let (state, action) =
            handle_remote_detail_event(RemoteDetailState::ConfirmingStop, MenuAction::Stop, true);
        assert_eq!(state, RemoteDetailState::Menu);
        assert_eq!(action, RemoteDetailAction::Kill);
    }

    #[test]
    fn detail_confirm_back_leaves_confirm_state() {
        let (state, action) =
            handle_remote_detail_event(RemoteDetailState::ConfirmingStop, MenuAction::Back, true);
        assert_eq!(state, RemoteDetailState::Menu);
        assert_eq!(action, RemoteDetailAction::LeaveConfirmingStop);
    }

    #[test]
    fn detail_back_in_menu() {
        let (_, action) =
            handle_remote_detail_event(RemoteDetailState::Menu, MenuAction::Back, false);
        assert_eq!(action, RemoteDetailAction::Back);
    }

    #[test]
    fn detail_dismiss_in_menu() {
        let (_, action) =
            handle_remote_detail_event(RemoteDetailState::Menu, MenuAction::Dismiss, false);
        assert_eq!(action, RemoteDetailAction::Dismiss);
    }

    #[test]
    fn build_menu_no_kill_skips_stop() {
        let menu = build_review_menu(false, true, false);
        assert_eq!(menu.options, vec![MenuAction::Open, MenuAction::Back]);
    }

    #[test]
    fn build_menu_running_with_kill_includes_stop() {
        let menu = build_review_menu(false, true, true);
        assert_eq!(
            menu.options,
            vec![MenuAction::Open, MenuAction::Stop, MenuAction::Back]
        );
    }

    #[test]
    fn build_menu_completed_skips_stop() {
        let menu = build_review_menu(true, false, true);
        assert_eq!(menu.options, vec![MenuAction::Open, MenuAction::Back]);
    }
}
