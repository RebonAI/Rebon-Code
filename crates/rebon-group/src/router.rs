//! The group the Rebon router's workers share in a project.
//!
//! Every worker the router dispatches into a project joins that project's
//! group named [`GROUP_NAME`], made the first time one is needed. What a
//! worker keeps with `group_remember` is then in the briefing of every
//! worker after it, so what one task found out — a convention, a gotcha
//! about the build, a decision and its reason — reaches the next task in the
//! project without the router having to carry it, and without the next
//! worker finding it out again.
//!
//! A worker joins in its own process, once its session exists and before
//! its first turn reads the group inbox; it leaves when its job is over
//! ([`leave_finished`]), and what it kept stays. Only members that joined
//! this way, marked by [`WORKER_ROLE`], are ever removed: someone the person
//! put in the group stays.

use anyhow::Result;

use crate::identity::AgentKind;
use crate::model::{Delivery, Group, Member, MemberKey};
use crate::store::{now_ms, GroupStore};

/// The name of a project's router group.
pub const GROUP_NAME: &str = "router";
/// The role a router worker joins with, followed by its task's title.
pub const WORKER_ROLE: &str = "router worker";
/// How much of a task's title the role carries.
const ROLE_TITLE_CHARS: usize = 60;
/// How many words of the title make the alias.
const ALIAS_WORDS: usize = 3;
const ALIAS_MAX_CHARS: usize = 24;

/// Whether `member` joined as one of the router's workers.
pub fn is_worker(member: &Member) -> bool {
    member.agent == AgentKind::REBON
        && member.role.as_deref().is_some_and(|role| {
            role == WORKER_ROLE
                || role
                    .strip_prefix(WORKER_ROLE)
                    .is_some_and(|rest| rest.starts_with(": "))
        })
}

/// Puts the Rebon session `session_id`, working on the task `title`, in the
/// router group of the project at `project`, making the group when the
/// project has none. Returns the group it is in, or `None` when it is in
/// another group already (the person put it there; that one stands) or Rebon
/// is switched out of groups.
pub fn join_worker(
    store: &GroupStore,
    project: &str,
    session_id: &str,
    title: &str,
) -> Result<Option<Group>> {
    if !store.agent_policy()?.allows(AgentKind::REBON) {
        return Ok(None);
    }
    let key = MemberKey {
        agent: AgentKind::REBON.to_string(),
        session_id: session_id.to_string(),
    };
    if let Some(group) = store.group_of(&key)? {
        let ours = group.name.eq_ignore_ascii_case(GROUP_NAME)
            && group.member(&key).is_some_and(is_worker);
        return Ok(ours.then_some(group));
    }
    let group = match store.find(project, GROUP_NAME)? {
        Some(group) => group,
        // Two workers starting together may both find none; the second
        // create is refused and finds the first one's.
        None => match store.create(GROUP_NAME, project) {
            Ok(group) => group,
            Err(error) => store.find(project, GROUP_NAME)?.ok_or(error)?,
        },
    };
    let role = format!("{WORKER_ROLE}: {}", clip(title.trim(), ROLE_TITLE_CHARS));
    // An alias taken between reading the members and joining is retried
    // against the members as they are then.
    let mut last_error = None;
    for _ in 0..3 {
        let current = store.load(&group.id)?;
        let member = Member {
            agent: AgentKind::REBON.to_string(),
            session_id: session_id.to_string(),
            alias: free_alias(&current, title),
            role: Some(role.clone()),
            delivery: Delivery::Auto,
            joined_at_ms: now_ms(),
        };
        match store.join(&group.id, member) {
            Ok((group, _)) => return Ok(Some(group)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("three attempts were made"))
}

/// Takes out of every router group the workers `still_working` says are no
/// longer at work, by their session. Returns how many left.
pub fn leave_finished(store: &GroupStore, still_working: &dyn Fn(&str) -> bool) -> Result<usize> {
    let mut left = 0;
    for group in store.list()? {
        if !group.name.eq_ignore_ascii_case(GROUP_NAME) {
            continue;
        }
        for member in group.members.iter().filter(|member| is_worker(member)) {
            if still_working(&member.session_id) {
                continue;
            }
            if store.leave(&group.id, &member.key())?.is_some() {
                left += 1;
            }
        }
    }
    Ok(left)
}

/// A short alias from the first words of `title`, numbered when another
/// member has it.
fn free_alias(group: &Group, title: &str) -> String {
    let words: Vec<String> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(ALIAS_WORDS)
        .map(str::to_ascii_lowercase)
        .collect();
    let mut base = words.join("-");
    base.truncate(ALIAS_MAX_CHARS);
    let base = base.trim_end_matches('-').to_string();
    let base = if base.is_empty() || base == "all" || base == "user" {
        "worker".to_string()
    } else {
        base
    };
    let taken = |alias: &str| group.member_by_alias(alias).is_some();
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|alias| !taken(alias))
        .expect("some number is free")
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{}…", head.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryKind;
    use crate::store::Draft;

    fn setup() -> (tempfile::TempDir, GroupStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path().join("groups"));
        (dir, store)
    }

    #[test]
    fn the_first_worker_makes_the_group_and_the_next_joins_it() {
        let (_dir, store) = setup();
        let first = join_worker(&store, "/w/app", "s1", "Fix the login redirect")
            .unwrap()
            .unwrap();
        let second = join_worker(&store, "/w/app", "s2", "Fix the login redirect again")
            .unwrap()
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.name, GROUP_NAME);
        let aliases: Vec<&str> = second.members.iter().map(|m| m.alias.as_str()).collect();
        assert_eq!(aliases, ["fix-the-login", "fix-the-login-2"]);
        assert_eq!(
            second.members[0].role.as_deref(),
            Some("router worker: Fix the login redirect")
        );
        assert!(second.members.iter().all(is_worker));
        // Another project has a group of its own.
        let other = join_worker(&store, "/w/site", "s3", "Docs")
            .unwrap()
            .unwrap();
        assert_ne!(other.id, first.id);
    }

    #[test]
    fn joining_again_changes_nothing_and_a_group_the_person_chose_stands() {
        let (_dir, store) = setup();
        let group = join_worker(&store, "/w/app", "s1", "Task")
            .unwrap()
            .unwrap();
        let again = join_worker(&store, "/w/app", "s1", "Task")
            .unwrap()
            .unwrap();
        assert_eq!(again.members.len(), 1);
        assert_eq!(again.id, group.id);

        let chosen = store.create("refactor", "/w/app").unwrap();
        store
            .join(
                &chosen.id,
                Member {
                    agent: AgentKind::REBON.into(),
                    session_id: "s9".into(),
                    alias: "planner".into(),
                    role: None,
                    delivery: Delivery::Auto,
                    joined_at_ms: 1,
                },
            )
            .unwrap();
        assert_eq!(join_worker(&store, "/w/app", "s9", "Task").unwrap(), None);
        assert_eq!(store.load(&group.id).unwrap().members.len(), 1);
    }

    #[test]
    fn finished_workers_leave_and_what_they_kept_stays() {
        let (_dir, store) = setup();
        let group = join_worker(&store, "/w/app", "s1", "One").unwrap().unwrap();
        join_worker(&store, "/w/app", "s2", "Two").unwrap();
        let key = |session: &str| MemberKey {
            agent: AgentKind::REBON.into(),
            session_id: session.into(),
        };
        store
            .append(
                &group.id,
                &key("s1"),
                Draft {
                    kind: EntryKind::Memory,
                    to: None,
                    re: None,
                    supersedes: None,
                    text: "Run the app suite with --skip hosted_sessions".into(),
                },
            )
            .unwrap();
        // Someone the person added is never removed.
        store
            .join(
                &group.id,
                Member {
                    agent: AgentKind::CLAUDE_CODE.into(),
                    session_id: "cc".into(),
                    alias: "reviewer".into(),
                    role: Some("reviews".into()),
                    delivery: Delivery::Auto,
                    joined_at_ms: 1,
                },
            )
            .unwrap();

        let left = leave_finished(&store, &|session| session == "s2").unwrap();
        assert_eq!(left, 1);
        let members: Vec<String> = store
            .load(&group.id)
            .unwrap()
            .members
            .into_iter()
            .map(|m| m.session_id)
            .collect();
        assert_eq!(members, ["s2", "cc"]);
        let memory = store.memory(&group.id).unwrap();
        assert_eq!(memory.len(), 1);
        assert_eq!(leave_finished(&store, &|_| true).unwrap(), 0);
    }

    #[test]
    fn an_alias_is_short_and_never_reserved() {
        let group = Group {
            id: "g".into(),
            name: GROUP_NAME.into(),
            cwd: "/w".into(),
            created_at_ms: 1,
            members: Vec::new(),
            warmth: Default::default(),
        };
        assert_eq!(
            free_alias(&group, "Check: Add a very long title"),
            "check-add-a"
        );
        assert_eq!(free_alias(&group, "用户 界面"), "worker");
        assert_eq!(free_alias(&group, "All"), "worker");
        assert_eq!(
            free_alias(&group, "Supercalifragilisticexpialidocious-word"),
            "supercalifragilisticexpi"
        );
    }
}
