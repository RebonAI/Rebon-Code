//! Asking npm what the latest published version is, and turning the answer
//! into an [`UpdateDecision`].
//!
//! The one place in the plugin that touches the network. Everything it needs
//! to decide with — the channel, the dist-tag, the comparison — comes from
//! [`crate::updater`]; everything it needs to suppress with (a skipped or
//! dismissed version) comes from [`UpdatePreferences`]. It returns a value and
//! prints nothing, so the terminal, `/status` and `rebon update status` all
//! read the same result.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use rebon_config::UpdatePreferences;
use serde::Deserialize;

use crate::updater::channel::{default_channel, npm_tag, parse_channel, ReleaseChannel};
use crate::updater::update_decision::{decide_update, UpdateDecision};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const PACKAGE_OVERRIDE_ENV: &str = "REBON_UPDATE_PACKAGE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub package_name: String,
    pub source: String,
    pub command: String,
    pub checked_at: SystemTime,
    pub decision: UpdateDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageNameError {}

impl std::fmt::Display for PackageNameError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

impl std::error::Error for PackageNameError {}

#[derive(Debug, Deserialize)]
struct NpmRegistryMetadata {
    #[serde(rename = "dist-tags")]
    dist_tags: BTreeMap<String, String>,
}

pub async fn check_for_update() -> anyhow::Result<UpdateCheckResult> {
    let prefs = rebon_config::load_update_preferences()?;
    check_for_update_with_preferences(reqwest::Client::new(), &prefs).await
}

async fn check_for_update_with_preferences(
    client: reqwest::Client,
    prefs: &UpdatePreferences,
) -> anyhow::Result<UpdateCheckResult> {
    if prefs.disabled {
        anyhow::bail!("update checks disabled by config");
    }
    let channel = prefs
        .channel_name()
        .and_then(parse_channel)
        .unwrap_or_else(default_channel);
    check_for_update_with(client, channel, prefs).await
}

async fn check_for_update_with(
    client: reqwest::Client,
    channel: ReleaseChannel,
    prefs: &UpdatePreferences,
) -> anyhow::Result<UpdateCheckResult> {
    let package_name = configured_package_name()?;
    let tag = npm_tag(channel);
    let url = format!(
        "https://registry.npmjs.org/{}",
        package_name.replace('/', "%2f")
    );

    let metadata = client
        .get(&url)
        .timeout(DEFAULT_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .json::<NpmRegistryMetadata>()
        .await?;

    let latest_version = latest_version_from_metadata(&metadata, tag).ok_or_else(|| {
        anyhow::anyhow!("npm metadata for {package_name} had no {tag:?} dist-tag")
    })?;
    Ok(build_result(package_name, url, tag, latest_version, prefs))
}

fn build_result(
    package_name: String,
    source: String,
    tag: &str,
    latest_version: String,
    prefs: &UpdatePreferences,
) -> UpdateCheckResult {
    // Every crate in the workspace carries `version.workspace = true`, so this
    // is the published `rebon` version whether the plugin or the binary asks.
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let mut decision = decide_update(
        false,
        Some(current_version.as_str()),
        Some(latest_version.as_str()),
        None,
    );
    if prefs.skipped_version.as_deref() == Some(latest_version.as_str())
        || prefs.dismissed_version.as_deref() == Some(latest_version.as_str())
    {
        decision = UpdateDecision::SkippedByMinimumVersion;
    }
    let command = format!("npm install -g {package_name}@{tag}");
    UpdateCheckResult {
        current_version,
        latest_version,
        package_name,
        source,
        command,
        checked_at: SystemTime::now(),
        decision,
    }
}

fn latest_version_from_metadata(metadata: &NpmRegistryMetadata, tag: &str) -> Option<String> {
    metadata
        .dist_tags
        .get(tag)
        .filter(|s| !s.is_empty())
        .cloned()
}

pub fn configured_package_name() -> Result<String, PackageNameError> {
    if let Ok(value) = std::env::var(PACKAGE_OVERRIDE_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    default_package_name(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn default_package_name(_os: &str, _arch: &str) -> Result<String, PackageNameError> {
    Ok("@rebon/cli".to_string())
}

#[cfg(test)]
pub fn parse_metadata_and_decide(
    json: &str,
    tag: &str,
    current_version: &str,
    minimum_version: Option<&str>,
) -> anyhow::Result<(String, UpdateDecision)> {
    let metadata: NpmRegistryMetadata = serde_json::from_str(json)?;
    let latest = latest_version_from_metadata(&metadata, tag)
        .ok_or_else(|| anyhow::anyhow!("missing {tag:?} dist-tag"))?;
    let decision = decide_update(false, Some(current_version), Some(&latest), minimum_version);
    Ok((latest, decision))
}

#[cfg(test)]
pub fn parse_metadata_and_decide_with_preferences(
    json: &str,
    prefs: &UpdatePreferences,
    current_version: &str,
) -> anyhow::Result<(String, String, UpdateDecision)> {
    let metadata: NpmRegistryMetadata = serde_json::from_str(json)?;
    let channel = prefs
        .channel_name()
        .and_then(parse_channel)
        .unwrap_or_else(default_channel);
    let tag = npm_tag(channel);
    let latest = latest_version_from_metadata(&metadata, tag)
        .ok_or_else(|| anyhow::anyhow!("missing {tag:?} dist-tag"))?;
    let result = build_result(
        "rebon-test-package".to_string(),
        "test".to_string(),
        tag,
        latest,
        prefs,
    );
    let mut decision = result.decision;
    if matches!(decision, UpdateDecision::Update { .. }) {
        decision = decide_update(
            false,
            Some(current_version),
            Some(&result.latest_version),
            None,
        );
        if prefs.skipped_version.as_deref() == Some(result.latest_version.as_str())
            || prefs.dismissed_version.as_deref() == Some(result.latest_version.as_str())
        {
            decision = UpdateDecision::SkippedByMinimumVersion;
        }
    }
    Ok((result.latest_version, tag.to_string(), decision))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_unified_default_package_name() {
        assert_eq!(
            default_package_name("windows", "x86_64").unwrap(),
            "@rebon/cli"
        );
        assert_eq!(
            default_package_name("freebsd", "mips64").unwrap(),
            "@rebon/cli"
        );
    }

    #[test]
    fn parses_npm_dist_tag_and_decides_update() {
        let json = r#"{
            "name": "@rebon/cli",
            "dist-tags": { "latest": "0.0.87", "stable": "0.0.86" }
        }"#;
        let (latest, decision) = parse_metadata_and_decide(json, "latest", "0.0.86", None).unwrap();
        assert_eq!(latest, "0.0.87");
        assert_eq!(
            decision,
            UpdateDecision::Update {
                target_version: "0.0.87".to_string()
            }
        );
    }

    #[test]
    fn parses_npm_dist_tag_and_decides_already_current() {
        let json = r#"{ "dist-tags": { "latest": "0.0.87" } }"#;
        let (latest, decision) = parse_metadata_and_decide(json, "latest", "0.0.87", None).unwrap();
        assert_eq!(latest, "0.0.87");
        assert_eq!(decision, UpdateDecision::AlreadyAtOrAboveLatest);
    }

    #[test]
    fn missing_dist_tag_is_error() {
        let json = r#"{ "dist-tags": { "stable": "0.0.86" } }"#;
        let err = parse_metadata_and_decide(json, "latest", "0.0.85", None).unwrap_err();
        assert!(err.to_string().contains("missing \"latest\" dist-tag"));
    }

    #[test]
    fn skipped_version_suppresses_update_notice_decision() {
        let json = r#"{ "dist-tags": { "latest": "99.0.0" } }"#;
        let prefs = UpdatePreferences {
            skipped_version: Some("99.0.0".to_string()),
            ..UpdatePreferences::default()
        };
        let (latest, tag, decision) =
            parse_metadata_and_decide_with_preferences(json, &prefs, "0.0.1").unwrap();
        assert_eq!(latest, "99.0.0");
        assert_eq!(tag, "latest");
        assert_eq!(decision, UpdateDecision::SkippedByMinimumVersion);
    }

    #[test]
    fn dismissed_version_suppresses_update_notice_decision() {
        let json = r#"{ "dist-tags": { "latest": "99.0.0" } }"#;
        let prefs = UpdatePreferences {
            dismissed_version: Some("99.0.0".to_string()),
            dismissed_at_ms: Some(1234),
            ..UpdatePreferences::default()
        };
        let (latest, _tag, decision) =
            parse_metadata_and_decide_with_preferences(json, &prefs, "0.0.1").unwrap();
        assert_eq!(latest, "99.0.0");
        assert_eq!(decision, UpdateDecision::SkippedByMinimumVersion);
    }

    #[test]
    fn configured_channel_selects_stable_dist_tag() {
        let json = r#"{
            "dist-tags": { "latest": "99.0.0", "stable": "98.0.0" }
        }"#;
        let prefs = UpdatePreferences {
            channel: Some("stable".to_string()),
            ..UpdatePreferences::default()
        };
        let (latest, tag, decision) =
            parse_metadata_and_decide_with_preferences(json, &prefs, "0.0.1").unwrap();
        assert_eq!(latest, "98.0.0");
        assert_eq!(tag, "stable");
        assert_eq!(
            decision,
            UpdateDecision::Update {
                target_version: "98.0.0".to_string()
            }
        );
    }
}
