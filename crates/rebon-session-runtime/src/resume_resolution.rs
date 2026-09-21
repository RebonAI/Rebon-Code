//! Which project directory holds the transcript a `--resume` names.
//!
//! A session id can be reachable from more than one place: the cwd's own
//! sidecar, a sidecar under another cwd, or a worktree a background job
//! left behind. This decides whether that is one transcript or several,
//! and it is asked both by the terminal's picker and by the session
//! builder when it opens a resumed session.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub enum ResumeTranscriptResolution {
    Missing,
    Unique(String),
    Ambiguous,
}

/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn resolve_resume_transcript_cwd(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> ResumeTranscriptResolution {
    resolve_resume_transcript_cwd_from(
        projects_root,
        cwd,
        session_id,
        &rebon_session::session_transcript_cwds(projects_root, session_id),
        None,
    )
}

/// `sidecar_cwds` are the project-sidecar cwds holding this transcript;
/// `jobs` is a pre-read background job listing, or `None` to read the
/// store here (the single-session callers, where one listing is cheap).
pub(crate) fn resolve_resume_transcript_cwd_from(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    sidecar_cwds: &[String],
    jobs: Option<&[rebon_session_host::BackgroundJobState]>,
) -> ResumeTranscriptResolution {
    let owned_jobs: Vec<rebon_session_host::BackgroundJobState>;
    let jobs = match jobs {
        Some(jobs) => jobs,
        None => {
            owned_jobs = projects_root
                .parent()
                .map(|rebon_root| {
                    rebon_session_host::BackgroundStore::new(rebon_root.to_path_buf())
                })
                .and_then(|store| store.list_jobs().ok())
                .unwrap_or_default();
            owned_jobs.as_slice()
        }
    };

    let mut matches: Vec<String> = Vec::new();
    let mut record_match = |candidate: String| {
        if !matches
            .iter()
            .any(|existing| rebon_session::same_cwd(existing, &candidate))
        {
            matches.push(candidate);
        }
    };

    if rebon_session::transcript_file_path(projects_root, cwd, session_id).is_file() {
        record_match(cwd.to_string());
    }
    for found in sidecar_cwds {
        record_match(found.clone());
    }

    for job in jobs {
        if !rebon_session::same_cwd(&job.identity.cwd, cwd)
            || job.identity.session_id.as_deref() != Some(session_id)
        {
            continue;
        }
        let Some(worktree_cwd) = job
            .workspace
            .worktree_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        else {
            continue;
        };
        if rebon_session::transcript_file_path(projects_root, worktree_cwd, session_id).is_file() {
            record_match(worktree_cwd.to_string());
        }
    }

    if matches.len() > 1
        && rebon_session::transcript_file_path(projects_root, cwd, session_id).is_file()
    {
        let target = rebon_session::transcript_file_path(projects_root, cwd, session_id);
        if let (Ok(target_bytes), Ok(target_modified)) = (
            std::fs::read(&target),
            target.metadata().and_then(|metadata| metadata.modified()),
        ) {
            let supersedes_all = matches.iter().all(|candidate| {
                if rebon_session::same_cwd(candidate, cwd) {
                    return true;
                }
                if rebon_session::is_session_active(projects_root, candidate, session_id) {
                    return false;
                }
                let source =
                    rebon_session::transcript_file_path(projects_root, candidate, session_id);
                let source_modified = source.metadata().and_then(|metadata| metadata.modified());
                match (std::fs::read(source), source_modified) {
                    (Ok(source_bytes), Ok(source_modified)) => {
                        target_modified >= source_modified
                            && target_bytes.starts_with(&source_bytes)
                    }
                    _ => false,
                }
            });
            if supersedes_all {
                return ResumeTranscriptResolution::Unique(cwd.to_string());
            }
        }
    }

    match matches.as_slice() {
        [] => ResumeTranscriptResolution::Missing,
        [found] => ResumeTranscriptResolution::Unique(found.clone()),
        _ => ResumeTranscriptResolution::Ambiguous,
    }
}
