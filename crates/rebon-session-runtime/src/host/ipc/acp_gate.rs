//! What a connection is allowed to do, and when.
//!
//! ACP already has the rule: "clients must call `initialize` first; other
//! methods are rejected until then". The internal
//! control plane's authentication on that same gate rather than adding a
//! second one. The token rides in `_meta.rebon.token` of `initialize`, and it
//! is checked exactly once, at that moment.
//!
//! **Why once, and not per request.** The legacy protocol put a token on every
//! envelope because every envelope was its own connection — there was nothing
//! else to hang it on. ACP is connection-oriented: one accepted socket, then a
//! conversation. What the token defends against is *another process on this
//! machine connecting to this port*, and that is decided when the connection
//! is established. Re-checking a token the same peer keeps sending over the
//! same socket answers a question nobody asked.
//!
//! The job and session fences are a different thing and stay per request. They
//! do not ask "who are you"; they ask "did you mean *this* worker", which a
//! client can get wrong on any single message after a worker was replaced.
//!
//! **Fail-closed means closed.** A bad token does not get an error and another
//! chance. It gets one error and the connection ends: a peer that guessed
//! wrong must pay for a new connection before it can guess again, and a peer
//! that is simply out of date learns immediately rather than after a
//! conversation's worth of refusals.
//!
//! This module is a state machine over method names and params, with no I/O in
//! it. That is deliberate: every rule here is a rule about *ordering* and
//! *authority*, and those are exactly the things that are hard to test through
//! a socket and easy to test as a function.

use rebon_proto::types::{error_code, JsonRpcError};
use rebon_session_host::session_ext::{method, RebonMeta};
use rebon_types::constant_time_eq;

/// What the connection should do with a message.
///
/// `PartialEq` without `Eq` because a `JsonRpcError` carries a `Value`; see
/// the note on that type.
#[derive(Debug, PartialEq)]
pub enum Admission {
    /// Hand it to the method dispatcher.
    Dispatch,
    /// Answer this one here: it is `initialize`, and the gate is what knows
    /// whether it was acceptable.
    Initialized,
    /// Answer with this error and keep the connection.
    Reject(JsonRpcError),
    /// Answer with this error, then close. Only authentication reaches this.
    RejectAndClose(JsonRpcError),
}

/// Where a connection is in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing has been accepted yet. Only `initialize` may pass.
    AwaitingInitialize,
    /// The token was accepted. Methods dispatch.
    Ready,
    /// Authentication failed. Nothing more is admitted, whatever arrives.
    Closed,
}

/// The per-connection gate.
///
/// One of these lives for as long as one connection does, and it is the only
/// thing that knows whether that connection has been authenticated.
#[derive(Debug)]
pub struct AcpGate {
    phase: Phase,
    /// The token this worker's endpoint published. Compared in constant time,
    /// as the legacy path does: a token that leaks its length or its first
    /// wrong byte through timing is a token with fewer bits than it looks.
    expected_token: String,
}

/// The ACP method that opens a connection. Standard, not an extension, which
/// is why it is spelled out here rather than living in `session_ext::method`.
pub const INITIALIZE: &str = "initialize";

impl AcpGate {
    pub fn new(expected_token: impl Into<String>) -> Self {
        Self {
            phase: Phase::AwaitingInitialize,
            expected_token: expected_token.into(),
        }
    }

    /// Whether this connection may still carry anything at all.
    pub fn is_open(&self) -> bool {
        self.phase != Phase::Closed
    }

    /// Whether `initialize` has been accepted.
    pub fn is_ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    /// Decide what to do with one inbound request.
    ///
    /// `meta` is the `_meta` object of the message, whatever it was — this
    /// pulls the rebon half out itself rather than trusting a caller to have
    /// done it, because forgetting to would silently disable the token check.
    pub fn admit(&mut self, request_method: &str, meta: Option<&serde_json::Value>) -> Admission {
        match self.phase {
            Phase::Closed => Admission::RejectAndClose(JsonRpcError {
                code: error_code::UNAUTHENTICATED,
                message: "this connection was closed by a failed authentication".to_string(),
                data: Some(serde_json::json!({"kind": "unauthenticated"})),
            }),
            Phase::AwaitingInitialize if request_method == INITIALIZE => {
                let presented = RebonMeta::from_meta(meta).token.unwrap_or_default();
                // Constant time, and a missing token takes the same path as a
                // wrong one: "you sent none" and "you sent the wrong one" are
                // the same answer, and telling them apart helps only a guesser.
                if constant_time_eq(&presented, &self.expected_token) {
                    self.phase = Phase::Ready;
                    Admission::Initialized
                } else {
                    self.phase = Phase::Closed;
                    Admission::RejectAndClose(JsonRpcError {
                        code: error_code::UNAUTHENTICATED,
                        message: "the session owner did not accept this token".to_string(),
                        data: Some(serde_json::json!({"kind": "unauthenticated"})),
                    })
                }
            }
            // Anything before `initialize`. Not an authentication failure — the
            // peer has not claimed anything yet — so the connection survives
            // and it can still initialize properly.
            Phase::AwaitingInitialize => Admission::Reject(JsonRpcError::invalid_request(format!(
                "{request_method} was sent before initialize"
            ))),
            Phase::Ready if request_method == INITIALIZE => {
                // A second `initialize` is a client bug, and answering it as a
                // fresh handshake would let a peer that has already been
                // admitted re-present a token. Refuse without closing: the
                // connection is authenticated and its other work is fine.
                Admission::Reject(JsonRpcError::invalid_request(
                    "initialize was already accepted on this connection",
                ))
            }
            Phase::Ready => Admission::Dispatch,
        }
    }

    /// Whether a method name is one this owner implements at all.
    ///
    /// Separate from [`Self::admit`] because they answer different questions
    /// and fail at different times: an unknown method from an authenticated
    /// peer is a `-32601`, not a reason to doubt the peer.
    pub fn is_known_method(request_method: &str) -> bool {
        request_method == INITIALIZE
            || method::ALL.contains(&request_method)
            || matches!(
                request_method,
                "session/prompt" | "session/cancel" | "_session/steering"
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta_with_token(token: &str) -> serde_json::Value {
        json!({"rebon": {"token": token}})
    }

    fn gate() -> AcpGate {
        AcpGate::new("the-endpoint-token")
    }

    #[test]
    fn the_right_token_opens_the_connection() {
        let mut gate = gate();
        assert_eq!(
            gate.admit(INITIALIZE, Some(&meta_with_token("the-endpoint-token"))),
            Admission::Initialized
        );
        assert!(gate.is_ready());
        assert_eq!(gate.admit(method::PING, None), Admission::Dispatch);
    }

    /// Fail-closed, and *closed*: one error, then nothing. A peer that guessed
    /// wrong pays for a new connection before it can guess again.
    #[test]
    fn a_wrong_token_ends_the_connection() {
        let mut gate = gate();
        let verdict = gate.admit(INITIALIZE, Some(&meta_with_token("not-the-token")));
        match verdict {
            Admission::RejectAndClose(error) => {
                assert_eq!(error.code, error_code::UNAUTHENTICATED);
            }
            other => panic!("a wrong token must close the connection, got {other:?}"),
        }
        assert!(!gate.is_open());
        assert!(!gate.is_ready());
        // And it stays closed, including to a correct token.
        assert!(matches!(
            gate.admit(INITIALIZE, Some(&meta_with_token("the-endpoint-token"))),
            Admission::RejectAndClose(_)
        ));
        assert!(matches!(
            gate.admit(method::PING, None),
            Admission::RejectAndClose(_)
        ));
    }

    /// No token is the same answer as a wrong token. Telling them apart tells
    /// a guesser whether the field name was right, which is a free hint.
    #[test]
    fn a_missing_token_is_answered_exactly_like_a_wrong_one() {
        let mut absent = gate();
        let mut wrong = gate();
        let without = absent.admit(INITIALIZE, None);
        let bad = wrong.admit(INITIALIZE, Some(&meta_with_token("not-the-token")));
        match (without, bad) {
            (Admission::RejectAndClose(left), Admission::RejectAndClose(right)) => {
                assert_eq!(left.code, right.code);
                assert_eq!(left.message, right.message);
                assert_eq!(left.data, right.data);
            }
            other => panic!("both must close with the same answer, got {other:?}"),
        }
    }

    /// An `_meta` that carries someone else's keys but no rebon token is a
    /// missing token, not a decode failure that might read as something else.
    #[test]
    fn someone_elses_meta_is_a_missing_token() {
        let mut gate = gate();
        let foreign = json!({"steering": {"supported": true}});
        assert!(matches!(
            gate.admit(INITIALIZE, Some(&foreign)),
            Admission::RejectAndClose(_)
        ));
    }

    /// Before `initialize` nothing dispatches — the rule ACP already has, and
    /// the reason the token can hang on that one moment.
    #[test]
    fn nothing_dispatches_before_initialize() {
        let mut gate = gate();
        match gate.admit(method::STATUS, None) {
            Admission::Reject(error) => {
                assert_eq!(error.code, error_code::INVALID_REQUEST);
            }
            other => panic!("expected a rejection that keeps the connection, got {other:?}"),
        }
        // The connection survives, so a client that sent something early can
        // still initialize properly.
        assert!(gate.is_open());
        assert_eq!(
            gate.admit(INITIALIZE, Some(&meta_with_token("the-endpoint-token"))),
            Admission::Initialized
        );
    }

    /// A second `initialize` must not be a second chance to present a token.
    #[test]
    fn initialize_is_not_a_door_that_reopens() {
        let mut gate = gate();
        assert_eq!(
            gate.admit(INITIALIZE, Some(&meta_with_token("the-endpoint-token"))),
            Admission::Initialized
        );
        assert!(matches!(
            gate.admit(INITIALIZE, Some(&meta_with_token("not-the-token"))),
            Admission::Reject(_)
        ));
        // Refused, but the connection was already good and stays good.
        assert!(gate.is_ready());
        assert_eq!(gate.admit(method::PING, None), Admission::Dispatch);
    }

    /// An unknown method is a `-32601` from an authenticated peer, not a
    /// reason to doubt the peer. The gate does not decide it at all.
    #[test]
    fn the_gate_does_not_judge_whether_a_method_exists() {
        let mut gate = gate();
        gate.admit(INITIALIZE, Some(&meta_with_token("the-endpoint-token")));
        assert_eq!(gate.admit("_session/nonsense", None), Admission::Dispatch);
        assert!(!AcpGate::is_known_method("_session/nonsense"));
    }

    /// Every extension this build declares is a method it will route, and so
    /// are the standard ones the plan maps onto.
    #[test]
    fn every_declared_method_is_known() {
        for name in method::ALL {
            assert!(AcpGate::is_known_method(name), "{name} would not route");
        }
        for name in [
            INITIALIZE,
            "session/prompt",
            "session/cancel",
            "_session/steering",
        ] {
            assert!(AcpGate::is_known_method(name), "{name} would not route");
        }
    }

    /// A prefix of the token is not the token.
    ///
    /// This pins the outcome and nothing more: it cannot show that the
    /// comparison is constant time, because a plain `==` rejects a prefix too.
    /// The timing property comes from calling `constant_time_eq`, which is the
    /// same guarantee — and the same unprovable-by-test guarantee — the legacy
    /// envelope path has.
    #[test]
    fn a_prefix_of_the_token_is_not_the_token() {
        let mut gate = gate();
        assert!(matches!(
            gate.admit(INITIALIZE, Some(&meta_with_token("the-endpoint"))),
            Admission::RejectAndClose(_)
        ));
    }
}
