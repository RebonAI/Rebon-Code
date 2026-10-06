//! What a group is made of: its members and the entries of its log.

use serde::{Deserialize, Serialize};

/// A group: who is in it and how they are reached. `group.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    /// The project the group works in. Only agents started in it, or in a
    /// directory under it, can join.
    pub cwd: String,
    pub created_at_ms: u64,
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub warmth: Warmth,
}

impl Group {
    pub fn member(&self, key: &MemberKey) -> Option<&Member> {
        self.members.iter().find(|member| member.key() == *key)
    }

    pub fn member_by_alias(&self, alias: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|member| member.alias.eq_ignore_ascii_case(alias))
    }
}

/// One agent in a group. A member is a session, not an agent program: two
/// Claude Code conversations are two members.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    /// Which program: `rebon`, `claude-code`, `codex`, … (see
    /// [`crate::identity::AgentKind`]).
    pub agent: String,
    pub session_id: String,
    /// What the others call it. Unique in the group, case-insensitively.
    pub alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default)]
    pub delivery: Delivery,
    pub joined_at_ms: u64,
}

impl Member {
    pub fn key(&self) -> MemberKey {
        MemberKey {
            agent: self.agent.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

/// A member's identity: the program and its session.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberKey {
    pub agent: String,
    pub session_id: String,
}

impl MemberKey {
    /// The form `cursors.json` is keyed by.
    pub fn as_string(&self) -> String {
        format!("{}:{}", self.agent, self.session_id)
    }
}

/// How new entries reach a member (RFC-0009 §8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    /// Pushed when the member is warm and idle, held while it is busy, left
    /// in its inbox while it is cold.
    #[default]
    Auto,
    /// Never pushed: the member reads its inbox when it chooses to.
    PullOnly,
}

/// When a member counts as cold: its prompt cache has most likely expired,
/// or its context is long enough that waking it costs a full re-read.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Warmth {
    pub idle_minutes: u32,
    pub context_ratio: f32,
}

impl Default for Warmth {
    /// RFC-0009 §2: an hour, and 60% of the window.
    fn default() -> Self {
        Self {
            idle_minutes: 60,
            context_ratio: 0.6,
        }
    }
}

/// One line of `log.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub seq: u64,
    pub at_ms: u64,
    /// The sender's alias at the time it wrote.
    pub from: String,
    pub kind: EntryKind,
    /// An alias, or `all`. Absent on memory, join and leave entries, which
    /// are for everyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// A request's id, `r<seq>`, which its replies name in `re`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    /// A memory entry that replaces an earlier one names its seq here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<u64>,
    pub text: String,
}

impl Entry {
    /// Whether `alias` is one of the entry's readers. A member does not get
    /// back what it wrote itself.
    pub fn is_for(&self, alias: &str) -> bool {
        if self.from.eq_ignore_ascii_case(alias) {
            return false;
        }
        match self.to.as_deref() {
            None => true,
            Some(to) => to.eq_ignore_ascii_case(ALL) || to.eq_ignore_ascii_case(alias),
        }
    }

    /// Addressed to one member by its alias rather than to everyone (or to
    /// the user, who is no member): a message to that member, not a notice
    /// the group leaves for whoever reads it.
    pub fn is_direct(&self) -> bool {
        self.to
            .as_deref()
            .is_some_and(|to| !to.eq_ignore_ascii_case(ALL) && !to.eq_ignore_ascii_case(USER))
    }
}

/// The address every member reads.
pub const ALL: &str = "all";

/// The person the agents work for, as a sender and as an address. Not a
/// member: what they write is posted from the desktop app, and what an agent
/// sends them (a reply to their request, most often) is for them to read
/// there. No member can take it as an alias.
pub const USER: &str = "user";

impl Entry {
    /// Written by the person rather than by an agent.
    pub fn is_from_user(&self) -> bool {
        self.from.eq_ignore_ascii_case(USER)
    }
}

/// Entries reaching a member, and by which way. One line of
/// `deliveries.jsonl`: the log says what was said, this says who has been
/// handed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub at_ms: u64,
    pub agent: String,
    pub session_id: String,
    /// The member's alias when it was handed them.
    pub alias: String,
    pub via: Via,
    /// The entries handed over, by seq.
    pub seqs: Vec<u64>,
}

/// The ways an entry reaches a member (RFC-0009 §8, §15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    /// Attached to the member's next turn by the `groups` plugin (Rebon).
    Attachment,
    /// Added as context by the group hook (Claude Code, Codex).
    Hook,
    /// Typed into the member's terminal by the app, which starts a turn.
    Terminal,
    /// Read by the member from its inbox, nothing having pushed it.
    Inbox,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::Hook => "hook",
            Self::Terminal => "terminal",
            Self::Inbox => "inbox",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Something the others should know.
    Note,
    /// Work asked of a member; it has an id its replies name.
    Request,
    /// The answer to a request.
    Reply,
    /// A fact the group keeps.
    Memory,
    Join,
    Leave,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Request => "request",
            Self::Reply => "reply",
            Self::Memory => "memory",
            Self::Join => "join",
            Self::Leave => "leave",
        }
    }
}
