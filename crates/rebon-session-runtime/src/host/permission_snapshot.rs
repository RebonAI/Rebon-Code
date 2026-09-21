//! Turning a live permission query into the snapshot the wire carries.
//!
//! This is the one place the two halves meet, and it is why the function
//! lives here rather than in `rebon-session-host`. [`OutboundPermissionQuery`]
//! carries a `oneshot::Sender` for the answer: it is a broker handle inside
//! one process, not something that can be sent anywhere.
//! [`BackgroundPermissionQuerySnapshot`] is the part that *can* be sent, and
//! `rebon-session-host` owns it.
//!
//! Keeping the projection in the host crate meant that crate depended on
//! `rebon-core` — one upward edge, for one type, in a crate that is meant to
//! be the bottom of the session layering. This crate already depends on both
//! sides, so the projection sits here and the edge is gone. The layering
//! canary in `rebon-session-host` now names `rebon-core`.

use rebon_core::permission::OutboundPermissionQuery;
use rebon_session_host::{BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot};

/// Project a live query onto the snapshot a client can be sent.
///
/// `turn_generation` and `endpoint` are left at their defaults: neither is a
/// property of the query, and both are filled in by whoever is about to
/// publish the snapshot, which is the only place that knows them.
pub fn background_permission_snapshot(
    query: &OutboundPermissionQuery,
) -> BackgroundPermissionQuerySnapshot {
    BackgroundPermissionQuerySnapshot {
        query_id: query.id,
        turn_generation: 0,
        endpoint: None,
        tool: Some(query.tool_name.clone()),
        tool_call_id: Some(query.tool_call_id.clone()),
        session_id: Some(query.session_id.clone()),
        title: (!query.title.trim().is_empty()).then(|| query.title.clone()),
        message: Some(query.message.clone()),
        tool_input: query.tool_input.clone(),
        metadata: query.metadata.clone(),
        options: query
            .options
            .iter()
            .map(|option| BackgroundPermissionOptionSnapshot {
                option_id: option.option_id.clone(),
                label: option.label.clone(),
                kind: format!("{:?}", option.kind),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query with no one listening for the answer. The receiver is dropped
    /// on purpose: this projection never sends one.
    fn detached_test_permission(query_id: u64, session_id: &str) -> OutboundPermissionQuery {
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        OutboundPermissionQuery {
            id: query_id,
            tool_name: "Bash".into(),
            tool_call_id: format!("tool-{query_id}"),
            session_id: session_id.into(),
            title: "Run command".into(),
            message: "Allow command?".into(),
            tool_input: None,
            metadata: None,
            options: vec![rebon_core::permission::PermissionQueryOption {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: rebon_core::permission::PermissionOptionKind::AllowOnce,
            }],
            response_tx,
        }
    }

    #[test]
    fn permission_snapshot_preserves_title_and_metadata() {
        let mut query = detached_test_permission(2, "session-2");
        query.metadata = Some(serde_json::json!({"kind":"workflowReview"}));

        let snapshot = background_permission_snapshot(&query);

        assert_eq!(snapshot.title.as_deref(), Some("Run command"));
        assert_eq!(snapshot.message.as_deref(), Some("Allow command?"));
        assert_eq!(snapshot.metadata.unwrap()["kind"], "workflowReview");
    }

    #[test]
    fn a_blank_title_becomes_no_title_rather_than_an_empty_one() {
        // The `.trim().is_empty()` guard is the only branch in this
        // projection, and it moved crates with everything else.
        let mut query = detached_test_permission(3, "session-3");
        query.title = "   ".into();

        let snapshot = background_permission_snapshot(&query);

        assert_eq!(snapshot.title, None);
        assert_eq!(snapshot.query_id, 3);
        assert_eq!(snapshot.options.len(), 1);
        assert_eq!(snapshot.options[0].kind, "AllowOnce");
    }
}
