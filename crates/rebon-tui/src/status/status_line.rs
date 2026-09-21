//! Bottom-of-screen status bar.
//!
//! The bar itself runs a debounced pipeline that calls a user-supplied
//! shell command and renders the result as a single line. That pipeline
//! (debounce timer, cancellation, state and lifecycle handling,
//! hot-reload re-logging, notification on a trust block) is
//! consumer-side plumbing and is not modelled here.
//!
//! ## Pure logic covered here
//!
//! 1. [`status_line_should_display`] — the visibility gate: hidden while
//!    assistant mode is both enabled and active, otherwise shown whenever a
//!    `statusLine` block is configured.
//! 2. [`status_line_padding`] — resolves `statusLine.padding`, defaulting
//!    to 0 when it is unset or no `statusLine` block exists.
//! 3. [`build_status_line_command_input`] — assembles the
//!    `StatusLineCommandInput` payload, including the optional fields
//!    (`session_name`, `vim`, `agent`, `remote`, `worktree`,
//!    `rate_limits`).
//!
//! ## Outbound seams
//!
//! Every value the payload needs — the assistant-mode flags, the current
//! working directory, the session title, the raw rate-limit utilization,
//! and so on — is resolved by the consumer and fed in through
//! [`BuildStatusLineInputs`]. This module never reads config itself.

use rebon_types::{
    effort_indicator::{effort_level_to_symbol, EffortProviderKind},
    ReasoningEffort,
};
use std::collections::BTreeMap;

/// The settings slice the gate reads: the `statusLine` block, whose
/// presence is the gate and whose `padding` the bar uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLineSettings {
    /// The `statusLine` block, if configured.
    pub status_line: Option<StatusLineConfig>,
}

/// The `statusLine` block from settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLineConfig {
    /// Shell command to run.
    pub command: String,
    /// Optional padding (defaults to 0 in
    /// [`status_line_padding`]).
    pub padding: Option<u32>,
}

/// Whether the status line should be displayed.
///
/// * If `assistant_mode_enabled && assistant_mode_active` ⇒ `false`.
/// * Otherwise ⇒ `settings.status_line.is_some()`.
pub fn status_line_should_display(
    settings: &StatusLineSettings,
    assistant_mode_enabled: bool,
    assistant_mode_active: bool,
) -> bool {
    if assistant_mode_enabled && assistant_mode_active {
        return false;
    }
    settings.status_line.is_some()
}

/// Horizontal padding, defaulting to 0 when unset.
pub fn status_line_padding(settings: &StatusLineSettings) -> u32 {
    settings
        .status_line
        .as_ref()
        .and_then(|c| c.padding)
        .unwrap_or(0)
}

/// The `model` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Resolved model id.
    pub id: String,
    /// Display name for the resolved model.
    pub display_name: String,
}

/// The `workspace` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Current working directory.
    pub current_dir: String,
    /// Project directory.
    pub project_dir: String,
    /// Additional directories added to the workspace.
    pub added_dirs: Vec<String>,
}

/// The `cost` block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostTotals {
    /// Total cost in US dollars.
    pub total_cost_usd: f64,
    /// Total wall-clock duration.
    pub total_duration_ms: u64,
    /// Total time spent in API calls.
    pub total_api_duration_ms: u64,
    /// Total lines added.
    pub total_lines_added: u64,
    /// Total lines removed.
    pub total_lines_removed: u64,
}

/// The `context_window` block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextWindow {
    /// Total input tokens.
    pub total_input_tokens: u64,
    /// Total output tokens.
    pub total_output_tokens: u64,
    /// Context-window size for the resolved model.
    pub context_window_size: u64,
    /// Current context usage.
    pub current_usage: u64,
    /// Used share of the context window, as a percentage.
    pub used_percentage: f64,
    /// Remaining share of the context window, as a percentage.
    pub remaining_percentage: f64,
}

/// One rate-limit window: either the five-hour or the seven-day one.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindow {
    /// Utilized share of the window, as a percentage.
    pub used_percentage: f64,
    /// ISO timestamp of the next reset.
    pub resets_at: String,
}

/// The `rate_limits` block. Both windows are optional; the block is only
/// included in the payload when at least one is present.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimits {
    /// The five-hour window.
    pub five_hour: Option<RateLimitWindow>,
    /// The seven-day window.
    pub seven_day: Option<RateLimitWindow>,
}

impl RateLimits {
    /// Whether at least one window is present.
    pub fn any(&self) -> bool {
        self.five_hour.is_some() || self.seven_day.is_some()
    }
}

/// The `agent` block. Only included when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// Agent type name.
    pub name: String,
}

/// The `output_style` block, nested so consumers see
/// `output_style.name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyle {
    /// Output style name, defaulting to the configuration default.
    pub name: String,
}

/// The `vim` block, nested so consumers see `vim.mode`. Only included
/// when vim mode is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VimInfo {
    /// Current vim mode. Defaults to `"INSERT"` when
    /// [`BuildStatusLineInputs::vim_mode`] is `None`.
    pub mode: String,
}

/// The `remote` block. Only included in remote mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    /// Remote session id.
    pub session_id: String,
}

/// `worktree` block. Only included when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSession {
    /// Worktree name.
    pub name: String,
    /// Worktree path.
    pub path: String,
    /// Worktree branch.
    pub branch: String,
    /// Original working directory.
    pub original_cwd: String,
    /// Original branch.
    pub original_branch: String,
}

/// The `effort` block. Only included when the model supports effort
/// (Anthropic) or thinking level (OpenAI-compatible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortInfo {
    /// Resolved effort level string (`"low"`, `"medium"`, `"high"`, `"xhigh"`).
    pub level: String,
    /// Glyph symbol for the effort level (e.g. `"◉"` for xhigh).
    pub symbol: String,
    /// Provider-aware concept label: `"effort"` for Anthropic,
    /// `"thinking"` for OpenAI-compatible.
    pub label: String,
}

/// The full payload assembled by [`build_status_line_command_input`].
/// `base_hook_input` is the flat hook-input map that leads the payload;
/// it is kept as an opaque key/value bag because its precise contents are
/// produced outside this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusLineCommandInput {
    /// Pre-resolved base hook input. Stored as a sorted map so the field
    /// ordering is deterministic.
    pub base_hook_input: BTreeMap<String, String>,
    /// The `session_name` field, only present when a session title exists.
    pub session_name: Option<String>,
    /// Resolved model info.
    pub model: ModelInfo,
    /// Workspace block.
    pub workspace: Workspace,
    /// CLI version string.
    pub version: String,
    /// The `output_style` block. Always present.
    pub output_style: OutputStyle,
    /// The `cost` block.
    pub cost: CostTotals,
    /// The `context_window` block.
    pub context_window: ContextWindow,
    /// `exceeds_200k_tokens` flag.
    pub exceeds_200k_tokens: bool,
    /// The `rate_limits` block, only present when at least one window is
    /// set.
    pub rate_limits: Option<RateLimits>,
    /// The `vim` block, only present when vim mode is enabled. The
    /// inner `mode` defaults to `"INSERT"` when no concrete mode is
    /// provided.
    pub vim: Option<VimInfo>,
    /// The `agent` block, only present when set.
    pub agent: Option<AgentInfo>,
    /// The `remote` block, only present in remote mode.
    pub remote: Option<RemoteInfo>,
    /// The `worktree` block, only present when set.
    pub worktree: Option<WorktreeSession>,
    /// The `effort` block, only present when the model supports effort
    /// (or thinking level for OpenAI-compatible providers).
    pub effort: Option<EffortInfo>,
}

/// All the values the payload needs, resolved up-front by the consumer.
#[derive(Debug, Clone)]
pub struct BuildStatusLineInputs {
    /// Sorted base hook input map.
    pub base_hook_input: BTreeMap<String, String>,
    /// Session title, when one exists.
    pub session_name: Option<String>,
    /// Resolved model.
    pub model: ModelInfo,
    /// Workspace block.
    pub workspace: Workspace,
    /// CLI version string.
    pub version: String,
    /// Output style name, defaulting to the configuration default.
    pub output_style_name: String,
    /// Cost totals.
    pub cost: CostTotals,
    /// Context window block.
    pub context_window: ContextWindow,
    /// Whether the most recent assistant message exceeds 200k tokens.
    pub exceeds_200k_tokens: bool,
    /// Pre-built rate limits (`five_hour` and/or `seven_day`).
    pub rate_limits: RateLimits,
    /// Whether vim mode is enabled.
    pub is_vim_mode_enabled: bool,
    /// Optional concrete vim mode. The default is `"INSERT"` when vim
    /// mode is enabled but no mode is supplied.
    pub vim_mode: Option<String>,
    /// Agent type name, when one is set.
    pub agent_name: Option<String>,
    /// Whether the session is in remote mode.
    pub is_remote_mode: bool,
    /// Session id (only used when `is_remote_mode`).
    pub remote_session_id: Option<String>,
    /// Current worktree session, when one exists.
    pub worktree: Option<WorktreeSession>,
    /// Resolved displayed effort level. `None` when the model does not
    /// support effort; the consumer resolves it and passes it in.
    pub effort_level: Option<ReasoningEffort>,
    /// Provider kind for effort labelling. Determines whether the
    /// status line says "effort" (Anthropic) or "thinking" (OpenAI).
    pub effort_provider_kind: EffortProviderKind,
}

/// Assemble the payload from the pre-resolved inputs.
pub fn build_status_line_command_input(inputs: BuildStatusLineInputs) -> StatusLineCommandInput {
    let rate_limits = if inputs.rate_limits.any() {
        Some(inputs.rate_limits.clone())
    } else {
        None
    };

    let vim = if inputs.is_vim_mode_enabled {
        Some(VimInfo {
            mode: inputs.vim_mode.unwrap_or_else(|| "INSERT".to_string()),
        })
    } else {
        None
    };

    let effort = inputs.effort_level.map(|level| EffortInfo {
        level: level.as_str().to_string(),
        symbol: effort_level_to_symbol(level).to_string(),
        label: inputs.effort_provider_kind.label().to_string(),
    });

    let agent = inputs.agent_name.map(|name| AgentInfo { name });
    let remote = if inputs.is_remote_mode {
        inputs
            .remote_session_id
            .map(|session_id| RemoteInfo { session_id })
    } else {
        None
    };

    StatusLineCommandInput {
        base_hook_input: inputs.base_hook_input,
        session_name: inputs.session_name,
        model: inputs.model,
        workspace: inputs.workspace,
        version: inputs.version,
        output_style: OutputStyle {
            name: inputs.output_style_name,
        },
        cost: inputs.cost,
        context_window: inputs.context_window,
        exceeds_200k_tokens: inputs.exceeds_200k_tokens,
        rate_limits,
        vim,
        agent,
        remote,
        worktree: inputs.worktree,
        effort,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_status_line(padding: Option<u32>) -> StatusLineSettings {
        StatusLineSettings {
            status_line: Some(StatusLineConfig {
                command: "echo hi".into(),
                padding,
            }),
        }
    }

    fn settings_without_status_line() -> StatusLineSettings {
        StatusLineSettings { status_line: None }
    }

    #[test]
    fn should_display_when_status_line_set() {
        assert!(status_line_should_display(
            &settings_with_status_line(None),
            false,
            false
        ));
    }

    #[test]
    fn should_hide_when_status_line_unset() {
        assert!(!status_line_should_display(
            &settings_without_status_line(),
            false,
            false
        ));
    }

    #[test]
    fn should_hide_when_assistant_mode_active() {
        assert!(!status_line_should_display(
            &settings_with_status_line(None),
            true,
            true
        ));
    }

    #[test]
    fn assistant_mode_disabled_does_not_hide() {
        assert!(status_line_should_display(
            &settings_with_status_line(None),
            false,
            true
        ));
    }

    #[test]
    fn assistant_mode_inactive_does_not_hide() {
        assert!(status_line_should_display(
            &settings_with_status_line(None),
            true,
            false
        ));
    }

    #[test]
    fn padding_defaults_to_zero_when_no_status_line() {
        assert_eq!(status_line_padding(&settings_without_status_line()), 0);
    }

    #[test]
    fn padding_defaults_to_zero_when_unset() {
        assert_eq!(status_line_padding(&settings_with_status_line(None)), 0);
    }

    #[test]
    fn padding_passes_through_set_value() {
        assert_eq!(status_line_padding(&settings_with_status_line(Some(2))), 2);
    }

    fn sample_inputs() -> BuildStatusLineInputs {
        BuildStatusLineInputs {
            base_hook_input: BTreeMap::new(),
            session_name: None,
            model: ModelInfo {
                id: "claude-opus-4-6".into(),
                display_name: "Claude Opus 4.6".into(),
            },
            workspace: Workspace {
                current_dir: "/cwd".into(),
                project_dir: "/proj".into(),
                added_dirs: vec![],
            },
            version: "1.2.3".into(),
            output_style_name: "default".into(),
            cost: CostTotals {
                total_cost_usd: 0.0,
                total_duration_ms: 0,
                total_api_duration_ms: 0,
                total_lines_added: 0,
                total_lines_removed: 0,
            },
            context_window: ContextWindow {
                total_input_tokens: 0,
                total_output_tokens: 0,
                context_window_size: 200_000,
                current_usage: 0,
                used_percentage: 0.0,
                remaining_percentage: 100.0,
            },
            exceeds_200k_tokens: false,
            rate_limits: RateLimits::default(),
            is_vim_mode_enabled: false,
            vim_mode: None,
            agent_name: None,
            is_remote_mode: false,
            remote_session_id: None,
            worktree: None,
            effort_level: None,
            effort_provider_kind: EffortProviderKind::Anthropic,
        }
    }

    #[test]
    fn build_includes_required_fields() {
        let payload = build_status_line_command_input(sample_inputs());
        assert_eq!(payload.model.id, "claude-opus-4-6");
        assert_eq!(payload.workspace.current_dir, "/cwd");
        assert_eq!(payload.version, "1.2.3");
        assert_eq!(payload.output_style.name, "default");
    }

    #[test]
    fn build_output_style_is_nested_object() {
        // The shape must stay nested so consumers reading the JSON payload
        // see `output_style.name`, not a flat `output_style_name`.
        let mut inputs = sample_inputs();
        inputs.output_style_name = "concise".into();
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.output_style,
            OutputStyle {
                name: "concise".into()
            }
        );
    }

    #[test]
    fn build_session_name_only_when_set() {
        let payload = build_status_line_command_input(sample_inputs());
        assert_eq!(payload.session_name, None);

        let mut inputs = sample_inputs();
        inputs.session_name = Some("My Session".into());
        let payload = build_status_line_command_input(inputs);
        assert_eq!(payload.session_name.as_deref(), Some("My Session"));
    }

    #[test]
    fn build_rate_limits_omitted_when_both_empty() {
        let payload = build_status_line_command_input(sample_inputs());
        assert_eq!(payload.rate_limits, None);
    }

    #[test]
    fn build_rate_limits_present_when_five_hour_set() {
        let mut inputs = sample_inputs();
        inputs.rate_limits.five_hour = Some(RateLimitWindow {
            used_percentage: 50.0,
            resets_at: "2026-01-01T00:00:00Z".into(),
        });
        let payload = build_status_line_command_input(inputs);
        assert!(payload.rate_limits.is_some());
        assert!(payload.rate_limits.unwrap().five_hour.is_some());
    }

    #[test]
    fn build_rate_limits_present_when_seven_day_set() {
        let mut inputs = sample_inputs();
        inputs.rate_limits.seven_day = Some(RateLimitWindow {
            used_percentage: 25.0,
            resets_at: "2026-01-01T00:00:00Z".into(),
        });
        let payload = build_status_line_command_input(inputs);
        assert!(payload.rate_limits.is_some());
        assert!(payload.rate_limits.unwrap().seven_day.is_some());
    }

    #[test]
    fn build_vim_mode_omitted_when_disabled() {
        let payload = build_status_line_command_input(sample_inputs());
        assert_eq!(payload.vim, None);
    }

    #[test]
    fn build_vim_mode_defaults_to_insert_when_enabled_but_undefined() {
        let mut inputs = sample_inputs();
        inputs.is_vim_mode_enabled = true;
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.vim.as_ref().map(|v| v.mode.as_str()),
            Some("INSERT")
        );
    }

    #[test]
    fn build_vim_mode_passes_through_concrete_value() {
        let mut inputs = sample_inputs();
        inputs.is_vim_mode_enabled = true;
        inputs.vim_mode = Some("NORMAL".into());
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.vim.as_ref().map(|v| v.mode.as_str()),
            Some("NORMAL")
        );
    }

    #[test]
    fn build_vim_is_nested_object() {
        // The inner payload must stay nested under `vim.mode`, not flat
        // into a `vim_mode` field.
        let mut inputs = sample_inputs();
        inputs.is_vim_mode_enabled = true;
        inputs.vim_mode = Some("VISUAL".into());
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.vim,
            Some(VimInfo {
                mode: "VISUAL".into()
            })
        );
    }

    #[test]
    fn build_agent_only_when_set() {
        let payload = build_status_line_command_input(sample_inputs());
        assert_eq!(payload.agent, None);

        let mut inputs = sample_inputs();
        inputs.agent_name = Some("research".into());
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.agent.as_ref().map(|a| a.name.as_str()),
            Some("research")
        );
    }

    #[test]
    fn build_remote_only_when_in_remote_mode() {
        let mut inputs = sample_inputs();
        inputs.remote_session_id = Some("abc-123".into());
        // is_remote_mode = false ⇒ omitted
        let payload = build_status_line_command_input(inputs.clone());
        assert_eq!(payload.remote, None);

        inputs.is_remote_mode = true;
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.remote.as_ref().map(|r| r.session_id.as_str()),
            Some("abc-123")
        );
    }

    #[test]
    fn build_worktree_only_when_set() {
        let payload = build_status_line_command_input(sample_inputs());
        assert_eq!(payload.worktree, None);

        let mut inputs = sample_inputs();
        inputs.worktree = Some(WorktreeSession {
            name: "wt".into(),
            path: "/wt".into(),
            branch: "feat".into(),
            original_cwd: "/orig".into(),
            original_branch: "main".into(),
        });
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.worktree.as_ref().map(|w| w.branch.as_str()),
            Some("feat")
        );
    }

    #[test]
    fn build_passes_through_exceeds_200k() {
        let mut inputs = sample_inputs();
        inputs.exceeds_200k_tokens = true;
        let payload = build_status_line_command_input(inputs);
        assert!(payload.exceeds_200k_tokens);
    }

    #[test]
    fn build_passes_through_workspace_added_dirs() {
        let mut inputs = sample_inputs();
        inputs.workspace.added_dirs = vec!["/a".into(), "/b".into()];
        let payload = build_status_line_command_input(inputs);
        assert_eq!(payload.workspace.added_dirs, vec!["/a", "/b"]);
    }

    #[test]
    fn build_includes_base_hook_input_keys() {
        let mut base = BTreeMap::new();
        base.insert("hook_event_name".into(), "Stop".into());
        let mut inputs = sample_inputs();
        inputs.base_hook_input = base.clone();
        let payload = build_status_line_command_input(inputs);
        assert_eq!(payload.base_hook_input, base);
    }

    #[test]
    fn rate_limits_any_returns_false_when_both_none() {
        assert!(!RateLimits::default().any());
    }

    #[test]
    fn rate_limits_any_returns_true_when_either_set() {
        let mut rl = RateLimits::default();
        rl.five_hour = Some(RateLimitWindow {
            used_percentage: 1.0,
            resets_at: "x".into(),
        });
        assert!(rl.any());
    }

    #[test]
    fn build_effort_omitted_when_none() {
        let payload = build_status_line_command_input(sample_inputs());
        assert_eq!(payload.effort, None);
    }

    #[test]
    fn build_effort_present_when_set() {
        let mut inputs = sample_inputs();
        inputs.effort_level = Some(ReasoningEffort::High);
        let payload = build_status_line_command_input(inputs);
        assert!(payload.effort.is_some());
    }

    #[test]
    fn build_effort_level_string_matches() {
        let mut inputs = sample_inputs();
        inputs.effort_level = Some(ReasoningEffort::XHigh);
        let payload = build_status_line_command_input(inputs);
        let effort = payload.effort.unwrap();
        assert_eq!(effort.level, "xhigh");
    }

    #[test]
    fn build_effort_symbol_matches_indicator() {
        use rebon_types::effort_indicator::{EFFORT_HIGH, EFFORT_LOW, EFFORT_MEDIUM, EFFORT_XHIGH};

        for (level, expected_symbol) in [
            (ReasoningEffort::Low, EFFORT_LOW),
            (ReasoningEffort::Medium, EFFORT_MEDIUM),
            (ReasoningEffort::High, EFFORT_HIGH),
            (ReasoningEffort::XHigh, EFFORT_XHIGH),
        ] {
            let mut inputs = sample_inputs();
            inputs.effort_level = Some(level);
            let payload = build_status_line_command_input(inputs);
            let effort = payload.effort.unwrap();
            assert_eq!(
                effort.symbol,
                expected_symbol,
                "symbol mismatch for {}",
                level.as_str()
            );
            assert_eq!(effort.level, level.as_str());
        }
    }

    #[test]
    fn build_effort_is_nested_object() {
        let mut inputs = sample_inputs();
        inputs.effort_level = Some(ReasoningEffort::Medium);
        let payload = build_status_line_command_input(inputs);
        assert_eq!(
            payload.effort,
            Some(EffortInfo {
                level: "medium".into(),
                symbol: "\u{25d0}".into(),
                label: "effort".into(),
            })
        );
    }

    #[test]
    fn build_effort_label_anthropic() {
        let mut inputs = sample_inputs();
        inputs.effort_level = Some(ReasoningEffort::High);
        inputs.effort_provider_kind = EffortProviderKind::Anthropic;
        let payload = build_status_line_command_input(inputs);
        assert_eq!(payload.effort.as_ref().unwrap().label, "effort");
    }

    #[test]
    fn build_effort_label_openai() {
        let mut inputs = sample_inputs();
        inputs.effort_level = Some(ReasoningEffort::High);
        inputs.effort_provider_kind = EffortProviderKind::OpenAi;
        let payload = build_status_line_command_input(inputs);
        assert_eq!(payload.effort.as_ref().unwrap().label, "thinking");
    }
}
