//! Tolerant handling of `{"summary":"…"}` envelope messages.
//!
//! Side-channel model calls (session titles, agent-view row summaries)
//! instruct the model to answer with a single-field JSON object. A
//! transport race on the codex websocket backend used to let such a
//! response be consumed by the live conversation and persisted as a
//! regular assistant message, so existing transcripts (and streams
//! from not-yet-updated binaries) can contain assistant text that is
//! literally `{"summary":"fixed mobile chat refresh"}`. Display
//! surfaces (TUI, ACP replay, desktop app, mobile) use
//! [`summary_envelope_text`] to show the summary sentence instead of
//! raw JSON.

/// If `text` is exactly one JSON object whose only field is a
/// non-empty string `summary`, return the summary text.
///
/// The match is deliberately strict — leading/trailing whitespace is
/// tolerated, but any surrounding prose, code fences, or extra JSON
/// fields mean the message was (or may have been) authored on purpose
/// and is returned unchanged by callers.
pub fn summary_envelope_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let summary = object.get("summary")?.as_str()?.trim();
    if summary.is_empty() {
        return None;
    }
    Some(summary.to_string())
}

/// Replace a summary-envelope message with its summary sentence;
/// return other text unchanged.
pub fn display_text_for_summary_envelope(text: &str) -> String {
    summary_envelope_text(text).unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_summary_from_bare_envelope() {
        assert_eq!(
            summary_envelope_text(
                r#"{"summary":"fixed mobile chat refresh when desktop session is paused"}"#
            )
            .as_deref(),
            Some("fixed mobile chat refresh when desktop session is paused")
        );
    }

    #[test]
    fn tolerates_surrounding_whitespace_and_spaced_json() {
        assert_eq!(
            summary_envelope_text("  { \"summary\" : \"updated cache tests\" }\n").as_deref(),
            Some("updated cache tests")
        );
    }

    #[test]
    fn rejects_prose_fences_extra_fields_and_non_envelopes() {
        // Prose around the object → authored content, keep raw.
        assert!(summary_envelope_text(r#"Done: {"summary":"x"}"#).is_none());
        // Code fence → the model is *showing* JSON, keep raw.
        assert!(summary_envelope_text("```json\n{\"summary\":\"x\"}\n```").is_none());
        // Extra fields → not the side-channel envelope shape.
        assert!(summary_envelope_text(r#"{"summary":"x","title":"y"}"#).is_none());
        // Wrong field / type / emptiness.
        assert!(summary_envelope_text(r#"{"title":"x"}"#).is_none());
        assert!(summary_envelope_text(r#"{"summary":42}"#).is_none());
        assert!(summary_envelope_text(r#"{"summary":"   "}"#).is_none());
        // Plain text and malformed JSON.
        assert!(summary_envelope_text("regular assistant reply").is_none());
        assert!(summary_envelope_text("{\"summary\":\"unterminated").is_none());
    }

    #[test]
    fn display_helper_passes_ordinary_text_through() {
        assert_eq!(
            display_text_for_summary_envelope("normal answer"),
            "normal answer"
        );
        assert_eq!(
            display_text_for_summary_envelope(r#"{"summary":"tidy"}"#),
            "tidy"
        );
    }
}
