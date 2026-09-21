//! The owner's event stream, read as ACP and handed back as `SessionEvent`.
//!
//! Every follower of a session — the terminal's mirror, `rebon serve`, the
//! desktop app — reads `SessionEvent`. None of them should learn that the wire
//! changed underneath, and the point of `StreamWatermark` is that they
//! do not have to: it de-duplicates by cursor, and the cursor is still there.
//! So the translation happens here, at the socket, and the shape above it is
//! the one it always was.
//!
//! **What comes back the other way.** A subscribed connection is the only one
//! the owner asks anything on: a pending permission arrives as a
//! `session/request_permission` *request*, and the answer is a JSON-RPC
//! response carrying that request's id. So this half owns two things a
//! request/response link does not — the write half of a subscription, and a
//! note of which id each question came under.
//!
//! That is also why answering a permission cannot go on the request link: the
//! owner is waiting for a response to a question it asked *here*, and a
//! request arriving anywhere else is not that.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};

use crate::protocol::{SessionEvent, SessionStatusSnapshot};
use crate::session_ext::{method, RebonMeta};
use crate::state::{
    BackgroundIpcEndpoint, BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot,
};

/// The write half of a subscription, and what it owes the owner an answer for.
///
/// Held apart from the reader because the answer is written by whoever decided
/// it — a terminal, a browser tab — and that is never the thread parked on the
/// socket waiting for the next event.
pub struct PermissionAnswerer {
    writer: Mutex<TcpStream>,
    /// Which JSON-RPC id each pending question came under.
    asked: Mutex<HashMap<u64, String>>,
}

impl PermissionAnswerer {
    fn new(writer: TcpStream) -> Self {
        Self {
            writer: Mutex::new(writer),
            asked: Mutex::new(HashMap::new()),
        }
    }

    fn note(&self, query_id: u64, request_id: String) {
        self.asked
            .lock()
            .expect("poisoned")
            .insert(query_id, request_id);
    }

    /// Whether this subscription is still holding a question unanswered.
    ///
    /// Only the tests ask. It is how they tell "the answer went out on the
    /// subscription" from "the answer went out some other way and worked" --
    /// the owner's behaviour is the same either way, so the only witness is
    /// this side's own bookkeeping.
    #[doc(hidden)]
    pub fn owes_an_answer(&self, query_id: u64) -> bool {
        self.asked.lock().expect("poisoned").contains_key(&query_id)
    }

    /// Answer one question, if this subscription is the one that was asked.
    ///
    /// `false` when it was not — a client answering a permission it heard
    /// about some other way, which the caller turns back into the legacy path
    /// rather than guessing an id.
    pub fn answer(
        &self,
        query_id: u64,
        option_id: Option<String>,
        extra_text: Option<String>,
        updated_input: Option<serde_json::Value>,
    ) -> bool {
        let Some(id) = self.asked.lock().expect("poisoned").remove(&query_id) else {
            return false;
        };
        let mut outcome = serde_json::json!({ "outcome": "selected" });
        if let Some(option_id) = option_id {
            outcome["optionId"] = serde_json::json!(option_id);
        } else {
            // No option chosen is a cancellation in ACP's spelling, which is
            // what the owner's own reader maps back to "the client declined to
            // choose" rather than to a choice it did not make.
            outcome["outcome"] = serde_json::json!("cancelled");
        }
        if let Some(updated_input) = updated_input.clone() {
            outcome["updatedInput"] = updated_input;
        }
        let mut result = serde_json::json!({ "outcome": outcome });
        if let Some(updated_input) = updated_input {
            result["updatedInput"] = updated_input;
        }
        if let Some(meta) = (RebonMeta {
            extra_text,
            ..RebonMeta::default()
        })
        .to_meta()
        {
            result["_meta"] = meta;
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        let Ok(mut bytes) = serde_json::to_vec(&body) else {
            return false;
        };
        bytes.push(b'\n');
        let mut writer = self.writer.lock().expect("poisoned");
        writer.write_all(&bytes).is_ok() && writer.flush().is_ok()
    }
}

/// The subscriptions this process holds that can answer a permission.
fn answerers() -> &'static Mutex<HashMap<(u32, u16, String), Arc<PermissionAnswerer>>> {
    static ANSWERERS: OnceLock<Mutex<HashMap<(u32, u16, String), Arc<PermissionAnswerer>>>> =
        OnceLock::new();
    ANSWERERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(endpoint: &BackgroundIpcEndpoint) -> (u32, u16, String) {
    (endpoint.pid, endpoint.port, endpoint.token.clone())
}

/// The subscription that can answer this owner's questions, if there is one.
pub fn answerer_for(endpoint: &BackgroundIpcEndpoint) -> Option<Arc<PermissionAnswerer>> {
    answerers()
        .lock()
        .expect("poisoned")
        .get(&key(endpoint))
        .cloned()
}

/// Reads an ACP subscription and yields the events every follower already
/// knows how to read.
pub struct AcpSubscription {
    lines: std::io::Lines<BufReader<TcpStream>>,
    endpoint: BackgroundIpcEndpoint,
    answerer: Arc<PermissionAnswerer>,
}

impl AcpSubscription {
    /// Take over a connection that has been subscribed.
    pub(crate) fn new(
        reader: BufReader<TcpStream>,
        writer: TcpStream,
        endpoint: BackgroundIpcEndpoint,
    ) -> Self {
        let answerer = Arc::new(PermissionAnswerer::new(writer));
        answerers()
            .lock()
            .expect("poisoned")
            .insert(key(&endpoint), Arc::clone(&answerer));
        Self {
            lines: reader.lines(),
            endpoint,
            answerer,
        }
    }

    /// The next event, or `None` when the owner hangs up.
    ///
    /// A read *error* ends the subscription too — there is nothing else to do
    /// with a socket that will not read — but it is not the same thing as the
    /// owner hanging up, and it used to be indistinguishable from it: both
    /// were a `?` that returned `None` and said nothing. The caller's response
    /// to either is to resubscribe, so a stream that ended for a reason nobody
    /// recorded looked exactly like a healthy reconnect.
    pub(crate) fn next_event(&mut self) -> Option<SessionEvent> {
        loop {
            let line = match self.lines.next()? {
                Ok(line) => line,
                Err(error) => {
                    tracing::debug!(
                        kind = ?error.kind(),
                        %error,
                        "rebon: the owner's event stream could not be read; ending the subscription"
                    );
                    return None;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                tracing::debug!("rebon: skipping an unreadable ACP message");
                continue;
            };
            if let Some(event) = self.event_for(&message) {
                return Some(event);
            }
        }
    }
}

impl Drop for AcpSubscription {
    fn drop(&mut self) {
        // The owner has nobody to answer it here any more. Leaving the entry
        // would let a later answer be written to a socket nothing reads.
        let mut held = answerers().lock().expect("poisoned");
        if let Some(current) = held.get(&key(&self.endpoint)) {
            if Arc::ptr_eq(current, &self.answerer) {
                held.remove(&key(&self.endpoint));
            }
        }
    }
}

impl AcpSubscription {
    /// One ACP message as the event it stands for, or `None` for one that
    /// stands for nothing a follower reads.
    fn event_for(&self, message: &serde_json::Value) -> Option<SessionEvent> {
        let method_name = message.get("method")?.as_str()?;
        let params = message.get("params").cloned().unwrap_or_default();
        let meta = RebonMeta::from_meta(params.get("_meta"));
        let cursor = meta.cursor.unwrap_or_default();
        match method_name {
            method::HELLO => {
                let status: SessionStatusSnapshot =
                    serde_json::from_value(params.get("status")?.clone()).ok()?;
                Some(SessionEvent::Hello {
                    cursor: params.get("cursor")?.as_u64()?,
                    epoch: params.get("epoch")?.as_u64()?,
                    // The owner sends it inside the snapshot rather than twice;
                    // they are the same number at the source.
                    turn_generation: status.turn_generation,
                    status: Box::new(status),
                })
            }
            "session/update" => {
                let mut update = params;
                if let Some(object) = update.as_object_mut() {
                    object.remove("_meta");
                }
                Some(SessionEvent::SessionUpdate { cursor, update })
            }
            method::TURN => Some(SessionEvent::Turn {
                cursor,
                state: serde_json::from_value(params.get("state")?.clone()).ok()?,
                stop_reason: params
                    .get("stopReason")
                    .and_then(|reason| reason.as_str())
                    .map(str::to_string),
                stop_refused: params
                    .get("stopRefused")
                    .and_then(|reason| reason.as_str())
                    .map(str::to_string),
            }),
            method::STATUS_CHANGED => Some(SessionEvent::Status {
                cursor,
                snapshot: serde_json::from_value(params.get("snapshot")?.clone()).ok()?,
            }),
            method::GAP => Some(SessionEvent::Gap {
                from: params.get("from")?.as_u64()?,
                to: params.get("to")?.as_u64()?,
            }),
            "session/request_permission" => {
                let query = permission_from(&params, &meta, &self.endpoint)?;
                // Remember which id it came under before handing the event up:
                // whoever answers it will name the query, not the id, and this
                // is the only place the two are seen together.
                if let Some(id) = message.get("id").and_then(|id| id.as_str()) {
                    self.answerer.note(query.query_id, id.to_string());
                }
                Some(SessionEvent::Permission {
                    cursor,
                    query: Box::new(query),
                })
            }
            _ => None,
        }
    }
}

/// A `session/request_permission` as the snapshot every follower renders.
///
/// Everything is in the standard params except which question it is, which
/// rides in `_meta.rebon`, and which owner asked, which the client knows
/// because it is the one it subscribed to.
fn permission_from(
    params: &serde_json::Value,
    meta: &RebonMeta,
    endpoint: &BackgroundIpcEndpoint,
) -> Option<BackgroundPermissionQuerySnapshot> {
    let options = params
        .get("options")
        .and_then(|options| options.as_array())
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    Some(BackgroundPermissionOptionSnapshot {
                        option_id: option.get("optionId")?.as_str()?.to_string(),
                        label: option.get("name")?.as_str()?.to_string(),
                        // The ACP spelling. The shared table reads both it and
                        // the engine's, so keeping what arrived loses nothing
                        // and invents nothing.
                        kind: option.get("kind")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(BackgroundPermissionQuerySnapshot {
        query_id: meta.query_id?,
        turn_generation: meta.turn_generation.unwrap_or_default(),
        endpoint: Some(endpoint.clone()),
        tool: params
            .get("toolName")
            .and_then(|tool| tool.as_str())
            .map(str::to_string),
        tool_call_id: params
            .get("toolCall")
            .and_then(|call| call.get("toolCallId"))
            .and_then(|id| id.as_str())
            .map(str::to_string),
        session_id: params
            .get("sessionId")
            .and_then(|session| session.as_str())
            .map(str::to_string),
        title: params
            .get("title")
            .and_then(|title| title.as_str())
            .map(str::to_string),
        message: params
            .get("message")
            .and_then(|message| message.as_str())
            .map(str::to_string),
        tool_input: params.get("toolInput").cloned(),
        metadata: params.get("metadata").cloned(),
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> BackgroundIpcEndpoint {
        BackgroundIpcEndpoint {
            pid: 7,
            port: 9,
            token: "t".to_string(),
        }
    }

    fn subscription() -> AcpSubscription {
        // A pair of sockets that go nowhere: these tests exercise the
        // translation, which never reads or writes.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let writer = client.try_clone().expect("clone");
        AcpSubscription::new(BufReader::new(client), writer, endpoint())
    }

    #[test]
    fn a_delta_keeps_its_cursor_and_loses_the_envelope_that_carried_it() {
        let subscription = subscription();
        let event = subscription
            .event_for(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s-1",
                    "update": {"sessionUpdate": "agent_message_chunk"},
                    "_meta": {"rebon": {"cursor": 12}},
                },
            }))
            .expect("an update");
        match event {
            SessionEvent::SessionUpdate { cursor, update } => {
                assert_eq!(cursor, 12);
                assert_eq!(update["sessionId"], serde_json::json!("s-1"));
                assert!(
                    update.get("_meta").is_none(),
                    "the transport's own field reached the follower: {update}"
                );
            }
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn a_turn_and_a_gap_come_back_as_themselves() {
        let subscription = subscription();
        let turn = subscription
            .event_for(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "_session/turn",
                "params": {
                    "state": "idle",
                    "stopReason": "end_turn",
                    "_meta": {"rebon": {"cursor": 4}},
                },
            }))
            .expect("a turn");
        assert!(matches!(
            turn,
            SessionEvent::Turn {
                cursor: 4,
                stop_reason: Some(ref reason),
                ..
            } if reason == "end_turn"
        ));

        let gap = subscription
            .event_for(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "_session/gap",
                "params": {"from": 4, "to": 9},
            }))
            .expect("a gap");
        assert!(matches!(gap, SessionEvent::Gap { from: 4, to: 9 }));
    }

    /// A permission arrives as a question, and the id it came under is
    /// remembered so the answer can name the query instead.
    #[test]
    fn a_permission_becomes_an_event_and_leaves_its_id_behind() {
        let subscription = subscription();
        let event = subscription
            .event_for(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "perm-s-1-42",
                "method": "session/request_permission",
                "params": {
                    "sessionId": "s-1",
                    "toolCall": {"toolCallId": "call-1"},
                    "toolName": "Bash",
                    "title": "Run a command",
                    "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
                    "_meta": {"rebon": {"queryId": 42, "turnGeneration": 3, "cursor": 7}},
                },
            }))
            .expect("a permission");
        match event {
            SessionEvent::Permission { cursor, query } => {
                assert_eq!(cursor, 7);
                assert_eq!(query.query_id, 42);
                assert_eq!(query.turn_generation, 3);
                assert_eq!(query.tool.as_deref(), Some("Bash"));
                assert_eq!(query.tool_call_id.as_deref(), Some("call-1"));
                assert_eq!(query.options.len(), 1);
                assert_eq!(query.options[0].option_id, "allow");
                // The owner that asked, which the client knows because it is
                // the one it subscribed to.
                assert_eq!(query.endpoint, Some(endpoint()));
            }
            other => panic!("expected a permission, got {other:?}"),
        }
        assert_eq!(
            subscription
                .answerer
                .asked
                .lock()
                .expect("poisoned")
                .get(&42)
                .map(String::as_str),
            Some("perm-s-1-42"),
            "the id the question came under was not remembered"
        );
    }

    /// A question with no query id is not one this client can answer, so it is
    /// not handed up as one. Guessing an id would answer somebody else's.
    #[test]
    fn a_permission_without_a_query_id_is_not_an_event() {
        let subscription = subscription();
        assert!(subscription
            .event_for(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "perm-s-1-42",
                "method": "session/request_permission",
                "params": {"sessionId": "s-1", "options": []},
            }))
            .is_none());
    }

    /// Answering names the query; the id is this half's business. A query
    /// nobody asked about here is refused rather than answered under a guess.
    #[test]
    fn answering_a_question_this_subscription_never_heard_is_refused() {
        let subscription = subscription();
        assert!(!subscription.answerer.answer(999, None, None, None));
    }

    /// A message this build has no meaning for is skipped, not fatal. An owner
    /// from a later version may say things this one does not read.
    #[test]
    fn an_unknown_notification_is_skipped() {
        let subscription = subscription();
        assert!(subscription
            .event_for(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "_session/something_later",
                "params": {},
            }))
            .is_none());
    }
}
