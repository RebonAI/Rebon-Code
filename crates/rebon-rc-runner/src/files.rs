//! What `rebon rc` keeps on disk, under `<config home>/rc/`.
//!
//! | File | Holds | Written by |
//! |---|---|---|
//! | `credentials.json` | server URL, account and device id, the device **refresh token** — owner-only | `rebon rc login` |
//! | `environment.json` | this machine's stable environment identity for that server | `rebon rc serve` |
//! | `ledger.json` | RC session ↔ local session, and which work items' prompts already ran | `rebon rc serve` ([`crate::ledger`]) |
//! | `serve.lock`, `serve.pid` | the one `rebon rc serve` allowed per machine, and its pid | `rebon rc serve` |
//!
//! No access token and no environment secret is ever written: both are
//! re-minted when `serve` starts.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use fs2::FileExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const CREDENTIALS_FILE: &str = "credentials.json";
const ENVIRONMENT_FILE: &str = "environment.json";
const LEDGER_FILE: &str = "ledger.json";
const LEDGER_LOCK: &str = "ledger.lock";
const SERVE_LOCK: &str = "serve.lock";
const SERVE_PID: &str = "serve.pid";

/// `<config home>/rc`.
#[derive(Debug, Clone)]
pub struct RcDir {
    root: PathBuf,
}

impl RcDir {
    pub fn new(config_home: &Path) -> Self {
        Self {
            root: config_home.join("rc"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.root.join(CREDENTIALS_FILE)
    }

    pub fn environment_path(&self) -> PathBuf {
        self.root.join(ENVIRONMENT_FILE)
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.root.join(LEDGER_FILE)
    }

    pub fn ledger_lock_path(&self) -> PathBuf {
        self.root.join(LEDGER_LOCK)
    }

    pub fn serve_lock_path(&self) -> PathBuf {
        self.root.join(SERVE_LOCK)
    }

    /// Create the directory, owner-only where the platform has modes.
    pub fn ensure(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to restrict {}", self.root.display()))?;
        }
        Ok(())
    }

    pub fn load_credentials(&self) -> anyhow::Result<Option<StoredCredentials>> {
        read_json(&self.credentials_path())
    }

    /// Replace the stored credentials. A login for a different server or
    /// account — or for an account that is not known, as with an adopted
    /// refresh token — also forgets the machine's identity: an environment
    /// belongs to one server and one account.
    pub fn save_credentials(&self, credentials: &StoredCredentials) -> anyhow::Result<()> {
        self.ensure()?;
        let previous = self.load_credentials().ok().flatten();
        let path = self.credentials_path();
        let payload = serde_json::to_vec_pretty(credentials)?;
        rebon_session::write_private_file_atomically(&path, &payload)
            .with_context(|| format!("failed to write {}", path.display()))?;
        let moved = credentials.account_id.is_empty()
            || previous.map_or(true, |previous| {
                previous.server != credentials.server
                    || previous.account_id != credentials.account_id
            });
        if moved {
            match std::fs::remove_file(self.environment_path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("failed to forget the old environment"),
            }
        }
        Ok(())
    }

    pub fn load_environment(&self) -> anyhow::Result<Option<EnvironmentIdentity>> {
        read_json(&self.environment_path())
    }

    pub fn save_environment(&self, identity: &EnvironmentIdentity) -> anyhow::Result<()> {
        self.ensure()?;
        let path = self.environment_path();
        let payload = serde_json::to_vec_pretty(identity)?;
        rebon_session::write_file_atomically(&path, &payload)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// Take the one-per-machine serve lock, or say who holds it.
    ///
    /// The pid goes in a file of its own: on Windows a locked range cannot
    /// be read through another handle, so a pid inside the lock file could
    /// not be reported by the process that was refused.
    pub fn lock_serve(&self) -> anyhow::Result<ServeLock> {
        self.ensure()?;
        let path = self.serve_lock_path();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            anyhow::bail!(
                "`rebon rc serve` is already running on this machine (pid {}); one machine is one \
                 environment, so a second one would fight it for the environment secret",
                self.read_serve_pid().unwrap_or_else(|| "unknown".into())
            );
        }
        let pid_path = self.serve_pid_path();
        let mut pid = File::create(&pid_path)
            .with_context(|| format!("failed to write {}", pid_path.display()))?;
        write!(pid, "{}", std::process::id())?;
        pid.flush()?;
        Ok(ServeLock { _file: file })
    }

    /// The pid of a running `rebon rc serve`, if one holds the lock.
    pub fn serve_holder(&self) -> Option<String> {
        let file = File::open(self.serve_lock_path()).ok()?;
        if FileExt::try_lock_shared(&file).is_ok() {
            // Nobody holds it exclusively.
            let _ = FileExt::unlock(&file);
            return None;
        }
        Some(self.read_serve_pid().unwrap_or_else(|| "unknown".into()))
    }

    fn serve_pid_path(&self) -> PathBuf {
        self.root.join(SERVE_PID)
    }

    fn read_serve_pid(&self) -> Option<String> {
        let mut holder = String::new();
        File::open(self.serve_pid_path())
            .ok()?
            .read_to_string(&mut holder)
            .ok()?;
        Some(holder.trim().to_string()).filter(|pid| !pid.is_empty())
    }
}

/// Held for as long as `rebon rc serve` runs.
pub struct ServeLock {
    _file: File,
}

/// A device credential bound to this machine.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCredentials {
    /// The RC API origin.
    pub server: String,
    pub account_id: String,
    pub device_id: String,
    pub refresh_token: String,
    pub created_at_ms: u64,
}

/// Redacted: the refresh token is the device's long-lived secret.
impl std::fmt::Debug for StoredCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredCredentials")
            .field("server", &self.server)
            .field("account_id", &self.account_id)
            .field("device_id", &self.device_id)
            .field("refresh_token", &"<redacted>")
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

/// This machine as one RC environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentIdentity {
    /// The server this identity was registered with.
    pub server: String,
    /// `BridgeConfig.environment_id`: the client's idempotency key, stable
    /// for the life of this file.
    pub client_environment_id: String,
    /// `BridgeConfig.bridge_id`.
    pub bridge_id: String,
    /// The environment id RC issued, reused on the next registration.
    pub environment_id: Option<String>,
}

impl EnvironmentIdentity {
    pub fn fresh(server: &str) -> Self {
        Self {
            server: server.to_string(),
            client_environment_id: crate::core::ids::random_id("envc-"),
            bridge_id: crate::core::ids::random_id("brg-"),
            environment_id: None,
        }
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(server: &str, account: &str) -> StoredCredentials {
        StoredCredentials {
            server: server.into(),
            account_id: account.into(),
            device_id: "dev_1".into(),
            refresh_token: "refresh-secret".into(),
            created_at_ms: 1,
        }
    }

    #[test]
    fn credentials_round_trip_and_never_print_the_token() {
        let home = tempfile::tempdir().unwrap();
        let dir = RcDir::new(home.path());
        assert!(dir.load_credentials().unwrap().is_none());
        let stored = credentials("https://rc.example.com", "acct_1");
        dir.save_credentials(&stored).unwrap();
        assert_eq!(dir.load_credentials().unwrap(), Some(stored.clone()));
        assert!(!format!("{stored:?}").contains("refresh-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.credentials_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
    }

    #[test]
    fn a_login_elsewhere_forgets_the_environment() {
        let home = tempfile::tempdir().unwrap();
        let dir = RcDir::new(home.path());
        dir.save_credentials(&credentials("https://a", "acct_1"))
            .unwrap();
        let identity = EnvironmentIdentity::fresh("https://a");
        dir.save_environment(&identity).unwrap();
        assert_eq!(dir.load_environment().unwrap(), Some(identity.clone()));

        // A new device on the same server and account keeps it.
        let mut again = credentials("https://a", "acct_1");
        again.device_id = "dev_2".into();
        dir.save_credentials(&again).unwrap();
        assert_eq!(dir.load_environment().unwrap(), Some(identity));

        // Another account, or another server, does not.
        dir.save_credentials(&credentials("https://a", "acct_2"))
            .unwrap();
        assert!(dir.load_environment().unwrap().is_none());
        dir.save_environment(&EnvironmentIdentity::fresh("https://a"))
            .unwrap();
        dir.save_credentials(&credentials("https://b", "acct_2"))
            .unwrap();
        assert!(dir.load_environment().unwrap().is_none());
        // An unknown account is never assumed to be the same one.
        dir.save_environment(&EnvironmentIdentity::fresh("https://b"))
            .unwrap();
        dir.save_credentials(&credentials("https://b", "")).unwrap();
        assert!(dir.load_environment().unwrap().is_none());
    }

    #[test]
    fn a_fresh_identity_is_random() {
        let first = EnvironmentIdentity::fresh("s");
        let second = EnvironmentIdentity::fresh("s");
        assert_ne!(first.client_environment_id, second.client_environment_id);
        assert_ne!(first.bridge_id, second.bridge_id);
        assert!(first.environment_id.is_none());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_blank() {
        let home = tempfile::tempdir().unwrap();
        let dir = RcDir::new(home.path());
        dir.ensure().unwrap();
        std::fs::write(dir.credentials_path(), b"{not json").unwrap();
        assert!(dir.load_credentials().is_err());
    }

    #[test]
    fn only_one_serve_holds_the_lock() {
        let home = tempfile::tempdir().unwrap();
        let dir = RcDir::new(home.path());
        assert!(dir.serve_holder().is_none());
        let held = dir.lock_serve().unwrap();
        let refused = dir.lock_serve().err().expect("a second serve is refused");
        assert!(
            refused
                .to_string()
                .contains(&std::process::id().to_string()),
            "{refused}"
        );
        assert_eq!(dir.serve_holder(), Some(std::process::id().to_string()));
        drop(held);
        assert!(dir.serve_holder().is_none());
        let _again = dir.lock_serve().unwrap();
    }
}
