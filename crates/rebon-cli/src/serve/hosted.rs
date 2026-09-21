//! `rebon serve` as a client of the session's owner.
//!
//! Until RFC-0004 stage 4 this process *was* the host: `session/new` built a
//! session in the ACP server behind the mux, took its active lock, and ran
//! every turn here. That made the browser a fourth kind of host — one that
//! held a session open for as long as the server ran, that a terminal could
//! not resume, and whose turns died with the server.
//!
//! Now a page's session lives in a worker, exactly like a terminal's. This
//! module is the translation between the two protocols:
//!
//! | ACP, from a tab | to the session's owner |
//! |---|---|
//! | `session/new` | mint (through the server, for its metadata) → job record + `spawn_worker_process` |
//! | `session/load` | `resolve_owner`: reachable → attach; free → revive or start a worker; unreachable → `-32000`; opaque → read-only |
//! | `session/prompt` | `Reply` (or a pending prompt on the job while the worker is still starting — invariant I8) |
//! | `_session/steering` | `Steer`, whose `injected` / `startedNewTurn` is the ACP outcome |
//! | `session/cancel` | `Cancel`, fenced on a fresh `Status` |
//! | `session/set_config_option` | `SetPermissionMode` / `SetSessionOption`; anything the owner has no say in stays local |
//!
//! and the other direction, from the owner's `Subscribe` stream:
//!
//! | event | to every tab |
//! |---|---|
//! | `session_update` | `session/update`, broadcast |
//! | `turn` | `_serve/turn`, and the answer to whoever's `session/prompt` is waiting |
//! | `status` | the token ledger, `config_option_update` when a shared value moved (I4), and the withdrawal of a permission somebody else answered |
//! | `permission` | `session/request_permission` to every tab, first answer wins |
//! | `gap` | `_serve/reload`: the transcript on disk is the authority, and the page re-reads it |
//!
//! Two things this deliberately does not do. It never takes a session over:
//! a lock held by a process that will not answer means a live host, and a
//! second writer is the defect the lock exists to prevent. And it never
//! hosts in-process as a fallback — the terminal's `--local` escape hatch
//! exists because the user is already sitting in that process; a page has
//! nothing to gain from a host that dies with the server it was a
//! workaround for.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use rebon_proto::types::{error_code, ConfigOption, ConfigOptionValue};
use rebon_session::SessionOwnerSurface;
use rebon_session_host::owner::{resolve_owner, OwnerHandle, OwnerState};
use rebon_session_host::{
    generate_command_id, BackgroundImageAttachment, BackgroundJobStatus,
    BackgroundPermissionQuerySnapshot, BackgroundStore, ClientLeaseKind, SessionEvent,
    SessionHostClient, SessionHostConnection, SessionStatusSnapshot, TurnStreamState,
};

use crate::background::{
    attach_background_job_in_store, cli_default_store, rebon_exe, BackgroundRuntimeFields,
};
use crate::serve::mux::{AcpMux, ClientId, HostedRouter};

/// How often the pump looks again for a session's owner while it has none.
const OWNER_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The same, for a session held by a host that publishes no endpoint. There
/// is nothing to wait for at the fast cadence: the answer changes when that
/// host lets go, not sooner.
const READ_ONLY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// And how rarely it will ask for a worker to be brought back. A revival is
/// a job-record write and a process spawn; a session whose worker refuses to
/// come up must not turn into a spawn loop.
const REVIVE_INTERVAL: Duration = Duration::from_secs(2);

/// The JSON-RPC codes this file answers with, read from the one table that
/// defines them rather than written out again.
///
/// They were spelled here as literals with a comment pointing at that table,
/// which is a copy with a note saying where the original is. The widening is
/// because the mux carries ids and codes as `i64`; the values are the same
/// numbers either way.
const SESSION_OWNED_ELSEWHERE: i64 = error_code::SESSION_OWNED_ELSEWHERE as i64;
const INVALID_PARAMS: i64 = error_code::INVALID_PARAMS as i64;
const INTERNAL_ERROR: i64 = error_code::INTERNAL_ERROR as i64;

/// The config options whose value belongs to the session's owner, not to
/// this process. Everything else in the list is a `config.json` setting the
/// next worker will read, and is applied here.
fn owner_config_option(config_id: &str) -> Option<OwnerOption> {
    match config_id {
        "permissions" => Some(OwnerOption::PermissionMode),
        "model" => Some(OwnerOption::SessionOption("model")),
        "effort" => Some(OwnerOption::SessionOption("effort")),
        "agent" => Some(OwnerOption::SessionOption("agent")),
        "kernel" => Some(OwnerOption::SessionOption("kernel")),
        _ => None,
    }
}

enum OwnerOption {
    PermissionMode,
    SessionOption(&'static str),
}

/// Every session a tab has open here, and the worker each one lives in.
pub(crate) struct HostedSessions {
    me: Weak<HostedSessions>,
    projects_root: PathBuf,
    default_cwd: String,
    store: BackgroundStore,
    runtime: BackgroundRuntimeFields,
    /// The ACP server's config option list, shared with the handler behind
    /// the mux so a change either of them makes is the same list.
    config_options: Arc<Mutex<Vec<ConfigOption>>>,
    config_option_applier: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
    /// The server's session records, read for the cwd it resolved.
    server_state: Arc<rebon_acp::ServerState>,
    mux: OnceLock<AcpMux>,
    sessions: Mutex<HashMap<String, Arc<HostedSession>>>,
    /// One lease id for the whole process: a session two tabs have open is
    /// one client of its owner, not two.
    lease_client_id: String,
    /// The one prompt ladder, rather than a second copy
    /// of it here. Cheap to hold: a resolver plus an owner cache.
    client: SessionHostClient,
    /// Permission prompts issued to the tabs: wire id → where the answer goes.
    permissions: Mutex<HashMap<String, PermissionRoute>>,
    handle: tokio::runtime::Handle,
}

struct PermissionRoute {
    session_id: String,
    query_id: u64,
}

struct HostedSession {
    session_id: String,
    cwd: String,
    job_id: Mutex<Option<String>>,
    /// The live connection to this session's owner, when there is one.
    ///
    /// One value where `serve` used to keep three — the owner handle,
    /// the lease guard and the subscription closer — plus a cursor the pump
    /// carried in a local. They all belong to one endpoint generation, so they
    /// are held and dropped as one: replacing the connection releases the lease
    /// and ends the subscription, instead of leaving that to three `take()`s a
    /// future edit could get out of step.
    connection: Mutex<Option<Arc<SessionHostConnection>>>,
    /// Held by a process that publishes no endpoint (`rebon --local`, or a
    /// build from before descriptors existed): readable, not commandable.
    read_only: AtomicBool,
    clients: Mutex<HashSet<ClientId>>,
    stop: Arc<AtomicBool>,
    turn: Mutex<TurnState>,
    permission: Mutex<Option<IssuedPermission>>,
    /// The last shared values broadcast as a `config_option_update`, so the
    /// same status twice does not become two updates.
    shared: Mutex<Option<SharedValues>>,
    /// Owner-held options this server set and the owner accepted, kept only
    /// for the one the owner does not publish back.
    ///
    /// `SessionStatusSnapshot.agent` is `None` from every worker: the router
    /// that holds the choice lives in the worker's own process and nothing
    /// carries it out. So a tab that switched agent would go on being shown
    /// `local`. Until the owner publishes it, what this server last
    /// successfully set is the best answer it has — and it is only ever used
    /// where the owner said nothing, so a value the owner *does* publish
    /// always wins. A restarted server forgets, and shows the default again.
    echoed: Mutex<HashMap<String, String>>,
}

#[derive(Default)]
struct TurnState {
    running: bool,
    waiters: VecDeque<PromptWaiter>,
}

/// A `session/prompt` waiting for the turn it queued to end.
///
/// `armed` says the waiter may be answered by the next turn that ends.
/// A prompt sent while the owner was idle has to see a turn *start* first,
/// because the next `idle` before that belongs to nothing. One sent into a
/// running turn is armed from the start: the owner's attachment poller
/// injects it into that turn, which is the turn that will end.
struct PromptWaiter {
    client: ClientId,
    id: Value,
    armed: bool,
}

struct IssuedPermission {
    wire_id: String,
    query_id: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct SharedValues {
    permission_mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    agent: Option<String>,
}

impl SharedValues {
    /// Nothing heard from the owner yet.
    fn unknown() -> Self {
        Self {
            permission_mode: None,
            model: None,
            effort: None,
            agent: None,
        }
    }

    fn of(status: &SessionStatusSnapshot) -> Self {
        Self {
            permission_mode: status.permission_mode.clone(),
            model: status.model.clone(),
            effort: status.effort.clone(),
            agent: status.agent.clone(),
        }
    }

    /// The owner's value for one config option, when its status carries one.
    ///
    /// *Which* options are the owner's is [`owner_config_option`]'s answer and
    /// is deliberately not repeated here. This says only which of them a
    /// status snapshot reports: `kernel` is the owner's and has no field in
    /// the snapshot, so it has no arm, and its value reaches a tab through the
    /// echo instead.
    fn value_of(&self, config_id: &str) -> Option<String> {
        match config_id {
            "permissions" => self.permission_mode.clone(),
            "model" => self.model.clone(),
            "effort" => self.effort.clone(),
            "agent" => self.agent.clone(),
            _ => None,
        }
    }
}

/// The option list a tab renders: this process's list, with every value the
/// owner holds replaced by the owner's answer or by what it last accepted.
///
/// Which ids those are comes from [`owner_config_option`] rather than from a
/// second list here. It used to be a second list, and the two disagreed:
/// `kernel` is the owner's and was missing from this one, so a tab that
/// changed it watched its own control snap back to the old value and no other
/// tab heard about the change at all.
fn options_with_owner_values(
    mut options: Vec<ConfigOption>,
    shared: &SharedValues,
    echoed: &HashMap<String, String>,
) -> Vec<ConfigOption> {
    for option in options.iter_mut() {
        if owner_config_option(&option.id).is_none() {
            continue;
        }
        // The status, or -- for a value the owner accepted but does not report
        // -- what it was set to here. Without the echo the control would snap
        // back to the old value between the answer and the next status.
        let Some(value) = shared
            .value_of(&option.id)
            .or_else(|| echoed.get(&option.id).cloned())
        else {
            continue;
        };
        if !option
            .options
            .iter()
            .any(|candidate| candidate.value == value)
        {
            // The owner is on something this process does not offer (a model
            // from another provider entry). Show it rather than render a
            // control with no selection.
            option.options.insert(
                0,
                ConfigOptionValue {
                    value: value.clone(),
                    name: value.clone(),
                    description: None,
                },
            );
        }
        option.current_value = value;
    }
    options
}

impl HostedSessions {
    pub(crate) fn new(
        projects_root: PathBuf,
        default_cwd: String,
        runtime: BackgroundRuntimeFields,
        config_options: Arc<Mutex<Vec<ConfigOption>>>,
        config_option_applier: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
        server_state: Arc<rebon_acp::ServerState>,
        handle: tokio::runtime::Handle,
    ) -> Arc<Self> {
        Self::with_store(
            projects_root,
            default_cwd,
            cli_default_store(),
            runtime,
            config_options,
            config_option_applier,
            server_state,
            handle,
        )
    }

    /// The same, on a job store the caller names — what a test drives, so it
    /// can put a worker of its own where this would look for one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_store(
        projects_root: PathBuf,
        default_cwd: String,
        store: BackgroundStore,
        runtime: BackgroundRuntimeFields,
        config_options: Arc<Mutex<Vec<ConfigOption>>>,
        config_option_applier: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
        server_state: Arc<rebon_acp::ServerState>,
        handle: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let client = SessionHostClient::new(
            store.clone(),
            projects_root.clone(),
            crate::background::rebon_exe(),
        );
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            projects_root,
            default_cwd,
            store,
            runtime,
            config_options,
            config_option_applier,
            server_state,
            mux: OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            lease_client_id: format!("serve-{}", std::process::id()),
            client,
            permissions: Mutex::new(HashMap::new()),
            handle,
        })
    }

    pub(crate) fn attach_mux(&self, mux: AcpMux) {
        let _ = self.mux.set(mux);
    }

    fn mux(&self) -> Option<&AcpMux> {
        self.mux.get()
    }

    fn arc(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    /// The owner of a session a tab has open here, when there is a reachable
    /// one. What the `/api/*` levers ask before doing anything themselves.
    pub(crate) fn owner_for(&self, session_id: &str) -> Option<OwnerHandle> {
        Some(self.connection_for(session_id)?.owner().clone())
    }

    /// The live connection for a session a tab has open here.
    fn connection_for(&self, session_id: &str) -> Option<Arc<SessionHostConnection>> {
        let session = self.session(session_id)?;
        let connection = session.connection.lock().expect("poisoned").clone();
        connection
    }

    /// Whether a tab has this session open here at all. What the tests
    /// assert against; production code asks for the owner instead.
    #[cfg(test)]
    pub(crate) fn is_open(&self, session_id: &str) -> bool {
        self.session(session_id).is_some()
    }

    /// The owner of any session, open here or not.
    ///
    /// The HTTP levers are addressed by session id and answer for whatever
    /// the caller names — a page can ask for the token count of a session it
    /// has not opened, and it did, and got zeroes. So a session nobody has
    /// open falls back to resolving its owner from disk: a lock probe, a
    /// descriptor read and a ping, which is what any other client pays to
    /// find a host it is not already talking to.
    pub(crate) fn reachable_owner(&self, session_id: &str, cwd: &str) -> Option<OwnerHandle> {
        if let Some(owner) = self.owner_for(session_id) {
            return Some(owner);
        }
        match resolve_owner(&self.projects_root, cwd, session_id) {
            OwnerState::OwnedReachable { owner } => Some(owner),
            _ => None,
        }
    }

    fn session(&self, session_id: &str) -> Option<Arc<HostedSession>> {
        self.sessions
            .lock()
            .expect("poisoned")
            .get(session_id)
            .cloned()
    }

    // ---- opening -------------------------------------------------------

    /// `session/new`: the ACP server names the session and answers with the
    /// metadata a tab renders (config options, the slash-command menu); this
    /// process then gives that session a worker and never touches it again.
    fn open_new(&self, client: ClientId, id: Value, params: &Value) {
        let Some(mux) = self.mux().cloned() else {
            return;
        };
        let Some(hosted) = self.arc() else { return };
        let request = json!({ "jsonrpc": "2.0", "method": "session/new", "params": params });
        mux.clone().request_server(
            request,
            Box::new(move |response| {
                let Some(result) = response.get("result").cloned() else {
                    mux.send_raw_to_client(client, response, id);
                    return;
                };
                let session_id = result["sessionId"].as_str().unwrap_or_default().to_string();
                if session_id.is_empty() {
                    mux.fail_client(client, id, INTERNAL_ERROR, "session/new named no session", None);
                    return;
                }
                let cwd = hosted.resolved_cwd(&session_id);
                hosted.clone().handle.spawn_blocking(move || {
                    match hosted.start_worker_for_new_session(&session_id, &cwd) {
                        Ok(job_id) => {
                            hosted.register(&session_id, &cwd, Some(job_id), client);
                            mux.answer_client(client, id, result);
                        }
                        Err(err) => {
                            tracing::warn!(%session_id, error = %err, "rebon serve: could not start a worker for a new session");
                            mux.fail_client(
                                client,
                                id,
                                INTERNAL_ERROR,
                                format!("could not start a host for this session: {err}"),
                                None,
                            );
                        }
                    }
                });
            }),
        );
    }

    /// `session/load`: find out who owns it before anything else, because a
    /// session held by a host that will not answer is one this process must
    /// refuse rather than open (invariant I5).
    fn open_existing(&self, client: ClientId, id: Value, params: &Value) {
        let Some(mux) = self.mux().cloned() else {
            return;
        };
        let Some(hosted) = self.arc() else { return };
        let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
        if session_id.is_empty() {
            mux.fail_client(
                client,
                id,
                INVALID_PARAMS,
                "session/load requires a sessionId",
                None,
            );
            return;
        }
        let cwd = params["cwd"]
            .as_str()
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.default_cwd.clone());
        let params = params.clone();
        self.handle.clone().spawn_blocking(move || {
            if let Some(session) = hosted.session(&session_id) {
                // Already open here: a second tab is another client of the
                // same lease, not another session.
                session.clients.lock().expect("poisoned").insert(client);
            } else if let OwnerState::OwnedUnreachable { descriptor } =
                resolve_owner(&hosted.projects_root, &cwd, &session_id)
            {
                mux.fail_client(
                    client,
                    id,
                    SESSION_OWNED_ELSEWHERE,
                    format!(
                        "Session {session_id} is held by a process that is not answering (pid {}); it cannot be opened here",
                        descriptor.pid
                    ),
                    Some(json!({ "owner": {
                        "sessionId": session_id,
                        "cwd": cwd,
                        "known": true,
                        "pid": descriptor.pid,
                        "surface": surface_name(descriptor.surface),
                        "reachable": false,
                    }})),
                );
                return;
            }
            let request = json!({ "jsonrpc": "2.0", "method": "session/load", "params": params });
            let follow_up = hosted.clone();
            mux.clone().request_server(
                request,
                Box::new(move |response| {
                    let Some(result) = response.get("result").cloned() else {
                        mux.send_raw_to_client(client, response, id);
                        return;
                    };
                    // Register either way. The check at the top of this
                    // function ran before the server was asked, and a session
                    // can be put on the map in between — by the tab that
                    // created it, whose own registration happens after its
                    // worker starts, or by another `session/load` answered
                    // first. A client that arrived in that window used to be
                    // added to nothing, and then the *other* tab leaving
                    // released a lease this one was still using.
                    //
                    // RFC-0004 §16.9: one lease per session, released when the
                    // last tab leaves. Counting a tab only when it happened to
                    // be the one that opened the session is not that.
                    match follow_up.session(&session_id) {
                        Some(session) => {
                            session.clients.lock().expect("poisoned").insert(client);
                        }
                        None => follow_up.register(&session_id, &cwd, None, client),
                    }
                    mux.answer_client(client, id, result);
                }),
            );
        });
    }

    /// Put the session on the map and start following its owner.
    fn register(&self, session_id: &str, cwd: &str, job_id: Option<String>, client: ClientId) {
        let Some(hosted) = self.arc() else { return };
        let session = Arc::new(HostedSession {
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            job_id: Mutex::new(job_id),
            connection: Mutex::new(None),
            read_only: AtomicBool::new(false),
            clients: Mutex::new(HashSet::from([client])),
            stop: Arc::new(AtomicBool::new(false)),
            turn: Mutex::new(TurnState::default()),
            permission: Mutex::new(None),
            shared: Mutex::new(None),
            echoed: Mutex::new(HashMap::new()),
        });
        {
            let mut sessions = self.sessions.lock().expect("poisoned");
            if let Some(existing) = sessions.get(session_id) {
                existing.clients.lock().expect("poisoned").insert(client);
                return;
            }
            sessions.insert(session_id.to_string(), session.clone());
        }
        let pump = session.clone();
        if std::thread::Builder::new()
            .name("rebon-serve-session".to_string())
            .spawn(move || pump_session(hosted, pump))
            .is_err()
        {
            tracing::warn!(%session_id, "rebon serve: could not start the session's event pump");
        }
    }

    /// The cwd the ACP server resolved for a session it just named — the
    /// worker has to be started in the same directory the record says, not
    /// in whatever the request left blank.
    fn resolved_cwd(&self, session_id: &str) -> String {
        self.server_state
            .get_session(session_id)
            .map(|record| record.cwd)
            .unwrap_or_else(|| self.default_cwd.clone())
    }

    /// An empty transcript the worker can resume, a `Foreground` job queued
    /// `resume_only`, and the worker itself. The same three writes the
    /// terminal's startup route makes, for a session the ACP
    /// server has already named.
    fn start_worker_for_new_session(&self, session_id: &str, cwd: &str) -> anyhow::Result<String> {
        let path = rebon_session::ensure_session_file_path(&self.projects_root, cwd, session_id)?;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        rebon_session_host::host_existing_session(
            &self.store,
            session_id,
            cwd,
            self.runtime.clone(),
            true,
            &rebon_exe(),
        )
    }

    // ---- leaving -------------------------------------------------------

    /// A tab went away. A session nobody is watching any more gives its
    /// lease up now rather than at the end of the owner's TTL — the linger
    /// clock is the owner's to run, and it cannot start while this process
    /// keeps saying "somebody is here".
    fn release_client(&self, client: ClientId) {
        let dropped: Vec<Arc<HostedSession>> = {
            let mut sessions = self.sessions.lock().expect("poisoned");
            let mut dropped = Vec::new();
            sessions.retain(|_, session| {
                let mut clients = session.clients.lock().expect("poisoned");
                clients.remove(&client);
                if clients.is_empty() {
                    drop(clients);
                    dropped.push(session.clone());
                    false
                } else {
                    true
                }
            });
            dropped
        };
        for session in dropped {
            self.close_session(&session);
        }
    }

    fn close_session(&self, session: &Arc<HostedSession>) {
        session.stop.store(true, Ordering::Relaxed);
        // Closing ends the subscription and gives the lease up together. The
        // subscription is broken rather than waited out because a quiet session
        // would otherwise hold a thread until its next turn.
        if let Some(connection) = session.connection.lock().expect("poisoned").take() {
            connection.close();
        }
        self.withdraw_permission(session, "session closed");
        if let Some(mux) = self.mux() {
            mux.forget_session(&session.session_id);
        }
        tracing::info!(
            session_id = %session.session_id,
            "rebon serve: last client of this session left; lease released"
        );
    }

    // ---- the owner's stream --------------------------------------------

    fn on_event(&self, session: &Arc<HostedSession>, event: SessionEvent, since: &mut Option<u64>) {
        if let Some(cursor) = event.cursor() {
            *since = Some(cursor);
        }
        match event {
            SessionEvent::Hello { status, .. }
            | SessionEvent::Status {
                snapshot: status, ..
            } => self.apply_status(session, &status),
            SessionEvent::SessionUpdate { update, .. } => {
                if let Some(mux) = self.mux() {
                    mux.deliver_notification(
                        json!({ "jsonrpc": "2.0", "method": "session/update", "params": update }),
                    );
                }
            }
            SessionEvent::Turn {
                state, stop_reason, ..
            } => self.note_turn(session, state, stop_reason),
            SessionEvent::Permission { query, .. } => self.issue_permission(session, &query),
            SessionEvent::Gap { from, to } => {
                *since = Some(to);
                tracing::info!(
                    session_id = %session.session_id,
                    from,
                    to,
                    "rebon serve: fell behind the owner's stream; asking the page to re-read the transcript"
                );
                if let Some(mux) = self.mux() {
                    mux.deliver_notification(json!({
                        "jsonrpc": "2.0",
                        "method": "_serve/reload",
                        "params": { "sessionId": session.session_id, "reason": "gap", "from": from, "to": to },
                    }));
                }
            }
        }
    }

    fn note_turn(
        &self,
        session: &Arc<HostedSession>,
        state: TurnStreamState,
        stop_reason: Option<String>,
    ) {
        let running = matches!(state, TurnStreamState::Running);
        let answer = {
            let mut turn = session.turn.lock().expect("poisoned");
            turn.running = running;
            if running {
                for waiter in turn.waiters.iter_mut() {
                    waiter.armed = true;
                }
                None
            } else if turn.waiters.front().map(|waiter| waiter.armed) == Some(true) {
                turn.waiters.pop_front()
            } else {
                None
            }
        };
        if let Some(mux) = self.mux() {
            mux.note_turn(&session.session_id, running, stop_reason.clone(), None);
            if let Some(waiter) = answer {
                mux.answer_client(
                    waiter.client,
                    waiter.id,
                    json!({ "stopReason": stop_reason.unwrap_or_else(|| "end_turn".to_string()) }),
                );
            }
        }
    }

    fn apply_status(&self, session: &Arc<HostedSession>, status: &SessionStatusSnapshot) {
        let Some(mux) = self.mux() else { return };
        let running = status.busy
            || matches!(
                status.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
            );
        session.turn.lock().expect("poisoned").running = running;
        mux.note_turn(&session.session_id, running, None, None);
        if let Some(usage) = &status.usage {
            mux.set_usage(&session.session_id, usage.input_tokens, usage.output_tokens);
        }
        match &status.pending_permission {
            Some(query) => self.issue_permission(session, query),
            None => self.withdraw_permission(session, "resolved by another client"),
        }
        // Invariant I4: a permission mode, model, effort or agent one client
        // changed has to reach the others. `config_option_update` is what a
        // tab already re-renders its controls from.
        let values = SharedValues::of(status);
        let changed = {
            let mut shared = session.shared.lock().expect("poisoned");
            let changed = shared.as_ref() != Some(&values);
            *shared = Some(values);
            changed
        };
        if changed {
            self.broadcast_config_options(session);
        }
    }

    /// Tell every tab what this session's controls read now.
    ///
    /// `config_option_update` is what a tab re-renders its controls from, so
    /// this is how a value one tab changed reaches the others -- whether it
    /// moved because the owner said so (invariant I4, above) or because
    /// somebody set a local one here.
    fn broadcast_config_options(&self, session: &Arc<HostedSession>) {
        let Some(mux) = self.mux() else { return };
        let options = self.config_options_for(session);
        mux.deliver_notification(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session.session_id,
                "update": { "sessionUpdate": "config_option_update", "configOptions": options },
            },
        }));
    }

    fn issue_permission(
        &self,
        session: &Arc<HostedSession>,
        query: &BackgroundPermissionQuerySnapshot,
    ) {
        let Some(mux) = self.mux() else { return };
        let wire_id = format!("perm-{}-{}", session.session_id, query.query_id);
        // One lock at a time, and always the session's before the router's:
        // an answer arriving from a tab takes them in the other order, and
        // two threads holding one each would be a deadlock instead of a
        // permission prompt.
        let previous = {
            let mut issued = session.permission.lock().expect("poisoned");
            if issued
                .as_ref()
                .map(|issued| issued.query_id == query.query_id)
                == Some(true)
            {
                return;
            }
            issued.replace(IssuedPermission {
                wire_id: wire_id.clone(),
                query_id: query.query_id,
            })
        };
        if let Some(previous) = previous {
            self.permissions
                .lock()
                .expect("poisoned")
                .remove(&previous.wire_id);
            if mux.withdraw_server_request(&json!(previous.wire_id)) {
                mux.deliver_notification(permission_resolved(
                    &session.session_id,
                    &previous.wire_id,
                    "superseded",
                ));
            }
        }
        self.permissions.lock().expect("poisoned").insert(
            wire_id.clone(),
            PermissionRoute {
                session_id: session.session_id.clone(),
                query_id: query.query_id,
            },
        );
        mux.issue_server_request(json!({
            "jsonrpc": "2.0",
            "id": wire_id,
            "method": "session/request_permission",
            "params": crate::session::host::permission_params(&session.session_id, query),
        }));
    }

    fn withdraw_permission(&self, session: &Arc<HostedSession>, reason: &str) {
        let Some(mux) = self.mux() else { return };
        let Some(issued) = session.permission.lock().expect("poisoned").take() else {
            return;
        };
        self.permissions
            .lock()
            .expect("poisoned")
            .remove(&issued.wire_id);
        if mux.withdraw_server_request(&json!(issued.wire_id)) {
            mux.deliver_notification(permission_resolved(
                &session.session_id,
                &issued.wire_id,
                reason,
            ));
        }
    }

    /// Whoever the owner is now, and a lease on it. `None` while there is
    /// none to be had — the caller waits and asks again.
    fn find_owner(
        &self,
        session: &Arc<HostedSession>,
        last_revive: &mut Instant,
    ) -> Option<OwnerHandle> {
        match resolve_owner(&self.projects_root, &session.cwd, &session.session_id) {
            OwnerState::OwnedReachable { owner } => {
                session.read_only.store(false, Ordering::Relaxed);
                Some(owner)
            }
            OwnerState::OwnedOpaque { descriptor } => {
                session.read_only.store(true, Ordering::Relaxed);
                tracing::debug!(
                    session_id = %session.session_id,
                    pid = ?descriptor.as_ref().map(|descriptor| descriptor.pid),
                    "rebon serve: session is held in another process without an endpoint; read-only"
                );
                None
            }
            OwnerState::OwnedUnreachable { descriptor } => {
                session.read_only.store(true, Ordering::Relaxed);
                tracing::debug!(
                    session_id = %session.session_id,
                    pid = descriptor.pid,
                    "rebon serve: the session's host is not answering; read-only, and not taken over"
                );
                None
            }
            OwnerState::Free => {
                session.read_only.store(false, Ordering::Relaxed);
                self.bring_a_worker_back(session, last_revive);
                None
            }
        }
    }

    /// Nobody holds the session. Either its worker has not taken the lock
    /// yet (it is starting), or it went away and the job record says whether
    /// that was a crash or a decision.
    fn bring_a_worker_back(&self, session: &Arc<HostedSession>, last_revive: &mut Instant) {
        if last_revive.elapsed() < REVIVE_INTERVAL {
            return;
        }
        *last_revive = Instant::now();
        let job = rebon_session_host::home_job_for_session(&self.store, &session.session_id)
            .ok()
            .flatten();
        let Some(job) = job else {
            match rebon_session_host::host_existing_session(
                &self.store,
                &session.session_id,
                &session.cwd,
                self.runtime.clone(),
                true,
                &rebon_exe(),
            ) {
                Ok(job_id) => {
                    tracing::info!(session_id = %session.session_id, %job_id, "rebon serve: gave the session a worker");
                    *session.job_id.lock().expect("poisoned") = Some(job_id);
                }
                Err(err) => tracing::warn!(
                    session_id = %session.session_id,
                    error = %err,
                    "rebon serve: could not give the session a worker"
                ),
            }
            return;
        };
        *session.job_id.lock().expect("poisoned") = Some(job.identity.job_id.clone());
        if job.process.pid.is_some() || matches!(job.process.status, BackgroundJobStatus::Queued) {
            // A worker is on its way; the lock arrives with it.
            return;
        }
        if matches!(job.process.status, BackgroundJobStatus::Stopped) {
            // Somebody stopped this worker on purpose. A page watching from
            // the side does not get to undo that.
            tracing::debug!(
                session_id = %session.session_id,
                job_id = %job.identity.job_id,
                "rebon serve: the session's worker was stopped; not reviving it from here"
            );
            return;
        }
        match attach_background_job_in_store(&self.store, &job.identity.job_id) {
            Ok(_) => tracing::info!(
                session_id = %session.session_id,
                job_id = %job.identity.job_id,
                "rebon serve: the session's worker went away; queued a replacement"
            ),
            Err(err) => tracing::warn!(
                session_id = %session.session_id,
                job_id = %job.identity.job_id,
                error = %err,
                "rebon serve: could not bring the session's worker back"
            ),
        }
    }

    /// The connection to an owner ended. Whatever was waiting on it has to
    /// be told, because nothing else will answer it.
    fn detach(&self, session: &Arc<HostedSession>) {
        if let Some(connection) = session.connection.lock().expect("poisoned").take() {
            connection.close();
        }
        self.withdraw_permission(session, "the session's host went away");
        let waiters: Vec<PromptWaiter> = {
            let mut turn = session.turn.lock().expect("poisoned");
            turn.running = false;
            turn.waiters.drain(..).collect()
        };
        if let Some(mux) = self.mux() {
            mux.note_turn(
                &session.session_id,
                false,
                Some("cancelled".to_string()),
                (!waiters.is_empty()).then(|| "the session's host went away".to_string()),
            );
            for waiter in waiters {
                mux.fail_client(
                    waiter.client,
                    waiter.id,
                    INTERNAL_ERROR,
                    "the session's host went away before the turn finished",
                    None,
                );
            }
        }
    }

    // ---- commands ------------------------------------------------------

    fn prompt(&self, session: Arc<HostedSession>, client: ClientId, id: Value, params: &Value) {
        let Some(mux) = self.mux().cloned() else {
            return;
        };
        if session.read_only.load(Ordering::Relaxed) {
            mux.fail_client(
                client,
                id,
                SESSION_OWNED_ELSEWHERE,
                "this session is open in another Rebon process; it can be read here but not driven",
                None,
            );
            return;
        }
        let (message, images) = prompt_to_message(&params["prompt"]);
        if message.trim().is_empty() && images.is_empty() {
            mux.fail_client(
                client,
                id,
                INVALID_PARAMS,
                "session/prompt requires a prompt",
                None,
            );
            return;
        }
        let command_id = generate_command_id();
        let prompts = self.client.clone();
        self.handle.clone().spawn_blocking(move || {
            let owner = session.connection.lock().expect("poisoned").clone();
            match owner {
                Some(owner) => {
                    // A prompt that lands in a running turn is injected into
                    // it, so the turn that ends is the one already running;
                    // one that lands on an idle owner has to see a turn start.
                    let busy = owner.status().map(|status| status.busy).unwrap_or(false);
                    session.push_waiter(client, id.clone(), busy);
                    if let Err(err) = owner.owner().reply(message, images, Some(command_id)) {
                        session.drop_waiter(&id);
                        mux.fail_client(client, id, INTERNAL_ERROR, err.to_string(), None);
                    }
                }
                None => {
                    // No connection here, so the decision is the ladder's
                    // rather than this module's. It
                    // matters which rung: a worker that is starting gets the
                    // prompt appended durably (invariant I8), while an owner
                    // that holds the lock and will not answer is refused —
                    // queueing for that one would park a prompt behind a worker
                    // that is already running and will never claim it. serve
                    // used to queue in both cases.
                    let job_id = session.job_id.lock().expect("poisoned").clone();
                    match prompts.send_prompt(
                        &session.cwd,
                        &session.session_id,
                        job_id.as_deref(),
                        message,
                        images,
                    ) {
                        Ok(_) => session.push_waiter(client, id, false),
                        Err(err) => mux.fail_client(
                            client,
                            id,
                            INTERNAL_ERROR,
                            format!("could not deliver the prompt: {err}"),
                            None,
                        ),
                    }
                }
            }
        });
    }

    fn steer(&self, session: Arc<HostedSession>, client: ClientId, id: Value, params: &Value) {
        let Some(mux) = self.mux().cloned() else {
            return;
        };
        let (message, images) = prompt_to_message(&params["prompt"]);
        if message.trim().is_empty() && images.is_empty() {
            mux.fail_client(
                client,
                id,
                INVALID_PARAMS,
                "_session/steering requires a non-empty prompt",
                None,
            );
            return;
        }
        let command_id = generate_command_id();
        self.handle.clone().spawn_blocking(move || {
            let Some(owner) = session.connection.lock().expect("poisoned").clone() else {
                mux.fail_client(
                    client,
                    id,
                    INTERNAL_ERROR,
                    "this session has no reachable host to steer",
                    None,
                );
                return;
            };
            match owner.owner().steer(message, images, Some(command_id)) {
                Ok(outcome) => mux.answer_client(
                    client,
                    id,
                    json!(rebon_proto::types::SessionSteeringResult { outcome }),
                ),
                Err(err) => mux.fail_client(client, id, INTERNAL_ERROR, err.to_string(), None),
            }
        });
    }

    fn cancel(&self, session: Arc<HostedSession>) {
        self.handle.clone().spawn_blocking(move || {
            let Some(owner) = session.connection.lock().expect("poisoned").clone() else {
                return;
            };
            match owner.owner().cancel_turn() {
                Ok(true) => tracing::info!(session_id = %session.session_id, "rebon serve: cancelled the running turn"),
                Ok(false) => tracing::debug!(session_id = %session.session_id, "rebon serve: nothing to cancel"),
                Err(err) => tracing::warn!(session_id = %session.session_id, error = %err, "rebon serve: cancel refused"),
            }
        });
    }

    fn set_config_option(
        &self,
        session: Arc<HostedSession>,
        client: ClientId,
        id: Value,
        params: &Value,
    ) {
        let Some(mux) = self.mux().cloned() else {
            return;
        };
        let Some(hosted) = self.arc() else { return };
        let config_id = params["configId"].as_str().unwrap_or_default().to_string();
        let value = params["value"].as_str().unwrap_or_default().to_string();
        if config_id.is_empty() {
            mux.fail_client(
                client,
                id,
                INVALID_PARAMS,
                "session/set_config_option requires a configId",
                None,
            );
            return;
        }
        self.handle.clone().spawn_blocking(move || {
            let owner = session.connection.lock().expect("poisoned").clone();
            let outcome = match owner_config_option(&config_id) {
                Some(kind) => {
                    let Some(owner) = owner else {
                        mux.fail_client(
                            client,
                            id,
                            INTERNAL_ERROR,
                            "this session has no reachable host to change",
                            None,
                        );
                        return;
                    };
                    match kind {
                        OwnerOption::PermissionMode => {
                            owner.owner().set_permission_mode(&value).map(|()| ())
                        }
                        OwnerOption::SessionOption(key) => {
                            owner.owner().set_session_option(key, &value).map(|_| ())
                        }
                    }
                }
                // Not the session's to hold: a `config.json` setting the next
                // worker reads. Applied here, where that file is written.
                None => {
                    hosted.apply_local_config_option(&config_id, &value);
                    Ok(())
                }
            };
            match outcome {
                Ok(()) => {
                    let owner_held = owner_config_option(&config_id).is_some();
                    if owner_held {
                        session
                            .echoed
                            .lock()
                            .expect("poisoned")
                            .insert(config_id.clone(), value.clone());
                    }
                    let options = hosted.config_options_for(&session);
                    mux.answer_client(client, id, json!({ "configOptions": options }));
                    if !owner_held {
                        // An owner-held value reaches the other tabs on the
                        // owner's next status, which is the I4 path and knows
                        // whether anything actually moved. A local one has no
                        // such echo: it is written to `config.json` here and
                        // nothing else will ever mention it, so a second tab
                        // went on showing the old value until it was reloaded.
                        hosted.broadcast_config_options(&session);
                    }
                }
                Err(err) => mux.fail_client(client, id, INVALID_PARAMS, err.to_string(), None),
            }
        });
    }

    fn apply_local_config_option(&self, config_id: &str, value: &str) {
        let accepted = {
            let mut options = self.config_options.lock().expect("poisoned");
            match options.iter_mut().find(|option| option.id == config_id) {
                Some(option)
                    if option
                        .options
                        .iter()
                        .any(|candidate| candidate.value == value) =>
                {
                    option.current_value = value.to_string();
                    true
                }
                _ => false,
            }
        };
        if accepted {
            if let Some(apply) = &self.config_option_applier {
                apply(config_id, value);
            }
        }
    }

    /// This session's option list, as [`options_with_owner_values`] renders it.
    fn config_options_for(&self, session: &Arc<HostedSession>) -> Vec<ConfigOption> {
        let options = self.config_options.lock().expect("poisoned").clone();
        let shared = session
            .shared
            .lock()
            .expect("poisoned")
            .clone()
            .unwrap_or_else(SharedValues::unknown);
        let echoed = session.echoed.lock().expect("poisoned").clone();
        options_with_owner_values(options, &shared, &echoed)
    }
}

impl HostedSession {
    fn push_waiter(&self, client: ClientId, id: Value, armed: bool) {
        self.turn
            .lock()
            .expect("poisoned")
            .waiters
            .push_back(PromptWaiter { client, id, armed });
    }

    fn drop_waiter(&self, id: &Value) {
        self.turn
            .lock()
            .expect("poisoned")
            .waiters
            .retain(|waiter| &waiter.id != id);
    }
}

impl HostedRouter for HostedSessions {
    fn take_request(&self, client: ClientId, id: &Value, method: &str, params: &Value) -> bool {
        match method {
            "session/new" => {
                self.open_new(client, id.clone(), params);
                true
            }
            "session/load" => {
                self.open_existing(client, id.clone(), params);
                true
            }
            "session/prompt" | "_session/steering" | "session/set_config_option" => {
                let session_id = params["sessionId"].as_str().unwrap_or_default();
                let Some(session) = self.session(session_id) else {
                    // Not a session a tab opened here. The ACP server behind
                    // the mux would answer "Session not found"; say the same
                    // thing rather than let it host a turn nobody owns.
                    if let Some(mux) = self.mux() {
                        mux.fail_client(
                            client,
                            id.clone(),
                            INVALID_PARAMS,
                            format!("Session not found: {session_id}"),
                            None,
                        );
                    }
                    return true;
                };
                match method {
                    "session/prompt" => self.prompt(session, client, id.clone(), params),
                    "_session/steering" => self.steer(session, client, id.clone(), params),
                    _ => self.set_config_option(session, client, id.clone(), params),
                }
                true
            }
            _ => false,
        }
    }

    fn take_notification(&self, method: &str, params: &Value) -> bool {
        if method != "session/cancel" {
            return false;
        }
        let session_id = params["sessionId"].as_str().unwrap_or_default();
        match self.session(session_id) {
            Some(session) => {
                self.cancel(session);
                true
            }
            None => false,
        }
    }

    fn owns_server_request(&self, id: &Value) -> bool {
        id.as_str()
            .map(|id| self.permissions.lock().expect("poisoned").contains_key(id))
            .unwrap_or(false)
    }

    fn answer_server_request(&self, id: &Value, response: &Value) {
        let Some(wire_id) = id.as_str().map(str::to_owned) else {
            return;
        };
        let Some(route) = self.permissions.lock().expect("poisoned").remove(&wire_id) else {
            return;
        };
        let Some(session) = self.session(&route.session_id) else {
            return;
        };
        session.permission.lock().expect("poisoned").take();
        // Every other tab is still showing the prompt this one answered.
        if let Some(mux) = self.mux() {
            mux.deliver_notification(permission_resolved(&route.session_id, &wire_id, "answered"));
        }
        let outcome = &response["result"]["outcome"];
        let option_id = (outcome["outcome"] != "cancelled")
            .then(|| outcome["optionId"].as_str().map(str::to_owned))
            .flatten();
        let extra_text = outcome["extraText"].as_str().map(str::to_owned);
        let query_id = route.query_id;
        // No id minted here any more: the connection allocates a monotonic one
        // for the call, which is also what a `CancelCall` would name.
        self.handle.clone().spawn_blocking(move || {
            let Some(owner) = session.connection.lock().expect("poisoned").clone() else {
                return;
            };
            match owner.answer_permission(query_id, option_id.as_deref(), extra_text) {
                Ok(outcome) => tracing::debug!(
                    session_id = %session.session_id,
                    query_id,
                    ?outcome,
                    "rebon serve: forwarded a permission answer to the session's owner"
                ),
                Err(err) => tracing::warn!(
                    session_id = %session.session_id,
                    query_id,
                    error = %err,
                    "rebon serve: the owner refused a permission answer"
                ),
            }
        });
    }

    fn client_gone(&self, client: ClientId) {
        self.release_client(client);
    }
}

/// Follow one session's owner for as long as a tab has it open.
///
/// A worker that goes away is not an error here: the job record says whether
/// it crashed (bring another) or was stopped (leave it), and either way the
/// session on disk is unharmed. What must not happen is this process
/// deciding to host the session itself.
fn pump_session(hosted: Arc<HostedSessions>, session: Arc<HostedSession>) {
    let mut since: Option<u64> = None;
    let mut owner_pid: Option<u32> = None;
    let mut last_revive = Instant::now() - REVIVE_INTERVAL;
    while !session.stop.load(Ordering::Relaxed) {
        let Some(owner) = hosted.find_owner(&session, &mut last_revive) else {
            // A session that is starting is polled quickly, because the
            // endpoint is about to appear and a page is waiting for it. One
            // held by a host that publishes no endpoint is not starting: it
            // will be answered by whatever ends that host, so asking four
            // times a second buys nothing but lock probes.
            std::thread::sleep(if session.read_only.load(Ordering::Relaxed) {
                READ_ONLY_POLL_INTERVAL
            } else {
                OWNER_POLL_INTERVAL
            });
            continue;
        };
        // A different process means a different cursor space; resuming from
        // the old one would ask a new ring for numbers it will never have.
        // The connection carries the cursor, so a replaced endpoint drops it
        // along with the lease and the subscription rather than leaving three
        // things to reset in step.
        let connection = Arc::new(SessionHostConnection::new(owner.clone()));
        if owner_pid == Some(owner.pid) {
            if let Some(cursor) = since {
                connection.observe_cursor(cursor);
            }
        }
        owner_pid = Some(owner.pid);
        connection.hold_lease(&hosted.lease_client_id, ClientLeaseKind::Serve);
        *session.connection.lock().expect("poisoned") = Some(Arc::clone(&connection));
        if let Some(job_id) = owner.job_id.clone() {
            *session.job_id.lock().expect("poisoned") = Some(job_id);
        }
        since = connection.last_cursor();
        let stream = match connection.subscribe() {
            Ok(stream) => stream,
            Err(err) => {
                tracing::debug!(
                    session_id = %session.session_id,
                    error = %err,
                    "rebon serve: could not open the owner's event stream"
                );
                hosted.detach(&session);
                std::thread::sleep(OWNER_POLL_INTERVAL);
                continue;
            }
        };
        tracing::info!(
            session_id = %session.session_id,
            pid = owner.pid,
            port = owner.port,
            "rebon serve: attached to the session's owner"
        );
        // The subscription's closer is kept by the connection, which
        // `subscribe` above already did — so ending the subscription and
        // giving the lease up are the one `close` that `detach` calls.
        for event in stream {
            if session.stop.load(Ordering::Relaxed) {
                break;
            }
            hosted.on_event(&session, event, &mut since);
        }
        hosted.detach(&session);
        if session.stop.load(Ordering::Relaxed) {
            break;
        }
        tracing::info!(
            session_id = %session.session_id,
            "rebon serve: the owner's stream ended; looking for it again"
        );
        std::thread::sleep(OWNER_POLL_INTERVAL);
    }
    hosted.detach(&session);
}

// ---- shapes ------------------------------------------------------------

fn surface_name(surface: SessionOwnerSurface) -> &'static str {
    match surface {
        SessionOwnerSurface::Worker => "worker",
        SessionOwnerSurface::Local => "local",
        SessionOwnerSurface::Acp => "acp",
    }
}

fn permission_resolved(session_id: &str, wire_id: &str, reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_serve/permission_resolved",
        "params": { "sessionId": session_id, "requestId": wire_id, "reason": reason },
    })
}

/// An ACP prompt as the owner's `Reply` takes it: one message, plus the
/// images as attachments. Resource blocks are folded into the text the way a
/// terminal would have pasted them, because that is what reaches the model
/// either way.
fn prompt_to_message(prompt: &Value) -> (String, Vec<BackgroundImageAttachment>) {
    let mut text = String::new();
    let mut images = Vec::new();
    let Some(blocks) = prompt.as_array() else {
        return (text, images);
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(chunk) = block["text"].as_str() {
                    if !text.is_empty() && !chunk.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(chunk);
                }
            }
            Some("image") => {
                let (Some(data), Some(media_type)) =
                    (block["data"].as_str(), block["mimeType"].as_str())
                else {
                    continue;
                };
                images.push(BackgroundImageAttachment {
                    id: images.len() as u32 + 1,
                    data: data.to_string(),
                    media_type: media_type.to_string(),
                    filename: None,
                    source_path: None,
                });
            }
            Some("resource") => {
                let resource = &block["resource"];
                let uri = resource["uri"].as_str().unwrap_or("attachment");
                if let Some(contents) = resource["text"].as_str() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!(
                        "\n<attachment uri=\"{uri}\">\n{contents}\n</attachment>"
                    ));
                }
            }
            _ => {}
        }
    }
    (text, images)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_becomes_one_message_and_its_images() {
        let prompt = json!([
            { "type": "text", "text": "look at this" },
            { "type": "image", "mimeType": "image/png", "data": "AAAA" },
            { "type": "resource", "resource": { "uri": "file:///notes.md", "text": "hello" } },
            { "type": "text", "text": "thanks" },
        ]);
        let (message, images) = prompt_to_message(&prompt);
        assert!(message.starts_with("look at this"));
        assert!(message.contains("<attachment uri=\"file:///notes.md\">"));
        assert!(message.contains("hello"));
        assert!(message.ends_with("thanks"));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "AAAA");
    }

    #[test]
    fn an_empty_prompt_is_empty_rather_than_a_panic() {
        let (message, images) = prompt_to_message(&json!(null));
        assert!(message.is_empty());
        assert!(images.is_empty());
        let (message, images) = prompt_to_message(&json!([]));
        assert!(message.is_empty());
        assert!(images.is_empty());
    }

    /// Each ACP method the page sends has exactly one internal command, and
    /// the ones that are not the owner's business are not claimed.
    #[test]
    fn the_translation_table_says_who_owns_each_config_option() {
        assert!(matches!(
            owner_config_option("permissions"),
            Some(OwnerOption::PermissionMode)
        ));
        for id in ["model", "effort", "agent", "kernel"] {
            match owner_config_option(id) {
                Some(OwnerOption::SessionOption(key)) => assert_eq!(key, id),
                _ => panic!("{id} belongs to the session's owner"),
            }
        }
        for id in [
            "fast_mode",
            "sub_agents",
            "update_auto_install",
            "claude_codex_fallback",
            "shell_tool",
        ] {
            assert!(
                owner_config_option(id).is_none(),
                "{id} is a config.json setting, not session state"
            );
        }
    }

    fn option(id: &str, values: &[&str]) -> ConfigOption {
        ConfigOption {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            category: None,
            option_type: rebon_proto::types::ConfigOptionType::Select,
            current_value: values[0].to_string(),
            options: values
                .iter()
                .map(|value| ConfigOptionValue {
                    value: value.to_string(),
                    name: value.to_string(),
                    description: None,
                })
                .collect(),
        }
    }

    /// Every option the owner holds is rendered from what the owner said,
    /// including the one its status never mentions.
    ///
    /// The list of owner-held ids used to be written twice: once in the table
    /// above and once in the render loop. `kernel` was in the first and not
    /// the second, so the tab that changed it saw its control snap back and
    /// the other tabs never heard about it. This is the case that stayed
    /// broken, next to the ones that did not.
    #[test]
    fn an_option_the_owner_holds_and_never_reports_is_still_rendered_from_its_echo() {
        let shared = SharedValues {
            permission_mode: Some("plan".to_string()),
            model: None,
            effort: None,
            agent: None,
        };
        // What the owner accepted since its last status, which for `kernel` is
        // the only place the value will ever be.
        let echoed = HashMap::from([
            ("kernel".to_string(), "loop-b".to_string()),
            ("effort".to_string(), "high".to_string()),
            // Not the owner's, and not to be taken from here: this process's
            // own list is the authority for a `config.json` setting.
            ("sub_agents".to_string(), "off".to_string()),
        ]);
        let rendered = options_with_owner_values(
            vec![
                option("permissions", &["default", "plan"]),
                option("effort", &["auto", "high"]),
                option("kernel", &["loop-a", "loop-b"]),
                option("sub_agents", &["on", "off"]),
            ],
            &shared,
            &echoed,
        );
        let value_of = |id: &str| {
            rendered
                .iter()
                .find(|option| option.id == id)
                .unwrap_or_else(|| panic!("{id} was dropped from the list"))
                .current_value
                .clone()
        };
        assert_eq!(value_of("permissions"), "plan", "straight from the status");
        assert_eq!(
            value_of("effort"),
            "high",
            "from the echo, pending a status"
        );
        assert_eq!(
            value_of("kernel"),
            "loop-b",
            "the owner holds it and never reports it, so the echo is all there is"
        );
        assert_eq!(
            value_of("sub_agents"),
            "on",
            "a local setting keeps this process's value even with an echo present"
        );
    }

    /// A value the owner is on that this process does not offer is shown,
    /// rather than leaving a control with nothing selected.
    #[test]
    fn a_value_this_process_does_not_offer_is_added_rather_than_dropped() {
        let shared = SharedValues {
            model: Some("some-other-providers-model".to_string()),
            ..SharedValues::unknown()
        };
        let rendered = options_with_owner_values(
            vec![option("model", &["sonnet", "opus"])],
            &shared,
            &HashMap::new(),
        );
        assert_eq!(rendered[0].current_value, "some-other-providers-model");
        assert_eq!(
            rendered[0].options[0].value, "some-other-providers-model",
            "and it is selectable, at the top"
        );
        assert_eq!(rendered[0].options.len(), 3);
    }
}
