use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use sha2::{Digest, Sha256};

use super::manifest::PluginManifest;
use super::package::{unpack_package, PackageLimits};
use super::source::{resolve_install_source, ResolvedPluginSource};
use super::store::{InstalledPluginRecord, PluginScope, PluginSourceKind, PluginStore};

#[derive(Debug, Clone)]
pub struct PluginInstaller {
    store: PluginStore,
    cwd: PathBuf,
    plugin_dirs: Vec<PathBuf>,
}

impl PluginInstaller {
    pub fn new(store: PluginStore, cwd: PathBuf, plugin_dirs: Vec<PathBuf>) -> Self {
        Self {
            store,
            cwd,
            plugin_dirs,
        }
    }

    /// Installs a directory, a package archive, or a built-in alias.
    ///
    /// `expected_sha256` binds the bytes of a package archive to a digest the
    /// caller was given out of band — a registry entry, a checksum carried
    /// alongside a file onto an air-gapped machine. It is checked before the
    /// archive is opened, so a tampered package is never even parsed, and it is
    /// rejected outright for a directory source, which has no canonical byte
    /// sequence to bind.
    pub fn install(
        &self,
        source: &str,
        scope: PluginScope,
        expected_sha256: Option<&str>,
    ) -> anyhow::Result<InstalledPluginRecord> {
        if matches!(scope, PluginScope::Project)
            && !crate::rebon_config::is_directory_trusted(&self.cwd)
        {
            bail!(
                "project-scope plugin install requires trusted workspace {}; run `rebon` interactively to accept trust first",
                self.cwd.display()
            );
        }

        let resolved = resolve_install_source(source, &self.cwd, &self.plugin_dirs)?;
        if expected_sha256.is_some() && !matches!(resolved, ResolvedPluginSource::Archive { .. }) {
            bail!(
                "--sha256 binds the bytes of a package archive; `{source}` is not one, so there are no bytes to bind"
            );
        }

        match resolved {
            ResolvedPluginSource::Archive { archive } => {
                if let Some(expected) = expected_sha256 {
                    verify_archive_digest(&archive, expected)?;
                }
                let staging = self.staging_dir(scope, "archive", "unpack");
                if staging.exists() {
                    let _ = fs::remove_dir_all(&staging);
                }
                fs::create_dir_all(&staging)?;
                let unpacked = match unpack_package(
                    &archive,
                    &staging.join("raw"),
                    PackageLimits::default(),
                ) {
                    Ok(unpacked) => unpacked,
                    Err(err) => {
                        let _ = fs::remove_dir_all(&staging);
                        return Err(err);
                    }
                };
                tracing::debug!(
                    archive = %archive.display(),
                    entries = unpacked.entries,
                    bytes = unpacked.bytes,
                    "unpacked plugin package"
                );
                self.publish_staged(
                    scope,
                    staging,
                    unpacked.root,
                    archive.to_string_lossy().to_string(),
                )
            }
            ResolvedPluginSource::BuiltinAlias(alias) => {
                let record = InstalledPluginRecord {
                    name: alias.name.to_string(),
                    version: alias.version.to_string(),
                    enabled: true,
                    disabled_capabilities: Vec::new(),
                    source_kind: PluginSourceKind::Builtin,
                    source: Some(alias.name.to_string()),
                    digest: None,
                    manifest: None,
                };
                self.store.upsert_record(scope, record.clone())?;
                Ok(record)
            }
            ResolvedPluginSource::LocalPath { root, manifest }
            | ResolvedPluginSource::ConfiguredDir { root, manifest } => {
                let staging = self.staging_dir(scope, &manifest.name, &manifest.version);
                if staging.exists() {
                    let _ = fs::remove_dir_all(&staging);
                }
                fs::create_dir_all(&staging)?;
                let staged_root = staging.join("package");
                if let Err(err) = copy_dir_recursive(&root, &staged_root) {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(err);
                }
                self.publish_staged(
                    scope,
                    staging,
                    staged_root,
                    root.to_string_lossy().to_string(),
                )
            }
        }
    }

    /// The half every install shares: read the staged manifest, refuse a package
    /// that is not self-contained, digest the tree, and swap it into place.
    ///
    /// Nothing inside the package runs here or anywhere else on this path. A
    /// `package.json` carrying `postinstall` is inert data — Rebon unpacks,
    /// checks, and moves files, and never hands any of them to a package
    /// manager.
    fn publish_staged(
        &self,
        scope: PluginScope,
        staging: PathBuf,
        staged_root: PathBuf,
        source_label: String,
    ) -> anyhow::Result<InstalledPluginRecord> {
        let admitted = (|| -> anyhow::Result<(PluginManifest, String)> {
            let manifest = PluginManifest::load_from_dir(&staged_root)?;
            ensure_self_contained(&staged_root, &manifest)?;
            let digest = compute_dir_digest(&staged_root)?;
            Ok((manifest, digest))
        })();
        let (staged_manifest, digest) = match admitted {
            Ok(admitted) => admitted,
            Err(err) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(err);
            }
        };

        let final_dir =
            self.store
                .plugin_version_dir(scope, &staged_manifest.name, &staged_manifest.version);
        if let Some(parent) = final_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        let backup_dir = final_dir.with_extension("rollback");
        if backup_dir.exists() {
            fs::remove_dir_all(&backup_dir)?;
        }
        let had_existing = final_dir.exists();
        if had_existing {
            fs::rename(&final_dir, &backup_dir).with_context(|| {
                format!(
                    "failed to stage rollback backup for {}",
                    final_dir.display()
                )
            })?;
        }
        if let Err(err) = fs::rename(&staged_root, &final_dir) {
            if had_existing && backup_dir.exists() {
                let _ = fs::rename(&backup_dir, &final_dir);
            }
            let _ = fs::remove_dir_all(&staging);
            return Err(err).with_context(|| {
                format!("failed to move staged plugin into {}", final_dir.display())
            });
        }
        if backup_dir.exists() {
            let _ = fs::remove_dir_all(&backup_dir);
        }
        let _ = fs::remove_dir_all(&staging);

        let record = InstalledPluginRecord {
            name: staged_manifest.name.clone(),
            version: staged_manifest.version.clone(),
            enabled: true,
            disabled_capabilities: Vec::new(),
            source_kind: PluginSourceKind::Local,
            source: Some(source_label),
            digest: Some(digest),
            manifest: Some(staged_manifest),
        };
        self.store.upsert_record(scope, record.clone())?;
        self.retire_superseded_versions(scope, &record)?;
        Ok(record)
    }

    /// Drop every other installed version of the same plugin.
    ///
    /// Records are keyed by `(name, version)`, so installing a bumped manifest
    /// used to leave the previous version enabled beside it. Both then
    /// materialize at startup and contribute the same capability ids — and the
    /// loser is whichever the iteration order happens to favour, which for a
    /// model provider means a session can silently run the stale manifest.
    /// Installing a version supersedes the others in that scope.
    fn retire_superseded_versions(
        &self,
        scope: PluginScope,
        installed: &InstalledPluginRecord,
    ) -> anyhow::Result<()> {
        let superseded: Vec<String> = self
            .store
            .load_state(scope)?
            .plugins
            .into_iter()
            .filter(|record| record.name == installed.name && record.version != installed.version)
            .map(|record| record.version)
            .collect();
        for version in superseded {
            self.store
                .remove_version_record(scope, &installed.name, &version)?;
            let dir = self
                .store
                .plugin_version_dir(scope, &installed.name, &version);
            if dir.exists() {
                fs::remove_dir_all(&dir).with_context(|| {
                    format!(
                        "failed to remove superseded plugin package {}",
                        dir.display()
                    )
                })?;
            }
        }
        Ok(())
    }

    pub fn enable(&self, name: &str, scope: PluginScope) -> anyhow::Result<InstalledPluginRecord> {
        if matches!(scope, PluginScope::Project)
            && !crate::rebon_config::is_directory_trusted(&self.cwd)
        {
            bail!(
                "project-scope plugin enable requires trusted workspace {}",
                self.cwd.display()
            );
        }
        self.store.set_enabled(scope, name, true)
    }

    pub fn disable(&self, name: &str, scope: PluginScope) -> anyhow::Result<InstalledPluginRecord> {
        self.store.set_enabled(scope, name, false)
    }

    pub fn uninstall(
        &self,
        name: &str,
        scope: PluginScope,
    ) -> anyhow::Result<Option<InstalledPluginRecord>> {
        let removed = self.store.uninstall(scope, name)?;
        if let Some(record) = &removed {
            if matches!(record.source_kind, PluginSourceKind::Local) {
                let dir = self
                    .store
                    .plugin_version_dir(scope, &record.name, &record.version);
                if dir.exists() {
                    fs::remove_dir_all(&dir).with_context(|| {
                        format!("failed to remove plugin package {}", dir.display())
                    })?;
                }
            }
        }
        Ok(removed)
    }

    pub fn list(
        &self,
        scope: Option<PluginScope>,
    ) -> anyhow::Result<Vec<(PluginScope, InstalledPluginRecord)>> {
        self.store.list_records(scope)
    }

    pub fn status(
        &self,
        name: Option<&str>,
        scope: Option<PluginScope>,
    ) -> anyhow::Result<Vec<(PluginScope, InstalledPluginRecord)>> {
        let records = self.store.list_records(scope)?;
        Ok(match name {
            Some(name) => records
                .into_iter()
                .filter(|(_, record)| record.name == name)
                .collect(),
            None => records,
        })
    }

    /// Re-hashes installed packages and compares against what was recorded at
    /// install time.
    ///
    /// The recorded digest only means something if something reads it back. An
    /// installed package is ordinary files in the user's config home: an editor
    /// save, a half-finished sync, or a partially removed uninstall all leave a
    /// tree that still loads but is no longer the package that was admitted.
    pub fn verify(
        &self,
        name: Option<&str>,
        scope: Option<PluginScope>,
    ) -> anyhow::Result<Vec<PluginVerification>> {
        let records = self.status(name, scope)?;
        let mut report = Vec::new();
        for (scope, record) in records {
            let outcome = match record.source_kind {
                // A builtin has no package on disk to drift.
                PluginSourceKind::Builtin => VerificationOutcome::NotApplicable,
                PluginSourceKind::Local => {
                    let dir = self
                        .store
                        .plugin_version_dir(scope, &record.name, &record.version);
                    match (&record.digest, dir.is_dir()) {
                        (_, false) => VerificationOutcome::Missing { path: dir },
                        (None, true) => VerificationOutcome::NoRecordedDigest,
                        (Some(recorded), true) => {
                            let actual = compute_dir_digest(&dir)?;
                            if actual == *recorded {
                                VerificationOutcome::Match
                            } else {
                                VerificationOutcome::Drifted {
                                    recorded: recorded.clone(),
                                    actual,
                                }
                            }
                        }
                    }
                }
            };
            report.push(PluginVerification {
                scope,
                name: record.name,
                version: record.version,
                outcome,
            });
        }
        Ok(report)
    }

    fn staging_dir(&self, scope: PluginScope, name: &str, version: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        self.store
            .scope_dir(scope)
            .join(format!(".stage-{name}-{version}-{millis}"))
    }
}

/// One installed package's answer to "is this still what was installed?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVerification {
    pub scope: PluginScope,
    pub name: String,
    pub version: String,
    pub outcome: VerificationOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationOutcome {
    Match,
    Drifted {
        recorded: String,
        actual: String,
    },
    /// The record survives but its package directory is gone, which is what a
    /// half-finished uninstall or a wiped config home looks like.
    Missing {
        path: PathBuf,
    },
    /// Installed before digests were recorded.
    NoRecordedDigest,
    /// Builtin aliases have no package on disk.
    NotApplicable,
}

impl PluginVerification {
    pub fn is_problem(&self) -> bool {
        matches!(
            self.outcome,
            VerificationOutcome::Drifted { .. } | VerificationOutcome::Missing { .. }
        )
    }

    pub fn describe(&self) -> String {
        let head = format!("{} {} [{}]", self.name, self.version, self.scope.as_str());
        match &self.outcome {
            VerificationOutcome::Match => format!("{head}: ok"),
            VerificationOutcome::Drifted { recorded, actual } => format!(
                "{head}: CHANGED since install\n  recorded: {recorded}\n  on disk:  {actual}"
            ),
            VerificationOutcome::Missing { path } => {
                format!("{head}: package directory is missing ({})", path.display())
            }
            VerificationOutcome::NoRecordedDigest => {
                format!("{head}: installed without a digest; reinstall to record one")
            }
            VerificationOutcome::NotApplicable => format!("{head}: builtin, nothing on disk"),
        }
    }
}

pub fn format_plugin_result(
    action: &str,
    scope: PluginScope,
    record: &InstalledPluginRecord,
) -> String {
    let enabled = if record.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let source = match record.source_kind {
        PluginSourceKind::Local => "local",
        PluginSourceKind::Builtin => "builtin",
    };
    let digest = record
        .digest
        .as_deref()
        .map(|digest| format!("\n  digest: {digest}"))
        .unwrap_or_default();
    format!(
        "plugin {action}: {} {}\n  scope: {}\n  source: {source}\n  state: {enabled}{digest}\n\nCapabilities are materialized on next startup.",
        record.name,
        record.version,
        scope.as_str()
    )
}

/// Checks a package archive's bytes against a digest the caller was given out
/// of band. Fail-closed and done before the archive is opened for reading.
fn verify_archive_digest(archive: &Path, expected: &str) -> anyhow::Result<()> {
    let expected = expected
        .trim()
        .trim_start_matches("sha256:")
        .to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("`{expected}` is not a 64-character hex SHA-256 digest");
    }
    let mut file = fs::File::open(archive)
        .with_context(|| format!("failed to read plugin package {}", archive.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = to_hex(&hasher.finalize());
    if actual != expected {
        bail!(
            "plugin package {} has SHA-256 {actual}, expected {expected} — refusing to install",
            archive.display()
        );
    }
    Ok(())
}

/// Native extension file suffixes. A package carrying one is not self-contained
/// in the sense that matters: the file is machine code for one platform and ABI,
/// and Rebon cannot tell what it links against.
const NATIVE_MODULE_SUFFIXES: [&str; 4] = [".node", ".dll", ".dylib", ".so"];

/// Refuses a package Rebon cannot carry as-is.
///
/// Two distinct refusals, because they are two distinct mistakes. A package
/// carrying an **undeclared** native module is misdescribing itself, and the fix
/// is on the publisher. A package that **declares** one is honest, and the
/// answer is simply that this version does not support them yet — the platform
/// refactor's package decision says native addons and other dependencies that
/// cannot be self-contained must be declared, and that the first version is
/// fail-closed on them.
fn ensure_self_contained(
    root: &Path,
    manifest: &super::manifest::PluginManifest,
) -> anyhow::Result<()> {
    let declared: Vec<String> = manifest
        .requirements
        .native_modules
        .iter()
        .map(|entry| entry.trim().replace('\\', "/"))
        .collect();
    if let Some(first) = declared.first() {
        bail!(
            "plugin `{}` declares the native module `{first}`; native modules are not supported \
             yet, so this package cannot be installed",
            manifest.name
        );
    }

    let mut found = Vec::new();
    collect_files(root, root, &mut found)?;
    let native: Vec<String> = found
        .into_iter()
        .map(|(relative, _)| relative)
        .filter(|relative| {
            let lowered = relative.to_ascii_lowercase();
            NATIVE_MODULE_SUFFIXES
                .iter()
                .any(|suffix| lowered.ends_with(suffix))
        })
        .collect();
    if let Some(first) = native.first() {
        bail!(
            "plugin `{}` contains the native module `{first}` without declaring it in \
             requirements.nativeModules; a package must describe what it carries",
            manifest.name
        );
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest = dst.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".stage-") {
                continue;
            }
            copy_dir_recursive(&path, &dest)?;
        } else if metadata.is_file() {
            fs::copy(&path, &dest).with_context(|| {
                format!("failed to copy {} to {}", path.display(), dest.display())
            })?;
        }
    }
    Ok(())
}

fn compute_dir_digest(root: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        let mut file = fs::File::open(&path)?;
        let mut buf = [0u8; 8192];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        hasher.update([0]);
    }
    Ok(format!("sha256:{}", to_hex(&hasher.finalize())))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_files(root, &path, out)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, path));
        }
    }
    Ok(())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set_config_dir(path: &Path) -> Self {
            let _lock = crate::test_env::lock_env();
            let previous = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("REBON_CONFIG_DIR", path);
            Self { previous, _lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.take() {
                std::env::set_var("REBON_CONFIG_DIR", previous);
            } else {
                std::env::remove_var("REBON_CONFIG_DIR");
            }
        }
    }

    fn trusted_project_config(config_dir: &Path, cwd: &Path) {
        std::fs::create_dir_all(config_dir).unwrap();
        let key = cwd.to_string_lossy().replace('\\', "/");
        let key = if cfg!(windows) {
            key.to_ascii_lowercase()
        } else {
            key
        };
        std::fs::write(
            config_dir.join("config.json"),
            serde_json::json!({
                "projects": { key: {"hasTrustDialogAccepted": true} }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn installs_builtin_alias_without_copying_package() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = PluginStore::new(tmp.path().join("home"), cwd.clone());
        let installer = PluginInstaller::new(store.clone(), cwd, Vec::new());

        let record = installer
            .install("rust-lsp", PluginScope::User, None)
            .unwrap();
        assert_eq!(record.name, "rust-lsp");
        assert_eq!(record.source_kind, PluginSourceKind::Builtin);
        assert!(!store
            .plugin_version_dir(PluginScope::User, "rust-lsp", "1")
            .exists());
    }

    #[test]
    fn installs_local_package_into_scope_store() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let cwd = tmp.path().join("project");
        let package = tmp.path().join("package");
        std::fs::create_dir_all(package.join("skills/demo")).unwrap();
        std::fs::write(
            package.join("rebon-plugin.json"),
            r#"{"name":"demo","version":"1.0.0","capabilities":{"skills":["skills/demo"]}}"#,
        )
        .unwrap();
        std::fs::write(package.join("skills/demo/SKILL.md"), "demo").unwrap();
        let store = PluginStore::new(tmp.path().join("home"), cwd.clone());
        let installer = PluginInstaller::new(store.clone(), cwd, Vec::new());

        let record = installer
            .install(package.to_str().unwrap(), PluginScope::User, None)
            .unwrap();
        assert_eq!(record.name, "demo");
        assert!(record.digest.as_deref().unwrap().starts_with("sha256:"));
        assert!(store
            .plugin_version_dir(PluginScope::User, "demo", "1.0.0")
            .join("rebon-plugin.json")
            .exists());
    }

    #[test]
    fn installing_a_bumped_version_supersedes_the_previous_one() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let cwd = tmp.path().join("project");
        let package = tmp.path().join("package");
        std::fs::create_dir_all(package.join("skills/demo")).unwrap();
        std::fs::write(package.join("skills/demo/SKILL.md"), "demo").unwrap();
        let write_manifest = |version: &str| {
            std::fs::write(
                package.join("rebon-plugin.json"),
                format!(
                    r#"{{"name":"demo","version":"{version}","capabilities":{{"skills":["skills/demo"]}}}}"#
                ),
            )
            .unwrap();
        };
        let store = PluginStore::new(tmp.path().join("home"), cwd.clone());
        let installer = PluginInstaller::new(store.clone(), cwd, Vec::new());

        write_manifest("1.0.0");
        installer
            .install(package.to_str().unwrap(), PluginScope::User, None)
            .unwrap();
        write_manifest("1.1.0");
        installer
            .install(package.to_str().unwrap(), PluginScope::User, None)
            .unwrap();

        let records = store.load_state(PluginScope::User).unwrap().plugins;
        let versions: Vec<&str> = records
            .iter()
            .filter(|record| record.name == "demo")
            .map(|record| record.version.as_str())
            .collect();
        assert_eq!(
            versions,
            vec!["1.1.0"],
            "the old version must not stay enabled beside the new one"
        );
        assert!(
            !store
                .plugin_version_dir(PluginScope::User, "demo", "1.0.0")
                .exists(),
            "the superseded package directory should be cleaned up"
        );
    }

    #[test]
    fn installs_visual_only_plugin_into_user_store() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let cwd = tmp.path().join("project");
        let package = tmp.path().join("visual-package");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("rebon-plugin.json"),
            r#"{"name":"visual-demo","version":"1.0.0","capabilities":{"appVisualEffects":[{"id":"black-hole","surface":"chatEmpty","kind":"blackHole"}]}}"#,
        )
        .unwrap();
        let store = PluginStore::new(tmp.path().join("home"), cwd.clone());
        let installer = PluginInstaller::new(store.clone(), cwd, Vec::new());

        let record = installer
            .install(package.to_str().unwrap(), PluginScope::User, None)
            .unwrap();
        assert!(record.enabled);
        assert_eq!(
            record
                .manifest
                .as_ref()
                .unwrap()
                .capabilities
                .app_visual_effects
                .len(),
            1
        );
        assert!(store
            .plugin_version_dir(PluginScope::User, "visual-demo", "1.0.0")
            .join("rebon-plugin.json")
            .exists());
    }

    /// A package archive on disk, plus the installer that would take it.
    fn archive_fixture(
        tmp: &Path,
        entries: &[(&str, &[u8])],
    ) -> (PathBuf, PluginStore, PluginInstaller) {
        let archive = tmp.join("demo-1.0.0.tar");
        std::fs::write(&archive, super::super::package::tar_archive(entries)).unwrap();
        let cwd = tmp.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = PluginStore::new(tmp.join("home"), cwd.clone());
        let installer = PluginInstaller::new(store.clone(), cwd, Vec::new());
        (archive, store, installer)
    }

    fn archive_sha256(archive: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(std::fs::read(archive).unwrap());
        to_hex(&hasher.finalize())
    }

    /// Declares a skill, so it may only be used by an archive that ships one:
    /// manifest loading checks that declared asset paths exist.
    const DEMO_MANIFEST: &[u8] =
        br#"{"name":"demo","version":"1.0.0","capabilities":{"skills":["skills/demo"]}}"#;
    /// The same plugin with nothing declared, for archives whose payload is not
    /// what the test is about.
    const PLAIN_MANIFEST: &[u8] = br#"{"name":"demo","version":"1.0.0"}"#;

    #[test]
    fn installs_from_a_package_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let (archive, store, installer) = archive_fixture(
            tmp.path(),
            &[
                ("rebon-plugin.json", DEMO_MANIFEST),
                ("skills/demo/SKILL.md", b"demo"),
            ],
        );

        let record = installer
            .install(archive.to_str().unwrap(), PluginScope::User, None)
            .unwrap();

        assert_eq!(record.name, "demo");
        assert_eq!(record.version, "1.0.0");
        assert!(record.digest.as_deref().unwrap().starts_with("sha256:"));
        let installed = store.plugin_version_dir(PluginScope::User, "demo", "1.0.0");
        assert!(installed.join("rebon-plugin.json").is_file());
        assert_eq!(
            std::fs::read(installed.join("skills").join("demo").join("SKILL.md")).unwrap(),
            b"demo"
        );
        assert!(
            !store.scope_dir(PluginScope::User).join("raw").exists(),
            "the unpack scratch does not survive"
        );
    }

    #[test]
    fn a_matching_archive_digest_installs_and_a_wrong_one_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let (archive, store, installer) =
            archive_fixture(tmp.path(), &[("rebon-plugin.json", PLAIN_MANIFEST)]);
        let digest = archive_sha256(&archive);

        let wrong = installer
            .install(
                archive.to_str().unwrap(),
                PluginScope::User,
                Some(&"0".repeat(64)),
            )
            .unwrap_err()
            .to_string();
        assert!(wrong.contains("refusing to install"), "{wrong}");
        assert!(
            !store
                .plugin_version_dir(PluginScope::User, "demo", "1.0.0")
                .exists(),
            "a refused package installs nothing"
        );

        // The same digest with the `sha256:` prefix the record uses is accepted.
        let record = installer
            .install(
                archive.to_str().unwrap(),
                PluginScope::User,
                Some(&format!("sha256:{digest}")),
            )
            .unwrap();
        assert_eq!(record.name, "demo");
    }

    #[test]
    fn a_malformed_digest_is_refused_before_the_archive_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let (archive, _store, installer) =
            archive_fixture(tmp.path(), &[("rebon-plugin.json", PLAIN_MANIFEST)]);

        let error = installer
            .install(archive.to_str().unwrap(), PluginScope::User, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("hex SHA-256"), "{error}");
    }

    /// A directory has no canonical byte sequence to hash, so silently ignoring
    /// the flag would let someone believe an unverifiable install was verified.
    #[test]
    fn a_digest_on_a_directory_source_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let package = tmp.path().join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("rebon-plugin.json"), PLAIN_MANIFEST).unwrap();
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let installer = PluginInstaller::new(
            PluginStore::new(tmp.path().join("home"), cwd.clone()),
            cwd,
            Vec::new(),
        );

        let error = installer
            .install(
                package.to_str().unwrap(),
                PluginScope::User,
                Some(&"a".repeat(64)),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("no bytes to bind"), "{error}");
    }

    /// The package format's central promise: installing copies files and does
    /// nothing else. A `package.json` full of lifecycle hooks is inert data.
    #[test]
    fn lifecycle_scripts_in_a_package_are_never_run() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let marker = tmp.path().join("postinstall-ran");
        let package_json = format!(
            r#"{{"name":"demo","scripts":{{"preinstall":"node -e \"require('fs').writeFileSync({0:?},'x')\"","postinstall":"node -e \"require('fs').writeFileSync({0:?},'x')\"","prepare":"exit 1"}}}}"#,
            marker.to_string_lossy()
        );
        let (archive, store, installer) = archive_fixture(
            tmp.path(),
            &[
                ("rebon-plugin.json", PLAIN_MANIFEST),
                ("package.json", package_json.as_bytes()),
            ],
        );

        installer
            .install(archive.to_str().unwrap(), PluginScope::User, None)
            .unwrap();

        assert!(
            !marker.exists(),
            "installing must not run lifecycle scripts"
        );
        let installed = store.plugin_version_dir(PluginScope::User, "demo", "1.0.0");
        assert_eq!(
            std::fs::read(installed.join("package.json")).unwrap(),
            package_json.as_bytes(),
            "the file is kept verbatim, as data"
        );
    }

    #[test]
    fn an_undeclared_native_module_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let (archive, store, installer) = archive_fixture(
            tmp.path(),
            &[
                ("rebon-plugin.json", PLAIN_MANIFEST),
                ("build/Release/fast.node", b"\x7fELF"),
            ],
        );

        let error = installer
            .install(archive.to_str().unwrap(), PluginScope::User, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("without declaring it"), "{error}");
        assert!(error.contains("build/Release/fast.node"), "{error}");
        assert!(!store
            .plugin_version_dir(PluginScope::User, "demo", "1.0.0")
            .exists());
    }

    #[test]
    fn a_declared_native_module_is_refused_as_not_yet_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let manifest = br#"{"name":"demo","version":"1.0.0","requirements":{"nativeModules":["build/Release/fast.node"]}}"#;
        let (archive, _store, installer) = archive_fixture(
            tmp.path(),
            &[
                ("rebon-plugin.json", manifest),
                ("build/Release/fast.node", b"\x7fELF"),
            ],
        );

        let error = installer
            .install(archive.to_str().unwrap(), PluginScope::User, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not supported yet"), "{error}");
    }

    /// The same refusal applies to a shared library under any of the platform
    /// suffixes, not just Node's own `.node`.
    #[test]
    fn shared_libraries_count_as_native_modules() {
        for name in ["lib/native.dll", "lib/native.dylib", "lib/libnative.so"] {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
            let (archive, _store, installer) = archive_fixture(
                tmp.path(),
                &[("rebon-plugin.json", PLAIN_MANIFEST), (name, b"binary")],
            );
            let error = installer
                .install(archive.to_str().unwrap(), PluginScope::User, None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("without declaring it"), "{name}: {error}");
        }
    }

    #[test]
    fn verify_reports_a_clean_install_a_drifted_one_and_a_missing_one() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let (archive, store, installer) = archive_fixture(
            tmp.path(),
            &[
                ("rebon-plugin.json", DEMO_MANIFEST),
                ("skills/demo/SKILL.md", b"demo"),
            ],
        );
        installer
            .install(archive.to_str().unwrap(), PluginScope::User, None)
            .unwrap();

        let report = installer.verify(None, None).unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].outcome, VerificationOutcome::Match);
        assert!(!report[0].is_problem());
        assert!(report[0].describe().ends_with(": ok"));

        let installed = store.plugin_version_dir(PluginScope::User, "demo", "1.0.0");
        std::fs::write(
            installed.join("skills").join("demo").join("SKILL.md"),
            "edited",
        )
        .unwrap();
        let report = installer.verify(None, None).unwrap();
        assert!(matches!(
            report[0].outcome,
            VerificationOutcome::Drifted { .. }
        ));
        assert!(report[0].is_problem());
        assert!(report[0].describe().contains("CHANGED since install"));

        std::fs::remove_dir_all(&installed).unwrap();
        let report = installer.verify(None, None).unwrap();
        assert!(matches!(
            report[0].outcome,
            VerificationOutcome::Missing { .. }
        ));
        assert!(report[0].is_problem());
    }

    #[test]
    fn verify_has_nothing_to_check_for_a_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let installer = PluginInstaller::new(
            PluginStore::new(tmp.path().join("home"), cwd.clone()),
            cwd,
            Vec::new(),
        );
        installer
            .install("rust-lsp", PluginScope::User, None)
            .unwrap();

        let report = installer.verify(None, None).unwrap();
        assert_eq!(report[0].outcome, VerificationOutcome::NotApplicable);
        assert!(!report[0].is_problem());
    }

    /// A rejected archive must not disturb what is already installed.
    #[test]
    fn a_refused_archive_leaves_the_previous_install_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set_config_dir(&tmp.path().join("home"));
        let (good, store, installer) = archive_fixture(
            tmp.path(),
            &[
                ("rebon-plugin.json", DEMO_MANIFEST),
                ("skills/demo/SKILL.md", b"original"),
            ],
        );
        installer
            .install(good.to_str().unwrap(), PluginScope::User, None)
            .unwrap();
        let before = installer.verify(None, None).unwrap();

        let bad = tmp.path().join("bad-1.0.0.tar");
        std::fs::write(
            &bad,
            super::super::package::tar_archive(&[
                ("rebon-plugin.json", PLAIN_MANIFEST),
                ("native.node", b"binary"),
            ]),
        )
        .unwrap();
        assert!(installer
            .install(bad.to_str().unwrap(), PluginScope::User, None)
            .is_err());

        assert_eq!(installer.verify(None, None).unwrap(), before);
        assert_eq!(
            std::fs::read(
                store
                    .plugin_version_dir(PluginScope::User, "demo", "1.0.0")
                    .join("skills")
                    .join("demo")
                    .join("SKILL.md")
            )
            .unwrap(),
            b"original"
        );
    }

    #[test]
    fn project_install_requires_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("home");
        let _guard = EnvGuard::set_config_dir(&config_dir);
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = PluginStore::new(config_dir, cwd.clone());
        let installer = PluginInstaller::new(store, cwd, Vec::new());

        assert!(installer
            .install("rust-lsp", PluginScope::Project, None)
            .is_err());
    }

    #[test]
    fn trusted_project_install_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("home");
        let _guard = EnvGuard::set_config_dir(&config_dir);
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        trusted_project_config(&config_dir, &cwd);
        let store = PluginStore::new(config_dir, cwd.clone());
        let installer = PluginInstaller::new(store, cwd, Vec::new());

        let record = installer
            .install("rust-lsp", PluginScope::Project, None)
            .unwrap();
        assert_eq!(record.name, "rust-lsp");
    }
}
