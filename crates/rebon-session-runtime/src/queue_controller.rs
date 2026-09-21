//! Read side of the Agent Queue control plane.
//!
//! The queue store has exactly one writer: the app that hosts the queue UI. It
//! keeps the store in memory and persists by rewriting the file wholesale about
//! once a second while a row is active, so a second process writing that file
//! would be both invisible to the app and erased by its next save. Rather than
//! give another process a way to try, the app publishes a rendered plan per
//! coordinator session and this side only ever reads it.

use async_trait::async_trait;
use rebon_tool::{QueueController, QueueVerdict, QueueVerdictOutcome};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Directory of published plans, one file per coordinator session.
const COORDINATOR_PLAN_DIR: &str = "queue-plans";
/// Spool of submitted intents awaiting the queue's single writer.
const QUEUE_INTENT_DIR: &str = "queue-intents";
/// Where that writer leaves the outcome of each intent.
const QUEUE_INTENT_RESULT_DIR: &str = "queue-intent-results";
/// How long to wait for the writer to answer. The app applies intents on its
/// queue tick (~100ms), so this is many chances; past it the intent may still
/// land, which is reported rather than dressed up as success.
const INTENT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
const INTENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

static INTENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Resolve the published plan for `session_id`.
///
/// The id lands in a path, so anything that could escape the directory is
/// refused outright rather than sanitized into something that still resolves.
/// Kept byte-identical to the app's publisher: both sides must agree, and a
/// silent mismatch would read as "this session has no queue".
pub(crate) fn coordinator_plan_path(session_id: &str) -> Option<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    Some(
        crate::rebon_config::paths::config_home_dir()
            .join(COORDINATOR_PLAN_DIR)
            .join(format!("{session_id}.json")),
    )
}

/// A session id safe to use as a path component, or `None`.
///
/// Refused outright rather than sanitized: sanitizing produces something that
/// still resolves, just not to what the caller named.
fn validated_session_id(session_id: &str) -> Option<&str> {
    let session_id = session_id.trim();
    (!session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
    .then_some(session_id)
}

/// Reads the plan the app publishes for a coordinator session.
///
/// Stateless: a session id is minted after the runtime that owns this
/// controller is built, so the caller supplies it per request.
#[derive(Debug, Clone, Default)]
pub(crate) struct PublishedQueueController;

#[async_trait]
impl QueueController for PublishedQueueController {
    async fn queue_plan(&self, session_id: &str) -> Result<Option<Value>, String> {
        let Some(path) = coordinator_plan_path(session_id) else {
            return Ok(None);
        };
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("published queue plan is unreadable: {error}")),
            // A session that coordinates no queue is the common case, not a
            // failure: every background session gets a controller attached.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("could not read the published queue plan: {error}")),
        }
    }

    async fn submit_verdict(
        &self,
        verdict: QueueVerdict<'_>,
    ) -> Result<QueueVerdictOutcome, String> {
        self.submit(
            verdict.session_id,
            serde_json::json!({
                "kind": "verdict",
                "rowId": verdict.row_id,
                "generation": verdict.generation,
                "pass": verdict.pass,
                "reason": verdict.reason,
            }),
        )
        .await
    }

    async fn dispatch_row(
        &self,
        session_id: &str,
        row_id: &str,
        worktree: &str,
    ) -> Result<QueueVerdictOutcome, String> {
        self.submit(
            session_id,
            serde_json::json!({ "kind": "dispatch", "rowId": row_id, "worktree": worktree }),
        )
        .await
    }

    async fn block_row(
        &self,
        session_id: &str,
        row_id: &str,
        reason: &str,
    ) -> Result<QueueVerdictOutcome, String> {
        self.submit(
            session_id,
            serde_json::json!({ "kind": "block", "rowId": row_id, "reason": reason }),
        )
        .await
    }
}

impl PublishedQueueController {
    /// Submit one intent and wait for the queue's single writer to answer it.
    ///
    /// One file per intent, written temp-then-rename into a per-session spool.
    /// No file ever has two writers — this side only creates, the applier only
    /// consumes — so the spool needs no lock.
    async fn submit(
        &self,
        session_id: &str,
        mut body: Value,
    ) -> Result<QueueVerdictOutcome, String> {
        let Some(session_dir) = session_scoped_dir(QUEUE_INTENT_DIR, session_id) else {
            return Err("this session id cannot address a queue".to_string());
        };
        let intent_id = format!(
            "{}-{:016x}-{}",
            std::process::id(),
            now_ms(),
            INTENT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        if let Some(object) = body.as_object_mut() {
            object.insert("id".into(), Value::String(intent_id.clone()));
            object.insert("sessionId".into(), Value::String(session_id.to_string()));
            object.insert("submittedAtMs".into(), Value::from(now_ms()));
        }
        let rendered = serde_json::to_vec(&body)
            .map_err(|error| format!("could not encode the request: {error}"))?;
        std::fs::create_dir_all(&session_dir)
            .map_err(|error| format!("could not open the queue intent spool: {error}"))?;
        atomic_write(&session_dir.join(format!("{intent_id}.json")), &rendered)
            .map_err(|error| format!("could not submit the request: {error}"))?;

        let Some(result_path) = session_scoped_dir(QUEUE_INTENT_RESULT_DIR, session_id)
            .map(|dir| dir.join(format!("{intent_id}.json")))
        else {
            return Ok(QueueVerdictOutcome::Unconfirmed);
        };
        let deadline = tokio::time::Instant::now() + INTENT_CONFIRM_TIMEOUT;
        loop {
            if let Ok(bytes) = std::fs::read(&result_path) {
                let _ = std::fs::remove_file(&result_path);
                let parsed: Value = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("the queue's answer was unreadable: {error}"))?;
                return Ok(match parsed.get("rejected").and_then(Value::as_str) {
                    Some(reason) => QueueVerdictOutcome::Rejected {
                        reason: reason.to_string(),
                    },
                    None => QueueVerdictOutcome::Applied {
                        status: parsed
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("applied")
                            .to_string(),
                    },
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(QueueVerdictOutcome::Unconfirmed);
            }
            tokio::time::sleep(INTENT_POLL_INTERVAL).await;
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// A per-session directory under the config home, or `None` when the session id
/// could address anything outside it.
fn session_scoped_dir(kind: &str, session_id: &str) -> Option<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    Some(
        crate::rebon_config::paths::config_home_dir()
            .join(kind)
            .join(session_id),
    )
}

/// Write through the shared staged write, so a reader never observes a
/// half-written intent.
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    rebon_session::write_file_atomically(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_refuses_ids_that_could_escape_the_directory() {
        assert!(coordinator_plan_path("../../etc/passwd").is_none());
        assert!(coordinator_plan_path("sess/../../x").is_none());
        assert!(coordinator_plan_path("").is_none());
        assert!(coordinator_plan_path("   ").is_none());
        assert!(coordinator_plan_path(&"s".repeat(129)).is_none());
        assert!(coordinator_plan_path("sess-18ca9e4802b41a24-0").is_some());
    }

    #[tokio::test]
    async fn a_session_with_no_published_plan_reports_no_queue() {
        let plan = PublishedQueueController
            .queue_plan("sess-does-not-exist-0")
            .await
            .unwrap();
        assert_eq!(plan, None);
    }

    /// The coordinator half of the cross-process contract, driven by
    /// `scripts/queue-coordinator-contract.py`.
    ///
    /// Uses the real controller against a real app process on the other end:
    /// reads the plan the app published, then submits a verdict and waits for
    /// the app to answer it. The path rules and field names live in both crates
    /// and unit tests on each side cannot show that the two agree — this can.
    #[tokio::test]
    #[ignore = "driven by scripts/queue-coordinator-contract.py"]
    async fn contract_talk_to_a_real_app_process() {
        let session_id =
            std::env::var("REBON_CONTRACT_SESSION").expect("the script sets the session");

        // The app publishes on its first tick; give it a moment to appear.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(60);
        let plan = loop {
            match PublishedQueueController.queue_plan(&session_id).await {
                Ok(Some(plan)) => break plan,
                Ok(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
                Ok(None) => panic!("the app never published a plan this side could read"),
                Err(error) => panic!("the published plan did not parse: {error}"),
            }
        };

        // Read the fields a coordinator actually steers by.
        assert_eq!(plan["queueId"], serde_json::json!("queue-contract"));
        let rows = plan["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 2, "{plan}");
        assert_eq!(rows[0]["id"], serde_json::json!("row-1"));
        assert_eq!(rows[0]["status"], serde_json::json!("verify"));
        let generation = rows[0]["generation"]
            .as_u64()
            .expect("a generation to cite");
        assert_eq!(rows[1]["blockedBy"], serde_json::json!(["row-1"]));

        // Submit a verdict for exactly the round the plan described.
        let outcome = PublishedQueueController
            .submit_verdict(QueueVerdict {
                session_id: &session_id,
                row_id: "row-1",
                generation,
                pass: true,
                reason: "gates green",
            })
            .await
            .expect("submitting must not fail");
        assert_eq!(
            outcome,
            QueueVerdictOutcome::Applied {
                status: "done".into()
            },
            "the app applied it and this side understood the answer"
        );
    }

    #[tokio::test]
    async fn an_invalid_session_id_reports_no_queue_rather_than_failing() {
        let plan = PublishedQueueController
            .queue_plan("../escape")
            .await
            .unwrap();
        assert_eq!(plan, None);
    }
}
