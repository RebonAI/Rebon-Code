//! The foreground command mailbox, as a transport.
//!
//! An interactive session that holds its own on-disk active lock is reachable
//! by an external process (the GPUI desktop app) through two files: a mailbox
//! it drops commands into, and a status sidecar this side publishes so the
//! controller can see a live session's busy/permission state and confirm that a
//! command was delivered. This module owns that traffic — draining claims,
//! answering them, remembering which ids were already served so a redelivery is
//! acknowledged rather than re-run, and deciding when the sidecar is stale
//! enough to rewrite.
//!
//! What a command *means* is not here. Injecting a prompt, stopping a turn and
//! answering a permission all run through the same in-loop functions a local
//! keypress would, so they stay with the terminal in
//! `crate::tui::runner::foreground_mailbox`, which owns this type and calls it.
//!
//! Only a session holding the lock is the authoritative owner; without it
//! another process owns the session, and this publishes nothing and drains
//! nothing.
//!
//! **Retirement withdrawn — kept indefinitely**. That RFC once
//! scheduled this for deletion one release after hosted sessions
//! became the default; on 8/31 hosted went back to being opt-in (`--hosted`),
//! so a plain `rebon` hosts its own session in-process again, holds its own
//! lock, and publishes no *owner* endpoint. It does advertise a narrow
//! agent-message endpoint in `.live.json`, but control commands still travel
//! only through this mailbox: that makes it the desktop app's only way to reach
//! the default session shape — the one it resolves as `OwnedOpaque` and
//! reaches through files — rather than an escape hatch for
//! `--local`. Hosted sessions still go through the endpoint, and this stays
//! inert for them: a mirror holds no lock. Revisit retirement only after the
//! mirror's control and presentation faces are finished, hosted is the default
//! again, and one release cycle has passed; retiring then still means deleting
//! this module, the terminal half above it, the app's `send_foreground_command`
//! fallback, the `<sid>.live.json` writer and its readers, and
//! `rebon_session_host::foreground` together.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rebon_session_host::{
    ForegroundCommandClaim, ForegroundCommandOutcome, ForegroundCommandResult, ForegroundQuestion,
    ForegroundQuestionAnswer, ForegroundSessionStatus,
};
// Only the question builder in the tests names an option.
#[cfg(test)]
use rebon_session_host::ForegroundQuestionOption;

use crate::session::EngineSession;

/// How often (at most) the status sidecar is refreshed when nothing changed —
/// a heartbeat so the controller can detect a hung/crashed publisher.
const STATUS_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(1000);

/// How often (at most) the sidecar is rewritten when only the activity moved.
///
/// A streaming tool updates its summary on every delta, and each rewrite is a
/// file the peer list reads. A second is finer than a reader can tell apart,
/// and the heartbeat interval already forces a write on its own schedule — so
/// this only ever *delays* a write, never prevents one.
const ACTIVITY_REWRITE_INTERVAL: Duration = Duration::from_millis(1000);

/// How many recently-processed command ids to remember for idempotency. Bounds
/// the dedup set so a long-lived session does not grow it without limit.
const PROCESSED_HISTORY: usize = 64;

/// How many `RunCommand` payloads to keep for replay. Far shorter than
/// [`PROCESSED_HISTORY`] on purpose: these carry a command's full rendered
/// output (a `/context` report runs to kilobytes), and only a redelivered claim
/// ever reads one back.
const COMMAND_RESPONSE_HISTORY: usize = 8;

pub(crate) fn command_was_already_resolved(error: &str) -> bool {
    error == "no permission pending"
        || error.starts_with("stale permission answer:")
        || error.starts_with("stale question answer:")
}

pub(crate) fn validate_question_answer(
    option_count: usize,
    multi_select: bool,
    answer: &ForegroundQuestionAnswer,
) -> Result<(), String> {
    if !multi_select && answer.selected_options.len() > 1 {
        return Err("single-select question received multiple options".to_string());
    }
    for (position, selected) in answer.selected_options.iter().enumerate() {
        if *selected >= option_count {
            return Err(format!("option index {selected} is out of bounds"));
        }
        if answer.selected_options[..position].contains(selected) {
            return Err(format!(
                "option index {selected} was selected more than once"
            ));
        }
    }
    let has_other_text = answer
        .other_text
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    if answer.selected_options.is_empty() && !has_other_text {
        return Err("question has no selected option or Other answer".to_string());
    }
    Ok(())
}

fn foreground_status_fingerprint(
    busy: bool,
    pending_permission: Option<&rebon_session_host::BackgroundPermissionQuerySnapshot>,
    ask_user_questions: Option<&[ForegroundQuestion]>,
    last_command_id: Option<&str>,
    activity: Option<&str>,
) -> String {
    let questions = serde_json::to_string(&ask_user_questions).unwrap_or_default();
    format!(
        "{busy}|{}|{questions}|{}|{}",
        pending_permission
            .map(|permission| permission.query_id)
            .unwrap_or(0),
        last_command_id.unwrap_or(""),
        activity.unwrap_or("")
    )
}

/// The mailbox and status sidecar for one session, with the bookkeeping that
/// makes a redelivered command idempotent.
///
/// Constructed once per event loop and dropped when it returns; dropping clears
/// the status sidecar so the controller stops offering controls the moment the
/// loop tears down.
pub(crate) struct ForegroundMailbox {
    enabled: bool,
    projects_root: PathBuf,
    cwd: String,
    session_id: String,
    last_status_publish: Option<Instant>,
    last_status_fp: String,
    /// The activity last written to the sidecar, so a rewrite caused only by an
    /// activity change can be told apart from one caused by anything else.
    last_activity: Option<String>,
    /// Recently-processed command ids (most-recent at the back) for idempotency.
    processed: VecDeque<String>,
    /// Acknowledgement of the most recently processed command, mirrored into the
    /// published status so the controller can confirm delivery / see errors.
    last_command_id: Option<String>,
    last_command_at_ms: u64,
    last_command_error: Option<String>,
    recent_command_results: VecDeque<ForegroundCommandResult>,
    /// Payloads of the most recent `RunCommand`s, so a redelivered claim can be
    /// answered from memory instead of leaving the client to time out.
    recent_command_responses: VecDeque<rebon_session_host::ForegroundCommandResponse>,
    /// The desktop terminal that launched this process, read once at startup.
    terminal_id: Option<String>,
    /// The loopback listener this session accepts agent messages on, when one
    /// started. Set by the terminal half after `enabled` is known.
    agent_endpoint: Option<rebon_session_host::ForegroundAgentEndpointInfo>,
}

impl ForegroundMailbox {
    pub(crate) fn new(session: &EngineSession) -> Self {
        Self {
            enabled: session.session_active_lock.is_some(),
            projects_root: session.projects_root.clone(),
            cwd: session.cwd.clone(),
            session_id: session.session_id.clone(),
            last_status_publish: None,
            last_status_fp: String::new(),
            last_activity: None,
            processed: VecDeque::with_capacity(PROCESSED_HISTORY),
            last_command_id: None,
            last_command_at_ms: 0,
            last_command_error: None,
            recent_command_results: VecDeque::with_capacity(PROCESSED_HISTORY),
            recent_command_responses: VecDeque::with_capacity(COMMAND_RESPONSE_HISTORY),
            terminal_id: rebon_session_host::terminal_id_from_env(),
            agent_endpoint: None,
        }
    }

    /// Advertise (or withdraw) the endpoint the terminal half started.
    pub(crate) fn set_agent_endpoint(
        &mut self,
        endpoint: Option<rebon_session_host::ForegroundAgentEndpointInfo>,
    ) {
        self.agent_endpoint = endpoint;
    }

    /// Whether this session is the authoritative owner, and therefore whether
    /// there is a mailbox to serve at all.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Re-point at whatever session is on screen now, clearing the previous
    /// session's sidecar and every piece of bookkeeping that belonged to it.
    pub(crate) fn sync_session(&mut self, session: &EngineSession) {
        let enabled = session.session_active_lock.is_some();
        if self.enabled == enabled
            && self.projects_root == session.projects_root
            && self.cwd == session.cwd
            && self.session_id == session.session_id
        {
            return;
        }
        if self.enabled {
            rebon_session_host::clear_foreground_status(
                &self.projects_root,
                &self.cwd,
                &self.session_id,
            );
        }
        self.enabled = enabled;
        self.projects_root = session.projects_root.clone();
        self.cwd = session.cwd.clone();
        self.session_id = session.session_id.clone();
        self.last_status_publish = None;
        self.last_status_fp.clear();
        self.last_activity = None;
        self.processed.clear();
        self.last_command_id = None;
        self.last_command_at_ms = 0;
        self.last_command_error = None;
        self.recent_command_results.clear();
        self.recent_command_responses.clear();
        self.agent_endpoint = None;
    }

    /// Everything queued for this session, claimed.
    pub(crate) fn claims(&self) -> Vec<ForegroundCommandClaim> {
        rebon_session_host::drain_foreground_commands(
            &self.projects_root,
            &self.cwd,
            &self.session_id,
        )
    }

    /// Remove a claim. Called only once its outcome is durably reflected in the
    /// published status, so a crash before that point re-delivers it.
    pub(crate) fn complete(&self, claim: &ForegroundCommandClaim) {
        let _ = rebon_session_host::complete_foreground_command(claim);
    }

    pub(crate) fn already_processed(&self, command_id: &str) -> bool {
        self.processed.iter().any(|id| id == command_id)
    }

    pub(crate) fn remember_processed(&mut self, command_id: String) {
        if self.processed.len() >= PROCESSED_HISTORY {
            self.processed.pop_front();
        }
        self.processed.push_back(command_id);
    }

    pub(crate) fn remembered_response(
        &self,
        command_id: &str,
    ) -> Option<rebon_session_host::ForegroundCommandResponse> {
        self.recent_command_responses
            .iter()
            .find(|response| response.command_id == command_id)
            .cloned()
    }

    pub(crate) fn remember_response(
        &mut self,
        response: rebon_session_host::ForegroundCommandResponse,
    ) {
        if self.recent_command_responses.len() >= COMMAND_RESPONSE_HISTORY {
            self.recent_command_responses.pop_front();
        }
        self.recent_command_responses.push_back(response);
    }

    /// Write the response sidecar a blocked caller is waiting on.
    pub(crate) fn write_response(&self, response: &rebon_session_host::ForegroundCommandResponse) {
        let _ = rebon_session_host::write_foreground_command_response(
            &self.projects_root,
            &self.cwd,
            &self.session_id,
            response,
        );
    }

    /// Record what a command did, for the acknowledgement the status carries.
    pub(crate) fn record_result(
        &mut self,
        command_id: String,
        processed_at_ms: u64,
        outcome: ForegroundCommandOutcome,
        error: Option<String>,
    ) {
        if self.recent_command_results.len() >= PROCESSED_HISTORY {
            self.recent_command_results.pop_front();
        }
        self.recent_command_results
            .push_back(ForegroundCommandResult {
                command_id: command_id.clone(),
                processed_at_ms,
                outcome,
                error: error.clone(),
            });
        self.last_command_id = Some(command_id);
        self.last_command_at_ms = processed_at_ms;
        self.last_command_error = error;
    }

    /// Publish the status sidecar when the busy/permission/ack state changed, or
    /// at the heartbeat interval otherwise. Cheap no-op when nothing changed.
    ///
    /// The three facts about the screen are passed in rather than read: an
    /// active prompt and a pending permission are terminal types, and naming
    /// them here would put the terminal in this signature.
    pub(crate) fn publish(
        &mut self,
        busy: bool,
        pending_permission: Option<&rebon_session_host::BackgroundPermissionQuerySnapshot>,
        ask_user_questions: Option<&[ForegroundQuestion]>,
        activity: Option<&str>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let port = self
            .agent_endpoint
            .as_ref()
            .map(|endpoint| endpoint.port)
            .unwrap_or(0);
        let fp = format!(
            "{}|{port}",
            foreground_status_fingerprint(
                busy,
                pending_permission,
                ask_user_questions,
                self.last_command_id.as_deref(),
                activity,
            )
        );
        // The fingerprint this would have had if the activity had not moved. If
        // that is what was last written, the activity is the only difference.
        let fp_without_new_activity = format!(
            "{}|{port}",
            foreground_status_fingerprint(
                busy,
                pending_permission,
                ask_user_questions,
                self.last_command_id.as_deref(),
                self.last_activity.as_deref(),
            )
        );
        let now = Instant::now();
        let heartbeat_due = self
            .last_status_publish
            .map(|t| now.duration_since(t) >= STATUS_HEARTBEAT_INTERVAL)
            .unwrap_or(true);
        if fp == self.last_status_fp && !heartbeat_due {
            return true;
        }
        // A streaming tool rewrites its summary on every delta. Everything else
        // is unchanged here, so this is the one case worth waiting out rather
        // than writing a file per delta.
        if fp_without_new_activity == self.last_status_fp && !heartbeat_due {
            let activity_rewrite_due = self
                .last_status_publish
                .map(|t| now.duration_since(t) >= ACTIVITY_REWRITE_INTERVAL)
                .unwrap_or(true);
            if !activity_rewrite_due {
                return true;
            }
        }
        let status = ForegroundSessionStatus {
            pid: std::process::id(),
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            heartbeat_ms: rebon_session_host::now_ms(),
            busy,
            pending_permission: pending_permission.cloned(),
            ask_user_questions: ask_user_questions.map(<[_]>::to_vec),
            last_command_id: self.last_command_id.clone(),
            last_command_at_ms: self.last_command_at_ms,
            last_command_error: self.last_command_error.clone(),
            recent_command_results: self.recent_command_results.iter().cloned().collect(),
            terminal_id: self.terminal_id.clone(),
            agent_endpoint: self.agent_endpoint.clone(),
            // A live TUI drains `ForegroundCommand::AgentMessage` even when its
            // listener could not bind, so the capability is advertised whenever
            // the mailbox itself is.
            capabilities: vec![rebon_session_host::AGENT_MESSAGE_CAPABILITY.to_string()],
            activity: activity.map(str::to_string),
        };
        let written = rebon_session_host::write_foreground_status(&self.projects_root, &status);
        if written {
            self.last_status_fp = fp;
            self.last_activity = activity.map(str::to_string);
            self.last_status_publish = Some(now);
        }
        written
    }
}

impl Drop for ForegroundMailbox {
    fn drop(&mut self) {
        if self.enabled {
            rebon_session_host::clear_foreground_status(
                &self.projects_root,
                &self.cwd,
                &self.session_id,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(label: &str) -> ForegroundQuestion {
        ForegroundQuestion {
            header: "选择".into(),
            question: "继续吗？".into(),
            multi_select: false,
            options: vec![
                ForegroundQuestionOption {
                    label: label.into(),
                    description: "继续执行".into(),
                    preview: None,
                },
                ForegroundQuestionOption {
                    label: "取消".into(),
                    description: "停止执行".into(),
                    preview: None,
                },
            ],
        }
    }

    #[test]
    fn foreground_control_rebinds_when_session_identity_changes() {
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.cwd = "C:/first".into();
        session.session_id = "session-first".into();
        let mut mailbox = ForegroundMailbox::new(&session);
        mailbox.processed.push_back("old-command".into());
        mailbox
            .recent_command_results
            .push_back(ForegroundCommandResult {
                command_id: "old-command".into(),
                processed_at_ms: 1,
                outcome: ForegroundCommandOutcome::Applied,
                error: None,
            });
        mailbox.remember_response(rebon_session_host::ForegroundCommandResponse {
            command_id: "old-command".into(),
            processed_at_ms: 1,
            output: None,
            error: None,
        });

        session.cwd = "C:/second".into();
        session.session_id = "session-second".into();
        mailbox.sync_session(&session);

        assert_eq!(mailbox.cwd, "C:/second");
        assert_eq!(mailbox.session_id, "session-second");
        assert!(mailbox.processed.is_empty());
        assert!(mailbox.recent_command_results.is_empty());
        assert!(mailbox.recent_command_responses.is_empty());
        assert!(mailbox.last_command_id.is_none());
    }

    #[test]
    fn remembered_responses_replay_the_newest_payload_and_stay_bounded() {
        let session = crate::tui::runner::test_support::make_test_tui_session();
        let mut mailbox = ForegroundMailbox::new(&session);
        for index in 0..COMMAND_RESPONSE_HISTORY + 2 {
            mailbox.remember_response(rebon_session_host::ForegroundCommandResponse {
                command_id: format!("cmd-{index}"),
                processed_at_ms: index as u64,
                output: Some(rebon_session_host::CommandOutput {
                    text: format!("output {index}"),
                    tone: "info".into(),
                }),
                error: None,
            });
        }

        assert_eq!(
            mailbox.recent_command_responses.len(),
            COMMAND_RESPONSE_HISTORY
        );
        assert!(mailbox.remembered_response("cmd-0").is_none());
        let newest = COMMAND_RESPONSE_HISTORY + 1;
        let replayed = mailbox
            .remembered_response(&format!("cmd-{newest}"))
            .expect("newest response is still remembered");
        assert_eq!(
            replayed.output.expect("payload").text,
            format!("output {newest}")
        );
    }

    #[test]
    fn stale_permission_commands_are_classified_as_already_resolved() {
        assert!(command_was_already_resolved("no permission pending"));
        assert!(command_was_already_resolved(
            "stale permission answer: have q1, got q2"
        ));
        assert!(command_was_already_resolved(
            "stale question answer: have q1, got q2"
        ));
        assert!(!command_was_already_resolved(
            "unknown permission option_id"
        ));
    }

    #[test]
    fn question_answer_accepts_other_only_and_selected_with_note() {
        let question = question("继续");
        assert!(validate_question_answer(
            question.options.len(),
            question.multi_select,
            &ForegroundQuestionAnswer {
                selected_options: Vec::new(),
                other_text: Some("自定义回答".into()),
            },
        )
        .is_ok());
        assert!(validate_question_answer(
            question.options.len(),
            question.multi_select,
            &ForegroundQuestionAnswer {
                selected_options: vec![0],
                other_text: Some("补充说明".into()),
            },
        )
        .is_ok());
    }

    #[test]
    fn question_answer_rejects_invalid_selections() {
        let question = question("继续");
        for answer in [
            ForegroundQuestionAnswer {
                selected_options: Vec::new(),
                other_text: Some("   ".into()),
            },
            ForegroundQuestionAnswer {
                selected_options: vec![0, 1],
                other_text: None,
            },
            ForegroundQuestionAnswer {
                selected_options: vec![2],
                other_text: None,
            },
        ] {
            assert!(validate_question_answer(
                question.options.len(),
                question.multi_select,
                &answer,
            )
            .is_err());
        }
    }

    #[test]
    fn status_fingerprint_tracks_question_presence() {
        let without_questions = foreground_status_fingerprint(false, None, None, None, None);
        let questions = vec![question("继续")];
        let with_questions =
            foreground_status_fingerprint(false, None, Some(&questions), None, None);
        assert_ne!(without_questions, with_questions);
    }

    #[test]
    fn status_fingerprint_tracks_question_shape() {
        let first = vec![question("继续")];
        let second = vec![question("执行")];
        assert_ne!(
            foreground_status_fingerprint(true, None, Some(&first), Some("command"), None),
            foreground_status_fingerprint(true, None, Some(&second), Some("command"), None)
        );
    }

    #[test]
    fn status_fingerprint_is_stable_for_identical_questions() {
        let questions = vec![question("继续")];
        assert_eq!(
            foreground_status_fingerprint(true, None, Some(&questions), Some("command"), None),
            foreground_status_fingerprint(true, None, Some(&questions), Some("command"), None)
        );
    }

    #[test]
    fn status_fingerprint_tracks_the_activity() {
        let idle = foreground_status_fingerprint(true, None, None, None, None);
        let working = foreground_status_fingerprint(true, None, None, None, Some("Bash cargo test"));
        assert_ne!(idle, working);
    }

    /// The sidecar carries the activity, and an activity-only change is held
    /// back rather than written once per streaming delta. Anything else moving
    /// writes immediately, activity included.
    #[test]
    fn the_status_carries_the_activity_and_holds_back_an_activity_only_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let projects_root = dir.path().to_path_buf();
        std::fs::create_dir_all(&projects_root).unwrap();
        let cwd = "F:/work/publish";
        let session_id = "sess-publish";
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.projects_root = projects_root.clone();
        session.cwd = cwd.into();
        session.session_id = session_id.into();
        session.session_active_lock = Some(
            rebon_session::try_acquire_session_active_lock(&projects_root, cwd, session_id)
                .expect("the lock probe works")
                .expect("the session lock is free"),
        );
        let mut mailbox = ForegroundMailbox::new(&session);
        let read = || {
            rebon_session_host::read_foreground_status(&projects_root, cwd, session_id)
                .expect("the status was written")
        };

        assert!(mailbox.publish(true, None, None, Some("Bash cargo test")));
        assert_eq!(read().activity.as_deref(), Some("Bash cargo test"));

        // Only the activity moved. The claim is still acknowledged — a caller
        // waits on `true` — but the file keeps the older activity until the
        // rewrite interval has passed.
        assert!(mailbox.publish(
            true,
            None,
            None,
            Some("Read crates/rebon-core/src/lib.rs")
        ));
        assert_eq!(read().activity.as_deref(), Some("Bash cargo test"));

        // The turn ending is not an activity change, so it writes at once.
        assert!(mailbox.publish(
            false,
            None,
            None,
            Some("Read crates/rebon-core/src/lib.rs")
        ));
        let settled = read();
        assert!(!settled.busy);
        assert_eq!(
            settled.activity.as_deref(),
            Some("Read crates/rebon-core/src/lib.rs")
        );
    }
}
