//! Dream-status string formatter.
//!
//! ## What it produces
//!
//! [`format_dream_status`] picks the suffix shown next to the memory row
//! from a running flag plus a [`LastDreamAt`] cell:
//!
//! ```text
//! running                -> "running"
//! LastDreamAt::Loading   -> ""
//! LastDreamAt::Never     -> "never"
//! LastDreamAt::Ran(ts)   -> "last ran <relative-time>"
//! ```
//!
//! Four load-bearing details:
//!
//! 1. **A running dream takes priority over everything else.**
//! Even when `last_dream_at` is set, the status is `"running"`.
//! 2. **`LastDreamAt::Loading`** is the "we haven't loaded the
//! timestamp yet" case → empty string.
//! 3. **`LastDreamAt::Never`** is the "we've loaded the timestamp
//! and there's never been a dream run" case → `"never"`.
//! 4. **`LastDreamAt::Ran(ts)`** is the "we have a real timestamp"
//! case → `last ran <relative-time>`. The relative-time
//! formatter is supplied by the caller as a closure so this
//! crate doesn't pull in i18n / a relative-time implementation.

/// State of the last-dream timestamp cell: not read yet, never run, or a
/// real timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LastDreamAt {
    /// Not read yet — the timestamp hasn't been loaded. Formats as
    /// the empty string.
    Loading,
    /// The timestamp read returned 0 because no dream has ever run.
    /// Formats as `"never"`.
    Never,
    /// A real Unix-milliseconds timestamp. Renders as
    /// `last ran <relative-time>` where `<relative-time>` is
    /// produced by the caller-supplied formatter.
    Ran(i64),
}

/// Format the dream-status suffix string for the memory row: `"running"`
/// while a dream is in flight, otherwise the state-dependent text
/// described above.
///
/// `format_relative` is the caller-supplied formatter that takes a
/// Unix-milliseconds timestamp and returns a human-readable string. It
/// runs only for [`LastDreamAt::Ran`], so the caller owns the
/// relative-time wording.
pub fn format_dream_status<F>(
    is_dream_running: bool,
    last_dream_at: LastDreamAt,
    format_relative: F,
) -> String
where
    F: FnOnce(i64) -> String,
{
    if is_dream_running {
        return "running".to_string();
    }
    match last_dream_at {
        LastDreamAt::Loading => String::new(),
        LastDreamAt::Never => "never".to_string(),
        LastDreamAt::Ran(ts) => format!("last ran {}", format_relative(ts)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_relative(_ts: i64) -> String {
        "5 minutes ago".to_string()
    }

    #[test]
    fn running_takes_priority_over_loading() {
        let s = format_dream_status(true, LastDreamAt::Loading, fixed_relative);
        assert_eq!(s, "running");
    }

    #[test]
    fn running_takes_priority_over_never() {
        let s = format_dream_status(true, LastDreamAt::Never, fixed_relative);
        assert_eq!(s, "running");
    }

    #[test]
    fn running_takes_priority_over_ran() {
        let s = format_dream_status(true, LastDreamAt::Ran(1_000), fixed_relative);
        assert_eq!(s, "running");
    }

    #[test]
    fn loading_renders_as_empty_string() {
        let s = format_dream_status(false, LastDreamAt::Loading, fixed_relative);
        assert_eq!(s, "");
    }

    #[test]
    fn never_renders_as_never() {
        let s = format_dream_status(false, LastDreamAt::Never, fixed_relative);
        assert_eq!(s, "never");
    }

    #[test]
    fn ran_renders_as_last_ran_with_relative_time() {
        let s = format_dream_status(false, LastDreamAt::Ran(1_700_000_000), fixed_relative);
        assert_eq!(s, "last ran 5 minutes ago");
    }

    #[test]
    fn relative_formatter_receives_actual_timestamp() {
        let received = std::cell::Cell::new(0i64);
        let _ = format_dream_status(false, LastDreamAt::Ran(42), |ts| {
            received.set(ts);
            "x".to_string()
        });
        assert_eq!(received.get(), 42);
    }

    #[test]
    fn relative_formatter_not_called_when_running() {
        let mut called = false;
        let _ = format_dream_status(true, LastDreamAt::Ran(42), |_| {
            called = true;
            "x".to_string()
        });
        assert!(!called);
    }

    #[test]
    fn relative_formatter_not_called_when_loading() {
        let mut called = false;
        let _ = format_dream_status(false, LastDreamAt::Loading, |_| {
            called = true;
            "x".to_string()
        });
        assert!(!called);
    }

    #[test]
    fn relative_formatter_not_called_when_never() {
        let mut called = false;
        let _ = format_dream_status(false, LastDreamAt::Never, |_| {
            called = true;
            "x".to_string()
        });
        assert!(!called);
    }

    #[test]
    fn relative_formatter_called_only_when_ran() {
        let mut called = false;
        let _ = format_dream_status(false, LastDreamAt::Ran(1), |_| {
            called = true;
            "x".to_string()
        });
        assert!(called);
    }

    #[test]
    fn dream_status_text_table() {
        struct Case {
            running: bool,
            last: LastDreamAt,
            expected: &'static str,
        }
        let cases = [
            Case {
                running: true,
                last: LastDreamAt::Loading,
                expected: "running",
            },
            Case {
                running: true,
                last: LastDreamAt::Never,
                expected: "running",
            },
            Case {
                running: true,
                last: LastDreamAt::Ran(123),
                expected: "running",
            },
            Case {
                running: false,
                last: LastDreamAt::Loading,
                expected: "",
            },
            Case {
                running: false,
                last: LastDreamAt::Never,
                expected: "never",
            },
            Case {
                running: false,
                last: LastDreamAt::Ran(123),
                expected: "last ran 5 minutes ago",
            },
        ];

        for c in cases {
            let actual = format_dream_status(c.running, c.last, fixed_relative);
            assert_eq!(actual, c.expected);
        }
    }
}
