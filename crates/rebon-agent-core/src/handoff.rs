//! Catching a cold agent up on the conversation so far.
//!
//! An external agent that starts a fresh session — because the user
//! switched agents, or because the session it had could not be loaded
//! — has never seen the conversation the user is looking at. Asked
//! "now fix the other one too", it has no idea what the other one is.
//!
//! Rebon cannot replay a conversation into somebody else's process,
//! but it can describe it. This module turns the tail of the session
//! transcript into a readable digest that gets prepended to that
//! agent's first prompt, once.
//!
//! What survives the projection is what a person would need to follow
//! the thread: the words. Tool calls are named but not reproduced, and
//! their results are left out entirely — a handoff is a summary of the
//! conversation, not a re-run of it, and pasting thousands of lines of
//! tool output into a fresh context helps nobody.

/// How much of the tail to carry over. Sized to be generous for a
/// normal conversation while staying far below any agent's context:
/// a handoff that blew the window would defeat itself.
pub const MAX_HANDOFF_CHARS: usize = 48_000;

/// Build the handoff block, or `None` when there is no conversation
/// worth handing over.
///
/// Takes the *tail* of the conversation: the recent exchanges are what
/// the next message will refer to, and the opening of a long session
/// is the first thing a person would drop too.
pub fn handoff_document(messages: &[rebon_api::Message], max_chars: usize) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;

    for message in messages.iter().rev() {
        let Some(rendered) = render_message(message) else {
            continue;
        };
        if used + rendered.len() > max_chars {
            truncated = true;
            break;
        }
        used += rendered.len();
        lines.push(rendered);
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();

    let mut out = String::with_capacity(used + 512);
    out.push_str(
        "<rebon-handoff>\nThis session already has a conversation, which you are joining \
         mid-way. It happened with a different agent, so you have none of it in your own \
         history. Read it for context, then answer the message that follows. Do not reply \
         to this summary itself.\n\n",
    );
    if truncated {
        out.push_str("[…earlier turns omitted…]\n\n");
    }
    out.push_str(&lines.join("\n\n"));
    out.push_str("\n</rebon-handoff>");
    Some(out)
}

/// One message as a line of dialogue, or `None` when it carries
/// nothing a reader needs.
fn render_message(message: &rebon_api::Message) -> Option<String> {
    let speaker = match message.role {
        rebon_api::Role::User => "User",
        rebon_api::Role::Assistant => "Assistant",
        // System messages are instructions to the engine, not part of
        // the conversation a joining agent needs.
        rebon_api::Role::System => return None,
    };

    let mut text = String::new();
    let mut tools: Vec<&str> = Vec::new();
    for block in &message.content {
        if let Some(chunk) = block.as_text() {
            let chunk = chunk.trim();
            // Rebon's own bookkeeping markers are addressed to the
            // engine, not to a reader.
            if chunk.is_empty() || chunk.starts_with("<rebon-") || chunk.starts_with("<system-") {
                continue;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(chunk);
        } else if let Some(tool_use) = block.as_tool_use() {
            tools.push(tool_use.name.as_str());
        }
        // Tool results are deliberately dropped: they are the bulk of
        // a transcript and the least useful part of a summary.
    }

    if text.is_empty() && tools.is_empty() {
        return None;
    }
    let mut rendered = format!("{speaker}: {text}");
    if !tools.is_empty() {
        // Named, not reproduced — enough to know work happened and
        // roughly what kind.
        rendered.push_str(&format!("\n[used tools: {}]", tools.join(", ")));
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{ContentBlock, Message, Role, TextBlock, ToolUseBlock};

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text(TextBlock { text: text.into() })],
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(TextBlock { text: text.into() })],
        }
    }

    #[test]
    fn a_conversation_becomes_readable_dialogue() {
        let doc = handoff_document(
            &[
                user("add a retry to the uploader"),
                assistant("Done — it retries three times."),
                user("now do the same for the downloader"),
            ],
            MAX_HANDOFF_CHARS,
        )
        .expect("a conversation to hand over");

        assert!(doc.starts_with("<rebon-handoff>"));
        assert!(doc.ends_with("</rebon-handoff>"));
        assert!(doc.contains("User: add a retry to the uploader"));
        assert!(doc.contains("Assistant: Done — it retries three times."));
        assert!(
            doc.find("add a retry").unwrap() < doc.find("now do the same").unwrap(),
            "the dialogue must read in the order it happened"
        );
    }

    #[test]
    fn tools_are_named_but_their_output_is_left_behind() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "Let me look.".into(),
                }),
                ContentBlock::ToolUse(ToolUseBlock {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "/repo/src/main.rs"}),
                }),
            ],
        };
        let doc = handoff_document(&[message], MAX_HANDOFF_CHARS).expect("rendered");
        assert!(doc.contains("Assistant: Let me look."));
        assert!(doc.contains("[used tools: Read]"));
        assert!(
            !doc.contains("main.rs"),
            "tool arguments are noise in a summary: {doc}"
        );
    }

    #[test]
    fn an_empty_or_bookkeeping_only_conversation_hands_over_nothing() {
        assert!(handoff_document(&[], MAX_HANDOFF_CHARS).is_none());
        assert!(handoff_document(
            &[user("<rebon-queued-user-input uuid=\"u-1\" />")],
            MAX_HANDOFF_CHARS
        )
        .is_none());
    }

    #[test]
    fn a_long_conversation_keeps_its_tail_and_says_what_it_dropped() {
        let mut messages = Vec::new();
        for index in 0..200 {
            messages.push(user(&format!("message {index} {}", "x".repeat(200))));
        }
        let doc = handoff_document(&messages, 2_000).expect("rendered");

        assert!(
            doc.len() < 3_000,
            "the cap must actually bind: {}",
            doc.len()
        );
        assert!(doc.contains("earlier turns omitted"));
        assert!(
            doc.contains("message 199"),
            "the most recent turns are the ones that matter"
        );
        assert!(!doc.contains("message 0 "), "the opening should be dropped");
    }
}
