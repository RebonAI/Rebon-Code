//! Scoped durable memory path resolution.
//!
//! This module owns the storage/recall boundary for durable memory. Memory
//! `type` remains content taxonomy (`user`, `feedback`, `project`,
//! `reference`); [`MemoryScope`] is where a memory is stored and recalled.
//!
//! It is a *layout* fact, not feature behavior: every directory it names is
//! `<config_home>/…` keyed by [`crate::session_storage::project_dir_component`],
//! the same two things that decide where a session's transcript lives. Callers
//! that only have to recognise these paths — to auto-approve a write into one,
//! to keep it out of a rewind snapshot, or to refuse letting it widen an
//! explicit write scope — must be able to do so without depending on the memory
//! feature, and the memory feature itself resolves its directories here too.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::config_home::config_home_with_env;

fn rebon_config_home() -> Option<PathBuf> {
    config_home_with_env(|name: &str| std::env::var_os(name))
}

/// Name of the memory index file.
pub const ENTRYPOINT_NAME: &str = "MEMORY.md";

/// Durable memory scopes this crate resolves directories for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// User-global memory, shared across repos, coordinators, and sessions.
    User,
    /// Canonical repo/project memory, shared across coordinators, sessions,
    /// and worktrees that resolve to the same canonical key.
    Repo,
}

/// Return the user-global durable memory directory.
///
/// Layout: `<rebon-config-home>/memory/user/`.
pub fn user_memory_dir() -> Option<PathBuf> {
    Some(rebon_config_home()?.join("memory").join("user"))
}

/// Return the canonical repo/project durable memory directory for `cwd`.
///
/// Layout: `<rebon-config-home>/projects/<canonical-repo-or-cwd-slug>/memory/`.
pub fn repo_memory_dir(cwd: &str) -> Option<PathBuf> {
    let base = rebon_config_home()?;
    let key = canonical_repo_or_cwd_key(cwd);
    // `project_dir_component` (not bare `sanitize_path`): the slug must land in
    // the same `projects/<component>/` bucket as the transcripts for this cwd,
    // which case-fold on Windows. A raw sanitize of a canonicalized path split
    // the project into a second `----D--…` directory.
    let slug = crate::session_storage::project_dir_component(&key);
    Some(base.join("projects").join(slug).join("memory"))
}

/// Where repo memory lived before the canonical key: `<rebon-config-home>/
/// projects/<sanitize(cwd)>/memory/`.
///
/// Private, because nothing may read from it. [`migrate_cwd_memory_dir`] is
/// the only caller — the directory is somewhere to move files *out of*.
fn cwd_keyed_memory_dir(cwd: &str) -> Option<PathBuf> {
    let base = rebon_config_home()?;
    let slug = crate::session_storage::sanitize_path(cwd);
    Some(base.join("projects").join(slug).join("memory"))
}

/// What one call to [`migrate_cwd_memory_dir`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CwdMemoryMigration {
    /// Files moved into the canonical directory under their own names.
    pub moved: usize,
    /// Files whose name was already taken, moved under `<stem>.from-cwd-scope`.
    pub renamed: Vec<PathBuf>,
    /// Files left where they were, because both names were taken.
    pub left_behind: Vec<PathBuf>,
}

/// Move a cwd-keyed memory directory into the canonical repo one, once.
///
/// Repo memory used to be keyed by the sanitized cwd, so a session started in
/// a subdirectory — or on Windows, where canonicalizing rewrites the string —
/// wrote to a different directory than one started at the repo root. The key
/// is now the canonical git root, and this brings the old directory's files
/// across so nothing has to keep reading two places.
///
/// A name already taken in the destination is not overwritten: the incoming
/// file lands beside it as `<stem>.from-cwd-scope<ext>`, and if that is taken
/// too the file stays where it is and is named in `left_behind`. The source
/// directory is removed once it is empty, which is what makes this a
/// migration rather than a fallback — a directory that still has files in it
/// is tried again next time.
///
/// Idempotent and cheap to call: with nothing to move it is two `stat` calls.
pub fn migrate_cwd_memory_dir(cwd: &str) -> CwdMemoryMigration {
    let mut outcome = CwdMemoryMigration::default();
    let (Some(from), Some(into)) = (cwd_keyed_memory_dir(cwd), repo_memory_dir(cwd)) else {
        return outcome;
    };
    if same_path(&from, &into) || !from.is_dir() {
        return outcome;
    }
    let Ok(entries) = std::fs::read_dir(&from) else {
        return outcome;
    };
    if let Err(error) = std::fs::create_dir_all(&into) {
        tracing::warn!(
            dir = %into.display(),
            %error,
            "cannot create the canonical memory directory; cwd-scope memory stays where it is"
        );
        return outcome;
    }

    for entry in entries.flatten() {
        let source = entry.path();
        if source.is_dir() {
            // Memory directories are flat. Anything nested was not put there
            // by Rebon, so leave it rather than guess at its shape.
            outcome.left_behind.push(source);
            continue;
        }
        let Some(name) = source.file_name() else {
            continue;
        };
        let free = into.join(name);
        if !free.exists() {
            move_memory_file(&source, &free, &mut outcome.moved);
            continue;
        }
        let beside = into.join(beside_name(Path::new(name)));
        if beside.exists() {
            outcome.left_behind.push(source);
            continue;
        }
        let before = outcome.moved;
        move_memory_file(&source, &beside, &mut outcome.moved);
        if outcome.moved > before {
            outcome.moved = before;
            outcome.renamed.push(beside);
        }
    }

    if outcome.left_behind.is_empty() {
        let _ = std::fs::remove_dir(&from);
    }
    outcome
}

/// `MEMORY.md` becomes `MEMORY.from-cwd-scope.md`; an extension-less name
/// gets the marker appended.
fn beside_name(name: &Path) -> PathBuf {
    let stem = name.file_stem().unwrap_or(name.as_os_str());
    match name.extension() {
        Some(ext) => PathBuf::from(format!(
            "{}.from-cwd-scope.{}",
            stem.to_string_lossy(),
            ext.to_string_lossy()
        )),
        None => PathBuf::from(format!("{}.from-cwd-scope", stem.to_string_lossy())),
    }
}

fn move_memory_file(source: &Path, destination: &Path, moved: &mut usize) {
    match std::fs::rename(source, destination) {
        Ok(()) => *moved += 1,
        // A rename across volumes fails; the copy is the same move, slower.
        Err(_) => {
            match std::fs::copy(source, destination).and_then(|_| std::fs::remove_file(source)) {
                Ok(()) => *moved += 1,
                Err(error) => tracing::warn!(
                    file = %source.display(),
                    %error,
                    "cannot move cwd-scope memory into the canonical directory"
                ),
            }
        }
    }
}

/// Return the durable memory directory for a scope.
pub fn memory_dir_for_scope(scope: MemoryScope, cwd: Option<&str>) -> Option<PathBuf> {
    match scope {
        MemoryScope::User => user_memory_dir(),
        MemoryScope::Repo => repo_memory_dir(cwd?),
    }
}

/// Return the MEMORY.md entrypoint for a durable memory scope.
pub fn memory_entrypoint_for_scope(scope: MemoryScope, cwd: Option<&str>) -> Option<PathBuf> {
    Some(memory_dir_for_scope(scope, cwd)?.join(ENTRYPOINT_NAME))
}

/// Component-aware check for paths under any auto-memory scope accepted for
/// writes/edits: the user and canonical repo memory directories.
pub fn is_memory_path_for_any_scope(absolute_path: &str, cwd: &str) -> bool {
    let path = normalize_for_prefix(Path::new(absolute_path));
    memory_dirs_for_any_scope(cwd).into_iter().any(|dir| {
        let dir = normalize_for_prefix(&dir);
        path == dir || path.starts_with(&dir)
    })
}

/// Return the user and repo memory dirs with duplicates removed.
pub fn memory_dirs_for_any_scope(cwd: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in [user_memory_dir(), repo_memory_dir(cwd)]
        .into_iter()
        .flatten()
    {
        if !dirs.iter().any(|existing| same_path(existing, &dir)) {
            dirs.push(dir);
        }
    }
    dirs
}

fn canonical_repo_or_cwd_key(cwd: &str) -> String {
    if let Some(git_root) = git_toplevel(cwd) {
        return canonicalize_to_string(&git_root).unwrap_or(git_root);
    }

    canonicalize_to_string(cwd).unwrap_or_else(|| cwd.to_string())
}

fn git_toplevel(cwd: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    if root.is_empty() {
        None
    } else {
        // TODO: To share one memory directory across linked worktrees,
        // refine this to key them by their git common dir. This foundation
        // crate intentionally starts with the toplevel/cwd behavior only.
        Some(root.to_string())
    }
}

/// `std::fs::canonicalize` on Windows yields extended-length paths
/// (`\\?\D:\…`, `\\?\UNC\server\share\…`). Sanitized verbatim they produce a
/// `----D--…` project slug that can never match the transcript directory of
/// the same cwd, so the prefix comes off first.
fn canonicalize_to_string(path: impl AsRef<Path>) -> Option<String> {
    std::fs::canonicalize(path).ok().map(|p| {
        rebon_tools_core::strip_windows_verbatim_prefix(p)
            .to_string_lossy()
            .into_owned()
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    normalize_for_prefix(a) == normalize_for_prefix(b)
}

fn normalize_for_prefix(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }

    #[cfg(windows)]
    {
        PathBuf::from(out.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here rewrites `HOME` / `USERPROFILE` / `REBON_CONFIG_DIR`,
    /// which are process-global: one lock serialises them.
    fn env_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct EnvGuard {
        _temp: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
        prev_rebon_config_dir: Option<std::ffi::OsString>,
        _home_path: PathBuf,
        config_home_path: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let _lock = env_test_lock().lock().unwrap_or_else(|p| p.into_inner());
            let temp = tempfile::tempdir().expect("temp home");
            let home_path = temp.path().to_path_buf();
            let config_home_path = home_path.join(".rebon-test");
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            let prev_rebon_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("HOME", &home_path);
            std::env::set_var("USERPROFILE", &home_path);
            std::env::set_var("REBON_CONFIG_DIR", &config_home_path);
            Self {
                _temp: temp,
                prev_home,
                prev_userprofile,
                prev_rebon_config_dir,
                _home_path: home_path,
                config_home_path,
                _lock,
            }
        }

        fn config_home(&self) -> &Path {
            &self.config_home_path
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev_rebon_config_dir.take() {
                Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_userprofile.take() {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    #[test]
    fn user_path_layout_is_config_memory_user() {
        let g = EnvGuard::new();
        assert_eq!(
            user_memory_dir().unwrap(),
            g.config_home().join("memory").join("user")
        );
    }

    #[test]
    fn repo_path_layout_falls_back_to_raw_cwd_when_git_and_canonicalize_unavailable() {
        let g = EnvGuard::new();
        let cwd = "/definitely/not/a/real/repo";
        let expected = g
            .config_home()
            .join("projects")
            .join(crate::session_storage::sanitize_path(cwd))
            .join("memory");
        assert_eq!(repo_memory_dir(cwd).unwrap(), expected);
    }

    #[test]
    fn repo_path_slug_matches_the_transcript_project_component() {
        let g = EnvGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let cwd = temp.path().to_string_lossy().into_owned();

        let dir = repo_memory_dir(&cwd).unwrap();
        let slug = dir
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        // Never the sanitized `\\?\` extended-length prefix…
        assert!(
            !slug.starts_with("----"),
            "slug leaked the Windows extended-length prefix: {slug:?}"
        );
        // …and byte-identical to where the transcripts for this cwd live.
        if cfg!(windows) {
            assert_eq!(slug, slug.to_lowercase(), "slug must be case-folded");
        }
        assert!(dir.starts_with(g.config_home()));
    }

    #[test]
    fn the_cwd_keyed_path_can_differ_from_the_canonical_repo_path() {
        let _g = EnvGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let raw = child.join("..").join("child");
        let raw_str = raw.to_string_lossy();
        assert_ne!(repo_memory_dir(&raw_str), cwd_keyed_memory_dir(&raw_str));
    }

    #[test]
    fn any_scope_check_accepts_user_and_repo_and_rejects_memory2() {
        let _g = EnvGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let cwd_path = child.join("..").join("child");
        let cwd = cwd_path.to_string_lossy();

        let user_file = user_memory_dir().unwrap().join("profile.md");
        let repo_file = repo_memory_dir(&cwd).unwrap().join("project.md");
        let sibling = repo_memory_dir(&cwd)
            .unwrap()
            .with_file_name("memory2")
            .join("not-memory.md");

        assert!(is_memory_path_for_any_scope(
            &user_file.to_string_lossy(),
            &cwd
        ));
        assert!(is_memory_path_for_any_scope(
            &repo_file.to_string_lossy(),
            &cwd
        ));
        assert!(!is_memory_path_for_any_scope(
            &sibling.to_string_lossy(),
            &cwd
        ));
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Everything the cwd-keyed directory held ends up in the canonical one,
    /// the source directory is gone, and a second call has nothing to do.
    #[test]
    fn cwd_keyed_memory_moves_into_the_canonical_directory_once() {
        let _g = EnvGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let cwd_path = child.join("..").join("child");
        let cwd = cwd_path.to_string_lossy().into_owned();

        let from = cwd_keyed_memory_dir(&cwd).unwrap();
        let into = repo_memory_dir(&cwd).unwrap();
        assert_ne!(from, into, "the fixture must exercise two directories");
        write(&from.join(ENTRYPOINT_NAME), "- [a](a.md)\n");
        write(&from.join("a.md"), "the a memory\n");

        let first = migrate_cwd_memory_dir(&cwd);
        assert_eq!(first.moved, 2);
        assert!(first.renamed.is_empty());
        assert!(first.left_behind.is_empty());
        assert_eq!(
            std::fs::read_to_string(into.join(ENTRYPOINT_NAME)).unwrap(),
            "- [a](a.md)\n"
        );
        assert_eq!(
            std::fs::read_to_string(into.join("a.md")).unwrap(),
            "the a memory\n"
        );
        assert!(!from.exists(), "the source directory is removed once empty");

        assert_eq!(migrate_cwd_memory_dir(&cwd), CwdMemoryMigration::default());
    }

    /// A name already taken in the destination is never overwritten.
    #[test]
    fn a_taken_name_lands_beside_the_file_it_would_have_replaced() {
        let _g = EnvGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let cwd_path = child.join("..").join("child");
        let cwd = cwd_path.to_string_lossy().into_owned();

        let from = cwd_keyed_memory_dir(&cwd).unwrap();
        let into = repo_memory_dir(&cwd).unwrap();
        write(&into.join(ENTRYPOINT_NAME), "the canonical index\n");
        write(&from.join(ENTRYPOINT_NAME), "the cwd-scope index\n");

        let outcome = migrate_cwd_memory_dir(&cwd);
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.renamed, vec![into.join("MEMORY.from-cwd-scope.md")]);
        assert!(outcome.left_behind.is_empty());
        assert_eq!(
            std::fs::read_to_string(into.join(ENTRYPOINT_NAME)).unwrap(),
            "the canonical index\n",
            "the file that was already there is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(into.join("MEMORY.from-cwd-scope.md")).unwrap(),
            "the cwd-scope index\n"
        );
        assert!(!from.exists());
    }

    /// Both names taken means the file stays put, and so does its directory,
    /// so the next run tries again rather than losing it.
    #[test]
    fn a_file_that_cannot_be_placed_is_left_where_it_is() {
        let _g = EnvGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let cwd_path = child.join("..").join("child");
        let cwd = cwd_path.to_string_lossy().into_owned();

        let from = cwd_keyed_memory_dir(&cwd).unwrap();
        let into = repo_memory_dir(&cwd).unwrap();
        write(&into.join(ENTRYPOINT_NAME), "one\n");
        write(&into.join("MEMORY.from-cwd-scope.md"), "two\n");
        write(&from.join(ENTRYPOINT_NAME), "three\n");

        let outcome = migrate_cwd_memory_dir(&cwd);
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.left_behind, vec![from.join(ENTRYPOINT_NAME)]);
        assert_eq!(
            std::fs::read_to_string(from.join(ENTRYPOINT_NAME)).unwrap(),
            "three\n"
        );
        assert!(from.is_dir(), "the directory stays so the next run retries");
    }
}
