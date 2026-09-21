use std::sync::Arc;

use rebon_api::{
    ContentBlock, ContentBlockDelta, ContentBlockStart, Message, MessageDeltaFields,
    MockModelClient, ModelClient, Role, SessionHandle, StopReason, StreamEvent, Usage,
};
use rebon_command_seat::{CommandSeat, CommandSeatService, COMMAND_SEAT_SERVICE};
use rebon_core::attachment_seat::{
    pollers_for_session, AttachmentSeat, AttachmentSeatService, SessionAttachmentBinding,
    ATTACHMENT_SEAT_SERVICE,
};
use rebon_core::query::{run_query, tools_from_engine, CancelToken, QueryEvent, QueryParams};
use rebon_core::tool_seat::{ToolSeat, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_core::turn_hook::{TurnHookSeat, TurnHookSeatService, TURN_HOOK_SEAT_SERVICE};
use rebon_core::Engine;
use rebon_kernel::{
    DesiredSet, Kernel, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta,
    PluginRegistry,
};
use rebon_plugin_skill::{
    RegistrySkillCatalog, Skill, SkillContext, SkillRegistry, SkillSource, SkillState, SkillTool,
};
use rebon_session_state::ServerState;
use rebon_tool::{ReadTool, ToolContext};

struct SeatPlugin;

impl Plugin for SeatPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("test-seat").provides(&[
            TOOL_SEAT_SERVICE,
            ATTACHMENT_SEAT_SERVICE,
            TURN_HOOK_SEAT_SERVICE,
            COMMAND_SEAT_SERVICE,
        ])
    }

    fn apply(&self, ctx: &rebon_kernel::Context) -> Result<(), KernelError> {
        ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())?;
        ctx.provide::<TurnHookSeatService>(TurnHookSeat::new())?;
        // `/skills` registers on the command seat since B7; a kernel without
        // one would refuse to load the plugin at all.
        ctx.provide::<CommandSeatService>(CommandSeat::new())?;
        ctx.provide::<ToolSeatService>(ToolSeat::new())
    }
}

fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(SeatPlugin))
}

static PLUGINS: &[PluginDef] = &[
    PluginDef {
        id: "test-seat",
        title: "Test seat",
        kind: PluginKind::Core,
        default_enabled: true,
        factory: make_seat,
    },
    rebon_plugin_skill::PLUGIN,
];

fn skill(id: &str, description: &str) -> Skill {
    Skill {
        id: id.into(),
        title: id.into(),
        description: description.into(),
        prompt_template: format!("Run {id}."),
        suggested_tools: Vec::new(),
        source: SkillSource::Project,
        argument_hint: None,
        argument_names: Vec::new(),
        skill_root: None,
        user_invocable: true,
        disable_model_invocation: false,
        required_tools: Vec::new(),
    }
}

fn message_start(id: &str) -> StreamEvent {
    StreamEvent::MessageStart {
        message_id: id.into(),
        model: "mock".into(),
        usage: Usage {
            input_tokens: 4,
            ..Default::default()
        },
    }
}

fn text_turn(id: &str, text: &str) -> Vec<StreamEvent> {
    vec![
        message_start(id),
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::TextDelta { text: text.into() },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage {
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]
}

fn tool_turn(id: &str, tool_name: &str, tool_id: &str, input: &str) -> Vec<StreamEvent> {
    vec![
        message_start(id),
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: tool_id.into(),
                name: tool_name.into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: input.into(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    output_tokens: 6,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]
}

async fn drain(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<QueryEvent>) -> Vec<QueryEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

fn listing_text(message: &Message) -> Option<&str> {
    if message.role != Role::User || message.content.len() != 1 {
        return None;
    }
    match message.content.first() {
        Some(ContentBlock::Text(text))
            if rebon_core::attachments::is_skill_listing_text(&text.text) =>
        {
            Some(&text.text)
        }
        _ => None,
    }
}

fn expected_listing(header: &str, entries: &[(&str, &str)]) -> String {
    let body = entries
        .iter()
        .map(|(id, description)| format!("- {id}: {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("<system-reminder>\n{header}\n\n{body}\n</system-reminder>")
}

fn write_dynamic_skill(
    project: &std::path::Path,
    id: &str,
    description: &str,
) -> std::path::PathBuf {
    let source = project.join("src").join("main.rs");
    std::fs::create_dir_all(source.parent().expect("source parent")).unwrap();
    std::fs::write(&source, "fn main() {}\n").unwrap();
    let skill_dir = project.join(".rebon").join("skills").join(id);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {id}\ndescription: {description}\n---\nRun {id}.\n"),
    )
    .unwrap();
    source
}

fn query_engine() -> Arc<Engine> {
    let mut engine = Engine::new();
    engine.register_tool(Arc::new(ReadTool));
    engine.register_tool(Arc::new(SkillTool));
    Arc::new(engine)
}

#[tokio::test]
async fn full_query_preserves_skill_listing_order_dedup_and_plugin_switching() {
    let temp = tempfile::tempdir().unwrap();
    let kernel = Kernel::new();
    let host = PluginHost {
        kernel: kernel.clone(),
        config_dir: temp.path().join("config"),
    };
    let plugins = PluginRegistry::new(kernel.clone(), PLUGINS, host);
    assert!(plugins.reconcile(&DesiredSet::new()).failed.is_empty());
    let attachment_seat = kernel
        .context()
        .get::<AttachmentSeatService>()
        .expect("attachment seat");
    let hook_seat = kernel
        .context()
        .get::<TurnHookSeatService>()
        .expect("turn hook seat");

    let registry = Arc::new(SkillRegistry::new());
    registry.register(skill("base-skill", "Available at turn start"));
    let source = write_dynamic_skill(
        &temp.path().join("enabled-project"),
        "late-review",
        "Discovered during the tool round",
    );
    let state = Arc::new(std::sync::Mutex::new(SkillState::empty(
        "sess-enabled",
        temp.path().to_string_lossy(),
    )));
    let server_state = Arc::new(ServerState::new());
    let record =
        server_state.create_session(temp.path().to_string_lossy().into_owned(), Vec::new());
    let binding = SessionAttachmentBinding::new(server_state.clone(), record.id.clone())
        .with_skills(Arc::new(RegistrySkillCatalog::new(registry.clone())));
    let pollers = pollers_for_session(kernel.context(), &binding);
    assert_eq!(pollers.len(), 1);
    let poller = pollers[0].clone();

    let engine = query_engine();
    let client = MockModelClient::new();
    let read_input = serde_json::json!({"file_path": source.to_string_lossy()}).to_string();
    client.push_turn(tool_turn(
        "msg_read_first",
        "Read",
        "toolu_read_first",
        &read_input,
    ));
    client.push_turn(tool_turn(
        "msg_load_skill",
        "Skill",
        "toolu_load_skill",
        "{\"skill\":\"late-review\"}",
    ));
    client.push_turn(tool_turn(
        "msg_unknown_skill",
        "Skill",
        "toolu_unknown_skill",
        "{\"skill\":\"missing\"}",
    ));
    client.push_turn(tool_turn(
        "msg_read_repeat",
        "Read",
        "toolu_read_repeat",
        &read_input,
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let captured_client = client.clone();
    let mut params = QueryParams::new("mock", vec![Message::user_text("inspect the file")])
        .with_tools(tools_from_engine(&engine))
        .with_attachment_poller(poller, record.id.clone(), "turn-enabled")
        .with_turn_hook_seat(hook_seat.clone())
        .with_max_iterations(5);
    // One value, two readers: the tool resolves names against the registry
    // and the discovery subscriber registers what it finds in the same one.
    let skill_context = SkillContext::new(registry.clone(), state);
    params.extensions.insert(skill_context.clone());
    let context = ToolContext::new()
        .with_cwd(temp.path().to_string_lossy().into_owned())
        .with_extension(skill_context);

    let mut receiver = run_query(
        engine,
        SessionHandle::new(Arc::new(client) as Arc<dyn ModelClient>),
        params,
        context,
        CancelToken::new(),
    );
    let events = drain(&mut receiver).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ToolDispatchResult {
            tool_use_id,
            outcome: Ok(_),
            ..
        } if tool_use_id == "toolu_load_skill"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ToolDispatchResult {
            tool_use_id,
            outcome: Err(_),
            ..
        } if tool_use_id == "toolu_unknown_skill"
    )));
    assert_eq!(registry.ids(), ["base-skill", "late-review"]);
    let listing_events = events
        .iter()
        .filter_map(|event| match event {
            QueryEvent::AttachmentInjected { message, .. } => listing_text(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        listing_events,
        [expected_listing(
            rebon_core::attachments::SKILL_LISTING_INITIAL_HEADER,
            &[
                ("base-skill", "Available at turn start"),
                ("late-review", "Discovered during the tool round"),
            ],
        )]
    );

    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 5);
    let second_history = &requests[1].messages;
    assert_eq!(
        listing_text(second_history.last().expect("skill listing last")),
        Some(listing_events[0])
    );
    assert!(matches!(
        second_history
            .get(second_history.len().saturating_sub(2))
            .and_then(|message| message.content.first()),
        Some(ContentBlock::ToolResult(result)) if result.tool_use_id == "toolu_read_first"
    ));
    assert_eq!(
        requests[4]
            .messages
            .iter()
            .filter(|message| listing_text(message).is_some())
            .count(),
        1,
        "successful/unknown Skill results and duplicate Read must not re-announce"
    );
    assert_eq!(
        server_state
            .get_session(&record.id)
            .expect("session remains")
            .attachment_state
            .sent_skill_names,
        ["base-skill", "late-review"]
    );

    plugins
        .set_enabled(rebon_plugin_skill::PLUGIN_ID, false)
        .expect("skill plugin is a feature plugin");
    assert!(attachment_seat.provider_ids().is_empty());

    let disabled_registry = Arc::new(SkillRegistry::new());
    let disabled_source = write_dynamic_skill(
        &temp.path().join("disabled-project"),
        "disabled-late",
        "Discovered while plugin disabled",
    );
    let disabled_state = Arc::new(std::sync::Mutex::new(SkillState::empty(
        "sess-disabled",
        temp.path().to_string_lossy(),
    )));
    let disabled_record =
        server_state.create_session(temp.path().to_string_lossy().into_owned(), Vec::new());
    let disabled_binding =
        SessionAttachmentBinding::new(server_state.clone(), disabled_record.id.clone())
            .with_skills(Arc::new(RegistrySkillCatalog::new(
                disabled_registry.clone(),
            )));
    assert!(pollers_for_session(kernel.context(), &disabled_binding).is_empty());

    let disabled_engine = query_engine();
    let disabled_client = MockModelClient::new();
    let disabled_input =
        serde_json::json!({"file_path": disabled_source.to_string_lossy()}).to_string();
    disabled_client.push_turn(tool_turn(
        "msg_disabled_read",
        "Read",
        "toolu_disabled_read",
        &disabled_input,
    ));
    disabled_client.push_turn(text_turn("msg_disabled_done", "done"));
    let disabled_capture = disabled_client.clone();
    let mut disabled_params =
        QueryParams::new("mock", vec![Message::user_text("read while disabled")])
            .with_tools(tools_from_engine(&disabled_engine))
            .with_turn_hook_seat(hook_seat.clone())
            .with_max_iterations(2);
    disabled_params.extensions.insert(SkillContext::new(
        disabled_registry.clone(),
        disabled_state.clone(),
    ));
    let mut disabled_receiver = run_query(
        disabled_engine,
        SessionHandle::new(Arc::new(disabled_client) as Arc<dyn ModelClient>),
        disabled_params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let disabled_events = drain(&mut disabled_receiver).await;
    assert!(matches!(
        disabled_events.last(),
        Some(QueryEvent::Done { .. })
    ));
    // Discovery goes off with the tool. Registering a skill nobody can be
    // told about and nobody can invoke is work with no consumer, so the
    // subscriber leaves the seat when the plugin does.
    assert!(disabled_registry.get("disabled-late").is_none());
    assert!(disabled_events
        .iter()
        .all(|event| !matches!(event, QueryEvent::AttachmentInjected { .. })));
    assert!(disabled_capture.captured_requests().iter().all(|request| {
        request
            .messages
            .iter()
            .all(|message| listing_text(message).is_none())
    }));
    assert!(server_state
        .get_session(&disabled_record.id)
        .expect("disabled session remains")
        .attachment_state
        .sent_skill_names
        .is_empty());

    plugins
        .set_enabled(rebon_plugin_skill::PLUGIN_ID, true)
        .expect("skill plugin can be restored");
    assert_eq!(
        attachment_seat.provider_ids(),
        [rebon_plugin_skill::PLUGIN_ID]
    );
    let restored_poller = pollers_for_session(kernel.context(), &disabled_binding)
        .into_iter()
        .next()
        .expect("restored plugin contributes its poller");
    let restored_engine = query_engine();
    let restored_client = MockModelClient::new();
    // The same read the disabled turn made. Nothing was registered then, so
    // this is the restored subscriber finding the skill for the first time.
    restored_client.push_turn(tool_turn(
        "msg_restored_read",
        "Read",
        "toolu_restored_read",
        &disabled_input,
    ));
    restored_client.push_turn(text_turn("msg_restored_done", "done"));
    let restored_capture = restored_client.clone();
    let mut restored_params =
        QueryParams::new("mock", vec![Message::user_text("continue after enabling")])
            .with_tools(tools_from_engine(&restored_engine))
            .with_attachment_poller(restored_poller, disabled_record.id.clone(), "turn-restored")
            .with_turn_hook_seat(hook_seat)
            .with_max_iterations(2);
    restored_params
        .extensions
        .insert(SkillContext::new(disabled_registry.clone(), disabled_state));
    let mut restored_receiver = run_query(
        restored_engine,
        SessionHandle::new(Arc::new(restored_client) as Arc<dyn ModelClient>),
        restored_params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let restored_events = drain(&mut restored_receiver).await;
    assert!(matches!(
        restored_events.last(),
        Some(QueryEvent::Done { .. })
    ));
    let restored_requests = restored_capture.captured_requests();
    assert_eq!(restored_requests.len(), 2);
    assert!(disabled_registry.get("disabled-late").is_some());
    assert_eq!(
        listing_text(
            restored_requests[1]
                .messages
                .last()
                .expect("restored listing is visible before the model request")
        ),
        Some(
            expected_listing(
                rebon_core::attachments::SKILL_LISTING_INITIAL_HEADER,
                &[("disabled-late", "Discovered while plugin disabled")],
            )
            .as_str()
        )
    );
}
