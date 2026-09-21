//! A pending permission as the ACP request that asks it.
//!
//! Moved down from `rebon-cli`'s `serve/hosted.rs`, because a
//! second consumer appeared: the worker's own ACP control plane re-asks a
//! pending permission on a connection that subscribes. Two copies of "how an
//! owner's permission snapshot becomes `session/request_permission`" would
//! drift, and the drift would look like one client seeing a different question
//! from another.
//!
//! `serve` still calls this; nothing about the bytes it sends changed.

use rebon_proto::types::PermissionOptionKind;
use rebon_session_host::BackgroundPermissionQuerySnapshot;
use serde_json::{json, Value};

/// The params of `session/request_permission` for one pending query.
///
/// Built as a `Value` rather than the typed `RequestPermissionParams` because
/// the optional halves are omitted rather than sent as null, and the typed
/// struct would need every one of them spelled as `None` at a call site that
/// has nothing to say about them.
pub fn permission_params(session_id: &str, query: &BackgroundPermissionQuerySnapshot) -> Value {
    let options: Vec<Value> = query
        .options
        .iter()
        .map(|option| {
            json!({
                "optionId": option.option_id,
                "name": option.label,
                "kind": permission_option_kind_wire(&option.kind),
            })
        })
        .collect();
    let mut params = json!({
        "sessionId": session_id,
        "toolCall": { "toolCallId": query.tool_call_id.clone().unwrap_or_default() },
        "options": options,
    });
    if let Some(title) = &query.title {
        params["title"] = json!(title);
    }
    if let Some(message) = &query.message {
        params["message"] = json!(message);
    }
    if let Some(tool) = &query.tool {
        params["toolName"] = json!(tool);
    }
    if let Some(input) = &query.tool_input {
        params["toolInput"] = input.clone();
    }
    if let Some(metadata) = &query.metadata {
        params["metadata"] = metadata.clone();
    }
    params
}

/// Which kind an option is, for drawing it.
///
/// The table itself is [`rebon_session_host::parse_permission_option_kind`],
/// next to the field it reads. What this adds is the answer for a kind that
/// table does not know: it is not quietly bent into one that looks plausible;
/// it falls to the default and says so.
pub fn permission_option_kind(kind: &str) -> PermissionOptionKind {
    rebon_session_host::parse_permission_option_kind(kind).unwrap_or_else(|| {
        // Visible rather than silent. The default is the least surprising
        // *presentation* -- this decides how an option is rendered and
        // ordered, never what it permits; the user still chooses one -- but
        // a kind nobody knows means two builds disagree about this field,
        // and that is worth a line in a log rather than a shrug.
        tracing::warn!(
            kind = %kind,
            "rebon: a permission option arrived with a kind this build does not know; \
             showing it as allow-once"
        );
        PermissionOptionKind::AllowOnce
    })
}

/// The same, as ACP spells it on the wire.
fn permission_option_kind_wire(kind: &str) -> &'static str {
    match permission_option_kind(kind) {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_option_kinds_map_to_the_acp_spelling() {
        assert_eq!(permission_option_kind_wire("AllowOnce"), "allow_once");
        assert_eq!(permission_option_kind_wire("AllowAlways"), "allow_always");
        assert_eq!(permission_option_kind_wire("RejectOnce"), "reject_once");
        assert_eq!(permission_option_kind_wire("RejectAlways"), "reject_always");
        assert_eq!(permission_option_kind_wire("allow_always"), "allow_always");
    }

    /// Both spellings of every kind, and nothing else.
    ///
    /// The engine's enum name and ACP's snake_case are the two a peer can
    /// legitimately write. Anything else is two builds disagreeing about this
    /// field, and the answer is the default plus a log line -- not a guess that
    /// happens to look right.
    #[test]
    fn a_kind_neither_side_knows_falls_to_the_default_rather_than_being_bent() {
        for known in [
            ("AllowOnce", PermissionOptionKind::AllowOnce),
            ("allow_once", PermissionOptionKind::AllowOnce),
            ("AllowAlways", PermissionOptionKind::AllowAlways),
            ("allow_always", PermissionOptionKind::AllowAlways),
            ("RejectOnce", PermissionOptionKind::RejectOnce),
            ("reject_once", PermissionOptionKind::RejectOnce),
            ("RejectAlways", PermissionOptionKind::RejectAlways),
            ("reject_always", PermissionOptionKind::RejectAlways),
        ] {
            assert_eq!(
                permission_option_kind(known.0),
                known.1,
                "{} is a spelling both sides use",
                known.0
            );
        }
        // Near misses, each of which the mirror's old table would have bent
        // into something plausible. The default is a *presentation* choice --
        // this decides how an option is drawn, never what it permits -- so
        // falling to it is safe, and the warning is what makes it visible.
        for bent in [
            "ALLOW_ALWAYS",
            "allow-always",
            "allowalways",
            " AllowAlways ",
            "reject",
            "",
        ] {
            assert_eq!(
                permission_option_kind(bent),
                PermissionOptionKind::AllowOnce,
                "{bent:?} should not have been read as a kind"
            );
        }
    }

    #[test]
    fn permission_params_carry_what_the_query_has_and_omit_what_it_does_not() {
        let query = BackgroundPermissionQuerySnapshot {
            query_id: 7,
            turn_generation: 3,
            endpoint: None,
            tool: Some("Bash".to_string()),
            tool_call_id: Some("call-1".to_string()),
            session_id: Some("sess".to_string()),
            title: Some("Run a command".to_string()),
            message: Some("rm -rf /tmp/x".to_string()),
            tool_input: Some(json!({ "command": "rm -rf /tmp/x" })),
            metadata: None,
            options: vec![rebon_session_host::BackgroundPermissionOptionSnapshot {
                option_id: "allow".to_string(),
                label: "Allow".to_string(),
                kind: "AllowOnce".to_string(),
            }],
        };
        let params = permission_params("sess", &query);
        assert_eq!(params["sessionId"], "sess");
        assert_eq!(params["toolCall"]["toolCallId"], "call-1");
        assert_eq!(params["toolName"], "Bash");
        assert_eq!(params["title"], "Run a command");
        assert_eq!(params["options"][0]["optionId"], "allow");
        assert_eq!(params["options"][0]["name"], "Allow");
        assert_eq!(params["options"][0]["kind"], "allow_once");
        assert!(params.get("metadata").is_none());
    }
}
