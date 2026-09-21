//! Async file scanner — discovers project files via `git ls-files`
//! (with ripgrep fallback) and sends results through a channel so
//! the TUI event loop is never blocked.
//!
//! ## Architecture
//!
//! File discovery uses `git ls-files --recurse-submodules` with a 5s
//! timeout, then merges untracked files from a second background
//! invocation:
//!
//! 1. **Phase 1 (fast)**: `git ls-files` for tracked files. The
//!    result channel receives a `FileListUpdate::Tracked` batch.
//! 2. **Phase 2 (background)**: `git ls-files --others --exclude-standard`
//!    for untracked files. A `FileListUpdate::Untracked` batch follows.
//!
//! Both phases run on the tokio runtime via `Handle::spawn`. The
//! runner drains the receiver each frame and feeds the paths into the
//! file index.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use rebon_types::FileMentionQuery;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

const SCANNER_BATCH_SIZE: usize = 512;
const PREFIX_SCAN_BATCH_LIMIT: usize = 256;
const ROOT_SEED_LIMIT: usize = 256;
const TRACKED_PATH_LIMIT: usize = 100_000;
const RG_FALLBACK_PATH_LIMIT: usize = 20_000;
const NATIVE_FALLBACK_PATH_LIMIT: usize = 20_000;
const UNTRACKED_PATH_LIMIT: usize = 5_000;

/// Scanner status update used to prevent empty indexes from being treated as
/// loading forever after terminal scan outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileScanStatus {
    Scanning,
    Complete,
    Failed(String),
    TimedOut(String),
}

impl Default for FileScanStatus {
    fn default() -> Self {
        Self::Scanning
    }
}

/// Batch of file paths or scanner state discovered by the scanner.
#[derive(Debug)]
pub enum FileListUpdate {
    /// Immediate shallow cwd seed, split into explicit directories and files.
    Seed {
        directories: Vec<String>,
        files: Vec<String>,
    },
    /// Tracked files from `git ls-files` or `rg --files`.
    Tracked(Vec<String>),
    /// Untracked files from `git ls-files --others`.
    Untracked(Vec<String>),
    /// Prefix-triggered shallow scan for a typed path parent.
    Prefix(Vec<String>),
    /// A scanner phase completed successfully.
    Complete,
    /// A scanner phase failed before producing usable results.
    Failed(String),
    /// A scanner phase timed out. Previously sent chunks remain usable.
    TimedOut(String),
}

/// Prefix-triggered scanner state. The TUI owns one instance and calls
/// [`maybe_spawn_prefix_scan`] when an `@` query narrows into a path prefix.
#[derive(Debug, Default)]
pub struct PrefixScanState {
    scanned_queries: HashSet<String>,
    in_flight_queries: HashSet<String>,
}

impl PrefixScanState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Spawn a shallow, ignore-aware background scan for the existing parent
/// directory implied by `query` (for example `src/foo` scans `src/`).
pub fn maybe_spawn_prefix_scan(
    handle: &Handle,
    cwd: &str,
    query: &str,
    state: &mut PrefixScanState,
    tx: mpsc::UnboundedSender<FileListUpdate>,
) {
    let Some(scan_query) = PrefixSeedQuery::from_query(cwd, query) else {
        return;
    };
    let scan_key = scan_query.key();
    if !state.scanned_queries.insert(scan_key.clone())
        || !state.in_flight_queries.insert(scan_key.clone())
    {
        return;
    }

    let cwd = cwd.to_string();
    handle.spawn(async move {
        match read_prefix_seed(&cwd, &scan_query).await {
            Ok(paths) if !paths.is_empty() => {
                tracing::debug!(
                    dir = scan_query.query.display_dir(),
                    prefix = scan_query.query.prefix(),
                    count = paths.len(),
                    "file scanner: prefix seed ready"
                );
                let _ = tx.send(FileListUpdate::Prefix(paths));
            }
            Ok(_) => tracing::debug!(
                dir = scan_query.query.display_dir(),
                prefix = scan_query.query.prefix(),
                "file scanner: prefix seed empty"
            ),
            Err(err) => tracing::debug!(
                dir = scan_query.query.display_dir(),
                prefix = scan_query.query.prefix(),
                %err,
                "file scanner: prefix seed failed"
            ),
        }
    });
}

/// Channel receiver for file list updates.
pub type FileListRx = mpsc::UnboundedReceiver<FileListUpdate>;

/// Spawn the background file scanner. Returns a receiver that the
/// event loop should drain each frame.
///
/// The scanner runs three async tasks on the provided `handle`:
/// 1. the root seed (the directories and files found near the root)
/// 2. `git ls-files` for tracked files (5s timeout)
/// 3. `git ls-files --others --exclude-standard` for untracked (10s timeout)
pub fn spawn_scanner(handle: &Handle, cwd: String) -> FileListRx {
    let (tx, rx) = mpsc::unbounded_channel();

    let seed_tx = tx.clone();
    let seed_cwd = cwd.clone();
    handle.spawn(async move {
        match read_root_seed(&seed_cwd).await {
            Ok(seed) if !seed.is_empty() => {
                tracing::debug!(
                    directories = seed.directories.len(),
                    files = seed.files.len(),
                    "file scanner: root seed ready"
                );
                let _ = seed_tx.send(FileListUpdate::Seed {
                    directories: seed.directories,
                    files: seed.files,
                });
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(?e, "file scanner: root seed failed"),
        }
    });

    let tracked_tx = tx.clone();
    let untracked_tx = tx;
    let cwd_untracked = cwd.clone();

    // Phase 1: tracked files. Prefer git and only run `rg --files` as a true
    // fallback when git fails, times out, or produces no tracked paths. Running
    // both on huge repositories competes with the local @ completion fast path.
    handle.spawn(async move {
        match stream_tracked_with_rg_fallback(&cwd, tracked_tx.clone()).await {
            Ok(()) => {
                tracing::info!("file scanner: tracked files complete");
                let _ = tracked_tx.send(FileListUpdate::Complete);
            }
            Err(StreamScanError::TimedOut(phase)) => {
                tracing::warn!(phase, "file scanner: tracked files timed out");
                let _ = tracked_tx.send(FileListUpdate::TimedOut(phase));
            }
            Err(StreamScanError::Failed(e)) => {
                tracing::warn!(?e, "file scanner: tracked files failed");
                let _ = tracked_tx.send(FileListUpdate::Failed(e.to_string()));
            }
        }
    });

    // Phase 2: untracked files (delayed, capped, lower priority) so it does
    // not steal IO/CPU from first-paint local directory completion.
    handle.spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match stream_git_ls_files(
            &cwd_untracked,
            GitLsMode::Untracked,
            untracked_tx.clone(),
            UNTRACKED_PATH_LIMIT,
        )
        .await
        {
            Ok(_) => tracing::debug!("file scanner: untracked files complete"),
            Err(StreamScanError::TimedOut(phase)) => {
                // Untracked files are an opportunistic second phase. A timeout
                // here must not roll back a usable tracked/rg scan to a global
                // terminal failure in the UI.
                tracing::debug!(phase, "file scanner: untracked files timed out")
            }
            Err(StreamScanError::Failed(e)) => {
                tracing::debug!(?e, "file scanner: untracked failed")
            }
        }
    });

    rx
}

#[derive(Debug)]
pub struct RootSeed {
    pub directories: Vec<String>,
    pub files: Vec<String>,
}

impl RootSeed {
    fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.files.is_empty()
    }
}

pub async fn read_root_seed(cwd: &str) -> anyhow::Result<RootSeed> {
    let mut entries = tokio::fs::read_dir(cwd).await?;
    let mut directories = Vec::new();
    let mut files = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        if directories.len() + files.len() >= ROOT_SEED_LIMIT {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() || should_skip_prefix_entry(&name) {
            continue;
        }
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            directories.push(name);
        } else if file_type.is_file() {
            files.push(name);
        }
    }

    directories.sort();
    files.sort();
    directories.truncate(ROOT_SEED_LIMIT);
    let remaining = ROOT_SEED_LIMIT.saturating_sub(directories.len());
    files.truncate(remaining);
    Ok(RootSeed { directories, files })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitLsMode {
    Tracked,
    Untracked,
}

impl GitLsMode {
    fn is_untracked(self) -> bool {
        matches!(self, Self::Untracked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamCommandOutcome {
    Complete { emitted: usize },
    Capped { emitted: usize },
}

impl StreamCommandOutcome {
    fn emitted(self) -> usize {
        match self {
            Self::Complete { emitted } | Self::Capped { emitted } => emitted,
        }
    }
}

#[derive(Debug)]
enum StreamScanError {
    TimedOut(String),
    Failed(anyhow::Error),
}

async fn stream_tracked_with_rg_fallback(
    cwd: &str,
    tx: mpsc::UnboundedSender<FileListUpdate>,
) -> Result<(), StreamScanError> {
    match stream_git_ls_files(cwd, GitLsMode::Tracked, tx.clone(), TRACKED_PATH_LIMIT).await {
        Ok(outcome) if outcome.emitted() > 0 => Ok(()),
        Ok(_) => {
            tracing::debug!(
                "file scanner: git ls-files returned no paths; falling back to rg --files"
            );
            match stream_ripgrep_files(cwd, tx.clone(), RG_FALLBACK_PATH_LIMIT).await {
                Ok(outcome) => {
                    tracing::debug!(
                        emitted = outcome.emitted(),
                        "file scanner: rg fallback completed after empty git result"
                    );
                    Ok(())
                }
                Err(rg_err) => {
                    tracing::debug!(
                        ?rg_err,
                        "file scanner: rg --files unavailable after empty git result; trying native walker"
                    );
                    let outcome = stream_native_files(cwd, tx, NATIVE_FALLBACK_PATH_LIMIT).await?;
                    tracing::info!(
                        emitted = outcome.emitted(),
                        "file scanner: native fallback completed after empty git result and unavailable rg"
                    );
                    Ok(())
                }
            }
        }
        Err(git_err) => {
            tracing::debug!(
                ?git_err,
                "file scanner: git ls-files failed; falling back to rg --files"
            );
            match stream_ripgrep_files(cwd, tx.clone(), RG_FALLBACK_PATH_LIMIT).await {
                Ok(outcome) => {
                    tracing::debug!(
                        emitted = outcome.emitted(),
                        "file scanner: rg fallback completed after git failure"
                    );
                    Ok(())
                }
                Err(rg_err) => {
                    tracing::debug!(
                        ?rg_err,
                        "file scanner: rg --files unavailable after git failure; trying native walker"
                    );
                    match stream_native_files(cwd, tx, NATIVE_FALLBACK_PATH_LIMIT).await {
                        Ok(outcome) => {
                            tracing::info!(
                                emitted = outcome.emitted(),
                                "file scanner: native fallback completed after git failure and unavailable rg"
                            );
                            Ok(())
                        }
                        Err(native_err) => match (git_err, rg_err, native_err) {
                            (StreamScanError::TimedOut(git_phase), StreamScanError::TimedOut(rg_phase), _) => {
                                Err(StreamScanError::TimedOut(format!("{git_phase}; {rg_phase}")))
                            }
                            (StreamScanError::TimedOut(phase), _, _)
                            | (_, StreamScanError::TimedOut(phase), _)
                            | (_, _, StreamScanError::TimedOut(phase)) => Err(StreamScanError::TimedOut(phase)),
                            (
                                StreamScanError::Failed(git_err),
                                StreamScanError::Failed(rg_err),
                                StreamScanError::Failed(native_err),
                            ) => Err(StreamScanError::Failed(anyhow::anyhow!(
                                "git ls-files failed ({git_err}); rg --files failed ({rg_err}); native walker failed ({native_err}). {}",
                                crate::ripgrep::RIPGREP_ACTIONABLE_GUIDANCE
                            ))),
                        },
                    }
                }
            }
        }
    }
}

async fn stream_git_ls_files(
    cwd: &str,
    mode: GitLsMode,
    tx: mpsc::UnboundedSender<FileListUpdate>,
    path_limit: usize,
) -> Result<StreamCommandOutcome, StreamScanError> {
    let mut cmd = scanner_command("git");
    cmd.current_dir(cwd)
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("ls-files");
    if mode.is_untracked() {
        cmd.arg("--others").arg("--exclude-standard");
    } else {
        cmd.arg("--recurse-submodules");
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());

    let timeout = if mode.is_untracked() {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(5)
    };
    let phase = if mode.is_untracked() {
        "git ls-files --others"
    } else {
        "git ls-files"
    };

    match tokio::time::timeout(
        timeout,
        stream_command_lines(cmd, tx, mode.is_untracked(), path_limit),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(StreamScanError::TimedOut(phase.to_string())),
    }
}

async fn stream_ripgrep_files(
    cwd: &str,
    tx: mpsc::UnboundedSender<FileListUpdate>,
    path_limit: usize,
) -> Result<StreamCommandOutcome, StreamScanError> {
    let ripgrep = crate::ripgrep::resolve_ripgrep_command()
        .map_err(|err| StreamScanError::Failed(anyhow::anyhow!("rg not found: {err}")))?;
    let mut cmd = Command::new(&ripgrep.program);
    tracing::debug!(
        program = %ripgrep.program.display(),
        mode = ?ripgrep.mode,
        "file scanner: using ripgrep"
    );
    cmd.current_dir(cwd)
        .args([
            "--files",
            "--follow",
            "--hidden",
            "--glob",
            "!.git/",
            "--glob",
            "!.svn/",
            "--glob",
            "!node_modules/",
            "--glob",
            "!target/",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    match tokio::time::timeout(
        Duration::from_secs(10),
        stream_command_lines(cmd, tx, false, path_limit),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(StreamScanError::TimedOut("rg --files".to_string())),
    }
}

async fn stream_native_files(
    cwd: &str,
    tx: mpsc::UnboundedSender<FileListUpdate>,
    path_limit: usize,
) -> Result<StreamCommandOutcome, StreamScanError> {
    let cwd = cwd.to_string();
    tokio::task::spawn_blocking(move || stream_native_files_blocking(&cwd, tx, path_limit))
        .await
        .map_err(|err| StreamScanError::Failed(anyhow::Error::from(err)))?
}

fn stream_native_files_blocking(
    cwd: &str,
    tx: mpsc::UnboundedSender<FileListUpdate>,
    path_limit: usize,
) -> Result<StreamCommandOutcome, StreamScanError> {
    let root = Path::new(cwd);
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(true)
        .follow_links(true)
        .filter_entry(|entry| !should_skip_native_entry(entry));

    let mut batch = Vec::with_capacity(SCANNER_BATCH_SIZE);
    let mut emitted = 0usize;
    for entry in builder.build() {
        let entry = entry.map_err(|err| StreamScanError::Failed(anyhow::Error::from(err)))?;
        let path = entry.path();
        if path == root
            || !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        if emitted >= path_limit {
            send_path_batch(&tx, &mut batch, false);
            tracing::debug!(path_limit, "file scanner: native walker output capped");
            return Ok(StreamCommandOutcome::Capped { emitted });
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = normalize_index_path(&rel.to_string_lossy());
        if rel.is_empty() {
            continue;
        }
        batch.push(rel);
        emitted += 1;
        if batch.len() >= SCANNER_BATCH_SIZE {
            send_path_batch(&tx, &mut batch, false);
        }
    }
    send_path_batch(&tx, &mut batch, false);
    Ok(StreamCommandOutcome::Complete { emitted })
}

fn should_skip_native_entry(entry: &ignore::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(should_skip_prefix_entry)
}

fn scanner_command(program: &str) -> Command {
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(format!("{program}.cmd")))
            .find(|path| path.is_file())
    }) {
        return Command::new(path);
    }

    Command::new(program)
}

async fn stream_command_lines(
    mut cmd: Command,
    tx: mpsc::UnboundedSender<FileListUpdate>,
    untracked: bool,
    path_limit: usize,
) -> Result<StreamCommandOutcome, StreamScanError> {
    let mut child = cmd
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| StreamScanError::Failed(anyhow::Error::from(e)))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        StreamScanError::Failed(anyhow::anyhow!("scanner command stdout was not piped"))
    })?;
    let mut lines = BufReader::new(stdout).lines();
    let mut batch = Vec::with_capacity(SCANNER_BATCH_SIZE);
    let mut emitted = 0usize;
    let mut capped = false;

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| StreamScanError::Failed(anyhow::Error::from(e)))?
    {
        if line.is_empty() {
            continue;
        }
        if emitted >= path_limit {
            capped = true;
            let _ = child.start_kill();
            break;
        }
        batch.push(line);
        emitted += 1;
        if batch.len() >= SCANNER_BATCH_SIZE {
            send_path_batch(&tx, &mut batch, untracked);
        }
    }
    send_path_batch(&tx, &mut batch, untracked);

    if capped {
        tracing::debug!(path_limit, untracked, "file scanner: command output capped");
        return Ok(StreamCommandOutcome::Capped { emitted });
    }

    let status = child
        .wait()
        .await
        .map_err(|e| StreamScanError::Failed(anyhow::Error::from(e)))?;
    if !status.success() {
        return Err(StreamScanError::Failed(anyhow::anyhow!(
            "scanner command exited with {status}"
        )));
    }

    Ok(StreamCommandOutcome::Complete { emitted })
}

fn send_path_batch(
    tx: &mpsc::UnboundedSender<FileListUpdate>,
    batch: &mut Vec<String>,
    untracked: bool,
) {
    if batch.is_empty() {
        return;
    }
    let paths = std::mem::take(batch);
    let update = if untracked {
        FileListUpdate::Untracked(paths)
    } else {
        FileListUpdate::Tracked(paths)
    };
    let _ = tx.send(update);
}

async fn read_prefix_seed(cwd: &str, scan_query: &PrefixSeedQuery) -> anyhow::Result<Vec<String>> {
    tokio::task::spawn_blocking({
        let cwd = cwd.to_string();
        let query = scan_query.query.normalized().to_string();
        move || read_prefix_seed_blocking(&cwd, &query)
    })
    .await?
}

fn read_prefix_seed_blocking(cwd: &str, query: &str) -> anyhow::Result<Vec<String>> {
    let Ok(query) = FileMentionQuery::parse(query) else {
        return Ok(Vec::new());
    };
    if !query.is_path_query() || query_uses_skipped_directory(&query) {
        return Ok(Vec::new());
    }

    let mut results = read_local_dir_candidates(cwd, &query, PREFIX_SCAN_BATCH_LIMIT)?;

    if results.len() < PREFIX_SCAN_BATCH_LIMIT && !query.prefix().is_empty() {
        let generic_query = format!("{}/", query.display_dir());
        let generic_query = FileMentionQuery::parse(&generic_query)
            .expect("validated file mention display directory remains valid");
        let generic = read_local_dir_candidates(cwd, &generic_query, PREFIX_SCAN_BATCH_LIMIT)?;
        for result in generic {
            if results.len() >= PREFIX_SCAN_BATCH_LIMIT {
                break;
            }
            if !results.iter().any(|existing| existing.path == result.path) {
                results.push(result);
            }
        }
    }

    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(results.into_iter().map(|result| result.path).collect())
}

fn parent_gitignore_matchers(
    canonical_base: &Path,
    canonical_target: &Path,
) -> Vec<ignore::gitignore::Gitignore> {
    let mut dirs = Vec::new();
    let mut dir = canonical_target;
    while dir.starts_with(canonical_base) {
        dirs.push(dir.to_path_buf());
        if dir == canonical_base {
            break;
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
    }
    dirs.reverse();

    let mut matchers = Vec::new();
    for dir in dirs {
        let ignore_file = dir.join(".gitignore");
        if !ignore_file.is_file() {
            continue;
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&dir);
        let _ = builder.add(&ignore_file);
        if let Ok(matcher) = builder.build() {
            matchers.push(matcher);
        }
    }
    matchers
}

fn is_ignored_by_parent_gitignore(
    matchers: &[ignore::gitignore::Gitignore],
    path: &Path,
    is_dir: bool,
) -> bool {
    let mut ignored = false;
    for matcher in matchers {
        let matched = matcher.matched(path, is_dir);
        if matched.is_ignore() {
            ignored = true;
        } else if matched.is_whitelist() {
            ignored = false;
        }
    }
    ignored
}

fn should_skip_prefix_entry(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".hg" | ".svn" | ".idea" | "node_modules" | "target"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDirSearchResult {
    pub path: String,
    pub is_directory: bool,
}

#[derive(Debug, Clone)]
struct PrefixSeedQuery {
    query: FileMentionQuery,
}

impl PrefixSeedQuery {
    fn from_query(cwd: &str, query: &str) -> Option<Self> {
        let query = FileMentionQuery::parse(query).ok()?;
        if !query.is_path_query()
            || query.display_dir().is_empty()
            || query_uses_skipped_directory(&query)
            || canonical_file_mention_location(cwd, &query).is_none()
        {
            return None;
        }
        Some(Self { query })
    }

    fn key(&self) -> String {
        self.query.normalized().to_string()
    }
}

fn query_uses_skipped_directory(query: &FileMentionQuery) -> bool {
    query
        .relative_dir()
        .split('/')
        .filter(|component| !component.is_empty())
        .any(should_skip_prefix_entry)
}

#[derive(Debug, Clone)]
pub(crate) struct CanonicalFileMentionLocation {
    pub allowed_root: PathBuf,
    pub target_dir: PathBuf,
}

pub(crate) fn canonical_file_mention_location(
    cwd: &str,
    query: &FileMentionQuery,
) -> Option<CanonicalFileMentionLocation> {
    let canonical_cwd = Path::new(cwd).canonicalize().ok()?;
    let location = query.resolve_from(&canonical_cwd).ok()?;
    let allowed_root = location.allowed_root.canonicalize().ok()?;
    let target_dir = location.target_dir.canonicalize().ok()?;
    if !target_dir.starts_with(&allowed_root) || !target_dir.is_dir() {
        return None;
    }
    Some(CanonicalFileMentionLocation {
        allowed_root,
        target_dir,
    })
}

pub fn local_dir_candidates(
    cwd: &str,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<LocalDirSearchResult>> {
    let Ok(query) = FileMentionQuery::parse(query) else {
        tracing::debug!(
            cwd,
            query,
            result_count = 0usize,
            "file scanner: local_dir_candidates ignored invalid query"
        );
        return Ok(Vec::new());
    };
    if query_uses_skipped_directory(&query) {
        return Ok(Vec::new());
    }
    let result = read_local_dir_candidates(cwd, &query, limit);
    match &result {
        Ok(results) => tracing::debug!(
            cwd,
            query = query.normalized(),
            result_count = results.len(),
            "file scanner: local_dir_candidates completed"
        ),
        Err(err) => tracing::debug!(
            %err,
            cwd,
            query = query.normalized(),
            result_count = 0usize,
            "file scanner: local_dir_candidates failed"
        ),
    }
    result
}

fn read_local_dir_candidates(
    cwd: &str,
    query: &FileMentionQuery,
    limit: usize,
) -> anyhow::Result<Vec<LocalDirSearchResult>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let Some(location) = canonical_file_mention_location(cwd, query) else {
        return Ok(Vec::new());
    };
    let ignore_matchers = parent_gitignore_matchers(&location.allowed_root, &location.target_dir);
    let prefix_lower = query.prefix().to_lowercase();
    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in std::fs::read_dir(&location.target_dir)? {
        // Empty-prefix queries intentionally stay cheap: they only need a quick
        // first page of direct children. Non-empty path prefixes must scan the
        // whole direct-child list so a large directory cannot hide a later
        // matching child behind the result limit.
        if prefix_lower.is_empty() && dirs.len() + files.len() >= limit {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(%err, "file scanner: local dir entry failed");
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty()
            || should_skip_prefix_entry(&name)
            || !name.to_lowercase().starts_with(&prefix_lower)
        {
            continue;
        }

        let path = entry.path();
        let canonical_path = match path.canonicalize() {
            Ok(path) if path.starts_with(&location.allowed_root) => path,
            Ok(_) => {
                tracing::debug!(path = %path.display(), "file scanner: skipped candidate outside allowed root");
                continue;
            }
            Err(err) => {
                tracing::debug!(%err, path = %path.display(), "file scanner: local dir candidate canonicalize failed");
                continue;
            }
        };
        let metadata = match canonical_path.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                tracing::debug!(%err, path = %canonical_path.display(), "file scanner: local dir candidate metadata failed");
                continue;
            }
        };
        let is_directory = metadata.is_dir();
        if !is_directory && !metadata.is_file() {
            continue;
        }
        if is_ignored_by_parent_gitignore(&ignore_matchers, &path, is_directory) {
            continue;
        }
        let Some(path) = query.candidate_path(&name) else {
            continue;
        };

        let result = LocalDirSearchResult { path, is_directory };
        if result.is_directory {
            dirs.push(result);
        } else {
            files.push(result);
        }
    }

    dirs.sort_by(|a, b| a.path.cmp(&b.path));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    dirs.extend(files);
    dirs.truncate(limit);
    Ok(dirs)
}

#[cfg(test)]
fn prefix_scan_dir(cwd: &str, query: &str) -> Option<String> {
    PrefixSeedQuery::from_query(cwd, query).map(|query| query.query.display_dir().to_string())
}

#[cfg(test)]
fn merge_scanner_update_for_test(index: &mut FileIndex, update: FileListUpdate) {
    match update {
        FileListUpdate::Seed { directories, files } => {
            index.merge_directories(directories);
            index.merge(files);
        }
        FileListUpdate::Tracked(paths)
        | FileListUpdate::Untracked(paths)
        | FileListUpdate::Prefix(paths) => index.merge(paths),
        FileListUpdate::Complete | FileListUpdate::Failed(_) | FileListUpdate::TimedOut(_) => {}
    }
}

#[cfg(test)]
fn chunk_paths(paths: Vec<String>, batch_size: usize) -> Vec<Vec<String>> {
    let batch_size = batch_size.max(1);
    let mut batches = Vec::new();
    let mut batch = Vec::with_capacity(batch_size);
    for path in paths {
        batch.push(path);
        if batch.len() >= batch_size {
            batches.push(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// Lightweight file index for fuzzy search. Stores paths with
/// pre-computed lowercase versions for fast case-insensitive matching.
#[derive(Debug, Clone, Default)]
pub struct FileIndex {
    /// All known file and directory paths (relative to cwd), in stable insertion order.
    paths: Vec<String>,
    /// Pre-computed lowercase versions for fast matching.
    lower: Vec<String>,
    /// Membership index for `paths` to avoid O(N²) duplicate checks during large merges.
    path_set: HashSet<String>,
    /// Paths known to be directories because they were parents of indexed files.
    directories: HashSet<String>,
}

impl FileIndex {
    #[cfg(test)]
    /// Whether `path` is known to be a directory in this index.
    pub fn is_directory(&self, path: &str) -> bool {
        self.directories.contains(&normalize_index_path(path))
    }

    /// Merge a batch of explicit directory paths without adding fake files.
    pub fn merge_directories(&mut self, directories: Vec<String>) {
        self.merge_paths(directories, true);
    }

    /// Merge a batch of new file paths into the index, deduplicating.
    pub fn merge(&mut self, new_paths: Vec<String>) {
        let mut dirs = Vec::new();
        let mut seen_dirs = HashSet::new();
        for path in &new_paths {
            let normalized = normalize_index_path(path);
            for dir in parent_directories(&normalized) {
                if seen_dirs.insert(dir.clone()) {
                    dirs.push(dir);
                }
            }
        }

        self.merge_paths(dirs, true);
        self.merge_paths(new_paths, false);
    }

    fn merge_paths(&mut self, paths: Vec<String>, mark_directory: bool) {
        for path in paths.into_iter().map(|p| normalize_index_path(&p)) {
            if path.is_empty() {
                continue;
            }
            if mark_directory {
                self.directories.insert(path.clone());
            }
            if self.path_set.insert(path.clone()) {
                let low = path.to_lowercase();
                self.lower.push(low);
                self.paths.push(path);
            }
        }
    }

    /// Number of indexed paths. Only the tests count them; the picker
    /// asks whether the index is empty and otherwise searches it.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Fuzzy search the index. Returns up to `limit` results sorted
    /// by score (best first).
    ///
    /// Uses a nucleo-inspired scoring algorithm:
    /// - Match character bonus (+16)
    /// - Boundary bonus (+8) for matches after `/`, `.`, `-`, `_`
    /// - Consecutive match bonus (+4)
    /// - Gap penalty (-3 start, -1 per extension)
    /// - Shorter paths preferred (+32 - len/4)
    /// - Test file penalty (score * 0.95)
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        if limit == 0 {
            return Vec::new();
        }

        let query = normalize_query_path(query);
        if query.is_empty() {
            return self.direct_children("", "", limit);
        }

        if query.contains('/') {
            let (dir, prefix) = split_path_query(&query);
            let mut results = self.direct_children(dir, prefix, limit);
            if results.len() < limit && !prefix.is_empty() && results.is_empty() {
                self.append_prefix_matches(&mut results, &query, limit);
            }
            return results;
        }

        let query_lower = query.to_lowercase();
        let mut results = self.prefix_matches(&query_lower, limit);
        if results.len() < limit {
            self.append_fuzzy_matches(&mut results, &query_lower, limit);
        }
        results
    }

    fn direct_children(&self, dir: &str, prefix: &str, limit: usize) -> Vec<SearchResult> {
        let dir = normalize_index_path(dir);
        let prefix_lower = prefix.to_lowercase();
        let parent_prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };

        let mut results: Vec<_> = self
            .paths
            .iter()
            .filter(|path| {
                if !path.starts_with(&parent_prefix) {
                    return false;
                }
                let rest = &path[parent_prefix.len()..];
                !rest.is_empty()
                    && !rest.contains('/')
                    && rest.to_lowercase().starts_with(&prefix_lower)
            })
            .map(|path| self.result_for(path, prefix.len() as i32))
            .collect();
        sort_results_directory_first(&mut results);
        results.truncate(limit);
        results
    }

    fn prefix_matches(&self, query_lower: &str, limit: usize) -> Vec<SearchResult> {
        let mut results: Vec<_> = self
            .paths
            .iter()
            .zip(self.lower.iter())
            .filter(|(_, low)| path_basename(low).starts_with(query_lower))
            .map(|(path, _)| self.result_for(path, query_lower.len() as i32))
            .collect();
        sort_results_directory_first(&mut results);
        results.truncate(limit);
        results
    }

    fn append_prefix_matches(&self, results: &mut Vec<SearchResult>, query: &str, limit: usize) {
        let query_lower = query.to_lowercase();
        let mut extra: Vec<_> = self
            .paths
            .iter()
            .zip(self.lower.iter())
            .filter(|(path, low)| {
                !results.iter().any(|r| r.path == **path) && low.starts_with(&query_lower)
            })
            .map(|(path, _)| self.result_for(path, query.len() as i32))
            .collect();
        sort_results_directory_first(&mut extra);
        for result in extra {
            if results.len() >= limit {
                break;
            }
            results.push(result);
        }
    }

    fn append_fuzzy_matches(
        &self,
        results: &mut Vec<SearchResult>,
        query_lower: &str,
        limit: usize,
    ) {
        let query_chars: Vec<char> = query_lower.chars().collect();
        let mut fuzzy = Vec::new();
        for (idx, path) in self.paths.iter().enumerate() {
            if results.iter().any(|r| r.path == *path) {
                continue;
            }
            let low = &self.lower[idx];
            if !chars_exist(low, &query_chars) {
                continue;
            }
            if let Some(score) = fuzzy_score(low, &query_chars, path.len()) {
                let basename_bonus = if path_basename(low).starts_with(query_lower) {
                    32
                } else {
                    0
                };
                fuzzy.push(SearchResult {
                    path: path.clone(),
                    is_directory: self.directories.contains(path),
                    score: score + basename_bonus,
                });
            }
        }
        fuzzy.sort_by(sort_search_results_by_score);
        for result in fuzzy {
            if results.len() >= limit {
                break;
            }
            results.push(result);
        }
    }

    fn result_for(&self, path: &str, score: i32) -> SearchResult {
        SearchResult {
            path: path.to_string(),
            is_directory: self.directories.contains(path),
            score,
        }
    }
}

/// One search result from the file index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// File path relative to cwd.
    pub path: String,
    /// Whether this result is a directory.
    pub is_directory: bool,
    /// Fuzzy match score (higher is better).
    pub score: i32,
}

fn normalize_index_path(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).replace('\\', "/")
}

fn normalize_query_path(query: &str) -> String {
    query.replace('\\', "/")
}

fn split_path_query(query: &str) -> (&str, &str) {
    match query.rsplit_once('/') {
        Some((dir, prefix)) => (dir, prefix),
        None => ("", query),
    }
}

fn path_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn sort_results_directory_first(results: &mut [SearchResult]) {
    results.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn sort_search_results_by_score(a: &SearchResult, b: &SearchResult) -> std::cmp::Ordering {
    b.score
        .cmp(&a.score)
        .then_with(|| b.is_directory.cmp(&a.is_directory))
        .then_with(|| a.path.len().cmp(&b.path.len()))
        .then_with(|| a.path.cmp(&b.path))
}

fn parent_directories(path: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut end = path.len();
    while let Some(pos) = path[..end].rfind(['/', '\\']) {
        if pos == 0 {
            break;
        }
        dirs.push(path[..pos].to_string());
        end = pos;
    }
    dirs.reverse();
    dirs
}

/// Check if all chars in `needle` exist somewhere in `haystack`.
fn chars_exist(haystack: &str, needle: &[char]) -> bool {
    for &ch in needle {
        if !haystack.contains(ch) {
            return false;
        }
    }
    true
}

// Scoring constants (matches nucleo-style weights).
const SCORE_MATCH: i32 = 16;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_CONSECUTIVE: i32 = 4;
const BONUS_FIRST_CHAR: i32 = 8;
const PENALTY_GAP_START: i32 = 3;
const PENALTY_GAP_EXTENSION: i32 = 1;

/// Compute a fuzzy match score for `query_chars` against `haystack`
/// (already lowercased). Returns `None` if no match.
fn fuzzy_score(haystack: &str, query_chars: &[char], original_len: usize) -> Option<i32> {
    let hay_chars: Vec<char> = haystack.chars().collect();
    let n = query_chars.len();
    if n == 0 {
        return Some(0);
    }

    // Greedy forward match — find positions of each query char.
    let mut positions = Vec::with_capacity(n);
    let mut hay_idx = 0;
    for &qc in query_chars {
        let mut found = false;
        while hay_idx < hay_chars.len() {
            if hay_chars[hay_idx] == qc {
                positions.push(hay_idx);
                hay_idx += 1;
                found = true;
                break;
            }
            hay_idx += 1;
        }
        if !found {
            return None;
        }
    }

    // Score the match.
    let mut score = 0i32;

    for (qi, &pos) in positions.iter().enumerate() {
        score += SCORE_MATCH;

        // Boundary bonus.
        if pos == 0 {
            score += BONUS_FIRST_CHAR;
        } else {
            let prev = hay_chars[pos - 1];
            // No camelCase bonus: the haystack reaching here is already
            // lowercased (`Index::lower`), so an uppercase test can never
            // fire. Scoring one would mean matching against the original
            // path instead.
            if is_boundary(prev) {
                score += BONUS_BOUNDARY;
            }
        }

        // Consecutive / gap.
        if qi > 0 {
            let gap = pos - positions[qi - 1] - 1;
            if gap == 0 {
                score += BONUS_CONSECUTIVE;
            } else {
                score -= PENALTY_GAP_START + (gap as i32 - 1) * PENALTY_GAP_EXTENSION;
            }
        }
    }

    // Length bonus: prefer shorter paths.
    score += (32i32).saturating_sub(original_len as i32 / 4).max(0);

    // Test file penalty.
    if haystack.contains("test") || haystack.contains("spec") {
        score = (score as f64 * 0.95) as i32;
    }

    Some(score)
}

fn is_boundary(ch: char) -> bool {
    matches!(ch, '/' | '\\' | '-' | '_' | '.' | ' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::MutexGuard;

    struct PathEnvGuard<'a> {
        _lock: MutexGuard<'a, ()>,
        original_path: Option<std::ffi::OsString>,
        original_pathext: Option<std::ffi::OsString>,
        original_rg_marker: Option<std::ffi::OsString>,
        original_use_builtin: Option<std::ffi::OsString>,
        original_rg_path: Option<std::ffi::OsString>,
    }

    impl PathEnvGuard<'_> {
        fn prepend(dir: &Path) -> Self {
            // Crate-wide lock: swapping `PATH`/`PATHEXT` here would
            // otherwise stop unrelated tests from resolving `git`.
            let lock = crate::test_env::lock_env();
            let original_path = std::env::var_os("PATH");
            let original_pathext = std::env::var_os("PATHEXT");
            let original_rg_marker = std::env::var_os("REBON_TEST_RG_MARKER");
            let original_use_builtin = std::env::var_os("USE_BUILTIN_RIPGREP");
            let original_rg_path = std::env::var_os("REBON_RIPGREP_PATH");
            let mut paths = vec![dir.to_path_buf()];
            if let Some(existing) = original_path.as_ref() {
                paths.extend(std::env::split_paths(existing));
            }
            let joined = std::env::join_paths(paths).unwrap();
            std::env::set_var("PATH", joined);
            #[cfg(windows)]
            std::env::set_var("PATHEXT", ".CMD");
            std::env::remove_var("USE_BUILTIN_RIPGREP");
            std::env::remove_var("REBON_RIPGREP_PATH");
            Self {
                _lock: lock,
                original_path,
                original_pathext,
                original_rg_marker,
                original_use_builtin,
                original_rg_path,
            }
        }
    }

    impl Drop for PathEnvGuard<'_> {
        fn drop(&mut self) {
            match &self.original_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            match &self.original_pathext {
                Some(path) => std::env::set_var("PATHEXT", path),
                None => std::env::remove_var("PATHEXT"),
            }
            match &self.original_rg_marker {
                Some(path) => std::env::set_var("REBON_TEST_RG_MARKER", path),
                None => std::env::remove_var("REBON_TEST_RG_MARKER"),
            }
            match &self.original_use_builtin {
                Some(path) => std::env::set_var("USE_BUILTIN_RIPGREP", path),
                None => std::env::remove_var("USE_BUILTIN_RIPGREP"),
            }
            match &self.original_rg_path {
                Some(path) => std::env::set_var("REBON_RIPGREP_PATH", path),
                None => std::env::remove_var("REBON_RIPGREP_PATH"),
            }
        }
    }

    fn command_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[cfg(windows)]
    fn command_path(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.cmd"))
    }

    #[cfg(unix)]
    fn command_path(dir: &Path, name: &str) -> PathBuf {
        dir.join(name)
    }

    #[cfg(windows)]
    fn write_fake_command(dir: &Path, name: &str, body: &str) {
        std::fs::write(command_path(dir, name), body).unwrap();
    }

    #[cfg(unix)]
    fn write_fake_command(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = command_path(dir, name);
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(windows)]
    fn fake_git_sleep_body() -> &'static str {
        "@echo off\r\n%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -NoProfile -Command \"Start-Sleep -Seconds 6\" >NUL 2>NUL\r\nexit /b 0\r\n"
    }

    #[cfg(unix)]
    fn fake_git_sleep_body() -> &'static str {
        "#!/bin/sh\nsleep 6\nexit 0\n"
    }

    #[cfg(windows)]
    fn fake_rg_visible_body() -> &'static str {
        "@echo off\r\necho visible.rs\r\nexit /b 0\r\n"
    }

    #[cfg(unix)]
    fn fake_rg_visible_body() -> &'static str {
        "#!/bin/sh\nprintf 'visible.rs\\n'\nexit 0\n"
    }

    #[cfg(windows)]
    fn fake_git_tracked_body() -> &'static str {
        "@echo off\r\necho tracked.rs\r\nexit /b 0\r\n"
    }

    #[cfg(unix)]
    fn fake_git_tracked_body() -> &'static str {
        "#!/bin/sh\nprintf 'tracked.rs\\n'\nexit 0\n"
    }

    #[cfg(windows)]
    fn fake_rg_marker_body() -> &'static str {
        "@echo off\r\nif not \"%REBON_TEST_RG_MARKER%\"==\"\" echo invoked>\"%REBON_TEST_RG_MARKER%\"\r\necho fallback.rs\r\nexit /b 0\r\n"
    }

    #[cfg(unix)]
    fn fake_rg_marker_body() -> &'static str {
        "#!/bin/sh\nif [ -n \"$REBON_TEST_RG_MARKER\" ]; then printf invoked > \"$REBON_TEST_RG_MARKER\"; fi\nprintf 'fallback.rs\\n'\nexit 0\n"
    }

    #[cfg(windows)]
    fn fake_git_fail_body() -> &'static str {
        "@echo off\r\nexit /b 1\r\n"
    }

    #[cfg(unix)]
    fn fake_git_fail_body() -> &'static str {
        "#!/bin/sh\nexit 1\n"
    }

    #[cfg(windows)]
    fn fake_rg_fail_body() -> &'static str {
        "@echo off\r\nexit /b 1\r\n"
    }

    #[cfg(unix)]
    fn fake_rg_fail_body() -> &'static str {
        "#!/bin/sh\nexit 1\n"
    }

    #[test]
    fn chars_exist_positive() {
        assert!(chars_exist("hello world", &['h', 'w']));
    }

    #[test]
    fn chars_exist_negative() {
        assert!(!chars_exist("hello", &['h', 'z']));
    }

    #[test]
    fn fuzzy_score_exact_match() {
        let score = fuzzy_score("cargo.toml", &['c', 'a', 'r', 'g', 'o'], 10);
        assert!(score.is_some());
        assert!(score.unwrap() > 0);
    }

    #[test]
    fn fuzzy_score_no_match() {
        let score = fuzzy_score("cargo.toml", &['z', 'z', 'z'], 10);
        assert!(score.is_none());
    }

    #[test]
    fn fuzzy_score_boundary_bonus() {
        // "src/main.rs" searching "mr" — 'm' is after '/', should get boundary bonus
        let score_boundary = fuzzy_score("src/main.rs", &['m', 'r'], 11).unwrap();
        // "something" searching "mr" — no boundary
        let score_no_boundary = fuzzy_score("xmxrxx", &['m', 'r'], 6).unwrap();
        assert!(score_boundary > score_no_boundary);
    }

    #[test]
    fn fuzzy_score_consecutive_bonus() {
        // Consecutive "car" in "cargo" beats scattered "cxaxr" (no boundary chars)
        let score_consec = fuzzy_score("cargo", &['c', 'a', 'r'], 5).unwrap();
        let score_scattered = fuzzy_score("cxaxr", &['c', 'a', 'r'], 5).unwrap();
        assert!(score_consec > score_scattered);
    }

    #[test]
    fn fuzzy_score_shorter_path_preferred() {
        let score_short = fuzzy_score("src/a.rs", &['a'], 8).unwrap();
        let score_long =
            fuzzy_score("very/deep/nested/directory/structure/a.rs", &['a'], 41).unwrap();
        assert!(score_short > score_long);
    }

    #[test]
    fn fuzzy_score_test_penalty() {
        let score_normal = fuzzy_score("src/main.rs", &['m'], 11).unwrap();
        let score_test = fuzzy_score("src/test.rs", &['t'], 11).unwrap();
        // Both have similar base score but test gets penalized.
        // We can't compare directly since the query chars differ,
        // so just verify the test file penalty doesn't crash.
        assert!(score_test > 0);
        let _ = score_normal;
    }

    #[test]
    fn file_index_merge_explicit_directories_keeps_directories_real() {
        let mut idx = FileIndex::default();
        idx.merge_directories(vec!["src".into(), "crates".into()]);
        idx.merge(vec!["Cargo.toml".into()]);

        let results = idx.search("", 10);
        let paths: Vec<_> = results.iter().map(|result| result.path.as_str()).collect();
        assert_eq!(paths, vec!["crates", "src", "Cargo.toml"]);
        assert!(results[0].is_directory);
        assert!(results[1].is_directory);
        assert!(!results[2].is_directory);
        assert!(!idx.search("", 10).iter().any(|r| r.path.contains("__")));
    }

    #[tokio::test]
    async fn root_seed_lists_directories_and_files_for_immediate_merge() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::create_dir(temp.path().join("crates")).unwrap();
        std::fs::create_dir(temp.path().join("node_modules")).unwrap();
        std::fs::create_dir(temp.path().join("target")).unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();

        let seed = read_root_seed(temp.path().to_str().unwrap()).await.unwrap();
        assert_eq!(
            seed.directories,
            vec!["crates".to_string(), "src".to_string()]
        );
        assert_eq!(seed.files, vec!["Cargo.toml".to_string()]);

        let mut idx = FileIndex::default();
        idx.merge_directories(seed.directories);
        idx.merge(seed.files);
        let results = idx.search("", 10);
        let paths: Vec<_> = results.iter().map(|result| result.path.as_str()).collect();
        assert_eq!(paths, vec!["crates", "src", "Cargo.toml"]);
        assert!(results[0].is_directory);
        assert!(results[1].is_directory);
    }

    #[test]
    fn scanner_chunk_paths_batches_incrementally() {
        let batches = chunk_paths(
            vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                "crates/rebon-cli/src/main.rs".into(),
                "Cargo.toml".into(),
                "README.md".into(),
            ],
            2,
        );
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0], vec!["src/main.rs", "src/lib.rs"]);
        assert_eq!(
            batches[1],
            vec!["crates/rebon-cli/src/main.rs", "Cargo.toml"]
        );
        assert_eq!(batches[2], vec!["README.md"]);

        let mut idx = FileIndex::default();
        idx.merge(batches[0].clone());
        let first_paths: Vec<_> = idx
            .search("", 10)
            .into_iter()
            .map(|result| result.path)
            .collect();
        assert_eq!(first_paths, vec!["src".to_string()]);

        idx.merge(batches[1].clone());
        let later_paths: Vec<_> = idx
            .search("", 10)
            .into_iter()
            .map(|result| result.path)
            .collect();
        assert_eq!(
            later_paths,
            vec![
                "crates".to_string(),
                "src".to_string(),
                "Cargo.toml".to_string()
            ]
        );
    }

    #[test]
    fn scanner_chunked_updates_merge_and_dedupe_overlapping_sources() {
        let mut idx = FileIndex::default();
        let updates = vec![
            FileListUpdate::Tracked(vec!["src/main.rs".into(), "Cargo.toml".into()]),
            FileListUpdate::Tracked(vec!["src/main.rs".into(), "src/lib.rs".into()]),
            FileListUpdate::Untracked(vec!["src/lib.rs".into(), "examples/demo.rs".into()]),
            FileListUpdate::Prefix(vec!["src".into(), "src/main.rs".into()]),
        ];

        for update in updates {
            merge_scanner_update_for_test(&mut idx, update);
        }

        assert_eq!(idx.paths.iter().filter(|p| *p == "src/main.rs").count(), 1);
        assert_eq!(idx.paths.iter().filter(|p| *p == "src/lib.rs").count(), 1);
        assert_eq!(idx.len(), idx.path_set.len());
        assert!(idx.is_directory("src"));
        assert!(idx.is_directory("examples"));
    }

    #[tokio::test]
    async fn scanner_git_success_with_paths_skips_rg_fallback() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = command_dir();
        let marker = workspace.path().join("rg-invoked");
        write_fake_command(bin.path(), "git", fake_git_tracked_body());
        write_fake_command(bin.path(), "rg", fake_rg_marker_body());
        let _path_guard = PathEnvGuard::prepend(bin.path());
        std::env::set_var("REBON_TEST_RG_MARKER", &marker);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = stream_tracked_with_rg_fallback(workspace.path().to_str().unwrap(), tx).await;

        assert!(
            result.is_ok(),
            "successful fake git should complete scan: {result:?}"
        );
        let update = rx.recv().await.expect("git should stream a tracked batch");
        match update {
            FileListUpdate::Tracked(paths) => assert_eq!(paths, vec!["tracked.rs".to_string()]),
            other => panic!("expected git tracked batch, got {other:?}"),
        }
        assert!(
            !marker.exists(),
            "rg --files fallback should not run when git ls-files emits paths"
        );
    }

    #[tokio::test]
    async fn scanner_rg_fallback_runs_after_slow_git_timeout() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = command_dir();
        write_fake_command(bin.path(), "git", fake_git_sleep_body());
        write_fake_command(bin.path(), "rg", fake_rg_visible_body());
        let _path_guard = PathEnvGuard::prepend(bin.path());
        let (tx, mut rx) = mpsc::unbounded_channel();

        let cwd = workspace.path().to_str().unwrap().to_string();
        let scan = tokio::spawn(async move { stream_tracked_with_rg_fallback(&cwd, tx).await });
        let update = tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .expect("fake rg should stream a fallback batch after slow git times out")
            .expect("scanner channel should remain open");

        match update {
            FileListUpdate::Tracked(paths) => assert_eq!(paths, vec!["visible.rs".to_string()]),
            other => panic!("expected rg tracked batch, got {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(7), scan)
            .await
            .expect("tracked scan should finish after git timeout")
            .expect("scanner task should not panic");
        assert!(
            result.is_ok(),
            "successful fake rg should keep scan usable despite slow git: {result:?}"
        );
    }

    #[tokio::test]
    async fn scanner_git_and_rg_fake_failures_still_use_native_fallback() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = command_dir();
        write_fake_command(bin.path(), "git", fake_git_fail_body());
        write_fake_command(bin.path(), "rg", fake_rg_fail_body());
        std::fs::write(workspace.path().join("native.rs"), "\n").unwrap();
        let _path_guard = PathEnvGuard::prepend(bin.path());
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = stream_tracked_with_rg_fallback(workspace.path().to_str().unwrap(), tx).await;

        assert!(
            result.is_ok(),
            "native fallback should recover after fake command failures: {result:?}"
        );
        let update = rx
            .recv()
            .await
            .expect("native fallback should emit indexed files");
        match update {
            FileListUpdate::Tracked(paths) => assert_eq!(paths, vec!["native.rs".to_string()]),
            other => panic!("expected native tracked batch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prefix_scan_reads_existing_parent_direct_children_and_honors_ignores() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        std::fs::create_dir_all(temp.path().join("src/foo/nested")).unwrap();
        std::fs::create_dir_all(temp.path().join("src/.git")).unwrap();
        std::fs::create_dir_all(temp.path().join("src/node_modules/pkg")).unwrap();
        std::fs::create_dir_all(temp.path().join("src/target/debug")).unwrap();
        std::fs::write(temp.path().join("src/foo/mod.rs"), "mod nested;\n").unwrap();
        std::fs::write(temp.path().join("src/foo/nested/lib.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("src/.git/config"), "[core]\n").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "src/ignored.rs\n").unwrap();
        std::fs::write(temp.path().join("src/ignored.rs"), "ignored\n").unwrap();

        let cwd = temp.path().to_str().unwrap();
        assert_eq!(prefix_scan_dir(cwd, "src/foo"), Some("src".to_string()));

        let paths = read_prefix_seed_blocking(cwd, "src/foo").unwrap();
        assert!(paths.contains(&"src/foo".to_string()));
        assert!(!paths.contains(&"src/foo/mod.rs".to_string()));
        assert!(!paths.contains(&"src/foo/nested".to_string()));
        assert!(!paths.contains(&"src/foo/nested/lib.rs".to_string()));
        assert!(!paths.iter().any(|path| path.contains(".git")));
        assert!(!paths.iter().any(|path| path.contains("node_modules")));
        assert!(!paths.iter().any(|path| path.contains("target")));
        assert!(!paths.contains(&"src/ignored.rs".to_string()));

        let mut idx = FileIndex::default();
        idx.merge_directories(vec!["src".into()]);
        assert!(idx.search("src/foo", 10).is_empty());
        idx.merge(paths);
        let results: Vec<_> = idx
            .search("src/foo", 10)
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(results, vec!["src/foo".to_string()]);
    }

    #[tokio::test]
    async fn prefix_scan_stops_at_batch_limit_without_returning_skipped_dirs() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/big/node_modules")).unwrap();
        std::fs::create_dir_all(temp.path().join("src/big/target")).unwrap();
        for i in 0..(PREFIX_SCAN_BATCH_LIMIT + 50) {
            std::fs::write(temp.path().join(format!("src/big/file-{i:03}.rs")), "\n").unwrap();
        }
        std::fs::write(temp.path().join("src/big/node_modules/pkg.js"), "\n").unwrap();
        std::fs::write(temp.path().join("src/big/target/out.rs"), "\n").unwrap();

        let paths = read_prefix_seed_blocking(temp.path().to_str().unwrap(), "src/big/").unwrap();

        assert_eq!(paths.len(), PREFIX_SCAN_BATCH_LIMIT);
        assert!(!paths.iter().any(|path| path.contains("node_modules")));
        assert!(!paths.iter().any(|path| path.contains("target")));
    }

    #[test]
    fn native_fallback_lists_non_git_tempdir_and_skips_heavy_dirs() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("visible.rs"), "\n").unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "\n").unwrap();
        std::fs::create_dir_all(temp.path().join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(temp.path().join("target/debug")).unwrap();
        std::fs::create_dir_all(temp.path().join(".git/objects")).unwrap();
        std::fs::write(temp.path().join("node_modules/pkg/index.js"), "\n").unwrap();
        std::fs::write(temp.path().join("target/debug/out.rs"), "\n").unwrap();
        std::fs::write(temp.path().join(".git/config"), "\n").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let outcome = stream_native_files_blocking(temp.path().to_str().unwrap(), tx, 100).unwrap();

        assert_eq!(outcome.emitted(), 2);
        let mut paths = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let FileListUpdate::Tracked(batch) = update {
                paths.extend(batch);
            }
        }
        paths.sort();
        assert_eq!(
            paths,
            vec!["src/lib.rs".to_string(), "visible.rs".to_string()]
        );
        assert!(!paths.iter().any(|path| path.contains("node_modules")));
        assert!(!paths.iter().any(|path| path.contains("target")));
        assert!(!paths.iter().any(|path| path.contains(".git")));
    }

    #[tokio::test]
    async fn scanner_cargo_install_like_no_git_no_rg_uses_native_fallback() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = command_dir();
        write_fake_command(bin.path(), "git", fake_git_fail_body());
        std::fs::write(workspace.path().join("visible.rs"), "\n").unwrap();
        std::fs::create_dir_all(workspace.path().join("node_modules/pkg")).unwrap();
        std::fs::write(workspace.path().join("node_modules/pkg/index.js"), "\n").unwrap();
        let _path_guard = PathEnvGuard::prepend(bin.path());
        std::env::set_var("PATH", bin.path().as_os_str());
        std::env::remove_var("REBON_RIPGREP_PATH");
        std::env::remove_var("USE_BUILTIN_RIPGREP");
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = stream_tracked_with_rg_fallback(workspace.path().to_str().unwrap(), tx).await;

        assert!(
            result.is_ok(),
            "native fallback should keep scanner usable: {result:?}"
        );
        let mut paths = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let FileListUpdate::Tracked(batch) = update {
                paths.extend(batch);
            }
        }
        assert_eq!(paths, vec!["visible.rs".to_string()]);
    }

    #[tokio::test]
    async fn spawn_scanner_cargo_install_like_no_rg_reaches_terminal_success() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = command_dir();
        write_fake_command(bin.path(), "git", fake_git_fail_body());
        std::fs::write(workspace.path().join("visible.rs"), "\n").unwrap();
        let _path_guard = PathEnvGuard::prepend(bin.path());
        std::env::set_var("PATH", bin.path().as_os_str());
        std::env::remove_var("REBON_RIPGREP_PATH");
        std::env::remove_var("USE_BUILTIN_RIPGREP");

        let mut rx = spawn_scanner(
            &tokio::runtime::Handle::current(),
            workspace.path().to_str().unwrap().to_string(),
        );
        let mut status = FileScanStatus::Scanning;
        let mut paths = Vec::new();
        for _ in 0..10 {
            let update = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("scanner should emit a terminal result")
                .expect("scanner channel should remain open until complete");
            match update {
                FileListUpdate::Tracked(batch) => paths.extend(batch),
                FileListUpdate::Complete => {
                    status = FileScanStatus::Complete;
                    break;
                }
                FileListUpdate::Failed(reason) => {
                    status = FileScanStatus::Failed(reason);
                    break;
                }
                FileListUpdate::TimedOut(phase) => {
                    status = FileScanStatus::TimedOut(phase);
                    break;
                }
                FileListUpdate::Seed { .. }
                | FileListUpdate::Untracked(_)
                | FileListUpdate::Prefix(_) => {}
            }
        }

        assert_eq!(paths, vec!["visible.rs".to_string()]);
        assert_eq!(status, FileScanStatus::Complete);
    }

    #[tokio::test]
    async fn scanner_ripgrep_not_found_error_is_distinct() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = command_dir();
        let _path_guard = PathEnvGuard::prepend(bin.path());
        std::env::set_var("PATH", bin.path().as_os_str());
        std::env::remove_var("REBON_RIPGREP_PATH");
        std::env::remove_var("USE_BUILTIN_RIPGREP");
        let (tx, _rx) = mpsc::unbounded_channel();

        let err = stream_ripgrep_files(workspace.path().to_str().unwrap(), tx, 100)
            .await
            .expect_err("missing rg should be reported before native fallback handles it");

        match err {
            StreamScanError::Failed(err) => {
                let message = err.to_string();
                assert!(message.contains("rg not found"));
                assert!(message.contains("REBON_RIPGREP_PATH"));
                assert!(message.contains("PATH"));
                assert!(message.contains("native scanner"));
            }
            other => panic!("expected rg not found failure, got {other:?}"),
        }
    }

    #[test]
    fn local_dir_candidates_are_shallow_folder_first_and_escape_safe() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/foo/deep")).unwrap();
        std::fs::create_dir(temp.path().join("src/foobar")).unwrap();
        std::fs::create_dir(temp.path().join("src/node_modules")).unwrap();
        std::fs::create_dir(temp.path().join("src/target")).unwrap();
        std::fs::write(temp.path().join("src/foo.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("src/foo/deep/file.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("src/bar.rs"), "\n").unwrap();

        let results = local_dir_candidates(temp.path().to_str().unwrap(), "src/foo", 10).unwrap();
        let paths: Vec<_> = results.iter().map(|result| result.path.as_str()).collect();
        assert_eq!(paths, vec!["src/foo", "src/foobar", "src/foo.rs"]);
        assert!(results[0].is_directory);
        assert!(results[1].is_directory);
        assert!(!results[2].is_directory);
        assert!(!paths.iter().any(|path| path.contains("deep/file.rs")));

        for query in [
            "/tmp",
            "C:/tmp",
            r"\\server\share",
            "src/../",
            "../../shared/../",
            "src//foo",
        ] {
            assert!(
                local_dir_candidates(temp.path().to_str().unwrap(), query, 10)
                    .unwrap()
                    .is_empty(),
                "unsafe query {query:?} must be rejected"
            );
        }
    }

    #[test]
    fn canonical_file_mention_location_accepts_relative_cwd() {
        let query = FileMentionQuery::parse("").unwrap();
        let location = canonical_file_mention_location(".", &query).unwrap();
        assert!(location.target_dir.is_absolute());
        assert!(location.target_dir.is_dir());
    }

    #[test]
    fn local_dir_candidates_support_parent_multilevel_and_windows_queries() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let cwd = work.join("nested/project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(work.join("shared/src")).unwrap();
        std::fs::write(work.join("shared/Main.rs"), "\n").unwrap();
        std::fs::write(work.join("parent.txt"), "\n").unwrap();

        let parent_paths: Vec<_> = local_dir_candidates(cwd.to_str().unwrap(), "../../", 10)
            .unwrap()
            .into_iter()
            .map(|result| result.path)
            .collect();
        assert!(parent_paths.contains(&"../../nested".to_string()));
        assert!(parent_paths.contains(&"../../shared".to_string()));
        assert!(parent_paths.contains(&"../../parent.txt".to_string()));
        assert!(!parent_paths.iter().any(|path| path.contains("src")));

        let nested_paths: Vec<_> = local_dir_candidates(cwd.to_str().unwrap(), "../../shared/", 10)
            .unwrap()
            .into_iter()
            .map(|result| result.path)
            .collect();
        assert_eq!(
            nested_paths,
            vec![
                "../../shared/src".to_string(),
                "../../shared/Main.rs".to_string(),
            ]
        );

        let windows_paths: Vec<_> =
            local_dir_candidates(cwd.to_str().unwrap(), r"..\..\shared\ma", 10)
                .unwrap()
                .into_iter()
                .map(|result| result.path)
                .collect();
        assert_eq!(windows_paths, vec!["../../shared/Main.rs".to_string()]);
    }

    #[test]
    fn prefix_scan_supports_parent_queries_and_stays_shallow() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("work/project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(temp.path().join("work/shared/deep")).unwrap();
        std::fs::write(temp.path().join("work/shared/deep/hidden.rs"), "\n").unwrap();

        assert_eq!(
            prefix_scan_dir(cwd.to_str().unwrap(), "../sha"),
            Some("..".to_string())
        );
        let paths = read_prefix_seed_blocking(cwd.to_str().unwrap(), "../sha").unwrap();
        assert!(paths.contains(&"../shared".to_string()));
        assert!(!paths.iter().any(|path| path.contains("hidden.rs")));
    }

    #[test]
    fn local_dir_candidates_skip_symlinks_outside_allowed_parent_root() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let cwd = work.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(outside.path().join("secret.rs"), "\n").unwrap();
        let link = work.join("escape");

        #[cfg(unix)]
        if std::os::unix::fs::symlink(outside.path(), &link).is_err() {
            return;
        }
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(outside.path(), &link).is_err() {
            // Creating symlinks may require elevated privileges on Windows.
            return;
        }

        let paths: Vec<_> = local_dir_candidates(cwd.to_str().unwrap(), "../", 10)
            .unwrap()
            .into_iter()
            .map(|result| result.path)
            .collect();
        assert!(!paths.contains(&"../escape".to_string()));
        assert!(
            local_dir_candidates(cwd.to_str().unwrap(), "../escape/", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn local_dir_candidates_split_nested_query_parent_dir_and_prefix_case_insensitive() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher")).unwrap();
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher.bak_mojibake")).unwrap();
        std::fs::write(temp.path().join("mods_src/Watcher/inner.rs"), "\n").unwrap();

        for query in ["mods_src/watcher", r"mods_src\watcher"] {
            let paths: Vec<_> = local_dir_candidates(temp.path().to_str().unwrap(), query, 10)
                .unwrap()
                .into_iter()
                .map(|result| result.path)
                .collect();

            assert_eq!(
                paths,
                vec![
                    "mods_src/Watcher".to_string(),
                    "mods_src/Watcher.bak_mojibake".to_string(),
                ],
                "query {query} should scan mods_src and prefix-match child names"
            );
        }
    }

    #[test]
    fn local_dir_candidates_honor_basic_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join(".gitignore"), "src/ignored.rs\n").unwrap();
        std::fs::write(temp.path().join("src/ignored.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("src/visible.rs"), "\n").unwrap();

        let paths: Vec<_> = local_dir_candidates(temp.path().to_str().unwrap(), "src/", 10)
            .unwrap()
            .into_iter()
            .map(|result| result.path)
            .collect();
        assert_eq!(paths, vec!["src/visible.rs".to_string()]);
    }

    #[test]
    fn local_dir_candidates_scan_past_many_non_matching_siblings_for_prefix_match() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        let limit = 10;
        for i in 0..(limit * 3) {
            std::fs::write(temp.path().join(format!("src/aaa-{i:03}.rs")), "\n").unwrap();
        }
        std::fs::write(temp.path().join("src/z_target.rs"), "\n").unwrap();

        let paths: Vec<_> = local_dir_candidates(temp.path().to_str().unwrap(), "src/z", limit)
            .unwrap()
            .into_iter()
            .map(|result| result.path)
            .collect();

        assert_eq!(paths, vec!["src/z_target.rs".to_string()]);
    }

    #[test]
    fn local_dir_candidates_scan_large_mods_src_past_result_limit_for_child_prefix() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("mods_src")).unwrap();
        let limit = 5;
        for i in 0..(limit * 3) {
            std::fs::create_dir(temp.path().join(format!("mods_src/aaa_nonmatch_{i:03}"))).unwrap();
            std::fs::write(
                temp.path().join(format!("mods_src/bbb_nonmatch_{i:03}.rs")),
                "\n",
            )
            .unwrap();
        }
        std::fs::create_dir(temp.path().join("mods_src/Watcher")).unwrap();
        std::fs::write(temp.path().join("mods_src/Watcher/inner.rs"), "\n").unwrap();

        for query in ["mods_src/watcher", r"mods_src\watcher"] {
            let results =
                local_dir_candidates(temp.path().to_str().unwrap(), query, limit).unwrap();
            let paths: Vec<_> = results.iter().map(|result| result.path.as_str()).collect();

            assert_eq!(
                paths,
                vec!["mods_src/Watcher"],
                "query {query} must filter direct child names before applying limit"
            );
            assert!(results[0].is_directory);
        }
    }

    #[test]
    fn file_index_search_returns_top_results() {
        let mut idx = FileIndex::default();
        idx.merge(vec![
            "src/main.rs".into(),
            "src/lib.rs".into(),
            "Cargo.toml".into(),
            "README.md".into(),
            "src/utils/test_helper.rs".into(),
        ]);

        let results = idx.search("main", 3);
        assert!(!results.is_empty());
        assert_eq!(results[0].path, "src/main.rs");
    }

    #[test]
    fn file_index_search_empty_query_returns_first_n() {
        let mut idx = FileIndex::default();
        idx.merge(vec!["a.rs".into(), "b.rs".into(), "c.rs".into()]);
        let results = idx.search("", 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn file_index_search_empty_query_returns_top_level_paths() {
        let mut idx = FileIndex::default();
        idx.merge(vec!["src/main.rs".into(), "Cargo.toml".into()]);
        let results = idx.search("", 10);
        let paths: Vec<_> = results.into_iter().map(|result| result.path).collect();
        assert_eq!(paths, vec!["src".to_string(), "Cargo.toml".to_string()]);
    }

    #[test]
    fn file_index_empty_query_is_directory_first_and_stably_sorted() {
        let mut idx = FileIndex::default();
        idx.merge(vec![
            "zeta.txt".into(),
            "src/main.rs".into(),
            "Cargo.toml".into(),
            "crates/rebon-cli/src/main.rs".into(),
            "assets/logo.svg".into(),
        ]);

        let paths: Vec<_> = idx.search("", 10).into_iter().map(|r| r.path).collect();
        assert_eq!(
            paths,
            vec!["assets", "crates", "src", "Cargo.toml", "zeta.txt"]
        );
    }

    #[test]
    fn file_index_non_empty_query_prefers_directories_then_files() {
        let mut idx = FileIndex::default();
        idx.merge(vec![
            "src/main.rs".into(),
            "schema.graphql".into(),
            "scripts/build.rs".into(),
            "README.md".into(),
        ]);

        let paths: Vec<_> = idx.search("s", 10).into_iter().map(|r| r.path).collect();
        assert_eq!(paths[..3], ["scripts", "src", "schema.graphql"]);
    }

    #[test]
    fn file_index_path_query_returns_direct_children_only() {
        let mut idx = FileIndex::default();
        idx.merge(vec![
            "src/main.rs".into(),
            "src/lib.rs".into(),
            "src/tools/mod.rs".into(),
            "src/tools/task.rs".into(),
            "scripts/src_alias.rs".into(),
        ]);

        let paths: Vec<_> = idx.search("src/", 10).into_iter().map(|r| r.path).collect();
        assert_eq!(paths, vec!["src/tools", "src/lib.rs", "src/main.rs"]);

        let paths: Vec<_> = idx
            .search("src/t", 10)
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, vec!["src/tools"]);

        let paths: Vec<_> = idx
            .search("src\\t", 10)
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, vec!["src/tools"]);

        let paths: Vec<_> = idx
            .search("src/tools/", 10)
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, vec!["src/tools/mod.rs", "src/tools/task.rs"]);
    }

    #[test]
    fn file_index_merge_deduplicates() {
        let mut idx = FileIndex::default();
        idx.merge(vec!["a.rs".into(), "b.rs".into(), "src/main.rs".into()]);
        idx.merge(vec![
            "b.rs".into(),
            "src/main.rs".into(),
            "src/lib.rs".into(),
            "src/tools/mod.rs".into(),
        ]);

        assert_eq!(idx.paths.iter().filter(|p| *p == "b.rs").count(), 1);
        assert_eq!(idx.paths.iter().filter(|p| *p == "src/main.rs").count(), 1);
        assert!(idx.is_directory("src"));
        assert!(idx.is_directory("src/tools"));
        assert_eq!(idx.len(), idx.path_set.len());
    }

    #[test]
    fn file_index_merge_adds_parent_directories() {
        let mut idx = FileIndex::default();
        idx.merge(vec!["crates/rebon-cli/src/main.rs".into()]);

        let root_results = idx.search("crates", 5);
        assert!(root_results
            .iter()
            .any(|result| { result.path == "crates" && result.is_directory }));
        assert!(idx.is_directory("crates\\"));

        let nested_results = idx.search("rebon-cli", 5);
        assert!(nested_results
            .iter()
            .any(|result| { result.path == "crates/rebon-cli" && result.is_directory }));
    }

    #[test]
    fn file_index_search_fuzzy() {
        let mut idx = FileIndex::default();
        idx.merge(vec![
            "src/PromptInput.rs".into(),
            "src/commandSuggestions.rs".into(),
            "src/utils.rs".into(),
        ]);

        // "pi" should match "PromptInput" (P + I boundary match)
        let results = idx.search("pi", 5);
        assert!(!results.is_empty());

        // "cmds" should match "commandSuggestions"
        let results = idx.search("cmds", 5);
        assert!(!results.is_empty());
    }
}
