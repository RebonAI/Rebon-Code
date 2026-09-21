use rebon_api::{
    ContentBlock as ApiContentBlock, Message as ApiMessage, PruneLevelHandle, Role, TextBlock,
    ToolResultContent, ToolResultContentBlock,
};

use crate::context_accounting::estimate_messages_input_tokens;

/// How a tool marks a tool result whose image is a *live capture* of a
/// surface — one a later capture supersedes.
///
/// The engine keeps only the newest couple of them in history and strips the
/// rest; images a `Read` returned are files, never superseded,
/// and must never be dropped. History carries no tool names, so the marker
/// travels in the result's own leading text: a tool that opts in starts the
/// first text block of [`rebon_tool::Tool::project_result_for_model`] with
/// this prefix. `rebon-plugin-computer-use` is its one writer today, which is
/// why the bytes read as they do — its summary line opens with the tool's own
/// name.
pub const LIVE_CAPTURE_RESULT_TEXT_PREFIX: &str = "ComputerUse ";

/// How many live captures stay in history. One is the surface the model is
/// looking at now; the second lets it compare against the frame before its last
/// action.
const LIVE_CAPTURES_KEPT: usize = 2;

const SUPERSEDED_CAPTURE_SUFFIX: &str = " (image dropped — superseded by a newer capture)";

/// Anchor for mixing server-reported usage with a local estimate of
/// items appended after the last API request.
///
/// Codex-ref's accounting: `last_api_response_total_tokens` +
/// estimate(items appended after the last model-generated item).
/// That's more accurate than re-estimating the entire history each
/// turn because the server's number is ground truth for the history
/// *up to the last request* — the only things we need to estimate
/// are the local deltas (assistant response + tool_results +
/// attachments) added after we sent that request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UsageBaseline {
    /// Server-reported `input_tokens` from the most recent API
    /// response. Ground truth for the history that was on the wire.
    server_input_tokens: u32,
    /// `self.messages.len()` at the moment the baseline was set.
    /// Items at or after this index were appended locally after the
    /// request that produced `server_input_tokens` went out (the
    /// assistant response + subsequent tool_results / attachments /
    /// iteration warnings). Must be estimated on top.
    message_index_after_request: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenBudgetPruneReport {
    pub(crate) before_tokens: u32,
    pub(crate) after_tokens: u32,
    pub(crate) before_messages: usize,
    pub(crate) after_messages: usize,
}

impl TokenBudgetPruneReport {
    pub(crate) fn changed(&self) -> bool {
        self.after_tokens < self.before_tokens || self.after_messages < self.before_messages
    }
}

pub(crate) fn truncate_messages_for_token_budget(
    system: Option<&str>,
    messages: Vec<ApiMessage>,
    target_tokens: u32,
    max_tail_messages: usize,
    min_tail_messages: usize,
) -> (Vec<ApiMessage>, TokenBudgetPruneReport) {
    let before_tokens = estimate_messages_input_tokens(system, &messages);
    let before_messages = messages.len();
    if before_tokens <= target_tokens || messages.len() <= 1 {
        return (
            messages,
            TokenBudgetPruneReport {
                before_tokens,
                after_tokens: before_tokens,
                before_messages,
                after_messages: before_messages,
            },
        );
    }

    let max_tail_messages = max_tail_messages.min(messages.len().saturating_sub(1));
    let min_tail_messages = min_tail_messages.max(1).min(max_tail_messages);
    if max_tail_messages == 0 || min_tail_messages > max_tail_messages {
        return (
            messages,
            TokenBudgetPruneReport {
                before_tokens,
                after_tokens: before_tokens,
                before_messages,
                after_messages: before_messages,
            },
        );
    }

    let mut best_messages = messages.clone();
    let mut best_tokens = before_tokens;
    let mut best_len = before_messages;

    for tail_messages in (min_tail_messages..=max_tail_messages).rev() {
        let keep_from = messages.len().saturating_sub(tail_messages);
        if keep_from == 0 {
            continue;
        }
        let protected_turns_for_label = (tail_messages + 1) / 2;
        let candidate = rebon_api::auto_compact_truncate_from(
            messages.clone(),
            keep_from,
            protected_turns_for_label,
        );
        let candidate_tokens = estimate_messages_input_tokens(system, &candidate);
        let candidate_len = candidate.len();
        if candidate_tokens < best_tokens {
            best_len = candidate_len;
            best_tokens = candidate_tokens;
            best_messages = candidate.clone();
        }
        if candidate_tokens <= target_tokens {
            return (
                candidate,
                TokenBudgetPruneReport {
                    before_tokens,
                    after_tokens: candidate_tokens,
                    before_messages,
                    after_messages: candidate_len,
                },
            );
        }
    }

    if best_tokens < before_tokens {
        let report = TokenBudgetPruneReport {
            before_tokens,
            after_tokens: best_tokens,
            before_messages,
            after_messages: best_len,
        };
        (best_messages, report)
    } else {
        (
            messages,
            TokenBudgetPruneReport {
                before_tokens,
                after_tokens: before_tokens,
                before_messages,
                after_messages: before_messages,
            },
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ContextManager {
    system: Option<String>,
    messages: Vec<ApiMessage>,
    usage_baseline: Option<UsageBaseline>,
}

impl ContextManager {
    pub(crate) fn new(system: Option<String>, messages: Vec<ApiMessage>) -> Self {
        Self {
            system,
            messages,
            usage_baseline: None,
        }
    }

    pub(crate) fn messages(&self) -> &[ApiMessage] {
        &self.messages
    }

    pub(crate) fn messages_mut(&mut self) -> &mut [ApiMessage] {
        &mut self.messages
    }

    pub(crate) fn replace_messages(&mut self, messages: Vec<ApiMessage>) {
        self.messages = messages;
        // A wholesale replace (compact summary, context reset) makes
        // the old server_input_tokens meaningless — the history it
        // was measuring no longer exists.
        self.usage_baseline = None;
    }

    pub(crate) fn push_message(&mut self, message: ApiMessage) {
        self.messages.push(message);
    }

    pub(crate) fn push_tool_results(&mut self, content: Vec<ApiContentBlock>) {
        if content.is_empty() {
            return;
        }
        self.messages.push(ApiMessage {
            role: Role::User,
            content,
        });
        self.drop_superseded_captures();
    }

    /// Strip the image from every live-capture tool result but the newest
    /// [`LIVE_CAPTURES_KEPT`].
    ///
    /// Each action answers with a full capture of the target surface, so a
    /// twenty-step session would otherwise carry twenty full-size images in
    /// every subsequent request — the context and the bill grow with the number
    /// of clicks. Only the latest frames describe the surface the model is about
    /// to act on; the older ones are replaced by their own summary line, which
    /// keeps the turn-by-turn narrative intact.
    ///
    /// Idempotent: an already-stripped result has no image left to find.
    fn drop_superseded_captures(&mut self) {
        let mut kept = 0usize;
        for message in self.messages.iter_mut().rev() {
            for block in message.content.iter_mut().rev() {
                let ApiContentBlock::ToolResult(result) = block else {
                    continue;
                };
                let ToolResultContent::Blocks(blocks) = &mut result.content else {
                    continue;
                };
                let is_live_capture = blocks.iter().any(|block| match block {
                    ToolResultContentBlock::Text(text) => {
                        text.text.starts_with(LIVE_CAPTURE_RESULT_TEXT_PREFIX)
                    }
                    _ => false,
                });
                if !is_live_capture
                    || !blocks
                        .iter()
                        .any(|block| matches!(block, ToolResultContentBlock::Image(_)))
                {
                    continue;
                }
                kept += 1;
                if kept <= LIVE_CAPTURES_KEPT {
                    continue;
                }
                blocks.retain(|block| !matches!(block, ToolResultContentBlock::Image(_)));
                if let Some(ToolResultContentBlock::Text(text)) = blocks.first_mut() {
                    text.text.push_str(SUPERSEDED_CAPTURE_SUFFIX);
                }
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub(crate) fn estimate_input_tokens(&self) -> u32 {
        estimate_messages_input_tokens(self.system.as_deref(), &self.messages)
    }

    /// Capture the server's `input_tokens` for the response we are
    /// about to process. MUST be called immediately before pushing
    /// the assistant message produced by that response — the stored
    /// index is `self.messages.len()` at the moment of this call, so
    /// later [`next_request_estimate`] can anchor its delta walk at
    /// the assistant response the server just returned.
    pub(crate) fn set_usage_baseline(&mut self, server_input_tokens: u32) {
        if server_input_tokens == 0 {
            // Don't replace a valid baseline with a zero reading —
            // mocked providers / interrupted streams can leave
            // `usage.input_tokens` unset, and re-estimating the full
            // history would be strictly worse than keeping the last
            // real anchor.
            return;
        }
        self.usage_baseline = Some(UsageBaseline {
            server_input_tokens,
            message_index_after_request: self.messages.len(),
        });
    }

    /// Estimated `input_tokens` cost of the NEXT API request.
    ///
    /// Prefers `server_input_tokens` + local estimate of items
    /// appended since that request was sent. Falls back to a full
    /// estimate when no baseline is available (cold start, post-
    /// compact, post-truncate).
    pub(crate) fn next_request_estimate(&self) -> u32 {
        match self.usage_baseline {
            Some(baseline) if baseline.message_index_after_request <= self.messages.len() => {
                let tail = &self.messages[baseline.message_index_after_request..];
                let tail_tokens = estimate_messages_input_tokens(None, tail);
                baseline.server_input_tokens.saturating_add(tail_tokens)
            }
            _ => self.estimate_input_tokens(),
        }
    }

    pub(crate) fn report_estimated_usage(&self, handle: &PruneLevelHandle) {
        let estimated = self.next_request_estimate();
        handle.report_estimated_usage(estimated);
        tracing::debug!(
            estimated_input_tokens = estimated,
            has_baseline = self.usage_baseline.is_some(),
            "context manager: recomputed estimated context usage"
        );
    }

    pub(crate) fn repair_pairing(&mut self) {
        rebon_api::ensure_tool_result_pairing(&mut self.messages);
    }

    pub(crate) fn pop_plain_user_tail(&mut self) -> Option<ApiMessage> {
        let is_plain_user = self.messages.last().is_some_and(|last| {
            last.role == Role::User
                && last
                    .content
                    .iter()
                    .all(|block| !matches!(block, ApiContentBlock::ToolResult(_)))
        });
        if is_plain_user {
            self.messages.pop()
        } else {
            None
        }
    }

    pub(crate) fn truncate_for_compact(&mut self, protected_turns: usize) {
        self.messages =
            rebon_api::auto_compact_truncate(std::mem::take(&mut self.messages), protected_turns);
        // Truncation can drop the message the server_input_tokens
        // baseline was anchored at; safer to drop the baseline than
        // to silently report a stale accumulated total.
        self.usage_baseline = None;
    }

    pub(crate) fn truncate_for_token_budget(
        &mut self,
        target_tokens: u32,
        max_tail_messages: usize,
        min_tail_messages: usize,
    ) -> TokenBudgetPruneReport {
        let (messages, report) = truncate_messages_for_token_budget(
            self.system.as_deref(),
            std::mem::take(&mut self.messages),
            target_tokens,
            max_tail_messages,
            min_tail_messages,
        );
        self.messages = messages;
        if report.changed() {
            self.usage_baseline = None;
        }
        report
    }

    pub(crate) fn truncate_to_tail(&mut self, tail_messages: usize) -> TokenBudgetPruneReport {
        let before_tokens = self.estimate_input_tokens();
        let before_messages = self.messages.len();
        if self.messages.len() <= 1 {
            return TokenBudgetPruneReport {
                before_tokens,
                after_tokens: before_tokens,
                before_messages,
                after_messages: before_messages,
            };
        }

        let tail_messages = tail_messages.max(1).min(self.messages.len());
        let keep_from = self.messages.len().saturating_sub(tail_messages);
        if keep_from == 0 {
            return TokenBudgetPruneReport {
                before_tokens,
                after_tokens: before_tokens,
                before_messages,
                after_messages: before_messages,
            };
        }

        self.messages = self.messages[keep_from..].to_vec();
        rebon_api::ensure_tool_result_pairing(&mut self.messages);
        self.usage_baseline = None;
        let after_tokens = self.estimate_input_tokens();
        TokenBudgetPruneReport {
            before_tokens,
            after_tokens,
            before_messages,
            after_messages: self.messages.len(),
        }
    }

    /// Append a user-role text block, merging into the trailing user
    /// message when one already exists. Matches the free
    /// `append_user_text` helper: after compact,
    /// `ensure_tool_result_pairing` often synthesises a trailing user
    /// turn carrying tool_result blocks; pushing a second user message
    /// for the continuation notice would create two consecutive User
    /// turns, which providers reject or collapse.
    pub(crate) fn append_user_text(&mut self, text: &str) {
        let block = ApiContentBlock::Text(TextBlock {
            text: text.to_string(),
        });
        if let Some(last) = self.messages.last_mut() {
            if last.role == Role::User {
                last.content.push(block);
                return;
            }
        }
        self.messages.push(ApiMessage {
            role: Role::User,
            content: vec![block],
        });
    }

    pub(crate) fn microcompact_tool_results(
        &mut self,
        current_tokens: u32,
        target_tokens: u32,
        protected_recent: usize,
    ) -> u32 {
        rebon_api::microcompact_tool_results(
            &mut self.messages,
            current_tokens,
            target_tokens,
            protected_recent,
        )
    }

    /// Clone the current history and apply `ensure_tool_result_pairing`
    /// so the request seen by the provider never contains an orphan
    /// tool_use. The manager's internal state is left untouched: the
    /// synthetic pairing is only applied to the outbound copy.
    pub(crate) fn messages_for_request(&self) -> Vec<ApiMessage> {
        let mut cloned = self.messages.clone();
        rebon_api::ensure_tool_result_pairing(&mut cloned);
        cloned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{TextBlock, ToolResultBlock, ToolUseBlock};
    use serde_json::json;

    fn user_text(text: &str) -> ApiMessage {
        ApiMessage {
            role: Role::User,
            content: vec![ApiContentBlock::Text(TextBlock { text: text.into() })],
        }
    }

    fn assistant_text(text: &str) -> ApiMessage {
        ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::Text(TextBlock { text: text.into() })],
        }
    }

    fn live_capture_result(id: &str, action: &str) -> ApiContentBlock {
        ApiContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: id.into(),
            content: ToolResultContent::blocks(vec![
                ToolResultContentBlock::Text(TextBlock {
                    text: format!(
                        "{LIVE_CAPTURE_RESULT_TEXT_PREFIX}{action} completed; screenshot 800x600."
                    ),
                }),
                ToolResultContentBlock::Image(rebon_api::ImageBlock::base64("image/png", "cG5n")),
            ]),
            is_error: false,
        })
    }

    fn screenshot_count(manager: &ContextManager) -> usize {
        manager
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter(|block| match block {
                ApiContentBlock::ToolResult(result) => match &result.content {
                    ToolResultContent::Blocks(blocks) => blocks
                        .iter()
                        .any(|block| matches!(block, ToolResultContentBlock::Image(_))),
                    ToolResultContent::Text(_) => false,
                },
                _ => false,
            })
            .count()
    }

    #[test]
    fn only_the_newest_live_captures_stay_in_history() {
        let mut manager = ContextManager::new(None, Vec::new());
        for index in 0..5 {
            manager.push_tool_results(vec![live_capture_result(&format!("call-{index}"), "click")]);
        }

        assert_eq!(screenshot_count(&manager), LIVE_CAPTURES_KEPT);
        // The summary line survives so the model still sees what each earlier
        // action did, and says why the image is gone.
        let ApiContentBlock::ToolResult(first) = &manager.messages[0].content[0] else {
            panic!("expected a tool result");
        };
        let ToolResultContent::Blocks(blocks) = &first.content else {
            panic!("expected block content");
        };
        assert_eq!(blocks.len(), 1);
        let ToolResultContentBlock::Text(text) = &blocks[0] else {
            panic!("expected the summary text");
        };
        assert!(text.text.contains("click completed"));
        assert!(text.text.ends_with(SUPERSEDED_CAPTURE_SUFFIX));
        // Idempotent: a further round must not append a second marker.
        manager.push_tool_results(vec![live_capture_result("call-5", "type")]);
        let ApiContentBlock::ToolResult(first) = &manager.messages[0].content[0] else {
            panic!("expected a tool result");
        };
        let ToolResultContent::Blocks(blocks) = &first.content else {
            panic!("expected block content");
        };
        let ToolResultContentBlock::Text(text) = &blocks[0] else {
            panic!("expected the summary text");
        };
        assert_eq!(text.text.matches(SUPERSEDED_CAPTURE_SUFFIX).count(), 1);
    }

    #[test]
    fn non_capture_images_are_never_dropped() {
        let mut manager = ContextManager::new(None, Vec::new());
        let read_image = ApiContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: "read-1".into(),
            content: ToolResultContent::blocks(vec![
                ToolResultContentBlock::Text(TextBlock {
                    text: "Read diagram.png".into(),
                }),
                ToolResultContentBlock::Image(rebon_api::ImageBlock::base64("image/png", "cG5n")),
            ]),
            is_error: false,
        });
        manager.push_tool_results(vec![read_image]);
        for index in 0..4 {
            manager.push_tool_results(vec![live_capture_result(&format!("call-{index}"), "click")]);
        }

        // Two live captures plus the `Read` image the user actually asked for.
        assert_eq!(screenshot_count(&manager), LIVE_CAPTURES_KEPT + 1);
    }

    fn assistant_tool_use(id: &str, name: &str) -> ApiMessage {
        ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::ToolUse(ToolUseBlock {
                id: id.into(),
                name: name.into(),
                input: json!({}),
            })],
        }
    }

    #[test]
    fn context_manager_pops_plain_user_tail() {
        let mut manager = ContextManager::new(None, vec![user_text("old"), user_text("current")]);

        let tail = manager.pop_plain_user_tail().expect("plain user tail");

        assert_eq!(manager.len(), 1);
        assert_eq!(tail.role, Role::User);
    }

    #[test]
    fn context_manager_keeps_tool_result_tail() {
        let mut manager = ContextManager::new(
            None,
            vec![ApiMessage {
                role: Role::User,
                content: vec![ApiContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: "toolu_1".into(),
                    content: "ok".into(),
                    is_error: false,
                })],
            }],
        );

        assert!(manager.pop_plain_user_tail().is_none());
        assert_eq!(manager.len(), 1);
    }

    #[test]
    fn context_manager_keeps_non_user_tail() {
        let mut manager = ContextManager::new(None, vec![assistant_text("assistant tail")]);

        assert!(manager.pop_plain_user_tail().is_none());
        assert_eq!(manager.len(), 1);
        assert_eq!(manager.messages()[0].role, Role::Assistant);
    }

    #[test]
    fn context_manager_pushes_tool_results_as_user_turn() {
        let mut manager = ContextManager::new(None, Vec::new());

        manager.push_tool_results(vec![ApiContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: "toolu_1".into(),
            content: "ok".into(),
            is_error: false,
        })]);

        assert_eq!(manager.len(), 1);
        assert_eq!(manager.messages()[0].role, Role::User);
        assert!(matches!(
            manager.messages()[0].content[0],
            ApiContentBlock::ToolResult(_)
        ));
    }

    #[test]
    fn context_manager_ignores_empty_tool_result_turn() {
        let mut manager = ContextManager::new(None, Vec::new());

        manager.push_tool_results(Vec::new());

        assert!(manager.is_empty());
    }

    #[test]
    fn context_manager_reports_estimated_usage() {
        let handle = rebon_api::PruneLevelHandle::with_context_window(
            rebon_api::PruneLevel::Conservative,
            1_000_000,
        );
        let manager = ContextManager::new(None, vec![user_text(&"x".repeat(4_000))]);

        manager.report_estimated_usage(&handle);

        assert!(handle.budget.last_input_tokens() >= 1_000);
    }

    #[test]
    fn context_manager_push_message_preserves_order_and_role() {
        let mut manager = ContextManager::new(None, vec![user_text("prompt")]);

        manager.push_message(assistant_text("reply"));
        manager.push_message(user_text("follow-up"));

        let msgs = manager.messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[2].role, Role::User);
    }

    #[test]
    fn context_manager_repair_pairing_synthesises_missing_tool_result() {
        let mut manager = ContextManager::new(
            None,
            vec![user_text("go"), assistant_tool_use("toolu_1", "Read")],
        );

        manager.repair_pairing();

        let msgs = manager.messages();
        // Trailing assistant tool_use must be followed by a synthetic
        // user message carrying a tool_result for tool_use_id toolu_1.
        assert!(msgs.len() >= 3);
        let last = msgs.last().expect("synthetic tool_result turn");
        assert_eq!(last.role, Role::User);
        let has_result = last.content.iter().any(
            |block| matches!(block, ApiContentBlock::ToolResult(tr) if tr.tool_use_id == "toolu_1"),
        );
        assert!(has_result, "expected synthesised tool_result for toolu_1");
    }

    #[test]
    fn context_manager_repair_pairing_is_idempotent_when_valid() {
        let mut manager = ContextManager::new(
            None,
            vec![
                user_text("go"),
                assistant_tool_use("toolu_1", "Read"),
                ApiMessage {
                    role: Role::User,
                    content: vec![ApiContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: "toolu_1".into(),
                        content: "ok".into(),
                        is_error: false,
                    })],
                },
            ],
        );

        let before = manager.messages().to_vec();
        manager.repair_pairing();
        assert_eq!(manager.messages(), before.as_slice());
    }

    #[test]
    fn context_manager_truncate_for_compact_keeps_protected_tail() {
        let mut messages = vec![user_text("oldest")];
        for i in 0..20 {
            messages.push(user_text(&format!("turn-{i}")));
            messages.push(assistant_text(&format!("reply-{i}")));
        }
        let before = messages.len();
        let mut manager = ContextManager::new(None, messages);

        manager.truncate_for_compact(/* protected_turns */ 4);

        // The last few turns must survive; the head must be trimmed.
        assert!(manager.len() <= before);
        let last = manager.messages().last().expect("keeps tail");
        assert!(matches!(
            &last.content[0],
            ApiContentBlock::Text(TextBlock { text }) if text == "reply-19"
        ));
    }

    #[test]
    fn context_manager_replace_messages_overwrites_state() {
        let mut manager = ContextManager::new(None, vec![user_text("a"), user_text("b")]);

        manager.replace_messages(vec![user_text("c")]);

        assert_eq!(manager.len(), 1);
        assert!(matches!(
            &manager.messages()[0].content[0],
            ApiContentBlock::Text(TextBlock { text }) if text == "c"
        ));
    }

    #[test]
    fn truncate_messages_for_token_budget_shrinks_recent_tail() {
        let mut messages = vec![user_text("old")];
        for i in 0..6 {
            messages.push(assistant_text(&format!("reply-{i}")));
            messages.push(user_text(&"x".repeat(20_000)));
        }
        let before_len = messages.len();

        let (pruned, report) = truncate_messages_for_token_budget(
            None, messages, 8_000, /* max_tail_messages */ 10, /* min_tail_messages */ 1,
        );

        assert!(report.before_tokens > report.after_tokens);
        assert!(report.after_messages < before_len);
        assert_eq!(pruned.last().unwrap().role, Role::User);
    }

    #[test]
    fn context_manager_truncate_for_token_budget_invalidates_baseline() {
        let mut manager = ContextManager::new(
            None,
            vec![
                user_text("old"),
                assistant_text(&"x".repeat(80_000)),
                user_text("tail"),
            ],
        );
        manager.set_usage_baseline(50_000);

        let report = manager.truncate_for_token_budget(
            4_000, /* max_tail_messages */ 2, /* min_tail_messages */ 1,
        );

        assert!(report.changed());
        assert!(manager.usage_baseline.is_none());
    }

    #[test]
    fn context_manager_estimate_accounts_for_system_prompt() {
        let with_system = ContextManager::new(Some("x".repeat(8_000)), vec![user_text("hello")]);
        let without_system = ContextManager::new(None, vec![user_text("hello")]);

        assert!(with_system.estimate_input_tokens() > without_system.estimate_input_tokens());
    }

    #[test]
    fn context_manager_append_user_text_merges_into_trailing_user() {
        let mut manager = ContextManager::new(None, vec![user_text("prior")]);

        manager.append_user_text("extra");

        assert_eq!(manager.len(), 1);
        let msg = &manager.messages()[0];
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(
            &msg.content[1],
            ApiContentBlock::Text(TextBlock { text }) if text == "extra"
        ));
    }

    #[test]
    fn context_manager_append_user_text_creates_new_turn_after_assistant() {
        let mut manager =
            ContextManager::new(None, vec![user_text("prompt"), assistant_text("reply")]);

        manager.append_user_text("continue");

        assert_eq!(manager.len(), 3);
        let last = manager.messages().last().expect("new user turn");
        assert_eq!(last.role, Role::User);
        assert!(matches!(
            &last.content[0],
            ApiContentBlock::Text(TextBlock { text }) if text == "continue"
        ));
    }

    #[test]
    fn context_manager_append_user_text_pushes_when_history_empty() {
        let mut manager = ContextManager::new(None, Vec::new());

        manager.append_user_text("first prompt");

        assert_eq!(manager.len(), 1);
        let only = &manager.messages()[0];
        assert_eq!(only.role, Role::User);
        assert!(matches!(
            &only.content[0],
            ApiContentBlock::Text(TextBlock { text }) if text == "first prompt"
        ));
    }

    #[test]
    fn context_manager_messages_for_request_pairs_without_mutating() {
        let manager = ContextManager::new(
            None,
            vec![user_text("go"), assistant_tool_use("toolu_1", "Read")],
        );

        let before_len = manager.len();
        let prepared = manager.messages_for_request();

        // Prepared request carries the synthetic tool_result, but the
        // manager's in-memory state is untouched.
        assert!(prepared.len() > before_len);
        assert_eq!(manager.len(), before_len);
        let last = prepared.last().expect("synthetic pairing turn");
        assert!(last.content.iter().any(|block| matches!(
            block,
            ApiContentBlock::ToolResult(tr) if tr.tool_use_id == "toolu_1"
        )));
    }

    // ── UsageBaseline ────────────────────────────────────────────────
    //
    // The baseline anchors the server's `input_tokens` at the history
    // index the server saw, so the next request's estimate is
    // `server_input_tokens + estimate(items appended after that)`. The
    // tests below pin that contract across the events that can make
    // the anchor stale (compact / truncate / replace) and the events
    // that must not (microcompact, a zero reading from a mock provider).

    fn assistant_long(n: usize) -> ApiMessage {
        assistant_text(&"y".repeat(n))
    }

    #[test]
    fn usage_baseline_cold_start_falls_back_to_full_estimate() {
        // No baseline set yet — the first request of a session must
        // still produce a sane budget number, so we fall back to a
        // full local estimate of the history.
        let manager = ContextManager::new(None, vec![user_text(&"x".repeat(4_000))]);
        assert!(manager.usage_baseline.is_none());

        let estimate = manager.next_request_estimate();
        assert_eq!(estimate, manager.estimate_input_tokens());
        assert!(estimate > 0);
    }

    #[test]
    fn usage_baseline_adds_tail_delta_to_server_tokens() {
        let mut manager = ContextManager::new(None, vec![user_text("hi")]);

        // Simulate: server replied with `input_tokens = 12_345`
        // describing the history it saw (just the single user turn),
        // and we're about to push the assistant response.
        manager.set_usage_baseline(12_345);
        let before_push = manager.next_request_estimate();
        assert_eq!(
            before_push, 12_345,
            "no tail yet → baseline is the estimate"
        );

        // Push the assistant response. Now the next request will send
        // [user, assistant] and — eventually — any tool_results we
        // append. The delta must show up on top of the server anchor.
        manager.push_message(assistant_long(8_000));
        let with_tail = manager.next_request_estimate();
        assert!(
            with_tail > before_push,
            "tail additions must inflate the estimate (got {with_tail}, baseline {before_push})"
        );
    }

    #[test]
    fn usage_baseline_zero_reading_is_ignored() {
        // Mocked providers / interrupted streams hand us
        // `usage.input_tokens == 0`. Clobbering a valid baseline with
        // that would make the next estimate drop off a cliff; instead
        // we keep the last real anchor.
        let mut manager = ContextManager::new(None, vec![user_text("hi")]);
        manager.set_usage_baseline(5_000);
        manager.push_message(assistant_text("a"));
        let with_real_baseline = manager.next_request_estimate();
        assert!(with_real_baseline >= 5_000);

        manager.set_usage_baseline(0);
        let after_zero = manager.next_request_estimate();
        assert_eq!(
            after_zero, with_real_baseline,
            "zero reading must not wipe a valid baseline"
        );
    }

    #[test]
    fn usage_baseline_invalidated_on_replace_messages() {
        let mut manager = ContextManager::new(None, vec![user_text("hi")]);
        manager.set_usage_baseline(7_777);
        assert!(manager.usage_baseline.is_some());

        manager.replace_messages(vec![user_text("new world")]);
        assert!(
            manager.usage_baseline.is_none(),
            "wholesale replace (compact summary / context reset) must drop the baseline"
        );
        // Post-replace estimate falls back to a full re-estimate of
        // the fresh history, not the stale server anchor.
        assert_eq!(
            manager.next_request_estimate(),
            manager.estimate_input_tokens()
        );
    }

    #[test]
    fn usage_baseline_invalidated_on_truncate_for_compact() {
        let mut messages = vec![user_text("oldest")];
        for i in 0..20 {
            messages.push(user_text(&format!("turn-{i}")));
            messages.push(assistant_text(&format!("reply-{i}")));
        }
        let mut manager = ContextManager::new(None, messages);
        manager.set_usage_baseline(9_999);
        assert!(manager.usage_baseline.is_some());

        manager.truncate_for_compact(/* protected_turns */ 4);
        assert!(
            manager.usage_baseline.is_none(),
            "truncation can drop the anchored message → baseline must be invalidated"
        );
    }

    #[test]
    fn usage_baseline_survives_microcompact() {
        // Microcompact only rewrites the *bodies* of existing
        // tool_result blocks; it doesn't drop messages, so the server
        // anchor stays valid and the tail estimate naturally reflects
        // the smaller bodies.
        let mut manager = ContextManager::new(
            None,
            vec![
                user_text("go"),
                assistant_tool_use("toolu_1", "Read"),
                ApiMessage {
                    role: Role::User,
                    content: vec![ApiContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: "toolu_1".into(),
                        content: "x".repeat(30_000).into(),
                        is_error: false,
                    })],
                },
            ],
        );
        manager.set_usage_baseline(4_242);
        let before = manager.usage_baseline;

        let _freed = manager.microcompact_tool_results(
            /* current_tokens */ 100_000, /* target_tokens  */ 10_000,
            /* protected_recent */ 0,
        );

        assert_eq!(
            manager.usage_baseline, before,
            "microcompact rewrites bodies in place; baseline must survive"
        );
    }
}
