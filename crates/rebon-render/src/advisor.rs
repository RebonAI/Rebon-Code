//! Advisor message projection: the tool-use branch carrying its loader
//! state, the error branch, and the result branch.

use std::collections::HashSet;

/// Loader state for an advisor tool-use row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolUseLoaderDisplay {
    /// True while the tool-use id has not been resolved yet.
    pub is_unresolved: bool,
    /// True when the tool-use id errored.
    pub is_error: bool,
    /// Whether the loader should animate.
    pub should_animate: bool,
}

/// Simplified advisor block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorBlock {
    /// `server_tool_use` block.
    ServerToolUse {
        /// Tool-use id.
        id: String,
        /// Stringified input when non-empty.
        input_json: Option<String>,
    },
    /// `advisor_tool_result_error` block.
    ToolResultError {
        /// Error code.
        error_code: String,
    },
    /// `advisor_result` block.
    Result {
        /// Advisor text.
        text: String,
    },
    /// `advisor_redacted_result` block.
    RedactedResult,
}

/// Projected advisor message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorProjection {
    /// Advising / server-tool-use branch.
    ToolUse {
        /// Whether extra vertical margin surrounds the block.
        add_margin: bool,
        /// Loader projection.
        loader: ToolUseLoaderDisplay,
        /// Optional advisor model display label.
        advisor_model_label: Option<String>,
        /// Optional JSON input string.
        input_json: Option<String>,
    },
    /// Error result branch.
    ResultError {
        /// Error code.
        error_code: String,
    },
    /// Advisor text or redacted summary result.
    Result {
        /// Verbose flag.
        verbose: bool,
        /// Text content.
        text: String,
        /// Whether compact mode shows the expand hint.
        show_expand_hint: bool,
    },
    /// Redacted-result branch.
    RedactedResult,
}

/// Projects one advisor block plus the surrounding flags into a renderable
/// shape: the tool-use branch resolves the loader flags from the id sets,
/// the result branch shows its expand hint only in compact (non-verbose)
/// mode.
pub fn project_advisor_message(
    block: &AdvisorBlock,
    add_margin: bool,
    resolved_tool_use_ids: &HashSet<String>,
    errored_tool_use_ids: &HashSet<String>,
    should_animate: bool,
    verbose: bool,
    advisor_model_label: Option<&str>,
) -> AdvisorProjection {
    match block {
        AdvisorBlock::ServerToolUse { id, input_json } => AdvisorProjection::ToolUse {
            add_margin,
            loader: ToolUseLoaderDisplay {
                is_unresolved: !resolved_tool_use_ids.contains(id),
                is_error: errored_tool_use_ids.contains(id),
                should_animate,
            },
            advisor_model_label: advisor_model_label.map(|s| s.to_string()),
            input_json: input_json.clone(),
        },
        AdvisorBlock::ToolResultError { error_code } => AdvisorProjection::ResultError {
            error_code: error_code.clone(),
        },
        AdvisorBlock::Result { text } => AdvisorProjection::Result {
            verbose,
            text: text.clone(),
            show_expand_hint: !verbose,
        },
        AdvisorBlock::RedactedResult => AdvisorProjection::RedactedResult,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_tool_use_projects_loader_flags() {
        let resolved = HashSet::from(["a1".to_string()]);
        let errors = HashSet::new();
        let projection = project_advisor_message(
            &AdvisorBlock::ServerToolUse {
                id: "a1".into(),
                input_json: Some("{\"x\":1}".into()),
            },
            true,
            &resolved,
            &errors,
            true,
            false,
            Some("Sonnet"),
        );
        let AdvisorProjection::ToolUse { loader, .. } = projection else {
            panic!("expected tool use");
        };
        assert!(!loader.is_unresolved);
        assert!(!loader.is_error);
        assert!(loader.should_animate);
    }

    #[test]
    fn result_branch_switches_expand_hint_by_verbose() {
        let compact = project_advisor_message(
            &AdvisorBlock::Result { text: "abc".into() },
            false,
            &HashSet::new(),
            &HashSet::new(),
            false,
            false,
            None,
        );
        assert!(matches!(
            compact,
            AdvisorProjection::Result {
                show_expand_hint: true,
                ..
            }
        ));
    }

    #[test]
    fn error_and_redacted_branches_project() {
        assert!(matches!(
            project_advisor_message(
                &AdvisorBlock::ToolResultError {
                    error_code: "x".into()
                },
                false,
                &HashSet::new(),
                &HashSet::new(),
                false,
                false,
                None
            ),
            AdvisorProjection::ResultError { .. }
        ));
        assert_eq!(
            project_advisor_message(
                &AdvisorBlock::RedactedResult,
                false,
                &HashSet::new(),
                &HashSet::new(),
                false,
                false,
                None
            ),
            AdvisorProjection::RedactedResult
        );
    }
}
