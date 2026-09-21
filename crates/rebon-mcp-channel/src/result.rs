//! `result.md`: what a job's last turn said, as a file the client reads
//! instead of a tool result it would have to hold in context.
//!
//! A projection, not a record. The transcript is the authority; this file is
//! rewritten from it whenever a push or a `job_result` needs it, so a stale
//! one corrects itself and two servers writing it write the same bytes. It
//! lives in the job's directory, so `rebon rm` removes it with the job —
//! retention is the job's, and nobody else has to clean up after this surface.
//!
//! The path is always Rebon's: `job_result` takes a job id, never a path, so a
//! client cannot turn it into a read of an arbitrary file (§6.2, §8).

use std::path::{Path, PathBuf};

use anyhow::Context;
use rebon_session_host::{BackgroundJobState, BackgroundJobStatus, BackgroundStore};

pub(crate) const RESULT_FILE: &str = "result.md";

/// Shown when a turn ended without the model writing any text.
const NO_TEXT: &str = "(The job's last turn produced no text.)";

pub(crate) fn result_path(store: &BackgroundStore, job_id: &str) -> PathBuf {
    store.job_dir(job_id).join(RESULT_FILE)
}

/// The written file and what is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobResult {
    pub path: PathBuf,
    pub text: String,
}

/// Rebuild `result.md` for a job whose turn is over.
pub(crate) fn write_result(
    store: &BackgroundStore,
    projects_root: &Path,
    state: &BackgroundJobState,
) -> anyhow::Result<JobResult> {
    let said = match rebon_session_host::background_job_transcript_path_in(projects_root, state) {
        Some(transcript) => rebon_session::last_turn_assistant_text(&transcript)
            .with_context(|| format!("failed to read {}", transcript.display()))?,
        None => None,
    };
    let text = compose(said.as_deref(), state.status(), state.error());
    let path = result_path(store, state.job_id());
    rebon_session::write_file_atomically(&path, text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(JobResult { path, text })
}

/// The file's body: what the model said, then — when the turn did not simply
/// finish — one line saying how it ended, so the file alone tells the story.
fn compose(said: Option<&str>, status: BackgroundJobStatus, error: Option<&str>) -> String {
    let mut text = said.map(str::trim).unwrap_or(NO_TEXT).to_string();
    let ending = match status {
        BackgroundJobStatus::Failed => Some(match error.map(str::trim).filter(|e| !e.is_empty()) {
            Some(error) => format!("The job failed: {error}"),
            None => "The job failed.".to_string(),
        }),
        BackgroundJobStatus::Stopped => Some("The job was stopped before it finished.".into()),
        BackgroundJobStatus::Idle => Some("The turn was cancelled before it finished.".into()),
        BackgroundJobStatus::Succeeded
        | BackgroundJobStatus::Queued
        | BackgroundJobStatus::Running
        | BackgroundJobStatus::NeedsInput => None,
    };
    if let Some(ending) = ending {
        text.push_str("\n\n---\n");
        text.push_str(&ending);
    }
    text.push('\n');
    text
}

/// The end of `text`, at most `max_chars` characters of it, and whether
/// anything was cut. The end rather than the start: a turn's conclusion is
/// its last paragraph, and the file holds the rest.
pub(crate) fn preview(text: &str, max_chars: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= max_chars {
        return (text.to_string(), false);
    }
    let tail: String = text.chars().skip(total - max_chars).collect();
    (format!("…{tail}"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_turn_is_just_what_the_model_said() {
        assert_eq!(
            compose(Some("  All done.  "), BackgroundJobStatus::Succeeded, None),
            "All done.\n"
        );
        assert_eq!(
            compose(None, BackgroundJobStatus::Succeeded, None),
            format!("{NO_TEXT}\n")
        );
    }

    #[test]
    fn a_turn_that_did_not_finish_says_how_it_ended() {
        let failed = compose(
            Some("Partial work."),
            BackgroundJobStatus::Failed,
            Some("provider returned 529"),
        );
        assert_eq!(
            failed,
            "Partial work.\n\n---\nThe job failed: provider returned 529\n"
        );
        assert!(
            compose(None, BackgroundJobStatus::Failed, Some("  ")).ends_with("The job failed.\n")
        );
        assert!(compose(None, BackgroundJobStatus::Stopped, None).contains("was stopped"));
        assert!(compose(None, BackgroundJobStatus::Idle, None).contains("was cancelled"));
    }

    #[test]
    fn the_preview_keeps_the_end_and_says_it_was_cut() {
        assert_eq!(preview("short", 10), ("short".into(), false));
        assert_eq!(preview("abcdef", 6), ("abcdef".into(), false));
        let (cut, truncated) = preview("intro, then conclusion", 10);
        assert!(truncated);
        assert_eq!(cut, "…conclusion");
        let (cjk, _) = preview("一二三四五六", 2);
        assert_eq!(cjk, "…五六", "cut by characters, not bytes");
    }
}
