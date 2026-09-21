//! Next-pending-task planner for spinner state.
//!
//! The planner works in four steps:
//!
//! 1. If there are no tasks, return none.
//! 2. Collect the pending tasks; if there are none, return none.
//! 3. Collect the ids of every task that is not completed.
//! 4. Pick the first pending task none of whose `blocked_by` ids are in
//!    that set, falling back to the first pending task.
//!
//! If every pending task is blocked, the first pending task is returned as a
//! fallback so the spinner still has something to show.

use std::collections::HashSet;

/// The task-list row and its status vocabulary, shared with every other
/// surface that reads the list.
pub use rebon_types::{ListTask, TaskListStatus};

/// Returns a clone of the chosen task, or `None` if `tasks` is empty
/// / has no pending entries.
pub fn find_next_pending_task(tasks: &[ListTask]) -> Option<ListTask> {
    let pending: Vec<&ListTask> = tasks
        .iter()
        .filter(|t| t.status == TaskListStatus::Pending)
        .collect();
    if pending.is_empty() {
        return None;
    }
    let unresolved: HashSet<&str> = tasks
        .iter()
        .filter(|t| t.status != TaskListStatus::Completed)
        .map(|t| t.id.as_str())
        .collect();
    let unblocked = pending.iter().find(|t| {
        !t.blocked_by
            .iter()
            .any(|id| unresolved.contains(id.as_str()))
    });
    Some((*unblocked.unwrap_or(&pending[0])).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: TaskListStatus, blocked_by: &[&str]) -> ListTask {
        ListTask {
            id: id.to_string(),
            subject: String::new(),
            status,
            owner: None,
            blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(find_next_pending_task(&[]).is_none());
    }

    #[test]
    fn no_pending_returns_none() {
        let tasks = vec![
            task("a", TaskListStatus::Completed, &[]),
            task("b", TaskListStatus::InProgress, &[]),
        ];
        assert!(find_next_pending_task(&tasks).is_none());
    }

    #[test]
    fn unblocked_pending_returned() {
        let tasks = vec![task("a", TaskListStatus::Pending, &[])];
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "a");
    }

    #[test]
    fn first_unblocked_pending_returned() {
        let tasks = vec![
            task("a", TaskListStatus::Pending, &["c"]), // blocked by in-progress c
            task("b", TaskListStatus::Pending, &[]),    // unblocked
            task("c", TaskListStatus::InProgress, &[]),
        ];
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "b");
    }

    #[test]
    fn pending_blocked_by_completed_is_unblocked() {
        let tasks = vec![
            task("a", TaskListStatus::Pending, &["c"]),
            task("c", TaskListStatus::Completed, &[]),
        ];
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "a");
    }

    #[test]
    fn pending_blocked_by_pending_is_blocked() {
        // 'a' blocked by 'b', 'b' is itself pending → blocked.
        let tasks = vec![
            task("a", TaskListStatus::Pending, &["b"]),
            task("b", TaskListStatus::Pending, &[]),
        ];
        // 'a' is blocked, but 'b' is unblocked → returns 'b'.
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "b");
    }

    #[test]
    fn all_blocked_returns_first_pending_fallback() {
        // Both 'a' and 'b' blocked by 'c' (in-progress).
        let tasks = vec![
            task("a", TaskListStatus::Pending, &["c"]),
            task("b", TaskListStatus::Pending, &["c"]),
            task("c", TaskListStatus::InProgress, &[]),
        ];
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "a"); // fallback to first pending
    }

    #[test]
    fn missing_blocker_id_treated_as_unblocked() {
        // 'a' blocked by 'z' which doesn't exist. The check is against
        // the unresolved-id set — missing IDs aren't in it, so 'a' is
        // considered unblocked.
        let tasks = vec![task("a", TaskListStatus::Pending, &["z"])];
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "a");
    }

    #[test]
    fn task_status_completed_excluded_from_unresolved() {
        let tasks = vec![
            task("a", TaskListStatus::Pending, &["b", "c"]),
            task("b", TaskListStatus::Completed, &[]),
            task("c", TaskListStatus::Completed, &[]),
        ];
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "a");
    }

    #[test]
    fn order_preserved_for_pending_filter() {
        let tasks = vec![
            task("z", TaskListStatus::Pending, &[]),
            task("a", TaskListStatus::Pending, &[]),
        ];
        let r = find_next_pending_task(&tasks).unwrap();
        assert_eq!(r.id, "z");
    }

    #[test]
    fn pending_task_blocked_by_self_is_blocked() {
        // Self-blocked: 'a' blocked by itself. Counts as blocked
        // because the unresolved set includes 'a' (it's pending, not
        // completed).
        let tasks = vec![task("a", TaskListStatus::Pending, &["a"])];
        let r = find_next_pending_task(&tasks).unwrap();
        // Falls back to first pending (which is 'a' itself).
        assert_eq!(r.id, "a");
    }
}
