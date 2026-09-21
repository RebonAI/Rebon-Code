//! What one submission carries, and how it becomes a model message.
//!
//! A [`SubmitPayload`] is the whole of what the user handed over in one
//! turn: the text, the model-facing rewrite of it, pasted images,
//! expanded directory attachments, an execution policy, and any skills
//! the prompt invoked. Every surface builds one — the TUI from its
//! prompt buffer, a worker from an IPC request, the mid-turn queue from
//! a message that arrived while a turn was running — so it is session
//! state, not terminal state, and it lives here rather than in the
//! TUI's dispatch layer.
//!
//! The uuid helpers are what make a payload addressable: a queued
//! message has to be findable again to be withdrawn, replayed, or
//! marked consumed, and the prefix distinguishes what the user typed
//! (`u-user`) from what Rebon injected on their behalf (`u-internal`).

use std::sync::atomic::{AtomicU64, Ordering};

use rebon_api::{
    ContentBlock as ApiContentBlock, ImageBlock, Message as ApiMessage, Role, TextBlock,
};
use rebon_types::ExecutionPolicy;
use rebon_types::PromptPasteContent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSkillInvocation {
    pub skill: String,
    pub args: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPayload {
    pub text: String,
    pub model_text: Option<String>,
    pub user_message_uuid: Option<String>,
    pub image_pastes: Vec<PromptPasteContent>,
    pub directory_attachments: Vec<DirectoryAttachment>,
    pub execution_policy: Option<ExecutionPolicy>,
    pub skill_invocations: Vec<PendingSkillInvocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryAttachment {
    pub path: String,
    pub display_path: String,
    pub content: String,
}

impl SubmitPayload {
    pub fn prompt_text(&self) -> &str {
        self.model_text.as_deref().unwrap_or(&self.text)
    }
}

pub fn ensure_submit_payload_user_uuid(submit: &mut SubmitPayload, session_id: &str) -> String {
    ensure_submit_payload_user_uuid_with_prefix(submit, session_id, "u-user")
}

pub fn ensure_internal_submit_payload_user_uuid(
    submit: &mut SubmitPayload,
    session_id: &str,
) -> String {
    ensure_submit_payload_user_uuid_with_prefix(submit, session_id, "u-internal")
}

fn ensure_submit_payload_user_uuid_with_prefix(
    submit: &mut SubmitPayload,
    session_id: &str,
    prefix: &str,
) -> String {
    if let Some(uuid) = submit.user_message_uuid.clone() {
        return uuid;
    }
    static USER_UUID_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = USER_UUID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let uuid = format!(
        "{prefix}-{session_id}-{}-{counter}",
        rebon_types::wall_clock_ms_u128()
    );
    submit.user_message_uuid = Some(uuid.clone());
    uuid
}

/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn submit_payload_to_api_message(submit: &SubmitPayload) -> ApiMessage {
    let mut content = Vec::new();
    if !submit.text.is_empty() {
        content.push(ApiContentBlock::Text(TextBlock {
            text: submit.text.clone(),
        }));
    }
    content.extend(submit.image_pastes.iter().map(|image| {
        ApiContentBlock::Image(ImageBlock::base64(
            image
                .media_type
                .clone()
                .unwrap_or_else(|| String::from("image/png")),
            image.content.clone(),
        ))
    }));
    ApiMessage {
        role: Role::User,
        content,
    }
}
