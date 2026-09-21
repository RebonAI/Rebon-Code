//! Engine-side bridge runtime state.
//!
//! This module is the first step in evolving [`crate::Engine`] from a pure
//! tool registry into the owner of the bridge runtime. It composes the
//! foundation primitives exposed by `rebon-bridge` (`BridgeHandle` +
//! `ActiveHandleSlot`) with a small engine-owned config snapshot and
//! status enum, so `Engine` holds an active bridge handle —
//! `rebon_bridge::ReplBridgeHandle` is the concrete implementation.
//!
//! Key types:
//!
//! - [`BridgeConfig`] — subset of the bridge bring-up config.
//! - [`BridgeStatus`] — small connection-state enum making the
//!   "is a bridge active" idea explicit.
//!
//! The actual handle trait lives in `rebon_bridge::active_handle` and is
//! re-eximplemented for this module as a convenience so downstream callers
//! only need `use rebon_core::bridge::*`.

use std::fmt;
use std::sync::Arc;

pub use rebon_bridge::{ActiveHandleChange, BridgeHandle};

/// Minimal view of the config used to bring up a bridge.
///
/// This carries only the
/// fields `Engine` needs to identify a running bridge and surface it in
/// diagnostics. The full config (spawn mode, sandbox flags, timeouts,
/// etc.) lives alongside the handle type in [`rebon_bridge::BridgeConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    /// Client-generated UUID identifying this bridge instance.
    pub bridge_id: String,
    /// Backend-issued or client-generated environment id.
    pub environment_id: String,
    /// `worker_type` metadata field (opaque, e.g. `"claude_code"`).
    pub worker_type: String,
    /// API base URL the bridge is polling.
    pub api_base_url: String,
    /// Session ingress base URL for websocket connections.
    pub session_ingress_url: String,
}

impl BridgeConfig {
    /// Convenience constructor accepting any string-like inputs.
    pub fn new(
        bridge_id: impl Into<String>,
        environment_id: impl Into<String>,
        worker_type: impl Into<String>,
        api_base_url: impl Into<String>,
        session_ingress_url: impl Into<String>,
    ) -> Self {
        Self {
            bridge_id: bridge_id.into(),
            environment_id: environment_id.into(),
            worker_type: worker_type.into(),
            api_base_url: api_base_url.into(),
            session_ingress_url: session_ingress_url.into(),
        }
    }
}

/// Project a full [`rebon_bridge::BridgeConfig`] into the narrow
/// diagnostic subset the engine tracks.
///
/// The runtime-facing config owned by `rebon-bridge` has ~14 fields
/// (dir, branch, git_repo_url, spawn_mode, …) that `Engine` never
/// needs to look at. This impl forwards only the diagnostic subset —
/// the full config still lives on the `ReplBridgeHandle` for callers
/// that need it.
impl From<&rebon_bridge::BridgeConfig> for BridgeConfig {
    fn from(full: &rebon_bridge::BridgeConfig) -> Self {
        Self {
            bridge_id: full.bridge_id.clone(),
            environment_id: full.environment_id.clone(),
            worker_type: full.worker_type.clone(),
            api_base_url: full.api_base_url.clone(),
            session_ingress_url: full.session_ingress_url.clone(),
        }
    }
}

/// High-level connection state of the bridge owned by an [`crate::Engine`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BridgeStatus {
    /// No bridge is attached (either never attached or already detached).
    #[default]
    Detached,
    /// A bridge handle is currently attached to the engine.
    Attached,
}

/// Engine-owned bridge runtime state.
///
/// Holds an optional active [`BridgeHandle`] alongside the config used
/// to bring it up and a high-level status enum. This is the canonical
/// value `Engine` stores behind a lock; [`Engine::bridge_state`]
/// returns a clone, which is why the type implements `Clone` manually
/// (`Arc<dyn BridgeHandle>` is `Clone` but the trait is not `Debug`, so
/// neither derive works automatically).
///
/// [`Engine::bridge_state`]: crate::Engine::bridge_state
pub struct BridgeRuntimeState {
    pub status: BridgeStatus,
    pub config: Option<BridgeConfig>,
    pub handle: Option<Arc<dyn BridgeHandle>>,
}

impl BridgeRuntimeState {
    /// Whether a bridge handle is currently attached.
    pub fn is_attached(&self) -> bool {
        matches!(self.status, BridgeStatus::Attached)
    }
}

impl Default for BridgeRuntimeState {
    fn default() -> Self {
        Self {
            status: BridgeStatus::Detached,
            config: None,
            handle: None,
        }
    }
}

impl Clone for BridgeRuntimeState {
    fn clone(&self) -> Self {
        Self {
            status: self.status,
            config: self.config.clone(),
            handle: self.handle.clone(),
        }
    }
}

impl fmt::Debug for BridgeRuntimeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `dyn BridgeHandle` doesn't require `Debug`, so surface only
        // whether a handle is present along with its `bridge_session_id`.
        f.debug_struct("BridgeRuntimeState")
            .field("status", &self.status)
            .field("config", &self.config)
            .field(
                "handle",
                &self
                    .handle
                    .as_ref()
                    .map(|h| h.bridge_session_id().to_owned()),
            )
            .finish()
    }
}
