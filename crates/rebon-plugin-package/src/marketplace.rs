//! Claude Code plugin marketplaces, read.
//!
//! A marketplace is a catalog: a `.claude-plugin/marketplace.json` naming
//! plugins and where each one comes from. Rebon reads the Claude Code format
//! as it is written, so a marketplace published for Claude Code is one Rebon
//! can browse and install from.
//!
//! This module is the half that touches no network: what a marketplace and
//! its entries say ([`MarketplaceManifest`], [`MarketplaceEntry`],
//! [`PluginSource`]), what `marketplace add` makes of what the person typed
//! ([`parse_marketplace_input`]), the marketplaces this machine knows
//! ([`KnownMarketplaces`], `plugins/known_marketplaces.json` under the config
//! home), what was installed from them ([`MarketplaceInstalls`]), and what a
//! fetched plugin folder is ([`PluginShape`]). Fetching and installing are the
//! session runtime's.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::PLUGIN_MANIFEST_FILE;

/// Where a marketplace's catalog sits under its root.
pub const MARKETPLACE_MANIFEST: &str = ".claude-plugin/marketplace.json";

/// The marketplace that ships with Rebon, always known, never removed.
pub const BUILTIN_MARKETPLACE: &str = "rebon";

/// Names no marketplace added here may take: Claude Code's reserved ones,
/// the words it keeps for package managers, and Rebon's own.
const RESERVED_NAMES: &[&str] = &[
    "claude-code-marketplace",
    "claude-code-plugins",
    "claude-plugins-official",
    "anthropic-marketplace",
    "anthropic-plugins",
    "agent-skills",
    "anthropic-agent-skills",
    "claude-community",
    "claude-plugins-community",
    "healthcare",
    "anthropic-plugin-directory",
    "claude-plugin-directory",
    "npm",
    "pip",
    "uv",
    "cargo",
    "github",
    "gh",
    BUILTIN_MARKETPLACE,
];

/// Where a marketplace's catalog comes from, as Claude Code spells it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum MarketplaceSource {
    /// A GitHub repository, `owner/repo`.
    Github {
        repo: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
    },
    /// Any git repository.
    Git {
        url: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
    },
    /// A `marketplace.json` fetched by itself: its relative plugin sources
    /// have no folder to resolve against.
    Url { url: String },
    /// A folder holding `.claude-plugin/marketplace.json`, read in place.
    Directory { path: PathBuf },
    /// A `marketplace.json` file, read in place.
    File { path: PathBuf },
}

impl MarketplaceSource {
    /// The git URL to clone, for the sources that are repositories.
    pub fn git_url(&self) -> Option<String> {
        match self {
            Self::Github { repo, .. } => Some(format!("https://github.com/{repo}.git")),
            Self::Git { url, .. } => Some(url.clone()),
            _ => None,
        }
    }

    pub fn git_ref(&self) -> Option<&str> {
        match self {
            Self::Github { git_ref, .. } | Self::Git { git_ref, .. } => git_ref.as_deref(),
            _ => None,
        }
    }

    /// Whether the catalog is read where it lies rather than from a copy.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Directory { .. } | Self::File { .. })
    }

    /// One line saying where it comes from.
    pub fn describe(&self) -> String {
        match self {
            Self::Github { repo, git_ref } => match git_ref {
                Some(r) => format!("github:{repo}#{r}"),
                None => format!("github:{repo}"),
            },
            Self::Git { url, git_ref } => match git_ref {
                Some(r) => format!("{url}#{r}"),
                None => url.clone(),
            },
            Self::Url { url } => url.clone(),
            Self::Directory { path } | Self::File { path } => path.display().to_string(),
        }
    }
}

/// Splits `text#ref` (or, for GitHub shorthand, `text@ref`).
fn split_ref(text: &str, at_too: bool) -> (&str, Option<String>) {
    if let Some((head, r)) = text.split_once('#') {
        return (head, Some(r.to_owned()).filter(|r| !r.is_empty()));
    }
    if at_too {
        if let Some((head, r)) = text.split_once('@') {
            return (head, Some(r.to_owned()).filter(|r| !r.is_empty()));
        }
    }
    (text, None)
}

fn is_github_shorthand(text: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    matches!(text.split_once('/'), Some((owner, repo)) if ok(owner) && ok(repo) && !repo.contains('/'))
}

/// What `marketplace add` makes of what was typed, by Claude Code's rules:
/// `owner/repo[#ref]` is GitHub; a `.git` URL, an `ssh` remote, an Azure
/// `/_git/` URL or a bare GitHub / GitLab project URL is git; any other URL
/// is a `marketplace.json` to fetch; a path is a folder, or a `.json` file.
pub fn parse_marketplace_input(
    raw: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> anyhow::Result<MarketplaceSource> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("name a marketplace: owner/repo, a git URL, a URL to a marketplace.json, or a path");
    }
    let looks_like_path = text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with(".\\")
        || text.starts_with("..\\")
        || text.starts_with("~/")
        || text.starts_with('/')
        || Path::new(text).is_absolute();
    if looks_like_path {
        let path = match (text.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            (Some(_), None) => bail!("~ does not name a home folder here"),
            _ => cwd.join(text),
        };
        let is_json = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        return Ok(if is_json {
            MarketplaceSource::File { path }
        } else {
            MarketplaceSource::Directory { path }
        });
    }
    if text.starts_with("file://") {
        let (url, git_ref) = split_ref(text, false);
        return Ok(MarketplaceSource::Git {
            url: url.to_owned(),
            git_ref,
        });
    }
    if let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    {
        let (url, git_ref) = split_ref(text, false);
        let (host_path, _) = split_ref(rest, false);
        let (host, path) = host_path.split_once('/').unwrap_or((host_path, ""));
        let segments: Vec<&str> = path
            .trim_end_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if url.ends_with(".git") || path.contains("/_git/") || path.starts_with("_git/") {
            return Ok(MarketplaceSource::Git {
                url: url.to_owned(),
                git_ref,
            });
        }
        if matches!(host, "github.com" | "gitlab.com") && segments.len() == 2 {
            return Ok(MarketplaceSource::Git {
                url: format!("{}.git", url.trim_end_matches('/')),
                git_ref,
            });
        }
        if git_ref.is_some() {
            bail!("a ref (#...) goes with a git repository, not a marketplace.json URL");
        }
        return Ok(MarketplaceSource::Url {
            url: url.to_owned(),
        });
    }
    // An ssh remote: `user@host:path`.
    if let Some((user_host, path)) = text.split_once(':') {
        if user_host.contains('@') && !user_host.contains('/') && !path.starts_with("//") {
            let (url, git_ref) = split_ref(text, false);
            return Ok(MarketplaceSource::Git {
                url: url.to_owned(),
                git_ref,
            });
        }
    }
    let (repo, git_ref) = split_ref(text, true);
    if is_github_shorthand(repo) {
        return Ok(MarketplaceSource::Github {
            repo: repo.to_owned(),
            git_ref,
        });
    }
    bail!("{text:?} is not owner/repo, a git URL, a URL to a marketplace.json, or a path")
}

/// Whether `name` is one a marketplace or a plugin may have: letters,
/// digits, `.`, `_`, `-`, starting with a letter or digit, no `..`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.contains("..")
}

/// Why a marketplace added here may not take `name`, when it may not.
pub fn refused_marketplace_name(name: &str) -> Option<String> {
    if !valid_name(name) {
        return Some(format!(
            "{name:?} is not a marketplace name (letters, digits, '.', '_', '-', starting with a letter or digit)"
        ));
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED_NAMES.contains(&lower.as_str()) || lower.starts_with("claudeai-") {
        return Some(format!("{name:?} is a reserved marketplace name"));
    }
    None
}

// ---- what a marketplace says ----------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MarketplaceOwner {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The folder a bare plugin `source` name resolves under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_root: Option<String>,
}

/// Where one plugin comes from, as Claude Code spells it: a path inside the
/// marketplace, or one of the remote kinds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginSource {
    Relative(String),
    Remote(RemoteSource),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum RemoteSource {
    Github {
        repo: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    },
    Url {
        url: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    },
    GitSubdir {
        url: String,
        path: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    },
    Npm {
        package: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        registry: Option<String>,
    },
    Archive {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    /// A shell command that prints a folder. Read so the catalog lists it;
    /// never run: installing it is refused.
    Command { command: String },
}

impl PluginSource {
    /// One word for the kind of source, as a listing shows it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Relative(_) => "path",
            Self::Remote(RemoteSource::Github { .. }) => "github",
            Self::Remote(RemoteSource::Url { .. }) => "git",
            Self::Remote(RemoteSource::GitSubdir { .. }) => "git-subdir",
            Self::Remote(RemoteSource::Npm { .. }) => "npm",
            Self::Remote(RemoteSource::Archive { .. }) => "archive",
            Self::Remote(RemoteSource::Command { .. }) => "command",
        }
    }
}

/// One plugin a marketplace lists.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceEntry {
    pub name: String,
    pub source: PluginSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<Value>,
    /// Rebon's own: the composition config a package from npm cannot start
    /// without, probed with and listed with. Claude Code ignores the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebon_config: Option<Value>,
}

/// A marketplace's catalog, as read: the entries that parsed, and why each
/// one that did not was left out (one odd entry does not hide the rest).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<MarketplaceOwner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MarketplaceMetadata>,
    pub plugins: Vec<MarketplaceEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

impl MarketplaceManifest {
    /// Reads a `marketplace.json` text.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: Value = serde_json::from_str(text).context("marketplace.json is not JSON")?;
        let object = raw
            .as_object()
            .ok_or_else(|| anyhow!("marketplace.json is not an object"))?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("marketplace.json names no marketplace (`name`)"))?
            .to_owned();
        if !valid_name(&name) {
            bail!("{name:?} is not a marketplace name");
        }
        let field = |key: &str| object.get(key).cloned().unwrap_or(Value::Null);
        let mut manifest = Self {
            name,
            owner: serde_json::from_value(field("owner")).ok(),
            description: object
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            version: object
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned),
            metadata: serde_json::from_value(field("metadata")).ok(),
            plugins: Vec::new(),
            skipped: Vec::new(),
        };
        let Some(plugins) = object.get("plugins").and_then(Value::as_array) else {
            bail!("marketplace.json lists no `plugins`");
        };
        for (index, raw) in plugins.iter().enumerate() {
            let label = raw
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("#{index}"));
            match serde_json::from_value::<MarketplaceEntry>(raw.clone()) {
                Ok(entry) if valid_name(&entry.name) => manifest.plugins.push(entry),
                Ok(entry) => manifest
                    .skipped
                    .push(format!("{}: not a plugin name", entry.name)),
                Err(error) => manifest.skipped.push(format!("{label}: {error}")),
            }
        }
        Ok(manifest)
    }

    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn entry(&self, plugin: &str) -> Option<&MarketplaceEntry> {
        self.plugins.iter().find(|entry| entry.name == plugin)
    }

    fn plugin_root(&self) -> Option<&str> {
        self.metadata.as_ref()?.plugin_root.as_deref()
    }
}

/// The folder a relative plugin source names inside a marketplace rooted at
/// `root`: `./path` from the root, a bare name under `metadata.pluginRoot`;
/// never above the root.
pub fn relative_plugin_dir(
    root: &Path,
    manifest: &MarketplaceManifest,
    source: &str,
) -> anyhow::Result<PathBuf> {
    let source = source.replace('\\', "/");
    let relative: PathBuf = if source == "." {
        PathBuf::new()
    } else if let Some(rest) = source.strip_prefix("./") {
        PathBuf::from(rest)
    } else if let Some(plugin_root) = manifest.plugin_root() {
        Path::new(plugin_root.trim_start_matches("./")).join(&source)
    } else {
        bail!("plugin source {source:?} must start with ./ (the marketplace sets no pluginRoot)");
    };
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        bail!("plugin source {source:?} leaves the marketplace's folder");
    }
    Ok(root.join(relative))
}

// ---- the marketplaces this machine knows -----------------------------------

/// One added marketplace: where it comes from and where its copy is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownMarketplace {
    pub source: MarketplaceSource,
    /// The clone or download; the folder itself for a local source.
    pub install_location: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated_ms: Option<u64>,
}

impl KnownMarketplace {
    /// Where its `marketplace.json` is.
    pub fn manifest_path(&self) -> PathBuf {
        match &self.source {
            MarketplaceSource::File { path } => path.clone(),
            _ => self.install_location.join(MARKETPLACE_MANIFEST),
        }
    }

    /// The folder its relative plugin sources resolve under; `None` for a
    /// catalog fetched by itself.
    pub fn root(&self) -> Option<PathBuf> {
        match &self.source {
            MarketplaceSource::Url { .. } => None,
            MarketplaceSource::File { path } => Some(path.parent()?.parent()?.to_path_buf()),
            _ => Some(self.install_location.clone()),
        }
    }
}

/// `plugins/known_marketplaces.json` under the config home.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KnownMarketplaces {
    #[serde(flatten)]
    pub by_name: BTreeMap<String, KnownMarketplace>,
}

/// The folder marketplace copies and records live in.
pub fn plugins_dir(config_home: &Path) -> PathBuf {
    config_home.join("plugins")
}

/// Where the copy of marketplace `name` goes.
pub fn marketplace_copy_dir(config_home: &Path, name: &str) -> PathBuf {
    plugins_dir(config_home).join("marketplaces").join(name)
}

impl KnownMarketplaces {
    pub fn path(config_home: &Path) -> PathBuf {
        plugins_dir(config_home).join("known_marketplaces.json")
    }

    pub fn load(config_home: &Path) -> anyhow::Result<Self> {
        read_json_or_default(&Self::path(config_home))
    }

    pub fn save(&self, config_home: &Path) -> anyhow::Result<()> {
        write_json(&Self::path(config_home), self)
    }
}

// ---- what was installed from them ------------------------------------------

/// What a marketplace plugin installed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallKind {
    /// A Claude Code mod, copied under the config home's `mods/`, where it
    /// loads with no configuration.
    Mod,
    /// A Rebon package, in the plugin store.
    Package,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceInstall {
    pub marketplace: String,
    pub plugin: String,
    pub kind: InstallKind,
    /// The mod folder, or the package's name in the store.
    pub location: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The kernel plugin ids the install listed in `kernelPlugins.plugins`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_plugins: Vec<String>,
    pub installed_at_ms: u64,
    /// What its container was granted beyond its own files: network hosts
    /// and variables (see [`crate::container`]).
    #[serde(
        default,
        skip_serializing_if = "crate::container::ContainerRequest::is_empty"
    )]
    pub granted: crate::container::ContainerGrant,
}

/// `name@marketplace`, the id an install is known by.
pub fn plugin_id(plugin: &str, marketplace: &str) -> String {
    format!("{plugin}@{marketplace}")
}

/// `plugins/marketplace_installs.json` under the config home.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MarketplaceInstalls {
    #[serde(flatten)]
    pub by_id: BTreeMap<String, MarketplaceInstall>,
}

impl MarketplaceInstalls {
    pub fn path(config_home: &Path) -> PathBuf {
        plugins_dir(config_home).join("marketplace_installs.json")
    }

    pub fn load(config_home: &Path) -> anyhow::Result<Self> {
        read_json_or_default(&Self::path(config_home))
    }

    pub fn save(&self, config_home: &Path) -> anyhow::Result<()> {
        write_json(&Self::path(config_home), self)
    }
}

fn read_json_or_default<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not readable", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    rebon_session::write_file_atomically(path, &data)?;
    Ok(())
}

// ---- what a fetched plugin folder is ---------------------------------------

/// What kind of plugin a folder holds, as far as Rebon installs plugins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "kebab-case")]
pub enum PluginShape {
    /// A Claude Code mod: `.claude-plugin/plugin.json` and a hooks module.
    ClaudeMod,
    /// A Rebon package: `rebon-plugin.json`.
    RebonPackage,
    /// Anything else, and why Rebon does not install it.
    Unsupported { reason: String },
}

impl PluginShape {
    pub fn of(dir: &Path) -> Self {
        if dir.join(PLUGIN_MANIFEST_FILE).is_file() {
            return Self::RebonPackage;
        }
        if crate::claude_mod::is_claude_mod_dir(dir) {
            if dir.join(rebon_types::CLAUDE_HOOKS_FILE).is_file() {
                return Self::ClaudeMod;
            }
            return Self::Unsupported {
                reason:
                    "a Claude Code plugin without a hooks module (its skills, commands, \
                         agents or MCP servers); Rebon installs Claude Code mods and Rebon packages"
                        .to_owned(),
            };
        }
        if !dir.is_dir() {
            return Self::Unsupported {
                reason: format!("{} is not a folder", dir.display()),
            };
        }
        Self::Unsupported {
            reason: "neither a Claude Code mod (.claude-plugin/plugin.json with hooks/hooks.json) \
                     nor a Rebon package (rebon-plugin.json)"
                .to_owned(),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::ClaudeMod => "mod",
            Self::RebonPackage => "package",
            Self::Unsupported { .. } => "unsupported",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(raw: &str) -> MarketplaceSource {
        parse_marketplace_input(raw, Path::new("/work"), Some(Path::new("/home/me"))).unwrap()
    }

    #[test]
    fn marketplace_input_reads_as_claude_code_reads_it() {
        assert_eq!(
            parse("hamzafer/claude-code-mods"),
            MarketplaceSource::Github {
                repo: "hamzafer/claude-code-mods".into(),
                git_ref: None
            }
        );
        assert_eq!(
            parse("org/market#v2"),
            MarketplaceSource::Github {
                repo: "org/market".into(),
                git_ref: Some("v2".into())
            }
        );
        assert_eq!(
            parse("org/market@main"),
            MarketplaceSource::Github {
                repo: "org/market".into(),
                git_ref: Some("main".into())
            }
        );
        assert_eq!(
            parse("git@github.com:org/repo#main"),
            MarketplaceSource::Git {
                url: "git@github.com:org/repo".into(),
                git_ref: Some("main".into())
            }
        );
        assert_eq!(
            parse("https://example.com/repo.git#v1"),
            MarketplaceSource::Git {
                url: "https://example.com/repo.git".into(),
                git_ref: Some("v1".into())
            }
        );
        assert_eq!(
            parse("https://dev.azure.com/org/project/_git/repo"),
            MarketplaceSource::Git {
                url: "https://dev.azure.com/org/project/_git/repo".into(),
                git_ref: None
            }
        );
        assert_eq!(
            parse("https://github.com/owner/repo"),
            MarketplaceSource::Git {
                url: "https://github.com/owner/repo.git".into(),
                git_ref: None
            }
        );
        assert_eq!(
            parse("file:///srv/market.git#main"),
            MarketplaceSource::Git {
                url: "file:///srv/market.git".into(),
                git_ref: Some("main".into())
            }
        );
        assert_eq!(
            parse("https://example.com/marketplace.json"),
            MarketplaceSource::Url {
                url: "https://example.com/marketplace.json".into()
            }
        );
        assert_eq!(
            parse("./market"),
            MarketplaceSource::Directory {
                path: Path::new("/work").join("./market")
            }
        );
        assert_eq!(
            parse("~/m/marketplace.json"),
            MarketplaceSource::File {
                path: Path::new("/home/me").join("m/marketplace.json")
            }
        );
    }

    #[test]
    fn marketplace_input_refuses_what_names_nothing() {
        for raw in [
            "",
            "   ",
            "just-a-word",
            "a/b/c",
            "https://example.com/m.json#ref",
        ] {
            assert!(
                parse_marketplace_input(raw, Path::new("/w"), None).is_err(),
                "{raw:?} should be refused"
            );
        }
    }

    #[test]
    fn names_follow_the_rules_and_reserved_ones_are_refused() {
        assert!(valid_name("claude-code-mods"));
        assert!(valid_name("a.b_c-1"));
        assert!(!valid_name("-lead"));
        assert!(!valid_name("a..b"));
        assert!(!valid_name("has space"));
        assert!(refused_marketplace_name("claude-code-mods").is_none());
        assert!(refused_marketplace_name("claude-plugins-official").is_some());
        assert!(refused_marketplace_name("GitHub").is_some());
        assert!(refused_marketplace_name("claudeai-team").is_some());
        assert!(refused_marketplace_name(BUILTIN_MARKETPLACE).is_some());
    }

    #[test]
    fn a_manifest_reads_every_source_kind_and_keeps_the_rest_when_one_entry_is_odd() {
        let manifest = MarketplaceManifest::parse(
            &json!({
                "name": "mixed",
                "owner": { "name": "me" },
                "metadata": { "pluginRoot": "./plugins" },
                "plugins": [
                    { "name": "local", "source": "./local", "description": "here" },
                    { "name": "bare", "source": "bare" },
                    { "name": "gh", "source": { "source": "github", "repo": "o/r", "ref": "main" } },
                    { "name": "git", "source": { "source": "url", "url": "https://x/y.git", "sha": "abc" } },
                    { "name": "sub", "source": { "source": "git-subdir", "url": "o/mono", "path": "tools/a" } },
                    { "name": "pkg", "source": { "source": "npm", "package": "@s/p", "version": "^1" } },
                    { "name": "zip", "source": { "source": "archive", "url": "https://x/a.tgz" } },
                    { "name": "cmd", "source": { "source": "command", "command": "print-path" } },
                    { "name": "odd", "source": { "source": "telepathy" } },
                    { "source": "./nameless" },
                ]
            })
            .to_string(),
        )
        .unwrap();
        let kinds: Vec<(&str, &str)> = manifest
            .plugins
            .iter()
            .map(|entry| (entry.name.as_str(), entry.source.kind()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("local", "path"),
                ("bare", "path"),
                ("gh", "github"),
                ("git", "git"),
                ("sub", "git-subdir"),
                ("pkg", "npm"),
                ("zip", "archive"),
                ("cmd", "command"),
            ]
        );
        assert_eq!(manifest.skipped.len(), 2, "{:?}", manifest.skipped);
        assert_eq!(manifest.owner.as_ref().unwrap().name, "me");
        assert_eq!(
            manifest.entry("local").unwrap().description.as_deref(),
            Some("here")
        );
    }

    #[test]
    fn a_manifest_without_a_name_or_plugins_is_refused() {
        assert!(MarketplaceManifest::parse(r#"{"plugins": []}"#).is_err());
        assert!(MarketplaceManifest::parse(r#"{"name": "x"}"#).is_err());
        assert!(MarketplaceManifest::parse(r#"{"name": "bad name", "plugins": []}"#).is_err());
        assert!(MarketplaceManifest::parse("not json").is_err());
    }

    #[test]
    fn relative_sources_resolve_inside_the_marketplace_only() {
        let root = Path::new("/m");
        let rooted = MarketplaceManifest {
            name: "m".into(),
            metadata: Some(MarketplaceMetadata {
                plugin_root: Some("./plugins".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let bare = MarketplaceManifest {
            name: "m".into(),
            ..Default::default()
        };
        assert_eq!(
            relative_plugin_dir(root, &bare, "./mods/a").unwrap(),
            root.join("mods/a")
        );
        assert_eq!(
            relative_plugin_dir(root, &bare, ".").unwrap(),
            root.to_path_buf()
        );
        assert_eq!(
            relative_plugin_dir(root, &rooted, "fmt").unwrap(),
            root.join("plugins").join("fmt")
        );
        assert!(
            relative_plugin_dir(root, &bare, "fmt").is_err(),
            "no pluginRoot: a bare name names nothing"
        );
        assert!(relative_plugin_dir(root, &bare, "./../escape").is_err());
        assert!(relative_plugin_dir(root, &rooted, "../escape").is_err());
    }

    #[test]
    fn known_marketplaces_and_installs_round_trip_and_start_empty() {
        let home = tempfile::tempdir().unwrap();
        assert!(KnownMarketplaces::load(home.path())
            .unwrap()
            .by_name
            .is_empty());
        let mut known = KnownMarketplaces::default();
        known.by_name.insert(
            "mods".into(),
            KnownMarketplace {
                source: MarketplaceSource::Github {
                    repo: "o/r".into(),
                    git_ref: None,
                },
                install_location: marketplace_copy_dir(home.path(), "mods"),
                last_updated_ms: Some(5),
            },
        );
        known.save(home.path()).unwrap();
        assert_eq!(KnownMarketplaces::load(home.path()).unwrap(), known);
        let text = std::fs::read_to_string(KnownMarketplaces::path(home.path())).unwrap();
        assert!(text.contains("\"source\": \"github\""), "{text}");

        let mut installs = MarketplaceInstalls::default();
        installs.by_id.insert(
            plugin_id("radar", "mods"),
            MarketplaceInstall {
                marketplace: "mods".into(),
                plugin: "radar".into(),
                kind: InstallKind::Mod,
                location: home.path().join("mods/radar"),
                version: Some("0.1.0".into()),
                kernel_plugins: Vec::new(),
                installed_at_ms: 9,
                granted: crate::container::ContainerGrant {
                    network: vec!["api.example.com".into()],
                    env: Vec::new(),
                },
            },
        );
        installs.save(home.path()).unwrap();
        assert_eq!(MarketplaceInstalls::load(home.path()).unwrap(), installs);
    }

    #[test]
    fn a_known_marketplace_says_where_its_catalog_and_root_are() {
        let github = KnownMarketplace {
            source: MarketplaceSource::Github {
                repo: "o/r".into(),
                git_ref: None,
            },
            install_location: PathBuf::from("/c/m"),
            last_updated_ms: None,
        };
        assert_eq!(
            github.manifest_path(),
            PathBuf::from("/c/m").join(MARKETPLACE_MANIFEST)
        );
        assert_eq!(github.root(), Some(PathBuf::from("/c/m")));
        let file = KnownMarketplace {
            source: MarketplaceSource::File {
                path: PathBuf::from("/x/.claude-plugin/marketplace.json"),
            },
            install_location: PathBuf::from("/x/.claude-plugin/marketplace.json"),
            last_updated_ms: None,
        };
        assert_eq!(file.root(), Some(PathBuf::from("/x")));
        let url = KnownMarketplace {
            source: MarketplaceSource::Url {
                url: "https://x/m.json".into(),
            },
            install_location: PathBuf::from("/c/u"),
            last_updated_ms: None,
        };
        assert_eq!(
            url.root(),
            None,
            "a catalog fetched by itself has no folder"
        );
    }

    #[test]
    fn a_folder_reads_as_a_mod_a_package_or_neither() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mod_dir = root.join("mod");
        std::fs::create_dir_all(mod_dir.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(mod_dir.join("hooks")).unwrap();
        std::fs::write(
            mod_dir.join(".claude-plugin/plugin.json"),
            r#"{"name":"m"}"#,
        )
        .unwrap();
        std::fs::write(
            mod_dir.join("hooks/hooks.json"),
            r#"{"modules":["./r.ts"]}"#,
        )
        .unwrap();
        assert_eq!(PluginShape::of(&mod_dir), PluginShape::ClaudeMod);

        let skills = root.join("skills");
        std::fs::create_dir_all(skills.join(".claude-plugin")).unwrap();
        std::fs::write(skills.join(".claude-plugin/plugin.json"), r#"{"name":"s"}"#).unwrap();
        assert!(
            matches!(PluginShape::of(&skills), PluginShape::Unsupported { reason } if reason.contains("hooks module"))
        );

        let package = root.join("pkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join(PLUGIN_MANIFEST_FILE), "{}").unwrap();
        assert_eq!(PluginShape::of(&package), PluginShape::RebonPackage);

        assert!(matches!(
            PluginShape::of(&root.join("missing")),
            PluginShape::Unsupported { .. }
        ));
    }
}
