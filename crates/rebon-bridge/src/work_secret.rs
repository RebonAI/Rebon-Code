//! The decoded form of the opaque `WorkResponse.secret` blob.
//!
//! [`crate::config::WorkResponse::secret`] is declared as "base64url-encoded
//! JSON that decodes into a work secret". The shape is defined here, and the
//! server imports it rather than keeping a private copy that could drift.
//!
//! ```json
//! {
//!   "session_token": "<43-character base64url>",
//!   "session_id": "sess_…",
//!   "ingress_url": "wss://rc.example.com/v1/sessions/sess_…/stream"
//! }
//! ```
//!
//! `session_id` is **absent** for a healthcheck item, which has no session;
//! `ingress_url` is then just the ingress origin. Minting the blob (choosing
//! the ingress origin and the session token) is the server's business and
//! stays there — this module owns only the shape and its codec.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

/// Decoded form of [`crate::config::WorkResponse::secret`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSecret {
    /// Credential scoped to this work item's session.
    pub session_token: String,
    /// Session the work item belongs to; absent for a healthcheck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// WebSocket URL the worker attaches the session stream to.
    pub ingress_url: String,
}

impl WorkSecret {
    /// Encode to the wire form carried in `WorkResponse.secret`:
    /// base64url without padding over the JSON above.
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("work secret is serializable"))
    }

    /// Decode the wire form. Returns `None` for anything that is not
    /// base64url or does not carry the required fields — the client
    /// treats a secret it cannot read as a protocol failure, not as a
    /// half-usable value.
    pub fn decode(encoded: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_secret() -> WorkSecret {
        WorkSecret {
            session_token: "token-value".into(),
            session_id: Some("sess_abc".into()),
            ingress_url: "wss://rc.example.com/v1/sessions/sess_abc/stream".into(),
        }
    }

    #[test]
    fn a_session_secret_round_trips() {
        let secret = session_secret();
        let decoded = WorkSecret::decode(&secret.encode()).expect("decodes");
        assert_eq!(decoded, secret);
    }

    #[test]
    fn a_healthcheck_secret_omits_the_session_id() {
        let secret = WorkSecret {
            session_token: "token-value".into(),
            session_id: None,
            ingress_url: "wss://rc.example.com".into(),
        };
        let encoded = secret.encode();
        let json = String::from_utf8(URL_SAFE_NO_PAD.decode(&encoded).expect("base64url decodes"))
            .expect("utf-8");
        assert!(!json.contains("session_id"), "{json}");
        assert_eq!(WorkSecret::decode(&encoded).expect("decodes"), secret);
    }

    #[test]
    fn the_wire_form_is_unpadded_base64url_json() {
        let encoded = session_secret().encode();
        assert!(!encoded.contains('='), "{encoded}");
        assert!(
            !encoded.contains('+') && !encoded.contains('/'),
            "{encoded}"
        );
        let json: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&encoded).expect("base64url"))
                .expect("json");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["ingress_url", "session_id", "session_token"]);
    }

    #[test]
    fn a_corrupt_secret_decodes_to_nothing() {
        assert!(WorkSecret::decode("not base64!").is_none());
        assert!(WorkSecret::decode(&URL_SAFE_NO_PAD.encode("{}")).is_none());
    }
}
