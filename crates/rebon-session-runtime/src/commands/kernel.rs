//! `/kernel`: which loop runs this session, and reloading the plugin plane.
//!
//! A session command like the rest of this module: the terminal runs it
//! when the user types it, and a worker runs the same code over IPC
//! (`super::control` dispatches both). It reaches into `rebon-harness`
//! for loop vendors and the plugin plane, which is why it is here rather
//! than in `rebon-agent-core::routing` with the other agent switching -- the
//! desktop app boots no plane, so the shared crate would have gained the
//! harness for a command it never runs.
//!
//! Recognising `/kernel` in typed text stays with the other parsers, in
//! the binary's `tui::runner::commands`.

use rebon_agent_core::routing::{AgentCommandResult, SessionAgents, KERNEL_CAVEAT};
use rebon_config::LOCAL_AGENT_ID;
use rebon_plugin_host::loop_host::{
    describe_loop_host, loop_backend_id, loop_vendor, KNOWN_LOOP_VENDORS,
};

/// Run `/kernel`, returning what to show the user.
///
/// `rebon` is the native engine (the `local` backend under its own
/// name); every other name is a loop vendor from the kernel roster,
/// switched to as its `kernel:<vendor>` backend.
/// What a reload moved, named rather than counted.
///
/// A count would not tell anyone which plugin came back different, and a reload
/// is exactly the moment that matters.
fn render_reload_outcome(outcome: &rebon_plugin_host::plugin_plane::ReloadOutcome) -> String {
    if !outcome.touched_anything() && outcome.failed.is_empty() {
        return format!(
            "Composition unchanged — {} {} already match the configuration (generation {}).",
            outcome.unchanged.len(),
            if outcome.unchanged.len() == 1 {
                "plugin"
            } else {
                "plugins"
            },
            outcome.generation
        );
    }

    let mut lines = vec![format!(
        "Composition reloaded — generation {}.",
        outcome.generation
    )];
    for id in &outcome.added {
        lines.push(format!("  + {id}  started"));
    }
    for id in &outcome.changed {
        lines.push(format!("  ~ {id}  restarted"));
    }
    for id in &outcome.removed {
        lines.push(format!("  - {id}  stopped"));
    }
    if !outcome.unchanged.is_empty() {
        lines.push(format!(
            "  = {} left running untouched",
            outcome.unchanged.len()
        ));
    }
    for (id, reason) in &outcome.failed {
        lines.push(format!("  ! {id}  {reason}"));
    }
    lines.join("\n")
}

pub fn handle_kernel_command(
    agents: &SessionAgents<rebon_acp_client::AcpAgentBackend>,
    args: &str,
    runtime: Option<&tokio::runtime::Handle>,
) -> AgentCommandResult {
    let args = args.trim();
    if args.is_empty() || args == "list" {
        return AgentCommandResult {
            text: render_kernel_list(agents),
            is_err: false,
        };
    }

    if args.eq_ignore_ascii_case("rebon") || args.eq_ignore_ascii_case(LOCAL_AGENT_ID) {
        return match agents.switch_to(LOCAL_AGENT_ID) {
            Ok(label) => AgentCommandResult {
                text: format!("Session now runs on {label}."),
                is_err: false,
            },
            Err(message) => AgentCommandResult {
                text: message,
                is_err: true,
            },
        };
    }

    if args.eq_ignore_ascii_case("reload") {
        // The reconciler the embedded composition had, on the plane: re-read
        // `kernelPlugins` and restart exactly what changed.
        let Some(runtime) = runtime else {
            return AgentCommandResult {
                text:
                    "This surface cannot reload a composition — it has no runtime to reconcile on."
                        .to_string(),
                is_err: true,
            };
        };
        // A reload takes as long as stopping and starting the plugins that
        // changed, so it blocks. Not with `Handle::block_on` on this thread:
        // the TUI dispatches from a `spawn_blocking` worker where that is
        // legal, but the app-hosted IPC path dispatches from a runtime worker
        // where it panics. A dedicated thread may block on the handle from
        // every dispatch surface.
        let handle = runtime.clone();
        let outcome = std::thread::spawn(move || {
            handle.block_on(rebon_plugin_host::plugin_boot::reload_process_composition(
                &rebon_harness::kernel_bootstrap::process_plugin_registry(),
            ))
        })
        .join();
        return match outcome {
            Ok(Ok(outcome)) => AgentCommandResult {
                is_err: !outcome.failed.is_empty(),
                text: render_reload_outcome(&outcome),
            },
            Ok(Err(reason)) => AgentCommandResult {
                text: format!("Reload failed: {reason}"),
                is_err: true,
            },
            Err(_) => AgentCommandResult {
                text: "Reload failed: the reload thread panicked.".to_string(),
                is_err: true,
            },
        };
    }

    // Kernel plugin registry (RFC kernel-plugins §4), driven from the terminal.
    if args.eq_ignore_ascii_case("plugins") {
        let registry = rebon_harness::kernel_bootstrap::process_plugin_registry();
        return AgentCommandResult {
            text: rebon_harness::kernel_bootstrap::render_plugin_registry_snapshot(
                &registry.snapshot(),
                &plane_contributions,
            ),
            is_err: false,
        };
    }
    if let Some((verb, id)) = args.split_once(char::is_whitespace) {
        let id = id.trim();
        let registry = rebon_harness::kernel_bootstrap::process_plugin_registry();
        let verb = verb.to_ascii_lowercase();
        let (outcome, persist) = match verb.as_str() {
            "reload" if !id.is_empty() => (Some(registry.reload(id)), None),
            "enable" if !id.is_empty() => (Some(registry.set_enabled(id, true)), Some(true)),
            "disable" if !id.is_empty() => (Some(registry.set_enabled(id, false)), Some(false)),
            _ => (None, None),
        };
        if let Some(outcome) = outcome {
            return match outcome {
                Ok(report) => {
                    // The switch outlives the session: settings.json carries
                    // it, and that write's ConfigChanged is a no-op reconcile.
                    let mut text = rebon_harness::kernel_bootstrap::render_reconcile_report(
                        &verb, id, &report,
                    );
                    if let Some(enabled) = persist {
                        match rebon_config::save_plugin_enabled(id, enabled) {
                            Ok(()) => {
                                // Other sessions run in their own processes with
                                // their own registry; tell the live ones now.
                                let store = rebon_session_host::BackgroundStore::new(
                                    rebon_config::config_home_dir(),
                                );
                                let outcomes =
                                    rebon_session_host::broadcast_reconcile_plugins(&store);
                                let reached = outcomes.iter().filter(|(_, o)| o.is_ok()).count();
                                let missed: Vec<String> = outcomes
                                    .iter()
                                    .filter_map(|(job, o)| {
                                        o.as_ref().err().map(|e| format!("{job}: {e}"))
                                    })
                                    .collect();
                                if reached > 0 || !missed.is_empty() {
                                    text.push_str(&format!(
                                        "\n  = {reached} running session(s) reconciled"
                                    ));
                                }
                                for miss in missed {
                                    text.push_str(&format!("\n  ! not reached — {miss}"));
                                }
                            }
                            Err(err) => text.push_str(&format!(
                                "\n  ! switch not saved to settings.json: {err:#}"
                            )),
                        }
                    }
                    AgentCommandResult {
                        is_err: !report.is_clean(),
                        text,
                    }
                }
                Err(err) => AgentCommandResult {
                    text: err.to_string(),
                    is_err: true,
                },
            };
        }
    }
    let Some(vendor) = loop_vendor(args) else {
        let known = KNOWN_LOOP_VENDORS
            .iter()
            .map(|vendor| vendor.id)
            .collect::<Vec<_>>()
            .join(", ");
        return AgentCommandResult {
            text: format!("no kernel named `{args}` — known kernels: rebon, {known}"),
            is_err: true,
        };
    };
    if !vendor.bundled {
        return AgentCommandResult {
            text: format!(
                "`{}` holds a seat in the kernel roster, but its loop assembly is \
                 not bundled in this build yet",
                vendor.id
            ),
            is_err: true,
        };
    }
    let backend_id = loop_backend_id(vendor.id);
    if !agents.has_kernel_backend(&backend_id) {
        return AgentCommandResult {
            text: format!(
                "`{id}` is not configured — add `kernelPlugins.loops.{id}` \
                 (`{{\"provider\": …, \"model\": …}}`) to config.json, then start \
                 a new session",
                id = vendor.id
            ),
            is_err: true,
        };
    }

    match agents.switch_to(&backend_id) {
        Ok(label) => {
            let mut text = format!("Session now runs on {label} (takes effect next turn).");
            text.push_str(&format!("\n  {KERNEL_CAVEAT}"));
            // Say where this session's turns will actually run, so the choice
            // is verifiable at the moment it is made and not only in the log.
            text.push_str(&format!("\n  host: {}", describe_loop_host()));
            AgentCommandResult {
                text,
                is_err: false,
            }
        }
        Err(message) => AgentCommandResult {
            text: message,
            is_err: true,
        },
    }
}

/// What the running plane says each plugin registered.
///
/// A `node:*` plugin declares no kernel services in its `meta()` -- it cannot,
/// the declaration is read before the plugin has loaded -- so the registry
/// snapshot alone shows it contributing nothing. What it actually contributed
/// lives on the plane, and this is where the two are put back together. No
/// plane running is a legitimate "nothing", not an error, and asking must not
/// start one.
fn plane_contributions(plugin_id: &str) -> (Vec<String>, Vec<String>) {
    let Some(plane) = rebon_plugin_host::plugin_boot::running_plugin_plane() else {
        return (Vec::new(), Vec::new());
    };
    // The registry row is `node:<entry>`; the plane knows it by the entry.
    let entry = plugin_id.strip_prefix("node:").unwrap_or(plugin_id);
    (
        plane.registered_services(entry),
        plane.registered_commands(entry),
    )
}

/// The registry table `/plugin list` appends after the installed packages.
pub(crate) fn kernel_plugins_summary() -> String {
    let registry = rebon_harness::kernel_bootstrap::process_plugin_registry();
    rebon_harness::kernel_bootstrap::render_plugin_registry_snapshot(
        &registry.snapshot(),
        &plane_contributions,
    )
}

fn render_kernel_list(agents: &SessionAgents<rebon_acp_client::AcpAgentBackend>) -> String {
    let current = agents.current_id();
    let mut out = String::from("Kernels for this session:\n");
    let marker = if current == LOCAL_AGENT_ID { "*" } else { " " };
    out.push_str(&format!("{marker} rebon — Rebon (local engine, native)\n"));
    for vendor in KNOWN_LOOP_VENDORS {
        let backend_id = loop_backend_id(vendor.id);
        let marker = if current.eq_ignore_ascii_case(&backend_id) {
            "*"
        } else {
            " "
        };
        let status = if !vendor.bundled {
            "assembly not bundled in this build yet".to_string()
        } else if agents.has_kernel_backend(&backend_id) {
            "configured".to_string()
        } else {
            format!("not configured (kernelPlugins.loops.{})", vendor.id)
        };
        out.push_str(&format!(
            "{marker} {} — {}  [{}]\n",
            vendor.id, vendor.label, status
        ));
    }
    out.push_str(&format!("\nHost: {}", describe_loop_host()));
    out.push_str("\nUse /kernel <name> to switch; /kernel rebon returns to the native engine.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use rebon_agent_core::{AgentBackend, LocalAgentBackend, StubPromptExecutor};
    use rebon_harness::agent_assembly::test_support::{session_agents, session_agents_declared};

    fn temp_root(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-cli-kernel-{name}-"))
            .tempdir()
            .expect("projects root")
    }

    #[test]
    fn kernel_plugins_lists_the_booted_core_plugins() {
        let registry = rebon_harness::kernel_bootstrap::process_plugin_registry();
        let text = rebon_harness::kernel_bootstrap::render_plugin_registry_snapshot(
            &registry.snapshot(),
            &rebon_harness::kernel_bootstrap::no_runtime_contributions,
        );
        for id in [
            "logger",
            "model-router",
            "config-seats",
            "core-commands",
            "core-tools",
        ] {
            assert!(
                text.contains(id),
                "{id} missing from:
{text}"
            );
        }
        assert!(text.contains("loaded"));
    }

    #[test]
    fn kernel_disable_refuses_a_core_plugin_and_unknown_ids() {
        let root = temp_root("kernel-registry");
        let agents = session_agents(root.path(), vec![]);
        let refused = handle_kernel_command(&agents, "disable core-tools", None);
        assert!(refused.is_err);
        assert!(
            refused.text.contains("cannot be disabled"),
            "{}",
            refused.text
        );
        let unknown = handle_kernel_command(&agents, "reload no-such-plugin", None);
        assert!(unknown.is_err);
        assert!(
            unknown.text.contains("no plugin definition"),
            "{}",
            unknown.text
        );
    }

    #[test]
    fn kernel_switch_to_rebon_lands_on_the_local_engine() {
        let root = temp_root("kernel-rebon");
        let agents = session_agents(root.path(), vec![]);
        let result = handle_kernel_command(&agents, "rebon", None);
        assert!(!result.is_err, "{}", result.text);
        assert_eq!(agents.current_id(), LOCAL_AGENT_ID);
    }

    #[test]
    fn kernel_switch_paths_answer_by_vendor_state() {
        let root = temp_root("kernel-states");
        let agents = session_agents(root.path(), vec![]);

        // Unknown vendor: the roster is named.
        let unknown = handle_kernel_command(&agents, "zsh", None);
        assert!(unknown.is_err);
        assert!(
            unknown.text.contains("known kernels: rebon, dsh"),
            "{}",
            unknown.text
        );

        // Seat without a bundled assembly: says so, does not switch.
        let unbundled = handle_kernel_command(&agents, "pi", None);
        assert!(unbundled.is_err);
        assert!(unbundled.text.contains("not bundled"), "{}", unbundled.text);
        assert_eq!(agents.current_id(), LOCAL_AGENT_ID);

        // Bundled but unconfigured: points at the config key.
        let unconfigured = handle_kernel_command(&agents, "dsh", None);
        assert!(unconfigured.is_err);
        assert!(
            unconfigured.text.contains("kernelPlugins.loops.dsh"),
            "{}",
            unconfigured.text
        );

        // Registered backend: the switch happens and the receipt names it.
        let local: Arc<dyn AgentBackend> =
            Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)));
        let agents = session_agents_declared(root.path(), vec![]).with_kernel_backend(
            "kernel:dsh",
            "dsh loop (deepseek-harness)",
            local,
        );
        let switched = handle_kernel_command(&agents, "dsh", None);
        assert!(!switched.is_err, "{}", switched.text);
        assert_eq!(agents.current_id(), "kernel:dsh");
        assert!(switched.text.contains("dsh loop"), "{}", switched.text);

        // The list marks the current row and shows every seat.
        let list = handle_kernel_command(&agents, "", None);
        assert!(list.text.contains("* dsh —"), "{}", list.text);
        assert!(list.text.contains("opencode"), "{}", list.text);
        assert!(list.text.contains("  rebon —"), "{}", list.text);
    }
}
