//! The session seat shim — dsh-session's
//! event-sourcing face mapped onto rebon's transcript, WITHOUT touching
//! rebon's storage format or write path.
//!
//! Surface (JSON service `session`, session-scoped like `tool-registry`):
//! - `append {type, data}` — the observable event face: emits the kernel
//!   event `session/append` (same vocabulary the compose bridge uses
//!   for `exec.agent.session.append`). Deliberately NOT a durable write —
//!   rebon's engine owns transcript persistence, and this seat never
//!   writes (the contract: transcript write path untouched).
//! - `deriveMessages {}` — dsh's derived-message read face: loads the
//!   session's authoritative JSONL transcript (read-only) and projects the
//!   reconstructed user/assistant chain into dsh message shape
//!   `[{role, content: [{type:'text', text}]}]`. Non-text blocks project
//!   as `[<type> content]` markers, the same folding rule the tool seams
//!   use.
//! - `info {}` — `{sessionId}`.

use std::path::PathBuf;
use std::sync::Arc;

use rebon_kernel::{Context, JsonService, KernelError};
use serde_json::Value;

/// JSON-plane name of the session seat.
pub const SESSION_SERVICE: &str = "session";

pub struct SessionSeat {
    session_id: String,
    projects_root: PathBuf,
    events: Context,
}

impl SessionSeat {
    /// Build and provide the seat on the session's kernel context.
    pub fn provide(ctx: &Context, session_id: &str, projects_root: PathBuf) -> Arc<Self> {
        let seat = Arc::new(Self {
            session_id: session_id.to_string(),
            projects_root,
            events: ctx.clone(),
        });
        if let Err(err) = ctx.provide_json(SESSION_SERVICE, seat.clone()) {
            // Additive service: a failure must not take the session down.
            tracing::warn!(%err, session = %seat.session_id, "session seat failed to publish");
        }
        seat
    }

    fn append(&self, params: &Value) -> Result<Value, KernelError> {
        let entry_type = params
            .get("type")
            .and_then(|t| t.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                KernelError::Other("session append requires a non-empty `type`".into())
            })?;
        let data = params.get("data").cloned().unwrap_or(Value::Null);
        self.events.emit_json(
            "session/append",
            &serde_json::json!({
                "type": entry_type,
                "data": data,
                "sessionId": self.session_id,
            }),
        );
        Ok(serde_json::json!({ "appended": true }))
    }

    fn derive_messages(&self) -> Result<Value, KernelError> {
        let Some(cwd) =
            rebon_session::find_session_transcript_cwd(&self.projects_root, &self.session_id)
        else {
            // No transcript yet (fresh session) — an empty derivation, not
            // an error: dsh's deriveMessages on an empty log is empty.
            return Ok(serde_json::json!({ "messages": [] }));
        };
        let path = rebon_session::transcript_file_path(&self.projects_root, &cwd, &self.session_id);
        let loaded = rebon_session::load_transcript_from_file(&path)
            .map_err(|err| KernelError::Other(format!("transcript read failed: {err}")))?;
        let messages: Vec<Value> = loaded
            .map(|transcript| {
                transcript
                    .messages
                    .iter()
                    .filter_map(|entry| project_entry(&entry.entry_type, &entry.raw))
                    .collect()
            })
            .unwrap_or_default();
        Ok(serde_json::json!({ "messages": messages }))
    }
}

/// Project one transcript entry into dsh message shape; `None` drops
/// non-message rows (attachments, system markers).
fn project_entry(entry_type: &str, raw: &Value) -> Option<Value> {
    if entry_type != "user" && entry_type != "assistant" {
        return None;
    }
    let message = raw.get("message")?;
    let role = message.get("role").and_then(|r| r.as_str())?;
    let content = message.get("content")?;
    let blocks: Vec<Value> = match content {
        Value::String(text) => vec![serde_json::json!({ "type": "text", "text": text })],
        Value::Array(entries) => entries
            .iter()
            .filter_map(|block| match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => Some(serde_json::json!({
                    "type": "text",
                    "text": block.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                })),
                Some(other) => Some(serde_json::json!({
                    "type": "text",
                    "text": format!("[{other} content]"),
                })),
                None => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if blocks.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "role": role, "content": blocks }))
}

impl JsonService for SessionSeat {
    fn call(&self, method: &str, params: Value) -> Result<Value, KernelError> {
        match method {
            "append" => self.append(&params),
            "deriveMessages" => self.derive_messages(),
            "info" => Ok(serde_json::json!({ "sessionId": self.session_id })),
            other => Err(KernelError::Other(format!(
                "session has no method `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_emits_and_derive_projects_the_real_transcript() {
        let kernel = rebon_kernel::Kernel::new();
        let ctx = kernel.context().fork_scoped("session/probe");
        let projects = tempfile::tempdir().expect("tempdir");

        // Seed a REAL transcript through the storage crate's own write API
        // (test-side usage; the seat itself never writes).
        let cwd = "F:/probe/project";
        let mut parent: Option<String> = None;
        for (entry_type, role, text) in [
            ("user", "user", "你好"),
            ("assistant", "assistant", "hello back"),
        ] {
            let mut entry = rebon_session::TranscriptWriteEntry::new(
                entry_type,
                serde_json::json!({
                    "type": entry_type,
                    "message": { "role": role, "content": [{ "type": "text", "text": text }] },
                }),
            );
            entry.parent_uuid = parent.clone();
            let written =
                rebon_session::append_transcript_entry(projects.path(), cwd, "sess-seat", entry)
                    .expect("transcript seeds");
            parent = Some(written.uuid);
        }

        let seat = SessionSeat::provide(&ctx, "sess-seat", projects.path().to_path_buf());

        // Observable append face.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            kernel.context().on_json("session/append", move |payload| {
                seen.lock().unwrap().push(payload.clone());
            });
        }
        let out = seat
            .call(
                "append",
                serde_json::json!({ "type": "todo/write", "data": { "todos": [] } }),
            )
            .unwrap();
        assert_eq!(out["appended"], true);
        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "todo/write");
        assert_eq!(events[0]["sessionId"], "sess-seat");
        drop(events);

        // Read face: real transcript, derived into dsh message shape.
        let derived = seat.call("deriveMessages", Value::Null).unwrap();
        let messages = derived["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2, "{derived}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "你好");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["text"], "hello back");

        // Malformed method fails loudly.
        assert!(seat.call("mutate", Value::Null).is_err());
    }
}
