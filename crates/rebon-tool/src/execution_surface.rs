//! Whether anyone is there to answer.
//!
//! Rebon runs the same agent in rooms that differ in one way it had no word
//! for. In some there is a person: they approve a plan, answer a question,
//! retry a call the safety classifier stopped. In a benchmark container there
//! is nobody, ever, and each of those moves is a dead end the model cannot
//! see is a dead end.
//!
//! Rebon had no way to say which room it was in. In unattended trials the
//! failure was uniform: 27 of 27 runs walked into plan mode, took the
//! `yes_auto` option that the unattended approver picks for `ExitPlanMode`,
//! escalated themselves into auto mode, and then lost 22 tool calls to a
//! classifier whose fail-closed rule assumes a human can override it.
//! Thirteen more calls went to tools — `AskUserQuestion`,
//! `PlanLedger`, `run_code`, `Workflow` — that cannot succeed in that room at
//! all: 13 calls, 0 successes. The model was not confused; it was told these
//! doors existed.
//!
//! # Two questions, not one
//!
//! Writing this down made it clear the surface answers two *different*
//! questions, and a background job answers them differently:
//!
//! | | Is there someone who can answer? | Did a person make this authorization? |
//! |---|---|---|
//! | [`ExecutionSurface::Interactive`] | yes, now | yes |
//! | [`ExecutionSurface::Detached`] | yes, eventually | **only what was pre-authorized** |
//! | [`ExecutionSurface::Unattended`] | no | no |
//!
//! Tool exposure asks the first ([`ExecutionSurface::is_unattended`]): a
//! background job's `AskUserQuestion` really is answerable — over IPC, when
//! someone opens it — so hiding it would break a shipped path. Escalation
//! asks the second ([`ExecutionSurface::is_interactive`]): a mode that
//! arrives out of a tool result had nobody behind it in either non-interactive
//! room — except for what a person authorized ahead of time, which is what
//! [`set_escalation_preauthorized`] records. Collapsing the two into one flag is
//! what made "declare background jobs unattended" look right and be wrong.
//!
//! The mechanism mirrors [`crate::shell_preference`] — env override, then a
//! process global, then a default — because the reader is the same
//! (`is_enabled` / exposure, run for every tool on every snapshot) and it must
//! stay allocation-free.
//!
//! It is *not* the whole answer. What the system prompt should say when
//! nobody is listening is separate and not implemented here.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// Which room the agent is running in.
///
/// Ordered by how much of a person is available, and every gate reads it as
/// a ladder rather than a set: `Interactive` is strictly the most permissive,
/// `Unattended` strictly the least, and a new rung slots between them without
/// re-deciding the rungs that already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionSurface {
    /// A person is present and can approve, answer, and retry. The TUI, the
    /// desktop app, an ACP client with a UI in front of it.
    #[default]
    Interactive,
    /// Nobody is watching right now, but the room has a door. A background
    /// job: its permission prompts and `AskUserQuestion` calls queue up and
    /// are answered over IPC whenever someone opens the job
    /// (`rebon_session_runtime::host::ipc::{permissions, questions}`).
    /// Asking is slow here, not impossible — so the tools that ask stay.
    ///
    /// What is impossible is a person making a decision the agent needs
    /// *right now* to keep going, which is exactly what a self-issued
    /// permission escalation is.
    Detached,
    /// Nobody is present and no door. `rebon exec`, benchmark containers.
    /// Anything that needs a human answer is not a slower path here — it is
    /// a wall.
    Unattended,
}

impl ExecutionSurface {
    /// Wire value for the env override and for logs.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Detached => "detached",
            Self::Unattended => "unattended",
        }
    }

    /// Parse a user-typed or persisted value. `None` distinguishes "not set"
    /// from "set to something we do not understand".
    pub fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], "")
            .as_str()
        {
            "interactive" | "attended" | "tui" => Some(Self::Interactive),
            "detached" | "background" | "job" => Some(Self::Detached),
            "unattended" | "headless" | "exec" | "batch" => Some(Self::Unattended),
            _ => None,
        }
    }

    /// Whether a person is present *now* — the question tool exposure asks.
    ///
    /// False for [`Self::Detached`] too: a background job's question is
    /// answerable, so its tools stay, but nobody is standing there.
    pub const fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive)
    }

    /// Whether there is no way to reach a person at all — the question tool
    /// exposure asks before hiding a tool.
    ///
    /// Deliberately *not* true for [`Self::Detached`]: hiding
    /// `AskUserQuestion` from a background job would break a path that
    /// works (`rebon_session_runtime::host::ipc::questions`), just slowly.
    pub const fn is_unattended(self) -> bool {
        matches!(self, Self::Unattended)
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::Interactive => 0,
            Self::Detached => 1,
            Self::Unattended => 2,
        }
    }

    const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Detached,
            2 => Self::Unattended,
            _ => Self::Interactive,
        }
    }
}

impl std::fmt::Display for ExecutionSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

static EXECUTION_SURFACE: AtomicU8 = AtomicU8::new(0);

/// Env override, checked ahead of whatever the host declared.
///
/// The escape hatch for the case the host gets wrong: a `serve` session that
/// is in fact being watched, or an `exec` invocation a person is babysitting.
pub const EXECUTION_SURFACE_ENV_VAR: &str = "REBON_EXECUTION_SURFACE";

/// The surface in force for this process.
///
/// Read from tool exposure, which runs for every tool on every snapshot, so
/// the env lookup happens once — it takes a process-wide lock and allocates,
/// and it cannot change under a running process anyway.
pub fn execution_surface() -> ExecutionSurface {
    static ENV_OVERRIDE: OnceLock<Option<ExecutionSurface>> = OnceLock::new();
    let override_value = ENV_OVERRIDE.get_or_init(|| {
        std::env::var(EXECUTION_SURFACE_ENV_VAR)
            .ok()
            .as_deref()
            .and_then(ExecutionSurface::parse)
    });
    override_value
        .unwrap_or_else(|| ExecutionSurface::from_u8(EXECUTION_SURFACE.load(Ordering::Relaxed)))
}

/// Declare which room this process is in.
///
/// Called once at startup by whoever knows: `rebon exec` declares
/// [`ExecutionSurface::Unattended`], the interactive surfaces leave the
/// default. It is a process-wide statement rather than a per-session one on
/// purpose — a process that hosts one unattended turn hosts nothing else.
pub fn set_execution_surface(surface: ExecutionSurface) {
    EXECUTION_SURFACE.store(surface.as_u8(), Ordering::Relaxed);
}

/// Whether this process is running with nobody watching.
pub fn is_unattended() -> bool {
    execution_surface().is_unattended()
}

/// Bits for the two modes any escalation gate refuses. Set once at startup by
/// the host that can read the acceptance, read on every `ExitPlanMode` result.
static PREAUTHORIZED_ESCALATIONS: AtomicU8 = AtomicU8::new(0);

const ESCALATION_AUTO: u8 = 1 << 0;
const ESCALATION_BYPASS: u8 = 1 << 1;

fn escalation_bit(mode: &str) -> Option<u8> {
    match mode {
        "auto" => Some(ESCALATION_AUTO),
        "bypassPermissions" => Some(ESCALATION_BYPASS),
        _ => None,
    }
}

/// Declare that a person authorized `mode` for this process *before* the run
/// started.
///
/// The escalation gate asks whether a mode arriving out of a tool result had
/// anybody behind it, and in a [`ExecutionSurface::Detached`] room the answer is
/// not always no. A background job refuses to *launch* in `auto` or
/// `bypassPermissions` unless the user accepted that mode interactively
/// (`rebon_config::ensure_background_permission_mode_allowed`), and when the
/// desktop app hosts a chat as a background job, the same person is the one
/// reading the plan and picking "Yes, run with auto mode" over IPC. Dropping
/// that answer left the session in plan mode while `ExitPlanMode` reported
/// success — a loop the model cannot see or escape.
///
/// Declared by the host rather than read here for the same reason the surface
/// is: the run loop cannot depend on the config layer, and the reader runs on
/// every plan-mode tool result and must stay allocation-free. Modes that no gate
/// refuses are ignored — there is nothing to pre-authorize.
pub fn set_escalation_preauthorized(mode: &str) {
    let Some(bit) = escalation_bit(mode) else {
        return;
    };
    PREAUTHORIZED_ESCALATIONS.fetch_or(bit, Ordering::Relaxed);
}

/// Whether a person authorized `mode` for this process before the run started.
///
/// Only meaningful for a room where someone can still be reached — see
/// [`set_escalation_preauthorized`]. An [`ExecutionSurface::Unattended`] run has
/// nobody to have answered the dialog at all, so its gate ignores this.
pub fn escalation_preauthorized(mode: &str) -> bool {
    let Some(bit) = escalation_bit(mode) else {
        return false;
    };
    PREAUTHORIZED_ESCALATIONS.load(Ordering::Relaxed) & bit != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ExecutionSurface; 3] = [
        ExecutionSurface::Interactive,
        ExecutionSurface::Detached,
        ExecutionSurface::Unattended,
    ];

    #[test]
    fn wire_values_round_trip() {
        for surface in ALL {
            assert_eq!(ExecutionSurface::parse(surface.as_wire()), Some(surface));
            assert_eq!(ExecutionSurface::from_u8(surface.as_u8()), surface);
        }
    }

    #[test]
    fn the_spellings_a_host_is_likely_to_write_are_accepted() {
        assert_eq!(
            ExecutionSurface::parse("head-less"),
            Some(ExecutionSurface::Unattended)
        );
        assert_eq!(
            ExecutionSurface::parse("  EXEC  "),
            Some(ExecutionSurface::Unattended)
        );
        assert_eq!(
            ExecutionSurface::parse("Attended"),
            Some(ExecutionSurface::Interactive)
        );
        assert_eq!(
            ExecutionSurface::parse("BACKGROUND"),
            Some(ExecutionSurface::Detached)
        );
        assert_eq!(ExecutionSurface::parse("maybe"), None);
    }

    /// The two predicates are a ladder, and `Detached` is the rung that
    /// exists only because they are not the same question: a background
    /// job can be asked, and still never authorized anything.
    #[test]
    fn detached_can_be_asked_but_has_authorized_nothing() {
        assert!(!ExecutionSurface::Detached.is_interactive());
        assert!(!ExecutionSurface::Detached.is_unattended());

        assert!(ExecutionSurface::Interactive.is_interactive());
        assert!(!ExecutionSurface::Interactive.is_unattended());

        assert!(!ExecutionSurface::Unattended.is_interactive());
        assert!(ExecutionSurface::Unattended.is_unattended());
    }

    /// Monotone: nothing that is hidden from a detached room is shown in
    /// an unattended one, and nothing gated for interactive is ungated
    /// further down.
    #[test]
    fn the_rungs_only_ever_get_stricter() {
        let mut previously_interactive = true;
        let mut previously_reachable = true;
        for surface in ALL {
            let interactive = surface.is_interactive();
            let reachable = !surface.is_unattended();
            assert!(previously_interactive || !interactive, "{surface}");
            assert!(previously_reachable || !reachable, "{surface}");
            previously_interactive = interactive;
            previously_reachable = reachable;
        }
    }

    /// Interactive is the default because getting it wrong that way only
    /// shows a tool that is hard to use; the other way hides tools from a
    /// person who could have used them.
    #[test]
    fn the_default_is_interactive() {
        assert_eq!(ExecutionSurface::default(), ExecutionSurface::Interactive);
        assert!(!ExecutionSurface::default().is_unattended());
    }

    /// The pre-authorization is a process global, so this is deliberately one
    /// test: it reads the "not declared" state before declaring anything, and
    /// nothing clears the bits afterwards.
    #[test]
    fn only_the_two_refusable_modes_can_be_preauthorized() {
        assert!(!escalation_preauthorized("auto"));
        assert!(!escalation_preauthorized("bypassPermissions"));

        // A mode no gate refuses has nothing to pre-authorize, and an unknown
        // spelling must not silently set some other mode's bit.
        for ignored in ["acceptEdits", "default", "plan", "AUTO", ""] {
            set_escalation_preauthorized(ignored);
            assert!(!escalation_preauthorized(ignored), "{ignored}");
        }
        assert!(!escalation_preauthorized("auto"));

        set_escalation_preauthorized("auto");
        assert!(escalation_preauthorized("auto"));
        assert!(
            !escalation_preauthorized("bypassPermissions"),
            "one mode's acceptance is not another's"
        );

        set_escalation_preauthorized("bypassPermissions");
        assert!(escalation_preauthorized("bypassPermissions"));
        assert!(escalation_preauthorized("auto"), "declaring is additive");
    }
}
