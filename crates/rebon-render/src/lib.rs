//! # rebon-render — the one ratatui-free render projection
//!
//! Everything Rebon shows a user is projected here first and painted
//! somewhere else. A terminal front-end paints the `String` / `Vec<String>` /
//! `RenderedLine` values this crate produces into its buffer; a desktop client
//! maps the same values onto styled elements; a web client ships them to a
//! browser. One source of truth, so a tool row cannot say one thing on one
//! surface and another on the next.
//!
//! **Nothing in this crate knows what a terminal cell is.** There is no
//! `ratatui` and no `crossterm` dependency and there must not be one: the
//! painters depend on this crate, never the other way round. The `compatibility`
//! test at the end of this file holds that line.
//!
//! ## What is here
//!
//! * **Message projection** — one display state per kind of row:
//!   [`advisor`], [`assistant_text`], [`assistant_tool_use`], [`attachment`],
//!   [`collapse_grouping`], [`collapsed_read_search`], [`compact_summary`],
//!   [`diagnostics`], [`mailbox`], [`message_actions`], [`plan_approval`],
//!   [`rate_limit`], [`session_preview`], [`simple_messages`],
//!   [`summary_envelope`], [`system_api_error`], [`system_text`],
//!   [`teammate_messages`], [`thinking`], [`tool_grouping`], [`tool_lookup`],
//!   [`tool_results`], [`user_prompt`], [`user_text`], [`wrappers`],
//!   [`common`]. Runtime and UI seams (upgrade prompts, model-name
//!   rendering, keychain detection) live above this crate and arrive as
//!   plain inputs.
//! * **Row and transcript projection** — [`transcript_row`], [`timeline`],
//!   [`types`], [`project`], [`row_projection`], [`row_logic`],
//!   [`render_plan`], [`slice`], [`memo`], [`messages_memo`], [`metadata`],
//!   [`brief`], [`interaction`], [`list_logic`], [`fallback`],
//!   [`file_edit`]. The persisted row shape, the message row union and its
//!   dispatch, brief-mode filters, the static-row gate, the crate-anchor
//!   math, and the edit-summary / diff and rejected file-edit branches.
//!
//! ## The row stream has two ends and they are two types
//!
//! [`transcript_row::Message`] is the **input** row: a serde mirror of one
//! persisted transcript line, tool use and tool result still nested as
//! content blocks inside their parent message, nothing grouped and nothing
//! collapsed. [`types::MessageRow`] is the **output** row: the same four
//! kinds, dispatched for drawing. Folding is a separate answer —
//! [`fold_rows`] returns a plan over row *indices*, so a folded group never
//! becomes a row of its own.
//!
//! Five type names appear on both sides (`UserMessage`, `AssistantMessage`,
//! `SystemMessage`, `UserContentBlock`, `AssistantContentBlock`), so
//! [`transcript_row`] is not re-exported at the crate root. Say which end
//! you mean.
//! * **Tool-call rendering** — [`agent`], [`content`], [`diff`], [`hidden`],
//!   [`json_compact`], [`kind`], [`plan_ledger`], [`shell_output`],
//!   [`streaming`], [`summary`], [`text`], [`tool_output`],
//!   [`workflow_body`], [`auto_mode_note`], [`code_mode`]. Edit/Write diff
//!   computation, tool-call body/title/kind formatting, the raw-output →
//!   `ToolCallContent` / `ToolCallLocation` projection a live turn and a
//!   replay both use, and the `StreamingToolUse` data model.
//! * **Markdown layout** — [`detect`], [`fence`], [`math`], [`pad_aligned`],
//!   [`streaming_boundary`], [`strip_prompt_tags`], [`table_layout`],
//!   [`table_render`], [`token_cache`].
//! * **Structured diffs** — [`diff_fallback`], [`gutter`], [`patch`],
//!   [`render_cache`].
//!
//! ## Markdown: what is deliberately out of scope
//!
//! * **The lexer call itself.** Picking a Markdown parser and walking its
//!   token shapes are substantial concerns that would dwarf the rest of the
//!   markdown modules. Here the lexer is an injected
//!   `Fn(&str) -> Vec<LexedToken>` callback, so [`streaming_boundary`]'s
//!   stable-prefix algorithm can be pinned without picking a parser.
//!   `pulldown-cmark` lives with the caller that formats tokens.
//! * **Inline-token → ANSI formatting** and **ANSI-aware text wrapping.**
//!   Style carry-over across line breaks, hyperlink escapes and the
//!   word-wrap / hard / trim interactions are their own concern;
//!   [`table_render`] takes already-wrapped `Vec<String>` cells.
//! * **Syntax-highlighter integration** and **rendering widgets.** Render
//!   targets are the consumer's responsibility.
//! * **Token-stream table flushing.** The loop that walks lexed tokens and
//!   emits a rendered table when it hits one depends on the token formatter,
//!   so it lives with the formatter.
//!
//! These are modeled as seams (injected callbacks and pre-computed inputs)
//! rather than stubs, because misaligned stubs have negative value: they hint
//! at the wrong API and force consumers to either preserve the mistake or do a
//! disruptive rename.
//!
//! ## Structured diffs: the injected seams and why
//!
//! [`diff_fallback`] takes the word-diff operation as a
//! `Fn(&str, &str) -> Vec<DiffPart>` callback and the wrapper as a
//! `Fn(&str, usize) -> Vec<String>`, so the pipeline is testable without an
//! ANSI-aware wrapper or a word-diff dependency. Colors are carried as intent
//! (`LineColor::Added`, `WordColor::RemovedWord`, …) and resolved to RGB by
//! the consumer.
//!
//! [`patch::PatchHunk`] is a plain struct exposing only the five fields these
//! modules read (`old_start`, `old_lines`, `new_start`, `new_lines`, `lines`)
//! rather than a unified-diff parser's full type, so callers convert at their
//! own seam and this crate needs no parser.
//!
//! ## Two hand-rolled caches, two different reasons
//!
//! [`token_cache`] is a real LRU with explicit MRU promotion, bounded at
//! `TOKEN_CACHE_MAX` (500). Its keys come from `djb2` rather than a hash
//! crate: the cache lives entirely in-process, so keys never cross a runtime
//! boundary and any deterministic hash is observationally indistinguishable
//! from a cryptographic one.
//!
//! [`render_cache`] is **not** an LRU — it is a hard cap-and-clear at four
//! entries. It holds two width × two dim variants in steady state and
//! bulk-evicts the moment a fifth distinct key arrives, which only happens
//! during an active terminal resize where every old entry is already stale.
//! An `lru` crate would force a per-key eviction policy that does not match.
//!
//! ## `missing_docs` is per module, not per crate
//!
//! Several modules below carry `#[deny(missing_docs)]` on their declaration and
//! must keep it; the tool-render, markdown and structured-diff modules do not
//! have it. Hoisting the attribute to the crate level would demand roughly
//! sixty new doc comments on items whose authors chose not to write them, so
//! the lint stays on exactly the modules that already carried it. A new module
//! should match the neighbours it looks like.

#[deny(missing_docs)]
pub mod activity_runs;
#[deny(missing_docs)]
pub mod advisor;
pub mod agent;
#[deny(missing_docs)]
pub mod assistant_text;
#[deny(missing_docs)]
pub mod assistant_tool_use;
#[deny(missing_docs)]
pub mod ask_user_answers;
#[deny(missing_docs)]
pub mod attachment;
pub mod auto_mode_note;
#[deny(missing_docs)]
pub mod brief;
pub mod code_mode;
#[deny(missing_docs)]
pub mod collapse_grouping;
#[deny(missing_docs)]
pub mod collapsed_read_search;
#[deny(missing_docs)]
pub mod common;
#[deny(missing_docs)]
pub mod compact_summary;
pub mod content;
pub mod detect;
#[deny(missing_docs)]
pub mod diagnostics;
pub mod diff;
pub mod diff_fallback;
#[deny(missing_docs)]
pub mod fallback;
pub mod fence;
#[deny(missing_docs)]
pub mod file_edit;
pub mod fold_rows;
pub mod gutter;
pub mod hidden;
#[deny(missing_docs)]
pub mod interaction;
pub mod json_compact;
pub mod kind;
#[deny(missing_docs)]
pub mod list_logic;
#[deny(missing_docs)]
pub mod mailbox;
pub mod math;
#[deny(missing_docs)]
pub mod memo;
#[deny(missing_docs)]
pub mod message_actions;
#[deny(missing_docs)]
pub mod messages_memo;
#[deny(missing_docs)]
pub mod metadata;
pub mod pad_aligned;
pub mod patch;
#[deny(missing_docs)]
pub mod plan_approval;
pub mod plan_ledger;
#[deny(missing_docs)]
pub mod project;
#[deny(missing_docs)]
pub mod rate_limit;
pub mod render_cache;
#[deny(missing_docs)]
pub mod render_plan;
#[deny(missing_docs)]
pub mod row_logic;
#[deny(missing_docs)]
pub mod row_projection;
#[deny(missing_docs)]
pub mod session_preview;
pub mod shell_output;
#[deny(missing_docs)]
pub mod simple_messages;
#[deny(missing_docs)]
pub mod slice;
pub mod streaming;
pub mod streaming_boundary;
pub mod strip_prompt_tags;
pub mod summary;
#[deny(missing_docs)]
pub mod summary_envelope;
#[deny(missing_docs)]
pub mod system_api_error;
#[deny(missing_docs)]
pub mod system_text;
pub mod table_layout;
pub mod table_render;
#[deny(missing_docs)]
pub mod teammate_messages;
pub mod text;
#[deny(missing_docs)]
pub mod thinking;
#[deny(missing_docs)]
pub mod timeline;
pub mod token_cache;
#[deny(missing_docs)]
pub mod tool_grouping;
#[deny(missing_docs)]
pub mod tool_lookup;
pub mod tool_output;
#[deny(missing_docs)]
pub mod tool_results;
pub mod transcript_replay;
// Deliberately not re-exported at the crate root: five of its type names
// (`UserMessage`, `AssistantMessage`, `SystemMessage`, `UserContentBlock`,
// `AssistantContentBlock`) also name the post-folding shapes in `types`.
// The collision is the point -- input rows and output rows are different
// types, and a caller has to say which it means.
pub mod transcript_row;
#[deny(missing_docs)]
pub mod types;
#[deny(missing_docs)]
pub mod user_prompt;
#[deny(missing_docs)]
pub mod user_text;
#[deny(missing_docs)]
pub mod user_text_kind;
pub mod workflow_body;
#[deny(missing_docs)]
pub mod wrappers;

pub use advisor::{project_advisor_message, AdvisorBlock, AdvisorProjection, ToolUseLoaderDisplay};
pub use agent::{
    agent_background_result_line, agent_description, agent_full_detail_lines,
    format_agent_token_count, is_background_agent_result,
};
pub use assistant_text::{
    is_empty_assistant_message_text, is_rate_limit_error_message, project_assistant_text_message,
    AssistantDotDisplay, AssistantMarkdownDisplay, AssistantResponseBlock,
    AssistantResponseDisplay, AssistantTextMessageInput, AssistantTextMessageProjection,
};
pub use assistant_tool_use::{
    project_assistant_tool_use, AssistantToolDefinition, AssistantToolHeaderDisplay,
    AssistantToolLeadingDisplay, AssistantToolProgressDisplay, AssistantToolRenderOutputs,
    AssistantToolSecondaryDisplay, AssistantToolUseHiddenReason, AssistantToolUseInput,
    AssistantToolUseInvocation, AssistantToolUseProjection, AssistantToolUseRowDisplay,
};
pub use attachment::{
    project_attachment_message, AttachmentFileDisplay, AttachmentFileKind, AttachmentInput,
    AttachmentLineDisplay, AttachmentMailboxMessage, AttachmentMessageInput, AttachmentProjection,
    AttachmentRelevantMemoriesDisplay, AttachmentRelevantMemoryEntry, AttachmentRelevantMemoryRow,
    AttachmentSkillDiscoveryEntry, AttachmentTaskStatusDisplay, AttachmentTeammateMailboxDisplay,
    AttachmentTeammateMailboxItemDisplay, AttachmentTone, QueuedCommandDisplay,
};
pub use auto_mode_note::{
    auto_mode_allowed_note, parse_auto_mode_allowed_sidecar, AUTO_MODE_ALLOWED_BY_CACHE_NOTE,
    AUTO_MODE_ALLOWED_BY_CLASSIFIER_NOTE, AUTO_MODE_ALLOWED_BY_USER_NOTE, AUTO_MODE_ALLOWED_NOTE,
};
pub use brief::{drop_text_in_brief_turns, filter_for_brief_tool};
pub use collapse_grouping::{
    classify_tool_use, command_as_hint, is_collapsible_tool_result, Aggregator, ClassifyOptions,
    FinalizeParams, MemoryPathPolicy, ResultStatus, RowDecision, ToolClass, MAX_HINT_CHARS,
};
pub use collapsed_read_search::{
    file_name_from_path, format_duration_compact, format_seconds_one_decimal,
    project_collapsed_read_search, CollapsedBranchSummary, CollapsedCommit, CollapsedCountsState,
    CollapsedHookInfo, CollapsedMemoryEntry, CollapsedPr, CollapsedPrAction,
    CollapsedProgressUpdate, CollapsedProgressUpdateKind, CollapsedReadSearchDisplay,
    CollapsedReadSearchInput, CollapsedReadSearchOutput, CollapsedReadSearchProjection,
    CollapsedToolUseVerboseDisplay, CollapsedToolUseVerboseInput, CollapsedVerboseDisplay,
    MIN_HINT_DISPLAY_MS,
};
pub use common::{
    extract_attr, extract_tag, humanize_bool_count, parse_channel_message, parse_resource_updates,
    simple_file_url, truncate_to_width, ExtractedAttribute, ResourceUpdate, ResourceUpdateKind,
    UserChannelSummary,
};
pub use compact_summary::{
    project_compact_summary as project_compact_summary_card, CompactFileEntry,
    CompactSummaryDisplay, CompactSummaryInput, CompactSummaryMetadata,
};
pub use content::render_tool_call_content;
pub use detect::has_markdown_syntax;
pub use diagnostics::{
    project_diagnostics_display, severity_symbol, DiagnosticEntry, DiagnosticFileDisplay,
    DiagnosticSeverity, DiagnosticsProjection, VerboseDiagnosticFile,
};
pub use diff::{
    build_context_diff, diff_line_counts, diff_summary, diff_summary_from_counts,
    generate_diff_lines, interleaved_diff_lines, lcs_diff_ops, DiffOp, EDIT_CONTEXT_LINES,
    LCS_DIFF_BUDGET,
};
pub use diff_fallback::{
    calculate_word_diff, decide_word_diff_path, format_diff_lines, number_diff_lines,
    process_adjacent_lines, transform_lines_to_objects, DiffPart, FormatOptions, LineColor,
    LineObject, LineSegment, LineType, RenderedLine, WordColor, WordDiffDecision, CHANGE_THRESHOLD,
};
pub use fallback::{
    project_fallback_tool_use_error, FallbackToolResultContent, FallbackToolUseErrorProjection,
    FALLBACK_TOOL_USE_REJECTED_HEIGHT, MAX_RENDERED_LINES,
};
pub use fence::{open_fence_at_end, OpenFence};
pub use file_edit::{
    fold_long_diff_runs, project_file_edit_rejected, project_file_edit_updated,
    project_notebook_edit_rejected, FileEditOperation, FileEditRejectedInput,
    FileEditRejectedProjection, FileEditUpdatedInput, FileEditUpdatedProjection, NotebookCellType,
    NotebookEditMode, NotebookEditRejectedInput, NotebookEditRejectedProjection,
    StructuredPatchHunk, MAX_LINES_TO_RENDER,
};
pub use gutter::compute_gutter_width;
pub use hidden::{is_transcript_hidden_tool, TRANSCRIPT_HIDDEN_TOOLS};
pub use interaction::{
    extract_search_text, is_item_clickable, ItemClickabilityInput, SearchTextExtractionInput,
};
pub use json_compact::{compact_json_value, is_default_json_value};
pub use kind::{tool_kind_from_name, ToolOutputVerbosity};
pub use list_logic::{
    expand_key, find_divider_before_index, find_last_thinking_block_id,
    find_latest_bash_output_uuid, find_selected_index, is_streaming_thinking_visible,
    StreamingThinkingState,
};
pub use mailbox::{
    check_has_team_mem_ops, get_shutdown_message_summary, get_task_assignment_summary,
    project_shutdown_rejected, project_shutdown_request, project_task_assignment,
    render_team_mem_count_parts, team_mem_saved_part, ShutdownRejectedDisplay,
    ShutdownRejectedMessage, ShutdownRequestDisplay, ShutdownRequestMessage, TaskAssignmentDisplay,
    TaskAssignmentMessage, TeamMemPart,
};
pub use math::{scan_math_fragments, MathDelimiter, MathDisplayMode, MathFragment};
pub use memo::{are_message_memo_inputs_equal, has_thinking_content, MessageMemoInput};
pub use message_actions::{
    available_message_actions, copy_text_of as copy_message_text, dispatch_message_action_key,
    is_navigable_message, project_message_actions_bar, strip_injected_wrappers,
    MessageActionAssistantBlock, MessageActionAssistantMessage, MessageActionAttachment,
    MessageActionBarDisplay, MessageActionButtonDisplay, MessageActionCollapsedReadSearch,
    MessageActionDispatch, MessageActionGroupedToolUse, MessageActionMessage, MessageActionState,
    MessageActionSystemMessage, MessageActionToolInput, MessageActionUserMessage,
};
pub use messages_memo::{
    are_messages_memo_inputs_equal, MessagesMemoInput, MessagesMemoUnseenDivider,
};
pub use metadata::{
    has_transcript_metadata, project_message_model, project_message_timestamp,
    should_show_message_timestamp, MetadataLabelProjection,
};
pub use pad_aligned::{pad_aligned, Alignment};
pub use patch::PatchHunk;
pub use plan_approval::{
    format_teammate_message_content, get_idle_notification_summary, get_plan_approval_summary,
    project_plan_approval_request, project_plan_approval_response, IdleNotificationMessage,
    PlanApprovalRenderable, PlanApprovalRequestDisplay, PlanApprovalRequestMessage,
    PlanApprovalResponseDisplay, PlanApprovalResponseMessage,
};
pub use plan_ledger::{
    plan_ledger_error_lines, plan_ledger_requirement_lines, plan_ledger_result_from_text,
    plan_ledger_status_line, plan_ledger_summary, PLAN_LEDGER_TOOL_NAME,
};
pub use project::{
    derive_user_image_indices, project_assistant_block, project_message, project_user_block,
    AssistantBlockProjection, ImageKey, MessageProjection, SystemProjection, UserBlockProjection,
};
pub use rate_limit::{
    get_upsell_message, project_rate_limit_message, RateLimitMessageInput,
    RateLimitMessageProjection, UpsellParams, DEFAULT_CLAUDE_MAX_20X_TIER,
};
pub use render_cache::{
    build_cache_key, CachedRender, HunkRenderCache, HunkRenderEntry, RENDER_CACHE_PER_HUNK_CAP,
};
pub use render_plan::{project_render_message_row, RenderMessageRowInput, RenderMessageRowPlan};
pub use row_logic::{
    all_tools_resolved, are_message_row_memo_inputs_equal, has_content_after_index,
    has_timeline_thinking_content, is_message_streaming, should_render_statically,
    MessageRowMemoInput,
};
pub use row_projection::{project_message_row, MessageRowProjection, MessageRowProjectionInput};
pub use session_preview::{
    project_session_preview, SessionPreviewDisplay, SessionPreviewInput,
    SessionPreviewLoadingDisplay, SessionPreviewLog, SessionPreviewMessagesDisplay,
    SessionPreviewProjection,
};
pub use shell_output::{
    shell_management_body_lines, shell_management_body_lines_from_value,
    shell_management_header_summary, shell_management_header_summary_from_value,
};
pub use simple_messages::{
    build_grouped_tool_use_data as build_grouped_tool_use_render_data, get_status_color,
    parse_bash_output_tags, project_assistant_redacted_thinking, project_hook_progress,
    project_user_agent_notification, project_user_bash_input, project_user_channel_message,
    project_user_command_message, project_user_image_message, project_user_local_command_output,
    project_user_memory_input, project_user_plan_message, project_user_resource_update,
    user_memory_saving_texts, AgentNotificationDisplay, BashInputDisplay, BashOutputDisplay,
    CloudLaunchDisplay, HookProgressDisplay, LocalCommandOutputBlock, LocalCommandOutputDisplay,
    ResourceUpdateDisplay, StatusColor, UserChannelDisplay, UserCommandDisplay, UserImageDisplay,
    UserMemoryInputDisplay, UserPlanDisplay,
};
pub use slice::{
    compute_slice_start, HasUuid, SliceAnchor, MAX_MESSAGES_WITHOUT_VIRTUALIZATION,
    MESSAGE_CAP_STEP,
};
pub use streaming::{
    is_workflow_tool_use, StreamingContentBlock, StreamingOverlay, StreamingThinking,
    StreamingToolUse, WORKFLOW_INTERRUPTED_MESSAGE,
};
pub use streaming_boundary::{LexedToken, StreamingBoundary, TokenKind};
pub use strip_prompt_tags::strip_prompt_xml_tags;
pub use summary::{
    compact_json_map, deferred_tool_verbose_summary, format_sleep_duration_ms,
    streaming_tool_display_name, streaming_tool_summary, truncate_param_value, SUMMARY_MAX_CHARS,
};
pub use system_api_error::{
    advance_system_api_error_countdown, normalize_system_api_error_text, project_system_api_error,
    SystemApiErrorInput, SystemApiErrorProjection, MAX_API_ERROR_CHARS,
};
pub use system_text::{
    project_system_text_message, MemoryFileRowStyleHint, StopHookInfoDisplay,
    StopHookSummaryDisplay, SystemBridgeStatusDisplay, SystemGenericTextDisplay,
    SystemMemorySavedDisplay, SystemMemorySavedEntry, SystemMemorySavedInput,
    SystemStopHookSummaryInput, SystemTextMessageInput, SystemTextProjection,
    SystemTextProjectionInput, SystemTurnBudgetDisplay, SystemTurnDurationDisplay,
    SystemTurnDurationInput, SystemVisualMarker,
};
pub use table_layout::{compute_column_widths, ColumnLayout};
pub use table_render::{
    render_border_line, render_horizontal_table, render_row_lines, render_vertical_format,
    BorderKind, RenderedTable, TableInput,
};
pub use teammate_messages::{
    parse_teammate_messages, project_teammate_message_content, project_user_teammate_messages,
    teammate_display_name, ParsedTeammateMessage, TeammateMessageContentDisplay,
    TeammateRenderable,
};
pub use text::expand_tabs_for_tui;
pub use thinking::{
    find_thinking_trigger_positions, get_rainbow_color, project_assistant_thinking_message,
    project_highlighted_thinking_text, AssistantThinkingCollapsedDisplay,
    AssistantThinkingExpandedDisplay, AssistantThinkingMessageInput,
    AssistantThinkingMessageProjection, BriefThinkingDisplay, HighlightedThinkingTextInput,
    HighlightedThinkingTextProjection, InlinePointerDisplay, InlineThinkingDisplay,
    ThinkingTextSegment, ThinkingThemeColor, ThinkingTriggerPosition, BRIEF_LABEL, POINTER_GLYPH,
    THINKING_LABEL, THINKING_LABEL_WITH_ELLIPSIS,
};
pub use timeline::{
    AssistantTimelineMessage, AttachmentTimelineMessage, CollapsedReadSearchTimelineMessage,
    GroupedToolUseTimelineMessage, ServerToolUseBlock, SystemTimelineMessage, TextBlock,
    TimelineContentBlock, TimelineLookups, TimelineMessage, TimelineScreen, TimelineSystemSubtype,
    ToolResultBlock, ToolUseBlock, UserTimelineMessage,
};
pub use token_cache::{djb2_key, TokenCache, TOKEN_CACHE_MAX};
pub use tool_grouping::{
    build_grouped_tool_use_data, is_null_rendering_attachment, AttachmentLike, GroupedToolUseData,
    GroupedToolUseInput, GroupedToolUseRenderRequest, NullRenderingAttachmentType,
    NULL_RENDERING_TYPES,
};
pub use tool_lookup::{
    get_tool_from_messages, LookupTool, LookupToolUse, ToolFromMessages, ToolMessageLookups,
};
pub use tool_output::{
    extract_locations, tool_result_update_content, trim_raw_output_for_transcript,
};
pub use tool_results::{
    project_user_tool_error, project_user_tool_reject, project_user_tool_result,
    UserToolErrorProjection, UserToolRejectProjection, UserToolResultInput,
    UserToolResultProjection, UserToolSuccessProjection, CANCEL_MESSAGE,
    INTERRUPT_MESSAGE_FOR_TOOL_USE, PLAN_REJECTION_PREFIX, REJECT_MESSAGE,
    REJECT_MESSAGE_WITH_REASON_PREFIX,
};
pub use types::{
    AssistantContentBlock, AssistantMessage, AttachmentMessage, MessageRow, RenderMessageInput,
    SystemMessage, SystemSubtype, UserContentBlock, UserMessage,
};
pub use user_prompt::{
    project_user_prompt_message, should_use_brief_layout, truncate_user_prompt_text,
    UserPromptBackground, UserPromptMessageDisplay, UserPromptMessageInput, MAX_DISPLAY_CHARS,
    TRUNCATE_HEAD_CHARS, TRUNCATE_TAIL_CHARS,
};
pub use user_text::{
    format_user_prompt_hidden_separator, project_user_prompt_display_lines, project_user_text,
    UserPromptDisplayLine, UserTextInput, UserTextProjection, NO_CONTENT_MESSAGE,
    USER_PROMPT_FOLD_DEFAULT_WIDTH, USER_PROMPT_FOLD_HEAD_LINES, USER_PROMPT_FOLD_TAIL_LINES,
    USER_PROMPT_FOLD_THRESHOLD_LINES,
};
pub use workflow_body::{workflow_body_lines, workflow_tool_summary};
pub use wrappers::{
    ctrl_o_to_expand_text, interrupted_by_user_texts, project_compact_boundary_message,
    project_ctrl_o_to_expand, project_message_response, CompactBoundaryDisplay,
    CtrlOToExpandDisplay, InterruptedByUserDisplay, MessageResponseContextValue,
    MessageResponseDisplay,
};

#[cfg(test)]
mod compatibility {
    /// Canary — the `rebon-*` dependencies this crate is allowed to have, and
    /// why each one is on the list:
    ///
    /// * `rebon-design-system` for theme colors. Zero runtime dependencies.
    /// * `rebon-width`, the workspace's one display-width policy, a leaf over
    ///   unicode-width.
    /// * `rebon-tools-core` for the builtin tool facts table. It is a
    ///   compile-time constant, not a runtime registry — the rule this canary
    ///   guards is "no registries, no IO", and a `const` table of tool names
    ///   breaks neither. A private copy of the tool names here is what the
    ///   table replaced, and that copy had already drifted.
    /// * `rebon-types` for the shared ACP content block / tool kind values
    ///   the projection reads.
    ///
    /// `ratatui` and `crossterm` are the two names that must never appear:
    /// the whole point of this crate is that every front-end can consume it.
    const ALLOWED: &[&str] = &[
        "rebon-design-system",
        "rebon-width",
        "rebon-tools-core",
        "rebon-types",
    ];

    #[test]
    fn only_approved_rebon_deps_and_no_terminal_framework() {
        let cargo = include_str!("../Cargo.toml");
        for line in cargo.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            assert!(
                !trimmed.starts_with("ratatui") && !trimmed.starts_with("crossterm"),
                "rebon-render is the ratatui-free half; found: {line}"
            );
            if trimmed.starts_with("rebon-") {
                assert!(
                    ALLOWED.iter().any(|allowed| trimmed.starts_with(allowed)),
                    "rebon-render allows only {ALLOWED:?}; found: {line}"
                );
            }
        }
    }
}
