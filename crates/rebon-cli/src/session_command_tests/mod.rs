//! Tests for the session runtime's commands that need a terminal.
//!
//! These tests stay with the binary rather than in `rebon-session-runtime`
//! because they build an `AppState`,
//! call `crate::session_shell::session_command_inputs_from_app`, or drive the TUI reducer.
//! Splitting the genuinely terminal-free ones back down is follow-up work.

mod commands_context;
mod commands_control;
mod commands_cost;
mod commands_doctor;
mod commands_mcp;
mod commands_permissions;
mod ultraplan_preflight;
mod ultraplan_review;
