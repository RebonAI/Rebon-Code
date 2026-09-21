//! Show-once timer for the `/fast` hint next to the fast-mode icon.

/// How long the hint stays visible, in milliseconds.
pub const HINT_DISPLAY_DURATION_MS: u64 = 5_000;

/// Explicit state machine for the show-once hint timer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FastIconHintState {
    /// Session-global "already shown once" latch.
    pub has_shown_this_session: bool,
    /// Current visible state.
    pub show_hint: bool,
    deadline_ms: Option<u64>,
}

impl FastIconHintState {
    /// Start the hint when the fast icon becomes visible, once per session.
    pub fn on_icon_visibility_change(&mut self, show_fast_icon: bool, now_ms: u64) {
        if self.has_shown_this_session || !show_fast_icon {
            return;
        }
        self.has_shown_this_session = true;
        self.show_hint = true;
        self.deadline_ms = Some(now_ms + HINT_DISPLAY_DURATION_MS);
    }

    /// Hide the hint once its display deadline has passed.
    pub fn tick(&mut self, now_ms: u64) {
        if self.deadline_ms.is_some_and(|deadline| now_ms >= deadline) {
            self.show_hint = false;
            self.deadline_ms = None;
        }
    }

    /// Hide the hint and drop any pending deadline, e.g. when the prompt goes away.
    pub fn cleanup(&mut self) {
        self.show_hint = false;
        self.deadline_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_visible_icon_shows_hint_once() {
        let mut state = FastIconHintState::default();
        state.on_icon_visibility_change(true, 100);
        assert!(state.show_hint);
        assert!(state.has_shown_this_session);
    }

    #[test]
    fn second_visibility_change_same_session_is_ignored() {
        let mut state = FastIconHintState::default();
        state.on_icon_visibility_change(true, 100);
        state.cleanup();
        state.on_icon_visibility_change(true, 200);
        assert!(!state.show_hint);
    }

    #[test]
    fn hidden_icon_does_not_start_hint() {
        let mut state = FastIconHintState::default();
        state.on_icon_visibility_change(false, 100);
        assert!(!state.show_hint);
        assert!(!state.has_shown_this_session);
    }

    #[test]
    fn tick_hides_after_timeout() {
        let mut state = FastIconHintState::default();
        state.on_icon_visibility_change(true, 100);
        state.tick(100 + HINT_DISPLAY_DURATION_MS - 1);
        assert!(state.show_hint);
        state.tick(100 + HINT_DISPLAY_DURATION_MS);
        assert!(!state.show_hint);
    }

    #[test]
    fn cleanup_hides_immediately() {
        let mut state = FastIconHintState::default();
        state.on_icon_visibility_change(true, 100);
        state.cleanup();
        assert!(!state.show_hint);
    }
}
