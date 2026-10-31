//! A person typing `@someone hello` at the prompt.
//!
//! The line looks like a prompt and is not one: the first word names who should
//! read the rest. A teammate of this session is tried first, by the name it was
//! given; failing that, another session running on this machine, which goes
//! through the `SendMessage` tool so a person's `@handle` and an agent's are
//! delivered by one sender, with one set of refusals.
//!
//! `@` is also how a file or directory is mentioned, so a token that names
//! something on disk is never a recipient. That check is what keeps
//! `@Makefile look at this` a prompt; it comes before any lookup because a file
//! happens to be named like a teammate far more often than the other way round.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rebon_plugin_tasks::runtime::TaskData;
use rebon_tool::{PeerMessenger, Tool, ToolContext};
use tokio::runtime::Handle;

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

use super::foreground_agent_submit::send_message_to_agent_task;

/// Who a typed `@name` is for, once the names in hand were checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DirectTarget {
    /// A teammate of this session, by the id its task carries.
    Teammate { task_id: String },
    /// Another session on this machine, by the id the resolver found.
    Peer { session_id: String },
}

/// Where a typed `@name` was delivered, or why it was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DirectDelivery {
    /// A teammate of this session read it.
    Teammate { task_id: String },
    /// Another session read it; the string is the receipt to keep.
    Peer(String),
    /// Nothing was sent, and the string says why.
    Refused(String),
}

/// The line as a direct message, or `None` when it is prompt text.
pub(super) fn parse_direct_message(
    text: &str,
    cwd: &Path,
) -> Option<rebon_tui::promptinput::DirectMemberMessage> {
    let parsed = rebon_tui::promptinput::parse_direct_member_message(text)?;
    if cwd.join(&parsed.recipient_name).exists() {
        return None;
    }
    Some(parsed)
}

/// Resolve a recipient against the names this session can address.
///
/// The teammate list decides first: a person who has both a teammate and a
/// session answering to `bob` is writing to the one they can see in the footer,
/// and the session is still reachable as `@bob-<tail>` or `session:<id>`. A name
/// two agents share is refused rather than resolved to whichever the registry
/// listed first, which is the one thing a person cannot have meant.
pub(super) fn resolve_direct_target(
    recipient: &str,
    teammates: &[String],
    peers: &[rebon_tool::PeerInfo],
) -> Result<DirectTarget, String> {
    match teammates {
        [task_id] => {
            return Ok(DirectTarget::Teammate {
                task_id: task_id.clone(),
            })
        }
        [] => {}
        several => {
            return Err(format!(
            "{} agents here answer to `{recipient}`: {}. Name the one you meant by its task id.",
            several.len(),
            several.join(", ")
        ))
        }
    }
    match rebon_tool::peer::resolve_peer(peers, recipient) {
        Ok(peer) => Ok(DirectTarget::Peer {
            session_id: peer.session_id.clone(),
        }),
        Err(error) => Err(peer_refusal(&error.to_string(), recipient)),
    }
}

/// The teammates of this session whose name is `name`, ignoring case.
fn teammate_ids(session: &TuiEngineSession, name: &str) -> Vec<String> {
    session
        .engine_half
        .tasks
        .snapshots()
        .into_iter()
        .filter(|task| agent_name(task).is_some_and(|agent| agent.eq_ignore_ascii_case(name)))
        .map(|task| task.id.to_string())
        .collect()
}

fn agent_name(task: &rebon_plugin_tasks::runtime::TaskSnapshot) -> Option<&str> {
    match &task.data {
        TaskData::InProcessTeammate(data) => Some(data.identity.agent_name.as_str()),
        TaskData::LocalAgent(data) => Some(data.agent_type.as_str()),
        _ => None,
    }
}

/// Deliver one typed direct message.
pub(super) fn deliver_direct_message(
    app: &AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    recipient: &str,
    message: &str,
) -> DirectDelivery {
    let messenger = peer_messenger(session);
    let peers = match handle.block_on(messenger.list_peers(Some(&session.session_id))) {
        Ok(peers) => peers,
        Err(error) => return DirectDelivery::Refused(error),
    };
    match resolve_direct_target(recipient, &teammate_ids(session, recipient), &peers) {
        Ok(DirectTarget::Teammate { task_id }) => {
            match send_message_to_agent_task(app, session, handle, &task_id, message.into()) {
                Ok(()) => DirectDelivery::Teammate { task_id },
                Err(error) => DirectDelivery::Refused(error),
            }
        }
        // Addressed as a handle, which is what `resolve_peer` just matched; the
        // tool resolves it again against the same list, so a person and an agent
        // cannot disagree about where a message went.
        Ok(DirectTarget::Peer { .. }) => {
            let context = ToolContext::new()
                .with_session_id(session.session_id.clone())
                .with_cwd(session.cwd.clone())
                .with_peer_messenger(messenger);
            let input = serde_json::json!({
                "to": format!("@{recipient}"),
                "message": message,
            });
            match handle
                .block_on(rebon_plugin_tasks::send_message::SendMessageTool.call(input, &context))
            {
                Ok(output) => DirectDelivery::Peer(sent_line(&output, recipient)),
                Err(error) => DirectDelivery::Refused(peer_refusal(&error.to_string(), recipient)),
            }
        }
        Err(refusal) => DirectDelivery::Refused(refusal),
    }
}

/// What the transcript keeps after a send that went out.
fn sent_line(output: &serde_json::Value, recipient: &str) -> String {
    let fields: HashMap<String, serde_json::Value> = output
        .as_object()
        .map(|object| object.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    rebon_render::session_message_display(&fields)
        .unwrap_or_else(|| format!("Message sent to @{recipient}"))
}

/// Reaches the other sessions here the way a tool does.
fn peer_messenger(session: &TuiEngineSession) -> Arc<dyn PeerMessenger> {
    Arc::new(
        rebon_session_runtime::peer_messenger::SessionPeerMessenger::new(
            crate::background::cli_default_store(),
            session.projects_root.clone(),
            crate::background::rebon_exe(),
        ),
    )
}

/// Why the peer half refused, worded for the person who typed the line.
///
/// `SendMessage` already names the candidates and the tool that lists the
/// sessions, so its words are kept and the hint is added only where it would
/// not repeat them.
fn peer_refusal(error: &str, recipient: &str) -> String {
    if error.contains("agent-view") {
        return error.to_string();
    }
    format!(
        "{error}\n@{recipient} named no session here. `/agent-view` lists the sessions running on this machine."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::PeerInfo;

    fn peer(session_id: &str, handle: Option<&str>, name: Option<&str>) -> PeerInfo {
        PeerInfo {
            session_id: session_id.into(),
            handle: handle.map(str::to_string),
            name: name.map(str::to_string),
            cwd: "/work".into(),
            kind: "foreground".into(),
            busy: false,
            reachable: true,
            terminal_id: None,
            activity: None,
        }
    }

    #[test]
    fn a_token_that_names_a_path_is_a_mention_not_a_recipient() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Makefile"), "all:").unwrap();

        assert!(
            parse_direct_message("@Makefile please read this", dir.path()).is_none(),
            "a file named on disk is what the model should read"
        );
        let parsed = parse_direct_message("@bob please read this", dir.path())
            .expect("a name with nothing behind it is a recipient");
        assert_eq!(parsed.recipient_name, "bob");
        assert_eq!(parsed.message, "please read this");
    }

    #[test]
    fn a_line_without_a_leading_name_is_a_prompt() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parse_direct_message("read @Makefile for me", dir.path()).is_none());
        assert!(
            parse_direct_message("@bob", dir.path()).is_none(),
            "nothing to say"
        );
    }

    /// A handle is only usable if a person can type it as a recipient. The
    /// generator and the `@name` parser live in different crates, so this is the
    /// one place the two rules are checked against each other: every handle an
    /// owner could actually claim has to parse back as a recipient.
    #[test]
    fn a_claimed_handle_is_a_recipient_a_person_can_type() {
        let projects = tempfile::tempdir().unwrap();
        let mentions = tempfile::tempdir().unwrap();
        let session_id = "0123456-789ab-cdef0-12345";

        for cwd in [
            "/repo",
            "/tmp/My Project!!",
            "F:\\dev\\Work",
            "/Users/bon/Dev/reboncode/.worktrees/cross-session",
            "/",
        ] {
            let handle = rebon_session::ensure_session_handle(projects.path(), cwd, session_id)
                .unwrap_or_else(|error| panic!("{cwd}: {error}"));
            assert_eq!(
                rebon_session::load_session_handle(projects.path(), cwd, session_id).as_deref(),
                Some(handle.as_str()),
                "{cwd}"
            );

            let line = format!("@{handle} take the next item");
            let parsed = rebon_tui::promptinput::parse_direct_member_message(&line)
                .unwrap_or_else(|| panic!("{handle} from {cwd} is not a typeable recipient"));
            assert_eq!(parsed.recipient_name, handle, "{cwd}");
            assert_eq!(parsed.message, "take the next item");
            assert!(
                parse_direct_message(&line, mentions.path()).is_some(),
                "nothing on disk is named {handle}"
            );
        }
    }

    #[test]
    fn a_name_only_a_session_answers_to_reaches_that_session() {
        let peers = [peer("abcde-fghij-klmno-pqrst", Some("repo-qrst"), None)];
        assert_eq!(
            resolve_direct_target("repo-qrst", &[], &peers),
            Ok(DirectTarget::Peer {
                session_id: "abcde-fghij-klmno-pqrst".into()
            })
        );
        assert_eq!(
            resolve_direct_target("abcde-f", &[], &peers),
            Ok(DirectTarget::Peer {
                session_id: "abcde-fghij-klmno-pqrst".into()
            }),
            "a typed prefix reaches it the same way the tool accepts one"
        );
    }

    #[test]
    fn a_name_only_a_teammate_answers_to_reaches_that_agent() {
        assert_eq!(
            resolve_direct_target("bob", &["task-bob".into()], &[]),
            Ok(DirectTarget::Teammate {
                task_id: "task-bob".into()
            })
        );
    }

    /// A person who has both is writing to the one in front of them: the footer
    /// names the teammate, and the session stays reachable by its handle.
    #[test]
    fn a_name_both_answer_to_goes_to_the_teammate() {
        let peers = [peer("abcde-fghij-klmno-pqrst", Some("bob"), Some("bob"))];
        assert_eq!(
            resolve_direct_target("bob", &["task-bob".into()], &peers),
            Ok(DirectTarget::Teammate {
                task_id: "task-bob".into()
            })
        );
    }

    #[test]
    fn an_unknown_name_says_where_the_sessions_are_listed() {
        let refusal = resolve_direct_target("ghost", &[], &[]).unwrap_err();
        assert!(
            refusal.contains("no running session matches `ghost`"),
            "{refusal}"
        );
        assert!(refusal.contains("/agent-view"), "{refusal}");
    }

    #[test]
    fn an_ambiguous_name_lists_the_handles() {
        let peers = [
            peer("abcde-one", Some("repo-one"), None),
            peer("abcde-two", Some("repo-only"), None),
        ];
        let refusal = resolve_direct_target("repo-o", &[], &peers).unwrap_err();
        assert!(refusal.contains("@repo-one"), "{refusal}");
        assert!(refusal.contains("@repo-only"), "{refusal}");
        assert!(refusal.contains("matches 2 sessions"), "{refusal}");
    }

    #[test]
    fn two_agents_sharing_a_name_are_refused_rather_than_guessed() {
        let refusal =
            resolve_direct_target("bob", &["task-one".into(), "task-two".into()], &[]).unwrap_err();
        assert!(refusal.contains("task-one"), "{refusal}");
        assert!(refusal.contains("task-two"), "{refusal}");
    }

    #[test]
    fn a_refusal_that_already_names_the_list_is_not_told_twice() {
        let said =
            "`ghost` matches nothing. `/agent-view` lists the sessions running on this machine.";
        assert_eq!(peer_refusal(said, "ghost"), said);
    }
}
