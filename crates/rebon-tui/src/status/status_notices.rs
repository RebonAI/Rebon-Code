//! Startup-notices visibility decision.
//!
//! The consumer evaluates the active notices and feeds their count; the
//! predicate decides whether the notices column is shown at all. The
//! column itself is a vertical stack with a single column of left
//! padding, and hides entirely when the list is empty.
//!
//! Pure logic covered here:
//!
//! 1. The visibility predicate ([`status_notices_visible`]).
//!
//! Notice evaluation itself pulls in 12 notice definitions and is *not*
//! covered here. This crate only exposes the gate.
//!
//! [`StatusNoticesContext`] is also pinned here as a pure data-only
//! struct so the consumer can pre-build it without pulling in full notice
//! evaluation.

/// Pure data view of the notice context. The consumer pre-resolves the
/// relevant config, agent definitions, and memory files, and feeds the
/// count of notices it produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusNoticesContext {
    /// Number of notices that survived the active-notice filter. The
    /// consumer runs the notice evaluation and feeds the length here.
    pub active_notices_len: usize,
}

/// Returns `true` when the notices column should render.
pub fn status_notices_visible(ctx: StatusNoticesContext) -> bool {
    ctx.active_notices_len > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_when_zero_notices() {
        assert!(!status_notices_visible(StatusNoticesContext {
            active_notices_len: 0
        }));
    }

    #[test]
    fn visible_when_one_notice() {
        assert!(status_notices_visible(StatusNoticesContext {
            active_notices_len: 1
        }));
    }

    #[test]
    fn visible_when_many_notices() {
        assert!(status_notices_visible(StatusNoticesContext {
            active_notices_len: 12
        }));
    }
}
