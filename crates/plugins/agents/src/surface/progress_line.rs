//! Agent progress-line formatter ([`format_agent_progress_line`]).
//!
//! Formats byte counts, token counts, elapsed time, and compact progress rows.
//! Each agent row includes:
//!
//! 1. A tree-character prefix (`├─` for non-last, `└─` for last).
//! 2. A header built from the agent type and optional description, or from the
//! name/description fallback used when type display is hidden.
//! 3. Tool-use and token counts when the agent is not backgrounded.
//! 4. A second status line while foregrounded.
//!
//! This module projects the inputs to a [`ProgressLineDisplay`]
//! struct the consumer renders.

/// Inputs to [`format_agent_progress_line`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProgressInputs {
    /// Stable agent type / kebab-case identifier.
    pub agent_type: String,
    /// Optional one-line description (rendered after the type).
    pub description: Option<String>,
    /// Optional bold name (only used in `hide_type` mode).
    pub name: Option<String>,
    /// Tool use count.
    pub tool_use_count: usize,
    /// Token count (`None` = hidden).
    pub tokens: Option<u64>,
    /// True if this is the last row in a group (changes tree char).
    pub is_last: bool,
    /// True once the agent has finished running.
    pub is_resolved: bool,
    /// True if the agent is async (background) — when combined with
    /// `is_resolved`, the row collapses to "Running in the background".
    pub is_async: bool,
    /// Optional last-tool info string shown while running.
    pub last_tool_info: Option<String>,
    /// Optional task description (used as the resolved status when
    /// the agent is backgrounded).
    pub task_description: Option<String>,
    /// Hide-type mode (changes the header rendering).
    pub hide_type: bool,
}

/// Pre-built display projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressLineDisplay {
    /// First line: tree char + header text.
    pub tree_char: &'static str,
    /// First line: header text (without tree char).
    pub header: String,
    /// First line: tool/token suffix (empty when backgrounded).
    pub usage_suffix: String,
    /// Second line: status text (`None` when backgrounded — suppressed).
    pub status_line: Option<String>,
    /// True if the row should be dim-colored (because the agent is
    /// still running).
    pub dim: bool,
}

/// Build the display projection for one agent row.
pub fn format_agent_progress_line(inputs: &AgentProgressInputs) -> ProgressLineDisplay {
    let tree_char = if inputs.is_last { "└─" } else { "├─" };
    let is_backgrounded = inputs.is_async && inputs.is_resolved;
    let header = build_header(inputs);
    let usage_suffix = if is_backgrounded {
        String::new()
    } else {
        build_usage_suffix(inputs)
    };
    let status_line = if is_backgrounded {
        None
    } else {
        Some(build_status_text(inputs, is_backgrounded))
    };
    ProgressLineDisplay {
        tree_char,
        header,
        usage_suffix,
        status_line,
        dim: !inputs.is_resolved,
    }
}

fn build_header(inputs: &AgentProgressInputs) -> String {
    if inputs.hide_type {
        // Prefer the explicit name, then the description, then the agent type.
        let primary = inputs
            .name
            .as_deref()
            .or(inputs.description.as_deref())
            .unwrap_or(&inputs.agent_type);
        let mut out = primary.to_string();
        if inputs.name.is_some() && inputs.description.is_some() {
            out.push_str(": ");
            out.push_str(inputs.description.as_deref().unwrap());
        }
        return out;
    }

    let mut out = inputs.agent_type.clone();
    if let Some(d) = &inputs.description {
        out.push_str(" (");
        out.push_str(d);
        out.push(')');
    }
    out
}

fn build_usage_suffix(inputs: &AgentProgressInputs) -> String {
    let mut out = format!(
        " \u{00B7} {} tool {}",
        inputs.tool_use_count,
        if inputs.tool_use_count == 1 {
            "use"
        } else {
            "uses"
        }
    );
    if let Some(tokens) = inputs.tokens {
        out.push_str(" \u{00B7} ");
        out.push_str(&format_number(tokens));
        out.push_str(" tokens");
    }
    out
}

fn build_status_text(inputs: &AgentProgressInputs, is_backgrounded: bool) -> String {
    if !inputs.is_resolved {
        return inputs
            .last_tool_info
            .clone()
            .unwrap_or_else(|| "Initializing\u{2026}".to_string());
    }
    if is_backgrounded {
        return inputs
            .task_description
            .clone()
            .unwrap_or_else(|| "Running in the background".to_string());
    }
    "Done".to_string()
}

/// Format a number with thousand separators (commas).
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> AgentProgressInputs {
        AgentProgressInputs {
            agent_type: "code-reviewer".into(),
            description: None,
            name: None,
            tool_use_count: 0,
            tokens: None,
            is_last: false,
            is_resolved: false,
            is_async: false,
            last_tool_info: None,
            task_description: None,
            hide_type: false,
        }
    }

    #[test]
    fn tree_char_non_last() {
        let i = inputs();
        let d = format_agent_progress_line(&i);
        assert_eq!(d.tree_char, "├─");
    }

    #[test]
    fn tree_char_last() {
        let mut i = inputs();
        i.is_last = true;
        let d = format_agent_progress_line(&i);
        assert_eq!(d.tree_char, "└─");
    }

    #[test]
    fn header_default_no_description() {
        let i = inputs();
        let d = format_agent_progress_line(&i);
        assert_eq!(d.header, "code-reviewer");
    }

    #[test]
    fn header_default_with_description() {
        let mut i = inputs();
        i.description = Some("reviewing".into());
        let d = format_agent_progress_line(&i);
        assert_eq!(d.header, "code-reviewer (reviewing)");
    }

    #[test]
    fn header_hide_type_uses_name() {
        let mut i = inputs();
        i.hide_type = true;
        i.name = Some("Reviewer".into());
        let d = format_agent_progress_line(&i);
        assert_eq!(d.header, "Reviewer");
    }

    #[test]
    fn header_hide_type_uses_name_and_description() {
        let mut i = inputs();
        i.hide_type = true;
        i.name = Some("Reviewer".into());
        i.description = Some("review code".into());
        let d = format_agent_progress_line(&i);
        assert_eq!(d.header, "Reviewer: review code");
    }

    #[test]
    fn header_hide_type_falls_back_to_description() {
        let mut i = inputs();
        i.hide_type = true;
        i.description = Some("review".into());
        let d = format_agent_progress_line(&i);
        assert_eq!(d.header, "review");
    }

    #[test]
    fn header_hide_type_falls_back_to_agent_type() {
        let mut i = inputs();
        i.hide_type = true;
        let d = format_agent_progress_line(&i);
        assert_eq!(d.header, "code-reviewer");
    }

    #[test]
    fn usage_suffix_singular_tool_use() {
        let mut i = inputs();
        i.tool_use_count = 1;
        let d = format_agent_progress_line(&i);
        assert!(d.usage_suffix.contains("1 tool use"));
        assert!(!d.usage_suffix.contains("uses"));
    }

    #[test]
    fn usage_suffix_plural_tool_uses() {
        let mut i = inputs();
        i.tool_use_count = 5;
        let d = format_agent_progress_line(&i);
        assert!(d.usage_suffix.contains("5 tool uses"));
    }

    #[test]
    fn usage_suffix_with_tokens() {
        let mut i = inputs();
        i.tool_use_count = 2;
        i.tokens = Some(12345);
        let d = format_agent_progress_line(&i);
        assert!(d.usage_suffix.contains("12,345 tokens"));
    }

    #[test]
    fn usage_suffix_omits_tokens_when_none() {
        let mut i = inputs();
        i.tool_use_count = 2;
        let d = format_agent_progress_line(&i);
        assert!(!d.usage_suffix.contains("tokens"));
    }

    #[test]
    fn backgrounded_suppresses_usage_and_status() {
        let mut i = inputs();
        i.is_async = true;
        i.is_resolved = true;
        let d = format_agent_progress_line(&i);
        assert!(d.usage_suffix.is_empty());
        assert!(d.status_line.is_none());
    }

    #[test]
    fn unresolved_status_uses_last_tool_info() {
        let mut i = inputs();
        i.last_tool_info = Some("Reading file…".into());
        let d = format_agent_progress_line(&i);
        assert_eq!(d.status_line.as_deref(), Some("Reading file…"));
    }

    #[test]
    fn unresolved_status_falls_back_to_initializing() {
        let i = inputs();
        let d = format_agent_progress_line(&i);
        assert_eq!(d.status_line.as_deref(), Some("Initializing\u{2026}"));
    }

    #[test]
    fn resolved_status_is_done() {
        let mut i = inputs();
        i.is_resolved = true;
        let d = format_agent_progress_line(&i);
        assert_eq!(d.status_line.as_deref(), Some("Done"));
    }

    #[test]
    fn dim_when_unresolved() {
        let i = inputs();
        let d = format_agent_progress_line(&i);
        assert!(d.dim);
    }

    #[test]
    fn not_dim_when_resolved() {
        let mut i = inputs();
        i.is_resolved = true;
        let d = format_agent_progress_line(&i);
        assert!(!d.dim);
    }

    #[test]
    fn format_number_no_separator_for_small() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn format_number_with_thousand_separator() {
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(12345), "12,345");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }
}
