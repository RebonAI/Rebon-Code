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
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub manifest: Option<PluginManifest>,
}

impl InstalledPluginRecord {
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
