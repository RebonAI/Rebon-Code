use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{parse_installation_type, InstallationType};

pub const UPDATE_STATE_FILE_NAME: &str = "update_state.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedUpdateState {
    pub last_run_unix_ms: u64,
    #[serde(
        serialize_with = "serialize_installation_type",
        deserialize_with = "deserialize_installation_type"
    )]
    pub last_detected_installation_source: InstallationType,
    pub check_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub auto_install_observed: bool,
    pub installation_attempted: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl PersistedUpdateState {
    pub fn dry_run(
        now_unix_ms: u64,
        installation_source: InstallationType,
        check_status: impl Into<String>,
        last_error: Option<String>,
        auto_install_observed: bool,
    ) -> Self {
        Self {
            last_run_unix_ms: now_unix_ms,
            last_detected_installation_source: installation_source,
            check_status: check_status.into(),
            last_error,
            auto_install_observed,
            installation_attempted: false,
            extra: BTreeMap::new(),
        }
    }
}

pub fn update_state_path(config_home: &Path) -> PathBuf {
    config_home.join(UPDATE_STATE_FILE_NAME)
}

pub fn serialize_update_state(state: &PersistedUpdateState) -> serde_json::Result<String> {
    serde_json::to_string_pretty(state).map(|mut json| {
        json.push('\n');
        json
    })
}

pub fn deserialize_update_state(json: &str) -> serde_json::Result<PersistedUpdateState> {
    serde_json::from_str(json)
}

pub fn read_update_state(path: &Path) -> io::Result<Option<PersistedUpdateState>> {
    match fs::read_to_string(path) {
        Ok(raw) => deserialize_update_state(&raw)
            .map(Some)
            .map_err(invalid_data),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn write_update_state(path: &Path, state: &PersistedUpdateState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serialize_update_state(state).map_err(invalid_data)?;
    fs::write(path, raw)
}

fn invalid_data(err: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

fn serialize_installation_type<S>(
    value: &InstallationType,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(value.as_str())
}

fn deserialize_installation_type<'de, D>(deserializer: D) -> Result<InstallationType, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse_installation_type(&raw)
        .ok_or_else(|| serde::de::Error::custom(format!("unknown installation type `{raw}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_state_json_round_trips_and_preserves_unknown_fields() {
        let raw = r#"{
  "lastRunUnixMs": 1710000000000,
  "lastDetectedInstallationSource": "npm-global",
  "checkStatus": "skipped",
  "lastError": "network disabled for dry run",
  "autoInstallObserved": true,
  "installationAttempted": false,
  "futureField": { "kept": true }
}"#;

        let state = deserialize_update_state(raw).expect("state parses");
        assert_eq!(state.last_run_unix_ms, 1_710_000_000_000);
        assert_eq!(
            state.last_detected_installation_source,
            InstallationType::NpmGlobal
        );
        assert!(!state.installation_attempted);
        assert!(state.extra.contains_key("futureField"));

        let serialized = serialize_update_state(&state).expect("state serializes");
        assert!(serialized.contains("\"futureField\""));
        assert!(serialized.contains("\"lastDetectedInstallationSource\": \"npm-global\""));
    }

    #[test]
    fn update_state_writes_and_reads_temp_directory() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-update-state-test-")
            .tempdir()
            .expect("temp dir");
        let path = update_state_path(dir.path());
        let state = PersistedUpdateState::dry_run(
            42,
            InstallationType::Development,
            "skipped",
            None,
            false,
        );

        write_update_state(&path, &state).expect("write state");
        let read = read_update_state(&path)
            .expect("read state")
            .expect("state exists");
        assert_eq!(read, state);
    }
}
