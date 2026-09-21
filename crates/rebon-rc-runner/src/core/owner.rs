//! Who holds a session, and what the runner may do about it.
//!
//! The same four owner states `rebon serve` follows, with the
//! same two rules: an owner that holds the session but will not answer is
//! never taken over, and a worker the user stopped is never brought back
//! from here.
//!
//! | Owner | Job record | Opening | Following |
//! |---|---|---|---|
//! | reachable | — | attach, live events only | connect |
//! | unreachable | — | refuse | wait (read-only) |
//! | opaque (a terminal hosts it) | — | refuse | wait (read-only) |
//! | free | none | give it a worker | give it a worker |
//! | free | worker starting (pid or queued) | wait for it | wait |
//! | free | stopped | refuse | end: the user stopped it |
//! | free | worker gone | queue a replacement | queue a replacement |

/// What is known about a session's owner, reduced to what decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerView {
    Reachable,
    Unreachable,
    Opaque,
    Free(JobView),
}

/// The session's home job, as far as the decision goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobView {
    None,
    /// A worker process is recorded, or the job is queued for one.
    Starting,
    /// The user stopped it.
    Stopped,
    /// There was a worker and it is gone.
    Gone,
}

/// Opening a session for a work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAction {
    /// A live owner: follow it from now on.
    Attach,
    /// A worker is on its way.
    AwaitWorker,
    /// Queue a replacement worker on the existing job.
    Revive,
    /// Give the session a job and a worker.
    Host,
    Refuse(Refusal),
}

/// Following a session that is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowAction {
    Connect,
    /// Poll again soon: a worker is starting.
    Wait,
    /// Poll again, slowly: somebody else holds it.
    WaitReadOnly(Refusal),
    Revive,
    Host,
    /// The session is over as far as this machine is concerned.
    End(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Unreachable,
    HeldByTerminal,
    Stopped,
}

impl Refusal {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Unreachable => {
                "the session is held by a Rebon process on this machine that is not answering"
            }
            Self::HeldByTerminal => {
                "the session is open in a terminal on this machine; it can be driven from there"
            }
            Self::Stopped => "the session was stopped on this machine",
        }
    }
}

pub fn open_action(view: OwnerView) -> OpenAction {
    match view {
        OwnerView::Reachable => OpenAction::Attach,
        OwnerView::Unreachable => OpenAction::Refuse(Refusal::Unreachable),
        OwnerView::Opaque => OpenAction::Refuse(Refusal::HeldByTerminal),
        OwnerView::Free(JobView::None) => OpenAction::Host,
        OwnerView::Free(JobView::Starting) => OpenAction::AwaitWorker,
        OwnerView::Free(JobView::Stopped) => OpenAction::Refuse(Refusal::Stopped),
        OwnerView::Free(JobView::Gone) => OpenAction::Revive,
    }
}

pub fn follow_action(view: OwnerView) -> FollowAction {
    match view {
        OwnerView::Reachable => FollowAction::Connect,
        OwnerView::Unreachable => FollowAction::WaitReadOnly(Refusal::Unreachable),
        OwnerView::Opaque => FollowAction::WaitReadOnly(Refusal::HeldByTerminal),
        OwnerView::Free(JobView::None) => FollowAction::Host,
        OwnerView::Free(JobView::Starting) => FollowAction::Wait,
        OwnerView::Free(JobView::Stopped) => FollowAction::End(Refusal::Stopped),
        OwnerView::Free(JobView::Gone) => FollowAction::Revive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_VIEW: [OwnerView; 7] = [
        OwnerView::Reachable,
        OwnerView::Unreachable,
        OwnerView::Opaque,
        OwnerView::Free(JobView::None),
        OwnerView::Free(JobView::Starting),
        OwnerView::Free(JobView::Stopped),
        OwnerView::Free(JobView::Gone),
    ];

    #[test]
    fn opening_follows_the_table() {
        let actions: Vec<OpenAction> = EVERY_VIEW.into_iter().map(open_action).collect();
        assert_eq!(
            actions,
            vec![
                OpenAction::Attach,
                OpenAction::Refuse(Refusal::Unreachable),
                OpenAction::Refuse(Refusal::HeldByTerminal),
                OpenAction::Host,
                OpenAction::AwaitWorker,
                OpenAction::Refuse(Refusal::Stopped),
                OpenAction::Revive,
            ]
        );
    }

    #[test]
    fn following_follows_the_table() {
        let actions: Vec<FollowAction> = EVERY_VIEW.into_iter().map(follow_action).collect();
        assert_eq!(
            actions,
            vec![
                FollowAction::Connect,
                FollowAction::WaitReadOnly(Refusal::Unreachable),
                FollowAction::WaitReadOnly(Refusal::HeldByTerminal),
                FollowAction::Host,
                FollowAction::Wait,
                FollowAction::End(Refusal::Stopped),
                FollowAction::Revive,
            ]
        );
    }

    #[test]
    fn an_owner_that_holds_the_session_is_never_taken_over() {
        for view in [OwnerView::Unreachable, OwnerView::Opaque] {
            assert!(matches!(open_action(view), OpenAction::Refuse(_)));
            assert!(matches!(follow_action(view), FollowAction::WaitReadOnly(_)));
        }
    }

    #[test]
    fn a_stopped_worker_is_never_brought_back() {
        let stopped = OwnerView::Free(JobView::Stopped);
        assert!(!matches!(
            open_action(stopped),
            OpenAction::Revive | OpenAction::Host
        ));
        assert!(!matches!(
            follow_action(stopped),
            FollowAction::Revive | FollowAction::Host
        ));
        assert!(Refusal::Stopped.describe().contains("stopped"));
    }
}
