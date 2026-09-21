use crate::{Context, KernelError};

/// The `settings` seat, by the name it goes by on the JSON plane.
///
/// The constant lives here rather than beside the seat implementation because
/// the kernel is what declares a plugin's settings namespace on its behalf: it
/// is the only thing that knows a plugin's real id, and the id *is* the
/// namespace.
pub const SETTINGS_SERVICE: &str = "settings";

/// What kind of value a declared settings key holds.
///
/// A closed, shallow set on purpose: the declaration exists so a write can be
/// checked against something a person can read in a manifest, not so the host
/// can validate arbitrary documents. Anything richer than "which of these six"
/// belongs to the plugin that owns the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingType {
    Bool,
    Number,
    String,
    Array,
    Object,
    /// Any JSON value, including `null`. The declaration still matters — it
    /// says the key exists — but the shape is the plugin's business.
    Any,
}

impl SettingType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
            Self::Any => "any",
        }
    }

    /// The JSON Schema spelling, which is what a manifest writes.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "boolean" | "bool" => Some(Self::Bool),
            "number" | "integer" => Some(Self::Number),
            "string" => Some(Self::String),
            "array" => Some(Self::Array),
            "object" => Some(Self::Object),
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    pub fn accepts(self, value: &serde_json::Value) -> bool {
        match self {
            Self::Any => true,
            Self::Bool => value.is_boolean(),
            Self::Number => value.is_number(),
            Self::String => value.is_string(),
            Self::Array => value.is_array(),
            Self::Object => value.is_object(),
        }
    }
}

/// One key a plugin declares under its own `plugins.<id>` namespace.
///
/// Declaring is what makes a key writable: a plugin may write the keys it
/// said it has and nothing else, so a typo lands as a refusal the author sees
/// rather than as a setting nothing reads.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingKey {
    pub name: String,
    pub ty: SettingType,
    /// What the key reads as when no settings file sets it.
    pub default: Option<serde_json::Value>,
}

impl SettingKey {
    pub fn new(name: impl Into<String>, ty: SettingType) -> Self {
        Self {
            name: name.into(),
            ty,
            default: None,
        }
    }

    pub fn with_default(mut self, default: serde_json::Value) -> Self {
        self.default = Some(default);
        self
    }
}

/// Static description of a plugin: its identity and the service names it
/// provides and consumes. The kernel orders plugin application by these
/// declarations (providers before consumers) and fails fast on gaps.
#[derive(Clone, Debug, Default)]
pub struct PluginMeta {
    pub name: String,
    /// Service names this plugin registers providers for.
    pub provides: Vec<String>,
    /// Service names this plugin requires at apply time.
    pub inject: Vec<String>,
    /// Service names this plugin uses when present but can live without.
    /// They influence ordering (soft edge) but never fail resolution.
    pub optional_inject: Vec<String>,
    /// Settings keys this plugin owns under `plugins.<name>`.
    ///
    /// The kernel declares them on the [`SETTINGS_SERVICE`] seat once the
    /// whole load batch has applied — after, not before, so a plugin listed
    /// ahead of the seat's provider in the same batch still gets its
    /// declaration in.
    pub settings: Vec<SettingKey>,
}

impl PluginMeta {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn provides(mut self, services: &[&str]) -> Self {
        self.provides = services.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn inject(mut self, services: &[&str]) -> Self {
        self.inject = services.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn optional_inject(mut self, services: &[&str]) -> Self {
        self.optional_inject = services.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Declare the keys this plugin owns under `plugins.<name>`.
    pub fn settings(mut self, keys: Vec<SettingKey>) -> Self {
        self.settings = keys;
        self
    }

    /// The declaration as the seat's `declare` method takes it.
    pub(crate) fn settings_declaration(&self) -> serde_json::Value {
        serde_json::json!({
            "callerPluginId": self.name,
            "namespace": self.name,
            "keys": self
                .settings
                .iter()
                .map(|key| {
                    let mut entry = serde_json::json!({
                        "name": key.name,
                        "type": key.ty.as_str(),
                    });
                    if let Some(default) = &key.default {
                        entry["default"] = default.clone();
                    }
                    entry
                })
                .collect::<Vec<_>>(),
        })
    }
}

/// A unit of capability. `apply` runs once at load time on a context forked
/// for this plugin; every registration made through that context is undone
/// when the plugin is unloaded.
pub trait Plugin: Send + Sync {
    fn meta(&self) -> PluginMeta;
    fn apply(&self, ctx: &Context) -> Result<(), KernelError>;
}
