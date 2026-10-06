//! What a mod in a container may do through the `mods` seat.
//!
//! The seat's `fs.*`, `process.run` and `http.fetch` run in this process, not
//! in the mod's host, so the container's Node permissions do not reach them;
//! this is where the same contract is kept for a contained mod:
//!
//! * reads stay inside the session's workspace, the mod's own folder and its
//!   data directory;
//! * writes land in its data directory directly — anywhere else they go
//!   through rebon's `Write` tool, where the person's permission is asked;
//! * a process runs through rebon's `Bash` tool, asked the same way;
//! * the network reaches only the hosts the mod was granted.
//!
//! A mod on the shared host (one the person trusts) keeps the seat's direct
//! behaviour.

use std::path::{Component, Path, PathBuf};

use rebon_plugin_protocol::{CallIdentity, Payload};
use rebon_plugin_supervisor::{ToolInvocation, ToolRefusal};
use serde_json::{json, Value};

use super::{ModRecord, ModsRegistry};
use crate::container::ContainerSpec;

/// A path with `.` and `..` folded away without touching the disk, so a
/// mod cannot climb out of a root by spelling `root/../elsewhere`.
pub(crate) fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn comparable(path: &Path) -> String {
    let text = crate::container::permission_path(&lexical(path))
        .to_string_lossy()
        .replace('\\', "/");
    let text = text.trim_end_matches('/').to_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// Whether `path` is `root` or inside it.
pub(crate) fn within(path: &Path, root: &Path) -> bool {
    let path = comparable(path);
    let root = comparable(root);
    !root.is_empty() && (path == root || path.starts_with(&format!("{root}/")))
}

fn read_roots(record: &ModRecord, spec: &ContainerSpec) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(&record.root), PathBuf::from(&spec.data_dir)];
    let cwd = record.facts().cwd;
    if !cwd.is_empty() {
        roots.push(PathBuf::from(cwd));
    }
    roots
}

/// Refuses a read outside the mod's workspace, folder and data.
pub(crate) fn check_read(
    record: &ModRecord,
    spec: &ContainerSpec,
    path: &Path,
) -> Result<(), ToolRefusal> {
    let roots = read_roots(record, spec);
    let lexically = roots.iter().any(|root| within(path, root));
    // A link inside a root may point anywhere: what it resolves to has to be
    // inside one too, compared against the roots as they resolve.
    let resolved = match std::fs::canonicalize(path) {
        Ok(real) => roots.iter().any(|root| {
            std::fs::canonicalize(root)
                .map(|root| within(&real, &root))
                .unwrap_or(false)
        }),
        Err(_) => true,
    };
    if lexically && resolved {
        return Ok(());
    }
    Err(ToolRefusal::new(
        "[NOT_PERMITTED]",
        format!(
            "{} runs in a container: it reads its workspace, its own folder and its data \
             directory, and {} is none of those",
            record.name,
            path.display()
        ),
    ))
}

/// Whether a write lands in the mod's own data directory.
pub(crate) fn writes_own_data(spec: &ContainerSpec, path: &Path) -> bool {
    within(path, Path::new(&spec.data_dir))
}

/// Whether `url` names a host the mod was granted.
pub(crate) fn check_fetch(
    record: &ModRecord,
    spec: &ContainerSpec,
    url: &str,
) -> Result<(), ToolRefusal> {
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let granted = rebon_plugin_package::container::admits_any_host(&spec.network)
        || spec.network.iter().any(|rule| {
            let rule = rule.trim().trim_end_matches('.').to_ascii_lowercase();
            match rule.strip_prefix("*.") {
                Some(apex) => host.ends_with(&format!(".{apex}")),
                None => !rule.is_empty() && (host == rule || host.ends_with(&format!(".{rule}"))),
            }
        });
    if granted {
        return Ok(());
    }
    Err(ToolRefusal::new(
        "[NOT_PERMITTED]",
        format!(
            "{} runs in a container granted {}; {} is not one of them",
            record.name,
            if spec.network.is_empty() {
                "no network hosts".to_owned()
            } else {
                format!("only {}", spec.network.join(", "))
            },
            if host.is_empty() { url } else { &host }
        ),
    ))
}

/// Runs one of rebon's own tools for the mod, through the plane's invoker —
/// the same path a plugin's `tool/invoke` takes, so the person's permission
/// is asked there.
pub(crate) async fn invoke(
    registry: &ModsRegistry,
    record: &ModRecord,
    tool: &str,
    input: Value,
) -> Result<Value, ToolRefusal> {
    let Some(invoker) = registry.tool_invoker() else {
        return Err(ToolRefusal::new(
            "[NOT_PERMITTED]",
            format!(
                "{} runs in a container and this plane cannot ask for {tool} on its behalf",
                record.name
            ),
        ));
    };
    let identity = CallIdentity {
        host_epoch: 1,
        plugin_id: record.id.clone(),
        scope_id: registry.scope().to_owned(),
        scope_generation: 1,
        call_id: format!("mods-{}-{tool}", record.id),
    };
    let answer = invoker
        .invoke(ToolInvocation {
            identity,
            tool: tool.to_owned(),
            input: Payload::from(input),
        })
        .await?;
    Ok(answer.to_value().unwrap_or(Value::Null))
}

/// A tool answer's text, whatever envelope it came in.
pub(crate) fn answer_text(answer: &Value) -> String {
    match answer {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(answer_text).collect::<Vec<_>>().join(""),
        Value::Object(map) => {
            for key in ["text", "content", "output", "ok"] {
                if let Some(inner) = map.get(key) {
                    return answer_text(inner);
                }
            }
            String::new()
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `process.run` for a contained mod: the argv as one `Bash` command.
pub(crate) async fn run_process(
    registry: &ModsRegistry,
    record: &ModRecord,
    params: &Value,
) -> Result<Value, ToolRefusal> {
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
    if argv.is_empty() {
        return Err(ToolRefusal::new(
            "[WRONG_SHAPE]",
            "process.run needs a non-empty argv",
        ));
    }
    let command = argv
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut input = json!({
        "command": command,
        "description": format!("{} (mod) runs a command", record.name),
    });
    if let Some(ms) = params.get("timeoutMs").and_then(Value::as_u64) {
        input["timeout"] = json!(ms);
    }
    let started = std::time::Instant::now();
    match invoke(registry, record, "Bash", input).await {
        Ok(answer) => Ok(json!({
            "exitCode": 0,
            "isSuccess": true,
            "stdout": answer_text(&answer),
            "stderr": "",
            "durationMs": started.elapsed().as_millis() as u64,
        })),
        Err(refusal) => Err(refusal),
    }
}

/// One argument as POSIX `sh` reads it literally.
pub(crate) fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_climbing_out_of_a_root_is_not_inside_it() {
        assert!(within(Path::new("/w/src/a.rs"), Path::new("/w")));
        assert!(within(Path::new("/w"), Path::new("/w/")));
        assert!(!within(Path::new("/w/../etc/passwd"), Path::new("/w")));
        assert!(
            !within(Path::new("/wx/a"), Path::new("/w")),
            "a sibling sharing a prefix"
        );
        assert!(!within(Path::new("/w/a"), Path::new("")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_without_case_or_verbatim_prefix() {
        assert!(within(
            Path::new(r"C:\Work\a.txt"),
            Path::new(r"\\?\c:\work")
        ));
    }

    #[test]
    fn arguments_are_quoted_for_sh() {
        assert_eq!(shell_quote("git"), "git");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn a_tool_answer_reads_as_its_text() {
        assert_eq!(answer_text(&json!("hi")), "hi");
        assert_eq!(
            answer_text(&json!({"content": [{"type": "text", "text": "a"}, {"text": "b"}]})),
            "ab"
        );
        assert_eq!(answer_text(&json!({"ok": {"output": "x"}})), "x");
    }
}
