//! 系统文本行的纯投影：回合耗时、记忆保存、离开摘要、agent 终止、思考、
//! bridge 状态、定时任务、provider 切换、权限重试、API 错误、stop hook 摘要和普通信息。

use crate::mailbox::team_mem_saved_part;
use crate::system_api_error::{
    project_system_api_error, SystemApiErrorInput, SystemApiErrorProjection,
};

/// Marker glyph kind used by system rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemVisualMarker {
    /// Filled-circle glyph, drawn on summary and non-info rows.
    BlackCircle,
    /// Reference-mark glyph, drawn on away-summary rows.
    ReferenceMark,
    /// Teardrop-asterisk glyph, drawn on thinking, scheduled-task,
    /// permission-retry and turn-duration rows.
    TeardropAsterisk,
}

/// Input for projecting one system-text row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTextMessageInput {
    /// Whether the standard top margin is added above the row.
    pub add_margin: bool,
    /// `verbose`.
    pub verbose: bool,
    /// Whether the transcript view, rather than the compact view, is active.
    pub is_transcript_mode: bool,
    /// Selected-message background.
    pub background: Option<String>,
    /// Optional terminal columns for inner width calculations.
    pub terminal_columns: Option<u16>,
    /// Message payload.
    pub message: SystemTextProjectionInput,
}

/// The system-message payloads this projection understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemTextProjectionInput {
    /// `turn_duration`.
    TurnDuration(SystemTurnDurationInput),
    /// `memory_saved`.
    MemorySaved(SystemMemorySavedInput),
    /// `away_summary`.
    AwaySummary {
        /// Content.
        content: String,
    },
    /// `agents_killed`.
    AgentsKilled,
    /// `thinking`.
    Thinking {
        /// Content text.
        content: String,
        /// Whether internal-only build logic is enabled.
        internal_build: bool,
    },
    /// `bridge_status`.
    BridgeStatus {
        /// URL.
        url: String,
        /// Optional upgrade nudge.
        upgrade_nudge: Option<String>,
    },
    /// `scheduled_task_fire`.
    ScheduledTaskFire {
        /// Content.
        content: String,
    },
    /// `permission_retry`.
    PermissionRetry {
        /// Allowed commands.
        commands: Vec<String>,
    },
    /// `api_error`.
    ApiError(SystemApiErrorInput),
    /// `stop_hook_summary`.
    StopHookSummary(SystemStopHookSummaryInput),
    /// Generic informational/warning/error system text.
    Generic {
        /// Subtype string.
        subtype: String,
        /// Level string (`info` / `warning` / other).
        level: String,
        /// Optional string content.
        content: Option<String>,
    },
}

/// Turn-duration input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTurnDurationInput {
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Optional budget tokens.
    pub budget_tokens: Option<u64>,
    /// Optional budget limit.
    pub budget_limit: Option<u64>,
    /// Optional budget nudges.
    pub budget_nudges: u64,
    /// Whether global config enables turn-duration display.
    pub show_turn_duration: bool,
    /// Past-tense verb for the duration line, picked by the caller.
    pub completion_verb: String,
    /// Optional background task summary label.
    pub background_task_summary: Option<String>,
    /// Tick glyph string used in the over-budget suffix.
    pub tick_glyph: String,
}

/// Turn-duration display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTurnDurationDisplay {
    /// Margin top.
    pub margin_top: u8,
    /// Background.
    pub background: Option<String>,
    /// Leading marker.
    pub marker: SystemVisualMarker,
    /// Optional main verb/duration string.
    pub duration_text: Option<String>,
    /// Optional budget suffix.
    pub budget: Option<SystemTurnBudgetDisplay>,
    /// Optional background-task suffix.
    pub background_task_summary: Option<String>,
}

/// Budget display for turn duration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTurnBudgetDisplay {
    /// Full usage text.
    pub usage_text: String,
    /// Whether a leading bullet should precede it.
    pub prefixed_with_separator: bool,
    /// Optional nudges suffix, without the bullet before usage.
    pub nudges_text: Option<String>,
}

/// Memory-saved input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemMemorySavedInput {
    /// Written paths.
    pub written_paths: Vec<String>,
    /// Optional custom verb.
    pub verb: Option<String>,
    /// Team memory count.
    pub team_count: usize,
}

/// Memory-saved display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemMemorySavedDisplay {
    /// Margin top.
    pub margin_top: u8,
    /// Background.
    pub background: Option<String>,
    /// Leading marker.
    pub marker: SystemVisualMarker,
    /// Verb (`Saved`, `Improved`, etc.).
    pub verb: String,
    /// Summary parts joined by bullets in the UI.
    pub parts: Vec<String>,
    /// Per-path rows.
    pub entries: Vec<SystemMemorySavedEntry>,
}

/// One memory path row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemMemorySavedEntry {
    /// Full path.
    pub path: String,
    /// Basename display.
    pub basename: String,
    /// Click target for the row.
    pub open_path: String,
    /// Style hint for the idle row: dim, no underline.
    pub idle_style: MemoryFileRowStyleHint,
    /// Style hint for the hovered row: underline, not dim.
    pub hover_style: MemoryFileRowStyleHint,
}

/// Dim and underline flags for one state of a memory path row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryFileRowStyleHint {
    /// Whether the row is drawn dim.
    pub dim: bool,
    /// Whether the row is drawn underlined.
    pub underline: bool,
}

/// Stop-hook summary input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemStopHookSummaryInput {
    /// Hook count.
    pub hook_count: usize,
    /// Hook info rows.
    pub hook_infos: Vec<StopHookInfoDisplay>,
    /// Hook errors.
    pub hook_errors: Vec<String>,
    /// Prevented continuation.
    pub prevented_continuation: bool,
    /// Optional stop reason.
    pub stop_reason: Option<String>,
    /// Optional hook label.
    pub hook_label: Option<String>,
    /// Optional total duration ms.
    pub total_duration_ms: Option<u64>,
    /// Whether internal-only timing strings are enabled.
    pub internal_build: bool,
    /// Timing threshold gate from tool execution.
    pub hook_timing_display_threshold_ms: u64,
    /// Terminal columns.
    pub terminal_columns: u16,
}

/// Hook info row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopHookInfoDisplay {
    /// Command string.
    pub command: String,
    /// Optional prompt text, present only when the command is `prompt`.
    pub prompt_text: Option<String>,
    /// Optional duration.
    pub duration_ms: Option<u64>,
}

/// Stop-hook summary display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopHookSummaryDisplay {
    /// Summary built from an explicit hook label.
    Labeled {
        /// Summary line.
        summary: String,
        /// Transcript-only detail lines.
        transcript_lines: Vec<String>,
    },
    /// Default stop-hook summary branch.
    Default {
        /// Margin top.
        margin_top: u8,
        /// Background.
        background: Option<String>,
        /// Leading marker.
        marker: SystemVisualMarker,
        /// Main summary line.
        summary: String,
        /// Verbose detail lines.
        detail_lines: Vec<String>,
        /// Optional prevented-continuation line.
        prevented_line: Option<String>,
        /// Error lines.
        error_lines: Vec<String>,
        /// Show expand hint in compact mode.
        show_expand_hint: bool,
        /// Width (`columns - 10`).
        width: u16,
    },
}

/// Bridge-status display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemBridgeStatusDisplay {
    /// Margin top.
    pub margin_top: u8,
    /// Background.
    pub background: Option<String>,
    /// Intro line.
    pub intro: &'static str,
    /// URL line.
    pub url: String,
    /// Optional upgrade nudge.
    pub upgrade_nudge: Option<String>,
    /// Fixed width for the status line: 999, so it is never truncated.
    pub width: u16,
}

/// Generic system-text inner display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemGenericTextDisplay {
    /// Margin top.
    pub margin_top: u8,
    /// Background.
    pub background: Option<String>,
    /// Optional leading marker.
    pub marker: Option<SystemVisualMarker>,
    /// Optional color string.
    pub color: Option<String>,
    /// Whether the text is drawn dim.
    pub dim_color: bool,
    /// Trimmed content.
    pub content: String,
    /// Width (`columns - 10`) when threaded from parent.
    pub width: Option<u16>,
}

/// Main projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemTextProjection {
    /// Nothing renders.
    Hidden,
    /// Turn duration.
    TurnDuration(SystemTurnDurationDisplay),
    /// Memory saved.
    MemorySaved(SystemMemorySavedDisplay),
    /// Away summary.
    AwaySummary(SystemGenericTextDisplay),
    /// Agents killed.
    AgentsKilled(SystemGenericTextDisplay),
    /// Thinking row.
    Thinking(SystemGenericTextDisplay),
    /// Bridge status.
    BridgeStatus(SystemBridgeStatusDisplay),
    /// Scheduled task fire.
    ScheduledTaskFire(SystemGenericTextDisplay),
    /// Permission retry.
    PermissionRetry {
        /// Margin top.
        margin_top: u8,
        /// Background.
        background: Option<String>,
        /// Marker.
        marker: SystemVisualMarker,
        /// Allowed commands joined by comma.
        commands: String,
    },
    /// API error.
    ApiError(SystemApiErrorProjection),
    /// Stop hook summary.
    StopHookSummary(StopHookSummaryDisplay),
    /// 从 `provider_switch` 消息解析的切换确认。
    ProviderSwitch {
        /// 顶部间距。
        margin_top: u8,
        /// 背景色。
        background: Option<String>,
        /// 使用当前 accent 颜色的 provider 名称。
        provider: String,
        /// 第二行的 model 名称。
        model: String,
    },
    /// Generic inner system text.
    Generic(SystemGenericTextDisplay),
}

/// Project one system-text message into the display form a renderer draws.
pub fn project_system_text_message(input: &SystemTextMessageInput) -> SystemTextProjection {
    match &input.message {
        SystemTextProjectionInput::TurnDuration(message) => {
            project_turn_duration(message, input.add_margin, input.background.clone())
        }
        SystemTextProjectionInput::MemorySaved(message) => {
            project_memory_saved(message, input.add_margin, input.background.clone())
        }
        SystemTextProjectionInput::AwaySummary { content } => {
            SystemTextProjection::AwaySummary(SystemGenericTextDisplay {
                margin_top: u8::from(input.add_margin),
                background: input.background.clone(),
                marker: Some(SystemVisualMarker::ReferenceMark),
                color: None,
                dim_color: true,
                content: content.clone(),
                width: None,
            })
        }
        SystemTextProjectionInput::AgentsKilled => {
            SystemTextProjection::AgentsKilled(SystemGenericTextDisplay {
                margin_top: u8::from(input.add_margin),
                background: input.background.clone(),
                marker: Some(SystemVisualMarker::BlackCircle),
                color: Some("error".into()),
                dim_color: true,
                content: "All background agents stopped".into(),
                width: None,
            })
        }
        SystemTextProjectionInput::Thinking {
            content,
            internal_build,
        } => {
            if !internal_build {
                SystemTextProjection::Hidden
            } else {
                SystemTextProjection::Thinking(SystemGenericTextDisplay {
                    margin_top: u8::from(input.add_margin),
                    background: input.background.clone(),
                    marker: Some(SystemVisualMarker::TeardropAsterisk),
                    color: None,
                    dim_color: true,
                    content: content.clone(),
                    width: None,
                })
            }
        }
        SystemTextProjectionInput::BridgeStatus { url, upgrade_nudge } => {
            SystemTextProjection::BridgeStatus(SystemBridgeStatusDisplay {
                margin_top: u8::from(input.add_margin),
                background: input.background.clone(),
                intro: "Remote Control compatibility is active. Code in CLI or at",
                url: url.clone(),
                upgrade_nudge: upgrade_nudge.clone(),
                width: 999,
            })
        }
        SystemTextProjectionInput::ScheduledTaskFire { content } => {
            SystemTextProjection::ScheduledTaskFire(SystemGenericTextDisplay {
                margin_top: u8::from(input.add_margin),
                background: input.background.clone(),
                marker: Some(SystemVisualMarker::TeardropAsterisk),
                color: None,
                dim_color: true,
                content: content.clone(),
                width: None,
            })
        }
        SystemTextProjectionInput::PermissionRetry { commands } => {
            SystemTextProjection::PermissionRetry {
                margin_top: u8::from(input.add_margin),
                background: input.background.clone(),
                marker: SystemVisualMarker::TeardropAsterisk,
                commands: commands.join(", "),
            }
        }
        SystemTextProjectionInput::ApiError(api_error) => {
            SystemTextProjection::ApiError(project_system_api_error(api_error))
        }
        SystemTextProjectionInput::StopHookSummary(message) => project_stop_hook_summary(
            message,
            input.add_margin,
            input.verbose,
            input.is_transcript_mode,
            input.background.clone(),
        ),
        SystemTextProjectionInput::Generic {
            subtype,
            level,
            content,
        } => {
            if subtype == "api_error" {
                let Some(content) = content else {
                    return SystemTextProjection::Hidden;
                };
                return SystemTextProjection::ApiError(project_system_api_error(
                    &SystemApiErrorInput {
                        retry_attempt: 0,
                        formatted_error: content.clone(),
                        retry_in_ms: 0,
                        max_retries: 0,
                        verbose: input.verbose,
                        countdown_ms: 0,
                        api_timeout_ms: None,
                    },
                ));
            }
            if subtype == "provider_switch" {
                if let Some((provider, model)) = content.as_deref().and_then(parse_provider_switch)
                {
                    return SystemTextProjection::ProviderSwitch {
                        margin_top: u8::from(input.add_margin),
                        background: input.background.clone(),
                        provider: provider.to_owned(),
                        model: model.to_owned(),
                    };
                }
            }
            if !matches!(
                subtype.as_str(),
                "stop_hook_summary" | "turn_duration" | "provider_switch"
            ) && !input.verbose
                && level == "info"
            {
                return SystemTextProjection::Hidden;
            }
            let Some(content) = content else {
                return SystemTextProjection::Hidden;
            };
            SystemTextProjection::Generic(SystemGenericTextDisplay {
                margin_top: u8::from(input.add_margin),
                background: input.background.clone(),
                marker: (level != "info").then_some(SystemVisualMarker::BlackCircle),
                color: (level == "warning").then(|| "warning".into()),
                dim_color: level == "info",
                content: if subtype == "turn_duration" {
                    format!("● {}", content.trim())
                } else {
                    content.trim().to_string()
                },
                width: input
                    .terminal_columns
                    .map(|columns| columns.saturating_sub(10)),
            })
        }
    }
}

// 历史记录中的异常内容仍走可见的普通文本路径，避免丢字或让各渲染器重复解析。
fn parse_provider_switch(content: &str) -> Option<(&str, &str)> {
    let mut lines = content.lines();
    let provider = lines.next()?.strip_prefix("Switched to provider ")?;
    let model = lines.next()?.strip_prefix("Using model ")?;
    if lines.next().is_some() || provider.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    Some((provider, model))
}

fn project_turn_duration(
    message: &SystemTurnDurationInput,
    add_margin: bool,
    background: Option<String>,
) -> SystemTextProjection {
    let has_budget = message.budget_limit.is_some();
    if !message.show_turn_duration && !has_budget {
        return SystemTextProjection::Hidden;
    }
    let duration_text = message.show_turn_duration.then(|| {
        format!(
            "{} for {}",
            message.completion_verb,
            crate::collapsed_read_search::format_duration_compact(message.duration_ms)
        )
    });

    let budget = if let (Some(tokens), Some(limit)) = (message.budget_tokens, message.budget_limit)
    {
        let usage_text = if tokens >= limit {
            format!(
                "{} used ({} min {})",
                format_compact_number(tokens),
                format_compact_number(limit),
                message.tick_glyph
            )
        } else {
            format!(
                "{} / {} ({}%)",
                format_compact_number(tokens),
                format_compact_number(limit),
                ((tokens as f64 / limit as f64) * 100.0).round() as u64
            )
        };
        Some(SystemTurnBudgetDisplay {
            usage_text,
            prefixed_with_separator: message.show_turn_duration,
            nudges_text: (message.budget_nudges > 0).then(|| {
                format!(
                    "{} {}",
                    message.budget_nudges,
                    if message.budget_nudges == 1 {
                        "nudge"
                    } else {
                        "nudges"
                    }
                )
            }),
        })
    } else {
        None
    };

    SystemTextProjection::TurnDuration(SystemTurnDurationDisplay {
        margin_top: u8::from(add_margin),
        background,
        marker: SystemVisualMarker::TeardropAsterisk,
        duration_text,
        budget,
        background_task_summary: message
            .background_task_summary
            .as_ref()
            .map(|summary| format!("{summary} still running")),
    })
}

fn project_memory_saved(
    message: &SystemMemorySavedInput,
    add_margin: bool,
    background: Option<String>,
) -> SystemTextProjection {
    let team = team_mem_saved_part(message.team_count);
    let private_count = message
        .written_paths
        .len()
        .saturating_sub(team.as_ref().map(|(_, count)| *count).unwrap_or(0));
    let mut parts = Vec::new();
    if private_count > 0 {
        parts.push(format!(
            "{} {}",
            private_count,
            if private_count == 1 {
                "memory"
            } else {
                "memories"
            }
        ));
    }
    if let Some((segment, _)) = team {
        parts.push(segment);
    }
    let entries = message
        .written_paths
        .iter()
        .map(|path| SystemMemorySavedEntry {
            path: path.clone(),
            basename: file_name_from_path(path),
            open_path: path.clone(),
            idle_style: MemoryFileRowStyleHint {
                dim: true,
                underline: false,
            },
            hover_style: MemoryFileRowStyleHint {
                dim: false,
                underline: true,
            },
        })
        .collect();
    SystemTextProjection::MemorySaved(SystemMemorySavedDisplay {
        margin_top: u8::from(add_margin),
        background,
        marker: SystemVisualMarker::BlackCircle,
        verb: message.verb.clone().unwrap_or_else(|| "Saved".into()),
        parts,
        entries,
    })
}

fn project_stop_hook_summary(
    message: &SystemStopHookSummaryInput,
    add_margin: bool,
    verbose: bool,
    is_transcript_mode: bool,
    background: Option<String>,
) -> SystemTextProjection {
    let total_duration_ms = message.total_duration_ms.unwrap_or_else(|| {
        message
            .hook_infos
            .iter()
            .map(|info| info.duration_ms.unwrap_or(0))
            .sum()
    });

    if message.hook_errors.is_empty()
        && !message.prevented_continuation
        && message.hook_label.is_none()
        && (!message.internal_build || total_duration_ms < message.hook_timing_display_threshold_ms)
    {
        return SystemTextProjection::Hidden;
    }

    let total_str = if message.internal_build && total_duration_ms > 0 {
        format!(
            " ({})",
            crate::collapsed_read_search::format_seconds_one_decimal(total_duration_ms)
        )
    } else {
        String::new()
    };

    if let Some(hook_label) = &message.hook_label {
        let transcript_lines = if is_transcript_mode {
            message
                .hook_infos
                .iter()
                .map(|info| format_hook_info_transcript_line(info, message.internal_build))
                .collect()
        } else {
            Vec::new()
        };
        return SystemTextProjection::StopHookSummary(StopHookSummaryDisplay::Labeled {
            summary: format!(
                "Ran {} {} {}{}",
                message.hook_count,
                hook_label,
                if message.hook_count == 1 {
                    "hook"
                } else {
                    "hooks"
                },
                total_str
            ),
            transcript_lines,
        });
    }

    let detail_lines = if verbose {
        message
            .hook_infos
            .iter()
            .map(|info| format_hook_info_verbose_line(info, message.internal_build))
            .collect()
    } else {
        Vec::new()
    };
    // Each error line is the U+23BF glyph, two spaces, then "<label> hook
    // error: <error>", where the label falls back to "Stop". The render layer
    // dims that glyph prefix; the text built here stays as written.
    let error_label = message.hook_label.as_deref().unwrap_or("Stop");
    let error_lines = message
        .hook_errors
        .iter()
        .map(|error| format!("\u{23BF}  {error_label} hook error: {error}"))
        .collect();

    SystemTextProjection::StopHookSummary(StopHookSummaryDisplay::Default {
        margin_top: u8::from(add_margin),
        background,
        marker: SystemVisualMarker::BlackCircle,
        summary: format!(
            "Ran {} {}{}{}",
            message.hook_count,
            message.hook_label.as_deref().unwrap_or("stop"),
            if message.hook_count == 1 {
                " hook"
            } else {
                " hooks"
            },
            total_str
        ),
        detail_lines,
        prevented_line: (message.prevented_continuation)
            .then(|| message.stop_reason.clone())
            .flatten()
            .map(|reason| format!("\u{23BF}  {reason}")),
        error_lines,
        show_expand_hint: !verbose && !message.hook_infos.is_empty(),
        width: message.terminal_columns.saturating_sub(10),
    })
}

fn format_hook_info_transcript_line(info: &StopHookInfoDisplay, internal_build: bool) -> String {
    let duration = if internal_build {
        info.duration_ms
            .map(|ms| {
                format!(
                    " ({})",
                    crate::collapsed_read_search::format_seconds_one_decimal(ms)
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    // Five-space indent before the U+23BF glyph.
    format!("     \u{23BF} {}{}", format_hook_command(info), duration)
}

fn format_hook_info_verbose_line(info: &StopHookInfoDisplay, internal_build: bool) -> String {
    let duration = if internal_build {
        info.duration_ms
            .map(|ms| {
                format!(
                    " ({})",
                    crate::collapsed_read_search::format_seconds_one_decimal(ms)
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!("\u{23BF}  {}{}", format_hook_command(info), duration)
}

fn format_hook_command(info: &StopHookInfoDisplay) -> String {
    if info.command == "prompt" {
        format!("prompt: {}", info.prompt_text.clone().unwrap_or_default())
    } else {
        info.command.clone()
    }
}

fn file_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn format_compact_number(number: u64) -> String {
    if number < 1_000 {
        return number.to_string();
    }
    if number < 1_000_000 {
        return compact_number(number as f64 / 1_000.0, "k");
    }
    if number < 1_000_000_000 {
        return compact_number(number as f64 / 1_000_000.0, "m");
    }
    compact_number(number as f64 / 1_000_000_000.0, "b")
}

fn compact_number(value: f64, suffix: &str) -> String {
    format!("{:.1}", value).to_lowercase() + suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(message: SystemTextProjectionInput) -> SystemTextMessageInput {
        SystemTextMessageInput {
            add_margin: true,
            verbose: false,
            is_transcript_mode: false,
            background: Some("messageActionsBackground".into()),
            terminal_columns: Some(120),
            message,
        }
    }

    #[test]
    fn away_agents_killed_bridge_and_permission_retry_project() {
        let projection =
            project_system_text_message(&input(SystemTextProjectionInput::AwaySummary {
                content: "summary".into(),
            }));
        assert!(matches!(projection, SystemTextProjection::AwaySummary(_)));

        let projection =
            project_system_text_message(&input(SystemTextProjectionInput::AgentsKilled));
        let SystemTextProjection::AgentsKilled(display) = projection else {
            panic!("expected agents killed");
        };
        assert_eq!(display.content, "All background agents stopped");
        assert_eq!(display.color.as_deref(), Some("error"));

        let projection =
            project_system_text_message(&input(SystemTextProjectionInput::BridgeStatus {
                url: "https://x".into(),
                upgrade_nudge: Some("upgrade app".into()),
            }));
        let SystemTextProjection::BridgeStatus(display) = projection else {
            panic!("expected bridge");
        };
        assert_eq!(
            display.intro,
            "Remote Control compatibility is active. Code in CLI or at"
        );
        assert_eq!(display.url, "https://x");
        assert_eq!(display.upgrade_nudge.as_deref(), Some("upgrade app"));
        assert_eq!(display.width, 999);

        let projection =
            project_system_text_message(&input(SystemTextProjectionInput::PermissionRetry {
                commands: vec!["Read".into(), "Bash".into()],
            }));
        let SystemTextProjection::PermissionRetry { commands, .. } = projection else {
            panic!("expected permission retry");
        };
        assert_eq!(commands, "Read, Bash");
    }

    #[test]
    fn provider_switch_projects_raw_payload_in_normal_and_verbose_modes() {
        for verbose in [false, true] {
            for add_margin in [false, true] {
                let projection = project_system_text_message(&SystemTextMessageInput {
                    verbose,
                    add_margin,
                    terminal_columns: Some(8),
                    ..input(SystemTextProjectionInput::Generic {
                        subtype: "provider_switch".into(),
                        level: "info".into(),
                        content: Some(
                            "Switched to provider 本地 gateway\nUsing model org/model-v2".into(),
                        ),
                    })
                });
                assert_eq!(
                    projection,
                    SystemTextProjection::ProviderSwitch {
                        margin_top: u8::from(add_margin),
                        background: Some("messageActionsBackground".into()),
                        provider: "本地 gateway".into(),
                        model: "org/model-v2".into(),
                    }
                );
            }
        }
    }

    #[test]
    fn provider_switch_recognition_requires_its_raw_subtype() {
        let content = "Switched to provider local\nUsing model model-v2";
        for verbose in [false, true] {
            let projection = project_system_text_message(&SystemTextMessageInput {
                verbose,
                ..input(SystemTextProjectionInput::Generic {
                    subtype: "informational".into(),
                    level: "info".into(),
                    content: Some(content.into()),
                })
            });
            if verbose {
                let SystemTextProjection::Generic(display) = projection else {
                    panic!("ordinary info must remain generic");
                };
                assert_eq!(display.content, content);
            } else {
                assert_eq!(projection, SystemTextProjection::Hidden);
            }
        }
    }

    #[test]
    fn provider_switch_malformed_payload_stays_visible_without_losing_text() {
        for content in [
            "Switched to provider local",
            "Switched to provider \nUsing model model-v2",
            "Switched to provider local\nUsing model ",
            "Switched to provider local\nUsing model model-v2\nextra detail",
            "unexpected payload",
        ] {
            for verbose in [false, true] {
                let projection = project_system_text_message(&SystemTextMessageInput {
                    verbose,
                    ..input(SystemTextProjectionInput::Generic {
                        subtype: "provider_switch".into(),
                        level: "info".into(),
                        content: Some(content.into()),
                    })
                });
                let SystemTextProjection::Generic(display) = projection else {
                    panic!("malformed provider switch must remain visible");
                };
                assert_eq!(display.content, content.trim());
            }
        }
    }

    #[test]
    fn generic_info_hidden_but_warning_and_string_content_render() {
        assert_eq!(
            project_system_text_message(&input(SystemTextProjectionInput::Generic {
                subtype: "informational".into(),
                level: "info".into(),
                content: Some("hello".into()),
            })),
            SystemTextProjection::Hidden
        );

        let projection = project_system_text_message(&input(SystemTextProjectionInput::Generic {
            subtype: "informational".into(),
            level: "warning".into(),
            content: Some("  hello  ".into()),
        }));
        let SystemTextProjection::Generic(display) = projection else {
            panic!("expected generic");
        };
        assert_eq!(display.content, "hello");
        assert_eq!(display.color.as_deref(), Some("warning"));
        assert_eq!(display.marker, Some(SystemVisualMarker::BlackCircle));
        assert_eq!(display.width, Some(110));

        assert_eq!(
            project_system_text_message(&input(SystemTextProjectionInput::Thinking {
                content: "pondering".into(),
                internal_build: false,
            })),
            SystemTextProjection::Hidden
        );
        let projection = project_system_text_message(&input(SystemTextProjectionInput::Thinking {
            content: "pondering".into(),
            internal_build: true,
        }));
        let SystemTextProjection::Thinking(display) = projection else {
            panic!("expected thinking");
        };
        assert_eq!(display.content, "pondering");
        assert_eq!(display.marker, Some(SystemVisualMarker::TeardropAsterisk));
    }

    #[test]
    fn api_error_delegates_to_existing_system_api_error_projection() {
        let projection = project_system_text_message(&input(SystemTextProjectionInput::ApiError(
            SystemApiErrorInput {
                retry_attempt: 4,
                formatted_error: "oops".into(),
                retry_in_ms: 5_000,
                max_retries: 8,
                verbose: false,
                countdown_ms: 0,
                api_timeout_ms: None,
            },
        )));
        assert!(matches!(projection, SystemTextProjection::ApiError(_)));

        let projection = project_system_text_message(&input(SystemTextProjectionInput::Generic {
            subtype: "api_error".into(),
            level: "error".into(),
            content: Some(
                "Prompt turn failed: prompt executor failed: model stream error: boom".into(),
            ),
        }));
        let SystemTextProjection::ApiError(display) = projection else {
            panic!("expected generic api_error to use api error projection");
        };
        assert_eq!(display.displayed_error.as_deref(), Some("boom"));
        assert_eq!(display.retry_text, None);
    }

    #[test]
    fn generic_turn_duration_remains_visible_without_verbose_output() {
        let projection = project_system_text_message(&input(SystemTextProjectionInput::Generic {
            subtype: "turn_duration".into(),
            level: "info".into(),
            content: Some("Worked 1m 02s".into()),
        }));
        let SystemTextProjection::Generic(display) = projection else {
            panic!("expected visible turn duration");
        };
        assert_eq!(display.content, "● Worked 1m 02s");
        assert!(display.dim_color);
    }

    #[test]
    fn turn_duration_hides_or_shows_budget_suffix_correctly() {
        assert_eq!(
            project_system_text_message(&input(SystemTextProjectionInput::TurnDuration(
                SystemTurnDurationInput {
                    duration_ms: 5_000,
                    budget_tokens: None,
                    budget_limit: None,
                    budget_nudges: 0,
                    show_turn_duration: false,
                    completion_verb: "Worked".into(),
                    background_task_summary: None,
                    tick_glyph: "✓".into(),
                }
            ))),
            SystemTextProjection::Hidden
        );

        let projection = project_system_text_message(&input(
            SystemTextProjectionInput::TurnDuration(SystemTurnDurationInput {
                duration_ms: 65_000,
                budget_tokens: Some(1_321),
                budget_limit: Some(10_000),
                budget_nudges: 2,
                show_turn_duration: true,
                completion_verb: "Cooked".into(),
                background_task_summary: Some("2 local agents".into()),
                tick_glyph: "✓".into(),
            }),
        ));
        let SystemTextProjection::TurnDuration(display) = projection else {
            panic!("expected turn duration");
        };
        assert_eq!(display.duration_text.as_deref(), Some("Cooked for 1m 5s"));
        let budget = display.budget.unwrap();
        assert_eq!(budget.usage_text, "1.3k / 10.0k (13%)");
        assert!(budget.prefixed_with_separator);
        assert_eq!(budget.nudges_text.as_deref(), Some("2 nudges"));
        assert_eq!(
            display.background_task_summary.as_deref(),
            Some("2 local agents still running")
        );
    }

    #[test]
    fn memory_saved_combines_private_and_team_segments() {
        let projection = project_system_text_message(&input(
            SystemTextProjectionInput::MemorySaved(SystemMemorySavedInput {
                written_paths: vec!["/tmp/a.md".into(), "/tmp/b.md".into(), "/tmp/c.md".into()],
                verb: Some("Improved".into()),
                team_count: 1,
            }),
        ));
        let SystemTextProjection::MemorySaved(display) = projection else {
            panic!("expected memory saved");
        };
        assert_eq!(display.verb, "Improved");
        assert_eq!(display.parts, vec!["2 memories", "1 team memory"]);
        assert_eq!(display.entries[0].basename, "a.md");
        assert_eq!(display.entries[0].open_path, "/tmp/a.md");
    }

    #[test]
    fn stop_hook_summary_hides_without_errors_or_label_but_projects_otherwise() {
        assert_eq!(
            project_system_text_message(&input(SystemTextProjectionInput::StopHookSummary(
                SystemStopHookSummaryInput {
                    hook_count: 1,
                    hook_infos: vec![],
                    hook_errors: vec![],
                    prevented_continuation: false,
                    stop_reason: None,
                    hook_label: None,
                    total_duration_ms: None,
                    internal_build: false,
                    hook_timing_display_threshold_ms: 500,
                    terminal_columns: 120,
                }
            ))),
            SystemTextProjection::Hidden
        );

        let projection = project_system_text_message(&SystemTextMessageInput {
            add_margin: true,
            verbose: false,
            is_transcript_mode: true,
            background: Some("messageActionsBackground".into()),
            terminal_columns: Some(120),
            message: SystemTextProjectionInput::StopHookSummary(SystemStopHookSummaryInput {
                hook_count: 2,
                hook_infos: vec![StopHookInfoDisplay {
                    command: "prompt".into(),
                    prompt_text: Some("ask".into()),
                    duration_ms: Some(500),
                }],
                hook_errors: vec![],
                prevented_continuation: false,
                stop_reason: None,
                hook_label: Some("PreToolUse".into()),
                total_duration_ms: Some(1_000),
                internal_build: true,
                hook_timing_display_threshold_ms: 500,
                terminal_columns: 120,
            }),
        });
        let SystemTextProjection::StopHookSummary(StopHookSummaryDisplay::Labeled {
            summary,
            transcript_lines,
        }) = projection
        else {
            panic!("expected labeled stop hook summary");
        };
        assert_eq!(summary, "Ran 2 PreToolUse hooks (1.0s)");
        assert_eq!(transcript_lines, vec!["     \u{23BF} prompt: ask (0.5s)"]);

        let projection = project_system_text_message(&input(
            SystemTextProjectionInput::StopHookSummary(SystemStopHookSummaryInput {
                hook_count: 2,
                hook_infos: vec![StopHookInfoDisplay {
                    command: "lint".into(),
                    prompt_text: None,
                    duration_ms: None,
                }],
                hook_errors: vec!["bad".into()],
                prevented_continuation: true,
                stop_reason: Some("Stopped".into()),
                hook_label: None,
                total_duration_ms: None,
                internal_build: true,
                hook_timing_display_threshold_ms: 500,
                terminal_columns: 100,
            }),
        ));
        let SystemTextProjection::StopHookSummary(StopHookSummaryDisplay::Default {
            summary,
            detail_lines,
            prevented_line,
            error_lines,
            show_expand_hint,
            width,
            ..
        }) = projection
        else {
            panic!("expected default stop hook summary");
        };
        assert_eq!(summary, "Ran 2 stop hooks");
        assert_eq!(detail_lines, Vec::<String>::new());
        assert_eq!(prevented_line.as_deref(), Some("\u{23BF}  Stopped"));
        assert_eq!(error_lines, vec!["\u{23BF}  Stop hook error: bad"]);
        assert!(show_expand_hint);
        assert_eq!(width, 90);
    }

    #[test]
    fn memory_saved_entries_carry_hover_state_hints() {
        let projection = project_system_text_message(&input(
            SystemTextProjectionInput::MemorySaved(SystemMemorySavedInput {
                written_paths: vec!["/tmp/a.md".into()],
                verb: None,
                team_count: 0,
            }),
        ));
        let SystemTextProjection::MemorySaved(display) = projection else {
            panic!("expected memory saved");
        };
        assert_eq!(display.verb, "Saved");
        assert_eq!(display.parts, vec!["1 memory"]);
        let entry = &display.entries[0];
        assert_eq!(
            entry.idle_style,
            MemoryFileRowStyleHint {
                dim: true,
                underline: false,
            }
        );
        assert_eq!(
            entry.hover_style,
            MemoryFileRowStyleHint {
                dim: false,
                underline: true,
            }
        );
    }

    #[test]
    fn stop_hook_summary_default_summary_uses_singular_hook_for_one() {
        let projection = project_system_text_message(&input(
            SystemTextProjectionInput::StopHookSummary(SystemStopHookSummaryInput {
                hook_count: 1,
                hook_infos: vec![StopHookInfoDisplay {
                    command: "lint".into(),
                    prompt_text: None,
                    duration_ms: None,
                }],
                hook_errors: vec!["bad".into()],
                prevented_continuation: false,
                stop_reason: None,
                hook_label: None,
                total_duration_ms: None,
                internal_build: false,
                hook_timing_display_threshold_ms: 500,
                terminal_columns: 100,
            }),
        ));
        let SystemTextProjection::StopHookSummary(StopHookSummaryDisplay::Default {
            summary, ..
        }) = projection
        else {
            panic!("expected default stop hook summary");
        };
        assert_eq!(summary, "Ran 1 stop hook");
    }
}
