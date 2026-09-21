#[cfg(any(unix, windows))]
use crate::runtime::{
    Action, ActionResponse, Client, KeyModifier, MouseButton, Request, ServiceState,
    ACTIVE_PATH_ENV, AUTH_TOKEN_ENV, SOCKET_PATH_ENV,
};
use async_trait::async_trait;
use rebon_api::{ImageBlock, TextBlock, ToolResultContent, ToolResultContentBlock};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, PermissionDecision, PermissionRequest, ToolError, ToolId,
    ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const COMPUTER_USE_TOOL_NAME: &str = "ComputerUse";

/// The one capability cell every `ComputerUse` registration reads.
///
/// Whether the active provider may be handed the local desktop is decided
/// where the provider is resolved — the step that resolved it derives and
/// publishes it through [`set_provider_capability`]. The tool is registered
/// on the *process* tool seat, which has no session of its own to ask, so
/// writer and
/// reader meet at this cell instead of at a handle passed in at construction.
///
/// Process-wide is the same reach the tool has always had: a sub-agent shares
/// its parent's engine and therefore already shared one cell. What it does
/// change is a process running sessions on two different providers at
/// once — the last session built wins. That is worth knowing but not worth
/// guarding here: every action except `observe` still asks the user, and the
/// tool is invisible unless the local Computer Use service is running.
pub(crate) fn provider_capability_cell() -> Arc<AtomicBool> {
    static CELL: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

/// Publish whether the provider this session resolved to may be handed the
/// local desktop.
///
/// The one writer is the step that resolved the provider. The cell
/// outlives the plugin's own enable/disable cycle on purpose: disabling the
/// plugin takes the tool off the seat, and re-enabling it must find the same
/// verdict the provider resolution left behind rather than a stale `false`.
pub fn set_provider_capability(enabled: bool) {
    provider_capability_cell().store(enabled, Ordering::Release);
}

const INVALID_INPUT_CODE: i64 = 400;
const DESCRIPTION: &str = "Controls the user-selected desktop window and returns its latest screenshot after every action. Use observe to inspect the window, then use screenshot-pixel coordinates for click, double_click, move, and scroll. Actions are serial and remain confined to the locked target.";

#[derive(Clone)]
pub struct ComputerUseTool {
    provider_enabled: Arc<AtomicBool>,
}

impl Default for ComputerUseTool {
    fn default() -> Self {
        Self::new(false)
    }
}

impl ComputerUseTool {
    pub fn new(provider_enabled: bool) -> Self {
        Self {
            provider_enabled: Arc::new(AtomicBool::new(provider_enabled)),
        }
    }

    pub fn with_provider_capability(provider_enabled: Arc<AtomicBool>) -> Self {
        Self { provider_enabled }
    }

    pub fn set_provider_enabled(&self, enabled: bool) {
        self.provider_enabled.store(enabled, Ordering::Release);
    }

    fn provider_enabled(&self) -> bool {
        self.provider_enabled.load(Ordering::Acquire)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
// Builds without a native runtime stop at the unsupported stub, so the payload
// fields are parsed and validated but never forwarded to a backend.
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
enum ComputerUseInput {
    Observe,
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        button: MouseButtonInput,
    },
    DoubleClick {
        x: f64,
        y: f64,
        #[serde(default)]
        button: MouseButtonInput,
    },
    Move {
        x: f64,
        y: f64,
    },
    Scroll {
        x: f64,
        y: f64,
        delta_x: i32,
        delta_y: i32,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
        #[serde(default)]
        modifiers: Vec<KeyModifierInput>,
    },
    Wait {
        duration_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MouseButtonInput {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
enum KeyModifierInput {
    Command,
    Control,
    Option,
    Shift,
    Function,
}

impl ComputerUseInput {
    fn action_name(&self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Click { .. } => "click",
            Self::DoubleClick { .. } => "double_click",
            Self::Move { .. } => "move",
            Self::Scroll { .. } => "scroll",
            Self::Type { .. } => "type",
            Self::Key { .. } => "key",
            Self::Wait { .. } => "wait",
        }
    }

    fn is_observe(&self) -> bool {
        matches!(self, Self::Observe)
    }

    fn validate_semantics(&self) -> Result<(), String> {
        let valid_coordinates =
            |x: f64, y: f64| x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0;
        match self {
            Self::Click { x, y, .. }
            | Self::DoubleClick { x, y, .. }
            | Self::Move { x, y }
            | Self::Scroll { x, y, .. }
                if !valid_coordinates(*x, *y) =>
            {
                Err("x and y must be finite non-negative coordinates".into())
            }
            Self::Type { text } if text.chars().count() > 16_384 => {
                Err("text must contain at most 16384 characters".into())
            }
            Self::Key { key, modifiers }
                if key.is_empty()
                    || key.len() > 64
                    || modifiers.len() > 5
                    || modifiers
                        .iter()
                        .collect::<std::collections::HashSet<_>>()
                        .len()
                        != modifiers.len() =>
            {
                Err("key must be non-empty and modifiers must be unique".into())
            }
            Self::Wait { duration_ms } if *duration_ms > 60_000 => {
                Err("duration_ms must not exceed 60000".into())
            }
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl Tool for ComputerUseTool {
    fn id(&self) -> ToolId {
        ToolId::new(COMPUTER_USE_TOOL_NAME)
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> ToolInputSchema {
        let coordinate = json!({ "type": "number", "minimum": 0 });
        let button = json!({ "type": "string", "enum": ["left", "right", "middle"] });
        let modifiers = json!({
            "type": "array",
            "items": { "type": "string", "enum": ["command", "control", "option", "shift", "function"] },
            "maxItems": 5,
            "uniqueItems": true
        });
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": { "action": { "const": "observe" } },
                    "required": ["action"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "click" },
                        "x": coordinate,
                        "y": coordinate,
                        "button": button
                    },
                    "required": ["action", "x", "y"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "double_click" },
                        "x": coordinate,
                        "y": coordinate,
                        "button": button
                    },
                    "required": ["action", "x", "y"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "move" },
                        "x": coordinate,
                        "y": coordinate
                    },
                    "required": ["action", "x", "y"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "scroll" },
                        "x": coordinate,
                        "y": coordinate,
                        "delta_x": { "type": "integer" },
                        "delta_y": { "type": "integer" }
                    },
                    "required": ["action", "x", "y", "delta_x", "delta_y"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "type" },
                        "text": { "type": "string", "maxLength": 16384 }
                    },
                    "required": ["action", "text"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "key" },
                        "key": { "type": "string", "minLength": 1, "maxLength": 64 },
                        "modifiers": modifiers
                    },
                    "required": ["action", "key"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "wait" },
                        "duration_ms": { "type": "integer", "minimum": 0, "maximum": 60000 }
                    },
                    "required": ["action", "duration_ms"],
                    "additionalProperties": false
                }
            ]
        })
    }

    fn is_enabled(&self) -> bool {
        self.provider_enabled() && local_service_available()
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn is_read_only(&self, input: &Value) -> bool {
        parse_input(input).is_ok_and(|input| input.is_observe())
    }

    fn needs_permission(&self, input: &Value) -> bool {
        // Fail closed. Only a well-formed `observe` — the one action that cannot
        // touch the target — may skip the prompt; anything unparseable is
        // treated as if it were about to click.
        !parse_input(input).is_ok_and(|input| input.is_observe())
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
        let parsed = parse_input(input)?;
        if parsed.is_observe() {
            return Ok(PermissionDecision::allow(input.clone()));
        }
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Control desktop window",
                format!(
                    "ComputerUse wants to perform the {} action in the selected desktop window.",
                    parsed.action_name()
                ),
            )
            .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        if !self.provider_enabled() {
            return Err(execution_error(
                "the active provider does not support ComputerUse",
            ));
        }
        let parsed = parse_input(&input)?;
        execute(parsed).await
    }

    /// A capture is worth nothing to the model as escaped JSON: the base64
    /// PNG would arrive as text and cost roughly what the image costs while
    /// saying nothing. Hand it over as a summary line plus a real image
    /// block instead.
    ///
    /// The leading text is load-bearing beyond being readable. It starts with
    /// [`rebon_core::LIVE_CAPTURE_RESULT_TEXT_PREFIX`], which is how the
    /// engine's history finds this tool's own captures when it drops the ones
    /// a newer frame superseded — history carries no tool names, so the
    /// marker has to travel in the content.
    ///
    /// Anything without a base64 screenshot falls back to `None` and takes
    /// the caller's generic projection.
    fn project_result_for_model(&self, value: &Value) -> Option<ToolResultContent> {
        let action = value
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let screenshot = value.get("screenshot").and_then(Value::as_object)?;
        let data = screenshot.get("base64").and_then(Value::as_str)?;
        let media_type = screenshot
            .get("mediaType")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        let width = screenshot.get("width").and_then(Value::as_u64).unwrap_or(0);
        let height = screenshot
            .get("height")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let prefix = rebon_core::LIVE_CAPTURE_RESULT_TEXT_PREFIX;
        Some(ToolResultContent::blocks(vec![
            ToolResultContentBlock::Text(TextBlock {
                text: format!("{prefix}{action} completed; screenshot {width}x{height}."),
            }),
            ToolResultContentBlock::Image(ImageBlock::base64(media_type, data)),
        ]))
    }
}

fn parse_input(input: &Value) -> ToolResult<ComputerUseInput> {
    reject_unknown_fields(input)?;
    let parsed: ComputerUseInput =
        serde_json::from_value(input.clone()).map_err(|error| invalid_input(error.to_string()))?;
    parsed.validate_semantics().map_err(invalid_input)?;
    Ok(parsed)
}

fn reject_unknown_fields(input: &Value) -> ToolResult<()> {
    let Some(object) = input.as_object() else {
        return Ok(());
    };
    let allowed: &[&str] = match object.get("action").and_then(Value::as_str) {
        Some("observe") => &["action"],
        Some("click") | Some("double_click") => &["action", "x", "y", "button"],
        Some("move") => &["action", "x", "y"],
        Some("scroll") => &["action", "x", "y", "delta_x", "delta_y"],
        Some("type") => &["action", "text"],
        Some("key") => &["action", "key", "modifiers"],
        Some("wait") => &["action", "duration_ms"],
        _ => return Ok(()),
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(invalid_input(format!("unknown field `{field}`")));
    }
    Ok(())
}

fn invalid_input(reason: impl Into<String>) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(COMPUTER_USE_TOOL_NAME),
        reason: format!("invalid ComputerUse input: {}", reason.into()),
        error_code: Some(INVALID_INPUT_CODE),
    }
}

fn execution_error(message: impl Into<String>) -> ToolError {
    ToolError::Execution {
        tool: ToolId::new(COMPUTER_USE_TOOL_NAME),
        source: anyhow::anyhow!(message.into()),
    }
}

/// Where the desktop runtime lives, resolved from the environment first and
/// the on-disk endpoint record second. The environment only reaches the app's
/// own descendants; background-job workers are spawned by a supervisor daemon
/// that frequently predates the app, so they discover the endpoint through
/// the record the app persists alongside it.
#[cfg(any(unix, windows))]
struct EndpointConfig {
    socket_path: std::path::PathBuf,
    token: String,
    active_path: std::path::PathBuf,
}

#[cfg(any(unix, windows))]
fn endpoint_config() -> Option<EndpointConfig> {
    let from_env = (|| {
        let non_empty = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        Some(EndpointConfig {
            socket_path: non_empty(SOCKET_PATH_ENV)?.into(),
            token: non_empty(AUTH_TOKEN_ENV)?,
            active_path: non_empty(ACTIVE_PATH_ENV)?.into(),
        })
    })();
    from_env.or_else(|| {
        let record = crate::runtime::load_endpoint_record()?;
        Some(EndpointConfig {
            socket_path: record.socket_path,
            token: record.token,
            active_path: record.active_path,
        })
    })
}

#[cfg(unix)]
fn local_service_available() -> bool {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let Some(config) = endpoint_config() else {
        return false;
    };
    let socket_ready = std::fs::symlink_metadata(&config.socket_path)
        .is_ok_and(|metadata| metadata.file_type().is_socket());
    let target_active = std::fs::symlink_metadata(&config.active_path).is_ok_and(|metadata| {
        metadata.file_type().is_file() && metadata.permissions().mode() & 0o777 == 0o600
    });
    socket_ready && target_active
}

#[cfg(windows)]
fn local_service_available() -> bool {
    let Some(config) = endpoint_config() else {
        return false;
    };
    // `endpoint_ready` probes for a listening named-pipe instance without
    // connecting to (and thereby consuming) one.
    let pipe_ready = crate::runtime::endpoint_ready(&config.socket_path);
    let target_active = std::fs::symlink_metadata(&config.active_path)
        .is_ok_and(|metadata| metadata.file_type().is_file());
    pipe_ready && target_active
}

#[cfg(not(any(unix, windows)))]
fn local_service_available() -> bool {
    false
}

#[cfg(any(unix, windows))]
async fn execute(input: ComputerUseInput) -> ToolResult<Value> {
    let config = endpoint_config()
        .ok_or_else(|| execution_error("the ComputerUse desktop endpoint is not published"))?;
    if !local_service_available() {
        return Err(execution_error(
            "ComputerUse desktop service is unavailable or has no locked target",
        ));
    }

    let client = Client::new(config.socket_path, config.token);
    let status = client
        .request(Request::Status)
        .await
        .map_err(|error| execution_error(error.to_string()))?;
    require_active(&status.status.state)?;
    let target_epoch = status.status.target_epoch;

    let action_name = input.action_name();
    let action = match input {
        ComputerUseInput::Observe => Action::Observe { target: None },
        ComputerUseInput::Click { x, y, button } => Action::Click {
            x,
            y,
            button: button.into(),
        },
        ComputerUseInput::DoubleClick { x, y, button } => Action::DoubleClick {
            x,
            y,
            button: button.into(),
        },
        ComputerUseInput::Move { x, y } => Action::Move { x, y },
        ComputerUseInput::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => Action::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        },
        ComputerUseInput::Type { text } => Action::Type { text },
        ComputerUseInput::Key { key, modifiers } => Action::Key {
            key,
            modifiers: modifiers.into_iter().map(Into::into).collect(),
        },
        ComputerUseInput::Wait { duration_ms } => Action::Wait { duration_ms },
    };
    let response = client
        .request(Request::Action {
            action,
            target_epoch: Some(target_epoch),
        })
        .await
        .map_err(|error| execution_error(error.to_string()))?;
    require_active(&response.status.state)?;
    let response_epoch = response.status.target_epoch;
    let response = if response.screenshot.is_some() {
        response
    } else {
        client
            .request(Request::Action {
                action: Action::Observe { target: None },
                target_epoch: Some(response_epoch),
            })
            .await
            .map_err(|error| execution_error(error.to_string()))?
    };
    format_result(action_name, response)
}

#[cfg(not(any(unix, windows)))]
async fn execute(_input: ComputerUseInput) -> ToolResult<Value> {
    Err(execution_error(
        "ComputerUse desktop service is unavailable on this platform",
    ))
}

#[cfg(any(unix, windows))]
fn require_active(state: &ServiceState) -> ToolResult<()> {
    if *state == ServiceState::Active {
        Ok(())
    } else {
        Err(execution_error(format!(
            "ComputerUse desktop service is not active (state: {state:?})"
        )))
    }
}

#[cfg(any(unix, windows))]
fn format_result(action: &str, response: ActionResponse) -> ToolResult<Value> {
    require_active(&response.status.state)?;
    let target = response
        .status
        .target
        .ok_or_else(|| execution_error("ComputerUse response did not include the locked target"))?;
    let screenshot = response.screenshot.ok_or_else(|| {
        execution_error("ComputerUse action response did not include a screenshot")
    })?;
    Ok(json!({
        "type": "computer_use",
        "action": action,
        "status": "active",
        "target": {
            "app": target.owner_name,
            "title": target.title
        },
        "screenshot": {
            "base64": screenshot.png_base64,
            "mediaType": "image/png",
            "width": screenshot.width,
            "height": screenshot.height,
            "scale": screenshot.scale
        }
    }))
}

#[cfg(any(unix, windows))]
impl From<MouseButtonInput> for MouseButton {
    fn from(value: MouseButtonInput) -> Self {
        match value {
            MouseButtonInput::Left => Self::Left,
            MouseButtonInput::Right => Self::Right,
            MouseButtonInput::Middle => Self::Middle,
        }
    }
}

#[cfg(any(unix, windows))]
impl From<KeyModifierInput> for KeyModifier {
    fn from(value: KeyModifierInput) -> Self {
        match value {
            KeyModifierInput::Command => Self::Command,
            KeyModifierInput::Control => Self::Control,
            KeyModifierInput::Option => Self::Option,
            KeyModifierInput::Shift => Self::Shift,
            KeyModifierInput::Function => Self::Function,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tools_core::PermissionBehavior;

    #[tokio::test]
    async fn schema_and_validation_cover_all_actions_and_reject_unknown_fields() {
        let tool = ComputerUseTool::new(true);
        let context = ToolContext::default();
        let valid = [
            json!({"action":"observe"}),
            json!({"action":"click","x":1,"y":2}),
            json!({"action":"double_click","x":1,"y":2,"button":"right"}),
            json!({"action":"move","x":1,"y":2}),
            json!({"action":"scroll","x":1,"y":2,"delta_x":0,"delta_y":-4}),
            json!({"action":"type","text":"hello"}),
            json!({"action":"key","key":"a","modifiers":["command"]}),
            json!({"action":"wait","duration_ms":10}),
        ];
        for input in valid {
            assert!(
                tool.validate_input(&input, &context)
                    .await
                    .unwrap()
                    .is_valid(),
                "{input}"
            );
        }
        for input in [
            json!({"action":"observe","target":{"x":1,"y":2}}),
            json!({"action":"click","x":1,"y":2,"window_id":7}),
            json!({"action":"scroll","delta_x":0,"delta_y":1}),
            json!({"action":"unknown"}),
        ] {
            assert!(
                !tool
                    .validate_input(&input, &context)
                    .await
                    .unwrap()
                    .is_valid(),
                "{input}"
            );
        }

        let schema = tool.input_schema();
        let encoded = serde_json::to_string(&schema).unwrap();
        assert!(!encoded.contains("target"));
        assert!(!encoded.contains("window_id"));
    }

    #[tokio::test]
    async fn observe_is_read_only_and_mutating_actions_ask_permission() {
        let tool = ComputerUseTool::new(true);
        let context = ToolContext::default();
        let observe = json!({"action":"observe"});
        assert!(tool.is_read_only(&observe));
        assert!(!tool.needs_permission(&observe));
        assert_eq!(
            tool.check_permissions(&observe, &context)
                .await
                .unwrap()
                .behavior,
            PermissionBehavior::Allow
        );

        let click = json!({"action":"click","x":1,"y":2});
        assert!(!tool.is_read_only(&click));
        assert!(tool.needs_permission(&click));

        // Unparseable input must not slip past the prompt by failing the
        // "is this an observe?" test.
        let malformed = json!({"action":"click","x":"nope","y":2});
        assert!(!tool.is_read_only(&malformed));
        assert!(tool.needs_permission(&malformed));
        assert_eq!(
            tool.check_permissions(&click, &context)
                .await
                .unwrap()
                .behavior,
            PermissionBehavior::Ask
        );
    }

    /// The projection the engine used to special-case by name. Two things
    /// are load-bearing and both are asserted: the image arrives as an image
    /// block rather than escaped base64, and the summary line starts with the
    /// marker the engine's history uses to drop superseded captures.
    #[test]
    fn a_capture_projects_as_the_marked_summary_line_plus_an_image() {
        let tool = ComputerUseTool::new(true);
        let value = json!({
            "type": "computer_use",
            "action": "click",
            "status": "active",
            "screenshot": {
                "base64": "cG5n",
                "mediaType": "image/png",
                "width": 800,
                "height": 600,
                "scale": 2
            }
        });

        let Some(ToolResultContent::Blocks(blocks)) = tool.project_result_for_model(&value) else {
            panic!("a capture must project as multimodal blocks");
        };
        assert_eq!(blocks.len(), 2);
        let ToolResultContentBlock::Text(text) = &blocks[0] else {
            panic!("the summary line comes first");
        };
        assert!(
            text.text
                .starts_with(rebon_core::LIVE_CAPTURE_RESULT_TEXT_PREFIX),
            "{}",
            text.text
        );
        assert!(text.text.contains("click") && text.text.contains("800x600"));
        assert!(matches!(
            &blocks[1],
            ToolResultContentBlock::Image(image)
                if image.source.media_type == "image/png" && image.source.data == "cG5n"
        ));
    }

    /// A result with no capture in it declines, so the caller's generic
    /// projection stays in charge — an error payload must not be dressed up
    /// as a screenshot summary.
    #[test]
    fn a_result_without_a_screenshot_declines_the_projection() {
        let tool = ComputerUseTool::new(true);
        assert!(tool
            .project_result_for_model(&json!({"type": "computer_use", "action": "wait"}))
            .is_none());
        assert!(tool
            .project_result_for_model(&json!({
                "type": "computer_use",
                "action": "click",
                "screenshot": {"mediaType": "image/png"}
            }))
            .is_none());
    }

    /// Points endpoint-record fallback at an isolated config home so the
    /// developer machine's real `~/.rebon` record can never leak into a test.
    #[cfg(any(unix, windows))]
    struct ConfigHomeGuard {
        previous: Option<std::ffi::OsString>,
        _home: tempfile::TempDir,
    }

    #[cfg(any(unix, windows))]
    impl ConfigHomeGuard {
        fn isolated() -> Self {
            let home = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("REBON_CONFIG_HOME");
            std::env::set_var("REBON_CONFIG_HOME", home.path());
            Self {
                previous,
                _home: home,
            }
        }
    }

    #[cfg(any(unix, windows))]
    impl Drop for ConfigHomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("REBON_CONFIG_HOME", value),
                None => std::env::remove_var("REBON_CONFIG_HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn disabled_without_env_or_socket_and_requires_provider_capability() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::net::UnixListener;

        let _lock = crate::env_test_lock();
        let _config_home = ConfigHomeGuard::isolated();
        let old_path = std::env::var_os(SOCKET_PATH_ENV);
        let old_token = std::env::var_os(AUTH_TOKEN_ENV);
        let old_active = std::env::var_os(ACTIVE_PATH_ENV);
        std::env::remove_var(SOCKET_PATH_ENV);
        std::env::remove_var(AUTH_TOKEN_ENV);
        std::env::remove_var(ACTIVE_PATH_ENV);

        let tool = ComputerUseTool::new(true);
        assert!(!tool.is_enabled());

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("computer-use.sock");
        let active = dir.path().join("active");
        std::env::set_var(SOCKET_PATH_ENV, &socket);
        std::env::set_var(AUTH_TOKEN_ENV, "token");
        std::env::set_var(ACTIVE_PATH_ENV, &active);
        assert!(!tool.is_enabled());
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(!tool.is_enabled());
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&active)
            .unwrap();
        assert!(tool.is_enabled());

        tool.set_provider_enabled(false);
        assert!(!tool.is_enabled());

        match old_path {
            Some(value) => std::env::set_var(SOCKET_PATH_ENV, value),
            None => std::env::remove_var(SOCKET_PATH_ENV),
        }
        match old_token {
            Some(value) => std::env::set_var(AUTH_TOKEN_ENV, value),
            None => std::env::remove_var(AUTH_TOKEN_ENV),
        }
        match old_active {
            Some(value) => std::env::set_var(ACTIVE_PATH_ENV, value),
            None => std::env::remove_var(ACTIVE_PATH_ENV),
        }
    }

    #[cfg(windows)]
    #[test]
    fn disabled_without_env_or_pipe_and_requires_provider_capability() {
        let _lock = crate::env_test_lock();
        let _config_home = ConfigHomeGuard::isolated();
        let old_path = std::env::var_os(SOCKET_PATH_ENV);
        let old_token = std::env::var_os(AUTH_TOKEN_ENV);
        let old_active = std::env::var_os(ACTIVE_PATH_ENV);
        std::env::remove_var(SOCKET_PATH_ENV);
        std::env::remove_var(AUTH_TOKEN_ENV);
        std::env::remove_var(ACTIVE_PATH_ENV);

        let tool = ComputerUseTool::new(true);
        assert!(!tool.is_enabled());

        let dir = tempfile::tempdir().unwrap();
        let pipe = format!(r"\\.\pipe\rebon-cu-tool-test-{}", std::process::id());
        let active = dir.path().join("active");
        std::env::set_var(SOCKET_PATH_ENV, &pipe);
        std::env::set_var(AUTH_TOKEN_ENV, "token");
        std::env::set_var(ACTIVE_PATH_ENV, &active);
        assert!(!tool.is_enabled());

        // Named-pipe handles need a live Tokio reactor.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _enter = runtime.enter();
        let _server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&pipe)
            .unwrap();
        assert!(!tool.is_enabled());
        std::fs::write(&active, b"").unwrap();
        assert!(tool.is_enabled());

        tool.set_provider_enabled(false);
        assert!(!tool.is_enabled());

        // Workers spawned outside the app's env tree (supervisor-daemon
        // children) discover the endpoint through the persisted record.
        tool.set_provider_enabled(true);
        std::env::remove_var(SOCKET_PATH_ENV);
        std::env::remove_var(AUTH_TOKEN_ENV);
        std::env::remove_var(ACTIVE_PATH_ENV);
        assert!(!tool.is_enabled());
        crate::runtime::store_endpoint_record(&crate::runtime::EndpointRecord {
            socket_path: std::path::PathBuf::from(&pipe),
            token: "token".into(),
            active_path: active.clone(),
        })
        .unwrap();
        assert!(tool.is_enabled());
        crate::runtime::clear_endpoint_record();
        assert!(!tool.is_enabled());

        match old_path {
            Some(value) => std::env::set_var(SOCKET_PATH_ENV, value),
            None => std::env::remove_var(SOCKET_PATH_ENV),
        }
        match old_token {
            Some(value) => std::env::set_var(AUTH_TOKEN_ENV, value),
            None => std::env::remove_var(AUTH_TOKEN_ENV),
        }
        match old_active {
            Some(value) => std::env::set_var(ACTIVE_PATH_ENV, value),
            None => std::env::remove_var(ACTIVE_PATH_ENV),
        }
    }
}
