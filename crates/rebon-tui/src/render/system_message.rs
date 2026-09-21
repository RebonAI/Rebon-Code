use crate::message::{SystemLevel, SystemMessage};

pub(super) fn system_level_label(msg: &SystemMessage) -> Option<String> {
    msg.level
        .map(|level| match level {
            SystemLevel::Info => "info",
            SystemLevel::Warning => "warning",
            SystemLevel::Error => "error",
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_message_with_level(level: Option<SystemLevel>) -> SystemMessage {
        SystemMessage {
            uuid: "s1".into(),
            timestamp: "t".into(),
            subtype: "api_error".into(),
            content: Some("message".into()),
            level,
            is_meta: None,
        }
    }

    #[test]
    fn system_level_label_maps_all_levels_to_lowercase_strings() {
        assert_eq!(
            system_level_label(&system_message_with_level(Some(SystemLevel::Info))).as_deref(),
            Some("info")
        );
        assert_eq!(
            system_level_label(&system_message_with_level(Some(SystemLevel::Warning))).as_deref(),
            Some("warning")
        );
        assert_eq!(
            system_level_label(&system_message_with_level(Some(SystemLevel::Error))).as_deref(),
            Some("error")
        );
    }

    #[test]
    fn system_level_label_omits_absent_level() {
        assert_eq!(system_level_label(&system_message_with_level(None)), None);
    }
}
