//! The loop a session's turns run through, one at a time.
//!
//! Every surface that runs turns (the terminal, the hosted worker, an ACP
//! server) hands them here instead of calling the prompt executor itself.
//! The loop takes inputs in the order they arrive and runs each to the end
//! before the next, so a session's history has a single writer: a turn, or
//! work that rewrites history between turns (a compaction).
//!
//! This is the part every surface shares. It knows a turn only as a future
//! that ends in a [`TurnResult`]; what a surface wraps around one (its
//! runtime, what the turn delivers, what waits for the next turn) lives with
//! the surface. `rebon_session_runtime::session_loop` is the terminal's and
//! the worker's.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use crate::{PromptExecutorError, PromptOutcome};

/// How a turn ended.
pub type TurnResult = Result<PromptOutcome, PromptExecutorError>;

/// One turn for the loop to run.
pub struct TurnJob {
    /// The turn. It runs on a task of its own, so a turn that panics reports
    /// an error instead of taking the loop down.
    pub run: Pin<Box<dyn Future<Output = TurnResult> + Send>>,
    /// What diagnostics call a turn of this kind ("main", "permission
    /// replay").
    pub kind: &'static str,
    /// Acknowledge what the turn delivered, once it has ended and before
    /// anyone hears how: a caller that stops waiting leaves nothing
    /// unsettled, and one that gets the result finds it settled.
    pub settle: Option<Box<dyn FnOnce(&TurnResult) + Send>>,
    /// Stop the turn once nobody is waiting for its result. ACP aborts a
    /// turn by dropping the request that runs it (a client that hung up, a
    /// request the server gave up on), and the turn has to stop with it.
    /// The terminal and the worker never do: they cancel through the
    /// prompt's cancel token, so the turn winds down and records that it
    /// was interrupted, and a dropped handle must not cut that short.
    pub stop_when_abandoned: bool,
}

/// Work that rewrites a session's history, and how it shares the loop.
pub struct HistoryWork {
    /// The work. It reports its own progress and result.
    pub work: Pin<Box<dyn Future<Output = ()> + Send>>,
    /// Work nobody asked for (an idle compaction) gives way: any input that
    /// arrives while it runs cancels it and goes next, so the person never
    /// waits on it. Work someone asked for (`/compact`) runs to the end,
    /// and a turn sent meanwhile waits its turn.
    pub yields: bool,
}

enum LoopInput {
    Turn {
        job: TurnJob,
        reply: oneshot::Sender<TurnResult>,
    },
    HistoryWork(HistoryWork),
}

/// The sending end of a session's loop. The loop task starts on the first
/// input and restarts if the runtime it ran on has gone away.
#[derive(Default)]
pub struct TurnLoop {
    sender: Mutex<Option<mpsc::UnboundedSender<LoopInput>>>,
}

impl TurnLoop {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand the loop a turn. Returns the receiver its result arrives on.
    pub fn submit_turn(&self, handle: &Handle, job: TurnJob) -> oneshot::Receiver<TurnResult> {
        let (reply, result) = oneshot::channel();
        self.send(handle, LoopInput::Turn { job, reply });
        result
    }

    /// Hand the loop work that rewrites history (see [`HistoryWork`]).
    pub fn submit_history_work(&self, handle: &Handle, work: HistoryWork) {
        self.send(handle, LoopInput::HistoryWork(work));
    }

    fn send(&self, handle: &Handle, input: LoopInput) {
        let mut sender = self.sender.lock().expect("turn loop sender poisoned");
        let input = match sender.as_ref() {
            Some(tx) => match tx.send(input) {
                Ok(()) => return,
                // The loop's runtime went away; start it again below.
                Err(mpsc::error::SendError(input)) => input,
            },
            None => input,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        handle.spawn(run_loop(rx));
        // A fresh receiver is alive until this function returns.
        let _ = tx.send(input);
        *sender = Some(tx);
    }
}

/// The loop itself: take the next input, act on it, repeat.
async fn run_loop(mut inputs: mpsc::UnboundedReceiver<LoopInput>) {
    // An input that arrived while yielding work ran, and cancelled it.
    let mut next = None;
    loop {
        let input = match next.take() {
            Some(input) => input,
            None => match inputs.recv().await {
                Some(input) => input,
                None => break,
            },
        };
        match input {
            LoopInput::Turn { job, mut reply } => {
                let TurnJob {
                    run,
                    kind,
                    settle,
                    stop_when_abandoned,
                } = job;
                let mut task = tokio::spawn(run);
                let joined = if stop_when_abandoned {
                    tokio::select! {
                        joined = &mut task => Some(joined),
                        () = reply.closed() => {
                            task.abort();
                            let _ = task.await;
                            None
                        }
                    }
                } else {
                    Some(task.await)
                };
                let result = match joined {
                    Some(Ok(result)) => result,
                    Some(Err(err)) => Err(join_error(kind, err)),
                    None => Err(PromptExecutorError::Cancelled),
                };
                if let Some(settle) = settle {
                    settle(&result);
                }
                let _ = reply.send(result);
            }
            LoopInput::HistoryWork(HistoryWork {
                work,
                yields: false,
            }) => work.await,
            LoopInput::HistoryWork(HistoryWork { work, yields: true }) => {
                tokio::select! {
                    () = work => {}
                    arrived = inputs.recv() => match arrived {
                        // Dropping the work cancels it.
                        Some(input) => next = Some(input),
                        None => break,
                    },
                }
            }
        }
    }
}

fn join_error(turn_kind: &'static str, err: tokio::task::JoinError) -> PromptExecutorError {
    if err.is_panic() {
        let join_error = err.to_string();
        let panic = panic_payload_message(err.into_panic());
        tracing::error!(
            turn_kind,
            panic = %panic,
            join_error = %join_error,
            "rebon: prompt executor task panicked"
        );
        return PromptExecutorError::Execution(format!(
            "{turn_kind} prompt executor task panicked: {panic}"
        ));
    }
    let message = if err.is_cancelled() {
        format!("{turn_kind} prompt executor task was aborted before reporting a result")
    } else {
        format!("{turn_kind} prompt executor task failed before reporting a result: {err}")
    };
    tracing::error!(turn_kind, error = %message, "rebon: prompt executor task failed");
    PromptExecutorError::Execution(message)
}

fn panic_payload_message(payload: Box<dyn Any + Send + 'static>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn job(log: &Arc<Mutex<Vec<String>>>, name: &'static str) -> TurnJob {
        let log = log.clone();
        TurnJob {
            run: Box::pin(async move {
                log.lock().unwrap().push(format!("turn {name}"));
                if name == "boom" {
                    panic!("synthetic panic");
                }
                Ok(PromptOutcome::end_turn())
            }),
            kind: "main",
            settle: None,
            stop_when_abandoned: false,
        }
    }

    #[tokio::test]
    async fn turns_run_in_order_and_survive_a_panic() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let turns = TurnLoop::new();
        let handle = Handle::current();
        let boom = turns.submit_turn(&handle, job(&log, "boom"));
        let after = turns.submit_turn(&handle, job(&log, "after"));

        let err = boom.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("main prompt executor task panicked"), "{err}");
        assert!(after.await.unwrap().is_ok());
        assert_eq!(*log.lock().unwrap(), ["turn boom", "turn after"]);
    }

    /// Dropped from under a turn, its dropped future is what stops it.
    struct DropFlag(Arc<Mutex<bool>>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = true;
        }
    }

    fn endless(stopped: &Arc<Mutex<bool>>, stop_when_abandoned: bool) -> TurnJob {
        let flag = DropFlag(stopped.clone());
        TurnJob {
            run: Box::pin(async move {
                let _flag = flag;
                std::future::pending::<TurnResult>().await
            }),
            kind: "main",
            settle: None,
            stop_when_abandoned,
        }
    }

    #[tokio::test]
    async fn an_abandoned_turn_stops_only_when_asked_to() {
        let turns = TurnLoop::new();
        let handle = Handle::current();
        let stopped = Arc::new(Mutex::new(false));
        drop(turns.submit_turn(&handle, endless(&stopped, true)));
        let after = turns.submit_turn(&handle, job(&Arc::new(Mutex::new(Vec::new())), "after"));
        assert!(after.await.unwrap().is_ok(), "the loop moved on");
        assert!(*stopped.lock().unwrap());

        let kept = Arc::new(Mutex::new(false));
        let turns = TurnLoop::new();
        drop(turns.submit_turn(&handle, endless(&kept, false)));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !*kept.lock().unwrap(),
            "the turn keeps running for its cancel token"
        );
    }

    #[tokio::test]
    async fn a_turn_is_settled_before_its_result_is_sent() {
        let settled = Arc::new(Mutex::new(false));
        let seen = settled.clone();
        let turns = TurnLoop::new();
        let result = turns.submit_turn(
            &Handle::current(),
            TurnJob {
                run: Box::pin(async { Ok(PromptOutcome::end_turn()) }),
                kind: "main",
                settle: Some(Box::new(move |result| {
                    assert!(result.is_ok());
                    *seen.lock().unwrap() = true;
                })),
                stop_when_abandoned: false,
            },
        );
        assert!(result.await.unwrap().is_ok());
        assert!(*settled.lock().unwrap());
    }
}
