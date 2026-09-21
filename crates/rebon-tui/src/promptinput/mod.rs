//! Prompt-input pure helpers — state machines and layout decisions.
//!
//! This module was a standalone crate before the merge. Its
//! consumers were this crate's `render_prompt_input`, the terminal
//! half, and the background runtime — and background only ever
//! wanted `PromptPasteContent`, which now lives in `rebon-types`. What
//! is left is the terminal prompt surface and nothing else.
//!
//! This module deliberately prioritizes the pure logic, state machines,
//! and layout decisions embedded in the prompt input surface. The large
//! UI render trees are out of scope; it covers the prompt-input
//! helpers plus queue/footer/banner logic
//! and prompt-surface parsing that can be represented as plain data.
//! Nothing here names ratatui, and nothing should start.
//!
//! ## Status widget composition (via [`crate::status`])
//!
//! * `footer_status` — composes the `crate::status` widgets that belong
//! to the prompt input surface, split by their actual render site:
//! * **Footer right**: the token warning (pos 8) — via
//! [`resolve_footer_notifications`].
//! * **Notification queue**: effort notification
//! pushed with key `"effort-level"` — via [`resolve_effort_notification`].
//! * **Footer left**: status line visibility
//! gate — via [`resolve_status_line_visibility`].
//!
//! Widgets rendered outside the prompt surface (`session_background_hint`,
//! `status_notices`, `bash_mode_progress`, `tool_use_loader`,
//! `press_enter_to_continue`) are NOT composed here — use `crate::status` directly for those.
//!
//! ## Deferred
//!
//! * prompt input footer widgets
//! * notifications, voice indicator, shimmered input, and most UI-heavy
//! modules that still depend on `Cursor`, message rendering, or live runtime
//! wiring

#![deny(missing_docs)]

pub mod fast_icon_hint;
pub mod footer_actions;
pub mod footer_motion;
pub mod footer_navigation;
pub mod footer_status;
pub mod footer_suggestions;
pub mod help_menu;
pub mod history_flow;
pub mod input_change;
pub mod input_event;
pub mod input_modes;
pub mod input_paste;
pub mod mode_cycle;
pub mod mode_indicator;
pub mod paste_flow;
pub mod prompt_input_frame;
pub mod prompt_input_placeholder;
pub mod prompt_input_runtime;
pub mod prompt_surface;
pub mod queue_display;
pub mod queued_commands;
pub mod spinner_hints;
pub mod submit_flow;
pub mod swarm_banner;
pub mod task_list_view;
pub mod text_input_view;
pub mod truncate_once;
pub mod utils;

pub use fast_icon_hint::{FastIconHintState, HINT_DISPLAY_DURATION_MS};
pub use footer_actions::{
    resolve_footer_close_action, resolve_footer_open_selected_action, FooterCloseAction,
    FooterCloseInput, FooterOpenSelectedAction, FooterOpenSelectedInput, VisibleFooterTask,
};
pub use footer_motion::{
    resolve_footer_clear_selection, resolve_footer_down, resolve_footer_next,
    resolve_footer_previous, resolve_footer_up, FooterMotionInput, FooterMotionPlan,
};
pub use footer_navigation::{
    build_footer_items, clamp_coordinator_task_index, enter_footer_from_history,
    min_coordinator_index, navigate_footer, resolve_visible_footer_selection, select_footer_item,
    EnterFooterFromHistoryResult, FooterItem, FooterNavigationResult, FooterSelectionUpdate,
    FooterVisibility,
};
pub use footer_status::{
    resolve_effort_notification, resolve_footer_notifications, resolve_status_line_visibility,
    EffortNotificationInput, EffortNotificationLayout, FooterNotificationItem,
    FooterNotificationsInput, FooterNotificationsLayout, StatusLineVisibilityInput,
    EFFORT_NOTIFICATION_KEY, EFFORT_NOTIFICATION_TIMEOUT_MS,
};
pub use footer_suggestions::{
    render_footer_suggestions, RenderedSuggestionList, RenderedSuggestionRow, StyledSegment,
    SuggestionItem, SuggestionSegmentRole, OVERLAY_MAX_ITEMS,
};
pub use help_menu::{build_help_menu_layout, HelpMenuColumn, HelpMenuInput, HelpMenuLayout};
pub use history_flow::{
    resolve_history_down_action, resolve_history_up_action, HistoryDownAction, HistoryUpAction,
};
pub use input_change::{
    plan_input_change, InputChangeInput, InputChangePlan, NormalizedInputChange,
};
pub use input_event::{
    plan_input_event, InputEventInput, InputEventPlan, InputEventPrimaryAction, OptionMetaHint,
};
pub use input_modes::{
    get_mode_from_input, get_value_from_input, is_input_mode_character,
    prepend_mode_character_to_input, HistoryMode, PromptInputMode,
};
pub use input_paste::{
    maybe_truncate_input, maybe_truncate_message_for_input, pasted_text_ref_num_lines,
    PastedContent, TruncateInputResult, TruncatedMessage, PREVIEW_LENGTH, TRUNCATION_THRESHOLD,
};
pub use mode_cycle::{
    plan_auto_mode_opt_in_accept, plan_auto_mode_opt_in_decline, plan_mode_cycle,
    AutoModeOptInAcceptInput, AutoModeOptInAcceptPlan, AutoModeOptInDeclineInput,
    AutoModeOptInDeclinePlan, ModeCycleInput, ModeCyclePlan, AUTO_MODE_OPT_IN_DELAY_MS,
};
pub use mode_indicator::{
    resolve_prompt_mode_indicator, PromptIndicatorColor, PromptIndicatorKind,
    PromptModeIndicatorInput, PromptModeIndicatorOutput,
};
pub use paste_flow::{
    apply_text_paste, plan_image_paste, plan_text_paste, prune_orphaned_image_ids,
    should_apply_paste_burst, ApplyTextPasteResult, ApplyTextPasteState, ImagePasteInput,
    ImagePastePlan, PasteBurstBuilder, PasteFragment, TextPasteInput, TextPastePlan,
};
pub use prompt_input_frame::{
    build_border_text, get_initial_paste_id, resolve_prompt_border_color, BorderText,
    BorderTextAlign, BorderTextPosition, PromptHistoryMessage,
};
pub use prompt_input_placeholder::{
    resolve_prompt_input_placeholder, PromptPlaceholderInput, MAX_TEAMMATE_NAME_LENGTH,
    NUM_TIMES_QUEUE_HINT_SHOWN,
};
pub use prompt_input_runtime::{
    derive_prompt_input_runtime_state, PromptInputRuntimeInput, PromptInputRuntimeState,
};
pub use prompt_surface::{
    build_prompt_highlights, extract_image_ref_positions, is_cursor_at_image_chip,
    parse_references, snap_cursor_out_of_image_chip, PromptHighlightInput, PromptReferenceMatch,
    PromptSurfaceHighlight, SurfaceMessageContentBlock, SurfacePromptHistoryMessage, TextRange,
    ThemeHighlightRange,
};
pub use queue_display::{
    build_queue_display, QueueDisplayInput, QueueDisplayItem, QueueDisplayLayout, QueueDisplayLine,
    QUEUE_STACK_GLYPH,
};
pub use queued_commands::{
    create_overflow_notification_message, is_idle_notification, process_queued_commands,
    QueuedCommand, QueuedCommandValue, MAX_VISIBLE_NOTIFICATIONS,
};
pub use spinner_hints::{
    compute_spinner_hint_actions, resolve_toggle_action, ExpandedView, SpinnerHintAction,
    SpinnerHintInput, SpinnerHintKind,
};
pub use submit_flow::{
    finalize_submit, parse_direct_member_message, prepare_submit,
    should_reset_prompt_suggestion_for_timing, should_show_prompt_suggestion, DirectMemberMessage,
    FinalizeSubmitInput, FinalizeSubmitResult, FinalizedSubmit, PreparedSubmit,
    PromptSuggestionState, SubmitBlockReason, SubmitPreparation, SubmitPreparationInput,
    SubmitRoute,
};
pub use swarm_banner::{
    resolve_swarm_banner, ActiveNamedAgent, LeaderTeamContext, SwarmBannerInfo, SwarmBannerInput,
    ViewedTeammate,
};
pub use task_list_view::{
    adjusted_subject_width, max_display_rows, owner_display_width, resolve_task_list_layout,
    should_show_owner, task_icon, CompletionTimestamp, HiddenSummary, ListTask, TaskCounts,
    TaskIcon, TaskItemLayout, TaskListLayout, TaskListLayoutInput, TaskListStatus,
    RECENT_COMPLETED_TTL_MS,
};
pub use text_input_view::{build_text_input_view_state, TextInputViewInput, TextInputViewState};
pub use truncate_once::{TruncateOnceEffect, TruncateOnceState};
pub use utils::{
    clamp_cursor_offset, get_newline_instructions, is_non_space_printable, is_vim_mode_enabled,
    KeyInput, NewlineInstructionInput,
};
