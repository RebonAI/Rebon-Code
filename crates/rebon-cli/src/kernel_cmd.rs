//! `rebon kernel …` — the kernel loop vendors and where they run.
//!
//! Kernel loops run on the plugin plane: each loop's composition entries are
//! plugins on a Node host of the session's own. There is nothing to install
//! here — the runtime that hosts them is Node, and `rebon node` is where that
//! lives.
//!
//! This module used to install a sidecar process carrying
//! an embedded V8 so a V8-free rebon could still run loops. The plane serves a
//! V8-free rebon directly, which left the sidecar as a second way to do what the
//! first way already did.

use rebon_plugin_host::kernel_loop_backend::KernelLoopConfig;
use rebon_plugin_host::loop_host::KNOWN_LOOP_VENDORS;

use crate::KernelCommand;

pub(crate) async fn run(command: KernelCommand) -> anyhow::Result<()> {
    match command {
        KernelCommand::Status => status(),
    }
}

fn status() -> anyhow::Result<()> {
    let config_dir = ::rebon_config::config_home_dir();
    println!(
        "Host: {}",
        rebon_plugin_host::loop_host::describe_loop_host()
    );

    println!("\nLoop vendors (/kernel <name> in a session):");
    for vendor in KNOWN_LOOP_VENDORS {
        let state = if !vendor.bundled {
            "assembly not bundled yet".to_string()
        } else {
            match KernelLoopConfig::for_vendor(&config_dir, vendor.id) {
                Some(config) => format!("configured → {}/{}", config.provider, config.model),
                None => format!("not configured (kernelPlugins.loops.{})", vendor.id),
            }
        };
        println!("  {} — {}  [{}]", vendor.id, vendor.label, state);
    }

    // The runtime a loop needs is the plugin plane's, so the answer to "can
    // this machine run one" is the answer `rebon node status` gives.
    println!("\nRuntime: `rebon node status` says which Node the plane would use.");
    Ok(())
}
