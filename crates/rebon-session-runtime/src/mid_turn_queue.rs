use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use rebon_api::{ContentBlock as ApiContentBlock, Message as ApiMessage, TextBlock};
use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};

use crate::submit_payload::{
    ensure_submit_payload_user_uuid, submit_payload_to_api_message, SubmitPayload,
};

const LOCAL_QUEUED_MODEL_TEXT_MARKER_PREFIX: &str = "<rebon-queued-user-model-text>\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedSubmitWithdrawal {
    PendingRemoved,
    AlreadyConsumed,
    NotTracked,
}

#[derive(Debug)]
pub struct MidTurnQueuedSubmitPoller {
    session_id: String,
    queue: Mutex<VecDeque<SubmitPayload>>,
    consumed_user_message_uuids: Mutex<HashSet<String>>,
    /// Messages handed to a steer that has not answered yet, keyed by
    /// their uuid and holding the payload so every exit path can put
    /// the text back.
    ///
    /// The local engine consumes a queued message synchronously, so it
    /// can mark it consumed the moment it takes it. Steering an
    /// external agent is a round trip that can fail — or whose pump can
    /// be aborted mid-await — so the message has to sit in a third
    /// state: neither queued (it must not be sent twice) nor consumed
    /// (a failure has to put it back). Whoever removes an entry from
    /// this map first owns the message; every racing party
    /// ([`Self::steer_delivered`], [`Self::steer_failed`],
    /// [`Self::reclaim_in_flight`], [`Self::remove_submit`]) checks the
    /// removal result before acting, which is what makes those races
    /// safe.
    in_flight: Mutex<HashMap<String, SubmitPayload>>,
    /// Withdrawn while in flight. The UI already dropped the text, so
    /// a late failure must not resurrect it.
    withdrawn_in_flight_uuids: Mutex<HashSet<String>>,
    /// Failed steers whose messages belong back in the UI queue. The
    /// runner drains this each frame — see `drain_returned_submits`.
    returned: Mutex<Vec<SubmitPayload>>,
    /// Held across foreground execution, including detached/canceling turns.
    /// Idle replay writes take the same lock before choosing their parent.
    active_turns: Mutex<usize>,
}

pub struct QueuedSubmitTurnGuard(Arc<MidTurnQueuedSubmitPoller>);

impl Drop for QueuedSubmitTurnGuard {
    fn drop(&mut self) {
        *self
            .0
            .active_turns
            .lock()
            .expect("queued submit turns poisoned") -= 1;
    }
}

impl MidTurnQueuedSubmitPoller {
    pub fn new(session_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            session_id: session_id.into(),
            queue: Mutex::new(VecDeque::new()),
            consumed_user_message_uuids: Mutex::new(HashSet::new()),
            in_flight: Mutex::new(HashMap::new()),
            withdrawn_in_flight_uuids: Mutex::new(HashSet::new()),
            returned: Mutex::new(Vec::new()),
            active_turns: Mutex::new(0),
        })
    }

    /// Acquire before spawning a foreground executor; drop only after it exits.
    pub fn begin_turn(self: &Arc<Self>) -> QueuedSubmitTurnGuard {
        *self
            .active_turns
            .lock()
            .expect("queued submit turns poisoned") += 1;
        QueuedSubmitTurnGuard(self.clone())
    }

    /// Save feedback not consumed by attachment injection for the next replay.
    /// Returns false while a foreground writer still owns the transcript. The
    /// caller retains the payload until this succeeds; its UUID is the receipt,
    /// not the transient consumed set (which the UI drains independently).
    pub fn persist_idle_submit(
        &self,
        projects_root: &std::path::Path,
        cwd: &str,
        state: &rebon_acp::ServerState,
        submit: &SubmitPayload,
    ) -> anyhow::Result<bool> {
        use rebon_session::session_storage::{
            append_transcript_entry, load_transcript_from_file, transcript_file_path,
            TranscriptWriteEntry,
        };
        let turns = self
            .active_turns
            .lock()
            .expect("queued submit turns poisoned");
        if *turns != 0 || state.is_prompt_active(&self.session_id) {
            return Ok(false);
        }
        let uuid = submit
            .user_message_uuid
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("durable feedback requires a user message UUID"))?;
        anyhow::ensure!(
            state.push_transcript_entries(&self.session_id, Vec::new()),
            "shell feedback session no longer exists"
        );
        let path = transcript_file_path(projects_root, cwd, &self.session_id);
        let transcript = load_transcript_from_file(&path)?;
        let entries = transcript
            .as_ref()
            .map(|t| t.messages.as_slice())
            .unwrap_or_default();
        if !entries.iter().any(|entry| entry.uuid == uuid) {
            let mut payload = serde_json::json!({
                "message": submit_payload_to_api_message(submit),
                "isMeta": uuid.starts_with("u-internal-"),
                "queuedCommand": true,
            });
            if let Some(model_text) = &submit.model_text {
                let mut model_submit = submit.clone();
                model_submit.text.clone_from(model_text);
                payload["modelContent"] =
                    serde_json::to_value(submit_payload_to_api_message(&model_submit).content)?;
            }
            let mut entry = TranscriptWriteEntry::new("user", payload).with_uuid(uuid);
            // Choose the tail NOW, never the tail from when the shell started.
            entry.parent_uuid = entries.last().map(|entry| entry.uuid.clone());
            let written = append_transcript_entry(projects_root, cwd, &self.session_id, entry)?;
            anyhow::ensure!(
                state.push_transcript_entries(&self.session_id, vec![written]),
                "shell feedback session no longer exists"
            );
        }
        self.remove_submit(submit);
        Ok(true)
    }

    /// Take the next queued message to steer into a running turn.
    ///
    /// The message leaves the queue but is *not* consumed yet: the
    /// caller must report back through [`Self::steer_delivered`] or
    /// [`Self::steer_failed`], or the pump's drop must
    /// [`Self::reclaim_in_flight`]. Every returned submit carries a
    /// uuid — `enqueue_submit` keys everything it accepts, and a keyless
    /// straggler goes back to the queue for the end-of-turn drain
    /// rather than leaving with no way to report back.
    pub fn take_for_steer(&self) -> Option<SubmitPayload> {
        let submit = {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let submit = queue.pop_front()?;
            if submit.user_message_uuid.is_none() {
                queue.push_front(submit);
                return None;
            }
            submit
        };
        let uuid = submit
            .user_message_uuid
            .clone()
            .expect("checked above while holding the queue lock");
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(uuid, submit.clone());
        Some(submit)
    }

    /// The agent took the message: it is now part of the turn, and the
    /// turn-end reconcile should drop it from the UI queue.
    ///
    /// If the in-flight entry is already gone, a reclaim got there
    /// first and put the message back in the queue — but the agent
    /// *did* receive it, so the requeued copy is removed again to keep
    /// it from being sent twice.
    pub fn steer_delivered(&self, submit: &SubmitPayload) {
        let Some(uuid) = submit.user_message_uuid.as_deref() else {
            return;
        };
        let owned = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(uuid)
            .is_some();
        {
            let mut withdrawn = self
                .withdrawn_in_flight_uuids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            withdrawn.remove(uuid);
        }
        if !owned {
            let requeued_copy_removed = self
                .queue
                .lock()
                .map(|mut queue| {
                    let before = queue.len();
                    queue.retain(|queued| queued.user_message_uuid.as_deref() != Some(uuid));
                    queue.len() != before
                })
                .unwrap_or(false);
            if !requeued_copy_removed {
                // Someone else (a drain's remove, or /clear) already
                // claimed the message outright; nothing left to mark.
                return;
            }
        }
        {
            let mut consumed = self
                .consumed_user_message_uuids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            consumed.insert(uuid.to_string());
        }
    }

    /// The steer did not land, so the message still owes the user a
    /// send. It goes back to the front of the queue, where the pump
    /// retries it and the turn-end drain sends it regardless.
    ///
    /// If the in-flight entry is already gone, the message was claimed
    /// while the steer was failing — reclaimed to the queue by the
    /// pump's drop, or removed by a drain that sent it as a fresh
    /// prompt — and putting this stale copy back would deliver it
    /// twice, so the failure is dropped instead.
    ///
    /// If the user withdrew it while it was in flight, the UI already
    /// dropped its row (a withdrawal mid-steer cannot know whether the
    /// message arrived, so it reports "already consumed" and keeps the
    /// text off screen). A late failure proves it never arrived — the
    /// message goes to [`Self::drain_returned_submits`] so the runner
    /// can put the row back rather than silently losing it.
    pub fn steer_failed(&self, submit: SubmitPayload) {
        let Some(uuid) = submit.user_message_uuid.clone() else {
            // Keyless submits never enter the in-flight map; hand the
            // text to the end-of-turn drain rather than dropping it.
            {
                let mut queue = self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queue.push_front(submit);
            }
            return;
        };
        let owned = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&uuid)
            .is_some();
        if !owned {
            return;
        }
        let withdrawn = self
            .withdrawn_in_flight_uuids
            .lock()
            .map(|mut withdrawn| withdrawn.remove(&uuid))
            .unwrap_or(false);
        if withdrawn {
            {
                let mut returned = self
                    .returned
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                returned.push(submit);
            }
            return;
        }
        {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.push_front(submit);
        }
    }

    /// Put every message still checked out to a steer back where it
    /// belongs. The pump's drop calls this after aborting the task: an
    /// abort can drop a steer future mid-await, and a message whose
    /// report never comes would otherwise stay in flight forever —
    /// reading as "already consumed" to a withdrawal, which then
    /// silently discards the user's text.
    ///
    /// Withdrawn messages go to [`Self::drain_returned_submits`] (their
    /// row already left the screen); the rest return to the front of
    /// the queue for the end-of-turn drain. A report that raced this
    /// reclaim settles inside [`Self::steer_delivered`] /
    /// [`Self::steer_failed`] via the in-flight removal result.
    pub fn reclaim_in_flight(&self) {
        let reclaimed: Vec<(String, SubmitPayload)> = {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            in_flight.drain().collect()
        };
        if reclaimed.is_empty() {
            return;
        }
        for (uuid, submit) in reclaimed {
            let withdrawn = self
                .withdrawn_in_flight_uuids
                .lock()
                .map(|mut withdrawn| withdrawn.remove(&uuid))
                .unwrap_or(false);
            if withdrawn {
                {
                    let mut returned = self
                        .returned
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    returned.push(submit);
                }
            } else {
                let mut queue = self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queue.push_front(submit);
            }
        }
    }

    /// Messages that failed to steer *and* were withdrawn from the UI
    /// queue in the meantime, needing to be put back on screen. Nearly
    /// always empty.
    pub fn drain_returned_submits(&self) -> Vec<SubmitPayload> {
        self.returned
            .lock()
            .map(|mut returned| std::mem::take(&mut *returned))
            .unwrap_or_default()
    }

    pub fn enqueue_submit(&self, submit: &mut SubmitPayload) -> bool {
        if submit.execution_policy.is_some() {
            return false;
        }
        let session_id = self.session_id.clone();
        ensure_submit_payload_user_uuid(submit, &session_id);
        let mut queue = match self.queue.lock() {
            Ok(queue) => queue,
            Err(_) => return false,
        };
        queue.push_back(submit.clone());
        true
    }

    pub fn remove_submit(&self, submit: &SubmitPayload) {
        let Some(uuid) = submit.user_message_uuid.as_deref() else {
            return;
        };
        self.remove_user_message_uuid(uuid);
    }

    pub fn withdraw_submit(&self, submit: &SubmitPayload) -> QueuedSubmitWithdrawal {
        let Some(uuid) = submit.user_message_uuid.as_deref() else {
            return QueuedSubmitWithdrawal::NotTracked;
        };
        let mut removed_pending = false;
        let mut queue = match self.queue.lock() {
            Ok(queue) => queue,
            Err(_) => return QueuedSubmitWithdrawal::NotTracked,
        };
        queue.retain(|queued| {
            let remove = queued.user_message_uuid.as_deref() == Some(uuid);
            removed_pending |= remove;
            !remove
        });
        if removed_pending {
            return QueuedSubmitWithdrawal::PendingRemoved;
        }
        drop(queue);
        // In flight to an agent: we cannot know yet whether it
        // arrived, so treat it as sent. If the steer later fails, the
        // message comes back through `drain_returned_submits` instead
        // of being lost.
        let in_flight = self
            .in_flight
            .lock()
            .map(|in_flight| in_flight.contains_key(uuid))
            .unwrap_or(false);
        if in_flight {
            {
                let mut withdrawn = self
                    .withdrawn_in_flight_uuids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                withdrawn.insert(uuid.to_string());
            }
            return QueuedSubmitWithdrawal::AlreadyConsumed;
        }
        let consumed = match self.consumed_user_message_uuids.lock() {
            Ok(consumed) => consumed,
            Err(_) => return QueuedSubmitWithdrawal::NotTracked,
        };
        if consumed.contains(uuid) {
            QueuedSubmitWithdrawal::AlreadyConsumed
        } else {
            QueuedSubmitWithdrawal::NotTracked
        }
    }

    /// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
    #[doc(hidden)]
    pub fn remove_user_message_uuid(&self, uuid: &str) {
        {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.retain(|submit| submit.user_message_uuid.as_deref() != Some(uuid));
        }
        // The caller (the end-of-turn drain) is sending this message as
        // a fresh prompt. If a steer still has it checked out, claim it
        // here so a late steer_failed cannot requeue a copy the user
        // already sees being answered.
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(uuid);
        {
            let mut withdrawn = self
                .withdrawn_in_flight_uuids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            withdrawn.remove(uuid);
        }
        {
            let mut consumed = self
                .consumed_user_message_uuids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            consumed.remove(uuid);
        }
    }

    /// Forget everything owed to the current session view — queued,
    /// checked out to a steer, withdrawn, or awaiting return. `/clear`
    /// severs the conversation these messages were addressed to, so a
    /// late steer report must find nothing to resurrect.
    pub fn clear_pending(&self) {
        {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.clear();
        }
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        {
            let mut withdrawn = self
                .withdrawn_in_flight_uuids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            withdrawn.clear();
        }
        {
            let mut returned = self
                .returned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            returned.clear();
        }
    }

    pub fn drain_consumed_user_message_uuids(&self) -> HashSet<String> {
        match self.consumed_user_message_uuids.lock() {
            Ok(mut consumed) => std::mem::take(&mut *consumed),
            Err(_) => HashSet::new(),
        }
    }

    /// How many submits are waiting.
    ///
    /// Reads through a poisoned lock rather than reporting zero. It became
    /// visible to the architecture ratchet when it was put behind
    /// `test-support` instead of `cfg(test)`, and the ratchet was right:
    /// answering "none queued" because a lock is poisoned is the failure mode
    /// this probe exists to catch.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

fn with_submit_marker(mut message: ApiMessage, submit: &SubmitPayload, uuid: &str) -> ApiMessage {
    let image_paste_ids = submit
        .image_pastes
        .iter()
        .map(|image| image.id.to_string())
        .collect::<Vec<_>>();
    let attrs = if image_paste_ids.is_empty() {
        format!("uuid=\"{uuid}\"")
    } else {
        format!(
            "uuid=\"{uuid}\" imagePasteIds=\"{}\"",
            image_paste_ids.join(",")
        )
    };
    message.content.push(ApiContentBlock::Text(TextBlock {
        text: format!("<rebon-queued-user-input {attrs} />"),
    }));
    if let Some(model_text) = submit.model_text.as_ref() {
        message.content.push(ApiContentBlock::Text(TextBlock {
            text: format!("{LOCAL_QUEUED_MODEL_TEXT_MARKER_PREFIX}{model_text}"),
        }));
    }
    message
}

impl AttachmentPoller for MidTurnQueuedSubmitPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let mut queue = match self.queue.lock() {
            Ok(queue) => queue,
            Err(_) => return Vec::new(),
        };
        let mut messages = Vec::with_capacity(queue.len());
        let mut consumed_ids = Vec::with_capacity(queue.len());
        while let Some(submit) = queue.pop_front() {
            let Some(uuid) = submit.user_message_uuid.clone() else {
                continue;
            };
            let message = submit_payload_to_api_message(&submit);
            messages.push(with_submit_marker(message, &submit, &uuid));
            consumed_ids.push(uuid);
        }
        drop(queue);

        if !consumed_ids.is_empty() {
            {
                let mut consumed = self
                    .consumed_user_message_uuids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                consumed.extend(consumed_ids);
            }
        }

        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{ContentBlock, Role};

    fn request(next_iteration: u64) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new(
            "session",
            "turn",
            next_iteration,
            AttachmentPollPhase::Regular,
        )
    }

    fn submit(text: &str) -> SubmitPayload {
        SubmitPayload {
            text: text.into(),
            model_text: None,
            user_message_uuid: None,
            image_pastes: Vec::new(),
            directory_attachments: Vec::new(),
            execution_policy: None,
            skill_invocations: Vec::new(),
        }
    }

    #[test]
    fn idle_shell_feedback_replays_success_and_error_once_per_uuid() {
        use rebon_session::session_storage::{load_transcript_from_file, transcript_file_path};
        let root = tempfile::tempdir().unwrap();
        let state = rebon_acp::ServerState::default();
        let session = state.create_session(".".into(), Vec::new());
        let poller = MidTurnQueuedSubmitPoller::new(&session.id);
        let outputs = [
            "ok",
            "stderr\n[exit code: 7]",
            "Command failed: unavailable",
            "ok",
        ];
        for (index, output) in outputs.iter().enumerate() {
            let mut feedback = submit(&format!("!echo test\n⎿ {output}"));
            feedback.user_message_uuid = Some(format!("s-shell-{index}"));
            feedback.model_text = Some(format!("Local shell output: {output}"));
            poller.enqueue_submit(&mut feedback);
            assert!(poller
                .persist_idle_submit(root.path(), ".", &state, &feedback)
                .unwrap());
            // A repeated completion, even after the UI drained its receipts,
            // must not append a second transcript entry.
            poller.drain_consumed_user_message_uuids();
            assert!(poller
                .persist_idle_submit(root.path(), ".", &state, &feedback)
                .unwrap());
        }
        let transcript =
            load_transcript_from_file(&transcript_file_path(root.path(), ".", &session.id))
                .unwrap()
                .unwrap();
        assert_eq!(transcript.messages.len(), outputs.len());
        let replay = rebon_core::query::transcript_to_api_messages(&transcript.messages);
        let text = serde_json::to_string(&replay).unwrap();
        for output in [
            "Local shell output: ok",
            "exit code: 7",
            "Command failed: unavailable",
        ] {
            assert!(text.contains(output), "missing replay output: {text}");
        }
        assert_eq!(text.matches("Local shell output: ok").count(), 2);
        assert_eq!(poller.pending_len(), 0);
        assert!(poller.poll(request(1)).is_empty());
    }

    #[test]
    fn idle_shell_feedback_waits_for_turn_and_uses_its_final_parent() {
        use rebon_session::session_storage::{
            append_transcript_entry, load_transcript_from_file, transcript_file_path,
            TranscriptWriteEntry,
        };
        let root = tempfile::tempdir().unwrap();
        let state = rebon_acp::ServerState::default();
        let session = state.create_session(".".into(), Vec::new());
        let poller = MidTurnQueuedSubmitPoller::new(&session.id);
        let guard = poller.begin_turn();
        let mut feedback = submit("!echo test\n⎿ output");
        poller.enqueue_submit(&mut feedback);
        assert!(!poller
            .persist_idle_submit(root.path(), ".", &state, &feedback)
            .unwrap());
        let path = transcript_file_path(root.path(), ".", &session.id);
        assert!(!path.exists());
        append_transcript_entry(root.path(), ".", &session.id,
            TranscriptWriteEntry::new("assistant", serde_json::json!({
                "message": {"role": "assistant", "content": [{"type": "text", "text": "finished turn"}]}
            })).with_uuid("a-final")).unwrap();
        drop(guard);
        assert!(poller
            .persist_idle_submit(root.path(), ".", &state, &feedback)
            .unwrap());
        let transcript = load_transcript_from_file(&path).unwrap().unwrap();
        assert_eq!(transcript.messages.len(), 2);
        assert_eq!(
            transcript.messages[1].parent_uuid.as_deref(),
            Some("a-final")
        );
    }

    #[test]
    fn idle_shell_feedback_checks_durable_receipt_after_attachment_consumption() {
        use rebon_session::session_storage::{
            append_transcript_entry, transcript_file_path, TranscriptWriteEntry,
        };
        let root = tempfile::tempdir().unwrap();
        let state = rebon_acp::ServerState::default();
        let session = state.create_session(".".into(), Vec::new());
        let poller = MidTurnQueuedSubmitPoller::new(&session.id);
        let mut feedback = submit("!echo test\n⎿ output");
        poller.enqueue_submit(&mut feedback);
        let attachment = poller.poll(request(1));
        assert_eq!(attachment.len(), 1);
        poller.drain_consumed_user_message_uuids();
        // Simulate the engine's attachment persistence, not just its volatile
        // consumed notification. Idle settlement must leave these bytes alone.
        append_transcript_entry(
            root.path(),
            ".",
            &session.id,
            TranscriptWriteEntry::new(
                "user",
                serde_json::json!({"message": submit_payload_to_api_message(&feedback)}),
            )
            .with_uuid(feedback.user_message_uuid.as_deref().unwrap()),
        )
        .unwrap();
        let path = transcript_file_path(root.path(), ".", &session.id);
        let before = std::fs::read(&path).unwrap();
        assert!(poller
            .persist_idle_submit(root.path(), ".", &state, &feedback)
            .unwrap());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn idle_shell_feedback_retries_failed_persistence_without_losing_payload() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        let state = rebon_acp::ServerState::default();
        let session = state.create_session(".".into(), Vec::new());
        let poller = MidTurnQueuedSubmitPoller::new(&session.id);
        let mut feedback = submit("shell output");
        poller.enqueue_submit(&mut feedback);
        assert!(poller
            .persist_idle_submit(&blocked, ".", &state, &feedback)
            .is_err());
        assert_eq!(poller.pending_len(), 1);
        assert!(poller
            .persist_idle_submit(root.path(), ".", &state, &feedback)
            .unwrap());
        assert_eq!(poller.pending_len(), 0);
    }

    #[test]
    fn poll_drains_queued_submits_fifo_and_records_consumed_ids() {
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut first = submit("first");
        let mut second = submit("second");
        poller.enqueue_submit(&mut first);
        poller.enqueue_submit(&mut second);

        let messages = poller.poll(request(1));

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::User);
        match &messages[0].content[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "first"),
            other => panic!("expected text block, got {other:?}"),
        }
        match &messages[1].content[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "second"),
            other => panic!("expected text block, got {other:?}"),
        }
        assert!(matches!(
            messages[0].content.last(),
            Some(ContentBlock::Text(text)) if text.text.starts_with("<rebon-queued-user-input uuid=\"")
        ));

        let consumed = poller.drain_consumed_user_message_uuids();
        assert!(consumed.contains(first.user_message_uuid.as_deref().unwrap()));
        assert!(consumed.contains(second.user_message_uuid.as_deref().unwrap()));
        assert!(poller.poll(request(2)).is_empty());
    }

    #[test]
    fn queued_model_text_is_model_only() {
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut submit = submit("@src/widgets\\");
        submit.model_text =
            Some("@src/widgets\\\n\nDirectory listing for src\\widgets:\nbutton\\".into());

        poller.enqueue_submit(&mut submit);
        let messages = poller.poll(request(1));

        assert_eq!(messages.len(), 1);
        match &messages[0].content[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "@src/widgets\\"),
            other => panic!("expected visible text block, got {other:?}"),
        }
        assert!(matches!(
            messages[0].content.last(),
            Some(ContentBlock::Text(text))
                if text.text.starts_with(LOCAL_QUEUED_MODEL_TEXT_MARKER_PREFIX)
                    && text.text.contains("Directory listing for src\\widgets")
        ));
    }

    #[test]
    fn withdraw_submit_reports_already_consumed_without_clearing_consumed_id() {
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut submit = submit("queued");
        assert!(poller.enqueue_submit(&mut submit));
        assert_eq!(poller.poll(request(1)).len(), 1);

        assert_eq!(
            poller.withdraw_submit(&submit),
            QueuedSubmitWithdrawal::AlreadyConsumed
        );
        let consumed = poller.drain_consumed_user_message_uuids();
        assert!(consumed.contains(submit.user_message_uuid.as_deref().unwrap()));
    }

    #[test]
    fn a_steer_is_two_phase_so_a_failure_can_put_the_message_back() {
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut first = submit("first");
        let mut second = submit("second");
        poller.enqueue_submit(&mut first);
        poller.enqueue_submit(&mut second);

        let taken = poller.take_for_steer().expect("queued");
        assert_eq!(taken.text, "first");
        assert_eq!(poller.pending_len(), 1, "taken, not merely peeked");
        assert!(
            poller.drain_consumed_user_message_uuids().is_empty(),
            "in flight is not consumed — a failure still has to put it back"
        );

        poller.steer_failed(taken);
        assert_eq!(poller.pending_len(), 2);
        assert_eq!(
            poller.take_for_steer().expect("requeued").text,
            "first",
            "a failed steer returns to the front, keeping order"
        );
    }

    #[test]
    fn a_delivered_steer_is_consumed_like_a_locally_injected_message() {
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut pending = submit("steer me");
        poller.enqueue_submit(&mut pending);

        let taken = poller.take_for_steer().expect("queued");
        poller.steer_delivered(&taken);

        assert_eq!(poller.pending_len(), 0);
        assert!(poller
            .drain_consumed_user_message_uuids()
            .contains(taken.user_message_uuid.as_deref().unwrap()));
    }

    #[test]
    fn withdrawing_an_in_flight_steer_reads_as_sent_and_is_restored_only_if_it_failed() {
        // The user cannot un-send what may already have arrived, so
        // the row leaves the screen. If the steer then fails, the
        // message must come back rather than vanish.
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut pending = submit("maybe sent");
        poller.enqueue_submit(&mut pending);
        let taken = poller.take_for_steer().expect("queued");

        assert_eq!(
            poller.withdraw_submit(&taken),
            QueuedSubmitWithdrawal::AlreadyConsumed
        );
        assert!(poller.drain_returned_submits().is_empty());

        poller.steer_failed(taken.clone());
        let returned = poller.drain_returned_submits();
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0].text, "maybe sent");
        assert_eq!(
            poller.pending_len(),
            0,
            "a withdrawn message must not also sit in the send queue"
        );

        // The same withdrawal followed by success drops it for good.
        let mut other = submit("delivered");
        poller.enqueue_submit(&mut other);
        let taken = poller.take_for_steer().expect("queued");
        poller.withdraw_submit(&taken);
        poller.steer_delivered(&taken);
        assert!(poller.drain_returned_submits().is_empty());
    }

    #[test]
    fn an_aborted_steer_reclaims_the_in_flight_message() {
        // The pump's abort drops the steer future without a report;
        // reclaim must put the message back where a withdrawal can
        // still reach it instead of reading as "already consumed".
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut pending = submit("typed mid-turn");
        poller.enqueue_submit(&mut pending);
        let taken = poller.take_for_steer().expect("queued");

        poller.reclaim_in_flight();

        assert_eq!(poller.pending_len(), 1);
        assert_eq!(
            poller.withdraw_submit(&taken),
            QueuedSubmitWithdrawal::PendingRemoved,
            "a reclaimed message must be withdrawable, not swallowed"
        );
    }

    #[test]
    fn a_late_failure_after_a_drain_claim_does_not_requeue() {
        // The turn ended and the drain sent the message as a fresh
        // prompt (claiming it via remove); the steer's failure report
        // arrives afterwards and must not put a second copy back.
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut pending = submit("sent by the drain");
        poller.enqueue_submit(&mut pending);
        let taken = poller.take_for_steer().expect("queued");

        poller.remove_user_message_uuid(taken.user_message_uuid.as_deref().unwrap());
        poller.steer_failed(taken);

        assert_eq!(poller.pending_len(), 0, "no duplicate delivery");
        assert!(poller.drain_returned_submits().is_empty());
    }

    #[test]
    fn a_late_delivery_after_a_reclaim_removes_the_requeued_copy() {
        // The reclaim raced a steer that actually landed: the agent has
        // the message, so the requeued copy must go away and the uuid
        // must read as consumed for the turn-end reconcile.
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut pending = submit("landed after all");
        poller.enqueue_submit(&mut pending);
        let taken = poller.take_for_steer().expect("queued");

        poller.reclaim_in_flight();
        assert_eq!(poller.pending_len(), 1);
        poller.steer_delivered(&taken);

        assert_eq!(poller.pending_len(), 0, "no duplicate delivery");
        assert!(poller
            .drain_consumed_user_message_uuids()
            .contains(taken.user_message_uuid.as_deref().unwrap()));
    }

    #[test]
    fn clear_pending_forgets_in_flight_and_returned_state() {
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut pending = submit("cleared away");
        poller.enqueue_submit(&mut pending);
        let taken = poller.take_for_steer().expect("queued");
        assert_eq!(
            poller.withdraw_submit(&taken),
            QueuedSubmitWithdrawal::AlreadyConsumed,
            "in flight reads as sent until cleared"
        );

        poller.clear_pending();
        poller.steer_failed(taken.clone());

        assert_eq!(poller.pending_len(), 0);
        assert!(
            poller.drain_returned_submits().is_empty(),
            "a report landing after /clear must find nothing to resurrect"
        );
        assert_eq!(
            poller.withdraw_submit(&taken),
            QueuedSubmitWithdrawal::NotTracked,
            "cleared state must not read as consumed"
        );
    }

    #[test]
    fn enqueue_skips_execution_policy_payloads() {
        let poller = MidTurnQueuedSubmitPoller::new("sess-1");
        let mut submit = submit("next turn only");
        submit.execution_policy = Some(rebon_types::ExecutionPolicy::default());

        assert!(!poller.enqueue_submit(&mut submit));
        assert_eq!(poller.pending_len(), 0);
        assert!(submit.user_message_uuid.is_none());
    }
}
