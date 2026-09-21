//! Tests for the worker host and the IPC server.
//!
//! They came from `rebon-cli`'s `background/tests`, which is
//! also where the split was decided: these drive a worker turn or the server,
//! and the ones that drive the binary's mirror half stayed behind.

mod support;

mod acp_control_plane;
mod agent_runtime;
mod cancel_call;
mod ipc_commands;
mod owner_descriptor_gate;
mod pending_prompt;
mod permissions_questions;
mod supervisor_process_reaper;
mod task_bridge;
mod task_bridge_worker;
mod worker_finalization;
