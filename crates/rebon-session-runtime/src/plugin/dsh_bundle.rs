//! A DeepSeek Harness *bundle*: several npm packages installed and run as one.
//!
//! A dsh package rarely works alone. `@deepseek-ai/dsh-tool-fs` registers the
//! model's `read`/`write`/`edit` tools and injects the `fs` service, which
//! `@deepseek-ai/dsh-fs-local` provides; the two reach each other through a
//! Cordis realm they share. dsh says which packages go together with a
//! *bundle*: a package whose `package.json` names a patch
//! (`"dsh": { "bundle": { "patch": "./cordis.patch.yml" } }`) inserting one
//! row per plugin — `dsh-base` is one. Rebon installs such a package as one
//! rebon package:
//!
//! 1. its dependencies, and every peer they declare that rebon does not own,
//!    go into one `node_modules` (`verve::npm`, no scripts), so the seams the
//!    members share are one copy;
//! 2. the probe mounts every row in one realm, in the order their injections
//!    allow, and says what each registered, which realm services each
//!    provided, and why any row could not mount;
//! 3. each mounted row becomes a `kernelPlugins` entry of the one manifest,
//!    with its module under `node_modules`, its row config as its default
//!    config, and its `inject`/`provides` so the composition keeps the order.
//!
//! One package means one container, so the members share a host and a realm
//! the way they shared one in dsh. Rows that do not mount are named in the
//! result and left out; a bundle none of whose rows mount is refused.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use rebon_harness::rebon_plugin_package::container::ContainerRequest;
use serde_json::{json, Value};

use super::dsh_npm::{
    fetches_any_url, package_sources, plugin_id_of, provided_modules, run_probe, scan_requests,
    seats_for, Adapted,
};

/// How many times peers are added and the tree installed again before giving
/// up: each round adds the peers the previous round's packages declared, and
/// dsh's own trees settle in two or three.
const PEER_ROUNDS: usize = 8;

/// Whether a `package.json` is a dsh bundle.
pub fn is_bundle(package: &Value) -> bool {
    package
        .get("dsh")
        .and_then(|dsh| dsh.get("bundle"))
        .is_some_and(Value::is_object)
}

/// One row the probe mounted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BundleEntry {
    pub id: String,
    pub package: String,
    pub tools: Vec<String>,
}

/// One row the probe could not mount, and why.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkippedRow {
    pub id: String,
    pub package: String,
    pub code: String,
    pub reason: String,
}

/// The bundle's `package.json` as rebon installs it: what the members need to
/// run, and nothing that runs at install.
pub fn slim_bundle(package: &Value) -> Value {
    let mut slim = serde_json::Map::new();
    for key in ["name", "version", "type", "description", "license", "dsh"] {
        if let Some(value) = package.get(key) {
            slim.insert(key.to_owned(), value.clone());
        }
    }
    slim.insert(
        "dependencies".to_owned(),
        package
            .get("dependencies")
            .cloned()
            .unwrap_or_else(|| json!({})),
    );
    Value::Object(slim)
}

/// The peers the installed tree declares and nobody installed: what the next
/// round adds to the bundle's own dependencies.
///
/// A peer rebon owns is rebon's to answer (`provided`); an optional peer
/// (`peerDependenciesMeta`) is the declarer's to do without. Two declarers
/// naming different ranges get the first one's, by package name, so the
/// same tree always asks for the same thing.
pub fn missing_peers(root: &Path, provided: &BTreeSet<String>) -> BTreeMap<String, String> {
    let modules = root.join("node_modules");
    let mut installed = BTreeSet::new();
    let mut wanted = BTreeMap::new();
    for manifest in installed_manifests(&modules) {
        if let Some(name) = manifest.get("name").and_then(Value::as_str) {
            installed.insert(name.to_owned());
        }
        let optional = |peer: &str| {
            manifest
                .get("peerDependenciesMeta")
                .and_then(|meta| meta.get(peer))
                .and_then(|meta| meta.get("optional"))
                .and_then(Value::as_bool)
                == Some(true)
        };
        for (peer, range) in manifest
            .get("peerDependencies")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if provided.contains(peer) || optional(peer) {
                continue;
            }
            if let Some(range) = range.as_str() {
                wanted
                    .entry(peer.clone())
                    .or_insert_with(|| range.to_owned());
            }
        }
    }
    wanted.retain(|peer, _| !installed.contains(peer));
    wanted
}

/// Every `package.json` directly under `node_modules`, scopes included —
/// hoisted, so the tree's members are its top level — by package name.
fn installed_manifests(modules: &Path) -> Vec<Value> {
    let read = |dir: &Path| -> Option<Value> {
        serde_json::from_slice(&std::fs::read(dir.join("package.json")).ok()?).ok()
    };
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(modules) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !path.is_dir() {
            continue;
        }
        if name.starts_with('@') {
            for scoped in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                if let Some(manifest) = read(&scoped.path()) {
                    out.push(manifest);
                }
            }
        } else if let Some(manifest) = read(&path) {
            out.push(manifest);
        }
    }
    let name = |manifest: &Value| {
        manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    out.sort_by_key(name);
    out
}

/// Installs the bundle's tree: its dependencies, then the peers they declare,
/// round by round until nothing is missing.
fn install_tree(dir: &Path, id: &str, provided: &BTreeSet<String>) -> anyhow::Result<()> {
    let options = verve::npm::InstallOptions {
        write_lockfile: false,
        ..Default::default()
    };
    let package_path = dir.join("package.json");
    for _ in 0..PEER_ROUNDS {
        verve::npm::install(dir, &options)
            .map_err(|error| anyhow!("{id}: could not install what it needs: {error}"))?;
        let missing = missing_peers(dir, provided);
        if missing.is_empty() {
            return Ok(());
        }
        let mut package: Value = serde_json::from_slice(&std::fs::read(&package_path)?)?;
        let dependencies = package["dependencies"]
            .as_object_mut()
            .ok_or_else(|| anyhow!("{id}: the bundle's dependencies are not an object"))?;
        for (peer, range) in missing {
            dependencies.insert(peer, Value::String(range));
        }
        std::fs::write(&package_path, serde_json::to_vec_pretty(&package)?)?;
    }
    bail!("{id}: its peers did not settle in {PEER_ROUNDS} installs")
}

/// The rebon manifest for a probed bundle.
pub fn bundle_manifest(
    package: &Value,
    probe: &Value,
    request: &ContainerRequest,
) -> anyhow::Result<Value> {
    let id = plugin_id_of(package)?;
    let mut kernel_plugins = serde_json::Map::new();
    let mut configs = serde_json::Map::new();
    let entries = probe
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // A row that starts programs reaches the `process` seat through the
    // realm's subprocess seam whichever row's call it happens in — the tool's
    // or the executor's — so the grant is the bundle's, on every row.
    let starts_programs = entries.iter().any(|entry| {
        ["required", "optional"].iter().any(|key| {
            entry["inject"][key]
                .as_array()
                .is_some_and(|names| names.iter().any(|name| name == "subprocess"))
        })
    });
    for entry in &entries {
        let Some(row) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let list = |key: &str| -> Vec<Value> {
            entry
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let names = |key: &str| -> Vec<Value> {
            list(key)
                .iter()
                .filter_map(|item| item.get("name").cloned())
                .collect()
        };
        let required = entry
            .get("inject")
            .and_then(|inject| inject.get("required"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        let uses_web = ["required", "optional"].iter().any(|key| {
            entry
                .get("inject")
                .and_then(|inject| inject.get(key))
                .and_then(Value::as_array)
                .is_some_and(|names| names.iter().any(|name| name == "web"))
        });
        let mut kernel = json!({
            "entry": entry.get("entry").cloned().unwrap_or(Value::Null),
            "tools": names("tools"),
            "commands": names("commands"),
            "services": list("services"),
            "llmProviders": list("llmProviders"),
            "eventTopics": list("eventTopics"),
            "seats": seats_for(starts_programs),
            // See `dsh_npm::manifest_for`: the web seat falls back to rebon's
            // own search and fetch.
            "invokableTools": if uses_web { json!(["WebSearch", "WebFetch"]) } else { json!([]) },
            "inject": required,
            "provides": list("provides"),
        });
        if entry
            .get("report")
            .and_then(|report| report.get("sections"))
            .and_then(Value::as_array)
            .is_some_and(|sections| !sections.is_empty())
        {
            kernel["publishedTopics"] = json!([]);
        }
        kernel_plugins.insert(row.to_owned(), kernel);
        if let Some(config) = entry.get("config").filter(|config| !config.is_null()) {
            configs.insert(row.to_owned(), config.clone());
        }
    }
    let skipped: Vec<Value> = probe
        .get("skipped")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut metadata = json!({
        "upstream": package.get("name").cloned().unwrap_or(Value::Null),
        "upstreamVersion": package.get("version").cloned().unwrap_or(Value::Null),
        "adaptedFrom": "npm",
        "bundle": {
            "rows": kernel_plugins.keys().collect::<Vec<_>>(),
            "skipped": skipped,
        },
    });
    if !configs.is_empty() {
        metadata["kernelPluginConfig"] = Value::Object(configs);
    }
    let mut manifest = json!({
        "name": id,
        "version": package.get("version").and_then(Value::as_str).unwrap_or("0.0.0"),
        "description": package.get("description").cloned().unwrap_or(Value::Null),
        "capabilities": { "kernelPlugins": kernel_plugins },
        "metadata": metadata,
    });
    if !request.is_empty() {
        manifest["container"] = serde_json::to_value(request)?;
    }
    Ok(manifest)
}

/// What the probe's `skipped` list says, typed.
pub fn skipped_rows(probe: &Value) -> Vec<SkippedRow> {
    probe
        .get("skipped")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            let text = |key: &str| {
                row.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            SkippedRow {
                id: text("id"),
                package: text("name"),
                code: text("code"),
                reason: text("error"),
            }
        })
        .collect()
}

/// Makes the bundle in `dir` a rebon package, in place.
///
/// `overrides` replaces row configs by row id, as a marketplace entry's
/// `rebonConfig` supplies them.
pub fn adapt_bundle_with(
    dir: &Path,
    package: &Value,
    overrides: Option<&Value>,
    runner: &Path,
    compose_root: &Path,
) -> anyhow::Result<Adapted> {
    let id = plugin_id_of(package)?;
    let provided = provided_modules(compose_root)?;
    std::fs::write(
        dir.join("package.json"),
        serde_json::to_vec_pretty(&slim_bundle(package))?,
    )?;
    install_tree(dir, &id, &provided)?;

    let real = rebon_plugin_host::container::real_path;
    let (compose_root, bundle_root) = (real(compose_root), real(dir));
    let mut args = vec!["--bundle".into(), bundle_root.clone().into_os_string()];
    if let Some(overrides) = overrides {
        args.push(overrides.to_string().into());
    }
    let answer = run_probe(&id, runner, &compose_root, &bundle_root, &args)?;
    let skipped = skipped_rows(&answer);
    if answer.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = answer
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                skipped
                    .iter()
                    .map(|row| format!("{} ({}): {}", row.id, row.code, row.reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            });
        bail!("{id}: none of its rows run on rebon: {error}");
    }
    let entries: Vec<BundleEntry> = answer
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| BundleEntry {
            id: entry["id"].as_str().unwrap_or_default().to_owned(),
            package: entry["name"].as_str().unwrap_or_default().to_owned(),
            tools: entry["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect(),
        })
        .collect();
    // What the members' own code reaches — not their dependencies'.
    let mut sources = Vec::new();
    for entry in &entries {
        let member = dir
            .join("node_modules")
            .join(entry.package.replace('/', std::path::MAIN_SEPARATOR_STR));
        sources.extend(package_sources(&member));
    }
    let mut request = scan_requests(&sources);
    if answer
        .get("entries")
        .and_then(Value::as_array)
        .is_some_and(|entries| entries.iter().any(fetches_any_url))
    {
        request.network = vec![rebon_harness::rebon_plugin_package::container::ANY_HOST.to_owned()];
    }
    let manifest = bundle_manifest(package, &answer, &request)?;
    std::fs::write(
        dir.join("rebon-plugin.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .with_context(|| format!("writing {id}'s manifest"))?;
    for row in &skipped {
        tracing::info!(bundle = %id, row = %row.id, code = %row.code, reason = %row.reason, "a bundle row does not run on rebon");
    }
    Ok(Adapted {
        plugin_id: id,
        request,
        tools: entries
            .iter()
            .flat_map(|entry| entry.tools.clone())
            .collect(),
        skills: Vec::new(),
        rows: entries.iter().map(|entry| entry.id.clone()).collect(),
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dsh's own base profile, `@deepseek-ai/dsh-base`, as rebon installs a
    /// bundle: which of its rows run on rebon, and why the rest do not.
    ///
    /// It is dsh's engine as much as a plugin set — the agent loop, sessions,
    /// the API gateway, telemetry — and rebon *is* the engine, so most of it
    /// is not meant to run here. What this holds is that the rows that are
    /// plugins on an engine do, and that every other row is refused by name.
    /// (Its `settings` row is one: its `disabled` is a Loader expression,
    /// `!ctx.get('profileContext')`, which rebon does not evaluate.)
    /// Needs the network and the runner (see `dsh_npm`'s registry test), so
    /// it is run by hand.
    #[test]
    #[ignore = "fetches from registry.npmjs.org"]
    fn the_dsh_base_profile_runs_the_rows_that_are_plugins() {
        let runner = std::env::var_os("REBON_VERVE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let exe = std::env::current_exe().unwrap();
                let name = if cfg!(windows) {
                    "rebon-verve.exe"
                } else {
                    "rebon-verve"
                };
                exe.parent().unwrap().parent().unwrap().join(name)
            });
        let compose_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../runtimes/node/compose-runtime");
        let scratch = tempfile::tempdir().unwrap();
        let packed = verve::npm::pack(
            "@deepseek-ai/dsh-base@0.2.1-rc.3",
            &verve::npm::InstallOptions::default(),
            scratch.path(),
        )
        .unwrap();
        let dir = crate::plugin::package::unpack_archive_folder(
            &packed.tarball,
            &scratch.path().join("unpacked"),
            crate::plugin::package::PackageLimits::default(),
        )
        .unwrap();
        let package: Value =
            serde_json::from_slice(&std::fs::read(dir.join("package.json")).unwrap()).unwrap();
        assert!(is_bundle(&package));
        let adapted = adapt_bundle_with(&dir, &package, None, &runner, &compose_root)
            .unwrap_or_else(|error| panic!("dsh-base adapts: {error:#}"));
        let mut skipped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in &adapted.skipped {
            skipped
                .entry(row.code.clone())
                .or_default()
                .push(format!("{}: {}", row.id, row.reason));
        }
        eprintln!("runs: {:?}", adapted.rows);
        for (code, rows) in &skipped {
            eprintln!("{code} ({}):", rows.len());
            for row in rows {
                eprintln!("  {row}");
            }
        }
        for row in [
            "skill",
            "skill-filesystem",
            "storage",
            "web-search-deepseek",
            "web-fetch-http",
            "tool-web",
        ] {
            assert!(
                adapted.rows.iter().any(|ran| ran == row),
                "{row} runs: {:?}",
                adapted.rows
            );
        }
        let refused: Vec<&str> = adapted.skipped.iter().map(|row| row.id.as_str()).collect();
        for row in ["agent", "agent-loop"] {
            assert!(refused.contains(&row), "{row} is dsh's engine: {refused:?}");
        }
        // dsh's own services for the seats rebon implements are rebon's.
        for row in ["llm", "tools", "web", "system-prompt", "commands"] {
            assert!(
                adapted
                    .skipped
                    .iter()
                    .any(|skip| skip.id == row && skip.code == "[REBON_PROVIDES]"),
                "{row}: {:?}",
                adapted.skipped
            );
        }
        assert!(adapted.skipped.iter().all(|row| !row.reason.is_empty()));
    }

    #[test]
    fn a_bundle_is_a_package_naming_a_dsh_bundle() {
        assert!(is_bundle(
            &json!({ "dsh": { "bundle": { "patch": "./cordis.patch.yml" } } })
        ));
        assert!(!is_bundle(&json!({ "dsh": { "client": {} } })));
        assert!(!is_bundle(&json!({ "name": "@deepseek-ai/dsh-tool-fs" })));
    }

    #[test]
    fn the_slim_bundle_keeps_its_rows_and_drops_what_runs() {
        let slim = slim_bundle(&json!({
            "name": "files", "version": "1.0.0", "type": "module",
            "dsh": { "bundle": { "patch": "./cordis.patch.yml" } },
            "dependencies": { "@deepseek-ai/dsh-tool-fs": "0.2.1-rc.3" },
            "peerDependencies": { "@deepseek-ai/cordis": "~4.0.5" },
            "scripts": { "postinstall": "evil" },
            "main": "lib/index.js",
        }));
        assert_eq!(
            slim,
            json!({
                "name": "files", "version": "1.0.0", "type": "module",
                "dsh": { "bundle": { "patch": "./cordis.patch.yml" } },
                "dependencies": { "@deepseek-ai/dsh-tool-fs": "0.2.1-rc.3" },
            })
        );
    }

    fn write_package(root: &Path, name: &str, manifest: Value) {
        let dir = root
            .join("node_modules")
            .join(name.replace('/', std::path::MAIN_SEPARATOR_STR));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
    }

    /// A peer nobody installed is wanted once, at the range its first
    /// declarer by name named; rebon's own modules, optional peers and peers
    /// already in the tree are not.
    #[test]
    fn the_missing_peers_are_the_ones_nobody_installed() {
        let root = tempfile::tempdir().unwrap();
        write_package(
            root.path(),
            "@deepseek-ai/dsh-tool-fs",
            json!({
                "name": "@deepseek-ai/dsh-tool-fs",
                "peerDependencies": {
                    "@deepseek-ai/dsh-fs": "0.2.1-rc.3",
                    "@deepseek-ai/cordis": "~4.0.5",
                    "@deepseek-ai/dsh-sandbox": "0.2.1-rc.3",
                    "@deepseek-ai/dsh-attachment": "0.2.1-rc.3",
                },
                "peerDependenciesMeta": { "@deepseek-ai/dsh-attachment": { "optional": true } },
            }),
        );
        write_package(
            root.path(),
            "@deepseek-ai/dsh-fs-local",
            json!({
                "name": "@deepseek-ai/dsh-fs-local",
                "peerDependencies": { "@deepseek-ai/dsh-fs": "^0.2.0", "@deepseek-ai/dsh-sandbox": "x" },
            }),
        );
        write_package(
            root.path(),
            "@deepseek-ai/dsh-sandbox",
            json!({ "name": "@deepseek-ai/dsh-sandbox" }),
        );
        let provided: BTreeSet<String> = ["@deepseek-ai/cordis".to_owned()].into();
        let missing = missing_peers(root.path(), &provided);
        assert_eq!(
            missing,
            BTreeMap::from([("@deepseek-ai/dsh-fs".to_owned(), "^0.2.0".to_owned())])
        );
    }

    /// A bundle with a row that injects `subprocess` asks, on every row,
    /// for the seat that starts programs.
    #[test]
    fn a_bundle_that_starts_programs_asks_for_the_process_seat() {
        let row = |id: &str, required: &[&str]| {
            json!({ "id": id, "name": format!("@deepseek-ai/dsh-{id}"), "entry": "x.js",
                    "inject": { "required": required, "optional": [] }, "provides": [] })
        };
        let probe = json!({ "ok": true, "entries": [
            row("pwsh-local", &["subprocess"]),
            row("tool-pwsh", &["tools", "shell"]),
        ] });
        let manifest = bundle_manifest(
            &json!({ "name": "shell", "version": "1.0.0" }),
            &probe,
            &ContainerRequest::default(),
        )
        .unwrap();
        for id in ["pwsh-local", "tool-pwsh"] {
            assert_eq!(
                manifest["capabilities"]["kernelPlugins"][id]["seats"],
                json!(["credentials", "logger", "settings", "process"]),
                "{id}"
            );
        }
    }

    #[test]
    fn the_manifest_has_one_kernel_plugin_per_mounted_row() {
        let package = json!({ "name": "dsh-files", "version": "1.0.0", "description": "files" });
        let probe = json!({
            "ok": true,
            "entries": [
                {
                    "id": "fs-local", "name": "@deepseek-ai/dsh-fs-local",
                    "entry": "node_modules/@deepseek-ai/dsh-fs-local/lib/index.js",
                    "config": null, "inject": { "required": [], "optional": [] },
                    "provides": ["fs"], "tools": [], "services": [], "llmProviders": [],
                    "eventTopics": [], "commands": [], "report": { "sections": [] },
                },
                {
                    "id": "tool-fs", "name": "@deepseek-ai/dsh-tool-fs",
                    "entry": "node_modules/@deepseek-ai/dsh-tool-fs/lib/index.js",
                    "config": { "readImage": false },
                    "inject": { "required": ["tools", "fs"], "optional": ["web"] },
                    "provides": [], "tools": [{ "name": "read" }, { "name": "write" }],
                    "services": [], "llmProviders": [], "eventTopics": [], "commands": [],
                    "report": { "sections": [{ "name": "tool:read" }] },
                },
            ],
            "skipped": [{ "id": "agent", "name": "@deepseek-ai/dsh-agent", "code": "[MISSING_INJECT]", "error": "needs agents" }],
        });
        let manifest = bundle_manifest(&package, &probe, &ContainerRequest::default()).unwrap();
        let kernel = &manifest["capabilities"]["kernelPlugins"];
        assert_eq!(kernel["fs-local"]["provides"], json!(["fs"]));
        assert_eq!(kernel["tool-fs"]["inject"], json!(["tools", "fs"]));
        assert_eq!(kernel["tool-fs"]["tools"], json!(["read", "write"]));
        assert_eq!(
            kernel["tool-fs"]["invokableTools"],
            json!(["WebSearch", "WebFetch"])
        );
        assert_eq!(kernel["tool-fs"]["publishedTopics"], json!([]));
        assert_eq!(
            kernel["tool-fs"]["entry"],
            "node_modules/@deepseek-ai/dsh-tool-fs/lib/index.js"
        );
        assert_eq!(
            manifest["metadata"]["kernelPluginConfig"],
            json!({ "tool-fs": { "readImage": false } })
        );
        assert_eq!(manifest["metadata"]["bundle"]["skipped"][0]["id"], "agent");
        assert!(
            !kernel["fs-local"]["seats"].to_string().contains("process"),
            "nothing here starts a program"
        );
        let parsed: rebon_harness::rebon_plugin_package::PluginManifest =
            serde_json::from_value(manifest).unwrap();
        assert_eq!(parsed.capabilities.kernel_plugins.len(), 2);
        assert_eq!(
            parsed.capabilities.kernel_plugins["tool-fs"].inject,
            vec!["tools".to_owned(), "fs".to_owned()]
        );
    }
}
