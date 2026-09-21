//! # rebon-hooks — hook configuration model and runtime
//!
//! Three things live here:
//!
//! * the hook data model — the `HookCommand` union (`command` / `prompt`
//!   / `agent` / `http`), `IndividualHookConfig`, hook sources, the
//!   canonical `HOOK_EVENTS` list, and the per-event metadata table;
//! * the hook runtime — matching, dedupe, dispatch, and output-protocol
//!   handling, with built-in command and HTTP executors;
//! * the settings-file loader the runtime reads hooks from, plus the
//!   status-line command that runs out of the same files.
//!
//! Every module pins its behaviour with a test table.
//!
//! ## What this crate is to the rest of rebon
//!
//! A policy-event subscriber, and nothing more: a caller wraps the
//! runtime in an adapter of its own and registers that with whatever
//! emits policy events, so no host calls [`runtime`] directly. What an
//! event *means* where it lands — whether a refusal blocks a tool, a
//! prompt, or a stop — belongs to the emitting site, not here. This
//! crate reports what the hooks said, as [`HookEffect`]s.
//!
//! [`grouping`] exists so the runtime can pick which hooks an event
//! fires; it is not a UI model.
//!
//! ## What is in this crate
//!
//! * [`event`] — `HookEvent` enum for the canonical `HOOK_EVENTS`
//!   list plus the `parse_hook_event` / `hook_event_name` round-trip.
//! * [`hook_source`] — `HookSource` enum plus its `header` label and
//!   the priority lookup used by `sorted_matchers_for_event`.
//! * [`hook_command`] — `HookCommand`, the
//!   `display_text` rule (`status_message` takes priority), and the
//!   `is_hook_equal` semantic identity.
//! * [`individual_hook`] — `IndividualHookConfig` and the
//!   `event_supports_matcher` / `matcher_or_all` / `matcher_key`
//!   helpers, which fix the label and the key form an absent matcher
//!   takes.
//! * [`event_metadata`] — pure `HookEventMetadata` table. Tool names
//!   and agent/elicitation value lists are injected as
//!   `MetadataInputs` so the table is a pure function of its input —
//!   no memoisation cache; consumers can layer one on top.
//! * [`grouping`] — `group_hooks_by_event_and_matcher`,
//!   `sorted_matchers_for_event` and `hooks_for_matcher`: how the
//!   runtime picks which hooks an event fires. The settings and
//!   registered-hook readers are **injected** as a `HookSourceProvider`
//!   trait, so this crate never reaches into a settings store itself.
//! * [`output_protocol`] — hook stdout/body protocol types and the
//!   output-parsing helpers. Owns sync-vs-async JSON output, prompt
//!   request/response, and permission-request wire shapes.
//! * [`runtime_result`] — runtime projection and aggregation helpers
//!   for hook JSON output plus the exit-code semantics around
//!   `0 / 2 / other`.
//!
//! ## Deliberately out of scope
//!
//! The following concerns are explicitly not owned by this crate;
//! each has a defined seam the consumer fills in:
//!
//! * **Settings-store reads.** This crate exposes the
//!   [`grouping::HookSourceProvider`] trait (`hooks_from_settings`,
//!   `registered_hooks`, `restricted_to_managed_only`). Production
//!   callers fill it in from whatever owns the settings store; tests use
//!   synthetic implementations. [`settings_provider`] is the shipped
//!   impl for callers that already hold a parsed snapshot.
//!
//!   ```rust,ignore
//!   pub trait HookSourceProvider {
//!       fn restricted_to_managed_only(&self) -> bool;
//!       fn hooks_from_settings(&self) -> Vec<IndividualHookConfig>;
//!       fn registered_hooks(&self) -> Vec<RegisteredHookEntry>;
//!   }
//!   ```
//!
//! * **Caching / memoisation.** The `group_hooks_by_event_and_matcher`
//!   output and the event-metadata table are rebuilt fresh per call —
//!   the consumer memoises if it wants to.
//! * **Prompt / agent transports** for running hooks. Execution goes
//!   through the [`executor::HookExecutor`] trait — the
//!   [`executor::DispatchExecutor`] picks one registered backend per
//!   hook variant. Command and HTTP backends ship in [`executors`];
//!   prompt and agent backends need the model client / worker spawner
//!   and are wired in by the host. Tests use synthetic
//!   [`executor::HookExecutor`] impls.
//! * **Filesystem reads of the merged settings view and plugin
//!   `hooks.json` files.** Hidden behind the `HookSourceProvider`
//!   seam — the production caller owns that I/O. ([`settings_loader`]
//!   provides a file-based loader for individual settings files.)
//! * **Schema-level parsing of a merged settings view.**
//!   [`settings_loader`] parses the three editable files into
//!   `IndividualHookConfig`; a caller that holds a merged, cached, or
//!   otherwise derived view owns its own authoritative parse.
//! * **The builtin-hook gate.** This crate models the
//!   `RegisteredHookEntry::Builtin` variant, but whether builtin hooks
//!   are surfaced at all is the provider's decision. Tests exercise
//!   both paths.

pub mod effects;
pub mod event;
pub mod event_metadata;
pub mod executor;
pub mod executors;
pub mod grouping;
pub mod hook_command;
pub mod hook_source;
pub mod individual_hook;
pub mod invocation;
pub mod output_protocol;
pub mod runtime;
pub mod runtime_result;
pub mod settings_loader;
pub mod settings_provider;
/// `settings.json#statusLine` and `appStatusLines`: loading the
/// config, running the command, and parsing what comes back.
///
/// It sits here rather than beside either of its readers because it is
/// the same thing a command hook is — same settings files, same
/// subprocess, same output to parse — differing only in that the product
/// is one line of text instead of a set of effects. How an ANSI span is
/// painted stays the reader's business; this module only produces spans.
pub mod status_line;

pub use effects::{project_effects, HookEffect};
pub use event::{parse_hook_event, HookEvent, HOOK_EVENTS};
pub use event_metadata::{
    build_hook_event_metadata, matcher_metadata_for_event, EventMetadataMap, HookEventMetadata,
    MatcherMetadata, MetadataInputs,
};
pub use executor::{
    DispatchExecutor, ExecutedHookResult, HookExecutionError, HookExecutor, HookRuntimeContext,
};
pub use executors::{CommandExecutor, HttpExecutor};
pub use grouping::{
    group_hooks_by_event_and_matcher, hooks_for_matcher, sorted_matchers_for_event,
    HookSourceProvider, HooksByEventAndMatcher, RegisteredHookEntry,
};
pub use hook_command::{
    display_text, is_hook_equal, AgentHook, BashCommandHook, HookCommand, HttpHook, PromptHook,
    ShellKind, DEFAULT_HOOK_SHELL,
};
pub use hook_source::{source_priority, HookSource, PLUGIN_OR_BUILTIN_PRIORITY};
pub use individual_hook::{
    event_supports_matcher, matcher_key, matcher_or_all, IndividualHookConfig,
};
pub use invocation::{HookEventPayload, HookInvocationContext, HookInvocationInput};
pub use output_protocol::{
    PromptRequest as HookPromptRequest, PromptRequestOption, PromptResponse as HookPromptResponse,
    SyncHookJsonOutput, ValidateHookJsonResult,
};
pub use runtime::{
    AllowAllIfEvaluator, HookExecutionErrorEntry, HookRuntime, HookRuntimeOptions,
    HookRuntimeOutput, HookValidationErrorEntry, IfEvaluator,
};
pub use runtime_result::{
    aggregate_hook_results, classify_exit_code, classify_plain_command_result,
    process_hook_json_output, AggregatedHookResult, ElicitationResponse, ExitCodeSemantic,
    HookBlockingError, HookResult, HookResultOutcome, ProcessHookJsonError,
};
pub use settings_loader::{
    load_all_editable, load_from_path as load_hooks_from_path, parse_settings_value, LoadedHooks,
    SettingsLoadError, SettingsLoadWarning, SettingsPaths,
};
pub use settings_provider::{SettingsHookProvider, SettingsSnapshot};
pub use status_line::{
    load_app_status_lines, parse_status_line_ansi, resolve_app_status_lines_values,
    run_status_line_command, run_status_line_command_with_timeout, AnsiColor, AnsiSpan, AnsiStyle,
    AppStatusLinePlacement, AppStatusLinesConfig, AppStatusLinesLoadError, StatusLineCommandConfig,
    StatusLineCommandErrorKind, StatusLineCommandFailure,
    DEFAULT_STATUS_LINE_REFRESH_INTERVAL_SECS, STATUS_LINE_MAX_LINES, STATUS_LINE_STDOUT_LIMIT,
    STATUS_LINE_TIMEOUT,
};
