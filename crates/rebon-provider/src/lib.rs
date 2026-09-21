//! Model providers, from the catalog down to the wire.
//!
//! This crate owns the provider layer end to end. Everything here changes for
//! one reason: a provider changed — a new entry in the store, a package that
//! declares a `modelProviders` block, a routing rule, a chunk the dsh protocol
//! grew. Assembling a session does not, which is why that stays elsewhere.
//!
//! The four faces, from the outside in:
//!
//! - [`provider_catalog`] — one list of every provider Rebon can see, whatever
//!   supplied it, and whether it can actually be used.
//! - [`provider_runtime_cache`] — the per-provider runtime a cross-provider
//!   sub-agent spawn reuses instead of rebuilding.
//! - [`kernel_model_router`] — the `model-router` kernel seat: the exact
//!   `{provider, model}` pair is the routing key, and a builtin provider always
//!   wins over a plugin route of the same name.
//! - [`model_provider_plugin`] / [`plane_model_provider`] /
//!   [`kernel_llm_dispatch`] — what a plugin contributes (manifest data), that
//!   contribution bound to the plugin host, and the translation between the dsh
//!   `StreamChunk` vocabulary and rebon's own.
//!
//! - [`oauth_refresher`] — the `TokenRefresher` an OpenAI Responses client is
//!   built with, which reads and rotates the stored OAuth tokens.
//!
//! Resolving a provider *entry* into a client is
//! `rebon_plugin_host::provider_registry`, not this crate: its external arm has
//! to reach the plugin host to load an adapter on demand, and the host sits
//! above this crate. Everything that arm needs on the way — the client
//! constructors, the refresher — is here, so the edge is one call and not a
//! layer.

pub mod kernel_llm_dispatch;
pub mod kernel_model_router;
pub mod model_provider_plugin;
pub mod oauth_refresher;
pub mod plane_model_provider;
pub mod provider_catalog;
pub mod provider_runtime_cache;
