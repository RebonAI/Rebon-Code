//! The `sub_agents` row this plugin puts in the settings panel.
//!
//! Note the two switches. `plugins.agents.enabled` decides whether this
//! plugin loads at all; `sub_agents` is the user-facing toggle persisted in
//! the config file, which the `Agent` tool's `is_enabled()` honours. The row
//! is the second one — and it belongs to this plugin because the first switch
//! taking the plugin out should take the row with it. A panel offering to turn
//! sub-agents "on" while the plugin that implements them is unloaded is
//! offering nothing.

use std::sync::Arc;

use rebon_config_seat::{
    ConfigOptionProvider, ConfigOptionSpec, ConfigOptionValue, ConfigSeatService,
};
use rebon_kernel::{Context, KernelError};

/// The id the panel and every surface's dispatch name this row by.
pub const SUB_AGENTS_OPTION: &str = "sub_agents";

/// Register the row, when a seat is there to take it.
pub(crate) fn register(ctx: &Context) -> Result<(), KernelError> {
    let Some(seat) = ctx.get::<ConfigSeatService>() else {
        return Ok(());
    };
    seat.register(
        ctx,
        ConfigOptionSpec::select(SUB_AGENTS_OPTION, "Sub-agents")
            .describe(
                "Offer the Agent tool so the model can delegate work to sub-agents. \
                 Turning it off also drops the delegation section from the system prompt.",
            )
            .in_category("agent"),
        Arc::new(SubAgentsOption),
    )
}

struct SubAgentsOption;

impl ConfigOptionProvider for SubAgentsOption {
    fn current(&self, _session: Option<&str>) -> String {
        if rebon_config::saved_sub_agents_enabled() {
            "on"
        } else {
            "off"
        }
        .to_string()
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some("Offer the Agent tool.".to_string()),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some(
                    "Take the Agent tool and the delegation prompt section off.".to_string(),
                ),
            },
        ]
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let enabled = match value {
            "on" => true,
            "off" => false,
            other => return Err(format!("`{other}` is not on or off")),
        };
        // The process-global atomic first, so the next turn filters the tool
        // without a restart; then the file, so the choice survives one.
        rebon_tool::set_sub_agents_enabled(enabled);
        rebon_config::save_sub_agents_enabled_in_dir(&rebon_config::config_home_dir(), enabled)
            .map_err(|error| format!("could not save the sub-agent setting: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_config_seat::ConfigSeat;
    use rebon_kernel::Kernel;

    #[test]
    fn registering_without_a_seat_is_not_a_failure() {
        let kernel = Kernel::new();
        register(kernel.context()).expect("a missing seat is tolerated");
    }

    /// The row must not outlive the plugin: offering to switch sub-agents on
    /// while nothing implements them is offering nothing.
    #[test]
    fn the_row_leaves_with_the_plugin() {
        let kernel = Kernel::new();
        let seat = ConfigSeat::new();
        kernel
            .context()
            .provide::<ConfigSeatService>(seat.clone())
            .expect("provides the seat");

        let scope = kernel.context().fork(crate::PLUGIN_ID);
        register(&scope).expect("registers");
        assert!(seat.has(SUB_AGENTS_OPTION));

        scope.dispose();
        assert!(!seat.has(SUB_AGENTS_OPTION));
    }

    /// The shape `rebon-acp`'s list used to pin, now pinned where the row is.
    #[test]
    fn the_row_keeps_the_name_and_values_every_surface_shows() {
        let kernel = Kernel::new();
        let seat = ConfigSeat::new();
        register_on(kernel.context(), &seat);
        let row = seat
            .options(None)
            .into_iter()
            .find(|option| option.id == SUB_AGENTS_OPTION)
            .expect("registered");
        assert_eq!(row.name, "Sub-agents");
        assert_eq!(row.category.as_deref(), Some("agent"));
        assert!(matches!(
            row.option_type,
            rebon_config_seat::ConfigOptionType::Select
        ));
        assert_eq!(
            row.options
                .iter()
                .map(|value| value.value.as_str())
                .collect::<Vec<_>>(),
            vec!["on", "off"]
        );
    }

    fn register_on(ctx: &Context, seat: &std::sync::Arc<ConfigSeat>) {
        ctx.provide::<ConfigSeatService>(seat.clone())
            .expect("provides the seat");
        register(ctx).expect("registers");
    }

    #[test]
    fn a_value_that_is_neither_on_nor_off_is_refused() {
        let err = SubAgentsOption
            .apply(None, "delegate")
            .expect_err("refused");
        assert!(err.contains("delegate"), "{err}");
    }
}
