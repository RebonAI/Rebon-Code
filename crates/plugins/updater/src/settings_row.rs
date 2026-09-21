//! The `update_auto_install` row this plugin puts in the settings panel.
//!
//! The preference is the updater's, so the row is the updater's. It used to
//! sit in a fixed list inside `rebon-acp`, which meant a setting for a
//! feature plugin was written next to settings that plugin knows nothing
//! about, and it stayed on the panel when the plugin was switched off — a row
//! that changed a file nothing read.

use std::sync::Arc;

use rebon_config_seat::{
    ConfigOptionProvider, ConfigOptionSpec, ConfigOptionValue, ConfigSeatService,
};
use rebon_kernel::{Context, KernelError};

/// The id the panel and every surface's dispatch name this row by.
pub const AUTO_INSTALL_OPTION: &str = "update_auto_install";

/// Register the row, when a seat is there to take it.
///
/// A missing seat is not an error: the seat is a Core plugin, and a
/// composition that leaves it out still updates, just without a row.
pub(crate) fn register(ctx: &Context) -> Result<(), KernelError> {
    let Some(seat) = ctx.get::<ConfigSeatService>() else {
        return Ok(());
    };
    seat.register(
        ctx,
        ConfigOptionSpec::select(AUTO_INSTALL_OPTION, "Auto install updates")
            .describe(
                "Stores the auto-install preference; register the per-user background \
                 runner explicitly with `rebon update service install`. Package \
                 installation waits for installer support.",
            )
            .in_category("updates"),
        Arc::new(AutoInstallOption),
    )
}

struct AutoInstallOption;

impl ConfigOptionProvider for AutoInstallOption {
    fn current(&self, _session: Option<&str>) -> String {
        let enabled = rebon_config::load_update_preferences()
            .map(|prefs| prefs.auto_install)
            .unwrap_or(false);
        if enabled { "on" } else { "off" }.to_string()
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some(
                    "Store the preference only; install the background runner explicitly."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some("Do not install updates in the background.".to_string()),
            },
        ]
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let enabled = match value {
            "on" => true,
            "off" => false,
            other => return Err(format!("`{other}` is not on or off")),
        };
        crate::save_update_auto_install_setting(enabled)
            .map_err(|error| format!("could not save the update preference: {error}"))
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
        assert!(seat.has(AUTO_INSTALL_OPTION));

        scope.dispose();
        assert!(
            !seat.has(AUTO_INSTALL_OPTION),
            "no check, no notice, and no row either"
        );
    }

    /// The shape `rebon-acp`'s list used to pin, now pinned where the row is.
    #[test]
    fn the_row_keeps_the_name_and_category_every_surface_shows() {
        let kernel = Kernel::new();
        let seat = ConfigSeat::new();
        kernel
            .context()
            .provide::<ConfigSeatService>(seat.clone())
            .expect("provides the seat");
        register(kernel.context()).expect("registers");
        let row = seat
            .options(None)
            .into_iter()
            .find(|option| option.id == AUTO_INSTALL_OPTION)
            .expect("registered");
        assert_eq!(row.name, "Auto install updates");
        assert_eq!(row.category.as_deref(), Some("updates"));
        assert!(matches!(
            row.option_type,
            rebon_config_seat::ConfigOptionType::Select
        ));
    }

    #[test]
    fn a_value_that_is_neither_on_nor_off_is_refused() {
        let err = AutoInstallOption
            .apply(None, "sometimes")
            .expect_err("refused");
        assert!(err.contains("sometimes"), "{err}");
    }
}
