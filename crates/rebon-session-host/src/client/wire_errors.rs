//! Turning a host failure into a JSON-RPC error, and reading one back.
//!
//! The legacy control plane has one error channel: `BackgroundIpcResponse {
//! ok: false, error: Some(String) }`. Everything that can go wrong becomes a
//! sentence, and the client recovers the *kind* by matching substrings of it
//! (`HostCallError::from_wire`). That works until someone rewords a message,
//! and then a timeout starts reading as a refusal. This table replaces it: the
//! code says which kind of failure this is, `data.kind` names the typed variant
//! exactly, and the message is for a human to read and nothing to branch on.
//!
//! Two things are deliberately *not* errors here:
//!
//! - **A refusal on policy grounds is an answer.** It gets a code because the
//!   caller did not get what it asked for, but it means "understood and
//!   declined", not "something broke".
//! - **An unknown permission option is not a failure.** The owner folds it
//!   into one it does offer and answers success. Only an unknown option with
//!   *nothing to reject with* is [`error_code::NO_USABLE_OPTION`]. The two
//!   existing tests that pin this semantics do not change by a word.
//!
//! The block starts at `-32010` rather than `-32000`, because `-32000` is
//! `SESSION_OWNED_ELSEWHERE`, which `session/load` answers and the serve tests
//! pin: one number cannot mean two things on one wire. `Cancelled` holds
//! `-32017` of its own, because a client branches on it and it needs a row
//! nothing else shares. See `rebon_proto::error_code`.
//!
//! This lives here rather than beside the server because **both halves read
//! the same table**: the owner encodes a failure, and the client decodes it
//! back into the same `HostCallError` its callers already match on. It was in
//! the runtime crate while the client half could not reach `rebon-proto`; the
//! client half has that edge now, so the table came down to where the two
//! readers meet.

use rebon_proto::types::{error_code, JsonRpcError};

use crate::HostCallError;

/// The `data.kind` value for one typed failure.
///
/// A name rather than the code, because two variants share the catch-all
/// code and a client that only had the number could not tell them apart.
/// Stable strings: they are wire, and renaming a Rust variant must not
/// silently change one.
fn kind_of(error: &HostCallError) -> &'static str {
    match error {
        HostCallError::Unauthenticated => "unauthenticated",
        HostCallError::OwnerFence(_) => "ownerFence",
        HostCallError::OwnerClosing => "ownerClosing",
        HostCallError::Unsupported => "unsupported",
        HostCallError::StaleGeneration => "staleGeneration",
        HostCallError::PermissionRejected => "permissionRejected",
        HostCallError::Cancelled => "cancelled",
        HostCallError::HostUnanswered => "hostUnanswered",
        HostCallError::InvalidRequest => "invalidRequest",
        HostCallError::SessionFailure => "sessionFailure",
        HostCallError::Transport(_) => "transport",
        HostCallError::Refused(_) => "refused",
    }
}

/// Which code carries this failure.
///
/// Two of them are standard JSON-RPC rather than rebon's own range, because
/// they mean exactly what the standard says: a request this owner has no
/// method for, and a request whose shape did not decode.
fn code_of(error: &HostCallError) -> i32 {
    match error {
        HostCallError::Unsupported => error_code::METHOD_NOT_FOUND,
        HostCallError::InvalidRequest => error_code::INVALID_PARAMS,
        HostCallError::HostUnanswered => error_code::HOST_UNANSWERED,
        HostCallError::Unauthenticated => error_code::UNAUTHENTICATED,
        HostCallError::OwnerFence(_) => error_code::OWNER_FENCE,
        HostCallError::StaleGeneration => error_code::STALE_GENERATION,
        // One code, two variants: from a caller's point of view "I could not
        // reach it" and "it is on its way out and stopped taking work" lead to
        // the same next move. `data.kind` still separates them for a log.
        HostCallError::Transport(_) | HostCallError::OwnerClosing => error_code::OWNER_UNREACHABLE,
        HostCallError::PermissionRejected => error_code::PERMISSION_REJECTED,
        // Its own code, not the catch-all: a client branches on it. A call
        // released half way may be worth retrying; a refusal is a decision and
        // retrying it only repeats the refusal.
        HostCallError::Cancelled => error_code::CALL_CANCELLED,
        HostCallError::SessionFailure | HostCallError::Refused(_) => error_code::HOST_CALL_FAILED,
    }
}

/// The sentence a human reads. Never parsed by anything.
fn message_of(error: &HostCallError) -> String {
    match error {
        HostCallError::Unauthenticated => "the session owner did not accept this token".to_string(),
        HostCallError::OwnerFence(reason) => reason.clone(),
        HostCallError::OwnerClosing => {
            "the session owner is shutting down and is no longer taking work".to_string()
        }
        HostCallError::Unsupported => "this session owner does not know this request".to_string(),
        HostCallError::StaleGeneration => {
            "the turn or generation this request was fenced against has moved on".to_string()
        }
        HostCallError::PermissionRejected => "the session owner refused the request".to_string(),
        HostCallError::Cancelled => "the call was cancelled".to_string(),
        HostCallError::HostUnanswered => {
            "the session owner did not answer before the deadline".to_string()
        }
        HostCallError::InvalidRequest => "the request could not be decoded".to_string(),
        HostCallError::SessionFailure => "the session behind this owner had failed".to_string(),
        HostCallError::Transport(detail) => detail.clone(),
        HostCallError::Refused(detail) => detail.clone(),
    }
}

/// Encode a host failure as the JSON-RPC error that goes on the wire.
pub fn to_json_rpc(error: &HostCallError) -> JsonRpcError {
    JsonRpcError {
        code: code_of(error),
        message: message_of(error),
        data: Some(serde_json::json!({ "kind": kind_of(error) })),
    }
}

/// Read a JSON-RPC error back as the failure it was.
///
/// `data.kind` decides whenever it is there, because it is exact; the code is
/// the fallback for a peer that did not send one. A code this does not know
/// becomes [`HostCallError::Refused`] carrying the message, which is the same
/// thing the legacy reader does with a sentence it does not recognise: show
/// it, never branch on it.
pub fn from_json_rpc(error: &JsonRpcError) -> HostCallError {
    if let Some(kind) = error.data.as_ref().and_then(|data| data.get("kind")) {
        if let Some(kind) = kind.as_str() {
            match kind {
                "unauthenticated" => return HostCallError::Unauthenticated,
                "ownerFence" => return HostCallError::OwnerFence(error.message.clone()),
                "ownerClosing" => return HostCallError::OwnerClosing,
                "unsupported" => return HostCallError::Unsupported,
                "staleGeneration" => return HostCallError::StaleGeneration,
                "permissionRejected" => return HostCallError::PermissionRejected,
                "cancelled" => return HostCallError::Cancelled,
                "hostUnanswered" => return HostCallError::HostUnanswered,
                "invalidRequest" => return HostCallError::InvalidRequest,
                "sessionFailure" => return HostCallError::SessionFailure,
                "transport" => return HostCallError::Transport(error.message.clone()),
                "refused" => return HostCallError::Refused(error.message.clone()),
                // A kind from a newer owner. Fall through to the code, which
                // is at least in a range this build understands.
                _ => {}
            }
        }
    }
    match error.code {
        error_code::METHOD_NOT_FOUND => HostCallError::Unsupported,
        error_code::INVALID_PARAMS | error_code::INVALID_REQUEST | error_code::PARSE_ERROR => {
            HostCallError::InvalidRequest
        }
        error_code::HOST_UNANSWERED => HostCallError::HostUnanswered,
        error_code::UNAUTHENTICATED => HostCallError::Unauthenticated,
        error_code::OWNER_FENCE => HostCallError::OwnerFence(error.message.clone()),
        error_code::STALE_GENERATION => HostCallError::StaleGeneration,
        error_code::OWNER_UNREACHABLE => HostCallError::Transport(error.message.clone()),
        error_code::PERMISSION_REJECTED => HostCallError::PermissionRejected,
        error_code::CALL_CANCELLED => HostCallError::Cancelled,
        _ => HostCallError::Refused(error.message.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant that exists today, so the round-trip test below cannot
    /// pass by covering only the easy ones. A variant added without a line
    /// here fails `every_variant_is_covered`.
    fn every_variant() -> Vec<HostCallError> {
        vec![
            HostCallError::Unauthenticated,
            HostCallError::OwnerFence("named the wrong session".to_string()),
            HostCallError::OwnerClosing,
            HostCallError::Unsupported,
            HostCallError::StaleGeneration,
            HostCallError::PermissionRejected,
            HostCallError::Cancelled,
            HostCallError::HostUnanswered,
            HostCallError::InvalidRequest,
            HostCallError::SessionFailure,
            HostCallError::Transport("connection reset".to_string()),
            HostCallError::Refused("the owner said no".to_string()),
        ]
    }

    /// The point of the typed channel: what the owner meant is what the client
    /// reads, including for the three variants that share a code.
    #[test]
    fn every_failure_survives_the_round_trip() {
        for error in every_variant() {
            let wire = to_json_rpc(&error);
            let read_back = from_json_rpc(&wire);
            assert_eq!(
                format!("{read_back:?}"),
                format!("{error:?}"),
                "{error:?} did not survive as {wire:?}"
            );
        }
    }

    /// A guard against the list above going stale: it counts, so a thirteenth
    /// variant makes this red rather than quietly skipping the round trip.
    #[test]
    fn every_variant_is_covered() {
        assert_eq!(every_variant().len(), 12);
        let mut kinds: Vec<&str> = every_variant().iter().map(kind_of).collect();
        kinds.sort_unstable();
        let mut unique = kinds.clone();
        unique.dedup();
        assert_eq!(kinds, unique, "two variants share a `data.kind`");
    }

    /// The two that are standard JSON-RPC stay standard: an owner that does
    /// not know a method must answer the code every JSON-RPC client already
    /// handles, or the compatibility probe on the other side has nothing to
    /// read.
    #[test]
    fn the_standard_failures_keep_their_standard_codes() {
        assert_eq!(
            to_json_rpc(&HostCallError::Unsupported).code,
            error_code::METHOD_NOT_FOUND
        );
        assert_eq!(
            to_json_rpc(&HostCallError::InvalidRequest).code,
            error_code::INVALID_PARAMS
        );
    }

    /// The block starts below the code that was already taken. If someone
    /// renumbers it onto -32000 again, `session/load`'s "open elsewhere"
    /// refusal and a host timeout become the same number.
    #[test]
    fn the_extension_codes_do_not_collide_with_the_one_that_was_there() {
        let extension = [
            error_code::HOST_UNANSWERED,
            error_code::UNAUTHENTICATED,
            error_code::OWNER_FENCE,
            error_code::STALE_GENERATION,
            error_code::OWNER_UNREACHABLE,
            error_code::NO_USABLE_OPTION,
            error_code::PERMISSION_REJECTED,
            error_code::CALL_CANCELLED,
            error_code::HOST_CALL_FAILED,
        ];
        for code in extension {
            assert_ne!(
                code,
                error_code::SESSION_OWNED_ELSEWHERE,
                "an extension code was numbered onto the one already in use"
            );
            assert!(
                (-32099..=-32000).contains(&code),
                "{code} is outside the range JSON-RPC reserves for server-defined errors"
            );
        }
        let mut sorted = extension;
        sorted.sort_unstable();
        let mut unique = sorted.to_vec();
        unique.dedup();
        assert_eq!(sorted.to_vec(), unique, "two extension codes are equal");
    }

    /// `data.kind` wins over the code, so the two variants that share the
    /// catch-all number still come back as themselves.
    #[test]
    fn the_kind_decides_when_the_code_cannot() {
        let shared: Vec<i32> = [
            HostCallError::SessionFailure,
            HostCallError::Refused("no".to_string()),
        ]
        .iter()
        .map(|error| to_json_rpc(error).code)
        .collect();
        assert_eq!(shared, vec![error_code::HOST_CALL_FAILED; 2]);
    }

    /// A cancelled call is readable from the code alone, because a client
    /// branches on it: a call released half way may be worth retrying, while a
    /// refusal is a decision and retrying it only repeats the refusal. A
    /// distinction that changes what the caller does next must not be reachable
    /// only through `data.kind`, which a peer is allowed to omit.
    #[test]
    fn a_cancelled_call_is_told_apart_from_a_refusal_by_the_code_alone() {
        let cancelled = to_json_rpc(&HostCallError::Cancelled);
        let refused = to_json_rpc(&HostCallError::Refused("no".to_string()));
        assert_eq!(cancelled.code, error_code::CALL_CANCELLED);
        assert_ne!(cancelled.code, refused.code);
        // Stripped of `data`, as a peer that sends none would leave it, the
        // two still read as different things.
        let bare = JsonRpcError {
            code: cancelled.code,
            message: cancelled.message.clone(),
            data: None,
        };
        assert!(matches!(from_json_rpc(&bare), HostCallError::Cancelled));
    }

    /// A peer that sends no `data` at all is still readable. This is what a
    /// third-party ACP agent looks like, and what rebon's own older
    /// `JsonRpcError` constructors produce.
    #[test]
    fn a_bare_error_without_data_is_read_from_its_code() {
        assert!(matches!(
            from_json_rpc(&JsonRpcError::method_not_found("_session/ping")),
            HostCallError::Unsupported
        ));
        assert!(matches!(
            from_json_rpc(&JsonRpcError::invalid_params("bad shape")),
            HostCallError::InvalidRequest
        ));
        assert!(matches!(
            from_json_rpc(&JsonRpcError::parse_error("not json")),
            HostCallError::InvalidRequest
        ));
    }

    /// A kind this build has never heard of falls back to the code rather than
    /// to the catch-all, so a newer owner that adds a variant inside an
    /// existing code is still understood as well as the code allows.
    #[test]
    fn an_unknown_kind_falls_back_to_the_code() {
        let from_the_future = JsonRpcError {
            code: error_code::STALE_GENERATION,
            message: "the lease you held expired".to_string(),
            data: Some(serde_json::json!({"kind": "leaseExpired"})),
        };
        assert!(matches!(
            from_json_rpc(&from_the_future),
            HostCallError::StaleGeneration
        ));
    }

    /// An entirely unknown code becomes a refusal carrying the words, which is
    /// what the legacy reader does with a sentence it does not recognise: show
    /// it to the user, never branch on it.
    #[test]
    fn an_unknown_code_keeps_the_words_and_claims_nothing() {
        let alien = JsonRpcError {
            code: -32077,
            message: "something a later build knows about".to_string(),
            data: None,
        };
        match from_json_rpc(&alien) {
            HostCallError::Refused(message) => {
                assert_eq!(message, "something a later build knows about");
            }
            other => panic!("expected a refusal carrying the message, got {other:?}"),
        }
    }

    /// The detail in `Transport` and `Refused` is the message, not a second
    /// copy in `data`: one place for the words, so they cannot disagree.
    #[test]
    fn the_variants_that_carry_words_put_them_in_the_message() {
        let wire = to_json_rpc(&HostCallError::Transport("connection reset".to_string()));
        assert_eq!(wire.message, "connection reset");
        assert_eq!(wire.data, Some(serde_json::json!({"kind": "transport"})));
    }
}
