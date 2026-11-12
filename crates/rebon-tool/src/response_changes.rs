//! Which files the calls of one model response have already changed.
//!
//! The calls of a response run in order and each sees what the earlier ones
//! wrote, which is what a content-anchored edit (`Edit`, `MultiEdit`, a cell
//! named by its `id`) wants: it finds its anchor in the current text. A
//! *positional* edit cannot. A `NotebookEdit` that names a cell by its index,
//! or a `str_replace_editor` insert at a line number, was written against the
//! file as the model last saw it — before this response. Once an earlier call
//! of the same response has changed that file, the position may name a
//! different cell or line, and applying the edit there would put it in the
//! wrong place without a word. Those tools ask [`changed_earlier_in_response`]
//! first and refuse instead.
//!
//! The record is per response: the scheduler attaches a fresh one to the
//! context of every tool round ([`crate::ToolContext::with_fresh_response_changes`]),
//! and the file tools note each path they write ([`record_file_change`]). A
//! context without one (a test, a direct call) records nothing and refuses
//! nothing. Changes made through the shell are not seen.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::ToolContext;

/// The paths changed so far by one response's calls, shared by every clone
/// of that response's context.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResponseChanges {
    changed: Arc<Mutex<HashSet<String>>>,
}

/// Note that `path` was just written by a call of the current response.
pub fn record_file_change(context: &ToolContext, path: &Path) {
    let Some(changes) = context.extension::<ResponseChanges>() else {
        return;
    };
    let mut changed = changes
        .changed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    changed.extend(crate::file_access_keys(path));
}

/// Whether an earlier call of the current response already wrote `path`.
pub fn changed_earlier_in_response(context: &ToolContext, path: &Path) -> bool {
    let Some(changes) = context.extension::<ResponseChanges>() else {
        return false;
    };
    let changed = changes
        .changed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::file_access_keys(path)
        .iter()
        .any(|key| changed.contains(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_is_seen_by_later_calls_of_the_same_response_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let other = dir.path().join("b.txt");
        std::fs::write(&other, "y").unwrap();

        let response = ToolContext::new().with_fresh_response_changes();
        let call = response.clone();
        assert!(!changed_earlier_in_response(&call, &file));
        record_file_change(&call, &file);
        assert!(changed_earlier_in_response(&response.clone(), &file));
        assert!(!changed_earlier_in_response(&response, &other));

        let next_response = response.with_fresh_response_changes();
        assert!(!changed_earlier_in_response(&next_response, &file));

        // No record attached: nothing is noted, nothing refused.
        let bare = ToolContext::new();
        record_file_change(&bare, &file);
        assert!(!changed_earlier_in_response(&bare, &file));
    }
}
