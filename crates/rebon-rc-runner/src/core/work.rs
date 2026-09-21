//! One work item: whether to take it, and what each thing that can happen
//! to it means.
//!
//! ```text
//! polled ──plan──▶ refuse ────────────────────────────▶ stop_work, done
//!    │               healthcheck ──ack──────────────────▶ stop_work, done
//!    └── session ──ack──▶ opening ──open failed──▶ failed state, stop_work, done
//!                           │
//!                           └──opened──▶ serving ──┬── lease lost ──────▶ stand down
//!                                           ▲      ├── stream: superseded / protocol error ─▶ retire
//!                                           │      ├── stream: lease gone ─▶ stand down
//!                                           │      ├── session stopped on the machine ─▶ retire
//!                                           │      ├── superseded by a newer item here ─▶ retire
//!                                           │      ├── shutdown ─────────▶ stand down
//!                                           └──────┴── stream: network / timeout / limit / close ─▶ reconnect
//! ```
//!
//! *Stand down* closes the stream and releases the session lease, and
//! leaves the work item alone: its lease is gone or about to lapse, and a
//! lapsed item that RC hands out again resumes the same session without
//! repeating its prompt (the ledger remembers which prompts ran). *Retire*
//! does the same and also stops the item, because nobody should be handed
//! it again.

use std::time::Duration;

use rebon_bridge::api_client::{BridgeApiError, BridgeApiResult};
use rebon_bridge::config::{valid_rebon_session_id, HeartbeatOutcome, WorkDataType, WorkItem};
use rebon_bridge::stream_client::{CloseReason, SessionStreamError};
use rebon_bridge::work_secret::WorkSecret;

/// Where a session comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeTarget {
    /// A brand-new local session.
    New,
    /// The item named this local session.
    Requested(String),
    /// The item named none, but this machine already ran the RC session
    /// as this local session.
    Remembered(String),
}

impl ResumeTarget {
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::New => None,
            Self::Requested(id) | Self::Remembered(id) => Some(id),
        }
    }
}

/// What this machine remembers about an RC session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remembered {
    pub rebon_session_id: String,
    pub project: String,
}

/// A session work item the runner will serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    pub work_id: String,
    pub rc_session_id: String,
    pub project: String,
    pub prompt: Option<String>,
    pub resume: ResumeTarget,
    pub secret: WorkSecret,
}

/// What to do with a polled item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkPlan {
    /// Acknowledge it and finish it: there is nothing to run.
    Healthcheck {
        work_id: String,
        secret: WorkSecret,
    },
    /// Stop it without running anything.
    Refuse {
        work_id: String,
        reason: String,
    },
    Session(SessionPlan),
}

/// Decide what to do with `item`.
///
/// `projects` is this machine's current list, compared byte for byte with
/// the item's project, as RC compares them. `remembered` looks an RC
/// session up in the local ledger. Everything here is external input — the
/// server is trusted to route, not to name paths — so every field is
/// checked again.
pub fn plan_work(
    item: &WorkItem,
    projects: &[String],
    remembered: impl Fn(&str) -> Option<Remembered>,
) -> WorkPlan {
    let work_id = item.response.id.clone();
    let refuse = |reason: String| WorkPlan::Refuse {
        work_id: work_id.clone(),
        reason,
    };
    let Some(secret) = WorkSecret::decode(&item.response.secret) else {
        return refuse("the work secret does not decode".into());
    };
    match item.response.data.data_type {
        WorkDataType::Healthcheck => {
            return WorkPlan::Healthcheck {
                work_id: work_id.clone(),
                secret,
            }
        }
        WorkDataType::Session => {}
    }
    let rc_session_id = item.response.data.id.clone();
    if secret.session_id.as_deref() != Some(rc_session_id.as_str()) {
        return refuse("the work secret is for another session".into());
    }
    let Some(session) = item.session.as_ref() else {
        return refuse("session work without a project (the server predates projects)".into());
    };
    if !projects.iter().any(|project| project == &session.project) {
        return refuse(format!(
            "this machine does not serve the project {}",
            session.project
        ));
    }
    let resume = match session.resume_rebon_session_id.as_deref() {
        Some(id) if !valid_rebon_session_id(id) => {
            return refuse("the resume target is not a plain session id".into())
        }
        Some(id) => ResumeTarget::Requested(id.to_string()),
        None => match remembered(&rc_session_id) {
            Some(known)
                if known.project == session.project
                    && valid_rebon_session_id(&known.rebon_session_id) =>
            {
                ResumeTarget::Remembered(known.rebon_session_id)
            }
            _ => ResumeTarget::New,
        },
    };
    let prompt = session
        .prompt
        .clone()
        .filter(|prompt| !prompt.trim().is_empty());
    if prompt.is_none() && resume == ResumeTarget::New {
        return refuse("session work with neither a prompt nor a session to resume".into());
    }
    WorkPlan::Session(SessionPlan {
        work_id,
        rc_session_id,
        project: session.project.clone(),
        prompt,
        resume,
        secret,
    })
}

/// How a work item's run ended, and what that obliges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Leave the item; its lease is gone or will lapse.
    StandDown(String),
    /// Stop the item too.
    Retire(String),
}

impl Exit {
    pub fn retires(&self) -> bool {
        matches!(self, Self::Retire(_))
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::StandDown(reason) | Self::Retire(reason) => reason,
        }
    }
}

/// What to do after the session stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamVerdict {
    Reconnect,
    Exit(Exit),
}

/// The stream closed with `reason`.
pub fn after_close(reason: &CloseReason) -> StreamVerdict {
    match reason {
        CloseReason::Superseded => StreamVerdict::Exit(Exit::Retire(
            "another worker connection took this session".into(),
        )),
        CloseReason::LeaseGone => {
            StreamVerdict::Exit(Exit::StandDown("the work item's lease is gone".into()))
        }
        CloseReason::ProtocolError => StreamVerdict::Exit(Exit::Retire(
            "the server refused a frame as malformed".into(),
        )),
        CloseReason::Normal
        | CloseReason::Timeout
        | CloseReason::ResourceLimit
        | CloseReason::Network
        | CloseReason::Other { .. } => StreamVerdict::Reconnect,
    }
}

/// Connecting (or sending) failed with `error`.
pub fn after_stream_error(error: &SessionStreamError) -> StreamVerdict {
    if error.is_lease_gone() {
        return StreamVerdict::Exit(Exit::StandDown("the work item's lease is gone".into()));
    }
    match error {
        SessionStreamError::Closed(reason) => after_close(reason),
        SessionStreamError::Rejected { status: 401, .. } => {
            StreamVerdict::Exit(Exit::StandDown("the session token was refused".into()))
        }
        // A frame that could not be encoded, or was too big, is this
        // runner's problem, not the connection's.
        _ if error.is_transient() => StreamVerdict::Reconnect,
        SessionStreamError::Protocol(_) => StreamVerdict::Reconnect,
        other => StreamVerdict::Exit(Exit::Retire(format!(
            "the session stream cannot be opened: {other}"
        ))),
    }
}

/// What a heartbeat says about the lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseVerdict {
    Held,
    Lost(String),
    /// Unknown this time; ask again.
    Unknown,
}

pub fn after_heartbeat(result: &BridgeApiResult<HeartbeatOutcome>) -> LeaseVerdict {
    match result {
        Ok(outcome) if outcome.lease_extended => LeaseVerdict::Held,
        Ok(outcome) => {
            LeaseVerdict::Lost(format!("the lease was not extended ({})", outcome.state))
        }
        // 404 / 409: the item is gone or no longer this worker's.
        Err(BridgeApiError::Permanent(detail)) => LeaseVerdict::Lost(detail.clone()),
        // A rotated environment secret is the serve loop's to fix, and a
        // network failure is worth another try; the lease itself may well
        // still be held.
        Err(_) => LeaseVerdict::Unknown,
    }
}

/// Exponential backoff with a ceiling.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempt: 0,
        }
    }

    /// The delay before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let factor = 1u32 << self.attempt.min(16);
        self.attempt = self.attempt.saturating_add(1);
        self.base.saturating_mul(factor).min(self.max)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn attempts(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_bridge::config::{SessionWork, WorkData, WorkResponse};

    fn secret(session: Option<&str>) -> String {
        WorkSecret {
            session_token: "token".into(),
            session_id: session.map(str::to_string),
            ingress_url: "ws://127.0.0.1:1/v1/sessions/s/stream".into(),
        }
        .encode()
    }

    fn item(session: Option<SessionWork>) -> WorkItem {
        WorkItem {
            response: WorkResponse {
                id: "wrk_1".into(),
                response_type: "work".into(),
                environment_id: "env_1".into(),
                state: "leased".into(),
                data: WorkData {
                    data_type: WorkDataType::Session,
                    id: "sess_1".into(),
                },
                secret: secret(Some("sess_1")),
                created_at: "now".into(),
            },
            session,
        }
    }

    fn work(project: &str, prompt: Option<&str>, resume: Option<&str>) -> SessionWork {
        SessionWork {
            project: project.into(),
            prompt: prompt.map(str::to_string),
            resume_rebon_session_id: resume.map(str::to_string),
        }
    }

    fn projects() -> Vec<String> {
        vec!["/srv/app".into(), r"D:\work\docs".into()]
    }

    fn nothing(_: &str) -> Option<Remembered> {
        None
    }

    fn refused(plan: WorkPlan) -> String {
        match plan {
            WorkPlan::Refuse { reason, work_id } => {
                assert_eq!(work_id, "wrk_1");
                reason
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_new_session_with_a_prompt_is_served() {
        let plan = plan_work(
            &item(Some(work("/srv/app", Some("fix it"), None))),
            &projects(),
            nothing,
        );
        let WorkPlan::Session(plan) = plan else {
            panic!("expected session work");
        };
        assert_eq!(plan.work_id, "wrk_1");
        assert_eq!(plan.rc_session_id, "sess_1");
        assert_eq!(plan.project, "/srv/app");
        assert_eq!(plan.prompt.as_deref(), Some("fix it"));
        assert_eq!(plan.resume, ResumeTarget::New);
        assert_eq!(plan.secret.session_token, "token");
    }

    #[test]
    fn a_resume_target_is_taken_from_the_item_then_from_memory() {
        let requested = plan_work(
            &item(Some(work(r"D:\work\docs", None, Some("local-1")))),
            &projects(),
            |_| {
                Some(Remembered {
                    rebon_session_id: "local-other".into(),
                    project: r"D:\work\docs".into(),
                })
            },
        );
        let WorkPlan::Session(plan) = requested else {
            panic!("expected session work");
        };
        assert_eq!(plan.resume, ResumeTarget::Requested("local-1".into()));
        assert_eq!(plan.resume.session_id(), Some("local-1"));
        assert_eq!(plan.prompt, None);

        let remembered = plan_work(
            &item(Some(work("/srv/app", None, None))),
            &projects(),
            |rc| {
                assert_eq!(rc, "sess_1");
                Some(Remembered {
                    rebon_session_id: "local-2".into(),
                    project: "/srv/app".into(),
                })
            },
        );
        let WorkPlan::Session(plan) = remembered else {
            panic!("expected session work");
        };
        assert_eq!(plan.resume, ResumeTarget::Remembered("local-2".into()));

        // A memory from another project is not used; with no prompt either,
        // there is nothing to do.
        let elsewhere = plan_work(
            &item(Some(work("/srv/app", None, None))),
            &projects(),
            |_| {
                Some(Remembered {
                    rebon_session_id: "local-2".into(),
                    project: "/elsewhere".into(),
                })
            },
        );
        assert!(refused(elsewhere).contains("neither a prompt"));
        let with_prompt = plan_work(
            &item(Some(work("/srv/app", Some("go"), None))),
            &projects(),
            |_| {
                Some(Remembered {
                    rebon_session_id: "local-2".into(),
                    project: "/elsewhere".into(),
                })
            },
        );
        assert!(matches!(
            with_prompt,
            WorkPlan::Session(SessionPlan {
                resume: ResumeTarget::New,
                ..
            })
        ));
    }

    #[test]
    fn malformed_items_are_refused() {
        // No session object.
        assert!(refused(plan_work(&item(None), &projects(), nothing)).contains("without a project"));
        // A project this machine does not serve, including a near miss.
        for project in ["/etc", "/srv/app/", "/SRV/APP"] {
            assert!(
                refused(plan_work(
                    &item(Some(work(project, Some("x"), None))),
                    &projects(),
                    nothing
                ))
                .contains("does not serve"),
                "{project}"
            );
        }
        // A resume target that would name a path.
        for target in ["../x", ".hidden", "a/b", ""] {
            assert!(
                refused(plan_work(
                    &item(Some(work("/srv/app", Some("x"), Some(target)))),
                    &projects(),
                    nothing
                ))
                .contains("plain session id"),
                "{target}"
            );
        }
        // A blank prompt is no prompt.
        assert!(refused(plan_work(
            &item(Some(work("/srv/app", Some("  "), None))),
            &projects(),
            nothing
        ))
        .contains("neither"));
        // A secret that does not decode, or is for another session.
        let mut garbled = item(Some(work("/srv/app", Some("x"), None)));
        garbled.response.secret = "!!".into();
        assert!(refused(plan_work(&garbled, &projects(), nothing)).contains("decode"));
        let mut foreign = item(Some(work("/srv/app", Some("x"), None)));
        foreign.response.secret = secret(Some("sess_other"));
        assert!(refused(plan_work(&foreign, &projects(), nothing)).contains("another session"));
        // Nothing is served when the machine serves nothing.
        assert!(refused(plan_work(
            &item(Some(work("/srv/app", Some("x"), None))),
            &[],
            nothing
        ))
        .contains("does not serve"));
    }

    #[test]
    fn a_healthcheck_is_acknowledged_and_finished() {
        let mut probe = item(None);
        probe.response.data = WorkData {
            data_type: WorkDataType::Healthcheck,
            id: "wrk_1".into(),
        };
        probe.response.secret = secret(None);
        assert!(matches!(
            plan_work(&probe, &projects(), nothing),
            WorkPlan::Healthcheck { .. }
        ));
    }

    #[test]
    fn every_close_reason_has_a_verdict() {
        let reconnect = [
            CloseReason::Normal,
            CloseReason::Timeout,
            CloseReason::ResourceLimit,
            CloseReason::Network,
            CloseReason::Other {
                code: 4999,
                reason: String::new(),
            },
        ];
        for reason in reconnect {
            assert_eq!(after_close(&reason), StreamVerdict::Reconnect, "{reason:?}");
        }
        assert!(matches!(
            after_close(&CloseReason::Superseded),
            StreamVerdict::Exit(Exit::Retire(_))
        ));
        assert!(matches!(
            after_close(&CloseReason::ProtocolError),
            StreamVerdict::Exit(Exit::Retire(_))
        ));
        assert!(matches!(
            after_close(&CloseReason::LeaseGone),
            StreamVerdict::Exit(Exit::StandDown(_))
        ));
    }

    #[test]
    fn stream_errors_split_into_retry_and_stop() {
        use rebon_bridge::api_client::BridgeApiError as E;
        let rejected = |status: u16, error: E| SessionStreamError::Rejected { status, error };
        assert!(matches!(
            after_stream_error(&rejected(409, E::Permanent("x".into()))),
            StreamVerdict::Exit(Exit::StandDown(_))
        ));
        assert!(matches!(
            after_stream_error(&SessionStreamError::Closed(CloseReason::LeaseGone)),
            StreamVerdict::Exit(Exit::StandDown(_))
        ));
        assert!(matches!(
            after_stream_error(&rejected(401, E::Unauthorized("x".into()))),
            StreamVerdict::Exit(Exit::StandDown(_))
        ));
        assert!(matches!(
            after_stream_error(&rejected(403, E::Permanent("x".into()))),
            StreamVerdict::Exit(Exit::Retire(_))
        ));
        assert!(matches!(
            after_stream_error(&SessionStreamError::Config("bad url".into())),
            StreamVerdict::Exit(Exit::Retire(_))
        ));
        for retry in [
            rejected(503, E::Transient("x".into())),
            SessionStreamError::Transport("reset".into()),
            SessionStreamError::Closed(CloseReason::Network),
            SessionStreamError::Protocol("frame too big".into()),
        ] {
            assert_eq!(
                after_stream_error(&retry),
                StreamVerdict::Reconnect,
                "{retry:?}"
            );
        }
        assert!(matches!(
            after_stream_error(&SessionStreamError::Closed(CloseReason::Superseded)),
            StreamVerdict::Exit(Exit::Retire(_))
        ));
    }

    #[test]
    fn heartbeats_decide_the_lease() {
        let outcome = |extended: bool| {
            Ok(HeartbeatOutcome {
                lease_extended: extended,
                state: "acked".into(),
            })
        };
        assert_eq!(after_heartbeat(&outcome(true)), LeaseVerdict::Held);
        assert!(matches!(
            after_heartbeat(&outcome(false)),
            LeaseVerdict::Lost(_)
        ));
        assert!(matches!(
            after_heartbeat(&Err(BridgeApiError::Permanent("409".into()))),
            LeaseVerdict::Lost(_)
        ));
        for unknown in [
            BridgeApiError::Transient("x".into()),
            BridgeApiError::Unauthorized("x".into()),
            BridgeApiError::Protocol("x".into()),
            BridgeApiError::Other("x".into()),
        ] {
            assert_eq!(after_heartbeat(&Err(unknown)), LeaseVerdict::Unknown);
        }
    }

    #[test]
    fn exits_say_whether_they_retire() {
        assert!(Exit::Retire("a".into()).retires());
        assert!(!Exit::StandDown("b".into()).retires());
        assert_eq!(Exit::StandDown("b".into()).reason(), "b");
    }

    #[test]
    fn backoff_doubles_up_to_its_ceiling() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(1));
        let delays: Vec<u128> = (0..6).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(delays, vec![100, 200, 400, 800, 1000, 1000]);
        assert_eq!(backoff.attempts(), 6);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
        // Many attempts never overflow.
        for _ in 0..100 {
            backoff.next_delay();
        }
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }
}
