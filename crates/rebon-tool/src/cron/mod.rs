//! Scheduled-prompt support — cron-driven runtime-state → model injection.
//!
//! Cron scheduling utilities for the `CronCreate` / `CronList` /
//! `CronDelete` tools. This module holds everything that can live
//! inside `rebon-tool` (i.e. doesn't need the attachment-poller contract):
//!
//! - [`expr`] — 5-field cron parser and next-fire arithmetic.
//! - [`tasks`] — data model, JSON IO under `<project>/.rebon/`, jitter, and
//!   missed-task detection.
//!
//! The tick loop / owner lock / poller impl live with the attachment poller
//! because they integrate with its contract. The three model-facing tools
//! (`CronCreate`, `CronList`, `CronDelete`) live in `rebon-tool` alongside
//! the other tool implementations.

pub mod expr;
pub mod tasks;

use std::sync::Arc;

pub use expr::{compute_next_cron_run, cron_to_human, parse_cron_expression, CronFields};
pub use tasks::{
    add_cron_task, cron_dir, cron_file_path, find_missed_tasks, is_recurring_task_aged,
    jittered_next_cron_run_ms, list_all_cron_tasks, mark_cron_tasks_fired, next_cron_run_ms,
    now_ms, one_shot_jittered_next_cron_run_ms, read_cron_tasks, remove_cron_tasks,
    write_cron_tasks, CronJitterConfig, CronTask, DEFAULT_CRON_JITTER_CONFIG,
};

/// Opening line of every user turn the scheduler injects instead of the user
/// typing it: a fired task's stored prompt, or the missed-task report.
///
/// The prompt may have been written by an agent through `CronCreate`, so it
/// must never read as live user intent. The auto-mode classifier's
/// Scheduled-Task Fires rule keys on this exact text; change both together.
pub const SCHEDULED_PROMPT_MARKER: &str =
    "[Scheduled task: delivered by Rebon's scheduler, not typed by the user]";

/// `prompt` as the scheduler delivers it: [`SCHEDULED_PROMPT_MARKER`] on its
/// own first line, then the prompt unchanged.
pub fn mark_scheduled_prompt(prompt: &str) -> String {
    format!("{SCHEDULED_PROMPT_MARKER}\n{prompt}")
}

/// `text` without a leading [`SCHEDULED_PROMPT_MARKER`] line, for places that
/// show a prompt as a label (job names, summaries) rather than feeding it to a
/// model.
pub fn strip_scheduled_marker(text: &str) -> &str {
    text.strip_prefix(SCHEDULED_PROMPT_MARKER)
        .map(|rest| {
            rest.strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix('\n'))
                .unwrap_or(rest)
        })
        .unwrap_or(text)
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn stripping_undoes_marking_and_leaves_other_text_alone() {
        let marked = mark_scheduled_prompt("check the build\nthen report");
        assert!(marked.starts_with(SCHEDULED_PROMPT_MARKER));
        assert_eq!(
            strip_scheduled_marker(&marked),
            "check the build\nthen report"
        );
        assert_eq!(strip_scheduled_marker("plain prompt"), "plain prompt");
    }
}

/// Cron state that [`ToolContext`](crate::ToolContext) carries in its
/// extension bag.
///
/// Storage only — read and written through the unchanged
/// `session_cron_store()` accessor.
#[derive(Clone, Default)]
pub struct CronContext {
    pub session_store: Option<Arc<tasks::SessionCronStore>>,
}
