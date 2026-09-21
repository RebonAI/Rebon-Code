//! Projection for user text rows: the tag-dispatched branches (plan, bash
//! output, local command output, interrupted, bash input, slash command,
//! memory input, teammate, task notification, resource update, channel)
//! and the plain-prompt fallback with its long-prompt folding.

use crate::simple_messages::{
    AgentNotificationDisplay, BashInputDisplay, BashOutputDisplay, LocalCommandOutputDisplay,
    ResourceUpdateDisplay, UserChannelDisplay, UserCommandDisplay, UserMemoryInputDisplay,
};
use crate::teammate_messages::TeammateRenderable;

/// The placeholder text that marks a user message with no content.
pub const NO_CONTENT_MESSAGE: &str = "(no content)";

/// Number of leading user-prompt lines kept when folding long prompts.
pub const USER_PROMPT_FOLD_HEAD_LINES: usize = 10;

/// Number of trailing user-prompt lines kept when folding long prompts.
pub const USER_PROMPT_FOLD_TAIL_LINES: usize = 10;

/// Minimum line count before a plain user prompt is folded.
pub const USER_PROMPT_FOLD_THRESHOLD_LINES: usize = 24;

/// Fallback separator width for non-widget render paths.
pub const USER_PROMPT_FOLD_DEFAULT_WIDTH: usize = 120;

/// One display line in a possibly folded plain user prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserPromptDisplayLine {
    /// A visible prompt line.
    Text(String),
    /// A separator summarizing the hidden middle section.
    HiddenSeparator {
        /// Number of hidden lines.
        hidden_line_count: usize,
    },
}

/// The branch one user text row projects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserTextProjection {
    /// Hidden because the content is `(no content)`.
    HiddenNoContent,
    /// Hidden because a `<tick>` tag is present.
    HiddenTick,
    /// Hidden because the text only carries the local-command caveat tag.
    HiddenLocalCommandCaveat,
    /// Plan-to-implement branch.
    Plan {
        /// Whether the standard top margin is added above the row.
        add_margin: bool,
        /// Plan markdown content.
        plan_content: String,
    },
    /// Bash output branch.
    BashOutput(BashOutputDisplay),
    /// Local command output branch.
    LocalCommandOutput(LocalCommandOutputDisplay),
    /// Interrupted-by-user branch.
    Interrupted,
    /// Deferred feature-gated GitHub webhook branch.
    GitHubWebhookDeferred,
    /// Bash input branch.
    BashInput(BashInputDisplay),
    /// Slash-command branch.
    Command(UserCommandDisplay),
    /// User memory input branch.
    MemoryInput(UserMemoryInputDisplay),
    /// Teammate-message branch.
    Teammate(Vec<TeammateRenderable>),
    /// Task notification branch.
    TaskNotification(AgentNotificationDisplay),
    /// MCP resource/polling update branch.
    ResourceUpdates(Vec<ResourceUpdateDisplay>),
    /// Deferred feature-gated fork boilerplate branch.
    ForkBoilerplateDeferred,
    /// Deferred feature-gated cross-session branch.
    CrossSessionDeferred,
    /// Channel branch.
    Channel(UserChannelDisplay),
    /// Plain prompt fallback branch.
    Prompt {
        /// Whether the standard top margin is added above the row.
        add_margin: bool,
        /// Raw prompt text.
        text: String,
        /// Verbose flag.
        verbose: bool,
        /// Transcript mode flag.
        is_transcript_mode: bool,
        /// Optional timestamp.
        timestamp: Option<String>,
    },
}

impl UserTextProjection {
    /// Whether this projection contains only teammate task-completion notices.
    pub fn is_teammate_task_completion(&self) -> bool {
        matches!(
            self,
            Self::Teammate(renderables)
                if !renderables.is_empty()
                    && renderables.iter().all(|renderable| {
                        matches!(
                            renderable,
                            TeammateRenderable::TaskCompleted { .. }
                                | TeammateRenderable::IdleNotification { .. }
                        )
                    })
        )
    }

    /// Whether this projection contains only plain teammate messages.
    pub fn is_plain_teammate_message(&self) -> bool {
        matches!(
            self,
            Self::Teammate(renderables)
                if !renderables.is_empty()
                    && renderables
                        .iter()
                        .all(|renderable| matches!(renderable, TeammateRenderable::Plain(_)))
        )
    }
}

/// Input for projecting one user text row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTextInput {
    /// Whether the standard top margin is added above the row.
    pub add_margin: bool,
    /// Raw text block content.
    pub text: String,
    /// Verbose flag.
    pub verbose: bool,
    /// Optional plan content routed ahead of raw text.
    pub plan_content: Option<String>,
    /// Transcript mode flag.
    pub is_transcript_mode: bool,
    /// Optional timestamp.
    pub timestamp: Option<String>,
    /// Feature gate for GitHub webhook messages.
    pub github_webhooks_enabled: bool,
    /// Feature gate for fork boilerplate messages.
    pub fork_subagent_enabled: bool,
    /// Feature gate for cross-session UDS messages.
    pub uds_inbox_enabled: bool,
    /// Feature gate for channel messages (`<channel source="…">`).
    pub channels_enabled: bool,
    /// Whether teammate/swarm messages are enabled.
    pub agent_swarms_enabled: bool,
}

/// Dispatch one user text row to its branch, testing the markers in this
/// order.
pub fn project_user_text(input: &UserTextInput) -> UserTextProjection {
    if input.text.trim() == NO_CONTENT_MESSAGE {
        return UserTextProjection::HiddenNoContent;
    }
    if let Some(plan_content) = &input.plan_content {
        return UserTextProjection::Plan {
            add_margin: input.add_margin,
            plan_content: plan_content.clone(),
        };
    }
    if crate::common::extract_tag(&input.text, "tick").is_some() {
        return UserTextProjection::HiddenTick;
    }
    if input.text.contains("<local-command-caveat>") {
        return UserTextProjection::HiddenLocalCommandCaveat;
    }
    if input.text.starts_with("<bash-stdout") || input.text.starts_with("<bash-stderr") {
        return UserTextProjection::BashOutput(
            crate::simple_messages::parse_bash_output_tags(&input.text, input.verbose).unwrap_or(
                BashOutputDisplay {
                    stdout: String::new(),
                    stderr: String::new(),
                    verbose: input.verbose,
                },
            ),
        );
    }
    if input.text.starts_with("<local-command-stdout")
        || input.text.starts_with("<local-command-stderr")
    {
        return UserTextProjection::LocalCommandOutput(
            crate::simple_messages::project_user_local_command_output(&input.text),
        );
    }
    if input.text == crate::tool_results::INTERRUPT_MESSAGE_FOR_TOOL_USE
        || input.text == crate::tool_results::INTERRUPT_MESSAGE
    {
        return UserTextProjection::Interrupted;
    }
    if input.github_webhooks_enabled && input.text.starts_with("<github-webhook-activity>") {
        return UserTextProjection::GitHubWebhookDeferred;
    }
    if input.text.contains("<bash-input>") {
        return UserTextProjection::BashInput(
            crate::simple_messages::project_user_bash_input(&input.text, input.add_margin)
                .unwrap_or(BashInputDisplay {
                    input: String::new(),
                    margin_top: 0,
                }),
        );
    }
    if input.text.contains("<command-message>") {
        return UserTextProjection::Command(
            crate::simple_messages::project_user_command_message(&input.text, input.add_margin)
                .unwrap(),
        );
    }
    if input.text.contains("<user-memory-input>") {
        return UserTextProjection::MemoryInput(
            crate::simple_messages::project_user_memory_input(&input.text, input.add_margin)
                .unwrap(),
        );
    }
    if input.agent_swarms_enabled && input.text.contains("<teammate-message") {
        return UserTextProjection::Teammate(
            crate::teammate_messages::project_user_teammate_messages(
                &input.text,
                input.is_transcript_mode,
            ),
        );
    }
    if input.text.contains("<task-notification>") {
        if let Some(notif) =
            crate::simple_messages::project_user_agent_notification(&input.text, input.add_margin)
        {
            return UserTextProjection::TaskNotification(notif);
        }
        // Malformed notification (missing <summary> tag) — fall
        // through to plain text rendering instead of panicking.
    }
    if input.text.contains("<mcp-resource-update") || input.text.contains("<mcp-polling-update") {
        return UserTextProjection::ResourceUpdates(
            crate::simple_messages::project_user_resource_update(&input.text),
        );
    }
    if input.fork_subagent_enabled && input.text.contains("<fork-boilerplate>") {
        return UserTextProjection::ForkBoilerplateDeferred;
    }
    if input.uds_inbox_enabled && input.text.contains("<cross-session-message") {
        return UserTextProjection::CrossSessionDeferred;
    }
    if input.channels_enabled && input.text.contains("<channel source=\"") {
        return UserTextProjection::Channel(
            crate::simple_messages::project_user_channel_message(&input.text, input.add_margin)
                .unwrap(),
        );
    }
    UserTextProjection::Prompt {
        add_margin: input.add_margin,
        text: input.text.clone(),
        verbose: input.verbose,
        is_transcript_mode: input.is_transcript_mode,
        timestamp: input.timestamp.clone(),
    }
}

/// Project a plain user prompt into visible lines, folding long middle sections.
pub fn project_user_prompt_display_lines(
    text: &str,
    verbose: bool,
    _is_transcript_mode: bool,
) -> Vec<UserPromptDisplayLine> {
    let lines = text.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    if verbose || lines.len() < USER_PROMPT_FOLD_THRESHOLD_LINES {
        return lines.into_iter().map(UserPromptDisplayLine::Text).collect();
    }

    let hidden_line_count = lines
        .len()
        .saturating_sub(USER_PROMPT_FOLD_HEAD_LINES + USER_PROMPT_FOLD_TAIL_LINES);
    if hidden_line_count == 0 {
        return lines.into_iter().map(UserPromptDisplayLine::Text).collect();
    }

    let mut display_lines =
        Vec::with_capacity(USER_PROMPT_FOLD_HEAD_LINES + 2 + USER_PROMPT_FOLD_TAIL_LINES);
    display_lines.extend(
        lines
            .iter()
            .take(USER_PROMPT_FOLD_HEAD_LINES)
            .cloned()
            .map(UserPromptDisplayLine::Text),
    );
    display_lines.push(UserPromptDisplayLine::HiddenSeparator { hidden_line_count });
    display_lines.push(UserPromptDisplayLine::Text(String::new()));
    display_lines.extend(
        lines
            .iter()
            .skip(lines.len() - USER_PROMPT_FOLD_TAIL_LINES)
            .cloned()
            .map(UserPromptDisplayLine::Text),
    );
    display_lines
}

/// Format the folded-prompt separator for the available terminal width.
pub fn format_user_prompt_hidden_separator(hidden_line_count: usize, width: usize) -> String {
    let line_word = if hidden_line_count == 1 {
        "line"
    } else {
        "lines"
    };
    let mut separator = format!("──── ({hidden_line_count} {line_word} hidden) ─");
    let current_width = separator.chars().count();
    if width == 0 {
        return String::new();
    }
    if current_width > width {
        return separator.chars().take(width).collect();
    }
    if width > current_width {
        separator.push_str(&"─".repeat(width - current_width));
    }
    separator
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str) -> UserTextInput {
        UserTextInput {
            add_margin: true,
            text: text.to_string(),
            verbose: false,
            plan_content: None,
            is_transcript_mode: false,
            timestamp: Some("ts".into()),
            github_webhooks_enabled: false,
            fork_subagent_enabled: false,
            uds_inbox_enabled: false,
            channels_enabled: false,
            agent_swarms_enabled: false,
        }
    }

    #[test]
    fn hides_no_content_tick_and_caveat() {
        assert_eq!(
            project_user_text(&input(NO_CONTENT_MESSAGE)),
            UserTextProjection::HiddenNoContent
        );
        assert_eq!(
            project_user_text(&input("<tick>x</tick>")),
            UserTextProjection::HiddenTick
        );
        assert_eq!(
            project_user_text(&input("<local-command-caveat>no</local-command-caveat>")),
            UserTextProjection::HiddenLocalCommandCaveat
        );
    }

    #[test]
    fn routes_plan_and_interrupt_and_prompt() {
        let mut i = input("plain");
        i.plan_content = Some("plan".into());
        assert!(matches!(
            project_user_text(&i),
            UserTextProjection::Plan { .. }
        ));

        assert_eq!(
            project_user_text(&input("[Request interrupted by user]")),
            UserTextProjection::Interrupted
        );

        assert!(matches!(
            project_user_text(&input("plain prompt")),
            UserTextProjection::Prompt { .. }
        ));
    }

    #[test]
    fn routes_bash_command_memory_and_channel_cases() {
        assert!(matches!(
            project_user_text(&input("<bash-input>ls</bash-input>")),
            UserTextProjection::BashInput(_)
        ));
        assert!(matches!(
            project_user_text(&input("<command-message>x</command-message>")),
            UserTextProjection::Command(_)
        ));
        assert!(matches!(
            project_user_text(&input("<user-memory-input>a</user-memory-input>")),
            UserTextProjection::MemoryInput(_)
        ));

        let mut i = input("<channel source=\"srv\">hi</channel>");
        i.channels_enabled = true;
        assert!(matches!(
            project_user_text(&i),
            UserTextProjection::Channel(_)
        ));
    }

    #[test]
    fn routes_teammate_task_notification_resource_and_deferred_feature_cases() {
        let mut i = input("<teammate-message teammate_id=\"a\">plain</teammate-message>");
        i.agent_swarms_enabled = true;
        assert!(matches!(
            project_user_text(&i),
            UserTextProjection::Teammate(_)
        ));

        let mut completed = input(
            "<teammate-message teammate_id=\"a\">{\"type\":\"idle_notification\",\"idleReason\":\"available\"}</teammate-message>",
        );
        completed.agent_swarms_enabled = true;
        assert!(project_user_text(&completed).is_teammate_task_completion());

        assert!(matches!(
            project_user_text(&input(
                "<task-notification><summary>x</summary></task-notification>"
            )),
            UserTextProjection::TaskNotification(_)
        ));
        assert!(matches!(
            project_user_text(&input(
                "<mcp-resource-update server=\"s\" uri=\"file:///a\"></mcp-resource-update>"
            )),
            UserTextProjection::ResourceUpdates(_)
        ));

        let mut g = input("<github-webhook-activity>e</github-webhook-activity>");
        g.github_webhooks_enabled = true;
        assert_eq!(
            project_user_text(&g),
            UserTextProjection::GitHubWebhookDeferred
        );
    }

    #[test]
    fn folds_long_plain_prompt_middle_lines() {
        let text = (1..=46)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let folded = project_user_prompt_display_lines(&text, false, false);

        assert_eq!(folded.len(), 22);
        assert_eq!(folded[0], UserPromptDisplayLine::Text("line 1".into()));
        assert_eq!(folded[9], UserPromptDisplayLine::Text("line 10".into()));
        assert_eq!(
            folded[10],
            UserPromptDisplayLine::HiddenSeparator {
                hidden_line_count: 26
            }
        );
        assert_eq!(folded[11], UserPromptDisplayLine::Text(String::new()));
        assert_eq!(folded[12], UserPromptDisplayLine::Text("line 37".into()));
        assert_eq!(folded[21], UserPromptDisplayLine::Text("line 46".into()));
    }

    #[test]
    fn verbose_prompt_display_lines_do_not_fold() {
        let text = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let verbose = project_user_prompt_display_lines(&text, true, false);

        assert_eq!(verbose.len(), 20);
        assert!(verbose
            .iter()
            .all(|line| matches!(line, UserPromptDisplayLine::Text(_))));
    }

    #[test]
    fn transcript_mode_still_folds_long_prompt() {
        let text = (1..=30)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let transcript = project_user_prompt_display_lines(&text, false, true);

        assert_eq!(transcript.len(), 22);
        assert_eq!(
            transcript[10],
            UserPromptDisplayLine::HiddenSeparator {
                hidden_line_count: 10
            }
        );
        assert_eq!(transcript[11], UserPromptDisplayLine::Text(String::new()));
    }

    #[test]
    fn hidden_separator_fills_available_width() {
        let separator = format_user_prompt_hidden_separator(38, 60);

        assert_eq!(separator.chars().count(), 60);
        assert!(separator.starts_with("──── (38 lines hidden) ─"));
    }
}
