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
