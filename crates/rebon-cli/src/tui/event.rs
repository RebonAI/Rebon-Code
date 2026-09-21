//! Translate a crossterm [`KeyEvent`] into pure-data inputs that
//! `rebon_tui::promptinput` can plan against.
//!
//! Two decision trees handle each event: the gesture decision tree
//! (`plan_input_event`) and the text-buffer decision tree
//! (`plan_input_change`). Each crossterm key event fans out to at
//! most both of those paths, plus a small set of local gestures
//! (cursor movement, exit) that the pure state machines do not own
//! because they are render-surface concerns.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rebon_tui::input::normalize_full_width_digit;
use rebon_tui::promptinput::{clamp_cursor_offset, InputChangeInput, InputEventInput};
use rebon_width::{WidthChar, WidthStr};

use crate::tui::app::AppState;

/// The decoded meaning of a single crossterm key event.
#[derive(Debug, Clone)]
pub enum KeyAction {
    /// The event has no meaning for this translator (modifier-only,
    /// Release, unsupported key, etc.).
    Ignored,
    /// Interrupt in-flight work without exiting the session.
    ///
    /// Used for `Esc`: close transient pickers first (handled before
    /// translation in the runner), otherwise abort the active turn /
    /// background tasks, or clear the prompt as a last resort.
    Interrupt,
    /// Cancel the current turn if one is in flight; otherwise clear
    /// input or exit depending on runner state.
    CancelOrExit,
    /// Restore the most recent input snapshot from undo history.
    Undo,
    /// Move the cursor left by one grapheme.
    CursorLeft,
    /// Move the cursor right by one grapheme.
    CursorRight,
    /// Move the cursor to column 0 of the current logical line.
    CursorHome,
    /// Move the cursor to the end of the current logical line.
    CursorEnd,
    /// Scroll transcript up by one message row. Emitted by the
    /// permission modal's vim-style `k` binding; a wheel-up gesture
    /// scrolls the transcript directly in
    /// `runner/layout_and_scroll.rs`.
    ScrollUp,
    /// Scroll transcript down by one message row. Emitted by the
    /// runner when it reroutes a bare `PromptDown` (keyboard arrow
    /// or DEC-1007 wheel gesture) onto the transcript scroll engine —
    /// see `reroute_prompt_down_to_scroll` in
    /// `runner/event_loop_entry.rs`.
    ScrollDown,
    /// Up arrow in prompt: move cursor up or navigate history.
    PromptUp,
    /// Down arrow in prompt: move cursor down or navigate history.
    PromptDown,
    /// Scroll transcript up by one page.
    PageUp,
    /// Scroll transcript down by one page.
    PageDown,
    /// Jump transcript to the first row.
    ScrollHome,
    /// Jump transcript to the latest rows and resume follow-tail.
    ScrollEnd,
    /// A key that mutates prompt mode without changing the prompt buffer.
    SetPromptMode(String),
    /// A key that toggles the help footer without changing the prompt buffer.
    ToggleHelp,
    /// Switch to the previous visible help tab.
    HelpPreviousTab,
    /// Switch to the next visible help tab.
    HelpNextTab,
    /// A key that mutates the prompt buffer. The runner should
    /// call `plan_input_event` followed by `plan_input_change`,
    /// then apply both results via [`crate::tui::dispatch`].
    TextEdit(TextEdit),
    /// The user pressed Enter without Shift. Carries a snapshot of
    /// the current prompt buffer at the moment of submit so the
    /// runner can build the `PromptRequest` even if a later edit
    /// sneaks in before dispatch.
    Submit(String),
    /// Toggle tool output verbosity between Compact and Verbose.
    /// Bound to Ctrl+O.
    ToggleToolOutput,
    /// Toggle tool output verbosity to/from Verbose (show all).
    /// Bound to Ctrl+E.
    ShowAllOutput,
    /// Background all foreground tasks. Bound to Ctrl+B.
    BackgroundTasks,
    /// Cycle the permission mode via Shift+Tab:
    /// Default → Plan → Accept edits → Auto → Default.
    CyclePermissionMode,
    /// Toggle OpenAI fast service tier (Alt+O). The runner
    /// updates the shared service-tier handle so the next request sees it.
    ToggleFastMode,
    /// Paste an image from the OS clipboard (Alt+V / Option+V).
    /// Image-only: reads bitmap or image-path from clipboard and
    /// splices an `[Image #N]` chip into the prompt.
    PasteImageFromClipboard,
    /// Paste from the OS clipboard (Ctrl+V / Cmd+V). Tries image
    /// first; if no image, reads text and routes through the normal
    /// paste path. Gives instant paste on terminals that forward the
    /// key event instead of synthesizing `Event::Paste`.
    PasteFromClipboard,
    /// Up arrow while a footer pill is selected. Routed to
    /// [`rebon_tui::promptinput::footer_motion::resolve_footer_up`].
    FooterUp,
    /// Down arrow while a footer pill is selected. Routed to
    /// [`rebon_tui::promptinput::footer_motion::resolve_footer_down`].
    FooterDown,
    /// Right arrow while a footer pill is selected. Routed to
    /// [`rebon_tui::promptinput::footer_motion::resolve_footer_next`].
    FooterNext,
    /// Left arrow while a footer pill is selected. Routed to
    /// [`rebon_tui::promptinput::footer_motion::resolve_footer_previous`].
    FooterPrevious,
    /// Enter while a footer pill is selected. Routed to
    /// [`rebon_tui::promptinput::footer_actions::resolve_footer_open_selected_action`].
    FooterOpenSelected,
    /// Esc while a footer pill is selected. Routed to
    /// [`rebon_tui::promptinput::footer_motion::resolve_footer_clear_selection`].
    FooterClear,
}

/// Pair of pre-built inputs plus the cursor offset that should
/// result from the edit when `plan_input_change` returns the
/// default normalisation path.
#[derive(Debug, Clone)]
pub struct TextEdit {
    /// Inputs for `plan_input_event` — the gesture decision tree.
    pub event_input: InputEventInput,
    /// Inputs for `plan_input_change` — the buffer decision tree.
    pub change_input: InputChangeInput,
    /// Cursor offset to apply when the resulting
    /// `InputChangePlan::Apply { replace_input_with, next_cursor_offset }`
    /// does not force an explicit new cursor.
    pub intended_cursor: usize,
}

/// Translate one crossterm [`KeyEvent`] into a [`KeyAction`].
pub fn translate_key(key: &KeyEvent, app: &AppState) -> KeyAction {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return KeyAction::Ignored;
    }

    // Help owns left/right and tab while open for tab switching.
    if app.help_open {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => return KeyAction::HelpPreviousTab,
            KeyCode::Right | KeyCode::Tab => return KeyAction::HelpNextTab,
            _ => {}
        }
    }

    // Footer pill selected — arrow keys + Enter + Esc route through
    // the footer-motion / footer-action resolvers in
    // `rebon_tui::promptinput::footer_motion` / `footer_actions` instead
    // of cursor / history / prompt-submit. Ctrl+C still bypasses so
    // the user can always cancel/exit from a focused pill.
    if app.footer_selection.is_some()
        && !(matches!(key.code, KeyCode::Char('c'))
            && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        match key.code {
            KeyCode::Up => return KeyAction::FooterUp,
            KeyCode::Down => return KeyAction::FooterDown,
            KeyCode::Right => return KeyAction::FooterNext,
            KeyCode::Left => return KeyAction::FooterPrevious,
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                return KeyAction::FooterOpenSelected;
            }
            KeyCode::Esc => return KeyAction::FooterClear,
            _ => {}
        }
    }

    if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::CancelOrExit;
    }

    if matches!(key.code, KeyCode::Char('d')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::CancelOrExit;
    }

    if matches!(key.code, KeyCode::Char('z')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::Undo;
    }

    if matches!(key.code, KeyCode::Char('o')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::ToggleToolOutput;
    }

    if matches!(key.code, KeyCode::Char('e')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::ShowAllOutput;
    }

    if matches!(key.code, KeyCode::Char('b')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::BackgroundTasks;
    }

    // Alt+O toggles fast mode. Must be matched before the Char(c)
    // branch's generic ctrl/alt-ignore guard.
    if matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
        && key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return KeyAction::ToggleFastMode;
    }

    // Ctrl+V / Cmd+V: paste from clipboard (image first, then text).
    // Must be matched before the Char(c) branch's generic ctrl/alt-ignore
    // guard. On terminals that forward the key instead of synthesizing
    // Event::Paste, this gives instant clipboard paste.
    if matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return KeyAction::PasteFromClipboard;
    }

    // Alt+V / Option+V: paste image only from clipboard.
    if matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
        && key.modifiers.contains(KeyModifiers::ALT)
    {
        return KeyAction::PasteImageFromClipboard;
    }

    // Shift+Tab cycles permission mode. crossterm delivers this as
    // KeyCode::BackTab on most terminals.
    if matches!(key.code, KeyCode::BackTab)
        || (matches!(key.code, KeyCode::Tab) && key.modifiers.contains(KeyModifiers::SHIFT))
    {
        return KeyAction::CyclePermissionMode;
    }

    match key.code {
        KeyCode::Left => return KeyAction::CursorLeft,
        KeyCode::Right => return KeyAction::CursorRight,
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return KeyAction::ScrollHome;
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return KeyAction::ScrollEnd;
        }
        KeyCode::Home => return KeyAction::CursorHome,
        KeyCode::End => return KeyAction::CursorEnd,
        KeyCode::Up => return KeyAction::PromptUp,
        KeyCode::Down => return KeyAction::PromptDown,
        KeyCode::PageUp => return KeyAction::PageUp,
        KeyCode::PageDown => return KeyAction::PageDown,
        _ => {}
    }

    match key.code {
        KeyCode::Esc => KeyAction::Interrupt,
        KeyCode::Backspace => {
            // Chip-aware backspace: if the cursor is right after a
            // [Pasted text #N ...] or [Image #N] reference, delete
            // the entire chip in one press.
            let (next_value, intended_cursor) =
                if let Some((start, end)) = paste_ref_ending_at(&app.input, app.cursor_offset) {
                    let mut next = String::with_capacity(app.input.len() - (end - start));
                    next.push_str(&app.input[..start]);
                    next.push_str(&app.input[end..]);
                    (next, start)
                } else {
                    splice_backspace(&app.input, app.cursor_offset)
                };
            let event_input = build_event_input(app, String::new(), KeyFlags::backspace());
            let change_input = InputChangeInput {
                next_value,
                current_input: app.input.clone(),
                cursor_offset: app.cursor_offset,
            };
            KeyAction::TextEdit(TextEdit {
                event_input,
                change_input,
                intended_cursor,
            })
        }
        KeyCode::Delete => {
            let (next_value, intended_cursor) = splice_delete(&app.input, app.cursor_offset);
            let event_input = build_event_input(app, String::new(), KeyFlags::delete());
            let change_input = InputChangeInput {
                next_value,
                current_input: app.input.clone(),
                cursor_offset: app.cursor_offset,
            };
            KeyAction::TextEdit(TextEdit {
                event_input,
                change_input,
                intended_cursor,
            })
        }
        KeyCode::Enter => {
            // Universal "insert newline" bindings.
            //
            // 1. Backslash+Enter: if the char before the cursor is `\`,
            //    delete the backslash and insert `\n`. This is the
            //    terminal-agnostic fallback that works without any
            //    setup step (the runtime also flips a
            //    backslash-return-used flag here so the hint text
            //    shortens to `\⏎ for newline`).
            // 2. Shift+Enter works on terminals that report the
            //    modifier (Apple Terminal via native detection,
            //    kitty-protocol sessions, terminals configured to
            //    send CSI u).
            // 3. Alt+Enter / Ctrl+Enter: crossterm reports the
            //    `ESC + \r` sequence (which VSCode/Cursor/Windsurf
            //    keybindings.json remaps Shift+Enter to via
            //    `/terminal-setup`) as Alt+Enter. Ctrl+Enter is a
            //    defensive fallback for terminals that emit it.
            //
            // Plain Enter on most Windows terminals (conhost, Windows
            // Terminal, VS Code integrated terminal) arrives with NO
            // modifier set — that's the Submit path.
            let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
            let has_alt = key.modifiers.contains(KeyModifiers::ALT);
            let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

            let cursor = clamp_cursor_offset(&app.input, app.cursor_offset);
            let prev_is_backslash = cursor > 0 && app.input[..cursor].ends_with('\\');

            if prev_is_backslash && !has_alt && !has_ctrl && !has_shift {
                // Delete the trailing `\` then insert a newline.
                let backslash_start = cursor - 1;
                let mut next_value = String::with_capacity(app.input.len());
                next_value.push_str(&app.input[..backslash_start]);
                next_value.push('\n');
                next_value.push_str(&app.input[cursor..]);
                let intended_cursor = backslash_start + 1;
                let event_input =
                    build_event_input(app, String::from("\n"), KeyFlags::return_key());
                let change_input = InputChangeInput {
                    next_value,
                    current_input: app.input.clone(),
                    cursor_offset: app.cursor_offset,
                };
                return KeyAction::TextEdit(TextEdit {
                    event_input,
                    change_input,
                    intended_cursor,
                });
            }

            if has_shift || has_alt || has_ctrl {
                let (next_value, intended_cursor) =
                    splice_char_str(&app.input, app.cursor_offset, "\n");
                let event_input =
                    build_event_input(app, String::from("\n"), KeyFlags::return_key());
                let change_input = InputChangeInput {
                    next_value,
                    current_input: app.input.clone(),
                    cursor_offset: app.cursor_offset,
                };
                KeyAction::TextEdit(TextEdit {
                    event_input,
                    change_input,
                    intended_cursor,
                })
            } else {
                KeyAction::Submit(app.input.clone())
            }
        }
        KeyCode::Tab => {
            let (next_value, intended_cursor) =
                splice_char_str(&app.input, app.cursor_offset, "\t");
            let event_input = build_event_input(app, String::from("\t"), KeyFlags::none());
            let change_input = InputChangeInput {
                next_value,
                current_input: app.input.clone(),
                cursor_offset: app.cursor_offset,
            };
            KeyAction::TextEdit(TextEdit {
                event_input,
                change_input,
                intended_cursor,
            })
        }
        KeyCode::Char(c) => {
            let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let has_alt = key.modifiers.contains(KeyModifiers::ALT);
            // Ctrl+J is ASCII LF — the canonical "insert newline" key on
            // Unix-style terminals. Accept it as a newline-insertion
            // fallback for terminals that don't distinguish Shift+Enter
            // from Enter (Windows Terminal, conhost, VS Code integrated).
            if has_ctrl && !has_alt && (c == 'j' || c == 'J') {
                let (next_value, intended_cursor) =
                    splice_char_str(&app.input, app.cursor_offset, "\n");
                let event_input =
                    build_event_input(app, String::from("\n"), KeyFlags::return_key());
                let change_input = InputChangeInput {
                    next_value,
                    current_input: app.input.clone(),
                    cursor_offset: app.cursor_offset,
                };
                return KeyAction::TextEdit(TextEdit {
                    event_input,
                    change_input,
                    intended_cursor,
                });
            }
            if has_ctrl || has_alt {
                return KeyAction::Ignored;
            }
            if c == '?' && app.input.is_empty() && app.cursor_offset == 0 {
                return KeyAction::ToggleHelp;
            }
            if c == '!' && app.input.is_empty() && app.cursor_offset == 0 {
                return KeyAction::SetPromptMode(String::from("bash"));
            }
            // Normalize full-width digits
            // (CJK IME input) to ASCII before splicing into the buffer
            // (０-９ → 0-9, as expected for survey responses).
            let normalized = normalize_full_width_digit(c);
            let ch = String::from(normalized);
            let (next_value, intended_cursor) = splice_char_str(&app.input, app.cursor_offset, &ch);
            let event_input = build_event_input(app, ch, KeyFlags::none());
            let change_input = InputChangeInput {
                next_value,
                current_input: app.input.clone(),
                cursor_offset: app.cursor_offset,
            };
            KeyAction::TextEdit(TextEdit {
                event_input,
                change_input,
                intended_cursor,
            })
        }
        _ => KeyAction::Ignored,
    }
}

#[derive(Default, Clone, Copy)]
struct KeyFlags {
    escape: bool,
    return_key: bool,
    backspace: bool,
    delete: bool,
}

impl KeyFlags {
    fn none() -> Self {
        Self::default()
    }
    fn return_key() -> Self {
        Self {
            return_key: true,
            ..Self::default()
        }
    }
    fn backspace() -> Self {
        Self {
            backspace: true,
            ..Self::default()
        }
    }
    fn delete() -> Self {
        Self {
            delete: true,
            ..Self::default()
        }
    }
}

fn build_event_input(app: &AppState, ch: String, flags: KeyFlags) -> InputEventInput {
    InputEventInput {
        ch,
        current_input: app.input.clone(),
        cursor_offset: app.cursor_offset,
        full_screen_dialog_open: app.has_fullscreen_dialog(),
        is_macos: cfg!(target_os = "macos"),
        option_shortcut: None,
        terminal_display_name: None,
        footer_item_selected: app.slash_picker.is_some() || app.at_mention_picker.is_some(),
        ctrl: false,
        meta: false,
        escape: flags.escape,
        return_key: flags.return_key,
        backspace: flags.backspace,
        delete: flags.delete,
        help_open: app.help_open,
        speculation_active: app.speculation_active,
        side_question_visible: app.side_question_visible,
        has_editable_queued_command: crate::tui::dispatch::has_editable_queued_commands(app),
        has_messages: !app.rebon_tui.transcript.is_empty(),
        input_is_empty: app.input.is_empty(),
        is_loading: app.is_loading,
    }
}

pub(crate) fn splice_char_str(input: &str, cursor: usize, ch: &str) -> (String, usize) {
    let cursor = clamp_cursor_offset(input, cursor);
    let mut next = String::with_capacity(input.len() + ch.len());
    next.push_str(&input[..cursor]);
    next.push_str(ch);
    next.push_str(&input[cursor..]);
    (next, cursor + ch.len())
}

pub(crate) fn splice_backspace(input: &str, cursor: usize) -> (String, usize) {
    let cursor = clamp_cursor_offset(input, cursor);
    if cursor == 0 {
        return (input.to_string(), 0);
    }
    let mut prev = cursor - 1;
    while !input.is_char_boundary(prev) && prev > 0 {
        prev -= 1;
    }
    let mut next = String::with_capacity(input.len().saturating_sub(cursor - prev));
    next.push_str(&input[..prev]);
    next.push_str(&input[cursor..]);
    (next, prev)
}

pub(crate) fn splice_delete(input: &str, cursor: usize) -> (String, usize) {
    let cursor = clamp_cursor_offset(input, cursor);
    if cursor >= input.len() {
        return (input.to_string(), cursor);
    }
    let mut end = cursor + 1;
    while end < input.len() && !input.is_char_boundary(end) {
        end += 1;
    }
    let mut next = String::with_capacity(input.len().saturating_sub(end - cursor));
    next.push_str(&input[..cursor]);
    next.push_str(&input[end..]);
    (next, cursor)
}

/// If the text immediately before `cursor` ends with a paste/image
/// reference chip (`[Pasted text #N ...]`, `[Image #N]`,
/// `[...Truncated text #N ...]`), return `Some((start, end))` of
/// that chip.
fn paste_ref_ending_at(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let cursor = clamp_cursor_offset(input, cursor);
    let before = &input[..cursor];
    // Find the last '[' before cursor
    let bracket_start = before.rfind('[')?;
    let candidate = &input[bracket_start..cursor];
    // Match the known chip patterns that end with ']'
    if !candidate.ends_with(']') {
        return None;
    }
    let is_chip = candidate.starts_with("[Pasted text #")
        || candidate.starts_with("[Image #")
        || candidate.starts_with("[...Truncated text #");
    if is_chip {
        Some((bracket_start, cursor))
    } else {
        None
    }
}

pub fn cursor_left(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor_offset(input, cursor);
    if cursor == 0 {
        return 0;
    }
    let mut prev = cursor - 1;
    while !input.is_char_boundary(prev) && prev > 0 {
        prev -= 1;
    }
    prev
}

pub fn cursor_right(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor_offset(input, cursor);
    if cursor >= input.len() {
        return input.len();
    }
    let mut next = cursor + 1;
    while next < input.len() && !input.is_char_boundary(next) {
        next += 1;
    }
    next
}

pub fn cursor_home(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor_offset(input, cursor);
    match input[..cursor].rfind('\n') {
        Some(nl) => nl + 1,
        None => 0,
    }
}

pub fn cursor_end(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor_offset(input, cursor);
    match input[cursor..].find('\n') {
        Some(offset) => cursor + offset,
        None => input.len(),
    }
}

/// Whether the cursor is on the first logical line of the input.
pub fn is_cursor_on_first_line(input: &str, cursor: usize) -> bool {
    match input.find('\n') {
        None => true,
        Some(first_nl) => cursor <= first_nl,
    }
}

/// Whether the cursor is on the last logical line of the input.
pub fn is_cursor_on_last_line(input: &str, cursor: usize) -> bool {
    match input.rfind('\n') {
        None => true,
        Some(last_nl) => cursor > last_nl,
    }
}

/// Find the byte offset in `line` that corresponds to the given
/// visual column. When the target falls in the middle of a wide
/// character, returns the byte offset before that character.
fn visual_col_to_byte(line: &str, target_col: usize) -> usize {
    let mut col = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        let w = WidthChar::width(ch).unwrap_or(0);
        if col + w > target_col {
            return byte_idx;
        }
        col += w;
    }
    line.len()
}

/// Move the cursor up by one logical line, preserving **visual column**
/// (display width) instead of byte offset. Returns `None` if already
/// on the first line.
pub fn cursor_up_line(input: &str, cursor: usize) -> Option<usize> {
    let cursor = clamp_cursor_offset(input, cursor);
    let current_line_start = match input[..cursor].rfind('\n') {
        Some(nl) => nl + 1,
        None => return None,
    };
    let visual_col = input[current_line_start..cursor].width();
    let prev_line_start = match input[..current_line_start.saturating_sub(1)].rfind('\n') {
        Some(nl) => nl + 1,
        None => 0,
    };
    let prev_line = &input[prev_line_start..current_line_start - 1];
    Some(prev_line_start + visual_col_to_byte(prev_line, visual_col))
}

/// Move the cursor down by one logical line, preserving **visual column**
/// (display width) instead of byte offset. Returns `None` if already
/// on the last line.
pub fn cursor_down_line(input: &str, cursor: usize) -> Option<usize> {
    let cursor = clamp_cursor_offset(input, cursor);
    let current_line_start = match input[..cursor].rfind('\n') {
        Some(nl) => nl + 1,
        None => 0,
    };
    let visual_col = input[current_line_start..cursor].width();
    let next_nl = match input[cursor..].find('\n') {
        Some(offset) => cursor + offset,
        None => return None,
    };
    let next_line_start = next_nl + 1;
    let next_line_end = match input[next_line_start..].find('\n') {
        Some(offset) => next_line_start + offset,
        None => input.len(),
    };
    let next_line = &input[next_line_start..next_line_end];
    Some(next_line_start + visual_col_to_byte(next_line, visual_col))
}

pub fn cursor_up_visual_line(input: &str, cursor: usize, area_width: usize) -> Option<usize> {
    let lines = prompt_visual_lines(input, area_width);
    let (line_idx, visual_col) = locate_visual_cursor(input, cursor, &lines)?;
    let target_idx = line_idx.checked_sub(1)?;
    Some(visual_line_col_to_offset(
        input,
        lines[target_idx],
        visual_col,
    ))
}

pub fn cursor_down_visual_line(input: &str, cursor: usize, area_width: usize) -> Option<usize> {
    let lines = prompt_visual_lines(input, area_width);
    let (line_idx, visual_col) = locate_visual_cursor(input, cursor, &lines)?;
    let target_idx = line_idx + 1;
    if target_idx >= lines.len() {
        return None;
    }
    Some(visual_line_col_to_offset(
        input,
        lines[target_idx],
        visual_col,
    ))
}

#[derive(Debug, Clone, Copy)]
struct PromptVisualLine {
    start: usize,
    end: usize,
}

fn prompt_visual_lines(input: &str, area_width: usize) -> Vec<PromptVisualLine> {
    let width = area_width.max(1);
    let mut lines = Vec::new();
    let mut logical_start = 0usize;

    loop {
        let logical_end = match input[logical_start..].find('\n') {
            Some(offset) => logical_start + offset,
            None => input.len(),
        };
        push_prompt_visual_lines(input, logical_start, logical_end, width, &mut lines);
        if logical_end == input.len() {
            break;
        }
        logical_start = logical_end + 1;
    }

    lines
}

fn push_prompt_visual_lines(
    input: &str,
    logical_start: usize,
    logical_end: usize,
    area_width: usize,
    lines: &mut Vec<PromptVisualLine>,
) {
    if logical_start == logical_end {
        lines.push(PromptVisualLine {
            start: logical_start,
            end: logical_end,
        });
        return;
    }

    let mut row_start = logical_start;
    while row_start < logical_end {
        let mut row_width = 0usize;
        let mut last_whitespace: Option<(usize, usize)> = None;
        let mut row_end = logical_end;
        let mut next_row_start = logical_end;

        for (relative_idx, ch) in input[row_start..logical_end].char_indices() {
            let byte_idx = row_start + relative_idx;
            let next_byte_idx = byte_idx + ch.len_utf8();
            let ch_width = WidthChar::width(ch).unwrap_or(0);
            if ch.is_whitespace() && row_width > 0 {
                last_whitespace = Some((byte_idx, next_byte_idx));
            }
            if ch_width > 0 && row_width + ch_width > area_width {
                if ch.is_whitespace() && row_width > 0 {
                    row_end = byte_idx;
                    next_row_start = next_byte_idx;
                } else if let Some((whitespace_start, whitespace_end)) = last_whitespace {
                    row_end = whitespace_start;
                    next_row_start = whitespace_end;
                } else if byte_idx == row_start {
                    row_end = next_byte_idx;
                    next_row_start = next_byte_idx;
                } else {
                    row_end = byte_idx;
                    next_row_start = byte_idx;
                }
                break;
            }
            row_width += ch_width;
        }

        lines.push(PromptVisualLine {
            start: row_start,
            end: row_end,
        });
        row_start = next_row_start;
    }
}

fn locate_visual_cursor(
    input: &str,
    cursor: usize,
    lines: &[PromptVisualLine],
) -> Option<(usize, usize)> {
    let cursor = clamp_cursor_offset(input, cursor);
    if let Some((idx, _)) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.start == cursor)
    {
        return Some((idx, 0));
    }

    for (idx, line) in lines.iter().enumerate() {
        if cursor >= line.start && cursor <= line.end {
            let visual_col = input[line.start..cursor].width();
            return Some((idx, visual_col));
        }
    }

    lines
        .last()
        .map(|line| (lines.len() - 1, input[line.start..line.end].width()))
}

fn visual_line_col_to_offset(input: &str, line: PromptVisualLine, target_col: usize) -> usize {
    let mut col = 0usize;
    for (relative_idx, ch) in input[line.start..line.end].char_indices() {
        let byte_idx = line.start + relative_idx;
        let ch_width = WidthChar::width(ch).unwrap_or(0);
        if col + ch_width > target_col {
            return byte_idx;
        }
        col += ch_width;
    }
    line.end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_char_handles_empty_input() {
        let (next, cursor) = splice_char_str("", 0, "a");
        assert_eq!(next, "a");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn splice_char_inserts_mid_string() {
        let (next, cursor) = splice_char_str("helo", 3, "l");
        assert_eq!(next, "hello");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn splice_char_snaps_cursor_to_utf8_boundary() {
        let (next, cursor) = splice_char_str("你a", 2, "x");
        assert_eq!(next, "x你a");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn splice_backspace_at_start_is_noop() {
        let (next, cursor) = splice_backspace("hello", 0);
        assert_eq!(next, "hello");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn splice_backspace_removes_previous_char() {
        let (next, cursor) = splice_backspace("hello", 3);
        assert_eq!(next, "helo");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn splice_backspace_handles_multibyte() {
        let (next, cursor) = splice_backspace("你好", 6);
        assert_eq!(next, "你");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn splice_delete_at_end_is_noop() {
        let (next, cursor) = splice_delete("hello", 5);
        assert_eq!(next, "hello");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn splice_delete_removes_current_char() {
        let (next, cursor) = splice_delete("hello", 2);
        assert_eq!(next, "helo");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn cursor_home_and_end_respect_newlines() {
        assert_eq!(cursor_home("ab\ncd", 4), 3);
        assert_eq!(cursor_end("ab\ncd", 4), 5);
        assert_eq!(cursor_home("ab\ncd", 1), 0);
        assert_eq!(cursor_end("ab\ncd", 1), 2);
    }

    #[test]
    fn cursor_movement_respects_char_boundaries() {
        let s = "a你b";
        assert_eq!(cursor_right(s, 0), 1);
        assert_eq!(cursor_right(s, 1), 4);
        assert_eq!(cursor_right(s, 4), 5);
        assert_eq!(cursor_left(s, 5), 4);
        assert_eq!(cursor_left(s, 4), 1);
        assert_eq!(cursor_left(s, 1), 0);
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn app_with_input(text: &str, cursor: usize) -> AppState {
        let mut app = AppState::new();
        app.input = text.to_string();
        app.cursor_offset = cursor;
        app
    }

    #[test]
    fn enter_without_shift_produces_submit_with_current_buffer_snapshot() {
        let app = app_with_input("hello world", 11);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::NONE), &app);
        match action {
            KeyAction::Submit(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn shift_enter_produces_text_edit_with_newline_splice() {
        let app = app_with_input("ab", 1);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "a\nb");
                assert_eq!(edit.intended_cursor, 2);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn alt_enter_inserts_newline_as_fallback_for_terminals_that_drop_shift() {
        // Windows Terminal and conhost don't report SHIFT with Enter;
        // Alt+Enter is the cross-terminal fallback for splitting lines.
        let app = app_with_input("ab", 1);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::ALT), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "a\nb");
                assert_eq!(edit.intended_cursor, 2);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_enter_inserts_newline_as_fallback_for_terminals_that_drop_shift() {
        let app = app_with_input("ab", 1);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::CONTROL), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "a\nb");
                assert_eq!(edit.intended_cursor, 2);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_j_inserts_newline_ascii_lf_fallback() {
        // Ctrl+J is ASCII LF — works on every Unix-style terminal and
        // is the classic readline binding for "insert newline".
        let app = app_with_input("ab", 1);
        let action = translate_key(&key(KeyCode::Char('j'), KeyModifiers::CONTROL), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "a\nb");
                assert_eq!(edit.intended_cursor, 2);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn backslash_enter_replaces_trailing_backslash_with_newline() {
        // The backslash-return behaviour — the universal
        // terminal-agnostic newline fallback. `ab\` + Enter becomes
        // `ab\n` with the cursor after the newline.
        let app = app_with_input("ab\\", 3);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::NONE), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "ab\n");
                assert_eq!(edit.intended_cursor, 3);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn backslash_enter_midstring_splits_around_cursor() {
        // Backslash escape works mid-string too: `a\|bc` + Enter →
        // `a\n|bc` where | is the cursor.
        let app = app_with_input("a\\bc", 2);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::NONE), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "a\nbc");
                assert_eq!(edit.intended_cursor, 2);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn plain_enter_without_trailing_backslash_still_submits() {
        let app = app_with_input("hello", 5);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(action, KeyAction::Submit(_)));
    }

    #[test]
    fn line_start_question_toggles_help_without_editing_input() {
        let app = app_with_input("", 0);
        let action = translate_key(&key(KeyCode::Char('?'), KeyModifiers::SHIFT), &app);
        assert!(matches!(action, KeyAction::ToggleHelp));
    }

    #[test]
    fn line_start_bang_enters_bash_mode_without_editing_input() {
        let app = app_with_input("", 0);
        let action = translate_key(&key(KeyCode::Char('!'), KeyModifiers::SHIFT), &app);
        assert!(matches!(action, KeyAction::SetPromptMode(mode) if mode == "bash"));
    }

    #[test]
    fn typing_while_help_is_open_closes_help_and_edits_input() {
        let mut app = app_with_input("", 0);
        app.help_open = true;
        let action = translate_key(&key(KeyCode::Char('a'), KeyModifiers::NONE), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert!(edit.event_input.help_open);
                assert_eq!(edit.change_input.next_value, "a");
                assert_eq!(edit.intended_cursor, 1);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn help_tab_keys_take_precedence_over_footer_selection() {
        use rebon_tui::promptinput::footer_navigation::FooterItem;

        let mut app = app_with_input("anything", 8);
        app.help_open = true;
        app.footer_selection = Some(FooterItem::Tasks);

        assert!(matches!(
            translate_key(&key(KeyCode::Left, KeyModifiers::NONE), &app),
            KeyAction::HelpPreviousTab
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::Right, KeyModifiers::NONE), &app),
            KeyAction::HelpNextTab
        ));
    }

    #[test]
    fn ctrl_c_maps_to_cancel_or_exit_in_s5() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), &app);
        assert!(matches!(action, KeyAction::CancelOrExit));
    }

    #[test]
    fn ctrl_d_maps_to_cancel_or_exit_for_attached_session_detach() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('d'), KeyModifiers::CONTROL), &app);
        assert!(matches!(action, KeyAction::CancelOrExit));
    }

    #[test]
    fn esc_maps_to_interrupt() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(action, KeyAction::Interrupt));
    }

    #[test]
    fn ctrl_z_maps_to_undo() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('z'), KeyModifiers::CONTROL), &app);
        assert!(matches!(action, KeyAction::Undo));
    }

    #[test]
    fn ctrl_o_maps_to_toggle_tool_output() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('o'), KeyModifiers::CONTROL), &app);
        assert!(matches!(action, KeyAction::ToggleToolOutput));
    }

    #[test]
    fn alt_o_maps_to_toggle_fast_mode() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('o'), KeyModifiers::ALT), &app);
        assert!(matches!(action, KeyAction::ToggleFastMode));
    }

    #[test]
    fn ctrl_b_maps_to_background_tasks() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('b'), KeyModifiers::CONTROL), &app);
        assert!(matches!(action, KeyAction::BackgroundTasks));
    }

    #[test]
    fn ctrl_v_maps_to_clipboard_paste() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('v'), KeyModifiers::CONTROL), &app);
        assert!(matches!(action, KeyAction::PasteFromClipboard));
    }

    #[test]
    fn alt_v_maps_to_image_clipboard_paste() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Char('v'), KeyModifiers::ALT), &app);
        assert!(matches!(action, KeyAction::PasteImageFromClipboard));
    }

    #[test]
    fn arrows_route_to_footer_actions_when_footer_selected() {
        use rebon_tui::promptinput::footer_navigation::FooterItem;
        let mut app = app_with_input("anything", 8);
        app.footer_selection = Some(FooterItem::Tasks);
        assert!(matches!(
            translate_key(&key(KeyCode::Up, KeyModifiers::NONE), &app),
            KeyAction::FooterUp
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::Down, KeyModifiers::NONE), &app),
            KeyAction::FooterDown
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::Right, KeyModifiers::NONE), &app),
            KeyAction::FooterNext
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::Left, KeyModifiers::NONE), &app),
            KeyAction::FooterPrevious
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::Enter, KeyModifiers::NONE), &app),
            KeyAction::FooterOpenSelected
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::Esc, KeyModifiers::NONE), &app),
            KeyAction::FooterClear
        ));
    }

    #[test]
    fn ctrl_c_still_cancels_when_footer_selected() {
        use rebon_tui::promptinput::footer_navigation::FooterItem;
        let mut app = app_with_input("anything", 8);
        app.footer_selection = Some(FooterItem::Tasks);
        let action = translate_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), &app);
        assert!(matches!(action, KeyAction::CancelOrExit));
    }

    #[test]
    fn shift_enter_still_inserts_newline_when_footer_selected() {
        use rebon_tui::promptinput::footer_navigation::FooterItem;
        let mut app = app_with_input("ab", 1);
        app.footer_selection = Some(FooterItem::Tasks);
        let action = translate_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), &app);
        assert!(matches!(action, KeyAction::TextEdit(_)));
    }

    #[test]
    fn arrow_and_page_keys_map_to_correct_actions() {
        let app = app_with_input("anything", 8);
        assert!(matches!(
            translate_key(&key(KeyCode::Up, KeyModifiers::NONE), &app),
            KeyAction::PromptUp
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::Down, KeyModifiers::NONE), &app),
            KeyAction::PromptDown
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::PageUp, KeyModifiers::NONE), &app),
            KeyAction::PageUp
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::PageDown, KeyModifiers::NONE), &app),
            KeyAction::PageDown
        ));
    }

    #[test]
    fn ctrl_home_end_map_to_transcript_edges() {
        let app = app_with_input("anything", 8);
        assert!(matches!(
            translate_key(&key(KeyCode::Home, KeyModifiers::CONTROL), &app),
            KeyAction::ScrollHome
        ));
        assert!(matches!(
            translate_key(&key(KeyCode::End, KeyModifiers::CONTROL), &app),
            KeyAction::ScrollEnd
        ));
    }

    #[test]
    fn paste_ref_ending_at_detects_pasted_text_chip() {
        let input = "hello [Pasted text #1 +5 lines]";
        let (start, end) = paste_ref_ending_at(input, input.len()).unwrap();
        assert_eq!(start, 6);
        assert_eq!(end, input.len());
        assert_eq!(&input[start..end], "[Pasted text #1 +5 lines]");
    }

    #[test]
    fn paste_ref_ending_at_detects_simple_pasted_text() {
        let input = "[Pasted text #3]";
        let (start, end) = paste_ref_ending_at(input, input.len()).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, input.len());
    }

    #[test]
    fn paste_ref_ending_at_detects_image_chip() {
        let input = "text [Image #2]";
        let (start, end) = paste_ref_ending_at(input, input.len()).unwrap();
        assert_eq!(&input[start..end], "[Image #2]");
    }

    #[test]
    fn paste_ref_ending_at_clamps_cursor_past_end() {
        let input = "text [Image #2]";
        let (start, end) = paste_ref_ending_at(input, usize::MAX).unwrap();
        assert_eq!(&input[start..end], "[Image #2]");
    }

    #[test]
    fn paste_ref_ending_at_handles_cursor_inside_multibyte_character() {
        assert!(paste_ref_ending_at("你[Image #2]", 2).is_none());
    }

    #[test]
    fn paste_ref_ending_at_returns_none_for_regular_text() {
        assert!(paste_ref_ending_at("hello world", 11).is_none());
        assert!(paste_ref_ending_at("[not a chip]", 12).is_none());
    }

    #[test]
    fn paste_ref_ending_at_returns_none_when_cursor_not_at_end_of_chip() {
        let input = "[Pasted text #1 +5 lines] more text";
        // cursor is in "more text", not right after the chip
        assert!(paste_ref_ending_at(input, input.len()).is_none());
    }

    #[test]
    fn backspace_on_paste_chip_deletes_entire_reference() {
        let input = "ask about [Pasted text #1 +3 lines]";
        let app = app_with_input(input, input.len());
        let action = translate_key(&key(KeyCode::Backspace, KeyModifiers::NONE), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "ask about ");
                assert_eq!(edit.intended_cursor, 10);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn backspace_on_normal_text_still_deletes_one_char() {
        let app = app_with_input("hello", 5);
        let action = translate_key(&key(KeyCode::Backspace, KeyModifiers::NONE), &app);
        match action {
            KeyAction::TextEdit(edit) => {
                assert_eq!(edit.change_input.next_value, "hell");
                assert_eq!(edit.intended_cursor, 4);
            }
            other => panic!("expected TextEdit, got {other:?}"),
        }
    }

    #[test]
    fn is_cursor_on_first_line_single_line() {
        assert!(is_cursor_on_first_line("hello", 0));
        assert!(is_cursor_on_first_line("hello", 5));
        assert!(is_cursor_on_first_line("", 0));
    }

    #[test]
    fn is_cursor_on_first_line_multi_line() {
        assert!(is_cursor_on_first_line("ab\ncd", 0));
        assert!(is_cursor_on_first_line("ab\ncd", 2));
        assert!(!is_cursor_on_first_line("ab\ncd", 3));
        assert!(!is_cursor_on_first_line("ab\ncd", 4));
    }

    #[test]
    fn is_cursor_on_last_line_single_line() {
        assert!(is_cursor_on_last_line("hello", 0));
        assert!(is_cursor_on_last_line("hello", 5));
    }

    #[test]
    fn is_cursor_on_last_line_multi_line() {
        assert!(!is_cursor_on_last_line("ab\ncd", 0));
        assert!(!is_cursor_on_last_line("ab\ncd", 2));
        // cursor at offset 3 = right after '\n', so on last line
        assert!(is_cursor_on_last_line("ab\ncd", 3));
        assert!(is_cursor_on_last_line("ab\ncd", 5));
    }

    #[test]
    fn cursor_up_line_moves_between_logical_lines() {
        assert_eq!(cursor_up_line("ab\ncd", 4), Some(1)); // 'd' → 'b'
        assert_eq!(cursor_up_line("ab\ncd", 3), Some(0)); // 'c' → 'a'
        assert_eq!(cursor_up_line("ab\ncd", 0), None); // already first line
        assert_eq!(cursor_up_line("ab\ncd", 2), None); // still first line
    }

    #[test]
    fn cursor_up_line_clamps_column_to_shorter_line() {
        assert_eq!(cursor_up_line("a\ncde", 5), Some(1)); // col 2 → clamp to 1
    }

    #[test]
    fn cursor_down_line_moves_between_logical_lines() {
        assert_eq!(cursor_down_line("ab\ncd", 0), Some(3)); // 'a' → 'c'
        assert_eq!(cursor_down_line("ab\ncd", 1), Some(4)); // 'b' → 'd'
        assert_eq!(cursor_down_line("ab\ncd", 3), None); // already last line
        assert_eq!(cursor_down_line("ab\ncd", 5), None); // still last line
    }

    #[test]
    fn cursor_down_line_clamps_column_to_shorter_line() {
        // "abc" col 2 → "d" col clamp to len 1 → position 5 (EOL)
        assert_eq!(cursor_down_line("abc\nd", 2), Some(5));
    }

    #[test]
    fn cursor_up_down_three_lines() {
        let s = "abc\ndef\nghi";
        // positions: a=0 b=1 c=2 \n=3 d=4 e=5 f=6 \n=7 g=8 h=9 i=10
        assert_eq!(cursor_up_line(s, 9), Some(5)); // 'h' col1 → 'e' col1
        assert_eq!(cursor_up_line(s, 5), Some(1)); // 'e' col1 → 'b' col1
        assert_eq!(cursor_down_line(s, 1), Some(5)); // 'b' col1 → 'e' col1
        assert_eq!(cursor_down_line(s, 5), Some(9)); // 'e' col1 → 'h' col1
    }

    #[test]
    fn cursor_up_line_preserves_visual_column_for_cjk() {
        // Line 0: "你好" (6 bytes, 4 visual cols)
        // Line 1: "abcde" (5 bytes, 5 visual cols)
        // Cursor after "abcd" (byte 10, visual col 4):
        //   should move to after "你好" (byte 6, visual col 4) on line 0.
        let s = "你好\nabcde";
        // byte layout: 你(0-2) 好(3-5) \n(6) a(7) b(8) c(9) d(10) e(11)
        assert_eq!(cursor_up_line(s, 11), Some(6)); // visual col 4 → end of 你好
    }

    #[test]
    fn cursor_down_line_preserves_visual_column_for_cjk() {
        // Line 0: "abcde" (5 bytes, 5 visual cols)
        // Line 1: "你好x" (7 bytes, 5 visual cols)
        // Cursor after "ab" (byte 2, visual col 2):
        //   should move to after "你" (byte 9, visual col 2) on line 1.
        let s = "abcde\n你好x";
        // byte layout: a(0) b(1) c(2) d(3) e(4) \n(5) 你(6-8) 好(9-11) x(12)
        assert_eq!(cursor_down_line(s, 2), Some(9)); // visual col 2 → after 你
    }

    #[test]
    fn cursor_up_visual_line_moves_within_wrapped_logical_line() {
        assert_eq!(cursor_up_visual_line("abcdef", 4, 3), Some(1));
        assert_eq!(cursor_up_visual_line("abcdef", 1, 3), None);
    }

    #[test]
    fn cursor_down_visual_line_moves_within_wrapped_logical_line() {
        assert_eq!(cursor_down_visual_line("abcdef", 1, 3), Some(4));
        assert_eq!(cursor_down_visual_line("abcdef", 4, 3), None);
    }

    #[test]
    fn cursor_visual_line_wraps_at_word_boundaries() {
        assert_eq!(cursor_up_visual_line("hello world", 11, 10), Some(5));
        assert_eq!(cursor_down_visual_line("hello world", 5, 10), Some(11));
    }

    #[test]
    fn cursor_visual_line_preserves_display_column_for_cjk() {
        let s = "你好ab";
        assert_eq!(cursor_down_visual_line(s, 3, 4), Some(8));
        assert_eq!(cursor_up_visual_line(s, 8, 4), Some(3));
    }

    #[test]
    fn backtab_maps_to_cycle_permission_mode() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::BackTab, KeyModifiers::SHIFT), &app);
        assert!(matches!(action, KeyAction::CyclePermissionMode));
    }

    #[test]
    fn shift_tab_maps_to_cycle_permission_mode() {
        let app = app_with_input("anything", 8);
        let action = translate_key(&key(KeyCode::Tab, KeyModifiers::SHIFT), &app);
        assert!(matches!(action, KeyAction::CyclePermissionMode));
    }
}
