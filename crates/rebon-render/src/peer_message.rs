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

/// `delivery` of a result whose message was left in the target's queue: it was
/// neither injected into a running turn nor allowed to start one.
const QUEUED_DELIVERY: &str = "queued";

/// Characters of a session id a card shows when the peer has no handle.
const SESSION_ID_SHORT_CHARS: usize = 8;

/// What the card says about a message that is only waiting in a queue.
const QUEUED_MARKER: &str = " · queued";

/// Replace everything that would break a single-row card: a newline splits the
/// row, a `\r` rewinds it, and any other control character is not text at all.
fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// How the card names the peer.
///
/// The handle a sender addressed is what it expects to see back; a peer whose
/// owner has not published one falls back to the head of its id, which is the
/// same `session:<id>` spelling the tool itself accepts.
fn peer_label(output: &HashMap<String, Value>) -> Option<String> {
    if let Some(handle) = non_empty(output, "target_handle") {
        return Some(format!("@{}", one_line(handle)));
    }
    let target = non_empty(output, "target")?;
    let session_id = target.strip_prefix("session:").unwrap_or(target);
    let short: String = session_id.chars().take(SESSION_ID_SHORT_CHARS).collect();
    Some(format!("session:{}", one_line(&short)))
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
///
/// `width` is the columns the line is painted in: a title is remote text, and a
/// row that overflows folds into a second line that reads as a second fact.
pub fn session_message_display(output: &HashMap<String, Value>, width: usize) -> Option<String> {
    if !is_session_delivery(output) || output.get("success").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    let label = peer_label(output)?;
    let mut line = match non_empty(output, "target_name") {
        Some(name) => format!("Message sent to {label} — {}", one_line(name)),
        None => format!("Message sent to {label}"),
    };
    if non_empty(output, "delivery") == Some(QUEUED_DELIVERY) {
        line.push_str(QUEUED_MARKER);
    }
    Some(rebon_width::truncate_to_ellipsis(&line, width))
}

/// What ctrl+o adds below that line: the sender's own summary, then the first
/// line of what it actually wrote.
///
/// Both come from the tool's *input*, not its result. The result is what the
/// model reads back, and neither of them helps it act: the summary is already
/// the sender's own words about the same message, and the body's first line is
/// the message it just wrote. Carrying them in the result spent tokens on
/// every send and put an unbounded line of remote text into the model's
/// context.
pub fn session_message_detail_lines(
    output: &HashMap<String, Value>,
    input: Option<&HashMap<String, Value>>,
    width: usize,
) -> Vec<String> {
    if session_message_display(output, width).is_none() {
        return Vec::new();
    }
    let summary = input.and_then(|input| non_empty(input, "summary"));
    let body_first_line = input
        .and_then(|input| non_empty(input, "message"))
        .and_then(|message| message.lines().map(str::trim).find(|line| !line.is_empty()));
    [summary, body_first_line]
        .into_iter()
        .flatten()
        .map(|line| rebon_width::truncate_to_ellipsis(&one_line(line), width))
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WIDE: usize = 120;

    fn output(pairs: Value) -> HashMap<String, Value> {
        serde_json::from_value(pairs).expect("an object")
    }

    fn input(pairs: Value) -> HashMap<String, Value> {
        serde_json::from_value(pairs).expect("an object")
    }

    #[test]
    fn a_send_names_the_peer_it_wrote_to() {
        let line = session_message_display(
            &output(json!({
                "success": true,
                "mode": "session",
                "target": "session:abcde-fghij-klmno-pqrst",
                "target_handle": "repo-qrst",
                "target_name": "worker",
            })),
            WIDE,
        )
        .expect("a session send has a line");
        assert_eq!(line, "Message sent to @repo-qrst — worker");
    }

    /// A peer whose owner has not published a handle is named by the head of
    /// its id — the same spelling the tool accepts as a target.
    #[test]
    fn a_peer_without_a_handle_is_named_by_its_session() {
        let line = session_message_display(
            &output(json!({
                "success": true,
                "mode": "session",
                "target": "session:abcde-fghij-klmno-pqrst",
            })),
            WIDE,
        )
        .expect("a session send has a line");
        assert_eq!(line, "Message sent to session:abcde-fg");
    }

    /// The em dash only appears when there is a title to follow it.
    #[test]
    fn a_peer_without_a_title_is_named_without_one() {
        let line = session_message_display(
            &output(json!({
                "success": true,
                "mode": "session",
                "target": "session:abcde-fghij-klmno-pqrst",
                "target_handle": "repo-qrst",
                "target_name": "   ",
            })),
            WIDE,
        )
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
            assert_eq!(
                session_message_display(&output(result.clone()), WIDE),
                None,
                "{result}"
            );
            assert!(session_message_detail_lines(&output(result), None, WIDE).is_empty());
        }
    }

    #[test]
    fn the_detail_shows_the_summary_then_the_first_line_of_the_body() {
        let result = output(json!({
            "success": true,
            "mode": "session",
            "target": "session:abcde-fghij-klmno-pqrst",
            "target_handle": "repo-qrst",
        }));
        let sent = input(json!({
            "to": "@repo-qrst",
            "summary": "handoff",
            "message": "please look at the failing test",
        }));
        assert_eq!(
            session_message_detail_lines(&result, Some(&sent), WIDE),
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
        }));
        assert!(session_message_detail_lines(
            &result,
            Some(&input(json!({ "summary": "   ", "message": "\n\n  \n" }))),
            WIDE,
        )
        .is_empty());
    }

    /// A title is whatever the peer's owner wrote, and its owner is another
    /// session: a newline in it would split the one-row card in two.
    #[test]
    fn a_title_that_carries_control_characters_stays_on_one_line() {
        let line = session_message_display(
            &output(json!({
                "success": true,
                "mode": "session",
                "target": "session:abcde-fghij-klmno-pqrst",
                "target_handle": "repo-qrst",
                "target_name": "work\ning\r\non it\u{7}",
            })),
            WIDE,
        )
        .expect("a session send has a line");
        assert_eq!(line, "Message sent to @repo-qrst — work ing on it");
        assert_eq!(line.lines().count(), 1, "{line:?}");
    }

    /// A row that overflows folds into a second line, which reads as a second
    /// fact; the card is truncated to the columns it is painted in instead.
    #[test]
    fn a_card_wider_than_the_terminal_is_truncated_to_it() {
        let result = output(json!({
            "success": true,
            "mode": "session",
            "target": "session:abcde-fghij-klmno-pqrst",
            "target_handle": "repo-qrst",
            "target_name": "a worker with a rather long title",
        }));
        let line = session_message_display(&result, 24).expect("a session send has a line");
        assert_eq!(rebon_width::str_width(&line), 24, "{line:?}");
        assert!(line.ends_with('…'), "{line:?}");
        let detail = session_message_detail_lines(
            &result,
            Some(&input(json!({ "summary": "x".repeat(80) }))),
            24,
        );
        assert_eq!(rebon_width::str_width(&detail[0]), 24, "{detail:?}");
    }

    /// A message that only reached the target's queue has not been read yet,
    /// and the card says so on the same row.
    #[test]
    fn a_queued_message_says_so_on_its_line() {
        let line = session_message_display(
            &output(json!({
                "success": true,
                "mode": "session",
                "target": "session:abcde-fghij-klmno-pqrst",
                "target_handle": "repo-qrst",
                "target_name": "worker",
                "delivery": "queued",
            })),
            WIDE,
        )
        .expect("a session send has a line");
        assert_eq!(line, "Message sent to @repo-qrst — worker · queued");
    }

    /// Every other outcome happened already; only `queued` is still pending.
    #[test]
    fn a_delivered_message_does_not_carry_the_queued_marker() {
        for delivery in ["injected", "started_turn"] {
            let line = session_message_display(
                &output(json!({
                    "success": true,
                    "mode": "session",
                    "target": "session:abcde-fghij-klmno-pqrst",
                    "target_handle": "repo-qrst",
                    "delivery": delivery,
                })),
                WIDE,
            )
            .expect("a session send has a line");
            assert_eq!(line, "Message sent to @repo-qrst", "{delivery}");
        }
    }
}
