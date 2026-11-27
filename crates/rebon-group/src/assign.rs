//! Work one member hands another and waits for.
//!
//! A member can hand a task to another in the middle of its own turn — a
//! Rebon workflow step addressed to `@Ada`, say — and wait there for the
//! answer. It posts the task as an *awaited* request
//! ([`GroupStore::append_awaited`]); the reply then comes back through that
//! wait, and the channels that hand a member what was written to it (the app
//! waking it, the attachment poller, the hook) leave the reply out
//! ([`crate::deliver::pending`]).
//!
//! It can also hand a task to a member that does not exist yet:
//! `@claudecode`, a new Claude Code session on the task. Only the app can
//! start sessions, so the asking side writes a [`SpawnRequest`] and the app
//! answers it with a [`SpawnOutcome`] — the alias it gave the new member and
//! the awaited request it posted for it, or why it could not:
//!
//! ```text
//! <group>/awaits.json                 request id → the alias waiting on it
//! <group>/spawns/<id>.json            a member asked for a new one
//! <group>/spawns/<id>.claimed.json    the app is starting it
//! <group>/spawns/<id>.done.json       what the app did
//! <group>/main.json                   the member that leads the group
//! ```
//!
//! A request the app has not claimed can be withdrawn
//! ([`GroupStore::withdraw_spawn`]); one it has claimed is answered.
//!
//! [`hand_on`], [`spawn_state`] and [`reply_to`] are the steps of handing a
//! task on and waiting for its answer, for whichever host waits — a Rebon
//! session's sub-agent runner, an orchestration tool another agent calls —
//! each with its own clock. The group's *main agent* ([`GroupStore::set_main`])
//! is the member that hands the others their work; a host can keep its
//! orchestration tools for it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity::AgentKind;
use crate::model::MemberKey;
use crate::store::{now_ms, write_json, GroupStore};

const SPAWNS_DIR: &str = "spawns";
const CLAIMED: &str = ".claimed.json";
const DONE: &str = ".done.json";

/// The agents the app can start a member of.
pub const SPAWNABLE: &[&str] = &[AgentKind::REBON, AgentKind::CLAUDE_CODE, AgentKind::CODEX];

/// What a new member may do on its task, set by whoever hands it out: a
/// reviewer reads, an implementer edits. The app starts the member's agent
/// with the permission mode that matches; anything past it is refused, and
/// the member says so in its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Read and search; no changes.
    Read,
    /// Change files in the project; commands still need a yes.
    Edit,
    /// Whatever its agent's own safety checks allow.
    Full,
}

impl Access {
    /// `read`, `read-only`, `edit`, `write`, `full`, `auto`.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "read" | "read-only" | "readonly" | "ro" => Some(Self::Read),
            "edit" | "write" | "edits" => Some(Self::Edit),
            "full" | "auto" | "all" => Some(Self::Full),
            _ => None,
        }
    }

    /// For the member's first message.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Read => "read-only — read and search, change nothing",
            Self::Edit => "edit — change files in this project; commands may be refused",
            Self::Full => "full — files and commands, within your agent's own safety checks",
        }
    }
}

/// The agent a spawn names, in the group's spelling: `claudecode`,
/// `Claude-Code` and `claude` are all `claude-code`. `None` for an agent the
/// app cannot start a member of.
pub fn spawnable_agent(name: &str) -> Option<&'static str> {
    let folded: String = name
        .trim()
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .flat_map(char::to_lowercase)
        .collect();
    match folded.as_str() {
        "rebon" => Some(AgentKind::REBON),
        "claudecode" | "claude" => Some(AgentKind::CLAUDE_CODE),
        "codex" => Some(AgentKind::CODEX),
        _ => None,
    }
}

/// A member's request that the app start a new member on `task`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnRequest {
    pub id: String,
    /// One of [`SPAWNABLE`].
    pub agent: String,
    /// The model the new member should run on, when the asker named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Who asks, and so who the task is posted from and whom its reply goes
    /// to.
    pub requested_by: MemberKey,
    /// The task, the new member's first message.
    pub task: String,
    /// What the new member may do; the app's default when the asker did
    /// not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    /// Where the asker works — the group's project, or a worktree of it —
    /// so the new member works on the same files. The project when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub at_ms: u64,
}

/// How a new member is to run ([`GroupStore::request_spawn`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct SpawnOptions<'a> {
    pub model: Option<&'a str>,
    pub access: Option<Access>,
    pub cwd: Option<&'a str>,
}

/// What the app did about a [`SpawnRequest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum SpawnOutcome {
    /// The new member is in the group as `alias` and its session is
    /// starting; `request` is the awaited request its reply will name.
    #[serde(rename_all = "camelCase")]
    Started {
        alias: String,
        session_id: String,
        request: String,
    },
    /// Nothing was started.
    Refused { reason: String },
}

static NEXT_SPAWN: AtomicU64 = AtomicU64::new(0);

impl GroupStore {
    fn spawns_dir(&self, group_id: &str) -> Result<PathBuf> {
        if group_id.is_empty()
            || !group_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            bail!("`{group_id}` is not a group id");
        }
        let group = self.root().join(group_id);
        if !group.is_dir() {
            bail!("no group `{group_id}`");
        }
        Ok(group.join(SPAWNS_DIR))
    }

    /// Asks the app to start a new `agent` member on `task`, for
    /// `requested_by`, which must be a member of the group.
    pub fn request_spawn(
        &self,
        group_id: &str,
        agent: &str,
        requested_by: &MemberKey,
        task: &str,
        options: SpawnOptions<'_>,
    ) -> Result<SpawnRequest> {
        let Some(agent) = spawnable_agent(agent) else {
            bail!(
                "Rebon cannot start a `{agent}` member; it starts {}",
                SPAWNABLE.join(", ")
            );
        };
        if task.trim().is_empty() {
            bail!("nothing to ask");
        }
        let group = self.load(group_id)?;
        if group.member(requested_by).is_none() {
            bail!("this session is not a member of group `{}`", group.name);
        }
        let dir = self.spawns_dir(group_id)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let at_ms = now_ms();
        let request = SpawnRequest {
            id: format!(
                "{at_ms}-{}-{}",
                std::process::id(),
                NEXT_SPAWN.fetch_add(1, Ordering::Relaxed)
            ),
            agent: agent.to_string(),
            model: options
                .model
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string),
            requested_by: requested_by.clone(),
            task: task.to_string(),
            access: options.access,
            cwd: options
                .cwd
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string),
            at_ms,
        };
        write_json(&dir.join(format!("{}.json", request.id)), &request)?;
        Ok(request)
    }

    /// The requests no one has claimed yet, oldest first.
    pub fn spawn_requests(&self, group_id: &str) -> Result<Vec<SpawnRequest>> {
        let dir = self.spawns_dir(group_id)?;
        let mut requests = Vec::new();
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(requests),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", dir.display()));
            }
        };
        for item in read.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") || name.ends_with(CLAIMED) || name.ends_with(DONE) {
                continue;
            }
            // One being written is not finished, and is read next time.
            if let Some(request) = read_json::<SpawnRequest>(&item.path()) {
                requests.push(request);
            }
        }
        requests.sort_by(|a, b| (a.at_ms, &a.id).cmp(&(b.at_ms, &b.id)));
        Ok(requests)
    }

    /// Takes `spawn_id` on, for the app about to start it. `false` when it
    /// is not there to take: withdrawn, or claimed already.
    pub fn claim_spawn(&self, group_id: &str, spawn_id: &str) -> Result<bool> {
        let dir = self.spawns_dir(group_id)?;
        check_spawn_id(spawn_id)?;
        Ok(std::fs::rename(
            dir.join(format!("{spawn_id}.json")),
            dir.join(format!("{spawn_id}{CLAIMED}")),
        )
        .is_ok())
    }

    /// Records what was done about the claimed `spawn_id`.
    pub fn answer_spawn(
        &self,
        group_id: &str,
        spawn_id: &str,
        outcome: &SpawnOutcome,
    ) -> Result<()> {
        let dir = self.spawns_dir(group_id)?;
        check_spawn_id(spawn_id)?;
        write_json(&dir.join(format!("{spawn_id}{DONE}")), outcome)?;
        let _ = std::fs::remove_file(dir.join(format!("{spawn_id}{CLAIMED}")));
        Ok(())
    }

    /// What the app did about `spawn_id`, once it has.
    pub fn spawn_outcome(&self, group_id: &str, spawn_id: &str) -> Result<Option<SpawnOutcome>> {
        let dir = self.spawns_dir(group_id)?;
        check_spawn_id(spawn_id)?;
        Ok(read_json(&dir.join(format!("{spawn_id}{DONE}"))))
    }

    /// Whether the app has taken `spawn_id` on but not answered it yet.
    pub fn spawn_claimed(&self, group_id: &str, spawn_id: &str) -> Result<bool> {
        let dir = self.spawns_dir(group_id)?;
        check_spawn_id(spawn_id)?;
        Ok(dir.join(format!("{spawn_id}{CLAIMED}")).is_file())
    }

    /// Takes back a request the app has not claimed. `false` when it has:
    /// it is being started, and will be answered.
    pub fn withdraw_spawn(&self, group_id: &str, spawn_id: &str) -> Result<bool> {
        let dir = self.spawns_dir(group_id)?;
        check_spawn_id(spawn_id)?;
        match std::fs::remove_file(dir.join(format!("{spawn_id}.json"))) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(!dir.join(format!("{spawn_id}{CLAIMED}")).is_file())
            }
            Err(error) => Err(error).with_context(|| format!("failed to withdraw {spawn_id}")),
        }
    }

    /// Clears an answered request away, once the asker has read the answer.
    pub fn finish_spawn(&self, group_id: &str, spawn_id: &str) -> Result<()> {
        let dir = self.spawns_dir(group_id)?;
        check_spawn_id(spawn_id)?;
        for suffix in [".json", CLAIMED, DONE] {
            let _ = std::fs::remove_file(dir.join(format!("{spawn_id}{suffix}")));
        }
        Ok(())
    }

    /// Makes `key` the group's main agent: the member that hands the others
    /// their work, and the one a host's orchestration tools are for. One per
    /// group; making another one main replaces it.
    pub fn set_main(&self, group_id: &str, key: &MemberKey) -> Result<()> {
        let group = self.load(group_id)?;
        if group.member(key).is_none() {
            bail!("the main agent is one of the group's members");
        }
        write_json(&self.root().join(group_id).join(MAIN_FILE), key)
    }

    /// The group's main agent, while it is still a member.
    pub fn main_of(&self, group_id: &str) -> Result<Option<MemberKey>> {
        let group = self.load(group_id)?;
        let main: Option<MemberKey> = read_json(&self.root().join(group_id).join(MAIN_FILE));
        Ok(main.filter(|key| group.member(key).is_some()))
    }

    /// The group no longer has a main agent.
    pub fn clear_main(&self, group_id: &str) -> Result<()> {
        self.load(group_id)?;
        match std::fs::remove_file(self.root().join(group_id).join(MAIN_FILE)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).context("failed to clear the main agent")
            }
            _ => Ok(()),
        }
    }

    /// The group `key` is the main agent of.
    pub fn group_led_by(&self, key: &MemberKey) -> Result<Option<crate::model::Group>> {
        let Some(group) = self.group_of(key)? else {
            return Ok(None);
        };
        Ok((self.main_of(&group.id)?.as_ref() == Some(key)).then_some(group))
    }
}

/// `main.json`: the member that leads the group ([`GroupStore::set_main`]).
const MAIN_FILE: &str = "main.json";

/// Where a task handed on with [`hand_on`] stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handed {
    /// Posted to the member `alias` as the awaited request `request` (log
    /// entry `seq`): its reply is the answer.
    Posted {
        alias: String,
        request: String,
        seq: u64,
    },
    /// The app was asked to start a new `agent` member on it
    /// ([`SpawnRequest`]); see [`spawn_state`].
    Spawning {
        spawn_id: String,
        agent: &'static str,
    },
}

/// One task to hand on ([`hand_on`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Assignment<'a> {
    /// A member's alias, or an agent the app starts (`codex`, `claudecode`,
    /// `rebon`, with or without `@`).
    pub to: &'a str,
    /// What is handed over, in a few words: the request's first line, and
    /// what a list of the group's work shows.
    pub title: Option<&'a str>,
    /// The task itself.
    pub task: &'a str,
    /// For a new member: the model it runs on.
    pub model: Option<&'a str>,
    /// For a new member: what it may do.
    pub access: Option<Access>,
    /// For a new member: where the asker works, so it works there too.
    pub cwd: Option<&'a str>,
}

/// The text a task is posted as: its title on the first line, so a list
/// of the group's work can show it ([`crate::render::headline`]), then the
/// task.
pub fn task_text(title: Option<&str>, task: &str) -> String {
    let task = task.trim();
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) if !task.starts_with(title) => format!("{title}\n\n{task}"),
        _ => task.to_string(),
    }
}

/// Hands a task from `asker` on in its group: to the member it names, or —
/// when it names an agent the app starts — to a new member of it, on the
/// model and with the access asked for. Who answers, and when, is
/// [`spawn_state`] and [`reply_to`]'s to find out; this only posts.
pub fn hand_on(
    store: &GroupStore,
    group_id: &str,
    asker: &MemberKey,
    assignment: &Assignment<'_>,
) -> Result<Handed> {
    let target = assignment.to.trim().trim_start_matches('@').trim();
    let task = task_text(assignment.title, assignment.task);
    let group = store.load(group_id)?;
    if group.member_by_alias(target).is_some() {
        let entry = store.append_awaited(group_id, asker, target, &task)?;
        return Ok(Handed::Posted {
            alias: target.to_string(),
            request: entry.id.context("the request was given no id")?,
            seq: entry.seq,
        });
    }
    let Some(agent) = spawnable_agent(target) else {
        let others: Vec<&str> = group
            .members
            .iter()
            .filter(|member| member.key() != *asker)
            .map(|member| member.alias.as_str())
            .collect();
        bail!(
            "no member called `{target}` in group `{}` (members: {}); a new member is \
             `@rebon`, `@claudecode` or `@codex`",
            group.name,
            if others.is_empty() {
                "none besides you".to_string()
            } else {
                others.join(", ")
            }
        );
    };
    let spawn = store.request_spawn(
        group_id,
        agent,
        asker,
        &task,
        SpawnOptions {
            model: assignment.model,
            access: assignment.access,
            cwd: assignment.cwd,
        },
    )?;
    Ok(Handed::Spawning {
        spawn_id: spawn.id,
        agent,
    })
}

/// How long an answer may be when it goes back into the asker's context; the
/// rest stays in the group's log.
pub const ANSWER_CHARS: usize = 1500;

/// A reply as the asker gets it back: whole, or — past [`ANSWER_CHARS`] —
/// cut, with where the rest is.
pub fn answer_for_asker(reply: &crate::model::Entry) -> String {
    let text = reply.text.trim();
    if text.chars().count() <= ANSWER_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(ANSWER_CHARS).collect();
    format!(
        "{}…\n\n(Cut here: the whole answer is the reply to {} in the group log — \
         group_inbox has it.)",
        cut.trim_end(),
        reply.re.as_deref().unwrap_or("the request")
    )
}

/// Where a [`Handed::Spawning`] request stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnState {
    /// Not answered yet; `claimed` once the app has taken it on.
    Waiting { claimed: bool },
    /// The app started it: now a [`Handed::Posted`] like any other.
    Started(Handed),
    /// The app did not, for `reason`.
    Refused(String),
}

/// Reads what the app did about `spawn_id`, and clears an answered request
/// away.
pub fn spawn_state(store: &GroupStore, group_id: &str, spawn_id: &str) -> Result<SpawnState> {
    let Some(outcome) = store.spawn_outcome(group_id, spawn_id)? else {
        return Ok(SpawnState::Waiting {
            claimed: store.spawn_claimed(group_id, spawn_id)?,
        });
    };
    let _ = store.finish_spawn(group_id, spawn_id);
    Ok(match outcome {
        SpawnOutcome::Started { alias, request, .. } => {
            let seq = store
                .entries_after(group_id, 0)?
                .iter()
                .find(|entry| entry.id.as_deref() == Some(request.as_str()))
                .map(|entry| entry.seq)
                .with_context(|| format!("request {request} is not in the group's log"))?;
            SpawnState::Started(Handed::Posted {
                alias,
                request,
                seq,
            })
        }
        SpawnOutcome::Refused { reason } => SpawnState::Refused(reason),
    })
}

/// The reply to `request` among the entries after `after`, and the last seq
/// looked at, to look after next time.
pub fn reply_to(
    store: &GroupStore,
    group_id: &str,
    request: &str,
    after: u64,
) -> Result<(Option<crate::model::Entry>, u64)> {
    let mut seen = after;
    for entry in store.entries_after(group_id, after)? {
        seen = entry.seq;
        if entry.kind == crate::model::EntryKind::Reply && entry.re.as_deref() == Some(request) {
            return Ok((Some(entry), seen));
        }
    }
    Ok((None, seen))
}

/// Whether the member `alias` has been handed entry `seq` by any channel:
/// its delivery or read cursor is past it.
pub fn handed_over(store: &GroupStore, group_id: &str, alias: &str, seq: u64) -> bool {
    let Ok(group) = store.load(group_id) else {
        return false;
    };
    let Some(member) = group.member_by_alias(alias) else {
        return false;
    };
    store
        .cursor(group_id, &member.key())
        .is_ok_and(|cursor| cursor.delivered >= seq || cursor.read >= seq)
}

/// No one waits on `request` any more: `alias` is told, and a late reply
/// reaches the asker as any other does.
pub fn withdraw(store: &GroupStore, group_id: &str, asker: &MemberKey, alias: &str, request: &str) {
    let _ = store.stop_awaiting(group_id, request);
    let _ = store.append(
        group_id,
        asker,
        crate::store::Draft {
            kind: crate::model::EntryKind::Note,
            to: Some(alias.to_string()),
            re: None,
            supersedes: None,
            text: format!("{request} is withdrawn: no one is waiting for it now."),
        },
    );
}

/// A member started for a task no one waits on any more: once the app has
/// started it, its request stops being awaited, and it is told the task is
/// withdrawn. `false` while the app has yet to answer.
pub fn release_started(
    store: &GroupStore,
    group_id: &str,
    asker: &MemberKey,
    spawn_id: &str,
) -> bool {
    match spawn_state(store, group_id, spawn_id) {
        Ok(SpawnState::Started(Handed::Posted { alias, request, .. })) => {
            withdraw(store, group_id, asker, &alias, &request);
            true
        }
        Ok(SpawnState::Waiting { .. }) => false,
        _ => true,
    }
}

/// Spawn ids are made in [`GroupStore::request_spawn`]; one that could
/// climb out of the directory is not one of ours.
fn check_spawn_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("`{id}` is not a spawn id");
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Delivery, Member};

    fn member(
        store: &GroupStore,
        group: &str,
        agent: &str,
        session: &str,
        alias: &str,
    ) -> MemberKey {
        let key = MemberKey {
            agent: agent.into(),
            session_id: session.into(),
        };
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
        key
    }

    #[test]
    fn spawnable_agents_are_spelled_the_groups_way() {
        assert_eq!(spawnable_agent("claudecode"), Some(AgentKind::CLAUDE_CODE));
        assert_eq!(
            spawnable_agent(" Claude-Code "),
            Some(AgentKind::CLAUDE_CODE)
        );
        assert_eq!(spawnable_agent("claude"), Some(AgentKind::CLAUDE_CODE));
        assert_eq!(spawnable_agent("Codex"), Some(AgentKind::CODEX));
        assert_eq!(spawnable_agent("rebon"), Some(AgentKind::REBON));
        assert_eq!(spawnable_agent("gemini-cli"), None);
    }

    #[test]
    fn a_spawn_is_claimed_once_answered_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");

        let request = store
            .request_spawn(
                &group.id,
                "claudecode",
                &lead,
                "review auth.rs",
                SpawnOptions {
                    model: Some(" opus "),
                    access: Some(Access::Read),
                    cwd: Some("C:/p/.rebon/worktrees/bg-1"),
                },
            )
            .unwrap();
        assert_eq!(request.agent, AgentKind::CLAUDE_CODE);
        assert_eq!(request.model.as_deref(), Some("opus"));
        assert_eq!(request.cwd.as_deref(), Some("C:/p/.rebon/worktrees/bg-1"));
        assert_eq!(
            store.spawn_requests(&group.id).unwrap(),
            vec![request.clone()]
        );

        assert!(store.claim_spawn(&group.id, &request.id).unwrap());
        assert!(
            !store.claim_spawn(&group.id, &request.id).unwrap(),
            "claimed once"
        );
        assert!(store.spawn_requests(&group.id).unwrap().is_empty());
        assert!(store.spawn_claimed(&group.id, &request.id).unwrap());
        assert!(
            !store.withdraw_spawn(&group.id, &request.id).unwrap(),
            "a claimed request is answered, not withdrawn"
        );

        let outcome = SpawnOutcome::Started {
            alias: "Ada".into(),
            session_id: "c-1".into(),
            request: "r4".into(),
        };
        store
            .answer_spawn(&group.id, &request.id, &outcome)
            .unwrap();
        assert!(!store.spawn_claimed(&group.id, &request.id).unwrap());
        assert_eq!(
            store.spawn_outcome(&group.id, &request.id).unwrap(),
            Some(outcome)
        );

        store.finish_spawn(&group.id, &request.id).unwrap();
        assert_eq!(store.spawn_outcome(&group.id, &request.id).unwrap(), None);
    }

    #[test]
    fn an_unclaimed_spawn_can_be_withdrawn() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");
        let request = store
            .request_spawn(
                &group.id,
                "codex",
                &lead,
                "write the tests",
                SpawnOptions::default(),
            )
            .unwrap();
        assert!(store.withdraw_spawn(&group.id, &request.id).unwrap());
        assert!(store.spawn_requests(&group.id).unwrap().is_empty());
        assert!(!store.claim_spawn(&group.id, &request.id).unwrap());
    }

    #[test]
    fn only_a_member_asks_and_only_for_what_the_app_starts() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let stranger = MemberKey {
            agent: "rebon".into(),
            session_id: "nobody".into(),
        };
        assert!(store
            .request_spawn(&group.id, "codex", &stranger, "x", SpawnOptions::default())
            .is_err());
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");
        let error = store
            .request_spawn(&group.id, "gemini", &lead, "x", SpawnOptions::default())
            .unwrap_err();
        assert!(format!("{error}").contains("claude-code"), "{error}");
    }

    #[test]
    fn the_reply_to_an_awaited_request_is_not_handed_to_its_asker_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");
        let ada = member(&store, &group.id, "claude-code", "c-ada", "Ada");

        let request = store
            .append_awaited(&group.id, &lead, "Ada", "review auth.rs")
            .unwrap();
        let id = request.id.clone().unwrap();
        assert_eq!(
            store
                .awaited(&group.id)
                .unwrap()
                .get(&id)
                .map(String::as_str),
            Some("Lead")
        );
        // Ada is handed the request as usual.
        let pending = crate::deliver::pending(&store, &ada).unwrap();
        assert!(pending.entries.iter().any(|entry| entry.seq == request.seq));

        store
            .append(
                &group.id,
                &ada,
                crate::store::Draft {
                    kind: crate::model::EntryKind::Reply,
                    to: Some("Lead".into()),
                    re: Some(id.clone()),
                    supersedes: None,
                    text: "two bugs, both in refresh()".into(),
                },
            )
            .unwrap();
        let pending = crate::deliver::pending(&store, &lead);
        assert!(
            pending.as_ref().is_none_or(|pending| pending
                .entries
                .iter()
                .all(|entry| entry.kind != crate::model::EntryKind::Reply)),
            "the asker takes the reply through its wait: {pending:?}"
        );

        // Once it stops waiting, a reply that comes after is its to hear.
        store.stop_awaiting(&group.id, &id).unwrap();
        store
            .append(
                &group.id,
                &ada,
                crate::store::Draft {
                    kind: crate::model::EntryKind::Reply,
                    to: Some("Lead".into()),
                    re: Some(id),
                    supersedes: None,
                    text: "a third one".into(),
                },
            )
            .unwrap();
        let pending = crate::deliver::pending(&store, &lead).unwrap();
        assert!(pending
            .entries
            .iter()
            .any(|entry| entry.text == "a third one"));
    }

    #[test]
    fn a_started_member_is_told_who_waits_and_how_to_answer() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");
        member(&store, &group.id, "claude-code", "c-ada", "Ada");
        let request = store
            .append_awaited(
                &group.id,
                &lead,
                "Ada",
                "Review auth.rs:\n- refresh()\n- login()",
            )
            .unwrap();
        let id = request.id.clone().unwrap();
        let group = store.load(&group.id).unwrap();
        let text = crate::render::task_prompt(&group, "Ada", &request, None);
        assert!(text.contains("You are Ada"), "{text}");
        assert!(text.contains(&format!("Lead asks you ({id})")), "{text}");
        assert!(
            text.contains("Review auth.rs:\n- refresh()\n- login()"),
            "{text}"
        );
        assert!(text.contains(&format!("re {id}, to Lead")), "{text}");

        let long = "x".repeat(crate::render::TASK_CHARS + 10);
        let request = store
            .append_awaited(&group.id, &lead, "Ada", &long)
            .unwrap();
        let text = crate::render::task_prompt(&group, "Ada", &request, None);
        assert!(
            text.contains("read it whole with group_inbox"),
            "a long task is cut"
        );
        assert!(text.chars().count() < crate::render::TASK_CHARS + 1000);
    }

    #[test]
    fn a_group_has_one_main_agent_among_its_members() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "claude-code", "c-lead", "Lead");
        let ada = member(&store, &group.id, "rebon", "s-ada", "Ada");
        assert_eq!(store.main_of(&group.id).unwrap(), None);
        assert!(store.group_led_by(&lead).unwrap().is_none());

        store.set_main(&group.id, &lead).unwrap();
        assert_eq!(store.main_of(&group.id).unwrap(), Some(lead.clone()));
        assert_eq!(
            store.group_led_by(&lead).unwrap().map(|g| g.id),
            Some(group.id.clone())
        );
        assert!(store.group_led_by(&ada).unwrap().is_none());

        store.set_main(&group.id, &ada).unwrap();
        assert!(
            store.group_led_by(&lead).unwrap().is_none(),
            "one per group"
        );
        let stranger = MemberKey {
            agent: "codex".into(),
            session_id: "x".into(),
        };
        assert!(store.set_main(&group.id, &stranger).is_err());

        // A main agent that leaves leads nothing.
        store.leave(&group.id, &ada).unwrap();
        assert_eq!(store.main_of(&group.id).unwrap(), None);
        store.set_main(&group.id, &lead).unwrap();
        store.clear_main(&group.id).unwrap();
        assert_eq!(store.main_of(&group.id).unwrap(), None);
    }

    #[test]
    fn a_task_is_handed_to_a_member_or_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "claude-code", "c-lead", "Lead");
        let ada = member(&store, &group.id, "rebon", "s-ada", "Ada");

        let Handed::Posted {
            alias,
            request,
            seq,
        } = hand_on(
            &store,
            &group.id,
            &lead,
            &Assignment {
                to: "@Ada",
                title: Some("Count the lines"),
                task: "count the lines of auth.rs",
                ..Assignment::default()
            },
        )
        .unwrap()
        else {
            panic!("Ada is a member");
        };
        assert_eq!(alias, "Ada");
        let posted = store.entries_after(&group.id, seq - 1).unwrap();
        assert_eq!(
            posted[0].text, "Count the lines\n\ncount the lines of auth.rs",
            "the title is the request's first line"
        );
        assert_eq!(
            crate::render::headline(&posted[0].text, 60),
            "Count the lines"
        );
        assert!(!handed_over(&store, &group.id, "Ada", seq));
        store.mark_delivered(&group.id, &ada, seq).unwrap();
        assert!(handed_over(&store, &group.id, "Ada", seq));
        assert_eq!(reply_to(&store, &group.id, &request, seq).unwrap().0, None);
        store
            .append(
                &group.id,
                &ada,
                crate::store::Draft {
                    kind: crate::model::EntryKind::Reply,
                    to: Some("Lead".into()),
                    re: Some(request.clone()),
                    supersedes: None,
                    text: "42 lines".into(),
                },
            )
            .unwrap();
        let (reply, _) = reply_to(&store, &group.id, &request, seq).unwrap();
        assert_eq!(reply.map(|entry| entry.text).as_deref(), Some("42 lines"));

        let Handed::Spawning { spawn_id, agent } = hand_on(
            &store,
            &group.id,
            &lead,
            &Assignment {
                to: "codex",
                task: "write tests",
                model: Some("gpt-5"),
                access: Some(Access::Edit),
                ..Assignment::default()
            },
        )
        .unwrap() else {
            panic!("codex is an agent the app starts");
        };
        assert_eq!(agent, AgentKind::CODEX);
        let asked = store.spawn_requests(&group.id).unwrap();
        assert_eq!(asked[0].model.as_deref(), Some("gpt-5"));
        assert_eq!(asked[0].access, Some(Access::Edit));
        assert_eq!(
            spawn_state(&store, &group.id, &spawn_id).unwrap(),
            SpawnState::Waiting { claimed: false }
        );
        assert!(store.claim_spawn(&group.id, &spawn_id).unwrap());
        member(&store, &group.id, "codex", "x-bo", "Bo");
        let task = store
            .append_awaited(&group.id, &lead, "Bo", "write tests")
            .unwrap();
        store
            .answer_spawn(
                &group.id,
                &spawn_id,
                &SpawnOutcome::Started {
                    alias: "Bo".into(),
                    session_id: "x-bo".into(),
                    request: task.id.clone().unwrap(),
                },
            )
            .unwrap();
        assert_eq!(
            spawn_state(&store, &group.id, &spawn_id).unwrap(),
            SpawnState::Started(Handed::Posted {
                alias: "Bo".into(),
                request: task.id.unwrap(),
                seq: task.seq,
            })
        );

        let error = hand_on(
            &store,
            &group.id,
            &lead,
            &Assignment {
                to: "Zed",
                task: "x",
                ..Assignment::default()
            },
        )
        .unwrap_err();
        assert!(format!("{error}").contains("members: Ada, Bo"), "{error}");
        assert!(
            format!("{error}").ends_with("a new member is `@rebon`, `@claudecode` or `@codex`"),
            "{error}"
        );
    }

    #[test]
    fn a_long_answer_is_cut_where_it_goes_back_to_the_asker() {
        let reply = |text: String| crate::model::Entry {
            seq: 9,
            at_ms: 0,
            from: "Ada".into(),
            kind: crate::model::EntryKind::Reply,
            to: Some("Lead".into()),
            id: None,
            re: Some("r8".into()),
            supersedes: None,
            text,
        };
        assert_eq!(
            answer_for_asker(&reply("Done: two bugs".into())),
            "Done: two bugs"
        );
        let long = answer_for_asker(&reply("x".repeat(ANSWER_CHARS + 50)));
        assert!(long.chars().count() < ANSWER_CHARS + 200);
        assert!(long.contains("reply to r8"), "{long}");
        assert!(
            long.contains("in the group log — group_inbox has it.)"),
            "the note reads as one sentence: {long}"
        );
    }

    #[test]
    fn access_reads_the_usual_words() {
        assert_eq!(Access::parse("read-only"), Some(Access::Read));
        assert_eq!(Access::parse(" Edit "), Some(Access::Edit));
        assert_eq!(Access::parse("auto"), Some(Access::Full));
        assert_eq!(Access::parse("root"), None);
    }

    #[test]
    fn an_awaited_request_goes_to_a_member_other_than_the_asker() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");
        assert!(store.append_awaited(&group.id, &lead, "Lead", "x").is_err());
        assert!(store
            .append_awaited(&group.id, &lead, "Nobody", "x")
            .is_err());
        assert!(store
            .append_awaited(&group.id, &lead, "Lead", "  ")
            .is_err());
    }
}
