//! The terminal's way in to the kernel's `ui-registry` seat.
//!
//! Every panel is named by a stable id and built by a factory taking
//! [`DialogArgs`], so an entry point asks for an id instead of naming a
//! type and a plugin's panel opens through the same call.
//!
//! No panel is registered here any more: `core-ui` in the harness holds
//! the ones whose data this surface collects, the feature plugins hold
//! their own, and this module is the one call that opens any of them.

use std::sync::{Arc, OnceLock};

use rebon_dialog::model::DialogModel;
use rebon_plugin_tasks::ui::background_tasks_dialog::{
    BackgroundTasksDialogOpen, BackgroundTasksDialogState,
};
use rebon_ui_seat::{ids, DialogArgs, UiSeat, UiSeatService};

/// Open the panel registered as `id`.
///
/// `None` when nothing answers to the id — the plugin that registers it
/// is off, or no kernel came up — or when the panel declined to open: no
/// active provider for the model picker, no loaded skills for the
/// selector, no diagnostics report for `/doctor`. Either way it is the
/// caller's cue to fall back to the textual command.
pub fn open(id: &str, args: DialogArgs) -> Option<Box<dyn DialogModel>> {
    seat()?.open(id, args)
}

/// Open the panel registered as `id` and hand back its concrete state.
///
/// The background-task panel is the one panel this surface still keeps in
/// `AppState` under its own type rather than on the dialog stack: a dozen
/// layout and key-routing decisions read it directly, so moving it is its
/// own piece of work. This is the downcast the trait's `as_any` exists
/// for, in one place instead of at every open site.
///
/// `None` means the panel is not registered — its plugin is off — or that
/// the factory declined, and the caller must not open anything.
pub fn open_as<T: Clone + 'static>(id: &str, args: DialogArgs) -> Option<T> {
    let model = open(id, args)?;
    let state = model.as_any().downcast_ref::<T>();
    debug_assert!(
        state.is_some(),
        "dialog {id} is registered under a different type than the caller expects"
    );
    state.cloned()
}

/// The process seat.
///
/// Resolved once, lazily: a kernel boots plugins, and the first panel a
/// user opens is a fair place to pay for that, whereas TUI startup is not.
fn seat() -> Option<&'static Arc<UiSeat>> {
    static SEAT: OnceLock<Option<Arc<UiSeat>>> = OnceLock::new();
    SEAT.get_or_init(|| {
        let kernel = rebon_harness::kernel_bootstrap::process_kernel();
        let ctx = kernel.context().fork("tui-dialogs");
        ctx.require::<UiSeatService>().ok()
    })
    .as_ref()
}

/// Open `/tasks` (or `/workflows`, which is the same panel with a kind
/// filter) through the seat.
///
/// `None` when the tasks plugin is off: its thirteen tools, its runtime
/// and this panel go together, so a session without it has no background
/// tasks to show.
pub fn open_background_tasks(
    opened: BackgroundTasksDialogOpen,
) -> Option<BackgroundTasksDialogState> {
    open_as(ids::dialog::TASKS, DialogArgs::payload(opened))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_id_opens_nothing() {
        assert!(open("no-such-panel", DialogArgs::none()).is_none());
    }

    #[test]
    fn a_panel_needing_a_payload_declines_the_wrong_one() {
        // The factories live on the seat now, so this is what a caller
        // that got its inputs wrong actually sees.
        assert!(open(ids::dialog::SETTINGS, DialogArgs::payload(7u32)).is_none());
        assert!(open(ids::dialog::DOCTOR, DialogArgs::none()).is_none());
    }

    #[test]
    fn a_registered_panel_opens_and_reports_the_id_it_was_asked_for() {
        // A panel registered under one id but reporting another would
        // route its actions into a dead arm.
        let opened = open(
            ids::dialog::SETTINGS,
            DialogArgs::payload(rebon_dialog::settings_dialog::SettingsDialogOpen::default()),
        )
        .expect("core-ui registers the settings panel");
        assert_eq!(opened.id(), ids::dialog::SETTINGS);
    }
}
