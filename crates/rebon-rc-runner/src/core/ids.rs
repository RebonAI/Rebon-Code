//! The ids the runner makes up, and the one rule each follows.
//!
//! Every id here is a pure function of what it names, never a counter or a
//! clock: a runner that reconnects and sends a frame again must send it
//! under the same id, or RC stores it twice.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rebon_session_host::{BackgroundIpcEndpoint, BackgroundPermissionQuerySnapshot};
use sha2::{Digest, Sha256};

/// Which numbering a stream cursor belongs to.
///
/// An owner numbers its stream from one every time it starts, so a cursor
/// is only meaningful next to the incarnation that issued it. The owner
/// names that incarnation with its `hello` epoch; an owner too old to have
/// one is told apart by its endpoint instead. Both spellings carry a
/// prefix, so an epoch can never read as a generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamIdentity(String);

impl StreamIdentity {
    /// The identity of an owner's stream.
    ///
    /// `epoch` is the `hello`'s; zero means the owner does not stamp its
    /// event log, and then `generation` (see [`endpoint_generation`]) is the
    /// only thing that tells two incarnations apart.
    pub fn of(epoch: u64, generation: &str) -> Self {
        if epoch != 0 {
            Self(format!("e{epoch:x}"))
        } else {
            Self(format!("g{generation}"))
        }
    }

    /// The identity before any `hello` has arrived on a connection.
    pub fn unannounced(generation: &str) -> Self {
        Self(format!("g{generation}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The `message_id` of a `session_message` that carries the updates
/// published at cursors `first..=last` of `identity`'s numbering.
///
/// The same range of the same stream always gets the same id, so a frame
/// resent after a reconnect is stored once. Ranges never overlap: the
/// uplink watermark hands every cursor to exactly one frame.
pub fn message_id(identity: &StreamIdentity, first: u64, last: u64) -> String {
    format!("{}.{first}-{last}", identity.as_str())
}

/// The `message_id` of one piece of a message that was split to fit the
/// frame cap.
pub fn message_piece_id(base: &str, piece: usize) -> String {
    format!("{base}.p{piece}")
}

/// A short, stable name for one endpoint generation of an owner.
///
/// The endpoint carries the owner's IPC token, which must never leave the
/// machine, so this is a digest rather than the fields themselves. Two
/// connections to the same process on the same port with the same token
/// are the same generation; a restarted worker is not.
pub fn endpoint_generation(endpoint: &BackgroundIpcEndpoint) -> String {
    let digest = digest(
        b"rebon-rc-endpoint-v1",
        &(endpoint.pid, endpoint.port, &endpoint.token),
    );
    digest[..16].to_string()
}

/// The `request_id` of the `permission_request` for `query`.
///
/// Binds the query id, the turn generation and the endpoint, the way the
/// app's phone path binds its approval nonce: an owner that is replaced, or
/// a turn that moves on, can reuse a query id, and an answer meant for the
/// old prompt must not land on the new one. The runner recomputes this from
/// the prompt the owner holds *now* before it answers, so a stale id simply
/// fails to match.
pub fn permission_request_id(query: &BackgroundPermissionQuerySnapshot) -> String {
    format!(
        "perm-{}",
        digest(
            b"rebon-rc-permission-v1",
            &(query.query_id, query.turn_generation, &query.endpoint),
        )
    )
}

/// The lease client id this runner holds on a session.
///
/// Carries the process id so `rebon agents` can tell two runners apart,
/// and the RC session so one runner's leases on two sessions are distinct.
pub fn lease_client_id(pid: u32, rc_session_id: &str) -> String {
    format!("rebon-rc:{pid}:{rc_session_id}")
}

/// A fresh random id with `prefix`, for the machine's own identity.
pub fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("the operating system provides randomness");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn digest(domain: &[u8], value: &impl serde::Serialize) -> String {
    let encoded = serde_json::to_vec(value).expect("an id's inputs serialize");
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0u8]);
    hasher.update(encoded);
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(pid: u32, port: u16, token: &str) -> BackgroundIpcEndpoint {
        BackgroundIpcEndpoint {
            pid,
            port,
            token: token.to_string(),
        }
    }

    fn query(
        query_id: u64,
        turn: u64,
        endpoint: Option<BackgroundIpcEndpoint>,
    ) -> BackgroundPermissionQuerySnapshot {
        BackgroundPermissionQuerySnapshot {
            query_id,
            turn_generation: turn,
            endpoint,
            tool: Some("Bash".into()),
            tool_call_id: None,
            session_id: None,
            title: None,
            message: None,
            tool_input: None,
            metadata: None,
            options: Vec::new(),
        }
    }

    #[test]
    fn an_identity_says_which_numbering_it_is() {
        assert_eq!(StreamIdentity::of(0x2a, "abc").as_str(), "e2a");
        assert_eq!(StreamIdentity::of(0, "abc").as_str(), "gabc");
        assert_eq!(
            StreamIdentity::unannounced("abc"),
            StreamIdentity::of(0, "abc")
        );
        assert_ne!(StreamIdentity::of(1, "x"), StreamIdentity::of(0, "x"));
    }

    #[test]
    fn a_message_id_is_a_function_of_its_range() {
        let identity = StreamIdentity::of(7, "g");
        assert_eq!(message_id(&identity, 3, 9), "e7.3-9");
        assert_eq!(message_id(&identity, 3, 9), message_id(&identity, 3, 9));
        assert_ne!(message_id(&identity, 3, 9), message_id(&identity, 3, 10));
        assert_ne!(
            message_id(&identity, 3, 9),
            message_id(&StreamIdentity::of(8, "g"), 3, 9)
        );
        assert_eq!(message_piece_id("e7.3-9", 2), "e7.3-9.p2");
        // Well inside the protocol's id limit.
        let long = message_id(&StreamIdentity::of(u64::MAX, "g"), u64::MAX, u64::MAX);
        assert!(long.len() < rebon_bridge::session_stream::MAX_FRAME_ID_BYTES);
    }

    #[test]
    fn an_endpoint_generation_is_stable_and_never_shows_the_token() {
        let first = endpoint_generation(&endpoint(10, 4000, "secret-token"));
        assert_eq!(
            first,
            endpoint_generation(&endpoint(10, 4000, "secret-token"))
        );
        assert_eq!(first.len(), 16);
        assert!(!first.contains("secret"));
        for other in [
            endpoint(11, 4000, "secret-token"),
            endpoint(10, 4001, "secret-token"),
            endpoint(10, 4000, "other-token"),
        ] {
            assert_ne!(first, endpoint_generation(&other), "{other:?}");
        }
    }

    #[test]
    fn a_permission_request_id_binds_query_turn_and_endpoint() {
        let here = Some(endpoint(10, 4000, "t"));
        let base = permission_request_id(&query(3, 1, here.clone()));
        assert!(base.starts_with("perm-"));
        assert!(base.len() <= rebon_bridge::session_stream::MAX_FRAME_ID_BYTES);
        assert_eq!(base, permission_request_id(&query(3, 1, here.clone())));
        // Anything that makes it a different prompt makes it a different id.
        assert_ne!(base, permission_request_id(&query(4, 1, here.clone())));
        assert_ne!(base, permission_request_id(&query(3, 2, here)));
        assert_ne!(
            base,
            permission_request_id(&query(3, 1, Some(endpoint(12, 4000, "t"))))
        );
        assert_ne!(base, permission_request_id(&query(3, 1, None)));
        // What the tool is does not identify the prompt; the owner does.
        let mut relabelled = query(3, 1, Some(endpoint(10, 4000, "t")));
        relabelled.tool = Some("Write".into());
        assert_eq!(base, permission_request_id(&relabelled));
    }

    #[test]
    fn lease_and_random_ids_have_their_shapes() {
        assert_eq!(lease_client_id(42, "sess_1"), "rebon-rc:42:sess_1");
        let first = random_id("env-");
        let second = random_id("env-");
        assert!(first.starts_with("env-"));
        assert_eq!(first.len(), "env-".len() + 22);
        assert_ne!(first, second);
    }
}
