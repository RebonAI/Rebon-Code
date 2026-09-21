//! Model provider contributions, as a package declares and materializes them.
//!
//! These are shared vocabulary rather than one consumer's private types: more
//! than one layer needs to read the same declarations, and they disagree if
//! each carries its own copy.
//!
//! Everything here is data. The one behaviour these types had — merging in the
//! capabilities a provider reports at run time — needs the wire protocol to say
//! what those are, so it lives beside the protocol as an extension trait.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rebon_types::ModelProfileMap;
use serde::{Deserialize, Serialize};

/// What a provider says it can do, before it has been asked.
///
/// A manifest may under-claim: the provider can report more at `initialize`,
/// and the union wins. It may not over-claim into working behaviour — a
/// capability rebon acts on that the provider does not implement fails at the
/// call, not here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderCapabilityManifest {
    #[serde(default)]
    pub request_scoped_transient_context: bool,
    #[serde(default)]
    pub forced_tool_choice: bool,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default)]
    pub computer_use: bool,
    #[serde(default)]
    pub context_management: bool,
    #[serde(default)]
    pub stateful_responses: bool,
    #[serde(default)]
    pub remote_compaction: bool,
    #[serde(default)]
    pub anchored_minimal: bool,
    #[serde(default)]
    pub reasoning_text: bool,
    #[serde(default)]
    pub custom_tool_call: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderModelManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// One provider a package contributes, with its placeholders already expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginModelProviderContribution {
    pub id: String,
    pub plugin_name: String,
    pub source: String,
    pub display_name: Option<String>,
    pub transport: MaterializedModelProviderTransport,
    pub capabilities: ModelProviderCapabilityManifest,
    pub default_model: Option<String>,
    pub models: BTreeMap<String, ModelProviderModelManifest>,
    pub profiles: ModelProfileMap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterializedModelProviderTransport {
    Plugin(MaterializedPluginModelProviderTransport),
}

/// Where a provider's module lives, as a plugin load request needs it.
///
/// Two fields where the child-process transport had nine. The seven that are
/// gone were all about running a process — the command, its arguments, its
/// environment, its working directory, and four timeouts — and the plugin host
/// answers every one of them for every plugin it runs, so a provider no longer
/// gets to answer them differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedPluginModelProviderTransport {
    /// The package root the entry is relative to.
    pub root: PathBuf,
    pub entry: String,
}
