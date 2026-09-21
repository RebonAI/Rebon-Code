//! The transactions that keep a job record consistent across processes.
//!
//! [`BackgroundStore`] owns the file layout and the locking; the free functions
//! above it are the job lifecycle — launch, queue, reply, respawn, stop — each
//! one a transaction rather than a read-modify-write a caller could interleave
//! with another process.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::*;
use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundRosterJob {
    pub job_id: String,
    pub session_id: Option<String>,
    pub cwd: String,
    pub status: BackgroundJobStatus,
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_identity: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub owner_detached_group: bool,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundRoster {
    pub supervisor_pid: u32,
    /// Which process instance that pid is, as
    /// [`process_identity`] records it (start time + boot id, or the
    /// platform equivalent).
    ///
    /// A pid alone cannot answer "is the supervisor still running": pids are
    /// reused, and a fresh roster naming a reused pid keeps
    /// `ensure_supervisor_running` from ever starting a replacement, so
    /// every queued job sits in `Queued` forever. Optional because rosters
    /// written before this field exist — those degrade to the pid-only
    /// check, exactly as they did before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_pid_identity: Option<String>,
    pub updated_at_ms: u64,
    pub jobs: Vec<BackgroundRosterJob>,
}

#[derive(Debug, Clone)]
pub struct BackgroundStore {
    root: PathBuf,
}

impl BackgroundStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The rebon config home this store reads/writes under. Background
    /// job data lives at `<root>/jobs/...` and the supervisor roster at
    /// `<root>/daemon/...`; subagent / dispatch resolution treats this as
    /// the user config home (`config_home_dir`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    pub fn job_dir(&self, job_id: &str) -> PathBuf {
        self.jobs_dir().join(job_id)
    }

    pub fn state_path(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join("state.json")
    }

    pub fn events_path(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join("events.jsonl")
    }

    pub fn log_path(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join("log.jsonl")
    }

    pub fn daemon_dir(&self) -> PathBuf {
        self.root.join("daemon")
    }

    pub fn daemon_log_path(&self) -> PathBuf {
        self.root.join("daemon.log")
    }

    pub fn supervisor_lock_path(&self) -> PathBuf {
        self.daemon_dir().join("supervisor.lock")
    }

    /// When a supervisor was last spawned, shared by every process that
    /// might spawn one. See [`supervisor_spawn_is_cooling_down`].
    pub fn supervisor_spawn_stamp_path(&self) -> PathBuf {
        self.daemon_dir().join("supervisor-spawn.stamp")
    }

    pub fn roster_path(&self) -> PathBuf {
        self.daemon_dir().join("roster.json")
    }

    pub fn ipc_addr(port: u16) -> String {
        format!("127.0.0.1:{port}")
    }

    #[allow(dead_code)]
    pub fn create_job(
        &self,
        prompt: String,
        cwd: PathBuf,
        runtime: BackgroundRuntimeFields,
    ) -> anyhow::Result<BackgroundJobState> {
        self.create_job_with_name(prompt, cwd, runtime, None)
    }

    pub fn create_job_with_name(
        &self,
        prompt: String,
        cwd: PathBuf,
        runtime: BackgroundRuntimeFields,
        name: Option<String>,
    ) -> anyhow::Result<BackgroundJobState> {
        self.create_job_with_parent(prompt, cwd, runtime, name, None)
    }

    /// Create a job, recording its parent in the **first** state write.
    ///
    /// Ownership has to be true from the moment a job is visible. The
    /// parent link used to be added by a follow-up update, which left a
    /// window where `list_jobs` showed the job as nobody's child: a parent
    /// stop walking the tree in that window would skip it, and the job
    /// would outlive the worker that started it — silently, and against
    /// the rule that everything a worker starts goes with it.
    pub fn create_job_with_parent(
        &self,
        prompt: String,
        cwd: PathBuf,
        runtime: BackgroundRuntimeFields,
        name: Option<String>,
        parent_job_id: Option<String>,
    ) -> anyhow::Result<BackgroundJobState> {
        // Only an all-whitespace prompt is invalid. A character minimum would
        // reject legitimate short prompts — "你好" or "hi" are complete
        // messages, and CJK packs a full request into very few chars.
        if prompt.trim().is_empty() {
            anyhow::bail!("background prompt is empty");
        }
        let mut state =
            BackgroundJobState::new(prompt, cwd.to_string_lossy().to_string(), runtime, name);
        state.identity.parent_job_id = parent_job_id;
        fs::create_dir_all(self.job_dir(&state.identity.job_id))?;
        self.write_state(&state)?;
        self.append_event(
            &state.identity.job_id,
            "created",
            serde_json::json!({
                "cwd": state.identity.cwd,
                "name": state.identity.name,
                "imageCount": state.identity.prompt_images.len(),
                "parentJobId": state.identity.parent_job_id,
            }),
        )?;
        Ok(state)
    }

    pub fn read_state(&self, job_id: &str) -> anyhow::Result<BackgroundJobState> {
        validate_job_id(job_id)?;
        let data = fs::read_to_string(self.state_path(job_id))
            .with_context(|| format!("failed to read background job state for {job_id}"))?;
        let mut state: BackgroundJobState = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse background job state for {job_id}"))?;
        state
            .normalize_pending_prompts()
            .with_context(|| format!("invalid pending prompt state for {job_id}"))?;
        Ok(state)
    }

    pub fn update_state<R>(
        &self,
        job_id: &str,
        update: impl FnOnce(&mut BackgroundJobState) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        validate_job_id(job_id)?;
        let dir = self.job_dir(job_id);
        fs::create_dir_all(&dir)?;
        let _lock = self.lock_job_state(job_id)?;
        let path = self.state_path(job_id);
        let data = fs::read_to_string(&path)
            .with_context(|| format!("failed to read background job state for {job_id}"))?;
        let mut state: BackgroundJobState = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse background job state for {job_id}"))?;
        state
            .normalize_pending_prompts()
            .with_context(|| format!("invalid pending prompt state for {job_id}"))?;
        let result = update(&mut state)?;
        state.validate_pending_prompts()?;
        self.write_state_unlocked(&state)?;
        Ok(result)
    }

    pub fn write_state(&self, state: &BackgroundJobState) -> anyhow::Result<()> {
        validate_job_id(&state.identity.job_id)?;
        let dir = self.job_dir(&state.identity.job_id);
        fs::create_dir_all(&dir)?;
        let _lock = self.lock_job_state(&state.identity.job_id)?;
        self.write_state_unlocked(state)
    }

    fn write_state_unlocked(&self, state: &BackgroundJobState) -> anyhow::Result<()> {
        state.validate_pending_prompts()?;
        let payload = serde_json::to_string_pretty(state)?;
        let path = self.state_path(&state.identity.job_id);
        rebon_session::write_file_atomically(&path, format!("{payload}\n").as_bytes())
            .with_context(|| {
                format!(
                    "failed to atomically replace background job state at {}",
                    path.display()
                )
            })?;
        Ok(())
    }

    fn lock_job_state(&self, job_id: &str) -> anyhow::Result<File> {
        validate_job_id(job_id)?;
        let dir = self.job_dir(job_id);
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(dir.join("state.lock"))?;
        FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    fn lock_job_events_exclusive(&self, job_id: &str) -> anyhow::Result<File> {
        validate_job_id(job_id)?;
        let dir = self.job_dir(job_id);
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(dir.join("events.lock"))?;
        FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    fn lock_job_events_shared(&self, job_id: &str) -> anyhow::Result<Option<File>> {
        validate_job_id(job_id)?;
        let dir = self.job_dir(job_id);
        if !dir.is_dir() {
            return Ok(None);
        }
        let file = match OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(dir.join("events.lock"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        FileExt::lock_shared(&file)?;
        Ok(Some(file))
    }

    fn read_events_snapshot(&self, job_id: &str) -> anyhow::Result<Vec<u8>> {
        validate_job_id(job_id)?;
        let path = self.events_path(job_id);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let Some(_lock) = self.lock_job_events_shared(job_id)? else {
            return Ok(Vec::new());
        };
        match fs::read(path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn lock_roster(&self) -> anyhow::Result<File> {
        fs::create_dir_all(self.daemon_dir())?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.daemon_dir().join("roster.lock"))?;
        FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    fn append_event_record(&self, job_id: &str, event: &BackgroundJobEvent) -> anyhow::Result<()> {
        let _lock = self.lock_job_events_exclusive(job_id)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path(job_id))?;
        writeln!(file, "{}", serde_json::to_string(event)?)?;
        Ok(())
    }

    pub fn append_event(
        &self,
        job_id: &str,
        kind: &str,
        data: serde_json::Value,
    ) -> anyhow::Result<()> {
        validate_job_id(job_id)?;
        fs::create_dir_all(self.job_dir(job_id))?;
        let event = BackgroundJobEvent {
            timestamp_ms: now_ms(),
            kind: kind.to_string(),
            data,
        };
        self.append_event_record(job_id, &event)?;
        let _ = self.update_state(job_id, |state| {
            state.outcome.event_count = state.outcome.event_count.saturating_add(1);
            state.process.updated_at_ms = state.process.updated_at_ms.max(event.timestamp_ms);
            Ok(())
        });
        Ok(())
    }

    pub fn append_log_line(&self, job_id: &str, line: &str) -> anyhow::Result<()> {
        validate_job_id(job_id)?;
        fs::create_dir_all(self.job_dir(job_id))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(job_id))?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn append_task_event_batch(
        &self,
        job_id: &str,
        batch: &BackgroundTaskEventBatch,
    ) -> anyhow::Result<()> {
        self.append_event(job_id, "task_live_batch", serde_json::to_value(batch)?)
    }

    pub fn append_session_update(
        &self,
        job_id: &str,
        update: &rebon_types::SessionUpdateParams,
    ) -> anyhow::Result<()> {
        self.append_session_update_with_generation(job_id, None, update, None)
    }

    pub fn append_session_update_for_turn(
        &self,
        job_id: &str,
        turn_generation: u64,
        update: &rebon_types::SessionUpdateParams,
    ) -> anyhow::Result<()> {
        self.append_session_update_with_generation(job_id, Some(turn_generation), update, None)
    }

    /// Log an update the owner has already published to its stream at
    /// `stamp`, so a client reading the log can line it up with the stream.
    pub fn append_session_update_for_turn_at(
        &self,
        job_id: &str,
        turn_generation: u64,
        update: &rebon_types::SessionUpdateParams,
        stamp: StreamStamp,
    ) -> anyhow::Result<()> {
        self.append_session_update_with_generation(
            job_id,
            Some(turn_generation),
            update,
            Some(stamp),
        )
    }

    fn append_session_update_with_generation(
        &self,
        job_id: &str,
        turn_generation: Option<u64>,
        update: &rebon_types::SessionUpdateParams,
        stamp: Option<StreamStamp>,
    ) -> anyhow::Result<()> {
        self.append_event(
            job_id,
            "session_update",
            session_update_event_data(update, turn_generation, stamp),
        )?;
        if let Some(summary) = summarize_session_update(&update.update) {
            let now = now_ms();
            let updated = self.update_state(job_id, |state| {
                if state.process.status != BackgroundJobStatus::Running {
                    return Ok(false);
                }
                let should_update = state
                    .outcome
                    .summary_updated_at_ms
                    .map(|last| now.saturating_sub(last) >= BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS)
                    .unwrap_or(true);
                if !should_update {
                    return Ok(false);
                }
                state.outcome.summary = Some(summary.clone());
                state.outcome.summary_updated_at_ms = Some(now);
                state.process.updated_at_ms = now;
                Ok(true)
            })?;
            if updated {
                self.append_event(
                    job_id,
                    "summary_updated",
                    serde_json::json!({ "summary": summary }),
                )?;
            }
        }
        Ok(())
    }

    pub fn open_log_for_append(&self, job_id: &str) -> anyhow::Result<File> {
        validate_job_id(job_id)?;
        fs::create_dir_all(self.job_dir(job_id))?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(job_id))
            .with_context(|| format!("failed to open background job log for {job_id}"))
    }

    pub fn open_daemon_log_for_append(&self) -> anyhow::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.daemon_log_path())
            .context("failed to open background supervisor log")
    }

    pub fn read_roster(&self) -> anyhow::Result<BackgroundRoster> {
        let data =
            fs::read_to_string(self.roster_path()).context("failed to read supervisor roster")?;
        serde_json::from_str(&data).context("failed to parse supervisor roster")
    }

    pub fn write_roster(&self, roster: &BackgroundRoster) -> anyhow::Result<()> {
        fs::create_dir_all(self.daemon_dir())?;
        let path = self.roster_path();
        let payload = serde_json::to_string_pretty(roster)?;
        let _lock = self.lock_roster()?;
        rebon_session::write_file_atomically(&path, format!("{payload}\n").as_bytes())
            .with_context(|| {
                format!("failed to atomically replace roster at {}", path.display())
            })?;
        Ok(())
    }

    pub fn read_events(&self, job_id: &str) -> anyhow::Result<Vec<BackgroundJobEvent>> {
        let snapshot = self.read_events_snapshot(job_id)?;
        Ok(parse_complete_event_lines(&snapshot))
    }

    pub fn read_task_snapshots(&self, job_id: &str) -> anyhow::Result<Vec<BackgroundTaskSnapshot>> {
        Ok(project_background_task_snapshots(
            &self.read_events(job_id)?,
        ))
    }

    /// Current byte length of the events log — the offset a live-follow
    /// consumer starts from when it wants "new events only". This is a
    /// metadata-only observation and never creates job storage.
    pub fn events_len(&self, job_id: &str) -> anyhow::Result<u64> {
        validate_job_id(job_id)?;
        match fs::metadata(self.events_path(job_id)) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    /// Incrementally read events appended after `byte_offset` (a value
    /// returned by a previous call; start from 0). Returns the parsed events
    /// plus the offset to resume from. Restarts from the beginning when the
    /// file shrank below the offset (job directory recreated). The returned
    /// offset only advances through complete newline-terminated records, so a
    /// concurrent or interrupted partial append can be retried.
    pub fn read_events_from_offset(
        &self,
        job_id: &str,
        byte_offset: u64,
    ) -> anyhow::Result<(Vec<BackgroundJobEvent>, u64)> {
        use std::io::{Read as _, Seek as _, SeekFrom};

        validate_job_id(job_id)?;
        let path = self.events_path(job_id);
        if !path.is_file() {
            return Ok((Vec::new(), 0));
        }
        let (remaining, start) = {
            let Some(_lock) = self.lock_job_events_shared(job_id)? else {
                return Ok((Vec::new(), 0));
            };
            let mut file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok((Vec::new(), 0));
                }
                Err(error) => return Err(error.into()),
            };
            let len = file.metadata()?.len();
            let start = if byte_offset <= len { byte_offset } else { 0 };
            file.seek(SeekFrom::Start(start))?;
            let mut remaining =
                Vec::with_capacity(usize::try_from(len.saturating_sub(start)).unwrap_or(0));
            file.read_to_end(&mut remaining)?;
            (remaining, start)
        };
        let Some(last_newline) = remaining.iter().rposition(|byte| *byte == b'\n') else {
            return Ok((Vec::new(), start));
        };
        let consumed = &remaining[..=last_newline];
        let next_offset = start.saturating_add(consumed.len() as u64);
        Ok((parse_complete_event_lines(consumed), next_offset))
    }

    /// Bounded variant of [`Self::read_events_from_offset`]: reads roughly
    /// `max_bytes` past `byte_offset`, always ending on a complete
    /// newline-terminated record, so callers can stream an arbitrarily large
    /// events log through a fixed-size window instead of materializing it
    /// whole. A single record longer than `max_bytes` is still returned in
    /// full (the budget is checked between lines), so every call with data
    /// available makes progress. Returns the parsed events plus the offset to
    /// resume from; a call that returns the offset it was given has reached
    /// the end of the complete records. Unlike the unbounded read, a file that
    /// shrank below `byte_offset` is NOT silently restarted — the call returns
    /// `(no events, 0)` so a streaming caller can detect the regression
    /// (`returned offset < byte_offset`) and rebuild its accumulated state
    /// from scratch instead of mixing two file generations.
    pub fn read_events_from_offset_bounded(
        &self,
        job_id: &str,
        byte_offset: u64,
        max_bytes: u64,
    ) -> anyhow::Result<(Vec<BackgroundJobEvent>, u64)> {
        use std::io::{BufRead as _, BufReader, Seek as _, SeekFrom};

        validate_job_id(job_id)?;
        let path = self.events_path(job_id);
        if !path.is_file() {
            return Ok((Vec::new(), 0));
        }
        let Some(_lock) = self.lock_job_events_shared(job_id)? else {
            return Ok((Vec::new(), 0));
        };
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), 0));
            }
            Err(error) => return Err(error.into()),
        };
        let len = file.metadata()?.len();
        if byte_offset > len {
            return Ok((Vec::new(), 0));
        }
        let start = byte_offset;
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        let mut consumed = 0u64;
        let mut line = Vec::new();
        while consumed < max_bytes {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 || line.last() != Some(&b'\n') {
                break; // EOF or an incomplete (still being appended) tail record
            }
            consumed = consumed.saturating_add(read as u64);
            events.extend(parse_complete_event_lines(&line));
        }
        Ok((events, start.saturating_add(consumed)))
    }

    pub fn read_events_tail(
        &self,
        job_id: &str,
        max_lines: usize,
    ) -> anyhow::Result<Vec<BackgroundJobEvent>> {
        validate_job_id(job_id)?;
        if max_lines == 0 {
            return Ok(Vec::new());
        }
        let path = self.events_path(job_id);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let lines = {
            let Some(_lock) = self.lock_job_events_shared(job_id)? else {
                return Ok(Vec::new());
            };
            read_tail_lines(&path, max_lines)?
        };
        Ok(lines
            .into_iter()
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect())
    }

    pub fn read_log_tail(&self, job_id: &str, max_lines: usize) -> anyhow::Result<Vec<String>> {
        validate_job_id(job_id)?;
        read_tail_lines(&self.log_path(job_id), max_lines)
    }

    pub fn read_peek_lines(&self, job_id: &str, max_lines: usize) -> Vec<String> {
        if validate_job_id(job_id).is_err() {
            return Vec::new();
        }
        let mut lines = Vec::new();
        let state = self.read_state(job_id).ok();
        if let Some(snapshot) = state
            .as_ref()
            .and_then(|s| s.outcome.pending_permission.as_ref())
        {
            let tool = snapshot.tool.as_deref().unwrap_or("tool");
            lines.push(format!(
                "permission query {}: allow {tool}?",
                snapshot.query_id
            ));
            for (idx, option) in snapshot.options.iter().enumerate() {
                lines.push(format!(
                    "  {}. {} ({})",
                    idx + 1,
                    option.label,
                    option.option_id
                ));
            }
        }
        if let Some(state_ref) = state.as_ref() {
            let preview_budget = max_lines.saturating_sub(lines.len()).min(5);
            if preview_budget > 0 {
                for line in read_transcript_preview(state_ref, preview_budget) {
                    if lines.len() >= max_lines {
                        break;
                    }
                    lines.push(line);
                }
            }
        }
        let events_budget = max_lines.saturating_sub(lines.len()).min(max_lines / 3);
        if events_budget > 0 {
            if let Ok(events) = self.read_events_tail(job_id, events_budget) {
                for event in events {
                    if event.kind == "permission_requested" {
                        continue;
                    }
                    if lines.len() >= max_lines {
                        break;
                    }
                    lines.push(format!("event {}: {}", event.timestamp_ms, event.kind));
                }
            }
        }
        if let Ok(logs) = self.read_log_tail(job_id, max_lines.saturating_sub(lines.len())) {
            for log in logs {
                if lines.len() >= max_lines {
                    break;
                }
                lines.push(format!("log: {log}"));
            }
        }
        lines
    }

    /// Cancel a job's **in-flight turn**, leaving the worker resident.
    ///
    /// This is turn cancellation, not job termination: the worker's current
    /// prompt is cancelled, the job lands `Idle`, and the same session can be
    /// continued afterwards. [`stop_job`](crate::stop_job) is the other one —
    /// it fences the job and ends the owner process.
    ///
    /// `Ok(false)` means there was no cancellable turn (the job was already
    /// terminal, or the worker said the turn had moved on). Callers must not
    /// quietly escalate that to a job stop: widening the blast radius because
    /// the narrow request found nothing is exactly how a "stop generating"
    /// button ends up killing a worker.
    ///
    /// Lives here rather than in the CLI so the desktop supervisor and
    /// `rebon stop --turn` share one implementation; the desktop previously had
    /// no route to it at all and reached for `rebon stop` instead.
    pub fn cancel_background_job_turn(&self, job_id: &str) -> anyhow::Result<bool> {
        // The cancel fence snapshots status/turn_generation/pending-permission —
        // the identity of the turn we observed, not `updated_at_ms`, which every
        // appended event bumps. A discrete transition landing between our read
        // and the worker handling the request (turn claimed, permission raised
        // or answered) still rejects with "turn changed"; re-read and retry
        // against the fresh fence instead of reporting the turn as
        // uncancellable.
        const FENCE_RETRIES: usize = 3;
        for _ in 0..FENCE_RETRIES {
            let mut state = self.read_state(job_id)?;
            self.reconcile_stale_pid(&mut state)?;
            if !matches!(
                state.process.status,
                BackgroundJobStatus::Queued
                    | BackgroundJobStatus::Running
                    | BackgroundJobStatus::NeedsInput
            ) && state.outcome.pending_permission.is_none()
            {
                return Ok(false);
            }
            let port = state
                .process
                .ipc_port
                .context("background job has no live IPC endpoint")?;
            let token = state
                .process
                .ipc_token
                .clone()
                .context("background job has a live IPC endpoint without a token")?;
            match send_background_ipc_request(
                &state,
                port,
                token,
                BackgroundIpcRequest::cancel_for(&state),
            ) {
                Ok(()) => {
                    self.append_event(job_id, "turn_cancel_sent", serde_json::json!({}))?;
                    return Ok(true);
                }
                Err(err) if err.to_string() == "background turn changed before cancellation" => {
                    continue;
                }
                Err(err) if err.to_string() == "background turn is no longer cancellable" => {
                    return Ok(false);
                }
                Err(err) => return Err(err),
            }
        }
        // Exhausting the retries means the job's turn state flipped on every
        // attempt. Reporting `Ok(false)` here is how a stop that did nothing
        // was once reported as accepted, leaving the caller to time out
        // waiting for a confirmation that could never come. Fail loudly so the
        // user is told immediately instead of thirty seconds later.
        anyhow::bail!("background turn kept changing while cancelling; press stop again")
    }

    /// Cancel one task through the existing `CancelTasks` IPC path. Single-task
    /// requests surface the coordinator's unknown/terminal task errors.
    pub fn cancel_background_task(&self, job_id: &str, task_id: String) -> anyhow::Result<()> {
        if task_id.is_empty() {
            anyhow::bail!("background task id is empty");
        }
        self.cancel_background_tasks(job_id, vec![task_id])?;
        Ok(())
    }

    pub fn cancel_background_tasks(
        &self,
        job_id: &str,
        task_ids: Vec<String>,
    ) -> anyhow::Result<bool> {
        if task_ids.is_empty() {
            return Ok(false);
        }
        let mut state = self.read_state(job_id)?;
        self.reconcile_stale_pid(&mut state)?;
        let port = state
            .process
            .ipc_port
            .context("background job has no live IPC endpoint")?;
        let token = state
            .process
            .ipc_token
            .clone()
            .context("background job has a live IPC endpoint without a token")?;
        send_background_ipc_request(
            &state,
            port,
            token,
            BackgroundIpcRequest::CancelTasks {
                task_ids: task_ids.clone(),
            },
        )?;
        self.append_event(
            job_id,
            "task_cancel_sent",
            serde_json::json!({ "taskIds": task_ids }),
        )?;
        Ok(true)
    }

    pub fn reply_to_background_task(
        &self,
        job_id: &str,
        task_id: String,
        message: String,
    ) -> anyhow::Result<()> {
        let message = message.trim().to_string();
        if message.is_empty() {
            anyhow::bail!("background task reply is empty");
        }
        let mut state = self.read_state(job_id)?;
        self.reconcile_stale_pid(&mut state)?;
        let port = state
            .process
            .ipc_port
            .context("background job has no live IPC endpoint")?;
        let token = state
            .process
            .ipc_token
            .clone()
            .context("background job has a live IPC endpoint without a token")?;
        send_background_ipc_request(
            &state,
            port,
            token,
            BackgroundIpcRequest::ReplyTask {
                task_id: task_id.clone(),
                message: message.clone(),
            },
        )?;
        self.append_event(
            job_id,
            "task_reply_sent",
            serde_json::json!({ "taskId": task_id, "message": message }),
        )?;
        Ok(())
    }

    pub fn answer_permission_query(
        &self,
        job_id: &str,
        query_id: u64,
        option_id: Option<String>,
        extra_text: Option<String>,
    ) -> anyhow::Result<()> {
        self.answer_permission_query_with_updated_input(
            job_id, query_id, option_id, extra_text, None,
        )
    }

    pub fn answer_permission_query_with_updated_input(
        &self,
        job_id: &str,
        query_id: u64,
        option_id: Option<String>,
        extra_text: Option<String>,
        updated_input: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.answer_permission_query_for_endpoint_with_updated_input(
            job_id,
            query_id,
            None,
            option_id,
            extra_text,
            updated_input,
        )
    }

    pub fn answer_permission_query_for_endpoint_with_updated_input(
        &self,
        job_id: &str,
        query_id: u64,
        expected_endpoint: Option<&BackgroundIpcEndpoint>,
        option_id: Option<String>,
        extra_text: Option<String>,
        updated_input: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.answer_permission_query_for_target_with_updated_input(
            job_id,
            query_id,
            None,
            expected_endpoint,
            option_id,
            extra_text,
            updated_input,
        )
    }

    pub fn answer_permission_query_for_target_with_updated_input(
        &self,
        job_id: &str,
        query_id: u64,
        expected_turn_generation: Option<u64>,
        expected_endpoint: Option<&BackgroundIpcEndpoint>,
        option_id: Option<String>,
        extra_text: Option<String>,
        updated_input: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let mut state = self.read_state(job_id)?;
        self.reconcile_stale_pid(&mut state)?;
        let port = state.process.ipc_port;
        let token = state.process.ipc_token.clone();
        let Some(snapshot) = state.outcome.pending_permission.clone() else {
            anyhow::bail!("background job {job_id} has no pending permission query");
        };
        if snapshot.query_id != query_id {
            anyhow::bail!("permission query {query_id} is not pending for background job {job_id}");
        }
        validate_permission_target(
            &state,
            &snapshot,
            expected_turn_generation,
            expected_endpoint,
        )?;
        if let Some(port) = port {
            let token = token.context("background job has a live IPC endpoint without a token")?;
            send_background_ipc_request(
                &state,
                port,
                token,
                BackgroundIpcRequest::PermissionAnswer {
                    query_id,
                    turn_generation: snapshot.turn_generation,
                    option_id,
                    extra_text,
                    updated_input,
                },
            )?;
        } else {
            // A permission is asked by the process running the turn, and
            // only a worker with an endpoint can be answered from outside
            // it. A turn that runs in a terminal answers there.
            anyhow::bail!(
                "background job {job_id} has no live worker endpoint to answer permission query {query_id} through"
            );
        }
        self.append_event(
            job_id,
            "permission_answer_sent",
            serde_json::json!({
                "queryId": query_id,
            }),
        )?;
        Ok(())
    }

    pub fn answer_question_query(
        &self,
        job_id: &str,
        query_id: u64,
        answers: Vec<ForegroundQuestionAnswer>,
    ) -> anyhow::Result<()> {
        self.answer_question_query_for_endpoint(job_id, query_id, None, answers)
    }

    pub fn answer_question_query_for_endpoint(
        &self,
        job_id: &str,
        query_id: u64,
        expected_endpoint: Option<&BackgroundIpcEndpoint>,
        answers: Vec<ForegroundQuestionAnswer>,
    ) -> anyhow::Result<()> {
        self.answer_question_query_for_target(job_id, query_id, None, expected_endpoint, answers)
    }

    pub fn answer_question_query_for_target(
        &self,
        job_id: &str,
        query_id: u64,
        expected_turn_generation: Option<u64>,
        expected_endpoint: Option<&BackgroundIpcEndpoint>,
        answers: Vec<ForegroundQuestionAnswer>,
    ) -> anyhow::Result<()> {
        let mut state = self.read_state(job_id)?;
        self.reconcile_stale_pid(&mut state)?;
        let Some(snapshot) = state.outcome.pending_permission.clone() else {
            anyhow::bail!("background job {job_id} has no pending permission query");
        };
        if snapshot.query_id != query_id {
            anyhow::bail!("question query {query_id} is not pending for background job {job_id}");
        }
        validate_permission_target(
            &state,
            &snapshot,
            expected_turn_generation,
            expected_endpoint,
        )?;
        // Answers that do not fit the question are refused here, before
        // anything is sent; the worker builds the same input on its side.
        build_ask_user_question_updated_input(&snapshot, &answers)?;
        if !snapshot
            .options
            .iter()
            .any(|option| option.option_id == "allow_once")
        {
            anyhow::bail!("question query {query_id} has no allow_once option");
        }

        if let Some(port) = state.process.ipc_port {
            let token = state
                .process
                .ipc_token
                .clone()
                .context("background job has a live IPC endpoint without a token")?;
            send_background_ipc_request(
                &state,
                port,
                token,
                BackgroundIpcRequest::AnswerQuestions {
                    query_id,
                    turn_generation: snapshot.turn_generation,
                    answers,
                },
            )?;
        } else {
            anyhow::bail!(
                "background job {job_id} has no live worker endpoint to answer question query {query_id} through"
            );
        }
        self.append_event(
            job_id,
            "question_answer_sent",
            serde_json::json!({ "queryId": query_id }),
        )?;
        Ok(())
    }

    pub fn answer_latest_permission_option(
        &self,
        job_id: &str,
        option_index: usize,
    ) -> anyhow::Result<()> {
        let state = self.read_state(job_id)?;
        let Some(snapshot) = state.outcome.pending_permission else {
            anyhow::bail!("background job {job_id} has no pending permission query");
        };
        let Some(option) = snapshot.options.get(option_index) else {
            anyhow::bail!(
                "permission query {} has no option {}",
                snapshot.query_id,
                option_index + 1
            );
        };
        self.answer_permission_query_for_target_with_updated_input(
            job_id,
            snapshot.query_id,
            Some(snapshot.turn_generation),
            snapshot.endpoint.as_ref(),
            Some(option.option_id.clone()),
            None,
            None,
        )
    }

    pub fn list_jobs(&self) -> anyhow::Result<Vec<BackgroundJobState>> {
        let dir = self.jobs_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let job_id = entry.file_name().to_string_lossy().to_string();
            // Event-only directories (notably the supervisor log stream) have
            // no state file and are not jobs. Once state.json exists, however,
            // silently dropping a read/parse failure would make supervision and
            // update quiescence publish an incomplete process tree.
            if !self.state_path(&job_id).is_file() {
                continue;
            }
            let mut state = self.read_state(&job_id).with_context(|| {
                format!("failed to enumerate background job state for {job_id}")
            })?;
            if state.identity.sort_order == 0 {
                state.identity.sort_order = state.process.created_at_ms as i64;
            }
            self.reconcile_stale_pid(&mut state)?;
            jobs.push(state);
        }
        jobs.sort_by(|a, b| {
            b.identity
                .pinned
                .cmp(&a.identity.pinned)
                .then_with(|| b.identity.sort_order.cmp(&a.identity.sort_order))
                .then_with(|| b.process.updated_at_ms.cmp(&a.process.updated_at_ms))
        });
        Ok(jobs)
    }

    pub fn update_job_presentation(
        &self,
        job_id: &str,
        name: Option<String>,
        pinned: Option<bool>,
        sort_order: Option<i64>,
    ) -> anyhow::Result<BackgroundJobState> {
        validate_job_id(job_id)?;
        let state = self.update_state(job_id, |state| {
            if let Some(name) = name.and_then(|name| non_empty_trimmed(&name)) {
                state.identity.name = name;
            }
            if let Some(pinned) = pinned {
                state.identity.pinned = pinned;
            }
            if let Some(sort_order) = sort_order {
                state.identity.sort_order = sort_order;
            } else if state.identity.sort_order == 0 {
                state.identity.sort_order = state.process.created_at_ms as i64;
            }
            state.process.updated_at_ms = now_ms();
            Ok(state.clone())
        })?;
        self.append_event(
            job_id,
            "presentation_updated",
            serde_json::json!({
                "name": state.identity.name,
                "pinned": state.identity.pinned,
                "sortOrder": state.identity.sort_order,
            }),
        )?;
        Ok(state)
    }

    pub fn move_job_before(&self, job_id: &str, before_job_id: &str) -> anyhow::Result<()> {
        validate_job_id(job_id)?;
        validate_job_id(before_job_id)?;
        if job_id == before_job_id {
            return Ok(());
        }
        let mut jobs = self.list_jobs()?;
        let Some(from) = jobs.iter().position(|job| job.identity.job_id == job_id) else {
            anyhow::bail!("background job {job_id} not found");
        };
        let Some(to) = jobs
            .iter()
            .position(|job| job.identity.job_id == before_job_id)
        else {
            anyhow::bail!("background job {before_job_id} not found");
        };
        let job = jobs.remove(from);
        let to = if from < to { to.saturating_sub(1) } else { to };
        jobs.insert(to, job);
        self.rewrite_sort_orders(&jobs)
    }

    pub fn move_job_after(&self, job_id: &str, after_job_id: &str) -> anyhow::Result<()> {
        validate_job_id(job_id)?;
        validate_job_id(after_job_id)?;
        if job_id == after_job_id {
            return Ok(());
        }
        let mut jobs = self.list_jobs()?;
        let Some(from) = jobs.iter().position(|job| job.identity.job_id == job_id) else {
            anyhow::bail!("background job {job_id} not found");
        };
        let Some(to) = jobs
            .iter()
            .position(|job| job.identity.job_id == after_job_id)
        else {
            anyhow::bail!("background job {after_job_id} not found");
        };
        let job = jobs.remove(from);
        let to = if from < to { to } else { to.saturating_add(1) };
        jobs.insert(to.min(jobs.len()), job);
        self.rewrite_sort_orders(&jobs)
    }

    fn rewrite_sort_orders(&self, jobs: &[BackgroundJobState]) -> anyhow::Result<()> {
        let base = now_ms() as i64;
        for (index, job) in jobs.iter().enumerate() {
            let sort_order = base.saturating_sub(index as i64);
            self.update_job_presentation(&job.identity.job_id, None, None, Some(sort_order))?;
        }
        Ok(())
    }

    fn reserve_job_removal(&self, job_id: &str) -> anyhow::Result<BackgroundJobState> {
        self.update_state(job_id, |state| {
            if state.process.process_owner_fenced {
                anyhow::bail!(
                    "background job {job_id} ownership is fenced until its recorded process exit is verified"
                );
            }
            if state.process.pid.is_some() || state.process.ipc_port.is_some() || state.process.ipc_token.is_some() {
                anyhow::bail!(
                    "background job {job_id} still has a recorded process owner; wait for its exit before removing metadata"
                );
            }
            if matches!(
                state.process.status,
                BackgroundJobStatus::Queued
                    | BackgroundJobStatus::Running
                    | BackgroundJobStatus::NeedsInput
                    | BackgroundJobStatus::Idle
            ) {
                anyhow::bail!(
                    "background job {job_id} is still active; stop it before removing metadata"
                );
            }
            if let Some(target_job_id) = state.identity.respawned_job_id.as_deref() {
                let target = self.read_state(target_job_id).with_context(|| {
                    format!(
                        "background job {job_id} respawn target {target_job_id} is not fully materialized; retry respawn before removing the source"
                    )
                })?;
                validate_respawned_job_materialization(state, &target, target_job_id)
                    .with_context(|| {
                        format!(
                            "background job {job_id} respawn target {target_job_id} is not fully materialized; retry respawn before removing the source"
                        )
                    })?;
            }
            state.process.removal_reserved = true;
            state.process.updated_at_ms = now_ms();
            Ok(state.clone())
        })
    }

    pub fn remove_job(&self, job_id: &str) -> anyhow::Result<()> {
        validate_job_id(job_id)?;
        let mut observed = self.read_state(job_id)?;
        self.reconcile_stale_pid(&mut observed)?;
        let state = self.reserve_job_removal(job_id)?;
        if let Some(path) = state
            .workspace
            .worktree_path
            .as_deref()
            .and_then(non_empty_trimmed)
        {
            let info = background_worktree_info_from_path(Path::new(&path), job_id);
            if let Some(info) = info {
                let removed = rebon_tool::worktree::remove_agent_worktree(&info);
                self.append_event(
                    job_id,
                    if removed {
                        "worktree_removed_on_delete"
                    } else {
                        "worktree_remove_on_delete_failed"
                    },
                    serde_json::json!({
                        "path": info.worktree_path,
                        "branch": info.worktree_branch,
                    }),
                )?;
            } else {
                self.append_event(
                    job_id,
                    "worktree_remove_on_delete_failed",
                    serde_json::json!({ "path": path, "reason": "could not resolve git root" }),
                )?;
            }
        }
        fs::remove_dir_all(self.job_dir(job_id))
            .with_context(|| format!("failed to remove background job {job_id}"))?;
        if let Ok(mut roster) = self.read_roster() {
            roster.jobs.retain(|job| job.job_id != job_id);
            roster.updated_at_ms = now_ms();
            let _ = self.write_roster(&roster);
        }
        Ok(())
    }

    pub fn reconcile_stale_pid(&self, state: &mut BackgroundJobState) -> anyhow::Result<()> {
        if state.process.process_owner_fenced {
            let Some(pid) = state.process.pid else {
                return Ok(());
            };
            let owner_exited = matches!(
                recorded_process_tree_is_running(
                    pid,
                    state.process.pid_identity.as_deref(),
                    state.process.owner_detached_group,
                ),
                Ok(false)
            );
            if !owner_exited {
                return Ok(());
            }
            let observed = state.clone();
            let observed_owner = observed.recorded_owner();
            let reconciled_at = now_ms();
            let (cleared, updated) = self.update_state(&observed.identity.job_id, |current| {
                if current.recorded_owner() != observed_owner
                    || current.process.spawn_admitted
                    || current.process.status != observed.process.status
                    || current.identity.session_id != observed.identity.session_id
                    || current.process.updated_at_ms != observed.process.updated_at_ms
                    || current.identity.pending_prompts != observed.identity.pending_prompts
                    || current.outcome.pending_permission != observed.outcome.pending_permission
                {
                    return Ok((false, current.clone()));
                }
                current.clear_recorded_owner();
                current.process.spawn_admitted = false;
                current.outcome.pending_permission = None;
                current.process.updated_at_ms = reconciled_at;
                Ok((true, current.clone()))
            })?;
            *state = updated;
            if cleared {
                self.append_event(
                    &state.identity.job_id,
                    "fenced_process_exit_reconciled",
                    serde_json::json!({ "pid": pid }),
                )?;
            }
            return Ok(());
        }
        // Deliberately not gated on the job's status. A recorded owner that has
        // provably exited is stale whatever the job says, and clearing it only
        // while the job was Running/NeedsInput stranded every job that reached a
        // terminal status *before* its worker died — a turn that failed on a
        // quota wall or a sub-agent error, then the worker going away without
        // ever handing back its owner record. Nothing else clears it, so the job
        // stays pinned to a dead pid forever: removal and respawn both refuse it
        // ("still has a recorded process owner"), and every IPC request keeps
        // dialing a port nobody is listening on, which is what surfaces as a
        // sub-agent that cannot be terminated.
        let observed_owner = state.recorded_owner();
        let Some(pid) = observed_owner.pid else {
            return Ok(());
        };
        let stale = match observed_owner.pid_identity.as_deref() {
            Some(_) => matches!(
                recorded_process_tree_is_running(
                    pid,
                    observed_owner.pid_identity.as_deref(),
                    observed_owner.owner_detached_group,
                ),
                Ok(false)
            ),
            None if observed_owner.owner_detached_group => false,
            None => process_is_running(pid) == Some(false),
        };
        if !stale {
            return Ok(());
        }
        let job_id = state.identity.job_id.clone();
        let updated = self.update_state(&job_id, |current| {
            // The owner comparison is the whole fence: it pins the exact
            // process record observed as exited, so a concurrent respawn that
            // recorded a fresh owner is never cleared out from under itself.
            if current.recorded_owner() != observed_owner {
                return Ok(None);
            }
            // A job still mid-turn loses that turn; one that had already
            // reached its own conclusion keeps it and only gives up the
            // dead owner record.
            if matches!(
                current.process.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
            ) {
                current.process.status = BackgroundJobStatus::Failed;
                current.outcome.error = Some(STALE_PID_EXIT_ERROR.to_string());
                current.process.completed_at_ms = Some(now_ms());
            }
            current.clear_recorded_owner();
            current.process.spawn_admitted = false;
            current.outcome.pending_permission = None;
            // Keep pending_prompts: the crashed worker never finished the
            // claimed reply, and a respawn re-runs it only while it remains
            // durably queued on the failed job.
            current.process.updated_at_ms = now_ms();
            Ok(Some(current.clone()))
        })?;
        let Some(updated) = updated else {
            return Ok(());
        };
        *state = updated;
        self.append_event(
            &state.identity.job_id,
            "stale_pid_reconciled",
            serde_json::json!({ "pid": pid }),
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct BackgroundLaunchOptions {
    pub prompt: String,
    pub images: Vec<BackgroundImageAttachment>,
    pub cwd: PathBuf,
    pub isolate_in_worktree: bool,
    /// Fail the job rather than fall back to the origin checkout when the
    /// isolated worktree cannot be prepared.
    pub require_worktree: bool,
    /// Keep the worktree + branch after a successful turn.
    pub preserve_worktree_on_success: bool,
    /// Grant Agent Queue control-plane tools to this background session.
    /// This is trusted launch metadata; it must never be inferred from prompt text.
    pub queue_session: bool,
    pub runtime: BackgroundRuntimeFields,
    pub name: Option<String>,
    pub agent_type: Option<String>,
    /// The worker this dispatch came from, if any. See
    /// [`BackgroundJobState::parent_job_id`].
    pub parent_job_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundDispatchPrompt {
    pub prompt: String,
    pub agent_type: Option<String>,
    pub cwd: Option<PathBuf>,
    pub skill_name: Option<String>,
}

pub(crate) fn ensure_background_job_ownership_reusable(
    state: &BackgroundJobState,
    job_id: &str,
) -> anyhow::Result<()> {
    if state.process.process_owner_fenced {
        anyhow::bail!(
            "background job {job_id} ownership is fenced until its recorded process exit is verified"
        );
    }
    if state.process.removal_reserved {
        anyhow::bail!("background job {job_id} is reserved for removal");
    }
    if let Some(respawned_job_id) = state.identity.respawned_job_id.as_deref() {
        anyhow::bail!("background job {job_id} was already respawned as {respawned_job_id}");
    }
    Ok(())
}

pub(crate) fn ensure_background_job_has_no_unfinished_follow_up(
    state: &BackgroundJobState,
    job_id: &str,
) -> anyhow::Result<()> {
    if state.has_pending_prompts() {
        anyhow::bail!("background job {job_id} already has accepted follow-ups awaiting recovery");
    }
    Ok(())
}

/// The job a session most recently ran under, if its record still exists.
///
/// Resuming replaces the job's whole `runtime` block, so a caller that means
/// "continue as before" has to read the previous values back rather than send
/// defaults — `capability_mode` has no "unset" wire form, and a default one
/// silently downgrades a Minimal session to Normal.
///
/// Every record counts, including one reserved for removal or already
/// respawned: the queue and adopt paths below reuse the answer and let the
/// ownership check refuse such a job. Where a session should be *attached*
/// or *resumed* is a different question — [`crate::home_job_for_session`].
pub fn latest_job_for_session(
    store: &BackgroundStore,
    session_id: &str,
) -> anyhow::Result<Option<BackgroundJobState>> {
    Ok(store
        .list_jobs()?
        .into_iter()
        .filter(|job| job.identity.session_id.as_deref() == Some(session_id))
        .max_by(|a, b| {
            a.process
                .updated_at_ms
                .cmp(&b.process.updated_at_ms)
                .then_with(|| a.process.created_at_ms.cmp(&b.process.created_at_ms))
        }))
}

pub fn queue_existing_background_session_with_images(
    store: &BackgroundStore,
    prompt: String,
    prompt_id: Option<String>,
    images: Vec<BackgroundImageAttachment>,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    session_id: String,
    name: Option<String>,
    rebon_exe_path: &Path,
) -> anyhow::Result<BackgroundJobState> {
    if let Some(existing) = latest_job_for_session(store, &session_id)? {
        let state = mark_existing_background_session_queued_with_images(
            store,
            &existing.identity.job_id,
            Some(prompt),
            prompt_id,
            images,
            cwd,
            runtime,
            session_id,
            name,
        )?;
        ensure_supervisor_running(store, rebon_exe_path)?;
        store.append_event(
            &state.identity.job_id,
            "queued_for_supervisor",
            serde_json::json!({ "resumed": true, "freshProcess": true }),
        )?;
        return Ok(state);
    }
    let pending_prompt = prompt_id
        .map(|id| PendingPrompt::new(id, prompt.clone(), images.clone(), now_ms()))
        .transpose()?;
    let mut state = store.create_job_with_name(prompt, cwd, runtime, name)?;
    let job_id = state.identity.job_id.clone();
    state = store.update_state(&job_id, |state| {
        state.identity.session_id = Some(session_id.clone());
        state.identity.prompt_images = if pending_prompt.is_some() {
            Vec::new()
        } else {
            images
        };
        state.process.status = BackgroundJobStatus::Queued;
        state.clear_recorded_owner();
        state.process.spawn_admitted = false;
        state.outcome.pending_permission = None;
        state.clear_pending_prompts();
        if let Some(prompt) = pending_prompt {
            state.append_pending_prompt(prompt)?;
        }
        state.outcome.exit_code = None;
        state.outcome.error = None;
        state.process.completed_at_ms = None;
        state.outcome.summary_updated_at_ms = None;
        state.process.updated_at_ms = now_ms();
        Ok(state.clone())
    })?;
    ensure_supervisor_running(store, rebon_exe_path)?;
    store.append_event(
        &state.identity.job_id,
        "session_backgrounded_queued",
        serde_json::json!({
            "sessionId": session_id,
            "freshProcess": true,
        }),
    )?;
    store.append_event(
        &state.identity.job_id,
        "queued_for_supervisor",
        serde_json::json!({ "resumed": true, "freshProcess": true }),
    )?;
    Ok(state)
}

pub fn mark_existing_background_session_queued(
    store: &BackgroundStore,
    job_id: &str,
    prompt: Option<String>,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    session_id: String,
    name: Option<String>,
) -> anyhow::Result<BackgroundJobState> {
    mark_existing_background_session_queued_with_images(
        store,
        job_id,
        prompt,
        None,
        Vec::new(),
        cwd,
        runtime,
        session_id,
        name,
    )
}

pub fn mark_existing_background_session_queued_with_images(
    store: &BackgroundStore,
    job_id: &str,
    prompt: Option<String>,
    prompt_id: Option<String>,
    prompt_images: Vec<BackgroundImageAttachment>,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    session_id: String,
    name: Option<String>,
) -> anyhow::Result<BackgroundJobState> {
    let name = name.and_then(|name| non_empty_trimmed(&name));
    let cwd = cwd.to_string_lossy().to_string();
    let queued_at = now_ms();
    let pending_prompt = prompt_id
        .map(|id| {
            let text = prompt
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("identified background prompt is missing text"))?;
            PendingPrompt::new(id, text.clone(), prompt_images.clone(), queued_at)
        })
        .transpose()?;
    let state = store.update_state(job_id, |state| {
        if state.identity.session_id.as_deref() != Some(session_id.as_str()) {
            anyhow::bail!("background job {job_id} does not belong to session {session_id}");
        }
        ensure_background_job_ownership_reusable(state, job_id)?;
        ensure_background_job_has_no_unfinished_follow_up(state, job_id)?;
        if matches!(
            state.process.status,
            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
        ) && state.process.pid != Some(std::process::id())
        {
            anyhow::bail!(
                "background job {job_id} is already active while status is {}",
                state.process.status.as_str()
            );
        }
        if let Some(prompt) = prompt.clone() {
            state.identity.prompt = prompt;
        }
        state.identity.prompt_images = if pending_prompt.is_some() {
            Vec::new()
        } else {
            prompt_images.clone()
        };
        state.identity.cwd = cwd.clone();
        state.identity.runtime = runtime.clone();
        if let Some(name) = name.clone() {
            state.identity.name = name;
        }
        state.process.status = BackgroundJobStatus::Queued;
        state.clear_recorded_owner();
        state.process.spawn_admitted = false;
        state.outcome.pending_permission = None;
        state.clear_pending_prompts();
        if let Some(prompt) = pending_prompt.clone() {
            state.append_pending_prompt(prompt)?;
        }
        state.outcome.exit_code = None;
        state.outcome.error = None;
        state.outcome.summary = None;
        state.process.completed_at_ms = None;
        state.outcome.summary_updated_at_ms = None;
        state.process.updated_at_ms = queued_at;
        Ok(state.clone())
    })?;
    store.append_event(
        job_id,
        "session_backgrounded_queued",
        serde_json::json!({
            "sessionId": session_id,
            "freshProcess": true,
            "reused": true,
        }),
    )?;
    Ok(state)
}

/// Turn a session a terminal is holding into a job a worker can own.
///
/// `placement` says whether a client will be sitting in front of the result:
/// a `/hosted` handover is going to be mirrored by the terminal that asked for
/// it, while `/bg` means the opposite in as many words. It decides how long
/// the worker lingers once nobody is watching.
pub fn adopt_existing_background_session(
    store: &BackgroundStore,
    prompt: String,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    session_id: String,
    name: Option<String>,
    start_supervisor: bool,
    placement: JobPlacement,
    rebon_exe_path: &Path,
) -> anyhow::Result<BackgroundJobState> {
    if let Some(existing) = latest_job_for_session(store, &session_id)? {
        let state = if start_supervisor {
            mark_existing_background_session_queued(
                store,
                &existing.identity.job_id,
                Some(prompt),
                cwd,
                runtime,
                session_id,
                name,
            )?
        } else {
            mark_existing_background_session_idle(
                store,
                &existing.identity.job_id,
                Some(prompt),
                cwd,
                runtime,
                session_id,
                name,
            )?
        };
        if start_supervisor {
            ensure_supervisor_running(store, rebon_exe_path)?;
            store.append_event(
                &state.identity.job_id,
                "queued_for_supervisor",
                serde_json::json!({ "resumed": true, "freshProcess": true }),
            )?;
        }
        let state = store.update_state(&existing.identity.job_id, |state| {
            state.lease.placement = placement;
            Ok(state.clone())
        })?;
        return Ok(state);
    }
    let mut state = store.create_job_with_name(prompt, cwd, runtime, name)?;
    let job_id = state.identity.job_id.clone();
    let updated = store.update_state(&job_id, |state| {
        state.lease.placement = placement;
        state.identity.session_id = Some(session_id.clone());
        state.process.status = if start_supervisor {
            BackgroundJobStatus::Queued
        } else {
            BackgroundJobStatus::Idle
        };
        state.process.updated_at_ms = now_ms();
        Ok(state.clone())
    })?;
    state = updated;
    store.append_event(
        &state.identity.job_id,
        "session_adopted",
        serde_json::json!({ "sessionId": session_id }),
    )?;
    if start_supervisor {
        ensure_supervisor_running(store, rebon_exe_path)?;
        store.append_event(
            &state.identity.job_id,
            "queued_for_supervisor",
            serde_json::json!({ "resumed": true, "freshProcess": true }),
        )?;
    }
    Ok(state)
}

/// Refuse to hang new work off a parent whose tree is already torn down.
///
/// An unreadable parent is not evidence of a teardown: the link is recorded
/// and the walk does what it can, which is the same answer as before this
/// check existed.
fn refuse_dispatch_from_a_stopped_parent(
    store: &BackgroundStore,
    parent: &str,
) -> anyhow::Result<()> {
    match store.read_state(parent) {
        Ok(state) if state.process.status == BackgroundJobStatus::Stopped => anyhow::bail!(
            "background job {parent} was stopped; work dispatched from it would outlive it"
        ),
        Ok(_) => Ok(()),
        Err(err) => {
            tracing::warn!(%err, parent, "could not check the parent job before dispatch");
            Ok(())
        }
    }
}

pub fn launch_background_prompt(
    store: &BackgroundStore,
    options: BackgroundLaunchOptions,
    rebon_exe_path: &Path,
    validate_permission_mode: impl FnOnce(&BackgroundRuntimeFields) -> anyhow::Result<()>,
) -> anyhow::Result<BackgroundJobState> {
    let BackgroundLaunchOptions {
        prompt,
        images,
        cwd,
        isolate_in_worktree,
        require_worktree,
        preserve_worktree_on_success,
        queue_session,
        mut runtime,
        name,
        agent_type,
        parent_job_id,
    } = options;
    let agent_type = agent_type.and_then(|agent| non_empty_trimmed(&agent));
    let agent_runtime_applied = match agent_type.as_deref() {
        Some(agent) => {
            apply_subagent_frontmatter_to_runtime(&mut runtime, &cwd, agent, store.root())
        }
        None => None,
    };
    validate_permission_mode(&runtime)?;
    // A tree that is being torn down must not grow. The stop walk visits
    // each job once, so a child dispatched after its parent was stopped is
    // never reached — it would outlive the worker that started it, which is
    // exactly the ownership rule the parent link exists to enforce. The
    // worker asking is on its way out anyway, so this is refused rather
    // than created and immediately killed.
    if let Some(parent) = parent_job_id.as_deref() {
        refuse_dispatch_from_a_stopped_parent(store, parent)?;
    }
    // The parent link rides the create, not a follow-up update: a job must
    // never be visible to a parent's stop walk without it.
    let mut state =
        store.create_job_with_parent(prompt, cwd, runtime, name, parent_job_id.clone())?;
    let job_id = state.identity.job_id.clone();
    // The check above cannot be atomic with the create — they are two
    // files — so it is made good afterwards instead: if the parent was
    // stopped while this job was being written, the walk that stopped it
    // has already passed, and nothing else will ever come for this job.
    // Stopping it here is what keeps "a stopped tree does not grow" true.
    if let Some(parent) = parent_job_id.as_deref() {
        if let Err(err) = refuse_dispatch_from_a_stopped_parent(store, parent) {
            if let Err(cleanup) = stop_job::stop_background_job_in_store(store, &mut state) {
                tracing::error!(
                    %cleanup,
                    job_id = %state.identity.job_id,
                    "a job dispatched from a stopped parent could not be stopped again"
                );
            }
            let _ = store.append_event(
                &job_id,
                "dispatch_refused_stopped_parent",
                serde_json::json!({ "parentJobId": parent }),
            );
            return Err(err);
        }
    }
    let agent_type_for_state = agent_type.clone();
    state = store.update_state(&job_id, |state| {
        state.identity.prompt_images = images;
        state.workspace.isolate_in_worktree = isolate_in_worktree;
        state.workspace.require_worktree = require_worktree;
        state.workspace.preserve_worktree_on_success = preserve_worktree_on_success;
        state.identity.queue_session = queue_session;
        state.identity.agent_type = agent_type_for_state.clone();
        Ok(state.clone())
    })?;
    if let Some(applied) = agent_runtime_applied {
        store.append_event(
            &state.identity.job_id,
            "agent_runtime_applied",
            serde_json::json!({
                "agentType": agent_type.as_deref(),
                "applied": applied,
            }),
        )?;
    }
    ensure_supervisor_running(store, rebon_exe_path)?;
    store.append_event(
        &state.identity.job_id,
        "queued_for_supervisor",
        serde_json::json!({}),
    )?;
    Ok(state)
}

pub fn create_attached_background_job(
    store: &BackgroundStore,
    prompt: String,
    images: Vec<BackgroundImageAttachment>,
    cwd: PathBuf,
    mut runtime: BackgroundRuntimeFields,
    session_id: String,
    agent_type: Option<String>,
    validate_permission_mode: impl FnOnce(&BackgroundRuntimeFields) -> anyhow::Result<()>,
) -> anyhow::Result<BackgroundJobState> {
    let agent_type = agent_type.and_then(|agent| non_empty_trimmed(&agent));
    let agent_runtime_applied = match agent_type.as_deref() {
        Some(agent) => {
            apply_subagent_frontmatter_to_runtime(&mut runtime, &cwd, agent, store.root())
        }
        None => None,
    };
    validate_permission_mode(&runtime)?;
    let mut state = store.create_job_with_name(prompt, cwd, runtime, None)?;
    let job_id = state.identity.job_id.clone();
    let agent_type_for_state = agent_type.clone();
    state = store.update_state(&job_id, |state| {
        state.identity.session_id = Some(session_id.clone());
        state.identity.prompt_images = images;
        state.identity.agent_type = agent_type_for_state.clone();
        state.process.status = BackgroundJobStatus::Idle;
        state.process.updated_at_ms = now_ms();
        Ok(state.clone())
    })?;
    store.append_event(
        &state.identity.job_id,
        "attached_session_created",
        serde_json::json!({ "sessionId": session_id }),
    )?;
    if let Some(applied) = agent_runtime_applied {
        store.append_event(
            &state.identity.job_id,
            "agent_runtime_applied",
            serde_json::json!({
                "agentType": agent_type.as_deref(),
                "applied": applied,
            }),
        )?;
    }
    Ok(state)
}

pub fn mark_background_job_idle(store: &BackgroundStore, job_id: &str) -> anyhow::Result<()> {
    store.update_state(job_id, |state| {
        ensure_background_job_ownership_reusable(state, job_id)?;
        ensure_background_job_has_no_unfinished_follow_up(state, job_id)?;
        if state.process.pid.is_some()
            || state.process.ipc_port.is_some()
            || state.process.ipc_token.is_some()
            || matches!(
                state.process.status,
                BackgroundJobStatus::Queued
                    | BackgroundJobStatus::Running
                    | BackgroundJobStatus::NeedsInput
            )
        {
            anyhow::bail!(
                "background job {job_id} acquired work or an owner before local takeover"
            );
        }
        state.process.status = BackgroundJobStatus::Idle;
        state.clear_recorded_owner();
        state.process.spawn_admitted = false;
        state.outcome.pending_permission = None;
        state.clear_pending_prompts();
        state.outcome.summary_updated_at_ms = None;
        state.process.updated_at_ms = now_ms();
        state.process.completed_at_ms = None;
        state.outcome.exit_code = None;
        state.outcome.error = None;
        Ok(())
    })?;
    store.append_event(job_id, "detached_idle", serde_json::json!({}))?;
    Ok(())
}

pub fn queue_background_job(
    store: &BackgroundStore,
    job_id: &str,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    queue_background_job_in_store(store, job_id, true, rebon_exe_path)
}

pub fn append_background_prompt_with_images(
    store: &BackgroundStore,
    job_id: &str,
    pending_prompt_id: String,
    message: String,
    images: Vec<BackgroundImageAttachment>,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<PendingPromptAcceptance> {
    let prompt = PendingPrompt::new(pending_prompt_id, message, images, now_ms())?;
    append_background_pending_prompt(store, job_id, prompt, start_supervisor, rebon_exe_path)
}

pub fn append_background_internal_prompt(
    store: &BackgroundStore,
    job_id: &str,
    pending_prompt_id: String,
    message: String,
    coordinator_report_paths: Vec<String>,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<PendingPromptAcceptance> {
    if !pending_prompt_id.starts_with("u-internal-") {
        anyhow::bail!("internal background prompt id must start with `u-internal-`");
    }
    let mut prompt = PendingPrompt::new(pending_prompt_id, message, Vec::new(), now_ms())?;
    prompt.coordinator_report_paths = coordinator_report_paths;
    append_background_pending_prompt(store, job_id, prompt, start_supervisor, rebon_exe_path)
}

fn append_background_pending_prompt(
    store: &BackgroundStore,
    job_id: &str,
    prompt: PendingPrompt,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<PendingPromptAcceptance> {
    let queued_at = prompt.enqueued_at_ms;
    let mut observed = store.read_state(job_id)?;
    store.reconcile_stale_pid(&mut observed)?;
    let mut should_start_supervisor = false;
    let acceptance = store.update_state(job_id, |state| {
        ensure_background_job_ownership_reusable(state, job_id)?;
        if state.identity.session_id.is_none() {
            anyhow::bail!("background job {job_id} has no session to reply to yet");
        }
        if state.outcome.pending_permission.is_some()
            && state.process.status != BackgroundJobStatus::NeedsInput
        {
            anyhow::bail!(
                "background job {job_id} is waiting for permission; answer or cancel it before sending another turn"
            );
        }
        let acceptance = state.append_pending_prompt(prompt.clone())?;
        let active = matches!(
            state.process.status,
            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
        );
        if !active {
            should_start_supervisor = true;
        }
        if !acceptance.appended {
            return Ok(acceptance);
        }
        if !active {
            state.process.status = BackgroundJobStatus::Queued;
            state.outcome.pending_permission = None;
            state.outcome.summary = None;
            state.outcome.summary_updated_at_ms = None;
            state.process.completed_at_ms = None;
            state.outcome.exit_code = None;
            state.outcome.error = None;
        }
        state.identity.resume_only = false;
        state.process.updated_at_ms = queued_at;
        Ok(acceptance)
    })?;
    if start_supervisor && should_start_supervisor {
        ensure_supervisor_running(store, rebon_exe_path)?;
    }
    if acceptance.appended {
        store.append_event(
            job_id,
            "reply_queued",
            serde_json::json!({
                "resumed": should_start_supervisor,
                "nonInterrupting": true,
                "promptId": acceptance.prompt.id,
                "queueDepth": acceptance.queue_depth,
            }),
        )?;
    }
    Ok(acceptance)
}

pub fn reply_to_background_job(
    store: &BackgroundStore,
    job_id: &str,
    message: String,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    reply_to_background_job_with_images(store, job_id, message, Vec::new(), rebon_exe_path)
}

pub fn reply_to_background_job_with_images(
    store: &BackgroundStore,
    job_id: &str,
    message: String,
    images: Vec<BackgroundImageAttachment>,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    reply_to_background_job_in_store_with_images(
        store,
        job_id,
        message,
        images,
        true,
        true,
        rebon_exe_path,
    )
}

pub fn queue_background_reply_if_inactive(
    store: &BackgroundStore,
    job_id: &str,
    message: String,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    queue_background_reply_if_inactive_with_images(
        store,
        job_id,
        message,
        Vec::new(),
        rebon_exe_path,
    )
}

pub fn queue_background_reply_if_inactive_with_images(
    store: &BackgroundStore,
    job_id: &str,
    message: String,
    images: Vec<BackgroundImageAttachment>,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    append_background_prompt_with_images(
        store,
        job_id,
        generate_pending_prompt_id(),
        message,
        images,
        true,
        rebon_exe_path,
    )?;
    Ok(())
}

pub fn warm_background_job_for_peek(
    store: &BackgroundStore,
    job_id: &str,
    rebon_exe_path: &Path,
) -> anyhow::Result<bool> {
    warm_background_job_for_peek_in_store(store, job_id, true, rebon_exe_path)
}

pub fn reply_to_background_job_in_store(
    store: &BackgroundStore,
    job_id: &str,
    message: String,
    start_supervisor: bool,
    try_ipc: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    reply_to_background_job_in_store_with_images(
        store,
        job_id,
        message,
        Vec::new(),
        start_supervisor,
        try_ipc,
        rebon_exe_path,
    )
}

pub fn reply_to_background_job_in_store_with_images(
    store: &BackgroundStore,
    job_id: &str,
    message: String,
    images: Vec<BackgroundImageAttachment>,
    start_supervisor: bool,
    _try_ipc: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    append_background_prompt_with_images(
        store,
        job_id,
        generate_pending_prompt_id(),
        message,
        images,
        start_supervisor,
        rebon_exe_path,
    )?;
    Ok(())
}

fn queue_background_job_in_store(
    store: &BackgroundStore,
    job_id: &str,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    let mut state = store.read_state(job_id)?;
    store.reconcile_stale_pid(&mut state)?;
    ensure_background_job_ownership_reusable(&state, job_id)?;
    if matches!(
        state.process.status,
        BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
    ) {
        anyhow::bail!("background job {job_id} is already running");
    }
    let had_session = state.identity.session_id.is_some();
    store.update_state(job_id, |state| {
        ensure_background_job_ownership_reusable(state, job_id)?;
        state.process.status = BackgroundJobStatus::Queued;
        state.identity.resume_only = false;
        state.outcome.pending_permission = None;
        state.process.updated_at_ms = now_ms();
        state.process.completed_at_ms = None;
        state.outcome.exit_code = None;
        state.outcome.error = None;
        Ok(())
    })?;
    if start_supervisor {
        ensure_supervisor_running(store, rebon_exe_path)?;
        store.append_event(
            job_id,
            "queued_for_supervisor",
            serde_json::json!({ "resumed": had_session }),
        )?;
    }
    Ok(())
}

pub fn print_background_logs(
    store: &BackgroundStore,
    job_id: &str,
    max_lines: usize,
) -> anyhow::Result<()> {
    for line in store.read_log_tail(job_id, max_lines)? {
        println!("{line}");
    }
    Ok(())
}

/// Stop a job from outside the UI (`rebon stop`), taking the work it
/// started with it. Same ownership rule as stopping it from Agent View —
/// where you stopped it from is not supposed to change what happens.
pub fn stop_background_job(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<StoppedJobTree> {
    let mut state = store.read_state(job_id)?;
    stop_background_job_tree_in_store(store, &mut state)
}

pub fn respawn_background_job(
    store: &BackgroundStore,
    job_id: &str,
    rebon_exe_path: &Path,
) -> anyhow::Result<BackgroundJobState> {
    respawn_background_job_in_store(store, job_id, true, rebon_exe_path)
}

pub fn respawn_all_background_jobs(
    store: &BackgroundStore,
    rebon_exe_path: &Path,
) -> anyhow::Result<Vec<BackgroundJobState>> {
    respawn_all_background_jobs_in_store(store, true, rebon_exe_path)
}

pub fn remove_background_job(store: &BackgroundStore, job_id: &str) -> anyhow::Result<()> {
    store.remove_job(job_id)
}

fn prepare_respawned_job(
    store: &BackgroundStore,
    job_id: &str,
    skip_ineligible: bool,
) -> anyhow::Result<Option<(BackgroundJobState, bool)>> {
    let mut observed = store.read_state(job_id)?;
    store.reconcile_stale_pid(&mut observed)?;

    let _lock = store.lock_job_state(job_id)?;
    let mut source = store.read_state(job_id)?;
    if source.process.removal_reserved {
        if skip_ineligible {
            return Ok(None);
        }
        anyhow::bail!("background job {job_id} is reserved for removal");
    }
    if source.process.process_owner_fenced {
        if skip_ineligible {
            return Ok(None);
        }
        anyhow::bail!(
            "background job {job_id} ownership is fenced until its recorded process exit is verified"
        );
    }
    if source.process.pid.is_some()
        || source.process.ipc_port.is_some()
        || source.process.ipc_token.is_some()
    {
        if skip_ineligible {
            return Ok(None);
        }
        anyhow::bail!(
            "background job {job_id} still has a recorded process owner; wait for its exit before respawning"
        );
    }
    if matches!(
        source.process.status,
        BackgroundJobStatus::Queued
            | BackgroundJobStatus::Running
            | BackgroundJobStatus::NeedsInput
            | BackgroundJobStatus::Idle
    ) {
        if skip_ineligible {
            return Ok(None);
        }
        anyhow::bail!("background job {job_id} is still active; stop it before respawning");
    }

    let target_job_id = match source.identity.respawned_job_id.clone() {
        Some(target_job_id) => target_job_id,
        None => loop {
            let candidate = generate_job_id();
            if candidate != source.identity.job_id && !store.state_path(&candidate).exists() {
                break candidate;
            }
        },
    };
    let target_exists = store.state_path(&target_job_id).exists();
    if source.identity.respawned_job_id.is_none() {
        source.identity.respawned_job_id = Some(target_job_id.clone());
        source.process.updated_at_ms = now_ms();
        store.write_state_unlocked(&source)?;
    }
    let target = create_respawned_job_with_id(store, &source, target_job_id)?;
    Ok(Some((target, !target_exists)))
}

fn respawn_background_job_in_store(
    store: &BackgroundStore,
    job_id: &str,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<BackgroundJobState> {
    let Some((new_job, created)) = prepare_respawned_job(store, job_id, false)? else {
        unreachable!("strict respawn preparation cannot skip an ineligible job");
    };
    if created {
        store.append_event(
            job_id,
            "respawned",
            serde_json::json!({ "newJobId": new_job.identity.job_id }),
        )?;
        store.append_event(
            &new_job.identity.job_id,
            "respawn_of",
            serde_json::json!({ "sourceJobId": job_id }),
        )?;
    }
    if start_supervisor && new_job.process.status == BackgroundJobStatus::Queued {
        ensure_supervisor_running(store, rebon_exe_path)?;
        if created {
            store.append_event(
                &new_job.identity.job_id,
                "queued_for_supervisor",
                serde_json::json!({}),
            )?;
        }
    }
    Ok(new_job)
}

fn respawn_all_background_jobs_in_store(
    store: &BackgroundStore,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<Vec<BackgroundJobState>> {
    let job_ids = store
        .list_jobs()?
        .into_iter()
        .map(|source| source.identity.job_id)
        .collect::<Vec<_>>();
    let mut new_jobs = Vec::new();
    let mut has_queued_target = false;
    for job_id in job_ids {
        let Some((new_job, created)) = prepare_respawned_job(store, &job_id, true)? else {
            continue;
        };
        has_queued_target |= new_job.process.status == BackgroundJobStatus::Queued;
        if !created {
            continue;
        }
        store.append_event(
            &job_id,
            "respawned",
            serde_json::json!({ "newJobId": new_job.identity.job_id, "all": true }),
        )?;
        store.append_event(
            &new_job.identity.job_id,
            "respawn_of",
            serde_json::json!({ "sourceJobId": job_id, "all": true }),
        )?;
        new_jobs.push(new_job);
    }
    if start_supervisor && has_queued_target {
        ensure_supervisor_running(store, rebon_exe_path)?;
        for job in &new_jobs {
            store.append_event(
                &job.identity.job_id,
                "queued_for_supervisor",
                serde_json::json!({}),
            )?;
        }
    }
    Ok(new_jobs)
}

fn background_worktree_info_from_path(
    path: &Path,
    job_id: &str,
) -> Option<rebon_tool::worktree::AgentWorktreeInfo> {
    let worktree_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        path.canonicalize().ok()?
    };
    let git_root = rebon_tool::worktree::find_git_root(&worktree_path)?;
    Some(rebon_tool::worktree::AgentWorktreeInfo {
        worktree_path,
        // Mirrors the `rebon/<slug>` branch convention in
        // `rebon_tool::worktree::create_authorized_agent_worktree`.
        worktree_branch: format!("rebon/{}", background_worktree_slug(job_id)),
        head_commit: String::new(),
        source_worktree: git_root.clone(),
        source_branch: None,
        git_root,
    })
}

/// How long a controller waits for one session command to answer.
///
/// Most commands render local state and answer in milliseconds, so 30s is a
/// generous ceiling that still surfaces a wedged worker quickly. `/compact`
/// is the exception: it runs a summariser over the whole conversation, which
/// takes ~30s on a normal context and longer on a full one, so a 30s ceiling
/// would time out the caller on essentially every successful compaction.
pub fn command_response_timeout(name: &str) -> Duration {
    if name
        .trim()
        .trim_start_matches('/')
        .eq_ignore_ascii_case("compact")
    {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    }
}

/// Run a slash command on the owner's turn loop.
///
/// Paired with [`run_background_command_with_id`], which is the form to use
/// when a caller may retry: this one mints a fresh id per call, so a retry
/// through here runs the command again rather than replaying the first answer.
pub fn run_background_command(
    state: &BackgroundJobState,
    port: u16,
    token: String,
    name: String,
    args: Vec<String>,
) -> anyhow::Result<CommandOutput> {
    run_background_command_with_id(state, port, token, name, args, generate_command_id())
}

/// [`run_background_command`] with the idempotency key named.
///
/// The id is not optional any more. It is what a `CancelCall` uses to find this
/// call in the owner's in-flight registry, and this is the longest-waiting
/// request there is — a `/compact` holds the caller for minutes. Sending it
/// anonymously, as it once did, meant the one request most worth
/// cancelling was the one request that could not be.
pub fn run_background_command_with_id(
    state: &BackgroundJobState,
    port: u16,
    token: String,
    name: String,
    args: Vec<String>,
    command_id: String,
) -> anyhow::Result<CommandOutput> {
    let mut stream = TcpStream::connect_timeout(
        &BackgroundStore::ipc_addr(port).parse()?,
        Duration::from_millis(500),
    )?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(command_response_timeout(&name)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    serde_json::to_writer(
        &mut stream,
        &BackgroundIpcEnvelope {
            protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
            job_id: Some(state.identity.job_id.clone()),
            session_id: state.identity.session_id.clone(),
            command_id: Some(command_id),
            token,
            request: BackgroundIpcRequest::RunCommand { name, args },
        },
    )?;
    stream.write_all(b"\n")?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let response: BackgroundCommandResponse = serde_json::from_reader(&mut stream)?;
    match (response.output, response.error) {
        (Some(output), None) => Ok(output),
        (_, Some(error)) => anyhow::bail!(error),
        _ => anyhow::bail!("background command returned no output"),
    }
}

pub fn send_background_ipc_request(
    state: &BackgroundJobState,
    port: u16,
    token: String,
    request: BackgroundIpcRequest,
) -> anyhow::Result<()> {
    send_background_ipc_request_full(state, port, token, request, None).map(|_| ())
}

/// Tell every job that publishes an endpoint to re-read the plugin switches.
/// Returns one entry per job reached, with the send outcome; jobs without an
/// endpoint are skipped, not failed — they pick the switch up on their next
/// start.
pub fn broadcast_reconcile_plugins(store: &BackgroundStore) -> Vec<(String, anyhow::Result<()>)> {
    let jobs = match store.list_jobs() {
        Ok(jobs) => jobs,
        Err(err) => return vec![("<list>".to_string(), Err(err))],
    };
    jobs.into_iter()
        .filter_map(|state| {
            let port = state.process.ipc_port?;
            let token = state.process.ipc_token.clone()?;
            let outcome = send_background_ipc_request(
                &state,
                port,
                token,
                BackgroundIpcRequest::ReconcilePlugins,
            );
            Some((state.identity.job_id.clone(), outcome))
        })
        .collect()
}

/// Send a request and hand back the whole response, payload included.
///
/// `command_id` makes the send idempotent: a retry after a lost connection
/// carries the same id and the owner answers from its recent-results memory
/// instead of running the command a second time.
pub fn send_background_ipc_request_full(
    state: &BackgroundJobState,
    port: u16,
    token: String,
    request: BackgroundIpcRequest,
    command_id: Option<String>,
) -> anyhow::Result<BackgroundIpcResponse> {
    let mut stream = TcpStream::connect_timeout(
        &BackgroundStore::ipc_addr(port).parse()?,
        Duration::from_millis(500),
    )?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let envelope = BackgroundIpcEnvelope {
        protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
        // Always sent when known: a worker from before the field became
        // optional requires it, and this is a client that may be talking to
        // one that has been lingering since the last release.
        job_id: Some(state.identity.job_id.clone()),
        session_id: state.identity.session_id.clone(),
        command_id,
        token,
        request,
    };
    serde_json::to_writer(&mut stream, &envelope)?;
    stream.write_all(b"\n")?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let response: BackgroundIpcResponse = serde_json::from_reader(&mut stream)?;
    if response.ok {
        Ok(response)
    } else {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| "background IPC failed".to_string()))
    }
}

/// Ask the owner for the session state every client must agree with (I4).
pub fn request_session_status(
    state: &BackgroundJobState,
    port: u16,
    token: String,
) -> anyhow::Result<SessionStatusSnapshot> {
    let response =
        send_background_ipc_request_full(state, port, token, BackgroundIpcRequest::Status, None)?;
    let data = response
        .data
        .ok_or_else(|| anyhow::anyhow!("the owner answered Status without a snapshot"))?;
    Ok(serde_json::from_value(data)?)
}

pub fn keep_supervisor_alive_for_agent_view(store: &BackgroundStore, rebon_exe_path: &Path) {
    let _ = store.touch_supervisor_client("agent-view");
    let _ = ensure_supervisor_running(store, rebon_exe_path);
}

pub fn refresh_agent_view_supervisor_heartbeat(store: &BackgroundStore, rebon_exe_path: &Path) {
    let now = now_ms();
    let last = AGENT_VIEW_HEARTBEAT_LAST_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SUPERVISOR_CLIENT_HEARTBEAT_INTERVAL_MS {
        return;
    }
    AGENT_VIEW_HEARTBEAT_LAST_MS.store(now, Ordering::Relaxed);
    let _ = store.touch_supervisor_client("agent-view");
    #[cfg(not(test))]
    let _ = ensure_supervisor_running(store, rebon_exe_path);
    #[cfg(test)]
    let _ = rebon_exe_path;
}

/// How long a freshly spawned supervisor gets to register itself in the
/// roster before anyone spawns another one.
///
/// Registration is not instant — the new process has to start, take the
/// lock and write the roster — but the callers re-check far faster than
/// that (the agent view every 2s). Without a cooldown "not registered
/// yet" is indistinguishable from "never going to register", so a slow
/// start produces a pile of supervisors and a wrong binary produces an
/// unbounded one. The stamp lives in the store so separate processes
/// share it.
const SUPERVISOR_SPAWN_COOLDOWN_MS: u64 = 10_000;

fn supervisor_spawn_is_cooling_down(store: &BackgroundStore) -> bool {
    let Ok(stamp) = fs::read_to_string(store.supervisor_spawn_stamp_path()) else {
        return false;
    };
    let Ok(spawned_at_ms) = stamp.trim().parse::<u64>() else {
        return false;
    };
    let now = now_ms();
    // A stamp from the future (clock change) must not wedge the cooldown
    // shut forever.
    spawned_at_ms <= now && now.saturating_sub(spawned_at_ms) < SUPERVISOR_SPAWN_COOLDOWN_MS
}

fn record_supervisor_spawn(store: &BackgroundStore) {
    let _ = fs::write(
        store.supervisor_spawn_stamp_path(),
        now_ms().to_string().as_bytes(),
    );
}

/// Whether the process a roster names is the supervisor that wrote it.
///
/// The recorded identity is what makes this a real answer rather than a pid
/// lookup: a reused pid would otherwise read as "the supervisor is up", the
/// gate would never start a replacement, and every queued job would wait
/// forever on a supervisor that is gone. A roster written before the
/// identity existed has none to check, and falls back to the pid — the same
/// answer it always gave.
pub fn supervisor_roster_is_live(roster: &BackgroundRoster) -> bool {
    match roster.supervisor_pid_identity.as_deref() {
        Some(identity) => {
            recorded_process_is_running(roster.supervisor_pid, Some(identity)).unwrap_or(false)
        }
        None => process_is_running(roster.supervisor_pid) == Some(true),
    }
}

pub fn ensure_supervisor_running(
    store: &BackgroundStore,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    fs::create_dir_all(store.daemon_dir())?;
    if let Ok(roster) = store.read_roster() {
        if supervisor_roster_is_live(&roster)
            && now_ms().saturating_sub(roster.updated_at_ms) < 60_000
        {
            return Ok(());
        }
    }
    if supervisor_spawn_is_cooling_down(store) {
        return Ok(());
    }
    spawn_supervisor_process(store, rebon_exe_path)
}

/// Resolve the executable a supervisor spawn would actually start.
///
/// A bare program name is not a path: the OS resolves it against the
/// working directory and PATH, so whatever happens to answer to `rebon`
/// near the caller gets launched — including a same-named build
/// artifact that is not the CLI at all. Nothing downstream can catch
/// that mistake. `spawn` reports success for any process that was
/// created, the roster gate only ever learns "still no supervisor", and
/// the callers retry forever by design so a stranded queue is never
/// abandoned. Requiring a real path to a real file keeps a wrong target
/// to a single error here.
fn resolve_supervisor_exe(rebon_exe_path: &Path) -> anyhow::Result<PathBuf> {
    if rebon_exe_path
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        anyhow::bail!(
            "refusing to start the background supervisor from `{}`: a bare program \
             name is resolved against the working directory and PATH, which can \
             start an unrelated executable",
            rebon_exe_path.display()
        );
    }
    let canonical = fs::canonicalize(rebon_exe_path).with_context(|| {
        format!(
            "background supervisor executable `{}` cannot be resolved",
            rebon_exe_path.display()
        )
    })?;
    if !canonical.is_file() {
        anyhow::bail!(
            "background supervisor executable `{}` is not a file",
            canonical.display()
        );
    }
    Ok(canonical)
}

pub fn spawn_supervisor_process(
    store: &BackgroundStore,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    let rebon_exe_path = resolve_supervisor_exe(rebon_exe_path)?;
    // Drain anything a previous spawn could not hand to a reaper.
    poll_unreaped_spawned_children();
    // Stamped before the spawn, not after: if the child turns out to be
    // the wrong binary, or dies before registering, the cooldown still
    // holds off the next attempt.
    record_supervisor_spawn(store);
    let log = store.open_daemon_log_for_append()?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(&rebon_exe_path);
    command
        .arg("__background-supervisor")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    hide_background_command_window(&mut command);
    // The supervisor outlives whoever asked for it, terminal included.
    detach_background_command(&mut command);
    let child = command
        .spawn()
        .context("failed to start background supervisor")?;
    // Reap it. Dropping the handle leaves an exited supervisor as a zombie
    // for as long as this process lives, and a zombie still answers "yes"
    // to a naive liveness check — which is how a dead supervisor with a
    // fresh roster entry kept `ensure_supervisor_running` from ever
    // starting a new one, stranding every queued job.
    reap_detached_child(child);
    Ok(())
}

/// Children whose reaper thread could not be started. Kept so a later spawn
/// can try again rather than dropping the handle — a dropped `Child` is
/// never waited on, and an exited process with nobody to wait on it is the
/// zombie that makes a dead supervisor look alive.
static UNREAPED_SPAWNED_CHILDREN: OnceLock<Mutex<Vec<std::process::Child>>> = OnceLock::new();

fn unreaped_spawned_children() -> &'static Mutex<Vec<std::process::Child>> {
    UNREAPED_SPAWNED_CHILDREN.get_or_init(|| Mutex::new(Vec::new()))
}

/// Collect whatever exited among the children we could not hand to a reaper.
/// Cheap, non-blocking, and called on the spawn path so the list drains
/// without a thread of its own.
fn poll_unreaped_spawned_children() {
    let mut children = unreaped_spawned_children()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
}

/// Wait out a spawned background process on a detached thread so it never
/// lingers as a zombie. The process is deliberately not owned or controlled
/// from here — this only collects its exit status when it ends.
fn reap_detached_child(child: std::process::Child) {
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::sync_channel::<std::process::Child>(0);
    match std::thread::Builder::new()
        .name(format!("background-spawn-reaper-{pid}"))
        .spawn(move || {
            if let Ok(mut child) = receiver.recv() {
                let _ = child.wait();
            }
        }) {
        Ok(_handle) => {
            // Handing the child over after the thread exists means a thread
            // that died at birth cannot take the handle down with it.
            if let Err(std::sync::mpsc::SendError(child)) = sender.send(child) {
                unreaped_spawned_children()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(child);
            }
        }
        Err(err) => {
            // The process keeps running either way; only the reaping is
            // lost. Hold the handle so a later spawn can collect it, rather
            // than dropping it and guaranteeing a zombie.
            tracing::warn!(%err, pid, "could not start a reaper for a spawned background process");
            unreaped_spawned_children()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(child);
        }
    }
}

fn parse_complete_event_lines(snapshot: &[u8]) -> Vec<BackgroundJobEvent> {
    let complete_len = snapshot
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    snapshot[..complete_len]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

fn read_tail_lines(path: &Path, max_lines: usize) -> anyhow::Result<Vec<String>> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    if max_lines == 0 || !path.exists() {
        return Ok(Vec::new());
    }

    const SCAN_CHUNK_BYTES: usize = 64 * 1024;

    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok(Vec::new());
    }

    let mut position = file_len;
    let mut start_offset = 0u64;
    let mut newline_count = 0usize;
    let mut first_chunk = true;
    let mut chunk = vec![0u8; SCAN_CHUNK_BYTES];

    'scan: while position > 0 {
        let chunk_start = position.saturating_sub(SCAN_CHUNK_BYTES as u64);
        let chunk_len = (position - chunk_start) as usize;
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(&mut chunk[..chunk_len])?;

        let mut scan_end = chunk_len;
        if first_chunk && chunk[chunk_len - 1] == b'\n' {
            scan_end -= 1;
        }
        first_chunk = false;

        for index in (0..scan_end).rev() {
            if chunk[index] != b'\n' {
                continue;
            }
            newline_count += 1;
            if newline_count == max_lines {
                start_offset = chunk_start + index as u64 + 1;
                break 'scan;
            }
        }
        position = chunk_start;
    }

    file.seek(SeekFrom::Start(start_offset))?;
    BufReader::new(file)
        .lines()
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn read_tail_lines_bounded(
    path: &Path,
    max_lines: usize,
    max_bytes: usize,
) -> anyhow::Result<Vec<String>> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    if max_lines == 0 || max_bytes == 0 || !path.exists() {
        return Ok(Vec::new());
    }

    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok(Vec::new());
    }

    let start_offset = file_len.saturating_sub(max_bytes as u64);
    file.seek(SeekFrom::Start(start_offset))?;
    let mut bytes = Vec::with_capacity((file_len - start_offset) as usize);
    file.read_to_end(&mut bytes)?;

    let start = if start_offset == 0 {
        0
    } else {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(bytes.len())
    };
    let end = if bytes.last() == Some(&b'\n') {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    if start_offset > 0 && start >= end {
        return Ok(Vec::new());
    }
    let mut lines = VecDeque::with_capacity(max_lines);
    for raw_line in bytes[start..end].split(|byte| *byte == b'\n') {
        let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        let line = std::str::from_utf8(raw_line)?.to_string();
        if lines.len() == max_lines {
            lines.pop_front();
        }
        lines.push_back(line);
    }
    Ok(lines.into_iter().collect())
}

pub fn validate_job_id(job_id: &str) -> anyhow::Result<()> {
    if job_id.is_empty()
        || !job_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        anyhow::bail!("invalid background job id `{job_id}`");
    }
    Ok(())
}

pub fn generate_ipc_token() -> anyhow::Result<String> {
    rebon_types::secure_random_hex_token().context("failed to generate background IPC token")
}

#[cfg(windows)]
pub fn hide_background_command_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_background_command_window(_command: &mut Command) {}

/// Cut a background process loose from the terminal that started it.
///
/// `/hosted` promises a session that "keeps running when you close the
/// terminal", and the supervisor is meant to outlive whatever asked for it.
/// A child inherits its parent's session and controlling terminal, so
/// closing the terminal delivers SIGHUP to that session and takes both
/// down — the promise held on Windows (where there is no such signal) and
/// quietly did not on Unix.
///
/// `setsid` in the child makes it a session leader with no controlling
/// terminal, which also gives it a process group of its own — the group
/// [`terminate_process`] signals so a worker's subprocesses go with it.
#[cfg(unix)]
pub fn detach_background_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the forked child between fork and exec, where only
    // async-signal-safe calls are allowed. `setsid` is one of them, and
    // nothing here allocates or locks.
    //
    // The error is propagated, which fails the spawn. `setsid` cannot fail
    // for a freshly forked child — it is not a process-group leader — so
    // this never fires in practice; if it ever did, everything downstream
    // (the terminal outliving, the process-group termination) would be
    // quietly untrue, and a loud spawn failure beats a worker that looks
    // detached and is not.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// No-op on Windows: there is no controlling terminal to leave, and the
/// spawn already carries `CREATE_NO_WINDOW`.
#[cfg(not(unix))]
pub fn detach_background_command(_command: &mut Command) {}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
