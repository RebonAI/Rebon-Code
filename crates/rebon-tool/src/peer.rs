//! Naming another session, once.
//!
//! Three spellings reach the same session: its handle (`@repo-qrst`), its id
//! (`session:abcde-fghij-klmno-pqrst`, or a prefix of it), and the name its job
//! was given. Every tool that addresses a peer goes through [`resolve_peer`] so
//! the three cannot drift into three different matching rules.
//!
//! The order is most specific first, and the first step that matches anything
//! decides: a handle, then a handle prefix, then an id, then a name. A step that
//! matches several sessions is an error rather than a reason to fall through —
//! continuing would deliver the message to a session the caller did not name.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::PeerInfo;

/// How much of a session id or handle a caller may abbreviate. Short enough to
/// type, long enough that two sessions rarely share it.
pub const MIN_SESSION_PREFIX_CHARS: usize = 6;

/// How many cross-session hand-offs a message may make before it stops being
/// allowed to start a turn.
///
/// Two agents that keep replying to each other will do so until something
/// stops them; a turn that answers a message is already one hop deeper, so the
/// count only has to be small. Past this the message is still delivered into a
/// running turn — a person is watching that one — but it may not wake an idle
/// session into another round.
pub const MAX_AGENT_MESSAGE_HOPS: u8 = 4;

/// How many turns one source session may start per hour.
///
/// Separate from the hop count because it bounds a different thing: hops bound
/// how far a chain runs, this bounds how often one session may repeatedly poke
/// this one even when every message it sends is a fresh conversation.
pub const AGENT_MESSAGE_WAKES_PER_HOUR: usize = 20;

const WAKE_WINDOW_MS: u64 = 60 * 60 * 1000;

/// What one session has been told this turn, and how often one peer may wake it.
///
/// Two different bounds share this because they are answered from the same two
/// facts: which peer spoke to this session, and how recently.
///
/// The hop count is turn-scoped — a reply inherits the depth of the message it
/// answers, and the next user prompt starts again at zero — so [`end_turn`]
/// clears it. The wake counts are not: they exist to slow a peer down across
/// turns, and clearing them per turn would make them count nothing.
///
/// [`end_turn`]: AgentReplyLedger::end_turn
#[derive(Debug, Default)]
pub struct AgentReplyLedger {
    state: Mutex<LedgerState>,
}

#[derive(Debug, Default)]
struct LedgerState {
    /// The peer whose message this turn is answering, and how deep it already
    /// came. `None` for a turn the person started.
    inbound: Option<(String, u8)>,
    /// Turn-start times per source session, oldest first.
    wakes: HashMap<String, Vec<u64>>,
}

/// Whether a message may start a turn in the session it reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeAdmission {
    /// The message may wake an idle session.
    Allowed,
    /// Too many hand-offs: deliver, but do not start another round.
    TooManyHops { hops: u8 },
    /// This peer has started too many turns here lately.
    RateLimited { window_ms: u64 },
}

impl WakeAdmission {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

impl AgentReplyLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that this turn is answering `from_session_id` at depth `hops`.
    ///
    /// The last message wins: a turn that received two messages answers the one
    /// it read second, which is the one whose context is freshest.
    pub fn note_inbound(&self, from_session_id: &str, hops: u8) {
        if from_session_id.trim().is_empty() {
            return;
        }
        self.state
            .lock()
            .expect("agent reply ledger poisoned")
            .inbound = Some((from_session_id.to_string(), hops));
    }

    /// The depth a reply to `target_session_id` should carry.
    ///
    /// Zero for anyone this turn was not answering — a conversation the sender
    /// started itself is not a hand-off, however deep the turn it happens in.
    pub fn reply_hops(&self, target_session_id: &str) -> u8 {
        let state = self.state.lock().expect("agent reply ledger poisoned");
        match state.inbound.as_ref() {
            Some((from, hops)) if from == target_session_id => hops.saturating_add(1),
            _ => 0,
        }
    }

    /// Whether one more message from `from_session_id` may start a turn now.
    ///
    /// Records the wake when it is allowed, so the caller does not have to
    /// remember to.
    pub fn admit_wake(&self, from_session_id: &str, hops: u8, now_ms: u64) -> WakeAdmission {
        if hops > MAX_AGENT_MESSAGE_HOPS {
            return WakeAdmission::TooManyHops { hops };
        }
        let mut state = self.state.lock().expect("agent reply ledger poisoned");
        let wakes = state.wakes.entry(from_session_id.to_string()).or_default();
        wakes.retain(|started| now_ms.saturating_sub(*started) < WAKE_WINDOW_MS);
        if wakes.len() >= AGENT_MESSAGE_WAKES_PER_HOUR {
            return WakeAdmission::RateLimited {
                window_ms: WAKE_WINDOW_MS,
            };
        }
        wakes.push(now_ms);
        WakeAdmission::Allowed
    }

    /// The turn is over; what it was answering no longer applies.
    pub fn end_turn(&self) {
        self.state
            .lock()
            .expect("agent reply ledger poisoned")
            .inbound = None;
    }
}

/// The ledger one session records arrivals in, for the process that hosts it.
///
/// A session's inbox writes it and its turn's `ToolContext` reads it, and those
/// sit in different crates — the inbox in the session runtime, the reader in
/// the task tools, which may not depend on it. So the ledger is reached by
/// session id rather than handed over: one process hosts one session, and a
/// second process hosting the same id is a different process with a ledger of
/// its own, which is correct — what it has been told is not shared state.
///
/// Named rather than implied, so the two ends are visibly the same fact.
pub fn session_reply_ledger(session_id: &str) -> Arc<AgentReplyLedger> {
    static LEDGERS: OnceLock<Mutex<HashMap<String, Arc<AgentReplyLedger>>>> = OnceLock::new();
    LEDGERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("agent reply ledgers poisoned")
        .entry(session_id.to_string())
        .or_default()
        .clone()
}

/// Why [`resolve_peer`] could not name exactly one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerResolveError {
    /// No running session answers to this at all.
    NotFound { requested: String },
    /// Several do; each candidate is spelled the way a caller should send to it.
    Ambiguous {
        requested: String,
        candidates: Vec<String>,
    },
}

impl PeerResolveError {
    /// The candidate list, spelled `@handle` where there is one and
    /// `session:<id>` otherwise.
    pub fn candidates(&self) -> &[String] {
        match self {
            Self::NotFound { .. } => &[],
            Self::Ambiguous { candidates, .. } => candidates,
        }
    }
}

impl std::fmt::Display for PeerResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { requested } => {
                write!(f, "no running session matches `{requested}`")
            }
            Self::Ambiguous {
                requested,
                candidates,
            } => write!(
                f,
                "`{requested}` matches {} sessions: {}",
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }
}

/// A session spelled the way a caller should write it.
pub fn peer_target_label(peer: &PeerInfo) -> String {
    match peer.handle.as_deref().map(str::trim) {
        Some(handle) if !handle.is_empty() => format!("@{handle}"),
        _ => format!("session:{}", peer.session_id),
    }
}

/// Strip one leading `@`, the sigil a handle is written with.
pub fn strip_handle_sigil(requested: &str) -> &str {
    requested.strip_prefix('@').unwrap_or(requested)
}

/// The one session `requested` names.
///
/// A leading `@` is accepted and ignored, so the same string works in a tool
/// argument and in what a person types at the prompt.
pub fn resolve_peer<'a>(
    peers: &'a [PeerInfo],
    requested: &str,
) -> Result<&'a PeerInfo, PeerResolveError> {
    let requested = strip_handle_sigil(requested).trim();
    let not_found = || PeerResolveError::NotFound {
        requested: requested.to_string(),
    };
    if requested.is_empty() {
        return Err(not_found());
    }

    let by_handle: Vec<&PeerInfo> = peers
        .iter()
        .filter(|peer| {
            peer.handle
                .as_deref()
                .is_some_and(|handle| handle.eq_ignore_ascii_case(requested))
        })
        .collect();
    if let Some(peer) = resolve_step(by_handle, requested)? {
        return Ok(peer);
    }

    if requested.len() >= MIN_SESSION_PREFIX_CHARS {
        let by_handle_prefix: Vec<&PeerInfo> = peers
            .iter()
            .filter(|peer| {
                peer.handle.as_deref().is_some_and(|handle| {
                    handle
                        .to_ascii_lowercase()
                        .starts_with(&requested.to_ascii_lowercase())
                })
            })
            .collect();
        if let Some(peer) = resolve_step(by_handle_prefix, requested)? {
            return Ok(peer);
        }

        if let Some(peer) = peers.iter().find(|peer| peer.session_id == requested) {
            return Ok(peer);
        }
        let by_id_prefix: Vec<&PeerInfo> = peers
            .iter()
            .filter(|peer| peer.session_id.starts_with(requested))
            .collect();
        if let Some(peer) = resolve_step(by_id_prefix, requested)? {
            return Ok(peer);
        }
    }

    let by_name: Vec<&PeerInfo> = peers
        .iter()
        .filter(|peer| {
            peer.name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(requested))
        })
        .collect();
    resolve_step(by_name, requested)?.ok_or_else(not_found)
}

/// One step of [`resolve_peer`]: a unique hit ends the search, several are an
/// ambiguity, none passes the decision to the next step.
fn resolve_step<'a>(
    matches: Vec<&'a PeerInfo>,
    requested: &str,
) -> Result<Option<&'a PeerInfo>, PeerResolveError> {
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => Err(PeerResolveError::Ambiguous {
            requested: requested.to_string(),
            candidates: matches.into_iter().map(peer_target_label).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(session_id: &str, handle: Option<&str>, name: Option<&str>) -> PeerInfo {
        PeerInfo {
            session_id: session_id.into(),
            handle: handle.map(str::to_string),
            name: name.map(str::to_string),
            cwd: "C:/work".into(),
            kind: "hosted".into(),
            busy: false,
            reachable: true,
            terminal_id: None,
            activity: None,
        }
    }

    fn peers() -> Vec<PeerInfo> {
        vec![
            peer("abcde-fghij-klmno-pqrst", Some("repo-qrst"), Some("worker")),
            peer("abcde-fghij-klmno-zzzz", Some("repo-zzzz"), Some("other")),
            peer("11111-22222-33333-44444", None, None),
        ]
    }

    #[test]
    fn a_full_handle_matches_whatever_its_case() {
        let peers = peers();
        for requested in ["repo-qrst", "@repo-qrst", "@REPO-QRST"] {
            let resolved = resolve_peer(&peers, requested).unwrap();
            assert_eq!(
                resolved.session_id, "abcde-fghij-klmno-pqrst",
                "{requested}"
            );
        }
    }

    #[test]
    fn a_handle_prefix_of_at_least_six_characters_matches() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "@repo-qr").unwrap().session_id,
            "abcde-fghij-klmno-pqrst"
        );
    }

    #[test]
    fn a_handle_prefix_shorter_than_six_characters_does_not_reach_the_handle_step() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "@repo"),
            Err(PeerResolveError::NotFound {
                requested: "repo".into()
            })
        );
    }

    #[test]
    fn a_full_session_id_and_a_unique_prefix_both_match() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "abcde-fghij-klmno-pqrst")
                .unwrap()
                .session_id,
            "abcde-fghij-klmno-pqrst"
        );
        assert_eq!(
            resolve_peer(&peers, "abcde-fghij-klmno-pq")
                .unwrap()
                .session_id,
            "abcde-fghij-klmno-pqrst"
        );
        assert_eq!(
            resolve_peer(&peers, "11111-22222-33333-44444")
                .unwrap()
                .session_id,
            "11111-22222-33333-44444"
        );
    }

    #[test]
    fn a_name_matches_whatever_its_case() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "Other").unwrap().session_id,
            "abcde-fghij-klmno-zzzz"
        );
        assert_eq!(
            resolve_peer(&peers, "11111-22222-33333-44444")
                .unwrap()
                .session_id,
            "11111-22222-33333-44444"
        );
    }

    #[test]
    fn the_handle_step_wins_over_a_name_that_spells_the_same_thing() {
        let peers = vec![
            peer("abcde-fghij-klmno-qqqq", Some("lead"), None),
            peer("abcde-fghij-klmno-wwww", None, Some("lead")),
        ];
        assert_eq!(
            resolve_peer(&peers, "lead").unwrap().session_id,
            "abcde-fghij-klmno-qqqq"
        );
    }

    #[test]
    fn an_ambiguous_step_lists_the_candidates_and_stops() {
        let peers = vec![
            peer("abcde-one", Some("repo-one"), Some("worker")),
            peer("abcde-two", Some("repo-only"), Some("worker")),
        ];
        let error = resolve_peer(&peers, "@repo-o").unwrap_err();
        assert_eq!(
            error,
            PeerResolveError::Ambiguous {
                requested: "repo-o".into(),
                candidates: vec!["@repo-one".into(), "@repo-only".into()],
            }
        );
        assert!(
            error.to_string().contains("@repo-one, @repo-only"),
            "{error}"
        );

        let error = resolve_peer(&peers, "worker").unwrap_err();
        assert_eq!(error.candidates(), ["@repo-one", "@repo-only"]);
    }

    #[test]
    fn an_ambiguous_id_prefix_lists_ids_when_the_peers_have_no_handle() {
        let peers = vec![
            peer("abcdef-one", None, None),
            peer("abcdef-two", None, None),
        ];
        let error = resolve_peer(&peers, "abcdef").unwrap_err();
        assert_eq!(
            error.candidates(),
            ["session:abcdef-one", "session:abcdef-two"]
        );
    }

    #[test]
    fn nothing_that_matches_reports_not_found() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "ghost"),
            Err(PeerResolveError::NotFound {
                requested: "ghost".into()
            })
        );
        assert_eq!(
            resolve_peer(&peers, ""),
            Err(PeerResolveError::NotFound {
                requested: "".into()
            })
        );
    }

    #[test]
    fn a_candidate_is_spelled_with_its_handle_when_it_has_one() {
        assert_eq!(
            peer_target_label(&peer("abcde-fghij-klmno-pqrst", Some("repo-qrst"), None)),
            "@repo-qrst"
        );
        assert_eq!(
            peer_target_label(&peer("abcde-fghij-klmno-pqrst", None, None)),
            "session:abcde-fghij-klmno-pqrst"
        );
    }

    // -----------------------------------------------------------------------
    // The reply ledger: how deep a chain may go, and how often one peer wakes.
    // -----------------------------------------------------------------------

    /// A reply to the peer whose message started this turn continues that chain;
    /// a message to anyone else starts a new one at zero.
    #[test]
    fn a_reply_carries_one_more_hop_than_the_message_it_answers() {
        let ledger = AgentReplyLedger::new();
        assert_eq!(ledger.reply_hops("sess-a"), 0, "nothing arrived yet");

        ledger.note_inbound("sess-a", 2);
        assert_eq!(ledger.reply_hops("sess-a"), 3);
        assert_eq!(
            ledger.reply_hops("sess-b"),
            0,
            "a fresh conversation is not a hand-off, however deep the turn"
        );

        ledger.end_turn();
        assert_eq!(
            ledger.reply_hops("sess-a"),
            0,
            "the next turn answers nobody"
        );
    }

    /// Two agents answering each other deepen the chain every round; past the
    /// bound the message may still join a running turn but may not start one.
    #[test]
    fn a_chain_past_the_hop_bound_may_not_wake_an_idle_session() {
        let ledger = AgentReplyLedger::new();
        assert_eq!(
            ledger.admit_wake("sess-a", MAX_AGENT_MESSAGE_HOPS, 0),
            WakeAdmission::Allowed
        );
        assert_eq!(
            ledger.admit_wake("sess-a", MAX_AGENT_MESSAGE_HOPS + 1, 0),
            WakeAdmission::TooManyHops {
                hops: MAX_AGENT_MESSAGE_HOPS + 1
            }
        );
    }

    /// The wake bound is per source session and slides: an old wake leaves the
    /// window and frees the slot again.
    #[test]
    fn one_peer_may_only_start_so_many_turns_an_hour() {
        let ledger = AgentReplyLedger::new();
        for _ in 0..AGENT_MESSAGE_WAKES_PER_HOUR {
            assert!(ledger.admit_wake("sess-a", 0, 1_000).is_allowed());
        }
        assert_eq!(
            ledger.admit_wake("sess-a", 0, 1_000),
            WakeAdmission::RateLimited {
                window_ms: WAKE_WINDOW_MS
            }
        );
        assert!(
            ledger.admit_wake("sess-b", 0, 1_000).is_allowed(),
            "another peer has its own budget"
        );

        let an_hour_later = 1_000 + WAKE_WINDOW_MS;
        assert!(
            ledger.admit_wake("sess-a", 0, an_hour_later).is_allowed(),
            "the oldest wake has left the window"
        );
    }

    /// A turn is what the hop count is scoped to; the wake counts are not, and
    /// clearing them per turn would make them count nothing.
    #[test]
    fn ending_a_turn_keeps_the_wake_counts() {
        let ledger = AgentReplyLedger::new();
        for _ in 0..AGENT_MESSAGE_WAKES_PER_HOUR {
            assert!(ledger.admit_wake("sess-a", 0, 1_000).is_allowed());
        }
        ledger.end_turn();
        assert_eq!(
            ledger.admit_wake("sess-a", 0, 1_000),
            WakeAdmission::RateLimited {
                window_ms: WAKE_WINDOW_MS
            }
        );
    }

    /// A ledger is per session: one session's depth is not another's.
    #[test]
    fn the_ledger_is_reached_by_session() {
        let first = session_reply_ledger("sess-ledger-a");
        first.note_inbound("sess-peer", 1);
        let second = session_reply_ledger("sess-ledger-b");
        assert_eq!(second.reply_hops("sess-peer"), 0);
        assert_eq!(session_reply_ledger("sess-ledger-a").reply_hops("sess-peer"), 2);
    }
}
