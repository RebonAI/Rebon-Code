//! Concrete [`HookExecutor`](crate::executor::HookExecutor) backends.
//!
//! These are the two transports that need no host service: a real
//! subprocess and a real HTTP client. They are the reason `tokio` and
//! `reqwest` are non-optional dependencies of this crate.
//!
//! | Module | Hook variant it serves | Transport |
//! |--------|-----------------------|-----------|
//! | [`command`] | `HookCommand::Command` | `tokio::process::Command` |
//! | [`http`] | `HookCommand::Http` | `reqwest::Client` |
//!
//! Prompt (`HookCommand::Prompt`) and Agent (`HookCommand::Agent`)
//! backends are not here: they need a model client and a worker spawner,
//! which would drag foreign dependencies into a crate that deliberately
//! has none. Callers register them with
//! [`DispatchExecutor`](crate::executor::DispatchExecutor) through
//! `with_prompt` / `with_agent`.

pub mod command;
pub mod http;

pub use command::CommandExecutor;
pub use http::HttpExecutor;
