//! Where an agent's file access lands.
//!
//! When Rebon advertises `fs.readTextFile` / `fs.writeTextFile`, the
//! agent stops touching the disk and asks us instead. That is the
//! whole point: a write that arrives as a request is a write Rebon can
//! snapshot first, and a snapshot is what `/rewind` restores from.
//!
//! Whether that actually happens depends on the implementation the
//! host wires in, so [`HostFs::snapshots_writes`] is not decoration —
//! it is what [`crate::AcpAgentBackend::snapshots_routed_writes`]
//! reports, and therefore what says whether a write that does route
//! through us keeps its pre-image. `writes_through_host_fs` answers a
//! different question — does the agent ask us to write at all — and is
//! always `false` on this leg. [`DirectHostFs`] answers `false`,
//! because it writes straight through; [`SnapshotHostFs`] answers
//! `true`, because it routes every write through the host's
//! file-history pipeline first.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

/// The host's filesystem, as the agent sees it.
#[async_trait]
pub trait HostFs: Send + Sync {
    /// Read a file on the agent's behalf.
    ///
    /// `line` is 1-based; `line`/`limit` together request a window
    /// rather than the whole file.
    async fn read_text_file(
        &self,
        session_id: &str,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> anyhow::Result<String>;

    /// Write a file on the agent's behalf.
    async fn write_text_file(
        &self,
        session_id: &str,
        path: &Path,
        content: &str,
    ) -> anyhow::Result<()>;

    /// Whether [`Self::write_text_file`] takes a pre-write snapshot
    /// that `/rewind` can restore.
    ///
    /// Answer `false` unless writes really do go through the host's
    /// file-history pipeline. A `true` here is a promise the rewind UI
    /// will make to the user on your behalf.
    fn snapshots_writes(&self) -> bool {
        false
    }
}

/// Plain disk access, with no snapshots and no permission checks.
///
/// This is the honest floor: it satisfies the agent's requests so the
/// agent does not fall back to writing behind Rebon's back, while
/// reporting that nothing it writes can be rewound.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectHostFs;

#[async_trait]
impl HostFs for DirectHostFs {
    async fn read_text_file(
        &self,
        _session_id: &str,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> anyhow::Result<String> {
        let path = PathBuf::from(path);
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| anyhow::anyhow!("failed to read {}: {err}", path.display()))?;
        Ok(slice_lines(&content, line, limit))
    }

    async fn write_text_file(
        &self,
        _session_id: &str,
        path: &Path,
        content: &str,
    ) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|err| {
                    anyhow::anyhow!("failed to create {}: {err}", parent.display())
                })?;
            }
        }
        tokio::fs::write(path, content)
            .await
            .map_err(|err| anyhow::anyhow!("failed to write {}: {err}", path.display()))
    }
}

/// The host's file-history pipeline, as this leg needs it.
///
/// Mirrors the shape of the tracker the local engine already drives
/// around `Write`/`Edit`, minus the tool vocabulary: a turn is armed,
/// files are snapshotted before being overwritten, and the turn is
/// closed out. Implemented by the host — the ACP client only knows it
/// has to call these in the right order, because a write that lands
/// without a snapshot is a write `/rewind` cannot undo.
///
/// Every method takes the *host* session id. A host serving one
/// session may ignore it; one serving several must route on it, and
/// [`SnapshotHostFs`] does not do that routing for you.
pub trait HostFileHistory: Send + Sync {
    /// Arm the turn identified by `turn_id` (the transcript's user-row
    /// uuid). Called before the agent is prompted.
    fn begin_turn(&self, session_id: &str, turn_id: &str) -> anyhow::Result<()>;

    /// Capture `path` as it is *now*, before it is overwritten.
    fn snapshot_before_write(&self, session_id: &str, path: &Path) -> anyhow::Result<()>;

    /// Close out the turn. Called however the turn ended.
    fn end_turn(&self, session_id: &str, turn_id: &str) -> anyhow::Result<()>;

    /// Whether a turn is currently armed — i.e. whether a snapshot
    /// taken right now would belong to a turn `/rewind` can restore.
    ///
    /// `None` means the pipeline cannot tell, which callers must treat
    /// as "assume yes". This exists because `snapshot_before_write`
    /// outside a turn quietly succeeds without capturing anything; a
    /// caller that wants to warn about that needs a probe.
    fn is_armed(&self) -> Option<bool> {
        None
    }
}

/// Host filesystem access that snapshots before it writes.
///
/// This is the implementation that makes `/rewind` mean something for
/// an ACP agent: the agent asks Rebon to write, Rebon photographs the
/// old contents into the session's file history, and only then does the
/// new content land. Hence [`HostFs::snapshots_writes`] answering
/// `true` — and hence the write refusing to happen at all when the
/// snapshot fails, because a silent snapshot failure would leave the
/// user with a rewind button that restores the wrong thing.
///
/// Writes are also confined to the session's roots. The agent is a
/// separate process with its own idea of what it may touch; a request
/// to write outside the workspace is refused here rather than trusted.
pub struct SnapshotHostFs {
    history: Arc<dyn HostFileHistory>,
    roots: Vec<PathBuf>,
}

impl SnapshotHostFs {
    /// `roots` are the directories the agent may write inside —
    /// normally the session cwd plus any `--add-dir` grants. An empty
    /// list refuses every write, which is the safe reading of "no
    /// directory was granted".
    pub fn new(
        history: Arc<dyn HostFileHistory>,
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        Self {
            history,
            roots: roots.into_iter().map(|root| normalize(&root)).collect(),
        }
    }

    /// The file-history pipeline behind this filesystem, so the caller
    /// can arm and close turns through the same handle it wired in.
    pub fn history(&self) -> &Arc<dyn HostFileHistory> {
        &self.history
    }

    fn check_write_allowed(&self, path: &Path) -> anyhow::Result<()> {
        let target = normalize(path);
        if self.roots.iter().any(|root| contains(root, &target)) {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "refusing to write outside the session's directories: {}",
            path.display()
        ))
    }
}

#[async_trait]
impl HostFs for SnapshotHostFs {
    async fn read_text_file(
        &self,
        session_id: &str,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> anyhow::Result<String> {
        // Reads are not confined to the roots. An agent that wanted to
        // read outside them would simply read the disk itself — the
        // refusal would buy nothing and would break legitimate reads of
        // config living in the user's home directory.
        DirectHostFs
            .read_text_file(session_id, path, line, limit)
            .await
    }

    async fn write_text_file(
        &self,
        session_id: &str,
        path: &Path,
        content: &str,
    ) -> anyhow::Result<()> {
        self.check_write_allowed(path)?;
        self.history
            .snapshot_before_write(session_id, path)
            .map_err(|err| {
                anyhow::anyhow!(
                    "refusing to write {} because it could not be snapshotted first \
                     (rewind would not be able to undo it): {err}",
                    path.display()
                )
            })?;
        DirectHostFs
            .write_text_file(session_id, path, content)
            .await
    }

    fn snapshots_writes(&self) -> bool {
        true
    }
}

impl std::fmt::Debug for SnapshotHostFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotHostFs")
            .field("roots", &self.roots)
            .finish()
    }
}

/// Resolve `.` and `..` without touching the filesystem.
///
/// Lexical on purpose: the file being written usually does not exist
/// yet, so `canonicalize` would fail on exactly the paths that matter.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` is `root` or sits underneath it.
///
/// Compares component by component so `/repo-secrets` is not read as
/// living inside `/repo`, and case-insensitively on Windows, where the
/// same directory has many spellings.
fn contains(root: &Path, path: &Path) -> bool {
    let mut root_components = root.components();
    let mut path_components = path.components();
    loop {
        match (root_components.next(), path_components.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(expected), Some(actual)) => {
                let expected = expected.as_os_str();
                let actual = actual.as_os_str();
                let same = if cfg!(windows) {
                    expected
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&actual.to_string_lossy())
                } else {
                    expected == actual
                };
                if !same {
                    return false;
                }
            }
        }
    }
}

/// Take the `limit` lines starting at 1-based `line`.
///
/// Out-of-range windows yield an empty string rather than an error:
/// the agent asked for a slice that is not there, which is not a
/// filesystem failure.
pub(crate) fn slice_lines(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return content.to_string();
    }
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let lines: Vec<&str> = content.lines().collect();
    if start >= lines.len() {
        return String::new();
    }
    let end = match limit {
        Some(limit) => (start + limit as usize).min(lines.len()),
        None => lines.len(),
    };
    lines[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_window_returns_the_whole_file_verbatim() {
        let content = "a\nb\nc\n";
        assert_eq!(slice_lines(content, None, None), content);
    }

    #[test]
    fn line_is_one_based() {
        assert_eq!(slice_lines("a\nb\nc", Some(2), None), "b\nc");
        assert_eq!(slice_lines("a\nb\nc", Some(1), Some(2)), "a\nb");
    }

    #[test]
    fn a_window_past_the_end_is_empty_not_an_error() {
        assert_eq!(slice_lines("a\nb", Some(50), Some(10)), "");
    }

    #[test]
    fn a_limit_longer_than_the_file_stops_at_the_end() {
        assert_eq!(slice_lines("a\nb", Some(1), Some(99)), "a\nb");
    }

    #[test]
    fn direct_fs_admits_it_cannot_rewind() {
        assert!(!DirectHostFs.snapshots_writes());
    }

    #[tokio::test]
    async fn direct_fs_round_trips_a_file() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-acp-client-fs-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("nested").join("note.txt");
        let fs = DirectHostFs;
        fs.write_text_file("sess", &path, "hello\nworld")
            .await
            .unwrap();
        let read = fs.read_text_file("sess", &path, None, None).await.unwrap();
        assert_eq!(read, "hello\nworld");
        let windowed = fs
            .read_text_file("sess", &path, Some(2), Some(1))
            .await
            .unwrap();
        assert_eq!(windowed, "world");
    }

    /// Records what the host's file-history pipeline was asked to do,
    /// and can be told to fail the way a real store fails.
    #[derive(Default)]
    struct RecordingHistory {
        calls: std::sync::Mutex<Vec<String>>,
        fail_snapshot: bool,
    }

    impl RecordingHistory {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("history mutex").clone()
        }

        fn record(&self, call: String) {
            self.calls.lock().expect("history mutex").push(call);
        }
    }

    impl HostFileHistory for RecordingHistory {
        fn begin_turn(&self, session_id: &str, turn_id: &str) -> anyhow::Result<()> {
            self.record(format!("begin {session_id} {turn_id}"));
            Ok(())
        }

        fn snapshot_before_write(&self, session_id: &str, path: &Path) -> anyhow::Result<()> {
            self.record(format!("snapshot {session_id} {}", path.display()));
            if self.fail_snapshot {
                anyhow::bail!("store is full");
            }
            Ok(())
        }

        fn end_turn(&self, session_id: &str, turn_id: &str) -> anyhow::Result<()> {
            self.record(format!("end {session_id} {turn_id}"));
            Ok(())
        }
    }

    fn scratch_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-acp-client-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    #[tokio::test]
    async fn a_snapshotting_fs_photographs_the_old_contents_before_overwriting() {
        let root = scratch_dir("snap-write");
        let path = root.path().join("note.txt");
        tokio::fs::write(&path, "before").await.unwrap();

        let history = Arc::new(RecordingHistory::default());
        let fs = SnapshotHostFs::new(history.clone(), [root.path().to_path_buf()]);
        assert!(
            fs.snapshots_writes(),
            "this is the promise the rewind UI is built on"
        );

        fs.write_text_file("sess", &path, "after").await.unwrap();

        assert_eq!(
            history.calls(),
            vec![format!("snapshot sess {}", path.display())],
            "the snapshot must happen, and must happen first"
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "after");
    }

    #[tokio::test]
    async fn a_write_that_cannot_be_snapshotted_does_not_happen() {
        // Letting the write through would leave the user with a rewind
        // button that restores the wrong contents — worse than an error.
        let root = scratch_dir("snap-fail");
        let path = root.path().join("note.txt");
        tokio::fs::write(&path, "before").await.unwrap();

        let history = Arc::new(RecordingHistory {
            fail_snapshot: true,
            ..Default::default()
        });
        let fs = SnapshotHostFs::new(history, [root.path().to_path_buf()]);
        let err = fs
            .write_text_file("sess", &path, "after")
            .await
            .expect_err("must refuse the write");

        assert!(err.to_string().contains("snapshotted"), "{err}");
        assert!(err.to_string().contains("store is full"), "{err}");
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "before",
            "the file must be untouched"
        );
    }

    #[tokio::test]
    async fn writes_outside_the_granted_roots_are_refused() {
        let root = scratch_dir("snap-roots");
        let history = Arc::new(RecordingHistory::default());
        let fs = SnapshotHostFs::new(history.clone(), [root.path().join("inside")]);

        let escape = root.path().join("inside").join("..").join("outside.txt");
        let err = fs
            .write_text_file("sess", &escape, "nope")
            .await
            .expect_err("`..` must not walk out of the granted root");
        assert!(err.to_string().contains("outside the session"), "{err}");

        // A sibling whose name merely starts with the root's is not
        // inside it.
        let sibling = root.path().join("inside-secrets").join("key.txt");
        assert!(fs.write_text_file("sess", &sibling, "nope").await.is_err());

        assert!(
            history.calls().is_empty(),
            "a refused write must not reach the snapshot store"
        );
    }

    #[tokio::test]
    async fn granting_no_roots_refuses_every_write_but_still_reads() {
        let root = scratch_dir("snap-noroots");
        let path = root.path().join("note.txt");
        tokio::fs::write(&path, "readable").await.unwrap();

        let fs = SnapshotHostFs::new(Arc::new(RecordingHistory::default()), []);
        assert!(fs.write_text_file("sess", &path, "nope").await.is_err());
        // Reads stay open: confining them would buy nothing, since an
        // agent that wanted to read the disk could just read the disk.
        assert_eq!(
            fs.read_text_file("sess", &path, None, None).await.unwrap(),
            "readable"
        );
    }

    #[test]
    fn containment_treats_a_root_as_inside_itself() {
        let root = PathBuf::from("/repo");
        assert!(contains(&root, &PathBuf::from("/repo")));
        assert!(contains(&root, &PathBuf::from("/repo/src/main.rs")));
        assert!(!contains(&root, &PathBuf::from("/repo-secrets/key")));
        assert!(!contains(&root, &PathBuf::from("/")));
    }

    #[test]
    #[cfg(windows)]
    fn containment_ignores_case_on_windows() {
        // The same directory has many spellings on Windows; treating
        // them as different roots would refuse legitimate writes.
        let root = PathBuf::from(r"C:\Projects\App");
        assert!(contains(
            &root,
            &PathBuf::from(r"c:\projects\app\src\main.rs")
        ));
    }

    #[tokio::test]
    async fn reading_a_missing_file_names_it() {
        let path = std::env::temp_dir().join("rebon-acp-client-does-not-exist.txt");
        let err = DirectHostFs
            .read_text_file("sess", &path, None, None)
            .await
            .expect_err("must not invent contents");
        assert!(err.to_string().contains("rebon-acp-client-does-not-exist"));
    }
}
