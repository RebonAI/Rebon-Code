//! Shared message wrapper helpers.
//!
//! Provides common row containers and decoration data used by the
//! message renderers.
use rebon_design_system::{format_shortcut_for_current_platform, format_shortcut_hint};

/// Whether a parent response wrapper is already in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageResponseContextValue(pub bool);

/// What the response wrapper draws around one message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageResponseDisplay {
    /// Whether the wrapper should be skipped because a parent response already exists.
    pub nested_passthrough: bool,
    /// Explicit height for the wrapper, when the caller sets one.
    pub height: Option<u16>,
    /// Prefix text (`"  ⏿ "` in practice).
    pub prefix: &'static str,
    /// Whether the wrapper needs an outer `RatchetLock::Offscreen` ratchet.
    pub wrap_in_ratchet: bool,
}

/// Display for the "ctrl+o to expand" hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtrlOToExpandDisplay {
    /// Whether the hint should be shown.
    pub visible: bool,
    /// Pre-rendered text when visible.
    pub text: Option<String>,
}

/// Display for the interrupted-by-user row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedByUserDisplay {
    /// Leading segment.
    pub prefix: &'static str,
    /// Trailing segment.
    pub suffix: &'static str,
}

/// Compact boundary banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactBoundaryDisplay {
    /// Shortcut used in the message.
    pub history_shortcut: String,
    /// Final text.
    pub text: String,
}

/// The hint is hidden inside a sub-agent and inside a virtualized list.
pub fn should_show_ctrl_o_to_expand(is_in_sub_agent: bool, in_virtual_list: bool) -> bool {
    !is_in_sub_agent && !in_virtual_list
}

/// Platform shortcut hint that tells the user how to expand.
pub fn ctrl_o_to_expand_text(shortcut: &str) -> String {
    format_shortcut_hint(shortcut, "expand", true, false).plain_text
}

/// Project the hint's visibility and text from the surrounding context.
pub fn project_ctrl_o_to_expand(
    is_in_sub_agent: bool,
    in_virtual_list: bool,
    shortcut: &str,
) -> CtrlOToExpandDisplay {
    let visible = should_show_ctrl_o_to_expand(is_in_sub_agent, in_virtual_list);
    CtrlOToExpandDisplay {
        visible,
        text: visible.then(|| ctrl_o_to_expand_text(shortcut)),
    }
}

/// The two halves of the interrupted-by-user line, as this build renders
/// them.
pub fn interrupted_by_user_texts() -> InterruptedByUserDisplay {
    InterruptedByUserDisplay {
        prefix: "Interrupted ",
        suffix: "· What should Rebon do instead?",
    }
}

/// Project the response wrapper's prefix and ratchet flag.
pub fn project_message_response(
    height: Option<u16>,
    ctx: MessageResponseContextValue,
) -> MessageResponseDisplay {
    if ctx.0 {
        return MessageResponseDisplay {
            nested_passthrough: true,
            height,
            prefix: "",
            wrap_in_ratchet: false,
        };
    }
    MessageResponseDisplay {
        nested_passthrough: false,
        height,
        prefix: "  ⏿ ",
        wrap_in_ratchet: height.is_none(),
    }
}

/// The compact-boundary banner text.
pub fn project_compact_boundary_message(history_shortcut: &str) -> CompactBoundaryDisplay {
    CompactBoundaryDisplay {
        history_shortcut: format_shortcut_for_current_platform(history_shortcut),
        text: format!(
            "✻ Conversation compacted ({} for history)",
            format_shortcut_for_current_platform(history_shortcut)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_o_hidden_in_subagent_or_virtual_list() {
        assert!(!project_ctrl_o_to_expand(true, false, "ctrl+o").visible);
        assert!(!project_ctrl_o_to_expand(false, true, "ctrl+o").visible);
        assert!(project_ctrl_o_to_expand(false, false, "ctrl+o").visible);
    }

    #[test]
    fn ctrl_o_text_is_pinned() {
        assert_eq!(ctrl_o_to_expand_text("ctrl+o"), "(Ctrl+O to expand)");
    }

    #[test]
    fn interrupted_by_user_uses_external_build_suffix() {
        let display = interrupted_by_user_texts();
        assert_eq!(display.prefix, "Interrupted ");
        assert_eq!(display.suffix, "· What should Rebon do instead?");
    }

    #[test]
    fn nested_message_response_passthroughs() {
        let d = project_message_response(None, MessageResponseContextValue(true));
        assert!(d.nested_passthrough);
        assert!(!d.wrap_in_ratchet);
    }

    #[test]
    fn top_level_message_response_wraps_in_ratchet_when_height_absent() {
        let d = project_message_response(None, MessageResponseContextValue(false));
        assert_eq!(d.prefix, "  ⏿ ");
        assert!(d.wrap_in_ratchet);
    }

    #[test]
    fn compact_boundary_message_threads_shortcut() {
        let d = project_compact_boundary_message("ctrl+o");
        assert!(d.text.contains("Ctrl+O"));
    }
}
