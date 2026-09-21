//! Agent Client Protocol (ACP) **server**.
//!
//! This crate is the side of ACP where somebody else drives Rebon: an editor /
//! IDE connects over stdio and speaks JSON-RPC 2.0 at us. The wire layer it
//! speaks lives in [`rebon_proto`]; the session state it mutates lives in
//! [`rebon_session`]. What is left here is the server proper:
//!
//! - [`server`] — JSON-RPC dispatch loop that classifies inbound messages,
//!   routes requests through a [`server::RequestHandler`], and writes
//!   framed responses back. Ships with a [`server::DefaultHandler`] that
//!   answers `initialize`, `session/new`, `session/load`, `session/list`,
//!   `session/prompt` and `session/set_config_option` (plus the internal
//!   `_session/steering`), enforcing the initialize-first ordering, and
//!   returns method-not-found for anything else.
//!
//! The session map that `session/new` mints into, and the tool-result
//! view-model shared with the transcript, moved out to
//! [`rebon_session_state`] — neither was ACP-specific, and keeping them
//! here forced every consumer to depend on this server crate. Both are
//! re-exported below under their old paths.
//!
//! The prompt-turn seam and the outbound update/permission sinks are
//! [`rebon_agent_core`]'s, not this crate's: the ACP *client* (Rebon spawning
//! a third-party agent CLI) runs turns and publishes updates through exactly
//! the same types, and must not depend on the server to do it.
//!
//! Beyond the session-state re-exports named above, nothing upstream is
//! re-exported from here, on purpose — `rebon-proto`,
//! `rebon-agent-core`, and `rebon-session` are all imported directly by
//! whoever needs them. Transcripts were never an ACP concern to begin with.

pub mod server;

/// The session table and the tool-output projections now live in
/// [`rebon_session_state`]; they were never ACP-specific, and the engine
/// had to depend on this crate to reach them. Re-exported here verbatim so
/// every existing `rebon_acp::…` path keeps resolving.
pub use rebon_session_state::{session, tool_output};

pub use rebon_session_state::session::{
    apply_plan_mode_transition_flags, resolve_session_cwd, PromptGeneration, PromptSessionSnapshot,
    ReplayFinalizeOutcome, ReplayTranscriptSource, ServerState, SessionAttachmentState,
    SessionOwner, SessionRecord, TranscriptSweep,
};
pub use rebon_session_state::tool_output::{
    extract_locations, tool_result_update_content, trim_raw_output_for_transcript,
};
pub use server::{
    acp_prompt_with_ultrawork_reminder, serve, serve_with_publisher, serve_with_publishers,
    starts_with_ultrawork_command, DefaultHandler, RequestHandler, SessionConfigOptions,
    SteeringMessage, SteeringSink, ACP_PROTOCOL_VERSION,
};

use rebon_proto::StdioReader;

/// Version string reported by the ACP runtime.
pub const ACP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Drive the ACP runtime against an empty stdin stream.
///
/// Retained as a smoke-test entrypoint: it exercises the transport
/// reader end-to-end without blocking on a real stdin TTY when no ACP
/// client is connected. The engine no longer calls this — it now uses
/// [`run_server`] — but tests still rely on the cheap "wire it up over
/// `tokio::io::empty()`" path.
pub async fn run_empty() -> anyhow::Result<()> {
    tracing::info!(version = ACP_VERSION, "rebon-acp run_empty entered");
    let mut reader = StdioReader::new(tokio::io::empty());
    let msg = reader.read_message().await?;
    tracing::info!(
        framing = %reader.framing(),
        got_message = msg.is_some(),
        "rebon-acp run_empty exiting"
    );
    Ok(())
}

/// Run the ACP server over real stdio with the [`DefaultHandler`].
///
/// This is the production entrypoint: it binds to `tokio::io::stdin()`
/// and `tokio::io::stdout()`, instantiates a [`DefaultHandler`], and
/// drives [`serve`] until stdin returns EOF (typically when the ACP
/// client disconnects).
///
/// The server answers `initialize`, `session/new`, `session/load`,
/// `session/list`, `session/prompt` and `session/set_config_option` (plus
/// the internal `_session/steering`), enforcing that `initialize` must
/// succeed first, and returns method-not-found for anything else.
/// `session/cancel` is the one notification it acts on; unknown
/// notifications are silently ignored.
pub async fn run_server() -> anyhow::Result<()> {
    tracing::info!(version = ACP_VERSION, "rebon-acp server starting on stdio");
    let result = serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        DefaultHandler::default(),
    )
    .await;
    tracing::info!(ok = result.is_ok(), "rebon-acp server exiting");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_empty_returns_ok_on_empty_stdin() {
        run_empty().await.unwrap();
    }

    /// `run_server` binds to real stdin/stdout, which would hang in a unit
    /// test. Drive the same underlying server loop with an empty in-memory
    /// transport instead so the wiring it uses is still exercised.
    #[tokio::test]
    async fn serve_loop_exits_cleanly_on_empty_input() {
        serve(
            tokio::io::empty(),
            tokio::io::sink(),
            DefaultHandler::default(),
        )
        .await
        .unwrap();
    }

    #[test]
    fn server_surface_is_reachable_from_the_crate_root() {
        // The wire types come from `rebon-proto` and the turn seam
        // from `rebon-agent-core`; what this crate must keep exporting
        // is the server surface.
        let _handler = DefaultHandler::default();
        let _state = ServerState::default();
    }
}
