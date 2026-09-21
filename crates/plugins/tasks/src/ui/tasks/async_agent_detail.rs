//! Async agent detail dialog projection ([`build_async_agent_detail`]).
//!
//! The detail builder is organized around these pure projections:
//!
//! 1. The title row (`{agent_type} › {description}`).
//! 2. The subtitle row (status pill + elapsed + tokens + tool count).
//! 3. The trimmed prompt (300 chars + ellipsis).
//! 4. The plan extraction ([`extract_tag`] with `plan`).
//! 5. The ACP streaming-output tail (last 600 chars of the most-recent
//!    assistant message text block).
//! 6. The keyboard reducer (`space → done`, `left → back`, `x → kill`).
//! 7. The byline / input-guide list.

use crate::ui::tasks::common::{SemanticColor, TaskStatus};
use crate::ui::tasks::status_utils::{task_status_color, StatusFlags};

/// Pre-built input shape for [`build_async_agent_detail`].
#[derive(Debug, Clone)]
pub struct AsyncAgentDetailInput {
    /// Agent type shown in the title row. The caller supplies
    /// `"agent"` when the agent reports no type of its own.
    pub agent_type: String,
    /// Description shown after the agent type in the title row.
    pub description: String,
    /// The agent task's status.
    pub status: TaskStatus,
    /// Pre-formatted elapsed-time string.
    pub elapsed_time: String,
    /// Token count for the subtitle. The caller resolves it from the
    /// agent's result, falling back to its progress counter; `None`
    /// when neither is available.
    pub token_count: Option<u64>,
    /// Tool-use count for the subtitle. The caller resolves it from the
    /// agent's result, falling back to its progress counter; `None`
    /// when neither is available.
    pub tool_use_count: Option<u64>,
    /// The agent's prompt. The plan extraction and 300-char truncation
    /// happen inside [`build_async_agent_detail`].
    pub prompt: String,
    /// The agent's error text, if any — only displayed when the status
    /// is `TaskStatus::Failed`.
    pub error: Option<String>,
    /// True when the dialog can stop the agent. Drives the "x: stop"
    /// hint.
    pub can_kill: bool,
    /// True when the dialog can go back to a parent view. Drives the
    /// "←: go back" hint.
    pub can_back: bool,
    /// Last assistant ACP text-block content, if any, already extracted
    /// by the caller. The 600-char "show last" trimming happens here.
    pub acp_last_assistant_text: Option<String>,
}

/// Projected async agent detail: what the dialog renders, flattened to
/// plain strings + enums.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncAgentDetail {
    /// Title row text — `"{agent_type} › {description}"`.
    pub title: String,
    /// Subtitle parts. Stitched into a single line by the consumer.
    pub subtitle: AsyncAgentSubtitle,
    /// Either the extracted plan tag (when present) or the truncated
    /// prompt (otherwise).
    pub prompt_block: AsyncAgentPromptBlock,
    /// Optional error block — only present when the status is
    /// `TaskStatus::Failed` and the agent reported a non-empty error.
    pub error: Option<String>,
    /// Optional ACP output tail (already trimmed to 600 chars + leading
    /// ellipsis).
    pub acp_output: Option<String>,
    /// Byline hints (input guide).
    pub byline: AsyncAgentByline,
}

/// Subtitle parts of the async agent dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncAgentSubtitle {
    /// Optional `Completed · ` / `Failed · ` / `Stopped · ` label
    /// shown when the status isn't `running`.
    pub status_label: Option<(String, SemanticColor)>,
    /// `elapsed_time` from input.
    pub elapsed_time: String,
    /// Pre-formatted "{n} tokens" suffix when token_count > 0.
    pub tokens_suffix: Option<String>,
    /// Pre-formatted "{n} tools" / "{n} tool" suffix when
    /// tool_use_count > 0. Renders `tool` when the count is exactly 1, else `tools`.
    pub tools_suffix: Option<String>,
}

/// Either the extracted plan or the truncated prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncAgentPromptBlock {
    /// [`extract_tag`] found a non-empty `plan` tag in the prompt.
    /// Rendered as the user-plan message.
    Plan(String),
    /// No plan tag — render the truncated prompt under a `Prompt`
    /// header. The string is already trimmed to 300 chars (`+ '…'`
    /// when truncated).
    Prompt(String),
}

/// Byline hints. These are conditionally added based on input flags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AsyncAgentByline {
    /// `← go back` (only when `can_back`).
    pub show_back: bool,
    /// `Esc/Enter/Space close` (always shown).
    pub show_close: bool,
    /// `x stop` (only when running and `can_kill`).
    pub show_stop: bool,
}

/// Keyboard event accepted by the async-agent dialog reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncAgentDetailEvent {
    /// Space key — close the dialog.
    Space,
    /// Left-arrow key — back to parent.
    Left,
    /// `x` key — kill the agent.
    XKey,
}

/// Action emitted by the reducer in response to an
/// [`AsyncAgentDetailEvent`]. The consumer threads these to the actual
/// callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncAgentDetailAction {
    /// Close the dialog.
    Done,
    /// Go back to the parent view.
    Back,
    /// Stop the agent.
    Kill,
    /// Event ignored — the gating flag was off.
    Ignore,
}

/// Reducer for the async-agent dialog. Precedence:
///
/// * Space → always Done
/// * Left  → Back if `can_back`, else Ignore
/// * x     → Kill if `running && can_kill`, else Ignore
pub fn handle_async_agent_event(
    event: AsyncAgentDetailEvent,
    status: TaskStatus,
    can_back: bool,
    can_kill: bool,
) -> AsyncAgentDetailAction {
    match event {
        AsyncAgentDetailEvent::Space => AsyncAgentDetailAction::Done,
        AsyncAgentDetailEvent::Left => {
            if can_back {
                AsyncAgentDetailAction::Back
            } else {
                AsyncAgentDetailAction::Ignore
            }
        }
        AsyncAgentDetailEvent::XKey => {
            if status == TaskStatus::Running && can_kill {
                AsyncAgentDetailAction::Kill
            } else {
                AsyncAgentDetailAction::Ignore
            }
        }
    }
}

const PROMPT_DISPLAY_MAX: usize = 300;
const PROMPT_DISPLAY_TRUNC: usize = 297;
const ACP_OUTPUT_TAIL_BYTES: usize = 600;

/// Extract a `<tag>...</tag>` body from a string. Returns the *first*
/// match's inner
/// content with surrounding whitespace trimmed. Returns `None` if no
/// matching pair is found. The matching is greedy on the close tag.
pub fn extract_tag(prompt: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = prompt.find(&open)?;
    let body_start = start + open.len();
    let close_idx = prompt[body_start..].find(&close)?;
    let body = &prompt[body_start..body_start + close_idx];
    Some(body.trim().to_owned())
}

fn truncate_prompt(prompt: &str) -> String {
    if prompt.chars().count() <= PROMPT_DISPLAY_MAX {
        return prompt.to_owned();
    }
    let head: String = prompt.chars().take(PROMPT_DISPLAY_TRUNC).collect();
    format!("{head}…")
}

fn trim_acp_tail(text: &str) -> String {
    if text.chars().count() <= ACP_OUTPUT_TAIL_BYTES {
        return text.to_owned();
    }
    let count = text.chars().count();
    let skip = count - ACP_OUTPUT_TAIL_BYTES;
    let tail: String = text.chars().skip(skip).collect();
    format!("…{tail}")
}

/// Project an async agent dialog state into a render-friendly shape.
pub fn build_async_agent_detail(input: &AsyncAgentDetailInput) -> AsyncAgentDetail {
    let title = format!("{} › {}", input.agent_type, input.description);

    // Status label, shown only when not running.
    let status_label = if input.status != TaskStatus::Running {
        let label = match input.status {
            TaskStatus::Completed => "Completed",
            TaskStatus::Failed => "Failed",
            // killed / others
            _ => "Stopped",
        };
        Some((
            format!("{label} · "),
            task_status_color(input.status, StatusFlags::default()),
        ))
    } else {
        None
    };

    let tokens_suffix = match input.token_count {
        Some(n) if n > 0 => Some(format!(" · {n} tokens")),
        _ => None,
    };

    let tools_suffix = match input.tool_use_count {
        Some(n) if n > 0 => {
            let word = if n == 1 { "tool" } else { "tools" };
            Some(format!(" · {n} {word}"))
        }
        _ => None,
    };

    let subtitle = AsyncAgentSubtitle {
        status_label,
        elapsed_time: input.elapsed_time.clone(),
        tokens_suffix,
        tools_suffix,
    };

    let prompt_block = match extract_tag(&input.prompt, "plan") {
        Some(plan) if !plan.is_empty() => AsyncAgentPromptBlock::Plan(plan),
        _ => AsyncAgentPromptBlock::Prompt(truncate_prompt(&input.prompt)),
    };

    let error = if input.status == TaskStatus::Failed {
        input.error.as_ref().filter(|s| !s.is_empty()).cloned()
    } else {
        None
    };

    let acp_output = if input.status == TaskStatus::Running {
        input
            .acp_last_assistant_text
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| trim_acp_tail(s))
    } else {
        None
    };

    let byline = AsyncAgentByline {
        show_back: input.can_back,
        show_close: true,
        show_stop: input.status == TaskStatus::Running && input.can_kill,
    };

    AsyncAgentDetail {
        title,
        subtitle,
        prompt_block,
        error,
        acp_output,
        byline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(status: TaskStatus) -> AsyncAgentDetailInput {
        AsyncAgentDetailInput {
            agent_type: "code-reviewer".into(),
            description: "Review changes".into(),
            status,
            elapsed_time: "00:30".into(),
            token_count: None,
            tool_use_count: None,
            prompt: "do the thing".into(),
            error: None,
            can_kill: false,
            can_back: false,
            acp_last_assistant_text: None,
        }
    }

    #[test]
    fn handle_event_space_always_done() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Completed,
        ] {
            assert_eq!(
                handle_async_agent_event(AsyncAgentDetailEvent::Space, status, false, false),
                AsyncAgentDetailAction::Done
            );
        }
    }

    #[test]
    fn handle_event_left_requires_can_back() {
        assert_eq!(
            handle_async_agent_event(
                AsyncAgentDetailEvent::Left,
                TaskStatus::Running,
                false,
                false
            ),
            AsyncAgentDetailAction::Ignore
        );
        assert_eq!(
            handle_async_agent_event(
                AsyncAgentDetailEvent::Left,
                TaskStatus::Running,
                true,
                false
            ),
            AsyncAgentDetailAction::Back
        );
    }

    #[test]
    fn handle_event_x_requires_running_and_can_kill() {
        // Not running
        assert_eq!(
            handle_async_agent_event(
                AsyncAgentDetailEvent::XKey,
                TaskStatus::Completed,
                false,
                true
            ),
            AsyncAgentDetailAction::Ignore
        );
        // Running but no can_kill
        assert_eq!(
            handle_async_agent_event(
                AsyncAgentDetailEvent::XKey,
                TaskStatus::Running,
                false,
                false
            ),
            AsyncAgentDetailAction::Ignore
        );
        // Running + can_kill
        assert_eq!(
            handle_async_agent_event(
                AsyncAgentDetailEvent::XKey,
                TaskStatus::Running,
                false,
                true
            ),
            AsyncAgentDetailAction::Kill
        );
    }

    #[test]
    fn extract_tag_basic() {
        assert_eq!(
            extract_tag("<plan>do thing</plan>", "plan"),
            Some("do thing".into())
        );
        // Surrounding text and whitespace
        assert_eq!(
            extract_tag("blah <plan>\n  step\n</plan> blah", "plan"),
            Some("step".into())
        );
        // No tag
        assert_eq!(extract_tag("hello", "plan"), None);
        // Open without close
        assert_eq!(extract_tag("<plan>orphan", "plan"), None);
    }

    #[test]
    fn truncate_prompt_short_passes_through() {
        let short = "abc";
        let detail = build_async_agent_detail(&AsyncAgentDetailInput {
            prompt: short.into(),
            ..input(TaskStatus::Running)
        });
        assert_eq!(
            detail.prompt_block,
            AsyncAgentPromptBlock::Prompt("abc".into())
        );
    }

    #[test]
    fn truncate_prompt_long_gets_ellipsis() {
        let long = "a".repeat(400);
        let detail = build_async_agent_detail(&AsyncAgentDetailInput {
            prompt: long,
            ..input(TaskStatus::Running)
        });
        match detail.prompt_block {
            AsyncAgentPromptBlock::Prompt(s) => {
                // 297 chars + 1 ellipsis
                assert_eq!(s.chars().count(), PROMPT_DISPLAY_TRUNC + 1);
                assert!(s.ends_with('…'));
            }
            _ => panic!("expected truncated prompt"),
        }
    }

    #[test]
    fn plan_tag_replaces_prompt() {
        let detail = build_async_agent_detail(&AsyncAgentDetailInput {
            prompt: "<plan>review the diff</plan>".into(),
            ..input(TaskStatus::Running)
        });
        assert_eq!(
            detail.prompt_block,
            AsyncAgentPromptBlock::Plan("review the diff".into())
        );
    }

    #[test]
    fn status_label_only_when_not_running() {
        let det_run = build_async_agent_detail(&input(TaskStatus::Running));
        assert!(det_run.subtitle.status_label.is_none());

        let det_done = build_async_agent_detail(&input(TaskStatus::Completed));
        let label = det_done.subtitle.status_label.unwrap();
        assert_eq!(label.0, "Completed · ");
        assert_eq!(label.1, SemanticColor::Success);

        let det_failed = build_async_agent_detail(&input(TaskStatus::Failed));
        let label = det_failed.subtitle.status_label.unwrap();
        assert_eq!(label.0, "Failed · ");
        assert_eq!(label.1, SemanticColor::Error);

        let det_killed = build_async_agent_detail(&input(TaskStatus::Killed));
        let label = det_killed.subtitle.status_label.unwrap();
        assert_eq!(label.0, "Stopped · ");
        assert_eq!(label.1, SemanticColor::Warning);
    }

    #[test]
    fn token_and_tool_suffix_singular_plural() {
        let mut i = input(TaskStatus::Running);
        i.token_count = Some(1234);
        i.tool_use_count = Some(1);
        let d = build_async_agent_detail(&i);
        assert_eq!(d.subtitle.tokens_suffix, Some(" · 1234 tokens".into()));
        assert_eq!(d.subtitle.tools_suffix, Some(" · 1 tool".into()));

        i.tool_use_count = Some(5);
        let d = build_async_agent_detail(&i);
        assert_eq!(d.subtitle.tools_suffix, Some(" · 5 tools".into()));
    }

    #[test]
    fn zero_counts_suppress_suffixes() {
        let mut i = input(TaskStatus::Running);
        i.token_count = Some(0);
        i.tool_use_count = Some(0);
        let d = build_async_agent_detail(&i);
        assert!(d.subtitle.tokens_suffix.is_none());
        assert!(d.subtitle.tools_suffix.is_none());
    }

    #[test]
    fn error_only_when_failed() {
        let mut i = input(TaskStatus::Running);
        i.error = Some("oops".into());
        // Status is running → no error block
        let d = build_async_agent_detail(&i);
        assert!(d.error.is_none());
        // Failed + non-empty error
        i.status = TaskStatus::Failed;
        let d = build_async_agent_detail(&i);
        assert_eq!(d.error.as_deref(), Some("oops"));
        // Failed + empty error
        i.error = Some("".into());
        let d = build_async_agent_detail(&i);
        assert!(d.error.is_none());
    }

    #[test]
    fn acp_output_only_when_running() {
        let mut i = input(TaskStatus::Completed);
        i.acp_last_assistant_text = Some("hello".into());
        let d = build_async_agent_detail(&i);
        assert!(d.acp_output.is_none());

        i.status = TaskStatus::Running;
        let d = build_async_agent_detail(&i);
        assert_eq!(d.acp_output.as_deref(), Some("hello"));
    }

    #[test]
    fn acp_output_tail_trim() {
        let mut i = input(TaskStatus::Running);
        i.acp_last_assistant_text = Some("a".repeat(800));
        let d = build_async_agent_detail(&i);
        let out = d.acp_output.unwrap();
        // 600 chars + 1 leading ellipsis
        assert_eq!(out.chars().count(), ACP_OUTPUT_TAIL_BYTES + 1);
        assert!(out.starts_with('…'));
    }

    #[test]
    fn byline_flags() {
        let mut i = input(TaskStatus::Running);
        i.can_back = true;
        i.can_kill = true;
        let d = build_async_agent_detail(&i);
        assert!(d.byline.show_back);
        assert!(d.byline.show_close);
        assert!(d.byline.show_stop);

        // Stopped state hides the stop hint
        i.status = TaskStatus::Completed;
        let d = build_async_agent_detail(&i);
        assert!(!d.byline.show_stop);
    }

    #[test]
    fn title_format() {
        let i = input(TaskStatus::Running);
        let d = build_async_agent_detail(&i);
        assert_eq!(d.title, "code-reviewer › Review changes");
    }
}
