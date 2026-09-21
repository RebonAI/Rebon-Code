//! Lightweight YAML frontmatter parser for memory files.
//!
//! Extracts the `---` delimited
//! YAML block from the top of a markdown file and parses key fields
//! (name, description, type) without pulling in a full YAML library.

use std::collections::HashMap;

/// Parsed frontmatter fields from a memory file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    /// All key-value pairs found in the frontmatter block.
    pub fields: HashMap<String, String>,
    /// Glob patterns parsed from a `paths:` YAML list.
    pub paths: Vec<String>,
}

impl Frontmatter {
    /// Get the `name` field.
    pub fn name(&self) -> Option<&str> {
        self.fields.get("name").map(|s| s.as_str())
    }

    /// Get the `description` field.
    pub fn description(&self) -> Option<&str> {
        self.fields.get("description").map(|s| s.as_str())
    }

    /// Get the `type` field.
    pub fn memory_type(&self) -> Option<&str> {
        self.fields.get("type").map(|s| s.as_str())
    }
}

/// Result of parsing a file with frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    /// The parsed frontmatter (empty if no frontmatter block found).
    pub frontmatter: Frontmatter,
    /// The body content after the frontmatter block.
    pub body: String,
}

/// Parse YAML frontmatter from the beginning of a string.
///
/// Expects the format:
/// ```text
/// ---
/// key1: value1
/// key2: value2
/// ---
/// body content here
/// ```
///
/// If no valid frontmatter block is found, returns empty frontmatter
/// and the full input as body.
pub fn parse_frontmatter(input: &str) -> ParsedFile {
    // Must start with `---` followed by newline
    let trimmed = input.trim_start_matches('\u{feff}'); // strip BOM
    if !trimmed.starts_with("---") {
        return ParsedFile {
            frontmatter: Frontmatter::default(),
            body: input.to_string(),
        };
    }

    // Find the closing `---`
    let after_opening = &trimmed[3..];
    let after_opening = after_opening.strip_prefix('\r').unwrap_or(after_opening);
    let after_opening = after_opening.strip_prefix('\n').unwrap_or(after_opening);

    let closing_pos = find_closing_delimiter(after_opening);
    let (yaml_block, body) = match closing_pos {
        Some(pos) => {
            let yaml = &after_opening[..pos];
            let rest = &after_opening[pos + 3..]; // skip "---"
                                                  // Strip leading newline from body
            let rest = rest.strip_prefix('\r').unwrap_or(rest);
            let rest = rest.strip_prefix('\n').unwrap_or(rest);
            (yaml, rest.to_string())
        }
        None => {
            // No closing delimiter — treat entire input as body
            return ParsedFile {
                frontmatter: Frontmatter::default(),
                body: input.to_string(),
            };
        }
    };

    let (fields, paths) = parse_yaml_simple(yaml_block);
    ParsedFile {
        frontmatter: Frontmatter { fields, paths },
        body,
    }
}

/// Find the position of the closing `---` delimiter.
/// Must appear at the start of a line.
fn find_closing_delimiter(s: &str) -> Option<usize> {
    // Check if it starts with ---
    if s.starts_with("---")
        && (s.len() == 3 || s[3..].starts_with('\n') || s[3..].starts_with('\r'))
    {
        return Some(0);
    }
    // Search for \n---
    let mut search_from = 0;
    while let Some(pos) = s[search_from..].find("\n---") {
        let abs_pos = search_from + pos + 1; // position of the first '-'
        let after = abs_pos + 3;
        if after >= s.len() || s[after..].starts_with('\n') || s[after..].starts_with('\r') {
            return Some(abs_pos);
        }
        search_from = abs_pos + 1;
    }
    None
}

/// Minimal YAML key-value parser. Handles simple `key: value` lines and
/// `paths:` lists in block or inline form.
fn parse_yaml_simple(yaml: &str) -> (HashMap<String, String>, Vec<String>) {
    let mut map = HashMap::new();
    let mut paths = Vec::new();
    let mut collecting_paths = false;

    for raw_line in yaml.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if collecting_paths {
            if let Some(item) = line.strip_prefix('-') {
                let item = decode_yaml_scalar(item.trim());
                if !item.is_empty() {
                    paths.push(item);
                }
                continue;
            }
            collecting_paths = false;
        }

        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            if key.is_empty() {
                continue;
            }
            let value = value.trim();
            if key == "paths" {
                if value.starts_with('[') && value.ends_with(']') {
                    let inner = &value[1..value.len() - 1];
                    paths.extend(
                        split_commas_outside_braces(inner)
                            .into_iter()
                            .map(|part| decode_yaml_scalar(part.trim()))
                            .filter(|part| !part.is_empty()),
                    );
                } else if value.is_empty() {
                    collecting_paths = true;
                } else {
                    paths.push(decode_yaml_scalar(value));
                }
            } else {
                map.insert(key, decode_yaml_scalar(value));
            }
        }
    }
    (map, paths)
}

/// Normalize frontmatter `paths:` entries: split on commas outside braces,
/// strip quotes, expand `{a,b}` braces, and drop trailing `/**`. Returns
/// `None` when nothing is left or every glob is `**` (the rule applies
/// everywhere).
pub fn normalize_frontmatter_paths(paths: Vec<String>) -> Option<Vec<String>> {
    let mut normalized = Vec::new();
    for entry in paths {
        for part in split_commas_outside_braces(&entry) {
            let part = strip_quotes(part.trim()).trim();
            if part.is_empty() {
                continue;
            }
            for expanded in expand_braces(part) {
                let trimmed = trim_trailing_match_all(expanded.trim());
                if !trimmed.is_empty() {
                    normalized.push(trimmed.to_string());
                }
            }
        }
    }

    if normalized.is_empty() || normalized.iter().all(|glob| glob == "**") {
        None
    } else {
        Some(normalized)
    }
}

fn split_commas_outside_braces(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (idx, ch) in s.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&s[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let Some((open, close)) = first_brace_pair(pattern) else {
        return vec![pattern.to_string()];
    };

    let prefix = &pattern[..open];
    let inner = &pattern[open + 1..close];
    let suffix = &pattern[close + 1..];
    let mut out = Vec::new();
    for option in split_commas_outside_braces(inner) {
        let combined = format!("{prefix}{option}{suffix}");
        out.extend(expand_braces(&combined));
    }
    out
}

fn first_brace_pair(pattern: &str) -> Option<(usize, usize)> {
    let mut open = None;
    let mut depth = 0usize;
    for (idx, ch) in pattern.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    open = Some(idx);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return open.map(|open| (open, idx));
                }
            }
            _ => {}
        }
    }
    None
}

fn trim_trailing_match_all(mut glob: &str) -> &str {
    while let Some(stripped) = glob.strip_suffix("/**") {
        glob = stripped;
    }
    glob
}

/// Decode a small subset of YAML scalar quoting used by memory frontmatter.
fn decode_yaml_scalar(s: &str) -> String {
    if s.len() >= 2 {
        if s.starts_with('\'') && s.ends_with('\'') {
            return s[1..s.len() - 1].replace("''", "'");
        }
        if s.starts_with('"') && s.ends_with('"') {
            return decode_double_quoted_yaml(&s[1..s.len() - 1]);
        }
    }
    s.to_string()
}

fn decode_double_quoted_yaml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Strip surrounding matching quotes from a string.
fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_frontmatter() {
        let input = "---\nname: test memory\ndescription: a test\ntype: user\n---\nBody content";
        let parsed = parse_frontmatter(input);
        assert_eq!(parsed.frontmatter.name(), Some("test memory"));
        assert_eq!(parsed.frontmatter.description(), Some("a test"));
        assert_eq!(parsed.frontmatter.memory_type(), Some("user"));
        assert_eq!(parsed.body, "Body content");
    }

    #[test]
    fn parse_no_frontmatter() {
        let input = "Just plain text\nno frontmatter here";
        let parsed = parse_frontmatter(input);
        assert!(parsed.frontmatter.fields.is_empty());
        assert_eq!(parsed.body, input);
    }

    #[test]
    fn parse_unclosed_frontmatter() {
        let input = "---\nname: orphan\nno closing delimiter";
        let parsed = parse_frontmatter(input);
        assert!(parsed.frontmatter.fields.is_empty());
        assert_eq!(parsed.body, input);
    }

    #[test]
    fn parse_quoted_values() {
        let input = "---\nname: \"quoted value\"\ndescription: 'single quoted'\n---\nbody";
        let parsed = parse_frontmatter(input);
        assert_eq!(parsed.frontmatter.name(), Some("quoted value"));
        assert_eq!(parsed.frontmatter.description(), Some("single quoted"));
    }

    #[test]
    fn parse_empty_frontmatter() {
        let input = "---\n---\nbody content";
        let parsed = parse_frontmatter(input);
        assert!(parsed.frontmatter.fields.is_empty());
        assert_eq!(parsed.body, "body content");
    }

    #[test]
    fn parse_frontmatter_with_comments() {
        let input = "---\n# comment\nname: test\n---\nbody";
        let parsed = parse_frontmatter(input);
        assert_eq!(parsed.frontmatter.name(), Some("test"));
        assert_eq!(parsed.body, "body");
    }

    #[test]
    fn parse_frontmatter_with_bom() {
        let input = "\u{feff}---\nname: test\n---\nbody";
        let parsed = parse_frontmatter(input);
        assert_eq!(parsed.frontmatter.name(), Some("test"));
    }

    #[test]
    fn parse_value_with_colon() {
        let input = "---\ndescription: key: value pair inside\n---\nbody";
        let parsed = parse_frontmatter(input);
        assert_eq!(
            parsed.frontmatter.description(),
            Some("key: value pair inside")
        );
    }

    #[test]
    fn parse_windows_line_endings() {
        let input = "---\r\nname: test\r\ntype: feedback\r\n---\r\nbody";
        let parsed = parse_frontmatter(input);
        assert_eq!(parsed.frontmatter.name(), Some("test"));
        assert_eq!(parsed.frontmatter.memory_type(), Some("feedback"));
    }

    #[test]
    fn normalizes_paths_frontmatter_like_typescript() {
        assert_eq!(
            normalize_frontmatter_paths(vec!["src/*.{rs,toml}, crates/**".to_string()]),
            Some(vec![
                "src/*.rs".to_string(),
                "src/*.toml".to_string(),
                "crates".to_string()
            ])
        );
        assert_eq!(
            normalize_frontmatter_paths(vec!["{a,b}/{c,d}".to_string()]),
            Some(vec![
                "a/c".to_string(),
                "a/d".to_string(),
                "b/c".to_string(),
                "b/d".to_string()
            ])
        );
    }

    #[test]
    fn parse_single_quoted_values_unescapes_doubled_quotes() {
        let input =
            "---\nname: 'Bob''s note: #1'\ndescription: '*leading and [flow] {chars}'\n---\nbody";
        let parsed = parse_frontmatter(input);
        assert_eq!(parsed.frontmatter.name(), Some("Bob's note: #1"));
        assert_eq!(
            parsed.frontmatter.description(),
            Some("*leading and [flow] {chars}")
        );
    }

    #[test]
    fn parse_double_quoted_values_unescapes_common_sequences() {
        let input =
            "---\nname: \"quoted \\\"value\\\"\"\ndescription: \"C:\\\\tmp\\\\file\"\n---\nbody";
        let parsed = parse_frontmatter(input);
        assert_eq!(parsed.frontmatter.name(), Some("quoted \"value\""));
        assert_eq!(parsed.frontmatter.description(), Some("C:\\tmp\\file"));
    }

    #[test]
    fn strip_quotes_works() {
        assert_eq!(strip_quotes("\"hello\""), "hello");
        assert_eq!(strip_quotes("'hello'"), "hello");
        assert_eq!(strip_quotes("hello"), "hello");
        assert_eq!(strip_quotes("\"\""), "");
        assert_eq!(strip_quotes("\"mismatched'"), "\"mismatched'");
    }

    #[test]
    fn frontmatter_missing_fields_return_none() {
        let fm = Frontmatter::default();
        assert_eq!(fm.name(), None);
        assert_eq!(fm.description(), None);
        assert_eq!(fm.memory_type(), None);
    }
}
