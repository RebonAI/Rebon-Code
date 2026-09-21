//! Session history and session listing: the controller's read side of a
//! session that streams whether or not anyone is watching.
//!
//! A worker streams whether or not anyone is watching, and RC persists
//! every frame. These are the shapes a controller reads that history
//! back through — over plain HTTP, so it works with the machine offline
//! and without the session stream's frame and queue limits:
//!
//! | Route | Response |
//! |---|---|
//! | `GET /v1/sessions/{session}/events?cursor=&limit=` | [`SessionEventPage`] |
//! | `GET /v1/sessions?environment=&cursor=&limit=` | [`SessionPage`] |
//!
//! These types are defined once, here: the server serializes them and the
//! HTTP client deserializes them. They are pure data and are not behind the
//! `http` feature, because the server, which does not link the client, needs
//! them too.
//!
//! ## Cursors
//!
//! `next_cursor` is an opaque string. A client stores it and sends it
//! back unchanged as `cursor`; it must not parse, build or compare one.
//! It is **absent** when there is nothing further to read. A cursor is
//! only valid for the request it came from — the same session, or the
//! same `environment` filter — and anything else is refused with 400.
//!
//! ## Event order
//!
//! Event pages walk **backwards** from the newest event, but each page
//! lists its events **oldest first**, so a client can prepend a page to
//! what it already shows without reordering it. `event_id` is the total
//! order of a session's history — the same order the session stream
//! replays in — and ids can have gaps (retention removes rows), so a
//! client must never infer "missing" from a gap.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session_stream::{SessionFrame, SessionRunState};

/// One persisted session event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventRecord {
    /// Position in the session's history. Increasing, not contiguous.
    pub event_id: u64,
    /// The frame's `type`, e.g. `session_message` or `control_response`.
    pub kind: String,
    /// The frame exactly as it was persisted — a
    /// [`SessionFrame`] object, `type` included.
    pub payload: Value,
    /// When the server persisted it (RFC 3339, millisecond precision).
    pub created_at: String,
}

impl SessionEventRecord {
    /// Decode the payload as a session-stream frame.
    ///
    /// A `type` this build does not know still decodes, as
    /// [`SessionFrame::Other`]; only a malformed known frame fails.
    pub fn frame(&self) -> Result<SessionFrame, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

/// Body of `GET /v1/sessions/{session}/events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventPage {
    /// This page's events, oldest first.
    pub events: Vec<SessionEventRecord>,
    /// Where the next (older) page starts. Absent once the page reaches
    /// the beginning of the session's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// The newest `session_state` frame a worker reported for a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportedSessionState {
    /// The frame's state, in the [`SessionRunState`] vocabulary.
    pub state: SessionRunState,
    /// The frame's `detail`, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The event it was persisted as.
    pub event_id: u64,
}

/// One row of `GET /v1/sessions`: enough to render a session list
/// without opening each session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session id.
    pub session_id: String,
    /// Environment the session runs on. The environment may since have
    /// been deregistered; its sessions stay readable.
    pub environment_id: String,
    /// The server's lifecycle state: `queued`, `running` or `archived`.
    pub state: String,
    /// What the worker last said about the session, if it ever sent a
    /// `session_state` frame. Distinct from `state`: this is the
    /// worker's view ([`SessionRunState`]), that is RC's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_state: Option<ReportedSessionState>,
    /// When the session was created (RFC 3339).
    pub created_at: String,
    /// Latest activity of any kind — a frame, a queued prompt, a lease,
    /// an archive (RFC 3339). This is the list's sort key.
    pub last_activity_at: String,
    /// Newest persisted event, when the session has any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<u64>,
    /// When that event was persisted (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<String>,
}

/// Body of `GET /v1/sessions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPage {
    /// This page's sessions, most recent activity first.
    pub sessions: Vec<SessionSummary>,
    /// Where the next page starts. Absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Which page to ask for: the query half of both routes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageRequest {
    /// `next_cursor` from the previous page; `None` for the first page.
    pub cursor: Option<String>,
    /// Page size. `None` takes the server's default; a larger value
    /// than the server's maximum is clamped to it, and `0` is refused.
    pub limit: Option<u32>,
}

impl PageRequest {
    /// The first page, at the server's default size.
    pub fn first() -> Self {
        Self::default()
    }

    /// The page after one that returned `cursor`.
    pub fn after(cursor: impl Into<String>) -> Self {
        Self {
            cursor: Some(cursor.into()),
            limit: None,
        }
    }

    /// Ask for `limit` items per page.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_event_page_without_a_cursor_omits_the_key() {
        let page = SessionEventPage {
            events: vec![SessionEventRecord {
                event_id: 7,
                kind: "session_message".into(),
                payload: json!({"type": "session_message", "message": {"seq": 1}}),
                created_at: "2026-09-16T00:00:00.000Z".into(),
            }],
            next_cursor: None,
        };
        let value = serde_json::to_value(&page).expect("serialize");
        assert_eq!(
            value,
            json!({"events": [{
                "event_id": 7,
                "kind": "session_message",
                "payload": {"type": "session_message", "message": {"seq": 1}},
                "created_at": "2026-09-16T00:00:00.000Z"
            }]})
        );
        let back: SessionEventPage = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, page);
    }

    #[test]
    fn an_event_payload_decodes_as_a_frame_including_unknown_types() {
        let known = SessionEventRecord {
            event_id: 1,
            kind: "session_state".into(),
            payload: json!({"type": "session_state", "state": "idle"}),
            created_at: String::new(),
        };
        assert_eq!(
            known.frame().expect("frame"),
            SessionFrame::SessionState {
                state: "idle".into(),
                detail: None
            }
        );
        let unknown = SessionEventRecord {
            payload: json!({"type": "from_the_future", "x": 1}),
            ..known
        };
        assert!(matches!(
            unknown.frame().expect("frame"),
            SessionFrame::Other(_)
        ));
    }

    #[test]
    fn a_summary_omits_what_a_fresh_session_does_not_have() {
        let summary = SessionSummary {
            session_id: "sess_1".into(),
            environment_id: "env_1".into(),
            state: "queued".into(),
            reported_state: None,
            created_at: "2026-09-16T00:00:00.000Z".into(),
            last_activity_at: "2026-09-16T00:00:00.000Z".into(),
            last_event_id: None,
            last_event_at: None,
        };
        let value = serde_json::to_value(&summary).expect("serialize");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "created_at",
                "environment_id",
                "last_activity_at",
                "session_id",
                "state"
            ]
        );
    }

    #[test]
    fn a_reported_state_reads_known_and_unknown_words() {
        let known: ReportedSessionState =
            serde_json::from_value(json!({"state": "failed", "detail": "exit 1", "event_id": 3}))
                .unwrap();
        assert_eq!(known.state, SessionRunState::Failed);
        assert!(known.state.is_terminal());
        let unknown: ReportedSessionState =
            serde_json::from_value(json!({"state": "requires_action", "event_id": 4})).unwrap();
        assert_eq!(
            unknown.state,
            SessionRunState::Other("requires_action".into())
        );
        assert_eq!(
            serde_json::to_value(&unknown).unwrap(),
            json!({"state": "requires_action", "event_id": 4})
        );
    }

    #[test]
    fn page_requests_compose() {
        assert_eq!(PageRequest::first(), PageRequest::default());
        let next = PageRequest::after("abc").with_limit(5);
        assert_eq!(next.cursor.as_deref(), Some("abc"));
        assert_eq!(next.limit, Some(5));
    }
}
