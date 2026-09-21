//! `MemoryFs` trait — file-existence / summary-read seam.
//!
//! ## Where the existence check happens
//!
//! The selector does **not** call `fs.exists` directly. The
//! `existing_memory_files` list is pre-resolved upstream (well outside
//! the selector crate). The selector reads `exists` only on the
//! synthetic `ExtendedMemoryFileInfo` objects produced
//! upstream, where it's set explicitly per row.
//!
//! This crate still exposes a `MemoryFs` trait for **downstream**
//! consumers that re-implement the path-resolution layer:
//!
//! > File-existence-check abstraction: a trait `MemoryFs { fn
//! > exists(&self, path: &Path) -> bool; fn read_summary(&self,
//! > path: &Path) -> Option<MemorySummary>; }`. Tests use a mock
//! > implementation.
//!
//! ## What this module implements
//!
//! * The [`MemoryFs`] trait — `exists` / `read_summary` seam.
//! * The [`MemorySummary`] return type for `read_summary`. The
//! shape is the minimum the selector / notification builder
//! would need for a row preview.
//! * [`NoFsMemoryFs`] — a no-op implementation that always
//! returns `false` / `None`. Useful for tests and for the
//! "selector receives a pre-resolved list" path this crate
//! actually uses.
//! * [`InMemoryMemoryFs`] — a hash-map-backed implementation
//! that tests can populate with synthetic entries.
//!
//! Implementations of `MemoryFs` are **not** wired into the
//! orchestrator in [`crate::memory::option_list`]. The orchestrator takes
//! the `Vec<ExtendedMemoryFileInfo>` directly, since the caller has
//! already resolved which files exist. No crate implements or calls the
//! trait today.

use std::collections::HashMap;

/// Pure-data summary of a memory file. The minimum set of fields a
/// preview row would need.
///
/// All fields are owned `String`s / primitives — no borrowed crates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemorySummary {
    /// File size in bytes (as reported by the underlying fs).
    pub size_bytes: u64,
    /// Number of lines in the file. The selector doesn't use
    /// this today but a future preview pane would.
    pub line_count: u64,
    /// First non-empty line of the file (truncated to 120 chars by
    /// convention — the trait doesn't enforce a length limit).
    pub first_line: String,
    /// Last-modified UNIX timestamp in milliseconds.
    pub mtime_ms: i64,
}

impl MemorySummary {
    /// Construct a [`MemorySummary`] from raw values. Convenience
    /// builder for tests.
    pub fn new(
        size_bytes: u64,
        line_count: u64,
        first_line: impl Into<String>,
        mtime_ms: i64,
    ) -> Self {
        Self {
            size_bytes,
            line_count,
            first_line: first_line.into(),
            mtime_ms,
        }
    }
}

/// File-existence / summary-read seam. Implementations are **not**
/// required to be `Send + Sync`; the crate is purely synchronous
/// and runs on whatever thread the consumer picks.
pub trait MemoryFs {
    /// `true` if the path exists on disk. The default implementation
    /// for [`NoFsMemoryFs`] is "always false" — useful for the
    /// "selector receives a pre-resolved list" path.
    fn exists(&self, path: &str) -> bool;

    /// Return a [`MemorySummary`] for the given path, or `None` if
    /// the file does not exist or cannot be read.
    fn read_summary(&self, path: &str) -> Option<MemorySummary>;
}

/// No-op implementation. Always returns `false` / `None`.
///
/// Use this in tests where the selector takes a pre-resolved list
/// and the trait is unused, or as a default for downstream code
/// that wants to defer wiring up the real fs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NoFsMemoryFs;

impl MemoryFs for NoFsMemoryFs {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn read_summary(&self, _path: &str) -> Option<MemorySummary> {
        None
    }
}

/// Hash-map-backed in-memory implementation. Tests populate it
/// with synthetic entries; production code should use a real
/// filesystem implementation defined in the consumer crate.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InMemoryMemoryFs {
    entries: HashMap<String, MemorySummary>,
}

impl InMemoryMemoryFs {
    /// Empty in-memory fs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or overwrite) an entry by path.
    pub fn insert(&mut self, path: impl Into<String>, summary: MemorySummary) {
        self.entries.insert(path.into(), summary);
    }

    /// Number of entries currently in the fs.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if the fs has zero entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl MemoryFs for InMemoryMemoryFs {
    fn exists(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    fn read_summary(&self, path: &str) -> Option<MemorySummary> {
        self.entries.get(path).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_fs_exists_is_always_false() {
        let fs = NoFsMemoryFs;
        assert!(!fs.exists("/anything"));
    }

    #[test]
    fn no_fs_read_summary_is_always_none() {
        let fs = NoFsMemoryFs;
        assert_eq!(fs.read_summary("/anything"), None);
    }

    #[test]
    fn in_memory_starts_empty() {
        let fs = InMemoryMemoryFs::new();
        assert!(fs.is_empty());
        assert_eq!(fs.len(), 0);
    }

    #[test]
    fn in_memory_insert_increments_len() {
        let mut fs = InMemoryMemoryFs::new();
        fs.insert(
            "/a",
            MemorySummary::new(10, 1, "first line", 1_700_000_000_000),
        );
        assert_eq!(fs.len(), 1);
        assert!(!fs.is_empty());
    }

    #[test]
    fn in_memory_exists_returns_true_for_inserted_path() {
        let mut fs = InMemoryMemoryFs::new();
        fs.insert("/a", MemorySummary::new(10, 1, "x", 0));
        assert!(fs.exists("/a"));
    }

    #[test]
    fn in_memory_exists_returns_false_for_unknown_path() {
        let mut fs = InMemoryMemoryFs::new();
        fs.insert("/a", MemorySummary::new(10, 1, "x", 0));
        assert!(!fs.exists("/b"));
    }

    #[test]
    fn in_memory_read_summary_returns_inserted_value() {
        let mut fs = InMemoryMemoryFs::new();
        let summary = MemorySummary::new(42, 7, "hello world", 1_700_000_001_000);
        fs.insert("/a", summary.clone());
        assert_eq!(fs.read_summary("/a"), Some(summary));
    }

    #[test]
    fn in_memory_read_summary_returns_none_for_unknown_path() {
        let fs = InMemoryMemoryFs::new();
        assert_eq!(fs.read_summary("/missing"), None);
    }

    #[test]
    fn in_memory_overwrite_replaces_existing_entry() {
        let mut fs = InMemoryMemoryFs::new();
        fs.insert("/a", MemorySummary::new(1, 1, "v1", 1));
        fs.insert("/a", MemorySummary::new(2, 2, "v2", 2));
        assert_eq!(fs.len(), 1);
        assert_eq!(
            fs.read_summary("/a"),
            Some(MemorySummary::new(2, 2, "v2", 2))
        );
    }

    #[test]
    fn memory_summary_new_round_trips_fields() {
        let s = MemorySummary::new(100, 5, "first", 12345);
        assert_eq!(s.size_bytes, 100);
        assert_eq!(s.line_count, 5);
        assert_eq!(s.first_line, "first");
        assert_eq!(s.mtime_ms, 12345);
    }

    #[test]
    fn trait_object_dispatch_works() {
        // Make sure the trait is object-safe so callers can hold a
        // `Box<dyn MemoryFs>` without monomorphisation.
        let mut fs = InMemoryMemoryFs::new();
        fs.insert("/a", MemorySummary::new(1, 1, "x", 0));
        let dyn_fs: Box<dyn MemoryFs> = Box::new(fs);
        assert!(dyn_fs.exists("/a"));
        assert_eq!(dyn_fs.read_summary("/missing"), None);
    }
}
