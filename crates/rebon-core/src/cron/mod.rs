//! Cron scheduler integration.
//!
//! The data model, JSON IO, and jitter math live in
//! [`rebon_tool::cron`] — that's what the three model-facing tools import.
//! This module holds everything tied to `AttachmentPoller`: the per-iteration
//! poller, the owner-elected scheduler task, and the cross-process lock.

pub mod lock;
pub mod poller;
pub mod scheduler;

pub use lock::{try_acquire_scheduler_lock, SchedulerLock};
pub use poller::{CompositePoller, CronPoller};
pub use scheduler::{
    start_scheduler, start_scheduler_with_session_store, SchedulerConfig, SchedulerHandle,
};
