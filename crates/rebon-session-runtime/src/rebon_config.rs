//! Facade over the `rebon-config` library crate.
//!
//! The provider/config resolution, OAuth refresh, settings + provider CRUD, and
//! sub-agent/background-permission helpers moved to the `rebon-config` lib crate
//! so the GPUI app + in-process harness can resolve providers without the binary.
//! Re-exported here so every `crate::rebon_config::*` call site across rebon-cli
//! compiles unchanged.
//!
//! `RuntimeOverride` stays binary-side (it references `crate::ui_config::UiMode`,
//! a binary-internal type the library cannot depend on).

pub use ::rebon_config::*;

#[path = "rebon_config_cli_overrides.rs"]
mod cli_overrides;
pub use cli_overrides::RuntimeOverride;
