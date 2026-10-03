use crate::model::{Delivery, EntryKind, Member, MemberKey};
use crate::store::Draft;
use crate::{render, GroupStore};

fn world() -> (tempfile::TempDir, GroupStore, String, MemberKey) {
    let dir = tempfile::tempdir().unwrap();
    let store = GroupStore::new(dir.path().join("groups"));
    let group = store.create("question", "/work").unwrap();
    let member = Member {
        agent: "codex".into(),
        session_id: "c1".into(),
        alias: "Cleo".into(),
        role: None,
        delivery: Delivery::Auto,
        joined_at_ms: 1,
    };
    let key = member.key();
    store.join(&group.id, member).unwrap();
    (dir, store, group.id, key)
}

fn draft(kind: EntryKind, to: &str, re: Option<&str>) -> Draft {
    Draft {
        kind,
        to: Some(to.into()),
        re: re.map(str::to_string),
        supersedes: None,
        text: "选择方案一".into(),
    }
}

#[test]
fn user_answer_reaches_only_the_asking_members_inbox_and_wakes_it() {
    let (_dir, store, id, key) = world();
    let question = store
        .append(&id, &key, draft(EntryKind::Request, "user", None))
        .unwrap();
    let answer = store
        .post_as_user(&id, draft(EntryKind::Reply, "Cleo", question.id.as_deref()))
        .unwrap();
    assert_eq!(answer.from, "user");
    assert_eq!(answer.id, None);
    let inbox = store.inbox(&id, &key, false).unwrap();
    assert!(inbox.entries.contains(&answer));
    let group = store.load(&id).unwrap();
    let prompt = render::user_prompt(&group, std::slice::from_ref(&answer)).unwrap();
    assert!(prompt.contains(&format!("answer to {}", question.id.unwrap())));
    assert!(prompt.contains("选择方案一"));
    assert!(!prompt.contains("When done, report back"));
    let terminal = render::terminal_prompt(&group, &[answer]).unwrap();
    assert!(terminal.contains("answer to"));
    assert!(terminal.contains("continue the original task"));
}

#[test]
fn malformed_or_misdirected_answers_do_not_mutate_the_log() {
    let (_dir, store, id, key) = world();
    let question = store
        .append(&id, &key, draft(EntryKind::Request, "user", None))
        .unwrap();
    let before = store.entries_after(&id, 0).unwrap();
    for bad in [
        draft(EntryKind::Reply, "Cleo", None),
        draft(EntryKind::Reply, "all", question.id.as_deref()),
        draft(EntryKind::Reply, "Cleo", Some("r999")),
    ] {
        assert!(store.post_as_user(&id, bad).is_err());
    }
    assert_eq!(store.entries_after(&id, 0).unwrap(), before);
}

#[test]
fn another_members_reply_does_not_prevent_the_users_answer_but_double_submit_does() {
    let (_dir, store, id, key) = world();
    let question = store
        .append(&id, &key, draft(EntryKind::Request, "user", None))
        .unwrap();
    store
        .append(
            &id,
            &key,
            draft(EntryKind::Reply, "user", question.id.as_deref()),
        )
        .unwrap();
    store
        .post_as_user(&id, draft(EntryKind::Reply, "cleo", question.id.as_deref()))
        .unwrap();
    let before = store.entries_after(&id, 0).unwrap();
    assert!(store
        .post_as_user(&id, draft(EntryKind::Reply, "Cleo", question.id.as_deref()))
        .is_err());
    assert_eq!(store.entries_after(&id, 0).unwrap(), before);
}

#[test]
fn requests_to_members_and_user_tasks_cannot_be_answered_as_user_questions() {
    let (_dir, store, id, key) = world();
    for to in ["all", "Cleo"] {
        let request = store
            .append(&id, &key, draft(EntryKind::Request, to, None))
            .unwrap();
        assert!(store
            .post_as_user(&id, draft(EntryKind::Reply, "Cleo", request.id.as_deref()))
            .is_err());
    }
    let task = store
        .post_as_user(&id, draft(EntryKind::Request, "Cleo", None))
        .unwrap();
    assert!(store
        .post_as_user(&id, draft(EntryKind::Reply, "Cleo", task.id.as_deref()))
        .is_err());
}

#[test]
fn a_departed_members_question_can_still_be_answered() {
    let (_dir, store, id, key) = world();
    let question = store
        .append(&id, &key, draft(EntryKind::Request, "user", None))
        .unwrap();
    store.leave(&id, &key).unwrap();
    assert!(store
        .post_as_user(&id, draft(EntryKind::Reply, "Cleo", question.id.as_deref()))
        .is_ok());
}

#[test]
fn agent_replies_alone_do_not_wake_and_guidance_names_the_user_request_path() {
    let (_dir, store, id, key) = world();
    let task = store
        .post_as_user(&id, draft(EntryKind::Request, "Cleo", None))
        .unwrap();
    let reply = store
        .append(
            &id,
            &key,
            draft(EntryKind::Reply, "user", task.id.as_deref()),
        )
        .unwrap();
    let group = store.load(&id).unwrap();
    assert!(render::terminal_prompt(&group, std::slice::from_ref(&reply)).is_none());
    assert!(render::user_prompt(&group, &[reply]).is_none());
    assert!(render::QUESTION_GUIDE.contains("kind=\"request\""));
    assert!(render::QUESTION_GUIDE.contains("end the turn"));
    assert!(render::QUESTION_GUIDE.contains("not permission to bypass"));
}

#[test]
fn a_user_answer_among_agent_requests_still_names_its_authority() {
    let (_dir, store, id, key) = world();
    let question = store
        .append(&id, &key, draft(EntryKind::Request, "user", None))
        .unwrap();
    let answer = store
        .post_as_user(&id, draft(EntryKind::Reply, "Cleo", question.id.as_deref()))
        .unwrap();
    let task = store
        .append(&id, &key, draft(EntryKind::Request, "all", None))
        .unwrap();
    let prompt = render::terminal_prompt(&store.load(&id).unwrap(), &[answer, task]).unwrap();
    assert!(prompt.contains("user answers"));
    assert!(prompt.contains("messages from user are the user's own words"));
}
