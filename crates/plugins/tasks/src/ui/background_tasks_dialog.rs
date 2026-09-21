//! The live state behind `/tasks`.
//!
//! [`crate::ui::tasks::tasks_dialog`] next door is the pure reducer: given a
//! list of items and an event, what does the dialog do. This module is that
//! reducer with a task registry behind it — it re-derives the layout from
//! live [`TaskSnapshot`]s every frame, answers keys, and hands the surface
//! back the two side effects it cannot perform itself (foreground this
//! agent, pause that task).
//!
//! Keys arrive as [`DialogKey`], never as a terminal event: the surface
//! translates its own key type once and every dialog reads the same one.
//! Painting is the surface's too — nothing here draws.

use crate::runtime::{TaskKind, TaskSnapshot};
use crate::ui::tasks::tasks_dialog::{
    build_actions, build_dialog_layout, build_subtitle, handle_tasks_dialog_event,
    initial_dialog_mode, nav_next, nav_previous, should_close_after_back, BackToListOutcome,
    DialogActions, DialogItem, DialogMode, TasksDialogAction, TasksDialogEvent, TasksDialogInput,
    TasksDialogLayout, LEADER_ID,
};
use rebon_dialog::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, PanelPane, PanelRow, PanelView,
    TextSpan, ViewSpec,
};

use crate::ui::panel_rows;
use crate::ui::task_activity::format_snapshot_activity;
use crate::ui::tasks_view;

/// Leader label shown on the synthetic teammate leader entry. The
/// session leader's display name belongs here, but the CLI does not yet
/// expose a leader concept, so we use a neutral `"main"` until that
/// lands.
const LEADER_LABEL: &str = "main";
const MAIN_AGENT_LABEL: &str = "Main agent";

/// Cloneable render + reducer state for the background-tasks dialog.
///
/// The dialog is a pure reducer driven by key events + a periodic
/// refresh against the live [`crate::runtime::TaskRegistry`]. Each frame
/// the surface calls [`Self::refresh`] to re-derive [`Self::layout`], reads
/// [`Self::subtitle`] and [`Self::actions`] to paint the chrome, and on a
/// key press calls [`Self::handle_key`] to move the selection or hand back
/// a side effect it cannot perform itself.
#[derive(Debug, Clone)]
pub struct BackgroundTasksDialogState {
    /// Current dialog mode (list or detail).
    pub mode: DialogMode,
    /// Currently-selected index inside [`TasksDialogLayout::all_selectable`].
    pub selected_index: usize,
    /// `true` when the dialog opened in detail mode (single task /
    /// explicit initial task id). Used by [`should_close_after_back`]
    /// to decide whether Back should drop to the list view or close.
    pub skipped_list_on_mount: bool,
    /// Optional foregrounded-task id filter forwarded to the layout
    /// builder so the active teammate is hidden from the list.
    pub foregrounded_task_id: Option<String>,
    /// Optional task kind filter used by specialized entry points like
    /// `/workflows`.
    pub kind_filter: Option<TaskKind>,
    /// Most recently computed layout. Cached so the runner can render
    /// without re-running the reducer per frame.
    pub layout: TasksDialogLayout,
    /// The snapshots [`Self::refresh`] last derived the layout from.
    ///
    /// Kept because the rows the panel describes carry live values the
    /// layout does not — a task's activity line, a shell's output tail,
    /// a workflow's phase table — and `view` takes no arguments.
    snapshots: Vec<TaskSnapshot>,
    /// Columns the surface last painted the body at. Rows are truncated
    /// and wrapped as they are built, so the width has to arrive before
    /// `view` is asked.
    viewport_cols: usize,
}

/// Width to build rows for before a surface has said how wide the panel
/// is. Only a test that never paints sees it.
const FALLBACK_WIDTH: usize = 80;

/// Result of handling a key event inside the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogKeyOutcome {
    /// Key consumed, dialog still open.
    Consumed,
    /// Promote the selected local agent to the live foreground view.
    SwitchToLiveAgent { task_id: String },
    /// Pause/preserve the selected item without cancelling its work.
    PausePreserve { task_id: String },
    /// Return to the main live agent view.
    SwitchToMainAgent,
    /// Dialog should be closed by the caller.
    Dismiss,
    /// Key ignored (no gating match).
    Ignored,
}

/// Action ids this dialog emits on the host stack, paired with
/// [`rebon_ui_seat::ids::dialog::TASKS`].
pub mod action {
    /// Promote the named local agent to the live foreground view. One
    /// value: the task id.
    pub const FOREGROUND: &str = "foreground";
    /// Pause the named task without cancelling its work. One value: the
    /// task id.
    pub const PAUSE_PRESERVE: &str = "pause-preserve";
    /// Return to the main live agent view. No values.
    pub const MAIN_AGENT: &str = "main-agent";
}

/// What [`BackgroundTasksDialogState::open`] is built from, handed to the
/// seat factory as an opaque payload.
///
/// Positional strings would not carry the task snapshots, and this
/// panel is opened only by front ends that already hold them.
#[derive(Debug, Clone, Default)]
pub struct BackgroundTasksDialogOpen {
    /// The registry's current snapshots.
    pub snapshots: Vec<TaskSnapshot>,
    /// Open straight into this task's detail pane.
    pub initial_detail_task_id: Option<String>,
    /// The foregrounded local-agent task, hidden from the list.
    pub foregrounded_task_id: Option<String>,
    /// Narrow the list to one kind. `/workflows` passes `LocalWorkflow`.
    pub kind_filter: Option<TaskKind>,
}

impl DialogModel for BackgroundTasksDialogState {
    rebon_dialog::dialog_plumbing!();

    fn id(&self) -> &'static str {
        rebon_ui_seat::ids::dialog::TASKS
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        match self.handle_key(press.key) {
            DialogKeyOutcome::Dismiss => DialogOutcome::Close,
            DialogKeyOutcome::SwitchToLiveAgent { task_id } => DialogOutcome::Action(
                DialogAction::closing(self.id(), action::FOREGROUND, task_id),
            ),
            DialogKeyOutcome::PausePreserve { task_id } => DialogOutcome::Action(
                DialogAction::staying(self.id(), action::PAUSE_PRESERVE, task_id),
            ),
            DialogKeyOutcome::SwitchToMainAgent => DialogOutcome::Action(
                DialogAction::closing_many(self.id(), action::MAIN_AGENT, Vec::new()),
            ),
            // An unmapped key keeps the dialog in focus rather than
            // falling through to whatever is underneath it.
            DialogKeyOutcome::Consumed | DialogKeyOutcome::Ignored => DialogOutcome::None,
        }
    }

    fn note_viewport(&mut self, _rows: u16, cols: u16) {
        self.viewport_cols = usize::from(cols).max(1);
    }

    fn view(&self) -> ViewSpec {
        ViewSpec::Panel(self.panel_view(self.viewport_cols))
    }

    fn is_fullscreen(&self) -> bool {
        false
    }
}

impl BackgroundTasksDialogState {
    /// Construct a fresh dialog state from the current snapshot of the
    /// registry. Uses the task-dialog default mode and selection rules.
    ///
    /// * `snapshots` — `crate::runtime::TaskRegistry::snapshots()`.
    /// * `initial_detail_task_id` — if `Some`, the dialog opens
    ///   directly in detail mode for that task id.
    pub fn open(
        snapshots: &[TaskSnapshot],
        initial_detail_task_id: Option<&str>,
        foregrounded_task_id: Option<String>,
    ) -> Self {
        Self::open_filtered(
            snapshots,
            initial_detail_task_id,
            foregrounded_task_id,
            None,
        )
    }

    /// Construct from the seat's payload — every field the three
    /// constructors below spell out, in one shape the `ui-registry`
    /// factory can carry.
    pub fn open_with(opened: &BackgroundTasksDialogOpen) -> Self {
        Self::open_filtered(
            &opened.snapshots,
            opened.initial_detail_task_id.as_deref(),
            opened.foregrounded_task_id.clone(),
            opened.kind_filter,
        )
    }

    /// Construct a dialog scoped to local workflow tasks. Used by
    /// `/workflows` and the workflow footer pill.
    pub fn open_workflows(
        snapshots: &[TaskSnapshot],
        initial_detail_task_id: Option<&str>,
    ) -> Self {
        Self::open_filtered(
            snapshots,
            initial_detail_task_id,
            None,
            Some(TaskKind::LocalWorkflow),
        )
    }

    fn open_filtered(
        snapshots: &[TaskSnapshot],
        initial_detail_task_id: Option<&str>,
        foregrounded_task_id: Option<String>,
        kind_filter: Option<TaskKind>,
    ) -> Self {
        let input =
            build_filtered_dialog_input(snapshots, foregrounded_task_id.clone(), kind_filter);
        let layout = dialog_layout_with_main_agent(input, foregrounded_task_id.is_some());
        let (mode, skipped_list_on_mount) =
            if foregrounded_task_id.is_some() && initial_detail_task_id.is_none() {
                (DialogMode::List, false)
            } else {
                initial_dialog_mode(initial_detail_task_id, &layout.all_selectable)
            };
        let selected_index = initial_detail_task_id
            .and_then(|id| {
                layout
                    .all_selectable
                    .iter()
                    .position(|item| item.id.as_str() == id)
            })
            .unwrap_or(0);
        Self {
            mode,
            selected_index,
            skipped_list_on_mount,
            foregrounded_task_id,
            kind_filter,
            layout,
            snapshots: snapshots.to_vec(),
            viewport_cols: FALLBACK_WIDTH,
        }
    }

    /// Re-derive the layout from the current snapshot of the registry.
    /// Called every frame so the dialog reflects in-flight state
    /// changes (task transitions Running → Completed, new tasks
    /// spawning, etc.) without the consumer having to push updates.
    ///
    /// Keeps the selected index in range if tasks disappear from the
    /// underlying list.
    pub fn refresh(&mut self, snapshots: &[TaskSnapshot]) {
        let selected_id = self.selected_item().map(|item| item.id.clone());
        self.snapshots = snapshots.to_vec();
        let input = build_filtered_dialog_input(
            snapshots,
            self.foregrounded_task_id.clone(),
            self.kind_filter,
        );
        self.layout = dialog_layout_with_main_agent(input, self.foregrounded_task_id.is_some());
        let total = self.layout.all_selectable.len();
        if total == 0 {
            self.selected_index = 0;
        } else if let Some(selected_id) = selected_id {
            if let Some(index) = self
                .layout
                .all_selectable
                .iter()
                .position(|item| item.id.as_str() == selected_id.as_str())
            {
                self.selected_index = index;
            } else if self.selected_index >= total {
                self.selected_index = total - 1;
            }
        } else if self.selected_index >= total {
            self.selected_index = total - 1;
        }
    }

    /// Whether the dialog is currently in list mode.
    pub fn is_list_mode(&self) -> bool {
        matches!(self.mode, DialogMode::List)
    }

    /// Return the currently-selected dialog item, if any.
    pub fn selected_item(&self) -> Option<&DialogItem> {
        match &self.mode {
            DialogMode::Detail { item_id } => self
                .layout
                .all_selectable
                .iter()
                .find(|item| item.id.as_str() == item_id.as_str())
                .or_else(|| self.layout.all_selectable.get(self.selected_index)),
            DialogMode::List => self.layout.all_selectable.get(self.selected_index),
        }
    }

    /// Selection-aware action shape for the footer actions row.
    pub fn actions(&self) -> DialogActions {
        let any_local_agent_running = self
            .layout
            .local_agent
            .iter()
            .any(|item| item.status == crate::ui::tasks::common::TaskStatus::Running);
        build_actions(self.selected_item(), any_local_agent_running)
    }

    /// Return the live activity text for a dialog item, if the
    /// registry snapshot has reported one.
    pub fn item_activity(&self, item: &DialogItem, snapshots: &[TaskSnapshot]) -> Option<String> {
        snapshots
            .iter()
            .find(|snap| snap.id.as_str() == item.id.as_str())
            .and_then(format_snapshot_activity)
    }

    /// Return the live activity text for the current selection.
    pub fn selected_activity(&self, snapshots: &[TaskSnapshot]) -> Option<String> {
        let item = self.selected_item()?;
        self.item_activity(item, snapshots)
    }

    /// Everything the panel wants painted, built for a body `width`
    /// columns wide.
    ///
    /// One scrolling pane over a one-row footer of key hints. The body
    /// opens with the subtitle and a blank separator either way; under
    /// that it is the selectable list, or the detail pane for whichever
    /// entry is open.
    pub fn panel_view(&self, width: usize) -> PanelView {
        let title = if self.kind_filter == Some(TaskKind::LocalWorkflow) {
            " Workflows "
        } else {
            " Background tasks "
        };
        let mut rows = vec![
            PanelRow::one(TextSpan::dim(format!(" {}", self.subtitle_or_empty()))),
            PanelRow::blank(),
        ];
        match &self.mode {
            DialogMode::List => rows.extend(self.list_rows(width)),
            DialogMode::Detail { item_id } => rows.extend(self.detail_rows(item_id, width)),
        }
        PanelView {
            title: title.into(),
            body: PanelPane::rows(rows),
            footer: vec![TextSpan::dim(format!(" {}", self.actions_hint()))],
            ..PanelView::default()
        }
    }

    /// The subtitle, or the words that stand in for it when nothing is
    /// running.
    fn subtitle_or_empty(&self) -> String {
        let subtitle = self.subtitle(&self.snapshots);
        if !subtitle.is_empty() {
            return subtitle;
        }
        if self.kind_filter == Some(TaskKind::LocalWorkflow) {
            "no workflows launched in this session".to_string()
        } else {
            "no running tasks".to_string()
        }
    }

    /// The selectable entries, or the line that says there are none.
    fn list_rows(&self, width: usize) -> Vec<PanelRow> {
        if self.layout.all_selectable.is_empty() {
            let empty = if self.kind_filter == Some(TaskKind::LocalWorkflow) {
                " (no workflows launched in this session)"
            } else {
                " (no background tasks — the registry is empty)"
            };
            return vec![PanelRow::one(TextSpan::dim(empty))];
        }
        self.layout
            .all_selectable
            .iter()
            .enumerate()
            .map(|(index, item)| {
                panel_rows::list_row(
                    &item.label,
                    item.status,
                    self.item_activity(item, &self.snapshots).as_deref(),
                    index == self.selected_index,
                    width,
                )
            })
            .collect()
    }

    /// The detail pane for `item_id`, or the line that says the task has
    /// been evicted from the registry since it was opened.
    fn detail_rows(&self, item_id: &str, width: usize) -> Vec<PanelRow> {
        let mut rows = Vec::new();
        if let Some(activity) = self.selected_activity(&self.snapshots) {
            rows.push(panel_rows::activity_row(&activity, width));
        }
        match self
            .snapshots
            .iter()
            .find(|snapshot| snapshot.id.as_str() == item_id)
        {
            Some(snapshot) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as u64)
                    .unwrap_or(0);
                rows.extend(panel_rows::detail_rows(snapshot, now_ms, width));
            }
            None => rows.push(PanelRow::one(TextSpan::dim(format!(
                " (task {item_id} no longer exists)"
            )))),
        }
        rows
    }

    /// The key hints, joined the way the footer shows them.
    fn actions_hint(&self) -> String {
        let actions = self.actions();
        let mut parts: Vec<&str> = Vec::new();
        if actions.show_select {
            parts.push("↑↓ select");
        }
        if actions.show_view {
            parts.push("↵ view");
        }
        if actions.show_foreground {
            parts.push("f foreground");
        }
        if actions.show_stop {
            parts.push("x background");
        }
        if actions.show_close {
            parts.push("← close");
        }
        parts.join(" · ")
    }

    /// Build the dialog subtitle ("2 active shells · 1 active agent").
    pub fn subtitle(&self, snapshots: &[TaskSnapshot]) -> String {
        let filtered;
        let snapshots = if let Some(kind) = self.kind_filter {
            filtered = snapshots
                .iter()
                .filter(|snapshot| snapshot.kind == kind)
                .cloned()
                .collect::<Vec<_>>();
            filtered.as_slice()
        } else {
            snapshots
        };
        if self.kind_filter == Some(TaskKind::LocalWorkflow) {
            return tasks_view::workflows_footer_label(snapshots).unwrap_or_default();
        }
        let counts = tasks_view::count_running_by_bucket(snapshots);
        build_subtitle(
            counts.running_teammates,
            counts.running_bash,
            counts.running_remote_or_agents,
        )
    }

    /// Apply one key press, dispatching through
    /// [`handle_tasks_dialog_event`]. Side effects (kill /
    /// enter-teammate-view / exit-teammate-view) are routed through
    /// `registry` when applicable.
    pub fn handle_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        // Detail-mode back gesture: the event loop reads the ← key even
        // in detail mode to back out to the list. The pure reducer
        // only accepts Left in list mode, so we handle the detail
        // path inline here.
        if !self.is_list_mode() {
            return self.handle_detail_key(key);
        }

        // Up/Down navigate the selection. The pure reducer does not
        // cover this because the event loop lets the dialog host own the
        // selected row index while the reducer next door focuses on
        // shared task layout and reducer data.
        match key {
            DialogKey::Up => {
                self.select_previous();
                return DialogKeyOutcome::Consumed;
            }
            DialogKey::Down => {
                self.select_next();
                return DialogKeyOutcome::Consumed;
            }
            _ => {}
        }

        let Some(event) = translate_dialog_key(key) else {
            return DialogKeyOutcome::Ignored;
        };

        let selection = self.selected_item().cloned();
        let action = handle_tasks_dialog_event(&self.mode, selection.as_ref(), event);
        self.apply_action(action)
    }

    fn handle_detail_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Enter => {
                if let Some(item) = self.selected_item() {
                    if item.id.as_str() == LEADER_ID {
                        DialogKeyOutcome::SwitchToMainAgent
                    } else if item.kind == Some(crate::ui::tasks::common::TaskKind::LocalAgent) {
                        DialogKeyOutcome::SwitchToLiveAgent {
                            task_id: item.id.clone(),
                        }
                    } else {
                        DialogKeyOutcome::Ignored
                    }
                } else {
                    DialogKeyOutcome::Ignored
                }
            }
            DialogKey::Char { value: 'x', .. } | DialogKey::Char { value: 'X', .. } => {
                if let Some(item) = self.selected_item() {
                    if item.status == crate::ui::tasks::common::TaskStatus::Running
                        && item.id.as_str() != LEADER_ID
                        && item.kind.is_some()
                    {
                        DialogKeyOutcome::PausePreserve {
                            task_id: item.id.clone(),
                        }
                    } else {
                        DialogKeyOutcome::Ignored
                    }
                } else {
                    DialogKeyOutcome::Ignored
                }
            }
            DialogKey::Up => {
                self.select_previous();
                self.sync_detail_item_to_selection();
                DialogKeyOutcome::Consumed
            }
            DialogKey::Down => {
                self.select_next();
                self.sync_detail_item_to_selection();
                DialogKeyOutcome::Consumed
            }
            DialogKey::Left | DialogKey::Escape => {
                let total = self.layout.all_selectable.len();
                match should_close_after_back(self.skipped_list_on_mount, total) {
                    BackToListOutcome::Close => DialogKeyOutcome::Dismiss,
                    BackToListOutcome::GoToList => {
                        self.mode = DialogMode::List;
                        DialogKeyOutcome::Consumed
                    }
                }
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn select_previous(&mut self) {
        self.selected_index = nav_previous(self.selected_index).selected_index;
    }

    fn select_next(&mut self) {
        let total = self.layout.all_selectable.len();
        self.selected_index = nav_next(self.selected_index, total).selected_index;
    }

    fn sync_detail_item_to_selection(&mut self) {
        let Some(item) = self.layout.all_selectable.get(self.selected_index) else {
            return;
        };
        if matches!(self.mode, DialogMode::Detail { .. }) {
            self.mode = DialogMode::Detail {
                item_id: item.id.clone(),
            };
        }
    }

    fn apply_action(&mut self, action: TasksDialogAction) -> DialogKeyOutcome {
        match action {
            TasksDialogAction::Dismiss(_) => DialogKeyOutcome::Dismiss,
            TasksDialogAction::OpenDetail(id) => {
                self.mode = DialogMode::Detail { item_id: id };
                DialogKeyOutcome::Consumed
            }
            TasksDialogAction::PausePreserve { id, .. } => {
                DialogKeyOutcome::PausePreserve { task_id: id }
            }
            // The `InProcessTeammate` kind does not exist in the
            // coordinator today; these actions are unreachable for
            // now, but we still handle them so adding teammate
            // Supporting a registry-backed parent task only requires
            // flipping the registry on.
            TasksDialogAction::EnterTeammateView(id) => {
                DialogKeyOutcome::SwitchToLiveAgent { task_id: id }
            }
            TasksDialogAction::ExitTeammateView => DialogKeyOutcome::SwitchToMainAgent,
            TasksDialogAction::Ignore => DialogKeyOutcome::Ignored,
        }
    }
}

fn build_filtered_dialog_input(
    snapshots: &[TaskSnapshot],
    foregrounded_task_id: Option<String>,
    kind_filter: Option<TaskKind>,
) -> TasksDialogInput {
    if let Some(kind) = kind_filter {
        let filtered = snapshots
            .iter()
            .filter(|snapshot| snapshot.kind == kind)
            .cloned()
            .collect::<Vec<_>>();
        tasks_view::build_dialog_input(&filtered, foregrounded_task_id)
    } else {
        tasks_view::build_dialog_input(snapshots, foregrounded_task_id)
    }
}

fn dialog_layout_with_main_agent(
    input: TasksDialogInput,
    include_main_agent: bool,
) -> TasksDialogLayout {
    let mut layout = build_dialog_layout(&input, LEADER_LABEL);
    if include_main_agent {
        let main = DialogItem {
            id: LEADER_ID.to_owned(),
            kind: None,
            status: crate::ui::tasks::common::TaskStatus::Running,
            label: MAIN_AGENT_LABEL.to_owned(),
        };
        layout.teammates.insert(0, main.clone());
        layout.all_selectable.insert(0, main);
    }
    layout
}

/// Translate a [`DialogKey`] into the [`TasksDialogEvent`] the pure
/// reducer accepts.
///
/// Returns `None` for keys the dialog does not care about, so the
/// caller can fall through to default behavior (e.g. scroll).
fn translate_dialog_key(key: DialogKey) -> Option<TasksDialogEvent> {
    match key {
        DialogKey::Left | DialogKey::Escape => Some(TasksDialogEvent::Left),
        DialogKey::Enter => Some(TasksDialogEvent::Enter),
        DialogKey::Char {
            value: 'x' | 'X', ..
        } => Some(TasksDialogEvent::XKey),
        DialogKey::Char {
            value: 'f' | 'F', ..
        } => Some(TasksDialogEvent::FKey),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{TaskKind, TaskStatus};
    use crate::test_support::task_snapshot;

    fn snap(id: &str, kind: TaskKind, status: TaskStatus, title: &str) -> TaskSnapshot {
        task_snapshot(id, kind, status, title)
    }

    #[test]
    fn open_empty_registry_starts_in_list_mode() {
        let state = BackgroundTasksDialogState::open(&[], None, None);
        assert!(state.is_list_mode());
        assert_eq!(state.selected_index, 0);
        assert!(state.layout.all_selectable.is_empty());
        assert!(!state.skipped_list_on_mount);
    }

    #[test]
    fn open_single_task_skips_to_detail() {
        let snapshots = vec![snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls")];
        let state = BackgroundTasksDialogState::open(&snapshots, None, None);
        assert!(!state.is_list_mode());
        assert!(state.skipped_list_on_mount);
        match &state.mode {
            DialogMode::Detail { item_id } => assert_eq!(item_id, "1"),
            _ => panic!("expected detail mode"),
        }
    }

    #[test]
    fn open_explicit_initial_id_forces_detail_mode() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let state = BackgroundTasksDialogState::open(&snapshots, Some("2"), None);
        match &state.mode {
            DialogMode::Detail { item_id } => assert_eq!(item_id, "2"),
            _ => panic!("expected detail mode"),
        }
        assert_eq!(
            state.selected_item().map(|item| item.id.as_str()),
            Some("2")
        );
        assert!(state.skipped_list_on_mount);
    }

    #[test]
    fn open_workflows_filters_to_local_workflow_tasks() {
        let snapshots = vec![
            snap("shell", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap(
                "workflow",
                TaskKind::LocalWorkflow,
                TaskStatus::Running,
                "release-flow",
            ),
        ];
        let state = BackgroundTasksDialogState::open_workflows(&snapshots, None);

        assert_eq!(state.kind_filter, Some(TaskKind::LocalWorkflow));
        assert_eq!(state.layout.all_selectable.len(), 1);
        assert_eq!(
            state.selected_item().map(|item| item.id.as_str()),
            Some("workflow")
        );
    }

    #[test]
    fn open_workflows_explicit_id_opens_detail() {
        let snapshots = vec![
            snap(
                "workflow-1",
                TaskKind::LocalWorkflow,
                TaskStatus::Running,
                "one",
            ),
            snap(
                "workflow-2",
                TaskKind::LocalWorkflow,
                TaskStatus::Running,
                "two",
            ),
        ];
        let state = BackgroundTasksDialogState::open_workflows(&snapshots, Some("workflow-2"));

        match &state.mode {
            DialogMode::Detail { item_id } => assert_eq!(item_id, "workflow-2"),
            _ => panic!("expected detail mode"),
        }
        assert_eq!(
            state.selected_item().map(|item| item.id.as_str()),
            Some("workflow-2")
        );
    }

    #[test]
    fn refresh_preserves_selection_by_item_id() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.selected_index = state
            .layout
            .all_selectable
            .iter()
            .position(|item| item.id.as_str() == "2")
            .unwrap();

        let reordered = vec![
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
        ];
        state.refresh(&reordered);

        assert_eq!(
            state.selected_item().map(|item| item.id.as_str()),
            Some("2")
        );
    }

    #[test]
    fn refresh_shrinks_selection_when_tasks_disappear() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
            snap("3", TaskKind::LocalShell, TaskStatus::Running, "df"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.selected_index = 2;
        state.refresh(&snapshots[..1]);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn refresh_zero_tasks_resets_selection() {
        let snapshots = vec![snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls")];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.selected_index = 0;
        state.refresh(&[]);
        assert_eq!(state.selected_index, 0);
        assert!(state.layout.all_selectable.is_empty());
    }

    #[test]
    fn selected_activity_uses_latest_non_empty_progress_line() {
        let mut snapshots = vec![snap("1", TaskKind::LocalAgent, TaskStatus::Running, "task")];
        snapshots[0].last_progress = Some("older\n\nreading src/lib.rs".into());
        let state = BackgroundTasksDialogState::open(&snapshots, None, None);

        assert_eq!(
            state.selected_activity(&snapshots).as_deref(),
            Some("reading src/lib.rs")
        );
    }

    #[test]
    fn list_mode_down_advances_selection() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        // open_multi starts in list mode because we have 2 items
        assert!(state.is_list_mode());
        let outcome = state.handle_key(DialogKey::Down);
        assert_eq!(outcome, DialogKeyOutcome::Consumed);
        assert_eq!(state.selected_index, 1);
    }

    #[test]
    fn list_mode_up_clamps_at_zero() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.handle_key(DialogKey::Up);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn list_mode_left_dismisses() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        assert_eq!(state.handle_key(DialogKey::Left), DialogKeyOutcome::Dismiss);
    }

    #[test]
    fn list_mode_enter_opens_local_agent_detail() {
        let snapshots = vec![
            snap("1", TaskKind::LocalAgent, TaskStatus::Running, "agent"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.selected_index = state
            .layout
            .all_selectable
            .iter()
            .position(|item| item.id == "1")
            .unwrap();
        let outcome = state.handle_key(DialogKey::Enter);
        assert_eq!(outcome, DialogKeyOutcome::Consumed);
        match &state.mode {
            DialogMode::Detail { item_id } => assert_eq!(item_id, "1"),
            _ => panic!("expected detail mode"),
        }
    }

    #[test]
    fn list_mode_enter_opens_shell_detail() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        let outcome = state.handle_key(DialogKey::Enter);
        assert_eq!(outcome, DialogKeyOutcome::Consumed);
        match &state.mode {
            DialogMode::Detail { item_id } => assert_eq!(item_id, "1"),
            _ => panic!("expected detail mode"),
        }
    }

    #[test]
    fn detail_mode_left_goes_back_to_list_when_multiple_items() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.mode = DialogMode::Detail {
            item_id: "1".into(),
        };
        assert!(matches!(state.mode, DialogMode::Detail { .. }));
        let outcome = state.handle_key(DialogKey::Left);
        assert_eq!(outcome, DialogKeyOutcome::Consumed);
        assert!(state.is_list_mode());
    }

    #[test]
    fn detail_mode_left_closes_when_skipped_list_on_mount_and_single_item() {
        let snapshots = vec![snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls")];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        assert!(state.skipped_list_on_mount);
        let outcome = state.handle_key(DialogKey::Left);
        assert_eq!(outcome, DialogKeyOutcome::Dismiss);
    }

    #[test]
    fn detail_mode_esc_also_backs_out() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.mode = DialogMode::Detail {
            item_id: "1".into(),
        };
        let outcome = state.handle_key(DialogKey::Escape);
        assert_eq!(outcome, DialogKeyOutcome::Consumed);
        assert!(state.is_list_mode());
    }

    #[test]
    fn detail_mode_down_switches_to_next_task() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, Some("1"), None);

        let outcome = state.handle_key(DialogKey::Down);

        assert_eq!(outcome, DialogKeyOutcome::Consumed);
        assert_eq!(state.selected_index, 1);
        match &state.mode {
            DialogMode::Detail { item_id } => assert_eq!(item_id, "2"),
            _ => panic!("expected detail mode"),
        }
    }

    #[test]
    fn detail_mode_up_switches_to_previous_task() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, Some("2"), None);

        let outcome = state.handle_key(DialogKey::Up);

        assert_eq!(outcome, DialogKeyOutcome::Consumed);
        assert_eq!(state.selected_index, 0);
        match &state.mode {
            DialogMode::Detail { item_id } => assert_eq!(item_id, "1"),
            _ => panic!("expected detail mode"),
        }
    }

    #[test]
    fn detail_mode_enter_switches_to_live_agent() {
        let snapshots = vec![
            snap("1", TaskKind::LocalAgent, TaskStatus::Running, "agent"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, Some("1"), None);

        let outcome = state.handle_key(DialogKey::Enter);

        assert_eq!(
            outcome,
            DialogKeyOutcome::SwitchToLiveAgent {
                task_id: "1".into()
            }
        );
    }

    #[test]
    fn detail_mode_x_routes_pause_preserve() {
        let snapshots = vec![
            snap("1", TaskKind::LocalAgent, TaskStatus::Running, "agent"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, Some("1"), None);

        let outcome = state.handle_key(DialogKey::plain('x'));

        assert_eq!(
            outcome,
            DialogKeyOutcome::PausePreserve {
                task_id: "1".into()
            }
        );
    }

    #[test]
    fn x_key_routes_pause_preserve_and_keeps_dialog_open() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        let outcome = state.handle_key(DialogKey::plain('x'));
        assert_eq!(
            outcome,
            DialogKeyOutcome::PausePreserve {
                task_id: "1".into()
            }
        );
        assert!(state.is_list_mode());
    }

    #[test]
    fn x_key_on_non_running_task_is_ignored() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Completed, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        // Running is sorted first, so index 0 = running "pwd".
        // Move to index 1 (Completed "ls").
        state.handle_key(DialogKey::Down);
        let outcome = state.handle_key(DialogKey::plain('x'));
        assert_eq!(outcome, DialogKeyOutcome::Ignored);
    }

    #[test]
    fn unknown_key_in_list_mode_is_ignored() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        let outcome = state.handle_key(DialogKey::plain('q'));
        assert_eq!(outcome, DialogKeyOutcome::Ignored);
    }

    #[test]
    fn actions_running_bash_shows_stop_and_not_foreground() {
        let snapshots = vec![
            snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls"),
            snap("2", TaskKind::LocalShell, TaskStatus::Running, "pwd"),
        ];
        let state = BackgroundTasksDialogState::open(&snapshots, None, None);
        let actions = state.actions();
        assert!(actions.show_stop);
        assert!(!actions.show_foreground);
        assert!(actions.show_select);
        assert!(actions.show_close);
        assert!(actions.show_view);
    }

    #[test]
    fn actions_running_local_agent_flags_stop_all() {
        let snapshots = vec![
            snap("a", TaskKind::LocalAgent, TaskStatus::Running, "task-a"),
            snap("b", TaskKind::LocalAgent, TaskStatus::Running, "task-b"),
        ];
        let state = BackgroundTasksDialogState::open(&snapshots, None, None);
        let actions = state.actions();
        assert!(actions.show_stop_all_agents);
    }

    #[test]
    fn subtitle_reflects_registry_counts() {
        let mut snapshots = vec![
            snap("a", TaskKind::LocalShell, TaskStatus::Running, "cmd-a"),
            snap("b", TaskKind::LocalShell, TaskStatus::Running, "cmd-b"),
            snap("c", TaskKind::LocalAgent, TaskStatus::Running, "task-c"),
            snap(
                "d",
                TaskKind::LocalAgent,
                TaskStatus::Running,
                "foreground-explorer",
            ),
        ];
        snapshots[0].is_backgrounded = true;
        snapshots[1].is_backgrounded = true;
        snapshots[2].is_backgrounded = true;
        let state = BackgroundTasksDialogState::open(&snapshots, None, None);
        assert_eq!(
            state.subtitle(&snapshots),
            "2 active shells · 1 active agent"
        );
    }

    #[test]
    fn subtitle_empty_when_all_tasks_terminal() {
        let snapshots = vec![
            snap("a", TaskKind::LocalShell, TaskStatus::Completed, "done-a"),
            snap("b", TaskKind::LocalAgent, TaskStatus::Failed, "failed-b"),
        ];
        let state = BackgroundTasksDialogState::open(&snapshots, None, None);
        assert_eq!(state.subtitle(&snapshots), "");
    }

    fn panel(state: &BackgroundTasksDialogState) -> PanelView {
        match state.view() {
            ViewSpec::Panel(view) => view,
            other => panic!("the tasks panel describes a panel, not {other:?}"),
        }
    }

    fn row_texts(rows: &[PanelRow]) -> Vec<String> {
        rows.iter().map(PanelRow::text).collect()
    }

    /// The list: a subtitle, a blank separator, then one row per entry,
    /// over a footer of key hints.
    #[test]
    fn the_list_view_opens_with_its_subtitle_and_ends_with_the_key_hints() {
        let mut snapshots = vec![
            snap("a", TaskKind::LocalShell, TaskStatus::Running, "build"),
            snap("b", TaskKind::LocalShell, TaskStatus::Running, "watch"),
        ];
        snapshots[0].is_backgrounded = true;
        snapshots[1].is_backgrounded = true;
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.mode = DialogMode::List;
        let view = panel(&state);

        assert_eq!(view.title, " Background tasks ");
        let rows = row_texts(&view.body.rows);
        assert!(rows[0].contains("2 active shells"), "{rows:?}");
        assert_eq!(rows[1], "");
        assert!(rows[2].contains("build"), "{rows:?}");
        assert!(view.body.rows[2].highlighted, "the first entry is selected");
        assert!(!view.body.rows[3].highlighted);
        assert!(view.side.is_none(), "the list is a single pane");

        let hints = view
            .footer
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(hints.contains("↑↓ select"), "{hints:?}");
        assert!(hints.contains("← close"), "{hints:?}");
    }

    /// An empty registry says so in the body rather than showing an
    /// empty frame.
    #[test]
    fn an_empty_registry_says_so_in_both_the_subtitle_and_the_body() {
        let state = BackgroundTasksDialogState::open(&[], None, None);
        let rows = row_texts(&panel(&state).body.rows);
        assert!(rows[0].contains("no running tasks"), "{rows:?}");
        assert!(rows[2].contains("the registry is empty"), "{rows:?}");
    }

    /// A task the registry has since evicted still opens: the pane says
    /// it is gone instead of painting nothing.
    #[test]
    fn a_detail_pane_for_an_evicted_task_says_it_no_longer_exists() {
        let snapshots = vec![snap("1", TaskKind::LocalShell, TaskStatus::Running, "ls")];
        let mut state = BackgroundTasksDialogState::open(&snapshots, None, None);
        state.refresh(&[]);
        state.mode = DialogMode::Detail {
            item_id: "1".to_string(),
        };
        let rows = row_texts(&panel(&state).body.rows);
        assert!(
            rows.iter().any(|row| row.contains("no longer exists")),
            "{rows:?}"
        );
    }

    /// `/workflows` is the same panel under another name and another
    /// set of words for "nothing here".
    #[test]
    fn the_workflow_filter_renames_the_panel_and_its_empty_lines() {
        let state = BackgroundTasksDialogState::open_workflows(&[], None);
        let view = panel(&state);
        assert_eq!(view.title, " Workflows ");
        let rows = row_texts(&view.body.rows);
        assert!(rows[0].contains("no workflows launched"), "{rows:?}");
        assert!(rows[2].contains("no workflows launched"), "{rows:?}");
    }
}
