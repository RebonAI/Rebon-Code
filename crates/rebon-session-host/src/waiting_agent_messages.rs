//! Agent messages a foreground session accepted and has not yet carried into a
//! turn, kept on disk so quitting the terminal does not lose them.
//!
//! A hosted session keeps its waiting messages in its job record. A plain
//! foreground terminal holds them in memory — in its session inbox, or queued
//! for the running turn — and its senders were told they had been accepted.
//! The terminal writes what is still waiting to
//! `<project_dir>/<session_id>.agent-waiting.json` whenever that set changes,
//! removes the file once nothing waits, and reads it back the next time it
//! holds the session's lock.
//!
//! Delivery from this file is at least once: a message that entered a turn just
//! before the process ended can still be listed, and comes back once more.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename suffix of the waiting-messages file.
pub const WAITING_AGENT_MESSAGES_SUFFIX: &str = ".agent-waiting.json";

/// One message still waiting for a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitingAgentMessage {
    /// The envelope's idempotency key.
    pub message_id: String,
    pub from_session_id: String,
    #[serde(default)]
    pub hops: u8,
    /// The `<teammate-message>` frame, exactly as the turn would read it.
    pub text: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WaitingFile {
    #[serde(default)]
    messages: Vec<WaitingAgentMessage>,
}

/// `<project_dir>/<session_id>.agent-waiting.json`.
pub fn waiting_agent_messages_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut p = rebon_session::project_dir_path(projects_root, cwd);
    p.push(format!("{session_id}{WAITING_AGENT_MESSAGES_SUFFIX}"));
    p
}

/// Record `messages` as what is waiting, replacing what the file held. An empty
/// list removes the file.
pub fn save_waiting_agent_messages(
    path: &Path,
    messages: &[WaitingAgentMessage],
) -> std::io::Result<()> {
    if messages.is_empty() {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(&WaitingFile {
        messages: messages.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    // Private: the bodies are other sessions' words.
    rebon_session::write_private_file_atomically(path, &body)
}

/// What the file says is waiting. A missing or unreadable file is nothing
/// waiting: the file only ever adds messages back, so reading too little loses
/// what a crash would have lost anyway, and reading garbage must not start
/// turns.
pub fn load_waiting_agent_messages(path: &Path) -> Vec<WaitingAgentMessage> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<WaitingFile>(&bytes) {
        Ok(file) => file.messages,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "rebon: ignoring an unreadable waiting agent messages file");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str) -> WaitingAgentMessage {
        WaitingAgentMessage {
            message_id: id.into(),
            from_session_id: "sess-peer".into(),
            hops: 2,
            text: format!("<teammate-message>{id}</teammate-message>"),
        }
    }

    #[test]
    fn what_is_saved_is_what_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = waiting_agent_messages_path(dir.path(), "/work/repo", "sess-1");
        let messages = vec![message("m-1"), message("m-2")];

        save_waiting_agent_messages(&path, &messages).unwrap();

        assert_eq!(load_waiting_agent_messages(&path), messages);
    }

    #[test]
    fn saving_nothing_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = waiting_agent_messages_path(dir.path(), "/work/repo", "sess-1");
        save_waiting_agent_messages(&path, &[message("m-1")]).unwrap();

        save_waiting_agent_messages(&path, &[]).unwrap();
        save_waiting_agent_messages(&path, &[]).unwrap();

        assert!(!path.exists());
        assert!(load_waiting_agent_messages(&path).is_empty());
    }

    #[test]
    fn an_unreadable_file_is_nothing_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess-1.agent-waiting.json");
        std::fs::write(&path, b"{not json").unwrap();

        assert!(load_waiting_agent_messages(&path).is_empty());
    }
}
