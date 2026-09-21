//! Memory content reading for per-turn surfacing.
//!
//! [`read_memories_for_surfacing`] reads selected memory files with line/byte limits, prepends
//! freshness headers, and marks truncated files.

use std::path::Path;

use crate::memory::age::{memory_age, memory_freshness_text};

/// Maximum lines to read from a single memory file.
const MAX_MEMORY_LINES: usize = 200;

/// Maximum bytes to read from a single memory file.
const MAX_MEMORY_BYTES: usize = 20_000;

/// A memory file prepared for injection into the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfacedMemory {
    /// Absolute path of the memory file.
    pub path: String,
    /// Header line (path + age + optional freshness caveat).
    pub header: String,
    /// File content (possibly truncated).
    pub content: String,
    /// Last-modified timestamp in milliseconds.
    pub mtime_ms: i64,
    /// Whether the content was truncated.
    pub was_truncated: bool,
}

/// Build the header line for a surfaced memory.
///
/// Format: `path (last updated: <age>)\n<freshness caveat if stale>`
pub fn memory_header(path: &str, mtime_ms: i64) -> String {
    let age = memory_age(mtime_ms);
    let freshness = memory_freshness_text(mtime_ms);
    if freshness.is_empty() {
        format!("{path} (last updated: {age})")
    } else {
        format!("{path} (last updated: {age})\n{freshness}")
    }
}

/// Read a memory file for surfacing, applying line and byte limits.
///
/// Returns `None` if the file cannot be read.
pub fn read_memory_for_surfacing(path: &Path, mtime_ms: i64) -> Option<SurfacedMemory> {
    let raw = std::fs::read_to_string(path).ok()?;
    let path_str = path.to_string_lossy().replace('\\', "/");
    let header = memory_header(&path_str, mtime_ms);

    let (content, was_truncated) = truncate_content(&raw, MAX_MEMORY_LINES, MAX_MEMORY_BYTES);

    let content = if was_truncated {
        format!(
            "{content}\n\n> This memory file was truncated at {MAX_MEMORY_LINES} lines / \
             {MAX_MEMORY_BYTES} bytes. Use the Read tool to view the complete file at `{path_str}`."
        )
    } else {
        content
    };

    Some(SurfacedMemory {
        path: path_str,
        header,
        content,
        mtime_ms,
        was_truncated,
    })
}

/// Read multiple memory files for surfacing.
///
/// Skips files that cannot be read. Returns in the same order as input.
pub fn read_memories_for_surfacing(memories: &[(impl AsRef<Path>, i64)]) -> Vec<SurfacedMemory> {
    memories
        .iter()
        .filter_map(|(path, mtime_ms)| read_memory_for_surfacing(path.as_ref(), *mtime_ms))
        .collect()
}

/// Format a surfaced memory as a system-reminder block for injection.
///
/// Wraps the header + content in `<system-reminder>` tags.
pub fn format_memory_attachment(memory: &SurfacedMemory) -> String {
    format!(
        "<system-reminder>\n{}\n\n{}\n</system-reminder>",
        memory.header, memory.content
    )
}

/// Format multiple surfaced memories as a single system-reminder block.
pub fn format_memory_attachments(memories: &[SurfacedMemory]) -> String {
    if memories.is_empty() {
        return String::new();
    }
    memories
        .iter()
        .map(format_memory_attachment)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Truncate content to line and byte limits.
/// Returns (truncated_content, was_truncated).
fn truncate_content(raw: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = raw.lines().collect();
    let line_truncated = lines.len() > max_lines;
    let byte_truncated = raw.len() > max_bytes;

    if !line_truncated && !byte_truncated {
        return (raw.to_string(), false);
    }

    let mut result = if line_truncated {
        lines[..max_lines].join("\n")
    } else {
        raw.to_string()
    };

    if result.len() > max_bytes {
        // Truncate at last newline before byte limit
        if let Some(cut) = result[..max_bytes].rfind('\n') {
            result.truncate(cut);
        } else {
            result.truncate(max_bytes);
        }
    }

    (result, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn memory_header_fresh() {
        let header = memory_header("/test/user.md", now_ms());
        assert!(header.contains("last updated: today"));
        assert!(!header.contains("days old"));
    }

    #[test]
    fn memory_header_stale() {
        let old = now_ms() - 10 * 86_400_000;
        let header = memory_header("/test/user.md", old);
        assert!(header.contains("last updated: 10 days ago"));
        assert!(header.contains("10 days old"));
    }

    #[test]
    fn truncate_within_limits() {
        let content = "line 1\nline 2\nline 3";
        let (result, truncated) = truncate_content(content, 100, 100_000);
        assert_eq!(result, content);
        assert!(!truncated);
    }

    #[test]
    fn truncate_over_line_limit() {
        let lines: Vec<String> = (0..300).map(|i| format!("line {i}")).collect();
        let content = lines.join("\n");
        let (result, truncated) = truncate_content(&content, 200, 100_000);
        assert!(truncated);
        assert_eq!(result.lines().count(), 200);
    }

    #[test]
    fn truncate_over_byte_limit() {
        let content = "x".repeat(30_000);
        let (result, truncated) = truncate_content(&content, 1000, 20_000);
        assert!(truncated);
        assert!(result.len() <= 20_000);
    }

    #[test]
    fn read_memory_for_surfacing_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        fs::write(&path, "---\nname: test\n---\nSome content").unwrap();
        let result = read_memory_for_surfacing(&path, now_ms());
        assert!(result.is_some());
        let mem = result.unwrap();
        assert!(mem.content.contains("Some content"));
        assert!(!mem.was_truncated);
    }

    #[test]
    fn read_memory_for_surfacing_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.md");
        let lines: Vec<String> = (0..300).map(|i| format!("line {i}")).collect();
        fs::write(&path, lines.join("\n")).unwrap();
        let result = read_memory_for_surfacing(&path, now_ms()).unwrap();
        assert!(result.was_truncated);
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let result = read_memory_for_surfacing(Path::new("/nonexistent"), now_ms());
        assert!(result.is_none());
    }

    #[test]
    fn format_attachment_wraps_in_system_reminder() {
        let mem = SurfacedMemory {
            path: "/test/user.md".to_string(),
            header: "header line".to_string(),
            content: "memory content".to_string(),
            mtime_ms: now_ms(),
            was_truncated: false,
        };
        let formatted = format_memory_attachment(&mem);
        assert!(formatted.starts_with("<system-reminder>"));
        assert!(formatted.ends_with("</system-reminder>"));
        assert!(formatted.contains("header line"));
        assert!(formatted.contains("memory content"));
    }

    #[test]
    fn format_attachments_empty() {
        assert!(format_memory_attachments(&[]).is_empty());
    }
}
