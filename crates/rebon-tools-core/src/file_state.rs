//! Per-session cache of files the model has observed via the `Read` tool, used
//! by `Edit` / `Write` to enforce "must read before edit" and to detect
//! external modification between a Read and a write.
//!
//! The cache is a plain `HashMap` behind a `Mutex`: no LRU eviction and no byte
//! cap.
//!
//! Key invariants:
//!
//! * Read stores the **raw file content** (not rendered / not line-numbered)
//!   along with the file's current mtime.
//! * Edit/Write **refresh the entry immediately after writing to disk**,
//!   setting `timestamp_ms` to the post-write mtime and clearing `offset` /
//!   `limit`. That is what lets the model call Edit multiple times on the same
//!   file *without* re-reading: the next decisive check compares disk content
//!   with the just-written cached snapshot, and the mtime stays useful metadata
//!   rather than a gate on content validation.
//! * `is_partial_view` is **not** a Read concept. It is set only when
//!   auto-injected memory content (REBON.md / MEMORY.md / CLAUDE.md) was
//!   processed (stripped HTML comments, stripped frontmatter, truncated
//!   MEMORY.md) such that the bytes the model saw no longer match the bytes on
//!   disk. In that one case `content` holds the RAW disk bytes (so diffing
//!   against disk still works) and Edit/Write refuse until the model performs
//!   an explicit Read to see the real content. Regular `offset` / `limit` /
//!   auto-truncation does NOT set this flag — the Read path stores exactly what
//!   the model saw.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

/// Snapshot of a file as observed by the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    /// The file's content at the time it was observed. For Reads, this
    /// is the raw byte-to-string decoded content (no line numbers, no
    /// truncation note). For post-Edit/Write refresh, this is the new
    /// content just written to disk.
    pub content: String,
    /// Whole milliseconds since the Unix epoch at observation time; 0 if
    /// the stat failed.
    pub timestamp_ms: u64,
    /// 1-based starting line of the Read, or `None` for full reads /
    /// post-Edit / post-Write refreshes.
    pub offset: Option<u64>,
    /// Number of lines the Read actually returned, or `None`.
    pub limit: Option<u64>,
    /// True when this entry was populated by auto-injection (REBON.md
    /// / MEMORY.md) and the injected content did not match disk
    /// (stripped HTML comments, stripped frontmatter, truncated
    /// MEMORY.md). The model has only seen a transformed view; Edit /
    /// Write require an explicit Read first. `content` here holds the
    /// RAW disk bytes (so it can still be diffed against disk), not
    /// what the model saw. Never set from the Read path — `offset` / `limit` /
    /// auto-truncation do not imply a partial view.
    pub is_partial_view: bool,
}

/// Shared, cloneable handle to the per-session file-state map.
///
/// Cloning the handle clones the `Arc`, not the map — all clones see
/// the same underlying state. Tool invocations can run concurrently, so
/// mutations go through a `Mutex`.
#[derive(Debug, Clone, Default)]
pub struct FileStateCache {
    inner: Arc<Mutex<HashMap<String, FileState>>>,
}

impl FileStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path) -> Option<FileState> {
        let key = normalize_path_key(path);
        self.inner
            .lock()
            .expect("file state table poisoned")
            .get(&key)
            .cloned()
    }

    pub fn set(&self, path: &Path, state: FileState) {
        let key = normalize_path_key(path);
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.insert(key, state);
        }
    }

    pub fn has(&self, path: &Path) -> bool {
        let key = normalize_path_key(path);
        self.inner
            .lock()
            .map(|g| g.contains_key(&key))
            .unwrap_or(false)
    }

    /// Remove any entry for `path`; returns the previous state if any.
    ///
    /// Not used by Edit/Write, and no production caller today — kept as
    /// public API.
    pub fn remove(&self, path: &Path) -> Option<FileState> {
        let key = normalize_path_key(path);
        self.inner
            .lock()
            .expect("file state table poisoned")
            .remove(&key)
    }

    #[doc(hidden)]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("file state table poisoned").len()
    }

    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn entries(&self) -> Vec<(String, FileState)> {
        let mut entries = self
            .inner
            .lock()
            .map(|g| {
                g.iter()
                    .map(|(path, state)| (path.clone(), state.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
}
/// Normalise a path into the map key used by `FileStateCache`.
///
/// * Backslashes → forward slashes (so Read-of-`F:\foo` and Edit-of-
///   `F:/foo` hit the same entry).
/// * Windows: lowercase, because the filesystem is case-insensitive.
///
/// This does **not** canonicalise (no symlink resolution, no `..`
/// collapsing). Canonicalisation would require the file to exist,
/// which it might not on the create-via-Edit path.
pub fn normalize_path_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let normalized: String = raw.replace('\\', "/");
    #[cfg(windows)]
    {
        normalized.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

/// Read the file's mtime as whole milliseconds since the Unix epoch —
/// `modified().duration_since(UNIX_EPOCH)` truncated by `as_millis()`.
///
/// Returns `Ok(0)` if the platform doesn't expose a monotonic mtime
/// (rare); returns an `Err` if the file itself can't be stat-ed.
pub fn file_mtime_ms(path: &Path) -> std::io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    let d = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    Ok(d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn roundtrip_full_read_then_overwrite() {
        let cache = FileStateCache::new();
        let p = PathBuf::from("/tmp/a.txt");
        assert!(!cache.has(&p));

        cache.set(
            &p,
            FileState {
                content: "hello".into(),
                timestamp_ms: 1000,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let got = cache.get(&p).unwrap();
        assert_eq!(got.content, "hello");
        assert_eq!(got.timestamp_ms, 1000);
        assert!(!got.is_partial_view);

        // Post-edit refresh: new content and higher timestamp.
        cache.set(
            &p,
            FileState {
                content: "hello world".into(),
                timestamp_ms: 1001,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let got = cache.get(&p).unwrap();
        assert_eq!(got.content, "hello world");
        assert_eq!(got.timestamp_ms, 1001);
    }

    #[test]
    fn partial_view_is_preserved() {
        let cache = FileStateCache::new();
        let p = PathBuf::from("/tmp/a.txt");
        cache.set(
            &p,
            FileState {
                content: "line1\nline2".into(),
                timestamp_ms: 500,
                offset: Some(10),
                limit: Some(2),
                is_partial_view: true,
            },
        );
        let got = cache.get(&p).unwrap();
        assert_eq!(got.offset, Some(10));
        assert_eq!(got.limit, Some(2));
        assert!(got.is_partial_view);
    }

    #[test]
    fn normalize_converts_backslashes() {
        let a = normalize_path_key(&PathBuf::from("F:\\foo\\bar.rs"));
        let b = normalize_path_key(&PathBuf::from("F:/foo/bar.rs"));
        assert_eq!(a, b);
    }

    #[cfg(windows)]
    #[test]
    fn windows_keys_are_case_insensitive() {
        let a = normalize_path_key(&PathBuf::from("F:\\Foo\\Bar.rs"));
        let b = normalize_path_key(&PathBuf::from("f:/foo/bar.rs"));
        assert_eq!(a, b);
    }

    #[cfg(windows)]
    #[test]
    fn windows_cache_hit_across_case_variants() {
        let cache = FileStateCache::new();
        let written = PathBuf::from("F:\\Code\\Foo.rs");
        let queried = PathBuf::from("f:/code/foo.rs");
        cache.set(
            &written,
            FileState {
                content: "x".into(),
                timestamp_ms: 1,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        assert!(
            cache.has(&queried),
            "Windows key should be case-insensitive"
        );
    }

    #[test]
    fn cloned_handle_shares_state() {
        let a = FileStateCache::new();
        let b = a.clone();
        let p = PathBuf::from("/tmp/shared.txt");
        a.set(
            &p,
            FileState {
                content: "x".into(),
                timestamp_ms: 1,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        assert!(b.has(&p), "clones must share the underlying map");
    }

    #[test]
    fn remove_returns_prior_state() {
        let cache = FileStateCache::new();
        let p = PathBuf::from("/tmp/a.txt");
        cache.set(
            &p,
            FileState {
                content: "x".into(),
                timestamp_ms: 1,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let prev = cache.remove(&p).unwrap();
        assert_eq!(prev.content, "x");
        assert!(!cache.has(&p));
        assert!(cache.remove(&p).is_none());
    }

    #[test]
    fn file_mtime_ms_reads_present_file() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-fsc-")
            .tempdir()
            .unwrap();
        let p = dir.path().join("m.txt");
        std::fs::write(&p, b"hi").unwrap();
        let ts = file_mtime_ms(&p).expect("mtime");
        assert!(ts > 0, "mtime_ms should be positive for a fresh file");
    }

    #[test]
    fn file_mtime_ms_missing_file_is_err() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("rebon-fsc-nope-{}.txt", std::process::id()));
        assert!(file_mtime_ms(&p).is_err());
    }
}
