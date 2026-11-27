//! Sub-agent tasks handed to members of the session's agent group.
//!
//! A sub-agent whose model slot is `@name` — an `agent()` step of a
//! workflow, an `Agent` call — does not run here. It goes to the agent
//! group this session is in (`rebon_group`):
//!
//! - `@Ada` names a member: the task is posted to it as an awaited request,
//!   and its reply is the task's result.
//! - `@claudecode`, `@codex`, `@rebon` name an agent: the app is asked to
//!   start a new member of it on the task ([`rebon_group::assign`]), on the
//!   model after the colon when one is given (`@codex:gpt-5`), and its reply
//!   is the result.
//!
//! The session waits on the group log for the reply; whatever gets it to the
//! member — the app waking it, its own next turn — is the group's usual
//! delivery. This process starts nothing and wakes no one: that is the
//! desktop app's part, so with no app running a new member is refused after
//! [`CLAIM_TIMEOUT`] and a request no one picks up after [`PICKUP_TIMEOUT`].
//!
//! [`ExternalRunners`] puts this beside the ACP pool on the one runner seat
//! the spawner has: `@` specs come here, the rest go to the pool.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rebon_group::assign::{self, Handed, SpawnState};
use rebon_group::model::MemberKey;
use rebon_group::{AgentKind, GroupStore};
use rebon_tool::external_agent::{
    ExternalSubAgentRunner, ExternalTaskEvent, ExternalTaskOutcome, ExternalTaskRequest,
    ExternalTaskStatus,
};
use rebon_types::{PromptCancel, ToolCallStatus};
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::acp_subagent_pool::AcpSubAgentPool;

/// How often the log is read while a task is out.
const POLL: Duration = Duration::from_millis(750);
/// How long the app has to take a new member on before the task is refused:
/// it reads the groups every few seconds while it runs.
pub const CLAIM_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the app has to start one it has taken on.
const START_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long a member has to be handed the request at all. It may be deep in
/// a turn of its own, so this is generous; a member that gets it and works
/// on it is waited for as long as it takes.
pub const PICKUP_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The runner for `@name` sub-agent specs.
#[derive(Clone, Debug)]
pub struct GroupMemberRunner {
    store: GroupStore,
    /// The session the runner was built for: the asker when a task does not
    /// name its own (`parent_session_id`).
    session: Option<String>,
    poll: Duration,
    claim_timeout: Duration,
    pickup_timeout: Duration,
}

impl GroupMemberRunner {
    /// The runner on the groups under `config_home`, for `session`.
    pub fn new(config_home: &Path, session: Option<&str>) -> Self {
        let mut runner = Self::with_store(GroupStore::new(rebon_group::default_root(config_home)));
        runner.session = session
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .map(str::to_string);
        runner
    }

    pub fn with_store(store: GroupStore) -> Self {
        Self {
            store,
            session: None,
            poll: POLL,
            claim_timeout: CLAIM_TIMEOUT,
            pickup_timeout: PICKUP_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeouts(mut self, poll: Duration, claim: Duration, pickup: Duration) -> Self {
        self.poll = poll;
        self.claim_timeout = claim;
        self.pickup_timeout = pickup;
        self
    }

    /// Waits for the app to start the member `spawn_id` asked for: the
    /// posted request, once it has.
    async fn started(
        &self,
        group_id: &str,
        asker: &MemberKey,
        spawn_id: &str,
        agent: &str,
        cancel: &PromptCancel,
    ) -> Result<Handed, String> {
        let asked = Instant::now();
        loop {
            let claimed =
                match assign::spawn_state(&self.store, group_id, spawn_id).map_err(describe)? {
                    SpawnState::Started(posted) => return Ok(posted),
                    SpawnState::Refused(reason) => {
                        return Err(format!("Rebon did not start a {agent} member: {reason}"));
                    }
                    SpawnState::Waiting { claimed } => claimed,
                };
            if cancel.is_cancelled() {
                if !self
                    .store
                    .withdraw_spawn(group_id, spawn_id)
                    .map_err(describe)?
                {
                    // Being started already: let it go to waste quietly.
                    self.release_when_started(group_id, asker, spawn_id);
                }
                return Err("cancelled".to_string());
            }
            let waited = asked.elapsed();
            if !claimed
                && waited >= self.claim_timeout
                && self
                    .store
                    .withdraw_spawn(group_id, spawn_id)
                    .map_err(describe)?
            {
                return Err(format!(
                    "Rebon did not start a {agent} member within {} s. New members are started \
                     by the Rebon desktop app: is it running?",
                    self.claim_timeout.as_secs()
                ));
            }
            if waited >= self.claim_timeout + START_TIMEOUT {
                self.release_when_started(group_id, asker, spawn_id);
                return Err(format!(
                    "Rebon is still starting the {agent} member; gave up"
                ));
            }
            tokio::select! {
                () = tokio::time::sleep(self.poll) => {}
                () = cancel.notified() => {}
            }
        }
    }

    /// A member started for a task no one waits on any more: withdrawn once
    /// the app has started it.
    fn release_when_started(&self, group_id: &str, asker: &MemberKey, spawn_id: &str) {
        let store = self.store.clone();
        let (group_id, asker, spawn_id) =
            (group_id.to_string(), asker.clone(), spawn_id.to_string());
        let poll = self.poll;
        tokio::spawn(async move {
            let until = Instant::now() + START_TIMEOUT;
            while Instant::now() < until {
                if assign::release_started(&store, &group_id, &asker, &spawn_id) {
                    return;
                }
                tokio::time::sleep(poll).await;
            }
        });
    }

    /// Waits for the reply to `request` (log entry `seq`) from `alias`.
    async fn reply(
        &self,
        group_id: &str,
        asker: &MemberKey,
        alias: &str,
        request: &str,
        seq: u64,
        cancel: &PromptCancel,
    ) -> Result<ExternalTaskOutcome, String> {
        let posted = Instant::now();
        let mut seen = seq;
        let mut handed = false;
        let outcome = |final_text: String, status: ExternalTaskStatus| ExternalTaskOutcome {
            final_text,
            status,
            tool_call_count: 0,
            acp_session_id: Some(format!("@{alias}")),
        };
        loop {
            let (reply, last) =
                assign::reply_to(&self.store, group_id, request, seen).map_err(describe)?;
            seen = last;
            if let Some(reply) = reply {
                return Ok(outcome(
                    assign::answer_for_asker(&reply),
                    ExternalTaskStatus::Completed,
                ));
            }
            if cancel.is_cancelled() {
                assign::withdraw(&self.store, group_id, asker, alias, request);
                return Ok(outcome(String::new(), ExternalTaskStatus::Cancelled));
            }
            if !handed {
                handed = assign::handed_over(&self.store, group_id, alias, seq);
                if !handed && posted.elapsed() >= self.pickup_timeout {
                    assign::withdraw(&self.store, group_id, asker, alias, request);
                    return Err(format!(
                        "@{alias} was not handed {request} within {} min. Members are woken \
                         by the Rebon desktop app: is it running?",
                        self.pickup_timeout.as_secs() / 60
                    ));
                }
            }
            tokio::select! {
                () = tokio::time::sleep(self.poll) => {}
                () = cancel.notified() => {}
            }
        }
    }
}

fn describe(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[async_trait]
impl ExternalSubAgentRunner for GroupMemberRunner {
    /// `@name`, as given: whether it names a member or an agent is the
    /// group's to say when the task runs. Anything else is not a group
    /// address.
    fn resolve_agent(&self, prefix: &str) -> Option<String> {
        let name = prefix.trim().strip_prefix('@')?.trim();
        (!name.is_empty()).then(|| format!("@{name}"))
    }

    async fn run_task(&self, request: ExternalTaskRequest) -> Result<ExternalTaskOutcome, String> {
        let target = request
            .agent_id
            .trim()
            .trim_start_matches('@')
            .trim()
            .to_string();
        let Some(session) = request
            .metadata
            .get("parent_session_id")
            .and_then(Value::as_str)
            .filter(|session| !session.is_empty())
            .or(self.session.as_deref())
        else {
            return Err(format!(
                "`@{target}` hands the task to an agent group, and this task does not know \
                 which session it is from"
            ));
        };
        let asker = MemberKey {
            agent: AgentKind::REBON.to_string(),
            session_id: session.to_string(),
        };
        let Some(group) = self.store.group_of(&asker).map_err(describe)? else {
            return Err(format!(
                "`@{target}` names a member of this session's agent group, and the session is in \
                 none. Add it to a group in Rebon (the chat's menu: 群组) and try again"
            ));
        };
        // The step's own name — the Agent call's description, the workflow
        // step's label — is what the group shows of the handoff.
        let title = ["description", "label", "workflow_label"]
            .iter()
            .find_map(|key| request.metadata.get(*key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|title| !title.is_empty());
        // Where this session works — its worktree, when it has one — so a
        // new member works on the same files.
        let cwd = request.cwd.clone().or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned())
        });
        let handed = assign::hand_on(
            &self.store,
            &group.id,
            &asker,
            &assign::Assignment {
                to: &target,
                title,
                task: &request.prompt,
                model: request.model_hint.as_deref(),
                access: None,
                cwd: cwd.as_deref(),
            },
        )
        .map_err(describe)?;
        let posted = match handed {
            Handed::Spawning { spawn_id, agent } => {
                self.started(&group.id, &asker, &spawn_id, agent, &request.cancel)
                    .await?
            }
            posted => posted,
        };
        let Handed::Posted {
            alias,
            request: request_id,
            seq,
        } = posted
        else {
            return Err("the started member was given no request".to_string());
        };
        if let Some(progress) = &request.progress {
            progress(ExternalTaskEvent::ToolCall {
                tool_call_id: request_id.clone(),
                title: format!("@{alias} · {request_id}"),
                input: Some(json!({ "group": group.name, "member": alias })),
            });
        }
        let outcome = self
            .reply(&group.id, &asker, &alias, &request_id, seq, &request.cancel)
            .await;
        if let Some(progress) = &request.progress {
            progress(ExternalTaskEvent::ToolCallUpdate {
                tool_call_id: request_id,
                title: format!("@{alias}"),
                status: Some(match &outcome {
                    Ok(outcome) if outcome.status == ExternalTaskStatus::Completed => {
                        ToolCallStatus::Completed
                    }
                    _ => ToolCallStatus::Failed,
                }),
                output: None,
            });
        }
        outcome
    }
}

/// The spawner's one external runner: `@` specs to the group, the rest to
/// the ACP pool when there is one.
pub struct ExternalRunners {
    acp: Option<Arc<AcpSubAgentPool>>,
    members: GroupMemberRunner,
}

impl ExternalRunners {
    pub fn new(acp: Option<Arc<AcpSubAgentPool>>, members: GroupMemberRunner) -> Self {
        Self { acp, members }
    }
}

/// The runner `session`'s spawner gets: its ACP pool, if it has one, and its
/// agent group.
pub fn external_runner(
    acp: Option<Arc<AcpSubAgentPool>>,
    session: &str,
) -> Arc<dyn ExternalSubAgentRunner> {
    Arc::new(ExternalRunners::new(
        acp,
        GroupMemberRunner::new(&crate::rebon_config::config_home_dir(), Some(session)),
    ))
}

#[async_trait]
impl ExternalSubAgentRunner for ExternalRunners {
    fn resolve_agent(&self, prefix: &str) -> Option<String> {
        if prefix.trim_start().starts_with('@') {
            self.members.resolve_agent(prefix)
        } else {
            self.acp.as_ref()?.resolve_agent(prefix)
        }
    }

    async fn run_task(&self, request: ExternalTaskRequest) -> Result<ExternalTaskOutcome, String> {
        if request.agent_id.starts_with('@') {
            return self.members.run_task(request).await;
        }
        match &self.acp {
            Some(pool) => pool.run_task(request).await,
            None => Err(format!("no external agent `{}`", request.agent_id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_group::assign::SpawnOutcome;
    use rebon_group::model::{Delivery, EntryKind, Member};
    use rebon_group::store::Draft;

    fn join(store: &GroupStore, group: &str, agent: &str, session: &str, alias: &str) -> MemberKey {
        store
            .join(
                group,
                Member {
                    agent: agent.into(),
                    session_id: session.into(),
                    alias: alias.into(),
                    role: None,
                    delivery: Delivery::Auto,
                    joined_at_ms: 1,
                },
            )
            .unwrap();
        MemberKey {
            agent: agent.into(),
            session_id: session.into(),
        }
    }

    fn task(agent_id: &str, parent: &str, prompt: &str) -> ExternalTaskRequest {
        ExternalTaskRequest {
            agent_id: agent_id.into(),
            model_hint: None,
            task_session_id: "subagent-1".into(),
            prompt: prompt.into(),
            cwd: None,
            cancel: PromptCancel::new(),
            progress: None,
            metadata: json!({ "parent_session_id": parent }),
        }
    }

    fn fast(store: GroupStore) -> GroupMemberRunner {
        GroupMemberRunner::with_store(store).with_timeouts(
            Duration::from_millis(10),
            Duration::from_millis(300),
            Duration::from_secs(5),
        )
    }

    /// The member's side: waits for the request to `alias`, marks it handed
    /// over as the app's wake would, and replies with `answer`.
    fn answer_when_asked(
        store: GroupStore,
        group: String,
        key: MemberKey,
        alias: &str,
        answer: &str,
    ) {
        let (alias, answer) = (alias.to_string(), answer.to_string());
        std::thread::spawn(move || {
            for _ in 0..500 {
                let log = store.entries_after(&group, 0).unwrap();
                if let Some(request) = log.iter().find(|entry| {
                    entry.kind == EntryKind::Request
                        && entry
                            .to
                            .as_deref()
                            .is_some_and(|to| to.eq_ignore_ascii_case(&alias))
                }) {
                    store.mark_delivered(&group, &key, request.seq).unwrap();
                    let asker = request.from.clone();
                    store
                        .append(
                            &group,
                            &key,
                            Draft {
                                kind: EntryKind::Reply,
                                to: Some(asker),
                                re: request.id.clone(),
                                supersedes: None,
                                text: answer,
                            },
                        )
                        .unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }

    #[test]
    fn only_group_addresses_resolve_here() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GroupMemberRunner::with_store(GroupStore::new(dir.path()));
        assert_eq!(runner.resolve_agent(" @Ada "), Some("@Ada".into()));
        assert_eq!(runner.resolve_agent("@"), None);
        assert_eq!(runner.resolve_agent("claudecode"), None);
        let both = ExternalRunners::new(None, runner);
        assert_eq!(both.resolve_agent("@codex"), Some("@codex".into()));
        assert_eq!(both.resolve_agent("llama3"), None, "no pool, no ACP agents");
    }

    #[tokio::test]
    async fn a_task_for_a_member_comes_back_as_its_reply() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        join(&store, &group.id, "rebon", "s-lead", "Lead");
        let ada = join(&store, &group.id, "claude-code", "c-ada", "Ada");
        answer_when_asked(
            store.clone(),
            group.id.clone(),
            ada,
            "Ada",
            "two bugs, in refresh()",
        );

        let outcome = fast(store.clone())
            .run_task(task("@Ada", "s-lead", "review auth.rs"))
            .await
            .unwrap();
        assert_eq!(outcome.status, ExternalTaskStatus::Completed);
        assert_eq!(outcome.final_text, "two bugs, in refresh()");

        let log = store.entries_after(&group.id, 0).unwrap();
        let request = log
            .iter()
            .find(|entry| entry.kind == EntryKind::Request)
            .unwrap();
        assert_eq!(request.from, "Lead");
        assert_eq!(request.text, "review auth.rs");
        assert!(
            store
                .awaited(&group.id)
                .unwrap()
                .contains_key(request.id.as_deref().unwrap()),
            "posted awaited, so the reply is not handed to the lead again"
        );
    }

    #[tokio::test]
    async fn a_new_member_is_asked_of_the_app_and_its_reply_is_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = join(&store, &group.id, "rebon", "s-lead", "Lead");

        // The app's side: take the request on, start "Bo", post the task.
        let app = store.clone();
        let group_id = group.id.clone();
        std::thread::spawn(move || {
            for _ in 0..500 {
                if let Some(spawn) = app.spawn_requests(&group_id).unwrap().pop() {
                    assert_eq!(spawn.agent, "codex");
                    assert!(app.claim_spawn(&group_id, &spawn.id).unwrap());
                    let bo = join(&app, &group_id, "codex", "x-bo", "Bo");
                    let request = app
                        .append_awaited(&group_id, &spawn.requested_by, "Bo", &spawn.task)
                        .unwrap();
                    app.answer_spawn(
                        &group_id,
                        &spawn.id,
                        &SpawnOutcome::Started {
                            alias: "Bo".into(),
                            session_id: "x-bo".into(),
                            request: request.id.clone().unwrap(),
                        },
                    )
                    .unwrap();
                    answer_when_asked(app.clone(), group_id.clone(), bo, "Bo", "tests written");
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let outcome = fast(store.clone())
            .run_task(task("@codex", "s-lead", "write the tests"))
            .await
            .unwrap();
        assert_eq!(outcome.final_text, "tests written");
        assert_eq!(outcome.acp_session_id.as_deref(), Some("@Bo"));
        assert!(store.spawn_requests(&group.id).unwrap().is_empty());
        let _ = lead;
    }

    #[tokio::test]
    async fn with_no_app_to_start_it_a_new_member_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        join(&store, &group.id, "rebon", "s-lead", "Lead");
        let error = fast(store.clone())
            .run_task(task("@claudecode", "s-lead", "review"))
            .await
            .unwrap_err();
        assert!(error.contains("is it running?"), "{error}");
        assert!(
            store.spawn_requests(&group.id).unwrap().is_empty(),
            "the request was withdrawn"
        );
    }

    #[tokio::test]
    async fn a_session_in_no_group_or_an_unknown_name_is_told_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let error = fast(store.clone())
            .run_task(task("@Ada", "s-lead", "x"))
            .await
            .unwrap_err();
        assert!(error.contains("is in none"), "{error}");

        join(&store, &group.id, "rebon", "s-lead", "Lead");
        join(&store, &group.id, "claude-code", "c-ada", "Ada");
        let error = fast(store)
            .run_task(task("@Zed", "s-lead", "x"))
            .await
            .unwrap_err();
        assert!(error.contains("members: Ada"), "{error}");
        assert!(error.contains("@codex"), "{error}");
    }

    #[tokio::test]
    async fn a_cancelled_task_withdraws_its_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        join(&store, &group.id, "rebon", "s-lead", "Lead");
        join(&store, &group.id, "claude-code", "c-ada", "Ada");
        let request = task("@Ada", "s-lead", "review");
        let cancel = request.cancel.clone();
        let runner = fast(store.clone());
        let running = tokio::spawn(async move { runner.run_task(request).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let outcome = running.await.unwrap().unwrap();
        assert_eq!(outcome.status, ExternalTaskStatus::Cancelled);
        let log = store.entries_after(&group.id, 0).unwrap();
        let id = log
            .iter()
            .find(|entry| entry.kind == EntryKind::Request)
            .and_then(|entry| entry.id.clone())
            .unwrap();
        assert!(!store.awaited(&group.id).unwrap().contains_key(&id));
        assert!(log
            .iter()
            .any(|entry| entry.kind == EntryKind::Note && entry.text.contains("withdrawn")));
    }

    #[tokio::test]
    async fn a_request_no_one_picks_up_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        join(&store, &group.id, "rebon", "s-lead", "Lead");
        join(&store, &group.id, "claude-code", "c-ada", "Ada");
        let runner = GroupMemberRunner::with_store(store).with_timeouts(
            Duration::from_millis(10),
            Duration::from_millis(300),
            Duration::from_millis(200),
        );
        let error = runner
            .run_task(task("@Ada", "s-lead", "review"))
            .await
            .unwrap_err();
        assert!(error.contains("was not handed"), "{error}");
    }
}
