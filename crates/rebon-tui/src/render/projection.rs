use super::*;

// ---------------------------------------------------------------------------
// Bridge: rebon-tui::Message → rebon-render::MessageRow
// ---------------------------------------------------------------------------

/// Convert a `rebon-tui` message into the `rebon-render` `MessageRow`
/// used by `RenderedMessageWidget`. Returns `None` for message types
/// that have no `rebon-render` equivalent (e.g. `Unknown`).
pub(super) fn to_message_row(
    msg: &Message,
    verbosity: ToolOutputVerbosity,
    content_width: u16,
) -> Option<RmMessageRow> {
    match msg {
        Message::User(u) => {
            // Meta messages (attachment injections like plan_mode
            // reminders, date_change, skill_listing). These are sent to
            // the API but never rendered as transcript rows.
            if u.is_meta == Some(true) {
                return None;
            }
            let content = u
                .message
                .content
                .iter()
                .map(|block| match block {
                    UserContentBlock::Text(t) => RmUserContentBlock::Text {
                        text: t.text.clone(),
                    },
                    UserContentBlock::Image(_) => RmUserContentBlock::Image { source_hint: None },
                    UserContentBlock::ToolResult(tr) => user_tool_result_content_block(tr),
                })
                .collect();
            // Thread paste ids through so the continuation chip
            // rendered under the user text reads the same number as
            // the `[Image #N]` chip in the text itself. The `Vec<u32>`
            // paste ids become one optional string per image slot,
            // aligned with the image indices.
            let image_paste_ids = u
                .image_paste_ids
                .as_ref()
                .map(|ids| ids.iter().map(|id| Some(id.to_string())).collect())
                .unwrap_or_default();
            Some(RmMessageRow::User(RmUserMessage {
                uuid: u.uuid.clone(),
                is_compact_summary: u.is_compact_summary.unwrap_or(false),
                content,
                image_paste_ids,
                plan_content: u.plan_content.clone(),
                timestamp: Some(u.timestamp.clone()),
            }))
        }
        Message::Assistant(a) => {
            let content = a
                .message
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContentBlock::Text(t) => Some(RmAssistantContentBlock::Text {
                        text: t.text.clone(),
                    }),
                    AssistantContentBlock::Thinking(t) => Some(RmAssistantContentBlock::Thinking {
                        thinking: Some(t.thinking.clone()),
                    }),
                    AssistantContentBlock::RedactedThinking(r) => {
                        Some(RmAssistantContentBlock::RedactedThinking {
                            data: r.data.clone(),
                        })
                    }
                    AssistantContentBlock::ToolUse(tu) => {
                        let display_name = if tu.name == "InvokeDeferredTool" {
                            tu.input
                                .get("tool_name")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| tu.name.clone())
                        } else {
                            tu.name.clone()
                        };
                        let is_web_search = display_name == "WebSearch";
                        let shell_management_summary = tu.raw_output.as_ref().and_then(|raw| {
                            rebon_render::shell_output::shell_management_header_summary_from_value(
                                &tu.name, raw,
                            )
                        });
                        let summary = if let Some(input) = plan_ledger_input(&tu.name, &tu.input) {
                            let fallback_result =
                                plan_ledger_result_from_content(tu.tool_call_content.as_deref());
                            Some(plan_ledger_summary(
                                input.get("operation").and_then(Value::as_str),
                                input.get("items"),
                                tu.raw_output
                                    .as_ref()
                                    .and_then(|output| output.get("requirements"))
                                    .or_else(|| {
                                        fallback_result
                                            .as_ref()
                                            .and_then(|output| output.get("requirements"))
                                    }),
                            ))
                        } else {
                            shell_management_summary.or_else(|| {
                                tu.input.as_object().filter(|o| !o.is_empty()).map(|map| {
                                    if tu.status.is_some() || tu.name == "Sleep" || is_web_search {
                                        compact_json_map_value_for_tool(Some(&tu.name), map)
                                    } else {
                                        compact_json_object(map)
                                    }
                                })
                            })
                        };
                        let diff = if is_web_search {
                            None
                        } else {
                            tu.tool_call_content.as_ref().and_then(
                                |content: &Vec<ToolCallContent>| {
                                    content.iter().find_map(|c| match c {
                                        ToolCallContent::Diff(d) => Some((
                                            d.path.clone(),
                                            d.old_text.clone(),
                                            d.new_text.clone(),
                                        )),
                                        _ => None,
                                    })
                                },
                            )
                        };
                        Some(RmAssistantContentBlock::ToolUse {
                            id: Some(tu.id.clone()),
                            name: Some(display_name),
                            input_summary: summary,
                            diff,
                            body_lines: collect_tool_block_body_lines_with_width(
                                tu,
                                verbosity,
                                content_width,
                            ),
                        })
                    }
                    AssistantContentBlock::GeneratedImage(gi) => {
                        let mut lines = vec![format!(
                            "Generated image ({})",
                            if gi.media_type.is_empty() {
                                "image/png"
                            } else {
                                gi.media_type.as_str()
                            }
                        )];
                        if let Some(path) = gi.saved_path.as_ref() {
                            lines.push(generated_image_saved_line(path, false));
                        }
                        if let Some(prompt) = gi.revised_prompt.as_ref() {
                            if !prompt.trim().is_empty() {
                                lines.push(format!("Prompt: {prompt}"));
                            }
                        }
                        Some(RmAssistantContentBlock::Text {
                            text: lines.join("\n"),
                        })
                    }
                    AssistantContentBlock::Other => None,
                })
                .collect();
            Some(RmMessageRow::Assistant(RmAssistantMessage {
                uuid: a.uuid.clone(),
                content,
                advisor_model: a.advisor_model.clone(),
                is_stream_continuation: a.is_stream_continuation == Some(true),
            }))
        }
        Message::System(s) => {
            let subtype = match s.subtype.as_str() {
                "compact_boundary" => RmSystemSubtype::CompactBoundary,
                "microcompact_boundary" => RmSystemSubtype::MicrocompactBoundary,
                "local_command" => RmSystemSubtype::LocalCommand,
                _ => RmSystemSubtype::Other,
            };
            Some(RmMessageRow::System(RmSystemMessage {
                uuid: s.uuid.clone(),
                subtype,
                raw_subtype: Some(s.subtype.clone()),
                level: system_message::system_level_label(s),
                content: s.content.clone().unwrap_or_default(),
                stop_hook_summary: None,
            }))
        }
        Message::Attachment(a) => Some(RmMessageRow::Attachment(RmAttachmentMessage {
            uuid: a.uuid.clone(),
            attachment: attachment_row_to_input(a).map(Box::new),
        })),
        Message::Unknown => None,
    }
}

pub(super) fn attachment_row_to_input(
    row: &crate::message::AttachmentRow,
) -> Option<AttachmentInput> {
    let ty = row.attachment.get("type").and_then(Value::as_str)?;
    match ty {
        "directory" => {
            let display_path = row
                .attachment
                .get("displayPath")
                .and_then(Value::as_str)
                .or_else(|| row.attachment.get("path").and_then(Value::as_str))?
                .to_string();
            Some(AttachmentInput::Directory { display_path })
        }
        _ => None,
    }
}

/// Build a `RenderMessageInput` from a `rebon-tui` message for the
/// `RenderedMessageWidget` rendering path.
pub(super) fn to_render_input_with_tool_state(
    msg: &Message,
    width: u16,
    add_margin: bool,
    verbosity: ToolOutputVerbosity,
    frame_time_ms: u64,
    mut in_progress_tool_use_ids: Vec<String>,
    mut errored_tool_use_ids: Vec<String>,
    last_thinking_block_id: Option<&str>,
    is_transcript_mode: bool,
    extras: TranscriptRenderExtras<'_>,
) -> Option<RenderMessageInput> {
    let visible_thinking_block_id = last_thinking_block_id.filter(|id| *id != "no-thinking");
    // The message widget reserves the outer gutter, then prefixes tool body rows
    // with a three-cell connector/indent inside the remaining content area.
    let message = to_message_row(msg, verbosity, width.saturating_sub(GUTTER + 3).max(1))?;
    // A stream-continuation row joins the previous row visually: the
    // leading margin would reintroduce the blank line the split removed.
    let add_margin =
        add_margin && !matches!(&message, RmMessageRow::Assistant(a) if a.is_stream_continuation);
    if let Message::Assistant(a) = msg {
        for block in &a.message.content {
            if let AssistantContentBlock::ToolUse(tu) = block {
                match tu.status {
                    Some(ToolCallStatus::Pending | ToolCallStatus::InProgress) => {
                        in_progress_tool_use_ids.push(tu.id.clone());
                    }
                    Some(ToolCallStatus::Failed) => errored_tool_use_ids.push(tu.id.clone()),
                    _ => {}
                }
            }
        }
    }
    let verbose = matches!(verbosity, ToolOutputVerbosity::Verbose) || extras.expand_thinking_rows;
    let style_condensed = matches!(verbosity, ToolOutputVerbosity::Compact);
    let has_thinking = has_thinking_block(msg);
    let compact_thinking_preview = has_thinking
        && !extras.expand_thinking_rows
        && matches!(verbosity, ToolOutputVerbosity::Compact);
    let show_thinking_expand_hint = compact_thinking_preview;
    Some(RenderMessageInput {
        message,
        container_width: Some(width),
        add_margin,
        verbose,
        style_condensed,
        is_transcript_mode,
        is_active_collapsed_group: false,
        is_user_continuation: false,
        last_thinking_block_id: visible_thinking_block_id.map(str::to_string),
        latest_bash_output_uuid: None,
        terminal_columns: width,
        fullscreen_env_enabled: false,
        compact_thinking_preview,
        show_thinking_expand_hint,
        frame_time_ms,
        in_progress_tool_use_ids,
        errored_tool_use_ids,
        show_tool_expand_hint: false,
    })
}
