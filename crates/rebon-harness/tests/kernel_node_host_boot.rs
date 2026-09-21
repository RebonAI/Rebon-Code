//! The `node-host` plugin, on the real plugin table.
//!
//! Moved here with the module itself. The question is whether the boot
//! this crate performs *loads* the plugin and leaves its service resolvable,
//! and only the crate that owns `builtin_plugin_defs()` can boot that table.
//! What stayed beside the module is what reads the definition alone.

use rebon_plugin_host::kernel_node_host::PLUGIN_ID;

/// The plane runs because a plugin is loaded, so the plugin has to be
/// loaded — and its service has to be resolvable, since that is the
/// question every host now asks instead of booting a plane itself.
#[test]
fn the_process_kernel_loads_the_host_and_offers_its_service() {
    let kernel = rebon_harness::kernel_bootstrap::process_kernel();
    assert!(
        kernel.plugin_names().iter().any(|name| name == PLUGIN_ID),
        "node-host is missing from {:?}",
        kernel.plugin_names()
    );
    assert!(
        kernel
            .context()
            .get::<rebon_plugin_node_host::NodeHostService>()
            .is_some(),
        "the service a host resolves before booting a plane"
    );
}
