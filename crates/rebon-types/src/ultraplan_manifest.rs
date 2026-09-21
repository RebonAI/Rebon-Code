use std::collections::HashSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanManifestSnapshot {
    pub source_path: String,
    pub canonical_path: String,
    pub display_path: String,
    pub content_sha256: String,
    pub items: Vec<UltraplanManifestItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanManifestItem {
    pub id: String,
    pub title: String,
    pub line: usize,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCoverageResult {
    pub covered: Vec<String>,
    pub missing: Vec<String>,
    pub unknown_ids: Vec<String>,
}

pub fn parse_markdown_manifest_items(
    content: &str,
) -> Result<Vec<UltraplanManifestItem>, UltraplanManifestError> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut in_fence = false;

    for (idx, line) in content.lines().enumerate() {
        let line_number = idx + 1;
        let trimmed = line.trim_start();
        if is_fence_boundary(trimmed) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let Some(payload) = manifest_line_payload(trimmed) else {
            continue;
        };
        let (id, title) = parse_manifest_payload(payload).ok_or_else(|| {
            UltraplanManifestError::InvalidItem {
                line: line_number,
                reason: "expected a manifest/custom plan item such as '- [ ] ID: title', '- ID - title', '1. ID: title', or '## ID - title' with a non-empty ID and title".into(),
            }
        })?;
        if !is_valid_manifest_id(id) {
            return Err(UltraplanManifestError::InvalidItem {
                line: line_number,
                reason: format!(
                    "invalid ID '{id}'; use only ASCII letters, digits, '_', '-', or '.'"
                ),
            });
        }
        if !seen.insert(id.to_string()) {
            return Err(UltraplanManifestError::DuplicateId {
                id: id.to_string(),
                line: line_number,
            });
        }
        items.push(UltraplanManifestItem {
            id: id.to_string(),
            title: title.to_string(),
            line: line_number,
            required: true,
        });
    }

    Ok(items)
}

fn is_fence_boundary(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn manifest_line_payload(trimmed: &str) -> Option<&str> {
    if let Some(payload) = checklist_payload(trimmed) {
        return Some(payload);
    }
    for payload in [
        bullet_payload(trimmed),
        ordered_list_payload(trimmed),
        heading_payload(trimmed),
    ]
    .into_iter()
    .flatten()
    {
        if manifest_payload_has_separator(payload) {
            return Some(payload);
        }
    }
    None
}

fn checklist_payload(trimmed: &str) -> Option<&str> {
    let trimmed = bullet_payload(trimmed)?;
    let rest = trimmed.strip_prefix('[')?;
    let mut chars = rest.chars();
    let marker = chars.next()?;
    if !matches!(marker, ' ' | 'x' | 'X') {
        return None;
    }
    let rest = chars.as_str();
    let rest = rest.strip_prefix(']')?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start())
}

fn bullet_payload(trimmed: &str) -> Option<&str> {
    let rest = trimmed
        .strip_prefix('-')
        .or_else(|| trimmed.strip_prefix('*'))
        .or_else(|| trimmed.strip_prefix('+'))?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start())
}

fn ordered_list_payload(trimmed: &str) -> Option<&str> {
    let marker_end = trimmed.find(|ch: char| !ch.is_ascii_digit())?;
    if marker_end == 0 {
        return None;
    }
    let marker = trimmed[marker_end..].chars().next()?;
    if !matches!(marker, '.' | ')') {
        return None;
    }
    let rest = &trimmed[marker_end + marker.len_utf8()..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start())
}

fn heading_payload(trimmed: &str) -> Option<&str> {
    let heading_len = trimmed.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&heading_len) {
        return None;
    }
    let rest = &trimmed[heading_len..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start())
}

fn manifest_payload_has_separator(payload: &str) -> bool {
    payload.contains(':') || split_on_spaced_dash(payload).is_some()
}

fn parse_manifest_payload(payload: &str) -> Option<(&str, &str)> {
    let separator = match (payload.find(':'), find_spaced_dash(payload)) {
        (Some(colon), Some(dash)) if dash < colon => dash,
        (Some(colon), _) => colon,
        (None, Some(dash)) => dash,
        (None, None) => return None,
    };
    let (id, title) = payload.split_at(separator);
    let title = title[1..].trim();
    let id = id.trim();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some((id, title))
}

fn split_on_spaced_dash(payload: &str) -> Option<(&str, &str)> {
    let idx = find_spaced_dash(payload)?;
    Some((&payload[..idx], &payload[idx + 1..]))
}

fn find_spaced_dash(payload: &str) -> Option<usize> {
    for (idx, ch) in payload.char_indices() {
        if ch != '-' {
            continue;
        }
        let before = payload[..idx].chars().next_back()?;
        let after_start = idx + ch.len_utf8();
        let after = payload[after_start..].chars().next()?;
        if before.is_whitespace() && after.is_whitespace() {
            return Some(idx);
        }
    }
    None
}

pub fn is_valid_manifest_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UltraplanManifestError {
    InvalidItem { line: usize, reason: String },
    DuplicateId { id: String, line: usize },
}

impl std::fmt::Display for UltraplanManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidItem { line, reason } => {
                write!(f, "invalid manifest item on line {line}: {reason}")
            }
            Self::DuplicateId { id, line } => {
                write!(f, "duplicate manifest item ID '{id}' on line {line}")
            }
        }
    }
}

impl std::error::Error for UltraplanManifestError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markdown_checklist_items_and_ignores_fences() {
        let content = "# Tasks\n- [ ] T1: 修复登录\n- [x] AUTH-001: Already done\n```\n- [ ] HIDDEN: ignored\n```\nplain text\n";
        let items = parse_markdown_manifest_items(content).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "T1");
        assert_eq!(items[0].title, "修复登录");
        assert_eq!(items[0].line, 2);
        assert!(items[0].required);
        assert_eq!(items[1].id, "AUTH-001");
        assert_eq!(items[1].line, 3);
        assert!(items[1].required);
    }

    #[test]
    fn parses_bullet_ordered_and_heading_manifest_items() {
        let content = "- R1: bullet colon\n* R2 - bullet dash\n+ R3: plus bullet\n1. R4: ordered dot\n2) R5 - ordered paren\n## R6: heading colon\n### R7 - heading dash\n";
        let items = parse_markdown_manifest_items(content).unwrap();
        let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
        let titles: Vec<_> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(ids, vec!["R1", "R2", "R3", "R4", "R5", "R6", "R7"]);
        assert_eq!(
            titles,
            vec![
                "bullet colon",
                "bullet dash",
                "plus bullet",
                "ordered dot",
                "ordered paren",
                "heading colon",
                "heading dash",
            ]
        );
        assert_eq!(items[6].line, 7);
        assert!(items.iter().all(|item| item.required));
    }

    #[test]
    fn ignores_code_blocks_and_non_manifest_markdown() {
        let content = "# Notes\nIntro text\n- plain note without id delimiter\n```\n- R1: hidden\n```\n~~~\n1. R2: also hidden\n~~~\n## R3: visible\n";
        let items = parse_markdown_manifest_items(content).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "R3");
        assert_eq!(items[0].line, 10);
    }

    #[test]
    fn returns_empty_for_empty_or_no_valid_manifest_items() {
        assert_eq!(parse_markdown_manifest_items("").unwrap(), Vec::new());
        assert_eq!(
            parse_markdown_manifest_items("# Notes\n- just context\nplain text").unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn rejects_missing_id_empty_title_invalid_id_and_duplicate_id() {
        assert!(matches!(
            parse_markdown_manifest_items("- [ ] : no id"),
            Err(UltraplanManifestError::InvalidItem { line: 1, .. })
        ));
        assert!(matches!(
            parse_markdown_manifest_items("- [ ] T1:"),
            Err(UltraplanManifestError::InvalidItem { line: 1, .. })
        ));
        assert!(matches!(
            parse_markdown_manifest_items("- T1 -   "),
            Err(UltraplanManifestError::InvalidItem { line: 1, .. })
        ));
        assert!(matches!(
            parse_markdown_manifest_items("## T/1: bad"),
            Err(UltraplanManifestError::InvalidItem { line: 1, .. })
        ));
        assert!(matches!(
            parse_markdown_manifest_items("- [ ] T1: one\n1. T1: two"),
            Err(UltraplanManifestError::DuplicateId { ref id, line: 2 }) if id == "T1"
        ));
    }
}
