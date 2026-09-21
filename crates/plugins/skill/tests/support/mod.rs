//! Shared fixtures for this crate's real-execution regressions.
//!
//! Everything here is built against the engine's *public* API, because these
//! tests live beside the plugin rather than inside the engine: a mock stream,
//! a tool that records what it was called with, an engine that approves every
//! permission, and a kernel with this plugin loaded on the three seats it
//! registers onto.

#![allow(dead_code)] // Shared by several test binaries; each uses a subset.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rebon_api::{
    ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StopReason, StreamEvent, Usage,
};
use rebon_command_seat::{CommandSeat, CommandSeatService, COMMAND_SEAT_SERVICE};
use rebon_core::attachment_seat::{AttachmentSeat, AttachmentSeatService, ATTACHMENT_SEAT_SERVICE};
use rebon_core::query::QueryEvent;
use rebon_core::tool_seat::{ToolSeat, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_core::turn_hook::{TurnHookSeat, TurnHookSeatService, TURN_HOOK_SEAT_SERVICE};
use rebon_core::Engine;
use rebon_kernel::{
    Context, DesiredSet, Kernel, KernelError, Plugin, PluginDef, PluginHost, PluginKind,
    PluginMeta, PluginRegistry,
};
use rebon_tool::{PermissionBroker, Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// A kernel with this plugin on it
// ---------------------------------------------------------------------------

/// Stands in for `core-tools` and `core-commands`, which cannot be
/// depended on from here. All this plugin needs is the three engine seats plus
/// the command seat `/skills` registers on.
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

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())?;
        ctx.provide::<TurnHookSeatService>(TurnHookSeat::new())?;
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

/// A loaded kernel plus the hook seat this plugin subscribed its
/// progressive-discovery subscriber onto. Holding the registry keeps the
/// plugin loaded for the caller's lifetime.
pub struct LoadedSkillPlugin {
    pub kernel: Arc<Kernel>,
    pub registry: Arc<PluginRegistry>,
    pub hook_seat: Arc<TurnHookSeat>,
}

pub fn load_skill_plugin(config_dir: std::path::PathBuf) -> LoadedSkillPlugin {
    let kernel = Kernel::new();
    let host = PluginHost {
        kernel: kernel.clone(),
        config_dir,
    };
    let registry = PluginRegistry::new(kernel.clone(), PLUGINS, host);
    let report = registry.reconcile(&DesiredSet::new());
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let hook_seat = kernel
        .context()
        .get::<TurnHookSeatService>()
        .expect("the hook seat is on the root");
    LoadedSkillPlugin {
        kernel,
        registry,
        hook_seat,
    }
}

// ---------------------------------------------------------------------------
// Tools and engines
// ---------------------------------------------------------------------------

/// A tool that answers with a fixed value and remembers its inputs, or fails.
pub struct RecordingTool {
    name: String,
    calls: Mutex<Vec<Value>>,
    response: Value,
    fail: bool,
}

impl RecordingTool {
    pub fn new(name: &str, response: Value) -> Self {
        Self {
            name: name.into(),
            calls: Mutex::new(Vec::new()),
            response,
            fail: false,
        }
    }

    pub fn failing(name: &str) -> Self {
        Self {
            name: name.into(),
            calls: Mutex::new(Vec::new()),
            response: Value::Null,
            fail: true,
        }
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.name.clone())
    }

    fn description(&self) -> &str {
        "recording tool"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({ "type": "object", "additionalProperties": true })
    }

    async fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        Ok(ValidationOutcome::valid())
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(PermissionDecision::allow(Value::Null))
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        self.calls
            .lock()
            .expect("recorded calls poisoned")
            .push(input);
        if self.fail {
            Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("tool blew up"),
            })
        } else {
            Ok(self.response.clone())
        }
    }
}

/// Approves whatever it is handed. Discovery reacts to a *completed* tool
/// round, so every one of these turns has to complete.
pub struct ApproveBroker;

#[async_trait]
impl PermissionBroker for ApproveBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        _decision: PermissionDecision,
    ) -> ToolResult<Value> {
        tool.call(input, context).await
    }
}

pub fn build_engine_with(tool: Arc<dyn Tool>) -> Arc<Engine> {
    build_engine_with_tools(vec![tool])
}

pub fn build_engine_with_tools(tools: Vec<Arc<dyn Tool>>) -> Arc<Engine> {
    let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
    for tool in tools {
        engine.register_tool(tool);
    }
    Arc::new(engine)
}

// ---------------------------------------------------------------------------
// Mock model stream shapes
// ---------------------------------------------------------------------------

pub fn message_start(id: &str) -> StreamEvent {
    StreamEvent::MessageStart {
        message_id: id.into(),
        model: "mock".into(),
        usage: Usage {
            input_tokens: 4,
            ..Default::default()
        },
    }
}

pub fn text_turn_with_stop(id: &str, text: &str, stop_reason: StopReason) -> Vec<StreamEvent> {
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
                stop_reason: Some(stop_reason),
                usage: Usage {
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]
}

pub fn text_turn(id: &str, text: &str) -> Vec<StreamEvent> {
    text_turn_with_stop(id, text, StopReason::EndTurn)
}

pub fn tool_turn(id: &str, tool_name: &str, tool_id: &str, input_json: &str) -> Vec<StreamEvent> {
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
                partial_json: input_json.into(),
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

pub fn two_tool_turn(
    id: &str,
    first: (&str, &str, &str),
    second: (&str, &str, &str),
) -> Vec<StreamEvent> {
    let mut events = vec![message_start(id)];
    for (index, (tool_name, tool_id, input_json)) in [first, second].into_iter().enumerate() {
        events.push(StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ToolUse {
                id: tool_id.into(),
                name: tool_name.into(),
            },
        });
        events.push(StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: input_json.into(),
            },
        });
        events.push(StreamEvent::ContentBlockStop { index });
    }
    events.extend([
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    output_tokens: 12,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]);
    events
}

pub async fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<QueryEvent>) -> Vec<QueryEvent> {
    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        let terminal = event.is_terminal();
        out.push(event);
        if terminal {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fixtures on disk
// ---------------------------------------------------------------------------

/// A project holding one discoverable skill plus a source file under it. The
/// returned path is the file a tool call touches to bring the skill into view.
pub fn write_progressive_test_skill(
    project: &std::path::Path,
    name: &str,
    description: &str,
) -> std::path::PathBuf {
    let source = project.join("src").join("main.rs");
    std::fs::create_dir_all(source.parent().expect("source parent")).unwrap();
    std::fs::write(&source, "fn main() {}\n").unwrap();
    let skill_dir = project.join(".rebon").join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nRun {name}.\n"),
    )
    .unwrap();
    source
}

pub fn temp_projects_root(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rebon-plugin-skill-{tag}-"))
        .tempdir()
        .unwrap()
}
