//! Shared ACP-compatible data types used across multiple rebon crates.
//!
//! Consumers need the wire shapes without the full ACP transport layer, so the
//! types live here and the transport depends on them, not the reverse.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{HashDriftRecord, UltraplanManifestSnapshot, UltraplanProfile};

// ==========================
// Common type aliases
// ==========================

pub type SessionId = String;

pub const MAX_NESTED_WORKFLOW_DEPTH: usize = 1;

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

// ==========================
// Usage accounting
// ==========================

/// Token usage reported by model providers.
///
/// Provider-agnostic, so transport and UI code can carry usage without
/// depending on any model client. Deserialization absorbs the per-provider
/// spellings (OpenAI, DeepSeek, Moonshot Kimi) below.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub prompt_cache_miss_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub total_input_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub total_output_tokens: u32,
    /// Reasoning/thinking tokens included in `output_tokens`, when the
    /// provider reports them separately.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub reasoning_tokens: u32,
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        struct PromptTokensDetails {
            #[serde(default)]
            cached_tokens: u32,
        }

        #[derive(Deserialize, Default)]
        struct OutputTokensDetails {
            #[serde(default)]
            reasoning_tokens: u32,
        }

        #[derive(Deserialize, Default)]
        struct UsageIteration {
            #[serde(default, alias = "prompt_tokens")]
            input_tokens: u32,
            #[serde(default, alias = "completion_tokens")]
            output_tokens: u32,
            #[serde(default)]
            cache_read_input_tokens: u32,
            #[serde(default)]
            cache_creation_input_tokens: u32,
        }

        #[derive(Deserialize, Default)]
        struct RawUsage {
            #[serde(default, alias = "prompt_tokens")]
            input_tokens: u32,
            #[serde(default, alias = "completion_tokens")]
            output_tokens: u32,
            #[serde(default)]
            cache_read_input_tokens: u32,
            #[serde(default)]
            cache_creation_input_tokens: u32,
            #[serde(default)]
            prompt_cache_hit_tokens: u32,
            #[serde(default)]
            prompt_cache_miss_tokens: u32,
            #[serde(default)]
            prompt_tokens_details: Option<PromptTokensDetails>,
            #[serde(default)]
            input_tokens_details: Option<PromptTokensDetails>,
            /// Moonshot Kimi reports the prefix-cache hit as a top-level
            /// `cached_tokens` rather than inside `prompt_tokens_details`.
            #[serde(default)]
            cached_tokens: u32,
            #[serde(default)]
            total_input_tokens: u32,
            #[serde(default)]
            total_output_tokens: u32,
            #[serde(default)]
            reasoning_tokens: u32,
            #[serde(default, alias = "completion_tokens_details")]
            output_tokens_details: Option<OutputTokensDetails>,
            #[serde(default)]
            iterations: Vec<UsageIteration>,
        }

        let raw = RawUsage::deserialize(deserializer)?;
        let mut hit = raw.prompt_cache_hit_tokens;
        let mut miss = raw.prompt_cache_miss_tokens;

        if hit == 0 && miss == 0 {
            let cached = raw
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .filter(|n| *n > 0)
                .or_else(|| {
                    raw.input_tokens_details
                        .as_ref()
                        .map(|d| d.cached_tokens)
                        .filter(|n| *n > 0)
                })
                .or_else(|| Some(raw.cached_tokens).filter(|n| *n > 0));
            if let Some(cached) = cached {
                hit = cached;
                miss = raw.input_tokens.saturating_sub(cached);
            }
        }

        let total_input_tokens = raw.iterations.iter().fold(0u32, |sum, iteration| {
            sum.saturating_add(iteration.input_tokens)
                .saturating_add(iteration.cache_read_input_tokens)
                .saturating_add(iteration.cache_creation_input_tokens)
        });
        let total_output_tokens = raw.iterations.iter().fold(0u32, |sum, iteration| {
            sum.saturating_add(iteration.output_tokens)
        });
        let total_input_tokens = total_input_tokens.max(raw.total_input_tokens);
        let total_output_tokens = total_output_tokens.max(raw.total_output_tokens);

        let reasoning_tokens = if raw.reasoning_tokens > 0 {
            raw.reasoning_tokens
        } else {
            raw.output_tokens_details
                .as_ref()
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0)
        };

        Ok(Usage {
            input_tokens: raw.input_tokens,
            output_tokens: raw.output_tokens,
            cache_read_input_tokens: raw.cache_read_input_tokens,
            cache_creation_input_tokens: raw.cache_creation_input_tokens,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            total_input_tokens,
            total_output_tokens,
            reasoning_tokens,
        })
    }
}

impl Usage {
    pub fn billed_input_tokens(&self) -> u32 {
        if self.total_input_tokens > 0 {
            return self.total_input_tokens;
        }

        if self.prompt_cache_hit_tokens > 0 || self.prompt_cache_miss_tokens > 0 {
            if self.input_tokens > 0 {
                self.input_tokens
            } else {
                self.prompt_cache_hit_tokens
                    .saturating_add(self.prompt_cache_miss_tokens)
            }
        } else {
            self.input_tokens
                .saturating_add(self.cache_read_input_tokens)
                .saturating_add(self.cache_creation_input_tokens)
        }
    }

    pub fn billed_output_tokens(&self) -> u32 {
        if self.total_output_tokens > 0 {
            self.total_output_tokens
        } else {
            self.output_tokens
        }
    }

    pub fn merge(&mut self, other: &Usage) {
        if other.input_tokens > 0 {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens > 0 {
            self.output_tokens = other.output_tokens;
        }
        if other.cache_read_input_tokens > 0 {
            self.cache_read_input_tokens = other.cache_read_input_tokens;
        }
        if other.cache_creation_input_tokens > 0 {
            self.cache_creation_input_tokens = other.cache_creation_input_tokens;
        }
        if other.prompt_cache_hit_tokens > 0 {
            self.prompt_cache_hit_tokens = other.prompt_cache_hit_tokens;
        }
        if other.prompt_cache_miss_tokens > 0 {
            self.prompt_cache_miss_tokens = other.prompt_cache_miss_tokens;
        }
        if other.total_input_tokens > 0 {
            self.total_input_tokens = other.total_input_tokens;
        }
        if other.total_output_tokens > 0 {
            self.total_output_tokens = other.total_output_tokens;
        }
        if other.reasoning_tokens > 0 {
            self.reasoning_tokens = other.reasoning_tokens;
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn usage_iterations_track_total_billed_tokens_without_overwriting_context_tokens() {
        let usage: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": 23_000,
            "output_tokens": 1_000,
            "iterations": [
                {"type": "compaction", "input_tokens": 180_000, "output_tokens": 3_500},
                {"type": "message", "input_tokens": 23_000, "output_tokens": 1_000}
            ]
        }))
        .unwrap();

        assert_eq!(usage.input_tokens, 23_000);
        assert_eq!(usage.output_tokens, 1_000);
        assert_eq!(usage.total_input_tokens, 203_000);
        assert_eq!(usage.total_output_tokens, 4_500);
        assert_eq!(usage.billed_input_tokens(), 203_000);
        assert_eq!(usage.billed_output_tokens(), 4_500);
    }

    #[test]
    fn reasoning_tokens_parse_from_details_or_flat_field() {
        let usage: Usage = serde_json::from_value(serde_json::json!({
            "completion_tokens": 100,
            "completion_tokens_details": {"reasoning_tokens": 40}
        }))
        .unwrap();
        assert_eq!(usage.output_tokens, 100);
        assert_eq!(usage.reasoning_tokens, 40);

        let usage: Usage = serde_json::from_value(serde_json::json!({
            "output_tokens": 5,
            "reasoning_tokens": 2
        }))
        .unwrap();
        assert_eq!(usage.reasoning_tokens, 2);

        let usage: Usage = serde_json::from_value(serde_json::json!({"output_tokens": 5})).unwrap();
        assert_eq!(usage.reasoning_tokens, 0);
    }

    #[test]
    fn ultrawork_execution_child_turn_preserves_cards_and_hash_drift() {
        let mut context =
            UltraplanContext::ultrawork_execution_controller_turn("run-1", PolicyMode::Enforce)
                .with_profile(UltraplanProfile::Grill);
        context.execution_cards.push(crate::ExecutionCard {
            step: "1".into(),
            covers: Some("R1".into()),
            files: vec!["src/lib.rs".into()],
            change: "change it".into(),
            verify: "cargo test".into(),
        });
        context.hash_drift.push(crate::HashDriftRecord {
            path: "src/lib.rs".into(),
            stored_sha256: "old".into(),
            current_sha256: Some("new".into()),
            kind: crate::HashDriftKind::Changed,
        });

        let child = context
            .ultrawork_execution_child_turn()
            .expect("child context");

        assert_eq!(child.execution_cards, context.execution_cards);
        assert_eq!(child.hash_drift, context.hash_drift);
        assert_eq!(child.phase, "ultrawork_child");
        assert_eq!(child.profile, UltraplanProfile::Grill);
        assert!(child.plan_fidelity);
    }

    #[test]
    fn workflow_controller_child_turn_is_writable_without_plan_fidelity() {
        let context = UltraplanContext::workflow_controller_turn(
            "workflow_controller",
            "workflow",
            PolicyMode::Enforce,
        );

        let child = context
            .workflow_controller_child_turn()
            .expect("child context");

        assert_eq!(child.phase, "workflow_child");
        assert!(!child.read_only);
        assert!(!child.plan_fidelity);
        assert_eq!(child.shell_policy, ShellPolicy::AllowShell);
        assert!(child.allowed_tools.iter().any(|tool| tool == "Edit"));
        assert!(child.allowed_tools.iter().any(|tool| tool == "Write"));
        assert!(child.allowed_tools.iter().any(|tool| tool == "Bash"));
    }

    #[test]
    fn workflow_controller_child_turn_does_not_fire_for_other_phases() {
        // The ultrawork execution controller (plan fidelity) and the
        // ultraplan planning phases must keep their own derivations.
        let execution =
            UltraplanContext::ultrawork_execution_controller_turn("run-1", PolicyMode::Enforce);
        assert!(execution.workflow_controller_child_turn().is_none());

        let planning = UltraplanContext::planning_turn("run-1", "planning", PolicyMode::Enforce);
        assert!(planning.workflow_controller_child_turn().is_none());
    }

    #[test]
    fn execution_policy_continuity_is_explicit_for_goal_and_ultrawork_constructors() {
        assert!(ExecutionPolicy::workflow_controller().auto_mode_script_continuity);
        assert!(
            ExecutionPolicy::ultrawork_execution_controller("run-1").auto_mode_script_continuity
        );
        assert!(ExecutionPolicy::goal().auto_mode_script_continuity);
        assert!(ExecutionPolicy::goal().is_active());
        assert!(
            !ExecutionPolicy::ultraplan(UltraplanContext::ultrawork_execution_controller_turn(
                "run-1",
                PolicyMode::Enforce,
            ))
            .auto_mode_script_continuity
        );
        assert!(
            !ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
                "run-1",
                "planning",
                PolicyMode::Enforce,
            ))
            .auto_mode_script_continuity
        );
    }

    #[test]
    fn usage_merge_prefers_nonzero_values() {
        let mut a = Usage {
            input_tokens: 10,
            ..Default::default()
        };
        let b = Usage {
            output_tokens: 42,
            cache_read_input_tokens: 5,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 7,
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.input_tokens, 10);
        assert_eq!(a.output_tokens, 42);
        assert_eq!(a.cache_read_input_tokens, 5);
        assert_eq!(a.prompt_cache_hit_tokens, 6);
        assert_eq!(a.prompt_cache_miss_tokens, 7);
    }

    #[test]
    fn usage_deserializes_openai_and_deepseek_token_aliases() {
        let usage: Usage = serde_json::from_str(
            r#"{
                "prompt_tokens": 100,
                "completion_tokens": 12,
                "prompt_cache_hit_tokens": 80,
                "prompt_cache_miss_tokens": 20
            }"#,
        )
        .unwrap();

        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(usage.prompt_cache_hit_tokens, 80);
        assert_eq!(usage.prompt_cache_miss_tokens, 20);
        assert_eq!(usage.billed_input_tokens(), 100);
    }

    #[test]
    fn usage_deserializes_openai_nested_cached_tokens() {
        let usage: Usage = serde_json::from_str(
            r#"{
                "prompt_tokens": 1000,
                "completion_tokens": 50,
                "prompt_tokens_details": { "cached_tokens": 800 }
            }"#,
        )
        .unwrap();

        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.prompt_cache_hit_tokens, 800);
        assert_eq!(usage.prompt_cache_miss_tokens, 200);
    }

    #[test]
    fn usage_deserializes_kimi_top_level_cached_tokens() {
        // Moonshot Kimi: `usage.cached_tokens` is a sibling of
        // `prompt_tokens`, not nested under `prompt_tokens_details`.
        let usage: Usage = serde_json::from_str(
            r#"{
                "prompt_tokens": 1000,
                "completion_tokens": 50,
                "cached_tokens": 640
            }"#,
        )
        .unwrap();

        assert_eq!(usage.prompt_cache_hit_tokens, 640);
        assert_eq!(usage.prompt_cache_miss_tokens, 360);

        // The nested OpenAI spelling still wins when both are present, so a
        // gateway that emits both cannot double-report.
        let usage: Usage = serde_json::from_str(
            r#"{
                "prompt_tokens": 1000,
                "cached_tokens": 640,
                "prompt_tokens_details": { "cached_tokens": 800 }
            }"#,
        )
        .unwrap();
        assert_eq!(usage.prompt_cache_hit_tokens, 800);
    }

    #[test]
    fn usage_prefers_deepseek_top_level_over_openai_nested() {
        let usage: Usage = serde_json::from_str(
            r#"{
                "prompt_tokens": 1000,
                "prompt_cache_hit_tokens": 700,
                "prompt_cache_miss_tokens": 300,
                "prompt_tokens_details": { "cached_tokens": 800 }
            }"#,
        )
        .unwrap();

        assert_eq!(usage.prompt_cache_hit_tokens, 700);
        assert_eq!(usage.prompt_cache_miss_tokens, 300);
    }

    #[test]
    fn usage_deserializes_responses_input_tokens_details() {
        let usage: Usage = serde_json::from_str(
            r#"{
                "input_tokens": 12000,
                "input_tokens_details": { "cached_tokens": 9500 },
                "output_tokens": 250,
                "output_tokens_details": { "reasoning_tokens": 64 },
                "total_tokens": 12250
            }"#,
        )
        .unwrap();

        assert_eq!(usage.input_tokens, 12000);
        assert_eq!(usage.output_tokens, 250);
        assert_eq!(usage.prompt_cache_hit_tokens, 9500);
        assert_eq!(usage.prompt_cache_miss_tokens, 2500);
    }

    #[test]
    fn usage_prefers_prompt_tokens_details_when_both_nested_keys_present() {
        let usage: Usage = serde_json::from_str(
            r#"{
                "input_tokens": 1000,
                "prompt_tokens_details": { "cached_tokens": 700 },
                "input_tokens_details": { "cached_tokens": 800 }
            }"#,
        )
        .unwrap();

        assert_eq!(usage.prompt_cache_hit_tokens, 700);
        assert_eq!(usage.prompt_cache_miss_tokens, 300);
    }

    #[test]
    fn usage_nested_cached_tokens_zero_leaves_fields_zero() {
        let usage: Usage = serde_json::from_str(
            r#"{
                "prompt_tokens": 1000,
                "prompt_tokens_details": { "cached_tokens": 0 }
            }"#,
        )
        .unwrap();

        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.prompt_cache_hit_tokens, 0);
        assert_eq!(usage.prompt_cache_miss_tokens, 0);
    }

    #[test]
    fn usage_billed_input_tokens_falls_back_to_deepseek_split_fields() {
        let usage = Usage {
            prompt_cache_miss_tokens: 20,
            prompt_cache_hit_tokens: 80,
            ..Default::default()
        };

        assert_eq!(usage.billed_input_tokens(), 100);
    }

    #[test]
    fn usage_billed_input_tokens_includes_anthropic_cache_fields() {
        let usage = Usage {
            input_tokens: 10,
            cache_read_input_tokens: 80,
            cache_creation_input_tokens: 20,
            ..Default::default()
        };

        assert_eq!(usage.billed_input_tokens(), 110);
    }
}

// ==========================
// Request-scoped tool filter
// ==========================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFilterSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deny: BTreeSet<String>,
}

// ==========================
// Request-scoped execution policy
// ==========================

/// Per-request runtime policy threaded from the prompt boundary into
/// engine/tool dispatch. Absence means the turn is unrestricted by this
/// policy layer (normal legacy behaviour).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ultraplan: Option<UltraplanContext>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub eager_promotions: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_mode_script_continuity: bool,
}

impl ExecutionPolicy {
    pub fn ultraplan(context: UltraplanContext) -> Self {
        let eager_promotions = context.eager_promotions();
        Self {
            ultraplan: Some(context),
            eager_promotions,
            auto_mode_script_continuity: false,
        }
    }

    pub fn with_eager_promotions<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.eager_promotions
            .extend(tools.into_iter().map(Into::into));
        self
    }

    pub fn with_auto_mode_script_continuity(mut self) -> Self {
        self.auto_mode_script_continuity = true;
        self
    }

    pub fn goal() -> Self {
        Self::default().with_auto_mode_script_continuity()
    }

    pub fn workflow_controller() -> Self {
        Self::ultraplan(UltraplanContext::workflow_controller_turn(
            "workflow_controller",
            "workflow",
            PolicyMode::Enforce,
        ))
        .with_eager_promotions(["Workflow", "RunWorkflow"])
        .with_auto_mode_script_continuity()
    }

    pub fn ultrawork_execution_controller(run_id: impl Into<String>) -> Self {
        Self::ultraplan(UltraplanContext::ultrawork_execution_controller_turn(
            run_id,
            PolicyMode::Enforce,
        ))
        .with_eager_promotions(["Workflow", "RunWorkflow"])
        .with_auto_mode_script_continuity()
    }

    /// True when relaxed auto-mode script review may be stamped onto
    /// this turn. A goal turn carries no ultraplan restrictions and
    /// always qualifies; an ultraplan turn qualifies only in its
    /// execution phases, because the planning and scouting phases are
    /// read-only by contract.
    pub fn accepts_auto_mode_script_continuity(&self) -> bool {
        match self.ultraplan.as_ref() {
            None => true,
            Some(context) => context.is_execution_phase(),
        }
    }

    /// A continuity-only policy (a goal turn) counts as active so it
    /// survives the `is_active()` gate in
    /// `workflow_agent_execution_policy` and reaches the sub-agents that
    /// turn spawns. That inheritance is deliberate, and for the
    /// `workflow_controller`/`ultrawork_execution_controller` policies it
    /// is the only way continuity does anything at all: those controller
    /// phases are `read_only` with `Bash`/`PowerShell` denied, so the
    /// relaxation is observable solely on their child turns.
    pub fn is_active(&self) -> bool {
        self.ultraplan.is_some()
            || !self.eager_promotions.is_empty()
            || self.auto_mode_script_continuity
    }
}

/// Local `/ultraplan` policy context for one prompt turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanContext {
    pub run_id: String,
    #[serde(default)]
    pub ledger_revision: u64,
    #[serde(default)]
    pub requirements_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal_hash: Option<String>,
    #[serde(default)]
    pub permission_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<String>,
    pub phase: String,
    #[serde(default)]
    pub profile: UltraplanProfile,
    pub local_only: bool,
    pub read_only: bool,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub shell_policy: ShellPolicy,
    pub mode: PolicyMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<UltraplanManifestSnapshot>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_fidelity: bool,
    #[serde(default)]
    pub execution_cards: Vec<crate::ExecutionCard>,
    #[serde(default)]
    pub hash_drift: Vec<HashDriftRecord>,
}

/// Phases that carry out work rather than plan it: the workflow and
/// ultrawork execution controllers, plus the child turns they spawn.
/// Every other phase comes from [`UltraplanContext::planning_turn`],
/// which is read-only with the shell denied.
const EXECUTION_PHASES: &[&str] = &[
    "workflow",
    "workflow_child",
    "ultrawork_execution",
    "ultrawork_child",
];

impl UltraplanContext {
    /// See [`EXECUTION_PHASES`]. Used to decide whether a turn is
    /// eligible for relaxed auto-mode script review.
    pub fn is_execution_phase(&self) -> bool {
        EXECUTION_PHASES.contains(&self.phase.as_str())
    }

    /// The tools a read-only ultraplan turn is denied on principle: anything
    /// that writes a file the caller named, and anything that runs a command.
    /// Both are classes, so both are asked of the tools rather than listed —
    /// a new editor or a third shell is denied the day it declares its kind.
    /// The caller appends whatever else its phase forbids.
    fn writers_and_shells() -> Vec<String> {
        rebon_tools_core::tool_names_of_kind(rebon_tools_core::ToolKind::FileEdit)
            .into_iter()
            .chain(rebon_tools_core::tool_names_of_kind(
                rebon_tools_core::ToolKind::Shell,
            ))
            .map(str::to_string)
            .collect()
    }

    /// Initial request policy for the main `/ultraplan` planning turn.
    /// Worker prompt policies are intentionally not set here.
    pub fn planning_turn(
        run_id: impl Into<String>,
        phase: impl Into<String>,
        mode: PolicyMode,
    ) -> Self {
        let allowed_tools = vec![
            "Agent".into(),
            "AskUserQuestion".into(),
            "ExitPlanMode".into(),
            "PlanLedger".into(),
            "Read".into(),
            "Glob".into(),
            "Grep".into(),
        ];
        Self {
            run_id: run_id.into(),
            ledger_revision: 0,
            requirements_hash: String::new(),
            seal_hash: None,
            permission_hash: String::new(),
            plan_hash: None,
            phase: phase.into(),
            profile: UltraplanProfile::Standard,
            local_only: true,
            read_only: true,
            allowed_tools,
            denied_tools: Self::writers_and_shells()
                .into_iter()
                .chain(
                    [
                        "TeamCreate",
                        "TeamDelete",
                        "SendMessage",
                        "TaskCreate",
                        "TaskUpdate",
                        "TaskStop",
                        "TodoWrite",
                        "Mcp",
                        "Skill",
                        "CronCreate",
                        "CronDelete",
                    ]
                    .into_iter()
                    .map(str::to_string),
                )
                .collect(),
            shell_policy: ShellPolicy::DenyShell,
            mode,
            manifest: None,
            plan_fidelity: false,
            execution_cards: Vec::new(),
            hash_drift: Vec::new(),
        }
    }

    pub fn workflow_controller_turn(
        run_id: impl Into<String>,
        phase: impl Into<String>,
        mode: PolicyMode,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            ledger_revision: 0,
            requirements_hash: String::new(),
            seal_hash: None,
            permission_hash: String::new(),
            plan_hash: None,
            phase: phase.into(),
            profile: UltraplanProfile::Standard,
            local_only: true,
            read_only: true,
            // Read/Glob/Grep stay allowed so the controller can scout a
            // work-list (files, symbols, diff scope) before authoring the
            // workflow script, instead of orchestrating blind.
            allowed_tools: vec![
                "Workflow".into(),
                "RunWorkflow".into(),
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
            ],
            denied_tools: std::iter::once("Agent".to_string())
                .chain(Self::writers_and_shells())
                .chain(
                    [
                        "TeamCreate",
                        "TeamDelete",
                        "SendMessage",
                        "TaskCreate",
                        "TaskUpdate",
                        "TaskStop",
                        "TodoWrite",
                        "Mcp",
                        "Skill",
                        "CronCreate",
                        "CronDelete",
                        "Sleep",
                        "ToolSearch",
                        "InvokeDeferredTool",
                        "WebSearch",
                        "StructuredOutput",
                    ]
                    .into_iter()
                    .map(str::to_string),
                )
                .collect(),
            shell_policy: ShellPolicy::DenyShell,
            mode,
            manifest: None,
            plan_fidelity: false,
            execution_cards: Vec::new(),
            hash_drift: Vec::new(),
        }
    }

    pub fn ultrawork_execution_controller_turn(
        run_id: impl Into<String>,
        mode: PolicyMode,
    ) -> Self {
        Self::workflow_controller_turn(run_id, "ultrawork_execution", mode).with_plan_fidelity(true)
    }

    pub fn ultrawork_execution_child_turn(&self) -> Option<Self> {
        if !(self.plan_fidelity && self.phase == "ultrawork_execution") {
            return None;
        }
        Some(Self {
            run_id: self.run_id.clone(),
            ledger_revision: self.ledger_revision,
            requirements_hash: self.requirements_hash.clone(),
            seal_hash: self.seal_hash.clone(),
            permission_hash: self.permission_hash.clone(),
            plan_hash: self.plan_hash.clone(),
            phase: "ultrawork_child".into(),
            profile: self.profile,
            local_only: true,
            read_only: false,
            allowed_tools: vec![
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
                "Edit".into(),
                "MultiEdit".into(),
                "Write".into(),
                "Bash".into(),
            ],
            denied_tools: vec![
                "Agent".into(),
                "TeamCreate".into(),
                "TeamDelete".into(),
                "SendMessage".into(),
                "Mcp".into(),
                "Skill".into(),
                "CronCreate".into(),
                "CronDelete".into(),
                "ToolSearch".into(),
                "InvokeDeferredTool".into(),
                "WebSearch".into(),
            ],
            shell_policy: ShellPolicy::AllowShell,
            mode: self.mode,
            manifest: self.manifest.clone(),
            plan_fidelity: true,
            execution_cards: self.execution_cards.clone(),
            hash_drift: self.hash_drift.clone(),
        })
    }

    /// Child policy for workflow agents spawned under a plain workflow
    /// controller (`/ultrawork` without an approved ultraplan). The
    /// controller itself stays a read-only scout/orchestrator, but its
    /// workers do the actual work: they get write + shell like
    /// [`Self::ultrawork_execution_child_turn`], minus plan fidelity —
    /// there is no approved plan or Execution Cards, so workers must be
    /// free to explore the repository.
    pub fn workflow_controller_child_turn(&self) -> Option<Self> {
        if self.plan_fidelity || self.phase != "workflow" {
            return None;
        }
        Some(Self {
            run_id: self.run_id.clone(),
            ledger_revision: self.ledger_revision,
            requirements_hash: self.requirements_hash.clone(),
            seal_hash: self.seal_hash.clone(),
            permission_hash: self.permission_hash.clone(),
            plan_hash: self.plan_hash.clone(),
            phase: "workflow_child".into(),
            profile: self.profile,
            local_only: true,
            read_only: false,
            allowed_tools: vec![
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
                "Edit".into(),
                "MultiEdit".into(),
                "Write".into(),
                "Bash".into(),
            ],
            denied_tools: vec![
                "Agent".into(),
                "TeamCreate".into(),
                "TeamDelete".into(),
                "SendMessage".into(),
                "Mcp".into(),
                "Skill".into(),
                "CronCreate".into(),
                "CronDelete".into(),
                "ToolSearch".into(),
                "InvokeDeferredTool".into(),
                "WebSearch".into(),
            ],
            shell_policy: ShellPolicy::AllowShell,
            mode: self.mode,
            manifest: self.manifest.clone(),
            plan_fidelity: false,
            execution_cards: self.execution_cards.clone(),
            hash_drift: self.hash_drift.clone(),
        })
    }

    pub fn with_profile(mut self, profile: UltraplanProfile) -> Self {
        self.profile = profile;
        self
    }

    pub fn with_run_head(mut self, head: &crate::RunHead) -> Self {
        self.run_id = head.run_id.clone();
        self.ledger_revision = head.ledger_revision;
        self.requirements_hash = head.requirements_hash.clone();
        self.seal_hash = head.seal_hash.clone();
        self.permission_hash = head.permission_hash.clone();
        self.plan_hash = head.plan_hash.clone();
        self
    }

    pub fn with_manifest(mut self, manifest: UltraplanManifestSnapshot) -> Self {
        self.manifest = Some(manifest);
        self
    }

    pub fn with_plan_fidelity(mut self, plan_fidelity: bool) -> Self {
        self.plan_fidelity = plan_fidelity;
        self
    }

    pub fn with_execution_cards(mut self, execution_cards: Vec<crate::ExecutionCard>) -> Self {
        self.execution_cards = execution_cards;
        self
    }

    pub fn with_hash_drift(mut self, hash_drift: Vec<HashDriftRecord>) -> Self {
        self.hash_drift = hash_drift;
        self
    }

    pub fn eager_promotions(&self) -> BTreeSet<String> {
        if self.read_only {
            let mut tools: BTreeSet<String> = ["Agent", "AskUserQuestion", "ExitPlanMode"]
                .into_iter()
                .map(str::to_string)
                .collect();
            if self.allowed_tools.iter().any(|tool| tool == "Workflow") {
                tools.insert("Workflow".to_string());
            }
            if self.allowed_tools.iter().any(|tool| tool == "RunWorkflow") {
                tools.insert("RunWorkflow".to_string());
            }
            tools
        } else {
            BTreeSet::new()
        }
    }
}

/// How much of the agent a session gets.
///
/// The wire spellings are stable: a job record written before `Chat` existed
/// deserializes unchanged, and `Normal` keeps its name on the wire even though
/// the desktop presents it as "work".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCapabilityMode {
    /// Conversation only: no tools at all, and therefore nothing that can
    /// read or write the workspace.
    Chat,
    /// A named, fixed tool surface.
    Minimal,
    #[default]
    Normal,
}

impl AgentCapabilityMode {
    pub fn is_chat(self) -> bool {
        matches!(self, Self::Chat)
    }

    pub fn is_minimal(self) -> bool {
        matches!(self, Self::Minimal)
    }

    pub fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }

    /// The stable wire spelling, matching the serde representation.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Minimal => "minimal",
            Self::Normal => "normal",
        }
    }

    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Self::Chat),
            "minimal" => Some(Self::Minimal),
            "normal" => Some(Self::Normal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    #[default]
    Observe,
    Enforce,
}

impl PolicyMode {
    pub fn is_enforce(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellPolicy {
    AllowShell,
    #[default]
    DenyShell,
}

// ==========================
// Content blocks
// ==========================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TextContent {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageContent {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioContent {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResourceBody {
    pub uri: String,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResourceContent {
    pub resource: ResourceBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResourceLinkContent {
    pub uri: String,
    pub name: String,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

/// MCP-compatible content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ContentBlock {
    Text(TextContent),
    Image(ImageContent),
    Audio(AudioContent),
    Resource(ResourceContent),
    ResourceLink(ResourceLinkContent),
}

// ==========================
// Stop reason
// ==========================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

// ==========================
// Tool-call types
// ==========================

/// Tool kind enum used by `tool_call` updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

/// Tool call lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// File-system anchor that a tool call references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolCallLocation {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

/// Diff content payload — `oldText` may be `null` (when the file did not
/// exist). `old_text` is optional, so the wire form `"oldText": null`
/// round-trips faithfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiffContent {
    pub path: String,
    /// **Always serialized** — `null` rather than omitted when `None`.
    // The only `Option` in the generated types that reaches the wire as
    // `null`. `schemars` makes every `Option` optional and `required` alone
    // would also drop the null from the type, so both halves are spelled out:
    // the generated TypeScript has to say `oldText: string | null`, not an
    // `oldText?: string` that hides the null a client must handle.
    #[cfg_attr(feature = "schema", schemars(required, extend("type" = ["string", "null"])))]
    pub old_text: Option<String>,
    pub new_text: String,
}

/// Terminal handle reference inside a tool-call content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalContent {
    pub terminal_id: String,
}

/// Plain content-block wrapper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RegularContent {
    pub content: ContentBlock,
}

/// Content payload attached to a tool-call update — discriminated by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ToolCallContent {
    Diff(DiffContent),
    Terminal(TerminalContent),
    Content(RegularContent),
}

/// Reference to the tool call a permission request is gating.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolCallReference {
    pub tool_call_id: String,
}

// ==========================
// Plan types
// ==========================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

// ==========================
// Slash commands
// ==========================

/// Category tag for slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SlashCommandCategory {
    /// Built-in CLI command (help, clear, compact, ...).
    Command,
    /// User/bundled skill (/simplify, /commit, ...).
    Skill,
    /// Agent-related command (/agent, /agents, /teams, ...).
    Agent,
}

impl SlashCommandCategory {
    /// Short label used in the picker UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "cmd",
            Self::Skill => "skill",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<SlashCommandInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<SlashCommandCategory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl SlashCommand {
    pub fn matches_name_or_alias(&self, name: &str) -> bool {
        self.name == name || self.aliases.iter().any(|alias| alias == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SlashCommandInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

// ==========================
// Config options
// ==========================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(rename = "type")]
    pub option_type: ConfigOptionType,
    pub current_value: String,
    pub options: Vec<ConfigOptionValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ConfigOptionType {
    Select,
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConfigOptionValue {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub const MODEL_PROFILE_GENERAL: &str = "general";
pub const MODEL_PROFILE_FAST: &str = "fast";
pub const MODEL_PROFILE_SMALL: &str = "small";
pub const MODEL_PROFILE_EXPLORE: &str = "explore";
pub const MODEL_PROFILE_LIBRARIAN: &str = "librarian";
pub const MODEL_PROFILE_BUILDER: &str = "builder";
pub const MODEL_PROFILE_REVIEWER: &str = "reviewer";
pub const MODEL_PROFILE_REASONING: &str = "reasoning";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfileEntry {
    model: String,
    reasoning_effort: Option<String>,
}

impl ModelProfileEntry {
    pub fn new<S, E>(model: S, reasoning_effort: Option<E>) -> Option<Self>
    where
        S: AsRef<str>,
        E: AsRef<str>,
    {
        let model = model.as_ref().trim();
        if model.is_empty() {
            return None;
        }
        let reasoning_effort = reasoning_effort
            .as_ref()
            .map(AsRef::as_ref)
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(str::to_string);
        Some(Self {
            model: model.to_string(),
            reasoning_effort,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }
}

impl Serialize for ModelProfileEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.reasoning_effort.is_none() {
            return serializer.serialize_str(&self.model);
        }
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("reasoningEffort", &self.reasoning_effort)?;
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelProfile {
    pub model: String,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelProfileMap {
    profiles: BTreeMap<String, ModelProfileEntry>,
}

pub type ModelProfileConfig = ModelProfileMap;

impl ModelProfileMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut map = Self::new();
        for (profile, model) in entries {
            map.insert(profile, model);
        }
        map
    }

    fn from_profile_values<I, K>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, ModelProfileValue)>,
        K: AsRef<str>,
    {
        let mut map = Self::new();
        for (profile, value) in entries {
            match value {
                ModelProfileValue::String(model) => map.insert(profile, model),
                ModelProfileValue::Object(object) => {
                    map.insert_with_reasoning_effort(
                        profile,
                        object.model,
                        object.reasoning_effort,
                    );
                }
            }
        }
        map
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn normalize_profile_name(profile: &str) -> String {
        profile.trim().to_ascii_lowercase().replace('_', "-")
    }

    pub fn insert(&mut self, profile: impl AsRef<str>, model: impl AsRef<str>) {
        self.insert_with_reasoning_effort(profile, model, None::<&str>);
    }

    pub fn insert_with_reasoning_effort<E>(
        &mut self,
        profile: impl AsRef<str>,
        model: impl AsRef<str>,
        reasoning_effort: Option<E>,
    ) where
        E: AsRef<str>,
    {
        let profile = Self::normalize_profile_name(profile.as_ref());
        let Some(entry) = ModelProfileEntry::new(model, reasoning_effort) else {
            return;
        };
        if profile.is_empty() {
            return;
        }
        self.profiles.insert(profile, entry);
    }

    /// Drop a profile, returning whether one was there.
    ///
    /// Removing is how a profile is set back to "follow the session's model":
    /// an undeclared profile resolves to the running model, so there is no
    /// sentinel value to store for it.
    pub fn remove(&mut self, profile: &str) -> bool {
        self.profiles
            .remove(&Self::normalize_profile_name(profile))
            .is_some()
    }

    pub fn get(&self, profile: &str) -> Option<&str> {
        self.profiles
            .get(&Self::normalize_profile_name(profile))
            .map(ModelProfileEntry::model)
    }

    pub fn get_reasoning_effort(&self, profile: &str) -> Option<&str> {
        self.profiles
            .get(&Self::normalize_profile_name(profile))
            .and_then(ModelProfileEntry::reasoning_effort)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.profiles
            .iter()
            .map(|(profile, entry)| (profile.as_str(), entry.model()))
    }

    pub fn as_map(&self) -> &BTreeMap<String, ModelProfileEntry> {
        &self.profiles
    }

    /// Adopt every profile `defaults` declares that this map does not.
    ///
    /// Used to layer a plugin manifest's profiles under the user's
    /// `config.json` ones. Merging per key, not per map: a user who overrides
    /// one profile is overriding *that* profile, and swapping the whole table
    /// silently dropped the cheap-tier slot a provider plugin had shipped.
    pub fn fill_missing_from(&mut self, defaults: &ModelProfileMap) {
        for (profile, entry) in &defaults.profiles {
            self.profiles
                .entry(profile.clone())
                .or_insert_with(|| entry.clone());
        }
    }

    pub fn resolve_model(
        &self,
        profile: &str,
        provider_model: Option<&str>,
        runtime_model: &str,
    ) -> String {
        self.resolve_profile_selection(profile, provider_model, runtime_model)
            .model
    }

    /// Resolve a profile to a concrete model.
    ///
    /// A profile the map declares wins. Everything else follows the model the
    /// session is *actually running*: `runtime_model` is the answer, and the
    /// provider entry's own `model` is only a last resort for callers that
    /// have no runtime to speak of. The other order — provider entry first —
    /// meant a session that had explicitly picked a cheap model still had its
    /// titles, auto-mode classifications, background summaries and compactions
    /// billed against whatever the provider entry happened to name.
    pub fn resolve_profile_selection(
        &self,
        profile: &str,
        provider_model: Option<&str>,
        runtime_model: &str,
    ) -> ResolvedModelProfile {
        if let Some(entry) = self.resolve_profile_entry(profile) {
            return ResolvedModelProfile {
                model: entry.model.clone(),
                reasoning_effort: entry.reasoning_effort.clone(),
            };
        }
        let runtime_model = runtime_model.trim();
        let model = (!runtime_model.is_empty())
            .then_some(runtime_model)
            .or_else(|| {
                provider_model
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
            })
            .unwrap_or_default()
            .to_string();
        ResolvedModelProfile {
            model,
            reasoning_effort: None,
        }
    }

    pub fn resolve_profile(&self, profile: &str) -> Option<&str> {
        self.resolve_profile_entry(profile)
            .map(ModelProfileEntry::model)
    }

    pub fn resolve_profile_entry(&self, profile: &str) -> Option<&ModelProfileEntry> {
        let normalized = Self::normalize_profile_name(profile);
        if !normalized.is_empty() {
            if let Some(entry) = self.get_normalized_entry(&normalized) {
                return Some(entry);
            }
        }
        for fallback in Self::fallback_chain(&normalized) {
            if fallback == normalized {
                continue;
            }
            if let Some(entry) = self.get_normalized_entry(fallback) {
                return Some(entry);
            }
        }
        None
    }

    /// Which declared profiles an undeclared one may borrow from.
    ///
    /// Only *neighbours* — a cheap tier borrows the cheap tier next to it, a
    /// reviewer borrows the reasoning slot. Nothing borrows `general`.
    /// `general` is not a tier at all: it names the session's main model, and
    /// the session already knows what that is. Routing every undeclared
    /// profile through it turned one declared `general` into a silent
    /// override of the model the user had selected — a provider that declared
    /// `general: <expensive>` billed its titles, classifier calls and
    /// summaries there even while the session ran a cheap model.
    pub fn fallback_chain(profile: &str) -> Vec<&'static str> {
        match Self::normalize_profile_name(profile).as_str() {
            MODEL_PROFILE_GENERAL => vec![MODEL_PROFILE_GENERAL],
            MODEL_PROFILE_FAST => vec![MODEL_PROFILE_FAST, MODEL_PROFILE_SMALL],
            MODEL_PROFILE_SMALL => vec![MODEL_PROFILE_SMALL],
            MODEL_PROFILE_EXPLORE => vec![MODEL_PROFILE_EXPLORE, MODEL_PROFILE_SMALL],
            MODEL_PROFILE_LIBRARIAN => vec![MODEL_PROFILE_LIBRARIAN, MODEL_PROFILE_SMALL],
            MODEL_PROFILE_BUILDER => vec![MODEL_PROFILE_BUILDER],
            MODEL_PROFILE_REVIEWER => vec![MODEL_PROFILE_REVIEWER, MODEL_PROFILE_REASONING],
            MODEL_PROFILE_REASONING => vec![MODEL_PROFILE_REASONING],
            _ => Vec::new(),
        }
    }

    fn get_normalized_entry(&self, profile: &str) -> Option<&ModelProfileEntry> {
        self.profiles.get(profile)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ModelProfileValue {
    String(String),
    Object(ModelProfileObject),
}

#[derive(Debug, Clone, Deserialize)]
struct ModelProfileObject {
    #[serde(default)]
    model: String,
    #[serde(
        default,
        rename = "reasoningEffort",
        alias = "reasoning_effort",
        alias = "effort",
        alias = "variant"
    )]
    reasoning_effort: Option<String>,
}

impl Serialize for ModelProfileMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.profiles.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ModelProfileMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = BTreeMap::<String, ModelProfileValue>::deserialize(deserializer)?;
        Ok(Self::from_profile_values(raw))
    }
}

#[cfg(test)]
mod ultraplan_policy_tests {
    use super::*;

    fn has_tool(tools: &[String], name: &str) -> bool {
        tools.iter().any(|tool| tool == name)
    }

    #[test]
    fn ultrawork_execution_controller_preserves_workflow_only_handoff_policy() {
        let policy = ExecutionPolicy::ultrawork_execution_controller("run-1");
        let context = policy.ultraplan.as_ref().expect("ultraplan context");

        assert_eq!(context.run_id, "run-1");
        assert_eq!(context.phase, "ultrawork_execution");
        assert!(context.local_only);
        assert!(context.plan_fidelity);
        assert!(context.read_only);
        assert_eq!(context.shell_policy, ShellPolicy::DenyShell);
        assert!(has_tool(&context.allowed_tools, "Workflow"));
        assert!(has_tool(&context.allowed_tools, "RunWorkflow"));
        assert!(has_tool(&context.denied_tools, "Edit"));
        assert!(has_tool(&context.denied_tools, "Write"));
        assert!(has_tool(&context.denied_tools, "Bash"));
        assert!(policy.eager_promotions.contains("Workflow"));
        assert!(policy.eager_promotions.contains("RunWorkflow"));
    }

    #[test]
    fn ultrawork_execution_child_helper_is_writable_but_local_only() {
        let parent = ExecutionPolicy::ultrawork_execution_controller("run-1")
            .ultraplan
            .expect("ultraplan context");
        let child = parent
            .ultrawork_execution_child_turn()
            .expect("ultrawork child policy");

        assert_eq!(child.run_id, "run-1");
        assert_eq!(child.phase, "ultrawork_child");
        assert!(child.local_only);
        assert!(child.plan_fidelity);
        assert!(!child.read_only);
        assert_eq!(child.shell_policy, ShellPolicy::AllowShell);
        assert!(has_tool(&child.allowed_tools, "Edit"));
        assert!(has_tool(&child.allowed_tools, "Write"));
        assert!(has_tool(&child.allowed_tools, "Bash"));
        assert!(!has_tool(&child.denied_tools, "Edit"));
        assert!(!has_tool(&child.denied_tools, "Write"));
        assert!(!has_tool(&child.denied_tools, "Bash"));
        assert!(has_tool(&child.denied_tools, "Agent"));
        assert!(has_tool(&child.denied_tools, "TeamCreate"));
        assert!(has_tool(&child.denied_tools, "TeamDelete"));
        assert!(has_tool(&child.denied_tools, "SendMessage"));
    }
}

#[cfg(test)]
mod model_profile_tests {
    use super::*;

    #[test]
    fn model_profile_map_normalizes_keys_and_skips_blank_values() {
        let map = ModelProfileMap::from_entries([
            (" Explore ", " gpt-mini "),
            ("SMALL", "gpt-nano"),
            ("blank", " "),
        ]);

        assert_eq!(map.get("explore"), Some("gpt-mini"));
        assert_eq!(map.get("small"), Some("gpt-nano"));
        assert_eq!(map.get("blank"), None);
    }

    #[test]
    fn model_profile_map_resolves_builtin_fallback_chain() {
        let map = ModelProfileMap::from_entries([
            ("general", "gpt-main"),
            ("small", "gpt-nano"),
            ("reasoning", "gpt-reasoning"),
        ]);

        assert_eq!(
            map.resolve_model("fast", Some("provider-main"), "runtime-main"),
            "gpt-nano"
        );
        assert_eq!(
            map.resolve_model("explore", Some("provider-main"), "runtime-main"),
            "gpt-nano"
        );
        assert_eq!(
            map.resolve_model("librarian", Some("provider-main"), "runtime-main"),
            "gpt-nano"
        );
        assert_eq!(
            map.resolve_model("reviewer", Some("provider-main"), "runtime-main"),
            "gpt-reasoning"
        );
    }

    #[test]
    fn model_profile_map_parses_object_entries_with_reasoning_effort() {
        let map: ModelProfileMap = serde_json::from_value(serde_json::json!({
            "explore": {
                "model": "gpt-mini",
                "reasoningEffort": "low"
            },
            "reasoning": {
                "model": "gpt-main",
                "effort": "xhigh"
            }
        }))
        .unwrap();

        assert_eq!(map.get("explore"), Some("gpt-mini"));
        assert_eq!(map.get_reasoning_effort("explore"), Some("low"));
        let resolved = map.resolve_profile_selection("reasoning", Some("provider"), "runtime");
        assert_eq!(resolved.model, "gpt-main");
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn model_profile_map_falls_back_to_the_runtime_model_over_the_provider_entry() {
        let map = ModelProfileMap::new();

        // The session's model wins: a session that switched to a cheap model
        // must not have its side-requests billed against whatever the
        // provider entry names.
        assert_eq!(
            map.resolve_model("small", Some("provider-main"), "runtime-main"),
            "runtime-main"
        );
        // The provider entry is only there for callers with no live runtime.
        assert_eq!(
            map.resolve_model("small", Some("provider-main"), "  "),
            "provider-main"
        );
    }

    #[test]
    fn an_undeclared_profile_never_borrows_the_general_slot() {
        // The exact shape that leaked: `general` declared as the expensive
        // model, session running the cheap one, nothing else declared.
        let map = ModelProfileMap::from_entries([("general", "big-pro")]);

        for profile in [
            "small",
            "fast",
            "explore",
            "librarian",
            "builder",
            "reviewer",
            "reasoning",
        ] {
            assert_eq!(
                map.resolve_model(profile, Some("big-pro"), "cheap-flash"),
                "cheap-flash",
                "`{profile}` must follow the session model, not the general slot"
            );
        }
        // Asking for `general` itself still honours the declaration.
        assert_eq!(
            map.resolve_model("general", Some("big-pro"), "cheap-flash"),
            "big-pro"
        );
    }

    #[test]
    fn neighbour_fallbacks_still_apply_within_a_tier() {
        let map =
            ModelProfileMap::from_entries([("small", "gpt-nano"), ("reasoning", "gpt-reasoning")]);

        for profile in ["fast", "explore", "librarian"] {
            assert_eq!(
                map.resolve_model(profile, Some("provider-main"), "runtime-main"),
                "gpt-nano"
            );
        }
        assert_eq!(
            map.resolve_model("reviewer", Some("provider-main"), "runtime-main"),
            "gpt-reasoning"
        );
        // `builder` has no neighbour, so it follows the session.
        assert_eq!(
            map.resolve_model("builder", Some("provider-main"), "runtime-main"),
            "runtime-main"
        );
    }

    #[test]
    fn fill_missing_from_layers_defaults_under_declared_profiles() {
        // A plugin manifest ships `small`; the user only overrode `general`.
        let mut user = ModelProfileMap::from_entries([("general", "big-pro")]);
        let manifest = ModelProfileMap::from_entries([
            ("small", "cheap-flash"),
            ("general", "manifest-default"),
        ]);

        user.fill_missing_from(&manifest);

        assert_eq!(
            user.get("small"),
            Some("cheap-flash"),
            "manifest slot survives"
        );
        assert_eq!(user.get("general"), Some("big-pro"), "user override wins");
    }
}

// ==========================
// Session update notifications (agent -> client)
// ==========================

/// Which part of the auto-mode gate let a tool call run without a dialog.
///
/// Display-only, and worth distinguishing because the three are not the same
/// event: a classifier verdict is the machine's judgement, a cached verdict is
/// that judgement replayed, and an exemption is the *user's* own approval — a
/// row that says "auto mode allowed this" over an approval the user gave by
/// hand misattributes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum AutoModeAllowSource {
    /// A fresh classifier verdict cleared the call.
    Classifier,
    /// A classifier verdict remembered from an identical earlier call.
    CachedVerdict,
    /// The user approved this call themselves (a `/permissions` retry).
    UserExemption,
    /// Source not recorded — a transcript or a peer written before the
    /// source was carried. Renders as the original unattributed note.
    #[default]
    #[serde(other)]
    Unspecified,
}

/// Discriminated union of `session/update` notification payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SessionUpdate {
    /// Streaming text content from the model.
    AgentMessageChunk { content: ContentBlock },

    /// User input that was queued while a prompt was already running and then
    /// consumed between tool rounds.
    #[serde(rename_all = "camelCase")]
    QueuedUserMessage {
        uuid: String,
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_paste_ids: Option<Vec<u32>>,
    },

    /// Streaming extended-thinking delta.
    ThinkingDelta { text: String },

    /// End of thinking stream for the current turn.
    ThinkingEnd,

    /// Initial tool-call announcement.
    #[serde(rename_all = "camelCase")]
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: ToolKind,
        status: ToolCallStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ToolCallContent>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<Vec<ToolCallLocation>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_input: Option<HashMap<String, Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_output: Option<HashMap<String, Value>>,
    },

    /// Follow-up tool-call status/content transitions.
    #[serde(rename_all = "camelCase")]
    ToolCallUpdate {
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ToolCallStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ToolCallContent>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<Vec<ToolCallLocation>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_output: Option<HashMap<String, Value>>,
    },

    /// A tool call cleared rebon's auto-mode gate without a permission
    /// dialog.
    ///
    /// Purely an annotation for the tool row that is already on screen: it
    /// carries no content, and clients must not fold it into anything the
    /// model reads back.
    #[serde(rename_all = "camelCase")]
    ToolCallAutoModeAllowed {
        tool_call_id: String,
        /// Defaulted so a peer that predates the field still decodes — it
        /// lands as `Unspecified` and renders the unattributed note.
        #[serde(default)]
        source: AutoModeAllowSource,
    },

    /// Context compaction has started.
    CompactingStarted { messages_before: usize },

    /// Context compaction finished.
    CompactingDone {
        messages_after: usize,
        used_model: bool,
    },

    /// Plan entries.
    Plan { entries: Vec<PlanEntry> },

    /// Slash commands list.
    SlashCommands { commands: Vec<SlashCommand> },

    /// Config option update.
    #[serde(rename_all = "camelCase")]
    ConfigOptionUpdate { config_options: Vec<ConfigOption> },

    /// Session-local context was reset and the engine is continuing
    /// with a fresh conversation (e.g. approved clear-context plan).
    /// `plan` carries the plan text so the client can render it as a
    /// system message in the cleared transcript — without it the user
    /// sees a blank screen even though the model is executing the plan.
    #[serde(rename_all = "camelCase")]
    ContextReset {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<String>,
    },

    /// Session info update (title, timestamp).
    #[serde(rename_all = "camelCase")]
    SessionInfoUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_at: Option<String>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<HashMap<String, Value>>,
    },

    /// Token usage reported by the model during streaming.
    /// Emitted as snapshots arrive from the provider or as the engine
    /// approximates streamed output so the client can display progress.
    #[serde(rename_all = "camelCase")]
    TokenUsage {
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        input_tokens: u32,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        output_tokens: u32,
    },
}

/// Outer envelope for the `session/update` notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionUpdateParams {
    pub session_id: SessionId,
    pub update: SessionUpdate,
}
