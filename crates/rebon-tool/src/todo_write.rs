//! The todo store `TodoWrite` writes and the TUI reads.
//!
//! The `TodoWrite` tool itself lives in the tasks plugin; this half
//! exists here because the store outlives any one tool call and has a second
//! reader: the TUI's task pane renders `get_todos("session")` whether or not
//! the tasks plugin is loaded. Turning the plugin off takes the tool off the
//! seat and leaves whatever was already written here readable.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// Global in-memory todo store.
///
/// A map from agent/session key to the current todo list. TodoWrite is a
/// simple state-replacement tool: the model sends the full list and we overwrite.
static TODOS: RwLock<Option<HashMap<String, Vec<TodoItem>>>> = RwLock::new(None);

/// One todo item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Imperative description of what needs to be done.
    pub content: String,
    /// `pending`, `in_progress`, or `completed`.
    pub status: String,
    /// Present-continuous form shown during execution (e.g. "Running tests").
    #[serde(rename = "activeForm")]
    pub active_form: String,
}

/// Default todo key when no agent id is available.
pub const DEFAULT_TODO_KEY: &str = "session";

/// Read the current todo list for a given key.
pub fn get_todos(key: &str) -> Vec<TodoItem> {
    let guard = TODOS.read().expect("todos lock poisoned");
    guard
        .as_ref()
        .and_then(|map| map.get(key))
        .cloned()
        .unwrap_or_default()
}

/// Replace the todo list for a given key.
pub fn set_todos(key: &str, todos: Vec<TodoItem>) {
    let mut guard = TODOS.write().expect("todos lock poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    if todos.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.to_string(), todos);
    }
}

/// Clear all todos (useful for test isolation).
///
/// Reachable from the `test-support` feature as well as this crate's own
/// tests, because the tool that exercises the store lives in the tasks
/// plugin.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_all_todos() {
    let mut guard = TODOS.write().expect("todos lock poisoned");
    *guard = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_an_empty_list_removes_the_key() {
        let key = "todo-store-empty-clears";
        set_todos(
            key,
            vec![TodoItem {
                content: "Run tests".into(),
                status: "pending".into(),
                active_form: "Running tests".into(),
            }],
        );
        assert_eq!(get_todos(key).len(), 1);

        set_todos(key, Vec::new());
        assert!(get_todos(key).is_empty());
    }

    #[test]
    fn keys_do_not_see_each_others_lists() {
        let one = "todo-store-key-one";
        let two = "todo-store-key-two";
        set_todos(
            one,
            vec![TodoItem {
                content: "Mine".into(),
                status: "pending".into(),
                active_form: "Doing mine".into(),
            }],
        );
        assert!(get_todos(two).is_empty());
        assert_eq!(get_todos(one)[0].content, "Mine");
        set_todos(one, Vec::new());
    }
}
