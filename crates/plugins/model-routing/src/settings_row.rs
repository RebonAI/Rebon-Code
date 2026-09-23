//! The rows this plugin puts in the settings panel.
//!
//! Routing offers a backend, a model for each backend and a policy. The rows
//! are registered by the plugin rather than built into the panel's list, so
//! they are there exactly when the feature is.
//!
//! What the model row offers is the models the router would actually accept.
//! `route()` refuses a `routerModel` that does not belong to the provider in
//! force, so a row offering anything else would let the panel write a value the
//! next turn rejects. The backend row offers the two classifiers `route()`
//! knows and nothing else. The policy row is free text: it is the user's own
//! instruction to a model, and both backends hand it to whatever they ask.

use std::collections::BTreeSet;
use std::sync::Arc;

use rebon_config_seat::{
    ConfigOptionProvider, ConfigOptionSpec, ConfigOptionValue, ConfigSeatService,
};
use rebon_kernel::{Context, KernelError};
use rebon_kernel_seats::kernel_config_seats::PluginSettings;

use crate::{
    BACKEND_PROMPT, BACKEND_SETTING, BACKEND_TYPESAFE, CLASSIFIER_ENDPOINT_SETTING,
    CLASSIFIER_MODEL_SETTING, PLUGIN_ID, POLICY_SETTING, ROUTER_MODEL_SETTING,
};

/// The id the panel and every surface's dispatch name the backend row by.
pub const BACKEND_OPTION: &str = "model_routing_backend";
pub const CLASSIFIER_MODEL_OPTION: &str = "model_routing_classifier_model";
pub const CLASSIFIER_ENDPOINT_OPTION: &str = "model_routing_classifier_endpoint";

/// The id the panel and every surface's dispatch name this row by.
pub const ROUTER_MODEL_OPTION: &str = "model_routing_router_model";

/// The policy row's id, alongside the model row's.
pub const ROUTING_POLICY_OPTION: &str = "model_routing_policy";

/// What the row shows when `routerModel` is unset. Routing refuses to run in
/// that state, which the description says rather than the row pretending to a
/// default it does not have.
const UNSET: &str = "unset";

/// Register the rows, when a seat is there to take them.
///
/// A missing seat is not an error: the seat is a Core plugin, and a
/// composition that leaves it out still gets routing, just without rows to
/// configure it from.
pub(crate) fn register(ctx: &Context) -> Result<(), KernelError> {
    let Some(seat) = ctx.get::<ConfigSeatService>() else {
        return Ok(());
    };
    seat.register(
        ctx,
        ConfigOptionSpec::select(BACKEND_OPTION, "Router backend")
            .describe(
                "Which classifier picks this session's provider, model and reasoning effort on \
                 the first real prompt. `Prompt` asks a model of the provider in force, so it \
                 needs a router model. `TypeSafe System One` sends the first prompt to a \
                 TypeSafe-compatible endpoint and reads typed answers back. The key comes from \
                 TYPESAFE_API_KEY for TypeSafe or AI_GATEWAY_API_KEY for Vercel, independently \
                 of the provider in force.",
            )
            .in_category("model"),
        Arc::new(BackendOption {
            settings: PluginSettings::new(ctx, PLUGIN_ID),
        }),
    )?;
    seat.register(
        ctx,
        ConfigOptionSpec::text(CLASSIFIER_MODEL_OPTION, "TypeSafe classifier model")
            .describe(
                "The System One model ID used by this router when the backend is TypeSafe. \
                 Leave empty for jev-latest; use typesafe-ai/jev when routing through Vercel's \
                 TypeSafe-compatible API. This does not change the permission classifier model.",
            )
            .in_category("model"),
        Arc::new(ClassifierModelOption {
            settings: PluginSettings::new(ctx, PLUGIN_ID),
        }),
    )?;
    seat.register(
        ctx,
        ConfigOptionSpec::text(CLASSIFIER_ENDPOINT_OPTION, "TypeSafe classifier endpoint")
            .describe(format!(
                "Full HTTPS System One URL. Leave empty for {}; use {} for Vercel AI Gateway. \
                 The first prompt is sent to this endpoint with its matching API key.",
                rebon_api::typesafe::DEFAULT_ENDPOINT,
                rebon_api::typesafe::VERCEL_ENDPOINT,
            ))
            .in_category("model"),
        Arc::new(ClassifierEndpointOption {
            settings: PluginSettings::new(ctx, PLUGIN_ID),
        }),
    )?;
    seat.register(
        ctx,
        ConfigOptionSpec::select(ROUTER_MODEL_OPTION, "Router model")
            .describe(
                "The cheap model that picks this session's provider, model and reasoning \
                 effort on the first real prompt, for the `Prompt` backend. Routing does \
                 nothing until one is chosen. It can move the task onto another configured \
                 provider, so the picker itself has to be a model of the provider in force.",
            )
            .in_category("model"),
        Arc::new(RouterModelOption {
            settings: PluginSettings::new(ctx, PLUGIN_ID),
        }),
    )?;
    seat.register(
        ctx,
        ConfigOptionSpec::text(ROUTING_POLICY_OPTION, "Routing policy")
            .describe(
                "What routing should go by — which tasks deserve a stronger or more expensive \
                 model, and which provider and model to use for them. Written for a model to \
                 read. Left empty, routing falls back to picking the cheapest fit.",
            )
            .in_category("model"),
        Arc::new(RoutingPolicyOption {
            settings: PluginSettings::new(ctx, PLUGIN_ID),
        }),
    )
}

/// One key's write or removal, as `settings.write` takes it.
///
/// Built rather than spelled out with `json!`, which would read a key constant as
/// the literal name of the constant instead of its value.
fn settings_patch(key: &str, value: Option<&str>) -> serde_json::Value {
    let mut patch = serde_json::Map::new();
    patch.insert(
        key.to_string(),
        value.map_or(serde_json::Value::Null, |value| {
            serde_json::Value::String(value.to_string())
        }),
    );
    serde_json::Value::Object(patch)
}

/// Which classifier runs.
///
/// Both values are real choices, so the row stores the one that is picked
/// rather than treating one of them as "unset" — absent still reads as the
/// text backend, which is what a settings file written before this row existed
/// meant.
struct BackendOption {
    settings: PluginSettings,
}

impl ConfigOptionProvider for BackendOption {
    fn current(&self, _session: Option<&str>) -> String {
        let Some(settings) = self.settings.read().ok() else {
            return BACKEND_PROMPT.to_string();
        };
        match crate::backend(&settings) {
            Ok(backend) => backend.as_str().to_string(),
            // 文件里是一个路由会拒绝的值：原样显示它，而不是把面板显示成一个
            // 其实没在生效的后端。
            Err(_) => settings
                .get(BACKEND_SETTING)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        vec![
            ConfigOptionValue {
                value: BACKEND_PROMPT.to_string(),
                name: "Prompt".to_string(),
                description: Some(
                    "A model of the provider in force answers with one JSON object. Needs the \
                     Router model row set."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: BACKEND_TYPESAFE.to_string(),
                name: "TypeSafe System One".to_string(),
                description: Some(
                    "A TypeSafe-compatible endpoint classifies the first prompt as a typed choice. \
                     Needs TYPESAFE_API_KEY (or AI_GATEWAY_API_KEY for Vercel) in the environment, \
                     and that prompt leaves this machine."
                        .to_string(),
                ),
            },
        ]
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value != BACKEND_PROMPT && value != BACKEND_TYPESAFE {
            return Err(format!(
                "`{value}` is not `{BACKEND_PROMPT}` or `{BACKEND_TYPESAFE}`"
            ));
        }
        self.settings
            .write(settings_patch(BACKEND_SETTING, Some(value)))
            .map(|_| ())
            .map_err(|error| format!("could not save the router backend: {error}"))
    }
}

struct ClassifierModelOption {
    settings: PluginSettings,
}

impl ConfigOptionProvider for ClassifierModelOption {
    fn current(&self, _session: Option<&str>) -> String {
        self.settings
            .read()
            .ok()
            .as_ref()
            .and_then(|settings| settings.get(CLASSIFIER_MODEL_SETTING))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or(rebon_api::typesafe::DEFAULT_MODEL)
            .to_string()
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        Vec::new()
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let value = value.trim();
        let configured =
            (value != rebon_api::typesafe::DEFAULT_MODEL && !value.is_empty()).then_some(value);
        self.settings
            .write(settings_patch(CLASSIFIER_MODEL_SETTING, configured))
            .map(|_| ())
            .map_err(|error| format!("could not save the TypeSafe classifier model: {error}"))
    }
}

struct ClassifierEndpointOption {
    settings: PluginSettings,
}

impl ConfigOptionProvider for ClassifierEndpointOption {
    fn current(&self, _session: Option<&str>) -> String {
        self.settings
            .read()
            .ok()
            .as_ref()
            .and_then(|settings| settings.get(CLASSIFIER_ENDPOINT_SETTING))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(rebon_api::typesafe::DEFAULT_ENDPOINT)
            .to_string()
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        Vec::new()
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let value = value.trim();
        let configured =
            (value != rebon_api::typesafe::DEFAULT_ENDPOINT && !value.is_empty()).then_some(value);
        if let Some(value) = configured {
            crate::classifier_endpoint(&settings_patch(CLASSIFIER_ENDPOINT_SETTING, Some(value)))
                .map_err(|error| error.to_string())?;
        }
        self.settings
            .write(settings_patch(CLASSIFIER_ENDPOINT_SETTING, configured))
            .map(|_| ())
            .map_err(|error| format!("could not save the TypeSafe classifier endpoint: {error}"))
    }
}

struct RouterModelOption {
    settings: PluginSettings,
}

impl RouterModelOption {
    /// The models the router would accept right now: the active provider's,
    /// plus whatever its profiles resolve to.
    ///
    /// Empty when no provider is configured, which the panel shows as a row
    /// with nothing to cycle rather than a list of models from a provider
    /// that is not in force.
    fn allowed_models() -> Vec<String> {
        let config_home = rebon_config::config_home_dir();
        let Some(active) = rebon_config::provider_setup_status_in(&config_home)
            .active_provider()
            .map(|provider| provider.name.clone())
        else {
            return Vec::new();
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| config_home.clone());
        let contributions =
            rebon_provider::provider_catalog::discover_plugin_model_providers(&config_home, &cwd);
        let catalog =
            rebon_provider::provider_catalog::provider_catalog(&config_home, &contributions);
        let Some(provider) = catalog.iter().find(|provider| provider.id == active) else {
            return Vec::new();
        };
        // Same set `route()` builds: the provider's models plus the models its
        // profiles name, so a profile spelling is offered as itself.
        let mut allowed: BTreeSet<String> = provider.models.iter().cloned().collect();
        allowed.extend(
            provider
                .model_profiles
                .iter()
                .map(|(_, model)| model.to_owned()),
        );
        allowed.into_iter().collect()
    }
}

impl ConfigOptionProvider for RouterModelOption {
    fn current(&self, _session: Option<&str>) -> String {
        self.settings
            .read()
            .ok()
            .as_ref()
            .and_then(|settings| settings.get(ROUTER_MODEL_SETTING))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| UNSET.to_string())
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        let models = Self::allowed_models();
        if models.is_empty() {
            return Vec::new();
        }
        let mut choices = vec![ConfigOptionValue {
            value: UNSET.to_string(),
            name: "Not set".to_string(),
            description: Some("Routing stays off until a model is chosen.".to_string()),
        }];
        choices.extend(models.into_iter().map(|model| ConfigOptionValue {
            name: model.clone(),
            value: model,
            description: None,
        }));
        choices
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let value = value.trim();
        let patch = if value.is_empty() || value == UNSET {
            settings_patch(ROUTER_MODEL_SETTING, None)
        } else {
            // Refuse here rather than let the next turn fail: `route()` checks
            // the same set, and a value written past this point would sit in
            // the file looking configured while every turn rejected it.
            if !Self::allowed_models().iter().any(|model| model == value) {
                return Err(format!("`{value}` is not a model of the provider in force"));
            }
            settings_patch(ROUTER_MODEL_SETTING, Some(value))
        };
        self.settings
            .write(patch)
            .map(|_| ())
            .map_err(|error| format!("could not save the router model: {error}"))
    }
}

struct RoutingPolicyOption {
    settings: PluginSettings,
}

impl ConfigOptionProvider for RoutingPolicyOption {
    /// Empty rather than a placeholder word: this row is typed into, and a value
    /// the user never wrote must not look like one they did.
    fn current(&self, _session: Option<&str>) -> String {
        self.settings
            .read()
            .ok()
            .as_ref()
            .and_then(|settings| settings.get(POLICY_SETTING))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// Nothing to cycle: the policy is written, not picked.
    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        Vec::new()
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let value = value.trim();
        // Empty removes the key, so "no policy" is one state in the file rather
        // than two that read the same and differ by whitespace.
        let patch = settings_patch(POLICY_SETTING, (!value.is_empty()).then_some(value));
        self.settings
            .write(patch)
            .map(|_| ())
            .map_err(|error| format!("could not save the routing policy: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelRoutingPlugin;
    use rebon_config_seat::ConfigSeat;
    use rebon_kernel::Kernel;

    /// A row nobody can take is not an error: routing still works, it just
    /// has no panel row to be configured from.
    #[test]
    fn registering_without_a_seat_is_not_a_failure() {
        let kernel = Kernel::new();
        register(kernel.context()).expect("a missing seat is tolerated");
    }

    #[test]
    fn the_row_is_registered_when_a_seat_is_there_and_leaves_with_the_plugin() {
        let kernel = Kernel::new();
        let seat = ConfigSeat::new();
        kernel
            .context()
            .provide::<ConfigSeatService>(seat.clone())
            .expect("provides the seat");

        let scope = kernel.context().fork(PLUGIN_ID);
        register(&scope).expect("registers");
        assert!(seat.has(BACKEND_OPTION));
        assert!(seat.has(CLASSIFIER_MODEL_OPTION));
        assert!(seat.has(CLASSIFIER_ENDPOINT_OPTION));
        assert!(seat.has(ROUTER_MODEL_OPTION));
        assert!(seat.has(ROUTING_POLICY_OPTION));

        scope.dispose();
        assert!(
            !seat.has(BACKEND_OPTION)
                && !seat.has(CLASSIFIER_MODEL_OPTION)
                && !seat.has(CLASSIFIER_ENDPOINT_OPTION)
                && !seat.has(ROUTER_MODEL_OPTION)
                && !seat.has(ROUTING_POLICY_OPTION),
            "an experimental plugin's rows must not outlive the plugin"
        );
    }

    /// 一个只属于自己的配置目录的 settings 席位；插件本身也要装上，否则它不
    /// 认识这些行声明的键。临时目录由调用方持有。
    fn option_settings(root: &std::path::Path) -> PluginSettings {
        let kernel = Kernel::new();
        kernel
            .load(vec![
                Box::new(
                    rebon_kernel_seats::kernel_config_seats::ConfigSeatsPlugin::new(
                        root.to_path_buf(),
                    )
                    .with_cwd(root.to_path_buf()),
                ),
                Box::new(ModelRoutingPlugin),
            ])
            .unwrap();
        PluginSettings::new(kernel.context(), PLUGIN_ID)
    }

    /// 策略行的写入与读回。
    fn policy_option(root: &std::path::Path) -> RoutingPolicyOption {
        RoutingPolicyOption {
            settings: option_settings(root),
        }
    }

    /// 策略是自由文本：去掉首尾空白后原样存回，空值等于没配。
    #[test]
    fn the_policy_row_round_trips_and_clears_itself() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("model-routing-policy");
        let root = tempfile::tempdir().unwrap();
        let option = policy_option(root.path());
        assert_eq!(option.current(None), "", "nothing configured yet");

        option
            .apply(None, "  hard work goes to the frontier model  ")
            .expect("writes");
        assert_eq!(option.current(None), "hard work goes to the frontier model");

        // 清空后回到没配的状态，而不是留一个空串。
        option.apply(None, "   ").expect("clears");
        assert_eq!(option.current(None), "", "whitespace is not a policy");
        let written: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.path().join("settings.json")).expect("the settings file"),
        )
        .expect("valid JSON");
        assert!(
            written.pointer("/plugins/model-routing/policy").is_none(),
            "{written}"
        );
    }

    /// 文本行没有可循环的值，前端不该给它画一个空选择器。
    #[test]
    fn the_policy_row_offers_nothing_to_cycle() {
        let root = tempfile::tempdir().unwrap();
        assert!(policy_option(root.path()).choices(None).is_empty());
    }

    /// The row refuses what `route()` would refuse, rather than writing a
    /// value that looks configured and fails on every turn.
    #[test]
    fn a_model_outside_the_providers_set_is_refused() {
        let kernel = Kernel::new();
        let option = RouterModelOption {
            settings: PluginSettings::new(kernel.context(), PLUGIN_ID),
        };
        let err = option
            .apply(None, "a-model-no-provider-offers")
            .expect_err("refused");
        assert!(err.contains("a-model-no-provider-offers"), "{err}");
        assert!(err.contains("provider in force"), "{err}");
    }

    #[test]
    fn type_safe_classifier_model_has_an_independent_setting() {
        let _home =
            rebon_tool::tasks::test_support::TestConfigHome::new("routing-classifier-model");
        let root = tempfile::tempdir().unwrap();
        let option = ClassifierModelOption {
            settings: option_settings(root.path()),
        };
        assert_eq!(option.current(None), rebon_api::typesafe::DEFAULT_MODEL);
        assert!(option.choices(None).is_empty());
        option.apply(None, "  another-systemone-id  ").unwrap();
        assert_eq!(option.current(None), "another-systemone-id");
        let settings = option.settings.read().unwrap();
        assert_eq!(settings[CLASSIFIER_MODEL_SETTING], "another-systemone-id");
        assert!(settings.get(ROUTER_MODEL_SETTING).is_none());
        option.apply(None, "").unwrap();
        assert_eq!(option.current(None), rebon_api::typesafe::DEFAULT_MODEL);
    }

    #[test]
    fn type_safe_classifier_endpoint_round_trips_and_refuses_http() {
        let _home =
            rebon_tool::tasks::test_support::TestConfigHome::new("routing-classifier-endpoint");
        let root = tempfile::tempdir().unwrap();
        let option = ClassifierEndpointOption {
            settings: option_settings(root.path()),
        };
        assert_eq!(option.current(None), rebon_api::typesafe::DEFAULT_ENDPOINT);
        assert!(option.choices(None).is_empty());
        let endpoint = "https://ai-gateway.vercel.sh/typesafe/v1/systemone";
        option.apply(None, &format!("  {endpoint}  ")).unwrap();
        assert_eq!(option.current(None), endpoint);
        assert_eq!(
            option.settings.read().unwrap()[CLASSIFIER_ENDPOINT_SETTING],
            endpoint
        );
        assert!(option
            .apply(None, "http://example.com/v1/systemone")
            .is_err());
        assert_eq!(option.current(None), endpoint);
        option.apply(None, "").unwrap();
        assert_eq!(option.current(None), rebon_api::typesafe::DEFAULT_ENDPOINT);
    }

    fn backend_option(root: &std::path::Path) -> BackendOption {
        BackendOption {
            settings: option_settings(root),
        }
    }

    /// 后端行：没写过时就是文字后端（老配置照旧），写入后读回，别的值被拒。
    #[test]
    fn the_backend_row_round_trips_and_refuses_what_it_does_not_offer() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("model-routing-backend");
        let root = tempfile::tempdir().unwrap();
        let option = backend_option(root.path());
        assert_eq!(option.current(None), BACKEND_PROMPT, "nothing written yet");

        option.apply(None, "typesafe").expect("writes");
        assert_eq!(option.current(None), BACKEND_TYPESAFE);

        let err = option.apply(None, "automatic").expect_err("refused");
        assert!(err.contains("automatic"), "{err}");

        option.apply(None, "prompt").expect("writes");
        assert_eq!(option.current(None), BACKEND_PROMPT);
    }

    /// 两个后端都是真选项；选 TypeSafe 那一项要把"prompt 会离开本机"说清楚。
    #[test]
    fn the_backend_row_offers_both_classifiers() {
        let root = tempfile::tempdir().unwrap();
        let choices = backend_option(root.path()).choices(None);
        let values: Vec<&str> = choices.iter().map(|choice| choice.value.as_str()).collect();
        assert_eq!(values, [BACKEND_PROMPT, BACKEND_TYPESAFE]);
        let type_safe = choices[1].description.as_deref().expect("described");
        assert!(type_safe.contains("TYPESAFE_API_KEY"), "{type_safe}");
        assert!(type_safe.contains("leaves this machine"), "{type_safe}");
    }

    /// 文件里放着路由会拒绝的值时，面板显示的是那个值本身：显示成 `prompt`
    /// 会让用户以为有一个后端在生效。
    #[test]
    fn the_backend_row_shows_a_value_the_router_would_refuse_as_itself() {
        let _home =
            rebon_tool::tasks::test_support::TestConfigHome::new("model-routing-backend-raw");
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("settings.json"),
            br#"{"plugins":{"model-routing":{"backend":"automatic"}}}"#,
        )
        .expect("writes the settings file");

        assert_eq!(backend_option(root.path()).current(None), "automatic");
    }
}
