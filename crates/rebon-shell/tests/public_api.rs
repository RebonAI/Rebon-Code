//! Integration coverage for the published crate-root surface.
//!
//! Every unit test inside `src/` reaches these items through `crate::` paths or
//! through a module path. This file imports them the way an out-of-crate caller
//! must, so a rename, a signature change, or a dropped re-export fails the build
//! here even when the crate itself still compiles.

use std::collections::HashMap;

use rebon_shell::output_line::{MAX_JSON_FORMAT_LENGTH, MAX_LINES_TO_SHOW, OSC8_END, OSC8_START};
use rebon_shell::{
    choose_fun_factoid, collect_fun_factoids, compute_shot_stats, create_hyperlink,
    date_range_label, expand_shell_output_enabled, extract_cwd_reset_warning,
    extract_sandbox_violations, format_duration, format_file_size, format_peak_day,
    generate_fun_factoid, generate_x_axis_labels, get_next_date_range, linkify_urls_in_text,
    parse_bash_tool_result_json, prepare_token_chart_plan, project_bash_tool_result_message,
    project_output_line, project_shell_progress_message, project_shell_time_display,
    strip_underline_ansi, try_format_json, try_json_format_content, BashToolResultBlock,
    BashToolResultDisplay, BashToolResultInput, BookComparison, ChartLegendEntry, ChartSeries,
    DailyModelTokens, DurationFormatOptions, EmptyShellProgressDisplay,
    ExpandShellOutputContextValue, OutputLineDisplay, OutputLineInput, OutputTone,
    ParsedBashToolResult, ShellProgressDisplay, ShellProgressInput, ShellProgressMetadata,
    ShellProgressOutputDisplay, ShellTimeDisplay, ShotBucket, ShotStatsData, StatsDateRange,
    StatsFactoidInput, TimeComparison, TokenChartPlan, BACKGROUND_TASK_HINT, BOOK_COMPARISONS,
    DATE_RANGE_ORDER, EMPTY_OUTPUT_DONE, EMPTY_OUTPUT_PLACEHOLDER, IMAGE_PLACEHOLDER,
    MAX_PROGRESS_LINES, PADDING_TO_PREVENT_OVERFLOW, SANDBOX_VIOLATIONS_CLOSE,
    SANDBOX_VIOLATIONS_OPEN, SHELL_CWD_RESET_PREFIX, TIME_COMPARISONS, URL_IN_JSON_PATTERN,
};

fn expand_flag(enabled: bool) -> ExpandShellOutputContextValue {
    ExpandShellOutputContextValue::from_bool(enabled)
}

#[test]
fn output_line_surface_is_usable_from_outside_the_crate() {
    let mut input = OutputLineInput {
        content: "https://example.com".to_string(),
        verbose: false,
        is_error: false,
        is_warning: false,
        linkify_urls: true,
        supports_hyperlinks: true,
        terminal_columns: 80,
        in_virtual_list: false,
        expand_shell_output: expand_flag(false),
    };
    assert!(!expand_shell_output_enabled(input.expand_shell_output));

    input.expand_shell_output = expand_flag(true);
    let display: OutputLineDisplay = project_output_line(&input);
    assert_eq!(display.tone, OutputTone::Neutral);
    assert!(!display.truncated);
    assert!(display.formatted.contains(OSC8_START));
    assert!(display.formatted.contains(OSC8_END));

    input.is_warning = true;
    assert_eq!(project_output_line(&input).tone, OutputTone::Warning);
}

#[test]
fn json_and_link_helpers_are_reachable() {
    assert_eq!(try_format_json("{\"a\":1}"), "{\n  \"a\": 1\n}");
    assert_eq!(try_json_format_content("{oops}"), "{oops}");
    assert_eq!(
        create_hyperlink("https://a.com", None, false),
        "https://a.com"
    );
    assert_eq!(
        linkify_urls_in_text("https://a.com", false),
        "https://a.com"
    );
    assert_eq!(strip_underline_ansi("\u{1b}[4mx\u{1b}[0m"), "x\u{1b}[0m");
    assert!(URL_IN_JSON_PATTERN.starts_with("https?"));
    assert!(PADDING_TO_PREVENT_OVERFLOW > 0);
    assert!(MAX_LINES_TO_SHOW > 0);
    assert!(MAX_JSON_FORMAT_LENGTH > MAX_LINES_TO_SHOW);
}

#[test]
fn progress_surface_is_reachable() {
    let input = ShellProgressInput {
        output: "1\n2".to_string(),
        full_output: "1\n2".to_string(),
        elapsed_time_seconds: Some(8),
        total_lines: Some(2000),
        total_bytes: Some(1024),
        timeout_ms: Some(120_000),
        verbose: false,
    };
    let display = project_shell_progress_message(&input);
    let ShellProgressDisplay::Output(ShellProgressOutputDisplay {
        display_lines,
        display_height,
        metadata,
    }) = display
    else {
        panic!("expected output branch");
    };
    let ShellProgressMetadata {
        line_status,
        time_display,
        total_bytes_label,
    } = metadata;
    assert_eq!(display_lines, "1\n2");
    assert_eq!(display_height, Some(2));
    assert_eq!(line_status.as_deref(), Some("~2000 lines"));
    assert_eq!(total_bytes_label.as_deref(), Some("1KB"));
    assert_eq!(
        time_display,
        Some(ShellTimeDisplay {
            text: "(8s \u{00b7} timeout 2m)".to_string()
        })
    );

    let empty = ShellProgressDisplay::Empty(EmptyShellProgressDisplay {
        status_text: "Running\u{2026}",
        time_display: None,
    });
    assert!(matches!(empty, ShellProgressDisplay::Empty(_)));
    assert_eq!(MAX_PROGRESS_LINES, 5);
}

#[test]
fn time_display_surface_is_reachable() {
    let display: ShellTimeDisplay = project_shell_time_display(Some(12), Some(120_000)).unwrap();
    assert_eq!(display.text, "(12s \u{00b7} timeout 2m)");
    assert_eq!(project_shell_time_display(None, None), None);
    assert_eq!(format_file_size(1024), "1KB");
    assert_eq!(
        format_duration(
            3_600_000,
            DurationFormatOptions {
                hide_trailing_zeros: true,
                most_significant_only: false,
            }
        ),
        "1h"
    );
}

#[test]
fn bash_result_surface_is_reachable() {
    let parsed: ParsedBashToolResult =
        parse_bash_tool_result_json("{\"stdout\":\"ok\"}").expect("parses");
    assert_eq!(parsed.stdout, "ok");
    assert!(parse_bash_tool_result_json("{}").is_none());

    let input = BashToolResultInput {
        stdout: "ok".to_string(),
        stderr: "bad".to_string(),
        is_image: false,
        return_code_interpretation: None,
        no_output_expected: false,
        background_task_id: None,
        timeout_ms: None,
        verbose: false,
        supports_hyperlinks: true,
        terminal_columns: 80,
        in_virtual_list: false,
        expand_shell_output: expand_flag(false),
    };
    let display: BashToolResultDisplay = project_bash_tool_result_message(&input);
    assert!(matches!(display.blocks[0], BashToolResultBlock::Stdout(_)));
    assert!(matches!(display.blocks[1], BashToolResultBlock::Stderr(_)));
    assert_eq!(display.blocks.len(), 2);
}

#[test]
fn bash_result_extractors_and_constants_are_reachable() {
    let tagged = format!("{SANDBOX_VIOLATIONS_OPEN}ignored{SANDBOX_VIOLATIONS_CLOSE}real error");
    assert_eq!(extract_sandbox_violations(&tagged), "real error");

    let stderr = format!("boom\n{SHELL_CWD_RESET_PREFIX}/tmp");
    let (cleaned, warning) = extract_cwd_reset_warning(&stderr);
    assert_eq!(cleaned, "boom");
    assert_eq!(
        warning.as_deref(),
        Some("Shell cwd was reset to /tmp"),
        "warning is the raw line, prefix included"
    );

    let blocks = BashToolResultDisplay {
        blocks: vec![
            BashToolResultBlock::ImagePlaceholder,
            BashToolResultBlock::CwdResetWarning(format!("{SHELL_CWD_RESET_PREFIX}/tmp")),
            BashToolResultBlock::EmptyOutputFallback(BACKGROUND_TASK_HINT.to_string()),
            BashToolResultBlock::EmptyOutputFallback(EMPTY_OUTPUT_DONE.to_string()),
            BashToolResultBlock::EmptyOutputFallback(EMPTY_OUTPUT_PLACEHOLDER.to_string()),
            BashToolResultBlock::TimeoutDisplay(ShellTimeDisplay {
                text: "(timeout 1m)".to_string(),
            }),
        ],
    };
    assert_eq!(blocks.blocks.len(), 6);
    assert_eq!(IMAGE_PLACEHOLDER, "[Image data detected and sent to Rebon]");
}

#[test]
fn stats_surface_is_reachable() {
    assert_eq!(DATE_RANGE_ORDER.len(), 3);
    assert_eq!(
        get_next_date_range(StatsDateRange::All),
        StatsDateRange::Days7
    );
    assert_eq!(date_range_label(StatsDateRange::Days30), "Last 30 days");

    assert_eq!(BOOK_COMPARISONS.len(), 24);
    assert_eq!(TIME_COMPARISONS.len(), 10);
    let book: BookComparison = BOOK_COMPARISONS[0];
    assert_eq!(book.name, "The Little Prince");
    let comparable_time: TimeComparison = TIME_COMPARISONS[0];
    assert_eq!(comparable_time.minutes, 18);

    let factoid_input = StatsFactoidInput {
        longest_session_duration_ms: Some(4 * 60 * 60 * 1000),
    };
    let factoids: Vec<String> = collect_fun_factoids(&factoid_input, 80_000);
    assert!(factoids.len() > 2);
    assert_eq!(choose_fun_factoid(&factoids, 2), factoids[2]);
    assert_eq!(generate_fun_factoid(&factoid_input, 80_000, 2), factoids[2]);
    assert_eq!(format_peak_day("2026-04-09").as_deref(), Some("Apr 9"));

    let summary: ShotStatsData =
        compute_shot_stats(&HashMap::from([(1u64, 2u64)])).expect("summary");
    let bucket: &ShotBucket = &summary.buckets[0];
    assert_eq!(bucket.count, 2);
    assert_eq!(bucket.pct, 100);
    assert_eq!(summary.avg_shots, "1.0");
    assert!(compute_shot_stats(&HashMap::new()).is_none());

    let days = vec![
        DailyModelTokens {
            date: "2026-01-01".to_string(),
            tokens_by_model: HashMap::from([("opus".to_string(), 1u64)]),
        },
        DailyModelTokens {
            date: "2026-01-02".to_string(),
            tokens_by_model: HashMap::from([("opus".to_string(), 2u64)]),
        },
    ];
    let plan: TokenChartPlan =
        prepare_token_chart_plan(&days, &["opus".to_string()], 60, str::to_uppercase)
            .expect("plan");
    let series: &ChartSeries = &plan.series[0];
    assert_eq!(series.display_name, "OPUS");
    let legend: &ChartLegendEntry = &plan.legend[0];
    assert_eq!(legend.color_slot, 0);
    assert_eq!(plan.y_axis_width, 7);
    assert!(generate_x_axis_labels(&days, 7).starts_with("       "));
}
