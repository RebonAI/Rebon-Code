//! The inputs a dialog is opened with, when strings alone will not do.
//!
//! A panel registered by a plugin is built on the other side of a crate
//! boundary from whatever collected its data, so the two halves need a
//! shape they both name. These are those shapes: plain data, serialized
//! into [`DialogArgs::values`](crate::DialogArgs::values)`[0]` as JSON by
//! the collector and parsed back by the factory.
//!
//! JSON rather than a positional encoding because these inputs are lists
//! of lists: a delimiter scheme would need escaping rules nobody would
//! remember, and a malformed one would open a panel showing the wrong
//! thing rather than failing. A parse failure here declines to open,
//! which the caller already handles.
//!
//! Each type mirrors one state constructor's parameters exactly. Adding a
//! field here without a use on the other side is how they drift.

use serde::{Deserialize, Serialize};

/// One loaded skill, as the selector needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRowInput {
    /// The skill's id, which is what the disabled set records.
    pub id: String,
    /// Where it came from, already labelled (`user`, `project`, `mcp`).
    pub source: String,
    /// Its full description. The selector shortens it for display.
    pub description: String,
}

/// What the `/skills` selector is opened with.
///
/// `checked` is not carried: a row is checked exactly when its id is
/// absent from `disabled`, and a second field saying so could disagree
/// with the first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SkillsDialogInput {
    /// Every skill loaded in this session.
    pub skills: Vec<SkillRowInput>,
    /// The complete persisted disabled set, including ids of skills that
    /// are not loaded right now — Apply has to carry those through.
    pub disabled: Vec<String>,
}

/// What the `/context` browser is opened with.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextDialogInput {
    /// The `/context` command's output. The overview lines and the
    /// per-category summaries are read out of it.
    pub content: String,
    /// One list of entries per category, in the browser's own category
    /// order. A shorter list leaves the remaining categories empty.
    pub category_items: Vec<Vec<String>>,
}

/// Serialize an input for [`DialogArgs::values`](crate::DialogArgs::values)`[0]`.
///
/// Infallible in practice — these are plain data — and an encoding
/// failure yields `""`, which the factory declines to parse.
pub fn encode<T: Serialize>(input: &T) -> String {
    serde_json::to_string(input).unwrap_or_default()
}

/// Parse an input back. `None` for anything that is not the expected
/// shape, which is a factory's cue to decline rather than to panic.
pub fn decode<T: for<'de> Deserialize<'de>>(encoded: &str) -> Option<T> {
    serde_json::from_str(encoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skills_input_round_trips() {
        let input = SkillsDialogInput {
            skills: vec![
                SkillRowInput {
                    id: "review-pr".into(),
                    source: "user".into(),
                    description: "Review a pull request.".into(),
                },
                SkillRowInput {
                    id: "grill".into(),
                    source: "project".into(),
                    description: String::new(),
                },
            ],
            disabled: vec!["grill".into(), "not-loaded".into()],
        };
        assert_eq!(decode::<SkillsDialogInput>(&encode(&input)), Some(input));
    }

    #[test]
    fn a_context_input_round_trips_including_empty_categories() {
        let input = ContextDialogInput {
            content: "Context Usage\n50%\n".into(),
            category_items: vec![vec!["#1 User: hi".into()], Vec::new(), Vec::new()],
        };
        assert_eq!(decode::<ContextDialogInput>(&encode(&input)), Some(input));
    }

    #[test]
    fn an_empty_input_round_trips_to_its_default() {
        assert_eq!(
            decode::<SkillsDialogInput>(&encode(&SkillsDialogInput::default())),
            Some(SkillsDialogInput::default())
        );
    }

    #[test]
    fn garbage_declines_instead_of_panicking() {
        assert_eq!(decode::<SkillsDialogInput>(""), None);
        assert_eq!(decode::<SkillsDialogInput>("not json"), None);
        // Right JSON, wrong shape.
        assert_eq!(decode::<ContextDialogInput>(r#"{"skills":[]}"#), None);
    }
}
