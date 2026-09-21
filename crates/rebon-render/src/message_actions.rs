//! Pure logic behind the message-actions bar: which rows are navigable,
//! which tool input a row acts on, which buttons apply, what text gets copied,
//! and how a keypress is dispatched.

use rebon_tools_core::{builtin_tool_facts_for_name, PrimaryInput};

use crate::assistant_text::is_empty_assistant_message_text;
use crate::tool_results::{CANCEL_MESSAGE, INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE};

const NO_RESPONSE_REQUESTED: &str = "No response requested.";
const SYNTHETIC_MESSAGES: &[&str] = &[
    INTERRUPT_MESSAGE,
    INTERRUPT_MESSAGE_FOR_TOOL_USE,
    CANCEL_MESSAGE,
    crate::tool_results::REJECT_MESSAGE,
    NO_RESPONSE_REQUESTED,
];

/// Minimal message row for message-actions logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageActionMessage {
    /// User row.
    User(MessageActionUserMessage),
    /// Assistant row.
    Assistant(MessageActionAssistantMessage),
    /// Grouped tool-use row.
    GroupedToolUse(MessageActionGroupedToolUse),
    /// Collapsed read/search row.
    CollapsedReadSearch(MessageActionCollapsedReadSearch),
    /// System row.
    System(MessageActionSystemMessage),
    /// Attachment row.
    Attachment(MessageActionAttachment),
}

/// Minimal user row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionUserMessage {
    /// UUID.
    pub uuid: String,
    /// Whether the row is harness-authored rather than typed by the user.
    pub is_meta: bool,
    /// Whether the row is a compaction summary.
    pub is_compact_summary: bool,
    /// First text block only.
    pub text: Option<String>,
}

/// Assistant block kinds relevant to message actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageActionAssistantBlock {
    /// Text block.
    Text(String),
    /// Tool-use block.
    ToolUse {
        /// Tool name.
        name: String,
        /// Tool input.
        input: MessageActionToolInput,
    },
    /// Any other block.
    Other,
}

/// Minimal assistant row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionAssistantMessage {
    /// UUID.
    pub uuid: String,
    /// First block.
    pub first_block: MessageActionAssistantBlock,
}

/// Tool input fields that message actions can extract.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageActionToolInput {
    /// `file_path`.
    pub file_path: Option<String>,
    /// `notebook_path`.
    pub notebook_path: Option<String>,
    /// `command`.
    pub command: Option<String>,
    /// `pattern`.
    pub pattern: Option<String>,
    /// `url`.
    pub url: Option<String>,
    /// `query`.
    pub query: Option<String>,
    /// `prompt`.
    pub prompt: Option<String>,
    /// `args`.
    pub args: Vec<String>,
}

/// Grouped tool-use row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionGroupedToolUse {
    /// UUID.
    pub uuid: String,
    /// Grouped tool name.
    pub tool_name: String,
    /// First tool input.
    pub first_tool_input: Option<MessageActionToolInput>,
    /// Tool result texts.
    pub results: Vec<String>,
}

/// Collapsed read/search row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionCollapsedReadSearch {
    /// UUID.
    pub uuid: String,
    /// Tool-result texts collected from nested rows.
    pub result_texts: Vec<String>,
}

/// System row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionSystemMessage {
    /// UUID.
    pub uuid: String,
    /// Subtype.
    pub subtype: String,
    /// Optional content string.
    pub content: Option<String>,
    /// Optional error string.
    pub error: Option<String>,
}

/// Attachment row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionAttachment {
    /// UUID.
    pub uuid: String,
    /// Attachment type.
    pub kind: String,
    /// Queued-command prompt text when applicable.
    pub queued_command_text: Option<String>,
}

/// Cursor state for message-actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionState {
    /// Selected row UUID.
    pub uuid: String,
    /// Message type.
    pub msg_type: String,
    /// Expanded flag.
    pub expanded: bool,
    /// Optional tool name.
    pub tool_name: Option<String>,
}

/// One button shown in the footer bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionButtonDisplay {
    /// Keybinding label.
    pub key: &'static str,
    /// Action label.
    pub label: String,
}

/// Footer bar display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageActionBarDisplay {
    /// Applicable action buttons.
    pub actions: Vec<MessageActionButtonDisplay>,
    /// Navigation hint.
    pub navigation_hint: &'static str,
}

/// Pure action result for a single keypress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageActionDispatch {
    /// No state change or action.
    Noop,
    /// Toggle `expanded`.
    ToggleExpanded {
        /// New expanded state.
        expanded: bool,
    },
    /// Close the cursor entirely.
    Close,
    /// Edit the selected user message.
    EditUser,
    /// Copy arbitrary text.
    CopyText(String),
}

/// True when the row can be selected in the message list: assistant rows
/// carrying real text (non-empty and not a known synthetic placeholder) or a
/// tool use whose primary input has a label; non-meta, non-compact user rows
/// whose wrapper-stripped text does not start with `<`; system rows outside
/// `api_metrics`, `stop_hook_summary`, `turn_duration`, `memory_saved`,
/// `agents_killed`, `away_summary` and `thinking`; every grouped tool-use and
/// collapsed read/search row; and `queued_command`, `diagnostics`,
/// `hook_blocking_error` and `hook_error_during_execution` attachments.
pub fn is_navigable_message(message: &MessageActionMessage) -> bool {
    match message {
        MessageActionMessage::Assistant(msg) => match &msg.first_block {
            MessageActionAssistantBlock::Text(text) => {
                !is_empty_assistant_message_text(text)
                    && !SYNTHETIC_MESSAGES.contains(&text.as_str())
            }
            MessageActionAssistantBlock::ToolUse { name, .. } => {
                primary_input_label(name).is_some()
            }
            MessageActionAssistantBlock::Other => false,
        },
        MessageActionMessage::User(msg) => {
            if msg.is_meta || msg.is_compact_summary {
                return false;
            }
            let Some(text) = &msg.text else {
                return false;
            };
            if SYNTHETIC_MESSAGES.contains(&text.as_str()) {
                return false;
            }
            !strip_injected_wrappers(text).starts_with('<')
        }
        MessageActionMessage::System(msg) => !matches!(
            msg.subtype.as_str(),
            "api_metrics"
                | "stop_hook_summary"
                | "turn_duration"
                | "memory_saved"
                | "agents_killed"
                | "away_summary"
                | "thinking"
        ),
        MessageActionMessage::GroupedToolUse(_) | MessageActionMessage::CollapsedReadSearch(_) => {
            true
        }
        MessageActionMessage::Attachment(msg) => matches!(
            msg.kind.as_str(),
            "queued_command"
                | "diagnostics"
                | "hook_blocking_error"
                | "hook_error_during_execution"
        ),
    }
}

/// The tool name and input a row acts on: an assistant row's `tool_use`
/// block, or a grouped tool-use row's tool name and first input. `None` for
/// every other row.
pub fn tool_call_of(message: &MessageActionMessage) -> Option<(String, MessageActionToolInput)> {
    match message {
        MessageActionMessage::Assistant(msg) => match &msg.first_block {
            MessageActionAssistantBlock::ToolUse { name, input } => {
                Some((name.clone(), input.clone()))
            }
            _ => None,
        },
        MessageActionMessage::GroupedToolUse(msg) => msg
            .first_tool_input
            .clone()
            .map(|input| (msg.tool_name.clone(), input)),
        _ => None,
    }
}

/// Buttons offered for the selected row: enter to expand or collapse on
/// grouped, collapsed, attachment and system rows, enter to edit on user
/// rows, `c` to copy on every row type, and `p` to copy the primary input on
/// assistant and grouped rows whose tool name maps to a labelled field.
pub fn available_message_actions(state: &MessageActionState) -> Vec<MessageActionButtonDisplay> {
    let mut actions = Vec::new();

    if matches!(
        state.msg_type.as_str(),
        "grouped_tool_use" | "collapsed_read_search" | "attachment" | "system"
    ) {
        actions.push(MessageActionButtonDisplay {
            key: "enter",
            label: if state.expanded { "collapse" } else { "expand" }.into(),
        });
    }
    if state.msg_type == "user" {
        actions.push(MessageActionButtonDisplay {
            key: "enter",
            label: "edit".into(),
        });
    }
    if matches!(
        state.msg_type.as_str(),
        "user"
            | "assistant"
            | "grouped_tool_use"
            | "collapsed_read_search"
            | "system"
            | "attachment"
    ) {
        actions.push(MessageActionButtonDisplay {
            key: "c",
            label: "copy".into(),
        });
    }
    if matches!(state.msg_type.as_str(), "grouped_tool_use" | "assistant") {
        if let Some(tool_name) = &state.tool_name {
            if let Some(label) = primary_input_label(tool_name) {
                actions.push(MessageActionButtonDisplay {
                    key: "p",
                    label: format!("copy {label}"),
                });
            }
        }
    }

    actions
}

/// The footer bar for the selected row: its action buttons plus the
/// `↑↓ navigate · esc back` hint.
pub fn project_message_actions_bar(state: &MessageActionState) -> MessageActionBarDisplay {
    MessageActionBarDisplay {
        actions: available_message_actions(state),
        navigation_hint: "↑↓ navigate · esc back",
    }
}

/// Turns one key into an action: `escape` collapses an expanded row or
/// closes the cursor, `ctrlc` closes it, `enter` toggles expansion on
/// grouped, collapsed, attachment and system rows and edits on user rows, `c`
/// copies the row's copy text, and `p` copies the row's primary tool input.
/// Every other key is a no-op.
pub fn dispatch_message_action_key(
    state: &MessageActionState,
    message: Option<&MessageActionMessage>,
    key: &str,
) -> MessageActionDispatch {
    match key {
        "escape" => {
            if state.expanded {
                MessageActionDispatch::ToggleExpanded { expanded: false }
            } else {
                MessageActionDispatch::Close
            }
        }
        "ctrlc" => MessageActionDispatch::Close,
        "enter" => {
            if matches!(
                state.msg_type.as_str(),
                "grouped_tool_use" | "collapsed_read_search" | "attachment" | "system"
            ) {
                return MessageActionDispatch::ToggleExpanded {
                    expanded: !state.expanded,
                };
            }
            if state.msg_type == "user" {
                return MessageActionDispatch::EditUser;
            }
            MessageActionDispatch::Noop
        }
        "c" => message
            .map(copy_text_of)
            .map(MessageActionDispatch::CopyText)
            .unwrap_or(MessageActionDispatch::Noop),
        "p" => {
            let Some(message) = message else {
                return MessageActionDispatch::Noop;
            };
            let Some((tool_name, tool_input)) = tool_call_of(message) else {
                return MessageActionDispatch::Noop;
            };
            extract_primary_input(&tool_name, &tool_input)
                .map(MessageActionDispatch::CopyText)
                .unwrap_or(MessageActionDispatch::Noop)
        }
        _ => MessageActionDispatch::Noop,
    }
}

/// Take the harness's injected wrappers back out of what a person typed.
///
/// Three wrappers are the harness's own: the `<system-reminder>` bodies the
/// engine attaches, the `<runtime_context>` it refreshes, and the
/// `<additional_context>` a `UserPromptSubmit` hook appends. All three are in
/// the durable user row because the model saw them, and no surface should
/// show them as the user's words.
///
/// Anywhere in the text, not only at the front. An earlier cut stripped only
/// leading wrappers, which left a reminder attached mid-prompt on screen; the
/// desktop app carried its own stripper partly for that reason, and the two
/// had drifted.
///
/// Three passes, and the middle one is deliberately narrow:
///
/// 1. Balanced `<tag>…</tag>` pairs go, wherever they sit.
/// 2. An **unclosed** `<system-reminder>` or `<runtime_context>` truncates
///    from there: the engine writes those last, so what follows is theirs.
///    `<additional_context>` is *not* truncated — a hook's block can be
///    anywhere, and cutting on a guess could eat text a person really typed.
/// 3. A stray closing tag is dropped, which is what a block whose opener
///    lived in an earlier row leaves behind.
pub fn strip_injected_wrappers(text: &str) -> String {
    const PAIRS: [(&str, &str); 3] = [
        ("<system-reminder>", "</system-reminder>"),
        ("<runtime_context>", "</runtime_context>"),
        ("<additional_context>", "</additional_context>"),
    ];
    let mut out = text.to_string();
    for (open, close) in PAIRS {
        while let (Some(a), Some(b)) = (out.find(open), out.find(close)) {
            if a > b {
                break;
            }
            out.replace_range(a..b + close.len(), "");
        }
    }
    for open in ["<system-reminder>", "<runtime_context>"] {
        if let Some(at) = out.find(open) {
            out.truncate(at);
        }
    }
    for (_, close) in PAIRS {
        out = out.replace(close, "");
    }
    // Only what a removal left behind is trimmed. A prompt nothing was taken
    // out of comes back byte for byte, indentation included — a person can
    // paste an indented block and it is not the harness's whitespace.
    if out.len() == text.len() {
        return out;
    }
    out.trim().to_string()
}

/// The text `c` copies for a row: wrapper-stripped user text, the assistant
/// text block or extracted primary input, the non-empty result texts of a
/// grouped or collapsed row joined by blank lines, a system row's content or
/// error or subtype, and for attachments the queued command text or a
/// bracketed kind name.
pub fn copy_text_of(message: &MessageActionMessage) -> String {
    match message {
        MessageActionMessage::User(msg) => msg
            .text
            .as_deref()
            .map(strip_injected_wrappers)
            .unwrap_or_default(),
        MessageActionMessage::Assistant(msg) => match &msg.first_block {
            MessageActionAssistantBlock::Text(text) => text.clone(),
            MessageActionAssistantBlock::ToolUse { name, input } => {
                extract_primary_input(name, input).unwrap_or_default()
            }
            MessageActionAssistantBlock::Other => String::new(),
        },
        MessageActionMessage::GroupedToolUse(msg) => msg
            .results
            .iter()
            .filter(|text| !text.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n"),
        MessageActionMessage::CollapsedReadSearch(msg) => msg
            .result_texts
            .iter()
            .filter(|text| !text.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n"),
        MessageActionMessage::System(msg) => msg
            .content
            .clone()
            .or_else(|| msg.error.clone())
            .unwrap_or_else(|| msg.subtype.clone()),
        MessageActionMessage::Attachment(msg) => {
            if msg.kind == "queued_command" {
                msg.queued_command_text.clone().unwrap_or_default()
            } else {
                format!("[{}]", msg.kind)
            }
        }
    }
}

/// Which builtin tools have a shown input, and what it is called, is a fact
/// about the tool rather than about this projection, so it is read from
/// `rebon-tools-core` rather than listed again here. A second copy of the list
/// here is what it replaced, and that copy had already drifted.
///
/// `Tmux` is the one name still spelled out: it has no row in the builtin
/// facts table, and its shown value is assembled from `args` rather than read
/// out of a single field.
const TMUX: &str = "Tmux";

fn shown_input(tool_name: &str) -> Option<PrimaryInput> {
    builtin_tool_facts_for_name(tool_name).and_then(|facts| facts.primary_input)
}

fn primary_input_label(tool_name: &str) -> Option<&'static str> {
    if tool_name == TMUX {
        return Some("command");
    }
    shown_input(tool_name).map(|shown| shown.label)
}

fn extract_primary_input(tool_name: &str, input: &MessageActionToolInput) -> Option<String> {
    if tool_name == TMUX {
        return (!input.args.is_empty()).then(|| format!("tmux {}", input.args.join(" ")));
    }
    // Matching the field rather than the tool: which field a tool shows is the
    // table's business, and where that field lives on this struct is ours.
    match shown_input(tool_name)?.field {
        "file_path" => input.file_path.clone().map(preserve_path_prefix),
        "notebook_path" => input.notebook_path.clone().map(preserve_path_prefix),
        "command" => input.command.clone(),
        "pattern" => input.pattern.clone(),
        "url" => input.url.clone(),
        "query" => input.query.clone(),
        "prompt" => input.prompt.clone(),
        _ => None,
    }
}

fn preserve_path_prefix(path: String) -> String {
    if path.starts_with('…') || path.starts_with("...") {
        return path;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_path_prefix_leaves_full_paths_unchanged() {
        let path = "C:\\projects\\example\\crates\\example\\src\\lib.rs";
        assert_eq!(preserve_path_prefix(path.into()), path);
    }

    #[test]
    fn strip_injected_wrappers_takes_the_harness_text_out_wherever_it_sits() {
        let leading =
            "<system-reminder>one</system-reminder>\n<system-reminder>two</system-reminder>\nhello";
        assert_eq!(strip_injected_wrappers(leading), "hello");
        assert_eq!(strip_injected_wrappers("plain"), "plain");
        // Untouched text keeps its own whitespace.
        assert_eq!(strip_injected_wrappers("  indented"), "  indented");

        // Mid-prompt, which the leading-only cut used to leave on screen.
        assert_eq!(
            strip_injected_wrappers("do it <system-reminder>psst</system-reminder> now"),
            "do it  now"
        );
        // Runtime context is the harness's too.
        assert_eq!(
            strip_injected_wrappers("<runtime_context>cwd</runtime_context>ask"),
            "ask"
        );
        // An unclosed reminder is the engine writing last; a stray closer is
        // the tail of a block whose opener was in an earlier row.
        assert_eq!(
            strip_injected_wrappers("real words\n<system-reminder>cut from here"),
            "real words"
        );
        assert_eq!(strip_injected_wrappers("tail</system-reminder>"), "tail");
    }

    #[test]
    fn strip_injected_wrappers_removes_appended_hook_blocks() {
        let text = "do the thing\n\n<additional_context>\nctx one\n</additional_context>\n\n<additional_context>\nctx two\n</additional_context>";
        assert_eq!(strip_injected_wrappers(text), "do the thing");
    }

    #[test]
    fn strip_injected_wrappers_leaves_plain_text_and_an_unclosed_hook_block() {
        assert_eq!(strip_injected_wrappers("just a prompt"), "just a prompt");
        let unclosed = "mentioning <additional_context> mid-sentence";
        assert_eq!(strip_injected_wrappers(unclosed), unclosed);
    }

    #[test]
    fn user_and_assistant_navigability_filters() {
        assert!(!is_navigable_message(&MessageActionMessage::User(
            MessageActionUserMessage {
                uuid: "u1".into(),
                is_meta: true,
                is_compact_summary: false,
                text: Some("hello".into()),
            }
        )));
        assert!(!is_navigable_message(&MessageActionMessage::User(
            MessageActionUserMessage {
                uuid: "u1".into(),
                is_meta: false,
                is_compact_summary: false,
                text: Some("[Request interrupted by user]".into()),
            }
        )));
        assert!(!is_navigable_message(&MessageActionMessage::User(
            MessageActionUserMessage {
                uuid: "u1".into(),
                is_meta: false,
                is_compact_summary: false,
                text: Some(
                    "<system-reminder>x</system-reminder><bash-input>ls</bash-input>".into()
                ),
            }
        )));
        assert!(is_navigable_message(&MessageActionMessage::User(
            MessageActionUserMessage {
                uuid: "u1".into(),
                is_meta: false,
                is_compact_summary: false,
                text: Some("real prompt".into()),
            }
        )));

        assert!(is_navigable_message(&MessageActionMessage::Assistant(
            MessageActionAssistantMessage {
                uuid: "a1".into(),
                first_block: MessageActionAssistantBlock::ToolUse {
                    name: "Read".into(),
                    input: MessageActionToolInput {
                        file_path: Some("src/lib.rs".into()),
                        ..Default::default()
                    },
                },
            },
        )));
    }

    #[test]
    fn system_and_attachment_navigability_follow_blocklist() {
        assert!(!is_navigable_message(&MessageActionMessage::System(
            MessageActionSystemMessage {
                uuid: "s1".into(),
                subtype: "turn_duration".into(),
                content: None,
                error: None,
            },
        )));
        assert!(is_navigable_message(&MessageActionMessage::System(
            MessageActionSystemMessage {
                uuid: "s1".into(),
                subtype: "api_error".into(),
                content: Some("oops".into()),
                error: None,
            },
        )));
        assert!(is_navigable_message(&MessageActionMessage::Attachment(
            MessageActionAttachment {
                uuid: "t1".into(),
                kind: "queued_command".into(),
                queued_command_text: Some("hello".into()),
            },
        )));
        assert!(!is_navigable_message(&MessageActionMessage::Attachment(
            MessageActionAttachment {
                uuid: "t1".into(),
                kind: "dynamic_skill".into(),
                queued_command_text: None,
            },
        )));
    }

    #[test]
    fn tool_call_and_primary_input_extraction_work_for_assistant_and_grouped_rows() {
        let assistant = MessageActionMessage::Assistant(MessageActionAssistantMessage {
            uuid: "a1".into(),
            first_block: MessageActionAssistantBlock::ToolUse {
                name: "Tmux".into(),
                input: MessageActionToolInput {
                    args: vec!["new".into(), "-s".into(), "x".into()],
                    ..Default::default()
                },
            },
        });
        assert_eq!(
            tool_call_of(&assistant),
            Some((
                "Tmux".into(),
                MessageActionToolInput {
                    args: vec!["new".into(), "-s".into(), "x".into()],
                    ..Default::default()
                },
            ))
        );
        assert_eq!(copy_text_of(&assistant), "tmux new -s x");

        let grouped = MessageActionMessage::GroupedToolUse(MessageActionGroupedToolUse {
            uuid: "g1".into(),
            tool_name: "Read".into(),
            first_tool_input: Some(MessageActionToolInput {
                file_path: Some("src/lib.rs".into()),
                ..Default::default()
            }),
            results: vec!["one".into(), "two".into()],
        });
        assert_eq!(copy_text_of(&grouped), "one\n\ntwo");
    }

    #[test]
    fn available_actions_match_message_type_and_tool_name() {
        let user_actions = available_message_actions(&MessageActionState {
            uuid: "u1".into(),
            msg_type: "user".into(),
            expanded: false,
            tool_name: None,
        });
        assert_eq!(
            user_actions,
            vec![
                MessageActionButtonDisplay {
                    key: "enter",
                    label: "edit".into(),
                },
                MessageActionButtonDisplay {
                    key: "c",
                    label: "copy".into(),
                },
            ]
        );

        let tool_actions = available_message_actions(&MessageActionState {
            uuid: "a1".into(),
            msg_type: "assistant".into(),
            expanded: false,
            tool_name: Some("Read".into()),
        });
        assert_eq!(
            tool_actions,
            vec![
                MessageActionButtonDisplay {
                    key: "c",
                    label: "copy".into(),
                },
                MessageActionButtonDisplay {
                    key: "p",
                    label: "copy path".into(),
                },
            ]
        );
    }

    #[test]
    fn bar_projection_threads_action_labels_and_navigation_hint() {
        let bar = project_message_actions_bar(&MessageActionState {
            uuid: "g1".into(),
            msg_type: "grouped_tool_use".into(),
            expanded: true,
            tool_name: Some("Bash".into()),
        });
        assert_eq!(
            bar.actions,
            vec![
                MessageActionButtonDisplay {
                    key: "enter",
                    label: "collapse".into(),
                },
                MessageActionButtonDisplay {
                    key: "c",
                    label: "copy".into(),
                },
                MessageActionButtonDisplay {
                    key: "p",
                    label: "copy command".into(),
                },
            ]
        );
        assert_eq!(bar.navigation_hint, "↑↓ navigate · esc back");
    }

    #[test]
    fn dispatch_key_mirrors_toggle_close_copy_and_edit_semantics() {
        let state = MessageActionState {
            uuid: "g1".into(),
            msg_type: "grouped_tool_use".into(),
            expanded: false,
            tool_name: Some("Read".into()),
        };
        assert_eq!(
            dispatch_message_action_key(&state, None, "enter"),
            MessageActionDispatch::ToggleExpanded { expanded: true }
        );
        assert_eq!(
            dispatch_message_action_key(
                &MessageActionState {
                    expanded: true,
                    ..state.clone()
                },
                None,
                "escape"
            ),
            MessageActionDispatch::ToggleExpanded { expanded: false }
        );
        assert_eq!(
            dispatch_message_action_key(&state, None, "ctrlc"),
            MessageActionDispatch::Close
        );

        let user_state = MessageActionState {
            uuid: "u1".into(),
            msg_type: "user".into(),
            expanded: false,
            tool_name: None,
        };
        assert_eq!(
            dispatch_message_action_key(&user_state, None, "enter"),
            MessageActionDispatch::EditUser
        );

        let assistant = MessageActionMessage::Assistant(MessageActionAssistantMessage {
            uuid: "a1".into(),
            first_block: MessageActionAssistantBlock::ToolUse {
                name: "Read".into(),
                input: MessageActionToolInput {
                    file_path: Some("src/lib.rs".into()),
                    ..Default::default()
                },
            },
        });
        assert_eq!(
            dispatch_message_action_key(&state, Some(&assistant), "p"),
            MessageActionDispatch::CopyText("src/lib.rs".into())
        );
        assert_eq!(
            dispatch_message_action_key(&state, Some(&assistant), "c"),
            MessageActionDispatch::CopyText("src/lib.rs".into())
        );
    }

    #[test]
    fn copy_text_of_handles_collapsed_system_and_attachment_rows() {
        assert_eq!(
            copy_text_of(&MessageActionMessage::CollapsedReadSearch(
                MessageActionCollapsedReadSearch {
                    uuid: "c1".into(),
                    result_texts: vec!["a".into(), String::new(), "b".into()],
                }
            )),
            "a\n\nb"
        );
        assert_eq!(
            copy_text_of(&MessageActionMessage::System(MessageActionSystemMessage {
                uuid: "s1".into(),
                subtype: "thinking".into(),
                content: None,
                error: Some("oops".into()),
            })),
            "oops"
        );
        assert_eq!(
            copy_text_of(&MessageActionMessage::Attachment(MessageActionAttachment {
                uuid: "a1".into(),
                kind: "dynamic_skill".into(),
                queued_command_text: None,
            })),
            "[dynamic_skill]"
        );
    }
}
