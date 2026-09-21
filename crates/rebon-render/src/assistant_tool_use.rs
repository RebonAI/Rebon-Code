//! Pure branch logic for projecting one tool-use row.
//!
//! Tool-specific callbacks stay injected as plain strings and flags, so this
//! crate remains leaf-safe and independent of any particular tool registry.

use std::collections::HashSet;

use rebon_width::WidthStr;

/// Minimal `tool_use` block metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolUseInvocation {
    /// Block id.
    pub id: String,
    /// Block name.
    pub name: String,
}

/// Tool callback outputs injected into the projector.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssistantToolRenderOutputs {
    /// Main tool message text. `None` when the tool produced none.
    pub message: Option<String>,
    /// Tool-specific tag, when the tool supplies one.
    pub tag: Option<String>,
    /// Tool-specific progress text, excluding the hook row.
    pub progress_message: Option<String>,
    /// Queued-message text, when the tool supplies one.
    pub queued_message: Option<String>,
    /// Precomputed hook-progress projection text, if any.
    pub hook_progress_message: Option<String>,
}

/// Tool descriptor resolved from the injected callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolDefinition {
    /// Tool name.
    pub name: String,
    /// Label the tool shows for this invocation.
    pub user_facing_name: String,
    /// Background color for that label, when the tool supplies one.
    pub user_facing_name_background_color: Option<String>,
    /// Whether the tool is a transparent wrapper; defaults to `false`.
    pub is_transparent_wrapper: bool,
    /// Render callback outputs.
    pub renders: AssistantToolRenderOutputs,
}

/// Everything the projection reads for one tool-use row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolUseInput {
    /// The tool-use block being projected.
    pub tool_use: AssistantToolUseInvocation,
    /// Whether a tool registry was available at all.
    pub tools_available: bool,
    /// Tool descriptor resolved from the callbacks, when lookup succeeded.
    pub tool: Option<AssistantToolDefinition>,
    /// Whether extra vertical margin surrounds the row.
    pub add_margin: bool,
    /// Tool-use ids currently in progress.
    pub in_progress_tool_use_ids: HashSet<String>,
    /// Tool-use ids that have resolved.
    pub resolved_tool_use_ids: HashSet<String>,
    /// Tool-use ids that errored.
    pub errored_tool_use_ids: HashSet<String>,
    /// Whether the loader may animate.
    pub should_animate: bool,
    /// Whether the leading dot is shown.
    pub should_show_dot: bool,
    /// Selected-message background, when one is in use.
    pub background: Option<String>,
    /// Tool-use id of the pending worker request, if any.
    pub pending_worker_tool_use_id: Option<String>,
    /// Platform-specific dot glyph.
    pub dot_glyph: String,
}

/// What a tool-use row renders as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantToolUseProjection {
    /// The row is hidden; the payload records why.
    Hidden(AssistantToolUseHiddenReason),
    /// Transparent-wrapper branch that only renders progress.
    Transparent(AssistantToolProgressDisplay),
    /// Standard visible row.
    Row(AssistantToolUseRowDisplay),
}

/// Reasons a tool-use row renders nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantToolUseHiddenReason {
    /// Tool lookup failed, so the row logs the reason instead of drawing.
    MissingTool {
        /// Exact log message for the failed tool lookup.
        log_message: String,
    },
    /// Transparent wrappers hide queued tool uses.
    TransparentWrapperQueued,
    /// Transparent wrappers hide resolved tool uses.
    TransparentWrapperResolved,
    /// Empty user-facing names render nothing.
    EmptyUserFacingName,
    /// The tool's main message produced nothing.
    NullRenderedToolUseMessage,
}

/// Shared progress area display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolProgressDisplay {
    /// Optional selected-message background.
    pub background: Option<String>,
    /// Optional hook-progress child.
    pub hook_progress_message: Option<String>,
    /// Optional tool-specific progress child.
    pub progress_message: Option<String>,
}

/// Standard visible row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolUseRowDisplay {
    /// Top margin, in rows.
    pub margin_top: u8,
    /// Selected-message background.
    pub background: Option<String>,
    /// Header row.
    pub header: AssistantToolHeaderDisplay,
    /// Active status row for unresolved/non-queued tools.
    pub progress: Option<AssistantToolSecondaryDisplay>,
    /// Queued-message row.
    pub queued_message: Option<String>,
}

/// Header row display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolHeaderDisplay {
    /// Minimum width: the display width of the user-facing tool label.
    pub min_width: usize,
    /// Leading dot or loader.
    pub leading: Option<AssistantToolLeadingDisplay>,
    /// User-facing tool label.
    pub user_facing_name: String,
    /// Background color for the tool label.
    pub user_facing_name_background_color: Option<String>,
    /// Whether the label uses the inverse text color.
    pub inverse_text: bool,
    /// Optional parenthesized message content, without the parentheses.
    pub rendered_message: Option<String>,
    /// Optional tool-specific tag.
    pub tag: Option<String>,
}

/// Leading indicator display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantToolLeadingDisplay {
    /// Queued state: a dimmed dot in a two-cell-wide box.
    QueuedDot {
        /// Platform-specific glyph.
        glyph: String,
        /// Whether the dot is dimmed.
        dim: bool,
        /// Width reserved for the dot cell; always 2.
        min_width: u8,
    },
    /// Active or resolved state: the loader indicator.
    Loader {
        /// Whether the loader may animate.
        should_animate: bool,
        /// True while the tool use is unresolved.
        is_unresolved: bool,
        /// True when the tool use errored.
        is_error: bool,
    },
}

/// Secondary rows under the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantToolSecondaryDisplay {
    /// Waiting-for-permission line, laid out at a fixed height of 1.
    WaitingForPermission {
        /// Height of the line, in rows.
        height: u8,
        /// Dim line copy.
        message: &'static str,
    },
    /// Hook-progress + tool-progress branch.
    Progress(AssistantToolProgressDisplay),
}

/// Projects one tool-use row: either a reason to hide it, the
/// transparent-wrapper progress area, or the full row.
pub fn project_assistant_tool_use(input: &AssistantToolUseInput) -> AssistantToolUseProjection {
    let Some(tool) = &input.tool else {
        let log_message = if input.tools_available {
            format!("Tool {} not found", input.tool_use.name)
        } else {
            format!("Tools array is undefined for tool {}", input.tool_use.name)
        };
        return AssistantToolUseProjection::Hidden(AssistantToolUseHiddenReason::MissingTool {
            log_message,
        });
    };

    let is_resolved = input.resolved_tool_use_ids.contains(&input.tool_use.id);
    let is_queued = !input.in_progress_tool_use_ids.contains(&input.tool_use.id) && !is_resolved;
    let is_waiting_for_permission =
        input.pending_worker_tool_use_id.as_deref() == Some(input.tool_use.id.as_str());

    if tool.is_transparent_wrapper {
        if is_queued {
            return AssistantToolUseProjection::Hidden(
                AssistantToolUseHiddenReason::TransparentWrapperQueued,
            );
        }
        if is_resolved {
            return AssistantToolUseProjection::Hidden(
                AssistantToolUseHiddenReason::TransparentWrapperResolved,
            );
        }
        return AssistantToolUseProjection::Transparent(AssistantToolProgressDisplay {
            background: input.background.clone(),
            hook_progress_message: tool.renders.hook_progress_message.clone(),
            progress_message: tool.renders.progress_message.clone(),
        });
    }

    if tool.user_facing_name.is_empty() {
        return AssistantToolUseProjection::Hidden(
            AssistantToolUseHiddenReason::EmptyUserFacingName,
        );
    }

    let Some(rendered_tool_use_message) = &tool.renders.message else {
        return AssistantToolUseProjection::Hidden(
            AssistantToolUseHiddenReason::NullRenderedToolUseMessage,
        );
    };

    let header = AssistantToolHeaderDisplay {
        min_width: WidthStr::width(tool.user_facing_name.as_str())
            + if input.should_show_dot { 2 } else { 0 },
        leading: build_leading_display(input, is_queued, is_resolved),
        user_facing_name: tool.user_facing_name.clone(),
        user_facing_name_background_color: tool.user_facing_name_background_color.clone(),
        inverse_text: tool.user_facing_name_background_color.is_some(),
        rendered_message: (!rendered_tool_use_message.is_empty())
            .then(|| rendered_tool_use_message.clone()),
        tag: tool.renders.tag.clone(),
    };

    let progress = if !is_resolved && !is_queued {
        if is_waiting_for_permission {
            Some(AssistantToolSecondaryDisplay::WaitingForPermission {
                height: 1,
                message: "Waiting for permission\u{2026}",
            })
        } else {
            let progress_display = AssistantToolProgressDisplay {
                background: None,
                hook_progress_message: tool.renders.hook_progress_message.clone(),
                progress_message: tool.renders.progress_message.clone(),
            };
            (progress_display.hook_progress_message.is_some()
                || progress_display.progress_message.is_some())
            .then_some(AssistantToolSecondaryDisplay::Progress(progress_display))
        }
    } else {
        None
    };

    let queued_message = (!is_resolved && is_queued)
        .then(|| tool.renders.queued_message.clone())
        .flatten();

    AssistantToolUseProjection::Row(AssistantToolUseRowDisplay {
        margin_top: u8::from(input.add_margin),
        background: input.background.clone(),
        header,
        progress,
        queued_message,
    })
}

fn build_leading_display(
    input: &AssistantToolUseInput,
    is_queued: bool,
    is_resolved: bool,
) -> Option<AssistantToolLeadingDisplay> {
    if !input.should_show_dot {
        return None;
    }

    if is_queued {
        return Some(AssistantToolLeadingDisplay::QueuedDot {
            glyph: input.dot_glyph.clone(),
            dim: true,
            min_width: 2,
        });
    }

    Some(AssistantToolLeadingDisplay::Loader {
        should_animate: input.should_animate,
        is_unresolved: !is_resolved,
        is_error: input.errored_tool_use_ids.contains(&input.tool_use.id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> AssistantToolUseInput {
        AssistantToolUseInput {
            tool_use: AssistantToolUseInvocation {
                id: "u1".into(),
                name: "Read".into(),
            },
            tools_available: true,
            tool: Some(AssistantToolDefinition {
                name: "Read".into(),
                user_facing_name: "Read".into(),
                user_facing_name_background_color: None,
                is_transparent_wrapper: false,
                renders: AssistantToolRenderOutputs {
                    message: Some("file.txt".into()),
                    tag: Some("timeout".into()),
                    progress_message: Some("Reading file".into()),
                    queued_message: Some("Queued behind another read".into()),
                    hook_progress_message: Some("1 PreToolUse hook ran".into()),
                },
            }),
            add_margin: true,
            in_progress_tool_use_ids: HashSet::from(["u1".into()]),
            resolved_tool_use_ids: HashSet::new(),
            errored_tool_use_ids: HashSet::new(),
            should_animate: true,
            should_show_dot: true,
            background: Some("messageActionsBackground".into()),
            pending_worker_tool_use_id: None,
            dot_glyph: "\u{25cf}".into(),
        }
    }

    #[test]
    fn missing_tool_reason_log_messages() {
        let mut input = base_input();
        input.tool = None;
        let projection = project_assistant_tool_use(&input);
        assert_eq!(
            projection,
            AssistantToolUseProjection::Hidden(AssistantToolUseHiddenReason::MissingTool {
                log_message: "Tool Read not found".into(),
            })
        );

        input.tools_available = false;
        let projection = project_assistant_tool_use(&input);
        assert_eq!(
            projection,
            AssistantToolUseProjection::Hidden(AssistantToolUseHiddenReason::MissingTool {
                log_message: "Tools array is undefined for tool Read".into(),
            })
        );
    }

    #[test]
    fn transparent_wrappers_hide_queued_and_resolved_states() {
        let mut input = base_input();
        input.tool.as_mut().unwrap().is_transparent_wrapper = true;
        input.in_progress_tool_use_ids.clear();
        let projection = project_assistant_tool_use(&input);
        assert_eq!(
            projection,
            AssistantToolUseProjection::Hidden(
                AssistantToolUseHiddenReason::TransparentWrapperQueued
            )
        );

        input.in_progress_tool_use_ids.insert("u1".into());
        input.resolved_tool_use_ids.insert("u1".into());
        let projection = project_assistant_tool_use(&input);
        assert_eq!(
            projection,
            AssistantToolUseProjection::Hidden(
                AssistantToolUseHiddenReason::TransparentWrapperResolved
            )
        );
    }

    #[test]
    fn transparent_wrappers_show_progress_only_when_active() {
        let mut input = base_input();
        input.tool.as_mut().unwrap().is_transparent_wrapper = true;
        let projection = project_assistant_tool_use(&input);
        assert_eq!(
            projection,
            AssistantToolUseProjection::Transparent(AssistantToolProgressDisplay {
                background: Some("messageActionsBackground".into()),
                hook_progress_message: Some("1 PreToolUse hook ran".into()),
                progress_message: Some("Reading file".into()),
            })
        );
    }

    #[test]
    fn hides_empty_user_facing_names_and_null_rendered_messages() {
        let mut input = base_input();
        input.tool.as_mut().unwrap().user_facing_name.clear();
        assert_eq!(
            project_assistant_tool_use(&input),
            AssistantToolUseProjection::Hidden(AssistantToolUseHiddenReason::EmptyUserFacingName)
        );

        let mut input = base_input();
        input.tool.as_mut().unwrap().renders.message = None;
        assert_eq!(
            project_assistant_tool_use(&input),
            AssistantToolUseProjection::Hidden(
                AssistantToolUseHiddenReason::NullRenderedToolUseMessage
            )
        );
    }

    #[test]
    fn standard_row_builds_loader_header_message_and_tag() {
        let projection = project_assistant_tool_use(&base_input());
        let AssistantToolUseProjection::Row(display) = projection else {
            panic!("expected row");
        };

        assert_eq!(display.margin_top, 1);
        assert_eq!(
            display.background.as_deref(),
            Some("messageActionsBackground")
        );
        assert_eq!(display.header.min_width, 6);
        assert_eq!(
            display.header.leading,
            Some(AssistantToolLeadingDisplay::Loader {
                should_animate: true,
                is_unresolved: true,
                is_error: false,
            })
        );
        assert_eq!(display.header.rendered_message.as_deref(), Some("file.txt"));
        assert_eq!(display.header.tag.as_deref(), Some("timeout"));
    }

    #[test]
    fn unresolved_row_prefers_waiting_for_permission_over_progress() {
        let mut input = base_input();
        input.pending_worker_tool_use_id = Some("u1".into());
        let projection = project_assistant_tool_use(&input);
        let AssistantToolUseProjection::Row(display) = projection else {
            panic!("expected row");
        };

        assert_eq!(
            display.progress,
            Some(AssistantToolSecondaryDisplay::WaitingForPermission {
                height: 1,
                message: "Waiting for permission\u{2026}",
            })
        );
    }

    #[test]
    fn queued_rows_switch_to_dim_dot_and_expose_queued_message() {
        let mut input = base_input();
        input.in_progress_tool_use_ids.clear();
        let projection = project_assistant_tool_use(&input);
        let AssistantToolUseProjection::Row(display) = projection else {
            panic!("expected row");
        };

        assert_eq!(
            display.header.leading,
            Some(AssistantToolLeadingDisplay::QueuedDot {
                glyph: "\u{25cf}".into(),
                dim: true,
                min_width: 2,
            })
        );
        assert_eq!(display.progress, None);
        assert_eq!(
            display.queued_message.as_deref(),
            Some("Queued behind another read")
        );
    }

    #[test]
    fn resolved_rows_keep_loader_but_hide_progress_and_queue() {
        let mut input = base_input();
        input.resolved_tool_use_ids.insert("u1".into());
        let projection = project_assistant_tool_use(&input);
        let AssistantToolUseProjection::Row(display) = projection else {
            panic!("expected row");
        };

        assert_eq!(
            display.header.leading,
            Some(AssistantToolLeadingDisplay::Loader {
                should_animate: true,
                is_unresolved: false,
                is_error: false,
            })
        );
        assert_eq!(display.progress, None);
        assert_eq!(display.queued_message, None);
    }

    #[test]
    fn progress_block_hides_when_tool_callbacks_return_nothing() {
        let mut input = base_input();
        input.tool.as_mut().unwrap().renders.progress_message = None;
        input.tool.as_mut().unwrap().renders.hook_progress_message = None;
        let projection = project_assistant_tool_use(&input);
        let AssistantToolUseProjection::Row(display) = projection else {
            panic!("expected row");
        };
        assert_eq!(display.progress, None);
    }
}
