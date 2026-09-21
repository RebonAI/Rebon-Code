//! The live state behind `/teams`.
//!
//! [`crate::ui::teams`] next door is the pure reducer and its command
//! projection. This module runs those commands: killing a live teammate
//! through the registry, writing a shutdown request to a tmux-backed one's
//! mailbox, hiding a pane through [`crate::ui::team_panes`].
//!
//! Keys arrive as [`DialogKey`], never as a terminal event, and nothing
//! here paints.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::runtime::{
    kill_in_process_teammate, request_teammate_shutdown, set_in_process_teammate_permission_mode,
    TaskId, TaskRegistry, TaskSnapshot,
};
use crate::ui::teams::{DialogLevel, TeamsDialogAction, TeamsDialogCommand};
use rebon_dialog::model::DialogKey;
use rebon_tool::{
    add_hidden_pane_id, remove_hidden_pane_id, remove_member_by_pane_id, set_team_member_mode,
    write_mailbox_message, TeamMailboxMessage,
};
use serde_json::json;
use tracing::warn;

use crate::ui::team_panes::TeamPaneBackendHandle;
use crate::ui::teams_view::{build_team_dialog_summaries, TeamDialogEntry};

const TEAM_LEAD_NAME: &str = "team-lead";

/// Cloneable render + reducer state for the teams dialog.
#[derive(Debug, Clone)]
pub struct TeamsDialogState {
    /// Pure reducer state from [`crate::ui::teams`].
    pub state: crate::ui::teams::TeamsDialogState,
    entries: Vec<TeamDialogEntry>,
    pane_backend: TeamPaneBackendHandle,
}

/// Result of a key event handled inside the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogKeyOutcome {
    /// Key consumed, dialog remains open.
    Consumed,
    /// Dialog should be dismissed.
    Dismiss,
    /// Open a teammate detail/output surface for the given task id.
    OpenTaskDetail { task_id: String },
    /// Key was not mapped by the dialog.
    Ignored,
}

impl TeamsDialogState {
    /// Open the dialog for the requested team or, when absent, the
    /// first discovered team.
    pub fn open(snapshots: &[TaskSnapshot], initial_team_name: Option<&str>) -> Option<Self> {
        Self::open_with_backend(
            snapshots,
            initial_team_name,
            TeamPaneBackendHandle::system(),
        )
    }

    fn open_with_backend(
        snapshots: &[TaskSnapshot],
        initial_team_name: Option<&str>,
        pane_backend: TeamPaneBackendHandle,
    ) -> Option<Self> {
        let summaries = build_team_dialog_summaries(snapshots);
        let summary = match initial_team_name {
            Some(name) => summaries.into_iter().find(|s| s.name == name),
            None => summaries.into_iter().next(),
        }?;
        let entries = summary.teammates;
        Some(Self {
            state: crate::ui::teams::TeamsDialogState::new(
                summary.name,
                entries.iter().map(|entry| entry.teammate.clone()).collect(),
            ),
            entries,
            pane_backend,
        })
    }

    fn current_team_name(&self) -> &str {
        match &self.state.dialog_level {
            DialogLevel::TeammateList { team_name }
            | DialogLevel::TeammateDetail { team_name, .. } => team_name,
        }
    }

    fn replace_entries(&mut self, entries: Vec<TeamDialogEntry>) {
        self.entries = entries;
        self.state.sync_teammates(
            self.entries
                .iter()
                .map(|entry| entry.teammate.clone())
                .collect(),
        );
    }

    fn entry_by_agent_id(&self, agent_id: &str) -> Option<&TeamDialogEntry> {
        self.entries
            .iter()
            .find(|entry| entry.teammate.agent_id == agent_id)
    }

    fn entry_by_target_id(&self, target_id: &str) -> Option<&TeamDialogEntry> {
        self.entries
            .iter()
            .find(|entry| entry.teammate.target_id == target_id)
    }

    /// Whether the current team has at least one tmux-backed teammate
    /// that supports hide/show.
    pub fn supports_hide_show(&self) -> bool {
        self.state
            .teammates
            .iter()
            .any(|teammate| teammate.can_hide)
    }

    /// Whether the currently-selected teammate supports hide/show.
    pub fn current_teammate_can_hide(&self) -> bool {
        self.state
            .current_teammate()
            .map(|teammate| teammate.can_hide)
            .unwrap_or(false)
    }

    /// Refresh teammate data from live snapshots and on-disk team files.
    /// Returns `false` when the current team no longer exists and the dialog
    /// should be closed by the caller.
    pub fn refresh(&mut self, snapshots: &[TaskSnapshot]) -> bool {
        let team_name = self.current_team_name().to_string();
        let summaries = build_team_dialog_summaries(snapshots);
        let Some(summary) = summaries.into_iter().find(|s| s.name == team_name) else {
            return false;
        };
        self.replace_entries(summary.teammates);
        !self.state.teammates.is_empty()
    }

    /// Handle a key event, routing emitted commands into the
    /// coordinator registry where supported.
    pub fn handle_key(&mut self, key: DialogKey, registry: &TaskRegistry) -> DialogKeyOutcome {
        match key {
            DialogKey::Left | DialogKey::Escape => {
                if matches!(self.state.dialog_level, DialogLevel::TeammateDetail { .. }) {
                    let _ = self.state.apply_action(TeamsDialogAction::Back);
                    DialogKeyOutcome::Consumed
                } else {
                    DialogKeyOutcome::Dismiss
                }
            }
            DialogKey::Up => self.apply_action(TeamsDialogAction::Previous, registry),
            DialogKey::Down => self.apply_action(TeamsDialogAction::Next, registry),
            DialogKey::Enter => self.apply_action(TeamsDialogAction::Enter, registry),
            DialogKey::Char {
                value: 'k' | 'K', ..
            } => self.apply_action(TeamsDialogAction::Kill, registry),
            DialogKey::Char {
                value: 's' | 'S', ..
            } => self.apply_action(TeamsDialogAction::Shutdown, registry),
            DialogKey::Char {
                value: 'p' | 'P', ..
            } => self.apply_action(TeamsDialogAction::PruneIdle, registry),
            DialogKey::Char { value: 'h', .. } => {
                self.apply_action(TeamsDialogAction::ToggleHide, registry)
            }
            DialogKey::Char { value: 'H', .. } => {
                self.apply_action(TeamsDialogAction::ToggleHideAll, registry)
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn apply_action(
        &mut self,
        action: TeamsDialogAction,
        registry: &TaskRegistry,
    ) -> DialogKeyOutcome {
        let outcome = self.state.apply_action(action);
        let mut key_outcome = if outcome.close_dialog {
            DialogKeyOutcome::Dismiss
        } else {
            DialogKeyOutcome::Consumed
        };
        let mut needs_refresh = false;

        for command in outcome.commands {
            match command {
                TeamsDialogCommand::Kill {
                    team_name,
                    agent_id,
                    target_id,
                    ..
                } => {
                    if let Some(entry) = self.entry_by_agent_id(&agent_id).cloned() {
                        if entry.is_in_process {
                            let _ = kill_in_process_teammate(registry, &TaskId::new(target_id));
                        } else if entry.backend_type.as_deref() == Some("tmux") {
                            if let Err(err) = self
                                .pane_backend
                                .kill_pane(&entry.pane_id, entry.backend_type.as_deref())
                            {
                                warn!(error = %err, pane_id = %entry.pane_id, "teams dialog: failed to kill tmux pane");
                            }
                            if let Err(err) = remove_member_by_pane_id(&team_name, &entry.pane_id) {
                                warn!(error = %err, pane_id = %entry.pane_id, "teams dialog: failed to remove tmux teammate from team file");
                            }
                            needs_refresh = true;
                        }
                    }
                }
                TeamsDialogCommand::Shutdown {
                    team_name,
                    teammate,
                    agent_id,
                    target_id,
                    ..
                } => {
                    if self
                        .entry_by_agent_id(&agent_id)
                        .map(|entry| entry.is_in_process)
                        .unwrap_or(false)
                    {
                        let _ = request_teammate_shutdown(registry, &TaskId::new(target_id));
                    } else if let Err(err) =
                        send_shutdown_request(&team_name, &teammate, "Requested from teams dialog")
                    {
                        warn!(error = %err, teammate = %teammate, "teams dialog: failed to write shutdown request");
                    }
                }
                TeamsDialogCommand::ViewOutput { target_id, .. } => {
                    if let Some(entry) = self.entry_by_target_id(&target_id).cloned() {
                        if entry.is_in_process {
                            key_outcome = DialogKeyOutcome::OpenTaskDetail { task_id: target_id };
                        } else if entry.backend_type.as_deref() == Some("tmux") {
                            if let Err(err) = self
                                .pane_backend
                                .view_pane(&entry.pane_id, entry.backend_type.as_deref())
                            {
                                warn!(error = %err, pane_id = %entry.pane_id, "teams dialog: failed to focus tmux pane");
                            }
                            key_outcome = DialogKeyOutcome::Dismiss;
                        }
                    }
                }
                TeamsDialogCommand::CycleMode {
                    team_name,
                    teammate,
                    target_id,
                    target_mode,
                } => {
                    let _ = set_in_process_teammate_permission_mode(
                        registry,
                        &TaskId::new(target_id),
                        target_mode.as_str().to_string(),
                    );
                    let _ = set_team_member_mode(&team_name, &teammate, target_mode.as_str());
                    needs_refresh = true;
                }
                TeamsDialogCommand::CycleModeAll {
                    team_name,
                    teammates,
                    target_mode,
                } => {
                    let snapshots = registry.snapshots();
                    for teammate in teammates {
                        if let Some(snapshot) = snapshots.iter().find(|snap| {
                            matches!(
                                &snap.data,
                                crate::runtime::TaskData::InProcessTeammate(data)
                                    if data.identity.team_name == team_name
                                        && data.identity.agent_name == teammate
                            )
                        }) {
                            let _ = set_in_process_teammate_permission_mode(
                                registry,
                                &snapshot.id,
                                target_mode.as_str().to_string(),
                            );
                        }
                        let _ = set_team_member_mode(&team_name, &teammate, target_mode.as_str());
                    }
                    needs_refresh = true;
                }
                TeamsDialogCommand::ToggleHide {
                    team_name,
                    target_id,
                    hide,
                    ..
                } => {
                    if let Some(entry) = self.entry_by_target_id(&target_id).cloned() {
                        if let Err(err) = self.toggle_hide(&team_name, &entry, hide) {
                            warn!(error = %err, pane_id = %entry.pane_id, hide, "teams dialog: failed to toggle teammate visibility");
                        } else {
                            needs_refresh = true;
                        }
                    }
                }
                TeamsDialogCommand::ToggleHideAll { team_name, hide } => {
                    let entries = self.entries.clone();
                    for entry in entries.iter().filter(|entry| entry.teammate.can_hide) {
                        if let Err(err) = self.toggle_hide(&team_name, entry, hide) {
                            warn!(error = %err, pane_id = %entry.pane_id, hide, "teams dialog: failed to toggle all teammate visibility");
                        } else {
                            needs_refresh = true;
                        }
                    }
                }
            }
        }

        if needs_refresh {
            let snapshots = registry.snapshots();
            if !self.refresh(&snapshots) {
                return DialogKeyOutcome::Dismiss;
            }
        }

        key_outcome
    }

    fn toggle_hide(
        &self,
        team_name: &str,
        entry: &TeamDialogEntry,
        hide: bool,
    ) -> anyhow::Result<()> {
        if !self
            .pane_backend
            .set_hidden(&entry.pane_id, entry.backend_type.as_deref(), hide)?
        {
            return Ok(());
        }
        if hide {
            add_hidden_pane_id(team_name, &entry.pane_id)?;
        } else {
            remove_hidden_pane_id(team_name, &entry.pane_id)?;
        }
        Ok(())
    }
}

fn send_shutdown_request(team_name: &str, teammate: &str, reason: &str) -> anyhow::Result<()> {
    write_mailbox_message(
        team_name,
        teammate,
        TeamMailboxMessage {
            from: TEAM_LEAD_NAME.into(),
            text: json!({
                "type": "shutdown_request",
                "reason": reason,
            })
            .to_string(),
            timestamp: current_timestamp(),
            read: false,
            color: None,
            summary: Some("shutdown requested".into()),
        },
    )?;
    Ok(())
}

fn current_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        register_in_process_teammate_task, InProcessTeammateTaskSpec, TaskStatus, TeammateIdentity,
    };
    use crate::ui::team_panes::TeamPaneBackend;
    use rebon_tool::tasks::test_support::TestConfigHome;
    use rebon_tool::{read_mailbox, write_team_file, TeamFile, TeamMember};
    use std::sync::{Arc, Mutex, MutexGuard};

    #[derive(Default)]
    struct RecordingPaneBackend {
        viewed: Mutex<Vec<String>>,
        killed: Mutex<Vec<String>>,
        hidden: Mutex<Vec<(String, bool)>>,
    }

    impl RecordingPaneBackend {
        fn viewed(&self) -> MutexGuard<'_, Vec<String>> {
            self.viewed.lock().unwrap()
        }

        fn killed(&self) -> MutexGuard<'_, Vec<String>> {
            self.killed.lock().unwrap()
        }

        fn hidden(&self) -> MutexGuard<'_, Vec<(String, bool)>> {
            self.hidden.lock().unwrap()
        }
    }

    impl TeamPaneBackend for RecordingPaneBackend {
        fn view_pane(&self, pane_id: &str, _backend_type: Option<&str>) -> anyhow::Result<bool> {
            self.viewed.lock().unwrap().push(pane_id.into());
            Ok(true)
        }

        fn kill_pane(&self, pane_id: &str, _backend_type: Option<&str>) -> anyhow::Result<bool> {
            self.killed.lock().unwrap().push(pane_id.into());
            Ok(true)
        }

        fn set_hidden(
            &self,
            pane_id: &str,
            _backend_type: Option<&str>,
            hide: bool,
        ) -> anyhow::Result<bool> {
            self.hidden.lock().unwrap().push((pane_id.into(), hide));
            Ok(true)
        }
    }

    fn insert_teammate(registry: &TaskRegistry, id: &str, team: &str) -> TaskId {
        register_in_process_teammate_task(
            registry,
            InProcessTeammateTaskSpec {
                id: TaskId::new(id),
                identity: TeammateIdentity {
                    agent_id: format!("{id}@{team}"),
                    agent_name: id.into(),
                    team_name: team.into(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "leader".into(),
                },
                prompt: format!("prompt-{id}"),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                agent_type: None,
                description: None,
            },
        )
    }

    fn write_tmux_team(team: &str, member_name: &str, pane_id: &str) {
        write_team_file(
            team,
            &TeamFile {
                name: team.into(),
                description: None,
                created_at: 0,
                lead_agent_id: format!("team-lead@{team}"),
                lead_session_id: None,
                hidden_pane_ids: Vec::new(),
                members: vec![TeamMember {
                    agent_id: format!("{member_name}@{team}"),
                    name: member_name.into(),
                    agent_type: None,
                    model: Some("gpt".into()),
                    model_profile: None,
                    prompt: Some("Investigate".into()),
                    color: Some("red".into()),
                    plan_mode_required: None,
                    joined_at: 0,
                    tmux_pane_id: pane_id.into(),
                    cwd: ".".into(),
                    worktree_path: None,
                    backend_type: Some("tmux".into()),
                    is_active: Some(true),
                    mode: Some("default".into()),
                    subscriptions: Vec::new(),
                }],
            },
        )
        .unwrap();
    }

    #[test]
    fn open_uses_first_discovered_team() {
        let _home = TestConfigHome::new("teams-dialog-open");
        let registry = TaskRegistry::new();
        insert_teammate(&registry, "b", "beta");
        insert_teammate(&registry, "a", "alpha");
        let snapshots = registry.snapshots();
        let dialog = TeamsDialogState::open(&snapshots, None).expect("dialog");
        assert_eq!(dialog.current_team_name(), "alpha");
    }

    #[test]
    fn detail_enter_routes_live_teammate_to_task_detail() {
        let _home = TestConfigHome::new("teams-dialog-live-view");
        let registry = TaskRegistry::new();
        insert_teammate(&registry, "alice", "red");
        let snapshots = registry.snapshots();
        let mut dialog = TeamsDialogState::open(&snapshots, None).expect("dialog");
        assert_eq!(
            dialog.handle_key(DialogKey::Enter, &registry),
            DialogKeyOutcome::Consumed
        );
        assert_eq!(
            dialog.handle_key(DialogKey::Enter, &registry),
            DialogKeyOutcome::OpenTaskDetail {
                task_id: "alice".into(),
            }
        );
    }

    #[test]
    fn kill_key_updates_registry_state_for_live_teammate() {
        let _home = TestConfigHome::new("teams-dialog-live-kill");
        let registry = TaskRegistry::new();
        let id = insert_teammate(&registry, "alice", "red");
        let snapshots = registry.snapshots();
        let mut dialog = TeamsDialogState::open(&snapshots, None).expect("dialog");
        assert_eq!(
            dialog.handle_key(DialogKey::plain('k'), &registry),
            DialogKeyOutcome::Consumed
        );
        let snap = registry.snapshot(&id).expect("snapshot");
        assert_eq!(snap.status, TaskStatus::Killed);
    }

    #[test]
    fn hide_key_updates_team_file_for_tmux_teammate() {
        let _home = TestConfigHome::new("teams-dialog-hide");
        let registry = TaskRegistry::new();
        write_tmux_team("red", "alice", "%7");
        let backend = Arc::new(RecordingPaneBackend::default());
        let mut dialog = TeamsDialogState::open_with_backend(
            &[],
            Some("red"),
            TeamPaneBackendHandle::from_arc(backend.clone() as std::sync::Arc<dyn TeamPaneBackend>),
        )
        .expect("dialog");

        assert_eq!(
            dialog.handle_key(DialogKey::plain('h'), &registry),
            DialogKeyOutcome::Consumed
        );
        let team = rebon_tool::read_team_file("red").unwrap().unwrap();
        assert_eq!(team.hidden_pane_ids, vec!["%7"]);
        assert_eq!(backend.hidden()[0], ("%7".into(), true));
    }

    #[test]
    fn enter_on_tmux_teammate_focuses_pane() {
        let _home = TestConfigHome::new("teams-dialog-view");
        let registry = TaskRegistry::new();
        write_tmux_team("red", "alice", "%9");
        let backend = Arc::new(RecordingPaneBackend::default());
        let mut dialog = TeamsDialogState::open_with_backend(
            &[],
            Some("red"),
            TeamPaneBackendHandle::from_arc(backend.clone() as std::sync::Arc<dyn TeamPaneBackend>),
        )
        .expect("dialog");

        assert_eq!(
            dialog.handle_key(DialogKey::Enter, &registry),
            DialogKeyOutcome::Consumed
        );
        assert_eq!(
            dialog.handle_key(DialogKey::Enter, &registry),
            DialogKeyOutcome::Dismiss
        );
        assert_eq!(backend.viewed()[0], "%9");
    }

    #[test]
    fn kill_tmux_teammate_removes_member_from_team_file() {
        let _home = TestConfigHome::new("teams-dialog-tmux-kill");
        let registry = TaskRegistry::new();
        write_tmux_team("red", "alice", "%11");
        let backend = Arc::new(RecordingPaneBackend::default());
        let mut dialog = TeamsDialogState::open_with_backend(
            &[],
            Some("red"),
            TeamPaneBackendHandle::from_arc(backend.clone() as std::sync::Arc<dyn TeamPaneBackend>),
        )
        .expect("dialog");

        assert_eq!(
            dialog.handle_key(DialogKey::plain('k'), &registry),
            DialogKeyOutcome::Dismiss
        );
        let team = rebon_tool::read_team_file("red").unwrap().unwrap();
        assert!(team.members.is_empty());
        assert_eq!(backend.killed()[0], "%11");
    }

    #[test]
    fn shutdown_tmux_teammate_writes_mailbox_request() {
        let _home = TestConfigHome::new("teams-dialog-tmux-shutdown");
        let registry = TaskRegistry::new();
        write_tmux_team("red", "alice", "%17");
        let backend = Arc::new(RecordingPaneBackend::default());
        let mut dialog = TeamsDialogState::open_with_backend(
            &[],
            Some("red"),
            TeamPaneBackendHandle::from_arc(backend as std::sync::Arc<dyn TeamPaneBackend>),
        )
        .expect("dialog");

        assert_eq!(
            dialog.handle_key(DialogKey::plain('s'), &registry),
            DialogKeyOutcome::Consumed
        );
        let mailbox = read_mailbox("red", "alice").unwrap();
        assert_eq!(mailbox.len(), 1);
        assert!(mailbox[0].text.contains("shutdown_request"));
    }
}
