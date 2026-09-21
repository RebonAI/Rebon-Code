//! `CronDelete` tool — cancels a scheduled cron task by id. Removes the id from
//! both the durable and the session task stores, via `rebon_tool::cron::tasks`.

use async_trait::async_trait;
use rebon_tool::cron::tasks::{cron_disabled, list_all_cron_tasks, remove_cron_tasks};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const CRON_DELETE_TOOL_NAME: &str = "CronDelete";

#[derive(Debug, Clone, Default)]
pub struct CronDeleteTool;

#[async_trait]
impl Tool for CronDeleteTool {
    fn id(&self) -> ToolId {
        ToolId::new(CRON_DELETE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["CronDeleteTool"]
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("cancel scheduled cron job")
    }

    fn description(&self) -> &str {
        "Cancel a scheduled cron job by id. Use `CronList` first to discover ids. \
         Removing a non-existent id is a validation error, not a silent no-op, so \
         the user sees a clear failure when they mistype."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Job id returned by CronCreate or shown by CronList."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let id = match parse_id(input) {
            Ok(id) => id,
            Err(ToolError::InvalidInput {
                reason, error_code, ..
            }) => {
                return Ok(ValidationOutcome::invalid(
                    reason,
                    error_code.unwrap_or(400),
                ))
            }
            Err(err) => return Err(err),
        };
        if cron_disabled() {
            return Ok(ValidationOutcome::invalid(
                "Cron scheduling is disabled by REBON_DISABLE_CRON",
                2,
            ));
        }
        if context.team_identity().is_some() {
            return Ok(ValidationOutcome::invalid(
                "Cron scheduling is only supported from the main session in rebon; teammate cron routing is not available yet.",
                3,
            ));
        }
        let root = project_root(context);
        let durable_exists = list_all_cron_tasks(&root).iter().any(|t| t.id == id);
        let session_exists = context
            .session_cron_store()
            .map(|store| store.list().iter().any(|t| t.id == id))
            .unwrap_or(false);
        if !durable_exists && !session_exists {
            return Ok(ValidationOutcome::invalid(
                format!("No scheduled job with id '{id}'"),
                1,
            ));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let id = parse_id(&input)?;
        if cron_disabled() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "Cron scheduling is disabled by REBON_DISABLE_CRON".into(),
                error_code: Some(2),
            });
        }
        if context.team_identity().is_some() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "Cron scheduling is only supported from the main session in rebon; teammate cron routing is not available yet.".into(),
                error_code: Some(3),
            });
        }
        let root = project_root(context);
        let durable_removed =
            remove_cron_tasks(&root, std::slice::from_ref(&id)).map_err(|err| {
                ToolError::Execution {
                    tool: self.id(),
                    source: err,
                }
            })?;
        let session_removed = context
            .session_cron_store()
            .map(|store| store.remove(std::slice::from_ref(&id)))
            .unwrap_or(0);
        let removed = durable_removed + session_removed;
        Ok(json!({
            "id": id,
            "removed": removed,
            "content": format!("Cancelled job {id}."),
        }))
    }
}

fn parse_id(input: &Value) -> ToolResult<String> {
    let tool = ToolId::new(CRON_DELETE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "CronDelete input must be an object".into(),
        error_code: Some(400),
    })?;
    let id = object
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "CronDelete requires a string `id`".into(),
            error_code: Some(400),
        })?
        .trim()
        .to_string();
    if id.is_empty() {
        return Err(ToolError::InvalidInput {
            tool,
            reason: "`id` must not be empty".into(),
            error_code: Some(400),
        });
    }
    Ok(id)
}

fn project_root(context: &ToolContext) -> PathBuf {
    context
        .cwd()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::cron::tasks::{read_cron_tasks, write_cron_tasks, CronTask};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    fn tmp_project() -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!(
                "rebon-cron-delete-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ))
            .tempdir()
            .unwrap()
    }

    fn ctx(dir: &TempDir) -> ToolContext {
        ToolContext::new().with_cwd(dir.path().to_string_lossy().into_owned())
    }

    fn seed(dir: &TempDir, ids: &[&str]) {
        let tasks: Vec<CronTask> = ids
            .iter()
            .map(|id| CronTask {
                id: (*id).into(),
                cron: "0 9 * * *".into(),
                prompt: "stub".into(),
                created_at: 1_000,
                last_fired_at: None,
                recurring: false,
                permanent: false,
            })
            .collect();
        write_cron_tasks(dir.path(), &tasks).unwrap();
    }

    #[tokio::test]
    async fn rejects_unknown_id() {
        let dir = tmp_project();
        seed(&dir, &["aaaabbbb"]);
        let outcome = CronDeleteTool
            .validate_input(&json!({"id": "nope"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert_eq!(outcome.error_code, Some(1));
    }

    #[tokio::test]
    async fn rejects_missing_id_field() {
        let dir = tmp_project();
        let outcome = CronDeleteTool
            .validate_input(&json!({}), &ctx(&dir))
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert_eq!(outcome.error_code, Some(400));
    }

    #[tokio::test]
    async fn removes_existing_task() {
        let dir = tmp_project();
        seed(&dir, &["aaaabbbb", "11112222"]);
        let out = CronDeleteTool
            .call(json!({"id": "aaaabbbb"}), &ctx(&dir))
            .await
            .unwrap();
        assert_eq!(out["id"], json!("aaaabbbb"));
        assert_eq!(out["removed"], json!(1));
        let remaining = read_cron_tasks(dir.path());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "11112222");
    }

    #[test]
    fn tool_defers_and_exposes_alias() {
        let tool = CronDeleteTool;
        assert!(tool.should_defer());
        assert_eq!(tool.aliases(), &["CronDeleteTool"]);
    }
}
