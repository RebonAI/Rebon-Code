//! What a package asks of the container it will run in, and what was granted.
//!
//! A plugin installed from somewhere else runs in a container
//! (`rebon-plugin-host`'s `container` module): its own Node host, reading its
//! own package and writing its own data directory, nothing else. Two things
//! it may need beyond that are a person's decision, so the package asks and
//! the install records the answer:
//!
//! * `network` — hosts its code connects to itself (an API it calls), reached
//!   through a proxy that admits exactly these;
//! * `env` — variables handed through from rebon's environment by name.
//!
//! The request lives in the package (`rebon-plugin.json`, `container`); the
//! grant lives in the config home (`plugins/grants.json`), keyed by container
//! id, because a person can narrow it without editing someone else's package
//! and an update must not widen it silently.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// The `container` block of a package manifest: what it asks for.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerRequest {
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

impl ContainerRequest {
    pub fn is_empty(&self) -> bool {
        self.network.is_empty() && self.env.is_empty()
    }
}

/// What one container was granted.
pub type ContainerGrant = ContainerRequest;

/// The network grant that admits any host: a fetch provider's, whose whole
/// job is reaching the URL it is asked for.
pub const ANY_HOST: &str = "*";

/// Whether a network grant admits any host.
pub fn admits_any_host(network: &[String]) -> bool {
    network.iter().any(|host| host == ANY_HOST)
}

/// `plugins/grants.json`: every container's grant, by container id.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ContainerGrants {
    #[serde(default)]
    pub containers: BTreeMap<String, ContainerGrant>,
}

impl ContainerGrants {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join("plugins").join("grants.json")
    }

    /// The grants on disk; none when the file is missing. An unreadable file
    /// grants nothing rather than failing the plane: the containers then start
    /// with no network and no variables, which is the safe reading.
    pub fn load(config_dir: &Path) -> Self {
        std::fs::read(Self::path(config_dir))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config_dir: &Path) -> Result<()> {
        let path = Self::path(config_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn get(&self, container: &str) -> ContainerGrant {
        self.containers.get(container).cloned().unwrap_or_default()
    }

    /// Records a grant; an empty one is no entry at all.
    pub fn set(&mut self, container: &str, grant: ContainerGrant) {
        if grant.is_empty() {
            self.containers.remove(container);
        } else {
            self.containers.insert(container.to_owned(), grant);
        }
    }

    pub fn remove(&mut self, container: &str) {
        self.containers.remove(container);
    }
}

/// The container id of an installed package: one host per package, so a
/// package's kernel plugins share a realm the way they were written to.
pub fn package_container_id(package: &str) -> String {
    format!("pkg-{}", safe_name(package))
}

/// The container id of a Claude Code mod.
pub fn mod_container_id(name: &str) -> String {
    format!("mod-{}", safe_name(name))
}

/// Where a container keeps its data: `plugins/data/<id>`.
pub fn container_data_dir(config_dir: &Path, container: &str) -> PathBuf {
    config_dir
        .join("plugins")
        .join("data")
        .join(safe_name(container))
}

/// A name a directory can carry on every platform: anything but ASCII
/// letters, digits, `.`, `_` and `-` becomes `_`, and a leading dot does not
/// survive (no hidden or `..` directories).
pub fn safe_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim_start_matches('.');
    if trimmed.is_empty() {
        "_".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_round_trip_and_an_empty_grant_is_no_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut grants = ContainerGrants::default();
        grants.set(
            "pkg-exa",
            ContainerGrant {
                network: vec!["api.exa.ai".into()],
                env: Vec::new(),
            },
        );
        grants.set("mod-snake", ContainerGrant::default());
        grants.save(dir.path()).unwrap();
        let back = ContainerGrants::load(dir.path());
        assert_eq!(back, grants);
        assert_eq!(back.get("pkg-exa").network, vec!["api.exa.ai".to_string()]);
        assert!(!back.containers.contains_key("mod-snake"));
        assert_eq!(back.get("missing"), ContainerGrant::default());
    }

    #[test]
    fn a_broken_grants_file_grants_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        std::fs::write(ContainerGrants::path(dir.path()), "{not json").unwrap();
        assert_eq!(
            ContainerGrants::load(dir.path()),
            ContainerGrants::default()
        );
    }

    #[test]
    fn container_names_are_safe_directory_names() {
        assert_eq!(
            safe_name("snake@claude-code-mods"),
            "snake_claude-code-mods"
        );
        assert_eq!(safe_name("../../etc"), "_.._etc");
        assert_eq!(safe_name("..."), "_");
        assert_eq!(
            package_container_id("@deepseek-ai/dsh-tool-web"),
            "pkg-_deepseek-ai_dsh-tool-web"
        );
        assert!(container_data_dir(Path::new("/c"), "mod-x").ends_with("plugins/data/mod-x"));
    }

    #[test]
    fn a_request_refuses_what_it_does_not_know() {
        let ok: ContainerRequest =
            serde_json::from_value(serde_json::json!({"network": ["a.b"]})).unwrap();
        assert_eq!(ok.network, vec!["a.b".to_string()]);
        assert!(
            serde_json::from_value::<ContainerRequest>(serde_json::json!({"shell": true})).is_err()
        );
    }
}
