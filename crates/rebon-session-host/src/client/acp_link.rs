//! One long connection to an owner that speaks ACP.
//!
//! The legacy protocol opens a socket per request because it has to: an
//! envelope is the whole conversation. ACP does not, and that is most of the
//! point — a connection costs a round trip and an accept, and a client that
//! pays it per call pays it on every status poll, every lease renewal, every
//! keystroke that asks the owner anything.
//!
//! So one connection per endpoint generation, and requests share it:
//!
//! - **One writer.** A `Mutex` around the socket's write half, held only for
//!   the length of one frame. Two writers interleaving would tear a frame, and
//!   a torn frame is not something a peer resynchronises from — the same
//!   reason the owner's side has a single writer thread.
//! - **One reader.** A thread that reads frames and hands each answer to
//!   whoever is waiting for that id. Nothing else reads the socket, so there
//!   is no question of two callers racing for the same bytes.
//! - **Paired by id.** Every request takes an id from one counter and leaves a
//!   one-shot behind; the reader looks the id up and delivers. A response for
//!   an id nobody is waiting on is dropped, which is what a caller that timed
//!   out and walked away leaves behind.
//!
//! **A link that dies releases its waiters.** The reader thread drops the
//! whole pending table on its way out, so every caller blocked on one wakes
//! with a transport failure rather than sitting until its own deadline. A
//! client that is told nothing is a client that has to guess.
//!
//! The link is keyed by endpoint generation — `{pid, port, token}` — for the
//! same reason the protocol probe is: a worker that replaced another publishes
//! a new token, so it is a different key and gets its own connection rather
//! than inheriting one pointed at a process that is gone.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rebon_proto::types::JsonRpcError;

use crate::state::BackgroundIpcEndpoint;
use crate::HostCallError;

/// How long to wait for `initialize` to be accepted when opening a link.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// One answer, as it came off the wire.
type Answer = Result<serde_json::Value, JsonRpcError>;

/// A connection to one owner, shared by everything that talks to it.
pub struct AcpLink {
    /// Held for one frame at a time. The socket has one writer.
    writer: Mutex<TcpStream>,
    /// Who is waiting for which answer.
    pending: Arc<Mutex<HashMap<String, SyncSender<Answer>>>>,
    next_id: AtomicU64,
    /// A number that makes this client's ids its own, so two clients on one
    /// owner cannot both be waiting for `"3"`.
    prefix: String,
    methods: Vec<String>,
}

impl AcpLink {
    /// Open a link and complete the handshake, or say why not.
    pub fn open(port: u16, token: &str) -> Result<Self, HostCallError> {
        let address = format!("127.0.0.1:{port}")
            .parse()
            .map_err(|_| HostCallError::Transport("bad endpoint address".to_string()))?;
        let stream = TcpStream::connect_timeout(&address, HANDSHAKE_TIMEOUT)
            .map_err(|error| HostCallError::Transport(error.to_string()))?;
        stream
            .set_nodelay(true)
            .map_err(|error| HostCallError::Transport(error.to_string()))?;
        let reader_half = stream
            .try_clone()
            .map_err(|error| HostCallError::Transport(error.to_string()))?;

        let pending: Arc<Mutex<HashMap<String, SyncSender<Answer>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        std::thread::spawn(move || read_answers(reader_half, reader_pending));

        let mut link = Self {
            writer: Mutex::new(stream),
            pending,
            next_id: AtomicU64::new(1),
            prefix: format!("c{}", std::process::id()),
            methods: Vec::new(),
        };
        // `initialize` first, because the owner rejects everything until it has
        // been accepted. Its failure is this link's failure: a token the owner
        // will not take is not something a later request recovers from.
        let initialized = link.call(
            "initialize",
            serde_json::json!({ "_meta": { "rebon": { "token": token } } }),
            HANDSHAKE_TIMEOUT,
        )?;
        link.methods = initialized
            .pointer("/_meta/rebon/methods")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
        Ok(link)
    }

    /// Whether this owner advertised an optional extension in its handshake.
    pub(crate) fn supports_method(&self, method: &str) -> bool {
        self.methods.iter().any(|advertised| advertised == method)
    }

    /// Send one request and wait for its answer.
    pub fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, HostCallError> {
        let id = format!(
            "{}-{}",
            self.prefix,
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (answered, answer) = sync_channel::<Answer>(1);
        self.pending
            .lock()
            .expect("poisoned")
            .insert(id.clone(), answered);

        let sent = self.write_request(&id, method, params);
        if let Err(error) = sent {
            self.pending.lock().expect("poisoned").remove(&id);
            return Err(error);
        }

        match answer.recv_timeout(timeout) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(crate::wire_errors::from_json_rpc(&error)),
            // The deadline passed. The waiter is removed here rather than left
            // for a late answer to find, so the table does not grow with
            // callers that walked away.
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().expect("poisoned").remove(&id);
                Err(HostCallError::HostUnanswered)
            }
            // The reader is gone, which it announces by dropping the table.
            Err(RecvTimeoutError::Disconnected) => Err(HostCallError::Transport(
                "the connection closed".to_string(),
            )),
        }
    }

    /// Send one notification. Nothing comes back, by definition.
    ///
    /// `false` only when the socket would not take it. A notification the
    /// owner refuses is a notification the caller never hears about, which is
    /// what JSON-RPC means by the word.
    pub fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), HostCallError> {
        let mut body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .map_err(|_| HostCallError::InvalidRequest)?;
        body.push(b'\n');
        let mut writer = self.writer.lock().expect("poisoned");
        writer
            .write_all(&body)
            .and_then(|()| writer.flush())
            .map_err(|error| HostCallError::Transport(error.to_string()))
    }

    /// How many requests have gone out on this link, the handshake included.
    ///
    /// Only the tests ask, and what they ask is whether a *client* used this
    /// link rather than opening its own connection. Nothing else can answer
    /// that: the answer to a request looks the same over either protocol,
    /// which is the point of the translation.
    #[doc(hidden)]
    pub fn calls_made(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed).saturating_sub(1)
    }

    /// Whether this link still has a reader behind it.
    ///
    /// A link whose reader has gone answers nothing, so a holder checks before
    /// handing it out rather than letting the next caller discover it.
    pub fn is_live(&self) -> bool {
        Arc::strong_count(&self.pending) > 1
    }

    fn write_request(
        &self,
        id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), HostCallError> {
        let mut body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|_| HostCallError::InvalidRequest)?;
        body.push(b'\n');
        let mut writer = self.writer.lock().expect("poisoned");
        writer
            .write_all(&body)
            .and_then(|()| writer.flush())
            .map_err(|error| HostCallError::Transport(error.to_string()))
    }
}

/// Read answers until the connection ends, handing each to whoever waits.
///
/// Requests arriving the other way are ignored here: the owner only asks a
/// client something on a *subscribed* connection, and this is not one. Reading
/// them would mean answering them, and this link has nobody to ask.
fn read_answers(stream: TcpStream, pending: Arc<Mutex<HashMap<String, SyncSender<Answer>>>>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").and_then(|id| id.as_str()) else {
            // A notification, or an answer with no id to pair it with. Neither
            // is something a caller is blocked on.
            continue;
        };
        let waiter = pending.lock().expect("poisoned").remove(id);
        let Some(waiter) = waiter else {
            // Nobody is waiting: a caller that timed out and walked away. Its
            // answer has nowhere to go, and that is the correct end for it.
            continue;
        };
        let answer = match message.get("error") {
            Some(error) => match serde_json::from_value::<JsonRpcError>(error.clone()) {
                Ok(error) => Err(error),
                Err(_) => Err(JsonRpcError::internal_error(
                    "the owner sent an error this build cannot read",
                )),
            },
            None => Ok(message
                .get("result")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}))),
        };
        let _ = waiter.send(answer);
    }
    // Dropping the table drops every waiter's sender, so everyone blocked on
    // one wakes with a disconnection rather than waiting out its own deadline.
    pending.lock().expect("poisoned").clear();
}

/// The links this process holds, one per endpoint generation.
fn links() -> &'static Mutex<HashMap<(u32, u16, String), Arc<AcpLink>>> {
    static LINKS: OnceLock<Mutex<HashMap<(u32, u16, String), Arc<AcpLink>>>> = OnceLock::new();
    LINKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(endpoint: &BackgroundIpcEndpoint) -> (u32, u16, String) {
    (endpoint.pid, endpoint.port, endpoint.token.clone())
}

/// The link to this endpoint, opening one if there is none or the last one
/// died.
pub fn link_to(endpoint: &BackgroundIpcEndpoint) -> Result<Arc<AcpLink>, HostCallError> {
    let existing = links()
        .lock()
        .expect("poisoned")
        .get(&key(endpoint))
        .cloned();
    if let Some(link) = existing {
        if link.is_live() {
            return Ok(link);
        }
        // Dead: drop it before opening a replacement, so two callers arriving
        // together do not both leave one behind.
        links().lock().expect("poisoned").remove(&key(endpoint));
    }
    let opened = Arc::new(AcpLink::open(endpoint.port, &endpoint.token)?);
    let mut held = links().lock().expect("poisoned");
    // Another caller may have opened one while this one was handshaking. Theirs
    // is as good as this one, and keeping both would mean two connections doing
    // one connection's work.
    let link = held
        .entry(key(endpoint))
        .or_insert_with(|| Arc::clone(&opened));
    Ok(Arc::clone(link))
}

/// Drop the link to an endpoint, if there is one.
pub fn close_link(endpoint: &BackgroundIpcEndpoint) {
    links().lock().expect("poisoned").remove(&key(endpoint));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A stand-in owner: answers `initialize`, then whatever the test says.
    fn fake_owner(answers: Vec<serde_json::Value>) -> (u16, std::thread::JoinHandle<()>) {
        fake_owner_answering_after(answers, Duration::ZERO)
    }

    /// The same, taking `delay` over every answer but the handshake -- which
    /// is how a caller is made to genuinely time out rather than by racing it.
    fn fake_owner_answering_after(
        answers: Vec<serde_json::Value>,
        delay: Duration,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let mut answers = answers.into_iter();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                    return;
                };
                let id = request["id"].clone();
                let body = if request["method"] == "initialize" {
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    std::thread::sleep(delay);
                    match answers.next() {
                        Some(mut answer) => {
                            answer["id"] = id;
                            answer
                        }
                        // Out of scripted answers: say nothing, which is what
                        // a hung owner looks like.
                        None => continue,
                    }
                };
                let mut bytes = serde_json::to_vec(&body).expect("serialize");
                bytes.push(b'\n');
                if writer.write_all(&bytes).is_err() {
                    return;
                }
                let _ = writer.flush();
            }
        });
        (port, handle)
    }

    #[test]
    fn queued_reply_mapping_uses_advertised_enqueue_with_old_worker_fallback() {
        for (methods, expected) in [
            (vec!["_session/status"], "session/prompt"),
            (
                vec!["_session/status", crate::session_ext::method::ENQUEUE],
                crate::session_ext::method::ENQUEUE,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = crate::BackgroundIpcEndpoint {
                pid: std::process::id(),
                port: listener.local_addr().unwrap().port(),
                token: format!("enqueue-test-{expected}"),
            };
            let owner = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let initialized: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(initialized["method"], "initialize");
                writeln!(writer, "{}", serde_json::json!({"jsonrpc": "2.0", "id": initialized["id"], "result": {"_meta": {"rebon": {"methods": methods}}}})).unwrap();
                line.clear();
                reader.read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], expected);
                assert_eq!(request["params"]["sessionId"], "session-test");
                assert_eq!(request["params"]["prompt"][0]["text"], "queued");
                assert_eq!(request["params"]["_meta"]["rebon"]["commandId"], "retry-id");
                writeln!(
                    writer,
                    "{}",
                    serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": {}})
                )
                .unwrap();
            });
            crate::protocol_probe::remember(&endpoint, crate::protocol_probe::OwnerProtocol::Acp);
            let handle =
                crate::OwnerHandle::for_worker("session-test", Some("job-test"), &endpoint);
            handle
                .send_fallibly(
                    crate::BackgroundIpcRequest::Reply {
                        message: "queued".into(),
                        images: vec![],
                    },
                    Some("retry-id".into()),
                    Duration::from_secs(2),
                )
                .expect("bounded enqueue acknowledgement");
            owner.join().unwrap();
            close_link(&endpoint);
            crate::protocol_probe::forget(&endpoint);
        }
    }

    #[test]
    fn a_link_carries_several_calls() {
        let (port, _owner) = fake_owner(vec![
            serde_json::json!({"jsonrpc": "2.0", "result": {"first": true}}),
            serde_json::json!({"jsonrpc": "2.0", "result": {"second": true}}),
        ]);
        let link = AcpLink::open(port, "token").expect("open");
        assert_eq!(
            link.call(
                "_session/ping",
                serde_json::json!({}),
                Duration::from_secs(2)
            )
            .expect("first"),
            serde_json::json!({"first": true})
        );
        assert_eq!(
            link.call(
                "_session/ping",
                serde_json::json!({}),
                Duration::from_secs(2)
            )
            .expect("second"),
            serde_json::json!({"second": true})
        );
    }

    /// An error answer comes back as the typed failure the caller matches on,
    /// through the one table both halves read.
    #[test]
    fn an_error_answer_becomes_the_typed_failure() {
        let (port, _owner) = fake_owner(vec![serde_json::json!({
            "jsonrpc": "2.0",
            "error": {"code": -32011, "message": "no", "data": {"kind": "unauthenticated"}},
        })]);
        let link = AcpLink::open(port, "token").expect("open");
        let failure = link
            .call(
                "_session/ping",
                serde_json::json!({}),
                Duration::from_secs(2),
            )
            .expect_err("an error answer");
        assert!(matches!(failure, HostCallError::Unauthenticated));
    }

    /// A deadline that passes is `HostUnanswered`, which is what tells the
    /// caller above to send a `CancelCall` rather than to retry blindly.
    #[test]
    fn a_deadline_that_passes_is_host_unanswered() {
        // No scripted answers, so the owner reads and says nothing.
        let (port, _owner) = fake_owner(Vec::new());
        let link = AcpLink::open(port, "token").expect("open");
        let failure = link
            .call(
                "_session/ping",
                serde_json::json!({}),
                Duration::from_millis(100),
            )
            .expect_err("nothing answers");
        assert!(matches!(failure, HostCallError::HostUnanswered));
    }

    /// A link whose owner went away releases whoever is waiting, rather than
    /// leaving them to wait out their own deadlines.
    #[test]
    fn a_link_that_dies_releases_its_waiters() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let owner = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            // Answer the handshake, then hang up mid-conversation.
            let _ = reader.read_line(&mut line);
            let request: serde_json::Value = serde_json::from_str(&line).expect("json");
            let body = serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": {}});
            let mut bytes = serde_json::to_vec(&body).expect("serialize");
            bytes.push(b'\n');
            let _ = writer.write_all(&bytes);
            let _ = writer.flush();
            line.clear();
            let _ = reader.read_line(&mut line);
            // Gone, without answering.
        });
        let link = AcpLink::open(port, "token").expect("open");

        let started = std::time::Instant::now();
        let failure = link
            .call(
                "_session/ping",
                serde_json::json!({}),
                // A deadline far longer than the test should take, so passing
                // it would mean the waiter waited it out instead of being
                // released.
                Duration::from_secs(30),
            )
            .expect_err("the owner hung up");
        assert!(
            matches!(failure, HostCallError::Transport(_)),
            "expected a transport failure, got {failure:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the waiter waited out its own deadline instead of being released"
        );
        drop(owner.join());
    }

    /// A caller that times out leaves an answer with nowhere to go, and the
    /// link keeps working for the next one.
    ///
    /// The two halves are both needed: the late answer must not be delivered
    /// to whoever asks next (it belongs to a question nobody is holding any
    /// more), and it must not wedge the reader. This is the ordinary shape of
    /// a slow owner, not an edge case.
    #[test]
    fn a_late_answer_goes_nowhere_and_the_link_keeps_working() {
        let (port, _owner) = fake_owner_answering_after(
            vec![
                serde_json::json!({"jsonrpc": "2.0", "result": {"late": true}}),
                serde_json::json!({"jsonrpc": "2.0", "result": {"after": true}}),
            ],
            Duration::from_millis(250),
        );
        let link = AcpLink::open(port, "token").expect("open");

        let timed_out = link
            .call(
                "_session/ping",
                serde_json::json!({}),
                Duration::from_millis(50),
            )
            .expect_err("the owner is slower than the deadline");
        assert!(matches!(timed_out, HostCallError::HostUnanswered));
        assert!(
            link.pending.lock().expect("poisoned").is_empty(),
            "a caller that walked away left its waiter behind"
        );

        // The late answer arrives during this, for an id nobody holds.
        let next = link
            .call(
                "_session/ping",
                serde_json::json!({}),
                Duration::from_secs(5),
            )
            .expect("the link still works");
        assert_eq!(
            next,
            serde_json::json!({"after": true}),
            "the next caller was handed the previous caller's answer"
        );
    }
}
