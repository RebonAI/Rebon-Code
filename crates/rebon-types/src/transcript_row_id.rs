//! Transcript row ids that say where a user-role row came from.
//!
//! A task notification the runtime delivers and the same text a person
//! pastes or quotes are byte-for-byte alike, so the row's id is what tells
//! them apart. The runtime writes the id when it delivers the notification,
//! at the start of a turn or in the middle of one; storage, replay and every
//! renderer read it back through [`is_task_notification_row_uuid`].

/// What the id of a task-notification row the runtime wrote starts with,
/// before the `-` that separates it from the rest of the id.
pub const TASK_NOTIFICATION_ROW_UUID_PREFIX: &str = "u-internal-task-notification";

/// Whether `uuid` names a task-notification row the runtime wrote itself,
/// rather than a person's words that happen to contain one.
pub fn is_task_notification_row_uuid(uuid: &str) -> bool {
    uuid.strip_prefix(TASK_NOTIFICATION_ROW_UUID_PREFIX)
        .is_some_and(|rest| rest.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_runtime_task_notification_ids_match() {
        assert!(is_task_notification_row_uuid(
            "u-internal-task-notification-sess-1-2"
        ));
        assert!(is_task_notification_row_uuid(&format!(
            "{TASK_NOTIFICATION_ROW_UUID_PREFIX}-x"
        )));
        for other in [
            "u-internal-task-notification",
            "u-internal-task-notifications-1",
            "u-internal-group-1",
            "u-internal-session-1",
            "u-attach-sess-1-2",
            "u-user-quote",
            "",
        ] {
            assert!(!is_task_notification_row_uuid(other), "{other}");
        }
    }
}
