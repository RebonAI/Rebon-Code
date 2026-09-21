//! Workspace file lookup for the page's `@` mentions.
//!
//! The TUI's at-mention picker searches a `FileIndex` the file scanner
//! fills in the background (git-tracked first, then untracked, then an
//! ignore-aware walk as the fallback). The page has no long-lived process
//! of its own, so this keeps one index per workspace directory on the
//! server, filled by the same scanner, and re-scanned when it has aged. A
//! path-shaped query (`src/`, or nothing typed yet) is answered from the
//! directory itself, exactly as the picker does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rebon_proto::web_api::FilesResponse;

use crate::file_scanner::{self, FileIndex, FileListUpdate};

/// Re-scan a workspace this long after its last scan started.
const REFRESH_AFTER: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Scanning,
    Complete,
    Failed,
}

struct CachedIndex {
    index: Mutex<FileIndex>,
    state: Mutex<ScanState>,
    started: Instant,
}

#[derive(Default)]
pub struct FileIndexCache {
    workspaces: Mutex<HashMap<String, Arc<CachedIndex>>>,
}

impl FileIndexCache {
    /// Files under `cwd` matching `query`, best first.
    pub fn search(&self, cwd: &str, query: &str, limit: usize) -> FilesResponse {
        let limit = limit.clamp(1, 200);
        let query = query.trim();
        let path_shaped = query.is_empty() || query.contains(['/', '\\']);
        if path_shaped {
            return match file_scanner::local_dir_candidates(cwd, query, limit + 1) {
                Ok(candidates) => FilesResponse {
                    root: cwd.to_string(),
                    truncated: candidates.len() > limit,
                    files: candidates
                        .iter()
                        .take(limit)
                        .map(|candidate| {
                            if candidate.is_directory {
                                format!("{}/", candidate.path.trim_end_matches('/'))
                            } else {
                                candidate.path.clone()
                            }
                        })
                        .collect(),
                    scanning: false,
                    problem: None,
                },
                Err(err) => FilesResponse {
                    root: cwd.to_string(),
                    files: Vec::new(),
                    truncated: false,
                    scanning: false,
                    problem: Some(err.to_string()),
                },
            };
        }
        let cached = self.index_for(cwd);
        let results = cached
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .search(query, limit + 1);
        let state = *cached
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        FilesResponse {
            root: cwd.to_string(),
            truncated: results.len() > limit,
            files: results
                .iter()
                .take(limit)
                .map(|result| {
                    if result.is_directory {
                        format!("{}/", result.path.trim_end_matches('/'))
                    } else {
                        result.path.clone()
                    }
                })
                .collect(),
            scanning: state == ScanState::Scanning,
            problem: None,
        }
    }

    fn index_for(&self, cwd: &str) -> Arc<CachedIndex> {
        let mut workspaces = self
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = workspaces.get(cwd) {
            let state = *cached
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let stale = cached.started.elapsed() > REFRESH_AFTER && state != ScanState::Scanning;
            if !stale {
                return cached.clone();
            }
        }
        let cached = Arc::new(CachedIndex {
            index: Mutex::new(FileIndex::default()),
            state: Mutex::new(ScanState::Scanning),
            started: Instant::now(),
        });
        workspaces.insert(cwd.to_string(), cached.clone());
        let mut updates =
            file_scanner::spawn_scanner(&tokio::runtime::Handle::current(), cwd.to_string());
        let fill = cached.clone();
        tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                let mut index = fill
                    .index
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match update {
                    FileListUpdate::Seed { directories, files } => {
                        index.merge_directories(directories);
                        index.merge(files);
                    }
                    FileListUpdate::Tracked(paths)
                    | FileListUpdate::Untracked(paths)
                    | FileListUpdate::Prefix(paths) => index.merge(paths),
                    FileListUpdate::Complete => {
                        *fill
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            ScanState::Complete;
                    }
                    FileListUpdate::Failed(reason) | FileListUpdate::TimedOut(reason) => {
                        tracing::debug!(reason, "rebon serve: workspace file scan ended early");
                        *fill
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = ScanState::Failed;
                    }
                }
            }
            let mut state = fill
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *state == ScanState::Scanning {
                *state = ScanState::Complete;
            }
        });
        cached
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn path_shaped_queries_read_the_directory_and_others_the_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("README.md"), "# x").unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let cache = FileIndexCache::default();

        let files = cache.search(&cwd, "", 10).files;
        assert!(files.iter().any(|f| f == "src/"), "{files:?}");
        assert!(files.iter().any(|f| f == "README.md"), "{files:?}");

        let files = cache.search(&cwd, "src/", 10).files;
        assert!(files.iter().any(|f| f.ends_with("main.rs")), "{files:?}");

        // The fuzzy index fills in the background; wait for it.
        let mut found = false;
        for _ in 0..100 {
            let files = cache.search(&cwd, "main", 10).files;
            if files.iter().any(|f| f.ends_with("main.rs")) {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(found, "the scanner indexes the workspace");
    }
}
