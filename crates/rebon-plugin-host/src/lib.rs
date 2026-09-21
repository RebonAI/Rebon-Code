//! The plugin plane: one Node host, however many external plugins — and
//! everything about starting one and keeping it in step with what a person
//! configured.
//!
//! Split out of the layer that assembles a session. What changes here is the
//! *protocol and the process* — a new method on the wire, a manifest field, a
//! payload patch, a rule about which seats a plugin may see, a decision about
//! when a host should come up. Which plugins a build ships is
//! that layer's own plugin list, above; nothing here boots a kernel.
//!
//! The plane itself:
//!
//! - [`plugin_plane`] — the host: load, unload, route, and the seat exposure
//!   rules.
//! - [`plugin_composition`] — the composition `kernelPlugins` describes, and
//!   where its payload lives.
//! - [`plugin_manifests`] — a package manifest turned into a load request.
//! - [`compose_patches`] — the patches a vendored payload needs before it runs.
//!
//! Starting one, and what runs on it:
//!
//! - [`plugin_boot`] — the process-wide plane: whether to start one, the
//!   refusal when it cannot, the credential grants that ride its fork.
//! - [`kernel_node_host`] — the host half of the `node-host` plugin: which
//!   entries this machine could run, and which of them a switch asks for.
//! - [`loop_host`] / [`kernel_loop_backend`] / [`kernel_loop_plane`] — a vendor
//!   agent loop as an `AgentBackend`, and the plane host one runs on.
//! - [`provider_registry`] — which model providers this build can connect to.
//!   It lives here rather than in `rebon-provider` for one reason, written
//!   down in the module: an external provider is *loaded on the plane*, so a
//!   registry that can build one has to be able to name the plane.
//!
//! Every entry point that needs the process kernel takes it as an argument —
//! usually the `PluginRegistry`, since that is both the kernel and the row
//! table `node:*` entries live in. Nothing here reads
//! `rebon_kernel::process_registry()`: a plane can be asked to start before
//! the assembly layer has booted (the desktop's boot and its kernel boot are
//! two tasks on one runtime), and a plane that refused because it lost that
//! race would be a plugin that silently does not exist.

pub mod compose_patches;
pub mod kernel_loop_backend;
pub mod kernel_loop_plane;
pub mod kernel_node_host;
pub mod loop_host;
pub mod plugin_boot;
pub mod plugin_composition;
pub mod plugin_manifests;
pub mod plugin_plane;
pub mod provider_registry;
