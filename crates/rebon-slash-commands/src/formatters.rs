//! Side-effect-free slash-command text projections.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostCommandDto {
    pub duration: String,
    pub context_tokens: String,
    pub context_window: String,
    pub context_source: String,
    pub streaming_tokens: String,
    pub total_input: String,
    pub total_output: String,
    pub total_cache_read: String,
    pub total_cache_write: String,
    pub last_input: String,
    pub last_output: String,
    pub usd_summary: Option<String>,
}

pub fn format_cost_command(dto: CostCommandDto) -> String {
    let mut out = format!(
        "Cost estimate (local)\n  session duration: {}\n  context usage: {} / {} tokens ({})\n  current output tokens: {}\n\nProvider usage\n  total input: {}\n  total output: {}\n  total cache read/hit: {}\n  total cache write/miss: {}\n  last turn input: {}\n  last turn output: {}",
        dto.duration, dto.context_tokens, dto.context_window, dto.context_source,
        dto.streaming_tokens, dto.total_input, dto.total_output, dto.total_cache_read,
        dto.total_cache_write, dto.last_input, dto.last_output,
    );
    out.push_str("\n\n");
    out.push_str(dto.usd_summary.as_deref().unwrap_or(
        "USD cost is not available: this session does not accumulate pricing data.",
    ));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    Ok,
    Warning,
    Error,
    Pass,
    Warn,
    Fail,
}
impl DoctorStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorRowDto {
    pub status: DoctorStatus,
    pub label: String,
    pub message: String,
    pub suggestion: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorSectionDto {
    pub title: String,
    pub rows: Vec<DoctorRowDto>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCommandDto {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub cwd: String,
    pub session_id: String,
    pub permission_mode: String,
    pub sections: Vec<DoctorSectionDto>,
}
pub fn format_doctor_command(dto: DoctorCommandDto) -> String {
    let mut out = format!("Doctor diagnostics (local-only)\n  Rebon version: {}\n  provider: {}\n  model: {}\n  cwd: {}\n  session id: {}\n  permission mode: {}\n", dto.version, dto.provider, dto.model, dto.cwd, dto.session_id, dto.permission_mode);
    for section in dto.sections {
        out.push('\n');
        out.push_str(&section.title);
        out.push('\n');
        if section.rows.is_empty() {
            out.push_str("  ok: none — no findings\n");
        }
        for row in section.rows {
            out.push_str(&format!(
                "  {}: {} — {}\n",
                row.status.label(),
                row.label,
                row.message.replace('\n', " ")
            ));
            if let Some(suggestion) = row.suggestion {
                out.push_str(&format!("    Fix: {suggestion}\n"));
            }
        }
    }
    out.trim_end().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFileDto {
    pub path: String,
    pub tokens: String,
}
pub fn format_memory_command(files: Vec<MemoryFileDto>) -> String {
    if files.is_empty() {
        return "No loaded memory or instruction files were discovered for this session.".into();
    }
    let mut out = format!("Loaded memory/instruction files ({}):\n", files.len());
    for file in files {
        out.push_str(&format!(
            "- {} (exists, ~{} tokens)\n",
            file.path, file.tokens
        ));
    }
    out.trim_end().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolDto {
    pub name: String,
    pub tokens: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCommandDto {
    pub loader: String,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub client: String,
    pub tools: Vec<McpToolDto>,
}
pub fn format_mcp_command(dto: McpCommandDto) -> String {
    let mut out = format!("MCP status\nloader: {}\n", dto.loader);
    if !dto.warnings.is_empty() {
        out.push_str(&format!("warnings: {}\n", dto.warnings.len()));
        for warning in dto.warnings {
            out.push_str(&format!("  - {warning}\n"));
        }
    }
    if let Some(error) = dto.error {
        out.push_str(&format!("error: {error}\n"));
    }
    out.push_str(&format!(
        "client: {}\nloaded MCP tools: {}\n",
        dto.client,
        dto.tools.len()
    ));
    if dto.tools.is_empty() {
        out.push_str("  none");
    } else {
        for tool in dto.tools {
            out.push_str(&format!("  - {} (~{} tokens)\n", tool.name, tool.tokens));
        }
    }
    out.trim_end().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookMatcherDto {
    pub field: String,
    pub values: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookConfigDto {
    pub kind: String,
    pub display: String,
    pub matcher: String,
    pub source: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEventDto {
    pub name: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub matcher: Option<HookMatcherDto>,
    pub configs: Vec<HookConfigDto>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HooksCommandDto {
    pub selected_event: Option<String>,
    pub events: Vec<HookEventDto>,
    pub warnings: Vec<String>,
}
pub fn format_hooks_command(dto: HooksCommandDto) -> String {
    let known = dto
        .events
        .iter()
        .map(|e| e.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = if let Some(selected) = dto.selected_event {
        let Some(event) = dto
            .events
            .iter()
            .find(|event| event.name.eq_ignore_ascii_case(&selected))
        else {
            return format!("Unknown hook event: {selected}\nKnown hook events: {known}\nRun `/hooks` for the full read-only summary.");
        };
        let mut out = format!("Hooks: {}\n", event.name);
        if let (Some(summary), Some(description)) = (&event.summary, &event.description) {
            out.push_str(&format!("summary: {summary}\ndescription: {description}\n"));
        } else {
            out.push_str("metadata: unavailable\n");
        }
        match &event.matcher {
            Some(m) => {
                out.push_str(&format!("matcher metadata: yes\nfield: {}\n", m.field));
                if m.values.is_empty() {
                    out.push_str("known values: none\n");
                } else {
                    out.push_str(&format!("known values: {}\n", m.values.join(", ")));
                }
            }
            None => out.push_str("matcher metadata: no\n"),
        }
        if event.configs.is_empty() {
            out.push_str("configured: no\n");
        } else {
            out.push_str(&format!("configured: yes ({})\n", event.configs.len()));
            for config in &event.configs {
                out.push_str(&format!(
                    "  - [{}] {} (matcher: {}; source: {})\n",
                    config.kind, config.display, config.matcher, config.source
                ));
            }
        }
        out
    } else {
        let mut out = format!(
            "Hooks\nRead-only hook event summary.\nKnown hook events: {}\n",
            dto.events.len()
        );
        for event in &dto.events {
            out.push_str(&format!("- {}", event.name));
            match &event.summary {
                Some(summary) => out.push_str(&format!(
                    " — matcher metadata: {}; {summary}",
                    if event.matcher.is_some() { "yes" } else { "no" }
                )),
                None => out.push_str(" — matcher metadata: no; metadata unavailable"),
            }
            if event.configs.is_empty() {
                out.push_str("; not configured");
            } else {
                let summaries = event
                    .configs
                    .iter()
                    .map(|config| format!("[{}] {}", config.kind, config.display))
                    .collect::<Vec<_>>()
                    .join(" | ");
                out.push_str(&format!(
                    "; configured ({}): {summaries}",
                    event.configs.len()
                ));
            }
            out.push('\n');
        }
        out
    };
    if !dto.warnings.is_empty() {
        out.push_str("settings warnings:\n");
        for warning in dto.warnings {
            out.push_str(&format!("  - {warning}\n"));
        }
    }
    out.trim_end().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCommandDto {
    pub provider: String,
    pub model: String,
    pub cwd: String,
    pub ui_mode: String,
    pub vim_mode: String,
    pub mcp_client: String,
    pub active_agents: usize,
    pub active_tasks: usize,
    pub session_title: Option<String>,
    pub ultraplan_phase: Option<String>,
    /// The update section, or `None` when the `updater` plugin is off — a
    /// front end with no update feature prints no update heading.
    pub update_status: Option<String>,
}
pub fn format_status_command(dto: StatusCommandDto) -> String {
    let mut lines = vec![
        "Session status".into(),
        format!("provider: {}", dto.provider),
        format!("model: {}", dto.model),
        format!("cwd: {}", dto.cwd),
        format!("ui mode: {}", dto.ui_mode),
        format!("vim mode: {}", dto.vim_mode),
        format!("MCP client: {}", dto.mcp_client),
        format!("active agents: {}", dto.active_agents),
        format!("active background tasks: {}", dto.active_tasks),
    ];
    if let Some(title) = dto.session_title.filter(|s| !s.is_empty()) {
        lines.push(format!("session title: {title}"));
    }
    if let Some(phase) = dto.ultraplan_phase {
        lines.push(format!("ultraplan: {phase}"));
    }
    if let Some(update_status) = dto.update_status {
        lines.push(String::new());
        lines.push("Update status".into());
        lines.extend(update_status.lines().map(str::to_string));
    }
    lines.join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextProviderUsageDto {
    pub input: String,
    pub output: String,
    pub current_output: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPromptSourcesDto {
    pub system_prompt: String,
    pub system_prompt_captured: bool,
    pub builtin_tools: String,
    pub deferred_tools: Option<String>,
    pub mcp_tools: String,
    pub skills: String,
    pub memory_files: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessagesDto {
    pub total: usize,
    pub scanned: usize,
    pub skipped: usize,
    pub user: u32,
    pub assistant: u32,
    pub system: u32,
    pub attachments: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlocksDto {
    pub text: u32,
    pub tool_use: u32,
    pub tool_result: u32,
    pub thinking: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextTokenBreakdownDto {
    pub user_text: Option<String>,
    pub assistant_text: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_results: Option<String>,
    pub thinking: Option<String>,
    pub total: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextTopToolDto {
    pub name: String,
    pub count: u32,
    pub total: String,
    pub call: String,
    pub result: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCategoryDto {
    pub name: String,
    pub tokens: String,
    pub percent: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextNamedTokensDto {
    pub name: String,
    pub tokens: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSkillGroupDto {
    pub display_name: String,
    pub items: Vec<ContextNamedTokensDto>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMemoryFileDto {
    pub path: String,
    pub tokens: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextProviderTotalsDto {
    pub input: String,
    pub output: String,
    pub cache: Option<(String, String)>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPruneStatsDto {
    pub cleared: u64,
    pub duplicates: u64,
    pub truncated: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextDetailsDto {
    pub model: String,
    pub provider: String,
    pub grid: String,
    pub percent: u8,
    pub tokens: String,
    pub window: String,
    pub usage_source: String,
    pub provider_usage: Option<ContextProviderUsageDto>,
    pub warning: bool,
    pub prompt_sources: ContextPromptSourcesDto,
    pub messages: ContextMessagesDto,
    pub blocks: ContextBlocksDto,
    pub token_breakdown: Option<ContextTokenBreakdownDto>,
    pub top_tools: Vec<ContextTopToolDto>,
    pub categories: Vec<ContextCategoryDto>,
    pub builtin_loaded: Vec<ContextNamedTokensDto>,
    pub builtin_available: Vec<String>,
    pub mcp_loaded: Vec<ContextNamedTokensDto>,
    pub mcp_available: Vec<String>,
    pub skill_groups: Vec<ContextSkillGroupDto>,
    pub memory_files: Vec<ContextMemoryFileDto>,
    pub auto_compact: bool,
    pub compact_threshold: String,
    pub provider_totals: Option<ContextProviderTotalsDto>,
    pub prune_level: String,
    pub prune_stats: Option<ContextPruneStatsDto>,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUnavailableDto {
    pub session_kind: String,
    pub live_messages: usize,
    pub loaded_records: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCommandDto {
    Detailed(ContextDetailsDto),
    Unavailable(ContextUnavailableDto),
}

pub fn format_context_command(dto: ContextCommandDto) -> String {
    match dto {
        ContextCommandDto::Detailed(dto) => format_context_details(dto),
        ContextCommandDto::Unavailable(dto) => format!(
            "Context Usage — {}\nMessages: {} live prompt blocks; {} loaded transcript records\nToken usage: {}",
            dto.session_kind, dto.live_messages, dto.loaded_records, dto.reason,
        ),
    }
}

fn format_context_details(dto: ContextDetailsDto) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!(
        "Context Usage — {} ({})\n{}  {}%\n{} / {} tokens ({})\n",
        dto.model, dto.provider, dto.grid, dto.percent, dto.tokens, dto.window, dto.usage_source
    ));
    if let Some(usage) = dto.provider_usage {
        out.push_str(&format!(
            "Provider usage — last turn: {} input / {} output, current output: {}\n",
            usage.input, usage.output, usage.current_output
        ));
    }
    if dto.warning {
        out.push_str("⚠ Near context window limit!\n");
    }
    let sources = dto.prompt_sources;
    out.push_str("\nPrompt Sources (estimated)\n");
    out.push_str(&format!("  system prompt:   ~{}", sources.system_prompt));
    if !sources.system_prompt_captured {
        out.push_str(" (not captured)");
    }
    out.push('\n');
    out.push_str(&format!("  built-in tools:  ~{}\n", sources.builtin_tools));
    if let Some(tokens) = sources.deferred_tools {
        out.push_str(&format!("  deferred tools:  ~{tokens}\n"));
    }
    out.push_str(&format!(
        "  MCP tools:       ~{}\n  skills:          ~{}\n  memory files:    ~{}\n",
        sources.mcp_tools, sources.skills, sources.memory_files
    ));
    let messages = dto.messages;
    if messages.skipped > 0 {
        out.push_str(&format!(
            "\nMessages ({}, scanned latest {}; skipped {})\n",
            messages.total, messages.scanned, messages.skipped
        ));
    } else {
        out.push_str(&format!("\nMessages ({})\n", messages.total));
    }
    out.push_str(&format!(
        "  user: {}  assistant: {}",
        messages.user, messages.assistant
    ));
    if messages.system > 0 {
        out.push_str(&format!("  system: {}", messages.system));
    }
    if messages.attachments > 0 {
        out.push_str(&format!("  attachment: {}", messages.attachments));
    }
    out.push('\n');
    out.push_str(&format!(
        "\nContent Blocks\n  text: {}  tool_use: {}  tool_result: {}",
        dto.blocks.text, dto.blocks.tool_use, dto.blocks.tool_result
    ));
    if dto.blocks.thinking > 0 {
        out.push_str(&format!("  thinking: {}", dto.blocks.thinking));
    }
    out.push('\n');
    if let Some(tokens) = dto.token_breakdown {
        out.push_str("\nToken Breakdown (estimated)\n");
        if let Some(value) = tokens.user_text {
            out.push_str(&format!("  user text:       ~{value}\n"));
        }
        if let Some(value) = tokens.assistant_text {
            out.push_str(&format!("  assistant text:  ~{value}\n"));
        }
        if let Some(value) = tokens.tool_calls {
            out.push_str(&format!("  tool calls:      ~{value}\n"));
        }
        if let Some(value) = tokens.tool_results {
            out.push_str(&format!("  tool results:    ~{value}\n"));
        }
        if let Some(value) = tokens.thinking {
            out.push_str(&format!("  thinking:        ~{value}\n"));
        }
        out.push_str(&format!("  total (msgs):    ~{}\n", tokens.total));
    }
    if !dto.top_tools.is_empty() {
        out.push_str("\nTop Tools (by tokens)\n");
        for tool in dto.top_tools {
            out.push_str(&format!(
                "  {}: {}x  ~{} (call: ~{}, result: ~{})\n",
                tool.name, tool.count, tool.total, tool.call, tool.result
            ));
        }
    }
    if !dto.categories.is_empty() {
        out.push_str("\nContext Categories\n");
        for category in dto.categories {
            out.push_str(&format!(
                "  {}: ~{} ({}%)\n",
                category.name, category.tokens, category.percent
            ));
        }
    }
    render_named_tokens(
        &mut out,
        "Built-in Tools",
        dto.builtin_loaded,
        dto.builtin_available,
    );
    render_named_tokens(&mut out, "MCP Tools", dto.mcp_loaded, dto.mcp_available);
    if !dto.skill_groups.is_empty() {
        out.push_str("\nSkills\n");
        for group in dto.skill_groups {
            out.push_str(&format!("  [{}]\n", group.display_name));
            for item in group.items {
                out.push_str(&format!("    {}: ~{}\n", item.name, item.tokens));
            }
        }
    }
    if !dto.memory_files.is_empty() {
        out.push_str("\nMemory Files\n");
        for file in dto.memory_files {
            out.push_str(&format!("  {}: ~{}\n", file.path, file.tokens));
        }
    }
    out.push_str(&format!(
        "\nAuto-compact: {} (threshold: {})\n",
        if dto.auto_compact {
            "triggered"
        } else {
            "standby"
        },
        dto.compact_threshold
    ));
    if let Some(totals) = dto.provider_totals {
        out.push_str(&format!(
            "Provider totals: {} input / {} output",
            totals.input, totals.output
        ));
        if let Some((read, write)) = totals.cache {
            out.push_str(&format!(" (cache read/hit: {read}, write/miss: {write})"));
        }
        out.push('\n');
    }
    out.push_str(&format!("Prune level: {}\n", dto.prune_level));
    if let Some(stats) = dto.prune_stats {
        out.push_str(&format!(
            "Prune stats: {} tool results cleared, {} duplicates removed, {} truncated\n",
            stats.cleared, stats.duplicates, stats.truncated
        ));
    }
    if !dto.suggestions.is_empty() {
        out.push_str("\nSuggestions\n");
        for suggestion in dto.suggestions {
            out.push_str(&format!("  {suggestion}\n"));
        }
    }
    truncate_context_output(out)
}

fn render_named_tokens(
    out: &mut String,
    title: &str,
    loaded: Vec<ContextNamedTokensDto>,
    available: Vec<String>,
) {
    if loaded.is_empty() && available.is_empty() {
        return;
    }
    out.push_str(&format!("\n{title}\n"));
    for item in loaded {
        out.push_str(&format!("  {}: ~{}\n", item.name, item.tokens));
    }
    if !available.is_empty() {
        out.push_str(&format!("  (deferred: {})\n", available.join(", ")));
    }
}

fn truncate_context_output(mut out: String) -> String {
    const LIMIT: usize = 16_000;
    if out.len() <= LIMIT {
        return out;
    }
    let mut boundary = LIMIT;
    while boundary > 0 && !out.is_char_boundary(boundary) {
        boundary -= 1;
    }
    out.truncate(boundary);
    out.push_str("\n\n[context output truncated]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_memory_reports_no_files() {
        assert_eq!(
            format_memory_command(vec![]),
            "No loaded memory or instruction files were discovered for this session."
        );
    }

    #[test]
    fn failed_mcp_loader_preserves_error_and_empty_tools() {
        assert_eq!(
            format_mcp_command(McpCommandDto {
                loader: "failed".into(),
                warnings: vec![],
                error: Some("connection refused".into()),
                client: "disconnected".into(),
                tools: vec![],
            }),
            "MCP status\nloader: failed\nerror: connection refused\nclient: disconnected\nloaded MCP tools: 0\n  none"
        );
    }

    #[test]
    fn unknown_hook_event_preserves_error_and_known_events() {
        assert_eq!(
            format_hooks_command(HooksCommandDto {
                selected_event: Some("unknown".into()),
                events: vec![HookEventDto {
                    name: "PreToolUse".into(),
                    summary: None,
                    description: None,
                    matcher: None,
                    configs: vec![],
                }],
                warnings: vec![],
            }),
            "Unknown hook event: unknown\nKnown hook events: PreToolUse\nRun `/hooks` for the full read-only summary."
        );
    }

    #[test]
    fn hook_selection_is_case_insensitive_with_empty_metadata() {
        assert_eq!(
            format_hooks_command(HooksCommandDto {
                selected_event: Some("pretooluse".into()),
                events: vec![HookEventDto {
                    name: "PreToolUse".into(),
                    summary: None,
                    description: None,
                    matcher: Some(HookMatcherDto {
                        field: "tool_name".into(),
                        values: vec![],
                    }),
                    configs: vec![],
                }],
                warnings: vec![],
            }),
            "Hooks: PreToolUse\nmetadata: unavailable\nmatcher metadata: yes\nfield: tool_name\nknown values: none\nconfigured: no"
        );
    }

    #[test]
    fn empty_hook_catalog_preserves_summary() {
        assert_eq!(
            format_hooks_command(HooksCommandDto {
                selected_event: None,
                events: vec![],
                warnings: vec![],
            }),
            "Hooks\nRead-only hook event summary.\nKnown hook events: 0"
        );
    }

    #[test]
    fn empty_doctor_section_preserves_no_findings() {
        assert_eq!(
            format_doctor_command(DoctorCommandDto {
                version: "1".into(),
                provider: "p".into(),
                model: "m".into(),
                cwd: "/x".into(),
                session_id: "s".into(),
                permission_mode: "default".into(),
                sections: vec![DoctorSectionDto {
                    title: "Configuration".into(),
                    rows: vec![],
                }],
            }),
            "Doctor diagnostics (local-only)\n  Rebon version: 1\n  provider: p\n  model: m\n  cwd: /x\n  session id: s\n  permission mode: default\n\nConfiguration\n  ok: none — no findings"
        );
    }

    #[test]
    fn unavailable_context_preserves_unknown_usage_reason() {
        assert_eq!(
            format_context_command(ContextCommandDto::Unavailable(ContextUnavailableDto {
                session_kind: "ACP session".into(),
                live_messages: 0,
                loaded_records: 0,
                reason: "unavailable from backend".into(),
            })),
            "Context Usage — ACP session\nMessages: 0 live prompt blocks; 0 loaded transcript records\nToken usage: unavailable from backend"
        );
    }

    #[test]
    fn context_truncation_keeps_exact_limit_and_utf8_boundary() {
        let exact_limit = "x".repeat(16_000);
        assert_eq!(truncate_context_output(exact_limit.clone()), exact_limit);
        let prefix = "x".repeat(15_999);
        assert_eq!(
            truncate_context_output(format!("{prefix}界tail")),
            format!("{prefix}\n\n[context output truncated]\n")
        );
        assert_eq!(truncate_context_output(String::new()), "");
    }

    #[test]
    fn cost_golden() {
        assert_eq!(format_cost_command(CostCommandDto { duration: "1m 2s".into(), context_tokens: "12.3k".into(), context_window: "200.0k".into(), context_source: "provider".into(), streaming_tokens: "7".into(), total_input: "10".into(), total_output: "20".into(), total_cache_read: "3".into(), total_cache_write: "4".into(), last_input: "5".into(), last_output: "6".into(), usd_summary: Some("USD estimate\n  total: $0.123456".into()) }), "Cost estimate (local)\n  session duration: 1m 2s\n  context usage: 12.3k / 200.0k tokens (provider)\n  current output tokens: 7\n\nProvider usage\n  total input: 10\n  total output: 20\n  total cache read/hit: 3\n  total cache write/miss: 4\n  last turn input: 5\n  last turn output: 6\n\nUSD estimate\n  total: $0.123456");
    }

    #[test]
    fn doctor_golden() {
        assert_eq!(format_doctor_command(DoctorCommandDto { version: "1.2.3".into(), provider: "p".into(), model: "m".into(), cwd: "/x".into(), session_id: "s".into(), permission_mode: "default".into(), sections: vec![DoctorSectionDto { title: "Configuration".into(), rows: vec![DoctorRowDto { status: DoctorStatus::Warn, label: "config".into(), message: "line one\nline two".into(), suggestion: Some("Fix it.".into()) }] }] }), "Doctor diagnostics (local-only)\n  Rebon version: 1.2.3\n  provider: p\n  model: m\n  cwd: /x\n  session id: s\n  permission mode: default\n\nConfiguration\n  warn: config — line one line two\n    Fix: Fix it.");
    }

    #[test]
    fn memory_golden() {
        assert_eq!(
            format_memory_command(vec![MemoryFileDto {
                path: "REBON.md".into(),
                tokens: "1.2k".into()
            }]),
            "Loaded memory/instruction files (1):\n- REBON.md (exists, ~1.2k tokens)"
        );
    }

    #[test]
    fn mcp_golden() {
        assert_eq!(format_mcp_command(McpCommandDto { loader: "ready".into(), warnings: vec!["slow".into()], error: None, client: "connected".into(), tools: vec![McpToolDto { name: "search".into(), tokens: "42".into() }] }), "MCP status\nloader: ready\nwarnings: 1\n  - slow\nclient: connected\nloaded MCP tools: 1\n  - search (~42 tokens)");
    }

    #[test]
    fn hooks_golden() {
        assert_eq!(format_hooks_command(HooksCommandDto { selected_event: Some("PreToolUse".into()), events: vec![HookEventDto { name: "PreToolUse".into(), summary: Some("Before tools".into()), description: Some("Runs before a tool.".into()), matcher: Some(HookMatcherDto { field: "tool_name".into(), values: vec!["Read".into(), "Write".into()] }), configs: vec![HookConfigDto { kind: "command".into(), display: "check".into(), matcher: "Read".into(), source: "project".into() }] }], warnings: vec!["legacy setting".into()] }), "Hooks: PreToolUse\nsummary: Before tools\ndescription: Runs before a tool.\nmatcher metadata: yes\nfield: tool_name\nknown values: Read, Write\nconfigured: yes (1)\n  - [command] check (matcher: Read; source: project)\nsettings warnings:\n  - legacy setting");
    }

    #[test]
    fn context_golden() {
        assert_eq!(format_context_command(ContextCommandDto::Detailed(ContextDetailsDto { model: "m".into(), provider: "p".into(), grid: "⬛⬜".into(), percent: 50, tokens: "50".into(), window: "100".into(), usage_source: "provider".into(), provider_usage: Some(ContextProviderUsageDto { input: "10".into(), output: "5".into(), current_output: "1".into() }), warning: false, prompt_sources: ContextPromptSourcesDto { system_prompt: "2".into(), system_prompt_captured: true, builtin_tools: "3".into(), deferred_tools: None, mcp_tools: "0".into(), skills: "0".into(), memory_files: "1".into() }, messages: ContextMessagesDto { total: 2, scanned: 2, skipped: 0, user: 1, assistant: 1, system: 0, attachments: 0 }, blocks: ContextBlocksDto { text: 2, tool_use: 0, tool_result: 0, thinking: 0 }, token_breakdown: Some(ContextTokenBreakdownDto { user_text: Some("1".into()), assistant_text: Some("1".into()), tool_calls: None, tool_results: None, thinking: None, total: "2".into() }), top_tools: vec![], categories: vec![ContextCategoryDto { name: "Messages".into(), tokens: "2".into(), percent: "2.0".into() }], builtin_loaded: vec![], builtin_available: vec![], mcp_loaded: vec![], mcp_available: vec![], skill_groups: vec![], memory_files: vec![ContextMemoryFileDto { path: "REBON.md".into(), tokens: "1".into() }], auto_compact: false, compact_threshold: "80".into(), provider_totals: None, prune_level: "medium".into(), prune_stats: None, suggestions: vec![] })), "Context Usage — m (p)\n⬛⬜  50%\n50 / 100 tokens (provider)\nProvider usage — last turn: 10 input / 5 output, current output: 1\n\nPrompt Sources (estimated)\n  system prompt:   ~2\n  built-in tools:  ~3\n  MCP tools:       ~0\n  skills:          ~0\n  memory files:    ~1\n\nMessages (2)\n  user: 1  assistant: 1\n\nContent Blocks\n  text: 2  tool_use: 0  tool_result: 0\n\nToken Breakdown (estimated)\n  user text:       ~1\n  assistant text:  ~1\n  total (msgs):    ~2\n\nContext Categories\n  Messages: ~2 (2.0%)\n\nMemory Files\n  REBON.md: ~1\n\nAuto-compact: standby (threshold: 80)\nPrune level: medium\n");
    }

    /// With the `updater` plugin off there is no update section at all — not
    /// an empty heading, which would read as "checked, nothing to say".
    #[test]
    fn status_golden_without_the_updater_plugin() {
        assert_eq!(format_status_command(StatusCommandDto { provider:"p".into(), model:"m".into(), cwd:"/x".into(), ui_mode:"Inline".into(), vim_mode:"disabled".into(), mcp_client:"not connected".into(), active_agents:0, active_tasks:0, session_title:None, ultraplan_phase:None, update_status:None }), "Session status\nprovider: p\nmodel: m\ncwd: /x\nui mode: Inline\nvim mode: disabled\nMCP client: not connected\nactive agents: 0\nactive background tasks: 0");
    }

    #[test]
    fn status_golden() {
        assert_eq!(format_status_command(StatusCommandDto { provider:"p".into(), model:"m".into(), cwd:"/x".into(), ui_mode:"Inline".into(), vim_mode:"disabled".into(), mcp_client:"not connected".into(), active_agents:0, active_tasks:0, session_title:None, ultraplan_phase:None, update_status:Some("current".into()) }), "Session status\nprovider: p\nmodel: m\ncwd: /x\nui mode: Inline\nvim mode: disabled\nMCP client: not connected\nactive agents: 0\nactive background tasks: 0\n\nUpdate status\ncurrent");
    }
}
