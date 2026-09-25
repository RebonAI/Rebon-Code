//! In-memory session state, independent of any wire protocol.
//!
//! Two things live here:
//!
//! - [`session`] — the session table (`ServerState`), the per-session
//!   record, the attachment state a turn consults (plan mode, skills,
//!   task reminders, runtime prompts), and the replay/transcript
//!   handover protocol.
//! - [`tool_output`] — pure projections from a raw tool-output JSON value
//!   into the structured tool-call content a client renders. It is
//!   re-exported from `rebon-render`: it is a projection, it reaches no
//!   further than `rebon-types` and `serde_json`, and the replay producer
//!   that needs it must not drag this crate's session table into the
//!   projection crate. The path stays so
//!   `rebon_session_state::tool_output::…` keeps resolving.
//!
//! Neither is ACP-specific: the ACP server drives them from one side and
//! the engine drives them from the other. This crate is that shared
//! middle, sitting above `rebon-session` / `rebon-permissions` /
//! `rebon-proto`.
//!
//! `ServerState` is a leftover name from when this only ever backed the
//! ACP server; renaming it is a separate change.

pub mod session;
pub use rebon_render::tool_output;

pub use session::{
    apply_plan_mode_transition_flags, resolve_session_cwd, PromptGeneration, PromptSessionSnapshot,
    PermissionModePublisher, ReplayFinalizeOutcome, ReplayTranscriptSource, ServerState,
    SessionAttachmentState, SessionOwner, SessionPermissionModeSource, SessionRecord,
    TranscriptSweep,
};
pub use tool_output::{
    extract_locations, tool_result_update_content, trim_raw_output_for_transcript,
};
