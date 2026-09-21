use crate::memory::save::{
    parse_scope, save_memory, MemoryContentType, MemorySource, SaveMemoryAction, SaveMemoryRequest,
};
use async_trait::async_trait;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, PermissionDecision, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use serde_json::{json, Map, Value};

pub const SAVE_MEMORY_TOOL_NAME: &str = "SaveMemory";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct SaveMemoryTool;

#[async_trait]
impl Tool for SaveMemoryTool {
    fn id(&self) -> ToolId {
        ToolId::new(SAVE_MEMORY_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["SaveMemoryTool"]
    }

    fn description(&self) -> &str {
        "Save or delete durable user/repo memory through a narrow, internally restricted primitive. \
         Use this for explicit durable preferences, feedback, project context, or references that should survive future sessions. \
         Do not use it for transient task state, scratchpad notes, worker progress, session state, or coordinator bookkeeping, \
         nor for what the repository already records (code patterns, git history and recent fixes, repair recipes, REBON.md content), even when asked. \
         Storage `scope` is only `user` or `repo`; content `type` is taxonomy (`user`, `feedback`, `project`, `reference`)."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["upsert", "delete"],
                    "description": "Whether to save/update (`upsert`) or remove (`delete`) a durable memory."
                },
                "scope": {
                    "type": "string",
                    "enum": ["user", "repo"],
                    "description": "Durable storage scope. Only `user` and `repo` are valid. Do not use task/session/coordinator/project as storage scopes; use scope `repo` with type `project` for project memories."
                },
                "type": {
                    "type": "string",
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "Required for upsert. Content taxonomy, separate from storage scope."
                },
                "title": {
                    "type": "string",
                    "description": "Required for upsert; optional for delete. Human-readable memory title."
                },
                "description": {
                    "type": "string",
                    "description": "Required for upsert. One-line hook used in the MEMORY.md index."
                },
                "content": {
                    "type": "string",
                    "description": "Required for upsert. Durable memory body."
                },
                "source": {
                    "type": "string",
                    "enum": ["explicit_user_request", "durable_feedback", "coordinator_synthesis", "assistant_inferred", "forget_request"],
                    "description": "Optional source classification for why this memory change is being made."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional rationale explaining why this is durable or why it should be deleted."
                },
                "dedupe_key": {
                    "type": "string",
                    "description": "Optional stable dedupe key. Exact matching frontmatter will update instead of creating a duplicate."
                },
                "target_file": {
                    "type": "string",
                    "description": "Optional basename-only .md target. No path separators, absolute paths, drive/UNC prefixes, `..`, or MEMORY.md."
                }
            },
            "required": ["action", "scope"],
            "additionalProperties": false
        })
    }

    fn is_destructive(&self, input: &Value) -> bool {
        input.get("action").and_then(Value::as_str) == Some("delete")
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_input(input))
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(PermissionDecision::allow(input.clone()))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let request = parse_input(&input)?;
        let cwd = context.cwd().ok_or_else(|| ToolError::InvalidInput {
            tool: self.id(),
            reason: "cwd_missing: SaveMemory requires ToolContext.cwd() to resolve repo memory"
                .into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

        let outcome = save_memory(cwd, request).map_err(|err| ToolError::InvalidInput {
            tool: self.id(),
            reason: err.to_string(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

        let mut result = json!({
            "status": outcome.status,
            "action": outcome.action.as_str(),
            "scope": scope_as_str(outcome.scope),
            "memory_path": outcome.memory_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "index_path": outcome.index_path.to_string_lossy().into_owned(),
            "message": outcome.message,
            "dedupe_matched": outcome.dedupe_matched,
        });

        if let Some(path) = outcome.memory_path.as_ref() {
            let note = crate::memory::update_notification::format_update_notification_from_env(
                &path.to_string_lossy(),
                cwd,
            );
            result["memoryNotification"] = Value::String(note);
        }

        Ok(result)
    }
}

fn parse_input(input: &Value) -> ToolResult<SaveMemoryRequest> {
    let tool = ToolId::new(SAVE_MEMORY_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "SaveMemory input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let action = SaveMemoryAction::parse(&required_string(object, "action", &tool)?)
        .map_err(|err| invalid(&tool, err.to_string()))?;
    let scope = parse_scope(&required_string(object, "scope", &tool)?)
        .map_err(|err| invalid(&tool, err.to_string()))?;
    let memory_type = optional_string(object, "type", &tool)?
        .map(|raw| MemoryContentType::parse(&raw))
        .transpose()
        .map_err(|err| invalid(&tool, err.to_string()))?;
    let source = optional_string(object, "source", &tool)?
        .map(|raw| MemorySource::parse(&raw))
        .transpose()
        .map_err(|err| invalid(&tool, err.to_string()))?;

    Ok(SaveMemoryRequest {
        action,
        scope,
        memory_type,
        title: optional_string(object, "title", &tool)?,
        description: optional_string(object, "description", &tool)?,
        content: optional_string(object, "content", &tool)?,
        source,
        reason: optional_string(object, "reason", &tool)?,
        dedupe_key: optional_string(object, "dedupe_key", &tool)?,
        target_file: optional_string(object, "target_file", &tool)?,
    })
}

fn required_string(object: &Map<String, Value>, field: &str, tool: &ToolId) -> ToolResult<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid(tool, format!("SaveMemory input requires string `{field}`")))
}

fn optional_string(
    object: &Map<String, Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<String>> {
    match object.get(field) {
        Some(Value::String(raw)) => Ok(Some(raw.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(invalid(
            tool,
            format!("`{field}` must be a string when provided"),
        )),
    }
}

fn invalid(tool: &ToolId, reason: String) -> ToolError {
    ToolError::InvalidInput {
        tool: tool.clone(),
        reason,
        error_code: Some(INVALID_INPUT_CODE),
    }
}

fn scope_as_str(scope: rebon_session::memory_paths::MemoryScope) -> &'static str {
    match scope {
        rebon_session::memory_paths::MemoryScope::User => "user",
        rebon_session::memory_paths::MemoryScope::Repo => "repo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::Tool;

    /// `std::env::set_var` is process-wide, so the tests that rewrite HOME
    /// take this before touching it — and it is *the* lock this crate's
    /// tests take, not a second one.
    ///
    /// This module kept its own until the store moved in beside it on
    /// 2026-09-05. Two locks over one process-global serialise nothing:
    /// `loaded_files` started reading the real `~/.rebon/REBON.md` whenever
    /// one of these tests happened to restore the environment mid-read. A
    /// lock cannot be shared across *crates* and does not need to be, since
    /// each test binary is its own process; inside one crate there is one.
    use crate::memory::test_env::env_test_lock;

    struct EnvGuard {
        _env_lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
        prev_rebon_config_dir: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let env_lock = env_test_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let temp = tempfile::tempdir().expect("temp home");
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            let prev_rebon_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("USERPROFILE", temp.path());
            std::env::set_var("REBON_CONFIG_DIR", temp.path().join(".rebon-test"));
            Self {
                _env_lock: env_lock,
                _temp: temp,
                prev_home,
                prev_userprofile,
                prev_rebon_config_dir,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev_rebon_config_dir.take() {
                Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_userprofile.take() {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    #[test]
    fn schema_exposes_scope_and_type_taxonomy() {
        let schema = SaveMemoryTool.input_schema();
        assert_eq!(
            schema["properties"]["scope"]["enum"],
            json!(["user", "repo"])
        );
        assert_eq!(
            schema["properties"]["type"]["enum"],
            json!(["user", "feedback", "project", "reference"])
        );
    }

    #[tokio::test]
    async fn validates_project_as_invalid_storage_scope() {
        let tool = SaveMemoryTool;
        let outcome = tool
            .validate_input(
                &json!({"action":"upsert","scope":"project"}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert!(outcome
            .message
            .unwrap()
            .contains("storage scope must be `user` or `repo`"));
    }

    #[tokio::test]
    async fn call_writes_memory_with_cwd() {
        let _env = EnvGuard::new();
        let tool = SaveMemoryTool;
        let context = ToolContext::new().with_cwd("/repo");
        let output = tool
            .call(
                json!({
                    "action": "upsert",
                    "scope": "user",
                    "type": "feedback",
                    "title": "Prefer terse replies",
                    "description": "User prefers concise final responses",
                    "content": "The user prefers concise final responses without repeated summaries."
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(output["status"], "created");
        assert!(output["memory_path"]
            .as_str()
            .unwrap()
            .ends_with("feedback-prefer-terse-replies.md"));
        assert!(output["memoryNotification"]
            .as_str()
            .unwrap()
            .contains("Memory updated"));
    }
}
