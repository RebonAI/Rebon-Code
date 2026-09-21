//! # rebon-dialog — pure dialog state machines
//!
//! This crate implements independent modal dialogs. Each dialog is
//! its own state machine, but every dialog fits the same shape: an
//! option list, a confirm/cancel handler, and a display projection.
//! Each dialog is its own module.
//!
//! ## Dialogs implemented
//!
//! * [`exit_flow`] — the goodbye message shown on exit.
//! * [`invalid_settings`] — exit/continue branch over a list of
//!   validation errors.
//! * [`dev_channels`] — three-way (latest/dev/cancel) channel chooser.
//! * [`mcp_server_approval`] — approve/reject/learn-more.
//! * [`history_search`] — text-input filter over a list of recent
//!   prompts with pagination.
//! * [`mcp_server_multiselect`] — toggle-list with all-on / all-off
//!   actions.
//! * [`invalid_config`] — multi-error config dialog with retry.
//! * [`quick_open`] — fuzzy file picker reducer.
//! * [`fuzzy_input`] — reusable query, selection, matching, and grouped-row
//!   helpers for fuzzy input dialogs.
//! * [`global_search`] — text-input reducer with paged result list.
//! * [`bridge_dialog`] — multi-step bridge wizard with input
//!   validation.
//! * [`doctor_dialog`] — scrolling diagnostics report with a rerun
//!   action.
//! * [`plugins_dialog`] — plugin list with enable / disable / uninstall,
//!   each emitted as a textual `/plugin` command.
//! * [`hooks_dialog`] — read-only two-pane browser over hook events and
//!   what is configured for each.
//! * [`settings_dialog`] — tabbed settings panel with a config list, a
//!   detail pane and in-place editing.
//!
//! ## The `DialogModel` seam
//!
//! [`model`] holds the trait every hosted dialog implements —
//! `on_key` / `view` / `is_fullscreen` — plus the declarative
//! `ViewSpec` its surfaces paint. [`host`] holds the stack those
//! models live on and the one key route into it. Both live here rather
//! than in a rendering crate, because a dialog model must not know
//! which surface is showing it.
//!
//! ## Test policy
//!
//! Every dialog module pins behaviour with a comprehensive
//! test table covering: every option, the cancel path, every input
//! validation rule (positive + negative), and any text projection /
//! formatting.
//!
//! ## Outbound seams
//!
//! Every outbound side-effect (settings write, graceful
//! shutdown, MCP wire call, etc.) is
//! modeled as an *action enum* the consumer routes. This crate never
//! performs IO, and its only rebon dependency is `rebon-customselect`,
//! which owns the option-list navigation state machine the pickers
//! drive.
//!
//! Common shapes:
//!
//! * **`SelectOption<V>`** — `{ label, value }` pair, matches the
//!   consumer's `Select` widget input.
//! * **`DialogAction`** — per-dialog enum the reducer emits.
//! * **`DialogEvent`** — keyboard / select event the reducer accepts.
//!
//! ## Deliberately out of scope
//!
//! * **All terminal rendering primitives** (`Box`, `Text`, `Dialog`,
//!   `Select`, `Link`, `Newline`, `Pane`). This crate produces plain
//!   `String`s, structs, and discriminated enums.
//! * **UI lifecycle plumbing** (effect/state/callback/memo hooks,
//!   keybinding registration, deferred values). Each dialog exposes a
//!   reducer + display projection instead.
//! * **Process shutdown** — modeled as an exit variant carrying the
//!   exit code.
//! * **The actual MCP wire call, file IO, network calls.** All
//!   deferred to the consumer.
//! * **Terminal size** — a value parameter (`columns: u16`).
//! * **`fzf`-style fuzzy match** in [`quick_open`] / [`global_search`]
//!   / [`history_search`] — modeled as a simple substring filter with
//!   a fuzzy subsequence fallback.
//!
//! ## Module map
//!
//! Each dialog has its own module to keep each test matrix easy to
//! grep.
//!
//! The settings panel is the one surface spread over four:
//! [`settings_dialog`] is the panel itself, and [`settings_status`],
//! [`settings_tabs`] and [`settings_usage`] are the projections its
//! three built-in tabs are made of. The split holds because the tab
//! projections carry no dialog state and are read on their own.

#![deny(missing_docs)]

pub mod bridge_dialog;
pub mod common;
pub mod context_dialog;
pub mod dev_channels;
pub mod doctor_dialog;
pub mod effort_dialog;
pub mod exit_flow;
pub mod fuzzy_input;
pub mod global_search;
pub mod history_search;
pub mod history_search_dialog;
pub mod hooks_dialog;
pub mod host;
pub mod invalid_config;
pub mod invalid_settings;
pub mod mcp_server_approval;
pub mod mcp_server_multiselect;
pub mod model;
pub mod model_dialog;
pub mod plugins_dialog;
pub mod provider_dialog;
pub mod quick_open;
pub mod quick_open_dialog;
pub mod settings_dialog;
pub mod settings_status;
pub mod settings_tabs;
pub mod settings_usage;
