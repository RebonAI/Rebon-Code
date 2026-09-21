//! What sessions there are to resume, and what each one says about itself.
//!
//! Walking the projects directory, reading each transcript's sidecar, asking
//! whether a session is already open and whether a background worker is
//! holding it: all of that answers "what could I resume", which is a question
//! about the directory rather than about a screen. The picker that draws the
//! answer, and the keys that move through it, stay in the terminal.
//!
//! The three entry points a caller uses -- [`discover_entries`] for the whole
//! list, [`stream_entries`] to take it in batches as it is found, and
//! [`discover_exact_entry`] for one named id -- were already free functions
//! written inside the dialog's `impl`. They take a path, a cwd, a server
//! state and, for the streaming one, a callback; none of them took `self`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::UNIX_EPOCH;

use rebon_width::truncate_to_width;

use crate::resume_resolution::{resolve_resume_transcript_cwd, ResumeTranscriptResolution};

/// One-shot filesystem index shared by an entire `/resume` discovery pass.
///
/// Resolving a session's transcript cwd needs two global scans: which
/// project directories hold `<session-id>.jsonl`, and which background
/// jobs park a transcript in a worktree. Running both *per candidate*
/// made opening the picker quadratic — every session re-walked
/// `projects/` and re-read every background job state file — which is
/// what froze the dialog for seconds on a machine with a long history.
/// Build them once and hand the same index to every candidate.
pub(crate) struct ResumeDiscoveryIndex {
    /// Session id → cwds whose project directory holds that transcript.
    transcript_cwds: HashMap<String, Vec<String>>,
    jobs: Vec<rebon_session_host::BackgroundJobState>,
}

impl ResumeDiscoveryIndex {
    pub fn build(projects_root: &Path) -> Self {
        let mut transcript_cwds: HashMap<String, Vec<String>> = HashMap::new();
        if let Ok(projects) = std::fs::read_dir(projects_root) {
            for project in projects.flatten() {
                if !project
                    .file_type()
                    .map(|kind| kind.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let project_dir = project.path();
                // Same contract as `session_transcript_cwds`: a project
                // directory without a cwd sidecar cannot name a cwd, so it
                // never contributes a match.
                let Some(project_cwd) =
                    rebon_session::session_storage::read_project_cwd_sidecar(&project_dir)
                else {
                    continue;
                };
                let Ok(files) = std::fs::read_dir(&project_dir) else {
                    continue;
                };
                for file in files.flatten() {
                    if !file.file_type().map(|kind| kind.is_file()).unwrap_or(false) {
                        continue;
                    }
                    let path = file.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                        continue;
                    };
                    transcript_cwds
                        .entry(session_id.to_string())
                        .or_default()
                        .push(project_cwd.clone());
                }
            }
        }

        Self {
            transcript_cwds,
            jobs: list_jobs_under(projects_root),
        }
    }

    fn transcript_cwds_for(&self, session_id: &str) -> &[String] {
        self.transcript_cwds
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

fn list_jobs_under(projects_root: &Path) -> Vec<rebon_session_host::BackgroundJobState> {
    projects_root
        .parent()
        .map(|rebon_root| rebon_session_host::BackgroundStore::new(rebon_root.to_path_buf()))
        .and_then(|store| store.list_jobs().ok())
        .unwrap_or_default()
}

/// The picker's test for "active, and joinable anyway": a job names this
/// session and its worker is up.
///
/// Under RFC-0004 a session whose write lock is held is normally held by
/// one of these, and joining it is exactly what the picker is for. The
/// filters are the ones [`home_job_for_session`] picks the attach
/// target with, plus the agent view's liveness test (§8.1) — so a row
/// marked joinable is a row `attach_instead_of_resume` can take. A lock
/// held by anything else (a `--local` instance) has no way in, and those
/// rows stay hidden as they always were.
///
/// [`home_job_for_session`]: rebon_session_host::home_job_for_session
fn hosted_by_live_worker(
    jobs: &[rebon_session_host::BackgroundJobState],
    session_id: &str,
) -> bool {
    jobs.iter().any(|job| {
        job.identity.session_id.as_deref() == Some(session_id)
            && !job.process.removal_reserved
            && job.identity.respawned_job_id.is_none()
            && job.process.pid.is_some()
            && job.process.status != rebon_session_host::BackgroundJobStatus::Stopped
    })
}

/// How many hydrated rows the discovery worker ships per batch. One batch
/// is roughly a screenful of the picker, so the first one covers the page
/// the user is actually looking at.
pub(crate) const RESUME_LOAD_BATCH: usize = 24;

/// A session that *may* become a picker row — cheap metadata only. The
/// filesystem work that decides whether it survives (and what it is
/// called) happens in [`hydrate_entry`].
struct ResumeCandidate {
    session_id: String,
    /// Where to look when the transcript cannot be located anywhere else.
    fallback_cwd: String,
    cached_title: Option<String>,
    /// `None` for background-job rows, whose date comes from the
    /// transcript file's mtime during hydration.
    created_at_ms: Option<u64>,
    sort_key_ms: u64,
    /// Background-job rows are dropped unless their transcript resolves to
    /// exactly one cwd; session-record rows fall back to their own cwd.
    require_resolved_cwd: bool,
}

fn hydrate_entry(
    index: &ResumeDiscoveryIndex,
    projects_root: &Path,
    cwd: &str,
    candidate: ResumeCandidate,
) -> Option<SessionEntry> {
    let session_id = candidate.session_id;
    let transcript_cwd =
        match resolve_resume_transcript_cwd_indexed(index, projects_root, cwd, &session_id) {
            ResumeTranscriptResolution::Unique(transcript_cwd) => transcript_cwd,
            ResumeTranscriptResolution::Missing if !candidate.require_resolved_cwd => {
                candidate.fallback_cwd
            }
            ResumeTranscriptResolution::Missing | ResumeTranscriptResolution::Ambiguous => {
                return None
            }
        };
    // Somebody holds this session's write lock. Hosted being the default,
    // that somebody is normally a worker, and the
    // picker's job is to let the user join it; a lock held by anything a
    // client cannot attach to keeps the row hidden, as it always was.
    let joinable = rebon_session::is_session_active(projects_root, &transcript_cwd, &session_id);
    if joinable && !hosted_by_live_worker(&index.jobs, &session_id) {
        return None;
    }

    let metadata = rebon_session::transcript_file_path(projects_root, &transcript_cwd, &session_id)
        .metadata()
        .ok();
    let created_ms = candidate.created_at_ms.unwrap_or_else(|| {
        metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|age| age.as_millis() as u64)
            .unwrap_or(0)
    });
    // The listing's cached title only covers sidecars under `cwd`; a
    // transcript that actually lives in a worktree keeps its title there.
    let title = candidate
        .cached_title
        .filter(|title| !title.trim().is_empty())
        .or_else(|| rebon_session::load_session_title(projects_root, &transcript_cwd, &session_id))
        .unwrap_or_else(|| derive_title(projects_root, &transcript_cwd, &session_id));

    Some(SessionEntry {
        session_id,
        transcript_cwd,
        title,
        created_at_ms: created_ms,
        jsonl_bytes: metadata.map(|metadata| metadata.len()),
        joinable,
    })
}

fn resolve_resume_transcript_cwd_indexed(
    index: &ResumeDiscoveryIndex,
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> ResumeTranscriptResolution {
    crate::resume_resolution::resolve_resume_transcript_cwd_from(
        projects_root,
        cwd,
        session_id,
        index.transcript_cwds_for(session_id),
        Some(index.jobs.as_slice()),
    )
}

/// A single session entry shown in the resume picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub session_id: String,
    pub transcript_cwd: String,
    pub title: String,
    pub created_at_ms: u64,
    pub jsonl_bytes: Option<u64>,
    /// A live worker holds this session, and picking the row attaches to
    /// it instead of resuming it here. Rows without it are
    /// sessions nobody has open.
    pub joinable: bool,
}

pub fn discover_exact_entry(
    projects_root: &Path,
    cwd: &str,
    server_state: &rebon_acp::ServerState,
    session_id: &str,
) -> Result<Vec<SessionEntry>, String> {
    let transcript_cwd = match resolve_resume_transcript_cwd(projects_root, cwd, session_id) {
        ResumeTranscriptResolution::Missing => return Ok(Vec::new()),
        ResumeTranscriptResolution::Unique(transcript_cwd) => transcript_cwd,
        ResumeTranscriptResolution::Ambiguous => {
            return Err(format!(
                "Session {session_id} has multiple distinct transcripts."
            ));
        }
    };
    let joinable = rebon_session::is_session_active(projects_root, &transcript_cwd, session_id);
    if joinable && !hosted_by_live_worker(&list_jobs_under(projects_root), session_id) {
        return Ok(Vec::new());
    }
    let path = rebon_session::transcript_file_path(projects_root, &transcript_cwd, session_id);
    let metadata = path.metadata().ok();
    let record = server_state.get_session(session_id);
    let created_ms = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .or_else(|| record.as_ref().map(|record| record.created_at))
        .and_then(|created| created.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let title = record
        .and_then(|record| record.title)
        .filter(|title| !title.trim().is_empty())
        .or_else(|| rebon_session::load_session_title(projects_root, &transcript_cwd, session_id))
        .unwrap_or_else(|| derive_title(projects_root, &transcript_cwd, session_id));
    Ok(vec![SessionEntry {
        session_id: session_id.to_string(),
        transcript_cwd,
        title,
        created_at_ms: created_ms,
        jsonl_bytes: metadata.map(|metadata| metadata.len()),
        joinable,
    }])
}

/// Discover every resumable session for `cwd`, shipping hydrated rows
/// to `emit` in batches instead of as one final list.
///
/// Discovery is two-phase on purpose. Phase one collects the cheap
/// metadata (session ids and a sort key) for every candidate; phase
/// two pays the expensive per-row work — an active-lock probe, a stat
/// and, when no sidecar title exists, a transcript head read. Ordering
/// candidates newest-first between the phases makes the first batch
/// exactly the page the user is looking at, so the picker paints in
/// the time it takes to hydrate [`RESUME_LOAD_BATCH`] rows instead of
/// the whole history.
///
/// `emit` returns false once the consumer is gone (the dialog closed),
/// which abandons the rest of the scan.
pub fn stream_entries(
    projects_root: &Path,
    cwd: &str,
    server_state: &rebon_acp::ServerState,
    current_session_id: &str,
    mut emit: impl FnMut(Vec<SessionEntry>) -> bool,
) -> Result<(), String> {
    let sessions = server_state
        .list_sessions(projects_root, Some(cwd), Some(cwd))
        .map_err(|err| format!("Failed to list sessions: {err:?}"))?;
    let index = ResumeDiscoveryIndex::build(projects_root);

    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(current_session_id.to_string());
    let mut candidates: Vec<ResumeCandidate> = Vec::with_capacity(sessions.len());
    for session in sessions {
        if !seen.insert(session.id.clone()) {
            continue;
        }
        let created_at_ms = session
            .created_at
            .duration_since(UNIX_EPOCH)
            .map(|age| age.as_millis() as u64)
            .unwrap_or(0);
        candidates.push(ResumeCandidate {
            session_id: session.id,
            fallback_cwd: session.cwd,
            cached_title: session.title,
            created_at_ms: Some(created_at_ms),
            sort_key_ms: created_at_ms,
            require_resolved_cwd: false,
        });
    }
    for job in &index.jobs {
        if !rebon_session::same_cwd(&job.identity.cwd, cwd) {
            continue;
        }
        let Some(session_id) = job.identity.session_id.clone().filter(|id| !id.is_empty()) else {
            continue;
        };
        if !seen.insert(session_id.clone()) {
            continue;
        }
        candidates.push(ResumeCandidate {
            session_id,
            fallback_cwd: job.identity.cwd.clone(),
            cached_title: None,
            // Background rows date from the transcript file itself,
            // which only the hydration pass stats.
            created_at_ms: None,
            sort_key_ms: job.process.updated_at_ms,
            require_resolved_cwd: true,
        });
    }

    candidates.sort_by(|a, b| {
        b.sort_key_ms
            .cmp(&a.sort_key_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    let mut batch: Vec<SessionEntry> = Vec::with_capacity(RESUME_LOAD_BATCH);
    for candidate in candidates {
        if let Some(entry) = hydrate_entry(&index, projects_root, cwd, candidate) {
            batch.push(entry);
        }
        if batch.len() >= RESUME_LOAD_BATCH && !emit(std::mem::take(&mut batch)) {
            return Ok(());
        }
    }
    if !batch.is_empty() {
        emit(batch);
    }
    Ok(())
}

/// Collected form of [`stream_entries`], for callers that want
/// the whole list at once.
#[cfg_attr(not(test), allow(dead_code))]
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn discover_entries(
    projects_root: &Path,
    cwd: &str,
    server_state: &rebon_acp::ServerState,
    current_session_id: &str,
) -> Result<Vec<SessionEntry>, String> {
    let mut entries: Vec<SessionEntry> = Vec::new();
    stream_entries(
        projects_root,
        cwd,
        server_state,
        current_session_id,
        |batch| {
            entries.extend(batch);
            true
        },
    )?;
    entries.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(entries)
}

/// Layer-A display fallback: when a session has no cached AI-generated
/// title in its sidecar (`{session_id}.meta.json`), derive a title from
/// the first user message stored in the transcript file. Strips XML
/// cruft that IDE and hook integrations inject (the tag families
/// [`strip_display_tags`] knows), then truncates to a readable width.
/// Falls back to the first 8 characters of the session id when no usable
/// user text exists.
pub(crate) fn derive_title(projects_root: &Path, cwd: &str, session_id: &str) -> String {
    if let Some(raw) = rebon_session::session_storage::extract_first_user_message_text(
        projects_root,
        cwd,
        session_id,
    ) {
        let stripped = strip_display_tags(&raw);
        let trimmed = stripped.trim();
        if !trimmed.is_empty() {
            // ~60 chars keeps the title column readable on standard
            // terminal widths without hiding the session id next to it.
            return truncate_to_width(trimmed, 60);
        }
    }
    session_id.chars().take(8).collect()
}

/// Strips the XML wrapper tags, contents included, that rebon actually
/// sees in user messages today; the result may be empty:
///
/// * `<ide_opened_file>…</ide_opened_file>` — injected by IDE plugins
///   when the user opens a file next to the prompt.
/// * `<command-name>…</command-name>` + `<command-message>…` — slash
///   command metadata wrappers (e.g. `/clear`, `/model`).
/// * `<session-start-hook>…</session-start-hook>` — session-start hook
///   output injected as a pseudo-user message.
/// * `<system-reminder>…</system-reminder>` — harness-injected system
///   reminders that ship alongside user turns.
///
/// Any other tags pass through untouched, and standalone `<` / `>` in
/// user text survive. An opening tag with no matching close loses only
/// the tag itself.
pub(crate) fn strip_display_tags(input: &str) -> String {
    const TAGS: &[&str] = &[
        "ide_opened_file",
        "command-name",
        "command-message",
        "command-args",
        "session-start-hook",
        "system-reminder",
        "local-command-stdout",
        "local-command-caveat",
    ];
    let mut out = input.to_string();
    for tag in TAGS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        // Repeatedly strip matching pairs — a single user message can
        // carry several IDE tags (multiple opened files) or both a
        // command-name and a command-message in sequence.
        loop {
            let Some(start) = out.find(&open) else { break };
            let search_from = start + open.len();
            let Some(end_rel) = out[search_from..].find(&close) else {
                // Unbalanced: strip only the opening tag.
                out.replace_range(start..search_from, "");
                continue;
            };
            let end = search_from + end_rel + close.len();
            out.replace_range(start..end, "");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_display_tags ─────────────────────────────────────────────

    #[test]
    fn strip_display_tags_removes_ide_opened_file_wrapper() {
        let input = "<ide_opened_file>src/main.rs</ide_opened_file>fix the build";
        assert_eq!(strip_display_tags(input), "fix the build");
    }

    #[test]
    fn strip_display_tags_removes_command_metadata_entirely() {
        // Tag and contents are stripped for these internal wrappers — the resulting string can be empty
        // so the caller falls through to the next fallback layer.
        let input =
            "<command-name>/clear</command-name><command-message>clear history</command-message>";
        let got = strip_display_tags(input);
        assert!(
            got.trim().is_empty(),
            "command wrappers + content removed, got {got:?}"
        );
    }

    #[test]
    fn strip_display_tags_removes_system_reminder_wrappers() {
        let input = "<system-reminder>internal noise</system-reminder>real user question";
        assert_eq!(strip_display_tags(input), "real user question");
    }

    #[test]
    fn strip_display_tags_handles_multiple_occurrences() {
        // A single message carrying several IDE-opened files in
        // sequence — every wrapper (and its inlined file path) gets
        // stripped, leaving only the human-authored prompt.
        let input = "<ide_opened_file>a.rs</ide_opened_file><ide_opened_file>b.rs</ide_opened_file>actual prompt";
        let got = strip_display_tags(input);
        assert_eq!(got, "actual prompt");
    }

    #[test]
    fn strip_display_tags_passes_unknown_tags_through() {
        // Tags rebon doesn't know about are user content — leave
        // them alone so the title still looks reasonable.
        let input = "<weird-tag>preserve me</weird-tag>";
        let got = strip_display_tags(input);
        assert_eq!(got, "<weird-tag>preserve me</weird-tag>");
    }

    #[test]
    fn strip_display_tags_leaves_plain_text_untouched() {
        let input = "fix the login button on mobile";
        assert_eq!(strip_display_tags(input), input);
    }

    // ── derive_title ───────────────────────────────────────────────────

    /// Helper: point derive_title at a temp projects root, write a
    /// transcript with the given user text, and return the derived
    /// title.
    fn derive_with_transcript(user_text: &str, session_id: &str) -> String {
        use rebon_session::session_storage::{append_transcript_entry, TranscriptWriteEntry};
        use serde_json::json;

        let root = tempfile::Builder::new()
            .prefix("rebon-derive-title-")
            .tempdir()
            .unwrap();
        let cwd = "/tmp/derive-title-fixture";

        append_transcript_entry(
            root.path(),
            cwd,
            session_id,
            TranscriptWriteEntry::new(
                "user",
                json!({"message": {"role": "user", "content": user_text}}),
            )
            .with_uuid("u1")
            .with_timestamp("2026-04-09T00:00:00.000Z"),
        )
        .unwrap();

        derive_title(root.path(), cwd, session_id)
    }

    #[test]
    fn derive_title_uses_first_user_message_when_sidecar_absent() {
        let got = derive_with_transcript("fix the login bug", "sess-derive-1");
        assert_eq!(got, "fix the login bug");
    }

    #[test]
    fn derive_title_strips_display_tags_before_falling_back() {
        let raw = "<ide_opened_file>src/auth.rs</ide_opened_file>fix auth flow";
        let got = derive_with_transcript(raw, "sess-derive-2");
        assert_eq!(got, "fix auth flow");
    }

    #[test]
    fn derive_title_falls_back_to_session_id_prefix_when_no_transcript() {
        // No file written — `extract_first_user_message_text` returns
        // None, and we get the truncated session id.
        let root = tempfile::Builder::new()
            .prefix("rebon-derive-title-missing-")
            .tempdir()
            .unwrap();
        let got = derive_title(root.path(), "/tmp/nope", "sess-abcdef12-xyz");
        assert_eq!(got, "sess-abc");
    }

    #[test]
    fn derive_title_falls_back_to_session_id_when_user_text_is_only_tags() {
        // A message whose content is entirely display-tag wrappers
        // yields an empty string after stripping — must fall through
        // to the session-id fallback, not show a blank title.
        let raw = "<ide_opened_file>a.rs</ide_opened_file><command-name></command-name>";
        let got = derive_with_transcript(raw, "sess-fallback-12345");
        assert_eq!(got, "sess-fal");
    }

    #[test]
    fn derive_title_truncates_long_first_message() {
        let long = "a".repeat(500);
        let got = derive_with_transcript(&long, "sess-derive-long");
        // truncate_to_width(60) must not produce more than 60 columns.
        assert!(got.chars().count() <= 60);
    }

    #[test]
    fn open_filters_active_sessions() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/tmp/resume-active-filter";
        let server_state = rebon_acp::ServerState::new();
        let active = server_state.create_session(cwd.to_string(), Vec::new());
        let stopped = server_state.create_session(cwd.to_string(), Vec::new());
        let _active_lock =
            rebon_session::try_acquire_session_active_lock(root.path(), cwd, &active.id)
                .unwrap()
                .unwrap();

        let entries = discover_entries(root.path(), cwd, &server_state, "current").unwrap();
        let ids = entries
            .iter()
            .map(|entry| entry.session_id.as_str())
            .collect::<Vec<_>>();

        assert!(!ids.contains(&active.id.as_str()));
        assert!(ids.contains(&stopped.id.as_str()));
    }

    fn test_runtime_fields() -> rebon_session_host::BackgroundRuntimeFields {
        rebon_session_host::BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }

    /// The same lock without a worker behind it — a `--local` instance —
    /// stays hidden: nothing can attach to it.
    #[test]
    fn open_still_hides_an_active_session_no_worker_holds() {
        let root = tempfile::tempdir().unwrap();
        let projects_root = root.path().join("projects");
        let cwd = "/repo/local-only";
        let session_id = "sess-local-held";
        let transcript =
            rebon_session::ensure_session_file_path(&projects_root, cwd, session_id).unwrap();
        std::fs::write(transcript, b"{}\n").unwrap();

        let store = rebon_session_host::BackgroundStore::new(root.path());
        let mut job = store
            .create_job("stopped prompt".into(), cwd.into(), test_runtime_fields())
            .unwrap();
        job.identity.session_id = Some(session_id.into());
        job.process.status = rebon_session_host::BackgroundJobStatus::Stopped;
        job.process.pid = Some(std::process::id());
        store.write_state(&job).unwrap();

        let _local_lock =
            rebon_session::try_acquire_session_active_lock(&projects_root, cwd, session_id)
                .unwrap()
                .unwrap();

        let entries = discover_entries(
            &projects_root,
            cwd,
            &rebon_acp::ServerState::new(),
            "current",
        )
        .unwrap();
        assert!(entries.iter().all(|entry| entry.session_id != session_id));
    }

    #[test]
    fn open_includes_worktree_session_owned_by_current_project() {
        let root = tempfile::tempdir().unwrap();
        let projects_root = root.path().join("projects");
        let cwd = "/repo/project";
        let worktree_cwd = "/repo/project/.rebon/worktrees/bg-1";
        let session_id = "sess-worktree-resume";
        let transcript =
            rebon_session::ensure_session_file_path(&projects_root, worktree_cwd, session_id)
                .unwrap();
        std::fs::write(transcript, b"{}\n").unwrap();
        rebon_session::save_session_title(
            &projects_root,
            worktree_cwd,
            session_id,
            "Worktree conversation",
        )
        .unwrap();

        let store = rebon_session_host::BackgroundStore::new(root.path());
        let runtime = rebon_session_host::BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        };
        let mut job = store
            .create_job("historical prompt".into(), cwd.into(), runtime)
            .unwrap();
        job.identity.session_id = Some(session_id.into());
        job.process.status = rebon_session_host::BackgroundJobStatus::Succeeded;
        store.write_state(&job).unwrap();

        let entries = discover_entries(
            &projects_root,
            cwd,
            &rebon_acp::ServerState::new(),
            "current",
        )
        .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.session_id == session_id)
            .unwrap();

        assert_eq!(entry.title, "Worktree conversation");
    }

    #[test]
    fn open_includes_legacy_worktree_session_without_cwd_sidecar() {
        let root = tempfile::tempdir().unwrap();
        let projects_root = root.path().join("projects");
        let cwd = "/repo/project";
        let worktree_cwd = "/repo/project/.rebon/worktrees/bg-legacy";
        let session_id = "sess-legacy-worktree-resume";
        let project_dir = rebon_session::project_dir_path(&projects_root, worktree_cwd);
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join(format!("{session_id}.jsonl")), b"{}\n").unwrap();

        let store = rebon_session_host::BackgroundStore::new(root.path());
        let runtime = rebon_session_host::BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        };
        let mut job = store
            .create_job("legacy historical prompt".into(), cwd.into(), runtime)
            .unwrap();
        job.identity.session_id = Some(session_id.into());
        job.workspace.worktree_path = Some(worktree_cwd.into());
        job.process.status = rebon_session_host::BackgroundJobStatus::Succeeded;
        store.write_state(&job).unwrap();

        let entries = discover_entries(
            &projects_root,
            cwd,
            &rebon_acp::ServerState::new(),
            "current",
        )
        .unwrap();

        assert!(entries.iter().any(|entry| entry.session_id == session_id));
        assert_eq!(
            resolve_resume_transcript_cwd(&projects_root, cwd, session_id),
            ResumeTranscriptResolution::Unique(worktree_cwd.into())
        );

        let _active_lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            worktree_cwd,
            session_id,
        )
        .unwrap()
        .unwrap();
        let active_entries = discover_entries(
            &projects_root,
            cwd,
            &rebon_acp::ServerState::new(),
            "current",
        )
        .unwrap();
        assert!(active_entries
            .iter()
            .all(|entry| entry.session_id != session_id));
    }

    #[test]
    fn resume_rejects_sidecar_and_legacy_job_transcript_ambiguity() {
        let root = tempfile::tempdir().unwrap();
        let projects_root = root.path().join("projects");
        let cwd = "/repo/project";
        let sidecar_cwd = cwd;
        let legacy_cwd = "/repo/project/.rebon/worktrees/legacy";
        let session_id = "sess-ambiguous-worktrees";

        let sidecar_transcript =
            rebon_session::ensure_session_file_path(&projects_root, sidecar_cwd, session_id)
                .unwrap();
        std::fs::write(sidecar_transcript, b"{}\n").unwrap();
        let legacy_dir = rebon_session::project_dir_path(&projects_root, legacy_cwd);
        std::fs::create_dir_all(&legacy_dir).unwrap();
        // The two transcripts must diverge. A sidecar that is a byte-prefix
        // superset of the worktree copy and not older supersedes it, and
        // two writes a few microseconds apart land on the same mtime tick
        // often enough that identical contents would collapse to `Unique`
        // and make this test depend on the clock.
        std::fs::write(
            legacy_dir.join(format!("{session_id}.jsonl")),
            b"{\"legacy\":true}\n",
        )
        .unwrap();

        let store = rebon_session_host::BackgroundStore::new(root.path());
        let runtime = rebon_session_host::BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        };
        let mut job = store
            .create_job("ambiguous historical prompt".into(), cwd.into(), runtime)
            .unwrap();
        job.identity.session_id = Some(session_id.into());
        job.workspace.worktree_path = Some(legacy_cwd.into());
        job.process.status = rebon_session_host::BackgroundJobStatus::Succeeded;
        store.write_state(&job).unwrap();

        assert_eq!(
            resolve_resume_transcript_cwd(&projects_root, cwd, session_id),
            ResumeTranscriptResolution::Ambiguous
        );
        let entries = discover_entries(
            &projects_root,
            cwd,
            &rebon_acp::ServerState::new(),
            "current",
        )
        .unwrap();
        assert!(entries.iter().all(|entry| entry.session_id != session_id));
    }
}
