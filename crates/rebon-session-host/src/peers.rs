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
use crate::{BackgroundJobState, BackgroundJobStatus, ForegroundSessionStatus, OwnerHandle};

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
    /// A session held by a foreground terminal. It publishes a heartbeat and
    /// no endpoint, so it cannot be sent an agent message yet.
    Foreground,
}

/// One session another agent may want to talk to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSession {
    pub session_id: String,
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
        Some(PeerSession {
            session_id: session_id.to_string(),
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
        })
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
        Some(PeerSession {
            name: rebon_session::load_session_title(
                &self.projects_root,
                &status.cwd,
                &status.session_id,
            ),
            session_id: status.session_id,
            cwd: status.cwd,
            kind: PeerKind::Foreground,
            busy: status.busy,
            reachable: false,
            job_id: None,
            terminal_id: status.terminal_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackgroundIpcRequest, BackgroundRuntimeFields, ForegroundSessionStatus};
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

    /// The request the peers feature exists for must at least spell itself.
    #[test]
    fn the_agent_message_spelling_points_at_the_session_extension() {
        assert_eq!(
            crate::session_ext::method_and_params(&BackgroundIpcRequest::AgentMessage {
                message_id: "m1".into(),
                from_session_id: "s1".into(),
                from_agent: "main".into(),
                from_cwd: None,
                summary: None,
                text: "hi".into(),
            })
            .expect("agent messages have an ACP spelling")
            .0,
            crate::session_ext::method::AGENT_MESSAGE
        );
    }
}
