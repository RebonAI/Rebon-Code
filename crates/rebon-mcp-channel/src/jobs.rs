//! The job operations behind the tools, and what the watcher reads.
//!
//! Every write here is one of the session host's own transactions —
//! [`rebon_session_host::launch_background_prompt`], the reply, answer and
//! stop paths the terminal and the desktop app use. This surface owns no job
//! state: it launches, it asks, and it reads `state.json`, which stays the one
//! authority.
//!
//! Everything here blocks (file locks, a loopback IPC round trip, a process
//! exit wait on stop), so callers run it on the blocking pool.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundLaunchOptions,
    BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot, BackgroundRuntimeFields,
    BackgroundStore, ForegroundQuestionAnswer,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ledger::{self, JobLedger, LedgerOwner};
use crate::push::{Pending, Update};
use crate::result;

/// Checked before any job is launched. The binary fills it with what only it
/// can check: that background jobs are enabled at all, and that the job's
/// permission mode is one the user has accepted for unattended runs.
pub type LaunchGate = Arc<dyn Fn(&BackgroundRuntimeFields) -> anyhow::Result<()> + Send + Sync>;

/// How long after starting a job a restarted server still takes it over.
/// A day: long enough to cover a client restart or an overnight job, short
/// enough that a server started next week does not replay old news.
pub(crate) const ADOPTION_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// `job_result`'s preview size when the caller does not name one, and the
/// range it may name. The file holds the rest; the preview only has to carry
/// the conclusion into the conversation.
const DEFAULT_PREVIEW_CHARS: usize = 4000;
const MIN_PREVIEW_CHARS: usize = 200;
const MAX_PREVIEW_CHARS: usize = 20_000;
/// A permission prompt's tool input, as `job_status` shows it. A `Write` of a
/// whole file would otherwise land in the conversation.
const MAX_INPUT_PREVIEW_CHARS: usize = 2000;

pub(crate) struct Desk {
    store: BackgroundStore,
    projects_root: PathBuf,
    root: PathBuf,
    rebon_exe: PathBuf,
    launch_gate: LaunchGate,
    owner: LedgerOwner,
    channel: bool,
    /// Jobs this server pushes for: the ones it started and the ones it took
    /// over. Only the watcher removes from it.
    tracked: Mutex<BTreeSet<String>>,
    /// Wakes the watcher when a job is added, so a new job is watched at the
    /// active cadence rather than whatever an idle backoff had reached.
    pub(crate) woken: tokio::sync::Notify,
}

pub(crate) struct DeskConfig {
    pub store: BackgroundStore,
    pub projects_root: PathBuf,
    pub root: PathBuf,
    pub rebon_exe: PathBuf,
    pub launch_gate: LaunchGate,
    pub owner: LedgerOwner,
    pub channel: bool,
}

// ── requests ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartRequest {
    pub prompt: String,
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobRequest {
    pub job_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResultRequest {
    pub job_id: String,
    pub max_chars: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyRequest {
    pub job_id: String,
    pub text: Option<String>,
    pub query_id: Option<u64>,
    pub answers: Option<Vec<AnswerRequest>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnswerRequest {
    /// 1-based option numbers, as `job_status` lists them.
    #[serde(default)]
    pub selected: Vec<usize>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PermitRequest {
    pub job_id: String,
    pub query_id: u64,
    pub option_id: String,
}

/// What the watcher saw of one job on one tick.
#[derive(Debug, Default)]
pub(crate) struct Observation {
    /// No longer a job this server pushes for: removed, or its ledger gone.
    pub gone: bool,
    /// Queued, running, or parked: worth watching at the active cadence.
    pub active: bool,
    /// Something not yet pushed.
    pub update: Option<Update>,
}

impl Desk {
    pub(crate) fn new(config: DeskConfig) -> Self {
        Self {
            store: config.store,
            projects_root: config.projects_root,
            root: config.root,
            rebon_exe: config.rebon_exe,
            launch_gate: config.launch_gate,
            owner: config.owner,
            channel: config.channel,
            tracked: Mutex::new(BTreeSet::new()),
            woken: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn channel_declared(&self) -> bool {
        self.channel
    }

    fn track(&self, job_id: &str) {
        self.tracked
            .lock()
            .expect("tracked jobs poisoned")
            .insert(job_id.to_string());
        self.woken.notify_one();
    }

    pub(crate) fn tracked(&self) -> Vec<String> {
        self.tracked
            .lock()
            .expect("tracked jobs poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub(crate) fn untrack(&self, job_id: &str) {
        self.tracked
            .lock()
            .expect("tracked jobs poisoned")
            .remove(job_id);
    }

    /// Take over the jobs a previous server under this root left behind.
    pub(crate) fn adopt_orphans(&self, now_ms: u64) -> Vec<String> {
        let adopted = ledger::adopt_orphans(
            &self.store,
            &self.root,
            &self.owner,
            now_ms,
            ADOPTION_WINDOW_MS,
            ledger::owner_is_alive,
        );
        for job_id in &adopted {
            self.track(job_id);
        }
        adopted
    }

    // ── exec_start ───────────────────────────────────────────────────

    /// Start a job. The only road to a new job is this call, made by the
    /// client: nothing this server *receives* — a channel notification
    /// included — reaches it. A server that turned
    /// inbound text into prompts would let whatever can post to a channel run
    /// commands unattended.
    pub(crate) fn start(&self, request: StartRequest, now_ms: u64) -> anyhow::Result<Value> {
        let prompt = request.prompt.trim().to_string();
        if prompt.is_empty() {
            anyhow::bail!("`prompt` is empty");
        }
        let cwd = self.resolve_cwd(request.cwd.as_deref())?;
        let runtime = runtime_fields(
            trimmed(request.provider),
            trimmed(request.model),
            trimmed(request.permission_mode),
        );
        let gate = Arc::clone(&self.launch_gate);
        let state = rebon_session_host::launch_background_prompt(
            &self.store,
            // `--bg`'s own options, so a job started here behaves like one
            // started from a terminal: isolated in a worktree where the
            // checkout allows, merged back when its turn succeeds.
            BackgroundLaunchOptions {
                prompt,
                images: Vec::new(),
                cwd,
                isolate_in_worktree: true,
                require_worktree: false,
                preserve_worktree_on_success: false,
                queue_session: false,
                runtime,
                name: trimmed(request.name),
                agent_type: trimmed(request.agent),
                parent_job_id: None,
            },
            &self.rebon_exe,
            move |runtime| gate(runtime),
        )?;
        let job_id = state.job_id().to_string();
        if let Err(error) = ledger::create(&self.store, &job_id, &self.root, &self.owner, now_ms) {
            // A job this surface can neither show nor cancel must not run on
            // unattended behind the client's back.
            let stopped = rebon_session_host::stop_background_job(&self.store, &job_id);
            let aftermath = match stopped {
                Ok(_) => "it has been stopped".to_string(),
                Err(stop_error) => {
                    format!("stopping it failed too ({stop_error}); run `rebon stop {job_id}`")
                }
            };
            return Err(error.context(format!(
                "job {job_id} started but could not be recorded for this server; {aftermath}"
            )));
        }
        self.track(&job_id);
        Ok(json!({
            "job_id": job_id,
            "state": state.status().as_str(),
            "cwd": state.cwd(),
            "channel": if self.channel { "declared" } else { "disabled" },
        }))
    }

    /// The directory a job runs in: this server's root, or a directory
    /// inside it. Anything else is refused rather than clamped — a job asked
    /// to run elsewhere must not silently run here instead (§8, fail-closed).
    fn resolve_cwd(&self, requested: Option<&str>) -> anyhow::Result<PathBuf> {
        let Some(raw) = requested.map(str::trim).filter(|raw| !raw.is_empty()) else {
            return Ok(self.root.clone());
        };
        let path = Path::new(raw);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let resolved =
            canonical_dir(&joined).with_context(|| format!("`cwd` {raw} is not a directory"))?;
        if !rebon_tool::path_scope::path_is_within_root(&resolved, &self.root) {
            anyhow::bail!(
                "`cwd` {} is outside {}, the directory this server was started in; \
                 a job can only run inside it",
                resolved.display(),
                self.root.display()
            );
        }
        Ok(resolved)
    }

    // ── job_status ───────────────────────────────────────────────────

    pub(crate) fn status(&self, job_id: &str) -> anyhow::Result<Value> {
        let (state, _) = self.open(job_id)?;
        Ok(status_view(&state))
    }

    /// A job this server may act on, as the host currently records it.
    ///
    /// Only jobs started through this surface: a client of `rebon mcp serve`
    /// cannot read the user's other Rebon sessions by guessing ids. A dead
    /// worker is reconciled first, so a job whose process vanished reads as
    /// failed rather than running forever.
    fn open(&self, job_id: &str) -> anyhow::Result<(BackgroundJobState, JobLedger)> {
        let job_id = job_id.trim();
        rebon_session_host::validate_job_id(job_id)
            .map_err(|_| anyhow::anyhow!("`{job_id}` is not a job id"))?;
        let Some(ledger) = ledger::read(&self.store, job_id)? else {
            if self.store.state_path(job_id).is_file() {
                anyhow::bail!(
                    "job {job_id} was not started through `rebon mcp serve`, so it is not \
                     visible here; use the rebon CLI (`rebon logs {job_id}`)"
                );
            }
            anyhow::bail!("there is no job {job_id}");
        };
        let mut state = self.store.read_state(job_id)?;
        self.store.reconcile_stale_pid(&mut state)?;
        Ok((state, ledger))
    }

    // ── job_result ───────────────────────────────────────────────────

    pub(crate) fn result(&self, request: ResultRequest) -> anyhow::Result<Value> {
        let (state, _) = self.open(&request.job_id)?;
        if is_active(state.status()) {
            anyhow::bail!(
                "job {} is still {} — its result is not ready. Wait for its channel \
                 message, or check job_status",
                state.job_id(),
                state.status().as_str()
            );
        }
        let written = result::write_result(&self.store, &self.projects_root, &state)?;
        let max_chars = request
            .max_chars
            .unwrap_or(DEFAULT_PREVIEW_CHARS)
            .clamp(MIN_PREVIEW_CHARS, MAX_PREVIEW_CHARS);
        let (summary, truncated) = result::preview(&written.text, max_chars);
        Ok(json!({
            "job_id": state.job_id(),
            "state": state.status().as_str(),
            "turn": state.turn_generation(),
            "result_path": written.path,
            "chars": written.text.chars().count(),
            "truncated": truncated,
            "summary": summary,
        }))
    }

    // ── job_cancel ───────────────────────────────────────────────────

    /// Stop a job and everything it started. The stop is recorded as already
    /// pushed first, so the client that asked for it does not get a channel
    /// message telling it what it just did.
    pub(crate) fn cancel(&self, job_id: &str, now_ms: u64) -> anyhow::Result<Value> {
        let (mut state, _) = self.open(job_id)?;
        if state.status() == BackgroundJobStatus::Stopped {
            return Ok(json!({ "ok": true, "job_id": state.job_id(), "already": "stopped" }));
        }
        let key = format!(
            "{}:{}",
            state.turn_generation(),
            BackgroundJobStatus::Stopped.as_str()
        );
        ledger::claim_delivery(&self.store, state.job_id(), &key, now_ms)?;
        let tree = rebon_session_host::stop_background_job_tree_in_store(&self.store, &mut state)?;
        Ok(json!({
            "ok": tree.failed_children.is_empty(),
            "job_id": state.job_id(),
            "stopped_children": tree.stopped_children,
            "failed_children": tree
                .failed_children
                .iter()
                .map(|(child, error)| json!({ "job_id": child, "error": error }))
                .collect::<Vec<_>>(),
        }))
    }

    // ── job_reply ────────────────────────────────────────────────────

    /// Answer the question a job is parked on, or — when it is not parked —
    /// send it a follow-up that runs as its next turn.
    pub(crate) fn reply(&self, request: ReplyRequest) -> anyhow::Result<Value> {
        let (state, ledger) = self.open(&request.job_id)?;
        let job_id = state.job_id().to_string();
        let answered = match state.pending_permission() {
            Some(pending) => self.answer_question(&state, pending, request)?,
            None => {
                if request.answers.is_some() || request.query_id.is_some() {
                    anyhow::bail!(
                        "job {job_id} is not waiting on a question; send `text` alone to \
                         give it a follow-up turn"
                    );
                }
                let text = trimmed(request.text)
                    .ok_or_else(|| anyhow::anyhow!("`text` is required for a follow-up"))?;
                rebon_session_host::reply_to_background_job(
                    &self.store,
                    &job_id,
                    text,
                    &self.rebon_exe,
                )?;
                "follow_up_queued"
            }
        };
        self.watch_again(&job_id, &ledger);
        Ok(json!({ "ok": true, "job_id": job_id, "delivered": answered }))
    }

    fn answer_question(
        &self,
        state: &BackgroundJobState,
        pending: &BackgroundPermissionQuerySnapshot,
        request: ReplyRequest,
    ) -> anyhow::Result<&'static str> {
        let job_id = state.job_id();
        let Some(questions) = rebon_session_host::ask_user_questions_from_permission(pending)
        else {
            anyhow::bail!(
                "job {job_id} is waiting on a permission decision for {}, not a question; \
                 answer it with job_permit",
                pending.tool.as_deref().unwrap_or("a tool")
            );
        };
        ensure_current_query(job_id, pending, request.query_id)?;
        let answers = match (request.answers, trimmed(request.text)) {
            (Some(answers), None) => answers
                .into_iter()
                .map(to_question_answer)
                .collect::<anyhow::Result<Vec<_>>>()?,
            // One question and a plain sentence: that sentence is the answer.
            (None, Some(text)) if questions.len() == 1 => vec![ForegroundQuestionAnswer {
                selected_options: Vec::new(),
                other_text: Some(text),
            }],
            (None, Some(_)) => anyhow::bail!(
                "job {job_id} asked {} questions; answer them with `answers`, one entry each",
                questions.len()
            ),
            (Some(_), Some(_)) => {
                anyhow::bail!("give either `answers` or `text`, not both")
            }
            (None, None) => anyhow::bail!("`answers` or `text` is required to answer"),
        };
        self.store.answer_question_query_for_target(
            job_id,
            pending.query_id,
            Some(pending.turn_generation),
            pending.endpoint.as_ref(),
            answers,
        )?;
        Ok("question_answered")
    }

    // ── job_permit ───────────────────────────────────────────────────

    /// Answer a job's permission prompt with one of the options it offered.
    ///
    /// Fail-closed three ways: the query must be the one pending now (so an
    /// answer meant for an earlier prompt cannot land on a later one), the
    /// option must be one the prompt offered, and it must not persist a rule.
    /// "Always allow" writes to the user's settings; that is the user's to
    /// grant in Rebon itself, not a remote client's.
    pub(crate) fn permit(&self, request: PermitRequest) -> anyhow::Result<Value> {
        let (state, ledger) = self.open(&request.job_id)?;
        let job_id = state.job_id().to_string();
        let Some(pending) = state.pending_permission() else {
            anyhow::bail!("job {job_id} is not waiting on a permission decision");
        };
        if rebon_session_host::ask_user_questions_from_permission(pending).is_some() {
            anyhow::bail!("job {job_id} is waiting on a question; answer it with job_reply");
        }
        ensure_current_query(&job_id, pending, Some(request.query_id))?;
        let Some(option) = pending
            .options
            .iter()
            .find(|option| option.option_id == request.option_id)
        else {
            anyhow::bail!(
                "`{}` is not an option of this prompt; it offers {}",
                request.option_id,
                answerable_options(pending)
                    .map(|option| option.option_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        if !is_answerable(option) {
            anyhow::bail!(
                "`{}` would persist a permission rule; only one-time answers can be given \
                 here — grant a lasting rule in Rebon itself",
                option.option_id
            );
        }
        self.store
            .answer_permission_query_for_target_with_updated_input(
                &job_id,
                pending.query_id,
                Some(pending.turn_generation),
                pending.endpoint.as_ref(),
                Some(option.option_id.clone()),
                None,
                None,
            )?;
        self.watch_again(&job_id, &ledger);
        Ok(json!({ "ok": true, "job_id": job_id, "option_id": option.option_id }))
    }

    /// A job someone answered through this server is one this server should
    /// be telling about: take it over if its owner is gone, and watch it.
    fn watch_again(&self, job_id: &str, ledger: &JobLedger) {
        if ledger.owner == self.owner {
            self.track(job_id);
            return;
        }
        match ledger::adopt(&self.store, job_id, &self.owner, ledger::owner_is_alive) {
            Ok(true) => self.track(job_id),
            Ok(false) => {}
            Err(error) => tracing::warn!(%job_id, %error, "rebon mcp: could not take over a job"),
        }
    }

    // ── the watcher's half ───────────────────────────────────────────

    /// What one job looks like now, and what about it has not been pushed.
    pub(crate) fn observe(&self, job_id: &str) -> Observation {
        let ledger = match ledger::read(&self.store, job_id) {
            Ok(Some(ledger)) => ledger,
            Ok(None) => {
                return Observation {
                    gone: true,
                    ..Observation::default()
                }
            }
            Err(error) => {
                tracing::warn!(%job_id, %error, "rebon mcp: could not read a job ledger");
                return Observation::default();
            }
        };
        let mut state = match self.store.read_state(job_id) {
            Ok(state) => state,
            Err(error) => {
                let gone = !self.store.state_path(job_id).is_file();
                if !gone {
                    tracing::warn!(%job_id, %error, "rebon mcp: could not read a job");
                }
                return Observation {
                    gone,
                    ..Observation::default()
                };
            }
        };
        if let Err(error) = self.store.reconcile_stale_pid(&mut state) {
            tracing::warn!(%job_id, %error, "rebon mcp: could not reconcile a job's worker");
        }
        let active = is_active(state.status());
        let update = update_for(&state).filter(|update| !ledger.was_delivered(&update.key()));
        let update = update.map(|update| self.with_result(update, &state));
        Observation {
            gone: false,
            active,
            update,
        }
    }

    /// A finished turn is pushed with the path of a result file that exists
    /// by the time the message arrives.
    fn with_result(&self, update: Update, state: &BackgroundJobState) -> Update {
        let Update::Settled {
            job_id,
            state: status,
            turn,
            duration_ms,
            exit_code,
            result_path: _,
        } = update
        else {
            return update;
        };
        let result_path = if matches!(
            status,
            BackgroundJobStatus::Succeeded | BackgroundJobStatus::Failed
        ) {
            match result::write_result(&self.store, &self.projects_root, state) {
                Ok(written) => Some(written.path),
                Err(error) => {
                    tracing::warn!(%job_id, %error, "rebon mcp: could not write a result file");
                    None
                }
            }
        } else {
            None
        };
        Update::Settled {
            job_id,
            state: status,
            turn,
            duration_ms,
            exit_code,
            result_path,
        }
    }

    /// Keep the updates in `batch` that nobody has pushed yet, recording each
    /// as pushed. The watcher writes exactly what this returns.
    pub(crate) fn claim(&self, batch: Vec<Update>, now_ms: u64) -> Vec<Update> {
        batch
            .into_iter()
            .filter(|update| {
                let Some(job_id) = update.job_id() else {
                    return true;
                };
                match ledger::claim_delivery(&self.store, job_id, &update.key(), now_ms) {
                    Ok(claimed) => claimed,
                    Err(error) => {
                        tracing::warn!(%job_id, %error, "rebon mcp: could not record a push");
                        false
                    }
                }
            })
            .collect()
    }
}

/// What about this job is worth a push, ignoring whether it was pushed.
fn update_for(state: &BackgroundJobState) -> Option<Update> {
    let job_id = state.job_id().to_string();
    let turn = state.turn_generation();
    let status = state.status();
    if let Some(pending) = state.pending_permission() {
        if matches!(
            status,
            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
        ) {
            let kind = if rebon_session_host::ask_user_questions_from_permission(pending).is_some()
            {
                Pending::Question
            } else {
                Pending::Permission {
                    tool: pending.tool.clone().unwrap_or_default(),
                }
            };
            return Some(Update::NeedsInput {
                job_id,
                turn,
                query_id: pending.query_id,
                pending: kind,
            });
        }
    }
    let settled = match status {
        BackgroundJobStatus::Succeeded
        | BackgroundJobStatus::Failed
        | BackgroundJobStatus::Stopped => true,
        // Idle after a turn ran is a turn that was cancelled; idle before any
        // turn is a job nobody has asked anything yet.
        BackgroundJobStatus::Idle => turn > 0,
        BackgroundJobStatus::Queued
        | BackgroundJobStatus::Running
        | BackgroundJobStatus::NeedsInput => false,
    };
    settled.then(|| Update::Settled {
        job_id,
        state: status,
        turn,
        duration_ms: duration_ms(state),
        exit_code: state.exit_code(),
        result_path: None,
    })
}

fn is_active(status: BackgroundJobStatus) -> bool {
    matches!(
        status,
        BackgroundJobStatus::Queued
            | BackgroundJobStatus::Running
            | BackgroundJobStatus::NeedsInput
    )
}

fn duration_ms(state: &BackgroundJobState) -> Option<u64> {
    match (state.started_at_ms(), state.completed_at_ms()) {
        (Some(started), Some(completed)) => Some(completed.saturating_sub(started)),
        _ => None,
    }
}

fn status_view(state: &BackgroundJobState) -> Value {
    let mut view = json!({
        "job_id": state.job_id(),
        "state": state.status().as_str(),
        "turn": state.turn_generation(),
        "name": state.name(),
        "cwd": state.cwd(),
        "created_at_ms": state.created_at_ms(),
        "started_at_ms": state.started_at_ms(),
        "completed_at_ms": state.completed_at_ms(),
        // Every event the worker records bumps this, so it is the heartbeat.
        "last_heartbeat_ms": state.updated_at_ms(),
        "duration_ms": duration_ms(state),
        "exit_code": state.exit_code(),
        "error": state.error(),
        "summary": state.summary(),
        "pending": pending_view(state.pending_permission()),
    });
    if let Some(retry) = state.retry() {
        view["retry"] = json!(retry.to_string());
    }
    if let Some(worktree) = state.worktree_path() {
        view["worktree"] = json!(worktree);
    }
    view
}

fn pending_view(pending: Option<&BackgroundPermissionQuerySnapshot>) -> Value {
    let Some(pending) = pending else {
        return Value::Null;
    };
    if let Some(questions) = rebon_session_host::ask_user_questions_from_permission(pending) {
        let questions: Vec<Value> = questions
            .iter()
            .map(|question| {
                json!({
                    "header": question.header,
                    "question": question.question,
                    "multi_select": question.multi_select,
                    "options": question
                        .options
                        .iter()
                        .enumerate()
                        .map(|(index, option)| json!({
                            "number": index + 1,
                            "label": option.label,
                            "description": option.description,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        return json!({
            "kind": "question",
            "query_id": pending.query_id,
            "questions": questions,
            "answer_with": "job_reply",
        });
    }
    json!({
        "kind": "permission",
        "query_id": pending.query_id,
        "tool": pending.tool,
        "title": pending.title,
        "message": pending.message,
        "input": pending.tool_input.as_ref().map(input_preview),
        "options": answerable_options(pending)
            .map(|option| json!({
                "option_id": option.option_id,
                "label": option.label,
                "kind": option.kind,
            }))
            .collect::<Vec<_>>(),
        "answer_with": "job_permit",
    })
}

/// The options `job_permit` accepts: the prompt's own, less any that would
/// persist a rule, and less any whose kind this build cannot read — an
/// unknown kind might be a lasting grant spelled a new way.
fn answerable_options(
    pending: &BackgroundPermissionQuerySnapshot,
) -> impl Iterator<Item = &BackgroundPermissionOptionSnapshot> {
    pending
        .options
        .iter()
        .filter(|option| is_answerable(option))
}

fn is_answerable(option: &BackgroundPermissionOptionSnapshot) -> bool {
    use rebon_proto::PermissionOptionKind as Kind;
    matches!(
        rebon_session_host::parse_permission_option_kind(&option.kind),
        Some(Kind::AllowOnce | Kind::RejectOnce | Kind::RejectAlways)
    )
}

fn input_preview(input: &Value) -> Value {
    let text = input.to_string();
    if text.chars().count() <= MAX_INPUT_PREVIEW_CHARS {
        return input.clone();
    }
    let cut: String = text.chars().take(MAX_INPUT_PREVIEW_CHARS).collect();
    json!(format!("{cut}…"))
}

/// An answer names the query it answers, and it must be the one pending now.
fn ensure_current_query(
    job_id: &str,
    pending: &BackgroundPermissionQuerySnapshot,
    query_id: Option<u64>,
) -> anyhow::Result<()> {
    match query_id {
        Some(query_id) if query_id == pending.query_id => Ok(()),
        Some(query_id) => anyhow::bail!(
            "query {query_id} is no longer pending for job {job_id} (the current one is {}); \
             check job_status again",
            pending.query_id
        ),
        None => anyhow::bail!(
            "job {job_id} is waiting on query {}; pass it as `query_id` so the answer \
             cannot land on a different prompt",
            pending.query_id
        ),
    }
}

fn to_question_answer(answer: AnswerRequest) -> anyhow::Result<ForegroundQuestionAnswer> {
    let selected_options = answer
        .selected
        .into_iter()
        .map(|number| {
            number
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("option numbers start at 1"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let other_text = trimmed(answer.text);
    if selected_options.is_empty() && other_text.is_none() {
        anyhow::bail!("each answer needs `selected` option numbers or `text`");
    }
    Ok(ForegroundQuestionAnswer {
        selected_options,
        other_text,
    })
}

fn runtime_fields(
    provider: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
) -> BackgroundRuntimeFields {
    BackgroundRuntimeFields {
        provider,
        model,
        fast_mode: None,
        channels: Vec::new(),
        development_channels: Vec::new(),
        provider_format: None,
        ui_mode: None,
        effort_level: None,
        permission_mode,
        capability_mode: rebon_types::AgentCapabilityMode::Normal,
        settings: Vec::new(),
        add_dirs: Vec::new(),
        plugin_dirs: Vec::new(),
        mcp_configs: Vec::new(),
        strict_mcp_config: false,
    }
}

fn trimmed(value: Option<String>) -> Option<String> {
    value.and_then(|value| rebon_session_host::non_empty_trimmed(&value))
}

/// `path` as a directory with every link resolved, spelled the way a user
/// would write it (no `\\?\` on Windows).
pub(crate) fn canonical_dir(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = rebon_tools_core::strip_windows_verbatim_prefix(
        std::fs::canonicalize(path)
            .with_context(|| format!("{} does not exist", path.display()))?,
    );
    if !canonical.is_dir() {
        anyhow::bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
