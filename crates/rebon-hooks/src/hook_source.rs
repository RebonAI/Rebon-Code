//! `HookSource` enum, its one display label, and the priority lookup
//! the runtime orders matchers by.
//!
//! ## Behaviour
//!
//! [`HookSource::header`] returns the label a hook from that source is
//! shown under: `User Settings`, `Project Settings`, `Local Settings`,
//! `Plugin Hooks`, `Session Hooks`, `Built-in Hooks`, and the raw
//! `policySettings` string for the policy variant. These are
//! user-visible strings, so the tests pin them one by one.
//!
//! [`HookSource::priority`] orders the editable sources by their index
//! in the `localSettings`, `projectSettings`, `userSettings` order:
//! `LocalSettings` = 0, `ProjectSettings` = 1, `UserSettings` = 2.
//! `PluginHook` and `BuiltinHook` map to the `999` sentinel. Lower
//! number wins.
//!
//! `PolicySettings` is modeled for completeness, but no hooks are
//! emitted with that source: the policy settings file is read for the
//! `disableAllHooks` / `allowManagedHooksOnly` flags only. `priority`
//! folds it into the same sentinel as plugin/builtin, since it never
//! reaches a matcher list.

/// Pseudo-priority returned for [`HookSource::PluginHook`] and
/// [`HookSource::BuiltinHook`]. `999` is the sentinel for "lowest
/// priority", chosen so the sort comparator produces a stable total
/// ordering.
pub const PLUGIN_OR_BUILTIN_PRIORITY: i32 = 999;

/// Where a hook came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookSource {
    /// `~/.rebon/settings.json` — user-global hook config.
    UserSettings,
    /// `.rebon/settings.json` — project-shared hook config.
    ProjectSettings,
    /// `.rebon/settings.local.json` — project-local, gitignored.
    LocalSettings,
    /// Managed/policy settings file. Read for `disableAllHooks` /
    /// `allowManagedHooksOnly` only; no hooks are emitted with this
    /// source today, but the variant is modeled for
    /// completeness.
    PolicySettings,
    /// Plugin hook from `~/.rebon/plugins/*/hooks/hooks.json`.
    PluginHook,
    /// Session-scoped hook (in-memory; cleared at session end).
    SessionHook,
    /// Rebon-internal builtin hook (registered programmatically).
    /// Surfaced in the UI only when the caller's internal-build gate
    /// emits it.
    BuiltinHook,
}

impl HookSource {
    /// The label a hook from this source is shown under.
    pub const fn header(self) -> &'static str {
        match self {
            HookSource::UserSettings => "User Settings",
            HookSource::ProjectSettings => "Project Settings",
            HookSource::LocalSettings => "Local Settings",
            HookSource::PluginHook => "Plugin Hooks",
            HookSource::SessionHook => "Session Hooks",
            HookSource::BuiltinHook => "Built-in Hooks",
            // The policy variant has no dedicated label; it falls back
            // to its raw source string.
            HookSource::PolicySettings => "policySettings",
        }
    }

    /// Display-sort priority for a hook source:
    ///
    /// ```text
    /// LocalSettings            → 0
    /// ProjectSettings          → 1
    /// UserSettings             → 2
    /// PluginHook / BuiltinHook → 999 (sentinel)
    /// ```
    ///
    /// Lower number wins. Variants outside the editable set are mapped
    /// to the same `999` sentinel so the comparator produces a
    /// deterministic total ordering.
    pub const fn priority(self) -> i32 {
        match self {
            HookSource::LocalSettings => 0,
            HookSource::ProjectSettings => 1,
            HookSource::UserSettings => 2,
            // `PolicySettings`, `PluginHook`, `SessionHook` and
            // `BuiltinHook` all sort into the lowest-priority bucket.
            // We pin all four to 999 so the sort is total; only
            // plugin/builtin actually appear in matcher lists today.
            _ => PLUGIN_OR_BUILTIN_PRIORITY,
        }
    }
}

/// Free-function alias for [`HookSource::priority`].
pub fn source_priority(source: HookSource) -> i32 {
    source.priority()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_strings_are_expected() {
        assert_eq!(HookSource::UserSettings.header(), "User Settings");
        assert_eq!(HookSource::ProjectSettings.header(), "Project Settings");
        assert_eq!(HookSource::LocalSettings.header(), "Local Settings");
        assert_eq!(HookSource::PluginHook.header(), "Plugin Hooks");
        assert_eq!(HookSource::SessionHook.header(), "Session Hooks");
        assert_eq!(HookSource::BuiltinHook.header(), "Built-in Hooks");
        assert_eq!(HookSource::PolicySettings.header(), "policySettings");
    }

    #[test]
    fn priority_local_is_highest() {
        // Lower number wins; LocalSettings is priority 0.
        assert_eq!(HookSource::LocalSettings.priority(), 0);
    }

    #[test]
    fn priority_project_is_middle() {
        assert_eq!(HookSource::ProjectSettings.priority(), 1);
    }

    #[test]
    fn priority_user_is_lowest_editable() {
        assert_eq!(HookSource::UserSettings.priority(), 2);
    }

    #[test]
    fn priority_plugin_is_sentinel() {
        assert_eq!(HookSource::PluginHook.priority(), 999);
    }

    #[test]
    fn priority_builtin_is_sentinel() {
        assert_eq!(HookSource::BuiltinHook.priority(), 999);
    }

    #[test]
    fn priority_total_ordering() {
        // Local < Project < User < Plugin == Builtin
        assert!(HookSource::LocalSettings.priority() < HookSource::ProjectSettings.priority());
        assert!(HookSource::ProjectSettings.priority() < HookSource::UserSettings.priority());
        assert!(HookSource::UserSettings.priority() < HookSource::PluginHook.priority());
        assert_eq!(
            HookSource::PluginHook.priority(),
            HookSource::BuiltinHook.priority()
        );
    }

    #[test]
    fn priority_constant_pinned_to_999() {
        assert_eq!(PLUGIN_OR_BUILTIN_PRIORITY, 999);
    }
}
