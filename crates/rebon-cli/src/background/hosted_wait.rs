//! How long a terminal waits for the worker a session was handed to, and how
//! often it asks.
//!
//! Nothing here blocks. The wait is a state an event loop polls,
//! so all this module owns is the arithmetic of that poll: how eagerly to probe
//! while the worker is expected any moment, how long to stay quiet before
//! saying the start is slow, when to give up, and — once a job has ended
//! instead of coming up — what it did and why, in a word each.
//!
//! The values are policy, not presentation. Which of them produces a row on a
//! screen, and what that row says, is
//! `crate::tui::runner::session_detach_attach`'s business.

/// How long a session waits for its worker's endpoint before it stops
/// waiting: a process start plus engine boot, with room. Nothing blocks on
/// this — the wait is a state the event loop polls, and the
/// terminal draws, takes input and queues prompts on the job throughout.
/// Past it the wait is given up and the session parked on its job, where a
/// late worker is still followed and the next prompt starts another; only
/// a dispatch that was never on screen lets go of the job instead.
pub(crate) const HOSTED_WAIT_GIVE_UP_AFTER: std::time::Duration =
    std::time::Duration::from_secs(60);

/// How often the job store may be read while waiting for a worker. The event
/// loop runs an order of magnitude faster than a process starts, and each
/// probe reads job state, reconciles a possibly-dead pid and can ping a TCP
/// endpoint — on the UI thread. Nothing is lost by asking five times a
/// second instead of every frame.
pub(crate) const HOSTED_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// The cadence for the first second of a wait, when the worker is expected
/// any moment: at startup its endpoint is up ~100 ms after the spawn and
/// the session installs ~350–450 ms after it, so the endpoint is usually
/// there by the first probe and the mirror attached a frame later rather
/// than up to 200 ms later. A worker that has taken a second will take
/// more, and the slower cadence is enough for it.
pub(crate) const HOSTED_PROBE_INTERVAL_EAGER: std::time::Duration =
    std::time::Duration::from_millis(25);
pub(crate) const HOSTED_PROBE_EAGER_FOR: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) fn hosted_probe_interval(
    pending: &crate::background::PendingHostedSession,
) -> std::time::Duration {
    if pending.started_at.elapsed() < HOSTED_PROBE_EAGER_FOR {
        HOSTED_PROBE_INTERVAL_EAGER
    } else {
        HOSTED_PROBE_INTERVAL
    }
}

/// How long the default startup waits for its worker before saying anything
/// in the transcript. Until then the status bar's dot is the only sign.
pub(crate) const HOSTED_STARTUP_SLOW_NOTICE: std::time::Duration =
    std::time::Duration::from_secs(30);

/// A job in one of these states has stopped being a worker anyone can
/// mirror. Waiting for it to publish an endpoint is waiting for something
/// that already happened, differently.
fn hosted_wait_is_over(status: crate::background::BackgroundJobStatus) -> bool {
    use crate::background::BackgroundJobStatus as Status;
    matches!(status, Status::Succeeded | Status::Failed | Status::Stopped)
}

/// What a job that ended before we could mirror it did, in a word.
fn hosted_terminal_verb(status: crate::background::BackgroundJobStatus) -> &'static str {
    use crate::background::BackgroundJobStatus as Status;
    match status {
        Status::Succeeded => "finished",
        Status::Failed => "failed",
        _ => "was stopped",
    }
}

/// Why the job ended, if it recorded a reason worth repeating.
fn hosted_terminal_detail(
    store: &crate::background::BackgroundStore,
    job_id: &str,
) -> Option<String> {
    let state = store.read_state(job_id).ok()?;
    let reason = state
        .outcome
        .error
        .as_deref()
        .or(state.outcome.summary.as_deref())?
        .lines()
        .next()?
        .trim()
        .to_string();
    if reason.is_empty() {
        return None;
    }
    Some(reason.chars().take(160).collect())
}

/// What one look at a hosted wait found.
///
/// The wait is a state an event loop polls, and the poll is in two parts
/// because the middle of it needs a screen: only a terminal can apply an attach
/// target, so the probe stops at "there is one" and the settle picks up after
/// the caller has tried it. A refused attach is not a special case on the way
/// back — it re-enters the settle as [`HostedWaitProbe::NotYet`] with the
/// refusal as its reason, and the budget decides what that means, exactly as it
/// did when both halves were one function.
pub(crate) enum HostedWaitProbe {
    /// No wait, or not due to probe this frame.
    Skip,
    /// The session this wait was started for is no longer the one on screen, so
    /// the wait retired itself. Every path that drops the job settles the claim
    /// itself; deciding that here, from a wait already stale, would guess.
    Retired,
    /// The worker published an endpoint. The caller attaches with the wait's
    /// kind and reports the result to [`settle_hosted_wait`].
    Mirrorable {
        target: Box<crate::background::BackgroundAttachTarget>,
        probe_ms: u64,
    },
    /// No endpoint yet and the job is not over.
    NotYet { reason: Option<String> },
    /// The job ended before it could be mirrored. Waiting for it to publish an
    /// endpoint is waiting for something that already happened, differently.
    Ended {
        status: crate::background::BackgroundJobStatus,
    },
}

/// Look at the wait this session is holding, if it is due a look.
///
/// Read-only against the job store on purpose. The takeover-flavoured attach
/// releases a job whose owner is not running, and a `Queued` job waiting for
/// the supervisor has no owner yet — polling with it stopped the very handover
/// it was waiting for. See `mirrorable_background_job_in_store`.
pub(crate) fn probe_hosted_wait(
    session: &mut crate::session_shell::TuiEngineSession,
    store: &crate::background::BackgroundStore,
) -> HostedWaitProbe {
    let Some(pending) = session.pending_hosted_session.as_ref() else {
        return HostedWaitProbe::Skip;
    };
    let job_id = pending.job_id.clone();

    if session.attached_background_job_id.as_deref() != Some(job_id.as_str()) {
        session.pending_hosted_session = None;
        tracing::debug!(%job_id, "hosted wait: session changed underneath, dropping it");
        return HostedWaitProbe::Retired;
    }

    let probe_now = session
        .pending_hosted_session
        .as_mut()
        .is_some_and(|pending| {
            let interval = hosted_probe_interval(pending);
            pending.should_probe_now(interval)
        });
    if !probe_now {
        return HostedWaitProbe::Skip;
    }

    let probe_started = std::time::Instant::now();
    match crate::background::mirrorable_background_job_in_store(store, &job_id) {
        Ok(Some(target)) => HostedWaitProbe::Mirrorable {
            target: Box::new(target),
            probe_ms: probe_started.elapsed().as_millis() as u64,
        },
        // Not mirrorable yet. Only the job's own status can say whether that
        // is "still starting" or "already over".
        Ok(None) => match store.read_state(&job_id) {
            Ok(state) if hosted_wait_is_over(state.process.status) => HostedWaitProbe::Ended {
                status: state.process.status,
            },
            Ok(_) => HostedWaitProbe::NotYet {
                reason: Some("the worker has not published an endpoint yet".to_string()),
            },
            Err(err) => HostedWaitProbe::NotYet {
                reason: Some(err.to_string()),
            },
        },
        Err(err) => {
            if let Ok(state) = store.read_state(&job_id) {
                if hosted_wait_is_over(state.process.status) {
                    return HostedWaitProbe::Ended {
                        status: state.process.status,
                    };
                }
            }
            HostedWaitProbe::NotYet {
                reason: Some(err.to_string()),
            }
        }
    }
}

/// What the terminal has to say and do about the wait, after any attach it was
/// asked to try.
///
/// The facts only. Which of these produces a row, and what the row says, is the
/// terminal's — as is parking, which builds an attachment over the rows already
/// on screen and therefore needs one.
pub(crate) enum HostedWaitReport {
    /// Nothing happened worth a word.
    Quiet,
    /// The session is now mirrored. `startup` is the ordinary start of a
    /// session, which says nothing: the dot in the status bar goes away, and
    /// that is the whole event.
    Attached {
        job_id: String,
        kind: crate::background::PendingHostedKind,
        elapsed_ms: u64,
        probe_ms: u64,
        startup: bool,
    },
    /// The job ended before it could be mirrored.
    Ended {
        job_id: String,
        kind: crate::background::PendingHostedKind,
        status: crate::background::BackgroundJobStatus,
        verb: &'static str,
        detail: Option<String>,
        park: Option<crate::background::BackgroundJobStatus>,
    },
    /// The budget is spent. Giving up waiting is not the same as getting the
    /// session back: a worker may still come up, so a wait that kept its
    /// session asks for it to be parked rather than dropped.
    GaveUp {
        job_id: String,
        kind: crate::background::PendingHostedKind,
        budget_secs: u64,
        reason: Option<String>,
        park: Option<crate::background::BackgroundJobStatus>,
    },
}

/// The settle's whole answer.
///
/// `slow_notice` is separate from `report` rather than a variant of it because
/// the two can both be true of one frame: a startup wait polled for the first
/// time after its budget ran out both says it was slow and gives up, and it
/// said both before this split.
pub(crate) struct HostedWaitSettled {
    /// A startup wait that has been quiet too long, named once.
    pub(crate) slow_notice: Option<String>,
    pub(crate) report: HostedWaitReport,
}

/// Settle the wait against what the probe found and what the attach did.
pub(crate) fn settle_hosted_wait(
    session: &mut crate::session_shell::TuiEngineSession,
    store: &crate::background::BackgroundStore,
    probe: HostedWaitProbe,
    attached: bool,
) -> HostedWaitSettled {
    fn only(report: HostedWaitReport) -> HostedWaitSettled {
        HostedWaitSettled {
            slow_notice: None,
            report,
        }
    }

    let Some(pending) = session.pending_hosted_session.as_ref() else {
        return only(HostedWaitReport::Quiet);
    };
    let job_id = pending.job_id.clone();
    let kind = pending.kind;
    let elapsed = pending.started_at.elapsed();

    let (refusal, ended) = match probe {
        HostedWaitProbe::Skip | HostedWaitProbe::Retired => return only(HostedWaitReport::Quiet),
        HostedWaitProbe::Mirrorable { probe_ms, .. } => {
            if attached {
                session.pending_hosted_session = None;
                return only(HostedWaitReport::Attached {
                    job_id,
                    kind,
                    elapsed_ms: elapsed.as_millis() as u64,
                    probe_ms,
                    startup: kind == crate::background::PendingHostedKind::Startup,
                });
            }
            // The endpoint is up but attaching was refused — a lock, or a
            // race with another client. One refusal does not end the wait;
            // the budget does.
            (
                Some("attaching to the worker was refused".to_string()),
                None,
            )
        }
        HostedWaitProbe::NotYet { reason } => (reason, None),
        HostedWaitProbe::Ended { status } => (None, Some(status)),
    };

    if let Some(status) = ended {
        session.pending_hosted_session = None;
        let detail = hosted_terminal_detail(store, &job_id);
        let park = match kind {
            // Nothing of this session went anywhere: the job it was only
            // waiting to mirror is over, and the session stays local.
            crate::background::PendingHostedKind::Dispatch
            | crate::background::PendingHostedKind::Reattach { keep_view: false } => {
                session.attached_background_job_id = None;
                None
            }
            // The job is over, so no worker will resume this session. It does
            // not come back here either: the session on screen stays the
            // job's, parked, and the next prompt gives the job a worker.
            _ => Some(status),
        };
        return only(HostedWaitReport::Ended {
            job_id,
            kind,
            status,
            verb: hosted_terminal_verb(status),
            detail,
            park,
        });
    }

    // The ordinary start of a session has one dot for a signal, and words
    // only when it has clearly gone wrong: a notice after thirty seconds,
    // once, with the way out.
    let mut slow_notice = None;
    if kind == crate::background::PendingHostedKind::Startup
        && elapsed >= HOSTED_STARTUP_SLOW_NOTICE
    {
        if let Some(pending) = session
            .pending_hosted_session
            .as_mut()
            .filter(|pending| !pending.slow_notice_shown)
        {
            pending.slow_notice_shown = true;
            slow_notice = Some(job_id.clone());
        }
    }
    if elapsed < HOSTED_WAIT_GIVE_UP_AFTER {
        if let Some(reason) = refusal {
            tracing::debug!(%job_id, %reason, "hosted wait: still waiting");
        }
        return HostedWaitSettled {
            slow_notice,
            report: HostedWaitReport::Quiet,
        };
    }

    session.pending_hosted_session = None;
    let park = match kind {
        // Nothing was handed over — the job is running on its own and only
        // the view is missing.
        crate::background::PendingHostedKind::Dispatch
        | crate::background::PendingHostedKind::Reattach { keep_view: false } => {
            session.attached_background_job_id = None;
            None
        }
        // The job is not over — it is slow — so a worker may still come up
        // and resume this very session.
        _ => Some(
            store
                .read_state(&job_id)
                .map(|state| state.process.status)
                .unwrap_or(crate::background::BackgroundJobStatus::Queued),
        ),
    };
    HostedWaitSettled {
        slow_notice,
        report: HostedWaitReport::GaveUp {
            job_id,
            kind,
            budget_secs: HOSTED_WAIT_GIVE_UP_AFTER.as_secs(),
            reason: refusal,
            park,
        },
    }
}
