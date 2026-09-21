//! Compact-summary row projection, plus the section extractors that read a
//! summary body: the last user message, the file entries, the numbered
//! sections and their dash-prefixed items.

use rebon_design_system::{format_shortcut_for_current_platform, format_shortcut_hint};

/// Summary metadata threaded by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummaryMetadata {
    /// Number of messages summarized.
    pub messages_summarized: usize,
    /// `"up_to"` or `"from_here"`-style direction.
    pub direction: String,
    /// Optional quoted user context.
    pub user_context: Option<String>,
}

/// Input bag for the compact-summary projector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummaryInput {
    /// Already-extracted user message text.
    pub text_content: String,
    /// Whether current screen is transcript.
    pub is_transcript_mode: bool,
    /// Optional summarize metadata.
    pub metadata: Option<CompactSummaryMetadata>,
    /// Shortcut used in the hint.
    pub history_shortcut: String,
}

/// A single file reference extracted from the compact summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactFileEntry {
    /// The file path.
    pub path: String,
    /// Optional line count hint (e.g. "218 lines").
    pub line_count: Option<String>,
}

/// What the compact-summary row renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummaryDisplay {
    /// The last user prompt text (shown on the `❯` line).
    pub user_prompt: Option<String>,
    /// Leading title (fallback when no user_prompt).
    pub title: String,
    /// Optional metadata line shown outside transcript mode.
    pub metadata_line: Option<String>,
    /// Optional context line.
    pub context_line: Option<String>,
    /// Optional shortcut hint.
    pub shortcut_hint: Option<String>,
    /// Optional transcript-mode content body.
    pub transcript_text: Option<String>,
    /// File references extracted from the summary.
    pub file_entries: Vec<CompactFileEntry>,
}

/// Builds the compact-summary row. With `metadata` present it renders the
/// `Summarized <n> messages up to this point` / `from this point` line, the
/// quoted context line and the history shortcut; without it, it pulls the last
/// user message, the file entries and the bullet summary out of the summary
/// text itself.
pub fn project_compact_summary(input: &CompactSummaryInput) -> CompactSummaryDisplay {
    if let Some(metadata) = &input.metadata {
        let direction = if metadata.direction == "up_to" {
            "up to this point"
        } else {
            "from this point"
        };
        return CompactSummaryDisplay {
            user_prompt: metadata.user_context.clone(),
            title: "Summarized conversation".to_string(),
            metadata_line: Some(format!(
                "Summarized {} messages {direction}",
                metadata.messages_summarized
            )),
            context_line: metadata
                .user_context
                .as_ref()
                .map(|ctx| format!("Context: \u{201c}{ctx}\u{201d}")),
            shortcut_hint: Some(format!(
                "({} for history)",
                format_shortcut_for_current_platform(&input.history_shortcut)
            )),
            transcript_text: None,
            file_entries: Vec::new(),
        };
    }

    let last_user_msg = extract_last_user_message(&input.text_content);
    let file_entries = extract_file_entries(&input.text_content);

    CompactSummaryDisplay {
        user_prompt: last_user_msg,
        title: "Compacted from last messages".to_string(),
        metadata_line: extract_compact_bullets(&input.text_content),
        context_line: None,
        shortcut_hint: Some(
            format_shortcut_hint(&input.history_shortcut, "expand", true, false).plain_text,
        ),
        transcript_text: None,
        file_entries,
    }
}

/// Extract the last user message from section 6 ("All user messages")
/// of the compact summary text.
fn extract_last_user_message(text: &str) -> Option<String> {
    let section = extract_numbered_section(text, &["All user messages", "All User Messages"]);
    if let Some(content) = section {
        let items = extract_list_items(&content);
        if let Some(last) = items.last() {
            let trimmed = last.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    let section = extract_numbered_section(text, &["Current Work"]);
    if let Some(content) = section {
        let first_line = content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('-'));
        if let Some(line) = first_line {
            return Some(truncate_display(line, 120));
        }
    }

    None
}

/// Extract file entries from section 3 ("Files and Code Sections").
fn extract_file_entries(text: &str) -> Vec<CompactFileEntry> {
    let mut entries = Vec::new();
    let section = extract_numbered_section(text, &["Files and Code Sections", "Files and Code"]);
    if let Some(content) = section {
        for item in extract_list_items(&content) {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                continue;
            }
            let path = trimmed
                .trim_start_matches('`')
                .trim_end_matches('`')
                .split(" - ")
                .next()
                .unwrap_or(trimmed)
                .split(':')
                .next()
                .unwrap_or(trimmed)
                .trim();
            if looks_like_file_path(path) {
                entries.push(CompactFileEntry {
                    path: path.to_string(),
                    line_count: None,
                });
            }
        }
    }
    entries.truncate(8);
    entries
}

fn looks_like_file_path(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() < 3 {
        return false;
    }
    s.contains('.') && (s.contains('/') || s.contains('\\'))
        || s.ends_with(".rs")
        || s.ends_with(".ts")
        || s.ends_with(".js")
        || s.ends_with(".py")
        || s.ends_with(".toml")
        || s.ends_with(".json")
        || s.ends_with(".yaml")
        || s.ends_with(".yml")
        || s.ends_with(".md")
        || s.ends_with(".html")
        || s.ends_with(".css")
        || s.ends_with(".go")
}

/// Extract the content of a numbered section by title.
fn extract_numbered_section(text: &str, titles: &[&str]) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut start = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let first_char = trimmed.chars().next().unwrap_or(' ');
        if first_char.is_ascii_digit() {
            if let Some(rest) = trimmed.splitn(2, ". ").nth(1) {
                let section_title = rest.trim_end_matches(':').trim();
                if titles.iter().any(|t| section_title.eq_ignore_ascii_case(t)) {
                    start = Some(i + 1);
                } else if start.is_some() {
                    let end = i;
                    return Some(lines[start.unwrap()..end].join("\n"));
                }
            }
        }
    }

    if let Some(s) = start {
        return Some(lines[s..].join("\n"));
    }
    None
}

/// Extract dash-prefixed list items from section content.
fn extract_list_items(content: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            if let Some(prev) = current.take() {
                items.push(prev);
            }
            current = Some(trimmed[2..].to_string());
        } else if trimmed.is_empty() {
            if let Some(prev) = current.take() {
                items.push(prev);
            }
        } else if let Some(ref mut cur) = current {
            cur.push(' ');
            cur.push_str(trimmed);
        }
    }
    if let Some(prev) = current {
        items.push(prev);
    }
    items
}

fn truncate_display(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let truncated: String = chars[..max.saturating_sub(1)].iter().collect();
        format!("{truncated}\u{2026}")
    }
}

/// Extract a short bullet-point summary from the compact summary body.
///
/// Looks for numbered top-level sections (e.g. "1. Primary Request")
/// and returns them as "- Section title" lines.  Falls back to the first
/// few non-empty lines when no numbered sections are found.
fn extract_compact_bullets(text: &str) -> Option<String> {
    let mut bullets = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let first_char = trimmed.chars().next().unwrap_or(' ');
        if first_char.is_ascii_digit() {
            if let Some(rest) = trimmed.splitn(2, ". ").nth(1) {
                let label = rest.trim_end_matches(':').trim();
                if !label.is_empty() {
                    bullets.push(format!("- {label}"));
                }
            }
        }
        if bullets.len() >= 5 {
            break;
        }
    }
    if bullets.is_empty() {
        let preview: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('<'))
            .take(3)
            .collect();
        if preview.is_empty() {
            return None;
        }
        return Some(preview.join("\n"));
    }
    Some(bullets.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> CompactSummaryInput {
        CompactSummaryInput {
            text_content: "summary text".into(),
            is_transcript_mode: false,
            metadata: None,
            history_shortcut: "ctrl+o".into(),
        }
    }

    #[test]
    fn default_compact_summary_uses_expand_hint() {
        let d = project_compact_summary(&input());
        assert_eq!(d.title, "Compacted from last messages");
        assert_eq!(d.shortcut_hint.as_deref(), Some("(Ctrl+O to expand)"));
        assert_eq!(d.transcript_text, None);
    }

    #[test]
    fn metadata_variant_uses_summarized_title_and_context() {
        let mut i = input();
        i.metadata = Some(CompactSummaryMetadata {
            messages_summarized: 12,
            direction: "up_to".into(),
            user_context: Some("ctx".into()),
        });
        let d = project_compact_summary(&i);
        assert_eq!(d.title, "Summarized conversation");
        assert_eq!(
            d.metadata_line.as_deref(),
            Some("Summarized 12 messages up to this point")
        );
        assert_eq!(
            d.context_line.as_deref(),
            Some("Context: \u{201c}ctx\u{201d}")
        );
        assert_eq!(d.shortcut_hint.as_deref(), Some("(Ctrl+O for history)"));
        assert_eq!(d.user_prompt.as_deref(), Some("ctx"));
    }

    #[test]
    fn metadata_variant_always_shows_hint_and_never_body() {
        let mut i = input();
        i.is_transcript_mode = true;
        i.metadata = Some(CompactSummaryMetadata {
            messages_summarized: 5,
            direction: "from_here".into(),
            user_context: None,
        });
        let d = project_compact_summary(&i);
        assert!(d.shortcut_hint.is_some());
        assert_eq!(d.transcript_text, None);
    }

    #[test]
    fn transcript_mode_never_shows_full_body() {
        let mut i = input();
        i.is_transcript_mode = true;
        let d = project_compact_summary(&i);
        assert_eq!(d.transcript_text, None);
        assert!(d.shortcut_hint.is_some());
    }

    #[test]
    fn extract_compact_bullets_from_numbered_sections() {
        let text = "\
1. Primary Request and Intent:
   The user is building a TUI app.

2. Key Technical Concepts:
   Inline mode, screen mode, compaction.

3. Files and Code Sections:
   Various Rust files.";
        let bullets = extract_compact_bullets(text).unwrap();
        assert_eq!(
            bullets,
            "- Primary Request and Intent\n- Key Technical Concepts\n- Files and Code Sections"
        );
    }

    #[test]
    fn extract_compact_bullets_fallback_to_first_lines() {
        let text = "Some plain text\nanother line\nthird line";
        let bullets = extract_compact_bullets(text).unwrap();
        assert_eq!(bullets, "Some plain text\nanother line\nthird line");
    }

    #[test]
    fn extract_compact_bullets_skips_xml_tags() {
        let text = "<system-generated-history-summary>\nActual content here\n</system-generated-history-summary>";
        let bullets = extract_compact_bullets(text).unwrap();
        assert_eq!(bullets, "Actual content here");
    }

    #[test]
    fn extracts_last_user_message_from_section_6() {
        let text = "\
1. Primary Request and Intent:
   Build a TUI app.

6. All user messages:
   - First user message about setup
   - Second message asking about rendering
   - 改一下 compact 的展示效果

7. Pending Tasks:
   None.";
        let msg = extract_last_user_message(text);
        assert_eq!(msg.as_deref(), Some("改一下 compact 的展示效果"));
    }

    #[test]
    fn extracts_file_entries_from_section_3() {
        let text = "\
1. Primary Request:
   Build a TUI.

3. Files and Code Sections:
   - crates/rebon-messages/src/compact_summary.rs - compact rendering
   - crates/rebon-tui/src/render/mod.rs: main render dispatch

4. Errors:
   None.";
        let entries = extract_file_entries(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].path,
            "crates/rebon-messages/src/compact_summary.rs"
        );
        assert_eq!(entries[1].path, "crates/rebon-tui/src/render/mod.rs");
    }

    #[test]
    fn compact_summary_includes_user_prompt_and_files() {
        let text = "\
1. Primary Request:
   TUI compact display.

3. Files and Code Sections:
   - src/render.rs - rendering code
   - src/app.rs - app state

6. All user messages:
   - 改成这样的效果

7. Pending Tasks:
   None.";
        let d = project_compact_summary(&CompactSummaryInput {
            text_content: text.into(),
            is_transcript_mode: false,
            metadata: None,
            history_shortcut: "ctrl+o".into(),
        });
        assert_eq!(d.user_prompt.as_deref(), Some("改成这样的效果"));
        assert_eq!(d.file_entries.len(), 2);
        assert_eq!(d.file_entries[0].path, "src/render.rs");
        assert_eq!(d.file_entries[1].path, "src/app.rs");
    }

    #[test]
    fn file_path_detection_works() {
        assert!(looks_like_file_path("src/main.rs"));
        assert!(looks_like_file_path("crates/foo/bar.rs"));
        assert!(looks_like_file_path("Cargo.toml"));
        assert!(!looks_like_file_path("None"));
        assert!(!looks_like_file_path(""));
        assert!(!looks_like_file_path("ab"));
    }

    #[test]
    fn extract_numbered_section_returns_content() {
        let text = "\
1. Foo:
   foo content

2. Bar:
   bar content
   more bar

3. Baz:
   baz";
        let bar = extract_numbered_section(text, &["Bar"]).unwrap();
        assert!(bar.contains("bar content"));
        assert!(bar.contains("more bar"));
        assert!(!bar.contains("baz"));
    }
}
