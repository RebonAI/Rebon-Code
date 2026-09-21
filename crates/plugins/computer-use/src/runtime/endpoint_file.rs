//! On-disk endpoint discovery for processes outside the app's env tree.
//!
//! The desktop app publishes the runtime endpoint through process environment
//! variables, which only descendants inherit. Background-job workers are
//! spawned by a long-lived supervisor daemon that frequently predates the app,
//! so they never see those variables. The app therefore also drops this
//! single-user record under the config home; consumers read the environment
//! first and fall back to the record. A stale record is harmless: the tool
//! still requires the endpoint to answer a liveness probe and the activation
//! marker to exist before it enables itself.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const ENDPOINT_FILE_NAME: &str = "computer-use-endpoint.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EndpointRecord {
    /// Unix socket path, or `\\.\pipe\...` name on Windows.
    pub socket_path: PathBuf,
    pub token: String,
    pub active_path: PathBuf,
}

/// `$REBON_CONFIG_DIR`, then `$REBON_CONFIG_HOME`, then `~/.rebon` — the same
/// resolution the rest of the process uses.
fn config_home() -> Option<PathBuf> {
    rebon_session::config_home_with_env(|name| std::env::var_os(name))
}

fn record_path() -> Option<PathBuf> {
    Some(config_home()?.join(ENDPOINT_FILE_NAME))
}

/// Persists the record for this user. Best-effort private: `0600` on Unix;
/// on Windows the config home already carries the profile's user-only ACL.
pub fn store_endpoint_record(record: &EndpointRecord) -> std::io::Result<()> {
    let Some(path) = record_path() else {
        return Err(std::io::Error::other("no config home for endpoint record"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    write_private(&path, &encoded)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

pub fn load_endpoint_record() -> Option<EndpointRecord> {
    let path = record_path()?;
    let bytes = std::fs::read(path).ok()?;
    let record: EndpointRecord = serde_json::from_slice(&bytes).ok()?;
    let usable = !record.token.trim().is_empty()
        && !record.socket_path.as_os_str().is_empty()
        && !record.active_path.as_os_str().is_empty();
    usable.then_some(record)
}

pub fn clear_endpoint_record() {
    if let Some(path) = record_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ConfigHomeGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl ConfigHomeGuard {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("REBON_CONFIG_HOME");
            std::env::set_var("REBON_CONFIG_HOME", path);
            Self { previous }
        }
    }

    impl Drop for ConfigHomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("REBON_CONFIG_HOME", value),
                None => std::env::remove_var("REBON_CONFIG_HOME"),
            }
        }
    }

    #[test]
    fn record_round_trips_and_clears() {
        let _lock = crate::env_test_lock();
        let home = tempfile::tempdir().unwrap();
        let _guard = ConfigHomeGuard::set(home.path());

        assert!(load_endpoint_record().is_none());
        let record = EndpointRecord {
            socket_path: PathBuf::from(r"\\.\pipe\rebon-cu-test"),
            token: "secret".into(),
            active_path: home.path().join("active"),
        };
        store_endpoint_record(&record).unwrap();
        assert_eq!(load_endpoint_record().unwrap(), record);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.path().join(ENDPOINT_FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        clear_endpoint_record();
        assert!(load_endpoint_record().is_none());
    }

    #[test]
    fn unusable_records_are_ignored() {
        let _lock = crate::env_test_lock();
        let home = tempfile::tempdir().unwrap();
        let _guard = ConfigHomeGuard::set(home.path());

        store_endpoint_record(&EndpointRecord {
            socket_path: PathBuf::new(),
            token: "  ".into(),
            active_path: PathBuf::from("x"),
        })
        .unwrap();
        assert!(load_endpoint_record().is_none());

        std::fs::write(
            home.path().join(ENDPOINT_FILE_NAME),
            b"{\"socketPath\":\"p\",\"unknown\":1}",
        )
        .unwrap();
        assert!(load_endpoint_record().is_none());
    }
}
