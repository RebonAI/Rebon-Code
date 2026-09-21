//! What `/help` lists is what the command-registry seat holds right now.
//!
//! The terminal seeds its slash-command list before the session is built, and
//! the session is what boots the kernel; a help page built from that snapshot
//! shows the built-in fallback table and none of the commands the `agents`,
//! `tasks`, `updater` or `profile` plugins register. These two assertions are
//! in one test on purpose: the second half switches a plugin off in the
//! process-wide registry, so it must not run beside the first.

use rebon_harness::kernel_bootstrap::{process_kernel, process_plugin_registry};
use rebon_slash_commands::help::{catalog_command_rows, HelpCommandRow};
use rebon_slash_commands::Surface;

fn labels() -> Vec<String> {
    catalog_command_rows(Surface::Tui)
        .into_iter()
        .map(|HelpCommandRow { label, .. }| {
            label.split_whitespace().next().unwrap_or("").to_string()
        })
        .collect()
}

#[test]
fn the_help_command_list_is_the_seat_and_follows_a_plugin_switch() {
    process_kernel();

    let listed = labels();
    for command in ["/agents", "/tasks", "/update"] {
        assert!(
            listed.iter().any(|label| label == command),
            "{command} is registered by a plugin and must appear in /help; listed: {listed:?}"
        );
    }

    let registry = process_plugin_registry();
    registry
        .set_enabled(rebon_plugin_agents::PLUGIN_ID, false)
        .expect("agents is a feature plugin");
    let without_agents = labels();
    registry
        .set_enabled(rebon_plugin_agents::PLUGIN_ID, true)
        .expect("and back");

    assert!(
        !without_agents.iter().any(|label| label == "/agents"),
        "switching the agents plugin off takes /agents out of /help; listed: {without_agents:?}"
    );
    assert!(
        without_agents.iter().any(|label| label == "/tasks"),
        "the other plugins' commands stay; listed: {without_agents:?}"
    );
    assert!(
        labels().iter().any(|label| label == "/agents"),
        "switching it back on puts /agents back"
    );
}
