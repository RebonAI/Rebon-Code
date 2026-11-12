//! Per-session cache of files the model has observed: what `Read` showed,
//! what this session's own `Edit` / `Write` / `MultiEdit` / `NotebookEdit`
//! wrote, and what the system prompt quoted verbatim.
//!
//! The cache is a plain `HashMap` behind a `Mutex`: no LRU eviction and no byte
//! cap.
//!
//! Edits do not consult it: files change under the session all the time
//! (formatters, hooks, other agents), and `Edit` matches `old_string`
//! against the current content instead. Only a whole-file `Write`, which
//! would silently drop such a change, compares the snapshot with the disk,
//! and a refusal hands the model the current content so it costs no extra
//! round. Two things read the cache:
//!
//! * **`Write`'s "seen" rule.** Overwriting an existing file needs either
//!   `overwrite: true`, or an entry here that shows the model saw the whole
//!   file (not a partial view, no `offset` / `limit`) whose `content` still
//!   matches the file. A changed file is refused with its current content.
//! * **`Read`'s range merge**, which widens a new range to an overlapping one
//!   it already showed while the content is unchanged (`content`, `offset`,
//!   `limit`).
//!
//! Key invariants:
//!
//! * Read stores the **raw file content** (not rendered / not line-numbered).
//! * Edit/Write **refresh the entry immediately after writing to disk** with
//!   the new content, clearing `offset` / `limit`. Only a whole-file write
//!   (`Write`, a file an edit creates) makes an unseen file seen; an edit of
//!   an existing file refreshes an entry that is there and adds none, since
//!   the model saw only the text it replaced.
//! * `is_partial_view` is **not** a Read concept. It is set only when
//!   auto-injected memory content (REBON.md / MEMORY.md / CLAUDE.md) was
//!   processed (stripped HTML comments, stripped frontmatter, truncated
//!   MEMORY.md) such that the bytes the model saw no longer match the bytes on
//!   disk. Such an entry does not count as seen for `Write`. Regular
//!   `offset` / `limit` / auto-truncation does NOT set this flag — the Read
//!   path stores exactly what the model saw.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Snapshot of a file as observed by the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    /// The file's content at the time it was observed. For Reads, this
    /// is the raw byte-to-string decoded content (no line numbers, no
    /// truncation note). For post-Edit/Write refresh, this is the new
    /// content just written to disk.
    pub content: String,
    /// 1-based starting line of the Read, or `None` for full reads /
    /// post-Edit / post-Write refreshes.
    pub offset: Option<u64>,
    /// Number of lines the Read actually returned, or `None`.
    pub limit: Option<u64>,
    /// True when this entry was populated by auto-injection (REBON.md
    /// / MEMORY.md) and the injected content did not match disk
    /// (stripped HTML comments, stripped frontmatter, truncated
    /// MEMORY.md). The model has only seen a transformed view, so the
    /// entry does not count as seen for Write. `content` here holds the
    /// RAW disk bytes, not what the model saw. Never set from the Read
    /// path — `offset` / `limit` / auto-truncation do not imply a partial
    /// view.
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

    /// Record that a whole file was just written with `content`: the writer
    /// chose every byte, so the file now counts as seen.
    pub fn record_whole_write(&self, path: &Path, content: String) {
        self.set(
            path,
            FileState {
                content,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
    }

    /// Record that part of a file was just rewritten in place (an `Edit`, a
    /// notebook cell), leaving its content `content`.
    ///
    /// An edit shows the model only the text it replaced, so it does not
    /// widen what the model has seen: with no entry this records nothing, a
    /// partial view or a ranged Read (`offset` / `limit`) stays partial, and
    /// an entry the model saw whole gets the new content, so `Write`'s
    /// changed-since-seen check does not mistake the session's own edits
    /// for someone else's.
    pub fn record_partial_write(&self, path: &Path, content: String) {
        let key = normalize_path_key(path);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.get_mut(&key) {
            entry.content = content;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A partial write never creates an entry or upgrades a partial view;
    /// a whole write makes the file seen.
    #[test]
    fn only_a_whole_write_makes_a_file_seen() {
        let cache = FileStateCache::new();
        let unseen = PathBuf::from("/tmp/unseen.txt");
        cache.record_partial_write(&unseen, "edited".into());
        assert!(cache.get(&unseen).is_none());

        let injected = PathBuf::from("/tmp/MEMORY.md");
        cache.set(
            &injected,
            FileState {
                content: "raw".into(),
                offset: None,
                limit: None,
                is_partial_view: true,
            },
        );
        cache.record_partial_write(&injected, "raw edited".into());
        let state = cache.get(&injected).unwrap();
        assert_eq!(state.content, "raw edited");
        assert!(state.is_partial_view);

        let read = PathBuf::from("/tmp/read.txt");
        cache.set(
            &read,
            FileState {
                content: "a\nb".into(),
                offset: Some(2),
                limit: Some(1),
                is_partial_view: false,
            },
        );
        // A ranged Read stays partial after an edit: the edit showed no more.
        cache.record_partial_write(&read, "a\nB".into());
        let state = cache.get(&read).unwrap();
        assert_eq!(
            (state.content.as_str(), state.offset, state.limit),
            ("a\nB", Some(2), Some(1))
        );

        cache.record_whole_write(&unseen, "whole".into());
        let state = cache.get(&unseen).unwrap();
        assert_eq!(state.content, "whole");
        assert!(!state.is_partial_view);
    }

    #[test]
    fn roundtrip_full_read_then_overwrite() {
        let cache = FileStateCache::new();
        let p = PathBuf::from("/tmp/a.txt");
        assert!(!cache.has(&p));

        cache.set(
            &p,
            FileState {
                content: "hello".into(),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let got = cache.get(&p).unwrap();
        assert_eq!(got.content, "hello");
        assert!(!got.is_partial_view);

        // Post-edit refresh: new content.
        cache.set(
            &p,
            FileState {
                content: "hello world".into(),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let got = cache.get(&p).unwrap();
        assert_eq!(got.content, "hello world");
    }

    #[test]
    fn partial_view_is_preserved() {
        let cache = FileStateCache::new();
        let p = PathBuf::from("/tmp/a.txt");
        cache.set(
            &p,
            FileState {
                content: "line1\nline2".into(),
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
}
