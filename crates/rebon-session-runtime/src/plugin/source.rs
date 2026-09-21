use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use super::builtin::{builtin_alias, BuiltinPluginAlias};
use super::manifest::{PluginManifest, PLUGIN_MANIFEST_FILE};
use super::package::is_package_archive;

#[derive(Debug, Clone)]
pub(crate) enum ResolvedPluginSource {
    LocalPath {
        root: PathBuf,
        manifest: PluginManifest,
    },
    BuiltinAlias(BuiltinPluginAlias),
    ConfiguredDir {
        root: PathBuf,
        manifest: PluginManifest,
    },
    /// A self-contained package archive. Unlike the directory arms this carries
    /// no manifest: reading one means unpacking, and unpacking is a validated
    /// step the installer owns rather than something resolution does silently.
    Archive {
        archive: PathBuf,
    },
}

pub(crate) fn resolve_install_source(
    raw: &str,
    cwd: &Path,
    plugin_dirs: &[PathBuf],
) -> anyhow::Result<ResolvedPluginSource> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("plugin source must not be empty");
    }

    let candidate = crate::rebon_config::resolve_against_cwd(cwd, trimmed);
    if is_package_archive(&candidate) {
        if !candidate.is_file() {
            bail!("plugin package {} does not exist", candidate.display());
        }
        let archive = candidate
            .canonicalize()
            .with_context(|| format!("failed to resolve plugin package {}", candidate.display()))?;
        return Ok(ResolvedPluginSource::Archive { archive });
    }
    if candidate.join(PLUGIN_MANIFEST_FILE).is_file() {
        let root = candidate
            .canonicalize()
            .with_context(|| format!("failed to resolve plugin path {}", candidate.display()))?;
        let manifest = PluginManifest::load_from_dir(&root)?;
        return Ok(ResolvedPluginSource::LocalPath { root, manifest });
    }

    if let Some(alias) = builtin_alias(trimmed) {
        return Ok(ResolvedPluginSource::BuiltinAlias(alias));
    }

    for dir in plugin_dirs {
        let base = if dir.is_absolute() {
            dir.clone()
        } else {
            cwd.join(dir)
        };
        let candidate = base.join(trimmed);
        if candidate.join(PLUGIN_MANIFEST_FILE).is_file() {
            let root = candidate.canonicalize().with_context(|| {
                format!(
                    "failed to resolve configured plugin path {}",
                    candidate.display()
                )
            })?;
            let manifest = PluginManifest::load_from_dir(&root)?;
            return Ok(ResolvedPluginSource::ConfiguredDir { root, manifest });
        }
    }

    bail!(
        "unknown plugin `{trimmed}`; v1 supports local paths, package archives (.tgz/.tar.gz/.tar), --plugin-dir local packages, and built-in aliases such as rust-lsp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_builtin_alias_after_local_path_probe() {
        let resolved = resolve_install_source("rust-lsp", Path::new("."), &[]).unwrap();
        assert!(matches!(resolved, ResolvedPluginSource::BuiltinAlias(_)));
    }

    #[test]
    fn unknown_bare_name_reports_local_only_v1() {
        let err = resolve_install_source("not-real", Path::new("."), &[]).unwrap_err();
        assert!(err.to_string().contains("v1 supports local paths"));
    }

    #[test]
    fn resolves_a_package_archive_by_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("demo-1.0.0.tgz");
        std::fs::write(&archive, b"not read during resolution").unwrap();

        let resolved = resolve_install_source("demo-1.0.0.tgz", tmp.path(), &[]).unwrap();
        assert!(matches!(resolved, ResolvedPluginSource::Archive { .. }));
    }

    /// An archive name that is not on disk must say so, rather than falling
    /// through to "unknown plugin" and hiding a typo in a path.
    #[test]
    fn a_missing_archive_reports_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_install_source("gone.tar.gz", tmp.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn resolves_configured_plugin_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path().join("plugins").join("demo");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(PLUGIN_MANIFEST_FILE),
            r#"{"name":"demo","version":"1.0.0"}"#,
        )
        .unwrap();

        let resolved =
            resolve_install_source("demo", tmp.path(), &[PathBuf::from("plugins")]).unwrap();
        assert!(matches!(
            resolved,
            ResolvedPluginSource::ConfiguredDir { .. }
        ));
    }
}
