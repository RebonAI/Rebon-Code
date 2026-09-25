//! Deferred `AskUserQuestion`: the question goes on screen, the turn goes on.
//!
//! A question the user has not answered yet used to hold the whole turn:
//! the broker awaited the dialog, and everything the model could have done
//! in the meantime waited with it. When a surface installs a
//! [`DeferredQuestionSink`] on its `ChannelPermissionBroker`, an eligible
//! question is still shown exactly as before, but the tool returns a
//! pending result at once and a background task waits for the dialog.
//! The answer then reaches the model the way anything else the user says
//! does — as a user message, steered into the running turn or starting the
//! next one — which is why the sink hands over text rather than a tool
//! result.
//!
//! Which questions may be deferred is decided next to the sink, in
//! `ChannelPermissionBroker::question_deferral`. This module owns only the
//! switch, the delivery shape, and the words: what the model is told while
//! it waits, and what arrives when the user answers.

use serde_json::{json, Value};

/// Environment switch for deferral. On unless set to an "off" spelling
/// (`0`, `false`, `no`, `off`), so a surface that installs a sink defers by
/// default and a user who wants the old blocking dialog back sets
/// `REBON_DEFERRED_QUESTIONS=0`.
pub const DEFERRED_QUESTIONS_ENV: &str = "REBON_DEFERRED_QUESTIONS";

/// Whether deferral is switched on. Read per question, so flipping the
/// variable needs no new session.
pub fn deferred_questions_enabled() -> bool {
    !rebon_types::env::env_defined_falsy(DEFERRED_QUESTIONS_ENV)
}

/// The tool whose questions may be deferred.
pub(crate) const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

/// What the user did with a deferred question, ready to hand to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredQuestionAnswer {
    /// The session whose turn asked. A surface that has moved on to another
    /// session drops the delivery instead of answering the wrong
    /// conversation.
    pub session_id: String,
    /// The `AskUserQuestion` call this answers.
    pub tool_use_id: String,
    /// The transcript row the user sees.
    pub display_text: String,
    /// What the model reads, tagged with the call it answers.
    pub model_text: String,
    /// Whether to start a turn when none is running. Off for a bare
    /// dismissal: the user closed the dialog without saying anything, and
    /// waking the model to hear that would only spend a turn.
    pub start_turn_if_idle: bool,
}

/// Where a surface receives deferred answers. Called from a background
/// task, possibly after the turn that asked has ended.
pub trait DeferredQuestionSink: Send + Sync {
    fn deliver(&self, answer: DeferredQuestionAnswer);
}

const PENDING_STATUS: &str = "pending";

/// First sentence of every pending result. The auto-mode classifier reads
/// an `AskUserQuestion` result as the user's answer, and this is how it
/// tells the placeholder apart from one.
const PENDING_LEAD: &str = "Your question is on the user's screen; the answer has not arrived yet.";

/// The tool output an eligible question returns immediately.
pub(crate) fn pending_result(tool_use_id: &str) -> Value {
    json!({
        "status": PENDING_STATUS,
        "toolUseId": tool_use_id,
        "message": pending_message(tool_use_id),
    })
}

fn pending_message(tool_use_id: &str) -> String {
    format!(
        "{PENDING_LEAD} It will come later as a user message wrapped in \
         <question-answer tool_use_id=\"{}\">. Until then, continue only with \
         work that does not depend on the answer, and never guess it. If \
         nothing independent is left, end your turn with a short note saying \
         what you are waiting for.",
        rebon_types::xml_escape(tool_use_id)
    )
}

/// The model-facing text of a pending result, or `None` for an answered
/// one.
pub(crate) fn pending_model_text(value: &Value) -> Option<&str> {
    if value.get("status").and_then(Value::as_str) != Some(PENDING_STATUS) {
        return None;
    }
    value.get("message").and_then(Value::as_str)
}

/// Whether a tool result's text is the pending placeholder.
pub(crate) fn is_pending_model_text(text: &str) -> bool {
    text.starts_with(PENDING_LEAD)
}

/// The user picked answers. `output` is what the tool returned for them —
/// the same value the synchronous path hands the model.
pub(crate) fn answered(
    session_id: &str,
    tool_use_id: &str,
    output: &Value,
) -> DeferredQuestionAnswer {
    // The transcript text is the one the answered-questions card parses, so
    // a deferred answer renders exactly like one given while the turn
    // waited.
    let display_text = crate::query::format_ask_user_question_answer_for_transcript(output)
        .unwrap_or_else(|| "Answered the question.".to_string());
    let mut body = display_text.clone();
    if let Some(note) = output
        .get("permissionExtraText")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|note| !note.is_empty())
    {
        body.push_str("\nUser note: ");
        body.push_str(note);
    }
    DeferredQuestionAnswer {
        session_id: session_id.to_string(),
        tool_use_id: tool_use_id.to_string(),
        model_text: tagged(tool_use_id, None, &body),
        display_text,
        start_turn_if_idle: true,
    }
}

/// The user closed the dialog without answering. A note they wrote while
/// declining is something they said, so it wakes an idle session; a bare
/// dismissal does not.
pub(crate) fn dismissed(
    session_id: &str,
    tool_use_id: &str,
    note: Option<&str>,
) -> DeferredQuestionAnswer {
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    let mut body = "The user dismissed the question without answering.".to_string();
    if let Some(note) = note {
        body.push_str("\nUser note: ");
        body.push_str(note);
    }
    body.push_str(
        "\nDo not assume an answer. If the decision still blocks you, say so \
         instead of asking the same question again right away.",
    );
    let display_text = match note {
        Some(note) => format!("Dismissed the question: {note}"),
        None => "Dismissed the question without answering.".to_string(),
    };
    DeferredQuestionAnswer {
        session_id: session_id.to_string(),
        tool_use_id: tool_use_id.to_string(),
        model_text: tagged(tool_use_id, Some("dismissed"), &body),
        display_text,
        start_turn_if_idle: note.is_some(),
    }
}

/// The user answered but the tool refused what came back. Still owed to
/// the model: it is waiting on this call.
pub(crate) fn unreadable(
    session_id: &str,
    tool_use_id: &str,
    error: &str,
) -> DeferredQuestionAnswer {
    let body = format!(
        "The user answered, but the answer could not be read: {error}\n\
         Ask again only if the decision still blocks you."
    );
    DeferredQuestionAnswer {
        session_id: session_id.to_string(),
        tool_use_id: tool_use_id.to_string(),
        model_text: tagged(tool_use_id, Some("failed"), &body),
        display_text: "The answer to the question could not be read.".to_string(),
        start_turn_if_idle: true,
    }
}

fn tagged(tool_use_id: &str, status: Option<&str>, body: &str) -> String {
    let status = status
        .map(|status| format!(" status=\"{status}\""))
        .unwrap_or_default();
    format!(
        "<question-answer tool_use_id=\"{}\"{status}>\n{body}\n</question-answer>",
        rebon_types::xml_escape(tool_use_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_result_names_the_tag_its_answer_will_carry() {
        let value = pending_result("toolu_1");
        let text = pending_model_text(&value).expect("pending text");
        assert!(is_pending_model_text(text));
        assert!(text.contains("<question-answer tool_use_id=\"toolu_1\">"));
        assert!(text.contains("never guess it"));
        assert!(text.contains("end your turn"));
    }

    #[test]
    fn an_answered_result_is_not_pending() {
        let value = json!({ "questions": [], "answers": { "Q?": "A" } });
        assert_eq!(pending_model_text(&value), None);
        assert!(!is_pending_model_text("User has answered your questions"));
    }

    #[test]
    fn an_answer_restates_each_question_with_its_answer_and_notes() {
        let output = json!({
            "questions": [
                { "question": "Which database?", "header": "DB", "options": [] },
                { "question": "Which cache?", "header": "Cache", "options": [] }
            ],
            "answers": { "Which database?": "Postgres", "Which cache?": "Redis" },
            "annotations": { "Which cache?": { "notes": "keep it small" } },
            "permissionExtraText": "ship today"
        });
        let answer = answered("s1", "toolu_2", &output);
        assert_eq!(
            answer.display_text,
            "Answered questions:\n- Which database?\n  Answer: Postgres\n- Which cache?\n  Answer: Redis\n  Notes: keep it small"
        );
        assert_eq!(
            answer.model_text,
            "<question-answer tool_use_id=\"toolu_2\">\n\
             Answered questions:\n- Which database?\n  Answer: Postgres\n- Which cache?\n  Answer: Redis\n  Notes: keep it small\n\
             User note: ship today\n\
             </question-answer>"
        );
        assert!(answer.start_turn_if_idle);
        assert_eq!(answer.session_id, "s1");
        assert_eq!(answer.tool_use_id, "toolu_2");
    }

    #[test]
    fn a_bare_dismissal_does_not_wake_an_idle_session() {
        let answer = dismissed("s1", "toolu_3", None);
        assert!(!answer.start_turn_if_idle);
        assert!(answer
            .model_text
            .starts_with("<question-answer tool_use_id=\"toolu_3\" status=\"dismissed\">"));
        assert!(answer.model_text.contains("Do not assume an answer"));

        let with_note = dismissed("s1", "toolu_3", Some("  let's talk first "));
        assert!(with_note.start_turn_if_idle);
        assert!(with_note.model_text.contains("User note: let's talk first"));
        assert_eq!(
            with_note.display_text,
            "Dismissed the question: let's talk first"
        );

        let blank_note = dismissed("s1", "toolu_3", Some("   "));
        assert!(!blank_note.start_turn_if_idle);
    }

    #[test]
    fn the_tag_escapes_the_call_id() {
        let answer = unreadable("s1", "a\"b<c", "bad input");
        assert!(answer
            .model_text
            .starts_with("<question-answer tool_use_id=\"a&quot;b&lt;c\" status=\"failed\">"));
        assert!(answer.model_text.contains("bad input"));
        assert!(answer.start_turn_if_idle);
    }
}
