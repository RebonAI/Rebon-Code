//! The exit action and its goodbye message, picked from a fixed list.
//!
//! ## Behaviour
//!
//! * The `GOODBYE_MESSAGES` constant pool.
//! * The [`pick_goodbye`] helper (with the `Goodbye!` fallback).
//!
//! ## Outbound seam
//!
//! Shutting down with code 0 and reason `prompt_input_exit` → modeled as the
//! [`ExitFlowAction::Exit`] variant. The consumer performs the actual
//! shutdown.

/// The fixed pool of goodbye messages, in declaration order.
pub const GOODBYE_MESSAGES: &[&str] = &["Goodbye!", "See ya!", "Bye!", "Catch you later!"];

/// The deterministic fallback when the random pick produces nothing
/// (`Goodbye!`).
pub const FALLBACK_GOODBYE: &str = "Goodbye!";

/// Pure helper that picks a goodbye message by index. The pick is
/// meant to be random — we expose the index so the consumer can
/// pre-randomize and the test matrix can pin every branch
/// deterministically.
pub fn pick_goodbye(index: usize) -> &'static str {
    if GOODBYE_MESSAGES.is_empty() {
        return FALLBACK_GOODBYE;
    }
    GOODBYE_MESSAGES[index % GOODBYE_MESSAGES.len()]
}

/// The action emitted when the exit flow runs to completion. The
/// caller resolves the optional `result_message` (if `None`, it
/// substitutes a goodbye), then performs the shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitFlowAction {
    /// Exit immediately with the given message and shutdown code.
    Exit {
        /// The user-facing goodbye message. May be one of
        /// [`GOODBYE_MESSAGES`] or a caller-supplied override.
        message: String,
        /// Always 0 for this flow (a clean shutdown).
        code: i32,
        /// Always `"prompt_input_exit"` for this flow.
        reason: &'static str,
    },
}

/// Resolve the final exit action given an optional caller-supplied
/// message and a goodbye index. If `result_message` is `None`, we
/// substitute `pick_goodbye(goodbye_index)`.
pub fn resolve_exit(result_message: Option<String>, goodbye_index: usize) -> ExitFlowAction {
    let message = result_message.unwrap_or_else(|| pick_goodbye(goodbye_index).to_string());
    ExitFlowAction::Exit {
        message,
        code: 0,
        reason: "prompt_input_exit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goodbye_pool_pinned() {
        assert_eq!(GOODBYE_MESSAGES.len(), 4);
        assert_eq!(GOODBYE_MESSAGES[0], "Goodbye!");
        assert_eq!(GOODBYE_MESSAGES[1], "See ya!");
        assert_eq!(GOODBYE_MESSAGES[2], "Bye!");
        assert_eq!(GOODBYE_MESSAGES[3], "Catch you later!");
    }

    #[test]
    fn pick_goodbye_wraps_around() {
        assert_eq!(pick_goodbye(0), "Goodbye!");
        assert_eq!(pick_goodbye(1), "See ya!");
        assert_eq!(pick_goodbye(2), "Bye!");
        assert_eq!(pick_goodbye(3), "Catch you later!");
        assert_eq!(pick_goodbye(4), "Goodbye!");
        assert_eq!(pick_goodbye(usize::MAX), GOODBYE_MESSAGES[usize::MAX % 4]);
    }

    #[test]
    fn resolve_exit_uses_caller_message() {
        let ExitFlowAction::Exit {
            message,
            code,
            reason,
        } = resolve_exit(Some("Bye!".into()), 0);
        assert_eq!(message, "Bye!");
        assert_eq!(code, 0);
        assert_eq!(reason, "prompt_input_exit");
    }

    #[test]
    fn resolve_exit_falls_back_to_random_goodbye() {
        let ExitFlowAction::Exit { message, .. } = resolve_exit(None, 1);
        assert_eq!(message, "See ya!");
    }

    #[test]
    fn resolve_exit_default_index_zero() {
        let ExitFlowAction::Exit { message, .. } = resolve_exit(None, 0);
        assert_eq!(message, "Goodbye!");
    }

    #[test]
    fn fallback_goodbye_constant() {
        assert_eq!(FALLBACK_GOODBYE, "Goodbye!");
    }
}
