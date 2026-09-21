//! Keeping every frame under RC's size cap.
//!
//! RC refuses a frame over `REBON_RC_MAX_BODY_BYTES` (256 KiB by default)
//! by closing the socket, and the stream client refuses to send one. A
//! session produces bigger things than that — a tool that read a large
//! file, a long pasted prompt echoed back — so the runner decides what
//! happens to them before they reach the wire:
//!
//! * **Text chunks are split.** An `agent_message_chunk` /
//!   `agent_thought_chunk` / `user_message_chunk` longer than
//!   [`TEXT_PIECE_BYTES`] goes out as several consecutive chunks, whose
//!   concatenation is the original text. Nothing is lost.
//! * **Everything else is truncated.** The longest strings in the update
//!   are cut (tool output, file contents, diffs) until the frame fits, each
//!   cut marked in-line, and the update's `_meta.rebonRc` records how many
//!   bytes were dropped. The transcript on the machine keeps the whole
//!   thing; the controller sees an honest prefix.
//! * **What cannot be cut down is omitted.** An update whose size is in
//!   its structure rather than in a few long strings is replaced by a stub
//!   that keeps its kind and tool call id and says it was omitted.

use serde_json::{json, Map, Value};

/// The size RC accepts per frame by default.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// What the runner lets one serialized frame grow to. Below the cap, so the
/// envelope, the ids and JSON escaping never push a fitted frame over it.
pub const FRAME_BUDGET_BYTES: usize = 200 * 1024;

/// The longest run of text one split chunk carries, in bytes of UTF-8.
/// Escaping can at most sextuple it (`\u00XX`), which still fits the
/// budget.
pub const TEXT_PIECE_BYTES: usize = 32 * 1024;

/// Strings at or below this are never cut: shortening them buys little
/// and they are the ones a reader needs whole (ids, titles, paths).
const MIN_CUT_BYTES: usize = 1024;

/// How a message was made to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// It already did.
    Unchanged,
    /// Long strings were cut; `dropped` bytes of text are gone.
    Truncated { dropped: usize },
    /// Nothing in it could be cut far enough; it was replaced by a stub.
    Omitted { original: usize },
}

/// The serialized size of `value`.
pub fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

/// Make `message` serialize to at most `budget` bytes. See the module docs.
pub fn fit_message(mut message: Value, budget: usize) -> (Value, Fit) {
    let original = json_len(&message);
    if original <= budget {
        return (message, Fit::Unchanged);
    }
    let mut dropped = 0usize;
    // Each pass cuts the longest string by what the whole message is over,
    // so a message with one huge string fits in one pass; the bound only
    // matters for messages made of many medium strings.
    for _ in 0..256 {
        let size = json_len(&message);
        if size <= budget {
            break;
        }
        let excess = size - budget;
        let Some(longest) = longest_string(&mut message) else {
            break;
        };
        let len = longest.len();
        if len <= MIN_CUT_BYTES {
            break;
        }
        // Cut the excess, and a margin for the marker, but never below
        // the floor.
        let keep = len.saturating_sub(excess + 96).max(MIN_CUT_BYTES);
        let keep = floor_char_boundary(longest, keep);
        let cut = len - keep;
        longest.truncate(keep);
        longest.push_str(&format!("…[rebon rc: {cut} bytes truncated]"));
        dropped += cut;
    }
    if json_len(&message) <= budget {
        mark(&mut message, json!({ "truncatedBytes": dropped }));
        if json_len(&message) <= budget {
            return (message, Fit::Truncated { dropped });
        }
    }
    (omitted_stub(&message, original), Fit::Omitted { original })
}

/// Split `text` into pieces of at most `max_bytes`, on character
/// boundaries. Never returns an empty list.
pub fn split_text(text: &str, max_bytes: usize) -> Vec<&str> {
    let max_bytes = max_bytes.max(4);
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.len() > max_bytes {
        let cut = floor_char_boundary(rest, max_bytes);
        let (head, tail) = rest.split_at(cut);
        pieces.push(head);
        rest = tail;
    }
    pieces.push(rest);
    pieces
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn longest_string(value: &mut Value) -> Option<&mut String> {
    match value {
        Value::String(text) => Some(text),
        Value::Array(items) => items
            .iter_mut()
            .filter_map(longest_string)
            .max_by_key(|text| text.len()),
        Value::Object(map) => map
            .iter_mut()
            // `_meta` is ours and the ids beside it are what a reader
            // matches on; neither is worth cutting.
            .filter(|(key, _)| key.as_str() != "_meta")
            .filter_map(|(_, value)| longest_string(value))
            .max_by_key(|text| text.len()),
        _ => None,
    }
}

/// Record what the runner did to this message, where ACP allows extra
/// data: `_meta` of the update when the message is a `session/update`
/// payload, of the message itself otherwise.
fn mark(message: &mut Value, note: Value) {
    let target = match message.get_mut("update") {
        Some(update @ Value::Object(_)) => update,
        _ => message,
    };
    let Value::Object(object) = target else {
        return;
    };
    let meta = object
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    meta.as_object_mut()
        .expect("replaced with an object above")
        .insert("rebonRc".to_string(), note);
}

fn omitted_stub(message: &Value, original: usize) -> Value {
    let note = json!({ "omittedBytes": original });
    let mut update = Map::new();
    if let Some(source) = message.get("update") {
        for key in ["sessionUpdate", "toolCallId", "status", "kind", "title"] {
            if let Some(value) = source.get(key).filter(|value| is_small(value)) {
                update.insert(key.to_string(), value.clone());
            }
        }
        update.insert("_meta".to_string(), json!({ "rebonRc": note }));
        let mut stub = Map::new();
        for key in ["sessionId", "turnGeneration"] {
            if let Some(value) = message.get(key).filter(|value| is_small(value)) {
                stub.insert(key.to_string(), value.clone());
            }
        }
        stub.insert("update".to_string(), Value::Object(update));
        return Value::Object(stub);
    }
    json!({ "_meta": { "rebonRc": note } })
}

fn is_small(value: &Value) -> bool {
    json_len(value) <= MIN_CUT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_update(output: String) -> Value {
        json!({
            "sessionId": "s-1",
            "turnGeneration": 2,
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": output}}]
            }
        })
    }

    #[test]
    fn a_message_that_fits_is_left_alone() {
        let message = tool_update("small".into());
        let (fitted, fit) = fit_message(message.clone(), FRAME_BUDGET_BYTES);
        assert_eq!(fit, Fit::Unchanged);
        assert_eq!(fitted, message);
    }

    #[test]
    fn a_long_tool_output_is_truncated_and_marked() {
        let output = "x".repeat(600 * 1024);
        let (fitted, fit) = fit_message(tool_update(output.clone()), FRAME_BUDGET_BYTES);
        let Fit::Truncated { dropped } = fit else {
            panic!("expected a truncation, got {fit:?}");
        };
        assert!(json_len(&fitted) <= FRAME_BUDGET_BYTES);
        assert!(dropped > 400 * 1024 && dropped < output.len());
        let text = fitted["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(output.starts_with(text.split('…').next().unwrap()));
        assert!(text.ends_with(&format!("[rebon rc: {dropped} bytes truncated]")));
        assert_eq!(
            fitted["update"]["_meta"]["rebonRc"]["truncatedBytes"],
            dropped
        );
        // Ids and the kind survive whole.
        assert_eq!(fitted["update"]["toolCallId"], "call-1");
        assert_eq!(fitted["sessionId"], "s-1");
    }

    #[test]
    fn many_long_strings_are_all_cut_down() {
        let parts: Vec<Value> = (0..8)
            .map(|index| json!({"type": "text", "text": format!("{index}").repeat(80 * 1024)}))
            .collect();
        let message =
            json!({"sessionId": "s", "update": {"sessionUpdate": "plan", "entries": parts}});
        let (fitted, fit) = fit_message(message, FRAME_BUDGET_BYTES);
        assert!(matches!(fit, Fit::Truncated { .. }), "{fit:?}");
        assert!(json_len(&fitted) <= FRAME_BUDGET_BYTES);
    }

    #[test]
    fn a_message_made_of_structure_is_omitted_with_a_stub() {
        let entries: Vec<Value> = (0..40_000).map(|index| json!({"n": index})).collect();
        let message = json!({
            "sessionId": "s",
            "turnGeneration": 4,
            "update": {"sessionUpdate": "plan", "entries": entries}
        });
        let original = json_len(&message);
        let (fitted, fit) = fit_message(message, FRAME_BUDGET_BYTES);
        assert_eq!(fit, Fit::Omitted { original });
        assert_eq!(
            fitted,
            json!({
                "sessionId": "s",
                "turnGeneration": 4,
                "update": {
                    "sessionUpdate": "plan",
                    "_meta": {"rebonRc": {"omittedBytes": original}}
                }
            })
        );
    }

    #[test]
    fn a_message_without_an_update_is_stubbed_too() {
        let message = json!({"a": (0..40_000).collect::<Vec<i32>>()});
        let original = json_len(&message);
        let (fitted, fit) = fit_message(message, 1024);
        assert_eq!(fit, Fit::Omitted { original });
        assert_eq!(
            fitted,
            json!({"_meta": {"rebonRc": {"omittedBytes": original}}})
        );
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        let output = "é".repeat(300 * 1024);
        let (fitted, fit) = fit_message(tool_update(output), FRAME_BUDGET_BYTES);
        assert!(matches!(fit, Fit::Truncated { .. }));
        // It serialized, so every string is still valid UTF-8, and it fits.
        assert!(json_len(&fitted) <= FRAME_BUDGET_BYTES);
    }

    #[test]
    fn text_splits_into_pieces_that_join_back() {
        assert_eq!(split_text("", 8), vec![""]);
        assert_eq!(split_text("abc", 8), vec!["abc"]);
        assert_eq!(split_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        let text = "日本語のテキスト".repeat(1000);
        let pieces = split_text(&text, 100);
        assert!(pieces.iter().all(|piece| piece.len() <= 100));
        assert_eq!(pieces.concat(), text);
        // A budget smaller than a character still makes progress.
        assert_eq!(split_text("日本", 1).concat(), "日本");
    }
}
