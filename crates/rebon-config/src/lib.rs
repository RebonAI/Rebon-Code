//! Read the rebon config directory (`~/.rebon` or
//! `$REBON_CONFIG_DIR`) and resolve the currently-active custom
//! provider into a ready-to-use [`ResolvedProvider`].
//!
//! The resolver combines active custom-provider selection with OpenAI
//! token lookup. It skips the intermediate env-var mutation step and
//! hands the resolved config straight to the caller so there is no
//! process-global state involved.
//!
//! ## Data sources
//!
//! * **`<config_dir>/config.json`** — primary config. Read fields:
//!   - `activeCustomProvider` (a provider name, or `null`)
//!   - `disabledSkills` (optional array of skill names to deny)
//!   - `customProviders`, an array of objects with `name`, `format`,
//!     `baseUrl`, `apiKey`, `model`, and an optional `useWebsocket`
//!   Every other field is ignored by readers and preserved by the
//!   round-trip-safe write APIs.
//! * **`<config_dir>/.credentials.json`** — secure storage. Read
//!   fields:
//!   - `openaiOAuth`, an object with `accessToken` and optional
//!     `refreshToken` / `expiresAt`
//!   Unknown keys are preserved on re-serialize (see
//!   [`Credentials::extra`]) so the refresh path can rewrite
//!   this file without losing sibling entries.
//!
//! ## Fallback semantics
//!
//! * Missing `config.json` → `Ok(None)`. The caller should then
//!   fall back to the env-var-based model client construction so
//!   rebon still works in CI and fresh environments that have no
//!   `.rebon` dir.
//! * Present `config.json` but no `activeCustomProvider` → `Ok(None)`.
//! * Present `activeCustomProvider` but name not in `customProviders`
//!   → `Err` with a clear message — this is a corrupted config and
//!   falling back silently would be confusing.
//! * Active provider uses the `$OPENAI_OAUTH_TOKEN` sentinel but
//!   `.credentials.json` is missing or has no `openaiOAuth` entry
//!   → `Err` with "please run `rebon` and `/login`" hint.
//! * Active provider uses the sentinel and credentials ARE present
//!   but expired → still returns `Ok(Some(resolved))`; the caller
//!   is expected to either refresh the token or bail.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rebon_api::OpenAiRequestOptions;
use rebon_permissions::PermissionMode;
use rebon_types::{ModelProfileMap, ReasoningEffort, SubAgentModelConfig, SubAgentModelSelection};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Config change observer
//
// Every write this crate makes to a config file ends by telling one
// observer, installed by the host (the kernel bootstrap turns it into a
// `ConfigChanged` event). This crate stays free of the kernel; the host
// stays free of knowing which function wrote which file.

/// Which file a config write touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileKind {
    Config,
    Settings,
    Credentials,
}

/// `(which file, its path, which plugin namespace)`.
///
/// The third argument is `Some` only when the write went through the settings
/// seat and touched exactly one `plugins.<id>` namespace — the case where a
/// subscriber can decide, without reading anything, that the change was not
/// about it. Everything else is `None`: a config write, a switch flip, a hand
/// edit noticed some other way.
type ConfigChangeObserver = Box<dyn Fn(ConfigFileKind, &Path, Option<&str>) + Send + Sync>;

static CONFIG_CHANGE_OBSERVER: std::sync::OnceLock<ConfigChangeObserver> =
    std::sync::OnceLock::new();

/// Install the process-wide observer. Returns `false` if one is already
/// installed (the first host wins; there is one kernel per process).
pub fn install_config_change_observer(
    observer: impl Fn(ConfigFileKind, &Path, Option<&str>) + Send + Sync + 'static,
) -> bool {
    CONFIG_CHANGE_OBSERVER.set(Box::new(observer)).is_ok()
}

fn notify_config_changed(kind: ConfigFileKind, path: &Path) {
    notify_config_changed_for(kind, path, None);
}

fn notify_config_changed_for(kind: ConfigFileKind, path: &Path, namespace: Option<&str>) {
    if let Some(observer) = CONFIG_CHANGE_OBSERVER.get() {
        observer(kind, path, namespace);
    }
}

/// Sentinel `apiKey` value that tells [`resolve_from_dir`]
/// to look up the real token in `.credentials.json`.
///
/// A stored provider keeps this literal value instead of a secret;
/// the real access token is read from secure storage when the provider
/// is resolved.
pub const OPENAI_OAUTH_TOKEN_SENTINEL: &str = "$OPENAI_OAUTH_TOKEN";
/// Synthetic provider name used for the built-in OpenAI Codex OAuth
/// entry written by the rebon `/login` flow.
pub const OPENAI_OAUTH_PROVIDER_NAME: &str = "openai";
/// Base URL used by the built-in OpenAI Codex OAuth provider.
pub const OPENAI_OAUTH_PROVIDER_BASE_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// Default model used by the built-in OpenAI Codex OAuth provider.
pub const OPENAI_OAUTH_PROVIDER_MODEL: &str = "gpt-5.4";
/// Models offered by the built-in OpenAI Codex OAuth provider. The
/// first entry becomes the provider's active model on login. The
/// gpt-5.6 family is Sol (flagship; `gpt-5.6` is its alias), Terra
/// (balanced) and Luna (fastest/cheapest); `gpt-6-astra`, `gpt-6-sol` and
/// `gpt-6-luna` are the GPT-6 line Codex ships (a staged rollout, so they
/// are listed but not the login default).
pub const OPENAI_OAUTH_PROVIDER_MODELS: &[&str] = &[
    OPENAI_OAUTH_PROVIDER_MODEL,
    "gpt-5.6-sol",
    // Virtual alias: sent as gpt-5.6-sol + `reasoning.mode: "pro"`.
    "gpt-5.6-sol-pro",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];

/// Default config directory name (sits under `$HOME`).
pub const DEFAULT_CONFIG_DIR_NAME: &str = ".rebon";

/// Top-level config key for inline/screen UI mode.
pub const UI_MODE_CONFIG_KEY: &str = "uiMode";

/// Top-level user settings key for terminal formula rendering.
pub const MATH_RENDERING_CONFIG_KEY: &str = "mathRendering";

/// Top-level user settings key for the main loop model override.
pub const USER_MODEL_CONFIG_KEY: &str = "model";
/// Top-level explicit response-language preference shared by every frontend.
pub const LANGUAGE_CONFIG_KEY: &str = "language";
/// Top-level user setting for the Normal capability base system prompt.
pub const NORMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY: &str = "normalSystemPromptOverride";
/// Top-level user setting for the Minimal capability base system prompt.
pub const MINIMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY: &str = "minimalSystemPromptOverride";
/// Top-level user setting for the Chat capability base system prompt.
pub const CHAT_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY: &str = "chatSystemPromptOverride";
/// Top-level user setting: which capability modes the new-chat switch offers.
pub const ENABLED_CAPABILITY_MODES_CONFIG_KEY: &str = "enabledCapabilityModes";
/// The app nests its own appearance settings here; `language` inside it is
/// read when the shared top-level key is absent.
const APP_APPEARANCE_CONFIG_KEY: &str = "appAppearance";
/// Top-level `config.json` key for per-million-token USD pricing overrides.
pub const MODEL_PRICING_CONFIG_KEY: &str = "modelPricing";
/// Top-level `config.json` key recording which one-shot migrations have run.
pub const CONFIG_SCHEMA_VERSION_KEY: &str = "configSchemaVersion";
/// The schema every migration in [`migrate_config_to_current_schema`] brings a
/// file up to. Bump it when a migration is added; a file stamped with this
/// number is never inspected for older shapes again.
pub const CONFIG_SCHEMA_VERSION: u64 = 1;

/// USD rates per one million tokens. Every category is independent because
/// providers price cache reads and writes differently from normal input.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTokenPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

pub type ModelPricingCatalog = BTreeMap<String, BTreeMap<String, ModelTokenPricing>>;

/// Built-in offline pricing.
///
/// Empty on purpose since 2026-09-09. This used to be a hand-written table
/// of three models, which meant `/cost` reported "no rates configured" for
/// every model anyone actually runs — a session on `gpt-6-astra` priced
/// nothing at all. Prices now come from the model table
/// ([`rebon_api::model_table`]), which carries all of them and is
/// regenerated from models.dev, so a second copy here could only go stale
/// and shadow the fresh one.
///
/// It stays as a function because [`load_model_pricing`] falls back to it
/// when `config.json` is unreadable: the answer then is "no overrides",
/// not "no prices".
pub fn default_model_pricing() -> ModelPricingCatalog {
    ModelPricingCatalog::new()
}

/// Load the built-in catalog and overlay exact provider/model entries from
/// `config.json`'s `modelPricing` object. Provider lookup is case-insensitive;
/// model ids remain exact so a nearby model is never used as an estimate.
pub fn load_model_pricing(config_dir: &Path) -> anyhow::Result<ModelPricingCatalog> {
    let mut catalog = default_model_pricing();
    let path = config_json_path(config_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(catalog),
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to read rebon config.json at {path:?}: {err}"
            ))
        }
    };
    let root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|err| anyhow::anyhow!("failed to parse rebon config.json at {path:?}: {err}"))?;
    let Some(overrides) = root.get(MODEL_PRICING_CONFIG_KEY) else {
        return Ok(catalog);
    };
    let overrides: ModelPricingCatalog =
        serde_json::from_value(overrides.clone()).map_err(|err| {
            anyhow::anyhow!("`{MODEL_PRICING_CONFIG_KEY}` in config.json is invalid: {err}")
        })?;
    for (provider, models) in overrides {
        catalog
            .entry(provider.to_ascii_lowercase())
            .or_default()
            .extend(models);
    }
    Ok(catalog)
}

/// A user's own rate for this exact provider and model, if they set one.
///
/// Exact match on purpose: fuzzy-matching an unknown model onto someone's
/// override would invent a price they never wrote.
pub fn model_pricing_for<'a>(
    catalog: &'a ModelPricingCatalog,
    provider: &str,
    model: &str,
) -> Option<&'a ModelTokenPricing> {
    catalog.get(&provider.to_ascii_lowercase())?.get(model)
}

/// What one million tokens cost, in USD.
///
/// The user's `modelPricing` override wins — it is the only way to price a
/// reseller, a proxy, or a negotiated rate — and the model table answers
/// for everything else. The table's lookup is the forgiving one
/// (case-insensitive, a dated snapshot suffix resolves to its base row), so
/// `gpt-6-astra-20260903` is priced like `gpt-6-astra` while an override
/// still has to name the id exactly.
///
/// `None` means nobody knows: a self-hosted model with no override.
pub fn resolve_model_pricing(
    catalog: &ModelPricingCatalog,
    provider: &str,
    model: &str,
) -> Option<ModelTokenPricing> {
    if let Some(override_rate) = model_pricing_for(catalog, provider, model) {
        return Some(*override_rate);
    }
    ensure_model_table_installed();
    let cost = rebon_api::model_table::model(None, model)?.cost?;
    // A model priced for input and output but not for cache reads is
    // charged the input rate for them, which is what a provider without a
    // cache discount does.
    let input = cost.input?;
    Some(ModelTokenPricing {
        input,
        output: cost.output?,
        cache_read: cost.cache_read.unwrap_or(input),
        cache_write: cost.cache_write.unwrap_or(input),
    })
}

#[cfg(test)]
mod model_pricing_tests {
    use super::*;

    /// The models people actually run are priced, and they are priced by
    /// the model table rather than by a copy kept here. Before this, `/cost`
    /// answered "no rates configured" for every one of them.
    #[test]
    fn the_model_table_prices_the_models_a_session_runs_on() {
        let catalog = default_model_pricing();
        assert!(catalog.is_empty(), "prices come from the model table now");
        for model in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-luna",
            "claude-opus-5",
            "deepseek-v4-pro",
        ] {
            let price = resolve_model_pricing(&catalog, "openai", model)
                .unwrap_or_else(|| panic!("{model} is unpriced"));
            assert!(price.input > 0.0 && price.output > 0.0, "{model}");
            assert!(
                price.cache_read <= price.input,
                "{model}: cache is a discount"
            );
        }
        // A dated snapshot is the same model.
        assert_eq!(
            resolve_model_pricing(&catalog, "openai", "gpt-6-astra-20260903"),
            resolve_model_pricing(&catalog, "openai", "gpt-6-astra")
        );
        // Nobody knows what a self-hosted model costs.
        assert!(resolve_model_pricing(&catalog, "local", "some-self-hosted-thing").is_none());
    }

    /// An override is exact, and it beats the table.
    #[test]
    fn an_override_wins_over_the_table() {
        let mut catalog = ModelPricingCatalog::new();
        catalog.entry("openai".into()).or_default().insert(
            "gpt-6-astra".into(),
            ModelTokenPricing {
                input: 1.0,
                output: 2.0,
                cache_read: 3.0,
                cache_write: 4.0,
            },
        );
        let priced = resolve_model_pricing(&catalog, "openai", "gpt-6-astra").unwrap();
        assert_eq!(priced.input, 1.0);
        // The suffix form is not the id the override named, so it falls
        // through to the table rather than borrowing the override.
        let dated = resolve_model_pricing(&catalog, "openai", "gpt-6-astra-20260903").unwrap();
        assert_ne!(dated.input, 1.0);
    }

    #[test]
    fn config_overrides_defaults_and_adds_models() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"modelPricing":{"anthropic":{"claude-sonnet-4-5":{"input":9.0,"output":8.0,"cacheRead":7.0,"cacheWrite":6.0}},"local":{"exact-model":{"input":1.0,"output":2.0,"cacheRead":3.0,"cacheWrite":4.0}}}}"#).unwrap();
        let catalog = load_model_pricing(dir.path()).unwrap();
        assert_eq!(
            model_pricing_for(&catalog, "anthropic", "claude-sonnet-4-5")
                .unwrap()
                .input,
            9.0
        );
        assert_eq!(
            model_pricing_for(&catalog, "LOCAL", "exact-model")
                .unwrap()
                .cache_write,
            4.0
        );
    }

    #[test]
    fn malformed_pricing_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"modelPricing":{"p":{"m":{"input":1}}}}"#,
        )
        .unwrap();
        assert!(load_model_pricing(dir.path()).is_err());
    }
}

/// Top-level user settings key for effort/thinking level.
pub const EFFORT_LEVEL_CONFIG_KEY: &str = "effortLevel";

/// Top-level user settings key for permission settings.
pub const PERMISSIONS_CONFIG_KEY: &str = "permissions";

/// Permission settings key for the default permission mode.
pub const PERMISSIONS_DEFAULT_MODE_CONFIG_KEY: &str = "defaultMode";

/// Back-compat spelling accepted for the default permission mode.
pub const PERMISSIONS_DEFAULT_MODE_SNAKE_CONFIG_KEY: &str = "default_mode";

/// Back-compat top-level key accepted for the default permission mode.
pub const DEFAULT_PERMISSION_MODE_CONFIG_KEY: &str = "defaultPermissionMode";

/// Top-level `config.json` key for generated image output base.
///
/// The engine still appends `/<session_id>` later so concurrent
/// sessions do not collide.
pub const GENERATED_IMAGES_DIR_CONFIG_KEY: &str = "generatedImagesDir";

/// Back-compat / CLI-friendly spelling accepted alongside
/// [`GENERATED_IMAGES_DIR_CONFIG_KEY`].
pub const GENERATED_IMAGES_DIR_SNAKE_CONFIG_KEY: &str = "generated_images_dir";

/// Top-level config key for OpenAI fast service tier.
pub const SERVICE_TIER_CONFIG_KEY: &str = "serviceTier";
/// Top-level feature map key used by Codex-style fast mode config.
pub const FEATURES_CONFIG_KEY: &str = "features";
/// Feature flag key for fast mode inside [`FEATURES_CONFIG_KEY`].
pub const FAST_MODE_FEATURE_KEY: &str = "fastMode";
/// Top-level config key for update preferences and dismissals.
pub const UPDATES_CONFIG_KEY: &str = "updates";
/// Top-level config key for Agent View UI preferences.
pub const AGENT_VIEW_CONFIG_KEY: &str = "agentView";
const AGENT_VIEW_ACCEPTED_BACKGROUND_PERMISSION_MODES_KEY: &str =
    "acceptedBackgroundPermissionModes";
/// Top-level config key for coordinator-specific options.
pub const COORDINATOR_CONFIG_KEY: &str = "coordinator";
/// Top-level config key for the persisted skill denylist.
pub const DISABLED_SKILLS_CONFIG_KEY: &str = "disabledSkills";
/// Coordinator option key for implementation-worker worktree isolation.
pub const COORDINATOR_USE_WORKTREE_CONFIG_KEY: &str = "useWorktree";
/// Back-compat / CLI-friendly spelling accepted for coordinator worktree isolation.
pub const COORDINATOR_USE_WORKTREE_SNAKE_CONFIG_KEY: &str = "use_worktree";

/// TUI formula-rendering preference stored in `settings.json` as
/// `off`, `unicode`, or `graphics-auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MathRenderingMode {
    #[default]
    Off,
    Unicode,
    GraphicsAuto,
}

impl MathRenderingMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Unicode => "unicode",
            Self::GraphicsAuto => "graphics-auto",
        }
    }
}

impl std::fmt::Display for MathRenderingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MathRenderingMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "unicode" => Ok(Self::Unicode),
            "graphics-auto" => Ok(Self::GraphicsAuto),
            other => Err(format!(
                "invalid mathRendering `{other}`; expected `off`, `unicode`, or `graphics-auto`"
            )),
        }
    }
}

/// Which shape the terminal draws itself in, stored in `settings.json` as
/// `screen` or `inline`.
///
/// This is a persisted user setting, which is why it lives beside the other
/// ones rather than in the binary: the setup wizard offers it as a step, and
/// the wizard is a plugin that cannot depend on the binary. The binary's
/// `--ui-mode` flag parses through [`std::str::FromStr`] below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiMode {
    #[default]
    Screen,
    Inline,
}

impl UiMode {
    /// Every mode, in the order `--ui-mode`'s help lists them. The flag's
    /// value parser is built from this so the accepted spellings and the
    /// documented ones cannot drift apart.
    pub const ALL: [Self; 2] = [Self::Screen, Self::Inline];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Inline => "inline",
        }
    }
}

impl std::fmt::Display for UiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for UiMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "screen" => Ok(Self::Screen),
            "inline" => Ok(Self::Inline),
            other => Err(format!(
                "invalid uiMode `{other}`; expected `screen` or `inline`"
            )),
        }
    }
}

/// Wire format the active provider expects.
///
/// Matches the `format` field of a stored custom provider. Unknown
/// formats are rejected at parse time because this value drives client
/// dispatch, and falling back to "assume openai" would hide config-drift
/// bugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFormat {
    /// OpenAI chat/completions format. Goes through
    /// [`rebon_api::openai_compatible_client`].
    Openai,
    /// OpenAI Responses API format (for ChatGPT Codex backend and
    /// compatible proxies). Goes through the Responses API provider
    /// in [`rebon_api`].
    OpenaiResponses,
    /// Anthropic messages format. Goes through
    /// [`rebon_api::anthropic_client`].
    Anthropic,
}

/// Registry lookup key for a resolved provider.
///
/// Built-in provider formats remain a closed enum, while plugin-backed
/// providers carry an opaque external id that is resolved by the runtime
/// registry after plugin manifests have been materialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSelection {
    BuiltIn(ProviderFormat),
    External(String),
}

impl ProviderSelection {
    pub fn id(&self) -> &str {
        match self {
            Self::BuiltIn(format) => format.as_str(),
            Self::External(id) => id.as_str(),
        }
    }

    pub fn builtin_format(&self) -> Option<ProviderFormat> {
        match self {
            Self::BuiltIn(format) => Some(*format),
            Self::External(_) => None,
        }
    }

    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }
}

impl ProviderFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::OpenaiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn from_str(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "openai" => Ok(Self::Openai),
            "openai-responses" => Ok(Self::OpenaiResponses),
            "anthropic" => Ok(Self::Anthropic),
            other => anyhow::bail!(
                "unknown built-in provider format `{other}` in rebon config.json \
                 (supported: openai, openai-responses, anthropic)"
            ),
        }
    }
}

/// Whether the fast service tier can affect this route: exactly the
/// first-party OpenAI routes ([`is_first_party_openai_route`]).
pub fn openai_service_tier_available(
    format: ProviderFormat,
    base_url: &str,
    has_oauth: bool,
    is_external_provider: bool,
) -> bool {
    is_first_party_openai_route(format, base_url, has_oauth, is_external_provider)
}

/// Whether a built-in OpenAI-format route is OpenAI's own: `api.openai.com`,
/// or the ChatGPT Codex backend reached with OpenAI OAuth. The features that
/// talk to OpenAI beyond the model wire — the fast service tier, the Images
/// API behind `ImageGen` — are offered on exactly these routes; a gateway or
/// a compatible vendor that merely speaks the format gets neither.
pub fn is_first_party_openai_route(
    format: ProviderFormat,
    base_url: &str,
    has_oauth: bool,
    is_external_provider: bool,
) -> bool {
    if is_external_provider
        || !matches!(
            format,
            ProviderFormat::Openai | ProviderFormat::OpenaiResponses
        )
    {
        return false;
    }

    let codex_oauth = has_oauth && rebon_api::is_chatgpt_codex_backend(base_url);
    codex_oauth || is_openai_api_endpoint(format, base_url)
}

pub fn installed_external_provider_ids(cwd: Option<&Path>) -> BTreeSet<String> {
    installed_external_provider_ids_in(&config_home_dir(), cwd)
}

/// The external model providers the installed packages register here.
///
/// Reads through `rebon_plugin_package::discover`, which is the one place
/// that knows what "installed and in effect" means: the record schema, the
/// manifest schema, and the project-shadows-user precedence. This module used
/// to answer the same question with a record type of its own, a manifest read
/// of its own and a hand-written capability-id list that had drifted from the
/// real one — it was missing `acpAgents` and `kernelPlugins`, so the two sides
/// disagreed about which package a project record shadowed.
///
/// The three built-in provider ids stay filtered out here rather than there:
/// which providers rebon ships is a config fact, and the package layer has no
/// business knowing them.
pub fn installed_external_provider_ids_in(
    config_dir: &Path,
    cwd: Option<&Path>,
) -> BTreeSet<String> {
    // With no project in play, the discovery still needs a directory to look
    // in; an untrusted one contributes nothing, which is the same answer as
    // "no project" and is what the `false` below says.
    let (cwd, trusted) = match cwd {
        Some(cwd) => (cwd.to_path_buf(), is_directory_trusted_in(config_dir, cwd)),
        None => (config_dir.to_path_buf(), false),
    };
    let store = rebon_plugin_package::PluginStore::new(config_dir.to_path_buf(), cwd.clone());
    let Ok(found) = rebon_plugin_package::discover(&store, &[], &cwd, trusted) else {
        return BTreeSet::new();
    };
    found
        .model_provider_ids()
        .into_iter()
        .filter(|id| !matches!(id.as_str(), "openai" | "openai-responses" | "anthropic"))
        .collect()
}

pub fn is_openai_api_endpoint(format: ProviderFormat, base_url: &str) -> bool {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return matches!(format, ProviderFormat::Openai);
    }
    let lower = trimmed.trim_end_matches('/').to_ascii_lowercase();
    lower == "https://api.openai.com" || lower.starts_with("https://api.openai.com/")
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdatePreferences {
    pub disabled: bool,
    pub auto_install: bool,
    pub channel: Option<String>,
    pub skipped_version: Option<String>,
    pub dismissed_version: Option<String>,
    pub dismissed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentViewPreferences {
    pub grouping: String,
    pub disabled: bool,
}

impl Default for AgentViewPreferences {
    fn default() -> Self {
        Self {
            grouping: "state".to_string(),
            disabled: false,
        }
    }
}

impl UpdatePreferences {
    pub fn channel_name(&self) -> Option<&str> {
        self.channel
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// OAuth metadata attached to a provider when its api key was
/// resolved from the `$OPENAI_OAUTH_TOKEN` sentinel.
///
/// These fields let the caller decide whether a refresh is needed and,
/// if so, run the refresh POST against
/// `https://auth.openai.com/oauth/token`.
#[derive(Debug, Clone)]
pub struct OAuthMeta {
    /// Expiry in milliseconds since the Unix epoch. `None` when the
    /// credentials file omitted the field — treated as "already
    /// expired" by [`is_token_expired`].
    pub expires_at_ms: Option<u64>,
    /// Refresh token. `None` when the credentials file omitted it —
    /// refresh is impossible and the caller must surface a login
    /// request.
    pub refresh_token: Option<String>,
}

/// Fully-resolved provider config ready to be fed into the right
/// `rebon_api` client constructor.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    /// Provider name as stored in `config.json` (e.g. `"openai"`,
    /// `"rightcodes"`).
    pub name: String,
    /// API base URL (no trailing `/responses` suffix — the
    /// Responses API provider adds it itself when needed).
    pub base_url: String,
    /// Resolved API key. For OAuth providers this is the live
    /// `accessToken` from `.credentials.json`; for API-key
    /// providers it is the literal value from `config.json`.
    pub api_key: String,
    /// Default model name to use when the caller does not override.
    pub model: String,
    /// Resolved provider profile-to-model map from `customProviders[].modelProfiles`.
    pub model_profiles: ModelProfileMap,
    /// Per-model context window overrides from `customProviders[].models`.
    pub model_context_windows: BTreeMap<String, u32>,
    /// Per-model maximum output-token reservations from `customProviders[].models`.
    pub model_output_token_limits: BTreeMap<String, u32>,
    /// Wire format — drives built-in rebon-api client behavior. For external
    /// plugin providers this is a conservative compatibility hint; callers
    /// must use [`provider_selection`] for the actual registry lookup.
    pub format: ProviderFormat,
    /// Who is behind the endpoint: the entry's `vendor` pin when present,
    /// otherwise recognised from `baseUrl`'s host. Drives the wire details
    /// that differ per company — thinking switches, reasoning replay,
    /// prefix-cache protection, model discovery.
    pub vendor: rebon_api::ProviderVendor,
    /// Registry selection resolved from the provider name against the
    /// registry-aware external provider id set, or from [`format`] for built-ins.
    pub provider_selection: ProviderSelection,
    /// OAuth metadata. `Some` when the provider's api_key was
    /// resolved from the `$OPENAI_OAUTH_TOKEN` sentinel; `None`
    /// for literal API-key providers.
    pub oauth: Option<OAuthMeta>,
    /// When `true`, the OpenAI Responses provider uses WebSocket
    /// transport (supports `previous_response_id`).
    pub use_websocket: bool,
    /// Extra OpenAI-compatible headers resolved from provider/model options.
    pub extra_headers: Vec<(String, String)>,
    /// Provider-level OpenAI-compatible request body options.
    pub request_options: OpenAiRequestOptions,
    /// Per-model OpenAI-compatible request body options.
    pub model_request_options: BTreeMap<String, OpenAiRequestOptions>,
    /// Whether volatile runtime context may stay request-scoped instead of
    /// being materialized into durable history.
    pub request_scoped_transient_context: bool,
    /// Reasoning mode from `customProviders[].reasoningMode`. Only
    /// `"pro"` (gpt-5.6+ pro mode) is recognized downstream; `None`
    /// means standard mode.
    pub reasoning_mode: Option<String>,
}

/// Syntactic parse failure for the primary `config.json`.
#[derive(Debug, Clone)]
pub struct ConfigParseFailure {
    /// Path of the invalid file.
    pub file_path: PathBuf,
    /// Human-readable serde parse error.
    pub message: String,
    /// Safe default config payload to write on reset.
    pub default_config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// On-disk JSON shapes — kept private so callers go through
// [`resolve_from_dir`] instead of threading raw serde types.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
/// The parts of `config.json` that are read without the provider store
/// overlaid — which, since providers moved to `providers/`, is only the
/// active selection. Anything wanting provider *definitions* must go through
/// `read_config_roundtrip`.
struct RebonConfig {
    #[serde(default)]
    active_custom_provider: Option<String>,
}

/// Round-trip-safe variant of [`RebonConfig`]. Adds `Serialize` +
/// `#[serde(flatten)]` so reading config, mutating it, and writing
/// it back preserves every key this crate does not model.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RebonConfigRoundTrip {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_custom_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    custom_providers: Vec<CustomProvider>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CustomProvider {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    /// Which company is behind the endpoint, when the host does not say
    /// (a gateway in front of DeepSeek, say). Absent means "detect from
    /// `baseUrl`". See `rebon_api::ProviderVendor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vendor: Option<String>,
    base_url: String,
    api_key: String,
    /// Current active model for this provider. Older configs only
    /// stored this field; newer configs also populate `models` below.
    #[serde(default)]
    model: String,
    /// Known models for this provider (optional). When populated,
    /// `/provider list` and the onboarding provider panel show the
    /// full set. Old configs without this field still read correctly
    /// (deserializes as empty).
    #[serde(default, skip_serializing_if = "CustomProviderModels::is_empty")]
    models: CustomProviderModels,
    /// Per-profile model overrides for this provider. Keys are profile names
    /// such as `general`, `small`, `explore`; values are concrete model ids.
    #[serde(
        default,
        rename = "modelProfiles",
        skip_serializing_if = "ModelProfileMap::is_empty"
    )]
    model_profiles: ModelProfileMap,
    /// Provider/model options used by OpenAI-compatible request mappings.
    #[serde(default, skip_serializing_if = "ProviderOptions::is_empty")]
    options: ProviderOptions,
    /// Whether volatile runtime context may stay request-scoped instead of
    /// being materialized into durable history.
    #[serde(
        default,
        rename = "requestScopedTransientContext",
        skip_serializing_if = "Option::is_none"
    )]
    request_scoped_transient_context: Option<bool>,
    /// When `true`, the OpenAI Responses provider uses WebSocket
    /// transport instead of HTTP POST. WebSocket supports
    /// `previous_response_id` for input-delta optimisation.
    #[serde(default, skip_serializing_if = "is_false")]
    use_websocket: bool,
    /// Optional OpenAI-compatible thinking extension used by DeepSeek.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_enabled: Option<bool>,
    /// Optional default effort: max/xhigh/high/medium/low.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_effort: Option<String>,
    /// Optional reasoning mode for the OpenAI Responses API. Only
    /// `"pro"` is defined (gpt-5.6+ pro mode); omitted = standard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_mode: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderOptions {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    headers: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    body: serde_json::Map<String, serde_json::Value>,
    #[serde(
        default,
        rename = "extraBody",
        skip_serializing_if = "serde_json::Map::is_empty"
    )]
    extra_body: serde_json::Map<String, serde_json::Value>,
    #[serde(
        default,
        rename = "omitBodyFields",
        skip_serializing_if = "Vec::is_empty"
    )]
    omit_body_fields: Vec<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl ProviderOptions {
    fn is_empty(&self) -> bool {
        self.headers.is_empty()
            && self.body.is_empty()
            && self.extra_body.is_empty()
            && self.omit_body_fields.is_empty()
            && self.extra.is_empty()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePreferencesSerde {
    #[serde(default, skip_serializing_if = "is_false")]
    disabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    auto_install: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dismissed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dismissed_at_ms: Option<u64>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentViewPreferencesSerde {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grouping: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "disableAgentView"
    )]
    disable_agent_view: Option<bool>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl From<AgentViewPreferencesSerde> for AgentViewPreferences {
    fn from(raw: AgentViewPreferencesSerde) -> Self {
        let grouping = match raw.grouping.as_deref().map(str::trim) {
            Some("directory") => "directory",
            _ => "state",
        };
        Self {
            grouping: grouping.to_string(),
            disabled: raw.disabled.or(raw.disable_agent_view).unwrap_or(false),
        }
    }
}

impl From<UpdatePreferencesSerde> for UpdatePreferences {
    fn from(raw: UpdatePreferencesSerde) -> Self {
        Self {
            disabled: raw.disabled,
            auto_install: raw.auto_install,
            channel: raw.channel.and_then(non_empty_string),
            skipped_version: raw.skipped_version.and_then(non_empty_string),
            dismissed_version: raw.dismissed_version.and_then(non_empty_string),
            dismissed_at_ms: raw.dismissed_at_ms,
        }
    }
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed.len() == value.len() {
        Some(value)
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum CustomProviderModels {
    List(Vec<CustomProviderModelEntry>),
    Map(BTreeMap<String, CustomProviderModelDetails>),
}

impl Default for CustomProviderModels {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

impl CustomProviderModels {
    fn is_empty(&self) -> bool {
        match self {
            Self::List(entries) => entries.is_empty(),
            Self::Map(entries) => entries.is_empty(),
        }
    }

    fn ids(&self) -> Vec<String> {
        match self {
            Self::List(entries) => entries.iter().map(CustomProviderModelEntry::id).collect(),
            Self::Map(entries) => entries.keys().cloned().collect(),
        }
    }

    fn contains_id(&self, id: &str) -> bool {
        match self {
            Self::List(entries) => entries.iter().any(|entry| entry.id() == id),
            Self::Map(entries) => entries.contains_key(id),
        }
    }

    /// Append a bare id, with no limits attached — the shape a hand-written
    /// entry has.
    fn push_id(&mut self, id: String) {
        match self {
            Self::List(entries) => entries.push(CustomProviderModelEntry::String(id)),
            Self::Map(entries) => {
                entries
                    .entry(id)
                    .or_insert_with(CustomProviderModelDetails::default);
            }
        }
    }

    fn ensure_id(&mut self, id: &str) {
        if !id.is_empty() && !self.contains_id(id) {
            self.push_id(id.to_string());
        }
    }

    /// Append a model with the limits a listing or catalogue stated. A
    /// model with no limits is a bare id, as a hand-written one would be.
    fn push_with_limits(
        &mut self,
        id: &str,
        context_window: Option<u32>,
        max_output_tokens: Option<u32>,
    ) {
        let details = CustomProviderModelDetails {
            context_window: context_window.map(|n| serde_json::json!(n)),
            max_output_tokens: max_output_tokens.map(|n| serde_json::json!(n)),
            ..CustomProviderModelDetails::default()
        };
        match self {
            Self::List(entries) => {
                if context_window.is_none() && max_output_tokens.is_none() {
                    entries.push(CustomProviderModelEntry::String(id.to_string()));
                } else {
                    entries.push(CustomProviderModelEntry::Object(
                        CustomProviderModelObject {
                            id: id.to_string(),
                            details,
                        },
                    ));
                }
            }
            Self::Map(entries) => {
                entries.insert(id.to_string(), details);
            }
        }
    }

    /// Fill in limits the entry does not have for an existing model. What
    /// the user wrote stays. Returns whether anything changed.
    fn fill_limits(
        &mut self,
        id: &str,
        context_window: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> bool {
        if context_window.is_none() && max_output_tokens.is_none() {
            return false;
        }
        let fill = |details: &mut CustomProviderModelDetails| -> bool {
            let mut changed = false;
            if details.context_window_value().is_none() {
                if let Some(window) = context_window {
                    details.context_window = Some(serde_json::json!(window));
                    changed = true;
                }
            }
            if details.output_token_limit_value().is_none() {
                if let Some(limit) = max_output_tokens {
                    details.max_output_tokens = Some(serde_json::json!(limit));
                    changed = true;
                }
            }
            changed
        };
        match self {
            Self::List(entries) => {
                for entry in entries.iter_mut() {
                    if entry.id() != id {
                        continue;
                    }
                    return match entry {
                        CustomProviderModelEntry::Object(obj) => fill(&mut obj.details),
                        CustomProviderModelEntry::String(_) => {
                            let mut details = CustomProviderModelDetails::default();
                            let changed = fill(&mut details);
                            if changed {
                                *entry =
                                    CustomProviderModelEntry::Object(CustomProviderModelObject {
                                        id: id.to_string(),
                                        details,
                                    });
                            }
                            changed
                        }
                    };
                }
                false
            }
            Self::Map(entries) => entries.get_mut(id).is_some_and(fill),
        }
    }

    fn model_options(&self) -> BTreeMap<String, ProviderOptions> {
        let mut out = BTreeMap::new();
        match self {
            Self::List(entries) => {
                for entry in entries {
                    if let Some((id, options)) = entry.options() {
                        out.insert(id, options);
                    }
                }
            }
            Self::Map(entries) => {
                for (id, details) in entries {
                    out.insert(id.clone(), details.options.clone());
                }
            }
        }
        out
    }

    fn context_windows(&self) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        match self {
            Self::List(entries) => {
                for entry in entries {
                    if let Some((id, window)) = entry.context_window() {
                        out.insert(id, window);
                    }
                }
            }
            Self::Map(entries) => {
                for (id, details) in entries {
                    if let Some(window) = details.context_window_value() {
                        out.insert(id.clone(), window);
                    }
                }
            }
        }
        out
    }

    fn output_token_limits(&self) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        match self {
            Self::List(entries) => {
                for entry in entries {
                    if let Some((id, limit)) = entry.output_token_limit() {
                        out.insert(id, limit);
                    }
                }
            }
            Self::Map(entries) => {
                for (id, details) in entries {
                    if let Some(limit) = details.output_token_limit_value() {
                        out.insert(id.clone(), limit);
                    }
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum CustomProviderModelEntry {
    String(String),
    Object(CustomProviderModelObject),
}

impl CustomProviderModelEntry {
    fn id(&self) -> String {
        match self {
            Self::String(id) => id.clone(),
            Self::Object(obj) => obj.id.clone(),
        }
    }

    fn options(&self) -> Option<(String, ProviderOptions)> {
        match self {
            Self::String(_) => None,
            Self::Object(obj) => Some((obj.id.clone(), obj.details.options.clone())),
        }
    }

    fn context_window(&self) -> Option<(String, u32)> {
        match self {
            Self::String(_) => None,
            Self::Object(obj) => obj
                .details
                .context_window_value()
                .map(|window| (obj.id.clone(), window)),
        }
    }

    fn output_token_limit(&self) -> Option<(String, u32)> {
        match self {
            Self::String(_) => None,
            Self::Object(obj) => obj
                .details
                .output_token_limit_value()
                .map(|limit| (obj.id.clone(), limit)),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CustomProviderModelObject {
    id: String,
    #[serde(flatten)]
    details: CustomProviderModelDetails,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CustomProviderModelDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "ProviderOptions::is_empty")]
    options: ProviderOptions,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl CustomProviderModelDetails {
    fn context_window_value(&self) -> Option<u32> {
        parse_u32_json(self.context_window.as_ref())
            .or_else(|| context_window_from_limit(self.limit.as_ref()))
    }

    fn output_token_limit_value(&self) -> Option<u32> {
        parse_u32_json(self.max_output_tokens.as_ref())
            .or_else(|| output_token_limit_from_limit(self.limit.as_ref()))
    }
}

fn context_window_from_limit(limit: Option<&serde_json::Value>) -> Option<u32> {
    let limit = limit?;
    parse_u32_json(limit.get("contextWindow"))
        .or_else(|| parse_u32_json(limit.get("context")))
        .or_else(|| parse_u32_json(limit.get("tokens")))
}

fn output_token_limit_from_limit(limit: Option<&serde_json::Value>) -> Option<u32> {
    let limit = limit?;
    parse_u32_json(limit.get("maxOutputTokens")).or_else(|| parse_u32_json(limit.get("output")))
}

fn parse_u32_json(value: Option<&serde_json::Value>) -> Option<u32> {
    let value = value?;
    if let Some(n) = value.as_u64() {
        return u32::try_from(n).ok();
    }
    value
        .as_str()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
}

fn is_false(v: &bool) -> bool {
    !v
}

use rebon_types::env::env_truthy;

/// On-disk shape of `.credentials.json`.
///
/// Public because the OAuth refresh helper needs to round-
/// trip the file without losing unknown keys (the file also stores
/// `claudeAiOauth` and other things in here). The `extra` field
/// captures anything we don't know about so serialization
/// preserves it byte-for-byte.
///
/// The explicit `rename = "openaiOAuth"` is load-bearing: serde's
/// `rename_all = "camelCase"` would turn `openai_oauth` into
/// `openaiOauth` (lowercase `o` after the word break), which does
/// NOT match the JSON key `openaiOAuth` (the key uses
/// `OAuth` as an acronym with uppercase `OA`). We skip the struct-
/// level `rename_all` and name the one field explicitly.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Credentials {
    /// OpenAI OAuth token bundle. `None` when the user has not
    /// logged in via `/login`.
    #[serde(
        rename = "openaiOAuth",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub openai_oauth: Option<OpenAIOAuthTokens>,
    /// Every other top-level key we don't explicitly model. Kept
    /// so rewriting the file (after a token refresh) does not lose
    /// unrelated sibling entries like `claudeAiOauth`.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// OpenAI OAuth token bundle stored under
/// `credentials.json::openaiOAuth`.
///
/// Stored OAuth token bundle shape: access token, optional refresh
/// token, and optional expiry. The account id lives in config.json,
/// not .credentials.json.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAIOAuthTokens {
    /// Current valid OAuth access token (Bearer).
    pub access_token: String,
    /// Refresh token used to mint new access tokens. Optional
    /// because credentials may store it as null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Expiry in milliseconds since Unix epoch. Optional because
    /// credentials may store it as null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

pub mod paths;
pub mod profile_store;
pub mod provider_store;

/// Turn a typed name into a file-name id for one of the `~/.rebon` stores.
///
/// Lower-cased so ids match the case-insensitive lookups the rest of the crate
/// does, and restricted to characters that are safe as a file name on every
/// platform we ship to — a provider called `openai/codex` must not become a
/// nested directory, and neither must a profile called `deep/review`. Shared
/// by both stores so the two cannot disagree about what a given name maps to.
pub(crate) fn store_file_id(name: &str, fallback: &str) -> String {
    let mut id = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            id.push(ch);
        } else if !id.ends_with('-') {
            id.push('-');
        }
    }
    let id = id.trim_matches(['-', '.']).to_string();
    if id.is_empty() {
        fallback.to_string()
    } else {
        id
    }
}

#[cfg(test)]
pub use paths::generated_images_output_base_in_dir;
pub use paths::*;
pub use paths::{agents_json_path, config_json_path, credentials_json_path, home_dir};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve the configured active provider from a caller-supplied
/// config directory (unit tests point this at a tempdir fixture
/// without mutating the process's `REBON_CONFIG_DIR` env var).
///
/// See the module-level docs for fallback semantics.
pub fn resolve_from_dir(config_dir: &Path) -> anyhow::Result<Option<ResolvedProvider>> {
    resolve_from_dir_with(config_dir, None)
}

/// Resolve the active provider from `<config_dir>/config.json`,
/// optionally overriding the stored `activeCustomProvider` with
/// the given name.
///
/// When `provider_override` is `Some(name)`, this function looks
/// up `name` in `customProviders[]` instead of using the stored
/// active value — lets `rebon --provider openrouter` pick a specific
/// entry without touching disk. An override that does not match
/// any entry errors out the same way a stale `activeCustomProvider`
/// does, with a clear hint about re-registering.
pub fn resolve_from_dir_with(
    config_dir: &Path,
    provider_override: Option<&str>,
) -> anyhow::Result<Option<ResolvedProvider>> {
    resolve_from_dir_with_external_provider_ids(
        config_dir,
        provider_override,
        std::iter::empty::<&str>(),
    )
}

/// Registry-aware variant of [`resolve_from_dir_with`].
///
/// External provider ids come from plugin manifests after runtime materialization.
/// When the selected `customProviders[].name` matches one of those ids, provider
/// selection is preserved as an opaque external id while the optional `format`
/// field remains the closed built-in compatibility wire format.
pub fn resolve_from_dir_with_external_provider_ids<I, S>(
    config_dir: &Path,
    provider_override: Option<&str>,
    external_provider_ids: I,
) -> anyhow::Result<Option<ResolvedProvider>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let external_provider_ids = external_provider_ids
        .into_iter()
        .map(|id| id.as_ref().to_string())
        .collect::<BTreeSet<_>>();
    // Provider definitions come from the store when it exists, and from
    // `config.json` otherwise; `activeCustomProvider` always comes from
    // `config.json`, because which provider is selected is session state
    // rather than part of any provider's definition.
    if !config_json_path(config_dir).exists() && !provider_store::is_active(config_dir) {
        return Ok(None);
    }
    let config = read_config_roundtrip(config_dir)?;

    // Override wins over stored activeCustomProvider.
    let active_name: Option<String> = provider_override
        .map(str::to_string)
        .or_else(|| config.active_custom_provider.clone());
    let Some(active_name) = active_name else {
        return Ok(None);
    };

    let provider = config
        .custom_providers
        .iter()
        .find(|p| p.name == active_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "rebon config.json has no customProviders[] entry named `{active_name}` \
                 — available: [{}]; run `rebon` to re-register or pass a valid \
                 --provider value",
                config
                    .custom_providers
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let format_raw = provider.format.as_deref().unwrap_or("openai"); // an absent `format` means `openai`
    let selected_external = external_provider_ids.contains(&provider.name);
    let format = ProviderFormat::from_str(format_raw)?;
    let provider_selection = if selected_external {
        ProviderSelection::External(provider.name.clone())
    } else {
        ProviderSelection::BuiltIn(format)
    };

    let (api_key, oauth) = resolve_api_key(provider, config_dir)?;

    let (extra_headers, request_options) = resolve_provider_options(provider);
    let model_request_options = resolve_model_request_options(provider);
    let model_context_windows = resolve_model_context_windows(provider);
    let model_output_token_limits = resolve_model_output_token_limits(provider);

    Ok(Some(ResolvedProvider {
        name: provider.name.clone(),
        base_url: provider.base_url.clone(),
        api_key,
        model: provider.model.clone(),
        model_profiles: provider.model_profiles.clone(),
        model_context_windows,
        model_output_token_limits,
        format,
        vendor: rebon_api::ProviderVendor::resolve(provider.vendor.as_deref(), &provider.base_url),
        provider_selection,
        oauth,
        use_websocket: provider.use_websocket,
        extra_headers,
        request_options,
        model_request_options,
        request_scoped_transient_context: provider.request_scoped_transient_context.unwrap_or(true),
        reasoning_mode: provider.reasoning_mode.clone(),
    }))
}

/// Resolve a configured model profile against the active provider's
/// `modelProfiles` map.
///
/// A declared profile wins; anything undeclared follows `runtime_model` —
/// the model the session is actually running. The provider entry's own
/// `model` is only reached for callers that pass no runtime model at all.
pub fn resolve_model_profile(
    provider: Option<&ResolvedProvider>,
    profile: &str,
    runtime_model: &str,
) -> String {
    match provider {
        Some(provider) => provider.model_profiles.resolve_model(
            profile,
            Some(provider.model.as_str()),
            runtime_model,
        ),
        None => {
            let runtime_model = runtime_model.trim();
            if runtime_model.is_empty() {
                String::new()
            } else {
                runtime_model.to_string()
            }
        }
    }
}

/// Load the persisted skill denylist from `config.json`.
///
/// Missing configuration defaults to an empty set. Skill ids are trimmed,
/// empty entries are ignored, and the returned [`BTreeSet`] provides stable
/// ordering while removing duplicates.
pub fn load_disabled_skills() -> anyhow::Result<BTreeSet<String>> {
    load_disabled_skills_in(&config_home_dir())
}

/// Testable/config-dir-specific variant of [`load_disabled_skills`].
pub fn load_disabled_skills_in(config_dir: &Path) -> anyhow::Result<BTreeSet<String>> {
    let config = read_config_roundtrip(config_dir)?;
    let Some(value) = config.extra.get(DISABLED_SKILLS_CONFIG_KEY) else {
        return Ok(BTreeSet::new());
    };
    let values = value.as_array().ok_or_else(|| {
        anyhow::anyhow!("`{DISABLED_SKILLS_CONFIG_KEY}` in config.json must be an array of strings")
    })?;

    let mut disabled = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let id = value.as_str().ok_or_else(|| {
            anyhow::anyhow!(
                "`{DISABLED_SKILLS_CONFIG_KEY}[{index}]` in config.json must be a string"
            )
        })?;
        let id = id.trim();
        if !id.is_empty() {
            disabled.insert(id.to_string());
        }
    }
    Ok(disabled)
}

/// Persist the skill denylist to `config.json` while preserving unrelated
/// top-level fields. Empty denylist entries are omitted from the file.
pub fn save_disabled_skills<I, S>(disabled_skills: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    save_disabled_skills_in(&config_home_dir(), disabled_skills)
}

/// Testable/config-dir-specific variant of [`save_disabled_skills`].
pub fn save_disabled_skills_in<I, S>(config_dir: &Path, disabled_skills: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let disabled = disabled_skills
        .into_iter()
        .filter_map(|id| {
            let id = id.as_ref().trim().to_string();
            (!id.is_empty()).then_some(id)
        })
        .collect::<BTreeSet<_>>();

    let mut config = read_config_roundtrip(config_dir)?;
    if disabled.is_empty() {
        config.extra.remove(DISABLED_SKILLS_CONFIG_KEY);
    } else {
        config.extra.insert(
            DISABLED_SKILLS_CONFIG_KEY.to_string(),
            serde_json::Value::Array(
                disabled
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    write_config_roundtrip(config_dir, &config)
}

// ---------------------------------------------------------------------------
// ACP agents — third-party agent CLIs Rebon can run a session on.
// ---------------------------------------------------------------------------

/// `config.json` key holding the configured agent CLIs.
pub const ACP_AGENTS_CONFIG_KEY: &str = "acpAgents";
/// `config.json` key holding the agent new sessions start on.
pub const ACTIVE_ACP_AGENT_CONFIG_KEY: &str = "activeAcpAgent";
/// Reserved id for Rebon's own engine. Never an ACP agent.
pub const LOCAL_AGENT_ID: &str = "local";

/// One third-party agent CLI, as configured in `config.json`.
///
/// This is a *process* description, not a provider: an ACP agent brings
/// its own model, its own key, and its own tool loop. What Rebon needs
/// to know is how to start it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentConfig {
    /// Stable id used by `/agent`, session metadata, and logs.
    pub id: String,
    /// Human-readable name. Falls back to [`Self::id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Executable to run.
    pub command: String,
    /// Arguments that put the CLI into ACP mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment for the child. Values support the same
    /// `$VAR` / `${VAR}` indirection as provider headers, so a key can
    /// live in the environment rather than in `config.json`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory for the child. Omitted means "the session's".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether Rebon injects its own `write_file`/`edit_file` MCP
    /// tools into this agent's sessions, pulling its file writes back
    /// through the host's snapshot pipeline so `/rewind` covers them.
    ///
    /// Omitted means yes: the injection is the only thing standing
    /// between an agent that writes disk itself and a rewind that
    /// restores nothing. Set `false` for an agent the tools confuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_fs_tools: Option<bool>,
    /// `_meta` object sent with this agent's `session/new` and
    /// `session/load` — adapter-specific options the ACP spec has no
    /// field for. For `claude-agent-acp` this is where
    /// `{"claudeCode": {"options": {"disallowedTools": [...]}}}` goes,
    /// which disables the agent's own edit tools so the injected
    /// `write_file`/`edit_file` actually get used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_meta: Option<BTreeMap<String, serde_json::Value>>,
}

impl AcpAgentConfig {
    /// What to show the user.
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.id)
    }

    /// Whether Rebon's fs tools are injected into this agent's
    /// sessions. Defaults to `true` — see [`Self::inject_fs_tools`].
    pub fn injects_fs_tools(&self) -> bool {
        self.inject_fs_tools.unwrap_or(true)
    }

    /// Environment with `$VAR` references resolved.
    ///
    /// Kept separate from the stored value so `config.json` can hold
    /// the reference and the resolution happens at spawn time, when
    /// the environment is the one the agent will actually run in.
    pub fn resolved_env(&self) -> BTreeMap<String, String> {
        self.env
            .iter()
            .map(|(key, value)| (key.clone(), resolve_env_value(value)))
            .collect()
    }
}

/// Read the configured agent CLIs from `config.json`.
///
/// A malformed entry fails the whole read rather than being skipped:
/// silently dropping an agent would show the user a `/agent` list that
/// is missing the one they just configured, with no explanation.
pub fn load_acp_agents() -> anyhow::Result<Vec<AcpAgentConfig>> {
    load_acp_agents_in(&config_home_dir())
}

/// Testable/config-dir-specific variant of [`load_acp_agents`].
pub fn load_acp_agents_in(config_dir: &Path) -> anyhow::Result<Vec<AcpAgentConfig>> {
    let config = read_config_roundtrip(config_dir)?;
    let Some(value) = config.extra.get(ACP_AGENTS_CONFIG_KEY) else {
        return Ok(Vec::new());
    };
    let entries: Vec<AcpAgentConfig> = serde_json::from_value(value.clone()).map_err(|err| {
        anyhow::anyhow!(
            "`{ACP_AGENTS_CONFIG_KEY}` in config.json must be an array of \
             {{ id, command, args?, env?, cwd?, displayName? }} entries: {err}"
        )
    })?;

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut agents = Vec::with_capacity(entries.len());
    for (index, entry) in entries.into_iter().enumerate() {
        let id = entry.id.trim().to_string();
        if id.is_empty() {
            anyhow::bail!("`{ACP_AGENTS_CONFIG_KEY}[{index}].id` in config.json must not be empty");
        }
        if id.eq_ignore_ascii_case(LOCAL_AGENT_ID) {
            anyhow::bail!(
                "`{ACP_AGENTS_CONFIG_KEY}[{index}].id` is `{id}`, which is reserved for \
                 rebon's own engine — pick another id"
            );
        }
        if !seen.insert(id.to_ascii_lowercase()) {
            anyhow::bail!(
                "`{ACP_AGENTS_CONFIG_KEY}` in config.json has more than one entry named `{id}`"
            );
        }
        let command = entry.command.trim().to_string();
        if command.is_empty() {
            anyhow::bail!(
                "`{ACP_AGENTS_CONFIG_KEY}[{index}].command` in config.json must not be empty \
                 (agent `{id}`)"
            );
        }
        agents.push(AcpAgentConfig {
            id,
            command,
            ..entry
        });
    }
    Ok(agents)
}

/// Top-level `config.json` key for Remote Control (`rebon rc`) settings.
pub const RC_CONFIG_KEY: &str = "rc";

/// One project `rebon rc serve` advertises, from `rc.projects`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcProjectConfig {
    pub path: String,
    pub label: Option<String>,
}

/// The projects `rebon rc serve` advertises, from `rc.projects`: each entry
/// a directory path, or `{ "path": …, "label": … }`. A relative path is
/// resolved against the config directory.
///
/// An absent key or `rc` object is no projects. A malformed entry fails the
/// whole read: advertising a subset of what the user listed would hide the
/// mistake behind a machine that simply never offers one of their projects.
pub fn load_rc_projects() -> anyhow::Result<Vec<RcProjectConfig>> {
    load_rc_projects_in(&config_home_dir())
}

/// Testable/config-dir-specific variant of [`load_rc_projects`].
pub fn load_rc_projects_in(config_dir: &Path) -> anyhow::Result<Vec<RcProjectConfig>> {
    let config = read_config_roundtrip(config_dir)?;
    let Some(rc) = config.extra.get(RC_CONFIG_KEY) else {
        return Ok(Vec::new());
    };
    let Some(projects) = rc.get("projects") else {
        return Ok(Vec::new());
    };
    let entries = projects.as_array().ok_or_else(|| {
        anyhow::anyhow!("`{RC_CONFIG_KEY}.projects` in config.json must be an array")
    })?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let (path, label) = match entry {
                serde_json::Value::String(path) => (path.as_str(), None),
                serde_json::Value::Object(object) => (
                    object.get("path").and_then(|path| path.as_str()).unwrap_or(""),
                    match object.get("label") {
                        None | Some(serde_json::Value::Null) => None,
                        Some(serde_json::Value::String(label)) => Some(label.clone()),
                        Some(_) => anyhow::bail!(
                            "`{RC_CONFIG_KEY}.projects[{index}].label` in config.json must be a string"
                        ),
                    },
                ),
                _ => anyhow::bail!(
                    "`{RC_CONFIG_KEY}.projects[{index}]` in config.json must be a path or \
                     {{ path, label? }}"
                ),
            };
            let path = path.trim();
            if path.is_empty() {
                anyhow::bail!("`{RC_CONFIG_KEY}.projects[{index}]` in config.json has no path");
            }
            Ok(RcProjectConfig {
                path: resolve_against_cwd(config_dir, path)
                    .to_string_lossy()
                    .into_owned(),
                label,
            })
        })
        .collect()
}

/// The agent new sessions start on, when the user set a default.
///
/// `None` means Rebon's own engine. An id that no longer matches a
/// configured agent is returned as-is — the caller reports the dangling
/// name rather than silently starting on the wrong agent.
pub fn active_acp_agent() -> Option<String> {
    active_acp_agent_in(&config_home_dir())
}

/// Testable/config-dir-specific variant of [`active_acp_agent`].
pub fn active_acp_agent_in(config_dir: &Path) -> Option<String> {
    let config = read_config_roundtrip(config_dir).ok()?;
    let id = config
        .extra
        .get(ACTIVE_ACP_AGENT_CONFIG_KEY)?
        .as_str()?
        .trim();
    if id.is_empty() || id.eq_ignore_ascii_case(LOCAL_AGENT_ID) {
        return None;
    }
    Some(id.to_string())
}

/// Persist which agent new sessions start on.
///
/// `None` (or `"local"`) removes the key, which is the same thing:
/// no configured agent means the local engine.
pub fn save_active_acp_agent(agent_id: Option<&str>) -> anyhow::Result<()> {
    save_active_acp_agent_in(&config_home_dir(), agent_id)
}

/// Testable/config-dir-specific variant of [`save_active_acp_agent`].
pub fn save_active_acp_agent_in(config_dir: &Path, agent_id: Option<&str>) -> anyhow::Result<()> {
    let agent_id = agent_id
        .map(str::trim)
        .filter(|id| !id.is_empty() && !id.eq_ignore_ascii_case(LOCAL_AGENT_ID));

    let mut config = read_config_roundtrip(config_dir)?;
    match agent_id {
        Some(id) => {
            config.extra.insert(
                ACTIVE_ACP_AGENT_CONFIG_KEY.to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }
        None => {
            config.extra.remove(ACTIVE_ACP_AGENT_CONFIG_KEY);
        }
    }
    write_config_roundtrip(config_dir, &config)
}

pub fn load_update_preferences() -> anyhow::Result<UpdatePreferences> {
    load_update_preferences_in(&config_home_dir())
}

pub fn load_update_preferences_in(config_dir: &Path) -> anyhow::Result<UpdatePreferences> {
    let config = read_config_roundtrip(config_dir)?;
    Ok(update_preferences_from_config(&config))
}

pub fn save_update_preferences(prefs: &UpdatePreferences) -> anyhow::Result<()> {
    save_update_preferences_in(&config_home_dir(), prefs)
}

pub fn save_update_preferences_in(
    config_dir: &Path,
    prefs: &UpdatePreferences,
) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    upsert_update_preferences(&mut config, prefs)?;
    write_config_roundtrip(config_dir, &config)
}

pub fn persist_update_dismissal(latest_version: &str, dismissed_at_ms: u64) -> anyhow::Result<()> {
    persist_update_dismissal_in(&config_home_dir(), latest_version, dismissed_at_ms)
}

pub fn persist_update_dismissal_in(
    config_dir: &Path,
    latest_version: &str,
    dismissed_at_ms: u64,
) -> anyhow::Result<()> {
    let mut prefs = load_update_preferences_in(config_dir)?;
    prefs.dismissed_version = non_empty_string(latest_version.to_string());
    prefs.dismissed_at_ms = Some(dismissed_at_ms);
    save_update_preferences_in(config_dir, &prefs)
}

pub fn load_agent_view_preferences() -> anyhow::Result<AgentViewPreferences> {
    load_agent_view_preferences_in(&config_home_dir())
}

pub fn load_agent_view_preferences_in(config_dir: &Path) -> anyhow::Result<AgentViewPreferences> {
    let config = read_config_roundtrip(config_dir)?;
    Ok(agent_view_preferences_from_config(&config))
}

pub fn save_agent_view_preferences(prefs: &AgentViewPreferences) -> anyhow::Result<()> {
    save_agent_view_preferences_in(&config_home_dir(), prefs)
}

pub fn save_agent_view_preferences_in(
    config_dir: &Path,
    prefs: &AgentViewPreferences,
) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    upsert_agent_view_preferences(&mut config, prefs)?;
    write_config_roundtrip(config_dir, &config)
}

pub fn agent_view_is_disabled() -> bool {
    if env_truthy("REBON_CODE_DISABLE_AGENT_VIEW") {
        return true;
    }
    load_agent_view_preferences()
        .map(|prefs| prefs.disabled)
        .unwrap_or(false)
}

pub fn background_permission_mode_requires_interactive_acceptance(mode: PermissionMode) -> bool {
    matches!(
        mode,
        PermissionMode::Auto | PermissionMode::BypassPermissions
    )
}

pub fn background_permission_mode_is_accepted(mode: PermissionMode) -> bool {
    background_permission_mode_is_accepted_in(&config_home_dir(), mode)
}

pub fn background_permission_mode_is_accepted_in(config_dir: &Path, mode: PermissionMode) -> bool {
    if !background_permission_mode_requires_interactive_acceptance(mode) {
        return true;
    }
    let Ok(config) = read_config_roundtrip(config_dir) else {
        return false;
    };
    accepted_background_permission_modes_from_config(&config)
        .iter()
        .any(|accepted| accepted == mode.as_wire())
}

pub fn ensure_background_permission_mode_allowed(mode: PermissionMode) -> anyhow::Result<()> {
    if !background_permission_mode_requires_interactive_acceptance(mode)
        || background_permission_mode_is_accepted(mode)
    {
        return Ok(());
    }
    bail_unaccepted_background_permission_mode(mode)
}

// (was #[cfg(test)] — promoted to a regular pub helper so an
// out-of-crate test suite can reach it across the crate boundary.)
pub fn ensure_background_permission_mode_allowed_in(
    config_dir: &Path,
    mode: PermissionMode,
) -> anyhow::Result<()> {
    if !background_permission_mode_requires_interactive_acceptance(mode)
        || background_permission_mode_is_accepted_in(config_dir, mode)
    {
        return Ok(());
    }
    bail_unaccepted_background_permission_mode(mode)
}

pub fn mark_background_permission_mode_accepted(mode: PermissionMode) -> anyhow::Result<()> {
    mark_background_permission_mode_accepted_in(&config_home_dir(), mode)
}

pub fn mark_background_permission_mode_accepted_wire(mode: &str) -> anyhow::Result<()> {
    mark_background_permission_mode_accepted_wire_in(&config_home_dir(), mode)
}

pub fn mark_background_permission_mode_accepted_wire_in(
    config_dir: &Path,
    mode: &str,
) -> anyhow::Result<()> {
    let Some(mode) = parse_user_permission_mode(mode) else {
        anyhow::bail!("unknown permission mode `{mode}`");
    };
    mark_background_permission_mode_accepted_in(config_dir, mode)
}

pub fn mark_background_permission_mode_accepted_in(
    config_dir: &Path,
    mode: PermissionMode,
) -> anyhow::Result<()> {
    if !background_permission_mode_requires_interactive_acceptance(mode) {
        return Ok(());
    }

    let mut config = read_config_roundtrip(config_dir)?;
    let mut raw = match config.extra.remove(AGENT_VIEW_CONFIG_KEY) {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    let mut accepted = accepted_background_permission_modes_from_agent_view(&raw);
    let wire = mode.as_wire().to_string();
    if accepted.iter().any(|value| value == &wire) {
        return Ok(());
    }
    accepted.push(wire);
    accepted.sort();
    raw.insert(
        AGENT_VIEW_ACCEPTED_BACKGROUND_PERMISSION_MODES_KEY.to_string(),
        serde_json::Value::Array(
            accepted
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    config.extra.insert(
        AGENT_VIEW_CONFIG_KEY.to_string(),
        serde_json::Value::Object(raw),
    );
    write_config_roundtrip(config_dir, &config)
}

fn agent_view_preferences_from_config(config: &RebonConfigRoundTrip) -> AgentViewPreferences {
    let Some(raw_agent_view) = config.extra.get(AGENT_VIEW_CONFIG_KEY) else {
        return AgentViewPreferences::default();
    };
    match serde_json::from_value::<AgentViewPreferencesSerde>(raw_agent_view.clone()) {
        Ok(raw) => raw.into(),
        Err(err) => {
            tracing::debug!(error = %err, "ignoring invalid agentView config");
            AgentViewPreferences::default()
        }
    }
}

fn accepted_background_permission_modes_from_config(config: &RebonConfigRoundTrip) -> Vec<String> {
    match config.extra.get(AGENT_VIEW_CONFIG_KEY) {
        Some(serde_json::Value::Object(raw)) => {
            accepted_background_permission_modes_from_agent_view(raw)
        }
        _ => Vec::new(),
    }
}

fn accepted_background_permission_modes_from_agent_view(
    raw: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    raw.get(AGENT_VIEW_ACCEPTED_BACKGROUND_PERMISSION_MODES_KEY)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|value| matches!(*value, "auto" | "bypassPermissions"))
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn bail_unaccepted_background_permission_mode(mode: PermissionMode) -> anyhow::Result<()> {
    let wire = mode.as_wire();
    anyhow::bail!(
        "permission mode `{wire}` cannot be used for background sessions until it has been accepted in an interactive session; run `rebon --permission-mode {wire}` interactively once, then retry"
    )
}

fn upsert_agent_view_preferences(
    config: &mut RebonConfigRoundTrip,
    prefs: &AgentViewPreferences,
) -> anyhow::Result<()> {
    let mut raw = match config.extra.remove(AGENT_VIEW_CONFIG_KEY) {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    let grouping = match prefs.grouping.trim() {
        "directory" => "directory",
        _ => "state",
    };
    raw.insert(
        "grouping".to_string(),
        serde_json::Value::String(grouping.to_string()),
    );
    if prefs.disabled {
        raw.insert("disabled".to_string(), serde_json::Value::Bool(true));
    } else {
        raw.remove("disabled");
    }
    config.extra.insert(
        AGENT_VIEW_CONFIG_KEY.to_string(),
        serde_json::Value::Object(raw),
    );
    Ok(())
}

fn update_preferences_from_config(config: &RebonConfigRoundTrip) -> UpdatePreferences {
    let Some(raw_updates) = config.extra.get(UPDATES_CONFIG_KEY) else {
        return UpdatePreferences::default();
    };
    match serde_json::from_value::<UpdatePreferencesSerde>(raw_updates.clone()) {
        Ok(raw) => raw.into(),
        Err(err) => {
            tracing::debug!(error = %err, "ignoring invalid updates config");
            UpdatePreferences::default()
        }
    }
}

fn upsert_update_preferences(
    config: &mut RebonConfigRoundTrip,
    prefs: &UpdatePreferences,
) -> anyhow::Result<()> {
    let mut raw = match config.extra.remove(UPDATES_CONFIG_KEY) {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };

    raw.insert(
        "disabled".to_string(),
        serde_json::Value::Bool(prefs.disabled),
    );
    raw.insert(
        "autoInstall".to_string(),
        serde_json::Value::Bool(prefs.auto_install),
    );
    set_optional_string_field(&mut raw, "channel", prefs.channel.as_deref());
    set_optional_string_field(&mut raw, "skippedVersion", prefs.skipped_version.as_deref());
    set_optional_string_field(
        &mut raw,
        "dismissedVersion",
        prefs.dismissed_version.as_deref(),
    );
    match prefs.dismissed_at_ms {
        Some(ms) => {
            raw.insert(
                "dismissedAtMs".to_string(),
                serde_json::Value::Number(serde_json::Number::from(ms)),
            );
        }
        None => {
            raw.remove("dismissedAtMs");
        }
    }

    config.extra.insert(
        UPDATES_CONFIG_KEY.to_string(),
        serde_json::Value::Object(raw),
    );
    Ok(())
}

fn set_optional_string_field(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<&str>,
) {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        Some(value) => {
            map.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        None => {
            map.remove(key);
        }
    }
}

fn resolve_provider_options(
    provider: &CustomProvider,
) -> (Vec<(String, String)>, OpenAiRequestOptions) {
    let mut request_options = provider_options_to_request_options(&provider.options);
    apply_thinking_options(provider, &mut request_options);
    (
        resolve_extra_headers(&provider.options.headers),
        request_options,
    )
}

fn resolve_model_request_options(
    provider: &CustomProvider,
) -> BTreeMap<String, OpenAiRequestOptions> {
    provider
        .models
        .model_options()
        .into_iter()
        .map(|(id, options)| (id, provider_options_to_request_options(&options)))
        .collect()
}

fn resolve_model_context_windows(provider: &CustomProvider) -> BTreeMap<String, u32> {
    provider.models.context_windows()
}

fn resolve_model_output_token_limits(provider: &CustomProvider) -> BTreeMap<String, u32> {
    provider.models.output_token_limits()
}

pub fn resolve_model_context_window(
    provider: Option<&ResolvedProvider>,
    model: &str,
) -> Option<u32> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    provider.and_then(|provider| provider.model_context_windows.get(model).copied())
}

pub fn resolve_model_output_token_limit(
    provider: Option<&ResolvedProvider>,
    model: &str,
) -> Option<u32> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    provider.and_then(|provider| provider.model_output_token_limits.get(model).copied())
}

fn provider_options_to_request_options(options: &ProviderOptions) -> OpenAiRequestOptions {
    OpenAiRequestOptions {
        body: options.body.clone(),
        extra_body: options.extra_body.clone(),
        omit_body_fields: options.omit_body_fields.clone(),
    }
}

fn apply_thinking_options(provider: &CustomProvider, options: &mut OpenAiRequestOptions) {
    let Some(enabled) = provider.thinking_enabled else {
        return;
    };
    options.extra_body.insert(
        "thinking".to_string(),
        serde_json::json!({ "type": if enabled { "enabled" } else { "disabled" } }),
    );
    if enabled {
        if !options
            .omit_body_fields
            .iter()
            .any(|field| field == "temperature")
        {
            options.omit_body_fields.push("temperature".to_string());
        }
        let effort = provider
            .thinking_effort
            .as_deref()
            .map(thinking_effort_to_wire)
            .unwrap_or("high");
        options
            .body
            .insert("reasoning_effort".to_string(), serde_json::json!(effort));
    }
}

fn thinking_effort_to_wire(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "xhigh" => "xhigh",
        "max" => "max",
        _ => "high",
    }
}

fn resolve_extra_headers(
    headers: &serde_json::Map<String, serde_json::Value>,
) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            if is_reserved_header(name) {
                return None;
            }
            let raw = value.as_str()?;
            let resolved = resolve_env_value(raw);
            if resolved.is_empty() {
                None
            } else {
                Some((name.clone(), resolved))
            }
        })
        .collect()
}

fn is_reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "content-type" | "accept" | "host" | "content-length"
    )
}

/// Read and parse `config.json`. Returns `Ok(None)` if the file
/// does not exist (the fallback branch); other IO or parse errors
/// surface as `Err` so the caller can report them instead of
/// silently dropping into env-var mode.
fn read_config_json(path: &Path) -> anyhow::Result<Option<RebonConfig>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(anyhow::Error::from(err)
                .context(format!("failed to read rebon config.json at {path:?}")));
        }
    };
    let config: RebonConfig = serde_json::from_slice(&bytes)
        .map_err(|err| anyhow::anyhow!("failed to parse rebon config.json at {path:?}: {err}"))?;
    Ok(Some(config))
}

/// Validate that the primary `config.json` is syntactically valid JSON.
///
/// Returns `Ok(None)` when the file does not exist, `Ok(Some(()))` when
/// it parses, and `Err(ConfigParseFailure)` when the file exists but
/// contains invalid JSON.
pub fn check_primary_config_json(config_dir: &Path) -> Result<Option<()>, ConfigParseFailure> {
    let path = config_json_path(config_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ConfigParseFailure {
                file_path: path,
                message: err.to_string(),
                default_config: default_primary_config(),
            });
        }
    };

    serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|err| ConfigParseFailure {
        file_path: path,
        message: err.to_string(),
        default_config: default_primary_config(),
    })?;

    Ok(Some(()))
}

pub fn check_config_roundtrip_schema(config_dir: &Path) -> anyhow::Result<Option<()>> {
    let path = config_json_path(config_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(
                anyhow::Error::from(err).context(format!("failed to read config at {path:?}"))
            );
        }
    };
    let config: RebonConfigRoundTrip = serde_json::from_slice(&bytes)
        .map_err(|err| anyhow::anyhow!("failed to parse config at {path:?}: {err}"))?;
    serde_json::to_vec(&config)
        .map_err(|err| anyhow::anyhow!("failed to serialize round-trip config: {err}"))?;
    Ok(Some(()))
}

fn default_primary_config() -> serde_json::Value {
    serde_json::json!({
        "customProviders": []
    })
}

/// Read and parse `.credentials.json`. Returns `Ok(None)` when
/// the file does not exist (the common case for fresh installs)
/// or when `openaiOAuth` is absent. IO / parse errors surface
/// as `Err`.
///
/// Made `pub` so the OAuth refresh helper can reuse
/// the same parse path.
pub fn read_credentials(config_dir: &Path) -> anyhow::Result<Credentials> {
    let path = credentials_json_path(config_dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Credentials::default());
        }
        Err(err) => {
            return Err(anyhow::Error::from(err)
                .context(format!("failed to read rebon credentials at {path:?}")));
        }
    };
    let credentials: Credentials = serde_json::from_slice(&bytes)
        .map_err(|err| anyhow::anyhow!("failed to parse rebon credentials at {path:?}: {err}"))?;
    Ok(credentials)
}

/// Resolve the provider's `apiKey` field. If it is the
/// `$OPENAI_OAUTH_TOKEN` sentinel, pull the real token out of
/// `.credentials.json`; otherwise return the literal value.
///
/// On the sentinel path, also returns [`OAuthMeta`] so the caller
/// (the refresh helper) can decide whether to rotate before
/// actually using the token.
fn resolve_api_key(
    provider: &CustomProvider,
    config_dir: &Path,
) -> anyhow::Result<(String, Option<OAuthMeta>)> {
    if provider.api_key != OPENAI_OAUTH_TOKEN_SENTINEL {
        return Ok((provider.api_key.clone(), None));
    }

    let credentials = read_credentials(config_dir)?;
    let Some(tokens) = credentials.openai_oauth else {
        anyhow::bail!(
            "rebon config points at OpenAI OAuth provider `{name}` but \
             no openaiOAuth entry exists in .credentials.json — run \
             `rebon` and `/login` first",
            name = provider.name,
        );
    };
    if tokens.access_token.is_empty() {
        anyhow::bail!("rebon OpenAI OAuth access token is empty — run `rebon` and `/login`");
    }
    let oauth = OAuthMeta {
        expires_at_ms: tokens.expires_at,
        refresh_token: tokens.refresh_token,
    };
    Ok((tokens.access_token, Some(oauth)))
}

// ---------------------------------------------------------------------------
// OAuth token refresh
// ---------------------------------------------------------------------------

/// OAuth token endpoint.
pub const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// OAuth client id used by the ChatGPT Codex login flow.
pub const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Buffer subtracted from the token's `expires_at` when deciding
/// whether to refresh.
pub const TOKEN_EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;

/// HTTP timeout for the refresh POST.
pub const OAUTH_REFRESH_TIMEOUT_MS: u64 = 15_000;

/// Pure decision function: is the given `expires_at` considered
/// expired at `now_ms`?
///
/// `None` means "no expiry info recorded, refresh immediately";
/// otherwise the buffer is applied to the comparison.
pub fn is_token_expired_at(now_ms: u64, expires_at_ms: Option<u64>) -> bool {
    match expires_at_ms {
        None => true,
        Some(expires_at) => now_ms.saturating_add(TOKEN_EXPIRY_BUFFER_MS) >= expires_at,
    }
}

/// Convenience wrapper around [`is_token_expired_at`] that uses
/// the wall-clock.
pub fn is_token_expired(expires_at_ms: Option<u64>) -> bool {
    is_token_expired_at(now_unix_ms(), expires_at_ms)
}

/// Wall-clock milliseconds since the Unix epoch. Silently returns
/// `0` if the clock is earlier than 1970 instead of failing.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wire shape of the `/oauth/token` refresh response.
///
/// Only the fields we actually store are modelled; `id_token` is
/// declined on purpose because extracting `accountId` would require a
/// JWT decoder and the config dir owns the account-id on disk.
#[derive(Debug, Clone, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Result of a successful refresh: the fresh tokens **and** the
/// updated on-disk credentials blob. Callers only need the tokens;
/// the blob is returned for observability (tests inspect it to
/// confirm round-trip behaviour).
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    /// The new token bundle written back to `.credentials.json`.
    pub tokens: OpenAIOAuthTokens,
}

/// Why an OAuth refresh failed. Callers need the distinction: a
/// rejected grant can only be fixed by logging in again, while a
/// timeout or a 5xx says nothing about the stored tokens and must
/// not cost the user their session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRefreshErrorKind {
    /// The authorization server refused the stored refresh token
    /// (`invalid_grant`, `refresh_token_invalidated`, 401/403), or no
    /// usable refresh token is on disk. Only `/login` fixes this.
    Unauthorized,
    /// Network trouble, a timeout, or a server-side error. The cached
    /// access token may still work and a later attempt may succeed.
    Transient,
}

/// A failed OAuth refresh, carrying [`OAuthRefreshErrorKind`] so a
/// caller can tell "log in again" from "try again later" without
/// pattern-matching on the message text.
#[derive(Debug, Clone)]
pub struct OAuthRefreshError {
    kind: OAuthRefreshErrorKind,
    message: String,
}

impl OAuthRefreshError {
    pub fn new(kind: OAuthRefreshErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(OAuthRefreshErrorKind::Unauthorized, message)
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(OAuthRefreshErrorKind::Transient, message)
    }

    pub fn kind(&self) -> OAuthRefreshErrorKind {
        self.kind
    }

    /// Whether recovering needs a fresh `/login`.
    pub fn requires_login(&self) -> bool {
        matches!(self.kind, OAuthRefreshErrorKind::Unauthorized)
    }
}

impl std::fmt::Display for OAuthRefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OAuthRefreshError {}

/// Classify a non-2xx response from the token endpoint.
///
/// The OAuth token endpoint answers a dead grant with
/// `400 invalid_grant`; the ChatGPT endpoint answers with
/// `401 refresh_token_invalidated`. Both
/// mean the same thing: the refresh token on disk is spent. Everything
/// else — 429, 5xx, anything unrecognised — is treated as transient so
/// a bad minute at the auth host never logs the user out.
pub fn classify_refresh_status(status: u16, body: &str) -> OAuthRefreshErrorKind {
    match status {
        400 | 401 | 403 => OAuthRefreshErrorKind::Unauthorized,
        _ => {
            let body = body.to_ascii_lowercase();
            if body.contains("invalid_grant")
                || body.contains("invalid_request_error")
                || body.contains("refresh_token_invalidated")
            {
                OAuthRefreshErrorKind::Unauthorized
            } else {
                OAuthRefreshErrorKind::Transient
            }
        }
    }
}

/// The refresh-failure kind behind `err`, when it came from this
/// module's refresh path. `None` for anything else (a disk write
/// failure, say), which callers should treat as "not the user's
/// credentials".
pub fn oauth_refresh_error_kind(err: &anyhow::Error) -> Option<OAuthRefreshErrorKind> {
    err.downcast_ref::<OAuthRefreshError>()
        .map(OAuthRefreshError::kind)
}

/// Whether `err` says the stored OAuth session is spent and only a new
/// `/login` will do.
pub fn oauth_refresh_requires_login(err: &anyhow::Error) -> bool {
    matches!(
        oauth_refresh_error_kind(err),
        Some(OAuthRefreshErrorKind::Unauthorized)
    )
}

/// POST to `https://auth.openai.com/oauth/token` with the stored
/// refresh token, update `.credentials.json` with the new access
/// token + rotated refresh token + recomputed `expires_at`, and
/// return the fresh tokens.
///
/// Uses a fresh reqwest
/// client rather than the shared model-client pool because the
/// auth endpoint is a completely different host and middleware
/// (retry, logging) should not interact with it.
///
/// **Token rotation**: the server invalidates the old refresh
/// token on success and returns a new one. If the file write
/// fails between "server rotated" and "disk updated", the caller
/// is stuck until the next `/login`.
pub async fn force_refresh_openai_token(
    config_dir: &Path,
    refresh_token: &str,
) -> anyhow::Result<RefreshOutcome> {
    if refresh_token.is_empty() {
        return Err(OAuthRefreshError::unauthorized(
            "rebon OpenAI OAuth refresh token is empty — run `rebon` and `/login`",
        )
        .into());
    }

    tracing::debug!(
        token_url = OPENAI_OAUTH_TOKEN_URL,
        "rebon-cli: refreshing OpenAI OAuth access token"
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(OAUTH_REFRESH_TIMEOUT_MS))
        .build()
        .map_err(|err| {
            OAuthRefreshError::transient(format!(
                "failed to build OAuth refresh HTTP client: {err}"
            ))
        })?;

    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
    ];

    let response = http
        .post(OPENAI_OAUTH_TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|err| {
            OAuthRefreshError::transient(format!("OAuth refresh HTTP request failed: {err}"))
        })?;

    let status = response.status();
    let body = response.text().await.map_err(|err| {
        OAuthRefreshError::transient(format!("failed to read OAuth refresh response body: {err}"))
    })?;

    if !status.is_success() {
        return Err(OAuthRefreshError::new(
            classify_refresh_status(status.as_u16(), &body),
            format!(
                "OAuth refresh returned {status}: {body} \
                 — run `rebon` and `/login` to re-authenticate"
            ),
        )
        .into());
    }

    let parsed: RefreshResponse = serde_json::from_str(&body).map_err(|err| {
        OAuthRefreshError::transient(format!("failed to parse OAuth refresh response: {err}"))
    })?;

    let new_tokens = OpenAIOAuthTokens {
        access_token: parsed.access_token,
        // Preserve the existing refresh token when the server
        // did not rotate it (spec says it's optional to return).
        refresh_token: parsed
            .refresh_token
            .or_else(|| Some(refresh_token.to_string())),
        expires_at: parsed
            .expires_in
            .map(|secs| now_unix_ms().saturating_add(secs.saturating_mul(1000))),
    };

    write_openai_oauth_tokens(config_dir, &new_tokens)?;

    tracing::info!(
        expires_at_ms = ?new_tokens.expires_at,
        "rebon-cli: refreshed OpenAI OAuth access token"
    );

    Ok(RefreshOutcome { tokens: new_tokens })
}

/// Check [`OAuthMeta`] and refresh if the stored access token is
/// within the expiry buffer. Returns the fresh access token when
/// a refresh ran, or `None` when the cached token is still valid.
pub async fn check_and_refresh_if_needed(
    config_dir: &Path,
    oauth: &OAuthMeta,
) -> anyhow::Result<Option<String>> {
    if !is_token_expired(oauth.expires_at_ms) {
        return Ok(None);
    }
    let Some(refresh_token) = oauth.refresh_token.as_deref() else {
        return Err(OAuthRefreshError::unauthorized(
            "rebon OpenAI OAuth access token is expired and no refresh token \
             is stored — run `rebon` and `/login` to re-authenticate",
        )
        .into());
    };
    let outcome = force_refresh_openai_token(config_dir, refresh_token).await?;
    Ok(Some(outcome.tokens.access_token))
}

/// What the startup OAuth check found, in the terms a startup surface
/// (TUI, ACP, harness) has to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthStartupState {
    /// The cached access token is still inside its validity window.
    Cached,
    /// The token was rotated — use this access token instead of the one
    /// resolution read off disk.
    Refreshed(String),
    /// The authorization server rejected the stored refresh token. Only a
    /// new `/login` recovers; the cached token will 401.
    LoginRequired,
    /// The refresh could not complete (network, timeout, 5xx). The stored
    /// tokens are untouched and the cached one is still worth trying.
    Unavailable,
}

/// Run the preemptive OAuth refresh and report what the caller should do
/// about it.
///
/// Deliberately infallible. Startup used to propagate the refresher's
/// error, so an invalidated refresh token killed the process before any
/// surface existed to offer a `/login` — the one action that fixes it.
/// Callers get a state to render instead: rotate the token, prompt for a
/// login, or carry on with what is cached and let the per-request 401
/// refresher try again.
pub async fn check_startup_oauth(config_dir: &Path, oauth: &OAuthMeta) -> OAuthStartupState {
    match check_and_refresh_if_needed(config_dir, oauth).await {
        Ok(Some(access_token)) => OAuthStartupState::Refreshed(access_token),
        Ok(None) => OAuthStartupState::Cached,
        Err(err) if oauth_refresh_requires_login(&err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "rebon: stored OAuth session was rejected; a new login is required"
            );
            OAuthStartupState::LoginRequired
        }
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "rebon: preemptive OAuth refresh failed; keeping the cached token"
            );
            OAuthStartupState::Unavailable
        }
    }
}

/// Atomically update `<config_dir>/.credentials.json` with the
/// given OpenAI OAuth tokens, preserving every other top-level
/// key already stored in the file (the `claudeAiOauth` entry among them).
///
/// Atomicity: write to a temp file alongside the target, then
/// rename. `std::fs::rename` is an atomic replace on POSIX and
/// Windows (since Win Server 2003), so a crash between the two
/// steps either leaves the old file intact or the new one — never
/// a truncated blob.
///
/// Permissions: on Unix the target file is chmod'ed to `0o600`.
/// On Windows the chmod is a
/// no-op and the file inherits its parent directory's ACL.
pub fn write_openai_oauth_tokens(
    config_dir: &Path,
    tokens: &OpenAIOAuthTokens,
) -> anyhow::Result<()> {
    let target = credentials_json_path(config_dir);
    let mut credentials = read_credentials(config_dir)?;
    credentials.openai_oauth = Some(tokens.clone());

    // Ensure the config dir exists before writing; an already-existing
    // directory is fine.
    if let Err(err) = std::fs::create_dir_all(config_dir) {
        return Err(anyhow::Error::from(err).context(format!(
            "failed to create rebon config dir at {config_dir:?}"
        )));
    }

    let serialized = serde_json::to_vec_pretty(&credentials)
        .map_err(|err| anyhow::anyhow!("failed to serialize updated credentials: {err}"))?;

    rebon_session::write_private_file_atomically(&target, &serialized).map_err(|err| {
        anyhow::Error::from(err).context(format!("failed to write credentials at {target:?}"))
    })?;
    notify_config_changed(ConfigFileKind::Credentials, &target);
    Ok(())
}

// ---------------------------------------------------------------------------
// Provider CRUD
// ---------------------------------------------------------------------------

/// Public view of a custom provider entry for display purposes.
/// Intentionally separate from the private [`CustomProvider`]
/// (on-disk serde shape) so callers don't depend on internal
/// serialization details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomProviderInfo {
    pub name: String,
    pub format: String,
    pub base_url: String,
    pub api_key: String,
    /// Currently active model for this provider (always set).
    pub model: String,
    /// All known models for this provider. Empty for old configs
    /// that only stored `model`.
    pub models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSetupCredential {
    Stored,
    Environment { variable: String, available: bool },
    OpenAiOAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSetupProvider {
    pub name: String,
    pub model: String,
    pub active: bool,
    pub credential: ProviderSetupCredential,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSetupEnvironment {
    pub provider: String,
    pub variable: String,
}

/// `Default` is a config home nobody has touched: no providers, no
/// environment credentials, no read error, setup not finished. It is what a
/// test that does not care about providers opens the setup wizard with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderSetupStatus {
    pub onboarding_completed: bool,
    pub providers: Vec<ProviderSetupProvider>,
    pub environment_providers: Vec<ProviderSetupEnvironment>,
    pub config_error: Option<String>,
}

impl ProviderSetupStatus {
    pub fn active_provider(&self) -> Option<&ProviderSetupProvider> {
        self.providers.iter().find(|provider| provider.active)
    }

    pub fn has_provider_configuration(&self) -> bool {
        !self.providers.is_empty() || !self.environment_providers.is_empty()
    }

    pub fn should_confirm_reconfiguration(&self) -> bool {
        self.onboarding_completed
            || self.has_provider_configuration()
            || self.config_error.is_some()
    }
}

/// Valid provider format strings accepted by `/provider add`.
pub const VALID_PROVIDER_FORMATS: &[&str] = &["openai", "openai-responses", "anthropic"];

/// One model a preset seeds an entry with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPresetModel {
    pub id: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

/// Where a vendor sells to. Only a grouping for the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetRegion {
    /// Sells worldwide.
    Global,
    /// A mainland-China vendor (or a China endpoint of one).
    China,
}

impl PresetRegion {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "国际",
            Self::China => "中国",
        }
    }
}

/// A vendor (or a way of deploying one) a user can add in one click.
///
/// What a preset carries is what the vendor's documentation states: the
/// endpoint, the wire format that endpoint speaks, the flagship model to
/// start on, and where to get a key. Everything behaviour-related — how to
/// switch thinking on, whether the prefix cache is byte-exact, where the
/// model list is — lives on [`rebon_api::ProviderVendor`] and is looked up
/// from the entry's `vendor` / host at request time, so an entry a user
/// typed by hand gets the same treatment as one a preset created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    /// Stable id — the provider entry's name and the `/provider add`
    /// argument.
    pub id: &'static str,
    pub display_name: &'static str,
    pub vendor: rebon_api::ProviderVendor,
    pub region: PresetRegion,
    /// Wire format: `openai`, `openai-responses` or `anthropic`.
    pub format: &'static str,
    pub base_url: &'static str,
    /// The vendor's Anthropic-compatible endpoint, when it documents one,
    /// so switching the entry's format to `anthropic` can swap the URL.
    pub anthropic_base_url: Option<&'static str>,
    /// Model a fresh entry starts on. Empty when only the endpoint knows
    /// (Ollama: whatever is installed).
    pub default_model: &'static str,
    /// Conventional environment variable for the key; `/provider add
    /// <preset> $VAR` stores the reference.
    pub api_key_env: &'static str,
    /// Whether the endpoint needs a key at all. Ollama does not; its
    /// entry stores a placeholder so the bearer header is well-formed.
    pub api_key_required: bool,
    /// Console page that issues keys.
    pub key_url: &'static str,
    /// The API documentation the preset's facts were read from.
    pub docs_url: &'static str,
    pub description: &'static str,
    /// What a user has to know before the entry works — placeholders to
    /// fill in, model naming, an endpoint quirk. Empty when nothing.
    pub notes: &'static str,
}

impl ProviderPreset {
    /// The vendor's documented catalogue, in preset-model shape.
    pub fn models(&self) -> Vec<ProviderPresetModel> {
        self.vendor
            .known_models()
            .iter()
            .map(|known| ProviderPresetModel {
                id: self.model_id_for_deployment(known.id),
                context_window: Some(known.context_window),
                max_output_tokens: known.max_output_tokens,
            })
            .collect()
    }

    /// `{PLACEHOLDER}` tokens still to be filled in `base_url`.
    pub fn base_url_placeholders(&self) -> Vec<&'static str> {
        base_url_placeholders(self.base_url)
    }

    /// The wire family, for model discovery.
    pub fn wire_family(&self) -> rebon_api::WireFamily {
        wire_family_for_format(self.format)
    }

    /// Bedrock names Claude with an `anthropic.` prefix; every other
    /// deployment takes the plain id.
    fn model_id_for_deployment(&self, id: &str) -> String {
        if self.id == "anthropic-bedrock" {
            format!("anthropic.{id}")
        } else {
            id.to_string()
        }
    }
}

/// `{PLACEHOLDER}` tokens in a URL.
pub fn base_url_placeholders(base_url: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = base_url;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let token = &after[..end];
        if !token.is_empty()
            && token
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && !out.contains(&token)
        {
            out.push(token);
        }
        rest = &after[end + 1..];
    }
    out
}

/// Map a provider `format` string onto the listing wire family.
pub fn wire_family_for_format(format: &str) -> rebon_api::WireFamily {
    match format.trim() {
        "anthropic" => rebon_api::WireFamily::Anthropic,
        "openai-responses" => rebon_api::WireFamily::OpenAiResponses,
        _ => rebon_api::WireFamily::OpenAiChat,
    }
}

use rebon_api::ProviderVendor as V;

/// Every preset, in picker order: China first (DeepSeek has been the
/// quick-setup default since the presets existed), then worldwide.
///
/// Endpoints, flagship models and key pages are taken from each vendor's
/// own documentation; the page is on [`ProviderPreset::docs_url`].
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    // ── China ──────────────────────────────────────────────────
    ProviderPreset {
        id: "deepseek",
        display_name: "DeepSeek",
        vendor: V::DeepSeek,
        region: PresetRegion::China,
        format: "openai",
        base_url: "https://api.deepseek.com",
        anthropic_base_url: Some("https://api.deepseek.com/anthropic"),
        default_model: "deepseek-flash",
        api_key_env: "DEEPSEEK_API_KEY",
        api_key_required: true,
        key_url: "https://platform.deepseek.com/api_keys",
        docs_url: "https://api-docs.deepseek.com/",
        description: "V4 Pro / Flash，磁盘前缀缓存",
        notes: "",
    },
    ProviderPreset {
        id: "volcengine",
        display_name: "火山方舟",
        vendor: V::Volcengine,
        region: PresetRegion::China,
        format: "openai",
        // docs.volcengine.com/docs/82379/1298459 (Base URL 及鉴权)
        base_url: "https://ark.cn-beijing.volces.com/api/v3",
        anthropic_base_url: None,
        default_model: "doubao-seed-evolving",
        api_key_env: "ARK_API_KEY",
        api_key_required: true,
        key_url: "https://console.volcengine.com/ark/region:ark+cn-beijing/apiKey",
        docs_url: "https://www.volcengine.com/docs/82379/1494384",
        description: "豆包 Seed 系列",
        notes: "方舟没有模型列表接口，模型来自内置目录；也可以直接填推理接入点 ID（ep-…）。Coding Plan / Agent Plan 走各自的专属 Base URL。",
    },
    ProviderPreset {
        id: "siliconflow",
        display_name: "硅基流动",
        vendor: V::SiliconFlow,
        region: PresetRegion::China,
        format: "openai",
        base_url: "https://api.siliconflow.cn/v1",
        anthropic_base_url: None,
        default_model: "deepseek-ai/DeepSeek-V4-Pro",
        api_key_env: "SILICONFLOW_API_KEY",
        api_key_required: true,
        key_url: "https://cloud.siliconflow.cn/account/ak",
        docs_url: "https://docs.siliconflow.cn/cn/api-reference/chat-completions/chat-completions",
        description: "开源模型托管，DeepSeek / Qwen / Kimi / GLM",
        notes: "",
    },
    ProviderPreset {
        id: "glm",
        display_name: "智谱 GLM",
        vendor: V::Zhipu,
        region: PresetRegion::China,
        format: "openai",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        anthropic_base_url: Some("https://open.bigmodel.cn/api/anthropic"),
        default_model: "glm-5.3",
        api_key_env: "ZHIPUAI_API_KEY",
        api_key_required: true,
        key_url: "https://open.bigmodel.cn/usercenter/proj-mgmt/apikeys",
        docs_url: "https://docs.bigmodel.cn/cn/guide/start/model-overview",
        description: "GLM-5.3，bigmodel.cn",
        notes: "",
    },
    ProviderPreset {
        id: "qwen",
        display_name: "通义千问",
        vendor: V::Qwen,
        region: PresetRegion::China,
        format: "openai",
        // help.aliyun.com/zh/model-studio/compatibility-of-openai-with-dashscope
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        anthropic_base_url: Some("https://dashscope.aliyuncs.com/apps/anthropic"),
        default_model: "qwen3.8-max",
        api_key_env: "DASHSCOPE_API_KEY",
        api_key_required: true,
        key_url: "https://bailian.console.aliyun.com/?apiKey=1#/api-key",
        docs_url: "https://help.aliyun.com/zh/model-studio/models",
        description: "阿里云百炼 DashScope",
        notes: "",
    },
    ProviderPreset {
        id: "kimi",
        display_name: "Kimi",
        vendor: V::Kimi,
        region: PresetRegion::China,
        format: "openai",
        base_url: "https://api.moonshot.cn/v1",
        anthropic_base_url: Some("https://api.moonshot.cn/anthropic"),
        default_model: "kimi-k3",
        api_key_env: "MOONSHOT_API_KEY",
        api_key_required: true,
        key_url: "https://platform.moonshot.cn/console/api-keys",
        docs_url: "https://platform.kimi.com/docs/models",
        description: "Moonshot K3 / K2.7",
        notes: "",
    },
    ProviderPreset {
        id: "minimax",
        display_name: "MiniMax",
        vendor: V::MiniMax,
        region: PresetRegion::China,
        format: "openai",
        base_url: "https://api.minimaxi.com/v1",
        anthropic_base_url: Some("https://api.minimaxi.com/anthropic"),
        default_model: "MiniMax-M3",
        api_key_env: "MINIMAX_API_KEY",
        api_key_required: true,
        key_url: "https://platform.minimaxi.com/user-center/basic-information/interface-key",
        docs_url: "https://platform.minimaxi.com/docs/api-reference/text-openai-api",
        description: "M3 / M2.7，国内站",
        notes: "海外站把 Base URL 换成 https://api.minimax.io/v1。",
    },
    // ── Worldwide ──────────────────────────────────────────────
    ProviderPreset {
        id: "openai",
        display_name: "OpenAI",
        vendor: V::OpenAi,
        region: PresetRegion::Global,
        // developers.openai.com/api/docs/guides/migrate-to-responses:
        // "Responses is recommended for all new projects"; it also carries
        // reasoning items and `prompt_cache_key` across turns.
        format: "openai-responses",
        base_url: "https://api.openai.com/v1",
        anthropic_base_url: None,
        default_model: "gpt-5.6-sol",
        api_key_env: "OPENAI_API_KEY",
        api_key_required: true,
        key_url: "https://platform.openai.com/api-keys",
        docs_url: "https://developers.openai.com/api/docs/models",
        description: "GPT-5.6 家族，Responses API",
        notes: "",
    },
    ProviderPreset {
        id: "anthropic",
        display_name: "Anthropic",
        vendor: V::Anthropic,
        region: PresetRegion::Global,
        format: "anthropic",
        base_url: "https://api.anthropic.com",
        anthropic_base_url: Some("https://api.anthropic.com"),
        default_model: "claude-opus-5",
        api_key_env: "ANTHROPIC_API_KEY",
        api_key_required: true,
        key_url: "https://platform.claude.com/settings/keys",
        docs_url: "https://platform.claude.com/docs/en/about-claude/models/overview",
        description: "Claude Opus 5 / Sonnet 5，官方 API",
        notes: "",
    },
    ProviderPreset {
        id: "anthropic-vertex",
        display_name: "Anthropic · Vertex AI",
        vendor: V::Anthropic,
        region: PresetRegion::Global,
        format: "anthropic",
        // platform.claude.com/docs/en/api/claude-on-vertex-ai: the global
        // endpoint is the recommended one and serves every current model.
        base_url: "https://aiplatform.googleapis.com/v1/projects/{PROJECT}/locations/global",
        anthropic_base_url: None,
        default_model: "claude-opus-5",
        api_key_env: "VERTEX_ACCESS_TOKEN",
        api_key_required: true,
        key_url: "https://console.cloud.google.com/vertex-ai/model-garden",
        docs_url: "https://platform.claude.com/docs/en/api/claude-on-vertex-ai",
        description: "Google Cloud 上的 Claude",
        notes: "把 Base URL 里的 {PROJECT} 换成 GCP 项目 ID；API Key 填 OAuth access token（`gcloud auth print-access-token`），有效期约 1 小时，建议存到环境变量后填 $VERTEX_ACCESS_TOKEN。Vertex 没有模型列表接口，模型来自内置目录。",
    },
    ProviderPreset {
        id: "anthropic-bedrock",
        display_name: "Anthropic · Amazon Bedrock",
        vendor: V::Anthropic,
        region: PresetRegion::Global,
        format: "anthropic",
        // platform.claude.com/docs/en/build-with-claude/claude-in-amazon-bedrock:
        // the Messages wire behind `bedrock-mantle`, bearer token in
        // `x-api-key`, model ids prefixed `anthropic.`.
        base_url: "https://bedrock-mantle.{REGION}.api.aws/anthropic",
        anthropic_base_url: None,
        default_model: "anthropic.claude-opus-5",
        api_key_env: "AWS_BEARER_TOKEN_BEDROCK",
        api_key_required: true,
        key_url: "https://console.aws.amazon.com/bedrock/home#/api-keys",
        docs_url: "https://platform.claude.com/docs/en/build-with-claude/claude-in-amazon-bedrock",
        description: "AWS 上的 Claude（Opus 4.7 及更新）",
        notes: "把 Base URL 里的 {REGION} 换成区域（如 us-east-1）；API Key 填 Bedrock API key（bearer token）。模型 ID 带 `anthropic.` 前缀。Bedrock 没有模型列表接口，模型来自内置目录。",
    },
    ProviderPreset {
        id: "anthropic-foundry",
        display_name: "Anthropic · Microsoft Foundry",
        vendor: V::Anthropic,
        region: PresetRegion::Global,
        format: "anthropic",
        // platform.claude.com/docs/en/build-with-claude/claude-in-microsoft-foundry
        base_url: "https://{RESOURCE}.services.ai.azure.com/anthropic",
        anthropic_base_url: None,
        default_model: "claude-opus-5",
        api_key_env: "ANTHROPIC_FOUNDRY_API_KEY",
        api_key_required: true,
        key_url: "https://ai.azure.com/",
        docs_url: "https://platform.claude.com/docs/en/build-with-claude/claude-in-microsoft-foundry",
        description: "Azure 上的 Claude",
        notes: "把 Base URL 里的 {RESOURCE} 换成 Foundry 资源名；模型名即部署名（默认与模型 ID 相同）。Foundry 没有模型列表接口，模型来自内置目录。",
    },
    ProviderPreset {
        id: "gemini",
        display_name: "Google Gemini",
        vendor: V::Gemini,
        region: PresetRegion::Global,
        format: "openai",
        // ai.google.dev/gemini-api/docs/openai
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        anthropic_base_url: None,
        default_model: "gemini-3.7-flash",
        api_key_env: "GEMINI_API_KEY",
        api_key_required: true,
        key_url: "https://aistudio.google.com/apikey",
        docs_url: "https://ai.google.dev/gemini-api/docs/openai",
        description: "Gemini 3.x，OpenAI 兼容接口",
        notes: "",
    },
    ProviderPreset {
        id: "ollama",
        display_name: "Ollama",
        vendor: V::Ollama,
        region: PresetRegion::Global,
        format: "openai",
        // docs.ollama.com/api/openai-compatibility
        base_url: "http://localhost:11434/v1",
        anthropic_base_url: None,
        default_model: "",
        api_key_env: "OLLAMA_API_KEY",
        api_key_required: false,
        key_url: "https://ollama.com/download",
        docs_url: "https://docs.ollama.com/api/openai-compatibility",
        description: "本机模型，无需 Key",
        notes: "不需要 API Key。点「获取模型列表」读取本机已安装的模型；上下文长度来自每个模型的 /api/show。",
    },
    ProviderPreset {
        id: "opencode",
        display_name: "OpenCode Zen",
        vendor: V::OpenCode,
        region: PresetRegion::Global,
        // opencode.ai/docs/zen: one key, three wires — Claude and Qwen over
        // Messages, GPT/Grok over Responses, the rest over chat
        // completions. Messages is the default because Rebon's prompt
        // caching is richest there; switch the format to reach the others.
        format: "anthropic",
        base_url: "https://opencode.ai/zen/v1",
        anthropic_base_url: Some("https://opencode.ai/zen/v1"),
        default_model: "claude-opus-5",
        api_key_env: "OPENCODE_API_KEY",
        api_key_required: true,
        key_url: "https://opencode.ai/auth",
        docs_url: "https://opencode.ai/docs/zen/",
        description: "一把 Key 转售多家模型",
        notes: "模型按家族分协议：Anthropic 格式跑 claude-*/qwen*，Responses 格式跑 gpt-*/grok-*，OpenAI 格式跑 deepseek/minimax/glm/kimi。换格式后重新获取模型列表。",
    },
];

pub fn provider_presets() -> &'static [ProviderPreset] {
    PROVIDER_PRESETS
}

pub fn provider_preset_by_id(id: &str) -> Option<&'static ProviderPreset> {
    let id = id.trim();
    PROVIDER_PRESETS
        .iter()
        .find(|preset| preset.id.eq_ignore_ascii_case(id))
}

/// Presets in one region, picker order.
pub fn provider_presets_in(region: PresetRegion) -> impl Iterator<Item = &'static ProviderPreset> {
    PROVIDER_PRESETS
        .iter()
        .filter(move |preset| preset.region == region)
}

/// The preset a provider entry was made from, if its endpoint still matches
/// one — by pinned vendor and URL, so a renamed entry is still recognised
/// and a re-pointed one is not.
pub fn provider_preset_for_entry(
    vendor: Option<&str>,
    base_url: &str,
) -> Option<&'static ProviderPreset> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let vendor = rebon_api::ProviderVendor::resolve(vendor, base);
    PROVIDER_PRESETS.iter().find(|preset| {
        preset.vendor == vendor
            && (preset
                .base_url
                .trim_end_matches('/')
                .eq_ignore_ascii_case(base)
                || preset
                    .anthropic_base_url
                    .is_some_and(|url| url.trim_end_matches('/').eq_ignore_ascii_case(base))
                || base_url_placeholders(preset.base_url)
                    .iter()
                    .any(|token| placeholder_url_matches(preset.base_url, token, base)))
    })
}

/// Whether `candidate` is `template` with `{token}` filled in by one
/// path-free segment.
fn placeholder_url_matches(template: &str, token: &str, candidate: &str) -> bool {
    let marker = format!("{{{token}}}");
    let Some((prefix, suffix)) = template.split_once(&marker) else {
        return false;
    };
    let suffix = suffix.trim_end_matches('/');
    let Some(rest) = candidate.strip_prefix(prefix) else {
        return false;
    };
    let Some(filled) = rest.strip_suffix(suffix) else {
        return false;
    };
    !filled.is_empty() && !filled.contains('/')
}

/// Everything a model listing needs from a provider entry, with the key
/// already resolved.
#[derive(Debug, Clone)]
pub struct ProviderModelDiscoveryInput {
    /// `openai`, `openai-responses` or `anthropic` (empty means `openai`).
    pub format: String,
    pub base_url: String,
    /// As stored: a literal, a `$VAR` reference, or the OAuth sentinel.
    pub api_key: String,
    /// The entry's `vendor` pin, if any.
    pub vendor: Option<String>,
    /// `options.headers`, unresolved.
    pub headers: Vec<(String, String)>,
}

/// List the models behind a provider entry, blocking.
///
/// Resolves the key the way a session would (`$VAR` from the environment,
/// the OAuth sentinel from `.credentials.json`), recognises the vendor from
/// the pin or the host, and returns the filtered, limit-annotated list.
/// Errors are the discovery error's message, ready for a status line.
pub fn discover_models_for_provider(
    input: &ProviderModelDiscoveryInput,
) -> Result<rebon_api::ModelDiscovery, String> {
    discover_models_for_provider_in(&config_home_dir(), input)
}

fn discover_models_for_provider_in(
    config_dir: &Path,
    input: &ProviderModelDiscoveryInput,
) -> Result<rebon_api::ModelDiscovery, String> {
    let raw_key = input.api_key.trim();
    let api_key = if raw_key == OPENAI_OAUTH_TOKEN_SENTINEL {
        let credentials = read_credentials(config_dir).map_err(|err| err.to_string())?;
        credentials
            .openai_oauth
            .map(|tokens| tokens.access_token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "not signed in to ChatGPT; run /login first".to_string())?
    } else {
        let resolved = resolve_env_value(raw_key);
        if resolved.is_empty() && !raw_key.is_empty() {
            return Err(format!("{raw_key} is not set in the environment"));
        }
        resolved
    };
    let vendor = rebon_api::ProviderVendor::resolve(input.vendor.as_deref(), &input.base_url);
    let request = rebon_api::ModelDiscoveryRequest {
        vendor,
        wire: wire_family_for_format(&input.format),
        base_url: input.base_url.trim().to_string(),
        api_key,
        extra_headers: input
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), resolve_env_value(value)))
            .collect(),
    };
    rebon_api::discover_models_blocking(&request).map_err(|err| err.to_string())
}

/// What `/provider models <name>` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelSync {
    pub info: CustomProviderInfo,
    /// Where the list came from — an endpoint URL, or the documented
    /// catalogue when the vendor has no list endpoint.
    pub source: String,
    /// Ids the entry did not have before.
    pub added: Vec<String>,
    /// Ids whose context window was filled in from the listing.
    pub windows_filled: usize,
    pub skipped_non_chat: usize,
    pub skipped_other_wire: usize,
    /// Set when the endpoint could not be listed and the catalogue was
    /// used instead: the reason.
    pub fallback_reason: Option<String>,
}

/// List a configured provider's models from its endpoint and fold them
/// into its `models`, blocking. Existing ids keep their context windows;
/// new ids arrive with the listed or documented limits; an empty or stale
/// default model is replaced by the preset's default when listed, else the
/// first listed model.
pub fn sync_custom_provider_models(provider_name: &str) -> anyhow::Result<ProviderModelSync> {
    sync_custom_provider_models_in(&config_home_dir(), provider_name)
}

fn sync_custom_provider_models_in(
    config_dir: &Path,
    provider_name: &str,
) -> anyhow::Result<ProviderModelSync> {
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = provider_name.trim().to_lowercase();
    let provider = config
        .custom_providers
        .iter_mut()
        .find(|p| p.name.to_lowercase() == name_lower)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Provider \"{provider_name}\" not found. Use /provider list to see available providers."
            )
        })?;
    let format = provider.format.clone().unwrap_or_else(|| "openai".into());
    let placeholders = base_url_placeholders(&provider.base_url);
    if !placeholders.is_empty() {
        anyhow::bail!(
            "Base URL still has {} to fill in; edit the provider first.",
            placeholders
                .iter()
                .map(|t| format!("{{{t}}}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let input = ProviderModelDiscoveryInput {
        format: format.clone(),
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        vendor: provider.vendor.clone(),
        headers: provider
            .options
            .headers
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect(),
    };
    let vendor = rebon_api::ProviderVendor::resolve(provider.vendor.as_deref(), &provider.base_url);
    let wire = wire_family_for_format(&format);
    let (discovery, fallback_reason) = match discover_models_for_provider_in(config_dir, &input) {
        Ok(discovery) => (discovery, None),
        Err(reason) => {
            let catalogue = rebon_api::catalogue_as_discovery(vendor, wire);
            if catalogue.models.is_empty() {
                anyhow::bail!("{reason}");
            }
            (catalogue, Some(reason))
        }
    };
    if discovery.models.is_empty() {
        anyhow::bail!("{} returned an empty model list", discovery.endpoint);
    }
    let preferred_default =
        provider_preset_for_entry(provider.vendor.as_deref(), &provider.base_url)
            .map(|preset| preset.default_model)
            .filter(|d| !d.is_empty());
    let merge = merge_discovered_models(provider, &discovery.models, preferred_default);
    let info = custom_provider_info_from_provider(provider);
    write_config_roundtrip(config_dir, &config)?;
    Ok(ProviderModelSync {
        info,
        source: discovery.endpoint,
        added: merge.added,
        windows_filled: merge.windows_filled,
        skipped_non_chat: discovery.skipped_non_chat,
        skipped_other_wire: discovery.skipped_other_wire,
        fallback_reason,
    })
}

struct ModelMerge {
    added: Vec<String>,
    windows_filled: usize,
}

/// Fold a listing into an entry's `models`, in place, keeping what the
/// user wrote.
fn merge_discovered_models(
    provider: &mut CustomProvider,
    discovered: &[rebon_api::DiscoveredModel],
    preferred_default: Option<&str>,
) -> ModelMerge {
    let mut added = Vec::new();
    let mut windows_filled = 0;
    for model in discovered {
        if provider.models.contains_id(&model.id) {
            if provider
                .models
                .fill_limits(&model.id, model.context_window, model.max_output_tokens)
            {
                windows_filled += 1;
            }
            continue;
        }
        provider
            .models
            .push_with_limits(&model.id, model.context_window, model.max_output_tokens);
        added.push(model.id.clone());
    }
    if provider.model.is_empty() || !provider.models.contains_id(&provider.model) {
        let next = preferred_default
            .filter(|d| provider.models.contains_id(d))
            .map(str::to_string)
            .or_else(|| discovered.first().map(|m| m.id.clone()));
        if let Some(next) = next {
            provider.model = next;
        }
    }
    ModelMerge {
        added,
        windows_filled,
    }
}

pub fn add_custom_provider_from_preset(
    preset_id: &str,
    api_key: &str,
) -> anyhow::Result<CustomProviderInfo> {
    add_custom_provider_from_preset_in(&config_home_dir(), preset_id, api_key)
}

fn provider_preset_models(preset: &ProviderPreset) -> CustomProviderModels {
    CustomProviderModels::List(
        preset
            .models()
            .into_iter()
            .map(|model| {
                if model.context_window.is_some() || model.max_output_tokens.is_some() {
                    CustomProviderModelEntry::Object(CustomProviderModelObject {
                        id: model.id,
                        details: CustomProviderModelDetails {
                            context_window: model.context_window.map(|n| serde_json::json!(n)),
                            max_output_tokens: model
                                .max_output_tokens
                                .map(|n| serde_json::json!(n)),
                            ..CustomProviderModelDetails::default()
                        },
                    })
                } else {
                    CustomProviderModelEntry::String(model.id)
                }
            })
            .collect(),
    )
}

fn custom_provider_info_from_provider(provider: &CustomProvider) -> CustomProviderInfo {
    CustomProviderInfo {
        name: provider.name.clone(),
        format: provider.format.clone().unwrap_or_else(|| "openai".into()),
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        model: provider.model.clone(),
        models: provider.models.ids(),
    }
}

fn add_custom_provider_from_preset_in(
    config_dir: &Path,
    preset_id: &str,
    api_key: &str,
) -> anyhow::Result<CustomProviderInfo> {
    let Some(preset) = provider_preset_by_id(preset_id) else {
        anyhow::bail!("Unknown provider preset \"{preset_id}\"");
    };
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = preset.id.to_lowercase();
    if config
        .custom_providers
        .iter()
        .any(|p| p.name.to_lowercase() == name_lower)
    {
        anyhow::bail!(
            "Provider \"{}\" already exists. Remove it first with: /provider remove {}",
            preset.id,
            preset.id
        );
    }
    // An endpoint that needs no key (Ollama) still gets a well-formed
    // bearer header; the vendor's docs say any non-empty value is ignored.
    let api_key = match api_key.trim() {
        "" if !preset.api_key_required => preset.id,
        "" => anyhow::bail!(
            "{} needs an API key. Get one at {} and run: /provider add {} <apiKey>",
            preset.display_name,
            preset.key_url,
            preset.id
        ),
        key => key,
    };
    let provider = CustomProvider {
        name: preset.id.to_string(),
        format: Some(preset.format.to_string()),
        // Pinned explicitly so a later edit of the URL (a mirror, a
        // gateway) keeps the vendor's wire dialect.
        vendor: Some(preset.vendor.id().to_string()),
        base_url: preset.base_url.to_string(),
        api_key: api_key.to_string(),
        model: preset.default_model.to_string(),
        models: provider_preset_models(preset),
        model_profiles: ModelProfileMap::default(),
        options: ProviderOptions::default(),
        request_scoped_transient_context: None,
        use_websocket: false,
        thinking_enabled: None,
        thinking_effort: None,
        reasoning_mode: None,
        extra: serde_json::Map::new(),
    };
    let info = custom_provider_info_from_provider(&provider);
    config.custom_providers.push(provider);
    config.active_custom_provider = Some(info.name.clone());
    write_config_roundtrip(config_dir, &config)?;
    Ok(info)
}

/// Resolve `$ENV_VAR` references in a string value.
/// - `$FOO` or `${FOO}` → value of the env var
/// - Literal strings (no `$` prefix) are returned as-is
pub fn resolve_env_value(raw: &str) -> String {
    // Exact match: entire value is a single $VAR or ${VAR}
    if let Some(name) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        return std::env::var(name).unwrap_or_default();
    }
    if let Some(rest) = raw.strip_prefix('$') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return std::env::var(rest).unwrap_or_default();
        }
    }
    raw.to_string()
}

/// Mask an API key for safe display.
/// - `$VAR` references are shown as-is
/// - Short keys (≤8 chars) → `****`
/// - Others → `first4...last4`
pub fn mask_api_key(key: &str) -> String {
    if key.starts_with('$') {
        return key.to_string();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let first: String = chars[..4].iter().collect();
    let last: String = chars[chars.len() - 4..].iter().collect();
    format!("{first}...{last}")
}

pub fn provider_setup_status() -> ProviderSetupStatus {
    provider_setup_status_in(&config_home_dir())
}

pub fn provider_setup_status_in(config_dir: &Path) -> ProviderSetupStatus {
    provider_setup_status_in_with_env(config_dir, |variable| {
        std::env::var(variable)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
}

fn provider_setup_status_in_with_env<F>(
    config_dir: &Path,
    mut environment_available: F,
) -> ProviderSetupStatus
where
    F: FnMut(&str) -> bool,
{
    let mut status = ProviderSetupStatus {
        onboarding_completed: false,
        providers: Vec::new(),
        environment_providers: Vec::new(),
        config_error: None,
    };

    match read_config_roundtrip(config_dir) {
        Ok(config) => {
            status.onboarding_completed = config
                .extra
                .get("hasCompletedOnboarding")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            for provider in config.custom_providers {
                let credential = if provider.api_key == OPENAI_OAUTH_TOKEN_SENTINEL {
                    ProviderSetupCredential::OpenAiOAuth
                } else if let Some(variable) = api_key_environment_variable(&provider.api_key) {
                    ProviderSetupCredential::Environment {
                        available: environment_available(variable),
                        variable: variable.to_string(),
                    }
                } else {
                    ProviderSetupCredential::Stored
                };
                status.providers.push(ProviderSetupProvider {
                    active: config.active_custom_provider.as_deref()
                        == Some(provider.name.as_str()),
                    name: provider.name,
                    model: provider.model,
                    credential,
                });
            }
        }
        Err(err) => status.config_error = Some(err.to_string()),
    }

    for (provider, variable) in [
        ("Anthropic", "ANTHROPIC_API_KEY"),
        ("DeepSeek", "DEEPSEEK_API_KEY"),
        ("OpenAI", "OPENAI_API_KEY"),
    ] {
        if environment_available(variable) {
            status.environment_providers.push(ProviderSetupEnvironment {
                provider: provider.to_string(),
                variable: variable.to_string(),
            });
        }
    }

    status
}

fn api_key_environment_variable(raw: &str) -> Option<&str> {
    if let Some(variable) = raw
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
    {
        return (!variable.is_empty()).then_some(variable);
    }
    let variable = raw.strip_prefix('$')?;
    (!variable.is_empty()
        && variable
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_'))
    .then_some(variable)
}

/// List all configured custom providers from `config.json`.
pub fn list_custom_providers() -> Vec<CustomProviderInfo> {
    list_custom_providers_from(&config_home_dir())
}

pub fn list_custom_providers_from(config_dir: &Path) -> Vec<CustomProviderInfo> {
    // Provider *definitions* come from the store once it exists, so this
    // reads through `read_config_roundtrip` rather than `config.json`
    // directly — the array there is emptied out after migration.
    let config = match read_config_roundtrip(config_dir) {
        Ok(config) => config,
        Err(_) => return Vec::new(),
    };
    config
        .custom_providers
        .into_iter()
        .map(|p| CustomProviderInfo {
            name: p.name,
            format: p.format.unwrap_or_else(|| "openai".into()),
            base_url: p.base_url,
            api_key: p.api_key,
            model: p.model,
            models: p.models.ids(),
        })
        .collect()
}

pub fn builtin_openai_computer_use_available(provider_id: &str) -> bool {
    builtin_openai_computer_use_available_in(&config_home_dir(), provider_id)
}

pub fn builtin_openai_computer_use_available_in(config_dir: &Path, provider_id: &str) -> bool {
    if provider_id != OPENAI_OAUTH_PROVIDER_NAME {
        return false;
    }
    let provider_matches = list_custom_providers_from(config_dir)
        .into_iter()
        .any(|provider| {
            provider.name == OPENAI_OAUTH_PROVIDER_NAME
                && provider.format == "openai-responses"
                && provider.api_key == OPENAI_OAUTH_TOKEN_SENTINEL
                && provider
                    .base_url
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(OPENAI_OAUTH_PROVIDER_BASE_URL.trim_end_matches('/'))
        });
    provider_matches
        && read_credentials(config_dir).is_ok_and(|credentials| credentials.openai_oauth.is_some())
}

/// Where `/model refresh` leaves the table it downloaded.
pub fn model_table_cache_path() -> PathBuf {
    model_table_cache_path_in(&config_home_dir())
}

pub fn model_table_cache_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("cache").join("models.json")
}

/// Install the cached table the first time anything asks for the table.
///
/// Every surface that reads the table (the picker, `/model list`, the fast
/// mode gate) funnels through a caller of this, and the read is one small
/// file, so a lazy install beats threading a startup hook through four
/// entry points.
pub fn ensure_model_table_installed() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        install_cached_model_table();
    });
}

/// Install the cached table over the embedded snapshot, if one is there.
///
/// A cache that will not parse is ignored rather than repaired: the
/// embedded snapshot is always a working answer, and a refresh overwrites
/// the bad file anyway.
pub fn install_cached_model_table() -> Option<usize> {
    install_cached_model_table_in(&config_home_dir())
}

pub fn install_cached_model_table_in(config_dir: &Path) -> Option<usize> {
    let path = model_table_cache_path_in(config_dir);
    let json = std::fs::read_to_string(&path).ok()?;
    match rebon_api::model_table::install_overlay(&json) {
        Ok(count) => Some(count),
        Err(err) => {
            tracing::debug!("rebon-config: ignoring unreadable model table cache {path:?}: {err}");
            None
        }
    }
}

/// Write a downloaded table to the cache and install it.
pub fn save_model_table(json: &str) -> anyhow::Result<usize> {
    save_model_table_in(&config_home_dir(), json)
}

pub fn save_model_table_in(config_dir: &Path, json: &str) -> anyhow::Result<usize> {
    let path = model_table_cache_path_in(config_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Install first: a table that will not parse must not reach the cache,
    // or every later start would read it and throw it away again.
    let count = rebon_api::model_table::install_overlay(json)?;
    std::fs::write(&path, json)?;
    Ok(count)
}

/// One row of a model picker: an id, whether the user put it there, and
/// the one-line description the catalogue can supply for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub id: String,
    /// `true` for a model the user's provider entry lists (or the OAuth
    /// entry's built-in set), `false` for one the catalogue supplied.
    pub configured: bool,
    /// Context window and price, when the catalogue knows them.
    pub detail: Option<String>,
}

/// Model options to offer for a provider in pickers (`/model` dialog
/// and `/model list`): the provider's own `models[]`, plus — for the
/// built-in OpenAI Codex OAuth entry (api_key is the OAuth sentinel)
/// — the built-in model catalogue, so newly shipped models (e.g. the
/// gpt-5.6 family) are selectable without re-running `/login`.
pub fn provider_model_options(provider: &CustomProviderInfo) -> Vec<String> {
    provider_model_choices(provider)
        .into_iter()
        .map(|choice| choice.id)
        .collect()
}

/// The same options, with the catalogue's own models appended and each row
/// annotated.
///
/// The user's `models[]` comes first and keeps its order — that list is a
/// preference, not just a set — then the vendor's catalogue fills in what
/// the entry never mentioned. A user who never edits config still sees
/// everything their key can reach; one who curated a short list still finds
/// it at the top.
pub fn provider_model_choices(provider: &CustomProviderInfo) -> Vec<ModelChoice> {
    ensure_model_table_installed();
    let mut choices: Vec<ModelChoice> = Vec::new();
    let mut push = |id: String, configured: bool| {
        if choices.iter().any(|seen| seen.id.eq_ignore_ascii_case(&id)) {
            return;
        }
        let detail = rebon_api::model_table::model(None, &id)
            .as_ref()
            .and_then(model_detail);
        choices.push(ModelChoice {
            id,
            configured,
            detail,
        });
    };

    for model in &provider.models {
        push(model.clone(), true);
    }
    if provider.api_key == OPENAI_OAUTH_TOKEN_SENTINEL {
        for model in OPENAI_OAUTH_PROVIDER_MODELS {
            push((*model).to_string(), true);
        }
    }
    for model in rebon_api::model_table::chat_models_for_base_url(&provider.base_url) {
        push(model.id.clone(), false);
    }
    choices
}

/// `1.05M ctx · $10 / $50 per M` — the two facts that decide a pick.
fn model_detail(model: &rebon_api::model_table::CatalogModel) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(context) = model.limit.context {
        parts.push(format!("{} ctx", compact_token_count(context)));
    }
    if let Some(cost) = model.cost.as_ref() {
        if let (Some(input), Some(output)) = (cost.input, cost.output) {
            parts.push(format!("${input} / ${output} per M"));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `1050000` reads as `1.05M`; `200000` as `200K`.
fn compact_token_count(tokens: u32) -> String {
    match tokens {
        0..=9_999 => tokens.to_string(),
        10_000..=999_999 => format!("{}K", tokens / 1_000),
        _ => {
            let millions = f64::from(tokens) / 1_000_000.0;
            format!("{millions:.2}M")
        }
    }
}

/// Get the name of the currently active custom provider, or `None`.
pub fn get_active_custom_provider_name() -> Option<String> {
    get_active_custom_provider_name_from(&config_home_dir())
}

pub fn get_active_custom_provider_name_from(config_dir: &Path) -> Option<String> {
    let path = config_json_path(config_dir);
    let config = read_config_json(&path).ok()??;
    config.active_custom_provider
}

/// Add a new custom provider to `config.json`. Returns an error if
/// a provider with the same name (case-insensitive) already exists.
pub fn add_custom_provider(
    name: &str,
    format: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    add_custom_provider_in(&config_home_dir(), name, format, base_url, api_key, model)
}

fn add_custom_provider_in(
    config_dir: &Path,
    name: &str,
    format: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = name.to_lowercase();
    if config
        .custom_providers
        .iter()
        .any(|p| p.name.to_lowercase() == name_lower)
    {
        anyhow::bail!(
            "Provider \"{name}\" already exists. Remove it first with: /provider remove {name}"
        );
    }
    let seeded_models = if model.is_empty() {
        CustomProviderModels::default()
    } else {
        CustomProviderModels::List(vec![CustomProviderModelEntry::String(model.to_string())])
    };
    let provider = CustomProvider {
        name: name.to_string(),
        format: Some(format.to_string()),
        vendor: None,
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        models: seeded_models,
        model_profiles: ModelProfileMap::default(),
        options: ProviderOptions::default(),
        request_scoped_transient_context: None,
        use_websocket: false,
        thinking_enabled: None,
        thinking_effort: None,
        reasoning_mode: None,
        extra: serde_json::Map::new(),
    };
    let info = custom_provider_info_from_provider(&provider);
    config.custom_providers.push(provider);
    config.active_custom_provider = Some(info.name.clone());
    write_config_roundtrip(config_dir, &config)?;
    Ok(info)
}

/// Update an existing custom provider. The lookup uses the provider's current
/// name case-insensitively; the provider may be renamed as long as the
/// new name doesn't collide with a different provider.
pub fn update_custom_provider(
    original_name: &str,
    name: &str,
    format: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    update_custom_provider_in(
        &config_home_dir(),
        original_name,
        name,
        format,
        base_url,
        api_key,
        model,
    )
}

fn update_custom_provider_in(
    config_dir: &Path,
    original_name: &str,
    name: &str,
    format: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    let name = name.trim();
    let format = format.trim();
    let base_url = base_url.trim();
    let api_key = api_key.trim();
    let model = model.trim();
    if name.is_empty() {
        anyhow::bail!("Provider name cannot be empty.");
    }
    if !VALID_PROVIDER_FORMATS.contains(&format) {
        anyhow::bail!(
            "Invalid provider format \"{format}\". Use one of: {}",
            VALID_PROVIDER_FORMATS.join(", ")
        );
    }
    if base_url.is_empty() {
        anyhow::bail!("Base URL cannot be empty.");
    }
    if api_key.is_empty() {
        anyhow::bail!("API key cannot be empty.");
    }
    if model.is_empty() {
        anyhow::bail!("Model name cannot be empty.");
    }

    let mut config = read_config_roundtrip(config_dir)?;
    let original_lower = original_name.to_lowercase();
    let Some(provider_idx) = config
        .custom_providers
        .iter()
        .position(|p| p.name.to_lowercase() == original_lower)
    else {
        anyhow::bail!("Provider \"{original_name}\" not found.");
    };
    let new_lower = name.to_lowercase();
    if config
        .custom_providers
        .iter()
        .enumerate()
        .any(|(idx, p)| idx != provider_idx && p.name.to_lowercase() == new_lower)
    {
        anyhow::bail!("Provider \"{name}\" already exists.");
    }

    let provider = &mut config.custom_providers[provider_idx];
    provider.name = name.to_string();
    provider.format = Some(format.to_string());
    provider.base_url = base_url.to_string();
    provider.api_key = api_key.to_string();
    provider.model = model.to_string();
    provider.models.ensure_id(model);
    let info = custom_provider_info_from_provider(provider);
    config.active_custom_provider = Some(info.name.clone());
    write_config_roundtrip(config_dir, &config)?;
    Ok(info)
}

/// Upsert the synthetic "openai" Codex-OAuth provider entry. Used by
/// the OAuth login flow after a successful token exchange: the
/// tokens themselves live in `.credentials.json::openaiOAuth`, and
/// this entry in `customProviders[]` is what the provider resolver
/// reads to pick up those tokens via the
/// [`OPENAI_OAUTH_TOKEN_SENTINEL`] indirection.
///
/// Idempotent: if an entry with the same name already exists it is
/// rewritten in place (format / base_url / api_key / model all
/// overwritten to the canonical values), and on a successful upsert
/// the provider becomes the active one. Unrelated providers are
/// preserved byte-for-byte.
pub fn upsert_openai_oauth_provider_in(config_dir: &Path, models: &[String]) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    let name = OPENAI_OAUTH_PROVIDER_NAME;
    let primary_model = models
        .first()
        .cloned()
        .unwrap_or_else(|| OPENAI_OAUTH_PROVIDER_MODEL.to_string());
    let models_list: CustomProviderModels = if models.is_empty() {
        CustomProviderModels::List(vec![CustomProviderModelEntry::String(
            OPENAI_OAUTH_PROVIDER_MODEL.to_string(),
        )])
    } else {
        CustomProviderModels::List(
            models
                .iter()
                .cloned()
                .map(CustomProviderModelEntry::String)
                .collect(),
        )
    };

    let name_lower = name.to_lowercase();
    let existing = config
        .custom_providers
        .iter_mut()
        .find(|p| p.name.to_lowercase() == name_lower);
    match existing {
        Some(entry) => {
            entry.format = Some("openai-responses".to_string());
            entry.base_url = OPENAI_OAUTH_PROVIDER_BASE_URL.to_string();
            entry.api_key = OPENAI_OAUTH_TOKEN_SENTINEL.to_string();
            entry.model = primary_model;
            // Merge models: keep any the user added manually, then
            // ensure the canonical default is present.
            for m in models_list.ids() {
                if !entry.models.contains_id(&m) {
                    entry.models.push_id(m);
                }
            }
            if entry.models.is_empty() {
                entry
                    .models
                    .push_id(OPENAI_OAUTH_PROVIDER_MODEL.to_string());
            }
        }
        None => {
            config.custom_providers.push(CustomProvider {
                name: name.to_string(),
                format: Some("openai-responses".to_string()),
                vendor: None,
                base_url: OPENAI_OAUTH_PROVIDER_BASE_URL.to_string(),
                api_key: OPENAI_OAUTH_TOKEN_SENTINEL.to_string(),
                model: primary_model,
                models: models_list,
                model_profiles: ModelProfileMap::default(),
                options: ProviderOptions::default(),
                request_scoped_transient_context: None,
                use_websocket: false,
                thinking_enabled: None,
                thinking_effort: None,
                reasoning_mode: None,
                extra: serde_json::Map::new(),
            });
        }
    }
    config.active_custom_provider = Some(name.to_string());
    write_config_roundtrip(config_dir, &config)
}

/// Append a model to an existing provider's `models` list and switch
/// the provider's active `model` to that value. Returns the updated
/// `CustomProviderInfo`.
///
/// Errors if:
/// - the provider name does not exist (case-insensitive)
/// - the model string is empty
/// - the model already exists in the provider's `models` list
///   (case-sensitive; two variants of the same name are valid —
///   `gpt-5` vs `gpt-5o` are distinct models).
pub fn add_custom_provider_model(
    provider_name: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    add_custom_provider_model_in(&config_home_dir(), provider_name, model)
}

fn add_custom_provider_model_in(
    config_dir: &Path,
    provider_name: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    let model = model.trim();
    if model.is_empty() {
        anyhow::bail!("Model name cannot be empty.");
    }
    if model.eq_ignore_ascii_case("default") {
        anyhow::bail!("\"default\" is not a model id. Pick a concrete model.");
    }
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = provider_name.to_lowercase();
    let provider = config
        .custom_providers
        .iter_mut()
        .find(|p| p.name.to_lowercase() == name_lower)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Provider \"{provider_name}\" not found. Use /provider list to see available providers."
            )
        })?;
    if provider.models.contains_id(model) {
        anyhow::bail!("Model \"{model}\" already exists for provider \"{provider_name}\".");
    }
    provider.models.push_id(model.to_string());
    provider.model = model.to_string();
    let snapshot = provider.clone();
    write_config_roundtrip(config_dir, &config)?;
    Ok(CustomProviderInfo {
        name: snapshot.name,
        format: snapshot.format.unwrap_or_else(|| "openai".into()),
        base_url: snapshot.base_url,
        api_key: snapshot.api_key,
        model: snapshot.model,
        models: snapshot.models.ids(),
    })
}

/// Set the active model for a configured provider. Adds the model to
/// the provider's models list when it is not already present.
pub fn set_custom_provider_model(
    provider_name: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    set_custom_provider_model_in(&config_home_dir(), provider_name, model)
}

fn set_custom_provider_model_in(
    config_dir: &Path,
    provider_name: &str,
    model: &str,
) -> anyhow::Result<CustomProviderInfo> {
    let model = model.trim();
    if model.is_empty() {
        anyhow::bail!("Model name cannot be empty.");
    }
    // "default" is a UI sentinel ("use the provider's configured model"),
    // never a real model id — storing it would poison the provider's model
    // list and send a literal "default" model on the wire.
    if model.eq_ignore_ascii_case("default") {
        anyhow::bail!("\"default\" is not a model id. Pick a concrete model, e.g. /model <model>.");
    }
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = provider_name.to_lowercase();
    let provider = config
        .custom_providers
        .iter_mut()
        .find(|p| p.name.to_lowercase() == name_lower)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Provider \"{provider_name}\" not found. Use /provider list to see available providers."
            )
        })?;
    provider.models.ensure_id(model);
    provider.model = model.to_string();
    let snapshot = provider.clone();
    write_config_roundtrip(config_dir, &config)?;
    Ok(CustomProviderInfo {
        name: snapshot.name,
        format: snapshot.format.unwrap_or_else(|| "openai".into()),
        base_url: snapshot.base_url,
        api_key: snapshot.api_key,
        model: snapshot.model,
        models: snapshot.models.ids(),
    })
}

/// Snapshot of one provider's `modelProfiles` table, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfileEntry {
    pub role: String,
    /// `None` means the role is undeclared and follows the session's model.
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// The profile roles the coordinator recognises, in display order.
///
/// Kept here rather than in either front end so the TUI listing and the
/// settings window cannot drift apart on which roles exist.
pub const PROVIDER_PROFILE_ROLES: &[&str] = &[
    rebon_types::MODEL_PROFILE_GENERAL,
    rebon_types::MODEL_PROFILE_SMALL,
    rebon_types::MODEL_PROFILE_FAST,
    rebon_types::MODEL_PROFILE_EXPLORE,
    rebon_types::MODEL_PROFILE_LIBRARIAN,
    rebon_types::MODEL_PROFILE_BUILDER,
    rebon_types::MODEL_PROFILE_REVIEWER,
    rebon_types::MODEL_PROFILE_REASONING,
];

/// Read one provider's profile table, one row per known role.
///
/// A row with `model: None` is not a gap to be filled in from somewhere
/// else — it is the normal case, and it means that role runs whatever model
/// the session is running.
pub fn custom_provider_profiles(provider_name: &str) -> anyhow::Result<Vec<ProviderProfileEntry>> {
    custom_provider_profiles_in(&config_home_dir(), provider_name)
}

pub fn custom_provider_profiles_in(
    config_dir: &Path,
    provider_name: &str,
) -> anyhow::Result<Vec<ProviderProfileEntry>> {
    let config = read_config_roundtrip(config_dir)?;
    let name_lower = provider_name.to_lowercase();
    let provider = config
        .custom_providers
        .iter()
        .find(|p| p.name.to_lowercase() == name_lower)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Provider \"{provider_name}\" not found. Use /provider list to see available providers."
            )
        })?;
    Ok(PROVIDER_PROFILE_ROLES
        .iter()
        .map(|role| ProviderProfileEntry {
            role: (*role).to_string(),
            model: provider.model_profiles.get(role).map(str::to_string),
            reasoning_effort: provider
                .model_profiles
                .get_reasoning_effort(role)
                .map(str::to_string),
        })
        .collect())
}

/// Pin one profile role to a model, or clear it so the role follows the
/// session's model.
///
/// `model: None` removes the row. There is deliberately no "inherit" value to
/// store: an undeclared role already resolves to the running model, and a
/// stored sentinel would be a second way to say the same thing — the kind of
/// duplicate that let a declared role quietly outrank the user's own choice.
pub fn set_custom_provider_profile(
    provider_name: &str,
    role: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> anyhow::Result<Vec<ProviderProfileEntry>> {
    set_custom_provider_profile_in(
        &config_home_dir(),
        provider_name,
        role,
        model,
        reasoning_effort,
    )
}

pub fn set_custom_provider_profile_in(
    config_dir: &Path,
    provider_name: &str,
    role: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> anyhow::Result<Vec<ProviderProfileEntry>> {
    let role_normalized = rebon_types::ModelProfileMap::normalize_profile_name(role);
    if !PROVIDER_PROFILE_ROLES.contains(&role_normalized.as_str()) {
        anyhow::bail!(
            "Unknown profile role \"{role}\". Known roles: {}.",
            PROVIDER_PROFILE_ROLES.join(", ")
        );
    }
    let model = model.map(str::trim).filter(|model| !model.is_empty());
    if let Some(model) = model {
        // Same sentinel guard as `/model`: "default" is a UI word, not a
        // model id, and storing it would put a literal "default" on the wire.
        if model.eq_ignore_ascii_case("default") {
            anyhow::bail!(
                "\"default\" is not a model id. Pass a concrete model, or `follow` to clear the role."
            );
        }
    }
    let effort = reasoning_effort
        .map(str::trim)
        .filter(|effort| !effort.is_empty());
    if let Some(effort) = effort {
        if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
            anyhow::bail!(
                "Unknown reasoning effort \"{effort}\". Use low, medium, high, xhigh or max."
            );
        }
        if model.is_none() {
            anyhow::bail!("A reasoning effort needs a model — pass the model first.");
        }
    }

    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = provider_name.to_lowercase();
    let provider = config
        .custom_providers
        .iter_mut()
        .find(|p| p.name.to_lowercase() == name_lower)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Provider \"{provider_name}\" not found. Use /provider list to see available providers."
            )
        })?;
    match model {
        Some(model) => {
            provider
                .model_profiles
                .insert_with_reasoning_effort(&role_normalized, model, effort);
        }
        None => {
            provider.model_profiles.remove(&role_normalized);
        }
    }
    write_config_roundtrip(config_dir, &config)?;
    custom_provider_profiles_in(config_dir, provider_name)
}

/// Remove a custom provider by name (case-insensitive). If the
/// provider is currently active, clears the active selection.
/// Returns `Ok(true)` if the removed provider was active.
pub fn remove_custom_provider(name: &str) -> anyhow::Result<bool> {
    remove_custom_provider_in(&config_home_dir(), name)
}

fn remove_custom_provider_in(config_dir: &Path, name: &str) -> anyhow::Result<bool> {
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = name.to_lowercase();
    let before_len = config.custom_providers.len();
    config
        .custom_providers
        .retain(|p| p.name.to_lowercase() != name_lower);
    if config.custom_providers.len() == before_len {
        anyhow::bail!("Provider \"{name}\" not found.");
    }
    let was_active = config
        .active_custom_provider
        .as_ref()
        .map(|a| a.to_lowercase() == name_lower)
        .unwrap_or(false);
    if was_active {
        config.active_custom_provider = None;
    }
    write_config_roundtrip(config_dir, &config)?;
    Ok(was_active)
}

/// Set the active custom provider by name. Returns error if the
/// name doesn't match any configured provider.
pub fn set_active_custom_provider(name: &str) -> anyhow::Result<CustomProviderInfo> {
    set_active_custom_provider_in(&config_home_dir(), name)
}

fn set_active_custom_provider_in(
    config_dir: &Path,
    name: &str,
) -> anyhow::Result<CustomProviderInfo> {
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = name.to_lowercase();
    let found = config
        .custom_providers
        .iter()
        .find(|p| p.name.to_lowercase() == name_lower)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Provider \"{name}\" not found. Use /provider list to see available providers."
            )
        })?;
    let info = CustomProviderInfo {
        name: found.name.clone(),
        format: found.format.clone().unwrap_or_else(|| "openai".into()),
        base_url: found.base_url.clone(),
        api_key: found.api_key.clone(),
        model: found.model.clone(),
        models: found.models.ids(),
    };
    config.active_custom_provider = Some(found.name.clone());
    write_config_roundtrip(config_dir, &config)?;
    Ok(info)
}

/// Clear the active custom provider (switch to default).
pub fn clear_active_custom_provider() -> anyhow::Result<()> {
    clear_active_custom_provider_in(&config_home_dir())
}

fn clear_active_custom_provider_in(config_dir: &Path) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    config.active_custom_provider = None;
    write_config_roundtrip(config_dir, &config)
}

// ---------------------------------------------------------------------------
// Round-trip config read/write helpers
// ---------------------------------------------------------------------------

/// Read `config.json` and overlay the provider store on top of it.
///
/// This is the read side every provider API goes through, which is why the
/// store could take over storage without any of them changing: once
/// `providers/` exists it is the authority for `customProviders`, and
/// `config.json`'s array is a read-only fallback for one release.
/// [`read_config_roundtrip_raw`] is the same read *without* the overlay — only
/// the migration itself wants that.
fn read_config_roundtrip(config_dir: &Path) -> anyhow::Result<RebonConfigRoundTrip> {
    let mut config = read_config_roundtrip_raw(config_dir)?;
    provider_store::merge_for_read(config_dir, &mut config);
    migrate_config_to_current_schema(config_dir, &mut config);
    Ok(config)
}

/// Bring a file written by an older Rebon up to [`CONFIG_SCHEMA_VERSION`],
/// once, and stamp it so the next read does not look again.
///
/// This is the only place that knows what an older `config.json` looked like.
/// Every other reader and writer in this crate sees the current shape and can
/// be written as if the old one never existed — which is the point: a
/// compatibility branch left on a write path has to be got right in every
/// function that ever touches the field, forever.
///
/// A write that fails (a read-only config home, a full disk) is logged and
/// dropped: the caller still gets the migrated value, and the next read
/// migrates again. A config home with no `config.json` at all is left alone —
/// there is nothing to migrate and creating the file to stamp it would put a
/// config in front of a user who has none.
fn migrate_config_to_current_schema(config_dir: &Path, config: &mut RebonConfigRoundTrip) {
    let stamped = config
        .extra
        .get(CONFIG_SCHEMA_VERSION_KEY)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if stamped >= CONFIG_SCHEMA_VERSION {
        return;
    }

    // Schema 1: a provider used to record its one model in `model` alone.
    // `models` arrived later, and every writer since has had to remember to
    // seed the list from the scalar before appending to it.
    for provider in &mut config.custom_providers {
        if provider.models.is_empty() && !provider.model.is_empty() {
            let seed = provider.model.clone();
            provider.models.push_id(seed);
        }
    }

    config.extra.insert(
        CONFIG_SCHEMA_VERSION_KEY.to_string(),
        serde_json::json!(CONFIG_SCHEMA_VERSION),
    );

    if !config_json_path(config_dir).exists() {
        return;
    }
    if let Err(err) = write_config_roundtrip(config_dir, config) {
        tracing::warn!(
            error = %err,
            "failed to write the migrated config; it will be migrated again on the next read"
        );
    }
}

fn read_config_roundtrip_raw(config_dir: &Path) -> anyhow::Result<RebonConfigRoundTrip> {
    let path = config_json_path(config_dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RebonConfigRoundTrip {
                active_custom_provider: None,
                custom_providers: Vec::new(),
                extra: serde_json::Map::new(),
            });
        }
        Err(err) => {
            return Err(
                anyhow::Error::from(err).context(format!("failed to read config at {path:?}"))
            );
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|err| anyhow::anyhow!("failed to parse config at {path:?}: {err}"))
}

/// Write `config.json`, sending providers to the store when it is in use.
///
/// With the store active, `config.json` keeps `activeCustomProvider` and every
/// unrelated key but stops carrying provider definitions — the array is
/// written empty so the two copies cannot drift. Before migration this is the
/// old single-file write, unchanged.
fn write_config_roundtrip(config_dir: &Path, config: &RebonConfigRoundTrip) -> anyhow::Result<()> {
    if !provider_store::is_active(config_dir) {
        return write_config_roundtrip_raw(config_dir, config);
    }
    provider_store::save_all(config_dir, &config.custom_providers)?;
    let mut without_providers = config.clone();
    without_providers.custom_providers = Vec::new();
    write_config_roundtrip_raw(config_dir, &without_providers)
}

fn write_config_roundtrip_raw(
    config_dir: &Path,
    config: &RebonConfigRoundTrip,
) -> anyhow::Result<()> {
    let target = config_json_path(config_dir);
    if let Err(err) = std::fs::create_dir_all(config_dir) {
        return Err(
            anyhow::Error::from(err).context(format!("failed to create config dir {config_dir:?}"))
        );
    }
    let serialized = serde_json::to_vec_pretty(config)
        .map_err(|err| anyhow::anyhow!("failed to serialize config: {err}"))?;
    rebon_session::write_private_file_atomically(&target, &serialized).map_err(|err| {
        anyhow::Error::from(err).context(format!("failed to write config at {target:?}"))
    })?;
    notify_config_changed(ConfigFileKind::Config, &target);
    Ok(())
}

// ---------------------------------------------------------------------------
// Onboarding completion flag
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Per-directory trust
// ---------------------------------------------------------------------------

/// Check if the given directory (or any ancestor) is trusted.
/// Reads `projects.<normalized_path>.hasTrustDialogAccepted` from
/// `config.json`, walking up from `cwd` to the filesystem root.
///
/// The user's home directory (and ancestors such as `/` or `C:\`)
/// are never considered persistently trusted — even if a flag exists
/// in `config.json`, those "too broad" paths require re-confirmation
/// every session.
pub fn is_directory_trusted(cwd: &Path) -> bool {
    is_directory_trusted_in(&config_home_dir(), cwd)
}

pub fn is_directory_trusted_in(config_dir: &Path, cwd: &Path) -> bool {
    let config = match read_config_roundtrip(config_dir) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let projects = match config.extra.get("projects") {
        Some(serde_json::Value::Object(m)) => m,
        _ => return false,
    };
    // Walk up from cwd, checking each ancestor.
    let mut dir = cwd.to_path_buf();
    loop {
        let key = normalize_trust_key(&dir);
        if let Some(proj) = projects.get(&key) {
            if proj
                .get("hasTrustDialogAccepted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                // The home directory (and its ancestors) are too broad to
                // be persistently trusted. Since every further ancestor is
                // even broader, we can short-circuit here.
                if is_home_dir_or_above(&dir) {
                    return false;
                }
                return true;
            }
        }
        if !dir.pop() {
            break;
        }
    }
    false
}

/// Persist directory trust by setting
/// `projects.<normalized_path>.hasTrustDialogAccepted = true` in
/// `config.json`. Uses the git root if available, otherwise `cwd`.
///
/// The user's home directory (and ancestors) are never persisted —
/// trust for those paths is session-only, so the dialog will appear
/// again on the next launch.
pub fn save_directory_trust(cwd: &Path) {
    let trust_root = resolve_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    // Never persist trust for the home directory or broader paths.
    if is_home_dir_or_above(&trust_root) {
        tracing::debug!(
            path = %trust_root.display(),
            "skipping trust persistence for home-or-above directory",
        );
        return;
    }
    let config_dir = config_home_dir();
    let mut config = match read_config_roundtrip(&config_dir) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "failed to read config for trust save");
            return;
        }
    };
    let projects = config
        .extra
        .entry("projects")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(map) = projects {
        let key = normalize_trust_key(&trust_root);
        let entry = map
            .entry(key)
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let serde_json::Value::Object(proj) = entry {
            proj.insert(
                "hasTrustDialogAccepted".to_string(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Err(err) = write_config_roundtrip(&config_dir, &config) {
        tracing::warn!(error = %err, "failed to write trust to config");
    }
}

/// Returns `true` when `path` is the user's home directory or an
/// ancestor of it (e.g. `/`, `C:\Users`). These "too broad"
/// directories must never be persistently trusted.
fn is_home_dir_or_above(path: &Path) -> bool {
    let Some(home) = home_dir() else {
        return false;
    };
    let home_key = normalize_trust_key(&home);
    let path_key = normalize_trust_key(path);
    // Component-aware prefix check: if home starts with path then
    // path is equal to home or is an ancestor of home.
    Path::new(&home_key).starts_with(&path_key)
}

/// Normalize a path to a stable map key: the one directory identity every
/// other keyed store in the repo uses.
///
/// This was its own copy -- forward slashes, lowercase on Windows -- which is
/// most of what `cwd_identity` does and not all of it. What it was missing is
/// the extended-length spelling: `\\?\F:\dev\x` keyed separately from
/// `F:\dev\x`, so a directory the user had already trusted asked again the
/// moment something handed it a canonicalized path, and the trust had to be
/// written twice to stick.
///
/// This crate already depends on `rebon-session` so that it and the session
/// paths "cannot disagree about where the data lives". Two spellings of one
/// directory is that disagreement, in the one place it was still possible.
fn normalize_trust_key(path: &Path) -> String {
    rebon_session::cwd_identity(&path.to_string_lossy())
}

/// Resolve the git root for the given directory, if any.
fn resolve_git_root(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

/// Read the user's saved theme from `config.json`. Returns `None`
/// when the key is absent or the config doesn't exist yet.
pub fn saved_theme() -> Option<String> {
    let config_dir = config_home_dir();
    let config = read_config_roundtrip(&config_dir).ok()?;
    config
        .extra
        .get("theme")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Check if the user has completed onboarding. Reads
/// `hasCompletedOnboarding` from `config.json`. Returns `false` when
/// the key is absent or the config doesn't exist yet.
pub fn has_completed_onboarding() -> bool {
    has_completed_onboarding_in(&config_home_dir())
}

pub fn has_completed_onboarding_in(config_dir: &Path) -> bool {
    let config = match read_config_roundtrip(config_dir) {
        Ok(config) => config,
        Err(_) => return false,
    };
    config
        .extra
        .get("hasCompletedOnboarding")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Save the user's theme selection to `config.json`. Preserves all
/// existing config keys.
///
/// Persisted as the top-level `theme` string in `config.json`.
pub fn save_theme(theme_id: &str) {
    let config_dir = config_home_dir();
    let mut config = match read_config_roundtrip(&config_dir) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "failed to read config for theme save");
            return;
        }
    };
    config.extra.insert(
        "theme".to_string(),
        serde_json::Value::String(theme_id.to_string()),
    );
    if let Err(err) = write_config_roundtrip(&config_dir, &config) {
        tracing::warn!(error = %err, "failed to write theme to config");
    }
}

pub fn saved_fast_mode_enabled() -> bool {
    saved_fast_mode_enabled_in_dir(&config_home_dir())
}

pub fn saved_fast_mode_enabled_in_dir(config_dir: &Path) -> bool {
    let config = match read_config_roundtrip(config_dir) {
        Ok(config) => config,
        Err(_) => return false,
    };
    let service_tier_fast = config
        .extra
        .get(SERVICE_TIER_CONFIG_KEY)
        .and_then(|value| value.as_str())
        .map(|value| value.eq_ignore_ascii_case("fast") || value.eq_ignore_ascii_case("priority"))
        .unwrap_or(false);
    let feature_fast = config
        .extra
        .get(FEATURES_CONFIG_KEY)
        .and_then(|value| value.as_object())
        .and_then(|features| features.get(FAST_MODE_FEATURE_KEY))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    service_tier_fast || feature_fast
}

pub fn save_fast_mode_enabled(enabled: bool) -> anyhow::Result<()> {
    save_fast_mode_enabled_in_dir(&config_home_dir(), enabled)
}

pub fn save_ui_mode(mode: &str) -> anyhow::Result<()> {
    save_ui_mode_in_dir(&config_home_dir(), mode)
}

pub fn save_math_rendering_mode(mode: MathRenderingMode) -> anyhow::Result<()> {
    save_math_rendering_mode_in_dir(&config_home_dir(), mode)
}

pub fn save_math_rendering_mode_in_file(
    target: &Path,
    mode: MathRenderingMode,
) -> anyhow::Result<()> {
    let mut settings = read_settings_json_object(target)?;
    settings.insert(
        MATH_RENDERING_CONFIG_KEY.to_string(),
        serde_json::Value::String(mode.to_string()),
    );
    write_settings_json_object(target, &settings)
}

pub fn saved_language() -> Option<String> {
    saved_language_in_dir(&config_home_dir())
}

pub fn saved_language_in_dir(config_dir: &Path) -> Option<String> {
    let target = config_dir.join("settings.json");
    let settings = read_settings_json_object(&target).ok()?;
    settings
        .get(LANGUAGE_CONFIG_KEY)
        .or_else(|| {
            settings
                .get(APP_APPEARANCE_CONFIG_KEY)
                .and_then(serde_json::Value::as_object)
                .and_then(|appearance| appearance.get(LANGUAGE_CONFIG_KEY))
        })
        .and_then(serde_json::Value::as_str)
        .and_then(response_language_name)
        .map(ToString::to_string)
}

fn response_language_name(language: &str) -> Option<&'static str> {
    match language.trim().to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "zh-hans" | "chinese" | "简体中文" => Some("Chinese"),
        "en" | "en-us" | "english" => Some("English"),
        "ja" | "ja-jp" | "japanese" | "日本語" => Some("Japanese"),
        _ => None,
    }
}

/// The locales [`saved_language`] recognises, as the settings panel offers
/// them. `auto` is the absence of the key, not a value written to it.
pub const LANGUAGE_LOCALES: [&str; 3] = ["en", "zh-CN", "ja"];

/// The locale the language key holds, as written rather than as a prompt name.
///
/// [`saved_language`] answers what the model is told ("Chinese"); a settings
/// row has to show and cycle what the file holds ("zh-CN"). `None` means the
/// key is absent or holds something unrecognised, which both read as "follow
/// the model's own default".
pub fn saved_language_locale() -> Option<String> {
    saved_language_locale_in_dir(&config_home_dir())
}

pub fn saved_language_locale_in_dir(config_dir: &Path) -> Option<String> {
    let target = config_dir.join("settings.json");
    let settings = read_settings_json_object(&target).ok()?;
    let raw = settings
        .get(LANGUAGE_CONFIG_KEY)
        .or_else(|| {
            settings
                .get(APP_APPEARANCE_CONFIG_KEY)
                .and_then(serde_json::Value::as_object)
                .and_then(|appearance| appearance.get(LANGUAGE_CONFIG_KEY))
        })
        .and_then(serde_json::Value::as_str)?;
    // Only a locale the reader recognises is reported. A row that showed a
    // value `saved_language` will not act on would say the setting is in
    // force when the model is never told about it.
    let canonical = LANGUAGE_LOCALES
        .iter()
        .find(|locale| locale.eq_ignore_ascii_case(raw.trim()))?;
    Some((*canonical).to_string())
}

/// Write the response-language preference, or clear it.
///
/// Always the top-level key: `appAppearance.language` is read for the app's
/// sake but never written here, so one file cannot end up with two answers.
pub fn save_language(locale: Option<&str>) -> anyhow::Result<()> {
    save_language_in_dir(&config_home_dir(), locale)
}

pub fn save_language_in_dir(config_dir: &Path, locale: Option<&str>) -> anyhow::Result<()> {
    let target = config_dir.join("settings.json");
    let mut settings = read_settings_json_object(&target).unwrap_or_default();
    match locale {
        Some(locale) => {
            let canonical = LANGUAGE_LOCALES
                .iter()
                .find(|known| known.eq_ignore_ascii_case(locale.trim()))
                .ok_or_else(|| anyhow::anyhow!("unknown language `{locale}`"))?;
            settings.insert(
                LANGUAGE_CONFIG_KEY.to_string(),
                serde_json::Value::String((*canonical).to_string()),
            );
        }
        None => {
            settings.remove(LANGUAGE_CONFIG_KEY);
        }
    }
    write_settings_json_object(&target, &settings)
}

#[cfg(test)]
mod saved_language_tests {
    use super::*;

    #[test]
    fn absent_language_does_not_create_a_prompt_preference() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        assert_eq!(saved_language_in_dir(dir.path()), None);
    }

    /// The settings row writes a locale and reads the same one back, and the
    /// prompt side agrees about what was written. Two readers of one key that
    /// disagree is the failure this pins.
    #[test]
    fn a_written_locale_reads_back_as_itself_and_as_a_prompt_name() {
        let dir = tempfile::tempdir().unwrap();
        for (locale, prompt_name) in [("zh-CN", "Chinese"), ("en", "English"), ("ja", "Japanese")] {
            save_language_in_dir(dir.path(), Some(locale)).expect("writes");
            assert_eq!(
                saved_language_locale_in_dir(dir.path()).as_deref(),
                Some(locale)
            );
            assert_eq!(
                saved_language_in_dir(dir.path()).as_deref(),
                Some(prompt_name)
            );
        }
    }

    #[test]
    fn a_locale_is_canonicalised_on_the_way_in_and_an_unknown_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        save_language_in_dir(dir.path(), Some("ZH-cn")).expect("case does not matter");
        assert_eq!(
            saved_language_locale_in_dir(dir.path()).as_deref(),
            Some("zh-CN")
        );
        save_language_in_dir(dir.path(), Some("klingon")).expect_err("unknown locale refused");
        assert_eq!(
            saved_language_locale_in_dir(dir.path()).as_deref(),
            Some("zh-CN"),
            "a refused write must not have changed the file"
        );
    }

    #[test]
    fn clearing_the_language_removes_the_key_rather_than_writing_a_word_for_absent() {
        let dir = tempfile::tempdir().unwrap();
        save_language_in_dir(dir.path(), Some("ja")).expect("writes");
        save_language_in_dir(dir.path(), None).expect("clears");
        assert_eq!(saved_language_locale_in_dir(dir.path()), None);
        assert_eq!(saved_language_in_dir(dir.path()), None);
        let raw = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        assert!(
            !raw.contains(LANGUAGE_CONFIG_KEY),
            "the key should be gone, not set to a placeholder: {raw}"
        );
    }

    /// The locale reader only reports what the prompt reader will act on.
    #[test]
    fn an_unrecognised_locale_in_the_file_reads_as_absent_on_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            format!(r#"{{"{LANGUAGE_CONFIG_KEY}":"fr-FR"}}"#),
        )
        .unwrap();
        assert_eq!(saved_language_locale_in_dir(dir.path()), None);
        assert_eq!(saved_language_in_dir(dir.path()), None);
    }

    #[test]
    fn shared_language_key_maps_supported_locales_to_prompt_names() {
        let dir = tempfile::tempdir().unwrap();
        for (locale, expected) in [("zh-CN", "Chinese"), ("en", "English"), ("ja", "Japanese")] {
            std::fs::write(
                dir.path().join("settings.json"),
                format!(r#"{{"{LANGUAGE_CONFIG_KEY}":"{locale}"}}"#),
            )
            .unwrap();
            assert_eq!(saved_language_in_dir(dir.path()).as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_language_nested_under_app_appearance_is_read_but_invalid_values_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"appAppearance":{"language":"ja-JP"}}"#,
        )
        .unwrap();
        assert_eq!(
            saved_language_in_dir(dir.path()).as_deref(),
            Some("Japanese")
        );

        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"language":"not-a-language"}"#,
        )
        .unwrap();
        assert_eq!(saved_language_in_dir(dir.path()), None);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPromptOverrides {
    pub normal: Option<String>,
    pub minimal: Option<String>,
    pub chat: Option<String>,
}

pub fn saved_system_prompt_overrides() -> SystemPromptOverrides {
    saved_system_prompt_overrides_in_dir(&config_home_dir())
}

pub fn saved_system_prompt_overrides_in_dir(config_dir: &Path) -> SystemPromptOverrides {
    let settings = read_settings_json_object(&config_dir.join("settings.json")).unwrap_or_default();
    SystemPromptOverrides {
        normal: read_prompt_override(&settings, NORMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY),
        minimal: read_prompt_override(&settings, MINIMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY),
        chat: read_prompt_override(&settings, CHAT_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY),
    }
}

/// Which capability modes the new-chat switch offers, in display order.
///
/// Defaults to conversation plus the full agent. Minimal is a named, fixed
/// tool surface that exists for provider anchoring rather than for everyday
/// use, so it is opt-in: offering three tabs by default would ask every user
/// to decide between two things that look alike and differ in a detail most
/// sessions never touch.
pub fn saved_enabled_capability_modes() -> Vec<rebon_types::AgentCapabilityMode> {
    saved_enabled_capability_modes_in_dir(&config_home_dir())
}

pub fn saved_enabled_capability_modes_in_dir(
    config_dir: &Path,
) -> Vec<rebon_types::AgentCapabilityMode> {
    let settings = read_settings_json_object(&config_dir.join("settings.json")).unwrap_or_default();
    let configured = settings
        .get(ENABLED_CAPABILITY_MODES_CONFIG_KEY)
        .and_then(|value| value.as_array())
        .map(|values| {
            let mut modes: Vec<rebon_types::AgentCapabilityMode> = Vec::new();
            for raw in values.iter().filter_map(|value| value.as_str()) {
                if let Some(mode) = rebon_types::AgentCapabilityMode::from_wire(raw) {
                    if !modes.contains(&mode) {
                        modes.push(mode);
                    }
                }
            }
            modes
        })
        .unwrap_or_default();

    // An empty or wholly unrecognised list is a config mistake, not a request
    // for a session that cannot be started. Fall back rather than strand the
    // user in a UI with no way to open a chat.
    if configured.is_empty() {
        default_enabled_capability_modes()
    } else {
        configured
    }
}

pub fn default_enabled_capability_modes() -> Vec<rebon_types::AgentCapabilityMode> {
    vec![
        rebon_types::AgentCapabilityMode::Chat,
        rebon_types::AgentCapabilityMode::Normal,
    ]
}

pub fn save_enabled_capability_modes(
    modes: &[rebon_types::AgentCapabilityMode],
) -> anyhow::Result<()> {
    save_enabled_capability_modes_in_dir(&config_home_dir(), modes)
}

pub fn save_enabled_capability_modes_in_dir(
    config_dir: &Path,
    modes: &[rebon_types::AgentCapabilityMode],
) -> anyhow::Result<()> {
    let target = config_dir.join("settings.json");
    let mut settings = read_settings_json_object(&target)?;
    if modes.is_empty() {
        settings.remove(ENABLED_CAPABILITY_MODES_CONFIG_KEY);
    } else {
        settings.insert(
            ENABLED_CAPABILITY_MODES_CONFIG_KEY.to_string(),
            serde_json::Value::Array(
                modes
                    .iter()
                    .map(|mode| serde_json::Value::String(mode.as_wire().to_string()))
                    .collect(),
            ),
        );
    }
    write_settings_json_object(&target, &settings)
}

pub fn save_system_prompt_overrides(overrides: &SystemPromptOverrides) -> anyhow::Result<()> {
    save_system_prompt_overrides_in_dir(&config_home_dir(), overrides)
}

pub fn save_system_prompt_overrides_in_dir(
    config_dir: &Path,
    overrides: &SystemPromptOverrides,
) -> anyhow::Result<()> {
    let target = config_dir.join("settings.json");
    let mut settings = read_settings_json_object(&target)?;
    write_prompt_override(
        &mut settings,
        NORMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY,
        overrides.normal.as_deref(),
    );
    write_prompt_override(
        &mut settings,
        MINIMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY,
        overrides.minimal.as_deref(),
    );
    write_prompt_override(
        &mut settings,
        CHAT_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY,
        overrides.chat.as_deref(),
    );
    write_settings_json_object(&target, &settings)
}

fn read_prompt_override(
    settings: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    settings
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
}

fn write_prompt_override(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<&str>,
) {
    match value.filter(|value| !value.trim().is_empty()) {
        Some(value) => {
            settings.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        None => {
            settings.remove(key);
        }
    }
}

#[cfg(test)]
mod system_prompt_override_tests {
    use super::*;

    #[test]
    fn prompt_overrides_default_to_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            saved_system_prompt_overrides_in_dir(dir.path()),
            SystemPromptOverrides::default()
        );
    }

    #[test]
    fn prompt_overrides_round_trip_exact_text_and_preserve_siblings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"keep":{"nested":true}}"#,
        )
        .unwrap();
        let overrides = SystemPromptOverrides {
            normal: Some("  normal prompt\nsecond line  ".into()),
            minimal: Some("minimal prompt\n".into()),
            chat: None,
        };

        save_system_prompt_overrides_in_dir(dir.path(), &overrides).unwrap();

        assert_eq!(saved_system_prompt_overrides_in_dir(dir.path()), overrides);
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["keep"]["nested"], true);
    }

    #[test]
    fn prompt_overrides_clear_independently_and_treat_whitespace_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        save_system_prompt_overrides_in_dir(
            dir.path(),
            &SystemPromptOverrides {
                normal: Some("normal".into()),
                minimal: Some("minimal".into()),
                chat: None,
            },
        )
        .unwrap();

        save_system_prompt_overrides_in_dir(
            dir.path(),
            &SystemPromptOverrides {
                normal: Some(" \n\t ".into()),
                minimal: Some("minimal updated".into()),
                chat: None,
            },
        )
        .unwrap();

        assert_eq!(
            saved_system_prompt_overrides_in_dir(dir.path()),
            SystemPromptOverrides {
                normal: None,
                minimal: Some("minimal updated".into()),
                chat: None,
            }
        );
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap())
                .unwrap();
        assert!(settings
            .get(NORMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY)
            .is_none());
        assert_eq!(
            settings[MINIMAL_SYSTEM_PROMPT_OVERRIDE_CONFIG_KEY],
            "minimal updated"
        );
    }
}

pub fn saved_user_model() -> Option<String> {
    saved_user_model_in_dir(&config_home_dir())
}

pub fn saved_user_model_in_dir(config_dir: &Path) -> Option<String> {
    let target = config_dir.join("settings.json");
    let settings = read_settings_json_object(&target).ok()?;
    settings
        .get(USER_MODEL_CONFIG_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default")
        .map(ToString::to_string)
}

pub fn save_user_model(model: Option<&str>) -> anyhow::Result<()> {
    save_user_model_in_dir(&config_home_dir(), model)
}

pub fn save_user_model_in_dir(config_dir: &Path, model: Option<&str>) -> anyhow::Result<()> {
    let target = config_dir.join("settings.json");
    let mut settings = read_settings_json_object(&target)?;
    match model
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default")
    {
        Some(model) => {
            settings.insert(
                USER_MODEL_CONFIG_KEY.to_string(),
                serde_json::Value::String(model.to_string()),
            );
        }
        None => {
            settings.remove(USER_MODEL_CONFIG_KEY);
        }
    }
    write_settings_json_object(&target, &settings)
}

/// Persist a user's "model" config-option choice with provider-first
/// semantics: when a custom provider is active, a concrete model is stored on
/// that provider entry and any global override is cleared, so the
/// provider's own `model` field stays canonical across provider switches.
/// Without an active provider the choice is saved as the global user model
/// (the env-fallback path). Empty or "default" clears the global override and
/// leaves provider entries untouched.
///
/// Returns the resolved provider info when a custom provider is active, so
/// callers can report/refresh the effective provider + model.
pub fn persist_model_config_choice(value: &str) -> anyhow::Result<Option<CustomProviderInfo>> {
    persist_model_config_choice_in(&config_home_dir(), value)
}

pub fn persist_model_config_choice_in(
    config_dir: &Path,
    value: &str,
) -> anyhow::Result<Option<CustomProviderInfo>> {
    let trimmed = value.trim();
    let concrete =
        (!trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("default")).then_some(trimmed);
    let Some(provider) = get_active_custom_provider_name_from(config_dir) else {
        save_user_model_in_dir(config_dir, concrete)?;
        return Ok(None);
    };
    let info = match concrete {
        Some(model) => set_custom_provider_model_in(config_dir, &provider, model)?,
        None => list_custom_providers_from(config_dir)
            .into_iter()
            .find(|candidate| candidate.name.eq_ignore_ascii_case(&provider))
            .ok_or_else(|| anyhow::anyhow!("Active provider \"{provider}\" not found."))?,
    };
    save_user_model_in_dir(config_dir, None)?;
    Ok(Some(info))
}

pub fn saved_effort_level() -> Option<String> {
    saved_effort_level_in_dir(&config_home_dir())
}

pub fn saved_effort_level_in_dir(config_dir: &Path) -> Option<String> {
    let target = config_dir.join("settings.json");
    let settings = read_settings_json_object(&target).ok()?;
    settings
        .get(EFFORT_LEVEL_CONFIG_KEY)
        .and_then(serde_json::Value::as_str)
        .and_then(normalize_effort_level)
        .map(ToString::to_string)
}

pub fn save_effort_level(level: Option<&str>) -> anyhow::Result<()> {
    save_effort_level_in_dir(&config_home_dir(), level)
}

pub fn save_effort_level_in_dir(config_dir: &Path, level: Option<&str>) -> anyhow::Result<()> {
    let target = config_dir.join("settings.json");
    let mut settings = read_settings_json_object(&target)?;
    match level.and_then(normalize_effort_level) {
        Some(level) => {
            settings.insert(
                EFFORT_LEVEL_CONFIG_KEY.to_string(),
                serde_json::Value::String(level.to_string()),
            );
        }
        None => {
            settings.remove(EFFORT_LEVEL_CONFIG_KEY);
        }
    }
    write_settings_json_object(&target, &settings)
}

pub fn saved_default_permission_mode() -> Option<PermissionMode> {
    saved_default_permission_mode_in_dir(&config_home_dir())
}

pub fn saved_default_permission_mode_wire() -> Option<String> {
    saved_default_permission_mode().map(|mode| mode.as_wire().to_string())
}

pub fn saved_default_permission_mode_in_dir(config_dir: &Path) -> Option<PermissionMode> {
    read_default_permission_mode_in_dir(config_dir).filter(|mode| !mode.is_session_scoped())
}

pub fn save_default_permission_mode(mode: PermissionMode) -> anyhow::Result<()> {
    save_default_permission_mode_in_dir(&config_home_dir(), mode)
}

pub fn save_default_permission_mode_wire(mode: &str) -> anyhow::Result<()> {
    let Some(mode) = parse_user_permission_mode(mode) else {
        anyhow::bail!("unknown permission mode `{mode}`");
    };
    save_default_permission_mode(mode)
}

pub fn save_default_permission_mode_in_dir(
    config_dir: &Path,
    mode: PermissionMode,
) -> anyhow::Result<()> {
    if mode.is_session_scoped() {
        return Ok(());
    }
    write_default_permission_mode(config_dir, mode)
}

/// Default modes an app front end may persist as a launch default. `plan`
/// stays session-scoped everywhere; `bypassPermissions` becomes an
/// app-selectable default while the CLI surfaces (TUI/ACP) keep excluding it
/// from their pickers and never read it back.
fn is_app_persistable_default_mode(mode: PermissionMode) -> bool {
    !matches!(mode, PermissionMode::Plan)
}

pub fn saved_app_default_permission_mode() -> Option<PermissionMode> {
    saved_app_default_permission_mode_in_dir(&config_home_dir())
}

pub fn saved_app_default_permission_mode_wire() -> Option<String> {
    saved_app_default_permission_mode().map(|mode| mode.as_wire().to_string())
}

pub fn saved_app_default_permission_mode_in_dir(config_dir: &Path) -> Option<PermissionMode> {
    read_default_permission_mode_in_dir(config_dir)
        .filter(|mode| is_app_persistable_default_mode(*mode))
}

pub fn save_app_default_permission_mode(mode: PermissionMode) -> anyhow::Result<()> {
    save_app_default_permission_mode_in_dir(&config_home_dir(), mode)
}

pub fn save_app_default_permission_mode_wire(mode: &str) -> anyhow::Result<()> {
    let Some(mode) = parse_user_permission_mode(mode) else {
        anyhow::bail!("unknown permission mode `{mode}`");
    };
    save_app_default_permission_mode(mode)
}

pub fn save_app_default_permission_mode_in_dir(
    config_dir: &Path,
    mode: PermissionMode,
) -> anyhow::Result<()> {
    if !is_app_persistable_default_mode(mode) {
        return Ok(());
    }
    write_default_permission_mode(config_dir, mode)
}

fn read_default_permission_mode_in_dir(config_dir: &Path) -> Option<PermissionMode> {
    let target = config_dir.join("settings.json");
    let settings = read_settings_json_object(&target).ok()?;
    read_default_permission_mode(&settings)
}

fn write_default_permission_mode(config_dir: &Path, mode: PermissionMode) -> anyhow::Result<()> {
    let target = config_dir.join("settings.json");
    let mut settings = read_settings_json_object(&target)?;
    let permissions = settings
        .entry(PERMISSIONS_CONFIG_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !permissions.is_object() {
        *permissions = serde_json::Value::Object(serde_json::Map::new());
    }
    let permissions = permissions
        .as_object_mut()
        .expect("permissions normalized to object");
    permissions.insert(
        PERMISSIONS_DEFAULT_MODE_CONFIG_KEY.to_string(),
        serde_json::Value::String(mode.as_wire().to_string()),
    );
    permissions.remove(PERMISSIONS_DEFAULT_MODE_SNAKE_CONFIG_KEY);
    settings.remove(DEFAULT_PERMISSION_MODE_CONFIG_KEY);
    write_settings_json_object(&target, &settings)
}

/// `settings.json` key holding the kernel plugin switches:
/// `{"plugins": {"<id>": {"enabled": false}}}` (a bare bool is accepted too).
pub const PLUGINS_CONFIG_KEY: &str = "plugins";

/// `settings.json` key holding the sandbox block. Its shape belongs to the
/// sandbox plugin, which parses it; only the two names below are written here.
pub const SANDBOX_CONFIG_KEY: &str = "sandbox";

/// `sandbox.allowUnsandboxedCommands` — the `/sandbox` override, spelled the
/// way the plugin's parser reads it.
pub const SANDBOX_ALLOW_UNSANDBOXED_KEY: &str = "allowUnsandboxedCommands";

/// Every explicit plugin switch in `settings.json`, as the plugin registry
/// reads them. Missing ids keep their definition's default.
/// The `plugins.<id>.enabled` switches across the settings chain for `cwd`
/// — user, then project, then local, a later file overriding an earlier one
/// per id. The same [`settings_files`] chain every other setting is read
/// from, so a switch and the setting it gates can never come from two
/// different files.
pub fn saved_plugin_switches_in(config_dir: &Path, cwd: &Path) -> BTreeMap<String, bool> {
    saved_plugin_switches_in_files(
        settings_files(config_dir, cwd)
            .into_iter()
            .map(|(_, path)| path),
    )
}

/// [`saved_plugin_switches_in`] over an explicit list of files, first to last.
///
/// A file that is missing, unreadable or not JSON contributes nothing, as it
/// always did for the user file — that is [`settings_layers_in_files`]'s rule,
/// shared with every other layered read here; the bare `"web": false`
/// shorthand and the `{"enabled": bool}` form are both accepted, and any other
/// value is skipped.
pub fn saved_plugin_switches_in_files(
    files: impl IntoIterator<Item = PathBuf>,
) -> BTreeMap<String, bool> {
    let mut switches = BTreeMap::new();
    for settings in settings_layers_in_files(files) {
        let Some(plugins) = settings
            .get(PLUGINS_CONFIG_KEY)
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for (id, value) in plugins {
            let enabled = match value {
                serde_json::Value::Bool(enabled) => *enabled,
                serde_json::Value::Object(entry) => {
                    match entry.get("enabled").and_then(serde_json::Value::as_bool) {
                        Some(enabled) => enabled,
                        None => continue,
                    }
                }
                _ => continue,
            };
            switches.insert(id.clone(), enabled);
        }
    }
    switches
}

/// The switches for this process: the config home and the current directory,
/// which is the project the kernel is booting for.
pub fn saved_plugin_switches() -> BTreeMap<String, bool> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    saved_plugin_switches_in(&config_home_dir(), &cwd)
}

/// Persist one plugin switch. The write reaches the change observer, so a
/// booted kernel reconciles its registry without being told twice.
pub fn save_plugin_enabled_in_dir(
    config_dir: &Path,
    id: &str,
    enabled: bool,
) -> anyhow::Result<()> {
    // The user layer of the chain the switches are read from — `settings.json`,
    // or `cowork_settings.json` in cowork mode. Writing anywhere else is a
    // toggle the settings UI shows and the kernel never sees.
    save_plugin_enabled_in_file(&user_settings_file(config_dir), id, enabled)
}

/// [`save_plugin_enabled_in_dir`] against one explicit settings file.
pub fn save_plugin_enabled_in_file(target: &Path, id: &str, enabled: bool) -> anyhow::Result<()> {
    let mut settings = read_settings_json_object(target)?;
    let plugins = settings
        .entry(PLUGINS_CONFIG_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !plugins.is_object() {
        *plugins = serde_json::Value::Object(serde_json::Map::new());
    }
    let plugins = plugins
        .as_object_mut()
        .expect("plugins normalized to object");
    let entry = plugins
        .entry(id.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    entry
        .as_object_mut()
        .expect("plugin entry normalized to object")
        .insert("enabled".to_string(), serde_json::Value::Bool(enabled));
    write_settings_json_object(target, &settings)
}

pub fn save_plugin_enabled(id: &str, enabled: bool) -> anyhow::Result<()> {
    save_plugin_enabled_in_dir(&config_home_dir(), id, enabled)
}

/// The key `enabled` under `plugins.<id>` — the kernel's switch, and the one
/// key inside a plugin's own namespace the plugin may not write.
///
/// A plugin that could switch itself on could not be switched off; a plugin
/// that could switch another on has escaped its namespace. Both are refused at
/// the seat, and the name lives here because this is where both the reader and
/// the writer of the switch already are.
pub const PLUGIN_ENABLED_KEY: &str = "enabled";

/// Everything `plugins.<id>` says across the settings chain for `cwd`.
///
/// The same user → project → local order every other setting is read in, a
/// later file overriding an earlier one **per key** rather than replacing the
/// whole namespace: a project that sets one key does not silently drop the
/// user's other five. The `enabled` switch is left out — it belongs to the
/// kernel, not to the plugin, and is read through
/// [`saved_plugin_switches_in`].
pub fn plugin_settings_in(
    config_dir: &Path,
    cwd: &Path,
    id: &str,
) -> serde_json::Map<String, serde_json::Value> {
    plugin_settings_in_files(
        settings_files(config_dir, cwd).into_iter().map(|(_, p)| p),
        id,
    )
}

/// [`plugin_settings_in`] over an explicit list of files, first to last.
pub fn plugin_settings_in_files(
    files: impl IntoIterator<Item = PathBuf>,
    id: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut merged = serde_json::Map::new();
    for settings in settings_layers_in_files(files) {
        let Some(entry) = settings
            .get(PLUGINS_CONFIG_KEY)
            .and_then(serde_json::Value::as_object)
            .and_then(|plugins| plugins.get(id))
        else {
            continue;
        };
        // The bare `"web": false` shorthand says only whether the plugin runs,
        // so it contributes no keys — not an empty namespace that wipes the
        // layer below it.
        let Some(object) = entry.as_object() else {
            continue;
        };
        for (key, value) in object {
            if key == PLUGIN_ENABLED_KEY {
                continue;
            }
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

/// Merge `patch` into `plugins.<id>` in the user layer of the chain.
///
/// The user layer and nothing else: a write to a project file would be a
/// setting the user cannot see in their own settings and cannot remove from
/// another checkout. Which file that is — `settings.json` or
/// `cowork_settings.json` — is [`user_settings_file`]'s answer.
pub fn save_plugin_settings_in_dir(
    config_dir: &Path,
    id: &str,
    patch: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    save_plugin_settings_in_file(&user_settings_file(config_dir), id, patch)
}

/// [`save_plugin_settings_in_dir`] against one explicit settings file.
///
/// Read the object, change the named keys, write the object back — so every
/// other plugin's namespace, this plugin's other keys, and its `enabled`
/// switch all survive. A `null` in the patch removes the key rather than
/// storing a null, which is how a plugin puts a setting back to its default.
///
/// Writing `enabled` is refused, loudly: see [`PLUGIN_ENABLED_KEY`].
pub fn save_plugin_settings_in_file(
    target: &Path,
    id: &str,
    patch: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if patch.contains_key(PLUGIN_ENABLED_KEY) {
        anyhow::bail!(
            "`{PLUGIN_ENABLED_KEY}` under plugins.{id} is the kernel's switch and is not \
             writable through the settings seat"
        );
    }
    let mut settings = read_settings_json_object(target)?;
    let plugins = settings
        .entry(PLUGINS_CONFIG_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !plugins.is_object() {
        *plugins = serde_json::Value::Object(serde_json::Map::new());
    }
    let plugins = plugins
        .as_object_mut()
        .expect("plugins normalized to object");
    let entry = plugins
        .entry(id.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    // The shorthand carries one fact, and normalising it keeps that fact.
    if let Some(enabled) = entry.as_bool() {
        *entry = serde_json::json!({ PLUGIN_ENABLED_KEY: enabled });
    }
    if !entry.is_object() {
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    let entry = entry
        .as_object_mut()
        .expect("plugin entry normalized to object");
    for (key, value) in patch {
        if value.is_null() {
            entry.remove(key);
        } else {
            entry.insert(key.clone(), value.clone());
        }
    }
    write_settings_json_object_for(target, &settings, Some(id))
}

/// Persist `sandbox.allowUnsandboxedCommands`, the `/sandbox` override.
///
/// Same landing place as [`save_plugin_enabled_in_dir`] and for the same
/// reason: the sandbox settings are read from the chain whose user layer is
/// [`user_settings_file`], so a write anywhere else is a toggle the panel
/// shows and the next session never reads.
pub fn save_sandbox_allow_unsandboxed_commands_in_dir(
    config_dir: &Path,
    allow: bool,
) -> anyhow::Result<()> {
    save_sandbox_allow_unsandboxed_commands_in_file(&user_settings_file(config_dir), allow)
}

/// [`save_sandbox_allow_unsandboxed_commands_in_dir`] against one explicit
/// settings file.
///
/// Read the object, change the one key, write the object back — so a
/// `sandbox` block with `enabled`, `excludedCommands` and a whole filesystem
/// section survives a change to the override.
pub fn save_sandbox_allow_unsandboxed_commands_in_file(
    target: &Path,
    allow: bool,
) -> anyhow::Result<()> {
    let mut settings = read_settings_json_object(target)?;
    let sandbox = settings
        .entry(SANDBOX_CONFIG_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !sandbox.is_object() {
        *sandbox = serde_json::Value::Object(serde_json::Map::new());
    }
    sandbox
        .as_object_mut()
        .expect("sandbox normalized to object")
        .insert(
            SANDBOX_ALLOW_UNSANDBOXED_KEY.to_string(),
            serde_json::Value::Bool(allow),
        );
    write_settings_json_object(target, &settings)
}

fn normalize_effort_level(level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

fn read_default_permission_mode(
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Option<PermissionMode> {
    settings
        .get(PERMISSIONS_CONFIG_KEY)
        .and_then(serde_json::Value::as_object)
        .and_then(|permissions| {
            permissions
                .get(PERMISSIONS_DEFAULT_MODE_CONFIG_KEY)
                .or_else(|| permissions.get(PERMISSIONS_DEFAULT_MODE_SNAKE_CONFIG_KEY))
        })
        .and_then(serde_json::Value::as_str)
        .and_then(parse_user_permission_mode)
        .or_else(|| {
            settings
                .get(DEFAULT_PERMISSION_MODE_CONFIG_KEY)
                .and_then(serde_json::Value::as_str)
                .and_then(parse_user_permission_mode)
        })
}

fn parse_user_permission_mode(mode: &str) -> Option<PermissionMode> {
    match mode.trim() {
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        "default" => Some(PermissionMode::Default),
        "dontAsk" => Some(PermissionMode::DontAsk),
        "plan" => Some(PermissionMode::Plan),
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    }
}

fn save_ui_mode_in_dir(config_dir: &Path, mode: &str) -> anyhow::Result<()> {
    let target = config_dir.join("settings.json");
    let mut settings = read_settings_json_object(&target)?;
    settings.insert(
        UI_MODE_CONFIG_KEY.to_string(),
        serde_json::Value::String(mode.to_string()),
    );
    write_settings_json_object(&target, &settings)
}

fn save_math_rendering_mode_in_dir(
    config_dir: &Path,
    mode: MathRenderingMode,
) -> anyhow::Result<()> {
    save_math_rendering_mode_in_file(&config_dir.join("settings.json"), mode)
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
pub struct SettingsPermissionRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

pub fn permission_rules_from_settings(path: &Path) -> anyhow::Result<SettingsPermissionRules> {
    let mut settings = read_settings_json_object(path)?;
    match settings.remove(PERMISSIONS_CONFIG_KEY) {
        None => Ok(SettingsPermissionRules::default()),
        Some(permissions @ serde_json::Value::Object(_)) => serde_json::from_value(permissions)
            .map_err(|err| anyhow::anyhow!("invalid permissions at {path:?}: {err}")),
        Some(_) => anyhow::bail!("permissions at {path:?} must be a JSON object"),
    }
}

/// The settings chain, read: one JSON object per file that yields one, in
/// the order given.
///
/// The one walk over a layered `settings.json` read in this crate. Three
/// readers used to write it out themselves — the plugin switches, a
/// plugin's own keys, and `sandbox.enabled` — which is three places to
/// disagree about what a missing or malformed file means. It means the
/// same thing in all three: the file drops out of the chain and the
/// layers around it are unaffected, so a fold over the result is
/// last-wins over exactly the files that parsed.
///
/// Which files those are is [`settings_files_for_mode`]'s answer, and it
/// is the only thing that knows about cowork mode. Keeping the two halves
/// apart is what lets a caller pin the order without touching the process
/// environment.
pub(crate) fn settings_layers_in_files(
    files: impl IntoIterator<Item = PathBuf>,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    files
        .into_iter()
        .filter_map(|path| read_settings_json_object(&path).ok())
        .collect()
}

fn read_settings_json_object(
    path: &Path,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Map::new());
        }
        Err(err) => {
            return Err(
                anyhow::Error::from(err).context(format!("failed to read settings at {path:?}"))
            );
        }
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|err| anyhow::anyhow!("failed to parse settings at {path:?}: {err}"))?
    {
        serde_json::Value::Object(object) => Ok(object),
        _ => anyhow::bail!("settings at {path:?} must be a JSON object"),
    }
}

fn write_settings_json_object(
    target: &Path,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    write_settings_json_object_for(target, settings, None)
}

/// [`write_settings_json_object`], saying which plugin namespace the write was
/// about when it was about exactly one.
fn write_settings_json_object_for(
    target: &Path,
    settings: &serde_json::Map<String, serde_json::Value>,
    namespace: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            anyhow::Error::from(err).context(format!("failed to create settings dir {parent:?}"))
        })?;
    }
    let serialized = serde_json::to_vec_pretty(settings)
        .map_err(|err| anyhow::anyhow!("failed to serialize settings: {err}"))?;
    rebon_session::write_file_atomically(&target, &serialized).map_err(|err| {
        anyhow::Error::from(err).context(format!("failed to write settings at {target:?}"))
    })?;
    notify_config_changed_for(ConfigFileKind::Settings, target, namespace);
    Ok(())
}

pub fn saved_coordinator_use_worktree() -> bool {
    saved_coordinator_use_worktree_in_dir(&config_home_dir())
}

pub fn saved_coordinator_use_worktree_in_dir(config_dir: &Path) -> bool {
    let config = match read_config_roundtrip(config_dir) {
        Ok(config) => config,
        Err(_) => return false,
    };
    read_coordinator_use_worktree(&config.extra).unwrap_or(false)
}

fn read_coordinator_use_worktree(
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Option<bool> {
    extra
        .get(COORDINATOR_CONFIG_KEY)
        .and_then(|value| value.as_object())
        .and_then(|coordinator| {
            coordinator
                .get(COORDINATOR_USE_WORKTREE_CONFIG_KEY)
                .or_else(|| coordinator.get(COORDINATOR_USE_WORKTREE_SNAKE_CONFIG_KEY))
        })
        .and_then(|value| value.as_bool())
        .or_else(|| {
            extra
                .get(COORDINATOR_USE_WORKTREE_CONFIG_KEY)
                .or_else(|| extra.get(COORDINATOR_USE_WORKTREE_SNAKE_CONFIG_KEY))
                .and_then(|value| value.as_bool())
        })
}

// (was #[cfg(test)] — promoted to a regular pub helper so an
// out-of-crate test suite can reach it across the crate boundary.)
pub fn save_coordinator_use_worktree_in_dir(
    config_dir: &Path,
    enabled: bool,
) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    let mut coordinator = config
        .extra
        .remove(COORDINATOR_CONFIG_KEY)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    coordinator.insert(
        COORDINATOR_USE_WORKTREE_CONFIG_KEY.to_string(),
        serde_json::Value::Bool(enabled),
    );
    config.extra.insert(
        COORDINATOR_CONFIG_KEY.to_string(),
        serde_json::Value::Object(coordinator),
    );
    write_config_roundtrip(config_dir, &config)
}

pub fn save_fast_mode_enabled_in_dir(config_dir: &Path, enabled: bool) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    if enabled {
        config.extra.insert(
            SERVICE_TIER_CONFIG_KEY.to_string(),
            serde_json::Value::String("fast".to_string()),
        );
    } else {
        config.extra.remove(SERVICE_TIER_CONFIG_KEY);
    }

    let mut features = config
        .extra
        .remove(FEATURES_CONFIG_KEY)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if enabled {
        features.insert(
            FAST_MODE_FEATURE_KEY.to_string(),
            serde_json::Value::Bool(true),
        );
    } else {
        features.remove(FAST_MODE_FEATURE_KEY);
    }
    if features.is_empty() {
        config.extra.remove(FEATURES_CONFIG_KEY);
    } else {
        config.extra.insert(
            FEATURES_CONFIG_KEY.to_string(),
            serde_json::Value::Object(features),
        );
    }

    write_config_roundtrip(config_dir, &config)
}

/// Read runtime sub-agent model choices from config.
///
/// Supported shapes:
/// - `<config_dir>/config.json` top-level `agents` / `categories`
/// - `<config_dir>/agents.json` with the same top-level keys
///
/// `agents.json` is applied last so users can make quick local
/// adjustments without touching the larger config.json file.
pub fn saved_sub_agent_model_config() -> SubAgentModelConfig {
    saved_sub_agent_model_config_in_dir(&config_home_dir())
}

pub fn saved_sub_agent_model_config_in_dir(config_dir: &Path) -> SubAgentModelConfig {
    let mut output = SubAgentModelConfig::default();

    if let Ok(config) = read_config_roundtrip(config_dir) {
        merge_sub_agent_model_config(
            &mut output,
            &serde_json::Value::Object(config.extra.clone()),
        );
    }

    let path = agents_json_path(config_dir);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => merge_sub_agent_model_config(&mut output, &value),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "failed to parse agents.json");
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "failed to read agents.json");
        }
    }

    output
}

fn merge_sub_agent_model_config(output: &mut SubAgentModelConfig, root: &serde_json::Value) {
    let Some(object) = root.as_object() else {
        return;
    };
    if let Some(agents) = object.get("agents").and_then(|v| v.as_object()) {
        for (name, value) in agents {
            if let Some(selection) = parse_sub_agent_model_selection(value) {
                output.insert_agent(name, selection);
            }
        }
    }
    if let Some(categories) = object.get("categories").and_then(|v| v.as_object()) {
        for (name, value) in categories {
            if let Some(selection) = parse_sub_agent_model_selection(value) {
                output.insert_category(name, selection);
            }
        }
    }
    for key in [
        "aliases",
        "modelAliases",
        "agentAliases",
        "agentModelAliases",
    ] {
        if let Some(aliases) = object.get(key).and_then(|v| v.as_object()) {
            for (name, value) in aliases {
                if let Some(selection) = parse_sub_agent_model_selection(value) {
                    output.insert_alias(name, selection);
                }
            }
        }
    }
}

fn parse_sub_agent_model_selection(value: &serde_json::Value) -> Option<SubAgentModelSelection> {
    match value {
        serde_json::Value::String(model) => {
            let model = model.trim();
            if model.is_empty() {
                None
            } else {
                Some(SubAgentModelSelection::model(model.to_string()))
            }
        }
        serde_json::Value::Object(map) => {
            let provider = map
                .get("provider")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let model = map
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let model_profile = map
                .get("modelProfile")
                .or_else(|| map.get("model_profile"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let reasoning_effort = map
                .get("variant")
                .or_else(|| map.get("effort"))
                .or_else(|| map.get("reasoning_effort"))
                .or_else(|| map.get("reasoningEffort"))
                .and_then(|v| v.as_str())
                .and_then(parse_reasoning_effort);
            if provider.is_none()
                && model.is_none()
                && model_profile.is_none()
                && reasoning_effort.is_none()
            {
                None
            } else {
                let mut selection =
                    SubAgentModelSelection::new(model, model_profile, reasoning_effort);
                if let Some(provider) = provider {
                    selection = selection.with_provider(provider);
                }
                Some(selection)
            }
        }
        _ => None,
    }
}

fn parse_reasoning_effort(raw: &str) -> Option<ReasoningEffort> {
    raw.parse().ok()
}

/// Read the persisted `subAgentsEnabled` flag from `config.json`.
///
/// Defaults to `true` when the key is missing, the config is absent,
/// or the file fails to parse — sub-agent delegation is on by default
/// so fresh installs get the full capability. The Settings dialog
/// ("Sub-agents" row) toggles this value via [`save_sub_agents_enabled`].
///
/// Read at startup and propagated to both
/// [`rebon_tool::agent::set_sub_agents_enabled`] (controls whether
/// the Agent tool is advertised to the model) and the ACP handler's
/// config-option list (so the row reflects the persisted state when
/// the Settings dialog opens).
pub fn saved_sub_agents_enabled() -> bool {
    saved_sub_agents_enabled_in_dir(&config_home_dir())
}

/// Testable variant of [`saved_sub_agents_enabled`] that operates on
/// an arbitrary directory instead of `config_home_dir()`.
pub fn saved_sub_agents_enabled_in_dir(config_dir: &Path) -> bool {
    let config = match read_config_roundtrip(config_dir) {
        Ok(c) => c,
        Err(_) => return true,
    };
    config
        .extra
        .get("subAgentsEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Persist the `subAgentsEnabled` flag to `config.json`. Preserves
/// every other top-level key on re-serialize (round-trip via
/// `extra` map). Called from the Settings dialog toggle handler in
/// the TUI runner.
pub fn save_sub_agents_enabled(enabled: bool) {
    if let Err(err) = save_sub_agents_enabled_in_dir(&config_home_dir(), enabled) {
        tracing::warn!(error = %err, "failed to persist sub_agents flag");
    }
}

/// Testable variant of [`save_sub_agents_enabled`] that operates on
/// an arbitrary directory. Returns the IO error instead of logging
/// so tests can assert on the path.
pub fn save_sub_agents_enabled_in_dir(config_dir: &Path, enabled: bool) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    config.extra.insert(
        "subAgentsEnabled".to_string(),
        serde_json::Value::Bool(enabled),
    );
    write_config_roundtrip(config_dir, &config)
}

/// `config.json` key selecting which shell tool(s) the agent is offered.
///
/// Values are the wire forms of `rebon_tool::ShellToolPreference` — `auto`,
/// `bash`, `powershell`, `both`. This crate stores the string as written and
/// leaves parsing to that enum, so the two can never disagree about what a
/// value means; an unrecognized value falls back to `auto` at read time there.
pub const SHELL_TOOL_CONFIG_KEY: &str = "shellTool";

/// The persisted shell-tool choice, or `None` when the user has not made one.
///
/// `None` is meaningfully different from `"auto"`: the Settings window
/// shows an explicit choice as selected, while an absent key means "whatever
/// the platform resolves to", which is what a fresh install should get.
pub fn saved_shell_tool() -> Option<String> {
    saved_shell_tool_in_dir(&config_home_dir())
}

/// Testable variant of [`saved_shell_tool`] against an arbitrary directory.
pub fn saved_shell_tool_in_dir(config_dir: &Path) -> Option<String> {
    let config = read_config_roundtrip(config_dir).ok()?;
    config
        .extra
        .get(SHELL_TOOL_CONFIG_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Persist the shell-tool choice, preserving every other `config.json` key.
pub fn save_shell_tool(value: &str) {
    if let Err(err) = save_shell_tool_in_dir(&config_home_dir(), value) {
        tracing::warn!(error = %err, shell_tool = value, "failed to persist shell tool preference");
    }
}

/// Testable variant of [`save_shell_tool`] that reports the IO error.
pub fn save_shell_tool_in_dir(config_dir: &Path, value: &str) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    config.extra.insert(
        SHELL_TOOL_CONFIG_KEY.to_string(),
        serde_json::Value::String(value.trim().to_string()),
    );
    write_config_roundtrip(config_dir, &config)
}

pub fn saved_claude_codex_fallback_enabled() -> bool {
    saved_claude_codex_fallback_enabled_in_dir(&config_home_dir())
}

pub fn saved_claude_codex_fallback_enabled_in_dir(config_dir: &Path) -> bool {
    let config = match read_config_roundtrip(config_dir) {
        Ok(config) => config,
        Err(_) => return false,
    };
    config
        .extra
        .get("claudeCodexFallbackEnabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub fn save_claude_codex_fallback_enabled(enabled: bool) {
    if let Err(err) = save_claude_codex_fallback_enabled_in_dir(&config_home_dir(), enabled) {
        tracing::warn!(error = %err, "failed to persist Claude/Codex fallback flag");
    }
}

pub fn save_claude_codex_fallback_enabled_in_dir(
    config_dir: &Path,
    enabled: bool,
) -> anyhow::Result<()> {
    let mut config = read_config_roundtrip(config_dir)?;
    config.extra.insert(
        "claudeCodexFallbackEnabled".to_string(),
        serde_json::Value::Bool(enabled),
    );
    write_config_roundtrip(config_dir, &config)
}

/// One-time upgrade bridge for the `.claude` / `.codex` fallback flag.
///
/// Those directories used to be scanned unconditionally; they are now opt-in
/// and default to off. Left alone, that silently strips every compatibility
/// skill and command from anyone already relying on them, with no signal
/// beyond "my commands vanished".
///
/// So an install that predates the flag inherits `true`: a `config.json` that
/// exists but carries no `claudeCodexFallbackEnabled` key can only come from a
/// build that scanned unconditionally.
///
/// A fresh install anchors `false` immediately instead of being left alone.
/// Without that anchor the *second* startup would find the `config.json` that
/// onboarding just wrote — still keyless — and mistake a brand new install for
/// an upgrade. Writing `config.json` early is safe: first-run state is keyed on
/// `hasCompletedOnboarding`, never on the file's existence.
///
/// Callers must still run this **before** any code path that can create
/// `config.json`, so the anchor lands first. Returns the seeded value, or
/// `None` when an explicit choice was already recorded.
pub fn migrate_claude_codex_fallback_default_in_dir(
    config_dir: &Path,
) -> anyhow::Result<Option<bool>> {
    let predates_flag = config_json_path(config_dir).exists();
    let mut config = read_config_roundtrip(config_dir)?;
    if config.extra.contains_key("claudeCodexFallbackEnabled") {
        return Ok(None);
    }
    config.extra.insert(
        "claudeCodexFallbackEnabled".to_string(),
        serde_json::Value::Bool(predates_flag),
    );
    write_config_roundtrip(config_dir, &config)?;
    Ok(Some(predates_flag))
}

/// [`migrate_claude_codex_fallback_default_in_dir`] against the active config
/// home. Failures are logged, never fatal — a config that cannot be seeded
/// falls back to the opt-in default rather than blocking startup.
pub fn migrate_claude_codex_fallback_default() {
    match migrate_claude_codex_fallback_default_in_dir(&config_home_dir()) {
        Ok(Some(true)) => {
            tracing::info!(
                "existing install predates the Claude/Codex fallback flag — seeding it enabled"
            );
        }
        Ok(Some(false)) => {
            tracing::debug!("fresh install — anchoring the Claude/Codex fallback flag disabled");
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(error = %err, "failed to seed Claude/Codex fallback flag");
        }
    }
}

/// Move `customProviders[]` into the provider store, once, at startup.
///
/// Failures are logged and never fatal: a migration that cannot complete
/// leaves the old layout in place and Rebon keeps reading it. Callers run this
/// alongside the other startup migrations, before anything resolves a
/// provider.
pub fn migrate_providers_to_store() {
    match provider_store::migrate(&config_home_dir()) {
        Ok(provider_store::MigrationOutcome::Migrated { count, backup }) => {
            tracing::info!(
                count,
                backup = %backup.display(),
                store = %provider_store::provider_store_dir(&config_home_dir()).display(),
                "moved custom providers into the provider store"
            );
        }
        Ok(provider_store::MigrationOutcome::NothingToMigrate) => {
            tracing::debug!("no custom providers to move into the provider store");
        }
        Ok(provider_store::MigrationOutcome::AlreadyMigrated) => {}
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to move custom providers into the provider store — staying on config.json"
            );
        }
    }
}

/// Mark onboarding as completed by setting `hasCompletedOnboarding:
/// true` in `config.json`. Preserves all existing config keys.
pub fn complete_onboarding() {
    let config_dir = config_home_dir();
    let mut config = match read_config_roundtrip(&config_dir) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "failed to read config for onboarding completion");
            return;
        }
    };
    config.extra.insert(
        "hasCompletedOnboarding".to_string(),
        serde_json::Value::Bool(true),
    );
    if let Err(err) = write_config_roundtrip(&config_dir, &config) {
        tracing::warn!(error = %err, "failed to write onboarding completion to config");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_settings_missing_files_and_sections_have_no_rules() {
        let dir = tempfile::tempdir().expect("settings directory");
        let path = dir.path().join("settings.json");
        assert_eq!(
            permission_rules_from_settings(&path).expect("missing settings are optional"),
            SettingsPermissionRules::default()
        );
        assert!(!path.exists());
        for contents in [r#"{}"#, r#"{"model":"test"}"#, r#"{"permissions":{}}"#] {
            std::fs::write(&path, contents).expect("write settings");
            assert_eq!(
                permission_rules_from_settings(&path).expect("empty permission rules"),
                SettingsPermissionRules::default()
            );
        }
    }

    #[test]
    fn permission_settings_preserve_rule_order_and_ignore_other_settings() {
        let dir = tempfile::tempdir().expect("settings directory");
        let path = dir.path().join("settings.json");
        let contents = r#"{"model":"test","permissions":{"defaultMode":"auto","allow":["Read","Bash(ls:*)","Read"],"deny":["Write","Edit"]}}"#;
        std::fs::write(&path, contents).expect("write settings");
        assert_eq!(
            permission_rules_from_settings(&path).expect("read rules"),
            SettingsPermissionRules {
                allow: vec!["Read".into(), "Bash(ls:*)".into(), "Read".into()],
                deny: vec!["Write".into(), "Edit".into()],
            }
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read settings"),
            contents
        );
    }

    #[test]
    fn permission_settings_reject_malformed_json_and_rule_shapes() {
        let dir = tempfile::tempdir().expect("settings directory");
        let path = dir.path().join("settings.json");
        for contents in [
            "{",
            "[]",
            "null",
            r#"{"permissions":null}"#,
            r#"{"permissions":[]}"#,
            r#"{"permissions":"deny"}"#,
            r#"{"permissions":{"allow":"Read"}}"#,
            r#"{"permissions":{"deny":false}}"#,
            r#"{"permissions":{"allow":["Read",42]}}"#,
            r#"{"permissions":{"allow":["Read"],"deny":[null]}}"#,
        ] {
            std::fs::write(&path, contents).expect("write settings");
            let error = permission_rules_from_settings(&path)
                .expect_err("invalid policy must not be skipped");
            assert!(error.to_string().contains("settings.json"), "{error}");
        }
    }

    #[test]
    fn permission_settings_report_unreadable_files() {
        let dir = tempfile::tempdir().expect("settings directory");
        let path = dir.path().join("settings.json");
        std::fs::create_dir(&path).expect("directory where settings file belongs");
        let error = permission_rules_from_settings(&path).expect_err("read must fail");
        assert!(
            error.to_string().contains("failed to read settings"),
            "{error}"
        );
        assert!(error.to_string().contains("settings.json"), "{error}");
    }

    #[test]
    fn plugin_switches_are_read_and_written_in_the_cowork_user_file_when_that_mode_is_on() {
        // Pinned through the `_for_mode` / `_in_files` / `_in_file` shapes so
        // the test never touches the process environment; the env-reading
        // wrappers are one `cowork_mode()` call each.
        let dir = tempfile::tempdir().expect("tempdir");
        let user_file = user_settings_file_for_mode(dir.path(), true);
        assert_eq!(user_file, dir.path().join("cowork_settings.json"));
        assert_eq!(
            user_settings_file_for_mode(dir.path(), false),
            dir.path().join("settings.json")
        );
        // The user layer of the settings chain is `cowork_settings.json` in
        // this mode — the same file `sandbox.enabled` is read from.
        std::fs::write(&user_file, r#"{"plugins":{"sandbox":{"enabled":false}}}"#)
            .expect("write cowork settings");
        let files = settings_files_for_mode(dir.path(), dir.path(), true)
            .into_iter()
            .map(|(_, path)| path);
        let switches = saved_plugin_switches_in_files(files);
        assert_eq!(
            switches.get("sandbox"),
            Some(&false),
            "the plugin switch must come from the same user file as every other setting"
        );
        // And the writer lands in the same file, not in a `settings.json`
        // nobody reads in this mode.
        save_plugin_enabled_in_file(&user_file, "cron", false).expect("write");
        assert!(!dir.path().join("settings.json").exists());
        let cowork = read_settings_json_object(&user_file).unwrap();
        assert_eq!(cowork["plugins"]["cron"]["enabled"], false);
    }

    /// Every layered read in this crate walks the chain through
    /// [`settings_layers_in_files`], so the order and the skip rules are
    /// pinned once here rather than three times over three key shapes.
    #[test]
    fn the_settings_chain_is_read_user_then_project_then_local() {
        let config = tempfile::tempdir().expect("config dir");
        let project = tempfile::tempdir().expect("project dir");
        std::fs::create_dir_all(project.path().join(".rebon")).expect("project .rebon");

        std::fs::write(
            config.path().join("settings.json"),
            r#"{"layer":"user","onlyUser":1}"#,
        )
        .expect("user settings");
        std::fs::write(
            project.path().join(".rebon").join("settings.json"),
            r#"{"layer":"project"}"#,
        )
        .expect("project settings");
        std::fs::write(
            project.path().join(".rebon").join("settings.local.json"),
            r#"{"layer":"local"}"#,
        )
        .expect("local settings");

        let layers = settings_layers_in_files(
            settings_files_for_mode(config.path(), project.path(), false)
                .into_iter()
                .map(|(_, path)| path),
        );
        let seen: Vec<&str> = layers
            .iter()
            .filter_map(|layer| layer.get("layer").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            seen,
            vec!["user", "project", "local"],
            "later files must arrive last so a fold over them is last-wins"
        );
        // A layer that says nothing about a key contributes nothing to it,
        // rather than blanking what an earlier layer said.
        assert_eq!(layers[0].get("onlyUser").and_then(|v| v.as_i64()), Some(1));
        assert!(layers[1].get("onlyUser").is_none());
    }

    /// In cowork mode the user layer is `cowork_settings.json`, for every
    /// key and not just the plugin switches.
    #[test]
    fn the_settings_chain_reads_the_cowork_user_file_in_cowork_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("settings.json"), r#"{"layer":"plain"}"#)
            .expect("plain settings");
        std::fs::write(
            dir.path().join("cowork_settings.json"),
            r#"{"layer":"cowork"}"#,
        )
        .expect("cowork settings");

        let read = |use_cowork| {
            settings_layers_in_files(
                settings_files_for_mode(dir.path(), dir.path(), use_cowork)
                    .into_iter()
                    .map(|(_, path)| path),
            )
            .into_iter()
            .filter_map(|layer| {
                layer
                    .get("layer")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
        };
        assert_eq!(read(true), vec!["cowork".to_string()]);
        assert_eq!(read(false), vec!["plain".to_string()]);
    }

    /// A file that is missing, unreadable, or not a JSON object drops out of
    /// the chain instead of ending it.
    #[test]
    fn an_unreadable_or_malformed_layer_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.json");
        let malformed = dir.path().join("malformed.json");
        let not_an_object = dir.path().join("array.json");
        std::fs::write(&good, r#"{"layer":"good"}"#).expect("good");
        std::fs::write(&malformed, "{not json").expect("malformed");
        std::fs::write(&not_an_object, "[1, 2]").expect("array");

        let layers = settings_layers_in_files(vec![
            dir.path().join("missing.json"),
            malformed,
            not_an_object,
            good,
        ]);
        let seen: Vec<&str> = layers
            .iter()
            .filter_map(|layer| layer.get("layer").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(seen, vec!["good"]);
    }

    /// The `/sandbox` override is one key inside a block the sandbox plugin
    /// owns, so writing it must not take the rest of the block with it.
    #[test]
    fn the_sandbox_override_is_written_without_disturbing_the_rest_of_the_block() {
        let dir = tempfile::tempdir().expect("config dir");
        let target = dir.path().join("settings.json");
        std::fs::write(
            &target,
            r#"{"sandbox":{"enabled":true,"excludedCommands":["git"]},"theme":"dark"}"#,
        )
        .expect("write settings");

        save_sandbox_allow_unsandboxed_commands_in_file(&target, true).expect("write");

        let settings = read_settings_json_object(&target).unwrap();
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], true);
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["excludedCommands"][0], "git");
        assert_eq!(settings["theme"], "dark");

        save_sandbox_allow_unsandboxed_commands_in_file(&target, false).expect("and back");
        let settings = read_settings_json_object(&target).unwrap();
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(settings["sandbox"]["enabled"], true);
    }

    /// A file with no `sandbox` block yet, and one whose `sandbox` is the
    /// wrong shape: both end up with a block holding the one key.
    #[test]
    fn the_sandbox_override_creates_or_repairs_the_block_it_writes_into() {
        let dir = tempfile::tempdir().expect("config dir");
        let fresh = dir.path().join("fresh.json");
        save_sandbox_allow_unsandboxed_commands_in_file(&fresh, true).expect("write");
        assert_eq!(
            read_settings_json_object(&fresh).unwrap()["sandbox"]["allowUnsandboxedCommands"],
            true
        );

        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, r#"{"sandbox":"yes please"}"#).unwrap();
        save_sandbox_allow_unsandboxed_commands_in_file(&broken, false).expect("write");
        assert_eq!(
            read_settings_json_object(&broken).unwrap()["sandbox"]["allowUnsandboxedCommands"],
            false
        );
    }

    /// The override lands in the same user-layer file the plugin switches do,
    /// which in cowork mode is `cowork_settings.json`.
    #[test]
    fn the_sandbox_override_targets_the_user_layer_of_the_chain() {
        let dir = tempfile::tempdir().expect("config dir");
        let cowork = user_settings_file_for_mode(dir.path(), true);
        save_sandbox_allow_unsandboxed_commands_in_file(&cowork, true).expect("write");
        assert!(cowork.ends_with("cowork_settings.json"));
        assert!(!dir.path().join("settings.json").exists());
    }

    #[test]
    fn plugin_switches_follow_the_settings_chain_with_later_files_winning() {
        let config = tempfile::tempdir().expect("config dir");
        let project = tempfile::tempdir().expect("project dir");
        std::fs::write(
            config.path().join("settings.json"),
            r#"{"plugins":{"cron":false,"memory":{"enabled":false},"web":false}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(".rebon")).unwrap();
        std::fs::write(
            project.path().join(".rebon").join("settings.json"),
            r#"{"plugins":{"cron":{"enabled":true}}}"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join(".rebon").join("settings.local.json"),
            r#"{"plugins":{"memory":true,"junk":"yes"}}"#,
        )
        .unwrap();

        let files = settings_files_for_mode(config.path(), project.path(), false)
            .into_iter()
            .map(|(_, path)| path);
        let switches = saved_plugin_switches_in_files(files);
        assert_eq!(switches.get("cron"), Some(&true), "project overrides user");
        assert_eq!(switches.get("memory"), Some(&true), "local overrides user");
        assert_eq!(
            switches.get("web"),
            Some(&false),
            "an id only the user file names survives"
        );
        assert!(!switches.contains_key("junk"));
    }

    #[test]
    fn plugin_switches_round_trip_and_accept_the_bare_bool_form() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(saved_plugin_switches_in(dir.path(), dir.path()).is_empty());

        save_plugin_enabled_in_dir(dir.path(), "cron", false).expect("write");
        save_plugin_enabled_in_dir(dir.path(), "memory", true).expect("write");
        let switches = saved_plugin_switches_in(dir.path(), dir.path());
        assert_eq!(switches.get("cron"), Some(&false));
        assert_eq!(switches.get("memory"), Some(&true));

        // Hand-written shorthand and junk values.
        let target = dir.path().join("settings.json");
        let mut settings = read_settings_json_object(&target).unwrap();
        let plugins = settings
            .get_mut(PLUGINS_CONFIG_KEY)
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        plugins.insert("web".into(), serde_json::Value::Bool(false));
        plugins.insert("junk".into(), serde_json::Value::String("yes".into()));
        write_settings_json_object(&target, &settings).unwrap();
        let switches = saved_plugin_switches_in(dir.path(), dir.path());
        assert_eq!(switches.get("web"), Some(&false));
        assert!(!switches.contains_key("junk"));
        assert_eq!(
            switches.get("cron"),
            Some(&false),
            "existing entries survive"
        );
    }

    #[test]
    fn a_plugin_namespace_merges_per_key_down_the_settings_chain() {
        let config = tempfile::tempdir().expect("config dir");
        let project = tempfile::tempdir().expect("project dir");
        std::fs::write(
            config.path().join("settings.json"),
            r#"{"plugins":{"demo":{"enabled":true,"greeting":"user","retries":3},"other":{"k":1}}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(".rebon")).unwrap();
        std::fs::write(
            project.path().join(".rebon").join("settings.json"),
            r#"{"plugins":{"demo":{"greeting":"project"}}}"#,
        )
        .unwrap();
        // The bare shorthand says only whether the plugin runs, so it must not
        // read as an empty namespace that erases the layers above it.
        std::fs::write(
            project.path().join(".rebon").join("settings.local.json"),
            r#"{"plugins":{"demo":false}}"#,
        )
        .unwrap();

        let files = settings_files_for_mode(config.path(), project.path(), false)
            .into_iter()
            .map(|(_, path)| path);
        let settings = plugin_settings_in_files(files, "demo");
        assert_eq!(
            settings.get("greeting"),
            Some(&serde_json::json!("project"))
        );
        assert_eq!(
            settings.get("retries"),
            Some(&serde_json::json!(3)),
            "a key only the user file sets survives the project layer"
        );
        assert!(
            !settings.contains_key("enabled"),
            "the switch is the kernel's, not the plugin's: {settings:?}"
        );
    }

    #[test]
    fn a_namespace_write_keeps_the_switch_the_neighbours_and_the_untouched_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_plugin_enabled_in_dir(dir.path(), "demo", false).expect("switch");
        save_plugin_enabled_in_dir(dir.path(), "other", true).expect("neighbour switch");
        let patch = |json: serde_json::Value| json.as_object().unwrap().clone();

        save_plugin_settings_in_dir(
            dir.path(),
            "demo",
            &patch(serde_json::json!({ "greeting": "hi", "retries": 3 })),
        )
        .expect("write");
        save_plugin_settings_in_dir(
            dir.path(),
            "demo",
            &patch(serde_json::json!({ "greeting": "hello" })),
        )
        .expect("second write");

        let settings = plugin_settings_in(dir.path(), dir.path(), "demo");
        assert_eq!(settings.get("greeting"), Some(&serde_json::json!("hello")));
        assert_eq!(
            settings.get("retries"),
            Some(&serde_json::json!(3)),
            "a key the patch did not name is untouched"
        );
        let switches = saved_plugin_switches_in(dir.path(), dir.path());
        assert_eq!(switches.get("demo"), Some(&false), "the switch survives");
        assert_eq!(switches.get("other"), Some(&true), "so does the neighbour");

        // Null removes rather than storing a null: how a plugin puts a key back
        // to its default.
        save_plugin_settings_in_dir(
            dir.path(),
            "demo",
            &patch(serde_json::json!({ "retries": serde_json::Value::Null })),
        )
        .expect("removal");
        let settings = plugin_settings_in(dir.path(), dir.path(), "demo");
        assert!(!settings.contains_key("retries"), "{settings:?}");
        assert_eq!(
            saved_plugin_switches_in(dir.path(), dir.path()).get("demo"),
            Some(&false)
        );
    }

    #[test]
    fn a_namespace_write_refuses_the_enabled_switch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let patch = serde_json::json!({ "enabled": true })
            .as_object()
            .unwrap()
            .clone();
        let err = save_plugin_settings_in_dir(dir.path(), "demo", &patch)
            .expect_err("the switch is not the plugin's key");
        assert!(err.to_string().contains("kernel's switch"), "{err}");
        assert!(
            saved_plugin_switches_in(dir.path(), dir.path()).is_empty(),
            "nothing was written"
        );
    }

    /// The shorthand carries exactly one fact; a write beside it must keep it
    /// rather than replace the entry with an object that has lost it.
    #[test]
    fn a_namespace_write_normalises_the_bare_bool_shorthand_without_losing_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("settings.json");
        std::fs::write(&target, r#"{"plugins":{"demo":false}}"#).unwrap();

        save_plugin_settings_in_file(
            &target,
            "demo",
            &serde_json::json!({ "greeting": "hi" })
                .as_object()
                .unwrap()
                .clone(),
        )
        .expect("write");

        assert_eq!(
            saved_plugin_switches_in(dir.path(), dir.path()).get("demo"),
            Some(&false)
        );
        assert_eq!(
            plugin_settings_in(dir.path(), dir.path(), "demo").get("greeting"),
            Some(&serde_json::json!("hi"))
        );
    }

    #[test]
    fn a_config_write_tells_the_installed_observer_which_file_moved() {
        static SEEN: std::sync::Mutex<Vec<(ConfigFileKind, PathBuf, Option<String>)>> =
            std::sync::Mutex::new(Vec::new());
        // One observer per process; whichever test installs first wins and
        // every later write in this process reports to the same sink.
        install_config_change_observer(|kind, path, namespace| {
            SEEN.lock()
                .unwrap()
                .push((kind, path.to_path_buf(), namespace.map(str::to_string)));
        });
        assert!(
            !install_config_change_observer(|_, _, _| {}),
            "a second observer is refused"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        // fastMode lives in config.json, so this is a Config notification.
        save_fast_mode_enabled_in_dir(dir.path(), true).expect("config write");
        // A seat write says which namespace it was about; the switch beside it
        // does not, because a switch is not a plugin's own key.
        save_plugin_settings_in_dir(
            dir.path(),
            "demo",
            &serde_json::json!({ "greeting": "hi" })
                .as_object()
                .unwrap()
                .clone(),
        )
        .expect("settings write");
        save_plugin_enabled_in_dir(dir.path(), "demo", true).expect("switch write");

        let seen = SEEN.lock().unwrap();
        assert!(
            seen.iter()
                .any(|(kind, path, namespace)| *kind == ConfigFileKind::Config
                    && path == &config_json_path(dir.path())
                    && namespace.is_none()),
            "expected a Config notification for {:?}, got {seen:?}",
            config_json_path(dir.path())
        );
        let settings_events: Vec<_> = seen
            .iter()
            .filter(|(kind, path, _)| {
                *kind == ConfigFileKind::Settings && path == &user_settings_file(dir.path())
            })
            .map(|(_, _, namespace)| namespace.clone())
            .collect();
        assert_eq!(
            settings_events,
            vec![Some("demo".to_string()), None],
            "the seat write names its namespace, the switch write does not"
        );
    }
    use std::fs;
    use tempfile::TempDir;

    fn write_config(dir: &Path, json: &str) {
        fs::write(config_json_path(dir), json).unwrap();
    }

    fn write_credentials(dir: &Path, json: &str) {
        fs::write(credentials_json_path(dir), json).unwrap();
    }

    fn provider_profile_fixture(dir: &Path) {
        write_config(
            dir,
            r#"{
                "activeCustomProvider":"vendor",
                "customProviders":[
                    {"name":"vendor","format":"openai","baseUrl":"https://example.com","apiKey":"sk",
                     "model":"vendor-pro",
                     "models":["vendor-pro","vendor-flash"],
                     "modelProfiles":{"general":{"model":"vendor-pro","reasoningEffort":"xhigh"}}}
                ]
            }"#,
        );
    }

    fn profile_row<'a>(rows: &'a [ProviderProfileEntry], role: &str) -> &'a ProviderProfileEntry {
        rows.iter()
            .find(|row| row.role == role)
            .expect("role is in the known list")
    }

    fn set_profile(
        dir: &Path,
        role: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> anyhow::Result<Vec<ProviderProfileEntry>> {
        set_custom_provider_profile_in(dir, "vendor", role, model, effort)
    }

    #[test]
    fn provider_profiles_report_every_known_role_and_mark_undeclared_ones() {
        let tmp = TempDir::new().unwrap();
        provider_profile_fixture(tmp.path());

        let rows = custom_provider_profiles_in(tmp.path(), "vendor").unwrap();

        assert_eq!(rows.len(), PROVIDER_PROFILE_ROLES.len());
        assert_eq!(
            profile_row(&rows, "general").model.as_deref(),
            Some("vendor-pro")
        );
        assert_eq!(
            profile_row(&rows, "general").reasoning_effort.as_deref(),
            Some("xhigh")
        );
        // Undeclared is the normal state, reported as `None` rather than
        // borrowed from `general`.
        assert_eq!(profile_row(&rows, "small").model, None);
        assert_eq!(profile_row(&rows, "reviewer").model, None);
    }

    #[test]
    fn setting_and_clearing_a_provider_profile_round_trips_to_disk() {
        let tmp = TempDir::new().unwrap();
        provider_profile_fixture(tmp.path());

        let rows = set_profile(tmp.path(), "small", Some("vendor-flash"), None).unwrap();
        assert_eq!(
            profile_row(&rows, "small").model.as_deref(),
            Some("vendor-flash")
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.model_profiles.get("small"), Some("vendor-flash"));

        // Clearing puts the role back to following the session's model, and
        // that is an absence on disk — not a stored sentinel.
        let rows = set_profile(tmp.path(), "small", None, None).unwrap();
        assert_eq!(profile_row(&rows, "small").model, None);
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.model_profiles.get("small"), None);
        assert_eq!(
            resolve_model_profile(Some(&resolved), "small", "vendor-flash"),
            "vendor-flash"
        );
    }

    #[test]
    fn setting_a_provider_profile_rejects_bad_input() {
        let tmp = TempDir::new().unwrap();
        provider_profile_fixture(tmp.path());

        // Unknown role — a typo must not silently create a dead entry.
        assert!(set_profile(tmp.path(), "smal", Some("vendor-flash"), None).is_err());
        // The `default` sentinel is a UI word, never a model id.
        assert!(set_profile(tmp.path(), "small", Some("default"), None).is_err());
        // An effort with nothing to attach it to.
        assert!(set_profile(tmp.path(), "small", None, Some("high")).is_err());
        assert!(set_profile(tmp.path(), "small", Some("x"), Some("turbo")).is_err());
        assert!(set_custom_provider_profile_in(tmp.path(), "ghost", "small", None, None).is_err());

        // None of the rejects touched the file.
        let rows = custom_provider_profiles_in(tmp.path(), "vendor").unwrap();
        assert_eq!(profile_row(&rows, "small").model, None);
        assert_eq!(
            profile_row(&rows, "general").model.as_deref(),
            Some("vendor-pro")
        );
    }

    #[test]
    fn missing_config_uses_empty_disabled_skills() {
        let tmp = TempDir::new().unwrap();
        assert!(load_disabled_skills_in(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn disabled_skills_round_trip_sorted_deduplicated_and_trimmed() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"disabledSkills":["  zeta  ","alpha","zeta","","   "]}"#,
        );
        assert_eq!(
            load_disabled_skills_in(tmp.path()).unwrap(),
            BTreeSet::from(["alpha".to_string(), "zeta".to_string()])
        );

        save_disabled_skills_in(tmp.path(), ["  zeta  ", "alpha", "zeta", "", "   "]).unwrap();

        assert_eq!(
            load_disabled_skills_in(tmp.path()).unwrap(),
            BTreeSet::from(["alpha".to_string(), "zeta".to_string()])
        );
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(
            config[DISABLED_SKILLS_CONFIG_KEY],
            serde_json::json!(["alpha", "zeta"])
        );
    }

    #[test]
    fn rc_projects_are_paths_or_labelled_entries() {
        let tmp = TempDir::new().unwrap();
        assert!(load_rc_projects_in(tmp.path()).unwrap().is_empty());
        write_config(tmp.path(), r#"{"rc": {"server": "https://rc"}}"#);
        assert!(load_rc_projects_in(tmp.path()).unwrap().is_empty());
        let absolute = tmp.path().join("abs").to_string_lossy().into_owned();
        write_config(
            tmp.path(),
            &serde_json::json!({
                "rc": {"projects": [
                    absolute,
                    {"path": " work/app ", "label": "App"},
                    {"path": "docs", "label": null}
                ]}
            })
            .to_string(),
        );
        let projects = load_rc_projects_in(tmp.path()).unwrap();
        assert_eq!(
            projects,
            vec![
                RcProjectConfig {
                    path: absolute.clone(),
                    label: None
                },
                RcProjectConfig {
                    path: tmp.path().join("work/app").to_string_lossy().into_owned(),
                    label: Some("App".into())
                },
                RcProjectConfig {
                    path: tmp.path().join("docs").to_string_lossy().into_owned(),
                    label: None
                },
            ]
        );
    }

    #[test]
    fn a_malformed_rc_project_fails_the_whole_list() {
        let tmp = TempDir::new().unwrap();
        for bad in [
            r#"{"rc": {"projects": "/a"}}"#,
            r#"{"rc": {"projects": [7]}}"#,
            r#"{"rc": {"projects": ["/a", ""]}}"#,
            r#"{"rc": {"projects": [{"label": "x"}]}}"#,
            r#"{"rc": {"projects": [{"path": "/a", "label": 3}]}}"#,
        ] {
            write_config(tmp.path(), bad);
            assert!(load_rc_projects_in(tmp.path()).is_err(), "{bad}");
        }
    }

    #[test]
    fn missing_config_has_no_acp_agents_and_no_active_one() {
        let tmp = TempDir::new().unwrap();
        assert!(load_acp_agents_in(tmp.path()).unwrap().is_empty());
        assert!(active_acp_agent_in(tmp.path()).is_none());
    }

    #[test]
    fn acp_agents_parse_with_defaults_and_label_fallback() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "acpAgents":[
                    {"id":"  claude-code  ","displayName":"Claude Code",
                     "command":" claude ","args":["--acp"],
                     "env":{"ANTHROPIC_API_KEY":"$ACP_TEST_KEY"},
                     "cwd":"/tmp/work"},
                    {"id":"gemini","command":"gemini"}
                ]
            }"#,
        );
        let agents = load_acp_agents_in(tmp.path()).unwrap();
        assert_eq!(agents.len(), 2);

        // Ids and commands are trimmed — a stray space in config.json
        // must not produce an agent nobody can select or spawn.
        assert_eq!(agents[0].id, "claude-code");
        assert_eq!(agents[0].command, "claude");
        assert_eq!(agents[0].args, vec!["--acp".to_string()]);
        assert_eq!(agents[0].cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(agents[0].label(), "Claude Code");

        // Everything but id and command is optional.
        assert!(agents[1].args.is_empty());
        assert!(agents[1].env.is_empty());
        assert_eq!(agents[1].label(), "gemini", "label falls back to the id");
    }

    #[test]
    fn fs_tool_injection_defaults_on_and_is_opt_out() {
        // Injection is what keeps `/rewind` meaningful for an agent
        // that writes disk itself, so absence must mean yes.
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"acpAgents":[
                {"id":"a","command":"a"},
                {"id":"b","command":"b","injectFsTools":false},
                {"id":"c","command":"c","injectFsTools":true}
            ]}"#,
        );
        let agents = load_acp_agents_in(tmp.path()).unwrap();
        assert!(agents[0].injects_fs_tools(), "omitted means injected");
        assert!(!agents[1].injects_fs_tools());
        assert!(agents[2].injects_fs_tools());

        // The tri-state survives a round trip: an omitted flag stays
        // omitted rather than being materialised as `true`.
        assert_eq!(agents[0].inject_fs_tools, None);
        let encoded = serde_json::to_value(&agents[0]).unwrap();
        assert!(encoded.get("injectFsTools").is_none());
        let encoded = serde_json::to_value(&agents[1]).unwrap();
        assert_eq!(encoded["injectFsTools"], false);
    }

    #[test]
    fn session_meta_parses_as_a_free_form_object_and_round_trips() {
        // The verified claude-agent-acp recipe: disallow the agent's
        // own edit tools so Rebon's injected ones get used.
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"acpAgents":[{
                "id":"claude","command":"claude-agent-acp",
                "sessionMeta":{"claudeCode":{"options":{"disallowedTools":["Write","Edit"]}}}
            },{"id":"bare","command":"b"}]}"#,
        );
        let agents = load_acp_agents_in(tmp.path()).unwrap();
        let meta = agents[0].session_meta.as_ref().expect("configured");
        assert_eq!(
            meta["claudeCode"]["options"]["disallowedTools"],
            serde_json::json!(["Write", "Edit"])
        );
        assert!(agents[1].session_meta.is_none());

        let encoded = serde_json::to_value(&agents[0]).unwrap();
        assert!(encoded["sessionMeta"]["claudeCode"].is_object());
        let encoded = serde_json::to_value(&agents[1]).unwrap();
        assert!(encoded.get("sessionMeta").is_none());
    }

    #[test]
    fn acp_agent_env_resolves_variable_references_at_read_time() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("ACP_TEST_KEY", "sk-from-env");
        write_config(
            tmp.path(),
            r#"{"acpAgents":[{"id":"a","command":"c",
                "env":{"KEY":"$ACP_TEST_KEY","BRACED":"${ACP_TEST_KEY}","LITERAL":"plain"}}]}"#,
        );
        let agents = load_acp_agents_in(tmp.path()).unwrap();
        let env = agents[0].resolved_env();
        assert_eq!(env["KEY"], "sk-from-env");
        assert_eq!(env["BRACED"], "sk-from-env");
        assert_eq!(env["LITERAL"], "plain");
        // The stored form keeps the reference so the secret stays out
        // of config.json.
        assert_eq!(agents[0].env["KEY"], "$ACP_TEST_KEY");
        std::env::remove_var("ACP_TEST_KEY");
    }

    #[test]
    fn acp_agents_reject_duplicate_reserved_and_incomplete_entries() {
        let cases = [
            (
                r#"{"acpAgents":[{"id":"dup","command":"a"},{"id":"DUP","command":"b"}]}"#,
                "more than one entry",
            ),
            (
                r#"{"acpAgents":[{"id":"local","command":"a"}]}"#,
                "reserved",
            ),
            (
                r#"{"acpAgents":[{"id":"  ","command":"a"}]}"#,
                "must not be empty",
            ),
            (
                r#"{"acpAgents":[{"id":"a","command":"   "}]}"#,
                "must not be empty",
            ),
            (r#"{"acpAgents":[{"id":"a"}]}"#, "must be an array"),
            (r#"{"acpAgents":"claude"}"#, "must be an array"),
        ];
        for (config, expected) in cases {
            let tmp = TempDir::new().unwrap();
            write_config(tmp.path(), config);
            let err = load_acp_agents_in(tmp.path())
                .expect_err("must not accept an unusable agent list")
                .to_string();
            assert!(
                err.contains(expected),
                "error for {config} should mention `{expected}`, got: {err}"
            );
        }
    }

    #[test]
    fn active_acp_agent_round_trips_and_preserves_other_config() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"theme":"dark","acpAgents":[{"id":"claude-code","command":"claude"}]}"#,
        );

        save_active_acp_agent_in(tmp.path(), Some("  claude-code  ")).unwrap();
        assert_eq!(
            active_acp_agent_in(tmp.path()).as_deref(),
            Some("claude-code")
        );
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["acpAgents"][0]["id"], "claude-code");

        // The agent list survives a write that only touches the
        // selection.
        assert_eq!(load_acp_agents_in(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn selecting_local_clears_the_stored_agent() {
        // `local` is not an agent — it is the absence of one, so it is
        // stored as an absent key rather than as a name nothing
        // resolves.
        let tmp = TempDir::new().unwrap();
        save_active_acp_agent_in(tmp.path(), Some("claude-code")).unwrap();
        for cleared in [Some(LOCAL_AGENT_ID), Some("  "), None] {
            save_active_acp_agent_in(tmp.path(), cleared).unwrap();
            assert!(active_acp_agent_in(tmp.path()).is_none(), "{cleared:?}");
            let config: serde_json::Value =
                serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
            assert!(config.get(ACTIVE_ACP_AGENT_CONFIG_KEY).is_none());
            save_active_acp_agent_in(tmp.path(), Some("claude-code")).unwrap();
        }
    }

    #[test]
    fn an_active_agent_that_no_longer_exists_is_still_reported() {
        // Reported, not silently dropped: the caller needs to tell the
        // user their configured agent is gone instead of quietly
        // starting the session somewhere else.
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"activeAcpAgent":"deleted-agent","acpAgents":[]}"#,
        );
        assert_eq!(
            active_acp_agent_in(tmp.path()).as_deref(),
            Some("deleted-agent")
        );
        assert!(load_acp_agents_in(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn saving_disabled_skills_preserves_unknown_config_fields() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"futureSetting":{"enabled":true},"anotherKey":"keep-me"}"#,
        );

        save_disabled_skills_in(tmp.path(), ["review", "commit"]).unwrap();

        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(
            config["futureSetting"],
            serde_json::json!({ "enabled": true })
        );
        assert_eq!(config["anotherKey"], "keep-me");
        assert_eq!(
            config[DISABLED_SKILLS_CONFIG_KEY],
            serde_json::json!(["commit", "review"])
        );
    }

    #[test]
    fn saving_empty_disabled_skills_omits_the_field() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"disabledSkills":["commit"],"unrelated":42}"#,
        );

        save_disabled_skills_in(tmp.path(), std::iter::empty::<&str>()).unwrap();

        assert!(load_disabled_skills_in(tmp.path()).unwrap().is_empty());
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert!(config.get(DISABLED_SKILLS_CONFIG_KEY).is_none());
        assert_eq!(config["unrelated"], 42);
    }

    #[test]
    fn missing_config_uses_default_update_preferences() {
        let tmp = TempDir::new().unwrap();
        let prefs = load_update_preferences_in(tmp.path()).unwrap();
        assert_eq!(prefs, UpdatePreferences::default());
        assert!(!prefs.auto_install);
    }

    #[test]
    fn write_and_read_update_preferences() {
        let tmp = TempDir::new().unwrap();
        let prefs = UpdatePreferences {
            disabled: true,
            auto_install: true,
            channel: Some("stable".to_string()),
            skipped_version: Some("1.2.3".to_string()),
            dismissed_version: Some("1.2.4".to_string()),
            dismissed_at_ms: Some(42),
        };
        save_update_preferences_in(tmp.path(), &prefs).unwrap();
        assert_eq!(load_update_preferences_in(tmp.path()).unwrap(), prefs);

        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(config["updates"]["disabled"], true);
        assert_eq!(config["updates"]["autoInstall"], true);
        assert_eq!(config["updates"]["channel"], "stable");
        assert_eq!(config["updates"]["skippedVersion"], "1.2.3");
        assert_eq!(config["updates"]["dismissedVersion"], "1.2.4");
        assert_eq!(config["updates"]["dismissedAtMs"], 42);
    }

    #[test]
    fn save_ui_mode_writes_user_settings() {
        let tmp = TempDir::new().unwrap();

        save_ui_mode_in_dir(tmp.path(), "inline").unwrap();

        assert!(!config_json_path(tmp.path()).exists());
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["uiMode"], "inline");
    }

    #[test]
    fn math_rendering_mode_defaults_and_wire_values_are_stable() {
        assert_eq!(MathRenderingMode::default(), MathRenderingMode::Off);
        for (wire, mode) in [
            ("off", MathRenderingMode::Off),
            ("unicode", MathRenderingMode::Unicode),
            ("graphics-auto", MathRenderingMode::GraphicsAuto),
        ] {
            assert_eq!(wire.parse::<MathRenderingMode>().unwrap(), mode);
            assert_eq!(mode.to_string(), wire);
            assert_eq!(serde_json::to_value(mode).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<MathRenderingMode>(serde_json::json!(wire)).unwrap(),
                mode
            );
        }
        assert!("graphics".parse::<MathRenderingMode>().is_err());
        assert!(
            serde_json::from_value::<MathRenderingMode>(serde_json::json!("graphics")).is_err()
        );
    }

    #[test]
    fn save_math_rendering_mode_preserves_other_user_settings() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("settings.json"),
            r#"{"model":"gpt-test","nested":{"keep":true},"mathRendering":"off"}"#,
        )
        .unwrap();

        save_math_rendering_mode_in_dir(tmp.path(), MathRenderingMode::GraphicsAuto).unwrap();

        assert!(!config_json_path(tmp.path()).exists());
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["mathRendering"], "graphics-auto");
        assert_eq!(settings["model"], "gpt-test");
        assert_eq!(settings["nested"]["keep"], true);
    }

    #[test]
    fn save_math_rendering_mode_can_target_cowork_settings() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cowork_settings.json");
        fs::write(&target, r#"{"model":"gpt-test","mathRendering":"off"}"#).unwrap();

        save_math_rendering_mode_in_file(&target, MathRenderingMode::Unicode).unwrap();

        assert!(!tmp.path().join("settings.json").exists());
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(target).unwrap()).unwrap();
        assert_eq!(settings["mathRendering"], "unicode");
        assert_eq!(settings["model"], "gpt-test");
    }

    #[test]
    fn shell_tool_is_unset_until_the_user_chooses_one() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(saved_shell_tool_in_dir(tmp.path()), None);
    }

    #[test]
    fn shell_tool_round_trips_and_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        save_sub_agents_enabled_in_dir(tmp.path(), false).unwrap();

        save_shell_tool_in_dir(tmp.path(), "powershell").unwrap();
        assert_eq!(
            saved_shell_tool_in_dir(tmp.path()).as_deref(),
            Some("powershell")
        );
        assert!(!saved_sub_agents_enabled_in_dir(tmp.path()));

        save_shell_tool_in_dir(tmp.path(), " bash ").unwrap();
        assert_eq!(saved_shell_tool_in_dir(tmp.path()).as_deref(), Some("bash"));
    }

    /// An empty string is the same as never having chosen — otherwise the
    /// settings UI would show a selected-but-blank option.
    #[test]
    fn a_blank_shell_tool_reads_as_unset() {
        let tmp = TempDir::new().unwrap();
        save_shell_tool_in_dir(tmp.path(), "   ").unwrap();
        assert_eq!(saved_shell_tool_in_dir(tmp.path()), None);
    }

    #[test]
    fn user_model_effort_and_permission_defaults_are_user_settings() {
        let tmp = TempDir::new().unwrap();

        save_user_model_in_dir(tmp.path(), Some("gpt-5.5")).unwrap();
        save_effort_level_in_dir(tmp.path(), Some("xhigh")).unwrap();
        save_default_permission_mode_in_dir(tmp.path(), PermissionMode::AcceptEdits).unwrap();

        assert!(!config_json_path(tmp.path()).exists());
        assert_eq!(
            saved_user_model_in_dir(tmp.path()).as_deref(),
            Some("gpt-5.5")
        );
        assert_eq!(
            saved_effort_level_in_dir(tmp.path()).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            saved_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::AcceptEdits)
        );

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["model"], "gpt-5.5");
        assert_eq!(settings["effortLevel"], "xhigh");
        assert_eq!(settings["permissions"]["defaultMode"], "acceptEdits");
    }

    #[test]
    fn user_model_and_effort_can_be_cleared() {
        let tmp = TempDir::new().unwrap();
        save_user_model_in_dir(tmp.path(), Some("gpt-5.5")).unwrap();
        save_effort_level_in_dir(tmp.path(), Some("high")).unwrap();

        save_user_model_in_dir(tmp.path(), None).unwrap();
        save_effort_level_in_dir(tmp.path(), None).unwrap();

        assert_eq!(saved_user_model_in_dir(tmp.path()), None);
        assert_eq!(saved_effort_level_in_dir(tmp.path()), None);
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert!(settings.get("model").is_none());
        assert!(settings.get("effortLevel").is_none());
    }

    #[test]
    fn persist_model_choice_targets_active_provider_and_clears_global_override() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "jun",
            "openai",
            "https://jun",
            "key",
            "grok-4.5",
        )
        .unwrap();
        set_active_custom_provider_in(tmp.path(), "jun").unwrap();
        save_user_model_in_dir(tmp.path(), Some("gpt-5.6-sol")).unwrap();

        let info = persist_model_config_choice_in(tmp.path(), "grok-4.6")
            .unwrap()
            .expect("active provider info");

        assert_eq!(info.name, "jun");
        assert_eq!(info.model, "grok-4.6");
        // The provider entry is the source of truth; the global
        // override must be gone so it cannot shadow provider switches.
        assert_eq!(saved_user_model_in_dir(tmp.path()), None);
        let stored = list_custom_providers_from(tmp.path())
            .into_iter()
            .find(|provider| provider.name == "jun")
            .unwrap();
        assert_eq!(stored.model, "grok-4.6");
    }

    #[test]
    fn persist_model_choice_default_keeps_provider_model_and_clears_override() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "jun",
            "openai",
            "https://jun",
            "key",
            "grok-4.5",
        )
        .unwrap();
        set_active_custom_provider_in(tmp.path(), "jun").unwrap();
        save_user_model_in_dir(tmp.path(), Some("gpt-5.6-sol")).unwrap();

        let info = persist_model_config_choice_in(tmp.path(), "default")
            .unwrap()
            .expect("active provider info");

        assert_eq!(info.model, "grok-4.5");
        assert_eq!(saved_user_model_in_dir(tmp.path()), None);
        // "default" must never be stored as a literal model id.
        let stored = list_custom_providers_from(tmp.path())
            .into_iter()
            .find(|provider| provider.name == "jun")
            .unwrap();
        assert_eq!(stored.model, "grok-4.5");
        assert!(!stored.models.iter().any(|model| model == "default"));
    }

    #[test]
    fn persist_model_choice_without_active_provider_saves_global_user_model() {
        let tmp = TempDir::new().unwrap();

        assert!(persist_model_config_choice_in(tmp.path(), "gpt-5.6-sol")
            .unwrap()
            .is_none());
        assert_eq!(
            saved_user_model_in_dir(tmp.path()).as_deref(),
            Some("gpt-5.6-sol")
        );

        assert!(persist_model_config_choice_in(tmp.path(), "default")
            .unwrap()
            .is_none());
        assert_eq!(saved_user_model_in_dir(tmp.path()), None);
    }

    #[test]
    fn auto_default_permission_mode_round_trips_canonical_user_settings() {
        let tmp = TempDir::new().unwrap();

        save_default_permission_mode_in_dir(tmp.path(), PermissionMode::Auto).unwrap();

        assert_eq!(
            saved_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::Auto)
        );
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "auto");
        assert!(settings.get("defaultPermissionMode").is_none());
    }

    #[test]
    fn plan_mode_does_not_replace_persisted_default_permission_mode() {
        let tmp = TempDir::new().unwrap();
        save_default_permission_mode_in_dir(tmp.path(), PermissionMode::Auto).unwrap();

        save_default_permission_mode_in_dir(tmp.path(), PermissionMode::Plan).unwrap();

        assert_eq!(
            saved_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::Auto)
        );
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "auto");
    }

    #[test]
    fn bypass_mode_does_not_replace_persisted_default_permission_mode() {
        let tmp = TempDir::new().unwrap();
        save_default_permission_mode_in_dir(tmp.path(), PermissionMode::Auto).unwrap();

        save_default_permission_mode_in_dir(tmp.path(), PermissionMode::BypassPermissions).unwrap();

        assert_eq!(
            saved_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::Auto)
        );
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "auto");
    }

    #[test]
    fn app_persistence_round_trips_bypass_default_mode() {
        let tmp = TempDir::new().unwrap();

        save_app_default_permission_mode_in_dir(tmp.path(), PermissionMode::BypassPermissions)
            .unwrap();

        assert_eq!(
            saved_app_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::BypassPermissions)
        );
        // The CLI-facing reader still ignores bypass, so TUI/ACP never inherit
        // a default only an app front end offers.
        assert_eq!(saved_default_permission_mode_in_dir(tmp.path()), None);

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
    }

    #[test]
    fn app_persistence_keeps_plan_session_scoped() {
        let tmp = TempDir::new().unwrap();
        save_app_default_permission_mode_in_dir(tmp.path(), PermissionMode::Auto).unwrap();

        save_app_default_permission_mode_in_dir(tmp.path(), PermissionMode::Plan).unwrap();

        assert_eq!(
            saved_app_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::Auto)
        );
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "auto");
    }

    #[test]
    fn app_persistence_reads_the_snake_case_default_permission_mode_key() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("settings.json"),
            r#"{"permissions":{"default_mode":"bypassPermissions"}}"#,
        )
        .unwrap();
        assert_eq!(
            saved_app_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::BypassPermissions)
        );
    }

    #[test]
    fn a_persisted_plan_launch_default_is_ignored() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("settings.json"),
            r#"{"permissions":{"defaultMode":"plan"}}"#,
        )
        .unwrap();

        assert_eq!(saved_default_permission_mode_in_dir(tmp.path()), None);
    }

    /// Older builds could persist a session-level bypass switch as the launch
    /// default. Ignore that stale value so future sessions return to a safe default.
    #[test]
    fn a_persisted_bypass_launch_default_is_ignored() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("settings.json"),
            r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();

        assert_eq!(saved_default_permission_mode_in_dir(tmp.path()), None);
    }

    #[test]
    fn reads_the_snake_case_default_permission_mode_key() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("settings.json"),
            r#"{"permissions":{"default_mode":"acceptEdits"}}"#,
        )
        .unwrap();
        assert_eq!(
            saved_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::AcceptEdits)
        );

        fs::write(
            tmp.path().join("settings.json"),
            r#"{"defaultPermissionMode":"auto"}"#,
        )
        .unwrap();
        assert_eq!(
            saved_default_permission_mode_in_dir(tmp.path()),
            Some(PermissionMode::Auto)
        );
    }

    #[test]
    fn claude_codex_fallback_seeds_enabled_for_installs_predating_the_flag() {
        let tmp = TempDir::new().unwrap();
        // An install that predates the flag: a config exists, but carries no
        // `claudeCodexFallbackEnabled` key.
        write_config(tmp.path(), r#"{"activeCustomProvider":"kimi"}"#);
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));

        assert_eq!(
            migrate_claude_codex_fallback_default_in_dir(tmp.path()).unwrap(),
            Some(true)
        );
        assert!(saved_claude_codex_fallback_enabled_in_dir(tmp.path()));

        // Unrelated keys survive the seeding.
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(config["activeCustomProvider"], "kimi");
    }

    #[test]
    fn claude_codex_fallback_anchors_fresh_installs_opted_out() {
        let tmp = TempDir::new().unwrap();
        // First run: no config.json yet, so the flag is anchored disabled.
        assert_eq!(
            migrate_claude_codex_fallback_default_in_dir(tmp.path()).unwrap(),
            Some(false)
        );
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));

        // Onboarding then writes the rest of the config the same read-modify-write
        // way the real flow does. The anchor is what stops the *second* startup
        // from reading this as an upgrade.
        let mut config = read_config_roundtrip(tmp.path()).unwrap();
        config.extra.insert(
            "hasCompletedOnboarding".to_string(),
            serde_json::Value::Bool(true),
        );
        write_config_roundtrip(tmp.path(), &config).unwrap();
        assert_eq!(
            migrate_claude_codex_fallback_default_in_dir(tmp.path()).unwrap(),
            None
        );
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn claude_codex_fallback_seeding_never_overwrites_an_explicit_choice() {
        let tmp = TempDir::new().unwrap();
        save_claude_codex_fallback_enabled_in_dir(tmp.path(), false).unwrap();

        assert_eq!(
            migrate_claude_codex_fallback_default_in_dir(tmp.path()).unwrap(),
            None
        );
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn write_and_read_agent_view_preferences() {
        let tmp = TempDir::new().unwrap();
        let prefs = AgentViewPreferences {
            grouping: "directory".to_string(),
            disabled: false,
        };
        save_agent_view_preferences_in(tmp.path(), &prefs).unwrap();
        assert_eq!(load_agent_view_preferences_in(tmp.path()).unwrap(), prefs);

        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(config["agentView"]["grouping"], "directory");
    }

    #[test]
    fn agent_view_preferences_accept_disable_agent_view_alias() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"agentView":{"grouping":"directory","disableAgentView":true}}"#,
        );

        let prefs = load_agent_view_preferences_in(tmp.path()).unwrap();
        assert_eq!(prefs.grouping, "directory");
        assert!(prefs.disabled);
    }

    #[test]
    fn background_permission_mode_acceptance_defaults_to_safe_modes_only() {
        let tmp = TempDir::new().unwrap();

        assert!(background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::AcceptEdits
        ));
        assert!(background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::Default
        ));
        assert!(background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::Plan
        ));
        assert!(!background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::Auto
        ));
        assert!(!background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::BypassPermissions
        ));
        assert!(
            ensure_background_permission_mode_allowed_in(tmp.path(), PermissionMode::Auto).is_err()
        );
    }

    #[test]
    fn background_permission_mode_wire_acceptance_is_strict_and_scoped() {
        let tmp = TempDir::new().unwrap();

        mark_background_permission_mode_accepted_wire_in(tmp.path(), "auto").unwrap();
        assert!(background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::Auto
        ));

        mark_background_permission_mode_accepted_wire_in(tmp.path(), "default").unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(
            config["agentView"]["acceptedBackgroundPermissionModes"],
            serde_json::json!(["auto"])
        );

        let error =
            mark_background_permission_mode_accepted_wire_in(tmp.path(), "automatic").unwrap_err();
        assert!(error.to_string().contains("unknown permission mode"));
    }

    #[test]
    fn mark_background_permission_mode_accepted_preserves_agent_view_config() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "theme":"dark",
                "agentView":{"grouping":"directory","unknown":true}
            }"#,
        );

        mark_background_permission_mode_accepted_in(tmp.path(), PermissionMode::Auto).unwrap();
        mark_background_permission_mode_accepted_in(tmp.path(), PermissionMode::Auto).unwrap();

        assert!(background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::Auto
        ));
        assert!(!background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::BypassPermissions
        ));
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["agentView"]["grouping"], "directory");
        assert_eq!(config["agentView"]["unknown"], true);
        assert_eq!(
            config["agentView"]["acceptedBackgroundPermissionModes"],
            serde_json::json!(["auto"])
        );
    }

    #[test]
    fn background_permission_mode_acceptance_ignores_unknown_values() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "agentView":{
                    "acceptedBackgroundPermissionModes":[
                        "auto",
                        "acceptEdits",
                        "bypasspermissions",
                        42,
                        "bypassPermissions"
                    ]
                }
            }"#,
        );

        assert!(background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::Auto
        ));
        assert!(background_permission_mode_is_accepted_in(
            tmp.path(),
            PermissionMode::BypassPermissions
        ));
        assert!(ensure_background_permission_mode_allowed_in(
            tmp.path(),
            PermissionMode::BypassPermissions
        )
        .is_ok());
    }

    #[test]
    fn update_preferences_preserve_unrelated_config_and_update_fields() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "theme":"dark",
                "updates":{"unknown":true,"channel":"latest"},
                "customProviders":[]
            }"#,
        );

        persist_update_dismissal_in(tmp.path(), "9.9.9", 123456).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_json_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["updates"]["unknown"], true);
        assert_eq!(config["updates"]["autoInstall"], false);
        assert_eq!(config["updates"]["channel"], "latest");
        assert_eq!(config["updates"]["dismissedVersion"], "9.9.9");
        assert_eq!(config["updates"]["dismissedAtMs"], 123456);
    }

    #[test]
    fn missing_config_dir_returns_none_so_caller_can_fall_back_to_env_vars() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_from_dir(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn config_without_active_provider_returns_none() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"customProviders":[]}"#);
        let result = resolve_from_dir(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn missing_active_provider_entry_errors_with_clear_hint() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"activeCustomProvider":"ghost","customProviders":[]}"#,
        );
        let err = resolve_from_dir(tmp.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("ghost"),
            "error should mention missing provider name, got: {msg}"
        );
        assert!(msg.contains("customProviders"));
    }

    #[test]
    fn resolves_literal_api_key_provider_without_touching_credentials() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"rightcodes",
                "customProviders":[
                    {"name":"rightcodes","format":"openai-responses",
                     "baseUrl":"https://right.codes/codex/v1",
                     "apiKey":"sk-literal","model":"gpt-5.4"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.name, "rightcodes");
        assert_eq!(resolved.api_key, "sk-literal");
        assert_eq!(resolved.base_url, "https://right.codes/codex/v1");
        assert_eq!(resolved.model, "gpt-5.4");
        assert!(resolved.model_profiles.is_empty());
        // No profile declared: the side-request follows the model the session
        // is running, not the provider entry's own `model`.
        assert_eq!(
            resolve_model_profile(Some(&resolved), "small", "runtime-model"),
            "runtime-model"
        );
        assert_eq!(
            resolve_model_profile(Some(&resolved), "small", ""),
            "gpt-5.4"
        );
        assert!(matches!(resolved.format, ProviderFormat::OpenaiResponses));
        assert!(matches!(
            resolved.provider_selection,
            ProviderSelection::BuiltIn(ProviderFormat::OpenaiResponses)
        ));
        assert!(resolved.oauth.is_none());
    }

    #[test]
    fn plugin_provider_id_survives_registry_aware_config_resolution() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"fake-plugin",
                "customProviders":[
                    {"name":"fake-plugin","format":"openai",
                     "baseUrl":"","apiKey":"","model":"acme-default"}
                ]
            }"#,
        );

        let resolved = resolve_from_dir_with_external_provider_ids(
            tmp.path(),
            None,
            std::iter::once("fake-plugin"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved.name, "fake-plugin");
        assert_eq!(resolved.model, "acme-default");
        assert!(matches!(resolved.format, ProviderFormat::Openai));
        assert!(matches!(
            resolved.provider_selection,
            ProviderSelection::External(ref id) if id == "fake-plugin"
        ));
    }

    #[test]
    fn provider_model_profiles_parse_roundtrip_and_resolve_fallbacks() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"openai",
                "customProviders":[
                    {"name":"openai","format":"openai-responses",
                     "baseUrl":"https://example.com","apiKey":"sk","model":"gpt-5.5",
                     "modelProfiles":{
                       "general":"gpt-5.5",
                       "small":"gpt-5.4-nano",
                       "reasoning":"gpt-5.5-reasoning"
                     }}
                ]
            }"#,
        );

        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.model_profiles.get("general"), Some("gpt-5.5"));
        assert_eq!(resolved.model_profiles.get("small"), Some("gpt-5.4-nano"));
        assert_eq!(resolved.model_profiles.get("explore"), None);
        assert_eq!(
            resolved.model_profiles.get_reasoning_effort("explore"),
            None
        );
        assert_eq!(
            resolve_model_profile(Some(&resolved), "small", "runtime-main"),
            "gpt-5.4-nano"
        );
        assert_eq!(
            resolve_model_profile(Some(&resolved), "librarian", "runtime-main"),
            "gpt-5.4-nano"
        );
        assert_eq!(
            resolve_model_profile(Some(&resolved), "reviewer", "runtime-main"),
            "gpt-5.5-reasoning"
        );

        let roundtrip = read_config_roundtrip(tmp.path()).unwrap();
        let json = serde_json::to_value(&roundtrip).unwrap();
        assert_eq!(
            json["customProviders"][0]["modelProfiles"]["small"],
            "gpt-5.4-nano"
        );
        assert_eq!(
            json["customProviders"][0]["modelProfiles"]["reasoning"],
            "gpt-5.5-reasoning"
        );
    }

    #[test]
    fn model_profile_resolution_falls_back_to_the_runtime_model() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"openai",
                "customProviders":[
                    {"name":"openai","format":"openai","baseUrl":"https://example.com","apiKey":"sk","model":"gpt-main"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(
            resolve_model_profile(Some(&resolved), "unknown", "runtime-main"),
            "runtime-main"
        );
        // Only a caller with no runtime model falls through to the entry.
        assert_eq!(
            resolve_model_profile(Some(&resolved), "unknown", ""),
            "gpt-main"
        );
        assert_eq!(
            resolve_model_profile(None, "small", "runtime-main"),
            "runtime-main"
        );
    }

    #[test]
    fn a_declared_general_no_longer_captures_the_side_request_profiles() {
        // The reported shape: `general` pinned to the expensive model, the
        // session switched to the cheap one. Titles, the auto-mode classifier,
        // background summaries and compaction all resolve `small`.
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"vendor",
                "customProviders":[
                    {"name":"vendor","format":"openai","baseUrl":"https://example.com","apiKey":"sk",
                     "model":"vendor-pro",
                     "models":["vendor-pro","vendor-flash"],
                     "modelProfiles":{"general":{"model":"vendor-pro","reasoningEffort":"xhigh"}}}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(
            resolve_model_profile(Some(&resolved), "small", "vendor-flash"),
            "vendor-flash"
        );
        assert_eq!(
            resolve_model_profile(Some(&resolved), "explore", "vendor-flash"),
            "vendor-flash"
        );
        // An explicit `general` request still honours the declaration.
        assert_eq!(
            resolve_model_profile(Some(&resolved), "general", "vendor-flash"),
            "vendor-pro"
        );
    }

    #[test]
    fn resolves_custom_provider_thinking_as_request_options() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"deepseek",
                "customProviders":[
                    {"name":"deepseek","format":"openai",
                     "baseUrl":"https://api.deepseek.com","apiKey":"sk-ds",
                     "model":"deepseek-v4-pro","thinkingEnabled":true,
                     "thinkingEffort":"xhigh"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.name, "deepseek");
        assert_eq!(
            resolved.request_options.extra_body["thinking"]["type"],
            "enabled"
        );
        assert_eq!(resolved.request_options.body["reasoning_effort"], "xhigh");
        assert!(resolved
            .request_options
            .omit_body_fields
            .iter()
            .any(|field| field == "temperature"));
    }

    #[test]
    fn resolves_custom_provider_reasoning_mode() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"openai",
                "customProviders":[
                    {"name":"openai","format":"openai-responses",
                     "baseUrl":"https://chatgpt.com/backend-api/codex/responses",
                     "apiKey":"sk-literal","model":"gpt-5.6-sol",
                     "reasoningMode":"pro"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.reasoning_mode.as_deref(), Some("pro"));
    }

    #[test]
    fn reasoning_mode_defaults_to_none() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"openai",
                "customProviders":[
                    {"name":"openai","format":"openai-responses",
                     "baseUrl":"https://chatgpt.com/backend-api/codex/responses",
                     "apiKey":"sk-literal","model":"gpt-5.6-sol"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.reasoning_mode, None);
    }

    #[test]
    fn provider_options_parse_headers_and_survive_round_trip() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("APP_REFERER", "https://app.example");
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"generic",
                "customProviders":[
                    {"name":"generic","format":"openai",
                     "baseUrl":"https://example.com","apiKey":"sk",
                     "model":"gpt-4o",
                     "unknownProviderField":"keep",
                     "options":{
                       "headers":{"HTTP-Referer":"$APP_REFERER","X-Empty":"$MISSING_HEADER","Authorization":"bad","X-Title":"Rebon"},
                       "body":{"streamOptions":{"includeUsage":true}},
                       "extraBody":{"thinking":{"type":"enabled"}},
                       "omitBodyFields":["temperature"]
                     }}
                ]
            }"#,
        );

        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert!(resolved
            .extra_headers
            .iter()
            .any(|(name, value)| name == "HTTP-Referer" && value == "https://app.example"));
        assert!(resolved
            .extra_headers
            .iter()
            .any(|(name, value)| name == "X-Title" && value == "Rebon"));
        assert!(!resolved
            .extra_headers
            .iter()
            .any(|(name, _)| name == "Authorization" || name == "X-Empty"));
        assert_eq!(
            resolved.request_options.body["streamOptions"]["includeUsage"],
            true
        );
        assert_eq!(
            resolved.request_options.extra_body["thinking"]["type"],
            "enabled"
        );

        let roundtrip = read_config_roundtrip(tmp.path()).unwrap();
        let json = serde_json::to_value(&roundtrip).unwrap();
        assert_eq!(json["customProviders"][0]["unknownProviderField"], "keep");
        assert_eq!(
            json["customProviders"][0]["options"]["headers"]["X-Title"],
            "Rebon"
        );
        std::env::remove_var("APP_REFERER");
    }

    #[test]
    fn resolves_deepseek_named_provider_without_heuristic_thinking_default() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"deepseek-custom",
                "customProviders":[
                    {"name":"deepseek-custom","format":"openai",
                     "baseUrl":"https://api.deepseek.com","apiKey":"sk-ds",
                     "model":"deepseek-v4-pro"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.name, "deepseek-custom");
        assert!(resolved.request_options.is_empty());
    }

    #[test]
    fn resolves_generic_openai_custom_provider_without_request_options() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"generic",
                "customProviders":[
                    {"name":"generic","format":"openai",
                     "baseUrl":"https://example.com/v1","apiKey":"sk-generic",
                     "model":"gpt-4o"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.name, "generic");
        assert!(resolved.request_options.is_empty());
    }

    #[test]
    fn model_profile_context_window_is_ignored() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"deepseek",
                "customProviders":[
                    {"name":"deepseek","format":"anthropic","baseUrl":"https://api.deepseek.com/anthropic","apiKey":"sk","model":"deepseek-v4-flash",
                     "modelProfiles":{
                       "general":{"model":"deepseek-v4-pro[1m]","contextWindow":1000000}
                     }}
                ]
            }"#,
        );

        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(
            resolve_model_profile(Some(&resolved), "general", "runtime-main"),
            "deepseek-v4-pro[1m]"
        );
        assert_eq!(
            resolve_model_context_window(Some(&resolved), "deepseek-v4-pro[1m]"),
            None
        );
        assert!(resolved.model_context_windows.is_empty());
    }

    #[test]
    fn model_options_parse_for_string_object_and_map_shapes() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"array-provider",
                "customProviders":[
                    {"name":"array-provider","format":"openai","baseUrl":"https://example.com","apiKey":"sk","model":"only-model-id",
                     "models":[
                        "only-model-id",
                        {"id":"deepseek-v4-pro","name":"DeepSeek-V4-Pro","limit":{"context":1048576,"output":128000},"options":{"body":{"reasoningEffort":"xhigh","thinking":{"type":"enabled"}}}},
                        {"id":"deepseek-v4-pro[1m]","contextWindow":1000000,"maxOutputTokens":"64000"}
                     ]},
                    {"name":"map-provider","format":"openai","baseUrl":"https://example.com","apiKey":"sk","model":"deepseek-v4-pro",
                     "models":{"deepseek-v4-pro":{"name":"DeepSeek-V4-Pro","limit":{"context":1048576,"output":128000},"options":{"body":{"reasoningEffort":"xhigh","thinking":{"type":"enabled"}}}}}}
                ]
            }"#,
        );

        let array_resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(
            array_resolved.model_request_options["deepseek-v4-pro"].body["reasoningEffort"],
            "xhigh"
        );
        assert_eq!(
            array_resolved.model_context_windows["deepseek-v4-pro"],
            1_048_576
        );
        assert_eq!(
            array_resolved.model_context_windows["deepseek-v4-pro[1m]"],
            1_000_000
        );
        assert_eq!(
            resolve_model_context_window(Some(&array_resolved), "deepseek-v4-pro"),
            Some(1_048_576)
        );
        assert_eq!(
            resolve_model_output_token_limit(Some(&array_resolved), "deepseek-v4-pro"),
            Some(128_000)
        );
        assert_eq!(
            resolve_model_output_token_limit(Some(&array_resolved), "deepseek-v4-pro[1m]"),
            Some(64_000)
        );
        let map_resolved = resolve_from_dir_with(tmp.path(), Some("map-provider"))
            .unwrap()
            .unwrap();
        assert_eq!(
            map_resolved.model_request_options["deepseek-v4-pro"].body["reasoningEffort"],
            "xhigh"
        );
        assert_eq!(
            map_resolved.model_context_windows["deepseek-v4-pro"],
            1_048_576
        );
        assert_eq!(
            map_resolved.model_output_token_limits["deepseek-v4-pro"],
            128_000
        );
        let providers = list_custom_providers_from(tmp.path());
        assert_eq!(providers[0].models[0], "only-model-id");
        assert!(providers[0].models.contains(&"deepseek-v4-pro".to_string()));
        assert_eq!(providers[1].models, vec!["deepseek-v4-pro".to_string()]);
    }

    #[test]
    fn model_context_window_prefers_context_window_over_the_nested_limit() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"deepseek",
                "customProviders":[
                    {"name":"deepseek","format":"anthropic","baseUrl":"https://api.deepseek.com/anthropic","apiKey":"sk","model":"deepseek-v4-flash",
                     "models":{
                       "deepseek-v4-pro[1m]":{"contextWindow":1000000,"limit":{"context":128000}},
                       "deepseek-v4-flash":{"contextWindow":"128000"}
                     }}
                ]
            }"#,
        );

        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(
            resolve_model_context_window(Some(&resolved), "deepseek-v4-pro[1m]"),
            Some(1_000_000)
        );
        assert_eq!(
            resolve_model_context_window(Some(&resolved), "deepseek-v4-flash"),
            Some(128_000)
        );
        assert_eq!(
            resolve_model_context_window(Some(&resolved), "missing"),
            None
        );
    }

    #[test]
    fn resolves_oauth_sentinel_from_credentials_file() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"openai",
                "customProviders":[
                    {"name":"openai","format":"openai-responses",
                     "baseUrl":"https://chatgpt.com/backend-api/codex/responses",
                     "apiKey":"$OPENAI_OAUTH_TOKEN","model":"gpt-5.4"}
                ]
            }"#,
        );
        write_credentials(
            tmp.path(),
            r#"{"openaiOAuth":{
                "accessToken":"sk-oauth-live",
                "refreshToken":"refresh-xxx",
                "expiresAt":1800000000000
            }}"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(resolved.api_key, "sk-oauth-live");
        let oauth = resolved.oauth.expect("oauth meta");
        assert_eq!(oauth.refresh_token.as_deref(), Some("refresh-xxx"));
        assert_eq!(oauth.expires_at_ms, Some(1800000000000));
    }

    #[test]
    fn sentinel_without_credentials_file_errors_with_login_hint() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"openai",
                "customProviders":[
                    {"name":"openai","format":"openai-responses",
                     "baseUrl":"https://chatgpt.com/backend-api/codex/responses",
                     "apiKey":"$OPENAI_OAUTH_TOKEN","model":"gpt-5.4"}
                ]
            }"#,
        );
        let err = resolve_from_dir(tmp.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("login") || msg.contains("/login"),
            "error should point at login, got: {msg}"
        );
    }

    #[test]
    fn sentinel_with_empty_access_token_errors() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"openai",
                "customProviders":[
                    {"name":"openai","format":"openai-responses",
                     "baseUrl":"https://chatgpt.com/backend-api/codex/responses",
                     "apiKey":"$OPENAI_OAUTH_TOKEN","model":"gpt-5.4"}
                ]
            }"#,
        );
        write_credentials(tmp.path(), r#"{"openaiOAuth":{"accessToken":""}}"#);
        let err = resolve_from_dir(tmp.path()).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("empty"));
    }

    #[test]
    fn unknown_format_is_rejected_so_silent_drift_is_impossible() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"weird",
                "customProviders":[
                    {"name":"weird","format":"future-format",
                     "baseUrl":"https://example.com","apiKey":"sk","model":"m"}
                ]
            }"#,
        );
        let err = resolve_from_dir(tmp.path()).unwrap_err();
        assert!(format!("{err}").contains("future-format"));
    }

    #[test]
    fn credentials_parser_preserves_unknown_top_level_keys_for_round_trip() {
        // The refresh rewrites .credentials.json in place. The
        // round-trip must not delete sibling entries like
        // `claudeAiOauth` that the file also stores alongside.
        let tmp = TempDir::new().unwrap();
        write_credentials(
            tmp.path(),
            r#"{
                "openaiOAuth":{"accessToken":"a","refreshToken":"r","expiresAt":1},
                "claudeAiOauth":{"accessToken":"keep-me"}
            }"#,
        );
        let creds = read_credentials(tmp.path()).unwrap();
        assert!(creds.extra.contains_key("claudeAiOauth"));

        // Re-serialize and confirm the extra key survives.
        let json = serde_json::to_value(&creds).unwrap();
        assert_eq!(json["claudeAiOauth"]["accessToken"], "keep-me");
    }

    #[test]
    fn computer_use_requires_canonical_openai_oauth_config_and_credentials() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            config_json_path(tmp.path()),
            serde_json::to_vec(&serde_json::json!({
                "customProviders": [{
                    "name": OPENAI_OAUTH_PROVIDER_NAME,
                    "format": "openai-responses",
                    "baseUrl": OPENAI_OAUTH_PROVIDER_BASE_URL,
                    "apiKey": OPENAI_OAUTH_TOKEN_SENTINEL,
                    "model": "gpt-5.6"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(!builtin_openai_computer_use_available_in(
            tmp.path(),
            OPENAI_OAUTH_PROVIDER_NAME
        ));
        write_openai_oauth_tokens(
            tmp.path(),
            &OpenAIOAuthTokens {
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                expires_at: Some(u64::MAX),
            },
        )
        .unwrap();
        assert!(builtin_openai_computer_use_available_in(
            tmp.path(),
            OPENAI_OAUTH_PROVIDER_NAME
        ));
        assert!(!builtin_openai_computer_use_available_in(
            tmp.path(),
            "OpenAI"
        ));
    }

    /// The live failure this classification exists for: the ChatGPT token
    /// endpoint answers an invalidated refresh token with 401
    /// `refresh_token_invalidated`. Only `/login` recovers from it.
    #[test]
    fn invalidated_refresh_token_response_requires_login() {
        let body = r#"{"error":{"message":"Your session has ended. Please log in again.","type":"invalid_request_error","param":null,"code":"refresh_token_invalidated"}}"#;

        assert_eq!(
            classify_refresh_status(401, body),
            OAuthRefreshErrorKind::Unauthorized
        );
        assert_eq!(
            classify_refresh_status(400, r#"{"error":"invalid_grant"}"#),
            OAuthRefreshErrorKind::Unauthorized
        );
        assert_eq!(
            classify_refresh_status(403, "forbidden"),
            OAuthRefreshErrorKind::Unauthorized
        );
    }

    /// A bad minute at the auth host must never cost the user their
    /// session — those failures leave the stored tokens untouched.
    #[test]
    fn server_side_and_unknown_refresh_failures_are_transient() {
        assert_eq!(
            classify_refresh_status(500, "internal error"),
            OAuthRefreshErrorKind::Transient
        );
        assert_eq!(
            classify_refresh_status(429, "slow down"),
            OAuthRefreshErrorKind::Transient
        );
        assert_eq!(
            classify_refresh_status(502, "<html>bad gateway</html>"),
            OAuthRefreshErrorKind::Transient
        );
    }

    /// A 5xx body that still names a dead grant is a dead grant.
    #[test]
    fn non_auth_status_with_invalid_grant_body_requires_login() {
        assert_eq!(
            classify_refresh_status(500, r#"{"error":"invalid_grant"}"#),
            OAuthRefreshErrorKind::Unauthorized
        );
    }

    #[test]
    fn refresh_error_kind_survives_the_anyhow_round_trip() {
        let unauthorized: anyhow::Error =
            OAuthRefreshError::unauthorized("session has ended").into();
        let transient: anyhow::Error = OAuthRefreshError::transient("connection reset").into();
        let unrelated = anyhow::anyhow!("failed to write credentials");

        assert!(oauth_refresh_requires_login(&unauthorized));
        assert_eq!(unauthorized.to_string(), "session has ended");
        assert!(!oauth_refresh_requires_login(&transient));
        assert_eq!(
            oauth_refresh_error_kind(&transient),
            Some(OAuthRefreshErrorKind::Transient)
        );
        // A failure that is not the refresh POST must not be read as
        // "log in again" — callers key their recovery on that.
        assert!(!oauth_refresh_requires_login(&unrelated));
        assert_eq!(oauth_refresh_error_kind(&unrelated), None);
    }

    /// A rejected grant reaches a startup surface as `LoginRequired`, not
    /// as an error that ends the process.
    #[tokio::test]
    async fn startup_oauth_check_reports_login_required_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = check_startup_oauth(
            tmp.path(),
            &OAuthMeta {
                expires_at_ms: Some(0),
                refresh_token: None,
            },
        )
        .await;

        assert_eq!(state, OAuthStartupState::LoginRequired);
    }

    /// A token still inside its window is not a network round trip.
    #[tokio::test]
    async fn startup_oauth_check_leaves_a_fresh_token_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let state = check_startup_oauth(
            tmp.path(),
            &OAuthMeta {
                expires_at_ms: Some(u64::MAX),
                refresh_token: Some("refresh".into()),
            },
        )
        .await;

        assert_eq!(state, OAuthStartupState::Cached);
    }

    /// The preemptive path with no refresh token on disk is the same
    /// "log in again" case, not a transient one.
    #[tokio::test]
    async fn missing_refresh_token_requires_login() {
        let tmp = tempfile::tempdir().unwrap();
        let err = check_and_refresh_if_needed(
            tmp.path(),
            &OAuthMeta {
                expires_at_ms: Some(0),
                refresh_token: None,
            },
        )
        .await
        .expect_err("an expired token with no refresh token cannot be rotated");

        assert!(oauth_refresh_requires_login(&err), "{err}");
    }

    #[test]
    fn token_expired_when_expires_at_is_none() {
        assert!(is_token_expired_at(1_000_000, None));
    }

    #[test]
    fn token_expired_when_within_5_minute_buffer() {
        let now = 1_000_000_000_u64;
        // expires in 4 minutes — within the 5-minute buffer, should refresh.
        assert!(is_token_expired_at(now, Some(now + 4 * 60 * 1000)));
        // expires right now — obviously expired.
        assert!(is_token_expired_at(now, Some(now)));
        // expires 1ms in the past — expired.
        assert!(is_token_expired_at(now, Some(now - 1)));
    }

    #[test]
    fn token_fresh_when_expiry_is_beyond_the_buffer() {
        let now = 1_000_000_000_u64;
        // expires in 6 minutes — outside the 5-minute buffer.
        assert!(!is_token_expired_at(now, Some(now + 6 * 60 * 1000)));
        // expires in 24 hours — fresh.
        assert!(!is_token_expired_at(now, Some(now + 24 * 60 * 60 * 1000)));
    }

    #[test]
    fn write_openai_oauth_tokens_creates_file_with_only_oauth_block() {
        let tmp = TempDir::new().unwrap();
        let tokens = OpenAIOAuthTokens {
            access_token: "sk-new".into(),
            refresh_token: Some("refresh-new".into()),
            expires_at: Some(1_900_000_000_000),
        };

        write_openai_oauth_tokens(tmp.path(), &tokens).unwrap();

        let raw = fs::read_to_string(credentials_json_path(tmp.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["openaiOAuth"]["accessToken"], "sk-new");
        assert_eq!(value["openaiOAuth"]["refreshToken"], "refresh-new");
        assert_eq!(value["openaiOAuth"]["expiresAt"], 1_900_000_000_000_u64);
    }

    #[test]
    fn write_openai_oauth_tokens_preserves_unknown_sibling_entries() {
        let tmp = TempDir::new().unwrap();
        // Seed with a non-OpenAI entry that the file might own.
        write_credentials(
            tmp.path(),
            r#"{
                "openaiOAuth":{"accessToken":"old","refreshToken":"old-r","expiresAt":1},
                "claudeAiOauth":{"accessToken":"dont-touch-me"}
            }"#,
        );

        let new_tokens = OpenAIOAuthTokens {
            access_token: "fresh".into(),
            refresh_token: Some("fresh-r".into()),
            expires_at: Some(2),
        };
        write_openai_oauth_tokens(tmp.path(), &new_tokens).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(credentials_json_path(tmp.path())).unwrap())
                .unwrap();
        assert_eq!(value["openaiOAuth"]["accessToken"], "fresh");
        assert_eq!(value["openaiOAuth"]["refreshToken"], "fresh-r");
        // The sibling entry survived the rewrite.
        assert_eq!(value["claudeAiOauth"]["accessToken"], "dont-touch-me");
    }

    #[test]
    fn write_openai_oauth_tokens_does_not_leak_temp_file_on_success() {
        let tmp = TempDir::new().unwrap();
        let tokens = OpenAIOAuthTokens {
            access_token: "sk".into(),
            refresh_token: None,
            expires_at: None,
        };
        write_openai_oauth_tokens(tmp.path(), &tokens).unwrap();

        let target = credentials_json_path(tmp.path());
        let tmp_path = target.with_extension("json.tmp");
        assert!(target.exists());
        assert!(
            !tmp_path.exists(),
            "temp file {tmp_path:?} should have been renamed away"
        );
    }

    #[test]
    fn home_dir_itself_is_home_or_above() {
        if let Some(home) = home_dir() {
            assert!(
                is_home_dir_or_above(&home),
                "home dir itself must be treated as too-broad"
            );
        }
    }

    #[test]
    fn root_dir_is_home_or_above() {
        #[cfg(windows)]
        {
            assert!(is_home_dir_or_above(Path::new("C:\\")));
        }
        #[cfg(not(windows))]
        {
            assert!(is_home_dir_or_above(Path::new("/")));
        }
    }

    #[test]
    fn child_of_home_is_not_home_or_above() {
        if let Some(home) = home_dir() {
            let child = home.join("definitely_a_project");
            assert!(
                !is_home_dir_or_above(&child),
                "subdirectory of home should be trustable"
            );
        }
    }

    #[test]
    fn format_default_is_openai_when_field_missing() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"plain",
                "customProviders":[
                    {"name":"plain","baseUrl":"https://api.example.com",
                     "apiKey":"sk","model":"m"}
                ]
            }"#,
        );
        let resolved = resolve_from_dir(tmp.path()).unwrap().unwrap();
        assert!(matches!(resolved.format, ProviderFormat::Openai));
    }

    // ------------------------------------------------------------------
    // sub-agent model config
    // ------------------------------------------------------------------

    #[test]
    fn sub_agent_model_config_reads_config_json_shape() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "$schema":"https://example.invalid/schema.json",
                "agents":{
                    "explore":{"provider":"deepseek","modelProfile":"explore","variant":"medium"}
                },
                "categories":{
                    "deep":{"provider":"openai","model":"openai/gpt-5.4","variant":"xhigh"}
                },
                "google_auth":false
            }"#,
        );

        let config = saved_sub_agent_model_config_in_dir(tmp.path());
        let explore = config.agent("Explore").unwrap();
        assert_eq!(explore.provider.as_deref(), Some("deepseek"));
        assert_eq!(explore.model.as_deref(), None);
        assert_eq!(explore.model_profile.as_deref(), Some("explore"));
        assert_eq!(explore.reasoning_effort, Some(ReasoningEffort::Medium));
        let deep = config.category("DEEP").unwrap();
        assert_eq!(deep.provider.as_deref(), Some("openai"));
        assert_eq!(deep.model.as_deref(), Some("openai/gpt-5.4"));
        assert_eq!(deep.reasoning_effort, Some(ReasoningEffort::XHigh));
    }

    #[test]
    fn sub_agent_model_config_agents_json_overrides_config_json() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"agents":{"explore":{"provider":"deepseek","model":"openai/gpt-5.4","variant":"high"}}}"#,
        );
        fs::write(
            agents_json_path(tmp.path()),
            r#"{"agents":{"Explore":{"provider":"openai","model":"openai/gpt-5.3-codex","variant":"low"}}}"#,
        )
        .unwrap();

        let config = saved_sub_agent_model_config_in_dir(tmp.path());
        let explore = config.agent("explore").unwrap();
        assert_eq!(explore.provider.as_deref(), Some("openai"));
        assert_eq!(explore.model.as_deref(), Some("openai/gpt-5.3-codex"));
        assert_eq!(explore.reasoning_effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn fast_mode_config_round_trips_and_preserves_other_feature_flags() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"theme":"dark","features":{"otherFlag":true},"customProviders":[]}"#,
        );

        assert!(!saved_fast_mode_enabled_in_dir(tmp.path()));
        save_fast_mode_enabled_in_dir(tmp.path(), true).unwrap();
        assert!(saved_fast_mode_enabled_in_dir(tmp.path()));
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(config_json_path(tmp.path())).unwrap())
                .unwrap();
        assert_eq!(parsed["serviceTier"], "fast");
        assert_eq!(parsed["features"]["fastMode"], true);
        assert_eq!(parsed["features"]["otherFlag"], true);
        assert_eq!(parsed["theme"], "dark");

        save_fast_mode_enabled_in_dir(tmp.path(), false).unwrap();
        assert!(!saved_fast_mode_enabled_in_dir(tmp.path()));
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(config_json_path(tmp.path())).unwrap())
                .unwrap();
        assert!(parsed.get("serviceTier").is_none());
        assert!(parsed["features"].get("fastMode").is_none());
        assert_eq!(parsed["features"]["otherFlag"], true);
    }

    #[test]
    fn fast_mode_reads_codex_compatible_shapes() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"serviceTier":"fast"}"#);
        assert!(saved_fast_mode_enabled_in_dir(tmp.path()));

        write_config(tmp.path(), r#"{"serviceTier":"priority"}"#);
        assert!(saved_fast_mode_enabled_in_dir(tmp.path()));

        write_config(tmp.path(), r#"{"features":{"fastMode":true}}"#);
        assert!(saved_fast_mode_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn service_tier_capability_is_limited_to_official_openai_backends() {
        assert!(openai_service_tier_available(
            ProviderFormat::Openai,
            "https://api.openai.com/v1",
            false,
            false,
        ));
        assert!(openai_service_tier_available(
            ProviderFormat::OpenaiResponses,
            "https://chatgpt.com/backend-api/codex",
            true,
            false,
        ));
        assert!(!openai_service_tier_available(
            ProviderFormat::Openai,
            "https://api.deepseek.com",
            false,
            false,
        ));
        assert!(!openai_service_tier_available(
            ProviderFormat::Anthropic,
            "https://api.anthropic.com",
            false,
            false,
        ));
        assert!(!openai_service_tier_available(
            ProviderFormat::Openai,
            "https://api.openai.com/v1",
            false,
            true,
        ));
    }

    #[test]
    fn first_party_openai_routes_are_openai_itself_and_codex_oauth() {
        for (format, base_url, oauth, external, expected) in [
            (
                ProviderFormat::OpenaiResponses,
                "https://api.openai.com/v1",
                false,
                false,
                true,
            ),
            (
                ProviderFormat::Openai,
                "https://api.openai.com",
                false,
                false,
                true,
            ),
            (ProviderFormat::Openai, "", false, false, true),
            (
                ProviderFormat::OpenaiResponses,
                "https://chatgpt.com/backend-api/codex/responses",
                true,
                false,
                true,
            ),
            // The Codex backend without OpenAI OAuth is not a route a key reaches.
            (
                ProviderFormat::OpenaiResponses,
                "https://chatgpt.com/backend-api/codex/responses",
                false,
                false,
                false,
            ),
            (ProviderFormat::OpenaiResponses, "", false, false, false),
            (
                ProviderFormat::Openai,
                "https://gateway.example.com/v1",
                false,
                false,
                false,
            ),
            (
                ProviderFormat::Openai,
                "https://api.openai.com.evil.test/v1",
                false,
                false,
                false,
            ),
            (
                ProviderFormat::Anthropic,
                "https://api.openai.com/v1",
                false,
                false,
                false,
            ),
            (
                ProviderFormat::OpenaiResponses,
                "https://api.openai.com/v1",
                false,
                true,
                false,
            ),
        ] {
            assert_eq!(
                is_first_party_openai_route(format, base_url, oauth, external),
                expected,
                "{format:?} {base_url:?} oauth={oauth} external={external}"
            );
            assert_eq!(
                openai_service_tier_available(format, base_url, oauth, external),
                expected,
                "the fast tier rides exactly the first-party routes"
            );
        }
    }

    #[test]
    fn installed_external_provider_ids_reads_effective_user_plugins() {
        let tmp = TempDir::new().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        std::fs::create_dir_all(plugins_dir.join("disk-plugin").join("1.0.0")).unwrap();
        std::fs::write(
            plugins_dir
                .join("disk-plugin")
                .join("1.0.0")
                .join("provider.mjs"),
            "export function activate() {}",
        )
        .unwrap();
        std::fs::write(
            plugins_dir
                .join("disk-plugin")
                .join("1.0.0")
                .join("rebon-plugin.json"),
            r#"{
                "name": "disk-plugin",
                "version": "1.0.0",
                "capabilities": {
                    "modelProviders": {"disk-provider": {"transport": {"type": "plugin", "entry": "provider.mjs"}}}
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            plugins_dir.join("installed.json"),
            r#"{
                "plugins": [
                    {
                        "name": "enabled-plugin",
                        "version": "1.0.0",
                        "enabled": true,
                        "sourceKind": "local",
                        "manifest": {
                            "name": "enabled-plugin",
                            "version": "1.0.0",
                            "capabilities": {
                                "modelProviders": {
                                    "plugin-openai": {"transport": {"type": "plugin", "entry": "provider.mjs"}},
                                    "openai": {"transport": {"type": "plugin", "entry": "provider.mjs"}},
                                    "openai-responses": {"transport": {"type": "plugin", "entry": "provider.mjs"}},
                                    "anthropic": {"transport": {"type": "plugin", "entry": "provider.mjs"}}
                                }
                            }
                        }
                    },
                    {
                        "name": "disk-plugin",
                        "version": "1.0.0",
                        "enabled": true,
                        "sourceKind": "local",
                        "manifest": null
                    },
                    {
                        "name": "disabled-plugin",
                        "version": "1.0.0",
                        "enabled": false,
                        "sourceKind": "local",
                        "manifest": {
                            "name": "disabled-plugin",
                            "version": "1.0.0",
                            "capabilities": {
                                "modelProviders": {"disabled-provider": {"transport": {"type": "plugin", "entry": "provider.mjs"}}}
                            }
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let ids = installed_external_provider_ids_in(tmp.path(), None);
        assert_eq!(
            ids,
            BTreeSet::from(["disk-provider".to_string(), "plugin-openai".to_string()])
        );
    }

    #[test]
    fn installed_external_provider_ids_match_project_record_precedence_and_trust() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("workspace").join("project");
        let user_plugins = tmp.path().join("plugins");
        let project_plugins = cwd.join(".rebon").join("plugins");
        std::fs::create_dir_all(user_plugins.join("disk-user").join("1.0.0")).unwrap();
        std::fs::write(
            user_plugins
                .join("disk-user")
                .join("1.0.0")
                .join("provider.mjs"),
            "export function activate() {}",
        )
        .unwrap();
        std::fs::create_dir_all(&project_plugins).unwrap();
        std::fs::write(
            user_plugins
                .join("disk-user")
                .join("1.0.0")
                .join("rebon-plugin.json"),
            r#"{
                "name": "disk-user",
                "version": "1.0.0",
                "capabilities": {
                    "modelProviders": {"disk-user-provider": {"transport": {"type": "plugin", "entry": "provider.mjs"}}}
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            user_plugins.join("installed.json"),
            r#"{
                "plugins": [
                    {
                        "name": "user-bundle",
                        "version": "1.0.0",
                        "enabled": true,
                        "sourceKind": "local",
                        "manifest": {
                            "name": "user-bundle",
                            "version": "1.0.0",
                            "capabilities": {
                                "modelProviders": {"user-a": {"transport": {"type": "plugin", "entry": "provider.mjs"}}, "user-b": {"transport": {"type": "plugin", "entry": "provider.mjs"}}}
                            }
                        }
                    },
                    {
                        "name": "disk-user",
                        "version": "1.0.0",
                        "enabled": true,
                        "sourceKind": "local",
                        "manifest": null
                    }
                ]
            }"#,
        )
        .unwrap();
        std::fs::write(
            project_plugins.join("installed.json"),
            r#"{
                "plugins": [
                    {
                        "name": "block-user-bundle",
                        "version": "1.0.0",
                        "enabled": false,
                        "sourceKind": "local",
                        "manifest": {
                            "name": "block-user-bundle",
                            "version": "1.0.0",
                            "capabilities": {
                                "modelProviders": {"user-a": {"transport": {"type": "plugin", "entry": "provider.mjs"}}}
                            }
                        }
                    },
                    {
                        "name": "disk-user",
                        "version": "1.0.0",
                        "enabled": false,
                        "sourceKind": "local",
                        "manifest": null
                    },
                    {
                        "name": "project-plugin",
                        "version": "1.0.0",
                        "enabled": true,
                        "sourceKind": "local",
                        "manifest": {
                            "name": "project-plugin",
                            "version": "1.0.0",
                            "capabilities": {
                                "modelProviders": {"project-only": {"transport": {"type": "plugin", "entry": "provider.mjs"}}}
                            }
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            installed_external_provider_ids_in(tmp.path(), Some(&cwd)),
            BTreeSet::from([
                "disk-user-provider".to_string(),
                "user-a".to_string(),
                "user-b".to_string(),
            ])
        );

        let trusted_ancestor = cwd.parent().unwrap();
        write_config(
            tmp.path(),
            &serde_json::json!({
                "projects": {
                    (normalize_trust_key(trusted_ancestor)): {
                        "hasTrustDialogAccepted": true
                    }
                }
            })
            .to_string(),
        );
        assert!(is_directory_trusted_in(tmp.path(), &cwd));
        assert_eq!(
            installed_external_provider_ids_in(tmp.path(), Some(&cwd)),
            BTreeSet::from(["project-only".to_string()])
        );
    }

    /// A directory already trusted is trusted under the extended-length
    /// spelling of the same path.
    ///
    /// The key was its own normalization -- slashes and case, not the prefix --
    /// so `\\?\F:\dev\x` and `F:\dev\x` were two projects. Anything that hands
    /// over a canonicalized path (which is where the prefix comes from) got the
    /// trust dialog again for a directory the user had already trusted, and the
    /// trust had to be written under both spellings to stick.
    #[cfg(windows)]
    #[test]
    fn an_extended_length_path_is_trusted_by_the_plain_spelling() {
        let tmp = TempDir::new().unwrap();
        let plain = std::path::PathBuf::from(r"F:\dev\trusted-project");
        write_config(
            tmp.path(),
            &serde_json::json!({
                "projects": {
                    (normalize_trust_key(&plain)): { "hasTrustDialogAccepted": true }
                }
            })
            .to_string(),
        );

        assert!(is_directory_trusted_in(tmp.path(), &plain));
        assert!(
            is_directory_trusted_in(
                tmp.path(),
                std::path::Path::new(r"\\?\F:\dev\trusted-project")
            ),
            "the same directory, spelled the way canonicalize spells it"
        );
        // And a different project is still a different project.
        assert!(!is_directory_trusted_in(
            tmp.path(),
            std::path::Path::new(r"F:\dev\other-project")
        ));
    }

    // ------------------------------------------------------------------
    // sub_agents toggle persistence
    // ------------------------------------------------------------------

    #[test]
    fn sub_agents_defaults_to_true_when_config_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn sub_agents_defaults_to_true_when_key_absent() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"customProviders":[]}"#);
        assert!(saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn sub_agents_reads_persisted_false() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"subAgentsEnabled":false,"customProviders":[]}"#,
        );
        assert!(!saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn sub_agents_reads_persisted_true() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"subAgentsEnabled":true,"customProviders":[]}"#,
        );
        assert!(saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn sub_agents_save_then_read_round_trip_false() {
        let tmp = TempDir::new().unwrap();
        save_sub_agents_enabled_in_dir(tmp.path(), false).unwrap();
        assert!(!saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn sub_agents_save_then_read_round_trip_true() {
        let tmp = TempDir::new().unwrap();
        save_sub_agents_enabled_in_dir(tmp.path(), false).unwrap();
        save_sub_agents_enabled_in_dir(tmp.path(), true).unwrap();
        assert!(saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn sub_agents_save_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"keep",
                "theme":"dark",
                "hasCompletedOnboarding":true,
                "customProviders":[
                    {"name":"keep","baseUrl":"https://api","apiKey":"sk","model":"m"}
                ]
            }"#,
        );
        save_sub_agents_enabled_in_dir(tmp.path(), false).unwrap();
        let raw = fs::read_to_string(config_json_path(tmp.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["subAgentsEnabled"], serde_json::Value::Bool(false));
        assert_eq!(parsed["activeCustomProvider"], "keep");
        assert_eq!(parsed["theme"], "dark");
        assert_eq!(parsed["hasCompletedOnboarding"], true);
        assert_eq!(parsed["customProviders"][0]["name"], "keep");
    }

    #[test]
    fn sub_agents_save_creates_config_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested").join("dir");
        assert!(!nested.exists());
        save_sub_agents_enabled_in_dir(&nested, false).unwrap();
        assert!(nested.exists());
        assert!(!saved_sub_agents_enabled_in_dir(&nested));
    }

    #[test]
    fn sub_agents_save_overwrites_existing_value() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"subAgentsEnabled":true,"customProviders":[]}"#,
        );
        save_sub_agents_enabled_in_dir(tmp.path(), false).unwrap();
        assert!(!saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn sub_agents_read_ignores_non_bool_value() {
        // Malformed value (string instead of bool) falls back to default.
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"subAgentsEnabled":"yes","customProviders":[]}"#,
        );
        assert!(saved_sub_agents_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn claude_codex_fallback_defaults_to_false_when_config_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn claude_codex_fallback_defaults_to_false_when_key_absent() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"customProviders":[]}"#);
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn claude_codex_fallback_reads_persisted_values() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"claudeCodexFallbackEnabled":true}"#);
        assert!(saved_claude_codex_fallback_enabled_in_dir(tmp.path()));

        write_config(tmp.path(), r#"{"claudeCodexFallbackEnabled":false}"#);
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn claude_codex_fallback_save_round_trips_and_overwrites() {
        let tmp = TempDir::new().unwrap();
        save_claude_codex_fallback_enabled_in_dir(tmp.path(), true).unwrap();
        assert!(saved_claude_codex_fallback_enabled_in_dir(tmp.path()));

        save_claude_codex_fallback_enabled_in_dir(tmp.path(), false).unwrap();
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));
    }

    #[test]
    fn claude_codex_fallback_save_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{"activeCustomProvider":"keep","theme":"dark","customProviders":[]}"#,
        );
        save_claude_codex_fallback_enabled_in_dir(tmp.path(), true).unwrap();

        let raw = fs::read_to_string(config_json_path(tmp.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["claudeCodexFallbackEnabled"], true);
        assert_eq!(parsed["activeCustomProvider"], "keep");
        assert_eq!(parsed["theme"], "dark");
    }

    #[test]
    fn claude_codex_fallback_save_creates_config_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested").join("dir");
        save_claude_codex_fallback_enabled_in_dir(&nested, true).unwrap();
        assert!(saved_claude_codex_fallback_enabled_in_dir(&nested));
    }

    #[test]
    fn claude_codex_fallback_ignores_non_bool_value() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"claudeCodexFallbackEnabled":"yes"}"#);
        assert!(!saved_claude_codex_fallback_enabled_in_dir(tmp.path()));
    }

    // ── `models` field compatibility + add_custom_provider_model ─────

    // ------------------------------------------------------------------
    // generatedImagesDir config
    // ------------------------------------------------------------------

    #[test]
    fn generated_images_output_base_defaults_to_config_dir() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("workspace");
        assert_eq!(
            generated_images_output_base_in_dir(tmp.path(), &cwd),
            tmp.path().join("generated_images")
        );
    }

    #[test]
    fn generated_images_output_base_reads_camel_case_key() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("workspace");
        write_config(
            tmp.path(),
            r#"{"generatedImagesDir":"{cwd}/.rebon/generated_images"}"#,
        );
        assert_eq!(
            generated_images_output_base_in_dir(tmp.path(), &cwd),
            cwd.join(".rebon").join("generated_images")
        );
    }

    #[test]
    fn generated_images_output_base_accepts_snake_case_alias() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("workspace");
        write_config(
            tmp.path(),
            r#"{"generated_images_dir":"{config}/generated"}"#,
        );
        assert_eq!(
            generated_images_output_base_in_dir(tmp.path(), &cwd),
            tmp.path().join("generated")
        );
    }

    #[test]
    fn generated_images_output_base_resolves_relative_path_against_cwd() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("workspace");
        write_config(tmp.path(), r#"{"generatedImagesDir":".rebon/images"}"#);
        assert_eq!(
            generated_images_output_base_in_dir(tmp.path(), &cwd),
            cwd.join(".rebon").join("images")
        );
    }

    #[test]
    fn generated_images_output_base_empty_value_uses_default() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("workspace");
        write_config(tmp.path(), r#"{"generatedImagesDir":"  "}"#);
        assert_eq!(
            generated_images_output_base_in_dir(tmp.path(), &cwd),
            tmp.path().join("generated_images")
        );
    }

    #[test]
    fn coordinator_use_worktree_defaults_false() {
        let tmp = TempDir::new().unwrap();
        assert!(!saved_coordinator_use_worktree_in_dir(tmp.path()));
        write_config(tmp.path(), r#"{}"#);
        assert!(!saved_coordinator_use_worktree_in_dir(tmp.path()));
    }

    #[test]
    fn coordinator_use_worktree_reads_nested_and_top_level_aliases() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"coordinator":{"useWorktree":true}}"#);
        assert!(saved_coordinator_use_worktree_in_dir(tmp.path()));

        write_config(tmp.path(), r#"{"useWorktree":true}"#);
        assert!(saved_coordinator_use_worktree_in_dir(tmp.path()));

        write_config(tmp.path(), r#"{"coordinator":{"use_worktree":true}}"#);
        assert!(saved_coordinator_use_worktree_in_dir(tmp.path()));
    }

    #[test]
    fn save_coordinator_use_worktree_writes_nested_config() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"theme":"dark"}"#);

        save_coordinator_use_worktree_in_dir(tmp.path(), true).unwrap();

        let config = read_config_roundtrip(tmp.path()).unwrap();
        assert_eq!(config.extra["theme"], "dark");
        assert_eq!(config.extra["coordinator"]["useWorktree"], true);
        assert!(saved_coordinator_use_worktree_in_dir(tmp.path()));
    }

    #[test]
    fn provider_preset_lookup_is_case_insensitive() {
        let preset = provider_preset_by_id("DeEpSeEk").unwrap();
        assert_eq!(preset.id, "deepseek");
        assert_eq!(preset.display_name, "DeepSeek");
        assert!(provider_preset_by_id("missing").is_none());
    }

    #[test]
    fn provider_presets_include_current_defaults() {
        let preset = provider_preset_by_id("deepseek").unwrap();
        assert_eq!(preset.format, "openai");
        assert_eq!(preset.base_url, "https://api.deepseek.com");
        assert_eq!(preset.default_model, "deepseek-flash");
        assert_eq!(preset.api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(preset.vendor, rebon_api::ProviderVendor::DeepSeek);
        let models = preset.models();
        assert_eq!(models[0].id, "deepseek-v4-pro");
        assert_eq!(models[0].context_window, Some(1_000_000));
        assert_eq!(models[0].max_output_tokens, Some(384_000));
        assert!(models.iter().any(|m| m.id == "deepseek-flash"));

        let glm = provider_preset_by_id("glm").unwrap();
        assert_eq!(glm.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(glm.default_model, "glm-5.3");
        assert_eq!(glm.api_key_env, "ZHIPUAI_API_KEY");
        assert_eq!(
            glm.anthropic_base_url,
            Some("https://open.bigmodel.cn/api/anthropic")
        );

        let kimi = provider_preset_by_id("kimi").unwrap();
        assert_eq!(kimi.base_url, "https://api.moonshot.cn/v1");
        assert_eq!(kimi.default_model, "kimi-k3");
        assert_eq!(kimi.api_key_env, "MOONSHOT_API_KEY");

        let minimax = provider_preset_by_id("minimax").unwrap();
        assert_eq!(minimax.base_url, "https://api.minimaxi.com/v1");
        assert_eq!(minimax.default_model, "MiniMax-M3");
        assert_eq!(minimax.api_key_env, "MINIMAX_API_KEY");

        let openai = provider_preset_by_id("openai").unwrap();
        assert_eq!(openai.format, "openai-responses");
        assert_eq!(openai.base_url, "https://api.openai.com/v1");
        assert_eq!(openai.default_model, "gpt-5.6-sol");

        let ollama = provider_preset_by_id("ollama").unwrap();
        assert!(!ollama.api_key_required);
        assert!(ollama.default_model.is_empty());
        assert!(ollama.models().is_empty());
    }

    #[test]
    fn every_preset_is_well_formed() {
        let mut ids: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PROVIDER_PRESETS.len(), "preset ids collide");
        for preset in PROVIDER_PRESETS {
            assert!(
                VALID_PROVIDER_FORMATS.contains(&preset.format),
                "{}: format {}",
                preset.id,
                preset.format
            );
            assert!(
                preset.base_url.starts_with("https://") || preset.base_url.starts_with("http://"),
                "{}: base url",
                preset.id
            );
            assert!(
                !preset.base_url.ends_with('/'),
                "{}: trailing slash",
                preset.id
            );
            assert!(
                preset.key_url.starts_with("https://"),
                "{}: key url",
                preset.id
            );
            assert!(
                preset.docs_url.starts_with("https://"),
                "{}: docs url",
                preset.id
            );
            assert!(
                preset
                    .api_key_env
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_'),
                "{}: env var",
                preset.id
            );
            assert_ne!(
                preset.vendor,
                rebon_api::ProviderVendor::Unknown,
                "{}",
                preset.id
            );
            // The host must round-trip through detection, or a hand-edited
            // copy of the preset's URL would lose its dialect.
            assert_eq!(
                rebon_api::ProviderVendor::detect(preset.base_url),
                preset.vendor,
                "{}: host detection disagrees with the preset",
                preset.id
            );
            // A placeholder-bearing URL says so in its notes.
            let placeholders = preset.base_url_placeholders();
            for token in &placeholders {
                assert!(
                    preset.notes.contains(&format!("{{{token}}}")),
                    "{}: notes must explain {{{token}}}",
                    preset.id
                );
            }
            // The default model is either empty (discovery fills it) or in
            // the catalogue the preset seeds.
            if !preset.default_model.is_empty() && !preset.models().is_empty() {
                assert!(
                    preset.models().iter().any(|m| m.id == preset.default_model),
                    "{}: default model {} not in catalogue",
                    preset.id,
                    preset.default_model
                );
            }
            if let Some(url) = preset.anthropic_base_url {
                assert!(url.starts_with("https://"), "{}: anthropic url", preset.id);
            }
        }
    }

    #[test]
    fn bedrock_preset_prefixes_model_ids() {
        let bedrock = provider_preset_by_id("anthropic-bedrock").unwrap();
        assert!(bedrock
            .models()
            .iter()
            .all(|m| m.id.starts_with("anthropic.")));
        assert_eq!(bedrock.base_url_placeholders(), vec!["REGION"]);
        let vertex = provider_preset_by_id("anthropic-vertex").unwrap();
        assert_eq!(vertex.base_url_placeholders(), vec!["PROJECT"]);
        assert!(provider_preset_by_id("anthropic")
            .unwrap()
            .base_url_placeholders()
            .is_empty());
    }

    #[test]
    fn base_url_placeholders_only_sees_upper_case_tokens() {
        assert_eq!(
            base_url_placeholders("https://{A}.x/{B_2}/{lower}/{A}/{}"),
            vec!["A", "B_2"]
        );
        assert!(base_url_placeholders("https://api.example.com/v1").is_empty());
        assert!(base_url_placeholders("https://x/{unclosed").is_empty());
    }

    #[test]
    fn preset_for_entry_matches_by_vendor_and_url() {
        let deepseek = provider_preset_for_entry(None, "https://api.deepseek.com/").unwrap();
        assert_eq!(deepseek.id, "deepseek");
        // The vendor's Anthropic-compatible endpoint is the same preset.
        assert_eq!(
            provider_preset_for_entry(None, "https://api.deepseek.com/anthropic")
                .unwrap()
                .id,
            "deepseek"
        );
        // A pin on a gateway URL is not the preset: the URL is not the
        // preset's.
        assert!(provider_preset_for_entry(Some("deepseek"), "https://relay.example/v1").is_none());
        // Placeholders match one filled-in segment.
        assert_eq!(
            provider_preset_for_entry(None, "https://bedrock-mantle.us-east-1.api.aws/anthropic")
                .unwrap()
                .id,
            "anthropic-bedrock"
        );
        assert_eq!(
            provider_preset_for_entry(None, "https://myres.services.ai.azure.com/anthropic")
                .unwrap()
                .id,
            "anthropic-foundry"
        );
        assert_eq!(
            provider_preset_for_entry(
                None,
                "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global"
            )
            .unwrap()
            .id,
            "anthropic-vertex"
        );
        assert!(provider_preset_for_entry(None, "").is_none());
        assert!(provider_preset_for_entry(None, "https://api.anthropic.com/v2").is_none());
    }

    #[test]
    fn placeholder_url_matching_fills_exactly_one_segment() {
        let template = "https://bedrock-mantle.{REGION}.api.aws/anthropic";
        assert!(placeholder_url_matches(
            template,
            "REGION",
            "https://bedrock-mantle.eu-west-1.api.aws/anthropic"
        ));
        assert!(!placeholder_url_matches(
            template,
            "REGION",
            "https://bedrock-mantle..api.aws/anthropic"
        ));
        assert!(!placeholder_url_matches(
            template,
            "REGION",
            "https://bedrock-mantle.a/b.api.aws/anthropic"
        ));
        assert!(!placeholder_url_matches(template, "NOPE", "https://x"));
    }

    #[test]
    fn discovery_input_resolves_env_references_before_dialling() {
        let tmp = TempDir::new().unwrap();
        // An unset reference is reported by name rather than sent as an
        // empty bearer.
        let input = ProviderModelDiscoveryInput {
            format: "openai".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "$REBON_TEST_UNSET_KEY_FOR_DISCOVERY".into(),
            vendor: None,
            headers: Vec::new(),
        };
        let err = discover_models_for_provider_in(tmp.path(), &input).unwrap_err();
        assert!(err.contains("REBON_TEST_UNSET_KEY_FOR_DISCOVERY"), "{err}");

        // The OAuth sentinel without a login is the login hint.
        let input = ProviderModelDiscoveryInput {
            api_key: OPENAI_OAUTH_TOKEN_SENTINEL.into(),
            ..input
        };
        let err = discover_models_for_provider_in(tmp.path(), &input).unwrap_err();
        assert!(err.contains("/login"), "{err}");

        // A vendor with no list endpoint is reported as such, without a
        // network round trip.
        let input = ProviderModelDiscoveryInput {
            format: "openai".into(),
            base_url: "https://ark.cn-beijing.volces.com/api/v3".into(),
            api_key: "literal".into(),
            vendor: None,
            headers: Vec::new(),
        };
        let err = discover_models_for_provider_in(tmp.path(), &input).unwrap_err();
        assert!(err.contains("model list endpoint"), "{err}");
    }

    fn listed(id: &str, cw: Option<u32>, out: Option<u32>) -> rebon_api::DiscoveredModel {
        rebon_api::DiscoveredModel {
            id: id.into(),
            display_name: None,
            context_window: cw,
            max_output_tokens: out,
            limits_source: rebon_api::LimitsSource::None,
        }
    }

    #[test]
    fn merging_a_listing_keeps_user_limits_and_upgrades_bare_ids() {
        let mut provider: CustomProvider = serde_json::from_value(serde_json::json!({
            "name": "deepseek",
            "baseUrl": "https://api.deepseek.com",
            "apiKey": "k",
            "model": "deepseek-v4-flash",
            "models": [
                { "id": "deepseek-v4-flash", "contextWindow": 123 },
                "my-fine-tune"
            ]
        }))
        .unwrap();
        let merge = merge_discovered_models(
            &mut provider,
            &[
                listed("deepseek-v4-pro", Some(1_000_000), Some(384_000)),
                listed("deepseek-v4-flash", Some(1_000_000), Some(384_000)),
                listed("my-fine-tune", Some(64_000), None),
            ],
            Some("deepseek-v4-flash"),
        );
        assert_eq!(merge.added, vec!["deepseek-v4-pro".to_string()]);
        // flash kept its 123; the bare fine-tune got its window.
        assert_eq!(
            merge.windows_filled, 2,
            "flash gained max output, fine-tune gained a window"
        );
        let windows = resolve_model_context_windows(&provider);
        assert_eq!(windows.get("deepseek-v4-flash"), Some(&123));
        assert_eq!(windows.get("my-fine-tune"), Some(&64_000));
        assert_eq!(windows.get("deepseek-v4-pro"), Some(&1_000_000));
        assert_eq!(
            provider.model, "deepseek-v4-flash",
            "a valid default is untouched"
        );
        assert_eq!(
            provider.models.ids(),
            vec!["deepseek-v4-flash", "my-fine-tune", "deepseek-v4-pro"]
        );
    }

    /// The provider here carries the shape a read produces — the migration has
    /// already put the entry's own model in `models` — so a discovery listing
    /// that does not mention it must not push it out.
    #[test]
    fn merging_keeps_a_model_the_listing_does_not_mention() {
        let mut provider: CustomProvider = serde_json::from_value(serde_json::json!({
            "name": "kimi",
            "baseUrl": "https://api.moonshot.cn/v1",
            "apiKey": "k",
            "model": "moonshot-v1-8k",
            "models": ["moonshot-v1-8k"]
        }))
        .unwrap();
        merge_discovered_models(
            &mut provider,
            &[
                listed("kimi-k3", Some(1_048_576), None),
                listed("kimi-k2.6", None, None),
            ],
            Some("kimi-k3"),
        );
        assert_eq!(
            provider.models.ids(),
            vec!["moonshot-v1-8k", "kimi-k3", "kimi-k2.6"]
        );
        // The old default is still in the list, so it stays.
        assert_eq!(provider.model, "moonshot-v1-8k");

        let mut empty: CustomProvider = serde_json::from_value(serde_json::json!({
            "name": "ollama",
            "baseUrl": "http://localhost:11434/v1",
            "apiKey": "ollama"
        }))
        .unwrap();
        merge_discovered_models(
            &mut empty,
            &[
                listed("qwen3:8b", Some(40_960), None),
                listed("llama3.3", None, None),
            ],
            None,
        );
        assert_eq!(
            empty.model, "qwen3:8b",
            "first listed model when nothing was set"
        );
        // A map-shaped list is honoured too.
        let mut mapped: CustomProvider = serde_json::from_value(serde_json::json!({
            "name": "m",
            "baseUrl": "https://api.deepseek.com",
            "apiKey": "k",
            "model": "gone",
            "models": { "a": { "contextWindow": 1 } }
        }))
        .unwrap();
        merge_discovered_models(&mut mapped, &[listed("b", Some(2), None)], Some("b"));
        assert!(matches!(mapped.models, CustomProviderModels::Map(_)));
        assert_eq!(mapped.model, "b");
        assert_eq!(resolve_model_context_windows(&mapped).get("b"), Some(&2));
    }

    #[test]
    fn syncing_a_vendor_without_a_list_endpoint_uses_the_catalogue() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "ark",
            "openai",
            "https://ark.cn-beijing.volces.com/api/v3",
            "literal",
            "",
        )
        .unwrap();
        let sync = sync_custom_provider_models_in(tmp.path(), "ark").unwrap();
        assert!(sync.fallback_reason.is_some());
        assert_eq!(sync.source, "documented catalogue");
        assert!(sync.added.contains(&"doubao-seed-evolving".to_string()));
        assert_eq!(sync.info.model, "doubao-seed-evolving");

        let err = sync_custom_provider_models_in(tmp.path(), "nope").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn syncing_refuses_a_url_with_placeholders() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_from_preset_in(tmp.path(), "anthropic-bedrock", "token").unwrap();
        let err = sync_custom_provider_models_in(tmp.path(), "anthropic-bedrock").unwrap_err();
        assert!(err.to_string().contains("{REGION}"), "{err}");
    }

    #[test]
    fn regions_split_the_picker() {
        let global: Vec<&str> = provider_presets_in(PresetRegion::Global)
            .map(|p| p.id)
            .collect();
        let china: Vec<&str> = provider_presets_in(PresetRegion::China)
            .map(|p| p.id)
            .collect();
        assert!(global.contains(&"openai") && global.contains(&"ollama"));
        assert!(china.contains(&"deepseek") && china.contains(&"qwen"));
        assert_eq!(global.len() + china.len(), PROVIDER_PRESETS.len());
        assert_eq!(PresetRegion::Global.label(), "国际");
    }

    #[test]
    fn wire_family_follows_the_format_string() {
        assert_eq!(
            wire_family_for_format("anthropic"),
            rebon_api::WireFamily::Anthropic
        );
        assert_eq!(
            wire_family_for_format("openai-responses"),
            rebon_api::WireFamily::OpenAiResponses
        );
        assert_eq!(
            wire_family_for_format("openai"),
            rebon_api::WireFamily::OpenAiChat
        );
        assert_eq!(
            wire_family_for_format(""),
            rebon_api::WireFamily::OpenAiChat
        );
    }

    #[test]
    fn preset_add_pins_the_vendor_and_tolerates_a_keyless_endpoint() {
        let tmp = TempDir::new().unwrap();
        let info = add_custom_provider_from_preset_in(tmp.path(), "ollama", "").unwrap();
        assert_eq!(info.name, "ollama");
        assert_eq!(
            info.api_key, "ollama",
            "a placeholder keeps the header well-formed"
        );
        let stored = read_config_roundtrip(tmp.path()).unwrap();
        let entry = stored
            .custom_providers
            .iter()
            .find(|p| p.name == "ollama")
            .unwrap();
        assert_eq!(entry.vendor.as_deref(), Some("ollama"));

        let err = add_custom_provider_from_preset_in(tmp.path(), "kimi", "  ").unwrap_err();
        assert!(err.to_string().contains("needs an API key"), "{err}");
    }

    #[test]
    fn add_custom_provider_from_preset_stores_defaults_and_activates() {
        let tmp = TempDir::new().unwrap();
        let info = add_custom_provider_from_preset_in(tmp.path(), "DeepSeek", "$DEEPSEEK_API_KEY")
            .unwrap();
        assert_eq!(info.name, "deepseek");
        assert_eq!(info.format, "openai");
        assert_eq!(info.base_url, "https://api.deepseek.com");
        assert_eq!(info.api_key, "$DEEPSEEK_API_KEY");
        assert_eq!(info.model, "deepseek-flash");
        assert_eq!(
            info.models,
            vec![
                "deepseek-v4-pro".to_string(),
                "deepseek-flash".to_string(),
                "deepseek-v4-flash-vision-exp".to_string()
            ]
        );
        let resolved = resolve_from_dir_with(tmp.path(), None)
            .unwrap()
            .expect("active preset provider");
        assert_eq!(resolved.vendor, rebon_api::ProviderVendor::DeepSeek);
        assert_eq!(
            resolved.model_context_windows.get("deepseek-flash"),
            Some(&1_000_000)
        );
        assert_eq!(
            resolved.model_output_token_limits.get("deepseek-v4-pro"),
            Some(&384_000)
        );
        assert_eq!(
            resolved.model_context_windows.get("deepseek-v4-pro"),
            Some(&1_000_000)
        );
        assert_eq!(
            get_active_custom_provider_name_from(tmp.path()).as_deref(),
            Some("deepseek")
        );
    }

    /// A provider written before `models` existed is brought up to the current
    /// shape once, at the read entry, and the file records that it was — so no
    /// later read looks at the scalar again.
    #[test]
    fn a_provider_with_only_model_is_migrated_once_and_stamped() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "customProviders":[
                    {"name":"ds","format":"openai",
                     "baseUrl":"https://api.deepseek.com/v1",
                     "apiKey":"sk-x","model":"deepseek-chat"}
                ]
            }"#,
        );

        let providers = list_custom_providers_from(tmp.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].model, "deepseek-chat");
        assert_eq!(providers[0].models, vec!["deepseek-chat".to_string()]);

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_json_path(tmp.path())).unwrap())
                .unwrap();
        assert_eq!(
            on_disk[CONFIG_SCHEMA_VERSION_KEY],
            serde_json::json!(CONFIG_SCHEMA_VERSION),
            "the migration stamps the file it rewrote"
        );
        assert_eq!(
            on_disk["customProviders"][0]["models"],
            serde_json::json!(["deepseek-chat"]),
            "the seeded list is written back, not only returned"
        );

        // Take the list away again but keep the stamp: a stamped file is never
        // inspected for the old shape, so nothing seeds it a second time.
        write_config(
            tmp.path(),
            &format!(
                r#"{{
                    "{CONFIG_SCHEMA_VERSION_KEY}": {CONFIG_SCHEMA_VERSION},
                    "customProviders":[
                        {{"name":"ds","format":"openai",
                         "baseUrl":"https://api.deepseek.com/v1",
                         "apiKey":"sk-x","model":"deepseek-chat"}}
                    ]
                }}"#
            ),
        );
        let again = list_custom_providers_from(tmp.path());
        assert_eq!(again[0].model, "deepseek-chat");
        assert!(
            again[0].models.is_empty(),
            "a stamped file is read as-is, with no seeding"
        );
    }

    /// A config home with nothing in it is not given a `config.json` just so
    /// the migration can stamp one.
    #[test]
    fn the_migration_does_not_create_a_config_for_a_fresh_install() {
        let tmp = TempDir::new().unwrap();
        assert!(list_custom_providers_from(tmp.path()).is_empty());
        assert!(!config_json_path(tmp.path()).exists());
    }

    #[test]
    fn add_custom_provider_seeds_models_with_initial_model() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "ds",
            "openai",
            "https://api.deepseek.com/v1",
            "sk-x",
            "deepseek-chat",
        )
        .unwrap();
        let providers = list_custom_providers_from(tmp.path());
        assert_eq!(providers[0].model, "deepseek-chat");
        assert_eq!(providers[0].models, vec!["deepseek-chat".to_string()]);
        assert_eq!(
            get_active_custom_provider_name_from(tmp.path()).as_deref(),
            Some("ds")
        );
    }

    #[test]
    fn update_custom_provider_rewrites_fields_renames_and_activates() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "ds",
            "openai",
            "https://old.example.com",
            "sk-old",
            "old-model",
        )
        .unwrap();
        let info = update_custom_provider_in(
            tmp.path(),
            "DS",
            "ds2",
            "openai-responses",
            "https://new.example.com",
            "$NEW",
            "new-model",
        )
        .unwrap();

        assert_eq!(info.name, "ds2");
        assert_eq!(info.format, "openai-responses");
        assert_eq!(info.base_url, "https://new.example.com");
        assert_eq!(info.api_key, "$NEW");
        assert_eq!(info.model, "new-model");
        assert_eq!(
            info.models,
            vec!["old-model".to_string(), "new-model".to_string()]
        );
        assert_eq!(
            get_active_custom_provider_name_from(tmp.path()).as_deref(),
            Some("ds2")
        );
    }

    #[test]
    fn update_custom_provider_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(tmp.path(), "a", "openai", "https://a", "ka", "ma").unwrap();
        add_custom_provider_in(tmp.path(), "b", "openai", "https://b", "kb", "mb").unwrap();
        let err =
            update_custom_provider_in(tmp.path(), "a", "B", "openai", "https://new", "k", "m")
                .unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    #[test]
    fn add_custom_provider_model_appends_and_switches_active() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "ds",
            "openai",
            "https://api.deepseek.com/v1",
            "sk-x",
            "deepseek-chat",
        )
        .unwrap();
        let info = add_custom_provider_model_in(tmp.path(), "ds", "deepseek-coder").unwrap();
        assert_eq!(info.model, "deepseek-coder");
        assert_eq!(
            info.models,
            vec!["deepseek-chat".to_string(), "deepseek-coder".to_string()]
        );
        // Case-insensitive lookup on provider name.
        let info2 = add_custom_provider_model_in(tmp.path(), "DS", "deepseek-v3").unwrap();
        assert_eq!(info2.models.len(), 3);
        assert_eq!(info2.model, "deepseek-v3");
    }

    #[test]
    fn add_custom_provider_model_keeps_the_model_a_pre_models_config_had() {
        // Only `model`, no `models`. Adding a second one must not lose the
        // first: the read entry seeds the list, so `add_model` appends to it.
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "customProviders":[
                    {"name":"ds","format":"openai",
                     "baseUrl":"https://api.deepseek.com/v1",
                     "apiKey":"sk-x","model":"deepseek-chat"}
                ]
            }"#,
        );
        let info = add_custom_provider_model_in(tmp.path(), "ds", "deepseek-coder").unwrap();
        assert_eq!(
            info.models,
            vec!["deepseek-chat".to_string(), "deepseek-coder".to_string()]
        );
        assert_eq!(info.model, "deepseek-coder");
    }

    #[test]
    fn set_custom_provider_model_switches_and_upserts_model() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "ds",
            "openai",
            "https://api.deepseek.com/v1",
            "sk-x",
            "deepseek-chat",
        )
        .unwrap();

        let info = set_custom_provider_model_in(tmp.path(), "ds", "deepseek-coder").unwrap();
        assert_eq!(info.model, "deepseek-coder");
        assert_eq!(
            info.models,
            vec!["deepseek-chat".to_string(), "deepseek-coder".to_string()]
        );
        let info2 = set_custom_provider_model_in(tmp.path(), "DS", "deepseek-chat").unwrap();
        assert_eq!(info2.model, "deepseek-chat");
        assert_eq!(info2.models.len(), 2);
    }

    #[test]
    fn provider_model_options_appends_builtin_catalogue_for_oauth_sentinel() {
        let provider = CustomProviderInfo {
            name: "openai".into(),
            format: "openai-responses".into(),
            base_url: OPENAI_OAUTH_PROVIDER_BASE_URL.into(),
            api_key: OPENAI_OAUTH_TOKEN_SENTINEL.into(),
            model: "gpt-5.6".into(),
            models: vec!["gpt-5.6".into(), "gpt-5.4".into()],
        };
        let options = provider_model_options(&provider);
        // User's own entries stay first and are not duplicated.
        assert_eq!(
            &options[..2],
            &["gpt-5.6".to_string(), "gpt-5.4".to_string()]
        );
        for model in OPENAI_OAUTH_PROVIDER_MODELS {
            assert!(options.iter().any(|m| m == model), "missing {model}");
        }
        assert_eq!(
            options.iter().filter(|m| m.as_str() == "gpt-5.4").count(),
            1
        );
    }

    /// A provider entry lists the models its owner cared to write down,
    /// which was never the same as the models the key can reach. The
    /// catalogue fills in the rest so a picker shows the vendor's whole
    /// line-up without anyone editing config.
    #[test]
    fn provider_model_choices_append_the_vendors_catalogue_after_the_configured_ones() {
        let provider = CustomProviderInfo {
            name: "deepseek".into(),
            format: "openai".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "sk-literal".into(),
            model: "deepseek-v4-pro".into(),
            models: vec!["deepseek-v4-pro".into()],
        };
        let choices = provider_model_choices(&provider);
        assert_eq!(choices[0].id, "deepseek-v4-pro");
        assert!(choices[0].configured, "the user's own entry stays first");
        assert!(
            choices.len() > 1,
            "the deepseek catalogue has more than one model"
        );
        assert!(
            choices[1..].iter().all(|choice| !choice.configured),
            "catalogue rows are not marked as configured"
        );
        assert!(
            choices
                .iter()
                .any(|choice| choice.detail.as_deref().is_some_and(|d| d.contains("ctx"))),
            "the catalogue supplies context/price detail"
        );
        let ids: Vec<&str> = choices.iter().map(|choice| choice.id.as_str()).collect();
        assert_eq!(
            ids.iter().filter(|id| **id == "deepseek-v4-pro").count(),
            1,
            "a configured model is not repeated by the catalogue"
        );
    }

    /// `/model refresh` writes the table it downloaded and a later start
    /// reads it back. Both halves are one line each and neither has a
    /// failure the caller sees, which is exactly how a cache goes stale
    /// unnoticed — so the round trip is checked here.
    #[test]
    fn a_refreshed_model_table_survives_the_cache_round_trip() {
        let tmp = TempDir::new().unwrap();
        let table = r#"{
          "source": "https://reboncode.ai/api/models",
          "generated_at": "2026-09-09T00:00:00Z",
          "providers": {"openai": {"id": "openai", "name": "OpenAI", "models": {
            "gpt-7-nova": {"id": "gpt-7-nova", "name": "GPT-7 Nova",
              "limit": {"context": 2000000, "output": 128000},
              "modes": {"fast": {"service_tier": "priority"}}}
          }}}
        }"#;
        assert_eq!(save_model_table_in(tmp.path(), table).unwrap(), 1);
        assert!(model_table_cache_path_in(tmp.path()).is_file());

        rebon_api::model_table::clear_overlay();
        assert_eq!(install_cached_model_table_in(tmp.path()), Some(1));
        let row = rebon_api::model_table::model(Some("openai"), "gpt-7-nova").expect("cached row");
        assert_eq!(row.limit.context, Some(2_000_000));

        // A table that will not parse never reaches the cache.
        let cached = std::fs::read_to_string(model_table_cache_path_in(tmp.path())).unwrap();
        assert!(save_model_table_in(tmp.path(), "{not json").is_err());
        assert_eq!(
            std::fs::read_to_string(model_table_cache_path_in(tmp.path())).unwrap(),
            cached
        );
        rebon_api::model_table::clear_overlay();
    }

    /// An endpoint no catalogue covers still answers with exactly what the
    /// user configured — the supplement can only add, never replace.
    #[test]
    fn provider_model_options_untouched_for_an_unrecognised_endpoint() {
        let provider = CustomProviderInfo {
            name: "relay".into(),
            format: "openai".into(),
            base_url: "https://relay.example.com/v1".into(),
            api_key: "sk-literal".into(),
            model: "house-model".into(),
            models: vec!["house-model".into()],
        };
        assert_eq!(
            provider_model_options(&provider),
            vec!["house-model".to_string()]
        );
    }

    #[test]
    fn add_custom_provider_model_rejects_duplicate() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "ds",
            "openai",
            "https://api.deepseek.com/v1",
            "sk-x",
            "deepseek-chat",
        )
        .unwrap();
        let err = add_custom_provider_model_in(tmp.path(), "ds", "deepseek-chat").unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    #[test]
    fn add_custom_provider_model_rejects_missing_provider() {
        let tmp = TempDir::new().unwrap();
        let err = add_custom_provider_model_in(tmp.path(), "ghost", "x").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"));
    }

    #[test]
    fn add_custom_provider_model_rejects_empty_name() {
        let tmp = TempDir::new().unwrap();
        add_custom_provider_in(
            tmp.path(),
            "ds",
            "openai",
            "https://api.deepseek.com/v1",
            "sk-x",
            "deepseek-chat",
        )
        .unwrap();
        let err = add_custom_provider_model_in(tmp.path(), "ds", "   ").unwrap_err();
        assert!(format!("{err}").contains("empty"));
    }

    // ── upsert_openai_oauth_provider_in ──────────────────────────────

    #[test]
    fn upsert_openai_oauth_provider_inserts_new_entry() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"customProviders":[]}"#);
        upsert_openai_oauth_provider_in(tmp.path(), &["gpt-5.4".to_string()]).unwrap();
        let raw = fs::read_to_string(config_json_path(tmp.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["activeCustomProvider"], "openai");
        let p = &parsed["customProviders"][0];
        assert_eq!(p["name"], "openai");
        assert_eq!(p["format"], "openai-responses");
        assert_eq!(p["baseUrl"], OPENAI_OAUTH_PROVIDER_BASE_URL);
        assert_eq!(p["apiKey"], OPENAI_OAUTH_TOKEN_SENTINEL);
        assert_eq!(p["model"], "gpt-5.4");
    }

    #[test]
    fn upsert_openai_oauth_provider_defaults_model_when_empty() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"customProviders":[]}"#);
        upsert_openai_oauth_provider_in(tmp.path(), &[]).unwrap();
        let raw = fs::read_to_string(config_json_path(tmp.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed["customProviders"][0]["model"],
            OPENAI_OAUTH_PROVIDER_MODEL
        );
    }

    #[test]
    fn upsert_openai_oauth_provider_overwrites_existing_entry() {
        let tmp = TempDir::new().unwrap();
        // Pre-populate with a stale "openai" entry carrying a real key
        // (not the sentinel). The upsert must rewrite it.
        write_config(
            tmp.path(),
            r#"{
                "customProviders":[
                    {"name":"openai","format":"openai",
                     "baseUrl":"https://old","apiKey":"sk-old","model":"old-m"}
                ]
            }"#,
        );
        upsert_openai_oauth_provider_in(tmp.path(), &["gpt-5.4".to_string()]).unwrap();
        let raw = fs::read_to_string(config_json_path(tmp.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let p = &parsed["customProviders"][0];
        assert_eq!(p["format"], "openai-responses");
        assert_eq!(p["baseUrl"], OPENAI_OAUTH_PROVIDER_BASE_URL);
        assert_eq!(p["apiKey"], OPENAI_OAUTH_TOKEN_SENTINEL);
        assert_eq!(p["model"], "gpt-5.4");
    }

    #[test]
    fn upsert_openai_oauth_provider_preserves_other_providers() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "customProviders":[
                    {"name":"deepseek","format":"openai",
                     "baseUrl":"https://api.deepseek.com/v1",
                     "apiKey":"sk-ds","model":"deepseek-chat"}
                ]
            }"#,
        );
        upsert_openai_oauth_provider_in(tmp.path(), &["gpt-5.4".to_string()]).unwrap();
        let providers = list_custom_providers_from(tmp.path());
        assert_eq!(providers.len(), 2);
        assert!(providers
            .iter()
            .any(|p| p.name == "deepseek" && p.api_key == "sk-ds"));
        assert!(providers
            .iter()
            .any(|p| p.name == "openai" && p.api_key == OPENAI_OAUTH_TOKEN_SENTINEL));
    }

    #[test]
    fn upsert_openai_oauth_provider_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"customProviders":[]}"#);
        upsert_openai_oauth_provider_in(tmp.path(), &["gpt-5.4".to_string()]).unwrap();
        upsert_openai_oauth_provider_in(tmp.path(), &["gpt-5.4".to_string()]).unwrap();
        let providers = list_custom_providers_from(tmp.path());
        // Only a single "openai" entry should exist after two calls.
        assert_eq!(providers.iter().filter(|p| p.name == "openai").count(), 1);
    }

    #[test]
    fn upsert_openai_oauth_provider_activates_entry() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"deepseek",
                "customProviders":[
                    {"name":"deepseek","format":"openai",
                     "baseUrl":"https://api.deepseek.com/v1",
                     "apiKey":"sk-ds","model":"deepseek-chat"}
                ]
            }"#,
        );
        upsert_openai_oauth_provider_in(tmp.path(), &["gpt-5.4".to_string()]).unwrap();
        let raw = fs::read_to_string(config_json_path(tmp.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // The upsert flips activeCustomProvider to the OAuth entry so
        // the next session picks it up without another user step.
        assert_eq!(parsed["activeCustomProvider"], "openai");
    }

    #[test]
    fn provider_setup_status_is_fresh_without_config_or_environment() {
        let tmp = TempDir::new().unwrap();
        let status = provider_setup_status_in_with_env(tmp.path(), |_| false);

        assert!(!status.onboarding_completed);
        assert!(status.providers.is_empty());
        assert!(status.environment_providers.is_empty());
        assert!(status.config_error.is_none());
        assert!(!status.should_confirm_reconfiguration());
    }

    #[test]
    fn provider_setup_status_preserves_completed_onboarding_without_provider() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), r#"{"hasCompletedOnboarding":true}"#);

        let status = provider_setup_status_in_with_env(tmp.path(), |_| false);

        assert!(status.onboarding_completed);
        assert!(!status.has_provider_configuration());
        assert!(status.should_confirm_reconfiguration());
        assert!(has_completed_onboarding_in(tmp.path()));
    }

    #[test]
    fn provider_setup_status_summarizes_saved_providers_without_secrets() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "hasCompletedOnboarding":true,
                "activeCustomProvider":"stored",
                "customProviders":[
                    {"name":"stored","baseUrl":"https://stored","apiKey":"sk-secret-value","model":"stored-model"},
                    {"name":"env","baseUrl":"https://env","apiKey":"$CUSTOM_PROVIDER_KEY","model":"env-model"},
                    {"name":"openai","baseUrl":"https://oauth","apiKey":"$OPENAI_OAUTH_TOKEN","model":"gpt-5.4"}
                ]
            }"#,
        );

        let status = provider_setup_status_in_with_env(tmp.path(), |variable| {
            variable == "CUSTOM_PROVIDER_KEY"
        });

        assert_eq!(status.providers.len(), 3);
        assert_eq!(status.active_provider().unwrap().name, "stored");
        assert_eq!(status.active_provider().unwrap().model, "stored-model");
        assert_eq!(
            status.providers[0].credential,
            ProviderSetupCredential::Stored
        );
        assert_eq!(
            status.providers[1].credential,
            ProviderSetupCredential::Environment {
                variable: "CUSTOM_PROVIDER_KEY".to_string(),
                available: true,
            }
        );
        assert_eq!(
            status.providers[2].credential,
            ProviderSetupCredential::OpenAiOAuth
        );
        assert!(!format!("{status:?}").contains("sk-secret-value"));
    }

    #[test]
    fn provider_setup_status_detects_environment_only_providers() {
        let tmp = TempDir::new().unwrap();
        let status = provider_setup_status_in_with_env(tmp.path(), |variable| {
            matches!(variable, "DEEPSEEK_API_KEY" | "OPENAI_API_KEY")
        });

        assert_eq!(
            status.environment_providers,
            vec![
                ProviderSetupEnvironment {
                    provider: "DeepSeek".to_string(),
                    variable: "DEEPSEEK_API_KEY".to_string(),
                },
                ProviderSetupEnvironment {
                    provider: "OpenAI".to_string(),
                    variable: "OPENAI_API_KEY".to_string(),
                },
            ]
        );
        assert!(status.has_provider_configuration());
        assert!(status.should_confirm_reconfiguration());
    }

    #[test]
    fn provider_setup_status_marks_unavailable_environment_reference() {
        let tmp = TempDir::new().unwrap();
        write_config(
            tmp.path(),
            r#"{
                "activeCustomProvider":"custom",
                "customProviders":[
                    {"name":"custom","baseUrl":"https://custom","apiKey":"${MISSING_KEY}","model":"custom-model"}
                ]
            }"#,
        );

        let status = provider_setup_status_in_with_env(tmp.path(), |_| false);

        assert_eq!(
            status.providers[0].credential,
            ProviderSetupCredential::Environment {
                variable: "MISSING_KEY".to_string(),
                available: false,
            }
        );
        assert!(status.should_confirm_reconfiguration());
    }

    #[test]
    fn provider_setup_status_guards_unreadable_existing_config() {
        let tmp = TempDir::new().unwrap();
        write_config(tmp.path(), "{not-json");

        let status = provider_setup_status_in_with_env(tmp.path(), |_| false);

        assert!(status.config_error.is_some());
        assert!(status.should_confirm_reconfiguration());
    }
}
