//! Claude Code marketplaces: added, fetched, browsed, installed from.
//!
//! The catalog vocabulary is `rebon_harness::rebon_plugin_package::marketplace`; this is
//! the acting half. A marketplace is added from what the person typed (a
//! GitHub `owner/repo`, a git URL, a URL to a `marketplace.json`, a path),
//! fetched into `plugins/marketplaces/<name>` under the config home (a
//! shallow `git clone`, a download, or nothing for a local folder) and
//! recorded under the name its own manifest gives. Rebon's own marketplace
//! (`rebon`) ships beside the executable and is always there.
//!
//! Installing `plugin@marketplace` fetches the plugin's folder by its entry's
//! source — a path inside the marketplace, a GitHub or git repository (or a
//! folder in one), an npm package, a tar archive — and then installs it by
//! what it is: a Claude Code mod is copied under the config home's `mods/`,
//! where it loads with no configuration; a Rebon package goes through the
//! plugin store and is listed in `kernelPlugins.plugins` so it loads. A
//! source that is a shell command is never run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context as _};
use serde::Serialize;
use sha2::{Digest, Sha256};

use rebon_harness::rebon_plugin_package::marketplace::{
    marketplace_copy_dir, parse_marketplace_input, plugin_id, plugins_dir,
    refused_marketplace_name, relative_plugin_dir, InstallKind, KnownMarketplace,
    KnownMarketplaces, MarketplaceEntry, MarketplaceInstall, MarketplaceInstalls,
    MarketplaceManifest, MarketplaceSource, PluginShape, PluginSource, RemoteSource,
    BUILTIN_MARKETPLACE, MARKETPLACE_MANIFEST,
};

use super::installer::copy_dir_recursive;
use super::package::{unpack_archive_folder, PackageLimits};
use super::{PluginInstaller, PluginScope, PluginStore};

/// The env var naming the folder of Rebon's own marketplace.
pub const MARKETPLACE_DIR_ENV: &str = "REBON_MARKETPLACE_DIR";

/// The folder Rebon's own marketplace is read from: `$REBON_MARKETPLACE_DIR`,
/// a `marketplace` folder beside the executable (as a release lays it out),
/// or the source tree's when running from a build.
pub fn builtin_marketplace_dir() -> Option<PathBuf> {
    let holds = |dir: &Path| dir.join(MARKETPLACE_MANIFEST).is_file();
    if let Some(dir) = std::env::var_os(MARKETPLACE_DIR_ENV).map(PathBuf::from) {
        return holds(&dir).then_some(dir);
    }
    // Beside the executable (the npm package, the Windows installer), or in
    // a macOS bundle's Resources.
    if let Some(parent) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        let beside = parent.join("marketplace");
        if holds(&beside) {
            return Some(beside);
        }
        if let Some(resources) = parent
            .parent()
            .map(|contents| contents.join("Resources").join("marketplace"))
        {
            if holds(&resources) {
                return Some(resources);
            }
        }
    }
    // The source tree's, for a binary run from its target folder; its
    // parent steps dropped so it reads as the folder it is.
    let tree = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)?
        .join("marketplace");
    holds(&tree).then_some(tree)
}

/// One marketplace as a listing shows it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceView {
    pub name: String,
    pub source: String,
    pub builtin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub plugins: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated_ms: Option<u64>,
    /// Why its catalog could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One plugin a browse lists.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    /// `plugin@marketplace`.
    pub id: String,
    pub marketplace: String,
    #[serde(flatten)]
    pub entry: MarketplaceEntry,
    pub source_kind: &'static str,
    /// `mod`, `package` or `unsupported` for a plugin whose folder is in the
    /// marketplace already; absent for one fetched from elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
    pub installed: bool,
    /// What it will ask of its container beyond its own files, when its
    /// folder is here to read: what a person agrees to by installing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<rebon_harness::rebon_plugin_package::container::ContainerRequest>,
}

/// Every marketplace and every plugin they list.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Catalog {
    pub marketplaces: Vec<MarketplaceView>,
    pub plugins: Vec<CatalogEntry>,
}

/// A marketplace whose catalog was read.
struct Loaded {
    name: String,
    root: Option<PathBuf>,
    manifest: MarketplaceManifest,
}

pub struct MarketplaceManager {
    config_home: PathBuf,
    cwd: PathBuf,
    builtin: Option<PathBuf>,
}

impl MarketplaceManager {
    pub fn new(config_home: PathBuf, cwd: PathBuf) -> Self {
        Self {
            config_home,
            cwd,
            builtin: builtin_marketplace_dir(),
        }
    }

    /// The same manager with Rebon's own marketplace read from `dir`.
    pub fn with_builtin(mut self, dir: Option<PathBuf>) -> Self {
        self.builtin = dir;
        self
    }

    fn known(&self) -> anyhow::Result<KnownMarketplaces> {
        KnownMarketplaces::load(&self.config_home)
    }

    /// Every marketplace's catalog, Rebon's first; one that cannot be read
    /// is listed with why.
    fn load_all(&self) -> anyhow::Result<Vec<(MarketplaceView, Option<Loaded>)>> {
        let mut out = Vec::new();
        if let Some(dir) = &self.builtin {
            let read = MarketplaceManifest::read(&dir.join(MARKETPLACE_MANIFEST));
            out.push(view_of(
                BUILTIN_MARKETPLACE,
                dir.display().to_string(),
                true,
                None,
                read.map(|manifest| Loaded {
                    name: BUILTIN_MARKETPLACE.to_owned(),
                    root: Some(dir.clone()),
                    manifest,
                }),
            ));
        }
        for (name, known) in self.known()?.by_name {
            let read = MarketplaceManifest::read(&known.manifest_path());
            out.push(view_of(
                &name,
                known.source.describe(),
                false,
                known.last_updated_ms,
                read.map(|manifest| Loaded {
                    name: name.clone(),
                    root: known.root(),
                    manifest,
                }),
            ));
        }
        Ok(out)
    }

    pub fn marketplaces(&self) -> anyhow::Result<Vec<MarketplaceView>> {
        Ok(self.load_all()?.into_iter().map(|(view, _)| view).collect())
    }

    /// Adds a marketplace: fetches it, reads its manifest, and records it
    /// under the name the manifest gives.
    pub fn add(&self, raw: &str) -> anyhow::Result<MarketplaceView> {
        let home = home_dir();
        let source = parse_marketplace_input(raw, &self.cwd, home.as_deref())?;
        let scratch = self.scratch_dir("marketplace")?;
        let fetched = fetch_marketplace(&source, scratch.path())?;
        let manifest = MarketplaceManifest::read(&fetched.manifest)?;
        if let Some(refusal) = refused_marketplace_name(&manifest.name) {
            bail!(refusal);
        }
        let mut known = self.known()?;
        if let Some(held) = known.by_name.get(&manifest.name) {
            if held.source != source {
                bail!(
                    "a marketplace named {:?} is already added (from {}); remove it first",
                    manifest.name,
                    held.source.describe()
                );
            }
        }
        let install_location = if source.is_local() {
            fetched.root.clone()
        } else {
            let copy = marketplace_copy_dir(&self.config_home, &manifest.name);
            replace_dir(&fetched.root, &copy)?;
            copy
        };
        known.by_name.insert(
            manifest.name.clone(),
            KnownMarketplace {
                source,
                install_location,
                last_updated_ms: Some(rebon_types::wall_clock_ms()),
            },
        );
        known.save(&self.config_home)?;
        let name = manifest.name.clone();
        self.marketplaces()?
            .into_iter()
            .find(|view| view.name == name)
            .ok_or_else(|| anyhow!("{name} was added but does not list"))
    }

    /// Fetches added marketplaces again: one by name, or every one. Answers
    /// each with what happened.
    pub fn update(&self, name: Option<&str>) -> anyhow::Result<Vec<(String, Result<(), String>)>> {
        let mut known = self.known()?;
        if let Some(name) = name {
            if name == BUILTIN_MARKETPLACE {
                bail!("the {BUILTIN_MARKETPLACE} marketplace ships with Rebon and updates with it");
            }
            if !known.by_name.contains_key(name) {
                bail!("no marketplace named {name:?} is added");
            }
        }
        let names: Vec<String> = known
            .by_name
            .keys()
            .filter(|held| name.is_none_or(|name| name == held.as_str()))
            .cloned()
            .collect();
        let mut outcomes = Vec::new();
        for held in names {
            let entry = known.by_name.get(&held).expect("listed above").clone();
            let outcome = self
                .refetch(&held, &entry)
                .map_err(|error| format!("{error:#}"));
            if outcome.is_ok() {
                if let Some(entry) = known.by_name.get_mut(&held) {
                    entry.last_updated_ms = Some(rebon_types::wall_clock_ms());
                }
            }
            outcomes.push((held, outcome));
        }
        known.save(&self.config_home)?;
        Ok(outcomes)
    }

    fn refetch(&self, name: &str, known: &KnownMarketplace) -> anyhow::Result<()> {
        if known.source.is_local() {
            // Read in place: there is nothing to fetch, only to check.
            MarketplaceManifest::read(&known.manifest_path())?;
            return Ok(());
        }
        let scratch = self.scratch_dir("marketplace")?;
        let fetched = fetch_marketplace(&known.source, scratch.path())?;
        let manifest = MarketplaceManifest::read(&fetched.manifest)?;
        if manifest.name != name {
            bail!(
                "its manifest now names it {:?}; remove it and add it again",
                manifest.name
            );
        }
        replace_dir(&fetched.root, &known.install_location)
    }

    /// Forgets an added marketplace and deletes its copy. What was installed
    /// from it stays installed.
    pub fn remove(&self, name: &str) -> anyhow::Result<()> {
        if name == BUILTIN_MARKETPLACE {
            bail!("the {BUILTIN_MARKETPLACE} marketplace ships with Rebon and cannot be removed");
        }
        let mut known = self.known()?;
        let Some(entry) = known.by_name.remove(name) else {
            bail!("no marketplace named {name:?} is added");
        };
        let copies = plugins_dir(&self.config_home).join("marketplaces");
        if !entry.source.is_local() && entry.install_location.starts_with(&copies) {
            remove_dir_if_present(&entry.install_location)?;
        }
        known.save(&self.config_home)
    }

    /// Every plugin every marketplace lists, with what is installed.
    pub fn browse(&self) -> anyhow::Result<Catalog> {
        let installs = MarketplaceInstalls::load(&self.config_home)?;
        let mut catalog = Catalog::default();
        for (view, loaded) in self.load_all()? {
            catalog.marketplaces.push(view);
            let Some(loaded) = loaded else { continue };
            for entry in &loaded.manifest.plugins {
                let id = plugin_id(&entry.name, &loaded.name);
                let folder = match (&entry.source, &loaded.root) {
                    (PluginSource::Relative(path), Some(root)) => {
                        relative_plugin_dir(root, &loaded.manifest, path).ok()
                    }
                    _ => None,
                };
                let shape = folder.as_deref().map(PluginShape::of);
                let container = match (&shape, &folder) {
                    (Some(PluginShape::RebonPackage), Some(dir)) => Some(
                        rebon_harness::rebon_plugin_package::PluginManifest::load_from_dir(dir)
                            .ok()
                            .and_then(|manifest| manifest.container)
                            .unwrap_or_default(),
                    ),
                    // A mod declares nothing beyond its own folder.
                    (Some(PluginShape::ClaudeMod), _) => Some(Default::default()),
                    _ => None,
                };
                catalog.plugins.push(CatalogEntry {
                    installed: installs.by_id.contains_key(&id),
                    id,
                    marketplace: loaded.name.clone(),
                    entry: entry.clone(),
                    source_kind: entry.source.kind(),
                    shape: shape.as_ref().map(PluginShape::label),
                    unsupported_reason: match shape {
                        Some(PluginShape::Unsupported { reason }) => Some(reason),
                        _ => None,
                    },
                    container,
                });
            }
        }
        Ok(catalog)
    }

    /// The installs this machine holds from marketplaces.
    pub fn installs(&self) -> anyhow::Result<MarketplaceInstalls> {
        MarketplaceInstalls::load(&self.config_home)
    }

    /// Finds `plugin@marketplace`, or a `plugin` only one marketplace lists.
    fn find(&self, spec: &str) -> anyhow::Result<(Loaded, MarketplaceEntry)> {
        let (plugin, marketplace) = match spec.rsplit_once('@') {
            Some((plugin, marketplace)) if !plugin.is_empty() => (plugin, Some(marketplace)),
            _ => (spec, None),
        };
        let mut found = Vec::new();
        for (view, loaded) in self.load_all()? {
            if marketplace.is_some_and(|wanted| wanted != view.name) {
                continue;
            }
            let Some(loaded) = loaded else {
                if marketplace.is_some() {
                    bail!(
                        "the {} marketplace cannot be read: {}",
                        view.name,
                        view.error.unwrap_or_default()
                    );
                }
                continue;
            };
            if let Some(entry) = loaded.manifest.entry(plugin).cloned() {
                found.push((loaded, entry));
            }
        }
        match found.len() {
            0 => match marketplace {
                Some(marketplace) => {
                    bail!("the {marketplace} marketplace lists no plugin {plugin:?}")
                }
                None => bail!("no marketplace lists a plugin {plugin:?}"),
            },
            1 => Ok(found.pop().expect("one found")),
            _ => {
                let names: Vec<String> = found
                    .iter()
                    .map(|(loaded, _)| plugin_id(plugin, &loaded.name))
                    .collect();
                bail!(
                    "several marketplaces list {plugin:?}; name one: {}",
                    names.join(", ")
                )
            }
        }
    }

    /// Installs `plugin@marketplace` (or a plugin only one marketplace
    /// lists), replacing an earlier install of it.
    pub fn install(&self, spec: &str, scope: PluginScope) -> anyhow::Result<MarketplaceInstall> {
        if scope != PluginScope::User {
            bail!("a marketplace plugin installs for the user (--scope user)");
        }
        let (loaded, entry) = self.find(spec)?;
        let id = plugin_id(&entry.name, &loaded.name);
        let scratch = self.scratch_dir("plugin")?;
        let folder = materialize(&loaded, &entry.source, scratch.path())?;
        let mut installs = MarketplaceInstalls::load(&self.config_home)?;
        let mut shape = PluginShape::of(&folder);
        if matches!(shape, PluginShape::Unsupported { .. })
            && super::dsh_npm::is_cordis_npm_package(&folder)
        {
            // A DeepSeek Harness package from npm: probed, and given the
            // manifest it does not carry, before anything is installed.
            super::dsh_npm::adapt(&folder, entry.rebon_config.as_ref())?;
            shape = PluginShape::RebonPackage;
        }
        let install =
            match shape {
                PluginShape::ClaudeMod => {
                    let target = self
                        .config_home
                        .join(rebon_types::MODS_DIR)
                        .join(&entry.name);
                    replace_dir(&folder, &target)?;
                    MarketplaceInstall {
                        marketplace: loaded.name.clone(),
                        plugin: entry.name.clone(),
                        kind: InstallKind::Mod,
                        location: target,
                        version: entry.version.clone(),
                        kernel_plugins: Vec::new(),
                        installed_at_ms: rebon_types::wall_clock_ms(),
                        // A mod declares nothing beyond its own folder.
                        granted: Default::default(),
                    }
                }
                PluginShape::RebonPackage => {
                    let installer = PluginInstaller::new(
                        PluginStore::new(self.config_home.clone(), self.cwd.clone()),
                        self.cwd.clone(),
                        Vec::new(),
                    );
                    let record =
                        installer.install(&folder.to_string_lossy(), PluginScope::User, None)?;
                    let kernel_plugins: Vec<String> = record
                        .manifest
                        .as_ref()
                        .map(|manifest| {
                            manifest
                                .capabilities
                                .kernel_plugins
                                .keys()
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    for kernel_plugin in &kernel_plugins {
                        let config = default_kernel_config(record.manifest.as_ref(), kernel_plugin);
                        rebon_config::set_kernel_plugin_listed_in(
                            &self.config_home,
                            kernel_plugin,
                            true,
                            config.as_ref(),
                        )?;
                    }
                    MarketplaceInstall {
                    marketplace: loaded.name.clone(),
                    plugin: entry.name.clone(),
                    kind: InstallKind::Package,
                    location: PathBuf::from(&record.name),
                    version: Some(record.version.clone()),
                    kernel_plugins,
                    installed_at_ms: rebon_types::wall_clock_ms(),
                    granted: rebon_harness::rebon_plugin_package::container::ContainerGrants::load(
                        &self.config_home,
                    )
                    .get(&rebon_harness::rebon_plugin_package::container::package_container_id(
                        &record.name,
                    )),
                }
                }
                PluginShape::Unsupported { reason } => {
                    bail!("{id} is not a plugin Rebon installs: {reason}")
                }
            };
        installs.by_id.insert(id, install.clone());
        installs.save(&self.config_home)?;
        Ok(install)
    }

    /// Uninstalls what was installed as `plugin@marketplace` (or the one
    /// install of `plugin`).
    pub fn uninstall(&self, spec: &str) -> anyhow::Result<MarketplaceInstall> {
        let mut installs = MarketplaceInstalls::load(&self.config_home)?;
        let id = if installs.by_id.contains_key(spec) {
            spec.to_owned()
        } else {
            let matching: Vec<&String> = installs
                .by_id
                .iter()
                .filter(|(_, install)| install.plugin == spec)
                .map(|(id, _)| id)
                .collect();
            match matching.as_slice() {
                [only] => (*only).clone(),
                [] => bail!("nothing is installed from a marketplace as {spec:?}"),
                _ => bail!("several installs are named {spec:?}; name one as plugin@marketplace"),
            }
        };
        let install = installs.by_id.remove(&id).expect("found above");
        match install.kind {
            InstallKind::Mod => {
                let mods = self.config_home.join(rebon_types::MODS_DIR);
                if install.location.starts_with(&mods) {
                    // The container is named after the mod's own manifest,
                    // read before the folder goes.
                    let name =
                        rebon_harness::rebon_plugin_package::read_claude_mod(&install.location)
                            .map(|mod_| mod_.manifest.name)
                            .unwrap_or_else(|_| install.plugin.clone());
                    remove_dir_if_present(&install.location)?;
                    super::installer::forget_container_in(
                        &self.config_home,
                        &rebon_harness::rebon_plugin_package::container::mod_container_id(&name),
                    );
                }
            }
            InstallKind::Package => {
                let installer = PluginInstaller::new(
                    PluginStore::new(self.config_home.clone(), self.cwd.clone()),
                    self.cwd.clone(),
                    Vec::new(),
                );
                installer.uninstall(&install.location.to_string_lossy(), PluginScope::User)?;
                for kernel_plugin in &install.kernel_plugins {
                    rebon_config::set_kernel_plugin_listed_in(
                        &self.config_home,
                        kernel_plugin,
                        false,
                        None,
                    )?;
                }
            }
        }
        installs.save(&self.config_home)?;
        Ok(install)
    }

    /// A scratch folder under the config home, on the volume the copies go
    /// to, deleted when dropped.
    fn scratch_dir(&self, what: &str) -> anyhow::Result<Scratch> {
        let parent = plugins_dir(&self.config_home);
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let dir = parent.join(format!(
            ".fetch-{what}-{}-{}",
            std::process::id(),
            rebon_types::wall_clock_ms()
        ));
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Scratch(dir))
    }
}

/// A scratch folder that is deleted when dropped, git's read-only object
/// files included (which a plain removal cannot delete on Windows).
struct Scratch(PathBuf);

impl Scratch {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        make_writable(&self.0);
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::debug!(%error, "a marketplace scratch folder was not removed");
        }
    }
}

fn make_writable(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(metadata) = entry.metadata() {
            if metadata.is_dir() {
                make_writable(&path);
            } else {
                let mut permissions = metadata.permissions();
                if permissions.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    let _ = std::fs::set_permissions(&path, permissions);
                }
            }
        }
    }
}

/// The composition config a package asks its kernel plugin `id` be listed
/// with: `metadata.kernelPluginConfig.<id>` in its `rebon-plugin.json`, for a
/// module whose own config schema has a field it cannot start without.
fn default_kernel_config(
    manifest: Option<&rebon_harness::rebon_plugin_package::PluginManifest>,
    id: &str,
) -> Option<serde_json::Value> {
    manifest?
        .metadata
        .get("kernelPluginConfig")?
        .get(id)
        .cloned()
}

fn view_of(
    name: &str,
    source: String,
    builtin: bool,
    last_updated_ms: Option<u64>,
    read: anyhow::Result<Loaded>,
) -> (MarketplaceView, Option<Loaded>) {
    match read {
        Ok(loaded) => (
            MarketplaceView {
                name: name.to_owned(),
                source,
                builtin,
                description: loaded.manifest.description.clone().or_else(|| {
                    loaded
                        .manifest
                        .metadata
                        .as_ref()
                        .and_then(|m| m.description.clone())
                }),
                plugins: loaded.manifest.plugins.len(),
                last_updated_ms,
                error: None,
            },
            Some(loaded),
        ),
        Err(error) => (
            MarketplaceView {
                name: name.to_owned(),
                source,
                builtin,
                description: None,
                plugins: 0,
                last_updated_ms,
                error: Some(format!("{error:#}")),
            },
            None,
        ),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// What a fetch of a marketplace left: its root and its manifest.
struct Fetched {
    root: PathBuf,
    manifest: PathBuf,
}

fn fetch_marketplace(source: &MarketplaceSource, scratch: &Path) -> anyhow::Result<Fetched> {
    match source {
        MarketplaceSource::Github { .. } | MarketplaceSource::Git { .. } => {
            let url = source.git_url().expect("a repository source");
            let root = scratch.join("repo");
            git_clone(&url, source.git_ref(), None, &root)?;
            let manifest = root.join(MARKETPLACE_MANIFEST);
            if !manifest.is_file() {
                bail!("{url} has no {MARKETPLACE_MANIFEST}");
            }
            Ok(Fetched { root, manifest })
        }
        MarketplaceSource::Url { url } => {
            let bytes = http_get(url)?;
            let root = scratch.join("download");
            let manifest = root.join(MARKETPLACE_MANIFEST);
            std::fs::create_dir_all(manifest.parent().expect("a parent"))?;
            std::fs::write(&manifest, bytes)?;
            Ok(Fetched { root, manifest })
        }
        MarketplaceSource::Directory { path } => {
            let manifest = path.join(MARKETPLACE_MANIFEST);
            if !manifest.is_file() {
                bail!("{} has no {MARKETPLACE_MANIFEST}", path.display());
            }
            Ok(Fetched {
                root: path.clone(),
                manifest,
            })
        }
        MarketplaceSource::File { path } => {
            if !path.is_file() {
                bail!("{} is not a file", path.display());
            }
            Ok(Fetched {
                root: path.clone(),
                manifest: path.clone(),
            })
        }
    }
}

/// The folder a plugin's source names, fetched into `scratch` when it is
/// not in the marketplace already.
fn materialize(loaded: &Loaded, source: &PluginSource, scratch: &Path) -> anyhow::Result<PathBuf> {
    match source {
        PluginSource::Relative(path) => {
            let Some(root) = &loaded.root else {
                bail!(
                    "the {} marketplace was fetched as a file: its plugin {path:?} has no folder to come from",
                    loaded.name
                );
            };
            let dir = relative_plugin_dir(root, &loaded.manifest, path)?;
            if !dir.is_dir() {
                bail!("{} is not in the marketplace", dir.display());
            }
            Ok(dir)
        }
        PluginSource::Remote(RemoteSource::Github { repo, git_ref, sha }) => {
            let dir = scratch.join("repo");
            git_clone(
                &format!("https://github.com/{repo}.git"),
                git_ref.as_deref(),
                sha.as_deref(),
                &dir,
            )?;
            Ok(dir)
        }
        PluginSource::Remote(RemoteSource::Url { url, git_ref, sha }) => {
            let dir = scratch.join("repo");
            git_clone(url, git_ref.as_deref(), sha.as_deref(), &dir)?;
            Ok(dir)
        }
        PluginSource::Remote(RemoteSource::GitSubdir {
            url,
            path,
            git_ref,
            sha,
        }) => {
            let url = if url.contains("://") || url.contains('@') {
                url.clone()
            } else {
                format!("https://github.com/{}.git", url.trim_end_matches(".git"))
            };
            let repo = scratch.join("repo");
            git_clone(&url, git_ref.as_deref(), sha.as_deref(), &repo)?;
            let inner = Path::new(path.trim_start_matches("./"));
            if inner
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                bail!("git-subdir path {path:?} leaves the repository");
            }
            let dir = repo.join(inner);
            if !dir.is_dir() {
                bail!("{url} has no folder {path}");
            }
            Ok(dir)
        }
        PluginSource::Remote(RemoteSource::Npm {
            package,
            version,
            registry,
        }) => npm_pack(package, version.as_deref(), registry.as_deref(), scratch),
        PluginSource::Remote(RemoteSource::Archive { url, sha256 }) => {
            if !url.starts_with("https://") {
                bail!("an archive source is fetched over https only");
            }
            let lower = url.to_ascii_lowercase();
            let name = if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
                "plugin.tgz"
            } else if lower.ends_with(".tar") {
                "plugin.tar"
            } else {
                bail!("Rebon unpacks .tgz, .tar.gz and .tar archives; {url} is none of them");
            };
            let bytes = http_get(url)?;
            if let Some(expected) = sha256 {
                let actual = hex(&Sha256::digest(&bytes));
                if !actual.eq_ignore_ascii_case(expected) {
                    bail!("{url} does not match its sha256 (expected {expected}, got {actual})");
                }
            }
            let archive = scratch.join(name);
            std::fs::write(&archive, bytes)?;
            unpack_archive_folder(
                &archive,
                &scratch.join("unpacked"),
                PackageLimits::default(),
            )
        }
        PluginSource::Remote(RemoteSource::Command { .. }) => {
            bail!("Rebon does not run a marketplace's command to find a plugin")
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A shallow clone of `url` at `git_ref`, or a full one checked out at
/// `sha`. Never prompts: a repository that wants credentials fails.
fn git_clone(
    url: &str,
    git_ref: Option<&str>,
    sha: Option<&str>,
    dest: &Path,
) -> anyhow::Result<()> {
    let mut clone = Command::new("git");
    clone
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("clone")
        .arg("--quiet");
    if sha.is_none() {
        clone.args(["--depth", "1"]);
    }
    if let Some(git_ref) = git_ref {
        clone.args(["--branch", git_ref]);
    }
    clone.arg("--").arg(url).arg(dest);
    run(&mut clone, &format!("git clone {url}"))?;
    if let Some(sha) = sha {
        let mut checkout = Command::new("git");
        checkout
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("-C")
            .arg(dest)
            .args(["checkout", "--quiet", "--detach", sha]);
        run(&mut checkout, &format!("git checkout {sha}"))?;
    }
    Ok(())
}

/// `npm pack` of the package into `scratch`, unpacked: install scripts never
/// run.
fn npm_pack(
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    scratch: &Path,
) -> anyhow::Result<PathBuf> {
    let spec = match version {
        Some(version) if !package.starts_with("https://") => format!("{package}@{version}"),
        _ => package.to_owned(),
    };
    let packs = scratch.join("pack");
    std::fs::create_dir_all(&packs)?;
    let mut pack = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" });
    pack.args(["pack", "--silent", "--ignore-scripts", "--pack-destination"])
        .arg(&packs)
        .arg(&spec);
    if let Some(registry) = registry {
        pack.arg("--registry").arg(registry);
    }
    run(&mut pack, &format!("npm pack {spec}"))?;
    let tarball = std::fs::read_dir(&packs)?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "tgz"))
        .ok_or_else(|| anyhow!("npm pack {spec} left no tarball"))?;
    unpack_archive_folder(
        &tarball,
        &scratch.join("unpacked"),
        PackageLimits::default(),
    )
}

fn run(command: &mut Command, what: &str) -> anyhow::Result<()> {
    let output = command
        .output()
        .with_context(|| format!("{what}: could not start it (is it installed?)"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{what} failed: {}", stderr.trim());
    }
    Ok(())
}

/// A GET of `url`, on a runtime of its own so it can be asked from any
/// thread.
fn http_get(url: &str) -> anyhow::Result<Vec<u8>> {
    let url = url.to_owned();
    std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let response = reqwest::get(&url)
                .await
                .with_context(|| format!("fetching {url}"))?
                .error_for_status()
                .with_context(|| format!("fetching {url}"))?;
            Ok(response.bytes().await?.to_vec())
        })
    })
    .join()
    .map_err(|_| anyhow!("the download thread panicked"))?
}

/// Puts a copy of `src` at `dst`, replacing what is there, through a staged
/// copy beside it so a failed copy leaves the old one.
fn replace_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", dst.display()))?;
    std::fs::create_dir_all(parent)?;
    let stage = parent.join(format!(
        ".stage-{}-{}",
        dst.file_name().unwrap_or_default().to_string_lossy(),
        rebon_types::wall_clock_ms()
    ));
    copy_dir_recursive(src, &stage)?;
    remove_dir_if_present(dst)?;
    std::fs::rename(&stage, dst).with_context(|| format!("moving {} into place", dst.display()))?;
    Ok(())
}

fn remove_dir_if_present(dir: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", dir.display())),
    }
}

/// Whether what was typed after `plugin install` names a marketplace
/// plugin (`plugin@marketplace`) rather than a path, archive or alias.
pub fn is_marketplace_spec(raw: &str, cwd: &Path) -> bool {
    let Some((plugin, marketplace)) = raw.rsplit_once('@') else {
        return false;
    };
    rebon_harness::rebon_plugin_package::marketplace::valid_name(plugin)
        && rebon_harness::rebon_plugin_package::marketplace::valid_name(marketplace)
        && !cwd.join(raw).exists()
}

/// The marketplaces, one per line: name, plugin count, source.
pub fn format_marketplaces(views: &[MarketplaceView]) -> String {
    if views.is_empty() {
        return "no marketplaces; add one with `/plugin marketplace add owner/repo`".to_owned();
    }
    views
        .iter()
        .map(|view| {
            let tag = if view.builtin { " (built in)" } else { "" };
            match &view.error {
                Some(error) => format!("{}{tag}  unreadable: {error}  {}", view.name, view.source),
                None => format!(
                    "{}{tag}  {} plugins  {}",
                    view.name, view.plugins, view.source
                ),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The catalog, one plugin per line: `id  kind  [installed]  description`,
/// for one marketplace or all.
pub fn format_catalog(catalog: &Catalog, marketplace: Option<&str>) -> String {
    let rows: Vec<String> = catalog
        .plugins
        .iter()
        .filter(|entry| marketplace.is_none_or(|wanted| wanted == entry.marketplace))
        .map(|entry| {
            let kind = entry.shape.unwrap_or(entry.source_kind);
            let installed = if entry.installed { "  [installed]" } else { "" };
            let description = entry.entry.description.as_deref().unwrap_or("");
            let asks = match &entry.container {
                Some(request) if !request.is_empty() => {
                    format!("[asks: {}] ", describe_request(request))
                }
                _ => String::new(),
            };
            format!("{}  {kind}{installed}  {asks}{description}", entry.id)
        })
        .collect();
    if rows.is_empty() {
        return match marketplace {
            Some(name) => format!("the {name} marketplace lists no plugins"),
            None => "no marketplace lists any plugins".to_owned(),
        };
    }
    rows.join("\n")
}

/// What an install or uninstall did, in one line.
pub fn format_install(action: &str, install: &MarketplaceInstall) -> String {
    let id = plugin_id(&install.plugin, &install.marketplace);
    let what = match install.kind {
        InstallKind::Mod => format!("as a mod in {}", install.location.display()),
        InstallKind::Package => format!("as package {}", install.location.display()),
    };
    let version = install
        .version
        .as_deref()
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    format!(
        "{action} {id}{version} {what}{}",
        if action != "installed" {
            String::new()
        } else if install.kind == InstallKind::Package && install.kernel_plugins.is_empty() {
            // Skills, commands, files: what rebon reads, nothing that runs.
            "\nnothing of it runs: it adds skills and files rebon reads".to_owned()
        } else {
            format!("\n{}", describe_grant(&install.granted))
        }
    )
}

/// What a package asks for beyond its own files, in a few words.
pub fn describe_request(
    request: &rebon_harness::rebon_plugin_package::container::ContainerRequest,
) -> String {
    let mut parts = Vec::new();
    if rebon_harness::rebon_plugin_package::container::admits_any_host(&request.network) {
        parts.push("network to any host".to_owned());
    } else if !request.network.is_empty() {
        parts.push(format!("network to {}", request.network.join(", ")));
    }
    if !request.env.is_empty() {
        parts.push(format!("reads {}", request.env.join(", ")));
    }
    parts.join("; ")
}

/// What a container was granted, in one line a person reads before trusting
/// it: everything beyond its own files is named.
pub fn describe_grant(
    grant: &rebon_harness::rebon_plugin_package::container::ContainerGrant,
) -> String {
    let mut parts = vec!["runs in a container: its own files only, no processes".to_owned()];
    parts.push(if grant.network.is_empty() {
        "no network".to_owned()
    } else if rebon_harness::rebon_plugin_package::container::admits_any_host(&grant.network) {
        "network to any host".to_owned()
    } else {
        format!("network to {}", grant.network.join(", "))
    });
    if !grant.env.is_empty() {
        parts.push(format!("reads {}", grant.env.join(", ")));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn write_mod(dir: &Path, name: &str) {
        write(
            &dir.join(".claude-plugin/plugin.json"),
            &json!({ "name": name, "version": "0.1.0" }).to_string(),
        );
        write(
            &dir.join("hooks/hooks.json"),
            r#"{ "modules": ["./register.ts"] }"#,
        );
        write(
            &dir.join("hooks/register.ts"),
            "export const register = (on) => { on('session.start', ($, e, next) => next(e)); };\n",
        );
    }

    fn write_package(dir: &Path, name: &str) {
        write(
            &dir.join("rebon-plugin.json"),
            &json!({
                "name": name,
                "version": "1.0.0",
                "capabilities": { "kernelPlugins": { name: { "entry": "plugin.mjs", "commands": ["hi"] } } }
            })
            .to_string(),
        );
        write(&dir.join("plugin.mjs"), "export function activate() {}\n");
    }

    fn write_package_with_config(dir: &Path, name: &str) {
        write(
            &dir.join("rebon-plugin.json"),
            &json!({
                "name": name,
                "version": "1.0.0",
                "capabilities": { "kernelPlugins": { name: { "entry": "plugin.mjs" } } },
                "metadata": { "kernelPluginConfig": { name: { "allowParallelInProgress": false } } }
            })
            .to_string(),
        );
        write(&dir.join("plugin.mjs"), "export function activate() {}\n");
    }

    #[test]
    fn a_package_s_container_request_is_granted_on_install_and_gone_on_uninstall() {
        let fx = fixture();
        let market = fx.work.join("market");
        write(
            &market.join("search/rebon-plugin.json"),
            &json!({
                "name": "search",
                "version": "1.0.0",
                "capabilities": { "kernelPlugins": { "search": { "entry": "plugin.mjs" } } },
                "container": { "network": ["api.exa.ai"], "env": ["EXA_API_KEY"] }
            })
            .to_string(),
        );
        write(
            &market.join("search/plugin.mjs"),
            "export function activate() {}\n",
        );
        write(
            &market.join(MARKETPLACE_MANIFEST),
            &json!({ "name": "m", "plugins": [{ "name": "search", "source": "./search" }] })
                .to_string(),
        );
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let install = manager.install("search@m", PluginScope::User).unwrap();
        assert_eq!(install.granted.network, vec!["api.exa.ai".to_owned()]);
        let said = format_install("installed", &install);
        assert!(said.contains("network to api.exa.ai"), "{said}");
        assert!(said.contains("reads EXA_API_KEY"), "{said}");
        let grants =
            rebon_harness::rebon_plugin_package::container::ContainerGrants::load(&fx.home);
        assert_eq!(grants.get("pkg-search").env, vec!["EXA_API_KEY".to_owned()]);

        // Its container kept data; uninstalling takes the grant and the data.
        let data = rebon_harness::rebon_plugin_package::container::container_data_dir(
            &fx.home,
            "pkg-search",
        );
        std::fs::create_dir_all(&data).unwrap();
        write(&data.join("cache.json"), "{}");
        manager.uninstall("search@m").unwrap();
        assert!(!data.exists(), "the container's data went with it");
        assert!(
            !rebon_harness::rebon_plugin_package::container::ContainerGrants::load(&fx.home)
                .containers
                .contains_key("pkg-search")
        );
    }

    #[test]
    fn the_catalog_says_what_a_package_will_ask_for_before_it_is_installed() {
        let fx = fixture();
        let market = fx.work.join("market");
        write(
            &market.join("search/rebon-plugin.json"),
            &json!({
                "name": "search",
                "version": "1.0.0",
                "capabilities": { "kernelPlugins": { "search": { "entry": "plugin.mjs" } } },
                "container": { "network": ["api.exa.ai"] }
            })
            .to_string(),
        );
        write(
            &market.join("search/plugin.mjs"),
            "export function activate() {}\n",
        );
        write_mod(&market.join("radar"), "radar");
        write(
            &market.join(MARKETPLACE_MANIFEST),
            &json!({ "name": "m", "plugins": [
                { "name": "search", "source": "./search", "description": "Search." },
                { "name": "radar", "source": "./radar" },
                { "name": "far", "source": { "source": "github", "repo": "a/b" } }
            ] })
            .to_string(),
        );
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let catalog = manager.browse().unwrap();
        let find = |id: &str| catalog.plugins.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(
            find("search@m").container.as_ref().unwrap().network,
            vec!["api.exa.ai".to_owned()]
        );
        assert!(find("radar@m").container.as_ref().unwrap().is_empty());
        assert!(find("far@m").container.is_none(), "not fetched, not known");
        let text = format_catalog(&catalog, Some("m"));
        assert!(
            text.contains("search@m  package  [asks: network to api.exa.ai] Search."),
            "{text}"
        );
    }

    #[test]
    fn an_uninstalled_mod_leaves_no_container_data() {
        let fx = fixture();
        marketplace(&fx.work.join("market"), "m");
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let install = manager.install("radar@m", PluginScope::User).unwrap();
        assert!(format_install("installed", &install).contains("no network"));
        let data = rebon_harness::rebon_plugin_package::container::container_data_dir(
            &fx.home,
            "mod-radar",
        );
        std::fs::create_dir_all(&data).unwrap();
        manager.uninstall("radar@m").unwrap();
        assert!(!data.exists());
    }

    #[test]
    fn a_package_naming_its_kernel_config_is_listed_with_it() {
        let fx = fixture();
        let market = fx.work.join("market");
        write_package_with_config(&market.join("todo"), "todo");
        write(
            &market.join(MARKETPLACE_MANIFEST),
            &json!({ "name": "m", "plugins": [{ "name": "todo", "source": "./todo" }] })
                .to_string(),
        );
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        manager.install("todo@m", PluginScope::User).unwrap();
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(rebon_config::paths::config_json_path(&fx.home)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            config["kernelPlugins"]["plugins"],
            json!([{ "id": "todo", "config": { "allowParallelInProgress": false } }])
        );
    }

    /// A marketplace folder: a mod, a package, a skills-only Claude plugin,
    /// and a command source.
    fn marketplace(root: &Path, name: &str) {
        write_mod(&root.join("mods/radar"), "radar");
        write_package(&root.join("plugins/greeter"), "greeter");
        write(
            &root.join("skills-only/.claude-plugin/plugin.json"),
            r#"{"name":"skills-only"}"#,
        );
        write(
            &root.join(MARKETPLACE_MANIFEST),
            &json!({
                "name": name,
                "owner": { "name": "tests" },
                "description": "for tests",
                "metadata": { "pluginRoot": "./plugins" },
                "plugins": [
                    { "name": "radar", "source": "./mods/radar", "version": "0.1.0" },
                    { "name": "greeter", "source": "greeter" },
                    { "name": "skills-only", "source": "./skills-only" },
                    { "name": "shell", "source": { "source": "command", "command": "echo nope" } }
                ]
            })
            .to_string(),
        );
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        work: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Fixture {
            _dir: dir,
            home,
            work,
        }
    }

    fn manager(fx: &Fixture) -> MarketplaceManager {
        MarketplaceManager::new(fx.home.clone(), fx.work.clone()).with_builtin(None)
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn file_url(path: &Path) -> String {
        format!(
            "file:///{}",
            path.to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    }

    #[test]
    fn a_local_marketplace_is_added_under_its_own_name_and_read_in_place() {
        let fx = fixture();
        marketplace(&fx.work.join("market"), "team-market");
        let manager = manager(&fx);
        let view = manager.add("./market").unwrap();
        assert_eq!(view.name, "team-market");
        assert_eq!(view.plugins, 4);
        assert_eq!(view.description.as_deref(), Some("for tests"));
        assert!(
            !marketplace_copy_dir(&fx.home, "team-market").exists(),
            "a local one is not copied"
        );
        assert_eq!(manager.marketplaces().unwrap().len(), 1);
        assert!(
            manager.add("./market").is_ok(),
            "adding the same source again is fine"
        );
    }

    #[test]
    fn a_name_taken_by_another_source_or_reserved_is_refused() {
        let fx = fixture();
        marketplace(&fx.work.join("one"), "same");
        marketplace(&fx.work.join("two"), "same");
        marketplace(&fx.work.join("reserved"), "claude-plugins-official");
        let manager = manager(&fx);
        manager.add("./one").unwrap();
        let error = manager.add("./two").unwrap_err().to_string();
        assert!(error.contains("already added"), "{error}");
        let error = manager.add("./reserved").unwrap_err().to_string();
        assert!(error.contains("reserved"), "{error}");
        assert!(manager.add("./missing").is_err());
    }

    #[test]
    fn browse_lists_every_entry_with_its_shape_and_what_is_installed() {
        let fx = fixture();
        marketplace(&fx.work.join("market"), "m");
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        manager.install("radar@m", PluginScope::User).unwrap();
        let catalog = manager.browse().unwrap();
        let rows: Vec<(&str, Option<&str>, &str, bool)> = catalog
            .plugins
            .iter()
            .map(|entry| {
                (
                    entry.id.as_str(),
                    entry.shape,
                    entry.source_kind,
                    entry.installed,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("radar@m", Some("mod"), "path", true),
                ("greeter@m", Some("package"), "path", false),
                ("skills-only@m", Some("unsupported"), "path", false),
                ("shell@m", None, "command", false),
            ]
        );
        assert!(catalog.plugins[2]
            .unsupported_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("hooks module")));
    }

    #[test]
    fn a_mod_installs_under_mods_where_it_loads_and_uninstalls_from_there() {
        let fx = fixture();
        marketplace(&fx.work.join("market"), "m");
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let install = manager.install("radar", PluginScope::User).unwrap();
        let target = fx.home.join(rebon_types::MODS_DIR).join("radar");
        assert_eq!(install.kind, InstallKind::Mod);
        assert_eq!(install.location, target);
        assert!(target.join("hooks/register.ts").is_file());
        assert!(
            rebon_harness::rebon_plugin_package::discover_mod_dirs(&fx.home, None, |_| None)
                .contains(&target),
            "a mod under mods/ is one the plane discovers with no configuration"
        );
        // Installing again replaces it.
        write(
            &fx.work.join("market/mods/radar/hooks/extra.ts"),
            "export {};\n",
        );
        manager.install("radar@m", PluginScope::User).unwrap();
        assert!(target.join("hooks/extra.ts").is_file());
        manager.uninstall("radar@m").unwrap();
        assert!(!target.exists());
        assert!(manager.installs().unwrap().by_id.is_empty());
        assert!(
            manager.uninstall("radar@m").is_err(),
            "nothing left to uninstall"
        );
    }

    #[test]
    fn a_package_installs_into_the_store_and_is_listed_to_load() {
        let fx = fixture();
        marketplace(&fx.work.join("market"), "m");
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let install = manager.install("greeter@m", PluginScope::User).unwrap();
        assert_eq!(install.kind, InstallKind::Package);
        assert_eq!(install.kernel_plugins, vec!["greeter".to_owned()]);
        let store = PluginStore::new(fx.home.clone(), fx.work.clone());
        assert!(store
            .load_state(PluginScope::User)
            .unwrap()
            .plugins
            .iter()
            .any(|record| record.name == "greeter"));
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(rebon_config::paths::config_json_path(&fx.home)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            config["kernelPlugins"]["plugins"],
            json!([{ "id": "greeter" }])
        );
        manager.uninstall("greeter").unwrap();
        assert!(!store
            .load_state(PluginScope::User)
            .unwrap()
            .plugins
            .iter()
            .any(|record| record.name == "greeter"));
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(rebon_config::paths::config_json_path(&fx.home)).unwrap(),
        )
        .unwrap();
        assert_eq!(config["kernelPlugins"]["plugins"], json!([]));
    }

    #[test]
    fn what_rebon_does_not_install_is_refused_with_why() {
        let fx = fixture();
        marketplace(&fx.work.join("market"), "m");
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let error = manager
            .install("skills-only@m", PluginScope::User)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a plugin Rebon installs"), "{error}");
        let error = manager
            .install("shell@m", PluginScope::User)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not run"), "{error}");
        let error = manager
            .install("radar@m", PluginScope::Project)
            .unwrap_err()
            .to_string();
        assert!(error.contains("for the user"), "{error}");
        assert!(manager.install("ghost", PluginScope::User).is_err());
        assert!(manager.install("radar@nowhere", PluginScope::User).is_err());
        assert!(
            manager.installs().unwrap().by_id.is_empty(),
            "nothing half-installed"
        );
    }

    #[test]
    fn a_plugin_two_marketplaces_list_is_named_by_its_marketplace() {
        let fx = fixture();
        marketplace(&fx.work.join("a"), "alpha");
        marketplace(&fx.work.join("b"), "beta");
        let manager = manager(&fx);
        manager.add("./a").unwrap();
        manager.add("./b").unwrap();
        let error = manager
            .install("radar", PluginScope::User)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("radar@alpha") && error.contains("radar@beta"),
            "{error}"
        );
        assert_eq!(
            manager
                .install("radar@beta", PluginScope::User)
                .unwrap()
                .marketplace,
            "beta"
        );
    }

    #[test]
    fn rebons_own_marketplace_is_always_listed_and_never_removed() {
        let fx = fixture();
        let builtin = fx.work.join("builtin");
        marketplace(&builtin, BUILTIN_MARKETPLACE);
        let manager =
            MarketplaceManager::new(fx.home.clone(), fx.work.clone()).with_builtin(Some(builtin));
        let views = manager.marketplaces().unwrap();
        assert_eq!(views[0].name, BUILTIN_MARKETPLACE);
        assert!(views[0].builtin);
        assert!(manager.remove(BUILTIN_MARKETPLACE).is_err());
        assert!(manager.update(Some(BUILTIN_MARKETPLACE)).is_err());
        assert_eq!(
            manager
                .install("radar@rebon", PluginScope::User)
                .unwrap()
                .marketplace,
            "rebon"
        );
    }

    #[test]
    fn a_git_marketplace_is_cloned_updated_and_removed() {
        if !git_available() {
            return;
        }
        let fx = fixture();
        let repo = fx.work.join("repo");
        marketplace(&repo, "git-market");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "one"]);
        let manager = manager(&fx);
        let view = manager.add(&file_url(&repo)).unwrap();
        assert_eq!(view.name, "git-market");
        let copy = marketplace_copy_dir(&fx.home, "git-market");
        assert!(
            copy.join(MARKETPLACE_MANIFEST).is_file(),
            "cloned into its copy"
        );
        assert!(!copy.join(".git").exists(), "the copy keeps no checkout");
        assert_eq!(
            manager
                .install("radar@git-market", PluginScope::User)
                .unwrap()
                .kind,
            InstallKind::Mod
        );

        write_mod(&repo.join("mods/second"), "second");
        let manifest_path = repo.join(MARKETPLACE_MANIFEST);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["plugins"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "name": "second", "source": "./mods/second" }));
        std::fs::write(&manifest_path, manifest.to_string()).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "two"]);
        let outcomes = manager.update(None).unwrap();
        assert_eq!(outcomes, vec![("git-market".to_owned(), Ok(()))]);
        assert_eq!(manager.marketplaces().unwrap()[0].plugins, 5);

        manager.remove("git-market").unwrap();
        assert!(!copy.exists());
        assert!(manager.marketplaces().unwrap().is_empty());
        assert!(
            fx.home.join(rebon_types::MODS_DIR).join("radar").is_dir(),
            "what was installed from it stays"
        );
    }

    #[test]
    fn a_git_subdir_source_installs_the_folder_inside_the_repository() {
        if !git_available() {
            return;
        }
        let fx = fixture();
        let mono = fx.work.join("mono");
        write_mod(&mono.join("tools/clock"), "clock");
        git(&mono, &["init", "-q", "-b", "main"]);
        git(&mono, &["add", "."]);
        git(&mono, &["commit", "-q", "-m", "one"]);
        let market = fx.work.join("market");
        write(
            &market.join(MARKETPLACE_MANIFEST),
            &json!({
                "name": "remote-market",
                "plugins": [
                    { "name": "clock", "source": { "source": "git-subdir", "url": file_url(&mono), "path": "tools/clock" } },
                    { "name": "escape", "source": { "source": "git-subdir", "url": file_url(&mono), "path": "../x" } },
                    { "name": "whole", "source": { "source": "url", "url": file_url(&mono.join("tools/clock")) } }
                ]
            })
            .to_string(),
        );
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let install = manager.install("clock", PluginScope::User).unwrap();
        assert!(install.location.join("hooks/register.ts").is_file());
        assert!(manager.install("escape", PluginScope::User).is_err());
        assert!(
            manager.install("whole", PluginScope::User).is_err(),
            "not a repository: the clone fails and says so"
        );
    }

    #[test]
    fn a_marketplace_spec_is_plugin_at_marketplace_and_not_a_path() {
        let fx = fixture();
        assert!(is_marketplace_spec("radar@claude-code-mods", &fx.work));
        assert!(!is_marketplace_spec("./plugin", &fx.work));
        assert!(!is_marketplace_spec("rust-lsp", &fx.work));
        assert!(!is_marketplace_spec("a b@c", &fx.work));
        std::fs::create_dir_all(fx.work.join("odd@folder")).unwrap();
        assert!(
            !is_marketplace_spec("odd@folder", &fx.work),
            "an existing folder is a path"
        );
    }

    #[test]
    fn listings_read_one_line_per_marketplace_and_plugin() {
        let fx = fixture();
        marketplace(&fx.work.join("market"), "m");
        let manager = manager(&fx);
        manager.add("./market").unwrap();
        let listed = format_marketplaces(&manager.marketplaces().unwrap());
        assert!(listed.starts_with("m  4 plugins  "), "{listed}");
        let install = manager.install("radar@m", PluginScope::User).unwrap();
        assert!(format_install("installed", &install)
            .starts_with("installed radar@m 0.1.0 as a mod in "));
        let catalog = format_catalog(&manager.browse().unwrap(), Some("m"));
        assert!(
            catalog
                .lines()
                .next()
                .unwrap()
                .starts_with("radar@m  mod  [installed]"),
            "{catalog}"
        );
        assert_eq!(catalog.lines().count(), 4);
        assert_eq!(
            format_catalog(&manager.browse().unwrap(), Some("other")),
            "the other marketplace lists no plugins"
        );
        assert!(format_marketplaces(&[]).contains("marketplace add"));
    }

    #[test]
    fn a_relative_source_in_a_catalog_fetched_by_itself_has_nowhere_to_come_from() {
        let fx = fixture();
        let file = fx.work.join("only/.claude-plugin/marketplace.json");
        write(
            &file,
            &json!({ "name": "lonely", "plugins": [{ "name": "a", "source": "./a" }] }).to_string(),
        );
        let loaded = Loaded {
            name: "lonely".into(),
            root: None,
            manifest: MarketplaceManifest::read(&file).unwrap(),
        };
        let error = materialize(&loaded, &PluginSource::Relative("./a".into()), &fx.work)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fetched as a file"), "{error}");
    }
}
