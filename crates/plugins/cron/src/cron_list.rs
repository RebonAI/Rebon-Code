//! `CronList` tool — lists scheduled cron tasks stored under `<cwd>/.rebon/`.
//! Reads the merged durable + session task list via `rebon_tool::cron::tasks`; read-only.

use async_trait::async_trait;
use rebon_tool::cron::expr::cron_to_human;
use rebon_tool::cron::tasks::{cron_disabled, list_all_cron_tasks, SessionCronTask};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const CRON_LIST_TOOL_NAME: &str = "CronList";

#[derive(Debug, Clone, Default)]
pub struct CronListTool;

#[async_trait]
impl Tool for CronListTool {
    fn id(&self) -> ToolId {
        ToolId::new(CRON_LIST_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["CronListTool"]
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("list scheduled cron jobs")
    }

    fn description(&self) -> &str {
        "List active scheduled cron jobs. Returns each job's id, cron expression, \
         human-readable schedule, and prompt. Use to show the user what's pending or \
         to pick an id for `CronDelete`."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
        if context.team_identity().is_some() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "Cron scheduling is only supported from the main session in rebon; teammate cron routing is not available yet.".into(),
                error_code: Some(1),
            });
        }
        if cron_disabled() {
            return Ok(json!({
                "jobs": [],
                "content": "Cron scheduling is disabled by REBON_DISABLE_CRON.",
            }));
        }
        let root = project_root(context);
        let tasks = merged_tasks(context, &root);
        let jobs: Vec<Value> = tasks
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "cron": t.cron,
                    "humanSchedule": cron_to_human(&t.cron),
                    "prompt": t.prompt,
                    "recurring": t.recurring,
                    "durable": t.durable,
                    "agentId": t.agent_id,
                })
            })
            .collect();

        let content = if tasks.is_empty() {
            "No scheduled jobs.".to_string()
        } else {
            tasks
                .iter()
                .map(|t| {
                    let human = cron_to_human(&t.cron);
                    let kind = if t.recurring { "recurring" } else { "one-shot" };
                    let durability = if t.durable { "durable" } else { "session-only" };
                    let prompt_preview = truncate(&t.prompt, 80);
                    format!(
                        "{} — {} ({}, {}): {}",
                        t.id, human, kind, durability, prompt_preview
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        Ok(json!({
            "jobs": jobs,
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

fn merged_tasks(context: &ToolContext, root: &std::path::Path) -> Vec<SessionCronTask> {
    let mut tasks: Vec<SessionCronTask> = list_all_cron_tasks(root)
        .iter()
        .map(SessionCronTask::from)
        .collect();
    if let Some(store) = context.session_cron_store() {
        tasks.extend(store.list());
    }
    tasks
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::cron::tasks::{write_cron_tasks, CronTask};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    fn tmp_project() -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!(
                "rebon-cron-list-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ))
            .tempdir()
            .unwrap()
    }

    fn ctx(dir: &TempDir) -> ToolContext {
        ToolContext::new().with_cwd(dir.path().to_string_lossy().into_owned())
    }

    #[tokio::test]
    async fn empty_project_lists_no_jobs() {
        let dir = tmp_project();
        let out = CronListTool.call(json!({}), &ctx(&dir)).await.unwrap();
        assert_eq!(out["jobs"].as_array().unwrap().len(), 0);
        assert_eq!(out["content"], json!("No scheduled jobs."));
    }

    #[tokio::test]
    async fn lists_all_persisted_jobs() {
        let dir = tmp_project();
        write_cron_tasks(
            dir.path(),
            &[
                CronTask {
                    id: "aaaabbbb".into(),
                    cron: "0 9 * * 1".into(),
                    prompt: "Monday morning".into(),
                    created_at: 1_000,
                    last_fired_at: None,
                    recurring: true,
                    permanent: false,
                },
                CronTask {
                    id: "11112222".into(),
                    cron: "30 14 27 2 *".into(),
                    prompt: "Feb 27".into(),
                    created_at: 2_000,
                    last_fired_at: None,
                    recurring: false,
                    permanent: false,
                },
            ],
        )
        .unwrap();

        let out = CronListTool.call(json!({}), &ctx(&dir)).await.unwrap();
        let jobs = out["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0]["id"], json!("aaaabbbb"));
        assert_eq!(jobs[0]["humanSchedule"], json!("Every Monday at 9:00 AM"));
        assert_eq!(jobs[0]["recurring"], json!(true));
        assert_eq!(jobs[1]["id"], json!("11112222"));
        assert_eq!(jobs[1]["recurring"], json!(false));
    }

    #[test]
    fn truncate_helper_is_unicode_aware() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn tool_is_read_only_and_concurrency_safe() {
        let tool = CronListTool;
        assert!(tool.is_read_only(&json!({})));
        assert!(tool.is_concurrency_safe(&json!({})));
        assert!(tool.should_defer());
        assert_eq!(tool.aliases(), &["CronListTool"]);
    }
}
