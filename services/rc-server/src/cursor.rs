//! Opaque, authenticated page cursors.
//!
//! Both paged routes hand out a `next_cursor` that the client must send
//! back verbatim. What it encodes is RC's business, and keeping it that
//! way is the point: a raw `event_id` in the contract would invite
//! clients to do arithmetic on it, and would freeze the pagination key
//! forever.
//!
//! ## Format
//!
//! Unpadded base64url of
//!
//! ```text
//! version (1) ‖ kind (1) ‖ body ‖ tag (16)
//! ```
//!
//! | kind | route | body |
//! |---|---|---|
//! | `1` | `GET /v1/sessions/{s}/events` | the oldest `event_id` already returned, `i64` big-endian |
//! | `2` | `GET /v1/sessions` | the last row's activity time (`i64` BE) ‖ its session id (UTF-8) |
//!
//! `tag` is the first 16 bytes of an HMAC-SHA256 under the server's
//! token key and its own domain separator, over the version, the kind,
//! the **scope** (length-prefixed) and the body. The scope is what the
//! cursor is only valid for — the session for an event cursor; the
//! account and the `environment` filter for a session-list cursor — so
//! a cursor replayed against another session, account or filter fails
//! verification rather than silently walking something else.
//!
//! Anything that does not verify is simply refused: the routes answer
//! 400. The tag is not there for secrecy — the body is readable by
//! anyone who decodes it — but so a client cannot *mint* a position.
//!
//! Positions are exclusive bounds compared with `<`, never looked up,
//! so a cursor whose row has since been deleted (retention) still
//! resumes at the right place.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use subtle::ConstantTimeEq;

use crate::ids;

/// Domain separator for cursor tags. Distinct from every credential
/// domain, so a tag can never double as a credential digest.
pub const DOMAIN_CURSOR: &[u8] = b"rebon-rc-cursor-v1";

const VERSION: u8 = 1;
const KIND_EVENTS: u8 = 1;
const KIND_SESSIONS: u8 = 2;
const TAG_BYTES: usize = 16;
/// Longest session id a session-list cursor will carry. Session ids are
/// `sess_` plus 22 characters today; controllers may supply their own on
/// enqueue, so the bound is generous but finite.
const MAX_SESSION_ID_BYTES: usize = 256;
/// Longest encoded cursor accepted, checked before decoding anything.
pub const MAX_CURSOR_CHARS: usize = 512;

/// Why a presented cursor was refused. The routes collapse every
/// variant into the same 400; the distinction exists for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorError {
    /// Too long, not canonical base64url, or too short to hold a tag.
    Malformed,
    /// A version or kind this build does not issue.
    Unsupported,
    /// The tag does not verify under this scope.
    Forged,
}

/// Where a session-list walk resumes: strictly after this row in
/// `(last_activity DESC, session_id DESC)` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPosition {
    /// `sessions.updated_at_unix` of the last row returned.
    pub updated_at_unix: i64,
    /// `sessions.session_id` of the last row returned.
    pub session_id: String,
}

/// Scope of an event cursor: the session it walks.
fn events_scope(session_id: &str) -> Vec<u8> {
    session_id.as_bytes().to_vec()
}

/// Scope of a session-list cursor: the account, and the filter. The
/// NUL separator cannot occur in either id, and the explicit marker for
/// "no filter" keeps an unfiltered cursor from matching a filter whose
/// id happens to be empty.
fn sessions_scope(account_id: &str, environment_id: Option<&str>) -> Vec<u8> {
    let mut scope = account_id.as_bytes().to_vec();
    match environment_id {
        None => scope.push(0),
        Some(environment_id) => {
            scope.push(1);
            scope.extend_from_slice(environment_id.as_bytes());
        }
    }
    scope
}

fn tag(key: &[u8; 32], kind: u8, scope: &[u8], body: &[u8]) -> [u8; TAG_BYTES] {
    let mut message = Vec::with_capacity(2 + 4 + scope.len() + body.len());
    message.push(VERSION);
    message.push(kind);
    message.extend_from_slice(&(scope.len() as u32).to_be_bytes());
    message.extend_from_slice(scope);
    message.extend_from_slice(body);
    let digest = ids::domain_digest(key, DOMAIN_CURSOR, &message);
    let mut truncated = [0u8; TAG_BYTES];
    truncated.copy_from_slice(&digest[..TAG_BYTES]);
    truncated
}

fn seal(key: &[u8; 32], kind: u8, scope: &[u8], body: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(2 + body.len() + TAG_BYTES);
    bytes.push(VERSION);
    bytes.push(kind);
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(&tag(key, kind, scope, body));
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode and verify, returning the body.
fn open(key: &[u8; 32], kind: u8, scope: &[u8], cursor: &str) -> Result<Vec<u8>, CursorError> {
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_CHARS {
        return Err(CursorError::Malformed);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| CursorError::Malformed)?;
    // Only the canonical spelling is accepted: base64 tolerates some
    // trailing-bit variation, and one position must have one cursor.
    if URL_SAFE_NO_PAD.encode(&bytes) != cursor {
        return Err(CursorError::Malformed);
    }
    if bytes.len() < 2 + TAG_BYTES {
        return Err(CursorError::Malformed);
    }
    if bytes[0] != VERSION || bytes[1] != kind {
        return Err(CursorError::Unsupported);
    }
    let (signed, presented) = bytes.split_at(bytes.len() - TAG_BYTES);
    let body = &signed[2..];
    let expected = tag(key, kind, scope, body);
    if !bool::from(expected.ct_eq(presented)) {
        return Err(CursorError::Forged);
    }
    Ok(body.to_vec())
}

/// Cursor for the event page after one whose oldest event is
/// `oldest_event_id`.
pub fn encode_events(key: &[u8; 32], session_id: &str, oldest_event_id: i64) -> String {
    seal(
        key,
        KIND_EVENTS,
        &events_scope(session_id),
        &oldest_event_id.to_be_bytes(),
    )
}

/// The exclusive upper bound an event cursor carries.
pub fn decode_events(key: &[u8; 32], session_id: &str, cursor: &str) -> Result<i64, CursorError> {
    let body = open(key, KIND_EVENTS, &events_scope(session_id), cursor)?;
    let bytes: [u8; 8] = body
        .as_slice()
        .try_into()
        .map_err(|_| CursorError::Malformed)?;
    Ok(i64::from_be_bytes(bytes))
}

/// Cursor for the session page after `position`.
pub fn encode_sessions(
    key: &[u8; 32],
    account_id: &str,
    environment_id: Option<&str>,
    position: &SessionPosition,
) -> String {
    let mut body = position.updated_at_unix.to_be_bytes().to_vec();
    body.extend_from_slice(position.session_id.as_bytes());
    seal(
        key,
        KIND_SESSIONS,
        &sessions_scope(account_id, environment_id),
        &body,
    )
}

/// The position a session-list cursor resumes after.
pub fn decode_sessions(
    key: &[u8; 32],
    account_id: &str,
    environment_id: Option<&str>,
    cursor: &str,
) -> Result<SessionPosition, CursorError> {
    let body = open(
        key,
        KIND_SESSIONS,
        &sessions_scope(account_id, environment_id),
        cursor,
    )?;
    if body.len() <= 8 || body.len() > 8 + MAX_SESSION_ID_BYTES {
        return Err(CursorError::Malformed);
    }
    let (time, id) = body.split_at(8);
    let updated_at_unix = i64::from_be_bytes(time.try_into().map_err(|_| CursorError::Malformed)?);
    let session_id = String::from_utf8(id.to_vec()).map_err(|_| CursorError::Malformed)?;
    Ok(SessionPosition {
        updated_at_unix,
        session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    fn flip(cursor: &str, at: usize) -> String {
        let mut bytes = URL_SAFE_NO_PAD.decode(cursor).expect("decode");
        bytes[at] ^= 0x01;
        URL_SAFE_NO_PAD.encode(bytes)
    }

    #[test]
    fn an_event_cursor_round_trips() {
        for id in [1, 42, i64::MAX] {
            let cursor = encode_events(&KEY, "sess_a", id);
            assert_eq!(decode_events(&KEY, "sess_a", &cursor), Ok(id));
        }
    }

    #[test]
    fn a_session_cursor_round_trips_with_and_without_a_filter() {
        let position = SessionPosition {
            updated_at_unix: 1_758_000_000,
            session_id: "sess_abc".into(),
        };
        for filter in [None, Some("env_1")] {
            let cursor = encode_sessions(&KEY, "acc_1", filter, &position);
            assert_eq!(
                decode_sessions(&KEY, "acc_1", filter, &cursor),
                Ok(position.clone())
            );
        }
    }

    #[test]
    fn the_cursor_does_not_spell_out_the_raw_id() {
        let cursor = encode_events(&KEY, "sess_a", 42);
        assert_ne!(cursor, "42");
        assert!(cursor.parse::<i64>().is_err());
    }

    #[test]
    fn a_cursor_only_opens_under_its_own_scope() {
        let cursor = encode_events(&KEY, "sess_a", 42);
        assert_eq!(
            decode_events(&KEY, "sess_b", &cursor),
            Err(CursorError::Forged)
        );

        let position = SessionPosition {
            updated_at_unix: 5,
            session_id: "sess_a".into(),
        };
        let unfiltered = encode_sessions(&KEY, "acc_1", None, &position);
        assert_eq!(
            decode_sessions(&KEY, "acc_2", None, &unfiltered),
            Err(CursorError::Forged)
        );
        assert_eq!(
            decode_sessions(&KEY, "acc_1", Some("env_1"), &unfiltered),
            Err(CursorError::Forged)
        );
        // An empty filter id is still a different scope from no filter.
        assert_eq!(
            decode_sessions(&KEY, "acc_1", Some(""), &unfiltered),
            Err(CursorError::Forged)
        );
    }

    #[test]
    fn a_cursor_from_one_route_is_refused_by_the_other() {
        let events = encode_events(&KEY, "acc_1", 42);
        assert_eq!(
            decode_sessions(&KEY, "acc_1", None, &events),
            Err(CursorError::Unsupported)
        );
    }

    #[test]
    fn a_cursor_under_another_key_is_forged() {
        let cursor = encode_events(&[8u8; 32], "sess_a", 42);
        assert_eq!(
            decode_events(&KEY, "sess_a", &cursor),
            Err(CursorError::Forged)
        );
    }

    #[test]
    fn a_tampered_position_or_tag_is_forged() {
        let cursor = encode_events(&KEY, "sess_a", 42);
        let length = URL_SAFE_NO_PAD.decode(&cursor).expect("decode").len();
        // Body byte, then the last tag byte.
        for at in [9, length - 1] {
            assert_eq!(
                decode_events(&KEY, "sess_a", &flip(&cursor, at)),
                Err(CursorError::Forged),
                "byte {at}"
            );
        }
        // A client that builds its own "cursor" for id 10.
        let mut minted = vec![VERSION, KIND_EVENTS];
        minted.extend_from_slice(&10i64.to_be_bytes());
        minted.extend_from_slice(&[0u8; TAG_BYTES]);
        assert_eq!(
            decode_events(&KEY, "sess_a", &URL_SAFE_NO_PAD.encode(minted)),
            Err(CursorError::Forged)
        );
    }

    #[test]
    fn garbled_cursors_are_malformed_or_unsupported() {
        let cursor = encode_events(&KEY, "sess_a", 42);
        for garbled in [
            String::new(),
            "not base64!".to_string(),
            "42".to_string(),
            format!("{cursor}="),
            cursor[..cursor.len() - 4].to_string(),
            "A".repeat(MAX_CURSOR_CHARS + 1),
            URL_SAFE_NO_PAD.encode([VERSION, KIND_EVENTS]),
        ] {
            let outcome = decode_events(&KEY, "sess_a", &garbled);
            assert!(
                matches!(outcome, Err(CursorError::Malformed | CursorError::Forged)),
                "{garbled:?} → {outcome:?}"
            );
        }
        // A future version is not ours to read.
        assert_eq!(
            decode_events(&KEY, "sess_a", &flip(&cursor, 0)),
            Err(CursorError::Unsupported)
        );
    }

    #[test]
    fn a_body_of_the_wrong_length_is_malformed_even_with_a_valid_tag() {
        let short = seal(&KEY, KIND_EVENTS, &events_scope("sess_a"), &[1, 2, 3]);
        assert_eq!(
            decode_events(&KEY, "sess_a", &short),
            Err(CursorError::Malformed)
        );
        let no_id = seal(
            &KEY,
            KIND_SESSIONS,
            &sessions_scope("acc_1", None),
            &5i64.to_be_bytes(),
        );
        assert_eq!(
            decode_sessions(&KEY, "acc_1", None, &no_id),
            Err(CursorError::Malformed)
        );
        let not_utf8 = seal(
            &KEY,
            KIND_SESSIONS,
            &sessions_scope("acc_1", None),
            &[0, 0, 0, 0, 0, 0, 0, 5, 0xff, 0xfe],
        );
        assert_eq!(
            decode_sessions(&KEY, "acc_1", None, &not_utf8),
            Err(CursorError::Malformed)
        );
    }
}
