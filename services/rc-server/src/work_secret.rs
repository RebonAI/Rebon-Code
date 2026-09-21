//! Minting the opaque `WorkResponse.secret` blob.
//!
//! The *shape* and its codec live in
//! [`rebon_bridge::work_secret::WorkSecret`] — RFC-0008 §4 makes
//! `rebon-bridge` the single definition point of the wire protocol, and a
//! second copy here would be exactly the hand-copied structure that rule
//! exists to prevent. What is genuinely server-side is [`mint`]: only RC
//! knows the ingress origin and only RC issues session tokens.
//!
//! ```json
//! {
//!   "session_token": "<43-character base64url>",
//!   "session_id": "sess_…",
//!   "ingress_url": "wss://rc.example.com/v1/sessions/sess_…/stream"
//! }
//! ```
//!
//! `session_id` is absent for a healthcheck item, which has no session;
//! `ingress_url` is then the ingress origin with no session path. For
//! session work it is the address of [`crate::routes::stream`], and the
//! session token next to it is the credential that opens it.

pub use rebon_bridge::work_secret::WorkSecret;

/// Build the secret for a claimed work item.
///
/// `ingress_origin` is `REBON_RC_SESSION_INGRESS_URL`; the session path is
/// appended for session work and omitted for a healthcheck, which has no
/// session to attach to.
pub fn mint(ingress_origin: &str, session_token: String, session_id: Option<String>) -> WorkSecret {
    let origin = ingress_origin.trim_end_matches('/');
    let ingress_url = match &session_id {
        Some(session_id) => format!("{origin}/v1/sessions/{session_id}/stream"),
        None => origin.to_string(),
    };
    WorkSecret {
        session_token,
        session_id,
        ingress_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_secret_points_at_the_session_stream() {
        let secret = mint(
            "wss://rc.example.com/",
            "token-value".into(),
            Some("sess_abc".into()),
        );
        assert_eq!(
            secret.ingress_url,
            "wss://rc.example.com/v1/sessions/sess_abc/stream"
        );
        // Round-trips through the codec `rebon-bridge` owns.
        assert_eq!(
            WorkSecret::decode(&secret.encode()).expect("decodes"),
            secret
        );
    }

    #[test]
    fn a_healthcheck_secret_is_the_bare_origin() {
        let secret = mint("wss://rc.example.com", "token-value".into(), None);
        assert_eq!(secret.ingress_url, "wss://rc.example.com");
        assert!(secret.session_id.is_none());
    }
}
