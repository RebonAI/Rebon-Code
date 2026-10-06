//! The `mods` seat: `$`, answered.
//!
//! Every method on a mod's `$` that has to leave the Node process arrives
//! here as `seat/call` with the method spelled `noun.method` and its
//! arguments as params. What a method does falls into four groups:
//!
//! * display — written to the UI table every surface reads
//!   (`ui.status`, `ui.toast`, `ui.log`, `ui.open`, `ui.close`,
//!   `ui.invalidate`, `ui.notice`, `ui.panes`), and the prompt queue
//!   (`prompt.submit`, `prompt.fill`, `prompt.suggest`);
//! * the mod's own values — `$.state` (the session's, with a version per
//!   value) and `$.store` (across sessions, a JSON file under the config
//!   home);
//! * the machine — files, child processes, HTTP, environment, settings,
//!   each run off the plane's reader so a slow one stalls nothing;
//! * registrations — `command.register` and `tool.register`, which refine
//!   a declared name's description on the seats it is already on.
//!
//! A method no surface on rebon can serve answers with the shape the
//! declarations give for "not here" (`{ isCopied: false, reason: ... }`),
//! or rejects naming the method, never silently.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rebon_plugin_supervisor::ToolRefusal;
use serde_json::{json, Value};

use super::ui::ModPane;
use super::{ModRecord, ModsRegistry};

/// How long `process.run` waits by default.
const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `http.fetch` waits by default.
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// The most `process.run` keeps of a child's output.
const MAX_PROCESS_OUTPUT: usize = 1024 * 1024;
/// The most `http.fetch` keeps of a body.
const MAX_HTTP_BODY: usize = 8 * 1024 * 1024;
/// The most `fs.read` hands back.
const MAX_FILE_READ: usize = 32 * 1024 * 1024;

fn refused(code: &str, message: impl Into<String>) -> ToolRefusal {
    ToolRefusal::new(code, message)
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, ToolRefusal> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| refused("[WRONG_SHAPE]", format!("{name} must be a string")))
}

fn opt_str(params: &Value, name: &str) -> Option<String> {
    params.get(name).and_then(Value::as_str).map(str::to_owned)
}

/// A path as the mod spelled it, resolved against the session's cwd.
fn resolve_path(record: &ModRecord, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let cwd = record.facts().cwd;
    if cwd.is_empty() {
        return path.to_path_buf();
    }
    Path::new(&cwd).join(path)
}

fn state_key(reference: &Value) -> Result<String, ToolRefusal> {
    let plugin = reference
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = reference
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| refused("[WRONG_SHAPE]", "a state reference names a key"))?;
    let id = reference.get("id").and_then(Value::as_str);
    Ok(match id {
        Some(id) => format!("{plugin}\u{0}{key}\u{0}{id}"),
        None => format!("{plugin}\u{0}{key}"),
    })
}

/// The surfaces that seat a mod's pane: the terminal docks it, the desktop
/// draws it in its side column.
const PANE_SURFACES: &[&str] = &["terminal", "desktop"];

/// `$.ui.open`'s answer (`UiOpenResult`): placed once a surface that seats
/// panes is attached, else waiting, with why. Whether a terminal is wide
/// enough to seat an unasked pane is that terminal's to decide, and it is
/// not asked here: the pane waits there undrawn as Claude Code's does.
fn pane_placement(attached: &[String]) -> Value {
    if attached
        .iter()
        .any(|surface| PANE_SURFACES.contains(&surface.as_str()))
    {
        json!({ "isPlaced": true })
    } else if attached.is_empty() {
        json!({
            "isPlaced": false,
            "reason": "no surface is attached to this session yet; the pane is seated when one is",
        })
    } else {
        json!({
            "isPlaced": false,
            "reason": format!("the attached surfaces ({}) place no panes", attached.join(", ")),
        })
    }
}

/// Answers one `$` call for `record`.
pub async fn call(
    registry: Arc<ModsRegistry>,
    record: Arc<ModRecord>,
    method: &str,
    params: Value,
) -> Result<Value, ToolRefusal> {
    let ui = registry.ui.clone();
    match method {
        // ---- display ------------------------------------------------
        "ui.status" => {
            ui.set_status(&record.id, opt_str(&params, "text"));
            Ok(json!({}))
        }
        "ui.toast" => {
            let text = str_param(&params, "text")?.to_owned();
            let id = ui.push_toast(&record.id, text, opt_str(&params, "kind"));
            Ok(json!({ "id": id }))
        }
        "ui.log" => {
            let text = str_param(&params, "text")?.to_owned();
            let to = opt_str(&params, "to").unwrap_or_else(|| "transcript".into());
            let line = format!("{}: {text}", record.name);
            if to == "debug" {
                tracing::debug!(mod_ = %record.name, "{text}");
            } else {
                tracing::info!(mod_ = %record.name, "{text}");
            }
            ui.push_log(&record.id, line, &to);
            Ok(json!({}))
        }
        "ui.notice" => {
            let tool_use_id = str_param(&params, "toolUseId")?.to_owned();
            ui.set_notice(&record.id, &tool_use_id, opt_str(&params, "text"));
            Ok(json!({}))
        }
        "ui.invalidate" => {
            let event = opt_str(&params, "event").unwrap_or_else(|| "ui.render".into());
            if event == "ui.render" {
                ui.invalidate(&record.id, None);
            }
            Ok(json!({}))
        }
        "ui.open" => {
            let id = str_param(&params, "id")?.to_owned();
            let title = opt_str(&params, "title").unwrap_or_else(|| id.clone());
            let unasked = !record.in_person_action();
            ui.open_pane(ModPane {
                plugin: record.id.clone(),
                plugin_name: record.name.clone(),
                id: id.clone(),
                title,
                version: 0,
                focus: params
                    .get("focus")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                close_on_escape: params
                    .get("closeOnEscape")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                rows: params.get("rows").and_then(Value::as_u64).map(|r| r as u32),
                unasked,
            });
            Ok(pane_placement(&registry.attached_surfaces()))
        }
        "ui.close" => {
            let id = str_param(&params, "id")?;
            let closed = ui.close_pane(&record.id, id);
            Ok(json!({ "id": id, "isClosed": closed }))
        }
        "ui.panes" => Ok(ui.panes_json(&record.id)),
        "ui.focus" => {
            // The keyboard is the person's to give: the surface holding the
            // site moves the ring when it takes the ask, and only then.
            ui.push_focus(
                &record.id,
                str_param(&params, "requestId")?.to_owned(),
                str_param(&params, "key")?.to_owned(),
            );
            Ok(json!({}))
        }
        "ui.scroll" => Ok(json!({})),
        "ui.copy" => Ok(json!({ "isCopied": false, "reason": "no-surface" })),
        "ui.selection" => Ok(Value::Null),
        "ui.ask" => Err(refused(
            "[NOT_ON_REBON]",
            "$.ui.ask has no dialog on rebon's surfaces; open a pane with Buttons instead",
        )),

        // ---- the prompt box ----------------------------------------
        "prompt.submit" => {
            let text = str_param(&params, "text")?.to_owned();
            let id = ui.push_prompt(&record.id, text);
            Ok(json!({ "isQueued": true, "id": id }))
        }
        "prompt.fill" => {
            let text = str_param(&params, "text")?.to_owned();
            let mode = opt_str(&params, "mode").unwrap_or_else(|| "replace".into());
            if !matches!(mode.as_str(), "replace" | "append" | "insert") {
                return Err(refused(
                    "[WRONG_SHAPE]",
                    "prompt.fill mode is replace, append or insert",
                ));
            }
            ui.push_fill(&record.id, text, mode);
            Ok(json!({ "isFilled": true }))
        }
        "prompt.suggest" => Ok(json!({ "isShown": false })),
        "prompt.read" => Ok(json!({ "text": "", "cursor": 0 })),
        "prompt.compose" => Err(refused(
            "[NOT_ON_REBON]",
            "$.prompt.compose is not served on rebon; the system prompt is the engine's",
        )),

        // ---- the mod's own values -----------------------------------
        "state.get" => {
            let key = state_key(params.get("ref").unwrap_or(&Value::Null))?;
            let (value, version) = record.state_get(&key);
            // An unset value has no `value` key at all: JSON has no
            // `undefined`, and a `null` would read as a value a mod set.
            if version == 0 {
                return Ok(json!({ "version": 0 }));
            }
            Ok(json!({ "value": value, "version": version }))
        }
        "state.set" => {
            let key = state_key(params.get("ref").unwrap_or(&Value::Null))?;
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            let if_version = params.get("ifVersion").and_then(Value::as_u64);
            match record.state_set(&key, value, if_version) {
                Ok((value, version)) => {
                    // A drawing that read the value is drawn again.
                    ui.invalidate(&record.id, None);
                    Ok(json!({ "isWritten": true, "value": value, "version": version }))
                }
                Err((value, version)) => {
                    Ok(json!({ "isWritten": false, "value": value, "version": version }))
                }
            }
        }
        "store.get" => {
            let key = str_param(&params, "key")?;
            Ok(registry
                .store_read(&record)
                .get(key)
                .cloned()
                .unwrap_or(Value::Null))
        }
        "store.set" => {
            let key = str_param(&params, "key")?.to_owned();
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            registry
                .store_update(&record, |store| {
                    store.insert(key, value);
                })
                .map_err(|error| refused("[STORE_FAILED]", error))?;
            Ok(json!({}))
        }
        "store.delete" => {
            let key = str_param(&params, "key")?.to_owned();
            registry
                .store_update(&record, |store| {
                    store.remove(&key);
                })
                .map_err(|error| refused("[STORE_FAILED]", error))?;
            Ok(json!({}))
        }
        "store.keys" => Ok(Value::Array(
            registry
                .store_read(&record)
                .keys()
                .map(|k| Value::String(k.clone()))
                .collect(),
        )),

        // ---- the machine ----------------------------------------------
        "fs.read" => {
            let path = resolve_path(&record, str_param(&params, "path")?);
            if let Some(spec) = &record.container {
                super::contained::check_read(&record, spec, &path)?;
            }
            let as_bytes = params.get("as").and_then(Value::as_str) == Some("bytes");
            tokio::task::spawn_blocking(move || {
                let bytes = std::fs::read(&path)
                    .map_err(|error| refused("[FS_FAILED]", format!("{}: {error}", path.display())))?;
                if bytes.len() > MAX_FILE_READ {
                    return Err(refused("[FS_TOO_LARGE]", format!("{} is larger than {MAX_FILE_READ} bytes", path.display())));
                }
                if as_bytes {
                    use base64::Engine as _;
                    Ok(json!({ "base64": base64::engine::general_purpose::STANDARD.encode(&bytes) }))
                } else {
                    Ok(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
                }
            })
            .await
            .map_err(|error| refused("[FS_FAILED]", error.to_string()))?
        }
        "fs.write" => {
            let path = resolve_path(&record, str_param(&params, "path")?);
            let text = str_param(&params, "text")?.to_owned();
            if let Some(spec) = &record.container {
                if !super::contained::writes_own_data(spec, &path) {
                    // Outside its data directory: the person decides, through
                    // rebon's own Write tool.
                    super::contained::invoke(
                        &registry,
                        &record,
                        "Write",
                        json!({ "file_path": path.to_string_lossy(), "content": text }),
                    )
                    .await?;
                    return Ok(json!({ "path": path.to_string_lossy() }));
                }
            }
            tokio::task::spawn_blocking(move || {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        refused("[FS_FAILED]", format!("{}: {error}", parent.display()))
                    })?;
                }
                rebon_session::write_file_atomically(&path, text.as_bytes()).map_err(|error| {
                    refused("[FS_FAILED]", format!("{}: {error}", path.display()))
                })?;
                Ok(json!({ "path": path.to_string_lossy() }))
            })
            .await
            .map_err(|error| refused("[FS_FAILED]", error.to_string()))?
        }
        "fs.list" => {
            let path = match params.get("path").and_then(Value::as_str) {
                Some(raw) => resolve_path(&record, raw),
                None => PathBuf::from(record.facts().cwd),
            };
            if let Some(spec) = &record.container {
                super::contained::check_read(&record, spec, &path)?;
            }
            tokio::task::spawn_blocking(move || {
                let entries = std::fs::read_dir(&path).map_err(|error| {
                    refused("[FS_FAILED]", format!("{}: {error}", path.display()))
                })?;
                let mut out = Vec::new();
                for entry in entries.flatten() {
                    let kind = entry.file_type().ok();
                    out.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "path": entry.path().to_string_lossy(),
                        "isDirectory": kind.is_some_and(|k| k.is_dir()),
                        "isFile": kind.is_some_and(|k| k.is_file()),
                        "isSymlink": kind.is_some_and(|k| k.is_symlink()),
                    }));
                }
                out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(Value::Array(out))
            })
            .await
            .map_err(|error| refused("[FS_FAILED]", error.to_string()))?
        }
        "fs.exists" => {
            let path = resolve_path(&record, str_param(&params, "path")?);
            if let Some(spec) = &record.container {
                super::contained::check_read(&record, spec, &path)?;
            }
            Ok(Value::Bool(path.exists()))
        }
        "fs.stat" => {
            let path = resolve_path(&record, str_param(&params, "path")?);
            if let Some(spec) = &record.container {
                super::contained::check_read(&record, spec, &path)?;
            }
            let resolve = params
                .get("resolve")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            tokio::task::spawn_blocking(move || {
                let meta = std::fs::symlink_metadata(&path).map_err(|error| {
                    refused("[FS_FAILED]", format!("{}: {error}", path.display()))
                })?;
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64);
                let mut out = json!({
                    "path": path.to_string_lossy(),
                    "isFile": meta.is_file(),
                    "isDirectory": meta.is_dir(),
                    "isSymlink": meta.file_type().is_symlink(),
                    "size": meta.len(),
                    "mtimeMs": modified,
                });
                if resolve {
                    if let Ok(real) = std::fs::canonicalize(&path) {
                        out["realPath"] = Value::String(
                            real.to_string_lossy()
                                .trim_start_matches(r"\\?\")
                                .to_owned(),
                        );
                    }
                }
                Ok(out)
            })
            .await
            .map_err(|error| refused("[FS_FAILED]", error.to_string()))?
        }
        "fs.ancestors" => {
            let path = resolve_path(&record, str_param(&params, "path")?);
            let mut out = Vec::new();
            let mut current = path.parent().map(Path::to_path_buf);
            while let Some(dir) = current {
                out.push(json!({ "path": dir.to_string_lossy() }));
                current = dir.parent().map(Path::to_path_buf);
            }
            Ok(json!({ "ancestors": out }))
        }
        "process.run" => match &record.container {
            Some(_) => super::contained::run_process(&registry, &record, &params).await,
            None => process_run(&record, &params).await,
        },
        "http.fetch" => {
            if let Some(spec) = &record.container {
                super::contained::check_fetch(&record, spec, str_param(&params, "url")?)?;
            }
            http_fetch(&params).await
        }
        "env.get" => {
            let name = str_param(&params, "name")?;
            if let Some(value) = record.env_get(name) {
                return Ok(Value::String(value));
            }
            Ok(std::env::var(name)
                .map(Value::String)
                .unwrap_or(Value::Null))
        }
        "env.set" => {
            let name = str_param(&params, "name")?.to_owned();
            record.env_set(name, opt_str(&params, "value"));
            Ok(json!({}))
        }
        "settings.read" => registry.settings_read(&record),

        // ---- the session ---------------------------------------------
        "session.surfaces" => Ok(json!(registry.attached_surfaces())),
        "session.version" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "base": env!("CARGO_PKG_VERSION"),
            "host": "rebon",
        })),
        "session.usage" => Ok(json!({})),
        "session.repo" => Ok(Value::Null),
        "session.messages" => Ok(Value::Array(Vec::new())),
        "session.append" | "session.send" | "session.compact" | "session.authorize"
        | "turn.abort" => Err(refused(
            "[NOT_ON_REBON]",
            format!(
                "$.{method} is not served on rebon's plane; the conversation is the session's own"
            ),
        )),

        // ---- tools, commands, config, agents ---------------------------
        "tool.list" => Ok(registry.tool_list()),
        "tool.check" => Ok(json!({ "decision": "ask" })),
        "tool.register" => registry.register_tool(&record, &params),
        "command.list" => Ok(registry.command_list()),
        "command.register" => registry.register_command(&record, &params),
        "command.run" => Err(refused(
            "[NOT_ON_REBON]",
            "$.command.run is not served; a mod runs its own command through its command.run hook",
        )),
        "config.list" => Ok(Value::Array(Vec::new())),
        "config.set" => Err(refused(
            "[NOT_ON_REBON]",
            "$.config.set has no rows on rebon",
        )),
        "model.complete" | "model.fork" | "model.classify" => Ok(json!({
            "isAnswered": false,
            "reason": "api-error",
            "status": 501,
            "error": "not-on-rebon",
            "usage": { "input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 },
        })),
        "agent.list" => Ok(Value::Array(Vec::new())),
        "agent.spawn" | "agent.register" | "mcp.connect" => Err(refused(
            "[NOT_ON_REBON]",
            format!("$.{method} is not served on rebon's plane"),
        )),
        other => Err(refused(
            "[UNKNOWN_METHOD]",
            format!("the mods seat has no method {other}"),
        )),
    }
}

async fn process_run(record: &ModRecord, params: &Value) -> Result<Value, ToolRefusal> {
    let argv: Vec<String> = params
        .get("argv")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let Some((program, args)) = argv.split_first() else {
        return Err(refused(
            "[WRONG_SHAPE]",
            "process.run needs a non-empty argv",
        ));
    };
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(|raw| resolve_path(record, raw))
        .unwrap_or_else(|| PathBuf::from(record.facts().cwd));
    let timeout = params
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_PROCESS_TIMEOUT);
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    if cwd.is_dir() {
        command.current_dir(&cwd);
    }
    for (name, value) in record.env_overlay() {
        match value {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
    if let Some(env) = params.get("env").and_then(Value::as_object) {
        for (name, value) in env {
            if let Some(value) = value.as_str() {
                command.env(name, value);
            }
        }
    }
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    let started = std::time::Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| refused("[PROCESS_FAILED]", format!("{program}: {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(input) = params.get("input").and_then(Value::as_str) {
            use tokio::io::AsyncWriteExt as _;
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        drop(stdin);
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let clip = |bytes: &[u8]| {
                let end = bytes.len().min(MAX_PROCESS_OUTPUT);
                String::from_utf8_lossy(&bytes[..end]).into_owned()
            };
            Ok(json!({
                "exitCode": output.status.code(),
                "isSuccess": output.status.success(),
                "stdout": clip(&output.stdout),
                "stderr": clip(&output.stderr),
                "durationMs": started.elapsed().as_millis() as u64,
            }))
        }
        Ok(Err(error)) => Err(refused("[PROCESS_FAILED]", error.to_string())),
        Err(_) => Ok(json!({
            "exitCode": null,
            "isSuccess": false,
            "isTimedOut": true,
            "stdout": "",
            "stderr": "",
            "durationMs": started.elapsed().as_millis() as u64,
        })),
    }
}

async fn http_fetch(params: &Value) -> Result<Value, ToolRefusal> {
    let url = str_param(params, "url")?;
    let method = params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let timeout = params
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_HTTP_TIMEOUT);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| refused("[HTTP_FAILED]", error.to_string()))?;
    let verb = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|error| refused("[WRONG_SHAPE]", error.to_string()))?;
    let mut request = client.request(verb, url);
    if let Some(headers) = params.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            if let Some(value) = value.as_str() {
                request = request.header(name, value);
            }
        }
    }
    match params.get("body") {
        Some(Value::String(text)) => request = request.body(text.clone()),
        Some(Value::Null) | None => {}
        Some(other) => request = request.json(other),
    }
    let response = request
        .send()
        .await
        .map_err(|error| refused("[HTTP_FAILED]", error.to_string()))?;
    let status = response.status().as_u16();
    let headers: serde_json::Map<String, Value> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                Value::String(value.to_str().unwrap_or_default().to_owned()),
            )
        })
        .collect();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| refused("[HTTP_FAILED]", error.to_string()))?;
    let end = bytes.len().min(MAX_HTTP_BODY);
    Ok(json!({
        "status": status,
        "ok": (200..300).contains(&status),
        "headers": headers,
        "text": String::from_utf8_lossy(&bytes[..end]),
        "isTruncated": bytes.len() > MAX_HTTP_BODY,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surfaces(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_pane_is_placed_once_a_surface_that_seats_panes_is_attached() {
        assert_eq!(
            pane_placement(&surfaces(&["terminal"])),
            json!({ "isPlaced": true })
        );
        assert_eq!(
            pane_placement(&surfaces(&["mobile", "desktop"])),
            json!({ "isPlaced": true })
        );
    }

    #[test]
    fn a_pane_with_nowhere_to_sit_waits_and_says_why() {
        let none = pane_placement(&[]);
        assert_eq!(none["isPlaced"], false);
        assert!(none["reason"].as_str().unwrap().contains("no surface"));
        let mobile = pane_placement(&surfaces(&["mobile"]));
        assert_eq!(mobile["isPlaced"], false);
        assert!(mobile["reason"].as_str().unwrap().contains("mobile"));
    }
}
