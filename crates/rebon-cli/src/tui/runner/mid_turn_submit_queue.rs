use crate::tui::app::AppState;

/// Put back messages a steer took off screen and then failed to
/// deliver.
///
/// Withdrawing a message that is mid-steer reports "already consumed"
/// — nobody can know yet whether the agent got it — so the row leaves
/// the queue. When the steer then fails, the message is owed to the
/// user again and comes back here. Empty on every ordinary frame.
fn restore_returned_steer_submits(app: &mut AppState) {
    let Some(poller) = app.mid_turn_queued_submit_poller.as_ref() else {
        return;
    };
    let returned = poller.drain_returned_submits();
    for submit in returned {
        crate::tui::dispatch::requeue_submit_payload_front(app, submit, "prompt".to_string());
    }
}

pub(super) fn reconcile_mid_turn_consumed_queued_submits(app: &mut AppState) {
    // Restoring first keeps the two directions from racing: a message
    // that came back must not then be dropped by a stale consumed id.
    restore_returned_steer_submits(app);
    let Some(poller) = app.mid_turn_queued_submit_poller.as_ref() else {
        return;
    };
    let consumed = poller.drain_consumed_user_message_uuids();
    if consumed.is_empty() {
        return;
    }

    let mut text_index = 0usize;
    app.queued_commands.retain(|cmd| match &cmd.value {
        rebon_tui::promptinput::QueuedCommandValue::Text(_) => {
            let remove = app
                .queued_submit_payloads
                .get(text_index)
                .and_then(|submit| submit.user_message_uuid.as_deref())
                .is_some_and(|uuid| consumed.contains(uuid));
            text_index = text_index.saturating_add(1);
            !remove
        }
        rebon_tui::promptinput::QueuedCommandValue::NonText => true,
    });
    app.queued_submit_payloads.retain(|submit| {
        !submit
            .user_message_uuid
            .as_deref()
            .is_some_and(|uuid| consumed.contains(uuid))
    });
    if app.queued_commands.is_empty() && app.queued_submit_payloads.is_empty() {
        app.queued_auto_drain_paused_after_withdrawal = false;
    }
}

#[cfg(test)]
mod tests {
    use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};

    use crate::session::submit_payload::SubmitPayload;
    use crate::tui::app::AppState;
    use crate::tui::dispatch::{enqueue_submit_payload, queued_text};

    use super::reconcile_mid_turn_consumed_queued_submits;

    #[test]
    fn reconcile_mid_turn_consumed_queued_submits_removes_consumed_items_only() {
        let mut app = AppState::new();
        let poller = crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller::new("sess-1");
        app.mid_turn_queued_submit_poller = Some(poller.clone());
        app.is_loading = true;
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "first".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "second".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let first_uuid = app.queued_submit_payloads[0]
            .user_message_uuid
            .clone()
            .expect("first uuid");

        let injected = poller.poll(AttachmentPollRequest::new(
            "sess-1",
            "turn-1",
            1,
            AttachmentPollPhase::Regular,
        ));
        assert_eq!(injected.len(), 2);
        poller.remove_user_message_uuid(
            app.queued_submit_payloads[1]
                .user_message_uuid
                .as_deref()
                .expect("second uuid"),
        );
        reconcile_mid_turn_consumed_queued_submits(&mut app);

        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(app.queued_submit_payloads[0].text, "second");
        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(queued_text(&app.queued_commands[0]), Some("second"));
        assert_ne!(
            app.queued_submit_payloads[0].user_message_uuid.as_deref(),
            Some(first_uuid.as_str())
        );
    }
}
