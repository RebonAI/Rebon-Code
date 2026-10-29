//! What a `/compact` run is, once the screen is taken away: the worker
//! handle that lives on the session, the report the run prints, and the
//! mailbox answer a deferred run owes its caller.
//!
//! The background worker runs `/compact` with no terminal and prints the
//! same report, so the formatting is here rather than in the TUI it used
//! to live in. What stayed behind in the binary's `tui::runner::compact_runtime`
//! is the half that needs a screen: starting a run from the submit path
//! and draining it once per frame into the transcript.

use std::sync::mpsc::Receiver;

use rebon_core::query::ManualCompactReport;

/// Gutter glyph the report's detail lines use, matching how tool output
/// continuation rows are drawn elsewhere in the transcript.
const REPORT_BULLET: &str = "⎿";

/// Per-session home for the compaction worker.
///
/// Lives on [`TuiEngineSession`] rather than the event loop's locals so
/// `/compact` can start a run from the submit path, which already has the
/// session in hand and would otherwise need the runtime threaded through
/// every caller between them.
#[derive(Default)]
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct CompactRuntime {
    pub worker: Option<CompactWorker>,
}

impl CompactRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&self) -> bool {
        self.worker.is_some()
    }
}

pub struct CompactWorker {
    pub session_id: String,
    pub rx: Receiver<CompactProgress>,
}

pub enum CompactProgress {
    /// The transcript is loaded; the provider call has begun.
    Summarizing,
    Done(Box<CompactDone>),
}

/// How a compaction ended. Whatever was installed, was installed before this
/// is reported, on the session loop, with no turn running.
pub enum CompactDone {
    /// The summary is the session's replay baseline now.
    Installed(ManualCompactReport),
    /// An idle compaction whose summariser fell back without the model;
    /// nothing was installed.
    NoSummary(ManualCompactReport),
    /// An idle compaction whose transcript moved on while it ran; dropped.
    Stale,
    /// An idle compaction that could not be checked against the transcript.
    Unverified(String),
    Failed(String),
}

/// Compact a session's history and install the result, reporting progress.
///
/// Meant to run as session-loop history work: nothing else writes the
/// history while it does, so the baseline it installs is in place before the
/// next turn starts. An idle (`automatic`) run installs only a summary the
/// model actually wrote, and only if the transcript is still the one it read.
pub async fn compact_and_install(
    resume_replay: rebon_core::query::ResumeReplayHandle,
    projects_root: std::path::PathBuf,
    cwd: String,
    session_id: String,
    instructions: Option<String>,
    automatic: bool,
    progress: std::sync::mpsc::Sender<CompactProgress>,
) {
    let done = compact_and_install_inner(
        &resume_replay,
        &projects_root,
        &cwd,
        &session_id,
        instructions.as_deref(),
        automatic,
        &progress,
    )
    .await;
    let _ = progress.send(CompactProgress::Done(Box::new(done)));
}

async fn compact_and_install_inner(
    resume_replay: &rebon_core::query::ResumeReplayHandle,
    projects_root: &std::path::Path,
    cwd: &str,
    session_id: &str,
    instructions: Option<&str>,
    automatic: bool,
    progress: &std::sync::mpsc::Sender<CompactProgress>,
) -> CompactDone {
    let loaded = match load_transcript(projects_root, cwd, session_id).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => {
            return CompactDone::Failed("This session has no transcript to compact yet.".into())
        }
        Err(err) => {
            return CompactDone::Failed(format!("Could not read this session's transcript: {err}"))
        }
    };
    let _ = progress.send(CompactProgress::Summarizing);
    let (prepared, report) = match resume_replay
        .compact_session_now(
            projects_root,
            cwd,
            session_id,
            &loaded.messages,
            instructions,
        )
        .await
    {
        Ok(result) => result,
        Err(message) => return CompactDone::Failed(message),
    };
    if automatic {
        if !report.used_model {
            return CompactDone::NoSummary(report);
        }
        match load_transcript(projects_root, cwd, session_id).await {
            Ok(Some(current)) if prepared.matches_transcript(&current.messages) => {}
            Ok(_) => {
                tracing::info!("idle compaction discarded because the transcript changed");
                return CompactDone::Stale;
            }
            Err(err) => return CompactDone::Unverified(err.to_string()),
        }
    }
    resume_replay.install_persistent_summary(projects_root, cwd, session_id.to_string(), prepared);
    CompactDone::Installed(report)
}

async fn load_transcript(
    projects_root: &std::path::Path,
    cwd: &str,
    session_id: &str,
) -> anyhow::Result<Option<rebon_session::LoadedTranscript>> {
    let path = rebon_session::transcript_file_path(projects_root, cwd, session_id);
    let loaded =
        tokio::task::spawn_blocking(move || rebon_session::load_transcript_from_file(&path))
            .await
            .map_err(|err| anyhow::anyhow!("transcript read stopped: {err}"))??;
    Ok(loaded)
}

/// Write the mailbox response a deferred foreground `/compact` owes its
/// caller. No-op for a locally-typed command.
///
/// Addressed by the session the *run* belongs to, not the session the TUI is
/// showing now: the controller polls the response sidecar under the session
/// id it sent the command to, so a `/resume` mid-run must not redirect the
/// answer to a directory nobody is watching.
pub fn answer_deferred_command(
    session: &crate::EngineSession,
    session_id: &str,
    command_id: Option<&str>,
    result: Result<String, String>,
) {
    let Some(command_id) = command_id else {
        return;
    };
    let (output, error) = match result {
        Ok(text) => (
            Some(rebon_session_host::CommandOutput {
                text,
                tone: "info".to_string(),
            }),
            None,
        ),
        Err(message) => (None, Some(message)),
    };
    let _ = rebon_session_host::write_foreground_command_response(
        &session.projects_root,
        &session.cwd,
        session_id,
        &rebon_session_host::ForegroundCommandResponse {
            command_id: command_id.to_string(),
            processed_at_ms: rebon_session_host::now_ms(),
            output,
            error,
        },
    );
}

/// The report a finished compaction prints.
///
/// Deliberately English regardless of interface language: it names the
/// engine-side structure of the compacted context (preserved prompts, the
/// generated summary, the verbatim tail), and those are the same words the
/// summary message itself uses.
pub fn render_compact_report(report: &ManualCompactReport) -> String {
    let mut lines = vec![format!(
        "Compacted successfully with {} context tokens fresh",
        thousands(report.tokens_after)
    )];

    if report.original_request_prompts > 0 {
        lines.push(format!(
            "{REPORT_BULLET} Original request · {} · {} tokens",
            plural(report.original_request_prompts, "prompt"),
            thousands(report.original_request_tokens)
        ));
    }
    if report.summary_tokens > 0 {
        lines.push(format!(
            "{REPORT_BULLET} Compact content from session · {} tokens",
            thousands(report.summary_tokens)
        ));
    }
    if report.preserved_tail_messages > 0 {
        lines.push(format!(
            "{REPORT_BULLET} Last response and prompt · {} · {} tokens",
            plural(report.preserved_tail_messages, "message"),
            thousands(report.preserved_tail_tokens)
        ));
    }
    for file in &report.files {
        lines.push(format!("{REPORT_BULLET} Recently accessed: {file}"));
    }
    if !report.used_model {
        lines.push(format!(
            "{REPORT_BULLET} No compact provider answered — older turns were dropped instead of summarized"
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "Freed {} tokens · {} → {} messages",
        thousands(report.tokens_freed()),
        report.messages_before,
        report.messages_after
    ));

    lines.join("\n")
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `1234567` → `1,234,567`. Context sizes are the one number here that is
/// genuinely hard to read as raw digits.
fn thousands(value: u32) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> ManualCompactReport {
        ManualCompactReport {
            used_model: true,
            messages_before: 142,
            messages_after: 7,
            tokens_before: 104_233,
            tokens_after: 18_432,
            original_request_prompts: 3,
            original_request_tokens: 1_204,
            summary_tokens: 6_918,
            preserved_tail_messages: 4,
            preserved_tail_tokens: 10_310,
            files: vec![
                "crates/rebon-core/src/query/compact.rs".into(),
                "crates/rebon-cli/src/tui/runner/commands.rs".into(),
            ],
        }
    }

    #[test]
    fn the_report_leads_with_the_fresh_context_size_and_lists_what_survived() {
        let text = render_compact_report(&report());
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(
            lines[0],
            "Compacted successfully with 18,432 context tokens fresh"
        );
        assert_eq!(lines[1], "⎿ Original request · 3 prompts · 1,204 tokens");
        assert_eq!(lines[2], "⎿ Compact content from session · 6,918 tokens");
        assert_eq!(
            lines[3],
            "⎿ Last response and prompt · 4 messages · 10,310 tokens"
        );
        assert_eq!(
            lines[4],
            "⎿ Recently accessed: crates/rebon-core/src/query/compact.rs"
        );
        assert_eq!(
            lines[5],
            "⎿ Recently accessed: crates/rebon-cli/src/tui/runner/commands.rs"
        );
        assert!(text.contains("Freed 85,801 tokens · 142 → 7 messages"));
    }

    #[test]
    fn a_singular_count_is_not_pluralized() {
        let mut report = report();
        report.original_request_prompts = 1;
        report.preserved_tail_messages = 1;
        let text = render_compact_report(&report);

        assert!(text.contains("· 1 prompt ·"), "{text}");
        assert!(text.contains("· 1 message ·"), "{text}");
    }

    #[test]
    fn empty_sections_are_omitted_rather_than_printed_as_zeroes() {
        let report = ManualCompactReport {
            used_model: true,
            messages_before: 12,
            messages_after: 3,
            tokens_before: 5_000,
            tokens_after: 900,
            preserved_tail_messages: 2,
            preserved_tail_tokens: 400,
            ..Default::default()
        };
        let text = render_compact_report(&report);

        assert!(!text.contains("Original request"), "{text}");
        assert!(!text.contains("Compact content"), "{text}");
        assert!(text.contains("Last response and prompt"), "{text}");
    }

    #[test]
    fn a_truncation_fallback_says_so_instead_of_claiming_a_summary() {
        let mut report = report();
        report.used_model = false;
        report.summary_tokens = 0;
        let text = render_compact_report(&report);

        assert!(text.contains("No compact provider answered"), "{text}");
        assert!(!text.contains("Compact content from session"), "{text}");
    }

    #[test]
    fn a_compaction_that_grew_the_context_reports_zero_freed_rather_than_wrapping() {
        let mut report = report();
        report.tokens_before = 100;
        report.tokens_after = 900;

        assert_eq!(report.tokens_freed(), 0);
        assert!(render_compact_report(&report).contains("Freed 0 tokens"));
    }

    #[test]
    fn thousands_groups_every_three_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
