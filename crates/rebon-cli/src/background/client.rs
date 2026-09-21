use super::*;
use rebon_session_host::client::stream_watermark::StreamWatermark;

pub use rebon_session_host::BackgroundAttachMode;

pub type BackgroundIpcEndpoint = rebon_session_host::BackgroundIpcEndpoint;

/// The live end of an attachment: a worker that is up and answering.
///
/// Everything here is about *that process* — where it listens, the event
/// stream it speaks, the probe that checks it is still there. When the
/// worker goes, all of it goes together, and the attachment stays behind
/// as the session's data source with nothing on the other end.
pub struct WorkerLink {
    /// This terminal's connection to the worker: the endpoint it is fenced to,
    /// and the lease it holds for as long as the link lives.
    ///
    /// One value where the link kept two, and the same one `serve` holds. What
    /// stays below is the mirror's own projection state — cursors, pending
    /// deltas, the file stamp — which is a terminal's business and not a
    /// client's (design §8.2).
    pub connection: Arc<rebon_session_host::SessionHostConnection>,
    /// The owner's live event stream, when it speaks one.
    ///
    /// `None` against a worker from before `Subscribe` existed, in which case
    /// everything keeps arriving the way it always did: from the job record
    /// and the event log, on a timer.
    pub events_rx: Option<std::sync::mpsc::Receiver<rebon_session_host::SessionEvent>>,
    pub endpoint_probe_rx: Option<std::sync::mpsc::Receiver<bool>>,
    pub last_endpoint_check_at: Instant,
    /// Which of the owner's deltas this link has already shown: epoch, cursor,
    /// the highest stamp seen in the file, and any gap still owed a read.
    ///
    /// One implementation, shared with the desktop app. What stays here
    /// is only the part that holds a payload — the queue below — because the
    /// watermark deliberately holds none.
    pub mark: StreamWatermark,
    /// Deltas off the stream not yet applied. They wait behind any read of
    /// the file the refresh has to make first — the initial scan, a gap —
    /// so that what the file says lands in the order the owner said it.
    pub pending_stream_updates: Vec<(u64, rebon_types::SessionUpdateParams)>,
    /// When the stream was last opened, so a link that lost it reconnects
    /// on a cadence rather than every frame.
    pub stream_opened_at: Instant,
}

impl RemoteBackgroundAttachment {
    /// Say this terminal is leaving because the user is done.
    ///
    /// The three ways a mirror lets go — the user exiting, the user handing
    /// the session on with `/bg`, and the terminal simply dying — all end in
    /// the same `Drop`, so the owner could not tell them apart and treated
    /// every one of them as "they might come back". That is why a `/exit`
    /// left a worker parked for ten minutes holding a plugin and MCP stack
    /// for a window that was closed on purpose.
    ///
    /// Only the first of the three calls this. `/bg` and Ctrl+Z must not:
    /// outliving this terminal is what they are for.
    pub fn mark_exit_deliberate(&self) {
        if let Some(worker) = self.worker.as_ref() {
            worker.connection.mark_lease_deliberate();
        }
    }

    /// The model the owner says it is running, once it has said.
    ///
    /// `None` before the first snapshot, and from an owner too old to publish
    /// one. Both mean the same thing to a caller — this terminal does not know
    /// — and "I do not know" is the honest answer while the alternative is
    /// this process's own start-up guess about somebody else's session.
    pub fn owner_model(&self) -> Option<&str> {
        self.owner.as_ref().and_then(|owner| owner.model.as_deref())
    }

    /// The reasoning effort the owner says it is running under. See
    /// [`Self::owner_model`] for what `None` means.
    /// Only the tests read this; the runtime carries the effort elsewhere.
    #[cfg(test)]
    pub fn owner_effort(&self) -> Option<&str> {
        self.owner
            .as_ref()
            .and_then(|owner| owner.effort.as_deref())
    }
}

/// The lease id this process holds sessions open under.
///
/// One per process, not per attachment: a terminal mirrors one session at
/// a time, and a lease keyed on the process is renewed rather than
/// duplicated when the terminal follows a replacement worker.
pub fn tui_lease_client_id() -> String {
    format!("tui-{}", std::process::id())
}

impl WorkerLink {
    /// Connect to the worker at `endpoint`: open its event stream, and hold
    /// a lease on it.
    pub fn connect(session_id: &str, job_id: &str, endpoint: BackgroundIpcEndpoint) -> Self {
        let connection = Arc::new(rebon_session_host::SessionHostConnection::new(
            rebon_session_host::OwnerHandle::for_worker(session_id, Some(job_id), &endpoint),
        ));
        // The lease first, then the stream: a worker with nobody watching
        // lingers and leaves, and a terminal that subscribed before claiming
        // one could watch its own host exit. The guard lives on the connection,
        // so leaving the session, stopping the worker and exiting the terminal
        // all release it by dropping the link.
        connection.hold_lease(
            &tui_lease_client_id(),
            rebon_session_host::ClientLeaseKind::Tui,
        );
        // Attach to the owner's stream now, so the first refresh already has
        // its state rather than reading a snapshot of the job record and
        // waiting a tick for the rest.
        let events_rx = connection.subscribe_in_background();
        Self {
            connection,
            events_rx,
            endpoint_probe_rx: None,
            last_endpoint_check_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            mark: StreamWatermark::new(),
            pending_stream_updates: Vec::new(),
            stream_opened_at: Instant::now(),
        }
    }

    /// Poll the in-flight endpoint probe or start the next one when due.
    ///
    /// The sender is moved into the probe thread, never cloned: disconnecting
    /// its receiver must remain a reliable signal that the probe ended.
    pub fn endpoint_is_healthy(&mut self, job_id: &str, now: Instant) -> bool {
        const ENDPOINT_PROBE_INTERVAL: Duration = Duration::from_secs(1);

        if let Some(receiver) = self.endpoint_probe_rx.as_ref() {
            match receiver.try_recv() {
                Ok(true) => self.endpoint_probe_rx = None,
                Ok(false) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.endpoint_probe_rx = None;
                    return false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            }
        }
        if now.duration_since(self.last_endpoint_check_at) < ENDPOINT_PROBE_INTERVAL {
            return true;
        }

        self.last_endpoint_check_at = now;
        let job_id = job_id.to_string();
        let endpoint = self.connection.endpoint();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(background_job_endpoint_is_live(&job_id, &endpoint));
        });
        self.endpoint_probe_rx = Some(rx);
        true
    }

    /// Whether this owner's deltas come off the stream.
    pub fn stream_delivers_deltas(&self) -> bool {
        self.mark.delivers_deltas()
    }

    /// The owner said hello: adopt its numbering.
    ///
    /// A new numbering makes whatever this link had queued meaningless — those
    /// cursors belonged to the old one — so the queue is dropped here. That is
    /// the half the watermark cannot do: it holds no payload, deliberately.
    pub fn note_stream_hello(&mut self, epoch: u64, cursor: u64) {
        if self.mark.note_hello(epoch, cursor) {
            self.pending_stream_updates.clear();
        }
    }

    /// A delta arrived on the stream. Kept until the refresh has done any
    /// file reading it owes; one at or below the cursor is already on screen.
    ///
    /// Queueing is not applying, so the watermark is told nothing yet — see
    /// [`rebon_session_host::client::stream_watermark::DeltaLanded`]. It moves
    /// in `apply_pending_stream_updates`, when the delta reaches the screen.
    pub fn note_stream_update(&mut self, cursor: u64, update: serde_json::Value) {
        if !self.mark.accepts_update(cursor) {
            return;
        }
        if let Ok(params) = serde_json::from_value::<rebon_types::SessionUpdateParams>(update) {
            self.pending_stream_updates.push((cursor, params));
        }
    }

    /// The owner said everything before `to` is gone from its ring. What is
    /// missing between here and there is in the file.
    pub fn note_stream_gap(&mut self, to: u64) {
        self.mark.note_gap(to);
    }

    /// Whether a line of the file stamped `stamp` still has to be applied.
    pub fn file_line_is_new(&mut self, stamp: rebon_session_host::StreamStamp) -> bool {
        self.mark.file_line_is_new(stamp)
    }

    /// Whether a file read is still owed for an announced gap.
    pub fn catching_up(&self) -> bool {
        self.mark.catching_up()
    }

    /// One file read toward the gap has been made.
    pub fn note_catch_up_read(&mut self) {
        self.mark.note_catch_up_read();
    }

    /// The stream ended; open it again from where this link got to, if it
    /// was ever a stream worth having and it has been a moment.
    pub fn reopen_stream_if_due(&mut self, job_id: &str, now: Instant) {
        const STREAM_REOPEN_INTERVAL: Duration = Duration::from_secs(1);
        if self.events_rx.is_some()
            || !self.stream_delivers_deltas()
            || now.duration_since(self.stream_opened_at) < STREAM_REOPEN_INTERVAL
        {
            return;
        }
        self.stream_opened_at = now;
        tracing::debug!(
            %job_id,
            since = self.mark.cursor(),
            "mirror: reopening the owner's stream"
        );
        self.events_rx = {
            self.connection.observe_cursor(self.mark.cursor());
            self.connection.subscribe_in_background()
        };
    }
}

/// What a settled turn's projection said, kept so entries of that turn that
/// reach the transcript file after the settle can still be recognized as
/// content the stream already committed locally, and covered rather than
/// spliced in as a second print.
#[derive(Debug, Clone)]
pub(crate) struct SettledTurnProjection {
    pub(crate) user_uuid: String,
    pub(crate) assistant_text: String,
    pub(crate) thinking_text: String,
    /// Every tool id the projection accounted for: the visible ones and the
    /// deliberately hidden ones alike.
    pub(crate) tool_call_ids: std::collections::HashSet<String>,
}

/// A session this terminal shows but does not run.
///
/// The job is where the session lives; the attachment is this terminal's
/// view of it. Whether a worker is up for the job at the moment is a
/// separate question, answered by `worker`: a mirror follows a live worker,
/// and a mirror whose worker stopped keeps the session on screen — every
/// row it had, the job it belongs to — with nothing on the other end. A
/// prompt typed into it is what gives the job a worker again. The session
/// never comes back to this process either way.
pub struct RemoteBackgroundAttachment {
    pub job_id: String,
    pub session_id: String,
    pub cwd: String,
    pub status: BackgroundJobStatus,
    /// The worker being mirrored, while there is one.
    pub worker: Option<WorkerLink>,
    /// When the job record was last checked for a worker somebody else gave
    /// it, while this attachment has none.
    pub last_worker_probe_at: Instant,
    pub last_event_count: u64,
    /// Byte offset into the job's events log up to which live
    /// `session_update` events have been folded into the TUI.
    pub live_events_offset: u64,
    /// Whether the initial event-history scan has completed. The first scan
    /// locates the last queued user boundary and rebuilds only that live turn.
    pub live_events_initialized: bool,
    /// User UUID that bounds the remote turn currently represented by the
    /// streaming overlay.
    pub current_turn_user_uuid: Option<String>,
    /// Hidden tool ids belong to the remote projection only. Keeping them off
    /// `AppState` prevents remote EnterPlanMode/task tools from changing local
    /// control state.
    pub(crate) remote_hidden_tool_call_ids: std::collections::HashSet<String>,
    /// Display content projected for the current remote turn. These markers are
    /// compared with persisted entries after the queued-user boundary before
    /// the overlay is discarded.
    pub(crate) current_turn_projected_text: String,
    pub(crate) current_turn_projected_thinking: String,
    pub(crate) current_turn_visible_tool_call_ids: std::collections::HashSet<String>,
    pub(crate) awaiting_overlay_absorption: bool,
    /// Persisted transcript fingerprint present when an initial mid-turn
    /// projection was rebuilt. The overlay is not absorbed against that same
    /// snapshot, which would otherwise erase a still-partial live tail.
    pub(crate) initial_overlay_fingerprint: Option<u64>,
    pub transcript_fingerprint: u64,
    /// `(len, mtime)` of the transcript file at the last full read. The
    /// refresh's cheap gate: while a forced refresh fires every interval
    /// during a turn, the file only grows once per completed message, and
    /// an unchanged file means the read, the hash and the replay below it
    /// could not produce anything new. `None` until the first read.
    pub(crate) transcript_file_stat: Option<(u64, Option<std::time::SystemTime>)>,
    pub persisted_transcript_uuids: std::collections::HashSet<String>,
    /// Persisted entry uuids whose content already reached inline scrollback
    /// as locally committed streaming rows (`partial-*` slabs and a settled
    /// turn's tail). A covered entry is never spliced into the transcript as
    /// a new row — that splice is the second print of a message the stream
    /// already drew. A covered entry whose row is on screen anyway keeps
    /// refreshing in place.
    ///
    /// Both this set and [`Self::last_settled_turn`] are only meaningful
    /// while the local rows standing for the covered entries are in the
    /// transcript store. That holds because every path that wipes the store
    /// of an attached session (`/new`, `/clear` — both `apply_new_session`
    /// routes) also drops the whole attachment, and a fresh attachment
    /// starts with both fields empty. A path that wipes the store while
    /// keeping the attachment would make covered entries silently vanish
    /// from the rebuilt transcript.
    pub(crate) covered_persisted_uuids: std::collections::HashSet<String>,
    /// The last turn this terminal settled from its own stream (overlay tail
    /// committed locally). JSONL only appends, so this is the one turn whose
    /// entries can still reach the file after the settle; they are covered
    /// against this snapshot as they appear, and the snapshot drops once the
    /// next user turn is on disk — or when a rewind removes the turn's
    /// boundary from the file.
    pub(crate) last_settled_turn: Option<SettledTurnProjection>,
    /// Local `partial-*` rows that belong to settled turns. They stand for
    /// covered persisted entries permanently, so the merge keeps them
    /// forever. An *unsettled* slab is kept only while its turn is watched
    /// live — a turn that falls to the splice drops its slabs like any
    /// other projection, or the slab and the persisted row it duplicates
    /// would both stay on screen.
    pub(crate) settled_local_row_uuids: std::collections::HashSet<String>,
    /// Whether the turn in flight is still provably watched live (the
    /// projection contained the file at the last refresh that could tell).
    /// The handoff settle consults it: a turn that fell to the splice must
    /// not be committed locally on top of the rows the splice printed.
    pub(crate) current_turn_watched: bool,
    /// Terminal job state can become visible before the final persisted rows
    /// are replayed locally; immutable scrollback must wait for that replay.
    pub(crate) terminal_transcript_synced: bool,
    pub last_refresh_at: Instant,
    pub last_transcript_refresh_at: Instant,
    pub pending_permission_query_id: Option<u64>,
    pub pending_permission_turn_generation: Option<u64>,
    pub pending_permission_endpoint: Option<BackgroundIpcEndpoint>,
    pub pending_permission_rx:
        Option<tokio::sync::oneshot::Receiver<rebon_core::permission::PermissionAnswer>>,
    /// A session-control command forwarded to the worker, still in flight.
    /// One at a time: the answer is what the user is waiting to read, and
    /// interleaving two of them would report them out of order.
    pub pending_command: Option<PendingRemoteCommand>,
    /// The permission mode this UI has told the worker about. `None` until
    /// the first refresh takes the attach-time baseline (the attach target
    /// already carries the worker's mode, so there is nothing to send yet).
    pub synced_permission_mode: Option<rebon_permissions::PermissionMode>,
    /// A permission-mode push in flight. Failure has to reach the user: a
    /// mode the UI shows but the worker never received is the dangerous
    /// direction of wrong.
    pub mode_sync_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// What the owner last said about its MCP servers, off that stream.
    ///
    /// This terminal hosts no servers of its own for a mirrored session, so
    /// this is all `/mcp` has to show. `None` until a `hello` or `status`
    /// carries it — an owner from before the field never sends one.
    ///
    /// Kept beside [`Self::owner`] rather than read out of it because it is
    /// the one field whose absence is ambiguous on the wire: `mcp: None` means
    /// both "I host none" and "I am older than this field", so the last word
    /// stands instead of being blanked.
    pub owner_mcp: Option<rebon_session_host::McpStatusSnapshot>,
    /// Everything the owner last said about itself, kept whole.
    ///
    /// Stored rather than picked apart, because picking apart is how the
    /// mirror got into trouble: the applier read two fields of eighteen and
    /// the rest — model, effort, plan mode — went on being displayed from
    /// this terminal's own start-up values. The snapshot's own doc names that
    /// as "the bug class this whole snapshot exists to remove".
    ///
    /// Two properties are the point, and both come from storing it whole:
    ///
    /// * **A field nobody reads yet costs nothing.** The owner can add one and
    ///   every client has it, with no edit anywhere and no ceremony for the
    ///   clients that do not care. A field nobody *stored* cost a released
    ///   regression.
    /// * **Replaced, never patched.** A value that has left the owner's answer
    ///   must not survive in ours; there is no old field left behind to go
    ///   stale, so a gap shows as unknown rather than as a confident lie.
    ///
    /// `None` only before the first `hello` or `status` lands.
    pub owner: Option<rebon_session_host::SessionStatusSnapshot>,
    /// When the turn on screen started, while one is in flight.
    ///
    /// The mirror's `active_prompt`. A session this terminal runs shows its
    /// spinner and its elapsed clock off the prompt future it holds; a session
    /// it mirrors holds no such future, and until this field existed the event
    /// loop read "no future" as "not loading" and wrote that over whatever the
    /// mirror had learned, every frame. The spinner never came on, the clock
    /// never ran, and the window title said idle through the whole turn.
    ///
    /// Set the instant the user presses Enter, the way a local session's
    /// `started_at` is — the clock the user watches counts from what they did,
    /// not from when the worker got round to it — and kept through the owner's
    /// own `Turn Running`, which arrives later. Cleared by the owner's
    /// `Turn Idle`, by the job record standing terminal with nothing queued
    /// when the owner streams no turns, and by the worker going away.
    pub turn_started_at: Option<Instant>,
    /// Whether the owner's stream has vouched for the turn in
    /// [`Self::turn_started_at`].
    ///
    /// A turn this terminal started optimistically may never run: the worker
    /// can refuse the prompt before it ever announces a turn, and the record
    /// then goes terminal with no `Turn` event to end what nothing began. The
    /// record is allowed to clear such a turn; it is not allowed to clear, or
    /// restart, one the stream announced — the stream says when that ends,
    /// and the record's copy of the status lags it by a write.
    pub turn_started_by_stream: bool,
    /// What the owner last published about a request it is retrying, off the
    /// job record. A local session reads this off its own model client; a
    /// mirror has no client, and a rate-limited turn that says nothing looks
    /// exactly like a hung one.
    pub owner_retry: Option<rebon_session_host::BackgroundRetryProgress>,
}

impl RemoteBackgroundAttachment {
    /// The turn in flight, as a local `ActivePrompt` would report it. The
    /// event loop's loading state and elapsed clock read this for a mirror.
    pub fn running_turn_started_at(&self) -> Option<Instant> {
        self.turn_started_at
    }

    /// A turn is in flight. Idempotent: a turn already running keeps the
    /// instant it started, so the clock does not restart when the owner
    /// confirms what the user's Enter already began.
    pub fn begin_turn(&mut self, now: Instant, from_stream: bool) {
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(now);
        }
        self.turn_started_by_stream |= from_stream;
    }

    /// The turn on screen is over, whoever said so.
    pub fn end_turn(&mut self) {
        self.turn_started_at = None;
        self.turn_started_by_stream = false;
    }
}

/// Ask the owner to change a session option, and turn its answer into the one
/// line the terminal shows.
///
/// A mirrored terminal changing its own configuration would be the exact
/// disagreement the shared-state rule exists to stop: the UI would show one
/// model while the worker kept running under another. The owner is asked, and
/// what it says about when the change takes effect is passed straight through
/// rather than being softened into "done".
pub fn spawn_remote_session_option(
    job_id: &str,
    session_id: &str,
    endpoint: &BackgroundIpcEndpoint,
    key: String,
    value: String,
) -> PendingRemoteCommand {
    let (tx, rx) = std::sync::mpsc::channel();
    let name = key.clone();
    // Named, not blank: the owner fences every envelope against the session it
    // is running, and an empty id is not that session.
    let owner = rebon_session_host::OwnerHandle::for_worker(session_id, Some(job_id), endpoint);
    std::thread::spawn(move || {
        let outcome = owner
            .set_session_option(&key, &value)
            .map_err(|err| err.to_string())
            .map(|applies_from| {
                let when = match applies_from {
                    rebon_session_host::SessionOptionAppliesFrom::NextTurn => {
                        " — it applies from the next turn"
                    }
                    rebon_session_host::SessionOptionAppliesFrom::NextSession => {
                        " — it applies when the session is next built"
                    }
                    rebon_session_host::SessionOptionAppliesFrom::Immediately => "",
                };
                // Not "on the session's host": the user changed this session's
                // model, and which process happens to be running the session is
                // not something they asked about. Where the change landed is
                // this terminal's problem; *when* it takes effect is theirs, so
                // that part stays.
                rebon_session_host::CommandOutput {
                    text: format!("Set {key} to {value}{when}."),
                    tone: "info".into(),
                }
            });
        let _ = tx.send(outcome);
    });
    PendingRemoteCommand { name, rx }
}

/// Hand a prompt to a live worker over IPC.
///
/// The worker writes it to the job record itself, so this is the same
/// durable queue the file path uses — reached through the worker rather
/// than around it, which is what wakes a parked worker now instead of at
/// its next tick. A worker that does not answer is not an error the prompt
/// has to pay for: the caller falls back to writing the record directly.
pub fn reply_to_live_worker(
    session_id: &str,
    job_id: &str,
    endpoint: &BackgroundIpcEndpoint,
    message: String,
    images: Vec<rebon_session_host::BackgroundImageAttachment>,
) -> anyhow::Result<()> {
    rebon_session_host::OwnerHandle::for_worker(session_id, Some(job_id), endpoint)
        .reply(message, images, None)
}

// There is one way to open an owner's stream — the connection — and one place
// a subscription starts. A second entry point took the cursor as an argument
// every caller had to remember, where the connection holds the one it has
// already seen; it was deleted rather than kept as a wrapper.

/// Why a session is waiting for a background worker.
///
/// The wait looks identical from the outside and means opposite things when
/// it ends badly, so the two are not one flag:
/// - `Handover` (`/hosted`): this session has already been given away. Its
///   active lock is gone and the worker is resuming it, so a worker that
///   never arrives leaves the conversation elsewhere, not here.
/// - `Dispatch` (Ctrl+N in agent view): a brand new job, running on its own.
///   Nothing was handed over — a mirror that never arrives costs the view,
///   not the work.
/// - `Reattach`: a job whose worker was gone has been given a new one, and
///   this terminal is waiting to mirror it — because the user opened it from
///   the list, asked for it at startup, or was mirroring it when the worker
///   died. Typing queues on the job, as for a handover: the session is
///   nowhere else.
/// - `Startup`: the default way a session begins. The terminal named the
///   session, started its worker, and built itself as the mirror — nothing
///   was handed over because nothing ran here first. Same consequences as a
///   handover (typing queues on the job, the view is kept), different
///   voice: this wait is the ordinary startup, so it says nothing unless it
///   takes too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingHostedKind {
    Handover,
    Dispatch,
    Reattach {
        /// The session is already on screen (a mirror whose worker died),
        /// so the view is kept rather than rebuilt when the new worker is up.
        keep_view: bool,
    },
    Startup,
}

impl PendingHostedKind {
    /// Whether the session on screen already belongs to the job — its lock
    /// is the worker's, and a prompt typed meanwhile goes to the job.
    pub fn session_is_the_jobs(self) -> bool {
        matches!(self, Self::Handover | Self::Reattach { .. } | Self::Startup)
    }
}

/// A session waiting for a background worker to become mirrorable.
///
/// In both cases what is left is the worker publishing its IPC endpoint,
/// which takes as long as a process start plus engine boot — polled from the
/// event loop rather than blocked on, so the UI keeps drawing.
pub struct PendingHostedSession {
    pub job_id: String,
    pub kind: PendingHostedKind,
    pub started_at: Instant,
    /// When the job store was last probed. The event loop runs far faster
    /// than a worker boots, and every probe reads the job state, reconciles a
    /// possibly-dead pid and may ping a TCP endpoint — all on the UI thread.
    /// `None` means "never probed", so the first poll is not delayed.
    pub last_probe_at: Option<Instant>,
    /// Whether the "this is taking long" notice has gone out. Once: a
    /// notice per probe would be a notice five times a second.
    pub slow_notice_shown: bool,
}

impl PendingHostedSession {
    /// This session was handed to `job_id` and is waiting to mirror it.
    pub fn handover(job_id: String) -> Self {
        Self::new(job_id, PendingHostedKind::Handover)
    }

    /// `job_id`'s worker was started for this brand-new session before the
    /// terminal built itself; mirror it once it is up.
    pub fn startup(job_id: String) -> Self {
        Self::new(job_id, PendingHostedKind::Startup)
    }

    /// A newly dispatched `job_id` is starting, and this session will mirror
    /// it once it is up.
    pub fn dispatch(job_id: String) -> Self {
        Self::new(job_id, PendingHostedKind::Dispatch)
    }

    /// `job_id` had no worker and was given one; mirror it once it is up.
    pub fn reattach(job_id: String, keep_view: bool) -> Self {
        Self::new(job_id, PendingHostedKind::Reattach { keep_view })
    }

    fn new(job_id: String, kind: PendingHostedKind) -> Self {
        Self {
            job_id,
            kind,
            started_at: Instant::now(),
            last_probe_at: None,
            slow_notice_shown: false,
        }
    }

    /// Whether the job store should be read again — and remember that it was.
    pub fn should_probe_now(&mut self, interval: std::time::Duration) -> bool {
        let now = Instant::now();
        if self
            .last_probe_at
            .is_some_and(|last| now.duration_since(last) < interval)
        {
            return false;
        }
        self.last_probe_at = Some(now);
        true
    }
}

/// A session-control command in flight to the attached worker. The name is
/// kept alongside the channel so a worker that dies mid-command can still be
/// reported as "/compact failed" rather than an anonymous dropped channel.
pub struct PendingRemoteCommand {
    pub name: String,
    pub rx: std::sync::mpsc::Receiver<Result<rebon_session_host::CommandOutput, String>>,
}

impl RemoteBackgroundAttachment {
    /// Mirror the worker at `endpoint`.
    pub fn new(
        job_id: String,
        session_id: String,
        cwd: String,
        status: BackgroundJobStatus,
        event_count: u64,
        endpoint: BackgroundIpcEndpoint,
    ) -> Self {
        let worker = WorkerLink::connect(&session_id, &job_id, endpoint);
        let mut attachment = Self::without_worker(job_id, session_id, cwd, status, event_count);
        attachment.worker = Some(worker);
        attachment
    }

    /// Show the session as `job_id`'s, with no worker to follow.
    ///
    /// What a handover that ended without a worker, or a `/stop`, leaves
    /// behind: the session stays on screen and stays the job's, and the
    /// next prompt typed into it gives the job a worker.
    pub fn without_worker(
        job_id: String,
        session_id: String,
        cwd: String,
        status: BackgroundJobStatus,
        event_count: u64,
    ) -> Self {
        Self {
            worker: None,
            // Nothing until the owner speaks. A mirror that has not heard from
            // its owner knows nothing about it, and saying so is the point.
            owner: None,
            last_worker_probe_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            job_id,
            session_id,
            cwd,
            status,
            last_event_count: event_count,
            live_events_offset: 0,
            live_events_initialized: false,
            current_turn_user_uuid: None,
            remote_hidden_tool_call_ids: std::collections::HashSet::new(),
            current_turn_projected_text: String::new(),
            current_turn_projected_thinking: String::new(),
            current_turn_visible_tool_call_ids: std::collections::HashSet::new(),
            awaiting_overlay_absorption: false,
            initial_overlay_fingerprint: None,
            transcript_fingerprint: 0,
            transcript_file_stat: None,
            persisted_transcript_uuids: std::collections::HashSet::new(),
            covered_persisted_uuids: std::collections::HashSet::new(),
            last_settled_turn: None,
            settled_local_row_uuids: std::collections::HashSet::new(),
            current_turn_watched: false,
            terminal_transcript_synced: matches!(
                status,
                BackgroundJobStatus::Idle
                    | BackgroundJobStatus::Succeeded
                    | BackgroundJobStatus::Failed
                    | BackgroundJobStatus::Stopped
            ),
            last_refresh_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            last_transcript_refresh_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            pending_permission_query_id: None,
            pending_permission_turn_generation: None,
            pending_permission_endpoint: None,
            pending_permission_rx: None,
            pending_command: None,
            synced_permission_mode: None,
            mode_sync_rx: None,
            owner_mcp: None,
            // A job attached while it runs shows its spinner from the first
            // frame; the owner's `hello` or the record confirms or clears it.
            turn_started_at: matches!(
                status,
                BackgroundJobStatus::Queued
                    | BackgroundJobStatus::Running
                    | BackgroundJobStatus::NeedsInput
            )
            .then(Instant::now),
            turn_started_by_stream: false,
            owner_retry: None,
        }
    }

    /// Whether a worker is on the other end.
    pub fn is_live(&self) -> bool {
        self.worker.is_some()
    }

    /// Where the worker listens, while there is one.
    ///
    /// Owned rather than borrowed: the endpoint now lives inside the
    /// connection, which builds it from the three fields it is fenced to.
    pub fn endpoint(&self) -> Option<BackgroundIpcEndpoint> {
        self.worker
            .as_ref()
            .map(|worker| worker.connection.endpoint())
    }

    /// The worker is gone — stopped, crashed, or never came — and the job
    /// stands at `status`.
    ///
    /// Everything that was about that process goes with it: the stream, the
    /// probe, the command and permission in flight, what it said about its
    /// MCP servers, the permission mode this terminal had pushed to it. The
    /// rows stay, and so does the job: the session is still the job's, and
    /// the next prompt gives it a worker.
    pub fn worker_gone(&mut self, status: BackgroundJobStatus) {
        self.worker = None;
        self.status = status;
        self.owner_mcp = None;
        self.pending_command = None;
        self.mode_sync_rx = None;
        self.synced_permission_mode = None;
        self.pending_permission_query_id = None;
        self.pending_permission_turn_generation = None;
        self.pending_permission_endpoint = None;
        self.pending_permission_rx = None;
        self.owner_retry = None;
        // Whatever turn was running ran in that process.
        self.end_turn();
        // The job's final rows may still be on their way to disk; let the
        // next transcript refresh fold them in before the tail is sealed.
        self.terminal_transcript_synced = false;
    }

    /// Follow the worker at `endpoint` — one somebody else gave the job, or
    /// one that turned up late. Replaces whatever link there was.
    pub fn link_worker(&mut self, endpoint: BackgroundIpcEndpoint) {
        self.worker = Some(WorkerLink::connect(
            &self.session_id,
            &self.job_id,
            endpoint,
        ));
        self.owner_mcp = None;
        self.synced_permission_mode = None;
        self.mode_sync_rx = None;
    }

    pub(crate) fn turn_is_terminal(&self) -> bool {
        matches!(
            self.status,
            BackgroundJobStatus::Idle
                | BackgroundJobStatus::Succeeded
                | BackgroundJobStatus::Failed
                | BackgroundJobStatus::Stopped
        )
    }

    pub(crate) fn inline_transcript_tail_is_mutable(&self) -> bool {
        !self.turn_is_terminal() || !self.terminal_transcript_synced
    }
}

pub struct BackgroundAttachTarget {
    pub overrides: crate::rebon_config::RuntimeOverride,
    pub cwd: String,
    pub session_id: String,
    pub job_id: String,
    pub name: String,
    pub status: BackgroundJobStatus,
    pub summary: Option<String>,
    pub mode: BackgroundAttachMode,
    pub remote_endpoint: Option<BackgroundIpcEndpoint>,
}

pub(crate) fn attach_target_from_state(
    state: &BackgroundJobState,
    mode: BackgroundAttachMode,
) -> anyhow::Result<BackgroundAttachTarget> {
    let Some(session_id) = state.identity.session_id.clone() else {
        anyhow::bail!(
            "background job {} has not started a session yet",
            state.identity.job_id
        );
    };
    let cwd = background_job_transcript_cwd(state);
    let mut overrides = state.identity.runtime.to_runtime_override()?;
    overrides.resume = (mode != BackgroundAttachMode::RemoteProxy).then(|| session_id.clone());
    overrides.cwd = Some(cwd.clone());
    overrides.attached_background_job_id = Some(state.identity.job_id.clone());
    let remote_endpoint = if mode == BackgroundAttachMode::RemoteProxy {
        Some(BackgroundIpcEndpoint {
            pid: state
                .process
                .pid
                .ok_or_else(|| anyhow::anyhow!("background job has no remote worker pid"))?,
            port: state
                .process
                .ipc_port
                .ok_or_else(|| anyhow::anyhow!("background job has no remote IPC port"))?,
            token: state
                .process
                .ipc_token
                .clone()
                .ok_or_else(|| anyhow::anyhow!("background job has no remote IPC token"))?,
        })
    } else {
        None
    };
    Ok(BackgroundAttachTarget {
        overrides,
        cwd,
        session_id,
        job_id: state.identity.job_id.clone(),
        name: state.identity.name.clone(),
        status: state.process.status,
        summary: state.outcome.summary.clone(),
        mode,
        remote_endpoint,
    })
}

pub fn attach_background_job(job_id: &str) -> anyhow::Result<BackgroundAttachTarget> {
    let store = cli_default_store();
    attach_background_job_in_store(&store, job_id)
}

/// The job `session_id` lives in, if any. A store that cannot be read is
/// logged and answers "none": the caller resumes locally, as it would have
/// before jobs were consulted at all.
pub fn job_for_session(session_id: &str) -> Option<BackgroundJobState> {
    match rebon_session_host::home_job_for_session(&cli_default_store(), session_id) {
        Ok(job) => job,
        Err(err) => {
            tracing::warn!(%err, %session_id, "could not look the session up among background jobs");
            None
        }
    }
}

/// Run a session-control command in the attached worker, off the UI thread.
///
/// The engine that owns this session's context lives over there, so
/// `/compact`, `/context`, `/cost` and the rest have to execute against the
/// real thing rather than this process's shell session. The call can block
/// for a while — a parked worker runs `/compact` for real, which is a
/// provider round-trip — so it goes on its own thread and the refresh loop
/// polls for the answer, the same shape as the endpoint probe.
pub fn spawn_remote_session_command(
    job_id: &str,
    endpoint: &BackgroundIpcEndpoint,
    name: String,
    args: Vec<String>,
) -> PendingRemoteCommand {
    let (tx, rx) = std::sync::mpsc::channel();
    let job_id = job_id.to_string();
    let endpoint = endpoint.clone();
    let sent_name = name.clone();
    std::thread::spawn(move || {
        let _ = tx.send(run_remote_session_command(
            &job_id, &endpoint, &sent_name, args,
        ));
    });
    PendingRemoteCommand { name, rx }
}

fn run_remote_session_command(
    job_id: &str,
    endpoint: &BackgroundIpcEndpoint,
    name: &str,
    args: Vec<String>,
) -> Result<rebon_session_host::CommandOutput, String> {
    let store = cli_default_store();
    let state = store.read_state(job_id).map_err(|err| err.to_string())?;
    // Re-check the endpoint the command was aimed at. A worker that was
    // replaced between keystroke and send would otherwise answer for a
    // different generation of this session.
    if state.process.pid != Some(endpoint.pid)
        || state.process.ipc_port != Some(endpoint.port)
        || state.process.ipc_token.as_deref() != Some(endpoint.token.as_str())
    {
        return Err("the attached worker was replaced before the command was sent".to_string());
    }
    rebon_session_host::run_background_command(
        &state,
        endpoint.port,
        endpoint.token.clone(),
        name.to_string(),
        args,
    )
    .map_err(|err| err.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MirroredJobCompletion {
    Succeeded,
    Failed,
}

pub(crate) fn mirrored_job_completion(
    status: BackgroundJobStatus,
) -> Option<MirroredJobCompletion> {
    match status {
        BackgroundJobStatus::Succeeded => Some(MirroredJobCompletion::Succeeded),
        BackgroundJobStatus::Failed | BackgroundJobStatus::Stopped => {
            Some(MirroredJobCompletion::Failed)
        }
        BackgroundJobStatus::Queued
        | BackgroundJobStatus::Running
        | BackgroundJobStatus::NeedsInput
        | BackgroundJobStatus::Idle => None,
    }
}

pub(crate) fn mirrored_job_is_terminal(status: BackgroundJobStatus) -> bool {
    matches!(
        status,
        BackgroundJobStatus::Succeeded
            | BackgroundJobStatus::Failed
            | BackgroundJobStatus::Stopped
            | BackgroundJobStatus::Idle
    )
}

/// Read the current owner record for a mirrored job.
pub(crate) fn mirrored_job_state_in_store(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<BackgroundJobState> {
    store.read_state(job_id)
}

/// Read the task projection published by a mirrored job.
pub(crate) fn mirrored_task_snapshots_in_store(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<Vec<rebon_session_host::BackgroundTaskSnapshot>> {
    store.read_task_snapshots(job_id)
}

/// Deliver the answer collected by this terminal to the attached owner.
pub(crate) fn answer_remote_permission(
    job_id: &str,
    query_id: u64,
    turn_generation: u64,
    endpoint: &BackgroundIpcEndpoint,
    answer: PermissionAnswer,
) -> anyhow::Result<()> {
    let (option_id, extra_text, updated_input) = match answer {
        PermissionAnswer::Selected {
            option_id,
            extra_text,
            updated_input,
        } => (Some(option_id), extra_text, updated_input),
        PermissionAnswer::Cancelled => (None, None, None),
    };
    cli_default_store().answer_permission_query_for_target_with_updated_input(
        job_id,
        query_id,
        Some(turn_generation),
        Some(endpoint),
        option_id,
        extra_text,
        updated_input,
    )
}

/// Push this UI's permission mode to the attached worker.
///
/// The worker's broker reads its own cell; the one this process cycles with
/// Shift+Tab is a mirror. Without this push the UI would show `plan` while
/// the worker kept asking under `default` — the failure that matters most,
/// because the user believes a restriction is in force.
pub fn spawn_remote_permission_mode(
    job_id: &str,
    endpoint: &BackgroundIpcEndpoint,
    mode: rebon_permissions::PermissionMode,
) -> std::sync::mpsc::Receiver<Result<(), String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let job_id = job_id.to_string();
    let endpoint = endpoint.clone();
    std::thread::spawn(move || {
        let outcome = (|| {
            let store = cli_default_store();
            let state = store.read_state(&job_id).map_err(|err| err.to_string())?;
            if state.process.pid != Some(endpoint.pid)
                || state.process.ipc_port != Some(endpoint.port)
                || state.process.ipc_token.as_deref() != Some(endpoint.token.as_str())
            {
                return Err("the attached worker was replaced".to_string());
            }
            send_background_ipc_request(
                &state,
                endpoint.port,
                endpoint.token.clone(),
                BackgroundIpcRequest::SetPermissionMode {
                    mode: mode.as_wire().to_string(),
                },
            )
            .map_err(|err| err.to_string())
        })();
        let _ = tx.send(outcome);
    });
    rx
}

pub fn background_job_endpoint_is_live(job_id: &str, endpoint: &BackgroundIpcEndpoint) -> bool {
    let store = cli_default_store();
    let Ok(mut state) = store.read_state(job_id) else {
        return false;
    };
    if store.reconcile_stale_pid(&mut state).is_err()
        || state.process.pid != Some(endpoint.pid)
        || state.process.ipc_port != Some(endpoint.port)
        || state.process.ipc_token.as_deref() != Some(endpoint.token.as_str())
    {
        return false;
    }
    send_background_ipc_request(
        &state,
        endpoint.port,
        endpoint.token.clone(),
        BackgroundIpcRequest::Ping,
    )
    .is_ok()
}
