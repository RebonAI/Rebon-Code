//! The provider store — `~/.rebon/providers/<id>.json`, one file per provider.
//!
//! The short version: a provider used to be
//! an entry in `config.json`'s `customProviders[]` array, addressable only by
//! its `name` field — which is neither unique (a plugin can claim the same id)
//! nor stable (renaming it orphans every reference). A profile, a background
//! job's frozen runtime, and `activeProvider` all need to point at a provider
//! and still find it later, so the provider needs an identity that does not
//! move: **the file name**.
//!
//! # What lives where
//!
//! The store holds *definitions*. It deliberately does not hold:
//!
//! - **which provider is active** — that is a choice, not a definition, and it
//!   stays in `config.json`. Sharing a provider file should not reach into the
//!   recipient's session and switch them onto it.
//! - **OAuth tokens** — still `.credentials.json`; `apiKey` here holds the
//!   sentinel, exactly as before.
//! - **plugin-contributed providers** — those are projected into the same
//!   shape at runtime by `rebon-harness`, never written to disk.
//!
//! # Reading and writing
//!
//! [`load`] is the single read entry point and [`save_all`] the single write
//! one; `read_config_roundtrip` / `write_config_roundtrip` in the parent
//! module route through them, so every existing provider API keeps working
//! unchanged and only its storage moved.
//!
//! Until [`migrate`] has run, the store does not exist and both functions are
//! no-ops — the parent module then reads and writes `config.json` the old way.
//! That is what makes the migration reversible: delete the directory and the
//! previous layout is still there.
//!
//! # Ordering
//!
//! Providers come back in file-name order. A directory has no intrinsic
//! order, and inventing one would mean a second file recording it — the same
//! "one concept, two places" the store exists to remove. The array's insertion
//! order is therefore not preserved across the migration; it only ever drove
//! display order in `/provider list` and the settings window.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    is_false, CustomProvider, CustomProviderModels, ProviderOptions, RebonConfigRoundTrip,
};
use rebon_types::ModelProfileMap;

/// Directory under the config home that holds one file per provider.
pub const PROVIDER_STORE_DIR: &str = "providers";

/// Schema version written into each file. Bump when a field changes meaning;
/// a reader that sees a newer version than it knows should refuse rather than
/// guess.
pub const PROVIDER_SCHEMA_VERSION: u32 = 1;

/// Marker written into `config.json` once the migration has run, so a human
/// reading the old file learns where its providers went.
pub const MIGRATED_MARKER_KEY: &str = "customProvidersMigratedTo";

pub fn provider_store_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(PROVIDER_STORE_DIR)
}

pub fn provider_file_path(config_dir: &Path, id: &str) -> PathBuf {
    provider_store_dir(config_dir).join(format!("{id}.json"))
}

/// Whether the store is in use. Before migration this is `false` and the
/// parent module stays on the `config.json` path.
pub fn is_active(config_dir: &Path) -> bool {
    provider_store_dir(config_dir).is_dir()
}

/// Derive a provider's file-name id from its display name.
///
/// See [`super::store_file_id`] for the rules; the profile store derives its
/// ids the same way, so a name maps to one spelling wherever it is stored.
pub fn provider_id(name: &str) -> String {
    super::store_file_id(name, "provider")
}

/// Transport-level switches. Grouped so they stop sitting in the same flat
/// namespace as model selection, where nothing distinguished "how we connect"
/// from "which model runs".
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredTransport {
    #[serde(default, skip_serializing_if = "is_false")]
    use_websocket: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_scoped_transient_context: Option<bool>,
}

impl StoredTransport {
    fn is_empty(&self) -> bool {
        !self.use_websocket && self.request_scoped_transient_context.is_none()
    }
}

/// Reasoning-related switches, grouped for the same reason as [`StoredTransport`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_effort: Option<String>,
    /// `reasoningMode` in the old flat layout. Only `"pro"` is defined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

impl StoredReasoning {
    fn is_empty(&self) -> bool {
        self.thinking_enabled.is_none() && self.thinking_effort.is_none() && self.mode.is_none()
    }
}

/// On-disk shape of one `providers/<id>.json`.
///
/// Three renames against the old flat entry, each one removing an ambiguity:
/// `name` became the file name plus an optional `displayName` (a name was
/// doing double duty as identity and as label); `model` became
/// `defaultModel` (it was never "the current model", only where a new session
/// starts); and the loose transport/reasoning flags moved into their own
/// objects.
///
/// `models` accepts both shapes the old entry accepted — a bare list of ids or
/// a map with per-model details — so migrating cannot drop a per-model
/// `contextWindow` or `options`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredProvider {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    /// Human-facing label. Absent means "use the id".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    /// Explicit vendor pin for endpoints whose host does not identify the
    /// company behind them. Absent means "detect from `baseUrl`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vendor: Option<String>,
    base_url: String,
    api_key: String,
    /// Model a *new* session on this provider starts from. Not the current
    /// model — a running session carries its own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    default_model: String,
    #[serde(default, skip_serializing_if = "CustomProviderModels::is_empty")]
    models: CustomProviderModels,
    /// Declared roles win; every undeclared role follows the session's model.
    #[serde(default, skip_serializing_if = "ModelProfileMap::is_empty")]
    model_profiles: ModelProfileMap,
    #[serde(default, skip_serializing_if = "ProviderOptions::is_empty")]
    options: ProviderOptions,
    #[serde(default, skip_serializing_if = "StoredTransport::is_empty")]
    transport: StoredTransport,
    #[serde(default, skip_serializing_if = "StoredReasoning::is_empty")]
    reasoning: StoredReasoning,
    /// Everything this struct does not model, preserved verbatim. A key a
    /// user hand-wrote must survive a round trip through any front end.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

fn default_schema_version() -> u32 {
    PROVIDER_SCHEMA_VERSION
}

impl StoredProvider {
    fn from_custom(provider: &CustomProvider, id: &str) -> Self {
        let display_name = (provider.name != id).then(|| provider.name.clone());
        Self {
            schema_version: PROVIDER_SCHEMA_VERSION,
            display_name,
            format: provider.format.clone(),
            vendor: provider.vendor.clone(),
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            default_model: provider.model.clone(),
            models: provider.models.clone(),
            model_profiles: provider.model_profiles.clone(),
            options: provider.options.clone(),
            transport: StoredTransport {
                use_websocket: provider.use_websocket,
                request_scoped_transient_context: provider.request_scoped_transient_context,
            },
            reasoning: StoredReasoning {
                thinking_enabled: provider.thinking_enabled,
                thinking_effort: provider.thinking_effort.clone(),
                mode: provider.reasoning_mode.clone(),
            },
            extra: provider.extra.clone(),
        }
    }

    fn into_custom(self, id: &str) -> CustomProvider {
        CustomProvider {
            name: self.display_name.unwrap_or_else(|| id.to_string()),
            format: self.format,
            vendor: self.vendor,
            base_url: self.base_url,
            api_key: self.api_key,
            model: self.default_model,
            models: self.models,
            model_profiles: self.model_profiles,
            options: self.options,
            request_scoped_transient_context: self.transport.request_scoped_transient_context,
            use_websocket: self.transport.use_websocket,
            thinking_enabled: self.reasoning.thinking_enabled,
            thinking_effort: self.reasoning.thinking_effort,
            reasoning_mode: self.reasoning.mode,
            extra: self.extra,
        }
    }
}

/// Read every provider in the store, in file-name order.
///
/// `Ok(None)` means the store does not exist yet — the caller stays on
/// `config.json`. A single unreadable or unparseable file is skipped with a
/// warning rather than failing the load: one broken provider must not make
/// Rebon unstartable when the others are fine.
pub(crate) fn load(config_dir: &Path) -> anyhow::Result<Option<Vec<CustomProvider>>> {
    let dir = provider_store_dir(config_dir);
    if !dir.is_dir() {
        return Ok(None);
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            return Err(
                anyhow::Error::from(err).context(format!("failed to read provider store {dir:?}"))
            );
        }
    };
    // BTreeMap gives file-name order without a second sort, and drops the
    // duplicate that a case-differing file name would otherwise produce.
    let mut by_id: BTreeMap<String, CustomProvider> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_ascii_lowercase)
        else {
            continue;
        };
        match read_provider_file(&path) {
            Ok(stored) => {
                by_id.insert(id.clone(), stored.into_custom(&id));
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "provider store: skipping unreadable provider file"
                );
            }
        }
    }
    Ok(Some(by_id.into_values().collect()))
}

fn read_provider_file(path: &Path) -> anyhow::Result<StoredProvider> {
    let bytes = std::fs::read(path)
        .map_err(|err| anyhow::Error::from(err).context(format!("read {path:?}")))?;
    let stored: StoredProvider =
        serde_json::from_slice(&bytes).map_err(|err| anyhow::anyhow!("parse {path:?}: {err}"))?;
    if stored.schema_version > PROVIDER_SCHEMA_VERSION {
        anyhow::bail!(
            "provider file {path:?} declares schemaVersion {} but this build understands at most {PROVIDER_SCHEMA_VERSION}",
            stored.schema_version
        );
    }
    Ok(stored)
}

/// Write the given set as the complete contents of the store.
///
/// Files for providers no longer in the list are deleted — the caller's vector
/// is the whole truth, which is what lets `remove_custom_provider` keep working
/// by just dropping an element. Writes go through a temp file + rename so an
/// interrupted save cannot leave a half-written provider behind.
pub(crate) fn save_all(config_dir: &Path, providers: &[CustomProvider]) -> anyhow::Result<()> {
    let dir = provider_store_dir(config_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|err| anyhow::Error::from(err).context(format!("create {dir:?}")))?;

    let mut keep = BTreeMap::new();
    for provider in providers {
        let id = unique_id(&provider.name, &keep);
        keep.insert(id, provider.clone());
    }

    for (id, provider) in &keep {
        write_provider_file(
            &provider_file_path(config_dir, id),
            &StoredProvider::from_custom(provider, id),
        )?;
    }

    // Anything left on disk is a provider the caller dropped.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let stale = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| !keep.contains_key(&stem.to_ascii_lowercase()))
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// An id that does not collide with one already claimed in this write.
///
/// Two providers whose names differ only in case or punctuation reduce to the
/// same id; suffixing keeps both rather than letting the second overwrite the
/// first.
fn unique_id(name: &str, taken: &BTreeMap<String, CustomProvider>) -> String {
    let base = provider_id(name);
    if !taken.contains_key(&base) {
        return base;
    }
    for suffix in 2..1000 {
        let candidate = format!("{base}-{suffix}");
        if !taken.contains_key(&candidate) {
            return candidate;
        }
    }
    base
}

fn write_provider_file(path: &Path, provider: &StoredProvider) -> anyhow::Result<()> {
    let serialized = serde_json::to_vec_pretty(provider)
        .map_err(|err| anyhow::anyhow!("serialize provider {path:?}: {err}"))?;
    // Same 0600 as config.json: these files carry API keys.
    rebon_session::write_private_file_atomically(path, &serialized)
        .map_err(|err| anyhow::Error::from(err).context(format!("write {path:?}")))?;
    Ok(())
}

/// What [`migrate`] did, for the caller to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// The store already existed; nothing to do.
    AlreadyMigrated,
    /// No providers were configured, so an empty store was created.
    NothingToMigrate,
    /// Providers were written out to the store.
    Migrated { count: usize, backup: PathBuf },
}

/// Move `config.json`'s `customProviders[]` into the store.
///
/// Reversible on purpose: `config.json` keeps its array for one release as a
/// read-only fallback, and the original file is backed up first. If any step
/// fails the whole thing is abandoned without writing the marker — a failed
/// migration must leave a working install, not a half-moved one.
pub fn migrate(config_dir: &Path) -> anyhow::Result<MigrationOutcome> {
    if is_active(config_dir) {
        return Ok(MigrationOutcome::AlreadyMigrated);
    }
    let config = super::read_config_roundtrip_raw(config_dir)?;
    if config.custom_providers.is_empty() {
        // Create the directory anyway: its existence is what switches the
        // read/write path over, and a user with no providers yet should still
        // land in the new layout when they add their first one.
        std::fs::create_dir_all(provider_store_dir(config_dir))?;
        return Ok(MigrationOutcome::NothingToMigrate);
    }

    let config_path = super::config_json_path(config_dir);
    let backup = config_path.with_extension("json.bak-provider-store");
    std::fs::copy(&config_path, &backup).map_err(|err| {
        anyhow::Error::from(err).context(format!("failed to back up {config_path:?}"))
    })?;

    if let Err(err) = save_all(config_dir, &config.custom_providers) {
        // Roll the directory back so the next start retries cleanly instead of
        // reading a partial store.
        let _ = std::fs::remove_dir_all(provider_store_dir(config_dir));
        return Err(err.context("failed to write the provider store; config.json is untouched"));
    }

    // Leave the array in place as a fallback, and say where the real copy is.
    let mut config = config;
    config.extra.insert(
        MIGRATED_MARKER_KEY.to_string(),
        serde_json::Value::String(format!("{PROVIDER_STORE_DIR}/")),
    );
    let count = config.custom_providers.len();
    super::write_config_roundtrip_raw(config_dir, &config)?;

    Ok(MigrationOutcome::Migrated { count, backup })
}

/// The store rendered as `config.json`'s `customProviders[]` array shape.
///
/// For front ends that already parse and merge that array shape field by
/// field — the settings window does, precisely so it never drops a key
/// it does not model. Handing them the same shape moves their storage without
/// touching that logic; a rewrite of it would risk the very thing it protects.
///
/// `Ok(None)` when the store does not exist yet.
pub fn read_as_config_array(config_dir: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    let Some(providers) = load(config_dir)? else {
        return Ok(None);
    };
    let array = providers
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| anyhow::anyhow!("failed to render the provider store as an array: {err}"))?;
    Ok(Some(serde_json::Value::Array(array)))
}

/// Replace the store's contents from a `customProviders[]` array.
///
/// The inverse of [`read_as_config_array`]. An element that does not parse as
/// a provider aborts the whole write: a partially applied save would leave the
/// user looking at a provider list that does not match what is on disk.
pub fn write_from_config_array(config_dir: &Path, array: &serde_json::Value) -> anyhow::Result<()> {
    let Some(items) = array.as_array() else {
        anyhow::bail!("customProviders must be an array");
    };
    let providers = items
        .iter()
        .map(|item| {
            serde_json::from_value::<CustomProvider>(item.clone())
                .map_err(|err| anyhow::anyhow!("failed to parse a provider entry: {err}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    save_all(config_dir, &providers)
}

/// Merge the store over `config.json`'s array for a read.
///
/// The store wins where both have a provider; entries only in `config.json`
/// come along behind it. That fallback is what makes the migration a
/// one-release overlap rather than a flag day — and it warns, because a
/// provider that never made it into the store is a migration that half
/// happened.
pub(crate) fn merge_for_read(config_dir: &Path, config: &mut RebonConfigRoundTrip) {
    let stored = match load(config_dir) {
        Ok(Some(stored)) => stored,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(error = %err, "provider store: falling back to config.json providers");
            return;
        }
    };
    let known: std::collections::BTreeSet<String> = stored
        .iter()
        .map(|provider| provider.name.to_ascii_lowercase())
        .collect();
    let mut merged = stored;
    for from_config in config.custom_providers.drain(..) {
        if known.contains(&from_config.name.to_ascii_lowercase()) {
            continue;
        }
        tracing::warn!(
            provider = %from_config.name,
            "provider store: provider only exists in config.json; run the migration again"
        );
        merged.push(from_config);
    }
    config.custom_providers = merged;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        add_custom_provider_model_in, list_custom_providers_from, remove_custom_provider_in,
        resolve_from_dir, set_custom_provider_profile_in,
    };
    use tempfile::TempDir;

    const PRE_STORE_CONFIG: &str = r#"{
  "theme": "light",
  "activeCustomProvider": "DeepSeek",
  "customProviders": [
    {
      "name": "openai",
      "format": "openai-responses",
      "baseUrl": "https://chatgpt.com/backend-api/codex/responses",
      "apiKey": "$OPENAI_OAUTH_TOKEN",
      "model": "gpt-5.6-sol",
      "models": ["gpt-5.5", { "id": "gpt-5.6-sol", "contextWindow": 500000 }],
      "useWebsocket": true,
      "handWrittenKey": { "kept": true }
    },
    {
      "name": "DeepSeek",
      "format": "openai",
      "baseUrl": "https://api.deepseek.com",
      "apiKey": "sk-test",
      "model": "deepseek-v4-pro",
      "models": { "deepseek-v4-flash": { "contextWindow": 1000000 } },
      "modelProfiles": { "small": "deepseek-v4-flash" },
      "options": { "extraBody": { "prompt_cache_retention": "24h" } },
      "thinkingEnabled": true,
      "thinkingEffort": "max"
    }
  ],
  "projects": { "a/b": { "trust": true } }
}"#;

    fn pre_store_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(crate::config_json_path(tmp.path()), PRE_STORE_CONFIG).unwrap();
        tmp
    }

    fn read_config_value(dir: &Path) -> serde_json::Value {
        let bytes = std::fs::read(crate::config_json_path(dir)).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn provider_id_is_a_safe_stable_file_name() {
        assert_eq!(provider_id("DeepSeek"), "deepseek");
        assert_eq!(provider_id("  openai  "), "openai");
        // A name that would otherwise escape the directory or nest.
        assert_eq!(provider_id("openai/codex"), "openai-codex");
        assert_eq!(provider_id("../../etc/passwd"), "etc-passwd");
        assert_eq!(provider_id("中文 名字"), "provider");
    }

    #[test]
    fn migration_writes_one_file_per_provider_and_keeps_a_fallback() {
        let tmp = pre_store_dir();

        let outcome = migrate(tmp.path()).unwrap();
        let MigrationOutcome::Migrated { count, backup } = outcome else {
            panic!("expected a migration, got {outcome:?}");
        };
        assert_eq!(count, 2);
        assert!(backup.exists(), "the original config.json is backed up");

        assert!(provider_file_path(tmp.path(), "openai").exists());
        assert!(provider_file_path(tmp.path(), "deepseek").exists());

        // config.json keeps the array (one-release fallback) plus a marker
        // saying where the real copy now lives, and never loses unrelated keys.
        let config = read_config_value(tmp.path());
        assert_eq!(config[MIGRATED_MARKER_KEY], "providers/");
        assert_eq!(config["activeCustomProvider"], "DeepSeek");
        assert_eq!(config["projects"]["a/b"]["trust"], true);

        // Running it again is a no-op rather than a second backup.
        assert_eq!(
            migrate(tmp.path()).unwrap(),
            MigrationOutcome::AlreadyMigrated
        );
    }

    #[test]
    fn migration_preserves_every_field_including_unmodelled_keys() {
        let tmp = pre_store_dir();
        migrate(tmp.path()).unwrap();

        let openai: serde_json::Value = serde_json::from_slice(
            &std::fs::read(provider_file_path(tmp.path(), "openai")).unwrap(),
        )
        .unwrap();
        assert_eq!(openai["schemaVersion"], 1);
        // `model` became `defaultModel`; the old name meant "current model" to
        // nobody but looked like it did.
        assert_eq!(openai["defaultModel"], "gpt-5.6-sol");
        assert!(openai.get("model").is_none());
        // Flat transport flags are grouped now.
        assert_eq!(openai["transport"]["useWebsocket"], true);
        assert!(openai.get("useWebsocket").is_none());
        // A key this crate does not model survives verbatim.
        assert_eq!(openai["handWrittenKey"]["kept"], true);
        // `name` equals the id here, so no redundant displayName is written.
        assert!(openai.get("displayName").is_none());

        let deepseek: serde_json::Value = serde_json::from_slice(
            &std::fs::read(provider_file_path(tmp.path(), "deepseek")).unwrap(),
        )
        .unwrap();
        // Casing that the id cannot carry is preserved as the label.
        assert_eq!(deepseek["displayName"], "DeepSeek");
        assert_eq!(deepseek["modelProfiles"]["small"], "deepseek-v4-flash");
        assert_eq!(
            deepseek["models"]["deepseek-v4-flash"]["contextWindow"],
            1_000_000
        );
        assert_eq!(
            deepseek["options"]["extraBody"]["prompt_cache_retention"],
            "24h"
        );
        assert_eq!(deepseek["reasoning"]["thinkingEnabled"], true);
        assert_eq!(deepseek["reasoning"]["thinkingEffort"], "max");
    }

    #[test]
    fn resolution_is_identical_before_and_after_migrating() {
        let before = pre_store_dir();
        let after = pre_store_dir();
        migrate(after.path()).unwrap();

        let before = resolve_from_dir(before.path()).unwrap().unwrap();
        let after = resolve_from_dir(after.path()).unwrap().unwrap();

        assert_eq!(before.name, after.name);
        assert_eq!(before.base_url, after.base_url);
        assert_eq!(before.api_key, after.api_key);
        assert_eq!(before.model, after.model);
        assert_eq!(before.model_profiles, after.model_profiles);
        assert_eq!(before.model_context_windows, after.model_context_windows);
        assert_eq!(
            before.request_scoped_transient_context,
            after.request_scoped_transient_context
        );
        assert_eq!(before.use_websocket, after.use_websocket);
    }

    #[test]
    fn writes_after_migration_land_in_the_store_not_config_json() {
        let tmp = pre_store_dir();
        migrate(tmp.path()).unwrap();

        add_custom_provider_model_in(tmp.path(), "deepseek", "deepseek-v4-turbo").unwrap();
        set_custom_provider_profile_in(
            tmp.path(),
            "deepseek",
            "explore",
            Some("deepseek-v4-flash"),
            None,
        )
        .unwrap();

        let stored: serde_json::Value = serde_json::from_slice(
            &std::fs::read(provider_file_path(tmp.path(), "deepseek")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored["modelProfiles"]["explore"], "deepseek-v4-flash");

        // The array in config.json is emptied out, so the two copies cannot
        // disagree about what a provider is.
        let config = read_config_value(tmp.path());
        assert!(
            config.get("customProviders").is_none(),
            "config.json still carries providers: {config}"
        );
        assert_eq!(config["activeCustomProvider"], "DeepSeek");

        // …and reads still see everything.
        let listed = list_custom_providers_from(tmp.path());
        let deepseek = listed
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case("deepseek"))
            .unwrap();
        assert!(deepseek.models.iter().any(|m| m == "deepseek-v4-turbo"));
    }

    #[test]
    fn removing_a_provider_deletes_its_file() {
        let tmp = pre_store_dir();
        migrate(tmp.path()).unwrap();

        remove_custom_provider_in(tmp.path(), "openai").unwrap();

        assert!(!provider_file_path(tmp.path(), "openai").exists());
        assert!(provider_file_path(tmp.path(), "deepseek").exists());
        assert_eq!(list_custom_providers_from(tmp.path()).len(), 1);
    }

    #[test]
    fn a_provider_left_only_in_config_json_is_still_read() {
        // The half-migrated shape: the store exists but is missing an entry
        // that config.json still has. Losing it silently would be worse than
        // reading it from the old place and warning.
        let tmp = pre_store_dir();
        migrate(tmp.path()).unwrap();
        std::fs::remove_file(provider_file_path(tmp.path(), "deepseek")).unwrap();
        std::fs::write(crate::config_json_path(tmp.path()), PRE_STORE_CONFIG).unwrap();

        let listed = list_custom_providers_from(tmp.path());
        assert_eq!(listed.len(), 2);
        // Still resolvable, which is the point of the fallback.
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.name, "DeepSeek");
    }

    #[test]
    fn an_unreadable_provider_file_is_skipped_not_fatal() {
        let tmp = pre_store_dir();
        migrate(tmp.path()).unwrap();
        // Any write empties the array in config.json, so this is the steady
        // state — the store alone, no fallback to paper over a bad file.
        remove_custom_provider_in(tmp.path(), "nonexistent").ok();
        add_custom_provider_model_in(tmp.path(), "deepseek", "deepseek-v4-turbo").unwrap();
        assert!(read_config_value(tmp.path())
            .get("customProviders")
            .is_none());

        std::fs::write(provider_file_path(tmp.path(), "openai"), "{ not json").unwrap();

        // The broken one drops out; the healthy one still loads. An install
        // must not become unstartable over one bad file.
        let listed = list_custom_providers_from(tmp.path());
        assert_eq!(listed.len(), 1);
        assert!(listed[0].name.eq_ignore_ascii_case("deepseek"));
    }

    #[test]
    fn the_config_json_array_still_rescues_a_broken_file_during_the_overlap() {
        // Before the first write empties it, `config.json` is a live fallback:
        // a provider whose store file went bad is read from the old place
        // rather than vanishing.
        let tmp = pre_store_dir();
        migrate(tmp.path()).unwrap();
        std::fs::write(provider_file_path(tmp.path(), "openai"), "{ not json").unwrap();

        let listed = list_custom_providers_from(tmp.path());
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|p| p.name.eq_ignore_ascii_case("openai")));
    }

    #[test]
    fn a_newer_schema_version_is_refused_rather_than_guessed() {
        let tmp = pre_store_dir();
        migrate(tmp.path()).unwrap();
        let path = provider_file_path(tmp.path(), "openai");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["schemaVersion"] = serde_json::json!(PROVIDER_SCHEMA_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        assert!(read_provider_file(&path).is_err());
    }

    #[test]
    fn migrating_an_install_with_no_providers_still_switches_to_the_store() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(crate::config_json_path(tmp.path()), r#"{"theme":"light"}"#).unwrap();

        assert_eq!(
            migrate(tmp.path()).unwrap(),
            MigrationOutcome::NothingToMigrate
        );
        assert!(is_active(tmp.path()));
        // A first provider added afterwards lands in the store.
        crate::add_custom_provider_in(
            tmp.path(),
            "local",
            "openai",
            "http://localhost:11434/v1",
            "ollama",
            "llama3",
        )
        .unwrap();
        assert!(provider_file_path(tmp.path(), "local").exists());
    }

    #[test]
    fn two_names_that_reduce_to_one_id_both_survive() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            crate::config_json_path(tmp.path()),
            r#"{"customProviders":[
                {"name":"my provider","baseUrl":"https://a.example","apiKey":"a","model":"m"},
                {"name":"My/Provider","baseUrl":"https://b.example","apiKey":"b","model":"m"}
            ]}"#,
        )
        .unwrap();

        migrate(tmp.path()).unwrap();

        assert!(provider_file_path(tmp.path(), "my-provider").exists());
        assert!(provider_file_path(tmp.path(), "my-provider-2").exists());
        assert_eq!(list_custom_providers_from(tmp.path()).len(), 2);
    }
}
