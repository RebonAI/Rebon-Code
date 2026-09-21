//! The denials this process has seen, kept so `/sandbox` can show them.
//!
//! A seatbelt denial is discovered by [`crate::runtime::macos_monitor`], which
//! reports it to whatever sink the session was built with. Until this module
//! existed the only sink was a `tracing::warn!` into a log file the terminal
//! never shows, so the violations tab of `/sandbox` had nothing to read and
//! the user learned nothing about why a command failed.
//!
//! [`sink`] keeps that warning and adds the store: the log line is the audit
//! trail, the store is the screen, and neither replaces the other.
//!
//! Process-wide rather than per-session on purpose. `log stream` is watched
//! per session, but a user opening `/sandbox` is asking "what has been
//! blocked", not "what has this particular `SessionSandbox` value blocked",
//! and the panel is opened from whichever session is in front of them.
//!
//! Only macOS reaches here. Linux's bubblewrap refusals surface as a failed
//! command rather than a log stream, and the Windows helper's
//! `SANDBOX_WIN_DENIED` marker is written to the child's stderr, which no
//! caller parses today — see the module note in
//! [`crate::runtime::windows`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use crate::runtime::macos::violations::Violation;
use crate::runtime::macos_monitor::ViolationSink;
use crate::view::violation::SandboxViolationEvent;

/// How many recent denials are kept.
///
/// The panel only ever shows the last ten
/// ([`crate::view::violation_view::VIOLATION_TAIL_LIMIT`]); the rest of the
/// budget is headroom so a burst of denials arriving while the panel is shut
/// does not cost anything to hold, and so a future surface that wants more
/// than ten has them. Bounded because a `deny default` profile against a
/// chatty program can produce denials indefinitely, and the total count is
/// kept separately so the header stays truthful after the queue has rolled.
pub const VIOLATION_CAPACITY: usize = 200;

#[derive(Default)]
struct Store {
    recent: VecDeque<SandboxViolationEvent>,
    total: usize,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

/// The sink a session is built with: the store, then the warning.
///
/// A denial that is recorded but not logged would take the audit trail away
/// from every headless run, so both happen and the log line is unchanged.
pub fn sink() -> Arc<dyn ViolationSink> {
    Arc::new(|violation: Violation| {
        record(&violation);
        tracing::warn!(
            operation = %violation.operation,
            "sandbox denied an operation: {}",
            violation.raw
        );
    })
}

/// Remember one denial.
pub fn record(violation: &Violation) {
    let event = to_event(violation, local_clock());
    let mut store = store().lock().unwrap_or_else(|err| err.into_inner());
    store.total = store.total.saturating_add(1);
    if store.recent.len() == VIOLATION_CAPACITY {
        store.recent.pop_front();
    }
    store.recent.push_back(event);
}

/// `(total ever seen, the recent ones in arrival order)`.
///
/// The total is not `recent.len()`: the queue rolls at
/// [`VIOLATION_CAPACITY`] and the header says how many operations were
/// blocked, not how many are still remembered.
pub fn snapshot() -> (usize, Vec<SandboxViolationEvent>) {
    let store = store().lock().unwrap_or_else(|err| err.into_inner());
    (store.total, store.recent.iter().cloned().collect())
}

/// Wall-clock hour, minute and second in the user's own timezone.
///
/// Local rather than UTC because the row sits next to the command the user
/// just ran, and a denial stamped three hours off reads as an old one.
fn local_clock() -> (u8, u8, u8) {
    use chrono::Timelike;
    let now = chrono::Local::now();
    (now.hour() as u8, now.minute() as u8, now.second() as u8)
}

/// One parsed denial as the violation view models it.
///
/// [`Violation::raw`] is two lines — the denial, then the session tag that
/// the log-stream predicate matched on — and only the first is about the
/// operation, so the tag is dropped here rather than wrapped onto a second
/// row of the panel.
///
/// `command` stays `None`: the macOS log stream reports the operation and
/// the path, and does not say which of the session's commands provoked it.
/// The view treats `None` and `Some("")` alike, so the row is
/// `"<time> <line>"`.
fn to_event(violation: &Violation, (hour, minute, second): (u8, u8, u8)) -> SandboxViolationEvent {
    let line = violation
        .raw
        .lines()
        .next()
        .unwrap_or(&violation.raw)
        .trim()
        .to_string();
    SandboxViolationEvent {
        hour,
        minute,
        second,
        command: None,
        line: if line.is_empty() {
            violation.operation.clone()
        } else {
            line
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn violation(operation: &str, raw: &str) -> Violation {
        Violation {
            operation: operation.to_string(),
            raw: raw.to_string(),
        }
    }

    #[test]
    fn the_tag_line_is_dropped_and_the_denial_kept() {
        let event = to_event(
            &violation(
                "file-read-data",
                "deny(1) file-read-data /etc/passwd\nrebon-sandbox-42",
            ),
            (13, 5, 9),
        );
        assert_eq!(event.line, "deny(1) file-read-data /etc/passwd");
        assert_eq!(event.command, None);
        assert_eq!((event.hour, event.minute, event.second), (13, 5, 9));
    }

    /// A denial whose message did not survive parsing still names its
    /// operation, because a row that says nothing is worse than a terse one.
    #[test]
    fn an_empty_message_falls_back_to_the_operation() {
        let event = to_event(
            &violation("network-outbound", "   \nrebon-sandbox-42"),
            (0, 0, 0),
        );
        assert_eq!(event.line, "network-outbound");
    }

    /// The store is process-wide, so this is the one test that writes to it:
    /// two tests appending to the same queue would each see the other's rows.
    #[test]
    fn the_queue_rolls_while_the_total_keeps_counting() {
        let mut store = Store::default();
        for index in 0..VIOLATION_CAPACITY + 5 {
            let event = to_event(
                &violation("file-read-data", &format!("deny {index}")),
                (1, 0, 0),
            );
            store.total += 1;
            if store.recent.len() == VIOLATION_CAPACITY {
                store.recent.pop_front();
            }
            store.recent.push_back(event);
        }
        assert_eq!(store.total, VIOLATION_CAPACITY + 5);
        assert_eq!(store.recent.len(), VIOLATION_CAPACITY);
        assert_eq!(store.recent.front().unwrap().line, "deny 5");
        assert_eq!(
            store.recent.back().unwrap().line,
            format!("deny {}", VIOLATION_CAPACITY + 4)
        );
    }

    /// The sink reports through the same path a session's monitor uses.
    #[test]
    fn the_sink_records_what_it_is_given() {
        let before = snapshot().0;
        sink().report(violation(
            "file-write-data",
            "deny(1) file-write-data /tmp/x\ntag",
        ));
        let (total, recent) = snapshot();
        assert_eq!(total, before + 1);
        assert_eq!(
            recent.last().unwrap().line,
            "deny(1) file-write-data /tmp/x"
        );
    }
}
