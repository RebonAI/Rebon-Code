//! The `settings` and `credentials` kernel seats — the connection facts a
//! real dsh llm adapter consumes (`ctx.settings` / `ctx.credentials`).
//!
//! Security model:
//! - `settings` has two halves. Reading **somebody else's** config is
//!   read-only, section-whitelisted, and **recursively strips secret-shaped
//!   keys** (apiKey/token/…) from whatever it returns, so a plugin can read
//!   connection facts (baseUrl, model catalog) but never a credential through
//!   this seat. Reading and writing **its own** namespace — `plugins.<id>` in
//!   the `settings.json` chain — is what a plugin does with its own settings,
//!   and every gate on it is fail-closed: another plugin's namespace, the
//!   `enabled` switch, and a key the plugin never declared are all `Err`
//!   rather than a quiet no-op.
//! - `credentials` is the only doorway to secrets and it is **fail-closed**:
//!   every `get` runs the `credentials/authorize` JSON waterfall first, and
//!   with no authorizer listening (or none granting), the request is denied.
//!   Hosts grant access by registering a listener that answers
//!   `{"allow": true}`; interactive UI approval can attach on the same seam
//!   later. Resolved keys are returned to the caller and never logged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rebon_kernel::{Context, JsonService, KernelError, Plugin, PluginMeta, SettingType};

/// JSON-plane names of the two seats.
///
/// The settings name is the kernel's, because the kernel declares each
/// plugin's namespace on the seat as it loads and has to know what to call.
pub use rebon_kernel::SETTINGS_SERVICE;
pub const CREDENTIALS_SERVICE: &str = "credentials";
/// Waterfall consulted before any credential leaves the process kernel.
pub const CREDENTIALS_AUTHORIZE_EVENT: &str = "credentials/authorize";

/// Config sections a plugin may read through the settings seat.
///
/// Somebody else's keys, so read-only and enumerated. A plugin's *own* keys
/// live under `plugins.<id>` and are reached through `read` / `write`, which
/// have nothing to do with this list.
const ALLOWED_SECTIONS: &[&str] = &["customProviders", "model", "modelPricing"];

/// The params key carrying which plugin is calling.
///
/// The plane **overwrites** it on every `seat/call` with the identity the host
/// assigned, so a Node plugin cannot name someone else by putting this in its
/// own params. A same-process Rust plugin sets it itself — it is already
/// trusted with an `Arc` to everything the kernel holds, and a plugin that
/// wanted another plugin's settings could write the file directly. What the
/// key buys there is that an *accident* is an error rather than a silent
/// cross-namespace write.
pub const CALLER_PLUGIN_ID: &str = "callerPluginId";

/// What a plugin declared it may write under its own namespace.
#[derive(Clone, Debug, Default, PartialEq)]
struct DeclaredKeys {
    keys: BTreeMap<String, (SettingType, Option<serde_json::Value>)>,
}

fn read_config(config_dir: &Path) -> Result<serde_json::Value, KernelError> {
    let path = config_dir.join("config.json");
    let bytes = std::fs::read(&path)
        .map_err(|err| KernelError::Other(format!("cannot read {}: {err}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| KernelError::Other(format!("cannot parse {}: {err}", path.display())))
}

/// Does this object key look like it holds a secret? Deliberately broad:
/// a stripped non-secret is an inconvenience, a leaked secret is not.
fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|marker| key.contains(marker))
}

fn strip_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, _| !is_secret_key(key));
            for child in map.values_mut() {
                strip_secrets(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                strip_secrets(item);
            }
        }
        _ => {}
    }
}

/// The settings seat: somebody else's config read-only, its own read-write.
pub struct SettingsService {
    config_dir: PathBuf,
    /// The project the settings chain is resolved against — `settings.json`,
    /// then `.rebon/settings.json`, then `.rebon/settings.local.json`.
    cwd: PathBuf,
    declared: Mutex<BTreeMap<String, DeclaredKeys>>,
}

impl SettingsService {
    pub fn new(config_dir: PathBuf) -> Arc<Self> {
        Self::new_in(
            config_dir,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
    }

    /// [`SettingsService::new`] with the project directory named, so a test
    /// pins both layers of the chain instead of inheriting the process's.
    pub fn new_in(config_dir: PathBuf, cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            config_dir,
            cwd,
            declared: Mutex::new(BTreeMap::new()),
        })
    }

    /// Which plugin is calling, or a refusal.
    ///
    /// Fail-closed: with no identity there is no namespace to be in, so the
    /// namespace half of the seat answers nothing at all. The read-only
    /// `get`/`list` half needs no identity and does not go through here.
    fn caller(params: &serde_json::Value) -> Result<String, KernelError> {
        params
            .get(CALLER_PLUGIN_ID)
            .and_then(|id| id.as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                KernelError::Other(format!(
                    "the settings namespace methods need a `{CALLER_PLUGIN_ID}`; the plugin \
                     plane supplies it, and a host calling directly must say who it is for"
                ))
            })
    }

    /// The namespace this call is about: the caller's own, always.
    ///
    /// An explicit `namespace` is accepted so a call reads plainly, and is
    /// checked rather than trusted — naming another plugin is refused, not
    /// silently redirected to your own.
    fn namespace_of(params: &serde_json::Value) -> Result<String, KernelError> {
        let caller = Self::caller(params)?;
        match params.get("namespace").and_then(|n| n.as_str()) {
            Some(asked) if asked.trim() != caller => Err(KernelError::Other(format!(
                "plugin `{caller}` may only read and write `plugins.{caller}`, not \
                 `plugins.{}`",
                asked.trim()
            ))),
            _ => Ok(caller),
        }
    }

    fn declare(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let namespace = Self::namespace_of(params)?;
        let mut keys = BTreeMap::new();
        let entries = params
            .get("keys")
            .and_then(|k| k.as_array())
            .ok_or_else(|| KernelError::Other("settings declare requires a `keys` array".into()))?;
        for entry in entries {
            let name = entry
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| {
                    KernelError::Other("every declared settings key needs a `name`".into())
                })?;
            if name == rebon_config::PLUGIN_ENABLED_KEY {
                return Err(KernelError::Other(format!(
                    "`{}` under plugins.{namespace} is the kernel's switch and cannot be \
                     declared as a plugin key",
                    rebon_config::PLUGIN_ENABLED_KEY
                )));
            }
            // An undeclared type is `any`: the declaration still says the key
            // exists, which is what makes it writable.
            let ty = match entry.get("type").and_then(|t| t.as_str()) {
                Some(raw) => SettingType::parse(raw).ok_or_else(|| {
                    KernelError::Other(format!(
                        "settings key `{name}` declares unknown type `{raw}`"
                    ))
                })?,
                None => SettingType::Any,
            };
            let default = entry.get("default").cloned();
            if let Some(default) = &default {
                if !ty.accepts(default) {
                    return Err(KernelError::Other(format!(
                        "settings key `{name}` declares a {} default for a {} key",
                        kind_of(default),
                        ty.as_str()
                    )));
                }
            }
            keys.insert(name.to_string(), (ty, default));
        }
        let count = keys.len();
        self.declared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(namespace.clone(), DeclaredKeys { keys });
        Ok(serde_json::json!({ "namespace": namespace, "keys": count }))
    }

    fn undeclare(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let namespace = Self::namespace_of(params)?;
        let removed = self
            .declared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&namespace)
            .is_some();
        Ok(serde_json::json!({ "namespace": namespace, "removed": removed }))
    }

    fn declaration(&self, namespace: &str) -> Option<DeclaredKeys> {
        self.declared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(namespace)
            .cloned()
    }

    /// Everything `plugins.<caller>` says, with declared defaults filled in.
    ///
    /// Secrets are stripped here too. A plugin that keeps a token in its own
    /// settings has put it in a file this seat reads, and the seat's rule is
    /// that credentials come from the `credentials` seat or not at all.
    fn read(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let namespace = Self::namespace_of(params)?;
        let stored = rebon_config::plugin_settings_in(&self.config_dir, &self.cwd, &namespace);
        let mut merged = serde_json::Map::new();
        if let Some(declaration) = self.declaration(&namespace) {
            for (name, (_, default)) in &declaration.keys {
                if let Some(default) = default {
                    merged.insert(name.clone(), default.clone());
                }
            }
        }
        merged.extend(stored);
        let mut value = serde_json::Value::Object(merged);
        strip_secrets(&mut value);
        Ok(value)
    }

    fn write(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let namespace = Self::namespace_of(params)?;
        let patch = params
            .get("patch")
            .and_then(|p| p.as_object())
            .ok_or_else(|| KernelError::Other("settings write requires a `patch` object".into()))?;
        let Some(declaration) = self.declaration(&namespace) else {
            return Err(KernelError::Other(format!(
                "plugin `{namespace}` declared no settings keys; a plugin writes the keys it \
                 declared and nothing else"
            )));
        };
        for (name, value) in patch {
            let Some((ty, _)) = declaration.keys.get(name) else {
                return Err(KernelError::Other(format!(
                    "`{name}` is not a settings key `{namespace}` declared (it declares: {})",
                    if declaration.keys.is_empty() {
                        "none".to_owned()
                    } else {
                        declaration
                            .keys
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )));
            };
            // A null is a removal, which any declared key accepts whatever its
            // type: it puts the key back to its default.
            if !value.is_null() && !ty.accepts(value) {
                return Err(KernelError::Other(format!(
                    "settings key `{name}` is declared {} and was given {}",
                    ty.as_str(),
                    kind_of(value)
                )));
            }
        }
        rebon_config::save_plugin_settings_in_dir(&self.config_dir, &namespace, patch)
            .map_err(|error| KernelError::Other(format!("{error}")))?;
        Ok(serde_json::json!({ "namespace": namespace, "written": patch.len() }))
    }

    fn get(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let section = params
            .get("section")
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                KernelError::Other("settings get requires a non-empty `section`".into())
            })?;
        if !ALLOWED_SECTIONS.contains(&section) {
            return Err(KernelError::Other(format!(
                "settings section `{section}` is not readable by plugins (allowed: {})",
                ALLOWED_SECTIONS.join(", ")
            )));
        }
        let config = read_config(&self.config_dir)?;
        let mut value = config
            .get(section)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        strip_secrets(&mut value);
        Ok(value)
    }
}

impl JsonService for SettingsService {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        match method {
            "get" => self.get(&params),
            "list" => Ok(serde_json::json!({
                "sections": ALLOWED_SECTIONS,
                "namespaces": self
                    .declared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
            })),
            "declare" => self.declare(&params),
            "undeclare" => self.undeclare(&params),
            "read" => self.read(&params),
            "write" => self.write(&params),
            other => Err(KernelError::Other(format!(
                "settings has no method `{other}`"
            ))),
        }
    }
}

/// What a JSON value is, for a type-mismatch message.
fn kind_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// A plugin's own settings, bound to the plugin that owns them.
///
/// The typed way for a Rust plugin to reach the seat: the id is bound once,
/// so a caller cannot name the wrong namespace by mistyping a JSON field. The
/// declaration itself comes from [`PluginMeta::settings`](rebon_kernel::PluginMeta),
/// which the kernel hands to the seat as the plugin loads — this is for
/// reading and writing afterwards.
pub struct PluginSettings {
    ctx: Context,
    plugin_id: String,
}

impl PluginSettings {
    pub fn new(ctx: &Context, plugin_id: impl Into<String>) -> Self {
        Self {
            ctx: ctx.clone(),
            plugin_id: plugin_id.into(),
        }
    }

    fn params(&self, extra: serde_json::Value) -> serde_json::Value {
        let mut params = serde_json::json!({ CALLER_PLUGIN_ID: self.plugin_id });
        if let (Some(target), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        params
    }

    /// Everything `plugins.<id>` says, declared defaults included.
    pub fn read(&self) -> Result<serde_json::Value, KernelError> {
        self.ctx
            .call_json(SETTINGS_SERVICE, "read", self.params(serde_json::json!({})))
    }

    /// Merge keys into `plugins.<id>` in the user settings file. A `null`
    /// value removes the key.
    pub fn write(&self, patch: serde_json::Value) -> Result<serde_json::Value, KernelError> {
        self.ctx.call_json(
            SETTINGS_SERVICE,
            "write",
            self.params(serde_json::json!({ "patch": patch })),
        )
    }

    /// Whether a [`ConfigChanged`](rebon_kernel::ConfigChanged) is this
    /// plugin's business.
    ///
    /// A settings write through the seat names the namespace it touched, and
    /// everything else names none — so "no namespace" means "re-read", not
    /// "ignore".
    pub fn is_mine(&self, event: &rebon_kernel::ConfigChanged) -> bool {
        event.kind == rebon_kernel::ConfigFileKind::Settings
            && event
                .namespace
                .as_deref()
                .is_none_or(|namespace| namespace == self.plugin_id)
    }
}

/// Fail-closed credential resolution for plugin provider adapters.
pub struct CredentialsService {
    config_dir: PathBuf,
    ctx: Context,
}

impl CredentialsService {
    pub fn new(config_dir: PathBuf, ctx: Context) -> Arc<Self> {
        Arc::new(Self { config_dir, ctx })
    }

    /// Run the authorize waterfall. Listeners see the request payload
    /// (`{provider}` for config lookups, `{ref, env: true}` for environment
    /// references) and answer `{"allow": true}` to grant or anything else to
    /// keep the chain going; the terminal (nobody granted) denies.
    fn authorize(&self, payload: serde_json::Value, what: &str) -> Result<(), KernelError> {
        let verdict = self.ctx.waterfall_json(
            CREDENTIALS_AUTHORIZE_EVENT,
            payload,
            |_| serde_json::json!({ "pass": true }),
        );
        if verdict.get("allow").and_then(|a| a.as_bool()) == Some(true) {
            return Ok(());
        }
        Err(KernelError::Other(format!(
            "credential access for {what} was not granted; a host authorizer \
             on `{CREDENTIALS_AUTHORIZE_EVENT}` must allow it"
        )))
    }

    fn provider_entry(&self, provider: &str) -> Result<serde_json::Value, KernelError> {
        let config = read_config(&self.config_dir)?;
        let entries = config.get("customProviders");
        let found = match entries {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .find(|entry| entry.get("name").and_then(|n| n.as_str()) == Some(provider))
                .cloned(),
            Some(serde_json::Value::Object(map)) => map.get(provider).cloned(),
            _ => None,
        };
        found.ok_or_else(|| {
            KernelError::Other(format!(
                "no provider named `{provider}` in config customProviders"
            ))
        })
    }

    fn get(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let provider = params
            .get("provider")
            .and_then(|p| p.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                KernelError::Other("credentials get requires a non-empty `provider`".into())
            })?;
        self.authorize(
            serde_json::json!({ "provider": provider }),
            &format!("provider `{provider}`"),
        )?;
        let entry = self.provider_entry(provider)?;
        let raw = entry
            .get("apiKey")
            .and_then(|k| k.as_str())
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                KernelError::Other(format!("provider `{provider}` has no apiKey configured"))
            })?;
        // `$VAR` indirection: the config convention for env-held keys.
        let api_key = if let Some(var) = raw.strip_prefix('$') {
            std::env::var(var).map_err(|_| {
                KernelError::Other(format!(
                    "provider `{provider}` points its apiKey at ${var}, but that environment \
                     variable is not set"
                ))
            })?
        } else {
            raw.to_string()
        };
        Ok(serde_json::json!({ "provider": provider, "apiKey": api_key }))
    }

    /// Resolve a credential *reference* (an environment-variable name, the
    /// convention dsh configuration carries instead of literal keys). Same
    /// fail-closed shape as `get`: the authorize waterfall runs first, with
    /// `env: true` on the payload so a host authorizer can tell the two
    /// request kinds apart. An unset or blank variable is "absent", not a
    /// value — the dsh seam rule that a blank never masquerades as a
    /// configured secret.
    fn resolve_env(&self, params: &serde_json::Value) -> Result<serde_json::Value, KernelError> {
        let ref_name = params
            .get("ref")
            .and_then(|r| r.as_str())
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| {
                KernelError::Other("credentials resolveEnv requires a non-empty `ref`".into())
            })?;
        // POSIX shell identifier — the same shape dsh's credentialRef brands.
        let valid = ref_name.chars().enumerate().all(|(i, c)| {
            c == '_'
                || if i == 0 {
                    c.is_ascii_alphabetic()
                } else {
                    c.is_ascii_alphanumeric()
                }
        });
        if !valid {
            return Err(KernelError::Other(format!(
                "credential ref `{ref_name}` must be an environment-variable name"
            )));
        }
        self.authorize(
            serde_json::json!({ "ref": ref_name, "env": true }),
            &format!("environment reference `{ref_name}`"),
        )?;
        match std::env::var(ref_name) {
            Ok(value) if !value.trim().is_empty() => {
                Ok(serde_json::json!({ "ref": ref_name, "value": value }))
            }
            _ => Err(KernelError::Other(format!(
                "environment variable `{ref_name}` is not set"
            ))),
        }
    }
}

impl JsonService for CredentialsService {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        match method {
            "get" => self.get(&params),
            "resolveEnv" => self.resolve_env(&params),
            other => Err(KernelError::Other(format!(
                "credentials has no method `{other}`"
            ))),
        }
    }
}

/// Kernel plugin registering both seats.
pub struct ConfigSeatsPlugin {
    config_dir: PathBuf,
    cwd: Option<PathBuf>,
}

impl ConfigSeatsPlugin {
    pub fn new(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            cwd: None,
        }
    }

    /// Pin the project directory the settings chain resolves against, instead
    /// of taking the process's. Tests want this; the host does not.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }
}

impl Plugin for ConfigSeatsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("config-seats").provides(&[SETTINGS_SERVICE, CREDENTIALS_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let settings = match &self.cwd {
            Some(cwd) => SettingsService::new_in(self.config_dir.clone(), cwd.clone()),
            None => SettingsService::new(self.config_dir.clone()),
        };
        ctx.provide_json(SETTINGS_SERVICE, settings)?;
        ctx.provide_json(
            CREDENTIALS_SERVICE,
            CredentialsService::new(self.config_dir.clone(), ctx.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    fn write_config(dir: &Path, json: &str) {
        std::fs::write(dir.join("config.json"), json).expect("config written");
    }

    fn kernel_with_seats(config_dir: &Path) -> std::sync::Arc<Kernel> {
        let kernel = Kernel::new();
        kernel
            .load(vec![Box::new(
                ConfigSeatsPlugin::new(config_dir.to_path_buf()).with_cwd(config_dir.to_path_buf()),
            )])
            .expect("config-seats plugin loads");
        kernel
    }

    const CONFIG: &str = r#"{
        "customProviders": [
            {
                "name": "dsh-ds",
                "format": "openai",
                "baseUrl": "https://api.deepseek.com",
                "apiKey": "sk-secret-literal",
                "model": "deepseek-v4",
                "options": { "headers": { "Authorization": "Bearer x" } }
            },
            { "name": "env-ds", "baseUrl": "https://env.example", "apiKey": "$REBON_TEST_R4_KEY" }
        ],
        "model": "fallback-model",
        "activeCustomProvider": "dsh-ds"
    }"#;

    #[test]
    fn settings_returns_redacted_sections_only() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_seats(tmp.path());
        let ctx = kernel.context();

        let providers = ctx
            .call_json(
                SETTINGS_SERVICE,
                "get",
                serde_json::json!({ "section": "customProviders" }),
            )
            .expect("whitelisted section resolves");
        let first = &providers[0];
        assert_eq!(first["baseUrl"], "https://api.deepseek.com");
        assert_eq!(first["model"], "deepseek-v4");
        // Secrets are stripped recursively — top-level apiKey and the nested
        // Authorization header both vanish.
        assert!(first.get("apiKey").is_none(), "{first}");
        assert!(first["options"]["headers"].get("Authorization").is_none());

        // Non-whitelisted sections are refused, not empty.
        let err = ctx
            .call_json(
                SETTINGS_SERVICE,
                "get",
                serde_json::json!({ "section": "activeCustomProvider" }),
            )
            .expect_err("section outside the whitelist must be refused");
        assert!(err.to_string().contains("not readable"), "{err}");
    }

    #[test]
    fn credentials_are_fail_closed_without_an_authorizer() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_seats(tmp.path());

        let err = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "get",
                serde_json::json!({ "provider": "dsh-ds" }),
            )
            .expect_err("no authorizer: deny");
        assert!(err.to_string().contains("not granted"), "{err}");
    }

    #[test]
    fn authorized_credentials_resolve_literal_and_env_keys() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_seats(tmp.path());
        let authorizer = kernel.context().fork("authorizer");
        authorizer.wrap_json(CREDENTIALS_AUTHORIZE_EVENT, |payload, next| {
            let provider = payload.get("provider").and_then(|p| p.as_str());
            if provider == Some("env-ds") || provider == Some("dsh-ds") {
                serde_json::json!({ "allow": true })
            } else {
                next.call(payload)
            }
        });

        let literal = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "get",
                serde_json::json!({ "provider": "dsh-ds" }),
            )
            .expect("authorized literal key resolves");
        assert_eq!(literal["apiKey"], "sk-secret-literal");

        std::env::set_var("REBON_TEST_R4_KEY", "sk-from-env");
        let env_key = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "get",
                serde_json::json!({ "provider": "env-ds" }),
            )
            .expect("authorized env key resolves");
        assert_eq!(env_key["apiKey"], "sk-from-env");
        std::env::remove_var("REBON_TEST_R4_KEY");

        // Authorized but unknown provider still errors cleanly.
        let err = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "get",
                serde_json::json!({ "provider": "ghost" }),
            )
            .expect_err("unknown provider errors after authorization");
        assert!(err.to_string().to_lowercase().contains("granted"), "{err}");
    }

    #[test]
    fn resolve_env_is_fail_closed_and_reads_the_environment_once_granted() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_seats(tmp.path());
        let params = serde_json::json!({ "ref": "REBON_TEST_R4_ENV_REF" });

        // No authorizer: denied before the environment is even consulted.
        std::env::set_var("REBON_TEST_R4_ENV_REF", "sk-embedded");
        let err = kernel
            .context()
            .call_json(CREDENTIALS_SERVICE, "resolveEnv", params.clone())
            .expect_err("no authorizer: deny");
        assert!(err.to_string().contains("not granted"), "{err}");

        // Authorizer keyed on the env-shaped payload grants this ref only.
        let authorizer = kernel.context().fork("authorizer");
        authorizer.wrap_json(CREDENTIALS_AUTHORIZE_EVENT, |payload, next| {
            if payload.get("env").and_then(|e| e.as_bool()) == Some(true)
                && payload.get("ref").and_then(|r| r.as_str()) == Some("REBON_TEST_R4_ENV_REF")
            {
                serde_json::json!({ "allow": true })
            } else {
                next.call(payload)
            }
        });

        let hit = kernel
            .context()
            .call_json(CREDENTIALS_SERVICE, "resolveEnv", params.clone())
            .expect("granted ref resolves from the environment");
        assert_eq!(hit["value"], "sk-embedded");

        // Granted but unset is "absent", an error — never an empty value.
        std::env::remove_var("REBON_TEST_R4_ENV_REF");
        let err = kernel
            .context()
            .call_json(CREDENTIALS_SERVICE, "resolveEnv", params)
            .expect_err("unset variable is absent");
        assert!(err.to_string().contains("not set"), "{err}");

        // Malformed refs are rejected before the waterfall ever runs.
        let err = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "resolveEnv",
                serde_json::json!({ "ref": "not a var!" }),
            )
            .expect_err("non-identifier ref refused");
        assert!(
            err.to_string().contains("environment-variable name"),
            "{err}"
        );
    }

    // ---- the namespace half of the seat ----

    /// A plugin that declares keys, writes them, reads them back, and is
    /// refused everything outside its own namespace.
    struct DemoPlugin {
        keys: Vec<rebon_kernel::SettingKey>,
    }

    impl Plugin for DemoPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("demo").settings(self.keys.clone())
        }

        fn apply(&self, _ctx: &Context) -> Result<(), KernelError> {
            Ok(())
        }
    }

    fn demo_keys() -> Vec<rebon_kernel::SettingKey> {
        use rebon_kernel::{SettingKey, SettingType};
        vec![
            SettingKey::new("greeting", SettingType::String)
                .with_default(serde_json::json!("Hello from the plugin plane")),
            SettingKey::new("retries", SettingType::Number),
        ]
    }

    /// A kernel with the seat and one plugin that declares two keys.
    fn kernel_with_demo(config_dir: &Path) -> std::sync::Arc<Kernel> {
        let kernel = kernel_with_seats(config_dir);
        kernel
            .load(vec![Box::new(DemoPlugin { keys: demo_keys() })])
            .expect("demo plugin loads");
        kernel
    }

    fn as_demo(params: serde_json::Value) -> serde_json::Value {
        let mut params = params;
        params[CALLER_PLUGIN_ID] = serde_json::json!("demo");
        params
    }

    #[test]
    fn a_plugin_declares_writes_and_reads_back_its_own_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_demo(tmp.path());
        let ctx = kernel.context();

        // Loading declared the namespace, so it is listed and the defaults
        // read back before anything has been written.
        let listed = ctx
            .call_json(SETTINGS_SERVICE, "list", serde_json::json!({}))
            .expect("list");
        assert_eq!(listed["namespaces"], serde_json::json!(["demo"]));
        let before = ctx
            .call_json(SETTINGS_SERVICE, "read", as_demo(serde_json::json!({})))
            .expect("read");
        assert_eq!(before["greeting"], "Hello from the plugin plane");
        assert!(before.get("retries").is_none(), "no default, no key");

        ctx.call_json(
            SETTINGS_SERVICE,
            "write",
            as_demo(serde_json::json!({ "patch": { "greeting": "hi", "retries": 2 } })),
        )
        .expect("write");

        let after = ctx
            .call_json(SETTINGS_SERVICE, "read", as_demo(serde_json::json!({})))
            .expect("read");
        assert_eq!(after["greeting"], "hi");
        assert_eq!(after["retries"], 2);
        // The file the switches are read from is the file that was written.
        assert_eq!(
            rebon_config::plugin_settings_in(tmp.path(), tmp.path(), "demo")
                .get("greeting")
                .cloned(),
            Some(serde_json::json!("hi"))
        );
    }

    #[test]
    fn every_way_out_of_the_namespace_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_demo(tmp.path());
        let ctx = kernel.context();

        let refusals = [
            (
                "another plugin's namespace",
                as_demo(serde_json::json!({
                    "namespace": "other",
                    "patch": { "greeting": "hi" }
                })),
                "may only read and write",
            ),
            (
                "the kernel's switch",
                as_demo(serde_json::json!({ "patch": { "enabled": true } })),
                "not a settings key",
            ),
            (
                "a key never declared",
                as_demo(serde_json::json!({ "patch": { "typo": 1 } })),
                "not a settings key",
            ),
            (
                "a declared key given the wrong type",
                as_demo(serde_json::json!({ "patch": { "retries": "two" } })),
                "is declared number",
            ),
            (
                "nobody at all",
                serde_json::json!({ "patch": { "greeting": "hi" } }),
                CALLER_PLUGIN_ID,
            ),
        ];
        for (what, params, expected) in refusals {
            let err = ctx
                .call_json(SETTINGS_SERVICE, "write", params)
                .expect_err(what);
            assert!(
                err.to_string().contains(expected),
                "{what}: expected {expected:?}, got {err}"
            );
        }
        // Nothing was written by any of them.
        assert!(
            rebon_config::plugin_settings_in(tmp.path(), tmp.path(), "demo").is_empty(),
            "a refused write must not land"
        );
        assert!(rebon_config::plugin_settings_in(tmp.path(), tmp.path(), "other").is_empty());
        assert!(
            rebon_config::saved_plugin_switches_in(tmp.path(), tmp.path()).is_empty(),
            "the switch is untouched"
        );

        // Reading someone else's namespace is refused on the same rule.
        let err = ctx
            .call_json(
                SETTINGS_SERVICE,
                "read",
                as_demo(serde_json::json!({ "namespace": "other" })),
            )
            .expect_err("cross-namespace read");
        assert!(err.to_string().contains("may only read and write"), "{err}");
    }

    /// The whitelist is the other half of the seat and does not move: a
    /// plugin may still read the sections that are somebody else's, still
    /// without secrets, and still nothing beyond them.
    #[test]
    fn the_read_only_sections_still_read_and_still_strip_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_demo(tmp.path());
        let ctx = kernel.context();

        let providers = ctx
            .call_json(
                SETTINGS_SERVICE,
                "get",
                serde_json::json!({ "section": "customProviders" }),
            )
            .expect("whitelisted section still resolves");
        assert_eq!(providers[0]["baseUrl"], "https://api.deepseek.com");
        assert!(providers[0].get("apiKey").is_none());
        let err = ctx
            .call_json(
                SETTINGS_SERVICE,
                "get",
                serde_json::json!({ "section": "plugins" }),
            )
            .expect_err("the namespace is not reachable through `get`");
        assert!(err.to_string().contains("not readable"), "{err}");
    }

    /// A write says which namespace moved, so a plugin can tell a change of
    /// its own from a change of somebody else's without reading a file.
    #[test]
    fn a_namespace_write_announces_itself_and_a_switch_write_does_not() {
        use rebon_kernel::{ConfigChanged, ConfigFileKind};
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_demo(tmp.path());
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        kernel.context().on::<ConfigChanged>(move |event| {
            if event.kind == ConfigFileKind::Settings {
                sink.lock().unwrap().push(event.namespace.clone());
            }
        });

        // The process observer is installed once per process and some other
        // test may already own it, so the event is emitted here the way the
        // bootstrap emits it rather than by writing through `rebon-config`.
        kernel.context().emit(&ConfigChanged {
            kind: ConfigFileKind::Settings,
            path: tmp.path().join("settings.json"),
            namespace: Some("demo".to_string()),
        });
        kernel.context().emit(&ConfigChanged {
            kind: ConfigFileKind::Settings,
            path: tmp.path().join("settings.json"),
            namespace: None,
        });

        let settings = PluginSettings::new(kernel.context(), "demo");
        let other = PluginSettings::new(kernel.context(), "other");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen, vec![Some("demo".to_string()), None]);
        let events: Vec<ConfigChanged> = seen
            .into_iter()
            .map(|namespace| ConfigChanged {
                kind: ConfigFileKind::Settings,
                path: tmp.path().join("settings.json"),
                namespace,
            })
            .collect();
        assert!(settings.is_mine(&events[0]), "demo's own write");
        assert!(!other.is_mine(&events[0]), "not somebody else's business");
        assert!(
            settings.is_mine(&events[1]) && other.is_mine(&events[1]),
            "an unnamed settings change is everyone's business"
        );
    }

    /// Unloading takes the declaration with it, so the next thing loaded
    /// under that name starts from its own declaration rather than
    /// inheriting one.
    #[test]
    fn unloading_a_plugin_withdraws_its_settings_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_demo(tmp.path());
        let ctx = kernel.context();

        assert!(kernel.unload("demo"), "demo unloads");
        let listed = ctx
            .call_json(SETTINGS_SERVICE, "list", serde_json::json!({}))
            .expect("list");
        assert_eq!(listed["namespaces"], serde_json::json!([]));
        let err = ctx
            .call_json(
                SETTINGS_SERVICE,
                "write",
                as_demo(serde_json::json!({ "patch": { "greeting": "hi" } })),
            )
            .expect_err("an unloaded plugin declares nothing");
        assert!(
            err.to_string().contains("declared no settings keys"),
            "{err}"
        );
    }

    #[test]
    fn a_declared_key_reads_its_default_and_a_null_write_puts_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), CONFIG);
        let kernel = kernel_with_demo(tmp.path());
        let settings = PluginSettings::new(kernel.context(), "demo");

        settings
            .write(serde_json::json!({ "greeting": "custom" }))
            .expect("write");
        assert_eq!(settings.read().expect("read")["greeting"], "custom");

        settings
            .write(serde_json::json!({ "greeting": serde_json::Value::Null }))
            .expect("removal");
        assert_eq!(
            settings.read().expect("read")["greeting"],
            "Hello from the plugin plane",
            "the declared default comes back"
        );
    }
}
