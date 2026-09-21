//! The stable ids a dialog and its actions answer to.
//!
//! They live here, in the seat, because the two halves that must agree on
//! them do not live together: a panel is registered by whoever owns its
//! state, while the action it emits is routed somewhere else entirely. A
//! literal spelled twice across a crate boundary is a bug nobody sees until
//! the action silently stops arriving, so both sides read the constant.
//!
//! An id is part of the seat's public surface. Renaming one breaks every
//! surface that routes it, the same way renaming a command would.

/// Dialog ids.
pub mod dialog {
    /// `/effort` — the reasoning-level picker.
    pub const EFFORT: &str = "effort";
    /// `/model` — the active provider's model picker.
    pub const MODEL: &str = "model";
    /// `/provider` — the provider switcher.
    pub const PROVIDER: &str = "provider";
    /// `/memory` — the loaded instruction-file browser.
    pub const MEMORY: &str = "memory";
    /// `/agents` — the agent-definition browser, wizard and editor.
    pub const AGENTS: &str = "agents";
    /// `/doctor` — the diagnostics panel.
    pub const DOCTOR: &str = "doctor";
    /// `/sandbox` — the sandbox config / overrides / violations panel.
    pub const SANDBOX: &str = "sandbox";
    /// `/skills` — the skill multi-selector.
    pub const SKILLS: &str = "skills";
    /// `/plugin` — the plugin manager.
    pub const PLUGINS: &str = "plugins";
    /// `/hooks` — the read-only hook browser.
    pub const HOOKS: &str = "hooks";
    /// `/settings` — the settings panel.
    pub const SETTINGS: &str = "settings";
    /// `/context` — the context-usage browser.
    pub const CONTEXT: &str = "context";
    /// `/tasks` — the background-task dialog. `/workflows` opens the same
    /// panel filtered to workflow runs, so it answers to this id too.
    pub const TASKS: &str = "tasks";
    /// `/teams` — the teammate overlay.
    pub const TEAMS: &str = "teams";
    /// Ctrl+R — the search over past prompts.
    pub const HISTORY_SEARCH: &str = "history-search";
    /// Ctrl+P — the fuzzy file picker.
    pub const QUICK_OPEN: &str = "quick-open";
}

/// Action ids, scoped by the dialog that emits them.
///
/// Several dialogs emit an action they all call `select` or `apply`; the
/// front end matches on the pair, so the names repeat without colliding.
pub mod action {
    /// Apply the highlighted reasoning level. One value: the level id.
    pub const SELECT: &str = "select";
    /// Persist and install a skill selection. Values: the disabled ids.
    pub const APPLY: &str = "apply";
    /// Open the highlighted file in an external editor. One value: path.
    pub const OPEN: &str = "open";
    /// Recompute the diagnostics report. No values.
    pub const RERUN: &str = "rerun";
    /// Run a textual command the panel composed. One value: the command.
    pub const EXECUTE: &str = "execute";
    /// Persist one config option. Values: `[config_id, value]`.
    pub const APPLY_CONFIG: &str = "apply-config";
    /// Activate a provider. One value, or none for the environment row.
    pub const ACTIVATE: &str = "activate";
    /// Open the provider form in add mode. No values.
    pub const ADD: &str = "add";
    /// Open the provider form editing one provider. One value: its name.
    pub const EDIT: &str = "edit";
    /// Remove one provider. One value: its name.
    pub const REMOVE: &str = "remove";
    /// Compact the transcript. No values.
    pub const COMPACT: &str = "compact";
    /// Run a prune sweep. No values.
    pub const PRUNE_SWEEP: &str = "prune-sweep";
    /// Generate an agent definition from a description. One value: the
    /// description. The panel stays open, waiting for the result.
    pub const GENERATE: &str = "generate";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dialog_id_is_distinct() {
        let ids = [
            dialog::EFFORT,
            dialog::MODEL,
            dialog::PROVIDER,
            dialog::MEMORY,
            dialog::AGENTS,
            dialog::DOCTOR,
            dialog::SANDBOX,
            dialog::SKILLS,
            dialog::PLUGINS,
            dialog::HOOKS,
            dialog::SETTINGS,
            dialog::CONTEXT,
            dialog::TASKS,
            dialog::TEAMS,
            dialog::HISTORY_SEARCH,
            dialog::QUICK_OPEN,
        ];
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "two panels share an id");
    }
}
