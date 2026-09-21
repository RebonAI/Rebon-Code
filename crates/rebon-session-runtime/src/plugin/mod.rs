//! Installing plugin packages, and turning what they declare into a session.
//!
//! The vocabulary — the manifest schema, its validation rules, the record of
//! what is installed — lives in `rebon-plugin-package`, below this crate and
//! below the plugin plane, because both have to read it. What is here is the
//! acting half: fetching a source, unpacking an archive, writing install state,
//! and materialising a package's contributions into a running session.
//!
//! The package layer is re-exported under the names it always had, so a module
//! here still says `super::manifest::PluginManifest`. What changed is which
//! crate owns the type, not what it is called.

pub mod builtin;
pub mod installer;
pub mod package;
pub mod runtime;
pub mod source;

pub use rebon_harness::rebon_plugin_package::{
    acp_agent_manifest, manifest, model_provider_manifest, security, store,
};

pub(crate) use installer::PluginVerification;
pub use installer::{format_plugin_result, PluginInstaller};
pub use rebon_harness::rebon_plugin_package::store::{
    InstalledPluginRecord, PluginScope, PluginStore,
};
pub use runtime::{resolve_runtime_contributions, PluginRuntimeOptions};
