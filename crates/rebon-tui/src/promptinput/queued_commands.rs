//! Queue preprocessing for pending commands.
//!
//! Drawing the queue is out of scope. This module covers only the
//! preprocessing that happens before anything is drawn:
//!
//! * detect and drop idle notifications;
//! * cap task-notification rows to 3 visible items;
//! * synthesize the overflow summary XML payload.

use serde_json::Value;

const TASK_NOTIFICATION_TAG: &str = "task-notification";
const SUMMARY_TAG: &str = "summary";
const STATUS_TAG: &str = "status";
const IDLE_NOTIFICATION_TYPE: &str = "idle_notification";

/// Cap on the number of visible task-notification rows.
pub const MAX_VISIBLE_NOTIFICATIONS: usize = 3;

/// Minimal queue value shape needed by the pure helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueuedCommandValue {
    /// String payloads may contain JSON task/idle notifications or shell input.
    Text(String),
    /// Non-string payloads bypass idle-notification parsing.
    NonText,
}

/// Minimal queued-command shape consumed by the preprocessing step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCommand {
    /// The command's mode tag, e.g. `task-notification`.
    pub mode: String,
    /// The command's payload.
    pub value: QueuedCommandValue,
}

/// Returns true when the queued string is a hidden idle notification.
///
/// A JSON object whose `type` field is `idle_notification`.
pub fn is_idle_notification(value: &str) -> bool {
    serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|parsed| {
            parsed
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some(IDLE_NOTIFICATION_TYPE)
}

/// Builds the synthetic overflow XML payload shown after task notifications
/// are capped.
///
/// A `task-notification` XML payload whose `summary` reads
/// `+{count} more tasks completed` and whose `status` is `completed`.
pub fn create_overflow_notification_message(count: usize) -> String {
    format!(
        "<{TASK_NOTIFICATION_TAG}>\n<{SUMMARY_TAG}>+{count} more tasks completed</{SUMMARY_TAG}>\n<{STATUS_TAG}>completed</{STATUS_TAG}>\n</{TASK_NOTIFICATION_TAG}>"
    )
}

/// Filter, group, and cap the queued commands.
///
/// Rules:
///
/// * idle notifications are filtered out entirely;
/// * task notifications are grouped after all other commands;
/// * when task notifications exceed the cap, only the first two are
///   retained and the last row becomes the synthetic overflow summary.
pub fn process_queued_commands(queued_commands: &[QueuedCommand]) -> Vec<QueuedCommand> {
    let filtered_commands: Vec<_> = queued_commands
        .iter()
        .filter(|cmd| match &cmd.value {
            QueuedCommandValue::Text(value) => !is_idle_notification(value),
            QueuedCommandValue::NonText => true,
        })
        .cloned()
        .collect();

    let task_notifications: Vec<_> = filtered_commands
        .iter()
        .filter(|cmd| cmd.mode == TASK_NOTIFICATION_TAG)
        .cloned()
        .collect();
    let other_commands: Vec<_> = filtered_commands
        .iter()
        .filter(|cmd| cmd.mode != TASK_NOTIFICATION_TAG)
        .cloned()
        .collect();

    if task_notifications.len() <= MAX_VISIBLE_NOTIFICATIONS {
        return other_commands
            .into_iter()
            .chain(task_notifications)
            .collect();
    }

    let visible_notifications = task_notifications
        .iter()
        .take(MAX_VISIBLE_NOTIFICATIONS - 1)
        .cloned();
    let overflow_count = task_notifications.len() - (MAX_VISIBLE_NOTIFICATIONS - 1);
    let overflow_command = QueuedCommand {
        mode: TASK_NOTIFICATION_TAG.to_string(),
        value: QueuedCommandValue::Text(create_overflow_notification_message(overflow_count)),
    };

    other_commands
        .into_iter()
        .chain(visible_notifications)
        .chain(std::iter::once(overflow_command))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(mode: &str, value: &str) -> QueuedCommand {
        QueuedCommand {
            mode: mode.to_string(),
            value: QueuedCommandValue::Text(value.to_string()),
        }
    }

    #[test]
    fn idle_notification_requires_valid_json_type_field() {
        assert!(is_idle_notification(r#"{"type":"idle_notification"}"#));
        assert!(!is_idle_notification(r#"{"type":"other"}"#));
        assert!(!is_idle_notification("not json"));
    }

    #[test]
    fn overflow_notification_message_has_expected_shape() {
        assert_eq!(
            create_overflow_notification_message(4),
            "<task-notification>\n<summary>+4 more tasks completed</summary>\n<status>completed</status>\n</task-notification>"
        );
    }

    #[test]
    fn process_filters_idle_notifications_and_keeps_non_text_values() {
        let commands = vec![
            text("task-notification", r#"{"type":"idle_notification"}"#),
            QueuedCommand {
                mode: "task-notification".to_string(),
                value: QueuedCommandValue::NonText,
            },
            text("prompt", "hello"),
        ];

        assert_eq!(
            process_queued_commands(&commands),
            vec![
                text("prompt", "hello"),
                QueuedCommand {
                    mode: "task-notification".to_string(),
                    value: QueuedCommandValue::NonText,
                },
            ]
        );
    }

    #[test]
    fn process_groups_other_commands_before_notifications() {
        let commands = vec![
            text("task-notification", "one"),
            text("prompt", "visible"),
            text("task-notification", "two"),
        ];

        assert_eq!(
            process_queued_commands(&commands),
            vec![
                text("prompt", "visible"),
                text("task-notification", "one"),
                text("task-notification", "two"),
            ]
        );
    }

    #[test]
    fn process_caps_notifications_with_overflow_summary() {
        let commands = vec![
            text("task-notification", "one"),
            text("task-notification", "two"),
            text("task-notification", "three"),
            text("task-notification", "four"),
        ];

        let processed = process_queued_commands(&commands);
        assert_eq!(processed.len(), 3);
        assert_eq!(processed[0], text("task-notification", "one"));
        assert_eq!(processed[1], text("task-notification", "two"));
        assert_eq!(
            processed[2],
            text(
                "task-notification",
                "<task-notification>\n<summary>+2 more tasks completed</summary>\n<status>completed</status>\n</task-notification>"
            )
        );
    }
}
