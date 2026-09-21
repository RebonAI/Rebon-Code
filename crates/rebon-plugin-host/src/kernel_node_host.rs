//! The host half of the `node-host` plugin
//! §8).
//!
//! `rebon-plugin-node-host` owns the part of the Node plugin plane that is
//! about *loading*: which entries should run, who switched one off, and what
//! stops when the plugin goes away. It cannot own the rest — starting a Node
//! process, reading `kernelPlugins`, turning a package's manifest into a load
//! request — because all of that needs the engine's tool catalog, and a plugin
//! crate that depended back on the plane's host would be a cycle.
//!
//! This module is the other side of that seam: an implementation of
//! [`PlaneHost`] over [`crate::plugin_boot`]'s plane, and the definition the
//! binary's plugin list carries.
//!
//! # What an entry's switch means
//!
//! Every composition entry gets a registry row named `node:<entry>`, whether
//! it came from `config.json` or from an installed package's
//! `capabilities.kernelPlugins`. The row's default is whether the
//! configuration lists it today, so a machine where nobody has touched a
//! switch runs exactly the composition it ran before. Flipping one settles the
//! plane through [`PluginPlane::reload`](crate::plugin_plane::PluginPlane::reload),
//! which restarts only what changed.
//!
//! An installed package the configuration never named has no `config` block
//! of its own, so it loads with none — its manifest supplies the root, the
//! entry module and the ceiling, which is everything a load request needs.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use rebon_kernel::{KernelError, Plugin, PluginDef, PluginHost};
use rebon_plugin_node_host::{AvailableEntry, NodeHostPlugin, PlaneHost};

use crate::plugin_composition::{plane_composition, CompositionRoots};
use crate::plugin_plane::ComposeEntry;

pub use rebon_plugin_node_host::{external_id, PLUGIN_ID};

fn make_node_host(host: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(NodeHostPlugin::new(Arc::new(HarnessPlaneHost {
        kernel: host.kernel.clone(),
    }))))
}

/// The row `builtin_plugin_defs` carries.
pub static PLUGIN: PluginDef = rebon_plugin_node_host::plugin_def(make_node_host);

/// The plane, as the plugin sees it.
struct HarnessPlaneHost {
    /// The kernel this plugin was applied to, kept because building a load
    /// request means asking an engine what tools it exposes, and that engine
    /// resolves them through a kernel context. Taken from the `PluginHost` the
    /// factory is handed rather than looked up: a plugin is applied *during*
    /// the boot, so the process slot is still empty when this is built.
    kernel: Arc<rebon_kernel::Kernel>,
}

impl PlaneHost for HarnessPlaneHost {
    fn available(&self) -> Vec<AvailableEntry> {
        available_entries()
    }

    fn reconcile(&self, wanted: &BTreeSet<String>) -> Result<(), String> {
        // Before anything else, and deliberately: this runs from a plugin's
        // `apply`, which the very first reconcile runs inside
        // `process_plugin_registry`'s initialisation. Reading the composition
        // needs an engine, building an engine needs `process_kernel`, and
        // that is the initialisation we are inside of. With no plane up there
        // is nothing to reconcile anyway — the boot reads the wanted set for
        // itself — so leaving early is both the cheap answer and the safe one.
        let running = crate::plugin_boot::running_plane();
        let (Some(plane), Some(runtime)) = (running.plane, running.runtime) else {
            return Ok(());
        };
        let entries = entries_for(self.kernel.context(), wanted)?;
        // Not `Handle::block_on` on this thread: a switch flipped from the
        // TUI arrives on a blocking worker where that is legal, and one
        // flipped from the app's IPC path arrives on a runtime worker where
        // it panics. A thread of its own may block from either.
        let outcome = std::thread::spawn(move || runtime.block_on(plane.reload(&entries))).join();
        match outcome {
            Ok(Ok(outcome)) => {
                if outcome.touched_anything() {
                    tracing::info!(
                        added = ?outcome.added,
                        removed = ?outcome.removed,
                        changed = ?outcome.changed,
                        "node-host: the composition was reconciled"
                    );
                }
                if outcome.failed.is_empty() {
                    Ok(())
                } else {
                    Err(outcome
                        .failed
                        .iter()
                        .map(|(id, why)| format!("{id}: {why}"))
                        .collect::<Vec<_>>()
                        .join("; "))
                }
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("the reconcile thread panicked".to_string()),
        }
    }

    fn shutdown(&self) {
        // Whatever the last boot decided stops being true the moment the plane
        // it decided about is gone.
        crate::plugin_boot::clear_composition_refusal();
        let running = crate::plugin_boot::take_running_plane();
        if let (Some(plane), Some(runtime)) = (running.plane, running.runtime) {
            let _ = std::thread::spawn(move || runtime.block_on(plane.shutdown())).join();
        }
        // After the host is down, not before: what the fork holds is what the
        // plane registered, and taking it away first would leave calls in
        // flight resolving to nothing.
        if let Some(ctx) = running.ctx {
            ctx.dispose();
        }
    }
}

/// Binds a client to a package's model provider, loading it on demand.
///
/// On demand, not at boot, and that is deliberate: before this the provider
/// was a child process spawned when it was selected, so a machine with a
/// provider package installed and unselected ran nothing. Loading every
/// installed provider into the host at boot would start a Node process for
/// people who never chose one. So the provider is a plugin on the shared host
/// that arrives when something first asks for it — which also means the host
/// itself has to be started here even when `kernelPlugins` configures no
/// composition at all.
pub async fn bind_plane_model_provider(
    plugins: &Arc<rebon_kernel::PluginRegistry>,
    contribution: &rebon_plugin_package::model_provider::PluginModelProviderContribution,
    connection: Option<rebon_api::model_provider_protocol::ProviderConnectionConfigV1>,
) -> anyhow::Result<rebon_provider::plane_model_provider::PlaneModelProviderClient> {
    use rebon_plugin_package::model_provider::MaterializedModelProviderTransport;

    let MaterializedModelProviderTransport::Plugin(transport) = &contribution.transport;
    let plane = crate::plugin_boot::ensure_plane_for_provider(plugins)
        .await
        .ok_or_else(|| {
            anyhow::anyhow!(match crate::plugin_boot::composition_refusal() {
                Some(refusal) => refusal.message(),
                None => "the plugin host did not start".to_string(),
            })
        })?;

    let entry = ComposeEntry {
        id: contribution.id.clone(),
        root: crate::plugin_manifests::plain_path(&transport.root),
        entry: transport.entry.clone(),
        // The user's connection settings ride each turn rather than the load,
        // because the same loaded adapter serves whichever provider entry is
        // selected right now. See `ModelProviderTurnV1`.
        config: serde_json::Value::Null,
        llm_providers: vec![contribution.id.clone()],
        ..ComposeEntry::default()
    };
    plane
        .load_standalone(&entry)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    rebon_provider::plane_model_provider::PlaneModelProviderClient::bind(
        rebon_provider::plane_model_provider::PlaneModelProviderClient::config_for(
            contribution,
            contribution.id.clone(),
            plane.workspace_root().to_string(),
            Arc::clone(plane.supervisor()),
            connection,
        ),
    )
    .await
}

/// Whether the external rows need deriving again.
///
/// Set at start, and again by anything that knows the answer changed. Without
/// a gate this would re-read the composition on every session assembly, which
/// is the startup path.
fn external_defs_stale() -> &'static std::sync::atomic::AtomicBool {
    static STALE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    &STALE
}

/// Say that what is installed, or which switches are set, may have changed.
pub fn mark_external_defs_stale() {
    external_defs_stale().store(true, std::sync::atomic::Ordering::Release);
    // The composition derives its entries from the same install state, and it
    // remembers its answer; a caller telling this side to look again is telling
    // that one too.
    rebon_plugin_package::discovery::invalidate();
}

/// `(len, mtime)` of the three files the rows are derived from.
///
/// An `installed.json` write does not go through `rebon-config`, so no
/// `ConfigChanged` announces `rebon plugin install`. Two or three `stat` calls
/// notice it anyway, which is the same thing `provider_runtime_cache` does and
/// costs about as much as deciding not to.
fn source_fingerprint() -> Vec<(u64, u64)> {
    let config_dir = rebon_config::config_home_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Where the two state files are is the package store's answer, not a path
    // spelled a second time here: a `stat` on a file the store no longer writes
    // is a fingerprint that never changes.
    let store = rebon_plugin_package::PluginStore::new(config_dir.clone(), cwd);
    [
        config_dir.join("config.json"),
        store.state_path(rebon_plugin_package::PluginScope::User),
        store.state_path(rebon_plugin_package::PluginScope::Project),
    ]
    .iter()
    .map(|path| {
        std::fs::metadata(path)
            .ok()
            .map(|meta| {
                let stamp = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|since| since.as_millis() as u64)
                    .unwrap_or_default();
                (meta.len(), stamp)
            })
            .unwrap_or((0, 0))
    })
    .collect()
}

/// Hand the registry the external rows this machine offers.
///
/// Called after the kernel has booted, never during: it resolves the
/// `node-host` service off the registry's kernel and writes the rows back into
/// that same registry, and the first reconcile is what building it consists of.
/// The registry is a parameter for exactly that reason — a lookup here would be
/// a lookup of the thing being built.
pub fn refresh_external_defs(registry: &Arc<rebon_kernel::PluginRegistry>) {
    static LAST: std::sync::Mutex<Option<Vec<(u64, u64)>>> = std::sync::Mutex::new(None);
    let fingerprint = source_fingerprint();
    {
        let mut last = LAST.lock().expect("node-host fingerprint poisoned");
        let unchanged = last.as_ref() == Some(&fingerprint);
        let forced = external_defs_stale().swap(false, std::sync::atomic::Ordering::AcqRel);
        if unchanged && !forced {
            return;
        }
        *last = Some(fingerprint);
    }
    let defs = match registry.kernel().context().get::<NodeHostServiceAlias>() {
        Some(host) => rebon_plugin_node_host::external_plugin_defs(&host.available()),
        // With the host switched off there is nothing for a `node:*` row to
        // inject, so listing them would only be a list of plugins that cannot
        // load. They come back when it does.
        None => Vec::new(),
    };
    let report = registry.set_external_defs(defs);
    if !report.failed.is_empty() {
        tracing::warn!(failed = ?report.failed, "node-host: some external plugins did not load");
    }
}

type NodeHostServiceAlias = rebon_plugin_node_host::NodeHostService;

/// Every entry this machine could run.
///
/// Reads the composition for its ids and nothing else, so it is handed an
/// empty tool list rather than the real one: `all_tools` only expands the
/// `$rebon/tools` sentinel inside a load request, and building the real list
/// means building an engine. This runs on the startup path; that does not.
pub fn available_entries() -> Vec<AvailableEntry> {
    let config_dir = rebon_config::config_home_dir();
    let configured = configured_entries(&[]).unwrap_or_default();
    let mut entries: Vec<AvailableEntry> = configured
        .iter()
        .map(|entry| AvailableEntry {
            id: entry.id.clone(),
            title: format!("Node plugin: {}", entry.id),
            configured: true,
        })
        .collect();
    let named: BTreeSet<&str> = configured.iter().map(|e| e.id.as_str()).collect();
    for (name, _, manifest) in crate::plugin_composition::installed_kernel_plugins(&config_dir) {
        if named.contains(name.as_str()) || manifest.entry.is_none() {
            continue;
        }
        entries.push(AvailableEntry {
            id: name.clone(),
            title: format!("Node plugin: {name} (installed)"),
            configured: false,
        });
    }
    entries
}

/// The ids `config.json` names today — the set that loads by itself.
pub fn configured_entry_ids() -> BTreeSet<String> {
    configured_entries(&[])
        .unwrap_or_default()
        .into_iter()
        .map(|entry| entry.id)
        .collect()
}

/// The load requests for `wanted`, configuration first and installed
/// packages after.
///
/// `upstream` is the kernel whose `tool-registry` seat the `$rebon/tools`
/// sentinel expands from.
pub fn entries_for(
    upstream: &rebon_kernel::Context,
    wanted: &BTreeSet<String>,
) -> Result<Vec<ComposeEntry>, String> {
    let config_dir = rebon_config::config_home_dir();
    let all_tools = crate::plugin_boot::exposed_tool_names(upstream);
    let mut entries = configured_entries(&all_tools)?;
    entries.retain(|entry| wanted.contains(&entry.id));
    entries.extend(unconfigured_entries(
        &config_dir,
        wanted,
        &entries,
        &all_tools,
    ));
    Ok(entries)
}

/// The entries an installed package declares and `wanted` asks for, that the
/// configuration says nothing about.
///
/// Their `config` is null: a package the user never wrote a block for gets
/// none, and everything else a load request needs — the package root, the
/// module, the ceiling — is in the manifest that declared it.
pub(crate) fn unconfigured_entries(
    config_dir: &Path,
    wanted: &BTreeSet<String>,
    already: &[ComposeEntry],
    all_tools: &[String],
) -> Vec<ComposeEntry> {
    let named: BTreeSet<&str> = already.iter().map(|e| e.id.as_str()).collect();
    crate::plugin_composition::installed_kernel_plugins(config_dir)
        .into_iter()
        .filter(|(name, _, _)| wanted.contains(name) && !named.contains(name.as_str()))
        .filter_map(|(name, root, manifest)| {
            let module = manifest.entry.clone()?;
            Some(crate::plugin_manifests::entry_for(
                &manifest,
                &name,
                root,
                module,
                serde_json::Value::Null,
                all_tools,
            ))
        })
        .collect()
}

/// `kernelPlugins` as load requests, before any switch is applied.
fn configured_entries(all_tools: &[String]) -> Result<Vec<ComposeEntry>, String> {
    let config_dir = rebon_config::config_home_dir();
    let scripts = crate::plugin_boot::plane_scripts()?;
    let composition = plane_composition(
        &config_dir,
        &CompositionRoots {
            payload: crate::plugin_composition::payload_root(&scripts.compose_root),
            runtime: scripts.compose_root.clone(),
        },
        all_tools,
    )?;
    for skipped in &composition.skipped {
        tracing::warn!(entry = %skipped, "composition entry not loaded");
    }
    Ok(composition.entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition the binary carries is the one the plugin crate
    /// describes, and it is a `Feature` — the plane can be turned off.
    #[test]
    fn the_definition_is_a_switchable_feature() {
        assert_eq!(PLUGIN.id, "node-host");
        assert_eq!(PLUGIN.kind, rebon_kernel::PluginKind::Feature);
        assert!(
            PLUGIN.default_enabled,
            "loading by default is what keeps a configured composition booting"
        );
    }

    /// An entry the configuration does not name is only built when a switch
    /// asks for it, and it is built from the manifest alone.
    #[test]
    fn an_unconfigured_package_is_built_only_when_wanted() {
        let dir = tempfile::tempdir().expect("config dir");
        let wanted = BTreeSet::from(["nothing-installed".to_string()]);
        let built = unconfigured_entries(dir.path(), &wanted, &[], &[]);
        assert!(
            built.is_empty(),
            "nothing is installed under a fresh config home"
        );
    }
}
