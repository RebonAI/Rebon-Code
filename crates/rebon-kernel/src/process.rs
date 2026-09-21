//! Where this process keeps its one plugin registry.
//!
//! Storing the registry and booting it are two different jobs, and only the
//! second needs to know what a plugin is. Booting means building the
//! definition table, reading the switches, reconciling, and subscribing to
//! the settings file — all of which names every plugin crate in the build, so
//! it can only live in the assembly layer that links them together. *Holding*
//! the result needs none of that, so it lives here, underneath everything.
//!
//! That split is what lets a seat or a plugin host ask for the kernel without
//! depending on the assembly crate that built it. The assembly layer calls
//! [`install_process_registry`] once, at the end of its boot; everyone else
//! reads [`process_registry`] or [`process_kernel`].
//!
//! # What `None` means
//!
//! Exactly one thing: **this process has not booted yet**. It does not mean
//! "the kernel failed", "the plugin is disabled", or "ask again later on this
//! thread" — a caller that reads `None` is running before the assembly layer
//! got there, and the honest answer is whatever that caller's seat says when
//! its service is missing.
//!
//! A caller that cannot answer without a kernel must therefore be handed one
//! by whoever called *it*, rather than reading this slot and hoping. The slot
//! is for the reads that provably happen after the boot.

use std::sync::{Arc, OnceLock};

use crate::{Kernel, PluginRegistry};

/// The process's registry, once something installed one.
static REGISTRY: OnceLock<Arc<PluginRegistry>> = OnceLock::new();

/// A second boot tried to claim the slot the first one already holds.
///
/// One process has one kernel: whoever lost the race must use the registry
/// that is already there rather than run a second one alongside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyInstalled;

impl std::fmt::Display for AlreadyInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this process already installed a plugin registry")
    }
}

impl std::error::Error for AlreadyInstalled {}

/// Claim the process slot for `registry`.
///
/// Called by the assembly layer at the end of its boot, and by nobody else.
/// The first caller wins for the life of the process.
pub fn install_process_registry(registry: Arc<PluginRegistry>) -> Result<(), AlreadyInstalled> {
    REGISTRY.set(registry).map_err(|_| AlreadyInstalled)
}

/// The registry this process booted, or `None` if it has not booted one.
pub fn process_registry() -> Option<Arc<PluginRegistry>> {
    REGISTRY.get().cloned()
}

/// The kernel this process booted, or `None` if it has not booted one.
pub fn process_kernel() -> Option<Arc<Kernel>> {
    Some(process_registry()?.kernel().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Installing answers both reads with the same instance, and a second
    /// install is refused rather than swapping the process's kernel out from
    /// under everything already holding it.
    ///
    /// One test rather than three: the slot is process-wide and `OnceLock` is
    /// permanent, so a second test could only ever see the first one's state.
    #[test]
    fn the_slot_is_empty_until_installed_and_then_permanent() {
        assert!(process_registry().is_none(), "nothing installed one yet");
        assert!(process_kernel().is_none());

        let kernel = Kernel::new();
        let registry = PluginRegistry::new(
            kernel.clone(),
            &[],
            crate::PluginHost {
                kernel: kernel.clone(),
                config_dir: std::path::PathBuf::from("."),
            },
        );
        install_process_registry(registry.clone()).expect("the first install wins");

        assert!(Arc::ptr_eq(
            &process_registry().expect("installed"),
            &registry
        ));
        assert!(Arc::ptr_eq(&process_kernel().expect("installed"), &kernel));

        let second = Kernel::new();
        let other = PluginRegistry::new(
            second.clone(),
            &[],
            crate::PluginHost {
                kernel: second,
                config_dir: std::path::PathBuf::from("."),
            },
        );
        assert_eq!(install_process_registry(other), Err(AlreadyInstalled));
    }
}
