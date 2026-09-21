//! Git worktree helpers for agent isolation.
//!
//! The external contract maps a slug to worktree info and removes clean
//! worktrees on exit while preserving dirty worktrees. Runtime-specific
//! hook layers and metadata sidecars are intentionally not included here.
//!
//! Slugs allow `[A-Za-z0-9-_]` up to 64 chars and already carry their
//! kind prefix (`agent-…`, `bg-…`). The worktree branch name is
//! `rebon/<slug>` and its on-disk path is
//! `<git-root>/.rebon/worktrees/<slug>`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;

/// Most files a worktree may hand to the source branch before automatic
/// integration refuses to run. A turn that touches more than this is not
/// hand-written work — it is build output, a vendored tree, or a stray
/// `git add -A` over an ignored directory.
const MAX_AUTO_INTEGRATION_FILES: usize = 300;

/// Most newly added blob bytes (10 MiB) automatic integration will merge.
/// Binary payloads count their real byte size, so a single committed
/// executable trips this even though it is only one file.
const MAX_AUTO_INTEGRATION_ADDED_BYTES: u64 = 10 * 1024 * 1024;

/// Where an agent worktree keeps the worktrees of agents it spawned.
/// Anything in here means another agent is checked out *inside* this
/// worktree and cleaning it up would delete that agent's files.
const NESTED_WORKTREES_RELATIVE_DIR: [&str; 2] = [".rebon", "worktrees"];

/// Info returned from [`create_agent_worktree`] — enough for the
/// caller to later call [`remove_agent_worktree`] cleanly.
#[derive(Debug, Clone)]
pub struct AgentWorktreeInfo {
    /// Absolute path to the created worktree directory.
    pub worktree_path: PathBuf,
    /// Name of the branch the worktree checked out.
    pub worktree_branch: String,
    /// SHA of HEAD at creation time — used by
    /// [`has_worktree_changes`] to detect if the agent wrote
    /// anything before cleanup.
    pub head_commit: String,
    /// Absolute path to the source worktree where the agent was launched.
    pub source_worktree: PathBuf,
    /// Branch checked out in the source worktree at creation time. `None`
    /// means the source was detached and cannot be integrated automatically.
    pub source_branch: Option<String>,
    /// Absolute path to the source worktree's git root.
    pub git_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeIntegrationStatus {
    NoChanges,
    Integrated,
    SourceDirty,
    SourceDetached,
    SourceBranchChanged,
    CommitFailed,
    MergeConflict,
    MergeFailed,
    /// The worktree still hosts nested agent worktrees, so integration and
    /// cleanup were deferred rather than deleting another agent's checkout.
    NestedAgentWorktrees,
    /// Staged or committed changes are past the automatic-integration
    /// limits; the branch and worktree were kept for a human to review.
    ChangesTooLarge,
    /// The recorded worktree is no longer a usable Git worktree. Nothing
    /// ran inside it — Git commands there would resolve to the *source*
    /// repository and commit into the wrong branch.
    WorktreeLost,
}

impl WorktreeIntegrationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoChanges => "no_changes",
            Self::Integrated => "integrated",
            Self::SourceDirty => "source_dirty",
            Self::SourceDetached => "source_detached",
            Self::SourceBranchChanged => "source_branch_changed",
            Self::CommitFailed => "commit_failed",
            Self::MergeConflict => "merge_conflict",
            Self::MergeFailed => "merge_failed",
            Self::NestedAgentWorktrees => "nested_agent_worktrees",
            Self::ChangesTooLarge => "changes_too_large",
            Self::WorktreeLost => "worktree_lost",
        }
    }

    pub const fn preserves_worktree(self) -> bool {
        !matches!(
            self,
            Self::NoChanges | Self::Integrated | Self::WorktreeLost
        )
    }
}

/// What actually happened to the worktree *directory* — the honest answer
/// the caller needs before it tells anyone the work was "preserved".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeDisposition {
    /// The directory is gone: cleanup completed.
    Removed,
    /// The directory is still a usable worktree (`.git` link resolves back
    /// to it), so the work in it can be inspected or merged by hand.
    Preserved,
    /// The directory is still on disk but is no longer a usable worktree —
    /// cleanup ran half-way, or something outside Rebon gutted it. This is
    /// never "preserved": Git commands run there escape to the enclosing
    /// repository.
    Damaged,
}

impl WorktreeDisposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::Preserved => "preserved",
            Self::Damaged => "damaged",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeIntegrationResult {
    pub status: WorktreeIntegrationStatus,
    pub commit_hash: Option<String>,
    pub merge_commit: Option<String>,
    pub error: Option<String>,
    /// State of the worktree directory after this call.
    pub worktree: WorktreeDisposition,
}

impl WorktreeIntegrationResult {
    fn preserved(
        status: WorktreeIntegrationStatus,
        commit_hash: Option<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            status,
            commit_hash,
            merge_commit: None,
            error: Some(error.into()),
            worktree: WorktreeDisposition::Preserved,
        }
    }

    /// True only when the worktree is still usable. A half-removed
    /// directory reports `false`: claiming otherwise sends the next turn
    /// back into a shell of a worktree whose Git commands land in the
    /// source repository.
    pub fn preserves_worktree(&self) -> bool {
        matches!(self.worktree, WorktreeDisposition::Preserved)
    }

    /// True when cleanup left an unusable directory behind.
    pub fn worktree_damaged(&self) -> bool {
        matches!(self.worktree, WorktreeDisposition::Damaged)
    }

    /// True when the worktree directory is gone.
    pub fn worktree_removed(&self) -> bool {
        matches!(self.worktree, WorktreeDisposition::Removed)
    }

    /// Notice for a preserved (deferred) integration, phrased so the
    /// reader keeps the completed work instead of redoing the task.
    pub fn deferred_notice(&self, worktree_path: &Path, worktree_branch: &str) -> String {
        let reason = self.error.as_deref().unwrap_or("integration could not run");
        format!(
            "worktree integration deferred ({status}): {reason}. The agent's completed changes \
             are preserved on branch `{worktree_branch}` at {path} and were NOT merged into the \
             source branch; inspect or merge them manually instead of redoing the task.",
            status = self.status.as_str(),
            path = worktree_path.display(),
        )
    }
}

/// Select a worktree source without escaping the caller's authorized roots.
pub fn authorized_worktree_base(
    cwd: Option<&Path>,
    authorized_roots: &[PathBuf],
) -> Result<PathBuf> {
    if let Some(cwd) = cwd {
        if authorized_roots.is_empty()
            || authorized_roots
                .iter()
                .any(|root| crate::path_scope::path_is_within_root(cwd, root))
        {
            return Ok(cwd.to_path_buf());
        }
    }
    if let Some(root) = authorized_roots.first() {
        return Ok(root.clone());
    }
    std::env::current_dir().context("cannot determine cwd for agent worktree")
}

fn ensure_authorized_worktree_paths(
    git_root: &Path,
    worktree_path: &Path,
    authorized_roots: &[PathBuf],
) -> Result<()> {
    if authorized_roots.is_empty()
        || authorized_roots.iter().any(|root| {
            crate::path_scope::path_is_within_root(git_root, root)
                && crate::path_scope::path_is_within_root(worktree_path, root)
        })
    {
        return Ok(());
    }
    bail!(
        "cannot create agent worktree: containing Git root `{}` and generated path `{}` must stay within authorized roots {}",
        git_root.display(),
        worktree_path.display(),
        crate::path_scope::format_roots(authorized_roots)
    )
}

/// Create an agent-scoped git worktree at `<git-root>/.rebon/worktrees/<slug>`.
///
/// Errors if `cwd` is not inside a git repo or if the worktree or
/// branch could not be created. The caller should catch the error
/// and run without isolation (same graceful-degrade contract).
pub fn create_agent_worktree(cwd: &Path, slug: &str) -> Result<AgentWorktreeInfo> {
    create_authorized_agent_worktree(cwd, &[], slug)
}

/// Create a worktree only when both its Git root and output path are covered
/// by the caller's original authorized roots.
pub fn create_authorized_agent_worktree(
    cwd: &Path,
    authorized_roots: &[PathBuf],
    slug: &str,
) -> Result<AgentWorktreeInfo> {
    validate_slug(slug)?;
    let git_root = find_git_root(cwd)
        .ok_or_else(|| anyhow!("cannot create agent worktree: not in a git repository"))?;
    let worktrees_dir = git_root.join(".rebon").join("worktrees");
    let worktree_path = worktrees_dir.join(slug);
    ensure_authorized_worktree_paths(&git_root, &worktree_path, authorized_roots)?;
    std::fs::create_dir_all(&worktrees_dir).with_context(|| {
        format!(
            "failed to create worktree parent dir {}",
            worktrees_dir.display()
        )
    })?;
    // Slugs already carry their kind prefix (`agent-…`, `bg-…`);
    // prefixing again here would produce `rebon/agent-agent-…` branches.
    let worktree_branch = format!("rebon/{slug}");
    let head_commit = git_head_sha(&git_root)
        .ok_or_else(|| anyhow!("cannot resolve HEAD in {}", git_root.display()))?;

    // `git worktree add -b <branch> <path>` is idempotent only when
    // the path doesn't exist — if it does, assume a prior run left it
    // there and reuse it via `git worktree add --force`.
    let exists = worktree_path.exists();
    let output = if exists {
        Command::new("git")
            .current_dir(&git_root)
            .arg("worktree")
            .arg("add")
            .arg("--force")
            .arg(&worktree_path)
            .arg(&worktree_branch)
            .output()
    } else {
        Command::new("git")
            .current_dir(&git_root)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&worktree_branch)
            .arg(&worktree_path)
            .output()
    }
    .with_context(|| {
        format!(
            "failed to spawn `git worktree add` in {}",
            git_root.display()
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        bail!("git worktree add failed: {}", stderr.trim());
    }

    Ok(AgentWorktreeInfo {
        worktree_path,
        worktree_branch,
        head_commit,
        source_worktree: git_root.clone(),
        source_branch: git_current_branch(&git_root)
            .ok()
            .filter(|branch| !branch.trim().is_empty()),
        git_root,
    })
}

/// Remove an agent worktree created by [`create_agent_worktree`].
/// Runs `git worktree remove --force <path>` from the main repo root
/// so the worktree dir is deleted even when clean, and attempts to
/// drop the temp branch afterwards (best-effort — a failure to delete
/// the branch is logged but doesn't bubble up).
///
/// Returns `true` if removal succeeded.
pub fn remove_agent_worktree(info: &AgentWorktreeInfo) -> bool {
    matches!(cleanup_agent_worktree(info), WorktreeDisposition::Removed)
}

/// Remove the worktree and report what the directory actually looks like
/// afterwards.
///
/// The verdict comes from the filesystem, not from git's exit code: a
/// removal that fails part-way (files held open by a still-running nested
/// agent, for instance) leaves a directory that no longer carries a `.git`
/// link, and reporting that as "preserved" is how a later turn ends up
/// running against the *source* repository.
pub fn cleanup_agent_worktree(info: &AgentWorktreeInfo) -> WorktreeDisposition {
    let remove = Command::new("git")
        .current_dir(&info.git_root)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(&info.worktree_path)
        .output();
    match remove {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            tracing::warn!(
                path = %info.worktree_path.display(),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "git worktree remove failed"
            );
        }
        Err(err) => {
            tracing::warn!(
                path = %info.worktree_path.display(),
                error = %err,
                "failed to spawn `git worktree remove`"
            );
        }
    }

    let disposition = if !info.worktree_path.exists() {
        WorktreeDisposition::Removed
    } else if worktree_is_intact(&info.worktree_path) {
        WorktreeDisposition::Preserved
    } else {
        tracing::warn!(
            path = %info.worktree_path.display(),
            "worktree cleanup left a directory that is no longer a usable Git worktree"
        );
        WorktreeDisposition::Damaged
    };

    // Best-effort branch cleanup — the worktree may have advanced the
    // branch past HEAD if the agent committed. Keep the branch when
    // `git branch -D` fails; branch cleanup failure is non-fatal. A
    // damaged directory keeps its branch too: the commits on it are the
    // only surviving copy of the agent's work.
    if matches!(disposition, WorktreeDisposition::Removed) {
        let _ = Command::new("git")
            .current_dir(&info.git_root)
            .args(["branch", "-D", &info.worktree_branch])
            .output();
    }
    disposition
}

/// True when `path` is the root of a live Git worktree.
///
/// The `.git` link check is load-bearing. Without it `git rev-parse`
/// happily answers from an *enclosing* repository, so a gutted worktree
/// directory reports the source repo's git dir and the source repo's
/// branch — which is exactly how a background job once merged into `main`
/// while believing it was isolated.
pub fn worktree_is_intact(path: &Path) -> bool {
    if !path.join(".git").exists() {
        return false;
    }
    let Ok(toplevel) = git_output(path, ["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    if toplevel.trim().is_empty() {
        return false;
    }
    same_path(Path::new(toplevel.trim()), path)
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (
        std::fs::canonicalize(left).ok(),
        std::fs::canonicalize(right).ok(),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

/// Names of the nested agent worktrees checked out inside `worktree_path`
/// (`<worktree>/.rebon/worktrees/*`). A non-empty result means another
/// agent is live in there: integrating or removing this worktree would
/// delete its checkout out from under it.
pub fn nested_agent_worktrees(worktree_path: &Path) -> Vec<String> {
    let mut dir = worktree_path.to_path_buf();
    for segment in NESTED_WORKTREES_RELATIVE_DIR {
        dir.push(segment);
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// True when `worktree_path` hosts at least one nested agent worktree.
pub fn has_nested_agent_worktrees(worktree_path: &Path) -> bool {
    !nested_agent_worktrees(worktree_path).is_empty()
}

/// True if the worktree at `path` has any uncommitted changes or
/// has advanced past `head_commit`. Used to decide whether the
/// post-run cleanup should preserve the worktree (agent did work)
/// or remove it (nothing changed).
pub fn has_worktree_changes(path: &Path, head_commit: &str) -> bool {
    // Any staged/unstaged/untracked change → dirty.
    let status = Command::new("git")
        .current_dir(path)
        .args(["status", "--porcelain"])
        .output();
    if let Ok(out) = status {
        if !out.stdout.is_empty() {
            return true;
        }
    } else {
        // If we can't check, conservatively treat as dirty.
        return true;
    }

    // Branch head may have advanced via a commit even when working
    // tree is clean.
    let rev = Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "HEAD"])
        .output();
    match rev {
        Ok(out) if out.status.success() => {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            !sha.is_empty() && sha != head_commit
        }
        _ => true,
    }
}

pub fn finalize_agent_worktree(
    info: &AgentWorktreeInfo,
    commit_message: &str,
) -> WorktreeIntegrationResult {
    // Nested agents first: `git worktree remove --force` deletes whatever
    // is under the directory, including the checkouts of agents still
    // running inside it, and a removal that only half-succeeds leaves them
    // spinning in a directory with no `.git` link. Defer instead — a later
    // turn integrates once the nested worktrees are gone.
    let nested = nested_agent_worktrees(&info.worktree_path);
    if !nested.is_empty() {
        return WorktreeIntegrationResult::preserved(
            WorktreeIntegrationStatus::NestedAgentWorktrees,
            None,
            format!(
                "nested agent worktrees active ({count}): {names}",
                count = nested.len(),
                names = nested.join(", "),
            ),
        );
    }
    // Never run Git inside a directory that is not a worktree any more:
    // `git add -A` there resolves to the enclosing source repository and
    // commits the user's tree onto the source branch.
    if !worktree_is_intact(&info.worktree_path) {
        return WorktreeIntegrationResult {
            status: WorktreeIntegrationStatus::WorktreeLost,
            commit_hash: None,
            merge_commit: None,
            error: Some(format!(
                "`{}` is no longer a usable Git worktree, so nothing was committed or merged; \
                 any work on branch `{}` must be recovered by hand",
                info.worktree_path.display(),
                info.worktree_branch,
            )),
            worktree: if info.worktree_path.exists() {
                WorktreeDisposition::Damaged
            } else {
                WorktreeDisposition::Removed
            },
        };
    }

    let commit_hash = match commit_agent_worktree(info, commit_message) {
        Ok(WorktreeCommit::Committed(commit_hash)) => commit_hash,
        Ok(WorktreeCommit::Unchanged) => {
            let disposition = cleanup_agent_worktree(info);
            let error = match disposition {
                WorktreeDisposition::Removed => None,
                WorktreeDisposition::Preserved => Some(format!(
                    "worktree had no changes but cleanup failed; it is still a usable worktree at {}",
                    info.worktree_path.display()
                )),
                WorktreeDisposition::Damaged => Some(format!(
                    "worktree had no changes and cleanup failed part-way: {} is no longer a usable \
                     Git worktree — run `git worktree prune` and delete the directory by hand",
                    info.worktree_path.display()
                )),
            };
            return WorktreeIntegrationResult {
                status: WorktreeIntegrationStatus::NoChanges,
                commit_hash: None,
                merge_commit: None,
                error,
                worktree: disposition,
            };
        }
        Ok(WorktreeCommit::TooLarge(summary)) => {
            return WorktreeIntegrationResult::preserved(
                WorktreeIntegrationStatus::ChangesTooLarge,
                git_current_head(&info.worktree_path).ok(),
                summary.blocked_reason("staged worktree changes", &info.worktree_branch),
            );
        }
        Err(error) => {
            return WorktreeIntegrationResult::preserved(
                WorktreeIntegrationStatus::CommitFailed,
                git_current_head(&info.worktree_path).ok(),
                error.to_string(),
            );
        }
    };

    // The staged guard only sees this turn's uncommitted work. A branch
    // that already carries a runaway commit — from an earlier turn, or
    // from the agent committing by hand — is measured against the commit
    // it forked from before anything reaches the source branch.
    if !info.head_commit.trim().is_empty() {
        match committed_change_summary(&info.worktree_path, &info.head_commit) {
            Ok(summary) if summary.exceeds_auto_integration_limits() => {
                return WorktreeIntegrationResult::preserved(
                    WorktreeIntegrationStatus::ChangesTooLarge,
                    Some(commit_hash),
                    summary.blocked_reason("worktree branch commits", &info.worktree_branch),
                );
            }
            Ok(_) => {}
            Err(error) => {
                // Measurement is advisory here; the merge below is still
                // gated by the source-branch checks.
                tracing::warn!(
                    path = %info.worktree_path.display(),
                    %error,
                    "could not measure agent worktree commits before integration"
                );
            }
        }
    }

    let Some(source_branch) = info.source_branch.as_deref() else {
        return WorktreeIntegrationResult::preserved(
            WorktreeIntegrationStatus::SourceDetached,
            Some(commit_hash),
            "source worktree was detached when the agent worktree was created",
        );
    };

    let _lock = match RepoIntegrationLock::acquire(&info.source_worktree) {
        Ok(lock) => lock,
        Err(error) => {
            return WorktreeIntegrationResult::preserved(
                WorktreeIntegrationStatus::MergeFailed,
                Some(commit_hash),
                error.to_string(),
            );
        }
    };

    let current_branch = match git_current_branch(&info.source_worktree) {
        Ok(branch) => branch,
        Err(error) => {
            return WorktreeIntegrationResult::preserved(
                WorktreeIntegrationStatus::SourceBranchChanged,
                Some(commit_hash),
                error.to_string(),
            );
        }
    };
    if current_branch.trim().is_empty() {
        return WorktreeIntegrationResult::preserved(
            WorktreeIntegrationStatus::SourceDetached,
            Some(commit_hash),
            "source worktree is detached",
        );
    }
    if current_branch != source_branch {
        return WorktreeIntegrationResult::preserved(
            WorktreeIntegrationStatus::SourceBranchChanged,
            Some(commit_hash),
            format!("source worktree changed branch from `{source_branch}` to `{current_branch}`"),
        );
    }

    match git_status_porcelain(&info.source_worktree) {
        Ok(status) if status.trim().is_empty() => {}
        Ok(_) => {
            return WorktreeIntegrationResult::preserved(
                WorktreeIntegrationStatus::SourceDirty,
                Some(commit_hash),
                "source worktree has uncommitted changes",
            );
        }
        Err(error) => {
            return WorktreeIntegrationResult::preserved(
                WorktreeIntegrationStatus::SourceDirty,
                Some(commit_hash),
                error.to_string(),
            );
        }
    }

    let merge = Command::new("git")
        .current_dir(&info.source_worktree)
        .args(["merge", "--no-edit", &info.worktree_branch])
        .output();
    match merge {
        Ok(output) if output.status.success() => {
            let merge_commit = git_current_head(&info.source_worktree).ok();
            let disposition = cleanup_agent_worktree(info);
            let cleanup_error = match disposition {
                WorktreeDisposition::Removed => None,
                WorktreeDisposition::Preserved => Some(format!(
                    "merge succeeded but worktree cleanup failed; {} is still a usable worktree",
                    info.worktree_path.display()
                )),
                WorktreeDisposition::Damaged => Some(format!(
                    "merge succeeded but worktree cleanup failed part-way: {} is no longer a \
                     usable Git worktree — run `git worktree prune` and delete the directory by hand",
                    info.worktree_path.display()
                )),
            };
            WorktreeIntegrationResult {
                status: WorktreeIntegrationStatus::Integrated,
                commit_hash: Some(commit_hash),
                merge_commit,
                error: cleanup_error,
                worktree: disposition,
            }
        }
        Ok(output) => {
            let conflict = git_output(
                &info.source_worktree,
                ["diff", "--name-only", "--diff-filter=U"],
            )
            .is_ok_and(|paths| !paths.trim().is_empty());
            let abort = Command::new("git")
                .current_dir(&info.source_worktree)
                .args(["merge", "--abort"])
                .output();
            let mut error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            match abort {
                Ok(abort) if abort.status.success() => {
                    match git_status_porcelain(&info.source_worktree) {
                        Ok(status) if status.trim().is_empty() => {}
                        Ok(status) => {
                            error = format!(
                                "{error}; merge abort left source worktree dirty:\n{status}"
                            );
                        }
                        Err(status_error) => {
                            error = format!(
                                "{error}; could not verify source after merge abort: {status_error}"
                            );
                        }
                    }
                }
                Ok(abort) => {
                    let abort_error = String::from_utf8_lossy(&abort.stderr).trim().to_string();
                    if !abort_error.is_empty() {
                        error = format!("{error}; merge abort failed: {abort_error}");
                    }
                }
                Err(abort_error) => {
                    error = format!("{error}; failed to spawn merge abort: {abort_error}");
                }
            }
            WorktreeIntegrationResult::preserved(
                if conflict {
                    WorktreeIntegrationStatus::MergeConflict
                } else {
                    WorktreeIntegrationStatus::MergeFailed
                },
                Some(commit_hash),
                error,
            )
        }
        Err(error) => WorktreeIntegrationResult::preserved(
            WorktreeIntegrationStatus::MergeFailed,
            Some(commit_hash),
            format!("failed to spawn git merge: {error}"),
        ),
    }
}

/// Result of the pre-integration commit inside the agent worktree.
enum WorktreeCommit {
    /// Nothing new relative to `info.head_commit`.
    Unchanged,
    /// The worktree branch now points at this commit.
    Committed(String),
    /// Staged content is past the automatic-integration limits. Nothing was
    /// committed; the changes stay staged in the preserved worktree.
    TooLarge(WorktreeChangeSummary),
}

fn commit_agent_worktree(info: &AgentWorktreeInfo, commit_message: &str) -> Result<WorktreeCommit> {
    let status = git_status_porcelain(&info.worktree_path)?;
    if !status.trim().is_empty() {
        git_command(&info.worktree_path, &["add", "-A"])?;
        let staged = Command::new("git")
            .current_dir(&info.worktree_path)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .with_context(|| {
                format!(
                    "failed to inspect staged changes in {}",
                    info.worktree_path.display()
                )
            })?;
        match staged.code() {
            Some(0) => {}
            Some(1) => {
                // `git add -A` is unguarded by design — an agent's work can
                // be anywhere in the tree — so this is where a build-cache
                // or binary blowout gets caught, before it becomes a commit
                // that automatic integration would merge into the source
                // branch. Failure to measure is fail-closed: it surfaces as
                // `commit_failed`, which preserves the worktree.
                let summary = staged_change_summary(&info.worktree_path)?;
                if summary.exceeds_auto_integration_limits() {
                    return Ok(WorktreeCommit::TooLarge(summary));
                }
                git_command(&info.worktree_path, &["commit", "-m", commit_message])?;
            }
            _ => bail!(
                "git diff --cached --quiet failed in {}",
                info.worktree_path.display()
            ),
        }
    }

    let head = git_current_head(&info.worktree_path)?;
    Ok(if head == info.head_commit {
        WorktreeCommit::Unchanged
    } else {
        WorktreeCommit::Committed(head)
    })
}

/// Size of a change set, in the two units that matter for deciding whether
/// a merge is somebody's work or somebody's `target/` directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeChangeSummary {
    /// Files touched.
    pub files: usize,
    /// Bytes of new blob content: the full size of added files plus the
    /// growth of modified ones. Binary payloads count real bytes.
    pub added_bytes: u64,
}

impl WorktreeChangeSummary {
    pub const fn exceeds_auto_integration_limits(&self) -> bool {
        self.files > MAX_AUTO_INTEGRATION_FILES
            || self.added_bytes > MAX_AUTO_INTEGRATION_ADDED_BYTES
    }

    fn blocked_reason(&self, scope: &str, branch: &str) -> String {
        format!(
            "{scope} exceed the automatic-integration limits: {files} files (limit {file_limit}) \
             and {bytes} bytes of new content (limit {byte_limit}). Nothing was merged; branch \
             `{branch}` and its worktree are preserved. A change this size is usually build \
             output or a vendored binary — review it before merging by hand.",
            files = self.files,
            file_limit = MAX_AUTO_INTEGRATION_FILES,
            bytes = self.added_bytes,
            byte_limit = MAX_AUTO_INTEGRATION_ADDED_BYTES,
        )
    }
}

/// Measure what `git add -A` just staged in the worktree.
fn staged_change_summary(worktree_path: &Path) -> Result<WorktreeChangeSummary> {
    change_summary(
        worktree_path,
        &[
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-renames",
            "--abbrev=40",
        ],
    )
}

/// Measure everything the worktree branch added since it forked.
fn committed_change_summary(worktree_path: &Path, base: &str) -> Result<WorktreeChangeSummary> {
    change_summary(
        worktree_path,
        &[
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--abbrev=40",
            base,
            "HEAD",
        ],
    )
}

/// Parse `git diff --raw -z` records into a file count plus added bytes.
///
/// `--raw` carries both blob object ids per path, so binary files are
/// measured by their real size instead of the `-` that `--numstat` prints
/// for them; sizes come from a single batched `git cat-file` call.
fn change_summary(cwd: &Path, args: &[&str]) -> Result<WorktreeChangeSummary> {
    let raw = git_output_args(cwd, args)?;
    let mut fields = raw.split('\0').filter(|field| !field.is_empty());
    let mut summary = WorktreeChangeSummary::default();
    let mut blobs: Vec<(String, String)> = Vec::new();
    while let Some(meta) = fields.next() {
        // Every record is `:<mode> <mode> <src> <dst> <status>\0<path>\0`.
        if fields.next().is_none() {
            break;
        }
        let Some(meta) = meta.strip_prefix(':') else {
            continue;
        };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }
        summary.files += 1;
        let (src, dst) = (parts[2], parts[3]);
        if is_null_object_id(dst) {
            // Deletion: removes bytes, never adds them.
            continue;
        }
        blobs.push((src.to_string(), dst.to_string()));
    }

    let mut wanted: Vec<String> = Vec::new();
    for (src, dst) in &blobs {
        wanted.push(dst.clone());
        if !is_null_object_id(src) {
            wanted.push(src.clone());
        }
    }
    wanted.sort();
    wanted.dedup();
    let sizes = blob_sizes(cwd, &wanted)?;
    for (src, dst) in &blobs {
        let new_size = sizes.get(dst).copied().unwrap_or(0);
        let old_size = if is_null_object_id(src) {
            0
        } else {
            sizes.get(src).copied().unwrap_or(0)
        };
        summary.added_bytes = summary
            .added_bytes
            .saturating_add(new_size.saturating_sub(old_size));
    }
    Ok(summary)
}

fn is_null_object_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|ch| ch == '0')
}

/// Ask git for the byte size of every listed blob in one batch.
fn blob_sizes(cwd: &Path, object_ids: &[String]) -> Result<HashMap<String, u64>> {
    if object_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut child = Command::new("git")
        .current_dir(cwd)
        .args(["cat-file", "--batch-check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn `git cat-file` in {}", cwd.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("git cat-file stdin was not piped"))?;
    let payload: String = object_ids
        .iter()
        .map(|id| format!("{id}\n"))
        .collect::<Vec<_>>()
        .concat();
    // Write from a helper thread: `cat-file --batch-check` answers as it
    // reads, so a large request written inline deadlocks once the stdout
    // pipe buffer fills.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.flush();
    });
    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to run `git cat-file` in {}", cwd.display()))?;
    let _ = writer.join();
    if !output.status.success() {
        bail!("git cat-file --batch-check failed in {}", cwd.display());
    }
    let mut sizes = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // `<oid> <type> <size>`; missing objects answer `<oid> missing`.
        if parts.len() == 3 && parts[1] == "blob" {
            if let Ok(size) = parts[2].parse::<u64>() {
                sizes.insert(parts[0].to_string(), size);
            }
        }
    }
    Ok(sizes)
}

fn git_command(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or("command"),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

struct RepoIntegrationLock {
    file: File,
}

impl RepoIntegrationLock {
    fn acquire(cwd: &Path) -> Result<Self> {
        let common_dir = git_common_dir(cwd)?;
        let lock_path = common_dir.join("rebon-worktree-integration.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open integration lock {}", lock_path.display()))?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock integration lock {}", lock_path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for RepoIntegrationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn git_common_dir(cwd: &Path) -> Result<PathBuf> {
    let raw = git_output(cwd, ["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

pub fn reopen_agent_worktree(source_cwd: &Path, worktree_path: &Path) -> Result<AgentWorktreeInfo> {
    let source_worktree = find_git_root(source_cwd).ok_or_else(|| {
        anyhow!(
            "cannot reopen agent worktree: `{}` is not in a Git repository",
            source_cwd.display()
        )
    })?;
    // A directory that lost its `.git` link is not this worktree any more.
    // Git would answer every question below from the *enclosing* repo — the
    // source branch, the source HEAD — and the caller would run a supposedly
    // isolated turn straight against the source checkout.
    if !worktree_is_intact(worktree_path) {
        bail!(
            "cannot reopen agent worktree `{}`: it is no longer a valid Git worktree (missing or \
             stale `.git` link)",
            worktree_path.display()
        );
    }
    let worktree_branch = git_current_branch(worktree_path)?;
    if worktree_branch.trim().is_empty() {
        bail!(
            "cannot reopen agent worktree `{}` from detached HEAD",
            worktree_path.display()
        );
    }
    let source_branch = git_current_branch(&source_worktree)?;
    if source_branch.trim().is_empty() {
        bail!(
            "cannot reopen agent worktree because source `{}` is detached",
            source_worktree.display()
        );
    }
    let head_commit = git_output(
        &source_worktree,
        [
            "merge-base",
            source_branch.as_str(),
            worktree_branch.as_str(),
        ],
    )?;
    Ok(AgentWorktreeInfo {
        worktree_path: worktree_path.to_path_buf(),
        worktree_branch,
        head_commit,
        source_worktree: source_worktree.clone(),
        source_branch: Some(source_branch),
        git_root: source_worktree,
    })
}

/// Locate the canonical git root containing `cwd`: runs
/// `git rev-parse --show-toplevel` and returns its absolute path.
pub fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

/// Validate a worktree slug. Accepts ASCII letters, digits, `-`, and `_`,
/// bounded to 64 chars so the on-disk path never exceeds Windows
/// `MAX_PATH` when nested under `.rebon/worktrees`.
fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || slug.len() > 64 {
        bail!("worktree slug must be 1–64 chars: got {} chars", slug.len());
    }
    for c in slug.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("worktree slug `{slug}` contains invalid char `{c}`");
        }
    }
    Ok(())
}

pub fn git_current_head(cwd: &Path) -> Result<String> {
    git_output(cwd, ["rev-parse", "HEAD"])
}

pub fn git_current_branch(cwd: &Path) -> Result<String> {
    git_output(cwd, ["branch", "--show-current"])
}

pub fn git_status_porcelain(cwd: &Path) -> Result<String> {
    git_output(cwd, ["status", "--porcelain"])
}

pub fn git_is_ancestor(cwd: &Path, base: &str, head: &str) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(cwd)
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(base)
        .arg(head)
        .output()
        .with_context(|| format!("failed to spawn `git merge-base` in {}", cwd.display()))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

pub fn git_head_sha(git_root: &Path) -> Option<String> {
    git_output(git_root, ["rev-parse", "HEAD"]).ok()
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    Ok(git_output_args(cwd, &args)?.trim().to_string())
}

/// Raw (untrimmed) stdout — `-z` output must keep its NUL framing.
fn git_output_args(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorized_worktree_base_prefers_cwd_inside_root() {
        let root = std::env::temp_dir().join("rebon-worktree-authorized-root");
        let cwd = root.join("child");

        assert_eq!(
            authorized_worktree_base(Some(&cwd), std::slice::from_ref(&root)).unwrap(),
            cwd
        );
    }

    #[test]
    fn authorized_worktree_base_falls_back_to_root_for_external_cwd() {
        let root = std::env::temp_dir().join("rebon-worktree-authorized-root");
        let outside = std::env::temp_dir().join("rebon-worktree-external-cwd");

        assert_eq!(
            authorized_worktree_base(Some(&outside), std::slice::from_ref(&root)).unwrap(),
            root
        );
    }

    #[test]
    fn authorized_worktree_base_uses_root_when_cwd_is_missing() {
        let root = std::env::temp_dir().join("rebon-worktree-authorized-root");

        assert_eq!(
            authorized_worktree_base(None, std::slice::from_ref(&root)).unwrap(),
            root
        );
    }

    #[test]
    fn authorized_worktree_paths_allow_git_root_scope() {
        let git_root = std::env::temp_dir().join("rebon-worktree-repo");
        let worktree = git_root.join(".rebon/worktrees/agent-test");

        assert!(ensure_authorized_worktree_paths(
            &git_root,
            &worktree,
            std::slice::from_ref(&git_root)
        )
        .is_ok());
    }

    #[test]
    fn authorized_worktree_paths_reject_repository_subdirectory_scope() {
        let git_root = std::env::temp_dir().join("rebon-worktree-repo");
        let authorized = git_root.join("crates/rebon-tool");
        let worktree = git_root.join(".rebon/worktrees/agent-test");

        let err = ensure_authorized_worktree_paths(
            &git_root,
            &worktree,
            std::slice::from_ref(&authorized),
        )
        .unwrap_err();
        assert!(err.to_string().contains("containing Git root"));
    }

    #[test]
    fn validate_slug_accepts_allowed_chars() {
        assert!(validate_slug("agent-1234abcd").is_ok());
        assert!(validate_slug("a_b-c").is_ok());
        assert!(validate_slug("A").is_ok());
    }

    #[test]
    fn validate_slug_rejects_empty_and_long() {
        assert!(validate_slug("").is_err());
        let long = "a".repeat(65);
        assert!(validate_slug(&long).is_err());
    }

    #[test]
    fn validate_slug_rejects_special_chars() {
        assert!(validate_slug("agent/1").is_err());
        assert!(validate_slug("agent.1").is_err());
        assert!(validate_slug("agent 1").is_err());
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A seeded repo that holds the crate-wide env lock for as long as the
    /// test keeps it: other tests in this binary blank `PATH` process-wide,
    /// and a test shelling out to `git` while that is in flight cannot
    /// resolve it.
    struct TestRepo {
        _env: std::sync::RwLockReadGuard<'static, ()>,
        dir: tempfile::TempDir,
    }

    impl TestRepo {
        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    fn init_repo(label: &str) -> Option<TestRepo> {
        // Shared: this only spawns `git`; it must not see a blanked `PATH`.
        let env = crate::test_env::hold_env();
        if !git_available() {
            return None;
        }
        let temp = tempfile::Builder::new().prefix(label).tempdir().unwrap();
        run_git(temp.path(), &["init", "-q"]);
        run_git(temp.path(), &["branch", "-M", "main"]);
        run_git(temp.path(), &["config", "user.email", "agent@example.com"]);
        run_git(temp.path(), &["config", "user.name", "Rebon Agent"]);
        std::fs::write(temp.path().join(".gitignore"), ".rebon\n").unwrap();
        std::fs::write(temp.path().join("README.md"), "seed\n").unwrap();
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-qm", "seed"]);
        Some(TestRepo {
            _env: env,
            dir: temp,
        })
    }

    #[test]
    fn finalize_agent_worktree_removes_unchanged_worktree() {
        let Some(repo) = init_repo("rebon-worktree-empty-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "agent-empty").unwrap();

        let result = finalize_agent_worktree(&info, "agent: empty");

        assert_eq!(result.status, WorktreeIntegrationStatus::NoChanges);
        assert!(!info.worktree_path.exists());
    }

    #[test]
    fn finalize_agent_worktree_commits_and_merges_changes() {
        let Some(repo) = init_repo("rebon-worktree-merge-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "agent-merge").unwrap();
        std::fs::write(info.worktree_path.join("agent.txt"), "agent work\n").unwrap();

        let result = finalize_agent_worktree(&info, "agent: merge work");

        assert_eq!(result.status, WorktreeIntegrationStatus::Integrated);
        assert!(result.commit_hash.is_some());
        assert!(result.merge_commit.is_some());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("agent.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "agent work\n"
        );
        assert!(!info.worktree_path.exists());
    }

    #[test]
    fn finalize_agent_worktree_serializes_multiple_agent_merges() {
        let Some(repo) = init_repo("rebon-worktree-series-") else {
            return;
        };
        let first = create_agent_worktree(repo.path(), "agent-first").unwrap();
        let second = create_agent_worktree(repo.path(), "agent-second").unwrap();
        std::fs::write(first.worktree_path.join("first.txt"), "first\n").unwrap();
        std::fs::write(second.worktree_path.join("second.txt"), "second\n").unwrap();

        let first_result = finalize_agent_worktree(&first, "agent: first");
        let second_result = finalize_agent_worktree(&second, "agent: second");

        assert_eq!(first_result.status, WorktreeIntegrationStatus::Integrated);
        assert_eq!(second_result.status, WorktreeIntegrationStatus::Integrated);
        assert!(repo.path().join("first.txt").exists());
        assert!(repo.path().join("second.txt").exists());
    }

    #[test]
    fn finalize_agent_worktree_preserves_when_source_is_dirty() {
        let Some(repo) = init_repo("rebon-worktree-dirty-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "agent-dirty").unwrap();
        std::fs::write(info.worktree_path.join("agent.txt"), "agent\n").unwrap();
        std::fs::write(repo.path().join("local.txt"), "local\n").unwrap();

        let result = finalize_agent_worktree(&info, "agent: blocked");

        assert_eq!(result.status, WorktreeIntegrationStatus::SourceDirty);
        assert!(result.commit_hash.is_some());
        assert!(info.worktree_path.exists());
        assert!(!repo.path().join("agent.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("local.txt")).unwrap(),
            "local\n"
        );
    }

    #[test]
    fn finalize_agent_worktree_merges_after_source_branch_advances() {
        let Some(repo) = init_repo("rebon-worktree-advanced-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "agent-advanced").unwrap();
        std::fs::write(repo.path().join("source.txt"), "source\n").unwrap();
        run_git(repo.path(), &["add", "source.txt"]);
        run_git(repo.path(), &["commit", "-qm", "advance source"]);
        std::fs::write(info.worktree_path.join("agent.txt"), "agent\n").unwrap();

        let result = finalize_agent_worktree(&info, "agent: merge after advance");

        assert_eq!(result.status, WorktreeIntegrationStatus::Integrated);
        assert!(repo.path().join("source.txt").exists());
        assert!(repo.path().join("agent.txt").exists());
    }

    #[test]
    fn finalize_agent_worktree_preserves_when_source_branch_changes() {
        let Some(repo) = init_repo("rebon-worktree-branch-change-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "agent-branch-change").unwrap();
        run_git(repo.path(), &["switch", "-q", "-c", "other"]);
        std::fs::write(info.worktree_path.join("agent.txt"), "agent\n").unwrap();

        let result = finalize_agent_worktree(&info, "agent: blocked branch change");

        assert_eq!(
            result.status,
            WorktreeIntegrationStatus::SourceBranchChanged
        );
        assert!(info.worktree_path.exists());
        assert!(!repo.path().join("agent.txt").exists());
    }

    #[test]
    fn finalize_agent_worktree_preserves_when_source_is_detached() {
        let Some(repo) = init_repo("rebon-worktree-detached-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "agent-detached").unwrap();
        run_git(repo.path(), &["switch", "-q", "--detach"]);
        std::fs::write(info.worktree_path.join("agent.txt"), "agent\n").unwrap();

        let result = finalize_agent_worktree(&info, "agent: blocked detached source");

        assert_eq!(result.status, WorktreeIntegrationStatus::SourceDetached);
        assert!(info.worktree_path.exists());
        assert!(!repo.path().join("agent.txt").exists());
    }

    #[test]
    fn finalize_agent_worktree_aborts_conflict_and_preserves_branch() {
        let Some(repo) = init_repo("rebon-worktree-conflict-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "agent-conflict").unwrap();
        std::fs::write(info.worktree_path.join("README.md"), "agent\n").unwrap();
        std::fs::write(repo.path().join("README.md"), "source\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-qm", "source change"]);

        let result = finalize_agent_worktree(&info, "agent: conflict");

        assert_eq!(result.status, WorktreeIntegrationStatus::MergeConflict);
        assert!(info.worktree_path.exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("README.md"))
                .unwrap()
                .replace("\r\n", "\n"),
            "source\n"
        );
        assert!(git_status_porcelain(repo.path()).unwrap().is_empty());
    }

    /// Gut a worktree the way a half-completed `git worktree remove`
    /// does: the `.git` link and the checkout are gone, the directory
    /// itself survives.
    fn gut_worktree(worktree_path: &Path) {
        for entry in std::fs::read_dir(worktree_path).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == ".rebon") {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
        }
    }

    #[test]
    fn finalize_agent_worktree_defers_while_nested_worktrees_are_checked_out() {
        let Some(repo) = init_repo("rebon-worktree-nested-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-nested").unwrap();
        std::fs::write(info.worktree_path.join("agent.txt"), "agent work\n").unwrap();
        // A sub-agent checked out inside the job's worktree.
        let nested = create_agent_worktree(&info.worktree_path, "agent-child").unwrap();
        assert!(nested.worktree_path.exists());

        let result = finalize_agent_worktree(&info, "background: integrate");

        assert_eq!(
            result.status,
            WorktreeIntegrationStatus::NestedAgentWorktrees
        );
        assert!(result.preserves_worktree());
        assert_eq!(result.worktree, WorktreeDisposition::Preserved);
        assert!(result
            .error
            .as_deref()
            .unwrap()
            .contains("nested agent worktrees active"));
        // Nothing merged, nothing deleted: the nested agent keeps working.
        assert!(nested.worktree_path.exists());
        assert!(worktree_is_intact(&nested.worktree_path));
        assert!(worktree_is_intact(&info.worktree_path));
        assert!(!repo.path().join("agent.txt").exists());
        assert_eq!(
            std::fs::read_to_string(info.worktree_path.join("agent.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "agent work\n"
        );
    }

    #[test]
    fn nested_agent_worktrees_lists_children_and_ignores_missing_dir() {
        let temp = tempfile::Builder::new()
            .prefix("rebon-worktree-nested-list-")
            .tempdir()
            .unwrap();

        assert!(nested_agent_worktrees(temp.path()).is_empty());
        assert!(!has_nested_agent_worktrees(temp.path()));

        let nested = temp.path().join(".rebon").join("worktrees");
        std::fs::create_dir_all(nested.join("agent-b")).unwrap();
        std::fs::create_dir_all(nested.join("agent-a")).unwrap();

        assert_eq!(
            nested_agent_worktrees(temp.path()),
            vec!["agent-a".to_string(), "agent-b".to_string()]
        );
        assert!(has_nested_agent_worktrees(temp.path()));
    }

    #[test]
    fn worktree_is_intact_rejects_a_gutted_directory() {
        let Some(repo) = init_repo("rebon-worktree-intact-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-intact").unwrap();
        assert!(worktree_is_intact(&info.worktree_path));
        assert!(!worktree_is_intact(&info.worktree_path.join("missing")));

        std::fs::create_dir_all(info.worktree_path.join(".rebon")).unwrap();
        gut_worktree(&info.worktree_path);

        // The load-bearing part: git still answers from the *enclosing*
        // repository here, so "rev-parse succeeded" is not evidence.
        assert!(git_current_branch(&info.worktree_path).is_ok());
        assert!(!worktree_is_intact(&info.worktree_path));
    }

    #[test]
    fn cleanup_agent_worktree_reports_damage_rather_than_preserved() {
        let Some(repo) = init_repo("rebon-worktree-damaged-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-damaged").unwrap();
        std::fs::create_dir_all(info.worktree_path.join(".rebon")).unwrap();
        gut_worktree(&info.worktree_path);

        let disposition = cleanup_agent_worktree(&info);

        assert_eq!(disposition, WorktreeDisposition::Damaged);
        assert!(!remove_agent_worktree(&info));
        assert!(info.worktree_path.exists());
    }

    #[test]
    fn finalize_agent_worktree_refuses_to_run_inside_a_gutted_worktree() {
        let Some(repo) = init_repo("rebon-worktree-lost-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-lost").unwrap();
        gut_worktree(&info.worktree_path);
        // Uncommitted work in the *source* checkout: the bug this guards
        // against committed it onto the source branch.
        std::fs::write(repo.path().join("user-edit.txt"), "user work\n").unwrap();
        let source_head = git_current_head(repo.path()).unwrap();

        let result = finalize_agent_worktree(&info, "background: integrate");

        assert_eq!(result.status, WorktreeIntegrationStatus::WorktreeLost);
        assert!(!result.preserves_worktree());
        assert!(result.worktree_damaged());
        assert_eq!(git_current_head(repo.path()).unwrap(), source_head);
        assert!(git_status_porcelain(repo.path())
            .unwrap()
            .contains("user-edit.txt"));
    }

    #[test]
    fn finalize_agent_worktree_blocks_when_too_many_files_are_staged() {
        let Some(repo) = init_repo("rebon-worktree-many-files-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-many-files").unwrap();
        let cache = info.worktree_path.join("build-cache");
        std::fs::create_dir_all(&cache).unwrap();
        for index in 0..=MAX_AUTO_INTEGRATION_FILES {
            std::fs::write(cache.join(format!("artifact-{index}.o")), "x").unwrap();
        }

        let result = finalize_agent_worktree(&info, "background: integrate");

        assert_eq!(result.status, WorktreeIntegrationStatus::ChangesTooLarge);
        assert!(result.preserves_worktree());
        let error = result.error.unwrap();
        assert!(error.contains(&format!("{} files", MAX_AUTO_INTEGRATION_FILES + 1)));
        assert!(error.contains(&info.worktree_branch));
        // Preserved, unmerged, uncommitted.
        assert!(worktree_is_intact(&info.worktree_path));
        assert!(!repo.path().join("build-cache").exists());
        assert_eq!(
            git_current_head(&info.worktree_path).unwrap(),
            info.head_commit
        );
    }

    #[test]
    fn finalize_agent_worktree_blocks_when_added_bytes_exceed_limit() {
        let Some(repo) = init_repo("rebon-worktree-big-blob-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-big-blob").unwrap();
        let oversized = (MAX_AUTO_INTEGRATION_ADDED_BYTES + 1) as usize;
        std::fs::write(info.worktree_path.join("app.exe"), vec![b'x'; oversized]).unwrap();

        let result = finalize_agent_worktree(&info, "background: integrate");

        assert_eq!(result.status, WorktreeIntegrationStatus::ChangesTooLarge);
        assert!(result.preserves_worktree());
        let error = result.error.unwrap();
        assert!(error.contains("1 files"));
        assert!(error.contains(&format!("{oversized} bytes")));
        assert!(!repo.path().join("app.exe").exists());
        assert_eq!(
            git_current_head(&info.worktree_path).unwrap(),
            info.head_commit
        );
    }

    #[test]
    fn finalize_agent_worktree_blocks_a_committed_oversized_change() {
        let Some(repo) = init_repo("rebon-worktree-big-commit-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-big-commit").unwrap();
        let oversized = (MAX_AUTO_INTEGRATION_ADDED_BYTES + 1) as usize;
        std::fs::write(info.worktree_path.join("app.exe"), vec![b'y'; oversized]).unwrap();
        run_git(&info.worktree_path, &["add", "-A"]);
        run_git(
            &info.worktree_path,
            &["commit", "-qm", "agent committed it"],
        );
        let committed = git_current_head(&info.worktree_path).unwrap();

        let result = finalize_agent_worktree(&info, "background: integrate");

        assert_eq!(result.status, WorktreeIntegrationStatus::ChangesTooLarge);
        assert_eq!(result.commit_hash.as_deref(), Some(committed.as_str()));
        assert!(result.merge_commit.is_none());
        assert!(result.preserves_worktree());
        assert!(!repo.path().join("app.exe").exists());
    }

    #[test]
    fn finalize_agent_worktree_integrates_a_change_below_the_limits() {
        let Some(repo) = init_repo("rebon-worktree-under-limit-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-under-limit").unwrap();
        for index in 0..10 {
            std::fs::write(
                info.worktree_path.join(format!("note-{index}.txt")),
                "small\n",
            )
            .unwrap();
        }

        let result = finalize_agent_worktree(&info, "background: integrate");

        assert_eq!(result.status, WorktreeIntegrationStatus::Integrated);
        assert_eq!(result.worktree, WorktreeDisposition::Removed);
        assert!(!result.preserves_worktree());
        assert!(repo.path().join("note-9.txt").exists());
    }

    #[test]
    fn reopen_agent_worktree_rejects_a_gutted_worktree() {
        let Some(repo) = init_repo("rebon-worktree-reopen-") else {
            return;
        };
        let info = create_agent_worktree(repo.path(), "bg-reopen").unwrap();

        let reopened = reopen_agent_worktree(repo.path(), &info.worktree_path).unwrap();
        assert_eq!(reopened.worktree_branch, "rebon/bg-reopen");

        gut_worktree(&info.worktree_path);

        let error = reopen_agent_worktree(repo.path(), &info.worktree_path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no longer a valid Git worktree"), "{error}");
    }

    /// Integration test: create + cleanup a worktree. Needs `git`
    /// on PATH and writeable temp dir; marked `ignore` so CI
    /// environments without git don't fail.
    #[test]
    #[ignore = "requires git on PATH"]
    fn create_then_remove_round_trip() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-worktree-it-")
            .tempdir()
            .unwrap();
        let tmp = tmp_dir.path().to_path_buf();
        // Bootstrap a throwaway repo.
        Command::new("git")
            .current_dir(&tmp)
            .args(["init", "-q"])
            .status()
            .unwrap();
        std::fs::write(tmp.join("README.md"), "hi\n").unwrap();
        Command::new("git")
            .current_dir(&tmp)
            .args(["add", "."])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(&tmp)
            .args([
                "-c",
                "user.email=a@b.c",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "seed",
            ])
            .status()
            .unwrap();

        let info = create_agent_worktree(&tmp, "agent-test1").unwrap();
        assert!(info.worktree_path.exists());
        assert_eq!(info.worktree_branch, "rebon/agent-test1");
        assert!(!has_worktree_changes(
            &info.worktree_path,
            &info.head_commit
        ));

        let removed = remove_agent_worktree(&info);
        assert!(removed);
        assert!(!info.worktree_path.exists());
    }
}
