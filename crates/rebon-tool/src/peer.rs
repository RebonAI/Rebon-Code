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
/// clears it. What a turn answers is also bound to the turn it was recorded in:
/// a message that arrives after the executor has finished its turn must not be
/// attributed to whatever turn starts next, which may be one the person typed.
/// [`begin_turn`] therefore opens a fresh slate, and [`end_turn`] closes only
/// the turn it names: a cancelled turn still winding down after the next one
/// has begun must not close that one. The wake counts are not turn scoped:
/// they exist to slow a peer down across turns, and clearing them per turn
/// would make them count nothing.
///
/// [`end_turn`]: AgentReplyLedger::end_turn
/// [`begin_turn`]: AgentReplyLedger::begin_turn
#[derive(Debug, Default)]
pub struct AgentReplyLedger {
    state: Mutex<LedgerState>,
}

#[derive(Debug, Default)]
struct LedgerState {
    /// Bumped for every turn opened here, so a note taken during one turn is
    /// never read as belonging to the next.
    generation: u64,
    /// The turn currently open, and therefore the only one a note may belong
    /// to. `None` between turns.
    open: Option<u64>,
    /// The peer whose message the open turn is answering, how deep it already
    /// came, and the turn it was recorded for. `None` for a turn the person
    /// started.
    inbound: Option<(u64, String, u8)>,
    /// What the turn that has not started yet will be answering. Staged by the
    /// code that builds that turn — the idle path that drains the inbox, the
    /// worker claiming an agent-message prompt — and adopted by [`begin_turn`].
    /// Kept apart from `inbound` so a note cannot be read before its turn
    /// exists, and so a note taken after one turn ended is not mistaken for
    /// one about the next.
    staged: Option<(String, u8)>,
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

/// What has been decided about whether one received message may start a turn.
///
/// A message is judged when it could be the reason for a turn — it reached a
/// session with nothing running and nothing owed — and only then, because
/// judging spends a wake. One that joined a running turn, or waited behind a
/// turn already owed, was never going to start anything and is left
/// [`Self::Unjudged`]; if it outlives that turn unread it is judged before it
/// may start one, so arriving mid-turn is not a way around the bounds.
///
/// A verdict, once reached, is kept: a hosted session writes it into the
/// message's queue entry, so an admitted message is never charged twice and a
/// refused one is never asked again. Unjudged is the default and is not
/// written, so an entry from before the verdict existed reads as not yet
/// judged — never as admitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WakeVerdict {
    /// The bounds admitted it and the wake is already counted: it may start a
    /// turn.
    Admitted,
    /// A bound refused it. It is still delivered — into the next turn somebody
    /// else starts — but it may not start one.
    Refused,
    /// Not judged yet: it was going to ride in a turn that existed anyway.
    #[default]
    Unjudged,
}

impl WakeVerdict {
    /// Whether nothing has been decided yet, which is what is left unwritten.
    pub fn is_unjudged(&self) -> bool {
        matches!(self, Self::Unjudged)
    }
}

impl AgentReplyLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// A turn is starting: note what any message arriving from now on belongs
    /// to, and take over whatever the caller staged for it.
    ///
    /// Returns the generation the new turn was given; every later
    /// [`Self::note_inbound`] recorded before the next `begin_turn` belongs to
    /// it.
    pub fn begin_turn(&self) -> u64 {
        let mut state = self.state.lock().expect("agent reply ledger poisoned");
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.open = Some(generation);
        state.inbound = state
            .staged
            .take()
            .map(|(from, hops)| (generation, from, hops));
        generation
    }

    /// Say what the turn that is about to start will be answering.
    ///
    /// For the paths that know, before the turn exists, which message it
    /// carries: the idle path draining an agent message into a fresh turn, and
    /// the worker claiming an agent-message prompt. Call it immediately before
    /// the turn is submitted: the next [`Self::begin_turn`] takes it, whatever
    /// turn that is, so a value staged for a turn that then never starts would
    /// be adopted by the next one — which may be a turn the person typed.
    pub fn stage_inbound(&self, from_session_id: &str, hops: u8) {
        if from_session_id.trim().is_empty() {
            return;
        }
        self.state
            .lock()
            .expect("agent reply ledger poisoned")
            .staged = Some((from_session_id.to_string(), hops));
    }

    /// Record that the turn `generation` is answering `from_session_id` at depth
    /// `hops`.
    ///
    /// The last message wins: a turn that received two messages answers the one
    /// it read second, which is the one whose context is freshest. A note for a
    /// turn that has already ended is dropped — the executor finishes its turn
    /// before the event loop has seen it end, and a message read in that window
    /// belongs to the next turn, not the one that just finished.
    pub fn note_inbound(&self, generation: u64, from_session_id: &str, hops: u8) {
        if from_session_id.trim().is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("agent reply ledger poisoned");
        if state.open != Some(generation) {
            return;
        }
        state.inbound = Some((generation, from_session_id.to_string(), hops));
    }

    /// Record a note for whichever turn is open right now.
    ///
    /// For the paths that read a message while a turn is running without
    /// naming it: the mid-turn injection queues on both halves, and a steer
    /// into an external agent. Between turns there is no turn to attribute it
    /// to and the note is dropped, so a message that arrives after the executor
    /// finished its turn can no longer be read as belonging to the next one —
    /// which may be a turn the person typed, and must report no hops at all.
    /// The idle path stages its own depth with
    /// [`Self::stage_inbound`] instead.
    pub fn note_inbound_now(&self, from_session_id: &str, hops: u8) {
        if from_session_id.trim().is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("agent reply ledger poisoned");
        let Some(generation) = state.open else {
            return;
        };
        state.inbound = Some((generation, from_session_id.to_string(), hops));
    }

    /// The depth a reply to `target_session_id` should carry.
    ///
    /// Zero for anyone this turn was not answering — a conversation the sender
    /// started itself is not a hand-off, however deep the turn it happens in.
    pub fn reply_hops(&self, target_session_id: &str) -> u8 {
        let state = self.state.lock().expect("agent reply ledger poisoned");
        let Some((generation, from, hops)) = state.inbound.as_ref() else {
            return 0;
        };
        // A note from a turn that has ended cannot be answered any more: the
        // turn it belonged to is over, so a reply written now is a fresh
        // conversation rather than the next link in that chain.
        if state.open != Some(*generation) || from != target_session_id {
            return 0;
        }
        hops.saturating_add(1)
    }

    /// Whether one more message from `from_session_id` may start a turn now.
    ///
    /// Records the wake when it is allowed, so the caller does not have to
    /// remember to.
    pub fn admit_wake(&self, from_session_id: &str, hops: u8, now_ms: u64) -> WakeAdmission {
        let mut state = self.state.lock().expect("agent reply ledger poisoned");
        admit_wake_in(
            state.wakes.entry(from_session_id.to_string()).or_default(),
            hops,
            now_ms,
        )
    }

    /// Judge whether a received message may start a turn, spending a wake if
    /// it may.
    pub fn judge_wake(&self, from_session_id: &str, hops: u8, now_ms: u64) -> WakeVerdict {
        let mut state = self.state.lock().expect("agent reply ledger poisoned");
        judge_wake_in(
            state.wakes.entry(from_session_id.to_string()).or_default(),
            from_session_id,
            hops,
            now_ms,
        )
    }

    /// Turn `generation` is over; what it was answering no longer applies.
    ///
    /// Only that turn: a cancelled turn's executor can still be winding down
    /// when the next turn begins, and its end must not wipe what the new turn
    /// is answering.
    pub fn end_turn(&self, generation: u64) {
        let mut state = self.state.lock().expect("agent reply ledger poisoned");
        if state.open != Some(generation) {
            return;
        }
        state.open = None;
        state.inbound = None;
    }
}

/// The wake bounds applied to one peer's record of the turns it started here,
/// oldest first: the hop bound, then the sliding hour. Records the wake when it
/// is allowed.
///
/// The one rule, whoever keeps the record: the ledger keeps it in memory for a
/// session hosted in this process, and a hosted session keeps it in its job
/// record, so a worker that exits and a new one that starts — or a sender
/// writing to a session with no worker at all — count the same hour.
pub fn admit_wake_in(wakes: &mut Vec<u64>, hops: u8, now_ms: u64) -> WakeAdmission {
    if hops > MAX_AGENT_MESSAGE_HOPS {
        return WakeAdmission::TooManyHops { hops };
    }
    wakes.retain(|started| now_ms.saturating_sub(*started) < WAKE_WINDOW_MS);
    if wakes.len() >= AGENT_MESSAGE_WAKES_PER_HOUR {
        return WakeAdmission::RateLimited {
            window_ms: WAKE_WINDOW_MS,
        };
    }
    wakes.push(now_ms);
    WakeAdmission::Allowed
}

/// [`admit_wake_in`] as a verdict, reporting a refusal.
///
/// The one place a refusal is reported, so every half of a session logs the
/// same sentence for it.
pub fn judge_wake_in(
    wakes: &mut Vec<u64>,
    from_session_id: &str,
    hops: u8,
    now_ms: u64,
) -> WakeVerdict {
    let admission = admit_wake_in(wakes, hops, now_ms);
    if admission.is_allowed() {
        return WakeVerdict::Admitted;
    }
    tracing::info!(
        from = %from_session_id,
        hops,
        ?admission,
        "rebon: a cross-session message will wait instead of starting a turn"
    );
    WakeVerdict::Refused
}

/// Whether a peer's record of turn starts still counts against the hour at
/// `now_ms`; a record with nothing left in the window can be forgotten.
pub fn wakes_still_count(wakes: &[u64], now_ms: u64) -> bool {
    wakes
        .iter()
        .any(|started| now_ms.saturating_sub(*started) < WAKE_WINDOW_MS)
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
        /// For a tool: `@handle` where there is one and `session:<id>`
        /// otherwise.
        candidates: Vec<String>,
        /// For a person at the prompt, which reads `@` and a word and nothing
        /// else: `@handle`, or `@` and as much of the id as it takes to name
        /// that session alone. Each one resolves to its own candidate.
        typed_candidates: Vec<String>,
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
                ..
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

/// The session doing the addressing, as far as it knows itself.
///
/// The peer list never contains the caller, yet a caller can name itself — by
/// its handle, its id, or an abbreviation of either — and an abbreviation can
/// name it and another session at once. [`resolve_peer_or_self`] puts it back
/// in the list so that such a name is judged by the same steps as any other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OwnSession<'a> {
    pub session_id: Option<&'a str>,
    pub handle: Option<&'a str>,
}

/// Who a name resolved to once the caller itself is counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTarget<'a> {
    /// Another session.
    Peer(&'a PeerInfo),
    /// The caller: a refusal, not something to deliver to.
    OwnSession,
}

/// The one session `requested` names.
///
/// A leading `@` is accepted and ignored, so the same string works in a tool
/// argument and in what a person types at the prompt.
pub fn resolve_peer<'a>(
    peers: &'a [PeerInfo],
    requested: &str,
) -> Result<&'a PeerInfo, PeerResolveError> {
    match resolve_peer_or_self(peers, OwnSession::default(), requested)? {
        PeerTarget::Peer(peer) => Ok(peer),
        PeerTarget::OwnSession => unreachable!("no own session was given"),
    }
}

/// The one session `requested` names, counting the caller as one of them.
///
/// This is the rule every sender applies, so one written name means one thing
/// whoever writes it: the caller's exact handle or id is the caller; an
/// abbreviation that only the caller answers to is the caller too; one that the
/// caller and another session both answer to is an ambiguity listing both; and
/// one that only others answer to is resolved among them as [`resolve_peer`]
/// would. A peer entry carrying the caller's own id is ignored — it is the
/// caller, already counted.
pub fn resolve_peer_or_self<'a>(
    peers: &'a [PeerInfo],
    own: OwnSession<'_>,
    requested: &str,
) -> Result<PeerTarget<'a>, PeerResolveError> {
    let own_id = own.session_id.map(str::trim).filter(|id| !id.is_empty());
    let own_handle = own
        .handle
        .map(str::trim)
        .filter(|handle| !handle.is_empty());
    let own_entry = (own_id.is_some() || own_handle.is_some()).then(|| PeerInfo {
        session_id: own_id.unwrap_or_default().to_string(),
        handle: own_handle.map(str::to_string),
        name: None,
        cwd: String::new(),
        kind: String::new(),
        busy: false,
        reachable: true,
        terminal_id: None,
        activity: None,
    });
    let mut everyone: Vec<&PeerInfo> = peers
        .iter()
        .filter(|peer| own_id.is_none_or(|own_id| peer.session_id != own_id))
        .collect();
    if let Some(own_entry) = own_entry.as_ref() {
        everyone.push(own_entry);
    }
    let is_own = |entry: &PeerInfo| {
        own_entry
            .as_ref()
            .is_some_and(|own_entry| std::ptr::eq(entry, own_entry))
    };

    let requested = strip_handle_sigil(requested).trim();
    match matches_for(&everyone, requested) {
        Matches::None => Err(PeerResolveError::NotFound {
            requested: requested.to_string(),
        }),
        Matches::One(index) if is_own(everyone[index]) => Ok(PeerTarget::OwnSession),
        Matches::One(index) => {
            // `everyone` borrows from `peers` for every entry but the caller's.
            let found = everyone[index];
            let peer = peers
                .iter()
                .find(|peer| std::ptr::eq(*peer, found))
                .expect("a match that is not the caller is one of the peers");
            Ok(PeerTarget::Peer(peer))
        }
        Matches::Several(indices) => {
            let label = |index: usize, spelled: String| {
                if is_own(everyone[index]) {
                    format!("{spelled} (this session)")
                } else {
                    spelled
                }
            };
            Err(PeerResolveError::Ambiguous {
                requested: requested.to_string(),
                candidates: indices
                    .iter()
                    .map(|&index| label(index, peer_target_label(everyone[index])))
                    .collect(),
                typed_candidates: indices
                    .iter()
                    .map(|&index| label(index, typed_label(&everyone, index)))
                    .collect(),
            })
        }
    }
}

/// `entry` spelled the way a person types it at the prompt, which accepts `@`
/// and a word and nothing else — so not `session:<id>`.
///
/// The handle when it names this session alone; otherwise the shortest id
/// prefix that does, so a session without a handle is still one word away.
fn typed_label(everyone: &[&PeerInfo], index: usize) -> String {
    let entry = everyone[index];
    let names_it =
        |spelled: &str| matches!(matches_for(everyone, spelled), Matches::One(hit) if hit == index);
    if let Some(handle) = entry.handle.as_deref().map(str::trim) {
        if !handle.is_empty() && names_it(handle) {
            return format!("@{handle}");
        }
    }
    let id = entry.session_id.as_str();
    id.char_indices()
        .map(|(at, ch)| &id[..at + ch.len_utf8()])
        .filter(|prefix| prefix.chars().count() >= MIN_SESSION_PREFIX_CHARS)
        .find(|prefix| names_it(prefix))
        .map(|prefix| format!("@{prefix}"))
        .unwrap_or_else(|| format!("@{id}"))
}

/// What one lookup matched, by position in the list it was given.
enum Matches {
    None,
    One(usize),
    Several(Vec<usize>),
}

/// The search [`resolve_peer_or_self`] runs, step by step.
fn matches_for(everyone: &[&PeerInfo], requested: &str) -> Matches {
    if requested.is_empty() {
        return Matches::None;
    }
    let lowered = requested.to_ascii_lowercase();
    let handle_of = |peer: &PeerInfo| {
        peer.handle
            .as_deref()
            .map(str::trim)
            .filter(|handle| !handle.is_empty())
            .map(str::to_ascii_lowercase)
    };
    let step = |keep: &dyn Fn(&PeerInfo) -> bool| -> Option<Matches> {
        let hits: Vec<usize> = everyone
            .iter()
            .enumerate()
            .filter(|(_, peer)| keep(peer))
            .map(|(index, _)| index)
            .collect();
        match hits.as_slice() {
            [] => None,
            [one] => Some(Matches::One(*one)),
            _ => Some(Matches::Several(hits)),
        }
    };

    if let Some(found) = step(&|peer| handle_of(peer).is_some_and(|handle| handle == lowered)) {
        return found;
    }
    if requested.chars().count() >= MIN_SESSION_PREFIX_CHARS {
        if let Some(found) =
            step(&|peer| handle_of(peer).is_some_and(|handle| handle.starts_with(&lowered)))
        {
            return found;
        }
        // An exact id ends the search even where it is also a prefix of a
        // longer id: it names one session in full.
        if let Some(found) =
            step(&|peer| !peer.session_id.is_empty() && peer.session_id == requested)
        {
            return found;
        }
        if let Some(found) =
            step(&|peer| !peer.session_id.is_empty() && peer.session_id.starts_with(requested))
        {
            return found;
        }
    }
    step(&|peer| {
        peer.name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(requested))
    })
    .unwrap_or(Matches::None)
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
        // Both assertions are name lookups: a case-folded name must reach the
        // name step, and a name that is not an id must still resolve by name.
        assert_eq!(
            resolve_peer(&peers, "Other").unwrap().session_id,
            "abcde-fghij-klmno-zzzz"
        );
        assert_eq!(
            resolve_peer(&peers, "WORKER").unwrap().session_id,
            "abcde-fghij-klmno-pqrst"
        );
    }

    /// The most specific step decides on its own: two sessions sharing a full
    /// handle is an error even though a longer string could have distinguished
    /// them, because the caller named that handle and nothing else.
    #[test]
    fn an_ambiguous_full_handle_lists_the_candidates_and_stops() {
        let peers = vec![
            peer("abcde-one", Some("repo-qrst"), Some("worker")),
            peer("abcde-two", Some("repo-qrst"), Some("other")),
        ];
        let error = resolve_peer(&peers, "@repo-qrst").unwrap_err();
        assert_eq!(
            error,
            PeerResolveError::Ambiguous {
                requested: "repo-qrst".into(),
                candidates: vec!["@repo-qrst".into(), "@repo-qrst".into()],
                typed_candidates: vec!["@abcde-o".into(), "@abcde-t".into()],
            }
        );
        assert_eq!(
            error.candidates().len(),
            2,
            "an ambiguous step never falls through to a step that would match"
        );
    }

    /// Falling through after an ambiguity would deliver a message the caller did
    /// not address: the second peer is also named `worker`, so the name step
    /// would have found something, and the answer must still be the ambiguity.
    #[test]
    fn an_ambiguous_step_reports_ambiguity_even_when_a_later_step_would_match() {
        let peers = vec![
            peer("abcdef-one", Some("repo-alpha"), Some("worker")),
            peer("abcdef-two", Some("repo-alpine"), Some("worker")),
            peer("zzzzzz-three", None, Some("repo-al")),
        ];
        // The handle prefix is what is ambiguous; the name step would otherwise
        // have matched `repo-al` uniquely.
        let error = resolve_peer(&peers, "repo-al").unwrap_err();
        assert_eq!(
            error.candidates(),
            ["@repo-alpha", "@repo-alpine"],
            "the handle-prefix step decides, and it matched two"
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
                typed_candidates: vec!["@repo-one".into(), "@repo-only".into()],
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

    /// The prompt reads `@` and a word, so `session:<id>` cannot be typed: a
    /// session without a handle is offered as the shortest id prefix that names
    /// it alone, and that prefix really does resolve to it.
    #[test]
    fn a_typed_candidate_without_a_handle_is_an_id_prefix_that_names_it_alone() {
        let peers = vec![
            peer("abcdef-1111-x", None, Some("worker")),
            peer("abcdef-1112-y", None, Some("worker")),
            peer("zzzzzz-0000-z", Some("repo-zzzz"), Some("worker")),
        ];
        let PeerResolveError::Ambiguous {
            typed_candidates, ..
        } = resolve_peer(&peers, "worker").unwrap_err()
        else {
            panic!("expected an ambiguity");
        };
        assert_eq!(
            typed_candidates,
            ["@abcdef-1111", "@abcdef-1112", "@repo-zzzz"]
        );
        for (typed, expected) in typed_candidates.iter().zip(&peers) {
            assert_eq!(
                resolve_peer(&peers, typed).unwrap().session_id,
                expected.session_id,
                "{typed}"
            );
        }
    }

    fn own<'a>(session_id: &'a str, handle: &'a str) -> OwnSession<'a> {
        OwnSession {
            session_id: Some(session_id),
            handle: Some(handle),
        }
    }

    /// The caller's own handle or id, written in full, is the caller.
    #[test]
    fn the_callers_full_handle_or_id_is_the_caller() {
        let peers = vec![peer("c3d4c3d4-peer", Some("reboncode-c3d4"), None)];
        let me = own("a1b2a1b2-self", "reboncode-a1b2");
        for requested in ["reboncode-a1b2", "@REBONCODE-A1B2", "a1b2a1b2-self"] {
            assert_eq!(
                resolve_peer_or_self(&peers, me, requested),
                Ok(PeerTarget::OwnSession),
                "{requested}"
            );
        }
    }

    /// An abbreviation the caller and another session both answer to names
    /// neither; one only the caller answers to is the caller; one only another
    /// answers to is that other.
    #[test]
    fn an_abbreviation_of_the_callers_handle_is_judged_like_any_other() {
        let peers = vec![peer("c3d4c3d4-peer", Some("reboncode-c3d4"), None)];
        let me = own("a1b2a1b2-self", "reboncode-a1b2");

        let error = resolve_peer_or_self(&peers, me, "@reboncode").unwrap_err();
        assert_eq!(
            error.candidates(),
            ["@reboncode-c3d4", "@reboncode-a1b2 (this session)"]
        );
        assert_eq!(
            resolve_peer_or_self(&peers, me, "reboncode-a1"),
            Ok(PeerTarget::OwnSession)
        );
        assert_eq!(
            resolve_peer_or_self(&peers, me, "reboncode-c3"),
            Ok(PeerTarget::Peer(&peers[0]))
        );
        assert_eq!(
            resolve_peer_or_self(&peers, me, "rebon"),
            Err(PeerResolveError::NotFound {
                requested: "rebon".into()
            }),
            "too short to abbreviate anything"
        );
    }

    /// The peer list leaves the caller out; if it did not, the caller is still
    /// one session, not two that share an id.
    #[test]
    fn a_peer_entry_for_the_caller_is_the_caller() {
        let peers = vec![peer("a1b2a1b2-self", Some("reboncode-a1b2"), None)];
        assert_eq!(
            resolve_peer_or_self(
                &peers,
                own("a1b2a1b2-self", "reboncode-a1b2"),
                "reboncode-a1b2"
            ),
            Ok(PeerTarget::OwnSession)
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

        let generation = ledger.begin_turn();
        ledger.note_inbound(generation, "sess-a", 2);
        assert_eq!(ledger.reply_hops("sess-a"), 3);
        assert_eq!(
            ledger.reply_hops("sess-b"),
            0,
            "a fresh conversation is not a hand-off, however deep the turn"
        );

        ledger.end_turn(generation);
        assert_eq!(
            ledger.reply_hops("sess-a"),
            0,
            "the next turn answers nobody"
        );
    }

    /// A note recorded for a turn that has ended belongs to nobody: the
    /// executor finishes its turn before the event loop has seen it end, and a
    /// message read in that window must not make the next turn — possibly one
    /// the person typed — report a depth it never answered.
    #[test]
    fn a_note_from_a_finished_turn_does_not_reach_the_next_one() {
        let ledger = AgentReplyLedger::new();
        let finished = ledger.begin_turn();

        ledger.note_inbound(finished, "sess-a", 3);
        assert_eq!(ledger.reply_hops("sess-a"), 4);

        let next = ledger.begin_turn();
        assert_eq!(
            ledger.reply_hops("sess-a"),
            0,
            "a new turn starts by answering nobody"
        );

        // The executor of the finished turn finishing late is what the
        // generation is for: its note cannot land in the new turn.
        ledger.note_inbound(finished, "sess-a", 3);
        assert_eq!(ledger.reply_hops("sess-a"), 0);

        ledger.note_inbound(next, "sess-a", 1);
        assert_eq!(ledger.reply_hops("sess-a"), 2);
    }

    /// The idle path knows, before its turn exists, which message the turn is
    /// carrying: it stages that depth and the turn adopts it.
    #[test]
    fn a_staged_depth_belongs_to_the_turn_that_starts_next() {
        let ledger = AgentReplyLedger::new();
        ledger.stage_inbound("sess-a", 2);
        let staged_turn = ledger.begin_turn();
        assert_eq!(ledger.reply_hops("sess-a"), 3);

        // Staged once, adopted once: a later turn answers nobody.
        ledger.end_turn(staged_turn);
        ledger.begin_turn();
        assert_eq!(ledger.reply_hops("sess-a"), 0);
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
        let generation = ledger.begin_turn();
        ledger.end_turn(generation);
        assert_eq!(
            ledger.admit_wake("sess-a", 0, 1_000),
            WakeAdmission::RateLimited {
                window_ms: WAKE_WINDOW_MS
            }
        );
    }

    /// A cancelled turn's executor can still be finishing after the next turn
    /// began. Its end closes only itself: the turn that is running keeps
    /// answering the peer it was started for, so its reply does not restart the
    /// chain at zero.
    #[test]
    fn ending_an_older_turn_leaves_the_running_one_open() {
        let ledger = AgentReplyLedger::new();
        let cancelled = ledger.begin_turn();
        ledger.stage_inbound("sess-a", 2);
        let running = ledger.begin_turn();
        assert_eq!(ledger.reply_hops("sess-a"), 3);

        ledger.end_turn(cancelled);
        assert_eq!(
            ledger.reply_hops("sess-a"),
            3,
            "the late end of a cancelled turn must not close the one after it"
        );

        ledger.end_turn(running);
        assert_eq!(ledger.reply_hops("sess-a"), 0);
    }

    /// Judging spends a wake only when it admits, and says which way it went.
    #[test]
    fn judging_a_wake_admits_within_the_bounds_and_refuses_past_them() {
        let ledger = AgentReplyLedger::new();
        assert_eq!(
            ledger.judge_wake("sess-a", MAX_AGENT_MESSAGE_HOPS + 1, 1_000),
            WakeVerdict::Refused
        );
        for _ in 0..AGENT_MESSAGE_WAKES_PER_HOUR {
            assert_eq!(
                ledger.judge_wake("sess-a", 0, 1_000),
                WakeVerdict::Admitted,
                "a refusal past the hop bound spent nothing"
            );
        }
        assert_eq!(ledger.judge_wake("sess-a", 0, 1_000), WakeVerdict::Refused);
    }

    /// The bounds are the same rule wherever the record of turn starts is
    /// kept — here a plain list, as a hosted session's job record keeps it —
    /// and a record whose wakes have all left the hour no longer counts.
    #[test]
    fn a_kept_record_of_wakes_is_bounded_like_the_ledger() {
        let mut wakes = Vec::new();
        for _ in 0..AGENT_MESSAGE_WAKES_PER_HOUR {
            assert_eq!(
                judge_wake_in(&mut wakes, "sess-a", 0, 1_000),
                WakeVerdict::Admitted
            );
        }
        assert_eq!(
            judge_wake_in(&mut wakes, "sess-a", 0, 1_000),
            WakeVerdict::Refused
        );
        assert_eq!(
            judge_wake_in(&mut Vec::new(), "sess-a", MAX_AGENT_MESSAGE_HOPS + 1, 1_000),
            WakeVerdict::Refused
        );
        assert!(wakes_still_count(&wakes, 1_000 + WAKE_WINDOW_MS - 1));
        assert!(!wakes_still_count(&wakes, 1_000 + WAKE_WINDOW_MS));
    }

    /// A ledger is per session: one session's depth is not another's.
    #[test]
    fn the_ledger_is_reached_by_session() {
        let first = session_reply_ledger("sess-ledger-a");
        let generation = first.begin_turn();
        first.note_inbound(generation, "sess-peer", 1);
        let second = session_reply_ledger("sess-ledger-b");
        assert_eq!(second.reply_hops("sess-peer"), 0);
        assert_eq!(
            session_reply_ledger("sess-ledger-a").reply_hops("sess-peer"),
            2
        );
    }
}
