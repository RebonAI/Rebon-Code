//! Where a team's teammates come from: live task snapshots, the
//! persisted team files, and the shape [`crate::ui::teams`]'s reducers take.

use std::collections::{BTreeMap, BTreeSet};

use crate::runtime::{TaskData, TaskSnapshot};
use crate::ui::teams::{TeamStatusTeammate, TeammateActivity, TeamsDialogTeammate};
use rebon_tool::{is_session_default_team_name, list_team_files, TeamFile, TeamMember};

const TEAM_LEAD_NAME: &str = "team-lead";

/// One discovered team plus the teammates that belong to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamDialogSummary {
    /// Team name.
    pub name: String,
    /// Teammates in display order.
    pub teammates: Vec<TeamDialogEntry>,
}

/// Runtime metadata the reducer in [`crate::ui::teams`] does not own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamDialogEntry {
    /// Pure reducer input.
    pub teammate: TeamsDialogTeammate,
    /// Persisted backend type, when known.
    pub backend_type: Option<String>,
    /// Pane id recorded in the team file for pane-backed teammates.
    pub pane_id: String,
    /// Whether a live in-process registry task currently backs this teammate.
    pub is_in_process: bool,
}

#[derive(Debug, Clone)]
struct LiveTeammate {
    task_id: String,
    agent_id: String,
    name: String,
    model: Option<String>,
    prompt: String,
    color: Option<String>,
    status: TeammateActivity,
    mode: String,
}

/// Build the teammate list consumed by `render_team_status`.
pub fn build_team_status_input(snapshots: &[TaskSnapshot]) -> Vec<TeamStatusTeammate> {
    snapshots
        .iter()
        .filter_map(|snap| match &snap.data {
            TaskData::InProcessTeammate(data) if !snap.status.is_terminal() => {
                Some(TeamStatusTeammate {
                    name: data.identity.agent_name.clone(),
                    is_team_lead: false,
                })
            }
            _ => None,
        })
        .collect()
}

/// Group teammates by team name, merging persisted team-file members
/// with live registry state.
pub fn build_team_dialog_summaries(snapshots: &[TaskSnapshot]) -> Vec<TeamDialogSummary> {
    let live_by_team = collect_live_teammates(snapshots);
    let mut summaries = Vec::new();
    let mut seen_teams = BTreeSet::new();

    if let Ok(team_files) = list_team_files() {
        for team in team_files {
            if is_session_default_team_name(&team.name) && !live_by_team.contains_key(&team.name) {
                continue;
            }
            seen_teams.insert(team.name.clone());
            summaries.push(build_summary_from_team_file(
                &team,
                live_by_team.get(&team.name),
            ));
        }
    }

    for (team_name, live_team) in &live_by_team {
        if seen_teams.contains(team_name) {
            continue;
        }
        summaries.push(TeamDialogSummary {
            name: team_name.clone(),
            teammates: build_entries_from_live_only(live_team),
        });
    }

    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    summaries
}

fn collect_live_teammates(
    snapshots: &[TaskSnapshot],
) -> BTreeMap<String, BTreeMap<String, LiveTeammate>> {
    let mut grouped = BTreeMap::new();

    for snap in snapshots {
        let TaskData::InProcessTeammate(data) = &snap.data else {
            continue;
        };
        if snap.status.is_terminal() {
            continue;
        }
        grouped
            .entry(data.identity.team_name.clone())
            .or_insert_with(BTreeMap::new)
            .insert(
                data.identity.agent_id.clone(),
                LiveTeammate {
                    task_id: snap.id.as_str().to_owned(),
                    agent_id: data.identity.agent_id.clone(),
                    name: data.identity.agent_name.clone(),
                    model: data.model.clone(),
                    prompt: data.prompt.clone(),
                    color: data.identity.color.clone(),
                    status: if data.is_idle {
                        TeammateActivity::Idle
                    } else {
                        TeammateActivity::Running
                    },
                    mode: data.permission_mode.clone(),
                },
            );
    }

    grouped
}

fn build_summary_from_team_file(
    team: &TeamFile,
    live: Option<&BTreeMap<String, LiveTeammate>>,
) -> TeamDialogSummary {
    let mut entries = Vec::new();
    let hidden_panes: BTreeSet<&str> = team.hidden_pane_ids.iter().map(String::as_str).collect();
    let live = live.cloned().unwrap_or_default();
    let mut seen_agent_ids = BTreeSet::new();

    for member in &team.members {
        if member.name == TEAM_LEAD_NAME {
            continue;
        }
        seen_agent_ids.insert(member.agent_id.clone());
        let live_member = live.get(&member.agent_id);
        entries.push(build_entry_from_member(member, live_member, &hidden_panes));
    }

    for live_member in live.values() {
        if seen_agent_ids.contains(&live_member.agent_id) {
            continue;
        }
        entries.push(build_entry_from_live_only(live_member));
    }

    entries.sort_by(|a, b| a.teammate.name.cmp(&b.teammate.name));
    TeamDialogSummary {
        name: team.name.clone(),
        teammates: entries,
    }
}

fn build_entries_from_live_only(live: &BTreeMap<String, LiveTeammate>) -> Vec<TeamDialogEntry> {
    let mut entries: Vec<_> = live.values().map(build_entry_from_live_only).collect();
    entries.sort_by(|a, b| a.teammate.name.cmp(&b.teammate.name));
    entries
}

fn build_entry_from_member(
    member: &TeamMember,
    live: Option<&LiveTeammate>,
    hidden_panes: &BTreeSet<&str>,
) -> TeamDialogEntry {
    let backend_type = member.backend_type.clone();
    let pane_id = member.tmux_pane_id.clone();
    let can_hide = backend_type.as_deref() == Some("tmux") && !pane_id.is_empty();
    let is_hidden = can_hide && hidden_panes.contains(pane_id.as_str());

    let teammate = TeamsDialogTeammate {
        name: member.name.clone(),
        target_id: live
            .map(|live| live.task_id.clone())
            .unwrap_or_else(|| fallback_target_id(member)),
        agent_id: member.agent_id.clone(),
        model: live
            .and_then(|live| live.model.clone())
            .or_else(|| member.model.clone()),
        prompt: Some(
            live.map(|live| live.prompt.clone())
                .or_else(|| member.prompt.clone())
                .unwrap_or_default(),
        ),
        status: live.map(|live| live.status).unwrap_or_else(|| {
            if member.is_active == Some(false) {
                TeammateActivity::Idle
            } else {
                TeammateActivity::Running
            }
        }),
        color: live
            .and_then(|live| live.color.clone())
            .or_else(|| member.color.clone()),
        is_hidden,
        can_hide,
        mode: live
            .map(|live| live.mode.clone())
            .or_else(|| member.mode.clone()),
    };

    TeamDialogEntry {
        teammate,
        backend_type,
        pane_id,
        is_in_process: live.is_some(),
    }
}

fn build_entry_from_live_only(live: &LiveTeammate) -> TeamDialogEntry {
    TeamDialogEntry {
        teammate: TeamsDialogTeammate {
            name: live.name.clone(),
            target_id: live.task_id.clone(),
            agent_id: live.agent_id.clone(),
            model: live.model.clone(),
            prompt: Some(live.prompt.clone()),
            status: live.status,
            color: live.color.clone(),
            is_hidden: false,
            can_hide: false,
            mode: Some(live.mode.clone()),
        },
        backend_type: Some("in-process".into()),
        pane_id: live.task_id.clone(),
        is_in_process: true,
    }
}

fn fallback_target_id(member: &TeamMember) -> String {
    if member.tmux_pane_id.is_empty() {
        member.agent_id.clone()
    } else {
        member.tmux_pane_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        InProcessTeammateData, TaskData, TaskId, TaskKind, TaskSnapshot, TaskStatus,
        TeammateIdentity,
    };
    use rebon_tool::tasks::test_support::TestConfigHome;
    use rebon_tool::{write_team_file, TeamFile, TeamMember};

    fn now_wall_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    fn teammate_snapshot(id: &str, team: &str, idle: bool, status: TaskStatus) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::InProcessTeammate,
            status,
            title: format!("@{id}"),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: serde_json::json!({}),
            data: TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
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
                awaiting_plan_approval: false,
                is_idle: idle,
                shutdown_requested: false,
                pending_user_messages: Vec::new(),
                tool_use_count: 0,
                token_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
            })),
        }
    }

    #[test]
    fn team_status_input_collects_only_live_teammates() {
        let snapshots = vec![
            teammate_snapshot("alice", "red", false, TaskStatus::Running),
            teammate_snapshot("bob", "red", true, TaskStatus::Killed),
        ];
        let input = build_team_status_input(&snapshots);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0].name, "alice");
    }

    #[test]
    fn dialog_summaries_merge_team_file_with_live_snapshot() {
        let _home = TestConfigHome::new("team-dialog-summary");
        write_team_file(
            "red",
            &TeamFile {
                name: "red".into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: "team-lead@red".into(),
                lead_session_id: None,
                hidden_pane_ids: vec!["%7".into()],
                members: vec![
                    TeamMember {
                        agent_id: "alice@red".into(),
                        name: "alice".into(),
                        agent_type: None,
                        model: Some("gpt".into()),
                        model_profile: None,
                        prompt: Some("Investigate".into()),
                        color: Some("red".into()),
                        plan_mode_required: None,
                        joined_at: now_wall_ms(),
                        tmux_pane_id: "%7".into(),
                        cwd: ".".into(),
                        worktree_path: None,
                        backend_type: Some("tmux".into()),
                        is_active: Some(true),
                        mode: Some("default".into()),
                        subscriptions: Vec::new(),
                    },
                    TeamMember {
                        agent_id: "bob@red".into(),
                        name: "bob".into(),
                        agent_type: None,
                        model: None,
                        model_profile: None,
                        prompt: Some("Ship it".into()),
                        color: None,
                        plan_mode_required: None,
                        joined_at: now_wall_ms(),
                        tmux_pane_id: "bob-stale".into(),
                        cwd: ".".into(),
                        worktree_path: None,
                        backend_type: Some("in-process".into()),
                        is_active: Some(true),
                        mode: Some("plan".into()),
                        subscriptions: Vec::new(),
                    },
                ],
            },
        )
        .unwrap();

        let summaries = build_team_dialog_summaries(&[teammate_snapshot(
            "bob",
            "red",
            true,
            TaskStatus::Running,
        )]);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "red");
        assert_eq!(summaries[0].teammates.len(), 2);
        assert!(summaries[0].teammates[0].teammate.can_hide);
        assert!(summaries[0].teammates[0].teammate.is_hidden);
        assert_eq!(
            summaries[0].teammates[0].backend_type.as_deref(),
            Some("tmux")
        );
        assert!(summaries[0].teammates[1].is_in_process);
        assert_eq!(
            summaries[0].teammates[1].teammate.status,
            TeammateActivity::Idle
        );
        assert_eq!(summaries[0].teammates[1].teammate.target_id, "bob");
    }

    #[test]
    fn dialog_summaries_hide_default_teams_without_live_teammates() {
        let _home = TestConfigHome::new("team-dialog-hidden-default");
        let default_team = rebon_tool::ensure_session_default_team("session-a").unwrap();
        write_team_file(
            "explicit-team",
            &TeamFile {
                name: "explicit-team".into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: "team-lead@explicit-team".into(),
                lead_session_id: None,
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();

        let summaries = build_team_dialog_summaries(&[]);
        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.name.as_str())
                .collect::<Vec<_>>(),
            vec!["explicit-team"]
        );

        let summaries = build_team_dialog_summaries(&[teammate_snapshot(
            "alice",
            &default_team,
            true,
            TaskStatus::Running,
        )]);
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().any(|summary| summary.name == default_team));
    }

    #[test]
    fn dialog_summaries_include_live_team_without_team_file() {
        let _home = TestConfigHome::new("team-dialog-live-only");
        let summaries = build_team_dialog_summaries(&[
            teammate_snapshot("zoe", "red", false, TaskStatus::Running),
            teammate_snapshot("amy", "red", true, TaskStatus::Running),
        ]);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "red");
        assert_eq!(summaries[0].teammates[0].teammate.name, "amy");
        assert_eq!(summaries[0].teammates[1].teammate.name, "zoe");
    }
}
