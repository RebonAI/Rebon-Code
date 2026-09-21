//! Small message-rendering projections shared by user, shell, hook,
//! resource-update, and local-command rows.
//!
//! The functions in this module parse lightweight message payloads into
//! plain display structs: status tones, margins, truncated bodies, file
//! links, hook progress text, and grouped tool-use summaries.

use std::collections::{HashMap, HashSet};

use crate::common::{
    display_server_name, extract_tag, parse_channel_message, parse_resource_updates,
    simple_file_url, truncate_to_width, ResourceUpdateKind,
};
use crate::tool_grouping::{
    build_grouped_tool_use_data as build_grouped_tool_use_data_inner, GroupedToolUseInput,
    GroupedToolUseRenderRequest,
};

/// User agent notification tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusColor {
    /// Success tone.
    Success,
    /// Error tone.
    Error,
    /// Warning tone.
    Warning,
    /// Default tone.
    Text,
}

/// Compact user-bash input row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashInputDisplay {
    /// Extracted bash input text.
    pub input: String,
    /// Top margin flag projected to a numeric value.
    pub margin_top: u8,
}

/// Parsed stdout/stderr for user bash output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashOutputDisplay {
    /// Stdout text.
    pub stdout: String,
    /// Stderr text.
    pub stderr: String,
    /// Verbose flag.
    pub verbose: bool,
}

/// User agent notification row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNotificationDisplay {
    /// Summary text.
    pub summary: String,
    /// Status tone.
    pub color: StatusColor,
    /// Top margin.
    pub margin_top: u8,
}

/// User channel message display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserChannelDisplay {
    /// Display server label.
    pub source_label: String,
    /// Optional ` · user` suffix.
    pub user_suffix: Option<String>,
    /// Truncated body text.
    pub body: String,
    /// Top margin.
    pub margin_top: u8,
}

/// User command display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCommandDisplay {
    /// Final displayed content.
    pub content: String,
    /// Skill-format branch flag.
    pub is_skill_format: bool,
    /// Top margin.
    pub margin_top: u8,
}

/// User image display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserImageDisplay {
    /// `[Image]` or `[Image #N]`.
    pub label: String,
    /// Optional file URL when hyperlinks are recognized.
    pub file_url: Option<String>,
    /// Whether the image starts a new user turn.
    pub add_margin: bool,
}

/// Local command output block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCommandOutputBlock {
    /// Markdown block.
    Markdown(String),
    /// Cloud-launch special row.
    CloudLaunch(CloudLaunchDisplay),
}

/// Special diamond-prefixed cloud launch projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudLaunchDisplay {
    /// Leading diamond glyph.
    pub diamond: char,
    /// Header label.
    pub label: String,
    /// Optional suffix beginning with ` · `.
    pub suffix: String,
    /// Optional remainder line.
    pub rest: Option<String>,
}

/// Local command output display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCommandOutputDisplay {
    /// Empty-content fallback.
    NoContent,
    /// One or more content blocks.
    Blocks(Vec<LocalCommandOutputBlock>),
}

/// Memory input display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMemoryInputDisplay {
    /// Extracted memory text.
    pub input: String,
    /// Saving status copy.
    pub saving_text: &'static str,
    /// Top margin.
    pub margin_top: u8,
}

/// Plan message display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPlanDisplay {
    /// Static title.
    pub title: &'static str,
    /// Markdown body.
    pub plan_content: String,
    /// Top margin.
    pub margin_top: u8,
}

/// Resource update row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceUpdateDisplay {
    /// Server name.
    pub server: String,
    /// Display target.
    pub target: String,
    /// Optional reason.
    pub reason: Option<String>,
    /// Update kind.
    pub kind: ResourceUpdateKind,
}

/// Hook progress display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookProgressDisplay {
    /// Transcript-mode static summary.
    TranscriptSummary {
        /// Number of in-progress hooks.
        in_progress_count: usize,
        /// Hook event name.
        hook_event: String,
        /// `" hook ran"` or `" hooks ran"`.
        plural_suffix: &'static str,
    },
    /// Running in-progress message.
    Running {
        /// Hook event name.
        hook_event: String,
        /// `" hook…"` or `" hooks…"`.
        plural_suffix: &'static str,
    },
}

/// Map command status strings to their notification tone.
pub fn get_status_color(status: Option<&str>) -> StatusColor {
    match status {
        Some("completed") => StatusColor::Success,
        Some("failed") => StatusColor::Error,
        Some("killed") => StatusColor::Warning,
        _ => StatusColor::Text,
    }
}

/// Fixed copy for a redacted thinking block.
pub fn project_assistant_redacted_thinking(_add_margin: bool) -> &'static str {
    "✻ Thinking…"
}

/// Project an agent notification payload into its display row.
pub fn project_user_agent_notification(
    text: &str,
    add_margin: bool,
) -> Option<AgentNotificationDisplay> {
    let summary = xml_unescape_text(&extract_tag(text, "summary")?);
    let status = extract_tag(text, "status");
    Some(AgentNotificationDisplay {
        summary,
        color: get_status_color(status.as_deref()),
        margin_top: if add_margin { 1 } else { 0 },
    })
}

fn xml_unescape_text(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Extract the `bash-input` tag into a display row.
pub fn project_user_bash_input(text: &str, add_margin: bool) -> Option<BashInputDisplay> {
    extract_tag(text, "bash-input").map(|input| BashInputDisplay {
        input,
        margin_top: if add_margin { 1 } else { 0 },
    })
}

/// Parse the `bash-stdout` / `bash-stderr` tags into a display row.
/// `None` when both are empty.
pub fn parse_bash_output_tags(content: &str, verbose: bool) -> Option<BashOutputDisplay> {
    let raw_stdout = extract_tag(content, "bash-stdout").unwrap_or_default();
    let stdout = extract_tag(&raw_stdout, "persisted-output").unwrap_or(raw_stdout);
    let stderr = extract_tag(content, "bash-stderr").unwrap_or_default();
    if stdout.is_empty() && stderr.is_empty() {
        return None;
    }
    Some(BashOutputDisplay {
        stdout,
        stderr,
        verbose,
    })
}

/// Project a channel message into its display row.
pub fn project_user_channel_message(text: &str, add_margin: bool) -> Option<UserChannelDisplay> {
    let parsed = parse_channel_message(text)?;
    Some(UserChannelDisplay {
        source_label: display_server_name(&parsed.source),
        user_suffix: parsed.user.map(|u| format!(" · {u}")),
        body: truncate_to_width(&parsed.body, 60),
        margin_top: if add_margin { 1 } else { 0 },
    })
}

/// Extract the `command-message` tag into a display row.
pub fn project_user_command_message(text: &str, add_margin: bool) -> Option<UserCommandDisplay> {
    let command = extract_tag(text, "command-message")?;
    let args = extract_tag(text, "command-args");
    let is_skill_format = extract_tag(text, "skill-format").as_deref() == Some("true");
    let content = if is_skill_format {
        format!("Skill({command})")
    } else {
        let parts = [Some(command.clone()), args]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        format!("/{}", parts.join(" "))
    };
    Some(UserCommandDisplay {
        content,
        is_skill_format,
        margin_top: if add_margin { 1 } else { 0 },
    })
}

/// Label an image message and link its stored path when the terminal
/// supports hyperlinks.
pub fn project_user_image_message(
    image_id: Option<usize>,
    add_margin: bool,
    stored_image_path: Option<&str>,
    supports_hyperlinks: bool,
) -> UserImageDisplay {
    let label = image_id
        .map(|id| format!("[Image #{id}]"))
        .unwrap_or_else(|| "[Image]".to_string());
    let file_url = if supports_hyperlinks {
        stored_image_path.map(simple_file_url)
    } else {
        None
    };
    UserImageDisplay {
        label,
        file_url,
        add_margin,
    }
}

/// Parse the local-command stdout/stderr tags into output blocks.
pub fn project_user_local_command_output(content: &str) -> LocalCommandOutputDisplay {
    let stdout = extract_tag(content, "local-command-stdout");
    let stderr = extract_tag(content, "local-command-stderr");
    if stdout.is_none() && stderr.is_none() {
        return LocalCommandOutputDisplay::NoContent;
    }
    let mut lines = Vec::new();
    if let Some(stdout) = stdout.filter(|v| !v.trim().is_empty()) {
        lines.push(parse_local_output_block(stdout.trim()));
    }
    if let Some(stderr) = stderr.filter(|v| !v.trim().is_empty()) {
        lines.push(parse_local_output_block(stderr.trim()));
    }
    LocalCommandOutputDisplay::Blocks(lines)
}

fn parse_local_output_block(text: &str) -> LocalCommandOutputBlock {
    if text.starts_with("◇ ") || text.starts_with("◆ ") {
        return LocalCommandOutputBlock::CloudLaunch(parse_cloud_launch(text));
    }
    LocalCommandOutputBlock::Markdown(text.to_string())
}

fn parse_cloud_launch(children: &str) -> CloudLaunchDisplay {
    let diamond = children.chars().next().unwrap_or('◇');
    let start = diamond.len_utf8() + 1;
    let nl = children.find('\n');
    let header = match nl {
        Some(idx) => &children[start..idx],
        None => &children[start..],
    };
    let rest = nl
        .map(|idx| children[idx + 1..].trim().to_string())
        .filter(|s| !s.is_empty());
    let sep = header.find(" · ");
    let (label, suffix) = match sep {
        Some(idx) => (header[..idx].to_string(), header[idx..].to_string()),
        None => (header.to_string(), String::new()),
    };
    CloudLaunchDisplay {
        diamond,
        label,
        suffix,
        rest,
    }
}

/// The three fixed memory-saving replies.
pub fn user_memory_saving_texts() -> [&'static str; 3] {
    ["Got it.", "Good to know.", "Noted."]
}

/// Extract the `user-memory-input` tag into a display row.
pub fn project_user_memory_input(text: &str, add_margin: bool) -> Option<UserMemoryInputDisplay> {
    let input = extract_tag(text, "user-memory-input")?;
    Some(UserMemoryInputDisplay {
        input,
        saving_text: user_memory_saving_texts()[0],
        margin_top: if add_margin { 1 } else { 0 },
    })
}

/// Project a plan message into its display row.
pub fn project_user_plan_message(plan_content: &str, add_margin: bool) -> UserPlanDisplay {
    UserPlanDisplay {
        title: "Plan to implement",
        plan_content: plan_content.to_string(),
        margin_top: if add_margin { 1 } else { 0 },
    }
}

/// Project resource updates into their display rows.
pub fn project_user_resource_update(text: &str) -> Vec<ResourceUpdateDisplay> {
    parse_resource_updates(text)
        .into_iter()
        .map(|update| ResourceUpdateDisplay {
            server: update.server,
            target: if update.kind == ResourceUpdateKind::Resource {
                if update.target.starts_with("file://") {
                    update
                        .target
                        .trim_start_matches("file://")
                        .rsplit('/')
                        .next()
                        .unwrap_or(update.target.as_str())
                        .to_string()
                } else {
                    truncate_to_width(&update.target, 40)
                }
            } else {
                update.target
            },
            reason: update.reason,
            kind: update.kind,
        })
        .collect()
}

/// Project hook progress. `None` when no hook is in progress.
pub fn project_hook_progress(
    hook_event: &str,
    in_progress_hook_count: usize,
    resolved_hook_count: usize,
    is_transcript_mode: bool,
) -> Option<HookProgressDisplay> {
    if in_progress_hook_count == 0 {
        return None;
    }
    if hook_event == "PreToolUse" || hook_event == "PostToolUse" {
        if is_transcript_mode {
            return Some(HookProgressDisplay::TranscriptSummary {
                in_progress_count: in_progress_hook_count,
                hook_event: hook_event.to_string(),
                plural_suffix: if in_progress_hook_count == 1 {
                    " hook ran"
                } else {
                    " hooks ran"
                },
            });
        }
        return None;
    }
    if resolved_hook_count == in_progress_hook_count {
        return None;
    }
    Some(HookProgressDisplay::Running {
        hook_event: hook_event.to_string(),
        plural_suffix: if in_progress_hook_count == 1 {
            " hook…"
        } else {
            " hooks…"
        },
    })
}

/// Forwarding wrapper around `tool_grouping::build_grouped_tool_use_data`.
pub fn build_grouped_tool_use_data(
    tool_name: &str,
    entries: &[GroupedToolUseInput],
    resolved_tool_use_ids: &HashSet<String>,
    errored_tool_use_ids: &HashSet<String>,
    in_progress_tool_use_ids: &HashSet<String>,
    result_payloads: &HashMap<String, String>,
    should_animate: bool,
) -> GroupedToolUseRenderRequest {
    build_grouped_tool_use_data_inner(
        tool_name,
        entries,
        resolved_tool_use_ids,
        errored_tool_use_ids,
        in_progress_tool_use_ids,
        result_payloads,
        should_animate,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_notification_extracts_summary_and_status_color() {
        let d = project_user_agent_notification(
            "<summary>Agent &quot;研究&quot; &amp; verify</summary><status>completed</status>",
            true,
        )
        .unwrap();
        assert_eq!(d.summary, "Agent \"研究\" & verify");
        assert_eq!(d.color, StatusColor::Success);
        assert_eq!(d.margin_top, 1);
    }

    #[test]
    fn bash_input_extracts_tag() {
        let d = project_user_bash_input("<bash-input>ls</bash-input>", false).unwrap();
        assert_eq!(d.input, "ls");
    }

    #[test]
    fn bash_output_unwraps_persisted_output() {
        let d = parse_bash_output_tags(
            "<bash-stdout><persisted-output>out</persisted-output></bash-stdout><bash-stderr>err</bash-stderr>",
            true,
        )
        .unwrap();
        assert_eq!(d.stdout, "out");
        assert_eq!(d.stderr, "err");
        assert!(d.verbose);
    }

    #[test]
    fn user_channel_formats_source_user_and_body() {
        let d = project_user_channel_message(
            "<channel source=\"plugin:slack-channel:slack\" user=\"alice\">hello there</channel>",
            true,
        )
        .unwrap();
        assert_eq!(d.source_label, "slack");
        assert_eq!(d.user_suffix.as_deref(), Some(" · alice"));
    }

    #[test]
    fn user_command_handles_skill_and_slash_modes() {
        let skill = project_user_command_message(
            "<command-message>x</command-message><skill-format>true</skill-format>",
            false,
        )
        .unwrap();
        assert_eq!(skill.content, "Skill(x)");
        let slash = project_user_command_message(
            "<command-message>x</command-message><command-args>y</command-args>",
            false,
        )
        .unwrap();
        assert_eq!(slash.content, "/x y");
    }

    #[test]
    fn user_image_builds_label_and_file_url() {
        let d = project_user_image_message(Some(3), false, Some(r"C:\img.png"), true);
        assert_eq!(d.label, "[Image #3]");
        assert!(d.file_url.unwrap().starts_with("file:///"));
    }

    #[test]
    fn local_command_output_returns_no_content_or_cloud_launch() {
        assert_eq!(
            project_user_local_command_output("plain"),
            LocalCommandOutputDisplay::NoContent
        );
        let d = project_user_local_command_output(
            "<local-command-stdout>◇ Deploy · prod\nok</local-command-stdout>",
        );
        let LocalCommandOutputDisplay::Blocks(blocks) = d else {
            panic!("expected blocks");
        };
        assert!(matches!(blocks[0], LocalCommandOutputBlock::CloudLaunch(_)));
    }

    #[test]
    fn user_memory_input_extracts_tag() {
        let d =
            project_user_memory_input("<user-memory-input>remember me</user-memory-input>", true)
                .unwrap();
        assert_eq!(d.input, "remember me");
        assert_eq!(d.saving_text, "Got it.");
    }

    #[test]
    fn user_plan_message_pins_title() {
        let d = project_user_plan_message("abc", true);
        assert_eq!(d.title, "Plan to implement");
    }

    #[test]
    fn user_resource_update_parses_and_formats_targets() {
        let d = project_user_resource_update(
            "<mcp-resource-update server=\"srv\" uri=\"file:///tmp/a.txt\"><reason>why</reason></mcp-resource-update>",
        );
        assert_eq!(d[0].server, "srv");
        assert_eq!(d[0].target, "a.txt");
        assert_eq!(d[0].reason.as_deref(), Some("why"));
    }

    #[test]
    fn hook_progress_transcript_and_running_branches() {
        assert!(matches!(
            project_hook_progress("PreToolUse", 2, 0, true),
            Some(HookProgressDisplay::TranscriptSummary { .. })
        ));
        assert!(matches!(
            project_hook_progress("PostHook", 2, 1, false),
            Some(HookProgressDisplay::Running { .. })
        ));
    }

    #[test]
    fn grouped_tool_use_delegates_to_tool_grouping() {
        let entries = vec![GroupedToolUseInput {
            tool_name: "Agent".into(),
            tool_use_id: "u1".into(),
            progress_messages: vec![],
        }];
        let req = build_grouped_tool_use_data(
            "Agent",
            &entries,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::from(["u1".to_string()]),
            &HashMap::new(),
            true,
        );
        assert!(req.should_animate);
    }
}
