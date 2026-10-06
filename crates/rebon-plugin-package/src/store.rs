use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::manifest::PluginManifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginScope {
    User,
    Project,
}

impl PluginScope {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            other => anyhow::bail!("invalid plugin scope `{other}`; expected user or project"),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginSourceKind {
    Local,
    Builtin,
}

/// Recorded by the installer; a package's source/name is not proof of origin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PluginSourceIdentity {
    Local {
        path: PathBuf,
    },
    Builtin {
        name: String,
    },
    BuiltinMarketplace {
        plugin: String,
    },
    Marketplace {
        catalog: crate::marketplace::MarketplaceSource,
        plugin: String,
        source: crate::marketplace::PluginSource,
    },
}

impl PluginSourceIdentity {
    pub fn local(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self::Local {
            path: Self::canonical_path(path)?,
        })
    }

    fn canonical_path(path: &std::path::Path) -> anyhow::Result<PathBuf> {
        let canonical = path.canonicalize()?;
        Ok(PathBuf::from(rebon_session::cwd_identity(
            &canonical.to_string_lossy(),
        )))
    }

    /// Versions and revisions identify artifacts, not origins; keep the full package name and registry.
    /// Remote addresses remain case-sensitive unless their equivalence can be established.
    pub fn marketplace(
        mut catalog: crate::marketplace::MarketplaceSource,
        plugin: String,
        mut source: crate::marketplace::PluginSource,
    ) -> anyhow::Result<Self> {
        use crate::marketplace::{MarketplaceSource, PluginSource, RemoteSource};
        match &mut catalog {
            MarketplaceSource::Github { git_ref, .. } | MarketplaceSource::Git { git_ref, .. } => {
                *git_ref = None
            }
            MarketplaceSource::Directory { path } | MarketplaceSource::File { path } => {
                *path = Self::canonical_path(path)?;
            }
            MarketplaceSource::Url { .. } => {}
        }
        if let PluginSource::Remote(remote) = &mut source {
            match remote {
                RemoteSource::Github { git_ref, sha, .. }
                | RemoteSource::Url { git_ref, sha, .. }
                | RemoteSource::GitSubdir { git_ref, sha, .. } => {
                    *git_ref = None;
                    *sha = None;
                }
                RemoteSource::Npm { version, .. } => *version = None,
                RemoteSource::Archive { sha256, .. } => *sha256 = None,
                RemoteSource::Command { .. } => {}
            }
        }
        Ok(Self::Marketplace {
            catalog,
            plugin,
            source,
        })
    }

    pub fn key(&self) -> String {
        serde_json::to_string(self)
            .expect("plugin source identity contains only serializable fields")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPluginRecord {
    pub name: String,
    pub version: String,
    pub enabled: bool,
    #[serde(default)]
    pub disabled_capabilities: Vec<String>,
    pub source_kind: PluginSourceKind,
    #[serde(default)]
    pub source: Option<String>,
    /// Missing legacy origins stay unknown; neither the current catalog nor package claims prove them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<PluginSourceIdentity>,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub manifest: Option<PluginManifest>,
}

impl InstalledPluginRecord {
    fn matches_source(&self, source: &PluginSourceIdentity) -> bool {
        if let Some(recorded) = &self.source_identity {
            return recorded == source;
        }
        match (&self.source_kind, self.source.as_deref(), source) {
            (PluginSourceKind::Builtin, Some(recorded), PluginSourceIdentity::Builtin { name }) => {
                recorded == name && self.name == *name
            }
            (PluginSourceKind::Local, Some(recorded), PluginSourceIdentity::Local { path }) => {
                std::path::Path::new(recorded).is_absolute()
                    && rebon_session::cwd_identity(recorded) == path.to_string_lossy()
            }
            _ => false,
        }
    }

    pub fn source_label(&self, scope: PluginScope) -> String {
        match self.source_kind {
            PluginSourceKind::Local => format!("plugin:{}@local:{}", self.name, scope.as_str()),
            PluginSourceKind::Builtin => format!("plugin:{}@builtin:{}", self.name, scope.as_str()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstallState {
    #[serde(default)]
    pub plugins: Vec<InstalledPluginRecord>,
}

#[derive(Debug, Clone)]
pub struct PluginStore {
    user_plugins_dir: PathBuf,
    project_plugins_dir: PathBuf,
}

impl PluginStore {
    pub fn new(config_home: PathBuf, cwd: PathBuf) -> Self {
        Self {
            user_plugins_dir: config_home.join("plugins"),
            project_plugins_dir: cwd.join(".rebon").join("plugins"),
        }
    }

    pub fn scope_dir(&self, scope: PluginScope) -> &PathBuf {
        match scope {
            PluginScope::User => &self.user_plugins_dir,
            PluginScope::Project => &self.project_plugins_dir,
        }
    }

    pub fn state_path(&self, scope: PluginScope) -> PathBuf {
        self.scope_dir(scope).join("installed.json")
    }

    pub fn plugin_version_dir(&self, scope: PluginScope, name: &str, version: &str) -> PathBuf {
        self.scope_dir(scope).join(name).join(version)
    }

    pub fn load_state(&self, scope: PluginScope) -> anyhow::Result<PluginInstallState> {
        let path = self.state_path(scope);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PluginInstallState::default())
            }
            Err(err) => return Err(err).map_err(anyhow::Error::from),
        };
        serde_json::from_slice(&bytes)
            .map_err(anyhow::Error::from)
            .map_err(|err| err.context(format!("failed to parse plugin state {}", path.display())))
    }

    pub fn save_state_atomic(
        &self,
        scope: PluginScope,
        state: &PluginInstallState,
    ) -> anyhow::Result<()> {
        let dir = self.scope_dir(scope);
        std::fs::create_dir_all(dir)?;
        let target = self.state_path(scope);
        let data = serde_json::to_vec_pretty(state)?;
        rebon_session::write_file_atomically(&target, &data)?;
        // What is installed just changed. `discover` re-checks the file stamps
        // on its own, but a write and a read inside one clock tick would look
        // unchanged to it; this is the half of the answer only the writer has.
        crate::discovery::invalidate();
        Ok(())
    }

    pub fn ensure_source_available(
        &self,
        name: &str,
        source: &PluginSourceIdentity,
        runtime_ids: &[String],
        target_scope: Option<PluginScope>,
        replace_source: bool,
    ) -> anyhow::Result<()> {
        for scope in [PluginScope::User, PluginScope::Project] {
            for record in self.load_state(scope)?.plugins {
                let runtime_conflict = runtime_ids.iter().find(|id| {
                    record.manifest.as_ref().is_some_and(|manifest| {
                        manifest.capabilities.kernel_plugins.contains_key(*id)
                    })
                });
                if record.name != name && runtime_conflict.is_none() {
                    continue;
                }
                if record.name == name
                    && (record.matches_source(source)
                        || (replace_source && target_scope == Some(scope)))
                {
                    continue;
                }
                if record.name == name && target_scope == Some(scope) {
                    anyhow::bail!(
                        "plugin `{name}` has another or unknown source in {} scope; use --replace-source to confirm replacement without deleting its data",
                        scope.as_str()
                    );
                }
                anyhow::bail!(
                    "plugin `{name}` conflicts with installed plugin `{}` in {} scope (runtime id: {}); remove the conflicting install before installing this source",
                    record.name, scope.as_str(), runtime_conflict.map(String::as_str).unwrap_or(name)
                );
            }
        }
        Ok(())
    }

    pub fn upsert_record(
        &self,
        scope: PluginScope,
        record: InstalledPluginRecord,
    ) -> anyhow::Result<()> {
        let mut state = self.load_state(scope)?;
        state.plugins.retain(|existing| {
            !(existing.name == record.name && existing.version == record.version)
        });
        state.plugins.push(record);
        state
            .plugins
            .sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        self.save_state_atomic(scope, &state)
    }

    #[cfg(test)]
    pub fn find_record(
        &self,
        scope: PluginScope,
        name: &str,
    ) -> anyhow::Result<Option<InstalledPluginRecord>> {
        let mut matches: Vec<_> = self
            .load_state(scope)?
            .plugins
            .into_iter()
            .filter(|record| record.name == name)
            .collect();
        matches.sort_by(|a, b| b.version.cmp(&a.version));
        Ok(matches.into_iter().next())
    }

    pub fn set_enabled(
        &self,
        scope: PluginScope,
        name: &str,
        enabled: bool,
    ) -> anyhow::Result<InstalledPluginRecord> {
        let mut state = self.load_state(scope)?;
        let Some(record) = state.plugins.iter_mut().find(|record| record.name == name) else {
            anyhow::bail!(
                "plugin `{name}` is not installed in {} scope",
                scope.as_str()
            );
        };
        record.enabled = enabled;
        let updated = record.clone();
        self.save_state_atomic(scope, &state)?;
        Ok(updated)
    }

    /// Remove one exact `(name, version)` record, leaving other versions alone.
    pub fn remove_version_record(
        &self,
        scope: PluginScope,
        name: &str,
        version: &str,
    ) -> anyhow::Result<()> {
        let mut state = self.load_state(scope)?;
        let before = state.plugins.len();
        state
            .plugins
            .retain(|record| !(record.name == name && record.version == version));
        if state.plugins.len() != before {
            self.save_state_atomic(scope, &state)?;
        }
        Ok(())
    }

    pub fn uninstall(
        &self,
        scope: PluginScope,
        name: &str,
    ) -> anyhow::Result<Option<InstalledPluginRecord>> {
        let mut state = self.load_state(scope)?;
        let before = state.plugins.len();
        let mut removed = None;
        state.plugins.retain(|record| {
            if record.name == name && removed.is_none() {
                removed = Some(record.clone());
                false
            } else {
                true
            }
        });
        if state.plugins.len() != before {
            self.save_state_atomic(scope, &state)?;
        }
        Ok(removed)
    }

    pub fn list_records(
        &self,
        scope: Option<PluginScope>,
    ) -> anyhow::Result<Vec<(PluginScope, InstalledPluginRecord)>> {
        let scopes: Vec<PluginScope> = match scope {
            Some(scope) => vec![scope],
            None => vec![PluginScope::Project, PluginScope::User],
        };
        let mut out = Vec::new();
        for scope in scopes {
            for record in self.load_state(scope)?.plugins {
                out.push((scope, record));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sourced_record(name: &str, source: Option<PluginSourceIdentity>) -> InstalledPluginRecord {
        let mut record: InstalledPluginRecord = serde_json::from_value(serde_json::json!({
            "name": name, "version": "1.0.0", "enabled": true, "sourceKind": "local",
            "manifest": { "name": name, "version": "1.0.0", "source": "builtin:forged",
                "capabilities": { "kernelPlugins": { "shared": { "entry": "index.mjs" } } } }
        }))
        .unwrap();
        record.source_identity = source;
        record
    }

    #[test]
    fn marketplace_identity_retains_npm_scope_but_not_versions() {
        use crate::marketplace::{MarketplaceSource, PluginSource, RemoteSource};
        let identity = |package: &str, version: &str| {
            PluginSourceIdentity::marketplace(
                MarketplaceSource::Github {
                    repo: "owner/catalog".into(),
                    git_ref: Some(version.into()),
                },
                "tool".into(),
                PluginSource::Remote(RemoteSource::Npm {
                    package: package.into(),
                    version: Some(version.into()),
                    registry: None,
                }),
            )
            .unwrap()
        };
        assert_eq!(identity("@one/tool", "1"), identity("@one/tool", "2"));
        assert_ne!(identity("@one/tool", "1"), identity("@two/tool", "1"));
        let key = identity("@one/tool", "1").key();
        assert!(key.contains("@one/tool"));
        assert_eq!(
            serde_json::from_str::<PluginSourceIdentity>(&key).unwrap(),
            identity("@one/tool", "1")
        );
    }

    #[test]
    fn source_identity_distinguishes_origins_and_kinds() {
        let local = PluginSourceIdentity::Local {
            path: PathBuf::from("same"),
        };
        let builtin = PluginSourceIdentity::Builtin {
            name: "same".into(),
        };
        assert_ne!(local.key(), builtin.key());
        assert_ne!(
            local.key(),
            PluginSourceIdentity::Local {
                path: PathBuf::from("other")
            }
            .key()
        );
    }

    #[test]
    fn install_source_allows_same_source_upgrade_without_using_manifest_source() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        let source = PluginSourceIdentity::Local {
            path: tmp.path().join("source"),
        };
        store
            .upsert_record(
                PluginScope::User,
                sourced_record("demo", Some(source.clone())),
            )
            .unwrap();
        assert!(store
            .ensure_source_available(
                "demo",
                &source,
                &["shared".into()],
                Some(PluginScope::User),
                false
            )
            .is_ok());
        assert!(store
            .ensure_source_available(
                "demo",
                &PluginSourceIdentity::Builtin {
                    name: "forged".into()
                },
                &["shared".into()],
                Some(PluginScope::User),
                false
            )
            .is_err());
    }

    #[test]
    fn install_source_rejects_another_package_claiming_the_runtime_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        let source = PluginSourceIdentity::Local {
            path: tmp.path().join("source"),
        };
        store
            .upsert_record(
                PluginScope::Project,
                sourced_record("one", Some(source.clone())),
            )
            .unwrap();
        let before = store.load_state(PluginScope::Project).unwrap();
        assert!(store
            .ensure_source_available(
                "two",
                &source,
                &["shared".into()],
                Some(PluginScope::User),
                false
            )
            .is_err());
        assert_eq!(store.load_state(PluginScope::Project).unwrap(), before);
        assert!(store
            .ensure_source_available(
                "two",
                &source,
                &["unrelated".into()],
                Some(PluginScope::User),
                false
            )
            .is_ok());
    }

    #[test]
    fn legacy_install_source_stays_unknown_and_cannot_be_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        let record = sourced_record("demo", None);
        assert!(record.source_identity.is_none());
        store.upsert_record(PluginScope::User, record).unwrap();
        let source = PluginSourceIdentity::Local {
            path: tmp.path().join("source"),
        };
        let error = store
            .ensure_source_available("demo", &source, &[], Some(PluginScope::User), false)
            .unwrap_err();
        assert!(error.to_string().contains("unknown source"));
    }

    #[test]
    fn legacy_builtin_and_canonical_local_sources_can_be_recognized() {
        let tmp = tempfile::tempdir().unwrap();
        let local = PluginSourceIdentity::local(tmp.path()).unwrap();
        assert_eq!(
            local,
            PluginSourceIdentity::local(&tmp.path().join(".")).unwrap()
        );
        let mut record = sourced_record("demo", None);
        record.source = Some(
            tmp.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        assert!(record.matches_source(&local));
        record.source = Some("relative/path".into());
        assert!(!record.matches_source(&local));
        record.source_kind = PluginSourceKind::Builtin;
        record.source = Some("demo".into());
        assert!(record.matches_source(&PluginSourceIdentity::Builtin {
            name: "demo".into()
        }));
        record.source = Some("different".into());
        assert!(!record.matches_source(&PluginSourceIdentity::Builtin {
            name: "demo".into()
        }));
    }

    #[test]
    fn replacement_is_limited_to_the_same_package_and_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        store
            .upsert_record(PluginScope::User, sourced_record("demo", None))
            .unwrap();
        let source = PluginSourceIdentity::local(tmp.path()).unwrap();
        assert!(store
            .ensure_source_available("demo", &source, &[], Some(PluginScope::User), true)
            .is_ok());
        let error = store
            .ensure_source_available("demo", &source, &[], Some(PluginScope::Project), true)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("--replace-source"), "{error}");
        assert!(error.contains("demo") && error.contains("user"), "{error}");
        assert!(store
            .ensure_source_available(
                "other",
                &source,
                &["shared".into()],
                Some(PluginScope::User),
                true
            )
            .is_err());
        assert!(store.load_state(PluginScope::User).unwrap().plugins[0]
            .source_identity
            .is_none());
    }

    #[test]
    fn round_trips_scope_state_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        let record = InstalledPluginRecord {
            name: "demo".into(),
            version: "1.0.0".into(),
            enabled: true,
            disabled_capabilities: Vec::new(),
            source_kind: PluginSourceKind::Builtin,
            source: Some("rust-lsp".into()),
            source_identity: None,
            digest: None,
            manifest: None,
        };

        store
            .upsert_record(PluginScope::User, record.clone())
            .unwrap();
        assert_eq!(
            store.find_record(PluginScope::User, "demo").unwrap(),
            Some(record)
        );
        assert!(store.state_path(PluginScope::User).exists());
        assert!(!store.state_path(PluginScope::Project).exists());
    }

    #[test]
    fn project_and_user_scopes_are_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        let mut user = InstalledPluginRecord {
            name: "demo".into(),
            version: "1.0.0".into(),
            enabled: true,
            disabled_capabilities: Vec::new(),
            source_kind: PluginSourceKind::Builtin,
            source: Some("a".into()),
            source_identity: None,
            digest: None,
            manifest: None,
        };
        let mut project = user.clone();
        project.enabled = false;
        user.source = Some("user".into());
        project.source = Some("project".into());
        store
            .upsert_record(PluginScope::User, user.clone())
            .unwrap();
        store
            .upsert_record(PluginScope::Project, project.clone())
            .unwrap();

        assert_eq!(
            store.find_record(PluginScope::User, "demo").unwrap(),
            Some(user)
        );
        assert_eq!(
            store.find_record(PluginScope::Project, "demo").unwrap(),
            Some(project)
        );
    }
}
