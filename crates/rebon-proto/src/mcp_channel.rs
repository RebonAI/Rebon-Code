//! The MCP channel extension: an MCP server pushing a message into the
//! session that connected to it, unasked.
//!
//! Not ACP. Rebon speaks it in both directions — a host that *reads* channel
//! notifications from the servers a session connects to, and a server that
//! *writes* them to whoever connected to it — and both halves need the same
//! method, capability key, meta-key rule and params shape. That agreement is
//! one fact, so it is written once, next to the codec.
//!
//! The shape is the one Claude Code clients speak, so a Rebon server
//! interoperates with them; it is observed behaviour, not a published
//! specification.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// Declared under `capabilities.experimental` by a server that pushes. Its
/// presence is what makes a host register a listener at all.
pub const CHANNEL_CAPABILITY: &str = "claude/channel";
/// Declared by a server that also relays permission prompts.
pub const CHANNEL_PERMISSION_CAPABILITY: &str = "claude/channel/permission";
/// Server → host: one pushed message.
pub const CHANNEL_NOTIFICATION_METHOD: &str = "notifications/claude/channel";
/// Server → host: the answer to a relayed permission prompt.
pub const CHANNEL_PERMISSION_METHOD: &str = "notifications/claude/channel/permission";
/// Host → server: a permission prompt the host wants relayed.
pub const CHANNEL_PERMISSION_REQUEST_METHOD: &str =
    "notifications/claude/channel/permission_request";

/// Whether `key` may become an attribute on the rendered `<channel>` tag.
///
/// `^[a-zA-Z_][a-zA-Z0-9_]*$`: the host renders each meta key verbatim as an
/// XML attribute name, so anything else would let a server break out of the
/// tag. Hosts drop keys that fail this; writers never emit them.
pub fn is_safe_meta_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// One pushed message: a body and the attributes the host renders on the tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMessage {
    pub content: String,
    /// Ordered so a message encodes to the same bytes every time.
    pub meta: BTreeMap<String, String>,
}

/// Why a `notifications/claude/channel` params object is not a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMessageError {
    /// `content` is missing or is not a string.
    ContentNotString,
}

impl std::fmt::Display for ChannelMessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContentNotString => write!(f, "channel notification content is not a string"),
        }
    }
}

impl std::error::Error for ChannelMessageError {}

impl ChannelMessage {
    /// The whole JSON-RPC notification, ready to frame.
    ///
    /// A meta key that fails [`is_safe_meta_key`] is a writer bug: it is
    /// dropped here rather than sent for the host to drop.
    pub fn to_notification(&self) -> Value {
        let meta: serde_json::Map<String, Value> = self
            .meta
            .iter()
            .filter(|(key, _)| is_safe_meta_key(key))
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        json!({
            "jsonrpc": "2.0",
            "method": CHANNEL_NOTIFICATION_METHOD,
            "params": { "content": self.content, "meta": meta },
        })
    }

    /// Read the `params` of a received notification.
    ///
    /// Meta entries whose value is not a string are dropped; keys are kept
    /// as sent, because rejecting unsafe ones is the renderer's job and it
    /// has to happen there anyway.
    pub fn from_params(params: &Value) -> Result<Self, ChannelMessageError> {
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or(ChannelMessageError::ContentNotString)?
            .to_string();
        let meta = params
            .get("meta")
            .and_then(Value::as_object)
            .map(|object| {
                object
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self { content, meta })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(meta: &[(&str, &str)]) -> ChannelMessage {
        ChannelMessage {
            content: "job bg-1 finished (succeeded).".into(),
            meta: meta
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn meta_keys_follow_the_attribute_name_rule() {
        for good in ["job_id", "_x", "A1", "state"] {
            assert!(is_safe_meta_key(good), "{good}");
        }
        for bad in ["", "1a", "bad-key", "a b", "a\"", "é", "x:y"] {
            assert!(!is_safe_meta_key(bad), "{bad}");
        }
    }

    #[test]
    fn a_message_round_trips_through_its_notification() {
        let sent = message(&[("job_id", "bg-1"), ("state", "succeeded")]);
        let notification = sent.to_notification();
        assert_eq!(notification["jsonrpc"], "2.0");
        assert_eq!(notification["method"], CHANNEL_NOTIFICATION_METHOD);
        assert!(notification.get("id").is_none(), "a notification has no id");
        let received = ChannelMessage::from_params(&notification["params"]).unwrap();
        assert_eq!(received, sent);
    }

    #[test]
    fn encoding_is_stable_and_drops_unsafe_keys() {
        let sent = ChannelMessage {
            content: "x".into(),
            meta: [("b", "2"), ("a", "1")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let bytes = serde_json::to_string(&sent.to_notification()).unwrap();
        assert_eq!(
            bytes,
            r#"{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"x","meta":{"a":"1","b":"2"}}}"#
        );
    }

    #[test]
    fn an_unsafe_key_never_reaches_the_wire() {
        let notification = message(&[("bad-key", "x"), ("ok", "y")]).to_notification();
        assert!(notification["params"]["meta"].get("bad-key").is_none());
        assert_eq!(notification["params"]["meta"]["ok"], "y");
    }

    #[test]
    fn decoding_keeps_string_meta_and_refuses_non_string_content() {
        let params = json!({
            "content": "hello",
            "meta": { "chat_id": "c1", "count": 3, "bad-key": "kept" },
        });
        let received = ChannelMessage::from_params(&params).unwrap();
        assert_eq!(received.content, "hello");
        assert_eq!(received.meta.get("chat_id").map(String::as_str), Some("c1"));
        assert!(
            !received.meta.contains_key("count"),
            "non-string values drop"
        );
        assert!(
            received.meta.contains_key("bad-key"),
            "key filtering is the renderer's job"
        );

        assert_eq!(
            ChannelMessage::from_params(&json!({ "content": 123 })),
            Err(ChannelMessageError::ContentNotString)
        );
        assert_eq!(
            ChannelMessage::from_params(&json!({})),
            Err(ChannelMessageError::ContentNotString)
        );
        let bare = ChannelMessage::from_params(&json!({ "content": "x" })).unwrap();
        assert!(bare.meta.is_empty(), "meta is optional");
    }
}
