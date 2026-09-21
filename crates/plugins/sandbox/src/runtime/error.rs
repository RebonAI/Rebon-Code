//! Sandbox errors — the RFC §13 "errors and degradation" table.
//!
//! The single rule this module exists to enforce: **a sandbox that
//! cannot be built is an error, never a quiet passthrough.** Every
//! variant below is a case where the RFC says the command must not
//! run. Cases where the RFC says "skip this rule and log" are not
//! errors — they are [`crate::runtime::Warning`]s carried alongside a
//! successfully built command.

use std::fmt;
use std::path::PathBuf;

/// Why a sandbox wrap failed.
///
/// `Display` is written for a tool result the model reads back, so
/// each message says what was refused and what would make it work.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// A native dependency the chosen backend needs is not on this
    /// machine (`bwrap`, `socat`, `sandbox-win.exe`, `sandbox-exec`).
    ///
    /// RFC §13: dependency missing → throw, do not degrade.
    #[error("sandbox dependency `{dependency}` is unavailable: {detail}")]
    MissingDependency {
        dependency: &'static str,
        detail: String,
    },

    /// `initialize()` never ran, or ran and failed. RFC §12: a failed
    /// initialize puts the runtime in an error state and every later
    /// wrap is refused rather than run unsandboxed.
    #[error("sandbox runtime is not initialized: {detail}")]
    NotInitialized { detail: String },

    /// The platform has no backend at all (RFC §11: an unsupported
    /// platform must never be told the sandbox is active).
    #[error("sandbox is not supported on this platform ({platform})")]
    UnsupportedPlatform { platform: &'static str },

    /// Windows-only. Per-exec `allowRead` / `allowWrite` cannot be
    /// honoured because ACLs are a session-level resource — RFC §6.4.
    /// Callers must `reset()` + `initialize()` with the new set.
    #[error(
        "per-command allowRead/allowWrite is not supported on Windows — ACLs are \
         session-scoped; reset() and initialize() with the new path set instead \
         (offending path: {path})"
    )]
    PerExecAclUnsupported { path: PathBuf },

    /// Windows-only. `CreateProcessW` caps the command line at 32767
    /// UTF-16 units; RFC §6.2 refuses at 30000 to leave headroom for
    /// the quoting the OS adds back.
    #[error(
        "sandboxed command line is {length} characters, over the {limit} limit — \
         write the script to a file and run the file instead"
    )]
    ArgvTooLong { length: usize, limit: usize },

    /// RFC §9: strict mode probed for real confinement and did not
    /// find it. The command is not executed.
    #[error("{SANDBOX_NOT_CONFINED_MESSAGE} ({reason})")]
    NotConfined { reason: String },

    /// The shell the sandbox is supposed to launch does not exist.
    /// RFC §13's `sandboxed bash on Windows requires Git Bash` row
    /// generalised: without a shell there is nothing to confine.
    #[error("{detail}")]
    ShellUnavailable { detail: String },

    /// The loopback proxy bridge (Linux `socat`) could not be brought
    /// up. RFC §4.3: kill the children and throw.
    #[error("failed to start the sandbox network bridge: {detail}")]
    BridgeStartFailed { detail: String },

    /// A configuration value cannot be compiled into platform
    /// primitives at all (as opposed to one rule being skipped).
    #[error("invalid sandbox configuration: {detail}")]
    InvalidConfig { detail: String },
}

/// The user-facing sentence for a failed confinement probe.
///
/// Pinned as a constant because the ACP client, the TUI, and the tool
/// result all show it and a drifting string would look like three
/// different failures.
pub const SANDBOX_NOT_CONFINED_MESSAGE: &str =
    "This command must run inside a fully isolated sandbox, but the current \
     environment cannot provide one — the command was not executed";

impl SandboxError {
    /// Whether this error means "the machine is missing something",
    /// which the doctor surface renders with an install hint, as
    /// opposed to "this command is wrong", which it does not.
    pub fn is_environmental(&self) -> bool {
        matches!(
            self,
            SandboxError::MissingDependency { .. }
                | SandboxError::UnsupportedPlatform { .. }
                | SandboxError::ShellUnavailable { .. }
                | SandboxError::BridgeStartFailed { .. }
                | SandboxError::NotConfined { .. }
        )
    }
}

/// A rule that was dropped while the sandbox was still built.
///
/// RFC §11.3 requires every skipped or degraded rule to be
/// auditable, so these are values rather than bare `tracing::warn!`
/// calls: the caller can surface them in `/sandbox`, and tests can
/// assert on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Backend that produced it — `"linux"`, `"macos"`, `"windows"`.
    pub backend: &'static str,
    /// Machine-readable reason, e.g. `"glob_write_pattern"`.
    pub code: &'static str,
    /// Human sentence, already including the offending path.
    pub detail: String,
}

impl Warning {
    pub fn new(backend: &'static str, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            backend,
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[Sandbox {}] {}", self.backend, self.detail)
    }
}

/// Warning codes, pinned so the consumer can branch on them without
/// matching on prose.
pub mod warning_code {
    /// A write rule used a glob; Linux needs concrete mount targets.
    pub const GLOB_WRITE_PATTERN: &str = "glob_write_pattern";
    /// A path in the plan does not exist, so there is nothing to bind.
    pub const PATH_MISSING: &str = "path_missing";
    /// `realpath` moved the path elsewhere — a symlink escape attempt.
    pub const SYMLINK_ESCAPE: &str = "symlink_escape";
    /// An ancestor component vanished or turned into a symlink.
    pub const ANCESTOR_UNSTABLE: &str = "ancestor_unstable";
    /// macOS cannot fake file contents; the rule became a plain deny.
    pub const MASK_DOWNGRADED_TO_DENY: &str = "mask_downgraded_to_deny";
    /// A `/dev` path was refused as a bind target.
    pub const DEV_PATH_REFUSED: &str = "dev_path_refused";
    /// Domain rules were configured but no loopback proxy is running,
    /// so the network was cut entirely instead of filtered.
    pub const DOMAIN_RULES_WITHOUT_PROXY: &str = "domain_rules_without_proxy";
    /// A seccomp filter is configured but the wrap seam cannot apply
    /// it, so syscall filtering is off.
    pub const SECCOMP_NOT_APPLIED: &str = "seccomp_not_applied";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_too_long_names_both_numbers() {
        let err = SandboxError::ArgvTooLong {
            length: 30_001,
            limit: 30_000,
        };
        let text = err.to_string();
        assert!(text.contains("30001"), "{text}");
        assert!(text.contains("30000"), "{text}");
    }

    #[test]
    fn not_confined_embeds_the_pinned_sentence() {
        let err = SandboxError::NotConfined {
            reason: "bwrap exited 1".into(),
        };
        assert!(err.to_string().starts_with(SANDBOX_NOT_CONFINED_MESSAGE));
        assert!(err.to_string().contains("bwrap exited 1"));
    }

    #[test]
    fn per_exec_acl_error_points_at_the_reset_path() {
        let err = SandboxError::PerExecAclUnsupported {
            path: PathBuf::from("C:/tmp/x"),
        };
        assert!(err.to_string().contains("reset()"));
    }

    #[test]
    fn environmental_classification_splits_machine_from_command_faults() {
        assert!(SandboxError::MissingDependency {
            dependency: "bwrap",
            detail: "not on PATH".into(),
        }
        .is_environmental());
        assert!(SandboxError::NotConfined { reason: "x".into() }.is_environmental());
        assert!(!SandboxError::ArgvTooLong {
            length: 1,
            limit: 0
        }
        .is_environmental());
        assert!(!SandboxError::PerExecAclUnsupported {
            path: PathBuf::new(),
        }
        .is_environmental());
        assert!(!SandboxError::InvalidConfig { detail: "x".into() }.is_environmental());
    }

    #[test]
    fn warning_display_carries_the_auditable_prefix() {
        let warning = Warning::new("linux", warning_code::GLOB_WRITE_PATTERN, "skipping /a/*");
        assert_eq!(warning.to_string(), "[Sandbox linux] skipping /a/*");
    }
}
