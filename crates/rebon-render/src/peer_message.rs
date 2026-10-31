//! The one line a successful cross-session send leaves in the transcript.
//!
//! A message to another session is a fact about who was written to, not a wall
//! of text: the card says `Message sent to @repo-qrst — <title>`, and what was
//! sent is one ctrl+o away. A target with no handle is still named by its
//! session, and a target with no title is named without one — an em dash with
//! nothing after it would read as a title that failed to load.

use std::collections::HashMap;

use serde_json::Value;

/// `mode` of a result a `SendMessage` returns when the recipient was another
/// session rather than a teammate or a local task.
const SESSION_DELIVERY_MODE: &str = "session";

/// Characters of a session id a card shows when the peer has no handle.
const SESSION_ID_SHORT_CHARS: usize = 8;

/// How the card names the peer.
///
/// The handle a sender addressed is what it expects to see back; a peer whose
/// owner has not published one falls back to the head of its id, which is the
/// same `session:<id>` spelling the tool itself accepts.
fn peer_label(output: &HashMap<String, Value>) -> Option<String> {
    if let Some(handle) = non_empty(output, "target_handle") {
        return Some(format!("@{handle}"));
    }
    let target = non_empty(output, "target")?;
    let session_id = target.strip_prefix("session:").unwrap_or(target);
    let short: String = session_id.chars().take(SESSION_ID_SHORT_CHARS).collect();
    Some(format!("session:{short}"))
}

fn non_empty<'a>(output: &'a HashMap<String, Value>, key: &str) -> Option<&'a str> {
    output
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_session_delivery(output: &HashMap<String, Value>) -> bool {
    non_empty(output, "mode") == Some(SESSION_DELIVERY_MODE)
}

/// The card's single line, or `None` for any result that was not a send to
/// another session — a teammate reply, a local task, a failure.
pub fn session_message_display(output: &HashMap<String, Value>) -> Option<String> {
    if !is_session_delivery(output) || output.get("success").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    let label = peer_label(output)?;
    match non_empty(output, "target_name") {
        Some(name) => Some(format!("Message sent to {label} — {name}")),
        None => Some(format!("Message sent to {label}")),
    }
}

/// What ctrl+o adds below that line: the sender's own summary, then the first
/// line of what it actually wrote.
pub fn session_message_detail_lines(output: &HashMap<String, Value>) -> Vec<String> {
    if session_message_display(output).is_none() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if let Some(summary) = non_empty(output, "summary") {
        lines.push(summary.to_string());
    }
    if let Some(first) = non_empty(output, "body_first_line") {
        lines.push(first.to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn output(pairs: Value) -> HashMap<String, Value> {
        serde_json::from_value(pairs).expect("an object")
    }

    #[test]
    fn a_send_names_the_peer_it_wrote_to() {
        let line = session_message_display(&output(json!({
            "success": true,
            "mode": "session",
            "target": "session:abcde-fghij-klmno-pqrst",
            "target_handle": "repo-qrst",
            "target_name": "worker",
        })))
        .expect("a session send has a line");
        assert_eq!(line, "Message sent to @repo-qrst — worker");
    }

    /// A peer whose owner has not published a handle is named by the head of
    /// its id — the same spelling the tool accepts as a target.
    #[test]
    fn a_peer_without_a_handle_is_named_by_its_session() {
        let line = session_message_display(&output(json!({
            "success": true,
            "mode": "session",
            "target": "session:abcde-fghij-klmno-pqrst",
        })))
        .expect("a session send has a line");
        assert_eq!(line, "Message sent to session:abcde-fg");
    }

    /// The em dash only appears when there is a title to follow it.
    #[test]
    fn a_peer_without_a_title_is_named_without_one() {
        let line = session_message_display(&output(json!({
            "success": true,
            "mode": "session",
            "target": "session:abcde-fghij-klmno-pqrst",
            "target_handle": "repo-qrst",
            "target_name": "   ",
        })))
        .expect("a session send has a line");
        assert_eq!(line, "Message sent to @repo-qrst");
        assert!(!line.contains('—'), "{line}");
    }

    #[test]
    fn a_teammate_or_task_or_failed_send_has_no_such_line() {
        for result in [
            json!({ "success": true, "target": "bob" }),
            json!({ "success": true, "target": "task-1", "mode": "runtime" }),
            json!({ "success": false, "mode": "session", "target": "session:abcde" }),
            json!({ "mode": "session" }),
        ] {
            assert_eq!(session_message_display(&output(result.clone())), None, "{result}");
            assert!(session_message_detail_lines(&output(result)).is_empty());
        }
    }

    #[test]
    fn the_detail_shows_the_summary_then_the_first_line_of_the_body() {
        let result = output(json!({
            "success": true,
            "mode": "session",
            "target": "session:abcde-fghij-klmno-pqrst",
            "target_handle": "repo-qrst",
            "summary": "handoff",
            "body_first_line": "please look at the failing test",
        }));
        assert_eq!(
            session_message_detail_lines(&result),
            vec![
                "handoff".to_string(),
                "please look at the failing test".to_string()
            ]
        );
    }

    #[test]
    fn a_detail_line_that_is_absent_is_not_an_empty_row() {
        let result = output(json!({
            "success": true,
            "mode": "session",
            "target": "session:abcde-fghij-klmno-pqrst",
            "summary": "   ",
        }));
        assert!(session_message_detail_lines(&result).is_empty());
    }
}
