//! The memory/instruction files a session has actually loaded, for `/memory`
//! and `/context` to report.
//!
//! Every surface that answers "which instruction files are in play for this
//! cwd" resolves the same set: canonical instruction discovery
//! ([`rebon_instructions::instruction_files::discover_instruction_files`] — the global
//! `REBON.md`, every ancestor `REBON.md` / `.rebon/REBON.md`, eager
//! `.rebon/rules/**/*.md`, `@` includes, and `REBON.local.md`) plus the auto
//! `MEMORY.md` entrypoint, which [`crate::memory::prompt`] owns separately and is
//! therefore appended here.
//!
//! This module is the one provider behind
//! [`rebon_instructions::loaded_documents`]: the seam is below the plugin
//! boundary because the ACP server asks through it, this answer is above it
//! because only the memory feature knows the per-project switch that decides
//! whether the `MEMORY.md` entrypoint is loaded at all.
//!
//! Sizes come from `fs::metadata`, never from reading the file: the token
//! figure is a `bytes / 4` estimate, and the callers include an async ACP
//! request handler where pulling each instruction file into a `String` just to
//! measure its length is waste. Keeping one implementation also keeps the two
//! surfaces from drifting into reporting different files or different numbers
//! for the same session.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rebon_instructions::loaded_documents::{LoadedDocuments, LoadedMemoryFile};

use crate::memory::prompt::get_auto_mem_path;

/// Return the memory/instruction file candidates for `cwd`, in load order and
/// deduped.
///
/// Candidates are paths that *may* exist; use [`gather_memory_files`] for the
/// ones that do. The auto `MEMORY.md` entrypoint is only a candidate when
/// auto-memory is enabled for `cwd`.
pub fn memory_file_candidates(cwd: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> =
        rebon_instructions::instruction_files::discover_instruction_files(cwd)
            .into_iter()
            .map(|file| PathBuf::from(file.path))
            .collect();

    if crate::memory::settings::is_auto_memory_enabled(Path::new(cwd)) {
        if let Some(memory_dir) = get_auto_mem_path(cwd) {
            paths.push(memory_dir.join(rebon_session::memory_paths::ENTRYPOINT_NAME));
        }
    }

    let mut seen: HashSet<PathBuf> = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
}

/// Return the memory/instruction files that exist on disk for `cwd`, in load
/// order, with their sizes.
///
/// Candidates that are missing, unreadable, or not regular files are skipped:
/// this reports what a session loaded, and a path that cannot be stat'd was
/// not loaded.
pub fn gather_memory_files(cwd: &str) -> Vec<LoadedMemoryFile> {
    memory_file_candidates(cwd)
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            metadata.is_file().then(|| LoadedMemoryFile {
                path,
                bytes: metadata.len(),
            })
        })
        .collect()
}

/// The memory feature's answer on the `loaded-documents` seam.
pub struct MemoryLoadedDocuments;

impl LoadedDocuments for MemoryLoadedDocuments {
    fn loaded_files(&self, cwd: &str) -> Vec<LoadedMemoryFile> {
        gather_memory_files(cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
        prev_config_dir: Option<std::ffi::OsString>,
        prev_disable_auto_memory: Option<std::ffi::OsString>,
        prev_simple: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        /// Point every home/config lookup at `config_home` and clear the
        /// auto-memory kill switches, so a test only sees what it created.
        fn new(config_home: &Path) -> Self {
            let _lock = crate::memory::test_env::env_test_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let guard = Self {
                _lock,
                prev_home: std::env::var_os("HOME"),
                prev_userprofile: std::env::var_os("USERPROFILE"),
                prev_config_dir: std::env::var_os("REBON_CONFIG_DIR"),
                prev_disable_auto_memory: std::env::var_os("REBON_DISABLE_AUTO_MEMORY"),
                prev_simple: std::env::var_os("REBON_SIMPLE"),
            };
            std::env::set_var("HOME", config_home);
            std::env::set_var("USERPROFILE", config_home);
            std::env::set_var("REBON_CONFIG_DIR", config_home);
            std::env::remove_var("REBON_DISABLE_AUTO_MEMORY");
            std::env::remove_var("REBON_SIMPLE");
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            let restore = |name: &str, value: Option<std::ffi::OsString>| match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            };
            restore("HOME", self.prev_home.take());
            restore("USERPROFILE", self.prev_userprofile.take());
            restore("REBON_CONFIG_DIR", self.prev_config_dir.take());
            restore(
                "REBON_DISABLE_AUTO_MEMORY",
                self.prev_disable_auto_memory.take(),
            );
            restore("REBON_SIMPLE", self.prev_simple.take());
        }
    }

    /// Resolve both sides through `canonicalize` before comparing: discovery
    /// records include paths unchanged, so a `@./included.md` candidate keeps
    /// its `./` segment on some platforms.
    fn canon(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn contains(files: &[LoadedMemoryFile], expected: &Path) -> bool {
        let expected = canon(expected);
        files.iter().any(|file| canon(&file.path) == expected)
    }

    #[test]
    fn gathers_rules_includes_and_user_level_instructions() {
        let tmp = tempfile::tempdir().unwrap();
        let config_home = tmp.path().join("config-home");
        let project = tmp.path().join("project");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(project.join(".rebon/rules")).unwrap();

        let user = config_home.join("REBON.md");
        let included = project.join("included.md");
        let rebon = project.join("REBON.md");
        let rule = project.join(".rebon/rules/rule.md");
        fs::write(&user, "user instructions").unwrap();
        fs::write(&included, "included content").unwrap();
        fs::write(&rebon, "@./included.md\nproject").unwrap();
        fs::write(&rule, "rule content").unwrap();

        let _guard = EnvGuard::new(&config_home);
        let cwd = project.to_string_lossy().into_owned();

        let memory_dir = get_auto_mem_path(&cwd).expect("auto-memory dir");
        fs::create_dir_all(&memory_dir).unwrap();
        let entrypoint = memory_dir.join("MEMORY.md");
        fs::write(&entrypoint, "- [note](note.md)").unwrap();

        let files = gather_memory_files(&cwd);

        assert!(contains(&files, &user), "user-level REBON.md: {files:?}");
        assert!(contains(&files, &rebon), "project REBON.md: {files:?}");
        assert!(contains(&files, &included), "@ include: {files:?}");
        assert!(contains(&files, &rule), ".rebon/rules file: {files:?}");
        assert!(contains(&files, &entrypoint), "auto MEMORY.md: {files:?}");
    }

    #[test]
    fn token_estimate_comes_from_file_size() {
        let tmp = tempfile::tempdir().unwrap();
        let config_home = tmp.path().join("config-home");
        let project = tmp.path().join("project");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&project).unwrap();
        // Frontmatter and an HTML comment are stripped from the *content*
        // discovery returns; the reported size stays the on-disk length.
        let body = "---\npaths: **\n---\n<!-- hidden -->\n";
        let rebon = project.join("REBON.md");
        fs::write(&rebon, format!("{body}{}", "hello ".repeat(20))).unwrap();
        let on_disk = fs::metadata(&rebon).unwrap().len();

        let _guard = EnvGuard::new(&config_home);
        let cwd = project.to_string_lossy().into_owned();

        let files = gather_memory_files(&cwd);
        let detail = files
            .iter()
            .find(|file| canon(&file.path) == canon(&rebon))
            .expect("fixture row");

        assert_eq!(detail.bytes, on_disk);
        assert_eq!(detail.approx_tokens(), (on_disk / 4) as usize);
    }

    #[test]
    fn skips_candidates_that_do_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let config_home = tmp.path().join("config-home");
        let project = tmp.path().join("project");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&project).unwrap();

        let _guard = EnvGuard::new(&config_home);
        let cwd = project.to_string_lossy().into_owned();

        // The MEMORY.md candidate is offered but never created.
        assert!(memory_file_candidates(&cwd)
            .iter()
            .any(|path| path.ends_with("MEMORY.md")));
        let found = gather_memory_files(&cwd);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn candidates_are_deduped_in_load_order() {
        let tmp = tempfile::tempdir().unwrap();
        let config_home = tmp.path().join("config-home");
        let project = tmp.path().join("project");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(config_home.join("REBON.md"), "user").unwrap();
        fs::write(project.join("REBON.md"), "project").unwrap();

        let _guard = EnvGuard::new(&config_home);
        let cwd = project.to_string_lossy().into_owned();

        let candidates = memory_file_candidates(&cwd);
        let mut deduped = candidates.clone();
        deduped.sort();
        deduped.dedup();

        assert_eq!(deduped.len(), candidates.len(), "{candidates:?}");
        assert!(candidates[0].starts_with(&config_home), "{candidates:?}");
        assert!(candidates.last().unwrap().ends_with("MEMORY.md"));
    }

    #[test]
    fn auto_memory_entrypoint_is_omitted_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let config_home = tmp.path().join("config-home");
        let project = tmp.path().join("project");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("REBON.md"), "project").unwrap();

        let _guard = EnvGuard::new(&config_home);
        std::env::set_var("REBON_DISABLE_AUTO_MEMORY", "1");
        let cwd = project.to_string_lossy().into_owned();

        let candidates = memory_file_candidates(&cwd);

        assert!(!candidates.iter().any(|path| path.ends_with("MEMORY.md")));
        assert!(candidates.iter().any(|path| path.ends_with("REBON.md")));
    }
}
