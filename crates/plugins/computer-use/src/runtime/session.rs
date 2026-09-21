//! Standing the runtime up: where it listens, who may talk to it, and how
//! anyone else finds it.
//!
//! The parts of standing a runtime up that are not about drawing live here, so
//! the desktop app is not the only thing that can start one: the backend runs
//! off the UI thread, and a target is acquired by naming a screen point rather
//! than by anyone clicking. Both `rebon computer-use serve` and the app use
//! them.
//!
//! # What a runtime publishes
//!
//! Three values, and every consumer needs all three: the endpoint to connect
//! to, the token that authenticates the connection, and the path of the
//! activation marker. The marker is not published *by* this module — the
//! backend creates it when a target is locked and removes it when one is not —
//! but its location is decided here, because it belongs to the same private
//! directory as the endpoint.
//!
//! They travel two ways. Environment variables reach descendants of whoever
//! started the runtime; the on-disk record reaches everyone else, which is what
//! a background-job worker spawned by a daemon that predates the runtime has to
//! use. A stale record is inert: the tool probes the endpoint for life and the
//! marker for a locked target before it enables itself.

use std::path::{Path, PathBuf};

use crate::runtime::{
    store_endpoint_record, EndpointRecord, ACTIVE_PATH_ENV, AUTH_TOKEN_ENV, SOCKET_PATH_ENV,
};

/// One runtime's private endpoint, before anything is listening on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEndpoint {
    /// The per-run private directory. Removed when the runtime stops.
    pub directory: PathBuf,
    /// Unix socket path, or `\\.\pipe\...` name on Windows.
    pub socket_path: PathBuf,
    /// Where the backend writes the activation marker.
    pub active_path: PathBuf,
    /// The per-runtime authentication token.
    pub token: String,
}

impl SessionEndpoint {
    /// Allocates a private directory and the endpoint inside it.
    ///
    /// The directory is created non-recursively so a name that already exists
    /// is a collision to retry rather than a directory to reuse — reusing one
    /// would mean listening inside a directory somebody else owns.
    pub fn allocate() -> Result<Self, String> {
        for _ in 0..16 {
            let suffix = hex(&random_bytes::<6>()?);
            let directory =
                std::env::temp_dir().join(format!("rcu-{}-{suffix}", std::process::id()));
            let builder = {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(false);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt as _;
                    builder.mode(0o700);
                }
                builder
            };
            match builder.create(&directory) {
                Ok(()) => {
                    #[cfg(windows)]
                    let socket_path = PathBuf::from(format!(
                        r"\\.\pipe\rebon-cu-{}-{suffix}",
                        std::process::id()
                    ));
                    #[cfg(not(windows))]
                    let socket_path = directory.join("s");
                    return Ok(Self {
                        active_path: directory.join("active"),
                        socket_path,
                        directory,
                        token: random_token()?,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to create private Computer Use endpoint: {error}"
                    ))
                }
            }
        }
        Err("failed to allocate private Computer Use endpoint".into())
    }

    /// The record consumers outside this process tree read.
    pub fn record(&self) -> EndpointRecord {
        EndpointRecord {
            socket_path: self.socket_path.clone(),
            token: self.token.clone(),
            active_path: self.active_path.clone(),
        }
    }

    /// Publishes this endpoint to descendants, through the environment.
    ///
    /// # Safety
    ///
    /// Mutating the process environment is unsound while another thread reads
    /// it. Call this once, during start-up, before any thread that might read
    /// the environment exists — which is what both callers do.
    pub unsafe fn publish_environment(&self) {
        unsafe {
            std::env::set_var(SOCKET_PATH_ENV, &self.socket_path);
            std::env::set_var(AUTH_TOKEN_ENV, &self.token);
            std::env::set_var(ACTIVE_PATH_ENV, &self.active_path);
        }
    }

    /// Publishes this endpoint to everyone else, through the config home.
    ///
    /// Best-effort by design: a runtime that cannot write the record still
    /// serves its own descendants, and failing to start over it would trade a
    /// working feature for a missing one.
    pub fn publish_record(&self) -> std::io::Result<()> {
        store_endpoint_record(&self.record())
    }

    /// Removes the private directory. The record is cleared separately, since
    /// its owner is the process that published it.
    pub fn discard(&self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// A fresh authentication token: 32 bytes of OS entropy, hex-encoded.
pub fn random_token() -> Result<String, String> {
    rebon_types::secure_random_hex_token()
        .map_err(|error| format!("no OS entropy for the Computer Use endpoint: {error}"))
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("no OS entropy for the Computer Use endpoint: {error}"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether a path names something that could be a live endpoint.
///
/// Cheap and advisory — a caller still has to connect. It exists so a second
/// runtime can notice the first one instead of quietly taking over the record.
pub fn endpoint_looks_live(socket_path: &Path) -> bool {
    #[cfg(windows)]
    {
        crate::runtime::endpoint_ready(socket_path)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        std::fs::symlink_metadata(socket_path)
            .is_ok_and(|metadata| metadata.file_type().is_socket())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = socket_path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_thirty_two_bytes_of_lowercase_hex_and_never_repeats() {
        let first = random_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        assert_ne!(first, random_token().unwrap());
    }

    #[test]
    fn an_allocated_endpoint_is_private_and_self_describing() {
        let endpoint = SessionEndpoint::allocate().unwrap();
        assert!(endpoint.directory.is_dir());
        assert_eq!(
            endpoint.active_path.parent(),
            Some(endpoint.directory.as_path())
        );
        assert!(!endpoint.token.is_empty());

        let record = endpoint.record();
        assert_eq!(record.socket_path, endpoint.socket_path);
        assert_eq!(record.token, endpoint.token);
        assert_eq!(record.active_path, endpoint.active_path);

        // Nothing is listening yet, and the marker is the backend's to write.
        assert!(!endpoint.active_path.exists());

        endpoint.discard();
        assert!(!endpoint.directory.exists());
    }

    #[test]
    fn two_runtimes_never_share_a_directory() {
        let first = SessionEndpoint::allocate().unwrap();
        let second = SessionEndpoint::allocate().unwrap();
        assert_ne!(first.directory, second.directory);
        assert_ne!(first.socket_path, second.socket_path);
        assert_ne!(first.token, second.token);
        first.discard();
        second.discard();
    }

    #[cfg(unix)]
    #[test]
    fn the_private_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let endpoint = SessionEndpoint::allocate().unwrap();
        let mode = std::fs::metadata(&endpoint.directory)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        endpoint.discard();
    }
}
