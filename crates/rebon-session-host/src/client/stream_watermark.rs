//! One delta lands once, whichever of the two sources carried it.
//!
//! A client following a session has two channels for the same content, and
//! they are numbered differently. The owner's [`crate::SessionEvent`] stream
//! counts by cursor, restarting from one every time the owner starts. The
//! job's event log counts by byte offset. The owner makes the two comparable:
//! it stamps the log lines it writes with the epoch and cursor it gave them,
//! so a line read from the file can be recognised as one the stream already
//! delivered.
//!
//! This is the bookkeeping that recognition needs, with nothing in it about
//! what is being projected. The terminal mirror wrote it first; the desktop
//! app needs the same answers, and two implementations of "has this cursor
//! landed yet" is exactly the kind of copy that drifts silently — one surface
//! printing a token twice while the other drops it.
//!
//! # Who owns what
//!
//! The watermark is here. The *state being watermarked* — the queue of deltas
//! waiting behind a file read, the projection they land in — stays with the
//! consumer: the mirror's projection state is a terminal's business and not a
//! client's. So this answers questions and never holds a caller's payload.
//!
//! # The four questions
//!
//! * [`StreamWatermark::delivers_deltas`] — is the stream the source at all?
//! * [`StreamWatermark::accepts_update`] — is this streamed cursor new?
//! * [`StreamWatermark::file_line_is_new`] — is this stamped file line new?
//! * [`StreamWatermark::catching_up`] — is a file read still owed for a gap?

use crate::protocol::SessionEvent;
use crate::state::StreamStamp;

/// How many refreshes a client will read the file for before giving up on
/// closing an announced gap. The owner said the lines are gone from its ring;
/// if the file has not caught up in three passes it is not going to.
const CATCH_UP_READS: u8 = 3;

/// Where a client has got to in one owner's stream, and what it still owes
/// the file.
///
/// Starts knowing nothing: no epoch, cursor zero, no stamp seen. In that state
/// [`Self::delivers_deltas`] is false and every file line is new, which is the
/// behaviour a client needs before the owner has said hello — and the
/// behaviour it keeps forever against an owner too old to stamp its log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamWatermark {
    /// The numbering this owner's stream uses, from its `hello`.
    ///
    /// `None` until the owner has said hello. `Some(0)` is an owner from
    /// before the event log was stamped, whose deltas keep coming from the
    /// file because the stream's copies could not be told apart from it.
    epoch: Option<u64>,
    /// The highest cursor of that numbering already applied — off the stream,
    /// or read from a stamped line of the file. At or below it is done.
    cursor: u64,
    /// The highest stamp read from the file, in whatever numbering. A file
    /// read that lands before the owner's hello cannot know which cursors it
    /// is applying; this is how the hello catches up with it.
    file_stamp: Option<StreamStamp>,
    /// A gap the owner announced: the file is read up to this cursor before
    /// the stream's deltas are applied again.
    catch_up_to: Option<u64>,
    catch_up_reads: u8,
}

impl StreamWatermark {
    /// A client that has heard nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this owner's deltas come off the stream.
    ///
    /// False before the hello, and false forever against an owner that does
    /// not stamp its event log — in both cases the file stays the source.
    pub fn delivers_deltas(&self) -> bool {
        self.epoch.is_some_and(|epoch| epoch != 0)
    }

    /// How far this client has got, for a `Subscribe { since }` on reconnect.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The numbering in play, or `None` before the owner has said hello.
    pub fn epoch(&self) -> Option<u64> {
        self.epoch
    }

    /// The owner said hello: adopt its numbering.
    ///
    /// A hello in a numbering already known is a reconnect, and the cursor
    /// only moves forward. A new numbering starts from the hello's cursor — or
    /// from the highest stamp the file already showed in it, because a file
    /// scan getting in before the hello is the normal case, not the odd one.
    ///
    /// Returns whether the numbering changed, which is a caller's signal to
    /// drop the deltas it had queued under the old one.
    pub fn note_hello(&mut self, epoch: u64, cursor: u64) -> bool {
        if self.epoch == Some(epoch) {
            self.cursor = self.cursor.max(cursor);
            return false;
        }
        self.epoch = Some(epoch);
        self.cursor = cursor;
        if let Some(stamp) = self.file_stamp.filter(|stamp| stamp.epoch == epoch) {
            self.cursor = self.cursor.max(stamp.cursor);
        }
        self.catch_up_to = None;
        self.catch_up_reads = 0;
        true
    }

    /// Whether a delta that arrived on the stream at `cursor` is still to be
    /// applied.
    ///
    /// This does **not** move the watermark: a streamed delta waits behind any
    /// file read the caller still owes, so it is only done when the caller
    /// applies it. Say so with [`Self::note_update_applied`].
    pub fn accepts_update(&self, cursor: u64) -> bool {
        self.delivers_deltas() && cursor > self.cursor
    }

    /// A delta from the stream reached the screen.
    pub fn note_update_applied(&mut self, cursor: u64) {
        self.cursor = self.cursor.max(cursor);
    }

    /// The owner said everything before `to` is gone from its ring. What is
    /// missing between here and there is in the file, which is the authority.
    pub fn note_gap(&mut self, to: u64) {
        if self.delivers_deltas() && to > self.cursor.saturating_add(1) {
            self.catch_up_to = Some(to);
            self.catch_up_reads = 0;
        }
    }

    /// Whether a line of the event log stamped `stamp` still has to be
    /// applied, moving the watermark when it is the stream's own numbering.
    ///
    /// A stamp in the stream's numbering at or below the watermark was already
    /// applied off the stream. Anything else — a later cursor, another
    /// numbering, a numbering the hello has not named yet — is applied.
    ///
    /// Only ask about a line that *has* a stamp. A line without one came from
    /// an owner that does not stamp, and the caller applies it without asking,
    /// because the stream carries no copy of it to collide with.
    pub fn file_line_is_new(&mut self, stamp: StreamStamp) -> bool {
        if self
            .file_stamp
            .is_none_or(|seen| seen.epoch != stamp.epoch || seen.cursor < stamp.cursor)
        {
            self.file_stamp = Some(stamp);
        }
        if self.epoch != Some(stamp.epoch) {
            return true;
        }
        if stamp.cursor <= self.cursor {
            return false;
        }
        self.cursor = stamp.cursor;
        true
    }

    /// Whether a file read is still owed for an announced gap. While this is
    /// true the caller reads the file first and holds its streamed deltas.
    pub fn catching_up(&self) -> bool {
        self.catch_up_to.is_some()
    }

    /// One file read toward the gap has been made. The gap closes when the
    /// file reached it, or after enough reads that it is not going to.
    pub fn note_catch_up_read(&mut self) {
        let Some(to) = self.catch_up_to else {
            return;
        };
        self.catch_up_reads = self.catch_up_reads.saturating_add(1);
        if self.cursor.saturating_add(1) >= to || self.catch_up_reads >= CATCH_UP_READS {
            self.catch_up_to = None;
            self.catch_up_reads = 0;
        }
    }
}

/// The owner's numbering for one line it wrote into the event log.
///
/// Beside `turnGeneration` rather than inside the update, so it is metadata
/// about the line and never something a client mistakes for content. `None`
/// from an owner that does not stamp, whose lines a client applies without
/// asking because the stream carries no copy of them to collide with.
pub fn event_log_line_stamp(data: &serde_json::Value) -> Option<StreamStamp> {
    let epoch = data
        .get("streamEpoch")
        .and_then(serde_json::Value::as_u64)?;
    let cursor = data
        .get("streamCursor")
        .and_then(serde_json::Value::as_u64)?;
    (epoch != 0).then_some(StreamStamp { epoch, cursor })
}

/// What a drain of the owner's channel did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    /// At least one delta was handed to `apply`.
    pub applied: bool,
    /// The channel closed. Every way that happens means the same thing to a
    /// client — the owner hung up, it never spoke this protocol, the
    /// connection broke — because the answer to all three is to read the file.
    pub disconnected: bool,
    /// The owner said something about the session's shared state: a tool is
    /// waiting for an answer, or an I4 field moved.
    ///
    /// Deliberately a signal and not the value. A client projects that state
    /// from the job record, and one producer of it is the point; what this
    /// buys is that the projection is refreshed when the owner says there is
    /// something to see, instead of on the next scheduled poll.
    pub state_signalled: bool,
}

/// Take everything the owner has said since the last pass, without blocking.
///
/// `apply` is called with `(cursor, update)` for each delta the watermark has
/// not seen, in the order the owner sent them, and the watermark is advanced
/// after each one. Hellos and gaps are folded into the watermark; turn,
/// status, permission and gap events also signal that shared state needs a
/// fresh projection.
pub fn drain_updates(
    events: &std::sync::mpsc::Receiver<SessionEvent>,
    mark: &mut StreamWatermark,
    mut apply: impl FnMut(u64, serde_json::Value),
) -> DrainOutcome {
    drain_updates_with(events, mark, |cursor, update| {
        apply(cursor, update);
        DeltaLanded::OnScreen
    })
}

/// What a caller did with a delta the drain handed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaLanded {
    /// It reached the screen. The watermark moves past it.
    OnScreen,
    /// It was queued behind work the caller still owes — a file read for an
    /// announced gap, most often — and has not been shown yet.
    ///
    /// The watermark deliberately does **not** move. A client that queues and
    /// reads the file at the same time has two paths to the same delta, and
    /// the file's stamped line is how the queued one is recognised as already
    /// covered; moving the watermark early would make that line look old and
    /// leave the delta only in a queue nobody has drained. Say
    /// [`StreamWatermark::note_update_applied`] when it does reach the screen.
    Queued,
}

/// [`drain_updates`], but the caller says whether each delta actually landed.
///
/// The two entries share this loop rather than each having their own: hello
/// and gap folding is the part that must not drift, and a second copy of it is
/// exactly how a cursor and a gap start disagreeing. Both clients can defer
/// deltas to a file read: the terminal while catching up, and the desktop
/// when an older owner's live updates omit the turn identity.
pub fn drain_updates_with(
    events: &std::sync::mpsc::Receiver<SessionEvent>,
    mark: &mut StreamWatermark,
    mut apply: impl FnMut(u64, serde_json::Value) -> DeltaLanded,
) -> DrainOutcome {
    let mut outcome = DrainOutcome::default();
    loop {
        match events.try_recv() {
            Ok(SessionEvent::Hello { epoch, cursor, .. }) => {
                mark.note_hello(epoch, cursor);
            }
            Ok(SessionEvent::SessionUpdate { cursor, update }) => {
                if mark.accepts_update(cursor) {
                    if apply(cursor, update) == DeltaLanded::OnScreen {
                        mark.note_update_applied(cursor);
                    }
                    outcome.applied = true;
                }
            }
            Ok(SessionEvent::Gap { to, .. }) => {
                mark.note_gap(to);
                outcome.state_signalled = true;
            }
            Ok(SessionEvent::Permission { .. })
            | Ok(SessionEvent::Status { .. })
            | Ok(SessionEvent::Turn { .. }) => {
                outcome.state_signalled = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                outcome.disconnected = true;
                break;
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(epoch: u64, cursor: u64) -> StreamStamp {
        StreamStamp { epoch, cursor }
    }

    /// A delta the caller only queued must leave the watermark alone.
    ///
    /// This is the double-handling this entry exists to prevent, and it is not
    /// hypothetical: a client that holds deltas back while it owes a file read
    /// for an announced gap has two paths to the same delta. If queueing moved
    /// the watermark, the file's stamped line covering that delta would read as
    /// already applied and be dropped, while the delta itself was still sitting
    /// in a queue — each path assuming the other had shown it.
    #[test]
    fn a_queued_delta_leaves_the_file_line_that_covers_it_still_new() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(SessionEvent::SessionUpdate {
            cursor: 4,
            update: serde_json::json!({"held": true}),
        })
        .unwrap();
        drop(tx);

        let mut mark = StreamWatermark::new();
        mark.note_hello(7, 0);
        let mut queued = Vec::new();
        let outcome = drain_updates_with(&rx, &mut mark, |cursor, update| {
            queued.push((cursor, update));
            DeltaLanded::Queued
        });
        assert!(outcome.applied, "the delta was handed over");
        assert_eq!(queued.len(), 1);

        assert!(
            mark.file_line_is_new(stamp(7, 4)),
            "the file line covering a merely-queued delta is still new"
        );

        // And once the caller does show it, the watermark moves for real.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(SessionEvent::SessionUpdate {
            cursor: 9,
            update: serde_json::json!({"shown": true}),
        })
        .unwrap();
        drop(tx);
        drain_updates_with(&rx, &mut mark, |_, _| DeltaLanded::OnScreen);
        assert!(
            !mark.file_line_is_new(stamp(7, 9)),
            "a delta that reached the screen is not applied twice"
        );
    }

    #[test]
    fn a_permission_is_signalled_rather_than_carried() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(SessionEvent::Permission {
            cursor: 1,
            query: Box::new(
                serde_json::from_value(serde_json::json!({
                    "queryId": 1,
                    "options": [],
                }))
                .expect("a minimal query"),
            ),
        })
        .unwrap();
        drop(tx);

        let mut mark = StreamWatermark::new();
        let outcome = drain_updates(&rx, &mut mark, |_, _| panic!("not a delta"));
        assert!(outcome.state_signalled, "the client is told to look again");
        assert!(!outcome.applied, "and nothing was folded as a delta");
        assert!(outcome.disconnected);
    }

    #[test]
    fn before_hello_the_file_is_the_source() {
        let mut mark = StreamWatermark::new();
        assert!(!mark.delivers_deltas());
        assert!(
            !mark.accepts_update(7),
            "a delta cannot land before a hello"
        );
        assert!(mark.file_line_is_new(stamp(9, 4)), "the file always can");
    }

    #[test]
    fn an_owner_that_does_not_stamp_its_log_keeps_the_file_as_the_source() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(0, 12);
        assert!(
            !mark.delivers_deltas(),
            "epoch zero is an owner that cannot be deduplicated"
        );
        assert!(
            !mark.accepts_update(13),
            "so its streamed deltas are dropped and the file stays the source"
        );
        // Its log lines carry no stamp at all, so `file_line_is_new` is never
        // asked about them — the caller applies every one.
    }

    #[test]
    fn turn_completion_and_gaps_signal_a_fresh_projection() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 1);
        for cursor in 2..6 {
            tx.send(SessionEvent::Turn {
                cursor,
                state: crate::TurnStreamState::Idle,
                stop_reason: None,
                stop_refused: None,
            })
            .unwrap();
            let outcome = drain_updates(&rx, &mut mark, |_, _| panic!("not a content delta"));
            assert!(outcome.state_signalled);
            assert!(!outcome.applied);
            assert!(!drain_updates(&rx, &mut mark, |_, _| {}).state_signalled);
        }
        tx.send(SessionEvent::Gap { from: 2, to: 9 }).unwrap();
        assert!(drain_updates(&rx, &mut mark, |_, _| {}).state_signalled);
        assert!(mark.catching_up());
    }

    #[test]
    fn a_second_hello_in_the_same_numbering_only_moves_forward() {
        let mut mark = StreamWatermark::new();
        assert!(mark.note_hello(9, 5), "the first hello names a numbering");
        mark.note_update_applied(11);
        assert!(!mark.note_hello(9, 5), "a reconnect is not a new numbering");
        assert_eq!(mark.cursor(), 11, "the watermark must not go backwards");
    }

    #[test]
    fn a_new_numbering_takes_the_hellos_cursor() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 40);
        assert!(
            mark.note_hello(10, 2),
            "a restarted owner is a new numbering"
        );
        assert_eq!(mark.cursor(), 2);
    }

    #[test]
    fn a_file_read_that_beat_the_hello_still_counts() {
        let mut mark = StreamWatermark::new();
        // The scan runs in a thread; the first frame need not have the hello.
        assert!(mark.file_line_is_new(stamp(9, 6)));
        mark.note_hello(9, 3);
        assert_eq!(mark.cursor(), 6, "the hello adopts what the file showed");
        assert!(!mark.accepts_update(6), "and does not replay it");
        assert!(mark.accepts_update(7));
    }

    #[test]
    fn a_stamp_from_another_numbering_is_applied_and_does_not_move_the_cursor() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 5);
        assert!(mark.file_line_is_new(stamp(8, 900)), "a predecessor's line");
        assert_eq!(mark.cursor(), 5);
    }

    #[test]
    fn what_the_stream_delivered_is_not_read_again_from_the_file() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 0);
        assert!(mark.accepts_update(1));
        mark.note_update_applied(1);
        assert!(!mark.file_line_is_new(stamp(9, 1)), "already on screen");
        assert!(mark.file_line_is_new(stamp(9, 2)), "this one is not");
        assert_eq!(mark.cursor(), 2);
    }

    #[test]
    fn a_gap_owes_a_file_read_and_closes_when_the_file_catches_up() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 1);
        mark.note_gap(6);
        assert!(mark.catching_up());

        mark.note_catch_up_read();
        assert!(mark.catching_up(), "the file has not reached the gap yet");
        assert!(mark.file_line_is_new(stamp(9, 5)));
        mark.note_catch_up_read();
        assert!(!mark.catching_up(), "cursor + 1 reached the gap's end");
    }

    #[test]
    fn a_gap_the_file_never_closes_is_given_up_on() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 1);
        mark.note_gap(9_000);
        for _ in 0..CATCH_UP_READS {
            assert!(mark.catching_up());
            mark.note_catch_up_read();
        }
        assert!(
            !mark.catching_up(),
            "three reads and the file is not coming"
        );
    }

    #[test]
    fn a_gap_that_names_nothing_missing_is_not_a_gap() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 4);
        mark.note_gap(5);
        assert!(
            !mark.catching_up(),
            "cursor 4 already covers everything before 5"
        );
    }

    #[test]
    fn a_hello_in_a_new_numbering_drops_an_open_gap() {
        let mut mark = StreamWatermark::new();
        mark.note_hello(9, 1);
        mark.note_gap(50);
        assert!(mark.catching_up());
        assert!(mark.note_hello(10, 0), "the owner restarted");
        assert!(!mark.catching_up(), "that gap was the old owner's");
    }
}
