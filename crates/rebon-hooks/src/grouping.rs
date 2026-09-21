//! Hook grouping, sorting, and lookup helpers.
//!
//! ## Behaviour
//!
//! [`group_hooks_by_event_and_matcher`] starts from `empty_grouped`,
//! which pre-inserts one empty `BTreeMap` per variant in `HOOK_EVENTS`.
//! Unless the provider reports `restricted_to_managed_only`, every hook
//! returned by `hooks_from_settings` is filed under
//! `matcher_key_for(event, hook.matcher, metadata)`. Then every entry of
//! `registered_hooks` is folded in. A [`RegisteredHookEntry::Plugin`]
//! pushes one [`IndividualHookConfig`] per hook, carrying
//! [`HookSource::PluginHook`] and the entry's `plugin_id` as
//! `plugin_name`. A [`RegisteredHookEntry::Builtin`] pushes `hook_count`
//! copies of a synthesised command hook whose text is the literal
//! `"[internal] Built-in Hook"`, with no `plugin_name`. Both kinds key on
//! `matcher.unwrap_or_default()`. Every insert goes through
//! `grouped.get_mut(&event)`, so an entry whose event is not one of
//! `HOOK_EVENTS` is silently skipped.
//!
//! [`sorted_matchers_for_event`] collects the inner keys for one event
//! and sorts them with `highest_priority` — the smallest `priority()`
//! among the bucket's distinct [`HookSource`] values — and falls back to
//! plain string comparison when two matchers tie. [`hooks_for_matcher`]
//! looks up one bucket by `matcher.unwrap_or("")` and returns an empty
//! slice when the bucket is absent.
//!
//! Five load-bearing details:
//!
//! 1. **Empty `matcher` collapses to `""`.** A `None` matcher and a
//!    `Some("")` matcher both write into the same dictionary key.
//! 2. **Events without matcher metadata always use `""` as the
//!    matcher key**, even if the hook record carries a matcher.
//! 3. **Plugin hooks are tagged with `plugin_name` taken from the
//!    plugin id.** Modeled by `RegisteredHookEntry::Plugin`.
//! 4. **Builtin hooks are gated on the internal build.** The seam
//!    surfaces them via `RegisteredHookEntry::Builtin`; the
//!    `HookSourceProvider` impl decides whether to emit them.
//! 5. **Matcher sort: priority first, then lexicographic name order.**
//!    Same priority numbers from [`HookSource::priority`] feed the
//!    comparator.
//!
//! ## The seam
//!
//! Reading the merged settings and the session-hooks store, and
//! reading the registered-hook bootstrap state, are both out of
//! scope for this crate. The [`HookSourceProvider`] trait abstracts
//! both: the production caller wires it up to whichever crate owns
//! the settings store and the registered-hook bootstrap.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::event::{HookEvent, HOOK_EVENTS};
use crate::event_metadata::EventMetadataMap;
use crate::hook_command::{BashCommandHook, HookCommand};
use crate::hook_source::HookSource;
use crate::individual_hook::IndividualHookConfig;

/// Outer map keyed by event; inner map keyed by matcher (with empty
/// string as the wildcard key). [`BTreeMap`] is used for the inner
/// map so iteration order is deterministic regardless of insertion
/// order.
pub type HooksByEventAndMatcher = HashMap<HookEvent, BTreeMap<String, Vec<IndividualHookConfig>>>;

/// One entry from the registered-hooks bootstrap: either a plugin
/// hook or a builtin hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisteredHookEntry {
    Plugin {
        event: HookEvent,
        matcher: Option<String>,
        plugin_id: String,
        hooks: Vec<HookCommand>,
    },
    /// Internal-build builtin entry. Synthesises a `command`-type
    /// hook with the literal text `"[internal] Built-in Hook"` for
    /// every entry at grouping time. The provider just needs to
    /// surface the count.
    Builtin {
        event: HookEvent,
        matcher: Option<String>,
        hook_count: usize,
    },
}

/// The injected reader for everything outside the hooks module.
///
/// * `restricted_to_managed_only` reports whether the policy settings'
///   `allowManagedHooksOnly` flag is set.
/// * `hooks_from_settings` returns the hooks read from settings
///   (sans the policy-restricted short-circuit, which is exposed
///   separately).
/// * `registered_hooks` returns the bootstrap-registered hooks.
///   Caller decides whether to surface builtin hooks based on its
///   own internal-build knowledge.
pub trait HookSourceProvider {
    fn restricted_to_managed_only(&self) -> bool;
    fn hooks_from_settings(&self) -> Vec<IndividualHookConfig>;
    fn registered_hooks(&self) -> Vec<RegisteredHookEntry>;
}

fn matcher_key_for(event: HookEvent, matcher: Option<&str>, metadata: &EventMetadataMap) -> String {
    let supports = metadata
        .get(&event)
        .map(|m| m.matcher_metadata.is_some())
        .unwrap_or(false);
    if supports {
        matcher.unwrap_or("").to_string()
    } else {
        String::new()
    }
}

fn empty_grouped() -> HooksByEventAndMatcher {
    let mut grouped = HooksByEventAndMatcher::new();
    for event in HOOK_EVENTS {
        grouped.insert(event, BTreeMap::new());
    }
    grouped
}

/// Groups hooks by event and matcher. Pure function over the
/// injected provider and the metadata table. The
/// `restricted_to_managed_only` short-circuit is **applied here** —
/// when true, no settings hooks are emitted, but registered hooks
/// (plugin / builtin) are still emitted.
pub fn group_hooks_by_event_and_matcher<P: HookSourceProvider + ?Sized>(
    provider: &P,
    metadata: &EventMetadataMap,
) -> HooksByEventAndMatcher {
    let mut grouped = empty_grouped();

    if !provider.restricted_to_managed_only() {
        for hook in provider.hooks_from_settings() {
            let key = matcher_key_for(hook.event, hook.matcher.as_deref(), metadata);
            if let Some(group) = grouped.get_mut(&hook.event) {
                group.entry(key).or_default().push(hook);
            }
        }
    }

    for entry in provider.registered_hooks() {
        match entry {
            RegisteredHookEntry::Plugin {
                event,
                matcher,
                plugin_id,
                hooks,
            } => {
                let key = matcher.clone().unwrap_or_default();
                if let Some(group) = grouped.get_mut(&event) {
                    let bucket = group.entry(key).or_default();
                    for hook in hooks {
                        bucket.push(IndividualHookConfig {
                            event,
                            config: hook,
                            matcher: matcher.clone(),
                            source: HookSource::PluginHook,
                            plugin_name: Some(plugin_id.clone()),
                        });
                    }
                }
            }
            RegisteredHookEntry::Builtin {
                event,
                matcher,
                hook_count,
            } => {
                let key = matcher.clone().unwrap_or_default();
                if let Some(group) = grouped.get_mut(&event) {
                    let bucket = group.entry(key).or_default();
                    for _ in 0..hook_count {
                        bucket.push(IndividualHookConfig {
                            event,
                            config: HookCommand::Command(BashCommandHook {
                                command: "[internal] Built-in Hook".into(),
                                r#if: None,
                                shell: None,
                                timeout: None,
                                status_message: None,
                                once: None,
                                r#async: None,
                                async_rewake: None,
                            }),
                            matcher: matcher.clone(),
                            source: HookSource::BuiltinHook,
                            plugin_name: None,
                        });
                    }
                }
            }
        }
    }

    grouped
}

/// Returns the matchers for an event, sorted by source priority
/// then lexicographically.
pub fn sorted_matchers_for_event(
    grouped: &HooksByEventAndMatcher,
    event: HookEvent,
) -> Vec<String> {
    let Some(group) = grouped.get(&event) else {
        return Vec::new();
    };
    let mut matchers: Vec<String> = group.keys().cloned().collect();
    matchers.sort_by(|a, b| {
        let a_priority = highest_priority(group.get(a));
        let b_priority = highest_priority(group.get(b));
        a_priority.cmp(&b_priority).then_with(|| a.cmp(b))
    });
    matchers
}

fn highest_priority(hooks: Option<&Vec<IndividualHookConfig>>) -> i32 {
    let Some(hooks) = hooks else {
        // Empty bucket sorts as the lowest-priority sentinel so the
        // sort remains total. The bucket is only ever created lazily
        // on push, so this path is unreachable in practice.
        return crate::hook_source::PLUGIN_OR_BUILTIN_PRIORITY;
    };
    let unique_sources: HashSet<HookSource> = hooks.iter().map(|h| h.source).collect();
    unique_sources
        .into_iter()
        .map(|s| s.priority())
        .min()
        .unwrap_or(crate::hook_source::PLUGIN_OR_BUILTIN_PRIORITY)
}

/// Returns the hooks registered for a given event and matcher.
pub fn hooks_for_matcher<'a>(
    grouped: &'a HooksByEventAndMatcher,
    event: HookEvent,
    matcher: Option<&str>,
) -> &'a [IndividualHookConfig] {
    let key = matcher.unwrap_or("");
    grouped
        .get(&event)
        .and_then(|g| g.get(key))
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_metadata::{build_hook_event_metadata, MetadataInputs};
    use crate::hook_command::{BashCommandHook, HookCommand};

    fn meta() -> EventMetadataMap {
        build_hook_event_metadata(&MetadataInputs {
            tool_names: vec!["Bash".into(), "Read".into()],
            agent_types: vec![],
            elicitation_servers: vec![],
        })
    }

    fn cmd(command: &str) -> HookCommand {
        HookCommand::Command(BashCommandHook {
            command: command.into(),
            r#if: None,
            shell: None,
            timeout: None,
            status_message: None,
            once: None,
            r#async: None,
            async_rewake: None,
        })
    }

    fn make_hook(
        event: HookEvent,
        matcher: Option<&str>,
        source: HookSource,
        cmd: HookCommand,
    ) -> IndividualHookConfig {
        IndividualHookConfig {
            event,
            config: cmd,
            matcher: matcher.map(Into::into),
            source,
            plugin_name: None,
        }
    }

    /// Synthetic provider — the test seam for `HookSourceProvider`.
    struct MockProvider {
        restricted: bool,
        settings: Vec<IndividualHookConfig>,
        registered: Vec<RegisteredHookEntry>,
    }

    impl HookSourceProvider for MockProvider {
        fn restricted_to_managed_only(&self) -> bool {
            self.restricted
        }
        fn hooks_from_settings(&self) -> Vec<IndividualHookConfig> {
            self.settings.clone()
        }
        fn registered_hooks(&self) -> Vec<RegisteredHookEntry> {
            self.registered.clone()
        }
    }

    #[test]
    fn empty_provider_yields_28_empty_groups() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        assert_eq!(grouped.len(), 28);
        for group in grouped.values() {
            assert!(group.is_empty());
        }
    }

    #[test]
    fn single_settings_hook_groups_by_event_and_matcher() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![make_hook(
                HookEvent::PreToolUse,
                Some("Bash"),
                HookSource::ProjectSettings,
                cmd("ls"),
            )],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let pre = grouped.get(&HookEvent::PreToolUse).unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre.get("Bash").map(|v| v.len()), Some(1));
    }

    #[test]
    fn matcher_none_collapses_to_empty_string_key() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![make_hook(
                HookEvent::PreToolUse,
                None,
                HookSource::ProjectSettings,
                cmd("ls"),
            )],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let pre = grouped.get(&HookEvent::PreToolUse).unwrap();
        assert_eq!(pre.get("").map(|v| v.len()), Some(1));
    }

    #[test]
    fn event_without_matcher_metadata_collapses_to_empty_key() {
        // Stop has NO matcher metadata, so even a hook with a matcher
        // string set should be filed under "".
        let provider = MockProvider {
            restricted: false,
            settings: vec![make_hook(
                HookEvent::Stop,
                Some("ignored"),
                HookSource::UserSettings,
                cmd("cleanup.sh"),
            )],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let stop = grouped.get(&HookEvent::Stop).unwrap();
        assert_eq!(stop.get("").map(|v| v.len()), Some(1));
        assert!(stop.get("ignored").is_none());
    }

    #[test]
    fn restricted_to_managed_only_blocks_settings_hooks() {
        let provider = MockProvider {
            restricted: true,
            settings: vec![make_hook(
                HookEvent::PreToolUse,
                Some("Bash"),
                HookSource::UserSettings,
                cmd("ls"),
            )],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let pre = grouped.get(&HookEvent::PreToolUse).unwrap();
        assert!(pre.is_empty());
    }

    #[test]
    fn restricted_to_managed_only_still_emits_plugin_hooks() {
        let provider = MockProvider {
            restricted: true,
            settings: vec![],
            registered: vec![RegisteredHookEntry::Plugin {
                event: HookEvent::PreToolUse,
                matcher: Some("Bash".into()),
                plugin_id: "demo".into(),
                hooks: vec![cmd("plugin-cmd")],
            }],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let pre = grouped.get(&HookEvent::PreToolUse).unwrap();
        assert_eq!(pre.get("Bash").map(|v| v.len()), Some(1));
        assert_eq!(pre.get("Bash").unwrap()[0].source, HookSource::PluginHook);
        assert_eq!(
            pre.get("Bash").unwrap()[0].plugin_name.as_deref(),
            Some("demo")
        );
    }

    #[test]
    fn plugin_entry_tags_each_hook_with_plugin_name() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![],
            registered: vec![RegisteredHookEntry::Plugin {
                event: HookEvent::PostToolUse,
                matcher: None,
                plugin_id: "linter".into(),
                hooks: vec![cmd("a"), cmd("b"), cmd("c")],
            }],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let post = grouped.get(&HookEvent::PostToolUse).unwrap();
        let bucket = post.get("").unwrap();
        assert_eq!(bucket.len(), 3);
        for h in bucket {
            assert_eq!(h.source, HookSource::PluginHook);
            assert_eq!(h.plugin_name.as_deref(), Some("linter"));
        }
    }

    #[test]
    fn builtin_entry_synthesises_command_hooks() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![],
            registered: vec![RegisteredHookEntry::Builtin {
                event: HookEvent::PreToolUse,
                matcher: Some("Read".into()),
                hook_count: 2,
            }],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let pre = grouped.get(&HookEvent::PreToolUse).unwrap();
        let bucket = pre.get("Read").unwrap();
        assert_eq!(bucket.len(), 2);
        for h in bucket {
            assert_eq!(h.source, HookSource::BuiltinHook);
            // The builtin placeholder text is exact and literal.
            if let HookCommand::Command(c) = &h.config {
                assert_eq!(c.command, "[internal] Built-in Hook");
            } else {
                panic!("expected command-type hook");
            }
        }
    }

    #[test]
    fn registered_event_unknown_to_grouped_is_skipped() {
        // Defensive: a registered hook for an event we don't have in
        // the grouped map (impossible today since grouped is
        // initialised with every variant) — should be a no-op.
        let provider = MockProvider {
            restricted: false,
            settings: vec![],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        // Smoke check
        assert!(grouped.contains_key(&HookEvent::PreToolUse));
    }

    #[test]
    fn sorted_matchers_orders_by_priority_then_locale() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![
                make_hook(
                    HookEvent::PreToolUse,
                    Some("zMatch"),
                    HookSource::LocalSettings,
                    cmd("z"),
                ),
                make_hook(
                    HookEvent::PreToolUse,
                    Some("aMatch"),
                    HookSource::UserSettings,
                    cmd("a"),
                ),
                make_hook(
                    HookEvent::PreToolUse,
                    Some("mMatch"),
                    HookSource::ProjectSettings,
                    cmd("m"),
                ),
            ],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let sorted = sorted_matchers_for_event(&grouped, HookEvent::PreToolUse);
        // local (0) < project (1) < user (2)
        assert_eq!(sorted, vec!["zMatch", "mMatch", "aMatch"]);
    }

    #[test]
    fn sorted_matchers_locale_tiebreak_within_same_priority() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![
                make_hook(
                    HookEvent::PreToolUse,
                    Some("zebra"),
                    HookSource::ProjectSettings,
                    cmd("a"),
                ),
                make_hook(
                    HookEvent::PreToolUse,
                    Some("apple"),
                    HookSource::ProjectSettings,
                    cmd("b"),
                ),
                make_hook(
                    HookEvent::PreToolUse,
                    Some("monkey"),
                    HookSource::ProjectSettings,
                    cmd("c"),
                ),
            ],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let sorted = sorted_matchers_for_event(&grouped, HookEvent::PreToolUse);
        assert_eq!(sorted, vec!["apple", "monkey", "zebra"]);
    }

    #[test]
    fn sorted_matchers_plugin_sentinel_sinks_to_bottom() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![make_hook(
                HookEvent::PreToolUse,
                Some("aMatch"),
                HookSource::UserSettings,
                cmd("a"),
            )],
            registered: vec![RegisteredHookEntry::Plugin {
                event: HookEvent::PreToolUse,
                matcher: Some("zPlugin".into()),
                plugin_id: "demo".into(),
                hooks: vec![cmd("p")],
            }],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let sorted = sorted_matchers_for_event(&grouped, HookEvent::PreToolUse);
        // user (2) < plugin (999)
        assert_eq!(sorted, vec!["aMatch", "zPlugin"]);
    }

    #[test]
    fn sorted_matchers_returns_empty_for_event_with_no_hooks() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let sorted = sorted_matchers_for_event(&grouped, HookEvent::PreToolUse);
        assert!(sorted.is_empty());
    }

    #[test]
    fn hooks_for_matcher_returns_empty_slice_for_unknown_matcher() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![make_hook(
                HookEvent::PreToolUse,
                Some("Bash"),
                HookSource::UserSettings,
                cmd("ls"),
            )],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let hooks = hooks_for_matcher(&grouped, HookEvent::PreToolUse, Some("Read"));
        assert!(hooks.is_empty());
    }

    #[test]
    fn hooks_for_matcher_none_uses_empty_string_key() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![make_hook(
                HookEvent::Stop,
                None,
                HookSource::UserSettings,
                cmd("cleanup"),
            )],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let hooks = hooks_for_matcher(&grouped, HookEvent::Stop, None);
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn hooks_for_matcher_some_empty_string_equivalent_to_none() {
        let provider = MockProvider {
            restricted: false,
            settings: vec![make_hook(
                HookEvent::Stop,
                None,
                HookSource::UserSettings,
                cmd("cleanup"),
            )],
            registered: vec![],
        };
        let grouped = group_hooks_by_event_and_matcher(&provider, &meta());
        let via_none = hooks_for_matcher(&grouped, HookEvent::Stop, None);
        let via_empty = hooks_for_matcher(&grouped, HookEvent::Stop, Some(""));
        assert_eq!(via_none.len(), via_empty.len());
    }

    /// The table for the matcher-key collapse rule across
    /// every event with and without matcher metadata.
    #[test]
    fn matcher_key_collapse_table() {
        let metadata = meta();
        let table = [
            (HookEvent::PreToolUse, Some("Bash"), "Bash"),
            (HookEvent::PreToolUse, None, ""),
            (HookEvent::PreToolUse, Some(""), ""),
            (HookEvent::Stop, Some("ignored"), ""),
            (HookEvent::Stop, None, ""),
            (HookEvent::UserPromptSubmit, Some("ignored"), ""),
            (HookEvent::Notification, Some("idle_prompt"), "idle_prompt"),
            (HookEvent::TeammateIdle, Some("ignored"), ""),
            (HookEvent::WorktreeCreate, Some("ignored"), ""),
        ];
        for (event, matcher, expected_key) in table {
            let key = matcher_key_for(event, matcher, &metadata);
            assert_eq!(key, expected_key, "{event:?} / {matcher:?}");
        }
    }
}
