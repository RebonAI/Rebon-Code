//! What a plugin is allowed to do, before it is loaded.
//!
//! Every plugin on the plane loads against a manifest: the ceiling on what it
//! may register and what it may reach. The declaration itself is
//! [`KernelPluginManifest`], which lives in `rebon-types` because it has two
//! readers — `rebon plugin install` validates it, and this module turns it into
//! a load request.
//!
//! # Where a declaration comes from
//!
//! A third-party package carries its own `rebon-plugin.json`, and its
//! declarations sit under `capabilities.kernelPlugins` beside every other
//! capability the package offers. That file has **one** schema: the package
//! manifest. Reading it any other way is what let an installed package look
//! undeclared to this side while looking perfectly valid to the installer.
//!
//! The packages rebon vendors into its own payload have no such file — they are
//! dsh packages, and dsh has never heard of the plugin plane — so rebon
//! declares on their behalf, in one table a person can read the same way:
//! `runtimes/node/compose-runtime/payload-manifests.json`. Same values, different place.
//!
//! The table is data rather than code for the same reason the manifest is: the
//! answer to "what can this plugin do" should be readable without building
//! anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rebon_plugin_package::PluginManifest;
use rebon_types::{KernelPluginManifest, KernelPluginRoot};
use serde::Deserialize;
use serde_json::Value;

use crate::plugin_plane::ComposeEntry;

/// The manifest file a package carries, by the only name it goes by.
///
/// Re-exported rather than spelled again: `rebon-plugin-package` owns the file
/// name along with the schema inside it, and a second `const` here is a second
/// answer waiting to drift.
pub use rebon_plugin_package::PLUGIN_MANIFEST_FILE;

/// Turns a manifest plus one composition entry's configuration into the load
/// request rebon will send.
pub fn entry_for(
    manifest: &KernelPluginManifest,
    id: &str,
    root: PathBuf,
    entry: String,
    config: Value,
    all_tools: &[String],
) -> ComposeEntry {
    ComposeEntry {
        id: id.to_owned(),
        root: plain_path(&root),
        entry,
        config,
        services: manifest.services.clone(),
        event_topics: manifest.event_topics.clone(),
        published_topics: manifest.published_topics.clone(),
        llm_providers: manifest.llm_providers.clone(),
        tools: manifest.tools.clone(),
        commands: manifest.commands.clone(),
        invokable_tools: manifest.invokable(all_tools),
        seats: manifest.seats.clone(),
        settings: manifest.settings.clone(),
        publish: true,
    }
}

/// A path as the protocol accepts it: no Windows verbatim prefix, forward
/// slashes. Every one of these crosses to Node as a string.
pub fn plain_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    #[allow(dead_code)]
    version: u32,
    packages: BTreeMap<String, KernelPluginManifest>,
}

/// The table of what rebon declares for the packages it ships.
#[derive(Clone, Debug, Default)]
pub struct PayloadManifests {
    packages: BTreeMap<String, KernelPluginManifest>,
}

impl PayloadManifests {
    /// Reads the table beside the composition runtime.
    ///
    /// A missing or broken table is not survivable: without it rebon cannot say
    /// what any vendored package may do, and loading them undeclared would be
    /// the one thing the plane exists to prevent.
    pub fn load(compose_root: &Path) -> Result<Self, String> {
        let file = compose_root.join("payload-manifests.json");
        let raw = std::fs::read(&file)
            .map_err(|error| format!("{} is unreadable: {error}", file.display()))?;
        let parsed: ManifestFile = serde_json::from_slice(&raw).map_err(|error| {
            format!("{} is not a valid manifest table: {error}", file.display())
        })?;
        Ok(Self {
            packages: parsed.packages,
        })
    }

    pub fn get(&self, specifier: &str) -> Option<&KernelPluginManifest> {
        self.packages.get(specifier)
    }

    /// Which of rebon's own packages an entry's module is relative to.
    ///
    /// `package` and `payload` mean the same thing in this table — the package
    /// a vendored entry belongs to *is* the payload tree — so the default needs
    /// no `root` on the twenty-odd entries that live there.
    pub fn root_is_runtime(manifest: &KernelPluginManifest) -> bool {
        matches!(manifest.root, KernelPluginRoot::Runtime)
    }
}

/// Why a package's module could not be turned into a declared plugin.
///
/// Each one is said out loud rather than silently degraded, because the failure
/// they all share — loading with an empty ceiling — produces a plugin that
/// starts fine and is refused at every registration it attempts.
#[derive(Debug, Eq, PartialEq)]
pub enum ManifestProblem {
    /// No `rebon-plugin.json` beside the module.
    Missing,
    /// The file is there and is not a package manifest.
    Malformed(String),
    /// A package manifest with no `capabilities.kernelPlugins` at all.
    NoPlanePlugins,
    /// Declarations exist, but none of them names this module.
    NoEntryFor {
        module: String,
        declared: Vec<String>,
    },
    /// A declaration this package is not allowed to make.
    Refused(String),
}

impl std::fmt::Display for ManifestProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "no {PLUGIN_MANIFEST_FILE} beside the module"),
            Self::Malformed(error) => {
                write!(
                    f,
                    "{PLUGIN_MANIFEST_FILE} is not a package manifest: {error}"
                )
            }
            Self::NoPlanePlugins => write!(
                f,
                "{PLUGIN_MANIFEST_FILE} declares no capabilities.kernelPlugins"
            ),
            Self::NoEntryFor { module, declared } => write!(
                f,
                "no capabilities.kernelPlugins entry names {module:?}; the package declares {}",
                if declared.is_empty() {
                    "none".to_owned()
                } else {
                    declared.join(", ")
                }
            ),
            Self::Refused(reason) => write!(f, "{reason}"),
        }
    }
}

/// Reads a package's own manifest and finds the declaration for one module.
///
/// The module is matched against each declaration's `entry`, so a package may
/// put more than one plugin on the plane. A package that declares exactly one
/// and names the same module twice — once in `kernelPlugins.modules` and once
/// in its own manifest — is the ordinary case and matches on the first try.
///
/// Parsed as the whole [`PluginManifest`], not as a private view of the one
/// field this module wants. A partial reader is how an installed package came
/// to look valid to `rebon plugin install` and undeclared here at the same
/// time; the price of the shared schema is that a manifest the installer would
/// refuse is `Malformed` here too, which is the answer the two sides should
/// have been giving all along.
pub fn read_package_manifest(
    module: &Path,
) -> Result<(String, KernelPluginManifest), ManifestProblem> {
    let dir = module.parent().ok_or(ManifestProblem::Missing)?;
    let raw =
        std::fs::read(dir.join(PLUGIN_MANIFEST_FILE)).map_err(|_| ManifestProblem::Missing)?;
    let package: PluginManifest = serde_json::from_slice(&raw)
        .map_err(|error| ManifestProblem::Malformed(error.to_string()))?;
    if package.capabilities.kernel_plugins.is_empty() {
        return Err(ManifestProblem::NoPlanePlugins);
    }

    let wanted = module
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let found = package
        .capabilities
        .kernel_plugins
        .iter()
        .find(|(_, manifest)| entry_names(manifest, &wanted));
    let (name, manifest) = found.ok_or_else(|| ManifestProblem::NoEntryFor {
        module: wanted.clone(),
        declared: package
            .capabilities
            .kernel_plugins
            .keys()
            .cloned()
            .collect(),
    })?;

    manifest
        .validate_for_package(name)
        .map_err(ManifestProblem::Refused)?;
    Ok((name.clone(), manifest.clone()))
}

/// Whether a declaration's `entry` names this module file.
///
/// Compared by the last path segment: `kernelPlugins.modules` points at a file,
/// a manifest names it relative to the package, and `./index.mjs` and
/// `index.mjs` are the same module written two ways.
fn entry_names(manifest: &KernelPluginManifest, file: &str) -> bool {
    let Some(entry) = manifest.entry.as_deref() else {
        return false;
    };
    entry
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .is_some_and(|last| last == file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(dir: &Path, body: &str) {
        std::fs::write(dir.join(PLUGIN_MANIFEST_FILE), body).unwrap();
    }

    #[test]
    fn a_package_manifest_declares_its_plane_plugin_beside_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"{
              "name": "demo",
              "version": "1.0.0",
              "capabilities": {
                "mcpServers": { "demo": { "command": "demo-mcp" } },
                "kernelPlugins": {
                  "demo-plane": {
                    "entry": "index.mjs",
                    "services": ["echo"],
                    "seats": ["logger"],
                    "invokableTools": "$rebon/tools"
                  }
                }
              },
              "requirements": { "externalCommands": ["demo-mcp"] }
            }"#,
        );

        let (name, manifest) = read_package_manifest(&dir.path().join("index.mjs")).unwrap();
        assert_eq!(name, "demo-plane");
        assert_eq!(manifest.services, vec!["echo".to_string()]);
        assert_eq!(
            manifest.invokable(&["Read".to_string()]),
            vec!["Read".to_string()],
            "the sentinel expands to what this build exposes"
        );
    }

    /// The whole point of the change: a manifest the installer accepts is the
    /// one this side reads. Capabilities rebon does not know about here — every
    /// field but `kernelPlugins` — must not make it unreadable.
    #[test]
    fn capabilities_this_side_does_not_care_about_do_not_break_the_read() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"{
              "name": "demo",
              "version": "2.0.0",
              "description": "…",
              "source": "https://example.invalid/demo",
              "integrity": { "algorithm": "sha256", "digest": "00" },
              "metadata": { "anything": true },
              "capabilities": {
                "modelProviders": {},
                "acpAgents": {},
                "appVisualEffects": [],
                "skills": ["skills"],
                "somethingRebonGainsLater": 42,
                "kernelPlugins": { "p": { "entry": "index.mjs" } }
              }
            }"#,
        );
        assert!(read_package_manifest(&dir.path().join("index.mjs")).is_ok());
    }

    #[test]
    fn a_package_with_no_plane_declarations_says_so() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"{"name":"demo","version":"1.0.0","capabilities":{"skills":["skills"]}}"#,
        );
        assert_eq!(
            read_package_manifest(&dir.path().join("index.mjs")),
            Err(ManifestProblem::NoPlanePlugins)
        );
    }

    #[test]
    fn a_module_no_declaration_names_is_reported_with_what_was_declared() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"{"name":"demo","version":"1.0.0","capabilities":{"kernelPlugins":{
                 "a": { "entry": "one.mjs" }, "b": { "entry": "two.mjs" }}}}"#,
        );
        let error = read_package_manifest(&dir.path().join("three.mjs")).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("three.mjs"), "{text}");
        assert!(text.contains("a, b"), "{text}");
    }

    #[test]
    fn one_package_can_put_two_plugins_on_the_plane() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"{"name":"demo","version":"1.0.0","capabilities":{"kernelPlugins":{
                 "first": { "entry": "one.mjs", "services": ["a"] },
                 "second": { "entry": "./nested/two.mjs", "services": ["b"] }}}}"#,
        );
        let (first, _) = read_package_manifest(&dir.path().join("one.mjs")).unwrap();
        assert_eq!(first, "first");
        let (second, manifest) = read_package_manifest(&dir.path().join("two.mjs")).unwrap();
        assert_eq!(second, "second");
        assert_eq!(manifest.services, vec!["b".to_string()]);
    }

    #[test]
    fn a_package_pointing_its_entry_at_rebons_payload_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"{"name":"demo","version":"1.0.0","capabilities":{"kernelPlugins":{
                 "sneaky": { "root": "payload", "entry": "index.mjs" }}}}"#,
        );
        let error = read_package_manifest(&dir.path().join("index.mjs")).unwrap_err();
        assert!(matches!(error, ManifestProblem::Refused(_)), "{error:?}");
    }

    #[test]
    fn a_missing_manifest_and_a_broken_one_are_different_diagnoses() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_package_manifest(&dir.path().join("index.mjs")),
            Err(ManifestProblem::Missing)
        );
        package(dir.path(), "{ not json");
        assert!(matches!(
            read_package_manifest(&dir.path().join("index.mjs")),
            Err(ManifestProblem::Malformed(_))
        ));
    }

    #[test]
    fn the_shipped_table_still_loads() {
        let table = PayloadManifests::load(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/compose-runtime"),
        )
        .expect("the shipped manifest table parses");
        let dsh = table
            .get("@deepseek-ai/dsh-llm-deepseek")
            .expect("a vendored package is declared");
        assert_eq!(dsh.llm_providers, vec!["deepseek-official".to_string()]);
        assert!(!PayloadManifests::root_is_runtime(dsh));
    }
}
