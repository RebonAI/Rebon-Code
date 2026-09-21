use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;

use super::{
    shorten_excerpt, BackgroundJobState, BackgroundPullRequestDotStatus,
    BackgroundPullRequestStatus, BackgroundStore, PULL_REQUEST_STATUS_REFRESH_INTERVAL_MS,
    PULL_REQUEST_STATUS_REFRESH_TIMEOUT_MS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GithubPullRequestReference {
    owner: String,
    repo: String,
    number: u64,
    url: String,
}

pub(super) fn refresh_stale_pull_request_statuses(
    store: &BackgroundStore,
    jobs: &[BackgroundJobState],
    now: u64,
) -> anyhow::Result<bool> {
    refresh_stale_pull_request_statuses_with(store, jobs, now, query_github_pull_request_status)
}

fn refresh_stale_pull_request_statuses_with(
    store: &BackgroundStore,
    jobs: &[BackgroundJobState],
    now: u64,
    mut query: impl FnMut(&Path, &GithubPullRequestReference, u64) -> BackgroundPullRequestStatus,
) -> anyhow::Result<bool> {
    let mut changed = false;
    for state in jobs {
        let references = state
            .outcome
            .summary
            .as_deref()
            .map(github_pull_request_references)
            .unwrap_or_default();
        if references.is_empty() {
            if !state.outcome.pull_requests.is_empty() {
                let job_id = state.identity.job_id.clone();
                store.update_state(&job_id, |current| {
                    current.outcome.pull_requests.clear();
                    current.process.updated_at_ms = now;
                    Ok(())
                })?;
                changed = true;
            }
            continue;
        }
        if pull_request_status_cache_is_fresh(state, &references, now) {
            continue;
        }

        let statuses = references
            .iter()
            .map(|reference| query(Path::new(&state.identity.cwd), reference, now))
            .collect::<Vec<_>>();
        let job_id = state.identity.job_id.clone();
        store.update_state(&job_id, |current| {
            current.outcome.pull_requests = statuses.clone();
            current.process.updated_at_ms = now;
            Ok(())
        })?;
        store.append_event(
            &job_id,
            "pr_status_refreshed",
            serde_json::json!({
                "count": statuses.len(),
                "errors": statuses.iter().filter(|status| status.error.is_some()).count(),
            }),
        )?;
        changed = true;

        // Keep the supervisor tick responsive. One stale job per tick is
        // enough while Agent View is open, and avoids running several
        // network-backed `gh` probes in a single UI refresh window.
        break;
    }
    Ok(changed)
}

fn pull_request_status_cache_is_fresh(
    state: &BackgroundJobState,
    references: &[GithubPullRequestReference],
    now: u64,
) -> bool {
    if references.len() != state.outcome.pull_requests.len() {
        return false;
    }
    references.iter().all(|reference| {
        state.outcome.pull_requests.iter().any(|status| {
            status.owner.eq_ignore_ascii_case(&reference.owner)
                && status.repo.eq_ignore_ascii_case(&reference.repo)
                && status.number == reference.number
                && now.saturating_sub(status.updated_at_ms)
                    < PULL_REQUEST_STATUS_REFRESH_INTERVAL_MS
        })
    })
}

fn github_pull_request_references(text: &str) -> Vec<GithubPullRequestReference> {
    let mut references = Vec::new();
    for term in text.split_whitespace() {
        let trimmed = trim_url_punctuation(term);
        let Some(reference) = github_pull_request_reference_from_url(trimmed) else {
            continue;
        };
        if !references
            .iter()
            .any(|existing: &GithubPullRequestReference| {
                existing.owner.eq_ignore_ascii_case(&reference.owner)
                    && existing.repo.eq_ignore_ascii_case(&reference.repo)
                    && existing.number == reference.number
            })
        {
            references.push(reference);
        }
    }
    references
}

fn trim_url_punctuation(value: &str) -> &str {
    value.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '<' | '>' | '`'
        )
    })
}

fn github_pull_request_reference_from_url(url: &str) -> Option<GithubPullRequestReference> {
    let lower = url.to_ascii_lowercase();
    let marker = "github.com/";
    let start = lower.find(marker)? + marker.len();
    let after = &url[start..];
    let mut parts = after.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    let pull = parts.next()?.trim();
    if owner.is_empty()
        || repo.is_empty()
        || !pull.eq_ignore_ascii_case("pull")
        || owner.contains(':')
    {
        return None;
    }
    let number_part = parts.next()?.trim();
    let number_text = number_part
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    let number = number_text.parse::<u64>().ok()?;
    Some(GithubPullRequestReference {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
    })
}

fn query_github_pull_request_status(
    cwd: &Path,
    reference: &GithubPullRequestReference,
    now: u64,
) -> BackgroundPullRequestStatus {
    match run_gh_pr_view(cwd, reference)
        .and_then(|stdout| github_pull_request_status_from_json(reference, &stdout, now))
    {
        Ok(status) => status,
        Err(err) => BackgroundPullRequestStatus {
            url: reference.url.clone(),
            owner: reference.owner.clone(),
            repo: reference.repo.clone(),
            number: reference.number,
            dot: None,
            state: None,
            merge_state: None,
            review_decision: None,
            checks_summary: None,
            error: Some(shorten_excerpt(&err.to_string(), 160)),
            updated_at_ms: now,
        },
    }
}

fn run_gh_pr_view(cwd: &Path, reference: &GithubPullRequestReference) -> anyhow::Result<String> {
    let repo = format!("{}/{}", reference.owner, reference.repo);
    let fields = "state,isDraft,mergeStateStatus,reviewDecision,statusCheckRollup,url,number";
    let mut child = Command::new("gh")
        .arg("pr")
        .arg("view")
        .arg(reference.number.to_string())
        .arg("--repo")
        .arg(repo)
        .arg("--json")
        .arg(fields)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start `gh pr view`")?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if output.status.success() {
                return String::from_utf8(output.stdout)
                    .context("`gh pr view` returned non-utf8 JSON");
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("`gh pr view` failed: {}", shorten_excerpt(&stderr, 160));
        }
        if started.elapsed() >= Duration::from_millis(PULL_REQUEST_STATUS_REFRESH_TIMEOUT_MS) {
            let _ = child.kill();
            let _ = child.wait_with_output();
            anyhow::bail!("`gh pr view` timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GithubCheckRollup {
    total: usize,
    passed: usize,
    failed: usize,
    pending: usize,
}

impl GithubCheckRollup {
    fn from_json(value: Option<&serde_json::Value>) -> Self {
        let Some(items) = value.and_then(|value| value.as_array()) else {
            return Self {
                total: 0,
                passed: 0,
                failed: 0,
                pending: 0,
            };
        };
        let mut rollup = Self {
            total: 0,
            passed: 0,
            failed: 0,
            pending: 0,
        };
        for item in items {
            rollup.total += 1;
            match github_check_state(item) {
                GithubCheckState::Passed => rollup.passed += 1,
                GithubCheckState::Failed => rollup.failed += 1,
                GithubCheckState::Pending => rollup.pending += 1,
            }
        }
        rollup
    }

    fn summary(self) -> Option<String> {
        if self.total == 0 {
            None
        } else if self.failed > 0 {
            Some(format!("{} check(s) failed", self.failed))
        } else if self.pending > 0 {
            Some(format!("{} check(s) pending", self.pending))
        } else {
            Some(format!("{}/{} checks passed", self.passed, self.total))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GithubCheckState {
    Passed,
    Failed,
    Pending,
}

fn github_check_state(item: &serde_json::Value) -> GithubCheckState {
    let conclusion = json_upper_str(item, "conclusion");
    let state = json_upper_str(item, "state");
    let status = json_upper_str(item, "status");

    if conclusion.as_deref().is_some_and(is_failed_github_state)
        || state.as_deref().is_some_and(is_failed_github_state)
    {
        return GithubCheckState::Failed;
    }
    if conclusion.as_deref().is_some_and(is_success_github_state)
        || state.as_deref().is_some_and(is_success_github_state)
    {
        return GithubCheckState::Passed;
    }
    if status.as_deref() == Some("COMPLETED") {
        return GithubCheckState::Passed;
    }
    GithubCheckState::Pending
}

fn is_success_github_state(value: &str) -> bool {
    matches!(value, "SUCCESS" | "SKIPPED" | "NEUTRAL")
}

fn is_failed_github_state(value: &str) -> bool {
    matches!(
        value,
        "FAILURE" | "ERROR" | "ACTION_REQUIRED" | "TIMED_OUT" | "CANCELLED"
    )
}

fn github_pull_request_status_from_json(
    reference: &GithubPullRequestReference,
    data: &str,
    now: u64,
) -> anyhow::Result<BackgroundPullRequestStatus> {
    let value: serde_json::Value =
        serde_json::from_str(data).context("failed to parse `gh pr view` JSON")?;
    let state = json_upper_str(&value, "state");
    let merge_state = json_upper_str(&value, "mergeStateStatus");
    let review_decision = json_upper_str(&value, "reviewDecision");
    let is_draft = value
        .get("isDraft")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let checks = GithubCheckRollup::from_json(value.get("statusCheckRollup"));
    let dot = classify_github_pull_request_status(
        state.as_deref(),
        is_draft,
        merge_state.as_deref(),
        review_decision.as_deref(),
        checks,
    );
    let url = value
        .get("url")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&reference.url)
        .to_string();
    Ok(BackgroundPullRequestStatus {
        url,
        owner: reference.owner.clone(),
        repo: reference.repo.clone(),
        number: reference.number,
        dot: Some(dot),
        state,
        merge_state,
        review_decision,
        checks_summary: checks.summary(),
        error: None,
        updated_at_ms: now,
    })
}

fn classify_github_pull_request_status(
    state: Option<&str>,
    is_draft: bool,
    merge_state: Option<&str>,
    review_decision: Option<&str>,
    checks: GithubCheckRollup,
) -> BackgroundPullRequestDotStatus {
    if state == Some("MERGED") {
        return BackgroundPullRequestDotStatus::Merged;
    }
    if state == Some("CLOSED") || is_draft {
        return BackgroundPullRequestDotStatus::Inactive;
    }
    if checks.failed > 0 || checks.pending > 0 || review_decision == Some("CHANGES_REQUESTED") {
        return BackgroundPullRequestDotStatus::Waiting;
    }
    if review_decision == Some("APPROVED") || checks.total > 0 && checks.failed == 0 {
        return BackgroundPullRequestDotStatus::Ready;
    }
    if merge_state.is_some_and(|state| matches!(state, "CLEAN" | "HAS_HOOKS" | "UNSTABLE")) {
        return BackgroundPullRequestDotStatus::Ready;
    }
    BackgroundPullRequestDotStatus::Waiting
}

fn json_upper_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::background::{BackgroundJobStatus, BackgroundRuntimeFields};

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
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

    #[test]
    fn github_pr_references_extract_unique_pull_urls() {
        let references = github_pull_request_references(
            "opened <https://github.com/Org/Repo/pull/2048>, duplicate (https://github.com/org/repo/pull/2048) and https://github.com/org/repo/pull/2049.",
        );

        assert_eq!(references.len(), 2);
        assert_eq!(references[0].owner, "Org");
        assert_eq!(references[0].repo, "Repo");
        assert_eq!(references[0].number, 2048);
        assert_eq!(references[0].url, "https://github.com/Org/Repo/pull/2048");
        assert_eq!(references[1].number, 2049);
    }

    #[test]
    fn github_pr_status_json_maps_ready_waiting_merged_and_inactive() {
        let reference = GithubPullRequestReference {
            owner: "org".into(),
            repo: "repo".into(),
            number: 2048,
            url: "https://github.com/org/repo/pull/2048".into(),
        };
        let ready = serde_json::json!({
            "url": reference.url.clone(),
            "number": 2048,
            "state": "OPEN",
            "isDraft": false,
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                { "status": "COMPLETED", "conclusion": "SUCCESS" },
                { "state": "SUCCESS" }
            ]
        });
        let parsed =
            github_pull_request_status_from_json(&reference, &ready.to_string(), 123).unwrap();
        assert_eq!(parsed.dot, Some(BackgroundPullRequestDotStatus::Ready));
        assert_eq!(parsed.checks_summary.as_deref(), Some("2/2 checks passed"));

        let pending = serde_json::json!({
            "state": "OPEN",
            "isDraft": false,
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [{ "status": "IN_PROGRESS" }]
        });
        let parsed =
            github_pull_request_status_from_json(&reference, &pending.to_string(), 123).unwrap();
        assert_eq!(parsed.dot, Some(BackgroundPullRequestDotStatus::Waiting));
        assert_eq!(parsed.checks_summary.as_deref(), Some("1 check(s) pending"));

        let merged = serde_json::json!({ "state": "MERGED", "isDraft": false });
        let parsed =
            github_pull_request_status_from_json(&reference, &merged.to_string(), 123).unwrap();
        assert_eq!(parsed.dot, Some(BackgroundPullRequestDotStatus::Merged));

        let draft = serde_json::json!({ "state": "OPEN", "isDraft": true });
        let parsed =
            github_pull_request_status_from_json(&reference, &draft.to_string(), 123).unwrap();
        assert_eq!(parsed.dot, Some(BackgroundPullRequestDotStatus::Inactive));
    }

    #[test]
    fn pull_request_status_cache_requires_matching_fresh_references() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.outcome.summary = Some("opened https://github.com/org/repo/pull/2048".into());
        state.outcome.pull_requests = vec![BackgroundPullRequestStatus {
            url: "https://github.com/org/repo/pull/2048".into(),
            owner: "org".into(),
            repo: "repo".into(),
            number: 2048,
            dot: Some(BackgroundPullRequestDotStatus::Ready),
            state: Some("OPEN".into()),
            merge_state: None,
            review_decision: None,
            checks_summary: None,
            error: None,
            updated_at_ms: 1000,
        }];
        let references = github_pull_request_references(state.outcome.summary.as_deref().unwrap());

        assert!(pull_request_status_cache_is_fresh(
            &state,
            &references,
            1000 + PULL_REQUEST_STATUS_REFRESH_INTERVAL_MS - 1
        ));
        assert!(!pull_request_status_cache_is_fresh(
            &state,
            &references,
            1000 + PULL_REQUEST_STATUS_REFRESH_INTERVAL_MS
        ));

        state.outcome.summary = Some("opened https://github.com/org/repo/pull/2049".into());
        let references = github_pull_request_references(state.outcome.summary.as_deref().unwrap());
        assert!(!pull_request_status_cache_is_fresh(
            &state,
            &references,
            1001
        ));
    }

    #[test]
    fn pull_request_status_refresh_writes_cache_and_event() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.outcome.summary = Some("opened https://github.com/org/repo/pull/2048".into());
        store.write_state(&state).unwrap();
        let jobs = store.list_jobs().unwrap();

        let refreshed =
            refresh_stale_pull_request_statuses_with(&store, &jobs, 123, |_cwd, reference, now| {
                BackgroundPullRequestStatus {
                    url: reference.url.clone(),
                    owner: reference.owner.clone(),
                    repo: reference.repo.clone(),
                    number: reference.number,
                    dot: Some(BackgroundPullRequestDotStatus::Ready),
                    state: Some("OPEN".into()),
                    merge_state: Some("CLEAN".into()),
                    review_decision: Some("APPROVED".into()),
                    checks_summary: Some("1/1 checks passed".into()),
                    error: None,
                    updated_at_ms: now,
                }
            })
            .unwrap();
        assert!(refreshed);

        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.outcome.pull_requests.len(), 1);
        assert_eq!(
            loaded.outcome.pull_requests[0].dot,
            Some(BackgroundPullRequestDotStatus::Ready)
        );
        assert_eq!(loaded.outcome.pull_requests[0].updated_at_ms, 123);
        let events = store.read_events_tail(&state.identity.job_id, 5).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "pr_status_refreshed"));
    }

    #[test]
    fn pull_request_status_refresh_skips_fresh_cache_without_querying() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.outcome.summary = Some("opened https://github.com/org/repo/pull/2048".into());
        state.outcome.pull_requests = vec![BackgroundPullRequestStatus {
            url: "https://github.com/org/repo/pull/2048".into(),
            owner: "org".into(),
            repo: "repo".into(),
            number: 2048,
            dot: Some(BackgroundPullRequestDotStatus::Ready),
            state: Some("OPEN".into()),
            merge_state: Some("CLEAN".into()),
            review_decision: Some("APPROVED".into()),
            checks_summary: Some("1/1 checks passed".into()),
            error: None,
            updated_at_ms: 1_000,
        }];
        store.write_state(&state).unwrap();
        let jobs = store.list_jobs().unwrap();
        let mut query_count = 0;

        let refreshed = refresh_stale_pull_request_statuses_with(
            &store,
            &jobs,
            1_000 + PULL_REQUEST_STATUS_REFRESH_INTERVAL_MS - 1,
            |_cwd, reference, now| {
                query_count += 1;
                BackgroundPullRequestStatus {
                    url: reference.url.clone(),
                    owner: reference.owner.clone(),
                    repo: reference.repo.clone(),
                    number: reference.number,
                    dot: Some(BackgroundPullRequestDotStatus::Ready),
                    state: Some("OPEN".into()),
                    merge_state: None,
                    review_decision: None,
                    checks_summary: None,
                    error: None,
                    updated_at_ms: now,
                }
            },
        )
        .unwrap();

        assert!(!refreshed);
        assert_eq!(query_count, 0);
        assert_eq!(
            store
                .read_state(&state.identity.job_id)
                .unwrap()
                .process
                .updated_at_ms,
            state.process.updated_at_ms
        );
    }

    #[test]
    fn pull_request_status_refresh_limits_work_to_one_stale_job_per_tick() {
        let (_dir, store) = store();
        let mut first = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        first.process.status = BackgroundJobStatus::Succeeded;
        first.outcome.summary = Some("opened https://github.com/org/repo/pull/2048".into());
        store.write_state(&first).unwrap();
        let mut second = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        second.process.status = BackgroundJobStatus::Succeeded;
        second.outcome.summary = Some("opened https://github.com/org/repo/pull/2049".into());
        store.write_state(&second).unwrap();
        let jobs = store.list_jobs().unwrap();
        let mut query_count = 0;

        let refreshed =
            refresh_stale_pull_request_statuses_with(&store, &jobs, 123, |_cwd, reference, now| {
                query_count += 1;
                BackgroundPullRequestStatus {
                    url: reference.url.clone(),
                    owner: reference.owner.clone(),
                    repo: reference.repo.clone(),
                    number: reference.number,
                    dot: Some(BackgroundPullRequestDotStatus::Ready),
                    state: Some("OPEN".into()),
                    merge_state: None,
                    review_decision: None,
                    checks_summary: None,
                    error: None,
                    updated_at_ms: now,
                }
            })
            .unwrap();

        assert!(refreshed);
        assert_eq!(query_count, 1);
        let refreshed_jobs = [
            store.read_state(&first.identity.job_id).unwrap(),
            store.read_state(&second.identity.job_id).unwrap(),
        ]
        .into_iter()
        .filter(|state| !state.outcome.pull_requests.is_empty())
        .count();
        assert_eq!(refreshed_jobs, 1);
    }
}
