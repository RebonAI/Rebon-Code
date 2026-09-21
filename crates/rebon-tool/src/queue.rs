//! The Agent Queue slice of [`ToolContext`].
//!
//! The four queue tools live in the tasks plugin; this half exists because
//! `ToolContext` carries the controller and hands it out through
//! `queue_controller()`, and the coordinator sets it. Storage only.

use std::sync::Arc;

/// Queue-coordinator state that [`crate::ToolContext`] carries in its
/// extension bag.
///
/// Storage only — read and written through the unchanged `queue_controller()`
/// accessor.
#[derive(Clone, Default)]
pub struct QueueContext {
    pub controller: Option<Arc<dyn crate::QueueController>>,
}
