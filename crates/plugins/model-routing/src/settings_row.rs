//! The two rows this plugin puts in the settings panel.
//!
//! Routing has exactly two things to choose: the cheap model that does the
//! classifying, and the policy that classifier follows. Until now the only way to
//! set either was to edit `settings.json` by hand. The rows are registered by the
//! plugin rather than built into the panel's list, so they are there exactly when
//! the feature is.
//!
//! What the model row offers is the models the router would actually accept.
//! `route()` refuses a `routerModel` that does not belong to the provider in
//! force, so a row offering anything else would let the panel write a value the
//! next turn rejects. The policy row is free text: it is the user's own
//! instruction to a model, and the plugin only appends it to the classifier's
//! system prompt.

use std::collections::BTreeSet;
use std::sync::Arc;

use rebon_config_seat::{
    ConfigOptionProvider, ConfigOptionSpec, ConfigOptionValue, ConfigSeatService,
};
use rebon_kernel::{Context, KernelError};
use rebon_kernel_seats::kernel_config_seats::PluginSettings;

use crate::{PLUGIN_ID, POLICY_SETTING, ROUTER_MODEL_SETTING};

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
        ConfigOptionSpec::select(ROUTER_MODEL_OPTION, "Router model")
            .describe(
                "The cheap model that picks this session's provider, model and reasoning \
                 effort on the first real prompt. Routing does nothing until one is chosen. \
                 It can move the task onto another configured provider, so the picker itself \
                 has to be a model of the provider in force.",
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
        assert!(seat.has(ROUTER_MODEL_OPTION));
        assert!(seat.has(ROUTING_POLICY_OPTION));

        scope.dispose();
        assert!(
            !seat.has(ROUTER_MODEL_OPTION) && !seat.has(ROUTING_POLICY_OPTION),
            "an experimental plugin's rows must not outlive the plugin"
        );
    }

    /// 策略行的写入与读回。一个只属于自己的配置目录，临时目录由调用方持有；
    /// 插件本身也要装上，否则 settings 席位不认识它声明的键。
    fn policy_option(root: &std::path::Path) -> RoutingPolicyOption {
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
        RoutingPolicyOption {
            settings: PluginSettings::new(kernel.context(), PLUGIN_ID),
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
}
