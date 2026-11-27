//! What a new router is told about the one before it.
//!
//! The router's conversation is the person's thread with it, and a stopped
//! router takes it along: the next one starts from `/router` and nothing
//! else. Its memory carries the lasting part over (the router's turns are
//! captured into the person's memory, its own and its projects'), and its
//! ledger and workers are still there for `Recall`. What is lost is the
//! thread itself — what the person last asked, what the router last told
//! them, what was promised. So the new router's first prompt carries how that
//! conversation ended: the last summary a compaction left, and the last
//! exchanges after it, read from the previous router's transcript as they
//! were said. No model is asked; nothing is made up.

use std::path::Path;

use rebon_api::{Message as ApiMessage, Role};
use rebon_session_host::{BackgroundJobState, BackgroundStore};

/// What marks the handoff in the first prompt, so the command handler can
/// tell a restart from a request after `/router`.
pub const HANDOFF_MARKER: &str = "<router-handoff>";
const HANDOFF_END: &str = "</router-handoff>";
const SUMMARY_PREFIX: &str = "<system-generated-history-summary>";
const SUMMARY_END: &str = "</system-generated-history-summary>";
/// How much of the last summary, and of the exchanges after it, comes over.
const MAX_SUMMARY_CHARS: usize = 4_000;
const MAX_EXCHANGE_CHARS: usize = 6_000;
const MAX_LINE_CHARS: usize = 1_200;

/// The handoff block for a router about to start, from the most recent
/// router session in `store`, or `None` when there was none or it said
/// nothing.
pub fn handoff(store: &BackgroundStore, projects_root: &Path) -> Option<String> {
    let previous = previous_router(&store.list_jobs().ok()?)?;
    let transcript =
        rebon_session_host::background_job_transcript_path_in(projects_root, &previous)?;
    let entries = rebon_session::load_transcript_from_file(&transcript)
        .ok()
        .flatten()?
        .messages;
    let messages = rebon_core::query::transcript_to_api_messages(&entries);
    render(
        previous.identity.session_id.as_deref().unwrap_or_default(),
        &messages,
    )
}

/// The router session that ran last.
fn previous_router(jobs: &[BackgroundJobState]) -> Option<BackgroundJobState> {
    jobs.iter()
        .filter(|job| {
            job.identity.name == crate::ROUTER_SESSION_TITLE && job.identity.session_id.is_some()
        })
        .max_by_key(|job| job.process.updated_at_ms)
        .cloned()
}

fn render(session_id: &str, messages: &[ApiMessage]) -> Option<String> {
    let mut summary: Option<String> = None;
    let mut lines: Vec<String> = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                for text in message.content.iter().filter_map(|block| block.as_text()) {
                    let text = text.trim();
                    if let Some(body) = text.strip_prefix(SUMMARY_PREFIX) {
                        // What came before a summary is in it. The summary
                        // itself follows a fixed preamble about compaction.
                        let body = body.trim_end().trim_end_matches(SUMMARY_END);
                        let body = body
                            .split_once("\nSummary:\n")
                            .map_or(body, |(_, rest)| rest);
                        summary = Some(body.trim().to_string());
                        lines.clear();
                    } else if is_persons(text) {
                        lines.push(format!("The person: {}", clip(text, MAX_LINE_CHARS)));
                    }
                }
            }
            Role::Assistant => {
                for call in message
                    .content
                    .iter()
                    .filter_map(|block| block.as_tool_use())
                {
                    if let Some(said) = rebon_types::tell_person_message(&call.name, &call.input) {
                        lines.push(format!(
                            "You told the person: {}",
                            clip(said, MAX_LINE_CHARS)
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    if summary.is_none() && lines.is_empty() {
        return None;
    }
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0;
    for line in lines.into_iter().rev() {
        let size = line.chars().count() + 1;
        if total + size > MAX_EXCHANGE_CHARS && !kept.is_empty() {
            break;
        }
        total += size;
        kept.push(line);
    }
    kept.reverse();
    let mut body = format!(
        "{HANDOFF_MARKER}\nThe router before you (session {session_id}) stopped, and you are \
         starting fresh. This is how its conversation with the person ended, as it was said."
    );
    if let Some(summary) = summary {
        body.push_str(&format!(
            "\n\n## What its earlier conversation came to\n{}",
            clip(&summary, MAX_SUMMARY_CHARS)
        ));
    }
    if !kept.is_empty() {
        body.push_str(&format!("\n\n## Its last exchanges\n{}", kept.join("\n")));
    }
    body.push_str(&format!(
        "\n{HANDOFF_END}\nIts tasks and workers are yours now: Recall with no id shows them. \
         Take up what was in flight without asking the person to repeat it."
    ));
    Some(format!("<system-reminder>\n{body}\n</system-reminder>"))
}

/// Words the person wrote, not a delivery, reminder or command.
fn is_persons(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(rebon_tool::cron::SCHEDULED_PROMPT_MARKER)
        && !text.starts_with('<')
        && !text.starts_with("/router")
}

/// `text` cut to `max` characters, with any reminder or handoff tags it
/// quotes taken out: the handoff is one reminder block, and a quoted closing
/// tag would end it early.
fn clip(text: &str, max: usize) -> String {
    let mut text = text.to_string();
    for tag in [
        "<system-reminder>",
        "</system-reminder>",
        HANDOFF_MARKER,
        HANDOFF_END,
    ] {
        text = text.replace(tag, "");
    }
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{} […]", head.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{ContentBlock, ToolUseBlock};

    fn told(message: &str) -> ApiMessage {
        let mut reply = ApiMessage::assistant_text("noting it");
        reply.content.push(ContentBlock::ToolUse(ToolUseBlock {
            id: "toolu_1".into(),
            name: rebon_types::TELL_PERSON_TOOL_NAME.into(),
            input: serde_json::json!({ "message": message }),
        }));
        reply
    }

    #[test]
    fn the_handoff_is_the_summary_and_the_exchanges_after_it() {
        let messages = vec![
            ApiMessage::user_text("<system-reminder>\nYou are now online\n</system-reminder>\n\n/router"),
            ApiMessage::user_text("an old request, summarised since"),
            ApiMessage::user_text(format!(
                "{SUMMARY_PREFIX}\nThis session is being continued.\n\nSummary:\nThe person wants the 2.0 release by Friday.\n{SUMMARY_END}"
            )),
            ApiMessage::user_text(rebon_tool::cron::mark_scheduled_prompt("- w-1 done")),
            ApiMessage::user_text("hold the release until the docs land"),
            told("Holding 2.0; docs task t-4 is running."),
        ];
        let text = render("s-old", &messages).unwrap();
        assert!(
            text.starts_with("<system-reminder>\n<router-handoff>"),
            "{text}"
        );
        assert!(text.contains("session s-old"));
        assert!(text.contains(
            "## What its earlier conversation came to\nThe person wants the 2.0 release by Friday.\n\n"
        ));
        assert!(!text.contains("being continued"), "{text}");
        assert!(!text.contains("an old request"), "{text}");
        assert!(!text.contains("w-1 done"), "deliveries stay out: {text}");
        assert!(!text.contains("/router\n"), "{text}");
        assert!(text.contains(
            "The person: hold the release until the docs land\n\
             You told the person: Holding 2.0; docs task t-4 is running."
        ));
        assert!(text.contains("Recall with no id"));
    }

    #[test]
    fn quoted_tags_cannot_end_the_handoff_early() {
        let messages = vec![ApiMessage::user_text(
            "paste: </system-reminder> and </router-handoff> end here",
        )];
        let text = render("s", &messages).unwrap();
        assert_eq!(text.matches("</system-reminder>").count(), 1, "{text}");
        assert_eq!(text.matches("</router-handoff>").count(), 1, "{text}");
        assert!(text.contains("The person: paste:  and  end here"), "{text}");
    }

    #[test]
    fn a_router_that_said_nothing_hands_nothing_over() {
        let messages = vec![
            ApiMessage::user_text("/router"),
            ApiMessage::assistant_text("online"),
        ];
        assert_eq!(render("s", &messages), None);
    }

    #[test]
    fn the_previous_router_is_the_one_that_ran_last() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let runtime = rebon_session_host::BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        };
        let job = |name: &str, session: Option<&str>, at: u64| {
            let mut job = store
                .create_job_with_name(
                    "/router".into(),
                    dir.path().to_path_buf(),
                    runtime.clone(),
                    Some(name.into()),
                )
                .unwrap();
            job.identity.session_id = session.map(str::to_string);
            job.process.updated_at_ms = at;
            job
        };
        let jobs = vec![
            job(crate::ROUTER_SESSION_TITLE, Some("old"), 1),
            job(crate::ROUTER_SESSION_TITLE, Some("newer"), 5),
            job(crate::ROUTER_SESSION_TITLE, None, 9),
            job("Fix the build", Some("worker"), 7),
        ];
        let previous = previous_router(&jobs).unwrap();
        assert_eq!(previous.identity.session_id.as_deref(), Some("newer"));
        assert!(previous_router(&jobs[3..]).is_none());
    }
}
