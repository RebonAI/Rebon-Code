//! XML-like wrapper classification for user-text blocks.
//!
//! A user text block is routed by the markers
//! its text carries. The detection order is load-bearing (several checks
//! are non-commuting) and is exactly the order of the chain in
//! [`detect_user_text_kind`]:
//!
//! ```text
//!   text trims to NO_CONTENT_MESSAGE                  → NoContent
//!   a balanced <tick>...</tick> pair is present       → Tick
//!   text contains `<local-command-caveat>`            → LocalCommandCaveat
//!   text starts with `<bash-stdout` or `<bash-stderr` → BashOutput
//!   text starts with `<local-command-stdout` or
//!     `<local-command-stderr`                         → LocalCommandOutput
//!   text is exactly one interrupt literal             → Interrupt
//!   text starts with `<github-webhook-activity>`      → GitHubWebhook
//!   text contains `<bash-input>`                      → BashInput
//!   text contains `<command-message>`                 → SlashCommand
//!   text contains `<user-memory-input>`               → MemoryInput
//!   text contains `<teammate-message`                 → TeammateMessage
//!   text contains `<task-notification`                → TaskNotification
//!   text contains `<mcp-resource-update` or
//!     `<mcp-polling-update`                           → McpResourceUpdate
//!   text contains `<fork-boilerplate>`                → ForkBoilerplate
//!   text contains `<cross-session-message`            → CrossSessionMessage
//!   text contains `<channel source="`                 → ChannelMessage
//!   otherwise                                         → Prompt
//! ```
//!
//! Two routing decisions cannot be derived from the text alone and are not
//! made here:
//!
//! * The plan-to-implement branch. A plan is a field the caller sets, not
//!   something the text carries, so the caller that would set it should fill
//!   [`UserMessage::plan_content`](crate::transcript_row::UserMessage::plan_content)
//!   instead. When that field is `Some`, the renderer dispatches straight to
//!   the plan branch and this classification never runs for the row.
//!
//! * `NO_CONTENT_MESSAGE` ("(no content)") is a sentinel injected when a
//!   message carries an empty string body. On the user side that check is
//!   defensive rather than load-bearing, but it is implemented here as
//!   [`UserTextKind::NoContent`] so callers can preserve the "render
//!   nothing" branch.
//!
//! ## `extract_tag` semantics
//!
//! [`extract_tag`] is a **balanced-pair** matcher: it only returns a value
//! when a `<tag ...>...</tag>` pair exists at nesting depth 0, with
//! non-empty content. A bare `<tick>` with no closing tag, or an imbalanced
//! pair, returns `None` — which means the tick branch is not taken.
//!
//! A naive `contains("<tick>")` check would fire on any substring match,
//! which diverges: a user prompt that happens to contain the literal
//! `<tick>` in the middle of a sentence would be routed to the tick branch.
//! The tests at the end of this module include a fixture that pins the
//! balanced-pair semantics.
//!
//! ## Where this lives
//!
//! The fold in [`crate::fold_rows`] asks it which user rows are coordinator
//! traffic, and every surface folds the same transcript, so the answer has to
//! come from one place.

use crate::tool_results::{INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE};
use crate::user_text::NO_CONTENT_MESSAGE;

/// Which wrapper a user-text block carries, one variant per branch the
/// renderer can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserTextKind {
    /// The text trims to `NO_CONTENT_MESSAGE`; nothing is drawn.
    NoContent,
    /// A balanced `<tick>...</tick>` pair is present; nothing is drawn.
    Tick,
    /// The text contains `<local-command-caveat>`; nothing is drawn.
    LocalCommandCaveat,
    /// The text starts with `<bash-stdout` or `<bash-stderr`.
    BashOutput,
    /// The text starts with `<local-command-stdout` or
    /// `<local-command-stderr`.
    LocalCommandOutput,
    /// The text is exactly one of the two interrupt literals.
    Interrupt,
    /// The text starts with `<github-webhook-activity>`; the GitHub webhook
    /// gate is checked by the caller.
    GitHubWebhook,
    /// The text contains `<bash-input>`.
    BashInput,
    /// The text contains `<command-message>`.
    SlashCommand,
    /// The text contains `<user-memory-input>`.
    MemoryInput,
    /// The text contains `<teammate-message`; the agent-swarm gate is
    /// checked by the caller.
    TeammateMessage,
    /// The text contains `<task-notification`.
    TaskNotification,
    /// The text contains `<mcp-resource-update` or `<mcp-polling-update`.
    McpResourceUpdate,
    /// The text contains `<fork-boilerplate>`; the fork-subagent gate is
    /// checked by the caller.
    ForkBoilerplate,
    /// The text contains `<cross-session-message`; the cross-session inbox
    /// gate is checked by the caller.
    CrossSessionMessage,
    /// The text contains `<channel source="`; the channel-message gate is
    /// checked by the caller.
    ChannelMessage,
    /// Nothing matched: a plain user prompt.
    Prompt,
}

/// Classify a user-text block from the markers its text carries, in the
/// order listed at the top of this module.
///
/// The caller owns the feature-flag check. This function does
/// **not** know which feature flags are on; it will return the
/// feature-flagged variants unconditionally, and the renderer is
/// responsible for downgrading to `Prompt` if the flag is off. This
/// keeps the flag check out of classification, where the `contains` check
/// already ran — so a
/// classification can safely be "pre-computed then flag-gated at
/// render time" without changing observed behaviour for
/// flag-off builds.
pub fn detect_user_text_kind(text: &str) -> UserTextKind {
    // The no-content sentinel, compared after trimming.
    if text.trim() == NO_CONTENT_MESSAGE {
        return UserTextKind::NoContent;
    }

    // A balanced tick pair, not a substring match.
    if extract_tag(text, "tick").is_some() {
        return UserTextKind::Tick;
    }

    // The local-command caveat marker, which must be checked BEFORE the
    // command-message marker below, because caveat payloads can nest
    // command-message tags inline.
    if text.contains("<local-command-caveat>") {
        return UserTextKind::LocalCommandCaveat;
    }

    // Bash output, recognised by its opening prefix.
    if text.starts_with("<bash-stdout") || text.starts_with("<bash-stderr") {
        return UserTextKind::BashOutput;
    }

    // Local-command output, recognised by its opening prefix.
    if text.starts_with("<local-command-stdout") || text.starts_with("<local-command-stderr") {
        return UserTextKind::LocalCommandOutput;
    }

    // The interrupt literals, matched exactly.
    if text == INTERRUPT_MESSAGE || text == INTERRUPT_MESSAGE_FOR_TOOL_USE {
        return UserTextKind::Interrupt;
    }

    // GitHub webhook activity, recognised by its opening prefix.
    if text.starts_with("<github-webhook-activity>") {
        return UserTextKind::GitHubWebhook;
    }

    // Bash-input marker.
    if text.contains("<bash-input>") {
        return UserTextKind::BashInput;
    }

    // Slash-command marker.
    if text.contains("<command-message>") {
        return UserTextKind::SlashCommand;
    }

    // Memory-input marker.
    if text.contains("<user-memory-input>") {
        return UserTextKind::MemoryInput;
    }

    // Teammate-message open tag.
    if text.contains("<teammate-message") {
        return UserTextKind::TeammateMessage;
    }

    // Task-notification open tag.
    if text.contains("<task-notification") {
        return UserTextKind::TaskNotification;
    }

    // MCP resource update or polling update.
    if text.contains("<mcp-resource-update") || text.contains("<mcp-polling-update") {
        return UserTextKind::McpResourceUpdate;
    }

    // Fork boilerplate.
    if text.contains("<fork-boilerplate>") {
        return UserTextKind::ForkBoilerplate;
    }

    // Cross-session message.
    if text.contains("<cross-session-message") {
        return UserTextKind::CrossSessionMessage;
    }

    // Channel message, recognised by its source attribute.
    if text.contains("<channel source=\"") {
        return UserTextKind::ChannelMessage;
    }

    // Nothing matched.
    UserTextKind::Prompt
}

/// Return the inner content of the first balanced tag pair.
///
/// Returns the inner content of the first **balanced**
/// `<tag...>...</tag>` pair in `haystack`, or `None` if no such
/// pair exists. "Balanced" means the pair is at nesting depth 0 —
/// if the tag appears inside a nested pair of the same name, the
/// outer pair is matched, not the inner.
///
/// Returns `None` for empty-content matches.
///
/// The walk is byte-by-byte: find an opening tag, find the next closing tag
/// after it, count same-name openers and closers in the text before the
/// opener to get the nesting depth, and return the first enclosed range
/// whose depth is 0 AND whose content is non-empty.
///
/// ## Why no regex dependency
///
/// Pulling in `regex` for a single balanced-tag matcher would add a
/// workspace dep for one function. The hand-rolled walk here covers the
/// same cases (attributes on the opening tag, self-closing not supported,
/// empty content skipped, nesting respected) in ~40 lines with no
/// allocations besides the returned `String`.
pub fn extract_tag(haystack: &str, tag_name: &str) -> Option<String> {
    if haystack.trim().is_empty() || tag_name.trim().is_empty() {
        return None;
    }
    let open = format!("<{tag_name}");
    let close = format!("</{tag_name}>");

    // Walk candidate open/close pairs left to right. For each candidate,
    // count the opening and closing tags before it to get the nesting
    // depth. The first depth-0 pair with non-empty content wins.
    let bytes = haystack.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Find the next `<tag` at or after cursor.
        let Some(open_rel) = haystack[cursor..].find(&open) else {
            break;
        };
        let open_start = cursor + open_rel;
        // Skip over the opening tag's attributes up to the matching
        // `>`. If there's no `>`, bail — malformed input.
        let after_open = &haystack[open_start + open.len()..];
        let gt_rel = after_open.find('>')?;
        let content_start = open_start + open.len() + gt_rel + 1;
        // Find the next closing tag after the content start.
        let Some(close_rel) = haystack[content_start..].find(&close) else {
            // No matching close — skip past this open tag and continue.
            cursor = content_start;
            continue;
        };
        let content_end = content_start + close_rel;

        // Count opening and closing tags of the same name in the prefix up
        // to `open_start` — the nesting depth of this candidate.
        let prefix = &haystack[..open_start];
        let mut depth: i32 = 0;
        // A bare `find("<tag")` would also match `<tagfoo`, so only count
        // full openers: the bytes after `<tag` must be `>`, a space, a tab,
        // a newline or a carriage return.
        let mut p = 0;
        while let Some(rel) = prefix[p..].find(&open) {
            let pos = p + rel;
            let next_byte = prefix.as_bytes().get(pos + open.len()).copied();
            if matches!(
                next_byte,
                Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
            ) {
                depth += 1;
            }
            p = pos + open.len();
        }
        // Count closers.
        let mut p = 0;
        while let Some(rel) = prefix[p..].find(&close) {
            let pos = p + rel;
            depth -= 1;
            p = pos + close.len();
        }

        let content = &haystack[content_start..content_end];
        if depth == 0 && !content.is_empty() {
            return Some(content.to_string());
        }

        cursor = content_end + close.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // extract_tag tests — pin the balanced-pair semantics.
    // -----------------------------------------------------------------

    #[test]
    fn extract_tag_returns_inner_of_simple_pair() {
        assert_eq!(extract_tag("<tick>1</tick>", "tick"), Some("1".into()));
    }

    #[test]
    fn extract_tag_matches_tag_with_attributes() {
        assert_eq!(
            extract_tag("<tick attr=\"x\">inner</tick>", "tick"),
            Some("inner".into())
        );
    }

    #[test]
    fn extract_tag_returns_none_on_empty_content() {
        // An empty-content pair returns `None`, so the empty-string case is
        // skipped.
        assert_eq!(extract_tag("<tick></tick>", "tick"), None);
    }

    #[test]
    fn extract_tag_returns_none_on_unmatched_open() {
        // Just `<tick>` with no closing tag — must NOT return Some.
        // This is the key alignment fix vs naive contains.
        assert_eq!(extract_tag("<tick>", "tick"), None);
    }

    #[test]
    fn extract_tag_returns_none_on_unmatched_close() {
        assert_eq!(extract_tag("</tick>", "tick"), None);
    }

    #[test]
    fn extract_tag_returns_none_on_empty_haystack_or_tag() {
        assert_eq!(extract_tag("", "tick"), None);
        assert_eq!(extract_tag("<tick>x</tick>", ""), None);
        assert_eq!(extract_tag("   ", "tick"), None);
    }

    #[test]
    fn extract_tag_returns_none_for_unrelated_content() {
        // A sentence that mentions `<tick>` but does not wrap content in a
        // balanced pair, so `extract_tag` returns `None`.
        assert_eq!(
            extract_tag("the tag `<tick>` is used for this", "tick"),
            None
        );
    }

    // -----------------------------------------------------------------
    // detect_user_text_kind — per-branch alignment tests
    // -----------------------------------------------------------------

    #[test]
    fn no_content_sentinel_classified() {
        assert_eq!(
            detect_user_text_kind(NO_CONTENT_MESSAGE),
            UserTextKind::NoContent
        );
        // Trimmed equality — leading/trailing whitespace still matches.
        assert_eq!(
            detect_user_text_kind("  (no content)  "),
            UserTextKind::NoContent
        );
        // Not a substring match — content around the sentinel falls
        // through to Prompt.
        assert_eq!(
            detect_user_text_kind("prefix (no content) suffix"),
            UserTextKind::Prompt
        );
    }

    #[test]
    fn tick_uses_balanced_pair_semantics() {
        // Balanced pair → Tick.
        assert_eq!(detect_user_text_kind("<tick>1</tick>"), UserTextKind::Tick);
        // Bare open tag → Prompt (not Tick). This is the load-
        // bearing fix vs the earlier `contains` check.
        assert_eq!(detect_user_text_kind("<tick>"), UserTextKind::Prompt);
        // Open with attributes → Tick if closed.
        assert_eq!(
            detect_user_text_kind("<tick id=\"a\">x</tick>"),
            UserTextKind::Tick
        );
    }

    #[test]
    fn tick_empty_content_not_classified_as_tick() {
        // An empty-content pair returns `None`, so detection must not fire
        // here. The row falls through to Prompt.
        assert_eq!(detect_user_text_kind("<tick></tick>"), UserTextKind::Prompt);
    }

    #[test]
    fn plain_prompt_with_angle_brackets_is_still_prompt() {
        assert_eq!(
            detect_user_text_kind("consider <Box> in React for layout"),
            UserTextKind::Prompt
        );
    }

    #[test]
    fn bash_stdout_prefix_at_column_zero() {
        assert_eq!(
            detect_user_text_kind("<bash-stdout>output</bash-stdout>"),
            UserTextKind::BashOutput
        );
        assert_eq!(
            detect_user_text_kind("<bash-stderr>err</bash-stderr>"),
            UserTextKind::BashOutput
        );
        // Not at column 0 — must be Prompt.
        assert_eq!(
            detect_user_text_kind("prefix <bash-stdout>x</bash-stdout>"),
            UserTextKind::Prompt
        );
    }

    #[test]
    fn local_command_output_prefix_at_column_zero() {
        assert_eq!(
            detect_user_text_kind("<local-command-stdout>ok</local-command-stdout>"),
            UserTextKind::LocalCommandOutput
        );
        assert_eq!(
            detect_user_text_kind("<local-command-stderr>err</local-command-stderr>"),
            UserTextKind::LocalCommandOutput
        );
    }

    #[test]
    fn local_command_caveat_beats_slash_command() {
        // Caveats can nest command-message tags, so the caveat check must
        // come first.
        let text = "<local-command-caveat>no tools</local-command-caveat>\
                    <command-message>/help</command-message>";
        assert_eq!(
            detect_user_text_kind(text),
            UserTextKind::LocalCommandCaveat
        );
    }

    #[test]
    fn interrupt_literals_are_exact_equality() {
        assert_eq!(
            detect_user_text_kind(INTERRUPT_MESSAGE),
            UserTextKind::Interrupt
        );
        assert_eq!(
            detect_user_text_kind(INTERRUPT_MESSAGE_FOR_TOOL_USE),
            UserTextKind::Interrupt
        );
        // Substring match must NOT fire.
        assert_eq!(
            detect_user_text_kind("the user said [Request interrupted by user] earlier"),
            UserTextKind::Prompt
        );
    }

    #[test]
    fn bash_input_is_substring_not_prefix() {
        assert_eq!(
            detect_user_text_kind("running: <bash-input>ls</bash-input>"),
            UserTextKind::BashInput
        );
    }

    #[test]
    fn slash_command_memory_input_teammate_task_notification() {
        assert_eq!(
            detect_user_text_kind("<command-message>/commit</command-message>"),
            UserTextKind::SlashCommand
        );
        assert_eq!(
            detect_user_text_kind("<user-memory-input>remember</user-memory-input>"),
            UserTextKind::MemoryInput
        );
        assert_eq!(
            detect_user_text_kind("<teammate-message from=\"a\">hi</teammate-message>"),
            UserTextKind::TeammateMessage
        );
        assert_eq!(
            detect_user_text_kind("<task-notification id=\"t\">done</task-notification>"),
            UserTextKind::TaskNotification
        );
    }

    #[test]
    fn mcp_update_variants_all_match() {
        assert_eq!(
            detect_user_text_kind("<mcp-resource-update uri=\"x\">data</mcp-resource-update>"),
            UserTextKind::McpResourceUpdate
        );
        assert_eq!(
            detect_user_text_kind("<mcp-polling-update uri=\"y\">data</mcp-polling-update>"),
            UserTextKind::McpResourceUpdate
        );
    }

    #[test]
    fn channel_message_requires_source_attribute() {
        assert_eq!(
            detect_user_text_kind("<channel source=\"mcp\">data</channel>"),
            UserTextKind::ChannelMessage
        );
        // Plain <channel> without source=" must NOT match.
        assert_eq!(
            detect_user_text_kind("<channel>plain</channel>"),
            UserTextKind::Prompt
        );
    }

    #[test]
    fn fork_boilerplate_cross_session_github_webhook() {
        assert_eq!(
            detect_user_text_kind("<fork-boilerplate>rules</fork-boilerplate>"),
            UserTextKind::ForkBoilerplate
        );
        assert_eq!(
            detect_user_text_kind("<cross-session-message from=\"s\">hi</cross-session-message>"),
            UserTextKind::CrossSessionMessage
        );
        assert_eq!(
            detect_user_text_kind("<github-webhook-activity>push</github-webhook-activity>"),
            UserTextKind::GitHubWebhook
        );
    }

    /// Exhaustive table. Each entry lists the exact text
    /// and the expected classification. Using a single table means
    /// "someone renamed a tag without updating detection" shows up
    /// as a clear table delta in review.
    #[test]
    fn classification_table() {
        let cases: &[(&str, UserTextKind)] = &[
            (NO_CONTENT_MESSAGE, UserTextKind::NoContent),
            ("<tick>1</tick>", UserTextKind::Tick),
            ("<tick>", UserTextKind::Prompt), // unbalanced → Prompt
            (
                "<local-command-caveat>x</local-command-caveat>",
                UserTextKind::LocalCommandCaveat,
            ),
            ("<bash-stdout>x</bash-stdout>", UserTextKind::BashOutput),
            ("<bash-stderr>x</bash-stderr>", UserTextKind::BashOutput),
            (
                "<local-command-stdout>x</local-command-stdout>",
                UserTextKind::LocalCommandOutput,
            ),
            (
                "<local-command-stderr>x</local-command-stderr>",
                UserTextKind::LocalCommandOutput,
            ),
            (INTERRUPT_MESSAGE, UserTextKind::Interrupt),
            (INTERRUPT_MESSAGE_FOR_TOOL_USE, UserTextKind::Interrupt),
            (
                "<github-webhook-activity>x</github-webhook-activity>",
                UserTextKind::GitHubWebhook,
            ),
            (
                "running <bash-input>x</bash-input>",
                UserTextKind::BashInput,
            ),
            (
                "<command-message>/help</command-message>",
                UserTextKind::SlashCommand,
            ),
            (
                "<user-memory-input>x</user-memory-input>",
                UserTextKind::MemoryInput,
            ),
            (
                "<teammate-message from=\"a\">x</teammate-message>",
                UserTextKind::TeammateMessage,
            ),
            (
                "<task-notification id=\"1\">x</task-notification>",
                UserTextKind::TaskNotification,
            ),
            (
                "<mcp-resource-update uri=\"x\">x</mcp-resource-update>",
                UserTextKind::McpResourceUpdate,
            ),
            (
                "<mcp-polling-update uri=\"x\">x</mcp-polling-update>",
                UserTextKind::McpResourceUpdate,
            ),
            (
                "<fork-boilerplate>x</fork-boilerplate>",
                UserTextKind::ForkBoilerplate,
            ),
            (
                "<cross-session-message from=\"s\">x</cross-session-message>",
                UserTextKind::CrossSessionMessage,
            ),
            (
                "<channel source=\"m\">x</channel>",
                UserTextKind::ChannelMessage,
            ),
            ("<channel>plain</channel>", UserTextKind::Prompt),
            ("hello world", UserTextKind::Prompt),
            ("", UserTextKind::Prompt),
        ];
        for (text, expected) in cases {
            assert_eq!(
                detect_user_text_kind(text),
                *expected,
                "table entry failed for input {text:?}",
            );
        }
    }
}
