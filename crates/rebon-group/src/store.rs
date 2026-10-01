//! The group files and the lock every change is made under.
//!
//! One exclusive lock per group (`lock`, taken with `fs2` as the job store
//! does) covers `group.json`, `log.jsonl` and `cursors.json` together: a
//! join is a member and an entry, an append is an entry and a sequence
//! number, and neither may be half done when another process looks.
//! Readers take it too — it is held for a few file operations, and a reader
//! that skipped it could see a line being written.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::model::{Entry, EntryKind, Group, Handoff, Member, MemberKey, Via, Warmth, ALL, USER};

const GROUP_FILE: &str = "group.json";
const LOG_FILE: &str = "log.jsonl";
const CURSORS_FILE: &str = "cursors.json";
const DELIVERIES_FILE: &str = "deliveries.jsonl";
const MEMORY_FILE: &str = "MEMORY.md";
const LOCK_FILE: &str = "lock";

/// The groups under one root (`<config home>/groups`).
#[derive(Clone, Debug)]
pub struct GroupStore {
    root: PathBuf,
}

/// What a member has had of the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    /// The last entry pushed to it as an attachment.
    pub delivered: u64,
    /// The last entry it read from its inbox.
    pub read: u64,
    /// Whether its context has had the group's briefing: who is in it, how
    /// the memory is kept, and the memory as it stood. A new member has
    /// not; a hook that starts a fresh context asks for it again anyway.
    #[serde(default)]
    pub briefed: bool,
    /// The last memory fact its context was handed in full. Kept apart from
    /// `delivered`, which a channel that only names a request (the app
    /// typing into a terminal) moves past facts it never showed.
    #[serde(default)]
    pub memory: u64,
}

/// An entry before it has a place in the log.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub kind: EntryKind,
    pub to: Option<String>,
    pub re: Option<String>,
    pub supersedes: Option<u64>,
    pub text: String,
}

/// What a member's inbox held.
#[derive(Clone, Debug, PartialEq)]
pub struct Inbox {
    pub entries: Vec<Entry>,
    /// The log's last seq when it was read; the read cursor moves here.
    pub last_seq: u64,
}

/// `group.json` plus the bookkeeping only the store reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupFile {
    #[serde(flatten)]
    group: Group,
    #[serde(default)]
    last_seq: u64,
}

impl GroupStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// The group's memory as a document, for agents that read files.
    pub fn memory_path(&self, id: &str) -> PathBuf {
        self.dir(id).join(MEMORY_FILE)
    }

    /// Starts a group named `name` for the project at `cwd`. A project has
    /// one group of a name.
    pub fn create(&self, name: &str, cwd: &str) -> Result<Group> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a group needs a name");
        }
        if self.find(cwd, name)?.is_some() {
            bail!("this project already has a group named `{name}`");
        }
        let now = now_ms();
        let id = format!("g{now:x}{:04x}", std::process::id() & 0xffff);
        let group = Group {
            id: id.clone(),
            name: name.to_string(),
            cwd: cwd.to_string(),
            created_at_ms: now,
            members: Vec::new(),
            warmth: Warmth::default(),
        };
        std::fs::create_dir_all(self.dir(&id))
            .with_context(|| format!("failed to create {}", self.dir(&id).display()))?;
        self.with_lock(&id, || {
            write_json(
                &self.dir(&id).join(GROUP_FILE),
                &GroupFile {
                    group: group.clone(),
                    last_seq: 0,
                },
            )
        })?;
        Ok(group)
    }

    /// Every group, oldest first. A directory without a readable
    /// `group.json` is skipped: it is being created, or it is not a group.
    pub fn list(&self) -> Result<Vec<Group>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to list {}", self.root.display()));
            }
        };
        let mut groups: Vec<Group> = entries
            .flatten()
            .filter_map(|entry| read_group_file(&entry.path().join(GROUP_FILE)).ok())
            .map(|file| file.group)
            .collect();
        groups.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms).then(a.id.cmp(&b.id)));
        Ok(groups)
    }

    pub fn load(&self, id: &str) -> Result<Group> {
        check_id(id)?;
        Ok(read_group_file(&self.dir(id).join(GROUP_FILE))?.group)
    }

    /// The project's group called `name_or_id` (by name, case-insensitively,
    /// or by id).
    pub fn find(&self, cwd: &str, name_or_id: &str) -> Result<Option<Group>> {
        let wanted = name_or_id.trim();
        Ok(self.list()?.into_iter().find(|group| {
            group.id == wanted
                || (same_dir(&group.cwd, cwd) && group.name.eq_ignore_ascii_case(wanted))
        }))
    }

    /// Renames the group. A project still has one group of a name.
    pub fn rename(&self, id: &str, name: &str) -> Result<Group> {
        check_id(id)?;
        let name = name.trim();
        if name.is_empty() {
            bail!("a group needs a name");
        }
        let cwd = self.load(id)?.cwd;
        if let Some(other) = self.find(&cwd, name)? {
            if other.id != id {
                bail!("this project already has a group named `{name}`");
            }
        }
        self.with_lock(id, || {
            let dir = self.dir(id);
            let path = dir.join(GROUP_FILE);
            let mut file = read_group_file(&path)?;
            file.group.name = name.to_string();
            write_json(&path, &file)?;
            let memory = effective_memory(read_log(&dir)?);
            write_text(&dir.join(MEMORY_FILE), &render_memory(&file.group, &memory))?;
            Ok(file.group)
        })
    }

    /// Deletes the group: its members, log, cursors and memory. The sessions
    /// that were in it are untouched; they are simply in no group.
    ///
    /// `group.json` goes first, under the lock, so every reader stops seeing
    /// the group at once; the rest of the directory is removed after, and a
    /// file another process still holds open only delays that.
    pub fn delete(&self, id: &str) -> Result<()> {
        check_id(id)?;
        let dir = self.dir(id);
        self.with_lock(id, || {
            std::fs::remove_file(dir.join(GROUP_FILE))
                .with_context(|| format!("failed to remove {}", dir.join(GROUP_FILE).display()))
        })?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// The group `key` is in. A session is in at most one.
    pub fn group_of(&self, key: &MemberKey) -> Result<Option<Group>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|group| group.member(key).is_some()))
    }

    /// Adds `member` to the group and logs it. Joining a group it is already
    /// in returns that membership unchanged; being in another group, or
    /// asking for an alias someone else has, is refused.
    pub fn join(&self, id: &str, member: Member) -> Result<(Group, Option<Entry>)> {
        check_id(id)?;
        let key = member.key();
        if let Some(other) = self.group_of(&key)? {
            if other.id == id {
                return Ok((other, None));
            }
            bail!(
                "this session is already in group `{}`; leave it first",
                other.name
            );
        }
        let alias = member.alias.trim().to_string();
        if alias.is_empty() || alias.eq_ignore_ascii_case(ALL) || alias.eq_ignore_ascii_case(USER) {
            bail!("`{alias}` cannot be an alias");
        }
        self.with_lock(id, || {
            let path = self.dir(id).join(GROUP_FILE);
            let mut file = read_group_file(&path)?;
            if file.group.member_by_alias(&alias).is_some() {
                bail!("`{alias}` is taken in this group; pick another alias");
            }
            let member = Member { alias, ..member };
            let text = format!("{} joined ({})", member.alias, member.agent);
            let from = member.alias.clone();
            file.group.members.push(member);
            let entry = self.append_locked(
                id,
                &mut file,
                &from,
                Draft {
                    kind: EntryKind::Join,
                    to: None,
                    re: None,
                    supersedes: None,
                    text,
                },
            )?;
            write_json(&path, &file)?;
            // Everything already said is history, not news: a new member
            // starts with nothing unread.
            let mut cursors = read_cursors(&self.dir(id))?;
            cursors.insert(
                key.as_string(),
                Cursor {
                    delivered: entry.seq,
                    read: entry.seq,
                    ..Cursor::default()
                },
            );
            write_json(&self.dir(id).join(CURSORS_FILE), &cursors)?;
            Ok((file.group, Some(entry)))
        })
    }

    /// Takes `key` out of the group and logs it.
    pub fn leave(&self, id: &str, key: &MemberKey) -> Result<Option<Entry>> {
        check_id(id)?;
        self.with_lock(id, || {
            let path = self.dir(id).join(GROUP_FILE);
            let mut file = read_group_file(&path)?;
            let Some(index) = file.group.members.iter().position(|m| m.key() == *key) else {
                return Ok(None);
            };
            let member = file.group.members.remove(index);
            let entry = self.append_locked(
                id,
                &mut file,
                &member.alias,
                Draft {
                    kind: EntryKind::Leave,
                    to: None,
                    re: None,
                    supersedes: None,
                    text: format!("{} left", member.alias),
                },
            )?;
            write_json(&path, &file)?;
            let mut cursors = read_cursors(&self.dir(id))?;
            cursors.remove(&key.as_string());
            write_json(&self.dir(id).join(CURSORS_FILE), &cursors)?;
            Ok(Some(entry))
        })
    }

    /// Gives a member the session id it turned out to have: one the app
    /// brought in before its agent had named the session (Codex names a
    /// thread only once it has written it). Its alias, its place in the log
    /// and its cursors carry over.
    pub fn rekey_member(&self, id: &str, key: &MemberKey, session_id: &str) -> Result<Group> {
        check_id(id)?;
        let new_key = MemberKey {
            agent: key.agent.clone(),
            session_id: session_id.to_string(),
        };
        if let Some(other) = self.group_of(&new_key)? {
            bail!("session {session_id} is already in group `{}`", other.name);
        }
        self.with_lock(id, || {
            let dir = self.dir(id);
            let path = dir.join(GROUP_FILE);
            let mut file = read_group_file(&path)?;
            let Some(member) = file.group.members.iter_mut().find(|m| m.key() == *key) else {
                bail!("not a member of this group");
            };
            member.session_id = session_id.to_string();
            write_json(&path, &file)?;
            let mut cursors = read_cursors(&dir)?;
            if let Some(cursor) = cursors.remove(&key.as_string()) {
                cursors.insert(new_key.as_string(), cursor);
                write_json(&dir.join(CURSORS_FILE), &cursors)?;
            }
            // What it was handed under the old id was handed to it.
            let deliveries = dir.join(DELIVERIES_FILE);
            let mut handoffs: Vec<Handoff> = read_lines(&deliveries)?;
            let mut moved = false;
            for handoff in handoffs.iter_mut().filter(|handoff| {
                handoff.agent == key.agent && handoff.session_id == key.session_id
            }) {
                handoff.session_id = session_id.to_string();
                moved = true;
            }
            if moved {
                let mut text = String::new();
                for handoff in &handoffs {
                    text.push_str(&serde_json::to_string(handoff)?);
                    text.push('\n');
                }
                write_text(&deliveries, &text)?;
            }
            Ok(file.group)
        })
    }

    /// Changes one member's record in place — its role, how it is reached.
    /// Its key and alias stay what they are.
    pub fn update_member(
        &self,
        id: &str,
        key: &MemberKey,
        change: impl FnOnce(&mut Member),
    ) -> Result<Group> {
        check_id(id)?;
        self.with_lock(id, || {
            let path = self.dir(id).join(GROUP_FILE);
            let mut file = read_group_file(&path)?;
            let Some(member) = file.group.members.iter_mut().find(|m| m.key() == *key) else {
                bail!("not a member of this group");
            };
            let (agent, session_id, alias) = (
                member.agent.clone(),
                member.session_id.clone(),
                member.alias.clone(),
            );
            change(member);
            member.agent = agent;
            member.session_id = session_id;
            member.alias = alias;
            write_json(&path, &file)?;
            Ok(file.group)
        })
    }

    /// Writes `draft` to the log as `from`. The recipient, when there is
    /// one, must be a member; a reply must name a request.
    pub fn append(&self, id: &str, from: &MemberKey, draft: Draft) -> Result<Entry> {
        check_id(id)?;
        self.with_lock(id, || {
            let path = self.dir(id).join(GROUP_FILE);
            let mut file = read_group_file(&path)?;
            let Some(sender) = file.group.member(from).cloned() else {
                bail!(
                    "this session is not a member of group `{}`",
                    file.group.name
                );
            };
            check_recipient(&file.group, draft.to.as_deref(), true)?;
            if draft.kind == EntryKind::Reply && draft.re.is_none() {
                bail!("a reply names the request it answers (`re`)");
            }
            let entry = self.append_locked(id, &mut file, &sender.alias, draft)?;
            write_json(&path, &file)?;
            if entry.kind == EntryKind::Memory {
                let memory = effective_memory(read_log(&self.dir(id))?);
                write_text(
                    &self.dir(id).join(MEMORY_FILE),
                    &render_memory(&file.group, &memory),
                )?;
            }
            Ok(entry)
        })
    }

    /// Writes `draft` to the log as the user ([`USER`]): a note, or a request
    /// a member's reply will name. Posted from the desktop app, so no member
    /// key stands behind it.
    pub fn post_as_user(&self, id: &str, draft: Draft) -> Result<Entry> {
        check_id(id)?;
        if !matches!(draft.kind, EntryKind::Note | EntryKind::Request) {
            bail!("the user posts notes and requests");
        }
        if draft.text.trim().is_empty() {
            bail!("nothing to post");
        }
        self.with_lock(id, || {
            let path = self.dir(id).join(GROUP_FILE);
            let mut file = read_group_file(&path)?;
            check_recipient(&file.group, draft.to.as_deref(), false)?;
            let entry = self.append_locked(id, &mut file, USER, draft)?;
            write_json(&path, &file)?;
            Ok(entry)
        })
    }

    /// Appends under a lock the caller holds, and bumps `last_seq` in the
    /// caller's copy of the group file, which the caller writes back.
    fn append_locked(
        &self,
        id: &str,
        file: &mut GroupFile,
        from: &str,
        draft: Draft,
    ) -> Result<Entry> {
        let seq = file.last_seq + 1;
        let entry = Entry {
            seq,
            at_ms: now_ms(),
            from: from.to_string(),
            kind: draft.kind,
            to: draft.to.map(|to| to.trim().to_string()),
            id: (draft.kind == EntryKind::Request).then(|| format!("r{seq}")),
            re: draft.re,
            supersedes: draft.supersedes,
            text: draft.text,
        };
        let path = self.dir(id).join(LOG_FILE);
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        // A write cut off by a crash leaves a line without its end; starting
        // a fresh one keeps this entry from being glued to it and lost too.
        let mut line = if ends_mid_line(&path)? {
            "\n".to_string()
        } else {
            String::new()
        };
        line.push_str(&serde_json::to_string(&entry)?);
        line.push('\n');
        log.write_all(line.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.last_seq = seq;
        Ok(entry)
    }

    /// Every entry after `seq`, in order.
    pub fn entries_after(&self, id: &str, seq: u64) -> Result<Vec<Entry>> {
        check_id(id)?;
        self.with_lock(id, || {
            Ok(read_log(&self.dir(id))?
                .into_iter()
                .filter(|entry| entry.seq > seq)
                .collect())
        })
    }

    pub fn cursor(&self, id: &str, key: &MemberKey) -> Result<Cursor> {
        check_id(id)?;
        self.with_lock(id, || {
            Ok(read_cursors(&self.dir(id))?
                .get(&key.as_string())
                .copied()
                .unwrap_or_default())
        })
    }

    /// What `key` has not read yet, and — with `advance` — marks it read.
    pub fn inbox(&self, id: &str, key: &MemberKey, advance: bool) -> Result<Inbox> {
        check_id(id)?;
        self.with_lock(id, || {
            let dir = self.dir(id);
            let file = read_group_file(&dir.join(GROUP_FILE))?;
            let Some(me) = file.group.member(key) else {
                bail!(
                    "this session is not a member of group `{}`",
                    file.group.name
                );
            };
            let mut cursors = read_cursors(&dir)?;
            let cursor = cursors.get(&key.as_string()).copied().unwrap_or_default();
            let entries: Vec<Entry> = read_log(&dir)?
                .into_iter()
                .filter(|entry| entry.seq > cursor.read && entry.is_for(&me.alias))
                .collect();
            if advance {
                // What nothing pushed first reached it here.
                let seqs: Vec<u64> = entries
                    .iter()
                    .map(|entry| entry.seq)
                    .filter(|seq| *seq > cursor.delivered)
                    .collect();
                append_handoff(&dir, key, &me.alias, Via::Inbox, seqs)?;
            }
            if advance && file.last_seq > cursor.read {
                cursors.insert(
                    key.as_string(),
                    Cursor {
                        read: file.last_seq,
                        delivered: cursor.delivered.max(file.last_seq),
                        ..cursor
                    },
                );
                write_json(&dir.join(CURSORS_FILE), &cursors)?;
            }
            Ok(Inbox {
                entries,
                last_seq: file.last_seq,
            })
        })
    }

    /// Records that everything up to `seq` has been pushed to `key`.
    pub fn mark_delivered(&self, id: &str, key: &MemberKey, seq: u64) -> Result<()> {
        self.mark_context(id, key, seq, false, 0)
    }

    // 消息与记忆游标必须同一事务更新，避免半次投递丢失上下文。
    pub(crate) fn mark_context(
        &self,
        id: &str,
        key: &MemberKey,
        through: u64,
        briefed: bool,
        memory: u64,
    ) -> Result<()> {
        check_id(id)?;
        self.with_lock(id, || {
            let dir = self.dir(id);
            let mut cursors = read_cursors(&dir)?;
            let cursor = cursors.entry(key.as_string()).or_default();
            let next = Cursor {
                delivered: cursor.delivered.max(through),
                briefed: cursor.briefed || briefed,
                memory: cursor.memory.max(memory),
                ..*cursor
            };
            if next == *cursor {
                return Ok(());
            }
            *cursor = next;
            write_json(&dir.join(CURSORS_FILE), &cursors)
        })
    }

    /// Records that `seqs` reached `key` by `via`. Nothing is written for an
    /// empty list, or for a session that is not a member.
    pub fn record_handoff(
        &self,
        id: &str,
        key: &MemberKey,
        via: Via,
        seqs: Vec<u64>,
    ) -> Result<()> {
        check_id(id)?;
        self.with_lock(id, || {
            let dir = self.dir(id);
            let file = read_group_file(&dir.join(GROUP_FILE))?;
            match file.group.member(key) {
                Some(me) => append_handoff(&dir, key, &me.alias, via, seqs),
                None => Ok(()),
            }
        })
    }

    /// Every handoff, oldest first.
    pub fn handoffs(&self, id: &str) -> Result<Vec<Handoff>> {
        check_id(id)?;
        self.with_lock(id, || read_lines(&self.dir(id).join(DELIVERIES_FILE)))
    }

    /// Every member's cursor, keyed as [`MemberKey::as_string`].
    pub fn cursors(&self, id: &str) -> Result<BTreeMap<String, Cursor>> {
        check_id(id)?;
        self.with_lock(id, || read_cursors(&self.dir(id)))
    }

    /// The group's memory as it stands: memory entries, less the ones a
    /// later entry superseded, oldest first.
    pub fn memory(&self, id: &str) -> Result<Vec<Entry>> {
        check_id(id)?;
        self.with_lock(id, || Ok(effective_memory(read_log(&self.dir(id))?)))
    }

    fn with_lock<T>(&self, id: &str, body: impl FnOnce() -> Result<T>) -> Result<T> {
        let dir = self.dir(id);
        if !dir.is_dir() {
            bail!("no group `{id}`");
        }
        let path = dir.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        let result = body();
        let _ = FileExt::unlock(&lock);
        result
    }
}

/// Memory entries that nothing later replaced.
fn effective_memory(entries: Vec<Entry>) -> Vec<Entry> {
    let memory: Vec<Entry> = entries
        .into_iter()
        .filter(|entry| entry.kind == EntryKind::Memory)
        .collect();
    let superseded: Vec<u64> = memory.iter().filter_map(|entry| entry.supersedes).collect();
    memory
        .into_iter()
        .filter(|entry| !superseded.contains(&entry.seq))
        .collect()
}

fn render_memory(group: &Group, memory: &[Entry]) -> String {
    let mut text = format!(
        "# {} — group memory\n\nKept by Rebon from the group's log. Do not edit: this file is \
         rebuilt on every change. Add or correct a fact with the group_remember tool.\n\n",
        group.name
    );
    if memory.is_empty() {
        text.push_str("_Nothing remembered yet._\n");
    }
    for entry in memory {
        text.push_str(&format!(
            "- {} _(#{} · {})_\n",
            entry.text.trim(),
            entry.seq,
            entry.from
        ));
    }
    text
}

/// A recipient is a member's alias, `all`, or — for what an agent sends —
/// the user.
fn check_recipient(group: &Group, to: Option<&str>, user_allowed: bool) -> Result<()> {
    let Some(to) = to else {
        return Ok(());
    };
    if to.eq_ignore_ascii_case(ALL)
        || (user_allowed && to.eq_ignore_ascii_case(USER))
        || group.member_by_alias(to).is_some()
    {
        return Ok(());
    }
    bail!("no member called `{to}`; members are {}", aliases(group))
}

fn aliases(group: &Group) -> String {
    let names: Vec<&str> = group.members.iter().map(|m| m.alias.as_str()).collect();
    if names.is_empty() {
        "none yet".to_string()
    } else {
        names.join(", ")
    }
}

/// Group ids are made here and handed back by tools; one that could climb
/// out of the root is not one of ours.
fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("`{id}` is not a group id");
    }
    Ok(())
}

/// Whether two directory strings name the same directory, as far as their
/// spelling can tell: separators and a trailing one do not matter, and on
/// Windows neither does case.
pub fn same_dir(a: &str, b: &str) -> bool {
    normalize_dir(a) == normalize_dir(b)
}

/// Whether `dir` is `root` or inside it, by spelling (as [`same_dir`]).
pub fn dir_is_within(dir: &str, root: &str) -> bool {
    let (dir, root) = (normalize_dir(dir), normalize_dir(root));
    dir == root || dir.starts_with(&format!("{root}/"))
}

fn normalize_dir(dir: &str) -> String {
    let dir = dir.trim().replace('\\', "/");
    let dir = dir.trim_start_matches("//?/");
    let dir = dir.trim_end_matches('/');
    if cfg!(windows) {
        dir.to_lowercase()
    } else {
        dir.to_string()
    }
}

fn read_group_file(path: &Path) -> Result<GroupFile> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_cursors(dir: &Path) -> Result<BTreeMap<String, Cursor>> {
    let path = dir.join(CURSORS_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn append_handoff(
    dir: &Path,
    key: &MemberKey,
    alias: &str,
    via: Via,
    seqs: Vec<u64>,
) -> Result<()> {
    if seqs.is_empty() {
        return Ok(());
    }
    let handoff = Handoff {
        at_ms: now_ms(),
        agent: key.agent.clone(),
        session_id: key.session_id.clone(),
        alias: alias.to_string(),
        via,
        seqs,
    };
    let path = dir.join(DELIVERIES_FILE);
    let mut line = serde_json::to_string(&handoff)?;
    line.push('\n');
    if path.exists() && ends_mid_line(&path)? {
        line.insert(0, '\n');
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("failed to append to {}", path.display()))
}

/// The log, in order.
fn read_log(dir: &Path) -> Result<Vec<Entry>> {
    read_lines(&dir.join(LOG_FILE))
}

/// A JSON-lines file, in order. A line that does not parse — the tail of a
/// write cut off by a crash — is skipped rather than failing every reader
/// after it.
fn read_lines<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", path.display()));
        }
    };
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<T>(&line) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Whether the file's last byte is something other than a newline.
fn ends_mid_line(path: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Writes through a temporary file and a rename, so a reader never sees
/// half a file.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_text(path, &serde_json::to_string_pretty(value)?)
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
