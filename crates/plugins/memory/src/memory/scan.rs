//! Memory directory scanning.
//!
//! Scans a memory directory for
//! `.md` files, reads their frontmatter headers, and returns a sorted
//! list capped at [`MAX_MEMORY_FILES`].

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rebon_instructions::frontmatter::parse_frontmatter;

/// Maximum number of memory files to return from a scan.
const MAX_MEMORY_FILES: usize = 200;

/// Maximum lines to read for frontmatter extraction.
const FRONTMATTER_MAX_BYTES: usize = 4096;

/// Header information extracted from a memory file.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHeader {
    /// Filename relative to the memory directory (e.g. `user_role.md`).
    pub filename: String,
    /// Absolute file path.
    pub file_path: PathBuf,
    /// Last-modified timestamp in milliseconds since epoch.
    pub mtime_ms: i64,
    /// Description from frontmatter, if present.
    pub description: Option<String>,
    /// Memory type from frontmatter (user/feedback/project/reference).
    pub memory_type: Option<String>,
}

/// Scan a memory directory for `.md` files (excluding MEMORY.md),
/// read their frontmatter, and return headers sorted newest-first.
///
/// Caps at [`MAX_MEMORY_FILES`] entries. Errors reading individual
/// files are silently skipped.
pub fn scan_memory_files(memory_dir: &Path) -> Vec<MemoryHeader> {
    if !memory_dir.is_dir() {
        return Vec::new();
    }

    let mut headers: Vec<MemoryHeader> = Vec::new();

    // Collect .md files (flat + one level of subdirectories).
    process_dir(memory_dir, memory_dir, &mut headers);
    if let Ok(entries) = std::fs::read_dir(memory_dir) {
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        subdirs.sort();
        for subdir in subdirs {
            process_dir(memory_dir, &subdir, &mut headers);
        }
    }

    // Sort newest-first, with filename as deterministic tie-breaker, cap at MAX_MEMORY_FILES
    headers.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then_with(|| a.filename.cmp(&b.filename))
    });
    headers.truncate(MAX_MEMORY_FILES);
    headers
}

/// Process one directory, extracting headers from .md files.
fn process_dir(base_dir: &Path, current_dir: &Path, headers: &mut Vec<MemoryHeader>) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };
    process_entries(base_dir, current_dir, entries, headers);
}

fn process_entries(
    base_dir: &Path,
    current_dir: &Path,
    entries: std::fs::ReadDir,
    headers: &mut Vec<MemoryHeader>,
) {
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !file_name.ends_with(".md") || file_name == "MEMORY.md" {
            continue;
        }

        // Get relative filename
        let relative = path
            .strip_prefix(base_dir)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(file_name);

        // Get mtime
        let mtime_ms = path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Read frontmatter (only first FRONTMATTER_MAX_BYTES)
        let content = read_file_head(&path, FRONTMATTER_MAX_BYTES);
        let parsed = parse_frontmatter(&content);

        headers.push(MemoryHeader {
            filename: relative.replace('\\', "/"),
            file_path: current_dir.join(path.file_name().unwrap_or_default()),
            mtime_ms,
            description: parsed.frontmatter.description().map(String::from),
            memory_type: parsed.frontmatter.memory_type().map(String::from),
        });
    }
}

/// Read the first `max_bytes` of a file as a string.
fn read_file_head(path: &Path, max_bytes: usize) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let len = bytes.len().min(max_bytes);
            String::from_utf8_lossy(&bytes[..len]).to_string()
        }
        Err(_) => String::new(),
    }
}

/// Format memory headers as a text manifest for the selection prompt.
///
/// One line per file: `- [type] filename (timestamp): description`
///
/// Joined with newlines; an empty slice yields an empty string.
pub fn format_memory_manifest(memories: &[MemoryHeader]) -> String {
    memories
        .iter()
        .map(|m| {
            let tag = match &m.memory_type {
                Some(t) => format!("[{t}] "),
                None => String::new(),
            };
            let ts = format_timestamp_iso(m.mtime_ms);
            match &m.description {
                Some(desc) => format!("- {tag}{} ({ts}): {desc}", m.filename),
                None => format!("- {tag}{} ({ts})", m.filename),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format millisecond timestamp as ISO 8601 string.
fn format_timestamp_iso(ms: i64) -> String {
    // Simple conversion without chrono dependency
    let secs = ms / 1000;
    let days = secs / 86400;
    let (y, mo, d) = days_to_ymd(days);
    let rem = secs % 86400;
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Howard Hinnant's civil_from_days algorithm.
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_temp_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        // Create a few memory files
        fs::write(
            dir.path().join("user_role.md"),
            "---\nname: User Role\ndescription: Senior engineer\ntype: user\n---\nContent here",
        )
        .unwrap();

        fs::write(
            dir.path().join("feedback_tests.md"),
            "---\nname: Testing feedback\ndescription: Always use real DB\ntype: feedback\n---\nMore content",
        ).unwrap();

        fs::write(
            dir.path().join("no_frontmatter.md"),
            "Just plain markdown without frontmatter",
        )
        .unwrap();

        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(
            dir.path().join("nested").join("child.md"),
            "---\ndescription: Nested\ntype: project\n---\nBody",
        )
        .unwrap();
        fs::write(dir.path().join("nested").join("MEMORY.md"), "excluded").unwrap();

        // MEMORY.md should be excluded
        fs::write(
            dir.path().join("MEMORY.md"),
            "- [User Role](user_role.md)\n- [Testing](feedback_tests.md)",
        )
        .unwrap();

        // Non-md file should be ignored
        fs::write(dir.path().join("notes.txt"), "not a memory file").unwrap();

        dir
    }

    #[test]
    fn scan_finds_md_files_excluding_memory_md() {
        let dir = setup_temp_dir();
        let headers = scan_memory_files(dir.path());
        let filenames: Vec<&str> = headers.iter().map(|h| h.filename.as_str()).collect();
        assert!(filenames.contains(&"user_role.md"));
        assert!(filenames.contains(&"feedback_tests.md"));
        assert!(filenames.contains(&"no_frontmatter.md"));
        assert!(filenames.contains(&"nested/child.md"));
        assert!(!filenames.contains(&"MEMORY.md"));
        assert!(!filenames.contains(&"nested/MEMORY.md"));
        assert!(!filenames.contains(&"notes.txt"));
    }

    #[test]
    fn scan_extracts_frontmatter() {
        let dir = setup_temp_dir();
        let headers = scan_memory_files(dir.path());
        let user = headers
            .iter()
            .find(|h| h.filename == "user_role.md")
            .unwrap();
        assert_eq!(user.description.as_deref(), Some("Senior engineer"));
        assert_eq!(user.memory_type.as_deref(), Some("user"));
    }

    #[test]
    fn scan_handles_missing_frontmatter() {
        let dir = setup_temp_dir();
        let headers = scan_memory_files(dir.path());
        let plain = headers
            .iter()
            .find(|h| h.filename == "no_frontmatter.md")
            .unwrap();
        assert_eq!(plain.description, None);
        assert_eq!(plain.memory_type, None);
    }

    #[test]
    fn scan_sorted_newest_first() {
        let dir = setup_temp_dir();
        let headers = scan_memory_files(dir.path());
        for w in headers.windows(2) {
            assert!(w[0].mtime_ms >= w[1].mtime_ms);
        }
    }

    #[test]
    fn scan_nonexistent_dir_returns_empty() {
        let headers = scan_memory_files(Path::new("/nonexistent/path/memory"));
        assert!(headers.is_empty());
    }

    #[test]
    fn format_manifest_includes_type_and_description() {
        let headers = vec![
            MemoryHeader {
                filename: "user_role.md".to_string(),
                file_path: PathBuf::from("/test/user_role.md"),
                mtime_ms: 1_700_000_000_000,
                description: Some("Senior engineer".to_string()),
                memory_type: Some("user".to_string()),
            },
            MemoryHeader {
                filename: "plain.md".to_string(),
                file_path: PathBuf::from("/test/plain.md"),
                mtime_ms: 1_700_000_000_000,
                description: None,
                memory_type: None,
            },
        ];
        let manifest = format_memory_manifest(&headers);
        assert!(manifest.contains("[user] user_role.md"));
        assert!(manifest.contains("Senior engineer"));
        assert!(manifest.contains("- plain.md"));
    }

    #[test]
    fn format_timestamp_iso_known_value() {
        // 2023-11-14T22:13:20Z = 1700000000 seconds
        let ts = format_timestamp_iso(1_700_000_000_000);
        assert_eq!(ts, "2023-11-14T22:13:20Z");
    }
}
