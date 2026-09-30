use super::*;
use crate::model::{Delivery, MemberKey};

fn member(agent: &str, session_id: &str, alias: &str) -> Member {
    Member {
        agent: agent.into(),
        session_id: session_id.into(),
        alias: alias.into(),
        role: None,
        delivery: Delivery::Auto,
        joined_at_ms: 1,
    }
}

fn key(agent: &str, session_id: &str) -> MemberKey {
    MemberKey {
        agent: agent.into(),
        session_id: session_id.into(),
    }
}

fn note(to: &str, text: &str) -> Draft {
    Draft {
        kind: EntryKind::Note,
        to: Some(to.into()),
        re: None,
        supersedes: None,
        text: text.into(),
    }
}

fn memory(text: &str, supersedes: Option<u64>) -> Draft {
    Draft {
        kind: EntryKind::Memory,
        to: None,
        re: None,
        supersedes,
        text: text.into(),
    }
}

/// A group of two in a fresh store: planner (rebon) and coder (claude-code).
fn pair() -> (tempfile::TempDir, GroupStore, Group) {
    let dir = tempfile::tempdir().unwrap();
    let store = GroupStore::new(dir.path().join("groups"));
    let group = store.create("refactor", "/work/app").unwrap();
    store
        .join(&group.id, member("rebon", "s1", "planner"))
        .unwrap();
    store
        .join(&group.id, member("claude-code", "c1", "coder"))
        .unwrap();
    (dir, store, group)
}

#[test]
fn a_project_has_one_group_of_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = GroupStore::new(dir.path());
    let group = store.create("Refactor", "C:\\work\\app").unwrap();
    assert!(store.create("refactor", "C:/work/app/").is_err());
    // Another project may use the name.
    store.create("refactor", "C:/work/other").unwrap();
    assert_eq!(
        store.find("c:/work/app", "REFACTOR").unwrap().unwrap().id,
        group.id
    );
    assert_eq!(store.list().unwrap().len(), 2);
}

#[test]
fn a_member_reads_what_was_sent_to_it_or_to_all_but_not_its_own() {
    let (_dir, store, group) = pair();
    store
        .append(&group.id, &key("rebon", "s1"), note("coder", "take r1"))
        .unwrap();
    store
        .append(&group.id, &key("rebon", "s1"), note("all", "api is v2"))
        .unwrap();
    store
        .append(&group.id, &key("claude-code", "c1"), note("all", "on it"))
        .unwrap();

    let coder = store
        .inbox(&group.id, &key("claude-code", "c1"), true)
        .unwrap();
    let texts: Vec<&str> = coder.entries.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, vec!["take r1", "api is v2"]);
    // Read once, gone.
    assert!(store
        .inbox(&group.id, &key("claude-code", "c1"), true)
        .unwrap()
        .entries
        .is_empty());

    // The planner saw the coder join before it and the coder's note after.
    let planner = store.inbox(&group.id, &key("rebon", "s1"), false).unwrap();
    let kinds: Vec<EntryKind> = planner.entries.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![EntryKind::Join, EntryKind::Note]);
    // Peeking leaves them unread.
    assert_eq!(
        store
            .inbox(&group.id, &key("rebon", "s1"), false)
            .unwrap()
            .entries
            .len(),
        2
    );
}

#[test]
fn a_new_member_starts_with_nothing_unread() {
    let (_dir, store, group) = pair();
    store
        .append(&group.id, &key("rebon", "s1"), note("all", "old news"))
        .unwrap();
    store
        .join(&group.id, member("codex", "x1", "reviewer"))
        .unwrap();
    assert!(store
        .inbox(&group.id, &key("codex", "x1"), false)
        .unwrap()
        .entries
        .is_empty());
    assert_eq!(
        store.cursor(&group.id, &key("codex", "x1")).unwrap().read,
        store
            .inbox(&group.id, &key("codex", "x1"), false)
            .unwrap()
            .last_seq
    );
}

#[test]
fn a_session_is_in_one_group_and_aliases_are_unique() {
    let (_dir, store, group) = pair();
    // Joining again is the same membership.
    let (_, again) = store
        .join(&group.id, member("rebon", "s1", "someone-else"))
        .unwrap();
    assert!(again.is_none());
    // Another group is refused while in this one.
    let other = store.create("docs", "/work/app").unwrap();
    assert!(store
        .join(&other.id, member("rebon", "s1", "planner"))
        .is_err());
    // An alias taken in the group is refused, whatever its case.
    assert!(store
        .join(&group.id, member("codex", "x1", "CODER"))
        .is_err());
    assert!(store.join(&group.id, member("codex", "x1", "all")).is_err());
    // Leaving frees the session for another group.
    store.leave(&group.id, &key("rebon", "s1")).unwrap();
    store
        .join(&other.id, member("rebon", "s1", "planner"))
        .unwrap();
}

#[test]
fn only_members_write_and_only_to_members() {
    let (_dir, store, group) = pair();
    assert!(store
        .append(&group.id, &key("codex", "x1"), note("all", "hi"))
        .is_err());
    let error = store
        .append(&group.id, &key("rebon", "s1"), note("nobody", "hi"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("planner") && error.contains("coder"),
        "{error}"
    );
    let reply = Draft {
        kind: EntryKind::Reply,
        re: None,
        ..note("planner", "done")
    };
    assert!(store
        .append(&group.id, &key("claude-code", "c1"), reply)
        .is_err());
}

#[test]
fn a_request_gets_an_id_its_reply_names() {
    let (_dir, store, group) = pair();
    let request = store
        .append(
            &group.id,
            &key("rebon", "s1"),
            Draft {
                kind: EntryKind::Request,
                ..note("coder", "add the tests")
            },
        )
        .unwrap();
    let id = request.id.clone().unwrap();
    assert_eq!(id, format!("r{}", request.seq));
    let reply = store
        .append(
            &group.id,
            &key("claude-code", "c1"),
            Draft {
                kind: EntryKind::Reply,
                re: Some(id.clone()),
                ..note("planner", "3 tests, all pass")
            },
        )
        .unwrap();
    assert_eq!(reply.re.as_deref(), Some(id.as_str()));
    assert_eq!(reply.id, None);
}

#[test]
fn memory_drops_what_was_superseded_and_is_written_out() {
    let (_dir, store, group) = pair();
    let first = store
        .append(&group.id, &key("rebon", "s1"), memory("api is v1", None))
        .unwrap();
    store
        .append(
            &group.id,
            &key("claude-code", "c1"),
            memory("tabs, not spaces", None),
        )
        .unwrap();
    store
        .append(
            &group.id,
            &key("rebon", "s1"),
            memory("api is v2", Some(first.seq)),
        )
        .unwrap();
    let facts: Vec<String> = store
        .memory(&group.id)
        .unwrap()
        .into_iter()
        .map(|e| e.text)
        .collect();
    assert_eq!(facts, vec!["tabs, not spaces", "api is v2"]);
    let document = std::fs::read_to_string(store.memory_path(&group.id)).unwrap();
    assert!(document.contains("api is v2") && !document.contains("api is v1"));
}

#[test]
fn a_torn_last_line_does_not_stop_readers() {
    let (_dir, store, group) = pair();
    store
        .append(&group.id, &key("rebon", "s1"), note("all", "whole"))
        .unwrap();
    let log = store.root().join(&group.id).join(LOG_FILE);
    let mut file = OpenOptions::new().append(true).open(&log).unwrap();
    file.write_all(b"{\"seq\":99,\"atMs\":1,\"fro").unwrap();
    let texts: Vec<String> = store
        .entries_after(&group.id, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.text)
        .collect();
    assert!(texts.contains(&"whole".to_string()));
    // And the next append still lands on its own line after the torn one.
    store
        .append(&group.id, &key("rebon", "s1"), note("all", "after"))
        .unwrap();
    assert!(store
        .entries_after(&group.id, 0)
        .unwrap()
        .iter()
        .any(|e| e.text == "after"));
}

#[test]
fn delivered_only_moves_forward() {
    let (_dir, store, group) = pair();
    let coder = key("claude-code", "c1");
    let start = store.cursor(&group.id, &coder).unwrap().delivered;
    store.mark_delivered(&group.id, &coder, start + 5).unwrap();
    store.mark_delivered(&group.id, &coder, start + 1).unwrap();
    assert_eq!(
        store.cursor(&group.id, &coder).unwrap().delivered,
        start + 5
    );
}

#[test]
fn ids_that_could_leave_the_root_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = GroupStore::new(dir.path());
    for id in ["", "..", "../x", "a/b", "a\\b"] {
        assert!(store.load(id).is_err(), "{id:?}");
    }
}

#[test]
fn directories_compare_by_spelling() {
    assert!(same_dir("C:\\work\\app\\", "C:/work/app"));
    assert!(dir_is_within("C:/work/app/src", "C:\\work\\app"));
    assert!(!dir_is_within("C:/work/application", "C:/work/app"));
    assert!(same_dir("\\\\?\\C:\\work", "C:/work"));
}
