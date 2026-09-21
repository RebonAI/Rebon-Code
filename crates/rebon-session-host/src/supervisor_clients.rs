use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{now_ms, process_is_running, BackgroundStore, SUPERVISOR_CLIENT_TTL_MS};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundSupervisorClientLease {
    client_id: String,
    kind: String,
    pid: u32,
    updated_at_ms: u64,
}

impl BackgroundStore {
    pub fn supervisor_clients_dir(&self) -> PathBuf {
        self.daemon_dir().join("clients")
    }

    pub fn supervisor_client_path(&self, client_id: &str) -> PathBuf {
        self.supervisor_clients_dir()
            .join(format!("{client_id}.json"))
    }

    pub fn touch_supervisor_client(&self, kind: &str) -> anyhow::Result<String> {
        let kind = sanitize_supervisor_client_kind(kind);
        let client_id = format!("{kind}-{}", std::process::id());
        let lease = BackgroundSupervisorClientLease {
            client_id: client_id.clone(),
            kind,
            pid: std::process::id(),
            updated_at_ms: now_ms(),
        };
        fs::create_dir_all(self.supervisor_clients_dir())?;
        let payload = serde_json::to_string_pretty(&lease)?;
        rebon_session::write_file_atomically(
            &self.supervisor_client_path(&client_id),
            format!("{payload}\n").as_bytes(),
        )?;
        Ok(client_id)
    }
}

pub fn active_supervisor_clients(
    store: &BackgroundStore,
    now: u64,
) -> anyhow::Result<Vec<BackgroundSupervisorClientLease>> {
    let dir = store.supervisor_clients_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut active = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let lease = fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str::<BackgroundSupervisorClientLease>(&data).ok());
        let Some(lease) = lease else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let stale_by_time = now.saturating_sub(lease.updated_at_ms) > SUPERVISOR_CLIENT_TTL_MS;
        let stale_by_pid = process_is_running(lease.pid) == Some(false);
        if stale_by_time || stale_by_pid {
            let _ = fs::remove_file(&path);
        } else {
            active.push(lease);
        }
    }
    Ok(active)
}

fn sanitize_supervisor_client_kind(kind: &str) -> String {
    let mut out = String::new();
    for ch in kind.chars().take(40) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "client".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn active_supervisor_clients_keeps_fresh_and_prunes_stale_by_time() {
        let (_dir, store) = store();

        let client_id = store.touch_supervisor_client("agent view").unwrap();

        // Fresh lease (current pid, just written) is retained.
        let active = active_supervisor_clients(&store, now_ms()).unwrap();
        assert_eq!(active.len(), 1);
        assert!(store.supervisor_client_path(&client_id).exists());

        // Evaluated far in the future, the same lease is stale-by-time and
        // is pruned from disk.
        let pruned =
            active_supervisor_clients(&store, now_ms() + SUPERVISOR_CLIENT_TTL_MS + 10_000)
                .unwrap();
        assert!(pruned.is_empty());
        assert!(!store.supervisor_client_path(&client_id).exists());
    }
}
