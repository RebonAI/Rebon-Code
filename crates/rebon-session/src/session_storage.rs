//! On-disk session transcript layout, parsing, and mutation: path keying,
//!
//! the `parentUuid` chain walk, append/finalize writes, locks, and the
//! rewind/summarize path. The format is much richer than the walk alone
//! (write path, compact boundaries, metadata lines, attachment snapshots,
//! context-collapse commits, legacy progress bridging, parallel tool-result
//! recovery…); what the walk covers is the `session/load` happy path:
//!
//! 1. Resolve a `(cwd, sessionId)` pair to `${root}/${sanitize(cwd)}/${sessionId}.jsonl`.
//! 2. Parse each JSONL line best-effort, skipping malformed lines.
//! 3. Find every terminal entry (no other entry references it as a parent),
//!    walk `parentUuid` backwards from each terminal until the nearest
//!    `user`/`assistant` ancestor, then pick the ancestor whose own
//!    timestamp sorts latest and walk its parent chain back to the root,
//!    returning the ordered chain.
//!
//! Deliberately deferred (documented here rather than implemented):
//!
//! - Compact boundaries, attachment snapshots, context-collapse commits,
//!   legacy progress bridging. A transcript that relies on any of these
//!   typically produces a shorter-than-expected chain — but if every
//!   user/assistant entry in the fixture depends on one of these
//!   deferred features (e.g. a context-collapse commit dropped the only
//!   remaining user/assistant ancestor), the chain walker will find no
//!   leaf and the loader returns `None`, which `session/load` surfaces
//!   as `Session not found`. In other words: these deferrals can still
//!   fail the load outright, they do not merely truncate the chain.
//! - Parallel tool-result recovery. The walker is the simple linked-list
//!   form; DAG-shaped histories collapse to a single branch with no
//!   recovery pass.
//! - Custom title / metadata lines. `loaded.title` is always `None`
//!   here.
//! - Sibling-worktree fallback when resolving the project directory.
//!   Callers pass an exact `cwd`, and we look up exactly
//!   `sanitize(cwd_identity(cwd))` under the project root.
//! - For paths exceeding `MAX_SANITIZED_LENGTH` (200 chars), the hash
//!   suffix uses djb2 (`simple_hash`). Transcripts written with a
//!   different hash for >200-char cwds would land in a different
//!   directory — document-only, no test coverage.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use rebon_types::{decode_ultraplan_run_state, UltraplanRunState};

pub use rebon_types::format_system_time_iso_ms;

/// Maximum length for a sanitized path component before the hash suffix
/// kicks in.
pub const MAX_SANITIZED_LENGTH: usize = 200;

/// Compute the default config home directory.
///
/// Resolution lives in [`crate::config_home`] so every consumer of the config
/// home agrees on it. We do **not** NFC-normalize the resulting string —
/// Rust `PathBuf`s are opaque byte sequences on most platforms, and the
/// only place NFC normalization would matter here is macOS file
/// lookups, which Rust's `std::fs` already handles natively.
pub fn default_config_home_dir() -> PathBuf {
    crate::config_home::default_config_home_dir()
}

/// Default "projects root" directory, i.e.
/// `${config_home}/projects`. There is one
/// subdirectory per sanitized cwd under this root with per-session
/// JSONL files inside it.
pub fn default_projects_root() -> PathBuf {
    let mut p = default_config_home_dir();
    p.push("projects");
    p
}

/// Sanitize a cwd for use as a single filesystem directory component.
///
/// 1. Replace every character that is not ASCII `[a-zA-Z0-9]` with `-`.
/// 2. If the result fits in `MAX_SANITIZED_LENGTH`, return it as-is.
/// 3. Otherwise truncate to `MAX_SANITIZED_LENGTH` and append
///    `-{hash}`, where the hash is the djb2 `simple_hash` of the
///    original name.
pub fn sanitize_path(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch);
        } else {
            sanitized.push('-');
        }
    }
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let hash = simple_hash(name);
    let mut truncated: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    truncated.push('-');
    truncated.push_str(&hash);
    truncated
}

pub fn cwd_identity(path: &str) -> String {
    cwd_identity_for_platform(path, cfg!(windows))
}

pub fn same_cwd(left: &str, right: &str) -> bool {
    same_cwd_for_platform(left, right, cfg!(windows))
}

/// Compare two cwds without building either identity.
///
/// The obvious spelling, `cwd_identity(left) == cwd_identity(right)`, allocates
/// four `String`s per comparison — one `replace` and one `to_lowercase` a side.
/// The sidebar asks this once per (session, job) pair on every refresh, which
/// made it one of the app's larger allocators for a question that needs no
/// memory at all.
///
/// The fast path is ASCII-only on purpose. Windows folding uses
/// `str::to_lowercase`, whose Unicode rules are context-sensitive — Greek final
/// sigma lowercases differently at the end of a word than `char::to_lowercase`
/// would — so anything outside ASCII defers to the real thing rather than risk
/// answering differently from `cwd_identity`.
fn same_cwd_for_platform(left: &str, right: &str, windows: bool) -> bool {
    // An extended-length spelling has to be folded before anything is compared,
    // and folding is not a per-byte operation. Defer, so this answers what
    // `cwd_identity` answers rather than becoming a second reading of it.
    let verbatim = windows && (left.starts_with(r"\\?\") || right.starts_with(r"\\?\"));
    if verbatim || !left.is_ascii() || !right.is_ascii() {
        return cwd_identity_for_platform(left, windows)
            == cwd_identity_for_platform(right, windows);
    }
    let left = &left.as_bytes()[..trimmed_ascii_cwd_len(left.as_bytes(), windows)];
    let right = &right.as_bytes()[..trimmed_ascii_cwd_len(right.as_bytes(), windows)];
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            normalized_ascii_cwd_byte(*left, windows) == normalized_ascii_cwd_byte(*right, windows)
        })
}

/// Length after `cwd_identity`'s trailing-separator trim. Backslashes count as
/// separators on Windows because the identity replaces them before trimming, and
/// a bare drive root keeps its slash.
fn trimmed_ascii_cwd_len(bytes: &[u8], windows: bool) -> usize {
    let mut len = bytes.len();
    while len > 1 {
        let last = bytes[len - 1];
        if last != b'/' && !(windows && last == b'\\') {
            break;
        }
        if windows && len == 3 && bytes[1] == b':' {
            break;
        }
        len -= 1;
    }
    len
}

fn normalized_ascii_cwd_byte(byte: u8, windows: bool) -> u8 {
    if !windows {
        return byte;
    }
    if byte == b'\\' { b'/' } else { byte }.to_ascii_lowercase()
}

fn cwd_identity_for_platform(path: &str, windows: bool) -> String {
    let mut normalized = if windows {
        // `\\?\F:\dev\x` is the same directory as `F:\dev\x`, and the
        // extended-length spelling is not exotic: `canonicalize` returns it,
        // and anything built to clear `MAX_PATH` carries it. Left in, it makes
        // a second project key for one directory, and a session recorded under
        // one key is invisible from the other -- which is how a session
        // with a live worker came to read as unowned: the owner
        // descriptor was there, under the other spelling.
        //
        // The rule has one home (`rebon-tools-core`), which is why this crate
        // already depends on it. This is the caller that was not using it.
        rebon_tools_core::strip_windows_verbatim_prefix_str(path, true).replace('\\', "/")
    } else {
        path.to_string()
    };
    while normalized.ends_with('/')
        && normalized.len() > 1
        && !(windows && normalized.len() == 3 && normalized.as_bytes()[1] == b':')
    {
        normalized.pop();
    }
    if windows {
        normalized = normalized.to_lowercase();
    }
    normalized
}

/// Canonical projects-root directory component for a cwd:
/// `sanitize_path(cwd_identity(cwd))`.
///
/// Every `${projects_root}/<component>/…` path MUST derive its component
/// through this function. Sanitizing the raw spelling splits one working
/// directory into several project keys on Windows (`F:\dev\x` vs
/// `f:/dev/x` → `F--dev-x` vs `f--dev-x`), which surfaces as transcripts,
/// sidecars, and file-history manifests "missing" from whichever spelling
/// the reader happens to use. `cwd_identity` is idempotent, so callers
/// holding an already-normalized identity may pass it as-is.
pub fn project_dir_component(cwd: &str) -> String {
    sanitize_path(&cwd_identity(cwd))
}

/// Fold an on-disk project directory *name* (a sanitized component read
/// back from `read_dir`, not a cwd) so it compares equal to
/// [`project_dir_component`] output for the same working directory.
///
/// Directories created before case normalization may be spelled
/// `F--dev-x` while the canonical key is `f--dev-x`. On Windows the
/// filesystem resolves both spellings to the same directory, so folding
/// only affects in-memory string keys; on case-sensitive platforms names
/// are already canonical and are returned unchanged.
pub fn fold_project_dir_name(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

/// djb2 hash → base-36 absolute value.
///
/// The digits end up in on-disk project directory names, so the arithmetic
/// is fixed: `hash * 31 + code` in wrapping signed 32-bit arithmetic, then
/// the magnitude taken through `i64` so a negative hash (including
/// `i32::MIN`) renders as its positive absolute value.
fn simple_hash(s: &str) -> String {
    let mut hash: i32 = 0;
    // One step per `char`. For BMP text this is the same sequence as
    // hashing UTF-16 code units; a non-BMP character contributes its whole
    // code point instead of a surrogate pair, which cwd paths don't contain.
    for ch in s.chars() {
        let code = ch as u32 as i32;
        hash = hash.wrapping_shl(5).wrapping_sub(hash).wrapping_add(code);
    }
    let abs = (hash as i64).unsigned_abs();
    to_base36(abs)
}

/// Convert a u64 to its lowercase base-36 representation.
fn to_base36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out: Vec<u8> = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 digits are ASCII")
}

/// Parse an RFC-3339 timestamp (the subset transcripts carry) into a
/// [`std::time::SystemTime`] without pulling in `time` or
/// `chrono`. Returns `None` on any shape the stdlib cannot round-trip.
///
/// Accepted forms:
///
/// - `YYYY-MM-DDTHH:MM:SS[.fff...][Z|±HH:MM|±HHMM]`
/// - Fractional seconds of any length (truncated at nanoseconds).
/// - Year 1970..=9999. Dates before the Unix epoch collapse to
///   [`UNIX_EPOCH`](std::time::UNIX_EPOCH).
///
/// The parser is intentionally strict about the leading `YYYY-MM-DDTHH:MM:SS`
/// skeleton — unexpected shapes (two-digit years, comma fractions, missing
/// seconds) return `None` rather than silently drifting. Every transcript
/// entry carries the accepted shape.

fn parse_rfc3339_to_system_time(input: &str) -> Option<std::time::SystemTime> {
    let bytes = input.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: u32 = input.get(0..4)?.parse().ok()?;
    let month: u32 = input.get(5..7)?.parse().ok()?;
    let day: u32 = input.get(8..10)?.parse().ok()?;
    let hour: u32 = input.get(11..13)?.parse().ok()?;
    let minute: u32 = input.get(14..16)?.parse().ok()?;
    let second: u32 = input.get(17..19)?.parse().ok()?;

    // Optional `.fractional` segment + mandatory tz suffix.
    let mut idx = 19usize;
    let mut subsec_nanos: u32 = 0;
    if bytes.get(idx) == Some(&b'.') {
        idx += 1;
        let frac_start = idx;
        while bytes.get(idx).map_or(false, |b| b.is_ascii_digit()) {
            idx += 1;
        }
        let mut frac = input.get(frac_start..idx)?.to_string();
        // Truncate to 9 digits (nanos); pad to 9 if shorter.
        if frac.len() > 9 {
            frac.truncate(9);
        } else {
            while frac.len() < 9 {
                frac.push('0');
            }
        }
        subsec_nanos = frac.parse().ok()?;
    }

    // Parse the tz offset (keep it signed, applied as a minute delta).
    let tz_offset_minutes: i64 = match bytes.get(idx) {
        Some(&b'Z') | Some(&b'z') => 0,
        Some(&sign @ (b'+' | b'-')) => {
            idx += 1;
            let off_hour: i64 = input.get(idx..idx + 2)?.parse().ok()?;
            idx += 2;
            if bytes.get(idx) == Some(&b':') {
                idx += 1;
            }
            let off_min: i64 = input.get(idx..idx + 2)?.parse().ok()?;
            let magnitude = off_hour * 60 + off_min;
            if sign == b'-' {
                -magnitude
            } else {
                magnitude
            }
        }
        _ => return None,
    };

    // Convert the date to days-since-epoch using Howard Hinnant's
    // civil_from_days inverse (days_from_civil). Only valid for the
    // proleptic Gregorian calendar — fine for every ISO-8601 timestamp
    // the harness writes.
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 {
        year as i64 - 1
    } else {
        year as i64
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as i64;
    let m = month as i64;
    let d = day as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146_097 + doe - 719_468;
    if days_since_epoch < 0 {
        return Some(std::time::UNIX_EPOCH);
    }

    let secs_of_day = (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let total_secs = days_since_epoch
        .checked_mul(86_400)?
        .checked_add(secs_of_day)?
        .checked_sub(tz_offset_minutes.checked_mul(60)?)?;
    if total_secs < 0 {
        return Some(std::time::UNIX_EPOCH);
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(total_secs as u64, subsec_nanos))
}

/// Resolve the transcript file path for a `(cwd, sessionId)` pair under
/// `projects_root`.
pub fn transcript_file_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut p = projects_root.to_path_buf();
    p.push(project_dir_component(cwd));
    p.push(format!("{session_id}.jsonl"));
    p
}

/// Resolve the project directory (without a session id suffix) for a given
/// `cwd` under `projects_root` — this is the
/// directory the `session/list` scanner walks to discover on-disk
/// transcripts for the effective cwd.
pub fn project_dir_path(projects_root: &Path, cwd: &str) -> PathBuf {
    let mut p = projects_root.to_path_buf();
    p.push(project_dir_component(cwd));
    p
}

/// The project-directory keys this build no longer produces for `cwd`.
///
/// Exactly two, and only the two: the extended-length spellings of a drive
/// path and of a UNC path, which is what `cwd_identity` used to leave in the
/// key. This is not a scan for anything that might once have been a key — a
/// scan would have to guess, and guessing about which directory holds someone's
/// transcripts is not a thing to do.
fn legacy_project_dir_identities(identity: &str) -> Vec<String> {
    // `\\?\F:\dev\x` -> `//?/f:/dev/x`, against a folded `f:/dev/x`.
    let drive = identity
        .as_bytes()
        .get(1)
        .is_some_and(|byte| *byte == b':')
        .then(|| format!("//?/{identity}"));
    // `\\?\UNC\host\share\x` -> `//?/unc/host/share/x`, against `//host/share/x`.
    let unc = identity
        .strip_prefix('/')
        .filter(|rest| rest.starts_with('/'))
        .map(|rest| format!("//?/unc{rest}"));
    drive.into_iter().chain(unc).collect()
}

/// Move a project directory left behind under the old key into the one this
/// build uses, once per directory per process.
///
/// Until the extended-length prefix was folded into the key, a Windows
/// worker keyed its session under `//?/f:/dev/x` while everything that read a
/// plain cwd looked under `f:/dev/x`. A worker's cwd comes from `canonicalize`
/// and therefore *always* carried the prefix, so this is not a stray corner:
/// on Windows, every background session's transcript is in the old directory.
/// Folding the key without moving them would turn "only the worker can see
/// these" into "nobody can".
///
/// Same-named entries are not overwritten. Two files with one name here means
/// two sessions were recorded under one id in two directories, and choosing
/// between them is not this function's business: the old one is left where it
/// is and says so in the log, which also leaves the old directory in place
/// rather than removing a directory that still holds something.
///
/// The one exception is the cwd sidecar, which is derived: the target
/// directory's own is the correct one by construction, so a stale copy is
/// dropped rather than kept as a puzzle.
fn fold_legacy_project_dir(projects_root: &Path, cwd: &str) {
    let identity = cwd_identity(cwd);
    let target = projects_root.join(sanitize_path(&identity));
    {
        let mut folded = folded_project_dirs().lock().expect("poisoned");
        if !folded.insert(target.clone()) {
            return;
        }
    }
    for legacy in legacy_project_dir_identities(&identity) {
        let old = projects_root.join(sanitize_path(&legacy));
        if old == target || !old.is_dir() {
            continue;
        }
        fold_one_project_dir(&old, &target);
    }
}

fn fold_one_project_dir(old: &Path, target: &Path) {
    let Ok(entries) = std::fs::read_dir(old) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(target) {
        tracing::warn!(
            from = %old.display(),
            to = %target.display(),
            %error,
            "rebon: could not open the project directory to move an older one into"
        );
        return;
    }
    let mut moved = 0usize;
    let mut kept = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let destination = target.join(&name);
        if destination.exists() {
            if name == PROJECT_CWD_SIDECAR {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            kept += 1;
            tracing::warn!(
                path = %entry.path().display(),
                conflicts_with = %destination.display(),
                "rebon: leaving a file from the older project directory in place; a file of that name is already here"
            );
            continue;
        }
        match std::fs::rename(entry.path(), &destination) {
            Ok(()) => moved += 1,
            Err(error) => {
                kept += 1;
                tracing::warn!(
                    path = %entry.path().display(),
                    to = %destination.display(),
                    %error,
                    "rebon: could not move a file out of the older project directory"
                );
            }
        }
    }
    if kept == 0 {
        let _ = std::fs::remove_dir(old);
    }
    if moved > 0 || kept > 0 {
        tracing::info!(
            from = %old.display(),
            to = %target.display(),
            moved,
            kept,
            "rebon: folded a project directory keyed under the extended-length path spelling into the current one"
        );
    }
}

/// Project directories already checked in this process. One check each: the
/// answer cannot change without another process writing under a key this
/// build no longer produces.
fn folded_project_dirs() -> &'static Mutex<HashSet<PathBuf>> {
    static FOLDED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    FOLDED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Resolve the **sidecar metadata** file path for a `(cwd, session_id)`
/// pair under `projects_root`. Lives next to the transcript and holds
/// AI-generated display titles (plus room to grow into richer per-session
/// metadata later). The metadata lives in this sidecar rather than as
/// lines *inside* the jsonl transcript.
///
/// Schema of the file (all fields optional, unknown fields ignored):
/// ```json
/// { "title": "Fix login bug on mobile", "mode": "coordinator" }
/// ```
pub fn session_meta_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut p = projects_root.to_path_buf();
    p.push(project_dir_component(cwd));
    p.push(format!("{session_id}.meta.json"));
    p
}

/// Resolve the advisory active-session lock path for a `(cwd, session_id)` pair.
pub fn session_active_lock_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut p = projects_root.to_path_buf();
    p.push(project_dir_component(cwd));
    p.push(format!("{session_id}.active.lock"));
    p
}

/// Filename suffix of the owner descriptor. Exported so a consumer walking a
/// project directory can recognize `<session_id>.owner.json` without
/// re-spelling the format.
pub const SESSION_OWNER_SUFFIX: &str = ".owner.json";

/// `<project_dir>/<session_id>.owner.json` — how to reach the process that
/// owns a session.
pub fn session_owner_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut p = projects_root.to_path_buf();
    p.push(project_dir_component(cwd));
    p.push(format!("{session_id}{SESSION_OWNER_SUFFIX}"));
    p
}

/// Which kind of process owns a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionOwnerSurface {
    /// A background worker: the default host, reachable and long-lived.
    Worker,
    /// A terminal hosting the session in its own process. No endpoint.
    Local,
    /// An ACP server hosting the session over the wire.
    Acp,
}

/// Where the owner of a session is and how to reach it.
///
/// The endpoint address cannot live in the lock file: a Windows exclusive byte
/// lock blocks other handles from reading it, so a client asking "who has
/// this?" would only be able to learn "someone". This sits beside the lock and
/// answers the rest.
///
/// Its freshness is decided by the lock alone — a descriptor next to a lock
/// nobody holds is stale, whatever it says about itself. That keeps the two
/// files from disagreeing about liveness, and means a crash (which frees the
/// lock but leaves the file) reads as "free", not as "owned by a dead pid".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOwnerDescriptor {
    pub version: u32,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_identity: Option<String>,
    pub surface: SessionOwnerSurface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// Loopback port of the owner's command endpoint. Absent for a `local`
    /// owner, which has none — such a session is readable but not operable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_token: Option<String>,
    pub started_at_ms: u64,
}

/// The descriptor version this build writes.
pub const SESSION_OWNER_VERSION: u32 = 1;

impl SessionOwnerDescriptor {
    /// Whether this owner can be sent commands, as opposed to merely being
    /// known to exist.
    pub fn is_reachable(&self) -> bool {
        self.ipc_port.is_some() && self.ipc_token.is_some()
    }
}

/// Publish the owner descriptor for a session.
///
/// Written through a temp file and renamed, so a reader never sees a half
/// descriptor. Call it after the lock is held and the endpoint is bound, and
/// before the first transcript write — a client that can see writes but not
/// the owner has no way to reach whoever is making them.
pub fn write_session_owner(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    descriptor: &SessionOwnerDescriptor,
) -> std::io::Result<()> {
    let path = session_owner_path(projects_root, cwd, session_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(descriptor)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    // The descriptor carries the endpoint token, so it is no more readable
    // than the job record that carries the same token.
    write_private_file_atomically(&path, &body)
}

/// Read the owner descriptor, if one is published and parseable.
///
/// Says nothing about whether that owner is still alive: ask
/// [`is_session_active`] for that. A descriptor left behind by a crash parses
/// perfectly well.
pub fn read_session_owner(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Option<SessionOwnerDescriptor> {
    let path = session_owner_path(projects_root, cwd, session_id);
    let body = std::fs::read(path).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Remove a session's owner descriptor. Best effort by design: the lock is
/// what decides liveness, so a descriptor that outlives its owner is stale
/// rather than wrong.
pub fn remove_session_owner(projects_root: &Path, cwd: &str, session_id: &str) {
    let _ = std::fs::remove_file(session_owner_path(projects_root, cwd, session_id));
}

pub fn ultraplan_run_dir_path(projects_root: &Path, cwd: &str) -> PathBuf {
    let mut p = project_dir_path(projects_root, cwd);
    p.push("ultraplan");
    p
}

pub fn ultraplan_run_path(projects_root: &Path, cwd: &str, run_id: &str) -> PathBuf {
    let mut p = ultraplan_run_dir_path(projects_root, cwd);
    p.push(format!("{run_id}.json"));
    p
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub session_id: String,
    pub task: String,
    pub phase: rebon_types::RunPhase,
    pub round: u32,
    pub updated_at_ms: u64,
}

impl From<&UltraplanRunState> for RunSummary {
    fn from(state: &UltraplanRunState) -> Self {
        Self {
            run_id: state.run_id.clone(),
            session_id: state.session_id.clone(),
            task: state.task.clone(),
            phase: state.phase,
            round: state.round,
            updated_at_ms: state.updated_at_ms,
        }
    }
}

/// Process-held marker that a TUI session is currently open.
#[derive(Debug)]
pub struct SessionActiveLock {
    file: Option<File>,
    path: PathBuf,
    /// The project directory to try to reclaim once this lock's file is gone.
    /// `None` when the lock does not live in a project directory of its own —
    /// an empty `cwd` sanitizes to an empty component, which would make the
    /// lock's parent the projects root itself.
    prunable_dir: Option<PathBuf>,
}

fn active_lock_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static ACTIVE_LOCK_PATHS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    ACTIVE_LOCK_PATHS.get_or_init(|| Mutex::new(HashSet::new()))
}

impl SessionActiveLock {
    pub fn release(mut self) {
        self.release_inner();
    }

    pub fn is_for(&self, projects_root: &Path, cwd: &str, session_id: &str) -> bool {
        self.path == session_active_lock_path(projects_root, cwd, session_id)
    }

    /// The owner descriptor that belongs beside this lock, derived from the
    /// lock's own path so the two can never be published for different
    /// sessions.
    fn owner_descriptor_path(&self) -> Option<PathBuf> {
        let name = self.path.file_name()?.to_str()?;
        let session_id = name.strip_suffix(".active.lock")?;
        Some(
            self.path
                .with_file_name(format!("{session_id}{SESSION_OWNER_SUFFIX}")),
        )
    }

    fn release_inner(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
            drop(file);
            active_lock_paths()
                .lock()
                .expect("active session lock registry poisoned")
                .remove(&self.path);
            // The owner descriptor is only meaningful while the lock is held,
            // so it goes with the lock rather than waiting for every owner to
            // remember to delete it. Best effort: a leftover reads as stale
            // because the lock beside it is free.
            if let Some(owner) = self.owner_descriptor_path() {
                let _ = std::fs::remove_file(owner);
            }
            if std::fs::remove_file(&self.path).is_ok() {
                if let Some(dir) = self.prunable_dir.as_deref() {
                    // Best effort: `remove_dir` refuses a directory that gained a
                    // transcript, a sidecar, or another session's lock in the
                    // meantime, which is exactly the guard we want.
                    let _ = std::fs::remove_dir(dir);
                }
            }
        }
    }
}

impl Drop for SessionActiveLock {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// Try to mark a session as active. Returns `None` when another process owns it.
pub fn try_acquire_session_active_lock(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> std::io::Result<Option<SessionActiveLock>> {
    // The other end of the same question: a reader asking
    // [`is_session_active`] has to find an owner recorded under the old key
    // here, or it decides nobody holds the session.
    fold_legacy_project_dir(projects_root, cwd);
    let path = session_active_lock_path(projects_root, cwd, session_id);
    let dir = path
        .parent()
        .expect("session active lock path always has a project directory")
        .to_path_buf();
    let prunable_dir = prunable_project_dir(projects_root, cwd);
    std::fs::create_dir_all(&dir)?;
    {
        let registry = active_lock_paths()
            .lock()
            .expect("active session lock registry poisoned");
        if registry.contains(&path) {
            return Ok(None);
        }
    }
    let open_lock_file = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
    };
    let file = match open_lock_file() {
        // Another process released the last lock in this project directory
        // between our `create_dir_all` and this open, and reclaimed the
        // now-empty directory. Recreate it and try once more rather than
        // failing the session start.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&dir)?;
            open_lock_file()?
        }
        result => result?,
    };

    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            active_lock_paths()
                .lock()
                .expect("active session lock registry poisoned")
                .insert(path.clone());
            // Opportunistically sweep sibling orphan locks left by sessions
            // that crashed without running their destructor. Cheap (runs only
            // when a session is opened) and keeps the project dir from
            // accumulating stale `.active.lock` files indefinitely.
            // Never reclaim the directory here: our own lock file is still in
            // it, so the sweep could only ever fail, and passing `false` keeps
            // that intent explicit.
            let swept = cleanup_stale_active_locks_in_dir(&dir, Some(&path), false);
            if swept > 0 {
                tracing::debug!(
                    swept,
                    dir = %dir.display(),
                    "rebon: removed stale session active-lock files"
                );
            }
            Ok(Some(SessionActiveLock {
                file: Some(file),
                path,
                prunable_dir,
            }))
        }
        Err(err) => {
            if is_lock_contention(&err) {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

/// Whether a failed `try_lock_exclusive` means "someone else holds it" rather
/// than "the lock could not be evaluated".
///
/// The distinction decides whether a session reads as active or as free, so
/// getting it wrong is not a cosmetic matter: [`is_session_active`] reports
/// `false` for anything it could not evaluate, and the surfaces that hide or
/// refuse a session someone else owns are built on that answer.
///
/// Unix `flock` contention arrives as `WouldBlock`. Windows returns
/// `ERROR_LOCK_VIOLATION` / `ERROR_SHARING_VIOLATION`, and **neither maps onto
/// a stable `ErrorKind`** — they surface as the unstable `Uncategorized`, which
/// no pattern can name. Matching `ErrorKind::Other` does not catch them either:
/// since Rust 1.60 no error produced by the standard library is ever `Other`,
/// so that arm is dead for OS errors. The raw code is the only reliable test.
fn is_lock_contention(err: &std::io::Error) -> bool {
    if matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
    ) {
        return true;
    }
    #[cfg(windows)]
    {
        /// The byte range is locked by another handle.
        const ERROR_LOCK_VIOLATION: i32 = 33;
        /// The file itself is open under an incompatible sharing mode.
        const ERROR_SHARING_VIOLATION: i32 = 32;
        if matches!(
            err.raw_os_error(),
            Some(ERROR_LOCK_VIOLATION) | Some(ERROR_SHARING_VIOLATION)
        ) {
            return true;
        }
    }
    false
}

/// Best-effort garbage collection of orphaned `*.active.lock` files in the
/// project directory for `cwd`.
///
/// A lock file is orphaned when its owning process died without running the
/// [`SessionActiveLock`] destructor (crash / SIGKILL / power loss), so the file
/// lingers even though no session is open. The OS releases the advisory lock on
/// process exit regardless, so any lock we can re-acquire has no live owner and
/// its file is safe to delete. Files still held by a live process (this one or
/// another rebon) fail `try_lock_exclusive` and are left untouched — this never
/// disturbs a running session.
///
/// Returns the number of stale lock files removed.
pub fn cleanup_stale_active_locks(projects_root: &Path, cwd: &str) -> usize {
    let dir = project_dir_path(projects_root, cwd);
    let prunable = prunable_project_dir(projects_root, cwd).is_some();
    cleanup_stale_active_locks_in_dir(&dir, None, prunable)
}

/// The project directory that may be reclaimed once it is empty, or `None` when
/// the `cwd` has no directory component of its own.
///
/// `project_dir_component("")` is the empty string, which makes
/// `project_dir_path` the projects root itself — reclaiming *that* would delete
/// the whole store the moment it happened to be empty.
fn prunable_project_dir(projects_root: &Path, cwd: &str) -> Option<PathBuf> {
    let component = project_dir_component(cwd);
    if component.is_empty() {
        return None;
    }
    Some(projects_root.join(component))
}

fn cleanup_stale_active_locks_in_dir(
    dir: &Path,
    keep: Option<&Path>,
    prune_empty_dir: bool,
) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    // Snapshot the in-process registry so we never delete a lock this process
    // is actively holding, without holding the mutex across file I/O.
    let held = active_lock_paths()
        .lock()
        .expect("active session lock registry poisoned")
        .clone();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if Some(path.as_path()) == keep || held.contains(&path) {
            continue;
        }
        let is_lock = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".active.lock"));
        if !is_lock {
            continue;
        }
        // Open without `create`: if the file vanished underneath us, skip it.
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        // A lock we can acquire has no live owner -> safe to remove. Release and
        // close the handle before unlinking so the path is no longer open.
        if FileExt::try_lock_exclusive(&file).is_ok() {
            let _ = FileExt::unlock(&file);
            drop(file);
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    if removed > 0 && prune_empty_dir {
        let _ = std::fs::remove_dir(dir);
    }
    removed
}

/// Best-effort check for whether another process currently owns a session.
pub fn is_session_active(projects_root: &Path, cwd: &str, session_id: &str) -> bool {
    match try_acquire_session_active_lock(projects_root, cwd, session_id) {
        Ok(Some(lock)) => {
            lock.release();
            false
        }
        Ok(None) => true,
        Err(err) => {
            tracing::warn!(
                error = %err,
                session_id = %session_id,
                "rebon: failed to probe session active lock"
            );
            false
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Sidecar metadata for one session.
///
/// Known fields stay typed for callers, while flattened fields preserve
/// metadata written by other surfaces when this file is updated.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMetaFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// When the session was created, ISO-8601, written once where the
    /// session is named.
    ///
    /// A session id is random and carries no date, so this — and the
    /// timestamp on the first transcript row — is where a session's age
    /// lives. Sessions made before this field existed have no value here
    /// and are dated from their transcript instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    hidden_from_chats: bool,
    /// Which agent runs this session's turns:
    /// [`SESSION_AGENT_LOCAL`] for Rebon's own engine, otherwise the id
    /// of a configured ACP agent.
    ///
    /// Absent means "never chosen" — the caller falls back to whatever
    /// the config's default is. That is deliberately different from an
    /// explicit `"local"`, which is a user who switched *back* and must
    /// not be dragged onto the configured default on the next resume.
    #[serde(skip_serializing_if = "Option::is_none")]
    acp_agent: Option<String>,
    /// The external agent's own id for this session.
    ///
    /// Meaningless to Rebon, and that is the point: handing it back on
    /// resume is the difference between the agent replaying its stored
    /// session and starting from nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    acp_session_id: Option<String>,
    /// The permission mode this session entered plan mode from, kept while
    /// it is in plan mode by a host that rebuilds the session every turn.
    /// See [`save_session_plan_entered_from`].
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_entered_from: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

/// Sentinel [`save_session_agent`] value meaning "Rebon's own engine".
///
/// Stored rather than implied so an explicit switch back to local
/// survives a restart instead of decaying into "unset".
pub const SESSION_AGENT_LOCAL: &str = "local";

fn load_session_meta(projects_root: &Path, cwd: &str, session_id: &str) -> Option<SessionMetaFile> {
    let path = session_meta_path(projects_root, cwd, session_id);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Update one session metadata sidecar without losing fields written by another
/// Rebon surface. Missing files start empty; unreadable or malformed files are
/// left untouched and reported to the caller.
pub fn update_session_metadata<F>(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    mutate: F,
) -> std::io::Result<()>
where
    F: FnOnce(&mut serde_json::Map<String, Value>),
{
    let dir = project_dir_path(projects_root, cwd);
    std::fs::create_dir_all(&dir)?;
    let path = session_meta_path(projects_root, cwd, session_id);
    let lock_path = path.with_extension("json.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;

    let result = (|| {
        let mut object = match std::fs::read(&path) {
            Ok(bytes) => {
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                value.as_object().cloned().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "session metadata must be a JSON object",
                    )
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(error) => return Err(error),
        };
        mutate(&mut object);
        write_session_metadata_unlocked(&path, &object)
    })();

    let _ = FileExt::unlock(&lock);
    result
}

fn write_session_metadata_unlocked(
    path: &Path,
    object: &serde_json::Map<String, Value>,
) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(object)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write_file_atomically(path, &body)?;
    sync_parent_directory(path);
    Ok(())
}

/// Read the sidecar metadata file for `(cwd, session_id)` and return the
/// cached title, if any. A missing file (the common case) returns `None`
/// silently — that's a normal state, not an error. Malformed JSON also
/// returns `None` so a corrupted sidecar never blocks `session/list`.
pub fn load_session_title(projects_root: &Path, cwd: &str, session_id: &str) -> Option<String> {
    let meta = load_session_meta(projects_root, cwd, session_id)?;
    meta.title.filter(|s| !s.trim().is_empty())
}

/// When this session was created, in milliseconds since the epoch, as its
/// metadata sidecar records it.
///
/// `None` for a session made before the field existed, and for one whose
/// sidecar is missing or malformed. Those are dated from their transcript's
/// first row ([`transcript_first_timestamp_ms`]) and, failing that, from the
/// transcript file's own dates.
pub fn load_session_created_at_ms(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Option<u64> {
    let meta = load_session_meta(projects_root, cwd, session_id)?;
    iso8601_to_epoch_ms(meta.created_at.as_deref()?)
}

/// Record when a session was created in its metadata sidecar, unless it
/// already records one.
///
/// Called where a session is named, so that a session nobody has spoken in
/// yet still knows its own age — a transcript with no rows has no timestamp
/// to offer, and the id has none by design. Never overwrites: an existing
/// value is the truth, and rewriting it on a later resume would move the
/// session to the top of every list sorted by age.
pub fn record_session_created_at(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> std::io::Result<()> {
    update_session_metadata(projects_root, cwd, session_id, |meta| {
        meta.entry("createdAt")
            .or_insert_with(|| Value::String(format_system_time_iso_ms(SystemTime::now())));
    })
}

/// How far into a transcript to look for the first stamped row. Every row a
/// Rebon writer appends carries a timestamp, so needing to look further than
/// this means the file is not a transcript.
const TRANSCRIPT_HEAD_ROWS_SCANNED_FOR_CREATION: usize = 80;

/// The epoch milliseconds on the first transcript row that carries a
/// timestamp, which is when the session first said anything.
///
/// Reads the head of the file instead of loading and reconstructing the
/// whole transcript: the answer is on the first row, and a caller asking
/// how old a session is does not want its messages.
pub fn transcript_first_timestamp_ms(path: &Path) -> Option<u64> {
    use std::io::{BufRead, BufReader};

    let file = File::open(path).ok()?;
    for line in BufReader::new(file)
        .lines()
        .take(TRANSCRIPT_HEAD_ROWS_SCANNED_FOR_CREATION)
        .map_while(Result::ok)
    {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(stamp) = value.get("timestamp").and_then(Value::as_str) else {
            continue;
        };
        if let Some(created_at_ms) = iso8601_to_epoch_ms(stamp) {
            return Some(created_at_ms);
        }
    }
    None
}

/// Milliseconds since the epoch for an ISO-8601 timestamp in the shape
/// [`format_system_time_iso_ms`] writes. Zero is rejected along with the
/// unparseable: a session created at the epoch is a session whose clock
/// was not readable.
fn iso8601_to_epoch_ms(input: &str) -> Option<u64> {
    let created = parse_rfc3339_to_system_time(input)?;
    let millis = created.duration_since(UNIX_EPOCH).ok()?.as_millis();
    u64::try_from(millis).ok().filter(|millis| *millis > 0)
}

/// Read the stored session mode (`"coordinator"` / `"normal"`) from the
/// sidecar metadata file.
pub fn load_session_mode(projects_root: &Path, cwd: &str, session_id: &str) -> Option<String> {
    let meta = load_session_meta(projects_root, cwd, session_id)?;
    meta.mode
        .filter(|s| matches!(s.as_str(), "coordinator" | "normal"))
}

/// Whether this internal session should be omitted from ordinary chat lists.
pub fn load_session_hidden_from_chats(projects_root: &Path, cwd: &str, session_id: &str) -> bool {
    load_session_meta(projects_root, cwd, session_id).is_some_and(|meta| meta.hidden_from_chats)
}

/// Write `title` to the sidecar metadata file for `(cwd, session_id)` while
/// preserving the session's other metadata. Creates the parent directory if
/// missing.
///
/// Errors are returned to the caller so they can decide whether to log
/// or retry; the title-generation background task treats any failure as
/// "no-op, try again next turn".
pub fn save_session_title(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    title: &str,
) -> std::io::Result<()> {
    update_session_metadata(projects_root, cwd, session_id, |meta| {
        meta.insert("title".into(), Value::String(title.to_string()));
    })
}

/// Persist the current session mode to the sidecar metadata file.
pub fn save_session_mode(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    mode: &str,
) -> std::io::Result<()> {
    let mode = match mode {
        "coordinator" | "normal" => mode,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session mode must be coordinator or normal",
            ));
        }
    };
    update_session_metadata(projects_root, cwd, session_id, |meta| {
        meta.insert("mode".into(), Value::String(mode.to_string()));
    })
}

/// Where this session entered plan mode from, as last saved.
pub fn load_session_plan_entered_from(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Option<String> {
    load_session_meta(projects_root, cwd, session_id)?.plan_entered_from
}

/// Remember where this session entered plan mode from; `None` forgets it.
///
/// A background job rebuilds its session every turn, and a rebuilt record
/// comes back in plan mode by being set there — which reads as entering it
/// from `default`. The mode it really came from decides whether auto mode's
/// classifier keeps answering while it plans, so it outlives the record here,
/// beside the rest of what a resume reads back.
pub fn save_session_plan_entered_from(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    plan_entered_from: Option<&str>,
) -> std::io::Result<()> {
    update_session_metadata(
        projects_root,
        cwd,
        session_id,
        |meta| match plan_entered_from {
            Some(mode) => {
                meta.insert("planEnteredFrom".into(), Value::String(mode.to_string()));
            }
            None => {
                meta.remove("planEnteredFrom");
            }
        },
    )
}

/// Persist whether this session is an internal workflow hidden from Chats.
pub fn save_session_hidden_from_chats(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    hidden: bool,
) -> std::io::Result<()> {
    update_session_metadata(projects_root, cwd, session_id, |meta| {
        if hidden {
            meta.insert("hiddenFromChats".into(), Value::Bool(true));
        } else {
            meta.remove("hiddenFromChats");
        }
    })
}

/// Which agent this session was last running on.
///
/// `None` means the session predates the choice, or never made one —
/// resume should fall back to the configured default. `Some("local")`
/// is an explicit choice and must be honoured as one.
pub fn load_session_agent(projects_root: &Path, cwd: &str, session_id: &str) -> Option<String> {
    let meta = load_session_meta(projects_root, cwd, session_id)?;
    meta.acp_agent.filter(|id| !id.trim().is_empty())
}

/// Record which agent runs this session, so `--resume` reopens it on
/// the same one instead of silently moving the conversation to a
/// different agent.
pub fn save_session_agent(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    agent: &str,
) -> std::io::Result<()> {
    let agent = agent.trim();
    if agent.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session agent id must not be empty",
        ));
    }
    update_session_metadata(projects_root, cwd, session_id, |meta| {
        if meta.get("acpAgent").and_then(Value::as_str) != Some(agent) {
            // Switching agents retires the stored external session id: it
            // belongs to the agent we are leaving, and offering it to the
            // next one would ask a stranger to load a session it never had.
            meta.remove("acpSessionId");
        }
        meta.insert("acpAgent".into(), Value::String(agent.to_string()));
    })
}

/// The external agent's own session id, when one was stored.
pub fn load_agent_session_id(projects_root: &Path, cwd: &str, session_id: &str) -> Option<String> {
    let meta = load_session_meta(projects_root, cwd, session_id)?;
    meta.acp_session_id.filter(|id| !id.trim().is_empty())
}

/// Remember the external agent's own session id for this session.
///
/// This is the whole of cross-restart resume for an ACP agent: Rebon
/// cannot replay a conversation into somebody else's process, but it
/// can hand back the id and let the agent do it.
pub fn save_agent_session_id(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    agent_session_id: &str,
) -> std::io::Result<()> {
    let agent_session_id = agent_session_id.trim();
    if agent_session_id.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "agent session id must not be empty",
        ));
    }
    update_session_metadata(projects_root, cwd, session_id, |meta| {
        meta.insert(
            "acpSessionId".into(),
            Value::String(agent_session_id.to_string()),
        );
    })
}

#[derive(Debug)]
pub enum UltraplanRunStoreError {
    Io(std::io::Error),
    StaleRevision { expected: u64, actual: u64 },
    RunIdMismatch { expected: String, actual: String },
}

impl std::fmt::Display for UltraplanRunStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::StaleRevision { expected, actual } => write!(
                f,
                "stale ultraplan revision: expected {expected}, current revision is {actual}"
            ),
            Self::RunIdMismatch { expected, actual } => write!(
                f,
                "ultraplan run id mismatch: expected `{expected}`, got `{actual}`"
            ),
        }
    }
}

impl std::error::Error for UltraplanRunStoreError {}

impl From<std::io::Error> for UltraplanRunStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn save_ultraplan_run(
    projects_root: &Path,
    cwd: &str,
    state: &UltraplanRunState,
) -> std::io::Result<()> {
    let dir = ultraplan_run_dir_path(projects_root, cwd);
    std::fs::create_dir_all(&dir)?;
    let path = ultraplan_run_path(projects_root, cwd, &state.run_id);
    let lock_path = path.with_extension("json.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    let result = write_ultraplan_run_unlocked(&path, state);
    let _ = FileExt::unlock(&lock);
    result
}

pub fn save_ultraplan_run_cas(
    projects_root: &Path,
    cwd: &str,
    expected_revision: u64,
    state: &UltraplanRunState,
) -> Result<(), UltraplanRunStoreError> {
    let dir = ultraplan_run_dir_path(projects_root, cwd);
    std::fs::create_dir_all(&dir)?;
    let path = ultraplan_run_path(projects_root, cwd, &state.run_id);
    let lock_path = path.with_extension("json.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;

    let result = (|| {
        if let Ok(bytes) = std::fs::read(&path) {
            let current = decode_ultraplan_run_state(&bytes)
                .map_err(|err| {
                    UltraplanRunStoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        err,
                    ))
                })?
                .ok_or_else(|| {
                    UltraplanRunStoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unsupported ultraplan run state version",
                    ))
                })?;
            if current.run_id != state.run_id {
                return Err(UltraplanRunStoreError::RunIdMismatch {
                    expected: current.run_id,
                    actual: state.run_id.clone(),
                });
            }
            if current.state_revision != expected_revision {
                return Err(UltraplanRunStoreError::StaleRevision {
                    expected: expected_revision,
                    actual: current.state_revision,
                });
            }
        } else if expected_revision != 0 {
            return Err(UltraplanRunStoreError::StaleRevision {
                expected: expected_revision,
                actual: 0,
            });
        }
        write_ultraplan_run_unlocked(&path, state).map_err(UltraplanRunStoreError::Io)
    })();

    let _ = FileExt::unlock(&lock);
    result
}

fn write_ultraplan_run_unlocked(path: &Path, state: &UltraplanRunState) -> std::io::Result<()> {
    let mut state = state.clone();
    state.version = rebon_types::ULTRAPLAN_RUN_STATE_VERSION;
    state.prepare_for_persist();
    let body = serde_json::to_vec_pretty(&state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_file_atomically(path, &body)?;
    sync_parent_directory(path);
    Ok(())
}

pub fn load_ultraplan_run(
    projects_root: &Path,
    cwd: &str,
    run_id: &str,
) -> Option<UltraplanRunState> {
    let path = ultraplan_run_path(projects_root, cwd, run_id);
    let bytes = std::fs::read(&path).ok()?;
    match decode_ultraplan_run_state(&bytes) {
        Ok(Some(state)) if state.run_id == run_id && state.identity.run_id == run_id => Some(state),
        Ok(Some(state)) => {
            tracing::warn!(
                run_id = %run_id,
                stored_run_id = %state.run_id,
                identity_run_id = %state.identity.run_id,
                path = %path.display(),
                "rejecting ultraplan RunState identity mismatch"
            );
            None
        }
        Ok(None) => {
            tracing::warn!(run_id = %run_id, path = %path.display(), "ignoring unsupported ultraplan run state version");
            None
        }
        Err(err) => {
            tracing::warn!(run_id = %run_id, path = %path.display(), error = %err, "failed to decode ultraplan run state");
            None
        }
    }
}

pub fn list_ultraplan_runs(projects_root: &Path, cwd: &str) -> Vec<RunSummary> {
    let dir = ultraplan_run_dir_path(projects_root, cwd);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut runs: Vec<RunSummary> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                return None;
            }
            let bytes = std::fs::read(&path).ok()?;
            match decode_ultraplan_run_state(&bytes) {
                Ok(Some(state)) => {
                    let path_run_id = path.file_stem().and_then(|stem| stem.to_str());
                    if path_run_id != Some(state.run_id.as_str())
                        || state.identity.run_id != state.run_id
                    {
                        tracing::warn!(
                            path = %path.display(),
                            stored_run_id = %state.run_id,
                            identity_run_id = %state.identity.run_id,
                            "ignoring ultraplan RunState with mismatched identity"
                        );
                        None
                    } else {
                        Some(RunSummary::from(&state))
                    }
                }
                Ok(None) => {
                    tracing::warn!(path = %path.display(), "ignoring unsupported ultraplan run state version");
                    None
                }
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "failed to decode ultraplan run state");
                    None
                }
            }
        })
        .collect();
    runs.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    runs
}

pub fn latest_active_run_for_session(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Option<UltraplanRunState> {
    let mut latest: Option<UltraplanRunState> = None;
    for summary in list_ultraplan_runs(projects_root, cwd) {
        if summary.session_id != session_id {
            continue;
        }
        let Some(state) = load_ultraplan_run(projects_root, cwd, &summary.run_id) else {
            continue;
        };
        if !state.is_active() {
            continue;
        }
        if latest
            .as_ref()
            .is_none_or(|current| state.updated_at_ms > current.updated_at_ms)
        {
            latest = Some(state);
        }
    }
    latest
}

/// Extract the first user-message text from the on-disk transcript for
/// `(cwd, session_id)`. Used by the resume dialog as the
/// display fallback when no sidecar title exists.
///
/// Reads the jsonl, walks entries in file order, and returns the first
/// entry whose `type` is `"user"` and whose `message.content` resolves
/// to a non-empty string. Returns `None` if the file is missing, has
/// no user entries, or every user entry is a tool-result wrapper with
/// no human text.
///
/// Deliberately does **not** run the full chain walker from
/// [`reconstruct_chain`] — we want the *oldest* user message (the one
/// that opened the session), not the latest leaf's ancestor. File order
/// matches the append-only writer, so the first line *is* the earliest
/// entry.
pub fn extract_first_user_message_text(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> Option<String> {
    let path = transcript_file_path(projects_root, cwd, session_id);
    let bytes = std::fs::read(&path).ok()?;
    let entries = parse_transcript_jsonl(&bytes);
    for entry in entries {
        if entry.entry_type != "user" {
            continue;
        }
        if let Some(text) = extract_user_text_from_raw(&entry.raw) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Pull plain user text out of a transcript entry's raw JSON payload.
///
/// Handles the three shapes that [`TranscriptWriteEntry`] can produce:
/// * `{ "message": { "content": "hello" } }` (string content)
/// * `{ "message": { "content": [{ "type": "text", "text": "hello" }] } }`
///   (array-of-blocks content — concatenates every `text` block)
/// * `{ "message": { "content": [{ "type": "tool_result", ... }] } }`
///   (no human text — returns `None`)
///
/// The function is intentionally tolerant: if `message` is absent or the
/// payload is in a shape we don't recognise, it returns `None` and the
/// caller moves on to the next entry.
fn extract_user_text_from_raw(raw: &Value) -> Option<String> {
    let message = raw.get("message")?;
    let content = message.get("content")?;

    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for block in arr {
            let ty = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ty == "text" {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Transcript parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
}

impl TranscriptFileIdentity {
    fn from_file(file: &File) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let metadata = file.metadata().ok()?;
            return Some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            });
        }
        #[cfg(windows)]
        {
            let (volume_serial_number, file_index) = windows_transcript_file_identity(file)?;
            return Some(Self {
                volume_serial_number,
                file_index,
            });
        }
        #[allow(unreachable_code)]
        None
    }
}

#[cfg(windows)]
fn windows_transcript_file_identity(file: &File) -> Option<(u32, u64)> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            file: *mut c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::uninit();
    let result = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
    };
    if result == 0 {
        return None;
    }
    let information = unsafe { information.assume_init() };
    Some((
        information.volume_serial_number,
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low),
    ))
}

pub const TRANSCRIPT_APPEND_JOURNAL_STEPS: usize = 256;
/// Hard cap for cooperative append sidecars, including one full journal and
/// its fixed-size generation metadata. Readers enforce this cap even for
/// externally forged files, so sidecar work remains bounded.
const TRANSCRIPT_APPEND_GENERATION_MAX_BYTES: usize = TRANSCRIPT_APPEND_JOURNAL_STEPS * 768 + 2048;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptAppendStep {
    pub generation: u64,
    pub from_len: u64,
    pub to_len: u64,
    pub previous_chain_sha256: Option<[u8; 32]>,
    pub append_sha256: [u8; 32],
    pub chain_sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptAppendGeneration {
    pub epoch: String,
    pub generation: u64,
    pub len: u64,
    pub modified_ns: u64,
    pub file_identity: TranscriptFileIdentity,
    /// A bounded, contiguous suffix of cooperative append steps, oldest first.
    /// Generation zero is only an anchor: readers adopt it only after a stable
    /// full parse, then may authenticate later generations retained here.
    pub journal: Vec<TranscriptAppendStep>,
}

/// The append-generation sidecar path for a transcript. Public so readers can
/// stat the sidecar to detect changes without loading and validating it.
pub fn transcript_append_generation_path(path: &Path) -> PathBuf {
    path.with_extension("jsonl.append-generation.json")
}

fn invalidate_transcript_append_proof(path: &Path) {
    std::fs::remove_file(transcript_append_generation_path(path)).ok();
}

fn transcript_metadata_revision(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    let modified_ns = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()?;
    Some((metadata.len(), modified_ns))
}

/// Read the cooperative append generation for `path`.
///
/// The record is useful only when its length, modification time, and filesystem
/// identity still match the transcript. Unsupported writers, rewrites, path
/// replacements, and publication races return `None`, forcing callers onto their
/// full-read path. A same-identity external mutation that preserves the visible
/// revision is outside the cooperative writer contract; appenders deliberately do
/// not reread the old prefix to defend against it.
pub fn load_transcript_append_generation(path: &Path) -> Option<TranscriptAppendGeneration> {
    let file = File::open(path).ok()?;
    load_transcript_append_generation_for_file(path, &file)
}

fn load_transcript_append_generation_for_file(
    path: &Path,
    file: &File,
) -> Option<TranscriptAppendGeneration> {
    let metadata = file.metadata().ok()?;
    let (len, modified_ns) = transcript_metadata_revision(&metadata)?;
    let file_identity = TranscriptFileIdentity::from_file(file)?;
    let mut sidecar = File::open(transcript_append_generation_path(path)).ok()?;
    let mut bytes = Vec::new();
    sidecar
        .by_ref()
        .take((TRANSCRIPT_APPEND_GENERATION_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > TRANSCRIPT_APPEND_GENERATION_MAX_BYTES {
        return None;
    }
    let generation: TranscriptAppendGeneration = serde_json::from_slice(&bytes).ok()?;
    (generation.len == len
        && generation.modified_ns == modified_ns
        && generation.file_identity == file_identity
        && transcript_append_journal_is_valid(&generation))
    .then_some(generation)
}

fn fresh_transcript_epoch(path: &Path, modified_ns: u64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_EPOCH: AtomicU64 = AtomicU64::new(0);
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{now_ns}-{modified_ns}-{}-{}",
        std::process::id(),
        NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
        simple_hash(&path.to_string_lossy())
    )
}

fn transcript_append_chain_sha256(
    epoch: &str,
    generation: u64,
    from_len: u64,
    to_len: u64,
    previous_chain_sha256: Option<[u8; 32]>,
    append_sha256: [u8; 32],
) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"rebon-transcript-append-chain-v2\0");
    digest.update((epoch.len() as u64).to_be_bytes());
    digest.update(epoch.as_bytes());
    digest.update(generation.to_be_bytes());
    digest.update(from_len.to_be_bytes());
    digest.update(to_len.to_be_bytes());
    match previous_chain_sha256 {
        Some(previous_digest) => {
            digest.update([1]);
            digest.update(previous_digest);
        }
        None => digest.update([0]),
    }
    digest.update(append_sha256);
    digest.finalize().into()
}

fn transcript_append_journal_is_valid(current: &TranscriptAppendGeneration) -> bool {
    if current.journal.is_empty() || current.journal.len() > TRANSCRIPT_APPEND_JOURNAL_STEPS {
        return false;
    }
    for (index, step) in current.journal.iter().enumerate() {
        if step.to_len < step.from_len
            || step.chain_sha256
                != transcript_append_chain_sha256(
                    &current.epoch,
                    step.generation,
                    step.from_len,
                    step.to_len,
                    step.previous_chain_sha256,
                    step.append_sha256,
                )
        {
            return false;
        }
        if let Some(previous) = index.checked_sub(1).and_then(|i| current.journal.get(i)) {
            if previous.generation.checked_add(1) != Some(step.generation)
                || step.from_len != previous.to_len
                || step.previous_chain_sha256 != Some(previous.chain_sha256)
            {
                return false;
            }
        } else if step.generation == 0 && step.previous_chain_sha256.is_some() {
            return false;
        }
    }
    current
        .journal
        .last()
        .is_some_and(|step| step.generation == current.generation && step.to_len == current.len)
}

/// Validate all retained cooperative append steps after `previous` against the
/// combined transcript `suffix`. The bytes are split by each authenticated
/// step's from/to lengths. Missing or non-contiguous generations, an evicted
/// first step, changed identity, malformed/oversized journals, or any digest or
/// length mismatch fail closed so the caller can perform a stable full parse.
pub fn validate_transcript_append_span(
    previous: &TranscriptAppendGeneration,
    current: &TranscriptAppendGeneration,
    suffix: &[u8],
) -> bool {
    let Some(expected_current_len) = previous.len.checked_add(suffix.len() as u64) else {
        return false;
    };
    if previous.epoch != current.epoch
        || previous.file_identity != current.file_identity
        || current.generation <= previous.generation
        || current.len != expected_current_len
        || !transcript_append_journal_is_valid(previous)
        || !transcript_append_journal_is_valid(current)
    {
        return false;
    }
    let Some(previous_latest) = previous.journal.last() else {
        return false;
    };
    let Some(needed_generation) = previous.generation.checked_add(1) else {
        return false;
    };
    let Some(first_index) = current
        .journal
        .iter()
        .position(|step| step.generation == needed_generation)
    else {
        return false;
    };
    let steps = &current.journal[first_index..];
    if steps.len() as u64 != current.generation - previous.generation
        || steps.first().is_none_or(|step| {
            step.from_len != previous.len
                || step.previous_chain_sha256 != Some(previous_latest.chain_sha256)
        })
    {
        return false;
    }

    let mut offset = 0usize;
    for step in steps {
        let Ok(step_len) = usize::try_from(step.to_len - step.from_len) else {
            return false;
        };
        let Some(end) = offset.checked_add(step_len) else {
            return false;
        };
        let Some(appended) = suffix.get(offset..end) else {
            return false;
        };
        let append_sha256: [u8; 32] = Sha256::digest(appended).into();
        if append_sha256 != step.append_sha256 {
            return false;
        }
        offset = end;
    }
    offset == suffix.len()
}

fn publish_transcript_append_generation(
    path: &Path,
    file: &File,
    previous: Option<&TranscriptAppendGeneration>,
    appended: &[u8],
) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    let (len, modified_ns) = transcript_metadata_revision(&metadata).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transcript modification time is not representable",
        )
    })?;
    let file_identity = TranscriptFileIdentity::from_file(file).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transcript file identity is unavailable",
        )
    })?;
    // Bind the predecessor, append handle, and final pathname to one file. A
    // replacement between open and publication must not inherit this proof.
    let named_file = File::open(path)?;
    let named_metadata = named_file.metadata()?;
    let named_revision = transcript_metadata_revision(&named_metadata);
    let named_identity = TranscriptFileIdentity::from_file(&named_file);
    if named_revision != Some((len, modified_ns)) || named_identity.as_ref() != Some(&file_identity)
    {
        return Err(std::io::Error::other(
            "transcript path no longer names the appended file",
        ));
    }
    let append_sha256: [u8; 32] = Sha256::digest(appended).into();
    let from_len = len.checked_sub(appended.len() as u64).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "appended bytes exceed transcript length",
        )
    })?;
    let (epoch, generation, previous_chain_sha256, mut journal) = match previous {
        Some(previous) => (
            previous.epoch.clone(),
            previous.generation.checked_add(1).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "transcript append generation overflow",
                )
            })?,
            previous.journal.last().map(|step| step.chain_sha256),
            previous.journal.clone(),
        ),
        None => (
            fresh_transcript_epoch(path, modified_ns),
            0,
            None,
            Vec::with_capacity(TRANSCRIPT_APPEND_JOURNAL_STEPS),
        ),
    };
    let chain_sha256 = transcript_append_chain_sha256(
        &epoch,
        generation,
        from_len,
        len,
        previous_chain_sha256,
        append_sha256,
    );
    journal.push(TranscriptAppendStep {
        generation,
        from_len,
        to_len: len,
        previous_chain_sha256,
        append_sha256,
        chain_sha256,
    });
    if journal.len() > TRANSCRIPT_APPEND_JOURNAL_STEPS {
        journal.drain(..journal.len() - TRANSCRIPT_APPEND_JOURNAL_STEPS);
    }
    let generation = TranscriptAppendGeneration {
        epoch,
        generation,
        len,
        modified_ns,
        file_identity,
        journal,
    };
    let serialized = serde_json::to_vec(&generation)?;
    if serialized.len() > TRANSCRIPT_APPEND_GENERATION_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transcript append generation sidecar exceeds size limit",
        ));
    }
    let target = transcript_append_generation_path(path);
    write_file_atomically(&target, &serialized)?;
    sync_parent_directory(&target);
    Ok(())
}

/// A single on-disk transcript entry.
///
/// A transcript message is a rich union of user/assistant/attachment/
/// system variants with a `message` payload whose shape differs per
/// variant. For `session/load` we only need the four fields the
/// chain walk touches — everything else is kept in `raw` so callers
/// can reach in without requiring schema changes here.
#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    /// `type` — usually one of `"user" | "assistant" | "attachment" | "system"`.
    pub entry_type: String,
    /// `uuid` — primary key in the on-disk chain.
    pub uuid: String,
    /// `parentUuid` — `None` for root entries.
    pub parent_uuid: Option<String>,
    /// `timestamp` — lexicographically sortable ISO-8601 (millisecond UTC
    /// when the writer mints it). Present on every transcript line written
    /// by [`append_transcript_entry`] / [`write_transcript_entries`], but we
    /// tolerate its absence on malformed or hand-authored fixtures.
    pub timestamp: Option<String>,
    /// The full parsed JSON line, kept as an opaque `Value` so the
    /// handler can later surface per-entry payloads without this module
    /// having to mint a richer schema.
    pub raw: Value,
}

/// Raw best-effort transcript parse plus facts captured by the same file read.
/// Recovery callers use these facts to distinguish a genuinely empty history
/// from a nonempty source for which every row was malformed or unsupported.
#[derive(Debug, Clone)]
pub struct RawTranscriptFile {
    pub entries: Vec<TranscriptEntry>,
    pub byte_len: usize,
    pub parsed_row_count: usize,
    /// Number of nonblank JSONL records observed in the same byte buffer.
    pub nonblank_row_count: usize,
    /// True only when the complete buffer is UTF-8 and every nonblank record
    /// has the supported transcript shape. Recovery must not silently accept a
    /// valid prefix after dropping a malformed former tail.
    pub parse_complete: bool,
}

/// Outcome of a successful transcript load.
#[derive(Debug, Clone)]
pub struct LoadedTranscript {
    /// The reconstructed chain of user/assistant messages, ordered
    /// oldest → newest, the order a protocol session replays them in.
    pub messages: Vec<TranscriptEntry>,
    /// Best-effort created timestamp derived from the oldest transcript
    /// entry's ISO-8601 `timestamp`. Falls back to `UNIX_EPOCH` when the
    /// transcript is malformed or lacks timestamps.
    pub created_at: std::time::SystemTime,
    /// Optional custom title — always `None` currently because
    /// metadata lines are not parsed.
    pub title: Option<String>,
}

/// Strong identity for the exact transcript bytes used to project history.
/// Metadata is retained for diagnostics and fast cache checks, while mutation
/// compare-and-swap must compare `sha256` as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptStamp {
    pub byte_len: u64,
    pub modified: Option<SystemTime>,
    pub sha256: [u8; 32],
}

/// A stable rewind boundary derived from the canonical UUID/parentUuid chain.
/// Selecting this turn means branching from `parent_uuid`, i.e. immediately
/// before the selected user message; rendered text and vector indices are not
/// part of its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTurn {
    pub user_uuid: String,
    pub parent_uuid: Option<String>,
    pub timestamp: Option<String>,
    pub prompt: String,
    pub turn_number: usize,
    pub completion_state: HistoryTurnCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryTurnCompletion {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone)]
pub struct SessionHistorySnapshot {
    pub transcript_path: PathBuf,
    pub stamp: TranscriptStamp,
    pub source_head_uuid: String,
    pub turns: Vec<HistoryTurn>,
}

/// Immutable compare-and-swap target displayed by the workbench. Text is
/// carried only for confirmation/prefill; UUID ancestry and SHA-256 are the
/// mutation identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindConversationRequest {
    pub mutation_id: String,
    pub session_id: String,
    pub cwd: String,
    pub selected_user_uuid: String,
    pub boundary_parent_uuid: Option<String>,
    pub selected_prompt: String,
    pub expected_sha256: [u8; 32],
    pub expected_source_head_uuid: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummarizeConversationMode {
    FromSelected,
    UpToSelected,
}

/// Immutable compare-and-swap request for a durable transcript summary.
/// `note_raw` is retained opaquely so rich transcript payloads remain owned by
/// their producer rather than being projected through a narrower schema.
#[derive(Debug, Clone)]
pub struct SummarizeConversationRequest {
    pub mutation_id: String,
    pub session_id: String,
    pub cwd: String,
    pub selected_user_uuid: String,
    pub selected_parent_uuid: Option<String>,
    pub expected_sha256: [u8; 32],
    pub expected_source_head_uuid: String,
    pub mode: SummarizeConversationMode,
    pub note_raw: Value,
}

#[derive(Debug, Clone)]
pub struct SummarizeConversationReceipt {
    pub mutation_id: String,
    pub old_stamp: TranscriptStamp,
    pub new_stamp: TranscriptStamp,
    pub new_source_head_uuid: String,
    pub recovery_backup: PathBuf,
    /// Exact canonical records committed to disk, in target order.
    pub entries: Vec<TranscriptEntry>,
    pub dropped_count: usize,
}

#[derive(Debug, Clone)]
pub struct RewindConversationReceipt {
    pub mutation_id: String,
    pub old_stamp: TranscriptStamp,
    pub new_stamp: TranscriptStamp,
    pub new_source_head_uuid: String,
    pub canonical_history: SessionHistorySnapshot,
    pub prefill_prompt: String,
    pub recovery_backup: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RewindConversationError {
    #[error("this session is owned by a live foreground or background runtime")]
    Busy,
    #[error("the supplied active-session lock does not own this session")]
    WrongSessionLock,
    #[error("the transcript revision changed; reload history and select again")]
    StaleRevision,
    #[error("the selected turn no longer exists")]
    TurnMissing,
    #[error("the selected turn parent changed")]
    ParentChanged,
    #[error("the selected rewind boundary is not on the canonical transcript chain")]
    InvalidBoundary,
    #[error("the transcript path does not match the requested session")]
    TranscriptPathMismatch,
    #[error("the mutation id must not be empty")]
    InvalidMutationId,
    #[error("the transcript cannot be safely rewritten: {0}")]
    InvalidTranscript(String),
    #[error("rewind recovery requires manual inspection: {0}")]
    RecoveryRequired(String),
    #[error("the transcript was committed but projection/finalization requires recovery: {0}")]
    CommittedRecoveryRequired(String),
    #[error("transcript I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RewindJournal {
    mutation_id: String,
    request_fingerprint: [u8; 32],
    selected_prompt: String,
    old_sha256: [u8; 32],
    old_byte_len: u64,
    target_sha256: [u8; 32],
    target_head_uuid: String,
    backup_path: PathBuf,
    temp_path: PathBuf,
    committed: bool,
}

fn history_from_bytes(
    path: &Path,
    bytes: &[u8],
    metadata: &std::fs::Metadata,
) -> SessionHistorySnapshot {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let loaded = reconstruct_chain(parse_transcript_jsonl(bytes));
    let messages = loaded.map(|loaded| loaded.messages).unwrap_or_default();
    let source_head_uuid = messages
        .last()
        .map(|entry| entry.uuid.clone())
        .unwrap_or_default();
    let mut turns = Vec::new();
    for (index, entry) in messages.iter().enumerate() {
        let Some(prompt) = turn_prompt_text(entry) else {
            continue;
        };
        let has_later_response = messages[index + 1..]
            .iter()
            .take_while(|later| later.entry_type != "user")
            .any(|later| later.entry_type == "assistant");
        turns.push(HistoryTurn {
            user_uuid: entry.uuid.clone(),
            parent_uuid: entry.parent_uuid.clone(),
            timestamp: entry.timestamp.clone(),
            prompt,
            turn_number: turns.len() + 1,
            completion_state: if has_later_response {
                HistoryTurnCompletion::Complete
            } else {
                HistoryTurnCompletion::Incomplete
            },
        });
    }
    SessionHistorySnapshot {
        transcript_path: path.to_path_buf(),
        stamp: TranscriptStamp {
            byte_len: bytes.len() as u64,
            modified: metadata.modified().ok(),
            sha256: digest,
        },
        source_head_uuid,
        turns,
    }
}

/// Load a canonical, UUID-backed history projection and the strong revision of
/// the bytes from which it was derived. An empty/malformed transcript is a
/// valid empty projection rather than a cache error.
pub fn load_session_history(path: &Path) -> std::io::Result<SessionHistorySnapshot> {
    let bytes = std::fs::read(path)?;
    let metadata = std::fs::metadata(path)?;
    Ok(history_from_bytes(path, &bytes, &metadata))
}

/// The prompt a chain entry opens a turn with, if it opens one: a user entry
/// carrying non-blank text. Tool results are user entries with no text, so
/// they continue the turn they answer. [`load_session_history`] numbers turns
/// by this and [`last_turn_assistant_text`] finds the last one by it, so the
/// two cannot disagree about where a turn starts.
fn turn_prompt_text(entry: &TranscriptEntry) -> Option<String> {
    if entry.entry_type != "user" {
        return None;
    }
    extract_user_text_from_raw(&entry.raw).filter(|prompt| !prompt.trim().is_empty())
}

/// Everything the model said as text in the transcript's last turn.
///
/// The turn runs from the last prompt [`load_session_history`] would number
/// as a turn to the end of the canonical chain — the turn `/rewind` would
/// list last. Each assistant entry's text blocks become one paragraph,
/// oldest first; tool calls, tool results and thinking are not text and are
/// skipped, so what is left is what a reader of the turn would have seen the
/// model write.
///
/// `Ok(None)` when there is no transcript, no turn, or a turn that produced no
/// text — one that ended on a tool call, or was cancelled before the model
/// answered.
pub fn last_turn_assistant_text(path: &Path) -> std::io::Result<Option<String>> {
    let Some(loaded) = load_transcript_from_file(path)? else {
        return Ok(None);
    };
    let messages = loaded.messages;
    let Some(start) = messages
        .iter()
        .rposition(|entry| turn_prompt_text(entry).is_some())
    else {
        return Ok(None);
    };
    let paragraphs: Vec<String> = messages[start + 1..]
        .iter()
        .filter(|entry| entry.entry_type == "assistant")
        // Named for its first caller; it reads the text blocks of any message.
        .filter_map(|entry| extract_user_text_from_raw(&entry.raw))
        .filter(|text| !text.trim().is_empty())
        .collect();
    if paragraphs.is_empty() {
        return Ok(None);
    }
    Ok(Some(paragraphs.join("\n\n")))
}

fn summarize_request_fingerprint(request: &SummarizeConversationRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let mode = match request.mode {
        SummarizeConversationMode::FromSelected => b"from-selected".as_slice(),
        SummarizeConversationMode::UpToSelected => b"up-to-selected".as_slice(),
    };
    let note = serde_json::to_vec(&request.note_raw).unwrap_or_default();
    for field in [
        request.mutation_id.as_bytes(),
        request.session_id.as_bytes(),
        request.cwd.as_bytes(),
        request.selected_user_uuid.as_bytes(),
        request
            .selected_parent_uuid
            .as_deref()
            .unwrap_or("")
            .as_bytes(),
        request.expected_sha256.as_slice(),
        request.expected_source_head_uuid.as_bytes(),
        mode,
        note.as_slice(),
    ] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    hasher.finalize().into()
}

fn request_fingerprint(request: &RewindConversationRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for field in [
        request.mutation_id.as_bytes(),
        request.session_id.as_bytes(),
        request.cwd.as_bytes(),
        request.selected_user_uuid.as_bytes(),
        request
            .boundary_parent_uuid
            .as_deref()
            .unwrap_or("")
            .as_bytes(),
        request.selected_prompt.as_bytes(),
        request.expected_sha256.as_slice(),
        request.expected_source_head_uuid.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    hasher.finalize().into()
}

fn rewind_artifact_path(transcript: &Path, mutation_id: &str, suffix: &str) -> PathBuf {
    let id_hash: [u8; 32] = Sha256::digest(mutation_id.as_bytes()).into();
    let id_hash = id_hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = transcript
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("transcript.jsonl");
    transcript.with_file_name(format!("{name}.rewind-{id_hash}.{suffix}"))
}

fn write_durable(path: &Path, bytes: &[u8], create_new: bool) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(!create_new)
        .create_new(create_new);
    let mut file = options.open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()
}

fn write_journal_atomically(path: &Path, journal: &RewindJournal) -> std::io::Result<()> {
    let next = path.with_extension("json.next");
    write_durable(
        &next,
        &serde_json::to_vec_pretty(journal).map_err(std::io::Error::other)?,
        false,
    )?;
    replace_file_atomically(&next, path)?;
    sync_parent_directory(path);
    Ok(())
}

/// The longest path Win32 accepts without the `\\?\` prefix, counting the
/// terminating NUL.
#[cfg(windows)]
const WINDOWS_MAX_PATH: usize = 260;

/// The wide string a Win32 file call is given, carrying the `\\?\` prefix when
/// the path is long enough to need it.
///
/// `std::fs` adds this prefix on our behalf; a hand-written call into the API
/// does not, and `MoveFileExW` answers `ERROR_PATH_NOT_FOUND` for a path at or
/// past `MAX_PATH`. That is not a hypothetical here: a project directory is
/// named after the whole cwd, and a rewind artifact's name is 85 characters
/// longer than the transcript's, so a session opened from a deep enough
/// directory could not be rewound at all — the journal was written (through
/// `std::fs`, which prefixed it) and then could not be moved into place.
///
/// The prefix turns off the path parsing the kernel would otherwise do, so it
/// is only sound on a fully-qualified path with no `..` in it. Anything else
/// is handed over exactly as it is: that is no worse than before, while a
/// prefix on a path that still needed parsing would name a different file.
///
/// Public because it is not only this crate's problem: `rebon-session-host`
/// once hand-wrote the same `MoveFileExW` call for the job record and the
/// foreground status file, and the latter is keyed by cwd into this very
/// directory. Both now go through [`replace_file_atomically`]; the rule stays
/// public for the next hand-written Win32 call that has to name a file.
#[cfg(windows)]
pub fn wide_path_for_win32(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    let as_is = || {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    if path.as_os_str().encode_wide().count() + 1 <= WINDOWS_MAX_PATH {
        return as_is();
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return as_is();
    };
    // `..` means something only while the kernel is parsing, which is exactly
    // what the prefix switches off.
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return as_is();
    }
    // Rebuilt from components rather than patched as a string: this is what
    // drops `.` and settles on backslashes, both of which a verbatim path
    // would otherwise take literally.
    let mut rebuilt = PathBuf::new();
    for component in path.components() {
        rebuilt.push(component.as_os_str());
    }
    let mut prefixed: Vec<u16> = r"\\?\".encode_utf16().collect();
    match prefix.kind() {
        // `C:\…` becomes `\\?\C:\…`.
        Prefix::Disk(_) => prefixed.extend(rebuilt.as_os_str().encode_wide()),
        // `\\server\share\…` becomes `\\?\UNC\server\share\…`, the `UNC\`
        // standing in for the two leading separators.
        Prefix::UNC(_, _) => {
            prefixed.extend(r"UNC\".encode_utf16());
            prefixed.extend(rebuilt.as_os_str().encode_wide().skip(2));
        }
        // Already verbatim, or a device path: prefixing again would break it.
        _ => return as_is(),
    }
    prefixed.push(0);
    prefixed
}

/// Write `bytes` to `path` so a reader sees either the previous file or the
/// whole new one, never a torn one.
///
/// The bytes go to a sibling staging file (same directory, so the final move
/// is a rename within one filesystem), are flushed to the device with
/// `sync_all`, and the staging file is then moved over `path` through
/// [`replace_file_atomically`], which carries the two Windows rules — the
/// long-path prefix and the rename past a reader holding the destination open.
/// Whatever fails, the staging file is removed and the previous `path` is left
/// intact.
///
/// The parent directory has to exist already. A caller that may be the first
/// to write into it creates it beforehand, as it did when it spelled the
/// rename out by hand.
pub fn write_file_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_file_atomically_with(path, bytes, |_| Ok(()))
}

/// [`write_file_atomically`] for a file that carries a secret.
///
/// On Unix the staging file is made owner-only (`0600`) before anything is
/// written to it, so there is no instant at which the secret is readable by
/// another user. On Windows the config home already carries the profile's
/// user-only ACL, and the write is the plain one.
pub fn write_private_file_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_file_atomically_with(path, bytes, |file| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(())
        }
    })
}

fn write_file_atomically_with(
    path: &Path,
    bytes: &[u8],
    prepare: impl FnOnce(&File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let staging = staging_path_beside(path)?;
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        prepare(&file)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        // The handle has to be closed before the move: Windows refuses to
        // rename a file this process still has open for writing.
        drop(file);
        replace_file_atomically(&staging, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written
}

/// A sibling of `path` that no other writer is using.
///
/// The process id and a per-process counter keep two writers in one process
/// apart; the random tail keeps a stale file from a crashed process — or from
/// another process that was handed the same pid — from colliding with the
/// `create_new` open. The `.tmp` suffix is what every sweeper of half-written
/// files in this codebase looks for.
fn staging_path_beside(path: &Path) -> std::io::Result<PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no directory to stage a write in", path.display()),
        ));
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    // `RandomState` is seeded from the OS once per thread and never repeats
    // within a process; it is the one source of randomness `std` offers
    // without a dependency, and a staging-file nonce needs no more.
    let nonce = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    Ok(parent.join(format!(
        ".{name}.{}-{}-{nonce:016x}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )))
}

/// Move `source` over `destination` in one step, so a reader sees either the
/// whole old file or the whole new one and never a torn one.
///
/// Public because three crates write files this way — this one for
/// transcripts, sidecars and journals, `rebon-session-host` for job records and
/// the foreground status file, the app for its queue — and the Windows half
/// of "atomic rename" has two rules that must not be re-derived three times:
/// the long-path prefix ([`wide_path_for_win32`]) and the answer to a
/// destination that someone else is holding open.
///
/// On Windows the superseding rename refuses with `ERROR_ACCESS_DENIED`
/// whenever the destination has an open handle — and the file being replaced
/// was itself published a moment ago, which is exactly when a virus scanner
/// or the search indexer has it open for a read. Rebon's own readers hold the
/// same kind of handle. Those handles are opened with `FILE_SHARE_DELETE`,
/// and a POSIX-semantics rename (Windows 10 1607 and later) succeeds past
/// them: the old file lingers unnamed until its last handle closes, as on
/// Unix. A handle without delete sharing still refuses, so the whole thing is
/// also retried for about a sixth of a second; what survives all of that is
/// reported as the original error.
#[cfg(windows)]
pub fn replace_file_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    replace_file_atomically_with(source, destination, std::thread::sleep)
}

/// Same, on a filesystem where `rename` has replaced since the beginning.
#[cfg(not(windows))]
pub fn replace_file_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_atomically_with(
    source: &Path,
    destination: &Path,
    wait: impl FnMut(std::time::Duration),
) -> std::io::Result<()> {
    let source_wide = wide_path_for_win32(source);
    let destination_wide = wide_path_for_win32(destination);
    retry_windows_atomic_replace(
        || replace_file_once(source, &source_wide, &destination_wide),
        wait,
    )
}

/// Attempts a transient refusal gets before it is final. The waits double
/// from 1 ms and cap at 32 ms, so this is about a sixth of a second.
#[cfg(windows)]
const WINDOWS_ATOMIC_REPLACE_ATTEMPTS: usize = 10;

#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

/// The three answers Windows gives for "someone else has this file right
/// now", none of which says anything about the next millisecond.
#[cfg(windows)]
fn retryable_windows_atomic_replace_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

#[cfg(windows)]
fn retry_windows_atomic_replace(
    mut replace: impl FnMut() -> std::io::Result<()>,
    mut wait: impl FnMut(std::time::Duration),
) -> std::io::Result<()> {
    let mut attempt = 0usize;
    loop {
        match replace() {
            Ok(()) => return Ok(()),
            Err(error) => {
                attempt += 1;
                if attempt == WINDOWS_ATOMIC_REPLACE_ATTEMPTS
                    || !retryable_windows_atomic_replace_error(&error)
                {
                    return Err(error);
                }
                wait(std::time::Duration::from_millis(
                    1_u64 << (attempt - 1).min(5),
                ));
            }
        }
    }
}

#[cfg(windows)]
fn replace_file_once(
    source: &Path,
    source_wide: &[u16],
    destination_wide: &[u16],
) -> std::io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_ACCESS_DENIED) {
        return Err(error);
    }
    // The refusal for an open destination. Rename past the handle; an older
    // Windows answers `ERROR_INVALID_PARAMETER` here and a handle without
    // delete sharing refuses again — either way the caller sees the first
    // refusal, which is the one the retry policy knows.
    rename_with_posix_semantics(source, destination_wide).map_err(|_| error)
}

/// `SetFileInformationByHandle(FileRenameInfoEx)` with
/// `FILE_RENAME_FLAG_POSIX_SEMANTICS` — what `std::fs::rename` does after the
/// same `ERROR_ACCESS_DENIED`. Hand-written because this crate has no Win32
/// binding crate and the struct is three fields and a name.
#[cfg(windows)]
fn rename_with_posix_semantics(source: &Path, destination_wide: &[u16]) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
    const FILE_RENAME_INFO_EX: u32 = 22;
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;
    #[link(name = "Kernel32")]
    extern "system" {
        fn SetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            class: u32,
            information: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    // `std` prefixes a long path on its own here. `FILE_FLAG_WRITE_THROUGH`
    // keeps the durability `MOVEFILE_WRITE_THROUGH` gave the first attempt.
    let file = OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_WRITE_THROUGH,
        )
        .open(source)?;

    // FILE_RENAME_INFO is `{ u32 Flags; HANDLE RootDirectory; u32
    // FileNameLength; u16 FileName[]; }`: the handle is pointer-aligned, the
    // length does not count the NUL, the name still carries one.
    let pointer = std::mem::size_of::<*const ()>();
    let length_offset = pointer * 2;
    let name_offset = length_offset + 4;
    let name_bytes = destination_wide.len().saturating_sub(1) * 2;
    let size = name_offset + name_bytes + 2;
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let bytes = buffer.as_mut_ptr().cast::<u8>();
    // SAFETY: every write stays inside `buffer`, whose length was computed
    // from the same offsets; the name copy is the whole NUL-terminated slice.
    // `RootDirectory` is the zero the buffer started with.
    unsafe {
        bytes
            .cast::<u32>()
            .write_unaligned(FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS);
        bytes
            .add(length_offset)
            .cast::<u32>()
            .write_unaligned(name_bytes as u32);
        std::ptr::copy_nonoverlapping(
            destination_wide.as_ptr().cast::<u8>(),
            bytes.add(name_offset),
            name_bytes + 2,
        );
    }
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FILE_RENAME_INFO_EX,
            bytes.cast_const().cast(),
            size as u32,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn publish_transcript_conditionally(
    temp_path: &Path,
    transcript_path: &Path,
    expected_sha256: [u8; 32],
) -> Result<(), RewindConversationError> {
    // Serialize cooperating appenders and publishers, then validate the named
    // destination immediately before an atomic, write-through replacement.
    // Never truncate the authoritative file in place: if the process stops at
    // any point, ordinary restart observes either the complete old transcript
    // or the complete durable target.
    let lock_path = transcript_path.with_extension("jsonl.publish.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;

    let current = std::fs::read(transcript_path)?;
    if <[u8; 32]>::from(Sha256::digest(&current)) != expected_sha256 {
        return Err(RewindConversationError::StaleRevision);
    }
    // Re-read at the commit boundary so a pathname replacement that raced the
    // first read is refused. The publisher lock binds all Rebon append/rewrite
    // paths; the second comparison matches the Unix conditional protocol.
    let commit_bytes = std::fs::read(transcript_path)?;
    if <[u8; 32]>::from(Sha256::digest(&commit_bytes)) != expected_sha256 {
        return Err(RewindConversationError::StaleRevision);
    }
    std::fs::remove_file(transcript_append_generation_path(transcript_path)).ok();
    replace_file_atomically(temp_path, transcript_path)?;
    std::fs::remove_file(transcript_append_generation_path(transcript_path)).ok();
    FileExt::unlock(&lock).ok();
    Ok(())
}

#[cfg(not(windows))]
fn publish_transcript_conditionally(
    temp_path: &Path,
    transcript_path: &Path,
    expected_sha256: [u8; 32],
) -> Result<(), RewindConversationError> {
    use std::os::unix::fs::MetadataExt;

    // Serialize every cooperating publisher independently of the live-session
    // lock. Keep the destination handle and verify that the pathname still names
    // that inode before publication, then repeat the content check at commit.
    let lock_path = transcript_path.with_extension("jsonl.publish.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;

    let mut opened = File::open(transcript_path)?;
    let opened_metadata = opened.metadata()?;
    let mut current = Vec::new();
    std::io::Read::read_to_end(&mut opened, &mut current)?;
    if <[u8; 32]>::from(Sha256::digest(&current)) != expected_sha256 {
        return Err(RewindConversationError::StaleRevision);
    }
    let named_metadata = std::fs::symlink_metadata(transcript_path)?;
    if opened_metadata.dev() != named_metadata.dev()
        || opened_metadata.ino() != named_metadata.ino()
    {
        return Err(RewindConversationError::StaleRevision);
    }
    let commit_bytes = std::fs::read(transcript_path)?;
    let commit_metadata = std::fs::symlink_metadata(transcript_path)?;
    if <[u8; 32]>::from(Sha256::digest(&commit_bytes)) != expected_sha256
        || opened_metadata.dev() != commit_metadata.dev()
        || opened_metadata.ino() != commit_metadata.ino()
    {
        return Err(RewindConversationError::StaleRevision);
    }
    // The publisher lock excludes cooperating appends. Remove proof before the
    // atomic replacement so a crash cannot leave a rewritten transcript with an
    // apparently extendable old generation.
    std::fs::remove_file(transcript_append_generation_path(transcript_path)).ok();
    replace_file_atomically(temp_path, transcript_path)?;
    std::fs::remove_file(transcript_append_generation_path(transcript_path)).ok();
    FileExt::unlock(&lock).ok();
    Ok(())
}

fn sync_parent_directory(path: &Path) {
    #[cfg(windows)]
    let _ = path;
    #[cfg(not(windows))]
    if let Some(parent) = path.parent() {
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

/// Rewind only the conversation transcript, under the same authoritative lock
/// used by live runtimes. Working-tree files and external effects are never
/// modified. The selected user entry and every later canonical entry are
/// excluded, and the selected prompt is returned for editor prefill.
pub fn rewind_conversation(
    projects_root: &Path,
    transcript_path: &Path,
    request: &RewindConversationRequest,
) -> Result<RewindConversationReceipt, RewindConversationError> {
    if request.mutation_id.trim().is_empty() {
        return Err(RewindConversationError::InvalidMutationId);
    }
    let authoritative_path = transcript_file_path(projects_root, &request.cwd, &request.session_id);
    if transcript_path != authoritative_path {
        return Err(RewindConversationError::TranscriptPathMismatch);
    }
    let Some(active_lock) =
        try_acquire_session_active_lock(projects_root, &request.cwd, &request.session_id)?
    else {
        return Err(RewindConversationError::Busy);
    };
    rewind_conversation_locked(projects_root, transcript_path, request, &active_lock)
}

/// Rewind using a matching lock already owned by the caller. This permits a
/// coordinator to hold one authoritative lock across a sequential (explicitly
/// non-atomic) conversation + code operation.
pub fn rewind_conversation_locked(
    projects_root: &Path,
    transcript_path: &Path,
    request: &RewindConversationRequest,
    active_lock: &SessionActiveLock,
) -> Result<RewindConversationReceipt, RewindConversationError> {
    if request.mutation_id.trim().is_empty() {
        return Err(RewindConversationError::InvalidMutationId);
    }
    let authoritative_path = transcript_file_path(projects_root, &request.cwd, &request.session_id);
    if transcript_path != authoritative_path {
        return Err(RewindConversationError::TranscriptPathMismatch);
    }
    if !active_lock.is_for(projects_root, &request.cwd, &request.session_id) {
        return Err(RewindConversationError::WrongSessionLock);
    }

    let fingerprint = request_fingerprint(request);
    let journal_path = rewind_artifact_path(transcript_path, &request.mutation_id, "journal.json");
    let backup_path = rewind_artifact_path(transcript_path, &request.mutation_id, "backup.jsonl");
    let temp_path = rewind_artifact_path(transcript_path, &request.mutation_id, "target.tmp");
    if journal_path.exists() {
        let journal: RewindJournal = serde_json::from_slice(&std::fs::read(&journal_path)?)
            .map_err(|error| RewindConversationError::RecoveryRequired(error.to_string()))?;
        if journal.mutation_id != request.mutation_id
            || journal.request_fingerprint != fingerprint
            || journal.backup_path != backup_path
            || journal.temp_path != temp_path
        {
            return Err(RewindConversationError::RecoveryRequired(
                "journal identity, request fingerprint, or artifact paths do not match".into(),
            ));
        }
        let current = load_session_history(transcript_path)?;
        if current.stamp.sha256 == journal.target_sha256 {
            let backup = std::fs::read(&backup_path).map_err(|error| {
                RewindConversationError::RecoveryRequired(format!(
                    "recovery backup is unavailable: {error}"
                ))
            })?;
            let backup_sha256: [u8; 32] = Sha256::digest(&backup).into();
            if backup.len() as u64 != journal.old_byte_len || backup_sha256 != journal.old_sha256 {
                return Err(RewindConversationError::RecoveryRequired(
                    "recovery backup does not match the verified old transcript".into(),
                ));
            }
            if !journal.committed {
                let mut finalized = journal.clone();
                finalized.committed = true;
                write_journal_atomically(&journal_path, &finalized)?;
            }
            return Ok(RewindConversationReceipt {
                mutation_id: journal.mutation_id,
                old_stamp: TranscriptStamp {
                    byte_len: journal.old_byte_len,
                    modified: None,
                    sha256: journal.old_sha256,
                },
                new_stamp: current.stamp.clone(),
                new_source_head_uuid: current.source_head_uuid.clone(),
                canonical_history: current,
                prefill_prompt: journal.selected_prompt,
                recovery_backup: backup_path,
            });
        }
        if current.stamp.sha256 == journal.old_sha256 && !journal.committed {
            // These paths have been independently re-derived and matched above;
            // never delete a path merely because persisted JSON names it.
            let _ = std::fs::remove_file(&temp_path);
            let _ = std::fs::remove_file(&backup_path);
            std::fs::remove_file(&journal_path)?;
        } else {
            return Err(RewindConversationError::RecoveryRequired(format!(
                "transcript hash matches neither the verified old nor target revision for mutation {}",
                journal.mutation_id
            )));
        }
    } else {
        // A crash while atomically publishing the first journal can leave only
        // its derived staging file. It is safe to reconcile because no journal
        // exists and its path is not read from persisted data.
        let _ = std::fs::remove_file(journal_path.with_extension("json.next"));
        let _ = std::fs::remove_file(&temp_path);
        let _ = std::fs::remove_file(&backup_path);
    }

    let old_bytes = std::fs::read(transcript_path)?;
    let metadata = std::fs::metadata(transcript_path)?;
    let old = history_from_bytes(transcript_path, &old_bytes, &metadata);
    if old.stamp.sha256 != request.expected_sha256
        || old.source_head_uuid != request.expected_source_head_uuid
    {
        return Err(RewindConversationError::StaleRevision);
    }
    let entries = parse_transcript_jsonl_strict(&old_bytes)?;
    let entry_count = entries.len();
    let canonical = reconstruct_chain(entries)
        .map(|loaded| loaded.messages)
        .ok_or_else(|| RewindConversationError::InvalidTranscript("no canonical chain".into()))?;
    if canonical.len() != entry_count {
        return Err(RewindConversationError::InvalidTranscript(
            "not every transcript record belongs to the single canonical chain".into(),
        ));
    }
    let selected_index = canonical
        .iter()
        .position(|entry| entry.uuid == request.selected_user_uuid)
        .ok_or(RewindConversationError::TurnMissing)?;
    let selected = &canonical[selected_index];
    if selected.entry_type != "user" {
        return Err(RewindConversationError::InvalidBoundary);
    }
    if selected.parent_uuid != request.boundary_parent_uuid {
        return Err(RewindConversationError::ParentChanged);
    }

    let mut target = Vec::new();
    for entry in &canonical[..selected_index] {
        serde_json::to_writer(&mut target, &entry.raw)
            .map_err(|error| RewindConversationError::Io(std::io::Error::other(error)))?;
        target.push(b'\n');
    }
    let target_sha256: [u8; 32] = Sha256::digest(&target).into();
    let target_head_uuid = canonical[..selected_index]
        .last()
        .map(|entry| entry.uuid.clone())
        .unwrap_or_default();
    if target_head_uuid != request.boundary_parent_uuid.clone().unwrap_or_default() {
        return Err(RewindConversationError::InvalidBoundary);
    }

    let journal = RewindJournal {
        mutation_id: request.mutation_id.clone(),
        request_fingerprint: fingerprint,
        selected_prompt: request.selected_prompt.clone(),
        old_sha256: old.stamp.sha256,
        old_byte_len: old.stamp.byte_len,
        target_sha256,
        target_head_uuid: target_head_uuid.clone(),
        backup_path: backup_path.clone(),
        temp_path: temp_path.clone(),
        committed: false,
    };
    // Publish recovery state before the first durable backup/target artifact.
    write_journal_atomically(&journal_path, &journal)?;
    write_durable(&backup_path, &old_bytes, true)?;
    write_durable(&temp_path, &target, true)?;

    // Conditionally publish against the same stable destination object. On
    // Windows this holds a deny-share handle across validation and durable
    // mutation, so a non-cooperating path replacement cannot enter the gap.
    publish_transcript_conditionally(&temp_path, transcript_path, old.stamp.sha256)?;
    sync_parent_directory(transcript_path);
    let new_history = load_session_history(transcript_path)?;
    if new_history.stamp.sha256 != target_sha256 || new_history.source_head_uuid != target_head_uuid
    {
        return Err(RewindConversationError::RecoveryRequired(
            "atomic replacement completed but verification failed".into(),
        ));
    }
    let mut committed = journal;
    committed.committed = true;
    write_journal_atomically(&journal_path, &committed)?;

    Ok(RewindConversationReceipt {
        mutation_id: request.mutation_id.clone(),
        old_stamp: old.stamp,
        new_stamp: new_history.stamp.clone(),
        new_source_head_uuid: new_history.source_head_uuid.clone(),
        canonical_history: new_history,
        prefill_prompt: request.selected_prompt.clone(),
        recovery_backup: backup_path,
    })
}

const DURABLE_SUMMARY_MARKER: &str = "_rebonDurableSummary";

fn summary_dropped_count(
    entries: &[TranscriptEntry],
    selected_index: usize,
    mode: SummarizeConversationMode,
) -> usize {
    match mode {
        SummarizeConversationMode::FromSelected => entries.len().saturating_sub(selected_index + 1),
        SummarizeConversationMode::UpToSelected => selected_index,
    }
}

fn committed_summary_entries(
    bytes: &[u8],
) -> Result<Vec<TranscriptEntry>, RewindConversationError> {
    let entries = parse_transcript_jsonl_strict(bytes)?;
    if entries.is_empty() {
        return Err(RewindConversationError::InvalidTranscript(
            "no canonical chain".into(),
        ));
    }

    // A persisted transcript may have one projection-only system/attachment
    // suffix. Validate the complete record set as one unbranched chain before
    // omitting that supported suffix from the model-visible canonical rows.
    let mut index_by_uuid = std::collections::HashMap::with_capacity(entries.len());
    let mut child_by_parent = std::collections::HashMap::with_capacity(entries.len());
    let mut root = None;
    for (index, entry) in entries.iter().enumerate() {
        if index_by_uuid.insert(entry.uuid.as_str(), index).is_some() {
            return Err(RewindConversationError::InvalidTranscript(
                "duplicate transcript UUID".into(),
            ));
        }
        match entry.parent_uuid.as_deref() {
            Some(parent) => {
                if child_by_parent.insert(parent, index).is_some() {
                    return Err(RewindConversationError::InvalidTranscript(
                        "transcript contains a branch".into(),
                    ));
                }
            }
            None if root.replace(index).is_some() => {
                return Err(RewindConversationError::InvalidTranscript(
                    "transcript contains multiple roots".into(),
                ));
            }
            None => {}
        }
    }
    for entry in &entries {
        if entry
            .parent_uuid
            .as_deref()
            .is_some_and(|parent| !index_by_uuid.contains_key(parent))
        {
            return Err(RewindConversationError::InvalidTranscript(
                "transcript parent is missing".into(),
            ));
        }
    }
    let mut ordered = Vec::with_capacity(entries.len());
    let mut cursor = root;
    while let Some(index) = cursor {
        ordered.push(index);
        cursor = child_by_parent.get(entries[index].uuid.as_str()).copied();
        if ordered.len() > entries.len() {
            break;
        }
    }
    if ordered.len() != entries.len() {
        return Err(RewindConversationError::InvalidTranscript(
            "transcript is cyclic or disconnected".into(),
        ));
    }

    let canonical_indices = reconstruct_chain_indices(&entries);
    if canonical_indices.is_empty() || !ordered.starts_with(&canonical_indices) {
        return Err(RewindConversationError::InvalidTranscript(
            "no canonical chain".into(),
        ));
    }
    if ordered[canonical_indices.len()..].iter().any(|&index| {
        entries[index].entry_type != "system" && entries[index].entry_type != "attachment"
    }) {
        return Err(RewindConversationError::InvalidTranscript(
            "non-canonical transcript records are not a supported suffix".into(),
        ));
    }
    Ok(canonical_indices
        .into_iter()
        .map(|index| entries[index].clone())
        .collect())
}

/// Durably rewrite a transcript for `/rewind` summarization while the caller
/// owns the active-session lock. The disk CAS is authoritative; the returned
/// entries are the exact raw records callers must use for engine/UI projection.
pub fn summarize_conversation_locked(
    projects_root: &Path,
    transcript_path: &Path,
    request: &SummarizeConversationRequest,
    active_lock: &SessionActiveLock,
) -> Result<SummarizeConversationReceipt, RewindConversationError> {
    if request.mutation_id.trim().is_empty() {
        return Err(RewindConversationError::InvalidMutationId);
    }
    let authoritative_path = transcript_file_path(projects_root, &request.cwd, &request.session_id);
    if transcript_path != authoritative_path {
        return Err(RewindConversationError::TranscriptPathMismatch);
    }
    if !active_lock.is_for(projects_root, &request.cwd, &request.session_id) {
        return Err(RewindConversationError::WrongSessionLock);
    }

    let fingerprint = summarize_request_fingerprint(request);
    let journal_path = rewind_artifact_path(transcript_path, &request.mutation_id, "journal.json");
    let backup_path = rewind_artifact_path(transcript_path, &request.mutation_id, "backup.jsonl");
    let temp_path = rewind_artifact_path(transcript_path, &request.mutation_id, "target.tmp");
    if journal_path.exists() {
        let journal: RewindJournal = serde_json::from_slice(&std::fs::read(&journal_path)?)
            .map_err(|error| RewindConversationError::RecoveryRequired(error.to_string()))?;
        if journal.mutation_id != request.mutation_id
            || journal.request_fingerprint != fingerprint
            || journal.backup_path != backup_path
            || journal.temp_path != temp_path
        {
            return Err(RewindConversationError::RecoveryRequired(
                "journal identity, request fingerprint, or artifact paths do not match".into(),
            ));
        }
        let current_bytes = std::fs::read(transcript_path)?;
        let current_metadata = std::fs::metadata(transcript_path)?;
        let current = history_from_bytes(transcript_path, &current_bytes, &current_metadata);
        if current.stamp.sha256 == journal.target_sha256 {
            let recovered = (|| -> Result<_, RewindConversationError> {
                let backup = std::fs::read(&backup_path).map_err(|error| {
                    RewindConversationError::RecoveryRequired(format!(
                        "recovery backup is unavailable: {error}"
                    ))
                })?;
                let backup_sha256: [u8; 32] = Sha256::digest(&backup).into();
                if backup.len() as u64 != journal.old_byte_len
                    || backup_sha256 != journal.old_sha256
                {
                    return Err(RewindConversationError::RecoveryRequired(
                        "recovery backup does not match the verified old transcript".into(),
                    ));
                }
                let old_entries = committed_summary_entries(&backup)?;
                let selected_index = old_entries
                    .iter()
                    .position(|entry| entry.uuid == request.selected_user_uuid)
                    .ok_or(RewindConversationError::TurnMissing)?;
                let entries = committed_summary_entries(&current_bytes)?;
                if !journal.committed {
                    let mut finalized = journal.clone();
                    finalized.committed = true;
                    write_journal_atomically(&journal_path, &finalized)?;
                }
                Ok(SummarizeConversationReceipt {
                    mutation_id: journal.mutation_id.clone(),
                    old_stamp: TranscriptStamp {
                        byte_len: journal.old_byte_len,
                        modified: None,
                        sha256: journal.old_sha256,
                    },
                    new_stamp: current.stamp.clone(),
                    new_source_head_uuid: current.source_head_uuid.clone(),
                    recovery_backup: backup_path.clone(),
                    entries,
                    dropped_count: summary_dropped_count(
                        &old_entries,
                        selected_index,
                        request.mode,
                    ),
                })
            })()
            .map_err(|error| {
                RewindConversationError::CommittedRecoveryRequired(error.to_string())
            })?;
            return Ok(recovered);
        }
        if current.stamp.sha256 == journal.old_sha256 && !journal.committed {
            let _ = std::fs::remove_file(&temp_path);
            let _ = std::fs::remove_file(&backup_path);
            std::fs::remove_file(&journal_path)?;
        } else {
            return Err(RewindConversationError::RecoveryRequired(format!(
                "transcript hash matches neither the verified old nor target revision for mutation {}",
                journal.mutation_id
            )));
        }
    } else {
        let _ = std::fs::remove_file(journal_path.with_extension("json.next"));
        let _ = std::fs::remove_file(&temp_path);
        let _ = std::fs::remove_file(&backup_path);
    }

    let old_bytes = std::fs::read(transcript_path)?;
    let metadata = std::fs::metadata(transcript_path)?;
    let old = history_from_bytes(transcript_path, &old_bytes, &metadata);
    if old.stamp.sha256 != request.expected_sha256
        || old.source_head_uuid != request.expected_source_head_uuid
    {
        return Err(RewindConversationError::StaleRevision);
    }
    let canonical = committed_summary_entries(&old_bytes)?;
    let selected_index = canonical
        .iter()
        .position(|entry| entry.uuid == request.selected_user_uuid)
        .ok_or(RewindConversationError::TurnMissing)?;
    let selected = &canonical[selected_index];
    if selected.entry_type != "user" {
        return Err(RewindConversationError::InvalidBoundary);
    }
    if selected.parent_uuid != request.selected_parent_uuid {
        return Err(RewindConversationError::ParentChanged);
    }

    let mut retained = match request.mode {
        SummarizeConversationMode::FromSelected => canonical[..=selected_index].to_vec(),
        SummarizeConversationMode::UpToSelected => canonical[selected_index..].to_vec(),
    };
    if request.mode == SummarizeConversationMode::UpToSelected {
        retained[0].parent_uuid = None;
        retained[0]
            .raw
            .as_object_mut()
            .ok_or_else(|| {
                RewindConversationError::InvalidTranscript("selected row is not an object".into())
            })?
            .insert("parentUuid".into(), Value::Null);
    }
    let note_object = request.note_raw.as_object().ok_or_else(|| {
        RewindConversationError::InvalidTranscript("summary note is not an object".into())
    })?;
    if note_object.get("type").and_then(Value::as_str) != Some("system") {
        return Err(RewindConversationError::InvalidTranscript(
            "summary note must be a system record".into(),
        ));
    }
    let note_uuid = note_object
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .ok_or_else(|| {
            RewindConversationError::InvalidTranscript("summary note has no UUID".into())
        })?
        .to_string();
    if canonical.iter().any(|entry| entry.uuid == note_uuid) {
        return Err(RewindConversationError::InvalidTranscript(
            "summary note UUID already exists".into(),
        ));
    }
    let note_parent = retained.last().map(|entry| entry.uuid.clone());
    let mut note_raw = request.note_raw.clone();
    let note_raw_object = note_raw.as_object_mut().expect("validated object");
    note_raw_object.insert(
        "parentUuid".into(),
        note_parent
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    note_raw_object.insert(DURABLE_SUMMARY_MARKER.into(), Value::Bool(true));
    let note_timestamp = note_raw_object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    retained.push(TranscriptEntry {
        entry_type: "system".into(),
        uuid: note_uuid.clone(),
        parent_uuid: note_parent,
        timestamp: note_timestamp,
        raw: note_raw,
    });

    let mut target = Vec::new();
    for entry in &retained {
        serde_json::to_writer(&mut target, &entry.raw)
            .map_err(|error| RewindConversationError::Io(std::io::Error::other(error)))?;
        target.push(b'\n');
    }
    let target_sha256: [u8; 32] = Sha256::digest(&target).into();
    let journal = RewindJournal {
        mutation_id: request.mutation_id.clone(),
        request_fingerprint: fingerprint,
        selected_prompt: String::new(),
        old_sha256: old.stamp.sha256,
        old_byte_len: old.stamp.byte_len,
        target_sha256,
        target_head_uuid: note_uuid.clone(),
        backup_path: backup_path.clone(),
        temp_path: temp_path.clone(),
        committed: false,
    };
    write_journal_atomically(&journal_path, &journal)?;
    write_durable(&backup_path, &old_bytes, true)?;
    write_durable(&temp_path, &target, true)?;
    if let Err(error) =
        publish_transcript_conditionally(&temp_path, transcript_path, old.stamp.sha256)
    {
        // Publication implementations can report cleanup/fsync failures after
        // changing the destination. Never present those as a safe pre-commit
        // refusal: compare the authoritative bytes with both known revisions.
        // Anything other than the verified old revision requires restart/recovery.
        match std::fs::read(transcript_path) {
            Ok(current) if <[u8; 32]>::from(Sha256::digest(&current)) == old.stamp.sha256 => {
                return Err(error);
            }
            Ok(current) => {
                let current_sha256: [u8; 32] = Sha256::digest(&current).into();
                let state = if current_sha256 == target_sha256 {
                    "target transcript is durable but publication finalization failed"
                } else {
                    "publication left a transcript matching neither the old nor target revision"
                };
                return Err(RewindConversationError::CommittedRecoveryRequired(format!(
                    "{state}: {error}"
                )));
            }
            Err(read_error) => {
                return Err(RewindConversationError::CommittedRecoveryRequired(format!(
                    "publication failed and the transcript revision could not be verified ({read_error}): {error}"
                )));
            }
        }
    }
    sync_parent_directory(transcript_path);

    let finalized = (|| -> Result<_, RewindConversationError> {
        let new_bytes = std::fs::read(transcript_path)?;
        let new_metadata = std::fs::metadata(transcript_path)?;
        let new_history = history_from_bytes(transcript_path, &new_bytes, &new_metadata);
        let entries = committed_summary_entries(&new_bytes)?;
        if new_history.stamp.sha256 != target_sha256
            || new_history.source_head_uuid != note_uuid
            || entries.len() != retained.len()
        {
            return Err(RewindConversationError::RecoveryRequired(
                "atomic replacement completed but verification failed".into(),
            ));
        }
        let mut committed = journal;
        committed.committed = true;
        write_journal_atomically(&journal_path, &committed)?;
        Ok((new_history, entries))
    })()
    .map_err(|error| RewindConversationError::CommittedRecoveryRequired(error.to_string()))?;
    let (new_history, entries) = finalized;
    Ok(SummarizeConversationReceipt {
        mutation_id: request.mutation_id.clone(),
        old_stamp: old.stamp,
        new_stamp: new_history.stamp,
        new_source_head_uuid: new_history.source_head_uuid,
        recovery_backup: backup_path,
        entries,
        dropped_count: summary_dropped_count(&canonical, selected_index, request.mode),
    })
}

/// Parse a full JSONL transcript byte buffer into `TranscriptEntry`s.
///
/// Malformed lines are dropped silently — best-effort, skip garbage.
/// Empty lines are also dropped.
/// Entries without a `uuid` or `type` field are rejected (they can't
/// participate in the chain walk).
pub fn parse_transcript_jsonl(bytes: &[u8]) -> Vec<TranscriptEntry> {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let entry_type = match object.get("type") {
            Some(Value::String(value)) if !value.is_empty() => value.clone(),
            _ => continue,
        };
        let uuid = match object.get("uuid") {
            Some(Value::String(value)) if !value.is_empty() => value.clone(),
            _ => continue,
        };
        let parent_uuid = match object.get("parentUuid") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => (!value.is_empty()).then(|| value.clone()),
            _ => continue,
        };
        let timestamp = match object.get("timestamp") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(value.clone()),
            _ => continue,
        };
        out.push(TranscriptEntry {
            entry_type,
            uuid,
            parent_uuid,
            timestamp,
            raw: value,
        });
    }
    out
}

fn parse_transcript_jsonl_strict(
    bytes: &[u8],
) -> Result<Vec<TranscriptEntry>, RewindConversationError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        RewindConversationError::InvalidTranscript(format!("invalid UTF-8: {error}"))
    })?;
    let nonblank_lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    let entries = parse_transcript_jsonl(bytes);
    if entries.len() != nonblank_lines {
        return Err(RewindConversationError::InvalidTranscript(
            "one or more nonblank JSONL records are malformed or unsupported".into(),
        ));
    }
    let mut uuids = std::collections::HashSet::new();
    if entries.iter().any(|entry| !uuids.insert(&entry.uuid)) {
        return Err(RewindConversationError::InvalidTranscript(
            "duplicate transcript UUIDs are ambiguous".into(),
        ));
    }
    Ok(entries)
}

/// Load every best-effort parsed transcript row from disk without selecting a
/// canonical chain. This narrow surface is for recovery overlays that must add
/// process-local rows before canonical reconstruction. File order and duplicate
/// UUIDs are preserved so the chain walker retains its last-write-wins behavior.
/// Source facts come from the same read, avoiding a racy metadata/read retry.
pub fn load_raw_transcript_from_file(path: &Path) -> std::io::Result<Option<RawTranscriptFile>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let byte_len = bytes.len();
    let text = std::str::from_utf8(&bytes).ok();
    let nonblank_row_count = text
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);
    let entries = parse_transcript_jsonl(&bytes);
    let parsed_row_count = entries.len();
    let parse_complete = text.is_some() && parsed_row_count == nonblank_row_count;
    Ok(Some(RawTranscriptFile {
        entries,
        byte_len,
        parsed_row_count,
        nonblank_row_count,
        parse_complete,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptPromptUuidState {
    Missing,
    MatchingIncomplete,
    MatchingComplete,
    Conflict,
}

pub fn classify_transcript_prompt_uuid(
    entries: &[TranscriptEntry],
    uuid: &str,
    expected_content: &Value,
) -> TranscriptPromptUuidState {
    let matching_indices = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| (entry.uuid == uuid).then_some(index))
        .collect::<Vec<_>>();
    if matching_indices.is_empty() {
        return TranscriptPromptUuidState::Missing;
    }
    if matching_indices.iter().any(|&index| {
        let entry = &entries[index];
        entry.entry_type != "user"
            || entry.raw.pointer("/message/content") != Some(expected_content)
    }) {
        return TranscriptPromptUuidState::Conflict;
    }

    let canonical = reconstruct_chain_indices(entries);
    let matching_positions = canonical
        .iter()
        .enumerate()
        .filter_map(|(position, &index)| (entries[index].uuid == uuid).then_some(position))
        .collect::<Vec<_>>();
    if matching_positions.is_empty() {
        return TranscriptPromptUuidState::MatchingIncomplete;
    }
    if matching_positions.into_iter().any(|position| {
        canonical[position + 1..]
            .iter()
            .any(|&index| transcript_assistant_is_terminal(&entries[index]))
    }) {
        TranscriptPromptUuidState::MatchingComplete
    } else {
        TranscriptPromptUuidState::MatchingIncomplete
    }
}

fn transcript_assistant_is_terminal(entry: &TranscriptEntry) -> bool {
    if entry.entry_type != "assistant" {
        return false;
    }
    if entry
        .raw
        .pointer("/message/stop_reason")
        .or_else(|| entry.raw.get("stop_reason"))
        .and_then(Value::as_str)
        == Some("tool_use")
    {
        return false;
    }
    !entry
        .raw
        .pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        })
}

/// Load and reconstruct a transcript chain from disk.
///
/// Steps:
///
/// 1. Read the JSONL file at `path`. ENOENT returns `Ok(None)`; any other
///    I/O error propagates as `Err`.
/// 2. Parse entries best-effort (malformed lines skipped).
/// 3. If no entries are left, return `Ok(None)`.
/// 4. Compute the set of terminal entries (entries whose `uuid` is not
///    referenced by any other entry's `parentUuid`).
/// 5. For each terminal, walk `parentUuid` backwards until the first
///    entry whose `type` is `user` or `assistant`; the ancestor found
///    this way is a leaf candidate. This walks past
///    trailing `system`/`attachment` entries to land on the nearest
///    user/assistant ancestor instead of dropping the chain entirely.
/// 6. Among leaf candidates, pick the one with the
///    lexicographically-greatest `timestamp` (ISO-8601 is sort-compatible
///    with chronological order).
/// 7. Walk `parentUuid` from that leaf back to the root, bailing on
///    cycles. Reverse the collected list so the result is oldest-first.
pub fn load_transcript_from_file(path: &Path) -> std::io::Result<Option<LoadedTranscript>> {
    Ok(load_raw_transcript_from_file(path)?.and_then(|raw| reconstruct_chain(raw.entries)))
}

/// Reconstruct the canonical chain as indices into `entries` without cloning the
/// retained records. This mirrors [`reconstruct_chain`] exactly and is intended
/// for caches that already own the parsed transcript.
pub fn reconstruct_chain_indices(entries: &[TranscriptEntry]) -> Vec<usize> {
    if entries.is_empty() {
        return Vec::new();
    }

    let mut index_by_uuid: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        index_by_uuid.insert(entry.uuid.as_str(), index);
    }
    let referenced_parents: std::collections::HashSet<&str> = index_by_uuid
        .values()
        .filter_map(|&index| entries[index].parent_uuid.as_deref())
        .collect();

    let mut leaf_uuids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    // Generic trailing system/attachment rows remain projection-only. A
    // storage-authored summary note is the sole terminal suffix that is part of
    // canonical model history and therefore survives restart.
    let mut summary_terminal_by_leaf: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    let mut considered: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(entries.len());
    for start_entry in entries {
        if !considered.insert(start_entry.uuid.as_str())
            || referenced_parents.contains(start_entry.uuid.as_str())
        {
            continue;
        }
        let Some(&canonical_index) = index_by_uuid.get(start_entry.uuid.as_str()) else {
            continue;
        };
        let mut walk_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut nearest_summary = None;
        let mut cursor = Some(canonical_index);
        while let Some(index) = cursor {
            let entry = &entries[index];
            if !walk_seen.insert(entry.uuid.as_str()) {
                break;
            }
            if nearest_summary.is_none()
                && entry.entry_type == "system"
                && entry
                    .raw
                    .get(DURABLE_SUMMARY_MARKER)
                    .and_then(Value::as_bool)
                    == Some(true)
            {
                nearest_summary = Some(index);
            }
            if entry.entry_type == "user" || entry.entry_type == "assistant" {
                leaf_uuids.insert(entry.uuid.as_str());
                if let Some(summary_index) = nearest_summary {
                    summary_terminal_by_leaf.insert(entry.uuid.as_str(), summary_index);
                }
                break;
            }
            cursor = entry
                .parent_uuid
                .as_deref()
                .and_then(|parent| index_by_uuid.get(parent).copied());
        }
    }

    let mut best: Option<usize> = None;
    let mut picked_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in entries {
        if !leaf_uuids.contains(entry.uuid.as_str()) || !picked_seen.insert(entry.uuid.as_str()) {
            continue;
        }
        let Some(&canonical_index) = index_by_uuid.get(entry.uuid.as_str()) else {
            continue;
        };
        match best {
            None => best = Some(canonical_index),
            Some(current_index) => {
                let timestamp = entries[canonical_index].timestamp.as_deref().unwrap_or("");
                let current = entries[current_index].timestamp.as_deref().unwrap_or("");
                if timestamp > current {
                    best = Some(canonical_index);
                }
            }
        }
    }

    let Some(mut leaf) = best else {
        return Vec::new();
    };
    if let Some(&summary_terminal) = summary_terminal_by_leaf.get(entries[leaf].uuid.as_str()) {
        leaf = summary_terminal;
    }
    let mut chain = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut cursor = Some(leaf);
    let mut cycle_detected = false;
    while let Some(index) = cursor {
        let entry = &entries[index];
        if !seen.insert(entry.uuid.as_str()) {
            cycle_detected = true;
            break;
        }
        chain.push(index);
        cursor = entry
            .parent_uuid
            .as_deref()
            .and_then(|parent| index_by_uuid.get(parent).copied());
    }
    if cycle_detected {
        let mut recovered = Vec::new();
        let mut recovered_seen = std::collections::HashSet::new();
        let mut recovered_cursor = Some(leaf);
        while let Some(index) = recovered_cursor {
            if !recovered_seen.insert(index) {
                break;
            }
            recovered.push(index);
            recovered_cursor = entries[index].parent_uuid.as_deref().and_then(|parent| {
                entries[..index]
                    .iter()
                    .rposition(|candidate| candidate.uuid == parent)
            });
        }
        if recovered.len() > chain.len() {
            chain = recovered;
        }
    }
    chain.reverse();
    chain
}

/// Pure-function core of [`load_transcript_from_file`] — exposed so tests
/// can exercise the leaf/chain walker without touching the filesystem.
pub fn reconstruct_chain(entries: Vec<TranscriptEntry>) -> Option<LoadedTranscript> {
    if entries.is_empty() {
        return None;
    }
    let indices = reconstruct_chain_indices(&entries);
    if indices.is_empty() {
        return None;
    }
    let created_at = indices
        .iter()
        .filter_map(|&index| {
            entries[index]
                .timestamp
                .as_deref()
                .and_then(parse_rfc3339_to_system_time)
        })
        .next()
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut entries: Vec<Option<TranscriptEntry>> = entries.into_iter().map(Some).collect();
    let messages = indices
        .into_iter()
        .filter_map(|index| entries[index].take())
        .collect();

    Some(LoadedTranscript {
        messages,
        created_at,
        title: None,
    })
}

// ─── Write path ────────────────────────────────────────────────────────────
//
// Implementation for the write path and the creation path. Both real
// callers need this: `session/prompt` appends user and assistant messages
// as the turn executes, and tests want to read the result back via the
// existing `load_transcript_from_file` path.
//
// Format invariants:
//
// - One line per entry, UTF-8 JSON without a trailing comma.
// - Each entry ships at minimum `type`, `uuid`, `timestamp`, and an
//   optional `parentUuid`. The loader tolerates missing fields beyond
//   those, so callers are free to add richer payloads.
// - Multiple entries per call land as sequential lines in the order
//   given. The write is not atomic across entries — a crash mid-append
//   may leave a partial line at the tail. `parse_transcript_jsonl`
//   already skips malformed lines.

/// A single transcript entry to persist via [`append_transcript_entry`]
/// or [`write_transcript_entries`].
///
/// Callers can either pass a minimal struct filled out via
/// [`TranscriptWriteEntry::new`] (in which case the write helpers mint
/// a uuid + timestamp themselves) or pass a fully-populated struct —
/// useful when reproducing an existing entry in tests.
#[derive(Debug, Clone, Default)]
pub struct TranscriptWriteEntry {
    /// `type` field — usually `"user"`, `"assistant"`, `"system"`, or
    /// `"attachment"`.
    pub entry_type: String,
    /// Optional pre-assigned uuid. `None` lets the writer mint an id by
    /// hashing the content together with the current nanosecond timestamp.
    pub uuid: Option<String>,
    /// Optional parent uuid linking this entry into the chain.
    pub parent_uuid: Option<String>,
    /// Optional ISO-8601 timestamp. `None` lets the writer stamp
    /// `SystemTime::now()`.
    pub timestamp: Option<String>,
    /// Free-form JSON payload. The write helpers merge the baseline
    /// fields (`type`, `uuid`, `timestamp`, `parentUuid`) into this
    /// object before serializing.
    pub payload: Value,
}

impl TranscriptWriteEntry {
    /// Minimal constructor that stamps only `entry_type` and
    /// `payload`. The writer fills in the rest.
    pub fn new(entry_type: impl Into<String>, payload: Value) -> Self {
        Self {
            entry_type: entry_type.into(),
            uuid: None,
            parent_uuid: None,
            timestamp: None,
            payload,
        }
    }

    /// Attach a parent uuid and return `self`.
    pub fn with_parent(mut self, parent_uuid: impl Into<String>) -> Self {
        self.parent_uuid = Some(parent_uuid.into());
        self
    }

    /// Attach an explicit uuid and return `self`.
    pub fn with_uuid(mut self, uuid: impl Into<String>) -> Self {
        self.uuid = Some(uuid.into());
        self
    }

    /// Attach an explicit timestamp and return `self`.
    pub fn with_timestamp(mut self, timestamp: impl Into<String>) -> Self {
        self.timestamp = Some(timestamp.into());
        self
    }
}

/// Ensure the project directory for `(projects_root, cwd)` exists and
/// return the path to the transcript file for `session_id`.
///
/// Creates the intermediate `projects_root/sanitize(cwd_identity(cwd))`
/// directory if needed. Does **not** create the file itself — callers that only
/// want the path without touching the filesystem should use
/// [`transcript_file_path`] + [`project_dir_path`] directly.
pub fn ensure_session_file_path(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> std::io::Result<PathBuf> {
    fold_legacy_project_dir(projects_root, cwd);
    let dir = project_dir_path(projects_root, cwd);
    std::fs::create_dir_all(&dir)?;
    write_project_cwd_sidecar(&dir, cwd);
    Ok(transcript_file_path(projects_root, cwd, session_id))
}

/// Create a session on disk: its project directory, an empty transcript
/// file, and the date it was created. Returns the transcript path.
///
/// A transcript is written on the first append, so a conversation nobody has
/// spoken in yet exists only in memory — and another process resumes *from
/// disk*. The empty file is what makes "this session, from the beginning" a
/// thing that process can open; the recorded date is what lets a list order
/// it, because the id carries no date and an empty file has no row to read
/// one from. The two belong in one call: a session created without the
/// second is one that shows up undated.
///
/// A session whose transcript already exists is not new. Its file is left
/// alone and so is its recorded date, which must not move to today because
/// something asked for it again.
pub fn create_session_on_disk(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> std::io::Result<PathBuf> {
    let path = ensure_session_file_path(projects_root, cwd, session_id)?;
    if path.exists() {
        return Ok(path);
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    record_session_created_at(projects_root, cwd, session_id)?;
    Ok(path)
}

/// Filename of the per-project sidecar that records the real `cwd` next to its
/// transcripts. `sanitize_path` is lossy (every non-alphanumeric char becomes
/// `-`) and irreversible, and transcript entries don't carry the cwd, so this
/// is the only way a *reader* (e.g. the desktop app's project library) can map a
/// `projects/<sanitize(cwd)>/` directory back to its original path.
pub const PROJECT_CWD_SIDECAR: &str = ".cwd";

/// Write the cwd sidecar once per project directory (best-effort; ignored on
/// error — it is purely an aid for readers and never affects correctness).
fn write_project_cwd_sidecar(project_dir: &Path, cwd: &str) {
    let sidecar = project_dir.join(PROJECT_CWD_SIDECAR);
    if !sidecar.exists() {
        let _ = std::fs::write(&sidecar, cwd);
    }
}

/// Read the real cwd a `projects/<sanitize(cwd)>/` directory was created for,
/// from its [`PROJECT_CWD_SIDECAR`] file. `None` when the sidecar is absent
/// (older transcripts written before the sidecar existed).
pub fn read_project_cwd_sidecar(project_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(project_dir.join(PROJECT_CWD_SIDECAR)).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Return every cwd sidecar whose project directory contains `session_id`.
pub fn session_transcript_cwds(projects_root: &Path, session_id: &str) -> Vec<String> {
    let transcript_name = format!("{session_id}.jsonl");
    let Ok(entries) = std::fs::read_dir(projects_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let project_dir = entry.path();
            project_dir
                .join(&transcript_name)
                .is_file()
                .then(|| read_project_cwd_sidecar(&project_dir))
                .flatten()
        })
        .collect()
}

/// Find the cwd whose project directory uniquely contains `session_id`.
///
/// This recovers transcripts whose original cwd no longer exists, such as a
/// completed background session whose temporary worktree has been removed.
/// Multiple matches are rejected rather than selecting one by filesystem order.
pub fn find_session_transcript_cwd(projects_root: &Path, session_id: &str) -> Option<String> {
    let mut matches = session_transcript_cwds(projects_root, session_id).into_iter();
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

/// Move a session transcript and its optional metadata into another project.
///
/// Continuation has a single canonical storage location, so relocating a session
/// removes the old transcript after the new copy is complete.
pub fn move_session_to_cwd(
    projects_root: &Path,
    source_cwd: &str,
    target_cwd: &str,
    session_id: &str,
) -> std::io::Result<PathBuf> {
    let source = transcript_file_path(projects_root, source_cwd, session_id);
    let target = ensure_session_file_path(projects_root, target_cwd, session_id)?;
    let source_meta = session_meta_path(projects_root, source_cwd, session_id);
    let target_meta = session_meta_path(projects_root, target_cwd, session_id);
    let has_meta = source_meta.is_file();

    // Relocation changes which pathname names the file. Invalidate both ends
    // before and after the rename so neither a stale target proof nor the
    // source's now-orphaned proof can authenticate a later append.
    invalidate_transcript_append_proof(&source);
    invalidate_transcript_append_proof(&target);
    if has_meta {
        std::fs::copy(&source_meta, &target_meta)?;
    }
    if let Err(move_err) = std::fs::rename(&source, &target) {
        if has_meta {
            let _ = std::fs::remove_file(&target_meta);
        }
        return Err(move_err);
    }
    if has_meta {
        let _ = std::fs::remove_file(&source_meta);
    }
    invalidate_transcript_append_proof(&source);
    invalidate_transcript_append_proof(&target);
    Ok(target)
}

/// Copy a session transcript and its optional metadata into another project.
/// The source is retained so an explicit cross-project copy is reversible.
pub fn copy_session_to_cwd(
    projects_root: &Path,
    source_cwd: &str,
    target_cwd: &str,
    session_id: &str,
) -> std::io::Result<PathBuf> {
    let source = transcript_file_path(projects_root, source_cwd, session_id);
    let target = ensure_session_file_path(projects_root, target_cwd, session_id)?;
    // The target may already have proof for a different file. It must remain
    // untrusted throughout replacement and start a fresh epoch on its next append.
    invalidate_transcript_append_proof(&target);
    std::fs::copy(source, &target)?;
    invalidate_transcript_append_proof(&target);

    let source_meta = session_meta_path(projects_root, source_cwd, session_id);
    if source_meta.is_file() {
        std::fs::copy(
            source_meta,
            session_meta_path(projects_root, target_cwd, session_id),
        )?;
    }
    Ok(target)
}

/// Append a single entry to the transcript file for
/// `(cwd, session_id)` under `projects_root`. Creates parent
/// directories on first use.
///
/// Returns the finalized [`TranscriptEntry`] so the caller can feed
/// it back into an in-memory chain without re-reading from disk.
///
/// Takes a single entry at a time
/// so the caller can interleave appends with stream updates. Call
/// [`write_transcript_entries`] for bulk writes.
pub fn append_transcript_entry(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    entry: TranscriptWriteEntry,
) -> std::io::Result<TranscriptEntry> {
    let path = ensure_session_file_path(projects_root, cwd, session_id)?;
    let line = serialize_transcript_entry(&entry);
    append_jsonl_line(&path, &line.serialized)?;
    Ok(line.parsed)
}

/// Bulk variant of [`append_transcript_entry`]. Writes every entry in
/// a single `OpenOptions::append` call (though each entry is still a
/// separate line), and returns the finalized [`TranscriptEntry`]
/// list.
pub fn write_transcript_entries(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    entries: Vec<TranscriptWriteEntry>,
) -> std::io::Result<Vec<TranscriptEntry>> {
    let path = ensure_session_file_path(projects_root, cwd, session_id)?;
    let mut buf = String::new();
    let mut parsed = Vec::with_capacity(entries.len());
    for entry in entries {
        let finalised = serialize_transcript_entry(&entry);
        buf.push_str(&finalised.serialized);
        parsed.push(finalised.parsed);
    }
    append_jsonl_lines(&path, &buf)?;
    Ok(parsed)
}

struct FinalisedEntry {
    serialized: String,
    parsed: TranscriptEntry,
}

/// Convert a [`TranscriptWriteEntry`] into a finalized
/// [`TranscriptEntry`] **without** writing to disk. Used by the
/// engine to push entries into the in-memory session record even
/// when disk persistence fails.
pub fn finalize_transcript_entry(entry: &TranscriptWriteEntry) -> TranscriptEntry {
    serialize_transcript_entry(entry).parsed
}

fn serialize_transcript_entry(entry: &TranscriptWriteEntry) -> FinalisedEntry {
    let uuid = entry
        .uuid
        .clone()
        .unwrap_or_else(|| mint_entry_uuid(&entry.entry_type, &entry.payload));
    let timestamp = entry
        .timestamp
        .clone()
        .unwrap_or_else(|| format_system_time_iso_ms(SystemTime::now()));

    let mut obj = match entry.payload.clone() {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            if !other.is_null() {
                map.insert("payload".into(), other);
            }
            map
        }
    };
    obj.insert("type".into(), Value::String(entry.entry_type.clone()));
    obj.insert("uuid".into(), Value::String(uuid.clone()));
    obj.insert("timestamp".into(), Value::String(timestamp.clone()));
    if let Some(parent) = &entry.parent_uuid {
        obj.insert("parentUuid".into(), Value::String(parent.clone()));
    }
    let raw = Value::Object(obj);
    let mut serialized = raw.to_string();
    serialized.push('\n');

    let parsed = TranscriptEntry {
        entry_type: entry.entry_type.clone(),
        uuid,
        parent_uuid: entry.parent_uuid.clone(),
        timestamp: Some(timestamp),
        raw,
    };
    FinalisedEntry { serialized, parsed }
}

fn append_jsonl_line(path: &Path, line: &str) -> std::io::Result<()> {
    append_jsonl_lines(path, line)
}

fn append_jsonl_lines(path: &Path, buffer: &str) -> std::io::Result<()> {
    use std::io::Write;

    fn append_without_proof(path: &Path, buffer: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let result = file.write_all(buffer.as_bytes());
        if result.is_ok() {
            file.sync_data().ok();
        }
        // Generation acceleration is best-effort. If its lock is unavailable,
        // preserve the historical append behavior but make the write untrusted.
        std::fs::remove_file(transcript_append_generation_path(path)).ok();
        result
    }

    // Serialize cooperating appenders with generation publication. The transcript
    // write is durable before the atomic sidecar replace; readers that observe an
    // in-between state reject the generation and perform a full parse.
    let lock_path = path.with_extension("jsonl.publish.lock");
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)
    {
        Ok(lock) => lock,
        Err(_) => return append_without_proof(path, buffer),
    };
    if lock.lock_exclusive().is_err() {
        return append_without_proof(path, buffer);
    }

    // Proof is deliberately cooperative and bounded-size: a metadata- and
    // identity-valid sidecar under this lock represents the exact file prefix
    // left by the previous cooperating append. Open the append handle before
    // loading that proof so a pathname replacement cannot inherit it. Missing or
    // stale proof starts a fresh epoch. Never read or hash old transcript bytes;
    // writer work stays O(new bytes + fixed metadata/sidecar work).
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let previous = load_transcript_append_generation_for_file(path, &file);

    let result = file.write_all(buffer.as_bytes());
    if result.is_ok() {
        match file.sync_data() {
            Ok(()) => {
                if let Err(error) = publish_transcript_append_generation(
                    path,
                    &file,
                    previous.as_ref(),
                    buffer.as_bytes(),
                ) {
                    // Projection acceleration is optional. Never turn a successfully
                    // persisted transcript entry into a caller-visible write failure;
                    // removing the proof forces supervisors onto the full parser.
                    std::fs::remove_file(transcript_append_generation_path(path)).ok();
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "failed to publish transcript append generation"
                    );
                }
            }
            Err(error) => {
                // Never publish proof for bytes whose durability could not be
                // established. Preserve the historical successful-write result,
                // but force the next reader to rebuild from the transcript.
                std::fs::remove_file(transcript_append_generation_path(path)).ok();
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "failed to sync transcript append before generation publication"
                );
            }
        }
    }
    FileExt::unlock(&lock).ok();
    result
}

/// Mint a stable-ish uuid for an entry without pulling in a UUID
/// crate.
///
/// Hashes the entry content + a monotonically-increasing nanosecond
/// timestamp into a base36 string. Collisions require two entries to
/// hash identically within the same nanosecond — good enough for the
/// current callers (which pass explicit uuids in integration tests).
fn mint_entry_uuid(entry_type: &str, payload: &Value) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    entry_type.hash(&mut hasher);
    payload.to_string().hash(&mut hasher);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (nanos as u64).hash(&mut hasher);
    format!("u-{}", to_base36(hasher.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The bug this guards: a lock another handle holds must read as
    /// *contention*, not as an unevaluable error. `is_session_active` reports
    /// `false` for the latter, so a session someone else owns would look free
    /// and every surface built on that answer — hiding it from a listing,
    /// refusing to resume it — would silently stop working.
    ///
    /// Exercised through a real lock rather than a synthesised error, because
    /// the failure was platform-specific: Windows reports contention with a
    /// raw code that maps onto no nameable `ErrorKind`.
    #[test]
    fn a_lock_held_by_another_handle_reads_as_contention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess-x.active.lock");
        let open = || {
            OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
        };

        let held = open().unwrap();
        FileExt::try_lock_exclusive(&held).expect("the first handle takes the lock");

        let contender = open().unwrap();
        let err = FileExt::try_lock_exclusive(&contender)
            .expect_err("a second handle must not also take it");

        assert!(
            is_lock_contention(&err),
            "contention went unrecognised: kind={:?} raw_os_error={:?}",
            err.kind(),
            err.raw_os_error()
        );

        let _ = FileExt::unlock(&held);
    }

    /// A genuinely unevaluable lock must stay an error, so a real failure is
    /// never quietly reported as "nobody owns this".
    #[test]
    fn an_unrelated_io_error_is_not_contention() {
        assert!(!is_lock_contention(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!is_lock_contention(&std::io::Error::from(
            std::io::ErrorKind::InvalidData
        )));
    }

    /// Why the raw code is checked instead of `ErrorKind::Other`: since Rust
    /// 1.60 no standard-library error is ever `Other`, so an arm matching it
    /// cannot catch an OS error. Locking that in stops the check from being
    /// "simplified" back into the form that failed.
    #[test]
    fn an_os_error_never_has_kind_other() {
        let os_error = std::io::Error::from_raw_os_error(33);
        assert_ne!(os_error.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn cwd_identity_matches_windows_path_spelling_variants() {
        assert_eq!(
            cwd_identity_for_platform(r"F:\Dev\Sandbox\Rebon\", true),
            cwd_identity_for_platform("f:/dev/sandbox/rebon", true)
        );
        assert_eq!(cwd_identity_for_platform("C:/", true), "c:/");
        assert_eq!(
            cwd_identity_for_platform("C:/ÜBER/Rebon", true),
            cwd_identity_for_platform("c:/über/rebon", true)
        );
    }

    /// `same_cwd` skips building either identity, so it has to agree with the
    /// spelling it replaced on every pair — including the corners the fast path
    /// handles by hand (drive roots, repeated and mixed separators) and the
    /// non-ASCII inputs it refuses to handle at all.
    #[test]
    fn same_cwd_agrees_with_comparing_two_identities() {
        const PATHS: &[&str] = &[
            "",
            "/",
            "//",
            r"\",
            "C:/",
            r"C:\",
            "c:/",
            "C:",
            "F:/dev/sandbox/rebon",
            r"F:\Dev\Sandbox\Rebon",
            r"F:\Dev\Sandbox\Rebon\",
            "F:/dev/sandbox/rebon//",
            "/repo/Rebon",
            "/repo/rebon",
            "/repo/rebon/",
            "C:/ÜBER/Rebon",
            "c:/über/rebon",
            "/仓库/雷本",
            "/仓库/雷本/",
        ];
        for windows in [true, false] {
            for left in PATHS {
                for right in PATHS {
                    assert_eq!(
                        same_cwd_for_platform(left, right, windows),
                        cwd_identity_for_platform(left, windows)
                            == cwd_identity_for_platform(right, windows),
                        "left={left:?} right={right:?} windows={windows}"
                    );
                }
            }
        }
    }

    #[test]
    fn cwd_identity_preserves_case_on_case_sensitive_platforms() {
        assert_ne!(
            cwd_identity_for_platform("/repo/Rebon", false),
            cwd_identity_for_platform("/repo/rebon", false)
        );
        assert_eq!(cwd_identity_for_platform("/repo/", false), "/repo");
    }

    #[test]
    fn parse_rfc3339_handles_z_suffix_and_fractionals() {
        // Ground-truth Unix seconds for 2026-04-17T07:58:31Z.
        // (20_560 days since 1970-01-01 × 86_400 + 7*3600 + 58*60 + 31.)
        const T_2026_04_17_075831: u64 = 1_776_412_711;

        // Zulu with milliseconds — the exact shape the transcript writer emits.
        let t =
            parse_rfc3339_to_system_time("2026-04-17T07:58:31.723Z").expect("parses with Z + ms");
        let expected =
            std::time::UNIX_EPOCH + std::time::Duration::new(T_2026_04_17_075831, 723_000_000);
        assert_eq!(t, expected);

        // Sub-second precision beyond millis — truncated to 9 digits.
        let t = parse_rfc3339_to_system_time("2026-04-17T07:58:31.723582Z")
            .expect("parses with Z + micros");
        let expected =
            std::time::UNIX_EPOCH + std::time::Duration::new(T_2026_04_17_075831, 723_582_000);
        assert_eq!(t, expected);

        // Whole seconds only, still with Z.
        let t = parse_rfc3339_to_system_time("2026-04-17T07:58:31Z").expect("parses without frac");
        let expected = std::time::UNIX_EPOCH + std::time::Duration::new(T_2026_04_17_075831, 0);
        assert_eq!(t, expected);
    }

    #[test]
    fn parse_rfc3339_handles_offsets_and_unix_epoch() {
        // +08:00 offset — 00:00 local is 16:00 the prior day in UTC.
        let t =
            parse_rfc3339_to_system_time("2026-04-17T00:00:00+08:00").expect("parses with +08:00");
        let expected_utc_from_z = parse_rfc3339_to_system_time("2026-04-16T16:00:00Z").unwrap();
        assert_eq!(t, expected_utc_from_z);

        // The Unix epoch itself round-trips exactly.
        let epoch = parse_rfc3339_to_system_time("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(epoch, std::time::UNIX_EPOCH);
    }

    #[test]
    fn parse_rfc3339_rejects_malformed_input() {
        assert!(parse_rfc3339_to_system_time("").is_none());
        assert!(parse_rfc3339_to_system_time("2026-04-17").is_none());
        assert!(parse_rfc3339_to_system_time("2026/04/17T00:00:00Z").is_none());
        assert!(parse_rfc3339_to_system_time("2026-04-17 07:58:31Z").is_none());
        // No tz suffix — we require one to avoid silent drift.
        assert!(parse_rfc3339_to_system_time("2026-04-17T07:58:31").is_none());
    }

    #[test]
    fn sanitize_path_replaces_non_alnum() {
        assert_eq!(
            sanitize_path("/Users/foo/my-project"),
            "-Users-foo-my-project"
        );
        assert_eq!(sanitize_path("C:\\tmp\\work"), "C--tmp-work");
        assert_eq!(sanitize_path("plugin:name:server"), "plugin-name-server");
        assert_eq!(sanitize_path(""), "");
    }

    #[test]
    fn sanitize_path_hashes_long_inputs() {
        let long = "a".repeat(MAX_SANITIZED_LENGTH + 50);
        let got = sanitize_path(&long);
        // Prefix is the first MAX_SANITIZED_LENGTH characters of the
        // sanitized input, followed by a `-{hash}` suffix.
        assert!(got.starts_with(&"a".repeat(MAX_SANITIZED_LENGTH)));
        let rest = &got[MAX_SANITIZED_LENGTH..];
        assert!(rest.starts_with('-'));
        assert!(rest.len() > 1);
    }

    #[test]
    fn transcript_file_path_composes_root_sanitize_and_session_id() {
        let root = PathBuf::from("/tmp/projects");
        let p = transcript_file_path(&root, "/tmp/work", "sess-abc");
        assert_eq!(
            p,
            PathBuf::from("/tmp/projects")
                .join("-tmp-work")
                .join("sess-abc.jsonl")
        );
    }

    fn entry(uuid: &str, parent: Option<&str>, ty: &str, ts: &str) -> TranscriptEntry {
        let mut obj = serde_json::Map::new();
        obj.insert("uuid".into(), json!(uuid));
        if let Some(p) = parent {
            obj.insert("parentUuid".into(), json!(p));
        } else {
            obj.insert("parentUuid".into(), Value::Null);
        }
        obj.insert("type".into(), json!(ty));
        obj.insert("timestamp".into(), json!(ts));
        TranscriptEntry {
            entry_type: ty.to_string(),
            uuid: uuid.to_string(),
            parent_uuid: parent.map(|s| s.to_string()),
            timestamp: Some(ts.to_string()),
            raw: Value::Object(obj),
        }
    }

    #[test]
    fn parse_transcript_skips_blank_and_garbage_lines() {
        let input = b"\n\n{not-json}\n{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2025-01-01T00:00:00Z\"}\n";
        let got = parse_transcript_jsonl(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "u1");
        assert_eq!(got[0].entry_type, "user");
    }

    #[test]
    fn parse_transcript_requires_uuid_and_type() {
        let input = br#"{"type":"user"}
{"uuid":"u1"}
{"type":"user","uuid":"u1"}
"#;
        let got = parse_transcript_jsonl(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "u1");
    }

    #[test]
    fn parse_transcript_rejects_non_string_header_fields() {
        let input = br#"{"type":1,"uuid":"bad-type"}
{"type":"user","uuid":2}
{"type":"user","uuid":"bad-parent","parentUuid":3}
{"type":"user","uuid":"bad-time","timestamp":4}
{"type":"assistant","uuid":"valid","parentUuid":null,"timestamp":null}
"#;
        let got = parse_transcript_jsonl(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "valid");
        assert!(got[0].parent_uuid.is_none());
        assert!(got[0].timestamp.is_none());
    }

    #[test]
    fn reconstruct_chain_empty_returns_none() {
        assert!(reconstruct_chain(Vec::new()).is_none());
    }

    #[test]
    fn reconstruct_chain_picks_latest_leaf_and_walks_parents() {
        let entries = vec![
            entry("u1", None, "user", "2025-01-01T00:00:00Z"),
            entry("a1", Some("u1"), "assistant", "2025-01-01T00:00:01Z"),
            entry("u2", Some("a1"), "user", "2025-01-01T00:00:02Z"),
        ];
        let loaded = reconstruct_chain(entries).unwrap();
        // Oldest-first ordering after reverse.
        let uuids: Vec<_> = loaded.messages.iter().map(|e| e.uuid.clone()).collect();
        assert_eq!(uuids, vec!["u1", "a1", "u2"]);
        assert!(loaded.title.is_none());
    }

    #[test]
    fn reconstruct_chain_ignores_non_user_assistant_leaves() {
        // A `system` leaf is present but should be ignored; the walker
        // picks the user leaf instead.
        let entries = vec![
            entry("u1", None, "user", "2025-01-01T00:00:00Z"),
            entry("sys1", None, "system", "2025-01-01T00:00:05Z"),
        ];
        let loaded = reconstruct_chain(entries).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].uuid, "u1");
    }

    #[test]
    fn reconstruct_chain_no_user_or_assistant_leaf_returns_none() {
        let entries = vec![entry("sys1", None, "system", "2025-01-01T00:00:05Z")];
        assert!(reconstruct_chain(entries).is_none());
    }

    #[test]
    fn reconstruct_chain_survives_parent_cycle() {
        // Pathological fixture: a leaf `good` walks into a self-loop at
        // `bad` (its parent is itself). The cycle guard must break out of
        // the walk after visiting `bad` once, returning the partial
        // chain `[bad, good]` rather than looping forever.
        //
        // `good` is the leaf (nothing references it as a parent). `bad`
        // is referenced by `good`, so it's not a leaf candidate — which
        // means the self-loop never becomes the starting point and the
        // cycle is only hit mid-walk.
        let entries = vec![
            entry("bad", Some("bad"), "assistant", "2025-01-01T00:00:00Z"),
            entry("good", Some("bad"), "user", "2025-01-01T00:00:02Z"),
        ];
        let loaded = reconstruct_chain(entries).unwrap();
        let uuids: Vec<_> = loaded.messages.iter().map(|m| m.uuid.clone()).collect();
        assert_eq!(uuids, vec!["bad", "good"]);
    }

    #[test]
    fn load_transcript_from_file_missing_returns_none() {
        let bogus = std::env::temp_dir().join("rebon-acp-nonexistent-xyzzy.jsonl");
        let _ = std::fs::remove_file(&bogus); // just in case
        let got = load_transcript_from_file(&bogus).unwrap();
        assert!(got.is_none());
        assert!(load_raw_transcript_from_file(&bogus).unwrap().is_none());
    }

    #[test]
    fn raw_transcript_loader_preserves_file_order_duplicates_and_skips_malformed_lines() {
        let root_dir = temp_projects_root("raw-transcript-loader");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/tmp/repo", "raw-loader").unwrap();
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"root\"}\n",
                "malformed\n",
                "{\"type\":\"assistant\",\"uuid\":\"dup\",\"parentUuid\":\"root\"}\n",
                "{\"type\":\"assistant\",\"uuid\":\"dup\",\"parentUuid\":null}\n"
            ),
        )
        .unwrap();

        let raw = load_raw_transcript_from_file(&path)
            .unwrap()
            .expect("existing transcript");
        assert_eq!(raw.parsed_row_count, 3);
        assert_eq!(raw.nonblank_row_count, 4);
        assert!(!raw.parse_complete);
        assert!(raw.byte_len > 0);
        assert_eq!(
            raw.entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["root", "dup", "dup"]
        );
        assert_eq!(raw.entries[1].parent_uuid.as_deref(), Some("root"));
        assert_eq!(raw.entries[2].parent_uuid, None);
    }

    #[test]
    fn simple_hash_matches_djb2_fixture() {
        // djb2('hello'), computed by hand:
        //   hash = 0
        //   for each c in 'hello':
        //     hash = ((hash << 5) - hash + code(c)) | 0
        //   abs(hash).toString(36)
        // Spot-check against a manually-computed value to guard against
        // shift / wrap regressions.
        let got = simple_hash("hello");
        // Recompute expected against the same algorithm as a sanity check.
        let expected = {
            let mut h: i32 = 0;
            for c in "hello".chars() {
                let code = c as i32;
                h = h.wrapping_shl(5).wrapping_sub(h).wrapping_add(code);
            }
            to_base36((h as i64).unsigned_abs())
        };
        assert_eq!(got, expected);
        assert!(!got.is_empty());
    }

    // ---- additional parser / chain / path edge cases ----
    //
    // Coverage matrix gap-fillers added in response to the project rule
    // that every Rust change ships with comprehensive
    // tests, not just a happy path.

    #[test]
    fn parse_transcript_skips_non_object_top_level() {
        // Top-level number / string / array values are valid JSON but
        // not transcript entries — they must be skipped, not crash the
        // parser: parsing is best effort.
        let input = br#"42
"hello"
[1,2,3]
{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z"}
"#;
        let got = parse_transcript_jsonl(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "u1");
    }

    #[test]
    fn parse_transcript_handles_crlf_line_endings_and_no_trailing_newline() {
        // Windows line endings and a missing final newline are both
        // valid JSONL — `String::lines()` already handles them, but
        // pinning that here guards against a future refactor that
        // switches to `split('\n')`.
        let input = b"{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2025-01-01T00:00:00Z\"}\r\n{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"timestamp\":\"2025-01-01T00:00:01Z\"}";
        let got = parse_transcript_jsonl(input);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].uuid, "u1");
        assert_eq!(got[1].uuid, "a1");
        assert_eq!(got[1].parent_uuid.as_deref(), Some("u1"));
    }

    #[test]
    fn parse_transcript_treats_empty_parent_as_none() {
        // `parentUuid: ""` is a degenerate-but-not-unheard-of legacy
        // form. The parser drops empty strings, so the chain walker
        // treats the entry as a root.
        let input =
            br#"{"type":"user","uuid":"u1","parentUuid":"","timestamp":"2025-01-01T00:00:00Z"}
"#;
        let got = parse_transcript_jsonl(input);
        assert_eq!(got.len(), 1);
        assert!(got[0].parent_uuid.is_none());
    }

    #[test]
    fn parse_transcript_invalid_utf8_returns_empty() {
        // The parser short-circuits on `from_utf8` failure rather than
        // panicking, matching the "best effort" promise for the loader.
        let bad = [0xff, 0xfe, 0xfd, b'\n'];
        let got = parse_transcript_jsonl(&bad);
        assert!(got.is_empty());
    }

    #[test]
    fn reconstruct_chain_walker_stops_at_missing_parent() {
        // The latest leaf points at a `parentUuid` that doesn't exist
        // in the entry map. The walker takes the leaf, then bails out
        // at the missing parent — returning a one-element chain.
        let entries = vec![entry("leaf", Some("ghost"), "user", "2025-01-01T00:00:00Z")];
        let loaded = reconstruct_chain(entries).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].uuid, "leaf");
    }

    #[test]
    fn reconstruct_chain_duplicate_uuid_last_writer_wins() {
        // Two entries with the same uuid: only the last one is kept in
        // `index_by_uuid` (a map insert overwrites). The chain walker
        // therefore sees the second
        // entry's metadata.
        let entries = vec![
            entry("dup", None, "user", "2025-01-01T00:00:00Z"),
            entry("dup", None, "assistant", "2025-01-01T00:00:05Z"),
        ];
        let loaded = reconstruct_chain(entries).unwrap();
        // The latest leaf is the assistant version.
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].entry_type, "assistant");
    }

    #[test]
    fn reconstruct_chain_recovers_mobile_duplicate_uuid_cycle_from_file_order() {
        let entries = vec![
            entry("root", None, "user", "2025-01-01T00:00:00Z"),
            entry(
                "old-tail",
                Some("root"),
                "assistant",
                "2025-01-01T00:00:01Z",
            ),
            entry(
                "u-mobile-command",
                Some("old-tail"),
                "user",
                "2025-01-01T00:00:02Z",
            ),
            entry(
                "first-answer",
                Some("u-mobile-command"),
                "assistant",
                "2025-01-01T00:00:03Z",
            ),
            entry(
                "u-mobile-command",
                Some("first-answer"),
                "user",
                "2025-01-01T00:00:04Z",
            ),
            entry(
                "latest-answer",
                Some("u-mobile-command"),
                "assistant",
                "2025-01-01T00:00:05Z",
            ),
        ];

        let indices = reconstruct_chain_indices(&entries);
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5]);
        let loaded = reconstruct_chain(entries).unwrap();
        assert_eq!(
            loaded
                .messages
                .iter()
                .map(|message| message.uuid.as_str())
                .collect::<Vec<_>>(),
            vec![
                "root",
                "old-tail",
                "u-mobile-command",
                "first-answer",
                "u-mobile-command",
                "latest-answer",
            ]
        );
    }

    #[test]
    fn reconstruct_chain_occurrence_recovery_keeps_partial_chain_when_not_longer() {
        let entries = vec![
            entry("loop", Some("loop"), "assistant", "2025-01-01T00:00:00Z"),
            entry("leaf", Some("loop"), "user", "2025-01-01T00:00:01Z"),
        ];

        assert_eq!(reconstruct_chain_indices(&entries), vec![0, 1]);
    }

    #[test]
    fn transcript_prompt_uuid_classification_covers_lifecycle_states() {
        let prompt_content = json!("mobile prompt");
        let user = TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-mobile-command".into(),
            parent_uuid: None,
            timestamp: Some("2025-01-01T00:00:00Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-mobile-command",
                "message": {"role": "user", "content": "mobile prompt"}
            }),
        };
        assert_eq!(
            classify_transcript_prompt_uuid(&[], "u-mobile-command", &prompt_content),
            TranscriptPromptUuidState::Missing
        );
        assert_eq!(
            classify_transcript_prompt_uuid(
                std::slice::from_ref(&user),
                "u-mobile-command",
                &prompt_content
            ),
            TranscriptPromptUuidState::MatchingIncomplete
        );

        let tool_use = TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a-tool".into(),
            parent_uuid: Some("u-mobile-command".into()),
            timestamp: Some("2025-01-01T00:00:01Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": "a-tool",
                "parentUuid": "u-mobile-command",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tool-1", "name": "Read", "input": {}}],
                    "stop_reason": "tool_use"
                }
            }),
        };
        assert_eq!(
            classify_transcript_prompt_uuid(
                &[user.clone(), tool_use],
                "u-mobile-command",
                &prompt_content
            ),
            TranscriptPromptUuidState::MatchingIncomplete
        );

        let answer = TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a-answer".into(),
            parent_uuid: Some("u-mobile-command".into()),
            timestamp: Some("2025-01-01T00:00:02Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": "a-answer",
                "parentUuid": "u-mobile-command",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "done"}],
                    "stop_reason": "end_turn"
                }
            }),
        };
        assert_eq!(
            classify_transcript_prompt_uuid(
                &[user.clone(), answer],
                "u-mobile-command",
                &prompt_content
            ),
            TranscriptPromptUuidState::MatchingComplete
        );
        assert_eq!(
            classify_transcript_prompt_uuid(
                &[user],
                "u-mobile-command",
                &json!("different prompt")
            ),
            TranscriptPromptUuidState::Conflict
        );
    }

    #[test]
    fn reconstruct_chain_single_root_message() {
        // Smallest non-degenerate transcript: one user message with no
        // parent. The walker terminates immediately after pushing it.
        let entries = vec![entry("only", None, "user", "2025-01-01T00:00:00Z")];
        let loaded = reconstruct_chain(entries).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].uuid, "only");
        assert!(loaded.messages[0].parent_uuid.is_none());
    }

    #[test]
    fn reconstruct_chain_long_linear_chain() {
        // Stress the walker against a 50-message linear chain to
        // catch any accidental O(n²) regressions and confirm ordering
        // for non-trivial transcripts.
        let mut entries = Vec::new();
        for i in 0..50 {
            let uuid = format!("u{i}");
            let parent = if i == 0 {
                None
            } else {
                Some(format!("u{}", i - 1))
            };
            let ts = format!("2025-01-01T00:00:{:02}Z", i);
            let mut obj = serde_json::Map::new();
            obj.insert("uuid".into(), serde_json::json!(uuid));
            obj.insert(
                "parentUuid".into(),
                match &parent {
                    Some(p) => serde_json::json!(p),
                    None => serde_json::Value::Null,
                },
            );
            obj.insert("type".into(), serde_json::json!("user"));
            obj.insert("timestamp".into(), serde_json::json!(ts));
            entries.push(TranscriptEntry {
                entry_type: "user".to_string(),
                uuid,
                parent_uuid: parent,
                timestamp: Some(ts),
                raw: Value::Object(obj),
            });
        }
        let loaded = reconstruct_chain(entries).unwrap();
        assert_eq!(loaded.messages.len(), 50);
        assert_eq!(loaded.messages[0].uuid, "u0");
        assert_eq!(loaded.messages[49].uuid, "u49");
    }

    #[test]
    fn sanitize_path_boundary_at_max_length() {
        // Length exactly = MAX_SANITIZED_LENGTH → no hash suffix.
        let exact = "a".repeat(MAX_SANITIZED_LENGTH);
        let s = sanitize_path(&exact);
        assert_eq!(s.len(), MAX_SANITIZED_LENGTH);
        assert_eq!(s, exact);

        // Length = MAX_SANITIZED_LENGTH + 1 → hash kicks in.
        let over = "a".repeat(MAX_SANITIZED_LENGTH + 1);
        let s = sanitize_path(&over);
        assert!(s.len() > MAX_SANITIZED_LENGTH);
        assert!(s.starts_with(&exact));
        assert!(s[MAX_SANITIZED_LENGTH..].starts_with('-'));
    }

    #[test]
    fn reconstruct_chain_walks_past_trailing_system_terminal_to_user_assistant_ancestor() {
        // Regression fixture for The behavioral gap: a trailing `system`
        // terminal (e.g. a late metadata line appended to the JSONL
        // after the assistant's turn) should NOT hide the rest of the
        // chain. The walker has to climb past it to find the nearest
        // user/assistant ancestor and anchor the chain there.
        //
        // Topology:
        //   u1 (user, root)
        //    └── a1 (assistant, parent=u1)
        //         └── sys1 (system, parent=a1)   <- only terminal
        //
        // Expected: chain = [u1, a1]. The system entry is walked past
        // but never appears in the returned chain (because the walker
        // re-ascend/ re-ascends from `a1`, and `a1.parentUuid = u1`).
        let entries = vec![
            entry("u1", None, "user", "2025-01-01T00:00:00Z"),
            entry("a1", Some("u1"), "assistant", "2025-01-01T00:00:01Z"),
            entry("sys1", Some("a1"), "system", "2025-01-01T00:00:02Z"),
        ];
        let loaded = reconstruct_chain(entries).expect("chain must be reconstructed");
        let uuids: Vec<_> = loaded.messages.iter().map(|e| e.uuid.clone()).collect();
        assert_eq!(
            uuids,
            vec!["u1", "a1"],
            "walker must walk past the trailing system terminal to the nearest user/assistant ancestor"
        );
        // Belt-and-suspenders: verify the system entry was not smuggled
        // into the returned chain under a different guise.
        assert!(loaded.messages.iter().all(|m| m.entry_type != "system"));
    }

    #[test]
    fn reconstruct_chain_handles_attachment_terminal_with_nearer_user_ancestor() {
        // A trailing `attachment` terminal off a `user` ancestor. Same
        // rule as the `system` case: walk past the non-user/assistant
        // terminal, anchor on the nearest user/assistant ancestor.
        //
        // Topology:
        //   u1 (user, root)
        //    └── att1 (attachment, parent=u1)
        let entries = vec![
            entry("u1", None, "user", "2025-02-02T00:00:00Z"),
            entry("att1", Some("u1"), "attachment", "2025-02-02T00:00:01Z"),
        ];
        let loaded = reconstruct_chain(entries).expect("chain must be reconstructed");
        let uuids: Vec<_> = loaded.messages.iter().map(|e| e.uuid.clone()).collect();
        assert_eq!(uuids, vec!["u1"]);
    }

    #[test]
    fn reconstruct_chain_picks_later_user_assistant_ancestor_across_multiple_terminals() {
        // Two independent trailing system terminals, each anchoring a
        // different user/assistant ancestor. The one with the later
        // *ancestor* timestamp must win — the "most recent leaf" selection
        // applies *after* the walker has re-ascended to user/assistant.
        //
        // Topology:
        //   u1 (user, root, ts=00:00:00)                   \
        //    └── sys_a (system, parent=u1, ts=05:00:00)    /  ancestor -> u1
        //   u2 (user, root, ts=01:00:00)                   \
        //    └── sys_b (system, parent=u2, ts=00:00:00)    /  ancestor -> u2
        //
        // Terminal timestamps would pick sys_a → u1 (`05:00 > 00:00`).
        // Ancestor timestamps pick u2 (`01:00 > 00:00`). This test
        // distinguishes the two and locks in the ancestor-timestamp
        // selection path.
        let entries = vec![
            entry("u1", None, "user", "2025-03-03T00:00:00Z"),
            entry("sys_a", Some("u1"), "system", "2025-03-03T05:00:00Z"),
            entry("u2", None, "user", "2025-03-03T01:00:00Z"),
            entry("sys_b", Some("u2"), "system", "2025-03-03T00:00:00Z"),
        ];
        let loaded = reconstruct_chain(entries).expect("chain must be reconstructed");
        let uuids: Vec<_> = loaded.messages.iter().map(|e| e.uuid.clone()).collect();
        assert_eq!(uuids, vec!["u2"]);
    }

    #[test]
    fn reconstruct_chain_still_returns_none_when_terminal_walk_finds_no_user_or_assistant() {
        // A transcript whose *only* entries are system/attachment lines
        // — the walker visits them, finds no user/assistant ancestor
        // anywhere, and bails with `None`. This is the "loader can
        // still fail outright" branch that the module-level docs now
        // describe honestly.
        let entries = vec![
            entry("sys1", None, "system", "2025-04-04T00:00:00Z"),
            entry("att1", Some("sys1"), "attachment", "2025-04-04T00:00:01Z"),
        ];
        assert!(reconstruct_chain(entries).is_none());
    }

    #[test]
    fn reconstruct_chain_walker_handles_cycle_mid_walk_from_system_terminal() {
        // Pathological fixture: a system terminal's walk towards the
        // nearest user/assistant ancestor hits a parent self-loop on a
        // non-user/assistant entry before finding one. The walker must
        // break out of the per-walk cycle (via its own `walk_seen`)
        // without crashing. Since no user/assistant ancestor is ever
        // found, the returned chain is None.
        let entries = vec![entry(
            "bad_sys",
            Some("bad_sys"),
            "system",
            "2025-05-05T00:00:00Z",
        )];
        assert!(reconstruct_chain(entries).is_none());
    }

    // ---- active-lock cleanup ----

    fn unique_tmp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-acp-test-{tag}-"))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn cleanup_removes_orphan_locks_but_keeps_live_and_non_locks() {
        let root_dir = unique_tmp_dir("lockgc");
        let root = root_dir.path();
        let cwd = "proj";

        // Acquire a live lock first, so its built-in sweep sees no orphans yet.
        let live = try_acquire_session_active_lock(root, cwd, "sess-live")
            .unwrap()
            .expect("acquire live lock");
        let live_path = session_active_lock_path(root, cwd, "sess-live");
        let dir = project_dir_path(root, cwd);

        // Drop in orphan lock files (crashed sessions) + an unrelated file.
        let orphan_a = session_active_lock_path(root, cwd, "sess-dead-a");
        let orphan_b = session_active_lock_path(root, cwd, "sess-dead-b");
        std::fs::write(&orphan_a, b"").unwrap();
        std::fs::write(&orphan_b, b"").unwrap();
        let keep_file = dir.join("sess-dead-a.jsonl");
        std::fs::write(&keep_file, b"transcript").unwrap();

        let removed = cleanup_stale_active_locks(root, cwd);

        assert_eq!(removed, 2, "both orphan locks removed");
        assert!(!orphan_a.exists());
        assert!(!orphan_b.exists());
        assert!(keep_file.exists(), "non-lock files untouched");
        assert!(
            live_path.exists(),
            "a lock held by this process must survive GC"
        );

        drop(live);
    }

    #[test]
    fn dropping_only_active_lock_removes_empty_project_dir() {
        let root_dir = unique_tmp_dir("lock-drop-empty");
        let root = root_dir.path();
        let cwd = "proj";
        let dir = project_dir_path(root, cwd);
        let lock = try_acquire_session_active_lock(root, cwd, "sess-live")
            .unwrap()
            .expect("acquire active lock");
        assert!(dir.is_dir());

        drop(lock);

        assert!(!dir.exists());
    }

    #[test]
    fn dropping_active_lock_preserves_nonempty_project_dir() {
        let root_dir = unique_tmp_dir("lock-drop-nonempty");
        let root = root_dir.path();
        let cwd = "proj";
        let dir = project_dir_path(root, cwd);
        let lock = try_acquire_session_active_lock(root, cwd, "sess-live")
            .unwrap()
            .expect("acquire active lock");
        let transcript = dir.join("sess-live.jsonl");
        std::fs::write(&transcript, b"transcript").unwrap();

        drop(lock);

        assert!(dir.is_dir());
        assert!(transcript.is_file());
    }

    #[test]
    fn cleanup_orphan_lock_removes_empty_project_dir() {
        let root_dir = unique_tmp_dir("lockgc-empty-dir");
        let root = root_dir.path();
        let cwd = "proj";
        let dir = project_dir_path(root, cwd);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(session_active_lock_path(root, cwd, "sess-dead"), b"").unwrap();

        assert_eq!(cleanup_stale_active_locks(root, cwd), 1);
        assert!(!dir.exists());
    }

    #[test]
    fn empty_cwd_lock_never_reclaims_the_projects_root() {
        let root_dir = unique_tmp_dir("lock-drop-empty-cwd");
        let root = root_dir.path();
        // An empty cwd sanitizes to an empty component, so the lock lives
        // directly in the projects root. Releasing it must not take the root.
        let lock = try_acquire_session_active_lock(root, "", "sess-live")
            .unwrap()
            .expect("acquire active lock");

        drop(lock);

        assert!(root.is_dir());
    }

    #[test]
    fn acquire_recreates_a_project_dir_reclaimed_mid_flight() {
        let root_dir = unique_tmp_dir("lock-dir-recreated");
        let root = root_dir.path();
        let cwd = "proj";
        let dir = project_dir_path(root, cwd);
        // Stand in for the race where another process reclaimed the empty
        // project directory between our `create_dir_all` and the lock open.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::remove_dir(&dir).unwrap();

        let lock = try_acquire_session_active_lock(root, cwd, "sess-live")
            .unwrap()
            .expect("acquire must recreate the reclaimed directory");

        assert!(session_active_lock_path(root, cwd, "sess-live").is_file());
        drop(lock);
    }

    #[test]
    fn inactive_probe_does_not_leave_empty_project_dir() {
        let root_dir = unique_tmp_dir("lock-probe-empty-dir");
        let root = root_dir.path();
        let cwd = "proj";
        let dir = project_dir_path(root, cwd);

        assert!(!is_session_active(root, cwd, "sess-missing"));
        assert!(!dir.exists());
    }

    #[cfg(windows)]
    #[test]
    fn active_lock_treats_equivalent_windows_cwds_as_same_session() {
        let root_dir = unique_tmp_dir("lock-identity");
        let root = root_dir.path();
        let first_cwd = r"F:\Dev\Rebon\";
        let equivalent_cwd = "f:/dev/rebon";
        let session_id = "sess-equivalent";
        let lock = try_acquire_session_active_lock(root, first_cwd, session_id)
            .unwrap()
            .expect("acquire first lock");

        assert!(lock.is_for(root, equivalent_cwd, session_id));
        assert!(
            try_acquire_session_active_lock(root, equivalent_cwd, session_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn acquire_sweeps_preexisting_orphan_locks() {
        let root_dir = unique_tmp_dir("lockacq");
        let root = root_dir.path();
        let cwd = "proj";
        std::fs::create_dir_all(project_dir_path(root, cwd)).unwrap();

        let orphan = session_active_lock_path(root, cwd, "sess-dead");
        std::fs::write(&orphan, b"").unwrap();
        assert!(orphan.exists());

        // Opening any session in this dir should sweep the orphan as a side
        // effect, while keeping its own lock.
        let live = try_acquire_session_active_lock(root, cwd, "sess-new")
            .unwrap()
            .expect("acquire new lock");

        assert!(!orphan.exists(), "preexisting orphan swept on acquire");
        assert!(session_active_lock_path(root, cwd, "sess-new").exists());

        drop(live);
    }

    // ---- project_dir_path ----

    #[test]
    fn project_dir_path_joins_sanitized_cwd_under_root() {
        let root = PathBuf::from("/tmp/projects");
        let p = project_dir_path(&root, "/tmp/foo/work");
        assert_eq!(p, PathBuf::from("/tmp/projects").join("-tmp-foo-work"));
    }

    /// The extended-length spelling is the same directory, and must key the same.
    ///
    /// `\\?\F:\dev\x` comes back from `canonicalize` and from anything built to
    /// clear `MAX_PATH`, so it reaches this function in ordinary use. While it
    /// keyed separately, a session its worker recorded under one spelling was
    /// invisible to a reader using the other: a reader found no owner for
    /// a session that had a live one, and opened it as if nobody held it.
    #[test]
    fn an_extended_length_path_keys_the_same_as_the_plain_spelling() {
        for (verbatim, plain) in [
            (r"\\?\F:\dev\x", r"F:\dev\x"),
            (r"\\?\F:\dev\x", "f:/dev/x"),
            (r"\\?\UNC\host\share\x", r"\\host\share\x"),
        ] {
            assert_eq!(
                cwd_identity_for_platform(verbatim, true),
                cwd_identity_for_platform(plain, true),
                "{verbatim} and {plain} are one directory"
            );
            assert_eq!(
                sanitize_path(&cwd_identity_for_platform(verbatim, true)),
                sanitize_path(&cwd_identity_for_platform(plain, true)),
                "and therefore one projects-root component"
            );
            assert!(
                same_cwd_for_platform(verbatim, plain, true),
                "the byte-wise fast path must answer what the identity answers"
            );
        }
        // Off Windows the prefix is not a prefix, it is a directory name.
        assert_ne!(
            cwd_identity_for_platform(r"\\?\F:\dev\x", false),
            cwd_identity_for_platform(r"F:\dev\x", false)
        );
    }

    /// A project directory left under the old key is folded into the new one
    /// the first time the new key is used.
    ///
    /// A Windows worker's cwd comes from `canonicalize` and always carried
    /// `\\?\`, so folding the key without moving the directory would take
    /// every background session's transcript from "only the worker can find
    /// it" to "nobody can".
    #[test]
    fn a_project_directory_under_the_old_key_is_folded_into_the_new_one() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let cwd = r"F:\dev\folded-project";

        // What a worker wrote before the key folded the prefix.
        let old = root.join(sanitize_path(&format!(
            "//?/{}",
            cwd_identity_for_platform(cwd, true)
        )));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("sess-old.jsonl"), b"{}\n").unwrap();
        std::fs::write(old.join(PROJECT_CWD_SIDECAR), cwd).unwrap();

        // On a non-Windows host the identity does not fold case or
        // separators, so drive the key the same way the platform-parameterised
        // identity is driven elsewhere in this file.
        let target = root.join(sanitize_path(&cwd_identity_for_platform(cwd, true)));
        assert_ne!(old, target, "the two keys are the point of the test");

        fold_one_project_dir(&old, &target);

        assert!(
            target.join("sess-old.jsonl").exists(),
            "the transcript moved to the key everything reads"
        );
        assert!(!old.exists(), "and the emptied directory is gone");

        // Folding again is a no-op rather than a second move.
        fold_one_project_dir(&old, &target);
        assert!(target.join("sess-old.jsonl").exists());
    }

    /// Opening a session by the current key folds the old directory in, and
    /// does it once.
    ///
    /// The end-to-end shape of the migration: nothing calls a migration
    /// function, it happens because a project directory was reached for.
    #[cfg(windows)]
    #[test]
    fn opening_a_session_folds_the_old_project_directory_and_does_not_repeat() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let cwd = r"F:\dev\folded-on-open";
        let old = root.join(sanitize_path(&format!("//?/{}", cwd_identity(cwd))));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("sess-from-the-worker.jsonl"), b"{}\n").unwrap();

        let transcript = ensure_session_file_path(root, cwd, "sess-new").unwrap();
        let target = transcript.parent().unwrap().to_path_buf();

        assert!(
            target.join("sess-from-the-worker.jsonl").exists(),
            "the worker's transcript is where a reader with a plain cwd looks"
        );
        assert!(!old.exists(), "and the old directory is gone");

        // A second arrival under the same key must not walk the filesystem
        // again, and must not disturb what is now here.
        std::fs::write(old.join("..").join("decoy"), b"x").ok();
        let again = ensure_session_file_path(root, cwd, "sess-other").unwrap();
        assert_eq!(again.parent().unwrap(), target);
        assert!(target.join("sess-from-the-worker.jsonl").exists());
    }

    /// A name that exists in both directories is left alone, and the old
    /// directory stays because it still holds something.
    #[test]
    fn a_name_that_exists_in_both_project_directories_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let old = root.join("old-key");
        let target = root.join("new-key");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(old.join("sess-1.jsonl"), b"from the old key\n").unwrap();
        std::fs::write(target.join("sess-1.jsonl"), b"from the new key\n").unwrap();
        std::fs::write(old.join("sess-2.jsonl"), b"only in the old key\n").unwrap();

        fold_one_project_dir(&old, &target);

        assert_eq!(
            std::fs::read_to_string(target.join("sess-1.jsonl")).unwrap(),
            "from the new key\n",
            "the file already here is not replaced"
        );
        assert!(
            old.join("sess-1.jsonl").exists(),
            "and the one that could not move is still where it was"
        );
        assert!(
            target.join("sess-2.jsonl").exists(),
            "the rest moves regardless"
        );
        assert!(
            old.exists(),
            "a directory that still holds something is not removed"
        );
    }

    /// Only the two extended-length spellings are candidates. This is not a
    /// scan: a directory that merely looks related is not touched.
    #[test]
    fn only_the_two_extended_length_spellings_are_folded() {
        assert_eq!(
            legacy_project_dir_identities("f:/dev/x"),
            vec!["//?/f:/dev/x".to_string()]
        );
        assert_eq!(
            legacy_project_dir_identities("//host/share/x"),
            vec!["//?/unc/host/share/x".to_string()]
        );
        // A plain POSIX path has neither spelling, so nothing is a candidate.
        assert!(legacy_project_dir_identities("/home/user/repo").is_empty());
        assert!(legacy_project_dir_identities("").is_empty());
    }

    #[test]
    fn project_dir_component_collapses_windows_spelling_variants() {
        // The projects-dir key must go through `cwd_identity`, so every
        // spelling of the same Windows directory lands on ONE component.
        assert_eq!(
            sanitize_path(&cwd_identity_for_platform(r"F:\Dev\Sandbox\Rebon", true)),
            sanitize_path(&cwd_identity_for_platform("f:/dev/sandbox/rebon", true))
        );
        // On case-sensitive platforms distinct spellings stay distinct.
        assert_ne!(
            sanitize_path(&cwd_identity_for_platform("/repo/Rebon", false)),
            sanitize_path(&cwd_identity_for_platform("/repo/rebon", false))
        );
        #[cfg(windows)]
        {
            let root = PathBuf::from("/tmp/projects");
            assert_eq!(
                project_dir_path(&root, r"F:\Dev\Work"),
                project_dir_path(&root, "f:/dev/work")
            );
        }
    }

    #[test]
    fn fold_project_dir_name_matches_component_for_legacy_case_spellings() {
        // A dir created from the raw spelling before normalization
        // (`sanitize_path` only) must fold onto the canonical component.
        let cwd = if cfg!(windows) {
            r"F:\Dev\Work"
        } else {
            "/dev/Work"
        };
        assert_eq!(
            fold_project_dir_name(&sanitize_path(cwd)),
            fold_project_dir_name(&project_dir_component(cwd))
        );
        // Canonical components are already folded.
        let component = project_dir_component(cwd);
        assert_eq!(fold_project_dir_name(&component), component);
    }

    #[test]
    fn project_dir_path_matches_transcript_file_path_parent() {
        // A session file for the same cwd must land inside the
        // directory `project_dir_path` returns — this is the invariant
        // the `session/list` scanner relies on.
        let root = PathBuf::from("/tmp/projects");
        let dir = project_dir_path(&root, "/tmp/work");
        let file = transcript_file_path(&root, "/tmp/work", "sess-x");
        assert_eq!(file.parent().unwrap(), dir.as_path());
    }

    #[test]
    fn project_dir_path_handles_empty_cwd_as_empty_component() {
        // An empty cwd sanitizes to an empty string, producing a
        // trailing join that still lives strictly under the root. This
        // should not panic and should not escape the root.
        let root = PathBuf::from("/tmp/projects");
        let p = project_dir_path(&root, "");
        assert!(p.starts_with(&root));
    }

    // format_system_time_iso_ms and civil_from_days tests live in rebon-types.

    #[test]
    fn transcript_file_path_handles_cwd_with_special_characters() {
        // A cwd with spaces, slashes, and a colon (e.g. a Windows path
        // with a drive letter) routes to the sanitized component name,
        // preserving the writer's path semantics. We assert that
        // the joined path has the right *file name* component instead
        // of comparing the full PathBuf, because the per-platform
        // separator differs.
        let root = PathBuf::from("/tmp/projects");
        let p = transcript_file_path(&root, "/path with space/foo:bar", "sess-x");
        let parent_name = p
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(parent_name, "-path-with-space-foo-bar");
        let leaf = p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(leaf, "sess-x.jsonl");
    }

    // ── write-path tests ──────────────────────────────────────────────

    fn temp_projects_root(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-acp-write-{tag}-"))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn creating_a_session_on_disk_leaves_an_empty_transcript_and_a_date() {
        let root_dir = temp_projects_root("create-on-disk");
        let root = root_dir.path();
        let cwd = "/repo";

        let path = create_session_on_disk(root, cwd, "s1").unwrap();
        assert!(path.is_file(), "an empty transcript another process opens");
        assert_eq!(std::fs::read(&path).unwrap(), b"", "nothing said yet");
        let created = load_session_created_at_ms(root, cwd, "s1").expect("a date");

        // Asking again is not a new session: neither the transcript nor the
        // date it was created may move.
        append_transcript_entry(
            root,
            cwd,
            "s1",
            TranscriptWriteEntry::new("user", json!({"text": "hi"})),
        )
        .unwrap();
        let said = std::fs::read(&path).unwrap();
        assert_eq!(create_session_on_disk(root, cwd, "s1").unwrap(), path);
        assert_eq!(std::fs::read(&path).unwrap(), said, "the turn survived");
        assert_eq!(load_session_created_at_ms(root, cwd, "s1"), Some(created));
    }

    #[test]
    fn a_recorded_creation_time_is_written_once_and_read_back() {
        let root_dir = temp_projects_root("created-at");
        let root = root_dir.path();
        let cwd = "/repo";
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        assert_eq!(load_session_created_at_ms(root, cwd, "s1"), None);
        record_session_created_at(root, cwd, "s1").unwrap();
        let recorded = load_session_created_at_ms(root, cwd, "s1").expect("a recorded date");
        assert!(recorded + 1_000 >= before, "{recorded} is before the call");

        // A later call is not a new session: rewriting the date would move
        // the session to the top of every list sorted by age.
        record_session_created_at(root, cwd, "s1").unwrap();
        assert_eq!(load_session_created_at_ms(root, cwd, "s1"), Some(recorded));
    }

    #[test]
    fn recording_a_creation_time_keeps_the_other_sidecar_fields() {
        let root_dir = temp_projects_root("created-at-merge");
        let root = root_dir.path();
        let cwd = "/repo";
        save_session_title(root, cwd, "s1", "a title").unwrap();
        record_session_created_at(root, cwd, "s1").unwrap();
        save_session_mode(root, cwd, "s1", "coordinator").unwrap();

        assert_eq!(
            load_session_title(root, cwd, "s1").as_deref(),
            Some("a title")
        );
        assert_eq!(
            load_session_mode(root, cwd, "s1").as_deref(),
            Some("coordinator")
        );
        assert!(load_session_created_at_ms(root, cwd, "s1").is_some());
    }

    #[test]
    fn the_first_stamped_transcript_row_dates_the_session() {
        let root_dir = temp_projects_root("first-stamp");
        let path = root_dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "not-json\n",
                "{\"type\":\"user\"}\n",
                "{\"timestamp\":\"invalid\"}\n",
                "{\"timestamp\":\"2024-01-01T00:00:00Z\"}\n",
                "{\"timestamp\":\"2025-01-01T00:00:00Z\"}\n"
            ),
        )
        .unwrap();

        assert_eq!(
            transcript_first_timestamp_ms(&path),
            Some(1_704_067_200_000)
        );
    }

    #[test]
    fn a_transcript_with_no_stamped_row_has_no_date_to_give() {
        let root_dir = temp_projects_root("no-stamp");
        let empty = root_dir.path().join("empty.jsonl");
        let unstamped = root_dir.path().join("unstamped.jsonl");
        std::fs::write(&empty, "").unwrap();
        std::fs::write(&unstamped, "{\"type\":\"user\"}\n").unwrap();

        assert_eq!(transcript_first_timestamp_ms(&empty), None);
        assert_eq!(transcript_first_timestamp_ms(&unstamped), None);
        assert_eq!(
            transcript_first_timestamp_ms(&root_dir.path().join("absent.jsonl")),
            None
        );
    }

    #[test]
    fn every_appended_transcript_row_carries_a_timestamp() {
        let root_dir = temp_projects_root("row-stamp");
        let root = root_dir.path();
        append_transcript_entry(
            root,
            "/repo",
            "s1",
            TranscriptWriteEntry::new("user", json!({"text": "hi"})),
        )
        .unwrap();

        let path = transcript_file_path(root, "/repo", "s1");
        let dated = transcript_first_timestamp_ms(&path).expect("the row dates the session");
        assert!(dated > 0);
    }

    #[test]
    fn ensure_session_file_path_creates_parent_dir() {
        let root_dir = temp_projects_root("ensure");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/Users/foo/project", "sess-1").unwrap();
        assert!(path.parent().unwrap().exists());
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "sess-1.jsonl");
    }

    #[test]
    fn find_session_transcript_cwd_returns_unique_sidecar_match() {
        let root_dir = temp_projects_root("find-session-unique");
        let root = root_dir.path();
        let cwd = "/repo/.rebon/worktrees/bg-1";
        let path = ensure_session_file_path(root, cwd, "sess-find").unwrap();
        std::fs::write(path, b"{}").unwrap();

        assert_eq!(
            find_session_transcript_cwd(root, "sess-find").as_deref(),
            Some(cwd)
        );
    }

    #[test]
    fn find_session_transcript_cwd_rejects_ambiguous_matches() {
        let root_dir = temp_projects_root("find-session-ambiguous");
        let root = root_dir.path();
        let first = ensure_session_file_path(root, "/repo/worktree-a", "sess-find").unwrap();
        let second = ensure_session_file_path(root, "/repo/worktree-b", "sess-find").unwrap();
        std::fs::write(first, b"{}").unwrap();
        std::fs::write(second, b"{}").unwrap();

        assert_eq!(find_session_transcript_cwd(root, "sess-find"), None);
    }

    #[test]
    fn find_session_transcript_cwd_ignores_match_without_sidecar() {
        let root_dir = temp_projects_root("find-session-no-sidecar");
        let root = root_dir.path();
        let project_dir = root.join("legacy-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("sess-find.jsonl"), b"{}").unwrap();

        assert_eq!(find_session_transcript_cwd(root, "sess-find"), None);
    }

    #[test]
    fn append_transcript_entry_writes_and_loads_back() {
        let root_dir = temp_projects_root("append");
        let root = root_dir.path();
        let entry = TranscriptWriteEntry::new(
            "user",
            json!({
                "message": { "role": "user", "content": "hello" }
            }),
        )
        .with_uuid("u-1")
        .with_timestamp("2026-04-09T00:00:00.000Z");

        let parsed = append_transcript_entry(root, "/tmp/repo", "sess-a", entry).unwrap();
        assert_eq!(parsed.uuid, "u-1");
        assert_eq!(parsed.entry_type, "user");
        assert_eq!(
            parsed.timestamp.as_deref(),
            Some("2026-04-09T00:00:00.000Z")
        );

        // Load via the read path to verify the wire shape round-trips.
        let path = transcript_file_path(root, "/tmp/repo", "sess-a");
        let loaded = load_transcript_from_file(&path).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].uuid, "u-1");
        assert_eq!(loaded.messages[0].entry_type, "user");
        assert_eq!(loaded.messages[0].raw["message"]["content"], json!("hello"));
    }

    #[test]
    fn write_transcript_entries_chain_round_trips_through_loader() {
        let root_dir = temp_projects_root("chain");
        let root = root_dir.path();
        let entries = vec![
            TranscriptWriteEntry::new(
                "user",
                json!({
                    "message": { "role": "user", "content": "read a.rs" }
                }),
            )
            .with_uuid("u-1")
            .with_timestamp("2026-04-09T00:00:00.000Z"),
            TranscriptWriteEntry::new(
                "assistant",
                json!({
                    "message": { "role": "assistant", "content": "reading now" }
                }),
            )
            .with_uuid("u-2")
            .with_parent("u-1")
            .with_timestamp("2026-04-09T00:00:01.000Z"),
            TranscriptWriteEntry::new(
                "user",
                json!({
                    "message": { "role": "user", "content": "tool_result: file body" }
                }),
            )
            .with_uuid("u-3")
            .with_parent("u-2")
            .with_timestamp("2026-04-09T00:00:02.000Z"),
        ];
        write_transcript_entries(root, "/tmp/repo", "sess-b", entries).unwrap();

        let path = transcript_file_path(root, "/tmp/repo", "sess-b");
        let loaded = load_transcript_from_file(&path).unwrap().unwrap();
        let uuids: Vec<_> = loaded.messages.iter().map(|m| m.uuid.clone()).collect();
        assert_eq!(uuids, vec!["u-1", "u-2", "u-3"]);
    }

    #[test]
    fn append_transcript_entry_is_additive_across_calls() {
        let root_dir = temp_projects_root("additive");
        let root = root_dir.path();
        append_transcript_entry(
            root,
            "/tmp/repo",
            "sess-c",
            TranscriptWriteEntry::new("user", json!({"message": {"role":"user","content":"a"}}))
                .with_uuid("u-1")
                .with_timestamp("2026-04-09T00:00:00.000Z"),
        )
        .unwrap();
        append_transcript_entry(
            root,
            "/tmp/repo",
            "sess-c",
            TranscriptWriteEntry::new(
                "assistant",
                json!({"message": {"role":"assistant","content":"b"}}),
            )
            .with_uuid("u-2")
            .with_parent("u-1")
            .with_timestamp("2026-04-09T00:00:01.000Z"),
        )
        .unwrap();

        let path = transcript_file_path(root, "/tmp/repo", "sess-c");
        let loaded = load_transcript_from_file(&path).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].uuid, "u-1");
        assert_eq!(loaded.messages[1].uuid, "u-2");
    }

    #[test]
    fn append_generation_extends_valid_proof_and_reseeds_missing_or_stale_proof() {
        let root_dir = temp_projects_root("append-epoch-proof");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/tmp/repo", "sess-proof").unwrap();
        append_jsonl_lines(
            &path,
            "{\"type\":\"user\"}
",
        )
        .unwrap();
        let first = load_transcript_append_generation(&path).unwrap();

        append_jsonl_lines(
            &path,
            "{\"type\":\"assistant\"}
",
        )
        .unwrap();
        let second = load_transcript_append_generation(&path).unwrap();
        assert_eq!(second.epoch, first.epoch);
        assert_eq!(second.generation, first.generation + 1);
        let second_step = second.journal.last().unwrap();
        assert_eq!(second_step.from_len, first.len);
        assert_eq!(
            second_step.previous_chain_sha256,
            first.journal.last().map(|step| step.chain_sha256)
        );
        assert!(validate_transcript_append_span(
            &first,
            &second,
            b"{\"type\":\"assistant\"}\n",
        ));
        assert!(!validate_transcript_append_span(
            &first,
            &second,
            b"different suffix\n",
        ));

        std::fs::remove_file(transcript_append_generation_path(&path)).unwrap();
        append_jsonl_lines(
            &path,
            "{\"type\":\"user\"}
",
        )
        .unwrap();
        let reseeded = load_transcript_append_generation(&path).unwrap();
        assert_ne!(reseeded.epoch, second.epoch);
        assert_eq!(reseeded.generation, 0);

        let mut stale = reseeded.clone();
        stale.len = stale.len.saturating_sub(1);
        std::fs::write(
            transcript_append_generation_path(&path),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        assert_eq!(load_transcript_append_generation(&path), None);
        append_jsonl_lines(
            &path,
            "{\"type\":\"assistant\"}
",
        )
        .unwrap();
        let after_stale = load_transcript_append_generation(&path).unwrap();
        assert_ne!(after_stale.epoch, reseeded.epoch);
        assert_eq!(after_stale.generation, 0);
    }

    #[test]
    fn append_journal_validates_generation_spans_and_fails_closed() {
        let root_dir = temp_projects_root("append-journal-span");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/tmp/repo", "sess-span").unwrap();
        append_jsonl_lines(&path, "{\"type\":\"user\"}\n").unwrap();
        let anchor = load_transcript_append_generation(&path).unwrap();

        append_jsonl_lines(&path, "{\"type\":\"assistant\"}\n").unwrap();
        append_jsonl_lines(&path, "{\"type\":\"tool_result\"}\n").unwrap();
        append_jsonl_lines(&path, "{\"type\":\"assistant\",\"final\":true}\n").unwrap();
        let latest = load_transcript_append_generation(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let suffix = &bytes[anchor.len as usize..];

        assert_eq!(latest.generation, anchor.generation + 3);
        assert!(validate_transcript_append_span(&anchor, &latest, suffix));

        let mut missing_first = latest.clone();
        missing_first.journal.remove(1);
        assert!(!validate_transcript_append_span(
            &anchor,
            &missing_first,
            suffix
        ));

        let mut evicted_needed_step = latest.clone();
        evicted_needed_step.journal.drain(..2);
        assert!(transcript_append_journal_is_valid(&evicted_needed_step));
        assert!(!validate_transcript_append_span(
            &anchor,
            &evicted_needed_step,
            suffix
        ));

        let mut tampered_suffix = suffix.to_vec();
        tampered_suffix[0] ^= 1;
        assert!(!validate_transcript_append_span(
            &anchor,
            &latest,
            &tampered_suffix
        ));

        let mut bad_digest = latest.clone();
        bad_digest.journal.last_mut().unwrap().append_sha256[0] ^= 1;
        assert!(!validate_transcript_append_span(
            &anchor,
            &bad_digest,
            suffix
        ));

        let mut oversized = latest.clone();
        let repeated = oversized.journal[0].clone();
        oversized
            .journal
            .resize(TRANSCRIPT_APPEND_JOURNAL_STEPS + 1, repeated);
        assert!(!validate_transcript_append_span(
            &anchor, &oversized, suffix
        ));
    }

    #[test]
    fn append_journal_and_serialized_sidecar_are_bounded() {
        let root_dir = temp_projects_root("append-journal-bound");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/tmp/repo", "sess-bound").unwrap();
        for generation in 0..(TRANSCRIPT_APPEND_JOURNAL_STEPS + 17) {
            append_jsonl_lines(&path, &format!("{{\"generation\":{generation}}}\n")).unwrap();
        }

        let latest = load_transcript_append_generation(&path).unwrap();
        assert_eq!(latest.journal.len(), TRANSCRIPT_APPEND_JOURNAL_STEPS);
        assert_eq!(
            latest.journal.first().unwrap().generation,
            latest.generation + 1 - TRANSCRIPT_APPEND_JOURNAL_STEPS as u64
        );
        let serialized = std::fs::read(transcript_append_generation_path(&path)).unwrap();
        assert!(
            serialized.len() <= TRANSCRIPT_APPEND_GENERATION_MAX_BYTES,
            "bounded journal sidecar unexpectedly grew to {} bytes",
            serialized.len()
        );
    }

    #[test]
    fn oversized_append_generation_sidecar_is_rejected() {
        let root_dir = temp_projects_root("append-sidecar-read-bound");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/tmp/repo", "sess-read-bound").unwrap();
        append_jsonl_lines(&path, "{\"type\":\"user\"}\n").unwrap();
        assert!(load_transcript_append_generation(&path).is_some());

        std::fs::write(
            transcript_append_generation_path(&path),
            vec![b' '; TRANSCRIPT_APPEND_GENERATION_MAX_BYTES + 1],
        )
        .unwrap();

        assert_eq!(load_transcript_append_generation(&path), None);
    }

    #[test]
    fn path_replacement_cannot_inherit_append_generation() {
        let root_dir = temp_projects_root("append-replacement-identity");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/tmp/repo", "sess-replaced").unwrap();
        append_jsonl_lines(&path, "{\"type\":\"user\",\"value\":\"old\"}\n").unwrap();
        let old_generation = load_transcript_append_generation(&path).unwrap();
        let displaced = path.with_extension("old.jsonl");
        std::fs::rename(&path, &displaced).unwrap();
        std::fs::write(&path, b"{\"type\":\"user\",\"value\":\"new\"}\n").unwrap();

        // Make every legacy sidecar check pass while deliberately retaining the
        // displaced file's identity. Identity binding must still reject it.
        let metadata = std::fs::metadata(&path).unwrap();
        let (len, modified_ns) = transcript_metadata_revision(&metadata).unwrap();
        let mut forged_stale = old_generation.clone();
        forged_stale.len = len;
        forged_stale.modified_ns = modified_ns;
        std::fs::write(
            transcript_append_generation_path(&path),
            serde_json::to_vec(&forged_stale).unwrap(),
        )
        .unwrap();
        assert_eq!(load_transcript_append_generation(&path), None);

        append_jsonl_lines(&path, "{\"type\":\"assistant\"}\n").unwrap();
        let reseeded = load_transcript_append_generation(&path).unwrap();
        assert_ne!(reseeded.epoch, old_generation.epoch);
        assert_eq!(reseeded.generation, 0);
    }

    #[test]
    fn append_hot_path_has_no_old_prefix_read_or_hash_across_helpers() {
        let source = include_str!("session_storage.rs");
        let append_body = source
            .split_once("fn append_jsonl_lines(path: &Path, buffer: &str)")
            .unwrap()
            .1
            .split_once("/// Mint a stable-ish uuid")
            .unwrap()
            .0;
        assert!(!append_body.contains("read_to_end"));
        assert!(!append_body.contains("std::fs::read(path)"));
        assert!(!append_body.contains("Sha256"));

        // Follow the two nontrivial helpers called by append_jsonl_lines. The
        // predecessor loader may read only the fixed-size sidecar, while the
        // publisher may hash only `appended`; neither may consume transcript bytes.
        let predecessor_body = source
            .split_once("fn load_transcript_append_generation_for_file(")
            .unwrap()
            .1
            .split_once("fn fresh_transcript_epoch")
            .unwrap()
            .0;
        assert!(predecessor_body.contains("TRANSCRIPT_APPEND_GENERATION_MAX_BYTES + 1"));
        assert!(predecessor_body.contains(".take("));
        assert!(predecessor_body.contains("read_to_end(&mut bytes)"));
        assert!(!predecessor_body.contains("std::fs::read("));
        assert!(!predecessor_body.contains("std::fs::read(path)"));
        assert!(!predecessor_body.contains("Sha256"));

        let publisher_body = source
            .split_once("fn publish_transcript_append_generation(")
            .unwrap()
            .1
            .split_once("/// A single on-disk transcript entry")
            .unwrap()
            .0;
        assert!(publisher_body.contains("Sha256::digest(appended)"));
        assert!(!publisher_body.contains("read_to_end"));
        assert!(!publisher_body.contains("std::fs::read(path)"));
        assert!(!publisher_body.contains("Sha256::digest(path"));
        assert!(!publisher_body.contains("Sha256::digest(file"));
    }

    #[test]
    fn append_succeeds_without_generation_when_publish_lock_is_unavailable() {
        let root_dir = temp_projects_root("append-lock-fallback");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/tmp/repo", "sess-lock").unwrap();
        let lock_path = path.with_extension("jsonl.publish.lock");
        std::fs::create_dir(&lock_path).unwrap();

        append_jsonl_lines(&path, "{\"type\":\"user\"}\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"type\":\"user\"}\n"
        );
        assert_eq!(load_transcript_append_generation(&path), None);
    }

    #[test]
    fn mint_uuid_is_stable_prefix() {
        let id = mint_entry_uuid("user", &json!({"message": "hi"}));
        assert!(id.starts_with("u-"));
        assert!(id.len() > 3);
    }

    #[test]
    fn write_entry_auto_stamps_timestamp_when_absent() {
        let root_dir = temp_projects_root("autostamp");
        let root = root_dir.path();
        let parsed = append_transcript_entry(
            root,
            "/tmp/repo",
            "sess-d",
            TranscriptWriteEntry::new("user", json!({"message": {"role":"user","content":"x"}})),
        )
        .unwrap();
        assert!(parsed.timestamp.is_some());
        // The uuid should have been minted.
        assert!(parsed.uuid.starts_with("u-"));
    }

    // ── session_meta sidecar ───────────────────────────────────────────

    #[test]
    fn session_meta_path_sits_next_to_transcript() {
        // The sidecar must live in the same directory as the jsonl so
        // the resume-dialog scanner can find both on a single
        // `read_dir` of the project directory.
        let root = PathBuf::from("/tmp/projects");
        let jsonl = transcript_file_path(&root, "/tmp/repo", "sess-x");
        let meta = session_meta_path(&root, "/tmp/repo", "sess-x");
        assert_eq!(jsonl.parent().unwrap(), meta.parent().unwrap());
        assert_eq!(
            meta.file_name().unwrap().to_string_lossy(),
            "sess-x.meta.json"
        );
    }

    #[test]
    fn session_title_roundtrip_through_sidecar() {
        let root_dir = temp_projects_root("meta-roundtrip");
        let root = root_dir.path();
        save_session_title(root, "/tmp/repo", "sess-m", "Fix login bug on mobile").expect("save");
        let got = load_session_title(root, "/tmp/repo", "sess-m");
        assert_eq!(got.as_deref(), Some("Fix login bug on mobile"));
    }

    #[test]
    fn hidden_from_chats_roundtrip_preserves_existing_metadata() {
        let root_dir = temp_projects_root("hidden-from-chats-roundtrip");
        let root = root_dir.path();
        save_session_title(root, "/tmp/repo", "sess-h", "Title").expect("save title");
        save_session_mode(root, "/tmp/repo", "sess-h", "coordinator").expect("save mode");

        save_session_hidden_from_chats(root, "/tmp/repo", "sess-h", true).expect("hide session");
        save_session_agent(root, "/tmp/repo", "sess-h", "verification")
            .expect("save agent after visibility");

        assert!(load_session_hidden_from_chats(root, "/tmp/repo", "sess-h"));
        assert_eq!(
            load_session_title(root, "/tmp/repo", "sess-h").as_deref(),
            Some("Title")
        );
        assert_eq!(
            load_session_mode(root, "/tmp/repo", "sess-h").as_deref(),
            Some("coordinator")
        );
    }

    #[test]
    fn hidden_from_chats_updates_preserve_unmodelled_sidecar_fields() {
        let root_dir = temp_projects_root("hidden-from-chats-extra-fields");
        let root = root_dir.path();
        let path = session_meta_path(root, "/tmp/repo", "sess-h");
        std::fs::create_dir_all(path.parent().expect("meta parent")).expect("create meta dir");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "title": "Title",
                "archived": true,
                "deleted": true,
                "futureMetadata": {"source": "app"}
            }))
            .expect("serialize meta"),
        )
        .expect("write meta");

        save_session_hidden_from_chats(root, "/tmp/repo", "sess-h", true).expect("hide session");
        let hidden: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read hidden meta"))
                .expect("parse hidden meta");
        assert_eq!(hidden["hiddenFromChats"], true);
        assert_eq!(hidden["archived"], true);
        assert_eq!(hidden["deleted"], true);
        assert_eq!(hidden["futureMetadata"], json!({"source": "app"}));

        save_session_hidden_from_chats(root, "/tmp/repo", "sess-h", false).expect("show session");
        let visible: Value =
            serde_json::from_slice(&std::fs::read(path).expect("read visible meta"))
                .expect("parse visible meta");
        assert!(visible.get("hiddenFromChats").is_none());
        assert_eq!(visible["archived"], true);
        assert_eq!(visible["deleted"], true);
        assert_eq!(visible["futureMetadata"], json!({"source": "app"}));
    }

    #[test]
    fn malformed_session_metadata_is_not_overwritten() {
        let root_dir = temp_projects_root("hidden-from-chats-malformed");
        let root = root_dir.path();
        let path = session_meta_path(root, "/tmp/repo", "sess-h");
        std::fs::create_dir_all(path.parent().expect("meta parent")).expect("create meta dir");
        std::fs::write(&path, b"{not-json").expect("write malformed meta");

        let error = save_session_hidden_from_chats(root, "/tmp/repo", "sess-h", true)
            .expect_err("malformed metadata must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read(path).expect("read unchanged meta"),
            b"{not-json"
        );
    }

    #[test]
    fn concurrent_session_metadata_updates_preserve_both_fields() {
        let root_dir = temp_projects_root("concurrent-session-meta");
        let root = root_dir.path();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        std::thread::scope(|scope| {
            let barrier_for_title = barrier.clone();
            scope.spawn(move || {
                barrier_for_title.wait();
                save_session_title(root, "/tmp/repo", "sess-c", "Title")
                    .expect("save title concurrently");
            });
            let barrier_for_mode = barrier.clone();
            scope.spawn(move || {
                barrier_for_mode.wait();
                save_session_mode(root, "/tmp/repo", "sess-c", "coordinator")
                    .expect("save mode concurrently");
            });
            barrier.wait();
        });

        assert_eq!(
            load_session_title(root, "/tmp/repo", "sess-c").as_deref(),
            Some("Title")
        );
        assert_eq!(
            load_session_mode(root, "/tmp/repo", "sess-c").as_deref(),
            Some("coordinator")
        );
    }

    #[test]
    fn hidden_from_chats_defaults_false_and_can_be_cleared() {
        let root_dir = temp_projects_root("hidden-from-chats-default");
        let root = root_dir.path();
        assert!(!load_session_hidden_from_chats(root, "/tmp/repo", "sess-h"));

        save_session_hidden_from_chats(root, "/tmp/repo", "sess-h", true).expect("hide session");
        save_session_hidden_from_chats(root, "/tmp/repo", "sess-h", false).expect("show session");

        assert!(!load_session_hidden_from_chats(root, "/tmp/repo", "sess-h"));
    }

    #[test]
    fn session_mode_roundtrip_preserves_existing_title() {
        let root_dir = temp_projects_root("mode-roundtrip");
        let root = root_dir.path();
        save_session_title(root, "/tmp/repo", "sess-m", "Title").expect("save title");
        save_session_mode(root, "/tmp/repo", "sess-m", "coordinator").expect("save mode");
        assert_eq!(
            load_session_title(root, "/tmp/repo", "sess-m").as_deref(),
            Some("Title")
        );
        assert_eq!(
            load_session_mode(root, "/tmp/repo", "sess-m").as_deref(),
            Some("coordinator")
        );
    }

    #[test]
    fn plan_entered_from_roundtrips_and_forgets_without_touching_the_rest() {
        let root_dir = temp_projects_root("plan-origin-roundtrip");
        let root = root_dir.path();
        assert_eq!(
            load_session_plan_entered_from(root, "/tmp/repo", "sess-p"),
            None
        );
        save_session_title(root, "/tmp/repo", "sess-p", "Title").expect("save title");

        save_session_plan_entered_from(root, "/tmp/repo", "sess-p", Some("auto"))
            .expect("save origin");
        assert_eq!(
            load_session_plan_entered_from(root, "/tmp/repo", "sess-p").as_deref(),
            Some("auto")
        );

        save_session_plan_entered_from(root, "/tmp/repo", "sess-p", None).expect("forget origin");
        assert_eq!(
            load_session_plan_entered_from(root, "/tmp/repo", "sess-p"),
            None
        );
        assert_eq!(
            load_session_title(root, "/tmp/repo", "sess-p").as_deref(),
            Some("Title")
        );
    }

    #[test]
    fn load_session_title_missing_file_returns_none() {
        let root_dir = temp_projects_root("meta-missing");
        let root = root_dir.path();
        let got = load_session_title(root, "/tmp/repo", "never-saved");
        assert!(got.is_none());
    }

    #[test]
    fn session_agent_roundtrip_keeps_the_other_metadata() {
        let root_dir = temp_projects_root("agent-roundtrip");
        let root = root_dir.path();
        save_session_title(root, "/tmp/repo", "sess-a", "Title").expect("save title");
        save_session_mode(root, "/tmp/repo", "sess-a", "normal").expect("save mode");
        save_session_agent(root, "/tmp/repo", "sess-a", "claude-code").expect("save agent");
        save_agent_session_id(root, "/tmp/repo", "sess-a", "agent-sess-1").expect("save agent sid");

        assert_eq!(
            load_session_agent(root, "/tmp/repo", "sess-a").as_deref(),
            Some("claude-code")
        );
        assert_eq!(
            load_agent_session_id(root, "/tmp/repo", "sess-a").as_deref(),
            Some("agent-sess-1")
        );
        // The sidecar is rewritten wholesale on every save, so the
        // fields written earlier are the ones most at risk.
        assert_eq!(
            load_session_title(root, "/tmp/repo", "sess-a").as_deref(),
            Some("Title")
        );
        assert_eq!(
            load_session_mode(root, "/tmp/repo", "sess-a").as_deref(),
            Some("normal")
        );
    }

    #[test]
    fn never_choosing_an_agent_is_distinct_from_choosing_local() {
        // Absent means "follow the configured default"; an explicit
        // `local` means the user switched back and must stay there.
        let root_dir = temp_projects_root("agent-unset");
        let root = root_dir.path();
        assert!(load_session_agent(root, "/tmp/repo", "sess-u").is_none());
        save_session_agent(root, "/tmp/repo", "sess-u", SESSION_AGENT_LOCAL).expect("save");
        assert_eq!(
            load_session_agent(root, "/tmp/repo", "sess-u").as_deref(),
            Some("local")
        );
    }

    #[test]
    fn switching_agents_drops_the_previous_agents_session_id() {
        // The stored id belongs to the agent being left behind; handing
        // it to the next agent would ask it to load a session it has
        // never seen.
        let root_dir = temp_projects_root("agent-switch");
        let root = root_dir.path();
        save_session_agent(root, "/tmp/repo", "sess-s", "claude-code").unwrap();
        save_agent_session_id(root, "/tmp/repo", "sess-s", "agent-sess-1").unwrap();
        save_session_agent(root, "/tmp/repo", "sess-s", "gemini").unwrap();
        assert!(load_agent_session_id(root, "/tmp/repo", "sess-s").is_none());

        // Re-saving the same agent is not a switch and keeps the id.
        save_agent_session_id(root, "/tmp/repo", "sess-s", "agent-sess-2").unwrap();
        save_session_agent(root, "/tmp/repo", "sess-s", "gemini").unwrap();
        assert_eq!(
            load_agent_session_id(root, "/tmp/repo", "sess-s").as_deref(),
            Some("agent-sess-2")
        );
    }

    #[test]
    fn blank_agent_ids_are_rejected_rather_than_stored() {
        let root_dir = temp_projects_root("agent-blank");
        let root = root_dir.path();
        assert!(save_session_agent(root, "/tmp/repo", "sess-b", "  ").is_err());
        assert!(save_agent_session_id(root, "/tmp/repo", "sess-b", "").is_err());
        assert!(load_session_agent(root, "/tmp/repo", "sess-b").is_none());
    }

    #[test]
    fn load_session_title_empty_trimmed_returns_none() {
        // A sidecar whose title is all-whitespace is treated as
        // "no cached title" so the resume dialog falls through to
        // the next fallback layer instead of showing a blank row.
        let root_dir = temp_projects_root("meta-blank");
        let root = root_dir.path();
        save_session_title(root, "/tmp/repo", "sess-blank", "   \t\n  ").expect("save");
        let got = load_session_title(root, "/tmp/repo", "sess-blank");
        assert!(got.is_none(), "whitespace-only title must be ignored");
    }

    #[test]
    fn save_session_title_overwrites_previous_content() {
        // Titles can be regenerated when the topic shifts — every
        // save must replace the previous content, not append.
        let root_dir = temp_projects_root("meta-overwrite");
        let root = root_dir.path();
        save_session_title(root, "/tmp/repo", "sess-o", "First title").unwrap();
        save_session_title(root, "/tmp/repo", "sess-o", "Second title").unwrap();
        let got = load_session_title(root, "/tmp/repo", "sess-o");
        assert_eq!(got.as_deref(), Some("Second title"));
    }

    #[test]
    fn load_session_title_corrupt_json_returns_none() {
        // A half-written sidecar (crash during save) must not
        // fail the resume dialog.
        let root_dir = temp_projects_root("meta-corrupt");
        let root = root_dir.path();
        let dir = project_dir_path(root, "/tmp/repo");
        std::fs::create_dir_all(&dir).unwrap();
        let path = session_meta_path(root, "/tmp/repo", "sess-c");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let got = load_session_title(root, "/tmp/repo", "sess-c");
        assert!(got.is_none());
    }

    // ── extract_first_user_message_text ────────────────────────────────

    #[test]
    fn extract_first_user_message_text_handles_string_content() {
        let root_dir = temp_projects_root("extract-string");
        let root = root_dir.path();
        append_transcript_entry(
            root,
            "/tmp/repo",
            "sess-fs",
            TranscriptWriteEntry::new(
                "user",
                json!({"message": {"role": "user", "content": "hello world"}}),
            )
            .with_uuid("u1")
            .with_timestamp("2026-04-09T00:00:00.000Z"),
        )
        .unwrap();
        let got = extract_first_user_message_text(root, "/tmp/repo", "sess-fs");
        assert_eq!(got.as_deref(), Some("hello world"));
    }

    #[test]
    fn extract_first_user_message_text_handles_array_of_text_blocks() {
        let root_dir = temp_projects_root("extract-array");
        let root = root_dir.path();
        append_transcript_entry(
            root,
            "/tmp/repo",
            "sess-fa",
            TranscriptWriteEntry::new(
                "user",
                json!({
                    "message": {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "first"},
                            {"type": "text", "text": "second"}
                        ]
                    }
                }),
            )
            .with_uuid("u1")
            .with_timestamp("2026-04-09T00:00:00.000Z"),
        )
        .unwrap();
        let got = extract_first_user_message_text(root, "/tmp/repo", "sess-fa");
        assert_eq!(got.as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn extract_first_user_message_text_skips_tool_result_wrapper() {
        // A user entry whose content is a `tool_result` block (no text)
        // must be skipped — the dialog wants the *prompt* the user
        // typed, not a synthetic tool-response envelope.
        let root_dir = temp_projects_root("extract-skip");
        let root = root_dir.path();
        let entries = vec![
            TranscriptWriteEntry::new(
                "user",
                json!({
                    "message": {
                        "role": "user",
                        "content": [
                            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                        ]
                    }
                }),
            )
            .with_uuid("u1")
            .with_timestamp("2026-04-09T00:00:00.000Z"),
            TranscriptWriteEntry::new(
                "user",
                json!({"message": {"role": "user", "content": "real prompt"}}),
            )
            .with_uuid("u2")
            .with_parent("u1")
            .with_timestamp("2026-04-09T00:00:01.000Z"),
        ];
        write_transcript_entries(root, "/tmp/repo", "sess-skip", entries).unwrap();
        let got = extract_first_user_message_text(root, "/tmp/repo", "sess-skip");
        assert_eq!(got.as_deref(), Some("real prompt"));
    }

    #[test]
    fn extract_first_user_message_text_missing_file_returns_none() {
        let root_dir = temp_projects_root("extract-missing");
        let root = root_dir.path();
        let got = extract_first_user_message_text(root, "/tmp/repo", "nope");
        assert!(got.is_none());
    }

    #[test]
    fn extract_first_user_message_text_ignores_assistant_entries() {
        // Belt-and-suspenders: an assistant-first transcript (edge case
        // from a replay or a manual fixture) must not return the
        // assistant's text as the "first user message".
        let root_dir = temp_projects_root("extract-assistant-first");
        let root = root_dir.path();
        let entries = vec![
            TranscriptWriteEntry::new(
                "assistant",
                json!({"message": {"role": "assistant", "content": "proactive output"}}),
            )
            .with_uuid("a1")
            .with_timestamp("2026-04-09T00:00:00.000Z"),
            TranscriptWriteEntry::new(
                "user",
                json!({"message": {"role": "user", "content": "user finally speaks"}}),
            )
            .with_uuid("u1")
            .with_parent("a1")
            .with_timestamp("2026-04-09T00:00:01.000Z"),
        ];
        write_transcript_entries(root, "/tmp/repo", "sess-af", entries).unwrap();
        let got = extract_first_user_message_text(root, "/tmp/repo", "sess-af");
        assert_eq!(got.as_deref(), Some("user finally speaks"));
    }

    #[test]
    fn move_session_to_cwd_relocates_transcript_and_metadata() {
        let root_dir = temp_projects_root("move-session");
        let root = root_dir.path();
        let source_cwd = "/tmp/repo/.rebon/worktrees/old";
        let target_cwd = "/tmp/repo";
        let session_id = "sess-move";
        let source = ensure_session_file_path(root, source_cwd, session_id).unwrap();
        std::fs::write(&source, b"transcript\n").unwrap();
        let target_path = ensure_session_file_path(root, target_cwd, session_id).unwrap();
        std::fs::write(transcript_append_generation_path(&source), b"source proof").unwrap();
        std::fs::write(
            transcript_append_generation_path(&target_path),
            b"target proof",
        )
        .unwrap();
        let source_meta = session_meta_path(root, source_cwd, session_id);
        std::fs::write(&source_meta, b"{\"title\":\"Moved\"}").unwrap();

        let target = move_session_to_cwd(root, source_cwd, target_cwd, session_id).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"transcript\n");
        assert_eq!(
            std::fs::read(session_meta_path(root, target_cwd, session_id)).unwrap(),
            b"{\"title\":\"Moved\"}"
        );
        assert!(!source.exists());
        assert!(!source_meta.exists());
        assert!(!transcript_append_generation_path(&source).exists());
        assert!(!transcript_append_generation_path(&target).exists());
    }

    #[test]
    fn copy_session_to_cwd_invalidates_only_target_append_proof() {
        let root_dir = temp_projects_root("copy-session-proof");
        let root = root_dir.path();
        let source_cwd = "/tmp/repo/source";
        let target_cwd = "/tmp/repo/target";
        let session_id = "sess-copy";
        let source = ensure_session_file_path(root, source_cwd, session_id).unwrap();
        append_jsonl_lines(&source, "{\"type\":\"user\"}\n").unwrap();
        let source_generation = load_transcript_append_generation(&source).unwrap();
        let target = ensure_session_file_path(root, target_cwd, session_id).unwrap();
        std::fs::write(
            transcript_append_generation_path(&target),
            b"stale target proof",
        )
        .unwrap();

        let copied = copy_session_to_cwd(root, source_cwd, target_cwd, session_id).unwrap();

        assert_eq!(copied, target);
        assert_eq!(
            std::fs::read(&copied).unwrap(),
            std::fs::read(&source).unwrap()
        );
        assert_eq!(
            load_transcript_append_generation(&source),
            Some(source_generation)
        );
        assert!(!transcript_append_generation_path(&copied).exists());
    }

    #[test]
    fn move_session_to_cwd_restores_metadata_when_transcript_move_fails() {
        let root_dir = temp_projects_root("move-session-rollback");
        let root = root_dir.path();
        let source_cwd = "/tmp/repo/.rebon/worktrees/old";
        let target_cwd = "/tmp/repo";
        let session_id = "sess-move-rollback";
        let source = ensure_session_file_path(root, source_cwd, session_id).unwrap();
        std::fs::write(&source, b"transcript\n").unwrap();
        let source_meta = session_meta_path(root, source_cwd, session_id);
        std::fs::write(&source_meta, b"{\"title\":\"Still source\"}").unwrap();
        let target = ensure_session_file_path(root, target_cwd, session_id).unwrap();
        std::fs::create_dir(&target).unwrap();

        assert!(move_session_to_cwd(root, source_cwd, target_cwd, session_id).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"transcript\n");
        assert_eq!(
            std::fs::read(&source_meta).unwrap(),
            b"{\"title\":\"Still source\"}"
        );
        assert!(!session_meta_path(root, target_cwd, session_id).exists());
    }

    #[test]
    fn ultraplan_run_storage_save_load_list_latest_and_ignores_tmp() {
        let root_dir = temp_projects_root("ultraplan-runs");
        let root = root_dir.path();
        let cwd = "/tmp/repo";
        let mut old = UltraplanRunState::new(
            "ultraplan-1-sess1234-0".into(),
            "sess1234".into(),
            "old task".into(),
            None,
            100,
        );
        old.updated_at_ms = 150;
        let mut latest = UltraplanRunState::new(
            "ultraplan-2-sess1234-1".into(),
            "sess1234".into(),
            "latest task".into(),
            None,
            200,
        );
        latest.phase = rebon_types::RunPhase::Researching;
        latest.updated_at_ms = 250;
        let mut done = UltraplanRunState::new(
            "ultraplan-3-sess1234-2".into(),
            "sess1234".into(),
            "done task".into(),
            None,
            300,
        );
        done.phase = rebon_types::RunPhase::Done;
        done.updated_at_ms = 350;
        old.prepare_for_persist();
        latest.prepare_for_persist();
        done.prepare_for_persist();
        old.updated_at_ms = 150;
        latest.updated_at_ms = 250;
        done.updated_at_ms = 350;
        old.prepare_for_persist();
        latest.prepare_for_persist();
        done.prepare_for_persist();

        save_ultraplan_run(root, cwd, &old).unwrap();
        save_ultraplan_run(root, cwd, &latest).unwrap();
        save_ultraplan_run(root, cwd, &done).unwrap();
        std::fs::write(
            ultraplan_run_dir_path(root, cwd).join("ultraplan-partial.json.tmp.1"),
            b"not json",
        )
        .unwrap();

        assert_eq!(
            load_ultraplan_run(root, cwd, &latest.run_id).unwrap(),
            latest
        );
        let listed = list_ultraplan_runs(root, cwd);
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].run_id, done.run_id);
        assert_eq!(listed[1].run_id, latest.run_id);
        assert_eq!(listed[2].run_id, old.run_id);

        let active = latest_active_run_for_session(root, cwd, "sess1234").unwrap();
        assert_eq!(active.run_id, latest.run_id);
    }

    #[test]
    fn ultraplan_run_storage_cas_rejects_stale_revisions() {
        let root_dir = temp_projects_root("ultraplan-cas");
        let root = root_dir.path();
        let cwd = "/tmp/repo";
        let mut state = UltraplanRunState::new(
            "ultraplan-cas-run".into(),
            "session".into(),
            "task".into(),
            None,
            100,
        );

        save_ultraplan_run_cas(root, cwd, 0, &state).unwrap();
        let stale = save_ultraplan_run_cas(root, cwd, 0, &state).unwrap_err();
        assert!(matches!(
            stale,
            UltraplanRunStoreError::StaleRevision {
                expected: 0,
                actual: 1
            }
        ));

        state.replace_requirements(vec![rebon_types::RequirementLedgerEntry {
            id: "R1".into(),
            title: "Requirement".into(),
            source: rebon_types::RequirementSource::Question,
            round_added: 1,
        }]);
        save_ultraplan_run_cas(root, cwd, 1, &state).unwrap();

        let loaded = load_ultraplan_run(root, cwd, &state.run_id).unwrap();
        assert_eq!(loaded.ledger_revision, 2);
        assert_eq!(loaded.requirement_ledger[0].id, "R1");
    }

    #[test]
    fn ultraplan_run_unknown_version_loads_as_none() {
        let root_dir = temp_projects_root("ultraplan-version");
        let root = root_dir.path();
        let cwd = "/tmp/repo";
        std::fs::create_dir_all(ultraplan_run_dir_path(root, cwd)).unwrap();
        std::fs::write(
            ultraplan_run_path(root, cwd, "ultraplan-future"),
            br#"{"version":999,"run_id":"ultraplan-future"}"#,
        )
        .unwrap();

        assert!(load_ultraplan_run(root, cwd, "ultraplan-future").is_none());
        assert!(list_ultraplan_runs(root, cwd).is_empty());
    }

    #[test]
    fn ultraplan_run_load_rejects_path_identity_mismatch() {
        let root_dir = temp_projects_root("ultraplan-identity");
        let root = root_dir.path();
        let cwd = "/tmp/repo";
        let state = UltraplanRunState::new(
            "ultraplan-source".into(),
            "session".into(),
            "task".into(),
            None,
            100,
        );
        save_ultraplan_run(root, cwd, &state).unwrap();
        let bytes = std::fs::read(ultraplan_run_path(root, cwd, &state.run_id)).unwrap();
        std::fs::write(ultraplan_run_path(root, cwd, "ultraplan-other"), bytes).unwrap();

        assert!(load_ultraplan_run(root, cwd, "ultraplan-other").is_none());
        let listed = list_ultraplan_runs(root, cwd);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].run_id, state.run_id);
    }

    #[test]
    fn session_history_uses_canonical_uuid_boundaries_and_marks_incomplete_turn() {
        let root_dir = temp_projects_root("history-projection");
        let root = root_dir.path();
        let path = root.join("history.jsonl");
        let lines = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2025-01-01T00:00:00Z","message":{"content":"same prompt"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2025-01-01T00:00:01Z","message":{"content":"done"}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2025-01-01T00:00:02Z","message":{"content":"same prompt"}}),
        ];
        let body = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();

        let history = load_session_history(&path).unwrap();
        assert_eq!(history.source_head_uuid, "u2");
        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.turns[0].user_uuid, "u1");
        assert_eq!(history.turns[0].parent_uuid, None);
        assert_eq!(
            history.turns[0].completion_state,
            HistoryTurnCompletion::Complete
        );
        assert_eq!(history.turns[1].user_uuid, "u2");
        assert_eq!(history.turns[1].parent_uuid.as_deref(), Some("a1"));
        assert_eq!(
            history.turns[1].completion_state,
            HistoryTurnCompletion::Incomplete
        );
    }

    fn write_jsonl(path: &Path, lines: &[Value]) {
        let body = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn last_turn_text_spans_tool_rounds_and_skips_non_text_blocks() {
        let root_dir = temp_projects_root("last-turn-text");
        let path = root_dir.path().join("turns.jsonl");
        write_jsonl(
            &path,
            &[
                json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"first task"}}),
                json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[{"type":"text","text":"old answer"}]}}),
                json!({"type":"user","uuid":"u2","parentUuid":"a1","message":{"content":"second task"}}),
                json!({"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"content":[
                    {"type":"thinking","thinking":"hidden"},
                    {"type":"text","text":"Looking at the file."},
                    {"type":"tool_use","id":"t1","name":"Read","input":{}}
                ]}}),
                json!({"type":"user","uuid":"r1","parentUuid":"a2","message":{"content":[
                    {"type":"tool_result","tool_use_id":"t1","content":"file body"}
                ]}}),
                json!({"type":"assistant","uuid":"a3","parentUuid":"r1","message":{"content":[{"type":"text","text":"Done: it works."}]}}),
            ],
        );

        let text = last_turn_assistant_text(&path).unwrap();
        assert_eq!(
            text.as_deref(),
            Some("Looking at the file.\n\nDone: it works."),
            "a tool result continues the turn; the old turn's answer is not in it"
        );
        let history = load_session_history(&path).unwrap();
        assert_eq!(
            history.turns.last().map(|turn| turn.user_uuid.as_str()),
            Some("u2"),
            "both readers agree on where the last turn starts"
        );
    }

    #[test]
    fn last_turn_text_is_none_without_a_transcript_a_turn_or_any_text() {
        let root_dir = temp_projects_root("last-turn-none");
        let root = root_dir.path();

        assert_eq!(
            last_turn_assistant_text(&root.join("missing.jsonl")).unwrap(),
            None,
            "a job that never wrote a transcript has no result yet"
        );

        let unanswered = root.join("unanswered.jsonl");
        write_jsonl(
            &unanswered,
            &[json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"task"}})],
        );
        assert_eq!(last_turn_assistant_text(&unanswered).unwrap(), None);

        let tool_only = root.join("tool-only.jsonl");
        write_jsonl(
            &tool_only,
            &[
                json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"task"}}),
                json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[
                    {"type":"tool_use","id":"t1","name":"Bash","input":{}}
                ]}}),
            ],
        );
        assert_eq!(
            last_turn_assistant_text(&tool_only).unwrap(),
            None,
            "a turn that ended on a tool call said nothing"
        );
    }

    /// A path past `MAX_PATH` is not exotic here: a project directory is named
    /// after the whole cwd, and a rewind artifact's name is 85 characters
    /// longer than the transcript's. The hand-written Win32 move needs the
    /// `\\?\` prefix `std::fs` would have added on its behalf; without it the
    /// call answers `ERROR_PATH_NOT_FOUND`.
    #[test]
    fn replace_file_atomically_moves_a_file_past_max_path() {
        let root_dir = temp_projects_root("atomic-replace-long");
        let mut deep = root_dir.path().to_path_buf();
        while deep.as_os_str().len() < 240 {
            deep = deep.join("d".repeat(60));
        }
        std::fs::create_dir_all(&deep).unwrap();
        let destination = deep.join("transcript.jsonl");
        let source = deep.join("transcript.jsonl.next");
        assert!(
            destination.as_os_str().len() > 260,
            "the regression only exists past MAX_PATH; this path is {} long",
            destination.as_os_str().len()
        );
        std::fs::write(&source, b"replacement").unwrap();
        std::fs::write(&destination, b"original").unwrap();

        replace_file_atomically(&source, &destination).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"replacement");
        assert!(!source.exists(), "a move consumes its source");
    }

    /// The prefix goes on only where it is needed. Every session in an ordinary
    /// directory takes this path, so it is the one that must not regress.
    #[test]
    fn replace_file_atomically_still_replaces_a_short_path() {
        let root_dir = temp_projects_root("atomic-replace-short");
        let destination = root_dir.path().join("transcript.jsonl");
        let source = root_dir.path().join("transcript.jsonl.next");
        std::fs::write(&source, b"replacement").unwrap();
        std::fs::write(&destination, b"original").unwrap();

        replace_file_atomically(&source, &destination).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"replacement");
        assert!(!source.exists());
    }

    /// What the flaky journal test was reporting: a sidecar is replaced a
    /// few milliseconds after it was published, and every so often a scanner
    /// still had that published file open. Rebon's own readers open it the
    /// same way — `File::open` shares reads, writes and deletes — and the
    /// superseding rename refuses them all with `ERROR_ACCESS_DENIED`. The
    /// POSIX-semantics rename gets past such a handle, which keeps the old
    /// file until it closes.
    #[cfg(windows)]
    #[test]
    fn replace_file_atomically_gets_past_a_reader_holding_the_destination_open() {
        use std::io::Read as _;

        let root_dir = temp_projects_root("atomic-replace-held-open");
        let destination = root_dir.path().join("sidecar.json");
        let source = root_dir.path().join("sidecar.json.tmp");
        std::fs::write(&destination, b"published").unwrap();
        std::fs::write(&source, b"next").unwrap();
        let mut reader = File::open(&destination).unwrap();

        let mut waits = 0usize;
        replace_file_atomically_with(&source, &destination, |_| waits += 1).unwrap();

        assert_eq!(waits, 0, "a shared handle is not something to wait for");
        assert_eq!(std::fs::read(&destination).unwrap(), b"next");
        assert!(!source.exists());
        let mut still_readable = String::new();
        reader.read_to_string(&mut still_readable).unwrap();
        assert_eq!(still_readable, "published", "the reader keeps its file");
    }

    /// A handle without delete sharing cannot be renamed past, and there is
    /// nothing to do but wait for it: the replace retries, and the moment the
    /// handle closes it goes through.
    #[cfg(windows)]
    #[test]
    fn replace_file_atomically_waits_out_a_reader_that_shares_nothing() {
        use std::os::windows::fs::OpenOptionsExt;

        let root_dir = temp_projects_root("atomic-replace-exclusive");
        let destination = root_dir.path().join("sidecar.json");
        let source = root_dir.path().join("sidecar.json.tmp");
        std::fs::write(&destination, b"published").unwrap();
        std::fs::write(&source, b"next").unwrap();
        let mut exclusive = Some(
            OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&destination)
                .unwrap(),
        );

        let mut waits = Vec::new();
        replace_file_atomically_with(&source, &destination, |delay| {
            waits.push(delay);
            // The scanner finishes while the replace is waiting on it.
            exclusive.take();
        })
        .unwrap();

        assert_eq!(waits, vec![std::time::Duration::from_millis(1)]);
        assert_eq!(std::fs::read(&destination).unwrap(), b"next");
        assert!(!source.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_replace_retries_transient_failures() {
        let errors = [
            ERROR_ACCESS_DENIED,
            ERROR_SHARING_VIOLATION,
            ERROR_LOCK_VIOLATION,
        ];
        let mut attempts = 0usize;
        let mut waits = Vec::new();
        retry_windows_atomic_replace(
            || {
                let result = errors
                    .get(attempts)
                    .map(|code| Err(std::io::Error::from_raw_os_error(*code)))
                    .unwrap_or(Ok(()));
                attempts += 1;
                result
            },
            |delay| waits.push(delay),
        )
        .unwrap();

        assert_eq!(attempts, 4);
        assert_eq!(
            waits,
            vec![
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(2),
                std::time::Duration::from_millis(4)
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_replace_does_not_retry_permanent_failures() {
        let mut attempts = 0usize;
        let mut waits = 0usize;
        let error = retry_windows_atomic_replace(
            || {
                attempts += 1;
                Err(std::io::Error::from_raw_os_error(2))
            },
            |_| waits += 1,
        )
        .unwrap_err();

        assert_eq!(attempts, 1);
        assert_eq!(waits, 0);
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_replace_bounds_transient_retries() {
        let mut attempts = 0usize;
        let mut waits = Vec::new();
        let error = retry_windows_atomic_replace(
            || {
                attempts += 1;
                Err(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED))
            },
            |delay| waits.push(delay),
        )
        .unwrap_err();

        assert_eq!(attempts, WINDOWS_ATOMIC_REPLACE_ATTEMPTS);
        assert_eq!(waits.len(), WINDOWS_ATOMIC_REPLACE_ATTEMPTS - 1);
        assert_eq!(
            waits.iter().sum::<std::time::Duration>(),
            std::time::Duration::from_millis(1 + 2 + 4 + 8 + 16 + 32 * 4),
            "the whole wait is about a sixth of a second"
        );
        assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED));
    }

    fn staging_files_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "tmp"))
            .collect()
    }

    #[test]
    fn write_file_atomically_publishes_the_whole_file_and_leaves_no_staging_file() {
        let root_dir = temp_projects_root("atomic-write-new");
        let destination = root_dir.path().join("sidecar.json");

        write_file_atomically(&destination, b"{\"whole\":true}").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"{\"whole\":true}");
        assert_eq!(staging_files_in(root_dir.path()), Vec::<PathBuf>::new());
        let entries = std::fs::read_dir(root_dir.path()).unwrap().count();
        assert_eq!(entries, 1, "only the published file is left behind");
    }

    #[test]
    fn write_file_atomically_replaces_an_existing_file() {
        let root_dir = temp_projects_root("atomic-write-replace");
        let destination = root_dir.path().join("sidecar.json");
        std::fs::write(&destination, b"previous, and longer than the next").unwrap();

        write_file_atomically(&destination, b"next").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"next");
        assert_eq!(staging_files_in(root_dir.path()), Vec::<PathBuf>::new());
    }

    /// The same regression [`replace_file_atomically`] guards: a foreground
    /// status file is keyed by cwd into the project directory, so a deep
    /// workspace writes past `MAX_PATH`.
    #[test]
    fn write_file_atomically_writes_past_max_path() {
        let root_dir = temp_projects_root("atomic-write-long");
        let mut deep = root_dir.path().to_path_buf();
        while deep.as_os_str().len() < 240 {
            deep = deep.join("d".repeat(60));
        }
        std::fs::create_dir_all(&deep).unwrap();
        let destination = deep.join("state.json");
        assert!(destination.as_os_str().len() > 260);
        std::fs::write(&destination, b"previous").unwrap();

        write_file_atomically(&destination, b"next").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"next");
        assert_eq!(staging_files_in(&deep), Vec::<PathBuf>::new());
    }

    /// A failed publish must not leave the caller with a half-written file
    /// or a stray staging file — only the previous contents.
    #[test]
    fn write_file_atomically_cleans_up_when_the_destination_cannot_be_replaced() {
        let root_dir = temp_projects_root("atomic-write-blocked");
        let destination = root_dir.path().join("sidecar.json");
        std::fs::create_dir_all(&destination).unwrap();

        write_file_atomically(&destination, b"next").unwrap_err();

        assert!(destination.is_dir(), "the obstacle is left as it was");
        assert_eq!(staging_files_in(root_dir.path()), Vec::<PathBuf>::new());
    }

    /// The parent directory is the caller's responsibility, and its absence
    /// is reported as the ordinary not-found error.
    #[test]
    fn write_file_atomically_refuses_a_missing_parent() {
        let root_dir = temp_projects_root("atomic-write-no-parent");
        let destination = root_dir.path().join("missing").join("sidecar.json");

        let error = write_file_atomically(&destination, b"next").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!destination.exists());
    }

    /// Windows: the file being replaced was published a moment ago, and a
    /// scanner or one of Rebon's own readers may still have it open. The
    /// staged write goes past such a handle, and the reader keeps the file
    /// it opened.
    #[cfg(windows)]
    #[test]
    fn write_file_atomically_gets_past_a_reader_holding_the_destination_open() {
        use std::io::Read as _;

        let root_dir = temp_projects_root("atomic-write-held-open");
        let destination = root_dir.path().join("sidecar.json");
        std::fs::write(&destination, b"published").unwrap();
        let mut reader = File::open(&destination).unwrap();

        write_file_atomically(&destination, b"next").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"next");
        assert_eq!(staging_files_in(root_dir.path()), Vec::<PathBuf>::new());
        let mut still_readable = String::new();
        reader.read_to_string(&mut still_readable).unwrap();
        assert_eq!(still_readable, "published");
    }

    /// Windows: the staging file is closed before it is moved. Had it still
    /// been open for writing, the move would refuse with a sharing violation
    /// and the write would report failure after having done all the work.
    #[cfg(windows)]
    #[test]
    fn write_file_atomically_publishes_while_the_previous_file_is_open_for_writing() {
        let root_dir = temp_projects_root("atomic-write-writer-open");
        let destination = root_dir.path().join("sidecar.json");
        std::fs::write(&destination, b"published").unwrap();
        let _writer = OpenOptions::new().append(true).open(&destination).unwrap();

        write_file_atomically(&destination, b"next").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"next");
    }

    #[cfg(unix)]
    #[test]
    fn write_private_file_atomically_publishes_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let root_dir = temp_projects_root("atomic-write-private");
        let destination = root_dir.path().join("owner.json");

        write_private_file_atomically(&destination, b"{\"token\":\"secret\"}").unwrap();

        let mode = std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"{\"token\":\"secret\"}"
        );
    }

    /// Two writers in one process must not share a staging file, or the
    /// second `create_new` open fails on the first one's file.
    #[test]
    fn staging_paths_beside_the_same_file_differ() {
        let destination = Path::new("/somewhere/state.json");
        let first = staging_path_beside(destination).unwrap();
        let second = staging_path_beside(destination).unwrap();

        assert_ne!(first, second);
        assert_eq!(first.parent(), destination.parent());
        assert_eq!(first.extension().unwrap(), "tmp");
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".state.json."));
    }

    #[test]
    fn a_path_with_no_directory_cannot_be_staged() {
        let error = staging_path_beside(Path::new("/")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// The other half of `MOVEFILE_REPLACE_EXISTING`: a journal's first write
    /// has nothing to replace.
    #[test]
    fn replace_file_atomically_accepts_a_destination_that_is_not_there_yet() {
        let root_dir = temp_projects_root("atomic-replace-new");
        let mut deep = root_dir.path().to_path_buf();
        while deep.as_os_str().len() < 240 {
            deep = deep.join("d".repeat(60));
        }
        std::fs::create_dir_all(&deep).unwrap();
        let destination = deep.join("journal.json");
        let source = deep.join("journal.json.next");
        std::fs::write(&source, b"journal").unwrap();

        replace_file_atomically(&source, &destination).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"journal");
    }

    /// What the field report looked like: the rewind wrote its journal through
    /// `std::fs` (which prefixed the path) and then could not move it into
    /// place, so a session opened from a deep enough directory could not be
    /// rewound at all — it failed with `transcript I/O failed: … (os error 3)`.
    #[test]
    fn a_session_in_a_deep_project_directory_can_still_be_rewound() {
        let root_dir = temp_projects_root("rewind-deep");
        let root = root_dir.path();
        let cwd = format!("/workspace/{}", "d".repeat(180));
        let path = ensure_session_file_path(root, &cwd, "session-deep").unwrap();
        assert!(
            path.as_os_str().len() + 85 > 260,
            "the rewind artifacts have to land past MAX_PATH for this to prove anything; \
             the transcript path is {} long",
            path.as_os_str().len()
        );
        let lines = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2025-01-01T00:00:00Z","message":{"content":"first"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2025-01-01T00:00:01Z","message":{"content":"answer"}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2025-01-01T00:00:02Z","message":{"content":"second"}}),
            json!({"type":"assistant","uuid":"a2","parentUuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"content":"later"}}),
        ];
        let body = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).unwrap();
        let displayed = load_session_history(&path).unwrap();
        let request = RewindConversationRequest {
            mutation_id: "mutation-deep".into(),
            session_id: "session-deep".into(),
            cwd: cwd.clone(),
            selected_user_uuid: "u2".into(),
            boundary_parent_uuid: Some("a1".into()),
            selected_prompt: "second".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };

        let outcome = rewind_conversation(root, &path, &request).unwrap();

        assert_eq!(outcome.new_source_head_uuid, "a1");
        assert_eq!(outcome.canonical_history.turns.len(), 1);
        assert_eq!(outcome.prefill_prompt, "second");
        assert!(outcome.recovery_backup.exists());
    }

    #[cfg(windows)]
    fn win32_text(path: &str) -> String {
        let wide = wide_path_for_win32(Path::new(path));
        assert_eq!(
            wide.last().copied(),
            Some(0),
            "the wide string stays NUL-terminated"
        );
        String::from_utf16(&wide[..wide.len() - 1]).unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn a_long_disk_path_reaches_win32_with_the_verbatim_prefix() {
        let long = format!(r"C:\{}\transcript.jsonl", "d".repeat(300));
        assert_eq!(win32_text(&long), format!(r"\\?\{long}"));
    }

    #[cfg(windows)]
    #[test]
    fn a_short_path_reaches_win32_untouched() {
        let short = r"C:\projects\workspace\transcript.jsonl";
        assert_eq!(win32_text(short), short);
    }

    #[cfg(windows)]
    #[test]
    fn a_path_that_is_already_verbatim_is_not_prefixed_twice() {
        let long = format!(r"\\?\C:\{}\transcript.jsonl", "d".repeat(300));
        assert_eq!(win32_text(&long), long);
    }

    #[cfg(windows)]
    #[test]
    fn a_long_unc_path_becomes_a_verbatim_unc_path() {
        let long = format!(r"\\server\share\{}\transcript.jsonl", "d".repeat(300));
        assert_eq!(
            win32_text(&long),
            format!(r"\\?\UNC\server\share\{}\transcript.jsonl", "d".repeat(300))
        );
    }

    /// `\\?\` switches off the parsing that resolves `..`, so a path that still
    /// needs it is handed over as it is rather than being pointed somewhere
    /// else.
    #[cfg(windows)]
    #[test]
    fn a_long_path_with_a_parent_component_is_left_alone() {
        let long = format!(r"C:\{}\..\transcript.jsonl", "d".repeat(300));
        assert_eq!(win32_text(&long), long);
    }

    /// A verbatim path is taken literally, so the separators have to be settled
    /// and `.` dropped before the prefix goes on — otherwise the kernel looks
    /// for a directory actually named `.`.
    #[cfg(windows)]
    #[test]
    fn separators_and_dot_components_are_settled_before_prefixing() {
        let long = format!("C:/{}/./transcript.jsonl", "d".repeat(300));
        assert_eq!(
            win32_text(&long),
            format!(r"\\?\C:\{}\transcript.jsonl", "d".repeat(300))
        );
    }

    #[test]
    fn rewind_excludes_selected_uuid_and_later_entries_and_is_idempotent() {
        let root_dir = temp_projects_root("rewind-conversation");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
        let lines = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2025-01-01T00:00:00Z","message":{"content":"duplicate"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2025-01-01T00:00:01Z","message":{"content":"first"}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2025-01-01T00:00:02Z","message":{"content":"duplicate"}}),
            json!({"type":"assistant","uuid":"a2","parentUuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"content":"second"}}),
        ];
        let body = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).unwrap();
        let file = File::open(&path).unwrap();
        publish_transcript_append_generation(&path, &file, None, body.as_bytes()).unwrap();
        drop(file);
        assert!(load_transcript_append_generation(&path).is_some());
        let displayed = load_session_history(&path).unwrap();
        let request = RewindConversationRequest {
            mutation_id: "mutation-1".into(),
            session_id: "session-1".into(),
            cwd: "/workspace".into(),
            selected_user_uuid: "u2".into(),
            boundary_parent_uuid: Some("a1".into()),
            selected_prompt: "duplicate".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };

        let first = rewind_conversation(root, &path, &request).unwrap();
        assert_eq!(load_transcript_append_generation(&path), None);
        assert_eq!(first.new_source_head_uuid, "a1");
        assert_eq!(first.canonical_history.turns.len(), 1);
        assert_eq!(first.prefill_prompt, "duplicate");
        assert!(first.recovery_backup.exists());
        let committed_bytes = std::fs::read(&path).unwrap();

        let duplicate = rewind_conversation(root, &path, &request).unwrap();
        assert_eq!(duplicate.new_stamp.sha256, first.new_stamp.sha256);
        assert_eq!(std::fs::read(&path).unwrap(), committed_bytes);
    }

    #[test]
    fn rewind_refuses_stale_revision_and_active_owner_without_writing() {
        let root_dir = temp_projects_root("rewind-refusal");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
        std::fs::write(
            &path,
            json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"prompt"}})
                .to_string(),
        )
        .unwrap();
        let displayed = load_session_history(&path).unwrap();
        let mut request = RewindConversationRequest {
            mutation_id: "mutation-stale".into(),
            session_id: "session-1".into(),
            cwd: "/workspace".into(),
            selected_user_uuid: "u1".into(),
            boundary_parent_uuid: None,
            selected_prompt: "prompt".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };
        request.expected_sha256[0] ^= 0xff;
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            rewind_conversation(root, &path, &request),
            Err(RewindConversationError::StaleRevision)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);

        request.mutation_id = "mutation-busy".into();
        request.expected_sha256 = displayed.stamp.sha256;
        let _owner = try_acquire_session_active_lock(root, &request.cwd, &request.session_id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            rewind_conversation(root, &path, &request),
            Err(RewindConversationError::Busy)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn rewind_rejects_mismatched_session_path_before_writing() {
        let root_dir = temp_projects_root("rewind-path-mismatch");
        let root = root_dir.path();
        let actual = ensure_session_file_path(root, "/workspace", "session-b").unwrap();
        std::fs::write(
            &actual,
            json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"prompt"}})
                .to_string(),
        )
        .unwrap();
        let displayed = load_session_history(&actual).unwrap();
        let request = RewindConversationRequest {
            mutation_id: "path-mismatch".into(),
            session_id: "session-a".into(),
            cwd: "/workspace".into(),
            selected_user_uuid: "u1".into(),
            boundary_parent_uuid: None,
            selected_prompt: "prompt".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };
        let before = std::fs::read(&actual).unwrap();
        assert!(matches!(
            rewind_conversation(root, &actual, &request),
            Err(RewindConversationError::TranscriptPathMismatch)
        ));
        assert_eq!(std::fs::read(&actual).unwrap(), before);
    }

    #[test]
    fn rewind_artifacts_and_replay_are_bound_to_the_full_request() {
        let root_dir = temp_projects_root("rewind-request-binding");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
        let body = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"first"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":"done"}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","message":{"content":"second"}}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, body).unwrap();
        let displayed = load_session_history(&path).unwrap();
        let request = RewindConversationRequest {
            mutation_id: "a/b".into(),
            session_id: "session-1".into(),
            cwd: "/workspace".into(),
            selected_user_uuid: "u2".into(),
            boundary_parent_uuid: Some("a1".into()),
            selected_prompt: "second".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };
        assert_ne!(
            rewind_artifact_path(&path, "a/b", "journal.json"),
            rewind_artifact_path(&path, "a_b", "journal.json")
        );
        rewind_conversation(root, &path, &request).unwrap();
        let mut changed = request.clone();
        changed.selected_prompt = "different payload".into();
        assert!(matches!(
            rewind_conversation(root, &path, &changed),
            Err(RewindConversationError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn rewind_reconciles_pre_journal_orphans_and_atomic_journal_staging() {
        let root_dir = temp_projects_root("rewind-orphans");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
        std::fs::write(
            &path,
            json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"prompt"}})
                .to_string(),
        )
        .unwrap();
        let displayed = load_session_history(&path).unwrap();
        let request = RewindConversationRequest {
            mutation_id: "orphaned".into(),
            session_id: "session-1".into(),
            cwd: "/workspace".into(),
            selected_user_uuid: "u1".into(),
            boundary_parent_uuid: None,
            selected_prompt: "prompt".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };
        let backup = rewind_artifact_path(&path, &request.mutation_id, "backup.jsonl");
        let temp = rewind_artifact_path(&path, &request.mutation_id, "target.tmp");
        let journal = rewind_artifact_path(&path, &request.mutation_id, "journal.json");
        std::fs::write(&backup, b"partial backup").unwrap();
        std::fs::write(&temp, b"partial target").unwrap();
        std::fs::write(journal.with_extension("json.next"), b"partial journal").unwrap();
        let receipt = rewind_conversation(root, &path, &request).unwrap();
        assert_eq!(receipt.new_source_head_uuid, "");
        assert_ne!(std::fs::read(&backup).unwrap(), b"partial backup");
    }

    #[test]
    fn rewind_replay_requires_an_intact_backup_and_rejects_persisted_path_tampering() {
        let root_dir = temp_projects_root("rewind-backup-integrity");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
        let original =
            json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"prompt"}})
                .to_string();
        std::fs::write(&path, &original).unwrap();
        let displayed = load_session_history(&path).unwrap();
        let request = RewindConversationRequest {
            mutation_id: "integrity".into(),
            session_id: "session-1".into(),
            cwd: "/workspace".into(),
            selected_user_uuid: "u1".into(),
            boundary_parent_uuid: None,
            selected_prompt: "prompt".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };
        let receipt = rewind_conversation(root, &path, &request).unwrap();
        std::fs::write(&receipt.recovery_backup, b"corrupt").unwrap();
        assert!(matches!(
            rewind_conversation(root, &path, &request),
            Err(RewindConversationError::RecoveryRequired(_))
        ));

        std::fs::write(&receipt.recovery_backup, original).unwrap();
        let journal_path = rewind_artifact_path(&path, &request.mutation_id, "journal.json");
        let mut journal: RewindJournal =
            serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
        let unrelated = root.join("do-not-delete.txt");
        std::fs::write(&unrelated, b"keep").unwrap();
        journal.temp_path = unrelated.clone();
        write_journal_atomically(&journal_path, &journal).unwrap();
        assert!(matches!(
            rewind_conversation(root, &path, &request),
            Err(RewindConversationError::RecoveryRequired(_))
        ));
        assert_eq!(std::fs::read(unrelated).unwrap(), b"keep");
    }

    #[test]
    fn rewind_refuses_unclassified_records_without_writing() {
        let root_dir = temp_projects_root("rewind-strict");
        let root = root_dir.path();
        let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
        let body = format!(
            "{}\nnot-json\n{}",
            json!({"type":"assistant","uuid":"a1","parentUuid":null,"message":{"content":"done"}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","message":{"content":"prompt"}})
        );
        std::fs::write(&path, &body).unwrap();
        let displayed = load_session_history(&path).unwrap();
        let request = RewindConversationRequest {
            mutation_id: "strict".into(),
            session_id: "session-1".into(),
            cwd: "/workspace".into(),
            selected_user_uuid: "u2".into(),
            boundary_parent_uuid: Some("a1".into()),
            selected_prompt: "prompt".into(),
            expected_sha256: displayed.stamp.sha256,
            expected_source_head_uuid: displayed.source_head_uuid,
        };
        assert!(matches!(
            rewind_conversation(root, &path, &request),
            Err(RewindConversationError::InvalidTranscript(_))
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), body);
    }

    #[test]
    fn durable_summary_modes_preserve_raw_rows_parents_and_restart_history() {
        for (label, mode, expected, dropped) in [
            (
                "from",
                SummarizeConversationMode::FromSelected,
                vec!["u1", "a1", "u2", "note-from"],
                2,
            ),
            (
                "upto",
                SummarizeConversationMode::UpToSelected,
                vec!["u2", "a2", "u3", "note-upto"],
                2,
            ),
        ] {
            let root_dir = temp_projects_root(&format!("summary-{label}"));
            let root = root_dir.path();
            let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
            let original = [
                json!({"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2025-01-01T00:00:00Z","message":{"content":"first"},"extension":{"keep":1}}),
                json!({"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2025-01-01T00:00:01Z","message":{"content":[{"type":"tool_use","id":"tool-1","input":{"deep":[1,2,3]},"rawOutput":{"status":"ok"}}]},"unknownRichField":{"title":"preserve"}}),
                json!({"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2025-01-01T00:00:02Z","message":{"content":"selected"},"selectedExtra":{"blocks":["x"]}}),
                json!({"type":"assistant","uuid":"a2","parentUuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"content":"later"},"toolUseResult":{"locations":[{"path":"src/lib.rs","line":7}]}}),
                json!({"type":"user","uuid":"u3","parentUuid":"a2","timestamp":"2025-01-01T00:00:04Z","message":{"content":"last"},"custom":"survives"}),
                json!({"type":"system","uuid":"generic-tail","parentUuid":"u3","timestamp":"2025-01-01T00:00:05Z","content":"projection only"}),
                json!({"type":"attachment","uuid":"attachment-tail","parentUuid":"generic-tail","timestamp":"2025-01-01T00:00:06Z","message":{"content":"projection only"}}),
            ];
            let body = original
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&path, body).unwrap();
            let displayed = load_session_history(&path).unwrap();
            let note_uuid = format!("note-{label}");
            let request = SummarizeConversationRequest {
                mutation_id: format!("summary-mutation-{label}"),
                session_id: "session-1".into(),
                cwd: "/workspace".into(),
                selected_user_uuid: "u2".into(),
                selected_parent_uuid: Some("a1".into()),
                expected_sha256: displayed.stamp.sha256,
                expected_source_head_uuid: displayed.source_head_uuid,
                mode,
                note_raw: json!({"type":"system","uuid":note_uuid,"parentUuid":"wrong","timestamp":"2025-01-01T00:00:05Z","subtype":"info","content":"durable note","level":"info","noteExtra":{"feedback":"keep"}}),
            };
            let lock = try_acquire_session_active_lock(root, "/workspace", "session-1")
                .unwrap()
                .unwrap();
            let receipt = summarize_conversation_locked(root, &path, &request, &lock).unwrap();
            assert_eq!(receipt.dropped_count, dropped);
            assert_eq!(
                receipt
                    .entries
                    .iter()
                    .map(|entry| entry.uuid.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(receipt.new_source_head_uuid, format!("note-{label}"));
            assert_eq!(
                receipt.entries.last().unwrap().raw["content"],
                "durable note"
            );
            assert_eq!(
                receipt.entries.last().unwrap().raw["noteExtra"]["feedback"],
                "keep"
            );
            assert_eq!(
                receipt.entries.last().unwrap().raw[DURABLE_SUMMARY_MARKER],
                true
            );
            if mode == SummarizeConversationMode::FromSelected {
                assert_eq!(
                    receipt.entries[1].raw["unknownRichField"]["title"],
                    "preserve"
                );
                assert_eq!(
                    receipt.entries.last().unwrap().parent_uuid.as_deref(),
                    Some("u2")
                );
            } else {
                assert_eq!(receipt.entries[0].parent_uuid, None);
                assert_eq!(
                    receipt.entries[0].raw["selectedExtra"],
                    original[2]["selectedExtra"]
                );
                assert_eq!(
                    receipt.entries[1].raw["toolUseResult"],
                    original[3]["toolUseResult"]
                );
                assert_eq!(receipt.entries[2].raw["custom"], "survives");
                assert_eq!(
                    receipt.entries.last().unwrap().parent_uuid.as_deref(),
                    Some("u3")
                );
            }
            let committed_bytes = std::fs::read(&path).unwrap();
            let committed_values = committed_bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            let mut expected_values = match mode {
                SummarizeConversationMode::FromSelected => original[..=2].to_vec(),
                SummarizeConversationMode::UpToSelected => {
                    let mut values = original[2..=4].to_vec();
                    values[0]["parentUuid"] = Value::Null;
                    values
                }
            };
            expected_values.push(receipt.entries.last().unwrap().raw.clone());
            assert_eq!(committed_values, expected_values);
            assert_eq!(
                receipt
                    .entries
                    .iter()
                    .map(|entry| entry.raw.clone())
                    .collect::<Vec<_>>(),
                expected_values
            );
            let duplicate = summarize_conversation_locked(root, &path, &request, &lock).unwrap();
            assert_eq!(duplicate.new_stamp.sha256, receipt.new_stamp.sha256);
            assert_eq!(std::fs::read(&path).unwrap(), committed_bytes);
            drop(lock);
            let restarted = load_transcript_from_file(&path).unwrap().unwrap();
            assert_eq!(restarted.messages.len(), receipt.entries.len());
            assert_eq!(
                restarted.messages.last().unwrap().uuid,
                format!("note-{label}")
            );
            for pair in restarted.messages.windows(2) {
                assert_eq!(pair[1].parent_uuid.as_deref(), Some(pair[0].uuid.as_str()));
            }
            let raw = load_raw_transcript_from_file(&path).unwrap().unwrap();
            assert!(raw.parse_complete);
            assert_eq!(raw.entries.len(), receipt.entries.len());
        }
    }

    #[test]
    fn durable_summary_note_survives_later_projection_only_suffixes() {
        for suffix_type in ["system", "attachment"] {
            let values = [
                json!({"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2025-01-01T00:00:00Z","message":{"content":"prompt"}}),
                json!({"type":"system","uuid":"note","parentUuid":"u1","timestamp":"2025-01-01T00:00:01Z","content":"durable","_rebonDurableSummary":true}),
                json!({"type":suffix_type,"uuid":"suffix","parentUuid":"note","timestamp":"2025-01-01T00:00:02Z","content":"projection only"}),
            ];
            let body = values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let entries = parse_transcript_jsonl(body.as_bytes());
            let indices = reconstruct_chain_indices(&entries);
            assert_eq!(indices, vec![0, 1], "suffix type {suffix_type}");
            let loaded = reconstruct_chain(entries).unwrap();
            assert_eq!(
                loaded
                    .messages
                    .iter()
                    .map(|entry| entry.uuid.as_str())
                    .collect::<Vec<_>>(),
                vec!["u1", "note"],
                "suffix type {suffix_type}"
            );
        }
    }

    #[test]
    fn durable_summary_stale_cas_refuses_without_writing_for_both_modes() {
        for (label, mode) in [
            ("from", SummarizeConversationMode::FromSelected),
            ("upto", SummarizeConversationMode::UpToSelected),
        ] {
            let root_dir = temp_projects_root(&format!("summary-stale-{label}"));
            let root = root_dir.path();
            let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
            std::fs::write(
                &path,
                json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"prompt"}})
                    .to_string(),
            )
            .unwrap();
            let displayed = load_session_history(&path).unwrap();
            let mut expected_sha256 = displayed.stamp.sha256;
            expected_sha256[0] ^= 0xff;
            let request = SummarizeConversationRequest {
                mutation_id: format!("summary-stale-{label}"),
                session_id: "session-1".into(),
                cwd: "/workspace".into(),
                selected_user_uuid: "u1".into(),
                selected_parent_uuid: None,
                expected_sha256,
                expected_source_head_uuid: displayed.source_head_uuid,
                mode,
                note_raw: json!({"type":"system","uuid":format!("note-{label}"),"content":"note"}),
            };
            let lock = try_acquire_session_active_lock(root, "/workspace", "session-1")
                .unwrap()
                .unwrap();
            let before = std::fs::read(&path).unwrap();
            assert!(matches!(
                summarize_conversation_locked(root, &path, &request, &lock),
                Err(RewindConversationError::StaleRevision)
            ));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn durable_summary_recovery_matrix_covers_old_target_and_corrupt_states() {
        for (mode_label, mode) in [
            ("from", SummarizeConversationMode::FromSelected),
            ("upto", SummarizeConversationMode::UpToSelected),
        ] {
            for state in [
                "old-journal-only",
                "old-with-backup",
                "old-with-artifacts",
                "target-uncommitted",
                "target-committed",
                "target-missing-backup",
                "target-corrupt-backup",
                "neither-revision",
            ] {
                let root_dir =
                    temp_projects_root(&format!("summary-recovery-{mode_label}-{state}"));
                let root = root_dir.path();
                let path = ensure_session_file_path(root, "/workspace", "session-1").unwrap();
                let original_values = [
                    json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"first"},"raw":{"deep":[1,2]}}),
                    json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":"reply"},"tool":{"result":{"ok":true}}}),
                    json!({"type":"user","uuid":"u2","parentUuid":"a1","message":{"content":"selected"},"selected":{"keep":true}}),
                    json!({"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"content":"later"},"later":{"keep":true}}),
                ];
                let original = original_values
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_bytes();
                std::fs::write(&path, &original).unwrap();
                let displayed = load_session_history(&path).unwrap();
                let request = SummarizeConversationRequest {
                    mutation_id: format!("recovery-{mode_label}-{state}"),
                    session_id: "session-1".into(),
                    cwd: "/workspace".into(),
                    selected_user_uuid: "u2".into(),
                    selected_parent_uuid: Some("a1".into()),
                    expected_sha256: displayed.stamp.sha256,
                    expected_source_head_uuid: displayed.source_head_uuid,
                    mode,
                    note_raw: json!({"type":"system","uuid":format!("note-{mode_label}-{state}"),"content":"durable note","rawNote":{"keep":true}}),
                };
                let lock = try_acquire_session_active_lock(root, "/workspace", "session-1")
                    .unwrap()
                    .unwrap();
                let receipt = summarize_conversation_locked(root, &path, &request, &lock).unwrap();
                let target = std::fs::read(&path).unwrap();
                let journal_path =
                    rewind_artifact_path(&path, &request.mutation_id, "journal.json");
                let backup_path = rewind_artifact_path(&path, &request.mutation_id, "backup.jsonl");
                let temp_path = rewind_artifact_path(&path, &request.mutation_id, "target.tmp");
                let mut journal: RewindJournal =
                    serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();

                match state {
                    "old-journal-only" | "old-with-backup" | "old-with-artifacts" => {
                        journal.committed = false;
                        write_journal_atomically(&journal_path, &journal).unwrap();
                        std::fs::write(&path, &original).unwrap();
                        if state == "old-journal-only" {
                            std::fs::remove_file(&backup_path).unwrap();
                        } else if state == "old-with-artifacts" {
                            std::fs::write(&temp_path, &target).unwrap();
                        }
                    }
                    "target-uncommitted" => {
                        journal.committed = false;
                        write_journal_atomically(&journal_path, &journal).unwrap();
                    }
                    "target-committed" => {}
                    "target-missing-backup" => {
                        std::fs::remove_file(&backup_path).unwrap();
                    }
                    "target-corrupt-backup" => {
                        std::fs::write(&backup_path, b"corrupt backup").unwrap();
                    }
                    "neither-revision" => {
                        journal.committed = false;
                        write_journal_atomically(&journal_path, &journal).unwrap();
                        std::fs::write(
                            &path,
                            json!({"type":"user","uuid":"other","parentUuid":null,"message":{"content":"other"}}).to_string(),
                        )
                        .unwrap();
                    }
                    _ => unreachable!(),
                }

                let recovered = summarize_conversation_locked(root, &path, &request, &lock);
                match state {
                    "target-missing-backup" | "target-corrupt-backup" => assert!(matches!(
                        recovered,
                        Err(RewindConversationError::CommittedRecoveryRequired(_))
                    )),
                    "neither-revision" => assert!(matches!(
                        recovered,
                        Err(RewindConversationError::RecoveryRequired(_))
                    )),
                    _ => {
                        let recovered = recovered.unwrap();
                        assert_eq!(recovered.new_stamp.sha256, receipt.new_stamp.sha256);
                        assert_eq!(std::fs::read(&path).unwrap(), target);
                        assert_eq!(
                            recovered.entries.last().unwrap().raw["rawNote"]["keep"],
                            true
                        );
                    }
                }
                drop(lock);
                let restarted = load_transcript_from_file(&path).unwrap().unwrap();
                if state == "neither-revision" {
                    assert_eq!(restarted.messages[0].uuid, "other");
                } else {
                    assert_eq!(
                        restarted.messages.last().unwrap().uuid,
                        format!("note-{mode_label}-{state}")
                    );
                    for pair in restarted.messages.windows(2) {
                        assert_eq!(pair[1].parent_uuid.as_deref(), Some(pair[0].uuid.as_str()));
                    }
                }
            }
        }
    }

    #[test]
    fn transcript_stamp_detects_same_length_content_replacement() {
        let root_dir = temp_projects_root("history-stamp");
        let root = root_dir.path();
        let path = root.join("history.jsonl");
        let first = b"{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"content\":\"aa\"}}";
        let second = b"{\"type\":\"user\",\"uuid\":\"u2\",\"message\":{\"content\":\"bb\"}}";
        assert_eq!(first.len(), second.len());
        std::fs::write(&path, first).unwrap();
        let before = load_session_history(&path).unwrap().stamp;
        std::fs::write(&path, second).unwrap();
        let after = load_session_history(&path).unwrap().stamp;
        assert_eq!(before.byte_len, after.byte_len);
        assert_ne!(before.sha256, after.sha256);
    }
}
