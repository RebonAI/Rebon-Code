//! The `Memory updated in … · /memory to edit` line, as a turn hook.
//!
//! One subscriber on the kernel's `turn-hooks` seat, annotating any
//! successful file-edit call whose target lands in an auto-memory directory.
//! Which tools those are, and which input field names their path, comes from
//! `rebon-tools-core`'s builtin tool table rather than a list of names written
//! here — so `NotebookEdit`, which is in the same permission class and has
//! always had the same auto-memory carve-out, gets the notification too.
//!
//! Switching the plugin off takes the line away with it, which is the point:
//! there is no `/memory` to edit when there is no memory feature.

use rebon_core::query::QueryEvent;
use rebon_core::turn_hook::{
    ToolAnnotationHookContext, ToolAnnotationHookEvent, TurnHook, TurnHookContext,
};
use rebon_tools_core::ToolKind;
use serde_json::Value;

/// Result field the transcript and the TUI read the line out of.
pub const MEMORY_NOTIFICATION_FIELD: &str = "memoryNotification";

/// Subscriber id on the `turn-hooks` seat.
pub const HOOK_ID: &str = "memory/update-notification";

/// The notification annotator.
pub struct MemoryUpdateNotification;

impl MemoryUpdateNotification {
    /// The auto-memory file this call wrote, if it wrote one.
    ///
    /// Two conditions, both read off shared tables rather than a name list:
    /// the tool is in the file-edit class and declares which input field
    /// holds its path, and that path resolves inside one of this project's
    /// memory directories.
    fn written_memory_path<'a>(
        event: &'a ToolAnnotationHookEvent<'a>,
    ) -> Option<(&'a str, &'a str)> {
        let cwd = event.cwd?;
        let facts = rebon_tools_core::builtin_tool_facts_for_name(event.tool_name)?;
        if facts.kind != ToolKind::FileEdit {
            return None;
        }
        let path = event
            .input
            .get(facts.file_target_field?)
            .and_then(Value::as_str)?;
        rebon_session::memory_paths::is_memory_path_for_any_scope(path, cwd).then_some((path, cwd))
    }
}

impl TurnHook for MemoryUpdateNotification {
    fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

    fn annotate_tool_result(
        &self,
        event: &ToolAnnotationHookEvent<'_>,
        context: &mut ToolAnnotationHookContext,
    ) {
        let Some((path, cwd)) = Self::written_memory_path(event) else {
            return;
        };
        context.annotate(
            MEMORY_NOTIFICATION_FIELD,
            Value::String(
                crate::memory::update_notification::format_update_notification_from_env(path, cwd),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The notification this hook would add, if any.
    fn annotate(tool: &str, input: Value, cwd: Option<&str>) -> Option<String> {
        let mut context = ToolAnnotationHookContext::default();
        MemoryUpdateNotification.annotate_tool_result(
            &ToolAnnotationHookEvent {
                tool_name: tool,
                input: &input,
                cwd,
            },
            &mut context,
        );
        let fields = context.fields();
        assert!(fields.len() <= 1, "one field at most: {fields:?}");
        fields.first().map(|(key, value)| {
            assert_eq!(key, MEMORY_NOTIFICATION_FIELD);
            value.as_str().expect("a string").to_string()
        })
    }

    /// A tool that is not in the file-edit class never gets the line, even
    /// when its input happens to carry a memory path.
    #[test]
    fn a_read_of_a_memory_file_is_not_a_memory_update() {
        assert_eq!(
            annotate("Read", json!({"file_path": "/repo/x"}), Some("/repo")),
            None
        );
    }

    /// No cwd means no project, so no memory directories to compare against.
    #[test]
    fn a_call_without_a_cwd_is_left_alone() {
        assert_eq!(
            annotate("Write", json!({"file_path": "/repo/x"}), None),
            None
        );
    }

    /// A file-edit call outside every memory directory is left alone.
    #[test]
    fn an_ordinary_write_is_left_alone() {
        assert_eq!(
            annotate(
                "Write",
                json!({"file_path": "/repo/src/lib.rs"}),
                Some("/repo")
            ),
            None
        );
    }

    /// A file-edit call with no path in its input is left alone rather than
    /// guessed at.
    #[test]
    fn a_call_without_its_path_field_is_left_alone() {
        assert_eq!(annotate("Write", json!({}), Some("/repo")), None);
    }
}
