//! # rebon-shell
//!
//! Pure shell-presentation helpers: everything needed to decide *what* a shell
//! command's rows should say, expressed as plain owned data with no IO, no
//! terminal handle and no dependency on any other `rebon-*` crate.
//!
//! * [`expand_shell_output`] - the "show full output" flag as a tiny
//!   bool-carrying value.
//! * [`output_line`] - JSON pretty-print probing, URL linkification,
//!   underline-ANSI stripping, and the full-versus-truncated decision.
//! * [`shell_time_display`] - the `(12s · timeout 5m)` text projection, plus
//!   the duration and file-size format helpers used elsewhere here.
//! * [`shell_progress_message`] - the row for a command still running: empty
//!   fallback, output tail, line-status suffix and byte-count label.
//! * [`bash_tool_result_message`] - the ordered row list for a finished bash
//!   tool result, plus the JSON seam that reads one.
//! * [`stats_logic`] - pure stats helpers: date-range cycling, factoids, shot
//!   buckets, and token-chart data preparation.
//!
//! ## Why the seam is plain data
//!
//! Terminal concerns stay outside this crate on purpose, so it can be compiled
//! and tested with no terminal at all:
//!
//! 1. terminal width and the "in virtual list" flag arrive as scalar input
//!    fields.
//! 2. hyperlink support is a boolean; the OSC 8 wrapper is still emitted here,
//!    as a pure string formatter.
//! 3. nothing is styled or painted - modules return owned structs and the
//!    caller decides how to draw them.
//! 4. duration and file-size formatting ([`format_duration`],
//!    [`format_file_size`]), truncation and JSON parsing are local helpers,
//!    which is why the dependency list is one crate long.

#![deny(missing_docs)]

pub mod bash_tool_result_message;
pub mod expand_shell_output;
pub mod output_line;
pub mod shell_progress_message;
pub mod shell_time_display;
pub mod stats_logic;

pub use bash_tool_result_message::{
    extract_cwd_reset_warning, extract_sandbox_violations, parse_bash_tool_result_json,
    project_bash_tool_result_message, BashToolResultBlock, BashToolResultDisplay,
    BashToolResultInput, ParsedBashToolResult, BACKGROUND_TASK_HINT, EMPTY_OUTPUT_DONE,
    EMPTY_OUTPUT_PLACEHOLDER, IMAGE_PLACEHOLDER, SANDBOX_VIOLATIONS_CLOSE, SANDBOX_VIOLATIONS_OPEN,
    SHELL_CWD_RESET_PREFIX,
};
pub use expand_shell_output::{expand_shell_output_enabled, ExpandShellOutputContextValue};
pub use output_line::{
    create_hyperlink, linkify_urls_in_text, project_output_line, strip_underline_ansi,
    try_format_json, try_json_format_content, OutputLineDisplay, OutputLineInput, OutputTone,
    PADDING_TO_PREVENT_OVERFLOW, URL_IN_JSON_PATTERN,
};
pub use shell_progress_message::{
    project_shell_progress_message, EmptyShellProgressDisplay, ShellProgressDisplay,
    ShellProgressInput, ShellProgressMetadata, ShellProgressOutputDisplay, MAX_PROGRESS_LINES,
};
pub use shell_time_display::{
    format_duration, format_file_size, project_shell_time_display, DurationFormatOptions,
    ShellTimeDisplay,
};
pub use stats_logic::{
    choose_fun_factoid, collect_fun_factoids, compute_shot_stats, date_range_label,
    format_peak_day, generate_fun_factoid, generate_x_axis_labels, get_next_date_range,
    prepare_token_chart_plan, BookComparison, ChartLegendEntry, ChartSeries, DailyModelTokens,
    ShotBucket, ShotStatsData, StatsDateRange, StatsFactoidInput, TimeComparison, TokenChartPlan,
    BOOK_COMPARISONS, DATE_RANGE_ORDER, TIME_COMPARISONS,
};

#[cfg(test)]
mod compatibility {
    /// Compatibility canary - `rebon-shell` is a leaf crate and must not gain
    /// a Rust dependency on another `rebon-*` crate without updating the
    /// crate-level docs, which promise that this crate has none.
    #[test]
    fn no_rebon_deps_in_cargo_toml() {
        let cargo = include_str!("../Cargo.toml");
        for line in cargo.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            assert!(
                !trimmed.starts_with("rebon-"),
                "rebon-shell must stay dep-free of other rebon crates; found: {line}"
            );
        }
    }
}
