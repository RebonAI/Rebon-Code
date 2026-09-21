use async_trait::async_trait;
use serde_json::{json, Value};

use rebon_tool::team_files::{
    ensure_team_task_dir, format_agent_id, now_wall_ms, set_current_team_name, team_file_path,
    unique_team_name, write_team_file, TeamFile, TeamMember, TEAM_LEAD_NAME,
};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};

pub const TEAM_CREATE_TOOL_NAME: &str = "TeamCreate";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct TeamCreateTool;

#[derive(Debug, Clone)]
struct TeamCreateInput {
    team_name: String,
    description: Option<String>,
    agent_type: Option<String>,
}

#[async_trait]
impl Tool for TeamCreateTool {
    fn id(&self) -> ToolId {
        ToolId::new(TEAM_CREATE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TeamCreateTool"]
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("multi-agent team parallel collaboration")
    }

    fn description(&self) -> &str {
        "Create a new multi-agent team and switch the current session into that team.\n\
         \n\
         Use this when the work benefits from multiple agents collaborating in parallel.\n\
         After TeamCreate succeeds:\n\
         - The shared team config lives at ~/.rebon/teams/{team-name}/config.json\n\
         - The shared task list lives at ~/.rebon/tasks/{team-name}/\n\
         - TaskCreate / TaskList / TaskUpdate automatically operate on that team task list\n\
         - Spawn teammates with Agent using `name` plus `team_name` (or omit `team_name` to use the current team)\n\
         \n\
         Team workflow:\n\
         1. Call TeamCreate once to create the team.\n\
         2. Use Agent with `name` and `team_name` to create teammates.\n\
         3. Use TaskCreate / TaskUpdate to create and assign shared tasks. Task owners must use teammate names.\n\
         4. Teammates read and update the shared task list while they work.\n\
         5. When all work is done, shut down teammates via SendMessage with `message: {type: \"shutdown_request\"}`.\n\
         \n\
         Automatic message delivery:\n\
         Messages from teammates are automatically delivered to you. You do NOT need to \
         manually check an inbox or poll for replies.\n\
         - Teammates send you messages when they complete tasks or need help.\n\
         - These messages appear automatically as new conversation turns.\n\
         - If you are busy (mid-turn), messages are queued and delivered when your turn ends.\n\
         \n\
         Teammate idle state:\n\
         Teammates go idle after every turn — this is completely normal and expected. \
         A teammate going idle immediately after sending you a message does NOT mean they \
         are done or unavailable. Idle simply means they are waiting for input.\n\
         - Idle teammates can receive messages. Sending a message wakes them up.\n\
         - Idle notifications are automatic. You do not need to react to them unless \
         you want to assign new work.\n\
         - Do not treat idle as an error.\n\
         \n\
         Important rules:\n\
         - Only one active team is supported per session.\n\
         - Teammates are identified by NAME for task ownership and messaging.\n\
         - Your plain text output is NOT visible to other agents — use SendMessage to communicate.\n\
         - Do NOT send structured JSON status messages. Use TaskUpdate to mark tasks completed.\n\
         - If the requested team name already exists, a unique variant is created automatically."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "team_name": {
                    "type": "string",
                    "description": "Name for the new team."
                },
                "description": {
                    "type": "string",
                    "description": "Optional team purpose/description."
                },
                "agent_type": {
                    "type": "string",
                    "description": "Optional role/type label for the team lead."
                }
            },
            "required": ["team_name"],
            "additionalProperties": false
        })
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_input(input))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input)?;
        if let Some(existing) = context.current_team_name() {
            return Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(
                    "already leading team `{existing}`; only one active team is supported per session"
                ),
            });
        }

        let final_team_name =
            unique_team_name(&parsed.team_name).map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            })?;
        ensure_team_task_dir(&final_team_name).map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;

        let lead_agent_id = format_agent_id(TEAM_LEAD_NAME, &final_team_name);
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());
        let team = TeamFile {
            name: final_team_name.clone(),
            description: parsed.description.clone(),
            created_at: now_wall_ms(),
            lead_agent_id: lead_agent_id.clone(),
            lead_session_id: context
                .session_id()
                .map(str::to_string)
                .or_else(|| std::env::var("REBON_SESSION_ID").ok()),
            hidden_pane_ids: Vec::new(),
            members: vec![TeamMember {
                agent_id: lead_agent_id.clone(),
                name: TEAM_LEAD_NAME.into(),
                agent_type: parsed.agent_type.clone(),
                model: None,
                model_profile: None,
                prompt: None,
                color: None,
                plan_mode_required: None,
                joined_at: now_wall_ms(),
                tmux_pane_id: String::new(),
                cwd,
                worktree_path: None,
                backend_type: None,
                is_active: Some(true),
                mode: Some("default".into()),
                subscriptions: Vec::new(),
            }],
        };
        write_team_file(&final_team_name, &team).map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;
        if context.session_id().is_none() {
            set_current_team_name(&final_team_name);
        }

        Ok(json!({
            "team_name": final_team_name,
            "team_file_path": team_file_path(&team.name),
            "lead_agent_id": lead_agent_id,
        }))
    }
}

fn parse_input(input: &Value) -> ToolResult<TeamCreateInput> {
    let tool = ToolId::new(TEAM_CREATE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "TeamCreate input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let team_name = object
        .get("team_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "TeamCreate input requires a non-empty string `team_name`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_string();

    let description = match object.get("description") {
        Some(Value::String(v)) => Some(v.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`description` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    let agent_type = match object.get("agent_type") {
        Some(Value::String(v)) => Some(v.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool,
                reason: "`agent_type` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    Ok(TeamCreateInput {
        team_name,
        description,
        agent_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::read_team_file;
    use rebon_tool::tasks::test_support::TestConfigHome;

    #[tokio::test]
    async fn call_creates_team_file_and_binds_session() {
        let _home = TestConfigHome::new("team-create");
        let tool = TeamCreateTool;
        let context = ToolContext::new().with_session_id("team-create-session");
        let out = tool
            .call(
                json!({
                    "team_name": "alpha squad",
                    "description": "test team",
                    "agent_type": "lead"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["team_name"], json!("alpha-squad"));
        assert_eq!(context.current_team_name().as_deref(), Some("alpha-squad"));
        assert_eq!(
            rebon_tool::team_name_for_session("team-create-session")
                .unwrap()
                .as_deref(),
            Some("alpha-squad")
        );
        assert!(rebon_tool::current_team_name().is_none());
        let team = read_team_file("alpha-squad").unwrap().unwrap();
        assert_eq!(team.members.len(), 1);
        assert_eq!(team.members[0].name, TEAM_LEAD_NAME);
    }
}
