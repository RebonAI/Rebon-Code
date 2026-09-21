//! Unix transport: a mode-0600 Unix domain socket in a private directory.

use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use super::io_error;
use crate::runtime::{ComputerUseError, ErrorCode};

pub(super) struct Listener {
    inner: UnixListener,
}

impl Listener {
    pub(super) fn bind(path: &Path) -> Result<Self, ComputerUseError> {
        remove_stale_socket(path)?;
        let inner = UnixListener::bind(path).map_err(io_error)?;
        set_socket_permissions(path)?;
        Ok(Self { inner })
    }

    pub(super) async fn accept(&mut self) -> Result<UnixStream, ComputerUseError> {
        let (stream, _) = self.inner.accept().await.map_err(io_error)?;
        Ok(stream)
    }
}

pub(super) async fn connect(path: &Path) -> Result<UnixStream, ComputerUseError> {
    UnixStream::connect(path).await.map_err(io_error)
}

pub(super) fn endpoint_ready(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn remove_stale_socket(path: &Path) -> Result<(), ComputerUseError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    {
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() {
            return Err(ComputerUseError::new(
                ErrorCode::InvalidRequest,
                "refusing to replace a non-socket IPC path",
                false,
            ));
        }
    }
    std::fs::remove_file(path).map_err(io_error)
}

fn set_socket_permissions(path: &Path) -> Result<(), ComputerUseError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io_error)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::super::test_support::MockBackend;
    use super::super::{serve, Client};
    use super::*;
    use crate::runtime::{Request, ServiceState};

    #[tokio::test]
    async fn socket_is_private_and_client_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sock");
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            serve(
                server_path,
                "secret",
                MockBackend {
                    calls: 0,
                    target_epoch: 0,
                    active: false,
                },
            )
            .await
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(endpoint_ready(&path));
        let response = Client::new(&path, "secret")
            .request(Request::Status)
            .await
            .unwrap();
        assert_eq!(response.status.state, ServiceState::WaitingForTarget);
        server.abort();
    }

    #[test]
    fn refuses_to_replace_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sock");
        std::fs::write(&path, b"do not delete").unwrap();
        assert_eq!(
            remove_stale_socket(&path).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"do not delete");
        assert!(!endpoint_ready(&path));
    }
}
