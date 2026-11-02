//! Run-now requests: how the app asks the supervisor to fire a rule.
//!
//! The app does not launch the run itself: its runs would then be written by
//! two processes. It drops a request file; the supervisor takes it on its
//! next pass, deletes it, and fires the rule as a manual run.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Directory of pending requests in the config home.
pub const REQUESTS_DIR_NAME: &str = "automation-requests";

/// `<config_home>/automation-requests`.
pub fn requests_dir(config_home: &Path) -> PathBuf {
    config_home.join(REQUESTS_DIR_NAME)
}

/// One pending Run now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub rule_id: String,
    pub requested_at_ms: u64,
}

/// Ask for `rule_id` to run now.
pub fn request_run(dir: &Path, rule_id: &str, now_ms: u64) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let safe_id: String = rule_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{now_ms:016x}-{safe_id}.json"));
    let body = serde_json::to_vec(&RunRequest {
        rule_id: rule_id.to_string(),
        requested_at_ms: now_ms,
    })
    .context("serialize a run request")?;
    rebon_session::write_file_atomically(&path, &body)
        .with_context(|| format!("write {}", path.display()))
}

/// Whether any request is waiting, without taking it.
pub fn has_run_requests(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
    })
}

/// Take every pending request, oldest first, removing each so it runs once.
/// A file that does not parse is removed and skipped.
pub fn take_run_requests(dir: &Path) -> Vec<RunRequest> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let request = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RunRequest>(&bytes).ok());
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), %error, "could not remove a run request");
                return None;
            }
            request
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_taken_once_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let dir = requests_dir(dir.path());
        assert!(take_run_requests(&dir).is_empty(), "no directory yet");
        assert!(!has_run_requests(&dir));
        request_run(&dir, "auto-b", 20).unwrap();
        assert!(has_run_requests(&dir));
        request_run(&dir, "auto-a", 10).unwrap();
        assert_eq!(
            take_run_requests(&dir),
            [
                RunRequest {
                    rule_id: "auto-a".into(),
                    requested_at_ms: 10
                },
                RunRequest {
                    rule_id: "auto-b".into(),
                    requested_at_ms: 20
                },
            ]
        );
        assert!(take_run_requests(&dir).is_empty());
        assert!(!has_run_requests(&dir));
    }

    #[test]
    fn a_rule_id_cannot_steer_the_file_name_and_still_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let dir = requests_dir(dir.path());
        request_run(&dir, "../../evil", 1).unwrap();
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(
            !names[0].contains('/') && !names[0].contains(".."),
            "{names:?}"
        );
        assert_eq!(take_run_requests(&dir)[0].rule_id, "../../evil");
    }

    #[test]
    fn an_unreadable_request_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let dir = requests_dir(dir.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0-x.json"), b"{nope").unwrap();
        std::fs::write(dir.join("ignored.txt"), b"").unwrap();
        assert!(take_run_requests(&dir).is_empty());
        assert!(!dir.join("0-x.json").exists());
        assert!(dir.join("ignored.txt").exists(), "only requests are taken");
    }
}
