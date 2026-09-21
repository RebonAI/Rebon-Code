//! Which protocol the owner at an endpoint speaks, asked once and remembered.
//!
//! During the compatibility release a client may meet either a worker that
//! speaks ACP or one that predates it. It cannot tell from anything it already
//! knows — the endpoint record is the same either way — so it asks, and the
//! asking has to be cheap enough to do on a cold connection and safe enough
//! that a wrong guess costs nothing.
//!
//! **The question is `initialize`, and the answer is judged by one key.** A
//! worker from before ACP tries to decode that frame as a legacy envelope, and
//! answers in the legacy shape:
//!
//! ```text
//! {"ok":false,"error":"missing field `token` at line 1 column 196"}
//! ```
//!
//! No `jsonrpc`, no `id`, and — this is the part that makes it usable — **it
//! does not drop the connection**. So one round trip decides, with no method
//! probing and no cost to a bad guess. Measured against a real pre-ACP binary
//! rather than assumed.
//!
//! **The write half stays open while asking.** That matters: the same probe
//! with `shutdown(Write)` makes a legacy worker reset the connection instead of
//! answering, which is a slower and less informative failure.
//!
//! Two rules the answer is read by:
//!
//! - **Anything that is not recognisably JSON-RPC is legacy.** A timeout, a
//!   closed connection, a body that will not parse: all of them mean "not the
//!   new one". That is the fail-safe direction. Deciding "legacy" wrongly
//!   costs one refused request from a client that will retry; deciding "ACP"
//!   wrongly means holding a connection open speaking a protocol the peer will
//!   never answer.
//! - **The answer is remembered per endpoint, and an endpoint is a
//!   generation.** `BackgroundIpcEndpoint` is `{pid, port, token}`, and a
//!   worker that replaces another publishes a new token — which is why the
//!   rest of this crate already calls a mismatch there an "endpoint generation"
//!   change. So remembering by the whole triple *is* remembering per
//!   generation: a replaced worker is a different key and is asked again.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::state::BackgroundIpcEndpoint;

/// How long to wait for an answer before deciding the peer is not the new one.
///
/// Generous next to a localhost round trip, which is under a millisecond once
/// the owner's accept loop stopped polling. It is a ceiling on being wrong,
/// not a budget to spend: a worker that has not answered in a second is one
/// this client should stop waiting on either way.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// What an owner turned out to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerProtocol {
    /// JSON-RPC, one connection, many requests.
    Acp,
    /// The one-shot envelope every build before ACP speaks.
    Legacy,
}

/// The answers this process has already learned, by endpoint generation.
fn memory() -> &'static Mutex<HashMap<(u32, u16, String), OwnerProtocol>> {
    static MEMORY: OnceLock<Mutex<HashMap<(u32, u16, String), OwnerProtocol>>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(endpoint: &BackgroundIpcEndpoint) -> (u32, u16, String) {
    (endpoint.pid, endpoint.port, endpoint.token.clone())
}

/// What this endpoint speaks, asking only if this process has not already
/// learned it.
pub fn owner_protocol(endpoint: &BackgroundIpcEndpoint) -> OwnerProtocol {
    if let Some(known) = memory().lock().expect("poisoned").get(&key(endpoint)) {
        return *known;
    }
    let learned = probe(endpoint.port, &endpoint.token, PROBE_TIMEOUT);
    memory()
        .lock()
        .expect("poisoned")
        .insert(key(endpoint), learned);
    learned
}

/// Record what an endpoint turned out to speak, without asking it.
///
/// For the one case the probe cannot see: it answered like the new protocol,
/// and then would not hold a connection open for it. Every call after that
/// would pay the handshake's whole timeout before falling back, so the first
/// one that pays it writes down what it learned.
pub fn remember(endpoint: &BackgroundIpcEndpoint, protocol: OwnerProtocol) {
    memory()
        .lock()
        .expect("poisoned")
        .insert(key(endpoint), protocol);
}

/// Forget what an endpoint answered.
///
/// For a caller that has reason to believe it was talking to a worker that has
/// since been replaced on the same port — which is rare, because a replacement
/// publishes a new token and is a different key already.
pub fn forget(endpoint: &BackgroundIpcEndpoint) {
    memory().lock().expect("poisoned").remove(&key(endpoint));
}

/// Ask one endpoint, without remembering the answer.
pub fn probe(port: u16, token: &str, timeout: Duration) -> OwnerProtocol {
    match ask(port, token, timeout) {
        Some(reply) => verdict(&reply),
        None => OwnerProtocol::Legacy,
    }
}

/// Read one answer to `initialize`, or `None` if there was none to read.
fn ask(port: u16, token: &str, timeout: Duration) -> Option<String> {
    let address = format!("127.0.0.1:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&address, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "probe",
        "method": "initialize",
        "params": { "_meta": { "rebon": { "token": token } } },
    });
    let mut body = serde_json::to_vec(&request).ok()?;
    body.push(b'\n');
    stream.write_all(&body).ok()?;
    stream.flush().ok()?;
    // Deliberately *not* `shutdown(Write)`. A legacy worker answers a frame it
    // cannot decode, but only while the connection is whole; with the write
    // half closed it resets instead, which turns a one-round-trip question
    // into a slower and less informative one.
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).ok()?;
    Some(reply)
}

/// Read one answer.
///
/// The discriminator is the presence of `jsonrpc`, not the absence of `ok`: a
/// peer that answers something this build has never seen is still not the new
/// protocol, and treating "unrecognised" as ACP would be the expensive
/// mistake.
fn verdict(reply: &str) -> OwnerProtocol {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(reply) else {
        return OwnerProtocol::Legacy;
    };
    match value.as_object() {
        Some(object) if object.contains_key("jsonrpc") => OwnerProtocol::Acp,
        _ => OwnerProtocol::Legacy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact answer a legacy worker gives, measured against the real
    /// binary rather than imagined. Column and all: the decoder's message
    /// carries the offset it stopped at.
    const PRE_E3_ANSWER: &str =
        r#"{"ok":false,"error":"missing field `token` at line 1 column 196"}"#;

    #[test]
    fn the_answer_a_pre_e3_worker_gives_reads_as_legacy() {
        assert_eq!(verdict(PRE_E3_ANSWER), OwnerProtocol::Legacy);
    }

    #[test]
    fn a_json_rpc_answer_reads_as_acp() {
        assert_eq!(
            verdict(r#"{"jsonrpc":"2.0","id":"probe","result":{"protocolVersion":1}}"#),
            OwnerProtocol::Acp
        );
        // An error answer is still ACP: the peer spoke the protocol, and what
        // it said about this particular request is a separate question.
        assert_eq!(
            verdict(r#"{"jsonrpc":"2.0","id":"probe","error":{"code":-32011}}"#),
            OwnerProtocol::Acp
        );
    }

    /// `jsonrpc` decides, not the absence of `ok`. A peer that answers
    /// something neither build has seen is not the new protocol, and guessing
    /// otherwise is the expensive direction.
    #[test]
    fn anything_unrecognisable_reads_as_legacy() {
        for reply in [
            "",
            "not json at all",
            "{}",
            r#"{"ok":true}"#,
            r#"{"id":"probe","result":{}}"#,
            r#"["jsonrpc"]"#,
            r#""jsonrpc""#,
        ] {
            assert_eq!(
                verdict(reply),
                OwnerProtocol::Legacy,
                "{reply:?} should not have read as ACP"
            );
        }
    }

    /// Nothing listening is not the new protocol either. Deciding "legacy"
    /// wrongly costs one refused request from a client that will retry;
    /// deciding "ACP" wrongly costs a connection held open for an answer that
    /// never comes.
    #[test]
    fn an_endpoint_that_answers_nothing_reads_as_legacy() {
        // Port 9 is discard, and nothing is bound to it here; either way this
        // gets no JSON-RPC answer.
        assert_eq!(
            probe(9, "token", Duration::from_millis(50)),
            OwnerProtocol::Legacy
        );
    }

    /// A peer that accepts and says nothing must not hold the client for
    /// longer than the probe's own deadline.
    #[test]
    fn a_silent_peer_is_bounded_by_the_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // Accept and then say nothing at all, holding the connection open.
        let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));

        let started = std::time::Instant::now();
        let verdict = probe(port, "token", Duration::from_millis(200));
        let elapsed = started.elapsed();
        assert_eq!(verdict, OwnerProtocol::Legacy);
        assert!(
            elapsed < Duration::from_secs(2),
            "a silent peer held the probe for {elapsed:?}"
        );
        drop(held.join());
    }

    /// The same endpoint is asked once. A second call reads what the first
    /// learned, which is what keeps a per-request cost from being a
    /// per-request round trip.
    #[test]
    fn an_endpoint_is_asked_once() {
        let endpoint = BackgroundIpcEndpoint {
            pid: 4242,
            port: 9,
            token: "a-token-nothing-answers".to_string(),
        };
        forget(&endpoint);
        assert_eq!(owner_protocol(&endpoint), OwnerProtocol::Legacy);
        // The second call must not reach the network at all. Nothing listens
        // on that port, so an answer arriving fast is the memory answering.
        let started = std::time::Instant::now();
        assert_eq!(owner_protocol(&endpoint), OwnerProtocol::Legacy);
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "the second ask went to the network"
        );
        forget(&endpoint);
    }

    /// A worker that replaced another publishes a new token, so it is a
    /// different key and is asked again. That is what "remember per
    /// generation" means here.
    #[test]
    fn a_replacement_worker_is_a_different_question() {
        let first = BackgroundIpcEndpoint {
            pid: 4242,
            port: 9,
            token: "first-generation".to_string(),
        };
        let replacement = BackgroundIpcEndpoint {
            token: "second-generation".to_string(),
            ..first.clone()
        };
        assert_ne!(key(&first), key(&replacement));
        forget(&first);
        forget(&replacement);
    }
}
