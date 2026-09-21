//! Shared data types and utilities for rebon.
//!
//! This crate houses the content-block, tool-call, session-update, slash-command,
//! permission, and other wire-compatible structures used by several rebon crates.
//! Keeping them here avoids forcing those consumers into a dependency on the full
//! ACP transport layer.

mod app_visual_effect;
mod cancel;
mod constant_time;
pub mod effort_indicator;
pub mod env;
mod file_mention;
mod kernel_plugin;
mod paste_content;
mod reasoning_effort;
mod secure_token;
mod session_id;
pub mod sibling_binary;
mod sub_agent_model;
mod summary_envelope;
mod task_status;
mod text;
mod time_fmt;
mod types;
mod ultraplan_manifest;
mod ultraplan_plan;
mod ultraplan_run;

pub use app_visual_effect::*;
pub use cancel::PromptCancel;
pub use constant_time::constant_time_eq;
pub use file_mention::{
    FileMentionLocation, FileMentionQuery, FileMentionQueryError, MAX_FILE_MENTION_QUERY_BYTES,
};
pub use kernel_plugin::*;
pub use paste_content::PromptPasteContent;
pub use reasoning_effort::{ParseReasoningEffortError, ReasoningEffort};
pub use secure_token::secure_random_hex_token;
pub use session_id::new_session_id;
pub use sub_agent_model::{SubAgentModelConfig, SubAgentModelSelection};
pub use summary_envelope::{display_text_for_summary_envelope, summary_envelope_text};
pub use task_status::{ListTask, TaskListStatus, TaskStatus};
pub use text::{truncate_chars, xml_escape};
pub use time_fmt::{format_system_time_iso_ms, wall_clock_ms, wall_clock_ms_u128};
pub use types::*;
pub use ultraplan_manifest::*;
pub use ultraplan_plan::*;
pub use ultraplan_run::*;
