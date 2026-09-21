//! The rewind picker: a two-screen state machine (message pick list →
//! restore confirm), the restore-option reducer, the diff-stats shape and
//! the message-filter rules.
//!
//! ## Covered behavior
//!
//! * The `MAX_VISIBLE_MESSAGES = 7` pin.
//! * The restore-option enum (`both`, `conversation`, `code`,
//!   `summarize`, `summarize_up_to`, `nevermind`) and
//!   [`is_summarize_option`].
//! * The [`build_restore_options`] option-list builder, with and without
//!   `can_restore_code`.
//! * The [`restore_option_conversation_text`] table.
//! * The restore-code file label projection (1 / 2 / 3+ files).
//! * The [`build_restore_code_confirmation`] branches (not loaded / no
//!   changes / restorable with diff).
//! * The [`first_visible_index`] scroll-window math.
//! * The [`selectable_user_message`] predicate and its
//!   synthetic-message exclusions.
//! * The [`messages_after_are_only_synthetic`] predicate.
//! * The `DiffStats` + [`format_diff_stats`] formatting shape.
//! * The navigation reducer (up / down / top / bottom).
//! * The escape reducer (back to the pick list, or close).
//! * The preselected-message branch (lands on confirm directly).
//! * The message-display projection ([`UserMessageDisplay`]): current,
//!   empty, bash-input, command, skill format, default text.
//! * The [`format_restore_error`] text assembly.
//!
//! ## Out of scope
//!
//! * Terminal rendering.
//! * File-history diff stats and restore eligibility — accepted as
//!   `DiffStats` inputs.
//! * The actual restore / summarize calls — the reducers emit an action
//!   enum the consumer routes.
//! * Per-option text input for summarize feedback — accepted as an
//!   input to [`handle_restore_option`].

use std::cmp::{max, min};

/// Maximum number of messages shown at once.
pub const MAX_VISIBLE_MESSAGES: usize = 7;

/// The restore option the user picks on the confirm screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestoreOption {
    /// Restore both code and conversation.
    Both,
    /// Restore only the conversation (fork).
    Conversation,
    /// Restore only the code snapshot.
    Code,
    /// Summarize messages from this point forward.
    Summarize,
    /// Summarize messages up to this point (leave current and later
    /// messages unchanged).
    SummarizeUpTo,
    /// Dismiss without restoring (goes back to the pick list, or
    /// closes entirely when a message was preselected).
    Nevermind,
}

impl RestoreOption {
    /// Machine-readable id for this option.
    pub fn id(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Conversation => "conversation",
            Self::Code => "code",
            Self::Summarize => "summarize",
            Self::SummarizeUpTo => "summarize_up_to",
            Self::Nevermind => "nevermind",
        }
    }

    /// Display label shown in the option list.
    pub fn label(self) -> &'static str {
        match self {
            Self::Both => "Restore code and conversation",
            Self::Conversation => "Restore conversation",
            Self::Code => "Restore code",
            Self::Summarize => "Summarize from here",
            Self::SummarizeUpTo => "Summarize up to here",
            Self::Nevermind => "Never mind",
        }
    }
}

/// Is this option a summarize variant?
pub fn is_summarize_option(option: RestoreOption) -> bool {
    matches!(
        option,
        RestoreOption::Summarize | RestoreOption::SummarizeUpTo
    )
}

/// Whether `RestoreOption::SummarizeUpTo` is included in the list.
/// `summarize_up_to` is exposed as a parameter so the consumer
/// controls the flag.
pub const INCLUDE_SUMMARIZE_UP_TO_FOR_ANT: bool = false;

/// Build the list of restore options.
pub fn build_restore_options(
    can_restore_code: bool,
    include_summarize_up_to: bool,
) -> Vec<RestoreOption> {
    let mut out: Vec<RestoreOption> = if can_restore_code {
        vec![
            RestoreOption::Both,
            RestoreOption::Conversation,
            RestoreOption::Code,
        ]
    } else {
        vec![RestoreOption::Conversation]
    };
    out.push(RestoreOption::Summarize);
    if include_summarize_up_to {
        out.push(RestoreOption::SummarizeUpTo);
    }
    out.push(RestoreOption::Nevermind);
    out
}

/// Return the text shown below the restore-option list when each
/// option is focused.
pub fn restore_option_conversation_text(option: RestoreOption) -> &'static str {
    match option {
        RestoreOption::Summarize => "Messages after this point will be summarized.",
        RestoreOption::SummarizeUpTo => {
            "Preceding messages will be summarized. This and subsequent messages will remain unchanged — you will stay at the end of the conversation."
        }
        RestoreOption::Both | RestoreOption::Conversation => {
            "The conversation will be forked."
        }
        RestoreOption::Code | RestoreOption::Nevermind => {
            "The conversation will be unchanged."
        }
    }
}

/// The diff-stats shape between two messages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffStats {
    /// Paths of files changed between the two messages (empty when
    /// no files changed).
    pub files_changed: Vec<String>,
    /// Total inserted lines.
    pub insertions: usize,
    /// Total deleted lines.
    pub deletions: usize,
}

/// Format a `DiffStats` as display text — `+N -M` — or the empty string
/// when `files_changed` is empty.
pub fn format_diff_stats(diff: &DiffStats) -> String {
    if diff.files_changed.is_empty() {
        return String::new();
    }
    format!("+{} -{}", diff.insertions, diff.deletions)
}

/// Compute the scroll window's first visible index:
/// `max(0, min(selected_index - MAX_VISIBLE_MESSAGES/2, total - MAX_VISIBLE_MESSAGES))`.
pub fn first_visible_index(selected_index: usize, total: usize) -> usize {
    // Both differences go through `saturating_sub`, so a short list
    // (`total <= MAX_VISIBLE_MESSAGES`) clamps to 0 without signed math.
    let half = MAX_VISIBLE_MESSAGES / 2;
    let selected_minus_half = selected_index.saturating_sub(half);
    let total_minus_max = total.saturating_sub(MAX_VISIBLE_MESSAGES);
    // The outer `max(0, …)` of the formula is therefore implicit.
    let clamped = min(selected_minus_half, total_minus_max);
    max(0, clamped)
}

/// Build the file-label shown on the restore-code confirmation line.
///
/// 1 file → `basename(file0)`
/// 2 files → `basename(file0) and basename(file1)`
/// 3+ files → `basename(file0) and N other files`
pub fn build_restore_code_file_label(files_changed: &[String]) -> Option<String> {
    let first = files_changed.first()?;
    let first_basename = basename(first);
    match files_changed.len() {
        0 => None,
        1 => Some(first_basename),
        2 => {
            let second = basename(files_changed.get(1)?);
            Some(format!("{first_basename} and {second}"))
        }
        n => Some(format!(
            "{first_basename} and {} other files",
            n.saturating_sub(1)
        )),
    }
}

/// Extract the basename of a path: the text after the last `/` or `\`
/// separator, or the whole path when there is none. A trailing separator
/// therefore yields an empty name.
pub fn basename(path: &str) -> String {
    let last_sep = path.rfind(|c: char| c == '/' || c == '\\');
    match last_sep {
        Some(idx) => path[idx + 1..].to_string(),
        None => path.to_string(),
    }
}

/// The sub-text shown under the file label in the restore-code
/// confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreCodeConfirmation {
    /// No diff stats were supplied (`None`) — nothing to confirm.
    NotLoaded,
    /// `files_changed` is empty → "The code has not changed (nothing
    /// will be restored)."
    NoChanges,
    /// `files_changed` has entries → "The code will be restored +N -M
    /// in {label}."
    Restorable {
        /// The file label shown (e.g. `"foo.rs and 2 other files"`).
        file_label: String,
        /// The +N -M diff stats text.
        diff_text: String,
    },
}

/// Build the restore-code confirmation shape.
pub fn build_restore_code_confirmation(diff: Option<&DiffStats>) -> RestoreCodeConfirmation {
    let Some(diff) = diff else {
        return RestoreCodeConfirmation::NotLoaded;
    };
    let Some(file_label) = build_restore_code_file_label(&diff.files_changed) else {
        return RestoreCodeConfirmation::NoChanges;
    };
    RestoreCodeConfirmation::Restorable {
        file_label,
        diff_text: format_diff_stats(diff),
    }
}

/// Whether a user message should appear in the rewind list. The
/// consumer supplies a pre-built `UserMessageFilterInput` (already
/// stripped of unsupported message kinds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessageFilterInput {
    /// Whether the message's kind is `user`.
    pub is_user_type: bool,
    /// Whether the message content is a single tool-result block.
    pub is_tool_result: bool,
    /// Whether the message is synthetic (cancellation / interrupt).
    pub is_synthetic: bool,
    /// Whether the message has the `is_meta` flag.
    pub is_meta: bool,
    /// Whether the message is a compact summary.
    pub is_compact_summary: bool,
    /// Whether the message is transcript-only.
    pub is_visible_in_transcript_only: bool,
    /// The computed last-block text (already stripped of unsupported
    /// blocks, never `None`).
    pub message_text: String,
}

/// The XML-tag exclusion list. A message whose text contains any
/// of these tags is filtered out.
pub const EXCLUDED_TAGS: &[&str] = &[
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<bash-stdout>",
    "<bash-stderr>",
    "<task-notification>",
    "<tick>",
    "<teammate-message",
];

/// Apply the user-message selectable filter. Returns `true` when the
/// message should appear in the pick list.
pub fn selectable_user_message(input: &UserMessageFilterInput) -> bool {
    if !input.is_user_type {
        return false;
    }
    if input.is_tool_result {
        return false;
    }
    if input.is_synthetic {
        return false;
    }
    if input.is_meta {
        return false;
    }
    if input.is_compact_summary || input.is_visible_in_transcript_only {
        return false;
    }
    for tag in EXCLUDED_TAGS {
        if input.message_text.contains(tag) {
            return false;
        }
    }
    true
}

/// One of the simplified message-kind markers used by
/// [`messages_after_are_only_synthetic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    /// A `user` message without the meta flag — meaningful.
    User,
    /// A `user` message carrying the meta flag — skipped.
    UserMeta,
    /// An `assistant` message — meaningful only when its text is
    /// non-blank (`text.trim() != ""`) or it carries a `tool_use` block.
    Assistant {
        /// Whether the assistant message has any meaningful content.
        has_meaningful_content: bool,
    },
    /// Synthetic (interrupt / cancel).
    Synthetic,
    /// Tool-result message.
    ToolResult,
    /// A `progress` message.
    Progress,
    /// A `system` message.
    System,
    /// An `attachment` message.
    Attachment,
    /// Anything else (e.g. tombstone) — non-meaningful.
    Other,
}

/// Returns `true` when every message after `from_index` is
/// non-meaningful.
pub fn messages_after_are_only_synthetic(messages: &[MessageKind], from_index: usize) -> bool {
    if from_index + 1 >= messages.len() {
        return true;
    }
    for msg in &messages[from_index + 1..] {
        match msg {
            MessageKind::Synthetic
            | MessageKind::ToolResult
            | MessageKind::Progress
            | MessageKind::System
            | MessageKind::Attachment
            | MessageKind::UserMeta
            | MessageKind::Other => continue,
            MessageKind::Assistant {
                has_meaningful_content,
            } => {
                if *has_meaningful_content {
                    return false;
                }
                continue;
            }
            MessageKind::User => return false,
        }
    }
    true
}

/// The two screens of the rewind state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSelectorScreen {
    /// The message pick list.
    PickList,
    /// The restore-confirm screen.
    Confirm,
    /// Error state showing a message.
    Error(String),
}

/// The full state machine state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSelectorState {
    /// The current screen.
    pub screen: MessageSelectorScreen,
    /// The index of the selected message in the options list.
    pub selected_index: usize,
    /// The total number of options (including the synthetic "current
    /// prompt" placeholder).
    pub total_options: usize,
    /// Whether a preselected message was supplied — when true, cancel
    /// from the confirm screen closes instead of going back.
    pub has_preselected: bool,
    /// Whether an async restore is in flight.
    pub is_restoring: bool,
}

impl MessageSelectorState {
    /// Create a fresh state. If `preselected` is `true`, the state
    /// lands directly on the confirm screen.
    pub fn new(total_options: usize, preselected: bool) -> Self {
        Self {
            screen: if preselected {
                MessageSelectorScreen::Confirm
            } else {
                MessageSelectorScreen::PickList
            },
            // Start on the last option — the synthetic "current prompt"
            // entry.
            selected_index: total_options.saturating_sub(1),
            total_options,
            has_preselected: preselected,
            is_restoring: false,
        }
    }

    /// Navigate up one row.
    pub fn move_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
    }

    /// Navigate down one row.
    pub fn move_down(&mut self) {
        let max_idx = self.total_options.saturating_sub(1);
        if self.selected_index < max_idx {
            self.selected_index += 1;
        }
    }

    /// Jump to the top.
    pub fn jump_to_top(&mut self) {
        self.selected_index = 0;
    }

    /// Jump to the bottom.
    pub fn jump_to_bottom(&mut self) {
        self.selected_index = self.total_options.saturating_sub(1);
    }
}

/// The action the consumer must route after a reducer returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSelectorAction {
    /// Close the rewind dialog entirely.
    Close,
    /// Open the restore-confirm screen for the given message index.
    OpenConfirm {
        /// Index in the options list.
        message_index: usize,
    },
    /// Run the restore with the given option.
    Restore {
        /// Restore option chosen.
        option: RestoreOption,
        /// The feedback string (for summarize variants, trimmed).
        feedback: Option<String>,
    },
    /// Dismiss confirm (go back to pick list) or close if
    /// preselected.
    DismissConfirm,
}

/// Handle the escape key:
/// * If on the confirm screen and not preselected → go back to list.
/// * Otherwise → close the dialog.
pub fn handle_escape(state: &MessageSelectorState) -> MessageSelectorAction {
    match &state.screen {
        MessageSelectorScreen::Confirm if !state.has_preselected => {
            MessageSelectorAction::DismissConfirm
        }
        _ => MessageSelectorAction::Close,
    }
}

/// Handle the select-current-message event. Returns the action the consumer
/// should route: [`MessageSelectorAction::OpenConfirm`] when file
/// history is enabled, or a straight `Restore { option: Conversation, .. }`
/// when it isn't.
pub fn handle_select_current(
    selected_index: usize,
    is_file_history_enabled: bool,
) -> MessageSelectorAction {
    if is_file_history_enabled {
        MessageSelectorAction::OpenConfirm {
            message_index: selected_index,
        }
    } else {
        // No file history: restore the conversation directly.
        MessageSelectorAction::Restore {
            option: RestoreOption::Conversation,
            feedback: None,
        }
    }
}

/// Handle the restore-option select event.
pub fn handle_restore_option(
    option: RestoreOption,
    summarize_from_feedback: &str,
    summarize_up_to_feedback: &str,
) -> MessageSelectorAction {
    match option {
        RestoreOption::Nevermind => MessageSelectorAction::DismissConfirm,
        RestoreOption::Summarize => MessageSelectorAction::Restore {
            option,
            feedback: trim_optional(summarize_from_feedback),
        },
        RestoreOption::SummarizeUpTo => MessageSelectorAction::Restore {
            option,
            feedback: trim_optional(summarize_up_to_feedback),
        },
        RestoreOption::Both | RestoreOption::Conversation | RestoreOption::Code => {
            MessageSelectorAction::Restore {
                option,
                feedback: None,
            }
        }
    }
}

fn trim_optional(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Build the restore-error message from the conversation and code
/// restore errors.
pub fn format_restore_error(
    conversation_error: Option<&str>,
    code_error: Option<&str>,
) -> Option<String> {
    match (conversation_error, code_error) {
        (Some(c), Some(k)) => Some(format!(
            "Failed to restore the conversation and code:\n{c}\n{k}"
        )),
        (Some(c), None) => Some(format!("Failed to restore the conversation:\n{c}")),
        (None, Some(k)) => Some(format!("Failed to restore the code:\n{k}")),
        (None, None) => None,
    }
}

/// The display kind for a user-message row in the rewind pick list, as
/// produced by [`project_user_message_display`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMessageDisplay {
    /// The virtual "current prompt" row.
    Current,
    /// Empty message row.
    Empty,
    /// A bash-input row (e.g. `! ls`).
    BashInput {
        /// The bash command text without the tag.
        input: String,
    },
    /// A slash-command invocation.
    Command {
        /// The command name (without the `/`).
        name: String,
        /// The args string (may be empty).
        args: String,
    },
    /// A skill-format command (`Skill(name)`).
    Skill {
        /// The skill name.
        name: String,
    },
    /// Default text row.
    Text {
        /// The prepared display text (already truncated / sliced).
        text: String,
    },
}

/// Project a raw last-block text to a display row. The caller has
/// already stripped display tags from the text.
///
/// * `is_current` — the virtual "current prompt" row.
/// * `is_empty_text` — the caller's "empty message text" predicate.
/// * `message_text` — the post-strip text.
/// * `padding_right` — when `Some`, truncate to `columns - padding`;
///   when `None`, slice first 500 chars and first 4 newlines.
pub fn project_user_message_display(
    is_current: bool,
    is_empty_text: bool,
    message_text: &str,
    columns: usize,
    padding_right: Option<usize>,
) -> UserMessageDisplay {
    if is_current {
        return UserMessageDisplay::Current;
    }
    if is_empty_text {
        return UserMessageDisplay::Empty;
    }
    if message_text.contains("<bash-input>") {
        if let Some(inner) = extract_tag(message_text, "bash-input") {
            return UserMessageDisplay::BashInput { input: inner };
        }
    }
    if message_text.contains("<command-message>") {
        if let Some(cmd) = extract_tag(message_text, "command-message") {
            let args = extract_tag(message_text, "command-args").unwrap_or_default();
            let is_skill = extract_tag(message_text, "skill-format")
                .map(|s| s == "true")
                .unwrap_or(false);
            if is_skill {
                return UserMessageDisplay::Skill { name: cmd };
            } else {
                return UserMessageDisplay::Command { name: cmd, args };
            }
        }
    }
    // Default text row.
    let text = match padding_right {
        Some(pad) => {
            // Truncate to columns - pad characters.
            let max_width = columns.saturating_sub(pad);
            truncate_to_width(message_text, max_width)
        }
        None => {
            // Slice first 500 chars and first 4 newlines.
            let sliced: String = message_text.chars().take(500).collect();
            sliced.split('\n').take(4).collect::<Vec<_>>().join("\n")
        }
    };
    UserMessageDisplay::Text { text }
}

/// Truncate to `max_chars`; when the text is longer, keep
/// `max_chars - 1` characters and append `…`.
pub fn truncate_to_width(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let take = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

/// Extract the first `<tag>…</tag>` payload from `text`. Returns
/// `None` if not found.
pub fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)?;
    Some(text[start..start + end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_visible_messages_is_7() {
        assert_eq!(MAX_VISIBLE_MESSAGES, 7);
    }

    #[test]
    fn restore_option_ids() {
        assert_eq!(RestoreOption::Both.id(), "both");
        assert_eq!(RestoreOption::Conversation.id(), "conversation");
        assert_eq!(RestoreOption::Code.id(), "code");
        assert_eq!(RestoreOption::Summarize.id(), "summarize");
        assert_eq!(RestoreOption::SummarizeUpTo.id(), "summarize_up_to");
        assert_eq!(RestoreOption::Nevermind.id(), "nevermind");
    }

    #[test]
    fn restore_option_labels() {
        assert_eq!(RestoreOption::Both.label(), "Restore code and conversation");
        assert_eq!(RestoreOption::Conversation.label(), "Restore conversation");
        assert_eq!(RestoreOption::Code.label(), "Restore code");
        assert_eq!(RestoreOption::Summarize.label(), "Summarize from here");
        assert_eq!(RestoreOption::SummarizeUpTo.label(), "Summarize up to here");
        assert_eq!(RestoreOption::Nevermind.label(), "Never mind");
    }

    #[test]
    fn is_summarize_option_table() {
        assert!(is_summarize_option(RestoreOption::Summarize));
        assert!(is_summarize_option(RestoreOption::SummarizeUpTo));
        assert!(!is_summarize_option(RestoreOption::Both));
        assert!(!is_summarize_option(RestoreOption::Conversation));
        assert!(!is_summarize_option(RestoreOption::Code));
        assert!(!is_summarize_option(RestoreOption::Nevermind));
    }

    #[test]
    fn build_restore_options_with_code() {
        let opts = build_restore_options(true, false);
        assert_eq!(
            opts,
            vec![
                RestoreOption::Both,
                RestoreOption::Conversation,
                RestoreOption::Code,
                RestoreOption::Summarize,
                RestoreOption::Nevermind,
            ]
        );
    }

    #[test]
    fn build_restore_options_without_code() {
        let opts = build_restore_options(false, false);
        assert_eq!(
            opts,
            vec![
                RestoreOption::Conversation,
                RestoreOption::Summarize,
                RestoreOption::Nevermind,
            ]
        );
    }

    #[test]
    fn build_restore_options_with_summarize_up_to() {
        let opts = build_restore_options(true, true);
        assert!(opts.contains(&RestoreOption::SummarizeUpTo));
        assert_eq!(opts[opts.len() - 1], RestoreOption::Nevermind);
    }

    #[test]
    fn restore_option_conversation_text_table() {
        assert_eq!(
            restore_option_conversation_text(RestoreOption::Summarize),
            "Messages after this point will be summarized."
        );
        assert!(
            restore_option_conversation_text(RestoreOption::SummarizeUpTo)
                .starts_with("Preceding messages will be summarized.")
        );
        assert_eq!(
            restore_option_conversation_text(RestoreOption::Both),
            "The conversation will be forked."
        );
        assert_eq!(
            restore_option_conversation_text(RestoreOption::Conversation),
            "The conversation will be forked."
        );
        assert_eq!(
            restore_option_conversation_text(RestoreOption::Code),
            "The conversation will be unchanged."
        );
        assert_eq!(
            restore_option_conversation_text(RestoreOption::Nevermind),
            "The conversation will be unchanged."
        );
    }

    #[test]
    fn format_diff_stats_empty() {
        let d = DiffStats::default();
        assert_eq!(format_diff_stats(&d), "");
    }

    #[test]
    fn format_diff_stats_with_files() {
        let d = DiffStats {
            files_changed: vec!["foo.ts".to_string()],
            insertions: 3,
            deletions: 5,
        };
        assert_eq!(format_diff_stats(&d), "+3 -5");
    }

    #[test]
    fn first_visible_index_small_list() {
        // total < MAX_VISIBLE_MESSAGES — the window always starts at 0.
        assert_eq!(first_visible_index(0, 3), 0);
        assert_eq!(first_visible_index(2, 3), 0);
    }

    #[test]
    fn first_visible_index_selected_at_top() {
        // total == 20, MAX=7, half=3, selected=0
        // max(0, min(0-3, 20-7)) = max(0, min(0, 13)) = 0
        assert_eq!(first_visible_index(0, 20), 0);
    }

    #[test]
    fn first_visible_index_middle() {
        // total == 20, selected=10 → min(10-3, 20-7) = min(7, 13) = 7
        assert_eq!(first_visible_index(10, 20), 7);
    }

    #[test]
    fn first_visible_index_selected_at_bottom_clamps_to_max() {
        // total == 20, selected=19 → min(19-3, 20-7) = min(16, 13) = 13
        assert_eq!(first_visible_index(19, 20), 13);
    }

    #[test]
    fn first_visible_index_exact_max() {
        // total == 7, selected = 6 → min(3, 0) = 0
        assert_eq!(first_visible_index(6, 7), 0);
    }

    #[test]
    fn basename_unix() {
        assert_eq!(basename("/tmp/foo/bar.ts"), "bar.ts");
    }

    #[test]
    fn basename_windows() {
        assert_eq!(basename("C:\\foo\\bar.ts"), "bar.ts");
    }

    #[test]
    fn basename_no_sep() {
        assert_eq!(basename("just-a-file.ts"), "just-a-file.ts");
    }

    #[test]
    fn basename_empty() {
        assert_eq!(basename(""), "");
    }

    #[test]
    fn build_restore_code_file_label_one_file() {
        let files = vec!["/a/foo.ts".to_string()];
        assert_eq!(build_restore_code_file_label(&files).unwrap(), "foo.ts");
    }

    #[test]
    fn build_restore_code_file_label_two_files() {
        let files = vec!["/a/foo.ts".to_string(), "/b/bar.ts".to_string()];
        assert_eq!(
            build_restore_code_file_label(&files).unwrap(),
            "foo.ts and bar.ts"
        );
    }

    #[test]
    fn build_restore_code_file_label_three_plus() {
        let files = vec![
            "a.ts".to_string(),
            "b.ts".to_string(),
            "c.ts".to_string(),
            "d.ts".to_string(),
        ];
        assert_eq!(
            build_restore_code_file_label(&files).unwrap(),
            "a.ts and 3 other files"
        );
    }

    #[test]
    fn build_restore_code_file_label_empty() {
        assert_eq!(build_restore_code_file_label(&[]), None);
    }

    #[test]
    fn build_restore_code_confirmation_not_loaded() {
        assert_eq!(
            build_restore_code_confirmation(None),
            RestoreCodeConfirmation::NotLoaded
        );
    }

    #[test]
    fn build_restore_code_confirmation_no_changes() {
        let d = DiffStats::default();
        assert_eq!(
            build_restore_code_confirmation(Some(&d)),
            RestoreCodeConfirmation::NoChanges
        );
    }

    #[test]
    fn build_restore_code_confirmation_restorable() {
        let d = DiffStats {
            files_changed: vec!["/a/foo.ts".to_string()],
            insertions: 2,
            deletions: 1,
        };
        assert_eq!(
            build_restore_code_confirmation(Some(&d)),
            RestoreCodeConfirmation::Restorable {
                file_label: "foo.ts".to_string(),
                diff_text: "+2 -1".to_string(),
            }
        );
    }

    fn ok_filter_input() -> UserMessageFilterInput {
        UserMessageFilterInput {
            is_user_type: true,
            is_tool_result: false,
            is_synthetic: false,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            message_text: "hello".to_string(),
        }
    }

    #[test]
    fn selectable_happy_path() {
        assert!(selectable_user_message(&ok_filter_input()));
    }

    #[test]
    fn selectable_not_user_type() {
        let mut i = ok_filter_input();
        i.is_user_type = false;
        assert!(!selectable_user_message(&i));
    }

    #[test]
    fn selectable_tool_result() {
        let mut i = ok_filter_input();
        i.is_tool_result = true;
        assert!(!selectable_user_message(&i));
    }

    #[test]
    fn selectable_synthetic() {
        let mut i = ok_filter_input();
        i.is_synthetic = true;
        assert!(!selectable_user_message(&i));
    }

    #[test]
    fn selectable_meta() {
        let mut i = ok_filter_input();
        i.is_meta = true;
        assert!(!selectable_user_message(&i));
    }

    #[test]
    fn selectable_compact_summary() {
        let mut i = ok_filter_input();
        i.is_compact_summary = true;
        assert!(!selectable_user_message(&i));
    }

    #[test]
    fn selectable_transcript_only() {
        let mut i = ok_filter_input();
        i.is_visible_in_transcript_only = true;
        assert!(!selectable_user_message(&i));
    }

    #[test]
    fn selectable_excluded_tags() {
        for tag in EXCLUDED_TAGS {
            let mut i = ok_filter_input();
            i.message_text = format!("before {} after", tag);
            assert!(!selectable_user_message(&i), "tag {tag} should exclude");
        }
    }

    #[test]
    fn messages_after_empty_returns_true() {
        assert!(messages_after_are_only_synthetic(&[], 0));
    }

    #[test]
    fn messages_after_from_last_index_returns_true() {
        let msgs = vec![MessageKind::User, MessageKind::User];
        assert!(messages_after_are_only_synthetic(&msgs, 1));
    }

    #[test]
    fn messages_after_has_user_returns_false() {
        let msgs = vec![MessageKind::User, MessageKind::User];
        assert!(!messages_after_are_only_synthetic(&msgs, 0));
    }

    #[test]
    fn messages_after_has_meaningful_assistant_returns_false() {
        let msgs = vec![
            MessageKind::User,
            MessageKind::Assistant {
                has_meaningful_content: true,
            },
        ];
        assert!(!messages_after_are_only_synthetic(&msgs, 0));
    }

    #[test]
    fn messages_after_ignores_empty_assistant() {
        let msgs = vec![
            MessageKind::User,
            MessageKind::Assistant {
                has_meaningful_content: false,
            },
        ];
        assert!(messages_after_are_only_synthetic(&msgs, 0));
    }

    #[test]
    fn messages_after_synthetic_chain() {
        let msgs = vec![
            MessageKind::User,
            MessageKind::Synthetic,
            MessageKind::ToolResult,
            MessageKind::Progress,
            MessageKind::System,
            MessageKind::Attachment,
            MessageKind::UserMeta,
            MessageKind::Other,
        ];
        assert!(messages_after_are_only_synthetic(&msgs, 0));
    }

    #[test]
    fn state_new_no_preselect() {
        let s = MessageSelectorState::new(5, false);
        assert_eq!(s.screen, MessageSelectorScreen::PickList);
        assert_eq!(s.selected_index, 4);
        assert!(!s.has_preselected);
    }

    #[test]
    fn state_new_preselect_lands_on_confirm() {
        let s = MessageSelectorState::new(5, true);
        assert_eq!(s.screen, MessageSelectorScreen::Confirm);
        assert!(s.has_preselected);
    }

    #[test]
    fn state_move_up_and_down() {
        let mut s = MessageSelectorState::new(5, false);
        assert_eq!(s.selected_index, 4);
        s.move_up();
        assert_eq!(s.selected_index, 3);
        s.move_down();
        assert_eq!(s.selected_index, 4);
        // Clamp at bottom
        s.move_down();
        assert_eq!(s.selected_index, 4);
    }

    #[test]
    fn state_move_up_clamps_at_zero() {
        let mut s = MessageSelectorState::new(5, false);
        s.jump_to_top();
        assert_eq!(s.selected_index, 0);
        s.move_up();
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn state_jump_to_top_and_bottom() {
        let mut s = MessageSelectorState::new(5, false);
        s.jump_to_top();
        assert_eq!(s.selected_index, 0);
        s.jump_to_bottom();
        assert_eq!(s.selected_index, 4);
    }

    #[test]
    fn state_zero_options() {
        let s = MessageSelectorState::new(0, false);
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn handle_escape_pick_list_closes() {
        let s = MessageSelectorState::new(5, false);
        assert_eq!(handle_escape(&s), MessageSelectorAction::Close);
    }

    #[test]
    fn handle_escape_confirm_goes_back() {
        let mut s = MessageSelectorState::new(5, false);
        s.screen = MessageSelectorScreen::Confirm;
        assert_eq!(handle_escape(&s), MessageSelectorAction::DismissConfirm);
    }

    #[test]
    fn handle_escape_confirm_preselected_closes() {
        let mut s = MessageSelectorState::new(5, true);
        s.screen = MessageSelectorScreen::Confirm;
        assert_eq!(handle_escape(&s), MessageSelectorAction::Close);
    }

    #[test]
    fn handle_select_current_fh_enabled_opens_confirm() {
        assert_eq!(
            handle_select_current(3, true),
            MessageSelectorAction::OpenConfirm { message_index: 3 }
        );
    }

    #[test]
    fn handle_select_current_fh_disabled_restores_conversation() {
        assert_eq!(
            handle_select_current(3, false),
            MessageSelectorAction::Restore {
                option: RestoreOption::Conversation,
                feedback: None,
            }
        );
    }

    #[test]
    fn handle_restore_option_nevermind() {
        assert_eq!(
            handle_restore_option(RestoreOption::Nevermind, "", ""),
            MessageSelectorAction::DismissConfirm
        );
    }

    #[test]
    fn handle_restore_option_summarize_trimmed_feedback() {
        assert_eq!(
            handle_restore_option(RestoreOption::Summarize, "  hey there  ", ""),
            MessageSelectorAction::Restore {
                option: RestoreOption::Summarize,
                feedback: Some("hey there".to_string()),
            }
        );
    }

    #[test]
    fn handle_restore_option_summarize_empty_feedback_becomes_none() {
        assert_eq!(
            handle_restore_option(RestoreOption::Summarize, "   ", ""),
            MessageSelectorAction::Restore {
                option: RestoreOption::Summarize,
                feedback: None,
            }
        );
    }

    #[test]
    fn handle_restore_option_summarize_up_to_uses_up_to_feedback() {
        assert_eq!(
            handle_restore_option(RestoreOption::SummarizeUpTo, "ignored", "earlier context"),
            MessageSelectorAction::Restore {
                option: RestoreOption::SummarizeUpTo,
                feedback: Some("earlier context".to_string()),
            }
        );
    }

    #[test]
    fn handle_restore_option_both() {
        assert_eq!(
            handle_restore_option(RestoreOption::Both, "", ""),
            MessageSelectorAction::Restore {
                option: RestoreOption::Both,
                feedback: None,
            }
        );
    }

    #[test]
    fn handle_restore_option_conversation() {
        assert_eq!(
            handle_restore_option(RestoreOption::Conversation, "", ""),
            MessageSelectorAction::Restore {
                option: RestoreOption::Conversation,
                feedback: None,
            }
        );
    }

    #[test]
    fn handle_restore_option_code() {
        assert_eq!(
            handle_restore_option(RestoreOption::Code, "", ""),
            MessageSelectorAction::Restore {
                option: RestoreOption::Code,
                feedback: None,
            }
        );
    }

    #[test]
    fn format_restore_error_both() {
        let msg = format_restore_error(Some("A"), Some("B")).unwrap();
        assert!(msg.contains("Failed to restore the conversation and code"));
        assert!(msg.contains('A'));
        assert!(msg.contains('B'));
    }

    #[test]
    fn format_restore_error_conversation_only() {
        let msg = format_restore_error(Some("oh"), None).unwrap();
        assert_eq!(msg, "Failed to restore the conversation:\noh");
    }

    #[test]
    fn format_restore_error_code_only() {
        let msg = format_restore_error(None, Some("oh")).unwrap();
        assert_eq!(msg, "Failed to restore the code:\noh");
    }

    #[test]
    fn format_restore_error_none() {
        assert_eq!(format_restore_error(None, None), None);
    }

    #[test]
    fn display_current() {
        let d = project_user_message_display(true, false, "", 80, Some(10));
        assert_eq!(d, UserMessageDisplay::Current);
    }

    #[test]
    fn display_empty() {
        let d = project_user_message_display(false, true, "", 80, Some(10));
        assert_eq!(d, UserMessageDisplay::Empty);
    }

    #[test]
    fn display_bash_input() {
        let d = project_user_message_display(
            false,
            false,
            "<bash-input>ls -la</bash-input>",
            80,
            Some(10),
        );
        assert_eq!(
            d,
            UserMessageDisplay::BashInput {
                input: "ls -la".to_string()
            }
        );
    }

    #[test]
    fn display_command() {
        let text = "<command-message>foo</command-message><command-args>bar baz</command-args>";
        let d = project_user_message_display(false, false, text, 80, Some(10));
        assert_eq!(
            d,
            UserMessageDisplay::Command {
                name: "foo".to_string(),
                args: "bar baz".to_string(),
            }
        );
    }

    #[test]
    fn display_skill() {
        let text = "<command-message>research</command-message><command-args></command-args><skill-format>true</skill-format>";
        let d = project_user_message_display(false, false, text, 80, Some(10));
        assert_eq!(
            d,
            UserMessageDisplay::Skill {
                name: "research".to_string()
            }
        );
    }

    #[test]
    fn display_text_truncated_with_padding() {
        let d = project_user_message_display(false, false, "hello world", 10, Some(5));
        if let UserMessageDisplay::Text { text } = d {
            assert_eq!(text.chars().count(), 5);
        } else {
            panic!("expected text");
        }
    }

    #[test]
    fn display_text_no_padding_slices_500_and_4_lines() {
        let long = "a\nb\nc\nd\ne\nf\ng";
        let d = project_user_message_display(false, false, long, 80, None);
        if let UserMessageDisplay::Text { text } = d {
            assert_eq!(text, "a\nb\nc\nd");
        } else {
            panic!("expected text");
        }
    }

    #[test]
    fn extract_tag_present() {
        assert_eq!(
            extract_tag("hello <tag>world</tag> end", "tag"),
            Some("world".to_string())
        );
    }

    #[test]
    fn extract_tag_absent() {
        assert_eq!(extract_tag("no tag here", "tag"), None);
    }

    #[test]
    fn truncate_to_width_table() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello world", 5), "hell…");
        assert_eq!(truncate_to_width("hello", 0), "");
        assert_eq!(truncate_to_width("hello", 1), "…");
    }
}
