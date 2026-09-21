use super::*;

pub(super) const HARD_CONTEXT_GUARD_BUFFER_TOKENS: u32 = 64_000;
pub(super) const HARD_CONTEXT_GUARD_PCT: u32 = 95;
pub(super) const MIN_CONTEXT_BUDGET_TOKENS: u32 = 8_000;
pub(super) const REPLAY_MAX_TAIL_MESSAGES: usize = 20;
pub(super) const REPLAY_MIN_TAIL_MESSAGES: usize = 1;
pub(super) const OVERFLOW_RETRY_MAX_TAIL_MESSAGES: usize = 8;
pub(super) const OVERFLOW_RETRY_MIN_TAIL_MESSAGES: usize = 1;

pub(super) fn hard_context_guard_target(
    handle: Option<&PruneLevelHandle>,
    max_tokens: u32,
) -> Option<u32> {
    let handle = handle?;
    let reserve = max_tokens
        .max(handle.budget.output_token_reserve())
        .max(rebon_api::FLOOR_OUTPUT_TOKENS);
    let input_budget = handle.budget.context_window().saturating_sub(reserve);
    let target = if handle.budget.output_token_reserve() > 0 {
        input_budget.saturating_mul(HARD_CONTEXT_GUARD_PCT) / 100
    } else {
        input_budget.saturating_sub(HARD_CONTEXT_GUARD_BUFFER_TOKENS)
    };
    Some(target.max(MIN_CONTEXT_BUDGET_TOKENS))
}

pub(super) fn budgeted_replay_target(
    handle: Option<&PruneLevelHandle>,
    max_tokens: u32,
) -> Option<u32> {
    let handle = handle?;
    Some(
        hard_context_guard_target(Some(handle), max_tokens)?
            .min(handle.budget.auto_compact_threshold()),
    )
}

pub(super) fn truncate_replay_window_for_budget(
    session_id: &str,
    system: Option<&str>,
    replayed: Vec<ApiMessage>,
    target_tokens: Option<u32>,
) -> Vec<ApiMessage> {
    let before = replayed.len();
    let Some(target_tokens) = target_tokens else {
        return replayed;
    };
    let (replayed, report) = truncate_messages_for_token_budget(
        system,
        replayed,
        target_tokens,
        REPLAY_MAX_TAIL_MESSAGES,
        REPLAY_MIN_TAIL_MESSAGES,
    );
    tracing::info!(
        session_id = %session_id,
        before_messages = before,
        after_messages = replayed.len(),
        before_tokens = report.before_tokens,
        after_tokens = report.after_tokens,
        target_tokens,
        "rebon-core replay window pruned for model budget"
    );
    replayed
}

pub(super) fn truncate_initial_query_history_for_request_budget(
    session_id: &str,
    params: &mut QueryParams,
) -> bool {
    let Some(request_target_tokens) =
        budgeted_replay_target(params.prune_level.as_ref(), params.max_tokens)
    else {
        return false;
    };
    let manager = ContextManager::new(params.system.clone(), params.messages.clone());
    let estimated_input_tokens = next_request_input_estimate(&manager, params);
    if estimated_input_tokens <= request_target_tokens {
        return false;
    }

    let history_target_tokens =
        history_target_for_request_budget(&manager, params, request_target_tokens);
    let min_tail =
        min_tail_messages_preserving_runtime_context(&params.messages, REPLAY_MIN_TAIL_MESSAGES);
    let (messages, report) = truncate_messages_for_token_budget(
        params.system.as_deref(),
        std::mem::take(&mut params.messages),
        history_target_tokens,
        REPLAY_MAX_TAIL_MESSAGES,
        min_tail,
    );
    params.messages = messages;
    if report.changed() {
        tracing::info!(
            session_id = %session_id,
            before_tokens = report.before_tokens,
            after_tokens = report.after_tokens,
            estimated_input_tokens,
            request_target_tokens,
            history_target_tokens,
            before_messages = report.before_messages,
            after_messages = report.after_messages,
            "rebon-core loaded transcript replay pruned with full request budget"
        );
    }
    report.changed()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompactTrigger {
    AutoPreTurn,
    Manual,
    MidTurn,
}

impl CompactTrigger {
    fn label(self) -> &'static str {
        match self {
            Self::AutoPreTurn => "pre-turn",
            Self::Manual => "manual",
            Self::MidTurn => "mid-turn",
        }
    }
}

/// Where the session-client rung sits relative to the configured ladder.
///
/// Ahead of it when replaying through the live client is *cheaper* than the
/// ladder; behind it when the ladder might not work at all. See the call site
/// for which provider gets which, and why they are not the same reason.
pub(super) enum AlignedRung {
    First(Arc<dyn CompactProvider>),
    Last(Arc<dyn CompactProvider>),
}

impl AlignedRung {
    fn first(&self) -> Option<Arc<dyn CompactProvider>> {
        match self {
            Self::First(provider) => Some(provider.clone()),
            Self::Last(_) => None,
        }
    }

    fn last(&self) -> Option<Arc<dyn CompactProvider>> {
        match self {
            Self::Last(provider) => Some(provider.clone()),
            Self::First(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct EngineCompactResult {
    pub(super) replaced_messages: bool,
    pub(super) preserved_user_tail: bool,
    pub(super) invalidated_previous_response_id: bool,
    pub(super) new_baseline_required: bool,
    pub(super) used_model: bool,
}

pub(super) async fn compact_with_tail_preservation(
    manager: &mut ContextManager,
    params: &QueryParams,
    session: &Arc<SessionHandle>,
    trigger: CompactTrigger,
    protected_turns: usize,
    manual_instructions: Option<&str>,
) -> EngineCompactResult {
    let current_user_tail = manager.pop_plain_user_tail();
    let preserved_user_tail = current_user_tail.is_some();
    let before_messages = manager.messages().to_vec();

    // Ahead of everything, where the backend serves it: OpenAI's remote
    // compaction v2. It rides the session's own client too, so it costs
    // the same cached prefix as the aligned rung below — but the
    // summarising happens server-side and comes back as one opaque item
    // instead of a summary streamed back to us token by token. Answers
    // `false` on every host that does not serve it, including the
    // Responses-format relays that would reject the trigger item.
    let remote_v2_rung = session
        .client_arc()
        .supports_remote_compaction_v2()
        .then(|| {
            Arc::new(
                rebon_api::RemoteCompactV2Provider::new(session.client_arc(), params.model.clone())
                    .with_tools(params.tools.clone())
                    .with_runtime_context(params.runtime_context_message.as_deref())
                    .with_turn_settings(
                        params.max_tokens,
                        params.thinking.clone(),
                        params.reasoning_effort,
                    ),
            ) as Arc<dyn CompactProvider>
        });

    let aligned_rung = {
        let client = session.client_arc();
        let provider_name = client.provider_name();
        let cheapest_first = rebon_api::should_preserve_prefix_cache(provider_name, &params.model)
            && (provider_name != "openai-responses" || rebon_api::is_deepseek_model(&params.model));
        let provider = Arc::new(
            rebon_api::PrefixAlignedCompactProvider::new(client, params.model.clone())
                .with_tools(params.tools.clone())
                .with_runtime_context(params.runtime_context_message.as_deref()),
        ) as Arc<dyn CompactProvider>;
        if cheapest_first {
            AlignedRung::First(provider)
        } else {
            AlignedRung::Last(provider)
        }
    };

    let used_model = if manager.is_empty() {
        false
    } else {
        compact_context_once(
            manager,
            params,
            remote_v2_rung,
            Some(aligned_rung),
            protected_turns,
            trigger.label(),
            manual_instructions,
        )
        .await
    };

    let replaced_messages = before_messages != manager.messages();
    if let Some(tail) = current_user_tail {
        manager.push_message(tail);
    }

    let invalidated_previous_response_id = replaced_messages;
    if invalidated_previous_response_id {
        session.invalidate_continuation();
    }

    EngineCompactResult {
        replaced_messages,
        preserved_user_tail,
        invalidated_previous_response_id,
        new_baseline_required: replaced_messages,
        used_model,
    }
}

pub(super) fn mid_turn_compact_guard_target(handle: &PruneLevelHandle, max_tokens: u32) -> u32 {
    let reserve = max_tokens
        .max(handle.budget.output_token_reserve())
        .max(rebon_api::FLOOR_OUTPUT_TOKENS);
    let hard_input_limit = handle.budget.context_window().saturating_sub(reserve);
    hard_input_limit.saturating_mul(92) / 100
}

pub(super) fn mid_turn_compact_allowed(
    handle: &PruneLevelHandle,
    max_tokens: u32,
    budget_tokens: u32,
) -> bool {
    budget_tokens >= mid_turn_compact_guard_target(handle, max_tokens)
}

pub(super) fn query_prune_handle(
    params: &QueryParams,
    session: &Arc<SessionHandle>,
) -> Option<PruneLevelHandle> {
    params
        .prune_level
        .clone()
        .or_else(|| session.context_prune_handle())
}

pub(super) fn collapse_tool_results_for_emergency_retry(messages: &mut [ApiMessage]) -> u32 {
    let mut cleared = 0;
    for message in messages.iter_mut() {
        if message.role != Role::User {
            continue;
        }
        for block in &mut message.content {
            let ApiContentBlock::ToolResult(result) = block else {
                continue;
            };
            if result.content.as_text() == Some(TOOL_RESULT_CLEARED) {
                continue;
            }
            result.content = ToolResultContent::text(TOOL_RESULT_CLEARED);
            cleared += 1;
        }
    }
    cleared
}

pub(super) fn shrink_context_for_overflow_retry(
    manager: &mut ContextManager,
    params: &QueryParams,
    session: &Arc<SessionHandle>,
) -> bool {
    let estimated = next_request_input_estimate(manager, params);
    let Some(target_tokens) =
        hard_context_guard_target(params.prune_level.as_ref(), params.max_tokens)
    else {
        let before_tokens = next_request_input_estimate(manager, params);
        manager.truncate_for_compact(2);
        let after_tokens = next_request_input_estimate(manager, params);
        if after_tokens < before_tokens {
            session.invalidate_continuation();
        }
        tracing::warn!(
            before_tokens,
            after_tokens,
            "turn_control: shrank context for overflow retry without configured budget"
        );
        return after_tokens < before_tokens;
    };
    let target_tokens = target_tokens
        .min(estimated.saturating_mul(70) / 100)
        .max(MIN_CONTEXT_BUDGET_TOKENS);
    let history_target_tokens = history_target_for_request_budget(manager, params, target_tokens);
    let report = manager.truncate_for_token_budget(
        history_target_tokens,
        OVERFLOW_RETRY_MAX_TAIL_MESSAGES,
        min_tail_messages_preserving_runtime_context(
            manager.messages(),
            OVERFLOW_RETRY_MIN_TAIL_MESSAGES,
        ),
    );
    if !report.changed() {
        let before_tokens = next_request_input_estimate(manager, params);
        let cleared = collapse_tool_results_for_emergency_retry(manager.messages_mut());
        let after_cleared_tokens = next_request_input_estimate(manager, params);
        if after_cleared_tokens < before_tokens {
            session.invalidate_continuation();
            tracing::warn!(
                before_tokens,
                after_tokens = after_cleared_tokens,
                cleared_tool_results = cleared,
                "turn_control: token-budget overflow retry shrink made no progress, cleared tool results as emergency fallback"
            );
        } else {
            let fallback_tail = min_tail_messages_preserving_runtime_context(
                manager.messages(),
                OVERFLOW_RETRY_MIN_TAIL_MESSAGES,
            );
            let fallback_report = manager.truncate_to_tail(fallback_tail);
            let after_tokens = next_request_input_estimate(manager, params);
            if after_tokens < before_tokens {
                session.invalidate_continuation();
            }
            tracing::warn!(
                before_tokens,
                after_tokens,
                cleared_tool_results = cleared,
                before_messages = fallback_report.before_messages,
                after_messages = fallback_report.after_messages,
                "turn_control: token-budget overflow retry shrink made no progress, kept minimal tail as emergency fallback"
            );
        }
    } else {
        session.invalidate_continuation();
        tracing::warn!(
            before_tokens = report.before_tokens,
            after_tokens = report.after_tokens,
            before_messages = report.before_messages,
            after_messages = report.after_messages,
            target_tokens = history_target_tokens,
            request_target_tokens = target_tokens,
            "turn_control: shrank context before retrying after context overflow"
        );
    }
    report_estimated_usage_after_compact(manager, params);
    true
}

pub(super) fn compact_custom_instructions_for_request(
    default_instructions: Option<&str>,
    manual_instructions: Option<&str>,
) -> Option<String> {
    let default_instructions = default_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let manual_instructions = manual_instructions.map(str::trim).filter(|s| !s.is_empty());
    match (default_instructions, manual_instructions) {
        (None, None) => None,
        (Some(default), None) => Some(default.to_string()),
        (None, Some(manual)) => Some(manual.to_string()),
        (Some(default), Some(manual)) => Some(format!(
            "{default}\n\nManual /compact instructions for this compaction:\n{manual}"
        )),
    }
}

pub(super) fn compact_result_token_reduction(
    context_manager: &ContextManager,
    params: &QueryParams,
    result: &rebon_api::CompactResult,
) -> i64 {
    let before = next_request_input_estimate(context_manager, params) as i64;
    let request_messages = messages_with_runtime_context(
        result.messages.clone(),
        params.runtime_context_message.as_deref(),
    );
    let request = CreateMessageRequest {
        messages: request_messages,
        ..build_model_request(context_manager, params)
    };
    let after = estimate_request_input_tokens(&request) as i64;
    before - after
}

pub(super) async fn run_compact_provider_once(
    provider: Arc<dyn CompactProvider>,
    context_manager: &ContextManager,
    params: &QueryParams,
    protected_turns: usize,
    log_label: &str,
    compact_custom_instructions: Option<&str>,
) -> Option<rebon_api::CompactResult> {
    match rebon_api::compact_with_retry(
        provider.as_ref(),
        context_manager.messages(),
        params.system.as_deref(),
        protected_turns,
        compact_custom_instructions,
        &params.compact_summary_options,
    )
    .await
    {
        Ok((result, attempts)) => {
            let token_delta = compact_result_token_reduction(context_manager, params, &result);
            tracing::info!(
                before = context_manager.len(),
                after = result.messages.len(),
                token_delta,
                attempts,
                label = log_label,
                "turn_control: model-based compaction succeeded"
            );
            if token_delta > 0 {
                Some(result)
            } else {
                tracing::warn!(
                    token_delta,
                    label = log_label,
                    "turn_control: model-based compaction did not reduce estimated tokens"
                );
                None
            }
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                label = log_label,
                "turn_control: model-based compaction failed"
            );
            None
        }
    }
}

// ── Manual `/compact` run immediately, outside a turn ─────────────
//
// The in-turn path above rewrites a live `ContextManager`. A `/compact`
// typed at an idle prompt has no such manager — the history only exists
// as transcript rows — so it compacts the transcript projection instead
// and installs the result as the session's replay baseline. Everything
// below is the shared vocabulary for that: how much tail survives, and
// what the run did, in the terms the UI reports it.

/// How many trailing turn-pairs an immediate `/compact` keeps verbatim.
///
/// Two round-trips is the "last response and prompt" the report names:
/// enough for the model to resume without re-reading the summary, small
/// enough that compaction actually frees context. The in-turn manual path
/// keeps 4 because it is mid-task with tool results still in flight.
pub const MANUAL_COMPACT_PROTECTED_TURNS: usize = 2;

/// Opening tag of the summary message every compact provider emits (see
/// `rebon_api::compact`'s `COMPACT_USER_SUMMARY_PREFIX`). Splitting the
/// compacted history on it separates preserved prompts / summary / tail
/// without the providers having to report that structure themselves.
pub(super) const COMPACT_SUMMARY_MARKER: &str = "<system-generated-history-summary>";

/// Separator [`rebon_api`] joins preserved user prompts with, so the count
/// of prompts that survived is recoverable from the merged message.
const COMPACT_USER_PROMPT_SEPARATOR: &str = "\n\n---\n\n";

/// How many recently-touched file paths the report lists.
const MANUAL_COMPACT_REPORT_FILES: usize = 5;

/// What one immediate `/compact` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualCompactReport {
    /// `false` when every compact provider failed and the run fell back to
    /// deterministic truncation.
    pub used_model: bool,
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: u32,
    /// Estimated size of the history the next turn will replay — the
    /// "fresh context" the report leads with.
    pub tokens_after: u32,
    pub original_request_prompts: usize,
    pub original_request_tokens: u32,
    pub summary_tokens: u32,
    pub preserved_tail_messages: usize,
    pub preserved_tail_tokens: u32,
    /// Recently-touched files, oldest first, capped at
    /// [`MANUAL_COMPACT_REPORT_FILES`].
    pub files: Vec<String>,
}

impl ManualCompactReport {
    /// Tokens the compaction freed. Saturating: a compaction that grew the
    /// history reports zero rather than wrapping.
    pub fn tokens_freed(&self) -> u32 {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

/// Describe a finished manual compaction by re-reading its own output.
///
/// The providers return only a replacement message list, so the structure
/// is recovered here: everything before the summary message is preserved
/// user prompts, everything after it is the verbatim tail. A run that
/// produced no summary (the truncation fallback) reports the whole result
/// as tail, which is exactly what it is.
pub(super) fn manual_compact_report(
    before: &[ApiMessage],
    after: &[ApiMessage],
    files: Vec<String>,
    used_model: bool,
) -> ManualCompactReport {
    let summary_index = after.iter().position(is_compact_summary_message);
    let (original, summary, tail) = match summary_index {
        Some(index) => (&after[..index], Some(&after[index]), &after[index + 1..]),
        None => (&after[..0], None, after),
    };

    ManualCompactReport {
        used_model,
        messages_before: before.len(),
        messages_after: after.len(),
        tokens_before: estimate_messages_input_tokens(None, before),
        tokens_after: estimate_messages_input_tokens(None, after),
        original_request_prompts: original.iter().map(preserved_prompt_count).sum(),
        original_request_tokens: estimate_messages_input_tokens(None, original),
        summary_tokens: summary
            .map(std::slice::from_ref)
            .map(|summary| estimate_messages_input_tokens(None, summary))
            .unwrap_or(0),
        preserved_tail_messages: tail.len(),
        preserved_tail_tokens: estimate_messages_input_tokens(None, tail),
        files,
    }
}

fn is_compact_summary_message(message: &ApiMessage) -> bool {
    message.role == Role::User
        && message
            .content
            .iter()
            .filter_map(ApiContentBlock::as_text)
            .any(|text| text.contains(COMPACT_SUMMARY_MARKER))
}

/// How many separate user prompts were merged into one preserved message.
fn preserved_prompt_count(message: &ApiMessage) -> usize {
    message
        .content
        .iter()
        .filter_map(ApiContentBlock::as_text)
        .filter(|text| !text.trim().is_empty())
        .map(|text| text.split(COMPACT_USER_PROMPT_SEPARATOR).count())
        .sum()
}

/// Files the session most recently read or wrote, oldest first.
///
/// Read off the raw transcript rather than the API projection: the raw rows
/// keep every tool call's input verbatim, including calls whose results have
/// since been pruned out of the replay window. Any `file_path`-shaped key is
/// accepted so a new file tool is listed without teaching this function
/// about it.
pub(super) fn recent_transcript_files(entries: &[rebon_session::TranscriptEntry]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for entry in entries {
        collect_tool_use_file_paths(&entry.raw, &mut seen);
    }
    // De-duplicate keeping the *last* mention, so a file touched early and
    // again at the end reads as recent work rather than ancient history.
    let mut unique: Vec<String> = Vec::new();
    for path in seen.into_iter().rev() {
        if !unique.iter().any(|existing| existing == &path) {
            unique.push(path);
        }
        if unique.len() >= MANUAL_COMPACT_REPORT_FILES {
            break;
        }
    }
    unique.reverse();
    unique
}

fn collect_tool_use_file_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_tool_use_file_paths(value, out);
            }
        }
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(input) = map.get("input").and_then(Value::as_object) {
                    for key in ["file_path", "path", "notebook_path", "filePath"] {
                        if let Some(path) = input.get(key).and_then(Value::as_str) {
                            let path = path.trim();
                            if !path.is_empty() {
                                out.push(path.to_string());
                            }
                            break;
                        }
                    }
                }
            }
            for value in map.values() {
                collect_tool_use_file_paths(value, out);
            }
        }
        _ => {}
    }
}

pub(super) async fn compact_context_once(
    context_manager: &mut ContextManager,
    params: &QueryParams,
    remote_v2_rung: Option<Arc<dyn CompactProvider>>,
    aligned_rung: Option<AlignedRung>,
    protected_turns: usize,
    log_label: &str,
    manual_instructions: Option<&str>,
) -> bool {
    let compact_custom_instructions = compact_custom_instructions_for_request(
        params.compact_custom_instructions.as_deref(),
        manual_instructions,
    );
    // Tried in order; the first rung that returns a summary wins, and only a
    // ladder that fails end to end truncates.
    let ladder = [
        remote_v2_rung,
        aligned_rung.as_ref().and_then(AlignedRung::first),
        params.compact_provider.clone(),
        params.compact_fallback_provider.clone(),
        aligned_rung.as_ref().and_then(AlignedRung::last),
    ];
    for provider in ladder.into_iter().flatten() {
        let Some(result) = run_compact_provider_once(
            provider,
            context_manager,
            params,
            protected_turns,
            log_label,
            compact_custom_instructions.as_deref(),
        )
        .await
        else {
            continue;
        };
        context_manager.replace_messages(result.messages);
        context_manager.repair_pairing();
        if let Some(handle) = &params.prune_level {
            context_manager.report_estimated_usage(handle);
        }
        return true;
    }

    let before = context_manager.len();
    context_manager.truncate_for_compact(protected_turns);
    tracing::info!(
        before,
        after = context_manager.len(),
        label = log_label,
        "turn_control: fallback truncation applied to context"
    );

    context_manager.repair_pairing();
    if let Some(handle) = &params.prune_level {
        context_manager.report_estimated_usage(handle);
    }
    false
}

#[cfg(test)]
mod ladder_tests {
    use super::*;
    use std::sync::Mutex;

    /// A rung that records that it ran and then either fails or hands back a
    /// shorter history. Anything not shorter is rejected upstream as "no
    /// reduction", so the success case returns the tail alone.
    struct Rung {
        name: &'static str,
        ran: Arc<Mutex<Vec<&'static str>>>,
        succeeds: bool,
    }

    #[async_trait::async_trait]
    impl CompactProvider for Rung {
        async fn compact(
            &self,
            messages: &[ApiMessage],
            _system: Option<&str>,
            _protected_turns: usize,
            _custom_instructions: Option<&str>,
            _summary_options: &rebon_api::CompactSummaryOptions,
        ) -> rebon_api::ModelResult<rebon_api::CompactResult> {
            self.ran.lock().expect("ran mutex poisoned").push(self.name);
            if !self.succeeds {
                // What the ChatGPT Codex backend answers on both configured
                // rungs: the compact endpoint and the fallback client alike.
                return Err(rebon_api::ModelError::BadRequest(
                    "404: {\"detail\":\"Not Found\"}".into(),
                ));
            }
            Ok(rebon_api::CompactResult {
                messages: messages.last().cloned().into_iter().collect(),
            })
        }
    }

    fn rung(
        name: &'static str,
        ran: &Arc<Mutex<Vec<&'static str>>>,
        succeeds: bool,
    ) -> Arc<dyn CompactProvider> {
        Arc::new(Rung {
            name,
            ran: ran.clone(),
            succeeds,
        })
    }

    fn history() -> Vec<ApiMessage> {
        vec![
            ApiMessage::user_text("first request, with plenty of text to shrink"),
            ApiMessage {
                role: Role::Assistant,
                content: vec![ApiContentBlock::Text(rebon_api::TextBlock {
                    text: "a long first answer that the summary will replace".into(),
                })],
            },
            ApiMessage::user_text("second request"),
            ApiMessage {
                role: Role::Assistant,
                content: vec![ApiContentBlock::Text(rebon_api::TextBlock {
                    text: "second answer".into(),
                })],
            },
        ]
    }

    fn params(
        configured: Option<Arc<dyn CompactProvider>>,
        fallback: Option<Arc<dyn CompactProvider>>,
    ) -> QueryParams {
        let mut params = QueryParams::new("test-model", Vec::new());
        params.compact_provider = configured;
        params.compact_fallback_provider = fallback;
        params
    }

    /// Server-side compaction is strictly cheaper than any rung that
    /// streams a summary back to us, so where the backend serves it,
    /// nothing else gets a turn.
    #[tokio::test]
    async fn remote_compaction_v2_runs_ahead_of_the_whole_ladder() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let params = params(
            Some(rung("configured", &ran, true)),
            Some(rung("fallback", &ran, true)),
        );
        let mut manager = ContextManager::new(None, history());

        let compacted = compact_context_once(
            &mut manager,
            &params,
            Some(rung("remote-v2", &ran, true)),
            Some(AlignedRung::First(rung("session", &ran, true))),
            1,
            "test",
            None,
        )
        .await;

        assert!(compacted);
        assert_eq!(*ran.lock().expect("ran mutex poisoned"), ["remote-v2"]);
    }

    /// And when it is not served, or fails, it costs nothing: the ladder
    /// underneath is unchanged.
    #[tokio::test]
    async fn a_failed_remote_v2_rung_falls_through_to_the_ladder() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let params = params(Some(rung("configured", &ran, true)), None);
        let mut manager = ContextManager::new(None, history());

        let compacted = compact_context_once(
            &mut manager,
            &params,
            Some(rung("remote-v2", &ran, false)),
            Some(AlignedRung::Last(rung("session", &ran, true))),
            1,
            "test",
            None,
        )
        .await;

        assert!(compacted);
        assert_eq!(
            *ran.lock().expect("ran mutex poisoned"),
            ["remote-v2", "configured"]
        );
    }

    /// The case that cost a benchmark round: both configured rungs 404 on the
    /// ChatGPT Codex backend, and before this the turn lost its whole context
    /// to truncation. The session's own client is the rung that still works.
    #[tokio::test]
    async fn the_session_rung_catches_a_configured_ladder_that_all_fails() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let params = params(
            Some(rung("configured", &ran, false)),
            Some(rung("fallback", &ran, false)),
        );
        let mut manager = ContextManager::new(None, history());

        let compacted = compact_context_once(
            &mut manager,
            &params,
            None,
            Some(AlignedRung::Last(rung("session", &ran, true))),
            1,
            "test",
            None,
        )
        .await;

        assert!(compacted, "the session rung should have produced a summary");
        assert_eq!(
            *ran.lock().expect("ran mutex poisoned"),
            ["configured", "fallback", "session"],
            "the session rung runs last, after the configured ladder"
        );
        assert_eq!(manager.len(), 1);
    }

    /// On a prefix-cached provider the same rung is there to save money, so it
    /// goes first and the configured ladder is never reached.
    #[tokio::test]
    async fn the_session_rung_runs_first_when_it_is_the_cheap_one() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let params = params(Some(rung("configured", &ran, true)), None);
        let mut manager = ContextManager::new(None, history());

        let compacted = compact_context_once(
            &mut manager,
            &params,
            None,
            Some(AlignedRung::First(rung("session", &ran, true))),
            1,
            "test",
            None,
        )
        .await;

        assert!(compacted);
        assert_eq!(*ran.lock().expect("ran mutex poisoned"), ["session"]);
    }

    /// Nothing left to try is still truncation — the change adds a rung, it
    /// does not remove the floor.
    #[tokio::test]
    async fn a_ladder_that_fails_end_to_end_still_truncates() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let params = params(Some(rung("configured", &ran, false)), None);
        let mut manager = ContextManager::new(None, history());

        let compacted = compact_context_once(
            &mut manager,
            &params,
            None,
            Some(AlignedRung::Last(rung("session", &ran, false))),
            1,
            "test",
            None,
        )
        .await;

        assert!(!compacted, "no rung summarised, so no model was used");
        assert_eq!(
            *ran.lock().expect("ran mutex poisoned"),
            ["configured", "session"]
        );
    }
}

#[cfg(test)]
mod manual_compact_tests {
    use super::*;

    fn user(text: &str) -> ApiMessage {
        ApiMessage::user_text(text)
    }

    fn assistant(text: &str) -> ApiMessage {
        ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::Text(rebon_api::TextBlock {
                text: text.to_string(),
            })],
        }
    }

    fn summary(body: &str) -> ApiMessage {
        user(&format!("{COMPACT_SUMMARY_MARKER}\n{body}\n"))
    }

    fn tool_use_entry(uuid: &str, path: &str) -> rebon_session::TranscriptEntry {
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: uuid.into(),
            parent_uuid: None,
            timestamp: None,
            raw: serde_json::json!({
                "type": "assistant",
                "uuid": uuid,
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": uuid,
                        "name": "Edit",
                        "input": { "file_path": path },
                    }],
                },
            }),
        }
    }

    #[test]
    fn the_report_splits_the_compacted_history_at_the_summary_message() {
        let before = vec![
            user("first"),
            assistant("a"),
            user("second"),
            assistant("b"),
        ];
        let after = vec![
            user("first\n\n---\n\nsecond"),
            summary("what happened"),
            user("latest"),
            assistant("latest answer"),
        ];

        let report = manual_compact_report(&before, &after, Vec::new(), true);

        assert!(report.used_model);
        assert_eq!(report.messages_before, 4);
        assert_eq!(report.messages_after, 4);
        assert_eq!(report.original_request_prompts, 2);
        assert_eq!(report.preserved_tail_messages, 2);
        assert!(report.summary_tokens > 0);
        assert!(report.original_request_tokens > 0);
        assert!(report.preserved_tail_tokens > 0);
    }

    /// The truncation fallback emits no summary message at all. Everything it
    /// kept is verbatim tail, and the report must say that rather than
    /// mislabelling the first surviving prompt as a generated summary.
    #[test]
    fn a_result_with_no_summary_message_is_reported_as_all_tail() {
        let before = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let after = vec![user("c"), assistant("d")];

        let report = manual_compact_report(&before, &after, Vec::new(), false);

        assert!(!report.used_model);
        assert_eq!(report.original_request_prompts, 0);
        assert_eq!(report.original_request_tokens, 0);
        assert_eq!(report.summary_tokens, 0);
        assert_eq!(report.preserved_tail_messages, 2);
    }

    #[test]
    fn a_compaction_that_shrank_nothing_reports_zero_freed_instead_of_wrapping() {
        let before = vec![user("short")];
        let after = vec![user("a much, much longer replacement message")];

        let report = manual_compact_report(&before, &after, Vec::new(), true);

        assert!(report.tokens_after >= report.tokens_before);
        assert_eq!(report.tokens_freed(), 0);
    }

    #[test]
    fn recent_files_are_deduplicated_to_the_last_touch_and_capped() {
        let mut entries = vec![
            tool_use_entry("t1", "src/a.rs"),
            tool_use_entry("t2", "src/b.rs"),
        ];
        for index in 0..MANUAL_COMPACT_REPORT_FILES {
            entries.push(tool_use_entry(
                &format!("later-{index}"),
                &format!("src/later-{index}.rs"),
            ));
        }
        // `src/a.rs` again at the very end: it is recent work, not history.
        entries.push(tool_use_entry("t-last", "src/a.rs"));

        let files = recent_transcript_files(&entries);

        assert_eq!(files.len(), MANUAL_COMPACT_REPORT_FILES);
        assert_eq!(files.last().unwrap(), "src/a.rs");
        assert!(!files.contains(&"src/b.rs".to_string()));
        assert!(files
            .iter()
            .all(|file| files.iter().filter(|other| *other == file).count() == 1));
    }

    #[test]
    fn file_paths_are_read_from_any_shape_of_tool_input_and_blanks_are_skipped() {
        let entries = vec![
            tool_use_entry("t1", "src/kept.rs"),
            tool_use_entry("t2", "   "),
            rebon_session::TranscriptEntry {
                entry_type: "assistant".into(),
                uuid: "t3".into(),
                parent_uuid: None,
                timestamp: None,
                raw: serde_json::json!({
                    "message": { "content": [{
                        "type": "tool_use",
                        "name": "NotebookEdit",
                        "input": { "notebook_path": "nb.ipynb" },
                    }] },
                }),
            },
            rebon_session::TranscriptEntry {
                entry_type: "assistant".into(),
                uuid: "t4".into(),
                parent_uuid: None,
                timestamp: None,
                raw: serde_json::json!({
                    "message": { "content": [{
                        "type": "text",
                        "text": "no file_path here",
                    }] },
                }),
            },
        ];

        assert_eq!(
            recent_transcript_files(&entries),
            vec!["src/kept.rs".to_string(), "nb.ipynb".to_string()]
        );
    }

    #[test]
    fn a_transcript_with_no_tool_calls_reports_no_files() {
        let entries = vec![rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u1".into(),
            parent_uuid: None,
            timestamp: None,
            raw: serde_json::json!({"type": "user", "uuid": "u1"}),
        }];

        assert!(recent_transcript_files(&entries).is_empty());
    }
}
