//! Foreground (interactive TUI) session control via an on-disk command
//! mailbox + status sidecar.
//!
//! Background *jobs* are controlled over localhost TCP IPC (see
//! [`crate::BackgroundIpcRequest`]). A *foreground* `rebon` TUI owns all of its
//! turn state on its own event-loop stack, so a TCP server would have to bridge
//! every command back across threads into that stack. Instead we mirror the same
//! three commands — inject / stop / answer-permission — over the filesystem,
//! which the single-consumer event loop drains race-free:
//!
//! * `<project_dir>/<session_id>.inbox/<ms>-<seq>-<id>.json` — one
//!   [`ForegroundCommandEnvelope`] per file. The controller (the desktop app)
//!   creates them with a temp-file + rename so the loop never reads a partial
//!   write; the foreground loop atomically claims, reads, applies, acknowledges,
//!   and only then deletes them. Unfinished claims are recovered after restart.
//!   The envelope carries a protocol `version` (readers skip versions they don't
//!   understand), a `command_id` (so the consumer can de-duplicate and
//!   acknowledge), and `created_at_ms`. Filenames embed `created_at_ms` + a
//!   per-process sequence so a lexical sort replays them in submission order.
//! * `<project_dir>/<session_id>.live.json` — a [`ForegroundSessionStatus`] the
//!   foreground loop publishes (heartbeat + the currently-pending permission +
//!   the last command it acknowledged) so the controller can render controls and
//!   a permission prompt for a live session it can otherwise only read.
//!
//! Liveness itself is *not* tracked here. A foreground session already holds an
//! OS advisory lock (`rebon_session::SessionActiveLock` / `rebon_session::is_session_active`),
//! and that flock is the authoritative "is this session alive" signal; the
//! controller additionally treats a stale `heartbeat_ms` as dead.
//!
//! This is a deliberately narrow *local* transport. It is structured as a thin,
//! replaceable adapter: a session hosted in a worker serves the same
//! command/status semantics over its endpoint (`SessionStatusSnapshot` and the
//! owner IPC), and `owner::foreground_status_from_owner` projects that answer
//! onto [`ForegroundSessionStatus`] so readers see one shape.
//!
//! **Retirement withdrawn — kept indefinitely.** An earlier plan scheduled this
//! module for deletion once hosted sessions had been the default for a release;
//! on 8/31 hosted went back to opt-in (`--hosted`), so a plain `rebon` holds
//! its own lock and publishes no endpoint, and this mailbox is the desktop
//! app's only path to that default session shape. Revisit only after the
//! mirror's control and presentation faces are done, hosted is the default
//! again, and one release cycle has passed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::{now_ms, BackgroundImageAttachment, BackgroundPermissionQuerySnapshot};

/// On-disk command-protocol version. A reader skips envelopes whose `version`
/// exceeds this (forward-compat) instead of misinterpreting them.
///
/// Adding a *variant* to [`ForegroundCommand`] must not bump this. The envelope
/// shape is unchanged, and a reader that predates the variant already skips
/// exactly that one file — an unknown `kind` fails to deserialize, which
/// [`drain_foreground_commands`] handles per entry. Bumping instead makes an
/// older CLI drop *every* command a newer app queues (the gate is
/// `version <= FOREGROUND_PROTOCOL_VERSION`), and the app and the CLI ship as
/// separate releases, so "new app + old CLI" is a routine combination. Bump this
/// only for a change to the envelope itself.
pub const FOREGROUND_PROTOCOL_VERSION: u32 = 1;

/// Monotonic per-process counter so commands queued within the same millisecond
/// still sort in submission order by filename.
static COMMAND_SEQ: AtomicU64 = AtomicU64::new(0);

/// A command delivered to a *live foreground* TUI session through its on-disk
/// mailbox. Mirrors [`crate::BackgroundIpcRequest`] semantics, but over files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForegroundCommand {
    /// Inject a user prompt as if it were typed and submitted. Queues behind
    /// the in-flight turn if one is running (same as the background `Reply`).
    Inject {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_message_uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<BackgroundImageAttachment>,
    },
    /// Run a slash command against the live session. The foreground runner
    /// returns the command's presentation payload through the result sidecar.
    RunCommand {
        name: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Interrupt the in-flight turn — the equivalent of the user pressing Esc /
    /// Ctrl-C, but a hard cancel (never an "undo back to the composer draft").
    Stop,
    /// Answer the currently-pending permission prompt. `query_id` guards against
    /// answering a stale prompt that was already replaced; `option_id` selects
    /// an option (`None` = cancel / deny). `extra_text` carries optional
    /// free-form feedback.
    AnswerPermission {
        query_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra_text: Option<String>,
    },
    /// Answer an interactive AskUserQuestion prompt. `answers` is parallel to the
    /// published `ask_user_questions` (one entry per question).
    AnswerQuestions {
        query_id: u64,
        answers: Vec<ForegroundQuestionAnswer>,
    },
    /// Switch the live session's permission mode, as the local Shift+Tab cycle
    /// and the settings dialog do. `mode` is a `PermissionMode` wire value.
    ///
    /// A background job takes this through its job state instead, where it
    /// applies on the next turn because the worker rebuilds its session per
    /// turn. A foreground TUI holds one long-lived session, so the mode has to
    /// arrive as a command and land on the live mode cell to take effect at all.
    SetPermissionMode { mode: String },
}

/// One option of an interactive question, mirrored from the engine's
/// `AskUserQuestionOption` so the controller can render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ForegroundQuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One interactive question (AskUserQuestion), mirrored for the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ForegroundQuestion {
    pub header: String,
    pub question: String,
    pub multi_select: bool,
    pub options: Vec<ForegroundQuestionOption>,
}

/// The controller's answer to one interactive question: chosen option indices
/// plus optional free-form text. With no selected option the text is an Other
/// answer; with selected options it is supplemental notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ForegroundQuestionAnswer {
    #[serde(default)]
    pub selected_options: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_text: Option<String>,
}

pub fn ask_user_questions_from_permission(
    permission: &BackgroundPermissionQuerySnapshot,
) -> Option<Vec<ForegroundQuestion>> {
    if permission.tool.as_deref() != Some("AskUserQuestion") {
        return None;
    }
    let questions = permission
        .tool_input
        .as_ref()?
        .get("questions")?
        .as_array()?;
    if !(1..=4).contains(&questions.len()) {
        return None;
    }
    questions
        .iter()
        .map(|question| {
            let options = question.get("options")?.as_array()?;
            if !(2..=4).contains(&options.len()) {
                return None;
            }
            let options = options
                .iter()
                .map(|option| {
                    Some(ForegroundQuestionOption {
                        label: option.get("label")?.as_str()?.to_string(),
                        description: option.get("description")?.as_str()?.to_string(),
                        // An explicit `"preview": null` means "no preview", same
                        // as an absent key — it must not fail the whole prompt.
                        preview: match option.get("preview") {
                            None | Some(serde_json::Value::Null) => None,
                            Some(preview) => Some(preview.as_str()?.to_string()),
                        },
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(ForegroundQuestion {
                header: question.get("header")?.as_str()?.to_string(),
                question: question.get("question")?.as_str()?.to_string(),
                // Same null tolerance as `preview`: `"multiSelect": null` reads
                // as the default (false), matching the TUI-side parser.
                multi_select: match question.get("multiSelect") {
                    None | Some(serde_json::Value::Null) => false,
                    Some(value) => value.as_bool()?,
                },
                options,
            })
        })
        .collect()
}

pub fn build_ask_user_question_updated_input(
    permission: &BackgroundPermissionQuerySnapshot,
    answers: &[ForegroundQuestionAnswer],
) -> anyhow::Result<serde_json::Value> {
    let questions = ask_user_questions_from_permission(permission)
        .ok_or_else(|| anyhow::anyhow!("permission query is not a valid AskUserQuestion prompt"))?;
    if questions.len() != answers.len() {
        anyhow::bail!(
            "question answer count {} does not match prompt count {}",
            answers.len(),
            questions.len()
        );
    }

    let mut input = permission
        .tool_input
        .clone()
        .ok_or_else(|| anyhow::anyhow!("AskUserQuestion prompt has no tool input"))?;
    let mut answer_map = serde_json::Map::new();
    let mut annotations_map = serde_json::Map::new();

    for (question, answer) in questions.iter().zip(answers) {
        let mut selected_options = answer.selected_options.clone();
        selected_options.sort_unstable();
        if selected_options.windows(2).any(|pair| pair[0] == pair[1]) {
            anyhow::bail!("question answer contains duplicate option indices");
        }
        if !question.multi_select && selected_options.len() > 1 {
            anyhow::bail!("single-select question cannot select multiple options");
        }
        if selected_options
            .iter()
            .any(|index| *index >= question.options.len())
        {
            anyhow::bail!("question answer contains an out-of-range option index");
        }
        let other_text = answer.other_text.as_deref().unwrap_or("").trim();
        if selected_options.is_empty() && other_text.is_empty() {
            anyhow::bail!("question answer must select an option or provide other text");
        }

        let labels = selected_options
            .iter()
            .map(|index| question.options[*index].label.clone())
            .collect::<Vec<_>>();
        let answer_text = if labels.is_empty() {
            other_text.to_string()
        } else if question.multi_select {
            labels.join(", ")
        } else {
            labels[0].clone()
        };
        answer_map.insert(
            question.question.clone(),
            serde_json::Value::String(answer_text),
        );

        let mut annotation = serde_json::Map::new();
        if !question.multi_select {
            if let Some(index) = selected_options.first() {
                if let Some(preview) = question.options[*index].preview.as_ref() {
                    annotation.insert("preview".into(), serde_json::Value::String(preview.clone()));
                }
            }
        }
        if !labels.is_empty() && !other_text.is_empty() {
            annotation.insert(
                "notes".into(),
                serde_json::Value::String(other_text.to_string()),
            );
        }
        if !annotation.is_empty() {
            annotations_map.insert(
                question.question.clone(),
                serde_json::Value::Object(annotation),
            );
        }
    }

    let Some(object) = input.as_object_mut() else {
        anyhow::bail!("AskUserQuestion tool input must be an object");
    };
    object.insert("answers".into(), serde_json::Value::Object(answer_map));
    if !annotations_map.is_empty() {
        object.insert(
            "annotations".into(),
            serde_json::Value::Object(annotations_map),
        );
    }
    Ok(input)
}

/// Final application outcome for a mailbox command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForegroundCommandOutcome {
    Applied,
    AlreadyResolved,
    Rejected,
}

/// A bounded application-level acknowledgement published in the live status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForegroundCommandResult {
    pub command_id: String,
    pub processed_at_ms: u64,
    pub outcome: ForegroundCommandOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Payload returned by a foreground `RunCommand` request. Kept in a separate
/// sidecar so adding command results does not enlarge the heartbeat status file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForegroundCommandResponse {
    pub command_id: String,
    pub processed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<crate::CommandOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Versioned, identified wrapper around a [`ForegroundCommand`] as written to a
/// mailbox file. `command_id` lets the consumer de-duplicate (idempotency) and
/// acknowledge (via the status sidecar); `created_at_ms` records submit time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForegroundCommandEnvelope {
    pub version: u32,
    pub command_id: String,
    pub created_at_ms: u64,
    pub command: ForegroundCommand,
}

/// Status a live foreground session publishes so an external controller can
/// render controls (and a permission prompt) for it. Authoritative liveness is
/// the session's OS advisory lock, with `heartbeat_ms` as a freshness hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForegroundSessionStatus {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    /// Wall-clock ms of the last status publish — a coarse freshness hint.
    pub heartbeat_ms: u64,
    /// `true` while a model turn is in flight (so the controller can show a Stop
    /// affordance instead of a Send one).
    pub busy: bool,
    /// The permission prompt currently blocking the session, if any. Reuses the
    /// background snapshot shape so the controller renders both identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission: Option<BackgroundPermissionQuerySnapshot>,
    /// When the pending permission is an interactive AskUserQuestion, the parsed
    /// questions for the controller to render. `pending_permission.query_id` is
    /// still the id to answer against (via `ForegroundCommand::AnswerQuestions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user_questions: Option<Vec<ForegroundQuestion>>,
    /// `command_id` of the most recently processed mailbox command (ack), so the
    /// controller can confirm delivery / surface errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_id: Option<String>,
    /// Wall-clock ms when `last_command_id` was processed.
    #[serde(default)]
    pub last_command_at_ms: u64,
    /// Error from processing `last_command_id`, if it could not be applied
    /// (e.g. an answer for a stale permission). `None` = applied cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_error: Option<String>,
    /// Recent application-level acknowledgements. A bounded history is required
    /// because multiple producers may enqueue commands before the next status poll.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_command_results: Vec<ForegroundCommandResult>,
}

/// `<project_dir>/<session_id>.inbox` — the command mailbox directory.
pub fn foreground_inbox_dir(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut p = rebon_session::project_dir_path(projects_root, cwd);
    p.push(format!("{session_id}.inbox"));
    p
}

/// Filename suffix of the status sidecar. Exported so a consumer walking a
/// project directory can recognize `<session_id>.live.json` — and recover the
/// session id from it — without re-spelling the format.
pub const FOREGROUND_STATUS_SUFFIX: &str = ".live.json";

/// `<project_dir>/<session_id>.live.json` — the status sidecar.
pub fn foreground_status_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut p = rebon_session::project_dir_path(projects_root, cwd);
    p.push(format!("{session_id}{FOREGROUND_STATUS_SUFFIX}"));
    p
}

/// `<project_dir>/<session_id>.command-results/<command_id>.json`.
pub fn foreground_command_response_path(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    command_id: &str,
) -> PathBuf {
    let mut p = rebon_session::project_dir_path(projects_root, cwd);
    p.push(format!("{session_id}.command-results"));
    p.push(format!("{command_id}.json"));
    p
}

pub fn read_foreground_command_response(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    command_id: &str,
) -> Option<ForegroundCommandResponse> {
    let path = foreground_command_response_path(projects_root, cwd, session_id, command_id);
    let bytes = std::fs::read(&path).ok()?;
    let response = serde_json::from_slice(&bytes).ok()?;
    let _ = std::fs::remove_file(path);
    Some(response)
}

pub fn write_foreground_command_response(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    response: &ForegroundCommandResponse,
) -> std::io::Result<()> {
    let path =
        foreground_command_response_path(projects_root, cwd, session_id, &response.command_id);
    let dir = path.parent().expect("command response path has parent");
    create_dir_all_private(dir)?;
    prune_stale_command_responses(dir);
    let body = serde_json::to_vec(response)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    rebon_session::write_file_atomically(&path, &body)
}

/// How long an unclaimed response sidecar survives. A reader deletes the file it
/// consumed, so what accumulates here is responses nobody came back for — the
/// client timed out (see [`crate::command_response_timeout`]) or died.
/// Comfortably longer than the longest of those waits, so a response still in
/// flight is never swept.
const COMMAND_RESPONSE_TTL: Duration = Duration::from_secs(600);

/// Drop response sidecars left behind by clients that stopped waiting. Runs on
/// the write path, where the directory is already open; best-effort throughout,
/// since failing to reclaim a stale file must never fail the response itself.
fn prune_stale_command_responses(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|x| x.to_str()),
            Some("json" | "tmp")
        ) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age >= COMMAND_RESPONSE_TTL)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn random_id() -> String {
    let mut buf = [0u8; 8];
    let _ = getrandom::getrandom(&mut buf);
    let n = u64::from_le_bytes(buf);
    format!("{n:016x}")
}

/// Create a directory tree, restricting it to the owner on Unix. The mailbox
/// carries control commands (inject / stop / permission answers), so it should
/// not be world-writable; on Windows the parent `~/.rebon` is already
/// user-scoped by the profile ACL.
fn create_dir_all_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

// ─── controller (app) side ─────────────────────────────────────────────────

/// Queue a [`ForegroundCommand`] into a live foreground session's mailbox,
/// returning the generated `command_id`.
///
/// Written via temp-file + atomic rename so the draining loop never observes a
/// partially-written command. Filenames embed `created_at_ms` + a per-process
/// sequence so [`drain_foreground_commands`] replays them in submission order.
pub fn send_foreground_command(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    command: &ForegroundCommand,
) -> std::io::Result<String> {
    let dir = foreground_inbox_dir(projects_root, cwd, session_id);
    create_dir_all_private(&dir)?;
    let command_id = random_id();
    let created_at_ms = now_ms();
    let seq = COMMAND_SEQ.fetch_add(1, Ordering::Relaxed);
    let envelope = ForegroundCommandEnvelope {
        version: FOREGROUND_PROTOCOL_VERSION,
        command_id: command_id.clone(),
        created_at_ms,
        command: command.clone(),
    };
    let final_path = dir.join(format!("{created_at_ms:013}-{seq:020}-{command_id}.json"));
    let body = serde_json::to_vec(&envelope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    rebon_session::write_file_atomically(&final_path, &body)?;
    Ok(command_id)
}

pub fn run_foreground_command(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    name: String,
    args: Vec<String>,
) -> anyhow::Result<crate::CommandOutput> {
    let timeout = crate::command_response_timeout(&name);
    let command_id = send_foreground_command(
        projects_root,
        cwd,
        session_id,
        &ForegroundCommand::RunCommand { name, args },
    )?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(response) =
            read_foreground_command_response(projects_root, cwd, session_id, &command_id)
        {
            return match (response.output, response.error) {
                (Some(output), None) => Ok(output),
                (_, Some(error)) => anyhow::bail!(error),
                _ => anyhow::bail!("foreground command returned no output"),
            };
        }
        if Instant::now() >= deadline {
            anyhow::bail!("foreground command timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Read the status sidecar a live foreground session publishes. `None` when the
/// file is absent (no live session, or it never published) or malformed.
pub fn read_foreground_status(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Option<ForegroundSessionStatus> {
    let bytes = std::fs::read(foreground_status_path(projects_root, cwd, session_id)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ─── foreground (TUI) side ─────────────────────────────────────────────────

/// A durably claimed foreground command. The mailbox entry remains on disk
/// until [`complete_foreground_command`] is called after its acknowledgement has
/// been published. An unfinished `.claimed` entry is returned by the next drain,
/// providing at-least-once delivery across process restarts.
#[derive(Debug)]
pub struct ForegroundCommandClaim {
    pub envelope: ForegroundCommandEnvelope,
    path: PathBuf,
}

/// Claim every queued command from this session's mailbox. Queued `.json` files
/// are atomically renamed to `.claimed` before they are read; existing claims are
/// recovered as well. Returns claims ordered by their queued-at filename prefix.
/// A missing mailbox directory (the common case) returns an empty vec.
///
/// Envelopes with a `version` newer than [`FOREGROUND_PROTOCOL_VERSION`] and
/// malformed files are removed and skipped — a corrupt or future-versioned entry
/// must never wedge the drain.
pub fn drain_foreground_commands(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Vec<ForegroundCommandClaim> {
    let dir = foreground_inbox_dir(projects_root, cwd, session_id);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("json" | "claimed")
            )
        })
        .collect();
    files.sort();
    let mut out = Vec::with_capacity(files.len());
    for queued_path in files {
        let path = if queued_path.extension().and_then(|x| x.to_str()) == Some("json") {
            let claimed_path = queued_path.with_extension("claimed");
            if std::fs::rename(&queued_path, &claimed_path).is_err() {
                continue;
            }
            claimed_path
        } else {
            queued_path
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        match serde_json::from_slice::<ForegroundCommandEnvelope>(&bytes) {
            Ok(envelope) if envelope.version <= FOREGROUND_PROTOCOL_VERSION => {
                out.push(ForegroundCommandClaim { envelope, path });
            }
            Ok(env) => {
                tracing::warn!(
                    path = %path.display(),
                    version = env.version,
                    "rebon: skipping foreground mailbox command with unsupported version"
                );
                let _ = std::fs::remove_file(&path);
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    %err,
                    "rebon: dropping malformed foreground mailbox command"
                );
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out
}

/// Complete a claimed command after its outcome has been persisted and its
/// acknowledgement published. Failure leaves the claim recoverable.
pub fn complete_foreground_command(claim: &ForegroundCommandClaim) -> std::io::Result<()> {
    std::fs::remove_file(&claim.path)
}

/// Publish (create or overwrite) this session's status sidecar. Best-effort:
/// any I/O error is swallowed because the sidecar is purely an aid for an
/// external controller and never affects the session itself. Atomic via
/// temp-file + rename so a reader never sees a half-written status.
pub fn write_foreground_status(projects_root: &Path, status: &ForegroundSessionStatus) -> bool {
    let path = foreground_status_path(projects_root, &status.cwd, &status.session_id);
    if let Some(parent) = path.parent() {
        if create_dir_all_private(parent).is_err() {
            return false;
        }
    }
    let Ok(body) = serde_json::to_vec(status) else {
        return false;
    };
    rebon_session::write_file_atomically(&path, &body).is_ok()
}

/// Remove this session's status sidecar (best-effort) when it shuts down, so the
/// controller stops offering controls for it immediately rather than waiting for
/// the heartbeat-staleness probe to notice the process is gone.
pub fn clear_foreground_status(projects_root: &Path, cwd: &str, session_id: &str) {
    let _ = std::fs::remove_file(foreground_status_path(projects_root, cwd, session_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_root() -> TempDir {
        tempfile::Builder::new()
            .prefix("rebon-fg-test-")
            .tempdir()
            .unwrap()
    }

    fn ask_permission() -> BackgroundPermissionQuerySnapshot {
        BackgroundPermissionQuerySnapshot {
            query_id: 7,
            turn_generation: 0,
            endpoint: None,
            tool: Some("AskUserQuestion".into()),
            tool_call_id: Some("tool-1".into()),
            session_id: Some("session-1".into()),
            title: Some("Answer questions".into()),
            message: None,
            tool_input: Some(serde_json::json!({
                "questions": [
                    {
                        "header": "Mode",
                        "question": "Choose a mode",
                        "multiSelect": false,
                        "options": [
                            {
                                "label": "Fast",
                                "description": "Finish quickly",
                                "preview": "fast-preview"
                            },
                            {
                                "label": "Safe",
                                "description": "Check everything"
                            }
                        ]
                    },
                    {
                        "header": "Checks",
                        "question": "Choose checks",
                        "multiSelect": true,
                        "options": [
                            {"label": "Tests", "description": "Run tests"},
                            {"label": "Lint", "description": "Run lint"}
                        ]
                    }
                ],
                "metadata": {"source": "test"}
            })),
            metadata: None,
            options: vec![crate::BackgroundPermissionOptionSnapshot {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: "AllowOnce".into(),
            }],
        }
    }

    #[test]
    fn ask_user_questions_preserve_structured_options() {
        let questions = ask_user_questions_from_permission(&ask_permission()).unwrap();

        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].header, "Mode");
        assert_eq!(questions[0].options[0].label, "Fast");
        assert_eq!(questions[0].options[0].description, "Finish quickly");
        assert_eq!(
            questions[0].options[0].preview.as_deref(),
            Some("fast-preview")
        );
        assert!(questions[1].multi_select);
    }

    #[test]
    fn ask_user_questions_tolerate_explicit_nulls() {
        // Some engines serialize absent optionals as explicit nulls; the prompt
        // must still parse (matching the TUI-side parser) instead of degrading
        // to a generic approval.
        let mut permission = ask_permission();
        let questions_json = &mut permission.tool_input.as_mut().unwrap()["questions"];
        questions_json[0]["options"][0]["preview"] = serde_json::Value::Null;
        questions_json[0]["multiSelect"] = serde_json::Value::Null;

        let questions = ask_user_questions_from_permission(&permission).unwrap();
        assert_eq!(questions[0].options[0].preview, None);
        assert!(!questions[0].multi_select);
    }

    #[test]
    fn ask_user_question_answers_build_updated_input() {
        let updated = build_ask_user_question_updated_input(
            &ask_permission(),
            &[
                ForegroundQuestionAnswer {
                    selected_options: vec![0],
                    other_text: Some("prefer this".into()),
                },
                ForegroundQuestionAnswer {
                    selected_options: vec![1, 0],
                    other_text: None,
                },
            ],
        )
        .unwrap();

        assert_eq!(updated["answers"]["Choose a mode"], "Fast");
        assert_eq!(updated["answers"]["Choose checks"], "Tests, Lint");
        assert_eq!(
            updated["annotations"]["Choose a mode"]["preview"],
            "fast-preview"
        );
        assert_eq!(
            updated["annotations"]["Choose a mode"]["notes"],
            "prefer this"
        );
        assert_eq!(updated["metadata"]["source"], "test");
    }

    #[test]
    fn ask_user_question_answers_reject_invalid_selections() {
        let permission = ask_permission();
        let incomplete = [
            ForegroundQuestionAnswer {
                selected_options: vec![],
                other_text: None,
            },
            ForegroundQuestionAnswer {
                selected_options: vec![0],
                other_text: None,
            },
        ];
        assert!(build_ask_user_question_updated_input(&permission, &incomplete).is_err());

        let invalid_single = [
            ForegroundQuestionAnswer {
                selected_options: vec![0, 1],
                other_text: None,
            },
            ForegroundQuestionAnswer {
                selected_options: vec![0],
                other_text: None,
            },
        ];
        assert!(build_ask_user_question_updated_input(&permission, &invalid_single).is_err());

        let duplicate = [
            ForegroundQuestionAnswer {
                selected_options: vec![0],
                other_text: None,
            },
            ForegroundQuestionAnswer {
                selected_options: vec![1, 1],
                other_text: None,
            },
        ];
        assert!(build_ask_user_question_updated_input(&permission, &duplicate).is_err());
    }

    #[test]
    fn run_command_waits_for_the_foreground_response_payload() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            run_foreground_command(
                &worker_root,
                "/work/proj",
                "sess-command",
                "context".into(),
                Vec::new(),
            )
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let claim = loop {
            if let Some(claim) =
                drain_foreground_commands(&root, "/work/proj", "sess-command").pop()
            {
                break claim;
            }
            assert!(Instant::now() < deadline, "command was not queued");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            claim.envelope.command,
            ForegroundCommand::RunCommand {
                name: "context".into(),
                args: Vec::new(),
            }
        );
        write_foreground_command_response(
            &root,
            "/work/proj",
            "sess-command",
            &ForegroundCommandResponse {
                command_id: claim.envelope.command_id.clone(),
                processed_at_ms: now_ms(),
                output: Some(crate::CommandOutput {
                    text: "live context".into(),
                    tone: "info".into(),
                }),
                error: None,
            },
        )
        .unwrap();
        complete_foreground_command(&claim).unwrap();

        let output = worker.join().unwrap().unwrap();
        assert_eq!(output.text, "live context");
        assert_eq!(output.tone, "info");
    }

    #[test]
    fn run_command_surfaces_a_foreground_rejection() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            run_foreground_command(
                &worker_root,
                "/work/proj",
                "sess-command-error",
                "compact".into(),
                Vec::new(),
            )
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let claim = loop {
            if let Some(claim) =
                drain_foreground_commands(&root, "/work/proj", "sess-command-error").pop()
            {
                break claim;
            }
            assert!(Instant::now() < deadline, "command was not queued");
            std::thread::sleep(Duration::from_millis(10));
        };
        write_foreground_command_response(
            &root,
            "/work/proj",
            "sess-command-error",
            &ForegroundCommandResponse {
                command_id: claim.envelope.command_id.clone(),
                processed_at_ms: now_ms(),
                output: None,
                error: Some("rejected command".into()),
            },
        )
        .unwrap();
        complete_foreground_command(&claim).unwrap();

        let error = worker.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("rejected command"));
    }

    #[test]
    fn command_round_trips_through_mailbox_in_order() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let cwd = "/work/proj";
        let sid = "sess-1";

        send_foreground_command(
            &root,
            cwd,
            sid,
            &ForegroundCommand::Inject {
                message: "first".into(),
                user_message_uuid: None,
                images: Vec::new(),
            },
        )
        .unwrap();
        send_foreground_command(&root, cwd, sid, &ForegroundCommand::Stop).unwrap();
        send_foreground_command(
            &root,
            cwd,
            sid,
            &ForegroundCommand::Inject {
                message: "third".into(),
                user_message_uuid: None,
                images: Vec::new(),
            },
        )
        .unwrap();

        let drained = drain_foreground_commands(&root, cwd, sid);
        assert_eq!(drained.len(), 3);
        // Submission order is preserved via the filename ms+seq prefix.
        assert_eq!(
            drained[0].envelope.command,
            ForegroundCommand::Inject {
                message: "first".into(),
                user_message_uuid: None,
                images: Vec::new(),
            }
        );
        assert_eq!(drained[1].envelope.command, ForegroundCommand::Stop);
        assert_eq!(
            drained[2].envelope.command,
            ForegroundCommand::Inject {
                message: "third".into(),
                user_message_uuid: None,
                images: Vec::new(),
            }
        );
        // Distinct command ids + correct protocol version.
        assert_eq!(drained[0].envelope.version, FOREGROUND_PROTOCOL_VERSION);
        assert_ne!(
            drained[0].envelope.command_id,
            drained[1].envelope.command_id
        );

        // Claims remain durable until the caller has persisted acknowledgement.
        let recovered = drain_foreground_commands(&root, cwd, sid);
        assert_eq!(recovered.len(), 3);
        for claim in &drained {
            complete_foreground_command(claim).unwrap();
        }
        assert!(drain_foreground_commands(&root, cwd, sid).is_empty());
    }

    #[test]
    fn drain_skips_future_version_and_malformed() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let cwd = "/work/proj";
        let sid = "sess-v";
        let dir = foreground_inbox_dir(&root, cwd, sid);
        std::fs::create_dir_all(&dir).unwrap();

        // A future-versioned envelope must be skipped, not misapplied.
        let future = ForegroundCommandEnvelope {
            version: FOREGROUND_PROTOCOL_VERSION + 1,
            command_id: "future".into(),
            created_at_ms: 1,
            command: ForegroundCommand::Stop,
        };
        std::fs::write(
            dir.join("0000000000001-00000000000000000000-future.json"),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();
        // Malformed JSON must be skipped, not panic.
        std::fs::write(
            dir.join("0000000000002-00000000000000000000-bad.json"),
            b"{not json",
        )
        .unwrap();
        // A valid current-version command still comes through.
        send_foreground_command(&root, cwd, sid, &ForegroundCommand::Stop).unwrap();

        let drained = drain_foreground_commands(&root, cwd, sid);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].envelope.command, ForegroundCommand::Stop);
        assert_eq!(drained[0].envelope.version, FOREGROUND_PROTOCOL_VERSION);
        complete_foreground_command(&drained[0]).unwrap();
    }

    #[test]
    fn protocol_version_is_pinned_across_added_command_kinds() {
        // `RunCommand` — and any future variant — rides on version 1 on purpose.
        // The read gate is `version <= FOREGROUND_PROTOCOL_VERSION`, so bumping
        // it makes an older CLI discard *every* command a newer app queues,
        // including Inject and Stop, which it understands perfectly well.
        assert_eq!(FOREGROUND_PROTOCOL_VERSION, 1);
    }

    /// The new variant must ride on version 1 like every other one, and it must
    /// survive the mailbox round trip — the desktop app queues it and the live
    /// TUI is the only thing that can apply it.
    #[test]
    fn set_permission_mode_round_trips_through_the_mailbox_on_version_one() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let cwd = "/work/proj";
        let sid = "sess-mode";
        let command = ForegroundCommand::SetPermissionMode {
            mode: "bypassPermissions".into(),
        };

        send_foreground_command(&root, cwd, sid, &command).unwrap();

        let drained = drain_foreground_commands(&root, cwd, sid);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].envelope.command, command);
        assert_eq!(drained[0].envelope.version, FOREGROUND_PROTOCOL_VERSION);
        complete_foreground_command(&drained[0]).unwrap();
    }

    #[test]
    fn drain_skips_an_unknown_command_kind_without_dropping_its_neighbours() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let cwd = "/work/proj";
        let sid = "sess-unknown-kind";
        let dir = foreground_inbox_dir(&root, cwd, sid);
        std::fs::create_dir_all(&dir).unwrap();

        let queued = |created_at_ms: u64, body: String| {
            std::fs::write(
                dir.join(format!(
                    "{created_at_ms:013}-00000000000000000000-cmd{created_at_ms}.json"
                )),
                body,
            )
            .unwrap();
        };
        let envelope = |command_id: &str, created_at_ms: u64, command: ForegroundCommand| {
            serde_json::to_string(&ForegroundCommandEnvelope {
                version: FOREGROUND_PROTOCOL_VERSION,
                command_id: command_id.into(),
                created_at_ms,
                command,
            })
            .unwrap()
        };
        queued(1, envelope("first", 1, ForegroundCommand::Stop));
        // What an older CLI sees when a newer app queues a command it predates:
        // the version matches, only the `kind` is unknown. That must cost exactly
        // this one entry, never the batch around it.
        queued(
            2,
            format!(
                r#"{{"version":{FOREGROUND_PROTOCOL_VERSION},"commandId":"unknown","createdAtMs":2,"command":{{"kind":"from_a_newer_app"}}}}"#
            ),
        );
        queued(
            3,
            envelope(
                "third",
                3,
                ForegroundCommand::Inject {
                    message: "still delivered".into(),
                    user_message_uuid: None,
                    images: Vec::new(),
                },
            ),
        );

        let drained = drain_foreground_commands(&root, cwd, sid);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].envelope.command_id, "first");
        assert_eq!(drained[1].envelope.command_id, "third");
    }

    #[test]
    fn writing_a_response_reclaims_the_ones_no_client_came_back_for() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let cwd = "/work/proj";
        let sid = "sess-response-gc";
        let write = |command_id: &str| {
            write_foreground_command_response(
                &root,
                cwd,
                sid,
                &ForegroundCommandResponse {
                    command_id: command_id.into(),
                    processed_at_ms: now_ms(),
                    output: None,
                    error: Some("rejected command".into()),
                },
            )
            .unwrap();
        };

        write("abandoned");
        let abandoned = foreground_command_response_path(&root, cwd, sid, "abandoned");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&abandoned)
            .unwrap()
            .set_modified(SystemTime::now() - COMMAND_RESPONSE_TTL - Duration::from_secs(60))
            .unwrap();

        write("fresh");
        assert!(!abandoned.exists());
        assert!(foreground_command_response_path(&root, cwd, sid, "fresh").exists());
    }

    #[test]
    fn inject_images_round_trip_and_legacy_payload_defaults_empty() {
        let image = BackgroundImageAttachment {
            id: 7,
            data: "base64-data".into(),
            media_type: "image/png".into(),
            filename: Some("shot.png".into()),
            source_path: None,
        };
        let command = ForegroundCommand::Inject {
            message: "inspect".into(),
            user_message_uuid: Some("u-mobile-command".into()),
            images: vec![image.clone()],
        };
        let json = serde_json::to_string(&command).unwrap();
        let restored: ForegroundCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, command);

        let legacy: ForegroundCommand =
            serde_json::from_str(r#"{"kind":"inject","message":"legacy"}"#).unwrap();
        assert_eq!(
            legacy,
            ForegroundCommand::Inject {
                message: "legacy".into(),
                user_message_uuid: None,
                images: Vec::new(),
            }
        );
    }

    #[test]
    fn answer_permission_command_serializes_with_query_id() {
        let cmd = ForegroundCommand::AnswerPermission {
            query_id: 7,
            option_id: Some("allow".into()),
            extra_text: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"kind\":\"answer_permission\""));
        assert!(json.contains("\"query_id\":7"));
        let back: ForegroundCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn status_round_trips_with_ack_and_clears() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let mut status = ForegroundSessionStatus {
            pid: 1234,
            session_id: "sess-2".into(),
            cwd: "/work/proj".into(),
            heartbeat_ms: now_ms(),
            busy: true,
            pending_permission: None,
            ask_user_questions: None,
            last_command_id: Some("cmd-1".into()),
            last_command_at_ms: now_ms(),
            last_command_error: None,
            recent_command_results: vec![ForegroundCommandResult {
                command_id: "cmd-1".into(),
                processed_at_ms: now_ms(),
                outcome: ForegroundCommandOutcome::Applied,
                error: None,
            }],
        };
        write_foreground_status(&root, &status);
        let read = read_foreground_status(&root, &status.cwd, &status.session_id).unwrap();
        assert_eq!(read, status);

        status.last_command_id = Some("cmd-2".into());
        assert!(write_foreground_status(&root, &status));
        let replaced = read_foreground_status(&root, &status.cwd, &status.session_id).unwrap();
        assert_eq!(replaced.last_command_id.as_deref(), Some("cmd-2"));

        clear_foreground_status(&root, &status.cwd, &status.session_id);
        assert!(read_foreground_status(&root, &status.cwd, &status.session_id).is_none());
    }

    /// The status file lives in the project directory, whose name is the whole
    /// cwd — so a deep enough workspace pushes the publish past `MAX_PATH`,
    /// where the hand-written `MoveFileExW` needs the prefix `std::fs` adds on
    /// its own. Before that prefix, this returned `false` and the controller
    /// simply never saw the session.
    ///
    /// Windows only: `MAX_PATH` is a Windows limit, and the test's own guard
    /// — that the publish lands past 260 bytes — cannot hold under a short
    /// Linux `/tmp` root, where the crate now runs in CI too.
    #[cfg(windows)]
    #[test]
    fn status_publishes_from_a_project_directory_past_max_path() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let cwd = format!("/work/{}", "d".repeat(200));
        let status = ForegroundSessionStatus {
            pid: 1234,
            session_id: "sess-deep".into(),
            cwd: cwd.clone(),
            heartbeat_ms: now_ms(),
            busy: false,
            pending_permission: None,
            ask_user_questions: None,
            last_command_id: None,
            last_command_at_ms: 0,
            last_command_error: None,
            recent_command_results: Vec::new(),
        };
        let path = foreground_status_path(&root, &cwd, &status.session_id);
        assert!(
            path.as_os_str().len() > 260,
            "the publish has to land past MAX_PATH for this to prove anything; \
             the status path is {} long",
            path.as_os_str().len()
        );

        assert!(
            write_foreground_status(&root, &status),
            "a session in a deep workspace must still be able to publish its status"
        );
        assert_eq!(
            read_foreground_status(&root, &cwd, &status.session_id).unwrap(),
            status
        );
    }

    #[test]
    fn old_status_without_ack_fields_still_reads() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let cwd = "/work/proj";
        let sid = "sess-old";
        let dir = rebon_session::project_dir_path(&root, cwd);
        std::fs::create_dir_all(&dir).unwrap();
        // A sidecar written before the ack fields existed must still deserialize.
        std::fs::write(
            foreground_status_path(&root, cwd, sid),
            br#"{"pid":1,"sessionId":"sess-old","cwd":"/work/proj","heartbeatMs":5,"busy":false}"#,
        )
        .unwrap();
        let read = read_foreground_status(&root, cwd, sid).unwrap();
        assert_eq!(read.pid, 1);
        assert!(read.last_command_id.is_none());
        assert_eq!(read.last_command_at_ms, 0);
        assert!(read.recent_command_results.is_empty());
    }
}
