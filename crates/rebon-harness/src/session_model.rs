//! The resolved model and provider a session is running on.
//!
//! [`crate::RuntimeModel`] is what provider resolution *returns*: a client, a
//! system-prompt snapshot, cache verdicts, profile maps. [`SessionModel`] is
//! the part of that answer a session *keeps* — the names surfaces display and
//! the shared knobs `/model`, `/settings` and the status bar read.
//!
//! One type for both surfaces on purpose. `HeadlessSession` and the CLI's
//! `EngineSession` had four and eight fields respectively saying the same
//! thing, and the CLI had the same ten-line re-adoption block written twice
//! (`/provider` refresh and a worker switching model for one turn). Those
//! blocks are [`SessionModel::adopt`] now.

use rebon_api::{PruneLevelHandle, ServiceTierHandle};
use rebon_config::ProviderFormat;
use rebon_core::query::SharedRuntimeModel;

use crate::RuntimeModel;

/// What a session keeps out of a resolved [`RuntimeModel`].
pub struct SessionModel {
    /// Human-readable provider name for the status bar: `"openai"` /
    /// `"anthropic"` / a custom provider name, or `"env"` for the env-var
    /// fallback.
    pub provider_name: String,
    /// Resolved provider transport format. Surfaces gate on it where backend
    /// support differs per transport.
    pub provider_format: ProviderFormat,
    /// The model this session's next turn will run on.
    pub name: String,
    /// Model a spawn helper gets when it names none — the `/agent` slash
    /// command delegating to the session default, say.
    pub default_name: String,
    /// Concrete model used by title generation and model-backed compaction.
    pub title_name: String,
    /// Shared handle the surfaces use to toggle context-prune level at runtime.
    pub prune_level: PruneLevelHandle,
    /// Shared runtime switch for OpenAI fast service tier.
    pub service_tier: ServiceTierHandle,
    /// Whether fast service tier can affect the current main provider.
    pub service_tier_available: bool,
    /// Shared runtime model config the executor reads, so `/provider` and
    /// `/model` take effect on the next turn without rebuilding the session.
    pub runtime_model: SharedRuntimeModel,
}

impl SessionModel {
    /// The session's view of a freshly resolved runtime.
    pub fn from_runtime(runtime: &RuntimeModel) -> Self {
        Self {
            provider_name: runtime.provider_name.clone(),
            provider_format: runtime.provider_format,
            name: runtime.model.clone(),
            default_name: runtime.model.clone(),
            title_name: runtime.title_model.clone(),
            prune_level: runtime.prune_level.clone(),
            service_tier: runtime.service_tier.clone(),
            service_tier_available: runtime.service_tier_available,
            runtime_model: runtime.runtime_model.clone(),
        }
    }

    /// Point this session at a newly resolved runtime.
    ///
    /// The shared runtime-model cell is *written through* rather than
    /// replaced: the executor is already holding it, and handing it a new cell
    /// would leave the running turn reading the old one.
    ///
    /// The caller still owns what does not belong to a session's model —
    /// the model client and the retry notifier live on the engine half.
    pub fn adopt(&mut self, runtime: &RuntimeModel) {
        self.provider_name = runtime.provider_name.clone();
        self.provider_format = runtime.provider_format;
        self.name = runtime.model.clone();
        self.default_name = runtime.model.clone();
        self.title_name = runtime.title_model.clone();
        self.prune_level = runtime.prune_level.clone();
        self.service_tier = runtime.service_tier.clone();
        self.service_tier_available = runtime.service_tier_available;
        self.runtime_model.set(runtime.runtime_model.get());
    }
}
