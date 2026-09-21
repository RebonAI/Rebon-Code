//! Shared text helpers used across this crate:
//!
//! * tag extraction — the text between `<tag>` and `</tag>`.
//! * attribute extraction — the value of `name="value"` inside an
//!   already isolated attribute area.
//! * char-based truncation to a width, appending a `…` ellipsis.
//! * conversion of a local path into a `file://` URL.
//! * `count noun` singular/plural formatting.
//! * display-name shortening of `plugin:server:name` identifiers.
//! * parsing of the `<channel ...>` user-message wrapper and of the
//!   `<mcp-resource-update ...>` / `<mcp-polling-update ...>` update tags.

/// Attribute extracted from a tag-like string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedAttribute {
    /// Attribute value.
    pub value: String,
}

/// Resource update kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceUpdateKind {
    /// `<mcp-resource-update ...>`
    Resource,
    /// `<mcp-polling-update ...>`
    Polling,
}

/// One parsed `<mcp-resource-update ...>` or `<mcp-polling-update ...>` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceUpdate {
    /// Resource vs polling.
    pub kind: ResourceUpdateKind,
    /// Server name.
    pub server: String,
    /// URI for resource updates, tool name for polling updates.
    pub target: String,
    /// Optional reason text.
    pub reason: Option<String>,
}

/// One parsed `<channel ...>` user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserChannelSummary {
    /// Source/server name.
    pub source: String,
    /// Optional user attribute.
    pub user: Option<String>,
    /// Normalized body text.
    pub body: String,
}

/// Text between `<tag>` and `</tag>`, or `None` when either is missing.
pub fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].to_string())
}

/// Extracts `attr="..."` from an already isolated attribute area.
pub fn extract_attr(attrs: &str, name: &str) -> Option<ExtractedAttribute> {
    let needle = format!(r#"{name}=""#);
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(ExtractedAttribute {
        value: rest[..end].to_string(),
    })
}

/// Small char-based width truncator.
pub fn truncate_to_width(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "\u{2026}".to_string();
    }
    chars[..width - 1].iter().collect::<String>() + "\u{2026}"
}

/// Converts a local path to a simple file URL.
pub fn simple_file_url(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        format!("file://{normalized}")
    } else {
        format!("file:///{normalized}")
    }
}

/// Small plural helper for `count noun`.
pub fn humanize_bool_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// `plugin:slack-channel:slack` -> `slack`
pub fn display_server_name(name: &str) -> String {
    match name.rfind(':') {
        Some(idx) => name[idx + 1..].to_string(),
        None => name.to_string(),
    }
}

/// Parses the `<channel source="..." user="...">body</channel>` wrapper.
pub fn parse_channel_message(text: &str) -> Option<UserChannelSummary> {
    let start = text.find("<channel ")?;
    let open_end = text[start..].find('>')? + start;
    let attrs = &text[start + "<channel ".len()..open_end];
    let source = extract_attr(attrs, "source")?.value;
    let user = extract_attr(attrs, "user").map(|x| x.value);
    let close = text[open_end + 1..].find("</channel>")? + open_end + 1;
    let body = text[open_end + 1..close]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(UserChannelSummary { source, user, body })
}

/// Parses `<mcp-resource-update ...>` and `<mcp-polling-update ...>` tags.
pub fn parse_resource_updates(text: &str) -> Vec<ResourceUpdate> {
    let mut updates = Vec::new();
    let mut cursor = 0usize;
    while let Some(idx) = text[cursor..].find("<mcp-resource-update ") {
        let start = cursor + idx;
        let open_end = match text[start..].find('>') {
            Some(v) => start + v,
            None => break,
        };
        let attrs = &text[start + "<mcp-resource-update ".len()..open_end];
        let close = match text[open_end + 1..].find("</mcp-resource-update>") {
            Some(v) => open_end + 1 + v,
            None => break,
        };
        let inner = &text[open_end + 1..close];
        updates.push(ResourceUpdate {
            kind: ResourceUpdateKind::Resource,
            server: extract_attr(attrs, "server")
                .map(|x| x.value)
                .unwrap_or_default(),
            target: extract_attr(attrs, "uri")
                .map(|x| x.value)
                .unwrap_or_default(),
            reason: extract_tag(inner, "reason").map(|x| x.trim().to_string()),
        });
        cursor = close + "</mcp-resource-update>".len();
    }

    cursor = 0;
    while let Some(idx) = text[cursor..].find("<mcp-polling-update ") {
        let start = cursor + idx;
        let open_end = match text[start..].find('>') {
            Some(v) => start + v,
            None => break,
        };
        let attrs = &text[start + "<mcp-polling-update ".len()..open_end];
        let close = match text[open_end + 1..].find("</mcp-polling-update>") {
            Some(v) => open_end + 1 + v,
            None => break,
        };
        let inner = &text[open_end + 1..close];
        updates.push(ResourceUpdate {
            kind: ResourceUpdateKind::Polling,
            server: extract_attr(attrs, "server")
                .map(|x| x.value)
                .unwrap_or_default(),
            target: extract_attr(attrs, "tool")
                .map(|x| x.value)
                .unwrap_or_default(),
            reason: extract_tag(inner, "reason").map(|x| x.trim().to_string()),
        });
        cursor = close + "</mcp-polling-update>".len();
    }

    updates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tag_finds_simple_content() {
        assert_eq!(extract_tag("<x>hi</x>", "x").as_deref(), Some("hi"));
        assert_eq!(extract_tag("<x>hi</x>", "y"), None);
    }

    #[test]
    fn truncate_to_width_uses_ellipsis() {
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("abc", 4), "abc");
    }

    #[test]
    fn simple_file_url_normalizes_backslashes() {
        assert_eq!(simple_file_url(r"C:\tmp\a.png"), "file:///C:/tmp/a.png");
    }

    #[test]
    fn parse_channel_message_extracts_attrs_and_body() {
        let parsed = parse_channel_message(
            "<channel source=\"plugin:slack-channel:slack\" user=\"alice\">\nhello   world\n</channel>",
        )
        .unwrap();
        assert_eq!(parsed.source, "plugin:slack-channel:slack");
        assert_eq!(parsed.user.as_deref(), Some("alice"));
        assert_eq!(parsed.body, "hello world");
    }

    #[test]
    fn parse_resource_updates_handles_resource_and_polling() {
        let text = concat!(
            "<mcp-resource-update server=\"srv\" uri=\"file:///a\"><reason>x</reason></mcp-resource-update>",
            "<mcp-polling-update type=\"tool\" server=\"srv2\" tool=\"grep\"><reason>y</reason></mcp-polling-update>"
        );
        let updates = parse_resource_updates(text);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].kind, ResourceUpdateKind::Resource);
        assert_eq!(updates[1].kind, ResourceUpdateKind::Polling);
    }
}
