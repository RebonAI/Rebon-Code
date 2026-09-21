//! Windows transport: a local-only named pipe.
//!
//! The endpoint "path" carries the pipe name (`\\.\pipe\...`) in the same
//! environment variable that holds the socket path on Unix. Remote clients are
//! rejected at the transport layer and the very first instance is claimed with
//! `first_pipe_instance`, so an already-squatted name fails loudly instead of
//! silently splitting traffic between two owners.

use std::path::Path;
use std::time::Duration;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

use super::io_error;
use crate::runtime::{ComputerUseError, ErrorCode};

const PIPE_PREFIX: &str = r"\\.\pipe\";
const CONNECT_ATTEMPTS: usize = 50;
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(20);

pub(super) struct Listener {
    name: String,
    next: Option<NamedPipeServer>,
}

impl Listener {
    pub(super) fn bind(path: &Path) -> Result<Self, ComputerUseError> {
        let name = pipe_name(path)?;
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)
            .map_err(io_error)?;
        Ok(Self {
            name,
            next: Some(first),
        })
    }

    pub(super) async fn accept(&mut self) -> Result<NamedPipeServer, ComputerUseError> {
        let server = match self.next.take() {
            Some(server) => server,
            None => self.new_instance().map_err(io_error)?,
        };
        server.connect().await.map_err(io_error)?;
        // Bind the replacement instance before handing the connected one out,
        // so the pipe name never disappears between connections.
        self.next = self.new_instance().ok();
        Ok(server)
    }

    fn new_instance(&self) -> std::io::Result<NamedPipeServer> {
        ServerOptions::new()
            .reject_remote_clients(true)
            .create(&self.name)
    }
}

pub(super) async fn connect(path: &Path) -> Result<NamedPipeClient, ComputerUseError> {
    let name = pipe_name(path)?;
    for _ in 0..CONNECT_ATTEMPTS {
        match ClientOptions::new().open(&name) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(BUSY_RETRY_DELAY).await;
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(ComputerUseError::new(
        ErrorCode::Internal,
        "Computer Use pipe has no free instances",
        true,
    ))
}

pub(super) fn endpoint_ready(path: &Path) -> bool {
    let Ok(name) = pipe_name(path) else {
        return false;
    };
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // Nonzero: a listening instance is available right now. ERROR_SEM_TIMEOUT:
    // the pipe exists but every instance is busy — the server is alive, so the
    // endpoint still counts as ready. Neither probe consumes an instance.
    let ready = unsafe { WaitNamedPipeW(wide.as_ptr(), 1) };
    ready != 0 || unsafe { GetLastError() } == ERROR_SEM_TIMEOUT
}

fn pipe_name(path: &Path) -> Result<String, ComputerUseError> {
    let invalid = || {
        ComputerUseError::new(
            ErrorCode::InvalidRequest,
            format!("Computer Use endpoint must be a local named pipe ({PIPE_PREFIX}...)"),
            false,
        )
    };
    let name = path.to_str().ok_or_else(invalid)?;
    let leaf = name.strip_prefix(PIPE_PREFIX).ok_or_else(invalid)?;
    if leaf.is_empty() || leaf.contains(['\\', '/']) {
        return Err(invalid());
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn pipe_names_are_validated() {
        assert!(pipe_name(&PathBuf::from(r"\\.\pipe\rebon-cu-1")).is_ok());
        for bad in [
            r"C:\temp\rcu\s",
            r"\\.\pipe\",
            r"\\.\pipe\a\b",
            r"\\remote\pipe\x",
            "/tmp/socket",
        ] {
            assert!(pipe_name(&PathBuf::from(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn endpoint_ready_is_false_for_missing_or_invalid_endpoints() {
        assert!(!endpoint_ready(&PathBuf::from(r"C:\not\a\pipe")));
        assert!(!endpoint_ready(&PathBuf::from(
            r"\\.\pipe\rebon-cu-definitely-not-served"
        )));
    }

    #[tokio::test]
    async fn binding_a_squatted_name_fails_instead_of_sharing_it() {
        let name = PathBuf::from(format!(
            r"\\.\pipe\rebon-cu-squat-test-{}",
            std::process::id()
        ));
        let _first = Listener::bind(&name).unwrap();
        assert!(Listener::bind(&name).is_err());
    }
}
