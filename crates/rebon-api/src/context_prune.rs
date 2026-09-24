//! Context-pruning [`ModelClient`] middleware.
//!
//! Reduces token waste by transforming the `messages` array before it
//! reaches the provider, without modifying the caller's copy.  Five
//! strategies are applied in order:
//!
//! 0. **Thinking-block stripping** — old assistant messages lose their
//!    `Thinking` blocks entirely.
//! 1. **Tool-result clearing** — old `ToolResult` content is replaced
//!    with a short placeholder.
//! 2. **Deduplication** — consecutive identical tool calls (same name +
//!    input) are collapsed, keeping only the most recent result.
//! 3. **Error-input purging** — the `input` field of old failed
//!    tool-use blocks is replaced with `{}` so the model doesn't waste
//!    tokens re-reading parameters it already knows failed.
//! 4. **Auto-compact truncation** — when the last-reported input token
//!    count approaches the context window, the oldest messages beyond
//!    a protected tail are dropped entirely.
//!
//! The middleware never touches the caller's `Vec<Message>` — it
//! clones + prunes only for the outbound request, so conversation
//! replay and transcript persistence stay intact. Providers that use
//! server-side continuation must verify their own request baseline
//! before reusing that continuation and fall back to a full replay on
//! mismatch.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::client::{ModelCapabilities, ModelClient};
use crate::error::ModelResult;
use crate::events::StreamEventStream;
use crate::request::CreateMessageRequest;
use crate::types::{ContentBlock, Message, Role, TextBlock, ToolResultBlock};

// ── Cleared-content placeholder ─────────────────────────────────

/// Placeholder injected when a tool-result's content is cleared.
pub const TOOL_RESULT_CLEARED: &str = "[Old tool result content cleared]";

// ── PruneLevel ──────────────────────────────────────────────────

/// How aggressively the middleware prunes context.
///
/// Stored as `AtomicU8` so the TUI can toggle it at runtime without
/// rebuilding the middleware stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PruneLevel {
    /// No pruning — pass messages through unchanged.
    Off = 0,
    /// Clear tool results older than `tool_result_max_age` turns.
    /// Safe default that never removes structural content.
    Conservative = 1,
    /// Conservative + deduplication + error-input purging.
    Aggressive = 2,
}

impl PruneLevel {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Off,
            1 => Self::Conservative,
            _ => Self::Aggressive,
        }
    }

    /// Parse from the config option value string used in `/settings`.
    pub fn from_config_value(s: &str) -> Self {
        match s {
            "off" => Self::Off,
            "conservative" => Self::Conservative,
            "aggressive" => Self::Aggressive,
            _ => Self::Conservative,
        }
    }

    /// The config option value string for serialisation.
    pub fn as_config_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Conservative => "conservative",
            Self::Aggressive => "aggressive",
        }
    }
}

// ── ContextPruneConfig ──────────────────────────────────────────

/// Tuning knobs for context pruning.
#[derive(Debug, Clone)]
pub struct ContextPruneConfig {
    /// Number of recent message pairs (user+assistant) to leave
    /// untouched. Everything older is eligible for pruning.
    pub protected_recent_turns: usize,
    /// Number of turns after which tool-result content is cleared.
    /// Only applies at [`PruneLevel::Conservative`] and above.
    pub tool_result_max_age_turns: usize,
    /// Number of turns after which failed tool inputs are stripped.
    /// Only applies at [`PruneLevel::Aggressive`].
    pub error_purge_age_turns: usize,
}

impl ContextPruneConfig {
    pub fn full_history_replay() -> Self {
        Self {
            protected_recent_turns: 2,
            tool_result_max_age_turns: 4,
            error_purge_age_turns: 2,
        }
    }
}

impl Default for ContextPruneConfig {
    fn default() -> Self {
        Self {
            protected_recent_turns: 4,
            // 8 turns (was 4) so typical Read→think→Edit flows that
            // span ~6–10 turns keep the Read result intact. Combined
            // with `protected_recent_turns=4`, the effective preserved
            // window is 12 turns of tool_results.
            tool_result_max_age_turns: 8,
            error_purge_age_turns: 4,
        }
    }
}

// ── PruneStats ──────────────────────────────────────────────────

/// Cumulative statistics updated by the middleware on every request.
/// All fields are atomic so the TUI can read them without locking.
#[derive(Debug, Default)]
pub struct PruneStats {
    /// Total tool-result blocks whose content was cleared.
    pub tool_results_cleared: AtomicU32,
    /// Total duplicate tool-call pairs removed.
    pub duplicates_removed: AtomicU32,
    /// Total error inputs purged.
    pub errors_purged: AtomicU32,
    /// Total thinking blocks stripped from old messages.
    pub thinking_cleared: AtomicU32,
    /// Total messages removed by truncation (auto-compact).
    pub messages_truncated: AtomicU32,
    /// Total requests processed by the middleware.
    pub requests_processed: AtomicU32,
    /// Last reported input token count from the API.
    pub last_input_tokens: AtomicU32,
}

impl PruneStats {
    pub fn snapshot(&self) -> PruneStatsSnapshot {
        PruneStatsSnapshot {
            tool_results_cleared: self.tool_results_cleared.load(Ordering::Relaxed),
            duplicates_removed: self.duplicates_removed.load(Ordering::Relaxed),
            errors_purged: self.errors_purged.load(Ordering::Relaxed),
            thinking_cleared: self.thinking_cleared.load(Ordering::Relaxed),
            messages_truncated: self.messages_truncated.load(Ordering::Relaxed),
            requests_processed: self.requests_processed.load(Ordering::Relaxed),
            last_input_tokens: self.last_input_tokens.load(Ordering::Relaxed),
        }
    }

    pub fn reset(&self) {
        self.tool_results_cleared.store(0, Ordering::Relaxed);
        self.duplicates_removed.store(0, Ordering::Relaxed);
        self.errors_purged.store(0, Ordering::Relaxed);
        self.thinking_cleared.store(0, Ordering::Relaxed);
        self.messages_truncated.store(0, Ordering::Relaxed);
        self.requests_processed.store(0, Ordering::Relaxed);
        self.last_input_tokens.store(0, Ordering::Relaxed);
    }
}

/// Non-atomic snapshot for display.
#[derive(Debug, Clone, Copy, Default)]
pub struct PruneStatsSnapshot {
    pub tool_results_cleared: u32,
    pub duplicates_removed: u32,
    pub errors_purged: u32,
    pub thinking_cleared: u32,
    pub messages_truncated: u32,
    pub requests_processed: u32,
    pub last_input_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextUsageSource {
    Unknown,
    Server,
    Estimated,
}

impl ContextUsageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextUsageSource::Unknown => "unknown",
            ContextUsageSource::Server => "server",
            ContextUsageSource::Estimated => "estimated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageSnapshot {
    pub tokens: u32,
    pub source: ContextUsageSource,
}

impl std::fmt::Display for PruneStatsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Requests: {} | Tool results cleared: {} | Duplicates removed: {} | \
             Error inputs purged: {} | Thinking cleared: {} | Messages truncated: {}",
            self.requests_processed,
            self.tool_results_cleared,
            self.duplicates_removed,
            self.errors_purged,
            self.thinking_cleared,
            self.messages_truncated,
        )
    }
}

// ── ContextBudget ───────────────────────────────────────────────

/// Default context window assumed when none is reported.
const DEFAULT_CONTEXT_WINDOW: u32 = 1_000_000;
/// Reserve tokens for compaction summary output (p99.99 = 17,387).
const RESERVED_FOR_SUMMARY: u32 = 20_000;
/// Auto-compact fires at 95% of the input budget remaining after the
/// model's maximum output reservation is removed from its total window.
const AUTOCOMPACT_TRIGGER_PCT: u32 = 95;

fn auto_compact_threshold_for_window(context_window: u32, output_token_reserve: u32) -> u32 {
    context_window
        .saturating_sub(output_token_reserve)
        .saturating_mul(AUTOCOMPACT_TRIGGER_PCT)
        / 100
}
const WARNING_THRESHOLD_BUFFER: u32 = 20_000;

/// Absolute ceiling on the auto-compact trigger, matching Codex: its model
/// catalogue advertises a 272k working window even for 1M-capable models and
/// compacts at 90% of it. A long session otherwise resends several hundred
/// thousand tokens on every tool round before the window-relative trigger
/// fires, and cached input still counts against the account's quota.
/// Callers opt in through [`ContextBudget::set_auto_compact_token_limit`].
pub const DEFAULT_AUTO_COMPACT_TOKEN_LIMIT: u32 = 272_000 * 9 / 10;

// ── Microcompact thresholds ────────────────────────────────────
// Microcompact trims stale tool results before heavier summarization.
// The default is window-relative so large-context providers can use their
// advertised window. Full-history replay ports can opt into absolute caps,
// because each tool round resends the whole messages array and stale tool
// results become a recurring input-token floor.

/// Microcompact fires when input tokens exceed this fraction of the
/// effective context window (context_window − RESERVED_FOR_SUMMARY), unless
/// the active [`ContextBudget`] has a lower provider-specific cap.
const MICROCOMPACT_TRIGGER_PCT: u32 = 70; // 70%
/// After microcompact, try to get usage below this fraction, unless the
/// active [`ContextBudget`] has a lower provider-specific cap.
const MICROCOMPACT_TARGET_PCT: u32 = 50; // 50%
const FULL_HISTORY_REPLAY_MICROCOMPACT_TRIGGER_MAX_TOKENS: u32 = 48_000;
const FULL_HISTORY_REPLAY_MICROCOMPACT_TARGET_MAX_TOKENS: u32 = 32_000;

/// Context-window budget tracking and auto-compact threshold.
///
/// The TUI reports `input_tokens` from each API response via
/// [`ContextBudget::report_usage`]. The middleware reads the budget
/// on each request to decide whether auto-compact truncation is
/// needed.
///
/// `/compact` sets a one-shot flag via [`ContextBudget::force_compact_once`].
/// The middleware checks and clears this flag atomically so the
/// truncation fires exactly once.
/// Max consecutive auto-compact failures before the circuit breaker
/// trips and skips further attempts.
const MAX_CONSECUTIVE_COMPACT_FAILURES: u32 = 3;

#[derive(Debug)]
pub struct ContextBudget {
    /// Total context window size (tokens).
    context_window: AtomicU32,
    /// Tokens reserved for the model's maximum output.
    output_token_reserve: AtomicU32,
    /// Auto-compact trigger = min((context_window − output reserve) ×
    /// trigger percentage, `auto_compact_token_limit`).
    auto_compact_threshold: AtomicU32,
    /// Absolute cap on the auto-compact trigger. `u32::MAX` means none.
    auto_compact_token_limit: AtomicU32,
    /// Provider-specific cap for the microcompact trigger. `u32::MAX`
    /// means the window-relative threshold is used unchanged.
    microcompact_trigger_max_tokens: AtomicU32,
    /// Provider-specific cap for the microcompact target. `u32::MAX`
    /// means the window-relative target is used unchanged.
    microcompact_target_max_tokens: AtomicU32,
    /// Last reported input token count.
    last_input_tokens: AtomicU32,
    /// Source of [`Self::last_input_tokens`].
    last_usage_source: AtomicU8, // 0=unknown, 1=server, 2=estimated
    /// Whether auto-compact is enabled.
    auto_compact_enabled: AtomicU8, // 0=disabled, 1=enabled
    /// One-shot flag: set by `/compact`, cleared after one request.
    force_compact_once: AtomicU8, // 0=no, 1=yes
    /// Optional one-shot instructions attached to a manual `/compact`.
    manual_compact_instructions: Mutex<Option<String>>,
    /// Consecutive auto-compact failures. When this reaches
    /// `MAX_CONSECUTIVE_COMPACT_FAILURES`, `should_auto_compact()`
    /// returns false to avoid hammering a failing compact path.
    /// Reset to 0 on any successful compact.
    consecutive_compact_failures: AtomicU32,
}

impl ContextBudget {
    pub fn new(context_window: u32) -> Self {
        Self::with_output_reserve(context_window, 0)
    }

    pub fn with_output_reserve(context_window: u32, output_token_reserve: u32) -> Self {
        let threshold = auto_compact_threshold_for_window(context_window, output_token_reserve);
        Self {
            context_window: AtomicU32::new(context_window),
            output_token_reserve: AtomicU32::new(output_token_reserve),
            auto_compact_threshold: AtomicU32::new(threshold),
            auto_compact_token_limit: AtomicU32::new(u32::MAX),
            microcompact_trigger_max_tokens: AtomicU32::new(u32::MAX),
            microcompact_target_max_tokens: AtomicU32::new(u32::MAX),
            last_input_tokens: AtomicU32::new(0),
            last_usage_source: AtomicU8::new(0),
            auto_compact_enabled: AtomicU8::new(1),
            force_compact_once: AtomicU8::new(0),
            manual_compact_instructions: Mutex::new(None),
            consecutive_compact_failures: AtomicU32::new(0),
        }
    }

    /// Report token usage from the latest API response.
    pub fn report_usage(&self, input_tokens: u32) {
        self.last_input_tokens
            .store(input_tokens, Ordering::Relaxed);
        self.last_usage_source.store(1, Ordering::Relaxed);
    }

    /// Clear reported usage after an explicit session reset.
    pub fn clear_usage(&self) {
        self.last_input_tokens.store(0, Ordering::Relaxed);
        self.last_usage_source.store(0, Ordering::Relaxed);
    }

    /// Report locally estimated token usage for the next request.
    pub fn report_estimated_usage(&self, input_tokens: u32) {
        self.last_input_tokens
            .store(input_tokens, Ordering::Relaxed);
        self.last_usage_source.store(2, Ordering::Relaxed);
    }

    pub fn usage_snapshot(&self) -> ContextUsageSnapshot {
        let source = match self.last_usage_source.load(Ordering::Relaxed) {
            1 => ContextUsageSource::Server,
            2 => ContextUsageSource::Estimated,
            _ => ContextUsageSource::Unknown,
        };
        ContextUsageSnapshot {
            tokens: self.last_input_tokens.load(Ordering::Relaxed),
            source,
        }
    }

    /// Update the total context window while preserving the configured output reserve.
    pub fn set_context_window(&self, window: u32) {
        self.set_context_limits(window, self.output_token_reserve());
    }

    pub fn set_context_limits(&self, window: u32, output_token_reserve: u32) {
        self.context_window.store(window, Ordering::Relaxed);
        self.output_token_reserve
            .store(output_token_reserve, Ordering::Relaxed);
        self.recompute_auto_compact_threshold();
    }

    /// Cap the auto-compact trigger at `limit` tokens regardless of how
    /// large the window is. `None` (or `u32::MAX`) removes the cap.
    pub fn set_auto_compact_token_limit(&self, limit: Option<u32>) {
        self.auto_compact_token_limit
            .store(limit.unwrap_or(u32::MAX), Ordering::Relaxed);
        self.recompute_auto_compact_threshold();
    }

    /// The absolute auto-compact cap, if one is set.
    pub fn auto_compact_token_limit(&self) -> Option<u32> {
        match self.auto_compact_token_limit.load(Ordering::Relaxed) {
            u32::MAX => None,
            limit => Some(limit),
        }
    }

    fn recompute_auto_compact_threshold(&self) {
        let threshold =
            auto_compact_threshold_for_window(self.context_window(), self.output_token_reserve())
                .min(self.auto_compact_token_limit.load(Ordering::Relaxed));
        self.auto_compact_threshold
            .store(threshold, Ordering::Relaxed);
    }

    pub fn set_auto_compact_enabled(&self, enabled: bool) {
        self.auto_compact_enabled
            .store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
    }

    pub fn context_window(&self) -> u32 {
        self.context_window.load(Ordering::Relaxed)
    }

    pub fn output_token_reserve(&self) -> u32 {
        self.output_token_reserve.load(Ordering::Relaxed)
    }

    pub fn input_token_budget(&self) -> u32 {
        self.context_window()
            .saturating_sub(self.output_token_reserve())
    }

    pub fn set_microcompact_caps(&self, trigger_max_tokens: u32, target_max_tokens: u32) {
        self.microcompact_trigger_max_tokens
            .store(trigger_max_tokens, Ordering::Relaxed);
        self.microcompact_target_max_tokens
            .store(target_max_tokens, Ordering::Relaxed);
    }

    fn microcompact_caps(&self) -> (u32, u32) {
        (
            self.microcompact_trigger_max_tokens.load(Ordering::Relaxed),
            self.microcompact_target_max_tokens.load(Ordering::Relaxed),
        )
    }

    pub fn use_full_history_replay_microcompact_profile(&self) {
        self.set_microcompact_caps(
            FULL_HISTORY_REPLAY_MICROCOMPACT_TRIGGER_MAX_TOKENS,
            FULL_HISTORY_REPLAY_MICROCOMPACT_TARGET_MAX_TOKENS,
        );
    }

    pub fn auto_compact_threshold(&self) -> u32 {
        self.auto_compact_threshold.load(Ordering::Relaxed)
    }

    pub fn last_input_tokens(&self) -> u32 {
        self.last_input_tokens.load(Ordering::Relaxed)
    }

    /// Set a one-shot flag that causes the next request to trigger
    /// auto-compact truncation regardless of token usage. The flag
    /// is cleared atomically by [`Self::take_compact_once`].
    pub fn force_compact_once(&self) {
        self.force_compact_once_with_instructions(None);
    }

    /// Set a one-shot compact flag with optional custom instructions
    /// for this manual compaction only.
    pub fn force_compact_once_with_instructions(&self, instructions: Option<String>) {
        let instructions = instructions.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        *self
            .manual_compact_instructions
            .lock()
            .expect("manual compact instructions mutex poisoned") = instructions;
        self.force_compact_once.store(1, Ordering::Release);
    }

    /// Check and atomically clear the one-shot compact flag.
    /// Returns `true` exactly once after [`Self::force_compact_once`].
    pub fn take_compact_once(&self) -> bool {
        self.take_compact_once_with_instructions().0
    }

    /// Check and atomically clear the one-shot compact flag, returning
    /// any manual instructions attached to that one compact request.
    pub fn take_compact_once_with_instructions(&self) -> (bool, Option<String>) {
        if self.force_compact_once.swap(0, Ordering::AcqRel) == 0 {
            return (false, None);
        }
        let instructions = self
            .manual_compact_instructions
            .lock()
            .expect("manual compact instructions mutex poisoned")
            .take();
        (true, instructions)
    }

    /// Whether the current token usage exceeds the auto-compact
    /// threshold. Does NOT consume the one-shot flag — call
    /// [`Self::take_compact_once`] separately for `/compact`.
    ///
    /// Returns `false` when the circuit breaker has tripped
    /// (`consecutive_compact_failures >= MAX`), preventing
    /// futile retries.
    pub fn should_auto_compact(&self) -> bool {
        self.should_auto_compact_for_tokens(self.last_input_tokens.load(Ordering::Relaxed))
    }

    /// Whether the supplied token count exceeds the auto-compact
    /// threshold while respecting the same enable flag and circuit
    /// breaker as [`Self::should_auto_compact`].
    pub fn should_auto_compact_for_tokens(&self, input_tokens: u32) -> bool {
        self.auto_compact_enabled.load(Ordering::Relaxed) != 0
            && self.consecutive_compact_failures.load(Ordering::Relaxed)
                < MAX_CONSECUTIVE_COMPACT_FAILURES
            && input_tokens >= self.auto_compact_threshold.load(Ordering::Relaxed)
    }

    /// Record a compact failure. After `MAX_CONSECUTIVE_COMPACT_FAILURES`,
    /// `should_auto_compact()` returns false.
    pub fn record_compact_failure(&self) {
        self.consecutive_compact_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a successful compact. Resets the circuit breaker.
    pub fn record_compact_success(&self) {
        self.consecutive_compact_failures
            .store(0, Ordering::Relaxed);
    }

    /// Current consecutive failure count.
    pub fn consecutive_compact_failures(&self) -> u32 {
        self.consecutive_compact_failures.load(Ordering::Relaxed)
    }

    /// Whether we're above the warning threshold (UI indicator).
    pub fn is_above_warning(&self) -> bool {
        let input_budget = self.input_token_budget();
        let tokens = self.last_input_tokens.load(Ordering::Relaxed);
        tokens >= input_budget.saturating_sub(WARNING_THRESHOLD_BUFFER)
    }

    /// Usage as a percentage of the context window (0–100).
    pub fn usage_percent(&self) -> u32 {
        let window = self.context_window.load(Ordering::Relaxed);
        if window == 0 {
            return 0;
        }
        let tokens = self.last_input_tokens.load(Ordering::Relaxed);
        ((tokens as u64 * 100) / window as u64) as u32
    }

    /// Whether the current token usage exceeds the microcompact
    /// trigger (proactive tool-result clearing, much lower than
    /// auto-compact).
    pub fn should_microcompact(&self) -> bool {
        self.auto_compact_enabled.load(Ordering::Relaxed) != 0
            && self.last_input_tokens.load(Ordering::Relaxed) >= self.microcompact_trigger()
    }

    /// Absolute token threshold at which microcompact fires.
    pub fn microcompact_trigger(&self) -> u32 {
        let effective = self
            .context_window
            .load(Ordering::Relaxed)
            .saturating_sub(RESERVED_FOR_SUMMARY);
        let scaled = effective * MICROCOMPACT_TRIGGER_PCT / 100;
        scaled.min(self.microcompact_trigger_max_tokens.load(Ordering::Relaxed))
    }

    /// Target token count after microcompact.
    pub fn microcompact_target(&self) -> u32 {
        let effective = self
            .context_window
            .load(Ordering::Relaxed)
            .saturating_sub(RESERVED_FOR_SUMMARY);
        let scaled = effective * MICROCOMPACT_TARGET_PCT / 100;
        scaled.min(self.microcompact_target_max_tokens.load(Ordering::Relaxed))
    }

    /// Formatted context info for `/prune context`.
    pub fn context_info(&self) -> String {
        let window = self.context_window.load(Ordering::Relaxed);
        let output_reserve = self.output_token_reserve.load(Ordering::Relaxed);
        let input_budget = window.saturating_sub(output_reserve);
        let tokens = self.last_input_tokens.load(Ordering::Relaxed);
        let threshold = self.auto_compact_threshold.load(Ordering::Relaxed);
        let pct = self.usage_percent();
        let auto = if self.auto_compact_enabled.load(Ordering::Relaxed) != 0 {
            "on"
        } else {
            "off"
        };
        format!(
            "Context window: {window} tokens\n\
             Output reserve: {output_reserve} tokens\n\
             Input budget:   {input_budget} tokens\n\
             Current usage:  {tokens} tokens ({pct}%, source: {source})\n\
             Auto-compact:   {auto} (threshold: {threshold} tokens)",
            source = self.usage_snapshot().source.as_str()
        )
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::new(DEFAULT_CONTEXT_WINDOW)
    }
}

// ── Shared runtime handle ───────────────────────────────────────

/// Shared, atomically-togglable prune level with stats and context
/// budget. The TUI calls [`PruneLevelHandle::set`] when the user
/// changes the setting; the middleware reads it on every request.
#[derive(Debug, Clone)]
pub struct PruneLevelHandle {
    inner: Arc<AtomicU8>,
    /// Cumulative pruning statistics.
    pub stats: Arc<PruneStats>,
    /// Context-window budget for auto-compact decisions.
    pub budget: Arc<ContextBudget>,
    default_context_window: u32,
    default_output_token_reserve: u32,
    model_context_windows: Arc<std::collections::BTreeMap<String, u32>>,
    model_output_token_limits: Arc<std::collections::BTreeMap<String, u32>>,
    /// One-shot sweep count: set by `/prune sweep`, consumed on next request.
    sweep_pending: Arc<SweepPending>,
}

/// One-shot sweep request consumed by the middleware.
#[derive(Debug, Default)]
struct SweepPending {
    /// 0 = no sweep, u32::MAX = sweep all, otherwise = sweep N
    count: AtomicU32,
}

impl PruneLevelHandle {
    pub fn new(initial: PruneLevel) -> Self {
        Self {
            inner: Arc::new(AtomicU8::new(initial as u8)),
            stats: Arc::new(PruneStats::default()),
            budget: Arc::new(ContextBudget::default()),
            default_context_window: DEFAULT_CONTEXT_WINDOW,
            default_output_token_reserve: 0,
            model_context_windows: Arc::new(std::collections::BTreeMap::new()),
            model_output_token_limits: Arc::new(std::collections::BTreeMap::new()),
            sweep_pending: Arc::new(SweepPending::default()),
        }
    }

    /// Create with an explicit context window size.
    pub fn with_context_window(initial: PruneLevel, context_window: u32) -> Self {
        Self {
            inner: Arc::new(AtomicU8::new(initial as u8)),
            stats: Arc::new(PruneStats::default()),
            budget: Arc::new(ContextBudget::new(context_window)),
            default_context_window: context_window,
            default_output_token_reserve: 0,
            model_context_windows: Arc::new(std::collections::BTreeMap::new()),
            model_output_token_limits: Arc::new(std::collections::BTreeMap::new()),
            sweep_pending: Arc::new(SweepPending::default()),
        }
    }

    /// Create with an explicit default context window and per-model overrides.
    pub fn with_model_context_windows<I, K>(
        initial: PruneLevel,
        default_context_window: u32,
        model_context_windows: I,
    ) -> Self
    where
        I: IntoIterator<Item = (K, u32)>,
        K: Into<String>,
    {
        Self::with_model_context_limits(
            initial,
            default_context_window,
            0,
            model_context_windows,
            std::iter::empty::<(String, u32)>(),
        )
    }

    pub fn with_model_context_limits<I, K, J, L>(
        initial: PruneLevel,
        default_context_window: u32,
        default_output_token_reserve: u32,
        model_context_windows: I,
        model_output_token_limits: J,
    ) -> Self
    where
        I: IntoIterator<Item = (K, u32)>,
        K: Into<String>,
        J: IntoIterator<Item = (L, u32)>,
        L: Into<String>,
    {
        let model_context_windows = model_context_windows
            .into_iter()
            .map(|(model, window)| (model.into(), window))
            .collect();
        let model_output_token_limits = model_output_token_limits
            .into_iter()
            .map(|(model, limit)| (model.into(), limit))
            .collect();
        Self {
            inner: Arc::new(AtomicU8::new(initial as u8)),
            stats: Arc::new(PruneStats::default()),
            budget: Arc::new(ContextBudget::with_output_reserve(
                default_context_window,
                default_output_token_reserve,
            )),
            default_context_window,
            default_output_token_reserve,
            model_context_windows: Arc::new(model_context_windows),
            model_output_token_limits: Arc::new(model_output_token_limits),
            sweep_pending: Arc::new(SweepPending::default()),
        }
    }

    pub fn get(&self) -> PruneLevel {
        PruneLevel::from_u8(self.inner.load(Ordering::Relaxed))
    }

    pub fn set(&self, level: PruneLevel) {
        self.inner.store(level as u8, Ordering::Relaxed);
    }

    pub fn use_full_history_replay_microcompact_profile(&self) {
        self.budget.use_full_history_replay_microcompact_profile();
    }

    /// Report token usage from the latest API response. Called by
    /// the TUI/engine after each model turn.
    pub fn report_usage(&self, input_tokens: u32) {
        self.budget.report_usage(input_tokens);
        self.stats
            .last_input_tokens
            .store(input_tokens, Ordering::Relaxed);
    }

    pub fn report_estimated_usage(&self, input_tokens: u32) {
        self.budget.report_estimated_usage(input_tokens);
        self.stats
            .last_input_tokens
            .store(input_tokens, Ordering::Relaxed);
    }

    pub fn clear_usage(&self) {
        self.budget.clear_usage();
        self.stats.last_input_tokens.store(0, Ordering::Relaxed);
    }

    pub fn set_context_window_for_model(&self, model: &str) {
        let model = model.trim();
        let window = self
            .model_context_windows
            .get(model)
            .copied()
            .unwrap_or(self.default_context_window);
        let output_token_reserve = self
            .model_output_token_limits
            .get(model)
            .copied()
            .unwrap_or_else(|| {
                if self.model_context_windows.contains_key(model) {
                    0
                } else {
                    self.default_output_token_reserve
                }
            });
        if self.budget.context_window() != window
            || self.budget.output_token_reserve() != output_token_reserve
        {
            self.budget.set_context_limits(window, output_token_reserve);
        }
    }

    pub fn fork_for_sub_agent(&self) -> Self {
        let budget = Arc::new(ContextBudget::with_output_reserve(
            self.default_context_window,
            self.default_output_token_reserve,
        ));
        let (microcompact_trigger_max, microcompact_target_max) = self.budget.microcompact_caps();
        budget.set_microcompact_caps(microcompact_trigger_max, microcompact_target_max);
        budget.set_auto_compact_token_limit(self.budget.auto_compact_token_limit());
        Self {
            inner: self.inner.clone(),
            stats: Arc::new(PruneStats::default()),
            budget,
            default_context_window: self.default_context_window,
            default_output_token_reserve: self.default_output_token_reserve,
            model_context_windows: self.model_context_windows.clone(),
            model_output_token_limits: self.model_output_token_limits.clone(),
            sweep_pending: Arc::new(SweepPending::default()),
        }
    }

    /// Queue a sweep of `count` recent tool results (or all if `None`).
    /// The middleware consumes this on the next request.
    pub fn request_sweep(&self, count: Option<usize>) {
        let val = count.map_or(u32::MAX, |n| n as u32);
        self.sweep_pending.count.store(val, Ordering::Relaxed);
    }

    /// Atomically take the pending sweep count, returning `None` if
    /// no sweep is pending.
    fn take_sweep(&self) -> Option<Option<usize>> {
        let val = self.sweep_pending.count.swap(0, Ordering::Relaxed);
        if val == 0 {
            None
        } else if val == u32::MAX {
            Some(None) // sweep all
        } else {
            Some(Some(val as usize))
        }
    }
}

impl Default for PruneLevelHandle {
    fn default() -> Self {
        Self::new(PruneLevel::Conservative)
    }
}

// ── ContextPruneMiddleware ──────────────────────────────────────

/// [`ModelClient`] middleware that prunes stale context before each
/// API call.
///
/// Like `RetryMiddleware` and `LoggingMiddleware` it wraps an inner
/// `Arc<dyn ModelClient>` and implements `ModelClient` itself.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use rebon_api::{
/// #     anthropic_client, AnthropicClientConfig,
/// #     ContextPruneMiddleware, ContextPruneConfig, PruneLevelHandle, PruneLevel,
/// #     ModelClient,
/// # };
/// # fn demo() {
/// let base: Arc<dyn ModelClient> =
///     Arc::new(anthropic_client(AnthropicClientConfig::with_api_key("sk-xxx")));
/// let handle = PruneLevelHandle::new(PruneLevel::Conservative);
/// let pruned = ContextPruneMiddleware::wrap(
///     base,
///     ContextPruneConfig::default(),
///     handle.clone(),
/// );
/// // TUI can later toggle:
/// handle.set(PruneLevel::Aggressive);
/// # }
/// ```
#[derive(Clone)]
pub struct ContextPruneMiddleware {
    inner: Arc<dyn ModelClient>,
    config: ContextPruneConfig,
    level: PruneLevelHandle,
}

impl std::fmt::Debug for ContextPruneMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextPruneMiddleware")
            .field("provider", &self.inner.provider_name())
            .field("config", &self.config)
            .field("level", &self.level.get())
            .finish()
    }
}

impl ContextPruneMiddleware {
    /// Wrap `inner` with the given config and a shared level handle.
    pub fn wrap(
        inner: Arc<dyn ModelClient>,
        config: ContextPruneConfig,
        level: PruneLevelHandle,
    ) -> Self {
        Self {
            inner,
            config,
            level,
        }
    }

    /// Apply all enabled strategies to a cloned message list.
    /// Returns (pruned_messages, stats_delta).
    ///
    /// `preserve_prefix_cache` switches the middleware into **cache-stable
    /// mode**. When true, every strategy that mutates *historical* message
    /// content (clearing old tool_results to a sentinel, dropping old
    /// thinking blocks, removing duplicate tool calls, purging error inputs)
    /// is skipped — because each such mutation rewrites bytes in the middle
    /// of the serialized message prefix and collapses the provider's prompt
    /// cache to whatever common head still matches. A user-triggered `sweep`
    /// is still honored (it's an explicit, one-shot user action, not a
    /// per-request automatic mutation).
    ///
    /// Callers pass `true` for models with byte-exact prefix caching that
    /// matters enough to protect — see [`crate::is_deepseek_model`] for the
    /// current trigger.
    fn prune(
        &self,
        messages: &[Message],
        level: PruneLevel,
        sweep: Option<Option<usize>>,
        preserve_prefix_cache: bool,
    ) -> (Vec<Message>, PruneDelta) {
        let mut delta = PruneDelta::default();

        if level == PruneLevel::Off && sweep.is_none() {
            return (messages.to_vec(), delta);
        }
        if messages.is_empty() {
            return (messages.to_vec(), delta);
        }

        let mut msgs = messages.to_vec();

        // One-shot sweep from `/prune sweep [n]`.
        if let Some(count) = sweep {
            delta.tool_results_cleared += sweep_recent_tool_results(&mut msgs, count);
        }

        // Strategies 0–3 all mutate *historical* (pre-boundary) messages
        // in place: clearing tool_result content to a 33-byte sentinel,
        // dropping thinking blocks, removing duplicate tool-call pairs,
        // emptying error inputs. Every such mutation rewrites bytes in
        // the middle of the request's serialized message prefix, which
        // collapses the provider's prefix cache to whatever common head
        // still matches.
        //
        // `preserve_prefix_cache` short-circuits all of them for callers
        // whose cache economics make in-place mutation net-negative —
        // the few hundred bytes saved per cleared tool_result are
        // dwarfed by the tens of KB of suffix that turn from a cache
        // hit into a cache miss on every subsequent request.
        if level != PruneLevel::Off && !preserve_prefix_cache {
            let boundary = self.protection_boundary(msgs.len());

            // Strategy 0: strip thinking blocks from old messages.
            // OpenAI-compatible thinking-mode providers such as
            // DeepSeek require retained assistant messages to replay
            // their full reasoning_content. Dropping only the thinking
            // block creates a partial assistant replay that the API
            // rejects; whole-message truncation remains safe later.
            if !provider_requires_assistant_reasoning_roundtrip(self.inner.provider_name()) {
                delta.thinking_cleared += strip_old_thinking_blocks(
                    &mut msgs,
                    boundary,
                    self.config.tool_result_max_age_turns,
                );
            }

            // Strategy 1: clear old tool results.
            delta.tool_results_cleared +=
                clear_old_tool_results(&mut msgs, boundary, self.config.tool_result_max_age_turns);

            if level == PruneLevel::Aggressive {
                // Strategy 2: deduplicate identical tool calls.
                let before = msgs.len();
                msgs = deduplicate_tool_calls(msgs, boundary);
                delta.duplicates_removed = (before - msgs.len()) as u32 / 2; // pairs

                // Recalculate boundary after dedup may have shrunk the list.
                let boundary = self.protection_boundary(msgs.len());

                // Strategy 3: purge inputs from old failed tool calls.
                delta.errors_purged =
                    purge_error_inputs(&mut msgs, boundary, self.config.error_purge_age_turns);
            }
        }

        (msgs, delta)
    }

    /// The message index below which pruning is allowed. Messages at
    /// or after this index are protected.
    fn protection_boundary(&self, total: usize) -> usize {
        // Each "turn" is roughly a user+assistant pair = 2 messages.
        let protected_msgs = self.config.protected_recent_turns * 2;
        total.saturating_sub(protected_msgs)
    }
}

/// Per-request delta used to update cumulative [`PruneStats`].
#[derive(Debug, Default)]
struct PruneDelta {
    tool_results_cleared: u32,
    duplicates_removed: u32,
    errors_purged: u32,
    thinking_cleared: u32,
    messages_truncated: u32,
}

#[async_trait]
impl ModelClient for ContextPruneMiddleware {
    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
    }

    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        self.inner.fork_for_sub_agent().map(|inner| {
            Arc::new(ContextPruneMiddleware::wrap(
                inner,
                self.config.clone(),
                self.level.fork_for_sub_agent(),
            )) as Arc<dyn ModelClient>
        })
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ModelClient>> {
        self.inner
            .fork_for_sub_agent_with_cache_key(prompt_cache_key)
            .map(|inner| {
                Arc::new(ContextPruneMiddleware::wrap(
                    inner,
                    self.config.clone(),
                    self.level.fork_for_sub_agent(),
                )) as Arc<dyn ModelClient>
            })
    }

    fn context_prune_handle(&self) -> Option<PruneLevelHandle> {
        Some(self.level.clone())
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::forwarded(self.inner.as_ref())
    }

    fn reset_session_state(&self) {
        // Clear the stale token count so auto-compact decisions
        // after a session reset are based on fresh usage data, not
        // the server-side count from the previous_response_id chain.
        self.level.clear_usage();
        self.inner.reset_session_state();
    }

    fn end_turn(&self) {
        self.inner.end_turn();
    }

    fn invalidate_previous_response_id(&self) {
        self.inner.invalidate_previous_response_id();
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        let level = self.level.get();
        self.level.set_context_window_for_model(&request.model);
        self.level
            .stats
            .requests_processed
            .fetch_add(1, Ordering::Relaxed);

        // Atomically consume one-shot flags.
        let force_compact = self.level.budget.take_compact_once();
        if force_compact {
            tracing::warn!(
                "rebon-api context-prune: compact flag ignored by middleware; engine owns compaction"
            );
        }
        let sweep = self.level.take_sweep();

        if level == PruneLevel::Off && sweep.is_none() {
            return self.inner.create_message_stream(request).await;
        }

        // Cache-stable mode: providers whose prompt cache is keyed on a
        // byte-exact prefix (DeepSeek's context cache; OpenAI Responses'
        // `prompt_cache_key` + `previous_response_id` delta; every vendor
        // whose docs describe a prefix cache, which the client reports
        // through `prefix_cache_is_byte_exact`) skip every sliding
        // mid-prefix mutation. See [`Self::prune`] for the full rationale
        // and trade.
        let preserve_prefix_cache = self.inner.prefix_cache_is_byte_exact()
            || should_preserve_prefix_cache(self.inner.provider_name(), &request.model);

        let (pruned_messages, delta) =
            self.prune(&request.messages, level, sweep, preserve_prefix_cache);
        let original_len = request.messages.len();
        let pruned_len = pruned_messages.len();

        // Update cumulative stats.
        let stats = &self.level.stats;
        stats
            .tool_results_cleared
            .fetch_add(delta.tool_results_cleared, Ordering::Relaxed);
        stats
            .duplicates_removed
            .fetch_add(delta.duplicates_removed, Ordering::Relaxed);
        stats
            .errors_purged
            .fetch_add(delta.errors_purged, Ordering::Relaxed);
        stats
            .thinking_cleared
            .fetch_add(delta.thinking_cleared, Ordering::Relaxed);
        stats
            .messages_truncated
            .fetch_add(delta.messages_truncated, Ordering::Relaxed);

        let content_cleared = delta.tool_results_cleared > 0 || delta.thinking_cleared > 0;
        if content_cleared {
            tracing::debug!(
                cleared = delta.tool_results_cleared,
                thinking = delta.thinking_cleared,
                "rebon-api context-prune: outbound content pruned"
            );
        }

        if pruned_len < original_len || content_cleared || delta.errors_purged > 0 {
            tracing::debug!(
                original = original_len,
                pruned = pruned_len,
                removed = original_len - pruned_len,
                cleared = delta.tool_results_cleared,
                thinking = delta.thinking_cleared,
                deduped = delta.duplicates_removed,
                errors = delta.errors_purged,
                truncated = delta.messages_truncated,
                level = ?level,
                "rebon-api context-prune: messages reduced"
            );
        }

        let pruned_request = CreateMessageRequest {
            messages: pruned_messages,
            ..request
        };
        self.inner.create_message_stream(pruned_request).await
    }
}

fn provider_requires_assistant_reasoning_roundtrip(provider_name: &str) -> bool {
    matches!(provider_name, "openai-compatible")
}

/// Whether the downstream provider's prompt cache is keyed on a
/// byte-exact request prefix, so the sliding mid-history prune
/// strategies (thinking-strip, tool-result clearing, dedup,
/// error-purge) must be short-circuited.
///
/// - **DeepSeek** (relay or direct): server-side context cache.
/// - **OpenAI Responses** (`openai-responses`): `prompt_cache_key` +
///   `previous_response_id` incremental delta both require the
///   reconstructed input to stay a stable growing prefix. Stripping an
///   old reasoning block rewrites the middle of that prefix, which
///   fails the delta's `starts_with` check and collapses the full
///   replay's cache to the static header — a few hundred saved bytes
///   for a tens-of-KB cache miss on every later turn.
/// - **Anthropic-format** (`anthropic`, direct or any relay speaking
///   the protocol): prompt caching is prefix-based (`cache_control`
///   breakpoints hash the exact preceding bytes), so a mid-history
///   sentinel rewrite invalidates every breakpoint after it. Measured
///   on real transcripts this was the dominant cache-break source:
///   sessions with sliding clears re-prefilled 40-70k tokens once per
///   clearing sweep.
///
/// `openai-compatible` (bare chat-completions) stays out: it is the
/// one format where no byte-exact prefix contract is advertised.
pub fn should_preserve_prefix_cache(provider_name: &str, model: &str) -> bool {
    crate::is_deepseek_model(model) || matches!(provider_name, "openai-responses" | "anthropic")
}

// ── Strategy 0: Strip old thinking blocks ──────────────────────

/// Remove `Thinking` content blocks from assistant messages older
/// than `max_age_turns` turns from the protection boundary.
///
/// Matches codex-ref's `should_keep_compacted_history_item()` which
/// drops all `Reasoning` items during compaction, and the Anthropic API's
/// `clear_thinking_20251015` context-management strategy.
///
/// Unlike tool-result clearing (which replaces content with a
/// placeholder), thinking blocks are removed entirely since the
/// model doesn't need to see old reasoning chains.
fn strip_old_thinking_blocks(
    messages: &mut Vec<Message>,
    boundary: usize,
    max_age_turns: usize,
) -> u32 {
    let age_cutoff = boundary.saturating_sub(max_age_turns * 2);
    let mut count = 0u32;

    for msg in messages[..age_cutoff].iter_mut() {
        if msg.role != Role::Assistant {
            continue;
        }
        let before = msg.content.len();
        msg.content
            .retain(|block| !matches!(block, ContentBlock::Thinking(_)));
        count += (before - msg.content.len()) as u32;
    }
    count
}

// ── Strategy 1: Clear old tool results ──────────────────────────

/// Signature used to pick the "latest Read" per file slice. Two
/// Reads collapse to the same signature only when they target the
/// same `file_path` AND the same `offset`/`limit` window.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadSignature {
    path: String,
    offset: Option<i64>,
    limit: Option<i64>,
}

/// Collect the tool_use ids of the latest `Read` invocation for each
/// unique `(file_path, offset, limit)` slice.
///
/// These results carry the authoritative file content the model needs
/// to construct future `Edit.old_string` arguments. Clearing them
/// breaks the Read→Edit chain when the Edit lands many turns after
/// the Read, since nothing else in the transcript holds that content.
///
/// Only `Read` is tracked. `Edit`/`Write` results are status strings,
/// not content; `Bash`/`Grep`/`Glob` results aren't path-addressable
/// in a way that supports future edits.
fn protected_file_read_ids(messages: &[Message]) -> std::collections::HashSet<String> {
    use std::collections::HashMap;

    let mut latest: HashMap<ReadSignature, String> = HashMap::new();

    for msg in messages.iter() {
        if msg.role != Role::Assistant {
            continue;
        }
        for block in &msg.content {
            let ContentBlock::ToolUse(tu) = block else {
                continue;
            };
            if tu.name != "Read" {
                continue;
            }
            let Some(path) = tu.input.get("file_path").and_then(|v| v.as_str()) else {
                continue;
            };
            let sig = ReadSignature {
                path: path.to_string(),
                offset: tu.input.get("offset").and_then(|v| v.as_i64()),
                limit: tu.input.get("limit").and_then(|v| v.as_i64()),
            };
            // Later occurrences overwrite earlier ones, so `latest`
            // ends up keyed on the newest id per signature.
            latest.insert(sig, tu.id.clone());
        }
    }

    latest.into_values().collect()
}

/// Replace `ToolResult.content` with [`TOOL_RESULT_CLEARED`] for
/// tool-result blocks older than `max_age_turns` turns from the
/// protection boundary. Returns the number of blocks cleared.
///
/// The latest `Read` result per `(file_path, offset, limit)` slice is
/// protected regardless of age — see [`protected_file_read_ids`].
fn clear_old_tool_results(messages: &mut [Message], boundary: usize, max_age_turns: usize) -> u32 {
    let age_cutoff = boundary.saturating_sub(max_age_turns * 2);
    let mut count = 0u32;

    let protected = protected_file_read_ids(messages);

    for msg in messages[..age_cutoff].iter_mut() {
        if msg.role != Role::User {
            continue;
        }
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult(tr) = block {
                if tr.content.as_text() != Some(TOOL_RESULT_CLEARED)
                    && !protected.contains(&tr.tool_use_id)
                {
                    tr.content = TOOL_RESULT_CLEARED.into();
                    count += 1;
                }
            }
        }
    }
    count
}

// ── Strategy 2: Deduplicate identical tool calls ────────────────

/// Remove older (tool_use + tool_result) pairs that are identical
/// to a later call, keeping only the most recent instance.
///
/// Two tool calls are considered identical when they share the same
/// `name` and `input` (JSON value equality).
///
/// The scan runs over the full message list from newest to oldest so
/// that the most recent call (whether in the protected suffix or the
/// unprotected prefix) is always kept. However, **only prefix
/// messages (before `boundary`) are actually modified** — the
/// protected suffix is emitted unchanged.
fn deduplicate_tool_calls(messages: Vec<Message>, boundary: usize) -> Vec<Message> {
    // Phase 1: scan the entire list from newest to oldest. The first
    // occurrence of each signature (from the end) is "kept"; any
    // earlier occurrence that falls in the *unprotected prefix* is
    // marked for removal.
    let mut ids_to_remove: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept_signatures: std::collections::HashSet<ToolSignature> =
        std::collections::HashSet::new();

    for (idx, msg) in messages.iter().enumerate().rev() {
        if msg.role != Role::Assistant {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse(tu) = block {
                let sig = ToolSignature {
                    name: tu.name.clone(),
                    input: tu.input.clone(),
                };
                if kept_signatures.contains(&sig) {
                    // Duplicate — only mark for removal if it's in
                    // the unprotected prefix.
                    if idx < boundary {
                        ids_to_remove.insert(tu.id.clone());
                    }
                } else {
                    kept_signatures.insert(sig);
                }
            }
        }
    }

    if ids_to_remove.is_empty() {
        return messages;
    }

    // Phase 2: rebuild the message list, removing marked tool_use
    // blocks (from assistant messages) and their corresponding
    // tool_result blocks (from user messages) — only in the
    // unprotected prefix.
    let mut result = Vec::with_capacity(messages.len());
    for (idx, msg) in messages.into_iter().enumerate() {
        if idx >= boundary {
            // Protected suffix — keep as-is.
            result.push(msg);
            continue;
        }

        let filtered_content: Vec<ContentBlock> = msg
            .content
            .into_iter()
            .filter(|block| match block {
                ContentBlock::ToolUse(tu) => !ids_to_remove.contains(&tu.id),
                ContentBlock::ToolResult(tr) => !ids_to_remove.contains(&tr.tool_use_id),
                _ => true,
            })
            .collect();

        // Drop the message entirely if all content blocks were removed.
        if !filtered_content.is_empty() {
            result.push(Message {
                content: filtered_content,
                ..msg
            });
        }
    }

    result
}

/// Signature for deduplication: tool name + normalised input.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolSignature {
    name: String,
    input: serde_json::Value,
}

// ── Strategy 3: Purge error inputs ──────────────────────────────

/// Strip the `input` field from old `ToolUse` blocks whose
/// corresponding `ToolResult` has `is_error = true`. Returns the
/// number of inputs purged.
fn purge_error_inputs(messages: &mut [Message], boundary: usize, max_age_turns: usize) -> u32 {
    let age_cutoff = boundary.saturating_sub(max_age_turns * 2);
    let mut count = 0u32;

    // Phase 1: collect tool_use IDs whose result was an error.
    let mut errored_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in messages[..boundary].iter() {
        if msg.role != Role::User {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolResult(tr) = block {
                if tr.is_error {
                    errored_tool_ids.insert(tr.tool_use_id.clone());
                }
            }
        }
    }

    if errored_tool_ids.is_empty() {
        return 0;
    }

    // Phase 2: strip inputs from matching tool_use blocks in the
    // age-eligible prefix.
    for msg in messages[..age_cutoff].iter_mut() {
        if msg.role != Role::Assistant {
            continue;
        }
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolUse(tu) = block {
                if errored_tool_ids.contains(&tu.id)
                    && tu.input != serde_json::Value::Object(serde_json::Map::new())
                {
                    tu.input = serde_json::json!({});
                    count += 1;
                }
            }
        }
    }
    count
}

// ── Strategy 4: Auto-compact truncation ─────────────────────────

/// Approximate token count for a text string (1 token ≈ 4 chars).
fn approx_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Maximum tokens worth of user messages to preserve in the
/// compacted history. Matches codex-ref's
/// `COMPACT_USER_MESSAGE_MAX_TOKENS`.
const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;
const COMPACT_ASSISTANT_SUMMARY_MAX_TOKENS: usize = 8_000;

fn truncate_text_to_approx_tokens(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if text.len() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutionHistorySummary {
    total_calls: usize,
    by_name: std::collections::BTreeMap<String, usize>,
    succeeded: usize,
    errored: usize,
}

impl ExecutionHistorySummary {
    fn render(&self) -> String {
        let names = self
            .by_name
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[EXECUTION HISTORY COMPACTION: {} older tool calls were summarized. Tool calls by name: {}. Results: {} succeeded, {} errored. Raw old tool outputs and thinking blocks were omitted.]",
            self.total_calls, names, self.succeeded, self.errored
        )
    }
}

fn summarize_complete_execution_history(messages: &[Message]) -> Option<ExecutionHistorySummary> {
    use std::collections::{BTreeMap, HashMap};

    let mut tool_names_by_id: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if msg.role != Role::Assistant {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse(tu) = block {
                tool_names_by_id.insert(tu.id.clone(), tu.name.clone());
            }
        }
    }

    if tool_names_by_id.is_empty() {
        return None;
    }

    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    let mut succeeded = 0usize;
    let mut errored = 0usize;
    let mut total_calls = 0usize;

    for msg in messages {
        if msg.role != Role::User {
            continue;
        }
        for block in &msg.content {
            let ContentBlock::ToolResult(tr) = block else {
                continue;
            };
            let Some(name) = tool_names_by_id.get(&tr.tool_use_id) else {
                continue;
            };
            total_calls += 1;
            *by_name.entry(name.clone()).or_insert(0) += 1;
            if tr.is_error {
                errored += 1;
            } else {
                succeeded += 1;
            }
        }
    }

    (total_calls > 0).then_some(ExecutionHistorySummary {
        total_calls,
        by_name,
        succeeded,
        errored,
    })
}

fn adjusted_keep_from_for_tool_pair_boundary(messages: &[Message], keep_from: usize) -> usize {
    if keep_from == 0 || keep_from >= messages.len() {
        return keep_from;
    }

    let tail_first = &messages[keep_from];
    if tail_first.role != Role::User
        || !tail_first
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult(_)))
    {
        return keep_from;
    }

    let previous_idx = keep_from.saturating_sub(1);
    let previous = &messages[previous_idx];
    if previous.role == Role::Assistant
        && previous
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse(_)))
    {
        previous_idx
    } else {
        keep_from
    }
}

/// When the context is approaching the window limit, rebuild the
/// conversation history preserving:
///
/// 1. **Recent user messages** (up to 20 K tokens, newest first) —
///    so the model remembers the original prompt and recent
///    instructions. Matches codex-ref's `collect_user_messages` +
///    `build_compacted_history_with_limit` backward walk.
///
/// 2. **Execution metadata for old complete tool calls** — old
///    assistant `tool_use` blocks, matching user `tool_result`
///    blocks, result bodies, and thinking blocks are omitted from
///    the compacted prefix and replaced with deterministic counts.
///
/// 3. **Last assistant message before the cut** — serves as a
///    handoff summary of what the model was doing.
///
/// 4. **Protected tail** — the most recent `protected_turns`
///    turn-pairs are kept unchanged.
///
/// This replaces the old "drop everything + placeholder" approach
/// that lost all task context.
/// When the context is approaching the window limit, rebuild the
/// conversation preserving recent turns, user messages, and a
/// handoff summary. Public so callers can use it as a direct
/// fallback when model-based compaction fails.
pub fn auto_compact_truncate(messages: Vec<Message>, protected_turns: usize) -> Vec<Message> {
    let protected_msgs = protected_turns * 2;
    if messages.len() <= protected_msgs + 1 {
        return messages;
    }

    let keep_from = messages.len().saturating_sub(protected_msgs);
    let keep_from = adjusted_keep_from_for_tool_pair_boundary(&messages, keep_from);
    auto_compact_truncate_from(messages, keep_from, protected_turns)
}

/// [`auto_compact_truncate`] with a quantized protected-tail boundary, for
/// callers that rebuild the projection every turn (the engine replay window).
///
/// The plain variant recomputes `keep_from = len - protected` on every call,
/// so a growing session shifts the boundary by one turn-pair per turn — the
/// synthetic digest over the dropped prefix changes and every retained tail
/// message shifts position, busting provider prefix caches on each request.
/// Snapping the boundary down to a whole multiple of the protected window
/// keeps the projected prefix byte-stable while the tail grows from one to
/// two windows, then jumps a full window at once: one cold request per
/// window's worth of turns instead of every turn.
pub fn auto_compact_truncate_cache_stable(
    messages: Vec<Message>,
    protected_turns: usize,
) -> Vec<Message> {
    let protected_msgs = protected_turns * 2;
    if messages.len() <= protected_msgs + 1 {
        return messages;
    }

    let quantum = protected_msgs.max(1);
    let keep_from = (messages.len() - protected_msgs) / quantum * quantum;
    if keep_from == 0 {
        return messages;
    }
    auto_compact_truncate_from(messages, keep_from, protected_turns)
}

/// Variant of [`auto_compact_truncate`] that lets callers choose the
/// exact protected-tail boundary. Used for token-budget replay pruning,
/// where "last N turns" may still be too large.
pub fn auto_compact_truncate_from(
    messages: Vec<Message>,
    keep_from: usize,
    protected_turns: usize,
) -> Vec<Message> {
    let keep_from = adjusted_keep_from_for_tool_pair_boundary(&messages, keep_from);
    if keep_from == 0 || keep_from >= messages.len() {
        return messages;
    }

    let dropped = &messages[..keep_from];
    let execution_summary = summarize_complete_execution_history(dropped);

    // ── Collect user messages from the dropped portion ──────────
    // Walk backwards (newest first) and accumulate up to the token
    // budget, mirroring codex-ref's backward traversal.
    let mut user_texts: Vec<String> = Vec::new();
    let mut remaining_tokens = COMPACT_USER_MESSAGE_MAX_TOKENS;

    for msg in dropped.iter().rev() {
        if msg.role != Role::User {
            continue;
        }
        for block in &msg.content {
            if let Some(text) = block.as_text() {
                // Skip cleared tool results — they carry no useful info.
                if text == TOOL_RESULT_CLEARED {
                    continue;
                }
                let tokens = approx_tokens(text);
                if tokens == 0 {
                    continue;
                }
                if tokens <= remaining_tokens {
                    user_texts.push(text.to_string());
                    remaining_tokens = remaining_tokens.saturating_sub(tokens);
                } else if remaining_tokens > 0 {
                    // Truncate to fit the budget.
                    let chars = remaining_tokens * 4;
                    let truncated: String = text.chars().take(chars).collect();
                    if !truncated.is_empty() {
                        user_texts.push(truncated);
                    }
                    remaining_tokens = 0;
                }
                if remaining_tokens == 0 {
                    break;
                }
            }
        }
        if remaining_tokens == 0 {
            break;
        }
    }
    user_texts.reverse(); // Restore chronological order.

    // ── Extract the last assistant message as handoff summary ───
    let last_assistant_text = dropped.iter().rev().find_map(|msg| {
        if msg.role != Role::Assistant {
            return None;
        }
        msg.content.iter().find_map(|b| {
            b.as_text()
                .filter(|t| !t.is_empty() && *t != TOOL_RESULT_CLEARED)
                .map(|t| t.to_string())
        })
    });

    // ── Build compacted result ──────────────────────────────────
    let mut result = Vec::new();

    // Re-inject preserved user messages.
    if !user_texts.is_empty() {
        let combined = user_texts.join("\n\n---\n\n");
        result.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text(TextBlock { text: combined })],
        });
    }

    if let Some(execution_summary) = execution_summary {
        result.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text(TextBlock {
                text: execution_summary.render(),
            })],
        });
    }

    // Handoff summary from the model's perspective.
    let summary_header = format!(
        "[CONTEXT COMPACTION: {} earlier messages were compacted to \
         stay within the context window. The user messages above are \
         preserved from the original conversation. The most recent \
         {} turns follow below.]",
        keep_from, protected_turns,
    );
    let summary = match last_assistant_text {
        Some(assistant_text) => {
            let assistant_text = truncate_text_to_approx_tokens(
                &assistant_text,
                COMPACT_ASSISTANT_SUMMARY_MAX_TOKENS,
            );
            format!(
                "{summary_header}\n\n\
             Here is the summary from the previous work:\n\n{assistant_text}"
            )
        }
        None => summary_header,
    };
    result.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text(TextBlock { text: summary })],
    });

    // Protected tail — recent turns kept unchanged.
    result.extend_from_slice(&messages[keep_from..]);

    // The protected tail may start with a user message whose
    // tool_result references a tool_use that was in the dropped
    // portion.  Repair any broken pairs so the API won't reject
    // the replay.
    ensure_tool_result_pairing(&mut result);
    result
}

// ── Tool-use / tool-result pairing repair ───────────────────────

/// Fixed prefix for the synthetic tool_result content. Exposed as a
/// constant so downstream code and tests can detect synthetic blocks
/// without parsing the full (tool-name-including) string.
///
/// The full content format is:
/// `"[Tool result missing — the `<name>` call was interrupted]"`,
/// built by [`synthetic_tool_result_content`].
pub const SYNTHETIC_TOOL_RESULT_PREFIX: &str = "[Tool result missing";

/// Backwards-compatible alias for the generic placeholder. Prefer
/// [`synthetic_tool_result_content`] which embeds the tool name so
/// the model can make sensible follow-up decisions (retry vs. skip).
pub const SYNTHETIC_TOOL_RESULT_PLACEHOLDER: &str =
    "[Tool result missing — the tool call was interrupted]";

/// Build the synthetic tool_result content for an orphan tool_use.
/// Including the tool name lets the model distinguish e.g. a
/// file-read that probably still needs to happen from a one-shot
/// shell command that's already had side effects.
pub fn synthetic_tool_result_content(tool_name: &str) -> String {
    if tool_name.is_empty() {
        SYNTHETIC_TOOL_RESULT_PLACEHOLDER.to_string()
    } else {
        format!("[Tool result missing — the `{tool_name}` call was interrupted]")
    }
}

/// What a call to [`ensure_tool_result_pairing_with_report`] changed.
/// Callers that persist the transcript to disk can use this to flush
/// the synthetic tool_result blocks back to the transcript file so
/// subsequent resumes don't re-run the same repair on every load.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PairingRepairReport {
    /// Tool-use IDs that got a synthetic error `tool_result` appended
    /// because the original result was missing from the transcript.
    /// `(tool_use_id, tool_name)` so the persister can rebuild a
    /// faithful ToolResult entry with the tool name in the content.
    pub synthesized: Vec<(String, String)>,
    /// Subset of `synthesized` that sits at the tail of the
    /// conversation — the **last** assistant message that owns a
    /// `tool_use` has orphans here. These are the only orphans that
    /// a caller can safely persist by appending a user-role heal
    /// entry to the transcript: on reload the heal entry sits
    /// directly after the orphan tool_use, so
    /// `ensure_tool_result_pairing` pairs them up cleanly.
    ///
    /// Orphans on an **earlier** assistant message (any synthesized
    /// entry that is NOT in this list) cannot be fixed by a tail
    /// heal — the pairing pass requires the tool_result to live in
    /// the user message **immediately following** the orphan
    /// tool_use, and a tail-appended entry sits behind one or more
    /// unrelated turns. Persisting those orphans would accumulate
    /// noise on every resume without actually repairing anything.
    ///
    /// Persisters should only flush this subset back to disk; the
    /// wider `synthesized` list still covers the in-memory repair
    /// that keeps the current request well-formed.
    pub synthesized_tail_orphans: Vec<(String, String)>,
    /// Tool-result IDs that were stripped because the matching
    /// `tool_use` does not exist anywhere in the conversation. These
    /// should NOT be re-persisted — they are being removed, not added.
    pub globally_orphaned_result_ids: Vec<String>,
    /// Tool-result IDs that were stripped from a specific user
    /// message because they referenced a tool_use not in the
    /// preceding assistant message.
    pub locally_orphaned_result_ids: Vec<String>,
}

impl PairingRepairReport {
    /// True when nothing was repaired. Callers can short-circuit any
    /// persistence step.
    pub fn is_empty(&self) -> bool {
        self.synthesized.is_empty()
            && self.globally_orphaned_result_ids.is_empty()
            && self.locally_orphaned_result_ids.is_empty()
    }

    /// Number of synthesized orphans that sit on an earlier assistant
    /// message and therefore cannot be resolved by appending a tail
    /// heal entry. Useful for diagnostics — a non-zero value means
    /// the in-memory repair will re-run on every resume for this
    /// conversation.
    pub fn mid_orphan_count(&self) -> usize {
        self.synthesized
            .len()
            .saturating_sub(self.synthesized_tail_orphans.len())
    }
}

/// Ensure every `tool_use` block on an assistant message has a
/// matching `tool_result` block on the following user message, and
/// vice-versa.
///
/// Without this, a user cancellation mid-tool-use or aggressive
/// truncation leaves orphan `tool_use` blocks; the API rejects the
/// replay with "No tool output found for function call".
///
/// This is the zero-report variant for code that only needs the
/// in-memory repair. Persisters should call
/// [`ensure_tool_result_pairing_with_report`] instead so they know
/// what to flush to disk.
pub fn ensure_tool_result_pairing(messages: &mut Vec<Message>) {
    let _ = ensure_tool_result_pairing_with_report(messages);
}

/// Variant of [`ensure_tool_result_pairing`] that returns a report
/// describing the exact repairs it made. Callers that own the
/// transcript file can use the report to append synthetic tool_result
/// entries to disk so the next resume starts clean, eliminating the
/// repeat-warning pattern on every load after a crashed session.
pub fn ensure_tool_result_pairing_with_report(messages: &mut Vec<Message>) -> PairingRepairReport {
    use std::collections::HashSet;

    let mut report = PairingRepairReport::default();

    // Pre-pass: collect all tool_use IDs across the entire conversation so
    // we can detect orphaned tool_results in user messages that are NOT
    // preceded by an assistant message (e.g. when deduplication dropped the
    // entire assistant message but the protected suffix kept the user reply).
    let all_tool_use_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse(tu) => Some(tu.id.clone()),
            _ => None,
        })
        .collect();

    // Find the index of the **last** assistant message that carries
    // any `tool_use` block. Orphans on this message are safe to
    // persist as a tail heal entry because — after repair — either
    // the trailing user message or a freshly-inserted one already
    // holds the synthetic `tool_result`s in the immediately-following
    // position that the pairing pass checks on the next resume.
    //
    // Orphans on any earlier assistant message are "middle orphans":
    // appending a heal to the transcript tail cannot satisfy the
    // immediately-following-user-message invariant, so re-persisting
    // them each resume just appends dead noise.
    let last_tool_use_assistant_idx: Option<usize> = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| {
            m.role == Role::Assistant
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse(_)))
        })
        .map(|(idx, _)| idx);

    for msg in messages.iter_mut().filter(|m| m.role == Role::User) {
        let orphaned_ids: HashSet<String> = msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(tr) => {
                    if !all_tool_use_ids.contains(&tr.tool_use_id) {
                        Some(tr.tool_use_id.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect();

        if !orphaned_ids.is_empty() {
            tracing::debug!(
                orphaned_results = orphaned_ids.len(),
                "ensure_tool_result_pairing: removing globally orphaned tool_results (no matching tool_use in any assistant message)"
            );
            report
                .globally_orphaned_result_ids
                .extend(orphaned_ids.iter().cloned());
            msg.content.retain(|block| match block {
                ContentBlock::ToolResult(tr) => !orphaned_ids.contains(&tr.tool_use_id),
                _ => true,
            });
        }
    }

    // Main pass: ensure each assistant tool_use has a matching tool_result
    // in the immediately following user message.
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role != Role::Assistant {
            i += 1;
            continue;
        }

        // Collect (id, name) for every tool_use in this assistant
        // message. The name is threaded into the synthetic placeholder
        // below so the model can tell which tool was interrupted.
        let tool_uses: Vec<(String, String)> = messages[i]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(tu) => Some((tu.id.clone(), tu.name.clone())),
                _ => None,
            })
            .collect();

        if tool_uses.is_empty() {
            i += 1;
            continue;
        }

        let tool_use_id_set: HashSet<&str> = tool_uses.iter().map(|(id, _)| id.as_str()).collect();

        // Check all contiguous user messages that follow this assistant
        // message. Transcript-level `isMeta` reminders are downgraded to
        // ordinary user text by the time we reach this API layer, so a
        // tool_result may legitimately be in the next user window rather
        // than exactly `i + 1`.
        let mut user_window_end = i + 1;
        let mut existing_result_ids: HashSet<String> = HashSet::new();
        while let Some(message) = messages.get(user_window_end) {
            if message.role != Role::User {
                break;
            }
            existing_result_ids.extend(message.content.iter().filter_map(|block| match block {
                ContentBlock::ToolResult(tr) => Some(tr.tool_use_id.clone()),
                _ => None,
            }));
            user_window_end += 1;
        }
        let has_following_user = user_window_end > i + 1;

        // Forward: tool_use without matching tool_result.
        let missing: Vec<&(String, String)> = tool_uses
            .iter()
            .filter(|(id, _)| !existing_result_ids.contains(id.as_str()))
            .collect();

        // Reverse: tool_result without matching tool_use.
        let orphaned: HashSet<&str> = existing_result_ids
            .iter()
            .filter(|id| !tool_use_id_set.contains(id.as_str()))
            .map(String::as_str)
            .collect();

        if missing.is_empty() && orphaned.is_empty() {
            i = if has_following_user {
                user_window_end
            } else {
                i + 1
            };
            continue;
        }

        tracing::debug!(
            missing_results = missing.len(),
            orphaned_results = orphaned.len(),
            "ensure_tool_result_pairing: repairing tool_use/tool_result mismatch"
        );

        // Build synthetic error tool_result blocks for missing IDs,
        // embedding the tool name so the model knows which call was
        // interrupted.
        let is_tail_assistant = Some(i) == last_tool_use_assistant_idx;
        let synthetic: Vec<ContentBlock> = missing
            .iter()
            .map(|(id, name)| {
                report.synthesized.push((id.clone(), name.clone()));
                if is_tail_assistant {
                    report
                        .synthesized_tail_orphans
                        .push((id.clone(), name.clone()));
                }
                ContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: id.clone(),
                    content: synthetic_tool_result_content(name).into(),
                    is_error: true,
                })
            })
            .collect();

        report
            .locally_orphaned_result_ids
            .extend(orphaned.iter().map(|s| (*s).to_string()));

        if has_following_user {
            // Patch the existing user window: strip locally orphaned
            // results wherever they appeared, then append synthetics to the
            // first user message so any newly-created pair is immediately
            // after the assistant tool_use.
            if !orphaned.is_empty() {
                for user_idx in (i + 1)..user_window_end {
                    messages[user_idx].content.retain(|block| match block {
                        ContentBlock::ToolResult(tr) => !orphaned.contains(tr.tool_use_id.as_str()),
                        _ => true,
                    });
                }
            }
            let user_msg = &mut messages[i + 1];
            user_msg.content.extend(synthetic);
        } else {
            // No user message follows — insert a new one with the synthetics.
            messages.insert(
                i + 1,
                Message {
                    role: Role::User,
                    content: synthetic,
                },
            );
        }

        // Skip past assistant + the (now-repaired) user window.
        i = if has_following_user {
            user_window_end
        } else {
            i + 2
        };
    }

    // Single summary WARN for callers that discard the report. Keeps
    // operators informed that a repair happened without spamming one
    // line per assistant message — which on a long resumed session
    // used to emit 12+ WARN lines for a single healed transcript.
    if !report.is_empty() {
        tracing::warn!(
            synthesized = report.synthesized.len(),
            synthesized_tail = report.synthesized_tail_orphans.len(),
            mid_orphans = report.mid_orphan_count(),
            globally_orphaned = report.globally_orphaned_result_ids.len(),
            locally_orphaned = report.locally_orphaned_result_ids.len(),
            "ensure_tool_result_pairing: repaired tool_use/tool_result pairing"
        );
    }

    report
}

// ── Public helpers for /prune sweep ─────────────────────────────

/// Sweep (force-clear) tool results from the most recent `count`
/// tool-result blocks. Used by `/prune sweep [count]`.
///
/// Operates on a cloned message list — caller should replace their
/// messages with the result.
pub fn sweep_recent_tool_results(messages: &mut [Message], count: Option<usize>) -> u32 {
    let limit = count.unwrap_or(usize::MAX);
    let mut cleared = 0u32;

    // Walk backwards to find and clear the most recent tool results.
    for msg in messages.iter_mut().rev() {
        if msg.role != Role::User {
            continue;
        }
        for block in msg.content.iter_mut().rev() {
            if let ContentBlock::ToolResult(tr) = block {
                if tr.content.as_text() != Some(TOOL_RESULT_CLEARED) {
                    tr.content = TOOL_RESULT_CLEARED.into();
                    cleared += 1;
                    if cleared as usize >= limit {
                        return cleared;
                    }
                }
            }
        }
    }
    cleared
}

// ── Microcompact: proactive tool-result clearing ───────────────

/// Whether a tool's result is safe to clear during microcompact.
///
/// Safe means the result is recoverable: a file can be read again, a search
/// re-run, a page re-fetched, a command re-issued. That is a property of what
/// the tool *is*, so it asks the kind rather than carrying a list of names
/// that quietly went stale every time a tool was added. Everything else — the
/// task board, agent transcripts, anything whose result exists nowhere but in
/// this conversation — is left alone.
fn is_microcompact_clearable(tool_name: &str) -> bool {
    use rebon_tools_core::ToolKind;
    matches!(
        rebon_tools_core::tool_kind_for_name(tool_name),
        ToolKind::Shell
            | ToolKind::Search
            | ToolKind::FileRead
            | ToolKind::FileEdit
            | ToolKind::Web
    )
}

/// Cache-aware reverse microcompact. Proactively clears tool-result blocks
/// to keep estimated tokens below `target_tokens` while preserving DeepSeek /
/// OpenAI prompt-cache prefix stability.
///
/// **Direction matters.** The naive "oldest first" walk clears blocks near
/// the start of history, which is exactly where prefix-cache divergence is
/// most expensive — every subsequent request's prefix differs from the
/// cached one starting at the first cleared byte, so the cache hit collapses
/// to roughly the system-prompt-and-runtime-context head.
///
/// Instead, we walk forwards to collect eligible candidates, then **consume
/// them in reverse** (newest unprotected first). The first cleared byte is
/// near the end of history, leaving the long, byte-stable head intact — the
/// cache continues to match the previous request all the way down to
/// wherever we start clearing. Each microcompact event invalidates only the
/// near-tail of the prefix rather than the whole history.
///
/// Eligibility criteria for a tool_result block:
/// 1. [`is_microcompact_clearable`] (reads / writes / search / shell / web —
///    never tools that carry irreplaceable runtime data).
/// 2. **Consumed**: at least one assistant message exists at a higher index,
///    proving the model has already seen this result. The just-appended
///    tool_result, which the next request will deliver to the model, is
///    therefore never cleared.
/// 3. Not already cleared.
/// 4. Not within the `protected_recent` most-recent eligible blocks (keeps
///    the model's working set warm).
///
/// Returns the number of blocks cleared.
pub fn microcompact_tool_results(
    messages: &mut [Message],
    current_tokens: u32,
    target_tokens: u32,
    protected_recent: usize,
) -> u32 {
    if current_tokens <= target_tokens {
        return 0;
    }

    // Build tool_use_id → tool_name so we can filter to clearable tools.
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in messages.iter() {
        if msg.role != Role::Assistant {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse(tu) = block {
                tool_names.insert(tu.id.clone(), tu.name.clone());
            }
        }
    }

    // The "consumed" boundary: anything strictly before the last assistant
    // message has already been observed by the model. If there is no
    // assistant message at all, nothing is consumed yet — leave history
    // alone.
    let Some(last_assistant_idx) = messages.iter().rposition(|msg| msg.role == Role::Assistant)
    else {
        return 0;
    };

    // Walk forwards (oldest first) so the protection slice cuts off the
    // *newest* eligible blocks — the ones nearest the tail — and the
    // unprotected pool stays ordered from oldest to newest.
    struct Candidate {
        msg_idx: usize,
        block_idx: usize,
        approx_tokens: u32,
    }
    let mut candidates: Vec<Candidate> = Vec::new();

    for (msg_idx, msg) in messages.iter().enumerate() {
        if msg.role != Role::User {
            continue;
        }
        // Consumed guard: msg must sit strictly before the last assistant.
        if msg_idx >= last_assistant_idx {
            continue;
        }
        for (block_idx, block) in msg.content.iter().enumerate() {
            if let ContentBlock::ToolResult(tr) = block {
                if tr.content.as_text() == Some(TOOL_RESULT_CLEARED) {
                    continue; // already cleared
                }
                let tool_name = tool_names.get(&tr.tool_use_id);
                let is_clearable =
                    tool_name.is_some_and(|name| is_microcompact_clearable(name.as_str()));
                if !is_clearable {
                    continue;
                }
                let approx = (tr.content.approx_len() / 4) as u32;
                candidates.push(Candidate {
                    msg_idx,
                    block_idx,
                    approx_tokens: approx,
                });
            }
        }
    }

    // Drop the `protected_recent` most-recent eligible blocks (they sit at
    // the end of `candidates`). What's left is the oldest-to-newer pool
    // that we're allowed to touch.
    let clearable_count = candidates.len().saturating_sub(protected_recent);
    if clearable_count == 0 {
        return 0;
    }
    candidates.truncate(clearable_count);

    // Consume the pool from the **newest** end backwards: the first byte
    // we mutate is as close to the tail as possible, so the cached prefix
    // remains valid for the longest possible head of the conversation.
    let mut tokens_to_free = current_tokens.saturating_sub(target_tokens);
    let mut to_clear: Vec<(usize, usize)> = Vec::new();

    for candidate in candidates.iter().rev() {
        if tokens_to_free == 0 {
            break;
        }
        let freed = candidate
            .approx_tokens
            .saturating_sub((TOOL_RESULT_CLEARED.len() / 4) as u32);
        tokens_to_free = tokens_to_free.saturating_sub(freed);
        to_clear.push((candidate.msg_idx, candidate.block_idx));
    }

    let cleared = to_clear.len() as u32;
    for (msg_idx, block_idx) in to_clear {
        if let ContentBlock::ToolResult(tr) = &mut messages[msg_idx].content[block_idx] {
            tr.content = TOOL_RESULT_CLEARED.into();
        }
    }

    cleared
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TextBlock, ThinkingBlock, ToolResultBlock, ToolUseBlock};

    fn make_tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse(ToolUseBlock {
            id: id.to_string(),
            name: name.to_string(),
            input,
        })
    }

    fn make_tool_result(tool_use_id: &str, content: &str, is_error: bool) -> ContentBlock {
        ContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: tool_use_id.to_string(),
            content: content.into(),
            is_error,
        })
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextBlock {
            text: text.to_string(),
        })
    }

    fn assistant_msg(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content: blocks,
        }
    }

    fn user_msg(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::User,
            content: blocks,
        }
    }

    // ── Cache-stable truncation boundary ────────────────────────

    #[test]
    fn cache_stable_truncate_moves_the_boundary_in_window_steps() {
        let conversation = |messages: usize| {
            (0..messages)
                .map(|index| {
                    if index % 2 == 0 {
                        user_msg(vec![text_block(&format!("user-{index}"))])
                    } else {
                        assistant_msg(vec![text_block(&format!("assistant-{index}"))])
                    }
                })
                .collect::<Vec<_>>()
        };

        // Below one window plus the compaction row: untouched, like the plain
        // variant.
        assert_eq!(
            auto_compact_truncate_cache_stable(conversation(21), 10),
            conversation(21)
        );
        // Under one full quantum past the window the boundary stays at zero,
        // so the history is still passed through whole.
        assert_eq!(
            auto_compact_truncate_cache_stable(conversation(39), 10),
            conversation(39)
        );

        // Within a band the earlier projection is a byte-identical prefix of
        // the later one — this is the property provider prompt caches need.
        let at_40 = auto_compact_truncate_cache_stable(conversation(40), 10);
        let at_58 = auto_compact_truncate_cache_stable(conversation(58), 10);
        assert_eq!(at_58[..at_40.len()], at_40[..]);
        // On the exact multiple the result matches the plain variant.
        assert_eq!(at_40, auto_compact_truncate(conversation(40), 10));
        // One message past the multiple the plain variant slides but the
        // cache-stable boundary holds.
        assert_ne!(
            auto_compact_truncate_cache_stable(conversation(41), 10),
            auto_compact_truncate(conversation(41), 10)
        );
    }

    // ── PruneLevel ──────────────────────────────────────────────

    #[test]
    fn prune_level_round_trips_through_u8() {
        for level in [
            PruneLevel::Off,
            PruneLevel::Conservative,
            PruneLevel::Aggressive,
        ] {
            assert_eq!(PruneLevel::from_u8(level as u8), level);
        }
    }

    #[test]
    fn prune_level_from_config_value() {
        assert_eq!(PruneLevel::from_config_value("off"), PruneLevel::Off);
        assert_eq!(
            PruneLevel::from_config_value("conservative"),
            PruneLevel::Conservative
        );
        assert_eq!(
            PruneLevel::from_config_value("aggressive"),
            PruneLevel::Aggressive
        );
        assert_eq!(
            PruneLevel::from_config_value("unknown"),
            PruneLevel::Conservative
        );
    }

    #[test]
    fn prune_level_config_value_string() {
        assert_eq!(PruneLevel::Off.as_config_value(), "off");
        assert_eq!(PruneLevel::Conservative.as_config_value(), "conservative");
        assert_eq!(PruneLevel::Aggressive.as_config_value(), "aggressive");
    }

    // ── PruneLevelHandle ────────────────────────────────────────

    #[test]
    fn prune_level_handle_get_set() {
        let handle = PruneLevelHandle::new(PruneLevel::Off);
        assert_eq!(handle.get(), PruneLevel::Off);
        handle.set(PruneLevel::Aggressive);
        assert_eq!(handle.get(), PruneLevel::Aggressive);
    }

    #[test]
    fn prune_level_handle_default_is_conservative() {
        let handle = PruneLevelHandle::default();
        assert_eq!(handle.get(), PruneLevel::Conservative);
    }

    // ── Strategy 1: clear_old_tool_results ──────────────────────

    #[test]
    fn clears_old_tool_results_beyond_age_cutoff() {
        // 10 messages: 5 turn pairs. protected_recent = 2 turns (4 msgs).
        // boundary = 10 - 4 = 6. max_age = 1 turn → age_cutoff = 6 - 2 = 4.
        // So messages 0..4 get cleared.
        let mut messages = vec![
            // Turn 0 (idx 0-1) — should be cleared
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "file contents here", false)]),
            // Turn 1 (idx 2-3) — should be cleared
            assistant_msg(vec![make_tool_use(
                "t1",
                "Read",
                serde_json::json!({"path": "b.rs"}),
            )]),
            user_msg(vec![make_tool_result("t1", "more contents", false)]),
            // Turn 2 (idx 4-5) — within max_age, should NOT be cleared
            assistant_msg(vec![make_tool_use(
                "t2",
                "Read",
                serde_json::json!({"path": "c.rs"}),
            )]),
            user_msg(vec![make_tool_result("t2", "recent contents", false)]),
            // Turn 3 (idx 6-7) — protected
            assistant_msg(vec![text_block("thinking...")]),
            user_msg(vec![text_block("user prompt")]),
            // Turn 4 (idx 8-9) — protected
            assistant_msg(vec![text_block("response")]),
            user_msg(vec![text_block("follow up")]),
        ];

        clear_old_tool_results(&mut messages, 6, 1);

        // Turn 0 cleared
        if let ContentBlock::ToolResult(tr) = &messages[1].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        } else {
            panic!("expected tool result");
        }
        // Turn 1 cleared
        if let ContentBlock::ToolResult(tr) = &messages[3].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        } else {
            panic!("expected tool result");
        }
        // Turn 2 NOT cleared (within max_age)
        if let ContentBlock::ToolResult(tr) = &messages[5].content[0] {
            assert_eq!(tr.content, "recent contents");
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn clear_tool_results_noop_when_all_recent() {
        let mut messages = vec![
            assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t0", "contents", false)]),
        ];
        let original = messages.clone();
        clear_old_tool_results(&mut messages, 2, 1);
        assert_eq!(messages, original);
    }

    // ── protected_file_read_ids: latest-Read-per-path retention ──

    #[test]
    fn latest_read_per_path_is_protected_from_clearing() {
        // Old Read for "a.rs" is the ONLY Read of that path → must
        // survive clearing because a later Edit will rely on it.
        //
        // Layout: 10 messages (5 turns). boundary = 10-4 = 6.
        // max_age = 1 → age_cutoff = 6-2 = 4. Turns 0-1 clearable.
        let mut messages = vec![
            // Turn 0: Read a.rs — old, normally cleared, but we
            // protect it because it's the latest Read of a.rs.
            assistant_msg(vec![make_tool_use(
                "read_a",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("read_a", "pub fn a() {}", false)]),
            // Turn 1: Bash — old, no path, should be cleared.
            assistant_msg(vec![make_tool_use(
                "bash1",
                "Bash",
                serde_json::json!({"command": "ls"}),
            )]),
            user_msg(vec![make_tool_result("bash1", "a.rs\nb.rs", false)]),
            // Turn 2-4: protected window (text only, no tool results).
            assistant_msg(vec![text_block("thinking")]),
            user_msg(vec![text_block("go ahead")]),
            assistant_msg(vec![text_block("ok")]),
            user_msg(vec![text_block("next")]),
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("thanks")]),
        ];

        clear_old_tool_results(&mut messages, 6, 1);

        // Turn 0 Read a.rs: PROTECTED (latest per path).
        if let ContentBlock::ToolResult(tr) = &messages[1].content[0] {
            assert_eq!(
                tr.content, "pub fn a() {}",
                "latest Read per path must survive clearing"
            );
        } else {
            panic!("expected tool result");
        }
        // Turn 1 Bash: CLEARED (no file_path → no protection).
        if let ContentBlock::ToolResult(tr) = &messages[3].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn superseded_read_is_cleared_when_newer_read_same_slice_exists() {
        // Two Reads of a.rs with identical (path, offset, limit).
        // The older one is dominated by the newer and gets cleared.
        let mut messages = vec![
            // Turn 0: older Read a.rs (clearable)
            assistant_msg(vec![make_tool_use(
                "r0",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("r0", "v1", false)]),
            // Turn 1: older Bash (clearable)
            assistant_msg(vec![make_tool_use(
                "b0",
                "Bash",
                serde_json::json!({"command": "true"}),
            )]),
            user_msg(vec![make_tool_result("b0", "ok", false)]),
            // Turn 2: newer Read a.rs (same signature — supersedes r0)
            assistant_msg(vec![make_tool_use(
                "r1",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("r1", "v2", false)]),
            // Protected tail.
            assistant_msg(vec![text_block("")]),
            user_msg(vec![text_block("")]),
            assistant_msg(vec![text_block("")]),
            user_msg(vec![text_block("")]),
        ];

        clear_old_tool_results(&mut messages, 6, 1);

        // r0 cleared: a newer Read of the same slice exists.
        if let ContentBlock::ToolResult(tr) = &messages[1].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        } else {
            panic!("expected tool result");
        }
        // r1 is inside the preserved window (turn 2 == idx 4-5,
        // age_cutoff=4 → preserved).
        if let ContentBlock::ToolResult(tr) = &messages[5].content[0] {
            assert_eq!(tr.content, "v2");
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn reads_with_different_slices_are_both_protected() {
        // Same file, different offset windows → each slice is
        // independently protected because the Edit may need either.
        let mut messages = vec![
            // Turn 0: Read lines 1-100 (clearable window)
            assistant_msg(vec![make_tool_use(
                "head",
                "Read",
                serde_json::json!({"file_path": "big.rs", "offset": 1, "limit": 100}),
            )]),
            user_msg(vec![make_tool_result("head", "lines 1..100", false)]),
            // Turn 1: Read lines 500-600 (clearable window)
            assistant_msg(vec![make_tool_use(
                "mid",
                "Read",
                serde_json::json!({"file_path": "big.rs", "offset": 500, "limit": 100}),
            )]),
            user_msg(vec![make_tool_result("mid", "lines 500..600", false)]),
            // Protected tail.
            assistant_msg(vec![text_block("")]),
            user_msg(vec![text_block("")]),
            assistant_msg(vec![text_block("")]),
            user_msg(vec![text_block("")]),
            assistant_msg(vec![text_block("")]),
            user_msg(vec![text_block("")]),
        ];

        clear_old_tool_results(&mut messages, 6, 1);

        // Both slices survive.
        if let ContentBlock::ToolResult(tr) = &messages[1].content[0] {
            assert_eq!(tr.content, "lines 1..100");
        } else {
            panic!("expected tool result");
        }
        if let ContentBlock::ToolResult(tr) = &messages[3].content[0] {
            assert_eq!(tr.content, "lines 500..600");
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn default_config_preserves_12_turn_window() {
        // Regression lock for Option C: the default must preserve at
        // least 12 turns (protected_recent_turns + tool_result_max_age_turns).
        let cfg = ContextPruneConfig::default();
        assert_eq!(cfg.protected_recent_turns, 4);
        assert_eq!(cfg.tool_result_max_age_turns, 8);
    }

    // ── Strategy 2: deduplicate_tool_calls ──────────────────────

    #[test]
    fn dedup_removes_older_identical_tool_calls() {
        let messages = vec![
            // Turn 0: Read("a.rs") — duplicate, should be removed
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "old content", false)]),
            // Turn 1: Read("a.rs") — kept (most recent)
            assistant_msg(vec![make_tool_use(
                "t1",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t1", "new content", false)]),
            // Turn 2: final response — protected
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("thanks")]),
        ];

        let result = deduplicate_tool_calls(messages, 4);
        // Turn 0 should be removed (both messages had only the
        // duplicate tool blocks). Turn 1+ remain.
        assert_eq!(result.len(), 4);
        // First message should now be assistant with t1
        if let ContentBlock::ToolUse(tu) = &result[0].content[0] {
            assert_eq!(tu.id, "t1");
        } else {
            panic!("expected tool use t1");
        }
    }

    #[test]
    fn dedup_preserves_different_inputs() {
        let messages = vec![
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "content a", false)]),
            assistant_msg(vec![make_tool_use(
                "t1",
                "Read",
                serde_json::json!({"path": "b.rs"}),
            )]),
            user_msg(vec![make_tool_result("t1", "content b", false)]),
        ];

        let result = deduplicate_tool_calls(messages.clone(), 0);
        assert_eq!(result.len(), 4); // No dedup — different inputs
    }

    #[test]
    fn dedup_does_not_touch_protected_suffix() {
        let messages = vec![
            // Unprotected: duplicate
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "old", false)]),
            // Protected: same call — should NOT cause t0 removal from protected zone
            assistant_msg(vec![make_tool_use(
                "t1",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t1", "new", false)]),
        ];

        // boundary = 2 → first 2 messages are unprotected
        let result = deduplicate_tool_calls(messages, 2);
        // t0 should be removed (it's a dup of t1 which is in protected zone)
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, Role::Assistant);
        if let ContentBlock::ToolUse(tu) = &result[0].content[0] {
            assert_eq!(tu.id, "t1");
        }
    }

    #[test]
    fn dedup_noop_when_no_duplicates() {
        let messages = vec![
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "content", false)]),
            assistant_msg(vec![make_tool_use(
                "t1",
                "Write",
                serde_json::json!({"path": "b.rs"}),
            )]),
            user_msg(vec![make_tool_result("t1", "ok", false)]),
        ];
        let result = deduplicate_tool_calls(messages.clone(), 0);
        assert_eq!(result.len(), 4);
    }

    // ── Strategy 3: purge_error_inputs ──────────────────────────

    #[test]
    fn purge_strips_input_from_old_errored_tool_use() {
        let mut messages = vec![
            // Turn 0 (idx 0-1) — errored, old enough to be purged
            assistant_msg(vec![make_tool_use(
                "t0",
                "Shell",
                serde_json::json!({"command": "rm -rf /"}),
            )]),
            user_msg(vec![make_tool_result("t0", "permission denied", true)]),
            // Turn 1 (idx 2-3) — errored but too recent
            assistant_msg(vec![make_tool_use(
                "t1",
                "Shell",
                serde_json::json!({"command": "ls"}),
            )]),
            user_msg(vec![make_tool_result("t1", "not found", true)]),
            // Turn 2 (idx 4-5) — protected
            assistant_msg(vec![text_block("ok")]),
            user_msg(vec![text_block("next")]),
            // Turn 3 (idx 6-7) — protected
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("thanks")]),
        ];

        // boundary=4, max_age=1 → age_cutoff = 4-2 = 2
        // So only messages 0..2 are eligible for purging.
        purge_error_inputs(&mut messages, 4, 1);

        // t0 input should be stripped
        if let ContentBlock::ToolUse(tu) = &messages[0].content[0] {
            assert_eq!(tu.input, serde_json::json!({}));
        } else {
            panic!("expected tool use");
        }
        // t1 input should NOT be stripped (within max_age)
        if let ContentBlock::ToolUse(tu) = &messages[2].content[0] {
            assert_eq!(tu.input, serde_json::json!({"command": "ls"}));
        } else {
            panic!("expected tool use");
        }
    }

    #[test]
    fn purge_does_not_strip_successful_tool_inputs() {
        let mut messages = vec![
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "file contents", false)]),
        ];
        let original_input = serde_json::json!({"path": "a.rs"});
        purge_error_inputs(&mut messages, 0, 0);
        if let ContentBlock::ToolUse(tu) = &messages[0].content[0] {
            assert_eq!(tu.input, original_input);
        }
    }

    // ── Integration: ContextPruneMiddleware.prune() ─────────────

    #[test]
    fn prune_off_returns_unchanged() {
        let mw = ContextPruneMiddleware {
            inner: Arc::new(crate::mock::MockModelClient::new()),
            config: ContextPruneConfig::default(),
            level: PruneLevelHandle::new(PruneLevel::Off),
        };
        let messages = vec![
            user_msg(vec![text_block("hello")]),
            assistant_msg(vec![text_block("hi")]),
        ];
        let (result, _) = mw.prune(&messages, PruneLevel::Off, None, false);
        assert_eq!(result, messages);
    }

    #[test]
    fn prune_conservative_only_clears_tool_results() {
        let mw = ContextPruneMiddleware {
            inner: Arc::new(crate::mock::MockModelClient::new()),
            config: ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            level: PruneLevelHandle::new(PruneLevel::Conservative),
        };

        let messages = vec![
            // Old turn — should be cleared
            assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t0", "old data", false)]),
            // Old errored turn — result cleared but input NOT stripped (conservative)
            assistant_msg(vec![make_tool_use(
                "t1",
                "Shell",
                serde_json::json!({"cmd": "bad"}),
            )]),
            user_msg(vec![make_tool_result("t1", "error!", true)]),
            // Protected recent turn
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("ok")]),
        ];

        let (result, delta) = mw.prune(&messages, PruneLevel::Conservative, None, false);
        assert_eq!(result.len(), 6); // No dedup in conservative
        assert_eq!(delta.tool_results_cleared, 2);

        // Tool result cleared
        if let ContentBlock::ToolResult(tr) = &result[1].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        }
        // Error tool result also cleared
        if let ContentBlock::ToolResult(tr) = &result[3].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        }
        // But error tool input NOT stripped in conservative mode
        if let ContentBlock::ToolUse(tu) = &result[2].content[0] {
            assert_eq!(tu.input, serde_json::json!({"cmd": "bad"}));
        }
    }

    #[test]
    fn prune_aggressive_applies_all_strategies() {
        let mw = ContextPruneMiddleware {
            inner: Arc::new(crate::mock::MockModelClient::new()),
            config: ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            level: PruneLevelHandle::new(PruneLevel::Aggressive),
        };

        let messages = vec![
            // Duplicate Read("a.rs") — should be deduped
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "old version", false)]),
            // Same Read("a.rs") — kept
            assistant_msg(vec![make_tool_use(
                "t1",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t1", "new version", false)]),
            // Protected
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("ok")]),
        ];

        let (result, delta) = mw.prune(&messages, PruneLevel::Aggressive, None, false);
        // Duplicate removed → 4 messages remain (t1 pair + protected pair)
        assert_eq!(result.len(), 4);
        assert_eq!(delta.duplicates_removed, 1);
    }

    #[test]
    fn prune_with_preserve_prefix_cache_skips_mid_history_mutations() {
        // Same fixture as prune_aggressive_applies_all_strategies, but
        // run twice — once with preserve_prefix_cache=false (control,
        // mutations expected) and once with preserve_prefix_cache=true
        // (cache-stable mode, every mutation suppressed).
        let mw = ContextPruneMiddleware {
            inner: Arc::new(crate::mock::MockModelClient::new()),
            config: ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            level: PruneLevelHandle::new(PruneLevel::Aggressive),
        };

        let messages = vec![
            // Old duplicate Read — would be deduped + cleared without the
            // flag.
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t0", "old version", false)]),
            // Same Read — kept (it's the latest of its signature).
            assistant_msg(vec![make_tool_use(
                "t1",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("t1", "new version", false)]),
            // Protected tail.
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("ok")]),
        ];

        // Control: default behavior mutates history.
        let (control, control_delta) = mw.prune(&messages, PruneLevel::Aggressive, None, false);
        assert!(
            control_delta.duplicates_removed > 0 || control_delta.tool_results_cleared > 0,
            "control run must demonstrate the fixture is mutate-eligible \
             (otherwise the cache-stable assertion is trivially satisfied)"
        );
        // Sanity: control actually shrunk or rewrote something.
        assert!(control.len() < messages.len() || control != messages);

        // Cache-stable mode: every history-mutating strategy is skipped,
        // so the returned messages MUST be byte-identical to the input
        // and every delta counter MUST stay at zero.
        let (preserved, delta) = mw.prune(
            &messages,
            PruneLevel::Aggressive,
            None,
            /* preserve */ true,
        );
        assert_eq!(
            preserved, messages,
            "preserve_prefix_cache=true must not alter any message bytes"
        );
        assert_eq!(delta.tool_results_cleared, 0);
        assert_eq!(delta.thinking_cleared, 0);
        assert_eq!(delta.duplicates_removed, 0);
        assert_eq!(delta.errors_purged, 0);
        assert_eq!(delta.messages_truncated, 0);
    }

    #[test]
    fn should_preserve_prefix_cache_covers_prefix_cached_providers() {
        // OpenAI Responses: prompt_cache_key + previous_response_id delta
        // depend on a byte-stable prefix — stripping old thinking here
        // was the root cause of the ~33% cache-hit collapse on gpt-5.5.
        assert!(should_preserve_prefix_cache("openai-responses", "gpt-5.5"));
        // DeepSeek stays covered regardless of provider name.
        assert!(should_preserve_prefix_cache("anything", "deepseek-chat"));
        assert!(should_preserve_prefix_cache(
            "openai-responses",
            "deepseek-r1"
        ));
        // Anthropic-format providers (direct or relayed) hash exact
        // preceding bytes at each cache_control breakpoint; transcript
        // audits showed sliding clears re-prefilling 40-70k tokens per
        // sweep on these sessions.
        assert!(should_preserve_prefix_cache("anthropic", "claude-opus-4-8"));
        assert!(should_preserve_prefix_cache("anthropic", "gpt-5.6-sol"));
        // Chat-completions relay keeps the sliding mid-history prune.
        assert!(!should_preserve_prefix_cache("openai-compatible", "gpt-4o"));
    }

    #[test]
    fn prune_empty_messages_returns_empty() {
        let mw = ContextPruneMiddleware {
            inner: Arc::new(crate::mock::MockModelClient::new()),
            config: ContextPruneConfig::default(),
            level: PruneLevelHandle::new(PruneLevel::Aggressive),
        };
        let (result, _) = mw.prune(&[], PruneLevel::Aggressive, None, false);
        assert!(result.is_empty());
    }

    // ── Auto-compact truncation ─────────────────────────────────

    #[test]
    fn auto_compact_truncate_preserves_user_messages_and_summary() {
        let messages = vec![
            // Old turn 0 — original user prompt
            user_msg(vec![text_block("Please implement feature X")]),
            assistant_msg(vec![text_block("I'll start by reading the code")]),
            // Old turn 1
            user_msg(vec![text_block("Also handle edge case Y")]),
            assistant_msg(vec![text_block("Done with feature X, now handling Y")]),
            // Protected tail (1 turn)
            assistant_msg(vec![text_block("recent response")]),
            user_msg(vec![text_block("recent prompt")]),
        ];

        let result = auto_compact_truncate(messages, 1);
        // user messages + summary + 2 protected messages = 4
        assert_eq!(result.len(), 4);

        // First message: preserved user messages from dropped portion.
        let preserved = result[0].content[0].as_text().unwrap();
        assert!(preserved.contains("Please implement feature X"));
        assert!(preserved.contains("Also handle edge case Y"));
        assert_eq!(result[0].role, Role::User);

        // Second message: compaction summary with last assistant text.
        let summary = result[1].content[0].as_text().unwrap();
        assert!(summary.contains("CONTEXT COMPACTION"));
        assert!(summary.contains("Done with feature X, now handling Y"));

        // Protected tail kept unchanged.
        assert_eq!(result[2].content[0].as_text(), Some("recent response"));
        assert_eq!(result[3].content[0].as_text(), Some("recent prompt"));
    }

    #[test]
    fn auto_compact_truncate_noop_when_small() {
        let messages = vec![
            user_msg(vec![text_block("hello")]),
            assistant_msg(vec![text_block("hi")]),
        ];
        let result = auto_compact_truncate(messages.clone(), 2);
        // Too small to truncate — returned unchanged.
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn auto_compact_truncate_skips_cleared_tool_results() {
        let messages = vec![
            // Original prompt
            user_msg(vec![text_block("Fix the bug in auth.rs")]),
            // Tool round with cleared result
            assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t0", TOOL_RESULT_CLEARED, false)]),
            // Another tool round
            assistant_msg(vec![text_block("Found the issue")]),
            // Protected tail
            assistant_msg(vec![text_block("tail")]),
            user_msg(vec![text_block("ok")]),
        ];

        let result = auto_compact_truncate(messages, 1);
        // Preserved user messages should contain the original prompt
        // but NOT the cleared tool result placeholder.
        let preserved = result[0].content[0].as_text().unwrap();
        assert!(preserved.contains("Fix the bug in auth.rs"));
        assert!(!preserved.contains(TOOL_RESULT_CLEARED));
    }

    #[test]
    fn auto_compact_truncate_truncates_large_assistant_summary() {
        let messages = vec![
            user_msg(vec![text_block("old request")]),
            assistant_msg(vec![text_block(&"a".repeat(80_000))]),
            user_msg(vec![text_block("tail prompt")]),
            assistant_msg(vec![text_block("tail response")]),
        ];

        let result = auto_compact_truncate(messages, 1);
        let summary = result[1].content[0].as_text().unwrap();

        assert!(summary.contains("CONTEXT COMPACTION"));
        assert!(
            summary.len() < 40_000,
            "assistant handoff summary should be capped, got {} chars",
            summary.len()
        );
    }

    #[test]
    fn auto_compact_truncate_from_allows_smaller_tail() {
        let messages = vec![
            user_msg(vec![text_block("old prompt")]),
            assistant_msg(vec![text_block("old reply")]),
            user_msg(vec![text_block("recent prompt")]),
            assistant_msg(vec![text_block("recent reply")]),
            user_msg(vec![text_block("latest prompt")]),
            assistant_msg(vec![text_block("latest reply")]),
        ];

        let result = auto_compact_truncate_from(messages, 4, 1);

        assert_eq!(
            result.last().unwrap().content[0].as_text(),
            Some("latest reply")
        );
        assert_eq!(
            result[result.len() - 2].content[0].as_text(),
            Some("latest prompt")
        );
        assert!(result
            .iter()
            .skip(result.len().saturating_sub(2))
            .all(|msg| msg.content[0].as_text() != Some("recent reply")));
    }

    #[test]
    fn auto_compact_truncate_summarizes_old_execution_history() {
        let old_result = "very large old read result that must not be replayed";
        let messages = vec![
            user_msg(vec![text_block("inspect the repo")]),
            assistant_msg(vec![make_tool_use(
                "old_read",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("old_read", old_result, false)]),
            assistant_msg(vec![text_block("I read the file")]),
            user_msg(vec![text_block("tail prompt")]),
            assistant_msg(vec![text_block("tail response")]),
        ];

        let result = auto_compact_truncate(messages, 1);
        let rendered = result
            .iter()
            .flat_map(|msg| msg.content.iter())
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("EXECUTION HISTORY COMPACTION"));
        assert!(rendered.contains("1 older tool calls were summarized"));
        assert!(rendered.contains("Tool calls by name: Read=1"));
        assert!(rendered.contains("Results: 1 succeeded, 0 errored"));
        assert!(!rendered.contains(old_result));
        assert!(!result
            .iter()
            .any(|msg| msg.content.iter().any(|block| matches!(
                block,
                ContentBlock::ToolUse(_) | ContentBlock::ToolResult(_)
            ))));
    }

    #[test]
    fn execution_summary_preserves_legacy_two_pass_duplicate_id_semantics() {
        let messages = vec![
            // Results may precede the final use in malformed repaired input. The
            // batch reference first builds a right-biased ID -> name map, then
            // counts every matching result in a separate pass.
            user_msg(vec![make_tool_result("late", "early result", false)]),
            assistant_msg(vec![make_tool_use("dup", "OldName", serde_json::json!({}))]),
            user_msg(vec![
                make_tool_result("dup", "first", false),
                make_tool_result("dup", "second", true),
            ]),
            assistant_msg(vec![
                make_tool_use("dup", "NewName", serde_json::json!({})),
                make_tool_use("late", "LateUse", serde_json::json!({})),
            ]),
        ];

        let summary = summarize_complete_execution_history(&messages).unwrap();
        assert_eq!(summary.total_calls, 3);
        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.errored, 1);
        assert_eq!(summary.by_name.get("NewName"), Some(&2));
        assert_eq!(summary.by_name.get("LateUse"), Some(&1));
        assert!(!summary.by_name.contains_key("OldName"));
    }

    #[test]
    fn auto_compact_truncate_preserves_protected_tail_tool_pair_verbatim() {
        let tail_tool_use = make_tool_use(
            "recent_bash",
            "Bash",
            serde_json::json!({"command": "cargo test"}),
        );
        let tail_tool_result = make_tool_result("recent_bash", "tests passed", true);
        let messages = vec![
            assistant_msg(vec![make_tool_use(
                "old_read",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
            )]),
            user_msg(vec![make_tool_result("old_read", "old output", false)]),
            assistant_msg(vec![tail_tool_use.clone()]),
            user_msg(vec![tail_tool_result.clone()]),
        ];

        let result = auto_compact_truncate(messages, 1);
        assert!(result
            .iter()
            .any(|msg| msg.content == vec![tail_tool_use.clone()]));
        assert!(result
            .iter()
            .any(|msg| msg.content == vec![tail_tool_result.clone()]));
        assert_eq!(result[result.len() - 2].content, vec![tail_tool_use]);
        assert_eq!(result[result.len() - 1].content, vec![tail_tool_result]);
    }

    #[test]
    fn auto_compact_truncate_omits_old_thinking_from_execution_compaction() {
        let messages = vec![
            assistant_msg(vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "raw private old thinking".to_string(),
                    signature: None,
                    data: None,
                }),
                make_tool_use("old_bash", "Bash", serde_json::json!({"command": "pwd"})),
            ]),
            user_msg(vec![make_tool_result("old_bash", "pwd output", false)]),
            user_msg(vec![text_block("tail prompt")]),
            assistant_msg(vec![text_block("tail response")]),
        ];

        let result = auto_compact_truncate(messages, 1);
        let rendered = result
            .iter()
            .flat_map(|msg| msg.content.iter())
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("EXECUTION HISTORY COMPACTION"));
        assert!(!rendered.contains("raw private old thinking"));
        assert!(!result.iter().any(|msg| msg
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking(_)))));
    }

    #[test]
    fn auto_compact_truncate_keeps_cross_boundary_tool_pair_together() {
        let tool_use = make_tool_use(
            "boundary_read",
            "Read",
            serde_json::json!({"file_path": "b.rs"}),
        );
        let tool_result = make_tool_result("boundary_read", "boundary output", false);
        let messages = vec![
            user_msg(vec![text_block("old prompt")]),
            assistant_msg(vec![tool_use.clone()]),
            user_msg(vec![tool_result.clone()]),
            assistant_msg(vec![text_block("tail response")]),
        ];

        let result = auto_compact_truncate_from(messages, 2, 1);

        assert_eq!(result[result.len() - 3].content, vec![tool_use]);
        assert_eq!(result[result.len() - 2].content, vec![tool_result]);
        assert!(!result.iter().any(|msg| msg
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse(tu) if tu.id == "boundary_read"))
            && !result.iter().any(|other| other.content.iter().any(
                |block| matches!(block, ContentBlock::ToolResult(tr) if tr.tool_use_id == "boundary_read")
            ))));
    }

    #[test]
    fn auto_compact_truncate_execution_summary_counts_are_sorted() {
        let messages = vec![
            assistant_msg(vec![make_tool_use("z", "Write", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("z", "write failed", true)]),
            assistant_msg(vec![make_tool_use("a", "Bash", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("a", "bash ok", false)]),
            assistant_msg(vec![make_tool_use("r", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("r", "read ok", false)]),
            user_msg(vec![text_block("tail prompt")]),
            assistant_msg(vec![text_block("tail response")]),
        ];

        let result = auto_compact_truncate(messages, 1);
        let summary = result
            .iter()
            .flat_map(|msg| msg.content.iter())
            .filter_map(ContentBlock::as_text)
            .find(|text| text.contains("EXECUTION HISTORY COMPACTION"))
            .expect("execution summary");

        assert!(summary.contains("3 older tool calls were summarized"));
        assert!(summary.contains("Tool calls by name: Bash=1, Read=1, Write=1"));
        assert!(summary.contains("Results: 2 succeeded, 1 errored"));
    }

    #[test]
    fn auto_compact_truncate_plain_text_old_history_gets_no_execution_summary() {
        let messages = vec![
            user_msg(vec![text_block("old prompt")]),
            assistant_msg(vec![text_block("old reply")]),
            user_msg(vec![text_block("tail prompt")]),
            assistant_msg(vec![text_block("tail response")]),
        ];

        let result = auto_compact_truncate(messages, 1);
        let rendered = result
            .iter()
            .flat_map(|msg| msg.content.iter())
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("CONTEXT COMPACTION"));
        assert!(!rendered.contains("EXECUTION HISTORY COMPACTION"));
    }

    // ── Sweep ───────────────────────────────────────────────────

    #[test]
    fn sweep_clears_recent_tool_results() {
        let mut messages = vec![
            assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t0", "content a", false)]),
            assistant_msg(vec![make_tool_use("t1", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t1", "content b", false)]),
        ];
        let cleared = sweep_recent_tool_results(&mut messages, Some(1));
        assert_eq!(cleared, 1);
        // t1 cleared, t0 untouched
        if let ContentBlock::ToolResult(tr) = &messages[3].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        }
        if let ContentBlock::ToolResult(tr) = &messages[1].content[0] {
            assert_eq!(tr.content, "content a");
        }
    }

    // ── PruneStats & ContextBudget ──────────────────────────────

    #[test]
    fn prune_stats_snapshot_and_reset() {
        let stats = PruneStats::default();
        stats.tool_results_cleared.store(5, Ordering::Relaxed);
        stats.duplicates_removed.store(3, Ordering::Relaxed);
        let snap = stats.snapshot();
        assert_eq!(snap.tool_results_cleared, 5);
        assert_eq!(snap.duplicates_removed, 3);
        stats.reset();
        let snap = stats.snapshot();
        assert_eq!(snap.tool_results_cleared, 0);
    }

    #[test]
    fn context_budget_should_auto_compact() {
        let budget = ContextBudget::new(1_000_000);
        budget.report_usage(949_999);
        assert!(!budget.should_auto_compact());
        budget.report_usage(950_000);
        assert!(budget.should_auto_compact());
    }

    #[test]
    fn context_budget_should_auto_compact_for_supplied_tokens() {
        let budget = ContextBudget::new(1_000_000);
        budget.report_usage(0);

        assert!(!budget.should_auto_compact());
        assert!(!budget.should_auto_compact_for_tokens(949_999));
        assert!(budget.should_auto_compact_for_tokens(950_000));
    }

    #[test]
    fn context_budget_500k_reserves_128k_output_and_compacts_at_95_percent() {
        let budget = ContextBudget::with_output_reserve(500_000, 128_000);

        assert_eq!(budget.context_window(), 500_000);
        assert_eq!(budget.output_token_reserve(), 128_000);
        assert_eq!(budget.input_token_budget(), 372_000);
        assert_eq!(budget.auto_compact_threshold(), 353_400);
        assert!(!budget.should_auto_compact_for_tokens(353_399));
        assert!(budget.should_auto_compact_for_tokens(353_400));
    }

    #[test]
    fn context_budget_manual_compact_instructions_are_one_shot() {
        let budget = ContextBudget::new(1_000_000);
        budget.force_compact_once_with_instructions(Some("  keep test failures  ".into()));

        assert_eq!(
            budget.take_compact_once_with_instructions(),
            (true, Some("keep test failures".into()))
        );
        assert_eq!(budget.take_compact_once_with_instructions(), (false, None));
    }

    #[test]
    fn context_budget_bare_manual_compact_clears_previous_instructions() {
        let budget = ContextBudget::new(1_000_000);
        budget.force_compact_once_with_instructions(Some("keep file reads".into()));
        budget.force_compact_once();

        assert_eq!(budget.take_compact_once_with_instructions(), (true, None));
    }

    #[test]
    fn context_budget_usage_snapshot_tracks_source() {
        let budget = ContextBudget::new(1_000_000);

        assert_eq!(budget.usage_snapshot().source, ContextUsageSource::Unknown);

        budget.report_usage(12_345);
        let server = budget.usage_snapshot();
        assert_eq!(server.tokens, 12_345);
        assert_eq!(server.source, ContextUsageSource::Server);

        budget.report_estimated_usage(23_456);
        let estimated = budget.usage_snapshot();
        assert_eq!(estimated.tokens, 23_456);
        assert_eq!(estimated.source, ContextUsageSource::Estimated);
    }

    #[test]
    fn context_budget_supplied_tokens_respects_disable_and_circuit_breaker() {
        let budget = ContextBudget::new(1_000_000);
        budget.set_auto_compact_enabled(false);
        assert!(!budget.should_auto_compact_for_tokens(960_000));

        budget.set_auto_compact_enabled(true);
        for _ in 0..3 {
            budget.record_compact_failure();
        }
        assert!(!budget.should_auto_compact_for_tokens(960_000));
    }

    #[test]
    fn context_budget_disabled_does_not_trigger() {
        let budget = ContextBudget::new(1_000_000);
        budget.set_auto_compact_enabled(false);
        budget.report_usage(999_000);
        assert!(!budget.should_auto_compact());
    }

    #[test]
    fn context_budget_reset_clears_usage() {
        let budget = ContextBudget::new(1_000_000);
        budget.report_usage(960_000);
        assert!(budget.should_auto_compact());
        budget.clear_usage();
        assert!(!budget.should_auto_compact());
        let snapshot = budget.usage_snapshot();
        assert_eq!(snapshot.tokens, 0);
        assert_eq!(snapshot.source, ContextUsageSource::Unknown);
    }

    #[test]
    fn context_budget_circuit_breaker() {
        let budget = ContextBudget::new(1_000_000);
        budget.report_usage(960_000);
        assert!(budget.should_auto_compact());

        // Record MAX failures → circuit breaker trips.
        for _ in 0..3 {
            budget.record_compact_failure();
        }
        assert!(!budget.should_auto_compact());

        // A success resets the breaker.
        budget.record_compact_success();
        assert!(budget.should_auto_compact());
    }

    #[test]
    fn microcompact_threshold() {
        // Default profile: large-context providers can use their real window.
        let budget = ContextBudget::new(1_000_000);
        assert_eq!(budget.context_window(), 1_000_000);
        assert_eq!(budget.microcompact_trigger(), 686_000);
        assert_eq!(budget.microcompact_target(), 490_000);
        budget.report_usage(685_000);
        assert!(!budget.should_microcompact());
        budget.report_usage(687_000);
        assert!(budget.should_microcompact());

        // 200K window: effective = 180K, 70% = 126K.
        let budget_small = ContextBudget::new(200_000);
        assert_eq!(budget_small.microcompact_trigger(), 126_000);
        budget_small.report_usage(125_000);
        assert!(!budget_small.should_microcompact());
        budget_small.report_usage(127_000);
        assert!(budget_small.should_microcompact());
    }

    #[test]
    fn full_history_replay_microcompact_profile_caps_thresholds() {
        let budget = ContextBudget::new(1_000_000);
        budget.use_full_history_replay_microcompact_profile();

        assert_eq!(budget.microcompact_trigger(), 48_000);
        assert_eq!(budget.microcompact_target(), 32_000);
        budget.report_usage(47_000);
        assert!(!budget.should_microcompact());
        budget.report_usage(49_000);
        assert!(budget.should_microcompact());
    }

    #[test]
    fn context_budget_small_windows_do_not_zero_threshold() {
        let budget_128k = ContextBudget::new(128_000);
        assert_eq!(budget_128k.auto_compact_threshold(), 121_600);
        let budget_200k = ContextBudget::new(200_000);
        assert_eq!(budget_200k.auto_compact_threshold(), 190_000);
    }

    #[test]
    fn context_budget_usage_percent() {
        let budget = ContextBudget::new(200_000);
        budget.report_usage(100_000);
        assert_eq!(budget.usage_percent(), 50);
        budget.report_usage(200_000);
        assert_eq!(budget.usage_percent(), 100);
    }

    #[test]
    fn microcompact_unknown_tool_result_is_not_clearable() {
        let mut messages = vec![
            assistant_msg(vec![make_tool_use(
                "known",
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            )]),
            user_msg(vec![make_tool_result("known", &"x".repeat(200), false)]),
            user_msg(vec![make_tool_result(
                "missing_tool_use",
                &"y".repeat(200),
                false,
            )]),
            // Trailing assistant turn marks the prior tool_results as
            // "consumed" so they become eligible candidates.
            assistant_msg(vec![text_block("ok")]),
        ];

        let cleared = microcompact_tool_results(&mut messages, 1_000, 100, 0);

        assert_eq!(cleared, 1);
        let known_content = match &messages[1].content[0] {
            ContentBlock::ToolResult(tr) => tr.content.as_text(),
            _ => None,
        };
        let unknown_content = match &messages[2].content[0] {
            ContentBlock::ToolResult(tr) => tr.content.as_text(),
            _ => None,
        };
        assert_eq!(known_content, Some(TOOL_RESULT_CLEARED));
        assert_ne!(unknown_content, Some(TOOL_RESULT_CLEARED));
    }

    #[test]
    fn microcompact_skips_unconsumed_just_returned_tool_result() {
        // Simulates the state at the call site: the model has just emitted
        // a tool_use, the dispatcher appended a tool_result, and microcompact
        // runs before the next request. That latest tool_result has no
        // assistant turn after it yet — it must NOT be cleared, otherwise
        // the model never sees its own tool's output.
        let mut messages = vec![
            assistant_msg(vec![make_tool_use("a", "Bash", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("a", &"x".repeat(4000), false)]),
            assistant_msg(vec![make_tool_use("b", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("b", &"y".repeat(4000), false)]),
            assistant_msg(vec![make_tool_use("c", "Grep", serde_json::json!({}))]),
            // Just-returned tool_result: no assistant after it.
            user_msg(vec![make_tool_result("c", &"z".repeat(4000), false)]),
        ];

        let cleared = microcompact_tool_results(&mut messages, 10_000, 1_000, 0);

        // The just-returned "c" result is unconsumed → protected.
        let c_text = match &messages[5].content[0] {
            ContentBlock::ToolResult(tr) => tr.content.as_text().map(str::to_string),
            _ => None,
        };
        assert_ne!(c_text.as_deref(), Some(TOOL_RESULT_CLEARED));
        // Older consumed results may have been cleared.
        assert!(cleared <= 2);
    }

    #[test]
    fn microcompact_clears_newest_unprotected_first() {
        // Five consumed tool_results. With protected_recent=1 the newest
        // (idx 9) is protected. Clearing only enough to free one block
        // should hit the next-newest unprotected candidate (idx 7), not
        // the oldest (idx 1) — preserving the long byte-stable prefix.
        let mut messages = vec![
            assistant_msg(vec![make_tool_use("t1", "Bash", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t1", &"a".repeat(4000), false)]),
            assistant_msg(vec![make_tool_use("t2", "Bash", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t2", &"b".repeat(4000), false)]),
            assistant_msg(vec![make_tool_use("t3", "Bash", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t3", &"c".repeat(4000), false)]),
            assistant_msg(vec![make_tool_use("t4", "Bash", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t4", &"d".repeat(4000), false)]),
            assistant_msg(vec![make_tool_use("t5", "Bash", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t5", &"e".repeat(4000), false)]),
            // Trailing assistant marks t5 as consumed too.
            assistant_msg(vec![text_block("done")]),
        ];

        // Each block ≈ 1000 approx tokens, clearing frees ~992. Asking to
        // free 900 should clear exactly one — the newest unprotected (t4 at
        // msg_idx=7).
        let cleared = microcompact_tool_results(&mut messages, 5_000, 4_100, 1);
        assert_eq!(cleared, 1);

        let texts: Vec<Option<String>> = messages
            .iter()
            .map(|m| {
                m.content.iter().find_map(|b| match b {
                    ContentBlock::ToolResult(tr) => tr.content.as_text().map(str::to_string),
                    _ => None,
                })
            })
            .collect();

        // t1..t3 (oldest) untouched — preserves cached prefix bytes.
        assert_ne!(texts[1].as_deref(), Some(TOOL_RESULT_CLEARED));
        assert_ne!(texts[3].as_deref(), Some(TOOL_RESULT_CLEARED));
        assert_ne!(texts[5].as_deref(), Some(TOOL_RESULT_CLEARED));
        // t4 (newest unprotected) cleared.
        assert_eq!(texts[7].as_deref(), Some(TOOL_RESULT_CLEARED));
        // t5 (within protected_recent=1) untouched.
        assert_ne!(texts[9].as_deref(), Some(TOOL_RESULT_CLEARED));
    }

    #[test]
    fn microcompact_returns_zero_without_assistant_turn() {
        // Edge case: history with tool_results but no assistant message at
        // all (shouldn't happen in practice — but guard against panics).
        // Nothing is consumed → nothing is cleared.
        let mut messages = vec![user_msg(vec![make_tool_result(
            "orphan",
            &"x".repeat(4000),
            false,
        )])];
        let cleared = microcompact_tool_results(&mut messages, 10_000, 100, 0);
        assert_eq!(cleared, 0);
    }

    // ── ensure_tool_result_pairing ─────────────────────────────

    #[test]
    fn pairing_removes_globally_orphaned_tool_results() {
        // Scenario: deduplication dropped the assistant message that
        // contained tool_use "t0", but the user message with
        // tool_result "t0" survived in the protected suffix.
        let mut messages = vec![
            // No assistant message for t0 — it was removed by dedup.
            user_msg(vec![
                make_tool_result("t0", "orphaned result", false),
                text_block("some user text"),
            ]),
            assistant_msg(vec![text_block("response")]),
            user_msg(vec![text_block("ok")]),
        ];

        ensure_tool_result_pairing(&mut messages);

        // The orphaned tool_result "t0" should be removed;
        // the text block should survive.
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text(_)));
    }

    #[test]
    fn pairing_removes_orphaned_result_when_assistant_has_different_tools() {
        // Assistant has tool_use "t1", but user message has
        // tool_result for both "t0" (orphaned) and "t1" (valid).
        let mut messages = vec![
            assistant_msg(vec![make_tool_use(
                "t1",
                "Read",
                serde_json::json!({"p": "a"}),
            )]),
            user_msg(vec![
                make_tool_result("t0", "orphaned", false),
                make_tool_result("t1", "valid", false),
            ]),
        ];

        ensure_tool_result_pairing(&mut messages);

        // t0 removed globally, t1 kept.
        let result_ids: Vec<&str> = messages[1]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult(tr) => Some(tr.tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(result_ids, vec!["t1"]);
    }

    #[test]
    fn pairing_keeps_valid_pairs_intact() {
        let mut messages = vec![
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"p": "a"}),
            )]),
            user_msg(vec![make_tool_result("t0", "content", false)]),
            assistant_msg(vec![make_tool_use(
                "t1",
                "Write",
                serde_json::json!({"p": "b"}),
            )]),
            user_msg(vec![make_tool_result("t1", "ok", false)]),
        ];

        ensure_tool_result_pairing(&mut messages);

        // Nothing should change — all pairs are valid.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].content.len(), 1);
        assert_eq!(messages[3].content.len(), 1);
    }

    #[test]
    fn pairing_accepts_tool_result_after_text_only_user_reminder() {
        // Transcript-level `isMeta` reminders reach this layer as ordinary
        // user text. They must not hide the real tool_result that follows.
        let mut messages = vec![
            assistant_msg(vec![make_tool_use(
                "t0",
                "Glob",
                serde_json::json!({"pattern": "**/*.rs"}),
            )]),
            user_msg(vec![text_block("skills available: imagegen")]),
            user_msg(vec![make_tool_result("t0", "glob output", false)]),
        ];

        let report = ensure_tool_result_pairing_with_report(&mut messages);

        assert!(
            report.synthesized.is_empty(),
            "real tool_result in the following user window should prevent synthetic repair"
        );
        assert!(report.synthesized_tail_orphans.is_empty());
        assert_eq!(messages.len(), 3);
        assert!(matches!(&messages[1].content[0], ContentBlock::Text(_)));
        assert!(matches!(
            &messages[2].content[0],
            ContentBlock::ToolResult(tr) if tr.tool_use_id == "t0"
        ));
    }

    #[test]
    fn pairing_collects_split_results_across_consecutive_user_messages() {
        let mut messages = vec![
            assistant_msg(vec![
                make_tool_use("t_read", "Read", serde_json::json!({})),
                make_tool_use("t_glob", "Glob", serde_json::json!({})),
            ]),
            user_msg(vec![make_tool_result("t_read", "read output", false)]),
            user_msg(vec![text_block("skills available: imagegen")]),
            user_msg(vec![make_tool_result("t_glob", "glob output", false)]),
        ];

        let report = ensure_tool_result_pairing_with_report(&mut messages);

        assert!(report.synthesized.is_empty());
        assert_eq!(messages.len(), 4);
        let result_count = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolResult(_)))
            .count();
        assert_eq!(result_count, 2);
    }

    #[test]
    fn pairing_synthesizes_missing_results_and_removes_orphans() {
        // tool_use "t0" has no result, and there's an orphaned
        // result for "t_gone" that has no tool_use anywhere.
        let mut messages = vec![
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"p": "a"}),
            )]),
            user_msg(vec![make_tool_result("t_gone", "orphaned", false)]),
        ];

        ensure_tool_result_pairing(&mut messages);

        // Pre-pass should remove "t_gone" (globally orphaned).
        // Main pass should add synthetic result for "t0".
        let result_ids: Vec<&str> = messages[1]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult(tr) => Some(tr.tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(result_ids, vec!["t0"]);

        // The synthetic result should be an error.
        if let ContentBlock::ToolResult(tr) = &messages[1].content[0] {
            assert!(tr.is_error);
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn synthetic_placeholder_embeds_tool_name() {
        // Interrupted `Read` call should surface the tool name so the
        // model can decide whether to retry vs. move on.
        let mut messages = vec![assistant_msg(vec![make_tool_use(
            "t0",
            "Read",
            serde_json::json!({"p": "a"}),
        )])];

        ensure_tool_result_pairing(&mut messages);

        // A trailing user message with one synthetic tool_result must exist.
        assert_eq!(messages.len(), 2, "synthetic user message appended");
        let tr = match &messages[1].content[0] {
            ContentBlock::ToolResult(tr) => tr,
            _ => panic!("expected tool result"),
        };
        assert!(tr.is_error);
        assert!(
            tr.content.starts_with(SYNTHETIC_TOOL_RESULT_PREFIX),
            "synthetic content should start with the fixed prefix: {}",
            tr.content.to_plain_text()
        );
        assert!(
            tr.content.contains("Read"),
            "synthetic content should embed tool name: {}",
            tr.content.to_plain_text()
        );
    }

    #[test]
    fn synthetic_placeholder_distinguishes_multiple_tool_names() {
        // Two different interrupted tools should produce two distinct
        // placeholder strings.
        let mut messages = vec![assistant_msg(vec![
            make_tool_use("t_read", "Read", serde_json::json!({})),
            make_tool_use("t_bash", "Bash", serde_json::json!({})),
        ])];

        ensure_tool_result_pairing(&mut messages);

        let contents: Vec<&str> = messages[1]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult(tr) => Some(tr.content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(contents.len(), 2);
        assert!(contents.iter().any(|s| s.contains("Read")));
        assert!(contents.iter().any(|s| s.contains("Bash")));
    }

    #[test]
    fn synthetic_tool_result_content_handles_empty_name() {
        // Backwards-compat: a missing tool name falls back to the
        // original generic placeholder rather than emitting a
        // malformed "[Tool result missing — the `` call ...]".
        let empty = synthetic_tool_result_content("");
        assert_eq!(empty, SYNTHETIC_TOOL_RESULT_PLACEHOLDER);
        assert!(empty.starts_with(SYNTHETIC_TOOL_RESULT_PREFIX));
    }

    #[test]
    fn synthetic_tool_result_content_embeds_name_in_known_shape() {
        // Full round-trip of the formatter so callers can match the
        // shape without parsing free-form text.
        let content = synthetic_tool_result_content("Read");
        assert!(content.starts_with(SYNTHETIC_TOOL_RESULT_PREFIX));
        assert!(content.contains("`Read`"));
        assert!(content.contains("interrupted"));
    }

    #[test]
    fn pairing_report_is_empty_when_noop() {
        // Already-paired transcript — report should be empty, and
        // is_empty() must agree so callers can cheaply skip persistence.
        let mut messages = vec![
            assistant_msg(vec![make_tool_use(
                "t0",
                "Read",
                serde_json::json!({"p": "a"}),
            )]),
            user_msg(vec![make_tool_result("t0", "ok", false)]),
        ];
        let report = ensure_tool_result_pairing_with_report(&mut messages);
        assert!(report.is_empty());
        assert!(report.synthesized.is_empty());
        assert!(report.globally_orphaned_result_ids.is_empty());
        assert!(report.locally_orphaned_result_ids.is_empty());
    }

    #[test]
    fn pairing_report_lists_synthesized_with_tool_names() {
        // Missing result for t_read → synthesized entry must carry the
        // tool name so a future persister can rebuild a faithful
        // ToolResult record.
        let mut messages = vec![assistant_msg(vec![make_tool_use(
            "t_read",
            "Read",
            serde_json::json!({"p": "a"}),
        )])];
        let report = ensure_tool_result_pairing_with_report(&mut messages);
        assert_eq!(
            report.synthesized,
            vec![("t_read".to_string(), "Read".to_string())]
        );
        // Single assistant at index 0 is also the tail — so the
        // orphan is safe to persist as a tail heal.
        assert_eq!(
            report.synthesized_tail_orphans,
            vec![("t_read".to_string(), "Read".to_string())]
        );
        assert_eq!(report.mid_orphan_count(), 0);
        assert!(report.globally_orphaned_result_ids.is_empty());
        assert!(report.locally_orphaned_result_ids.is_empty());
        assert!(!report.is_empty());
    }

    /// An earlier assistant message has an orphan tool_use, but a
    /// later assistant message also has tool_uses (paired). The
    /// orphan is NOT at the tail — persisting a tail heal would not
    /// pair it up on reload, so `synthesized_tail_orphans` must be
    /// empty while `synthesized` still lists the orphan (for the
    /// in-memory repair). This is the core invariant distinguishing
    /// "fixable by tail heal" from "already-polluted history".
    #[test]
    fn pairing_report_classifies_mid_orphan_as_non_tail() {
        let mut messages = vec![
            // Orphan: no matching tool_result in the user message
            // that follows.
            assistant_msg(vec![make_tool_use(
                "t_mid",
                "Read",
                serde_json::json!({"p": "a"}),
            )]),
            // The user message that follows does not carry the
            // matching tool_result — unrelated text only.
            user_msg(vec![ContentBlock::Text(TextBlock {
                text: "something happened".into(),
            })]),
            // Later, an assistant with a properly paired tool_use —
            // this is the tail of tool_uses in the conversation.
            assistant_msg(vec![make_tool_use(
                "t_tail",
                "Bash",
                serde_json::json!({"cmd": "ls"}),
            )]),
            user_msg(vec![make_tool_result("t_tail", "ok", false)]),
        ];
        let report = ensure_tool_result_pairing_with_report(&mut messages);
        // The orphan was repaired in-memory.
        assert_eq!(
            report.synthesized,
            vec![("t_mid".to_string(), "Read".to_string())],
            "mid orphan must still be synthesized for in-memory correctness"
        );
        // But it is NOT tail-eligible — persisting a tail heal would
        // not land in the immediately-following-user slot and would
        // just accumulate noise on every resume.
        assert!(
            report.synthesized_tail_orphans.is_empty(),
            "mid orphan must not appear in the tail subset"
        );
        assert_eq!(report.mid_orphan_count(), 1);
    }

    #[test]
    fn pairing_report_lists_globally_orphaned() {
        // User message holds a tool_result whose tool_use is nowhere in
        // the transcript — pre-pass must strip it and report it under
        // globally_orphaned_result_ids (so the persister knows NOT to
        // re-append it).
        let mut messages = vec![user_msg(vec![make_tool_result("t_gone", "orphan", false)])];
        let report = ensure_tool_result_pairing_with_report(&mut messages);
        assert_eq!(
            report.globally_orphaned_result_ids,
            vec!["t_gone".to_string()]
        );
        assert!(report.synthesized.is_empty());
        assert!(report.locally_orphaned_result_ids.is_empty());
    }

    #[test]
    fn pairing_report_lists_locally_orphaned() {
        // Assistant message has one tool_use (t_real). User message has
        // a tool_result for t_real AND a stray tool_result (t_wrong)
        // whose tool_use lives elsewhere in the transcript — so it is
        // NOT globally orphaned, just locally misplaced.
        let mut messages = vec![
            assistant_msg(vec![make_tool_use("t_real", "Read", serde_json::json!({}))]),
            user_msg(vec![
                make_tool_result("t_real", "ok", false),
                make_tool_result("t_wrong", "stray", false),
            ]),
            assistant_msg(vec![make_tool_use(
                "t_wrong",
                "Bash",
                serde_json::json!({}),
            )]),
            user_msg(vec![make_tool_result("t_wrong", "ok", false)]),
        ];
        let report = ensure_tool_result_pairing_with_report(&mut messages);
        assert_eq!(
            report.locally_orphaned_result_ids,
            vec!["t_wrong".to_string()]
        );
        assert!(report.synthesized.is_empty());
        assert!(report.globally_orphaned_result_ids.is_empty());
    }

    // ── Middleware ModelClient impl ─────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn middleware_passes_pruned_messages_to_inner() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ]);

        let handle = PruneLevelHandle::new(PruneLevel::Aggressive);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 0,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            handle,
        );

        // Build a request with a duplicate tool call
        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: vec![
                assistant_msg(vec![make_tool_use(
                    "t0",
                    "Read",
                    serde_json::json!({"p": "a"}),
                )]),
                user_msg(vec![make_tool_result("t0", "old", false)]),
                assistant_msg(vec![make_tool_use(
                    "t1",
                    "Read",
                    serde_json::json!({"p": "a"}),
                )]),
                user_msg(vec![make_tool_result("t1", "new", false)]),
            ],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let msg = mw.create_message(req).await.unwrap();
        assert_eq!(msg.id, "m");

        // The inner mock should have received a pruned request with
        // the duplicate removed.
        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        // Duplicate t0 removed → only t1 pair remains.
        assert_eq!(captured[0].messages.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn middleware_off_passes_through_unchanged() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ]);

        let handle = PruneLevelHandle::new(PruneLevel::Off);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig::default(),
            handle,
        );

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: vec![
                user_msg(vec![text_block("hello")]),
                assistant_msg(vec![text_block("hi")]),
            ],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let _ = mw.create_message(req).await.unwrap();
        let captured = mock.captured_requests();
        assert_eq!(captured[0].messages.len(), 2);
    }

    #[test]
    fn fork_for_sub_agent_uses_independent_budget() {
        let parent = PruneLevelHandle::with_model_context_windows(
            PruneLevel::Conservative,
            80_000,
            [("large", 1_000_000)],
        );
        parent.report_usage(79_000);

        let child = parent.fork_for_sub_agent();
        child.set_context_window_for_model("large");
        child.report_usage(900_000);

        assert_eq!(parent.budget.context_window(), 80_000);
        assert_eq!(parent.budget.last_input_tokens(), 79_000);
        assert_eq!(child.budget.context_window(), 1_000_000);
        assert_eq!(child.budget.last_input_tokens(), 900_000);
    }

    #[test]
    fn reset_session_state_clears_budget_usage() {
        use crate::mock::MockModelClient;

        let mock: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let handle = PruneLevelHandle::new(PruneLevel::Conservative);
        handle.report_usage(970_000); // > 950k threshold
        assert!(handle.budget.should_auto_compact());

        let mw = ContextPruneMiddleware::wrap(mock, ContextPruneConfig::default(), handle.clone());
        mw.reset_session_state();

        // After reset, stale usage is cleared — auto-compact no longer fires.
        assert!(!handle.budget.should_auto_compact());
        assert_eq!(handle.budget.last_input_tokens(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn openai_compatible_pruning_preserves_full_assistant_reasoning() {
        use crate::events::StreamEvent;
        use crate::types::Usage;

        #[derive(Debug, Default)]
        struct OpenAiCompatibleCapture {
            inner: Mutex<Vec<CreateMessageRequest>>,
        }

        #[async_trait]
        impl ModelClient for OpenAiCompatibleCapture {
            fn provider_name(&self) -> &'static str {
                "openai-compatible"
            }

            async fn create_message_stream(
                &self,
                request: CreateMessageRequest,
            ) -> ModelResult<StreamEventStream> {
                self.inner.lock().expect("capture poisoned").push(request);
                let events = vec![
                    StreamEvent::MessageStart {
                        message_id: "m".into(),
                        model: "mock".into(),
                        usage: Usage::default(),
                    },
                    StreamEvent::MessageStop,
                ];
                Ok(Box::pin(futures_util::stream::iter(
                    events.into_iter().map(Ok),
                )))
            }
        }

        let capture = Arc::new(OpenAiCompatibleCapture::default());
        let mw = ContextPruneMiddleware::wrap(
            capture.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            PruneLevelHandle::new(PruneLevel::Conservative),
        );

        let mut req = CreateMessageRequest::simple("mock", "unused");
        req.messages = vec![
            assistant_msg(vec![thinking_block("full reasoning"), text_block("answer")]),
            user_msg(vec![text_block("ok")]),
            assistant_msg(vec![text_block("recent")]),
            user_msg(vec![text_block("continue")]),
        ];

        let _ = mw.create_message(req).await.unwrap();
        let captured = capture.inner.lock().expect("capture poisoned");
        let ContentBlock::Thinking(thinking) = &captured[0].messages[0].content[0] else {
            panic!("expected thinking block to survive openai-compatible pruning");
        };
        assert_eq!(thinking.thinking, "full reasoning");
    }

    #[test]
    fn fork_for_sub_agent_preserves_prune_config() {
        use crate::mock::MockModelClient;

        // Use a mock that supports forking.
        struct ForkableMock;

        #[async_trait]
        impl ModelClient for ForkableMock {
            fn provider_name(&self) -> &'static str {
                "forkable"
            }
            async fn create_message_stream(
                &self,
                _request: CreateMessageRequest,
            ) -> ModelResult<StreamEventStream> {
                unimplemented!()
            }
            fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
                Some(Arc::new(MockModelClient::new()))
            }
        }

        let inner: Arc<dyn ModelClient> = Arc::new(ForkableMock);
        let handle = PruneLevelHandle::new(PruneLevel::Aggressive);
        let mw = ContextPruneMiddleware::wrap(inner, ContextPruneConfig::default(), handle);

        let forked = mw.fork_for_sub_agent();
        assert!(forked.is_some());
    }

    // ── Strategy 0: strip_old_thinking_blocks ──────────────────

    fn thinking_block(text: &str) -> ContentBlock {
        ContentBlock::Thinking(crate::types::ThinkingBlock {
            thinking: text.to_string(),
            signature: None,
            data: None,
        })
    }

    #[test]
    fn strips_thinking_from_old_assistant_messages() {
        // 10 messages: 5 turn pairs. boundary = 6, max_age = 1 → cutoff = 4.
        // Turns 0-1 (idx 0..4) should have thinking stripped.
        let mut messages = vec![
            // Turn 0 (idx 0-1) — old, thinking should be stripped
            assistant_msg(vec![
                thinking_block("let me think about this..."),
                text_block("Here's my answer"),
            ]),
            user_msg(vec![text_block("ok")]),
            // Turn 1 (idx 2-3) — old, thinking should be stripped
            assistant_msg(vec![
                thinking_block("another reasoning chain"),
                text_block("Another answer"),
            ]),
            user_msg(vec![text_block("good")]),
            // Turn 2 (idx 4-5) — within max_age, thinking preserved
            assistant_msg(vec![
                thinking_block("recent thinking"),
                text_block("Recent answer"),
            ]),
            user_msg(vec![text_block("next")]),
            // Turn 3 (idx 6-7) — protected
            assistant_msg(vec![
                thinking_block("protected thinking"),
                text_block("Protected answer"),
            ]),
            user_msg(vec![text_block("continue")]),
            // Turn 4 (idx 8-9) — protected
            assistant_msg(vec![text_block("final")]),
            user_msg(vec![text_block("done")]),
        ];

        let cleared = strip_old_thinking_blocks(&mut messages, 6, 1);
        assert_eq!(cleared, 2);

        // Turn 0: thinking stripped, text preserved
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text(_)));

        // Turn 1: thinking stripped, text preserved
        assert_eq!(messages[2].content.len(), 1);
        assert!(matches!(&messages[2].content[0], ContentBlock::Text(_)));

        // Turn 2: thinking preserved (within max_age)
        assert_eq!(messages[4].content.len(), 2);
        assert!(matches!(&messages[4].content[0], ContentBlock::Thinking(_)));

        // Turn 3: thinking preserved (protected)
        assert_eq!(messages[6].content.len(), 2);
        assert!(matches!(&messages[6].content[0], ContentBlock::Thinking(_)));
    }

    #[test]
    fn strip_thinking_noop_when_all_recent() {
        let mut messages = vec![
            assistant_msg(vec![thinking_block("thinking"), text_block("answer")]),
            user_msg(vec![text_block("ok")]),
        ];
        let cleared = strip_old_thinking_blocks(&mut messages, 2, 1);
        assert_eq!(cleared, 0);
        assert_eq!(messages[0].content.len(), 2);
    }

    #[test]
    fn strip_thinking_ignores_user_messages() {
        // Even if a user message somehow had a thinking block,
        // strip_old_thinking_blocks only processes Assistant messages.
        let mut messages = vec![
            user_msg(vec![text_block("user text")]),
            assistant_msg(vec![text_block("response")]),
            user_msg(vec![text_block("more")]),
            assistant_msg(vec![text_block("more response")]),
            // Protected
            assistant_msg(vec![text_block("recent")]),
            user_msg(vec![text_block("recent user")]),
        ];
        let cleared = strip_old_thinking_blocks(&mut messages, 4, 1);
        assert_eq!(cleared, 0);
    }

    #[test]
    fn prune_level_handle_applies_per_model_context_limits() {
        let handle = PruneLevelHandle::with_model_context_limits(
            PruneLevel::Conservative,
            128_000,
            0,
            [("gpt-5.6-sol", 500_000), ("deepseek-v4-pro[1m]", 1_000_000)],
            [("gpt-5.6-sol", 128_000)],
        );
        assert_eq!(handle.budget.context_window(), 128_000);
        assert_eq!(handle.budget.output_token_reserve(), 0);

        handle.set_context_window_for_model("gpt-5.6-sol");
        assert_eq!(handle.budget.context_window(), 500_000);
        assert_eq!(handle.budget.output_token_reserve(), 128_000);
        assert_eq!(handle.budget.auto_compact_threshold(), 353_400);

        handle.set_context_window_for_model("deepseek-v4-pro[1m]");
        assert_eq!(handle.budget.context_window(), 1_000_000);
        assert_eq!(handle.budget.output_token_reserve(), 0);

        handle.set_context_window_for_model("deepseek-v4-flash");
        assert_eq!(handle.budget.context_window(), 128_000);
        assert_eq!(handle.budget.output_token_reserve(), 0);
    }

    #[test]
    fn context_budget_default_is_1m() {
        let budget = ContextBudget::default();
        assert_eq!(budget.context_window(), 1_000_000);
        // With no output reservation, threshold = 95% of the raw 1M window.
        assert_eq!(
            budget.auto_compact_threshold(),
            1_000_000 * AUTOCOMPACT_TRIGGER_PCT / 100
        );
    }

    #[test]
    fn set_context_window_recalculates_percentage_threshold() {
        let budget = ContextBudget::new(1_000_000);
        let initial = budget.auto_compact_threshold();
        assert_eq!(initial, 1_000_000 * AUTOCOMPACT_TRIGGER_PCT / 100);

        budget.set_context_window(500_000);
        assert_eq!(budget.context_window(), 500_000);
        assert_eq!(
            budget.auto_compact_threshold(),
            500_000 * AUTOCOMPACT_TRIGGER_PCT / 100
        );
    }

    #[test]
    fn set_context_window_handles_small_windows() {
        let budget = ContextBudget::new(1_000_000);
        budget.set_context_window(100);
        assert_eq!(budget.auto_compact_threshold(), 95);

        budget.set_context_window(400_000);
        assert_eq!(budget.auto_compact_threshold(), 380_000);
    }

    // ── Pruning preserves provider continuation state ────────────

    #[tokio::test(start_paused = true)]
    async fn middleware_updates_context_window_from_request_model() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        for _ in 0..2 {
            mock.push_turn(vec![
                StreamEvent::MessageStart {
                    message_id: "m".into(),
                    model: "mock".into(),
                    usage: Usage::default(),
                },
                StreamEvent::MessageStop,
            ]);
        }

        let handle = PruneLevelHandle::with_model_context_windows(
            PruneLevel::Conservative,
            128_000,
            [("deepseek-v4-pro[1m]", 1_000_000)],
        );
        let mw = ContextPruneMiddleware::wrap(
            mock as Arc<dyn ModelClient>,
            ContextPruneConfig::default(),
            handle.clone(),
        );

        mw.create_message(CreateMessageRequest::simple("deepseek-v4-pro[1m]", "hi"))
            .await
            .unwrap();
        assert_eq!(handle.budget.context_window(), 1_000_000);

        mw.create_message(CreateMessageRequest::simple("deepseek-v4-flash", "hi"))
            .await
            .unwrap();
        assert_eq!(handle.budget.context_window(), 128_000);
    }

    #[tokio::test(start_paused = true)]
    async fn pruning_preserves_provider_state_when_tool_results_cleared() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ]);

        let handle = PruneLevelHandle::new(PruneLevel::Conservative);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            handle,
        );

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: vec![
                // Old turn — tool result eligible for clearing
                assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
                user_msg(vec![make_tool_result(
                    "t0",
                    "big file contents here",
                    false,
                )]),
                // Protected recent turn
                assistant_msg(vec![text_block("done")]),
                user_msg(vec![text_block("ok")]),
            ],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let _ = mw.create_message(req).await.unwrap();

        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "pruning should not invalidate provider continuation state"
        );
        assert_eq!(
            mock.reset_count(),
            0,
            "should not reset provider session state on prune"
        );

        // The inner mock should see the cleared tool result.
        let captured = mock.captured_requests();
        if let ContentBlock::ToolResult(tr) = &captured[0].messages[1].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        } else {
            panic!("expected cleared tool result");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_prune_of_same_history_keeps_same_shape_without_invalidate() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        for _ in 0..2 {
            mock.push_turn(vec![
                StreamEvent::MessageStart {
                    message_id: "m".into(),
                    model: "mock".into(),
                    usage: Usage::default(),
                },
                StreamEvent::MessageStop,
            ]);
        }

        let handle = PruneLevelHandle::new(PruneLevel::Conservative);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            handle,
        );

        let messages = vec![
            assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result(
                "t0",
                "big file contents here",
                false,
            )]),
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("ok")]),
        ];
        let mut req = CreateMessageRequest::simple("mock", "unused");
        req.messages = messages.clone();

        let _ = mw.create_message(req.clone()).await.unwrap();
        let _ = mw.create_message(req).await.unwrap();

        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "re-pruning cloned history should not repeatedly invalidate provider state"
        );
        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].messages, captured[1].messages);
        if let ContentBlock::ToolResult(tr) = &messages[1].content[0] {
            assert_eq!(tr.content, "big file contents here");
        } else {
            panic!("expected original tool result");
        }
        if let ContentBlock::ToolResult(tr) = &captured[0].messages[1].content[0] {
            assert_eq!(tr.content, TOOL_RESULT_CLEARED);
        } else {
            panic!("expected pruned tool result");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pruning_preserves_provider_state_when_nothing_changed() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ]);

        let handle = PruneLevelHandle::new(PruneLevel::Conservative);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 2,
                tool_result_max_age_turns: 4,
                error_purge_age_turns: 4,
            },
            handle,
        );

        // All messages within protection window — nothing to clear.
        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: vec![
                assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
                user_msg(vec![make_tool_result("t0", "data", false)]),
                assistant_msg(vec![text_block("done")]),
                user_msg(vec![text_block("ok")]),
            ],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let _ = mw.create_message(req).await.unwrap();

        // Nothing was cleared → no invalidation.
        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "should not invalidate when nothing was pruned"
        );
        assert_eq!(
            mock.reset_count(),
            0,
            "should not reset when nothing was pruned"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pruning_preserves_provider_state_when_thinking_stripped() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ]);

        let handle = PruneLevelHandle::new(PruneLevel::Conservative);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            handle,
        );

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: vec![
                // Old turn with thinking — eligible for stripping
                assistant_msg(vec![
                    thinking_block("let me reason about this..."),
                    text_block("answer"),
                ]),
                user_msg(vec![text_block("ok")]),
                // Protected
                assistant_msg(vec![text_block("done")]),
                user_msg(vec![text_block("next")]),
            ],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let _ = mw.create_message(req).await.unwrap();

        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "thinking prune should not invalidate provider continuation state"
        );

        // Verify thinking was actually stripped from the request.
        let captured = mock.captured_requests();
        let first_assistant = &captured[0].messages[0];
        assert_eq!(
            first_assistant.content.len(),
            1,
            "thinking block should be removed"
        );
        assert_eq!(first_assistant.content[0].as_text(), Some("answer"));
    }

    #[tokio::test(start_paused = true)]
    async fn pruning_off_keeps_provider_state() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ]);

        let handle = PruneLevelHandle::new(PruneLevel::Off);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig::default(),
            handle,
        );

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: vec![
                assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
                user_msg(vec![make_tool_result("t0", "data", false)]),
            ],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let _ = mw.create_message(req).await.unwrap();
        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "PruneLevel::Off should never invalidate provider continuation state"
        );
        assert_eq!(
            mock.reset_count(),
            0,
            "PruneLevel::Off should never reset provider session state"
        );
    }

    // ── Multi-turn integration: simulates real conversation ────

    /// Simulates a multi-turn conversation through the middleware,
    /// verifying that pruning kicks in at the right point without
    /// resetting provider continuation state.
    ///
    /// Config: `protected_recent_turns=1, tool_result_max_age_turns=1`
    ///
    /// This means the combined protection window is 4 messages
    /// (2 protected + 2 max_age). After the 3rd turn, the 1st
    /// turn's tool results should be cleared without resetting the
    /// provider's continuation state.
    #[tokio::test(start_paused = true)]
    async fn multi_turn_pruning_preserves_provider_state_at_prune_boundary() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        // Pre-seed enough turns for the full test.
        for _ in 0..5 {
            mock.push_turn(vec![
                StreamEvent::MessageStart {
                    message_id: "m".into(),
                    model: "mock".into(),
                    usage: Usage::default(),
                },
                StreamEvent::MessageStop,
            ]);
        }

        let handle = PruneLevelHandle::new(PruneLevel::Conservative);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 1,
                error_purge_age_turns: 1,
            },
            handle,
        );

        // Simulate an accumulating conversation history, as
        // TurnControlPlugin would build it.
        let mut messages: Vec<Message> = Vec::new();

        // ── Turn 1 ─────────────────────────────────────────────
        messages.push(assistant_msg(vec![make_tool_use(
            "t0",
            "Read",
            serde_json::json!({"path": "big_file.rs"}),
        )]));
        messages.push(user_msg(vec![make_tool_result(
            "t0",
            "fn main() { /* 50KB of code */ }",
            false,
        )]));

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: messages.clone(),
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };
        let _ = mw.create_message(req).await.unwrap();

        // Only 2 messages, all within protection — no pruning.
        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "turn 1: nothing to prune yet"
        );
        let captured = mock.captured_requests();
        // Tool result should be untouched.
        if let ContentBlock::ToolResult(tr) = &captured[0].messages[1].content[0] {
            assert_ne!(
                tr.content, TOOL_RESULT_CLEARED,
                "turn 1: tool result should be intact"
            );
        }

        // ── Turn 2 ─────────────────────────────────────────────
        messages.push(assistant_msg(vec![make_tool_use(
            "t1",
            "Grep",
            serde_json::json!({"pattern": "error"}),
        )]));
        messages.push(user_msg(vec![make_tool_result(
            "t1",
            "line 42: handle_error()\nline 99: log_error()",
            false,
        )]));

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: messages.clone(),
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };
        let _ = mw.create_message(req).await.unwrap();

        // 4 messages. boundary=2, age_cutoff=0. No tool results at
        // index < 0, so nothing to clear.
        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "turn 2: still within window"
        );

        // ── Turn 3 ─────────────────────────────────────────────
        messages.push(assistant_msg(vec![make_tool_use(
            "t2",
            "Edit",
            serde_json::json!({"file": "big_file.rs"}),
        )]));
        messages.push(user_msg(vec![make_tool_result(
            "t2",
            "edit applied",
            false,
        )]));

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: messages.clone(),
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };
        let _ = mw.create_message(req).await.unwrap();

        // 6 messages. boundary = 6-2 = 4, age_cutoff = 4-2 = 2.
        // Messages at index 0..2 → index 1 has turn 1's tool
        // result → should be cleared.
        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "turn 3: pruning should not invalidate provider continuation state"
        );

        // Verify the inner client received the cleared version.
        let captured = mock.captured_requests();
        let turn3_req = &captured[2]; // 3rd request
        if let ContentBlock::ToolResult(tr) = &turn3_req.messages[1].content[0] {
            assert_eq!(
                tr.content, TOOL_RESULT_CLEARED,
                "turn 3: turn 1 tool result should be '[Old tool result content cleared]'"
            );
        } else {
            panic!("expected tool result at index 1");
        }
        // Turn 2 tool result should still be intact.
        if let ContentBlock::ToolResult(tr) = &turn3_req.messages[3].content[0] {
            assert_ne!(
                tr.content, TOOL_RESULT_CLEARED,
                "turn 3: turn 2 tool result should still be intact"
            );
        }

        // ── Turn 4 ─────────────────────────────────────────────
        messages.push(assistant_msg(vec![make_tool_use(
            "t3",
            "Read",
            serde_json::json!({"path": "other.rs"}),
        )]));
        messages.push(user_msg(vec![make_tool_result(
            "t3",
            "other file contents",
            false,
        )]));

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages: messages.clone(),
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };
        let _ = mw.create_message(req).await.unwrap();

        // 8 messages. boundary=6, age_cutoff=4.
        // Turn 1 and turn 2 are now old enough to be cleared in the
        // outbound clone. The middleware still leaves provider
        // continuation state intact; providers with server-side history
        // detect any baseline divergence themselves and fall back to a
        // full replay for that request.
        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "turn 4: pruning should still not invalidate provider continuation state"
        );

        // Verify turn 2 tool result now cleared too.
        let captured = mock.captured_requests();
        let turn4_req = &captured[3];
        if let ContentBlock::ToolResult(tr) = &turn4_req.messages[3].content[0] {
            assert_eq!(
                tr.content, TOOL_RESULT_CLEARED,
                "turn 4: turn 2 tool result should now be cleared"
            );
        }
    }

    #[test]
    fn context_prune_forwards_anchored_minimal_capability() {
        use crate::mock::MockModelClient;

        let mock = Arc::new(MockModelClient::new());
        mock.set_supports_anchored_minimal(true);
        let mw = ContextPruneMiddleware::wrap(
            mock as Arc<dyn ModelClient>,
            ContextPruneConfig::default(),
            PruneLevelHandle::new(PruneLevel::Conservative),
        );

        assert!(mw.supports_anchored_minimal());
    }

    /// Verifies that already-cleared tool results stay idempotent.
    #[tokio::test(start_paused = true)]
    async fn already_cleared_tool_results_remain_idempotent() {
        use crate::events::StreamEvent;
        use crate::mock::MockModelClient;
        use crate::types::Usage;

        let mock = Arc::new(MockModelClient::new());
        for _ in 0..2 {
            mock.push_turn(vec![
                StreamEvent::MessageStart {
                    message_id: "m".into(),
                    model: "mock".into(),
                    usage: Usage::default(),
                },
                StreamEvent::MessageStop,
            ]);
        }

        let handle = PruneLevelHandle::new(PruneLevel::Conservative);
        let mw = ContextPruneMiddleware::wrap(
            mock.clone() as Arc<dyn ModelClient>,
            ContextPruneConfig {
                protected_recent_turns: 1,
                tool_result_max_age_turns: 0,
                error_purge_age_turns: 0,
            },
            handle,
        );

        // Build messages where the old tool result is ALREADY cleared
        // (e.g., from a previous pruning pass that was stored).
        let messages = vec![
            assistant_msg(vec![make_tool_use("t0", "Read", serde_json::json!({}))]),
            user_msg(vec![make_tool_result("t0", TOOL_RESULT_CLEARED, false)]),
            assistant_msg(vec![text_block("done")]),
            user_msg(vec![text_block("ok")]),
        ];

        let req = CreateMessageRequest {
            model: "mock".into(),
            messages,
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };
        let _ = mw.create_message(req).await.unwrap();

        // Already cleared → no additional clearing.
        assert_eq!(
            mock.invalidate_previous_response_id_count(),
            0,
            "already-cleared tool results should not invalidate provider continuation state"
        );
    }
}
