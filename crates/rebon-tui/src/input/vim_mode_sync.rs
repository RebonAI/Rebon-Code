//! Vim initial-mode sync predicate.
//!
//! The caller calls `set_mode(initial_mode)` whenever
//! `initial_mode` is provided and differs from the current mode. This
//! module exposes that decision as a pure predicate; the caller owns
//! the state slot and performs the actual `set_mode` call.
//!
//! The vim-mode state machine itself (normal / insert / replace
//! modes, operator-pending state, visual mode, and the rest) is out
//! of scope here; only the external sync decision is implemented.
//!
//! Only two modes are represented: `VimMode::Insert` and
//! `VimMode::Normal`.

/// The vim input modes: only INSERT and NORMAL are represented. The
/// larger vim state machine (visual, replace, operator-pending) is
/// not implemented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VimMode {
    /// Insert mode — characters type into the buffer.
    Insert,
    /// Normal mode — keys are motions / operators.
    Normal,
}

impl VimMode {
    /// Canonical uppercase label for this mode.
    pub const fn as_label(self) -> &'static str {
        match self {
            VimMode::Insert => "INSERT",
            VimMode::Normal => "NORMAL",
        }
    }
}

/// Should the consumer call `set_mode(initial_mode)` right now?
///
/// Returns true iff:
///
/// 1. `initial_mode` is `Some`.
/// 2. `initial_mode != Some(current_mode)`.
///
/// Returns false when `initial_mode` is `None` (no initial mode was
/// provided) or when it already matches the current mode.
pub fn needs_mode_sync(initial_mode: Option<VimMode>, current_mode: VimMode) -> bool {
    match initial_mode {
        Some(m) => m != current_mode,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{needs_mode_sync, VimMode};

    // ------- needs_mode_sync predicate -----------------------------------

    #[test]
    fn no_initial_mode_returns_false() {
        assert!(!needs_mode_sync(None, VimMode::Insert));
        assert!(!needs_mode_sync(None, VimMode::Normal));
    }

    #[test]
    fn initial_matches_current_returns_false() {
        assert!(!needs_mode_sync(Some(VimMode::Insert), VimMode::Insert));
        assert!(!needs_mode_sync(Some(VimMode::Normal), VimMode::Normal));
    }

    #[test]
    fn initial_differs_from_current_returns_true() {
        assert!(needs_mode_sync(Some(VimMode::Normal), VimMode::Insert));
        assert!(needs_mode_sync(Some(VimMode::Insert), VimMode::Normal));
    }

    // ------- VimMode enum pinning ----------------------------------------

    #[test]
    fn vim_mode_literals_are_expected() {
        assert_eq!(VimMode::Insert.as_label(), "INSERT");
        assert_eq!(VimMode::Normal.as_label(), "NORMAL");
    }

    #[test]
    fn vim_modes_are_distinct() {
        assert_ne!(VimMode::Insert, VimMode::Normal);
    }

    #[test]
    fn vim_mode_is_copy() {
        // Compile-time canary that VimMode implements Copy. If a
        // future refactor makes it non-Copy the consumer has to
        // start cloning and that's a breaking change.
        fn take_copy<T: Copy>(_: T) {}
        take_copy(VimMode::Insert);
    }

    #[test]
    fn vim_mode_hash_eq() {
        let mut set = HashSet::new();
        set.insert(VimMode::Insert);
        set.insert(VimMode::Normal);
        set.insert(VimMode::Insert); // dup
        assert_eq!(set.len(), 2);
    }
}
