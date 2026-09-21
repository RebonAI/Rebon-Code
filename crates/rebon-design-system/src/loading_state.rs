/// Projected display for a loading indicator: the spinner line, its text
/// emphasis flags, and an optional second line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadingDisplay {
    /// The first line, already assembled as `<spinner glyph> <message>`.
    /// No frame is animated here — the consumer's render loop owns the
    /// frame counter.
    pub primary_line: String,
    /// True when the consumer should render the message bold.
    pub bold: bool,
    /// True when the consumer should render the message dim.
    pub dim: bool,
    /// Optional second line, always rendered dim.
    pub subtitle: Option<String>,
}

/// Assemble the display for one loading frame.
///
/// `spinner_glyph` is the frame the caller wants drawn; picking frames is
/// the caller's job, not this crate's. Glyph and message are joined by a
/// single space.
pub fn loading_display(
    spinner_glyph: &str,
    message: &str,
    bold: bool,
    dim_color: bool,
    subtitle: Option<&str>,
) -> LoadingDisplay {
    LoadingDisplay {
        primary_line: format!("{spinner_glyph} {message}"),
        bold,
        dim: dim_color,
        subtitle: subtitle.map(str::to_string),
    }
}

/// Lifecycle of a load: idle, in flight, finished, or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadingState {
    /// Nothing is happening.
    Idle,
    /// A load is in flight; `message` is what the spinner shows.
    Loading {
        /// Text shown beside the spinner.
        message: String,
    },
    /// The load finished; `subtitle` is optional follow-up text.
    Loaded {
        /// Optional follow-up text.
        subtitle: Option<String>,
    },
    /// The load failed; `error` is shown to the user.
    Error {
        /// Text shown to the user.
        error: String,
    },
}

/// Events the loading reducer accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadingEvent {
    /// Begin a load, superseding whatever state came before.
    Start {
        /// Text shown beside the spinner.
        message: String,
    },
    /// Complete the current load.
    Succeed {
        /// Optional follow-up text.
        subtitle: Option<String>,
    },
    /// Fail the current load.
    Fail {
        /// Text shown to the user.
        error: String,
    },
    /// Return to [`LoadingState::Idle`].
    Reset,
}

impl LoadingState {
    /// Fold an event into the state and return the successor.
    ///
    /// `Start` and `Reset` apply from any state. `Succeed` and `Fail` are
    /// accepted outside `Loading` too, so a completion that arrives late is
    /// not swallowed and callers never have to gate the call on
    /// [`LoadingState::is_loading`] first.
    pub fn step(self, event: LoadingEvent) -> LoadingState {
        match (self, event) {
            (_, LoadingEvent::Start { message }) => LoadingState::Loading { message },
            (LoadingState::Loading { .. }, LoadingEvent::Succeed { subtitle }) => {
                LoadingState::Loaded { subtitle }
            }
            (LoadingState::Loading { .. }, LoadingEvent::Fail { error }) => {
                LoadingState::Error { error }
            }
            (_, LoadingEvent::Succeed { subtitle }) => {
                // Idempotent succeed when not loading still produces
                // Loaded — callers do not need to gate on the current
                // state.
                LoadingState::Loaded { subtitle }
            }
            (_, LoadingEvent::Fail { error }) => LoadingState::Error { error },
            (_, LoadingEvent::Reset) => LoadingState::Idle,
        }
    }

    /// True while a load is in flight.
    pub fn is_loading(&self) -> bool {
        matches!(self, LoadingState::Loading { .. })
    }
}

impl Default for LoadingState {
    fn default() -> Self {
        LoadingState::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_line_format_includes_spinner_and_message() {
        let d = loading_display("⠋", "Loading", false, false, None);
        assert_eq!(d.primary_line, "⠋ Loading");
    }

    #[test]
    fn flags_propagate() {
        let d = loading_display("⠋", "X", true, true, None);
        assert!(d.bold);
        assert!(d.dim);
    }

    #[test]
    fn subtitle_present() {
        let d = loading_display("⠋", "Loading", false, false, Some("almost there"));
        assert_eq!(d.subtitle.as_deref(), Some("almost there"));
    }

    #[test]
    fn subtitle_absent() {
        let d = loading_display("⠋", "Loading", false, false, None);
        assert_eq!(d.subtitle, None);
    }

    // ────────────────────────────────────────────────────────────────
    // Lifecycle reducer
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn default_is_idle() {
        assert_eq!(LoadingState::default(), LoadingState::Idle);
    }

    #[test]
    fn start_transitions_idle_to_loading() {
        let s = LoadingState::Idle.step(LoadingEvent::Start {
            message: "go".into(),
        });
        assert_eq!(
            s,
            LoadingState::Loading {
                message: "go".into()
            }
        );
        assert!(s.is_loading());
    }

    #[test]
    fn loading_then_succeed() {
        let s = LoadingState::Idle
            .step(LoadingEvent::Start {
                message: "x".into(),
            })
            .step(LoadingEvent::Succeed { subtitle: None });
        assert_eq!(s, LoadingState::Loaded { subtitle: None });
    }

    #[test]
    fn loading_then_succeed_with_subtitle() {
        let s = LoadingState::Loading {
            message: "x".into(),
        }
        .step(LoadingEvent::Succeed {
            subtitle: Some("done".into()),
        });
        assert_eq!(
            s,
            LoadingState::Loaded {
                subtitle: Some("done".into())
            }
        );
    }

    #[test]
    fn loading_then_fail() {
        let s = LoadingState::Loading {
            message: "x".into(),
        }
        .step(LoadingEvent::Fail {
            error: "boom".into(),
        });
        assert_eq!(
            s,
            LoadingState::Error {
                error: "boom".into()
            }
        );
    }

    #[test]
    fn start_overrides_any_state() {
        let s = LoadingState::Error {
            error: "old".into(),
        }
        .step(LoadingEvent::Start {
            message: "new".into(),
        });
        assert_eq!(
            s,
            LoadingState::Loading {
                message: "new".into()
            }
        );
    }

    #[test]
    fn reset_returns_to_idle_from_any_state() {
        let cases = [
            LoadingState::Idle,
            LoadingState::Loading {
                message: "a".into(),
            },
            LoadingState::Loaded { subtitle: None },
            LoadingState::Error { error: "e".into() },
        ];
        for c in cases {
            assert_eq!(c.step(LoadingEvent::Reset), LoadingState::Idle);
        }
    }

    #[test]
    fn is_loading_only_for_loading_variant() {
        assert!(!LoadingState::Idle.is_loading());
        assert!(LoadingState::Loading {
            message: "x".into()
        }
        .is_loading());
        assert!(!LoadingState::Loaded { subtitle: None }.is_loading());
        assert!(!LoadingState::Error { error: "x".into() }.is_loading());
    }

    #[test]
    fn succeed_from_idle_still_lands_in_loaded() {
        let s = LoadingState::Idle.step(LoadingEvent::Succeed { subtitle: None });
        assert_eq!(s, LoadingState::Loaded { subtitle: None });
    }
}
