//! Which installed packages are in effect here, and in what order.
//!
//! Three sources contribute plugins, and they are not equal: session
//! directories a user pointed at explicitly, a trusted project's own installs,
//! and the user's installs. This module owns that precedence and the shadowing
//! that goes with it, because more than one consumer needs the answer and two
//! copies of a precedence rule is two behaviours.
//!
//! # The rule
//!
//! 1. `--plugin-dir` session directories, in the order given. Explicit beats
//!    installed, always.
//! 2. A **trusted** project's enabled installs. An untrusted project's installs
//!    do not run at all — the directory is somebody else's checkout until the
//!    user says otherwise.
//! 3. The user's enabled installs, minus any capability a project record
//!    disabled or already provides. A project disabling a capability is the
//!    project saying "not here", and it outranks the user's global choice.
//!
//! Shadowing is per **capability**, not per package: two packages may both
//! carry a `demo_mcp`, and which one wins has to be decided by capability name
//! or the loser's other capabilities would be lost with it.
//!
//! # What this deliberately does not do
//!
//! It does not decide whether the project is trusted — that is a caller's
//! question, answered with the user's config, and passing it in keeps this
//! crate free of a config dependency and the rule testable without one.
//! It also materialises nothing: expanding placeholders and building runnable
//! commands belongs to whoever is going to run them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rebon_types::KernelPluginManifest;

use crate::manifest::{PluginManifest, PLUGIN_MANIFEST_FILE};
use crate::store::{InstalledPluginRecord, PluginScope, PluginSourceKind, PluginStore};

/// Where a plugin came from, in precedence order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum PluginOrigin {
    /// An explicit `--plugin-dir` for this session.
    SessionDir,
    /// Installed into a trusted project.
    Project,
    /// Installed for the user.
    User,
}

impl PluginOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionDir => "session",
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// What a discovered entry actually is.
#[derive(Clone, Debug, PartialEq)]
pub enum InstalledPlugin {
    /// A package on disk, with the manifest that describes it.
    Package {
        root: PathBuf,
        manifest: Box<PluginManifest>,
    },
    /// A built-in alias, which has no package and no manifest.
    Builtin { name: String },
}

/// One plugin in effect, with where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredPlugin {
    pub origin: PluginOrigin,
    /// The label already used in diagnostics and MCP config attribution.
    pub source: String,
    pub plugin: InstalledPlugin,
}

impl DiscoveredPlugin {
    /// The manifest, for an entry that has one.
    pub fn manifest(&self) -> Option<&PluginManifest> {
        match &self.plugin {
            InstalledPlugin::Package { manifest, .. } => Some(manifest),
            InstalledPlugin::Builtin { .. } => None,
        }
    }

    pub fn root(&self) -> Option<&Path> {
        match &self.plugin {
            InstalledPlugin::Package { root, .. } => Some(root),
            InstalledPlugin::Builtin { .. } => None,
        }
    }
}

/// Everything in effect, plus what could not be read.
///
/// Warnings rather than failures: one unreadable package must not take down a
/// session that has five working ones, and the user has to be told which.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Discovered {
    pub plugins: Vec<DiscoveredPlugin>,
    pub warnings: Vec<String>,
}

impl Discovered {
    /// The model providers the packages in effect register, by id.
    ///
    /// Config resolution needs this to know which `provider` values in
    /// `config.json` name something real rather than a typo. It used to read
    /// `installed.json` and the manifests itself, with its own record type,
    /// its own precedence and its own capability-id list — which had drifted
    /// (no `acpAgents`, no `kernelPlugins`), so the two sides disagreed about
    /// which package a project record shadowed. One reader, one answer.
    pub fn model_provider_ids(&self) -> BTreeSet<String> {
        self.plugins
            .iter()
            .filter_map(DiscoveredPlugin::manifest)
            .flat_map(|manifest| manifest.capabilities.model_providers.keys().cloned())
            .collect()
    }

    /// Kernel plugin declarations, by the name a composition refers to.
    ///
    /// First writer wins, so the precedence above decides which package
    /// provides a name two packages both declare. The package root travels
    /// with it because an `entry` is relative to it.
    pub fn kernel_plugins(&self) -> Vec<(String, PathBuf, KernelPluginManifest)> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for plugin in &self.plugins {
            let (Some(manifest), Some(root)) = (plugin.manifest(), plugin.root()) else {
                continue;
            };
            for (name, declaration) in &manifest.capabilities.kernel_plugins {
                if seen.insert(name.clone()) {
                    out.push((name.clone(), root.to_path_buf(), declaration.clone()));
                }
            }
        }
        out
    }
}

/// What one answer was derived from, so a later ask can tell it is still good.
///
/// `(len, mtime_nanos)` of each state file and each session directory's
/// manifest, plus a counter this crate bumps whenever it writes install state
/// itself. The stat pair catches another process; the counter catches this one
/// inside the resolution of a single clock tick.
#[derive(PartialEq, Eq)]
struct SourceStamp {
    generation: u64,
    files: Vec<(u64, u128)>,
}

#[derive(PartialEq, Eq)]
struct AskedFor {
    user_dir: PathBuf,
    project_dir: PathBuf,
    session_dirs: Vec<PathBuf>,
    cwd: PathBuf,
    project_trusted: bool,
}

struct Remembered {
    asked_for: AskedFor,
    stamp: SourceStamp,
    found: Discovered,
}

/// One entry, not a table: a process resolves plugins for one project, and a
/// second key means the question changed rather than that both answers are
/// worth keeping.
static REMEMBERED: Mutex<Option<Remembered>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicU64 = AtomicU64::new(0);
static RESOLVED: AtomicU64 = AtomicU64::new(0);

/// Say that install state changed under this process. [`PluginStore`] calls it
/// on every write; a host that learns about a change some other way (a config
/// event, an install run elsewhere) may call it too.
pub fn invalidate() {
    GENERATION.fetch_add(1, Ordering::Release);
}

/// `(asked, resolved)` — how many times [`discover`] was called, and how many
/// of those had to read the disk. The second number is the one the startup
/// path is measured by.
pub fn discovery_counts() -> (u64, u64) {
    (
        ASKED.load(Ordering::Acquire),
        RESOLVED.load(Ordering::Acquire),
    )
}

fn stamp_of(path: &Path) -> (u64, u128) {
    std::fs::metadata(path)
        .ok()
        .map(|meta| {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_nanos())
                .unwrap_or_default();
            (meta.len(), modified)
        })
        .unwrap_or((0, 0))
}

/// Resolves what is in effect for one session.
///
/// `project_trusted` is the caller's answer, not this crate's: see the module
/// note.
///
/// # Asked many times, resolved once
///
/// A single startup asks this question several times over, and each answer
/// used to cost a fresh read of both `installed.json` files plus a manifest
/// read per package that predates recorded manifests. The answer is a pure
/// function of files on disk, so it is remembered and re-checked against
/// `(len, mtime)` rather than re-derived. [`discovery_counts`] is what a test
/// asserts on.
pub fn discover(
    store: &PluginStore,
    session_dirs: &[PathBuf],
    cwd: &Path,
    project_trusted: bool,
) -> anyhow::Result<Discovered> {
    ASKED.fetch_add(1, Ordering::AcqRel);
    let asked_for = AskedFor {
        user_dir: store.scope_dir(PluginScope::User).clone(),
        project_dir: store.scope_dir(PluginScope::Project).clone(),
        session_dirs: session_dirs.to_vec(),
        cwd: cwd.to_path_buf(),
        project_trusted,
    };
    let stamp = SourceStamp {
        generation: GENERATION.load(Ordering::Acquire),
        files: std::iter::once(stamp_of(&store.state_path(PluginScope::User)))
            .chain(std::iter::once(stamp_of(
                &store.state_path(PluginScope::Project),
            )))
            .chain(session_dirs.iter().map(|dir| {
                let root = if dir.is_absolute() {
                    dir.clone()
                } else {
                    cwd.join(dir)
                };
                stamp_of(&root.join(PLUGIN_MANIFEST_FILE))
            }))
            .collect(),
    };

    // Held across the resolution on purpose: two threads asking the same
    // question at once should read the disk once between them, and the work
    // under the lock is the same work one of them would have done alone.
    //
    // A panic mid-resolution leaves no half-written answer — the entry is
    // replaced whole — so a poisoned lock is taken rather than skipped.
    let mut remembered = REMEMBERED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = remembered.as_ref() {
        if entry.asked_for == asked_for && entry.stamp == stamp {
            return Ok(entry.found.clone());
        }
    }
    RESOLVED.fetch_add(1, Ordering::AcqRel);
    let found = resolve(store, session_dirs, cwd, project_trusted)?;
    *remembered = Some(Remembered {
        asked_for,
        stamp,
        found: found.clone(),
    });
    Ok(found)
}

fn resolve(
    store: &PluginStore,
    session_dirs: &[PathBuf],
    cwd: &Path,
    project_trusted: bool,
) -> anyhow::Result<Discovered> {
    let mut out = Discovered::default();

    for dir in session_dirs {
        let root = if dir.is_absolute() {
            dir.clone()
        } else {
            cwd.join(dir)
        };
        if !root.join(PLUGIN_MANIFEST_FILE).is_file() {
            out.warnings.push(format!(
                "plugin-dir {} has no {}",
                root.display(),
                PLUGIN_MANIFEST_FILE
            ));
            continue;
        }
        match PluginManifest::load_from_dir(&root) {
            Ok(manifest) => out.plugins.push(DiscoveredPlugin {
                origin: PluginOrigin::SessionDir,
                source: format!("plugin:{}@local:session", manifest.name),
                plugin: InstalledPlugin::Package {
                    root,
                    manifest: Box::new(manifest),
                },
            }),
            Err(error) => out.warnings.push(error.to_string()),
        }
    }

    let user_records = store.load_state(PluginScope::User)?.plugins;
    let project_records = if project_trusted {
        store.load_state(PluginScope::Project)?.plugins
    } else {
        Vec::new()
    };

    // A project record that is switched off still speaks: "not this capability,
    // not here" outranks the user's global enable.
    let disabled_by_project: BTreeSet<String> = project_records
        .iter()
        .filter(|record| !record.enabled)
        .flat_map(capability_ids)
        .collect();
    let provided_by_project: BTreeSet<String> = project_records
        .iter()
        .filter(|record| record.enabled)
        .flat_map(capability_ids)
        .collect();

    for record in project_records.iter().filter(|record| record.enabled) {
        push_record(&mut out, store, PluginScope::Project, record);
    }

    for record in user_records.iter().filter(|record| record.enabled) {
        let ids = capability_ids(record);
        if ids
            .iter()
            .any(|id| disabled_by_project.contains(id) || provided_by_project.contains(id))
        {
            continue;
        }
        push_record(&mut out, store, PluginScope::User, record);
    }

    Ok(out)
}

fn push_record(
    out: &mut Discovered,
    store: &PluginStore,
    scope: PluginScope,
    record: &InstalledPluginRecord,
) {
    let origin = match scope {
        PluginScope::Project => PluginOrigin::Project,
        PluginScope::User => PluginOrigin::User,
    };
    let source = record.source_label(scope);
    match record.source_kind {
        PluginSourceKind::Builtin => out.plugins.push(DiscoveredPlugin {
            origin,
            source,
            plugin: InstalledPlugin::Builtin {
                name: record.name.clone(),
            },
        }),
        PluginSourceKind::Local => {
            let root = store.plugin_version_dir(scope, &record.name, &record.version);
            // The manifest recorded at install time is the one the user saw and
            // the installer validated. Re-reading the directory is the fallback
            // for a record written before manifests were kept.
            let manifest = match record.manifest.clone() {
                Some(manifest) => Ok(manifest),
                None => PluginManifest::load_from_dir(&root),
            };
            match manifest {
                Ok(manifest) => out.plugins.push(DiscoveredPlugin {
                    origin,
                    source,
                    plugin: InstalledPlugin::Package {
                        root,
                        manifest: Box::new(manifest),
                    },
                }),
                Err(error) => out.warnings.push(error.to_string()),
            }
        }
    }
}

/// The capability names one record occupies, for shadowing.
///
/// A package with no capabilities at all still occupies its own name, so two
/// installs of the same empty package do not both appear.
pub fn capability_ids(record: &InstalledPluginRecord) -> Vec<String> {
    let Some(manifest) = &record.manifest else {
        return if record.name == "rust-lsp" {
            vec!["mcp:rust_lsp".to_string()]
        } else {
            vec![format!("plugin:{}", record.name)]
        };
    };
    let ids = manifest.capability_ids();
    if ids.is_empty() {
        vec![format!("plugin:{}", record.name)]
    } else {
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PluginInstallState;

    fn manifest_with(name: &str, body: serde_json::Value) -> PluginManifest {
        let mut value = serde_json::json!({ "name": name, "version": "1.0.0" });
        value
            .as_object_mut()
            .unwrap()
            .insert("capabilities".into(), body);
        serde_json::from_value(value).unwrap()
    }

    fn record(name: &str, enabled: bool, capabilities: serde_json::Value) -> InstalledPluginRecord {
        InstalledPluginRecord {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            enabled,
            disabled_capabilities: Vec::new(),
            source_kind: PluginSourceKind::Local,
            source: None,
            digest: None,
            manifest: Some(manifest_with(name, capabilities)),
        }
    }

    fn store_with(
        dir: &Path,
        user: Vec<InstalledPluginRecord>,
        project: Vec<InstalledPluginRecord>,
    ) -> PluginStore {
        let store = PluginStore::new(dir.join("home"), dir.join("project"));
        store
            .save_state_atomic(PluginScope::User, &PluginInstallState { plugins: user })
            .unwrap();
        store
            .save_state_atomic(
                PluginScope::Project,
                &PluginInstallState { plugins: project },
            )
            .unwrap();
        store
    }

    /// Asking three times is legitimate; reading the disk three times is not.
    ///
    /// Serialised against the other tests in this module through
    /// [`ONE_AT_A_TIME`], because the memory is process-wide and a concurrent
    /// test with a different question evicts the entry this one just made.
    #[test]
    fn the_same_question_is_resolved_once_and_re_resolved_when_the_state_changes() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(
            dir.path(),
            vec![record("u", true, serde_json::json!({}))],
            Vec::new(),
        );

        let before = discovery_counts();
        let first = discover(&store, &[], dir.path(), true).unwrap();
        let second = discover(&store, &[], dir.path(), true).unwrap();
        let third = discover(&store, &[], dir.path(), true).unwrap();
        assert_eq!(first, second);
        assert_eq!(second, third);
        let after = discovery_counts();
        assert_eq!(after.0 - before.0, 3, "asked three times");
        assert_eq!(after.1 - before.1, 1, "resolved once");

        // A write through the store is a change this process made, so the next
        // ask reads again even though no clock has necessarily ticked.
        store
            .save_state_atomic(
                PluginScope::User,
                &PluginInstallState {
                    plugins: vec![
                        record("u", true, serde_json::json!({})),
                        record("v", true, serde_json::json!({})),
                    ],
                },
            )
            .unwrap();
        let fourth = discover(&store, &[], dir.path(), true).unwrap();
        assert_eq!(fourth.plugins.len(), 2);
        let reread = discovery_counts();
        assert_eq!(reread.1 - after.1, 1, "an install re-resolves");

        // A different question is a different answer, not the remembered one.
        let untrusted = discover(&store, &[], dir.path(), false).unwrap();
        assert_eq!(untrusted.plugins.len(), 2);
        assert_eq!(
            discovery_counts().1 - reread.1,
            1,
            "changing the question re-resolves"
        );
    }

    /// The memory is one process-wide entry, so a test that counts resolutions
    /// has to be the only one asking.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    #[test]
    fn an_untrusted_project_contributes_nothing() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(
            dir.path(),
            vec![record("u", true, serde_json::json!({}))],
            vec![record("p", true, serde_json::json!({}))],
        );

        let trusted = discover(&store, &[], dir.path(), true).unwrap();
        assert_eq!(trusted.plugins.len(), 2);

        let untrusted = discover(&store, &[], dir.path(), false).unwrap();
        assert_eq!(untrusted.plugins.len(), 1);
        assert_eq!(untrusted.plugins[0].origin, PluginOrigin::User);
    }

    #[test]
    fn a_project_capability_shadows_the_users_copy_of_it() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let same = serde_json::json!({"mcpServers": {"shared": {"command": "x"}}});
        let store = store_with(
            dir.path(),
            vec![record("u", true, same.clone())],
            vec![record("p", true, same)],
        );

        let found = discover(&store, &[], dir.path(), true).unwrap();
        assert_eq!(found.plugins.len(), 1);
        assert_eq!(found.plugins[0].origin, PluginOrigin::Project);
    }

    /// A project record that is switched off is a statement, not an absence.
    #[test]
    fn a_project_disable_suppresses_the_users_copy() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let same = serde_json::json!({"mcpServers": {"shared": {"command": "x"}}});
        let store = store_with(
            dir.path(),
            vec![record("u", true, same.clone())],
            vec![record("p", false, same)],
        );

        let found = discover(&store, &[], dir.path(), true).unwrap();
        assert!(found.plugins.is_empty(), "{:?}", found.plugins);
    }

    #[test]
    fn a_session_directory_comes_first_and_needs_no_install() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session-plugin");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join(PLUGIN_MANIFEST_FILE),
            br#"{"name":"sess","version":"1.0.0","capabilities":{}}"#,
        )
        .unwrap();
        let store = store_with(
            dir.path(),
            vec![record("u", true, serde_json::json!({}))],
            Vec::new(),
        );

        let found = discover(&store, &[session], dir.path(), true).unwrap();
        assert_eq!(found.plugins[0].origin, PluginOrigin::SessionDir);
        assert!(found.plugins[0].source.contains("session"));
    }

    #[test]
    fn a_session_directory_without_a_manifest_is_a_warning_not_a_failure() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let store = store_with(dir.path(), Vec::new(), Vec::new());

        let found = discover(&store, &[empty], dir.path(), true).unwrap();
        assert!(found.plugins.is_empty());
        assert_eq!(found.warnings.len(), 1);
        assert!(found.warnings[0].contains(PLUGIN_MANIFEST_FILE));
    }

    /// The point of the whole module for the plugin plane: a package that
    /// declares a kernel plugin is loadable by that name, with its own
    /// directory as the root its entry is relative to.
    #[test]
    fn kernel_plugin_declarations_come_back_with_their_package_root() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(
            dir.path(),
            vec![record(
                "plane-pkg",
                true,
                serde_json::json!({
                    "kernelPlugins": {
                        "demo": { "entry": "index.mjs", "services": ["echo"] }
                    }
                }),
            )],
            Vec::new(),
        );

        let found = discover(&store, &[], dir.path(), true).unwrap();
        let declared = found.kernel_plugins();
        assert_eq!(declared.len(), 1);
        let (name, root, manifest) = &declared[0];
        assert_eq!(name, "demo");
        assert!(root.ends_with(Path::new("plane-pkg").join("1.0.0")));
        assert_eq!(manifest.entry.as_deref(), Some("index.mjs"));
        assert_eq!(manifest.services, vec!["echo".to_string()]);
    }

    #[test]
    fn the_first_source_to_declare_a_kernel_plugin_name_keeps_it() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let declaring =
            |entry: &str| serde_json::json!({"kernelPlugins": {"demo": {"entry": entry}}});
        let store = store_with(
            dir.path(),
            vec![record("u", true, declaring("user.mjs"))],
            vec![record("p", true, declaring("project.mjs"))],
        );

        let found = discover(&store, &[], dir.path(), true).unwrap();
        let declared = found.kernel_plugins();
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].2.entry.as_deref(), Some("project.mjs"));
    }
}
