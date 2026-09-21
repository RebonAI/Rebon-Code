//! `HookSourceProvider` impl backed by parsed settings files.
//!
//! Glue that turns the flat [`crate::settings_loader::LoadedHooks`] vec,
//! an optional policy flag, and the optional plugin and session hook
//! lists into the trait shape the grouping and runtime layers expect.
//!
//! The provider surfaces, in this composition:
//!
//! * Editable settings hooks (user + project + local)
//! * Plugin hooks (from `~/.rebon/plugins/*/hooks/hooks.json` — caller
//!   supplies them already parsed)
//! * Session hooks (host-injected, in-memory)
//! * Builtin/registered hooks (surfaced through `registered_hooks`)
//! * `allowManagedHooksOnly` short-circuit from policy settings
//!
//! The provider is **immutable**. Hot-reload rebuilds a fresh
//! provider from a new snapshot; the runtime only ever sees a stable
//! view for the duration of one event firing.

use crate::grouping::{HookSourceProvider, RegisteredHookEntry};
use crate::individual_hook::IndividualHookConfig;

/// Immutable snapshot of everything the provider surfaces. Built from
/// settings files, session hooks, plugin hooks, and registered hooks.
///
/// Kept as a flat struct with `Vec` fields so callers can swap it out
/// wholesale for hot-reload without touching the trait impl.
#[derive(Debug, Clone, Default)]
pub struct SettingsSnapshot {
    /// Hooks sourced from the editable settings files (user, project,
    /// local). Tagged with the correct [`crate::hook_source::HookSource`].
    pub settings_hooks: Vec<IndividualHookConfig>,
    /// Hooks sourced from plugins. Tagged with `PluginHook` and the
    /// plugin name in `plugin_name`.
    pub plugin_hooks: Vec<IndividualHookConfig>,
    /// Hooks registered for the lifetime of the current session (e.g.
    /// injected from a worker or a TUI action). Tagged with
    /// `SessionHook`.
    pub session_hooks: Vec<IndividualHookConfig>,
    /// Registered / builtin hook *entries*, passed straight through by
    /// [`HookSourceProvider::registered_hooks`].
    pub registered: Vec<RegisteredHookEntry>,
    /// The `allowManagedHooksOnly` policy setting. Blocks all
    /// non-plugin/non-builtin hooks from selection.
    pub allow_managed_hooks_only: bool,
}

/// `HookSourceProvider` impl that reads from a pre-built snapshot.
///
/// The runtime rebuilds a provider whenever the underlying settings
/// files change; between rebuilds the snapshot is immutable, so the
/// trait impl never has to lock.
#[derive(Debug, Clone, Default)]
pub struct SettingsHookProvider {
    snapshot: SettingsSnapshot,
}

impl SettingsHookProvider {
    pub fn new(snapshot: SettingsSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn snapshot(&self) -> &SettingsSnapshot {
        &self.snapshot
    }

    /// Replace the snapshot in-place. Used by whatever watches the
    /// settings files, when a change fires.
    pub fn replace_snapshot(&mut self, snapshot: SettingsSnapshot) {
        self.snapshot = snapshot;
    }
}

impl HookSourceProvider for SettingsHookProvider {
    fn restricted_to_managed_only(&self) -> bool {
        self.snapshot.allow_managed_hooks_only
    }

    fn hooks_from_settings(&self) -> Vec<IndividualHookConfig> {
        // Deterministic hook order: editable settings → plugin hooks →
        // session hooks. The runtime's dedupe pass
        // keeps the first occurrence, so this ordering is the one
        // observable via `matcher` priority resolution.
        let mut out = Vec::with_capacity(
            self.snapshot.settings_hooks.len()
                + self.snapshot.plugin_hooks.len()
                + self.snapshot.session_hooks.len(),
        );
        out.extend(self.snapshot.settings_hooks.iter().cloned());
        out.extend(self.snapshot.plugin_hooks.iter().cloned());
        out.extend(self.snapshot.session_hooks.iter().cloned());
        out
    }

    fn registered_hooks(&self) -> Vec<RegisteredHookEntry> {
        self.snapshot.registered.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookEvent;
    use crate::hook_command::{BashCommandHook, HookCommand};
    use crate::hook_source::HookSource;

    fn cmd(source: HookSource, command: &str) -> IndividualHookConfig {
        IndividualHookConfig {
            event: HookEvent::PreToolUse,
            config: HookCommand::Command(BashCommandHook {
                command: command.into(),
                r#if: None,
                shell: None,
                timeout: None,
                status_message: None,
                once: None,
                r#async: None,
                async_rewake: None,
            }),
            matcher: None,
            source,
            plugin_name: None,
        }
    }

    #[test]
    fn empty_snapshot_produces_empty_provider() {
        let p = SettingsHookProvider::default();
        assert!(p.hooks_from_settings().is_empty());
        assert!(p.registered_hooks().is_empty());
        assert!(!p.restricted_to_managed_only());
    }

    #[test]
    fn provider_merges_all_three_sources_in_order() {
        let p = SettingsHookProvider::new(SettingsSnapshot {
            settings_hooks: vec![cmd(HookSource::UserSettings, "a")],
            plugin_hooks: vec![cmd(HookSource::PluginHook, "b")],
            session_hooks: vec![cmd(HookSource::SessionHook, "c")],
            registered: vec![],
            allow_managed_hooks_only: false,
        });
        let all = p.hooks_from_settings();
        assert_eq!(all.len(), 3);
        assert!(matches!(all[0].config, HookCommand::Command(ref c) if c.command == "a"));
        assert!(matches!(all[1].config, HookCommand::Command(ref c) if c.command == "b"));
        assert!(matches!(all[2].config, HookCommand::Command(ref c) if c.command == "c"));
    }

    #[test]
    fn provider_exposes_policy_flag() {
        let p = SettingsHookProvider::new(SettingsSnapshot {
            allow_managed_hooks_only: true,
            ..SettingsSnapshot::default()
        });
        assert!(p.restricted_to_managed_only());
    }

    #[test]
    fn replace_snapshot_swaps_backing_data() {
        let mut p = SettingsHookProvider::new(SettingsSnapshot {
            settings_hooks: vec![cmd(HookSource::UserSettings, "old")],
            ..SettingsSnapshot::default()
        });
        p.replace_snapshot(SettingsSnapshot {
            settings_hooks: vec![cmd(HookSource::UserSettings, "new")],
            ..SettingsSnapshot::default()
        });
        let all = p.hooks_from_settings();
        assert_eq!(all.len(), 1);
        match &all[0].config {
            HookCommand::Command(c) => assert_eq!(c.command, "new"),
            _ => unreachable!(),
        }
    }
}
