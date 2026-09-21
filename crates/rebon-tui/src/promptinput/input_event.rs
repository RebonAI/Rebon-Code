//! Decision order for one raw key event in the prompt input.
//!
//! This module models the ordering of guards and early returns. The caller still
//! owns the actual state mutations, prompt insertion, notifications, and
//! message-selector double-press handling.

use crate::promptinput::utils::clamp_cursor_offset;

/// Minimal option-meta hint payload for macOS Option-key failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionMetaHint {
    /// Shortcut name like `alt+p`.
    pub shortcut: String,
    /// Optional terminal display name.
    pub terminal_display_name: Option<String>,
}

/// Main effect selected by one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEventPrimaryAction {
    /// Event was ignored by an early guard.
    None,
    /// Type a printable char while a footer pill is selected.
    TypeToExitFooter {
        /// Full next input string after splicing the char.
        next_input: String,
        /// Cursor offset after insertion.
        next_cursor_offset: usize,
    },
    /// Abort active speculation.
    AbortSpeculation,
    /// Dismiss visible side question.
    DismissSideQuestion,
    /// Let footer keybindings own Escape while a pill is selected.
    DeferToFooterSelection,
    /// Pop queued command into the prompt (used by Up-arrow editing).
    PopQueuedCommand,
    /// Flush all queued commands by auto-submitting the queue.
    FlushQueue,
    /// Trigger the empty-input ESC double-press handler.
    TriggerDoublePressEscFromEmpty,
}

/// Pure result of one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEventPlan {
    /// Optional Option-as-Meta hint side effect.
    pub option_meta_hint: Option<OptionMetaHint>,
    /// Whether prompt mode should be reset to `prompt`.
    pub reset_prompt_mode: bool,
    /// Whether help should be closed.
    pub close_help: bool,
    /// Main early-return action.
    pub primary_action: InputEventPrimaryAction,
}

impl Default for InputEventPlan {
    fn default() -> Self {
        Self {
            option_meta_hint: None,
            reset_prompt_mode: false,
            close_help: false,
            primary_action: InputEventPrimaryAction::None,
        }
    }
}

/// Inputs the key-event branch order reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEventInput {
    /// Inserted character from the terminal input handler.
    pub ch: String,
    /// Current full input string.
    pub current_input: String,
    /// Current cursor offset.
    pub cursor_offset: usize,
    /// Whether a full-screen dialog is open.
    pub full_screen_dialog_open: bool,
    /// Whether the platform is macOS.
    pub is_macos: bool,
    /// Optional Option-key shortcut mapping for the typed char.
    pub option_shortcut: Option<String>,
    /// Optional display name of the current terminal.
    pub terminal_display_name: Option<String>,
    /// Whether a footer item is selected.
    pub footer_item_selected: bool,
    /// Ctrl is held.
    pub ctrl: bool,
    /// Meta / Alt is held.
    pub meta: bool,
    /// Escape key.
    pub escape: bool,
    /// Return / Enter key.
    pub return_key: bool,
    /// Backspace key.
    pub backspace: bool,
    /// Delete key.
    pub delete: bool,
    /// Whether help is open.
    pub help_open: bool,
    /// Whether speculation is active.
    pub speculation_active: bool,
    /// Whether side question UI is visible.
    pub side_question_visible: bool,
    /// Whether there is an editable queued command.
    pub has_editable_queued_command: bool,
    /// Whether transcript already has messages.
    pub has_messages: bool,
    /// Whether the input string is empty.
    pub input_is_empty: bool,
    /// Whether the assistant is currently loading.
    pub is_loading: bool,
}

/// Plan one key event: the Option-as-Meta hint, typing out of a selected
/// footer pill, mode/help resets, then the Escape chain (speculation, side
/// question, help, footer, queued commands, empty-input double press).
pub fn plan_input_event(input: &InputEventInput) -> InputEventPlan {
    if input.full_screen_dialog_open {
        return InputEventPlan::default();
    }

    let mut plan = InputEventPlan::default();
    if input.is_macos {
        if let Some(shortcut) = &input.option_shortcut {
            plan.option_meta_hint = Some(OptionMetaHint {
                shortcut: shortcut.clone(),
                terminal_display_name: input.terminal_display_name.clone(),
            });
        }
    }

    if input.footer_item_selected
        && !input.ch.is_empty()
        && !input.ctrl
        && !input.meta
        && !input.escape
        && !input.return_key
    {
        let (next_input, next_cursor_offset) =
            splice_char(&input.current_input, input.cursor_offset, &input.ch);
        return InputEventPlan {
            primary_action: InputEventPrimaryAction::TypeToExitFooter {
                next_input,
                next_cursor_offset,
            },
            ..plan
        };
    }

    if input.cursor_offset == 0
        && (input.escape || input.backspace || input.delete || (input.ctrl && input.ch == "u"))
    {
        plan.reset_prompt_mode = true;
        plan.close_help = true;
    }

    if input.help_open && input.input_is_empty && (input.backspace || input.delete) {
        plan.close_help = true;
    }

    if input.escape {
        if input.speculation_active {
            plan.primary_action = InputEventPrimaryAction::AbortSpeculation;
            return plan;
        }
        if input.side_question_visible {
            plan.primary_action = InputEventPrimaryAction::DismissSideQuestion;
            return plan;
        }
        if input.help_open {
            plan.close_help = true;
            return plan;
        }
        if input.footer_item_selected {
            plan.primary_action = InputEventPrimaryAction::DeferToFooterSelection;
            return plan;
        }
        if input.has_editable_queued_command {
            plan.primary_action = InputEventPrimaryAction::FlushQueue;
            return plan;
        }
        if input.has_messages && input.input_is_empty && !input.is_loading {
            plan.primary_action = InputEventPrimaryAction::TriggerDoublePressEscFromEmpty;
            return plan;
        }
    }

    if input.return_key && input.help_open {
        plan.close_help = true;
    }

    plan
}

fn splice_char(current_input: &str, cursor_offset: usize, ch: &str) -> (String, usize) {
    let cursor_offset = clamp_cursor_offset(current_input, cursor_offset);
    (
        format!(
            "{}{}{}",
            &current_input[..cursor_offset],
            ch,
            &current_input[cursor_offset..]
        ),
        cursor_offset + ch.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> InputEventInput {
        InputEventInput {
            ch: String::new(),
            current_input: String::from("abc"),
            cursor_offset: 1,
            full_screen_dialog_open: false,
            is_macos: false,
            option_shortcut: None,
            terminal_display_name: None,
            footer_item_selected: false,
            ctrl: false,
            meta: false,
            escape: false,
            return_key: false,
            backspace: false,
            delete: false,
            help_open: false,
            speculation_active: false,
            side_question_visible: false,
            has_editable_queued_command: false,
            has_messages: false,
            input_is_empty: false,
            is_loading: false,
        }
    }

    #[test]
    fn dialog_guard_short_circuits_everything() {
        let mut input = base();
        input.full_screen_dialog_open = true;
        input.escape = true;
        assert_eq!(plan_input_event(&input), InputEventPlan::default());
    }

    #[test]
    fn macos_option_char_emits_hint_but_can_continue() {
        let mut input = base();
        input.is_macos = true;
        input.option_shortcut = Some(String::from("alt+p"));
        input.terminal_display_name = Some(String::from("Ghostty"));
        let plan = plan_input_event(&input);
        assert_eq!(
            plan.option_meta_hint,
            Some(OptionMetaHint {
                shortcut: String::from("alt+p"),
                terminal_display_name: Some(String::from("Ghostty")),
            })
        );
        assert_eq!(plan.primary_action, InputEventPrimaryAction::None);
    }

    #[test]
    fn printable_char_while_footer_selected_types_to_exit_footer() {
        let mut input = base();
        input.footer_item_selected = true;
        input.ch = String::from("x");
        let plan = plan_input_event(&input);
        assert_eq!(
            plan.primary_action,
            InputEventPrimaryAction::TypeToExitFooter {
                next_input: String::from("axbc"),
                next_cursor_offset: 2,
            }
        );
    }

    #[test]
    fn printable_char_while_footer_selected_clamps_cursor_past_end() {
        let mut input = base();
        input.footer_item_selected = true;
        input.ch = String::from("x");
        input.cursor_offset = usize::MAX;

        assert_eq!(
            plan_input_event(&input).primary_action,
            InputEventPrimaryAction::TypeToExitFooter {
                next_input: String::from("abcx"),
                next_cursor_offset: 4,
            }
        );
    }

    #[test]
    fn printable_char_while_footer_selected_snaps_to_utf8_boundary() {
        let mut input = base();
        input.current_input = String::from("你a");
        input.footer_item_selected = true;
        input.ch = String::from("x");
        input.cursor_offset = 2;

        assert_eq!(
            plan_input_event(&input).primary_action,
            InputEventPrimaryAction::TypeToExitFooter {
                next_input: String::from("x你a"),
                next_cursor_offset: 1,
            }
        );
    }

    #[test]
    fn cursor_zero_escape_like_keys_reset_prompt_mode_and_help() {
        let mut input = base();
        input.cursor_offset = 0;
        input.backspace = true;
        input.help_open = true;
        let plan = plan_input_event(&input);
        assert!(plan.reset_prompt_mode);
        assert!(plan.close_help);
    }

    #[test]
    fn escape_priority_order() {
        let mut speculation = base();
        speculation.escape = true;
        speculation.speculation_active = true;
        assert_eq!(
            plan_input_event(&speculation).primary_action,
            InputEventPrimaryAction::AbortSpeculation
        );

        let mut side = base();
        side.escape = true;
        side.side_question_visible = true;
        assert_eq!(
            plan_input_event(&side).primary_action,
            InputEventPrimaryAction::DismissSideQuestion
        );

        let mut footer = base();
        footer.escape = true;
        footer.footer_item_selected = true;
        assert_eq!(
            plan_input_event(&footer).primary_action,
            InputEventPrimaryAction::DeferToFooterSelection
        );

        let mut queued = base();
        queued.escape = true;
        queued.has_editable_queued_command = true;
        assert_eq!(
            plan_input_event(&queued).primary_action,
            InputEventPrimaryAction::FlushQueue
        );
    }

    #[test]
    fn escape_from_empty_idle_prompt_triggers_message_selector_double_press() {
        let mut input = base();
        input.escape = true;
        input.has_messages = true;
        input.input_is_empty = true;
        let plan = plan_input_event(&input);
        assert_eq!(
            plan.primary_action,
            InputEventPrimaryAction::TriggerDoublePressEscFromEmpty
        );
    }

    #[test]
    fn return_closes_help_without_other_side_effects() {
        let mut input = base();
        input.return_key = true;
        input.help_open = true;
        let plan = plan_input_event(&input);
        assert!(plan.close_help);
        assert_eq!(plan.primary_action, InputEventPrimaryAction::None);
    }
}
