//! Locating a helper binary that ships beside `rebon`.
//!
//! Several Rebon features are separate executables rather than subcommands:
//! the Boa status-line helper, the Windows sandbox helper, and the binary half
//! of a plugin. Every one of them is
//! published *next to* the main executable — an npm install puts them in the
//! platform package's `bin/`/`payload/`, the desktop bundle copies them beside
//! the sidecar, and a checkout has them in the same `target/<profile>/`.
//!
//! So the search is two product locations and nothing else:
//!
//! 1. the running executable's own directory;
//! 2. `../Resources/<name>` beside it, which is where a macOS app bundle puts
//!    auxiliary binaries (`Contents/MacOS/rebon` → `Contents/Resources/…`).
//!
//! `PATH` and Cargo target directories are deliberately **not** searched.
//! Accepting an unrelated binary that happens to carry the right file name
//! would weaken the product boundary; for the sandbox helper it would mean
//! accepting an arbitrary program's claim to be confining the user's commands.
//! Not finding the binary is reported, never silently replaced by something
//! else.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The on-disk file name for a sibling binary named `name`.
///
/// `name` is the bare product name (`rebon-lsp-mcp`); the platform's
/// executable suffix is added here so callers never spell `.exe` themselves.
pub fn file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The product locations for `name`, given the running `executable`.
///
/// Order is significant: the executable's own directory comes first, so an
/// install always finds the helper that shipped with it rather than one from a
/// different contract version somewhere else on the machine.
pub fn candidates(name: &str, executable: &Path) -> Vec<PathBuf> {
    candidates_for_file_name(&file_name(name), executable)
}

/// The product locations for an exact file name, given the running `executable`.
///
/// The same rule as [`candidates`], for the one helper whose file name is a
/// Windows-only literal rather than a per-platform product name: the sandbox
/// helper is `sandbox-win.exe` in source that compiles everywhere, so deriving
/// its suffix from the *build* target would rename it on a Linux build.
pub fn candidates_for_file_name(file: &str, executable: &Path) -> Vec<PathBuf> {
    // An empty parent is not a directory to search. `Path::parent` returns
    // `Some("")` for a bare file name, and joining onto it yields a bare
    // `rebon-lsp-mcp.exe` — which `CreateProcess` would then resolve off PATH,
    // the one place this function exists to never look.
    let Some(directory) = executable
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
    else {
        return Vec::new();
    };
    let mut candidates = vec![directory.join(file)];
    if let Some(contents) = directory.parent() {
        let resources = contents.join("Resources").join(file);
        if !candidates.contains(&resources) {
            candidates.push(resources);
        }
    }
    candidates
}

/// The first existing candidate for `name` beside `executable`.
pub fn resolve_from_executable(
    name: &str,
    executable: &Path,
) -> Result<PathBuf, SiblingBinaryMissing> {
    let searched = candidates(name, executable);
    searched
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .ok_or_else(|| SiblingBinaryMissing::NotFound {
            name: name.to_string(),
            searched,
        })
}

/// The first existing candidate for `name` beside the running executable.
pub fn resolve(name: &str) -> Result<PathBuf, SiblingBinaryMissing> {
    // A start-up path that cannot name its own executable reports that rather
    // than guessing: falling back to a bare name would hand the decision to
    // PATH, which is what this module exists to avoid.
    let executable = std::env::current_exe().map_err(SiblingBinaryMissing::CurrentExecutable)?;
    resolve_from_executable(name, &executable)
}

/// Why a sibling binary could not be located.
#[derive(Debug)]
pub enum SiblingBinaryMissing {
    /// The running executable's own path could not be read.
    CurrentExecutable(io::Error),
    /// None of the product locations held the binary.
    NotFound {
        name: String,
        searched: Vec<PathBuf>,
    },
}

impl fmt::Display for SiblingBinaryMissing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentExecutable(err) => {
                write!(f, "failed to locate the running executable: {err}")
            }
            Self::NotFound { name, searched } => {
                write!(f, "`{name}` was not found beside the running executable")?;
                if searched.is_empty() {
                    write!(f, " (no directory to search)")?;
                } else {
                    write!(f, " (searched")?;
                    for (index, path) in searched.iter().enumerate() {
                        let separator = if index == 0 { " " } else { ", " };
                        write!(f, "{separator}{}", path.display())?;
                    }
                    write!(f, ")")?;
                }
                write!(
                    f,
                    ". It ships beside the `rebon` executable: reinstall Rebon, \
                     or in a checkout run `cargo build` so it lands in the same \
                     target directory."
                )
            }
        }
    }
}

impl std::error::Error for SiblingBinaryMissing {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CurrentExecutable(err) => Some(err),
            Self::NotFound { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn the_executables_own_directory_is_probed_first() {
        let executable = native(&["npm", "payload", "rebon.exe"]);
        let candidates = candidates("rebon-lsp-mcp", &executable);
        assert_eq!(
            candidates[0],
            native(&["npm", "payload", &file_name("rebon-lsp-mcp")])
        );
    }

    #[test]
    fn a_mac_bundles_resources_directory_is_probed_second() {
        let executable = native(&["Rebon.app", "Contents", "MacOS", "rebon"]);
        let candidates = candidates("rebon-browser-mcp", &executable);
        assert_eq!(
            candidates[1],
            native(&[
                "Rebon.app",
                "Contents",
                "Resources",
                &file_name("rebon-browser-mcp"),
            ])
        );
    }

    #[test]
    fn path_is_deliberately_not_searched() {
        // An executable path with no directory in it at all is the case that
        // would otherwise produce a bare file name, which `CreateProcess` and
        // `execvp` resolve off PATH.
        let bare = candidates("rebon-computer-use", Path::new("rebon.exe"));
        assert!(bare.is_empty());
    }

    #[test]
    fn a_missing_binary_reports_both_places_it_looked() {
        let directory = tempfile::tempdir().expect("temp dir");
        let executable = directory.path().join("bundle").join("rebon");
        std::fs::create_dir_all(executable.parent().expect("parent")).expect("create bundle dir");
        let error = resolve_from_executable("rebon-lsp-mcp", &executable)
            .expect_err("nothing was installed beside the executable");
        let SiblingBinaryMissing::NotFound { searched, .. } = &error else {
            panic!("expected NotFound, got {error:?}");
        };
        assert_eq!(searched.len(), 2);
        let rendered = error.to_string();
        for candidate in searched {
            assert!(
                rendered.contains(&candidate.display().to_string()),
                "{rendered} does not name {}",
                candidate.display()
            );
        }
    }

    #[test]
    fn a_binary_beside_the_executable_is_found() {
        let directory = tempfile::tempdir().expect("temp dir");
        let executable = directory.path().join(file_name("rebon"));
        let sibling = directory.path().join(file_name("rebon-lsp-mcp"));
        std::fs::write(&sibling, b"binary").expect("write sibling");
        assert_eq!(
            resolve_from_executable("rebon-lsp-mcp", &executable).expect("found beside"),
            sibling
        );
    }

    #[test]
    fn a_binary_under_resources_is_found_when_the_directory_has_none() {
        let directory = tempfile::tempdir().expect("temp dir");
        let contents = directory.path().join("Contents");
        let executable = contents.join("MacOS").join(file_name("rebon"));
        std::fs::create_dir_all(executable.parent().expect("parent")).expect("create MacOS");
        std::fs::create_dir_all(contents.join("Resources")).expect("create Resources");
        let sibling = contents.join("Resources").join(file_name("rebon-lsp-mcp"));
        std::fs::write(&sibling, b"binary").expect("write sibling");
        assert_eq!(
            resolve_from_executable("rebon-lsp-mcp", &executable).expect("found under Resources"),
            sibling
        );
    }
}
