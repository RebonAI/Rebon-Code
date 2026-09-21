//! Strict mode and the confinement probe — RFC §9.
//!
//! The rule this module enforces is short and the reason it exists is
//! not obvious: **a sandbox that silently is not there is worse than
//! no sandbox at all.** Every other layer in Rebon is willing to
//! degrade — a missing dependency downgrades a feature, a failed
//! probe falls back. Here degrading means the user believes commands
//! are confined while they are running with the agent's full
//! authority, which is the exact situation the enterprise `required`
//! policy exists to rule out.
//!
//! So under [`SandboxMode::Strict`] the runtime does not trust its
//! own configuration. It asks the operating system to actually
//! confine a trivial process, and refuses to run the real one unless
//! that worked.

use crate::runtime::error::SandboxError;
use std::path::PathBuf;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::{Command, Stdio};

/// How hard the runtime insists on real confinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Confinement is verified before every command; a failed
    /// verification refuses the command. The default, and what an
    /// enterprise `sandbox: required` policy pins.
    #[default]
    Strict,
    /// The probe is skipped: a machine that *looks* able to confine is
    /// taken at its word, and a command that would have been refused by
    /// [`assert_confined`] runs instead. Reachable only when the user
    /// explicitly relaxes it, and never when policy has locked the
    /// setting.
    ///
    /// This relaxes the verification, not the sandbox. A machine that
    /// cannot build one at all — a missing `bwrap`, a `sandbox-win.exe`
    /// that is absent or not installed — still refuses the command here,
    /// because [`crate::runtime::SandboxRuntime::wrap`] has nothing to wrap it
    /// with. Relaxed is for "the probe cannot prove it", not for "there
    /// is no sandbox"; the latter would be the silent absence this
    /// module exists to rule out.
    Relaxed,
}

impl SandboxMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxMode::Strict => "strict",
            SandboxMode::Relaxed => "relaxed",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "strict" => Some(SandboxMode::Strict),
            "relaxed" => Some(SandboxMode::Relaxed),
            _ => None,
        }
    }
}

/// The answer to "is a command started right now actually confined?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinedVerdict {
    pub confined: bool,
    /// Why — shown to the user on refusal, so it names the missing
    /// piece rather than restating the refusal.
    pub reason: String,
}

impl ConfinedVerdict {
    pub fn confined(reason: impl Into<String>) -> Self {
        Self {
            confined: true,
            reason: reason.into(),
        }
    }

    pub fn unconfined(reason: impl Into<String>) -> Self {
        Self {
            confined: false,
            reason: reason.into(),
        }
    }
}

/// Something that can answer the confinement question.
pub trait ConfinedProbe: Send + Sync {
    fn probe(&self) -> ConfinedVerdict;
}

/// What [`assert_confined`] is being asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfinementContext {
    /// Whether this command was going to be sandboxed at all. A
    /// command that legitimately bypasses the sandbox (an excluded
    /// command, or one the user allowed to run unsandboxed) is not
    /// subject to the probe — nothing claimed it was confined.
    pub sandbox_requested: bool,
    pub mode: SandboxMode,
    /// A debugging session runs the agent under a debugger that
    /// itself needs the ptrace the sandbox drops. The exemption is
    /// narrow and deliberate: it is the one case where refusing would
    /// make the tool unusable for the person diagnosing the sandbox.
    pub debug_session: bool,
}

/// RFC §9.2 — refuse the command unless confinement is real.
///
/// Called twice per command, before and after the wrap, because the
/// two answer different questions: the first asks whether this
/// machine can confine anything at all, the second whether *this*
/// wrap produced something confined. A probe that only ran before
/// would pass on a machine where the wrap silently produced a
/// passthrough.
pub fn assert_confined(
    context: ConfinementContext,
    probe: &dyn ConfinedProbe,
) -> Result<(), SandboxError> {
    if !context.sandbox_requested || context.mode == SandboxMode::Relaxed || context.debug_session {
        return Ok(());
    }
    let verdict = probe.probe();
    if verdict.confined {
        return Ok(());
    }
    Err(SandboxError::NotConfined {
        reason: verdict.reason,
    })
}

/// The real probe: ask the platform to confine `true` and see.
///
/// The point is that this exercises the same binary and the same
/// kernel path the real wrap will use. Checking that `bwrap` exists
/// on disk would not catch the common failure — a container or a
/// hardened kernel where user namespaces are disabled, so `bwrap` is
/// present, executable, and fails at `unshare` every time.
pub struct PlatformProbe {
    pub bwrap_path: Option<PathBuf>,
    pub sandbox_win_ready: bool,
    pub sandbox_win_reason: String,
}

impl PlatformProbe {
    pub fn linux(bwrap_path: Option<PathBuf>) -> Self {
        Self {
            bwrap_path,
            sandbox_win_ready: false,
            sandbox_win_reason: String::new(),
        }
    }

    pub fn windows(ready: bool, reason: impl Into<String>) -> Self {
        Self {
            bwrap_path: None,
            sandbox_win_ready: ready,
            sandbox_win_reason: reason.into(),
        }
    }
}

impl ConfinedProbe for PlatformProbe {
    fn probe(&self) -> ConfinedVerdict {
        #[cfg(target_os = "linux")]
        {
            let Some(bwrap) = &self.bwrap_path else {
                return ConfinedVerdict::unconfined(
                    "bubblewrap is not installed, so no command can be confined",
                );
            };
            return match Command::new(bwrap)
                .args(["--unshare-all", "--ro-bind", "/", "/", "--", "/bin/true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
            {
                Ok(output) if output.status.success() => {
                    ConfinedVerdict::confined("bubblewrap confined a probe process")
                }
                Ok(output) => ConfinedVerdict::unconfined(format!(
                    "bubblewrap could not create a namespace: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(err) => {
                    ConfinedVerdict::unconfined(format!("bubblewrap could not be run: {err}"))
                }
            };
        }

        #[cfg(target_os = "macos")]
        {
            return match Command::new(crate::runtime::macos::SANDBOX_EXEC)
                .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
            {
                Ok(output) if output.status.success() => {
                    ConfinedVerdict::confined("sandbox-exec applied a probe profile")
                }
                Ok(output) => ConfinedVerdict::unconfined(format!(
                    "sandbox-exec refused a probe profile: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(err) => {
                    ConfinedVerdict::unconfined(format!("sandbox-exec could not be run: {err}"))
                }
            };
        }

        #[cfg(target_os = "windows")]
        {
            return if self.sandbox_win_ready {
                ConfinedVerdict::confined("the sandbox-win helper reports a ready sandbox")
            } else {
                ConfinedVerdict::unconfined(if self.sandbox_win_reason.is_empty() {
                    "the Windows sandbox helper is not installed".to_string()
                } else {
                    self.sandbox_win_reason.clone()
                })
            };
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = &self.sandbox_win_reason;
            ConfinedVerdict::unconfined("this platform has no sandbox backend")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(bool);

    impl ConfinedProbe for Fixed {
        fn probe(&self) -> ConfinedVerdict {
            if self.0 {
                ConfinedVerdict::confined("fixture")
            } else {
                ConfinedVerdict::unconfined("fixture says no")
            }
        }
    }

    struct Counting(std::sync::atomic::AtomicUsize);

    impl ConfinedProbe for Counting {
        fn probe(&self) -> ConfinedVerdict {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ConfinedVerdict::confined("counted")
        }
    }

    fn context(
        sandbox_requested: bool,
        mode: SandboxMode,
        debug_session: bool,
    ) -> ConfinementContext {
        ConfinementContext {
            sandbox_requested,
            mode,
            debug_session,
        }
    }

    #[test]
    fn strict_mode_refuses_when_the_probe_fails() {
        let error =
            assert_confined(context(true, SandboxMode::Strict, false), &Fixed(false)).unwrap_err();
        match error {
            SandboxError::NotConfined { reason } => assert_eq!(reason, "fixture says no"),
            other => panic!("expected NotConfined, got {other:?}"),
        }
    }

    #[test]
    fn strict_mode_passes_when_the_probe_succeeds() {
        assert!(assert_confined(context(true, SandboxMode::Strict, false), &Fixed(true)).is_ok());
    }

    #[test]
    fn relaxed_mode_never_refuses() {
        assert!(assert_confined(context(true, SandboxMode::Relaxed, false), &Fixed(false)).is_ok());
    }

    #[test]
    fn a_command_that_was_not_going_to_be_sandboxed_is_not_probed() {
        let probe = Counting(std::sync::atomic::AtomicUsize::new(0));
        assert!(assert_confined(context(false, SandboxMode::Strict, false), &probe).is_ok());
        assert_eq!(probe.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_debug_session_is_exempt() {
        assert!(assert_confined(context(true, SandboxMode::Strict, true), &Fixed(false)).is_ok());
    }

    #[test]
    fn mode_round_trips_through_the_wire_strings() {
        for mode in [SandboxMode::Strict, SandboxMode::Relaxed] {
            assert_eq!(SandboxMode::from_wire(mode.as_str()), Some(mode));
        }
        assert_eq!(SandboxMode::from_wire("open"), None);
    }

    #[test]
    fn strict_is_the_default_mode() {
        assert_eq!(SandboxMode::default(), SandboxMode::Strict);
    }

    #[test]
    fn windows_probe_reports_the_helpers_own_reason() {
        let probe = PlatformProbe::windows(false, "WFP filters are not installed");
        let verdict = probe.probe();
        if cfg!(target_os = "windows") {
            assert!(!verdict.confined);
            assert_eq!(verdict.reason, "WFP filters are not installed");
        }
    }

    #[test]
    fn linux_probe_without_bwrap_is_unconfined() {
        let probe = PlatformProbe::linux(None);
        let verdict = probe.probe();
        if cfg!(target_os = "linux") {
            assert!(!verdict.confined);
            assert!(verdict.reason.contains("bubblewrap"));
        }
    }
}
