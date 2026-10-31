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
//!
//! Nothing answering to the name means the line was prompt text all along —
//! `@dataclass` above a class body and `@Override` above a method are code a
//! person pasted, not messages to anyone — so the caller hands it to the model
//! unchanged. Only a name that really is a recipient, or one that several
//! recipients answer to, takes the line away from the prompt.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rebon_plugin_tasks::runtime::TaskData;
use rebon_tool::{PeerMessenger, Tool, ToolContext};
use tokio::runtime::Handle;

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

use super::foreground_agent_submit::send_message_to_agent_task;

/// A teammate this session can still write to, and the name `@` reaches it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunningTeammate {
    pub(super) task_id: String,
    pub(super) name: String,
}

/// The one recipient a typed `@name` turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DirectTarget {
    /// A teammate of this session, by the id its task carries.
    Teammate { task_id: String, name: String },
    /// Another session on this machine, by the id the resolver found.
    Peer { session_id: String },
}

/// What looking a typed name up came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DirectResolution {
    /// Exactly one recipient answers to the name.
    Addressed(DirectTarget),
    /// A recipient was named and cannot be written to as typed; the string says
    /// why, and lists what to type instead where there is such a thing.
    Refused(String),
    /// Nothing here answers to the name, so the line was prompt text all along.
    Unaddressed,
}

/// What happened to a message [`send_to_direct_target`] was asked to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DirectAttempt {
    /// A recipient read it; the string is the receipt to keep.
    Sent(String),
    /// A recipient was named and the line was refused, so it stays in the
    /// prompt for the person to fix; the string says why.
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
///
/// `own_handle` is this session's own name, which the peer list never contains:
/// naming it is a refusal rather than a name nothing answers to.
pub(super) fn resolve_direct_target(
    recipient: &str,
    teammates: &[RunningTeammate],
    peers: &[rebon_tool::PeerInfo],
    own_handle: Option<&str>,
) -> DirectResolution {
    match teammates {
        [one] => {
            return DirectResolution::Addressed(DirectTarget::Teammate {
                task_id: one.task_id.clone(),
                name: one.name.clone(),
            })
        }
        [] => {}
        several => return DirectResolution::Refused(several_teammates_answer(recipient, several)),
    }
    match rebon_tool::peer::resolve_peer(peers, recipient) {
        Ok(peer) => DirectResolution::Addressed(DirectTarget::Peer {
            session_id: peer.session_id.clone(),
        }),
        Err(rebon_tool::peer::PeerResolveError::Ambiguous { candidates, .. }) => {
            DirectResolution::Refused(format!(
                "`@{recipient}` matches {} sessions here: {}. Type one of those to reach exactly one.",
                candidates.len(),
                candidates.join(", ")
            ))
        }
        Err(rebon_tool::peer::PeerResolveError::NotFound { .. }) => {
            if own_handle.is_some_and(|own| names_own_handle(recipient, own)) {
                DirectResolution::Refused(self_refusal())
            } else {
                DirectResolution::Unaddressed
            }
        }
    }
}

/// The refusal for a name several running teammates answer to.
///
/// A name cannot pick one of them, and a task id is not something a person can
/// type at the prompt, so the way to reach one is the list that shows them.
fn several_teammates_answer(recipient: &str, several: &[RunningTeammate]) -> String {
    format!(
        "{} running agents here are named `{recipient}`, and a name cannot tell them apart. Open one of them in `/background-agents` and type the message there.",
        several.len()
    )
}

/// Whether `recipient` is this session's own handle, or enough of a prefix of it
/// to name nobody else.
///
/// The full handle is unambiguous — handles are unique machine-wide — and a
/// prefix only when it is long enough to be one a caller may abbreviate to.
fn names_own_handle(recipient: &str, own_handle: &str) -> bool {
    let recipient = rebon_tool::peer::strip_handle_sigil(recipient).trim();
    recipient.eq_ignore_ascii_case(own_handle)
        || (recipient.len() >= rebon_tool::peer::MIN_SESSION_PREFIX_CHARS
            && own_handle
                .to_ascii_lowercase()
                .starts_with(&recipient.to_ascii_lowercase()))
}

fn self_refusal() -> String {
    "A session cannot send a message to itself.".to_string()
}

/// The teammates of this session that are still running and whose name is
/// `name`, ignoring case.
///
/// The same rule `TeamManager::teammate_roster` applies when `SendMessage` looks
/// a teammate up by name: an in-process teammate whose task has not reached a
/// terminal state. A local sub-agent task is not a teammate and its `agent_type`
/// is not a name — `@general-purpose` names nothing — and a teammate that has
/// finished is not reachable by name either, because a message to it would
/// resume work the person asked to end.
fn teammates_named(session: &TuiEngineSession, name: &str) -> Vec<RunningTeammate> {
    session
        .engine_half
        .tasks
        .snapshots()
        .into_iter()
        .filter(|task| !task.status.is_terminal())
        .filter_map(|task| match &task.data {
            TaskData::InProcessTeammate(data) => Some(RunningTeammate {
                task_id: task.id.to_string(),
                name: data.identity.agent_name.clone(),
            }),
            _ => None,
        })
        .filter(|teammate| teammate.name.eq_ignore_ascii_case(name))
        .collect()
}

/// Look a typed name up among this session's teammates and then its peers.
///
/// `Unaddressed` is the answer that matters most: it means the line was prompt
/// text after all, and the caller must send it to the model unchanged. Only a
/// name a recipient really answers to — or several that do — takes the line out
/// of the prompt, because only then is there a person who can be meant and a
/// candidate list worth showing.
pub(super) fn resolve_typed_recipient(
    session: &TuiEngineSession,
    handle: &Handle,
    recipient: &str,
) -> DirectResolution {
    let teammates = teammates_named(session, recipient);
    if !teammates.is_empty() {
        return resolve_direct_target(recipient, &teammates, &[], None);
    }

    // The peer list is a directory scan and a lock probe per session, so it is
    // only paid for a line the teammate list did not already claim.
    let own_handle = rebon_session::load_session_handle(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    );
    let peers = match handle.block_on(peer_messenger(session).list_peers(Some(&session.session_id)))
    {
        Ok(peers) => peers,
        // Discovery failing says nothing about the name, so the line stays a
        // prompt rather than being held back by an error about something else.
        Err(error) => {
            tracing::warn!(%error, "rebon: could not list peer sessions for a typed @name");
            return DirectResolution::Unaddressed;
        }
    };
    resolve_direct_target(recipient, &[], &peers, own_handle.as_deref())
}

/// Send one message to a recipient [`resolve_typed_recipient`] found.
pub(super) fn send_to_direct_target(
    app: &AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    target: &DirectTarget,
    message: &str,
) -> DirectAttempt {
    match target {
        DirectTarget::Teammate { task_id, name } => {
            match send_message_to_agent_task(app, session, handle, task_id, message.into()) {
                Ok(()) => DirectAttempt::Sent(format!("Message sent to @{name} ({task_id}).")),
                Err(error) => DirectAttempt::Refused(error),
            }
        }
        DirectTarget::Peer { session_id } => send_to_peer(session, handle, session_id, message),
    }
}

/// Send through the `SendMessage` tool, so a person and an agent cannot
/// disagree about where a message went or what a refusal says.
fn send_to_peer(
    session: &TuiEngineSession,
    handle: &Handle,
    target_session_id: &str,
    message: &str,
) -> DirectAttempt {
    let mut context = ToolContext::new()
        .with_session_id(session.session_id.clone())
        .with_cwd(session.cwd.clone())
        .with_peer_messenger(peer_messenger(session));
    // Its own handle, so `@mine-1234` is refused here too rather than reported
    // as a session that does not exist — the peer list never contains it.
    if let Some(own_handle) = rebon_session::load_session_handle(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    ) {
        context = context.with_peer_handle(own_handle);
    }
    let input = serde_json::json!({
        "to": format!("session:{target_session_id}"),
        "message": message,
    });
    match handle.block_on(rebon_plugin_tasks::send_message::SendMessageTool.call(input, &context)) {
        Ok(output) => DirectAttempt::Sent(sent_line(&output, target_session_id)),
        Err(error) => DirectAttempt::Refused(peer_refusal(&error.to_string())),
    }
}

/// What the transcript keeps after a send that went out.
fn sent_line(output: &serde_json::Value, target_session_id: &str) -> String {
    let fields: HashMap<String, serde_json::Value> = output
        .as_object()
        .map(|object| object.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    rebon_render::session_message_display(&fields, usize::MAX)
        .unwrap_or_else(|| format!("Message sent to session:{target_session_id}."))
}

/// Reaches the other sessions here the way a tool does.
fn peer_messenger(session: &TuiEngineSession) -> Arc<dyn PeerMessenger> {
    Arc::new(
        rebon_session_runtime::peer_messenger::SessionPeerMessenger::new(
            crate::background::cli_default_store(),
            session.projects_root.clone(),
            crate::background::rebon_exe(),
            // The cwd the session was keyed under: the sender's own sidecar is
            // looked up there, not in whatever directory a tool runs in.
            session.cwd.clone(),
        ),
    )
}

/// Why the peer half refused, worded for the person who typed the line.
///
/// `SendMessage` already names the candidates, so its words are kept and the
/// hint is added only where it would not repeat them.
fn peer_refusal(error: &str) -> String {
    if error.contains("/background-agents") {
        return error.to_string();
    }
    format!("{error}\n`/background-agents` lists the sessions running on this machine.")
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

    fn teammate(task_id: &str, name: &str) -> RunningTeammate {
        RunningTeammate {
            task_id: task_id.into(),
            name: name.into(),
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
            resolve_direct_target("repo-qrst", &[], &peers, None),
            DirectResolution::Addressed(DirectTarget::Peer {
                session_id: "abcde-fghij-klmno-pqrst".into()
            })
        );
        assert_eq!(
            resolve_direct_target("abcde-f", &[], &peers, None),
            DirectResolution::Addressed(DirectTarget::Peer {
                session_id: "abcde-fghij-klmno-pqrst".into()
            }),
            "a typed prefix reaches it the same way the tool accepts one"
        );
    }

    #[test]
    fn a_name_only_a_teammate_answers_to_reaches_that_agent() {
        assert_eq!(
            resolve_direct_target("bob", &[teammate("task-bob", "bob")], &[], None),
            DirectResolution::Addressed(DirectTarget::Teammate {
                task_id: "task-bob".into(),
                name: "bob".into(),
            })
        );
    }

    /// A person who has both is writing to the one in front of them: the footer
    /// names the teammate, and the session stays reachable by its handle.
    #[test]
    fn a_name_both_answer_to_goes_to_the_teammate() {
        let peers = [peer("abcde-fghij-klmno-pqrst", Some("bob"), Some("bob"))];
        assert_eq!(
            resolve_direct_target("bob", &[teammate("task-bob", "bob")], &peers, None),
            DirectResolution::Addressed(DirectTarget::Teammate {
                task_id: "task-bob".into(),
                name: "bob".into(),
            })
        );
    }

    /// A name nothing answers to is not a recipient, so the line was a prompt.
    #[test]
    fn an_unknown_name_leaves_the_line_to_the_model() {
        assert_eq!(
            resolve_direct_target("ghost", &[], &[], None),
            DirectResolution::Unaddressed
        );
        assert_eq!(
            resolve_direct_target("dataclass", &[], &[], None),
            DirectResolution::Unaddressed,
            "pasted code is not an address"
        );
    }

    #[test]
    fn an_ambiguous_name_lists_the_candidates_a_person_can_type() {
        let peers = [
            peer("abcde-one", Some("repo-one"), None),
            peer("abcde-two", Some("repo-only"), None),
        ];
        let DirectResolution::Refused(refusal) = resolve_direct_target("repo-o", &[], &peers, None)
        else {
            panic!("expected an ambiguity");
        };
        assert!(refusal.contains("@repo-one"), "{refusal}");
        assert!(refusal.contains("@repo-only"), "{refusal}");
        assert!(refusal.contains("matches 2 sessions"), "{refusal}");
    }

    /// Two teammates sharing a name cannot be told apart by name, and a task id
    /// is not something the prompt accepts — so the refusal has to send the
    /// person to the surface that does show them.
    #[test]
    fn two_agents_sharing_a_name_are_refused_with_a_way_to_reach_one() {
        let DirectResolution::Refused(refusal) = resolve_direct_target(
            "bob",
            &[teammate("task-one", "bob"), teammate("task-two", "bob")],
            &[],
            None,
        ) else {
            panic!("expected an ambiguity");
        };
        assert!(refusal.contains("/background-agents"), "{refusal}");
        assert!(!refusal.contains("task id"), "{refusal}");
    }

    #[test]
    fn a_session_naming_itself_is_refused_rather_than_not_found() {
        let own = Some("repo-qrst");
        assert_eq!(
            resolve_direct_target("repo-qrst", &[], &[], own),
            DirectResolution::Refused(self_refusal())
        );
        assert_eq!(
            resolve_direct_target("repo-qr", &[], &[], own),
            DirectResolution::Refused(self_refusal()),
            "a unique handle prefix names this session too"
        );
        assert_eq!(
            resolve_direct_target("rep", &[], &[], own),
            DirectResolution::Unaddressed,
            "too short to reach any session"
        );
        assert_eq!(
            resolve_direct_target("ghost", &[], &[], own),
            DirectResolution::Unaddressed
        );
    }

    #[test]
    fn a_refusal_that_already_names_the_list_is_not_told_twice() {
        let said = "`ghost` matches nothing. `/background-agents` lists the sessions running on this machine.";
        assert_eq!(peer_refusal(said), said);
    }
}
