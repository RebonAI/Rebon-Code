//! Row projections: the top-level dispatch from a persisted row and its
//! render inputs down to a renderable shape, plus the per-block dispatches
//! beneath it and the image-index derivation the user branch needs.

use crate::types::{
    AssistantContentBlock, AttachmentMessage, MessageRow, RenderMessageInput, SystemSubtype,
    UserContentBlock, UserMessage,
};

/// Identity of an image block: the paste id recorded for it, or failing that
/// its 1-based image position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageKey {
    /// The paste id recorded for this image.
    PasteId(String),
    /// 1-based image position.
    Position(usize),
}

/// Top-level dispatch: the shape this row projects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageProjection {
    /// Attachment branch.
    Attachment {
        /// Attachment payload.
        attachment: AttachmentMessage,
        /// Whether extra vertical margin surrounds the row.
        add_margin: bool,
        /// Verbose render mode.
        verbose: bool,
        /// Whether the transcript screen is active.
        is_transcript_mode: bool,
    },
    /// Assistant branch with projected blocks.
    Assistant {
        /// Optional explicit width.
        container_width: Option<u16>,
        /// Per-block projections.
        blocks: Vec<AssistantBlockProjection>,
    },
    /// Compact summary shortcut from the user branch.
    UserCompactSummary {
        /// True when the compact summary is rendered on the transcript screen.
        transcript_screen: bool,
    },
    /// Normal user-content branch.
    UserContent {
        /// Optional explicit width.
        container_width: Option<u16>,
        /// Per-block projections.
        blocks: Vec<UserBlockProjection>,
        /// True only for the latest bash-output user message.
        wrap_in_expand_shell_output: bool,
    },
    /// System branch.
    System(SystemProjection),
}

/// Per-block dispatch inside a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserBlockProjection {
    /// Text block.
    Text {
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
        /// Verbose render mode.
        verbose: bool,
        /// Plan content attached to the message, if any.
        plan_content: Option<String>,
        /// Timestamp of the message, if any.
        timestamp: Option<String>,
        /// Whether the transcript screen is active.
        is_transcript_mode: bool,
    },
    /// Image block.
    Image {
        /// Derived image identity.
        image_id: ImageKey,
        /// Extra margin: on when the caller asked for it and this message does
        /// not continue a previous one.
        add_margin: bool,
    },
    /// Tool-result block.
    ToolResult {
        /// True when the style is `condensed`.
        style_condensed: bool,
        /// Verbose render mode.
        verbose: bool,
        /// Render width: terminal columns minus 5.
        width: i32,
        /// Whether the transcript screen is active.
        is_transcript_mode: bool,
    },
}

/// Context bag for `project_user_block`: everything the user-block dispatch
/// needs besides the block itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserBlockProjectInput {
    /// Parent user message.
    pub message: UserMessage,
    /// Concrete block within `message.content`.
    pub block: UserContentBlock,
    /// Derived image identifier when the block is an image.
    pub image_id: Option<ImageKey>,
    /// Whether extra vertical margin surrounds the block.
    pub add_margin: bool,
    /// Verbose render mode.
    pub verbose: bool,
    /// True when the style is `condensed`.
    pub style_condensed: bool,
    /// Whether this message continues the previous one.
    pub is_user_continuation: bool,
    /// Whether the transcript screen is active.
    pub is_transcript_mode: bool,
    /// Terminal width, in columns.
    pub terminal_columns: u16,
}

/// Per-block dispatch inside an assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantBlockProjection {
    /// Tool-use block.
    ToolUse {
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
    },
    /// Assistant text block.
    Text {
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
        /// Verbose render mode.
        verbose: bool,
    },
    /// Redacted-thinking block; hidden unless transcript mode or verbose.
    RedactedThinking {
        /// Whether the block renders nothing.
        hidden: bool,
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
    },
    /// Thinking block.
    Thinking {
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
        /// Whether the transcript screen is active.
        is_transcript_mode: bool,
        /// Verbose render mode.
        verbose: bool,
        /// Whether the block is hidden on the transcript screen.
        hide_in_transcript: bool,
    },
    /// Connector text, rendered as assistant text.
    ConnectorText {
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
        /// Verbose render mode.
        verbose: bool,
    },
    /// Advisor block.
    Advisor {
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
        /// Verbose output: on in verbose mode or on the transcript screen.
        verbose: bool,
    },
    /// Block kind that renders nothing: the dispatch logs and returns here.
    Null,
}

/// System routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemProjection {
    /// Nothing is rendered: every microcompact boundary, and a compact
    /// boundary when fullscreen mode is enabled.
    Hidden,
    /// Compact boundary row.
    CompactBoundary,
    /// A local command row, rendered as user text.
    LocalCommandAsUserText {
        /// Whether extra vertical margin surrounds the row.
        add_margin: bool,
        /// Verbose render mode.
        verbose: bool,
        /// Local command text.
        text: String,
    },
    /// Generic system text row.
    SystemText {
        /// Whether extra vertical margin surrounds the row.
        add_margin: bool,
        /// Verbose render mode.
        verbose: bool,
        /// Raw subtype string.
        raw_subtype: Option<String>,
        /// System level string.
        level: Option<String>,
        /// System text payload.
        text: String,
    },
}

/// One entry per content block: `Some(image key)` for image blocks and `None`
/// for everything else. An image takes the paste id recorded at its running
/// image position when there is one, and otherwise its own 1-based image
/// position.
pub fn derive_user_image_indices(message: &UserMessage) -> Vec<Option<ImageKey>> {
    let mut out = Vec::with_capacity(message.content.len());
    let mut image_position = 0usize;
    for block in &message.content {
        match block {
            UserContentBlock::Image { .. } => {
                let idx = message
                    .image_paste_ids
                    .get(image_position)
                    .cloned()
                    .flatten()
                    .map(ImageKey::PasteId)
                    .unwrap_or_else(|| {
                        image_position += 1;
                        ImageKey::Position(image_position)
                    });
                if !matches!(idx, ImageKey::Position(_)) {
                    image_position += 1;
                }
                out.push(Some(idx));
            }
            _ => out.push(None),
        }
    }
    out
}

/// Projects a persisted row plus its render inputs into that row's
/// top-level projection.
pub fn project_message(input: &RenderMessageInput) -> MessageProjection {
    match &input.message {
        MessageRow::Attachment(attachment) => MessageProjection::Attachment {
            attachment: attachment.clone(),
            add_margin: input.add_margin,
            verbose: input.verbose,
            is_transcript_mode: input.is_transcript_mode,
        },
        MessageRow::Assistant(message) => MessageProjection::Assistant {
            container_width: input.container_width,
            blocks: message
                .content
                .iter()
                .enumerate()
                .map(|(index, block)| {
                    project_assistant_block(
                        block,
                        input.add_margin,
                        input.verbose,
                        input.is_transcript_mode,
                        input.last_thinking_block_id.as_deref(),
                        &format!("{}:{index}", message.uuid),
                    )
                })
                .collect(),
        },
        MessageRow::User(message) => {
            if message.is_compact_summary {
                return MessageProjection::UserCompactSummary {
                    transcript_screen: input.is_transcript_mode,
                };
            }
            let image_indices = derive_user_image_indices(message);
            let blocks = message
                .content
                .iter()
                .enumerate()
                .map(|(index, block)| {
                    project_user_block(&UserBlockProjectInput {
                        message: message.clone(),
                        block: block.clone(),
                        image_id: image_indices[index].clone(),
                        // The margin is the gap *above the row*, so only the
                        // first block carries it. Handing it to every block
                        // put a blank line inside the message — between the
                        // `❯` text and the `⎿ [Image #N]` chip that belongs
                        // directly under it.
                        add_margin: input.add_margin && index == 0,
                        verbose: input.verbose,
                        style_condensed: input.style_condensed,
                        is_user_continuation: input.is_user_continuation,
                        is_transcript_mode: input.is_transcript_mode,
                        terminal_columns: input.terminal_columns,
                    })
                })
                .collect();
            MessageProjection::UserContent {
                container_width: input.container_width,
                blocks,
                wrap_in_expand_shell_output: input.latest_bash_output_uuid.as_deref()
                    == Some(message.uuid.as_str()),
            }
        }
        MessageRow::System(message) => match message.subtype {
            SystemSubtype::CompactBoundary => {
                if input.fullscreen_env_enabled {
                    MessageProjection::System(SystemProjection::Hidden)
                } else {
                    MessageProjection::System(SystemProjection::CompactBoundary)
                }
            }
            SystemSubtype::MicrocompactBoundary => {
                MessageProjection::System(SystemProjection::Hidden)
            }
            SystemSubtype::LocalCommand => {
                MessageProjection::System(SystemProjection::LocalCommandAsUserText {
                    add_margin: input.add_margin,
                    verbose: input.verbose,
                    text: message.content.clone(),
                })
            }
            SystemSubtype::Other => MessageProjection::System(SystemProjection::SystemText {
                add_margin: input.add_margin,
                verbose: input.verbose,
                raw_subtype: message.raw_subtype.clone(),
                level: message.level.clone(),
                text: message.content.clone(),
            }),
        },
    }
}

/// Projects one user content block. Image blocks must carry a derived image
/// key; the dispatch panics if one is missing.
pub fn project_user_block(input: &UserBlockProjectInput) -> UserBlockProjection {
    match &input.block {
        UserContentBlock::Text { .. } => UserBlockProjection::Text {
            add_margin: input.add_margin,
            verbose: input.verbose,
            plan_content: input.message.plan_content.clone(),
            timestamp: input.message.timestamp.clone(),
            is_transcript_mode: input.is_transcript_mode,
        },
        UserContentBlock::Image { .. } => UserBlockProjection::Image {
            image_id: input
                .image_id
                .clone()
                .expect("image blocks always have a derived image index"),
            add_margin: input.add_margin && !input.is_user_continuation,
        },
        UserContentBlock::ToolResult { .. } => UserBlockProjection::ToolResult {
            style_condensed: input.style_condensed,
            verbose: input.verbose,
            width: input.terminal_columns as i32 - 5,
            is_transcript_mode: input.is_transcript_mode,
        },
    }
}

/// Projects one assistant content block, including the thinking-block
/// expansion key it is given.
pub fn project_assistant_block(
    block: &AssistantContentBlock,
    add_margin: bool,
    verbose: bool,
    is_transcript_mode: bool,
    last_thinking_block_id: Option<&str>,
    thinking_block_id: &str,
) -> AssistantBlockProjection {
    match block {
        AssistantContentBlock::ConnectorText { .. } => AssistantBlockProjection::ConnectorText {
            add_margin,
            verbose,
        },
        AssistantContentBlock::ToolUse { name, .. } => {
            if is_hidden_tool_use(name.as_deref().unwrap_or("")) {
                AssistantBlockProjection::Null
            } else {
                AssistantBlockProjection::ToolUse { add_margin }
            }
        }
        AssistantContentBlock::Text { .. } => AssistantBlockProjection::Text {
            add_margin,
            verbose,
        },
        AssistantContentBlock::RedactedThinking { .. } => {
            AssistantBlockProjection::RedactedThinking {
                hidden: !is_transcript_mode && !verbose,
                add_margin,
            }
        }
        AssistantContentBlock::Thinking { .. } => {
            let is_last_thinking = if last_thinking_block_id == Some("no-thinking") {
                false
            } else {
                last_thinking_block_id.is_none()
                    || last_thinking_block_id == Some(thinking_block_id)
            };
            AssistantBlockProjection::Thinking {
                add_margin,
                is_transcript_mode,
                verbose,
                hide_in_transcript: is_transcript_mode && !is_last_thinking,
            }
        }
        AssistantContentBlock::AdvisorBlock { .. } => AssistantBlockProjection::Advisor {
            add_margin,
            verbose: verbose || is_transcript_mode,
        },
        AssistantContentBlock::NonAdvisorServerBlock { .. }
        | AssistantContentBlock::Unknown { .. } => AssistantBlockProjection::Null,
    }
}

/// Tools that produce no inline tool-use message of their own: they render in
/// dedicated surfaces (task list, plan panel, etc.) rather than in the
/// transcript.
///
/// The list itself lives in [`crate::hidden::TRANSCRIPT_HIDDEN_TOOLS`]. A
/// second hand-written copy here had drifted to ten names against its
/// thirteen, so `TeamCreate`, `TeamDelete` and `SyntheticOutput` rendered a
/// card here and nowhere else.
fn is_hidden_tool_use(name: &str) -> bool {
    crate::hidden::is_transcript_hidden_tool(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantMessage, AttachmentMessage, MessageRow, RenderMessageInput, SystemMessage,
        UserMessage,
    };

    fn input(message: MessageRow) -> RenderMessageInput {
        RenderMessageInput {
            message,
            container_width: Some(80),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 120,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        }
    }

    fn user(content: Vec<UserContentBlock>) -> UserMessage {
        UserMessage {
            uuid: "u1".into(),
            is_compact_summary: false,
            content,
            image_paste_ids: Vec::new(),
            plan_content: Some("plan".into()),
            timestamp: Some("ts".into()),
        }
    }

    #[test]
    fn derive_user_image_indices_uses_paste_ids_then_position_fallback() {
        let message = UserMessage {
            uuid: "u1".into(),
            is_compact_summary: false,
            content: vec![
                UserContentBlock::Text { text: "x".into() },
                UserContentBlock::Image { source_hint: None },
                UserContentBlock::Image { source_hint: None },
            ],
            image_paste_ids: vec![Some("paste-a".into())],
            plan_content: None,
            timestamp: None,
        };
        let ids = derive_user_image_indices(&message);
        assert_eq!(ids[0], None);
        assert_eq!(ids[1], Some(ImageKey::PasteId("paste-a".into())));
        assert_eq!(ids[2], Some(ImageKey::Position(2)));
    }

    #[test]
    fn attachment_branch_projects_passthrough_flags() {
        let projection = project_message(&input(MessageRow::Attachment(AttachmentMessage {
            uuid: "a1".into(),
            attachment: None,
        })));
        assert!(matches!(projection, MessageProjection::Attachment { .. }));
    }

    #[test]
    fn assistant_branch_projects_all_blocks() {
        let projection = project_message(&input(MessageRow::Assistant(AssistantMessage {
            uuid: "a1".into(),
            content: vec![
                AssistantContentBlock::Text { text: "hi".into() },
                AssistantContentBlock::ToolUse {
                    id: Some("toolu-1".into()),
                    name: Some("Bash".into()),
                    input_summary: Some("ls -la".into()),
                    diff: None,
                    body_lines: Vec::new(),
                },
            ],
            advisor_model: None,
            is_stream_continuation: false,
        })));
        let MessageProjection::Assistant { blocks, .. } = projection else {
            panic!("expected assistant");
        };
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], AssistantBlockProjection::Text { .. }));
        assert!(matches!(
            blocks[1],
            AssistantBlockProjection::ToolUse { .. }
        ));
    }

    #[test]
    fn hidden_tool_use_projects_to_null() {
        for tool_name in [
            "TodoWrite",
            "TaskCreate",
            "TaskUpdate",
            "TaskList",
            "TaskGet",
            "TaskStop",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "ToolSearch",
        ] {
            let block = AssistantContentBlock::ToolUse {
                id: Some("toolu-1".into()),
                name: Some(tool_name.into()),
                input_summary: None,
                diff: None,
                body_lines: Vec::new(),
            };
            let projection = project_assistant_block(&block, false, false, false, None, "0");
            assert!(
                matches!(projection, AssistantBlockProjection::Null),
                "{tool_name} should project to Null"
            );
        }
    }

    #[test]
    fn visible_tool_use_projects_normally() {
        for tool_name in ["Bash", "Read", "Edit", "Write", "Grep", "Glob", "Agent"] {
            let block = AssistantContentBlock::ToolUse {
                id: Some("toolu-1".into()),
                name: Some(tool_name.into()),
                input_summary: None,
                diff: None,
                body_lines: Vec::new(),
            };
            let projection = project_assistant_block(&block, false, false, false, None, "0");
            assert!(
                matches!(projection, AssistantBlockProjection::ToolUse { .. }),
                "{tool_name} should project to ToolUse"
            );
        }
    }

    #[test]
    fn user_compact_summary_short_circuits() {
        let mut msg = user(vec![UserContentBlock::Text { text: "x".into() }]);
        msg.is_compact_summary = true;
        let projection = project_message(&input(MessageRow::User(msg)));
        assert_eq!(
            projection,
            MessageProjection::UserCompactSummary {
                transcript_screen: false
            }
        );
    }

    #[test]
    fn only_the_first_user_block_carries_the_row_margin() {
        let projection = project_message(&input(MessageRow::User(user(vec![
            UserContentBlock::Text { text: "x".into() },
            UserContentBlock::Image { source_hint: None },
        ]))));
        let MessageProjection::UserContent { blocks, .. } = projection else {
            panic!("expected user");
        };
        let UserBlockProjection::Text { add_margin, .. } = &blocks[0] else {
            panic!("expected text first, got {:?}", blocks[0]);
        };
        assert!(add_margin, "the row's gap belongs to its first block");
        let UserBlockProjection::Image { add_margin, .. } = &blocks[1] else {
            panic!("expected image second, got {:?}", blocks[1]);
        };
        assert!(
            !add_margin,
            "the image chip sits directly under the user text"
        );
    }

    #[test]
    fn latest_bash_output_wraps_expand_provider() {
        let mut i = input(MessageRow::User(user(vec![UserContentBlock::Text {
            text: "x".into(),
        }])));
        i.latest_bash_output_uuid = Some("u1".into());
        let projection = project_message(&i);
        let MessageProjection::UserContent {
            wrap_in_expand_shell_output,
            ..
        } = projection
        else {
            panic!("expected user");
        };
        assert!(wrap_in_expand_shell_output);
    }

    #[test]
    fn local_command_system_routes_to_user_text_path() {
        let projection = project_message(&input(MessageRow::System(SystemMessage {
            uuid: "s1".into(),
            subtype: SystemSubtype::LocalCommand,
            raw_subtype: Some("local_command".into()),
            level: Some("info".into()),
            content: "ls".into(),
            stop_hook_summary: None,
        })));
        assert_eq!(
            projection,
            MessageProjection::System(SystemProjection::LocalCommandAsUserText {
                add_margin: true,
                verbose: false,
                text: "ls".into(),
            })
        );
    }

    #[test]
    fn compact_boundary_hidden_in_fullscreen() {
        let mut i = input(MessageRow::System(SystemMessage {
            uuid: "s1".into(),
            subtype: SystemSubtype::CompactBoundary,
            raw_subtype: Some("compact_boundary".into()),
            level: Some("info".into()),
            content: String::new(),
            stop_hook_summary: None,
        }));
        i.fullscreen_env_enabled = true;
        assert_eq!(
            project_message(&i),
            MessageProjection::System(SystemProjection::Hidden)
        );
    }

    #[test]
    fn microcompact_boundary_is_hidden() {
        let projection = project_message(&input(MessageRow::System(SystemMessage {
            uuid: "s1".into(),
            subtype: SystemSubtype::MicrocompactBoundary,
            raw_subtype: Some("microcompact_boundary".into()),
            level: Some("info".into()),
            content: String::new(),
            stop_hook_summary: None,
        })));
        assert_eq!(
            projection,
            MessageProjection::System(SystemProjection::Hidden)
        );
    }

    #[test]
    fn redacted_thinking_hidden_only_when_not_verbose_and_not_transcript() {
        assert_eq!(
            project_assistant_block(
                &AssistantContentBlock::RedactedThinking { data: None },
                true,
                false,
                false,
                None,
                "a1:0"
            ),
            AssistantBlockProjection::RedactedThinking {
                hidden: true,
                add_margin: true,
            }
        );
    }

    #[test]
    fn thinking_hides_non_last_block_in_transcript() {
        assert_eq!(
            project_assistant_block(
                &AssistantContentBlock::Thinking {
                    thinking: Some("thought".into()),
                },
                true,
                false,
                true,
                Some("a1:1"),
                "a1:0"
            ),
            AssistantBlockProjection::Thinking {
                add_margin: true,
                is_transcript_mode: true,
                verbose: false,
                hide_in_transcript: true,
            }
        );
    }

    #[test]
    fn non_advisor_server_blocks_render_null() {
        assert_eq!(
            project_assistant_block(
                &AssistantContentBlock::NonAdvisorServerBlock {
                    raw_type: "server_tool_use".into(),
                },
                true,
                false,
                false,
                None,
                "a1:0"
            ),
            AssistantBlockProjection::Null
        );
    }

    #[test]
    fn user_tool_result_width_is_columns_minus_five() {
        let projection = project_user_block(&UserBlockProjectInput {
            message: user(vec![UserContentBlock::ToolResult {
                tool_use_id: Some("toolu-1".into()),
                content: Some("ok".into()),
                is_error: false,
            }]),
            block: UserContentBlock::ToolResult {
                tool_use_id: Some("toolu-1".into()),
                content: Some("ok".into()),
                is_error: false,
            },
            image_id: None,
            add_margin: true,
            verbose: false,
            style_condensed: true,
            is_user_continuation: false,
            is_transcript_mode: false,
            terminal_columns: 100,
        });
        assert_eq!(
            projection,
            UserBlockProjection::ToolResult {
                style_condensed: true,
                verbose: false,
                width: 95,
                is_transcript_mode: false,
            }
        );
    }

    /// The projection and the shared list have to agree name for name.
    ///
    /// They did not: this file named ten tools and
    /// [`crate::hidden::TRANSCRIPT_HIDDEN_TOOLS`] names thirteen, so
    /// `TeamCreate`, `TeamDelete` and `SyntheticOutput` rendered a card here
    /// while every other surface hid them. A second copy of a judgement is
    /// how that happens, which is why this pins the two together rather than
    /// pinning a list of names.
    #[test]
    fn the_projection_hides_exactly_the_shared_transcript_list() {
        for name in crate::hidden::TRANSCRIPT_HIDDEN_TOOLS {
            assert!(
                is_hidden_tool_use(name),
                "{name} is hidden everywhere else but renders a card here"
            );
        }
        for name in ["Read", "Write", "Edit", "Bash", "Agent", "Grep"] {
            assert!(
                !is_hidden_tool_use(name),
                "{name} is an ordinary tool and has to keep its card"
            );
        }
    }
}
