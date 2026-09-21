//! Session-title generation via a concrete small-profile model.
//!
//! Shared session-title generation behavior.
//! Single source of truth for AI-generated session titles across
//! rebon — the resume dialog, future SDK / IPC surfaces, and any
//! later "rename" command all go through
//! [`generate_session_title`].
//!
//! ## Implementation notes
//!
//! * The title model is resolved by the caller from the active provider's
//!   `small` profile. This module only receives the concrete model id to
//!   place on the request.
//! * Responses are deserialized with `serde` into [`TitleEnvelope`], which
//!   enforces the requested `{"title": "..."}` contract.
//! * Outcomes are logged at `debug` with the success/failure reason.
//!
//! ## What this module deliberately does NOT do
//!
//! * It does not persist the result. The caller writes the title to
//!   its own session store (or equivalent) so the display layer can
//!   pick it up on the next
//!   `session/list`.
//! * It does not decide *when* to generate a title. Callers spawn
//!   this on their own cadence — typically once per session, guarded by the
//!   sidecar's presence.
//! * It does not fork the [`ModelClient`] itself — the *caller* is
//!   responsible for passing an isolated client. Side-channel calls
//!   like this run concurrently with the live conversation, and on
//!   providers with sticky session state (the codex websocket
//!   transport chains `previous_response_id` per connection) sharing
//!   the conversation's client corrupts that chain and can leak this
//!   call's response into the main turn's stream. Callers should pass
//!   `client.fork_for_sub_agent()` (same auth + middleware, fresh
//!   session state), falling back to the shared client only when the
//!   provider has no fork.

use crate::client::ModelClient;
use crate::request::CreateMessageRequest;
use crate::types::{ContentBlock, Message};

/// Maximum length (in chars, not bytes) of the conversation text fed
/// to the title model. Tail-slicing to the last 1000 chars keeps recent
/// context in view while bounding token spend.
pub const MAX_CONVERSATION_TEXT: usize = 1000;

/// System prompt driving the title model. Kept centralized so all callers
/// request titles with the same wording and response contract.
pub const SESSION_TITLE_PROMPT: &str = "Generate a concise, sentence-case title (3-7 words) that captures the main topic or goal of this coding session. The title should be clear enough that the user recognizes the session in a list. Use sentence case: capitalize only the first word and proper nouns. Write the title in the language the user is writing in.

Return JSON with a single \"title\" field.

Good examples:
{\"title\": \"Fix login button on mobile\"}
{\"title\": \"Add OAuth authentication\"}
{\"title\": \"Debug failing CI tests\"}
{\"title\": \"Refactor API client error handling\"}

Bad (too vague): {\"title\": \"Code changes\"}
Bad (too long): {\"title\": \"Investigate and fix the issue where the login button does not respond on mobile devices\"}
Bad (wrong case): {\"title\": \"Fix Login Button On Mobile\"}

Reply at once with the JSON object alone; the output budget is tiny, so do not deliberate first.";

/// JSON response shape the model is instructed to emit.
#[derive(Debug, serde::Deserialize)]
struct TitleEnvelope {
    title: String,
}

/// Flatten a message list into a single text blob suitable for
/// feeding to the title model.
///
/// Collapse the trailing conversation into a compact title-model prompt.
/// * Skips anything that isn't a plain user/assistant message (no
///   tool_use, no tool_result, no server-side tool blocks).
/// * Concatenates `Text` block bodies with `\n` between messages.
/// * Tail-slices to the last [`MAX_CONVERSATION_TEXT`] *characters*
///   so recent context wins when the conversation is long.
///
/// Rebon does not have per-message `isMeta` / `origin` flags, so the
/// "skip meta/non-human entries" filter only fires on role.
pub fn extract_conversation_text(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        // Only human-authored user and assistant messages contribute.
        // Tool_result messages are still role=User but their content
        // is all ToolResult blocks; those get filtered below.
        if !matches!(
            msg.role,
            crate::types::Role::User | crate::types::Role::Assistant
        ) {
            continue;
        }
        let mut msg_text = String::new();
        for block in &msg.content {
            if let ContentBlock::Text(t) = block {
                if !msg_text.is_empty() {
                    msg_text.push('\n');
                }
                msg_text.push_str(&t.text);
            }
        }
        if !msg_text.is_empty() {
            parts.push(msg_text);
        }
    }
    let text = parts.join("\n");
    if text.chars().count() <= MAX_CONVERSATION_TEXT {
        return text;
    }
    // Tail-slice on char boundaries so multi-byte sequences stay
    // intact. `String::chars().rev().take(...)` reverses per char,
    // which is what we want, but we need to re-reverse to preserve
    // reading order.
    let keep: String = text
        .chars()
        .rev()
        .take(MAX_CONVERSATION_TEXT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    keep
}

/// Generate a session title by asking `client` to summarise
/// `conversation_text` with the caller-resolved small-profile model.
///
/// Returns `None` on *any* failure: empty input, model error,
/// unparseable response, missing `title` field, or an empty title
/// string. The caller is expected to treat `None` as "try again
/// next turn" and fall through to the non-AI display chain.
pub async fn generate_session_title(
    client: &dyn ModelClient,
    model: &str,
    conversation_text: &str,
) -> Option<String> {
    let trimmed = conversation_text.trim();
    if trimmed.is_empty() {
        tracing::debug!("session-title: skipping generation — empty conversation text");
        return None;
    }

    let mut request =
        CreateMessageRequest::simple(model, trimmed).with_system(SESSION_TITLE_PROMPT);
    request.stream = false;
    request.max_tokens = 128; // titles are short; cap hard.

    let message = match client.create_message(request).await {
        Ok(m) => m,
        Err(err) => {
            tracing::debug!(error = %err, "session-title: model call failed");
            return None;
        }
    };

    let raw = message.text();
    let parsed = parse_title_from_response(&raw);
    if parsed.is_none() {
        tracing::debug!(raw = %raw, "session-title: response did not yield a usable title");
    }
    parsed
}

/// Extract the `title` field from a model response. The model is
/// instructed to return bare JSON, but real-world responses
/// sometimes wrap it in markdown code fences or prose; we tolerate
/// both by scanning for the first `{...}` block.
fn parse_title_from_response(raw: &str) -> Option<String> {
    let slice = extract_first_json_object(raw)?;
    let env: TitleEnvelope = serde_json::from_str(slice).ok()?;
    let cleaned = env.title.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.to_string())
}

/// Return the substring spanning the first top-level `{...}` block
/// in `raw`, handling nested braces correctly. Returns `None` if
/// the braces are unbalanced.
fn extract_first_json_object(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        match b {
            b'\\' if in_string => escape = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    let end = start + i + 1;
                    return Some(&raw[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent};
    use crate::mock::MockModelClient;
    use crate::types::{StopReason, TextBlock, Usage};
    use std::sync::Arc;

    // ── SESSION_TITLE_PROMPT invariants ────────────────────────────

    #[test]
    fn prompt_requires_sentence_case_and_bounded_length() {
        // Regression guard. If these assertions fail, the prompt lost an
        // instruction the title parser and display rely on — check that the
        // edit was intended before updating them.
        assert!(SESSION_TITLE_PROMPT.contains("sentence-case"));
        assert!(SESSION_TITLE_PROMPT.contains("3-7 words"));
        assert!(SESSION_TITLE_PROMPT.contains("\"title\""));
    }

    // ── extract_conversation_text ──────────────────────────────────

    fn user_text(s: &str) -> Message {
        Message::user_text(s)
    }

    fn assistant_text(s: &str) -> Message {
        Message::assistant_text(s)
    }

    #[test]
    fn extract_conversation_text_joins_user_and_assistant() {
        let msgs = vec![
            user_text("hi"),
            assistant_text("hello! how can I help?"),
            user_text("fix my login"),
        ];
        let got = extract_conversation_text(&msgs);
        assert!(got.contains("hi"));
        assert!(got.contains("hello! how can I help?"));
        assert!(got.contains("fix my login"));
    }

    #[test]
    fn extract_conversation_text_skips_tool_result_blocks() {
        // A user message whose content is a tool_result envelope has
        // no Text block, so it contributes nothing — the role-based
        // filter treats it as non-human.
        let msgs = vec![
            user_text("read main.rs"),
            Message {
                role: crate::types::Role::User,
                content: vec![ContentBlock::ToolResult(crate::types::ToolResultBlock {
                    tool_use_id: "t1".into(),
                    content: "fn main() {}".into(),
                    is_error: false,
                })],
            },
        ];
        let got = extract_conversation_text(&msgs);
        assert_eq!(got.trim(), "read main.rs");
        assert!(!got.contains("fn main"));
    }

    #[test]
    fn extract_conversation_text_tail_slices_long_input() {
        let long = "x".repeat(MAX_CONVERSATION_TEXT * 3);
        let msg = Message {
            role: crate::types::Role::User,
            content: vec![ContentBlock::Text(TextBlock { text: long })],
        };
        let got = extract_conversation_text(&[msg]);
        assert_eq!(got.chars().count(), MAX_CONVERSATION_TEXT);
    }

    #[test]
    fn extract_conversation_text_preserves_multibyte_boundary() {
        // Tail-slicing by chars (not bytes) must never split a
        // multi-byte UTF-8 codepoint. Build input that forces the
        // slice boundary to land inside a Chinese character if the
        // implementation were buggy (byte-slicing).
        let prefix = "a".repeat(MAX_CONVERSATION_TEXT);
        let text = format!("{}修复登录", prefix);
        let msg = Message {
            role: crate::types::Role::User,
            content: vec![ContentBlock::Text(TextBlock { text })],
        };
        let got = extract_conversation_text(&[msg]);
        assert_eq!(got.chars().count(), MAX_CONVERSATION_TEXT);
        // The tail (most recent chars) must survive intact.
        assert!(got.ends_with("修复登录"));
    }

    // ── parse_title_from_response ──────────────────────────────────

    #[test]
    fn parse_title_from_bare_json() {
        let got = parse_title_from_response(r#"{"title": "Fix login bug"}"#);
        assert_eq!(got.as_deref(), Some("Fix login bug"));
    }

    #[test]
    fn parse_title_from_markdown_fenced_json() {
        // Models sometimes wrap JSON in markdown fences despite the
        // prompt asking for bare JSON. The scanner must still find
        // the object.
        let raw = "```json\n{\"title\": \"Debug failing CI tests\"}\n```";
        let got = parse_title_from_response(raw);
        assert_eq!(got.as_deref(), Some("Debug failing CI tests"));
    }

    #[test]
    fn parse_title_from_prose_prefixed_json() {
        let raw = "Here you go: {\"title\": \"Add OAuth authentication\"}";
        let got = parse_title_from_response(raw);
        assert_eq!(got.as_deref(), Some("Add OAuth authentication"));
    }

    #[test]
    fn parse_title_ignores_empty_string() {
        let got = parse_title_from_response(r#"{"title": "   "}"#);
        assert!(got.is_none());
    }

    #[test]
    fn parse_title_none_on_missing_field() {
        let got = parse_title_from_response(r#"{"summary": "nope"}"#);
        assert!(got.is_none());
    }

    #[test]
    fn parse_title_none_on_garbage() {
        assert!(parse_title_from_response("not json at all").is_none());
        assert!(parse_title_from_response("").is_none());
        assert!(parse_title_from_response("{").is_none());
        assert!(parse_title_from_response("{\"title\": \"unterminated").is_none());
    }

    #[test]
    fn parse_title_handles_nested_objects() {
        let raw = r#"{"title": "Nested {braces} in title", "meta": {"nested": true}}"#;
        let got = parse_title_from_response(raw);
        assert_eq!(got.as_deref(), Some("Nested {braces} in title"));
    }

    #[test]
    fn parse_title_handles_escaped_quotes_inside_title() {
        // Strings with escaped quotes must not prematurely terminate
        // the JSON scan. `\"` inside a string is single char.
        let raw = r#"{"title": "Titles with \"quotes\" preserved"}"#;
        let got = parse_title_from_response(raw);
        assert_eq!(got.as_deref(), Some(r#"Titles with "quotes" preserved"#));
    }

    // ── generate_session_title end-to-end through MockModelClient ──

    fn mock_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: "title-mock".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    #[tokio::test]
    async fn generate_returns_title_on_well_formed_response() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(mock_reply(r#"{"title": "Fix auth bug"}"#));
        let got =
            generate_session_title(mock.as_ref(), "small-title-model", "some conversation").await;
        assert_eq!(got.as_deref(), Some("Fix auth bug"));
    }

    #[tokio::test]
    async fn generate_returns_none_on_unparseable_response() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(mock_reply("the model refused to follow instructions"));
        let got =
            generate_session_title(mock.as_ref(), "small-title-model", "some conversation").await;
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn generate_returns_none_on_empty_input() {
        let mock = Arc::new(MockModelClient::new());
        // Mock has no scripted turn — if the client is reached, it
        // would error. The empty-input guard must short-circuit
        // *before* the client call.
        let got = generate_session_title(mock.as_ref(), "small-title-model", "   \n  \t").await;
        assert!(got.is_none());
        assert_eq!(
            mock.call_count(),
            0,
            "empty input must not trigger a model call"
        );
    }

    #[tokio::test]
    async fn generate_returns_none_on_model_error() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_error(crate::ModelError::transient("network"));
        let got =
            generate_session_title(mock.as_ref(), "small-title-model", "some conversation").await;
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn generate_sends_the_resolved_small_profile_model_on_the_request() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(mock_reply(r#"{"title": "anything"}"#));
        let _ = generate_session_title(mock.as_ref(), "small-title-model", "conversation").await;

        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].model, "small-title-model");
    }
}
