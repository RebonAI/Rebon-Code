//! What the router's tools ask of the host that runs its workers.
//!
//! The router (see `rebon_core::router_mode`) dispatches background worker
//! jobs, continues them, stops them, hands threads to them and looks them up.
//! All of that is the session host's business. The tools see only this trait,
//! which the surface owning the host attaches to the router's tool context,
//! so the task tools stay out of the host's dependency graph.

use std::sync::Arc;

use async_trait::async_trait;

/// A task for a new worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerDispatch {
    /// The project directory the worker works in.
    pub project: String,
    /// A short name for the task, shown wherever the worker is listed.
    pub title: String,
    /// Everything the worker needs to know. It sees nothing else.
    pub brief: String,
    /// Give the worker its own git worktree.
    pub isolate: bool,
    /// A model profile (`fast`, `reasoning`, …) for a task that needs a
    /// lighter or a stronger model than the default.
    pub model_profile: Option<String>,
}

/// A started worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchedWorker {
    pub worker_id: String,
    /// Set when the requested isolation could not be given, saying why.
    pub note: Option<String>,
}

#[async_trait]
pub trait WorkerControl: Send + Sync {
    /// Start a worker under the router whose session is `router_session_id`.
    /// Returns as soon as the job is queued.
    async fn dispatch(
        &self,
        router_session_id: &str,
        request: WorkerDispatch,
    ) -> Result<DispatchedWorker, String>;

    /// Give one of the router's workers its next turn, in its own session.
    async fn continue_worker(
        &self,
        router_session_id: &str,
        worker_id: &str,
        message: &str,
    ) -> Result<(), String>;

    /// Stop one of the router's workers, with anything it started.
    async fn stop_worker(&self, router_session_id: &str, worker_id: &str) -> Result<(), String>;

    /// Hand `thread` (`pr:owner/repo#12`, or a pull request's URL) to a
    /// worker, so its events go straight to that worker. Returns the
    /// thread's id.
    async fn subscribe_thread(
        &self,
        router_session_id: &str,
        thread: &str,
        worker_id: &str,
    ) -> Result<String, String>;

    /// Render what `id` points to (a worker, a thread, the router itself)
    /// as a short digest that lists the ids next to it. With no id, list the
    /// router's workers.
    async fn recall(&self, router_session_id: &str, id: Option<&str>) -> Result<String, String>;
}

/// Storage for the attached [`WorkerControl`], read and written through
/// `ToolContext::worker_control`.
#[derive(Clone, Default)]
pub struct WorkerControlContext {
    pub control: Option<Arc<dyn WorkerControl>>,
}
