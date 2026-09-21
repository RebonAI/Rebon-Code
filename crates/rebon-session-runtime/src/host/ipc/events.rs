//! The owner's live event stream: a bounded ring plus its subscribers.
//!
//! Every client used to learn what a session was doing by re-reading a file on
//! a timer — 100ms for a terminal mirror, the same for a browser — so a token
//! the owner already had took up to a tenth of a second to appear anywhere
//! else, and there was no way for a client to say "I fell behind". This is the
//! push side of the replacement: the owner numbers each event and hands it to
//! whoever is attached.
//!
//! Two properties matter more than throughput here:
//!
//! * **A slow client must never stall the turn.** Publishing is non-blocking.
//!   A subscriber that cannot keep up is dropped and told so with a
//!   [`SessionEvent::Gap`] rather than being waited on.
//! * **A dropped connection must be resumable.** Recent events are kept, so a
//!   client that reconnects with the cursor it reached gets the difference,
//!   and one that fell outside the window gets a gap and re-reads the
//!   transcript, which is the authority anyway.

use std::collections::VecDeque;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use rebon_session_host::{
    BackgroundPermissionQuerySnapshot, SessionEvent, SessionStatusSnapshot, TurnStreamState,
};

/// How many events the owner keeps for a reconnecting client.
///
/// A turn's streaming deltas are the bulk of this; a couple of thousand covers
/// a normal turn end to end, which is the window that matters — a client away
/// longer than one turn is better off re-reading the transcript than replaying
/// deltas at it.
const MAX_RING_ENTRIES: usize = 2_000;

/// And a byte ceiling, because one pasted file can be worth a thousand deltas.
const MAX_RING_BYTES: usize = 8 * 1024 * 1024;

/// How many lines a subscriber may fall behind before it is dropped.
///
/// Generous enough that an ordinary redraw pause is invisible, small enough
/// that a wedged client cannot hold megabytes of the owner's memory hostage.
const SUBSCRIBER_BACKLOG: usize = 512;

/// What a subscriber gets on attach.
pub(crate) struct Subscription {
    pub(crate) id: u64,
    /// Events the owner still had from `since`, oldest first. Already
    /// serialized, so the connection thread only has to write them.
    pub(crate) replay: Vec<String>,
    /// The stream from here on. Closed when the owner drops this subscriber.
    pub(crate) events: Receiver<String>,
    /// The cursor immediately before the first event this subscription will
    /// deliver, replay included.
    ///
    /// So `hello` and everything after it read as one increasing sequence with
    /// no repeats and no holes, and a client that saw only `hello` resumes
    /// from it and gets exactly what it was about to be sent. Reporting the
    /// *next* cursor instead made `hello` and the first delivered event claim
    /// the same number — caught against a live worker.
    pub(crate) cursor: u64,
}

struct Subscriber {
    id: u64,
    sender: SyncSender<String>,
}

#[derive(Default)]
struct Ring {
    next_subscriber: u64,
    /// Cursor to assign to the next event. Starts at 1 so that a client with
    /// no history can send `since: 0` and mean "everything you still have".
    next_cursor: u64,
    entries: VecDeque<(u64, String)>,
    bytes: usize,
    subscribers: Vec<Subscriber>,
}

/// The owner's event stream, shared by the turn loop and every connection.
#[derive(Clone)]
pub struct SessionEventStream {
    ring: Arc<Mutex<Ring>>,
    /// Which incarnation of the owner is numbering. Cursors start over with
    /// every process, so a client holding one from the previous worker must
    /// be able to tell — and the event log, which outlives any one worker,
    /// stamps each update with both. Minted once, never zero: zero is what a
    /// client reads from an owner that predates the field.
    epoch: u64,
}

impl Default for SessionEventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionEventStream {
    pub(crate) fn new() -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(1);
        // Time first, pid in the low bits: two workers for one job never
        // number at the same millisecond in the same process.
        let epoch = ((now_ms << 20) | (u64::from(std::process::id()) & 0xF_FFFF)).max(1);
        Self {
            ring: Arc::new(Mutex::new(Ring {
                next_cursor: 1,
                ..Ring::default()
            })),
            epoch,
        }
    }

    /// The numbering this owner's cursors belong to. See [`Self::new`].
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The cursor the next published event will carry.
    pub(crate) fn cursor(&self) -> u64 {
        self.ring.lock().expect("poisoned").next_cursor
    }

    /// Push an update to every subscriber; the cursor it went out at, so the
    /// caller can stamp the same update where it logs it.
    /// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
    #[doc(hidden)]
    pub fn publish_update(
        &self,
        update: &rebon_types::SessionUpdateParams,
    ) -> Option<rebon_session_host::StreamStamp> {
        self.publish_update_value(serde_json::to_value(update).ok()?)
    }

    /// Stream the same turn identity as the stamped event log. Consumers must
    /// not assign delayed chunks to whichever turn their snapshot last saw.
    pub(crate) fn publish_update_for_turn(
        &self,
        update: &rebon_types::SessionUpdateParams,
        turn_generation: u64,
    ) -> Option<rebon_session_host::StreamStamp> {
        let mut update = serde_json::to_value(update).ok()?;
        update["turnGeneration"] = turn_generation.into();
        self.publish_update_value(update)
    }

    fn publish_update_value(
        &self,
        update: serde_json::Value,
    ) -> Option<rebon_session_host::StreamStamp> {
        let cursor = self.publish(|cursor| SessionEvent::SessionUpdate { cursor, update })?;
        Some(rebon_session_host::StreamStamp {
            epoch: self.epoch,
            cursor,
        })
    }

    /// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
    #[doc(hidden)]
    pub fn publish_turn(&self, state: TurnStreamState, stop_reason: Option<String>) {
        self.publish(|cursor| SessionEvent::Turn {
            cursor,
            state,
            stop_reason,
            stop_refused: None,
        });
    }

    /// Tell every subscriber that a cancel did not stop the turn, and why.
    ///
    /// On the stream rather than only in the answer to whoever asked: the Stop
    /// hook kept a turn running, and every client watching this session is
    /// looking at a turn that did not stop, not only the one who pressed stop.
    /// The state stays `Running`, because it is.
    pub(crate) fn publish_stop_refused(&self, reason: String) {
        self.publish(|cursor| SessionEvent::Turn {
            cursor,
            state: TurnStreamState::Running,
            stop_reason: None,
            stop_refused: Some(reason),
        });
    }

    pub(crate) fn publish_status(&self, snapshot: SessionStatusSnapshot) {
        self.publish(|cursor| SessionEvent::Status {
            cursor,
            snapshot: Box::new(snapshot),
        });
    }

    pub(crate) fn publish_permission(&self, query: BackgroundPermissionQuerySnapshot) {
        self.publish(|cursor| SessionEvent::Permission {
            cursor,
            query: Box::new(query),
        });
    }

    /// Attach a subscriber, replaying from `since` where possible.
    ///
    /// `since` is the last cursor the client saw, so replay starts *after* it.
    /// When the ring no longer reaches back that far the caller is handed a
    /// gap instead — losing events is recoverable (the transcript is on disk),
    /// silently skipping them is not.
    pub(crate) fn subscribe(&self, since: Option<u64>) -> Subscription {
        let (sender, events) = sync_channel(SUBSCRIBER_BACKLOG);
        let mut ring = self.ring.lock().expect("poisoned");
        let oldest = ring.entries.front().map(|(cursor, _)| *cursor);
        let next = ring.next_cursor;
        let mut replay = Vec::new();
        // The first numbered entry this subscription delivers, which is what
        // `cursor` is derived from.
        let mut first_delivered: Option<u64> = None;
        match since {
            // A cursor beyond anything this owner has published belongs to a
            // different incarnation of it — a worker that restarted numbers
            // from one again. Saying "continuous" there would let a client
            // believe it had seen everything up to a point that never existed.
            Some(since) if since >= next => {
                let gap = SessionEvent::Gap {
                    from: since,
                    to: next,
                };
                if let Ok(line) = serde_json::to_string(&gap) {
                    replay.push(line);
                }
                first_delivered = oldest;
                replay.extend(ring.entries.iter().map(|(_, line)| line.clone()));
            }
            Some(since) => {
                let reaches_back = oldest.is_none_or(|oldest| oldest <= since.saturating_add(1));
                if reaches_back {
                    first_delivered = ring
                        .entries
                        .iter()
                        .find(|(entry, _)| *entry > since)
                        .map(|(entry, _)| *entry);
                    replay.extend(
                        ring.entries
                            .iter()
                            .filter(|(entry, _)| *entry > since)
                            .map(|(_, line)| line.clone()),
                    );
                } else {
                    let gap = SessionEvent::Gap {
                        from: since,
                        to: oldest.unwrap_or(next),
                    };
                    if let Ok(line) = serde_json::to_string(&gap) {
                        replay.push(line);
                    }
                    first_delivered = oldest;
                    replay.extend(ring.entries.iter().map(|(_, line)| line.clone()));
                }
            }
            // No `since` means a fresh client: it reads history from the
            // transcript, so replaying deltas at it would duplicate what it
            // already has.
            None => {}
        }
        ring.next_subscriber += 1;
        let id = ring.next_subscriber;
        ring.subscribers.push(Subscriber { id, sender });
        Subscription {
            id,
            replay,
            events,
            // Without a replay the next event to arrive is `next`, so the one
            // before it is `next - 1`.
            cursor: first_delivered.unwrap_or(next).saturating_sub(1),
        }
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        self.ring
            .lock()
            .expect("poisoned")
            .subscribers
            .retain(|subscriber| subscriber.id != id);
    }

    /// How many subscribers are attached. Used by the tests and by the
    /// diagnostics that report why an owner is staying up.
    pub(crate) fn subscriber_count(&self) -> usize {
        self.ring.lock().expect("poisoned").subscribers.len()
    }

    /// Number and fan out one event. The cursor it took, or `None` when it
    /// could not be sent at all.
    fn publish(&self, make: impl FnOnce(u64) -> SessionEvent) -> Option<u64> {
        let mut ring = self.ring.lock().expect("poisoned");
        let cursor = ring.next_cursor;
        let event = make(cursor);
        let Ok(line) = serde_json::to_string(&event) else {
            // A payload that will not serialize would break every subscriber's
            // stream if written raw. Skipping it costs one event; the cursor is
            // not consumed, so nobody sees a hole.
            return None;
        };
        ring.next_cursor = cursor.saturating_add(1);
        ring.bytes += line.len();
        ring.entries.push_back((cursor, line.clone()));
        while ring.entries.len() > MAX_RING_ENTRIES || ring.bytes > MAX_RING_BYTES {
            match ring.entries.pop_front() {
                Some((_, dropped)) => ring.bytes = ring.bytes.saturating_sub(dropped.len()),
                None => break,
            }
        }
        // A subscriber that cannot take this line is behind by a full backlog.
        // Dropping it here is what turns "the owner is stalled behind a wedged
        // browser tab" into "that tab reconnects and asks for what it missed".
        ring.subscribers.retain(|subscriber| {
            !matches!(
                subscriber.sender.try_send(line.clone()),
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_))
            )
        });
        Some(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> SessionEventStream {
        SessionEventStream::new()
    }

    fn turn(stream: &SessionEventStream, state: TurnStreamState) {
        stream.publish_turn(state, None);
    }

    fn kinds(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn live_updates_carry_the_same_turn_identity_as_the_event_log() {
        let stream = stream();
        let subscription = stream.subscribe(None);
        let update = rebon_types::SessionUpdateParams {
            session_id: "sess".into(),
            update: rebon_types::SessionUpdate::AgentMessageChunk {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "answer".into(),
                    annotations: None,
                }),
            },
        };
        for generation in 7..11 {
            let stamp = stream.publish_update_for_turn(&update, generation).unwrap();
            let event: SessionEvent =
                serde_json::from_str(&subscription.events.try_recv().unwrap()).unwrap();
            let SessionEvent::SessionUpdate {
                cursor,
                update: value,
            } = event
            else {
                panic!("expected update")
            };
            assert_eq!(value["turnGeneration"].as_u64(), Some(generation));
            assert_eq!(cursor, stamp.cursor);
            assert_eq!(
                serde_json::from_value::<rebon_types::SessionUpdateParams>(value)
                    .unwrap()
                    .session_id,
                update.session_id
            );
        }
    }

    /// The stamp a published update comes back with is where the owner's
    /// log has to file it: the stream's own numbering, one cursor per update,
    /// under an epoch that is never the zero an older owner reads as.
    #[test]
    fn a_published_update_is_stamped_with_the_streams_numbering() {
        let stream = stream();
        let update = |text: &str| rebon_types::SessionUpdateParams {
            session_id: "sess".into(),
            update: rebon_types::SessionUpdate::AgentMessageChunk {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: text.into(),
                    annotations: None,
                }),
            },
        };
        turn(&stream, TurnStreamState::Running);

        let first = stream.publish_update(&update("a")).expect("published");
        let second = stream.publish_update(&update("b")).expect("published");

        assert_ne!(stream.epoch(), 0);
        assert_eq!(first.epoch, stream.epoch());
        assert_eq!((first.cursor, second.cursor), (2, 3));
        assert_eq!(stream.cursor(), 4);
    }

    #[test]
    fn a_subscriber_receives_what_is_published_after_it_attaches() {
        let stream = stream();
        let subscription = stream.subscribe(None);
        assert!(subscription.replay.is_empty());

        turn(&stream, TurnStreamState::Running);
        turn(&stream, TurnStreamState::Idle);

        let first = subscription.events.recv().unwrap();
        let second = subscription.events.recv().unwrap();
        assert_eq!(kinds(&[first.clone(), second]), ["turn", "turn"]);
        let parsed: SessionEvent = serde_json::from_str(&first).unwrap();
        assert_eq!(parsed.cursor(), Some(1));
    }

    /// The reconnect case: a client says where it got to and gets exactly the
    /// difference, without the events it already rendered.
    #[test]
    fn resuming_from_a_cursor_replays_only_what_came_after_it() {
        let stream = stream();
        turn(&stream, TurnStreamState::Running);
        turn(&stream, TurnStreamState::Idle);
        turn(&stream, TurnStreamState::Running);

        let subscription = stream.subscribe(Some(1));

        assert_eq!(subscription.replay.len(), 2);
        let cursors: Vec<_> = subscription
            .replay
            .iter()
            .map(|line| {
                serde_json::from_str::<SessionEvent>(line)
                    .unwrap()
                    .cursor()
                    .unwrap()
            })
            .collect();
        assert_eq!(cursors, [2, 3]);
    }

    /// Falling outside the retained window must be *said*, not papered over: a
    /// client that silently skipped events would render a transcript with a
    /// hole in it and never know.
    #[test]
    fn resuming_from_a_forgotten_cursor_reports_a_gap() {
        let stream = stream();
        for _ in 0..(MAX_RING_ENTRIES + 10) {
            turn(&stream, TurnStreamState::Running);
        }

        let subscription = stream.subscribe(Some(1));

        let first = kinds(&subscription.replay[..1]);
        assert_eq!(first, ["gap"]);
        let gap: SessionEvent = serde_json::from_str(&subscription.replay[0]).unwrap();
        match gap {
            SessionEvent::Gap { from, to } => {
                assert_eq!(from, 1);
                assert!(to > 1, "a gap must name where the stream resumes");
            }
            other => panic!("expected a gap, got {other:?}"),
        }
    }

    /// The cursor a subscription reports is the last one *accounted for*, not
    /// the next one to be published. Reporting the next one made `hello` and
    /// the first live event claim the same number, so a client resuming from
    /// what `hello` told it skipped that event — caught on a live worker, not
    /// by any of the tests that preceded this one.
    #[test]
    fn the_reported_cursor_never_collides_with_a_later_event() {
        let stream = stream();
        turn(&stream, TurnStreamState::Running);

        let subscription = stream.subscribe(Some(0));
        turn(&stream, TurnStreamState::Idle);

        let replayed: Vec<u64> = subscription
            .replay
            .iter()
            .map(|line| {
                serde_json::from_str::<SessionEvent>(line)
                    .unwrap()
                    .cursor()
                    .unwrap()
            })
            .collect();
        let live: SessionEvent =
            serde_json::from_str(&subscription.events.recv().unwrap()).unwrap();

        assert_eq!(replayed, [1], "the event published before the attach");
        assert_eq!(
            subscription.cursor, 0,
            "hello sits immediately before the first event it hands over"
        );
        assert_eq!(
            live.cursor(),
            Some(2),
            "and the live stream continues where the replay stopped"
        );
    }

    /// The other half of the same contract: resuming from the reported cursor
    /// yields exactly what followed it, with nothing repeated and nothing lost.
    #[test]
    fn resuming_from_the_reported_cursor_yields_exactly_the_remainder() {
        let stream = stream();
        turn(&stream, TurnStreamState::Running);
        let first = stream.subscribe(None);
        turn(&stream, TurnStreamState::Idle);
        turn(&stream, TurnStreamState::Running);
        drop(first);

        let resumed = stream.subscribe(Some(1));

        let cursors: Vec<u64> = resumed
            .replay
            .iter()
            .map(|line| {
                serde_json::from_str::<SessionEvent>(line)
                    .unwrap()
                    .cursor()
                    .unwrap()
            })
            .collect();
        assert_eq!(cursors, [2, 3]);
        assert_eq!(
            resumed.cursor, 1,
            "hello precedes the replay it is about to hand over"
        );
    }

    /// A worker that respawned starts numbering at one again. A client
    /// reconnecting with the cursor it reached on the previous owner has not
    /// seen those events — telling it the stream is continuous would leave it
    /// rendering a transcript with a hole and no way to know.
    #[test]
    fn resuming_past_the_end_reports_a_gap() {
        let stream = stream();
        turn(&stream, TurnStreamState::Running);

        let subscription = stream.subscribe(Some(500));

        assert_eq!(kinds(&subscription.replay[..1]), ["gap"]);
        match serde_json::from_str::<SessionEvent>(&subscription.replay[0]).unwrap() {
            SessionEvent::Gap { from, to } => {
                assert_eq!(from, 500);
                assert_eq!(to, 2, "the stream resumes where this owner actually is");
            }
            other => panic!("expected a gap, got {other:?}"),
        }
        assert_eq!(
            subscription.replay.len(),
            2,
            "and everything this owner does have follows the gap"
        );
    }

    #[test]
    fn the_ring_stays_bounded() {
        let stream = stream();
        for _ in 0..(MAX_RING_ENTRIES * 2) {
            turn(&stream, TurnStreamState::Running);
        }
        let ring = stream.ring.lock().unwrap();
        assert!(ring.entries.len() <= MAX_RING_ENTRIES);
    }

    /// The property that keeps a wedged client from wedging the session: the
    /// owner drops it rather than blocking, and the turn keeps running.
    #[test]
    fn a_subscriber_that_stops_reading_is_dropped_not_waited_on() {
        let stream = stream();
        // Held but never read from, which is what a wedged client looks like
        // from here. Dropping it would close the channel and prove nothing.
        let _subscription = stream.subscribe(None);
        assert_eq!(stream.subscriber_count(), 1);

        // Publish past its backlog without anyone draining it.
        for _ in 0..(SUBSCRIBER_BACKLOG + 5) {
            turn(&stream, TurnStreamState::Running);
        }

        assert_eq!(
            stream.subscriber_count(),
            0,
            "a subscriber that fell a whole backlog behind must be let go"
        );
        // And the owner kept publishing throughout.
        assert!(stream.cursor() > SUBSCRIBER_BACKLOG as u64);
    }

    #[test]
    fn a_disconnected_subscriber_is_forgotten() {
        let stream = stream();
        let subscription = stream.subscribe(None);
        drop(subscription);

        turn(&stream, TurnStreamState::Running);

        assert_eq!(stream.subscriber_count(), 0);
    }
}
