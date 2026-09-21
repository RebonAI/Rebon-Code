//! Model provider contributions, as the harness consumes them.
//!
//! The values themselves moved to `rebon-plugin-package`, below both this crate
//! and the plugin plane that now also reads manifests. They are re-exported
//! here so every existing `crate::model_provider_plugin::…` import keeps
//! resolving — the move is about who may depend on whom, not about renaming
//! anything.
//!
//! What stayed is the one thing that is not data: folding in the capabilities a
//! provider reports at `initialize`, which needs the wire protocol to say what
//! those are.

use rebon_api::model_provider_protocol::ModelProviderRuntimeCapabilitiesV1;
pub use rebon_plugin_package::model_provider::{
    MaterializedModelProviderTransport, MaterializedPluginModelProviderTransport,
    ModelProviderCapabilityManifest, ModelProviderModelManifest, PluginModelProviderContribution,
};

/// Folds what a provider reports at run time into what its manifest declared.
///
/// An extension trait rather than an inherent method because the type is no
/// longer this crate's to add methods to. The union is deliberate: a manifest
/// may under-claim and the provider corrects it at `initialize`, and a provider
/// that reports nothing leaves a declaring manifest intact.
pub trait ModelProviderCapabilityManifestExt {
    fn with_runtime_capabilities(self, runtime: &ModelProviderRuntimeCapabilitiesV1) -> Self;
}

impl ModelProviderCapabilityManifestExt for ModelProviderCapabilityManifest {
    fn with_runtime_capabilities(mut self, runtime: &ModelProviderRuntimeCapabilitiesV1) -> Self {
        self.request_scoped_transient_context |= runtime.request_scoped_transient_context;
        self.forced_tool_choice |= runtime.forced_tool_choice;
        self.web_search |= runtime.web_search;
        self.computer_use |= runtime.computer_use;
        self.context_management |= runtime.context_management;
        self.stateful_responses |= runtime.stateful_responses;
        self.remote_compaction |= runtime.remote_compaction;
        self.anchored_minimal |= runtime.anchored_minimal;
        self.reasoning_text |= runtime.reasoning_text;
        self.custom_tool_call |= runtime.custom_tool_call;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_capabilities_or_merge_anchored_minimal() {
        let runtime = ModelProviderRuntimeCapabilitiesV1 {
            anchored_minimal: true,
            ..Default::default()
        };
        assert!(
            ModelProviderCapabilityManifest::default()
                .with_runtime_capabilities(&runtime)
                .anchored_minimal
        );

        let manifest = ModelProviderCapabilityManifest {
            anchored_minimal: true,
            ..Default::default()
        };
        assert!(
            manifest
                .with_runtime_capabilities(&ModelProviderRuntimeCapabilitiesV1::default())
                .anchored_minimal
        );
    }
}
