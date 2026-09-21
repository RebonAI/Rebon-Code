//! Collapsed read/search summary row: the monotonic count state tracked
//! across renders plus the projection that turns it into display lines.

use std::collections::HashSet;
use std::path::Path;

/// Minimum hint hold time.
pub const MIN_HINT_DISPLAY_MS: u16 = 700;

/// Monotonic max-count state tracked across renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollapsedCountsState {
    /// Max seen read count.
    pub read_count: usize,
    /// Max seen search count.
    pub search_count: usize,
    /// Max seen list count.
    pub list_count: usize,
    /// Max seen MCP count.
    pub mcp_call_count: usize,
    /// Max seen bash count.
    pub bash_count: usize,
}

/// Latest progress payload for one tool use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedProgressUpdate {
    /// Tool-use ID.
    pub tool_use_id: String,
    /// Latest progress payload.
    pub kind: CollapsedProgressUpdateKind,
}

/// Progress payloads consumed by the collapsed renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollapsedProgressUpdateKind {
    /// `repl_tool_call` with `phase` set to `"start"`.
    ReplToolCallStart {
        /// Inner tool name.
        tool_name: String,
        /// Optional command text.
        command: Option<String>,
        /// Optional pattern.
        pattern: Option<String>,
        /// Optional file path.
        file_path: Option<String>,
    },
    /// `bash_progress`.
    BashProgress {
        /// Elapsed seconds.
        elapsed_time_seconds: u64,
        /// Output lines seen so far.
        total_lines: usize,
    },
    /// `powershell_progress`.
    PowershellProgress {
        /// Elapsed seconds.
        elapsed_time_seconds: u64,
        /// Output lines seen so far.
        total_lines: usize,
    },
}

/// Commit summary item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedCommit {
    /// Commit kind.
    pub kind: String,
    /// Commit SHA.
    pub sha: String,
}

/// Branch summary item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedBranchSummary {
    /// Action (`merged` / `rebased`).
    pub action: String,
    /// Reference name.
    pub reference: String,
}

/// PR action kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapsedPrAction {
    /// `created`.
    Created,
    /// `edited`.
    Edited,
    /// `merged`.
    Merged,
    /// `commented`.
    Commented,
    /// `closed`.
    Closed,
    /// `ready`.
    Ready,
}

/// PR summary item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedPr {
    /// Action.
    pub action: CollapsedPrAction,
    /// PR number.
    pub number: usize,
}

/// Hook timing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedHookInfo {
    /// Command text.
    pub command: String,
    /// Duration in milliseconds.
    pub duration_ms: u64,
}

/// Relevant-memory row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedMemoryEntry {
    /// File path.
    pub path: String,
    /// Memory content.
    pub content: String,
}

/// Already-resolved verbose tool row input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedToolUseVerboseInput {
    /// Tool-use ID.
    pub id: String,
    /// User-facing tool name. `None` means unresolved tool.
    pub user_facing_name: Option<String>,
    /// Optional tool-use message without parentheses.
    pub tool_use_message: Option<String>,
    /// Optional tag text.
    pub tag: Option<String>,
    /// Optional rendered result summary.
    pub result_message: Option<String>,
}

/// Input seam for collapsed read/search rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedReadSearchInput {
    /// Previous max counts.
    pub previous_counts: CollapsedCountsState,
    /// Raw search count.
    pub raw_search_count: usize,
    /// Raw read count.
    pub raw_read_count: usize,
    /// Raw list count.
    pub raw_list_count: usize,
    /// REPL count.
    pub repl_count: usize,
    /// Memory search count.
    pub memory_search_count: usize,
    /// Memory read count.
    pub memory_read_count: usize,
    /// Memory write count.
    pub memory_write_count: usize,
    /// Team-memory search count.
    pub team_memory_search_count: usize,
    /// Team-memory read count.
    pub team_memory_read_count: usize,
    /// Team-memory write count.
    pub team_memory_write_count: usize,
    /// MCP call count.
    pub mcp_call_count: usize,
    /// Raw bash count.
    pub raw_bash_count: usize,
    /// Git-op bash count.
    pub git_op_bash_count: usize,
    /// Read file paths.
    pub read_file_paths: Vec<String>,
    /// Search arguments.
    pub search_args: Vec<String>,
    /// Latest display hint.
    pub latest_display_hint: Option<String>,
    /// Tool-use IDs in this group.
    pub tool_use_ids: Vec<String>,
    /// Resolved tool-use IDs.
    pub resolved_tool_use_ids: HashSet<String>,
    /// Errored tool-use IDs.
    pub errored_tool_use_ids: HashSet<String>,
    /// In-progress tool-use IDs.
    pub in_progress_tool_use_ids: HashSet<String>,
    /// Latest progress payloads.
    pub latest_progress_updates: Vec<CollapsedProgressUpdate>,
    /// Commit summaries.
    pub commits: Vec<CollapsedCommit>,
    /// Push branch names.
    pub pushes: Vec<String>,
    /// Branch summaries.
    pub branches: Vec<CollapsedBranchSummary>,
    /// PR summaries.
    pub prs: Vec<CollapsedPr>,
    /// MCP server names.
    pub mcp_server_names: Vec<String>,
    /// Hook count.
    pub hook_count: usize,
    /// Hook total milliseconds.
    pub hook_total_ms: Option<u64>,
    /// Hook detail rows.
    pub hook_infos: Vec<CollapsedHookInfo>,
    /// Relevant memories.
    pub relevant_memories: Vec<CollapsedMemoryEntry>,
    /// Verbose tool rows.
    pub verbose_tool_uses: Vec<CollapsedToolUseVerboseInput>,
    /// Should animate loaders.
    pub should_animate: bool,
    /// Verbose mode.
    pub verbose: bool,
    /// Active group flag.
    pub is_active_group: bool,
    /// Fullscreen env flag.
    pub fullscreen_enabled: bool,
    /// Selected-message background.
    pub background: Option<String>,
}

/// Projection result plus next max-count state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedReadSearchOutput {
    /// Updated max counts.
    pub next_counts: CollapsedCountsState,
    /// Render projection.
    pub projection: CollapsedReadSearchProjection,
}

/// Top-level renderer projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollapsedReadSearchProjection {
    /// Nothing should render.
    Hidden,
    /// Verbose mode.
    Verbose(CollapsedVerboseDisplay),
    /// Summary mode.
    Summary(CollapsedReadSearchDisplay),
}

/// Verbose display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedVerboseDisplay {
    /// Tool rows.
    pub tool_rows: Vec<CollapsedToolUseVerboseDisplay>,
    /// Optional hook summary.
    pub hook_summary: Option<String>,
    /// Hook detail lines.
    pub hook_info_lines: Vec<String>,
    /// Relevant memories as `(basename, content)`.
    pub relevant_memories: Vec<(String, String)>,
}

/// Verbose tool-use row display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedToolUseVerboseDisplay {
    /// Tool-use ID.
    pub id: String,
    /// Background color.
    pub background: Option<String>,
    /// Loader animation flag.
    pub should_animate: bool,
    /// Loader unresolved flag.
    pub is_unresolved: bool,
    /// Loader error flag.
    pub is_error: bool,
    /// User-facing name.
    pub user_facing_name: String,
    /// Optional tool-use message.
    pub tool_use_message: Option<String>,
    /// Optional tag.
    pub tag: Option<String>,
    /// Optional result summary.
    pub result_message: Option<String>,
}

/// Summary-mode display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedReadSearchDisplay {
    /// Background color.
    pub background: Option<String>,
    /// Margin top.
    pub margin_top: u8,
    /// Whether to show the loader.
    pub show_loader: bool,
    /// Loader animate flag.
    pub should_animate: bool,
    /// Loader unresolved flag.
    pub is_unresolved: bool,
    /// Loader error flag.
    pub is_error: bool,
    /// Dim the summary text when inactive.
    pub dim_summary: bool,
    /// Summary text without ellipsis / expand hint.
    pub summary_text: String,
    /// Active groups show an ellipsis.
    pub show_ellipsis: bool,
    /// Expand hint is always shown in summary mode.
    pub show_expand_hint: bool,
    /// Optional hint lines under the summary.
    pub hint_lines: Vec<String>,
    /// Minimum hint hold time.
    pub min_hint_display_ms: u16,
    /// Optional hook summary line.
    pub hook_summary: Option<String>,
}

/// Format milliseconds as seconds with one decimal place and an `s` suffix.
pub fn format_seconds_one_decimal(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// Format milliseconds as `0s`, `5s`, `1m 5s`, `1h 2m 3s` or `1d 2h 3m`.
pub fn format_duration_compact(ms: u64) -> String {
    if ms < 60_000 {
        if ms == 0 {
            return "0s".into();
        }
        if ms < 1 {
            return format!("{:.1}s", ms as f64 / 1000.0);
        }
        return format!("{}s", ms / 1000);
    }
    let mut days = ms / 86_400_000;
    let mut hours = (ms % 86_400_000) / 3_600_000;
    let mut minutes = (ms % 3_600_000) / 60_000;
    let mut seconds = ((ms % 60_000) as f64 / 1000.0).round() as u64;
    if seconds == 60 {
        seconds = 0;
        minutes += 1;
    }
    if minutes == 60 {
        minutes = 0;
        hours += 1;
    }
    if hours == 24 {
        hours = 0;
        days += 1;
    }
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

/// File name of a path, for verbose memory rows.
pub fn file_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Project one collapsed read/search group into its display form.
pub fn project_collapsed_read_search(
    input: &CollapsedReadSearchInput,
) -> CollapsedReadSearchOutput {
    let next_counts = CollapsedCountsState {
        read_count: input.previous_counts.read_count.max(input.raw_read_count),
        search_count: input
            .previous_counts
            .search_count
            .max(input.raw_search_count),
        list_count: input.previous_counts.list_count.max(input.raw_list_count),
        mcp_call_count: input
            .previous_counts
            .mcp_call_count
            .max(input.mcp_call_count),
        bash_count: input.previous_counts.bash_count.max(input.raw_bash_count),
    };

    if input.verbose {
        return CollapsedReadSearchOutput {
            next_counts,
            projection: CollapsedReadSearchProjection::Verbose(build_verbose_display(input)),
        };
    }

    let read_count = next_counts.read_count;
    let search_count = next_counts.search_count;
    let list_count = next_counts.list_count;
    let mcp_call_count = next_counts.mcp_call_count;
    let bash_count = if input.fullscreen_enabled {
        next_counts
            .bash_count
            .saturating_sub(input.git_op_bash_count)
    } else {
        0
    };

    let has_memory_ops = input.memory_search_count > 0
        || input.memory_read_count > 0
        || input.memory_write_count > 0;
    let has_team_memory_ops = input.team_memory_search_count > 0
        || input.team_memory_read_count > 0
        || input.team_memory_write_count > 0;
    let has_non_memory_ops = search_count > 0
        || read_count > 0
        || list_count > 0
        || input.repl_count > 0
        || mcp_call_count > 0
        || bash_count > 0
        || input.git_op_bash_count > 0;

    if !has_memory_ops && !has_team_memory_ops && !has_non_memory_ops {
        return CollapsedReadSearchOutput {
            next_counts,
            projection: CollapsedReadSearchProjection::Hidden,
        };
    }

    let summary_text = build_summary_text(
        input,
        search_count,
        read_count,
        list_count,
        mcp_call_count,
        bash_count,
    );
    let hint_lines = compute_incoming_hint(input)
        .map(|hint| split_hint_lines(&hint, &compute_shell_progress_suffix(input)))
        .unwrap_or_default();
    let hook_summary = input.hook_total_ms.filter(|ms| *ms > 0).map(|ms| {
        format!(
            "Ran {} PreToolUse {} ({})",
            input.hook_count,
            if input.hook_count == 1 {
                "hook"
            } else {
                "hooks"
            },
            format_seconds_one_decimal(ms)
        )
    });

    CollapsedReadSearchOutput {
        next_counts,
        projection: CollapsedReadSearchProjection::Summary(CollapsedReadSearchDisplay {
            background: input.background.clone(),
            margin_top: 1,
            show_loader: input.is_active_group,
            should_animate: input.is_active_group && input.should_animate,
            is_unresolved: input.is_active_group,
            is_error: input
                .tool_use_ids
                .iter()
                .any(|id| input.errored_tool_use_ids.contains(id)),
            dim_summary: !input.is_active_group,
            summary_text,
            show_ellipsis: input.is_active_group,
            show_expand_hint: true,
            hint_lines,
            min_hint_display_ms: MIN_HINT_DISPLAY_MS,
            hook_summary,
        }),
    }
}

fn build_verbose_display(input: &CollapsedReadSearchInput) -> CollapsedVerboseDisplay {
    let tool_rows = input
        .verbose_tool_uses
        .iter()
        .filter_map(|tool_use| {
            let user_facing_name = tool_use.user_facing_name.clone()?;
            let is_resolved = input.resolved_tool_use_ids.contains(&tool_use.id);
            let is_error = input.errored_tool_use_ids.contains(&tool_use.id);
            let is_in_progress = input.in_progress_tool_use_ids.contains(&tool_use.id);
            Some(CollapsedToolUseVerboseDisplay {
                id: tool_use.id.clone(),
                background: input.background.clone(),
                should_animate: input.should_animate && is_in_progress,
                is_unresolved: !is_resolved,
                is_error,
                user_facing_name,
                tool_use_message: tool_use
                    .tool_use_message
                    .clone()
                    .filter(|message| !message.is_empty()),
                tag: tool_use.tag.clone(),
                result_message: (is_resolved && !is_error)
                    .then(|| tool_use.result_message.clone())
                    .flatten(),
            })
        })
        .collect();

    let hook_summary = (!input.hook_infos.is_empty()).then(|| {
        format!(
            "Ran {} PreToolUse {} ({})",
            input.hook_count,
            if input.hook_count == 1 {
                "hook"
            } else {
                "hooks"
            },
            format_seconds_one_decimal(input.hook_total_ms.unwrap_or(0))
        )
    });
    let hook_info_lines = input
        .hook_infos
        .iter()
        .map(|info| {
            format!(
                "{} ({})",
                info.command,
                format_seconds_one_decimal(info.duration_ms)
            )
        })
        .collect();
    let relevant_memories = input
        .relevant_memories
        .iter()
        .map(|memory| (file_name_from_path(&memory.path), memory.content.clone()))
        .collect();

    CollapsedVerboseDisplay {
        tool_rows,
        hook_summary,
        hook_info_lines,
        relevant_memories,
    }
}

fn build_summary_text(
    input: &CollapsedReadSearchInput,
    search_count: usize,
    read_count: usize,
    list_count: usize,
    mcp_call_count: usize,
    bash_count: usize,
) -> String {
    let mut parts = Vec::new();

    if input.fullscreen_enabled && !input.commits.is_empty() {
        for kind in ["committed", "amended", "cherry-picked"] {
            let shas = input
                .commits
                .iter()
                .filter(|commit| commit.kind == kind)
                .map(|commit| commit.sha.clone())
                .collect::<Vec<_>>();
            if !shas.is_empty() {
                push_summary_part(
                    &mut parts,
                    match kind {
                        "committed" => "committed",
                        "amended" => "amended commit",
                        _ => "cherry-picked",
                    },
                    shas.join(", "),
                );
            }
        }
    }
    if input.fullscreen_enabled && !input.pushes.is_empty() {
        push_summary_part(
            &mut parts,
            "pushed to",
            dedup_preserving_order(&input.pushes).join(", "),
        );
    }
    if input.fullscreen_enabled && !input.branches.is_empty() {
        for branch in &input.branches {
            push_summary_part(
                &mut parts,
                if branch.action == "merged" {
                    "merged"
                } else {
                    "rebased onto"
                },
                branch.reference.clone(),
            );
        }
    }
    if input.fullscreen_enabled && !input.prs.is_empty() {
        for pr in &input.prs {
            push_summary_part(
                &mut parts,
                match pr.action {
                    CollapsedPrAction::Created => "created",
                    CollapsedPrAction::Edited => "edited",
                    CollapsedPrAction::Merged => "merged",
                    CollapsedPrAction::Commented => "commented on",
                    CollapsedPrAction::Closed => "closed",
                    CollapsedPrAction::Ready => "marked ready",
                },
                format!("PR #{}", pr.number),
            );
        }
    }
    if search_count > 0 {
        parts.push(format!(
            "{} {} {}",
            pick_verb(
                input.is_active_group,
                parts.is_empty(),
                "Searching for",
                "searching for",
                "Searched for",
                "searched for"
            ),
            search_count,
            plural_word(search_count, "pattern", "patterns")
        ));
    }
    if read_count > 0 {
        parts.push(format!(
            "{} {} {}",
            pick_verb(
                input.is_active_group,
                parts.is_empty(),
                "Reading",
                "reading",
                "Read",
                "read"
            ),
            read_count,
            plural_word(read_count, "file", "files")
        ));
    }
    if list_count > 0 {
        parts.push(format!(
            "{} {} {}",
            pick_verb(
                input.is_active_group,
                parts.is_empty(),
                "Listing",
                "listing",
                "Listed",
                "listed"
            ),
            list_count,
            plural_word(list_count, "directory", "directories")
        ));
    }
    if input.repl_count > 0 {
        parts.push(format!(
            "{} {} {}",
            if input.is_active_group {
                "REPL'ing"
            } else {
                "REPL'd"
            },
            input.repl_count,
            plural_word(input.repl_count, "time", "times")
        ));
    }
    if mcp_call_count > 0 {
        let server_label = if input.mcp_server_names.is_empty() {
            "MCP".to_string()
        } else {
            input.mcp_server_names.join(", ")
        };
        let suffix = if mcp_call_count > 1 {
            format!(" {} times", mcp_call_count)
        } else {
            String::new()
        };
        parts.push(format!(
            "{} {}{}",
            pick_verb(
                input.is_active_group,
                parts.is_empty(),
                "Querying",
                "querying",
                "Queried",
                "queried"
            ),
            server_label,
            suffix
        ));
    }
    if input.fullscreen_enabled && bash_count > 0 {
        parts.push(format!(
            "{} {} bash {}",
            pick_verb(
                input.is_active_group,
                parts.is_empty(),
                "Running",
                "running",
                "Ran",
                "ran"
            ),
            bash_count,
            plural_word(bash_count, "command", "commands")
        ));
    }

    let had_non_mem = !parts.is_empty();
    append_memory_parts(
        &mut parts,
        had_non_mem,
        input.is_active_group,
        input.memory_read_count,
        input.memory_search_count,
        input.memory_write_count,
        false,
    );
    let had_any_before_team = !parts.is_empty();
    append_memory_parts(
        &mut parts,
        had_any_before_team,
        input.is_active_group,
        input.team_memory_read_count,
        input.team_memory_search_count,
        input.team_memory_write_count,
        true,
    );

    parts.join(", ")
}

fn compute_incoming_hint(input: &CollapsedReadSearchInput) -> Option<String> {
    let mut incoming_hint = input
        .latest_display_hint
        .clone()
        .and_then(non_blank_string)
        .or_else(|| {
            input
                .read_file_paths
                .iter()
                .rev()
                .find_map(|path| non_blank_string(path.clone()))
        })
        .or_else(|| {
            input
                .search_args
                .iter()
                .rev()
                .find_map(|arg| non_blank_string(arg.clone()).map(|arg| format!("\"{arg}\"")))
        });

    if input.is_active_group {
        for tool_use_id in &input.tool_use_ids {
            if !input.in_progress_tool_use_ids.contains(tool_use_id) {
                continue;
            }
            let Some(progress) = input
                .latest_progress_updates
                .iter()
                .find(|progress| progress.tool_use_id == *tool_use_id)
            else {
                continue;
            };
            if let CollapsedProgressUpdateKind::ReplToolCallStart {
                tool_name,
                command,
                pattern,
                file_path,
            } = &progress.kind
            {
                incoming_hint = file_path
                    .clone()
                    .and_then(non_blank_string)
                    .or_else(|| {
                        pattern
                            .clone()
                            .and_then(non_blank_string)
                            .map(|pattern| format!("\"{pattern}\""))
                    })
                    .or_else(|| command.clone().and_then(non_blank_string))
                    .or_else(|| non_blank_string(tool_name.clone()));
            }
        }
    }

    incoming_hint
}

fn non_blank_string(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn compute_shell_progress_suffix(input: &CollapsedReadSearchInput) -> String {
    if !(input.fullscreen_enabled && input.is_active_group) {
        return String::new();
    }

    let mut elapsed = None;
    let mut lines = 0usize;
    for tool_use_id in &input.tool_use_ids {
        if !input.in_progress_tool_use_ids.contains(tool_use_id) {
            continue;
        }
        let Some(progress) = input
            .latest_progress_updates
            .iter()
            .find(|progress| progress.tool_use_id == *tool_use_id)
        else {
            continue;
        };
        let (candidate_elapsed, candidate_lines) = match progress.kind {
            CollapsedProgressUpdateKind::BashProgress {
                elapsed_time_seconds,
                total_lines,
            }
            | CollapsedProgressUpdateKind::PowershellProgress {
                elapsed_time_seconds,
                total_lines,
            } => (Some(elapsed_time_seconds), total_lines),
            CollapsedProgressUpdateKind::ReplToolCallStart { .. } => (None, 0),
        };
        if let Some(candidate_elapsed) = candidate_elapsed {
            if elapsed.is_none() || candidate_elapsed > elapsed.unwrap() {
                elapsed = Some(candidate_elapsed);
                lines = candidate_lines;
            }
        }
    }

    let Some(elapsed) = elapsed else {
        return String::new();
    };
    if elapsed < 2 {
        return String::new();
    }
    let time = format_duration_compact(elapsed * 1000);
    if lines > 0 {
        format!(
            " ({time} · {lines} {})",
            plural_word(lines, "line", "lines")
        )
    } else {
        format!(" ({time})")
    }
}

fn split_hint_lines(hint: &str, shell_progress_suffix: &str) -> Vec<String> {
    let mut lines = hint
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if let Some(last) = lines.last_mut() {
        last.push_str(shell_progress_suffix);
    }
    lines
}

fn push_summary_part(parts: &mut Vec<String>, verb: &str, body: String) {
    let verb = if parts.is_empty() {
        capitalize_ascii_first(verb)
    } else {
        verb.to_string()
    };
    parts.push(format!("{verb} {body}"));
}

fn append_memory_parts(
    parts: &mut Vec<String>,
    had_preceding_parts: bool,
    is_active_group: bool,
    read_count: usize,
    search_count: usize,
    write_count: usize,
    team: bool,
) {
    let mut local_count = usize::from(had_preceding_parts);
    if read_count > 0 {
        parts.push(format!(
            "{} {}{} {}",
            pick_verb(
                is_active_group,
                local_count == 0,
                "Recalling",
                "recalling",
                "Recalled",
                "recalled"
            ),
            read_count,
            if team { " team" } else { "" },
            plural_word(read_count, "memory", "memories")
        ));
        local_count += 1;
    }
    if search_count > 0 {
        parts.push(format!(
            "{}{} memories",
            pick_verb(
                is_active_group,
                local_count == 0,
                "Searching",
                "searching",
                "Searched",
                "searched"
            ),
            if team { " team" } else { "" }
        ));
        local_count += 1;
    }
    if write_count > 0 {
        parts.push(format!(
            "{} {}{} {}",
            pick_verb(
                is_active_group,
                local_count == 0,
                "Writing",
                "writing",
                "Wrote",
                "wrote"
            ),
            write_count,
            if team { " team" } else { "" },
            plural_word(write_count, "memory", "memories")
        ));
    }
}

fn pick_verb(
    is_active_group: bool,
    is_first: bool,
    active_first: &str,
    active_rest: &str,
    done_first: &str,
    done_rest: &str,
) -> String {
    if is_active_group {
        if is_first {
            active_first
        } else {
            active_rest
        }
    } else if is_first {
        done_first
    } else {
        done_rest
    }
    .to_string()
}

fn plural_word<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

fn dedup_preserving_order(values: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        if !out.contains(value) {
            out.push(value.clone());
        }
    }
    out
}

fn capitalize_ascii_first(text: &str) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_uppercase().collect::<String>() + chars.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> CollapsedReadSearchInput {
        CollapsedReadSearchInput {
            previous_counts: CollapsedCountsState::default(),
            raw_search_count: 0,
            raw_read_count: 0,
            raw_list_count: 0,
            repl_count: 0,
            memory_search_count: 0,
            memory_read_count: 0,
            memory_write_count: 0,
            team_memory_search_count: 0,
            team_memory_read_count: 0,
            team_memory_write_count: 0,
            mcp_call_count: 0,
            raw_bash_count: 0,
            git_op_bash_count: 0,
            read_file_paths: Vec::new(),
            search_args: Vec::new(),
            latest_display_hint: None,
            tool_use_ids: Vec::new(),
            resolved_tool_use_ids: HashSet::new(),
            errored_tool_use_ids: HashSet::new(),
            in_progress_tool_use_ids: HashSet::new(),
            latest_progress_updates: Vec::new(),
            commits: Vec::new(),
            pushes: Vec::new(),
            branches: Vec::new(),
            prs: Vec::new(),
            mcp_server_names: Vec::new(),
            hook_count: 0,
            hook_total_ms: None,
            hook_infos: Vec::new(),
            relevant_memories: Vec::new(),
            verbose_tool_uses: Vec::new(),
            should_animate: true,
            verbose: false,
            is_active_group: false,
            fullscreen_enabled: false,
            background: Some("messageActionsBackground".into()),
        }
    }

    #[test]
    fn hidden_only_when_no_counts_exist() {
        let output = project_collapsed_read_search(&base_input());
        assert_eq!(output.projection, CollapsedReadSearchProjection::Hidden);

        let mut input = base_input();
        input.previous_counts.read_count = 2;
        let output = project_collapsed_read_search(&input);
        let CollapsedReadSearchProjection::Summary(display) = output.projection else {
            panic!("expected summary");
        };
        assert_eq!(display.summary_text, "Read 2 files");
    }

    #[test]
    fn summary_keeps_monotonic_counts_and_memory_parts() {
        let mut input = base_input();
        input.previous_counts = CollapsedCountsState {
            read_count: 1,
            search_count: 3,
            list_count: 0,
            mcp_call_count: 0,
            bash_count: 0,
        };
        input.memory_read_count = 1;
        input.memory_search_count = 1;
        input.team_memory_write_count = 2;

        let output = project_collapsed_read_search(&input);
        assert_eq!(output.next_counts.search_count, 3);
        let CollapsedReadSearchProjection::Summary(display) = output.projection else {
            panic!("expected summary");
        };
        assert_eq!(
            display.summary_text,
            "Searched for 3 patterns, read 1 file, recalled 1 memory, searched memories, wrote 2 team memories"
        );
        assert!(display.dim_summary);
    }

    #[test]
    fn summary_prioritizes_git_and_fullscreen_parts() {
        let mut input = base_input();
        input.fullscreen_enabled = true;
        input.is_active_group = true;
        input.raw_bash_count = 5;
        input.git_op_bash_count = 2;
        input.commits = vec![
            CollapsedCommit {
                kind: "committed".into(),
                sha: "abc123".into(),
            },
            CollapsedCommit {
                kind: "amended".into(),
                sha: "def456".into(),
            },
        ];
        input.pushes = vec!["main".into(), "main".into(), "release".into()];
        input.branches = vec![CollapsedBranchSummary {
            action: "merged".into(),
            reference: "feature/x".into(),
        }];
        input.prs = vec![CollapsedPr {
            action: CollapsedPrAction::Created,
            number: 42,
        }];
        input.mcp_call_count = 2;
        input.mcp_server_names = vec!["GitHub".into()];
        input.raw_search_count = 1;

        let output = project_collapsed_read_search(&input);
        let CollapsedReadSearchProjection::Summary(display) = output.projection else {
            panic!("expected summary");
        };
        assert_eq!(
            display.summary_text,
            "Committed abc123, amended commit def456, pushed to main, release, merged feature/x, created PR #42, searching for 1 pattern, querying GitHub 2 times, running 3 bash commands"
        );
        assert!(display.show_loader);
        assert!(display.show_ellipsis);
    }

    #[test]
    fn active_hint_uses_repl_progress_and_shell_suffix() {
        let mut input = base_input();
        input.is_active_group = true;
        input.fullscreen_enabled = true;
        input.raw_read_count = 1;
        input.tool_use_ids = vec!["u1".into(), "u2".into()];
        input.in_progress_tool_use_ids = HashSet::from(["u1".into(), "u2".into()]);
        input.latest_display_hint = Some("old".into());
        input.latest_progress_updates = vec![
            CollapsedProgressUpdate {
                tool_use_id: "u1".into(),
                kind: CollapsedProgressUpdateKind::ReplToolCallStart {
                    tool_name: "Read".into(),
                    command: None,
                    pattern: None,
                    file_path: Some("src/lib.rs".into()),
                },
            },
            CollapsedProgressUpdate {
                tool_use_id: "u2".into(),
                kind: CollapsedProgressUpdateKind::BashProgress {
                    elapsed_time_seconds: 5,
                    total_lines: 12,
                },
            },
        ];

        let output = project_collapsed_read_search(&input);
        let CollapsedReadSearchProjection::Summary(display) = output.projection else {
            panic!("expected summary");
        };
        assert_eq!(display.hint_lines, vec!["src/lib.rs (5s · 12 lines)"]);
        assert_eq!(display.min_hint_display_ms, MIN_HINT_DISPLAY_MS);
    }

    #[test]
    fn inactive_summary_keeps_latest_hint_until_caller_replaces_group() {
        let mut input = base_input();
        input.raw_search_count = 1;
        input.latest_display_hint = Some("\"foo\"".into());
        input.search_args = vec!["foo".into()];

        let output = project_collapsed_read_search(&input);
        let CollapsedReadSearchProjection::Summary(display) = output.projection else {
            panic!("expected summary");
        };
        assert_eq!(display.hint_lines, vec!["\"foo\""]);
        assert!(!display.show_loader);
        assert!(display.dim_summary);
    }

    #[test]
    fn summary_hint_lines_skip_blank_rows() {
        let mut input = base_input();
        input.raw_read_count = 1;
        input.read_file_paths = vec!["src/lib.rs".into()];
        input.latest_display_hint = Some("\n\n".into());

        let output = project_collapsed_read_search(&input);
        let CollapsedReadSearchProjection::Summary(display) = output.projection else {
            panic!("expected summary");
        };
        assert_eq!(display.hint_lines, vec!["src/lib.rs"]);

        input.latest_display_hint = Some("first\n\nsecond\n".into());
        let output = project_collapsed_read_search(&input);
        let CollapsedReadSearchProjection::Summary(display) = output.projection else {
            panic!("expected summary");
        };
        assert_eq!(display.hint_lines, vec!["first", "second"]);
    }

    #[test]
    fn verbose_projection_shapes_rows_hook_lines_and_memories() {
        let mut input = base_input();
        input.verbose = true;
        input.tool_use_ids = vec!["u1".into(), "u2".into()];
        input.resolved_tool_use_ids = HashSet::from(["u2".into()]);
        input.in_progress_tool_use_ids = HashSet::from(["u1".into()]);
        input.errored_tool_use_ids = HashSet::from(["u2".into()]);
        input.verbose_tool_uses = vec![
            CollapsedToolUseVerboseInput {
                id: "u1".into(),
                user_facing_name: Some("Read".into()),
                tool_use_message: Some("foo.rs".into()),
                tag: Some("cached".into()),
                result_message: Some("123 lines".into()),
            },
            CollapsedToolUseVerboseInput {
                id: "u2".into(),
                user_facing_name: Some("Grep".into()),
                tool_use_message: None,
                tag: None,
                result_message: Some("match".into()),
            },
            CollapsedToolUseVerboseInput {
                id: "u3".into(),
                user_facing_name: None,
                tool_use_message: None,
                tag: None,
                result_message: None,
            },
        ];
        input.hook_count = 2;
        input.hook_total_ms = Some(1_500);
        input.hook_infos = vec![CollapsedHookInfo {
            command: "lint".into(),
            duration_ms: 500,
        }];
        input.relevant_memories = vec![CollapsedMemoryEntry {
            path: "/tmp/memory.md".into(),
            content: "remember me".into(),
        }];

        let output = project_collapsed_read_search(&input);
        let CollapsedReadSearchProjection::Verbose(display) = output.projection else {
            panic!("expected verbose");
        };
        assert_eq!(display.tool_rows.len(), 2);
        assert_eq!(display.tool_rows[0].user_facing_name, "Read");
        assert!(display.tool_rows[0].should_animate);
        assert_eq!(display.tool_rows[0].result_message, None);
        assert!(display.tool_rows[1].is_error);
        assert_eq!(
            display.hook_summary.as_deref(),
            Some("Ran 2 PreToolUse hooks (1.5s)")
        );
        assert_eq!(display.hook_info_lines, vec!["lint (0.5s)"]);
        assert_eq!(
            display.relevant_memories,
            vec![("memory.md".into(), "remember me".into())]
        );
    }

    #[test]
    fn helper_formatters_produce_expected_shapes() {
        assert_eq!(format_seconds_one_decimal(1_234), "1.2s");
        assert_eq!(format_duration_compact(0), "0s");
        assert_eq!(format_duration_compact(5_000), "5s");
        assert_eq!(format_duration_compact(65_000), "1m 5s");
        assert_eq!(file_name_from_path("/tmp/a.txt"), "a.txt");
        // A backslash is a separator only where the OS says so; on Unix it is
        // part of the name, which is what `Path::file_name` reports there.
        #[cfg(windows)]
        assert_eq!(file_name_from_path(r"C:\tmp\a.txt"), "a.txt");
    }
}
