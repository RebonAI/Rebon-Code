//! Whether a member can be woken cheaply (RFC-0009 §8.1, §15.1).
//!
//! Waking an agent re-sends its whole context. While its provider still
//! caches that context the cost is small; once the cache has expired, or the
//! context is so long that a re-read is expensive anyway, a group message is
//! not worth it and waits in the inbox. A short context — a fresh session,
//! or one just compacted — is cheap to re-read uncached, so an idle member
//! carrying one stays wakeable past the cache and the group's idle limit.
//! Each agent keeps the facts that tell these apart in its own files, and
//! this module reads them:
//!
//! | agent | file | busy | context |
//! |---|---|---|---|
//! | rebon | `<projects>/*/<session>.jsonl` | last turn not ended | last assistant `usage`, or the size of a newer `<session>.compact.json` |
//! | Claude Code | `<projects>/*/<session>.jsonl` | last turn not ended | last assistant `usage`, or the last compaction's |
//! | Codex | `<codex home>/sessions/Y/M/D/rollout-*-<session>.jsonl` | `task_started` last | `token_count` |
//! | Grok Build | `<grok home>/sessions/*/<session>/` | — | `signals.json` |
//!
//! When the files were last written is when the agent was last active. An
//! agent this table does not know is [`State::Unknown`], which delivery
//! treats as cold: nothing is pushed into a session whose state nobody can
//! read.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::identity::AgentKind;
use crate::model::Warmth;

/// The window assumed when an agent does not record its own.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// How much of a file's end is read for its last entries.
const TAIL_BYTES: u64 = 512 * 1024;

/// A turn left open this long ago is taken to have died with its process.
const STALE_BUSY_MS: u64 = 10 * 60 * 1000;

// Leave a gap between turns rather than spending a compaction call while work resumes.
const COMPACT_IDLE_MS: u64 = 5 * 60 * 1000;

/// A context at most this share of the long-context threshold is short:
/// cheap to re-read uncached. A fresh Claude Code session measures about
/// 45k tokens with its system prompt and tools, and a compaction summary
/// adds 10–20k; two thirds of the default threshold (80k of 120k) holds
/// that with room and stays clear of the contexts compaction is for.
const SHORT_CONTEXT_SHARE: f64 = 2.0 / 3.0;

/// Roughly how many bytes of serialized history make a token, for a size
/// read off a file rather than reported by a model. JSON's own syntax makes
/// it overestimate, which errs toward leaving a member alone.
const BYTES_PER_TOKEN: u64 = 4;

/// Where each agent keeps its sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Homes {
    /// Rebon's `projects` directory.
    pub rebon_projects: Option<PathBuf>,
    /// Claude Code's config directory (`CLAUDE_CONFIG_DIR`, else `~/.claude`).
    pub claude: Option<PathBuf>,
    /// `CODEX_HOME`, else `~/.codex`.
    pub codex: Option<PathBuf>,
    /// `GROK_HOME`, else `~/.grok`.
    pub grok: Option<PathBuf>,
}

impl Homes {
    /// The usual places, given Rebon's config home.
    pub fn from_env(rebon_config_home: &Path) -> Self {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        let under_home = |var: &str, dir: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(dir)))
        };
        Self {
            rebon_projects: Some(rebon_config_home.join("projects")),
            claude: under_home("CLAUDE_CONFIG_DIR", ".claude"),
            codex: under_home("CODEX_HOME", ".codex"),
            grok: under_home("GROK_HOME", ".grok"),
        }
    }
}

/// What a member's own files say about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    /// When its session was last written.
    pub last_activity_ms: Option<u64>,
    /// In the middle of a turn. `None` when its files do not say.
    pub busy: Option<bool>,
    /// The context its last request carried.
    pub context_tokens: Option<u64>,
    /// Its model's window, when it records one.
    pub context_window: Option<u64>,
    /// How long its provider keeps the context cached, when its files say.
    pub cache_ttl_minutes: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Working on a turn: deliver at its next boundary, never wake.
    Busy,
    /// Idle and cheap to wake.
    Warm,
    /// Idle past its cache with more than a short context, or carrying a
    /// long context: leave it be.
    Cold,
    /// Its files could not be read or are not known.
    Unknown,
}

/// Whether the table above says where `agent` keeps its sessions.
pub fn knows(agent: &str) -> bool {
    matches!(
        agent,
        AgentKind::REBON | AgentKind::CLAUDE_CODE | AgentKind::CODEX | AgentKind::GROK
    )
}

/// Where the session `session_id` of `agent` is kept, by the table above:
/// a transcript file, or Grok Build's session directory. `None` for an
/// agent the table does not know or a session that is not there.
pub fn session_path(homes: &Homes, agent: &str, session_id: &str) -> Option<PathBuf> {
    match agent {
        AgentKind::REBON => homes
            .rebon_projects
            .as_deref()
            .and_then(|root| find_transcript(root, session_id)),
        AgentKind::CLAUDE_CODE => homes
            .claude
            .as_deref()
            .and_then(|root| find_transcript(&root.join("projects"), session_id)),
        AgentKind::CODEX => homes
            .codex
            .as_deref()
            .and_then(|root| find_rollout(&root.join("sessions"), session_id)),
        AgentKind::GROK => homes
            .grok
            .as_deref()
            .and_then(|root| find_grok_session(&root.join("sessions"), session_id)),
        _ => None,
    }
}

/// Reads the session `session_id` of `agent`.
pub fn activity(homes: &Homes, agent: &str, session_id: &str) -> Activity {
    let Some(path) = session_path(homes, agent, session_id) else {
        return Activity::default();
    };
    match agent {
        AgentKind::REBON => rebon_activity(&path, session_id),
        AgentKind::CLAUDE_CODE => transcript_activity(&path, true),
        AgentKind::CODEX => rollout_activity(&path),
        AgentKind::GROK => grok_activity(&path),
        _ => unreachable!("session_path finds nothing for an agent it does not know"),
    }
}

/// How `activity` stands against the group's thresholds at `now_ms`.
pub fn state(activity: &Activity, warmth: &Warmth, now_ms: u64) -> State {
    let Some(last) = activity.last_activity_ms else {
        return State::Unknown;
    };
    let idle_ms = now_ms.saturating_sub(last);
    if activity.busy == Some(true) && idle_ms < STALE_BUSY_MS {
        return State::Busy;
    }
    let minutes = activity
        .cache_ttl_minutes
        .map_or(warmth.idle_minutes, |ttl| ttl.min(warmth.idle_minutes));
    // An open turn gone stale may still be working, or dead with its
    // process: only a finished turn is idle enough to wake on a short context.
    let short = activity.busy == Some(false) && short_context(activity, warmth);
    if idle_ms > u64::from(minutes) * 60_000 && !short {
        return State::Cold;
    }
    if long_context(activity, warmth) {
        return State::Cold;
    }
    State::Warm
}

/// Whether an explicitly completed turn has been idle long enough to compact.
/// Unlike wake-up warmth, a stale open turn is never considered idle here.
pub fn compact_due(activity: &Activity, warmth: &Warmth, now_ms: u64) -> bool {
    activity.busy == Some(false)
        && activity
            .last_activity_ms
            .is_some_and(|last| now_ms.saturating_sub(last) >= COMPACT_IDLE_MS)
        && activity.context_window != Some(0)
        && long_context(activity, warmth)
}

fn long_context(activity: &Activity, warmth: &Warmth) -> bool {
    activity
        .context_tokens
        .is_some_and(|tokens| tokens as f64 > long_context_tokens(activity, warmth))
}

/// At most [`SHORT_CONTEXT_SHARE`] of the long-context threshold. A context
/// whose size is not known is not short.
fn short_context(activity: &Activity, warmth: &Warmth) -> bool {
    activity.context_tokens.is_some_and(|tokens| {
        tokens as f64 <= SHORT_CONTEXT_SHARE * long_context_tokens(activity, warmth)
    })
}

/// The group's share of the member's window.
fn long_context_tokens(activity: &Activity, warmth: &Warmth) -> f64 {
    let window = activity.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW);
    f64::from(warmth.context_ratio) * window as f64
}

/// `<root>/<any project>/<session_id>.jsonl`.
fn find_transcript(root: &Path, session_id: &str) -> Option<PathBuf> {
    if !safe_id(session_id) {
        return None;
    }
    let name = format!("{session_id}.jsonl");
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&name))
        .find(|path| path.is_file())
}

/// Rebon's `/compact` leaves the transcript as it is and writes the compacted
/// baseline beside it, as `<session>.compact.json` (`rebon-core`'s
/// `compact_baseline_path`). A baseline at least as new as the transcript
/// means the context its last reply measured is gone; the baseline holds
/// the history that replaced it, so its size, at [`BYTES_PER_TOKEN`], stands
/// in for the size no reply has measured yet.
fn rebon_activity(path: &Path, session_id: &str) -> Activity {
    let mut activity = transcript_activity(path, false);
    let baseline = path.with_file_name(format!("{session_id}.compact.json"));
    let Ok(metadata) = std::fs::metadata(&baseline) else {
        return activity;
    };
    let Some(compacted_ms) = modified_ms(&baseline) else {
        return activity;
    };
    if activity
        .last_activity_ms
        .map_or(true, |last| compacted_ms >= last)
    {
        activity.last_activity_ms = Some(compacted_ms);
        activity.context_tokens = Some(metadata.len() / BYTES_PER_TOKEN);
    }
    activity
}

/// Rebon's and Claude Code's transcripts: one JSON object per line, the
/// model's replies carrying `message.stop_reason` and `message.usage`.
/// Claude Code marks a compaction with a `compact_boundary` row; what came
/// before it describes a context that is gone.
fn transcript_activity(path: &Path, reads_cache_ttl: bool) -> Activity {
    let mut activity = Activity {
        last_activity_ms: modified_ms(path),
        ..Activity::default()
    };
    let lines = tail_lines(path);
    let boundary = lines.iter().rposition(|value| {
        value.get("type").and_then(Value::as_str) == Some("system")
            && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
    });
    let lines = match boundary {
        Some(index) => {
            activity.context_tokens = lines[index]
                .pointer("/compactMetadata/postTokens")
                .and_then(Value::as_u64);
            &lines[index + 1..]
        }
        None => &lines[..],
    };
    // A compaction with no turn row after it finished while idle.
    activity.busy = last_turn_busy(lines).or(boundary.map(|_| false));
    if let Some(usage) = lines
        .iter()
        .rev()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("assistant"))
        .find_map(|value| value.pointer("/message/usage"))
    {
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        activity.context_tokens = Some(
            count("input_tokens")
                + count("cache_read_input_tokens")
                + count("cache_creation_input_tokens"),
        );
        if reads_cache_ttl {
            let written = |key: &str| {
                usage
                    .pointer(&format!("/cache_creation/{key}"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            activity.cache_ttl_minutes = if written("ephemeral_1h_input_tokens") > 0 {
                Some(60)
            } else if written("ephemeral_5m_input_tokens") > 0 {
                Some(5)
            } else {
                None
            };
        }
    }
    activity
}

/// Whether the last turn in `lines` is still open: the last assistant row's
/// stop reason, or a user row after it. `None` when neither is there.
///
/// Claude Code also writes user rows that open no turn: a local command
/// (`/model`, `/compact`) records a caveat, its `<command-name>` and its
/// output once it has finished, and a compaction records its summary. A
/// `<command-name>` counts as the local command's only when that command's
/// output follows it; a prompt command (`/review`) has none and opens a turn.
fn last_turn_busy(lines: &[Value]) -> Option<bool> {
    let mut local_output = false;
    for value in lines.iter().rev() {
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let stop = value
                    .pointer("/message/stop_reason")
                    .and_then(Value::as_str);
                return Some(!matches!(
                    stop,
                    Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal")
                ));
            }
            Some("user") => {
                let text = value
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.starts_with("<local-command-stdout>")
                    || text.starts_with("<local-command-stderr>")
                {
                    local_output = true;
                } else if text.starts_with("<command-name>") && local_output {
                    local_output = false;
                } else if !text.starts_with("<local-command-caveat>")
                    && value.get("isCompactSummary").and_then(Value::as_bool) != Some(true)
                {
                    return Some(true);
                }
            }
            _ => {}
        }
    }
    None
}

/// `<sessions>/YYYY/MM/DD/rollout-<time>-<session_id>.jsonl`, newest day
/// first. A rollout compressed after going cold (`.jsonl.zst`) is not
/// matched: a cold session is cold either way.
fn find_rollout(sessions: &Path, session_id: &str) -> Option<PathBuf> {
    if !safe_id(session_id) {
        return None;
    }
    let suffix = format!("-{session_id}.jsonl");
    let newest_first = |dir: &Path| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs.reverse();
        dirs
    };
    let mut days_seen = 0;
    for year in newest_first(sessions) {
        for month in newest_first(&year) {
            for day in newest_first(&month) {
                days_seen += 1;
                if days_seen > 400 {
                    return None;
                }
                let found = std::fs::read_dir(&day).ok()?.flatten().find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(suffix.as_str())
                });
                if let Some(entry) = found {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

/// Codex's rollout: `{timestamp, type, payload}` per line; `event_msg`
/// payloads mark turns and carry the token count.
fn rollout_activity(path: &Path) -> Activity {
    let mut activity = Activity {
        last_activity_ms: modified_ms(path),
        ..Activity::default()
    };
    for value in tail_lines(path).iter().rev() {
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let payload = value.get("payload").unwrap_or(&Value::Null);
        match payload.get("type").and_then(Value::as_str) {
            Some("task_started") if activity.busy.is_none() => activity.busy = Some(true),
            Some("task_complete" | "turn_aborted") if activity.busy.is_none() => {
                activity.busy = Some(false)
            }
            Some("token_count") if activity.context_tokens.is_none() => {
                let info = payload.get("info").unwrap_or(&Value::Null);
                activity.context_tokens = info
                    .pointer("/last_token_usage/input_tokens")
                    .and_then(Value::as_u64);
                activity.context_window = info.get("model_context_window").and_then(Value::as_u64);
            }
            _ => {}
        }
        if activity.busy.is_some() && activity.context_tokens.is_some() {
            break;
        }
    }
    activity
}

/// `<sessions>/<encoded cwd>/<session_id>/`.
fn find_grok_session(sessions: &Path, session_id: &str) -> Option<PathBuf> {
    if !safe_id(session_id) {
        return None;
    }
    std::fs::read_dir(sessions)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(session_id))
        .find(|path| path.is_dir())
}

fn grok_activity(dir: &Path) -> Activity {
    let last = [
        "updates.jsonl",
        "chat_history.jsonl",
        "summary.json",
        "signals.json",
    ]
    .iter()
    .filter_map(|name| modified_ms(&dir.join(name)))
    .max();
    let signals: Value = std::fs::read(dir.join("signals.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    Activity {
        last_activity_ms: last,
        busy: None,
        context_tokens: signals.get("context_tokens_used").and_then(Value::as_u64),
        context_window: signals.get("context_window_tokens").and_then(Value::as_u64),
        cache_ttl_minutes: None,
    }
}

fn modified_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// The JSON lines at the end of a file; a first line cut by the tail's
/// start, and any line that does not parse, are skipped.
fn tail_lines(path: &Path) -> Vec<Value> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    lines
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect()
}

/// A session id is a file-name component here; one that could climb out of
/// the directory it is looked up in is nobody's.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
#[path = "warmth_tests.rs"]
mod tests;
