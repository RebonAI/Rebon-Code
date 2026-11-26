//! Memory a session keeps without being asked to, as a seam.
//!
//! A model saves memory when it thinks to call `SaveMemory`, and in practice
//! that is rarely: the preference the person stated twice, the correction
//! they made, the decision the project took are said once in passing and
//! then lost to the next compaction. So the engine asks, in the background
//! at the start of each turn, what in the conversation since it last asked
//! is worth keeping — and the memory store decides how to ask and how to
//! keep the answer.
//!
//! Two parties, neither of which may depend on the other: the engine holds
//! the transcript, the session's model client and the turn's timing; the
//! `memory` plugin owns the store, its index and its rules for what belongs
//! in it. The shape of the exchange lives here, below both, as
//! [`crate::agent_documents`] does for per-agent memory.
//!
//! With no provider nothing is captured, which is what a session with the
//! memory plugin turned off should get.
//!
//! An agent group's memory is kept the same way, on a seat of its own
//! ([`GroupCaptureService`]), which the `groups` plugin fills: at the end of
//! a turn of a session in a group, what the turn found out that the other
//! members should know. The person may have said nothing — a worker the
//! router sent is given a task and reports back — so it reads the agent's
//! work, not only the person's words.

use std::path::Path;

use rebon_kernel::Service;

/// JSON/typed name of the memory-capture seam.
pub const MEMORY_CAPTURE_SERVICE: &str = "memory-capture";

/// What to ask a model about one stretch of conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureAsk {
    /// The instructions: what is worth keeping and the answer's shape.
    pub system: String,
    /// The memory as it stands and the conversation to read.
    pub prompt: String,
    /// The answer's budget.
    pub max_tokens: u32,
}

/// The provider behind the seam.
pub trait MemoryCapture: Send + Sync {
    /// The question for `conversation` — text in which `Person:` and
    /// `Assistant:` lines alternate — in the project at `cwd`, or `None`
    /// when this session keeps no memory (turned off, or nothing to ask).
    fn ask(&self, cwd: &Path, conversation: &str) -> Option<CaptureAsk>;

    /// Keep what the model answered to [`Self::ask`]. Returns the title of
    /// each memory written or updated, for the log; an answer that is not
    /// in the asked-for shape keeps nothing.
    fn keep(&self, cwd: &Path, answer: &str) -> Vec<String>;
}

/// Typed definition for the kernel's `memory-capture` seat.
pub struct MemoryCaptureService;

impl Service for MemoryCaptureService {
    type Interface = dyn MemoryCapture;
    const NAME: &'static str = MEMORY_CAPTURE_SERVICE;
}

/// JSON/typed name of the group-capture seam.
pub const GROUP_CAPTURE_SERVICE: &str = "group-capture";

/// The provider behind the group seam.
pub trait GroupCapture: Send + Sync {
    /// Whether the session `session_id` is in a group at all: asked first,
    /// so a session in none costs no transcript read.
    fn in_group(&self, session_id: &str) -> bool;

    /// The question for `conversation` — `Task:`, `Person:` and `Assistant:`
    /// lines — of the session `session_id`, or `None` when the session is in
    /// no group.
    fn ask(&self, session_id: &str, conversation: &str) -> Option<CaptureAsk>;

    /// Keep what the model answered to [`Self::ask`] in the session's group.
    /// Returns each fact kept, for the log.
    fn keep(&self, session_id: &str, answer: &str) -> Vec<String>;
}

/// Typed definition for the kernel's `group-capture` seat.
pub struct GroupCaptureService;

impl Service for GroupCaptureService {
    type Interface = dyn GroupCapture;
    const NAME: &'static str = GROUP_CAPTURE_SERVICE;
}

/// The shapes a key or a private key takes. The instructions already say not
/// to keep secrets; this is for the answer that keeps one anyway.
pub fn looks_like_a_secret(text: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "-----BEGIN",
        "sk-ant-",
        "ghp_",
        "github_pat_",
        "AKIA",
        "xoxb-",
    ];
    if MARKERS.iter().any(|marker| text.contains(marker)) {
        return true;
    }
    // `sk-` followed by a long run of key characters (OpenAI-style keys).
    text.match_indices("sk-").any(|(at, _)| {
        text[at + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .count()
            >= 20
    })
}

/// The first balanced `{…}` in `text`: models wrap JSON in fences or prose
/// now and then.
pub fn first_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;
    use std::sync::Arc;

    struct Echo;

    impl MemoryCapture for Echo {
        fn ask(&self, _cwd: &Path, conversation: &str) -> Option<CaptureAsk> {
            Some(CaptureAsk {
                system: "keep".into(),
                prompt: conversation.into(),
                max_tokens: 10,
            })
        }

        fn keep(&self, _cwd: &Path, answer: &str) -> Vec<String> {
            vec![answer.to_string()]
        }
    }

    #[test]
    fn the_seat_is_empty_until_a_provider_fills_it() {
        let kernel = Kernel::new();
        assert!(kernel.context().get::<MemoryCaptureService>().is_none());
        let ctx = kernel.context().fork("memory");
        ctx.provide::<MemoryCaptureService>(Arc::new(Echo)).unwrap();
        let capture = kernel.context().get::<MemoryCaptureService>().unwrap();
        let ask = capture.ask(Path::new("/w"), "Person: hi").unwrap();
        assert_eq!(ask.prompt, "Person: hi");
        assert_eq!(capture.keep(Path::new("/w"), "x"), ["x"]);
        ctx.dispose();
        assert!(kernel.context().get::<MemoryCaptureService>().is_none());
    }
}
