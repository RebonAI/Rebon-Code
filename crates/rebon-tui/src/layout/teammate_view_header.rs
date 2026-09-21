//! Teammate-view header display projection.
//!
//! [`project_teammate_view_header`] takes the currently viewed teammate
//! (or `None`), returns `None` when
//! no teammate is currently being viewed, and otherwise builds a
//! vertically stacked box with:
//!
//! 1. A row: `"Viewing "` + colored bold `@`-prefixed agent name + the
//!    dim separator + the `esc` / `return` keyboard-shortcut hint.
//! 2. A dim task prompt text line.
//!
//! The projection carries plain data — no box or hint primitive of its
//! own — so the consumer decides how to draw the rows.

/// Static separator between the agent name and the esc hint: one ASCII
/// space, one U+00B7 middle-dot, one ASCII space.
pub const HINT_SEPARATOR: &str = " \u{00B7} ";

/// The keyboard shortcut rendered at the right of the header row:
/// [`EscReturnHint::SHORTCUT`] `"esc"` + [`EscReturnHint::ACTION`] `"return"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscReturnHint;

impl EscReturnHint {
    pub const SHORTCUT: &'static str = "esc";
    pub const ACTION: &'static str = "return";
}

/// Input: the fields the header projects out of a viewed teammate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewedTeammate {
    /// Resolved color name for the agent (e.g. `"cyan"`, or a hex like
    /// `"#ff00aa"`) — we assume the consumer has already resolved it.
    pub name_color: String,
    /// The agent's display name (without the `@` prefix).
    pub agent_name: String,
    /// The task prompt to show as a second dim row.
    pub prompt: String,
}

/// The output display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateViewHeaderDisplay {
    /// Literal `"Viewing "` plain text. Pinned here so the consumer
    /// doesn't have to maintain its own copy.
    pub prefix: &'static str,
    /// The agent's name with a leading `@` — rendered bold in the given color.
    pub agent_name_with_at: String,
    pub name_color: String,
    /// `" · "` separator between name and the esc hint.
    pub hint_separator: &'static str,
    pub esc_hint: EscReturnHint,
    /// The task prompt text rendered dim below the first row.
    pub prompt: String,
}

/// Projection. Returns `None` when no teammate is being viewed.
pub fn project_teammate_view_header(
    viewed: Option<&ViewedTeammate>,
) -> Option<TeammateViewHeaderDisplay> {
    let v = viewed?;
    Some(TeammateViewHeaderDisplay {
        prefix: "Viewing ",
        agent_name_with_at: format!("@{}", v.agent_name),
        name_color: v.name_color.clone(),
        hint_separator: HINT_SEPARATOR,
        esc_hint: EscReturnHint,
        prompt: v.prompt.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vt() -> ViewedTeammate {
        ViewedTeammate {
            name_color: "cyan".into(),
            agent_name: "alice".into(),
            prompt: "Fix the build".into(),
        }
    }

    #[test]
    fn hidden_when_no_teammate() {
        assert_eq!(project_teammate_view_header(None), None);
    }

    #[test]
    fn visible_with_full_projection() {
        let d = project_teammate_view_header(Some(&vt())).unwrap();
        assert_eq!(d.prefix, "Viewing ");
        assert_eq!(d.agent_name_with_at, "@alice");
        assert_eq!(d.name_color, "cyan");
        assert_eq!(d.hint_separator, " \u{00B7} ");
        assert_eq!(d.prompt, "Fix the build");
    }

    #[test]
    fn prepends_at_sign_even_if_empty_name() {
        let mut v = vt();
        v.agent_name.clear();
        let d = project_teammate_view_header(Some(&v)).unwrap();
        assert_eq!(d.agent_name_with_at, "@");
    }

    #[test]
    fn preserves_raw_name_color_string() {
        let mut v = vt();
        v.name_color = "#ff00aa".into();
        let d = project_teammate_view_header(Some(&v)).unwrap();
        assert_eq!(d.name_color, "#ff00aa");
    }

    #[test]
    fn empty_prompt_allowed() {
        let mut v = vt();
        v.prompt.clear();
        let d = project_teammate_view_header(Some(&v)).unwrap();
        assert_eq!(d.prompt, "");
    }

    #[test]
    fn long_prompt_not_truncated_here() {
        // Truncation is a rendering concern — the projection passes through.
        let mut v = vt();
        v.prompt = "a".repeat(500);
        let d = project_teammate_view_header(Some(&v)).unwrap();
        assert_eq!(d.prompt.len(), 500);
    }

    #[test]
    fn hint_separator_is_space_middot_space() {
        assert_eq!(HINT_SEPARATOR, " \u{00B7} ");
        assert_eq!(HINT_SEPARATOR.chars().count(), 3);
    }

    #[test]
    fn esc_hint_shortcut_is_esc() {
        assert_eq!(EscReturnHint::SHORTCUT, "esc");
    }

    #[test]
    fn esc_hint_action_is_return() {
        assert_eq!(EscReturnHint::ACTION, "return");
    }

    #[test]
    fn unicode_agent_name_preserved() {
        let mut v = vt();
        v.agent_name = "小明".into();
        let d = project_teammate_view_header(Some(&v)).unwrap();
        assert_eq!(d.agent_name_with_at, "@小明");
    }
}
