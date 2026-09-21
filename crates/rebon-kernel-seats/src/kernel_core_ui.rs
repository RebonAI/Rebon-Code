//! `core-ui`: the Core plugin that owns the `ui-registry` seat.
//!
//! It provides the seat on the kernel root and registers the panels whose
//! reducers live in `rebon-dialog` and whose data is not a front end's
//! own: the reasoning level, the provider switcher and its model picker,
//! read out of `rebon-config`, plus the context browser, whose entries
//! the front end collects and hands over as data.
//!
//! It also registers the panels a front end collects the data for but does
//! not own the shape of — the diagnostics report, the hook browser, the
//! plugin manager, the settings surface. Their inputs arrive as a payload
//! the factory downcasts; a feature plugin's panels stay with the plugin.

use rebon_dialog::context_dialog::ContextDialogState;
use rebon_dialog::doctor_dialog::{DoctorDialogState, DoctorReport};
use rebon_dialog::effort_dialog::EffortDialogState;
use rebon_dialog::hooks_dialog::{HooksDialogInput, HooksDialogState};
use rebon_dialog::model_dialog::ModelDialogState;
use rebon_dialog::plugins_dialog::PluginsDialogState;
use rebon_dialog::provider_dialog::ProviderDialogState;
use rebon_dialog::settings_dialog::{SettingsDialogOpen, SettingsDialogState};
use rebon_kernel::{Context, KernelError, Plugin, PluginMeta, Service};
use rebon_ui_seat::ids;
use rebon_ui_seat::input::{decode, ContextDialogInput};
pub use rebon_ui_seat::{DialogArgs, DialogDef, UiSeat, UiSeatService};

/// The plugin id, which is also its config key and the name `/plugins` shows.
pub const PLUGIN_ID: &str = "core-ui";

pub struct CoreUiPlugin;

impl Plugin for CoreUiPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).provides(&[<UiSeatService as Service>::NAME])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = UiSeat::new();
        ctx.provide::<UiSeatService>(seat.clone())?;
        for def in config_dialog_defs() {
            seat.register_dialog(ctx, def)?;
        }
        Ok(())
    }
}

/// The panels this plugin owns: the ones whose rows come out of the
/// configuration this process already reads.
fn config_dialog_defs() -> Vec<DialogDef> {
    vec![
        // `[model_name, current_level_id]`. An unknown or absent level
        // leaves the picker on its default.
        DialogDef::new(ids::dialog::EFFORT, |args| {
            let current = Some(args.value_at(1)).filter(|id| !id.is_empty());
            Some(Box::new(EffortDialogState::open(args.value_at(0), current)))
        }),
        // No inputs: the active provider and its models come from config.
        DialogDef::new(ids::dialog::MODEL, |_| {
            let name = rebon_config::get_active_custom_provider_name()?;
            let provider = rebon_config::list_custom_providers()
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(&name))?;
            // The provider's `models[]` first, then the built-in set for
            // the OpenAI OAuth entry (new models without re-login), then
            // the rest of that vendor's catalogue — each row carrying the
            // context window and price the catalogue knows for it.
            let options = rebon_config::provider_model_choices(&provider)
                .into_iter()
                .map(|choice| rebon_dialog::model_dialog::ModelOption {
                    id: choice.id,
                    detail: choice.detail,
                    configured: choice.configured,
                })
                .collect();
            let model = provider.model.clone();
            ModelDialogState::new(provider.name, options, model)
                .map(|dialog| Box::new(dialog) as Box<dyn rebon_dialog::model::DialogModel>)
        }),
        // One value: a JSON `ContextDialogInput`. The entries under
        // each category come from a transcript this process cannot see,
        // so the front end collects them and sends them along.
        DialogDef::new(ids::dialog::CONTEXT, |args| {
            let input: ContextDialogInput = decode(args.value_at(0))?;
            Some(Box::new(ContextDialogState::open(
                &input.content,
                input.category_items,
            )))
        }),
        // A `DoctorReport` payload: the probes run in the front end's
        // session half, which is what can read the disk and ask the tool
        // layer questions. Nothing to show without one, so no report is
        // a declined open.
        DialogDef::new(ids::dialog::DOCTOR, |args| {
            let report = args.payload_as::<DoctorReport>()?;
            Some(Box::new(DoctorDialogState::open(report)))
        }),
        // `[list_text, "ok" | "err"]`: the output of `/plugin list`,
        // which only the front end's command path can run. Always opens
        // — an error is a row in the panel, not a refusal.
        DialogDef::new(ids::dialog::PLUGINS, |args| {
            Some(Box::new(PluginsDialogState::open(
                args.value_at(0),
                args.value_at(1) == "err",
            )))
        }),
        // A `HooksDialogInput` payload: which events exist needs the
        // tool list and the agent registry, and what is configured needs
        // the settings files, so the front end collects both.
        DialogDef::new(ids::dialog::HOOKS, |args| {
            let input = args.payload_as::<HooksDialogInput>()?;
            Some(Box::new(HooksDialogState::open(input)))
        }),
        // A `SettingsDialogOpen` payload: which tab, plus the first
        // projection, so the opening frame paints live values instead of
        // an empty shell. Later frames refresh it through `top_as_mut`.
        DialogDef::new(ids::dialog::SETTINGS, |args| {
            let opened = args.payload_as::<SettingsDialogOpen>()?;
            Some(Box::new(SettingsDialogState::open(opened)))
        }),
        // No inputs either. Always opens: even with no custom provider
        // the "Default (env)" row is there to add one from.
        DialogDef::new(ids::dialog::PROVIDER, |_| {
            let rows = rebon_config::list_custom_providers()
                .into_iter()
                .map(|provider| {
                    let subtitle = if provider.model.trim().is_empty() {
                        "no model set".to_string()
                    } else {
                        provider.model
                    };
                    (provider.name, subtitle)
                })
                .collect::<Vec<_>>();
            Some(Box::new(ProviderDialogState::new(
                rows,
                rebon_config::get_active_custom_provider_name(),
            )))
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    #[test]
    fn the_plugin_provides_the_seat_with_its_config_and_report_panels() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        CoreUiPlugin.apply(&ctx).unwrap();

        let seat = ctx.require::<UiSeatService>().expect("seat provided");
        assert_eq!(
            seat.ids(),
            vec![
                ids::dialog::EFFORT,
                ids::dialog::MODEL,
                ids::dialog::CONTEXT,
                ids::dialog::DOCTOR,
                ids::dialog::PLUGINS,
                ids::dialog::HOOKS,
                ids::dialog::SETTINGS,
                ids::dialog::PROVIDER
            ]
        );
        assert_eq!(<UiSeatService as Service>::NAME, "ui-registry");
    }

    #[test]
    fn the_effort_picker_opens_off_its_two_values() {
        let seat = UiSeat::new();
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        for def in config_dialog_defs() {
            seat.register_dialog(&ctx, def).unwrap();
        }

        let opened = seat
            .open(ids::dialog::EFFORT, DialogArgs::values(["gpt", "max"]))
            .expect("the effort picker always opens");
        assert_eq!(opened.id(), ids::dialog::EFFORT);
        // An empty level is "none given", not an unknown level.
        assert!(seat
            .open(ids::dialog::EFFORT, DialogArgs::values(["gpt", ""]))
            .is_some());
    }

    #[test]
    fn the_context_browser_declines_a_malformed_input() {
        let seat = UiSeat::new();
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        for def in config_dialog_defs() {
            seat.register_dialog(&ctx, def).unwrap();
        }

        assert!(seat
            .open(ids::dialog::CONTEXT, DialogArgs::values(["not json"]))
            .is_none());
        let input = rebon_ui_seat::input::encode(&ContextDialogInput {
            content: "Context Usage\n".into(),
            category_items: vec![vec!["#1 User: hi".into()]],
        });
        assert!(seat
            .open(ids::dialog::CONTEXT, DialogArgs::values([input]))
            .is_some());
    }

    #[test]
    fn the_diagnostics_panel_needs_a_report_to_open() {
        let seat = UiSeat::new();
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        for def in config_dialog_defs() {
            seat.register_dialog(&ctx, def).unwrap();
        }

        assert!(seat.open(ids::dialog::DOCTOR, DialogArgs::none()).is_none());
        // A payload of the wrong type is the same "no" as none at all.
        assert!(seat
            .open(ids::dialog::DOCTOR, DialogArgs::payload(7u32))
            .is_none());
        let report = DoctorReport {
            summary: vec![("version".into(), "1.0".into())],
            sections: Vec::new(),
        };
        assert!(seat
            .open(ids::dialog::DOCTOR, DialogArgs::payload(report))
            .is_some());
    }

    #[test]
    fn the_plugin_manager_opens_on_a_list_and_on_an_error() {
        let seat = UiSeat::new();
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        for def in config_dialog_defs() {
            seat.register_dialog(&ctx, def).unwrap();
        }

        assert!(seat
            .open(
                ids::dialog::PLUGINS,
                DialogArgs::values(["alpha 1.0.0 user enabled src", "ok"])
            )
            .is_some());
        assert!(seat
            .open(
                ids::dialog::PLUGINS,
                DialogArgs::values(["no plugin store", "err"])
            )
            .is_some());
    }

    #[test]
    fn the_hook_browser_needs_its_collected_events() {
        let seat = UiSeat::new();
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        for def in config_dialog_defs() {
            seat.register_dialog(&ctx, def).unwrap();
        }

        assert!(seat.open(ids::dialog::HOOKS, DialogArgs::none()).is_none());
        assert!(seat
            .open(ids::dialog::HOOKS, DialogArgs::payload(7u32))
            .is_none());
        assert!(seat
            .open(
                ids::dialog::HOOKS,
                DialogArgs::payload(HooksDialogInput::default())
            )
            .is_some());
    }

    #[test]
    fn the_settings_panel_needs_its_first_projection() {
        let seat = UiSeat::new();
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        for def in config_dialog_defs() {
            seat.register_dialog(&ctx, def).unwrap();
        }

        assert!(seat
            .open(ids::dialog::SETTINGS, DialogArgs::none())
            .is_none());
        let opened = seat
            .open(
                ids::dialog::SETTINGS,
                DialogArgs::payload(SettingsDialogOpen::default()),
            )
            .expect("a projection is enough to open on");
        assert_eq!(opened.id(), ids::dialog::SETTINGS);
    }

    #[test]
    fn the_provider_switcher_opens_even_with_nothing_configured() {
        let seat = UiSeat::new();
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        for def in config_dialog_defs() {
            seat.register_dialog(&ctx, def).unwrap();
        }
        assert!(seat
            .open(ids::dialog::PROVIDER, DialogArgs::none())
            .is_some());
    }

    #[test]
    fn the_meta_advertises_the_seat_it_provides() {
        let meta = CoreUiPlugin.meta();
        assert_eq!(meta.name, PLUGIN_ID);
        assert!(meta.provides.iter().any(|name| name == "ui-registry"));
    }
}
