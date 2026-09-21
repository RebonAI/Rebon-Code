use crate::runtime::{Action, ActionResponse, ComputerUseError, StatusResponse};

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod common;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod overlay;
#[cfg(target_os = "windows")]
mod windows;

/// Serialized native backend interface used by the Unix socket runtime.
pub trait Backend: Send + 'static {
    fn status(&mut self) -> Result<StatusResponse, ComputerUseError>;
    fn execute(&mut self, action: Action) -> Result<ActionResponse, ComputerUseError>;
    fn request_permissions(&mut self) -> Result<StatusResponse, ComputerUseError>;
    fn pause(&mut self) -> Result<(), ComputerUseError>;
    fn resume(&mut self) -> Result<(), ComputerUseError>;
    fn stop(&mut self) -> Result<(), ComputerUseError>;
}

#[cfg(target_os = "macos")]
pub use macos::MacBackend as NativeBackend;
#[cfg(target_os = "windows")]
pub use windows::WindowsBackend as NativeBackend;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug, Default)]
pub struct NativeBackend;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl NativeBackend {
    pub fn new() -> Result<Self, ComputerUseError> {
        Err(ComputerUseError::unsupported())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl Backend for NativeBackend {
    fn status(&mut self) -> Result<StatusResponse, ComputerUseError> {
        Err(ComputerUseError::unsupported())
    }

    fn execute(&mut self, _action: Action) -> Result<ActionResponse, ComputerUseError> {
        Err(ComputerUseError::unsupported())
    }

    fn request_permissions(&mut self) -> Result<StatusResponse, ComputerUseError> {
        Err(ComputerUseError::unsupported())
    }

    fn pause(&mut self) -> Result<(), ComputerUseError> {
        Err(ComputerUseError::unsupported())
    }

    fn resume(&mut self) -> Result<(), ComputerUseError> {
        Err(ComputerUseError::unsupported())
    }

    fn stop(&mut self) -> Result<(), ComputerUseError> {
        Err(ComputerUseError::unsupported())
    }
}
