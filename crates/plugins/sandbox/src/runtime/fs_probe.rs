//! The filesystem questions a mount plan has to ask.
//!
//! The Linux backend cannot build a correct mount plan from paths
//! alone: it needs to know whether a path exists, whether it is a
//! directory, and — the security-critical one — where a symlink
//! actually resolves to. RFC §4.1 turns each of those into a check
//! that can *reject* a rule.
//!
//! Those questions go through a trait for one reason: the checks are
//! the part most likely to be wrong, and a trait lets the whole
//! rejection matrix be tested on any machine, including the Windows
//! and macOS boxes where `bwrap` will never run. [`RealFs`] is the
//! only implementation shipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What the mount planner needs to know about a path.
pub trait FsProbe {
    /// Whether the path exists, following symlinks.
    fn exists(&self, path: &Path) -> bool;
    /// Whether the path is a directory, following symlinks.
    fn is_dir(&self, path: &Path) -> bool;
    /// Whether the path itself is a symlink (does **not** follow).
    fn is_symlink(&self, path: &Path) -> bool;
    /// Fully resolved path, or `None` when it cannot be resolved.
    fn real_path(&self, path: &Path) -> Option<PathBuf>;
}

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl FsProbe for RealFs {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_symlink(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
    }

    fn real_path(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }
}

/// An in-memory filesystem for tests.
///
/// Deliberately minimal: a path is either a directory, a file, or a
/// symlink pointing somewhere. That is exactly the three-way
/// distinction the mount planner branches on, and nothing more.
#[derive(Debug, Clone, Default)]
pub struct FakeFs {
    entries: BTreeMap<PathBuf, FakeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FakeEntry {
    Dir,
    File,
    Symlink(PathBuf),
}

impl FakeFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.entries.insert(path.into(), FakeEntry::Dir);
        self
    }

    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.entries.insert(path.into(), FakeEntry::File);
        self
    }

    /// A symlink at `path` whose contents resolve to `target`.
    pub fn symlink(mut self, path: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        self.entries
            .insert(path.into(), FakeEntry::Symlink(target.into()));
        self
    }

    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        // One hop is enough: the planner only needs to know whether
        // the resolved location differs from the requested one, and a
        // chain of links differs at the first hop already.
        match self.entries.get(path) {
            Some(FakeEntry::Symlink(target)) => Some(target.clone()),
            Some(_) => Some(path.to_path_buf()),
            None => None,
        }
    }
}

impl FsProbe for FakeFs {
    fn exists(&self, path: &Path) -> bool {
        match self.entries.get(path) {
            Some(FakeEntry::Symlink(target)) => self.entries.contains_key(target),
            Some(_) => true,
            None => false,
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        match self.entries.get(path) {
            Some(FakeEntry::Dir) => true,
            Some(FakeEntry::Symlink(target)) => {
                matches!(self.entries.get(target), Some(FakeEntry::Dir))
            }
            _ => false,
        }
    }

    fn is_symlink(&self, path: &Path) -> bool {
        matches!(self.entries.get(path), Some(FakeEntry::Symlink(_)))
    }

    fn real_path(&self, path: &Path) -> Option<PathBuf> {
        self.resolve(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_fs_reports_directories_and_files_apart() {
        let fs = FakeFs::new().dir("/a").file("/a/b");
        assert!(fs.exists(Path::new("/a")));
        assert!(fs.is_dir(Path::new("/a")));
        assert!(fs.exists(Path::new("/a/b")));
        assert!(!fs.is_dir(Path::new("/a/b")));
        assert!(!fs.exists(Path::new("/a/c")));
    }

    #[test]
    fn fake_symlink_resolves_elsewhere_and_reports_as_a_link() {
        let fs = FakeFs::new().dir("/real").symlink("/link", "/real");
        assert!(fs.is_symlink(Path::new("/link")));
        assert!(!fs.is_symlink(Path::new("/real")));
        assert_eq!(
            fs.real_path(Path::new("/link")),
            Some(PathBuf::from("/real"))
        );
        assert!(fs.is_dir(Path::new("/link")));
    }

    #[test]
    fn dangling_fake_symlink_does_not_exist() {
        let fs = FakeFs::new().symlink("/link", "/gone");
        assert!(fs.is_symlink(Path::new("/link")));
        assert!(!fs.exists(Path::new("/link")));
    }

    #[test]
    fn real_fs_agrees_with_the_trait_on_a_temp_dir() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();

        let fs = RealFs;
        assert!(fs.exists(dir));
        assert!(fs.is_dir(dir));
        assert!(fs.exists(&file));
        assert!(!fs.is_dir(&file));
        assert!(!fs.is_symlink(&file));
        assert!(!fs.exists(&dir.join("missing")));
        assert!(fs.real_path(&file).is_some());
    }
}
