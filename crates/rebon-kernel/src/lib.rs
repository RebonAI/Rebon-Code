//! The minimal plugin kernel.
//!
//! The core is a [`Context`] tree in which every registration is an effect
//! yielding a [`Disposer`]. Disposers are collected into the [`Scope`] that
//! owns them and unwound in reverse registration order when that scope is
//! disposed, so unloading a plugin or a session fork leaves nothing behind.
//! Capabilities meet through the service registry (the [`Service`] definition
//! / provider / consumer triangle) and through the event bus.
//!
//! # Dispatch modes
//!
//! The event plane exposes four synchronous dispatch primitives, together
//! covering the five known modes (`serial` being the async flavour of `bail`):
//!
//! | Mode | Primitive | Listener panics |
//! | --- | --- | --- |
//! | broadcast | `emit` | contained |
//! | first-non-empty | `bail` | propagated |
//! | middleware chain | `waterfall` | propagated |
//! | all-run barrier | `parallel` | contained |
//!
//! `bail` and `waterfall` are *policy* modes: a listener panic is a bug the
//! caller must see, so it unwinds. `emit` and `parallel` are *observation*
//! modes: one bad listener must not take down dispatch for the rest, so panics
//! are contained.
//!
//! Scoped dispatch (`emit_scoped`, `parallel_scoped`, and their JSON twins)
//! confines an event to the dispatching fork's root-to-leaf chain — sibling
//! forks never observe it. See the `events` module for the full contract.
//!
//! # Two planes
//!
//! Every surface exists twice:
//!
//! - the **typed plane** — Rust generics, zero-cost, for in-process plugins;
//! - the **JSON plane** — string names plus `serde_json::Value`, which is the
//!   ABI for dynamically-typed hosts such as an embedded JS runtime.

mod context;
mod def;
mod disposer;
mod error;
mod events;
mod kernel;
mod plugin;
pub mod process;
mod registry;
pub mod seat;
mod service;
pub mod testing;

pub use context::Context;
pub use def::{
    ConfigChanged, ConfigFileKind, DynPluginDef, PluginDef, PluginFactory, PluginHost, PluginKind,
    PluginState, PluginStateChanged, SessionClosed, SessionOpened,
};
pub use disposer::{Disposer, Scope};
pub use error::KernelError;
pub use events::{EventBusStats, JsonNext, Next};
pub use kernel::{Kernel, UnloadError, UnloadOptions, UnloadReport};
pub use plugin::{Plugin, PluginMeta, SettingKey, SettingType, SETTINGS_SERVICE};
pub use process::{install_process_registry, process_kernel, process_registry, AlreadyInstalled};
pub use registry::{DesiredSet, PluginRegistry, PluginStatus, ReconcileReport, RegistryError};
pub use seat::{SeatError, SeatRegistry};
pub use service::{JsonService, Service, ServiceLease};
