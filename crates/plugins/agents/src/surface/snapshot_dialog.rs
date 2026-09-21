//! Agent snapshot-update dialog — pinned default action constant.
//!
//! Opening the dialog immediately produces the default keep action
//! and renders nothing — the dialog is currently a no-op stub. The
//! module pins the default action so the consumer can reproduce the
//! behaviour without re-deriving it.

/// The user's choice in the snapshot-update dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapshotChoice {
    /// Merge the project snapshot into the local copy.
    Merge,
    /// Keep the local copy as-is.
    Keep,
    /// Replace the local copy with the project snapshot.
    Replace,
}

impl SnapshotChoice {
    /// Lowercase name of the choice.
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotChoice::Merge => "merge",
            SnapshotChoice::Keep => "keep",
            SnapshotChoice::Replace => "replace",
        }
    }
}

/// The default action — what the dialog auto-completes with today.
pub const SNAPSHOT_DIALOG_DEFAULT_CHOICE: SnapshotChoice = SnapshotChoice::Keep;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_choice_is_keep() {
        assert_eq!(SNAPSHOT_DIALOG_DEFAULT_CHOICE, SnapshotChoice::Keep);
    }

    #[test]
    fn choice_strings() {
        assert_eq!(SnapshotChoice::Merge.as_str(), "merge");
        assert_eq!(SnapshotChoice::Keep.as_str(), "keep");
        assert_eq!(SnapshotChoice::Replace.as_str(), "replace");
    }
}
