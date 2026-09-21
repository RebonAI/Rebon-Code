//! The `routerModel` row this plugin puts in the settings panel.
//!
//! The routing model is the one thing about this feature a user has to choose,
//! and until now the only way to choose it was to edit `settings.json` by
//! hand. The row is registered by the plugin rather than built into the
//! panel's list, so it is there exactly when the feature is.
//!
//! What it offers is the models the router would actually accept. `route()`
//! refuses a `routerModel` that does not belong to the provider in force, so a
//! row offering anything else would let the panel write a value the next turn
//! rejects.

use std::collections::BTreeSet;
use std::sync::Arc;

use rebon_config_seat::{
    ConfigOptionProvider, ConfigOptionSpec, ConfigOptionValue, ConfigSeatService,
};
use rebon_kernel::{Context, KernelError};
use rebon_kernel_seats::kernel_config_seats::PluginSettings;

use crate::PLUGIN_ID;

/// The id the panel and every surface's dispatch name this row by.
pub const ROUTER_MODEL_OPTION: &str = "model_routing_router_model";

/// What the row shows when `routerModel` is unset. Routing refuses to run in
/// that state, which the description says rather than the row pretending to a
/// default it does not have.
const UNSET: &str = "unset";

/// Register the row, when a seat is there to take it.
///
/// A missing seat is not an error: the seat is a Core plugin, and a
/// composition that leaves it out still gets routing, just without a row to
/// configure it from.
pub(crate) fn register(ctx: &Context) -> Result<(), KernelError> {
    let Some(seat) = ctx.get::<ConfigSeatService>() else {
        return Ok(());
    };
    seat.register(
        ctx,
        ConfigOptionSpec::select(ROUTER_MODEL_OPTION, "Router model")
            .describe(
                "The cheap model that picks this session's model and reasoning effort on \
                 the first real prompt. Routing does nothing until one is chosen.",
            )
            .in_category("model"),
        Arc::new(RouterModelOption {
            settings: PluginSettings::new(ctx, PLUGIN_ID),
        }),
    )
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
            .and_then(|settings| settings.get("routerModel"))
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
            serde_json::json!({ "routerModel": serde_json::Value::Null })
        } else {
            // Refuse here rather than let the next turn fail: `route()` checks
            // the same set, and a value written past this point would sit in
            // the file looking configured while every turn rejected it.
            if !Self::allowed_models().iter().any(|model| model == value) {
                return Err(format!("`{value}` is not a model of the provider in force"));
            }
            serde_json::json!({ "routerModel": value })
        };
        self.settings
            .write(patch)
            .map(|_| ())
            .map_err(|error| format!("could not save the router model: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

        scope.dispose();
        assert!(
            !seat.has(ROUTER_MODEL_OPTION),
            "an experimental plugin's row must not outlive the plugin"
        );
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
