//! Real-execution regressions for how a skill invocation reaches the model.
//!
//! These ran in `rebon-core`'s query tests while the registry was
//! `rebon-tool`'s and the executor resolved a typed `/name` against it
//! directly. They moved here with the code: the catalogue is this plugin's
//! now, and what the engine has is one handle it asks. Both paths still run a
//! real `EngineQueryExecutor` turn, so what they pin is the request the model
//! receives and the transcript the session keeps.

mod support;

use std::sync::Arc;

use async_trait::async_trait;
use rebon_agent_core::{PromptExecutor, PromptRequest, SkillInvocationRequest};
use rebon_api::{
    ContentBlock as ApiContentBlock, CreateMessageRequest, MockModelClient, ModelClient, Role,
};
use rebon_core::query::{transcript_to_api_messages, CancelToken, EngineQueryExecutor};
use rebon_core::Engine;
use rebon_plugin_skill::{
    RegistrySkillCatalog, Skill, SkillContext, SkillRegistry, SkillSource, SkillTool,
};
use rebon_types::AgentCapabilityMode;

use support::{temp_projects_root, text_turn, ApproveBroker};

/// A mock that admits to the Anchored Minimal follow-up shape, which is the
/// path the explicit-invocation test exercises.
struct AnchoredMockClient {
    inner: MockModelClient,
}

#[async_trait]
impl ModelClient for AnchoredMockClient {
    fn provider_name(&self) -> &'static str {
        "mock-anchored"
    }

    fn supports_anchored_minimal(&self) -> bool {
        true
    }

    fn reset_session_state(&self) {
        self.inner.reset_session_state();
    }

    fn end_turn(&self) {
        self.inner.end_turn();
    }

    fn invalidate_previous_response_id(&self) {
        self.inner.invalidate_previous_response_id();
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
        self.inner.create_message_stream(request).await
    }
}

fn radare2(user_invocable: bool) -> Skill {
    Skill {
        id: "radare2".into(),
        title: "radare2".into(),
        description: "Reverse engineering workflow.".into(),
        prompt_template: "Use radare2. Args: $ARGUMENTS".into(),
        suggested_tools: Vec::new(),
        source: SkillSource::Project,
        argument_hint: None,
        argument_names: Vec::new(),
        skill_root: None,
        user_invocable,
        disable_model_invocation: false,
        required_tools: Vec::new(),
    }
}

/// Build an executor whose registry holds one skill. `user_invocable`
/// controls whether a user may call it directly.
fn typed_skill_executor(
    name: &str,
    user_invocable: bool,
    disabled: bool,
    projects_root: &std::path::Path,
) -> (
    EngineQueryExecutor,
    MockModelClient,
    Arc<rebon_session_state::ServerState>,
    String,
) {
    let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
    engine.register_tool(Arc::new(SkillTool));
    let engine = Arc::new(engine);
    let registry = Arc::new(SkillRegistry::new());
    let mut skill = radare2(user_invocable);
    skill.id = name.into();
    skill.title = name.into();
    registry.register(skill);
    if disabled {
        registry.set_disabled_skills(vec![name.to_string()]);
    }
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_skill", "loaded"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone())
        .with_turn_skill_catalog(Arc::new(RegistrySkillCatalog::new(registry.clone())))
        .with_extension(SkillContext::with_registry(registry));
    (executor, client_handle, state, cwd)
}

fn typed_skill_request(session_id: String, cwd: String, text: &str) -> PromptRequest {
    PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id,
        cwd,
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: text.into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: CancelToken::new(),
        thinking_budget: None,
        max_tokens: None,
        reasoning_effort_ordinal: None,
        additional_working_directories: Vec::new(),
        coordinator_mode: None,
        coordinator_report_paths: Vec::new(),
        user_message_uuid: None,
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests: Vec::new(),
        // Empty on purpose: the desktop app's background worker, `rebon exec`,
        // and ACP clients all send the typed text without resolving it.
        skill_invocations: Vec::new(),
    }
}

/// Typing `/radare2 args` must load the skill even when the caller did not
/// resolve it. Before this, only the interactive TUI resolved skill commands,
/// so the same text sent from the desktop app reached the model as literal
/// characters and the skill never ran.
#[tokio::test]
async fn typed_skill_command_is_resolved_without_an_explicit_invocation() {
    let projects_root_dir = temp_projects_root("typed_skill_invocation");
    let projects_root = projects_root_dir.path();
    let (executor, client_handle, state, cwd) =
        typed_skill_executor("radare2", true, false, projects_root);
    let session = state.create_session(cwd.clone(), Vec::new());

    let outcome = executor
        .execute(typed_skill_request(
            session.id.clone(),
            cwd.clone(),
            "/radare2 analyze main",
        ))
        .await;
    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());

    let request = client_handle
        .captured_requests()
        .into_iter()
        .next()
        .expect("captured request");
    assert_eq!(request.messages.len(), 3);
    assert!(matches!(
        &request.messages[0].content[0],
        ApiContentBlock::ToolUse(tool) if tool.name == "Skill" && tool.input["skill"] == "radare2"
    ));
    assert!(matches!(
        &request.messages[1].content[0],
        ApiContentBlock::ToolResult(result)
            if result.content.as_text().is_some_and(|text| text.contains("Use radare2. Args: analyze main"))
    ));
    assert_eq!(
        request.messages[2].content[0].as_text(),
        Some("/radare2 analyze main")
    );

    let transcript_path = rebon_session::transcript_file_path(projects_root, &cwd, &session.id);
    let persisted = rebon_session::load_transcript_from_file(&transcript_path)
        .unwrap()
        .expect("persisted typed-skill transcript");
    let replayed = transcript_to_api_messages(&persisted.messages);
    assert_eq!(replayed[0].role, Role::User);
    assert_eq!(
        replayed[0].content[0].as_text(),
        Some("/radare2 analyze main")
    );
}

/// A name that is not a usable skill stays ordinary prompt text. Resolving it
/// anyway would hijack prompts that merely open with a slash.
#[tokio::test]
async fn typed_skill_command_is_ignored_when_the_skill_is_not_user_callable() {
    for (skill, disabled, user_invocable, text) in [
        // Registered but model-only.
        ("radare2", false, false, "/radare2 analyze main"),
        // Registered but switched off in /skills.
        ("radare2", true, true, "/radare2 analyze main"),
        // Not registered at all.
        ("radare2", false, true, "/not-a-skill do a thing"),
        // Not a skill token in the first place.
        ("radare2", false, true, "/tmp/file explain this"),
    ] {
        let projects_root_dir = temp_projects_root("typed_skill_ignored");
        let projects_root = projects_root_dir.path();
        let (executor, client_handle, state, cwd) =
            typed_skill_executor(skill, user_invocable, disabled, projects_root);
        let session = state.create_session(cwd.clone(), Vec::new());

        let outcome = executor
            .execute(typed_skill_request(session.id.clone(), cwd, text))
            .await;
        assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());

        let request = client_handle
            .captured_requests()
            .into_iter()
            .next()
            .expect("captured request");
        assert_eq!(
            request.messages.len(),
            1,
            "{text} must reach the model as plain text"
        );
        assert_eq!(request.messages[0].content[0].as_text(), Some(text));
    }
}

/// An invocation the caller resolved itself runs before the model request and
/// is written to the transcript as a real tool call, so a resume replays it.
#[tokio::test]
async fn explicit_skill_invocation_loads_before_model_request() {
    let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
    engine.register_tool(Arc::new(SkillTool));
    let engine = Arc::new(engine);
    let registry = Arc::new(SkillRegistry::new());
    registry.register(radare2(true));
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_skill", "loaded"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(AnchoredMockClient { inner: client });
    let projects_root_dir = temp_projects_root("explicit_skill_invocation");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone())
        .with_turn_skill_catalog(Arc::new(RegistrySkillCatalog::new(registry.clone())))
        .with_extension(SkillContext::with_registry(registry))
        .with_capability_mode(AgentCapabilityMode::Minimal)
        .with_max_tokens(4096);

    let mut request = typed_skill_request(session.id.clone(), cwd.clone(), "/radare2 analyze main");
    request.skill_invocations = vec![SkillInvocationRequest {
        skill: "radare2".into(),
        args: Some("analyze main".into()),
    }];
    let outcome = executor.execute(request).await;
    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());

    let request = client_handle
        .captured_requests()
        .into_iter()
        .next()
        .expect("captured request");
    assert_eq!(request.max_tokens, 4096);
    assert_eq!(request.messages.len(), 3);
    assert!(matches!(
        &request.messages[0].content[0],
        ApiContentBlock::ToolUse(tool) if tool.name == "Skill" && tool.input["skill"] == "radare2"
    ));
    assert!(matches!(
        &request.messages[1].content[0],
        ApiContentBlock::ToolResult(result) if result.content.as_text().is_some_and(|text| text.contains("Use radare2. Args: analyze main"))
    ));
    assert_eq!(
        request.messages[2].content[0].as_text(),
        Some("/radare2 analyze main")
    );

    let transcript_path = rebon_session::transcript_file_path(projects_root, &cwd, &session.id);
    let persisted = rebon_session::load_transcript_from_file(&transcript_path)
        .unwrap()
        .expect("persisted skill transcript");
    let replayed = transcript_to_api_messages(&persisted.messages);
    assert_eq!(replayed.len(), 4);
    assert_eq!(replayed[0].role, Role::User);
    assert_eq!(
        replayed[0].content[0].as_text(),
        Some("/radare2 analyze main")
    );
    assert!(matches!(
        &replayed[1].content[0],
        ApiContentBlock::ToolUse(tool) if tool.name == "Skill"
    ));
    assert!(matches!(
        &replayed[2].content[0],
        ApiContentBlock::ToolResult(_)
    ));
}
