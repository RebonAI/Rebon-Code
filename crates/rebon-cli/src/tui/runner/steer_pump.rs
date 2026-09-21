//! Feeding mid-turn messages to an agent that is already working.
//!
//! When a turn is running and the user types another message, the
//! local engine picks it up from
//! [`MidTurnQueuedSubmitPoller`](crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller)
//! between tool rounds — the message reaches the model without waiting
//! for the turn to end. A session running on an external agent has the
//! same queue but nobody reading it: the engine loop that polls is not
//! the loop doing the work.
//!
//! This is that reader. While an ACP turn is in flight it takes queued
//! messages and steers them into the running turn, then echoes them
//! into the transcript view the same way the local leg does, so the
//! two legs behave alike from the user's side.
//!
//! Everything here degrades to the old behaviour. An agent that never
//! advertised steering, an agent that dropped its connection, a turn
//! that ended first — each leaves the message in the queue, where the
//! end-of-turn drain sends it as an ordinary next prompt.

use std::sync::Arc;
use std::time::Duration;

use rebon_agent_core::{AgentBackendError, SessionUpdatePublisher, SteerOutcome};

use crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller;
use crate::session::submit_payload::SubmitPayload;
use crate::tui::runner::active_prompt::SteerPumpHandle;

/// How often the pump looks for a message to steer. Short enough that
/// typing during a turn feels immediate, long enough to be invisible
/// next to the model's own latency.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Start the pump for a turn about to run on `agents`' current agent.
///
/// Returns `None` — and starts nothing — when the session is on the
/// local engine, which polls the same queue from inside its own loop.
pub(super) fn spawn(
    handle: &tokio::runtime::Handle,
    agents: Arc<rebon_agent_core::routing::SessionAgents<rebon_acp_client::AcpAgentBackend>>,
    poller: Arc<MidTurnQueuedSubmitPoller>,
    update_publisher: Arc<dyn SessionUpdatePublisher>,
    session_id: String,
) -> Option<SteerPumpHandle> {
    if agents.current_id() == rebon_config::LOCAL_AGENT_ID {
        return None;
    }
    let task = handle.spawn(run(agents, poller.clone(), update_publisher, session_id));
    Some(SteerPumpHandle::new(task, poller))
}

async fn run(
    agents: Arc<rebon_agent_core::routing::SessionAgents<rebon_acp_client::AcpAgentBackend>>,
    poller: Arc<MidTurnQueuedSubmitPoller>,
    update_publisher: Arc<dyn SessionUpdatePublisher>,
    session_id: String,
) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let Some(submit) = poller.take_for_steer() else {
            continue;
        };
        let Some(uuid) = submit.user_message_uuid.clone() else {
            // Nothing to key an echo or a de-dup on; leave it to the
            // end-of-turn drain.
            poller.steer_failed(submit);
            continue;
        };

        match agents
            .steer(&session_id, steer_blocks(&submit), &uuid)
            .await
        {
            Ok(SteerOutcome::Injected) => {
                poller.steer_delivered(&submit);
                echo(&update_publisher, &session_id, &submit, &uuid).await;
            }
            Ok(SteerOutcome::TurnAlreadyOver) => {
                // The turn finished underneath us. The message is
                // still owed to the user; the queue drain that follows
                // a finished turn sends it as the next prompt.
                poller.steer_failed(submit);
                return;
            }
            Err(AgentBackendError::Unsupported(reason)) => {
                // Nothing about this turn will change that, so stop
                // polling instead of retrying every tick.
                tracing::debug!(%reason, "rebon: steering unavailable; queueing instead");
                poller.steer_failed(submit);
                return;
            }
            Err(err) => {
                tracing::debug!(error = %err, "rebon: could not steer; queueing instead");
                poller.steer_failed(submit);
                return;
            }
        }
    }
}

fn steer_blocks(submit: &SubmitPayload) -> Vec<rebon_types::ContentBlock> {
    let mut blocks = vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
        // `model_text` is the expanded form (directory listings and
        // the like) the model should read; `text` is what the user
        // typed. Prefer the former when they differ, matching what the
        // local leg sends.
        text: submit
            .model_text
            .clone()
            .unwrap_or_else(|| submit.text.clone()),
        annotations: None,
    })];
    for image in &submit.image_pastes {
        blocks.push(rebon_types::ContentBlock::Image(
            rebon_types::ImageContent {
                mime_type: image
                    .media_type
                    .clone()
                    .unwrap_or_else(|| String::from("image/png")),
                data: image.content.clone(),
                uri: None,
                annotations: None,
            },
        ));
    }
    blocks
}

/// Put the steered message on screen, through the same update the
/// local engine publishes when it injects one — so the transcript
/// looks identical whichever leg ran the turn.
async fn echo(
    publisher: &Arc<dyn SessionUpdatePublisher>,
    session_id: &str,
    submit: &SubmitPayload,
    uuid: &str,
) {
    let image_paste_ids: Vec<u32> = submit.image_pastes.iter().map(|image| image.id).collect();
    publisher
        .publish_to(
            &session_id.to_string(),
            rebon_types::SessionUpdate::QueuedUserMessage {
                uuid: uuid.to_string(),
                content: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: submit.text.clone(),
                    annotations: None,
                })],
                image_paste_ids: (!image_paste_ids.is_empty()).then_some(image_paste_ids),
            },
        )
        .await;
}
