//! The "thinking…" status reducer for spinner rows.
//!
//! Two minimum-display rules apply: "show 'thinking' for at least 2s",
//! then "show 'thought for Ns' for 2s" before clearing.
//!
//! Lifecycle: the consumer owns a [`ThinkingStatusReducer`] and feeds it
//! monotonic time + mode changes via [`ThinkingStatusReducer::on_event`]. The
//! reducer returns the current status to display.

/// The current value displayed in the spinner status line: the word
/// "thinking", a duration in milliseconds, or nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingStatus {
    /// Show "thinking…".
    Thinking,
    /// Show "thought for Ns" with this duration in milliseconds.
    Thought {
        /// Duration of the most recent thinking phase, in ms.
        duration_ms: u64,
    },
    /// No status displayed.
    None,
}

/// The events the reducer reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingStatusEvent {
    /// The spinner mode is now thinking.
    EnterThinking,
    /// The spinner mode left thinking.
    LeaveThinking,
    /// A clock tick (no mode change). Drives the 2s timers.
    Tick,
}

/// The minimum display time, pinned by the tests below. Both the
/// "show thinking for at least 2s" gate and the "show duration for 2s"
/// auto-clear use the same value.
const MIN_DISPLAY_MS: u64 = 2000;

/// The reducer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingStatusReducer {
    status: ThinkingStatus,
    /// `Some` while we're in or just exited the thinking phase. Holds
    /// the absolute monotonic time when the thinking phase began.
    thinking_start: Option<u64>,
    /// When the most recent duration is being displayed, this is the
    /// time at which the auto-clear should fire.
    clear_at: Option<u64>,
    /// When the duration display is delayed because the thinking
    /// phase didn't last 2 seconds, this is the time at which the
    /// duration should appear.
    show_duration_at: Option<u64>,
    /// The duration to show once `show_duration_at` fires.
    pending_duration_ms: u64,
}

impl Default for ThinkingStatusReducer {
    fn default() -> Self {
        Self {
            status: ThinkingStatus::None,
            thinking_start: None,
            clear_at: None,
            show_duration_at: None,
            pending_duration_ms: 0,
        }
    }
}

impl ThinkingStatusReducer {
    /// Construct a fresh reducer with [`ThinkingStatus::None`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently displayed status.
    pub fn current(&self) -> ThinkingStatus {
        self.status
    }

    /// Process an event at `now_ms`. Returns the (possibly updated)
    /// status.
    pub fn on_event(&mut self, event: ThinkingStatusEvent, now_ms: u64) -> ThinkingStatus {
        match event {
            ThinkingStatusEvent::EnterThinking => {
                if self.thinking_start.is_none() {
                    self.thinking_start = Some(now_ms);
                    self.status = ThinkingStatus::Thinking;
                    self.clear_at = None;
                    self.show_duration_at = None;
                    self.pending_duration_ms = 0;
                }
            }
            ThinkingStatusEvent::LeaveThinking => {
                if let Some(start) = self.thinking_start {
                    let elapsed = now_ms.saturating_sub(start);
                    let duration = elapsed;
                    let remaining = MIN_DISPLAY_MS.saturating_sub(elapsed);
                    self.thinking_start = None;
                    self.pending_duration_ms = duration;
                    if remaining > 0 {
                        // Need to keep "thinking" visible for the
                        // remaining time, then flip to the duration.
                        self.show_duration_at = Some(now_ms + remaining);
                    } else {
                        // Show duration immediately.
                        self.status = ThinkingStatus::Thought {
                            duration_ms: duration,
                        };
                        self.clear_at = Some(now_ms + MIN_DISPLAY_MS);
                        self.show_duration_at = None;
                    }
                }
            }
            ThinkingStatusEvent::Tick => {
                // Drive timers — no state change.
            }
        }
        // Drive timers regardless of which event we just processed.
        if let Some(at) = self.show_duration_at {
            if now_ms >= at {
                self.status = ThinkingStatus::Thought {
                    duration_ms: self.pending_duration_ms,
                };
                self.clear_at = Some(now_ms + MIN_DISPLAY_MS);
                self.show_duration_at = None;
            }
        }
        if let Some(at) = self.clear_at {
            if now_ms >= at {
                self.status = ThinkingStatus::None;
                self.clear_at = None;
            }
        }
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_is_none() {
        let r = ThinkingStatusReducer::new();
        assert_eq!(r.current(), ThinkingStatus::None);
    }

    #[test]
    fn enter_thinking_sets_thinking() {
        let mut r = ThinkingStatusReducer::new();
        let s = r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        assert_eq!(s, ThinkingStatus::Thinking);
    }

    #[test]
    fn enter_thinking_idempotent() {
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        r.on_event(ThinkingStatusEvent::EnterThinking, 100);
        // Second EnterThinking should be a no-op (the reducer only
        // starts a phase while `thinking_start` is `None`).
        assert_eq!(r.current(), ThinkingStatus::Thinking);
    }

    #[test]
    fn quick_thinking_keeps_displaying_for_two_seconds() {
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        // Leave after 500ms — too short. Status should stay
        // 'thinking' until 2000ms total.
        r.on_event(ThinkingStatusEvent::LeaveThinking, 500);
        assert_eq!(r.current(), ThinkingStatus::Thinking);
        // Tick at 1900 — still thinking.
        let s = r.on_event(ThinkingStatusEvent::Tick, 1900);
        assert_eq!(s, ThinkingStatus::Thinking);
        // Tick at 2000 — flips to duration.
        let s = r.on_event(ThinkingStatusEvent::Tick, 2000);
        assert_eq!(s, ThinkingStatus::Thought { duration_ms: 500 });
    }

    #[test]
    fn long_thinking_immediately_shows_duration() {
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        // Leave after 5s — well past the minimum.
        let s = r.on_event(ThinkingStatusEvent::LeaveThinking, 5000);
        assert_eq!(s, ThinkingStatus::Thought { duration_ms: 5000 });
    }

    #[test]
    fn duration_clears_after_two_seconds() {
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        r.on_event(ThinkingStatusEvent::LeaveThinking, 5000);
        // 2s tick → cleared.
        let s = r.on_event(ThinkingStatusEvent::Tick, 7000);
        assert_eq!(s, ThinkingStatus::None);
    }

    #[test]
    fn duration_persists_until_clear_at() {
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        r.on_event(ThinkingStatusEvent::LeaveThinking, 5000);
        let s = r.on_event(ThinkingStatusEvent::Tick, 6500);
        assert_eq!(s, ThinkingStatus::Thought { duration_ms: 5000 });
    }

    #[test]
    fn quick_thinking_then_clear_chain() {
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        r.on_event(ThinkingStatusEvent::LeaveThinking, 500);
        // At 2000 → flip to duration.
        let s = r.on_event(ThinkingStatusEvent::Tick, 2000);
        assert_eq!(s, ThinkingStatus::Thought { duration_ms: 500 });
        // At 4000 → cleared.
        let s = r.on_event(ThinkingStatusEvent::Tick, 4000);
        assert_eq!(s, ThinkingStatus::None);
    }

    #[test]
    fn re_enter_thinking_after_clear() {
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        r.on_event(ThinkingStatusEvent::LeaveThinking, 5000);
        r.on_event(ThinkingStatusEvent::Tick, 7000);
        assert_eq!(r.current(), ThinkingStatus::None);
        // New thinking phase.
        let s = r.on_event(ThinkingStatusEvent::EnterThinking, 8000);
        assert_eq!(s, ThinkingStatus::Thinking);
    }

    #[test]
    fn leave_without_enter_no_op() {
        let mut r = ThinkingStatusReducer::new();
        let s = r.on_event(ThinkingStatusEvent::LeaveThinking, 100);
        assert_eq!(s, ThinkingStatus::None);
    }

    #[test]
    fn rapid_jitter_within_two_seconds_keeps_thinking() {
        // Compatibility corner case: the parent re-renders with the mode
        // oscillating between thinking and non-thinking three times
        // within the 2s minimum-display window. Each EnterThinking is
        // idempotent (the existing start timestamp stays set), so the
        // *first* thinking_start anchors the entire 2s gate.
        let mut r = ThinkingStatusReducer::new();

        // First entry at t=0.
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        assert_eq!(r.current(), ThinkingStatus::Thinking);

        // Leave at 200ms — the 2s minimum-display gate should keep
        // 'thinking' on screen until 2000ms total.
        r.on_event(ThinkingStatusEvent::LeaveThinking, 200);
        assert_eq!(r.current(), ThinkingStatus::Thinking);

        // Re-enter at 400ms while the gate is still pending. The
        // `thinking_start.is_none()` guard means EnterThinking starts a
        // *new* phase only when the previous one has ended. After
        // LeaveThinking the start was nulled, so a new phase begins
        // here. The pending duration display is also cleared.
        r.on_event(ThinkingStatusEvent::EnterThinking, 400);
        assert_eq!(r.current(), ThinkingStatus::Thinking);

        // Leave again at 600ms — the previous show_duration_at must
        // not survive across the new phase, and a new minimum-display
        // gate must be in flight relative to the *new* start time.
        r.on_event(ThinkingStatusEvent::LeaveThinking, 600);
        assert_eq!(r.current(), ThinkingStatus::Thinking);

        // Tick at 800ms — well within 2s of the second start (400ms);
        // status must still be Thinking.
        let s = r.on_event(ThinkingStatusEvent::Tick, 800);
        assert_eq!(s, ThinkingStatus::Thinking);

        // Re-enter at 1000ms.
        r.on_event(ThinkingStatusEvent::EnterThinking, 1000);
        assert_eq!(r.current(), ThinkingStatus::Thinking);

        // Leave at 1500ms — duration is 500ms, less than the 2s gate.
        r.on_event(ThinkingStatusEvent::LeaveThinking, 1500);
        assert_eq!(r.current(), ThinkingStatus::Thinking);

        // Tick at 2999ms — still inside the 2s gate from the third
        // start (1000ms + 2000ms = 3000ms).
        let s = r.on_event(ThinkingStatusEvent::Tick, 2999);
        assert_eq!(s, ThinkingStatus::Thinking);

        // Tick at 3000ms — gate fires, flips to the duration display
        // for the *most recent* leave. duration = 1500 - 1000 = 500ms.
        let s = r.on_event(ThinkingStatusEvent::Tick, 3000);
        assert_eq!(s, ThinkingStatus::Thought { duration_ms: 500 });
    }

    #[test]
    fn re_enter_during_thought_display_resets_to_thinking() {
        // After the duration display is up, a new thinking phase
        // should clear `clear_at` and re-display 'thinking'. Leaving
        // the thinking mode nulls the clear timer, and the next
        // EnterThinking starts a fresh phase.
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        r.on_event(ThinkingStatusEvent::LeaveThinking, 5000);
        // Now in the Thought display state with clear_at = 7000.
        assert_eq!(r.current(), ThinkingStatus::Thought { duration_ms: 5000 });
        // Re-enter at 6000 (well before clear_at).
        let s = r.on_event(ThinkingStatusEvent::EnterThinking, 6000);
        assert_eq!(s, ThinkingStatus::Thinking);
        // The stale clear_at must NOT fire on the next tick.
        let s = r.on_event(ThinkingStatusEvent::Tick, 7000);
        assert_eq!(s, ThinkingStatus::Thinking);
    }

    #[test]
    fn re_enter_during_pending_show_duration_starts_fresh_phase() {
        // We're in the "thinking is shown but show_duration_at is
        // pending because the previous phase was too short" state. A
        // new EnterThinking event should start a fresh phase and
        // clear the pending show_duration_at so it doesn't fire on a
        // later tick and clobber the new phase.
        let mut r = ThinkingStatusReducer::new();
        r.on_event(ThinkingStatusEvent::EnterThinking, 0);
        r.on_event(ThinkingStatusEvent::LeaveThinking, 500);
        // show_duration_at = 2000, status still Thinking.
        assert_eq!(r.current(), ThinkingStatus::Thinking);
        // Re-enter at 1000 — fresh phase begins.
        r.on_event(ThinkingStatusEvent::EnterThinking, 1000);
        // Tick at 2000 — the stale show_duration_at would fire here
        // if it weren't cleared, flipping to Thought; we want it to
        // stay Thinking.
        let s = r.on_event(ThinkingStatusEvent::Tick, 2000);
        assert_eq!(s, ThinkingStatus::Thinking);
    }
}
