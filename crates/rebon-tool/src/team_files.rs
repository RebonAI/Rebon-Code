use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tasks::{config_home_dir, sanitize_path_component, tasks_dir, FileLock};

/// Team leader's stable display name.
pub const TEAM_LEAD_NAME: &str = "team-lead";

/// One team member entry persisted in `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    /// Unique agent id (`name@team`).
    #[serde(rename = "agentId")]
    pub agent_id: String,
    /// Human-readable name.
    pub name: String,
    /// Optional agent type / role.
    #[serde(rename = "agentType", default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Optional model label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional model profile label.
    #[serde(
        rename = "modelProfile",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub model_profile: Option<String>,
    /// Optional prompt summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Optional color label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Whether plan approval is required.
    #[serde(
        rename = "planModeRequired",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub plan_mode_required: Option<bool>,
    /// Join timestamp.
    #[serde(rename = "joinedAt")]
    pub joined_at: u64,
    /// Pane/task handle. In-process teammates leave this empty.
    #[serde(rename = "tmuxPaneId")]
    pub tmux_pane_id: String,
    /// Working directory.
    pub cwd: String,
    /// Optional worktree path.
    #[serde(
        rename = "worktreePath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub worktree_path: Option<String>,
    /// Optional backend type. In-process teammates write
    /// `"in-process"`; the reserved future value `"acp:<agent-id>"`
    /// marks a teammate whose turns run on an external ACP agent, so
    /// no schema change is needed when that lands.
    #[serde(
        rename = "backendType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub backend_type: Option<String>,
    /// Optional activity flag.
    #[serde(rename = "isActive", default, skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
    /// Optional permission mode snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Subscriptions list.
    #[serde(default)]
    pub subscriptions: Vec<String>,
}

/// Persisted team file shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamFile {
    /// Team name.
    pub name: String,
    /// Optional description/purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Creation timestamp.
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// Leader agent id.
    #[serde(rename = "leadAgentId")]
    pub lead_agent_id: String,
    /// Optional leader session id.
    #[serde(
        rename = "leadSessionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub lead_session_id: Option<String>,
    /// Hidden pane ids.
    #[serde(
        rename = "hiddenPaneIds",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hidden_pane_ids: Vec<String>,
    /// Team members.
    pub members: Vec<TeamMember>,
}

/// Current wall-clock milliseconds since Unix epoch.
pub fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Sanitize a team name for file paths.
pub fn sanitize_team_name(name: &str) -> String {
    sanitize_path_component(name).to_lowercase()
}

/// Sanitize an agent name before embedding it in `agent@team`.
pub fn sanitize_agent_name(name: &str) -> String {
    name.replace('@', "-")
}

/// Format a deterministic agent id.
pub fn format_agent_id(name: &str, team_name: &str) -> String {
    format!("{}@{}", sanitize_agent_name(name), team_name)
}

static DEFAULT_TEAM_PROCESS_NONCE: OnceLock<String> = OnceLock::new();

fn default_team_process_nonce() -> &'static str {
    DEFAULT_TEAM_PROCESS_NONCE
        .get_or_init(|| format!("{:x}-{:x}", std::process::id(), now_wall_ms()))
        .as_str()
}

pub fn default_team_name(session_id: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_id.hash(&mut hasher);
    format!(
        "__rebon-default-{:016x}-{}",
        hasher.finish(),
        default_team_process_nonce()
    )
}

pub fn is_session_default_team_name(team_name: &str) -> bool {
    team_name.starts_with("__rebon-default-")
}

fn session_default_team_process_id(team_name: &str) -> Option<u32> {
    let suffix = team_name.strip_prefix("__rebon-default-")?;
    let mut parts = suffix.split('-');
    let _session_hash = parts.next()?;
    let process_id = parts.next()?;
    let _started_at = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    u32::from_str_radix(process_id, 16).ok()
}

#[cfg(windows)]
fn process_is_alive(process_id: u32) -> bool {
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if handle == 0 {
        return false;
    }
    let mut exit_code = 0;
    let read = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
    read != 0 && exit_code == windows_sys::Win32::Foundation::STILL_ACTIVE as u32
}

#[cfg(unix)]
fn process_is_alive(process_id: u32) -> bool {
    let Ok(process_id) = i32::try_from(process_id) else {
        return false;
    };
    let result = unsafe { libc::kill(process_id, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_process_id: u32) -> bool {
    true
}

pub fn ensure_session_default_team(session_id: &str) -> Result<String> {
    let team_name = default_team_name(session_id);
    if read_team_file(&team_name)?.is_some() {
        return Ok(team_name);
    }

    let _lock = FileLock::acquire(&team_lock_path(&team_name))?;
    if read_team_file(&team_name)?.is_some() {
        return Ok(team_name);
    }

    let lead_agent_id = format_agent_id(TEAM_LEAD_NAME, &team_name);
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    write_team_file(
        &team_name,
        &TeamFile {
            name: team_name.clone(),
            description: Some("Session default team".into()),
            created_at: now_wall_ms(),
            lead_agent_id: lead_agent_id.clone(),
            lead_session_id: Some(session_id.to_string()),
            hidden_pane_ids: Vec::new(),
            members: vec![TeamMember {
                agent_id: lead_agent_id,
                name: TEAM_LEAD_NAME.into(),
                agent_type: Some("main".into()),
                model: None,
                model_profile: None,
                prompt: None,
                color: None,
                plan_mode_required: None,
                joined_at: now_wall_ms(),
                tmux_pane_id: String::new(),
                cwd,
                worktree_path: None,
                backend_type: None,
                is_active: Some(true),
                mode: Some("default".into()),
                subscriptions: Vec::new(),
            }],
        },
    )?;
    Ok(team_name)
}

pub fn remove_session_default_team(session_id: &str) -> Result<bool> {
    let mut team_names = list_team_files()?
        .into_iter()
        .filter(|team| {
            is_session_default_team_name(&team.name)
                && team.lead_session_id.as_deref() == Some(session_id)
        })
        .map(|team| team.name)
        .collect::<Vec<_>>();
    let current_team_name = default_team_name(session_id);
    if !team_names.contains(&current_team_name) {
        team_names.push(current_team_name);
    }

    let mut removed = false;
    for team_name in team_names {
        removed |= remove_session_default_team_data(&team_name)?;
    }
    Ok(removed)
}

fn remove_session_default_team_data(team_name: &str) -> Result<bool> {
    let mut removed = false;
    for dir in [team_dir(team_name), tasks_dir(team_name)] {
        match fs::remove_dir_all(&dir) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove session default team data {}",
                        dir.display()
                    )
                })
            }
        }
    }
    Ok(removed)
}

/// Directory that holds all team configs.
pub fn teams_dir() -> PathBuf {
    config_home_dir().join("teams")
}

/// Team directory path.
pub fn team_dir(team_name: &str) -> PathBuf {
    teams_dir().join(sanitize_team_name(team_name))
}

/// Team config file path.
pub fn team_file_path(team_name: &str) -> PathBuf {
    team_dir(team_name).join("config.json")
}

/// Advisory lock path guarding read-modify-write mutations of a team config.
/// Analogous to `crate::tasks::list_lock_path` for task lists.
fn team_lock_path(team_name: &str) -> PathBuf {
    team_dir(team_name).join(".lock")
}

/// Current team name bound to this process/session, if any.
pub fn current_team_name() -> Option<String> {
    std::env::var("REBON_TEAM_NAME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Set the process-global current team name.
pub fn set_current_team_name(team_name: &str) {
    std::env::set_var("REBON_TEAM_NAME", team_name);
}

/// Clear the process-global current team name.
pub fn clear_current_team_name() {
    std::env::remove_var("REBON_TEAM_NAME");
}

/// Read a team config from disk.
pub fn read_team_file(team_name: &str) -> Result<Option<TeamFile>> {
    let path = team_file_path(team_name);
    match fs::read_to_string(&path) {
        Ok(content) => {
            let parsed = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse team file {}", path.display()))?;
            Ok(Some(parsed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Write a team config to disk.
///
/// The final swap is atomic: bytes are staged to a sibling temp file and then
/// renamed over the target, so a concurrent reader (which does not take the
/// lock) never observes a truncated file. Callers doing a read-modify-write
/// must still hold [`team_lock_path`] via [`FileLock`] to avoid lost updates.
pub fn write_team_file(team_name: &str, team: &TeamFile) -> Result<()> {
    let dir = team_dir(team_name);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create team dir {}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(team)
        .with_context(|| format!("failed to serialize team {}", team.name))?;
    let path = team_file_path(team_name);
    write_atomic(&path, &bytes)
}

/// Write `bytes` to `path` atomically through the shared staged write, so a
/// concurrent reader never observes a truncated team file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    rebon_session::write_file_atomically(path, bytes)
        .with_context(|| format!("failed to replace {}", path.display()))
}

/// Read every team config from disk, ordered by sanitized team name.
pub fn list_team_files() -> Result<Vec<TeamFile>> {
    list_team_files_with_process_liveness(process_is_alive)
}

fn list_team_files_with_process_liveness(
    process_is_alive: impl Fn(u32) -> bool,
) -> Result<Vec<TeamFile>> {
    let dir = teams_dir();
    let mut teams = Vec::new();
    let mut stale_default_teams = Vec::new();
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.with_context(|| format!("failed to read {}", dir.display()))?;
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let Some(team_name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if session_default_team_process_id(&team_name)
                    .is_some_and(|process_id| !process_is_alive(process_id))
                {
                    stale_default_teams.push(team_name);
                    continue;
                }
                if let Some(team) = read_team_file(&team_name)? {
                    teams.push(team);
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read teams dir {}", dir.display()))
        }
    }
    for team_name in stale_default_teams {
        if let Err(error) = remove_session_default_team_data(&team_name) {
            tracing::warn!(
                team_name,
                %error,
                "failed to remove stale session default team"
            );
        }
    }
    teams.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(teams)
}

/// Resolve the team currently owned by a main session.
///
/// Explicit teams take precedence over the hidden session-default team. Within
/// the same class, the newest config wins so stale duplicate files do not make
/// routing depend on directory iteration order.
pub fn team_name_for_session(session_id: &str) -> Result<Option<String>> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Ok(None);
    }
    let mut teams = list_team_files()?
        .into_iter()
        .filter(|team| team.lead_session_id.as_deref() == Some(session_id))
        .collect::<Vec<_>>();
    teams.sort_by(|left, right| {
        is_session_default_team_name(&left.name)
            .cmp(&is_session_default_team_name(&right.name))
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(teams.into_iter().next().map(|team| team.name))
}

/// Bind an existing team to one main session. Rebinding to the same session is
/// idempotent; binding a team owned by another session fails without mutation.
pub fn bind_session_team(team_name: &str, session_id: &str) -> Result<bool> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Ok(false);
    }
    if read_team_file(team_name)?.is_none() {
        return Err(anyhow!(missing_team_error(team_name)));
    }
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Err(anyhow!(missing_team_error(team_name)));
    };
    match team.lead_session_id.as_deref() {
        Some(owner) if owner == session_id => Ok(false),
        Some(owner) => Err(anyhow!(
            "team `{team_name}` belongs to session `{owner}` and cannot be used by session `{session_id}`"
        )),
        None => {
            team.lead_session_id = Some(session_id.to_string());
            write_team_file(team_name, &team)?;
            Ok(true)
        }
    }
}

/// Clear every persisted team binding owned by one main session while keeping
/// explicit team files intact. Session-default team directories are removed by
/// [`remove_session_default_team`] after this binding cleanup.
pub fn clear_session_team_bindings(session_id: &str) -> Result<usize> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Ok(0);
    }
    let team_names = list_team_files()?
        .into_iter()
        .filter(|team| {
            !is_session_default_team_name(&team.name)
                && team.lead_session_id.as_deref() == Some(session_id)
        })
        .map(|team| team.name)
        .collect::<Vec<_>>();
    let mut cleared = 0;
    for team_name in team_names {
        let _lock = FileLock::acquire(&team_lock_path(&team_name))?;
        let Some(mut team) = read_team_file(&team_name)? else {
            continue;
        };
        if team.lead_session_id.as_deref() != Some(session_id) {
            continue;
        }
        team.lead_session_id = None;
        write_team_file(&team_name, &team)?;
        cleared += 1;
    }
    Ok(cleared)
}

/// Compute a unique team name by suffixing `-N` when needed.
pub fn unique_team_name(base: &str) -> Result<String> {
    let sanitized = sanitize_team_name(base);
    if sanitized.is_empty() {
        return Err(anyhow!(
            "team_name must contain at least one alphanumeric character"
        ));
    }
    if read_team_file(&sanitized)?.is_none() {
        return Ok(sanitized);
    }
    for idx in 2..1000 {
        let candidate = format!("{sanitized}-{idx}");
        if read_team_file(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "failed to generate a unique team name for `{base}`"
    ))
}

/// Actionable error for operations that require an existing team. Lists
/// the teams that do exist so a caller (typically a model retrying an
/// Agent spawn) can correct the name in one step instead of guessing.
pub fn missing_team_error(team_name: &str) -> String {
    let existing = list_team_files()
        .map(|teams| {
            teams
                .into_iter()
                .filter(|team| !is_session_default_team_name(&team.name))
                .map(|team| team.name)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    format!(
        "team `{team_name}` does not exist (existing teams: [{existing}]); \
         omit team_name to spawn a standalone agent, or create the team \
         first with TeamCreate"
    )
}

/// Append a member to an existing team, replacing any previous member with
/// the same `agent_id`.
pub fn append_team_member(team_name: &str, member: TeamMember) -> Result<()> {
    // Probe before taking the lock: acquiring the lock creates the team
    // directory, which would leave an empty dir behind for a team that
    // never existed.
    if read_team_file(team_name)?.is_none() {
        return Err(anyhow!(missing_team_error(team_name)));
    }
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Err(anyhow!(missing_team_error(team_name)));
    };
    team.members
        .retain(|existing| existing.agent_id != member.agent_id);
    team.members.push(member);
    write_team_file(team_name, &team)
}

/// Remove a member from a team by agent id.
pub fn remove_member_by_agent_id(team_name: &str, agent_id: &str) -> Result<bool> {
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Ok(false);
    };
    let original_len = team.members.len();
    team.members.retain(|member| member.agent_id != agent_id);
    if team.members.len() == original_len {
        return Ok(false);
    }
    write_team_file(team_name, &team)?;
    Ok(true)
}

/// Update whether a persisted member is actively executing a turn.
pub fn set_team_member_active(team_name: &str, member_name: &str, is_active: bool) -> Result<bool> {
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Ok(false);
    };
    let mut updated = false;
    for member in &mut team.members {
        if member.name == member_name && member.is_active != Some(is_active) {
            member.is_active = Some(is_active);
            updated = true;
        }
    }
    if updated {
        write_team_file(team_name, &team)?;
    }
    Ok(updated)
}

/// Update a member's stored mode snapshot.
pub fn set_team_member_mode(team_name: &str, member_name: &str, mode: &str) -> Result<bool> {
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Ok(false);
    };
    let mut updated = false;
    for member in &mut team.members {
        if member.name == member_name && member.mode.as_deref() != Some(mode) {
            member.mode = Some(mode.to_string());
            updated = true;
        }
    }
    if updated {
        write_team_file(team_name, &team)?;
    }
    Ok(updated)
}

/// Add a pane id to the team's hidden pane list.
pub fn add_hidden_pane_id(team_name: &str, pane_id: &str) -> Result<bool> {
    if pane_id.is_empty() {
        return Ok(false);
    }
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Ok(false);
    };
    if team.hidden_pane_ids.iter().any(|id| id == pane_id) {
        return Ok(false);
    }
    team.hidden_pane_ids.push(pane_id.to_string());
    write_team_file(team_name, &team)?;
    Ok(true)
}

/// Remove a pane id from the team's hidden pane list.
pub fn remove_hidden_pane_id(team_name: &str, pane_id: &str) -> Result<bool> {
    if pane_id.is_empty() {
        return Ok(false);
    }
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Ok(false);
    };
    let original_len = team.hidden_pane_ids.len();
    team.hidden_pane_ids.retain(|id| id != pane_id);
    if team.hidden_pane_ids.len() == original_len {
        return Ok(false);
    }
    write_team_file(team_name, &team)?;
    Ok(true)
}

/// Remove a member from a team by pane id and clear any matching hidden-pane state.
pub fn remove_member_by_pane_id(team_name: &str, pane_id: &str) -> Result<bool> {
    if pane_id.is_empty() {
        return Ok(false);
    }
    let _lock = FileLock::acquire(&team_lock_path(team_name))?;
    let Some(mut team) = read_team_file(team_name)? else {
        return Ok(false);
    };
    let original_len = team.members.len();
    team.members.retain(|member| member.tmux_pane_id != pane_id);
    if team.members.len() == original_len {
        return Ok(false);
    }
    team.hidden_pane_ids.retain(|id| id != pane_id);
    write_team_file(team_name, &team)?;
    Ok(true)
}

/// Ensure the on-disk task list directory exists for the team.
pub fn ensure_team_task_dir(team_name: &str) -> Result<()> {
    let dir = tasks_dir(team_name);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create team task dir {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::test_support::TestConfigHome;

    #[test]
    fn session_default_team_is_idempotent_isolated_and_removable() {
        let _home = TestConfigHome::new("session-default-team");
        let first = ensure_session_default_team("session-a").unwrap();
        let repeated = ensure_session_default_team("session-a").unwrap();
        let other = ensure_session_default_team("session-b").unwrap();

        assert_eq!(first, repeated);
        assert_ne!(first, other);
        assert!(is_session_default_team_name(&first));
        assert_eq!(
            session_default_team_process_id(&first),
            Some(std::process::id())
        );
        assert!(current_team_name().is_none());
        let team = read_team_file(&first).unwrap().unwrap();
        assert_eq!(team.lead_session_id.as_deref(), Some("session-a"));
        assert_eq!(team.description.as_deref(), Some("Session default team"));
        assert_eq!(team.members.len(), 1);
        assert_eq!(team.members[0].name, TEAM_LEAD_NAME);

        ensure_team_task_dir(&first).unwrap();
        assert!(team_dir(&first).is_dir());
        assert!(tasks_dir(&first).is_dir());
        assert!(remove_session_default_team("session-a").unwrap());
        assert!(!team_dir(&first).exists());
        assert!(!tasks_dir(&first).exists());
        assert!(!remove_session_default_team("session-a").unwrap());
        assert!(team_dir(&other).is_dir());
    }

    #[test]
    fn list_team_files_prunes_default_teams_owned_by_dead_processes() {
        let _home = TestConfigHome::new("stale-session-default-team");
        let stale_team = "__rebon-default-0000000000000000-dead-beef";
        write_team_file(
            stale_team,
            &TeamFile {
                name: stale_team.into(),
                description: Some("Session default team".into()),
                created_at: now_wall_ms(),
                lead_agent_id: format!("team-lead@{stale_team}"),
                lead_session_id: Some("stale-session".into()),
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();
        ensure_team_task_dir(stale_team).unwrap();
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

        let teams =
            list_team_files_with_process_liveness(|process_id| process_id != 0xdead).unwrap();

        assert_eq!(
            teams.into_iter().map(|team| team.name).collect::<Vec<_>>(),
            vec!["explicit-team"]
        );
        assert!(!team_dir(stale_team).exists());
        assert!(!tasks_dir(stale_team).exists());
    }

    #[test]
    fn remove_session_default_team_removes_all_process_generations() {
        let _home = TestConfigHome::new("all-session-default-teams");
        let team_names = ["__rebon-default-legacy-one", "__rebon-default-legacy-two"];
        for team_name in team_names {
            write_team_file(
                team_name,
                &TeamFile {
                    name: team_name.into(),
                    description: Some("Session default team".into()),
                    created_at: now_wall_ms(),
                    lead_agent_id: format!("team-lead@{team_name}"),
                    lead_session_id: Some("session-a".into()),
                    hidden_pane_ids: Vec::new(),
                    members: Vec::new(),
                },
            )
            .unwrap();
            ensure_team_task_dir(team_name).unwrap();
        }

        assert!(remove_session_default_team("session-a").unwrap());
        for team_name in team_names {
            assert!(!team_dir(team_name).exists());
            assert!(!tasks_dir(team_name).exists());
        }
    }

    #[test]
    fn team_name_for_session_is_isolated_and_prefers_explicit_team() {
        let _home = TestConfigHome::new("session-team-lookup");
        let default_team = ensure_session_default_team("session-a").unwrap();
        write_team_file(
            "explicit-team",
            &TeamFile {
                name: "explicit-team".into(),
                description: None,
                created_at: now_wall_ms() + 1,
                lead_agent_id: "team-lead@explicit-team".into(),
                lead_session_id: Some("session-a".into()),
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();

        assert_eq!(
            team_name_for_session("session-a").unwrap().as_deref(),
            Some("explicit-team")
        );
        assert_eq!(team_name_for_session("session-b").unwrap(), None);
        assert!(read_team_file(&default_team).unwrap().is_some());
    }

    #[test]
    fn bind_session_team_is_idempotent_and_rejects_other_session() {
        let _home = TestConfigHome::new("session-team-bind");
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

        assert!(bind_session_team("explicit-team", "session-a").unwrap());
        assert!(!bind_session_team("explicit-team", "session-a").unwrap());
        let error = bind_session_team("explicit-team", "session-b").unwrap_err();
        assert!(error.to_string().contains("belongs to session `session-a`"));
        assert_eq!(
            read_team_file("explicit-team")
                .unwrap()
                .unwrap()
                .lead_session_id
                .as_deref(),
            Some("session-a")
        );
        assert!(!bind_session_team("explicit-team", "").unwrap());
    }

    #[test]
    fn clear_session_team_bindings_keeps_explicit_team_and_isolates_other_sessions() {
        let _home = TestConfigHome::new("session-team-clear");
        let default_team = ensure_session_default_team("session-a").unwrap();
        for (name, session_id) in [("explicit-team", "session-a"), ("other-team", "session-b")] {
            write_team_file(
                name,
                &TeamFile {
                    name: name.into(),
                    description: None,
                    created_at: now_wall_ms(),
                    lead_agent_id: format!("team-lead@{name}"),
                    lead_session_id: Some(session_id.into()),
                    hidden_pane_ids: Vec::new(),
                    members: Vec::new(),
                },
            )
            .unwrap();
        }

        assert_eq!(clear_session_team_bindings("session-a").unwrap(), 1);
        assert_eq!(
            team_name_for_session("session-a").unwrap().as_deref(),
            Some(default_team.as_str())
        );
        assert!(read_team_file("explicit-team").unwrap().is_some());
        assert_eq!(
            read_team_file("explicit-team")
                .unwrap()
                .unwrap()
                .lead_session_id,
            None
        );
        assert_eq!(
            read_team_file(&default_team)
                .unwrap()
                .unwrap()
                .lead_session_id
                .as_deref(),
            Some("session-a")
        );
        assert!(remove_session_default_team("session-a").unwrap());
        assert_eq!(team_name_for_session("session-a").unwrap(), None);
        assert_eq!(
            team_name_for_session("session-b").unwrap().as_deref(),
            Some("other-team")
        );
    }

    #[test]
    fn unique_team_name_suffixes_when_existing_file_present() {
        let _home = TestConfigHome::new("team-files");
        let team = TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            hidden_pane_ids: Vec::new(),
            members: Vec::new(),
        };
        write_team_file("alpha", &team).unwrap();
        assert_eq!(unique_team_name("alpha").unwrap(), "alpha-2");
    }

    #[test]
    fn append_and_remove_member_round_trip() {
        let _home = TestConfigHome::new("team-members");
        let team = TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            hidden_pane_ids: Vec::new(),
            members: Vec::new(),
        };
        write_team_file("alpha", &team).unwrap();
        append_team_member(
            "alpha",
            TeamMember {
                agent_id: "alice@alpha".into(),
                name: "alice".into(),
                agent_type: Some("general-purpose".into()),
                model: None,
                model_profile: None,
                prompt: None,
                color: None,
                plan_mode_required: None,
                joined_at: now_wall_ms(),
                tmux_pane_id: String::new(),
                cwd: ".".into(),
                worktree_path: None,
                backend_type: Some("in-process".into()),
                is_active: Some(true),
                mode: Some("default".into()),
                subscriptions: Vec::new(),
            },
        )
        .unwrap();
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert_eq!(read_back.members.len(), 1);
        assert!(remove_member_by_agent_id("alpha", "alice@alpha").unwrap());
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert!(read_back.members.is_empty());
    }

    #[test]
    fn append_member_to_missing_team_errors_without_creating_dir() {
        let _home = TestConfigHome::new("team-missing-append");
        let team = TeamFile {
            name: "real-team".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@real-team".into(),
            lead_session_id: None,
            hidden_pane_ids: Vec::new(),
            members: Vec::new(),
        };
        write_team_file("real-team", &team).unwrap();
        let default_team = ensure_session_default_team("session-a").unwrap();

        let err = append_team_member(
            "ghost-team",
            TeamMember {
                agent_id: "alice@ghost-team".into(),
                name: "alice".into(),
                agent_type: None,
                model: None,
                model_profile: None,
                prompt: None,
                color: None,
                plan_mode_required: None,
                joined_at: now_wall_ms(),
                tmux_pane_id: String::new(),
                cwd: ".".into(),
                worktree_path: None,
                backend_type: None,
                is_active: None,
                mode: None,
                subscriptions: Vec::new(),
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("team `ghost-team` does not exist"), "{err}");
        assert!(err.contains("real-team"), "{err}");
        assert!(!err.contains(&default_team), "{err}");
        assert!(err.contains("TeamCreate"), "{err}");
        assert!(
            !team_dir("ghost-team").exists(),
            "failed append must not leave an empty team dir behind"
        );
    }

    #[test]
    fn set_team_member_mode_updates_member() {
        let _home = TestConfigHome::new("team-mode");
        let team = TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            hidden_pane_ids: Vec::new(),
            members: vec![TeamMember {
                agent_id: "alice@alpha".into(),
                name: "alice".into(),
                agent_type: None,
                model: None,
                model_profile: None,
                prompt: None,
                color: None,
                plan_mode_required: None,
                joined_at: now_wall_ms(),
                tmux_pane_id: String::new(),
                cwd: ".".into(),
                worktree_path: None,
                backend_type: None,
                is_active: None,
                mode: Some("default".into()),
                subscriptions: Vec::new(),
            }],
        };
        write_team_file("alpha", &team).unwrap();
        assert!(set_team_member_mode("alpha", "alice", "plan").unwrap());
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert_eq!(read_back.members[0].mode.as_deref(), Some("plan"));
    }

    #[test]
    fn set_team_member_active_updates_idle_state() {
        let _home = TestConfigHome::new("team-active");
        let team = TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            hidden_pane_ids: Vec::new(),
            members: vec![TeamMember {
                agent_id: "alice@alpha".into(),
                name: "alice".into(),
                agent_type: None,
                model: None,
                model_profile: None,
                prompt: None,
                color: None,
                plan_mode_required: None,
                joined_at: now_wall_ms(),
                tmux_pane_id: String::new(),
                cwd: ".".into(),
                worktree_path: None,
                backend_type: Some("in-process".into()),
                is_active: Some(true),
                mode: None,
                subscriptions: Vec::new(),
            }],
        };
        write_team_file("alpha", &team).unwrap();
        assert!(set_team_member_active("alpha", "alice", false).unwrap());
        assert!(!set_team_member_active("alpha", "alice", false).unwrap());
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert_eq!(read_back.members[0].is_active, Some(false));
    }

    #[test]
    fn hidden_pane_round_trip_updates_team_file() {
        let _home = TestConfigHome::new("team-hidden");
        let team = TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            hidden_pane_ids: Vec::new(),
            members: Vec::new(),
        };
        write_team_file("alpha", &team).unwrap();
        assert!(add_hidden_pane_id("alpha", "%12").unwrap());
        assert!(!add_hidden_pane_id("alpha", "%12").unwrap());
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert_eq!(read_back.hidden_pane_ids, vec!["%12"]);
        assert!(remove_hidden_pane_id("alpha", "%12").unwrap());
        assert!(!remove_hidden_pane_id("alpha", "%12").unwrap());
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert!(read_back.hidden_pane_ids.is_empty());
    }

    #[test]
    fn remove_member_by_pane_id_clears_hidden_entry() {
        let _home = TestConfigHome::new("team-pane-member");
        let team = TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            hidden_pane_ids: vec!["%7".into()],
            members: vec![TeamMember {
                agent_id: "alice@alpha".into(),
                name: "alice".into(),
                agent_type: None,
                model: None,
                model_profile: None,
                prompt: None,
                color: None,
                plan_mode_required: None,
                joined_at: now_wall_ms(),
                tmux_pane_id: "%7".into(),
                cwd: ".".into(),
                worktree_path: None,
                backend_type: Some("tmux".into()),
                is_active: Some(true),
                mode: Some("default".into()),
                subscriptions: Vec::new(),
            }],
        };
        write_team_file("alpha", &team).unwrap();
        assert!(remove_member_by_pane_id("alpha", "%7").unwrap());
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert!(read_back.members.is_empty());
        assert!(read_back.hidden_pane_ids.is_empty());
    }

    #[test]
    fn concurrent_member_appends_serialize_and_do_not_lose_updates() {
        let _home = TestConfigHome::new("team-concurrent");
        let team = TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: now_wall_ms(),
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            hidden_pane_ids: Vec::new(),
            members: Vec::new(),
        };
        write_team_file("alpha", &team).unwrap();

        let threads = 8usize;
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                std::thread::spawn(move || {
                    append_team_member(
                        "alpha",
                        TeamMember {
                            agent_id: format!("agent-{i}@alpha"),
                            name: format!("agent-{i}"),
                            agent_type: None,
                            model: None,
                            model_profile: None,
                            prompt: None,
                            color: None,
                            plan_mode_required: None,
                            joined_at: now_wall_ms(),
                            tmux_pane_id: String::new(),
                            cwd: ".".into(),
                            worktree_path: None,
                            backend_type: Some("in-process".into()),
                            is_active: Some(true),
                            mode: None,
                            subscriptions: Vec::new(),
                        },
                    )
                    .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        // Without the file lock, concurrent read-modify-writes lose updates;
        // with it every append is serialized and all members survive.
        let read_back = read_team_file("alpha").unwrap().unwrap();
        assert_eq!(read_back.members.len(), threads);

        // The atomic rename must never leave a staged temp file behind.
        let leftover_temps = fs::read_dir(team_dir("alpha"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".tmp"))
            })
            .count();
        assert_eq!(leftover_temps, 0, "atomic write must not leak temp files");
    }
}
