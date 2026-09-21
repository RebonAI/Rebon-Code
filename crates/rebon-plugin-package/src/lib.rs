//! The plugin package layer: what a package declares, and what is installed.
//!
//! A plugin package is a directory (or a tarball of one) with a
//! `rebon-plugin.json` at its root. That file has exactly one schema —
//! [`manifest::PluginManifest`] — and this crate owns it, along with the rules
//! that make it safe to install and the record of what has been installed.
//!
//! # Why it is shared vocabulary and not a binary's private type
//!
//! More than one consumer has to answer "what is installed and what did it
//! declare", and they disagree if each answers it alone. A package that
//! declares a plugin under `capabilities.kernelPlugins` should be loadable by
//! name, not by the user writing its module path a second time, so the plugin
//! plane has to read the same file the installer validated.
//!
//! *Acting* on a package — unpacking archives, verifying digests, writing
//! install state, materialising contributions into a running session — belongs
//! to the caller. What lives here is the vocabulary those actions are written
//! in, plus the precedence rule that says which of several installed packages
//! is in effect.

pub mod acp_agent_manifest;
pub mod discovery;
pub mod manifest;
pub mod model_provider;
pub mod model_provider_manifest;
pub mod security;
pub mod store;

pub use discovery::{discover, Discovered, DiscoveredPlugin, InstalledPlugin, PluginOrigin};
pub use manifest::{PluginCapabilities, PluginManifest, PLUGIN_MANIFEST_FILE};
pub use store::{InstalledPluginRecord, PluginScope, PluginSourceKind, PluginStore};
