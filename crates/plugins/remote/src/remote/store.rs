//! `~/.rebon/remotes.json`.
//!
//! A sidecar rather than a section of `config.json`, following
//! `agents.json`: the root config object is shared with the desktop
//! app and the mobile bridge, and a key added there is a schema change
//! three surfaces have to agree on. Remotes are written by exactly one
//! writer (`rebon remote …`) and read by two, so they get their own
//! file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::remote::host::{HostError, RemoteHost};

/// Bumped only when an older Rebon would misread the file. Unknown
/// future versions are refused rather than parsed optimistically —
/// silently dropping a host the user configured is worse than an
/// error that says to upgrade.
pub const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStore {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub hosts: Vec<RemoteHost>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} was written by a newer Rebon (format {found}, this build understands {STORE_VERSION}) — upgrade to read it")]
    FutureVersion { path: PathBuf, found: u32 },
    #[error("no remote named `{name}`{}", suggestions(.available))]
    UnknownHost {
        name: String,
        available: Vec<String>,
    },
    #[error("a remote named `{0}` already exists — pass --force to replace it")]
    DuplicateHost(String),
    #[error(transparent)]
    Host(#[from] HostError),
}

fn suggestions(available: &[String]) -> String {
    if available.is_empty() {
        " — add one with `rebon remote add <name> <user@host>`".to_string()
    } else {
        format!(" (configured: {})", available.join(", "))
    }
}

/// Path of the remotes sidecar for a config directory.
pub fn remotes_json_path(config_dir: &Path) -> PathBuf {
    config_dir.join("remotes.json")
}

impl RemoteStore {
    /// Read the store, treating "no file yet" as "no remotes".
    pub fn load(config_dir: &Path) -> Result<Self, StoreError> {
        let path = remotes_json_path(config_dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: STORE_VERSION,
                    hosts: Vec::new(),
                })
            }
            Err(source) => return Err(StoreError::Read { path, source }),
        };
        // An empty or whitespace-only file is what a crashed write
        // leaves behind. Treat it as absent instead of failing every
        // later command until the user deletes it by hand.
        if raw.trim().is_empty() {
            return Ok(Self {
                version: STORE_VERSION,
                hosts: Vec::new(),
            });
        }
        let store: Self = serde_json::from_str(&raw).map_err(|source| StoreError::Parse {
            path: path.clone(),
            source,
        })?;
        if store.version > STORE_VERSION {
            return Err(StoreError::FutureVersion {
                path,
                found: store.version,
            });
        }
        Ok(store)
    }

    /// Write the store atomically.
    ///
    /// Temp file plus rename, because the alternative — truncating the
    /// real file and writing into it — turns a crash mid-write into a
    /// config with every remote gone.
    pub fn save(&self, config_dir: &Path) -> Result<(), StoreError> {
        let path = remotes_json_path(config_dir);
        std::fs::create_dir_all(config_dir).map_err(|source| StoreError::Write {
            path: config_dir.to_path_buf(),
            source,
        })?;
        let mut body = serde_json::to_string_pretty(&Self {
            version: STORE_VERSION,
            hosts: self.hosts.clone(),
        })
        .map_err(|source| StoreError::Parse {
            path: path.clone(),
            source,
        })?;
        body.push('\n');

        rebon_session::write_file_atomically(&path, body.as_bytes()).map_err(|source| {
            StoreError::Write {
                path: path.clone(),
                source,
            }
        })?;
        Ok(())
    }

    pub fn names(&self) -> Vec<String> {
        self.hosts.iter().map(|host| host.name.clone()).collect()
    }

    /// Look a host up case-insensitively, the way `/agent` resolves
    /// agent ids.
    pub fn get(&self, name: &str) -> Option<&RemoteHost> {
        let wanted = name.trim();
        self.hosts
            .iter()
            .find(|host| host.name.eq_ignore_ascii_case(wanted))
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut RemoteHost> {
        let wanted = name.trim();
        self.hosts
            .iter_mut()
            .find(|host| host.name.eq_ignore_ascii_case(wanted))
    }

    pub fn require(&self, name: &str) -> Result<&RemoteHost, StoreError> {
        self.get(name).ok_or_else(|| StoreError::UnknownHost {
            name: name.to_string(),
            available: self.names(),
        })
    }

    /// Add a host, or replace one of the same name when `force`.
    pub fn insert(&mut self, host: RemoteHost, force: bool) -> Result<(), StoreError> {
        host.validate()?;
        match self
            .hosts
            .iter()
            .position(|existing| existing.name.eq_ignore_ascii_case(&host.name))
        {
            Some(_) if !force => Err(StoreError::DuplicateHost(host.name)),
            Some(idx) => {
                self.hosts[idx] = host;
                Ok(())
            }
            None => {
                self.hosts.push(host);
                Ok(())
            }
        }
    }

    pub fn remove(&mut self, name: &str) -> Result<RemoteHost, StoreError> {
        let wanted = name.trim();
        match self
            .hosts
            .iter()
            .position(|host| host.name.eq_ignore_ascii_case(wanted))
        {
            Some(idx) => Ok(self.hosts.remove(idx)),
            None => Err(StoreError::UnknownHost {
                name: name.to_string(),
                available: self.names(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::host::InstallStrategy;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rebon-remote-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn host(name: &str) -> RemoteHost {
        RemoteHost::from_target(name, "deploy@host.example").expect("host")
    }

    #[test]
    fn a_missing_file_reads_as_an_empty_store() {
        let dir = temp_dir("missing");
        let store = RemoteStore::load(&dir).expect("load");
        assert!(store.hosts.is_empty());
        assert_eq!(store.version, STORE_VERSION);
    }

    #[test]
    fn a_truncated_file_reads_as_empty_rather_than_wedging_every_command() {
        let dir = temp_dir("truncated");
        std::fs::write(remotes_json_path(&dir), "   \n").unwrap();
        let store = RemoteStore::load(&dir).expect("load");
        assert!(store.hosts.is_empty());
    }

    #[test]
    fn malformed_json_is_an_error_that_names_the_file() {
        let dir = temp_dir("malformed");
        std::fs::write(remotes_json_path(&dir), "{ not json").unwrap();
        let err = RemoteStore::load(&dir).unwrap_err();
        assert!(matches!(err, StoreError::Parse { .. }), "{err:?}");
        assert!(err.to_string().contains("remotes.json"), "{err}");
    }

    #[test]
    fn a_newer_format_is_refused_instead_of_silently_dropping_hosts() {
        let dir = temp_dir("future");
        std::fs::write(
            remotes_json_path(&dir),
            r#"{"version": 99, "hosts": [], "somethingNew": true}"#,
        )
        .unwrap();
        let err = RemoteStore::load(&dir).unwrap_err();
        assert!(
            matches!(err, StoreError::FutureVersion { found: 99, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_saved_store_round_trips() {
        let dir = temp_dir("roundtrip");
        let mut store = RemoteStore::default();
        let mut prod = host("prod");
        prod.install = InstallStrategy::Push;
        prod.path = Some("/srv/app".into());
        store.insert(prod.clone(), false).unwrap();
        store.save(&dir).unwrap();

        let back = RemoteStore::load(&dir).expect("load");
        assert_eq!(back.hosts, vec![prod]);
        assert_eq!(back.version, STORE_VERSION);
    }

    #[test]
    fn saving_stamps_the_current_version_even_on_a_default_store() {
        let dir = temp_dir("stamp");
        // `RemoteStore::default()` has version 0; a save must not
        // persist that, or the next load would think the file predates
        // the format.
        RemoteStore::default().save(&dir).unwrap();
        let raw = std::fs::read_to_string(remotes_json_path(&dir)).unwrap();
        assert!(raw.contains(r#""version": 1"#), "{raw}");
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = temp_dir("tmp-cleanup");
        RemoteStore::default().save(&dir).unwrap();
        assert!(!dir.join("remotes.json.tmp").exists());
    }

    #[test]
    fn a_save_over_an_existing_file_replaces_it() {
        // Exercises the Windows remove-then-rename path.
        let dir = temp_dir("overwrite");
        let mut store = RemoteStore::default();
        store.insert(host("a"), false).unwrap();
        store.save(&dir).unwrap();
        store.insert(host("b"), false).unwrap();
        store.save(&dir).unwrap();
        assert_eq!(RemoteStore::load(&dir).unwrap().names(), vec!["a", "b"]);
    }

    #[test]
    fn duplicate_names_need_force() {
        let mut store = RemoteStore::default();
        store.insert(host("prod"), false).unwrap();
        let err = store.insert(host("prod"), false).unwrap_err();
        assert!(matches!(err, StoreError::DuplicateHost(_)), "{err:?}");
        store.insert(host("prod"), true).unwrap();
        assert_eq!(store.hosts.len(), 1);
    }

    #[test]
    fn a_replacement_keeps_its_position_in_the_list() {
        let mut store = RemoteStore::default();
        store.insert(host("a"), false).unwrap();
        store.insert(host("b"), false).unwrap();
        store.insert(host("c"), false).unwrap();
        let mut updated = host("b");
        updated.path = Some("/new".into());
        store.insert(updated, true).unwrap();
        assert_eq!(store.names(), vec!["a", "b", "c"]);
        assert_eq!(store.get("b").unwrap().path.as_deref(), Some("/new"));
    }

    #[test]
    fn lookup_folds_case_like_the_agent_switch_does() {
        let mut store = RemoteStore::default();
        store.insert(host("Prod-Web"), false).unwrap();
        assert!(store.get("prod-web").is_some());
        assert!(store.get("PROD-WEB").is_some());
        // And a duplicate that differs only in case is still a
        // duplicate, or `/agent` would see two agents with one id.
        assert!(store.insert(host("PROD-web"), false).is_err());
    }

    #[test]
    fn an_unknown_host_error_lists_what_is_configured() {
        let mut store = RemoteStore::default();
        store.insert(host("prod"), false).unwrap();
        let err = store.require("staging").unwrap_err();
        assert!(err.to_string().contains("prod"), "{err}");

        let empty = RemoteStore::default();
        let err = empty.require("prod").unwrap_err();
        assert!(err.to_string().contains("rebon remote add"), "{err}");
    }

    #[test]
    fn remove_returns_the_host_it_deleted() {
        let mut store = RemoteStore::default();
        store.insert(host("prod"), false).unwrap();
        let removed = store.remove("PROD").unwrap();
        assert_eq!(removed.name, "prod");
        assert!(store.hosts.is_empty());
        assert!(store.remove("prod").is_err());
    }

    #[test]
    fn an_invalid_host_is_rejected_before_it_reaches_disk() {
        let mut store = RemoteStore::default();
        let mut bad = host("prod");
        bad.host = String::new();
        assert!(store.insert(bad, false).is_err());
        assert!(store.hosts.is_empty());
    }
}
