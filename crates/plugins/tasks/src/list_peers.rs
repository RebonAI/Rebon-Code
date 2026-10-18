use async_trait::async_trait;
use rebon_tool::{PeerInfo, Tool, ToolContext};
use rebon_tools_core::{
    ToolError, ToolErrorPresentation, ToolId, ToolInputSchema, ToolKind, ToolResult,
    ValidationOutcome,
};
use serde::Serialize;
use serde_json::{json, Value};

pub const LIST_PEERS_TOOL_NAME: &str = "ListPeers";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct ListPeersTool;

#[derive(Debug, Serialize)]
struct PeerEntry {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    cwd: String,
    kind: String,
    busy: bool,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_id: Option<String>,
}

impl From<PeerInfo> for PeerEntry {
    fn from(peer: PeerInfo) -> Self {
        Self {
            session_id: peer.session_id,
            name: peer.name,
            cwd: peer.cwd,
            kind: peer.kind,
            busy: peer.busy,
            reachable: peer.reachable,
            terminal_id: peer.terminal_id,
        }
    }
}

/// Whether the caller asked for peers that cannot accept a message right now.
fn include_unreachable_requested(input: &Value) -> ToolResult<bool> {
    match input.get("include_unreachable") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ToolError::InvalidInput {
            tool: ToolId::new(LIST_PEERS_TOOL_NAME),
            reason: "`include_unreachable` must be a boolean".into(),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

#[async_trait]
impl Tool for ListPeersTool {
    fn id(&self) -> ToolId {
        ToolId::new(LIST_PEERS_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["ListPeersTool"]
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Task
    }

    fn description(&self) -> &str {
        "List the other sessions running on this machine, so a message can be \
         addressed to one of them with SendMessage `to: \"session:<id>\"`.\n\
         \n\
         ## When to Use This Tool\n\
         - Before sending to another session when its id is unknown\n\
         - To find the session id behind a bare name\n\
         - To see whether a session is busy or cannot accept a message\n\
         \n\
         ## Output\n\
         Returns each peer's session_id, name, cwd, kind (hosted or foreground), \
         busy, reachable and terminal_id. The calling session is not included. \
         Pass `include_unreachable: true` to list sessions that cannot accept a \
         message right now."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "include_unreachable": {
                    "type": "boolean",
                    "description": "Include sessions that cannot accept a message right now (default false)."
                }
            },
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match input {
            Value::Object(_) => Ok(ValidationOutcome::valid()),
            _ => Ok(ValidationOutcome::invalid(
                "ListPeers input must be an object",
                INVALID_INPUT_CODE,
            )),
        }
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let validation = self.validate_input(&input, context).await?;
        if !validation.is_valid() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: validation
                    .message
                    .unwrap_or_else(|| "ListPeers input is invalid".into()),
                error_code: validation.error_code,
            });
        }
        let include_unreachable = include_unreachable_requested(&input)?;
        let Some(messenger) = context.peer_messenger() else {
            return Ok(json!({
                "peers": [],
                "note": "Cross-session messaging is not available in this surface.",
            }));
        };
        let own_session_id = context
            .session_id()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty());
        let peers = messenger
            .list_peers(own_session_id)
            .await
            .map_err(|message| ToolError::Presented {
                tool: self.id(),
                presentation: ToolErrorPresentation::new(
                    "peer_list_failed",
                    "Other sessions could not be listed.",
                    message,
                ),
            })?;
        let peers: Vec<PeerEntry> = peers
            .into_iter()
            .filter(|peer| Some(peer.session_id.as_str()) != own_session_id)
            .filter(|peer| include_unreachable || peer.reachable)
            .map(PeerEntry::from)
            .collect();
        Ok(json!({ "peers": peers }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::{PeerDelivery, PeerMessage, PeerMessenger};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct StubMessenger {
        peers: Vec<PeerInfo>,
        excluded: Mutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl PeerMessenger for StubMessenger {
        async fn list_peers(
            &self,
            exclude_session_id: Option<&str>,
        ) -> Result<Vec<PeerInfo>, String> {
            self.excluded
                .lock()
                .expect("stub messenger lock poisoned")
                .push(exclude_session_id.map(str::to_string));
            Ok(self.peers.clone())
        }

        async fn send(
            &self,
            _target_session_id: &str,
            _message: PeerMessage,
        ) -> Result<PeerDelivery, String> {
            panic!("ListPeers must not send")
        }
    }

    fn peer(session_id: &str, reachable: bool) -> PeerInfo {
        PeerInfo {
            session_id: session_id.into(),
            name: Some(format!("peer-{session_id}")),
            cwd: "C:/work".into(),
            kind: "hosted".into(),
            busy: false,
            reachable,
            terminal_id: Some("term-1".into()),
        }
    }

    #[tokio::test]
    async fn call_lists_peers_and_leaves_the_caller_out() {
        let messenger = Arc::new(StubMessenger {
            peers: vec![peer("caller-session", true), peer("other-session", true)],
            excluded: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("caller-session")
            .with_peer_messenger(messenger.clone());

        let output = ListPeersTool.call(json!({}), &context).await.unwrap();

        let peers = output["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["session_id"], json!("other-session"));
        assert_eq!(peers[0]["kind"], json!("hosted"));
        assert_eq!(peers[0]["terminal_id"], json!("term-1"));
        assert_eq!(
            messenger.excluded.lock().unwrap().as_slice(),
            &[Some("caller-session".to_string())]
        );
    }

    #[tokio::test]
    async fn unreachable_peers_are_hidden_unless_requested() {
        let messenger = Arc::new(StubMessenger {
            peers: vec![peer("reachable-one", true), peer("stale-one", false)],
            excluded: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new().with_peer_messenger(messenger);

        let output = ListPeersTool.call(json!({}), &context).await.unwrap();
        assert_eq!(output["peers"].as_array().unwrap().len(), 1);

        let output = ListPeersTool
            .call(json!({ "include_unreachable": true }), &context)
            .await
            .unwrap();
        assert_eq!(output["peers"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn call_without_a_messenger_notes_the_surface_instead_of_failing() {
        let output = ListPeersTool
            .call(json!({}), &ToolContext::new())
            .await
            .unwrap();
        assert!(output["peers"].as_array().unwrap().is_empty());
        assert!(output["note"]
            .as_str()
            .unwrap()
            .contains("not available in this surface"));
    }

    #[test]
    fn include_unreachable_must_be_a_boolean() {
        let error =
            include_unreachable_requested(&json!({ "include_unreachable": "yes" })).unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput { .. }));
    }
}
