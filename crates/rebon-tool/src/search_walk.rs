//! The directory walk behind `Glob` and `Grep`: bounded, cancellable, and run
//! off the async worker.
//!
//! A model that points either tool at a parent folder of many repos, a drive
//! root, or a multi-hundred-GB `CARGO_TARGET_DIR` used to start a walk that
//! visited every entry, held a tokio worker the whole time, and could not be
//! interrupted: the walk never awaited, so the turn's cancel `select!` had
//! nothing to win against. Here the walk runs in `spawn_blocking`, watches a
//! stop flag that dropping the call future sets, and stops on an entry cap or
//! a wall-clock budget — reporting that it stopped, so the model narrows the
//! search instead of trusting a partial answer.

use ignore::{DirEntry, WalkBuilder};
use rebon_tools_core::{ToolError, ToolId, ToolResult};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Entries (files and directories the walker yields) a single search may
/// visit. A large monorepo checkout is well under this once its ignore rules
/// apply; a folder of many repos or a build-output tree is not.
pub(crate) const MAX_VISITED_ENTRIES: usize = 100_000;

/// Wall-clock budget for one search, walk and (for `Grep`) file reads alike.
pub(crate) const WALK_TIME_BUDGET: Duration = Duration::from_secs(10);

/// How many entries pass between clock reads. Reading the clock is cheap next
/// to the syscalls behind each entry, but `Grep` reads a file per entry, so a
/// small interval keeps the overshoot past the budget short.
const CLOCK_CHECK_INTERVAL: usize = 64;

/// VCS metadata is noise even when nobody gitignored it, e.g. `**/HEAD` over
/// a repo root must not dump internal refs.
const VCS_DIRECTORIES_TO_EXCLUDE: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Dependency trees skipped below the search root even outside a git repo.
const DEPENDENCY_DIRECTORIES_TO_EXCLUDE: &[&str] = &["node_modules"];

/// Bounds on one search. `Default` is what the tools run with; tests lower
/// the numbers instead of building 100k-entry trees.
#[derive(Clone)]
pub(crate) struct WalkLimits {
    pub(crate) max_entries: usize,
    pub(crate) time_budget: Duration,
    #[cfg(test)]
    pub(crate) probe: Option<Arc<WalkProbe>>,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_VISITED_ENTRIES,
            time_budget: WALK_TIME_BUDGET,
            #[cfg(test)]
            probe: None,
        }
    }
}

/// Test hooks into the blocking walk: one runs on every entry (to slow the
/// walk down), one when the walk ends (to observe how and when it stopped).
#[cfg(test)]
pub(crate) struct WalkProbe {
    pub(crate) on_entry: Box<dyn Fn() + Send + Sync>,
    pub(crate) on_finish: Box<dyn Fn(&WalkSummary) + Send + Sync>,
}

/// Why a walk ended before running out of entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkStop {
    /// More entries remained after `max_entries` had been visited.
    EntryCap,
    /// The wall-clock budget ran out.
    TimeBudget,
    /// The call future was dropped (the user interrupted the turn).
    Cancelled,
    /// The visitor asked to stop, e.g. `Glob` reached its match cap.
    Visitor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalkSummary {
    pub(crate) visited: usize,
    pub(crate) stopped: Option<WalkStop>,
}

impl WalkSummary {
    /// The note the model reads when the walk hit a limit, `None` when it
    /// finished (or was cancelled, in which case nobody reads the result).
    pub(crate) fn incomplete_note(&self, root: &Path, limits: &WalkLimits) -> Option<String> {
        let root = root.display();
        match self.stopped? {
            WalkStop::EntryCap => Some(format!(
                "Stopped after visiting {} entries under {root}; results are partial. \
                 Narrow `path` or use a more specific pattern.",
                limits.max_entries
            )),
            WalkStop::TimeBudget => Some(format!(
                "Stopped after {}s ({} entries visited) under {root}; results are partial. \
                 Narrow `path` or use a more specific pattern.",
                limits.time_budget.as_secs_f32(),
                self.visited
            )),
            WalkStop::Cancelled | WalkStop::Visitor => None,
        }
    }
}

/// Sets the stop flag when dropped. Held by the async call, so the blocking
/// walk sees the flag at its next entry once the dispatcher drops the future.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Run `job` on tokio's blocking pool and await it. The job gets a stop flag
/// that is set if this future is dropped before the job finishes; a panic or
/// a join failure becomes a [`ToolError::Execution`]. Without a runtime (a
/// caller driving the future some other way) the job runs inline.
pub(crate) async fn run_cancellable<T, F>(tool: ToolId, job: F) -> ToolResult<T>
where
    T: Send + 'static,
    F: FnOnce(Arc<AtomicBool>) -> ToolResult<T> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_on_drop = StopOnDrop(Arc::clone(&stop));
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return job(stop);
    };
    match handle.spawn_blocking(move || job(stop)).await {
        Ok(result) => result,
        Err(err) => {
            let what = if err.is_panic() {
                "panicked"
            } else {
                "was cancelled"
            };
            Err(ToolError::Execution {
                source: anyhow::anyhow!("the {tool} search task {what}"),
                tool,
            })
        }
    }
}

/// Walk `root` with the shared ignore rules and exclusions, handing each
/// entry to `visit` until the tree is exhausted, a limit trips, the stop
/// flag is set, or `visit` breaks. `configure` adds tool-specific builder
/// options (file types, a max file size).
pub(crate) fn walk(
    root: &Path,
    limits: &WalkLimits,
    stop: &Arc<AtomicBool>,
    configure: impl FnOnce(&mut WalkBuilder),
    mut visit: impl FnMut(DirEntry) -> ControlFlow<()>,
) -> WalkSummary {
    let started = Instant::now();
    let mut builder = WalkBuilder::new(root);
    configure(&mut builder);
    let filter_stop = Arc::clone(stop);
    let walker = builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        // Inside a repo, keep git's own scope: ignore files apply up to the
        // repo root and no further, so a stray `.gitignore` in a folder above
        // the checkout (`bin/`, `build/`) cannot hide the repo's sources.
        // Outside any repo there is no such boundary, and honouring the
        // folder's `.gitignore` is what keeps a walk of a folder-of-repos out
        // of their build output.
        .require_git(is_inside_repo(root))
        .follow_links(false)
        .filter_entry(move |entry| !filter_stop.load(Ordering::Relaxed) && should_visit(entry))
        .build();

    let mut visited = 0usize;
    let mut stopped = None;
    for result in walker {
        if stop.load(Ordering::Relaxed) {
            stopped = Some(WalkStop::Cancelled);
            break;
        }
        if visited >= limits.max_entries {
            stopped = Some(WalkStop::EntryCap);
            break;
        }
        visited += 1;
        if visited % CLOCK_CHECK_INTERVAL == 0 && started.elapsed() >= limits.time_budget {
            stopped = Some(WalkStop::TimeBudget);
            break;
        }
        #[cfg(test)]
        if let Some(probe) = &limits.probe {
            (probe.on_entry)();
        }
        let Ok(entry) = result else {
            continue;
        };
        if visit(entry).is_break() {
            stopped = Some(WalkStop::Visitor);
            break;
        }
    }

    // Once the flag is set the `filter_entry` above prunes everything left,
    // so the walk can also end by running dry rather than at the check.
    if stopped.is_none() && stop.load(Ordering::Relaxed) {
        stopped = Some(WalkStop::Cancelled);
    }
    let summary = WalkSummary { visited, stopped };
    #[cfg(test)]
    if let Some(probe) = &limits.probe {
        (probe.on_finish)(&summary);
    }
    summary
}

/// Whether the walk should enter (or yield) this entry. The root is always
/// walked — an explicit search of a build directory still runs, under the
/// limits — but below it VCS metadata, `node_modules` and cargo target
/// directories are skipped whether or not anything gitignores them.
fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if VCS_DIRECTORIES_TO_EXCLUDE.contains(&name.as_ref()) {
        return false;
    }
    if entry.file_type().is_some_and(|ft| ft.is_dir()) {
        if DEPENDENCY_DIRECTORIES_TO_EXCLUDE.contains(&name.as_ref()) {
            return false;
        }
        if is_cargo_target_dir(entry.path(), &name) {
            return false;
        }
    }
    true
}

/// A cargo target directory, or any directory tagged as a cache: cargo writes
/// `.rustc_info.json` into a target dir (including a shared
/// `CARGO_TARGET_DIR` and most per-project dirs inside one), `CACHEDIR.TAG`
/// into the ones it creates, and `.cargo-lock` into every profile dir
/// (`debug/`, `release/`, `<triple>/debug/`) — the marker that still holds
/// when a per-project dir under a shared target dir has neither of the others.
fn is_cargo_target_dir(dir: &Path, name: &str) -> bool {
    if dir.join(".rustc_info.json").exists()
        || dir.join("CACHEDIR.TAG").exists()
        || dir.join(".cargo-lock").exists()
    {
        return true;
    }
    name == "target"
        && (dir.join("debug").join(".cargo-lock").exists()
            || dir.join("release").join(".cargo-lock").exists())
}

/// Whether `root` sits in a git (or jj) checkout — the same markers the
/// `ignore` crate looks for when `require_git` is on.
fn is_inside_repo(root: &Path) -> bool {
    let absolute = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    absolute
        .ancestors()
        .any(|dir| dir.join(".git").exists() || dir.join(".jj").exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn walk_stops_at_the_entry_cap_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for idx in 0..20 {
            fs::write(dir.path().join(format!("f{idx}.txt")), "x").unwrap();
        }
        let limits = WalkLimits {
            max_entries: 5,
            ..WalkLimits::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut seen = 0;
        let summary = walk(
            dir.path(),
            &limits,
            &stop,
            |_| {},
            |_| {
                seen += 1;
                ControlFlow::Continue(())
            },
        );
        assert_eq!(summary.stopped, Some(WalkStop::EntryCap));
        assert_eq!(seen, 5);
        let note = summary.incomplete_note(dir.path(), &limits).unwrap();
        assert!(note.contains("5 entries"), "{note}");
    }

    #[test]
    fn walk_that_fits_reports_no_stop() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        let limits = WalkLimits::default();
        let summary = walk(
            dir.path(),
            &limits,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
            |_| ControlFlow::Continue(()),
        );
        assert_eq!(summary.stopped, None);
        assert_eq!(summary.incomplete_note(dir.path(), &limits), None);
    }

    #[test]
    fn walk_stops_on_the_time_budget() {
        let dir = tempfile::tempdir().unwrap();
        for idx in 0..(CLOCK_CHECK_INTERVAL * 2) {
            fs::write(dir.path().join(format!("f{idx}.txt")), "x").unwrap();
        }
        let limits = WalkLimits {
            time_budget: Duration::ZERO,
            ..WalkLimits::default()
        };
        let summary = walk(
            dir.path(),
            &limits,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
            |_| ControlFlow::Continue(()),
        );
        assert_eq!(summary.stopped, Some(WalkStop::TimeBudget));
        assert_eq!(summary.visited, CLOCK_CHECK_INTERVAL);
        assert!(summary
            .incomplete_note(dir.path(), &limits)
            .unwrap()
            .contains("Stopped after 0s"));
    }

    #[tokio::test]
    async fn a_panicking_job_becomes_an_execution_error() {
        let err = run_cancellable::<(), _>(ToolId::new("Glob"), |_| panic!("boom"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution { .. }), "{err:?}");
        assert!(err.to_string().contains("panicked"), "{err}");
    }

    #[test]
    fn cargo_target_markers_are_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let tagged = dir.path().join("cache");
        fs::create_dir_all(&tagged).unwrap();
        fs::write(
            tagged.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        assert!(is_cargo_target_dir(&tagged, "cache"));

        let target = dir.path().join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        assert!(!is_cargo_target_dir(&target, "target"));
        fs::write(target.join("debug").join(".cargo-lock"), "").unwrap();
        assert!(is_cargo_target_dir(&target, "target"));

        // A profile dir on its own, as under a shared CARGO_TARGET_DIR whose
        // per-project dir has no `.rustc_info.json`.
        let profile = dir.path().join("shared").join("app").join("debug");
        fs::create_dir_all(&profile).unwrap();
        assert!(!is_cargo_target_dir(&profile, "debug"));
        fs::write(profile.join(".cargo-lock"), "").unwrap();
        assert!(is_cargo_target_dir(&profile, "debug"));

        // A plain directory called `target` (a JS build target, say) stays.
        let plain = dir.path().join("other").join("target");
        fs::create_dir_all(&plain).unwrap();
        assert!(!is_cargo_target_dir(&plain, "target"));
    }
}
