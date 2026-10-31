//! The sessions running on this machine right now.
//!
//! Discovery merges the two records that already exist — a hosted job's state
//! and owner descriptor, and a foreground terminal's `.live.json` heartbeat —
//! rather than introducing a registry that would be a third thing to keep in
//! sync. A session that is not in either record is not running, and this module
//! has nothing to say about it.
//!
//! Descriptors are read first; the session lock is probed only for the
//! candidates that could still be live. That ordering matters because the lock
//! probe takes an exclusive OS lock, and asking it about every job record ever
//! written would be both slower and noisier than reading the files it was
//! written beside.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::store::BackgroundStore;
use crate::{
    BackgroundJobEvent, BackgroundJobState, BackgroundJobStatus, ForegroundSessionStatus,
    OwnerHandle, BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS,
};

/// How stale a foreground heartbeat may be and still count as running.
///
/// The publisher rewrites it every second. Thirty seconds is well past a
/// scheduling hiccup, and a heartbeat that stopped for that long is a process
/// that is gone — the lock probe is what decides for certain.
pub const FOREGROUND_HEARTBEAT_FRESH_MS: u64 = 30_000;

/// What kind of endpoint a peer session has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerKind {
    /// A session hosted by a background worker, reachable through its owner
    /// descriptor (or through its job record when the worker is starting).
    Hosted,
    /// A session held by a foreground terminal. It reaches peers either through
    /// the loopback agent endpoint it advertises in `.live.json` or, when that
    /// listener could not start, through the mailbox its capability names.
    Foreground,
}

/// One session another agent may want to talk to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSession {
    pub session_id: String,
    /// The short, typeable name the session's owner claimed for it, read from
    /// its metadata sidecar. Absent until that owner has published a status
    /// since handles existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    pub kind: PeerKind,
    /// A turn is in flight or waiting on the user.
    pub busy: bool,
    /// Whether [`crate::SessionHostClient::send_agent_message`] would accept a
    /// message for this session right now.
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// The desktop terminal the session was started from, when its owner
    /// record carries one (`REBON_TERMINAL_ID`). A label for recognition, not
    /// an endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    /// What this session is doing right now — the owner's own summary while it
    /// is fresh, and the last tool it started once that summary has gone stale.
    /// `None` for a session that is running but has said nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
}

/// The peer list, read from the records the session host already keeps.
#[derive(Clone)]
pub struct PeerDirectory {
    store: BackgroundStore,
    projects_root: PathBuf,
}

impl PeerDirectory {
    pub fn new(store: BackgroundStore, projects_root: impl Into<PathBuf>) -> Self {
        Self {
            store,
            projects_root: projects_root.into(),
        }
    }

    /// Every session currently running on this machine.
    pub fn list(&self) -> Vec<PeerSession> {
        self.collect(None)
    }

    /// Every running session except `session_id` — the caller itself, when the
    /// caller is the one asking.
    pub fn list_excluding(&self, session_id: &str) -> Vec<PeerSession> {
        self.collect(Some(session_id))
    }

    fn collect(&self, excluded: Option<&str>) -> Vec<PeerSession> {
        let mut seen = HashSet::new();
        let mut peers = Vec::new();
        for job in self.hosted_jobs() {
            let Some(session_id) = job.session_id() else {
                continue;
            };
            if excluded == Some(session_id) {
                continue;
            }
            let Some(peer) = self.hosted_peer(&job) else {
                continue;
            };
            if seen.insert(peer.session_id.clone()) {
                peers.push(peer);
            }
        }
        for status in self.foreground_statuses() {
            if excluded == Some(status.session_id.as_str())
                || !seen.insert(status.session_id.clone())
            {
                continue;
            }
            let Some(peer) = self.foreground_peer(status) else {
                continue;
            };
            peers.push(peer);
        }
        peers.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        peers
    }

    fn hosted_jobs(&self) -> Vec<BackgroundJobState> {
        match self.store.list_jobs() {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::warn!(%error, "rebon: could not read the job records for peer discovery");
                Vec::new()
            }
        }
    }

    /// One hosted job, if it is running.
    ///
    /// The owner endpoint is the strongest evidence, and a descriptor is read
    /// for every job before anything else is asked. A job whose status still
    /// claims work is only then confirmed by the lock: a stale record with no
    /// holder is not a peer, and starting a worker to check would be a side
    /// effect discovery must never have.
    fn hosted_peer(&self, job: &BackgroundJobState) -> Option<PeerSession> {
        let session_id = job.session_id()?;
        let descriptor =
            rebon_session::read_session_owner(&self.projects_root, job.cwd(), session_id);
        let endpoint = descriptor
            .as_ref()
            .and_then(|descriptor| OwnerHandle::from_descriptor(session_id, descriptor));
        let endpoint_reachable = endpoint.as_ref().is_some_and(|owner| owner.ping());
        let candidate = matches!(
            job.status(),
            BackgroundJobStatus::Running
                | BackgroundJobStatus::NeedsInput
                | BackgroundJobStatus::Queued
        );
        let live = endpoint_reachable
            || (candidate
                && rebon_session::is_session_active(&self.projects_root, job.cwd(), session_id));
        if !live {
            return None;
        }
        // An endpoint that exists and does not answer is the one rung of the
        // message ladder that refuses; a descriptor with no endpoint at all
        // still has the durable queue behind it.
        let unreachable_endpoint = !endpoint_reachable && endpoint.is_some();
        let name = job.name().trim();
        let activity = self.hosted_activity(job);
        Some(PeerSession {
            session_id: session_id.to_string(),
            handle: rebon_session::load_session_handle(&self.projects_root, job.cwd(), session_id),
            name: (!name.is_empty()).then(|| name.to_string()),
            cwd: job.cwd().to_string(),
            kind: PeerKind::Hosted,
            busy: matches!(
                job.status(),
                BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
            ),
            reachable: !unreachable_endpoint,
            job_id: Some(job.job_id().to_string()),
            terminal_id: descriptor
                .as_ref()
                .and_then(|descriptor| descriptor.terminal_id.clone()),
            activity,
        })
    }

    /// What a hosted session is doing right now.
    ///
    /// The owner's own summary is the best answer and the only one written for
    /// this purpose, but the owner refreshes it on an interval, so a summary
    /// older than two intervals describes a turn that may already be over. Past
    /// that the event log is the fallback: the tool the session itself is
    /// running right now is what a caller wants to know, and it costs one
    /// bounded tail read of a file the owner is already writing.
    fn hosted_activity(&self, job: &BackgroundJobState) -> Option<String> {
        if !matches!(job.status(), BackgroundJobStatus::Running) {
            return None;
        }
        let fresh_window = BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS.saturating_mul(2);
        let now = crate::now_ms();
        if let Some(summary) = job.summary().map(str::trim).filter(|s| !s.is_empty()) {
            if job
                .outcome
                .summary_updated_at_ms
                .is_some_and(|updated| summary_is_fresh(updated, now, fresh_window))
            {
                return Some(summary.to_string());
            }
        }
        latest_running_tool_activity(&self.store, job.job_id())
    }

    /// Every `.live.json` under the projects root that is fresh.
    ///
    /// Freshness is the candidate test; the lock is the confirmation. A
    /// heartbeat left behind by a crashed terminal stops changing, so it is
    /// never mistaken for a running session even before the lock probe.
    fn foreground_statuses(&self) -> Vec<ForegroundSessionStatus> {
        let now = crate::now_ms();
        crate::legacy_foreground::read_foreground_statuses(&self.projects_root)
            .into_iter()
            .filter(|status| {
                !status.session_id.is_empty()
                    && now.saturating_sub(status.heartbeat_ms) <= FOREGROUND_HEARTBEAT_FRESH_MS
            })
            .collect()
    }

    fn foreground_peer(&self, status: ForegroundSessionStatus) -> Option<PeerSession> {
        if !rebon_session::is_session_active(&self.projects_root, &status.cwd, &status.session_id) {
            return None;
        }
        let reachable = foreground_agent_reachable(&status);
        Some(PeerSession {
            handle: rebon_session::load_session_handle(
                &self.projects_root,
                &status.cwd,
                &status.session_id,
            ),
            name: rebon_session::load_session_title(
                &self.projects_root,
                &status.cwd,
                &status.session_id,
            ),
            session_id: status.session_id,
            cwd: status.cwd,
            kind: PeerKind::Foreground,
            busy: status.busy,
            reachable,
            job_id: None,
            terminal_id: status.terminal_id,
            activity: status.activity.clone(),
        })
    }
}

/// Whether an owner summary written at `updated` still describes the turn that
/// is running at `now`.
///
/// The owner refreshes its summary once per interval, so a summary that has
/// missed two refreshes may describe a turn that already ended. The comparison
/// lives here, with the numbers passed in, because the boundary is exactly the
/// fact a test has to be able to state without racing a clock.
fn summary_is_fresh(updated_ms: u64, now_ms: u64, fresh_window_ms: u64) -> bool {
    now_ms.saturating_sub(updated_ms) <= fresh_window_ms
}

/// How much of a job's event log the fallback activity read looks at.
///
/// A turn can write thousands of lines; the tool the session is running right
/// now is what a peer card shows, and the last 64 KiB of the log holds it unless
/// the turn has written that much since its last tool call — in which case the
/// tool is genuinely not what the session is doing any more.
const ACTIVITY_EVENT_TAIL_BYTES: usize = 64 * 1024;

/// Lines of the event-log tail this fallback parses.
const ACTIVITY_EVENT_TAIL_LINES: usize = 256;

/// The tool the session itself is running right now, described the way its
/// summary describes one.
///
/// The session's own tool calls are recorded as `session_update` events — the
/// same stream `summarize_session_update` reads — and not as the
/// `task_live_batch` events, which belong to the sub-agents and teammates a turn
/// spawned. Reading those instead answered with whatever a *child* had done last,
/// which is not what the session is doing.
///
/// The tail is walked newest first and the answer is the most recent tool call
/// that has not finished. A completed or failed tool is history: past it the
/// session is thinking or answering, and `None` is the honest answer.
fn latest_running_tool_activity(store: &BackgroundStore, job_id: &str) -> Option<String> {
    let path = store.events_path(job_id);
    let lines = crate::store::read_tail_lines_bounded(
        &path,
        ACTIVITY_EVENT_TAIL_LINES,
        ACTIVITY_EVENT_TAIL_BYTES,
    )
    .ok()?;
    let mut settled = HashSet::new();
    for line in lines.iter().rev() {
        let Ok(event) = serde_json::from_str::<BackgroundJobEvent>(line) else {
            continue;
        };
        if event.kind != "session_update" {
            continue;
        }
        let Ok(update) = serde_json::from_value::<rebon_types::SessionUpdateParams>(event.data)
        else {
            continue;
        };
        let Some(state) = crate::summary::tool_call_state(&update.update) else {
            continue;
        };
        match state.status {
            // Newest first: a finished call means every earlier event about it
            // is older news, so it can no longer be the running tool.
            Some(rebon_types::ToolCallStatus::Completed | rebon_types::ToolCallStatus::Failed) => {
                settled.insert(state.tool_call_id.to_string());
            }
            Some(
                rebon_types::ToolCallStatus::Pending | rebon_types::ToolCallStatus::InProgress,
            ) => {
                // Newest first: a newer event can already have finished this
                // call, and a running-looking event that old is not running.
                if settled.contains(state.tool_call_id) {
                    continue;
                }
                return state
                    .description
                    .map(|activity| activity.trim().to_string())
                    .filter(|activity| !activity.is_empty());
            }
            // An update that says nothing about status: the call is running
            // unless a newer event already finished it.
            None => {
                if !settled.contains(state.tool_call_id) {
                    return state
                        .description
                        .map(|activity| activity.trim().to_string())
                        .filter(|activity| !activity.is_empty());
                }
            }
        }
    }
    None
}

/// Whether a foreground status names a way to take an agent message.
///
/// The capability is the strong answer: the mailbox is drained by a build that
/// understands the command, and no network probe is needed to say so. An
/// endpoint with no capability is asked directly, because a listener that
/// crashed while its process kept the lock must not be advertised as reachable.
fn foreground_agent_reachable(status: &ForegroundSessionStatus) -> bool {
    if status
        .capabilities
        .iter()
        .any(|capability| capability == crate::AGENT_MESSAGE_CAPABILITY)
    {
        return true;
    }
    let Some(endpoint) = status.agent_endpoint.as_ref() else {
        return false;
    };
    let Ok(token) = std::fs::read_to_string(&endpoint.token_path) else {
        return false;
    };
    let owner = OwnerHandle {
        session_id: status.session_id.clone(),
        job_id: None,
        pid: status.pid,
        port: endpoint.port,
        token: token.trim().to_string(),
        surface: rebon_session::SessionOwnerSurface::Local,
    };
    owner.ping()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BackgroundIpcRequest, BackgroundRuntimeFields, BackgroundTaskEventBatch,
        BackgroundTaskEventKind, ForegroundSessionStatus,
    };
    use tempfile::TempDir;

    fn runtime() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }

    fn store_and_projects() -> (TempDir, BackgroundStore, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path().join("state"));
        let projects_root = dir.path().join("projects");
        std::fs::create_dir_all(&projects_root).unwrap();
        (dir, store, projects_root)
    }

    fn host_job(store: &BackgroundStore, session_id: &str, cwd: &str) -> String {
        let job = store
            .create_job("hello".into(), PathBuf::from(cwd), runtime())
            .unwrap();
        store
            .update_state(job.job_id(), |state| {
                state.identity.session_id = Some(session_id.to_string());
                state.identity.name = "worker".into();
                state.process.status = BackgroundJobStatus::Running;
                Ok(())
            })
            .unwrap();
        job.job_id().to_string()
    }

    fn foreground_status(session_id: &str, cwd: &str) -> ForegroundSessionStatus {
        ForegroundSessionStatus {
            pid: std::process::id(),
            session_id: session_id.into(),
            cwd: cwd.into(),
            heartbeat_ms: crate::now_ms(),
            busy: false,
            pending_permission: None,
            ask_user_questions: None,
            last_command_id: None,
            last_command_at_ms: 0,
            last_command_error: None,
            recent_command_results: Vec::new(),
            terminal_id: None,
            agent_endpoint: None,
            capabilities: Vec::new(),
            activity: None,
        }
    }

    /// A hosted job that holds its lock is a peer, even with no endpoint: the
    /// durable queue is the delivery mechanism until one is published.
    #[test]
    fn a_running_hosted_job_is_a_reachable_peer() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-hosted", "F:/work/hosted");
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/hosted",
            "sess-hosted",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        let peer = &peers[0];
        assert_eq!(peer.session_id, "sess-hosted");
        assert_eq!(peer.kind, PeerKind::Hosted);
        assert_eq!(peer.name.as_deref(), Some("worker"));
        assert_eq!(peer.cwd, "F:/work/hosted");
        assert!(peer.busy);
        assert!(peer.reachable);
        assert_eq!(peer.job_id.as_deref(), Some(job_id.as_str()));
        assert_eq!(peer.terminal_id, None);
        // No sidecar, no handle: discovery only ever reads one.
        assert_eq!(peer.handle, None);
    }

    /// The handle is the owner's, read from the metadata sidecar its session
    /// writes; discovery claims nothing of its own.
    #[test]
    fn a_peer_reports_the_handle_its_owner_claimed() {
        let (_dir, store, projects_root) = store_and_projects();
        host_job(&store, "abcde-fghij-klmno-pqrst", "F:/work/hosted");
        rebon_session::ensure_session_handle(
            &projects_root,
            "F:/work/hosted",
            "abcde-fghij-klmno-pqrst",
        )
        .expect("the handle is claimed");
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/hosted",
            "abcde-fghij-klmno-pqrst",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].handle.as_deref(), Some("hosted-qrst"));
    }

    /// A job record with a status of Running but nobody holding the lock is
    /// leftover state, not a running session.
    #[test]
    fn a_hosted_job_without_its_lock_is_not_a_peer() {
        let (_dir, store, projects_root) = store_and_projects();
        host_job(&store, "sess-dead", "F:/work/dead");
        assert!(PeerDirectory::new(store, &projects_root).list().is_empty());
    }

    #[test]
    fn a_foreground_heartbeat_with_a_held_lock_is_a_peer_that_cannot_be_messaged() {
        let (_dir, store, projects_root) = store_and_projects();
        let status = foreground_status("sess-foreground", "F:/work/foreground");
        assert!(crate::write_foreground_status(&projects_root, &status));
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            &status.cwd,
            &status.session_id,
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].session_id, "sess-foreground");
        assert_eq!(peers[0].kind, PeerKind::Foreground);
        assert!(!peers[0].busy);
        assert!(!peers[0].reachable);
        assert_eq!(peers[0].job_id, None);
        assert_eq!(peers[0].terminal_id, None);
    }

    /// A new TUI drains its mailbox for agent messages even when its listener
    /// could not bind; the capability is what it promises.
    #[test]
    fn a_foreground_capability_makes_the_peer_reachable() {
        let (_dir, store, projects_root) = store_and_projects();
        let mut status = foreground_status("sess-capable", "F:/work/capable");
        status.capabilities = vec![crate::AGENT_MESSAGE_CAPABILITY.to_string()];
        assert!(crate::write_foreground_status(&projects_root, &status));
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            &status.cwd,
            &status.session_id,
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].reachable);
    }

    /// An endpoint with no capability is asked directly rather than assumed:
    /// a status that still names a dead port is stale data, not a live peer.
    #[test]
    fn a_foreground_endpoint_is_reachable_only_while_it_answers() {
        let (_dir, store, projects_root) = store_and_projects();
        let cwd = "F:/work/listening";
        let session_id = "sess-listening";
        let inbox = std::sync::Arc::new(crate::ForegroundAgentInbox::new());
        let endpoint =
            crate::ForegroundAgentEndpoint::start(&projects_root, cwd, session_id, inbox)
                .expect("the endpoint binds");
        let mut status = foreground_status(session_id, cwd);
        status.agent_endpoint = Some(endpoint.info());
        assert!(crate::write_foreground_status(&projects_root, &status));
        let _lock = rebon_session::try_acquire_session_active_lock(&projects_root, cwd, session_id)
            .expect("the lock probe works")
            .expect("the session lock is free");

        let peers = PeerDirectory::new(store.clone(), &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].reachable);

        let token_path = endpoint.info().token_path;
        drop(endpoint);
        let dead_port = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
            listener.local_addr().expect("the bound address").port()
        };
        let mut stale = status;
        stale.agent_endpoint = Some(crate::ForegroundAgentEndpointInfo {
            port: dead_port,
            token_path,
        });
        assert!(crate::write_foreground_status(&projects_root, &stale));
        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert!(!peers[0].reachable);
    }

    /// The terminal label is additive: each record reports the one it named.
    #[test]
    fn the_terminal_label_comes_from_the_record_that_holds_it() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-hosted", "F:/work/hosted");
        rebon_session::write_session_owner(
            &projects_root,
            "F:/work/hosted",
            "sess-hosted",
            &rebon_session::SessionOwnerDescriptor {
                version: rebon_session::SESSION_OWNER_VERSION,
                pid: 1,
                pid_identity: None,
                surface: rebon_session::SessionOwnerSurface::Worker,
                job_id: Some(job_id),
                ipc_port: None,
                ipc_token: None,
                terminal_id: Some("term-hosted".into()),
                started_at_ms: 0,
            },
        )
        .unwrap();
        let _hosted_lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/hosted",
            "sess-hosted",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let mut status = foreground_status("sess-foreground", "F:/work/foreground");
        status.terminal_id = Some("term-foreground".into());
        assert!(crate::write_foreground_status(&projects_root, &status));
        let _foreground_lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            &status.cwd,
            &status.session_id,
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        let hosted = peers
            .iter()
            .find(|peer| peer.session_id == "sess-hosted")
            .expect("the hosted session is listed");
        assert_eq!(hosted.terminal_id.as_deref(), Some("term-hosted"));
        let foreground = peers
            .iter()
            .find(|peer| peer.session_id == "sess-foreground")
            .expect("the foreground session is listed");
        assert_eq!(foreground.terminal_id.as_deref(), Some("term-foreground"));
    }

    #[test]
    fn a_stale_heartbeat_is_not_a_peer_even_while_the_lock_is_held() {
        let (_dir, store, projects_root) = store_and_projects();
        let mut status = foreground_status("sess-stale", "F:/work/stale");
        status.heartbeat_ms = crate::now_ms() - FOREGROUND_HEARTBEAT_FRESH_MS - 1;
        assert!(crate::write_foreground_status(&projects_root, &status));
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            &status.cwd,
            &status.session_id,
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        assert!(PeerDirectory::new(store, &projects_root).list().is_empty());
    }

    #[test]
    fn the_caller_can_leave_itself_out() {
        let (_dir, store, projects_root) = store_and_projects();
        host_job(&store, "sess-a", "F:/work/a");
        host_job(&store, "sess-b", "F:/work/b");
        let locks: Vec<_> = ["sess-a", "sess-b"]
            .into_iter()
            .map(|session| {
                rebon_session::try_acquire_session_active_lock(
                    &projects_root,
                    &format!("F:/work/{}", session.trim_start_matches("sess-")),
                    session,
                )
                .expect("the lock probe works")
                .expect("the session lock is free")
            })
            .collect();

        let peers = PeerDirectory::new(store, &projects_root).list_excluding("sess-a");
        assert_eq!(
            peers
                .iter()
                .map(|peer| peer.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["sess-b"]
        );
        drop(locks);
    }

    /// A fresh owner summary is the activity; the reader never has to parse the
    /// event log for the common case.
    #[test]
    fn a_fresh_summary_is_the_activity() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-fresh", "F:/work/fresh");
        store
            .update_state(&job_id, |state| {
                state.outcome.summary = Some("Bash cargo test".into());
                state.outcome.summary_updated_at_ms = Some(crate::now_ms());
                Ok(())
            })
            .unwrap();
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/fresh",
            "sess-fresh",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].activity.as_deref(), Some("Bash cargo test"));
    }

    /// A stale summary falls back to the tool the session itself is running.
    ///
    /// The session's own tool calls are `session_update` events; a
    /// `task_live_batch` is a child's, and reading that instead would answer with
    /// what a sub-agent did rather than what this session is doing.
    #[test]
    fn a_stale_summary_falls_back_to_the_tool_the_session_is_running() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-stale-summary", "F:/work/stale-summary");
        store
            .update_state(&job_id, |state| {
                state.outcome.summary = Some("an older turn".into());
                state.outcome.summary_updated_at_ms =
                    Some(crate::now_ms() - BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS * 3);
                Ok(())
            })
            .unwrap();
        // A child's finished tool, which is not what this session is doing.
        append_tool_start_batch(&store, &job_id, "Read crates/rebon-core/src/lib.rs");
        // The session's own running tool.
        append_tool_call(
            &store,
            &job_id,
            "tool-own",
            "Read crates/rebon-session/src/lib.rs",
            rebon_types::ToolKind::Read,
            Some(rebon_types::ToolCallStatus::InProgress),
        );
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/stale-summary",
            "sess-stale-summary",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0].activity.as_deref(),
            Some("reading: Read crates/rebon-session/src/lib.rs"),
            "the session's own running tool, not a child's"
        );
    }

    /// Once the session's tool finishes, nothing is running any more: it is
    /// thinking or answering, and an old tool is not what it is doing.
    #[test]
    fn a_finished_tool_is_no_longer_the_activity() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-finished", "F:/work/finished");
        append_tool_call(
            &store,
            &job_id,
            "tool-done",
            "Read crates/rebon-core/src/lib.rs",
            rebon_types::ToolKind::Read,
            Some(rebon_types::ToolCallStatus::InProgress),
        );
        append_tool_call(
            &store,
            &job_id,
            "tool-done",
            "Read crates/rebon-core/src/lib.rs",
            rebon_types::ToolKind::Read,
            Some(rebon_types::ToolCallStatus::Completed),
        );
        // Writing the calls also refreshed the summary; make it stale so the
        // question this test asks is answered by the log and not by the owner.
        store
            .update_state(&job_id, |state| {
                state.outcome.summary = None;
                state.outcome.summary_updated_at_ms = None;
                Ok(())
            })
            .unwrap();
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/finished",
            "sess-finished",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].activity, None);
    }

    /// A log past the tail window still answers from what the window holds, and
    /// a line the window cut in half is skipped rather than guessed at.
    #[test]
    fn a_long_event_log_answers_from_the_tail_window_it_reads() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-long-log", "F:/work/long-log");
        // Push the log past the 64 KiB window with a line long enough that the
        // window's first line is a fragment of it.
        for index in 0..600 {
            store
                .append_event(
                    &job_id,
                    "chatter",
                    serde_json::json!({ "index": index, "pad": "x".repeat(200) }),
                )
                .unwrap();
        }
        append_tool_call(
            &store,
            &job_id,
            "tool-recent",
            "crates/rebon-session-host/src/peers.rs",
            rebon_types::ToolKind::Edit,
            Some(rebon_types::ToolCallStatus::InProgress),
        );
        // The premise, asserted rather than assumed: the file is past the window
        // and the window begins in the middle of a line.
        let path = store.events_path(&job_id);
        let raw = std::fs::read(&path).unwrap();
        assert!(
            raw.len() > ACTIVITY_EVENT_TAIL_BYTES,
            "the log is past the {} byte window",
            ACTIVITY_EVENT_TAIL_BYTES
        );
        let window_start = raw.len() - ACTIVITY_EVENT_TAIL_BYTES;
        assert_ne!(
            raw[window_start - 1],
            b'\n',
            "the window starts mid-line, so its first line is a fragment"
        );
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/long-log",
            "sess-long-log",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0].activity.as_deref(),
            Some("editing: crates/rebon-session-host/src/peers.rs"),
            "the answer is at the end of the log, which is what the window holds"
        );
    }

    /// The freshness boundary is exactly two intervals: at it the summary still
    /// describes this turn, one millisecond past it does not.
    ///
    /// The comparison is asserted on numbers rather than a wall clock, because a
    /// test that measures "now" twice straddles the boundary it is testing.
    #[test]
    fn the_summary_freshness_boundary_is_two_intervals() {
        let window = BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS * 2;
        let updated = 1_000_000;
        assert!(summary_is_fresh(updated, updated + window, window));
        assert!(!summary_is_fresh(updated, updated + window + 1, window));
        // A record from the future still counts as fresh rather than wrapping.
        assert!(summary_is_fresh(updated, updated - 1, window));
    }

    /// End to end: a summary past two intervals is not the activity, and the
    /// fallback answers instead.
    #[test]
    fn a_summary_past_the_freshness_window_gives_way_to_the_fallback() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-stale-e2e", "F:/work/stale-e2e");
        store
            .update_state(&job_id, |state| {
                state.outcome.summary = Some("an older turn".into());
                state.outcome.summary_updated_at_ms =
                    Some(crate::now_ms() - BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS * 3);
                Ok(())
            })
            .unwrap();
        append_tool_call(
            &store,
            &job_id,
            "tool-own",
            "Read crates/rebon-core/src/lib.rs",
            rebon_types::ToolKind::Read,
            Some(rebon_types::ToolCallStatus::InProgress),
        );
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/stale-e2e",
            "sess-stale-e2e",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0].activity.as_deref(),
            Some("reading: Read crates/rebon-core/src/lib.rs"),
            "a summary past the window is history; the running tool answers"
        );
    }

    /// Nothing fresh and nothing started: the session is running but has no
    /// activity to report, which is `None` rather than an invented sentence.
    #[test]
    fn a_running_session_with_no_summary_and_no_tool_reports_no_activity() {
        let (_dir, store, projects_root) = store_and_projects();
        host_job(&store, "sess-quiet", "F:/work/quiet");
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/quiet",
            "sess-quiet",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].activity, None);
    }

    /// A session that is not running has nothing to report even when its record
    /// still carries the summary of its last turn.
    #[test]
    fn an_idle_session_reports_no_activity() {
        let (_dir, store, projects_root) = store_and_projects();
        let job_id = host_job(&store, "sess-needs-input", "F:/work/needs-input");
        store
            .update_state(&job_id, |state| {
                state.outcome.summary = Some("Bash cargo test".into());
                state.outcome.summary_updated_at_ms = Some(crate::now_ms());
                state.process.status = BackgroundJobStatus::NeedsInput;
                Ok(())
            })
            .unwrap();
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            "F:/work/needs-input",
            "sess-needs-input",
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].activity, None);
    }

    /// The foreground half publishes the activity itself, and discovery passes
    /// it through untouched.
    #[test]
    fn a_foreground_peer_reports_the_activity_its_heartbeat_carries() {
        let (_dir, store, projects_root) = store_and_projects();
        let mut status = foreground_status("sess-working", "F:/work/working");
        status.activity = Some("Edit src/main.rs".into());
        assert!(crate::write_foreground_status(&projects_root, &status));
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            &status.cwd,
            &status.session_id,
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].activity.as_deref(), Some("Edit src/main.rs"));
    }

    /// A `.live.json` written before the field existed still parses, and simply
    /// has no activity.
    #[test]
    fn a_heartbeat_from_before_the_field_existed_still_parses() {
        let (_dir, store, projects_root) = store_and_projects();
        let cwd = "F:/work/old-heartbeat";
        let session_id = "sess-old-heartbeat";
        let path =
            crate::legacy_foreground::foreground_status_path(&projects_root, cwd, session_id);
        std::fs::create_dir_all(path.parent().expect("the status file has a parent")).unwrap();
        let body = format!(
            r#"{{"pid":{},"sessionId":"{session_id}","cwd":"{cwd}","heartbeatMs":{},"busy":true,"lastCommandAtMs":0}}"#,
            std::process::id(),
            crate::now_ms()
        );
        std::fs::write(&path, body).unwrap();
        let _lock = rebon_session::try_acquire_session_active_lock(&projects_root, cwd, session_id)
            .expect("the lock probe works")
            .expect("the session lock is free");

        let peers = PeerDirectory::new(store, &projects_root).list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].session_id, session_id);
        assert_eq!(peers[0].activity, None);
    }

    /// One of the session's own tool-call updates, as the owner logs it.
    fn append_tool_call(
        store: &BackgroundStore,
        job_id: &str,
        tool_call_id: &str,
        title: &str,
        kind: rebon_types::ToolKind,
        status: Option<rebon_types::ToolCallStatus>,
    ) {
        let update = rebon_types::SessionUpdateParams {
            session_id: "sess-tool".into(),
            update: match status {
                Some(status) => rebon_types::SessionUpdate::ToolCall {
                    tool_call_id: tool_call_id.into(),
                    title: title.into(),
                    kind,
                    status,
                    content: None,
                    locations: None,
                    raw_input: None,
                    raw_output: None,
                },
                None => rebon_types::SessionUpdate::ToolCallUpdate {
                    tool_call_id: tool_call_id.into(),
                    status: None,
                    title: Some(title.into()),
                    content: None,
                    locations: None,
                    raw_output: None,
                },
            },
        };
        store
            .append_session_update_for_turn(job_id, 1, &update)
            .unwrap();
    }

    fn append_tool_start_batch(store: &BackgroundStore, job_id: &str, summary: &str) {
        let batch = BackgroundTaskEventBatch {
            schema: 1,
            stream_id: "stream-1".into(),
            from_cursor: 1,
            through_cursor: 1,
            cursor_was_stale: false,
            reset_tasks: Vec::new(),
            events: vec![crate::BackgroundTaskEvent {
                cursor: 1,
                task_id: "task-1".into(),
                timestamp_ms: crate::now_ms(),
                task: None,
                event: BackgroundTaskEventKind::ToolStart {
                    tool_use_id: "tool-1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({ "file_path": summary.trim_start_matches("Read ") }),
                },
            }],
        };
        store.append_task_event_batch(job_id, &batch).unwrap();
    }

    /// The request the peers feature exists for must at least spell itself.
    #[test]
    fn the_agent_message_spelling_points_at_the_session_extension() {
        assert_eq!(
            crate::session_ext::method_and_params(&BackgroundIpcRequest::AgentMessage {
                message_id: "m1".into(),
                from_session_id: "s1".into(),
                from_agent: "main".into(),
                from_cwd: None,
                from_handle: None,
                from_name: None,
                summary: None,
                text: "hi".into(),
                hops: 0,
            })
            .expect("agent messages have an ACP spelling")
            .0,
            crate::session_ext::method::AGENT_MESSAGE
        );
    }
}
