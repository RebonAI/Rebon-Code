//! The session host's [`PeerMessenger`], for the tools that speak to it.
//!
//! Discovery and delivery both already exist in `rebon-session-host`:
//! [`PeerDirectory`] merges the job records and foreground heartbeats into the
//! list of sessions running here, and [`SessionHostClient::send_agent_message`]
//! follows the prompt ladder without ever launching a worker. This module is
//! only the adapter between those two and the trait `rebon-tool` names, so the
//! task tools can reach a peer without depending on the host crate.
//!
//! Both halves do blocking I/O — a directory scan with lock probes, and a
//! loopback call with a deadline — so each runs on the blocking pool rather
//! than on a runtime worker.

use std::path::PathBuf;

use async_trait::async_trait;
use rebon_session_host::{
    AgentMessageDelivery, AgentMessageEnvelope, BackgroundStore, HostCallError, PeerDirectory,
    PeerKind, PeerSession, SessionHostClient,
};
use rebon_tool::{PeerDelivery, PeerInfo, PeerMessage, PeerMessenger};

/// Reaches the other sessions running on this machine.
pub struct SessionPeerMessenger {
    directory: PeerDirectory,
    client: SessionHostClient,
}

impl SessionPeerMessenger {
    pub fn new(
        store: BackgroundStore,
        projects_root: impl Into<PathBuf>,
        worker_exe: impl Into<PathBuf>,
    ) -> Self {
        let projects_root = projects_root.into();
        Self {
            directory: PeerDirectory::new(store.clone(), projects_root.clone()),
            client: SessionHostClient::new(store, projects_root, worker_exe),
        }
    }
}

#[async_trait]
impl PeerMessenger for SessionPeerMessenger {
    async fn list_peers(&self, exclude_session_id: Option<&str>) -> Result<Vec<PeerInfo>, String> {
        let directory = self.directory.clone();
        let excluded = exclude_session_id.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let peers = match excluded.as_deref() {
                Some(session_id) => directory.list_excluding(session_id),
                None => directory.list(),
            };
            peers.into_iter().map(peer_info).collect()
        })
        .await
        .map_err(|error| format!("peer discovery did not finish: {error}"))
    }

    async fn send(
        &self,
        target_session_id: &str,
        message: PeerMessage,
    ) -> Result<PeerDelivery, String> {
        let envelope = envelope(self.client.projects_root(), message)?;
        let client = self.client.clone();
        let target = target_session_id.to_string();
        let result =
            tokio::task::spawn_blocking(move || client.send_agent_message(&target, envelope))
                .await
                .map_err(|error| {
                    format!("sending to session {target_session_id} did not finish: {error}")
                })?;
        result
            .map(|result| delivery(result.outcome))
            .map_err(|error| describe_send_error(target_session_id, error))
    }
}

/// A new id per send means a retry is a new message to the owner rather than a
/// duplicate it silently drops.
fn envelope(
    projects_root: &std::path::Path,
    message: PeerMessage,
) -> Result<AgentMessageEnvelope, String> {
    let message_id = rebon_types::secure_random_hex_token()
        .map_err(|error| format!("could not mint a message id: {error}"))?;
    // The sender's own sidecar is where its handle and title live. Read here
    // rather than in the tool: the projects root is this adapter's fact, and a
    // tool that guessed at it would read somebody else's config home.
    let (from_handle, from_name) = match message.from_cwd.as_deref() {
        Some(cwd) if !message.from_session_id.is_empty() => (
            message.from_handle.clone().or_else(|| {
                rebon_session::load_session_handle(projects_root, cwd, &message.from_session_id)
            }),
            message.from_name.clone().or_else(|| {
                rebon_session::load_session_title(projects_root, cwd, &message.from_session_id)
            }),
        ),
        _ => (message.from_handle, message.from_name),
    };
    Ok(AgentMessageEnvelope {
        message_id,
        from_session_id: message.from_session_id,
        from_agent: message.from_agent,
        from_cwd: message.from_cwd,
        from_handle,
        from_name,
        summary: message.summary,
        text: message.text,
        hops: message.hops,
    })
}

fn peer_info(peer: PeerSession) -> PeerInfo {
    PeerInfo {
        session_id: peer.session_id,
        handle: peer.handle,
        name: peer.name,
        cwd: peer.cwd,
        kind: match peer.kind {
            PeerKind::Hosted => "hosted".to_string(),
            PeerKind::Foreground => "foreground".to_string(),
        },
        busy: peer.busy,
        reachable: peer.reachable,
        terminal_id: peer.terminal_id,
        activity: peer.activity,
    }
}

fn delivery(outcome: AgentMessageDelivery) -> PeerDelivery {
    match outcome {
        AgentMessageDelivery::Injected => PeerDelivery::Injected,
        AgentMessageDelivery::StartedNewTurn => PeerDelivery::StartedTurn,
        AgentMessageDelivery::Queued => PeerDelivery::Queued,
    }
}

/// The owner's refusal, in words the model can act on.
///
/// `ForegroundTerminal` is the one the host already knows is permanent: a
/// terminal that advertises neither an agent endpoint nor the mailbox
/// capability publishes no owner endpoint either, so retrying changes nothing.
/// The rest keep the transport's own sentence, which names what was wrong.
fn describe_send_error(target_session_id: &str, error: HostCallError) -> String {
    match error {
        HostCallError::ForegroundTerminal => format!(
            "session {target_session_id} is held by a foreground terminal that advertises no agent-message endpoint or capability; it may need a newer Rebon"
        ),
        other => format!("could not send to session {target_session_id}: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::{write_foreground_status, ForegroundSessionStatus};

    fn foreground_status(session_id: &str, cwd: &str) -> ForegroundSessionStatus {
        ForegroundSessionStatus {
            pid: std::process::id(),
            session_id: session_id.into(),
            cwd: cwd.into(),
            heartbeat_ms: rebon_session_host::now_ms(),
            busy: true,
            pending_permission: None,
            ask_user_questions: None,
            last_command_id: None,
            last_command_at_ms: 0,
            last_command_error: None,
            recent_command_results: Vec::new(),
            terminal_id: Some("term-7".into()),
            agent_endpoint: None,
            capabilities: Vec::new(),
            activity: None,
        }
    }

    fn messenger(dir: &tempfile::TempDir) -> SessionPeerMessenger {
        SessionPeerMessenger::new(
            BackgroundStore::new(dir.path().join("state")),
            dir.path().join("projects"),
            dir.path().join("rebon"),
        )
    }

    /// The adapter is not a second discovery implementation: what the client
    /// lists must be what `PeerDirectory` found, kind and terminal id
    /// included.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_peers_projects_the_directory_and_leaves_the_caller_out() {
        let dir = tempfile::tempdir().unwrap();
        let messenger = messenger(&dir);
        let projects_root = dir.path().join("projects");
        std::fs::create_dir_all(&projects_root).unwrap();
        let status = foreground_status("sess-live", "F:/work/live");
        assert!(write_foreground_status(&projects_root, &status));
        let _lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            &status.cwd,
            &status.session_id,
        )
        .expect("the lock probe works")
        .expect("the session lock is free");

        let peers = messenger.list_peers(None).await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].session_id, "sess-live");
        assert_eq!(peers[0].kind, "foreground");
        assert!(peers[0].busy);
        assert!(!peers[0].reachable);
        assert_eq!(peers[0].terminal_id.as_deref(), Some("term-7"));

        assert!(messenger
            .list_peers(Some("sess-live"))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sending_to_a_session_nobody_hosts_reports_the_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let messenger = messenger(&dir);
        let error = messenger
            .send(
                "sess-missing",
                PeerMessage {
                    from_session_id: "sess-caller".into(),
                    from_agent: "main".into(),
                    from_cwd: None,
                    from_handle: None,
                    from_name: None,
                    summary: None,
                    text: "hello".into(),
                    hops: 0,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("sess-missing"), "{error}");
    }

    #[test]
    fn each_send_mints_a_fresh_message_id() {
        let message = || PeerMessage {
            from_session_id: "sess-caller".into(),
            from_agent: "main".into(),
            from_cwd: None,
            from_handle: None,
            from_name: None,
            summary: None,
            text: "hello".into(),
            hops: 0,
        };
        let root = std::path::Path::new("/nonexistent-projects");
        let first = envelope(root, message()).unwrap();
        let second = envelope(root, message()).unwrap();
        assert_ne!(first.message_id, second.message_id);
        assert_eq!(first.text, "hello");
        assert_eq!(first.from_session_id, "sess-caller");
    }
}
