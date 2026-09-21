//! `CronCreate` tool — schedules a recurring or one-shot prompt for future
//! delivery. Validation is local; storage and the timer live in `rebon_tool::cron::tasks`.
//!
//! - `durable=false` (default) stores in the process-local session store.
//! - `durable=true` persists the task to `<cwd>/.rebon/scheduled_tasks.json`.
//! - Validates the 5-field cron expression up front; rejects patterns with no
//!   match in the next year; rejects creation past the `MAX_JOBS` ceiling.
//! - Returns the generated 8-hex-char id and a human-readable schedule.

use async_trait::async_trait;
use rebon_tool::cron::expr::cron_to_human;
use rebon_tool::cron::parse_cron_expression;
use rebon_tool::cron::tasks::{
    add_cron_task, cron_disabled, list_all_cron_tasks, next_cron_run_ms, now_ms,
};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const CRON_CREATE_TOOL_NAME: &str = "CronCreate";
const MAX_JOBS: usize = 50;
/// Default recurring expiry in days — reported in the tool result so the
/// model can tell the user when the job will auto-cancel.
const DEFAULT_MAX_AGE_DAYS: i64 = 7;

#[derive(Debug, Clone, Default)]
pub struct CronCreateTool;

#[derive(Debug, Clone)]
struct Parsed {
    cron: String,
    prompt: String,
    recurring: bool,
    durable: bool,
}

#[async_trait]
impl Tool for CronCreateTool {
    fn id(&self) -> ToolId {
        ToolId::new(CRON_CREATE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["CronCreateTool"]
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("schedule recurring one-shot cron prompt timer")
    }

    fn description(&self) -> &str {
        "Schedule when to resume work in a future turn — the user has a task that should \
         fire on a wall-clock schedule (cron). The harness wakes the agent at the scheduled \
         time, delivers the prompt as a turn marked as a scheduled task, and resumes the conversation. \
         That delivery is not new user consent: the user did not type it at fire time. \
         \n\
         ## When to use\n\
         - \"every Monday at 9am check my PRs\" → recurring=true, cron=\"0 9 * * 1\".\n\
         - \"remind me at 3pm today to take meds\" → recurring=false, cron pinned to 3pm on today's date \
         (on 18 September that is cron=\"0 15 18 9 *\"; never a bare \"0 15 * * *\").\n\
         - \"run the healthcheck every 5 minutes\" → recurring=true, cron=\"*/5 * * * *\".\n\
         \n\
         ## Cron syntax\n\
         Standard 5 fields, local time: M H DoM Mon DoW. Supports `*`, `N`, `N-M`, `N-M/S`, \
         `*/N`, comma lists. DoW: 0=Sun, 7 also accepted as Sun.\n\
         \n\
         ## Storage\n\
         By default, tasks are session-only and disappear when this rebon process exits. \
         Set `durable=true` to persist to `<cwd>/.rebon/scheduled_tasks.json` across restarts. \
         Recurring durable tasks auto-expire after 7 days unless explicitly extended. Call \
         `CronDelete` to cancel sooner."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "cron": {
                    "type": "string",
                    "description": "Standard 5-field cron expression in local time: \"M H DoM Mon DoW\" (e.g. \"*/5 * * * *\" = every 5 minutes, \"30 14 28 2 *\" = Feb 28 at 2:30pm local once)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt to enqueue at each fire time."
                },
                "recurring": {
                    "type": "boolean",
                    "description": "true (default) = fire on every cron match until deleted or auto-expired after 7 days. false = fire once at the next match, then auto-delete. Use false for \"remind me at X\" one-shot requests with pinned minute/hour/dom/month."
                },
                "durable": {
                    "type": "boolean",
                    "description": "false (default) = session-only task that disappears when this rebon process exits. true = persist to <cwd>/.rebon/scheduled_tasks.json and survive restarts."
                }
            },
            "required": ["cron", "prompt"],
            "additionalProperties": false
        })
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let Parsed { cron, durable, .. } = match parse_input(input) {
            Ok(p) => p,
            refused => return validation_outcome_from(refused),
        };

        if parse_cron_expression(&cron).is_none() {
            return Ok(ValidationOutcome::invalid(
                format!("Invalid cron expression '{cron}'. Expected 5 fields: M H DoM Mon DoW."),
                1,
            ));
        }
        if next_cron_run_ms(&cron, now_ms()).is_none() {
            return Ok(ValidationOutcome::invalid(
                format!(
                    "Cron expression '{cron}' does not match any calendar date in the next year."
                ),
                2,
            ));
        }
        if cron_disabled() {
            return Ok(ValidationOutcome::invalid(
                "Cron scheduling is disabled by REBON_DISABLE_CRON",
                4,
            ));
        }
        if context.team_identity().is_some() {
            return Ok(ValidationOutcome::invalid(
                "Cron scheduling is only supported from the main session in rebon; teammate cron routing is not available yet.",
                5,
            ));
        }
        if !durable && context.session_cron_store().is_none() {
            return Ok(ValidationOutcome::invalid(
                "Session-only cron scheduling is unavailable in this runtime. Set durable=true to persist the task.",
                6,
            ));
        }
        let total_jobs = list_all_cron_tasks(&project_root(context)).len()
            + context
                .session_cron_store()
                .map(|store| store.list().len())
                .unwrap_or(0);
        if total_jobs >= MAX_JOBS {
            return Ok(ValidationOutcome::invalid(
                format!("Too many scheduled jobs (max {MAX_JOBS}). Cancel one first."),
                3,
            ));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let Parsed {
            cron,
            prompt,
            recurring,
            durable,
        } = parse_input(&input)?;

        if cron_disabled() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "Cron scheduling is disabled by REBON_DISABLE_CRON".into(),
                error_code: Some(4),
            });
        }
        if context.team_identity().is_some() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "Cron scheduling is only supported from the main session in rebon; teammate cron routing is not available yet.".into(),
                error_code: Some(5),
            });
        }

        let root = project_root(context);
        let created_at = now_ms();
        let id = if durable {
            add_cron_task(&root, &cron, &prompt, recurring, created_at).map_err(|err| {
                ToolError::Execution {
                    tool: self.id(),
                    source: err,
                }
            })?
        } else {
            let store = context.session_cron_store().ok_or_else(|| ToolError::InvalidInput {
                tool: self.id(),
                reason: "Session-only cron scheduling is unavailable in this runtime. Set durable=true to persist the task.".into(),
                error_code: Some(6),
            })?;
            store.add(&cron, &prompt, recurring, created_at, None)
        };

        let human = cron_to_human(&cron);
        let storage = if durable {
            "durable; persisted to .rebon/scheduled_tasks.json"
        } else {
            "session-only; will disappear when this rebon process exits"
        };
        let content = if recurring {
            let expiry = if durable {
                format!(" Auto-expires after {DEFAULT_MAX_AGE_DAYS} days.")
            } else {
                String::new()
            };
            format!(
                "Scheduled recurring job {id} ({human}) as {storage}.{expiry} Use CronDelete to cancel sooner."
            )
        } else {
            format!(
                "Scheduled one-shot task {id} ({human}) as {storage}. It will fire once then auto-delete."
            )
        };

        Ok(json!({
            "id": id,
            "humanSchedule": human,
            "recurring": recurring,
            "durable": durable,
            "content": content,
        }))
    }
}

fn project_root(context: &ToolContext) -> PathBuf {
    context
        .cwd()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn parse_input(input: &Value) -> ToolResult<Parsed> {
    let tool = ToolId::new(CRON_CREATE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "CronCreate input must be an object".into(),
        error_code: Some(400),
    })?;
    let cron = object
        .get("cron")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "CronCreate requires a string `cron`".into(),
            error_code: Some(400),
        })?
        .trim()
        .to_string();
    let prompt = object
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "CronCreate requires a string `prompt`".into(),
            error_code: Some(400),
        })?
        .to_string();
    let recurring = object
        .get("recurring")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let durable = object
        .get("durable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(Parsed {
        cron,
        prompt,
        recurring,
        durable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::cron::tasks::read_cron_tasks;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    fn tmp_project() -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!(
                "rebon-cron-create-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ))
            .tempdir()
            .unwrap()
    }

    fn ctx_with_cwd(dir: &TempDir) -> ToolContext {
        ToolContext::new().with_cwd(dir.path().to_string_lossy().into_owned())
    }

    fn ctx_with_session_store(dir: &TempDir) -> ToolContext {
        ctx_with_cwd(dir).with_session_cron_store(rebon_tool::SessionCronStore::new())
    }

    #[tokio::test]
    async fn rejects_invalid_cron() {
        let dir = tmp_project();
        let ctx = ctx_with_cwd(&dir);
        let outcome = CronCreateTool
            .validate_input(&json!({"cron": "not a cron", "prompt": "hi"}), &ctx)
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert_eq!(outcome.error_code, Some(1));
    }

    #[tokio::test]
    async fn durable_true_persists_task() {
        let dir = tmp_project();
        let ctx = ctx_with_session_store(&dir);
        let out = CronCreateTool
            .call(
                json!({"cron": "0 9 * * *", "prompt": "morning", "recurring": true, "durable": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["recurring"], json!(true));
        assert_eq!(out["durable"], json!(true));
        assert_eq!(out["humanSchedule"], json!("Every day at 9:00 AM"));
        assert!(out["content"].as_str().unwrap().contains("durable"));

        let tasks = read_cron_tasks(dir.path());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].cron, "0 9 * * *");
        assert_eq!(tasks[0].prompt, "morning");
        assert!(tasks[0].recurring);
    }

    #[tokio::test]
    async fn durable_omitted_defaults_to_session_only() {
        let dir = tmp_project();
        let ctx = ctx_with_session_store(&dir);
        let out = CronCreateTool
            .call(
                json!({"cron": "0 9 * * *", "prompt": "morning", "recurring": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["durable"], json!(false));
        assert!(out["content"].as_str().unwrap().contains("session-only"));
        assert!(read_cron_tasks(dir.path()).is_empty());
        assert_eq!(ctx.session_cron_store().unwrap().list().len(), 1);
    }

    #[tokio::test]
    async fn rejects_when_max_jobs_hit() {
        let dir = tmp_project();
        let ctx = ctx_with_session_store(&dir);
        // Pre-fill MAX_JOBS tasks directly.
        let mut tasks = Vec::new();
        for i in 0..MAX_JOBS {
            tasks.push(rebon_tool::cron::tasks::CronTask {
                id: format!("{:08x}", i),
                cron: "0 9 * * *".into(),
                prompt: "stub".into(),
                created_at: 1_000,
                last_fired_at: None,
                recurring: false,
                permanent: false,
            });
        }
        rebon_tool::cron::tasks::write_cron_tasks(dir.path(), &tasks).unwrap();

        let outcome = CronCreateTool
            .validate_input(&json!({"cron": "0 9 * * *", "prompt": "p"}), &ctx)
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert_eq!(outcome.error_code, Some(3));
    }

    #[tokio::test]
    async fn recurring_default_is_true() {
        let dir = tmp_project();
        let ctx = ctx_with_session_store(&dir);
        let out = CronCreateTool
            .call(json!({"cron": "0 9 * * *", "prompt": "m"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["recurring"], json!(true));
    }

    #[test]
    fn tool_exposes_alias_and_defers() {
        let tool = CronCreateTool;
        assert_eq!(tool.aliases(), &["CronCreateTool"]);
        assert!(tool.should_defer());
        assert_eq!(tool.id().as_str(), CRON_CREATE_TOOL_NAME);
    }
}
