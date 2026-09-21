//! Ctrl+B background-session hint.
//!
//! The behaviour has five parts:
//!
//! 1. On a background-task press, foreground tasks that are running get
//!    backgrounded, and the first such press also flips the
//!    has-used-background-task config flag (a one-shot config write).
//! 2. Otherwise, when the session-background switch is on and a query is
//!    in progress, the press starts a double-press flow: the first press
//!    shows the hint, a second press within 800ms backgrounds the session.
//! 3. The background-task keybinding is only "active" when
//!    there's something to background (avoids double-firing readline
//!    `backward-char` at idle prompts).
//! 4. The shortcut text comes from the user's configured keybinding, with
//!    one extra rule: if the terminal is `tmux` and the resolved shortcut
//!    is exactly `ctrl+b`, it becomes `ctrl+b ctrl+b` (since `ctrl+b`
//!    is the tmux prefix).
//! 5. The hint is hidden unless a query is in progress and the session
//!    hint flag is set.
//!
//! Pure logic covered here:
//!
//! 1. [`session_background_keybinding_active`] — the gating for the
//!    keybinding (a foreground task is running, or the session-background
//!    switch is on while a query is in progress).
//! 2. [`session_background_press`] — the press dispatcher (returns
//!    one of [`BackgroundPressOutcome`]).
//! 3. [`resolve_background_shortcut`] — the tmux double-press override.
//! 4. [`session_background_hint_visible`] — the hint visibility gate.
//!
//! The double-press timer itself belongs to the consumer; we only expose
//! the decision shape.

/// Inputs the press dispatcher reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionBackgroundInputs {
    /// Background tasks are disabled for this process. When true the press is
    /// dropped entirely.
    pub background_tasks_disabled: bool,
    /// Whether any foreground shell task is still running.
    pub has_foreground_tasks: bool,
    /// Whether the user has previously used a background task (a
    /// persisted config flag).
    pub has_used_background_task: bool,
    /// Whether backgrounding the whole session is enabled.
    pub session_bg_enabled: bool,
    /// Whether a query is in progress.
    pub is_loading: bool,
}

/// What the press should do; the dispatcher has three branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundPressOutcome {
    /// Background-tasks env-disabled or no-op condition: the press is
    /// dropped.
    Ignored,
    /// Foreground tasks are present: background them all. The
    /// [`mark_used`](Self::BackgroundForeground) flag is `true` if the
    /// consumer should also persist the has-used-background-task config
    /// flag.
    BackgroundForeground {
        /// Whether to flip the has-used-background-task config flag.
        mark_used: bool,
    },
    /// Session-background flow: start the double-press handling. The
    /// consumer owns the timer.
    DoublePressSession,
}

/// Pure dispatcher for a background-task press.
pub fn session_background_press(inputs: SessionBackgroundInputs) -> BackgroundPressOutcome {
    if inputs.background_tasks_disabled {
        return BackgroundPressOutcome::Ignored;
    }
    if inputs.has_foreground_tasks {
        return BackgroundPressOutcome::BackgroundForeground {
            mark_used: !inputs.has_used_background_task,
        };
    }
    if inputs.session_bg_enabled && inputs.is_loading {
        return BackgroundPressOutcome::DoublePressSession;
    }
    BackgroundPressOutcome::Ignored
}

/// Whether the `task:background` keybinding should be active: a
/// foreground task is running, or the session-background switch is on
/// while a query is in progress.
pub fn session_background_keybinding_active(
    has_foreground: bool,
    session_bg_enabled: bool,
    is_loading: bool,
) -> bool {
    has_foreground || (session_bg_enabled && is_loading)
}

/// Apply the tmux Ctrl+B override.
///
/// * If the terminal is `"tmux"` AND the base shortcut is exactly
///   `"ctrl+b"`, return `"Ctrl+B Ctrl+B"`.
/// * Otherwise return the base shortcut formatted for display.
pub fn resolve_background_shortcut(terminal: &str, base_shortcut: &str) -> String {
    let shortcut = if terminal == "tmux" && base_shortcut == "ctrl+b" {
        "ctrl+b ctrl+b"
    } else {
        base_shortcut
    };
    format_shortcut(shortcut)
}

fn format_shortcut(shortcut: &str) -> String {
    shortcut
        .split_whitespace()
        .map(|chord| {
            chord
                .split('+')
                .map(format_shortcut_part)
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_shortcut_part(part: &str) -> String {
    let lower = part.to_ascii_lowercase();
    match lower.as_str() {
        "ctrl" | "control" => "Ctrl".to_string(),
        "shift" => "Shift".to_string(),
        "alt" | "option" => "Alt".to_string(),
        "cmd" | "command" => "Cmd".to_string(),
        "meta" | "super" => "Meta".to_string(),
        "tab" => "Tab".to_string(),
        "esc" | "escape" => "Esc".to_string(),
        "return" => "Return".to_string(),
        "enter" => "Enter".to_string(),
        _ if lower.len() == 1 && lower.as_bytes()[0].is_ascii_alphabetic() => {
            lower.to_ascii_uppercase()
        }
        _ => part.to_string(),
    }
}

/// Whether the hint row should be shown: only while a query is in
/// progress and the session hint flag is set.
pub fn session_background_hint_visible(is_loading: bool, show_session_hint: bool) -> bool {
    is_loading && show_session_hint
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> SessionBackgroundInputs {
        SessionBackgroundInputs {
            background_tasks_disabled: false,
            has_foreground_tasks: false,
            has_used_background_task: false,
            session_bg_enabled: false,
            is_loading: false,
        }
    }

    #[test]
    fn press_ignored_when_env_disabled() {
        let mut i = inputs();
        i.background_tasks_disabled = true;
        i.has_foreground_tasks = true;
        assert_eq!(session_background_press(i), BackgroundPressOutcome::Ignored);
    }

    #[test]
    fn press_backgrounds_foreground_and_marks_used_first_time() {
        let mut i = inputs();
        i.has_foreground_tasks = true;
        i.has_used_background_task = false;
        assert_eq!(
            session_background_press(i),
            BackgroundPressOutcome::BackgroundForeground { mark_used: true }
        );
    }

    #[test]
    fn press_backgrounds_foreground_without_marking_when_already_used() {
        let mut i = inputs();
        i.has_foreground_tasks = true;
        i.has_used_background_task = true;
        assert_eq!(
            session_background_press(i),
            BackgroundPressOutcome::BackgroundForeground { mark_used: false }
        );
    }

    #[test]
    fn press_double_press_when_idle_and_session_bg_and_loading() {
        let mut i = inputs();
        i.session_bg_enabled = true;
        i.is_loading = true;
        assert_eq!(
            session_background_press(i),
            BackgroundPressOutcome::DoublePressSession
        );
    }

    #[test]
    fn press_ignored_when_session_bg_disabled_even_if_loading() {
        let mut i = inputs();
        i.is_loading = true;
        assert_eq!(session_background_press(i), BackgroundPressOutcome::Ignored);
    }

    #[test]
    fn press_ignored_when_loading_false() {
        let mut i = inputs();
        i.session_bg_enabled = true;
        assert_eq!(session_background_press(i), BackgroundPressOutcome::Ignored);
    }

    #[test]
    fn keybinding_active_when_foreground() {
        assert!(session_background_keybinding_active(true, false, false));
    }

    #[test]
    fn keybinding_active_when_session_bg_and_loading() {
        assert!(session_background_keybinding_active(false, true, true));
    }

    #[test]
    fn keybinding_inactive_when_no_foreground_no_loading() {
        assert!(!session_background_keybinding_active(false, false, false));
    }

    #[test]
    fn keybinding_inactive_when_session_bg_but_idle() {
        assert!(!session_background_keybinding_active(false, true, false));
    }

    #[test]
    fn shortcut_passes_through_when_not_tmux() {
        assert_eq!(resolve_background_shortcut("xterm", "ctrl+b"), "Ctrl+B");
    }

    #[test]
    fn shortcut_passes_through_when_not_ctrl_b() {
        assert_eq!(resolve_background_shortcut("tmux", "ctrl+x"), "Ctrl+X");
    }

    #[test]
    fn shortcut_doubles_in_tmux_with_ctrl_b() {
        assert_eq!(
            resolve_background_shortcut("tmux", "ctrl+b"),
            "Ctrl+B Ctrl+B"
        );
    }

    #[test]
    fn shortcut_does_not_double_for_custom_shortcut_in_tmux() {
        // The override is gated on EXACTLY "ctrl+b" — a custom binding
        // like "ctrl+alt+b" should not be doubled.
        assert_eq!(
            resolve_background_shortcut("tmux", "ctrl+alt+b"),
            "Ctrl+Alt+B"
        );
    }

    #[test]
    fn hint_hidden_when_not_loading() {
        assert!(!session_background_hint_visible(false, true));
    }

    #[test]
    fn hint_hidden_when_show_session_hint_false() {
        assert!(!session_background_hint_visible(true, false));
    }

    #[test]
    fn hint_visible_when_both_flags_true() {
        assert!(session_background_hint_visible(true, true));
    }
}
