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

/// Short names for ACP agents whose registry ids say more than people do.
/// A name that is the id itself, or the id without an `-acp`, `-cli`,
/// `-code` or `-agent` ending (`@qwen` for `qwen-code`, `@pi` for
/// `pi-acp`), needs no entry here.
const ACP_ALIASES: &[(&str, &str)] = &[
    ("copilot", "github-copilot-cli"),
    ("droid", "factory-droid"),
    ("vibe", "mistral-vibe"),
    ("grok", "grok-build"),
];

/// The endings an ACP agent's id may be named without.
const ACP_ID_ENDINGS: &[&str] = &["acp", "cli", "code", "agent"];

/// `name` as names are compared: lower case, without `-`, `_`, `.` or
/// spaces.
fn fold(name: &str) -> String {
    name.trim()
        .trim_start_matches('@')
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// A folded id without the endings it may be named without.
fn stem(folded: &str) -> &str {
    let mut stem = folded;
    while let Some(shorter) = ACP_ID_ENDINGS
        .iter()
        .find_map(|ending| stem.strip_suffix(ending).filter(|rest| !rest.is_empty()))
    {
        stem = shorter;
    }
    stem
}

/// Google Antigravity's provider allows no client in front of its own CLI,
/// so Rebon never starts it as a member, by any name.
fn is_antigravity(folded: &str) -> bool {
    folded.contains("antigravity") || folded == "agy"
}

/// The ACP agent among `declared` (the `acpAgents` ids in Rebon's config)
/// that `name` names: its id, an alias of it ([`ACP_ALIASES`]), or its id
/// without an ending ([`ACP_ID_ENDINGS`]). `None` when it names none of
/// them; an error for Google Antigravity, or a name two of them answer to.
pub fn acp_agent_named<'a>(name: &str, declared: &'a [String]) -> Result<Option<&'a str>> {
    let folded = fold(name);
    let antigravity = || {
        anyhow::anyhow!(
            "Rebon does not start Google Antigravity as a member: its provider allows only \
             its own CLI"
        )
    };
    if folded.is_empty() {
        return Ok(None);
    }
    if is_antigravity(&folded) {
        return Err(antigravity());
    }
    let by_id = |id: &str| declared.iter().find(|declared| fold(declared) == fold(id));
    let found = by_id(&folded).or_else(|| {
        ACP_ALIASES
            .iter()
            .find(|(alias, _)| *alias == folded)
            .and_then(|(_, id)| by_id(id))
    });
    let found = match found {
        Some(id) => Some(id),
        None => {
            let stemmed: Vec<&String> = declared
                .iter()
                .filter(|id| stem(&fold(id)) == stem(&folded))
                .collect();
            match stemmed.as_slice() {
                [] => None,
                [id] => Some(*id),
                many => bail!(
                    "`@{}` could be any of {}; name one by its id",
                    name.trim().trim_start_matches('@'),
                    many.iter()
                        .map(|id| format!("`@{id}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    match found {
        Some(id) if is_antigravity(&fold(id)) => Err(antigravity()),
        found => Ok(found.map(String::as_str)),
    }
}

/// What a new member can be, as an asker names it: `@rebon`,
/// `@claudecode`, `@codex`, and each ACP agent in `declared` but Google
/// Antigravity.
pub fn new_member_names(declared: &[String]) -> Vec<String> {
    ["@rebon", "@claudecode", "@codex"]
        .into_iter()
        .map(str::to_string)
        .chain(acp_member_names(declared))
        .collect()
}

/// The ACP agents among `declared` a new member can run on, as an asker
/// names them (`@gemini`): all but Google Antigravity.
pub fn acp_member_names(declared: &[String]) -> Vec<String> {
    declared
        .iter()
        .filter(|id| !is_antigravity(&fold(id)))
        .map(|id| format!("@{id}"))
        .collect()
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
    /// The ACP agent (an `acpAgents` id) a new Rebon member runs its turns
    /// on in place of Rebon's own engine — `@gemini`. Only with `agent`
    /// `rebon`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_agent: Option<String>,
    pub at_ms: u64,
}

/// How a new member is to run ([`GroupStore::request_spawn`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct SpawnOptions<'a> {
    pub model: Option<&'a str>,
    pub access: Option<Access>,
    pub cwd: Option<&'a str>,
    /// For a Rebon member: the ACP agent it runs on.
    pub acp_agent: Option<&'a str>,
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
        let acp_agent = options.acp_agent.map(str::trim).filter(|id| !id.is_empty());
        if acp_agent.is_some() && agent != AgentKind::REBON {
            bail!("only a Rebon member runs on an ACP agent");
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
            acp_agent: acp_agent.map(str::to_string),
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
    /// ([`SpawnRequest`]); see [`spawn_state`]. `agent` is what it runs:
    /// `codex`, or the ACP agent a Rebon member runs on (`gemini`).
    Spawning { spawn_id: String, agent: String },
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
    /// The ACP agents Rebon's config declares (`acpAgents` ids), which `to`
    /// can name for a new Rebon member that runs on one: `@gemini`.
    pub acp_agents: &'a [String],
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
    // A member's alias, then the agents Rebon starts itself, then an ACP
    // agent the config declares, which a Rebon member runs on.
    let (agent, acp_agent) = match spawnable_agent(target) {
        Some(agent) => (agent, None),
        None => match acp_agent_named(target, assignment.acp_agents)? {
            Some(id) => (AgentKind::REBON, Some(id)),
            None => {
                let others: Vec<&str> = group
                    .members
                    .iter()
                    .filter(|member| member.key() != *asker)
                    .map(|member| member.alias.as_str())
                    .collect();
                bail!(
                    "no member called `{target}` in group `{}` (members: {}); a new member is \
                     one of {}",
                    group.name,
                    if others.is_empty() {
                        "none besides you".to_string()
                    } else {
                        others.join(", ")
                    },
                    new_member_names(assignment.acp_agents).join(", ")
                );
            }
        },
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
            acp_agent,
        },
    )?;
    Ok(Handed::Spawning {
        spawn_id: spawn.id,
        agent: acp_agent.unwrap_or(agent).to_string(),
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

/// A member's turn as its answer, for a member that has no `group_send` —
/// a Rebon session whose turns an ACP agent runs: `text`, the turn's last
/// message, is posted as its reply to each request put to it alone that it
/// had been handed ([`handed_over`]) and had not answered. A request it was
/// never shown is left for a turn that shows it. Returns the replies; none
/// when it is in no group, `text` is empty, or nothing is open.
pub fn answer_with_turn(
    store: &GroupStore,
    member: &MemberKey,
    text: &str,
) -> Result<Vec<crate::model::Entry>> {
    use crate::model::EntryKind;
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let Some(group) = store.group_of(member)? else {
        return Ok(Vec::new());
    };
    let Some(alias) = group.member(member).map(|me| me.alias.clone()) else {
        return Ok(Vec::new());
    };
    let cursor = store.cursor(&group.id, member)?;
    let shown = cursor.delivered.max(cursor.read);
    let log = store.entries_after(&group.id, 0)?;
    let answered: std::collections::HashSet<&str> = log
        .iter()
        .filter(|entry| entry.kind == EntryKind::Reply && entry.from.eq_ignore_ascii_case(&alias))
        .filter_map(|entry| entry.re.as_deref())
        .collect();
    let open: Vec<(String, String)> = log
        .iter()
        .filter(|entry| {
            entry.kind == EntryKind::Request
                && entry.seq <= shown
                && entry
                    .to
                    .as_deref()
                    .is_some_and(|to| to.eq_ignore_ascii_case(&alias))
        })
        .filter_map(|entry| Some((entry.id.clone()?, entry.from.clone())))
        .filter(|(id, _)| !answered.contains(id.as_str()))
        .collect();
    let mut replies = Vec::with_capacity(open.len());
    for (request, asker) in open {
        replies.push(store.append(
            &group.id,
            member,
            crate::store::Draft {
                kind: EntryKind::Reply,
                to: Some(asker),
                re: Some(request),
                supersedes: None,
                text: text.to_string(),
            },
        )?);
    }
    Ok(replies)
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
    use crate::model::{Delivery, EntryKind, Member};
    use crate::render::AnswerBy;

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
                    acp_agent: None,
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
        let text = crate::render::task_prompt(&group, "Ada", &request, None, AnswerBy::GroupSend);
        assert!(text.contains("You are Ada"), "{text}");
        assert!(text.contains(&format!("Lead asks you ({id})")), "{text}");
        assert!(
            text.contains("Review auth.rs:\n- refresh()\n- login()"),
            "{text}"
        );
        assert!(text.contains(&format!("re {id}, to Lead")), "{text}");

        let by_message =
            crate::render::task_prompt(&group, "Ada", &request, None, AnswerBy::LastMessage);
        assert!(
            by_message.contains("Your last message is your answer"),
            "{by_message}"
        );
        assert!(
            !by_message.contains("group_send") && !by_message.contains("group_inbox"),
            "a member with no group tools is not sent to them: {by_message}"
        );
        assert!(by_message.contains("`Blocked:`"), "the same answer form");

        let long = "x".repeat(crate::render::TASK_CHARS + 10);
        let request = store
            .append_awaited(&group.id, &lead, "Ada", &long)
            .unwrap();
        let text = crate::render::task_prompt(&group, "Ada", &request, None, AnswerBy::GroupSend);
        assert!(
            text.contains("read it whole with group_inbox"),
            "a long task is cut"
        );
        assert!(text.chars().count() < crate::render::TASK_CHARS + 1000);
        let whole =
            crate::render::task_prompt(&group, "Ada", &request, None, AnswerBy::LastMessage);
        assert!(
            whole.contains(&long),
            "handed whole where it cannot be read again"
        );
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
            format!("{error}").ends_with("a new member is one of @rebon, @claudecode, @codex"),
            "{error}"
        );
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_turn_answers_the_requests_its_member_was_shown_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "claude-code", "c-lead", "Lead");
        let ada = member(&store, &group.id, "rebon", "s-ada", "Ada");
        let asked = store
            .append_awaited(&group.id, &lead, "Ada", "count the lines")
            .unwrap();
        let id = asked.id.clone().unwrap();

        assert!(
            answer_with_turn(&store, &ada, "Done: 42")
                .unwrap()
                .is_empty(),
            "a request it was never shown waits for a turn that shows it"
        );
        store.mark_delivered(&group.id, &ada, asked.seq).unwrap();
        let later = store
            .append_awaited(&group.id, &lead, "Ada", "and the words")
            .unwrap();
        assert!(
            answer_with_turn(&store, &ada, "  ").unwrap().is_empty(),
            "a turn with nothing to say answers nothing"
        );

        let replies = answer_with_turn(&store, &ada, " Done: 42 lines ").unwrap();
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0].kind, EntryKind::Reply);
        assert_eq!(replies[0].from, "Ada");
        assert_eq!(replies[0].to.as_deref(), Some("Lead"));
        assert_eq!(replies[0].re.as_deref(), Some(id.as_str()));
        assert_eq!(replies[0].text, "Done: 42 lines");
        let (reply, _) = reply_to(&store, &group.id, &id, asked.seq).unwrap();
        assert_eq!(
            reply.map(|reply| reply.text).as_deref(),
            Some("Done: 42 lines")
        );
        assert!(
            answer_with_turn(&store, &ada, "again").unwrap().is_empty(),
            "answered once"
        );

        store.mark_delivered(&group.id, &ada, later.seq).unwrap();
        let replies = answer_with_turn(&store, &ada, "Done: 7 words").unwrap();
        assert_eq!(
            replies
                .iter()
                .map(|reply| reply.re.clone().unwrap())
                .collect::<Vec<_>>(),
            [later.id.clone().unwrap()]
        );

        // The user's request is answered the same way; one to everyone, or
        // to another member, is not Ada's alone to answer.
        let from_user = store
            .post_as_user(
                &group.id,
                crate::store::Draft {
                    kind: EntryKind::Request,
                    to: Some("Ada".into()),
                    re: None,
                    supersedes: None,
                    text: "and you?".into(),
                },
            )
            .unwrap();
        let to_all = store
            .append(
                &group.id,
                &lead,
                crate::store::Draft {
                    kind: EntryKind::Request,
                    to: None,
                    re: None,
                    supersedes: None,
                    text: "anyone?".into(),
                },
            )
            .unwrap();
        store.mark_delivered(&group.id, &ada, to_all.seq).unwrap();
        let replies = answer_with_turn(&store, &ada, "Done: fine").unwrap();
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0].re, from_user.id);
        assert_eq!(replies[0].to.as_deref(), Some(crate::model::USER));

        let stranger = MemberKey {
            agent: "rebon".into(),
            session_id: "s-nobody".into(),
        };
        assert!(answer_with_turn(&store, &stranger, "Done")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_member_with_no_group_tools_is_woken_with_everything_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "claude-code", "c-lead", "Lead");
        member(&store, &group.id, "rebon", "s-ada", "Ada");
        let long = format!("Review auth.rs\n\n{}", "detail ".repeat(100));
        let request = store
            .append_awaited(&group.id, &lead, "Ada", &long)
            .unwrap();
        let group = store.load(&group.id).unwrap();

        let text =
            crate::render::last_message_prompt(&group, "Ada", std::slice::from_ref(&request))
                .unwrap();
        assert!(text.contains("You are Ada"), "{text}");
        assert!(
            text.contains(&format!(
                "Lead asks you ({}):\n\n{}",
                request.id.as_deref().unwrap(),
                long.trim()
            )),
            "whole, not a summary line: {text}"
        );
        assert!(text.contains("Your last message is your answer"), "{text}");
        assert!(!text.contains("group_inbox") && !text.contains("group_send"));

        let note = crate::model::Entry {
            kind: EntryKind::Memory,
            ..request.clone()
        };
        assert_eq!(
            crate::render::last_message_prompt(&group, "Ada", &[note]),
            None,
            "nothing that wakes a member"
        );
    }

    #[test]
    fn an_acp_agent_answers_to_its_id_an_alias_or_its_short_name() {
        let declared = ids(&[
            "gemini",
            "qwen-code",
            "pi-acp",
            "github-copilot-cli",
            "glm-acp-agent",
        ]);
        let named = |name: &str| acp_agent_named(name, &declared).unwrap();
        assert_eq!(named("@gemini"), Some("gemini"));
        assert_eq!(named("Gemini"), Some("gemini"), "case is not the name");
        assert_eq!(named("gemini-cli"), Some("gemini"), "an ending more");
        assert_eq!(named("qwen"), Some("qwen-code"), "an ending fewer");
        assert_eq!(named("Qwen Code"), Some("qwen-code"));
        assert_eq!(named("pi"), Some("pi-acp"));
        assert_eq!(named("copilot"), Some("github-copilot-cli"), "an alias");
        assert_eq!(
            named("glm"),
            Some("glm-acp-agent"),
            "endings one after another"
        );
        assert_eq!(named("cursor"), None, "not declared");
        assert_eq!(named("@"), None);
        assert_eq!(
            acp_agent_named("copilot", &ids(&["gemini"])).unwrap(),
            None,
            "an alias of an agent that is not declared"
        );
    }

    #[test]
    fn google_antigravity_is_never_a_member() {
        let declared = ids(&["antigravity-acp", "gemini"]);
        for name in [
            "antigravity",
            "@antigravity-acp",
            "agy",
            "Google Antigravity",
        ] {
            let error = acp_agent_named(name, &declared).unwrap_err();
            assert!(
                format!("{error}").contains("Antigravity"),
                "{name}: {error}"
            );
        }
        assert!(
            acp_agent_named("agy", &[]).is_err(),
            "refused whether or not it is declared"
        );
        assert_eq!(
            new_member_names(&declared),
            ["@rebon", "@claudecode", "@codex", "@gemini"],
            "never offered"
        );
    }

    #[test]
    fn a_short_name_two_agents_answer_to_is_refused() {
        let declared = ids(&["kimi", "kimi-code"]);
        assert_eq!(
            acp_agent_named("kimi", &declared).unwrap(),
            Some("kimi"),
            "the id itself comes first"
        );
        let error = acp_agent_named("kimi-cli", &declared).unwrap_err();
        assert!(
            format!("{error}").contains("`@kimi`, `@kimi-code`"),
            "{error}"
        );
    }

    #[test]
    fn a_task_for_an_acp_agent_asks_for_a_rebon_member_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");
        let declared = ids(&["qwen-code"]);

        let handed = hand_on(
            &store,
            &group.id,
            &lead,
            &Assignment {
                to: "@qwen",
                task: "review auth.rs",
                access: Some(Access::Read),
                acp_agents: &declared,
                ..Assignment::default()
            },
        )
        .unwrap();
        let Handed::Spawning { spawn_id, agent } = handed else {
            panic!("a new member: {handed:?}");
        };
        assert_eq!(agent, "qwen-code", "named for what it runs");
        let requests = store.spawn_requests(&group.id).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].id, spawn_id);
        assert_eq!(requests[0].agent, AgentKind::REBON);
        assert_eq!(requests[0].acp_agent.as_deref(), Some("qwen-code"));
        assert_eq!(requests[0].access, Some(Access::Read));

        // The agents Rebon starts itself come before an ACP agent of the
        // same name, and a member before either.
        let codex = hand_on(
            &store,
            &group.id,
            &lead,
            &Assignment {
                to: "@codex",
                task: "x",
                acp_agents: &ids(&["codex"]),
                ..Assignment::default()
            },
        )
        .unwrap();
        assert!(matches!(codex, Handed::Spawning { ref agent, .. } if agent == AgentKind::CODEX));
        let error = hand_on(
            &store,
            &group.id,
            &lead,
            &Assignment {
                to: "@agy",
                task: "x",
                acp_agents: &declared,
                ..Assignment::default()
            },
        )
        .unwrap_err();
        assert!(format!("{error}").contains("Antigravity"), "{error}");
        let error = hand_on(
            &store,
            &group.id,
            &lead,
            &Assignment {
                to: "@cursor",
                task: "x",
                acp_agents: &declared,
                ..Assignment::default()
            },
        )
        .unwrap_err();
        assert!(
            format!("{error}").ends_with("one of @rebon, @claudecode, @codex, @qwen-code"),
            "{error}"
        );
    }

    #[test]
    fn only_a_rebon_member_runs_on_an_acp_agent() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("crew", "C:/p").unwrap();
        let lead = member(&store, &group.id, "rebon", "s-lead", "Lead");
        let options = SpawnOptions {
            acp_agent: Some("gemini"),
            ..SpawnOptions::default()
        };
        assert!(store
            .request_spawn(&group.id, "codex", &lead, "x", options)
            .is_err());
        let request = store
            .request_spawn(&group.id, "rebon", &lead, "x", options)
            .unwrap();
        assert_eq!(request.acp_agent.as_deref(), Some("gemini"));
        let blank = SpawnOptions {
            acp_agent: Some(" "),
            ..SpawnOptions::default()
        };
        assert_eq!(
            store
                .request_spawn(&group.id, "rebon", &lead, "x", blank)
                .unwrap()
                .acp_agent,
            None
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
