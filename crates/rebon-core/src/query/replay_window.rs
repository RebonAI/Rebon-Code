use super::*;

const REPLAY_PROTECTED_TURNS: usize = 10;
const MAX_REPLAY_WINDOWS: usize = 32;
const MAX_REPLAY_CONFLICT_RETRIES: usize = 3;

/// Engine-owned, bounded first-stage projection of a session's model-visible
/// replay. Raw transcript rows and the complete normalized history are consumed
/// while constructing this value and are never retained here.
///
/// The projection is deliberately rebuilt from the authoritative transcript
/// whenever the source revision changes. `auto_compact_truncate` is not
/// associative or idempotent, and an exact bounded incremental fold has not been
/// proven for malformed/duplicate tool IDs and the legacy two-pass execution
/// summary.
#[derive(Debug, Clone)]
pub(super) struct ReplayWindow {
    cwd: String,
    incarnation: u64,
    revision: u64,
    last_raw_uuid: Option<String>,
    /// Exact legacy title-model input derived before replay compaction. This is
    /// bounded by `extract_conversation_text`; `None` means the legacy path had
    /// fewer than three prior messages and must use the current user prompt.
    title_conversation_text: Option<String>,
    has_anchored_minimal_anchor: bool,
    has_assistant_turn: bool,
    projection: Vec<ApiMessage>,
}

impl ReplayWindow {
    fn from_normalized(
        source: &rebon_session_state::ReplayTranscriptSource,
        messages: Vec<ApiMessage>,
    ) -> Self {
        let title_conversation_text =
            (messages.len() >= 3).then(|| rebon_api::extract_conversation_text(&messages));
        let has_anchored_minimal_anchor = history_has_anchored_minimal_anchor(&messages);
        let has_assistant_turn = history_has_assistant_turn(&messages);
        Self {
            cwd: source.cwd.clone(),
            incarnation: source.incarnation,
            revision: source.revision,
            last_raw_uuid: source.last_uuid.clone(),
            title_conversation_text,
            has_anchored_minimal_anchor,
            has_assistant_turn,
            // The cache-stable variant is load-bearing here: this projection is
            // rebuilt every turn, and a per-turn sliding boundary would shift
            // the whole model-visible prefix each request, busting provider
            // prompt caches for app, background and ACP sessions past ~10 turns.
            projection: rebon_api::auto_compact_truncate_cache_stable(
                messages,
                REPLAY_PROTECTED_TURNS,
            ),
        }
    }

    fn matches(&self, source: &rebon_session_state::ReplayTranscriptSource) -> bool {
        self.cwd == source.cwd
            && self.incarnation == source.incarnation
            && self.revision == source.revision
    }

    fn render(&self) -> Vec<ApiMessage> {
        self.projection.clone()
    }
}

#[derive(Debug, Clone)]
pub struct PreparedResumeSummary {
    pub(super) projection: Vec<ApiMessage>,
    pub(super) anchor_uuid: String,
    anchor_entry: rebon_session::TranscriptEntry,
    prefix_hash: u64,
}

#[derive(Debug, Clone)]
struct ResumeSummaryBaseline {
    projection: Vec<ApiMessage>,
    anchor_uuid: String,
    anchor_entry: rebon_session::TranscriptEntry,
    prefix_hash: u64,
}

/// On-disk form of a compacted baseline.
///
/// `/compact` has to outlive the process that ran it. A foreground TUI keeps
/// one session for hours, but a background worker rebuilds its session — and
/// with it this whole store — for every single turn, so an in-memory-only
/// baseline would be thrown away before the compaction it represents was
/// ever used. Persisting it is what makes "compacted" mean compacted for the
/// desktop app.
///
/// The anchor's own row is inside `prefix_hash`, so the hash alone decides
/// whether the baseline still describes this transcript; a rewind or an edit
/// upstream of the anchor changes it and the baseline is dropped.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedResumeSummary {
    version: u32,
    anchor_uuid: String,
    prefix_hash: u64,
    projection: Vec<ApiMessage>,
}

const RESUME_SUMMARY_SIDECAR_VERSION: u32 = 1;

/// `<project_dir>/<session_id>.compact.json`.
fn compact_baseline_path(
    projects_root: &std::path::Path,
    cwd: &str,
    session_id: &str,
) -> std::path::PathBuf {
    let mut path = rebon_session::project_dir_path(projects_root, cwd);
    path.push(format!("{session_id}.compact.json"));
    path
}

/// Best-effort: a baseline that cannot be written is still installed in
/// memory, so the compaction the user just waited for is not wasted — it
/// just does not survive this process.
fn write_compact_baseline(
    path: &std::path::Path,
    baseline: &ResumeSummaryBaseline,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(&PersistedResumeSummary {
        version: RESUME_SUMMARY_SIDECAR_VERSION,
        anchor_uuid: baseline.anchor_uuid.clone(),
        prefix_hash: baseline.prefix_hash,
        projection: baseline.projection.clone(),
    })
    .map_err(std::io::Error::other)?;
    rebon_session::write_file_atomically(path, &body)
}

fn read_compact_baseline(path: &std::path::Path) -> Option<PersistedResumeSummary> {
    let bytes = std::fs::read(path).ok()?;
    let persisted: PersistedResumeSummary = serde_json::from_slice(&bytes).ok()?;
    (persisted.version == RESUME_SUMMARY_SIDECAR_VERSION).then_some(persisted)
}

/// Per-executor, per-session normalized replay store. A store miss or source
/// change performs an O(file) rebuild; disk JSONL remains authoritative. The
/// LRU bound prevents detached/evicted session churn from retaining projections
/// indefinitely (active sessions transparently rebuild on an eviction miss).
#[derive(Debug, Default)]
struct ReplayWindows {
    windows: HashMap<String, ReplayWindow>,
    lru: std::collections::VecDeque<String>,
}

impl ReplayWindows {
    fn touch(&mut self, session_id: &str) {
        self.lru.retain(|id| id != session_id);
        self.lru.push_back(session_id.to_string());
    }

    fn insert(&mut self, session_id: String, window: ReplayWindow) {
        self.windows.insert(session_id.clone(), window);
        self.touch(&session_id);
        while self.windows.len() > MAX_REPLAY_WINDOWS {
            let Some(evicted) = self.lru.pop_front() else {
                break;
            };
            self.windows.remove(&evicted);
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct ReplayWindowStore {
    windows: Mutex<ReplayWindows>,
    resume_summaries: Mutex<HashMap<String, ResumeSummaryBaseline>>,
}

#[derive(Debug)]
struct CanonicalLeafFacts {
    selected_uuid: Option<String>,
}

/// Select only the raw canonical leaf identity. This mirrors the leaf-selection
/// half of `reconstruct_chain` (including duplicate UUID last-write-wins and
/// equal-timestamp first-candidate behavior) without constructing ancestry.
fn canonical_leaf_facts(entries: &[rebon_session::TranscriptEntry]) -> CanonicalLeafFacts {
    if entries.is_empty() {
        return CanonicalLeafFacts {
            selected_uuid: None,
        };
    }
    let mut by_uuid = std::collections::HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        by_uuid.insert(entry.uuid.as_str(), index);
    }
    let referenced = by_uuid
        .values()
        .filter_map(|&index| entries[index].parent_uuid.as_deref())
        .collect::<std::collections::HashSet<_>>();
    let mut candidates = std::collections::HashSet::new();
    let mut considered = std::collections::HashSet::new();
    for entry in entries {
        if !considered.insert(entry.uuid.as_str()) || referenced.contains(entry.uuid.as_str()) {
            continue;
        }
        let mut cursor = by_uuid.get(entry.uuid.as_str()).copied();
        let mut seen = std::collections::HashSet::new();
        while let Some(index) = cursor {
            let entry = &entries[index];
            if !seen.insert(entry.uuid.as_str()) {
                break;
            }
            if entry.entry_type == "user" || entry.entry_type == "assistant" {
                candidates.insert(entry.uuid.as_str());
                break;
            }
            cursor = entry
                .parent_uuid
                .as_deref()
                .and_then(|parent| by_uuid.get(parent).copied());
        }
    }

    let mut best = None;
    let mut picked_seen = std::collections::HashSet::new();
    for entry in entries {
        if !candidates.contains(entry.uuid.as_str()) || !picked_seen.insert(entry.uuid.as_str()) {
            continue;
        }
        let index = by_uuid[entry.uuid.as_str()];
        match best {
            None => best = Some(index),
            Some(current) => {
                let timestamp = entries[index].timestamp.as_deref().unwrap_or("");
                let current_timestamp = entries[current].timestamp.as_deref().unwrap_or("");
                if timestamp > current_timestamp {
                    best = Some(index);
                }
            }
        }
    }

    CanonicalLeafFacts {
        selected_uuid: best.map(|index| entries[index].uuid.clone()),
    }
}

fn canonical_ancestry_uuids(
    entries: &[rebon_session::TranscriptEntry],
    selected_uuid: Option<&str>,
) -> std::collections::HashSet<String> {
    let by_uuid = entries
        .iter()
        .map(|entry| (entry.uuid.as_str(), entry))
        .collect::<std::collections::HashMap<_, _>>();
    let mut ancestry = std::collections::HashSet::new();
    let mut cursor = selected_uuid;
    while let Some(uuid) = cursor {
        let Some(entry) = by_uuid.get(uuid) else {
            break;
        };
        if !ancestry.insert(entry.uuid.clone()) {
            break;
        }
        cursor = entry.parent_uuid.as_deref();
    }
    ancestry
}

#[cfg(test)]
thread_local! {
    static RECONSTRUCT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn reconstruct_replay_chain(
    entries: Vec<rebon_session::TranscriptEntry>,
) -> Option<rebon_session::LoadedTranscript> {
    #[cfg(test)]
    RECONSTRUCT_CALLS.with(|calls| calls.set(calls.get() + 1));
    rebon_session::reconstruct_chain(entries)
}

fn transcript_prefix_hash(entries: &[rebon_session::TranscriptEntry]) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for entry in entries {
        entry.uuid.hash(&mut hasher);
        entry.entry_type.hash(&mut hasher);
        entry.parent_uuid.hash(&mut hasher);
        entry.timestamp.hash(&mut hasher);
        entry.raw.to_string().hash(&mut hasher);
    }
    hasher.finish()
}

fn same_raw_entry(
    left: &rebon_session::TranscriptEntry,
    right: &rebon_session::TranscriptEntry,
) -> bool {
    left.uuid == right.uuid
        && left.entry_type == right.entry_type
        && left.parent_uuid == right.parent_uuid
        && left.timestamp == right.timestamp
        && left.raw == right.raw
}

/// Resolve one replay source into the raw transcript rows it stands for.
///
/// A complete in-memory source is used as it stands; an incomplete one is
/// reconciled against the file on disk, and every way that reconciliation can
/// fail returns through `failed_replay`, which hands the source back to the
/// session before reporting. `source` therefore travels by value and is
/// returned on the success path: the error path consumes it.
fn raw_history_for_source(
    state: &ServerState,
    source: rebon_session_state::ReplayTranscriptSource,
    projects_root: &std::path::Path,
    session_id: &str,
    complete_baseline: &mut Option<
        std::collections::HashMap<String, rebon_session::TranscriptEntry>,
    >,
    retained: &mut Vec<rebon_session::TranscriptEntry>,
) -> Result<
    (
        rebon_session_state::ReplayTranscriptSource,
        Vec<rebon_session::TranscriptEntry>,
    ),
    PromptExecutorError,
> {
    let raw = if source.complete {
        let last_source_index = source
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.uuid.as_str(), index))
            .collect::<std::collections::HashMap<_, _>>();
        let final_source_entries = source
            .entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| last_source_index.get(entry.uuid.as_str()) == Some(index))
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        if let Some(baseline) = complete_baseline.as_ref() {
            *retained = final_source_entries
                .iter()
                .filter(|entry| {
                    baseline
                        .get(entry.uuid.as_str())
                        .is_none_or(|original| !same_raw_entry(original, entry))
                })
                .map(|entry| (*entry).clone())
                .collect();
        } else {
            *complete_baseline = Some(
                final_source_entries
                    .iter()
                    .map(|entry| (entry.uuid.clone(), (*entry).clone()))
                    .collect(),
            );
        }

        if source.entries.is_empty() {
            Vec::new()
        } else {
            let Some(rebuilt) = reconstruct_replay_chain(source.entries.clone()) else {
                return Err(failed_replay(
                    state,
                    source,
                    format!(
                        "failed to rebuild replay window for {session_id}: complete transcript has no canonical chain"
                    ),
                ));
            };
            let rebuilt_by_uuid = rebuilt
                .messages
                .iter()
                .map(|entry| (entry.uuid.as_str(), entry))
                .collect::<std::collections::HashMap<_, _>>();
            if !retained.iter().all(|required| {
                rebuilt_by_uuid
                    .get(required.uuid.as_str())
                    .is_some_and(|entry| same_raw_entry(entry, required))
            }) {
                return Err(failed_replay(
                    state,
                    source,
                    format!(
                        "failed to rebuild replay window for {session_id}: pending complete-source delta is not canonical"
                    ),
                ));
            }
            rebuilt.messages
        }
    } else {
        // Disk is authoritative, but canonical selection must happen only
        // after every pending row is overlaid in original order. The final
        // pending occurrence of a UUID is its recovery value under LWW.
        let path = rebon_session::transcript_file_path(projects_root, &source.cwd, session_id);
        let loaded = match rebon_session::load_raw_transcript_from_file(&path) {
            Ok(loaded) => loaded,
            Err(err) => {
                return Err(failed_replay(
                    state,
                    source,
                    format!("failed to rebuild replay window for {session_id}: {err}"),
                ))
            }
        };
        let (mut entries, disk_ancestry) = match loaded {
            Some(loaded) => {
                if !loaded.parse_complete {
                    return Err(failed_replay(
                        state,
                        source,
                        format!(
                            "failed to rebuild replay window for {session_id}: transcript contains malformed or unsupported JSONL records ({} of {} parsed)",
                            loaded.parsed_row_count, loaded.nonblank_row_count
                        ),
                    ));
                }
                let disk_facts = canonical_leaf_facts(&loaded.entries);
                if loaded.byte_len != 0 && disk_facts.selected_uuid.is_none() {
                    return Err(failed_replay(
                        state,
                        source,
                        format!(
                            "failed to rebuild replay window for {session_id}: nonempty transcript has no canonical chain ({} parsed rows)",
                            loaded.parsed_row_count
                        ),
                    ));
                }
                if loaded.byte_len == 0 && source.last_uuid.is_some() {
                    return Err(failed_replay(
                        state,
                        source,
                        format!(
                            "failed to rebuild replay window for {session_id}: empty transcript cannot recover the prior canonical tail"
                        ),
                    ));
                }
                let disk_ancestry = canonical_ancestry_uuids(
                    &loaded.entries,
                    disk_facts.selected_uuid.as_deref(),
                );
                (loaded.entries, disk_ancestry)
            }
            None if source.last_uuid.is_none() => {
                (Vec::new(), std::collections::HashSet::new())
            }
            None => return Err(failed_replay(
                state,
                source,
                format!(
                    "failed to rebuild replay window for {session_id}: transcript is missing or has no canonical chain"
                ),
            )),
        };

        let last_pending_index = source
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.uuid.as_str(), index))
            .collect::<std::collections::HashMap<_, _>>();
        let disk_by_uuid = entries
            .iter()
            .map(|entry| (entry.uuid.as_str(), entry))
            .collect::<std::collections::HashMap<_, _>>();
        *retained = source
            .entries
            .iter()
            .enumerate()
            .filter(|(index, pending)| {
                last_pending_index.get(pending.uuid.as_str()) == Some(index)
                    && disk_by_uuid
                        .get(pending.uuid.as_str())
                        .is_none_or(|disk| !same_raw_entry(disk, pending))
            })
            .map(|(_, entry)| entry.clone())
            .collect();
        drop(disk_by_uuid);

        entries.extend(source.entries.iter().cloned());
        if entries.is_empty() {
            entries
        } else {
            let Some(rebuilt) = reconstruct_replay_chain(entries) else {
                return Err(failed_replay(
                    state,
                    source,
                    format!(
                        "failed to rebuild replay window for {session_id}: pending overlay has no canonical chain"
                    ),
                ));
            };
            let rebuilt_by_uuid = rebuilt
                .messages
                .iter()
                .map(|entry| (entry.uuid.as_str(), entry))
                .collect::<std::collections::HashMap<_, _>>();
            let required_survives = retained.iter().all(|required| {
                rebuilt_by_uuid
                    .get(required.uuid.as_str())
                    .is_some_and(|entry| same_raw_entry(entry, required))
            });
            if !required_survives
                || disk_ancestry
                    .iter()
                    .any(|uuid| !rebuilt_by_uuid.contains_key(uuid.as_str()))
            {
                return Err(failed_replay(
                    state,
                    source,
                    format!(
                        "failed to rebuild replay window for {session_id}: pending overlay is not continuous with the canonical disk chain"
                    ),
                ));
            }
            rebuilt.messages
        }
    };
    Ok((source, raw))
}

fn failed_replay(
    state: &ServerState,
    source: rebon_session_state::ReplayTranscriptSource,
    message: String,
) -> PromptExecutorError {
    match state.restore_transcript_after_failed_replay(source) {
        Ok(()) => PromptExecutorError::Execution(message),
        Err(recovery) => PromptExecutorError::Execution(format!(
            "{message}; failed to restore replay recovery ownership: {recovery}"
        )),
    }
}

impl ReplayWindowStore {
    pub(super) fn prepare_resume_summary(
        entries: &[rebon_session::TranscriptEntry],
        projection: Vec<ApiMessage>,
    ) -> Result<PreparedResumeSummary, String> {
        let anchor_entry = entries
            .last()
            .cloned()
            .ok_or_else(|| "Cannot summarize an empty transcript.".to_string())?;
        Ok(PreparedResumeSummary {
            projection,
            anchor_uuid: anchor_entry.uuid.clone(),
            anchor_entry,
            prefix_hash: transcript_prefix_hash(entries),
        })
    }

    pub(super) fn install_resume_summary(
        &self,
        session_id: String,
        prepared: PreparedResumeSummary,
    ) {
        self.invalidate(&session_id);
        self.resume_summaries
            .lock()
            .expect("resume summary store mutex poisoned")
            .insert(
                session_id,
                ResumeSummaryBaseline {
                    projection: prepared.projection,
                    anchor_uuid: prepared.anchor_uuid,
                    anchor_entry: prepared.anchor_entry,
                    prefix_hash: prepared.prefix_hash,
                },
            );
    }

    /// Install a baseline that survives this process.
    ///
    /// Used by `/compact`, where the whole point is that the *next* turn
    /// replays less — and that turn may well run in a session this store no
    /// longer exists for.
    pub(super) fn install_persistent_resume_summary(
        &self,
        projects_root: &std::path::Path,
        cwd: &str,
        session_id: String,
        prepared: PreparedResumeSummary,
    ) {
        let path = compact_baseline_path(projects_root, cwd, &session_id);
        let baseline = ResumeSummaryBaseline {
            projection: prepared.projection.clone(),
            anchor_uuid: prepared.anchor_uuid.clone(),
            anchor_entry: prepared.anchor_entry.clone(),
            prefix_hash: prepared.prefix_hash,
        };
        if let Err(error) = write_compact_baseline(&path, &baseline) {
            tracing::warn!(
                %session_id,
                path = %path.display(),
                %error,
                "failed to persist the compacted replay baseline; it will not survive this process"
            );
        }
        self.install_resume_summary(session_id, prepared);
    }

    pub(super) fn clear_resume_summary(&self, session_id: &str) {
        self.resume_summaries
            .lock()
            .expect("resume summary store mutex poisoned")
            .remove(session_id);
        self.invalidate(session_id);
    }

    /// Drop a persisted baseline, in memory and on disk.
    pub(super) fn clear_persistent_resume_summary(
        &self,
        projects_root: &std::path::Path,
        cwd: &str,
        session_id: &str,
    ) {
        let _ = std::fs::remove_file(compact_baseline_path(projects_root, cwd, session_id));
        self.clear_resume_summary(session_id);
    }

    pub(super) fn project_resume_summary(
        &self,
        projects_root: &std::path::Path,
        cwd: &str,
        session_id: &str,
        raw: &[rebon_session::TranscriptEntry],
    ) -> Result<Option<Vec<ApiMessage>>, String> {
        let baseline = self
            .resume_summaries
            .lock()
            .expect("resume summary store mutex poisoned")
            .get(session_id)
            .cloned();
        let Some(baseline) = baseline else {
            return Ok(self.project_persisted_resume_summary(projects_root, cwd, session_id, raw));
        };
        let Some(anchor_index) = raw
            .iter()
            .position(|entry| entry.uuid == baseline.anchor_uuid)
        else {
            return Err(format!(
                "resume summary baseline for {session_id} is no longer present in canonical history"
            ));
        };
        if !same_raw_entry(&raw[anchor_index], &baseline.anchor_entry)
            || transcript_prefix_hash(&raw[..=anchor_index]) != baseline.prefix_hash
        {
            return Err(format!(
                "resume summary baseline for {session_id} no longer matches canonical history"
            ));
        }
        let mut projection = baseline.projection;
        projection.extend(transcript_to_api_messages(&raw[anchor_index + 1..]));
        rebon_api::ensure_tool_result_pairing(&mut projection);
        Ok(Some(projection))
    }

    /// In-memory-only projection, for tests that are about the live baseline
    /// rather than the sidecar. The empty projects root makes every sidecar
    /// lookup miss without touching the filesystem.
    #[cfg(test)]
    pub(super) fn project_resume_summary_in_memory(
        &self,
        session_id: &str,
        raw: &[rebon_session::TranscriptEntry],
    ) -> Result<Option<Vec<ApiMessage>>, String> {
        self.project_resume_summary(std::path::Path::new(""), "", session_id, raw)
    }

    /// Apply a baseline written by an earlier process, if one still fits.
    ///
    /// Unlike the in-memory path this never fails the replay: a stale sidecar
    /// (the transcript was rewound, or edited upstream of the anchor) is
    /// deleted and the turn falls back to full history. Refusing the turn
    /// instead would let a leftover file from a rewound session brick it,
    /// and the cost of the fallback is only that the context is bigger than
    /// the user asked for — never that it is wrong.
    fn project_persisted_resume_summary(
        &self,
        projects_root: &std::path::Path,
        cwd: &str,
        session_id: &str,
        raw: &[rebon_session::TranscriptEntry],
    ) -> Option<Vec<ApiMessage>> {
        let path = compact_baseline_path(projects_root, cwd, session_id);
        let persisted = read_compact_baseline(&path)?;
        let anchor_index = raw
            .iter()
            .position(|entry| entry.uuid == persisted.anchor_uuid)
            .filter(|index| transcript_prefix_hash(&raw[..=*index]) == persisted.prefix_hash);
        let Some(anchor_index) = anchor_index else {
            tracing::info!(
                %session_id,
                "dropping a compacted replay baseline that no longer matches this transcript"
            );
            let _ = std::fs::remove_file(&path);
            return None;
        };

        // Re-seat it in memory so later turns in this process skip the file.
        self.resume_summaries
            .lock()
            .expect("resume summary store mutex poisoned")
            .insert(
                session_id.to_string(),
                ResumeSummaryBaseline {
                    projection: persisted.projection.clone(),
                    anchor_uuid: persisted.anchor_uuid,
                    anchor_entry: raw[anchor_index].clone(),
                    prefix_hash: persisted.prefix_hash,
                },
            );

        let mut projection = persisted.projection;
        projection.extend(transcript_to_api_messages(&raw[anchor_index + 1..]));
        rebon_api::ensure_tool_result_pairing(&mut projection);
        Some(projection)
    }

    pub(super) fn has_anchored_minimal_anchor(&self, session_id: &str) -> bool {
        self.windows
            .lock()
            .expect("replay window store mutex poisoned")
            .windows
            .get(session_id)
            .is_some_and(|window| window.has_anchored_minimal_anchor)
    }

    pub(super) fn has_assistant_turn(&self, session_id: &str) -> bool {
        self.windows
            .lock()
            .expect("replay window store mutex poisoned")
            .windows
            .get(session_id)
            .is_some_and(|window| window.has_assistant_turn)
    }

    pub(super) fn history_for(
        &self,
        state: &ServerState,
        projects_root: &std::path::Path,
        session_id: &str,
    ) -> Result<(Vec<ApiMessage>, Option<String>, Option<String>), PromptExecutorError> {
        self.history_for_after_each_take(state, projects_root, session_id, |_| {})
    }

    #[cfg(test)]
    fn history_for_after_take<F>(
        &self,
        state: &ServerState,
        projects_root: &std::path::Path,
        session_id: &str,
        after_take: F,
    ) -> Result<(Vec<ApiMessage>, Option<String>, Option<String>), PromptExecutorError>
    where
        F: FnOnce(),
    {
        let mut after_take = Some(after_take);
        self.history_for_after_each_take(state, projects_root, session_id, |_| {
            if let Some(after_take) = after_take.take() {
                after_take();
            }
        })
    }

    fn history_for_after_each_take<F>(
        &self,
        state: &ServerState,
        projects_root: &std::path::Path,
        session_id: &str,
        mut after_take: F,
    ) -> Result<(Vec<ApiMessage>, Option<String>, Option<String>), PromptExecutorError>
    where
        F: FnMut(usize),
    {
        let mut conflict_retries = 0usize;
        // If a complete in-memory source conflicts, the retry source contains
        // that complete prefix plus rows appended during the lease. Keep the
        // first snapshot only for this bounded acquisition so later attempts can
        // identify and validate the pending delta without retaining raw history
        // after the operation returns.
        let mut complete_baseline: Option<
            std::collections::HashMap<String, rebon_session::TranscriptEntry>,
        > = None;
        loop {
            let source = state
                .take_transcript_for_replay(session_id)
                .ok_or_else(|| {
                    PromptExecutorError::Execution(format!(
                        "session not found or replay handoff already active while loading replay: {session_id}"
                    ))
                })?;
            after_take(conflict_retries);

            if source.entries.is_empty() {
                let cached = {
                    let mut windows = self
                        .windows
                        .lock()
                        .expect("replay window store mutex poisoned");
                    let rendered = windows
                        .windows
                        .get(session_id)
                        .filter(|window| window.matches(&source))
                        .map(|window| {
                            (
                                window.render(),
                                window.last_raw_uuid.clone(),
                                window.title_conversation_text.clone(),
                            )
                        });
                    if rendered.is_some() {
                        windows.touch(session_id);
                    }
                    rendered
                };
                if let Some(rendered) = cached {
                    match state
                        .finalize_cached_transcript_after_replay(source, rendered.1.clone())
                        .map_err(|err| {
                            PromptExecutorError::Execution(format!(
                                "failed to finalize cached replay handoff for {session_id}: {err}"
                            ))
                        })? {
                        rebon_session_state::ReplayFinalizeOutcome::Finalized(live_last_uuid) => {
                            return Ok((rendered.0, live_last_uuid, rendered.2));
                        }
                        rebon_session_state::ReplayFinalizeOutcome::RetryRequired => {
                            self.invalidate(session_id);
                            if conflict_retries >= MAX_REPLAY_CONFLICT_RETRIES {
                                return Err(Self::unstable_replay_error(session_id));
                            }
                            conflict_retries += 1;
                            continue;
                        }
                    }
                }
            }

            let mut retained = Vec::new();
            let (source, raw) = raw_history_for_source(
                state,
                source,
                projects_root,
                session_id,
                &mut complete_baseline,
                &mut retained,
            )?;

            // Window boundaries are computed only after full conversion/filtering
            // and global tool-pair repair. Finalization verifies the same source
            // revision before either the projection or its parent may escape.
            let last_raw_uuid = raw.last().map(|entry| entry.uuid.clone());
            let canonical = transcript_to_api_messages(&raw);
            let canonical_has_anchored_minimal_anchor =
                history_has_anchored_minimal_anchor(&canonical);
            let canonical_has_assistant_turn = history_has_assistant_turn(&canonical);
            let normalized =
                match self.project_resume_summary(projects_root, &source.cwd, session_id, &raw) {
                    Ok(Some(projected)) => projected,
                    Ok(None) => canonical,
                    Err(message) => return Err(failed_replay(state, source, message)),
                };
            let mut window = ReplayWindow::from_normalized(&source, normalized);
            window.has_anchored_minimal_anchor = canonical_has_anchored_minimal_anchor;
            window.has_assistant_turn = canonical_has_assistant_turn;
            window.last_raw_uuid = last_raw_uuid.clone();
            let rendered = window.render();
            let title_conversation_text = window.title_conversation_text.clone();
            match state
                .finalize_transcript_after_replay(source, retained, last_raw_uuid)
                .map_err(|err| {
                    PromptExecutorError::Execution(format!(
                        "failed to finalize replay handoff for {session_id}: {err}"
                    ))
                })? {
                rebon_session_state::ReplayFinalizeOutcome::Finalized(live_last_uuid) => {
                    self.windows
                        .lock()
                        .expect("replay window store mutex poisoned")
                        .insert(session_id.to_string(), window);
                    return Ok((rendered, live_last_uuid, title_conversation_text));
                }
                rebon_session_state::ReplayFinalizeOutcome::RetryRequired => {
                    self.invalidate(session_id);
                    if conflict_retries >= MAX_REPLAY_CONFLICT_RETRIES {
                        return Err(Self::unstable_replay_error(session_id));
                    }
                    conflict_retries += 1;
                }
            }
        }
    }

    fn invalidate(&self, session_id: &str) {
        let mut windows = self
            .windows
            .lock()
            .expect("replay window store mutex poisoned");
        windows.windows.remove(session_id);
        windows.lru.retain(|id| id != session_id);
    }

    fn unstable_replay_error(session_id: &str) -> PromptExecutorError {
        PromptExecutorError::Execution(format!(
            "replay source for {session_id} remained unstable after {} retries",
            MAX_REPLAY_CONFLICT_RETRIES
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn text_message(role: Role, text: impl Into<String>) -> ApiMessage {
        ApiMessage {
            role,
            content: vec![ApiContentBlock::Text(TextBlock { text: text.into() })],
        }
    }

    fn tool_use_message(id: impl Into<String>) -> ApiMessage {
        let id = id.into();
        ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::ToolUse(ToolUseBlock {
                name: format!("Tool-{id}"),
                id,
                input: serde_json::json!({"value": 1}),
            })],
        }
    }

    fn tool_result_message(id: impl Into<String>, is_error: bool) -> ApiMessage {
        ApiMessage {
            role: Role::User,
            content: vec![ApiContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: id.into(),
                content: ToolResultContent::Text("result".into()),
                is_error,
            })],
        }
    }

    fn raw_text_entry(
        entry_type: &str,
        uuid: &str,
        parent_uuid: Option<&str>,
        text: &str,
    ) -> rebon_session::TranscriptEntry {
        rebon_session::TranscriptEntry {
            entry_type: entry_type.into(),
            uuid: uuid.into(),
            parent_uuid: parent_uuid.map(str::to_string),
            timestamp: None,
            raw: serde_json::json!({
                "type": entry_type,
                "uuid": uuid,
                "parentUuid": parent_uuid,
                "message": {"role": entry_type, "content": text}
            }),
        }
    }

    fn source(revision: u64) -> rebon_session_state::ReplayTranscriptSource {
        let state = ServerState::new();
        let session = state.create_session("/repo".into(), Vec::new());
        let mut source = state
            .take_transcript_for_replay(&session.id)
            .expect("test replay source");
        source.session_id = "s".into();
        source.incarnation = 7;
        source.revision = revision;
        source.last_uuid = Some(format!("raw-{revision}"));
        source
    }

    fn populated_disk_cache(
        cwd: &str,
        session_id: &str,
        rows: &[rebon_session::TranscriptEntry],
    ) -> (tempfile::TempDir, ServerState, ReplayWindowStore) {
        let root = tempfile::tempdir().unwrap();
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            rows.iter()
                .map(|entry| entry.raw.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load cache fixture");
        let store = ReplayWindowStore::default();
        store
            .history_for(&state, root.path(), session_id)
            .expect("populate matching replay window");
        (root, state, store)
    }

    #[test]
    fn resume_summary_projection_persists_across_new_tail_entries() {
        let store = ReplayWindowStore::default();
        let mut raw = vec![
            raw_text_entry("user", "u-old", None, "old"),
            raw_text_entry("assistant", "a-anchor", Some("u-old"), "answer"),
        ];
        let prepared = ReplayWindowStore::prepare_resume_summary(
            &raw,
            vec![text_message(Role::User, "fresh summary")],
        )
        .unwrap();
        store.install_resume_summary("session".to_string(), prepared);

        let initial = store
            .project_resume_summary_in_memory("session", &raw)
            .unwrap()
            .unwrap();
        assert_eq!(initial, vec![text_message(Role::User, "fresh summary")]);

        raw.push(raw_text_entry(
            "user",
            "u-new",
            Some("a-anchor"),
            "new work",
        ));
        let updated = store
            .project_resume_summary_in_memory("session", &raw)
            .unwrap()
            .unwrap();
        assert_eq!(updated.len(), 2);
        assert_eq!(updated[0], text_message(Role::User, "fresh summary"));
        assert_eq!(updated[1], text_message(Role::User, "new work"));
    }

    #[test]
    fn compacted_replay_uses_canonical_history_for_anchored_minimal_phase() {
        fn phase_after_compact(raw: Vec<rebon_session::TranscriptEntry>) -> bool {
            let root = tempfile::tempdir().unwrap();
            let cwd = "/anchored-compact";
            let session_id = "anchored-compact";
            let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                raw.iter()
                    .map(|entry| entry.raw.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();

            let state = ServerState::new();
            state
                .load_session(root.path(), session_id, cwd, None, Vec::new())
                .unwrap();
            let prepared = ReplayWindowStore::prepare_resume_summary(
                &raw,
                vec![text_message(
                    Role::User,
                    "<system-generated-history-summary>compacted</system-generated-history-summary>",
                )],
            )
            .unwrap();
            ReplayWindowStore::default().install_persistent_resume_summary(
                root.path(),
                cwd,
                session_id.to_string(),
                prepared,
            );

            let store = ReplayWindowStore::default();
            store.history_for(&state, root.path(), session_id).unwrap();
            store.has_anchored_minimal_anchor(session_id)
        }

        assert!(phase_after_compact(vec![
            raw_text_entry("user", "u1", None, "prompt"),
            raw_text_entry("assistant", "a1", Some("u1"), "answer"),
        ]));
        assert!(!phase_after_compact(vec![
            raw_text_entry("user", "u1", None, "first failed prompt"),
            raw_text_entry("user", "u2", Some("u1"), "second failed prompt"),
        ]));
        assert!(!phase_after_compact(vec![
            raw_text_entry("user", "u1", None, "prompt"),
            raw_text_entry("assistant", "a1", Some("u1"), ""),
        ]));
    }

    #[test]
    fn resume_summary_projection_rejects_changed_canonical_prefix() {
        let store = ReplayWindowStore::default();
        let raw = vec![
            raw_text_entry("user", "u-old", None, "old"),
            raw_text_entry("assistant", "a-anchor", Some("u-old"), "answer"),
        ];
        let prepared = ReplayWindowStore::prepare_resume_summary(
            &raw,
            vec![text_message(Role::User, "fresh summary")],
        )
        .unwrap();
        store.install_resume_summary("session".to_string(), prepared);
        let changed = vec![
            raw_text_entry("user", "u-old", None, "changed"),
            raw[1].clone(),
        ];

        assert!(store
            .project_resume_summary_in_memory("session", &changed)
            .is_err());
    }

    #[test]
    fn clearing_resume_summary_restores_canonical_projection() {
        let store = ReplayWindowStore::default();
        let raw = vec![raw_text_entry("user", "u1", None, "old")];
        let prepared = ReplayWindowStore::prepare_resume_summary(
            &raw,
            vec![text_message(Role::User, "fresh summary")],
        )
        .unwrap();
        store.install_resume_summary("session".to_string(), prepared);
        store.clear_resume_summary("session");

        assert!(store
            .project_resume_summary_in_memory("session", &raw)
            .unwrap()
            .is_none());
    }

    // ── Persisted compact baselines ───────────────────────────────
    //
    // These are what make `/compact` mean anything for a background job: it
    // rebuilds its session — and therefore this whole store — every turn, so
    // a baseline that only lives in memory is thrown away before the turn it
    // was meant to shrink ever runs.

    fn compact_baseline_fixture() -> (
        tempfile::TempDir,
        Vec<rebon_session::TranscriptEntry>,
        PreparedResumeSummary,
    ) {
        let root = tempfile::tempdir().expect("tempdir");
        let raw = vec![
            raw_text_entry("user", "u-old", None, "old"),
            raw_text_entry("assistant", "a-anchor", Some("u-old"), "answer"),
        ];
        let prepared = ReplayWindowStore::prepare_resume_summary(
            &raw,
            vec![text_message(Role::User, "fresh summary")],
        )
        .unwrap();
        (root, raw, prepared)
    }

    #[test]
    fn a_persisted_compact_baseline_is_read_back_by_a_store_that_never_saw_it() {
        let (root, raw, prepared) = compact_baseline_fixture();
        ReplayWindowStore::default().install_persistent_resume_summary(
            root.path(),
            "/work/proj",
            "session".to_string(),
            prepared,
        );

        // A different store stands in for the next turn's rebuilt session.
        let next_turn = ReplayWindowStore::default();
        let projected = next_turn
            .project_resume_summary(root.path(), "/work/proj", "session", &raw)
            .unwrap()
            .expect("the sidecar must survive the store that wrote it");

        assert_eq!(projected, vec![text_message(Role::User, "fresh summary")]);
    }

    #[test]
    fn a_persisted_baseline_still_appends_rows_written_after_the_compaction() {
        let (root, mut raw, prepared) = compact_baseline_fixture();
        ReplayWindowStore::default().install_persistent_resume_summary(
            root.path(),
            "/work/proj",
            "session".to_string(),
            prepared,
        );
        raw.push(raw_text_entry(
            "user",
            "u-new",
            Some("a-anchor"),
            "new work",
        ));

        let projected = ReplayWindowStore::default()
            .project_resume_summary(root.path(), "/work/proj", "session", &raw)
            .unwrap()
            .unwrap();

        assert_eq!(projected.len(), 2);
        assert_eq!(projected[1], text_message(Role::User, "new work"));
    }

    /// A rewind rewrites history under the baseline. Unlike the in-memory
    /// path (which fails the replay so the caller can recover), a leftover
    /// file must never be able to brick a turn — it is dropped and the turn
    /// replays real history.
    #[test]
    fn a_stale_persisted_baseline_is_deleted_and_falls_back_to_full_history() {
        let (root, raw, prepared) = compact_baseline_fixture();
        ReplayWindowStore::default().install_persistent_resume_summary(
            root.path(),
            "/work/proj",
            "session".to_string(),
            prepared,
        );
        let rewritten = vec![
            raw_text_entry("user", "u-old", None, "rewound"),
            raw[1].clone(),
        ];

        let store = ReplayWindowStore::default();
        assert!(store
            .project_resume_summary(root.path(), "/work/proj", "session", &rewritten)
            .unwrap()
            .is_none());
        // Dropped, not merely ignored: a second pass must not re-read it.
        assert!(!compact_baseline_path(root.path(), "/work/proj", "session").exists());
    }

    #[test]
    fn a_persisted_baseline_whose_anchor_is_gone_falls_back_to_full_history() {
        let (root, _raw, prepared) = compact_baseline_fixture();
        ReplayWindowStore::default().install_persistent_resume_summary(
            root.path(),
            "/work/proj",
            "session".to_string(),
            prepared,
        );
        let unrelated = vec![raw_text_entry("user", "u-other", None, "different session")];

        assert!(ReplayWindowStore::default()
            .project_resume_summary(root.path(), "/work/proj", "session", &unrelated)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_sidecar_from_a_future_version_is_ignored_rather_than_misread() {
        let (root, raw, prepared) = compact_baseline_fixture();
        ReplayWindowStore::default().install_persistent_resume_summary(
            root.path(),
            "/work/proj",
            "session".to_string(),
            prepared,
        );
        let path = compact_baseline_path(root.path(), "/work/proj", "session");
        let mut persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        persisted["version"] = serde_json::json!(RESUME_SUMMARY_SIDECAR_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec(&persisted).unwrap()).unwrap();

        assert!(ReplayWindowStore::default()
            .project_resume_summary(root.path(), "/work/proj", "session", &raw)
            .unwrap()
            .is_none());
    }

    #[test]
    fn clearing_a_persistent_baseline_removes_the_sidecar_too() {
        let (root, raw, prepared) = compact_baseline_fixture();
        let store = ReplayWindowStore::default();
        store.install_persistent_resume_summary(
            root.path(),
            "/work/proj",
            "session".to_string(),
            prepared,
        );
        store.clear_persistent_resume_summary(root.path(), "/work/proj", "session");

        assert!(!compact_baseline_path(root.path(), "/work/proj", "session").exists());
        assert!(ReplayWindowStore::default()
            .project_resume_summary(root.path(), "/work/proj", "session", &raw)
            .unwrap()
            .is_none());
    }

    #[test]
    fn normalized_window_matches_phase_one_reference_and_retains_only_projection() {
        let mut messages = Vec::new();
        for turn in 0..500 {
            messages.push(text_message(Role::User, format!("user-{turn}")));
            messages.push(text_message(Role::Assistant, format!("assistant-{turn}")));
        }
        let expected = rebon_api::auto_compact_truncate_cache_stable(messages.clone(), 10);
        let window = ReplayWindow::from_normalized(&source(1), messages);
        assert_eq!(window.render(), expected);
        // 1000 messages sit exactly on a quantum boundary, so the projection is
        // preserved-user + compaction-summary + one 20-message window at most.
        // This fixture has no execution row.
        assert!(window.projection.len() <= 24);
    }

    #[test]
    fn replay_projection_prefix_stays_stable_while_turns_append() {
        // Simulates the per-turn rebuild: within a quantum band the previous
        // projection must be a byte-identical prefix of the next one, so
        // provider prompt caches keep hitting as the session grows; the
        // boundary may move only in whole-window jumps.
        let build = |turns: usize| {
            let mut messages = Vec::new();
            for turn in 0..turns {
                messages.push(text_message(Role::User, format!("user-{turn}")));
                messages.push(text_message(Role::Assistant, format!("assistant-{turn}")));
            }
            ReplayWindow::from_normalized(&source(1), messages).render()
        };

        let mut previous = build(30);
        let mut jumps = 0usize;
        for turns in 31..=60 {
            let current = build(turns);
            let extends_previous =
                current.len() >= previous.len() && current[..previous.len()] == previous[..];
            if !extends_previous {
                jumps += 1;
            }
            previous = current;
        }
        // 30 appended turn-pairs with a 10-turn protected window cross the
        // quantum boundary exactly three times (at 40, 50, and 60 turns).
        assert_eq!(
            jumps, 3,
            "boundary must move in whole-window steps, not per turn"
        );
    }

    #[test]
    fn deterministic_message_matrix_matches_batch_reference() {
        for seed in 0..64usize {
            let mut messages = Vec::new();
            for turn in 0..(24 + seed % 17) {
                messages.push(text_message(
                    Role::User,
                    format!("seed-{seed}-user-{turn}-{}", "x".repeat((seed + turn) % 97)),
                ));
                if (seed + turn) % 4 == 0 {
                    let id = format!("tool-{}-{}", seed % 5, turn % 7);
                    messages.push(tool_use_message(&id));
                    messages.push(tool_result_message(&id, turn % 9 == 0));
                }
                messages.push(text_message(
                    Role::Assistant,
                    format!("seed-{seed}-assistant-{turn}"),
                ));
            }

            let expected = rebon_api::auto_compact_truncate_cache_stable(messages.clone(), 10);
            let actual = ReplayWindow::from_normalized(&source(seed as u64), messages).render();
            assert_eq!(actual, expected, "projection mismatch for seed {seed}");
        }
    }

    #[test]
    fn transcript_filtering_and_global_tool_repair_happen_before_windowing() {
        let entries = vec![
            raw_text_entry("user", "u-1", None, "hello"),
            rebon_session::TranscriptEntry {
                entry_type: "attachment".into(),
                uuid: "attachment-1".into(),
                parent_uuid: Some("u-1".into()),
                timestamp: None,
                raw: serde_json::json!({"type": "attachment", "text": "not model history"}),
            },
            rebon_session::TranscriptEntry {
                entry_type: "assistant".into(),
                uuid: "tool-use".into(),
                parent_uuid: Some("attachment-1".into()),
                timestamp: None,
                raw: serde_json::json!({
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "tool_use", "id": "orphan", "name": "Read", "input": {}}]
                    }
                }),
            },
            rebon_session::TranscriptEntry {
                entry_type: "user".into(),
                uuid: "empty".into(),
                parent_uuid: Some("tool-use".into()),
                timestamp: None,
                raw: serde_json::json!({"message": {"role": "user", "content": []}}),
            },
        ];
        let normalized = transcript_to_api_messages(&entries);
        assert_eq!(normalized.len(), 3, "tool orphan repair adds a user result");
        assert!(normalized.iter().all(|message| !message.content.is_empty()));
        let expected = rebon_api::auto_compact_truncate_cache_stable(normalized.clone(), 10);
        let actual = ReplayWindow::from_normalized(&source(1), normalized).render();
        assert_eq!(actual, expected);
    }

    #[test]
    fn failed_disk_rebuild_restores_pending_raw_suffix() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();
        let session = state.create_session("/missing/replay-project".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "u-1", None, "first")],
        );
        let store = ReplayWindowStore::default();
        store
            .history_for(&state, root.path(), &session.id)
            .expect("complete in-memory source builds without disk");
        store
            .history_for(&state, root.path(), &session.id)
            .expect("unchanged released source hits the engine window without disk");

        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "u-2", Some("u-1"), "pending")],
        );
        let err = store
            .history_for(&state, root.path(), &session.id)
            .expect_err("changed released source cannot rebuild without canonical disk");
        assert!(err
            .to_string()
            .contains("missing or has no canonical chain"));

        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("pending source restored after rebuild failure");
        assert!(!restored.complete);
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].uuid, "u-2");
    }

    #[test]
    fn concurrent_duplicate_is_rebuilt_before_returning_history_and_parent() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();
        let session = state.create_session("/replay/concurrent-duplicate".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![
                raw_text_entry("user", "D", None, "root"),
                raw_text_entry("assistant", "T", Some("D"), "tail"),
            ],
        );

        let (history, next_parent, _) = ReplayWindowStore::default()
            .history_for_after_take(&state, root.path(), &session.id, || {
                state.push_transcript_entries(
                    &session.id,
                    vec![raw_text_entry("user", "D", None, "concurrent rewrite")],
                );
            })
            .expect("the revision conflict is retried and rebuilt canonically");
        assert_eq!(next_parent.as_deref(), Some("T"));
        assert_eq!(
            history
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(ApiContentBlock::as_text)
                .collect::<Vec<_>>(),
            vec!["concurrent rewrite", "tail"]
        );
        assert_eq!(
            state.transcript_tail_uuid(&session.id).as_deref(),
            Some("T")
        );
        let retained = state
            .take_transcript_for_replay(&session.id)
            .expect("successful retry released the lease");
        assert_eq!(retained.entries.len(), 1);
        assert_eq!(retained.entries[0].uuid, "D");
        assert_eq!(
            retained.entries[0].raw["message"]["content"],
            "concurrent rewrite"
        );
    }

    #[test]
    fn concurrent_equal_time_branches_fail_closed_after_rebuild_retry() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();
        let session = state.create_session("/replay/concurrent-branches".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![
                raw_text_entry("user", "D", None, "root"),
                raw_text_entry("assistant", "T", Some("D"), "tail"),
            ],
        );

        let err = ReplayWindowStore::default()
            .history_for_after_take(&state, root.path(), &session.id, || {
                let mut left = raw_text_entry("user", "A", Some("T"), "left");
                left.timestamp = Some("2026-01-01T00:00:00Z".into());
                left.raw["timestamp"] = serde_json::json!(left.timestamp);
                let mut right = raw_text_entry("user", "B", Some("T"), "right");
                right.timestamp = Some("2026-01-01T00:00:00Z".into());
                right.raw["timestamp"] = serde_json::json!(right.timestamp);
                state.push_transcript_entries(&session.id, vec![left, right]);
            })
            .expect_err("both competing pending branches cannot be canonical");
        assert!(err
            .to_string()
            .contains("pending complete-source delta is not canonical"));
        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("failed rebuild released its lease and restored every row");
        assert_eq!(
            restored
                .entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["D", "T", "A", "B"]
        );
    }

    #[test]
    fn matching_cache_direct_append_is_rebuilt_model_visible_and_recreatable() {
        let cwd = "/replay/cache-direct";
        let session_id = "cache-direct";
        let durable = raw_text_entry("user", "D", None, "durable");
        let (root, state, store) = populated_disk_cache(cwd, session_id, &[durable]);

        let (history, next_parent, _) = store
            .history_for_after_take(&state, root.path(), session_id, || {
                state.push_transcript_entries(
                    session_id,
                    vec![raw_text_entry("assistant", "T", Some("D"), "concurrent")],
                );
            })
            .expect("cache conflict rebuilds direct append");
        assert_eq!(next_parent.as_deref(), Some("T"));
        assert!(history.iter().any(|message| message
            .content
            .iter()
            .any(|block| block.as_text() == Some("concurrent"))));
        assert_eq!(
            state
                .get_session(session_id)
                .unwrap()
                .loaded_transcript
                .len(),
            1
        );

        let (rebuilt, recreated_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("released lease and retained row survive store recreation");
        assert_eq!(recreated_parent.as_deref(), Some("T"));
        assert!(rebuilt.iter().any(|message| message
            .content
            .iter()
            .any(|block| block.as_text() == Some("concurrent"))));
    }

    #[test]
    fn matching_cache_duplicate_is_canonicalized_after_overlay() {
        let cwd = "/replay/cache-duplicate";
        let session_id = "cache-duplicate";
        let durable = raw_text_entry("user", "D", None, "old D");
        let (root, state, store) = populated_disk_cache(cwd, session_id, &[durable]);

        let (history, next_parent, _) = store
            .history_for_after_take(&state, root.path(), session_id, || {
                state.push_transcript_entries(
                    session_id,
                    vec![raw_text_entry("user", "D", None, "new D")],
                );
            })
            .expect("duplicate cache conflict uses rebuilt LWW result");
        let texts = history
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(ApiContentBlock::as_text)
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["new D"]);
        assert_eq!(next_parent.as_deref(), Some("D"));

        let (rebuilt, recreated_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("duplicate override survives store recreation");
        assert_eq!(recreated_parent.as_deref(), Some("D"));
        assert_eq!(rebuilt[0].content[0].as_text(), Some("new D"));
    }

    #[test]
    fn matching_cache_equal_time_branch_fails_closed_without_losing_rows_or_lease() {
        let cwd = "/replay/cache-branch";
        let session_id = "cache-branch";
        let mut durable = raw_text_entry("user", "D", None, "durable");
        durable.timestamp = Some("2026-01-01T00:00:00Z".into());
        durable.raw["timestamp"] = serde_json::json!(durable.timestamp);
        let mut tail = raw_text_entry("assistant", "T", Some("D"), "tail");
        tail.timestamp = Some("2026-01-01T00:00:01Z".into());
        tail.raw["timestamp"] = serde_json::json!(tail.timestamp);
        let (root, state, store) = populated_disk_cache(cwd, session_id, &[durable, tail]);

        let err = store
            .history_for_after_take(&state, root.path(), session_id, || {
                let mut branch = raw_text_entry("assistant", "B", Some("D"), "branch");
                branch.timestamp = Some("2026-01-01T00:00:01Z".into());
                branch.raw["timestamp"] = serde_json::json!(branch.timestamp);
                state.push_transcript_entries(session_id, vec![branch]);
            })
            .expect_err("equal-time competing branch must use rebuilt rejection");
        assert!(err.to_string().contains("not continuous"));

        let retained = state
            .take_transcript_for_replay(session_id)
            .expect("failure released cache and rebuild leases");
        assert_eq!(retained.entries.len(), 1);
        assert_eq!(retained.entries[0].uuid, "B");
        state
            .restore_transcript_after_failed_replay(retained)
            .expect("release inspection lease");
        let recreated_err = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect_err("retained branch remains rejected after store recreation");
        assert!(recreated_err.to_string().contains("not continuous"));
    }

    #[test]
    fn matching_cache_two_appends_rebuild_to_exact_live_tail() {
        let cwd = "/replay/cache-two-appends";
        let session_id = "cache-two-appends";
        let durable = raw_text_entry("user", "D", None, "durable");
        let (root, state, store) = populated_disk_cache(cwd, session_id, &[durable]);

        let (history, next_parent, _) = store
            .history_for_after_take(&state, root.path(), session_id, || {
                state.push_transcript_entries(
                    session_id,
                    vec![
                        raw_text_entry("assistant", "T", Some("D"), "first append"),
                        raw_text_entry("user", "U", Some("T"), "second append"),
                    ],
                );
            })
            .expect("both concurrent rows are rebuilt");
        assert_eq!(next_parent.as_deref(), Some("U"));
        let texts = history
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(ApiContentBlock::as_text)
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["durable", "first append", "second append"]);
        assert_eq!(
            state
                .get_session(session_id)
                .unwrap()
                .loaded_transcript
                .len(),
            2
        );

        let (_, recreated_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("two-row recovery survives store recreation");
        assert_eq!(recreated_parent.as_deref(), Some("U"));
    }

    #[test]
    fn released_store_miss_rebuilds_complete_canonical_disk_history() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/replay/resume";
        let session_id = "resume-store-miss";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows = [
            serde_json::json!({
                "type": "user",
                "uuid": "u-1",
                "message": {"role": "user", "content": "question"}
            }),
            serde_json::json!({
                "type": "assistant",
                "uuid": "a-1",
                "parentUuid": "u-1",
                "message": {"role": "assistant", "content": "answer"}
            }),
        ];
        std::fs::write(
            &path,
            rows.iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load complete presentation history");
        assert!(state.release_transcript_residency(session_id));

        let (history, tail, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("engine store miss rebuilds from canonical JSONL");
        assert_eq!(tail.as_deref(), Some("a-1"));
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content[0].as_text(), Some("question"));
        assert_eq!(history[1].content[0].as_text(), Some("answer"));
    }

    #[test]
    fn concurrent_append_during_replay_becomes_the_next_parent() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();
        let session = state.create_session("/replay/concurrent-tail".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "u-1", None, "question")],
        );

        let (_, next_parent_uuid, _) = ReplayWindowStore::default()
            .history_for_after_take(&state, root.path(), &session.id, || {
                state.push_transcript_entries(
                    &session.id,
                    vec![raw_text_entry(
                        "assistant",
                        "a-concurrent",
                        Some("u-1"),
                        "concurrent answer",
                    )],
                );
            })
            .expect("replay finalization reconciles the live tail");
        assert_eq!(next_parent_uuid.as_deref(), Some("a-concurrent"));
        let next = raw_text_entry(
            "user",
            "u-next",
            next_parent_uuid.as_deref(),
            "next question",
        );
        assert_eq!(next.parent_uuid.as_deref(), Some("a-concurrent"));
        assert_eq!(
            state.transcript_tail_uuid(&session.id).as_deref(),
            Some("a-concurrent")
        );
    }

    #[test]
    fn disk_backed_store_miss_uses_canonical_leaf_not_stale_acp_metadata() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/replay/canonical-tail";
        let session_id = "canonical-tail-store-miss";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let user = raw_text_entry("user", "u-1", None, "question");
        let stale = raw_text_entry("assistant", "a-stale", Some("u-1"), "stale answer");
        std::fs::write(&path, format!("{}\n{}", user.raw, stale.raw)).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load establishes stale ACP tail metadata");
        assert_eq!(
            state.transcript_tail_uuid(session_id).as_deref(),
            Some("a-stale")
        );
        assert!(state.release_transcript_residency(session_id));

        let canonical = raw_text_entry("assistant", "a-disk", Some("u-1"), "canonical answer");
        std::fs::write(&path, format!("{}\n{}", user.raw, canonical.raw)).unwrap();

        let (_, next_parent_uuid, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("store miss rebuilds from canonical disk chain");
        assert_eq!(next_parent_uuid.as_deref(), Some("a-disk"));
        let next_entry = raw_text_entry(
            "user",
            "u-next",
            next_parent_uuid.as_deref(),
            "next question",
        );
        assert_eq!(next_entry.parent_uuid.as_deref(), Some("a-disk"));
        assert_eq!(
            state.transcript_tail_uuid(session_id).as_deref(),
            Some("a-disk"),
            "same-incarnation ACP metadata is reconciled to the rebuilt leaf"
        );
    }

    #[test]
    fn replacement_incarnation_cannot_reuse_stale_projection() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();
        let session = state.create_session("/replacement".into(), Vec::new());
        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "old", None, "old history")],
        );
        let store = ReplayWindowStore::default();
        store
            .history_for(&state, root.path(), &session.id)
            .expect("initial window");

        assert!(state.replace_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "new", None, "replacement history")],
        ));
        let (history, tail, _) = store
            .history_for(&state, root.path(), &session.id)
            .expect("replacement complete source rebuilds without stale cache reuse");
        assert_eq!(tail.as_deref(), Some("new"));
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content[0].as_text(), Some("replacement history"));
    }

    #[test]
    fn ordinary_retained_projection_does_not_grow_with_session_length() {
        fn build(turns: usize) -> ReplayWindow {
            let messages = (0..turns)
                .flat_map(|turn| {
                    [
                        text_message(Role::User, format!("user-{turn}-{}", "x".repeat(400))),
                        text_message(Role::Assistant, format!("assistant-{turn}")),
                    ]
                })
                .collect();
            ReplayWindow::from_normalized(&source(turns as u64), messages)
        }

        let medium = build(500);
        let long = build(5_000);
        assert_eq!(medium.projection.len(), long.projection.len());
        let medium_bytes = serde_json::to_vec(&medium.projection).unwrap().len();
        let long_bytes = serde_json::to_vec(&long.projection).unwrap().len();
        assert!(long_bytes < medium_bytes.saturating_mul(2));
    }

    #[test]
    fn source_change_requires_exact_rebuild() {
        let messages = vec![text_message(Role::User, "hello")];
        let window = ReplayWindow::from_normalized(&source(1), messages);
        assert!(window.matches(&source(1)));
        assert!(!window.matches(&source(2)));
        let mut relocated = source(1);
        relocated.cwd = "/other".into();
        assert!(!window.matches(&relocated));
        let mut reincarnated = source(1);
        reincarnated.incarnation += 1;
        assert!(!window.matches(&reincarnated));
    }

    #[test]
    fn title_input_is_extracted_from_complete_normalized_history() {
        let full = (0..40)
            .flat_map(|turn| {
                [
                    text_message(Role::User, format!("original-user-{turn}")),
                    text_message(Role::Assistant, format!("original-assistant-{turn}")),
                ]
            })
            .collect::<Vec<_>>();
        let expected = rebon_api::extract_conversation_text(&full);
        let window = ReplayWindow::from_normalized(&source(1), full);
        assert_eq!(
            window.title_conversation_text.as_deref(),
            Some(expected.as_str())
        );
        assert_ne!(
            window.title_conversation_text.as_deref(),
            Some(rebon_api::extract_conversation_text(&window.projection).as_str()),
            "compacted synthetic summaries must not feed title generation"
        );
    }

    #[test]
    fn successful_overlay_retains_unpersisted_rows_across_store_recreation() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/overlay-recovery";
        let state = ServerState::new();
        let session = state.create_session(cwd.into(), Vec::new());
        let path = rebon_session::transcript_file_path(root.path(), cwd, &session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "u-1", None, "durable");
        std::fs::write(&path, durable.raw.to_string()).unwrap();
        assert!(state.replace_transcript_entries(&session.id, vec![durable]));
        let first_store = ReplayWindowStore::default();
        first_store
            .history_for(&state, root.path(), &session.id)
            .expect("initial complete handoff");

        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry(
                "assistant",
                "a-undurable",
                Some("u-1"),
                "survives append failure",
            )],
        );
        let (history, _, _) = first_store
            .history_for(&state, root.path(), &session.id)
            .expect("pending row overlays durable disk");
        assert!(history.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| block.as_text() == Some("survives append failure"))
        }));
        assert_eq!(
            state
                .get_session(&session.id)
                .unwrap()
                .loaded_transcript
                .len(),
            1,
            "ACP retains the only process-local copy of the undurable row"
        );

        let (rebuilt, _, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), &session.id)
            .expect("fresh store rebuild includes retained undurable row");
        assert!(rebuilt.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| block.as_text() == Some("survives append failure"))
        }));
    }

    #[test]
    fn nonempty_unparseable_or_chainless_disk_fails_but_empty_new_history_is_valid() {
        let cases: &[(&str, &[u8])] = &[
            ("not-json", b"not-json\n"),
            ("invalid-utf8", &[0xff, 0xfe, b'\n']),
            (
                "only-system",
                b"{\"type\":\"system\",\"uuid\":\"system-only\"}\n",
            ),
        ];
        for (name, bytes) in cases {
            let root = tempfile::tempdir().unwrap();
            let cwd = format!("/replay/{name}");
            let state = ServerState::new();
            let session = state.create_session(cwd.clone(), Vec::new());
            assert!(state.release_transcript_residency(&session.id));
            let path = rebon_session::transcript_file_path(root.path(), &cwd, &session.id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();

            let err = ReplayWindowStore::default()
                .history_for(&state, root.path(), &session.id)
                .expect_err("nonempty disk without a canonical message chain must fail");
            let text = err.to_string();
            assert!(
                text.contains("nonempty transcript")
                    || text.contains("malformed or unsupported JSONL records"),
                "{name}: {err}"
            );
            let restored = state
                .take_transcript_for_replay(&session.id)
                .expect("failed recovery restores the handoff");
            assert!(restored.entries.is_empty());
            state
                .restore_transcript_after_failed_replay(restored)
                .expect("release inspection handoff");
        }

        let root = tempfile::tempdir().unwrap();
        let cwd = "/replay/genuinely-empty";
        let state = ServerState::new();
        let session = state.create_session(cwd.into(), Vec::new());
        assert!(state.release_transcript_residency(&session.id));
        let path = rebon_session::transcript_file_path(root.path(), cwd, &session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, []).unwrap();
        let (history, tail, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), &session.id)
            .expect("a zero-byte new-session transcript is genuinely empty");
        assert!(history.is_empty());
        assert!(tail.is_none());
    }

    #[test]
    fn valid_prefix_plus_malformed_former_tail_cannot_regress_canonical_tail() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/replay/partial-malformed-tail";
        let state = ServerState::new();
        let session = state.create_session(cwd.into(), Vec::new());
        let root_entry = raw_text_entry("user", "D", None, "durable root");
        let tail_entry = raw_text_entry("assistant", "T", Some("D"), "former tail");
        assert!(state.replace_transcript_entries(&session.id, vec![root_entry.clone(), tail_entry]));
        let store = ReplayWindowStore::default();
        store
            .history_for(&state, root.path(), &session.id)
            .expect("initial complete source establishes T");

        let path = rebon_session::transcript_file_path(root.path(), cwd, &session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{}\nnot-json\n", root_entry.raw)).unwrap();
        RECONSTRUCT_CALLS.with(|calls| calls.set(0));
        let err = ReplayWindowStore::default()
            .history_for(&state, root.path(), &session.id)
            .expect_err("a malformed former tail must not be dropped as best-effort garbage");
        assert_eq!(
            RECONSTRUCT_CALLS.with(std::cell::Cell::get),
            0,
            "parse-incomplete input must fail before canonical reconstruction"
        );
        assert!(err
            .to_string()
            .contains("malformed or unsupported JSONL records"));
        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("failed rebuild restores replay ownership");
        assert_eq!(restored.last_uuid.as_deref(), Some("T"));
        state
            .restore_transcript_after_failed_replay(restored)
            .expect("release inspection handoff");
    }

    #[test]
    fn explicit_load_overlays_same_uuid_recovery_and_survives_store_recreation() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/explicit-overlay-recreation";
        let session_id = "explicit-overlay-recreation";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        let old_parent = raw_text_entry("user", "P", Some("D"), "old disk parent");
        let child = raw_text_entry("assistant", "C", Some("P"), "disk child");
        std::fs::write(
            &path,
            format!("{}\n{}\n{}", durable.raw, old_parent.raw, child.raw),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("initial disk load");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(
            session_id,
            vec![raw_text_entry("user", "P", Some("D"), "pending P-new")],
        );

        let presented = state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("explicit load overlays pending rows after raw disk rows");
        assert_eq!(
            presented.loaded_transcript[1].raw["message"]["content"],
            "pending P-new"
        );
        let resident = state.get_session(session_id).expect("resident recovery");
        assert_eq!(resident.loaded_transcript.len(), 1);
        assert_eq!(
            resident.loaded_transcript[0].raw["message"]["content"],
            "pending P-new"
        );

        for _ in 0..2 {
            let (history, next_parent, _) = ReplayWindowStore::default()
                .history_for(&state, root.path(), session_id)
                .expect("fresh replay store retains the same-UUID recovery overlay");
            let texts = history
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(ApiContentBlock::as_text)
                .collect::<Vec<_>>();
            assert_eq!(texts, vec!["durable root", "pending P-new", "disk child"]);
            assert_eq!(next_parent.as_deref(), Some("C"));
        }
    }

    #[test]
    fn explicit_load_rejects_pending_branch_without_mutating_recovery() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/explicit-overlay-branch";
        let session_id = "explicit-overlay-branch";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut root_entry = raw_text_entry("user", "D", None, "root");
        root_entry.timestamp = Some("2026-01-01T00:00:00Z".into());
        root_entry.raw["timestamp"] = serde_json::json!(root_entry.timestamp);
        let mut tail = raw_text_entry("assistant", "T", Some("D"), "tail");
        tail.timestamp = Some("2026-01-01T00:00:01Z".into());
        tail.raw["timestamp"] = serde_json::json!(tail.timestamp);
        std::fs::write(&path, format!("{}\n{}", root_entry.raw, tail.raw)).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("initial disk load");
        assert!(state.release_transcript_residency(session_id));
        let mut branch = raw_text_entry("assistant", "X", Some("D"), "competing branch");
        branch.timestamp = Some("2026-01-02T00:00:00Z".into());
        branch.raw["timestamp"] = serde_json::json!(branch.timestamp);
        state.push_transcript_entries(session_id, vec![branch.clone()]);

        let err = state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect_err("explicit load must not let a pending branch replace the disk tail");
        assert!(err.message.contains("not continuous"));
        let resident = state
            .get_session(session_id)
            .expect("recovery remains resident");
        assert_eq!(resident.loaded_transcript.len(), 1);
        assert_eq!(resident.loaded_transcript[0].uuid, "X");
        assert_eq!(state.transcript_tail_uuid(session_id).as_deref(), Some("T"));
    }

    #[test]
    fn raw_disk_rows_are_overlaid_before_single_canonical_reconstruction() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-parent";
        let session_id = "raw-overlay-parent";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        let child = raw_text_entry("assistant", "C", Some("P"), "disk child");
        std::fs::write(&path, format!("{}\n{}", durable.raw, child.raw)).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load current public canonical view");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(
            session_id,
            vec![raw_text_entry("user", "P", Some("D"), "recovery parent")],
        );

        RECONSTRUCT_CALLS.with(|calls| calls.set(0));
        let (history, next_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("raw disk plus recovery reconstructs one connected chain");
        assert_eq!(
            RECONSTRUCT_CALLS.with(std::cell::Cell::get),
            1,
            "recovery must reconstruct exactly once, after the pending overlay"
        );
        let texts = history
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(ApiContentBlock::as_text)
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["durable root", "recovery parent", "disk child"]);
        assert_eq!(next_parent.as_deref(), Some("C"));
    }

    #[test]
    fn pending_duplicate_uuid_is_last_writer_before_branch_selection() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-duplicate";
        let session_id = "raw-overlay-duplicate";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        let stale_parent = raw_text_entry("user", "P", Some("other-root"), "stale parent");
        let child = raw_text_entry("assistant", "C", Some("P"), "disk child");
        std::fs::write(
            &path,
            format!("{}\n{}\n{}", durable.raw, stale_parent.raw, child.raw),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(
            session_id,
            vec![raw_text_entry("user", "P", Some("D"), "replacement parent")],
        );

        let (history, next_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("pending duplicate wins before reconstruction");
        let texts = history
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(ApiContentBlock::as_text)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["durable root", "replacement parent", "disk child"]
        );
        assert_eq!(next_parent.as_deref(), Some("C"));
    }

    #[test]
    fn duplicate_uuid_overlay_survives_store_recreation() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-duplicate-recreation";
        let session_id = "raw-overlay-duplicate-recreation";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        let stale_parent = raw_text_entry("user", "P", Some("other-root"), "stale parent");
        let child = raw_text_entry("assistant", "C", Some("P"), "disk child");
        std::fs::write(
            &path,
            format!("{}\n{}\n{}", durable.raw, stale_parent.raw, child.raw),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(
            session_id,
            vec![raw_text_entry("user", "P", Some("D"), "replacement parent")],
        );

        ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("first overlay succeeds");
        assert_eq!(
            state
                .get_session(session_id)
                .unwrap()
                .loaded_transcript
                .len(),
            1,
            "the non-durable replacement must remain resident despite sharing a disk UUID"
        );
        let (rebuilt, next_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("fresh store reuses retained duplicate override");
        let texts = rebuilt
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(ApiContentBlock::as_text)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["durable root", "replacement parent", "disk child"]
        );
        assert_eq!(next_parent.as_deref(), Some("C"));
    }

    #[test]
    fn pending_duplicate_validation_uses_only_the_final_uuid_occurrence() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-final-pending";
        let session_id = "raw-overlay-final-pending";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        std::fs::write(&path, durable.raw.to_string()).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(
            session_id,
            vec![
                raw_text_entry("assistant", "P", Some("missing"), "obsolete pending"),
                raw_text_entry("assistant", "P", Some("D"), "final pending"),
            ],
        );

        let (history, next_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("only the final pending value is required to survive reconstruction");
        assert_eq!(next_parent.as_deref(), Some("P"));
        assert_eq!(history[1].content[0].as_text(), Some("final pending"));
        let retained = state
            .take_transcript_for_replay(session_id)
            .expect("inspect retained recovery suffix");
        assert_eq!(retained.entries.len(), 1);
        assert_eq!(retained.entries[0].uuid, "P");
        assert_eq!(
            retained.entries[0].raw["message"]["content"],
            "final pending"
        );
    }

    #[test]
    fn final_pending_duplicate_rewrites_disk_middle_and_survives_recreation() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-two-pending-writers";
        let session_id = "raw-overlay-two-pending-writers";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        let old_parent = raw_text_entry("assistant", "P", Some("D"), "disk parent");
        let child = raw_text_entry("user", "C", Some("P"), "disk child");
        std::fs::write(
            &path,
            format!("{}\n{}\n{}", durable.raw, old_parent.raw, child.raw),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(
            session_id,
            vec![
                raw_text_entry("assistant", "P", Some("D"), "pending parent one"),
                raw_text_entry("assistant", "P", Some("D"), "pending parent two"),
            ],
        );

        let (history, next_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("the final pending writer replaces the disk middle row");
        assert_eq!(next_parent.as_deref(), Some("C"));
        assert_eq!(
            history
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(ApiContentBlock::as_text)
                .collect::<Vec<_>>(),
            vec!["durable root", "pending parent two", "disk child"]
        );
        let resident = state.get_session(session_id).expect("live session");
        assert_eq!(resident.loaded_transcript.len(), 1);
        assert_eq!(resident.loaded_transcript[0].uuid, "P");
        assert_eq!(
            resident.loaded_transcript[0].raw["message"]["content"],
            "pending parent two"
        );

        let (rebuilt, recreated_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("the final pending writer survives store recreation");
        assert_eq!(recreated_parent.as_deref(), Some("C"));
        assert_eq!(
            rebuilt
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(ApiContentBlock::as_text)
                .collect::<Vec<_>>(),
            vec!["durable root", "pending parent two", "disk child"]
        );
    }

    #[test]
    fn final_pending_row_identical_to_final_disk_row_is_not_retained() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-identical-final";
        let session_id = "raw-overlay-identical-final";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        let parent = raw_text_entry("assistant", "P", Some("D"), "durable parent");
        let child = raw_text_entry("user", "C", Some("P"), "durable child");
        std::fs::write(
            &path,
            format!("{}\n{}\n{}", durable.raw, parent.raw, child.raw),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(session_id, vec![parent]);

        ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("an exactly durable final pending row is accepted");
        assert!(state
            .get_session(session_id)
            .expect("live session")
            .loaded_transcript
            .is_empty());
        let (_, parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("no recovery row is needed after store recreation");
        assert_eq!(parent.as_deref(), Some("C"));
    }

    #[test]
    fn pending_value_is_retained_against_the_final_disk_uuid_occurrence() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-final-disk";
        let session_id = "raw-overlay-final-disk";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable root");
        let pending_value = raw_text_entry("assistant", "P", Some("D"), "pending value");
        let final_disk_value = raw_text_entry("assistant", "P", Some("D"), "final disk value");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}",
                durable.raw, pending_value.raw, final_disk_value.raw
            ),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load duplicate disk transcript");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(session_id, vec![pending_value]);

        ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("pending override succeeds");
        assert_eq!(
            state
                .get_session(session_id)
                .expect("live session")
                .loaded_transcript
                .len(),
            1,
            "matching an obsolete disk occurrence must not drop the live override"
        );
        let rebuilt = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("fresh store sees retained final pending value")
            .0;
        assert_eq!(rebuilt[1].content[0].as_text(), Some("pending value"));
    }

    #[test]
    fn rebuild_revision_conflict_retries_the_whole_acquisition() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-rebuild-conflict";
        let session_id = "raw-overlay-rebuild-conflict";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable");
        std::fs::write(&path, durable.raw.to_string()).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        state.push_transcript_entries(
            session_id,
            vec![raw_text_entry("assistant", "P", Some("D"), "first pending")],
        );

        let mut attempts = Vec::new();
        let (history, next_parent, _) = ReplayWindowStore::default()
            .history_for_after_each_take(&state, root.path(), session_id, |attempt| {
                attempts.push(attempt);
                if attempt == 0 {
                    state.push_transcript_entries(
                        session_id,
                        vec![raw_text_entry("user", "Q", Some("P"), "raced pending")],
                    );
                }
            })
            .expect("a rebuild conflict restarts acquisition from a new handoff");
        assert_eq!(attempts, vec![0, 1]);
        assert_eq!(next_parent.as_deref(), Some("Q"));
        let texts = history
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(ApiContentBlock::as_text)
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["durable", "first pending", "raced pending"]);
    }

    #[test]
    fn cache_conflict_then_rebuild_conflict_share_one_retry_budget() {
        let cwd = "/replay/cache-then-rebuild-conflict";
        let session_id = "cache-then-rebuild-conflict";
        let durable = raw_text_entry("user", "D", None, "durable");
        let (root, state, store) = populated_disk_cache(cwd, session_id, &[durable]);

        let mut attempts = Vec::new();
        let (history, next_parent, _) = store
            .history_for_after_each_take(&state, root.path(), session_id, |attempt| {
                attempts.push(attempt);
                match attempt {
                    0 => state.push_transcript_entries(
                        session_id,
                        vec![raw_text_entry("assistant", "A", Some("D"), "cache race")],
                    ),
                    1 => state.push_transcript_entries(
                        session_id,
                        vec![raw_text_entry("user", "B", Some("A"), "rebuild race")],
                    ),
                    _ => true,
                };
            })
            .expect("cache and rebuild conflicts are both retried canonically");
        assert_eq!(attempts, vec![0, 1, 2]);
        assert_eq!(next_parent.as_deref(), Some("B"));
        assert_eq!(
            history
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(ApiContentBlock::as_text)
                .collect::<Vec<_>>(),
            vec!["durable", "cache race", "rebuild race"]
        );
        assert_eq!(
            state
                .get_session(session_id)
                .expect("live session")
                .loaded_transcript
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        let (rebuilt, recreated_parent, _) = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect("both raced rows survive store recreation");
        assert_eq!(recreated_parent.as_deref(), Some("B"));
        assert_eq!(
            rebuilt
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(ApiContentBlock::as_text)
                .collect::<Vec<_>>(),
            vec!["durable", "cache race", "rebuild race"]
        );
    }

    #[test]
    fn repeated_rebuild_revision_conflicts_exhaust_the_retry_bound() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-rebuild-unstable";
        let session_id = "raw-overlay-rebuild-unstable";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable");
        std::fs::write(&path, durable.raw.to_string()).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        let mut parent = "D".to_string();
        let mut attempts = Vec::new();
        let err = ReplayWindowStore::default()
            .history_for_after_each_take(&state, root.path(), session_id, |attempt| {
                attempts.push(attempt);
                let uuid = format!("R-{attempt}");
                state.push_transcript_entries(
                    session_id,
                    vec![raw_text_entry(
                        if attempt % 2 == 0 {
                            "assistant"
                        } else {
                            "user"
                        },
                        &uuid,
                        Some(&parent),
                        &format!("raced-{attempt}"),
                    )],
                );
                parent = uuid;
            })
            .expect_err("each rebuild conflicts until the bounded retry budget is exhausted");
        assert_eq!(attempts, vec![0, 1, 2, 3]);
        assert!(err
            .to_string()
            .contains("remained unstable after 3 retries"));
        let restored = state
            .take_transcript_for_replay(session_id)
            .expect("the final conflict releases and restores the handoff");
        assert_eq!(
            restored
                .entries
                .iter()
                .map(|entry| entry.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["R-0", "R-1", "R-2", "R-3"]
        );
        state
            .restore_transcript_after_failed_replay(restored)
            .expect("inspection releases the lease without changing retained rows");
    }

    #[test]
    fn newer_disconnected_pending_leaf_cannot_replace_disk_chain() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-disconnected";
        let session_id = "raw-overlay-disconnected";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut disk = raw_text_entry("user", "D", None, "durable root");
        disk.timestamp = Some("2026-01-01T00:00:00Z".into());
        disk.raw["timestamp"] = serde_json::json!(disk.timestamp);
        std::fs::write(&path, disk.raw.to_string()).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        let mut pending = raw_text_entry("assistant", "X", Some("missing"), "detached");
        pending.timestamp = Some("2026-01-02T00:00:00Z".into());
        pending.raw["timestamp"] = serde_json::json!(pending.timestamp);
        state.push_transcript_entries(session_id, vec![pending]);

        let err = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect_err("a newer detached leaf must fail closed");
        assert!(err.to_string().contains("not continuous"));
        let restored = state
            .take_transcript_for_replay(session_id)
            .expect("failed overlay restores pending recovery");
        assert_eq!(restored.entries[0].uuid, "X");
    }

    #[test]
    fn same_uuid_pending_tail_cannot_disconnect_canonical_disk_ancestry() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-tail-rewrite";
        let session_id = "raw-overlay-tail-rewrite";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let disk = [
            raw_text_entry("user", "D", None, "durable root"),
            raw_text_entry("assistant", "P", Some("D"), "durable parent"),
            raw_text_entry("assistant", "C", Some("P"), "durable tail"),
        ];
        std::fs::write(
            &path,
            disk.iter()
                .map(|entry| entry.raw.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load canonical disk transcript");
        assert!(state.release_transcript_residency(session_id));
        let rewrite = raw_text_entry("assistant", "C", Some("missing"), "detached rewrite");
        state.push_transcript_entries(session_id, vec![rewrite.clone()]);

        let err = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect_err("same-UUID tail rewrite must not discard disk ancestry");
        assert!(err.to_string().contains("not continuous"));
        let restored = state
            .take_transcript_for_replay(session_id)
            .expect("failed overlay restores the pending tail rewrite");
        assert_eq!(restored.entries.len(), 1);
        assert!(same_raw_entry(&restored.entries[0], &rewrite));
    }

    #[test]
    fn same_uuid_pending_middle_cannot_disconnect_canonical_disk_ancestry() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-middle-rewrite";
        let session_id = "raw-overlay-middle-rewrite";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let disk = [
            raw_text_entry("user", "D", None, "durable root"),
            raw_text_entry("assistant", "P", Some("D"), "durable parent"),
            raw_text_entry("assistant", "C", Some("P"), "durable tail"),
        ];
        std::fs::write(
            &path,
            disk.iter()
                .map(|entry| entry.raw.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load canonical disk transcript");
        assert!(state.release_transcript_residency(session_id));
        let rewrite = raw_text_entry("assistant", "P", Some("missing"), "detached rewrite");
        state.push_transcript_entries(session_id, vec![rewrite.clone()]);

        let err = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect_err("same-UUID middle rewrite must not discard disk ancestry");
        assert!(err.to_string().contains("not continuous"));
        let restored = state
            .take_transcript_for_replay(session_id)
            .expect("failed overlay restores the pending middle rewrite");
        assert_eq!(restored.entries.len(), 1);
        assert!(same_raw_entry(&restored.entries[0], &rewrite));
    }

    #[test]
    fn newer_competing_pending_branch_cannot_replace_disk_tail() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/raw-overlay-competing";
        let session_id = "raw-overlay-competing";
        let path = rebon_session::transcript_file_path(root.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut root_entry = raw_text_entry("user", "D", None, "durable root");
        root_entry.timestamp = Some("2026-01-01T00:00:00Z".into());
        root_entry.raw["timestamp"] = serde_json::json!(root_entry.timestamp);
        let mut disk_tail = raw_text_entry("assistant", "C", Some("D"), "disk tail");
        disk_tail.timestamp = Some("2026-01-01T00:00:01Z".into());
        disk_tail.raw["timestamp"] = serde_json::json!(disk_tail.timestamp);
        std::fs::write(&path, format!("{}\n{}", root_entry.raw, disk_tail.raw)).unwrap();

        let state = ServerState::new();
        state
            .load_session(root.path(), session_id, cwd, None, Vec::new())
            .expect("load disk transcript");
        assert!(state.release_transcript_residency(session_id));
        let mut pending = raw_text_entry("assistant", "X", Some("D"), "competing tail");
        pending.timestamp = Some("2026-01-02T00:00:00Z".into());
        pending.raw["timestamp"] = serde_json::json!(pending.timestamp);
        state.push_transcript_entries(session_id, vec![pending]);

        let err = ReplayWindowStore::default()
            .history_for(&state, root.path(), session_id)
            .expect_err("a newer competing branch must not supersede the durable tail");
        assert!(err.to_string().contains("not continuous"));
        let restored = state
            .take_transcript_for_replay(session_id)
            .expect("failed competing overlay restores recovery");
        assert_eq!(restored.entries[0].uuid, "X");
    }

    #[test]
    fn active_handoff_refuses_replacement_and_recovery_survives_store_recreation() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        let cwd = "/replacement-race";
        let state = std::sync::Arc::new(ServerState::new());
        let session = state.create_session(cwd.into(), Vec::new());
        let path = rebon_session::transcript_file_path(&root_path, cwd, &session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "D", None, "durable");
        std::fs::write(&path, durable.raw.to_string()).unwrap();
        assert!(state.replace_transcript_entries(&session.id, vec![durable]));
        let store = std::sync::Arc::new(ReplayWindowStore::default());
        store
            .history_for(&state, &root_path, &session.id)
            .expect("initial source is released");
        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry(
                "assistant",
                "P",
                Some("D"),
                "undurable recovery",
            )],
        );

        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let worker_state = std::sync::Arc::clone(&state);
        let worker_store = std::sync::Arc::clone(&store);
        let worker_root = root_path.clone();
        let worker_session_id = session.id.clone();
        let worker = std::thread::spawn(move || {
            worker_store.history_for_after_take(
                &worker_state,
                &worker_root,
                &worker_session_id,
                || {
                    taken_tx.send(()).expect("announce active handoff");
                    resume_rx.recv().expect("resume replay build");
                },
            )
        });

        taken_rx.recv().expect("replay owns the undurable row");
        assert!(!state.replace_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "R", None, "replacement")]
        ));
        resume_tx.send(()).expect("allow replay finalization");
        worker
            .join()
            .expect("replay worker joins")
            .expect("replay finalizes without losing recovery");

        drop(store);
        let rebuilt = ReplayWindowStore::default()
            .history_for(&state, &root_path, &session.id)
            .expect("fresh store sees retained recovery")
            .0;
        assert!(rebuilt.iter().any(|message| message
            .content
            .iter()
            .any(|block| block.as_text() == Some("undurable recovery"))));
        assert!(state.replace_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "R", None, "replacement")]
        ));
    }

    #[test]
    fn malformed_disk_with_no_canonical_chain_restores_pending_rows() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/malformed-no-chain";
        let state = ServerState::new();
        let session = state.create_session(cwd.into(), Vec::new());
        let path = rebon_session::transcript_file_path(root.path(), cwd, &session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry("user", "initial", None, "initial")],
        );
        let store = ReplayWindowStore::default();
        store
            .history_for(&state, root.path(), &session.id)
            .expect("initial source");
        let pending = rebon_session::TranscriptEntry {
            entry_type: "system".into(),
            uuid: "pending-system".into(),
            parent_uuid: None,
            timestamp: None,
            raw: serde_json::json!({"type": "system", "uuid": "pending-system"}),
        };
        state.push_transcript_entries(&session.id, vec![pending]);
        std::fs::write(
            &path,
            "not-json\n{\"type\":\"system\",\"uuid\":\"disk-system\"}",
        )
        .unwrap();

        let err = store
            .history_for(&state, root.path(), &session.id)
            .expect_err("overlay without a user/assistant chain fails closed");
        assert!(err
            .to_string()
            .contains("malformed or unsupported JSONL records"));
        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("pending row restored after failure");
        assert_eq!(restored.entries[0].uuid, "pending-system");
    }

    #[test]
    fn explicit_load_during_handoff_preserves_undurable_recovery_across_store_recreation() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        let cwd = "/overlay-load-race";
        let state = std::sync::Arc::new(ServerState::new());
        let session = state.create_session(cwd.into(), Vec::new());
        let path = rebon_session::transcript_file_path(&root_path, cwd, &session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "u-1", None, "durable");
        std::fs::write(&path, durable.raw.to_string()).unwrap();
        assert!(state.replace_transcript_entries(&session.id, vec![durable]));

        let store = std::sync::Arc::new(ReplayWindowStore::default());
        store
            .history_for(&state, &root_path, &session.id)
            .expect("initial complete handoff");
        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry(
                "assistant",
                "a-undurable",
                Some("u-1"),
                "survives raced load",
            )],
        );

        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let worker_state = std::sync::Arc::clone(&state);
        let worker_store = std::sync::Arc::clone(&store);
        let worker_root = root_path.clone();
        let worker_session_id = session.id.clone();
        let worker = std::thread::spawn(move || {
            worker_store.history_for_after_take(
                &worker_state,
                &worker_root,
                &worker_session_id,
                || {
                    taken_tx.send(()).expect("announce active handoff");
                    resume_rx.recv().expect("resume replay build");
                },
            )
        });

        taken_rx.recv().expect("replay took undurable suffix");
        let materialized = state
            .load_session(&root_path, &session.id, cwd, None, Vec::new())
            .expect("explicit load materializes disk while handoff remains active");
        assert_eq!(materialized.loaded_transcript.len(), 1);
        assert_eq!(materialized.loaded_transcript[0].uuid, "u-1");
        assert!(
            state
                .get_session(&session.id)
                .expect("live leased session")
                .loaded_transcript
                .is_empty(),
            "load must not commit over the active handoff"
        );
        resume_tx.send(()).expect("release replay build");
        let raced_history = worker
            .join()
            .expect("replay worker joins")
            .expect("raced replay succeeds")
            .0;
        assert!(raced_history.iter().any(|message| message
            .content
            .iter()
            .any(|block| block.as_text() == Some("survives raced load"))));

        drop(store);
        let rebuilt = ReplayWindowStore::default()
            .history_for(&state, &root_path, &session.id)
            .expect("fresh store rebuild retains the undurable recovery row")
            .0;
        assert!(rebuilt.iter().any(|message| message
            .content
            .iter()
            .any(|block| block.as_text() == Some("survives raced load"))));
    }

    #[test]
    fn malformed_pending_overlay_is_restored_instead_of_collapsing_history() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/malformed-overlay";
        let state = ServerState::new();
        let session = state.create_session(cwd.into(), Vec::new());
        let path = rebon_session::transcript_file_path(root.path(), cwd, &session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let durable = raw_text_entry("user", "u-1", None, "durable");
        std::fs::write(&path, durable.raw.to_string()).unwrap();
        assert!(state.replace_transcript_entries(&session.id, vec![durable]));
        let store = ReplayWindowStore::default();
        store
            .history_for(&state, root.path(), &session.id)
            .expect("initial window");
        state.push_transcript_entries(
            &session.id,
            vec![raw_text_entry(
                "assistant",
                "bad",
                Some("missing-parent"),
                "must not disappear",
            )],
        );

        let err = store
            .history_for(&state, root.path(), &session.id)
            .expect_err("noncanonical pending branch must fail closed");
        assert!(err.to_string().contains("pending overlay"));
        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("moved pending row restored");
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].uuid, "bad");
    }

    #[test]
    fn replay_handoff_is_restored_when_history_callback_panics() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();
        let session = state.create_session("/panic-replay".into(), Vec::new());
        let mut moved = raw_text_entry("assistant", "same", None, "moved");
        moved.raw["marker"] = serde_json::json!("moved");
        state.push_transcript_entries(&session.id, vec![moved]);
        let store = ReplayWindowStore::default();

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.history_for_after_take(&state, root.path(), &session.id, || {
                let mut concurrent = raw_text_entry("assistant", "same", None, "concurrent");
                concurrent.raw["marker"] = serde_json::json!("concurrent");
                state.push_transcript_entries(&session.id, vec![concurrent]);
                panic!("after replay take");
            });
        }));
        assert!(unwind.is_err());

        let restored = state
            .take_transcript_for_replay(&session.id)
            .expect("unwind restored and released replay handoff");
        assert!(restored.complete);
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].uuid, "same");
        assert_eq!(restored.entries[0].raw["marker"], "concurrent");
    }

    #[test]
    fn replay_window_insert_terminates_with_corrupt_empty_lru() {
        let mut windows = ReplayWindows::default();
        for index in 0..=MAX_REPLAY_WINDOWS {
            let mut source = source(index as u64);
            source.session_id = format!("corrupt-{index}");
            windows.windows.insert(
                source.session_id.clone(),
                ReplayWindow::from_normalized(&source, vec![text_message(Role::User, "old")]),
            );
        }
        assert!(windows.lru.is_empty());

        let mut inserted = source(u64::MAX);
        inserted.session_id = "new-entry".into();
        windows.insert(
            inserted.session_id.clone(),
            ReplayWindow::from_normalized(&inserted, vec![text_message(Role::User, "new")]),
        );

        assert_eq!(windows.windows.len(), MAX_REPLAY_WINDOWS + 1);
        assert!(!windows.windows.contains_key("new-entry"));
        assert!(windows.windows.contains_key("corrupt-0"));
        assert!(windows.lru.is_empty());
    }

    #[test]
    fn replay_window_store_is_lru_bounded() {
        let mut windows = ReplayWindows::default();
        for index in 0..(MAX_REPLAY_WINDOWS + 5) {
            let mut source = source(index as u64);
            source.session_id = format!("session-{index}");
            windows.insert(
                source.session_id.clone(),
                ReplayWindow::from_normalized(
                    &source,
                    vec![text_message(Role::User, "x".repeat(64 * 1024))],
                ),
            );
        }
        assert_eq!(windows.windows.len(), MAX_REPLAY_WINDOWS);
        assert!(!windows.windows.contains_key("session-0"));
        assert!(windows
            .windows
            .contains_key(&format!("session-{}", MAX_REPLAY_WINDOWS + 4)));
    }

    #[test]
    fn raw_pipeline_is_provider_wire_equivalent_after_replay_budget_and_prompt_insertion() {
        let root = tempfile::tempdir().unwrap();
        let state = ServerState::new();
        let session = state.create_session("/wire".into(), Vec::new());
        let mut raw = Vec::new();
        let mut parent: Option<String> = None;
        for turn in 0..18 {
            let user_id = format!("u-{turn}");
            raw.push(raw_text_entry(
                "user",
                &user_id,
                parent.as_deref(),
                &format!("user-{turn}-{}", "x".repeat(80)),
            ));
            let assistant_id = format!("a-{turn}");
            raw.push(raw_text_entry(
                "assistant",
                &assistant_id,
                Some(&user_id),
                &format!("assistant-{turn}"),
            ));
            parent = Some(assistant_id);
        }
        // A transcript-only row and an orphan tool use exercise filtering and
        // global pairing repair before the message-count boundary is chosen.
        raw.push(rebon_session::TranscriptEntry {
            entry_type: "attachment".into(),
            uuid: "attachment-tail".into(),
            parent_uuid: parent.clone(),
            timestamp: None,
            raw: serde_json::json!({"type": "attachment", "text": "presentation only"}),
        });
        raw.push(rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "orphan-use".into(),
            parent_uuid: Some("attachment-tail".into()),
            timestamp: None,
            raw: serde_json::json!({
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "missing-result", "name": "Read", "input": {}}
                ]}
            }),
        });

        let legacy_full = transcript_to_api_messages(&raw);
        let legacy_title = rebon_api::extract_conversation_text(&legacy_full);
        let legacy_phase_one = rebon_api::auto_compact_truncate_cache_stable(legacy_full, 10);
        let mut expected_messages = truncate_replay_window_for_budget(
            &session.id,
            Some("system"),
            legacy_phase_one,
            Some(1_200),
        );
        expected_messages.push(text_message(Role::User, "current prompt"));

        assert!(state.replace_transcript_entries(&session.id, raw));
        let (actual_phase_one, _, actual_title) = ReplayWindowStore::default()
            .history_for(&state, root.path(), &session.id)
            .expect("complete raw source builds a replay window");
        assert_eq!(actual_title.as_deref(), Some(legacy_title.as_str()));
        let mut actual_messages = truncate_replay_window_for_budget(
            &session.id,
            Some("system"),
            actual_phase_one,
            Some(1_200),
        );
        actual_messages.push(text_message(Role::User, "current prompt"));
        assert_eq!(actual_messages, expected_messages);

        let mut expected = rebon_api::CreateMessageRequest::simple("wire-model", "placeholder");
        expected.system = Some("system".into());
        expected.messages = expected_messages;
        let mut actual = expected.clone();
        actual.messages = actual_messages;

        assert_eq!(
            rebon_api::anthropic::build_request_body(&actual),
            rebon_api::anthropic::build_request_body(&expected)
        );
        let config = rebon_api::OpenAiCompatibleClientConfig::default();
        assert_eq!(
            rebon_api::build_openai_request_body(&actual, &config),
            rebon_api::build_openai_request_body(&expected, &config)
        );
        assert_eq!(
            rebon_api::build_responses_request_body(
                &actual,
                false,
                "session-cache",
                Some("resp-1")
            ),
            rebon_api::build_responses_request_body(
                &expected,
                false,
                "session-cache",
                Some("resp-1")
            )
        );
    }
}
