//! Resolve a program name against `PATH` without spawning `where.exe`.
//!
//! The shell probes (`is Git Bash here?`, `is there a PowerShell?`) used to
//! shell out to `where.exe`, which walks `PATH` in a child process: 100–300
//! ms per call on a loaded machine, paid on the first tool snapshot of every
//! process — every session build, every worker — before a single frame or
//! prompt. Walking `PATH` here costs about a millisecond and answers the
//! same question.

use std::path::{Path, PathBuf};

/// The extensions tried for a bare name, in the order `where.exe` tries
/// them for the default `PATHEXT`.
const EXECUTABLE_EXTENSIONS: &[&str] = &[".exe", ".cmd", ".bat", ".com"];

/// Every match of `name` on `PATH`, in `PATH` order — what `where.exe name`
/// prints, one path per line.
pub(crate) fn executables_on_path(name: &str) -> Vec<PathBuf> {
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    executables_in_dirs(name, std::env::split_paths(&path_var))
}

/// [`executables_on_path`] over an explicit directory list.
pub(crate) fn executables_in_dirs(
    name: &str,
    dirs: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in candidates_in(&dir, name) {
            if is_present_file(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

fn candidates_in(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(EXECUTABLE_EXTENSIONS.len() + 1);
    // `git.exe` is looked up as given first; `git` only with an extension
    // appended. A name with a dot in it that is not an extension
    // (`python3.12`) still gets the extensions tried after the exact name.
    if Path::new(name).extension().is_some() {
        candidates.push(dir.join(name));
    }
    for extension in EXECUTABLE_EXTENSIONS {
        candidates.push(dir.join(format!("{name}{extension}")));
    }
    candidates
}

/// `symlink_metadata` on purpose: a Store app's execution alias (the
/// `pwsh.exe` under `WindowsApps`) is a reparse point that `metadata`
/// refuses to follow, yet `CreateProcess` runs it — `where.exe` lists it,
/// and so does this.
fn is_present_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::write(path, b"").unwrap();
    }

    /// The lookup answers like `where.exe`: every directory on the list in
    /// order, the extensions in `PATHEXT` order within a directory, and a
    /// name given with its extension found as written.
    #[test]
    fn finds_executables_in_path_order_with_where_exe_extension_order() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        touch(&first.join("git.cmd"));
        touch(&first.join("git.exe"));
        touch(&second.join("git.exe"));
        touch(&second.join("unrelated.exe"));

        let found = executables_in_dirs("git", vec![first.clone(), PathBuf::new(), second.clone()]);
        assert_eq!(
            found,
            vec![
                first.join("git.exe"),
                first.join("git.cmd"),
                second.join("git.exe")
            ]
        );

        assert_eq!(
            executables_in_dirs("git.cmd", vec![first.clone()]),
            vec![first.join("git.cmd")]
        );
        assert!(executables_in_dirs("bash", vec![first, second]).is_empty());
    }

    /// A directory is not a program, and a missing directory is skipped
    /// rather than failing the whole lookup.
    #[test]
    fn directories_and_missing_entries_are_not_matches() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("bin");
        std::fs::create_dir_all(dir.join("git.exe")).unwrap();
        let found = executables_in_dirs("git", vec![dir, root.path().join("does-not-exist")]);
        assert!(found.is_empty());
    }
}
