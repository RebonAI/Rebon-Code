//! The user's `kernelPlugins` section, as the plugin plane loads it.
//!
//! The config shape does not change — the same entry list, the same
//! `modules` map, the same `patches` layers, the same `web` section. What
//! changes is what rebon does with it: instead of handing the whole list to one
//! opaque runtime, rebon turns it into a *structure* (which entries are grouped,
//! which groups isolate a service) and a *load request per entry*, each carrying
//! the declarations its manifest allows.
//!
//! Where a declaration comes from:
//!
//! * a package rebon vendors → `runtimes/node/compose-runtime/payload-manifests.json`,
//!   because dsh packages have never heard of the plugin plane and rebon is the
//!   one that has to say what they do;
//! * an **installed** package that declares the name → its own
//!   `capabilities.kernelPlugins`, found through
//!   `rebon_plugin_package::discovery`. Installing it is enough; the name it
//!   declared is the name a composition uses.
//! * a package the user pointed `modules` at → the same field of the same file.
//!   An explicit path still wins, because pointing at a directory is a
//!   statement about *which* copy to load.
//!
//! An entry with neither is skipped and said out loud — with the reason, since
//! "no manifest", "a manifest declaring no plane plugins", and "a manifest that
//! declares some but not this module" are three different things to go fix.
//! Loading a plugin with no declared ceiling is the one thing this plane exists
//! to prevent, and guessing a ceiling for it would be the same thing with extra
//! steps.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use rebon_types::KernelPluginManifest;

use crate::plugin_manifests::{entry_for, plain_path, read_package_manifest, PayloadManifests};
use crate::plugin_plane::{ComposeEntry, ComposeNode};

/// A composition, ready to load.
#[derive(Clone, Debug, Default)]
pub struct PlaneComposition {
    /// Ids and placement — groups and isolated realms.
    pub structure: Vec<ComposeNode>,
    /// One load request per leaf, in load order.
    pub entries: Vec<ComposeEntry>,
    /// Extra bare-specifier mappings, for modules outside the payload.
    pub modules: BTreeMap<String, String>,
    /// The `web` configuration section.
    pub web: Value,
    /// Environment-variable credential references the user granted.
    pub credential_grants: Vec<String>,
    /// Entries that could not be turned into a load request, and why.
    pub skipped: Vec<String>,
}

/// Where the composition's packages live.
#[derive(Clone, Debug)]
pub struct CompositionRoots {
    /// The vendored JS payload.
    pub payload: PathBuf,
    /// The composition runtime package, which holds rebon's own modules.
    pub runtime: PathBuf,
}

/// Where the vendored JS payload is, as this side needs to name it.
///
/// The composition runtime resolves this for itself — `REBON_KERNEL_JS_DIR`,
/// then `payload/` beside the package — and rebon does not tell it. What rebon
/// needs the path for is different and smaller: a manifest's `entry` is
/// relative to a package, and a load request has to name that package. Same
/// ladder, read from this side.
///
/// The tree lives beside the runtime that reads it, not beside whatever
/// happens to ship it.
pub fn payload_root(compose_root: &Path) -> PathBuf {
    match std::env::var_os("REBON_KERNEL_JS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => compose_root.join("payload"),
    }
}

/// Reads `kernelPlugins` and resolves it against the manifests.
///
/// `all_tools` is what this build exposes to the plane, for the manifests that
/// declare the whole set rather than naming it.
pub fn plane_composition(
    config_dir: &Path,
    roots: &CompositionRoots,
    all_tools: &[String],
) -> Result<PlaneComposition, String> {
    let raw = std::fs::read(config_dir.join("config.json"))
        .map_err(|error| format!("config.json is unreadable: {error}"))?;
    let config: Value =
        serde_json::from_slice(&raw).map_err(|error| format!("config.json is invalid: {error}"))?;
    let Some(section) = config.get("kernelPlugins") else {
        return Ok(PlaneComposition::default());
    };
    let manifests = PayloadManifests::load(&roots.runtime)?;
    Ok(compose_section(
        section,
        roots,
        all_tools,
        &manifests,
        &installed_kernel_plugins(config_dir),
    ))
}

/// The kernel plugins the installed packages declare, in precedence order.
///
/// Best-effort: an unreadable install state must not stop a composition whose
/// entries all name vendored packages or explicit module paths. What it costs
/// is that an installed package will not be found by name, which the entry's
/// own skip message then says.
///
/// The project scope and the trust check both key off the process's working
/// directory, which is the project a session was started in — the same thing
/// every other project-scoped lookup uses.
pub fn installed_kernel_plugins(config_dir: &Path) -> Vec<(String, PathBuf, KernelPluginManifest)> {
    // Also runs on plane reloads mid-session, where a vanished cwd must not
    // fail the composition; the relative root then finds no project plugins.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let store = rebon_plugin_package::PluginStore::new(config_dir.to_path_buf(), cwd.clone());
    let trusted = rebon_config::is_directory_trusted_in(config_dir, &cwd);
    match rebon_plugin_package::discover(&store, &[], &cwd, trusted) {
        Ok(found) => {
            for warning in &found.warnings {
                tracing::warn!(%warning, "reading installed plugins for the plane");
            }
            found.kernel_plugins()
        }
        Err(error) => {
            tracing::warn!(%error, "could not read installed plugins; the plane will only see vendored packages and explicit modules");
            Vec::new()
        }
    }
}

/// The pure half, so the resolution rules are testable without a config file.
pub fn compose_section(
    section: &Value,
    roots: &CompositionRoots,
    all_tools: &[String],
    manifests: &PayloadManifests,
    installed: &[(String, PathBuf, KernelPluginManifest)],
) -> PlaneComposition {
    let base: Vec<Value> = section
        .get("plugins")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Layered patches keep working: they are a pure transform over the entry
    // list, and this reads the list they produce.
    let patches = section.get("patches").cloned().unwrap_or(Value::Null);
    let resolved =
        crate::compose_patches::resolve_composition(&base, &[("user-patches", &patches)]);
    for warning in &resolved.warnings {
        tracing::warn!(%warning, "kernelPlugins patch layer");
    }

    let modules: BTreeMap<String, PathBuf> = section
        .get("modules")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(name, path)| Some((name.clone(), PathBuf::from(path.as_str()?))))
                .collect()
        })
        .unwrap_or_default();

    let mut out = PlaneComposition {
        web: section.get("web").cloned().unwrap_or(Value::Null),
        credential_grants: section
            .get("credentialGrants")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        modules: modules
            .iter()
            .map(|(name, path)| (name.clone(), plain_path(path)))
            .collect(),
        ..PlaneComposition::default()
    };
    out.structure = walk(
        &resolved.entries,
        roots,
        all_tools,
        manifests,
        &modules,
        installed,
        &mut out,
    );
    out
}

fn walk(
    entries: &[Value],
    roots: &CompositionRoots,
    all_tools: &[String],
    manifests: &PayloadManifests,
    modules: &BTreeMap<String, PathBuf>,
    installed: &[(String, PathBuf, KernelPluginManifest)],
    out: &mut PlaneComposition,
) -> Vec<ComposeNode> {
    let mut nodes = Vec::new();
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .or_else(|| entry.get("name"))
            .and_then(Value::as_str)
        else {
            out.skipped
                .push("an entry with neither `id` nor `name` was skipped".to_owned());
            continue;
        };
        let isolate = entry.get("isolate").cloned();
        if let Some(group) = entry.get("group").and_then(Value::as_array) {
            // A group is a container, not a module: it becomes structure and
            // nothing is loaded for it.
            let children = walk(group, roots, all_tools, manifests, modules, installed, out);
            nodes.push(ComposeNode {
                id: id.to_owned(),
                isolate,
                group: Some(children),
            });
            continue;
        }
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned();
        let config = entry.get("config").cloned().unwrap_or(Value::Null);
        match load_request(
            id, &name, config, roots, all_tools, manifests, modules, installed,
        ) {
            Ok(request) => {
                out.entries.push(request);
                nodes.push(ComposeNode {
                    id: id.to_owned(),
                    isolate,
                    group: None,
                });
            }
            Err(reason) => {
                tracing::warn!(entry = id, %reason, "composition entry skipped");
                out.skipped.push(format!("{id}: {reason}"));
            }
        }
    }
    nodes
}

fn load_request(
    id: &str,
    name: &str,
    config: Value,
    roots: &CompositionRoots,
    all_tools: &[String],
    manifests: &PayloadManifests,
    modules: &BTreeMap<String, PathBuf>,
    installed: &[(String, PathBuf, KernelPluginManifest)],
) -> Result<ComposeEntry, String> {
    if let Some(manifest) = manifests.get(name) {
        let entry = manifest
            .entry
            .clone()
            .ok_or_else(|| format!("the manifest for {name} names no module"))?;
        // In rebon's own table, the package an entry belongs to *is* the
        // payload tree, so only `runtime` needs saying.
        let root = if PayloadManifests::root_is_runtime(manifest) {
            roots.runtime.clone()
        } else {
            roots.payload.clone()
        };
        return Ok(entry_for(manifest, id, root, entry, config, all_tools));
    }
    let Some(module) = modules.get(name) else {
        // Nothing pointed at a file, so the name has to have been declared by
        // something installed. This is what makes `rebon plugin install` enough:
        // the package says what it puts on the plane, and a composition names it.
        if let Some((_, root, manifest)) =
            installed.iter().find(|(declared, _, _)| declared == name)
        {
            let entry = manifest
                .entry
                .clone()
                .ok_or_else(|| format!("the installed declaration for {name} names no module"))?;
            return Ok(entry_for(
                manifest,
                id,
                root.clone(),
                entry,
                config,
                all_tools,
            ));
        }
        return Err(format!(
            "{name} is not a package rebon ships, not a name `kernelPlugins.modules` maps, and \
             not declared by any installed plugin"
        ));
    };
    let root = module
        .parent()
        .ok_or_else(|| format!("{} has no directory", module.display()))?
        .to_path_buf();
    let file = module
        .file_name()
        .ok_or_else(|| format!("{} is not a file", module.display()))?
        .to_string_lossy()
        .into_owned();
    // A package that declares nothing is not loaded declaring nothing: it would
    // start, be refused at every registration it attempts, and look like a bug
    // in the plugin rather than a missing manifest.
    let (declared_as, manifest) = read_package_manifest(module)
        .map_err(|problem| format!("{} — {problem}", plain_path(&root)))?;
    tracing::debug!(
        plugin = id,
        declaration = %declared_as,
        package = %root.display(),
        "composition entry loaded against its package's own declaration"
    );
    Ok(entry_for(&manifest, id, root, file, config, all_tools))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> CompositionRoots {
        CompositionRoots {
            payload: PathBuf::from("/payload"),
            runtime: PathBuf::from("/runtime"),
        }
    }

    fn manifests() -> PayloadManifests {
        PayloadManifests::load(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/compose-runtime"),
        )
        .expect("the shipped manifest table parses")
    }

    #[test]
    fn a_vendored_package_takes_its_declarations_from_the_table() {
        let section = serde_json::json!({
            "plugins": [{ "id": "llm", "name": "@deepseek-ai/dsh-llm-deepseek", "config": { "a": 1 } }]
        });
        let out = compose_section(&section, &roots(), &[], &manifests(), &[]);

        assert_eq!(out.entries.len(), 1);
        let entry = &out.entries[0];
        assert_eq!(entry.id, "llm");
        assert_eq!(entry.entry, "vendor/dsh/llm-deepseek.js");
        assert_eq!(entry.llm_providers, vec!["deepseek-official"]);
        assert_eq!(entry.seats, vec!["credentials"]);
        assert_eq!(entry.config, serde_json::json!({ "a": 1 }));
        assert_eq!(out.structure.len(), 1);
    }

    fn installed(name: &str, root: &str, entry: &str) -> (String, PathBuf, KernelPluginManifest) {
        (
            name.to_string(),
            PathBuf::from(root),
            KernelPluginManifest {
                entry: Some(entry.to_string()),
                services: vec!["echo".to_string()],
                ..KernelPluginManifest::default()
            },
        )
    }

    /// What installing a plugin is supposed to buy: the composition names it,
    /// and nothing else has to be written down.
    #[test]
    fn an_installed_package_is_named_directly_with_no_modules_mapping() {
        let section = serde_json::json!({
            "plugins": [{ "id": "demo", "name": "demo-plane", "config": { "a": 1 } }]
        });
        let out = compose_section(
            &section,
            &roots(),
            &[],
            &manifests(),
            &[installed("demo-plane", "/packages/demo", "index.mjs")],
        );

        assert_eq!(out.skipped, Vec::<String>::new());
        assert_eq!(out.entries.len(), 1);
        let entry = &out.entries[0];
        assert_eq!(entry.id, "demo");
        assert_eq!(entry.root, "/packages/demo");
        assert_eq!(entry.entry, "index.mjs");
        assert_eq!(entry.services, vec!["echo".to_string()]);
        assert_eq!(entry.config, serde_json::json!({ "a": 1 }));
    }

    /// Pointing `modules` at a directory is a statement about *which* copy to
    /// load, so it outranks whatever happens to be installed under that name.
    #[test]
    fn an_explicit_module_path_wins_over_an_installed_declaration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rebon-plugin.json"),
            br#"{"name":"local","version":"0.0.1","capabilities":{"kernelPlugins":{
                 "demo-plane": { "entry": "local.mjs", "services": ["from-disk"] }}}}"#,
        )
        .unwrap();
        let module = dir.path().join("local.mjs");
        std::fs::write(&module, b"export function activate() {}").unwrap();

        let section = serde_json::json!({
            "plugins": [{ "id": "demo", "name": "demo-plane" }],
            "modules": { "demo-plane": module.to_string_lossy() }
        });
        let out = compose_section(
            &section,
            &roots(),
            &[],
            &manifests(),
            &[installed("demo-plane", "/packages/demo", "index.mjs")],
        );

        assert_eq!(out.entries.len(), 1, "{:?}", out.skipped);
        assert_eq!(out.entries[0].entry, "local.mjs");
        assert_eq!(out.entries[0].services, vec!["from-disk".to_string()]);
    }

    /// A name nothing accounts for names all three places it could have come
    /// from, because which one is missing decides what to go fix.
    #[test]
    fn a_name_nothing_declares_is_skipped_and_says_where_it_looked() {
        let section = serde_json::json!({ "plugins": [{ "id": "x", "name": "nowhere" }] });
        let out = compose_section(&section, &roots(), &[], &manifests(), &[]);

        assert!(out.entries.is_empty());
        assert_eq!(out.skipped.len(), 1);
        let reason = &out.skipped[0];
        assert!(reason.contains("nowhere"), "{reason}");
        assert!(reason.contains("modules"), "{reason}");
        assert!(reason.contains("installed"), "{reason}");
    }

    #[test]
    fn the_whole_tool_set_sentinel_expands_to_what_this_build_exposes() {
        let section = serde_json::json!({
            "plugins": [{ "id": "loop", "name": "@deepseek-ai/dsh-agent-loop" }]
        });
        let tools = vec!["Read".to_string(), "Write".to_string()];
        let out = compose_section(&section, &roots(), &tools, &manifests(), &[]);
        assert_eq!(out.entries[0].invokable_tools, tools);
    }

    #[test]
    fn a_group_is_structure_and_loads_nothing_of_its_own() {
        let section = serde_json::json!({
            "plugins": [{
                "id": "loop",
                "isolate": { "systemPrompt": "loop" },
                "group": [{ "id": "sessions", "name": "@deepseek-ai/dsh-session" }],
            }]
        });
        let out = compose_section(&section, &roots(), &[], &manifests(), &[]);

        assert_eq!(out.entries.len(), 1, "only the leaf loads");
        assert_eq!(out.entries[0].id, "sessions");
        assert_eq!(out.structure.len(), 1);
        assert_eq!(out.structure[0].id, "loop");
        assert!(out.structure[0].isolate.is_some());
        assert_eq!(out.structure[0].group.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn an_entry_with_no_manifest_anywhere_is_skipped_by_name() {
        let section = serde_json::json!({ "plugins": [{ "id": "mystery", "name": "who-knows" }] });
        let out = compose_section(&section, &roots(), &[], &manifests(), &[]);

        assert!(out.entries.is_empty());
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].contains("mystery"), "{:?}", out.skipped);
    }

    #[test]
    fn patches_still_apply_before_anything_is_resolved() {
        let section = serde_json::json!({
            "plugins": [{ "id": "llm", "name": "@deepseek-ai/dsh-llm-deepseek" }],
            "patches": [{ "remove": "llm" }],
        });
        let out = compose_section(&section, &roots(), &[], &manifests(), &[]);
        assert!(out.entries.is_empty(), "the patch layer removed the entry");
    }
}
