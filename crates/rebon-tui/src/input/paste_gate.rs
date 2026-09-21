//! Paste-Return gate.
//!
//! The guard is a single predicate: during a bracketed-paste in
//! progress, swallow any keyboard event whose `key.return` is true.
//! This prevents a pasted newline from triggering submit mid-paste.
//!
//! The bracketed-paste decoder and the `is_pasting` timer that drive
//! this gate are out of scope here; this module just exposes the pure
//! predicate the input shell wraps around them. The caller owns the
//! `is_pasting` state slot and passes its resolved value in, so there
//! is no closure-capture timing to reason about here.

/// A minimal projection of a keyboard event: just the `Return` /
/// `Enter` flag the paste gate inspects. Every other key field is the
/// consumer's concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PasteGateEvent {
    /// Whether this keyboard event is the Return / Enter key.
    pub return_key: bool,
}

/// Should the caller swallow this event?
///
/// Returns true iff a bracketed-paste is currently in progress
/// (`is_pasting == true`) AND the event is the Return key. In every
/// other case the caller should forward the event to the downstream
/// input handler.
pub fn should_swallow_event(is_pasting: bool, event: &PasteGateEvent) -> bool {
    is_pasting && event.return_key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_pasting_forwards_return() {
        assert!(!should_swallow_event(
            false,
            &PasteGateEvent { return_key: true }
        ));
    }

    #[test]
    fn pasting_swallows_return() {
        assert!(should_swallow_event(
            true,
            &PasteGateEvent { return_key: true }
        ));
    }

    #[test]
    fn pasting_forwards_non_return() {
        assert!(!should_swallow_event(
            true,
            &PasteGateEvent { return_key: false }
        ));
    }

    #[test]
    fn not_pasting_forwards_non_return() {
        assert!(!should_swallow_event(
            false,
            &PasteGateEvent { return_key: false }
        ));
    }

    #[test]
    fn default_event_is_non_return() {
        assert!(!should_swallow_event(true, &PasteGateEvent::default()));
        assert!(!should_swallow_event(false, &PasteGateEvent::default()));
    }
}
