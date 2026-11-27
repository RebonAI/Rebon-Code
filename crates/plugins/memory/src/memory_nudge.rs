//! A reminder to save, beside a message that looks like it says something
//! lasting.
//!
//! The memory section asks the model to save on its own, and it mostly does,
//! but it sits at the head of the context, and a short turn is answered
//! without a look back at it: "from now on answer in Chinese — just reply OK"
//! gets "OK" in two seconds and nothing saved. Measured on deepseek-v4.1-flash
//! over twenty scenarios, nearly every miss was such a turn. So when the
//! person's message carries the words people use for a standing preference, a
//! fact about themselves, or a rule of the project, one short reminder is put
//! right after it, before the turn's first request. The model still decides;
//! a message that only happens to say "always" gets a reminder it can ignore.
//!
//! Appended to the turn, never spliced into the prefix, so the cache keeps.
//! Only where the turn offers SaveMemory, and only for words the person wrote:
//! a scheduled prompt, a reminder or a digest is no one's preference.

use std::sync::Arc;

use rebon_api::{make_meta_user_message, Message as ApiMessage, Role};
use rebon_core::attachment_seat::{SeatAttachmentProducer, SessionAttachmentBinding, TurnToolkit};
use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};

/// What the reminder says.
pub const MEMORY_NUDGE: &str = "If the message above tells you something lasting (a preference \
or correction, who the user is, or a rule, convention or fact about this project), save it with \
SaveMemory now, before you reply: user scope if it holds in every project, repo scope if only \
here. A request to answer briefly limits the reply, not the save. If it tells you nothing \
lasting, ignore this.";

/// Words a standing preference, a fact about the person or a project rule is
/// said with, lowercased. English and Chinese, the languages it was measured
/// in.
const SIGNALS: &[&str] = &[
    "from now on",
    "going forward",
    "always",
    "never",
    "i prefer",
    "i'd prefer",
    "i like",
    "i hate",
    "i don't like",
    "please stop",
    "stop using",
    "don't ever",
    "remember",
    "keep in mind",
    "fyi",
    "i'm a",
    "i am a",
    "i'm the",
    "i work",
    "i use",
    "i mostly",
    "in this repo",
    "in this project",
    "for this repo",
    "for this project",
    "for this app",
    "we use",
    "we decided",
    "convention",
    "must be",
    "every project",
    "以后",
    "从现在起",
    "今后",
    "总是",
    "永远",
    "一律",
    "不要",
    "别再",
    "记住",
    "记得",
    "我是",
    "我平时",
    "我习惯",
    "我喜欢",
    "我讨厌",
    "我用",
    "这个项目",
    "这个仓库",
    "本项目",
    "我们用",
    "我们决定",
    "规定",
    "必须",
    "约定",
    "流程",
    "所有项目",
];

/// Whether `text` uses any of [`SIGNALS`].
pub fn says_something_lasting(text: &str) -> bool {
    let text = text.to_lowercase();
    SIGNALS.iter().any(|signal| text.contains(signal))
}

/// The person's words in the newest user message of `history`, or `None`
/// when it is a delivery: a scheduled prompt, a reminder, a tool result.
fn newest_person_text(history: &[ApiMessage]) -> Option<String> {
    let message = history
        .iter()
        .rev()
        .find(|message| message.role == Role::User)?;
    let text: Vec<&str> = message
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(str::trim)
        .filter(|text| {
            !text.is_empty()
                && !text.starts_with('<')
                && !text.starts_with(rebon_tool::cron::SCHEDULED_PROMPT_MARKER)
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

pub struct MemoryNudgePoller {
    toolkit: Option<Arc<dyn TurnToolkit>>,
}

impl AttachmentPoller for MemoryNudgePoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        // Before the turn's first request only: later polls are the turn's
        // own work, and the reply already started.
        if request.phase != AttachmentPollPhase::Eager || request.next_iteration != 0 {
            return Vec::new();
        }
        if !self
            .toolkit
            .as_ref()
            .is_some_and(|toolkit| toolkit.has_tool(crate::SAVE_MEMORY_TOOL_NAME))
        {
            return Vec::new();
        }
        match newest_person_text(request.history) {
            Some(text) if says_something_lasting(&text) => {
                vec![make_meta_user_message(MEMORY_NUDGE)]
            }
            _ => Vec::new(),
        }
    }
}

/// The seat entry.
pub struct MemoryNudgeProducer;

impl SeatAttachmentProducer for MemoryNudgeProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        Some(Arc::new(MemoryNudgePoller {
            toolkit: binding.toolkit.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tools(bool);

    impl TurnToolkit for Tools {
        fn has_tool(&self, name: &str) -> bool {
            self.0 && name == crate::SAVE_MEMORY_TOOL_NAME
        }
    }

    fn poll(save_memory: bool, history: &[ApiMessage], iteration: u64) -> Vec<ApiMessage> {
        let poller = MemoryNudgePoller {
            toolkit: Some(Arc::new(Tools(save_memory))),
        };
        poller.poll(
            AttachmentPollRequest::new("s", "t", iteration, AttachmentPollPhase::Eager)
                .with_history(history),
        )
    }

    #[test]
    fn a_lasting_statement_gets_one_reminder_before_the_first_request() {
        for said in [
            "From now on keep answers short. Now: what is git stash?",
            "以后回答我都用中文。现在只需回复：好的。",
            "In this repo migrations live in db/migrations.",
            "我是做嵌入式的，平时主要写 C。",
            "FYI the staging server is staging.internal",
        ] {
            let history = vec![ApiMessage::user_text(said)];
            let nudged = poll(true, &history, 0);
            assert_eq!(nudged.len(), 1, "{said}");
            assert!(nudged[0].content[0]
                .as_text()
                .unwrap()
                .contains("SaveMemory now"));
            assert!(
                poll(true, &history, 1).is_empty(),
                "only before the first request"
            );
            assert!(
                poll(false, &history, 0).is_empty(),
                "only where SaveMemory is offered"
            );
        }
    }

    #[test]
    fn plain_requests_and_deliveries_get_none() {
        for said in [
            "What is 17 * 23?",
            "Change the println in src/main.rs to print hi.",
            "给 main.rs 加一行注释",
        ] {
            assert!(
                poll(true, &[ApiMessage::user_text(said)], 0).is_empty(),
                "{said}"
            );
        }
        let digest = rebon_tool::cron::mark_scheduled_prompt("- worker always failing");
        assert!(poll(true, &[ApiMessage::user_text(digest)], 0).is_empty());
        let reminder = "<system-reminder>\nfrom now on\n</system-reminder>";
        assert!(poll(true, &[ApiMessage::user_text(reminder)], 0).is_empty());
    }
}
