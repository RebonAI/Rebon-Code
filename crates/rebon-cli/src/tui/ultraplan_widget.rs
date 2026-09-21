use std::time::SystemTime;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use rebon_plugin_tasks::runtime::{TaskData, TaskKind, TaskSnapshot, TaskStatus};
use rebon_tui::{parse_theme_color, RenderTheme};
use rebon_types::{
    PlanCoverageResult, PlanEntry, PlanEntryStatus, ReviewerVerdictRecord, VerdictSource,
};

use crate::session::ultraplan_run::UltraplanPhase;
use crate::tui::app::AppState;
use crate::tui::permission_modal::PermissionKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltraplanViewModel {
    pub task_title: String,
    pub elapsed: Option<String>,
    pub phase: UltraplanPhase,
    pub round: u32,
    pub last_verdict: Option<UltraplanVerdictView>,
    pub last_coverage: Option<UltraplanCoverageView>,
    pub pending_action: Option<&'static str>,
    pub tasks: Vec<UltraplanTaskRow>,
    pub plan_entries: Vec<UltraplanPlanEntryRow>,
    pub hidden_plan_entry_count: usize,
    pub execution_reexploration_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltraplanVerdictView {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltraplanCoverageView {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltraplanTaskRow {
    pub role: UltraplanTaskRole,
    pub title: String,
    pub status: TaskStatus,
    pub progress: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UltraplanTaskRole {
    Supervisor,
    Researcher,
    Reviewer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltraplanPlanEntryRow {
    pub text: String,
    pub status: PlanEntryStatus,
}

pub fn build_view_model(app: &AppState, now_ms: u64) -> Option<UltraplanViewModel> {
    let status = app.ultraplan_status.as_ref()?;
    let snapshots = app.task_snapshots();
    let tasks = related_task_rows(
        &snapshots,
        Some(status.run_id.as_str()),
        status.started_at_ms,
    );
    let phase = derive_phase(app, status.phase, &tasks);

    let plan_entries = plan_entry_rows(&app.plan_entries);
    let hidden_plan_entry_count = hidden_plan_entry_count(phase, plan_entries.len());
    Some(UltraplanViewModel {
        task_title: status.task_title.clone(),
        elapsed: status
            .started_at_ms
            .and_then(|started| now_ms.checked_sub(started))
            .map(format_elapsed),
        phase,
        round: status.round.max(1),
        last_verdict: verdict_view(status.last_verdict.as_ref(), phase),
        last_coverage: coverage_view(status.last_coverage.as_ref(), phase),
        pending_action: pending_action(app),
        tasks,
        plan_entries,
        hidden_plan_entry_count,
        execution_reexploration_count: status.execution_reexploration_count,
    })
}

pub fn render(frame: &mut Frame, area: Rect, app: &AppState, theme: &RenderTheme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let Some(vm) = build_view_model(app, now_ms) else {
        return;
    };
    render_view_model(frame, area, &vm, theme);
}

pub fn desired_height(app: &AppState, max_height: u16) -> u16 {
    if app.ultraplan_status.is_none() || max_height == 0 {
        return 0;
    }
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let Some(vm) = build_view_model(app, now_ms) else {
        return 0;
    };
    let task_count = visible_task_rows(&vm).len() as u16;
    let plan_count = visible_plan_entries(&vm).len() as u16;
    (base_height(&vm) + task_count + plan_count)
        .max(1)
        .min(max_height)
}

const REEXPLORATION_WARNING_THRESHOLD: u32 = 0;

fn execution_reexploration_warning(vm: &UltraplanViewModel) -> Option<String> {
    (matches!(vm.phase, UltraplanPhase::Executing)
        && vm.execution_reexploration_count > REEXPLORATION_WARNING_THRESHOLD)
        .then(|| {
            format!(
                "Plan fidelity warning: {} broad re-exploration tool use(s); declare DEVIATION if continuing.",
                vm.execution_reexploration_count
            )
        })
}

fn render_view_model(frame: &mut Frame, area: Rect, vm: &UltraplanViewModel, theme: &RenderTheme) {
    let ds = rebon_design_system::theme::get_active_theme();
    let accent = parse_theme_color(ds.chromeYellow);
    let muted = theme
        .system_info
        .fg
        .unwrap_or_else(|| parse_theme_color(ds.subtle));
    let mut lines: Vec<Line<'static>> = Vec::new();

    if let Some(verdict) = &vm.last_verdict {
        lines.push(Line::from(vec![
            Span::styled("Review: ", Style::default().fg(muted)),
            Span::raw(verdict.text.clone()),
        ]));
    }
    if let Some(coverage) = &vm.last_coverage {
        lines.push(Line::from(vec![
            Span::styled("Coverage: ", Style::default().fg(muted)),
            Span::raw(coverage.text.clone()),
        ]));
    }
    if let Some(warning) = execution_reexploration_warning(vm) {
        lines.push(Line::from(vec![
            Span::styled("Warning: ", Style::default().fg(accent)),
            Span::raw(warning),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("Activity: ", Style::default().fg(muted)),
        Span::raw(format_activity_line(vm)),
    ]));
    if let Some(action) = vm.pending_action {
        lines.push(Line::from(vec![
            Span::styled("Action: ", Style::default().fg(muted)),
            Span::styled(action, Style::default().fg(accent)),
        ]));
    }

    for task in visible_task_rows(vm) {
        let progress = task
            .progress
            .as_ref()
            .map(|p| format!(" — {p}"))
            .unwrap_or_default();
        lines.push(Line::from(format!(
            "{} [{}] {}{}",
            role_label(task.role),
            task.status.as_str(),
            task.title,
            progress
        )));
    }

    for entry in visible_plan_entries(vm) {
        lines.push(Line::from(format!(
            "{} {}",
            plan_entry_icon(entry.status),
            entry.text
        )));
    }
    if vm.hidden_plan_entry_count > 0 {
        lines.push(Line::from(format!(
            "(+{} more)",
            vm.hidden_plan_entry_count
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn base_height(vm: &UltraplanViewModel) -> u16 {
    let action_row = u16::from(vm.pending_action.is_some());
    let verdict_rows =
        u16::from(!matches!(vm.phase, UltraplanPhase::Executing) && vm.last_verdict.is_some());
    let coverage_rows =
        u16::from(!matches!(vm.phase, UltraplanPhase::Executing) && vm.last_coverage.is_some());
    let truncation_row = u16::from(vm.hidden_plan_entry_count > 0);
    let warning_row = u16::from(execution_reexploration_warning(vm).is_some());
    if matches!(vm.phase, UltraplanPhase::Executing) {
        1 + action_row + truncation_row + warning_row
    } else {
        1 + action_row + verdict_rows + coverage_rows + truncation_row + warning_row
    }
}

fn visible_task_rows(vm: &UltraplanViewModel) -> Vec<&UltraplanTaskRow> {
    let limit = if matches!(vm.phase, UltraplanPhase::Executing) {
        1
    } else {
        3
    };
    let active = vm
        .tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .take(limit)
        .collect::<Vec<_>>();
    if !active.is_empty() {
        return active;
    }
    if matches!(vm.phase, UltraplanPhase::Executing) {
        return Vec::new();
    }
    vm.tasks.iter().take(1).collect()
}

fn visible_plan_entries(vm: &UltraplanViewModel) -> Vec<&UltraplanPlanEntryRow> {
    if matches!(vm.phase, UltraplanPhase::Executing) {
        let active = vm
            .plan_entries
            .iter()
            .filter(|entry| entry.status != PlanEntryStatus::Completed)
            .take(2)
            .collect::<Vec<_>>();
        if !active.is_empty() {
            return active;
        }
        return vm.plan_entries.iter().rev().take(1).collect();
    }
    vm.plan_entries.iter().take(4).collect()
}

fn pending_action(app: &AppState) -> Option<&'static str> {
    let pending = app.pending_permission_view.as_ref()?;
    if !matches!(pending.kind, PermissionKind::ExitPlanMode { .. }) {
        return None;
    }
    match pending
        .selected_option()
        .map(|option| option.option_id.as_str())
    {
        Some("yes_clear_context_auto") => Some("Clear context -> auto execution"),
        Some("yes_auto") => Some("Run with auto mode"),
        Some("yes_accept_edits") => Some("Auto-accept edits"),
        Some("yes_default") => Some("Manually approve edits"),
        Some("reject_once") => Some("Awaiting feedback -> folding into requirements"),
        _ => None,
    }
}

fn derive_phase(
    app: &AppState,
    stored: UltraplanPhase,
    tasks: &[UltraplanTaskRow],
) -> UltraplanPhase {
    if stored == UltraplanPhase::Executing {
        return stored;
    }
    if matches!(
        app.pending_permission_view.as_ref().map(|p| &p.kind),
        Some(PermissionKind::ExitPlanMode { .. })
    ) {
        return UltraplanPhase::AwaitingPlanApproval;
    }
    if !app.plan_entries.is_empty() {
        return UltraplanPhase::Synthesizing;
    }
    if tasks.iter().any(|t| t.role == UltraplanTaskRole::Reviewer) {
        return UltraplanPhase::Reviewing;
    }
    if tasks
        .iter()
        .any(|t| t.role == UltraplanTaskRole::Researcher)
    {
        return UltraplanPhase::Researching;
    }
    stored
}

fn related_task_rows(
    snapshots: &[TaskSnapshot],
    run_id: Option<&str>,
    started_at_ms: Option<u64>,
) -> Vec<UltraplanTaskRow> {
    let agent_snapshots = snapshots
        .iter()
        .filter(|snapshot| {
            matches!(
                snapshot.kind,
                TaskKind::LocalAgent | TaskKind::RemoteAgent | TaskKind::InProcessTeammate
            )
        })
        .collect::<Vec<_>>();

    if let Some(run_id) = run_id.filter(|id| !id.is_empty()) {
        return agent_snapshots
            .iter()
            .copied()
            .filter(|snapshot| snapshot.ultraplan_id() == Some(run_id))
            .map(task_row)
            .collect::<Vec<_>>();
    }

    agent_snapshots
        .into_iter()
        .filter(|snapshot| {
            started_at_ms
                .map(|started| snapshot.start_time_ms >= started.saturating_sub(1_000))
                .unwrap_or(true)
                || matches!(snapshot.status, TaskStatus::Running | TaskStatus::Pending)
        })
        .map(task_row)
        .collect()
}

fn task_row(snapshot: &TaskSnapshot) -> UltraplanTaskRow {
    let role = role_from_metadata(snapshot).unwrap_or_else(|| {
        if is_reviewer_like(snapshot) {
            UltraplanTaskRole::Reviewer
        } else if matches!(
            snapshot.kind,
            TaskKind::LocalWorkflow | TaskKind::RemoteAgent
        ) {
            UltraplanTaskRole::Supervisor
        } else {
            UltraplanTaskRole::Researcher
        }
    });
    UltraplanTaskRow {
        role,
        title: snapshot.title.clone(),
        status: snapshot.status,
        progress: snapshot.last_progress.clone(),
    }
}

fn role_from_metadata(snapshot: &TaskSnapshot) -> Option<UltraplanTaskRole> {
    match snapshot.ultraplan_role()?.to_ascii_lowercase().as_str() {
        "researcher" => Some(UltraplanTaskRole::Researcher),
        "reviewer" => Some(UltraplanTaskRole::Reviewer),
        "supervisor" => Some(UltraplanTaskRole::Supervisor),
        _ => None,
    }
}

fn is_reviewer_like(snapshot: &TaskSnapshot) -> bool {
    let mut haystack = format!(
        "{} {}",
        snapshot.title,
        snapshot.last_progress.as_deref().unwrap_or("")
    );
    if let TaskData::LocalAgent(data) = &snapshot.data {
        haystack.push(' ');
        haystack.push_str(&data.agent_type);
        haystack.push(' ');
        haystack.push_str(&data.prompt);
    }
    let lower = haystack.to_ascii_lowercase();
    ["review", "verify", "verification", "评审"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn plan_entry_rows(entries: &[PlanEntry]) -> Vec<UltraplanPlanEntryRow> {
    entries
        .iter()
        .map(|entry| UltraplanPlanEntryRow {
            text: entry.content.clone(),
            status: entry.status,
        })
        .collect()
}

fn hidden_plan_entry_count(phase: UltraplanPhase, total: usize) -> usize {
    let visible_limit = if matches!(phase, UltraplanPhase::Executing) {
        2
    } else {
        4
    };
    total.saturating_sub(visible_limit)
}

fn verdict_view(
    verdict: Option<&ReviewerVerdictRecord>,
    phase: UltraplanPhase,
) -> Option<UltraplanVerdictView> {
    if matches!(phase, UltraplanPhase::Executing) {
        return None;
    }
    verdict.map(|record| {
        let text = match record.source {
            VerdictSource::PlanHunter => {
                format!("attack lenses -> {} confirmed", record.blocking_gaps)
            }
            _ => {
                let verdict = record.verdict.trim();
                if record.blocking_gaps == 1 {
                    format!("{verdict} · 1 blocking gap")
                } else {
                    format!("{verdict} · {} blocking gaps", record.blocking_gaps)
                }
            }
        };
        UltraplanVerdictView { text }
    })
}

fn coverage_view(
    coverage: Option<&PlanCoverageResult>,
    phase: UltraplanPhase,
) -> Option<UltraplanCoverageView> {
    if matches!(phase, UltraplanPhase::Executing) {
        return None;
    }
    coverage.map(|coverage| {
        let total = coverage.covered.len() + coverage.missing.len();
        let mut text = format!("{}/{}", coverage.covered.len(), total);
        if !coverage.missing.is_empty() {
            let shown = coverage.missing.iter().take(3).cloned().collect::<Vec<_>>();
            text.push_str(" · missing ");
            text.push_str(&shown.join(" "));
            let hidden = coverage.missing.len().saturating_sub(shown.len());
            if hidden > 0 {
                text.push_str(&format!(" +{hidden}"));
            }
        }
        UltraplanCoverageView { text }
    })
}

fn format_activity_line(vm: &UltraplanViewModel) -> String {
    let running = vm
        .tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    let completed = vm
        .tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Completed)
        .count();
    let failed = vm
        .tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Failed)
        .count();
    let draft_items = vm.plan_entries.len();
    let phase = match vm.phase {
        UltraplanPhase::PlanModeActive | UltraplanPhase::Orchestrating if running == 0 => {
            "starting scout"
        }
        UltraplanPhase::PlanModeActive | UltraplanPhase::Orchestrating => "research running",
        UltraplanPhase::Researching => "research running",
        UltraplanPhase::Reviewing => "review running",
        UltraplanPhase::Synthesizing => "drafting plan",
        UltraplanPhase::AwaitingPlanApproval => "awaiting approval",
        UltraplanPhase::Executing => "executing approved plan",
    };
    let mut parts = vec![phase.to_string(), format!("{running} running")];
    if completed > 0 {
        parts.push(format!("{completed} done"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if draft_items > 0 {
        parts.push(format!("{draft_items} plan item(s)"));
    }
    if vm.round > 1 && !matches!(vm.phase, UltraplanPhase::Executing) {
        parts.push(format!("revision {}", vm.round));
    }
    parts.join(" · ")
}

fn format_elapsed(ms: u64) -> String {
    let seconds = ms / 1_000;
    let minutes = seconds / 60;
    let rem = seconds % 60;
    if minutes > 0 {
        format!("{minutes}m {rem}s")
    } else {
        format!("{rem}s")
    }
}

fn role_label(role: UltraplanTaskRole) -> &'static str {
    match role {
        UltraplanTaskRole::Supervisor => "supervisor",
        UltraplanTaskRole::Researcher => "research",
        UltraplanTaskRole::Reviewer => "review",
    }
}

fn plan_entry_icon(status: PlanEntryStatus) -> &'static str {
    match status {
        PlanEntryStatus::Pending => "☐",
        PlanEntryStatus::InProgress => "◐",
        PlanEntryStatus::Completed => "☑",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_plugin_tasks::runtime::{LocalAgentData, TaskData, TaskId};

    fn local_agent_snapshot(
        id: &str,
        title: &str,
        prompt: &str,
        start_time_ms: u64,
    ) -> TaskSnapshot {
        let mut snapshot = TaskSnapshot::new_pending(
            TaskId::new(id),
            title.to_string(),
            TaskData::LocalAgent(LocalAgentData {
                prompt: prompt.to_string(),
                agent_type: "general-purpose".to_string(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        );
        snapshot.start_time_ms = start_time_ms;
        snapshot
    }

    fn active_app() -> AppState {
        let mut app = AppState::default();
        app.ultraplan_status = Some(crate::session::ultraplan_run::UltraplanStatus {
            run_id: "ultraplan-active".to_string(),
            phase: UltraplanPhase::PlanModeActive,
            task_title: "ship feature".to_string(),
            started_at_ms: Some(1_000),
            worker_count: None,
            context: None,
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        app
    }

    #[test]
    fn empty_status_has_no_view_model() {
        assert!(build_view_model(&AppState::default(), 1_000).is_none());
    }

    #[test]
    fn executing_reexploration_warning_affects_view_and_height() {
        let mut app = active_app();
        app.ultraplan_status.as_mut().unwrap().phase = UltraplanPhase::Executing;
        let base = desired_height(&app, 20);
        let vm = build_view_model(&app, 2_000).expect("view model");
        assert_eq!(vm.execution_reexploration_count, 0);
        assert!(execution_reexploration_warning(&vm).is_none());

        app.ultraplan_status
            .as_mut()
            .unwrap()
            .execution_reexploration_count = 1;
        let warned = desired_height(&app, 20);
        let vm = build_view_model(&app, 2_000).expect("view model");

        assert_eq!(warned, base + 1);
        assert!(execution_reexploration_warning(&vm)
            .is_some_and(|warning| warning.contains("Plan fidelity warning")));
    }

    #[test]
    fn plan_active_view_model_shows_title_elapsed_and_activity() {
        let app = active_app();
        let vm = build_view_model(&app, 6_000).expect("view model");
        assert_eq!(vm.task_title, "ship feature");
        assert_eq!(vm.elapsed.as_deref(), Some("5s"));
        assert_eq!(format_activity_line(&vm), "starting scout · 0 running");
    }

    #[test]
    fn reviewer_heuristic_classifies_review_agent() {
        let app = active_app();
        let mut snapshot = local_agent_snapshot(
            "a-review",
            "Verify edge cases",
            "review the draft plan",
            1_000,
        );
        if let TaskData::LocalAgent(data) = &mut snapshot.data {
            data.agent_type = "code-reviewer".to_string();
        }
        snapshot.metadata = serde_json::json!({ "ultraplan_id": "ultraplan-active" });
        app.tasks.insert(
            TaskId::new("a-review"),
            snapshot,
            rebon_types::PromptCancel::new(),
        );
        let vm = build_view_model(&app, 6_000).expect("view model");
        assert_eq!(vm.tasks[0].role, UltraplanTaskRole::Reviewer);
        assert_eq!(vm.phase, UltraplanPhase::Reviewing);
    }

    #[test]
    fn metadata_first_filters_to_matching_run_and_uses_role() {
        let app = active_app();
        let mut matching = local_agent_snapshot("a-match", "Inspect storage", "read code", 500);
        matching.metadata = serde_json::json!({
            "ultraplan_id": "ultraplan-active",
            "ultraplan_role": "reviewer",
        });
        let mut other_run = local_agent_snapshot("a-other", "Other run", "review unrelated", 1_000);
        other_run.metadata = serde_json::json!({
            "ultraplan_id": "ultraplan-other",
            "ultraplan_role": "researcher",
        });
        app.tasks.insert(
            TaskId::new("a-match"),
            matching,
            rebon_types::PromptCancel::new(),
        );
        app.tasks.insert(
            TaskId::new("a-other"),
            other_run,
            rebon_types::PromptCancel::new(),
        );

        let vm = build_view_model(&app, 6_000).expect("view model");
        assert_eq!(vm.tasks.len(), 1);
        assert_eq!(vm.tasks[0].title, "Inspect storage");
        assert_eq!(vm.tasks[0].role, UltraplanTaskRole::Reviewer);
        assert_eq!(vm.phase, UltraplanPhase::Reviewing);
    }

    #[test]
    fn metadata_filter_hides_unmatched_tasks_when_run_id_exists() {
        let app = active_app();
        let old_unrelated = local_agent_snapshot("a-old", "Old task", "review old", 0);
        app.tasks.insert(
            TaskId::new("a-old"),
            old_unrelated,
            rebon_types::PromptCancel::new(),
        );

        let vm = build_view_model(&app, 6_000).expect("view model");
        assert!(vm.tasks.is_empty());
    }

    #[test]
    fn metadata_role_maps_researcher_and_supervisor() {
        let mut researcher = local_agent_snapshot("a-research", "Research", "read code", 1_000);
        researcher.metadata = serde_json::json!({ "ultraplan_role": "researcher" });
        let mut supervisor = local_agent_snapshot("a-super", "Supervise", "coordinate", 1_000);
        supervisor.metadata = serde_json::json!({ "ultraplan_role": "supervisor" });

        assert_eq!(task_row(&researcher).role, UltraplanTaskRole::Researcher);
        assert_eq!(task_row(&supervisor).role, UltraplanTaskRole::Supervisor);
    }

    #[test]
    fn approval_view_model_shows_selected_execution_action() {
        let mut app = active_app();
        app.pending_permission_view = Some(crate::tui::permission_modal::PermissionModalView {
            query_id: 1,
            tool_call_id: "tool-exit".into(),
            title: "Plan ready for review".into(),
            summary: "Review the proposed plan and choose how to proceed.".into(),
            options: vec![crate::tui::permission_modal::PermissionOptionView {
                option_id: "yes_auto".into(),
                label: "Yes, run with auto mode".into(),
                kind: rebon_core::permission::PermissionOptionKind::AllowOnce,
            }],
            selected: 0,
            extra_text: String::new(),
            extra_text_focused: false,
            kind: PermissionKind::ExitPlanMode {
                plan: "approved plan".into(),
            },
        });

        let vm = build_view_model(&app, 6_000).expect("view model");

        assert_eq!(vm.phase, UltraplanPhase::AwaitingPlanApproval);
        assert_eq!(vm.pending_action, Some("Run with auto mode"));
    }

    #[test]
    fn completed_agents_are_hidden_during_execute_phase() {
        let mut app = active_app();
        app.ultraplan_status.as_mut().unwrap().phase = UltraplanPhase::Executing;
        let mut completed =
            local_agent_snapshot("a-done", "Completed research", "read code", 1_000);
        completed.status = TaskStatus::Completed;
        completed.metadata = serde_json::json!({
            "ultraplan_id": "ultraplan-active",
            "ultraplan_role": "researcher",
        });
        app.tasks.insert(
            TaskId::new("a-done"),
            completed,
            rebon_types::PromptCancel::new(),
        );

        let vm = build_view_model(&app, 6_000).expect("view model");

        assert!(visible_task_rows(&vm).is_empty());
        assert_eq!(desired_height(&app, 20), 1);
    }

    #[test]
    fn execute_phase_keeps_only_live_agent_rows() {
        let mut app = active_app();
        app.ultraplan_status.as_mut().unwrap().phase = UltraplanPhase::Executing;
        let mut running = local_agent_snapshot("a-running", "Implement code", "edit code", 1_000);
        running.status = TaskStatus::Running;
        running.metadata = serde_json::json!({
            "ultraplan_id": "ultraplan-active",
            "ultraplan_role": "researcher",
        });
        let mut completed = local_agent_snapshot("a-completed", "Old research", "read code", 1_000);
        completed.status = TaskStatus::Completed;
        completed.metadata = serde_json::json!({
            "ultraplan_id": "ultraplan-active",
            "ultraplan_role": "reviewer",
        });
        app.tasks.insert(
            TaskId::new("a-running"),
            running,
            rebon_types::PromptCancel::new(),
        );
        app.tasks.insert(
            TaskId::new("a-completed"),
            completed,
            rebon_types::PromptCancel::new(),
        );

        let vm = build_view_model(&app, 6_000).expect("view model");
        let rows = visible_task_rows(&vm);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Implement code");
    }

    #[test]
    fn execute_phase_prioritizes_unfinished_plan_entries() {
        let mut app = active_app();
        app.ultraplan_status.as_mut().unwrap().phase = UltraplanPhase::Executing;
        app.plan_entries.extend([
            PlanEntry {
                content: "Done item".to_string(),
                priority: rebon_types::PlanEntryPriority::High,
                status: PlanEntryStatus::Completed,
            },
            PlanEntry {
                content: "Current item".to_string(),
                priority: rebon_types::PlanEntryPriority::High,
                status: PlanEntryStatus::InProgress,
            },
            PlanEntry {
                content: "Next item".to_string(),
                priority: rebon_types::PlanEntryPriority::Medium,
                status: PlanEntryStatus::Pending,
            },
        ]);

        let vm = build_view_model(&app, 6_000).expect("view model");
        let entries = visible_plan_entries(&vm);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "Current item");
        assert_eq!(entries[1].text, "Next item");
    }

    #[test]
    fn planning_view_model_shows_round_verdict_and_coverage() {
        let mut app = active_app();
        let status = app.ultraplan_status.as_mut().unwrap();
        status.round = 3;
        status.last_verdict = Some(ReviewerVerdictRecord {
            round: 2,
            verdict: "FAIL".into(),
            blocking_gaps: 2,
            source: VerdictSource::Agent,
        });
        status.last_coverage = Some(PlanCoverageResult {
            covered: vec!["T1".into(), "T2".into()],
            missing: vec!["T3".into(), "T7".into(), "T8".into(), "T9".into()],
            unknown_ids: Vec::new(),
        });

        let vm = build_view_model(&app, 6_000).expect("view model");

        assert_eq!(vm.round, 3);
        assert_eq!(
            vm.last_verdict.as_ref().unwrap().text,
            "FAIL · 2 blocking gaps"
        );
        assert_eq!(
            vm.last_coverage.as_ref().unwrap().text,
            "2/6 · missing T3 T7 T8 +1"
        );
        assert!(format_activity_line(&vm).contains("revision 3"));
    }

    #[test]
    fn activity_line_reports_real_task_counts() {
        let app = active_app();
        let mut running = local_agent_snapshot("a-running", "Scout", "read code", 1_000);
        running.status = TaskStatus::Running;
        running.metadata = serde_json::json!({
            "ultraplan_id": "ultraplan-active",
            "ultraplan_role": "researcher",
        });
        let mut completed = local_agent_snapshot("a-done", "Done", "read code", 1_000);
        completed.status = TaskStatus::Completed;
        completed.metadata = serde_json::json!({
            "ultraplan_id": "ultraplan-active",
            "ultraplan_role": "reviewer",
        });
        app.tasks.insert(
            TaskId::new("a-running"),
            running,
            rebon_types::PromptCancel::new(),
        );
        app.tasks.insert(
            TaskId::new("a-done"),
            completed,
            rebon_types::PromptCancel::new(),
        );
        let vm = build_view_model(&app, 6_000).expect("view model");

        assert_eq!(
            format_activity_line(&vm),
            "review running · 1 running · 1 done"
        );
        let mut app = active_app();
        let status = app.ultraplan_status.as_mut().unwrap();
        status.phase = UltraplanPhase::Executing;
        status.last_verdict = Some(ReviewerVerdictRecord {
            round: 1,
            verdict: "PASS".into(),
            blocking_gaps: 0,
            source: VerdictSource::Agent,
        });
        status.last_coverage = Some(PlanCoverageResult {
            covered: vec!["T1".into()],
            missing: Vec::new(),
            unknown_ids: Vec::new(),
        });

        let vm = build_view_model(&app, 6_000).expect("view model");

        assert!(vm.last_verdict.is_none());
        assert!(vm.last_coverage.is_none());
        assert_eq!(desired_height(&app, 20), 1);
    }

    #[test]
    fn plan_entry_truncation_reports_hidden_count() {
        let mut app = active_app();
        for idx in 0..6 {
            app.plan_entries.push(PlanEntry {
                content: format!("Item {idx}"),
                priority: rebon_types::PlanEntryPriority::Medium,
                status: PlanEntryStatus::Pending,
            });
        }

        let vm = build_view_model(&app, 6_000).expect("view model");

        assert_eq!(visible_plan_entries(&vm).len(), 4);
        assert_eq!(vm.hidden_plan_entry_count, 2);
        assert_eq!(desired_height(&app, 20), 6);
    }
}
