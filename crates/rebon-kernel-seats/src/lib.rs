//! The Core seats a rebon process always boots with.
//!
//! A seat here changes when the *contract*
//! between the kernel and whoever fills it changes — a new tool fact, a dialog
//! that grew a field, a settings key. It does not change when the list of
//! plugins changes: that list is the process bootstrap's, which is
//! also where each of these plugins is registered.
//!
//! - [`kernel_core_tools`] / [`kernel_core_ui`] / [`kernel_config_seats`] — the
//!   three seats the kernel has always booted with: tools, dialogs, and the
//!   settings/credentials pair.
//! - [`kernel_services`] — the per-session scopes: which seats a session sees,
//!   in which namespace, and for how long.
//! - [`kernel_session_seat`] — dsh-session's append/deriveMessages face over the
//!   authoritative transcript.
//! - [`kernel_prompt_sections`] — the system-prompt seat.
//! - [`kernel_web_seat`] — `WebSearch` / `WebFetch` behind one router.
//! - [`kernel_compose_tools`] / [`kernel_tool_dispatch`] — the process-level
//!   registration face for composition-hosted tools, and the merge that lets a
//!   session see them.
//! - [`kernel_code_mode`] — the `run_code` tool.
//! - [`kernel_core_commands`] — the `command-registry` seat and the built-in
//!   slash commands registered on it.
//! - [`kernel_tool_asks`] — the suspend-and-answer surface a composed plugin's
//!   ungranted tool call parks on.
//! - [`kernel_tool_invoke`] — the engine-backed invoker behind that seat.
//!
//! Each of the last three is either handed a kernel by its caller or reads the
//! process slot (`rebon_kernel::process`) and fails closed when nothing has
//! booted. Nothing in this crate boots one.

pub mod kernel_code_mode;
pub mod kernel_compose_tools;
pub mod kernel_config_seats;
pub mod kernel_core_commands;
pub mod kernel_core_tools;
pub mod kernel_core_ui;
pub mod kernel_prompt_sections;
pub mod kernel_services;
pub mod kernel_session_seat;
pub mod kernel_tool_asks;
pub mod kernel_tool_dispatch;
pub mod kernel_tool_invoke;
pub mod kernel_web_seat;
