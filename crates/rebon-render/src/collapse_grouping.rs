//! Pure logic for folding consecutive read, search, list, MCP and bash tool
//! uses into one collapsed group.
//!
//! This module provides the building blocks for collapsing consecutive
//! Read/Search/List/MCP/Bash tool uses into a single
//! [`crate::CollapsedReadSearchInput`] payload. Because this crate is a
//! projection with no IO and no runtime registries, memory-path detection
//! is expressed as a pluggable policy rather than a direct dependency on
//! whatever the host uses to resolve those paths, and the caller drives the
//! per-row iteration using its own message type.
//!
//! The high-level flow for the caller:
//!
//! 1. For each tool use, classify it with [`classify_tool_use`].
//! 2. Feed classifications + tool-result events into an
//!    [`Aggregator`].
//! 3. Whenever the group breaks (assistant text, non-collapsible tool
//!    use), call [`Aggregator::finalize`] to produce a
//!    [`crate::CollapsedReadSearchInput`] and start a fresh
//!    aggregator.
//!
//! The same [`Aggregator::finalize`] is used for the in-flight
//! streaming overlay — pass `is_active_group = true` so the projection
//! renders verbs in the present tense ("Searching for 2 patterns…").

use std::collections::{HashMap, HashSet};

use serde_json::Value as JsonValue;

use crate::collapsed_read_search::{
    CollapsedCommit, CollapsedCountsState, CollapsedPr, CollapsedProgressUpdate,
    CollapsedReadSearchInput,
};

/// Maximum number of characters kept on a bash command hint line.
pub const MAX_HINT_CHARS: usize = 300;

/// Pluggable memory-path predicates. The default policy reports
/// everything as non-memory; a caller that can resolve a memory root
/// replaces the callbacks.
///
/// The four predicates classify a path as an auto-managed memory file,
/// a memory directory, a glob pattern rooted at a memory directory, or a
/// shell command targeting one.
pub struct MemoryPathPolicy {
    /// Returns true when the argument is an auto-managed memory file.
    pub is_memory_file: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// Returns true when the argument is a memory directory.
    pub is_memory_dir: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// Returns true when the argument is a glob pattern rooted at a
    /// memory directory.
    pub is_memory_pattern: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// Returns true when the argument is a shell command that targets
    /// a memory path (e.g. `grep … ~/.rebon/memory/…`).
    pub is_shell_cmd_targeting_memory: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl std::fmt::Debug for MemoryPathPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryPathPolicy").finish_non_exhaustive()
    }
}

impl Default for MemoryPathPolicy {
    fn default() -> Self {
        Self {
            is_memory_file: Box::new(|_| false),
            is_memory_dir: Box::new(|_| false),
            is_memory_pattern: Box::new(|_| false),
            is_shell_cmd_targeting_memory: Box::new(|_| false),
        }
    }
}

/// Classification of a single tool use for collapse purposes.
///
/// A strongly-typed enum rather than a flag bag, so each consumer matches on
/// exactly the shape it handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolClass {
    /// The tool use is not collapsible — callers should flush the
    /// current group and emit the row as-is.
    NotCollapsible,
    /// Read a file (`Read`, or `Bash` reading a file). `file_path`
    /// is populated when the tool exposes one.
    Read {
        /// Path passed to the tool, if any.
        file_path: Option<String>,
        /// `true` if the path is an auto-managed memory file.
        memory: bool,
        /// Display hint fallback (e.g. for Bash `cat` commands).
        hint: Option<String>,
    },
    /// Search the filesystem (`Grep`, `Glob`, `Bash` regex commands).
    Search {
        /// Literal pattern when known.
        pattern: Option<String>,
        /// `true` if the search targets memory paths.
        memory: bool,
        /// Display hint fallback.
        hint: Option<String>,
    },
    /// List a directory (`LS`, `Bash ls`, etc.).
    List {
        /// Display hint fallback (typically the command).
        hint: Option<String>,
    },
    /// REPL wrapper (`repl` / `REPL`). Counted and rendered in the
    /// summary, but never gets a verbose row; does not break the group.
    Repl,
    /// Write/Edit on an auto-managed memory file.
    MemoryWrite,
    /// MCP tool call (`mcp__server__method`).
    Mcp {
        /// Server segment (`slack` in `mcp__slack__read_messages`).
        server_name: String,
        /// Display hint fallback.
        hint: Option<String>,
    },
    /// Non-search/read Bash command under fullscreen mode — counted
    /// separately so the summary says "Ran N bash commands".
    Bash {
        /// The raw command string, preserved so results can later be
        /// scanned for commit SHAs / PR URLs.
        command: String,
        /// Display hint fallback.
        hint: Option<String>,
    },
    /// Meta-tool absorbed silently (`Snip`, `ToolSearch`).
    AbsorbedSilently,
}

impl ToolClass {
    /// True when this classification does not break a collapse group.
    pub fn is_collapsible(&self) -> bool {
        !matches!(self, ToolClass::NotCollapsible)
    }
}

/// Options controlling [`classify_tool_use`] behaviour that can't be
/// derived from the tool name and input alone.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClassifyOptions {
    /// Controls whether `Bash` falls into `Bash { .. }` or is rejected as
    /// non-collapsible.
    pub fullscreen: bool,
}

/// Classify a single tool invocation.
///
/// Recognised tool names:
///
/// * `Read` / `Write` / `Edit`
/// * `Grep` / `Glob`
/// * `LS`
/// * `Bash` / `PowerShell`
/// * `mcp__*` → MCP call
/// * `ToolSearch` / `Snip` → absorbed silently
/// * `repl` / `REPL` → counted in the summary, no verbose row
pub fn classify_tool_use(
    tool_name: &str,
    input: &JsonValue,
    policy: &MemoryPathPolicy,
    options: ClassifyOptions,
) -> ToolClass {
    // MCP tool: `mcp__<server>__<method>`.
    if let Some(rest) = tool_name.strip_prefix("mcp__") {
        let server = rest.split("__").next().unwrap_or(rest).to_string();
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| format!("\"{q}\""));
        return ToolClass::Mcp {
            server_name: server,
            hint: query,
        };
    }

    match tool_name {
        "Read" => {
            let file_path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let memory = file_path
                .as_deref()
                .map(|p| (policy.is_memory_file)(p))
                .unwrap_or(false);
            ToolClass::Read {
                hint: file_path.clone(),
                file_path,
                memory,
            }
        }
        // `Search` is the ACP-normalized label the CLI reducer assigns
        // to any tool whose ACP `ToolKind` is `Search` (rebon-core
        // maps both `Grep` and `Glob` to that kind). The raw_input still
        // exposes `pattern`, so the classification is identical.
        "Grep" | "Glob" | "Search" => {
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let memory = is_memory_search(input, policy);
            let hint = pattern.as_ref().map(|p| format!("\"{p}\""));
            ToolClass::Search {
                pattern,
                memory,
                hint,
            }
        }
        "LS" => {
            let hint = input
                .get("path")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            ToolClass::List { hint }
        }
        "Write" | "Edit" => {
            let file_path = input.get("file_path").and_then(|v| v.as_str());
            let is_memory = file_path
                .map(|p| (policy.is_memory_file)(p))
                .unwrap_or(false);
            if is_memory {
                ToolClass::MemoryWrite
            } else {
                ToolClass::NotCollapsible
            }
        }
        "ToolSearch" | "Snip" => ToolClass::AbsorbedSilently,
        "repl" | "REPL" => ToolClass::Repl,
        "Bash" | "PowerShell" => classify_shell_command(input, policy, options),
        _ => ToolClass::NotCollapsible,
    }
}

fn classify_shell_command(
    input: &JsonValue,
    policy: &MemoryPathPolicy,
    options: ClassifyOptions,
) -> ToolClass {
    let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let analysis_command = strip_leading_cd_prefix(command);
    let memory = (policy.is_shell_cmd_targeting_memory)(command);
    let hint = if command.is_empty() {
        None
    } else {
        Some(command_as_hint(command))
    };
    let normalized_hint = if analysis_command.is_empty() {
        None
    } else {
        Some(command_as_hint(analysis_command))
    };

    if let Some(segment) = find_shell_segment(analysis_command, is_shell_search_command) {
        let pattern = extract_shell_search_pattern(&segment);
        return ToolClass::Search {
            pattern: pattern.clone(),
            memory,
            hint: pattern
                .as_ref()
                .map(|pattern| format!("\"{pattern}\""))
                .or_else(|| normalized_hint.clone())
                .or_else(|| hint.clone()),
        };
    }

    if let Some(segment) = find_shell_segment(analysis_command, is_shell_read_command) {
        let file_path = extract_shell_read_hint(&segment);
        let read_memory = file_path
            .as_deref()
            .map(|path| (policy.is_memory_file)(path))
            .unwrap_or(false);
        return ToolClass::Read {
            file_path: file_path.clone(),
            memory: read_memory,
            hint: file_path
                .or_else(|| normalized_hint.clone())
                .or_else(|| hint.clone()),
        };
    }

    let first = shell_segment_base_command(analysis_command).unwrap_or_default();
    match first.as_str() {
        "ls" | "tree" | "dir" | "du" => ToolClass::List {
            hint: normalized_hint.or_else(|| hint),
        },
        _ if options.fullscreen => ToolClass::Bash {
            command: command.to_string(),
            hint,
        },
        _ => ToolClass::NotCollapsible,
    }
}

fn strip_leading_cd_prefix(command: &str) -> &str {
    let trimmed = command.trim();
    let after_cd = trimmed
        .strip_prefix("cd ")
        .or_else(|| trimmed.strip_prefix("CD "));
    if let Some(after_cd) = after_cd {
        if let Some(sep_pos) = after_cd.find(" && ") {
            return after_cd[sep_pos + 4..].trim_start();
        }
    }
    trimmed
}

fn find_shell_segment(command: &str, predicate: fn(&str) -> bool) -> Option<String> {
    split_shell_segments(command).into_iter().find(|segment| {
        shell_segment_base_command(segment)
            .as_deref()
            .map(predicate)
            .unwrap_or(false)
    })
}

fn shell_segment_base_command(segment: &str) -> Option<String> {
    for token in first_shell_segment_tokens(segment) {
        let token = token.trim_start_matches('!');
        if token.eq_ignore_ascii_case("sudo") {
            continue;
        }
        let base = token
            .rsplit(|c| c == '/' || c == '\\')
            .next()
            .unwrap_or(token);
        let lower = base.to_ascii_lowercase();
        let lower = lower.strip_suffix(".exe").unwrap_or(lower.as_str());
        return Some(lower.to_string());
    }
    None
}

fn is_shell_search_command(command: &str) -> bool {
    matches!(
        command,
        "grep"
            | "rg"
            | "ripgrep"
            | "ag"
            | "ack"
            | "find"
            | "fd"
            | "fdfind"
            | "findstr"
            | "select-string"
            | "get-childitem"
    )
}

fn is_shell_read_command(command: &str) -> bool {
    matches!(command, "cat" | "head" | "tail" | "less" | "more" | "type")
}

fn split_shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        if ch == '\\' && !in_single {
            current.push(ch);
            if let Some(next) = chars.next() {
                current.push(next);
            }
            continue;
        }

        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            '|' | ';' if !in_single && !in_double => {
                push_shell_segment(&mut segments, &mut current);
            }
            '&' if !in_single && !in_double => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                push_shell_segment(&mut segments, &mut current);
            }
            _ => current.push(ch),
        }
    }

    push_shell_segment(&mut segments, &mut current);
    segments
}

fn push_shell_segment(segments: &mut Vec<String>, current: &mut String) {
    let segment = current.trim();
    if !segment.is_empty() {
        segments.push(segment.to_string());
    }
    current.clear();
}

fn extract_shell_read_hint(command: &str) -> Option<String> {
    let tokens = first_shell_segment_tokens(command);
    if tokens.len() < 2 {
        return None;
    }

    for idx in (1..tokens.len()).rev() {
        let token = &tokens[idx];
        if token.starts_with('-') {
            continue;
        }
        if idx > 1 && shell_option_expects_value(&tokens[idx - 1]) {
            continue;
        }
        return Some(token.clone());
    }

    None
}

fn extract_shell_search_pattern(command: &str) -> Option<String> {
    let tokens = first_shell_segment_tokens(command);
    if tokens.len() < 2 {
        return None;
    }

    for idx in 1..tokens.len() {
        if idx > 1 && shell_pattern_option(&tokens[idx - 1]) {
            return Some(tokens[idx].clone());
        }
    }

    let mut skip_next = false;
    for token in tokens.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if token.starts_with('-') {
            skip_next = shell_option_expects_value(token);
            continue;
        }
        return Some(token.clone());
    }

    None
}

fn first_shell_segment_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        if ch == '\\' && !in_single {
            let escaped = chars.peek().copied();
            let should_escape = if in_double {
                matches!(escaped, Some('"' | '\\' | '$' | '`'))
            } else {
                matches!(escaped, Some(next) if next.is_whitespace() || matches!(next, '"' | '\'' | '|' | ';' | '&' | '\\'))
            };
            if should_escape {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            current.push(ch);
            continue;
        }

        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '|' | ';' | '&' if !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                break;
            }
            _ if ch.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn shell_option_expects_value(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    matches!(token, "-A" | "-B" | "-C")
        || matches!(
            lower.as_str(),
            "-n" | "-c"
                | "-e"
                | "-f"
                | "-g"
                | "-m"
                | "-s"
                | "-p"
                | "-i"
                | "-x"
                | "--after-context"
                | "--before-context"
                | "--bytes"
                | "--context"
                | "--depth"
                | "--exclude"
                | "--file"
                | "--filter"
                | "--format"
                | "--glob"
                | "--iglob"
                | "--include"
                | "--lines"
                | "--literalpath"
                | "--max-count"
                | "--path"
                | "--pattern"
                | "--regexp"
                | "--skip"
                | "--type"
                | "--type-not"
        )
}

fn shell_pattern_option(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "-e" | "-name"
            | "-iname"
            | "-path"
            | "-ipath"
            | "-wholename"
            | "-pattern"
            | "-filter"
            | "-match"
            | "-like"
            | "--regexp"
    )
}

fn is_memory_search(input: &JsonValue, policy: &MemoryPathPolicy) -> bool {
    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
        if (policy.is_memory_file)(path) || (policy.is_memory_dir)(path) {
            return true;
        }
    }
    if let Some(glob) = input.get("glob").and_then(|v| v.as_str()) {
        if (policy.is_memory_pattern)(glob) {
            return true;
        }
    }
    if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
        if (policy.is_shell_cmd_targeting_memory)(command) {
            return true;
        }
    }
    false
}

/// Format a bash command for the `⎿` hint: drop blank lines, collapse inline
/// whitespace runs, then cap the total length.
pub fn command_as_hint(command: &str) -> String {
    let mut cleaned = String::from("$ ");
    let mut first = true;
    for line in command.split('\n') {
        let compact = collapse_whitespace(line);
        let compact = compact.trim();
        if compact.is_empty() {
            continue;
        }
        if !first {
            cleaned.push('\n');
        }
        cleaned.push_str(compact);
        first = false;
    }
    if cleaned.chars().count() > MAX_HINT_CHARS {
        let trimmed: String = cleaned.chars().take(MAX_HINT_CHARS - 1).collect();
        format!("{trimmed}…")
    } else {
        cleaned
    }
}

fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_was_ws = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_was_ws {
                out.push(' ');
                prev_was_ws = true;
            }
        } else {
            out.push(ch);
            prev_was_ws = false;
        }
    }
    out
}

/// Accumulator that tracks an in-progress group of collapsible tool uses.
///
/// It is designed to be driven externally: the caller walks its own
/// message stream, classifies each tool use with
/// [`classify_tool_use`], and calls [`Aggregator::push_tool_use`] /
/// [`Aggregator::record_result`] accordingly. When the group ends
/// (text break, non-collapsible tool, or end-of-stream) it calls
/// [`Aggregator::finalize`] to obtain a projection-ready
/// [`CollapsedReadSearchInput`].
#[derive(Debug, Clone, Default)]
pub struct Aggregator {
    /// Tool-use IDs collected into this group, in insertion order.
    pub tool_use_ids: Vec<String>,
    /// Resolved tool-use IDs.
    pub resolved_tool_use_ids: HashSet<String>,
    /// Errored tool-use IDs.
    pub errored_tool_use_ids: HashSet<String>,
    /// In-progress tool-use IDs (those we've seen a tool_use for, but
    /// no result yet).
    pub in_progress_tool_use_ids: HashSet<String>,

    /// Raw search count (includes memory searches).
    pub search_count: usize,
    /// Unique non-bash read file paths.
    pub read_file_paths: Vec<String>,
    read_file_paths_set: HashSet<String>,
    /// Read operations without a file path (e.g. bash `cat`, `head`).
    pub read_operation_count: usize,
    /// List count.
    pub list_count: usize,
    /// REPL count (rendered in the summary; no verbose row).
    pub repl_count: usize,

    /// Memory operations.
    pub memory_search_count: usize,
    memory_read_file_paths_set: HashSet<String>,
    /// Memory write count.
    pub memory_write_count: usize,

    /// Non-memory search patterns, in insertion order.
    pub non_mem_search_args: Vec<String>,
    /// Most recent display hint (falls back to the classification hint).
    pub latest_display_hint: Option<String>,

    /// MCP call count.
    pub mcp_call_count: usize,
    /// MCP server names, insertion order.
    pub mcp_server_names: Vec<String>,
    mcp_server_names_set: HashSet<String>,

    /// Raw bash count (non-search/read bash commands in fullscreen mode).
    pub bash_count: usize,
    /// Map of tool-use ID → raw bash command, for later git-op scanning.
    pub bash_commands: HashMap<String, String>,

    /// Commit / push / branch / PR summaries.
    pub commits: Vec<CollapsedCommit>,
    /// Push branch names (dedup handled at render time).
    pub pushes: Vec<String>,
    /// Merge / rebase events.
    pub branches: Vec<crate::CollapsedBranchSummary>,
    /// PR events.
    pub prs: Vec<CollapsedPr>,
    /// Count of bash commands that actually produced a git op.
    pub git_op_bash_count: usize,

    /// PreToolUse hook total duration (absorbed from hook summaries).
    pub hook_total_ms: Option<u64>,
    /// PreToolUse hook count.
    pub hook_count: usize,
    /// PreToolUse hook detail rows.
    pub hook_infos: Vec<crate::CollapsedHookInfo>,

    /// Auto-injected memory attachments absorbed into this group.
    pub relevant_memories: Vec<crate::CollapsedMemoryEntry>,

    /// Per-tool-use progress updates.
    pub latest_progress_updates: Vec<CollapsedProgressUpdate>,

    /// Verbose tool-use rows — populated only when the caller needs
    /// `verbose = true` projection.
    pub verbose_tool_uses: Vec<crate::CollapsedToolUseVerboseInput>,
}

impl Aggregator {
    /// Create a fresh, empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// True iff no tool uses have been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.tool_use_ids.is_empty()
    }

    /// Push a classified tool use into the accumulator.
    ///
    /// The `tool_use_id` is the Anthropic-SDK `id` of the tool use
    /// block. The `count` lets grouped-tool-use rows contribute more
    /// than one invocation in a single call.
    pub fn push_tool_use(&mut self, tool_use_id: &str, class: &ToolClass, count: usize) {
        if !self.tool_use_ids.iter().any(|id| id == tool_use_id) {
            self.tool_use_ids.push(tool_use_id.to_string());
            self.in_progress_tool_use_ids
                .insert(tool_use_id.to_string());
        }
        match class {
            ToolClass::NotCollapsible | ToolClass::AbsorbedSilently => {}
            ToolClass::Repl => {
                self.repl_count += count;
            }
            ToolClass::MemoryWrite => {
                self.memory_write_count += count;
            }
            ToolClass::Read {
                file_path,
                memory,
                hint,
            } => {
                if let Some(path) = file_path {
                    if self.read_file_paths_set.insert(path.clone()) {
                        self.read_file_paths.push(path.clone());
                    }
                    if *memory {
                        self.memory_read_file_paths_set.insert(path.clone());
                    } else {
                        self.latest_display_hint = Some(path.clone());
                    }
                } else {
                    self.read_operation_count += count;
                    if let Some(hint) = hint {
                        self.latest_display_hint = Some(hint.clone());
                    }
                }
            }
            ToolClass::Search {
                pattern,
                memory,
                hint,
            } => {
                self.search_count += count;
                if *memory {
                    self.memory_search_count += count;
                } else if let Some(pattern) = pattern {
                    self.non_mem_search_args.push(pattern.clone());
                    self.latest_display_hint = Some(format!("\"{pattern}\""));
                } else if let Some(hint) = hint {
                    self.latest_display_hint = Some(hint.clone());
                }
            }
            ToolClass::List { hint } => {
                self.list_count += count;
                if let Some(hint) = hint {
                    self.latest_display_hint = Some(hint.clone());
                }
            }
            ToolClass::Mcp { server_name, hint } => {
                self.mcp_call_count += count;
                if self.mcp_server_names_set.insert(server_name.clone()) {
                    self.mcp_server_names.push(server_name.clone());
                }
                if let Some(hint) = hint {
                    self.latest_display_hint = Some(hint.clone());
                }
            }
            ToolClass::Bash { command, hint } => {
                self.bash_count += count;
                self.bash_commands
                    .insert(tool_use_id.to_string(), command.clone());
                if let Some(hint) = hint {
                    self.latest_display_hint = Some(hint.clone());
                }
            }
        }
    }

    /// Record a tool result lifecycle status against a tool-use ID.
    pub fn record_result(&mut self, tool_use_id: &str, status: ResultStatus) {
        self.in_progress_tool_use_ids.remove(tool_use_id);
        match status {
            ResultStatus::Resolved => {
                self.resolved_tool_use_ids.insert(tool_use_id.to_string());
            }
            ResultStatus::Errored => {
                self.errored_tool_use_ids.insert(tool_use_id.to_string());
            }
            ResultStatus::Pending => {
                self.in_progress_tool_use_ids
                    .insert(tool_use_id.to_string());
            }
        }
    }

    /// Record that a PreToolUse hook summary row was absorbed into the
    /// group.
    pub fn record_hook_summary(
        &mut self,
        hook_count: usize,
        total_ms: Option<u64>,
        infos: impl IntoIterator<Item = crate::CollapsedHookInfo>,
    ) {
        self.hook_count += hook_count;
        let fallback_ms = if total_ms.is_none() {
            let summed: u64 = self
                .hook_infos
                .iter()
                .map(|info| info.duration_ms)
                .sum::<u64>();
            Some(summed)
        } else {
            None
        };
        let delta = total_ms.or(fallback_ms).unwrap_or(0);
        self.hook_total_ms = Some(self.hook_total_ms.unwrap_or(0) + delta);
        self.hook_infos.extend(infos);
    }

    /// Record an auto-injected relevant memory attachment.
    pub fn record_relevant_memory(&mut self, entry: crate::CollapsedMemoryEntry) {
        self.relevant_memories.push(entry);
    }

    /// Record a streaming progress update for a tool use already in
    /// the accumulator.
    pub fn record_progress_update(&mut self, update: CollapsedProgressUpdate) {
        if let Some(existing) = self
            .latest_progress_updates
            .iter_mut()
            .find(|p| p.tool_use_id == update.tool_use_id)
        {
            *existing = update;
        } else {
            self.latest_progress_updates.push(update);
        }
    }

    /// Record a verbose tool-use row. Only surfaced in verbose mode
    /// projections.
    pub fn record_verbose_tool_use(&mut self, input: crate::CollapsedToolUseVerboseInput) {
        self.verbose_tool_uses.push(input);
    }

    /// Finalize the accumulator into a projection-ready
    /// [`CollapsedReadSearchInput`].
    ///
    /// Arguments:
    ///
    /// * `params.is_active_group` — `true` when the group is still
    ///   accumulating (streaming path) so verbs use the present tense.
    /// * `params.verbose` — true when Ctrl+O is toggled.
    /// * `params.previous_counts` — the monotonic max-count state
    ///   from the previous projection, so counters never go down
    ///   between frames.
    pub fn finalize(self, params: FinalizeParams) -> CollapsedReadSearchInput {
        // The set of memory read paths is a subset of `read_file_paths` by
        // construction (only Read calls populate both).
        let tool_memory_read_count = self.memory_read_file_paths_set.len();
        let relevant_memory_count = self.relevant_memories.len();
        let total_read_count = if !self.read_file_paths.is_empty() {
            self.read_file_paths.len()
        } else {
            self.read_operation_count
        };
        let memory_read_count = tool_memory_read_count + relevant_memory_count;

        let non_mem_read_file_paths: Vec<String> = self
            .read_file_paths
            .iter()
            .filter(|p| !self.memory_read_file_paths_set.contains(p.as_str()))
            .cloned()
            .collect();

        let raw_read_count = total_read_count.saturating_sub(tool_memory_read_count);
        let raw_search_count = self.search_count.saturating_sub(self.memory_search_count);

        CollapsedReadSearchInput {
            previous_counts: params.previous_counts,
            raw_search_count,
            raw_read_count,
            raw_list_count: self.list_count,
            repl_count: self.repl_count,
            memory_search_count: self.memory_search_count,
            memory_read_count,
            memory_write_count: self.memory_write_count,
            team_memory_search_count: 0,
            team_memory_read_count: 0,
            team_memory_write_count: 0,
            mcp_call_count: self.mcp_call_count,
            raw_bash_count: self.bash_count,
            git_op_bash_count: self.git_op_bash_count,
            read_file_paths: non_mem_read_file_paths,
            search_args: self.non_mem_search_args,
            latest_display_hint: self.latest_display_hint,
            tool_use_ids: self.tool_use_ids,
            resolved_tool_use_ids: self.resolved_tool_use_ids,
            errored_tool_use_ids: self.errored_tool_use_ids,
            in_progress_tool_use_ids: self.in_progress_tool_use_ids,
            latest_progress_updates: self.latest_progress_updates,
            commits: self.commits,
            pushes: self.pushes,
            branches: self.branches,
            prs: self.prs,
            mcp_server_names: self.mcp_server_names,
            hook_count: self.hook_count,
            hook_total_ms: self.hook_total_ms,
            hook_infos: self.hook_infos,
            relevant_memories: self.relevant_memories,
            verbose_tool_uses: self.verbose_tool_uses,
            should_animate: params.should_animate,
            verbose: params.verbose,
            is_active_group: params.is_active_group,
            fullscreen_enabled: params.fullscreen_enabled,
            background: params.background,
        }
    }
}

/// Finalization knobs derived from render-time flags.
#[derive(Debug, Clone, Default)]
pub struct FinalizeParams {
    /// Monotonic max-count state from the previous projection.
    pub previous_counts: CollapsedCountsState,
    /// Active-group flag (verb tense / loader visibility).
    pub is_active_group: bool,
    /// Verbose-mode flag.
    pub verbose: bool,
    /// Animate loaders.
    pub should_animate: bool,
    /// Fullscreen flag.
    pub fullscreen_enabled: bool,
    /// Selected-message background color.
    pub background: Option<String>,
}

/// Lifecycle of a tool result for [`Aggregator::record_result`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    /// The tool has not yet finished — still counts as in-progress.
    Pending,
    /// The tool finished successfully.
    Resolved,
    /// The tool finished with an error (`is_error: true`).
    Errored,
}

/// Decide whether a group should start / continue at a given row kind.
///
/// Callers use this after they have classified the row themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowDecision {
    /// The row is a collapsible tool use — push into the current group.
    Absorb,
    /// The row is a tool result for a tool use already in the group.
    AbsorbResult,
    /// The row is a skippable non-breaker (thinking, attachment,
    /// non-relevant-memory system row); if a group is active, defer
    /// it until after the collapsed badge.
    Skip,
    /// The row breaks the group. Flush first, then emit the row.
    Break,
}

/// Convenience: decide whether a tool-result content block should be
/// absorbed into the current aggregator.
///
/// A user message whose tool-result ids are all contained in the group's
/// `tool_use_ids` set is absorbed; any foreign id breaks the group.
pub fn is_collapsible_tool_result(aggregator: &Aggregator, tool_result_ids: &[String]) -> bool {
    !tool_result_ids.is_empty()
        && tool_result_ids
            .iter()
            .all(|id| aggregator.tool_use_ids.iter().any(|own| own == id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> MemoryPathPolicy {
        MemoryPathPolicy {
            is_memory_file: Box::new(|p| {
                p.contains("/.rebon/memory/") || p.ends_with("/MEMORY.md")
            }),
            is_memory_dir: Box::new(|p| {
                p.ends_with("/.rebon/memory") || p.contains("/.rebon/memory/")
            }),
            is_memory_pattern: Box::new(|p| p.contains(".rebon/memory")),
            is_shell_cmd_targeting_memory: Box::new(|c| c.contains(".rebon/memory")),
        }
    }

    #[test]
    fn read_classification_captures_file_path_and_memory_flag() {
        let class = classify_tool_use(
            "Read",
            &json!({"file_path": "/home/u/project/file.rs"}),
            &policy(),
            ClassifyOptions::default(),
        );
        match class {
            ToolClass::Read {
                file_path,
                memory,
                hint,
            } => {
                assert_eq!(file_path.as_deref(), Some("/home/u/project/file.rs"));
                assert!(!memory);
                assert_eq!(hint.as_deref(), Some("/home/u/project/file.rs"));
            }
            other => panic!("expected Read, got {other:?}"),
        }

        let class = classify_tool_use(
            "Read",
            &json!({"file_path": "/home/u/.rebon/memory/x.md"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(class, ToolClass::Read { memory: true, .. }));
    }

    #[test]
    fn grep_classification_reports_pattern_and_memory() {
        let class = classify_tool_use(
            "Grep",
            &json!({"pattern": "TODO", "path": "/home/u/project"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(
            class,
            ToolClass::Search {
                pattern: Some(_),
                memory: false,
                ..
            }
        ));

        let class = classify_tool_use(
            "Grep",
            &json!({"pattern": "TODO", "path": "/home/u/.rebon/memory"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(class, ToolClass::Search { memory: true, .. }));
    }

    #[test]
    fn glob_uses_search_classification() {
        let class = classify_tool_use(
            "Glob",
            &json!({"pattern": "**/*.rs"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(class, ToolClass::Search { memory: false, .. }));
    }

    #[test]
    fn acp_normalized_search_label_classifies_as_search() {
        // The CLI reducer collapses both Grep and Glob into the ACP
        // `ToolKind::Search` label — the classifier must still recognise
        // the normalized name so streaming + committed Grep runs fold
        // into a single "Searching for N patterns" summary.
        let class = classify_tool_use(
            "Search",
            &json!({"pattern": "TODO"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(
            class,
            ToolClass::Search {
                pattern: Some(_),
                memory: false,
                ..
            }
        ));
    }

    #[test]
    fn ls_tool_is_list() {
        let class = classify_tool_use(
            "LS",
            &json!({"path": "/tmp"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(class, ToolClass::List { .. }));
    }

    #[test]
    fn write_on_memory_file_is_memory_write() {
        let class = classify_tool_use(
            "Write",
            &json!({"file_path": "/home/u/.rebon/memory/mood.md"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(class, ToolClass::MemoryWrite));
    }

    #[test]
    fn write_on_non_memory_is_not_collapsible() {
        let class = classify_tool_use(
            "Write",
            &json!({"file_path": "/home/u/project/src/lib.rs"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert_eq!(class, ToolClass::NotCollapsible);
    }

    #[test]
    fn mcp_tool_splits_server_name() {
        let class = classify_tool_use(
            "mcp__slack__read_messages",
            &json!({"query": "hello"}),
            &policy(),
            ClassifyOptions::default(),
        );
        match class {
            ToolClass::Mcp { server_name, hint } => {
                assert_eq!(server_name, "slack");
                assert_eq!(hint.as_deref(), Some("\"hello\""));
            }
            other => panic!("expected Mcp, got {other:?}"),
        }
    }

    #[test]
    fn bash_ls_becomes_list() {
        let class = classify_tool_use(
            "Bash",
            &json!({"command": "ls -la"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(class, ToolClass::List { .. }));
    }

    #[test]
    fn bash_cat_becomes_read_with_path() {
        let class = classify_tool_use(
            "Bash",
            &json!({"command": "cat /tmp/foo.txt"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(
            class,
            ToolClass::Read {
                file_path: Some(ref path),
                memory: false,
                ..
            } if path == "/tmp/foo.txt"
        ));
    }

    #[test]
    fn bash_grep_becomes_search_with_pattern() {
        let class = classify_tool_use(
            "Bash",
            &json!({"command": "rg foo /tmp"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(
            class,
            ToolClass::Search {
                pattern: Some(ref pattern),
                memory: false,
                ..
            } if pattern == "foo"
        ));
    }

    #[test]
    fn bash_cd_prefixed_diff_pipeline_extracts_grep_pattern() {
        let class = classify_tool_use(
            "Bash",
            &json!({
                "command": r#"cd "/repo" && git diff -- crates/rebon-hooks/src/output_protocol.rs crates/rebon-hooks/src/lib.rs 2>&1 | grep -E "^\+[^+]" | grep output"#
            }),
            &policy(),
            ClassifyOptions::default(),
        );
        assert!(matches!(
            class,
            ToolClass::Search {
                pattern: Some(ref pattern),
                memory: false,
                ..
            } if pattern == r"^\+[^+]"
        ));
    }

    #[test]
    fn bash_other_is_non_collapsible_outside_fullscreen() {
        let class = classify_tool_use(
            "Bash",
            &json!({"command": "git status"}),
            &policy(),
            ClassifyOptions::default(),
        );
        assert_eq!(class, ToolClass::NotCollapsible);
    }

    #[test]
    fn bash_other_is_bash_in_fullscreen_mode() {
        let class = classify_tool_use(
            "Bash",
            &json!({"command": "git status"}),
            &policy(),
            ClassifyOptions { fullscreen: true },
        );
        assert!(matches!(class, ToolClass::Bash { .. }));
    }

    #[test]
    fn repl_tool_is_absorbed_silently() {
        assert!(matches!(
            classify_tool_use("repl", &json!({}), &policy(), ClassifyOptions::default()),
            ToolClass::Repl
        ));
    }

    #[test]
    fn unknown_tool_is_not_collapsible() {
        assert_eq!(
            classify_tool_use(
                "CustomTool",
                &json!({}),
                &policy(),
                ClassifyOptions::default()
            ),
            ToolClass::NotCollapsible
        );
    }

    #[test]
    fn aggregator_counts_reads_by_unique_file_path() {
        let mut agg = Aggregator::new();
        let class = ToolClass::Read {
            file_path: Some("/tmp/a.rs".into()),
            memory: false,
            hint: Some("/tmp/a.rs".into()),
        };
        agg.push_tool_use("t1", &class, 1);
        agg.push_tool_use("t2", &class, 1);
        let input = agg.finalize(FinalizeParams::default());
        assert_eq!(input.raw_read_count, 1);
        assert_eq!(input.read_file_paths, vec!["/tmp/a.rs".to_string()]);
    }

    #[test]
    fn aggregator_memory_read_is_excluded_from_raw_counts() {
        let mut agg = Aggregator::new();
        agg.push_tool_use(
            "t1",
            &ToolClass::Read {
                file_path: Some("/home/u/.rebon/memory/m.md".into()),
                memory: true,
                hint: Some("/home/u/.rebon/memory/m.md".into()),
            },
            1,
        );
        agg.push_tool_use(
            "t2",
            &ToolClass::Read {
                file_path: Some("/tmp/b.rs".into()),
                memory: false,
                hint: Some("/tmp/b.rs".into()),
            },
            1,
        );
        let input = agg.finalize(FinalizeParams::default());
        assert_eq!(input.raw_read_count, 1, "memory read excluded from raw");
        assert_eq!(input.memory_read_count, 1);
        assert_eq!(
            input.read_file_paths,
            vec!["/tmp/b.rs".to_string()],
            "memory paths excluded"
        );
    }

    #[test]
    fn aggregator_bash_read_uses_operation_count_fallback() {
        let mut agg = Aggregator::new();
        agg.push_tool_use(
            "t1",
            &ToolClass::Read {
                file_path: None,
                memory: false,
                hint: Some("$ cat x".into()),
            },
            1,
        );
        agg.push_tool_use(
            "t2",
            &ToolClass::Read {
                file_path: None,
                memory: false,
                hint: Some("$ head x".into()),
            },
            1,
        );
        let input = agg.finalize(FinalizeParams::default());
        assert_eq!(input.raw_read_count, 2);
        assert_eq!(input.latest_display_hint.as_deref(), Some("$ head x"));
    }

    #[test]
    fn aggregator_search_memory_excluded_from_raw_count() {
        let mut agg = Aggregator::new();
        agg.push_tool_use(
            "t1",
            &ToolClass::Search {
                pattern: Some("todo".into()),
                memory: true,
                hint: None,
            },
            1,
        );
        agg.push_tool_use(
            "t2",
            &ToolClass::Search {
                pattern: Some("bar".into()),
                memory: false,
                hint: None,
            },
            1,
        );
        let input = agg.finalize(FinalizeParams::default());
        assert_eq!(input.raw_search_count, 1);
        assert_eq!(input.memory_search_count, 1);
        assert_eq!(input.search_args, vec!["bar".to_string()]);
    }

    #[test]
    fn aggregator_mcp_hint_survives_through_finalize() {
        let mut agg = Aggregator::new();
        agg.push_tool_use(
            "t1",
            &ToolClass::Mcp {
                server_name: "slack".into(),
                hint: Some("\"hello\"".into()),
            },
            1,
        );
        agg.push_tool_use(
            "t2",
            &ToolClass::Mcp {
                server_name: "slack".into(),
                hint: Some("\"world\"".into()),
            },
            1,
        );
        let input = agg.finalize(FinalizeParams::default());
        assert_eq!(input.mcp_call_count, 2);
        assert_eq!(input.mcp_server_names, vec!["slack".to_string()]);
        assert_eq!(input.latest_display_hint.as_deref(), Some("\"world\""));
    }

    #[test]
    fn record_result_updates_status_sets() {
        let mut agg = Aggregator::new();
        agg.push_tool_use(
            "t1",
            &ToolClass::Read {
                file_path: Some("/a".into()),
                memory: false,
                hint: None,
            },
            1,
        );
        agg.push_tool_use(
            "t2",
            &ToolClass::Read {
                file_path: Some("/b".into()),
                memory: false,
                hint: None,
            },
            1,
        );
        agg.record_result("t1", ResultStatus::Resolved);
        agg.record_result("t2", ResultStatus::Errored);
        assert!(agg.resolved_tool_use_ids.contains("t1"));
        assert!(agg.errored_tool_use_ids.contains("t2"));
        assert!(!agg.in_progress_tool_use_ids.contains("t1"));
    }

    #[test]
    fn is_collapsible_tool_result_requires_every_id_owned() {
        let mut agg = Aggregator::new();
        agg.push_tool_use(
            "t1",
            &ToolClass::Read {
                file_path: Some("/a".into()),
                memory: false,
                hint: None,
            },
            1,
        );
        assert!(is_collapsible_tool_result(&agg, &["t1".to_string()]));
        assert!(!is_collapsible_tool_result(
            &agg,
            &["t1".to_string(), "t2".to_string()]
        ));
        assert!(!is_collapsible_tool_result(&agg, &[]));
    }

    #[test]
    fn command_as_hint_trims_blank_lines_and_collapses_whitespace() {
        let out = command_as_hint("  ls   -la \n\n  /tmp\n\n");
        assert_eq!(out, "$ ls -la\n/tmp");
    }

    #[test]
    fn command_as_hint_truncates_long_input() {
        let long = "a".repeat(MAX_HINT_CHARS + 100);
        let hinted = command_as_hint(&long);
        assert!(hinted.chars().count() <= MAX_HINT_CHARS);
        assert!(hinted.ends_with('…'));
    }

    #[test]
    fn finalize_respects_monotonic_previous_counts() {
        let agg = Aggregator::new();
        let prev = CollapsedCountsState {
            read_count: 5,
            search_count: 3,
            list_count: 2,
            mcp_call_count: 1,
            bash_count: 0,
        };
        let input = agg.finalize(FinalizeParams {
            previous_counts: prev,
            ..Default::default()
        });
        assert_eq!(input.previous_counts, prev);
    }
}
