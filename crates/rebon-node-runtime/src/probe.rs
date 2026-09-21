//! Asking a candidate executable what it is.
//!
//! The probe runs `<candidate> -p process.versions.node`, the same expression
//! the release workflow's Node gate uses. Reading `--version` would be cheaper,
//! but a runtime that cannot evaluate an expression cannot host a plugin either,
//! so the gate that admits it should be the one that proves it runs.
//!
//! Probing is a trait because resolution order is the interesting behaviour and
//! it must be testable without six real Node installations on disk.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

use thiserror::Error;

use crate::version::{NodeVersion, VersionError};

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("cannot run {path}: {message}")]
    Spawn { path: PathBuf, message: String },
    #[error("{path} exited with {status} while reporting its version: {stderr}")]
    Failed {
        path: PathBuf,
        status: String,
        stderr: String,
    },
    #[error("{path} did not report a Node version: {source}")]
    Unrecognised {
        path: PathBuf,
        #[source]
        source: VersionError,
    },
}

impl ProbeError {
    pub fn path(&self) -> &Path {
        match self {
            Self::Spawn { path, .. }
            | Self::Failed { path, .. }
            | Self::Unrecognised { path, .. } => path,
        }
    }
}

pub trait NodeProbe: Send + Sync {
    fn probe(&self, executable: &Path) -> Result<NodeVersion, ProbeError>;
}

/// The production probe: spawns the candidate.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecutingProbe;

impl NodeProbe for ExecutingProbe {
    fn probe(&self, executable: &Path) -> Result<NodeVersion, ProbeError> {
        let mut command = Command::new(executable);
        command.arg("-p").arg("process.versions.node");
        // A probe on the desktop app's startup path must not flash a console.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command.output().map_err(|error| ProbeError::Spawn {
            path: executable.to_path_buf(),
            message: error.to_string(),
        })?;
        if !output.status.success() {
            return Err(ProbeError::Failed {
                path: executable.to_path_buf(),
                status: output.status.to_string(),
                stderr: truncate(&String::from_utf8_lossy(&output.stderr)),
            });
        }
        let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
        NodeVersion::parse(&reported).map_err(|source| ProbeError::Unrecognised {
            path: executable.to_path_buf(),
            source,
        })
    }
}

/// Keeps a misbehaving candidate's output out of an error message that ends up
/// on a status line.
fn truncate(text: &str) -> String {
    const LIMIT: usize = 200;
    let trimmed = text.trim();
    let mut out = String::with_capacity(LIMIT);
    for character in trimmed.chars() {
        if out.chars().count() >= LIMIT {
            out.push('…');
            break;
        }
        out.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    out
}

/// A candidate a resolver looked at, and what came back. Kept so an
/// "unavailable" error can say *which* runtimes were rejected and why, instead
/// of only that nothing was found.
#[derive(Debug)]
pub struct RejectedCandidate {
    pub executable: PathBuf,
    pub reason: RejectionReason,
}

#[derive(Debug)]
pub enum RejectionReason {
    /// It ran and reported a version outside the supported range.
    Unsupported(NodeVersion),
    /// It could not be run, or did not answer with a version.
    Unusable(ProbeError),
}

impl fmt::Display for RejectedCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            RejectionReason::Unsupported(version) => {
                write!(f, "{} is Node {version}", self.executable.display())
            }
            RejectionReason::Unusable(error) => {
                write!(f, "{} is unusable ({error})", self.executable.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_and_length_are_bounded() {
        let noisy = format!("a\nb\tc{}", "x".repeat(500));
        let cleaned = truncate(&noisy);
        assert!(cleaned.starts_with("a b c"));
        assert_eq!(cleaned.chars().count(), 201, "200 chars plus the ellipsis");
        assert!(cleaned.ends_with('…'));
        assert!(!cleaned.contains('\n'));
    }

    #[test]
    fn short_output_is_returned_whole() {
        assert_eq!(truncate("  boom  "), "boom");
    }

    #[test]
    fn a_missing_executable_is_a_spawn_error_naming_the_path() {
        let missing = Path::new("Z:/definitely/not/here/node");
        let error = ExecutingProbe.probe(missing).unwrap_err();
        assert!(matches!(error, ProbeError::Spawn { .. }));
        assert_eq!(error.path(), missing);
        assert!(error.to_string().contains("not/here/node"));
    }

    #[test]
    fn rejection_renders_the_version_it_found() {
        let rejected = RejectedCandidate {
            executable: PathBuf::from("/usr/bin/node"),
            reason: RejectionReason::Unsupported(NodeVersion::new(22, 21, 1)),
        };
        assert!(rejected.to_string().contains("is Node 22.21.1"));
    }

    #[test]
    fn unusable_rejection_carries_the_probe_error() {
        let rejected = RejectedCandidate {
            executable: PathBuf::from("/usr/bin/node"),
            reason: RejectionReason::Unusable(ProbeError::Failed {
                path: PathBuf::from("/usr/bin/node"),
                status: "exit status: 1".into(),
                stderr: "boom".into(),
            }),
        };
        let rendered = rejected.to_string();
        assert!(rendered.contains("is unusable"));
        assert!(rendered.contains("boom"));
    }
}
