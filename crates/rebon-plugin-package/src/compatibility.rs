use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use crate::npm_range::NpmRange;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FORMAT_VERSION: u32 = 1;
pub const ADAPTER_REVISION: u32 = 1;
pub const DSH_SNAPSHOT: &str =
    "sha256:baa83e6917f79e67c213a00a2882848baa195a86d185dfab65d31831bdce3711";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompatibilityDeclaration {
    pub format: String,
    pub format_version: u32,
    pub adapter_revision: u32,
    pub sdk: Vec<SdkRequirement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsh_snapshot: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdkRequirement {
    pub name: String,
    pub range: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginFormat {
    Rebon,
    DshNpm,
    ClaudeMods,
}

impl PluginFormat {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rebon => "rebon-plugin",
            Self::DshNpm => "dsh-npm",
            Self::ClaudeMods => "claude-mods",
        }
    }

    pub fn adapter_id(self) -> &'static str {
        match self {
            Self::Rebon => "native",
            Self::DshNpm => "cordis",
            Self::ClaudeMods => "claude-mods",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompatibilityError {
    MissingCompatibility {
        plugin: String,
        reason: String,
    },
    UnsupportedFormat {
        plugin: String,
        requested: String,
        supported: String,
    },
    UnsupportedSdk {
        plugin: String,
        name: String,
        requirement: String,
        provided: Option<String>,
    },
    UnsupportedAdapter {
        plugin: String,
        adapter: String,
        requested: u32,
        supported: u32,
    },
}

impl fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCompatibility { plugin, reason } => write!(f, "{plugin}: missing compatibility declaration ({reason}); update the declaration or reinstall from a compatible source"),
            Self::UnsupportedFormat { plugin, requested, supported } => write!(f, "{plugin}: unsupported plugin format {requested}; supported: {supported}"),
            Self::UnsupportedSdk { plugin, name, requirement, provided } => {
                write!(f, "{plugin}: unsupported SDK {name} requirement {requirement}; ")?;
                match provided {
                    Some(version) => write!(f, "host provides {version}"),
                    None => write!(f, "host has not declared a version for this module"),
                }
            }
            Self::UnsupportedAdapter { plugin, adapter, requested, supported } => write!(f, "{plugin}: unsupported adapter {adapter} revision {requested}; supported revision: {supported}"),
        }
    }
}

impl std::error::Error for CompatibilityError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadCompatibility {
    Current(PluginFormat),
    // Removed at the next major release; never selected from a package declaration.
    Legacy19,
}

impl LoadCompatibility {
    pub fn adapter_id(self) -> &'static str {
        match self {
            Self::Current(format) => format.adapter_id(),
            Self::Legacy19 => "legacy-1.9",
        }
    }
}

pub fn resolve_package(
    root: &Path,
    manifest: &crate::manifest::PluginManifest,
    record: Option<&crate::store::InstalledPluginRecord>,
) -> Result<LoadCompatibility, CompatibilityError> {
    if let Some(declaration) = &manifest.compatibility {
        let format = declaration.plugin_format(&manifest.name)?;
        declaration.validate_format(&manifest.name, format)?;
        declaration.validate_sdk(&manifest.name, &host_sdk_versions())?;
        return Ok(LoadCompatibility::Current(format));
    }
    let missing = |reason: &str| CompatibilityError::MissingCompatibility {
        plugin: manifest.name.clone(),
        reason: reason.to_owned(),
    };
    let record = record.ok_or_else(|| missing("plugin-dir has no installer record"))?;
    if record.source_identity.is_none() || record.manifest.as_ref() != Some(manifest) {
        return Err(missing(
            "installer provenance or the original manifest is unavailable",
        ));
    }
    let digest = record
        .digest
        .as_deref()
        .ok_or_else(|| missing("the installer did not record an integrity digest"))?;
    let actual = crate::integrity::compute_legacy_dir_digest(root)
        .map_err(|error| missing(&format!("cannot verify installed content: {error}")))?;
    if actual != digest {
        return Err(missing(
            "installed files no longer match the installer record",
        ));
    }
    Ok(LoadCompatibility::Legacy19)
}

pub fn resolve_mod(
    mod_: &crate::claude_mod::ClaudeMod,
    config_home: &Path,
) -> Result<LoadCompatibility, CompatibilityError> {
    let mut manifest = crate::claude_mod::plugin_manifest_for(mod_);
    let missing = |reason: String| CompatibilityError::MissingCompatibility {
        plugin: manifest.name.clone(),
        reason,
    };
    let root = mod_
        .root
        .canonicalize()
        .map_err(|error| missing(error.to_string()))?;
    let installs = crate::marketplace::MarketplaceInstalls::load(config_home)
        .map_err(|error| missing(error.to_string()))?;
    let installed = installs.by_id.iter().find(|(_, record)| {
        record.kind == crate::marketplace::InstallKind::Mod
            && record
                .location
                .canonicalize()
                .is_ok_and(|location| location == root)
    });
    if manifest.compatibility.is_some()
        && installed.is_none_or(|(_, install)| {
            !install
                .mod_compatibility
                .as_ref()
                .is_some_and(|record| record.first_seen)
        })
    {
        return resolve_package(&root, &manifest, None);
    }
    let (install_id, install) =
        installed.ok_or_else(|| missing("mod directory has no installer record".to_owned()))?;
    if !install.matches_mod_owner(install_id, &manifest.name) {
        return Err(missing(
            "installer provenance or runtime identity does not match".to_owned(),
        ));
    }
    if !config_home
        .join(rebon_types::MODS_DIR)
        .join(&install.plugin)
        .canonicalize()
        .is_ok_and(|expected| expected == root)
    {
        return Err(missing(
            "mod directory does not match its installed location".to_owned(),
        ));
    }
    if install
        .mod_compatibility
        .as_ref()
        .is_none_or(|record| record.first_seen)
    {
        return resolve_legacy_mod(&root, &manifest, config_home, &installs, install_id);
    }
    let recorded = install
        .mod_compatibility
        .as_ref()
        .expect("current mod record exists");
    let actual = crate::integrity::compute_dir_digest(&mod_.root)
        .map_err(|error| missing(format!("cannot verify installed content: {error}")))?;
    if actual != recorded.digest {
        return Err(missing(
            "installed files no longer match the installer record".to_owned(),
        ));
    }
    manifest.compatibility = recorded.manifest.compatibility.clone();
    if manifest != recorded.manifest {
        return Err(CompatibilityError::MissingCompatibility {
            plugin: manifest.name,
            reason: "the synthesized manifest no longer matches the installer record".to_owned(),
        });
    }
    if let Some(declaration) = &manifest.compatibility {
        declaration.validate_format(&manifest.name, PluginFormat::ClaudeMods)?;
    }
    resolve_package(&mod_.root, &manifest, None)
}

fn resolve_legacy_mod(
    root: &Path,
    manifest: &crate::manifest::PluginManifest,
    config_home: &Path,
    installs: &crate::marketplace::MarketplaceInstalls,
    install_id: &str,
) -> Result<LoadCompatibility, CompatibilityError> {
    let missing = |reason: String| CompatibilityError::MissingCompatibility {
        plugin: manifest.name.clone(),
        reason,
    };
    let actual = crate::integrity::compute_legacy_dir_digest(root)
        .map_err(|error| missing(format!("cannot verify installed content: {error}")))?;
    let install = installs
        .by_id
        .get(install_id)
        .expect("matched mod install exists");
    let write_error = if let Some(recorded) = &install.mod_compatibility {
        if !recorded.first_seen
            || recorded.manifest.compatibility.is_some()
            || recorded.manifest != *manifest
            || recorded.digest != actual
        {
            return Err(missing(
                "installed files no longer match the first-seen record".to_owned(),
            ));
        }
        None
    } else {
        (|| -> anyhow::Result<()> {
            let mut latest = crate::marketplace::MarketplaceInstalls::load(config_home)?;
            let current = latest.by_id.get_mut(install_id).ok_or_else(|| {
                anyhow::anyhow!("mod install was removed before recording its digest")
            })?;
            if current != install {
                anyhow::bail!("mod install changed before recording its digest");
            }
            current.mod_compatibility = Some(crate::marketplace::ModCompatibilityRecord {
                manifest: manifest.clone(),
                digest: actual,
                first_seen: true,
            });
            latest.save(config_home)
        })()
        .err()
    };
    if let Some(mut warning) = legacy_mod_diagnostic(root, &manifest.name) {
        if let Some(error) = write_error {
            warning.push_str(&format!(
                " Could not persist the first-seen digest: {error}; the next load will retry."
            ));
        }
        tracing::warn!(plugin = %manifest.name, %warning, "legacy mod compatibility");
    }
    Ok(LoadCompatibility::Legacy19)
}

fn legacy_mod_diagnostic(root: &Path, plugin: &str) -> Option<String> {
    legacy_diagnostic(root, plugin).map(|warning| {
        format!("{warning} Integrity at installation cannot be verified; the first-seen digest is only a later observation.")
    })
}

pub fn legacy_diagnostic(root: &Path, plugin: &str) -> Option<String> {
    static REPORTED: std::sync::Mutex<BTreeSet<std::path::PathBuf>> =
        std::sync::Mutex::new(BTreeSet::new());
    if !REPORTED
        .lock()
        .expect("legacy diagnostic registry poisoned")
        .insert(root.to_path_buf())
    {
        return None;
    }
    Some(format!("{plugin}: loading with Rebon legacy-1.9 compatibility; reinstall or update its compatibility declaration. This profile will be removed in the next major release."))
}

pub fn host_sdk_versions() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("rebon-plugin-api".to_owned(), "1.0.0".to_owned()),
        ("rebon-claude-mods-api".to_owned(), "1.0.0".to_owned()),
        ("cordis".to_owned(), "4.0.0-rc.8".to_owned()),
        ("@deepseek-ai/cordis".to_owned(), "4.0.0-rc.8".to_owned()),
        ("cosmokit".to_owned(), "1.8.1".to_owned()),
        ("@deepseek-ai/cosmokit".to_owned(), "1.8.1".to_owned()),
        ("schemastery".to_owned(), "3.18.0".to_owned()),
        ("@deepseek-ai/schemastery".to_owned(), "3.18.0".to_owned()),
        ("eventsource-parser".to_owned(), "3.1.0".to_owned()),
    ])
}

impl CompatibilityDeclaration {
    pub fn plugin_format(&self, plugin: &str) -> Result<PluginFormat, CompatibilityError> {
        match self.format.as_str() {
            "rebon-plugin" => Ok(PluginFormat::Rebon),
            "dsh-npm" => Ok(PluginFormat::DshNpm),
            "claude-mods" => Ok(PluginFormat::ClaudeMods),
            other => Err(CompatibilityError::UnsupportedFormat {
                plugin: plugin.to_owned(),
                requested: other.to_owned(),
                supported: "rebon-plugin, dsh-npm, claude-mods".to_owned(),
            }),
        }
    }
    pub fn read(
        plugin: &str,
        value: Option<&Value>,
        expected: PluginFormat,
    ) -> Result<Self, CompatibilityError> {
        let value = value.ok_or_else(|| CompatibilityError::MissingCompatibility {
            plugin: plugin.to_owned(),
            reason: format!("{} requires an explicit declaration", expected.name()),
        })?;
        let declaration: Self = serde_json::from_value(value.clone()).map_err(|error| {
            CompatibilityError::UnsupportedFormat {
                plugin: plugin.to_owned(),
                requested: format!("invalid declaration: {error}"),
                supported: format!("{} version {FORMAT_VERSION}", expected.name()),
            }
        })?;
        declaration.validate_format(plugin, expected)?;
        Ok(declaration)
    }

    pub fn validate_format(
        &self,
        plugin: &str,
        expected: PluginFormat,
    ) -> Result<(), CompatibilityError> {
        if self.format != expected.name()
            || self.format_version != FORMAT_VERSION
            || (self.dsh_snapshot.is_some() && expected != PluginFormat::DshNpm)
        {
            return Err(CompatibilityError::UnsupportedFormat {
                plugin: plugin.to_owned(),
                requested: format!("{} version {}", self.format, self.format_version),
                supported: format!("{} version {FORMAT_VERSION}", expected.name()),
            });
        }
        if self.adapter_revision != ADAPTER_REVISION {
            return Err(CompatibilityError::UnsupportedAdapter {
                plugin: plugin.to_owned(),
                adapter: expected.adapter_id().to_owned(),
                requested: self.adapter_revision,
                supported: ADAPTER_REVISION,
            });
        }
        Ok(())
    }

    /// Host-provided modules include prereleases when matching npm ranges.
    /// Their exact versions still come from the host, never from package claims.
    pub fn validate_sdk(
        &self,
        plugin: &str,
        provided: &BTreeMap<String, String>,
    ) -> Result<(), CompatibilityError> {
        let names: &[&str] = match self.plugin_format(plugin)? {
            PluginFormat::Rebon => &["rebon-plugin-api"],
            PluginFormat::DshNpm => &["cordis", "@deepseek-ai/cordis"],
            PluginFormat::ClaudeMods => &["rebon-claude-mods-api"],
        };
        if !self
            .sdk
            .iter()
            .any(|requirement| names.contains(&requirement.name.as_str()))
        {
            return Err(CompatibilityError::MissingCompatibility {
                plugin: plugin.to_owned(),
                reason: format!("no SDK requirement for {}", names.join(" or ")),
            });
        }
        if let Some(snapshot) = &self.dsh_snapshot {
            if snapshot != DSH_SNAPSHOT {
                return Err(CompatibilityError::UnsupportedSdk {
                    plugin: plugin.to_owned(),
                    name: "rebon-dsh-snapshot".to_owned(),
                    requirement: snapshot.clone(),
                    provided: Some(DSH_SNAPSHOT.to_owned()),
                });
            }
        }
        for requirement in &self.sdk {
            let version = provided.get(&requirement.name);
            let matches = version
                .and_then(|version| Version::parse(version).ok())
                .zip(NpmRange::parse(&requirement.range))
                .is_some_and(|(version, range)| range.matches(&version));
            if !matches {
                return Err(CompatibilityError::UnsupportedSdk {
                    plugin: plugin.to_owned(),
                    name: requirement.name.clone(),
                    requirement: requirement.range.clone(),
                    provided: version.cloned(),
                });
            }
        }
        Ok(())
    }
}

pub fn npm_sdk_requirements(
    plugin: &str,
    package: &Value,
    provided: &BTreeSet<String>,
) -> Result<Vec<SdkRequirement>, CompatibilityError> {
    let mut requirements = Vec::new();
    for field in ["dependencies", "peerDependencies"] {
        let Some(entries) = package.get(field) else {
            continue;
        };
        let entries = entries
            .as_object()
            .ok_or_else(|| CompatibilityError::UnsupportedFormat {
                plugin: plugin.to_owned(),
                requested: format!("{field} is not an object"),
                supported: "npm dependency objects".to_owned(),
            })?;
        for (name, range) in entries {
            if !provided.contains(name) {
                continue;
            }
            let range = range
                .as_str()
                .ok_or_else(|| CompatibilityError::UnsupportedSdk {
                    plugin: plugin.to_owned(),
                    name: name.clone(),
                    requirement: range.to_string(),
                    provided: None,
                })?;
            if NpmRange::parse(range).is_none() {
                return Err(CompatibilityError::UnsupportedSdk {
                    plugin: plugin.to_owned(),
                    name: name.clone(),
                    requirement: range.to_owned(),
                    provided: None,
                });
            }
            requirements.push(SdkRequirement {
                name: name.clone(),
                range: range.to_owned(),
            });
        }
    }
    Ok(requirements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn legacy_marketplace_mod(config: &Path) -> crate::claude_mod::ClaudeMod {
        let root = config.join("mods/demo");
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/plugin.json"),
            r#"{"name":"demo"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("hooks/hooks.json"),
            r#"{"modules":["./register.ts"]}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("hooks/register.ts"),
            "export const register = () => {};",
        )
        .unwrap();
        let installs: crate::marketplace::MarketplaceInstalls = serde_json::from_value(json!({
            "demo@team": {
                "marketplace": "team", "plugin": "demo", "kind": "mod",
                "location": root, "kernelPlugins": ["demo"], "installedAtMs": 1,
                "sourceIdentity": {
                    "kind": "marketplace", "plugin": "demo",
                    "catalog": {"source":"directory", "path":config.join("catalog")},
                    "source": "./mods/demo"
                }
            }
        }))
        .unwrap();
        installs.save(config).unwrap();
        crate::claude_mod::read_claude_mod(&root).unwrap()
    }

    #[test]
    fn legacy_marketplace_mod_records_a_first_seen_digest_not_install_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mod_ = legacy_marketplace_mod(dir.path());
        assert_eq!(
            resolve_mod(&mod_, dir.path()).unwrap(),
            LoadCompatibility::Legacy19
        );
        let raw: Value = serde_json::from_slice(
            &std::fs::read(crate::marketplace::MarketplaceInstalls::path(dir.path())).unwrap(),
        )
        .unwrap();
        let record = &raw["demo@team"]["modCompatibility"];
        assert_eq!(record["firstSeen"], true);
        assert_eq!(
            record["digest"],
            crate::integrity::compute_legacy_dir_digest(&mod_.root).unwrap()
        );
        assert!(record["manifest"]["compatibility"].is_null());
    }

    #[test]
    fn legacy_marketplace_mod_unchanged_content_keeps_its_first_seen_record() {
        let dir = tempfile::tempdir().unwrap();
        let mod_ = legacy_marketplace_mod(dir.path());
        resolve_mod(&mod_, dir.path()).unwrap();
        let path = crate::marketplace::MarketplaceInstalls::path(dir.path());
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            resolve_mod(&mod_, dir.path()).unwrap(),
            LoadCompatibility::Legacy19
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn legacy_marketplace_mod_changed_content_cannot_replace_its_first_seen_digest() {
        let dir = tempfile::tempdir().unwrap();
        let mod_ = legacy_marketplace_mod(dir.path());
        resolve_mod(&mod_, dir.path()).unwrap();
        let path = crate::marketplace::MarketplaceInstalls::path(dir.path());
        let before = std::fs::read(&path).unwrap();
        std::fs::write(mod_.root.join("extra.js"), "changed").unwrap();
        let error = resolve_mod(&mod_, dir.path()).unwrap_err();
        assert!(matches!(
            error,
            CompatibilityError::MissingCompatibility { .. }
        ));
        assert!(error.to_string().contains("first-seen"));
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn legacy_marketplace_mod_cannot_bypass_first_seen_by_adding_a_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let mod_ = legacy_marketplace_mod(dir.path());
        resolve_mod(&mod_, dir.path()).unwrap();
        std::fs::write(
            mod_.root.join(".claude-plugin/plugin.json"),
            json!({
                "name":"demo", "rebon": {
                    "format":"claude-mods", "formatVersion":1, "adapterRevision":1,
                    "sdk":[{"name":"rebon-claude-mods-api", "range":"^1"}]
                }
            })
            .to_string(),
        )
        .unwrap();
        let mod_ = crate::claude_mod::read_claude_mod(&mod_.root).unwrap();
        assert!(matches!(
            resolve_mod(&mod_, dir.path()),
            Err(CompatibilityError::MissingCompatibility { .. })
        ));
    }

    #[test]
    fn legacy_marketplace_mod_refuses_mismatched_ownership_before_recording_content() {
        for case in [
            "source",
            "plugin",
            "runtime",
            "extra-runtime",
            "location",
            "key",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mod_ = legacy_marketplace_mod(dir.path());
            let mut installs = crate::marketplace::MarketplaceInstalls::load(dir.path()).unwrap();
            let record = installs.by_id.get_mut("demo@team").unwrap();
            match case {
                "source" => record.source_identity = None,
                "plugin" => record.plugin = "other".to_owned(),
                "runtime" => record.kernel_plugins = vec!["other".to_owned()],
                "extra-runtime" => record.kernel_plugins.push("other".to_owned()),
                "location" => record.location = dir.path().to_path_buf(),
                "key" => {
                    let record = installs.by_id.remove("demo@team").unwrap();
                    installs.by_id.insert("other@team".to_owned(), record);
                }
                _ => unreachable!(),
            }
            installs.save(dir.path()).unwrap();
            assert!(
                matches!(
                    resolve_mod(&mod_, dir.path()),
                    Err(CompatibilityError::MissingCompatibility { .. })
                ),
                "{case}"
            );
            assert_eq!(
                crate::marketplace::MarketplaceInstalls::load(dir.path()).unwrap(),
                installs
            );
        }
    }

    #[test]
    fn legacy_marketplace_mod_preserves_installs_added_before_digest_save() {
        let dir = tempfile::tempdir().unwrap();
        let mod_ = legacy_marketplace_mod(dir.path());
        let snapshot = crate::marketplace::MarketplaceInstalls::load(dir.path()).unwrap();
        let mut latest = snapshot.clone();
        let mut other = latest.by_id["demo@team"].clone();
        other.plugin = "other".to_owned();
        latest.by_id.insert("other@team".to_owned(), other.clone());
        latest.save(dir.path()).unwrap();
        let manifest = crate::claude_mod::plugin_manifest_for(&mod_);
        assert_eq!(
            resolve_legacy_mod(&mod_.root, &manifest, dir.path(), &snapshot, "demo@team").unwrap(),
            LoadCompatibility::Legacy19
        );
        let saved = crate::marketplace::MarketplaceInstalls::load(dir.path()).unwrap();
        assert_eq!(saved.by_id.get("other@team"), Some(&other));
        assert!(
            saved.by_id["demo@team"]
                .mod_compatibility
                .as_ref()
                .unwrap()
                .first_seen
        );
    }

    #[test]
    fn legacy_marketplace_mod_does_not_overwrite_a_changed_install_before_digest_save() {
        for case in [
            "removed",
            "source",
            "location",
            "runtime",
            "reinstalled",
            "recorded",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mod_ = legacy_marketplace_mod(dir.path());
            let manifest = crate::claude_mod::plugin_manifest_for(&mod_);
            let snapshot = crate::marketplace::MarketplaceInstalls::load(dir.path()).unwrap();
            let mut latest = snapshot.clone();
            let install = latest.by_id.get_mut("demo@team").unwrap();
            match case {
                "removed" => {
                    latest.by_id.remove("demo@team");
                }
                "source" => install.source_identity = None,
                "location" => install.location = dir.path().join("elsewhere"),
                "runtime" => install.kernel_plugins = vec!["other".to_owned()],
                "reinstalled" => install.installed_at_ms += 1,
                "recorded" => {
                    install.mod_compatibility = Some(crate::marketplace::ModCompatibilityRecord {
                        manifest: manifest.clone(),
                        digest: "another observation".to_owned(),
                        first_seen: true,
                    })
                }
                _ => unreachable!(),
            }
            latest.save(dir.path()).unwrap();
            resolve_legacy_mod(&mod_.root, &manifest, dir.path(), &snapshot, "demo@team").unwrap();
            assert_eq!(
                crate::marketplace::MarketplaceInstalls::load(dir.path()).unwrap(),
                latest,
                "{case}"
            );
        }
    }

    #[test]
    fn legacy_marketplace_mod_keeps_loading_when_first_seen_cannot_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mod_ = legacy_marketplace_mod(dir.path());
        let installs = crate::marketplace::MarketplaceInstalls::load(dir.path()).unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(crate::marketplace::MarketplaceInstalls::path(&blocked)).unwrap();
        let manifest = crate::claude_mod::plugin_manifest_for(&mod_);
        assert_eq!(
            resolve_legacy_mod(&mod_.root, &manifest, &blocked, &installs, "demo@team").unwrap(),
            LoadCompatibility::Legacy19
        );
        assert!(crate::marketplace::MarketplaceInstalls::load(dir.path())
            .unwrap()
            .by_id["demo@team"]
            .mod_compatibility
            .is_none());
        assert_eq!(
            resolve_mod(&mod_, dir.path()).unwrap(),
            LoadCompatibility::Legacy19
        );
        assert!(
            crate::marketplace::MarketplaceInstalls::load(dir.path())
                .unwrap()
                .by_id["demo@team"]
                .mod_compatibility
                .as_ref()
                .unwrap()
                .first_seen
        );
    }

    #[test]
    fn legacy_marketplace_mod_diagnostic_explains_integrity_and_expiry_once() {
        let dir = tempfile::tempdir().unwrap();
        let warning = legacy_mod_diagnostic(dir.path(), "demo").unwrap();
        for expected in [
            "legacy-1.9",
            "reinstall",
            "next major release",
            "cannot be verified",
            "first-seen",
        ] {
            assert!(warning.contains(expected), "{warning}");
        }
        assert!(legacy_mod_diagnostic(dir.path(), "demo").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_marketplace_mod_only_accepts_internal_npm_bin_links() {
        for internal in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mod_ = legacy_marketplace_mod(dir.path());
            let bin = mod_.root.join("node_modules/.bin");
            std::fs::create_dir_all(&bin).unwrap();
            let target = if internal {
                mod_.root.join("node_modules/cli.js")
            } else {
                dir.path().join("outside.js")
            };
            std::fs::write(&target, "cli").unwrap();
            std::os::unix::fs::symlink(&target, bin.join("cli")).unwrap();
            if internal {
                assert_eq!(
                    resolve_mod(&mod_, dir.path()).unwrap(),
                    LoadCompatibility::Legacy19
                );
                std::fs::write(target, "changed").unwrap();
            }
            assert!(matches!(
                resolve_mod(&mod_, dir.path()),
                Err(CompatibilityError::MissingCompatibility { .. })
            ));
        }
    }

    fn declaration() -> Value {
        json!({"format":"dsh-npm", "formatVersion":1, "adapterRevision":1, "sdk":[{"name":"@deepseek-ai/cordis","range":"^4"}]})
    }

    fn snapshot_digest(root: &Path) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for (relative, path) in crate::integrity::package_files(root).unwrap() {
            hasher.update(relative.as_bytes());
            hasher.update([0]);
            hasher.update(
                std::fs::read_to_string(path)
                    .unwrap()
                    .replace("\r\n", "\n")
                    .as_bytes(),
            );
            hasher.update([0]);
        }
        format!("sha256:{:x}", hasher.finalize())
    }

    #[test]
    fn the_bundled_snapshot_and_first_party_declarations_cannot_drift() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(
            snapshot_digest(&root.join("runtimes/node/compose-runtime/payload/vendor/dsh")),
            DSH_SNAPSHOT
        );
        for name in [
            "dsh-llm-deepseek",
            "dsh-tool-todo",
            "dsh-tool-web",
            "dsh-web-search-exa",
        ] {
            let manifest = crate::manifest::PluginManifest::load_from_dir(
                &root.join("marketplace/plugins").join(name),
            )
            .unwrap();
            let declaration = manifest
                .compatibility
                .as_ref()
                .expect("first-party snapshot declaration");
            declaration
                .validate_format(name, PluginFormat::DshNpm)
                .unwrap();
            assert_eq!(declaration.dsh_snapshot.as_deref(), Some(DSH_SNAPSHOT));
            declaration
                .validate_sdk(name, &host_sdk_versions())
                .unwrap();
        }
    }

    #[test]
    fn snapshot_identity_tracks_names_and_content_but_not_checkout_line_endings() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("a.js");
        std::fs::write(&file, "first\nsecond\n").unwrap();
        let original = snapshot_digest(root.path());
        std::fs::write(&file, "first\r\nsecond\r\n").unwrap();
        assert_eq!(snapshot_digest(root.path()), original);
        std::fs::write(&file, "changed\nsecond\n").unwrap();
        let changed = snapshot_digest(root.path());
        assert_ne!(changed, original);
        std::fs::rename(&file, root.path().join("b.js")).unwrap();
        assert_ne!(snapshot_digest(root.path()), changed);
    }

    #[test]
    fn a_snapshot_mismatch_names_both_identities_and_does_not_bypass_npm_peers() {
        let mut value = declaration();
        value["dshSnapshot"] = json!("sha256:old");
        let mut parsed =
            CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm).unwrap();
        let error = parsed
            .validate_sdk("demo", &host_sdk_versions())
            .unwrap_err();
        assert!(error.to_string().contains("sha256:old"));
        assert!(error.to_string().contains(DSH_SNAPSHOT));
        parsed.dsh_snapshot = Some(DSH_SNAPSHOT.to_owned());
        parsed.validate_sdk("demo", &host_sdk_versions()).unwrap();
        parsed.sdk.push(SdkRequirement {
            name: "@deepseek-ai/dsh-session".to_owned(),
            range: "*".to_owned(),
        });
        assert!(matches!(
            parsed.validate_sdk("demo", &host_sdk_versions()),
            Err(CompatibilityError::UnsupportedSdk { provided: None, .. })
        ));
    }

    #[test]
    fn documented_vendor_versions_cover_the_resolver_aliases() {
        let mut value = declaration();
        value["sdk"].as_array_mut().unwrap().extend([
            json!({"name":"cosmokit","range":"^1.8"}),
            json!({"name":"@deepseek-ai/cosmokit","range":"1.8.1"}),
            json!({"name":"schemastery","range":"^3"}),
            json!({"name":"@deepseek-ai/schemastery","range":"3.18.0"}),
            json!({"name":"eventsource-parser","range":"^3.1"}),
        ]);
        CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm)
            .unwrap()
            .validate_sdk("demo", &host_sdk_versions())
            .unwrap();
    }

    #[test]
    fn reads_an_explicit_contract_without_using_package_version() {
        let value = declaration();
        let parsed =
            CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm).unwrap();
        assert_eq!(parsed.format_version, 1);
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }

    #[test]
    fn missing_and_malformed_declarations_are_not_current_contracts() {
        assert!(matches!(
            CompatibilityDeclaration::read("demo", None, PluginFormat::DshNpm),
            Err(CompatibilityError::MissingCompatibility { .. })
        ));
        for value in [
            Value::Null,
            json!({}),
            json!({"format":"dsh-npm","formatVersion":"1","adapterRevision":1,"sdk":[]}),
        ] {
            assert!(matches!(
                CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm),
                Err(CompatibilityError::UnsupportedFormat { .. })
            ));
        }
    }

    #[test]
    fn unsupported_formats_and_ecosystems_do_not_fall_back() {
        for version in [0, 2, u32::MAX] {
            let mut value = declaration();
            value["formatVersion"] = json!(version);
            assert!(matches!(
                CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm),
                Err(CompatibilityError::UnsupportedFormat { .. })
            ));
        }
        for format in ["opencode", "pi", "claude-mods", "rebon-plugin"] {
            let mut value = declaration();
            value["format"] = json!(format);
            assert!(matches!(
                CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm),
                Err(CompatibilityError::UnsupportedFormat { .. })
            ));
        }
    }

    #[test]
    fn unknown_adapter_revisions_are_distinct_errors() {
        for revision in [0, 2, u32::MAX] {
            let mut value = declaration();
            value["adapterRevision"] = json!(revision);
            assert!(matches!(
                CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm),
                Err(CompatibilityError::UnsupportedAdapter { .. })
            ));
        }
    }

    #[test]
    fn host_prereleases_match_npm_ranges_without_becoming_stable_versions() {
        let mut value = declaration();
        let provided =
            BTreeMap::from([("@deepseek-ai/cordis".to_owned(), "4.0.0-rc.8".to_owned())]);
        for (range, expected) in [
            ("^4", true),
            ("4.0.0-rc.8", true),
            ("^3 || ^4", true),
            (">=4.0.0", false),
            ("^5", false),
            ("^4.1", false),
            ("garbage", false),
        ] {
            value["sdk"][0]["range"] = json!(range);
            let parsed =
                CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm).unwrap();
            assert_eq!(
                parsed.validate_sdk("demo", &provided).is_ok(),
                expected,
                "{range}"
            );
        }
    }

    #[test]
    fn an_unknown_host_sdk_is_not_satisfied_by_a_wildcard() {
        let mut value = declaration();
        value["sdk"][0]["range"] = json!("*");
        let parsed =
            CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm).unwrap();
        let error = parsed.validate_sdk("demo", &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            error,
            CompatibilityError::UnsupportedSdk { provided: None, .. }
        ));
        assert!(error
            .to_string()
            .contains("host has not declared a version"));
    }

    #[test]
    fn preserves_both_dependency_constraints_before_slimming() {
        let package = json!({"version":"999", "dependencies":{"cordis":"^4", "external":"^1"}, "peerDependencies":{"cordis":">=4.0.0-rc.8"}});
        let requirements =
            npm_sdk_requirements("demo", &package, &BTreeSet::from(["cordis".to_owned()])).unwrap();
        assert_eq!(requirements.len(), 2);
        assert_eq!(requirements[0].range, "^4");
        assert_eq!(requirements[1].range, ">=4.0.0-rc.8");
    }

    #[test]
    fn legacy_installs_need_provenance_and_unchanged_content() {
        use crate::store::{InstalledPluginRecord, PluginSourceIdentity, PluginSourceKind};
        let root = tempfile::tempdir().unwrap();
        let manifest: crate::manifest::PluginManifest =
            serde_json::from_value(json!({"name":"demo", "version":"1"})).unwrap();
        std::fs::write(
            root.path().join("rebon-plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(root.path().join("index.js"), "original").unwrap();
        let mut record = InstalledPluginRecord {
            name: "demo".to_owned(),
            version: "1".to_owned(),
            enabled: true,
            disabled_capabilities: Vec::new(),
            source_kind: PluginSourceKind::Local,
            source: None,
            source_identity: Some(PluginSourceIdentity::local(root.path()).unwrap()),
            digest: Some(crate::integrity::compute_dir_digest(root.path()).unwrap()),
            manifest: Some(manifest.clone()),
        };
        assert_eq!(
            resolve_package(root.path(), &manifest, Some(&record)).unwrap(),
            LoadCompatibility::Legacy19
        );
        std::fs::write(root.path().join("index.js"), "modified").unwrap();
        assert!(matches!(
            resolve_package(root.path(), &manifest, Some(&record)),
            Err(CompatibilityError::MissingCompatibility { .. })
        ));
        std::fs::write(root.path().join("index.js"), "original").unwrap();
        record.source_identity = None;
        assert!(matches!(
            resolve_package(root.path(), &manifest, Some(&record)),
            Err(CompatibilityError::MissingCompatibility { .. })
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_symlink_added_after_install_disqualifies_the_legacy_profile() {
        use crate::store::{InstalledPluginRecord, PluginSourceIdentity, PluginSourceKind};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugin");
        std::fs::create_dir(&root).unwrap();
        let manifest: crate::manifest::PluginManifest =
            serde_json::from_value(json!({"name":"demo", "version":"1"})).unwrap();
        let record = InstalledPluginRecord {
            name: "demo".to_owned(),
            version: "1".to_owned(),
            enabled: true,
            disabled_capabilities: Vec::new(),
            source_kind: PluginSourceKind::Local,
            source: None,
            source_identity: Some(PluginSourceIdentity::local(&root).unwrap()),
            digest: Some(crate::integrity::compute_dir_digest(&root).unwrap()),
            manifest: Some(manifest.clone()),
        };
        let target = dir.path().join("outside.js");
        std::fs::write(&target, "outside").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, root.join("extra.js"));
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, root.join("extra.js"));
        if let Err(error) = linked {
            #[cfg(windows)]
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!(
                    "skipping symlink test: Windows did not grant symbolic-link creation: {error}"
                );
                return;
            }
            panic!("creating the symlink: {error}");
        }
        let error = resolve_package(&root, &manifest, Some(&record)).unwrap_err();
        assert!(matches!(
            error,
            CompatibilityError::MissingCompatibility { .. }
        ));
        assert!(error.to_string().contains("unverifiable entry"), "{error}");
    }

    #[test]
    fn plugin_dirs_cannot_claim_the_legacy_profile() {
        let root = tempfile::tempdir().unwrap();
        let manifest: crate::manifest::PluginManifest =
            serde_json::from_value(json!({"name":"demo", "version":"1"})).unwrap();
        assert!(matches!(
            resolve_package(root.path(), &manifest, None),
            Err(CompatibilityError::MissingCompatibility { .. })
        ));
        let mut value = declaration();
        value["format"] = json!("legacy-1.9");
        assert!(matches!(
            CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm),
            Err(CompatibilityError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn a_declaration_must_name_the_sdk_its_adapter_exposes() {
        let mut value = declaration();
        value["sdk"] = json!([]);
        let parsed =
            CompatibilityDeclaration::read("demo", Some(&value), PluginFormat::DshNpm).unwrap();
        assert!(matches!(
            parsed.validate_sdk("demo", &host_sdk_versions()),
            Err(CompatibilityError::MissingCompatibility { .. })
        ));
    }

    #[test]
    fn malformed_sdk_requirements_are_refused() {
        let provided = BTreeSet::from(["cordis".to_owned()]);
        for range in [
            json!(42),
            json!("workspace:*"),
            json!("https://example.com/sdk.tgz"),
        ] {
            let package = json!({"peerDependencies":{"cordis":range}});
            assert!(matches!(
                npm_sdk_requirements("demo", &package, &provided),
                Err(CompatibilityError::UnsupportedSdk { .. })
            ));
        }
        assert!(matches!(
            npm_sdk_requirements("demo", &json!({"dependencies":[]}), &provided),
            Err(CompatibilityError::UnsupportedFormat { .. })
        ));
    }
}
