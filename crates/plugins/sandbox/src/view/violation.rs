//! `SandboxViolationEvent` shape + the 12-hour clock formatter +
//! the per-violation row text builder.
//!
//! ## Behaviour notes
//!
//! [`format_time`] renders an `(hour, minute, second)` triple as
//! `"h:mm:ssa"`:
//!
//! ```text
//! h  = hour % 12, with 0 written as 12
//! mm = minute, zero-padded to 2 digits
//! ss = second, zero-padded to 2 digits
//! a  = "am" when hour < 12, otherwise "pm"
//! ```
//!
//! [`format_violation_row`] then joins the optional command and the
//! detail line:
//!
//! ```text
//! <time> <command>: <line>   when the command is present and non-empty
//! <time> <line>              otherwise
//! ```
//!
//! The event itself carries the wall-clock components, the raw `line`
//! and the optional `command` as parsed from the OS log stream.
//!
//! ## Pinned rules
//!
//! 1. **12-hour clock with am/pm.** Hour is `hour % 12` with 0 mapped
//! to 12 — so 0 → 12, 13 → 1. Minutes and seconds are zero-padded to
//! 2 digits; the hour is NOT padded.
//! 2. **AM/PM threshold is `< 12`.** Midnight (0) is am, noon (12) is
//! pm.
//! 3. **The command part is omitted entirely when `command` is `None`
//! or the empty string.** Both forms are treated alike, so the
//! leading space is gone too. Pinned by
//! `format_violation_row_empty_string_command_treated_as_none`.
//! 4. **There is ALWAYS a space between the (optional) command and
//! the line.** So with no command the row is `"<time> <line>"`; with a
//! command it is `"<time> <command>: <line>"`. An empty `line` still
//! emits that separator space.
//! 5. **The timestamp is the wall-clock at violation receive time.**
//! It is captured when the log line is parsed; here it is broken into
//! three fields the consumer fills in.

/// A blocked operation as reported by the sandbox on macOS.
///
/// `timestamp` is broken into pieces here so this crate has no
/// dependency on `chrono` / `time` / system clock. The consumer fills
/// in the components from whichever clock it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxViolationEvent {
    /// Hour of the day, 0..=23.
    pub hour: u8,
    /// Minute, 0..=59.
    pub minute: u8,
    /// Second, 0..=59.
    pub second: u8,
    /// The bash command that triggered the violation, when the log
    /// line carried one. `None` means absent; `Some("")` is treated
    /// the same way — both drop the command from the row.
    pub command: Option<String>,
    /// The violation detail string from the OS log stream.
    pub line: String,
}

/// Format an `(hour, minute, second)` triple as `"h:mm:ssa"`.
///
/// * `hour` is a 24-hour value in 0..=23.
/// * The output uses a 12-hour clock with am/pm.
/// * Minutes and seconds are zero-padded to 2 digits; the hour is not.
pub fn format_time(hour: u8, minute: u8, second: u8) -> String {
    let h12 = hour % 12;
    let h12 = if h12 == 0 { 12 } else { h12 };
    let ampm = if hour < 12 { "am" } else { "pm" };
    format!("{}:{:02}:{:02}{}", h12, minute, second, ampm)
}

/// Format a single violation row as it appears in the expanded view.
///
/// * No command, or an empty one → `"<time> <line>"`
/// * With a non-empty command → `"<time> <command>: <line>"`
pub fn format_violation_row(event: &SandboxViolationEvent) -> String {
    let time = format_time(event.hour, event.minute, event.second);
    let cmd_suffix = match event.command.as_deref() {
        Some("") | None => String::new(),
        Some(cmd) => format!(" {}:", cmd),
    };
    format!("{}{} {}", time, cmd_suffix, event.line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(hour: u8, command: Option<&str>, line: &str) -> SandboxViolationEvent {
        SandboxViolationEvent {
            hour,
            minute: 30,
            second: 45,
            command: command.map(|s| s.to_string()),
            line: line.to_string(),
        }
    }

    #[test]
    fn format_time_midnight_is_12am() {
        assert_eq!(format_time(0, 0, 0), "12:00:00am");
    }

    #[test]
    fn format_time_one_am() {
        assert_eq!(format_time(1, 5, 9), "1:05:09am");
    }

    #[test]
    fn format_time_eleven_am() {
        assert_eq!(format_time(11, 30, 45), "11:30:45am");
    }

    #[test]
    fn format_time_noon_is_12pm() {
        assert_eq!(format_time(12, 0, 0), "12:00:00pm");
    }

    #[test]
    fn format_time_one_pm_is_one_pm() {
        assert_eq!(format_time(13, 30, 45), "1:30:45pm");
    }

    #[test]
    fn format_time_eleven_pm() {
        assert_eq!(format_time(23, 59, 59), "11:59:59pm");
    }

    #[test]
    fn format_time_pads_minutes_and_seconds() {
        assert_eq!(format_time(9, 1, 2), "9:01:02am");
    }

    #[test]
    fn format_time_does_not_pad_hours() {
        // 1:05:09am, NOT 01:05:09am — the hour is written
        // unpadded, only minutes and seconds are padded.
        assert_eq!(format_time(1, 5, 9), "1:05:09am");
    }

    #[test]
    fn format_time_table() {
        // (hour, min, sec, expected)
        let table = [
            (0u8, 0u8, 0u8, "12:00:00am"),
            (0, 30, 0, "12:30:00am"),
            (1, 0, 0, "1:00:00am"),
            (5, 5, 5, "5:05:05am"),
            (11, 59, 59, "11:59:59am"),
            (12, 0, 0, "12:00:00pm"),
            (12, 30, 0, "12:30:00pm"),
            (13, 0, 0, "1:00:00pm"),
            (15, 30, 45, "3:30:45pm"),
            (23, 59, 59, "11:59:59pm"),
        ];
        for (h, m, s, expected) in table {
            assert_eq!(format_time(h, m, s), expected, "format_time({h},{m},{s})");
        }
    }

    #[test]
    fn format_violation_row_no_command() {
        let e = evt(13, None, "denied open /etc/passwd");
        assert_eq!(
            format_violation_row(&e),
            "1:30:45pm denied open /etc/passwd"
        );
    }

    #[test]
    fn format_violation_row_empty_string_command_treated_as_none() {
        // SECURITY-RELEVANT pin: the empty command string is
        // treated exactly like `None`, so the command marker is
        // omitted. We model `Some("")` and pin that it behaves the
        // same as `None`.
        let e = evt(13, Some(""), "denied open /etc/passwd");
        assert_eq!(
            format_violation_row(&e),
            "1:30:45pm denied open /etc/passwd"
        );
    }

    #[test]
    fn format_violation_row_with_command() {
        let e = evt(13, Some("ls /tmp"), "denied open /etc/passwd");
        assert_eq!(
            format_violation_row(&e),
            "1:30:45pm ls /tmp: denied open /etc/passwd"
        );
    }

    #[test]
    fn format_violation_row_with_command_and_midnight() {
        let e = evt(0, Some("ls /tmp"), "denied open /etc/passwd");
        assert_eq!(
            format_violation_row(&e),
            "12:30:45am ls /tmp: denied open /etc/passwd"
        );
    }

    #[test]
    fn format_violation_row_command_with_colon_already() {
        // Edge case: the command already contains a colon. No escaping
        // happens — a `:` is appended after the command regardless. We
        // pin that exact behaviour.
        let e = evt(13, Some("env: PATH=/bin"), "denied write");
        assert_eq!(
            format_violation_row(&e),
            "1:30:45pm env: PATH=/bin: denied write"
        );
    }

    #[test]
    fn format_violation_row_empty_line() {
        let e = evt(13, Some("ls"), "");
        // The separator space is still emitted when the line is empty.
        assert_eq!(format_violation_row(&e), "1:30:45pm ls: ");
    }

    #[test]
    fn format_violation_row_empty_line_no_command() {
        let e = evt(13, None, "");
        assert_eq!(format_violation_row(&e), "1:30:45pm ");
    }
}
