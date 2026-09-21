use async_trait::async_trait;
use serde_json::{json, Value};

use rebon_tool::{clear_current_team_name, current_team_name, Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome};

pub const TEAM_DELETE_TOOL_NAME: &str = "TeamDelete";

#[derive(Debug, Clone, Default)]
pub struct TeamDeleteTool;

#[async_trait]
impl Tool for TeamDeleteTool {
    fn id(&self) -> ToolId {
        ToolId::new(TEAM_DELETE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TeamDeleteTool"]
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Delete the current team after all teammates have exited.\n\
         \n\
         This removes the team's config directory and shared task list.\n\
         TeamDelete fails while active teammates still exist."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
        let Some(team_name) = context.current_team_name() else {
            return Ok(json!({
                "success": true,
                "message": "No active team to delete",
            }));
        };
        let manager = context
            .team_manager()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("TeamDelete requires a TeamManager"),
            })?
            .clone();
        manager
            .delete_team(&team_name)
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;
        if context.session_id().is_none()
            && context.team_identity().is_none()
            && current_team_name().as_deref() == Some(team_name.as_str())
        {
            clear_current_team_name();
        }
        Ok(json!({
            "success": true,
            "team_name": team_name,
            "message": format!("Deleted team `{}`", team_name),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::tasks::test_support::TestConfigHome;
    use rebon_tool::{TeamFile, TeamManager, TeammateSpawnResult, TeammateSpawnSpec};
    use std::sync::{Arc, Mutex};

    struct ScriptedTeamManager {
        deleted: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TeamManager for ScriptedTeamManager {
        async fn spawn_teammate(
            &self,
            _spec: TeammateSpawnSpec,
        ) -> Result<TeammateSpawnResult, String> {
            unreachable!()
        }

        async fn send_message(
            &self,
            _team_name: &str,
            _recipient: &str,
            _message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            Ok(())
        }

        async fn request_shutdown(
            &self,
            _team_name: &str,
            _recipient: &str,
            _reason: Option<String>,
        ) -> Result<String, String> {
            Ok("req".into())
        }

        async fn request_plan_approval(
            &self,
            _team_name: &str,
            _agent_name: &str,
            _plan_content: String,
        ) -> Result<String, String> {
            Ok("plan".into())
        }

        async fn delete_team(&self, team_name: &str) -> Result<(), String> {
            self.deleted.lock().unwrap().push(team_name.into());
            std::fs::remove_dir_all(rebon_tool::team_files::team_dir(team_name))
                .map_err(|error| error.to_string())?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn team_delete_uses_manager_and_clears_context() {
        let _home = TestConfigHome::new("team-delete");
        rebon_tool::write_team_file(
            "alpha",
            &TeamFile {
                name: "alpha".into(),
                description: None,
                created_at: rebon_tool::team_files::now_wall_ms(),
                lead_agent_id: "team-lead@alpha".into(),
                lead_session_id: Some("team-delete-session".into()),
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();
        let manager = Arc::new(ScriptedTeamManager {
            deleted: Mutex::new(Vec::new()),
        });
        let tool = TeamDeleteTool;
        let context = ToolContext::new()
            .with_session_id("team-delete-session")
            .with_team_manager(manager.clone() as Arc<dyn TeamManager>);
        let out = tool.call(json!({}), &context).await.unwrap();
        assert_eq!(out["success"], json!(true));
        assert!(context.current_team_name().is_none());
        assert_eq!(manager.deleted.lock().unwrap()[0], "alpha");
    }
}
