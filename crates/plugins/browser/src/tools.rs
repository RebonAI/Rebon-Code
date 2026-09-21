use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde_json::{json, Map, Value};
use url::Url;

pub fn tool_definitions() -> Value {
    json!({
        "tools": [
            tool(
                "status",
                "Return the local bridge, pairing, extension, and controlled-tab status. Use this first when browser tools are unavailable or not paired.",
                object_schema(json!({}), &[]),
                true,
                false
            ),
            tool(
                "start",
                "Create or resume the dedicated blue Rebon tab group and optionally open an initial http(s) URL. Only tabs created in this group are controlled.",
                object_schema(json!({
                    "url": { "type": "string", "minLength": 1, "description": "Optional initial http(s) URL. Defaults to about:blank." }
                }), &[]),
                false,
                false
            ),
            tool(
                "tabs",
                "List, create, activate, or close tabs inside the dedicated Rebon tab group.",
                object_schema(json!({
                    "action": { "type": "string", "enum": ["list", "new", "activate", "close"] },
                    "tab_id": { "type": "integer", "minimum": 0 },
                    "url": { "type": "string", "minLength": 1, "description": "Optional http(s) URL for action=new." }
                }), &["action"]),
                false,
                false
            ),
            tool(
                "navigate",
                "Navigate a controlled Rebon tab to an http(s) URL.",
                object_schema(json!({
                    "url": { "type": "string", "minLength": 1 },
                    "tab_id": { "type": "integer", "minimum": 0 }
                }), &["url"]),
                false,
                false
            ),
            tool(
                "observe",
                "Observe a controlled page. Returns URL, title, viewport, visible text, and interactive elements with document-scoped refs. Call again after navigation or when a ref becomes stale.",
                object_schema(json!({
                    "tab_id": { "type": "integer", "minimum": 0 },
                    "include_screenshot": { "type": "boolean", "default": false },
                    "max_elements": { "type": "integer", "minimum": 1, "maximum": 500, "default": 200 },
                    "max_text_chars": { "type": "integer", "minimum": 100, "maximum": 50000, "default": 16000 }
                }), &[]),
                true,
                false
            ),
            tool(
                "screenshot",
                "Capture the visible viewport of a controlled tab. Returns an absolute PNG path that can be inspected with Rebon's Read tool.",
                object_schema(json!({
                    "tab_id": { "type": "integer", "minimum": 0 }
                }), &[]),
                true,
                false
            ),
            tool(
                "click",
                "Move the visible virtual cursor and issue a trusted click in a controlled tab. Prefer an element ref from observe; viewport x/y coordinates are also supported.",
                target_schema(json!({
                    "button": { "type": "string", "enum": ["left", "middle", "right"], "default": "left" },
                    "click_count": { "type": "integer", "minimum": 1, "maximum": 3, "default": 1 },
                    "modifiers": modifiers_schema()
                })),
                false,
                true
            ),
            tool(
                "hover",
                "Move the visible virtual cursor and issue a trusted mouse-move over an element ref or viewport coordinates.",
                target_schema(json!({
                    "modifiers": modifiers_schema()
                })),
                false,
                false
            ),
            tool(
                "type",
                "Focus an observed element and type text using trusted browser input. Existing password values are never returned by observe.",
                object_schema(json!({
                    "ref": { "type": "string", "minLength": 1 },
                    "text": { "type": "string" },
                    "tab_id": { "type": "integer", "minimum": 0 },
                    "clear": { "type": "boolean", "default": false },
                    "submit": { "type": "boolean", "default": false }
                }), &["ref", "text"]),
                false,
                true
            ),
            tool(
                "key",
                "Send a trusted key press or key combination to a controlled tab.",
                object_schema(json!({
                    "key": { "type": "string", "minLength": 1, "description": "Key such as Enter, Escape, Tab, ArrowDown, or a printable character." },
                    "tab_id": { "type": "integer", "minimum": 0 },
                    "modifiers": modifiers_schema()
                }), &["key"]),
                false,
                true
            ),
            tool(
                "scroll",
                "Scroll a controlled page by viewport pixels, or bring an observed element ref into view.",
                scroll_schema(),
                false,
                false
            ),
            tool(
                "wait",
                "Wait for a controlled page delay, load completion, URL substring, or visible text. This does not control tabs outside the Rebon group.",
                wait_schema(),
                true,
                false
            ),
            tool(
                "stop",
                "Detach Rebon from every controlled tab and remove the cursor/glow overlay. Open tabs are preserved.",
                object_schema(json!({}), &[]),
                false,
                false
            )
        ]
    })
}

fn tool(
    name: &str,
    description: &str,
    input_schema: Value,
    read_only: bool,
    destructive: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": read_only,
            "openWorldHint": true
        },
        "_meta": {
            "anthropic/alwaysLoad": true,
            "anthropic/searchHint": format!("browser chrome edge tab page {name}")
        }
    })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn scroll_schema() -> Value {
    let mut schema = object_schema(
        json!({
            "tab_id": { "type": "integer", "minimum": 0 },
            "ref": { "type": "string", "minLength": 1 },
            "delta_x": { "type": "number", "default": 0 },
            "delta_y": { "type": "number", "default": 0 }
        }),
        &[],
    );
    schema["anyOf"] = json!([
        { "required": ["ref"] },
        { "required": ["delta_x"] },
        { "required": ["delta_y"] }
    ]);
    schema
}

fn wait_schema() -> Value {
    let mut schema = object_schema(
        json!({
            "tab_id": { "type": "integer", "minimum": 0 },
            "condition": { "type": "string", "enum": ["delay", "load", "url", "text"] },
            "value": { "type": ["string", "integer"] },
            "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 60000, "default": 10000 }
        }),
        &["condition"],
    );
    schema["allOf"] = json!([
        {
            "if": {
                "properties": { "condition": { "const": "delay" } },
                "required": ["condition"]
            },
            "then": {
                "properties": { "value": { "type": "integer", "minimum": 0, "maximum": 60000 } },
                "required": ["value"]
            }
        },
        {
            "if": {
                "properties": { "condition": { "enum": ["url", "text"] } },
                "required": ["condition"]
            },
            "then": {
                "properties": { "value": { "type": "string", "minLength": 1 } },
                "required": ["value"]
            }
        },
        {
            "if": {
                "properties": { "condition": { "const": "load" } },
                "required": ["condition"]
            },
            "then": { "not": { "required": ["value"] } }
        }
    ]);
    schema
}

fn target_schema(extra: Value) -> Value {
    let mut properties = Map::from_iter([
        (
            "ref".to_string(),
            json!({ "type": "string", "minLength": 1 }),
        ),
        ("x".to_string(), json!({ "type": "number", "minimum": 0 })),
        ("y".to_string(), json!({ "type": "number", "minimum": 0 })),
        (
            "tab_id".to_string(),
            json!({ "type": "integer", "minimum": 0 }),
        ),
    ]);
    if let Some(extra) = extra.as_object() {
        properties.extend(extra.clone());
    }
    json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
        "anyOf": [
            { "required": ["ref"] },
            { "required": ["x", "y"] }
        ]
    })
}

fn modifiers_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": ["Alt", "Control", "Meta", "Shift"] },
        "uniqueItems": true,
        "default": []
    })
}

pub fn validate_arguments(name: &str, arguments: &Value) -> anyhow::Result<()> {
    let object = arguments
        .as_object()
        .ok_or_else(|| anyhow!("browser tool arguments must be an object"))?;
    match name {
        "status" | "stop" => validate_fields(object, &[])?,
        "start" => {
            validate_fields(object, &["url"])?;
            validate_optional_url(object.get("url"))?;
        }
        "navigate" => {
            validate_fields(object, &["url", "tab_id"])?;
            validate_optional_tab_id(object)?;
            validate_required_url(object.get("url"))?;
        }
        "tabs" => validate_tabs(object)?,
        "observe" => {
            validate_fields(
                object,
                &[
                    "tab_id",
                    "include_screenshot",
                    "max_elements",
                    "max_text_chars",
                ],
            )?;
            validate_optional_tab_id(object)?;
            validate_optional_bool(object, "include_screenshot")?;
            validate_optional_u64_range(object, "max_elements", 1, 500)?;
            validate_optional_u64_range(object, "max_text_chars", 100, 50_000)?;
        }
        "screenshot" => {
            validate_fields(object, &["tab_id"])?;
            validate_optional_tab_id(object)?;
        }
        "click" => {
            validate_fields(
                object,
                &[
                    "ref",
                    "x",
                    "y",
                    "tab_id",
                    "button",
                    "click_count",
                    "modifiers",
                ],
            )?;
            validate_optional_tab_id(object)?;
            validate_target(object)?;
            validate_modifiers(object)?;
            if let Some(button) = object.get("button") {
                let button = button
                    .as_str()
                    .ok_or_else(|| anyhow!("`button` must be a string"))?;
                if !matches!(button, "left" | "middle" | "right") {
                    bail!("`button` must be left, middle, or right");
                }
            }
            validate_optional_u64_range(object, "click_count", 1, 3)?;
        }
        "hover" => {
            validate_fields(object, &["ref", "x", "y", "tab_id", "modifiers"])?;
            validate_optional_tab_id(object)?;
            validate_target(object)?;
            validate_modifiers(object)?;
        }
        "type" => {
            validate_fields(object, &["ref", "text", "tab_id", "clear", "submit"])?;
            validate_optional_tab_id(object)?;
            require_non_empty_string(object, "ref")?;
            require_string(object, "text")?;
            validate_optional_bool(object, "clear")?;
            validate_optional_bool(object, "submit")?;
        }
        "key" => {
            validate_fields(object, &["key", "tab_id", "modifiers"])?;
            validate_optional_tab_id(object)?;
            require_non_empty_string(object, "key")?;
            validate_modifiers(object)?;
        }
        "scroll" => {
            validate_fields(object, &["tab_id", "ref", "delta_x", "delta_y"])?;
            validate_optional_tab_id(object)?;
            let has_ref = object
                .get("ref")
                .map(|value| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| anyhow!("`ref` must be a non-empty string"))
                })
                .transpose()?
                .is_some();
            let has_delta_x = validate_optional_number(object, "delta_x", false)?;
            let has_delta_y = validate_optional_number(object, "delta_y", false)?;
            if !has_ref && !has_delta_x && !has_delta_y {
                bail!("scroll requires `ref`, `delta_x`, or `delta_y`");
            }
        }
        "wait" => validate_wait(object)?,
        _ => bail!("unknown browser tool `{name}`"),
    }
    Ok(())
}

fn validate_tabs(object: &Map<String, Value>) -> anyhow::Result<()> {
    validate_fields(object, &["action", "tab_id", "url"])?;
    let action = require_non_empty_string(object, "action")?;
    validate_optional_tab_id(object)?;
    validate_optional_url(object.get("url"))?;
    match action {
        "list" | "new" => Ok(()),
        "activate" | "close" => {
            if object.get("tab_id").and_then(Value::as_u64).is_none() {
                bail!("tabs action `{action}` requires numeric `tab_id`");
            }
            Ok(())
        }
        _ => bail!("unsupported tabs action `{action}`"),
    }
}

fn validate_target(object: &Map<String, Value>) -> anyhow::Result<()> {
    let has_ref = object
        .get("ref")
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("`ref` must be a non-empty string"))
        })
        .transpose()?
        .is_some();
    let has_x = validate_optional_number(object, "x", true)?;
    let has_y = validate_optional_number(object, "y", true)?;
    if !(has_ref || has_x && has_y) {
        bail!("browser target requires element `ref` or both viewport `x` and `y`");
    }
    Ok(())
}

fn validate_wait(object: &Map<String, Value>) -> anyhow::Result<()> {
    validate_fields(object, &["tab_id", "condition", "value", "timeout_ms"])?;
    validate_optional_tab_id(object)?;
    validate_optional_u64_range(object, "timeout_ms", 1, 60_000)?;
    let condition = require_non_empty_string(object, "condition")?;
    match condition {
        "delay" => {
            let value = object.get("value").and_then(Value::as_u64).ok_or_else(|| {
                anyhow!("wait condition `delay` requires integer millisecond `value`")
            })?;
            if value > 60_000 {
                bail!("wait delay `value` must be at most 60000");
            }
        }
        "url" | "text" => {
            require_non_empty_string(object, "value")?;
        }
        "load" => {
            if object.contains_key("value") {
                bail!("wait condition `load` does not accept `value`");
            }
        }
        _ => bail!("unsupported wait condition `{condition}`"),
    }
    Ok(())
}

fn validate_fields(object: &Map<String, Value>, allowed: &[&str]) -> anyhow::Result<()> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        bail!("unexpected browser tool argument `{field}`");
    }
    Ok(())
}

fn validate_optional_tab_id(object: &Map<String, Value>) -> anyhow::Result<()> {
    if let Some(value) = object.get("tab_id") {
        if value.as_u64().is_none() {
            bail!("`tab_id` must be a non-negative integer");
        }
    }
    Ok(())
}

fn validate_optional_bool(object: &Map<String, Value>, field: &str) -> anyhow::Result<()> {
    if let Some(value) = object.get(field) {
        if !value.is_boolean() {
            bail!("`{field}` must be a boolean");
        }
    }
    Ok(())
}

fn validate_optional_u64_range(
    object: &Map<String, Value>,
    field: &str,
    minimum: u64,
    maximum: u64,
) -> anyhow::Result<()> {
    if let Some(value) = object.get(field) {
        let value = value
            .as_u64()
            .ok_or_else(|| anyhow!("`{field}` must be an integer"))?;
        if !(minimum..=maximum).contains(&value) {
            bail!("`{field}` must be between {minimum} and {maximum}");
        }
    }
    Ok(())
}

fn validate_optional_number(
    object: &Map<String, Value>,
    field: &str,
    non_negative: bool,
) -> anyhow::Result<bool> {
    let Some(value) = object.get(field) else {
        return Ok(false);
    };
    let value = value
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| anyhow!("`{field}` must be a finite number"))?;
    if non_negative && value < 0.0 {
        bail!("`{field}` must be non-negative");
    }
    Ok(true)
}

fn validate_modifiers(object: &Map<String, Value>) -> anyhow::Result<()> {
    let Some(value) = object.get("modifiers") else {
        return Ok(());
    };
    let modifiers = value
        .as_array()
        .ok_or_else(|| anyhow!("`modifiers` must be an array"))?;
    let mut seen = Vec::with_capacity(modifiers.len());
    for modifier in modifiers {
        let modifier = modifier
            .as_str()
            .ok_or_else(|| anyhow!("each modifier must be a string"))?;
        if !matches!(modifier, "Alt" | "Control" | "Meta" | "Shift") {
            bail!("unsupported modifier `{modifier}`");
        }
        if seen.contains(&modifier) {
            bail!("modifier `{modifier}` must not be repeated");
        }
        seen.push(modifier);
    }
    Ok(())
}

fn validate_optional_url(value: Option<&Value>) -> anyhow::Result<()> {
    match value {
        None => Ok(()),
        Some(value) => validate_required_url(Some(value)),
    }
}

fn validate_required_url(value: Option<&Value>) -> anyhow::Result<()> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("browser URL must be a string"))?;
    validate_browser_url(value)
}

pub fn validate_browser_url(value: &str) -> anyhow::Result<()> {
    if value == "about:blank" {
        return Ok(());
    }
    let parsed = Url::parse(value).with_context(|| format!("invalid browser URL `{value}`"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!(
            "browser URL must use http or https; `{}` is not allowed",
            parsed.scheme()
        );
    }
    let Some(host) = parsed
        .host_str()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
    else {
        bail!("browser URL must include a host");
    };
    let path = parsed.path().to_ascii_lowercase();
    let is_chrome_store = host == "chromewebstore.google.com"
        || (host == "chrome.google.com" && path.starts_with("/webstore"));
    let is_edge_store = (host == "microsoftedge.microsoft.com" && path.starts_with("/addons"))
        || host == "addons.microsoft.com";
    if is_chrome_store || is_edge_store {
        bail!("browser extension store pages are not allowed");
    }
    Ok(())
}

fn require_non_empty_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> anyhow::Result<&'a str> {
    let value = require_string(object, field)?;
    if value.is_empty() {
        bail!("`{field}` must not be empty");
    }
    Ok(value)
}

fn require_string<'a>(object: &'a Map<String, Value>, field: &str) -> anyhow::Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("`{field}` must be a string"))
}

pub fn materialize_screenshots(
    tool_name: &str,
    value: &mut Value,
    screenshot_root: &Path,
) -> anyhow::Result<()> {
    if tool_name == "screenshot" {
        if value.get("data").and_then(Value::as_str).is_some() {
            let path = write_screenshot(value, screenshot_root)?;
            replace_image_data(value, &path);
        }
        return Ok(());
    }
    if tool_name == "observe" {
        if let Some(screenshot) = value.get_mut("screenshot") {
            if screenshot.get("data").and_then(Value::as_str).is_some() {
                let path = write_screenshot(screenshot, screenshot_root)?;
                replace_image_data(screenshot, &path);
            }
        }
    }
    Ok(())
}

fn write_screenshot(image: &Value, screenshot_root: &Path) -> anyhow::Result<PathBuf> {
    let data = image
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("browser screenshot response is missing base64 data"))?;
    let mime_type = image
        .get("mime_type")
        .or_else(|| image.get("mimeType"))
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    if mime_type != "image/png" {
        bail!("unsupported browser screenshot type `{mime_type}`");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("browser screenshot is not valid base64")?;
    if bytes.len() > 32 * 1024 * 1024 {
        bail!("browser screenshot exceeds the 32 MiB limit");
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("browser screenshot is not a PNG file");
    }
    create_private_directory(screenshot_root)?;
    for _ in 0..8 {
        let mut random = [0u8; 8];
        getrandom::getrandom(&mut random)
            .map_err(|err| anyhow!("could not generate screenshot filename: {err}"))?;
        let filename = format!(
            "browser-{}.png",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let path = screenshot_root.join(filename);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&bytes).with_context(|| {
                    format!("failed to write browser screenshot {}", path.display())
                })?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to create browser screenshot {}", path.display())
                });
            }
        }
    }
    bail!("could not allocate a unique browser screenshot filename")
}

fn create_private_directory(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", path.display()))?;
    }
    Ok(())
}

fn replace_image_data(image: &mut Value, path: &Path) {
    if let Some(object) = image.as_object_mut() {
        object.remove("data");
        object.remove("mimeType");
        object.insert(
            "mime_type".to_string(),
            Value::String("image/png".to_string()),
        );
        object.insert(
            "path".to_string(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_allows_web_and_rejects_privileged_schemes() {
        assert!(validate_browser_url("https://example.com/path").is_ok());
        assert!(validate_browser_url("http://127.0.0.1:3000").is_ok());
        assert!(validate_browser_url("about:blank").is_ok());
        assert!(validate_browser_url("file:///tmp/secret").is_err());
        assert!(validate_browser_url("chrome://settings").is_err());
        assert!(validate_browser_url("javascript:alert(1)").is_err());
        assert!(validate_browser_url("data:text/html,hello").is_err());
        for url in [
            "https://chromewebstore.google.com/detail/example/abcdefghijklmnop",
            "https://chromewebstore.google.com./detail/example/abcdefghijklmnop",
            "https://chrome.google.com/webstore/detail/example/abcdefghijklmnop",
            "https://chrome.google.com./webstore/detail/example/abcdefghijklmnop",
            "https://microsoftedge.microsoft.com/addons/detail/example/abcdefghijklmnop",
            "https://microsoftedge.microsoft.com./addons/detail/example/abcdefghijklmnop",
            "https://addons.microsoft.com/detail/example/abcdefghijklmnop",
            "https://addons.microsoft.com./detail/example/abcdefghijklmnop",
        ] {
            assert!(validate_browser_url(url).is_err(), "allowed {url}");
        }
    }

    #[test]
    fn target_requires_ref_or_complete_coordinates() {
        assert!(validate_arguments("click", &json!({ "ref": "e1" })).is_ok());
        assert!(validate_arguments("click", &json!({ "x": 10, "y": 20 })).is_ok());
        assert!(validate_arguments("click", &json!({ "x": 10 })).is_err());
        assert!(validate_arguments("hover", &json!({})).is_err());
    }

    #[test]
    fn tab_actions_require_their_specific_fields() {
        assert!(validate_arguments("tabs", &json!({ "action": "list" })).is_ok());
        assert!(validate_arguments(
            "tabs",
            &json!({ "action": "new", "url": "https://example.com" })
        )
        .is_ok());
        assert!(validate_arguments("tabs", &json!({ "action": "activate" })).is_err());
        assert!(validate_arguments("tabs", &json!({ "action": "close", "tab_id": 7 })).is_ok());
    }

    #[test]
    fn argument_validation_matches_declared_schema_boundaries() {
        assert!(validate_arguments("status", &json!({ "unexpected": true })).is_err());
        assert!(validate_arguments("scroll", &json!({})).is_err());
        assert!(validate_arguments("scroll", &json!({ "delta_y": 100 })).is_ok());
        assert!(
            validate_arguments("wait", &json!({ "condition": "delay", "value": 1.5 })).is_err()
        );
        assert!(validate_arguments("wait", &json!({ "condition": "delay", "value": 500 })).is_ok());
        assert!(validate_arguments("wait", &json!({ "condition": "load", "value": 1 })).is_err());
        assert!(validate_arguments(
            "hover",
            &json!({ "x": 10, "y": 20, "modifiers": ["Shift"] })
        )
        .is_ok());
        assert!(validate_arguments(
            "hover",
            &json!({ "x": 10, "y": 20, "modifiers": ["Shift", "Shift"] })
        )
        .is_err());
        assert!(validate_arguments("observe", &json!({ "max_elements": 501 })).is_err());
    }

    #[test]
    fn screenshot_data_becomes_a_png_path() {
        let directory = tempfile::tempdir().unwrap();
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nrest");
        let mut result = json!({ "data": png, "mime_type": "image/png" });
        materialize_screenshots("screenshot", &mut result, directory.path()).unwrap();
        let path = PathBuf::from(result["path"].as_str().unwrap());
        assert!(path.exists());
        assert!(result.get("data").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn definitions_mark_observation_read_only_and_click_destructive() {
        let definitions = tool_definitions();
        let tools = definitions["tools"].as_array().unwrap();
        let observe = tools.iter().find(|tool| tool["name"] == "observe").unwrap();
        let click = tools.iter().find(|tool| tool["name"] == "click").unwrap();
        let hover = tools.iter().find(|tool| tool["name"] == "hover").unwrap();
        let scroll = tools.iter().find(|tool| tool["name"] == "scroll").unwrap();
        let wait = tools.iter().find(|tool| tool["name"] == "wait").unwrap();
        assert_eq!(observe["annotations"]["readOnlyHint"], true);
        assert_eq!(click["annotations"]["destructiveHint"], true);
        assert!(hover["inputSchema"]["properties"]
            .get("modifiers")
            .is_some());
        assert_eq!(scroll["inputSchema"]["anyOf"].as_array().unwrap().len(), 3);
        assert_eq!(wait["inputSchema"]["allOf"].as_array().unwrap().len(), 3);
    }
}
