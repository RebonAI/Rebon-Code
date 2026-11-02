//! Worker digests: how the router hears what its workers did.
//!
//! Each tick the supervisor looks at the router's workers. A worker that
//! finished a turn, failed, is waiting for an answer, or was stopped gets a
//! line. Lines from the same few seconds are batched into one scheduled turn
//! for the router, so a fan-out of workers finishing together interrupts it
//! once rather than once per worker. Each line carries the worker's id; the
//! full reply stays in the worker's transcript, where `Recall` finds it.
//!
//! What has been reported lives in `<store>/router-digest.json`, which only
//! the supervisor writes, so a restart neither repeats nor loses a line.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use rebon_session_host::{BackgroundJobState, BackgroundJobStatus, BackgroundStore};
use serde::{Deserialize, Serialize};

const STATE_FILE_NAME: &str = "router-digest.json";
/// How long the first line waits for others before the digest goes out.
const BATCH_MS: u64 = 15_000;
const MAX_LINE_CHARS: usize = 300;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DigestState {
    #[serde(default)]
    router_job_id: Option<String>,
    /// Each worker's state as last seen: its status and turn.
    #[serde(default)]
    seen: BTreeMap<String, (String, u64)>,
    /// Lines waiting to go out.
    #[serde(default)]
    pending: Vec<String>,
    #[serde(default)]
    pending_since_ms: Option<u64>,
}

/// The supervisor's digest state between ticks.
pub(crate) struct RouterDigest {
    path: PathBuf,
    state: RefCell<Option<DigestState>>,
}

impl RouterDigest {
    pub(crate) fn new(store: &BackgroundStore) -> Self {
        Self {
            path: store.root().join(STATE_FILE_NAME),
            state: RefCell::new(None),
        }
    }

    /// One tick. `true` while lines wait to go out, which keeps the
    /// supervisor up for them.
    pub(crate) fn tick(
        &self,
        store: &BackgroundStore,
        jobs: &[BackgroundJobState],
        now_ms: u64,
    ) -> bool {
        let Some(router) = crate::router::current_router(store) else {
            return false;
        };
        let mut slot = self.state.borrow_mut();
        let state = slot.get_or_insert_with(|| {
            std::fs::read(&self.path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        });
        let before = state.clone();
        let router_id = router.identity.job_id.as_str();
        collect(state, router_id, jobs, now_ms);
        let due = state
            .pending_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) >= BATCH_MS);
        if due {
            let id = format!("u-internal-scheduled-digest-{now_ms}");
            let text = rebon_tool::cron::mark_scheduled_prompt(&render(&state.pending));
            match rebon_session_host::append_background_scheduled_prompt(store, router_id, id, text)
            {
                Ok(_) => {
                    state.pending.clear();
                    state.pending_since_ms = None;
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "rebon: could not deliver a worker digest; retrying");
                }
            }
        }
        if *state != before {
            let written = serde_json::to_vec_pretty(state)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    rebon_session::write_file_atomically(&self.path, &bytes)
                        .map_err(anyhow::Error::from)
                });
            if let Err(error) = written {
                tracing::warn!(error = %format!("{error:#}"), "rebon: could not save the worker digest");
            }
        }
        !state.pending.is_empty()
    }
}

/// Note every worker of `router_id` whose state moved to one worth a line.
fn collect(state: &mut DigestState, router_id: &str, jobs: &[BackgroundJobState], now_ms: u64) {
    if state.router_job_id.as_deref() != Some(router_id) {
        *state = DigestState {
            router_job_id: Some(router_id.to_string()),
            ..DigestState::default()
        };
    }
    let workers: Vec<&BackgroundJobState> = jobs
        .iter()
        .filter(|job| job.identity.parent_job_id.as_deref() == Some(router_id))
        .collect();
    for job in &workers {
        let now = (
            status_key(job.process.status).to_string(),
            job.process.turn_generation,
        );
        let changed = state.seen.get(&job.identity.job_id) != Some(&now);
        if changed {
            if let Some(line) = line(job) {
                state.pending.push(line);
                state.pending_since_ms.get_or_insert(now_ms);
            }
            state.seen.insert(job.identity.job_id.clone(), now);
        }
    }
    state
        .seen
        .retain(|id, _| workers.iter().any(|job| job.identity.job_id == *id));
}

fn status_key(status: BackgroundJobStatus) -> &'static str {
    match status {
        BackgroundJobStatus::Queued => "queued",
        BackgroundJobStatus::Running => "running",
        BackgroundJobStatus::NeedsInput => "needs_input",
        BackgroundJobStatus::Idle => "idle",
        BackgroundJobStatus::Succeeded => "succeeded",
        BackgroundJobStatus::Failed => "failed",
        BackgroundJobStatus::Stopped => "stopped",
    }
}

/// The line for a worker's new state, if the state is worth one.
fn line(job: &BackgroundJobState) -> Option<String> {
    let id = &job.identity.job_id;
    let name = &job.identity.name;
    let what = match job.process.status {
        BackgroundJobStatus::Queued | BackgroundJobStatus::Running => return None,
        BackgroundJobStatus::Idle | BackgroundJobStatus::Succeeded => {
            let summary = job
                .outcome
                .summary
                .as_deref()
                .and_then(|summary| summary.lines().find(|line| !line.trim().is_empty()))
                .unwrap_or("no summary yet");
            format!("finished a turn: {}", clip(summary.trim()))
        }
        BackgroundJobStatus::Failed => format!(
            "failed: {}",
            clip(job.outcome.error.as_deref().unwrap_or("no error recorded"))
        ),
        BackgroundJobStatus::NeedsInput => {
            let asking = job.outcome.pending_permission.as_ref().and_then(|query| {
                query
                    .title
                    .as_deref()
                    .or(query.tool.as_deref())
                    .map(str::to_string)
            });
            match asking {
                Some(asking) => format!("is waiting for an answer: {}", clip(&asking)),
                None => "is waiting for an answer".to_string(),
            }
        }
        BackgroundJobStatus::Stopped => "was stopped".to_string(),
    };
    Some(format!("- {id} \"{name}\" {what}"))
}

fn clip(text: &str) -> String {
    let mut clipped: String = text.chars().take(MAX_LINE_CHARS).collect();
    if text.chars().count() > MAX_LINE_CHARS {
        clipped.push('…');
    }
    clipped
}

fn render(lines: &[String]) -> String {
    format!(
        "[Worker digest]\n{}\n\nRecall an id to see its last reply. Decide what each one needs; most need nothing.",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::BackgroundRuntimeFields;

    fn worker(
        id: &str,
        parent: &str,
        status: BackgroundJobStatus,
        turn: u64,
    ) -> BackgroundJobState {
        let mut job = BackgroundJobState::new(
            "p".into(),
            ".".into(),
            BackgroundRuntimeFields {
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
            },
            Some(format!("task {id}")),
        );
        job.identity.job_id = id.into();
        job.identity.parent_job_id = Some(parent.into());
        job.process.status = status;
        job.process.turn_generation = turn;
        job
    }

    #[test]
    fn a_worker_is_reported_once_per_settled_turn() {
        let mut state = DigestState::default();
        let mut job = worker("w1", "r", BackgroundJobStatus::Running, 1);
        collect(&mut state, "r", std::slice::from_ref(&job), 0);
        assert!(state.pending.is_empty(), "running is not news");

        job.process.status = BackgroundJobStatus::Idle;
        job.outcome.summary = Some("\nOpened a pull request\nmore".into());
        collect(&mut state, "r", std::slice::from_ref(&job), 10);
        collect(&mut state, "r", std::slice::from_ref(&job), 20);
        assert_eq!(
            state.pending,
            ["- w1 \"task w1\" finished a turn: Opened a pull request"]
        );
        assert_eq!(state.pending_since_ms, Some(10));

        job.process.turn_generation = 2;
        job.process.status = BackgroundJobStatus::Failed;
        job.outcome.error = Some("quota".into());
        collect(&mut state, "r", std::slice::from_ref(&job), 30);
        assert_eq!(state.pending.len(), 2);
        assert!(state.pending[1].ends_with("failed: quota"));
    }

    #[test]
    fn only_the_routers_own_workers_count_and_a_new_router_starts_over() {
        let mut state = DigestState::default();
        let mine = worker("w1", "r", BackgroundJobStatus::Idle, 1);
        let other = worker("w2", "someone", BackgroundJobStatus::Idle, 1);
        collect(&mut state, "r", &[mine.clone(), other], 0);
        assert_eq!(state.pending.len(), 1);
        collect(&mut state, "r2", &[mine], 5);
        assert!(state.pending.is_empty(), "w1 is not r2's");
        assert_eq!(state.router_job_id.as_deref(), Some("r2"));
    }

    #[test]
    fn the_digest_names_ids_to_recall() {
        let text = render(&["- w1 \"t\" was stopped".to_string()]);
        assert!(text.starts_with("[Worker digest]\n- w1"));
        assert!(text.contains("Recall an id"));
    }
}
