use super::super::*;

#[derive(Debug)]
pub(crate) struct BackgroundWorktreeGuard {
    info: Option<rebon_tool::worktree::AgentWorktreeInfo>,
    path: Option<PathBuf>,
}

impl BackgroundWorktreeGuard {
    pub(crate) fn none(path: Option<PathBuf>) -> Self {
        Self { info: None, path }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path
            .as_deref()
            .or_else(|| self.info.as_ref().map(|info| info.worktree_path.as_path()))
    }

    pub(crate) fn preserve(
        self,
        store: &BackgroundStore,
        state: &mut BackgroundJobState,
        reason: &str,
    ) {
        let Some(info) = self.info else {
            return;
        };
        preserve_background_worktree(store, state, &info, reason);
    }

    pub(crate) fn finish_success(self, store: &BackgroundStore, state: &mut BackgroundJobState) {
        let Some(info) = self.info else {
            return;
        };
        // A job whose launcher owns the worktree past the turn (an Agent Queue
        // row, whose diff is reviewed there and merged by the queue) keeps its
        // worktree and branch: committing, merging and removing them here would
        // destroy the very artifact the launcher is waiting on.
        if state.workspace.preserve_worktree_on_success {
            preserve_background_worktree(
                store,
                state,
                &info,
                "job requested the worktree be preserved after a successful turn",
            );
            return;
        }
        finish_background_worktree(store, state, info);
    }
}

fn preserve_background_worktree(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    info: &rebon_tool::worktree::AgentWorktreeInfo,
    reason: &str,
) {
    let path = info.worktree_path.to_string_lossy().to_string();
    state.workspace.worktree_path = Some(path.clone());
    state.process.updated_at_ms = now_ms();
    let job_id = state.identity.job_id.clone();
    let _ = store.update_state(&job_id, |current| {
        current.workspace.worktree_path = Some(path.clone());
        current.process.updated_at_ms = state.process.updated_at_ms;
        Ok(())
    });
    let _ = store.append_event(
        &state.identity.job_id,
        "worktree_preserved",
        serde_json::json!({
            "path": info.worktree_path,
            "branch": info.worktree_branch,
            "source_worktree": info.source_worktree,
            "source_branch": info.source_branch,
            "reason": reason,
        }),
    );
}

/// A job that declared `require_worktree` never falls back to the origin
/// checkout: record why isolation could not be obtained and hand the caller an
/// error, which fails the turn before the model is ever prompted.
fn required_worktree_failure(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    path: Option<&Path>,
    reason: &str,
) -> anyhow::Error {
    let _ = store.append_event(
        &state.identity.job_id,
        "worktree_required_failed",
        serde_json::json!({
            "cwd": state.identity.cwd,
            "path": path.map(|path| path.to_string_lossy().to_string()),
            "reason": reason,
        }),
    );
    anyhow::anyhow!(
        "background job requires an isolated worktree for `{}`, but one could not be prepared: {reason}",
        state.identity.cwd,
    )
}

pub(crate) fn should_prepare_background_worktree(state: &BackgroundJobState) -> bool {
    state.workspace.isolate_in_worktree && !should_skip_background_worktree(&state.identity.cwd)
}

/// Prepare the isolated worktree this job runs in.
///
/// Isolation is best effort by default: a repo that cannot host a worktree
/// falls back to the origin checkout. A job that declared `require_worktree`
/// gets an `Err` instead, which the caller turns into a normal failed turn
/// before the model is prompted.
pub(crate) fn prepare_background_worktree(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<BackgroundWorktreeGuard> {
    // Non-isolated jobs never get past this gate, so everything below runs
    // for a job that was promised an isolated worktree — and may therefore
    // refuse to run at all rather than fall back to the source checkout.
    if !should_prepare_background_worktree(state) {
        // A job already running inside `.rebon/worktrees/...` is isolated by
        // construction; only one that asked for isolation and is getting none
        // breaks the contract.
        if state.workspace.require_worktree && !should_skip_background_worktree(&state.identity.cwd)
        {
            return Err(required_worktree_failure(
                store,
                state,
                None,
                "the job is not marked for worktree isolation",
            ));
        }
        return Ok(BackgroundWorktreeGuard::none(None));
    }
    let slug = background_worktree_slug(&state.identity.job_id);
    let expected_branch = format!("rebon/{slug}");
    if let Some(path) = preserved_background_worktree_path(state) {
        // Reopening is where isolation is silently lost: a gutted worktree
        // directory still answers `git rev-parse` — from the *source*
        // repository, on the *source* branch. An isolated job that cannot
        // prove it is back in its own worktree fails the turn instead.
        let reopened =
            rebon_tool::worktree::reopen_agent_worktree(Path::new(&state.identity.cwd), &path)
                .and_then(|info| {
                    if info.worktree_branch == expected_branch {
                        Ok(info)
                    } else {
                        Err(anyhow::anyhow!(
                            "worktree `{}` is on branch `{}`, expected `{expected_branch}`",
                            info.worktree_path.display(),
                            info.worktree_branch,
                        ))
                    }
                });
        match reopened {
            Ok(info) => {
                let _ = store.append_event(
                    &state.identity.job_id,
                    "worktree_reopened",
                    serde_json::json!({
                        "path": info.worktree_path,
                        "branch": info.worktree_branch,
                        "source_worktree": info.source_worktree,
                        "source_branch": info.source_branch,
                    }),
                );
                return Ok(BackgroundWorktreeGuard {
                    info: Some(info),
                    path: None,
                });
            }
            Err(error) => {
                let _ = store.append_event(
                    &state.identity.job_id,
                    "worktree_lost",
                    serde_json::json!({
                        "path": path,
                        "expected_branch": expected_branch,
                        "error": error.to_string(),
                    }),
                );
                // `require_worktree` only decides what happens when a worktree
                // cannot be *created*. Losing one that was already prepared is
                // never best effort: the recorded worktree holds the job's work
                // so far, and falling back would run the turn in the source
                // checkout and merge onto its branch.
                anyhow::bail!(
                    "background job {job} is isolated in a worktree, but `{path}` could not be \
                     reopened as branch `{expected_branch}`: {error}. Refusing to run this turn \
                     in the source checkout — recover or delete the worktree first.",
                    job = state.identity.job_id,
                    path = path.display(),
                );
            }
        }
    }
    match rebon_tool::worktree::create_agent_worktree(Path::new(&state.identity.cwd), &slug) {
        Ok(info) => {
            let path = info.worktree_path.to_string_lossy().to_string();
            let branch = info.worktree_branch.clone();
            state.workspace.worktree_path = Some(path.clone());
            state.process.updated_at_ms = now_ms();
            let job_id = state.identity.job_id.clone();
            let _ = store.update_state(&job_id, |current| {
                current.workspace.worktree_path = Some(path.clone());
                current.process.updated_at_ms = state.process.updated_at_ms;
                Ok(())
            });
            let _ = store.append_event(
                &state.identity.job_id,
                "worktree_created",
                serde_json::json!({ "path": path, "branch": branch }),
            );
            Ok(BackgroundWorktreeGuard {
                info: Some(info),
                path: None,
            })
        }
        Err(err) => {
            if state.workspace.require_worktree {
                return Err(required_worktree_failure(
                    store,
                    state,
                    None,
                    &err.to_string(),
                ));
            }
            let _ = store.append_event(
                &state.identity.job_id,
                "worktree_skipped",
                serde_json::json!({ "reason": err.to_string() }),
            );
            Ok(BackgroundWorktreeGuard::none(None))
        }
    }
}

pub(crate) fn finish_background_worktree(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    info: rebon_tool::worktree::AgentWorktreeInfo,
) {
    let integration = rebon_tool::worktree::finalize_agent_worktree(
        &info,
        &format!("background: integrate {} changes", state.identity.job_id),
    );
    use rebon_tool::worktree::WorktreeIntegrationStatus as Status;
    let event_type = match integration.status {
        Status::Integrated => "worktree_integrated",
        Status::NoChanges => "worktree_no_changes",
        Status::MergeConflict => "worktree_conflicted",
        // Deferred, not blocked: nested agents are still checked out inside
        // this worktree, so it stays exactly as it is.
        Status::NestedAgentWorktrees => "worktree_finalize_deferred",
        Status::WorktreeLost => "worktree_lost",
        _ => "worktree_integration_blocked",
    };
    let mut payload = serde_json::json!({
        "path": info.worktree_path,
        "branch": info.worktree_branch,
        "source_worktree": info.source_worktree,
        "source_branch": info.source_branch,
        "status": integration.status.as_str(),
        "commit": integration.commit_hash,
        "merge_commit": integration.merge_commit,
        "error": integration.error,
        // `preserved` means "still a usable worktree" — a half-removed
        // directory reports false even though it is still on disk.
        "preserved": integration.preserves_worktree(),
        "worktree": integration.worktree.as_str(),
    });
    if matches!(integration.status, Status::NestedAgentWorktrees) {
        payload["reason"] = serde_json::json!("nested agent worktrees active");
    }
    let _ = store.append_event(&state.identity.job_id, event_type, payload);
    // Only forget the path once the directory is actually gone. A damaged
    // worktree keeps its recorded path so the next turn hits the reopen
    // guard instead of quietly starting over in the source checkout.
    if integration.worktree_removed() {
        state.workspace.worktree_path = None;
        state.process.updated_at_ms = now_ms();
        let job_id = state.identity.job_id.clone();
        let _ = store.update_state(&job_id, |current| {
            current.workspace.worktree_path = None;
            current.process.updated_at_ms = state.process.updated_at_ms;
            Ok(())
        });
    }
    // Deferred integration is not job failure: the completed work is intact
    // on the preserved branch (event + `worktree_path` recorded above), so
    // the job keeps its honest `completed` outcome instead of triggering a
    // redo of work that only failed to merge.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path().join("background"));
        (dir, store)
    }

    fn runtime() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }

    /// A job rooted in a plain directory: `create_agent_worktree` finds no Git
    /// root there, which is exactly the isolation failure this policy governs.
    fn isolated_job(store: &BackgroundStore, cwd: &Path) -> BackgroundJobState {
        let mut state = BackgroundJobState::new(
            "prompt".into(),
            cwd.to_string_lossy().into_owned(),
            runtime(),
            None,
        );
        state.workspace.isolate_in_worktree = true;
        store.write_state(&state).unwrap();
        state
    }

    fn event_kinds(store: &BackgroundStore, job_id: &str) -> Vec<String> {
        store
            .read_events_tail(job_id, 20)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    fn integrated(kinds: &[String]) -> bool {
        kinds.iter().any(|kind| {
            kind.starts_with("worktree_integrat")
                || kind == "worktree_no_changes"
                || kind == "worktree_conflicted"
        })
    }

    fn worktree_info(root: &Path) -> rebon_tool::worktree::AgentWorktreeInfo {
        rebon_tool::worktree::AgentWorktreeInfo {
            worktree_path: root.join(".rebon").join("worktrees").join("bg-row"),
            worktree_branch: "rebon/bg-row".into(),
            head_commit: "0".repeat(40),
            source_worktree: root.to_path_buf(),
            source_branch: Some("main".into()),
            git_root: root.to_path_buf(),
        }
    }

    #[test]
    fn required_isolation_fails_the_turn_when_the_worktree_cannot_be_created() {
        let (dir, store) = store();
        let repo = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut state = isolated_job(&store, &repo);
        state.workspace.require_worktree = true;
        store.write_state(&state).unwrap();

        let error = prepare_background_worktree(&store, &mut state).unwrap_err();

        assert!(
            error.to_string().contains("requires an isolated worktree"),
            "error: {error}"
        );
        assert_eq!(state.workspace.worktree_path, None);
        let kinds = event_kinds(&store, &state.identity.job_id);
        assert!(kinds.iter().any(|kind| kind == "worktree_required_failed"));
        assert!(!kinds.iter().any(|kind| kind == "worktree_skipped"));
    }

    #[test]
    fn required_isolation_fails_when_the_job_is_not_marked_isolated() {
        let (dir, store) = store();
        let repo = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut state = isolated_job(&store, &repo);
        state.workspace.isolate_in_worktree = false;
        state.workspace.require_worktree = true;
        store.write_state(&state).unwrap();

        assert!(prepare_background_worktree(&store, &mut state).is_err());
        assert!(event_kinds(&store, &state.identity.job_id)
            .iter()
            .any(|kind| kind == "worktree_required_failed"));
    }

    #[test]
    fn required_isolation_accepts_a_cwd_that_is_already_an_agent_worktree() {
        let (dir, store) = store();
        let inside = dir
            .path()
            .join("repo")
            .join(".rebon")
            .join("worktrees")
            .join("bg-row");
        std::fs::create_dir_all(&inside).unwrap();
        let mut state = isolated_job(&store, &inside);
        state.workspace.require_worktree = true;
        store.write_state(&state).unwrap();

        let guard = prepare_background_worktree(&store, &mut state).unwrap();

        assert!(guard.path().is_none());
        assert!(!event_kinds(&store, &state.identity.job_id)
            .iter()
            .any(|kind| kind == "worktree_required_failed"));
    }

    #[test]
    fn best_effort_isolation_still_falls_back_when_the_worktree_cannot_be_created() {
        let (dir, store) = store();
        let repo = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut state = isolated_job(&store, &repo);

        let guard = prepare_background_worktree(&store, &mut state).unwrap();

        assert!(
            guard.path().is_none(),
            "caller falls back to the origin cwd"
        );
        assert_eq!(state.workspace.worktree_path, None);
        let kinds = event_kinds(&store, &state.identity.job_id);
        assert!(kinds.iter().any(|kind| kind == "worktree_skipped"));
        assert!(!kinds.iter().any(|kind| kind == "worktree_required_failed"));
    }

    #[test]
    fn required_isolation_fails_the_turn_when_the_preserved_worktree_cannot_reopen() {
        let (dir, store) = store();
        let repo = dir.path().join("not-a-repo");
        let preserved = dir.path().join("preserved-worktree");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&preserved).unwrap();
        let mut state = isolated_job(&store, &repo);
        state.workspace.require_worktree = true;
        state.workspace.worktree_path = Some(preserved.to_string_lossy().into_owned());
        store.write_state(&state).unwrap();

        assert!(prepare_background_worktree(&store, &mut state).is_err());

        let kinds = event_kinds(&store, &state.identity.job_id);
        assert!(kinds.iter().any(|kind| kind == "worktree_lost"));
    }

    /// Losing a worktree that was already prepared is never best effort:
    /// `require_worktree` governs creation, but a job that has a recorded
    /// worktree it cannot reopen fails rather than re-enter the source
    /// checkout and merge onto its branch.
    #[test]
    fn best_effort_isolation_still_fails_when_the_preserved_worktree_cannot_reopen() {
        let (dir, store) = store();
        let repo = dir.path().join("not-a-repo");
        let preserved = dir.path().join("preserved-worktree");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&preserved).unwrap();
        let mut state = isolated_job(&store, &repo);
        state.workspace.worktree_path = Some(preserved.to_string_lossy().into_owned());
        store.write_state(&state).unwrap();

        let error = prepare_background_worktree(&store, &mut state)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Refusing to run this turn"), "{error}");
        let kinds = event_kinds(&store, &state.identity.job_id);
        assert!(kinds.iter().any(|kind| kind == "worktree_lost"));
        assert!(!kinds.iter().any(|kind| kind == "worktree_required_failed"));
    }

    #[test]
    fn preserve_on_success_keeps_the_worktree_and_never_integrates_it() {
        let (dir, store) = store();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut state = isolated_job(&store, &repo);
        state.workspace.preserve_worktree_on_success = true;
        store.write_state(&state).unwrap();
        let info = worktree_info(&repo);
        let guard = BackgroundWorktreeGuard {
            info: Some(info.clone()),
            path: None,
        };

        guard.finish_success(&store, &mut state);

        let expected = info.worktree_path.to_string_lossy().into_owned();
        assert_eq!(
            state.workspace.worktree_path.as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            store
                .read_state(&state.identity.job_id)
                .unwrap()
                .workspace
                .worktree_path
                .as_deref(),
            Some(expected.as_str())
        );
        let kinds = event_kinds(&store, &state.identity.job_id);
        assert!(kinds.iter().any(|kind| kind == "worktree_preserved"));
        assert!(!integrated(&kinds), "integration must not run: {kinds:?}");
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
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

    /// A seeded repo plus the agent worktree a turn would have run in.
    /// Integration refuses to touch a directory that is not a live worktree,
    /// so exercising it needs a real one rather than a synthesized path.
    fn repo_with_agent_worktree(
        root: &Path,
        slug: &str,
    ) -> rebon_tool::worktree::AgentWorktreeInfo {
        run_git(root, &["init", "-q"]);
        run_git(root, &["branch", "-M", "main"]);
        run_git(root, &["config", "user.email", "agent@example.com"]);
        run_git(root, &["config", "user.name", "Rebon Agent"]);
        std::fs::write(root.join(".gitignore"), ".rebon\n").unwrap();
        std::fs::write(root.join("README.md"), "seed\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-qm", "seed"]);
        rebon_tool::worktree::create_agent_worktree(root, slug).unwrap()
    }

    #[test]
    fn success_without_preserve_still_runs_integration() {
        let _env = crate::test_env::lock_env();
        if !git_available() {
            return;
        }
        let (dir, store) = store();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let info = repo_with_agent_worktree(&repo, "bg-row");
        let mut state = isolated_job(&store, &repo);
        let guard = BackgroundWorktreeGuard {
            info: Some(info),
            path: None,
        };

        guard.finish_success(&store, &mut state);

        let kinds = event_kinds(&store, &state.identity.job_id);
        assert!(!kinds.iter().any(|kind| kind == "worktree_preserved"));
        assert!(integrated(&kinds), "integration must run: {kinds:?}");
    }
}
