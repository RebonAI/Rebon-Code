//! Best-effort external-editor launcher for picker dialogs.
//!
//! Editors split into GUI editors
//! (detached spawn) and terminal editors (foreground handoff). Both
//! paths are implemented here: `open_file_in_external_editor` detaches,
//! which is the common case for Quick Open / Global Search on desktop
//! terminals, and `edit_text_in_external_editor` runs the editor in the
//! foreground and waits for its exit (the caller suspends the terminal
//! around the call).

use anyhow::{anyhow, Context};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const GUI_EDITORS: &[&str] = &[
    "code",
    "cursor",
    "windsurf",
    "codium",
    "subl",
    "atom",
    "gedit",
    "notepad++",
    "notepad",
];

const VSCODE_FAMILY: &[&str] = &["code", "cursor", "windsurf", "codium"];

pub fn edit_text_in_external_editor(initial_text: &str) -> anyhow::Result<String> {
    let editor = external_editor().ok_or_else(|| anyhow!("VISUAL or EDITOR is not set"))?;
    let temp_path = external_edit_temp_path();
    fs::write(&temp_path, initial_text).with_context(|| {
        format!(
            "failed to write external editor temp file {}",
            temp_path.display()
        )
    })?;

    let result = run_editor_waiting(&editor, &temp_path).and_then(|()| {
        fs::read_to_string(&temp_path).with_context(|| {
            format!(
                "failed to read external editor temp file {}",
                temp_path.display()
            )
        })
    });
    let _ = fs::remove_file(&temp_path);
    result
}

/// Best-effort launch of a file in the user's external editor.
///
/// Returns `true` when the spawn succeeded, `false` when no supported
/// editor is configured or the launch failed.
pub fn open_file_in_external_editor(path: &Path, line: Option<u64>) -> bool {
    let Some(editor) = external_editor() else {
        return false;
    };
    let Some(gui_family) = classify_gui_editor(&editor) else {
        tracing::debug!(
            editor = %editor,
            "rebon-cli: external editor is not a known GUI editor; skipping detached open"
        );
        return false;
    };

    let (base, editor_args) = split_editor_command(&editor);
    let goto_args = gui_goto_argv(gui_family, path, line);

    if cfg!(windows) {
        spawn_windows(&base, &editor_args, &goto_args)
    } else {
        spawn_posix(&base, &editor_args, &goto_args)
    }
}

fn external_editor() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            if cfg!(windows) {
                Some(String::from("notepad"))
            } else {
                Some(String::from("code"))
            }
        })
}

fn classify_gui_editor(editor: &str) -> Option<&'static str> {
    let base = split_editor_command(editor).0.to_lowercase();
    let filename = base
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(base.as_str())
        .to_string();
    GUI_EDITORS
        .iter()
        .copied()
        .find(|candidate| filename.contains(candidate))
}

fn split_editor_command(editor: &str) -> (String, Vec<String>) {
    let parts: Vec<String> = editor.split_whitespace().map(ToString::to_string).collect();
    let base = parts.first().cloned().unwrap_or_else(|| editor.to_string());
    let args = if parts.len() > 1 {
        parts[1..].to_vec()
    } else {
        Vec::new()
    };
    (base, args)
}

fn gui_goto_argv(gui_family: &str, path: &Path, line: Option<u64>) -> Vec<OsString> {
    let path_str = path.as_os_str().to_os_string();
    match (line, gui_family) {
        (Some(line), family) if VSCODE_FAMILY.contains(&family) => {
            vec![
                OsString::from("-g"),
                OsString::from(format!("{}:{line}", path.display())),
            ]
        }
        (Some(line), "subl") => vec![OsString::from(format!("{}:{line}", path.display()))],
        _ => vec![path_str],
    }
}

fn run_editor_waiting(editor: &str, path: &Path) -> anyhow::Result<()> {
    let (base, editor_args) = split_editor_command(editor);
    let status = Command::new(&base)
        .args(editor_args)
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch external editor {base}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("external editor {base} exited with {status}"))
    }
}

fn external_edit_temp_path() -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("rebon-agent-view-{millis}.md"))
}

fn spawn_windows(base: &str, editor_args: &[String], goto_args: &[OsString]) -> bool {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C")
        .arg("start")
        .arg("")
        .arg(base)
        .args(editor_args)
        .args(goto_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().map(|_| true).unwrap_or_else(|err| {
        tracing::warn!(error = %err, editor = %base, "rebon-cli: failed to launch external editor");
        false
    })
}

fn spawn_posix(base: &str, editor_args: &[String], goto_args: &[OsString]) -> bool {
    let mut cmd = Command::new(base);
    cmd.args(editor_args)
        .args(goto_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().map(|_| true).unwrap_or_else(|err| {
        tracing::warn!(error = %err, editor = %base, "rebon-cli: failed to launch external editor");
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_gui_editor_matches_vscode_family() {
        assert_eq!(classify_gui_editor("code"), Some("code"));
        assert_eq!(classify_gui_editor("cursor --wait"), Some("cursor"));
        assert_eq!(
            classify_gui_editor("C:\\tools\\windsurf.exe"),
            Some("windsurf")
        );
    }

    #[test]
    fn classify_gui_editor_rejects_terminal_editors() {
        assert_eq!(classify_gui_editor("vim"), None);
        assert_eq!(classify_gui_editor("nvim"), None);
    }

    #[test]
    fn split_editor_command_preserves_extra_args() {
        let (base, args) = split_editor_command("code --reuse-window");
        assert_eq!(base, "code");
        assert_eq!(args, vec!["--reuse-window".to_string()]);
    }

    #[test]
    fn vscode_family_uses_dash_g() {
        let args = gui_goto_argv("code", &PathBuf::from("src/main.rs"), Some(17));
        assert_eq!(args[0], OsString::from("-g"));
        assert_eq!(args[1], OsString::from("src/main.rs:17"));
    }

    #[test]
    fn non_line_editor_just_opens_path() {
        let args = gui_goto_argv("notepad", &PathBuf::from("README.md"), Some(3));
        assert_eq!(args, vec![OsString::from("README.md")]);
    }
}
