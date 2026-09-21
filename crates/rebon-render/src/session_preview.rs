//! Session preview projection.
//!
//! Full-log loading, relative-time formatting, and session-id extraction stay
//! injected as plain input values.

/// Minimal log shape this projection consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreviewLog {
    /// Messages shown in the transcript preview.
    pub messages: Vec<String>,
    /// Injected conversation/session ID.
    pub conversation_id: Option<String>,
    /// Already formatted relative time text.
    pub relative_time: String,
    /// Message count shown in the footer.
    pub message_count: usize,
    /// Optional git branch suffix.
    pub git_branch: Option<String>,
}

/// Everything the projection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreviewInput {
    /// The log being previewed.
    pub log: SessionPreviewLog,
    /// Whether `log` is a lite log that still needs hydration.
    pub log_is_lite: bool,
    /// Loaded full log, if hydration has completed.
    pub full_log: Option<SessionPreviewLog>,
}

/// The projection's two states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPreviewProjection {
    /// Loading, while the full log is still being read.
    Loading(SessionPreviewLoadingDisplay),
    /// Fully resolved preview state.
    Ready(Box<SessionPreviewDisplay>),
}

/// Loading-state display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreviewLoadingDisplay {
    /// Fixed loading copy.
    pub message: &'static str,
    /// Cancel shortcut fallback text.
    pub cancel_fallback: &'static str,
}

/// Ready-state display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreviewDisplay {
    /// Whether the caller should read the full log.
    pub should_load_full_log: bool,
    /// Full log when present, otherwise the lite log.
    pub display_log: SessionPreviewLog,
    /// Log object that Enter/confirm should select.
    pub selected_log: SessionPreviewLog,
    /// Inputs for the transcript preview.
    pub messages: SessionPreviewMessagesDisplay,
    /// Footer summary line.
    pub footer_summary: String,
    /// Resume shortcut label.
    pub resume_shortcut: &'static str,
    /// Cancel shortcut fallback text.
    pub cancel_fallback: &'static str,
}

/// Fixed transcript display settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreviewMessagesDisplay {
    /// Transcript messages.
    pub messages: Vec<String>,
    /// Conversation id shown in the transcript.
    pub conversation_id: String,
    /// Screen name for the transcript view.
    pub screen: &'static str,
    /// Always verbose.
    pub verbose: bool,
    /// Always show every message in the transcript.
    pub show_all_in_transcript: bool,
    /// Never loading once ready.
    pub is_loading: bool,
    /// Never showing the message selector.
    pub is_message_selector_visible: bool,
}

/// Decide the loading or ready state.
pub fn project_session_preview(input: &SessionPreviewInput) -> SessionPreviewProjection {
    let should_load_full_log = input.log_is_lite;
    let is_loading = input.log_is_lite && input.full_log.is_none();

    if is_loading {
        return SessionPreviewProjection::Loading(SessionPreviewLoadingDisplay {
            message: "Loading session\u{2026}",
            cancel_fallback: "Esc",
        });
    }

    let display_log = input.full_log.clone().unwrap_or_else(|| input.log.clone());
    let selected_log = display_log.clone();
    let conversation_id = display_log.conversation_id.clone().unwrap_or_default();
    let git_branch_suffix = display_log
        .git_branch
        .as_ref()
        .map(|branch| format!(" · {branch}"))
        .unwrap_or_default();

    SessionPreviewProjection::Ready(Box::new(SessionPreviewDisplay {
        should_load_full_log,
        display_log: display_log.clone(),
        selected_log,
        messages: SessionPreviewMessagesDisplay {
            messages: display_log.messages.clone(),
            conversation_id,
            screen: "transcript",
            verbose: true,
            show_all_in_transcript: true,
            is_loading: false,
            is_message_selector_visible: false,
        },
        footer_summary: format!(
            "{} · {} messages{}",
            display_log.relative_time, display_log.message_count, git_branch_suffix
        ),
        resume_shortcut: "Enter",
        cancel_fallback: "Esc",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> SessionPreviewLog {
        SessionPreviewLog {
            messages: vec!["a".into(), "b".into()],
            conversation_id: Some("conv-123".into()),
            relative_time: "2 hours ago".into(),
            message_count: 2,
            git_branch: Some("main".into()),
        }
    }

    #[test]
    fn loading_branch_matches_lite_log_without_hydration() {
        let projection = project_session_preview(&SessionPreviewInput {
            log: log(),
            log_is_lite: true,
            full_log: None,
        });

        assert_eq!(
            projection,
            SessionPreviewProjection::Loading(SessionPreviewLoadingDisplay {
                message: "Loading session\u{2026}",
                cancel_fallback: "Esc",
            })
        );
    }

    #[test]
    fn ready_branch_prefers_full_log_and_threads_fixed_message_props() {
        let mut full = log();
        full.messages = vec!["full".into()];
        full.relative_time = "1 minute ago".into();
        full.message_count = 1;
        full.git_branch = Some("feature/x".into());

        let projection = project_session_preview(&SessionPreviewInput {
            log: log(),
            log_is_lite: true,
            full_log: Some(full.clone()),
        });

        let SessionPreviewProjection::Ready(display) = projection else {
            panic!("expected ready projection");
        };

        assert!(display.should_load_full_log);
        assert_eq!(display.display_log, full);
        assert_eq!(display.selected_log.messages, vec!["full"]);
        assert_eq!(display.messages.conversation_id, "conv-123");
        assert_eq!(display.messages.screen, "transcript");
        assert!(display.messages.verbose);
        assert!(display.messages.show_all_in_transcript);
        assert!(!display.messages.is_loading);
        assert!(!display.messages.is_message_selector_visible);
        assert_eq!(
            display.footer_summary,
            "1 minute ago · 1 messages · feature/x"
        );
        assert_eq!(display.resume_shortcut, "Enter");
        assert_eq!(display.cancel_fallback, "Esc");
    }

    #[test]
    fn ready_branch_uses_original_log_when_not_lite() {
        let projection = project_session_preview(&SessionPreviewInput {
            log: log(),
            log_is_lite: false,
            full_log: None,
        });

        let SessionPreviewProjection::Ready(display) = projection else {
            panic!("expected ready projection");
        };

        assert!(!display.should_load_full_log);
        assert_eq!(display.display_log.message_count, 2);
        assert_eq!(display.footer_summary, "2 hours ago · 2 messages · main");
    }

    #[test]
    fn missing_conversation_id_and_git_branch_fall_back_cleanly() {
        let mut value = log();
        value.conversation_id = None;
        value.git_branch = None;
        let projection = project_session_preview(&SessionPreviewInput {
            log: value,
            log_is_lite: false,
            full_log: None,
        });

        let SessionPreviewProjection::Ready(display) = projection else {
            panic!("expected ready projection");
        };

        assert_eq!(display.messages.conversation_id, "");
        assert_eq!(display.footer_summary, "2 hours ago · 2 messages");
    }
}
