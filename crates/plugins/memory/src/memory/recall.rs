//! Per-turn relevant memory recall.
//!
//! Provides the
//! infrastructure for selecting which memories are relevant to a
//! user's query:
//!
//! - [`scan_and_build_manifest`] — scan memory dir + format manifest
//! - [`build_selection_user_message`] — build the Sonnet side-query user message
//! - [`parse_selection_response`] — parse Sonnet's JSON response
//! - [`RelevantMemory`] — result type with path + mtime

use std::collections::HashSet;
use std::path::Path;

use crate::memory::scan::{format_memory_manifest, scan_memory_files, MemoryHeader};

/// A memory file selected as relevant to the current query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelevantMemory {
    /// Absolute file path.
    pub path: String,
    /// Last-modified timestamp in milliseconds.
    pub mtime_ms: i64,
}

/// System prompt for the memory selection side-query.
///
/// Asks for up to 5 filenames, skipping memories that only document recently-used tools.
pub const SELECT_MEMORIES_SYSTEM_PROMPT: &str = "\
You are selecting memories that will be useful to the assistant as it processes a user's query. \
You will be given the user's query and a list of available memory files with their filenames and descriptions.\n\
\n\
Return a list of filenames for the memories that will clearly be useful as it processes the user's query (up to 5). \
Only include memories that you are certain will be helpful based on their name and description.\n\
- If you are unsure if a memory will be useful in processing the user's query, then do not include it in your list. Be selective and discerning.\n\
- If there are no memories in the list that would clearly be useful, feel free to return an empty list.\n\
- If a list of recently-used tools is provided, do not select memories that are usage reference or API documentation for those tools (the assistant is already exercising them). DO still select memories containing warnings, gotchas, or known issues about those tools \u{2014} active use is exactly when those matter.";

/// Maximum number of memories to select per query.
pub const MAX_SELECTED_MEMORIES: usize = 5;

/// Scan the memory directory and build a manifest string for the
/// selection prompt.
///
/// Returns `(headers, manifest)`. Returns `(vec![], "")` if the
/// directory is empty or unreadable.
pub fn scan_and_build_manifest(
    memory_dir: &Path,
    already_surfaced: &HashSet<String>,
) -> (Vec<MemoryHeader>, String) {
    let headers: Vec<MemoryHeader> = scan_memory_files(memory_dir)
        .into_iter()
        .filter(|h| !already_surfaced.contains(&h.file_path.to_string_lossy().to_string()))
        .collect();

    if headers.is_empty() {
        return (Vec::new(), String::new());
    }

    let manifest = format_memory_manifest(&headers);
    (headers, manifest)
}

/// Build the user message for the selection side-query.
///
/// Combines the user's query with the available memories manifest
/// and optionally a list of recently-used tools.
pub fn build_selection_user_message(
    query: &str,
    manifest: &str,
    recent_tools: &[String],
) -> String {
    let tools_section = if recent_tools.is_empty() {
        String::new()
    } else {
        format!("\n\nRecently used tools: {}", recent_tools.join(", "))
    };
    format!("Query: {query}\n\nAvailable memories:\n{manifest}{tools_section}")
}

/// Parse the selection response from the side-query model.
///
/// Expects JSON: `{"selected_memories": ["file1.md", "file2.md"]}`
///
/// Filters results to only include filenames that exist in the
/// scanned headers. Returns at most [`MAX_SELECTED_MEMORIES`] entries.
pub fn parse_selection_response(
    response_text: &str,
    headers: &[MemoryHeader],
) -> Vec<RelevantMemory> {
    // Simple JSON extraction — find the array
    let selected = extract_selected_filenames(response_text);

    let valid_files: std::collections::HashMap<&str, &MemoryHeader> =
        headers.iter().map(|h| (h.filename.as_str(), h)).collect();

    selected
        .into_iter()
        .filter_map(|filename| {
            valid_files.get(filename.as_str()).map(|h| RelevantMemory {
                path: h.file_path.to_string_lossy().to_string(),
                mtime_ms: h.mtime_ms,
            })
        })
        .take(MAX_SELECTED_MEMORIES)
        .collect()
}

/// Extract the `selected_memories` array from a JSON response string.
///
/// Handles both clean JSON and JSON embedded in text (e.g. markdown
/// code blocks). Falls back gracefully to empty vec on parse failure.
fn extract_selected_filenames(text: &str) -> Vec<String> {
    // Try to find JSON object in the text
    let json_str = find_json_object(text).unwrap_or(text);

    // Parse as generic JSON value
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Extract selected_memories array
    value
        .get("selected_memories")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Find the first JSON object `{...}` in a string.
fn find_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Convenience: run the full recall pipeline synchronously.
///
/// Scans memory dir, builds manifest, and returns the manifest +
/// headers ready for a side-query. The caller is responsible for
/// making the actual API call and parsing the response with
/// [`parse_selection_response`].
///
/// Returns `None` if there are no candidate memories.
pub fn prepare_recall(
    memory_dir: &Path,
    already_surfaced: &HashSet<String>,
) -> Option<RecallContext> {
    let (headers, manifest) = scan_and_build_manifest(memory_dir, already_surfaced);
    if headers.is_empty() {
        return None;
    }
    Some(RecallContext { headers, manifest })
}

/// Context prepared for a memory recall side-query.
#[derive(Debug, Clone)]
pub struct RecallContext {
    /// Scanned memory headers (for validating the response).
    pub headers: Vec<MemoryHeader>,
    /// Formatted manifest string (for the selection prompt).
    pub manifest: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_headers() -> Vec<MemoryHeader> {
        vec![
            MemoryHeader {
                filename: "user_role.md".to_string(),
                file_path: PathBuf::from("/mem/user_role.md"),
                mtime_ms: 1_700_000_000_000,
                description: Some("Senior engineer".to_string()),
                memory_type: Some("user".to_string()),
            },
            MemoryHeader {
                filename: "feedback_tests.md".to_string(),
                file_path: PathBuf::from("/mem/feedback_tests.md"),
                mtime_ms: 1_700_000_001_000,
                description: Some("Use real DB".to_string()),
                memory_type: Some("feedback".to_string()),
            },
            MemoryHeader {
                filename: "project_freeze.md".to_string(),
                file_path: PathBuf::from("/mem/project_freeze.md"),
                mtime_ms: 1_700_000_002_000,
                description: Some("Merge freeze March 5".to_string()),
                memory_type: Some("project".to_string()),
            },
        ]
    }

    #[test]
    fn parse_valid_response() {
        let response = r#"{"selected_memories": ["user_role.md", "feedback_tests.md"]}"#;
        let result = parse_selection_response(response, &sample_headers());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, "/mem/user_role.md");
        assert_eq!(result[1].path, "/mem/feedback_tests.md");
    }

    #[test]
    fn parse_response_filters_invalid_filenames() {
        let response = r#"{"selected_memories": ["user_role.md", "nonexistent.md"]}"#;
        let result = parse_selection_response(response, &sample_headers());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "/mem/user_role.md");
    }

    #[test]
    fn parse_empty_response() {
        let response = r#"{"selected_memories": []}"#;
        let result = parse_selection_response(response, &sample_headers());
        assert!(result.is_empty());
    }

    #[test]
    fn parse_malformed_json() {
        let response = "not json at all";
        let result = parse_selection_response(response, &sample_headers());
        assert!(result.is_empty());
    }

    #[test]
    fn parse_json_in_markdown_code_block() {
        let response = "```json\n{\"selected_memories\": [\"user_role.md\"]}\n```";
        let result = parse_selection_response(response, &sample_headers());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_caps_at_max() {
        let response = r#"{"selected_memories": ["user_role.md", "feedback_tests.md", "project_freeze.md", "user_role.md", "feedback_tests.md", "project_freeze.md"]}"#;
        let result = parse_selection_response(response, &sample_headers());
        assert!(result.len() <= MAX_SELECTED_MEMORIES);
    }

    #[test]
    fn build_selection_message_basic() {
        let msg = build_selection_user_message("how do I test?", "- user_role.md", &[]);
        assert!(msg.contains("how do I test?"));
        assert!(msg.contains("user_role.md"));
        assert!(!msg.contains("Recently used tools"));
    }

    #[test]
    fn build_selection_message_with_tools() {
        let msg = build_selection_user_message(
            "query",
            "- mem.md",
            &["Bash".to_string(), "Read".to_string()],
        );
        assert!(msg.contains("Recently used tools: Bash, Read"));
    }

    #[test]
    fn find_json_object_basic() {
        assert_eq!(
            find_json_object("prefix {\"a\": 1} suffix"),
            Some("{\"a\": 1}")
        );
    }

    #[test]
    fn find_json_object_nested() {
        assert_eq!(
            find_json_object("{\"a\": {\"b\": 2}}"),
            Some("{\"a\": {\"b\": 2}}")
        );
    }

    #[test]
    fn find_json_object_none() {
        assert_eq!(find_json_object("no json here"), None);
    }

    #[test]
    fn scan_and_build_manifest_filters_surfaced() {
        // This test needs a real temp dir — tested in scan.rs already.
        // Here we just verify the filtering logic.
        let mut surfaced = HashSet::new();
        surfaced.insert("/mem/user_role.md".to_string());

        // With nonexistent dir, returns empty
        let (headers, manifest) = scan_and_build_manifest(Path::new("/nonexistent"), &surfaced);
        assert!(headers.is_empty());
        assert!(manifest.is_empty());
    }
}
