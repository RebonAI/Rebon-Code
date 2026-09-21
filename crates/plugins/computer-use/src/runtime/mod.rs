//! Native, window-scoped Computer Use runtime.
//!
//! The wire protocol deliberately has no request field containing a native
//! window identifier. A caller can only acquire a target by giving a screen
//! point to [`Action::Observe`]; subsequent coordinates are target-local.
//!
//! Hosted out of process from the sessions that drive it: `rebon computer-use
//! serve` and the desktop app both run this runtime and publish an endpoint,
//! and [`crate::computer_use::ComputerUseTool`] is a client of it like any
//! other. That is why turning the plugin off takes the tool away and leaves a
//! locked desktop session running.

mod backend;
mod endpoint_file;
#[cfg(any(unix, windows))]
mod ipc;
pub mod main_thread;
mod protocol;
mod session;

pub use backend::{Backend, NativeBackend};
pub use endpoint_file::{
    clear_endpoint_record, load_endpoint_record, store_endpoint_record, EndpointRecord,
};
#[cfg(any(unix, windows))]
pub use ipc::{endpoint_ready, serve, Client, Runtime};
pub use protocol::*;
pub use session::{endpoint_looks_live, random_token, SessionEndpoint};

/// Environment variable containing the IPC endpoint: a Unix domain socket
/// path, or a `\\.\pipe\...` named-pipe name on Windows.
pub const SOCKET_PATH_ENV: &str = "REBON_COMPUTER_USE_SOCKET";
/// Environment variable containing the per-runtime authentication token.
pub const AUTH_TOKEN_ENV: &str = "REBON_COMPUTER_USE_TOKEN";
/// Environment variable containing the private activation-marker path.
pub const ACTIVE_PATH_ENV: &str = "REBON_COMPUTER_USE_ACTIVE_PATH";
