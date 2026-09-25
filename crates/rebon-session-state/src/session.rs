//! In-memory ACP server state: initialization flag + session map.
//!
//! This module owns the narrow state the ACP server handler needs:
//!
//! - track whether the client has completed a successful `initialize` turn
//!   (so other methods can enforce ordering),
//! - mint unique session IDs and remember the cwd each session was created
//!   with, so `session/prompt` handling has something to hang off.
//!
//! Deliberately not implemented here:
//!
//! - a persisted write index, or real abort-controller bookkeeping beyond
//!   the cancel counter. Message history is tracked in a minimal form: each
//!   `session/prompt` request appends its `ContentBlock`s to
//!   `SessionRecord::messages`, but no assistant reply / tool-result
//!   bookkeeping is performed here.
//! - Disk persistence (the JSONL transcript directory under
//!   `~/.rebon/projects/...` is read via [`rebon_session::session_storage`]).
//! - Aborting an in-flight query on `session/cancel`: this module has no
//!   in-flight query to abort, so it records the cancel notification for
//!   observability; the prompt-turn loop at a higher layer consumes it.
//!
//! Everything here uses `std::sync` primitives. The critical sections are all
//! tiny map inserts / atomic flag flips, so a lightweight `Mutex` is a better
//! fit than an async lock.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rebon_proto::types::{ContentBlock, JsonRpcError, McpServerConfig, SessionId, SlashCommand};
use rebon_session::session_storage::{
    find_session_transcript_cwd, load_raw_transcript_from_file, load_session_mode,
    load_session_title, project_dir_path, read_project_cwd_sidecar, reconstruct_chain, same_cwd,
    transcript_file_path, try_acquire_session_active_lock, RawTranscriptFile, SessionActiveLock,
    TranscriptEntry,
};

/// Monotonic identity/revision source for raw transcript handoffs. A process-wide
/// counter prevents a same-id session restored after eviction from matching a
/// stale engine replay cache entry.
static TRANSCRIPT_SOURCE_COUNTER: AtomicU64 = AtomicU64::new(1);
/// Monotonic identity for active prompt ownership. A generation is never reused,
/// so cleanup from a dropped older turn cannot affect a replacement turn for the
/// same session.
static PROMPT_GENERATION_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_transcript_source_version() -> u64 {
    TRANSCRIPT_SOURCE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn next_prompt_generation() -> PromptGeneration {
    PromptGeneration(PROMPT_GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Identity of one acquisition of a session's active-prompt slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptGeneration(u64);

/// Narrow session data needed by `session/prompt`. Unlike `SessionRecord`, this
/// snapshot never clones raw transcript history across the executor await.
#[derive(Debug, Clone)]
pub struct PromptSessionSnapshot {
    pub cwd: String,
    pub mcp_servers: Vec<McpServerConfig>,
    pub generation: PromptGeneration,
}

/// Narrow per-poll view that avoids cloning prompt payloads, raw transcript
/// recovery rows, slash commands, and unrelated session metadata.
#[derive(Debug, Clone)]
pub struct AttachmentSessionSnapshot {
    pub permission_mode: String,
    pub attachment_state: SessionAttachmentState,
}

/// Move-only raw transcript handoff from ACP storage ownership to the engine.
/// `complete` distinguishes a full materialization (load/replace) from the
/// bounded suffix accumulated after the preceding handoff.
pub struct ReplayTranscriptSource {
    pub session_id: SessionId,
    pub cwd: String,
    pub incarnation: u64,
    pub revision: u64,
    pub complete: bool,
    pub last_uuid: Option<String>,
    pub entries: Vec<TranscriptEntry>,
    handoff_id: u64,
    restore_sessions: Option<Weak<Mutex<HashMap<SessionId, SessionRecord>>>>,
}

impl std::fmt::Debug for ReplayTranscriptSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayTranscriptSource")
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("incarnation", &self.incarnation)
            .field("revision", &self.revision)
            .field("complete", &self.complete)
            .field("last_uuid", &self.last_uuid)
            .field("entries", &self.entries)
            .field("handoff_id", &self.handoff_id)
            .finish_non_exhaustive()
    }
}

impl ReplayTranscriptSource {
    fn disarm(&mut self) {
        self.restore_sessions = None;
    }

    fn belongs_to(&self, sessions: &Arc<Mutex<HashMap<SessionId, SessionRecord>>>) -> bool {
        self.restore_sessions
            .as_ref()
            .is_some_and(|origin| Weak::ptr_eq(origin, &Arc::downgrade(sessions)))
    }
}

impl Drop for ReplayTranscriptSource {
    fn drop(&mut self) {
        let Some(sessions) = self
            .restore_sessions
            .take()
            .and_then(|sessions| sessions.upgrade())
        else {
            return;
        };

        // Replay recovery is best-effort and must never introduce a second panic
        // while unwinding the caller. In particular, recover a poisoned map
        // directly rather than routing through the diagnostic explicit API.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut sessions = match sessions.lock() {
                Ok(sessions) => sessions,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(record) = sessions.get_mut(&self.session_id) else {
                return;
            };
            if record.cwd != self.cwd
                || record.transcript_incarnation != self.incarnation
                || record.active_replay_handoff != Some(self.handoff_id)
            {
                return;
            }

            let moved = std::mem::take(&mut self.entries);
            let concurrent = std::mem::take(&mut record.loaded_transcript);
            record.loaded_transcript = ServerState::meld_replay_rows(moved, concurrent);
            record.loaded_transcript_complete = self.complete;
            record.active_replay_handoff = None;
        }));
    }
}

/// Result of finalizing any replay handoff. A revision conflict is resolved
/// atomically by restoring the moved source together with rows appended during
/// the lease and ending the lease before the caller retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayFinalizeOutcome {
    Finalized(Option<String>),
    RetryRequired,
}

/// Opaque host policy with the same lifetime as its session record.
#[derive(Default)]
struct HostPermissionPolicy {
    // The host supplies the concrete policy type and loader. State only owns
    // its lifetime; neither settings parsing nor engine types belong here.
    value: OnceLock<Result<Arc<dyn Any + Send + Sync>, String>>,
}

impl std::fmt::Debug for HostPermissionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostPermissionPolicy")
            .field("initialized", &self.value.get().is_some())
            .finish()
    }
}

/// Per-session in-memory record.
///
/// Currently holds the resolved cwd, a creation timestamp, and a flat list
/// of prompt `ContentBlock`s that have been accepted for this session.
/// Richer fields (assistant replies, tool-use history, …) can land here
/// as the server grows.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: SessionId,
    pub cwd: String,
    host_permission_policy: Arc<HostPermissionPolicy>,
    pub permission_mode: String,
    /// Session mode: `"coordinator"` or `"normal"` when known.
    pub mode: Option<String>,
    pub slash_commands: Arc<Vec<SlashCommand>>,
    /// Wall-clock creation time. Kept as an opaque `SystemTime` instead of a
    /// formatted string; formatting is the concern of whatever layer
    /// surfaces sessions over the wire.
    pub created_at: SystemTime,
    /// Concatenated prompt content blocks in the order they were received.
    ///
    /// This is a stub message history: it only needs to let tests observe
    /// that a `session/prompt` request reached the session. A proper
    /// message transcript (user, assistant, tool-result records) lives at
    /// a higher layer.
    pub messages: Vec<ContentBlock>,
    /// MCP servers supplied by the ACP client for this session activation.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Raw transcript entries loaded from disk by `session/load`.
    ///
    /// This is the read-only companion to `messages`. On-disk transcript
    /// records are richer than the `ContentBlock` stub used for
    /// `messages` — user / assistant / tool-result messages carry
    /// payloads that don't map onto it cleanly. Storing the raw chain
    /// here lets the handler prove the load succeeded and lets higher
    /// layers reach in for the real message bodies without a schema
    /// change here.
    ///
    /// Empty for sessions created via `session/new`; populated only when
    /// a `session/load` request restored a persisted transcript.
    pub loaded_transcript: Vec<TranscriptEntry>,
    /// Identity of this logical raw source. Replacements/reloads receive a new
    /// incarnation so an engine-owned window can never survive a discontinuity.
    transcript_incarnation: u64,
    /// Version of the raw source represented by `loaded_transcript` plus
    /// `last_transcript_uuid`.
    transcript_revision: u64,
    /// True when `loaded_transcript` is the complete canonical chain. False
    /// after the engine has moved it out, when the vector is only a pending
    /// append suffix.
    loaded_transcript_complete: bool,
    /// Raw-chain tail identity retained after the complete vector is released.
    last_transcript_uuid: Option<String>,
    /// Exclusive ownership token while raw recovery rows are moved into an
    /// engine replay rebuild. A load may materialize disk for presentation
    /// during this lease, but may not commit over it.
    active_replay_handoff: Option<u64>,
    /// Last time this session was used for something that implies a live
    /// client: a prompt turn, a transcript append, a replay handoff, a load
    /// that hit memory. Reads that any poller performs (`session/list`,
    /// status queries) deliberately do not count, so an idle session stays
    /// idle no matter how often a dashboard refreshes.
    ///
    /// Only [`ServerState::sweep_idle_transcripts`] reads it, and only to
    /// decide whether the resident raw copy has outlived its usefulness.
    last_touched_at: SystemTime,
    /// Optional custom title loaded from the session sidecar or transcript
    /// metadata. Always `None` for a newly-created session.
    pub title: Option<String>,
    /// Per-iteration attachment state. Tracks transition flags that
    /// the engine's `rebon_agent_core::prompt_executor::PromptExecutor` poller
    /// reads between tool rounds to inject attachment user messages.
    /// Flags (`needs_plan_mode_exit_attachment`, `has_exited_plan_mode`, etc.)
    /// are scoped per-session so parallel sessions don't leak signals
    /// across each other.
    pub attachment_state: SessionAttachmentState,
}

/// Drop a record's resident raw transcript, leaving the tail identity that a
/// later replay needs to validate what it re-reads from disk.
fn release_residency(record: &mut SessionRecord) {
    record.loaded_transcript.clear();
    record.loaded_transcript.shrink_to_fit();
    record.loaded_transcript_complete = false;
}

/// What one [`ServerState::sweep_idle_transcripts`] pass released.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSweep {
    /// Sessions whose resident copy was dropped.
    pub sessions: usize,
    /// Raw transcript rows those copies held.
    pub entries: usize,
}

impl TranscriptSweep {
    /// Whether the pass released anything, so a caller can stay quiet when it
    /// did not.
    pub fn is_empty(&self) -> bool {
        self.sessions == 0
    }
}

/// Transition flags the attachment injector consumes on each tool
/// round. All fields are trivial to clone and the record is held
/// behind the session map's mutex, so callers mutate via dedicated
/// [`ServerState`] methods rather than borrowing the struct directly.
///
/// Behavior notes:
/// * `needs_plan_mode_exit_attachment` tracks pending exit notices.
/// * `has_exited_plan_mode` tracks whether this session has exited plan mode.
/// * `last_emitted_date` stores the last date-change attachment value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionAttachmentState {
    /// Set when the user transitions *out* of plan mode. The next tool
    /// round emits a one-shot `plan_mode_exit` attachment and clears
    /// this flag.
    pub needs_plan_mode_exit_attachment: bool,
    /// Set while the session has ever exited plan mode. Consumed by
    /// `plan_mode_reentry` to distinguish the first plan_mode
    /// injection from a re-entry after a prior exit.
    pub has_exited_plan_mode: bool,
    /// Last ISO date emitted to the model. `None` on the first turn
    /// (no `date_change` attachment is produced, only recorded).
    pub last_emitted_date: Option<String>,
    /// Skill names already announced to the model. Used by
    /// `skill_listing` to dedupe — only the delta is injected on each
    /// poll. The announced names are tracked in `sent_skill_names`.
    pub sent_skill_names: Vec<String>,
    /// Cumulative count of `plan_mode` attachments injected since the
    /// last `plan_mode_exit`. Drives the periodic full reminder that
    /// refreshes a long planning session (every Nth is full). Reset on
    /// exit, so the refresh counts from the start of each stay rather
    /// than across one.
    ///
    /// It does not decide whether a *return* to plan mode gets the full
    /// reminder — `has_exited_plan_mode` does, and a return is always
    /// sparse because the conversation has read the full workflow
    /// already; the `plan_mode` attachment producer implements that split.
    pub plan_mode_attachment_count: u64,
    /// Iteration number of the last `plan_mode` injection within the
    /// current turn. The throttle skips injections within
    /// `TURNS_BETWEEN_ATTACHMENTS` of the previous one.
    pub last_plan_mode_iteration: Option<u64>,
    /// Iteration of the last TaskCreate/TaskUpdate tool use. Used by
    /// the `task_reminder` producer to throttle nudges.
    pub last_task_tool_iteration: Option<u64>,
    /// Iteration of the last `task_reminder` attachment injection.
    pub last_task_reminder_iteration: Option<u64>,
    /// Model-facing runtime messages queued by asynchronous local runtime
    /// events. The attachment poller consumes only the prefix it observed so
    /// concurrent producers cannot lose later messages.
    pub pending_runtime_prompts: Vec<String>,
    /// When `Some`, the next poll should trigger a context reset:
    /// `TurnControlPlugin::run` starts a fresh controller drive with an
    /// "Implement the following plan" user message. Set by
    /// `notify_plan_mode_tool` when the ExitPlanMode tool result
    /// contains `clearContext: true`. That reset clears the conversation
    /// and seeds it with `initialMessage`.
    pub pending_context_reset_plan: Option<String>,
    /// Plan text from the most recent ExitPlanMode call (when
    /// `clearContext` is false). The `plan_mode_exit` attachment
    /// producer embeds this in its message so the plan survives
    /// context pruning and compaction as regular text rather than
    /// being buried in a tool_result JSON blob.
    pub pending_exit_plan_text: Option<String>,
    /// Set when the mode `ExitPlanMode` handed back could not be
    /// applied and a narrower one was used instead. The
    /// `plan_mode_exit` attachment carries it, because the tool
    /// result — written before the mode is applied — already told the
    /// model it got what it asked for.
    pub pending_exit_mode_note: Option<String>,
    /// The mode this session was in when it entered plan mode, while it is
    /// in plan mode; `None` otherwise. A plan entered from `auto` keeps
    /// auto's classifier answering its prompts (see
    /// `rebon_permissions::denial_sink::AutoModeHooks::auto_gates`).
    pub plan_entered_from: Option<String>,
}

/// Apply plan-mode transition side effects to a
/// [`SessionAttachmentState`]. Pure — no I/O, no mutex, safe to call
/// under the session lock.
///
/// Entering plan mode
/// clears any pending `plan_mode_exit` attachment; leaving plan mode
/// sets both `needs_plan_mode_exit_attachment` and
/// `has_exited_plan_mode`. Unchanged cycles (`from == to`) are no-ops
/// so callers can invoke this unconditionally after mutating
/// `permission_mode`.
pub fn apply_plan_mode_transition_flags(state: &mut SessionAttachmentState, from: &str, to: &str) {
    if from == to {
        return;
    }
    let entering_plan = to == "plan" && from != "plan";
    let leaving_plan = from == "plan" && to != "plan";
    if entering_plan {
        state.needs_plan_mode_exit_attachment = false;
        state.pending_exit_plan_text = None;
        state.pending_exit_mode_note = None;
        state.plan_entered_from = Some(from.to_string());
    }
    if leaving_plan {
        state.plan_entered_from = None;
        state.needs_plan_mode_exit_attachment = true;
        state.has_exited_plan_mode = true;
        // Restart the periodic full-reminder cadence, so the refresh
        // counts from the start of the next stay in plan mode rather
        // than across one. It does not make that stay *open* with the
        // full reminder: `has_exited_plan_mode`, set just above, tells
        // the attachment producer this is a return, and a return is
        // sparse.
        state.plan_mode_attachment_count = 0;
        state.last_plan_mode_iteration = None;
    }
}

/// Shared ACP server state.
///
/// Held behind an `Arc` by the ACP server handler so clones of the handler
/// (e.g. when the dispatcher spawns work) observe the same initialization
/// flag, session map, active-prompt slots, and cancel notification tallies.
#[derive(Debug)]
pub struct ServerState {
    initialized: AtomicBool,
    sessions: Arc<Mutex<HashMap<SessionId, SessionRecord>>>,
    /// Sessions that currently have a prompt turn in progress.
    ///
    /// Used to reject a second `session/prompt` request on the
    /// same session while the first one is still streaming. The
    /// handler currently runs the prompt turn synchronously inside
    /// `handle_request`, so the slot is acquired and released within one
    /// call — but the bookkeeping is shaped so a future async turn can
    /// move the slot release to the end of the turn without touching
    /// callers.
    /// Per-session mirrors of `permission_mode`, written under no lock but
    /// only ever from [`ServerState::set_permission_mode`], so a cell can
    /// never disagree with the record it shadows for longer than that call.
    /// See [`ServerState::attach_permission_mode_cell`] for why the gate
    /// reads a cell rather than the record.
    permission_mode_cells:
        Mutex<HashMap<SessionId, Arc<Mutex<rebon_permissions::types::PermissionMode>>>>,
    /// Per-session publishers told every mode a record takes, after the
    /// record and its cell have it. See
    /// [`ServerState::attach_permission_mode_publisher`].
    permission_mode_publishers: Mutex<HashMap<SessionId, PermissionModePublisher>>,
    active_prompts: Mutex<HashMap<SessionId, PromptGeneration>>,
    /// Per-session count of `session/cancel` notifications received.
    ///
    /// This module has no in-flight query to abort, so cancel signals
    /// are recorded by session id for tests and diagnostics; the real
    /// abort wiring lives in the prompt-turn loop at a higher layer.
    cancel_counts: Mutex<HashMap<SessionId, u64>>,
    /// Woken whenever any session releases its active-prompt slot.
    ///
    /// `session/prompt` requests are serialized by the FIFO request
    /// worker, so under client-driven traffic the slot is free by the
    /// time a prompt reaches the handler. A turn spawned off-queue by
    /// `_session/steering` is the one owner a prompt can still collide
    /// with; the prompt path waits on this instead of rejecting.
    prompt_slot_released: tokio::sync::Notify,
    /// Projects root this state takes session ownership under, once
    /// [`ServerState::enable_session_ownership`] has been called.
    ///
    /// `None` means this state does not claim sessions at all: it mints and
    /// loads records without touching the active lock, which is what every
    /// test wants and what an embedder that still holds the lock itself
    /// (today: the TUI) needs. Set once and never changed — a state that
    /// moved between roots mid-life would hold locks under two roots.
    ownership_root: OnceLock<PathBuf>,
    /// Active locks held for the sessions this state owns, keyed by session id.
    ///
    /// An entry here is the process-level assertion that this process,
    /// and only this process, may write that session's transcript.
    /// The map is the custodian rather than [`SessionRecord`] because a record
    /// is `Clone` and handed out on every read, while the lock is deliberately
    /// not clonable — exactly one live value must mean exactly one owner.
    ///
    /// Lock order: never take this while holding `sessions`; the claim path
    /// reads `sessions` first, releases it, then takes this.
    session_locks: Mutex<HashMap<SessionId, SessionActiveLock>>,
}

/// Told the mode a session's record just took, and — while that mode is
/// plan — the mode plan was entered from. See
/// [`ServerState::attach_permission_mode_publisher`].
#[derive(Clone)]
pub struct PermissionModePublisher(Arc<dyn Fn(&str, Option<&str>) + Send + Sync>);

impl PermissionModePublisher {
    pub fn new(publish: impl Fn(&str, Option<&str>) + Send + Sync + 'static) -> Self {
        Self(Arc::new(publish))
    }
}

impl std::fmt::Debug for PermissionModePublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissionModePublisher")
    }
}

/// The permission gate's view of one session's mode.
///
/// The mode comes from the session's cell when the gate has one — the
/// surfaces that hold a cell write it directly, and the record mirrors into
/// it — and from the record otherwise. Where plan mode was entered from only
/// the record knows.
pub struct SessionPermissionModeSource {
    state: Arc<ServerState>,
    session_id: SessionId,
    cell: Option<Arc<Mutex<rebon_permissions::types::PermissionMode>>>,
}

impl SessionPermissionModeSource {
    /// Read everything from the record. An unknown session reads as
    /// `default`, never as something more permissive.
    pub fn record(state: Arc<ServerState>, session_id: impl Into<SessionId>) -> Self {
        Self {
            state,
            session_id: session_id.into(),
            cell: None,
        }
    }

    /// Read the mode from `cell` and the rest from the record.
    pub fn cell(
        state: Arc<ServerState>,
        session_id: impl Into<SessionId>,
        cell: Arc<Mutex<rebon_permissions::types::PermissionMode>>,
    ) -> Self {
        Self {
            state,
            session_id: session_id.into(),
            cell: Some(cell),
        }
    }
}

impl rebon_permissions::denial_sink::PermissionModeProvider for SessionPermissionModeSource {
    fn current_mode(&self) -> rebon_permissions::types::PermissionMode {
        match &self.cell {
            Some(cell) => *cell.lock().expect("mode cell poisoned"),
            None => self
                .state
                .session_permission_mode(&self.session_id)
                .map(|mode| rebon_permissions::types::PermissionMode::from_wire(&mode))
                .unwrap_or(rebon_permissions::types::PermissionMode::Default),
        }
    }

    fn plan_entered_from(&self) -> Option<rebon_permissions::types::PermissionMode> {
        self.state
            .plan_entered_from(&self.session_id)
            .map(|mode| rebon_permissions::types::PermissionMode::from_wire(&mode))
    }
}

/// What a refused claim knows about the process that holds the session.
///
/// Today a refusal can only report *that* the session is held: the lock file
/// carries no payload (a Windows exclusive byte lock blocks other handles from
/// reading it) and the `<sid>.owner.json` descriptor that names pid, surface,
/// and endpoint does not exist yet. The struct is the seam that description
/// lands in; clients already get a typed shape to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOwner {
    pub session_id: String,
    pub cwd: String,
}

impl SessionOwner {
    /// The `data.owner` payload of a refusal.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": self.session_id,
            "cwd": self.cwd,
            // There is no owner descriptor to read; say so rather than
            // inventing a pid.
            "known": false,
        })
    }

    /// The JSON-RPC refusal an ACP surface answers with.
    pub fn to_error(&self) -> JsonRpcError {
        JsonRpcError::session_owned_elsewhere(
            format!(
                "Session {} is open in another Rebon process; close it there first",
                self.session_id
            ),
            self.to_json(),
        )
    }
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            active_prompts: Mutex::new(HashMap::new()),
            cancel_counts: Mutex::new(HashMap::new()),
            prompt_slot_released: tokio::sync::Notify::new(),
            ownership_root: OnceLock::new(),
            session_locks: Mutex::new(HashMap::new()),
            permission_mode_cells: Mutex::new(HashMap::new()),
            permission_mode_publishers: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the host's single authoritative permission policy for this
    /// session activation. Removing the record releases the slot once its
    /// in-flight clones finish; reloading a live record keeps its grants.
    /// There is no server-wide grant overlay.
    /// A failed load stays failed for this activation. A different cwd or host
    /// type is an error, never a reason to silently build a replacement policy.
    pub fn session_permission_policy<T: Any + Send + Sync>(
        &self,
        sid: &str,
        cwd: &str,
        init: impl FnOnce() -> Result<T, String>,
    ) -> Result<Arc<T>, String> {
        let slot = {
            let sessions = self.sessions.lock().map_err(|_| "session state poisoned")?;
            let record = sessions.get(sid).ok_or("permission session not found")?;
            if record.cwd != cwd {
                return Err("permission session cwd mismatch".into());
            }
            record.host_permission_policy.clone()
        };
        slot.value
            .get_or_init(|| init().map(|value| Arc::new(value) as Arc<dyn Any + Send + Sync>))
            .clone()?
            .downcast::<T>()
            .map_err(|_| "permission policy host type mismatch".into())
    }

    /// Register a cell that must keep step with a session's
    /// `permission_mode`.
    ///
    /// The record is where the mode is *stored*, but it is not where every
    /// reader reads it: the engine's permission gate consults a
    /// `PermissionModeProvider` over a plain cell on every tool call,
    /// because it runs far below the layer that can poll a session record.
    /// Two writers reached that pair and only one of them wrote both — the
    /// TUI wrote cell and record together, while a tool-driven move
    /// (`EnterPlanMode` succeeding, `ExitPlanMode` handing back a mode)
    /// wrote only the record, so the gate went on enforcing the mode the
    /// session had left. Registering the cell here makes the record the one
    /// writer everybody goes through, and the cell its shadow.
    ///
    /// Last registration for a session id wins; a session never has two
    /// live gates, and a rebuilt one brings a new cell.
    pub fn attach_permission_mode_cell(
        &self,
        sid: &str,
        cell: Arc<Mutex<rebon_permissions::types::PermissionMode>>,
    ) {
        self.permission_mode_cells
            .lock()
            .expect("permission mode cell map mutex poisoned")
            .insert(sid.to_string(), cell);
    }

    /// Register who has to hear about a session's mode beyond this process.
    ///
    /// The cell keeps the gate in step with the record; nothing kept the
    /// *host* in step. A background job rebuilds its session from the job
    /// record every turn, and only a mode set over IPC ever reached that
    /// record — so a plan the user approved with "auto" was running in plan
    /// again one turn later, and a plan the model entered was forgotten the
    /// same way. The publisher is how a tool-driven move gets to wherever
    /// the host keeps the mode, and to the clients watching it.
    ///
    /// Called after the record and the cell have the mode, with no lock
    /// held, so it may take locks of its own. Last registration for a
    /// session id wins, as with the cell.
    pub fn attach_permission_mode_publisher(&self, sid: &str, publisher: PermissionModePublisher) {
        self.permission_mode_publishers
            .lock()
            .expect("permission mode publisher map mutex poisoned")
            .insert(sid.to_string(), publisher);
    }

    fn publish_permission_mode(&self, sid: &str, mode: &str, plan_entered_from: Option<&str>) {
        let publisher = self
            .permission_mode_publishers
            .lock()
            .expect("permission mode publisher map mutex poisoned")
            .get(sid)
            .cloned();
        if let Some(publisher) = publisher {
            (publisher.0)(mode, plan_entered_from);
        }
    }

    /// Write `mode` into the session's registered cell, if it has one.
    ///
    /// Called with no other lock held: the cell's reader is the permission
    /// gate, which takes the cell and nothing else, so this is a leaf.
    fn mirror_permission_mode(&self, sid: &str, mode: &str) {
        let cell = self
            .permission_mode_cells
            .lock()
            .expect("permission mode cell map mutex poisoned")
            .get(sid)
            .cloned();
        let Some(cell) = cell else {
            return;
        };
        match cell.lock() {
            Ok(mut guard) => *guard = rebon_permissions::types::PermissionMode::from_wire(mode),
            Err(_) => tracing::warn!(
                session_id = %sid,
                mode,
                "rebon: permission mode cell is poisoned; the gate keeps the mode it had"
            ),
        };
    }

    /// Make this state the owner of every session it mints or loads: from here
    /// on `create_session*` and `load_session` take the session's active lock
    /// under `projects_root`, and `load_session` refuses a session another
    /// process holds instead of quietly opening a second writer.
    ///
    /// Idempotent for the same root; returns `false` (and changes nothing) if
    /// a different root was already installed, which is a wiring bug rather
    /// than a runtime condition.
    ///
    /// Not on by default: a state that claims sessions must also be the thing
    /// that outlives them, and the surfaces migrate one at a time.
    pub fn enable_session_ownership(&self, projects_root: PathBuf) -> bool {
        match self.ownership_root.set(projects_root.clone()) {
            Ok(()) => true,
            Err(_) => self.ownership_root.get() == Some(&projects_root),
        }
    }

    /// The root this state claims sessions under, or `None` when ownership is
    /// off and the caller still holds the lock itself.
    pub fn session_ownership_root(&self) -> Option<&Path> {
        self.ownership_root.get().map(PathBuf::as_path)
    }

    /// Whether this state currently holds `sid`'s active lock.
    pub fn owns_session(&self, sid: &str) -> bool {
        self.session_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(sid)
    }

    /// Every session this state holds the active lock for, with the cwd its
    /// record runs under — what a host ending all of its sessions at once has
    /// to clean up after. A record without a lock is somebody else's session
    /// this state only reads, and is left out.
    pub fn owned_sessions(&self) -> Vec<(String, String)> {
        let owned: Vec<String> = self
            .session_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        let sessions = self.sessions.lock().expect("session map mutex poisoned");
        owned
            .into_iter()
            .filter_map(|sid| {
                let cwd = sessions.get(&sid)?.cwd.clone();
                Some((sid, cwd))
            })
            .collect()
    }

    /// Take custody of a lock the caller acquired, returning whatever lock this
    /// state was holding for `sid`.
    ///
    /// The relocation flows acquire under both the source and the target cwd
    /// before any record exists, so they cannot go through the claim path: the
    /// in-process registry inside `try_acquire_session_active_lock` would read
    /// their own lock as another owner. Handing it over here makes the state
    /// the single custodian afterwards.
    ///
    /// A displaced lock comes back to the caller rather than being dropped
    /// here: for one session id the two locks are the *source* and *target*
    /// cwd of a move, and dropping the wrong one deletes a lock file the other
    /// still needs. The caller releases the source once the transcript has
    /// landed, which is the order the relocation already uses.
    #[must_use = "a displaced lock must be released deliberately; dropping it here could release the wrong cwd"]
    pub fn adopt_session_lock(
        &self,
        sid: &str,
        lock: SessionActiveLock,
    ) -> Option<SessionActiveLock> {
        self.session_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(sid.to_string(), lock)
    }

    /// Hand the lock back to the caller, leaving the record in place.
    ///
    /// Used by a handover that keeps serving the session from somewhere else
    /// (`/hosted`) rather than by ordinary close, which goes through
    /// [`Self::close_session`].
    pub fn take_session_lock(&self, sid: &str) -> Option<SessionActiveLock> {
        self.session_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(sid)
    }

    /// Claim `sid` for this process, under the cwd its transcript lives in.
    ///
    /// A no-op when ownership is off or this state already holds the lock.
    /// A failure to acquire because another process holds it is the refusal
    /// that keeps two writers off one transcript; an I/O failure is reported
    /// as an internal error rather than silently read as "free", because
    /// "could not evaluate" must never widen into "nobody owns it".
    pub fn claim_session_ownership(
        &self,
        projects_root: &Path,
        cwd: &str,
        sid: &str,
    ) -> Result<(), JsonRpcError> {
        if self.ownership_root.get().is_none() {
            return Ok(());
        }
        let mut locks = self
            .session_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if locks.contains_key(sid) {
            return Ok(());
        }
        match try_acquire_session_active_lock(projects_root, cwd, sid) {
            Ok(Some(lock)) => {
                locks.insert(sid.to_string(), lock);
                Ok(())
            }
            Ok(None) => Err(SessionOwner {
                session_id: sid.to_string(),
                cwd: cwd.to_string(),
            }
            .to_error()),
            Err(err) => Err(JsonRpcError::internal_error(format!(
                "failed to evaluate the active lock for session {sid}: {err}"
            ))),
        }
    }

    /// A future that resolves after the next active-prompt slot release.
    /// Call `enable()` on the pinned future *before* re-checking the slot
    /// so a release between the check and the await cannot be missed.
    pub fn prompt_slot_released(&self) -> tokio::sync::futures::Notified<'_> {
        self.prompt_slot_released.notified()
    }

    /// Flip the initialized flag. Returns `Err(Already initialized)` if a
    /// previous `initialize` call already succeeded — the handler turns
    /// this into `INVALID_REQUEST: "Already initialized"`.
    pub fn mark_initialized(&self) -> Result<(), JsonRpcError> {
        // Using compare_exchange keeps this race-free even under the
        // (currently unused) possibility of concurrent request handling.
        match self
            .initialized
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(_) => Err(JsonRpcError::invalid_request("Already initialized")),
        }
    }

    /// Return an `Err` if no successful `initialize` has landed yet. Used by
    /// the `session/*` surface as the initialize-first guard, responding
    /// with `INVALID_REQUEST: "Not initialized. Call initialize first."`.
    pub fn require_initialized(&self) -> Result<(), JsonRpcError> {
        if self.initialized.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(JsonRpcError::invalid_request(
                "Not initialized. Call initialize first.",
            ))
        }
    }

    /// True once `initialize` has completed successfully. Exposed for tests
    /// and potential future diagnostics; the dispatch path uses
    /// [`Self::require_initialized`] instead.
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Create, store, and return a new session bound to `cwd`.
    ///
    /// The caller is responsible for having already resolved the effective
    /// cwd (see [`resolve_session_cwd`]). IDs are process-wide distinct via
    /// the shared atomic counter.
    pub fn create_session(&self, cwd: String, mcp_servers: Vec<McpServerConfig>) -> SessionRecord {
        self.create_session_with_permission_mode(cwd, mcp_servers, "default")
    }

    /// Create, store, and return a new session with an explicit initial
    /// permission mode.
    pub fn create_session_with_permission_mode(
        &self,
        cwd: String,
        mcp_servers: Vec<McpServerConfig>,
        permission_mode: &str,
    ) -> SessionRecord {
        let id = rebon_types::new_session_id();
        // A freshly minted id cannot be held by anyone, so the only way this
        // fails is a filesystem that could not be written. Report it loudly
        // and still hand back the session: refusing to start a brand-new
        // conversation because a lock file could not be created would be a
        // worse failure than running it unprotected, and the surfaces that
        // must not run unprotected go through `load_session`.
        if let Some(root) = self.ownership_root.get().cloned() {
            if let Err(err) = self.claim_session_ownership(&root, &cwd, &id) {
                tracing::error!(
                    session_id = %id,
                    cwd = %cwd,
                    error = %err.message,
                    "rebon: could not claim the active lock for a new session"
                );
            }
            // The id says nothing about when it was minted, so this is where
            // a session that never writes a transcript row learns its own age.
            if let Err(err) =
                rebon_session::session_storage::record_session_created_at(&root, &cwd, &id)
            {
                tracing::warn!(
                    session_id = %id,
                    cwd = %cwd,
                    error = %err,
                    "rebon: could not record when a new session was created"
                );
            }
        }
        let record = SessionRecord {
            id: id.clone(),
            cwd,
            permission_mode: permission_mode.to_string(),
            mode: None,
            slash_commands: Arc::new(Vec::new()),
            created_at: SystemTime::now(),
            messages: Vec::new(),
            mcp_servers,
            loaded_transcript: Vec::new(),
            transcript_incarnation: next_transcript_source_version(),
            transcript_revision: next_transcript_source_version(),
            loaded_transcript_complete: true,
            last_transcript_uuid: None,
            active_replay_handoff: None,
            last_touched_at: SystemTime::now(),
            title: None,
            attachment_state: SessionAttachmentState::default(),
            host_permission_policy: Arc::default(),
        };
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .insert(id, record.clone());
        record
    }

    /// Load a session from disk and insert it into the live session map.
    ///
    /// The load path, in order:
    ///
    /// 1. If the session id is already known, update its `cwd` to the
    ///    `requested_cwd` value when non-empty (an empty string
    ///    leaves the existing cwd alone) and return the existing record
    ///    without touching disk.
    /// 2. Otherwise, resolve the effective cwd via [`resolve_session_cwd`]
    ///    — the same fallback chain `session/new` uses, so
    ///    `session/load` with an omitted/empty `cwd` lands on the same
    ///    default path, rather than sanitizing the empty string unchanged.
    ///    Then look up `${projects_root}/${sanitize(effective)}/${sid}.jsonl`
    ///    and try to load it. A missing file, unreadable file, or empty /
    ///    chainless transcript all map to `Err(INVALID_PARAMS: "Session
    ///    not found: {sid}")` when no loadable session is available.
    /// 3. On successful load, mint a fresh `SessionRecord` keyed on the
    ///    resolved cwd, insert it into the session map keyed on the id the client
    ///    asked for, and return it.
    ///
    /// The method is deliberately synchronous. The internal work is all
    /// filesystem I/O — blocking `std::fs` calls are fine for the
    /// current single-task dispatcher and keep tests free of extra
    /// async plumbing. If the dispatcher ever moves onto a thread pool,
    /// this can be wrapped in `spawn_blocking`.
    pub fn load_session(
        &self,
        projects_root: &Path,
        sid: &str,
        requested_cwd: &str,
        fallback_cwd: Option<&str>,
        mcp_servers: Vec<McpServerConfig>,
    ) -> Result<SessionRecord, JsonRpcError> {
        // Ownership before anything else. A record we already have in memory
        // names the cwd its lock lives under; the disk path claims below, once
        // it has resolved where the transcript actually is. Both refuse rather
        // than open a second writer on a session another process holds.
        if let Some(existing_cwd) = self.session_storage_cwd(sid) {
            self.claim_session_ownership(projects_root, &existing_cwd, sid)?;
        }

        // Existing sessions keep their canonical storage cwd. Changing only the
        // in-memory cwd would make later appends diverge from the loaded transcript;
        // callers that need relocation must evict, move under active locks, and load.
        let released_existing_source = {
            let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
            if let Some(existing) = sessions.get_mut(sid) {
                existing.mcp_servers = mcp_servers.clone();
                existing.last_touched_at = SystemTime::now();
                if existing.loaded_transcript_complete && existing.active_replay_handoff.is_none() {
                    return Ok(existing.clone());
                }
                Some((
                    existing.cwd.clone(),
                    existing.transcript_incarnation,
                    existing.transcript_revision,
                ))
            } else {
                None
            }
        };
        if let Some((existing_cwd, captured_incarnation, captured_revision)) =
            released_existing_source
        {
            let path = transcript_file_path(projects_root, &existing_cwd, sid);
            let loaded = load_raw_transcript_from_file(&path)
                .map_err(|e| {
                    JsonRpcError::internal_error(format!(
                        "failed to read transcript for session {sid}: {e}"
                    ))
                })?
                .ok_or_else(|| JsonRpcError::invalid_params(format!("Session not found: {sid}")))?;
            return self.commit_released_load(
                sid,
                &existing_cwd,
                captured_incarnation,
                captured_revision,
                loaded,
            );
        }

        // Fall-through disk path: resolve the effective cwd using the
        // same fallback chain as `session/new`
        // (the requested `cwd`, then the options' `cwd`, then the current
        // process directory), then compose the transcript path. Reusing
        // [`resolve_session_cwd`] keeps the
        // two session entrypoints behaviourally symmetric — in
        // particular, an empty-string `cwd` lands on the handler's
        // `default_cwd` (and then the current process directory) instead of
        // being sanitized into a bogus empty directory component.
        let resolved_cwd = resolve_session_cwd(requested_cwd, fallback_cwd)?;

        let exact_path = transcript_file_path(projects_root, &resolved_cwd, sid);
        let (storage_cwd, path) = if exact_path.is_file() {
            let sidecar_cwd =
                read_project_cwd_sidecar(&project_dir_path(projects_root, &resolved_cwd))
                    .filter(|sidecar_cwd| same_cwd(sidecar_cwd, &resolved_cwd));
            (
                sidecar_cwd.unwrap_or_else(|| resolved_cwd.clone()),
                exact_path,
            )
        } else if let Some(source_cwd) = find_session_transcript_cwd(projects_root, sid) {
            let path = transcript_file_path(projects_root, &source_cwd, sid);
            (source_cwd, path)
        } else {
            (resolved_cwd.clone(), exact_path)
        };
        let raw = load_raw_transcript_from_file(&path).map_err(|e| {
            JsonRpcError::internal_error(format!(
                "failed to read transcript for session {sid}: {e}"
            ))
        })?;
        let raw =
            raw.ok_or_else(|| JsonRpcError::invalid_params(format!("Session not found: {sid}")))?;
        if !raw.parse_complete {
            return Err(JsonRpcError::invalid_params(format!(
                "Session not found: {sid} (transcript contains malformed or unsupported JSONL records)"
            )));
        }
        // A session that exists but has nothing in it yet is still a session.
        // `reconstruct_chain` answers `None` both for "no entries at all" and
        // for "entries that form no chain", and treating the first as absence
        // is how handing a conversation to a background worker *before its
        // first turn* failed: the transcript file was right there, and the
        // worker was told the session did not exist. Only the two real
        // absences stay errors — no file, and rows that cannot be chained.
        let was_empty = raw.entries.is_empty();
        let loaded = match reconstruct_chain(raw.entries) {
            Some(loaded) => loaded,
            None if was_empty => rebon_session::LoadedTranscript {
                messages: Vec::new(),
                created_at: std::time::UNIX_EPOCH,
                title: None,
            },
            None => {
                return Err(JsonRpcError::invalid_params(format!(
                    "Session not found: {sid}"
                )))
            }
        };

        // The transcript's real home is known and the session has been proven
        // to exist, so this is the point to claim it. Reading needed no lock;
        // claiming earlier would leave a lock (and a project directory) behind
        // for every `session/load` of an id that turns out not to be a session.
        self.claim_session_ownership(projects_root, &storage_cwd, sid)?;

        let last_transcript_uuid = loaded.messages.last().map(|entry| entry.uuid.clone());
        let record = SessionRecord {
            id: sid.to_string(),
            cwd: storage_cwd.clone(),
            permission_mode: "default".to_string(),
            mode: load_session_mode(projects_root, &storage_cwd, sid),
            slash_commands: Arc::new(Vec::new()),
            created_at: loaded.created_at,
            messages: Vec::new(),
            mcp_servers,
            loaded_transcript: loaded.messages,
            transcript_incarnation: next_transcript_source_version(),
            transcript_revision: next_transcript_source_version(),
            loaded_transcript_complete: true,
            last_transcript_uuid,
            active_replay_handoff: None,
            last_touched_at: SystemTime::now(),
            title: load_session_title(projects_root, &storage_cwd, sid).or(loaded.title),
            attachment_state: SessionAttachmentState::default(),
            host_permission_policy: Arc::default(),
        };
        self.commit_initial_load(record)
    }

    fn commit_initial_load(&self, record: SessionRecord) -> Result<SessionRecord, JsonRpcError> {
        let sid = record.id.clone();
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.entry(sid) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(record.clone());
                Ok(record)
            }
            std::collections::hash_map::Entry::Occupied(slot) => {
                // The disk read ran without the map lock. A concurrent create/load
                // owns the newer incarnation and may contain undurable recovery
                // rows; never replace it with this stale initially-absent read.
                Ok(slot.get().clone())
            }
        }
    }

    fn commit_released_load(
        &self,
        sid: &str,
        captured_cwd: &str,
        captured_incarnation: u64,
        captured_revision: u64,
        loaded: RawTranscriptFile,
    ) -> Result<SessionRecord, JsonRpcError> {
        if !loaded.parse_complete {
            return Err(JsonRpcError::internal_error(format!(
                "failed to reconcile transcript for session {sid}: one or more nonblank JSONL records are malformed or unsupported"
            )));
        }
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        let existing = sessions
            .get_mut(sid)
            .ok_or_else(|| JsonRpcError::invalid_params(format!("Session not found: {sid}")))?;
        // Filesystem I/O ran without the session lock. A replacement,
        // rewind/reload, relocation, or restoration may have installed a newer
        // source meanwhile; never overwrite it with this stale read.
        if existing.cwd != captured_cwd
            || existing.transcript_incarnation != captured_incarnation
            || existing.transcript_revision < captured_revision
            || existing.loaded_transcript_complete
        {
            return Ok(existing.clone());
        }

        if existing.active_replay_handoff.is_some() {
            // The engine still owns moved recovery rows. Explicit load presents
            // the complete canonical disk history without committing a new
            // incarnation over that ownership.
            let entries = reconstruct_chain(loaded.entries)
                .map(|loaded| loaded.messages)
                .ok_or_else(|| {
                    JsonRpcError::internal_error(format!(
                        "failed to reconcile transcript for session {sid}: disk history has no canonical chain"
                    ))
                })?;
            if existing
                .last_transcript_uuid
                .as_deref()
                .is_some_and(|tail| !entries.iter().any(|entry| entry.uuid == tail))
            {
                return Err(JsonRpcError::internal_error(format!(
                    "failed to reconcile transcript for session {sid}: disk history lost the prior canonical tail"
                )));
            }
            let mut materialized = existing.clone();
            materialized.loaded_transcript = entries;
            materialized.loaded_transcript_complete = true;
            materialized.last_transcript_uuid = materialized
                .loaded_transcript
                .last()
                .map(|entry| entry.uuid.clone());
            materialized.active_replay_handoff = None;
            return Ok(materialized);
        }

        let same_entry = |left: &TranscriptEntry, right: &TranscriptEntry| {
            left.uuid == right.uuid
                && left.entry_type == right.entry_type
                && left.parent_uuid == right.parent_uuid
                && left.timestamp == right.timestamp
                && left.raw == right.raw
        };
        let disk_by_uuid = loaded
            .entries
            .iter()
            .map(|entry| (entry.uuid.as_str(), entry))
            .collect::<HashMap<_, _>>();
        let last_pending_index = existing
            .loaded_transcript
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.uuid.as_str(), index))
            .collect::<HashMap<_, _>>();
        let required_recovery = existing
            .loaded_transcript
            .iter()
            .enumerate()
            .filter(|(index, pending)| {
                last_pending_index.get(pending.uuid.as_str()) == Some(index)
                    && disk_by_uuid
                        .get(pending.uuid.as_str())
                        .is_none_or(|disk| !same_entry(disk, pending))
            })
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>();

        // Pending rows follow raw disk rows. This is the single canonical
        // reconstruction, and therefore preserves the transcript's normal
        // last-write-wins duplicate-UUID behavior.
        let mut overlaid = loaded.entries;
        overlaid.extend(existing.loaded_transcript.iter().cloned());
        let entries = reconstruct_chain(overlaid)
            .map(|loaded| loaded.messages)
            .ok_or_else(|| {
                JsonRpcError::internal_error(format!(
                    "failed to reconcile transcript for session {sid}: disk history and pending rows have no canonical chain"
                ))
            })?;
        let canonical_by_uuid = entries
            .iter()
            .map(|entry| (entry.uuid.as_str(), entry))
            .collect::<HashMap<_, _>>();
        if required_recovery.iter().any(|required| {
            canonical_by_uuid
                .get(required.uuid.as_str())
                .is_none_or(|canonical| !same_entry(canonical, required))
        }) || existing
            .last_transcript_uuid
            .as_deref()
            .is_some_and(|tail| !canonical_by_uuid.contains_key(tail))
        {
            return Err(JsonRpcError::internal_error(format!(
                "failed to reconcile transcript for session {sid}: required pending rows are not continuous with the canonical chain"
            )));
        }

        let canonical_tail = entries.last().map(|entry| entry.uuid.clone());
        // Keep only rows that are not durably represented by disk. The returned
        // presentation is complete, while resident recovery remains bounded and
        // survives release/rebuild cycles.
        existing.loaded_transcript = required_recovery;
        existing.last_transcript_uuid = canonical_tail.clone();
        existing.loaded_transcript_complete = false;
        existing.transcript_incarnation = next_transcript_source_version();
        existing.transcript_revision = next_transcript_source_version();

        let mut materialized = existing.clone();
        materialized.loaded_transcript = entries;
        materialized.loaded_transcript_complete = true;
        materialized.last_transcript_uuid = canonical_tail;
        Ok(materialized)
    }

    /// List every session known to this server for the given request.
    ///
    /// Behavior:
    ///
    /// 1. Resolve the effective cwd via the same fallback chain that
    ///    `session/new` and `session/load` use (explicit param → handler
    ///    fallback → process cwd). A blank `requested_cwd` activates the
    ///    fallback; anything non-blank wins unchanged.
    /// 2. Seed the result from a snapshot of the entire in-memory
    ///    session map, not just sessions that match `effective_cwd`.
    /// 3. Scan `${projects_root}/${sanitize(effective_cwd)}/` for
    ///    `*.jsonl` files. Every `.jsonl` stem that is not already
    ///    present in the result becomes a lightweight placeholder
    ///    [`SessionRecord`] with `cwd = effective_cwd`, `created_at = mtime`
    ///    (or [`UNIX_EPOCH`] when the stat or mtime lookup fails), an
    ///    empty message log, and no title. Non-`.jsonl` files are
    ///    ignored and `read_dir` failures (including "directory does
    ///    not exist") degrade to "no disk entries".
    /// 4. Apply a post-filter **only** if the caller supplied an
    ///    explicit non-blank cwd. With explicit cwd the result is
    ///    filtered to records whose cwd is the same working directory
    ///    (`same_cwd`, so Windows spelling variants of one directory
    ///    still match). With omitted/blank
    ///    cwd the result is every in-memory session (regardless of
    ///    their cwd) plus the on-disk placeholders discovered under
    ///    the effective cwd. This asymmetry is intentional and must be
    ///    preserved rather than papered over.
    ///
    /// In-memory sessions always win on id collision: the disk scanner
    /// checks for existing ids before inserting a placeholder. The
    /// returned slice is **not** sorted; callers (and tests) that care
    /// about ordering should use set-membership assertions or sort
    /// explicitly.
    pub fn list_sessions(
        &self,
        projects_root: &Path,
        requested_cwd: Option<&str>,
        fallback_cwd: Option<&str>,
    ) -> Result<Vec<SessionRecord>, JsonRpcError> {
        // Truthiness semantics: empty string falls back, any
        // other string (including whitespace-only) wins unchanged.
        let effective_cwd = resolve_session_cwd(requested_cwd.unwrap_or(""), fallback_cwd)?;

        // Should the post-filter kick in?
        let filter_cwd: Option<String> = match requested_cwd {
            Some(c) if !c.is_empty() => Some(c.to_string()),
            _ => None,
        };

        // Step 1: snapshot of the entire in-memory session map. The starting
        // set is *not* pre-filtered by cwd.
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut merged: Vec<SessionRecord> = {
            let sessions = self.sessions.lock().expect("session map mutex poisoned");
            let mut v = Vec::with_capacity(sessions.len());
            for record in sessions.values() {
                seen_ids.insert(record.id.clone());
                v.push(record.clone());
            }
            v
        };

        // Step 2: scan the disk-level project directory for this cwd.
        // Errors from `read_dir` (nonexistent directory, permission
        // denied, etc.) degrade to "no disk sessions" — never propagate
        // as `internal_error` to the client.
        let project_dir = project_dir_path(projects_root, &effective_cwd);
        if let Ok(read_dir) = std::fs::read_dir(&project_dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                // Only `.jsonl` files contribute to the placeholder set.
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => continue,
                };
                if seen_ids.contains(&stem) {
                    // In-memory sessions win on id collision.
                    continue;
                }
                // Resolve the file's mtime; any stat/mtime error
                // falls back to `UNIX_EPOCH`.
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(UNIX_EPOCH);
                // Read the AI-generated title from the sidecar file
                // (`{session_id}.meta.json`) if one exists. Missing or
                // corrupt sidecars silently produce `None`, matching
                // the old behaviour — the resume dialog then falls
                // through to the jsonl-first-message layer.
                let cached_title = rebon_session::session_storage::load_session_title(
                    projects_root,
                    &effective_cwd,
                    &stem,
                );
                seen_ids.insert(stem.clone());
                let session_mode = load_session_mode(projects_root, &effective_cwd, &stem);
                merged.push(SessionRecord {
                    id: stem,
                    cwd: effective_cwd.clone(),
                    permission_mode: "default".to_string(),
                    mode: session_mode,
                    slash_commands: Arc::new(Vec::new()),
                    created_at: mtime,
                    messages: Vec::new(),
                    mcp_servers: Vec::new(),
                    loaded_transcript: Vec::new(),
                    transcript_incarnation: next_transcript_source_version(),
                    transcript_revision: next_transcript_source_version(),
                    loaded_transcript_complete: true,
                    last_transcript_uuid: None,
                    active_replay_handoff: None,
                    last_touched_at: SystemTime::now(),
                    title: cached_title,
                    attachment_state: SessionAttachmentState::default(),
                    host_permission_policy: Arc::default(),
                });
            }
        }

        // Step 3: optional post-filter. With explicit non-blank cwd we
        // keep only records for the same working directory (`same_cwd`
        // collapses Windows case/separator spelling variants); with omitted
        // cwd we return everything — including in-memory sessions
        // that live under completely unrelated cwds.
        if let Some(ref cwd) = filter_cwd {
            merged.retain(|s| same_cwd(&s.cwd, cwd));
        }

        Ok(merged)
    }

    /// Begin a prompt turn for `sid`.
    ///
    /// Error ordering:
    ///
    /// 1. Missing session id → `INVALID_PARAMS: "Session not found: {sid}"`.
    /// 2. Slot already taken → `INVALID_REQUEST: "Session already has an
    ///    active prompt turn"`.
    ///
    /// On success the caller owns the active-prompt slot. Production callers
    /// retain the returned generation in an RAII guard and release it through
    /// [`Self::finish_prompt`], so task cancellation and unwinding cannot leak
    /// ownership. [`Self::end_prompt`] remains for direct API/test callers.
    ///
    /// The returned [`SessionRecord`] is a snapshot of the session at
    /// slot-acquire time; callers that want to append to the live session
    /// should use [`Self::append_prompt_messages`] rather than mutating
    /// this clone.
    /// Acquire the active-prompt slot while cloning only the fields the handler
    /// needs. This is the production path; `begin_prompt` remains for API/test
    /// compatibility but may clone a materialized transcript.
    pub fn begin_prompt_snapshot(&self, sid: &str) -> Result<PromptSessionSnapshot, JsonRpcError> {
        let (cwd, mcp_servers) = {
            let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
            sessions.get_mut(sid).map(|record| {
                record.last_touched_at = SystemTime::now();
                (record.cwd.clone(), record.mcp_servers.clone())
            })
        }
        .ok_or_else(|| JsonRpcError::invalid_params(format!("Session not found: {sid}")))?;

        let generation = next_prompt_generation();
        let mut active = self
            .active_prompts
            .lock()
            .expect("active prompts mutex poisoned");
        if active.contains_key(sid) {
            return Err(JsonRpcError::invalid_request(
                "Session already has an active prompt turn",
            ));
        }
        active.insert(sid.to_string(), generation);
        Ok(PromptSessionSnapshot {
            cwd,
            mcp_servers,
            generation,
        })
    }

    /// Acquire prompt ownership and retain its diagnostic message snapshot as one
    /// state transition. Callers serialize this transition with cancellation-handle
    /// registration, so cancellation can never observe active ownership without the
    /// matching generation-tagged handle. Poisoned lifecycle locks are recovered:
    /// poisoning must not turn a prior unwind into a permanently stuck session.
    pub fn begin_prompt_lifecycle(
        &self,
        sid: &str,
        messages: Vec<ContentBlock>,
    ) -> Result<PromptSessionSnapshot, JsonRpcError> {
        // This is the same order used by `finish_prompt`; keep it stable.
        let mut active = self
            .active_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if active.contains_key(sid) {
            return Err(JsonRpcError::invalid_request(
                "Session already has an active prompt turn",
            ));
        }
        let record = sessions
            .get_mut(sid)
            .ok_or_else(|| JsonRpcError::invalid_params(format!("Session not found: {sid}")))?;
        let snapshot = PromptSessionSnapshot {
            cwd: record.cwd.clone(),
            mcp_servers: record.mcp_servers.clone(),
            generation: next_prompt_generation(),
        };

        record.messages = messages;
        record.last_touched_at = SystemTime::now();
        active.insert(sid.to_string(), snapshot.generation);
        Ok(snapshot)
    }

    pub fn begin_prompt(&self, sid: &str) -> Result<SessionRecord, JsonRpcError> {
        let record = {
            let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
            sessions.get_mut(sid).map(|record| {
                record.last_touched_at = SystemTime::now();
                record.clone()
            })
        };
        let record = record
            .ok_or_else(|| JsonRpcError::invalid_params(format!("Session not found: {sid}")))?;

        let generation = next_prompt_generation();
        let mut active = self
            .active_prompts
            .lock()
            .expect("active prompts mutex poisoned");
        if active.contains_key(sid) {
            return Err(JsonRpcError::invalid_request(
                "Session already has an active prompt turn",
            ));
        }
        active.insert(sid.to_string(), generation);
        Ok(record)
    }

    /// Release the active-prompt slot for `sid`. Idempotent: releasing a
    /// slot that was never held is a no-op so callers can place this in a
    /// cleanup path without tracking whether `begin_prompt` succeeded.
    pub fn end_prompt(&self, sid: &str) {
        self.active_prompts
            .lock()
            .expect("active prompts mutex poisoned")
            .remove(sid);
    }

    /// Clear the diagnostic prompt snapshot and release the slot only when it
    /// is still owned by `generation`. Holding the ownership map while clearing
    /// the session closes the race where a replacement prompt could publish its
    /// messages between a stale ownership check and the clear. The lock order is
    /// always active-prompts then sessions; no other path nests these locks.
    pub fn finish_prompt(&self, sid: &str, generation: PromptGeneration) -> bool {
        let mut active = self
            .active_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.get(sid) != Some(&generation) {
            return false;
        }

        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = sessions.get_mut(sid) {
            record.messages.clear();
            record.messages.shrink_to_fit();
        }
        active.remove(sid);
        drop(sessions);
        drop(active);
        // Wake after both locks are released so a woken prompt's re-acquire
        // never contends with this cleanup.
        self.prompt_slot_released.notify_waiters();
        true
    }

    /// True if `sid` currently has an active prompt turn. Exposed for
    /// tests and diagnostics; the dispatch path goes through
    /// [`Self::begin_prompt`] / [`Self::end_prompt`] instead.
    pub fn is_prompt_active(&self, sid: &str) -> bool {
        self.active_prompts
            .lock()
            .expect("active prompts mutex poisoned")
            .contains_key(sid)
    }

    /// Replace the live session's message log with `blocks`.
    ///
    /// Returns `true` if the session existed (and the blocks were stored),
    /// `false` if the session id was unknown. The dispatch path calls this
    /// after [`Self::begin_prompt`] has already verified existence, so the
    /// `false` arm is effectively unreachable — it's still returned so the
    /// function is safe to call from tests that poke the state directly.
    ///
    /// Store only the current prompt's diagnostic snapshot. Historical model
    /// truth lives in JSONL; accumulating these blocks duplicates arbitrarily
    /// large images/resources in steady-state RAM.
    pub fn append_prompt_messages(&self, sid: &str, blocks: Vec<ContentBlock>) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.messages = blocks;
                record.last_touched_at = SystemTime::now();
                true
            }
            None => false,
        }
    }

    /// Release the current prompt snapshot after its executor turn completes.
    pub fn clear_prompt_messages(&self, sid: &str) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.messages.clear();
                record.messages.shrink_to_fit();
                true
            }
            None => false,
        }
    }

    /// Capacity of a session's in-memory prompt-message buffer.
    ///
    /// Test-observability only: the ACP server's tests assert that a
    /// finished prompt actually shrinks the buffer. Deliberately not
    /// `#[cfg(test)]`-gated: a `cfg(test)` item is invisible across a crate
    /// boundary, and the tests that need this live in another crate.
    pub fn prompt_messages_capacity(&self, sid: &str) -> Option<usize> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(sid)
            .map(|record| record.messages.capacity())
    }

    /// Atomically move resident raw history to the engine under an exclusive
    /// handoff lease. Once taken, ACP retains source metadata, the lease token,
    /// and subsequent pending appends until the engine explicitly finalizes.
    pub fn take_transcript_for_replay(&self, sid: &str) -> Option<ReplayTranscriptSource> {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        let record = sessions.get_mut(sid)?;
        if record.active_replay_handoff.is_some() {
            return None;
        }
        let handoff_id = next_transcript_source_version();
        record.active_replay_handoff = Some(handoff_id);
        record.last_touched_at = SystemTime::now();
        let complete = record.loaded_transcript_complete;
        record.loaded_transcript_complete = false;
        Some(ReplayTranscriptSource {
            session_id: record.id.clone(),
            cwd: record.cwd.clone(),
            incarnation: record.transcript_incarnation,
            revision: record.transcript_revision,
            complete,
            last_uuid: record.last_transcript_uuid.clone(),
            entries: std::mem::take(&mut record.loaded_transcript),
            handoff_id,
            restore_sessions: Some(Arc::downgrade(&self.sessions)),
        })
    }

    fn replay_handoff_record<'a>(
        sessions: &'a mut HashMap<SessionId, SessionRecord>,
        source: &ReplayTranscriptSource,
    ) -> Result<Option<&'a mut SessionRecord>, String> {
        let Some(record) = sessions.get_mut(&source.session_id) else {
            // Eviction/recreation intentionally supersedes this source.
            return Ok(None);
        };
        if record.cwd != source.cwd || record.transcript_incarnation != source.incarnation {
            // Replacement, rewind, or relocation installed a new incarnation.
            return Ok(None);
        }
        if record.active_replay_handoff != Some(source.handoff_id) {
            return Err(format!(
                "replay handoff ownership mismatch for session {}",
                source.session_id
            ));
        }
        Ok(Some(record))
    }

    fn retain_final_replay_rows(entries: Vec<TranscriptEntry>) -> Vec<TranscriptEntry> {
        // Transcript reconstruction is last-write-wins by UUID. Keeping an older
        // occurrence in the recovery suffix is both redundant and dangerous: a
        // validator cannot require two different values for the same canonical
        // UUID. Walk backwards, retain each UUID once, then restore the ordering
        // of the final occurrences.
        let mut seen = HashSet::with_capacity(entries.len());
        let mut retained = entries
            .into_iter()
            .rev()
            .filter(|entry| seen.insert(entry.uuid.clone()))
            .collect::<Vec<_>>();
        retained.reverse();
        retained
    }

    fn meld_replay_rows(
        mut moved: Vec<TranscriptEntry>,
        concurrent: Vec<TranscriptEntry>,
    ) -> Vec<TranscriptEntry> {
        // Rows appended while the replay lease was active are newer than the
        // moved source. Applying one final-occurrence pass to the concatenation
        // preserves last-write-wins behavior across and within both batches.
        moved.extend(concurrent);
        Self::retain_final_replay_rows(moved)
    }

    /// Put a moved raw source back after replay fails. Older moved rows are
    /// prepended to any suffix that arrived meanwhile and the lease ends in the
    /// same critical section. A newer incarnation safely supersedes the source.
    pub fn restore_transcript_after_failed_replay(
        &self,
        mut source: ReplayTranscriptSource,
    ) -> Result<(), String> {
        if !source.belongs_to(&self.sessions) {
            return Err(format!(
                "replay handoff belongs to a different server state for session {}",
                source.session_id
            ));
        }
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        let record = match Self::replay_handoff_record(&mut sessions, &source) {
            Ok(Some(record)) => record,
            Ok(None) => {
                source.disarm();
                return Ok(());
            }
            Err(err) => {
                source.disarm();
                return Err(err);
            }
        };

        let current = std::mem::take(&mut record.loaded_transcript);
        let moved = std::mem::take(&mut source.entries);
        record.loaded_transcript = Self::meld_replay_rows(moved, current);
        record.loaded_transcript_complete = source.complete;
        record.active_replay_handoff = None;
        source.disarm();
        Ok(())
    }

    /// Finalize a handoff served from a matching engine replay window. The
    /// window intentionally retains no raw ancestry, so any revision advance is
    /// a conflict rather than something this path may guess through. Conflict
    /// handling restores all rows and releases the lease in this critical
    /// section; the engine must invalidate the window and rebuild canonically.
    pub fn finalize_cached_transcript_after_replay(
        &self,
        mut source: ReplayTranscriptSource,
        cached_tail: Option<String>,
    ) -> Result<ReplayFinalizeOutcome, String> {
        if !source.belongs_to(&self.sessions) {
            return Err(format!(
                "replay handoff belongs to a different server state for session {}",
                source.session_id
            ));
        }
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        let record = match Self::replay_handoff_record(&mut sessions, &source) {
            Ok(Some(record)) => record,
            Ok(None) => {
                source.disarm();
                return Ok(ReplayFinalizeOutcome::RetryRequired);
            }
            Err(err) => {
                source.disarm();
                return Err(err);
            }
        };

        let current = std::mem::take(&mut record.loaded_transcript);
        let moved = std::mem::take(&mut source.entries);
        if record.transcript_revision != source.revision {
            record.loaded_transcript = Self::meld_replay_rows(moved, current);
            record.loaded_transcript_complete = source.complete;
            record.active_replay_handoff = None;
            source.disarm();
            return Ok(ReplayFinalizeOutcome::RetryRequired);
        }

        // A matching released window has no resident source rows. Preserve them
        // defensively if that invariant changes rather than silently dropping
        // ownership during a nominal cache hit.
        record.loaded_transcript = Self::meld_replay_rows(moved, current);
        record.loaded_transcript_complete = false;
        record.last_transcript_uuid = cached_tail.clone();
        record.active_replay_handoff = None;
        source.disarm();
        Ok(ReplayFinalizeOutcome::Finalized(cached_tail))
    }

    /// Complete a successful replay rebuild. Finalization succeeds only when the
    /// source revision is still current, guaranteeing that the projection and
    /// returned parent describe the same raw history. On conflict, all moved and
    /// concurrently appended rows are melded back atomically and the lease ends.
    pub fn finalize_transcript_after_replay(
        &self,
        mut source: ReplayTranscriptSource,
        retained: Vec<TranscriptEntry>,
        canonical_tail: Option<String>,
    ) -> Result<ReplayFinalizeOutcome, String> {
        if !source.belongs_to(&self.sessions) {
            return Err(format!(
                "replay handoff belongs to a different server state for session {}",
                source.session_id
            ));
        }
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        let record = match Self::replay_handoff_record(&mut sessions, &source) {
            Ok(Some(record)) => record,
            Ok(None) => {
                source.disarm();
                return Ok(ReplayFinalizeOutcome::RetryRequired);
            }
            Err(err) => {
                source.disarm();
                return Err(err);
            }
        };

        let current = std::mem::take(&mut record.loaded_transcript);
        if record.transcript_revision != source.revision {
            let moved = std::mem::take(&mut source.entries);
            record.loaded_transcript = Self::meld_replay_rows(moved, current);
            record.loaded_transcript_complete = source.complete;
            record.active_replay_handoff = None;
            source.disarm();
            return Ok(ReplayFinalizeOutcome::RetryRequired);
        }

        // With an unchanged revision there are no concurrent rows. Still use the
        // duplicate-aware meld defensively so a future non-revisioned resident
        // row cannot reverse last-writer-wins ordering.
        record.loaded_transcript = Self::meld_replay_rows(retained, current);
        record.loaded_transcript_complete = false;
        record.last_transcript_uuid = canonical_tail.clone();
        record.active_replay_handoff = None;
        source.disarm();
        Ok(ReplayFinalizeOutcome::Finalized(canonical_tail))
    }

    /// Release duplicate complete raw residency after a presentation store has
    /// materialized it. Unlike a replay take, this operation creates no lease.
    pub fn release_transcript_residency(&self, sid: &str) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        let Some(record) = sessions.get_mut(sid) else {
            return false;
        };
        if record.active_replay_handoff.is_some() {
            return false;
        }
        release_residency(record);
        true
    }

    /// Release resident raw transcripts for sessions that have gone idle.
    ///
    /// A complete resident transcript is a duplicate of the on-disk chain, not
    /// the only copy: [`Self::load_session`] re-reads from disk once residency
    /// is released, and the engine's replay rebuild treats disk as
    /// authoritative for a released source. This is the same release the TUI
    /// already performs the moment it has consumed a resumed transcript — what
    /// it adds is a trigger for the sessions nobody ever consumes.
    ///
    /// That case is specific to a long-lived server. `rebon serve` keeps a
    /// session record for as long as a browser tab remembers its id, and every
    /// turn appends to the resident copy ([`Self::push_transcript_entries`]),
    /// so an abandoned tab's session grows for the life of the process. A
    /// short-lived `--acp` or TUI process never notices; a server running for
    /// days does.
    ///
    /// A session is skipped while anything could still be mid-flight — an
    /// active prompt turn, an outstanding replay handoff, or a transcript that
    /// is only a pending append suffix (`loaded_transcript_complete == false`),
    /// whose rows have not yet been proven durable. A session whose transcript
    /// file is missing is skipped as well, so this can never discard the only
    /// copy of anything.
    ///
    /// Returns what was released, for the caller to log.
    pub fn sweep_idle_transcripts(
        &self,
        projects_root: &Path,
        idle_after: Duration,
    ) -> TranscriptSweep {
        let now = SystemTime::now();
        // Same lock order as `begin_prompt_lifecycle`: active-prompts, then
        // sessions. Taking them the other way round here would be the one site
        // that can deadlock against a starting turn.
        let active = self
            .active_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut swept = TranscriptSweep::default();
        for (sid, record) in sessions.iter_mut() {
            if record.loaded_transcript.is_empty()
                || !record.loaded_transcript_complete
                || record.active_replay_handoff.is_some()
                || active.contains_key(sid)
            {
                continue;
            }
            let idle = now
                .duration_since(record.last_touched_at)
                .unwrap_or_default();
            if idle < idle_after {
                continue;
            }
            if !transcript_file_path(projects_root, &record.cwd, sid).is_file() {
                continue;
            }
            swept.sessions += 1;
            swept.entries += record.loaded_transcript.len();
            release_residency(record);
        }
        swept
    }

    /// The cwd a live record's transcript is stored under, without cloning the
    /// transcript vector. `None` when the session is not in memory.
    pub fn session_storage_cwd(&self, sid: &str) -> Option<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(sid)
            .map(|record| record.cwd.clone())
    }

    /// Raw-chain tail identity without cloning the transcript vector.
    pub fn transcript_tail_uuid(&self, sid: &str) -> Option<String> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(sid)
            .and_then(|record| record.last_transcript_uuid.clone())
    }

    /// Session title without cloning the transcript vector.
    pub fn session_title(&self, sid: &str) -> Option<String> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(sid)
            .and_then(|record| record.title.clone())
    }

    pub fn push_transcript_entries(&self, sid: &str, entries: Vec<TranscriptEntry>) -> bool {
        if entries.is_empty() {
            // No-op but still succeed if the session exists.
            return self
                .sessions
                .lock()
                .expect("session map mutex poisoned")
                .contains_key(sid);
        }
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                // Once complete raw residency has been released, appended rows
                // are only an unverified recovery suffix. Replay finalization
                // advances this canonical tail atomically after proving that
                // suffix is direct and unambiguous.
                if record.loaded_transcript_complete {
                    record.last_transcript_uuid = entries.last().map(|entry| entry.uuid.clone());
                }
                record.loaded_transcript.extend(entries);
                record.transcript_revision = next_transcript_source_version();
                record.last_touched_at = SystemTime::now();
                true
            }
            None => false,
        }
    }

    /// Replace the loaded transcript for `sid`. Used by the `/resume`
    /// dialog to inject a different session's history into the current
    /// session so the engine sees it on the next `execute()` call.
    ///
    /// Replacement is refused while a replay handoff is active. During that
    /// lease the engine owns rows that may be the only recovery copy, so changing
    /// incarnation or clearing the lease would make stale completion drop them.
    /// The caller may retry after the handoff finalizes or is restored.
    pub fn replace_transcript_entries(&self, sid: &str, entries: Vec<TranscriptEntry>) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) if record.active_replay_handoff.is_none() => {
                record.last_transcript_uuid = entries.last().map(|entry| entry.uuid.clone());
                record.loaded_transcript = entries;
                record.loaded_transcript_complete = true;
                record.transcript_incarnation = next_transcript_source_version();
                record.transcript_revision = next_transcript_source_version();
                record.last_touched_at = SystemTime::now();
                true
            }
            Some(_) | None => false,
        }
    }

    /// Update the live session title.
    pub fn set_session_title(&self, sid: &str, title: String) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.title = Some(title);
                true
            }
            None => false,
        }
    }

    /// Remove a session from the in-memory map, returning the evicted
    /// record (if any). Used by the `/resume` flow to force
    /// [`load_session`](Self::load_session) to re-read from disk so
    /// stale in-memory transcripts don't shadow on-disk updates.
    ///
    /// A record with an active replay handoff is not evictable: the engine owns
    /// moved rows under that lease, and removing the record would either orphan
    /// the lease token or let a recreated incarnation discard the only recovery
    /// copy when the stale owner completes.
    /// Deliberately keeps this state's active lock: eviction drops the *cached
    /// record*, and every caller of it (a rewind that wants the page to re-read
    /// disk, the transactional resume that reloads immediately after) still
    /// owns the session across the gap. Giving the lock up here would open a
    /// window for another process to claim a session we are about to write.
    /// Ending ownership is [`Self::close_session`].
    pub fn evict_session(&self, sid: &str) -> Option<SessionRecord> {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        if sessions
            .get(sid)
            .is_some_and(|record| record.active_replay_handoff.is_some())
        {
            return None;
        }
        sessions.remove(sid)
    }

    /// Give the session up entirely: drop the cached record *and* release the
    /// active lock, so another process can open it.
    ///
    /// Refuses (and keeps everything) while a replay handoff is outstanding,
    /// for the same reason [`Self::evict_session`] does. Returns whether the
    /// session was closed.
    pub fn close_session(&self, sid: &str) -> bool {
        {
            let sessions = self.sessions.lock().expect("session map mutex poisoned");
            if sessions
                .get(sid)
                .is_some_and(|record| record.active_replay_handoff.is_some())
            {
                return false;
            }
        }
        let evicted = self.evict_session(sid);
        // Release after the record is gone. The reverse order would leave a
        // window where the lock is free while this process still answers for
        // the session out of its cache.
        let released = self.take_session_lock(sid);
        if let Some(lock) = released {
            lock.release();
        }
        evicted.is_some()
    }

    /// Put back a record removed by a failed transactional resume. A record
    /// created while the resume performed unlocked I/O always wins: replacing it
    /// could orphan an active replay handoff or discard undurable recovery rows.
    pub fn restore_session(&self, record: SessionRecord) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.entry(record.id.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(record);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    /// Restore a known session when an external runtime record exists but no
    /// transcript has been written yet. Existing in-memory state always wins.
    pub fn restore_empty_session(
        &self,
        sid: String,
        cwd: String,
        mcp_servers: Vec<McpServerConfig>,
        permission_mode: &str,
    ) -> SessionRecord {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        if let Some(existing) = sessions.get(&sid) {
            return existing.clone();
        }
        let record = SessionRecord {
            id: sid.clone(),
            cwd,
            permission_mode: permission_mode.to_string(),
            mode: None,
            slash_commands: Arc::new(Vec::new()),
            created_at: SystemTime::now(),
            messages: Vec::new(),
            mcp_servers,
            loaded_transcript: Vec::new(),
            transcript_incarnation: next_transcript_source_version(),
            transcript_revision: next_transcript_source_version(),
            loaded_transcript_complete: true,
            last_transcript_uuid: None,
            active_replay_handoff: None,
            last_touched_at: SystemTime::now(),
            title: None,
            attachment_state: SessionAttachmentState::default(),
            host_permission_policy: Arc::default(),
        };
        sessions.insert(sid, record.clone());
        record
    }

    /// Replace the slash-command list for `sid`.
    pub fn set_slash_commands(&self, sid: &str, commands: Vec<SlashCommand>) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.slash_commands = Arc::new(commands);
                true
            }
            None => false,
        }
    }

    /// Update the session permission mode for `sid` and run the
    /// plan-mode transition flag updates.
    ///
    /// Transition semantics: entering plan mode clears the
    /// pending `plan_mode_exit` attachment, leaving plan mode flips
    /// both `needs_plan_mode_exit_attachment` and
    /// `has_exited_plan_mode`. Kept on `ServerState` (not a free
    /// function) so the flag writes happen under the same mutex that
    /// guards `permission_mode` — no lost updates across concurrent
    /// cycle + prompt-executor reads.
    pub fn set_permission_mode(&self, sid: &str, mode: &str) -> bool {
        let applied = {
            let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
            match sessions.get_mut(sid) {
                Some(record) => {
                    let from = record.permission_mode.clone();
                    let to = mode.to_string();
                    record.permission_mode = to.clone();
                    apply_plan_mode_transition_flags(&mut record.attachment_state, &from, &to);
                    Some(record.attachment_state.plan_entered_from.clone())
                }
                None => None,
            }
        };
        // Outside the session lock, so the two mutexes are never held at
        // once and the cell stays a leaf. Only a mode that actually landed
        // on a record is mirrored: a write to an unknown session changed
        // nothing to shadow.
        match applied {
            Some(plan_entered_from) => {
                self.mirror_permission_mode(sid, mode);
                self.publish_permission_mode(sid, mode, plan_entered_from.as_deref());
                true
            }
            None => false,
        }
    }

    /// The mode `sid` entered plan mode from, while it is in plan mode.
    pub fn plan_entered_from(&self, sid: &str) -> Option<String> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(sid)
            .filter(|record| record.permission_mode == "plan")
            .and_then(|record| record.attachment_state.plan_entered_from.clone())
    }

    /// Put back where a rebuilt session entered plan mode from.
    ///
    /// A host that rebuilds its session every turn (a background job) brings
    /// the record back in plan mode by *setting* it, which reads as entering
    /// plan from `default`; the mode it really came from is something the
    /// host kept. Ignored unless the record is in plan mode, and never
    /// published — it restores what was already published.
    pub fn restore_plan_entered_from(&self, sid: &str, plan_entered_from: Option<&str>) {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        if let Some(record) = sessions
            .get_mut(sid)
            .filter(|record| record.permission_mode == "plan")
        {
            record.attachment_state.plan_entered_from = plan_entered_from.map(str::to_string);
        }
    }

    /// Update the collaboration mode stored on the session record.
    pub fn set_session_mode(&self, sid: &str, mode: &str) -> bool {
        if !matches!(mode, "coordinator" | "normal") {
            return false;
        }
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.mode = Some(mode.to_string());
                true
            }
            None => false,
        }
    }

    /// Record the emitted `plan_mode` attachment (full or sparse) in
    /// the session's counter + iteration marker. The injector calls
    /// this immediately after it yields a plan_mode user message.
    pub fn record_plan_mode_attachment(&self, sid: &str, iteration: u64) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.plan_mode_attachment_count = record
                    .attachment_state
                    .plan_mode_attachment_count
                    .saturating_add(1);
                record.attachment_state.last_plan_mode_iteration = Some(iteration);
                true
            }
            None => false,
        }
    }

    /// Clear the one-shot `needs_plan_mode_exit_attachment` flag
    /// and any pending plan text after the injector yields the exit
    /// message.
    pub fn clear_plan_mode_exit_flag(&self, sid: &str) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.needs_plan_mode_exit_attachment = false;
                record.attachment_state.pending_exit_plan_text = None;
                record.attachment_state.pending_exit_mode_note = None;
                true
            }
            None => false,
        }
    }

    /// Clear the `has_exited_plan_mode` flag after a `plan_mode_reentry`
    /// attachment is yielded.
    pub fn clear_plan_mode_exited_flag(&self, sid: &str) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.has_exited_plan_mode = false;
                true
            }
            None => false,
        }
    }

    /// Store the plan text for the `plan_mode_exit` attachment. Used
    /// when `clearContext` is false so the plan survives as regular
    /// text in the conversation.
    pub fn set_pending_exit_plan_text(&self, sid: &str, plan: String) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.pending_exit_plan_text = Some(plan);
                true
            }
            None => false,
        }
    }

    /// Store the note explaining which permission mode the session
    /// actually left plan mode in, when it is not the one
    /// `ExitPlanMode` asked for. Read once by the `plan_mode_exit`
    /// attachment and cleared with it.
    pub fn set_pending_exit_mode_note(&self, sid: &str, note: String) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.pending_exit_mode_note = Some(note);
                true
            }
            None => false,
        }
    }

    /// Store a plan for context reset. The next attachment poll will
    /// signal `TurnControlPlugin::run` to start a fresh controller drive with an
    /// "Implement the following plan" user message.
    pub fn set_pending_context_reset(&self, sid: &str, plan: String) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.pending_context_reset_plan = Some(plan);
                true
            }
            None => false,
        }
    }

    /// Take the pending context reset plan (if any). Returns `Some`
    /// once and clears the flag atomically.
    pub fn take_pending_context_reset(&self, sid: &str) -> Option<String> {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        sessions
            .get_mut(sid)
            .and_then(|r| r.attachment_state.pending_context_reset_plan.take())
    }

    /// Replace the last-emitted date (used by the `date_change`
    /// producer).
    pub fn set_last_emitted_date(&self, sid: &str, date: String) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.last_emitted_date = Some(date);
                true
            }
            None => false,
        }
    }

    /// Extend the set of announced skill names. Returns the list of
    /// names that were actually added (i.e. the delta). Used by the
    /// `skill_listing` producer to dedupe across polls.
    pub fn record_sent_skill_names(&self, sid: &str, names: &[String]) -> Vec<String> {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        let Some(record) = sessions.get_mut(sid) else {
            return Vec::new();
        };
        let mut added = Vec::new();
        for name in names {
            if !record.attachment_state.sent_skill_names.contains(name) {
                record.attachment_state.sent_skill_names.push(name.clone());
                added.push(name.clone());
            }
        }
        added
    }

    pub fn enqueue_runtime_prompt(&self, sid: &str, prompt: String) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.pending_runtime_prompts.push(prompt);
                true
            }
            None => false,
        }
    }

    pub fn consume_runtime_prompts(&self, sid: &str, count: usize) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                let count = count.min(record.attachment_state.pending_runtime_prompts.len());
                record
                    .attachment_state
                    .pending_runtime_prompts
                    .drain(..count);
                true
            }
            None => false,
        }
    }

    /// Overwrite a session's attachment state directly.
    pub fn set_attachment_state(&self, sid: &str, state: SessionAttachmentState) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state = state;
                true
            }
            None => false,
        }
    }

    /// Record that a task-management tool (TaskCreate/TaskUpdate) was
    /// invoked at the given iteration. The `task_reminder` attachment
    /// producer reads this to decide whether a nudge is needed.
    pub fn record_task_tool_iteration(&self, sid: &str, iteration: u64) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.last_task_tool_iteration = Some(iteration);
                true
            }
            None => false,
        }
    }

    /// Record that a `task_reminder` attachment was injected at the
    /// given iteration. Prevents back-to-back reminders within the
    /// throttle window.
    pub fn record_task_reminder_iteration(&self, sid: &str, iteration: u64) -> bool {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        match sessions.get_mut(sid) {
            Some(record) => {
                record.attachment_state.last_task_reminder_iteration = Some(iteration);
                true
            }
            None => false,
        }
    }

    /// Record a `session/cancel` notification for `sid`.
    ///
    /// - Unknown sessions are silently ignored.
    /// - Known sessions get their cancel count incremented so tests can
    ///   observe that the notification was processed. The real abort of
    ///   the session's in-flight prompt turn (and minting a fresh abort
    ///   handle for the next turn) happens at a higher layer.
    pub fn record_cancel(&self, sid: &str) {
        let exists = self
            .sessions
            .lock()
            .expect("session map mutex poisoned")
            .contains_key(sid);
        if !exists {
            return;
        }
        *self
            .cancel_counts
            .lock()
            .expect("cancel counts mutex poisoned")
            .entry(sid.to_string())
            .or_insert(0) += 1;
    }

    /// Number of `session/cancel` notifications that have been recorded
    /// for `sid`. Exposed for tests and diagnostics.
    pub fn cancel_count(&self, sid: &str) -> u64 {
        self.cancel_counts
            .lock()
            .expect("cancel counts mutex poisoned")
            .get(sid)
            .copied()
            .unwrap_or(0)
    }

    /// Look up a session record by id. Returned by value (cheap `Clone`) so
    /// callers don't hold the internal mutex across awaits.
    pub fn get_session(&self, id: &str) -> Option<SessionRecord> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(id)
            .cloned()
    }

    /// Narrow snapshot used by the between-iteration attachment poller.
    pub fn attachment_session_snapshot(&self, id: &str) -> Option<AttachmentSessionSnapshot> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(id)
            .map(|record| AttachmentSessionSnapshot {
                permission_mode: record.permission_mode.clone(),
                attachment_state: record.attachment_state.clone(),
            })
    }

    /// Current permission mode of a session, without cloning the whole
    /// record. Cheap enough for per-tool-call mode providers.
    pub fn session_permission_mode(&self, id: &str) -> Option<String> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(id)
            .map(|record| record.permission_mode.clone())
    }

    /// Number of currently tracked sessions. Primarily for tests.
    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .len()
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the effective cwd for a `session/new` request.
///
/// Cwd resolution prefers the request value, then the configured fallback,
/// then the current process directory. An empty string falls back; any
/// non-empty string (including whitespace-only input) wins unchanged.
///
/// Kept as a free function (rather than a method on `ServerState`) so the
/// dispatcher can call it without holding the state mutex and so it's easy
/// to unit-test the resolution order in isolation.
pub fn resolve_session_cwd(
    params_cwd: &str,
    fallback_cwd: Option<&str>,
) -> Result<String, JsonRpcError> {
    if !params_cwd.is_empty() {
        return Ok(params_cwd.to_string());
    }
    if let Some(fb) = fallback_cwd {
        if !fb.is_empty() {
            return Ok(fb.to_string());
        }
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| JsonRpcError::internal_error(format!("failed to resolve default cwd: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_proto::types::error_code;

    #[test]
    fn mark_initialized_rejects_second_call() {
        let s = ServerState::new();
        assert!(!s.is_initialized());
        s.mark_initialized().unwrap();
        assert!(s.is_initialized());
        let err = s.mark_initialized().unwrap_err();
        assert_eq!(err.code, error_code::INVALID_REQUEST);
        assert!(err.message.contains("Already initialized"));
    }

    #[test]
    fn require_initialized_errors_before_initialize() {
        let s = ServerState::new();
        let err = s.require_initialized().unwrap_err();
        assert_eq!(err.code, error_code::INVALID_REQUEST);
        assert!(err.message.contains("Not initialized"));
        s.mark_initialized().unwrap();
        s.require_initialized().unwrap();
    }

    #[test]
    fn create_session_mints_distinct_ids_and_stores_records() {
        let s = ServerState::new();
        let a = s.create_session("/tmp/a".into(), Vec::new());
        let b = s.create_session("/tmp/b".into(), Vec::new());
        assert_ne!(a.id, b.id, "session ids must be unique");
        assert_eq!(s.session_count(), 2);
        assert_eq!(s.get_session(&a.id).unwrap().cwd, "/tmp/a");
        assert_eq!(s.get_session(&b.id).unwrap().cwd, "/tmp/b");
        assert_eq!(s.get_session(&a.id).unwrap().permission_mode, "default");
        assert!(s.get_session(&a.id).unwrap().slash_commands.is_empty());
        assert!(
            s.get_session(&a.id).unwrap().messages.is_empty(),
            "new sessions must start with an empty message log"
        );
    }

    #[test]
    fn create_session_mints_distinct_ids_across_server_states() {
        let a = ServerState::new().create_session("/tmp/a".into(), Vec::new());
        let b = ServerState::new().create_session("/tmp/b".into(), Vec::new());

        assert_ne!(a.id, b.id, "session ids must be process-wide unique");
    }

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::Text(rebon_proto::types::TextContent {
            text: s.to_string(),
            annotations: None,
        })
    }

    #[test]
    fn begin_prompt_rejects_unknown_session() {
        let s = ServerState::new();
        let err = s.begin_prompt("sess-missing").unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(
            err.message.contains("Session not found"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[test]
    fn begin_prompt_rejects_double_slot_until_end() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        s.begin_prompt(&rec.id).unwrap();
        assert!(s.is_prompt_active(&rec.id));
        let err = s.begin_prompt(&rec.id).unwrap_err();
        assert_eq!(err.code, error_code::INVALID_REQUEST);
        assert!(err.message.contains("active prompt turn"));
        s.end_prompt(&rec.id);
        assert!(!s.is_prompt_active(&rec.id));
        // After release, a fresh slot can be acquired.
        s.begin_prompt(&rec.id).unwrap();
        s.end_prompt(&rec.id);
    }

    #[test]
    fn prompt_message_snapshot_replaces_prior_turn_and_can_be_released() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        assert!(s.append_prompt_messages(&rec.id, vec![text_block("first")]));
        assert!(s.append_prompt_messages(&rec.id, vec![text_block("second"), text_block("third")],));
        let stored = s.get_session(&rec.id).unwrap().messages;
        assert_eq!(stored.len(), 2);
        match &stored[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "second"),
            other => panic!("expected text block, got {other:?}"),
        }
        assert!(s.append_prompt_messages(&rec.id, vec![text_block(&"x".repeat(1_000_000))]));
        assert_eq!(s.get_session(&rec.id).unwrap().messages.len(), 1);
        assert!(s.clear_prompt_messages(&rec.id));
        let released = s.get_session(&rec.id).unwrap().messages;
        assert!(released.is_empty());
        assert_eq!(released.capacity(), 0);
    }

    #[test]
    fn append_prompt_messages_returns_false_for_unknown_session() {
        let s = ServerState::new();
        assert!(!s.append_prompt_messages("sess-missing", vec![text_block("x")]));
    }

    #[test]
    fn set_session_title_updates_known_session() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        assert!(s.set_session_title(&rec.id, "Fix title flow".into()));
        let stored = s.get_session(&rec.id).unwrap();
        assert_eq!(stored.title.as_deref(), Some("Fix title flow"));
    }

    #[test]
    fn set_session_title_returns_false_for_unknown_session() {
        let s = ServerState::new();
        assert!(!s.set_session_title("sess-missing", "Title".into()));
    }

    #[test]
    fn set_session_metadata_updates_known_session() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        assert!(s.set_permission_mode(&rec.id, "plan"));
        assert!(s.set_slash_commands(
            &rec.id,
            vec![SlashCommand {
                name: "review".into(),
                description: "Review code".into(),
                input: None,
                category: None,
                aliases: Vec::new(),
            }],
        ));
        let stored = s.get_session(&rec.id).unwrap();
        assert_eq!(stored.permission_mode, "plan");
        assert_eq!(stored.slash_commands.len(), 1);
        assert_eq!(stored.slash_commands[0].name, "review");
    }

    /// The permission gate reads a cell, not the record, so a mode that
    /// only reached the record was never enforced — which is how a
    /// tool-driven `EnterPlanMode` left the session planning in whatever
    /// mode it started in, and its `ExitPlanMode` skipped the approval.
    #[test]
    fn a_registered_cell_follows_every_write_to_the_record() {
        use rebon_permissions::types::PermissionMode;

        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        let cell = Arc::new(Mutex::new(PermissionMode::Auto));
        s.attach_permission_mode_cell(&rec.id, Arc::clone(&cell));

        assert!(s.set_permission_mode(&rec.id, "plan"));
        assert_eq!(*cell.lock().unwrap(), PermissionMode::Plan);
        assert_eq!(s.get_session(&rec.id).unwrap().permission_mode, "plan");

        // And back out again, so the cell tracks rather than latches.
        assert!(s.set_permission_mode(&rec.id, "default"));
        assert_eq!(*cell.lock().unwrap(), PermissionMode::Default);
    }

    /// A host that rebuilds the session from its own record has to hear a
    /// tool-driven move, or the next turn is built in the mode the session
    /// left. The publisher hears it after the cell, so whatever it reads
    /// back already agrees.
    #[test]
    fn a_registered_publisher_hears_every_mode_the_record_takes() {
        use rebon_permissions::types::PermissionMode;

        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        let cell = Arc::new(Mutex::new(PermissionMode::Default));
        s.attach_permission_mode_cell(&rec.id, Arc::clone(&cell));
        let heard = Arc::new(Mutex::new(Vec::<(String, PermissionMode)>::new()));
        let publisher_heard = Arc::clone(&heard);
        let publisher_cell = Arc::clone(&cell);
        s.attach_permission_mode_publisher(
            &rec.id,
            PermissionModePublisher::new(move |mode, _| {
                let in_cell = *publisher_cell.lock().unwrap();
                publisher_heard
                    .lock()
                    .unwrap()
                    .push((mode.to_string(), in_cell));
            }),
        );

        assert!(s.set_permission_mode(&rec.id, "plan"));
        assert!(s.set_permission_mode(&rec.id, "auto"));
        assert!(!s.set_permission_mode("sess-missing", "default"));

        assert_eq!(
            *heard.lock().unwrap(),
            vec![
                ("plan".to_string(), PermissionMode::Plan),
                ("auto".to_string(), PermissionMode::Auto),
            ]
        );
    }

    /// Another session's publisher stays out of it, and the last one
    /// registered for a session is the one that hears.
    #[test]
    fn a_publisher_hears_only_its_own_session_and_the_latest_wins() {
        let s = ServerState::new();
        let first = s.create_session("/tmp/w".into(), Vec::new());
        let second = s.create_session("/tmp/w".into(), Vec::new());
        let heard = Arc::new(Mutex::new(Vec::<String>::new()));
        let stale = Arc::clone(&heard);
        s.attach_permission_mode_publisher(
            &first.id,
            PermissionModePublisher::new(move |mode, _| {
                stale.lock().unwrap().push(format!("stale:{mode}"));
            }),
        );
        let live = Arc::clone(&heard);
        s.attach_permission_mode_publisher(
            &first.id,
            PermissionModePublisher::new(move |mode, _| {
                live.lock().unwrap().push(format!("live:{mode}"));
            }),
        );

        assert!(s.set_permission_mode(&second.id, "plan"));
        assert!(s.set_permission_mode(&first.id, "acceptEdits"));

        assert_eq!(*heard.lock().unwrap(), vec!["live:acceptEdits".to_string()]);
    }

    /// The record remembers the mode plan was entered from for as long as
    /// the session stays in plan, and forgets it on the way out.
    #[test]
    fn the_record_remembers_where_plan_was_entered_from() {
        let s = ServerState::new();
        let rec = s.create_session_with_permission_mode("/tmp/w".into(), Vec::new(), "auto");
        assert_eq!(s.plan_entered_from(&rec.id), None);

        assert!(s.set_permission_mode(&rec.id, "plan"));
        assert_eq!(s.plan_entered_from(&rec.id).as_deref(), Some("auto"));
        // Setting plan again is not entering it again.
        assert!(s.set_permission_mode(&rec.id, "plan"));
        assert_eq!(s.plan_entered_from(&rec.id).as_deref(), Some("auto"));

        assert!(s.set_permission_mode(&rec.id, "acceptEdits"));
        assert_eq!(s.plan_entered_from(&rec.id), None);
        assert!(s.set_permission_mode(&rec.id, "plan"));
        assert_eq!(s.plan_entered_from(&rec.id).as_deref(), Some("acceptEdits"));
        assert_eq!(s.plan_entered_from("sess-missing"), None);
    }

    /// The publisher hears the origin with the mode, so a host that keeps
    /// the mode can keep where plan came from beside it.
    #[test]
    fn the_publisher_hears_where_plan_was_entered_from() {
        let s = ServerState::new();
        let rec = s.create_session_with_permission_mode("/tmp/w".into(), Vec::new(), "auto");
        let heard = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let publisher_heard = Arc::clone(&heard);
        s.attach_permission_mode_publisher(
            &rec.id,
            PermissionModePublisher::new(move |mode, origin| {
                publisher_heard
                    .lock()
                    .unwrap()
                    .push((mode.to_string(), origin.map(str::to_string)));
            }),
        );

        assert!(s.set_permission_mode(&rec.id, "plan"));
        assert!(s.set_permission_mode(&rec.id, "default"));

        assert_eq!(
            *heard.lock().unwrap(),
            vec![
                ("plan".to_string(), Some("auto".to_string())),
                ("default".to_string(), None),
            ]
        );
    }

    /// A rebuilt record comes back in plan by being set there, which reads
    /// as entering it from `default`; the host restores the real origin. A
    /// record that is not in plan has no origin to restore.
    #[test]
    fn a_restored_origin_applies_only_in_plan() {
        let s = ServerState::new();
        let planning = s.create_session("/tmp/w".into(), Vec::new());
        assert!(s.set_permission_mode(&planning.id, "plan"));
        assert_eq!(
            s.plan_entered_from(&planning.id).as_deref(),
            Some("default")
        );

        s.restore_plan_entered_from(&planning.id, Some("auto"));
        assert_eq!(s.plan_entered_from(&planning.id).as_deref(), Some("auto"));
        s.restore_plan_entered_from(&planning.id, None);
        assert_eq!(s.plan_entered_from(&planning.id), None);

        let editing = s.create_session_with_permission_mode("/tmp/w".into(), Vec::new(), "auto");
        s.restore_plan_entered_from(&editing.id, Some("bypassPermissions"));
        assert_eq!(
            s.get_session(&editing.id)
                .unwrap()
                .attachment_state
                .plan_entered_from,
            None
        );
    }

    /// The gate's source reads the mode from the cell when it has one, and
    /// the origin from the record either way.
    #[test]
    fn the_mode_source_reads_the_cell_and_the_records_origin() {
        use rebon_permissions::denial_sink::PermissionModeProvider;
        use rebon_permissions::types::PermissionMode;

        let s = Arc::new(ServerState::new());
        let rec = s.create_session_with_permission_mode("/tmp/w".into(), Vec::new(), "auto");
        let cell = Arc::new(Mutex::new(PermissionMode::Auto));
        s.attach_permission_mode_cell(&rec.id, Arc::clone(&cell));
        let from_cell = SessionPermissionModeSource::cell(Arc::clone(&s), rec.id.clone(), cell);
        let from_record = SessionPermissionModeSource::record(Arc::clone(&s), rec.id.clone());

        assert!(s.set_permission_mode(&rec.id, "plan"));
        for source in [&from_cell, &from_record] {
            assert_eq!(source.current_mode(), PermissionMode::Plan);
            assert_eq!(source.plan_entered_from(), Some(PermissionMode::Auto));
        }

        let unknown = SessionPermissionModeSource::record(Arc::clone(&s), "sess-missing");
        assert_eq!(unknown.current_mode(), PermissionMode::Default);
        assert_eq!(unknown.plan_entered_from(), None);
    }

    /// A write that landed on no record has nothing to shadow: mirroring it
    /// would enforce a mode the session does not have.
    #[test]
    fn an_unknown_session_does_not_move_another_sessions_cell() {
        use rebon_permissions::types::PermissionMode;

        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        let cell = Arc::new(Mutex::new(PermissionMode::Auto));
        s.attach_permission_mode_cell(&rec.id, Arc::clone(&cell));

        assert!(!s.set_permission_mode("sess-missing", "plan"));
        assert_eq!(*cell.lock().unwrap(), PermissionMode::Auto);
    }

    #[test]
    fn set_session_metadata_returns_false_for_unknown_session() {
        let s = ServerState::new();
        assert!(!s.set_permission_mode("sess-missing", "plan"));
        assert!(!s.set_slash_commands("sess-missing", Vec::new()));
    }

    // ── attachment state flag transitions ─────────────────────────

    #[test]
    fn apply_plan_mode_transition_flags_leaving_plan_sets_exit_and_exited() {
        let mut state = SessionAttachmentState::default();
        apply_plan_mode_transition_flags(&mut state, "plan", "default");
        assert!(state.needs_plan_mode_exit_attachment);
        assert!(state.has_exited_plan_mode);
        assert_eq!(state.plan_mode_attachment_count, 0);
    }

    #[test]
    fn apply_plan_mode_transition_flags_entering_plan_clears_pending_exit() {
        let mut state = SessionAttachmentState {
            needs_plan_mode_exit_attachment: true,
            ..Default::default()
        };
        apply_plan_mode_transition_flags(&mut state, "default", "plan");
        assert!(!state.needs_plan_mode_exit_attachment);
        // Entering plan mode leaves has_exited_plan_mode untouched.
        assert!(!state.has_exited_plan_mode);
    }

    #[test]
    fn apply_plan_mode_transition_flags_is_noop_when_same_mode() {
        let mut state = SessionAttachmentState {
            needs_plan_mode_exit_attachment: true,
            has_exited_plan_mode: true,
            plan_mode_attachment_count: 3,
            last_plan_mode_iteration: Some(2),
            ..Default::default()
        };
        let before = state.clone();
        apply_plan_mode_transition_flags(&mut state, "plan", "plan");
        assert_eq!(state, before);
    }

    #[test]
    fn apply_plan_mode_transition_flags_resets_throttle_on_exit() {
        let mut state = SessionAttachmentState {
            plan_mode_attachment_count: 7,
            last_plan_mode_iteration: Some(5),
            ..Default::default()
        };
        apply_plan_mode_transition_flags(&mut state, "plan", "default");
        assert_eq!(state.plan_mode_attachment_count, 0);
        assert_eq!(state.last_plan_mode_iteration, None);
    }

    #[test]
    fn set_permission_mode_updates_session_and_attachment_state() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        // default → plan: no side effects, flag stays clear.
        assert!(s.set_permission_mode(&rec.id, "plan"));
        let after_enter = s.get_session(&rec.id).unwrap();
        assert_eq!(after_enter.permission_mode, "plan");
        assert!(!after_enter.attachment_state.needs_plan_mode_exit_attachment);

        // plan → default: exit flag + exited flag both set.
        assert!(s.set_permission_mode(&rec.id, "default"));
        let after_exit = s.get_session(&rec.id).unwrap();
        assert_eq!(after_exit.permission_mode, "default");
        assert!(after_exit.attachment_state.needs_plan_mode_exit_attachment);
        assert!(after_exit.attachment_state.has_exited_plan_mode);
    }

    #[test]
    fn record_plan_mode_attachment_bumps_counter_and_iteration() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        assert!(s.record_plan_mode_attachment(&rec.id, 3));
        assert!(s.record_plan_mode_attachment(&rec.id, 7));
        let after = s.get_session(&rec.id).unwrap();
        assert_eq!(after.attachment_state.plan_mode_attachment_count, 2);
        assert_eq!(after.attachment_state.last_plan_mode_iteration, Some(7));
    }

    #[test]
    fn runtime_prompt_prefix_consumption_preserves_concurrent_appends() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/w".into(), Vec::new());
        assert!(state.enqueue_runtime_prompt(&session.id, "first".into()));
        let observed = state
            .get_session(&session.id)
            .unwrap()
            .attachment_state
            .pending_runtime_prompts
            .len();
        assert!(state.enqueue_runtime_prompt(&session.id, "second".into()));

        assert!(state.consume_runtime_prompts(&session.id, observed));

        assert_eq!(
            state
                .get_session(&session.id)
                .unwrap()
                .attachment_state
                .pending_runtime_prompts,
            vec!["second".to_string()]
        );
    }

    #[test]
    fn clear_plan_mode_exit_flag_is_idempotent() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        s.set_permission_mode(&rec.id, "plan");
        s.set_permission_mode(&rec.id, "default");
        assert!(s.clear_plan_mode_exit_flag(&rec.id));
        let first = s.get_session(&rec.id).unwrap();
        assert!(!first.attachment_state.needs_plan_mode_exit_attachment);
        // second call still returns true (session exists) but is a
        // no-op.
        assert!(s.clear_plan_mode_exit_flag(&rec.id));
        let second = s.get_session(&rec.id).unwrap();
        assert!(!second.attachment_state.needs_plan_mode_exit_attachment);
    }

    #[test]
    fn record_sent_skill_names_returns_delta_only() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        let first =
            s.record_sent_skill_names(&rec.id, &["commit".to_string(), "review".to_string()]);
        assert_eq!(first, vec!["commit".to_string(), "review".to_string()]);
        let second = s.record_sent_skill_names(
            &rec.id,
            &[
                "commit".to_string(),
                "review".to_string(),
                "lint".to_string(),
            ],
        );
        assert_eq!(second, vec!["lint".to_string()]);
        let stored = s.get_session(&rec.id).unwrap();
        assert_eq!(
            stored.attachment_state.sent_skill_names,
            vec![
                "commit".to_string(),
                "review".to_string(),
                "lint".to_string(),
            ]
        );
    }

    #[test]
    fn set_last_emitted_date_updates_session_state() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        assert!(s.set_last_emitted_date(&rec.id, "2026-04-10".into()));
        let after = s.get_session(&rec.id).unwrap();
        assert_eq!(
            after.attachment_state.last_emitted_date.as_deref(),
            Some("2026-04-10")
        );
    }

    #[test]
    fn record_cancel_ignores_unknown_session() {
        let s = ServerState::new();
        s.record_cancel("sess-missing");
        assert_eq!(s.cancel_count("sess-missing"), 0);
    }

    #[test]
    fn record_cancel_increments_known_session() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/w".into(), Vec::new());
        s.record_cancel(&rec.id);
        s.record_cancel(&rec.id);
        assert_eq!(s.cancel_count(&rec.id), 2);
    }

    #[test]
    fn resolve_session_cwd_uses_params_when_present() {
        let got = resolve_session_cwd("/explicit", Some("/fallback")).unwrap();
        assert_eq!(got, "/explicit");
    }

    #[test]
    fn resolve_session_cwd_falls_back_when_params_empty() {
        let got = resolve_session_cwd("", Some("/fallback")).unwrap();
        assert_eq!(got, "/fallback");
    }

    #[test]
    fn resolve_session_cwd_preserves_whitespace_only_params() {
        let got = resolve_session_cwd("   ", Some("/fallback")).unwrap();
        assert_eq!(got, "   ");
    }

    #[test]
    fn resolve_session_cwd_preserves_whitespace_only_fallback() {
        let got = resolve_session_cwd("", Some("   ")).unwrap();
        assert_eq!(got, "   ");
    }

    #[test]
    fn resolve_session_cwd_final_fallback_is_process_cwd() {
        // No params, no explicit fallback → uses std::env::current_dir().
        let got = resolve_session_cwd("", None).unwrap();
        let expected = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, expected);
    }

    // ---- load_session direct tests ----

    use rebon_session::session_storage::project_dir_component;

    fn fresh_load_session_tempdir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-acp-state-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn write_simple_transcript(
        projects_root: &std::path::Path,
        cwd: &str,
        sid: &str,
        entries: &[(&str, Option<&str>, &str, &str)],
    ) {
        let pdir = projects_root.join(project_dir_component(cwd));
        std::fs::create_dir_all(&pdir).unwrap();
        let mut body = String::new();
        for (u, p, t, ts) in entries {
            let parent = match p {
                Some(pp) => format!("\"{pp}\""),
                None => "null".into(),
            };
            body.push_str(&format!(
                "{{\"type\":\"{t}\",\"uuid\":\"{u}\",\"parentUuid\":{parent},\"timestamp\":\"{ts}\",\"message\":null}}\n"
            ));
        }
        std::fs::write(pdir.join(format!("{sid}.jsonl")), body).unwrap();
    }

    #[test]
    fn load_session_missing_transcript_returns_session_not_found() {
        let tmp = fresh_load_session_tempdir("missing");
        let s = ServerState::new();
        let err = s
            .load_session(tmp.path(), "sess-bogus", "/tmp/nope", None, Vec::new())
            .unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(err.message.contains("Session not found"));
        assert_eq!(s.session_count(), 0);
    }

    // ---- session ownership ----

    fn owning_state(projects_root: &std::path::Path) -> ServerState {
        let state = ServerState::new();
        assert!(state.enable_session_ownership(projects_root.to_path_buf()));
        state
    }

    fn seed_transcript(projects_root: &std::path::Path, cwd: &str, sid: &str) {
        write_simple_transcript(
            projects_root,
            cwd,
            sid,
            &[("u1", None, "user", "2026-04-10T00:00:00.000Z")],
        );
    }

    /// The default state must not touch the lock: the terminal still takes it
    /// itself, and a state that claimed it would fight its own caller through
    /// the in-process lock registry.
    #[test]
    fn ownership_is_off_until_enabled() {
        let tmp = fresh_load_session_tempdir("own-off");
        let state = ServerState::new();
        let record = state.create_session("/work/off".into(), Vec::new());
        assert!(!state.owns_session(&record.id));
        assert!(!rebon_session::session_storage::is_session_active(
            tmp.path(),
            "/work/off",
            &record.id
        ));
    }

    #[test]
    fn owned_sessions_lists_only_the_sessions_this_state_holds() {
        let tmp = fresh_load_session_tempdir("own-list");
        let state = owning_state(tmp.path());
        let owned = state.create_session("/work/owned".into(), Vec::new());
        let closed = state.create_session("/work/closed".into(), Vec::new());
        assert!(state.close_session(&closed.id));
        state.restore_empty_session(
            "sess-read".into(),
            "/work/read".into(),
            Vec::new(),
            "default",
        );

        assert_eq!(
            state.owned_sessions(),
            vec![(owned.id.clone(), "/work/owned".to_string())]
        );
        assert!(ServerState::new().owned_sessions().is_empty());
    }

    #[test]
    fn an_owning_state_claims_the_session_it_mints() {
        let tmp = fresh_load_session_tempdir("own-new");
        let state = owning_state(tmp.path());
        let record = state.create_session("/work/new".into(), Vec::new());
        assert!(state.owns_session(&record.id));
        assert!(rebon_session::session_storage::is_session_active(
            tmp.path(),
            "/work/new",
            &record.id
        ));
    }

    /// The id is random and the transcript is not written until the first
    /// turn, so the session records when it was made as it is named.
    #[test]
    fn a_new_session_records_when_it_was_created() {
        let tmp = fresh_load_session_tempdir("own-created-at");
        let state = owning_state(tmp.path());
        let record = state.create_session("/work/dated".into(), Vec::new());

        let recorded = rebon_session::session_storage::load_session_created_at_ms(
            tmp.path(),
            "/work/dated",
            &record.id,
        )
        .expect("a new session dates itself");
        let in_memory = record
            .created_at
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after the epoch")
            .as_millis() as u64;
        assert!(
            recorded.abs_diff(in_memory) < 5_000,
            "recorded {recorded} is nowhere near the record's own {in_memory}"
        );
    }

    /// The double-writer this whole change exists to stop: a second host
    /// opening a session someone else is writing gets a refusal, not a
    /// transcript.
    #[test]
    fn loading_a_session_another_owner_holds_is_refused() {
        let tmp = fresh_load_session_tempdir("own-conflict");
        let cwd = "/work/conflict";
        seed_transcript(tmp.path(), cwd, "sess-held");
        let first = owning_state(tmp.path());
        first
            .load_session(tmp.path(), "sess-held", cwd, None, Vec::new())
            .expect("the first owner loads it");

        let second = owning_state(tmp.path());
        let err = second
            .load_session(tmp.path(), "sess-held", cwd, None, Vec::new())
            .expect_err("the second owner is refused");

        assert_eq!(err.code, error_code::SESSION_OWNED_ELSEWHERE);
        assert_eq!(err.data.as_ref().unwrap()["owner"]["known"], false);
        assert_eq!(
            err.data.as_ref().unwrap()["owner"]["sessionId"],
            "sess-held"
        );
        assert!(!second.owns_session("sess-held"));
        assert_eq!(second.session_count(), 0);
    }

    /// A state with ownership off is the compatibility case, not a second
    /// writer waiting to happen: it must still refuse nothing and claim
    /// nothing, because its caller owns the lock.
    #[test]
    fn a_state_without_ownership_still_loads_a_held_session() {
        let tmp = fresh_load_session_tempdir("own-passthrough");
        let cwd = "/work/passthrough";
        seed_transcript(tmp.path(), cwd, "sess-passthrough");
        let owner = owning_state(tmp.path());
        owner
            .load_session(tmp.path(), "sess-passthrough", cwd, None, Vec::new())
            .unwrap();

        let unowned = ServerState::new();
        unowned
            .load_session(tmp.path(), "sess-passthrough", cwd, None, Vec::new())
            .expect("ownership off means the state does not arbitrate");
    }

    /// Eviction drops the cache, not the claim. Every caller of it reloads
    /// immediately afterwards, and losing the lock in that gap would let
    /// someone else claim a session we are about to write.
    #[test]
    fn evicting_a_session_keeps_its_lock() {
        let tmp = fresh_load_session_tempdir("own-evict");
        let cwd = "/work/evict";
        seed_transcript(tmp.path(), cwd, "sess-evicted");
        let state = owning_state(tmp.path());
        state
            .load_session(tmp.path(), "sess-evicted", cwd, None, Vec::new())
            .unwrap();

        assert!(state.evict_session("sess-evicted").is_some());

        assert!(state.owns_session("sess-evicted"));
        assert!(rebon_session::session_storage::is_session_active(
            tmp.path(),
            cwd,
            "sess-evicted"
        ));
    }

    #[test]
    fn closing_a_session_releases_it_to_the_next_owner() {
        let tmp = fresh_load_session_tempdir("own-close");
        let cwd = "/work/close";
        seed_transcript(tmp.path(), cwd, "sess-closed");
        let first = owning_state(tmp.path());
        first
            .load_session(tmp.path(), "sess-closed", cwd, None, Vec::new())
            .unwrap();

        assert!(first.close_session("sess-closed"));

        assert!(!first.owns_session("sess-closed"));
        assert!(!rebon_session::session_storage::is_session_active(
            tmp.path(),
            cwd,
            "sess-closed"
        ));
        let second = owning_state(tmp.path());
        second
            .load_session(tmp.path(), "sess-closed", cwd, None, Vec::new())
            .expect("the released session opens elsewhere");
    }

    /// Dropping the state is the crash-equivalent path: no explicit close ran,
    /// and the session must still come free.
    #[test]
    fn dropping_the_state_releases_every_lock_it_held() {
        let tmp = fresh_load_session_tempdir("own-drop");
        let sid = {
            let state = owning_state(tmp.path());
            let record = state.create_session("/work/drop".into(), Vec::new());
            assert!(rebon_session::session_storage::is_session_active(
                tmp.path(),
                "/work/drop",
                &record.id
            ));
            record.id
        };
        assert!(!rebon_session::session_storage::is_session_active(
            tmp.path(),
            "/work/drop",
            &sid
        ));
    }

    #[test]
    fn adopting_a_caller_held_lock_makes_the_state_the_custodian() {
        let tmp = fresh_load_session_tempdir("own-adopt");
        let cwd = "/work/adopt";
        seed_transcript(tmp.path(), cwd, "sess-adopted");
        let lock = rebon_session::session_storage::try_acquire_session_active_lock(
            tmp.path(),
            cwd,
            "sess-adopted",
        )
        .unwrap()
        .expect("the caller acquires first, as a relocation does");

        let state = owning_state(tmp.path());
        assert!(state.adopt_session_lock("sess-adopted", lock).is_none());
        assert!(state.owns_session("sess-adopted"));
        // The claim path must read the adopted lock as "already ours" rather
        // than as another owner, or every relocation would refuse itself.
        state
            .load_session(tmp.path(), "sess-adopted", cwd, None, Vec::new())
            .expect("the adopted session loads");

        assert!(state.close_session("sess-adopted"));
        assert!(!rebon_session::session_storage::is_session_active(
            tmp.path(),
            cwd,
            "sess-adopted"
        ));
    }

    /// A `session/load` for an id that is not a session must leave nothing
    /// behind. Claiming before the transcript is proven would strand a lock
    /// (and its project directory) on every mistyped or stale id.
    #[test]
    fn a_failed_load_claims_nothing() {
        let tmp = fresh_load_session_tempdir("own-missing");
        let state = owning_state(tmp.path());

        let err = state
            .load_session(
                tmp.path(),
                "sess-not-a-session",
                "/work/missing",
                None,
                Vec::new(),
            )
            .unwrap_err();

        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(!state.owns_session("sess-not-a-session"));
        assert!(!rebon_session::session_storage::is_session_active(
            tmp.path(),
            "/work/missing",
            "sess-not-a-session"
        ));
    }

    // ---- idle transcript sweep ----

    fn seed_loaded_session(tag: &str, sid: &str) -> (tempfile::TempDir, ServerState, String) {
        let tmp = fresh_load_session_tempdir(tag);
        let cwd = format!("/work/{tag}");
        write_simple_transcript(
            tmp.path(),
            &cwd,
            sid,
            &[
                ("u1", None, "user", "2026-04-10T00:00:00.000Z"),
                ("a1", Some("u1"), "assistant", "2026-04-10T00:00:01.000Z"),
            ],
        );
        let state = ServerState::new();
        let loaded = state
            .load_session(tmp.path(), sid, &cwd, None, Vec::new())
            .unwrap();
        assert_eq!(loaded.loaded_transcript.len(), 2);
        (tmp, state, cwd)
    }

    /// Releasing an idle resident copy must be invisible: it duplicates disk,
    /// and the next load reads the same chain back.
    #[test]
    fn sweep_releases_idle_transcript_and_load_restores_it() {
        let (tmp, state, cwd) = seed_loaded_session("sweep-idle", "sess-idle");

        let swept = state.sweep_idle_transcripts(tmp.path(), Duration::ZERO);

        assert_eq!(swept.sessions, 1);
        assert_eq!(swept.entries, 2);
        assert!(!swept.is_empty());
        assert!(state
            .get_session("sess-idle")
            .unwrap()
            .loaded_transcript
            .is_empty());

        let reloaded = state
            .load_session(tmp.path(), "sess-idle", &cwd, None, Vec::new())
            .unwrap();
        assert_eq!(reloaded.loaded_transcript.len(), 2);
        assert_eq!(reloaded.loaded_transcript[0].uuid, "u1");
        assert_eq!(reloaded.loaded_transcript[1].uuid, "a1");
    }

    /// A session used inside the idle window keeps its resident copy — the
    /// sweep collects abandoned sessions, it is not a cache eviction policy.
    #[test]
    fn sweep_keeps_recently_touched_transcript() {
        let (tmp, state, _cwd) = seed_loaded_session("sweep-fresh", "sess-fresh");

        let swept = state.sweep_idle_transcripts(tmp.path(), Duration::from_secs(3600));

        assert!(swept.is_empty());
        assert_eq!(
            state
                .get_session("sess-fresh")
                .unwrap()
                .loaded_transcript
                .len(),
            2
        );
    }

    /// A turn in flight owns the session's history even at zero idle time.
    #[test]
    fn sweep_skips_session_with_active_prompt() {
        let (tmp, state, _cwd) = seed_loaded_session("sweep-busy", "sess-busy");
        let _slot = state.begin_prompt_snapshot("sess-busy").unwrap();

        let swept = state.sweep_idle_transcripts(tmp.path(), Duration::ZERO);

        assert!(swept.is_empty());
        assert_eq!(
            state
                .get_session("sess-busy")
                .unwrap()
                .loaded_transcript
                .len(),
            2
        );
    }

    /// Once residency is released the vector is an unproven append suffix, not
    /// a duplicate of disk. Dropping it would lose those rows.
    #[test]
    fn sweep_skips_pending_append_suffix() {
        let (tmp, state, _cwd) = seed_loaded_session("sweep-suffix", "sess-suffix");
        assert!(state.release_transcript_residency("sess-suffix"));
        assert!(state.push_transcript_entries("sess-suffix", vec![make_entry("user", "u2")]));

        let swept = state.sweep_idle_transcripts(tmp.path(), Duration::ZERO);

        assert!(swept.is_empty());
        assert_eq!(
            state
                .get_session("sess-suffix")
                .unwrap()
                .loaded_transcript
                .len(),
            1
        );
    }

    /// A session with no transcript file has no second copy to fall back on.
    #[test]
    fn sweep_skips_session_whose_transcript_file_is_missing() {
        let tmp = fresh_load_session_tempdir("sweep-nodisk");
        let state = ServerState::new();
        let record = state.create_session("/work/unsaved".into(), Vec::new());
        assert!(state.push_transcript_entries(&record.id, vec![make_entry("user", "u1")]));

        let swept = state.sweep_idle_transcripts(tmp.path(), Duration::ZERO);

        assert!(swept.is_empty());
        assert_eq!(
            state
                .get_session(&record.id)
                .unwrap()
                .loaded_transcript
                .len(),
            1
        );
    }

    #[test]
    fn host_permission_policy_lifetime_and_failures_follow_session_record() {
        let state = ServerState::new();
        state.restore_empty_session("policy".into(), "/workspace".into(), Vec::new(), "default");
        let policy = state
            .session_permission_policy("policy", "/workspace", || Ok(7u32))
            .unwrap();
        let weak = Arc::downgrade(&policy);
        drop(policy);
        assert!(weak.upgrade().is_some());
        assert_eq!(
            *state
                .session_permission_policy("policy", "/workspace", || Ok(8u32))
                .unwrap(),
            7
        );
        assert!(state
            .session_permission_policy("policy", "/wrong", || Ok(9u32))
            .is_err());
        assert!(state
            .session_permission_policy("policy", "/workspace", || Ok(String::new()))
            .is_err());
        assert!(state.close_session("policy"));
        assert!(weak.upgrade().is_none());
        assert!(state
            .session_permission_policy("policy", "/workspace", || Ok(9u32))
            .is_err());
        state.restore_empty_session("policy".into(), "/workspace".into(), Vec::new(), "default");
        assert!(state
            .session_permission_policy::<u32>("policy", "/workspace", || Err(
                "invalid policy".into()
            ))
            .is_err());
        assert!(state
            .session_permission_policy("policy", "/workspace", || Ok(9u32))
            .is_err());
    }

    #[test]
    fn host_permission_policy_poisoned_session_state_fails_closed() {
        let state = Arc::new(ServerState::new());
        state.restore_empty_session("policy".into(), "/workspace".into(), Vec::new(), "default");
        state
            .session_permission_policy("policy", "/workspace", || Ok(7u32))
            .unwrap();
        let worker = state.clone();
        assert!(std::thread::spawn(move || {
            let _guard = worker.sessions.lock().unwrap();
            panic!("poison state");
        })
        .join()
        .is_err());
        assert!(state
            .session_permission_policy("policy", "/workspace", || Ok(8u32))
            .is_err());
    }

    #[test]
    fn restore_empty_session_registers_known_external_session() {
        let state = ServerState::new();

        let record = state.restore_empty_session(
            "sess-external".into(),
            "/tmp/mobile".into(),
            Vec::new(),
            "auto",
        );

        assert_eq!(record.id, "sess-external");
        assert_eq!(record.cwd, "/tmp/mobile");
        assert_eq!(record.permission_mode, "auto");
        assert!(record.loaded_transcript.is_empty());
        assert_eq!(
            state.get_session("sess-external").unwrap().cwd,
            "/tmp/mobile"
        );
    }

    #[test]
    fn restore_empty_session_does_not_replace_existing_state() {
        let state = ServerState::new();
        let original = state.restore_empty_session(
            "sess-external".into(),
            "/tmp/original".into(),
            Vec::new(),
            "plan",
        );

        let restored = state.restore_empty_session(
            "sess-external".into(),
            "/tmp/replacement".into(),
            Vec::new(),
            "auto",
        );

        assert_eq!(restored.cwd, original.cwd);
        assert_eq!(restored.permission_mode, original.permission_mode);
        assert_eq!(state.session_count(), 1);
    }

    /// A transcript file that exists and is empty is a session nobody has
    /// spoken in yet — which is exactly what `/hosted` (and `rebon --hosted`)
    /// hands to a worker before the first turn. Reporting it as missing made
    /// the worker fail the job a second after starting, and the "not found"
    /// message sent the user looking for a session that was right there.
    #[test]
    fn load_session_opens_a_session_whose_transcript_is_still_empty() {
        let tmp = fresh_load_session_tempdir("empty-transcript");
        let cwd = "/repo";
        let sid = "sess-never-spoken";
        let path =
            rebon_session::session_storage::ensure_session_file_path(tmp.path(), cwd, sid).unwrap();
        std::fs::write(&path, b"").unwrap();

        let record = ServerState::new()
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap();

        assert_eq!(record.id, sid);
        assert_eq!(record.cwd, cwd);
        assert!(record.loaded_transcript.is_empty());
    }

    /// The other `None` from `reconstruct_chain` still means what it always
    /// meant: rows that exist and cannot be chained are a broken transcript,
    /// not an empty one.
    #[test]
    fn load_session_still_refuses_a_transcript_whose_rows_form_no_chain() {
        let tmp = fresh_load_session_tempdir("unchainable-transcript");
        let cwd = "/repo";
        let sid = "sess-unchainable";
        let path =
            rebon_session::session_storage::ensure_session_file_path(tmp.path(), cwd, sid).unwrap();
        // Rows that parse but carry no user/assistant leaf: a transcript with
        // content and no conversation in it.
        std::fs::write(
            &path,
            "{\"type\":\"system\",\"uuid\":\"s1\",\"parentUuid\":null,\"timestamp\":\"2025-02-02T00:00:00Z\",\"content\":\"note\"}\n",
        )
        .unwrap();

        let err = ServerState::new()
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap_err();

        assert!(err.message.contains("Session not found"));
    }

    #[test]
    fn load_session_preserves_unique_cross_cwd_transcript_storage() {
        let tmp = fresh_load_session_tempdir("cross-cwd");
        let source_cwd = "/repo/.rebon/worktrees/bg-1";
        let target_cwd = "/repo";
        let sid = "sess-cross-cwd";
        let source =
            rebon_session::session_storage::ensure_session_file_path(tmp.path(), source_cwd, sid)
                .unwrap();
        std::fs::write(
            source,
            "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"timestamp\":\"2025-02-02T00:00:00Z\",\"message\":null}\n",
        )
        .unwrap();
        rebon_session::session_storage::save_session_title(
            tmp.path(),
            source_cwd,
            sid,
            "Worktree session",
        )
        .unwrap();

        let state = ServerState::new();
        let record = state
            .load_session(tmp.path(), sid, target_cwd, None, Vec::new())
            .unwrap();

        assert_eq!(record.cwd, source_cwd);
        assert_eq!(record.loaded_transcript.len(), 1);
        assert_eq!(record.title.as_deref(), Some("Worktree session"));
        assert!(!transcript_file_path(tmp.path(), target_cwd, sid).exists());
        assert!(transcript_file_path(tmp.path(), source_cwd, sid).is_file());
    }

    #[cfg(windows)]
    #[test]
    fn load_session_keeps_existing_storage_cwd_for_windows_equivalent_path() {
        let tmp = fresh_load_session_tempdir("windows-equivalent-cwd");
        let source_cwd = r"F:\Dev\Sandbox\Rebon";
        let requested_cwd = "f:/dev/sandbox/rebon";
        let sid = "sess-windows-equivalent-cwd";
        let source =
            rebon_session::session_storage::ensure_session_file_path(tmp.path(), source_cwd, sid)
                .unwrap();
        std::fs::write(
            source,
            "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"timestamp\":\"2025-02-02T00:00:00Z\",\"message\":null}\n",
        )
        .unwrap();

        let record = ServerState::new()
            .load_session(tmp.path(), sid, requested_cwd, None, Vec::new())
            .unwrap();

        assert_eq!(record.cwd, source_cwd);
        assert_eq!(
            rebon_session::session_storage::read_project_cwd_sidecar(&project_dir_path(
                tmp.path(),
                source_cwd
            ))
            .as_deref(),
            Some(source_cwd)
        );
    }

    #[test]
    fn load_session_rejects_ambiguous_cross_cwd_transcripts() {
        let tmp = fresh_load_session_tempdir("ambiguous-cross-cwd");
        let sid = "sess-ambiguous-cross-cwd";
        for cwd in ["/repo/worktree-a", "/repo/worktree-b"] {
            let path =
                rebon_session::session_storage::ensure_session_file_path(tmp.path(), cwd, sid)
                    .unwrap();
            std::fs::write(path, b"{}\n").unwrap();
        }

        let state = ServerState::new();
        let error = state
            .load_session(tmp.path(), sid, "/repo", None, Vec::new())
            .unwrap_err();

        assert_eq!(error.code, error_code::INVALID_PARAMS);
        assert!(!transcript_file_path(tmp.path(), "/repo", sid).exists());
    }

    #[test]
    fn load_session_restores_chain_and_inserts_into_state() {
        let tmp = fresh_load_session_tempdir("happy");
        let cwd = "/tmp/work";
        let sid = "sess-load-happy";
        write_simple_transcript(
            tmp.path(),
            cwd,
            sid,
            &[
                ("u1", None, "user", "2025-02-02T00:00:00Z"),
                ("a1", Some("u1"), "assistant", "2025-02-02T00:00:01Z"),
            ],
        );

        let s = ServerState::new();
        let rec = s
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap();
        assert_eq!(rec.id, sid);
        assert_eq!(rec.cwd, cwd);
        assert_eq!(rec.loaded_transcript.len(), 2);
        assert_eq!(rec.loaded_transcript[0].uuid, "u1");
        assert_eq!(rec.loaded_transcript[1].uuid, "a1");
        assert!(rec.messages.is_empty());

        // The freshly-loaded session is now known to begin_prompt.
        s.begin_prompt(sid).unwrap();
        assert!(s.is_prompt_active(sid));
        s.end_prompt(sid);
    }

    #[test]
    fn load_session_restores_title_from_sidecar() {
        let tmp = fresh_load_session_tempdir("title");
        let cwd = "/tmp/work";
        let sid = "sess-load-title";
        write_simple_transcript(
            tmp.path(),
            cwd,
            sid,
            &[("u1", None, "user", "2025-02-02T00:00:00Z")],
        );
        rebon_session::session_storage::save_session_title(
            tmp.path(),
            cwd,
            sid,
            "Existing conversation",
        )
        .unwrap();

        let state = ServerState::new();
        let record = state
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap();

        assert_eq!(record.title.as_deref(), Some("Existing conversation"));
        assert_eq!(
            state.get_session(sid).unwrap().title.as_deref(),
            Some("Existing conversation")
        );
    }

    #[test]
    fn load_session_reuses_existing_in_memory_record_without_changing_storage_cwd() {
        let s = ServerState::new();
        let rec = s.create_session("/tmp/original".into(), Vec::new());
        let sid = rec.id.clone();

        // Reuse — no transcript file exists, but the in-memory
        // short-circuit branch must not consult disk.
        let tmp = fresh_load_session_tempdir("cached");
        let reloaded = s
            .load_session(tmp.path(), &sid, "/tmp/updated", None, Vec::new())
            .expect("existing in-memory session must load");
        assert_eq!(reloaded.id, sid);
        assert_eq!(reloaded.cwd, "/tmp/original");
        assert_eq!(s.get_session(&sid).unwrap().cwd, "/tmp/original");
        assert_eq!(s.session_count(), 1);
    }

    #[test]
    fn load_session_second_call_returns_record_without_re_reading_disk() {
        // Idempotency: after the first successful load, deleting the
        // fixture file underneath us should be invisible to a second
        // load call — the in-memory short-circuit takes over.
        let tmp = fresh_load_session_tempdir("idempotent");
        let cwd = "/tmp/work";
        let sid = "sess-idempotent";
        write_simple_transcript(
            tmp.path(),
            cwd,
            sid,
            &[("u1", None, "user", "2025-03-03T00:00:00Z")],
        );

        let s = ServerState::new();
        let first = s
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap();
        assert_eq!(first.loaded_transcript.len(), 1);

        // Yank the transcript file. A second load that goes to disk
        // would return Session not found; the in-memory branch must
        // succeed instead.
        let pdir = tmp.path().join(project_dir_component(cwd));
        std::fs::remove_file(pdir.join(format!("{sid}.jsonl"))).unwrap();

        let second = s
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap();
        assert_eq!(second.id, sid);
        assert_eq!(second.cwd, cwd);
        assert_eq!(second.loaded_transcript.len(), 1);
        assert_eq!(s.session_count(), 1);
    }

    #[test]
    fn load_session_supports_prompt_and_cancel_after_restore() {
        // Once a session is loaded, it must integrate with the rest of
        // the session-state machinery: begin_prompt / end_prompt slot,
        // append_prompt_messages on the live record, and record_cancel.
        // This is the regression guard against future refactors that
        // might forget to wire loaded sessions into the same maps.
        let tmp = fresh_load_session_tempdir("interop");
        let cwd = "/tmp/work";
        let sid = "sess-interop";
        write_simple_transcript(
            tmp.path(),
            cwd,
            sid,
            &[
                ("u1", None, "user", "2025-04-04T00:00:00Z"),
                ("a1", Some("u1"), "assistant", "2025-04-04T00:00:01Z"),
            ],
        );

        let s = ServerState::new();
        let _ = s
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap();

        // begin_prompt acquires the active-prompt slot.
        let snap = s.begin_prompt(sid).unwrap();
        assert_eq!(snap.id, sid);
        assert_eq!(snap.loaded_transcript.len(), 2);
        assert!(s.is_prompt_active(sid));

        // append_prompt_messages still mutates the live record without
        // touching loaded_transcript.
        s.append_prompt_messages(sid, vec![text_block("hello")]);
        let live = s.get_session(sid).unwrap();
        assert_eq!(live.messages.len(), 1);
        assert_eq!(live.loaded_transcript.len(), 2);

        // end_prompt clears the slot.
        s.end_prompt(sid);
        assert!(!s.is_prompt_active(sid));

        // record_cancel works against the loaded session id, and cancelling
        // only counts for sessions the state already knows about.
        s.record_cancel(sid);
        s.record_cancel(sid);
        assert_eq!(s.cancel_count(sid), 2);
    }

    // ---- list_sessions state-level tests ----

    fn write_empty_jsonl(
        projects_root: &std::path::Path,
        cwd: &str,
        sid: &str,
    ) -> std::path::PathBuf {
        let pdir = projects_root.join(project_dir_component(cwd));
        std::fs::create_dir_all(&pdir).unwrap();
        let p = pdir.join(format!("{sid}.jsonl"));
        std::fs::write(&p, b"").unwrap();
        p
    }

    fn list_ids(sessions: &[SessionRecord]) -> std::collections::HashSet<String> {
        sessions.iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn list_sessions_empty_state_and_empty_disk_returns_empty() {
        let tmp = fresh_load_session_tempdir("list-empty");
        let s = ServerState::new();
        let got = s
            .list_sessions(tmp.path(), Some("/tmp/nowhere"), None)
            .expect("list_sessions must not error on missing dir");
        assert!(got.is_empty());
    }

    #[test]
    fn list_sessions_in_memory_only_omitted_cwd_returns_everything() {
        // Omitted cwd (None) must return every in-memory session
        // regardless of each session's own cwd — the snapshot is
        // returned unfiltered when no cwd was supplied.
        let tmp = fresh_load_session_tempdir("list-in-mem-omit");
        let s = ServerState::new();
        let a = s.create_session("/tmp/a".into(), Vec::new());
        let b = s.create_session("/tmp/b".into(), Vec::new());
        let c = s.create_session("/tmp/c".into(), Vec::new());

        let got = s.list_sessions(tmp.path(), None, None).unwrap();
        let ids = list_ids(&got);
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&a.id));
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
    }

    #[test]
    fn list_sessions_explicit_cwd_filters_in_memory() {
        // Explicit cwd filter keeps only matching in-memory sessions.
        let tmp = fresh_load_session_tempdir("list-filter");
        let s = ServerState::new();
        let a = s.create_session("/tmp/a".into(), Vec::new());
        let _b = s.create_session("/tmp/b".into(), Vec::new());

        let got = s.list_sessions(tmp.path(), Some("/tmp/a"), None).unwrap();
        let ids = list_ids(&got);
        assert_eq!(ids, std::iter::once(a.id).collect());
    }

    #[test]
    fn list_sessions_disk_only_creates_placeholders() {
        let tmp = fresh_load_session_tempdir("list-disk-only");
        let cwd = "/tmp/disk";
        write_empty_jsonl(tmp.path(), cwd, "sess-disk-1");
        write_empty_jsonl(tmp.path(), cwd, "sess-disk-2");

        let s = ServerState::new();
        let got = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        let ids = list_ids(&got);
        assert!(ids.contains("sess-disk-1"));
        assert!(ids.contains("sess-disk-2"));
        // Each placeholder carries the effective cwd unchanged.
        for rec in &got {
            assert_eq!(rec.cwd, cwd);
            assert!(rec.messages.is_empty());
            assert!(rec.loaded_transcript.is_empty());
            assert!(rec.title.is_none());
        }
        // list_sessions is read-only against the state map: the
        // placeholders must NOT be inserted into ServerState.sessions.
        assert_eq!(s.session_count(), 0);
    }

    #[test]
    fn list_sessions_merges_in_memory_and_disk() {
        let tmp = fresh_load_session_tempdir("list-merge");
        let cwd = "/tmp/shared";
        let s = ServerState::new();
        let mem = s.create_session(cwd.into(), Vec::new());
        write_empty_jsonl(tmp.path(), cwd, "disk-only");

        let got = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        let ids = list_ids(&got);
        assert!(ids.contains(&mem.id));
        assert!(ids.contains("disk-only"));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn list_sessions_in_memory_wins_on_id_collision() {
        let tmp = fresh_load_session_tempdir("list-collision");
        let cwd = "/tmp/collide";
        let s = ServerState::new();
        // Prime an in-memory session so we know its id.
        let mem = s.create_session(cwd.into(), Vec::new());
        // Drop a jsonl with the SAME stem on disk. The in-memory
        // record (with its live cwd + empty-transcript markers) must
        // win; the disk placeholder must be discarded.
        write_empty_jsonl(tmp.path(), cwd, &mem.id);

        let got = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        assert_eq!(got.len(), 1, "duplicate id must collapse to one entry");
        assert_eq!(got[0].id, mem.id);
        // The kept record is the in-memory one — identified by the
        // matching `created_at` (the placeholder would use mtime).
        assert_eq!(got[0].created_at, mem.created_at);
    }

    #[test]
    fn list_sessions_ignores_non_jsonl_files() {
        let tmp = fresh_load_session_tempdir("list-non-jsonl");
        let cwd = "/tmp/mixed";
        let pdir = tmp.path().join(project_dir_component(cwd));
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("keep.jsonl"), b"").unwrap();
        std::fs::write(pdir.join("ignore.txt"), b"not a session").unwrap();
        std::fs::write(pdir.join("ignore.json"), b"{}").unwrap();
        std::fs::write(pdir.join("sess.jsonl.bak"), b"").unwrap();
        // A bare `.jsonl` file with no stem should also be skipped.
        std::fs::write(pdir.join(".jsonl"), b"").unwrap();

        let s = ServerState::new();
        let got = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        let ids = list_ids(&got);
        assert!(ids.contains("keep"));
        assert!(!ids.contains("ignore"));
        assert!(!ids.contains("ignore.json"));
        assert!(!ids.contains("sess.jsonl"));
    }

    #[test]
    fn list_sessions_missing_project_dir_degrades_to_empty() {
        let tmp = fresh_load_session_tempdir("list-missing");
        // No project dir created at all — read_dir returns NotFound.
        let s = ServerState::new();
        let got = s
            .list_sessions(tmp.path(), Some("/tmp/ghost"), None)
            .expect("read_dir failures must degrade, not propagate");
        assert!(got.is_empty());
    }

    #[test]
    fn list_sessions_missing_project_dir_still_returns_in_memory() {
        // Disk-empty does NOT clobber in-memory sessions — the
        // scanner is strictly additive.
        let tmp = fresh_load_session_tempdir("list-missing-memin");
        let s = ServerState::new();
        let mem = s.create_session("/tmp/live".into(), Vec::new());
        let got = s.list_sessions(tmp.path(), None, None).unwrap();
        let ids = list_ids(&got);
        assert!(ids.contains(&mem.id));
    }

    #[test]
    fn list_sessions_blank_cwd_does_not_activate_filter() {
        // A whitespace-only cwd still counts as set, so it does activate the
        // explicit-cwd filter and does NOT fall back.
        let tmp = fresh_load_session_tempdir("list-blank");
        let s = ServerState::new();
        let _a = s.create_session("/tmp/a".into(), Vec::new());
        let _b = s.create_session("/tmp/b".into(), Vec::new());
        // Write a disk-only session under the whitespace cwd, plus one under
        // the fallback to prove fallback is not consulted.
        write_empty_jsonl(tmp.path(), "   ", "sess-under-blank");
        write_empty_jsonl(tmp.path(), "/tmp/fallback", "sess-under-fallback");

        let got = s
            .list_sessions(tmp.path(), Some("   "), Some("/tmp/fallback"))
            .unwrap();
        let ids = list_ids(&got);
        assert_eq!(
            ids,
            std::iter::once("sess-under-blank".to_string()).collect()
        );
    }

    #[test]
    fn list_sessions_explicit_cwd_omits_in_memory_at_other_cwds() {
        let tmp = fresh_load_session_tempdir("list-explicit-omit");
        let s = ServerState::new();
        let _unrelated = s.create_session("/tmp/other".into(), Vec::new());
        let wanted = s.create_session("/tmp/want".into(), Vec::new());
        let got = s
            .list_sessions(tmp.path(), Some("/tmp/want"), None)
            .unwrap();
        let ids = list_ids(&got);
        assert_eq!(ids, std::iter::once(wanted.id).collect());
    }

    #[test]
    fn list_sessions_disk_placeholder_uses_mtime_as_created_at() {
        let tmp = fresh_load_session_tempdir("list-mtime");
        let cwd = "/tmp/stamped";
        let path = write_empty_jsonl(tmp.path(), cwd, "timed");
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        let s = ServerState::new();
        let got = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        let rec = got.iter().find(|r| r.id == "timed").unwrap();
        assert_eq!(rec.created_at, mtime);
    }

    #[test]
    fn list_sessions_non_uuid_stems_still_listed() {
        // The scanner does NOT validate UUID shape — any non-empty
        // `.jsonl` stem becomes a session id, deliberately, so ids of
        // any shape written to disk remain listable.
        let tmp = fresh_load_session_tempdir("list-nonuuid");
        let cwd = "/tmp/weird";
        write_empty_jsonl(tmp.path(), cwd, "plain-string");
        write_empty_jsonl(tmp.path(), cwd, "123");
        write_empty_jsonl(tmp.path(), cwd, "not-a-uuid-at-all");

        let s = ServerState::new();
        let got = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        let ids = list_ids(&got);
        assert!(ids.contains("plain-string"));
        assert!(ids.contains("123"));
        assert!(ids.contains("not-a-uuid-at-all"));
    }

    #[test]
    fn list_sessions_sanitized_cwd_routing_matches_transcript_file_path() {
        // A cwd with special characters must route to the same
        // directory the writer would have used.
        let tmp = fresh_load_session_tempdir("list-sanitize");
        let cwd = "/Users/foo bar/My Project";
        write_empty_jsonl(tmp.path(), cwd, "sess-sanitized");

        let s = ServerState::new();
        let got = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        let ids = list_ids(&got);
        assert!(ids.contains("sess-sanitized"));
    }

    #[test]
    fn list_sessions_disk_only_with_omitted_cwd_scans_fallback_dir() {
        // With explicit fallback, an omitted cwd (None) scans
        // `sanitize(fallback)` — not a random default. Tests the
        // asymmetry: in-memory sessions are returned independent of
        // their cwd, but the disk scan is anchored to the fallback.
        let tmp = fresh_load_session_tempdir("list-fallback-dir");
        write_empty_jsonl(tmp.path(), "/tmp/fb", "fb-session");
        write_empty_jsonl(tmp.path(), "/tmp/other", "other-session");

        let s = ServerState::new();
        let got = s.list_sessions(tmp.path(), None, Some("/tmp/fb")).unwrap();
        let ids = list_ids(&got);
        // fb-session is under the fallback cwd's dir → listed.
        assert!(ids.contains("fb-session"));
        // other-session lives under a different cwd's dir → NOT listed.
        assert!(!ids.contains("other-session"));
    }

    #[test]
    fn list_sessions_preserves_session_count_after_call() {
        // list_sessions is read-only — it must not grow or shrink the
        // state map as a side effect.
        let tmp = fresh_load_session_tempdir("list-readonly");
        let cwd = "/tmp/readonly";
        write_empty_jsonl(tmp.path(), cwd, "disk-sess");

        let s = ServerState::new();
        s.create_session(cwd.into(), Vec::new());
        assert_eq!(s.session_count(), 1);
        let _ = s.list_sessions(tmp.path(), Some(cwd), None).unwrap();
        assert_eq!(s.session_count(), 1, "list_sessions must be read-only");
    }

    #[test]
    fn load_session_keeps_other_sessions_intact() {
        // A new session created via session/new must not be perturbed
        // by a subsequent successful load of a different session id —
        // the session map keys are independent.
        let tmp = fresh_load_session_tempdir("isolation");
        let cwd_a = "/tmp/a";
        let cwd_b = "/tmp/b";
        let sid_b = "sess-loaded-b";
        write_simple_transcript(
            tmp.path(),
            cwd_b,
            sid_b,
            &[("rb", None, "user", "2025-05-05T00:00:00Z")],
        );

        let s = ServerState::new();
        let in_mem = s.create_session(cwd_a.into(), Vec::new());
        let sid_a = in_mem.id.clone();

        s.load_session(tmp.path(), sid_b, cwd_b, None, Vec::new())
            .unwrap();

        assert_eq!(s.session_count(), 2);
        assert_eq!(s.get_session(&sid_a).unwrap().cwd, cwd_a);
        let loaded = s.get_session(sid_b).unwrap();
        assert_eq!(loaded.cwd, cwd_b);
        assert_eq!(loaded.loaded_transcript.len(), 1);
    }

    #[test]
    fn load_session_returns_err_for_io_error_distinct_from_not_found() {
        // A directory exists where the transcript file should be —
        // `std::fs::read` returns an error that is NOT `NotFound`.
        // The handler must surface this as `internal_error`, not as
        // `Session not found`, so client-side retries don't paper over
        // a real I/O fault.
        let tmp = fresh_load_session_tempdir("ioerror");
        let cwd = "/tmp/work";
        let sid = "sess-io";
        let pdir = tmp.path().join(project_dir_component(cwd));
        std::fs::create_dir_all(&pdir).unwrap();
        // Create a *directory* at the path where the loader expects
        // the transcript file. Reading it will fail with an OS error
        // distinct from NotFound on every supported platform.
        std::fs::create_dir_all(pdir.join(format!("{sid}.jsonl"))).unwrap();

        let s = ServerState::new();
        let err = s
            .load_session(tmp.path(), sid, cwd, None, Vec::new())
            .unwrap_err();
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert!(
            err.message.contains("failed to read transcript"),
            "unexpected error message: {}",
            err.message
        );
        assert_eq!(s.session_count(), 0);
    }

    // -------------------------------------------------------------------
    // push_transcript_entries tests
    // -------------------------------------------------------------------

    fn make_entry(entry_type: &str, uuid: &str) -> TranscriptEntry {
        TranscriptEntry {
            entry_type: entry_type.to_string(),
            uuid: uuid.to_string(),
            parent_uuid: None,
            timestamp: Some("2026-04-10T00:00:00.000Z".to_string()),
            raw: serde_json::json!({"type": entry_type, "uuid": uuid}),
        }
    }

    #[test]
    fn push_transcript_entries_to_existing_session() {
        let s = ServerState::new();
        let session = s.create_session("/tmp".into(), Vec::new());
        assert!(session.loaded_transcript.is_empty());

        let entries = vec![make_entry("user", "u-1"), make_entry("assistant", "a-1")];
        assert!(s.push_transcript_entries(&session.id, entries));

        let record = s.get_session(&session.id).unwrap();
        assert_eq!(record.loaded_transcript.len(), 2);
        assert_eq!(record.loaded_transcript[0].uuid, "u-1");
        assert_eq!(record.loaded_transcript[1].uuid, "a-1");
    }

    #[test]
    fn push_transcript_entries_to_nonexistent_session_returns_false() {
        let s = ServerState::new();
        let entries = vec![make_entry("user", "u-1")];
        assert!(!s.push_transcript_entries("no-such-session", entries));
    }

    #[test]
    fn push_transcript_entries_empty_vec_returns_true_if_session_exists() {
        let s = ServerState::new();
        let session = s.create_session("/tmp".into(), Vec::new());
        assert!(s.push_transcript_entries(&session.id, Vec::new()));
        // Loaded transcript should still be empty
        let record = s.get_session(&session.id).unwrap();
        assert!(record.loaded_transcript.is_empty());
    }

    #[test]
    fn push_transcript_entries_empty_vec_returns_false_if_no_session() {
        let s = ServerState::new();
        assert!(!s.push_transcript_entries("no-such", Vec::new()));
    }

    #[test]
    fn push_transcript_entries_accumulates_across_multiple_pushes() {
        let s = ServerState::new();
        let session = s.create_session("/tmp".into(), Vec::new());

        s.push_transcript_entries(
            &session.id,
            vec![make_entry("user", "u-1"), make_entry("assistant", "a-1")],
        );
        s.push_transcript_entries(
            &session.id,
            vec![make_entry("user", "u-2"), make_entry("assistant", "a-2")],
        );

        let record = s.get_session(&session.id).unwrap();
        assert_eq!(record.loaded_transcript.len(), 4);
        assert_eq!(record.loaded_transcript[0].uuid, "u-1");
        assert_eq!(record.loaded_transcript[1].uuid, "a-1");
        assert_eq!(record.loaded_transcript[2].uuid, "u-2");
        assert_eq!(record.loaded_transcript[3].uuid, "a-2");
    }

    #[test]
    fn push_transcript_entries_extends_loaded_session() {
        // Simulate a session loaded from disk (loaded_transcript already populated)
        let s = ServerState::new();
        let session = s.create_session("/tmp".into(), Vec::new());

        // Simulate loaded_transcript being populated (like session/load)
        s.push_transcript_entries(
            &session.id,
            vec![
                make_entry("user", "loaded-u-1"),
                make_entry("assistant", "loaded-a-1"),
            ],
        );

        // Now push new entries (like a new turn)
        s.push_transcript_entries(
            &session.id,
            vec![
                make_entry("user", "new-u-1"),
                make_entry("assistant", "new-a-1"),
            ],
        );

        let record = s.get_session(&session.id).unwrap();
        assert_eq!(record.loaded_transcript.len(), 4);
        assert_eq!(record.loaded_transcript[0].uuid, "loaded-u-1");
        assert_eq!(record.loaded_transcript[3].uuid, "new-a-1");
    }

    #[test]
    fn push_to_different_sessions_does_not_interfere() {
        let s = ServerState::new();
        let a = s.create_session("/a".into(), Vec::new());
        let b = s.create_session("/b".into(), Vec::new());

        s.push_transcript_entries(&a.id, vec![make_entry("user", "a-u-1")]);
        s.push_transcript_entries(&b.id, vec![make_entry("user", "b-u-1")]);

        let ra = s.get_session(&a.id).unwrap();
        let rb = s.get_session(&b.id).unwrap();
        assert_eq!(ra.loaded_transcript.len(), 1);
        assert_eq!(ra.loaded_transcript[0].uuid, "a-u-1");
        assert_eq!(rb.loaded_transcript.len(), 1);
        assert_eq!(rb.loaded_transcript[0].uuid, "b-u-1");
    }

    #[test]
    fn replace_transcript_entries_overwrites_existing_transcript() {
        let s = ServerState::new();
        let session = s.create_session("/tmp".into(), Vec::new());
        s.push_transcript_entries(
            &session.id,
            vec![make_entry("user", "u-1"), make_entry("assistant", "a-1")],
        );

        assert!(s.replace_transcript_entries(
            &session.id,
            vec![make_entry("user", "u-2"), make_entry("system", "s-1")],
        ));

        let record = s.get_session(&session.id).unwrap();
        assert_eq!(record.loaded_transcript.len(), 2);
        assert_eq!(record.loaded_transcript[0].uuid, "u-2");
        assert_eq!(record.loaded_transcript[1].uuid, "s-1");
    }

    #[test]
    fn replay_handoff_moves_full_history_then_retains_only_pending_suffix() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![make_entry("user", "u-1"), make_entry("assistant", "a-1")],
        );

        let first = state
            .take_transcript_for_replay(&session.id)
            .expect("first replay source");
        assert!(first.complete);
        assert_eq!(first.entries.len(), 2);
        assert_eq!(first.last_uuid.as_deref(), Some("a-1"));
        assert!(state
            .get_session(&session.id)
            .unwrap()
            .loaded_transcript
            .is_empty());

        let first_revision = first.revision;
        state
            .finalize_transcript_after_replay(first, Vec::new(), Some("a-1".into()))
            .expect("no-undurable replay finalizes its lease");
        state.push_transcript_entries(&session.id, vec![make_entry("user", "u-2")]);
        let second = state
            .take_transcript_for_replay(&session.id)
            .expect("pending replay source");
        assert!(!second.complete);
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.last_uuid.as_deref(), Some("a-1"));
        assert_ne!(first_revision, second.revision);
    }

    #[test]
    fn failed_replay_restore_preserves_moved_rows_and_newer_pending_suffix() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![make_entry("user", "u-1"), make_entry("assistant", "a-1")],
        );
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("moved replay source");

        state.push_transcript_entries(&session.id, vec![make_entry("user", "u-2")]);
        state
            .restore_transcript_after_failed_replay(source)
            .expect("failure restoration reconciles ownership");

        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("restored replay source");
        assert!(restored.complete);
        assert_eq!(
            restored
                .entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["u-1", "a-1", "u-2"]
        );
        assert_eq!(restored.last_uuid.as_deref(), Some("a-1"));
    }

    #[test]
    fn cached_replay_finalize_conflict_restores_all_rows_and_releases_lease() {
        let state = ServerState::new();
        let session = state.create_session("C:/replay/cache-conflict".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![make_entry("user", "D"), make_entry("assistant", "T")],
        );
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("capture cache handoff");
        state.push_transcript_entries(&session.id, vec![make_entry("user", "U")]);

        let outcome = state
            .finalize_cached_transcript_after_replay(source, Some("T".into()))
            .expect("conflict restores atomically");
        assert_eq!(outcome, ReplayFinalizeOutcome::RetryRequired);
        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("conflict ended the lease");
        assert!(restored.complete);
        assert_eq!(
            restored
                .entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["D", "T", "U"]
        );
        assert_eq!(restored.last_uuid.as_deref(), Some("T"));
    }

    #[test]
    fn cached_replay_finalize_without_concurrency_returns_cached_tail() {
        let state = ServerState::new();
        let session = state.create_session("C:/replay/cache-stable".into(), Vec::new());
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("capture stable cache handoff");
        let outcome = state
            .finalize_cached_transcript_after_replay(source, Some("T".into()))
            .expect("stable cache hit finalizes");
        assert_eq!(outcome, ReplayFinalizeOutcome::Finalized(Some("T".into())));
        assert_eq!(
            state.transcript_tail_uuid(&session.id).as_deref(),
            Some("T")
        );
        assert!(state.take_transcript_for_replay(&session.id).is_some());
    }

    #[test]
    fn rebuild_finalize_conflict_restores_source_and_concurrent_suffix_for_retry() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "durable")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased replay source");
        state.push_transcript_entries(&session.id, vec![make_entry("assistant", "newer")]);

        let outcome = state
            .finalize_transcript_after_replay(
                source,
                vec![make_entry("user", "undurable")],
                Some("durable".into()),
            )
            .expect("revision conflict restores atomically");
        assert_eq!(outcome, ReplayFinalizeOutcome::RetryRequired);

        let retained = state
            .take_transcript_for_replay(&session.id)
            .expect("conflict ended the lease for a whole-operation retry");
        assert!(retained.complete);
        assert_eq!(
            retained
                .entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["durable", "newer"]
        );
        assert_eq!(retained.last_uuid.as_deref(), Some("durable"));
    }

    #[test]
    fn failed_replay_restore_keeps_newest_duplicate_uuid() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        let mut old = make_entry("assistant", "same");
        old.raw["marker"] = serde_json::json!("moved");
        state.push_transcript_entries(&session.id, vec![old]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("moved replay source");
        let mut newer = make_entry("assistant", "same");
        newer.raw["marker"] = serde_json::json!("concurrent");
        state.push_transcript_entries(&session.id, vec![newer]);

        state
            .restore_transcript_after_failed_replay(source)
            .expect("restore reconciles duplicate UUIDs");
        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("restored replay source");
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].raw["marker"], "concurrent");
    }

    #[test]
    fn dropped_replay_source_restores_concurrent_suffix_with_last_write_wins() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/drop-replay".into(), Vec::new());
        let mut moved = make_entry("assistant", "same");
        moved.raw["marker"] = serde_json::json!("moved");
        state.push_transcript_entries(&session.id, vec![moved]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased replay source");
        let mut concurrent = make_entry("assistant", "same");
        concurrent.raw["marker"] = serde_json::json!("concurrent");
        state.push_transcript_entries(&session.id, vec![concurrent, make_entry("user", "suffix")]);

        drop(source);

        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("drop released and restored the lease");
        assert!(restored.complete);
        assert_eq!(restored.entries.len(), 2);
        assert_eq!(restored.entries[0].uuid, "same");
        assert_eq!(restored.entries[0].raw["marker"], "concurrent");
        assert_eq!(restored.entries[1].uuid, "suffix");
    }

    #[test]
    fn replay_source_drop_is_poison_safe_during_unwind() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/poisoned-drop".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "moved")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased replay source");
        state.push_transcript_entries(&session.id, vec![make_entry("assistant", "suffix")]);

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _sessions = state.sessions.lock().expect("unpoisoned fixture mutex");
            panic!("poison fixture mutex");
        }));
        assert!(poisoned.is_err());

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _source = source;
            panic!("caller unwind");
        }));
        assert!(
            unwound.is_err(),
            "drop must not replace or abort caller unwind"
        );

        let sessions = match state.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        };
        let record = sessions.get(&session.id).expect("session remains resident");
        assert_eq!(record.active_replay_handoff, None);
        assert!(record.loaded_transcript_complete);
        assert_eq!(
            record
                .loaded_transcript
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["moved", "suffix"]
        );
    }

    #[test]
    fn stale_replay_drop_cannot_disturb_current_owner() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/stale-drop".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "owned")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("current owner");
        let stale_sessions = source.restore_sessions.clone();
        let stale_identity = ReplayTranscriptSource {
            session_id: source.session_id.clone(),
            cwd: source.cwd.clone(),
            incarnation: source.incarnation.wrapping_add(1),
            revision: source.revision,
            complete: true,
            last_uuid: None,
            entries: vec![make_entry("user", "stale-identity")],
            handoff_id: source.handoff_id,
            restore_sessions: stale_sessions.clone(),
        };
        let stale_owner = ReplayTranscriptSource {
            session_id: source.session_id.clone(),
            cwd: source.cwd.clone(),
            incarnation: source.incarnation,
            revision: source.revision,
            complete: true,
            last_uuid: None,
            entries: vec![make_entry("user", "stale-owner")],
            handoff_id: source.handoff_id.wrapping_add(1),
            restore_sessions: stale_sessions,
        };

        drop(stale_identity);
        drop(stale_owner);
        {
            let sessions = state.sessions.lock().expect("session map mutex");
            let record = sessions.get(&session.id).expect("session remains");
            assert_eq!(record.active_replay_handoff, Some(source.handoff_id));
            assert!(record.loaded_transcript.is_empty());
        }

        drop(source);
        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("true owner restored the lease");
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].uuid, "owned");
    }

    #[test]
    fn explicit_replay_completion_rejects_wrong_state_without_stranding_origin() {
        enum CompletionPath {
            Restore,
            CachedFinalize,
            RebuildFinalize,
        }

        for path in [
            CompletionPath::Restore,
            CompletionPath::CachedFinalize,
            CompletionPath::RebuildFinalize,
        ] {
            let origin = ServerState::new();
            let wrong_state = ServerState::new();
            let session = origin.create_session("/tmp/wrong-state".into(), Vec::new());
            origin.push_transcript_entries(&session.id, vec![make_entry("user", "moved")]);
            let source = origin
                .take_transcript_for_replay(&session.id)
                .expect("origin leased replay source");
            origin
                .push_transcript_entries(&session.id, vec![make_entry("assistant", "concurrent")]);

            let error = match path {
                CompletionPath::Restore => wrong_state
                    .restore_transcript_after_failed_replay(source)
                    .expect_err("wrong state must reject restore"),
                CompletionPath::CachedFinalize => wrong_state
                    .finalize_cached_transcript_after_replay(source, Some("moved".into()))
                    .expect_err("wrong state must reject cached finalize"),
                CompletionPath::RebuildFinalize => wrong_state
                    .finalize_transcript_after_replay(
                        source,
                        vec![make_entry("assistant", "retained")],
                        Some("moved".into()),
                    )
                    .expect_err("wrong state must reject rebuild finalize"),
            };
            assert!(error.contains("different server state"));

            let restored = origin
                .take_transcript_for_replay(&session.id)
                .expect("rejected completion restored the origin lease");
            assert!(restored.complete);
            assert_eq!(
                restored
                    .entries
                    .iter()
                    .map(|entry| entry.uuid.as_str())
                    .collect::<Vec<_>>(),
                vec!["moved", "concurrent"]
            );
        }
    }

    #[test]
    fn replay_source_guard_is_non_owning_and_hidden_from_debug() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/weak-replay-guard".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "moved")]);
        let sessions = Arc::downgrade(&state.sessions);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased replay source");

        let debug = format!("{source:?}");
        assert!(!debug.contains("restore_sessions"));
        drop(state);
        assert!(sessions.upgrade().is_none());

        drop(source);
    }

    #[test]
    fn successful_replay_finalize_retains_only_final_duplicate_uuid_occurrence() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "durable")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased replay source");
        let mut older = make_entry("assistant", "same");
        older.raw["marker"] = serde_json::json!("older recovery");
        let mut newest = make_entry("assistant", "same");
        newest.raw["marker"] = serde_json::json!("newest recovery");

        let outcome = state
            .finalize_transcript_after_replay(source, vec![older, newest], Some("same".into()))
            .expect("successful replay finalizes");
        assert_eq!(
            outcome,
            ReplayFinalizeOutcome::Finalized(Some("same".into()))
        );
        let retained = state
            .take_transcript_for_replay(&session.id)
            .expect("retained replay source");
        assert_eq!(retained.entries.len(), 1);
        assert_eq!(retained.entries[0].raw["marker"], "newest recovery");
    }

    #[test]
    fn active_replay_handoff_cannot_be_evicted_before_successful_finalize() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "moved")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased replay source");

        assert!(
            state.evict_session(&session.id).is_none(),
            "the record that owns a replay lease must remain resident"
        );
        state.push_transcript_entries(&session.id, vec![make_entry("assistant", "newer")]);
        let outcome = state
            .finalize_transcript_after_replay(
                source,
                vec![make_entry("user", "undurable")],
                Some("moved".into()),
            )
            .expect("resident lease remains completable");
        assert_eq!(outcome, ReplayFinalizeOutcome::RetryRequired);

        let retained = state
            .take_transcript_for_replay(&session.id)
            .expect("conflicting completion releases the lease");
        assert!(retained.complete);
        assert_eq!(
            retained
                .entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["moved", "newer"]
        );
    }

    #[test]
    fn active_replay_handoff_cannot_be_evicted_before_failed_restore() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "moved")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased replay source");

        assert!(state.evict_session(&session.id).is_none());
        state.push_transcript_entries(&session.id, vec![make_entry("assistant", "newer")]);
        state
            .restore_transcript_after_failed_replay(source)
            .expect("resident lease remains restorable");

        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("failed completion releases the lease");
        assert!(restored.complete);
        assert_eq!(
            restored
                .entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["moved", "newer"]
        );
    }

    #[test]
    fn active_handoff_refuses_replacement_until_finalize() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "old")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased old source");

        assert!(
            !state.replace_transcript_entries(&session.id, vec![make_entry("user", "replacement")])
        );
        state
            .finalize_transcript_after_replay(
                source,
                vec![make_entry("assistant", "undurable")],
                Some("undurable".into()),
            )
            .expect("handoff finalizes without losing recovery rows");
        let retained = state
            .take_transcript_for_replay(&session.id)
            .expect("recovery remains available");
        assert_eq!(retained.entries[0].uuid, "undurable");
        state
            .restore_transcript_after_failed_replay(retained)
            .expect("release inspection handoff");

        assert!(
            state.replace_transcript_entries(&session.id, vec![make_entry("user", "replacement")])
        );
        let current = state.get_session(&session.id).expect("replacement remains");
        assert_eq!(current.loaded_transcript[0].uuid, "replacement");
    }

    #[test]
    fn active_handoff_refuses_replacement_until_restore() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "old")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased old source");

        assert!(
            !state.replace_transcript_entries(&session.id, vec![make_entry("user", "replacement")])
        );
        state
            .restore_transcript_after_failed_replay(source)
            .expect("failed replay restores ownership");
        assert!(
            state.replace_transcript_entries(&session.id, vec![make_entry("user", "replacement")])
        );
    }

    #[test]
    fn stale_completion_cannot_overwrite_new_incarnation() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "old")]);
        let source = state
            .take_transcript_for_replay(&session.id)
            .expect("leased old source");
        let stale = ReplayTranscriptSource {
            session_id: source.session_id.clone(),
            cwd: source.cwd.clone(),
            incarnation: source.incarnation,
            revision: source.revision,
            complete: source.complete,
            last_uuid: source.last_uuid.clone(),
            entries: Vec::new(),
            handoff_id: source.handoff_id,
            restore_sessions: source.restore_sessions.clone(),
        };
        state
            .finalize_transcript_after_replay(source, Vec::new(), Some("old".into()))
            .expect("original completion releases lease");
        assert!(
            state.replace_transcript_entries(&session.id, vec![make_entry("user", "replacement")])
        );

        state
            .finalize_transcript_after_replay(
                stale,
                vec![make_entry("assistant", "stale-undurable")],
                Some("old".into()),
            )
            .expect("new incarnation intentionally supersedes stale completion");
        let current = state.get_session(&session.id).expect("replacement remains");
        assert_eq!(current.loaded_transcript.len(), 1);
        assert_eq!(current.loaded_transcript[0].uuid, "replacement");
    }

    #[test]
    fn released_load_reconciles_append_that_arrives_after_disk_read() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        let root = make_entry("user", "D");
        let mut tail = make_entry("assistant", "T");
        tail.parent_uuid = Some("D".into());
        tail.raw["parentUuid"] = serde_json::json!("D");
        state.push_transcript_entries(&session.id, vec![root.clone(), tail.clone()]);
        let initial = state
            .take_transcript_for_replay(&session.id)
            .expect("initial complete source");
        state
            .finalize_transcript_after_replay(initial, Vec::new(), Some("T".into()))
            .expect("release complete residency");

        let captured = state
            .take_transcript_for_replay(&session.id)
            .expect("capture identity before disk read");
        let cwd = captured.cwd.clone();
        let incarnation = captured.incarnation;
        let revision = captured.revision;
        state
            .restore_transcript_after_failed_replay(captured)
            .expect("end simulated disk read handoff");
        let mut concurrent = make_entry("user", "C");
        concurrent.parent_uuid = Some("T".into());
        concurrent.raw["parentUuid"] = serde_json::json!("T");
        state.push_transcript_entries(&session.id, vec![concurrent.clone()]);

        let materialized = state
            .commit_released_load(
                &session.id,
                &cwd,
                incarnation,
                revision,
                RawTranscriptFile {
                    entries: vec![root, tail],
                    byte_len: 2,
                    parsed_row_count: 2,
                    nonblank_row_count: 2,
                    parse_complete: true,
                },
            )
            .expect("concurrent pending append is included in the one overlay rebuild");
        assert_eq!(
            materialized
                .loaded_transcript
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["D", "T", "C"]
        );
        let resident = state
            .get_session(&session.id)
            .expect("recovery remains resident");
        assert_eq!(resident.loaded_transcript.len(), 1);
        assert_eq!(resident.loaded_transcript[0].uuid, "C");
        assert_eq!(resident.last_transcript_uuid.as_deref(), Some("C"));
    }

    #[test]
    fn stale_released_load_cannot_overwrite_replacement() {
        let state = ServerState::new();
        let session = state.create_session("/tmp/project".into(), Vec::new());
        state.push_transcript_entries(&session.id, vec![make_entry("user", "old")]);
        let captured = state
            .take_transcript_for_replay(&session.id)
            .expect("capture released source identity");
        let cwd = captured.cwd.clone();
        let incarnation = captured.incarnation;
        let revision = captured.revision;
        state
            .restore_transcript_after_failed_replay(captured)
            .expect("release captured handoff");
        assert!(
            state.replace_transcript_entries(&session.id, vec![make_entry("user", "replacement")])
        );

        let result = state
            .commit_released_load(
                &session.id,
                &cwd,
                incarnation,
                revision,
                RawTranscriptFile {
                    entries: vec![make_entry("user", "stale-disk")],
                    byte_len: 1,
                    parsed_row_count: 1,
                    nonblank_row_count: 1,
                    parse_complete: true,
                },
            )
            .expect("newer replacement is preserved");
        assert_eq!(result.loaded_transcript.len(), 1);
        assert_eq!(result.loaded_transcript[0].uuid, "replacement");
    }

    #[test]
    fn initially_absent_load_commit_preserves_concurrent_resident_recovery() {
        let state = ServerState::new();
        let created = state.create_session("/old".into(), Vec::new());
        let stale_disk_record = state
            .evict_session(&created.id)
            .expect("capture stale load");
        state.restore_empty_session(created.id.clone(), "/new".into(), Vec::new(), "default");
        let recovery = make_entry("user", "concurrent-recovery");
        state.push_transcript_entries(&created.id, vec![recovery]);

        let result = state
            .commit_initial_load(stale_disk_record)
            .expect("newer resident wins the compare-and-swap");
        assert_eq!(result.cwd, "/new");
        assert_eq!(result.loaded_transcript[0].uuid, "concurrent-recovery");
        assert_eq!(
            state.get_session(&created.id).unwrap().loaded_transcript[0].uuid,
            "concurrent-recovery"
        );
    }

    #[test]
    fn transactional_restore_cannot_overwrite_new_active_handoff() {
        let state = ServerState::new();
        let created = state.create_session("/old".into(), Vec::new());
        let evicted = state
            .evict_session(&created.id)
            .expect("evict old incarnation");
        state.restore_empty_session(created.id.clone(), "/new".into(), Vec::new(), "default");
        state.push_transcript_entries(&created.id, vec![make_entry("user", "newer-recovery")]);
        let handoff = state
            .take_transcript_for_replay(&created.id)
            .expect("new incarnation owns replay handoff");

        assert!(!state.restore_session(evicted));
        let resident = state
            .get_session(&created.id)
            .expect("newer record remains");
        assert_eq!(resident.cwd, "/new");
        assert!(resident.active_replay_handoff.is_some());
        state
            .restore_transcript_after_failed_replay(handoff)
            .expect("new handoff remains valid after refused restore");
        assert_eq!(
            state.get_session(&created.id).unwrap().loaded_transcript[0].uuid,
            "newer-recovery"
        );
    }

    #[test]
    fn replace_transcript_entries_returns_false_for_missing_session() {
        let s = ServerState::new();
        assert!(!s.replace_transcript_entries("missing", vec![make_entry("user", "u-1")],));
    }
}
