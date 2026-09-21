//! The three values every other module of the `/profile` surface hands back:
//! what a command has to say for itself, what it could not finish, and the one
//! step no shared layer can perform.
//!
//! They live in their own module rather than beside any one of the surfaces
//! that produce them, because [`command`](crate::command),
//! [`field`](crate::field), [`apply`](crate::apply) and
//! [`proposal`](crate::proposal) all return them and none of them owns the
//! shape.

/// What a `/profile` command has to say for itself.
///
/// `is_err` is the front end's cue for how to show it, not a claim that
/// nothing happened — a partial apply is an error that already changed things,
/// and says so in its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileCommandResult {
    pub text: String,
    pub is_err: bool,
}

impl ProfileCommandResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_err: false,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_err: true,
        }
    }
}

/// A provider/model move that has landed on disk and still has to reach the
/// session's model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRefresh {
    pub provider_name: String,
    pub model_name: String,
}

/// What [`crate::apply_to_session`] could not finish on its own.
#[derive(Debug, Clone)]
pub struct ProfileApplyOutcome {
    pub report: ProfileCommandResult,
    /// Set when provider/model moved on disk and the session's runtime still
    /// has to be re-resolved.
    ///
    /// That step is the one part of applying a profile that needs a live
    /// runtime, and the permission-approval path does not have one — it runs
    /// mid-callback. So the work is handed back rather than done here, and the
    /// caller performs it where it can: `/profile use` immediately, an
    /// approved `ProfileSwitch` on the next turn of the event loop.
    pub runtime_refresh: Option<RuntimeRefresh>,
}

impl ProfileApplyOutcome {
    /// A report that reached no step at all.
    pub fn nothing_applied(report: ProfileCommandResult) -> Self {
        Self {
            report,
            runtime_refresh: None,
        }
    }
}
