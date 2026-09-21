use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rebon_session::session_storage as session_store;
use rebon_types::Usage;
use serde::{Deserialize, Serialize};

use crate::day::{
    date_from_day_number, day_number_from_date, day_number_from_ms, days_from_civil,
    local_utc_offset_seconds,
};
use crate::{now_ms, BackgroundJobState, BackgroundStore};

const GLOBAL_STATS_CACHE_DIR: &str = "cache";
const GLOBAL_STATS_CACHE_FILE: &str = "global-stats.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalStats {
    pub total_tokens: u64,
    pub peak_tokens: u64,
    pub peak_day: Option<String>,
    pub longest_task_ms: u64,
    pub current_streak_days: u64,
    pub longest_streak_days: u64,
    pub session_count: u64,
    pub daily_tokens: Vec<DailyTokenActivity>,
    pub model_usage: Vec<ModelUsageStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyTokenActivity {
    pub date: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsageStats {
    pub model: Option<String>,
    pub total_tokens: u64,
    pub request_count: u64,
}

#[derive(Debug, Clone, Default)]
struct SessionStats {
    tokens: u64,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
    /// One entry per distinct local day, folded as the scan goes. Keeping one
    /// entry per *turn* here made a long transcript's stats as large as the
    /// aggregate they feed.
    daily_tokens: Vec<(String, u64)>,
    /// One entry per distinct model: `(model, tokens, requests)`. The request
    /// count used to be recovered by counting elements, which is why this was
    /// unfolded; carrying it explicitly costs 8 bytes and saves a vector the
    /// length of the conversation.
    model_usage: Vec<(Option<String>, u64, u64)>,
}

impl SessionStats {
    fn record_day(&mut self, day: String, tokens: u64) {
        if let Some(slot) = self
            .daily_tokens
            .iter_mut()
            .find(|(known, _)| *known == day)
        {
            slot.1 = slot.1.saturating_add(tokens);
            return;
        }
        self.daily_tokens.push((day, tokens));
    }

    fn record_model(&mut self, model: Option<&str>, tokens: u64) {
        if let Some(slot) = self
            .model_usage
            .iter_mut()
            .find(|(known, _, _)| known.as_deref() == model)
        {
            slot.1 = slot.1.saturating_add(tokens);
            slot.2 = slot.2.saturating_add(1);
            return;
        }
        self.model_usage
            .push((model.map(str::to_string), tokens, 1));
    }

    /// Fold in the owning job's own start/end, then drop a session that
    /// contributed nothing at all.
    fn with_job_timing(mut self, job_timing: Option<(u64, u64)>) -> Option<Self> {
        if let Some((first, last)) = job_timing {
            self.first_ms = Some(self.first_ms.map(|ms| ms.min(first)).unwrap_or(first));
            self.last_ms = Some(self.last_ms.map(|ms| ms.max(last)).unwrap_or(last));
        }
        (self.first_ms.is_some() || self.tokens > 0).then_some(self)
    }
}

impl GlobalStats {
    fn from_sessions(sessions: Vec<SessionStats>, now_ms: u64, offset_seconds: i32) -> Self {
        let mut daily: BTreeMap<String, u64> = BTreeMap::new();
        let mut usage_by_model: BTreeMap<Option<String>, (u64, u64)> = BTreeMap::new();
        let mut total_tokens = 0u64;
        let mut longest_task_ms = 0u64;

        let session_count = sessions.len() as u64;
        for session in sessions {
            if session.tokens > 0 {
                total_tokens = total_tokens.saturating_add(session.tokens);
                for (day, tokens) in session.daily_tokens {
                    let slot = daily.entry(day).or_default();
                    *slot = slot.saturating_add(tokens);
                }
            }
            for (model, tokens, requests) in session.model_usage {
                let usage = usage_by_model.entry(model).or_default();
                usage.0 = usage.0.saturating_add(tokens);
                usage.1 = usage.1.saturating_add(requests);
            }
            if let (Some(first), Some(last)) = (session.first_ms, session.last_ms) {
                longest_task_ms = longest_task_ms.max(last.saturating_sub(first));
            }
        }

        let daily_tokens: Vec<DailyTokenActivity> = daily
            .iter()
            .map(|(date, tokens)| DailyTokenActivity {
                date: date.clone(),
                tokens: *tokens,
            })
            .collect();
        let mut model_usage: Vec<ModelUsageStats> = usage_by_model
            .into_iter()
            .map(|(model, (total_tokens, request_count))| ModelUsageStats {
                model,
                total_tokens,
                request_count,
            })
            .collect();
        model_usage.sort_by(|left, right| {
            right
                .total_tokens
                .cmp(&left.total_tokens)
                .then_with(|| right.request_count.cmp(&left.request_count))
                .then_with(|| match (&left.model, &right.model) {
                    (Some(left), Some(right)) => left.cmp(right),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
        });
        let (peak_day, peak_tokens) = daily_tokens
            .iter()
            .max_by_key(|entry| entry.tokens)
            .map(|entry| (Some(entry.date.clone()), entry.tokens))
            .unwrap_or((None, 0));
        let active_days: HashSet<i64> = daily_tokens
            .iter()
            .filter(|entry| entry.tokens > 0)
            .filter_map(|entry| day_number_from_date(&entry.date))
            .collect();
        let today = day_number_from_ms(now_ms, offset_seconds);

        Self {
            total_tokens,
            peak_tokens,
            peak_day,
            longest_task_ms,
            current_streak_days: current_streak(&active_days, today),
            longest_streak_days: longest_streak(&active_days),
            session_count,
            daily_tokens,
            model_usage,
        }
    }
}

impl BackgroundStore {
    pub fn global_stats(&self) -> GlobalStats {
        collect_global_stats(self.root(), now_ms())
    }
}

/// Rescan every transcript, refreshing the on-disk snapshot. Returns `None` when
/// the scan matches `previous`, so callers skip both the cache write and a repaint.
pub fn refresh_default_global_stats(previous: &GlobalStats) -> Option<GlobalStats> {
    let root = session_store::default_config_home_dir();
    let stats = collect_global_stats(&root, now_ms());
    if &stats == previous {
        return None;
    }
    if let Err(error) = write_global_stats_cache(&root, &stats) {
        tracing::debug!(%error, "failed to cache global stats");
    }
    Some(stats)
}

/// A scan reads every transcript under the config home, which on a well-used
/// install is gigabytes of JSONL. Every surface showing these numbers shares one
/// clock, so neither reopening a menu nor a background poll can start a scan
/// while another is in flight or its result is still this fresh.
const GLOBAL_STATS_MIN_RESCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

static GLOBAL_STATS_SCAN_CLOCK: Mutex<GlobalStatsScanClock> = Mutex::new(GlobalStatsScanClock {
    in_flight: false,
    finished_at: None,
    latest: None,
});

struct GlobalStatsScanClock {
    in_flight: bool,
    finished_at: Option<Instant>,
    latest: Option<GlobalStats>,
}

/// Like [`refresh_default_global_stats`], but rescans only once the shared
/// snapshot has gone stale; otherwise it hands back the newest stats another
/// caller already scanned. `None` means "keep rendering `previous`" — either
/// nothing changed or the scan was skipped.
pub fn refresh_default_global_stats_if_stale(previous: &GlobalStats) -> Option<GlobalStats> {
    let mut lease = match ScanLease::claim(GLOBAL_STATS_MIN_RESCAN_INTERVAL) {
        ScanClaim::Granted(lease) => lease,
        ScanClaim::Throttled(latest) => return latest.filter(|stats| stats != previous),
    };
    let scanned = refresh_default_global_stats(previous);
    lease.record(scanned.clone().unwrap_or_else(|| previous.clone()));
    scanned
}

enum ScanClaim {
    Granted(ScanLease),
    /// Carries the freshest scan this process has seen, if any.
    Throttled(Option<GlobalStats>),
}

/// Holds the in-flight flag for the length of a scan. The clock advances on drop
/// so a scan that panics cannot wedge the throttle shut.
struct ScanLease {
    result: Option<GlobalStats>,
}

impl ScanLease {
    fn claim(max_age: Duration) -> ScanClaim {
        let mut clock = lock_scan_clock();
        if clock.in_flight || clock.finished_at.is_some_and(|at| at.elapsed() < max_age) {
            return ScanClaim::Throttled(clock.latest.clone());
        }
        clock.in_flight = true;
        ScanClaim::Granted(Self { result: None })
    }

    fn record(&mut self, stats: GlobalStats) {
        self.result = Some(stats);
    }
}

impl Drop for ScanLease {
    fn drop(&mut self) {
        let mut clock = lock_scan_clock();
        clock.in_flight = false;
        clock.finished_at = Some(Instant::now());
        if let Some(stats) = self.result.take() {
            clock.latest = Some(stats);
        }
    }
}

fn lock_scan_clock() -> MutexGuard<'static, GlobalStatsScanClock> {
    GLOBAL_STATS_SCAN_CLOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn load_cached_default_global_stats() -> Option<GlobalStats> {
    load_global_stats_cache(&session_store::default_config_home_dir())
}

fn global_stats_cache_path(root: &Path) -> PathBuf {
    root.join(GLOBAL_STATS_CACHE_DIR)
        .join(GLOBAL_STATS_CACHE_FILE)
}

fn load_global_stats_cache(root: &Path) -> Option<GlobalStats> {
    let payload = fs::read(global_stats_cache_path(root)).ok()?;
    serde_json::from_slice(&payload).ok()
}

fn write_global_stats_cache(root: &Path, stats: &GlobalStats) -> anyhow::Result<()> {
    let path = global_stats_cache_path(root);
    let dir = path.parent().expect("global stats cache path has a parent");
    fs::create_dir_all(dir)?;
    rebon_session::write_file_atomically(&path, &serde_json::to_vec(stats)?)?;
    Ok(())
}

/// Aggregate every transcript under `root`. Day buckets follow the local wall
/// clock at `now_ms`; [`collect_global_stats_at_offset`] pins the offset instead.
pub fn collect_global_stats(root: &Path, now_ms: u64) -> GlobalStats {
    collect_global_stats_at_offset(root, now_ms, local_utc_offset_seconds(now_ms))
}

fn collect_global_stats_at_offset(root: &Path, now_ms: u64, offset_seconds: i32) -> GlobalStats {
    let states = read_job_states(root);
    let job_timings = job_timings(&states);
    let mut sessions = Vec::new();
    let projects_root = root.join("projects");

    // A transcript that has not been written since the last scan produces the
    // same numbers it produced then. Without this memo every scan re-reads and
    // re-parses the entire transcript tree — on a well-used install that is
    // gigabytes of file I/O and tens of millions of allocations, every time.
    let mut index = TranscriptStatsIndex::load(root);
    let mut seen = HashSet::new();

    for transcript in transcript_paths(&projects_root) {
        let session_id = transcript
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
            .unwrap_or_default();
        let job_timing = job_timings.get(&session_id).copied().flatten();
        let key = transcript.to_string_lossy().into_owned();
        let revision = TranscriptRevision::of(&transcript);
        seen.insert(key.clone());

        // The offset only shapes day buckets, so a memo taken under a different
        // offset (a travelling laptop) must be recomputed rather than reused.
        if let Some(cached) = index.get(&key, revision, offset_seconds) {
            if let Some(stats) = cached.with_job_timing(job_timing) {
                sessions.push(stats);
            }
            continue;
        }
        let scanned = session_stats_from_transcript_uncached(&transcript, offset_seconds);
        if let (Some(revision), Some(scanned)) = (revision, scanned.as_ref()) {
            index.insert(key, revision, offset_seconds, scanned);
        }
        if let Some(stats) = scanned.and_then(|stats| stats.with_job_timing(job_timing)) {
            sessions.push(stats);
        }
    }
    index.retain(&seen);
    index.save(root);

    for state in states {
        let session_id = state
            .identity
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or(&state.identity.job_id);
        let transcript_path =
            session_store::transcript_file_path(&projects_root, &state.identity.cwd, session_id);
        if transcript_path.exists() {
            continue;
        }
        if let Some(stats) = session_stats_from_job(&state) {
            sessions.push(stats);
        }
    }

    GlobalStats::from_sessions(sessions, now_ms, offset_seconds)
}

fn read_job_states(root: &Path) -> Vec<BackgroundJobState> {
    let Ok(entries) = fs::read_dir(root.join("jobs")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path().join("state.json")).ok())
        .filter_map(|text| {
            let mut state = serde_json::from_str::<BackgroundJobState>(&text).ok()?;
            state.normalize_pending_prompts().ok()?;
            Some(state)
        })
        .collect()
}

fn job_timings(states: &[BackgroundJobState]) -> HashMap<String, Option<(u64, u64)>> {
    let mut out = HashMap::new();
    for state in states {
        let timing = job_timing(state);
        out.insert(state.identity.job_id.clone(), timing);
        if let Some(session_id) = state
            .identity
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
        {
            out.insert(session_id.to_string(), timing);
        }
    }
    out
}

fn job_timing(state: &BackgroundJobState) -> Option<(u64, u64)> {
    let start = state
        .process
        .started_at_ms
        .unwrap_or(state.process.created_at_ms);
    let end = state
        .process
        .completed_at_ms
        .unwrap_or(state.process.updated_at_ms);
    (start > 0 && end >= start).then_some((start, end))
}

fn transcript_paths(projects_root: &Path) -> Vec<PathBuf> {
    let Ok(projects) = fs::read_dir(projects_root) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for project in projects.flatten() {
        let Ok(kind) = project.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        if let Ok(files) = fs::read_dir(project.path()) {
            paths.extend(files.flatten().filter_map(|file| {
                let path = file.path();
                (path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")).then_some(path)
            }));
        }
    }
    paths
}

fn session_stats_from_job(state: &BackgroundJobState) -> Option<SessionStats> {
    let (first, last) = job_timing(state)?;
    Some(SessionStats {
        tokens: 0,
        first_ms: Some(first),
        last_ms: Some(last),
        daily_tokens: Vec::new(),
        model_usage: Vec::new(),
    })
}

/// The only four things a transcript line contributes to these stats.
///
/// Declared as a borrowed struct rather than read through `serde_json::Value`:
/// a transcript line is mostly message content and tool payloads, and building a
/// `Value` for all of it allocates a map entry and a `String` per key just to
/// throw them away. Serde skips every undeclared field without materialising it,
/// and the two `&str`s point into the caller's line buffer — so a line costs
/// zero allocations instead of dozens.
#[derive(Deserialize)]
struct TranscriptLine<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<&'a str>,
    #[serde(borrow, default)]
    timestamp: Option<&'a str>,
    #[serde(borrow, default)]
    message: Option<TranscriptMessage<'a>>,
}

#[derive(Deserialize)]
struct TranscriptMessage<'a> {
    #[serde(borrow, default)]
    model: Option<&'a str>,
    #[serde(default)]
    usage: Option<Usage>,
}

fn session_stats_from_transcript_uncached(
    path: &Path,
    offset_seconds: i32,
) -> Option<SessionStats> {
    let file = fs::File::open(path).ok()?;
    let mut stats = SessionStats::default();
    let mut reader = BufReader::new(file);
    // One buffer for the whole file. `BufReader::lines()` hands back an owned
    // `String` per line, which on a multi-gigabyte transcript tree is millions of
    // allocations on its own.
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<TranscriptLine>(trimmed) else {
            continue;
        };
        let entry_ms = entry.timestamp.and_then(parse_rfc3339_ms);
        if stats.first_ms.is_none() {
            stats.first_ms = entry_ms;
        }
        if let Some(ms) = entry_ms {
            stats.last_ms = Some(ms);
        }
        if entry.kind != Some("assistant") {
            continue;
        }
        let Some(message) = entry.message else {
            continue;
        };
        let model = message
            .model
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let tokens = message
            .usage
            .map(|usage| {
                u64::from(usage.billed_input_tokens())
                    .saturating_add(u64::from(usage.billed_output_tokens()))
            })
            .unwrap_or(0);
        stats.record_model(model, tokens);
        stats.tokens = stats.tokens.saturating_add(tokens);
        if tokens > 0 {
            if let Some(day) = entry_ms.map(|ms| day_key_from_ms(ms, offset_seconds)) {
                stats.record_day(day, tokens);
            }
        }
    }
    Some(stats)
}

// ---------------------------------------------------------------------------
// Per-transcript memo
// ---------------------------------------------------------------------------

const TRANSCRIPT_STATS_INDEX_FILE: &str = "global-stats-transcripts.json";

/// Serialized index is capped: it is a *cache*, and a dropped entry only costs
/// one rescan of that file.
const TRANSCRIPT_STATS_INDEX_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Filesystem identity of a transcript, for deciding whether a memo still holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TranscriptRevision {
    len: u64,
    mtime_ms: u64,
}

impl TranscriptRevision {
    fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let mtime_ms = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        Some(Self {
            len: meta.len(),
            mtime_ms,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct TranscriptStatsRecord {
    len: u64,
    mtime_ms: u64,
    /// Day buckets depend on the local offset the scan ran under, so a memo
    /// taken in another timezone must be recomputed rather than reused.
    offset_seconds: i32,
    #[serde(default)]
    tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    daily: Vec<(String, u64)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    models: Vec<(Option<String>, u64, u64)>,
}

/// Remembers what each transcript contributed, keyed by its `(len, mtime)`.
///
/// Without it every scan re-reads and re-parses the whole transcript tree. With
/// it, a scan reads only the files that were actually written since last time —
/// which on an idle machine is none of them.
#[derive(Default)]
struct TranscriptStatsIndex {
    entries: HashMap<String, TranscriptStatsRecord>,
    dirty: bool,
}

impl TranscriptStatsIndex {
    fn path(root: &Path) -> PathBuf {
        root.join(GLOBAL_STATS_CACHE_DIR)
            .join(TRANSCRIPT_STATS_INDEX_FILE)
    }

    fn load(root: &Path) -> Self {
        let entries = fs::read(Self::path(root))
            .ok()
            .and_then(|payload| serde_json::from_slice(&payload).ok())
            .unwrap_or_default();
        Self {
            entries,
            dirty: false,
        }
    }

    fn get(
        &self,
        key: &str,
        revision: Option<TranscriptRevision>,
        offset_seconds: i32,
    ) -> Option<SessionStats> {
        let revision = revision?;
        let record = self.entries.get(key)?;
        if record.len != revision.len
            || record.mtime_ms != revision.mtime_ms
            || record.offset_seconds != offset_seconds
        {
            return None;
        }
        Some(SessionStats {
            tokens: record.tokens,
            first_ms: record.first_ms,
            last_ms: record.last_ms,
            daily_tokens: record.daily.clone(),
            model_usage: record.models.clone(),
        })
    }

    fn insert(
        &mut self,
        key: String,
        revision: TranscriptRevision,
        offset_seconds: i32,
        stats: &SessionStats,
    ) {
        self.entries.insert(
            key,
            TranscriptStatsRecord {
                len: revision.len,
                mtime_ms: revision.mtime_ms,
                offset_seconds,
                tokens: stats.tokens,
                first_ms: stats.first_ms,
                last_ms: stats.last_ms,
                daily: stats.daily_tokens.clone(),
                models: stats.model_usage.clone(),
            },
        );
        self.dirty = true;
    }

    /// Forget transcripts that no longer exist.
    fn retain(&mut self, seen: &HashSet<String>) {
        let before = self.entries.len();
        self.entries.retain(|key, _| seen.contains(key));
        self.dirty |= self.entries.len() != before;
    }

    fn save(&self, root: &Path) {
        if !self.dirty {
            return;
        }
        let mut entries = self.entries.clone();
        let mut payload = match serde_json::to_vec(&entries) {
            Ok(payload) => payload,
            Err(_) => return,
        };
        if payload.len() > TRANSCRIPT_STATS_INDEX_MAX_BYTES {
            // Drop the memos for the smallest transcripts first — they are the
            // cheapest to recompute if they are asked for again.
            let mut by_cost: Vec<(String, u64)> = entries
                .iter()
                .map(|(key, record)| (key.clone(), record.len))
                .collect();
            by_cost.sort_by_key(|(_, len)| *len);
            for (key, _) in by_cost {
                entries.remove(&key);
                payload = match serde_json::to_vec(&entries) {
                    Ok(payload) => payload,
                    Err(_) => return,
                };
                if payload.len() <= TRANSCRIPT_STATS_INDEX_MAX_BYTES {
                    break;
                }
            }
        }
        let path = Self::path(root);
        let Some(dir) = path.parent() else {
            return;
        };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        // Best-effort: a memo that fails to land is recomputed next time.
        let _ = rebon_session::write_file_atomically(&path, &payload);
    }
}

fn parse_rfc3339_ms(input: &str) -> Option<u64> {
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
    let year: i64 = input.get(0..4)?.parse().ok()?;
    let month: i64 = input.get(5..7)?.parse().ok()?;
    let day: i64 = input.get(8..10)?.parse().ok()?;
    let hour: i64 = input.get(11..13)?.parse().ok()?;
    let minute: i64 = input.get(14..16)?.parse().ok()?;
    let second: i64 = input.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let mut idx = 19usize;
    let mut millis = 0u64;
    if bytes.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while bytes.get(idx).is_some_and(|b| b.is_ascii_digit()) {
            idx += 1;
        }
        let mut frac = input.get(start..idx)?.to_string();
        if frac.len() > 3 {
            frac.truncate(3);
        }
        while frac.len() < 3 {
            frac.push('0');
        }
        millis = frac.parse().ok()?;
    }

    let tz_offset_minutes = match bytes.get(idx) {
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

    let days = days_from_civil(year, month, day)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_sub(tz_offset_minutes * 60)?;
    if seconds < 0 {
        return None;
    }
    Some((seconds as u64).saturating_mul(1000).saturating_add(millis))
}

fn day_key_from_ms(ms: u64, offset_seconds: i32) -> String {
    date_from_day_number(day_number_from_ms(ms, offset_seconds))
}

fn current_streak(active_days: &HashSet<i64>, today: i64) -> u64 {
    let mut day = today;
    if !active_days.contains(&day) && !active_days.contains(&(day - 1)) {
        return 0;
    }
    if !active_days.contains(&day) {
        day -= 1;
    }
    let mut streak = 0u64;
    while active_days.contains(&day) {
        streak += 1;
        day -= 1;
    }
    streak
}

fn longest_streak(active_days: &HashSet<i64>) -> u64 {
    let mut days: Vec<i64> = active_days.iter().copied().collect();
    days.sort_unstable();
    let mut best = 0u64;
    let mut current = 0u64;
    let mut prev = None;
    for day in days {
        current = if prev == Some(day - 1) {
            current + 1
        } else {
            1
        };
        best = best.max(current);
        prev = Some(day);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Every assertion below pins the day offset so a scan's buckets never depend
    /// on the test machine's timezone.
    const UTC: i32 = 0;
    const UTC_PLUS_8: i32 = 8 * 3_600;

    fn usage_line(ts: &str, input: u32, output: u32) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "message": {
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                }
            }
        })
        .to_string()
    }

    fn model_usage_line(ts: &str, model: Option<&str>, input: u32, output: u32) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "message": {
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn parses_usage_and_daily_streaks_from_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let mut file = fs::File::create(project.join("s1.jsonl")).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T10:00:00.000Z", 100, 50)).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T10:05:00.000Z", 20, 10)).unwrap();
        let mut file = fs::File::create(project.join("s2.jsonl")).unwrap();
        writeln!(file, "{}", usage_line("2026-07-05T09:00:00.000Z", 40, 10)).unwrap();
        let stats = collect_global_stats_at_offset(
            dir.path(),
            parse_rfc3339_ms("2026-07-05T12:00:00.000Z").unwrap(),
            UTC,
        );

        assert_eq!(stats.total_tokens, 230);
        assert_eq!(stats.peak_day.as_deref(), Some("2026-07-04"));
        assert_eq!(stats.peak_tokens, 180);
        assert_eq!(stats.current_streak_days, 2);
        assert_eq!(stats.longest_streak_days, 2);
        assert_eq!(stats.longest_task_ms, 5 * 60 * 1000);
    }

    /// A second scan of an untouched tree must reuse the memo *and* agree with
    /// the first to the token — the memo is a cache, not a second answer.
    #[test]
    fn a_rescan_reuses_the_memo_and_agrees_with_the_first_scan() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let mut file = fs::File::create(project.join("s1.jsonl")).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T10:00:00.000Z", 100, 50)).unwrap();
        drop(file);
        let now = parse_rfc3339_ms("2026-07-05T12:00:00.000Z").unwrap();

        let first = collect_global_stats_at_offset(dir.path(), now, UTC);
        let index_path = TranscriptStatsIndex::path(dir.path());
        assert!(index_path.exists(), "the first scan must record its memo");
        let index = TranscriptStatsIndex::load(dir.path());
        assert_eq!(index.entries.len(), 1);

        let second = collect_global_stats_at_offset(dir.path(), now, UTC);
        assert_eq!(first, second);
        assert_eq!(second.total_tokens, 150);
    }

    /// Appending to a transcript changes its `(len, mtime)`, so the memo must
    /// stand aside rather than report yesterday's numbers forever.
    #[test]
    fn appending_to_a_transcript_invalidates_its_memo() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("s1.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T10:00:00.000Z", 100, 50)).unwrap();
        drop(file);
        let now = parse_rfc3339_ms("2026-07-05T12:00:00.000Z").unwrap();
        assert_eq!(
            collect_global_stats_at_offset(dir.path(), now, UTC).total_tokens,
            150
        );

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T10:05:00.000Z", 20, 10)).unwrap();
        drop(file);
        assert_eq!(
            collect_global_stats_at_offset(dir.path(), now, UTC).total_tokens,
            180
        );
    }

    /// Day buckets are computed under one offset; a memo taken under another one
    /// would silently file tokens on the wrong day.
    #[test]
    fn a_memo_from_another_timezone_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let mut file = fs::File::create(project.join("s1.jsonl")).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T17:00:00.000Z", 100, 0)).unwrap();
        drop(file);
        let now = parse_rfc3339_ms("2026-07-04T17:30:00.000Z").unwrap();

        let utc = collect_global_stats_at_offset(dir.path(), now, UTC);
        let shifted = collect_global_stats_at_offset(dir.path(), now, UTC_PLUS_8);
        assert_eq!(utc.daily_tokens[0].date, "2026-07-04");
        assert_eq!(shifted.daily_tokens[0].date, "2026-07-05");
    }

    /// A transcript that disappears takes its memo with it, so the index cannot
    /// grow without bound on a machine that keeps deleting sessions.
    #[test]
    fn a_deleted_transcript_drops_out_of_the_memo() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("s1.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T10:00:00.000Z", 100, 50)).unwrap();
        drop(file);
        let now = parse_rfc3339_ms("2026-07-05T12:00:00.000Z").unwrap();
        collect_global_stats_at_offset(dir.path(), now, UTC);
        assert_eq!(TranscriptStatsIndex::load(dir.path()).entries.len(), 1);

        fs::remove_file(&path).unwrap();
        collect_global_stats_at_offset(dir.path(), now, UTC);
        assert!(TranscriptStatsIndex::load(dir.path()).entries.is_empty());
    }

    /// Per-session folding: one entry per model and per day, whatever the
    /// conversation's length, with the request count carried rather than
    /// recovered by counting elements.
    #[test]
    fn a_sessions_stats_fold_by_model_and_day() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("s1.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        for _ in 0..50 {
            writeln!(
                file,
                "{}",
                model_usage_line("2026-07-04T10:00:00.000Z", Some("m1"), 10, 1)
            )
            .unwrap();
        }
        writeln!(
            file,
            "{}",
            model_usage_line("2026-07-05T10:00:00.000Z", Some("m2"), 4, 1)
        )
        .unwrap();
        drop(file);

        let stats = session_stats_from_transcript_uncached(&path, UTC).unwrap();
        assert_eq!(stats.model_usage.len(), 2);
        assert_eq!(stats.daily_tokens.len(), 2);
        let m1 = stats
            .model_usage
            .iter()
            .find(|(model, _, _)| model.as_deref() == Some("m1"))
            .unwrap();
        assert_eq!(m1.1, 50 * 11);
        assert_eq!(m1.2, 50, "request count is carried, not re-derived");
        assert_eq!(stats.tokens, 50 * 11 + 5);
    }

    #[test]
    fn day_buckets_and_streaks_follow_the_local_offset() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let mut file = fs::File::create(project.join("s1.jsonl")).unwrap();
        // 17:00Z is already past midnight in UTC+8, so the tokens belong to the
        // 5th locally and to the 4th in UTC.
        writeln!(file, "{}", usage_line("2026-07-04T17:00:00.000Z", 100, 0)).unwrap();
        writeln!(file, "{}", usage_line("2026-07-04T02:00:00.000Z", 10, 0)).unwrap();
        let now = parse_rfc3339_ms("2026-07-04T17:30:00.000Z").unwrap();

        let utc = collect_global_stats_at_offset(dir.path(), now, UTC);
        assert_eq!(
            utc.daily_tokens,
            vec![DailyTokenActivity {
                date: "2026-07-04".into(),
                tokens: 110,
            }]
        );
        assert_eq!(utc.current_streak_days, 1);

        let local = collect_global_stats_at_offset(dir.path(), now, UTC_PLUS_8);
        assert_eq!(
            local.daily_tokens,
            vec![
                DailyTokenActivity {
                    date: "2026-07-04".into(),
                    tokens: 10,
                },
                DailyTokenActivity {
                    date: "2026-07-05".into(),
                    tokens: 100,
                },
            ]
        );
        assert_eq!(local.peak_day.as_deref(), Some("2026-07-05"));
        // "Today" is the 5th locally, and the 4th is active too.
        assert_eq!(local.current_streak_days, 2);
    }

    #[test]
    fn empty_data_has_no_model_usage() {
        let dir = tempfile::tempdir().unwrap();
        let stats = collect_global_stats(dir.path(), 0);

        assert!(stats.model_usage.is_empty());
        assert_eq!(stats.total_tokens, 0);
        assert_eq!(stats.session_count, 0);
    }

    #[test]
    fn aggregates_blank_and_missing_models_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let mut file = fs::File::create(project.join("unknown.jsonl")).unwrap();
        writeln!(file, "{}", usage_line("2026-07-05T09:00:00.000Z", 10, 5)).unwrap();
        writeln!(
            file,
            "{}",
            model_usage_line("2026-07-05T09:01:00.000Z", Some("   "), 20, 5)
        )
        .unwrap();

        let stats = collect_global_stats(dir.path(), 0);
        assert_eq!(
            stats.model_usage,
            vec![ModelUsageStats {
                model: None,
                total_tokens: 40,
                request_count: 2,
            }]
        );
    }

    #[test]
    fn aggregates_multiple_models_with_deterministic_usage_order() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects").join("repo");
        fs::create_dir_all(&project).unwrap();
        let mut file = fs::File::create(project.join("models.jsonl")).unwrap();
        for line in [
            model_usage_line("2026-07-05T09:00:00.000Z", Some("zeta"), 20, 10),
            model_usage_line("2026-07-05T09:01:00.000Z", Some("beta"), 5, 5),
            model_usage_line("2026-07-05T09:02:00.000Z", Some(" alpha "), 25, 5),
            model_usage_line("2026-07-05T09:03:00.000Z", Some("beta"), 10, 10),
        ] {
            writeln!(file, "{line}").unwrap();
        }

        let stats = collect_global_stats(dir.path(), 0);
        assert_eq!(
            stats.model_usage,
            vec![
                ModelUsageStats {
                    model: Some("beta".into()),
                    total_tokens: 30,
                    request_count: 2,
                },
                ModelUsageStats {
                    model: Some("alpha".into()),
                    total_tokens: 30,
                    request_count: 1,
                },
                ModelUsageStats {
                    model: Some("zeta".into()),
                    total_tokens: 30,
                    request_count: 1,
                },
            ]
        );
    }

    #[test]
    fn uses_job_timing_when_transcript_belongs_to_background_job() {
        let session = SessionStats {
            tokens: 7,
            first_ms: Some(2000),
            last_ms: Some(3000),
            daily_tokens: vec![("1970-01-01".into(), 7)],
            model_usage: vec![(Some("test-model".into()), 7, 1)],
        };
        let stats = GlobalStats::from_sessions(vec![session], 3000, UTC);
        assert_eq!(stats.total_tokens, 7);
        assert_eq!(stats.longest_task_ms, 1000);
    }

    #[test]
    fn global_stats_cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let stats = GlobalStats {
            total_tokens: 42,
            daily_tokens: vec![DailyTokenActivity {
                date: "2026-07-25".into(),
                tokens: 42,
            }],
            model_usage: vec![ModelUsageStats {
                model: Some("test-model".into()),
                total_tokens: 42,
                request_count: 1,
            }],
            ..GlobalStats::default()
        };

        write_global_stats_cache(dir.path(), &stats).unwrap();

        assert_eq!(load_global_stats_cache(dir.path()), Some(stats));
    }

    #[test]
    fn invalid_global_stats_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = global_stats_cache_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not json").unwrap();

        assert_eq!(load_global_stats_cache(dir.path()), None);
    }

    #[test]
    fn global_stats_cache_accepts_missing_new_fields() {
        let stats: GlobalStats = serde_json::from_str(r#"{"total_tokens":7}"#).unwrap();

        assert_eq!(stats.total_tokens, 7);
        assert!(stats.daily_tokens.is_empty());
        assert!(stats.model_usage.is_empty());
    }

    #[test]
    fn parses_timezone_offsets() {
        assert_eq!(
            day_key_from_ms(parse_rfc3339_ms("2026-07-05T01:00:00+01:00").unwrap(), UTC),
            "2026-07-05"
        );
        assert_eq!(
            day_key_from_ms(parse_rfc3339_ms("2026-07-04T23:00:00-01:00").unwrap(), UTC),
            "2026-07-05"
        );
        // Same instant, bucketed by a UTC+8 wall clock.
        assert_eq!(
            day_key_from_ms(
                parse_rfc3339_ms("2026-07-05T01:00:00+01:00").unwrap(),
                UTC_PLUS_8
            ),
            "2026-07-05"
        );
        assert_eq!(
            day_key_from_ms(
                parse_rfc3339_ms("2026-07-04T20:00:00Z").unwrap(),
                UTC_PLUS_8
            ),
            "2026-07-05"
        );
    }

    #[test]
    fn scan_lease_throttles_recent_and_concurrent_scans() {
        let fresh = GlobalStats {
            total_tokens: 42,
            ..GlobalStats::default()
        };
        *lock_scan_clock() = GlobalStatsScanClock {
            in_flight: false,
            finished_at: None,
            latest: None,
        };

        let ScanClaim::Granted(mut lease) = ScanLease::claim(Duration::from_secs(300)) else {
            panic!("first claim with no prior scan should be granted");
        };
        // A second surface asking mid-scan waits rather than starting its own.
        assert!(matches!(
            ScanLease::claim(Duration::from_secs(300)),
            ScanClaim::Throttled(None)
        ));
        lease.record(fresh.clone());
        drop(lease);

        // Within the window the caller gets the completed scan, not a new one.
        assert!(matches!(
            ScanLease::claim(Duration::from_secs(300)),
            ScanClaim::Throttled(Some(stats)) if stats == fresh
        ));
        // Past it, scanning resumes.
        assert!(matches!(
            ScanLease::claim(Duration::ZERO),
            ScanClaim::Granted(_)
        ));

        *lock_scan_clock() = GlobalStatsScanClock {
            in_flight: false,
            finished_at: None,
            latest: None,
        };
    }
}
