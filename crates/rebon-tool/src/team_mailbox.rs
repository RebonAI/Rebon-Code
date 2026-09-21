use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::Notify;

use crate::tasks::FileLock;
use crate::team_files::{sanitize_team_name, teams_dir, write_atomic};

static MAILBOX_NOTIFIERS: OnceLock<Mutex<HashMap<(String, String), Weak<Notify>>>> =
    OnceLock::new();

fn mailbox_notifiers() -> &'static Mutex<HashMap<(String, String), Weak<Notify>>> {
    MAILBOX_NOTIFIERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn mailbox_notification(team_name: &str, agent_name: &str) -> Arc<Notify> {
    let key = (sanitize_team_name(team_name), agent_name.to_string());
    let mut notifiers = mailbox_notifiers()
        .lock()
        .expect("mailbox notifier registry poisoned");
    if let Some(notify) = notifiers.get(&key).and_then(Weak::upgrade) {
        return notify;
    }
    let notify = Arc::new(Notify::new());
    notifiers.insert(key, Arc::downgrade(&notify));
    notify
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMailboxMessage {
    pub from: String,
    pub text: String,
    pub timestamp: String,
    pub read: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

pub fn inbox_path(team_name: &str, agent_name: &str) -> PathBuf {
    teams_dir()
        .join(sanitize_team_name(team_name))
        .join("inboxes")
        .join(format!("{agent_name}.json"))
}

fn mailbox_lock_path(team_name: &str, agent_name: &str) -> PathBuf {
    inbox_path(team_name, agent_name).with_extension("json.lock")
}

pub fn read_mailbox(team_name: &str, agent_name: &str) -> Result<Vec<TeamMailboxMessage>> {
    let path = inbox_path(team_name, agent_name);
    match fs::read_to_string(&path) {
        Ok(content) => {
            let parsed = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse mailbox {}", path.display()))?;
            Ok(parsed)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn write_mailbox_message(
    team_name: &str,
    agent_name: &str,
    message: TeamMailboxMessage,
) -> Result<()> {
    let path = inbox_path(team_name, agent_name);
    {
        let _lock = FileLock::acquire(&mailbox_lock_path(team_name, agent_name))?;
        let mut messages = read_mailbox(team_name, agent_name)?;
        messages.push(message);
        let bytes = serde_json::to_vec_pretty(&messages)
            .with_context(|| format!("failed to serialize mailbox {}", path.display()))?;
        write_atomic(&path, &bytes)?;
    }
    mailbox_notification(team_name, agent_name).notify_one();
    Ok(())
}

pub fn drain_unread_mailbox(team_name: &str, agent_name: &str) -> Result<Vec<TeamMailboxMessage>> {
    drain_unread_mailbox_matching(team_name, agent_name, |_| true)
}

/// Drain unread messages that satisfy `predicate`, marking only the
/// matching messages as read. Non-matching unread messages stay
/// unread so a later handler can process them out-of-band.
///
/// Used by the engine's mid-turn teammate_mailbox producer to skip
/// structured protocol envelopes (plan_approval_response, shutdown_*)
/// without consuming them — the teammate's between-worker
/// `try_read_mailbox_prompt` path is the authoritative handler for
/// those and needs to see them still unread on the next iteration.
pub fn drain_unread_mailbox_matching<F>(
    team_name: &str,
    agent_name: &str,
    predicate: F,
) -> Result<Vec<TeamMailboxMessage>>
where
    F: Fn(&TeamMailboxMessage) -> bool,
{
    let path = inbox_path(team_name, agent_name);
    let _lock = FileLock::acquire(&mailbox_lock_path(team_name, agent_name))?;
    let mut messages = read_mailbox(team_name, agent_name)?;
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let mut drained = Vec::new();
    let mut changed = false;
    for message in &mut messages {
        if !message.read && predicate(message) {
            drained.push(message.clone());
            message.read = true;
            changed = true;
        }
    }
    if changed {
        let bytes = serde_json::to_vec_pretty(&messages)
            .with_context(|| format!("failed to serialize mailbox {}", path.display()))?;
        write_atomic(&path, &bytes)?;
    }
    Ok(drained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::test_support::TestConfigHome;
    use std::sync::{Arc, Barrier};

    fn message(index: usize) -> TeamMailboxMessage {
        TeamMailboxMessage {
            from: format!("agent-{index}"),
            text: format!("message-{index}-{}", "x".repeat(2_048)),
            timestamp: index.to_string(),
            read: false,
            color: None,
            summary: None,
        }
    }

    #[test]
    fn concurrent_mailbox_writers_preserve_every_message() {
        let _home = TestConfigHome::new("mailbox-concurrent-writes");
        let writers = 24;
        let barrier = Arc::new(Barrier::new(writers));
        let handles = (0..writers)
            .map(|index| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    write_mailbox_message("alpha", "team-lead", message(index)).unwrap();
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        let mut senders = read_mailbox("alpha", "team-lead")
            .unwrap()
            .into_iter()
            .map(|message| message.from)
            .collect::<Vec<_>>();
        senders.sort();
        let mut expected = (0..writers)
            .map(|index| format!("agent-{index}"))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(senders, expected);
    }

    #[test]
    fn concurrent_drain_and_append_do_not_lose_messages() {
        let _home = TestConfigHome::new("mailbox-drain-write");
        write_mailbox_message("alpha", "team-lead", message(0)).unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let drain = {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                drain_unread_mailbox("alpha", "team-lead").unwrap();
            })
        };
        let append = {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                write_mailbox_message("alpha", "team-lead", message(1)).unwrap();
            })
        };
        barrier.wait();
        drain.join().unwrap();
        append.join().unwrap();

        let mailbox = read_mailbox("alpha", "team-lead").unwrap();
        assert_eq!(mailbox.len(), 2);
        assert!(mailbox.iter().any(|message| message.from == "agent-0"));
        assert!(mailbox.iter().any(|message| message.from == "agent-1"));
        assert!(
            mailbox
                .iter()
                .find(|message| message.from == "agent-0")
                .unwrap()
                .read
        );
    }
}
