use std::path::{Path, PathBuf};

use super::{read_tail_lines_bounded, BackgroundJobState};

const TRANSCRIPT_PREVIEW_MAX_SCAN_LINES: usize = 2048;
const TRANSCRIPT_PREVIEW_MAX_SCAN_BYTES: usize = 512 * 1024;

fn effective_background_job_cwd(state: &BackgroundJobState) -> String {
    preserved_background_worktree_path(state)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| state.identity.cwd.clone())
}

pub fn background_job_transcript_cwd(state: &BackgroundJobState) -> String {
    background_job_transcript_cwd_in(&rebon_session::default_projects_root(), state)
}

fn background_job_transcript_cwd_in(projects_root: &Path, state: &BackgroundJobState) -> String {
    let effective_cwd = effective_background_job_cwd(state);
    let Some(session_id) = state.identity.session_id.as_deref() else {
        return effective_cwd;
    };

    if transcript_file_exists(projects_root, &effective_cwd, session_id) {
        return effective_cwd;
    }

    if state.identity.cwd != effective_cwd
        && transcript_file_exists(projects_root, &state.identity.cwd, session_id)
    {
        return state.identity.cwd.clone();
    }

    rebon_session::find_session_transcript_cwd(projects_root, session_id).unwrap_or(effective_cwd)
}

fn transcript_file_exists(projects_root: &Path, cwd: &str, session_id: &str) -> bool {
    rebon_session::transcript_file_path(projects_root, cwd, session_id).is_file()
}

/// Where this job's conversation is written, under `projects_root`.
///
/// `None` until the worker has given the job a session. The cwd half is
/// [`background_job_transcript_cwd`]'s answer, so a job that ran in a
/// preserved worktree — or whose transcript was moved — is found where the
/// rest of the host finds it. The root is a parameter because a reader that
/// is handed its config home must not fall back to the process's own.
pub fn background_job_transcript_path_in(
    projects_root: &Path,
    state: &BackgroundJobState,
) -> Option<PathBuf> {
    let session_id = state.identity.session_id.as_deref()?;
    let cwd = background_job_transcript_cwd_in(projects_root, state);
    Some(rebon_session::transcript_file_path(
        projects_root,
        &cwd,
        session_id,
    ))
}

pub fn read_transcript_preview(state: &BackgroundJobState, max_entries: usize) -> Vec<String> {
    if max_entries == 0 {
        return Vec::new();
    }
    let Some(session_id) = state.identity.session_id.as_deref() else {
        return Vec::new();
    };
    let cwd = background_job_transcript_cwd(state);
    let projects_root = rebon_session::default_projects_root();
    let path = rebon_session::transcript_file_path(&projects_root, &cwd, session_id);
    let scan_lines = max_entries
        .saturating_mul(16)
        .max(64)
        .min(TRANSCRIPT_PREVIEW_MAX_SCAN_LINES);
    let Ok(lines) = read_tail_lines_bounded(&path, scan_lines, TRANSCRIPT_PREVIEW_MAX_SCAN_BYTES)
    else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in lines.iter().rev() {
        if out.len() >= max_entries {
            break;
        }
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let label = match raw.get("type").and_then(serde_json::Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if transcript_entry_is_meta(&raw) {
            continue;
        }
        let Some(text) = extract_transcript_entry_text(&raw) else {
            continue;
        };
        let excerpt = shorten_excerpt(&text, 120);
        if excerpt.is_empty() {
            continue;
        }
        out.push(format!("{label}: {excerpt}"));
    }
    out.reverse();
    out
}

fn transcript_entry_is_meta(raw: &serde_json::Value) -> bool {
    let top = raw
        .get("isMeta")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let nested = raw
        .get("message")
        .and_then(|m| m.get("isMeta"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let runtime_context = raw
        .get("runtimeContext")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    top || nested || runtime_context
}

fn extract_transcript_entry_text(raw: &serde_json::Value) -> Option<String> {
    let content = raw.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return non_empty_transcript_excerpt(s);
    }
    let arr = content.as_array()?;
    let mut buf = String::new();
    for block in arr {
        let Some(obj) = block.as_object() else {
            continue;
        };
        let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                    if !buf.is_empty() {
                        buf.push(' ');
                    }
                    buf.push_str(text);
                }
            }
            "tool_use" => {
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push('[');
                buf.push_str(name);
                buf.push(']');
            }
            _ => continue,
        }
    }
    non_empty_transcript_excerpt(&buf)
}

fn non_empty_transcript_excerpt(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("<system-reminder>") {
        return None;
    }
    Some(trimmed.to_string())
}

pub fn shorten_excerpt(text: &str, max_chars: usize) -> String {
    shorten_labelled_excerpt("", text, max_chars)
}

/// `shorten_excerpt` over `"<label> <text>"` without ever building that string.
///
/// Callers summarise assistant snapshots and terminal chunks — inputs that run
/// to megabytes — down to a couple of hundred characters, and they do it on
/// every refresh. The obvious spelling of both halves,
/// `format!("{label}: {}", text.split_whitespace().collect::<Vec<_>>().join(" "))`
/// followed by a collapse inside `shorten_excerpt`, walks the whole input four
/// times and allocates a word-pointer vector plus three full copies of it, then
/// throws all but 180 characters away. That made this the single largest
/// allocator in the app.
///
/// This walks the input once and stops as soon as it has enough characters to
/// decide whether an ellipsis is needed, so the peak allocation is `max_chars`,
/// not the length of the input. The result is byte-identical to the old
/// spelling: `label` is collapsed together with `text`, so an empty `text`
/// yields a bare label with no trailing space.
pub fn shorten_labelled_excerpt(label: &str, text: &str, max_chars: usize) -> String {
    // One character past the limit is exactly enough to prove the input
    // overflowed while still holding everything that survives truncation.
    let cap = max_chars.saturating_add(1);
    let mut collapsed = String::new();
    let mut chars = 0usize;
    'words: for word in label.split_whitespace().chain(text.split_whitespace()) {
        if !collapsed.is_empty() {
            if chars >= cap {
                break;
            }
            collapsed.push(' ');
            chars += 1;
        }
        for ch in word.chars() {
            if chars >= cap {
                break 'words;
            }
            collapsed.push(ch);
            chars += 1;
        }
    }
    // Never capped, so this is the whole collapsed input.
    if chars <= max_chars {
        return collapsed;
    }
    let truncated: String = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    format!("{truncated}…")
}

pub fn preserved_background_worktree_path(state: &BackgroundJobState) -> Option<PathBuf> {
    let path = PathBuf::from(state.workspace.worktree_path.as_deref()?);
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::{BackgroundRuntimeFields, BackgroundStore};

    fn write_transcript_marker(projects_root: &Path, cwd: &str, session_id: &str) {
        let path = rebon_session::ensure_session_file_path(projects_root, cwd, session_id).unwrap();
        fs::write(
            path,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-05-19T00:00:00.000Z","message":null}"#,
        )
        .unwrap();
    }

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path().join(".rebon"));
        (dir, store)
    }

    fn runtime() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }

    #[test]
    fn effective_background_job_cwd_prefers_existing_worktree() {
        let (_dir, store) = store();
        let worktree = tempfile::tempdir().unwrap();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("/repo/project"), runtime())
            .unwrap();
        state.workspace.worktree_path = Some(worktree.path().to_string_lossy().to_string());

        assert_eq!(
            effective_background_job_cwd(&state),
            state.workspace.worktree_path.clone().unwrap()
        );
    }

    #[test]
    fn transcript_cwd_falls_back_to_original_cwd_when_worktree_has_no_session_file() {
        let (_dir, store) = store();
        let projects_root = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("/repo/project"), runtime())
            .unwrap();
        state.identity.session_id = Some("sess-existing".into());
        state.workspace.worktree_path = Some(worktree.path().to_string_lossy().to_string());
        write_transcript_marker(projects_root.path(), &state.identity.cwd, "sess-existing");

        assert_eq!(
            background_job_transcript_cwd_in(projects_root.path(), &state),
            state.identity.cwd
        );
    }

    #[test]
    fn transcript_path_is_none_before_a_session_and_follows_the_cwd_rule_after() {
        let (_dir, store) = store();
        let projects_root = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let worktree_cwd = worktree.path().to_string_lossy().to_string();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("/repo/project"), runtime())
            .unwrap();
        assert_eq!(
            background_job_transcript_path_in(projects_root.path(), &state),
            None,
            "a job the worker has not claimed has no conversation yet"
        );

        state.identity.session_id = Some("sess-path".into());
        state.workspace.worktree_path = Some(worktree_cwd.clone());
        write_transcript_marker(projects_root.path(), &worktree_cwd, "sess-path");
        assert_eq!(
            background_job_transcript_path_in(projects_root.path(), &state),
            Some(rebon_session::transcript_file_path(
                projects_root.path(),
                &worktree_cwd,
                "sess-path"
            )),
        );
    }

    #[test]
    fn transcript_cwd_keeps_worktree_when_session_file_lives_there() {
        let (_dir, store) = store();
        let projects_root = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let worktree_cwd = worktree.path().to_string_lossy().to_string();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("/repo/project"), runtime())
            .unwrap();
        state.identity.session_id = Some("sess-worktree".into());
        state.workspace.worktree_path = Some(worktree_cwd.clone());
        write_transcript_marker(projects_root.path(), &worktree_cwd, "sess-worktree");

        assert_eq!(
            background_job_transcript_cwd_in(projects_root.path(), &state),
            worktree_cwd
        );
    }

    #[test]
    fn transcript_cwd_recovers_removed_worktree_from_project_sidecar() {
        let (_dir, store) = store();
        let projects_root = tempfile::tempdir().unwrap();
        let removed_worktree_cwd = "/repo/.rebon/worktrees/bg-job";
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("/repo/project"), runtime())
            .unwrap();
        state.identity.session_id = Some("sess-removed-worktree".into());
        state.workspace.worktree_path = None;
        write_transcript_marker(
            projects_root.path(),
            removed_worktree_cwd,
            "sess-removed-worktree",
        );

        assert_eq!(
            background_job_transcript_cwd_in(projects_root.path(), &state),
            removed_worktree_cwd
        );
    }

    #[test]
    fn transcript_cwd_does_not_guess_between_duplicate_session_files() {
        let (_dir, store) = store();
        let projects_root = tempfile::tempdir().unwrap();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("/repo/project"), runtime())
            .unwrap();
        state.identity.session_id = Some("sess-duplicate".into());
        write_transcript_marker(projects_root.path(), "/repo/worktree-a", "sess-duplicate");
        write_transcript_marker(projects_root.path(), "/repo/worktree-b", "sess-duplicate");

        assert_eq!(
            background_job_transcript_cwd_in(projects_root.path(), &state),
            state.identity.cwd
        );
    }

    #[test]
    fn extract_transcript_entry_text_handles_string_content() {
        let raw = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "hello there" },
        });
        assert_eq!(
            extract_transcript_entry_text(&raw).as_deref(),
            Some("hello there")
        );
    }

    #[test]
    fn extract_transcript_entry_text_handles_array_with_text_and_tool_use() {
        let raw = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "let me check" },
                    { "type": "tool_use", "id": "tu_1", "name": "Read", "input": {} },
                ],
            },
        });
        assert_eq!(
            extract_transcript_entry_text(&raw).as_deref(),
            Some("let me check [Read]")
        );
    }

    #[test]
    fn extract_transcript_entry_text_rejects_system_reminder_only() {
        let raw = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "<system-reminder>noise</system-reminder>" },
        });
        assert!(extract_transcript_entry_text(&raw).is_none());
    }

    #[test]
    fn transcript_entry_is_meta_detects_nested_flag() {
        let raw = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "x", "isMeta": true },
        });
        assert!(transcript_entry_is_meta(&raw));
    }

    #[test]
    fn transcript_entry_is_meta_detects_runtime_context_flag() {
        let raw = serde_json::json!({
            "type": "user",
            "runtimeContext": true,
            "message": { "role": "user", "content": "ctx" },
        });
        assert!(transcript_entry_is_meta(&raw));
    }

    #[test]
    fn shorten_excerpt_collapses_whitespace_and_truncates() {
        let collapsed = shorten_excerpt("hello\n\nworld\t  foo", 100);
        assert_eq!(collapsed, "hello world foo");

        let truncated = shorten_excerpt("abcdefghij", 5);
        assert_eq!(truncated.chars().count(), 5);
        assert!(truncated.ends_with('…'));
    }

    /// The single-pass collapse replaced
    /// `shorten_excerpt(&format!("{label} {}", collapse(text)), n)`, so it has to
    /// agree with that spelling everywhere — including the corners where the
    /// difference would be a stray space or a lost ellipsis.
    fn eager_reference(label: &str, text: &str, max_chars: usize) -> String {
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let joined = if label.is_empty() {
            collapsed
        } else {
            format!("{label} {collapsed}")
        };
        let collapsed: String = joined.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.chars().count() <= max_chars {
            return collapsed;
        }
        let truncated: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        format!("{truncated}…")
    }

    #[test]
    fn labelled_excerpt_matches_the_eager_spelling() {
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("User:", ""),
            ("User:", "   \n\t "),
            ("User:", "hello"),
            ("", "hello\n\nworld\t  foo"),
            ("Thinking:", "hello\n\nworld\t  foo"),
            (
                "stdout:",
                "a b c d e f g h i j k l m n o p q r s t u v w x y z",
            ),
            (
                "agent idle after error:",
                "多字节 字符 也要 按字符 截断 而不是 字节",
            ),
        ];
        for (label, text) in cases {
            for max_chars in [0usize, 1, 2, 5, 12, 180] {
                assert_eq!(
                    shorten_labelled_excerpt(label, text, max_chars),
                    eager_reference(label, text, max_chars),
                    "label={label:?} text={text:?} max_chars={max_chars}"
                );
            }
        }
    }

    /// The point of the rewrite: a megabyte of input costs `max_chars`, not a
    /// megabyte, so summarising a growing assistant snapshot stays linear.
    #[test]
    fn labelled_excerpt_does_not_walk_past_the_limit() {
        let huge = "word ".repeat(200_000);
        let summary = shorten_labelled_excerpt("stdout:", &huge, 180);
        assert_eq!(summary.chars().count(), 180);
        assert!(summary.starts_with("stdout: word"));
        assert!(summary.ends_with('…'));
        assert_eq!(summary, eager_reference("stdout:", &huge, 180));
    }
}
