//! Permission-rule string parsing and escaping helpers.
//!
//! Implements rule-content escaping/unescaping and permission-rule value
//! parsing/serialization. A rule names the tool it is about, verbatim: the
//! `Task` → `Agent` rewrite that used to sit in front of every parse is gone.

use crate::types::PermissionRuleValue;

pub fn escape_rule_content(content: &str) -> String {
    content
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

pub fn unescape_rule_content(content: &str) -> String {
    content
        .replace("\\(", "(")
        .replace("\\)", ")")
        .replace("\\\\", "\\")
}

pub fn permission_rule_value_from_string(rule_string: &str) -> PermissionRuleValue {
    let open_paren_index = find_first_unescaped_char(rule_string, '(');
    if open_paren_index.is_none() {
        return PermissionRuleValue::new(rule_string, Option::<String>::None);
    }

    let open_paren_index = open_paren_index.expect("checked above");
    let close_paren_index = find_last_unescaped_char(rule_string, ')');
    if close_paren_index.is_none() || close_paren_index <= Some(open_paren_index) {
        return PermissionRuleValue::new(rule_string, Option::<String>::None);
    }

    let close_paren_index = close_paren_index.expect("checked above");
    if close_paren_index != rule_string.len() - 1 {
        return PermissionRuleValue::new(rule_string, Option::<String>::None);
    }

    let tool_name = &rule_string[..open_paren_index];
    let raw_content = &rule_string[open_paren_index + 1..close_paren_index];
    if tool_name.is_empty() {
        return PermissionRuleValue::new(rule_string, Option::<String>::None);
    }

    if raw_content.is_empty() || raw_content == "*" {
        return PermissionRuleValue::new(tool_name, Option::<String>::None);
    }

    PermissionRuleValue::new(tool_name, Some(unescape_rule_content(raw_content)))
}

pub fn permission_rule_value_to_string(rule_value: &PermissionRuleValue) -> String {
    match &rule_value.rule_content {
        Some(rule_content) if !rule_content.is_empty() => {
            format!(
                "{}({})",
                rule_value.tool_name,
                escape_rule_content(rule_content)
            )
        }
        _ => rule_value.tool_name.clone(),
    }
}

fn find_first_unescaped_char(input: &str, needle: char) -> Option<usize> {
    for (idx, ch) in input.char_indices() {
        if ch == needle && is_unescaped(input, idx) {
            return Some(idx);
        }
    }
    None
}

fn find_last_unescaped_char(input: &str, needle: char) -> Option<usize> {
    for (idx, ch) in input.char_indices().rev() {
        if ch == needle && is_unescaped(input, idx) {
            return Some(idx);
        }
    }
    None
}

fn is_unescaped(input: &str, idx: usize) -> bool {
    let mut backslash_count = 0usize;
    for ch in input[..idx].chars().rev() {
        if ch != '\\' {
            break;
        }
        backslash_count += 1;
    }
    backslash_count % 2 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rule names the tool it is about and nothing else.
    ///
    /// `Task` was `Agent`'s name in a much older release, and every parse
    /// used to rewrite it. A rule written `Task(...)` therefore silently
    /// governed the `Agent` tool — a name no tool has answered to for a long
    /// time, quietly still deciding what a delegated agent could do. It is
    /// now an ordinary tool name that matches nothing.
    #[test]
    fn a_rule_keeps_the_tool_name_it_was_written_with() {
        assert_eq!(
            permission_rule_value_from_string("Task"),
            PermissionRuleValue::new("Task", Option::<String>::None)
        );
        assert_eq!(
            permission_rule_value_from_string("Task(explore:*)"),
            PermissionRuleValue::new("Task", Some("explore:*"))
        );
        assert_eq!(
            permission_rule_value_from_string("Agent(explore:*)"),
            PermissionRuleValue::new("Agent", Some("explore:*"))
        );
    }

    #[test]
    fn escapes_and_unescapes_rule_content() {
        let escaped = escape_rule_content(r#"echo "test\nvalue" && print(1)"#);
        assert_eq!(escaped, r#"echo "test\\nvalue" && print\(1\)"#);
        assert_eq!(
            unescape_rule_content(r#"python -c "print\\(1\\)" \\ path"#),
            r#"python -c "print\(1\)" \ path"#
        );
    }

    #[test]
    fn parses_plain_tool_name() {
        assert_eq!(
            permission_rule_value_from_string("Bash"),
            PermissionRuleValue::new("Bash", Option::<String>::None)
        );
    }

    #[test]
    fn parses_rule_content_and_unescapes_parentheses() {
        assert_eq!(
            permission_rule_value_from_string(r#"Bash(python -c "print\(1\)")"#),
            PermissionRuleValue::new("Bash", Some(r#"python -c "print(1)""#))
        );
    }

    #[test]
    fn collapses_empty_and_wildcard_content_to_tool_only() {
        assert_eq!(
            permission_rule_value_from_string("Bash()"),
            PermissionRuleValue::new("Bash", Option::<String>::None)
        );
        assert_eq!(
            permission_rule_value_from_string("Bash(*)"),
            PermissionRuleValue::new("Bash", Option::<String>::None)
        );
    }

    #[test]
    fn malformed_parentheses_fall_back_to_tool_name() {
        assert_eq!(
            permission_rule_value_from_string("Bash(foo)bar"),
            PermissionRuleValue::new("Bash(foo)bar", Option::<String>::None)
        );
        assert_eq!(
            permission_rule_value_from_string("(foo)"),
            PermissionRuleValue::new("(foo)", Option::<String>::None)
        );
    }

    #[test]
    fn serializes_rule_value_with_escaping() {
        let rule = PermissionRuleValue::new("Bash", Some(r#"python -c "print(1)""#));
        assert_eq!(
            permission_rule_value_to_string(&rule),
            r#"Bash(python -c "print\(1\)")"#
        );
    }

    #[test]
    fn round_trips_escaped_backslashes() {
        let rule = PermissionRuleValue::new("Bash", Some(r#"echo "test\nvalue""#));
        let encoded = permission_rule_value_to_string(&rule);
        assert_eq!(
            permission_rule_value_from_string(&encoded),
            PermissionRuleValue::new("Bash", Some(r#"echo "test\nvalue""#))
        );
    }
}
