//! The local TUI event loop: submit, queue, cancel,
//! inline permission overlay, transcript/update draining, status bar,
//! and animated spinner.
//!
//! ## Remaining extraction candidates
//!
//! No in-crate clusters are left to extract — each one is already its
//! own submodule. New candidates can be added here as they're identified;
//! rg by name when picking one up.
//!
//! True out-of-crate candidates (post-submodule split): `inline_banner`
//! is a pure ratatui widget — eligible to move into rebon-tui.
//! `transcript_replay` has already gone: the entry-to-row projection lives
//! in `crate::session::transcript_replay`, where a caller with no screen
//! reads the rows, and what stays here is the shell that commits them.

mod active_prompt;
mod agent_view;
mod background_tasks;
pub(crate) mod commands;
pub(crate) mod compact_runtime;
// Windows-only: batched console input reads for pasted runs. See the
// module docs — crossterm reads one console record per syscall, which is
// what makes a large non-bracketed paste stall the prompt.
#[cfg(windows)]
mod console_input_batch;
mod context_prompts;
pub(crate) mod custom_status_line;
mod dialog_keys;
mod event_loop_entry;
mod footer_navigation;
mod foreground_agent_submit;
mod foreground_mailbox;
mod global_search;
mod goal_continuation;
mod inline_banner;
mod inline_commit_cursor;
mod input_latency_probe;
mod interrupt_flow;
mod key_actions;
mod layout_and_scroll;
mod live_agent_view;
mod local_agent_continuation;
mod mid_turn_submit_queue;
mod native_commands;
mod onboarding_hooks;
mod paste_burst;
mod paste_burst_pipeline;
pub(crate) mod paste_echo;
pub(crate) mod permission_flow;
mod permission_mode;
mod profile_command;
mod profile_proposal;
mod prompt_history;
mod prompt_lifecycle;
mod quick_open;
mod remote_background_attachment;
mod remote_session_option;
mod render;
mod resume_runtime;
mod resume_selection;
pub(crate) mod rewind;
mod run_blocking_entry;
mod runtime_refresh;
mod session_detach_attach;
mod session_slot;
mod slash_commands;
mod startup_submit;
mod status_bar;
mod steer_pump;
mod submit;
pub(crate) mod task_runtime;
mod team_messages;
mod terminal_capabilities;
#[cfg(test)]
#[path = "test_support_tests.rs"]
pub(crate) mod test_support;
pub(crate) mod title;
mod transcript_messages;
pub(crate) mod transcript_replay;
mod ultraplan;
mod updater_ui;

pub(in crate::tui::runner) use self::active_prompt::{
    ActivePrompt, LocalTurnSource, PromptResultRx, WithdrawDestination, WithdrawableSubmit,
};
use self::commands::apply_new_session;
use self::context_prompts::drain_model_context_prompts;
use self::global_search::apply_global_search_action;
use self::goal_continuation::{commit_goal_continuation_feedback, maybe_prepare_goal_continuation};
use self::layout_and_scroll::{
    build_scroll_snapshot, repin_transcript_to_bottom, reset_transcript_page_state,
    sync_follow_tail_after_updates, to_message_lite, visual_lines_with_cursor,
};
use self::live_agent_view::{
    drain_main_agent_updates, pause_preserve_task, switch_to_live_agent, switch_to_main_agent,
    sync_foreground_agent_view, with_main_agent_view,
};
use self::local_agent_continuation::{
    interrupt_and_continue_local_agent_task, parse_agent_interrupt_redirect,
};
use self::mid_turn_submit_queue::reconcile_mid_turn_consumed_queued_submits;
use self::prompt_lifecycle::admit_active_prompt;
use self::quick_open::apply_quick_open_action;
pub(crate) use self::render::InlineTailMeasureSlot;
use self::resume_selection::{apply_background_attach_target, apply_handover_attach_target};
use self::rewind::apply_rewind_outcome;
pub use self::run_blocking_entry::run_blocking;
use self::session_detach_attach::handle_agent_view_outcome;
pub(crate) use self::session_slot::{SessionSlot, StartupPreview};
pub(crate) use self::status_bar::stale_resume_warning;
pub(super) use self::status_bar::StatusBarInfo;
use self::task_runtime::spawn_agent_generation;
pub(crate) use self::transcript_messages::{
    apply_context_reset_to_tui, inject_local_command_feedback, inject_plan_card,
    inject_system_message,
};
pub(crate) use rebon_session_host::HostedStartup;
