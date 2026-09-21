//! Projection of one finished bash tool result into the rows a renderer paints.
//!
//! The result becomes an ordered column of up to five regions:
//!
//! 1. a stdout row, skipped when stdout is empty;
//! 2. a stderr row, marked as an error and drawn with the error tone, skipped
//!    when the trimmed stderr is empty;
//! 3. a dim `"Shell cwd was reset to <path>"` row, pulled out of stderr so the
//!    error tone does not colour it red;
//! 4. a single-row empty-output fallback:
//!       * the background-task hint, when a background task id is present, or
//!       * the return-code interpretation string, or
//!       * `"Done"` when the expected-empty flag is set, or
//!       * `"(No output)"` otherwise;
//! 5. a timeout row when a timeout is present.
//!
//! The projection is pure data: the consumer paints each
//! [`BashToolResultBlock`] with whatever terminal primitive it prefers, and no
//! style or widget objects are allocated here.
//!
//! [`parse_bash_tool_result_json`] is the seam that reads a bash result out of
//! the raw JSON string the tool returned. It keys off the `stdout`/`stderr`
//! string fields, so any other tool's JSON body yields `None` and the caller
//! can fall back to a generic renderer without guessing.

use crate::expand_shell_output::ExpandShellOutputContextValue;
use crate::output_line::{project_output_line, OutputLineDisplay, OutputLineInput};
use crate::shell_time_display::{project_shell_time_display, ShellTimeDisplay};

use serde_json::Value;

/// The copy rendered when stdout and the trimmed stderr are both empty,
/// no cwd-reset warning is present, but the expected-empty flag is set.
pub const EMPTY_OUTPUT_DONE: &str = "Done";
/// The copy used when the expected-empty flag is false and there is no
/// background task id and no return-code interpretation.
pub const EMPTY_OUTPUT_PLACEHOLDER: &str = "(No output)";
/// The copy used for the background-task branch. The `↓` names the key
/// that opens task management; the TUI can remap it.
pub const BACKGROUND_TASK_HINT: &str = "Running in the background (\u{2193} to manage)";
/// The copy used when the result carries image data.
pub const IMAGE_PLACEHOLDER: &str = "[Image data detected and sent to Rebon]";
/// Opening tag used in stderr sandbox-violation markers.
pub const SANDBOX_VIOLATIONS_OPEN: &str = "<sandbox_violations>";
/// Closing tag used in stderr sandbox-violation markers.
pub const SANDBOX_VIOLATIONS_CLOSE: &str = "</sandbox_violations>";
/// Prefix of the cwd-reset warning rendered in dim color instead of the
/// error tone.
pub const SHELL_CWD_RESET_PREFIX: &str = "Shell cwd was reset to ";

/// Input fields taken from the bash tool's result, minus `interrupted`,
/// which the projection does not render.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BashToolResultInput {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Whether the result carries image data rather than text.
    pub is_image: bool,
    /// Post-exit text interpretation, such as `"Command exited with
    /// status 1"`.
    pub return_code_interpretation: Option<String>,
    /// When true the empty fallback says `"Done"` instead of
    /// `"(No output)"`.
    pub no_output_expected: bool,
    /// Background task id; its presence toggles the background-task hint.
    pub background_task_id: Option<String>,
    /// Optional timeout for the run, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Whether the verbose output style is in use.
    pub verbose: bool,
    /// Whether the terminal supports hyperlinks.
    pub supports_hyperlinks: bool,
    /// Terminal column count, used for truncation.
    pub terminal_columns: usize,
    /// Whether the row sits inside a virtualized list.
    pub in_virtual_list: bool,
    /// Whether the expand-shell-output flag is set.
    pub expand_shell_output: ExpandShellOutputContextValue,
}

/// One visible row of the projection. The order of
/// [`BashToolResultDisplay::blocks`] is the order the rows are painted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashToolResultBlock {
    /// The image branch: the projection collapses to this single dim line.
    ImagePlaceholder,
    /// The stdout row.
    Stdout(OutputLineDisplay),
    /// The stderr row, drawn with the error tone.
    Stderr(OutputLineDisplay),
    /// The dim `"Shell cwd was reset to <path>"` message, extracted from
    /// stderr so the error-tone stderr row does not colour it red.
    CwdResetWarning(String),
    /// Fallback row when all three output channels are empty.
    EmptyOutputFallback(String),
    /// The shell-time line for the run's timeout.
    TimeoutDisplay(ShellTimeDisplay),
}

/// Ordered list of rows the consumer should paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashToolResultDisplay {
    /// Empty when the projection yields nothing visible (never happens
    /// in practice but represented for symmetry).
    pub blocks: Vec<BashToolResultBlock>,
}

/// Parsed bash tool JSON seeds for [`BashToolResultInput`]. Only the fields
/// the projection actually renders are pulled out of the JSON; ignored keys
/// (`interrupted`, `exitCode`, `timedOut`, `command`,
/// `dangerouslyDisableSandbox`, …) do not influence it and are dropped here
/// so the consumer does not carry a full `serde_json::Value` around.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedBashToolResult {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Whether the result carries image data rather than text.
    pub is_image: bool,
    /// Post-exit text interpretation, when the result carries one.
    pub return_code_interpretation: Option<String>,
    /// Whether the caller expected the run to produce no output.
    pub no_output_expected: bool,
    /// Background task id, when the run was moved to the background.
    pub background_task_id: Option<String>,
}

/// Try to parse a raw tool-result JSON string as a bash result.
///
/// Returns `Some` when the JSON object carries either a `stdout` or
/// `stderr` string key — that is the shape the bash tool emits. Returns
/// `None` for non-object JSON, non-bash shapes, and parse errors.
pub fn parse_bash_tool_result_json(raw: &str) -> Option<ParsedBashToolResult> {
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    let map = value.as_object()?;
    let stdout = map.get("stdout").and_then(Value::as_str);
    let stderr = map.get("stderr").and_then(Value::as_str);
    if stdout.is_none() && stderr.is_none() {
        return None;
    }
    Some(ParsedBashToolResult {
        stdout: stdout.unwrap_or("").to_string(),
        stderr: stderr.unwrap_or("").to_string(),
        is_image: map.get("isImage").and_then(Value::as_bool).unwrap_or(false),
        return_code_interpretation: map
            .get("returnCodeInterpretation")
            .and_then(Value::as_str)
            .map(str::to_string),
        no_output_expected: map
            .get("noOutputExpected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        background_task_id: map
            .get("backgroundTaskId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Remove the sandbox-violation tags from `text`, then trim the result.
pub fn extract_sandbox_violations(stderr: &str) -> String {
    let mut cleaned = String::with_capacity(stderr.len());
    let mut cursor = 0usize;
    while let Some(open) = stderr[cursor..].find(SANDBOX_VIOLATIONS_OPEN) {
        let start = cursor + open;
        cleaned.push_str(&stderr[cursor..start]);
        let after_open = start + SANDBOX_VIOLATIONS_OPEN.len();
        if let Some(rel_close) = stderr[after_open..].find(SANDBOX_VIOLATIONS_CLOSE) {
            cursor = after_open + rel_close + SANDBOX_VIOLATIONS_CLOSE.len();
        } else {
            cursor = stderr.len();
            break;
        }
    }
    if cursor < stderr.len() {
        cleaned.push_str(&stderr[cursor..]);
    }
    // Only trim when a tag was actually removed, so an untouched stderr is
    // handed back byte-for-byte.
    if cleaned.len() != stderr.len() {
        cleaned.trim().to_string()
    } else {
        stderr.to_string()
    }
}

/// Extract the cwd-reset warning from stderr.
///
/// The warning must sit at the start of a line *and* run to the end of
/// the string. Returns the cleaned stderr, without the warning, plus the
/// warning text when present.
pub fn extract_cwd_reset_warning(stderr: &str) -> (String, Option<String>) {
    let candidates = stderr
        .match_indices(SHELL_CWD_RESET_PREFIX)
        .filter(|(idx, _)| *idx == 0 || stderr.as_bytes()[*idx - 1] == b'\n')
        .collect::<Vec<_>>();

    let Some((start, _)) = candidates.last().copied() else {
        return (stderr.to_string(), None);
    };

    let rest = &stderr[start..];
    if rest.contains('\n') {
        return (stderr.to_string(), None);
    }
    let warning = rest.to_string();

    let preceding_newline = start > 0 && stderr.as_bytes()[start - 1] == b'\n';
    let cleaned_end = if preceding_newline { start - 1 } else { start };
    let cleaned = stderr[..cleaned_end].trim_end().to_string();
    (cleaned, Some(warning))
}

/// Project one parsed bash tool result into the rows a renderer paints.
pub fn project_bash_tool_result_message(input: &BashToolResultInput) -> BashToolResultDisplay {
    let stderr_without_violations = extract_sandbox_violations(&input.stderr);
    let (stderr, cwd_reset_warning) = extract_cwd_reset_warning(&stderr_without_violations);

    if input.is_image {
        return BashToolResultDisplay {
            blocks: vec![BashToolResultBlock::ImagePlaceholder],
        };
    }

    let mut blocks = Vec::new();
    let stdout = trim_leading_blank_lines(&input.stdout);
    let stderr = trim_leading_blank_lines(&stderr);

    if !stdout.is_empty() {
        blocks.push(BashToolResultBlock::Stdout(project_output_line(
            &make_output_input(stdout, input, false),
        )));
    }

    if !stderr.trim().is_empty() {
        blocks.push(BashToolResultBlock::Stderr(project_output_line(
            &make_output_input(stderr, input, true),
        )));
    }

    if let Some(warning) = cwd_reset_warning.as_ref() {
        blocks.push(BashToolResultBlock::CwdResetWarning(warning.clone()));
    }

    let empty = stdout.is_empty() && stderr.trim().is_empty() && cwd_reset_warning.is_none();
    if empty {
        let fallback = if input.background_task_id.is_some() {
            BACKGROUND_TASK_HINT.to_string()
        } else if let Some(interpretation) = input
            .return_code_interpretation
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            interpretation.clone()
        } else if input.no_output_expected {
            EMPTY_OUTPUT_DONE.to_string()
        } else {
            EMPTY_OUTPUT_PLACEHOLDER.to_string()
        };
        blocks.push(BashToolResultBlock::EmptyOutputFallback(fallback));
    }

    if let Some(timeout_ms) = input.timeout_ms {
        if let Some(display) = project_shell_time_display(None, Some(timeout_ms)) {
            blocks.push(BashToolResultBlock::TimeoutDisplay(display));
        }
    }

    BashToolResultDisplay { blocks }
}

fn trim_leading_blank_lines(content: &str) -> &str {
    content.trim_start_matches(|c| c == '\n' || c == '\r')
}

fn make_output_input(
    content: &str,
    input: &BashToolResultInput,
    is_error: bool,
) -> OutputLineInput {
    OutputLineInput {
        content: content.to_string(),
        verbose: input.verbose,
        is_error,
        is_warning: false,
        linkify_urls: false,
        supports_hyperlinks: input.supports_hyperlinks,
        terminal_columns: input.terminal_columns,
        in_virtual_list: input.in_virtual_list,
        expand_shell_output: input.expand_shell_output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_line::OutputTone;

    fn base_input() -> BashToolResultInput {
        BashToolResultInput {
            stdout: String::new(),
            stderr: String::new(),
            is_image: false,
            return_code_interpretation: None,
            no_output_expected: false,
            background_task_id: None,
            timeout_ms: None,
            verbose: false,
            supports_hyperlinks: true,
            terminal_columns: 80,
            in_virtual_list: false,
            expand_shell_output: ExpandShellOutputContextValue::default(),
        }
    }

    // ── extract_sandbox_violations coverage ─────────────────────────

    #[test]
    fn extract_sandbox_violations_passthrough_when_absent() {
        assert_eq!(extract_sandbox_violations("normal error"), "normal error");
    }

    #[test]
    fn extract_sandbox_violations_strips_single_block_and_trims() {
        let cleaned = extract_sandbox_violations(
            "   before <sandbox_violations>ignored</sandbox_violations> after   ",
        );
        assert_eq!(cleaned, "before  after");
    }

    #[test]
    fn extract_sandbox_violations_strips_multiple_blocks() {
        let cleaned = extract_sandbox_violations(
            "a<sandbox_violations>x</sandbox_violations>b<sandbox_violations>y</sandbox_violations>c",
        );
        assert_eq!(cleaned, "abc");
    }

    #[test]
    fn extract_sandbox_violations_ignores_unterminated_tag() {
        let cleaned = extract_sandbox_violations("before <sandbox_violations>incomplete tail");
        assert_eq!(cleaned, "before");
    }

    // ── extract_cwd_reset_warning coverage ──────────────────────────

    #[test]
    fn extract_cwd_reset_returns_none_when_absent() {
        let (cleaned, warn) = extract_cwd_reset_warning("permission denied");
        assert_eq!(cleaned, "permission denied");
        assert!(warn.is_none());
    }

    #[test]
    fn extract_cwd_reset_matches_at_start_of_stderr() {
        let (cleaned, warn) = extract_cwd_reset_warning("Shell cwd was reset to /tmp");
        assert!(cleaned.is_empty());
        assert_eq!(warn.as_deref(), Some("Shell cwd was reset to /tmp"));
    }

    #[test]
    fn extract_cwd_reset_matches_after_newline_and_strips_newline() {
        let (cleaned, warn) = extract_cwd_reset_warning("oops\nShell cwd was reset to /work/proj");
        assert_eq!(cleaned, "oops");
        assert_eq!(warn.as_deref(), Some("Shell cwd was reset to /work/proj"));
    }

    #[test]
    fn extract_cwd_reset_ignores_mid_line_occurrence() {
        let (cleaned, warn) = extract_cwd_reset_warning("error: Shell cwd was reset to /tmp");
        assert_eq!(cleaned, "error: Shell cwd was reset to /tmp");
        assert!(warn.is_none());
    }

    #[test]
    fn extract_cwd_reset_ignores_marker_not_at_end() {
        let (cleaned, warn) = extract_cwd_reset_warning("Shell cwd was reset to /tmp\nmore text");
        assert_eq!(cleaned, "Shell cwd was reset to /tmp\nmore text");
        assert!(warn.is_none());
    }

    #[test]
    fn extract_cwd_reset_prefers_last_matching_line() {
        let stderr = "Shell cwd was reset to /a\nShell cwd was reset to /b";
        let (cleaned, warn) = extract_cwd_reset_warning(stderr);
        assert_eq!(cleaned, "Shell cwd was reset to /a");
        assert_eq!(warn.as_deref(), Some("Shell cwd was reset to /b"));
    }

    // ── parse_bash_tool_result_json coverage ───────────────────────

    #[test]
    fn parse_bash_tool_result_returns_none_for_non_bash_shapes() {
        assert!(parse_bash_tool_result_json("null").is_none());
        assert!(parse_bash_tool_result_json("\"not-json-object\"").is_none());
        assert!(parse_bash_tool_result_json("{}").is_none());
        assert!(parse_bash_tool_result_json("{\"other\":42}").is_none());
    }

    #[test]
    fn parse_bash_tool_result_reads_stdout_and_stderr_and_flags() {
        let raw = r#"{
            "stdout": "hello\nworld",
            "stderr": "",
            "isImage": false,
            "noOutputExpected": true,
            "exitCode": 0,
            "returnCodeInterpretation": "Command exited with status 0",
            "backgroundTaskId": "bg-1"
        }"#;
        let parsed = parse_bash_tool_result_json(raw).expect("parses");
        assert_eq!(parsed.stdout, "hello\nworld");
        assert_eq!(parsed.stderr, "");
        assert!(!parsed.is_image);
        assert!(parsed.no_output_expected);
        assert_eq!(
            parsed.return_code_interpretation.as_deref(),
            Some("Command exited with status 0")
        );
        assert_eq!(parsed.background_task_id.as_deref(), Some("bg-1"));
    }

    #[test]
    fn parse_bash_tool_result_tolerates_stdout_only_shape() {
        let parsed = parse_bash_tool_result_json(r#"{"stdout":"ok"}"#).expect("parses");
        assert_eq!(parsed.stdout, "ok");
        assert_eq!(parsed.stderr, "");
    }

    #[test]
    fn parse_bash_tool_result_tolerates_stderr_only_shape() {
        let parsed = parse_bash_tool_result_json(r#"{"stderr":"bad"}"#).expect("parses");
        assert_eq!(parsed.stdout, "");
        assert_eq!(parsed.stderr, "bad");
    }

    // ── project_bash_tool_result_message coverage ───────────────────

    #[test]
    fn image_branch_collapses_to_placeholder_only() {
        let mut input = base_input();
        input.is_image = true;
        input.stdout = "ignored".into();
        input.timeout_ms = Some(60_000);
        let display = project_bash_tool_result_message(&input);
        assert_eq!(display.blocks, vec![BashToolResultBlock::ImagePlaceholder]);
    }

    #[test]
    fn stdout_only_renders_single_stdout_block() {
        let mut input = base_input();
        input.stdout = "hello".into();
        let display = project_bash_tool_result_message(&input);
        assert_eq!(display.blocks.len(), 1);
        match &display.blocks[0] {
            BashToolResultBlock::Stdout(output) => {
                assert_eq!(output.formatted, "hello");
                assert_eq!(output.tone, OutputTone::Neutral);
            }
            other => panic!("expected stdout block, got {other:?}"),
        }
    }

    #[test]
    fn stdout_trims_leading_blank_lines_before_rendering() {
        let mut input = base_input();
        input.stdout = "\n\r\n  running 38 tests".into();
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::Stdout(output) = &display.blocks[0] else {
            panic!("expected stdout block");
        };
        assert_eq!(output.formatted, "  running 38 tests");
    }

    #[test]
    fn stderr_trims_leading_blank_lines_before_rendering() {
        let mut input = base_input();
        input.stderr = "\n\nerror".into();
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::Stderr(output) = &display.blocks[0] else {
            panic!("expected stderr block");
        };
        assert_eq!(output.formatted, "error");
    }

    #[test]
    fn stderr_is_trimmed_for_empty_check_but_rendered_raw() {
        let mut input = base_input();
        input.stderr = "   \n".into();
        let display = project_bash_tool_result_message(&input);
        assert_eq!(display.blocks.len(), 1);
        assert!(matches!(
            display.blocks[0],
            BashToolResultBlock::EmptyOutputFallback(_)
        ));
    }

    #[test]
    fn stderr_block_renders_with_error_tone() {
        let mut input = base_input();
        input.stderr = "boom".into();
        let display = project_bash_tool_result_message(&input);
        match &display.blocks[0] {
            BashToolResultBlock::Stderr(output) => {
                assert_eq!(output.formatted, "boom");
                assert_eq!(output.tone, OutputTone::Error);
            }
            other => panic!("expected stderr block, got {other:?}"),
        }
    }

    #[test]
    fn both_stdout_and_stderr_preserve_order() {
        let mut input = base_input();
        input.stdout = "out".into();
        input.stderr = "err".into();
        let display = project_bash_tool_result_message(&input);
        assert!(matches!(display.blocks[0], BashToolResultBlock::Stdout(_)));
        assert!(matches!(display.blocks[1], BashToolResultBlock::Stderr(_)));
    }

    #[test]
    fn cwd_reset_warning_is_separated_from_stderr_tone() {
        let mut input = base_input();
        input.stderr = "warning\nShell cwd was reset to /tmp".into();
        let display = project_bash_tool_result_message(&input);
        let kinds: Vec<_> = display
            .blocks
            .iter()
            .map(|block| match block {
                BashToolResultBlock::Stderr(_) => "stderr",
                BashToolResultBlock::CwdResetWarning(_) => "cwd",
                BashToolResultBlock::EmptyOutputFallback(_) => "fallback",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["stderr", "cwd"]);
        let BashToolResultBlock::CwdResetWarning(text) = &display.blocks[1] else {
            panic!("expected cwd warning block");
        };
        assert_eq!(text, "Shell cwd was reset to /tmp");
    }

    #[test]
    fn cwd_reset_alone_suppresses_empty_fallback() {
        let mut input = base_input();
        input.stderr = "Shell cwd was reset to /home/user".into();
        let display = project_bash_tool_result_message(&input);
        assert_eq!(display.blocks.len(), 1);
        assert!(matches!(
            display.blocks[0],
            BashToolResultBlock::CwdResetWarning(_)
        ));
    }

    #[test]
    fn empty_output_background_task_overrides_other_fallbacks() {
        let mut input = base_input();
        input.background_task_id = Some("bg-1".into());
        input.return_code_interpretation = Some("status 42".into());
        input.no_output_expected = true;
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::EmptyOutputFallback(text) = &display.blocks[0] else {
            panic!("expected fallback block");
        };
        assert_eq!(text, BACKGROUND_TASK_HINT);
    }

    #[test]
    fn empty_output_uses_return_code_interpretation_over_done() {
        let mut input = base_input();
        input.return_code_interpretation = Some("status 1".into());
        input.no_output_expected = true;
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::EmptyOutputFallback(text) = &display.blocks[0] else {
            panic!("expected fallback block");
        };
        assert_eq!(text, "status 1");
    }

    #[test]
    fn empty_output_uses_done_when_no_output_expected() {
        let mut input = base_input();
        input.no_output_expected = true;
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::EmptyOutputFallback(text) = &display.blocks[0] else {
            panic!("expected fallback block");
        };
        assert_eq!(text, EMPTY_OUTPUT_DONE);
    }

    #[test]
    fn empty_output_default_is_no_output_marker() {
        let display = project_bash_tool_result_message(&base_input());
        let BashToolResultBlock::EmptyOutputFallback(text) = &display.blocks[0] else {
            panic!("expected fallback block");
        };
        assert_eq!(text, EMPTY_OUTPUT_PLACEHOLDER);
    }

    #[test]
    fn empty_return_code_interpretation_falls_back_to_done_marker() {
        let mut input = base_input();
        input.return_code_interpretation = Some(String::new());
        input.no_output_expected = true;
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::EmptyOutputFallback(text) = &display.blocks[0] else {
            panic!("expected fallback block");
        };
        assert_eq!(text, EMPTY_OUTPUT_DONE);
    }

    #[test]
    fn timeout_display_appended_after_output_blocks() {
        let mut input = base_input();
        input.stdout = "ok".into();
        input.timeout_ms = Some(120_000);
        let display = project_bash_tool_result_message(&input);
        assert_eq!(display.blocks.len(), 2);
        let BashToolResultBlock::TimeoutDisplay(time) = &display.blocks[1] else {
            panic!("expected timeout display block");
        };
        assert_eq!(time.text, "(timeout 2m)");
    }

    #[test]
    fn timeout_display_appears_even_on_empty_fallback() {
        let mut input = base_input();
        input.timeout_ms = Some(5_000);
        let display = project_bash_tool_result_message(&input);
        let kinds: Vec<_> = display
            .blocks
            .iter()
            .map(|block| match block {
                BashToolResultBlock::EmptyOutputFallback(_) => "fallback",
                BashToolResultBlock::TimeoutDisplay(_) => "timeout",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["fallback", "timeout"]);
    }

    #[test]
    fn verbose_input_threads_through_to_output_line() {
        let mut input = base_input();
        input.verbose = true;
        input.stdout = "a\nb\nc\nd\ne\nf".into();
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::Stdout(output) = &display.blocks[0] else {
            panic!("expected stdout block");
        };
        assert!(!output.truncated);
        assert!(output.formatted.contains('f'));
    }

    #[test]
    fn sandbox_violations_are_stripped_before_stderr_block() {
        let mut input = base_input();
        input.stderr = "real error<sandbox_violations>path=/etc</sandbox_violations>".into();
        let display = project_bash_tool_result_message(&input);
        let BashToolResultBlock::Stderr(output) = &display.blocks[0] else {
            panic!("expected stderr block");
        };
        assert_eq!(output.formatted, "real error");
    }
}
