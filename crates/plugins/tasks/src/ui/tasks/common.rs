//! Shared task status / task kind types.
//!
//! * [`TaskStatus`] — the task lifecycle status.
//! * [`TaskKind`] — the background task kind (`local_bash`,
//!   `remote_agent`, `local_agent`, `local_workflow`, `monitor_mcp`,
//!   `dream`, `in_process_teammate`).
//! * [`SemanticColor`] — the design-system color a status renders in.
//! * [`ReviewStage`] — `finding` / `verifying` / `synthesizing`.
//!
//! These types are pure-data discriminants; renderer primitives stay outside
//! this crate.

use core::fmt;

/// Task status discriminant, shared with the task runtime.
///
/// The widgets in this crate render exactly the states the runtime produces,
/// so they read the runtime's own enum rather than a parallel copy.
pub use rebon_types::TaskStatus;

/// Background task kind discriminant, shared with the task runtime.
///
/// There is one such enum, [`crate::runtime::TaskKind`], and these widgets
/// read it. A second copy here drifted: it lacked the runtime's `Monitor`
/// variant, so every snapshot had to be run through a lossy conversion
/// before it could be displayed, and the two spellings of the same kind
/// (`LocalBash` vs `LocalShell`) had to be kept in step by hand.
///
/// `Monitor` and `LocalShell` are both a shell command to a reader, so the
/// projections that group by kind fold them together where it shows —
/// [`crate::ui::tasks::tasks_dialog::build_dialog_layout`]'s bash bucket
/// and the two action gates beside it.
pub use crate::runtime::TaskKind;

/// Semantic color discriminant — the design-system color names
/// `success` / `error` / `warning` / `background`.
///
/// Returned by [`crate::ui::tasks::status_utils::task_status_color`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticColor {
    /// Green — completed.
    Success,
    /// Red — failed.
    Error,
    /// Yellow — warning / killed / shutting down.
    Warning,
    /// Default dim color — running / idle / unknown.
    Background,
}

impl SemanticColor {
    /// The design-system color name.
    pub fn as_str(&self) -> &'static str {
        match self {
            SemanticColor::Success => "success",
            SemanticColor::Error => "error",
            SemanticColor::Warning => "warning",
            SemanticColor::Background => "background",
        }
    }
}

impl fmt::Display for SemanticColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Review stage discriminant for the remote-session ultrareview line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewStage {
    /// Searching for candidate bugs.
    Finding,
    /// Verifying candidate bugs.
    Verifying,
    /// Synthesizing / deduping the verified bugs.
    Synthesizing,
}

impl ReviewStage {
    /// Lowercase stage name; [`ReviewStage::from_str`] parses it back.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewStage::Finding => "finding",
            ReviewStage::Verifying => "verifying",
            ReviewStage::Synthesizing => "synthesizing",
        }
    }

    /// Round-trip parser. Returns `None` for unknown variants.
    pub fn from_str(s: &str) -> Option<ReviewStage> {
        Some(match s {
            "finding" => ReviewStage::Finding,
            "verifying" => ReviewStage::Verifying,
            "synthesizing" => ReviewStage::Synthesizing,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_color_strings() {
        assert_eq!(SemanticColor::Success.as_str(), "success");
        assert_eq!(SemanticColor::Error.as_str(), "error");
        assert_eq!(SemanticColor::Warning.as_str(), "warning");
        assert_eq!(SemanticColor::Background.as_str(), "background");
    }

    #[test]
    fn review_stage_round_trip() {
        for s in ["finding", "verifying", "synthesizing"] {
            assert_eq!(ReviewStage::from_str(s).unwrap().as_str(), s);
        }
        assert!(ReviewStage::from_str("done").is_none());
    }
}
