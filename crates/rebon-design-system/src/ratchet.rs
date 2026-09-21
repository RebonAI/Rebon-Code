/// How the ratchet decides whether its `min_height` applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RatchetLock {
    /// Apply `min_height` unconditionally (the default).
    Always,
    /// Apply `min_height` only while the box is scrolled out of the
    /// terminal viewport.
    Offscreen,
}

impl Default for RatchetLock {
    fn default() -> Self {
        RatchetLock::Always
    }
}

/// Reducer holding the tallest content height seen so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RatchetState {
    /// Largest content height observed so far, already clamped to the
    /// terminal rows that were available at the time.
    pub max_height: u32,
    /// The floor a renderer should honour. Tracks `max_height`.
    pub min_height: u32,
}

impl RatchetState {
    /// Fresh ratchet: nothing measured yet, so both heights are zero.
    pub fn new() -> Self {
        Self {
            max_height: 0,
            min_height: 0,
        }
    }

    /// Feed a measured content height in. The ratchet only grows: a
    /// measurement above the current maximum raises `max_height` and
    /// `min_height`, and a smaller one is ignored.
    ///
    /// The stored maximum is clamped to the available `rows`, so content
    /// taller than the viewport never asks for more room than exists.
    /// Returns `true` when the state moved.
    pub fn observe(&mut self, measured_height: u32, rows: u32) -> bool {
        if measured_height > self.max_height {
            // max_height = min(measured_height, rows)
            self.max_height = measured_height.min(rows);
            self.min_height = self.max_height;
            return true;
        }
        false
    }

    /// True while the ratchet should apply its `min_height`: always under
    /// [`RatchetLock::Always`], and only while the box is not visible under
    /// [`RatchetLock::Offscreen`].
    pub fn engaged(&self, lock: RatchetLock, is_visible: bool) -> bool {
        match lock {
            RatchetLock::Always => true,
            RatchetLock::Offscreen => !is_visible,
        }
    }
}

impl Default for RatchetState {
    fn default() -> Self {
        RatchetState::new()
    }
}

/// The `min_height` a renderer should apply right now, or `None` when the
/// ratchet is disengaged and the box may size itself.
pub fn ratchet_min_height(
    state: &RatchetState,
    lock: RatchetLock,
    is_visible: bool,
) -> Option<u32> {
    if state.engaged(lock, is_visible) {
        Some(state.min_height)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_lock_is_always() {
        assert_eq!(RatchetLock::default(), RatchetLock::Always);
    }

    #[test]
    fn fresh_state_is_zero() {
        let s = RatchetState::new();
        assert_eq!(s.max_height, 0);
        assert_eq!(s.min_height, 0);
    }

    #[test]
    fn observe_smaller_height_does_not_shrink() {
        let mut s = RatchetState::new();
        s.observe(10, 100);
        assert_eq!(s.min_height, 10);
        let changed = s.observe(5, 100);
        assert_eq!(s.min_height, 10);
        assert_eq!(changed, false);
    }

    #[test]
    fn observe_larger_height_grows() {
        let mut s = RatchetState::new();
        s.observe(10, 100);
        let changed = s.observe(20, 100);
        assert!(changed);
        assert_eq!(s.min_height, 20);
        assert_eq!(s.max_height, 20);
    }

    #[test]
    fn observe_clamps_to_rows() {
        let mut s = RatchetState::new();
        // max_height is clamped to rows: min(height, rows)
        s.observe(50, 30);
        assert_eq!(s.max_height, 30);
        assert_eq!(s.min_height, 30);
    }

    #[test]
    fn observe_equal_height_does_not_change() {
        let mut s = RatchetState::new();
        s.observe(10, 100);
        let changed = s.observe(10, 100);
        assert_eq!(changed, false);
    }

    #[test]
    fn engaged_always_mode_is_true_regardless_of_visibility() {
        let s = RatchetState::new();
        assert!(s.engaged(RatchetLock::Always, true));
        assert!(s.engaged(RatchetLock::Always, false));
    }

    #[test]
    fn engaged_offscreen_mode_only_when_not_visible() {
        let s = RatchetState::new();
        assert_eq!(s.engaged(RatchetLock::Offscreen, true), false);
        assert_eq!(s.engaged(RatchetLock::Offscreen, false), true);
    }

    #[test]
    fn ratchet_min_height_returns_none_when_disengaged() {
        let mut s = RatchetState::new();
        s.observe(10, 100);
        let v = ratchet_min_height(&s, RatchetLock::Offscreen, true);
        assert_eq!(v, None);
    }

    #[test]
    fn ratchet_min_height_returns_some_when_engaged() {
        let mut s = RatchetState::new();
        s.observe(10, 100);
        let v = ratchet_min_height(&s, RatchetLock::Always, true);
        assert_eq!(v, Some(10));
    }

    #[test]
    fn growth_sequence() {
        let mut s = RatchetState::new();
        for h in [3, 7, 5, 9, 8, 12, 11] {
            s.observe(h, 100);
        }
        // monotonically nondecreasing maxima: 3, 7, 7, 9, 9, 12, 12
        assert_eq!(s.max_height, 12);
        assert_eq!(s.min_height, 12);
    }
}
