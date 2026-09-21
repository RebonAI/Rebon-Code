//! `/context`, `/skills`, `/memory`, `/prune` and `/compact`: what is
//! in the context window and what would change it.
//!
//! `/context` still counts rendered transcript rows rather than engine
//! entries, which is why [`super::SessionCommandInputs`] carries them.

use crate::commands::context_visualization as cv;

use super::{approx_tokens, fmt_tokens, SessionCommandInputs};
use crate::EngineSession;
use rebon_slash_commands::strip_command_prefix;

/// Parsed payload of a `/compact` slash command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactCommand {
    /// Optional extra instructions for the compaction summary.
    pub instructions: Option<String>,
}

/// Recognized `/prune` subcommands.
pub enum PruneCommand {
    /// `/prune` — show help
    Help,
    /// `/prune context` — show token usage breakdown
    Context,
    /// `/prune stats` — show cumulative pruning statistics
    Stats,
    /// `/prune sweep [n]` — clear recent tool results
    Sweep(Option<usize>),
    /// `/prune manual [on|off]` — toggle manual mode
    Manual(Option<bool>),
}

/// Recognize `/compact` with optional per-run summary instructions.
/// Strict prefix match prevents `/compactfoo` from being captured.
pub fn parse_compact_command(text: &str) -> Option<CompactCommand> {
    let rest = strip_command_prefix(text, "compact")?;
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with(':') {
        return None;
    }
    let instructions = rest
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim();
    Some(CompactCommand {
        instructions: (!instructions.is_empty()).then(|| instructions.to_string()),
    })
}

pub fn parse_prune_command(text: &str) -> Option<PruneCommand> {
    let rest = strip_command_prefix(text, "prune")?;
    if rest.is_empty() {
        return Some(PruneCommand::Help);
    }
    let rest = rest.strip_prefix(' ')?.trim();
    let parts: Vec<&str> = rest.split_whitespace().collect();
    match parts.first().copied() {
        Some("context") => Some(PruneCommand::Context),
        Some("stats") => Some(PruneCommand::Stats),
        Some("sweep") => {
            let count = parts.get(1).and_then(|s| s.parse::<usize>().ok());
            Some(PruneCommand::Sweep(count))
        }
        Some("manual") => {
            let state = match parts.get(1).copied() {
                Some("on") | Some("true") => Some(true),
                Some("off") | Some("false") => Some(false),
                _ => None,
            };
            Some(PruneCommand::Manual(state))
        }
        _ => None,
    }
}

const CONTEXT_SCAN_LIMIT_ROWS: usize = 2_000;

pub fn execute_memory_command(session: &EngineSession) -> String {
    rebon_slash_commands::formatters::format_memory_command(
        super::context_sources::gather_memory_files(&session.cwd)
            .into_iter()
            .map(|memory| rebon_slash_commands::formatters::MemoryFileDto {
                path: memory.path,
                tokens: fmt_tokens(memory.tokens as u32),
            })
            .collect(),
    )
}

/// What the context window says about itself when `/context` runs. Read
/// once, because `usage_snapshot` is live and the report has to be
/// internally consistent.
struct ContextWindowMetrics {
    window: u32,
    tokens: u32,
    usage_source: String,
    pct: u32,
    threshold: u32,
    warning: bool,
}

/// The tallies one `/context` run takes over the transcript tail: how
/// many messages and blocks of each kind, the tokens each kind carries,
/// and the five tools with the largest call + result totals.
struct TranscriptScan {
    total_messages: usize,
    scanned_messages: usize,
    skipped_messages: usize,
    user_count: u32,
    assistant_count: u32,
    system_count: u32,
    attachment_count: u32,
    tool_use_count: u32,
    tool_result_count: u32,
    thinking_count: u32,
    text_block_count: u32,
    user_text_tokens: usize,
    assistant_text_tokens: usize,
    tool_use_tokens: usize,
    tool_result_tokens: usize,
    thinking_tokens: usize,
    /// Tool name → (calls, call tokens, result tokens), largest first.
    sorted_tools: Vec<(String, (u32, usize, usize))>,
}

/// Count the transcript tail, and tell `sources` which deferred tools
/// and skills that tail loaded. Only the last `CONTEXT_SCAN_LIMIT_ROWS`
/// rows are read; the rest are reported as skipped.
fn scan_context_transcript(
    all_rows: &[rebon_render::transcript_row::Message],
    sources: &mut super::context_sources::ContextSources,
    session: &EngineSession,
) -> TranscriptScan {
    let total_messages = all_rows.len();
    let skipped_messages = total_messages.saturating_sub(CONTEXT_SCAN_LIMIT_ROWS);
    let rows = &all_rows[skipped_messages..];
    mark_loaded_deferred_tools(
        &mut sources.deferred_builtin_tools,
        &loaded_deferred_tool_names_from_rows(rows),
    );
    sources.skills = loaded_skill_info_from_rows(rows, session.engine_half.skill_registry.as_ref());
    let scanned_messages = rows.len();
    let mut user_count = 0u32;
    let mut assistant_count = 0u32;
    let mut system_count = 0u32;
    let mut attachment_count = 0u32;

    let mut tool_use_count = 0u32;
    let mut tool_result_count = 0u32;
    let mut thinking_count = 0u32;
    let mut text_block_count = 0u32;

    let mut user_text_tokens = 0usize;
    let mut assistant_text_tokens = 0usize;
    let mut tool_use_tokens = 0usize;
    let mut tool_result_tokens = 0usize;
    let mut thinking_tokens = 0usize;

    // Per-tool breakdown: name → (count, call_tokens, result_tokens)
    let mut tool_calls: std::collections::HashMap<String, (u32, usize, usize)> =
        std::collections::HashMap::new();
    let mut tool_names_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for msg in rows {
        if let rebon_render::transcript_row::Message::Assistant(a) = msg {
            for block in &a.message.content {
                if let rebon_render::transcript_row::AssistantContentBlock::ToolUse(tu) = block {
                    tool_names_by_id.insert(tu.id.clone(), tu.name.clone());
                }
            }
        }
    }

    for msg in rows {
        match msg {
            rebon_render::transcript_row::Message::User(u) => {
                user_count += 1;
                for block in &u.message.content {
                    match block {
                        rebon_render::transcript_row::UserContentBlock::Text(t) => {
                            text_block_count += 1;
                            user_text_tokens += approx_tokens(&t.text);
                        }
                        rebon_render::transcript_row::UserContentBlock::ToolResult(tr) => {
                            tool_result_count += 1;
                            let tok = approx_tokens(&tr.content.as_display_string());
                            tool_result_tokens += tok;
                            let tool_name = tool_names_by_id
                                .get(&tr.tool_use_id)
                                .cloned()
                                .unwrap_or_else(|| "<unknown>".into());
                            let entry = tool_calls.entry(tool_name).or_insert((0, 0, 0));
                            entry.2 += tok;
                        }
                        rebon_render::transcript_row::UserContentBlock::Image(_) => {}
                    }
                }
            }
            rebon_render::transcript_row::Message::Assistant(a) => {
                assistant_count += 1;
                for block in &a.message.content {
                    match block {
                        rebon_render::transcript_row::AssistantContentBlock::Text(t) => {
                            text_block_count += 1;
                            assistant_text_tokens += approx_tokens(&t.text);
                        }
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(tu) => {
                            tool_use_count += 1;
                            let tok =
                                approx_tokens(&tu.name) + approx_tokens(&tu.input.to_string());
                            tool_use_tokens += tok;
                            let entry = tool_calls.entry(tu.name.clone()).or_insert((0, 0, 0));
                            entry.0 += 1;
                            entry.1 += tok;
                        }
                        rebon_render::transcript_row::AssistantContentBlock::Thinking(th) => {
                            thinking_count += 1;
                            thinking_tokens += approx_tokens(&th.thinking);
                        }
                        rebon_render::transcript_row::AssistantContentBlock::RedactedThinking(
                            _,
                        ) => {
                            thinking_count += 1;
                        }
                        rebon_render::transcript_row::AssistantContentBlock::GeneratedImage(_) => {}
                        rebon_render::transcript_row::AssistantContentBlock::Other => {}
                    }
                }
            }
            rebon_render::transcript_row::Message::System(_) => system_count += 1,
            rebon_render::transcript_row::Message::Attachment(_) => attachment_count += 1,
            rebon_render::transcript_row::Message::Unknown => {}
        }
    }

    // Sort tools by total tokens (top 5).
    let mut sorted_tools: Vec<_> = tool_calls.into_iter().collect();
    sorted_tools.sort_by(|a, b| (b.1 .1 + b.1 .2).cmp(&(a.1 .1 + a.1 .2)));
    sorted_tools.truncate(5);

    TranscriptScan {
        total_messages,
        scanned_messages,
        skipped_messages,
        user_count,
        assistant_count,
        system_count,
        attachment_count,
        tool_use_count,
        tool_result_count,
        thinking_count,
        text_block_count,
        user_text_tokens,
        assistant_text_tokens,
        tool_use_tokens,
        tool_result_tokens,
        thinking_tokens,
        sorted_tools,
    }
}

pub fn execute_context_command(inputs: &SessionCommandInputs, session: &EngineSession) -> String {
    use crate::commands::context_visualization as cv;

    // Gather side-channel sources (tools split into built-in vs MCP,
    // deferred tools, memory files). Cheap to call — no network and only
    // a handful of stat syscalls + iteration over in-memory registries.
    let mut sources = super::context_sources::ContextSources::gather(session);

    let handle = &session.model.prune_level;
    let budget = &handle.budget;

    // ── Context window metrics ──────────────────────────────────
    let usage = budget.usage_snapshot();
    let metrics = ContextWindowMetrics {
        window: budget.context_window(),
        tokens: usage.tokens,
        usage_source: usage.source.as_str().to_string(),
        pct: budget.usage_percent(),
        threshold: budget.auto_compact_threshold(),
        warning: budget.is_above_warning(),
    };
    let ContextWindowMetrics {
        window,
        tokens,
        pct,
        threshold,
        ..
    } = metrics;

    // ── Scan transcript ─────────────────────────────────────────
    let scan = scan_context_transcript(inputs.rows, &mut sources, session);
    let TranscriptScan {
        total_messages,
        assistant_text_tokens,
        tool_use_tokens,
        tool_result_tokens,
        thinking_tokens,
        user_text_tokens,
        ref sorted_tools,
        ..
    } = scan;

    // ── Build the visualization plan via rebon-misc ─────────────
    let window_usize = window as usize;
    let used_usize = (tokens as usize).min(window_usize);
    let free_tokens = window_usize.saturating_sub(used_usize);
    let autocompact_reserved = window_usize.saturating_sub(threshold as usize);

    let mut categories: Vec<cv::ContextCategory> = Vec::new();
    let messages_tokens = user_text_tokens + assistant_text_tokens;
    if messages_tokens > 0 {
        categories.push(cv::ContextCategory {
            name: "Messages".into(),
            tokens: messages_tokens,
            color_key: "blue".into(),
            is_deferred: false,
        });
    }
    if tool_use_tokens > 0 {
        categories.push(cv::ContextCategory {
            name: "Tool calls".into(),
            tokens: tool_use_tokens,
            color_key: "green".into(),
            is_deferred: false,
        });
    }
    if tool_result_tokens > 0 {
        categories.push(cv::ContextCategory {
            name: "Tool results".into(),
            tokens: tool_result_tokens,
            color_key: "yellow".into(),
            is_deferred: false,
        });
    }
    if thinking_tokens > 0 {
        categories.push(cv::ContextCategory {
            name: "Thinking".into(),
            tokens: thinking_tokens,
            color_key: "magenta".into(),
            is_deferred: false,
        });
    }
    if autocompact_reserved > 0 {
        categories.push(cv::ContextCategory {
            name: cv::RESERVED_CATEGORY_NAME.into(),
            tokens: autocompact_reserved,
            color_key: "amber".into(),
            is_deferred: false,
        });
    }
    if free_tokens > 0 {
        categories.push(cv::ContextCategory {
            name: "Free space".into(),
            tokens: free_tokens,
            color_key: "dim".into(),
            is_deferred: false,
        });
    }

    // Single-row grid of 20 squares, filled proportional to token usage.
    // Filled squares are tagged with the top-weighted "real" category so
    // the plan builder can still assign a meaningful color_key per cell.
    let grid_size = 20usize;
    let filled = if window > 0 {
        ((tokens as u64 * grid_size as u64) / window as u64) as usize
    } else {
        0
    };
    let filled = filled.min(grid_size);
    let dominant = categories
        .iter()
        .filter(|c| c.name != "Free space" && c.name != cv::RESERVED_CATEGORY_NAME && c.tokens > 0)
        .max_by_key(|c| c.tokens);
    let (fill_name, fill_color) = dominant
        .map(|c| (c.name.clone(), c.color_key.clone()))
        .unwrap_or_else(|| ("Messages".into(), "blue".into()));
    let grid_row: Vec<cv::GridSquare> = (0..grid_size)
        .map(|i| {
            if i < filled {
                cv::GridSquare {
                    color_key: fill_color.clone(),
                    category_name: fill_name.clone(),
                    square_fullness: 1.0,
                }
            } else {
                cv::GridSquare {
                    color_key: "dim".into(),
                    category_name: "Free space".into(),
                    square_fullness: 0.0,
                }
            }
        })
        .collect();

    let message_breakdown = if total_messages > 0 {
        Some(cv::MessageBreakdown {
            tool_call_tokens: tool_use_tokens,
            tool_result_tokens,
            attachment_tokens: 0,
            assistant_message_tokens: assistant_text_tokens,
            user_message_tokens: user_text_tokens,
            tool_calls_by_type: sorted_tools
                .iter()
                .map(|(name, (_count, call, res))| cv::MessageToolBreakdown {
                    name: name.clone(),
                    call_tokens: *call,
                    result_tokens: *res,
                })
                .collect(),
            attachments_by_type: Vec::new(),
        })
    } else {
        None
    };

    let input = cv::ContextVisualizationInput {
        header: cv::ContextHeader {
            model: session.model.name.clone(),
            total_tokens: tokens as usize,
            raw_max_tokens: window_usize,
            percentage: pct as f64,
        },
        categories,
        grid_rows: vec![grid_row],
        collapse_status: None,
        memory_files: sources.memory_files.clone(),
        mcp_tools: sources.mcp_tools.clone(),
        deferred_builtin_tools: sources.deferred_builtin_tools.clone(),
        system_tools: sources.system_tools.clone(),
        system_prompt_sections: sources.system_prompt_sections.clone(),
        agents: sources.agents.clone(),
        skills: sources.skills.clone(),
        message_breakdown,
        // The message-breakdown / builtin / system-prompt legend fields on
        // `ContextVisualizationPlan` are gated by `internal_mode` in the
        // plan builder. We surface the breakdown here, so flip it on;
        // the other internal-only input fields are left empty above and thus
        // stay empty in the plan.
        internal_mode: true,
    };

    let plan = cv::build_context_visualization(&input, |path| path.to_string());

    format_context_details(inputs, session, &sources, scan, metrics, &plan)
}

/// What the report suggests doing about the numbers it just showed: a
/// nearly full window, tool results crowding out the conversation, or
/// thinking blocks doing the same.
fn context_suggestions(
    pct: u32,
    tool_result_tokens: usize,
    thinking_tokens: usize,
    total_msg_tokens: usize,
) -> Vec<String> {
    let mut suggestions = Vec::new();
    if pct >= 80 {
        suggestions.push("⚠ Context is ≥80% full. Consider /compact or /clear.".to_string());
    }
    if tool_result_tokens > 0
        && total_msg_tokens > 0
        && tool_result_tokens * 100 / total_msg_tokens > 50
    {
        suggestions.push(format!(
            "ℹ Tool results use ~{}% of message tokens. Consider /prune sweep to clear old results.",
            tool_result_tokens * 100 / total_msg_tokens,
        ));
    }
    if thinking_tokens > 0 && total_msg_tokens > 0 && thinking_tokens * 100 / total_msg_tokens > 20
    {
        suggestions.push(format!(
            "ℹ Thinking blocks use ~{}% of message tokens. Use /effort low to reduce.",
            thinking_tokens * 100 / total_msg_tokens,
        ));
    }
    suggestions
}

/// Turn the visualization plan and the session's own state into the
/// `/context` report. Everything it reads was measured above; nothing
/// here asks the budget or the transcript a second question.
fn format_context_details(
    inputs: &SessionCommandInputs,
    session: &EngineSession,
    sources: &super::context_sources::ContextSources,
    scan: TranscriptScan,
    metrics: ContextWindowMetrics,
    plan: &cv::ContextVisualizationPlan,
) -> String {
    let handle = &session.model.prune_level;
    let budget = &handle.budget;
    let ContextWindowMetrics {
        window,
        tokens,
        usage_source,
        pct,
        threshold,
        warning,
    } = metrics;
    let TranscriptScan {
        total_messages,
        scanned_messages,
        skipped_messages,
        user_count,
        assistant_count,
        system_count,
        attachment_count,
        tool_use_count,
        tool_result_count,
        thinking_count,
        text_block_count,
        user_text_tokens,
        assistant_text_tokens,
        tool_use_tokens,
        tool_result_tokens,
        thinking_tokens,
        sorted_tools,
    } = scan;

    // ── Render plan + session state to text ─────────────────────
    let snap = handle.stats.snapshot();
    let prune_level = handle.get();

    let provider_usage = (inputs.usage.last_turn.input_tokens > 0
        || inputs.usage.last_turn.output_tokens > 0
        || inputs.streaming_token_count > 0)
        .then(
            || rebon_slash_commands::formatters::ContextProviderUsageDto {
                input: fmt_tokens(inputs.usage.last_turn.input_tokens),
                output: fmt_tokens(inputs.usage.last_turn.output_tokens),
                current_output: fmt_tokens(inputs.streaming_token_count),
            },
        );

    let system_prompt_tokens: usize = sources
        .system_prompt_sections
        .iter()
        .map(|s| s.tokens)
        .sum();
    let builtin_tool_tokens: usize = sources.system_tools.iter().map(|t| t.tokens).sum();
    let deferred_tool_tokens: usize = sources
        .deferred_builtin_tools
        .iter()
        .filter(|t| t.is_loaded)
        .map(|t| t.tokens)
        .sum();
    let mcp_tool_tokens: usize = sources.mcp_tools.iter().map(|t| t.tokens).sum();
    let skills_tokens: usize = sources
        .skills
        .as_ref()
        .map(|s| s.tokens)
        .unwrap_or_default();
    let memory_tokens: usize = sources.memory_files.iter().map(|m| m.tokens).sum();

    let token_breakdown = plan.message_breakdown.as_ref().and_then(|mb| {
        let total = mb.user_message_tokens
            + mb.assistant_message_tokens
            + mb.tool_call_tokens
            + mb.tool_result_tokens
            + thinking_tokens;
        (total > 0).then(
            || rebon_slash_commands::formatters::ContextTokenBreakdownDto {
                user_text: (mb.user_message_tokens > 0)
                    .then(|| fmt_tokens(mb.user_message_tokens as u32)),
                assistant_text: (mb.assistant_message_tokens > 0)
                    .then(|| fmt_tokens(mb.assistant_message_tokens as u32)),
                tool_calls: (mb.tool_call_tokens > 0)
                    .then(|| fmt_tokens(mb.tool_call_tokens as u32)),
                tool_results: (mb.tool_result_tokens > 0)
                    .then(|| fmt_tokens(mb.tool_result_tokens as u32)),
                thinking: (thinking_tokens > 0).then(|| fmt_tokens(thinking_tokens as u32)),
                total: fmt_tokens(total as u32),
            },
        )
    });
    let top_tools = plan
        .message_breakdown
        .as_ref()
        .map(|mb| {
            mb.tool_calls_by_type
                .iter()
                .map(|tool| {
                    let count = sorted_tools
                        .iter()
                        .find(|(name, _)| name == &tool.name)
                        .map(|(_, (count, _, _))| *count)
                        .unwrap_or(0);
                    rebon_slash_commands::formatters::ContextTopToolDto {
                        name: tool.name.clone(),
                        count,
                        total: fmt_tokens((tool.call_tokens + tool.result_tokens) as u32),
                        call: fmt_tokens(tool.call_tokens as u32),
                        result: fmt_tokens(tool.result_tokens as u32),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let mut categories = plan
        .visible_categories
        .iter()
        .map(
            |entry| rebon_slash_commands::formatters::ContextCategoryDto {
                name: entry.name.clone(),
                tokens: fmt_tokens(entry.tokens as u32),
                percent: format!("{:.1}", entry.percent_of_raw_max),
            },
        )
        .collect::<Vec<_>>();
    categories.extend(plan.autocompact.iter().map(|entry| {
        rebon_slash_commands::formatters::ContextCategoryDto {
            name: entry.name.clone(),
            tokens: fmt_tokens(entry.tokens as u32),
            percent: format!("{:.1}", entry.percent_of_raw_max),
        }
    }));
    categories.extend(plan.free_space.iter().map(|entry| {
        rebon_slash_commands::formatters::ContextCategoryDto {
            name: entry.name.clone(),
            tokens: fmt_tokens(entry.tokens as u32),
            percent: format!("{:.1}", entry.percent_of_raw_max),
        }
    }));
    let named_tokens =
        |name: &str, tokens: usize| rebon_slash_commands::formatters::ContextNamedTokensDto {
            name: name.to_string(),
            tokens: fmt_tokens(tokens as u32),
        };
    let builtin_loaded = plan
        .builtin_loaded
        .iter()
        .map(|tool| named_tokens(&tool.name, tool.tokens))
        .collect();
    let mcp_loaded = plan
        .mcp_loaded
        .iter()
        .map(|tool| named_tokens(&tool.name, tool.tokens))
        .collect();
    let skill_groups = plan
        .skill_groups
        .iter()
        .map(
            |group| rebon_slash_commands::formatters::ContextSkillGroupDto {
                display_name: group.display_name.clone(),
                items: group
                    .items
                    .iter()
                    .map(|item| named_tokens(&item.name, item.tokens))
                    .collect(),
            },
        )
        .collect();
    let memory_files = plan
        .memory_files
        .iter()
        .map(
            |file| rebon_slash_commands::formatters::ContextMemoryFileDto {
                path: file.display_path.clone(),
                tokens: fmt_tokens(file.tokens as u32),
            },
        )
        .collect();

    let provider_totals = (inputs.usage.total.input_tokens > 0
        || inputs.usage.total.output_tokens > 0
        || inputs.usage.total.prompt_cache_hit_tokens > 0
        || inputs.usage.total.cache_read_input_tokens > 0)
        .then(|| {
            let cache_read = inputs
                .usage
                .total
                .cache_read_input_tokens
                .saturating_add(inputs.usage.total.prompt_cache_hit_tokens);
            let cache_write = inputs
                .usage
                .total
                .cache_creation_input_tokens
                .saturating_add(inputs.usage.total.prompt_cache_miss_tokens);
            rebon_slash_commands::formatters::ContextProviderTotalsDto {
                input: fmt_tokens(inputs.usage.total.input_tokens),
                output: fmt_tokens(inputs.usage.total.output_tokens),
                cache: (cache_read > 0 || cache_write > 0)
                    .then(|| (fmt_tokens(cache_read), fmt_tokens(cache_write))),
            }
        });
    let prune_stats = (snap.requests_processed > 0).then(|| {
        rebon_slash_commands::formatters::ContextPruneStatsDto {
            cleared: snap.tool_results_cleared as u64,
            duplicates: snap.duplicates_removed as u64,
            truncated: snap.messages_truncated as u64,
        }
    });
    let total_msg_tokens = user_text_tokens
        + assistant_text_tokens
        + tool_use_tokens
        + tool_result_tokens
        + thinking_tokens;
    let suggestions =
        context_suggestions(pct, tool_result_tokens, thinking_tokens, total_msg_tokens);

    let grid = plan
        .grid
        .iter()
        .flat_map(|row| row.iter())
        .map(|cell| match cell.kind {
            cv::GridCellKind::FreeSpace => "⬜",
            _ => "⬛",
        })
        .collect();
    rebon_slash_commands::formatters::format_context_command(
        rebon_slash_commands::formatters::ContextCommandDto::Detailed(
            rebon_slash_commands::formatters::ContextDetailsDto {
                model: plan.header.model.clone(),
                provider: session.model.provider_name.clone(),
                grid,
                percent: pct as u8,
                tokens: fmt_tokens(tokens),
                window: fmt_tokens(window),
                usage_source: usage_source.to_string(),
                provider_usage,
                warning,
                prompt_sources: rebon_slash_commands::formatters::ContextPromptSourcesDto {
                    system_prompt: fmt_tokens(system_prompt_tokens as u32),
                    system_prompt_captured: system_prompt_tokens != 0,
                    builtin_tools: fmt_tokens(builtin_tool_tokens as u32),
                    deferred_tools: (deferred_tool_tokens > 0)
                        .then(|| fmt_tokens(deferred_tool_tokens as u32)),
                    mcp_tools: fmt_tokens(mcp_tool_tokens as u32),
                    skills: fmt_tokens(skills_tokens as u32),
                    memory_files: fmt_tokens(memory_tokens as u32),
                },
                messages: rebon_slash_commands::formatters::ContextMessagesDto {
                    total: total_messages,
                    scanned: scanned_messages,
                    skipped: skipped_messages,
                    user: user_count,
                    assistant: assistant_count,
                    system: system_count,
                    attachments: attachment_count,
                },
                blocks: rebon_slash_commands::formatters::ContextBlocksDto {
                    text: text_block_count,
                    tool_use: tool_use_count,
                    tool_result: tool_result_count,
                    thinking: thinking_count,
                },
                token_breakdown,
                top_tools,
                categories,
                builtin_loaded,
                builtin_available: plan.builtin_available.clone(),
                mcp_loaded,
                mcp_available: plan.mcp_available.clone(),
                skill_groups,
                memory_files,
                auto_compact: budget.should_auto_compact() || tokens >= threshold,
                compact_threshold: fmt_tokens(threshold),
                provider_totals,
                prune_level: prune_level.as_config_value().to_string(),
                prune_stats,
                suggestions,
            },
        ),
    )
}

fn mark_loaded_deferred_tools(
    tools: &mut [cv::DeferredBuiltinTool],
    loaded_names: &std::collections::HashSet<String>,
) {
    for tool in tools {
        tool.is_loaded = loaded_names.contains(&tool.name);
    }
}

fn loaded_deferred_tool_names_from_rows(
    rows: &[rebon_render::transcript_row::Message],
) -> std::collections::HashSet<String> {
    let mut tool_search_calls = std::collections::HashSet::new();
    for msg in rows {
        let rebon_render::transcript_row::Message::Assistant(assistant) = msg else {
            continue;
        };
        for block in &assistant.message.content {
            let rebon_render::transcript_row::AssistantContentBlock::ToolUse(tool_use) = block
            else {
                continue;
            };
            if tool_use.name == rebon_tool::TOOL_SEARCH_TOOL_NAME {
                tool_search_calls.insert(tool_use.id.clone());
            }
        }
    }

    let mut loaded = std::collections::HashSet::new();
    for msg in rows {
        let rebon_render::transcript_row::Message::User(user) = msg else {
            continue;
        };
        for block in &user.message.content {
            let rebon_render::transcript_row::UserContentBlock::ToolResult(result) = block else {
                continue;
            };
            if !tool_search_calls.contains(&result.tool_use_id) || result.is_error.unwrap_or(false)
            {
                continue;
            }
            loaded.extend(deferred_tool_names_from_tool_search_result(&result.content));
        }
    }
    loaded
}

fn deferred_tool_names_from_tool_search_result(
    content: &rebon_render::transcript_row::ToolResultContent,
) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content.as_display_string()) else {
        return Vec::new();
    };
    value
        .get("matched_tools")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn loaded_skill_info_from_rows(
    rows: &[rebon_render::transcript_row::Message],
    registry: &rebon_plugin_skill::SkillRegistry,
) -> Option<cv::SkillInfo> {
    use crate::commands::context_visualization as cv;

    let mut skill_calls: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for msg in rows {
        let rebon_render::transcript_row::Message::Assistant(assistant) = msg else {
            continue;
        };
        for block in &assistant.message.content {
            let rebon_render::transcript_row::AssistantContentBlock::ToolUse(tool_use) = block
            else {
                continue;
            };
            if tool_use.name == rebon_tools_core::SKILL_TOOL_NAME {
                skill_calls.insert(
                    tool_use.id.clone(),
                    skill_id_from_tool_input(&tool_use.input),
                );
            }
        }
    }

    if skill_calls.is_empty() {
        return None;
    }

    let mut loaded: std::collections::HashMap<String, (cv::SourceKind, usize)> =
        std::collections::HashMap::new();
    for msg in rows {
        let rebon_render::transcript_row::Message::User(user) = msg else {
            continue;
        };
        for block in &user.message.content {
            let rebon_render::transcript_row::UserContentBlock::ToolResult(result) = block else {
                continue;
            };
            let Some(input_skill_id) = skill_calls.get(&result.tool_use_id) else {
                continue;
            };
            if result.is_error.unwrap_or(false) {
                continue;
            }
            let skill_id = input_skill_id
                .clone()
                .or_else(|| skill_id_from_tool_result(&result.content))
                .unwrap_or_else(|| "<unknown>".into());
            let source = registry
                .get(&skill_id)
                .map(|skill| super::context_sources::skill_source_kind(skill.source))
                .unwrap_or(cv::SourceKind::Plugin);
            let tokens = approx_tokens(&result.content.as_display_string());
            let entry = loaded.entry(skill_id).or_insert((source, 0));
            entry.0 = source;
            entry.1 += tokens;
        }
    }

    let mut entries = loaded
        .into_iter()
        .map(|(name, (source, tokens))| cv::SkillDetail {
            name,
            source,
            tokens,
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let tokens = entries.iter().map(|entry| entry.tokens).sum();
    Some(cv::SkillInfo { tokens, entries })
}

fn skill_id_from_tool_input(input: &serde_json::Value) -> Option<String> {
    input
        .get("skill")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|skill| !skill.is_empty())
        .map(str::to_string)
}

fn skill_id_from_tool_result(
    content: &rebon_render::transcript_row::ToolResultContent,
) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&content.as_display_string()).ok()?;
    value
        .get("skill")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|skill| !skill.is_empty())
        .map(str::to_string)
}

#[cfg(any(test, feature = "test-support"))]
pub fn find_tool_name_for_result(
    rows: &[rebon_render::transcript_row::Message],
    tool_use_id: &str,
) -> String {
    for msg in rows {
        if let rebon_render::transcript_row::Message::Assistant(a) = msg {
            for block in &a.message.content {
                if let rebon_render::transcript_row::AssistantContentBlock::ToolUse(tu) = block {
                    if tu.id == tool_use_id {
                        return tu.name.clone();
                    }
                }
            }
        }
    }
    "<unknown>".into()
}

pub fn execute_prune_command(cmd: PruneCommand, session: &EngineSession) -> String {
    let handle = &session.model.prune_level;
    match cmd {
        PruneCommand::Help => "/prune — Context pruning commands\n\n\
             /prune context   — Show token usage and context window info\n\
             /prune stats     — Show cumulative pruning statistics\n\
             /prune sweep [n] — Clear recent tool results (optional count)\n\
             /prune manual [on|off] — Toggle prune level: on=aggressive, off=off"
            .to_string(),
        PruneCommand::Context => handle.budget.context_info(),
        PruneCommand::Stats => {
            let snap = handle.stats.snapshot();
            let level = handle.get();
            format!("Prune level: {}\n{snap}", level.as_config_value(),)
        }
        PruneCommand::Sweep(count) => {
            handle.request_sweep(count);
            format!(
                "Sweep queued (count: {}). Tool results will be cleared on the next model request.",
                count.map_or("all".to_string(), |n| n.to_string()),
            )
        }
        PruneCommand::Manual(state) => {
            match state {
                Some(true) => {
                    handle.set(rebon_api::PruneLevel::Aggressive);
                    "Context pruning set to aggressive (manual mode on).".to_string()
                }
                Some(false) => {
                    handle.set(rebon_api::PruneLevel::Off);
                    "Context pruning disabled (manual mode off).".to_string()
                }
                None => {
                    // Toggle
                    let current = handle.get();
                    let next = if current == rebon_api::PruneLevel::Off {
                        rebon_api::PruneLevel::Aggressive
                    } else {
                        rebon_api::PruneLevel::Off
                    };
                    handle.set(next);
                    format!(
                        "Context pruning toggled: {} → {}",
                        current.as_config_value(),
                        next.as_config_value(),
                    )
                }
            }
        }
    }
}

/// Arm the engine's in-turn manual compaction for the next model request.
///
/// This is the *fallback* path. At an idle prompt `/compact` runs
/// immediately (see `compact_runtime`), which is what the user asked for and
/// what reports real numbers. It lands here only when a turn is already in
/// flight: that turn owns the live `ContextManager`, and it is going to
/// compact inside itself at the next request anyway, so a second compaction
/// racing it would be work thrown away at best.
pub fn execute_compact_command(session: &EngineSession, instructions: Option<&str>) -> String {
    let handle = &session.model.prune_level;
    let snap = handle.stats.snapshot();
    let budget_info = handle.budget.context_info();
    let instructions = instructions.map(str::trim).filter(|s| !s.is_empty());

    // Set a one-shot flag consumed by the middleware on the very next
    // request. The flag is cleared atomically so the truncation fires
    // exactly once — no sticky state.
    handle
        .budget
        .force_compact_once_with_instructions(instructions.map(str::to_string));

    let instruction_note = instructions
        .map(|value| format!("\n\nCompact instructions:\n{value}"))
        .unwrap_or_default();

    format!(
        "Compaction queued: a turn is in flight, so it runs at the start of the next model request.{instruction_note}\n\n\
         Before compact:\n{budget_info}\n\n\
         Cumulative stats: {snap}"
    )
}
