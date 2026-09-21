//! Pure reducer for the focused-toggle state machine.
//!
//! ## Behavior notes
//!
//! Two toggles (auto-memory at index 0, auto-dream at index 1) sit
//! above a selector. This reducer models focus moving between the
//! selector area and the toggles:
//!
//! * [`FocusState::focused`] is the focused toggle, or `None` for the
//!   selector area.
//! * [`FocusState::auto_memory_on`] / [`FocusState::auto_dream_on`]
//!   hold the two toggle values.
//! * [`FocusState::show_dream_row`] records whether the auto-dream row
//!   exists at all.
//! * `last_toggle_index` is `1` when `show_dream_row` is set, else `0`.
//!
//! The events map onto the four keybindings plus the selector's
//! "up from first item" callback:
//!
//! * `select:next` — move to the next toggle, or exit to the selector
//!   area when already at the last one.
//! * `select:previous` — move to the previous toggle, or stay on the
//!   first one.
//! * `confirm:yes` — toggle whichever toggle is focused (fires only
//!   when a toggle is focused).
//! * `confirm:no` — cancel the entire selector (wired regardless of
//!   focus).
//! * up-from-first-item — enter the toggle area at the bottom toggle.
//!
//! Six load-bearing details:
//!
//! 1. **`focused == None`** is the "selector area is active"
//! state. Pressing `confirm:no` always cancels the entire
//! selector (handled by the `Cancel` event, which is wired
//! regardless of focus).
//! 2. **`select:previous` from inside toggles never exits.** The
//! reducer keeps you inside the toggle area when at index 0.
//! 3. **`select:next` from the *last* toggle exits to the selector
//! area.** The reducer returns `None` once at the last index.
//! 4. **`confirm:yes` only fires when a toggle is focused.** When
//! fired, it dispatches to the appropriate toggle
//! handler.
//! 5. **Up-from-first-item** (delivered when the user presses up from
//! the first option) sets focus
//! to `last_toggle_index` — i.e. the BOTTOM toggle. Counter-
//! intuitive but matches the visual layout: the toggles sit
//! above the selector, so pressing "up" enters the toggle area
//! from below.
//! 6. **`show_dream_row`** is captured once at construction from
//! `auto_memory_enabled`. It does NOT reactively
//! update when `auto_memory_on` toggles. The crate carries it as a
//! separate field on [`FocusState`] that
//! the consumer initialises once and never mutates.
//!
//! This is a pure reducer: the [`FocusReducerOutcome`] return
//! value carries the new state plus the side effects the caller is
//! responsible for applying.

/// One of the two toggles, addressed by its `0` / `1` index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToggleSlot {
    /// Auto-memory toggle (index 0).
    AutoMemory,
    /// Auto-dream toggle (index 1). Only present when
    /// [`FocusState::show_dream_row`] is `true`.
    AutoDream,
}

impl ToggleSlot {
    /// 0-based index of this slot.
    pub const fn index(self) -> usize {
        match self {
            Self::AutoMemory => 0,
            Self::AutoDream => 1,
        }
    }

    /// Inverse of [`Self::index`]. Returns `None` for indices that
    /// don't map to a known slot.
    pub const fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(Self::AutoMemory),
            1 => Some(Self::AutoDream),
            _ => None,
        }
    }
}

/// Full state of the focused-toggle subsystem at one point in time.
///
/// The subsystem state at one point in time:
/// * `focused` — the focused toggle, or `None` for the selector area
/// * `auto_memory_on` — current auto-memory value
/// * `auto_dream_on` — current auto-dream value
/// * `show_dream_row` — captured once from the auto-memory setting
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FocusState {
    /// Currently focused toggle, or `None` for "selector area".
    pub focused: Option<ToggleSlot>,
    /// Auto-memory toggle current value.
    pub auto_memory_on: bool,
    /// Auto-dream toggle current value.
    pub auto_dream_on: bool,
    /// Whether the auto-dream row is shown at all. Captured once at
    /// construction from `auto_memory_enabled` — does not change
    /// afterwards.
    pub show_dream_row: bool,
}

impl FocusState {
    /// Construct an initial state. `auto_memory_enabled` seeds both
    /// `auto_memory_on` and `show_dream_row`; `auto_dream_enabled`
    /// seeds `auto_dream_on`.
    pub fn new(auto_memory_enabled: bool, auto_dream_enabled: bool) -> Self {
        Self {
            focused: None,
            auto_memory_on: auto_memory_enabled,
            auto_dream_on: auto_dream_enabled,
            show_dream_row: auto_memory_enabled,
        }
    }

    /// The largest index a toggle can have given the current
    /// `show_dream_row` value: `1` when it is set, otherwise `0`.
    pub const fn last_toggle_index(self) -> usize {
        if self.show_dream_row {
            1
        } else {
            0
        }
    }

    /// Whether a toggle is currently focused.
    pub const fn toggle_focused(self) -> bool {
        self.focused.is_some()
    }
}

/// Inputs to [`reduce`]: the four keybinding actions plus the
/// selector's up-from-first-item callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusEvent {
    /// Moves to the next toggle, or exits the toggle
    /// area if already at the last one.
    SelectNext,
    /// Moves to the previous toggle, or stays at the
    /// first one if already there.
    SelectPrevious,
    /// Toggles whichever toggle is currently focused.
    ConfirmYes,
    /// Cancels the entire selector (regardless of focus).
    ConfirmNo,
    /// Pressing up from the first option in the selector enters
    /// the toggle area at the bottom toggle.
    UpFromFirstSelectItem,
}

/// One side-effect emitted by the reducer. The consumer pattern-
/// matches on these and forwards them to the appropriate sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusEffect {
    /// Persist the new auto-memory value to the `autoMemoryEnabled`
    /// user setting.
    PersistAutoMemory(bool),
    /// Persist the new auto-dream value to the user's settings.
    PersistAutoDream(bool),
    /// Cancel the entire selector.
    CancelSelector,
}

/// Result of a single [`reduce`] call. Pure data — the consumer
/// applies the new state and runs each effect in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusReducerOutcome {
    /// New state after the event was applied.
    pub state: FocusState,
    /// Side effects to run, in order.
    pub effects: Vec<FocusEffect>,
}

/// Pure reducer for the four keybinding actions plus the selector's
/// up-from-first-item callback.
pub fn reduce(prev: FocusState, event: FocusEvent) -> FocusReducerOutcome {
    let mut state = prev;
    let mut effects = Vec::new();

    match event {
        // confirm:no is wired unconditionally (no focus gate).
        // Always cancels.
        FocusEvent::ConfirmNo => {
            effects.push(FocusEffect::CancelSelector);
        }

        // confirm:yes is gated on a toggle being focused. When
        // fired, it dispatches to the focused toggle.
        FocusEvent::ConfirmYes => {
            if let Some(slot) = state.focused {
                match slot {
                    ToggleSlot::AutoMemory => {
                        let new_value = !state.auto_memory_on;
                        state.auto_memory_on = new_value;
                        effects.push(FocusEffect::PersistAutoMemory(new_value));
                    }
                    ToggleSlot::AutoDream => {
                        let new_value = !state.auto_dream_on;
                        state.auto_dream_on = new_value;
                        effects.push(FocusEffect::PersistAutoDream(new_value));
                    }
                }
            }
            // No-op when no toggle is focused.
        }

        // select:next is gated on a toggle being focused.
        // Moves to the next index while below `last_toggle_index`,
        // otherwise clears the focus.
        FocusEvent::SelectNext => {
            if let Some(slot) = state.focused {
                let prev_idx = slot.index();
                let last = state.last_toggle_index();
                state.focused = if prev_idx < last {
                    ToggleSlot::from_index(prev_idx + 1)
                } else {
                    None
                };
            }
            // No-op when not focused.
        }

        // select:previous is gated on a toggle being focused.
        // Moves to the previous index while above 0, otherwise stays.
        FocusEvent::SelectPrevious => {
            if let Some(slot) = state.focused {
                let prev_idx = slot.index();
                if prev_idx > 0 {
                    state.focused = ToggleSlot::from_index(prev_idx - 1);
                }
                // else: stays at index 0 — does NOT exit to None.
            }
            // No-op when not focused.
        }

        // Up-from-first-item sets focus to the last toggle index.
        FocusEvent::UpFromFirstSelectItem => {
            let last = state.last_toggle_index();
            state.focused = ToggleSlot::from_index(last);
        }
    }

    FocusReducerOutcome { state, effects }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> FocusState {
        // auto_memory_enabled = true → show_dream_row = true,
        // auto_memory_on = true. auto_dream_enabled = false.
        FocusState::new(true, false)
    }

    fn baseline_no_dream() -> FocusState {
        // auto_memory_enabled = false → show_dream_row = false.
        FocusState::new(false, false)
    }

    // ──────────────────────────────────────────────────────────────
    // ToggleSlot
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn auto_memory_index_is_zero() {
        assert_eq!(ToggleSlot::AutoMemory.index(), 0);
    }

    #[test]
    fn auto_dream_index_is_one() {
        assert_eq!(ToggleSlot::AutoDream.index(), 1);
    }

    #[test]
    fn from_index_round_trips() {
        assert_eq!(ToggleSlot::from_index(0), Some(ToggleSlot::AutoMemory));
        assert_eq!(ToggleSlot::from_index(1), Some(ToggleSlot::AutoDream));
        assert_eq!(ToggleSlot::from_index(2), None);
    }

    // ──────────────────────────────────────────────────────────────
    // FocusState construction
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn new_starts_with_none_focus() {
        let s = FocusState::new(true, true);
        assert_eq!(s.focused, None);
    }

    #[test]
    fn new_uses_auto_memory_for_show_dream_row() {
        let s = FocusState::new(true, false);
        assert!(s.show_dream_row);
        let s = FocusState::new(false, false);
        assert!(!s.show_dream_row);
    }

    #[test]
    fn last_toggle_index_with_dream_row_is_1() {
        let s = baseline();
        assert_eq!(s.last_toggle_index(), 1);
    }

    #[test]
    fn last_toggle_index_without_dream_row_is_0() {
        let s = baseline_no_dream();
        assert_eq!(s.last_toggle_index(), 0);
    }

    #[test]
    fn toggle_focused_predicate() {
        let s = baseline();
        assert!(!s.toggle_focused());
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            ..s
        };
        assert!(s.toggle_focused());
    }

    // ──────────────────────────────────────────────────────────────
    // ConfirmNo
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn confirm_no_cancels_when_unfocused() {
        let s = baseline();
        let outcome = reduce(s, FocusEvent::ConfirmNo);
        assert_eq!(outcome.effects, vec![FocusEffect::CancelSelector]);
        assert_eq!(outcome.state, s);
    }

    #[test]
    fn confirm_no_cancels_when_focused_on_auto_memory() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            ..baseline()
        };
        let outcome = reduce(s, FocusEvent::ConfirmNo);
        assert_eq!(outcome.effects, vec![FocusEffect::CancelSelector]);
        assert_eq!(outcome.state, s);
    }

    #[test]
    fn confirm_no_cancels_when_focused_on_auto_dream() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoDream),
            ..baseline()
        };
        let outcome = reduce(s, FocusEvent::ConfirmNo);
        assert_eq!(outcome.effects, vec![FocusEffect::CancelSelector]);
    }

    // ──────────────────────────────────────────────────────────────
    // ConfirmYes
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn confirm_yes_when_unfocused_is_noop() {
        let s = baseline();
        let outcome = reduce(s, FocusEvent::ConfirmYes);
        assert_eq!(outcome.state, s);
        assert!(outcome.effects.is_empty());
    }

    #[test]
    fn confirm_yes_on_auto_memory_flips_value() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            auto_memory_on: true,
            auto_dream_on: false,
            show_dream_row: true,
        };
        let outcome = reduce(s, FocusEvent::ConfirmYes);
        assert!(!outcome.state.auto_memory_on);
        // auto_dream untouched
        assert!(!outcome.state.auto_dream_on);
        assert_eq!(outcome.effects, vec![FocusEffect::PersistAutoMemory(false)]);
    }

    #[test]
    fn confirm_yes_on_auto_memory_flips_off_to_on() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            auto_memory_on: false,
            auto_dream_on: false,
            show_dream_row: true,
        };
        let outcome = reduce(s, FocusEvent::ConfirmYes);
        assert!(outcome.state.auto_memory_on);
        assert_eq!(outcome.effects, vec![FocusEffect::PersistAutoMemory(true)]);
    }

    #[test]
    fn confirm_yes_on_auto_dream_flips_value() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoDream),
            auto_memory_on: true,
            auto_dream_on: false,
            show_dream_row: true,
        };
        let outcome = reduce(s, FocusEvent::ConfirmYes);
        assert!(outcome.state.auto_dream_on);
        // auto_memory untouched
        assert!(outcome.state.auto_memory_on);
        assert_eq!(outcome.effects, vec![FocusEffect::PersistAutoDream(true)]);
    }

    #[test]
    fn confirm_yes_does_not_change_focus() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            ..baseline()
        };
        let outcome = reduce(s, FocusEvent::ConfirmYes);
        assert_eq!(outcome.state.focused, Some(ToggleSlot::AutoMemory));
    }

    // ──────────────────────────────────────────────────────────────
    // SelectNext
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn select_next_when_unfocused_is_noop() {
        let s = baseline();
        let outcome = reduce(s, FocusEvent::SelectNext);
        assert_eq!(outcome.state, s);
        assert!(outcome.effects.is_empty());
    }

    #[test]
    fn select_next_from_auto_memory_advances_to_auto_dream_when_dream_row_shown() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            ..baseline()
        };
        let outcome = reduce(s, FocusEvent::SelectNext);
        assert_eq!(outcome.state.focused, Some(ToggleSlot::AutoDream));
    }

    #[test]
    fn select_next_from_auto_memory_exits_when_dream_row_hidden() {
        // show_dream_row=false → last_toggle_index=0 → already at
        // last → exit to None.
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            ..baseline_no_dream()
        };
        let outcome = reduce(s, FocusEvent::SelectNext);
        assert_eq!(outcome.state.focused, None);
    }

    #[test]
    fn select_next_from_auto_dream_exits_to_selector() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoDream),
            ..baseline()
        };
        let outcome = reduce(s, FocusEvent::SelectNext);
        assert_eq!(outcome.state.focused, None);
    }

    // ──────────────────────────────────────────────────────────────
    // SelectPrevious
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn select_previous_when_unfocused_is_noop() {
        let s = baseline();
        let outcome = reduce(s, FocusEvent::SelectPrevious);
        assert_eq!(outcome.state, s);
        assert!(outcome.effects.is_empty());
    }

    #[test]
    fn select_previous_from_auto_memory_stays_at_auto_memory() {
        // Stays at index 0. Does NOT exit to None.
        let s = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            ..baseline()
        };
        let outcome = reduce(s, FocusEvent::SelectPrevious);
        assert_eq!(outcome.state.focused, Some(ToggleSlot::AutoMemory));
    }

    #[test]
    fn select_previous_from_auto_dream_moves_to_auto_memory() {
        let s = FocusState {
            focused: Some(ToggleSlot::AutoDream),
            ..baseline()
        };
        let outcome = reduce(s, FocusEvent::SelectPrevious);
        assert_eq!(outcome.state.focused, Some(ToggleSlot::AutoMemory));
    }

    // ──────────────────────────────────────────────────────────────
    // UpFromFirstSelectItem
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn up_from_first_select_item_with_dream_row_focuses_auto_dream() {
        // last_toggle_index = 1 (dream row shown) → focus dream
        let s = baseline();
        let outcome = reduce(s, FocusEvent::UpFromFirstSelectItem);
        assert_eq!(outcome.state.focused, Some(ToggleSlot::AutoDream));
    }

    #[test]
    fn up_from_first_select_item_without_dream_row_focuses_auto_memory() {
        // last_toggle_index = 0 (no dream row) → focus auto-memory
        let s = baseline_no_dream();
        let outcome = reduce(s, FocusEvent::UpFromFirstSelectItem);
        assert_eq!(outcome.state.focused, Some(ToggleSlot::AutoMemory));
    }

    #[test]
    fn up_from_first_select_item_emits_no_effects() {
        let s = baseline();
        let outcome = reduce(s, FocusEvent::UpFromFirstSelectItem);
        assert!(outcome.effects.is_empty());
    }

    // ──────────────────────────────────────────────────────────────
    // Multi-step flow
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn full_navigation_flow() {
        // Start: unfocused, dream row shown
        let s0 = baseline();
        // Press up from first selector item → focuses bottom toggle
        let s1 = reduce(s0, FocusEvent::UpFromFirstSelectItem).state;
        assert_eq!(s1.focused, Some(ToggleSlot::AutoDream));
        // Press select:previous → up to auto-memory
        let s2 = reduce(s1, FocusEvent::SelectPrevious).state;
        assert_eq!(s2.focused, Some(ToggleSlot::AutoMemory));
        // Press select:previous again → stays at auto-memory
        let s3 = reduce(s2, FocusEvent::SelectPrevious).state;
        assert_eq!(s3.focused, Some(ToggleSlot::AutoMemory));
        // Press select:next → down to auto-dream
        let s4 = reduce(s3, FocusEvent::SelectNext).state;
        assert_eq!(s4.focused, Some(ToggleSlot::AutoDream));
        // Press select:next again → exits to selector area
        let s5 = reduce(s4, FocusEvent::SelectNext).state;
        assert_eq!(s5.focused, None);
        // Press select:next once more → no-op (already unfocused)
        let s6 = reduce(s5, FocusEvent::SelectNext).state;
        assert_eq!(s6.focused, None);
    }

    #[test]
    fn full_toggle_flow_with_persistence() {
        // Start: focused on auto-memory, both off
        let s0 = FocusState {
            focused: Some(ToggleSlot::AutoMemory),
            auto_memory_on: false,
            auto_dream_on: false,
            show_dream_row: true,
        };
        // Toggle on
        let o1 = reduce(s0, FocusEvent::ConfirmYes);
        assert!(o1.state.auto_memory_on);
        assert!(!o1.state.auto_dream_on);
        // Move to auto-dream
        let o2 = reduce(o1.state, FocusEvent::SelectNext);
        assert_eq!(o2.state.focused, Some(ToggleSlot::AutoDream));
        // Toggle dream on
        let o3 = reduce(o2.state, FocusEvent::ConfirmYes);
        assert!(o3.state.auto_memory_on);
        assert!(o3.state.auto_dream_on);
        assert_eq!(o3.effects, vec![FocusEffect::PersistAutoDream(true)]);
        // Cancel
        let o4 = reduce(o3.state, FocusEvent::ConfirmNo);
        assert_eq!(o4.effects, vec![FocusEffect::CancelSelector]);
    }

    #[test]
    fn focus_navigation_table() {
        // (initial_focus, event, show_dream_row) → expected_focus
        struct Case {
            from: Option<ToggleSlot>,
            event: FocusEvent,
            dream: bool,
            to: Option<ToggleSlot>,
        }
        let cases = [
            // Unfocused → no-op for nav events
            Case {
                from: None,
                event: FocusEvent::SelectNext,
                dream: true,
                to: None,
            },
            Case {
                from: None,
                event: FocusEvent::SelectPrevious,
                dream: true,
                to: None,
            },
            // UpFromFirstSelectItem with/without dream row
            Case {
                from: None,
                event: FocusEvent::UpFromFirstSelectItem,
                dream: true,
                to: Some(ToggleSlot::AutoDream),
            },
            Case {
                from: None,
                event: FocusEvent::UpFromFirstSelectItem,
                dream: false,
                to: Some(ToggleSlot::AutoMemory),
            },
            // SelectNext from auto-memory
            Case {
                from: Some(ToggleSlot::AutoMemory),
                event: FocusEvent::SelectNext,
                dream: true,
                to: Some(ToggleSlot::AutoDream),
            },
            Case {
                from: Some(ToggleSlot::AutoMemory),
                event: FocusEvent::SelectNext,
                dream: false,
                to: None,
            },
            // SelectNext from auto-dream → always exits
            Case {
                from: Some(ToggleSlot::AutoDream),
                event: FocusEvent::SelectNext,
                dream: true,
                to: None,
            },
            // SelectPrevious from auto-memory → stays
            Case {
                from: Some(ToggleSlot::AutoMemory),
                event: FocusEvent::SelectPrevious,
                dream: true,
                to: Some(ToggleSlot::AutoMemory),
            },
            // SelectPrevious from auto-dream → moves up
            Case {
                from: Some(ToggleSlot::AutoDream),
                event: FocusEvent::SelectPrevious,
                dream: true,
                to: Some(ToggleSlot::AutoMemory),
            },
            // UpFromFirstSelectItem from already-focused → re-focuses
            Case {
                from: Some(ToggleSlot::AutoMemory),
                event: FocusEvent::UpFromFirstSelectItem,
                dream: true,
                to: Some(ToggleSlot::AutoDream),
            },
        ];

        for c in cases {
            let s = FocusState {
                focused: c.from,
                auto_memory_on: false,
                auto_dream_on: false,
                show_dream_row: c.dream,
            };
            let outcome = reduce(s, c.event);
            assert_eq!(
                outcome.state.focused, c.to,
                "from={:?} event={:?} dream={}",
                c.from, c.event, c.dream
            );
        }
    }
}
