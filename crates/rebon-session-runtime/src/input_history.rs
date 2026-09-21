use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use rebon_types::PromptPasteContent;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_HISTORY_ITEMS: usize = 100;
const MAX_INLINE_PASTED_CONTENT_LEN: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HistoryLogEntry {
    pub display: String,
    #[serde(default, rename = "pastedContents")]
    pub pasted_contents: BTreeMap<u32, StoredPastedContent>,
    pub timestamp: u64,
    pub project: String,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredPastedContent {
    pub id: u32,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, rename = "contentHash")]
    pub content_hash: Option<String>,
    #[serde(default, rename = "mediaType")]
    pub media_type: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub display: String,
    pub pasted_contents: Vec<PromptPasteContent>,
    pub timestamp: u64,
}

pub fn history_path(config_home: &Path) -> PathBuf {
    config_home.join("history.jsonl")
}

pub(crate) fn paste_cache_path(config_home: &Path, hash: &str) -> PathBuf {
    config_home.join("paste-cache").join(format!("{hash}.txt"))
}

pub(crate) fn image_paste_cache_path(config_home: &Path, hash: &str) -> PathBuf {
    config_home.join("paste-cache").join(format!("{hash}.b64"))
}

pub(crate) fn hash_pasted_text(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn append_history(
    config_home: &Path,
    project: &str,
    session_id: &str,
    entry: &HistoryEntry,
) -> io::Result<bool> {
    if prompt_history_skip_enabled() {
        return Ok(false);
    }

    std::fs::create_dir_all(config_home)?;
    let log_entry = HistoryLogEntry {
        display: entry.display.clone(),
        pasted_contents: stored_pasted_contents(config_home, &entry.pasted_contents),
        timestamp: entry.timestamp,
        project: project.to_string(),
        session_id: Some(session_id.to_string()),
    };

    let path = history_path(config_home);
    let mut file = open_private_append_file(&path)?;
    file.lock_exclusive()?;
    let write_result = (|| {
        let line = serde_json::to_string(&log_entry).map_err(io::Error::other)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()
    })();
    let unlock_result = file.unlock();
    write_result.and(unlock_result)?;
    set_private_file_permissions(&path)?;
    Ok(true)
}

pub fn load_history(
    config_home: &Path,
    current_project: &str,
    current_session: &str,
) -> io::Result<Vec<HistoryEntry>> {
    let path = history_path(config_home);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut current_session_entries = Vec::new();
    let mut other_session_entries = Vec::new();

    for line in contents.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<HistoryLogEntry>(line) else {
            continue;
        };
        if entry.project != current_project {
            continue;
        }

        if entry.session_id.as_deref() == Some(current_session) {
            current_session_entries.push(log_entry_to_history_entry(config_home, entry));
        } else {
            other_session_entries.push(log_entry_to_history_entry(config_home, entry));
        }

        if current_session_entries.len() + other_session_entries.len() >= MAX_HISTORY_ITEMS {
            break;
        }
    }

    current_session_entries.extend(other_session_entries);
    Ok(current_session_entries)
}

pub(crate) fn resolve_pasted_contents(
    config_home: &Path,
    stored: &BTreeMap<u32, StoredPastedContent>,
) -> Vec<PromptPasteContent> {
    let mut resolved = Vec::with_capacity(stored.len());
    for content in stored.values() {
        let payload = if let Some(inline) = content.content.clone() {
            Some(inline)
        } else if let Some(hash) = content.content_hash.as_deref() {
            if content.r#type == "image" {
                std::fs::read_to_string(image_paste_cache_path(config_home, hash)).ok()
            } else {
                std::fs::read_to_string(paste_cache_path(config_home, hash)).ok()
            }
        } else {
            None
        };

        if let Some(payload) = payload {
            resolved.push(PromptPasteContent {
                id: content.id,
                kind: content.r#type.clone(),
                content: payload,
                media_type: content.media_type.clone(),
                filename: content.filename.clone(),
                source_path: None,
            });
        }
    }
    resolved
}

fn log_entry_to_history_entry(config_home: &Path, entry: HistoryLogEntry) -> HistoryEntry {
    let pasted_contents = resolve_pasted_contents(config_home, &entry.pasted_contents);
    HistoryEntry {
        display: entry.display,
        pasted_contents,
        timestamp: entry.timestamp,
    }
}

fn stored_pasted_contents(
    config_home: &Path,
    pasted_contents: &[PromptPasteContent],
) -> BTreeMap<u32, StoredPastedContent> {
    let mut stored = BTreeMap::new();
    for content in pasted_contents {
        let max_inline_len = if content.kind == "image" {
            0
        } else {
            MAX_INLINE_PASTED_CONTENT_LEN
        };

        let stored_content = if content.content.len() <= max_inline_len {
            StoredPastedContent {
                id: content.id,
                r#type: content.kind.clone(),
                content: Some(content.content.clone()),
                content_hash: None,
                media_type: content.media_type.clone(),
                filename: content.filename.clone(),
            }
        } else {
            let hash = hash_pasted_text(&content.content);
            let store_result = if content.kind == "image" {
                store_pasted_image(config_home, &hash, &content.content)
            } else {
                store_pasted_text(config_home, &hash, &content.content)
            };
            let _ = store_result;
            StoredPastedContent {
                id: content.id,
                r#type: content.kind.clone(),
                content: None,
                content_hash: Some(hash),
                media_type: content.media_type.clone(),
                filename: content.filename.clone(),
            }
        };
        stored.insert(content.id, stored_content);
    }
    stored
}

fn store_pasted_text(config_home: &Path, hash: &str, content: &str) -> io::Result<()> {
    let path = paste_cache_path(config_home, hash);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = open_private_write_file(&path)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    set_private_file_permissions(&path)
}

fn store_pasted_image(config_home: &Path, hash: &str, content: &str) -> io::Result<()> {
    let path = image_paste_cache_path(config_home, hash);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = open_private_write_file(&path)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    set_private_file_permissions(&path)
}

fn prompt_history_skip_enabled() -> bool {
    rebon_types::env::env_truthy("REBON_SKIP_PROMPT_HISTORY")
}

fn open_private_append_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn open_private_write_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn paste(id: u32, content: &str) -> PromptPasteContent {
        PromptPasteContent {
            id,
            kind: "text".into(),
            content: content.into(),
            media_type: None,
            filename: None,
            source_path: None,
        }
    }

    fn image(id: u32) -> PromptPasteContent {
        PromptPasteContent {
            id,
            kind: "image".into(),
            content: "BASE64".into(),
            media_type: Some("image/png".into()),
            filename: Some("image.png".into()),
            source_path: None,
        }
    }

    fn entry(display: &str, timestamp: u64) -> HistoryEntry {
        HistoryEntry {
            display: display.into(),
            pasted_contents: Vec::new(),
            timestamp,
        }
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn append_for_test(
        config_home: &Path,
        project: &str,
        session_id: &str,
        entry: &HistoryEntry,
    ) -> io::Result<bool> {
        let _guard = env_lock().lock().unwrap();
        let previous = std::env::var_os("REBON_SKIP_PROMPT_HISTORY");
        std::env::remove_var("REBON_SKIP_PROMPT_HISTORY");
        let result = append_history(config_home, project, session_id, entry);
        if let Some(previous) = previous {
            std::env::set_var("REBON_SKIP_PROMPT_HISTORY", previous);
        }
        result
    }

    #[test]
    fn append_and_load_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        append_for_test(dir.path(), "project", "session", &entry("hello", 1)).unwrap();

        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].display, "hello");
        assert_eq!(loaded[0].timestamp, 1);
    }

    #[test]
    fn bad_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(history_path(dir.path()), "not json\n").unwrap();
        append_for_test(dir.path(), "project", "session", &entry("valid", 1)).unwrap();

        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|e| e.display.as_str())
                .collect::<Vec<_>>(),
            vec!["valid"]
        );
    }

    #[test]
    fn filters_by_project() {
        let dir = tempfile::tempdir().unwrap();
        append_for_test(dir.path(), "other", "session", &entry("hidden", 1)).unwrap();
        append_for_test(dir.path(), "project", "session", &entry("visible", 2)).unwrap();

        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|e| e.display.as_str())
                .collect::<Vec<_>>(),
            vec!["visible"]
        );
    }

    #[test]
    fn current_session_is_loaded_before_other_sessions() {
        let dir = tempfile::tempdir().unwrap();
        append_for_test(dir.path(), "project", "other", &entry("other-old", 1)).unwrap();
        append_for_test(dir.path(), "project", "current", &entry("current-old", 2)).unwrap();
        append_for_test(dir.path(), "project", "other", &entry("other-new", 3)).unwrap();
        append_for_test(dir.path(), "project", "current", &entry("current-new", 4)).unwrap();

        let loaded = load_history(dir.path(), "project", "current").unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|e| e.display.as_str())
                .collect::<Vec<_>>(),
            vec!["current-new", "current-old", "other-new", "other-old"]
        );
    }

    #[test]
    fn load_caps_to_recent_window() {
        let dir = tempfile::tempdir().unwrap();
        for idx in 0..105 {
            append_for_test(
                dir.path(),
                "project",
                "session",
                &entry(&format!("item-{idx}"), idx),
            )
            .unwrap();
        }

        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(loaded.len(), 100);
        assert_eq!(loaded[0].display, "item-104");
        assert_eq!(loaded[99].display, "item-5");
    }

    #[test]
    fn large_paste_writes_hash_and_resolves_from_cache() {
        let dir = tempfile::tempdir().unwrap();
        let large = "x".repeat(MAX_INLINE_PASTED_CONTENT_LEN + 1);
        let mut entry = entry("[Pasted text #1]", 1);
        entry.pasted_contents.push(paste(1, &large));

        append_for_test(dir.path(), "project", "session", &entry).unwrap();

        let hash = hash_pasted_text(&large);
        assert_eq!(
            std::fs::read_to_string(paste_cache_path(dir.path(), &hash)).unwrap(),
            large
        );
        let line = std::fs::read_to_string(history_path(dir.path())).unwrap();
        assert!(line.contains("contentHash"));
        assert!(!line.contains(&large));

        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(loaded[0].pasted_contents[0].content, large);
    }

    #[test]
    fn inline_paste_resolves_without_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = entry("[Pasted text #1]", 1);
        entry.pasted_contents.push(paste(1, "hello"));

        append_for_test(dir.path(), "project", "session", &entry).unwrap();

        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(loaded[0].pasted_contents[0].content, "hello");
        assert_eq!(loaded[0].pasted_contents[0].id, 1);
    }

    #[test]
    fn missing_hash_file_skips_payload_but_keeps_display() {
        let dir = tempfile::tempdir().unwrap();
        let mut stored = BTreeMap::new();
        stored.insert(
            1,
            StoredPastedContent {
                id: 1,
                r#type: "text".into(),
                content: None,
                content_hash: Some("missing".into()),
                media_type: None,
                filename: None,
            },
        );
        let line = serde_json::to_string(&HistoryLogEntry {
            display: "[Pasted text #1]".into(),
            pasted_contents: stored,
            timestamp: 1,
            project: "project".into(),
            session_id: Some("session".into()),
        })
        .unwrap();
        std::fs::write(history_path(dir.path()), format!("{line}\n")).unwrap();

        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(loaded[0].display, "[Pasted text #1]");
        assert!(loaded[0].pasted_contents.is_empty());
    }

    #[test]
    fn image_payloads_are_cached_and_restored() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = entry("[Image #1]", 1);
        entry.pasted_contents.push(image(1));

        append_for_test(dir.path(), "project", "session", &entry).unwrap();

        let line = std::fs::read_to_string(history_path(dir.path())).unwrap();
        assert!(line.contains("contentHash"));
        assert!(!line.contains("BASE64"));
        let loaded = load_history(dir.path(), "project", "session").unwrap();
        assert_eq!(loaded[0].pasted_contents.len(), 1);
        assert_eq!(loaded[0].pasted_contents[0].kind, "image");
        assert_eq!(loaded[0].pasted_contents[0].content, "BASE64");
        assert_eq!(
            loaded[0].pasted_contents[0].media_type.as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn skip_env_prevents_disk_append() {
        let _guard = env_lock().lock().unwrap();
        let previous = std::env::var_os("REBON_SKIP_PROMPT_HISTORY");
        std::env::set_var("REBON_SKIP_PROMPT_HISTORY", "true");
        let dir = tempfile::tempdir().unwrap();

        let wrote = append_history(dir.path(), "project", "session", &entry("hidden", 1)).unwrap();

        assert!(!wrote);
        assert!(!history_path(dir.path()).exists());
        if let Some(previous) = previous {
            std::env::set_var("REBON_SKIP_PROMPT_HISTORY", previous);
        } else {
            std::env::remove_var("REBON_SKIP_PROMPT_HISTORY");
        }
    }
}
