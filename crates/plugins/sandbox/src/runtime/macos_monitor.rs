//! Watching seatbelt refuse things — RFC §5.3.
//!
//! [`crate::runtime::macos::violations`] decides what a `log stream` line *means*.
//! This module is the process that produces the lines, and the loop that
//! feeds them through.
//!
//! ## Why it is worth having at all
//!
//! A seatbelt profile is `deny default`. When a command fails inside one, the
//! command's own output almost never says why — a tool reports "permission
//! denied" for a path, or nothing at all, and the fact that a sandbox refused
//! it is nowhere in what the model reads back. The refusal is recorded, but
//! only in the unified log, and only for as long as somebody is listening.
//! Nobody was, which made every macOS sandbox failure a puzzle with the
//! answer thrown away.
//!
//! ## Why the line source is injected
//!
//! [`Monitor::start`] spawns `log stream`; [`Monitor::start_with`] takes any
//! iterator of lines. That is not test scaffolding for its own sake — it is
//! what lets the filtering, the tag isolation, and the shutdown behaviour be
//! asserted on a machine that has no `log` binary, which is every machine the
//! CI runs on except one.
//!
//! ## The tag
//!
//! Each session's profile embeds a tag ([`crate::runtime::macos::session_log_tag`]),
//! the predicate selects on it, and [`crate::runtime::macos::violations::parse_line`]
//! checks it again. Two checks for one property because the predicate is a
//! string handed to another program: if it is ever wrong, the second check is
//! what stops one session from reporting another's denials as its own.

use crate::runtime::macos::violations::{self, Violation};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Where violations go.
///
/// A trait rather than a channel so the caller decides — the TUI surfaces
/// them, a headless run logs them, a test collects them.
pub trait ViolationSink: Send + Sync + 'static {
    fn report(&self, violation: Violation);
}

impl<F> ViolationSink for F
where
    F: Fn(Violation) + Send + Sync + 'static,
{
    fn report(&self, violation: Violation) {
        self(violation)
    }
}

/// A running monitor. Dropping it stops the reader and kills `log stream`.
pub struct Monitor {
    stop: Arc<AtomicBool>,
    child: Option<std::process::Child>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Monitor")
            .field("running", &!self.stop.load(Ordering::Relaxed))
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("could not start the macOS sandbox log monitor: {0}")]
    Spawn(#[source] std::io::Error),
}

/// The command the monitor runs.
pub const LOG_BINARY: &str = "/usr/bin/log";

/// The arguments for one session's tag.
///
/// `--style ndjson` because a seatbelt denial is **not one line**. The
/// profile's `(with message ...)` tag is appended to the message with a
/// newline before it, so the event reads:
///
/// ```text
/// Sandbox: sh(123) deny(1) file-write-create /x
/// rebon-sbx-...
/// ```
///
/// `--style compact` prints that as two physical lines, and a reader taking
/// them one at a time sees a denial carrying no tag followed by a tag
/// carrying no denial. [`violations::parse_line`] requires both in the same
/// string, so it matched neither half and violation reporting silently did
/// nothing on every real machine — the injected-line tests all put the tag
/// on the same line, which is the one shape `log` never produces.
///
/// The JSON record keeps the whole message, newline included, in a single
/// field on a single line. `--level default` keeps debug chatter out.
pub fn log_stream_argv(log_tag: &str) -> Vec<String> {
    vec![
        "stream".to_string(),
        "--style".to_string(),
        "ndjson".to_string(),
        "--level".to_string(),
        "default".to_string(),
        "--predicate".to_string(),
        violations::predicate(log_tag),
    ]
}

impl Monitor {
    /// Start `log stream` for `log_tag` and report what it refuses.
    ///
    /// Off macOS this is a no-op monitor rather than an error: the caller
    /// wires it unconditionally, and a platform with no seatbelt has no
    /// seatbelt denials to miss.
    pub fn start(log_tag: &str, sink: Arc<dyn ViolationSink>) -> Result<Self, MonitorError> {
        if !cfg!(target_os = "macos") {
            return Ok(Self::inert());
        }

        use std::process::{Command, Stdio};
        let mut child = Command::new(LOG_BINARY)
            .args(log_stream_argv(log_tag))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(MonitorError::Spawn)?;

        let stdout = child.stdout.take().ok_or_else(|| {
            MonitorError::Spawn(std::io::Error::other("log stream produced no stdout"))
        })?;

        let stop = Arc::new(AtomicBool::new(false));
        let reader = spawn_reader(
            std::io::BufReader::new(stdout),
            log_tag.to_string(),
            sink,
            stop.clone(),
        );

        Ok(Self {
            stop,
            child: Some(child),
            reader: Some(reader),
        })
    }

    /// A monitor over an arbitrary line source.
    pub fn start_with<R>(source: R, log_tag: &str, sink: Arc<dyn ViolationSink>) -> Self
    where
        R: std::io::BufRead + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let reader = spawn_reader(source, log_tag.to_string(), sink, stop.clone());
        Self {
            stop,
            child: None,
            reader: Some(reader),
        }
    }

    /// A monitor that watches nothing. Off macOS, and when the user turned
    /// violation reporting off.
    pub fn inert() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(true)),
            child: None,
            reader: None,
        }
    }

    /// Wait for the line source to end, reporting everything it produced.
    ///
    /// `log stream` never ends on its own, so in the product this blocks
    /// until the process is killed — [`Monitor::stop`] is what a session
    /// calls. It exists for a source that *does* end: a captured log, a
    /// replay, a test. The difference matters because `stop` is checked
    /// before each read and is therefore allowed to skip pending lines,
    /// which is the right behaviour for shutting down and the wrong one for
    /// draining.
    pub fn drain(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    /// Stop reading and terminate `log stream`.
    ///
    /// Called from [`Drop`] as well, so a session that ends without a clean
    /// shutdown does not leave a `log` process running for the rest of the
    /// machine's uptime — one per session, invisible, and each holding a
    /// predicate on a tag that will never appear again.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(child) = &mut self.child {
            // Killing it is what unblocks the reader thread: it is parked in
            // a blocking read on a pipe that only closes when the writer
            // goes away.
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The `eventMessage` field out of one `--style ndjson` record.
///
/// Separated from [`violations::parse_line`] because the two answer
/// different questions: this one unwraps the transport `log` chose, that one
/// decides what a message means. Keeping the envelope here is also what lets
/// `parse_line` stay a pure function over a message, testable without a JSON
/// fixture around every case.
///
/// `None` for anything that is not such a record. `log` prints a plain-text
/// banner naming the predicate before the first event, and a non-JSON line
/// must not be mistaken for a message — the banner contains the session tag,
/// so a lenient fallback that treated the raw line as the message would
/// report the banner itself as a violation.
pub fn event_message(line: &str) -> Option<String> {
    let record: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    Some(record.get("eventMessage")?.as_str()?.to_string())
}

fn spawn_reader<R>(
    source: R,
    log_tag: String,
    sink: Arc<dyn ViolationSink>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()>
where
    R: std::io::BufRead + Send + 'static,
{
    std::thread::spawn(move || {
        let mut source = source;
        let mut line = String::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            line.clear();
            match source.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    // Two steps, because they fail for different reasons: a
                    // line that is not a record at all (the banner `log`
                    // prints before the first event) is not the same as a
                    // record whose message is not a denial of ours.
                    let Some(message) = event_message(&line) else {
                        continue;
                    };
                    if let Some(violation) = violations::parse_line(&message, &log_tag) {
                        sink.report(violation);
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Collector(Mutex<Vec<Violation>>);

    impl ViolationSink for Arc<Collector> {
        fn report(&self, violation: Violation) {
            self.0.lock().unwrap().push(violation);
        }
    }

    fn run(lines: &str, log_tag: &str) -> Vec<Violation> {
        let collector = Arc::new(Collector::default());
        let mut monitor = Monitor::start_with(
            std::io::Cursor::new(lines.to_string()),
            log_tag,
            Arc::new(collector.clone()),
        );
        // The cursor ends, so the reader returns on its own and `drain`
        // joins it — deterministic, and without the flag that would let
        // shutdown skip lines still pending.
        monitor.drain();
        let collected = collector.0.lock().unwrap().clone();
        collected
    }

    const TAG: &str = "rebon-sbx-abc-1f4";

    /// One `--style ndjson` record carrying `message`.
    fn record(message: &str) -> String {
        serde_json::json!({
            "messageType": "Error",
            "processImagePath": "/kernel",
            "eventMessage": message,
        })
        .to_string()
    }

    /// A denial exactly as `log` delivers one.
    ///
    /// The tag is on **its own line inside the message**, which is where
    /// seatbelt's `(with message ...)` puts it. The fixture used to append it
    /// to the same line, and that one detail is why a monitor that reported
    /// nothing at all on a real machine passed every test here.
    fn deny_line(operation: &str, tag: &str) -> String {
        record(&format!(
            "Sandbox: node(123) deny(1) {operation} /etc/passwd\n{tag}"
        ))
    }

    #[test]
    fn a_denial_for_this_session_is_reported() {
        let violations = run(&deny_line("file-read-data", TAG), TAG);

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].operation, "file-read-data");
        assert!(violations[0].raw.contains("/etc/passwd"));
    }

    #[test]
    fn another_sessions_denial_is_not_reported() {
        // Two Rebon sessions on one machine each run their own monitor. The
        // predicate is supposed to separate them, but it is a string handed
        // to another program; this is the check that does not depend on it.
        let violations = run(&deny_line("file-read-data", "rebon-sbx-other-99"), TAG);

        assert!(violations.is_empty());
    }

    #[test]
    fn noisy_daemons_are_filtered_out() {
        // These are denied constantly under any `deny default` profile and
        // would bury the denials that are about the user's command.
        let lines: String = ["mDNSResponder", "diagnosticd", "analyticsd"]
            .iter()
            .map(|noise| {
                format!(
                    "{}\n",
                    record(&format!(
                        "Sandbox: {noise}(9) deny(1) mach-lookup com.apple.x\n{TAG}"
                    ))
                )
            })
            .collect();

        assert!(run(&lines, TAG).is_empty());
    }

    #[test]
    fn several_denials_arrive_in_order() {
        let lines = format!(
            "{}\n{}\n{}\n",
            deny_line("file-read-data", TAG),
            deny_line("network-outbound", TAG),
            deny_line("file-write-create", TAG),
        );

        let operations: Vec<String> = run(&lines, TAG)
            .into_iter()
            .map(|violation| violation.operation)
            .collect();

        assert_eq!(
            operations,
            vec!["file-read-data", "network-outbound", "file-write-create"]
        );
    }

    #[test]
    fn unrelated_log_lines_are_ignored() {
        let lines = format!(
            "{}\n{}\n",
            record(&format!("something entirely unrelated\n{TAG}")),
            record(&format!(
                "Sandbox: node(1) allow file-read-data /tmp\n{TAG}"
            )),
        );

        assert!(run(&lines, TAG).is_empty());
    }

    #[test]
    fn an_empty_stream_reports_nothing_and_still_stops() {
        assert!(run("", TAG).is_empty());
    }

    #[test]
    fn an_inert_monitor_is_safe_to_stop_twice() {
        // The drop impl calls `stop`, so an explicit stop must not make
        // dropping it a double-join.
        let mut monitor = Monitor::inert();
        monitor.stop();
        monitor.stop();
    }

    #[test]
    fn stopping_a_running_monitor_joins_its_reader() {
        // A monitor whose reader outlived it would keep reporting into a
        // sink the session has already discarded.
        let collector = Arc::new(Collector::default());
        let mut monitor = Monitor::start_with(
            std::io::Cursor::new(deny_line("file-read-data", TAG)),
            TAG,
            Arc::new(collector.clone()),
        );

        monitor.stop();

        assert!(monitor.reader.is_none());
    }

    #[test]
    fn the_predicate_selects_only_this_sessions_tag() {
        let argv = log_stream_argv(TAG);
        let predicate = argv.last().unwrap();

        assert!(predicate.contains(TAG), "{predicate}");
        assert!(
            predicate.starts_with("eventMessage ENDSWITH"),
            "{predicate}"
        );
    }

    #[test]
    fn the_argv_asks_for_one_record_per_event() {
        // The reader takes a line at a time, and a seatbelt denial spans two
        // of them in every text style — the tag sits on the second. Only
        // `ndjson` keeps the whole message on one line.
        let argv = log_stream_argv(TAG);
        assert_eq!(argv[0], "stream");
        assert!(argv.windows(2).any(|pair| pair == ["--style", "ndjson"]));
        assert!(argv.windows(2).any(|pair| pair == ["--level", "default"]));
    }

    #[test]
    fn a_denial_is_reported_when_its_tag_is_on_the_second_line() {
        // The shape `log` actually delivers, pinned on its own so a future
        // change to the fixture helper cannot quietly stop covering it.
        let line = record(&format!(
            "Sandbox: sh(30707) deny(1) file-write-create /etc/x\n{TAG}"
        ));

        let violations = run(&line, TAG);

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].operation, "file-write-create");
    }

    #[test]
    fn the_banner_before_the_first_event_is_not_a_violation() {
        // `log` announces its predicate in plain text on stdout, and that
        // banner quotes the tag. A reader that fell back to treating a
        // non-JSON line as the message would report it as a denial.
        let banner = format!("Filtering the log data using \"composedMessage ENDSWITH \"{TAG}\"\"");
        assert!(event_message(&banner).is_none());
        assert!(run(&format!("{banner}\n"), TAG).is_empty());
    }

    #[test]
    fn a_record_without_an_event_message_is_skipped() {
        let line = serde_json::json!({ "messageType": "Error" }).to_string();
        assert!(event_message(&line).is_none());
    }

    #[test]
    fn a_closure_can_be_the_sink() {
        // The common case: the caller wants a line logged, not a type.
        let seen = Arc::new(Mutex::new(0usize));
        let counter = seen.clone();
        let mut monitor = Monitor::start_with(
            std::io::Cursor::new(deny_line("file-read-data", TAG)),
            TAG,
            Arc::new(move |_violation: Violation| {
                *counter.lock().unwrap() += 1;
            }),
        );
        monitor.drain();

        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[test]
    fn starting_off_macos_is_inert_rather_than_an_error() {
        // The caller wires this unconditionally; a platform with no seatbelt
        // has no seatbelt denials to miss.
        if cfg!(target_os = "macos") {
            return;
        }
        let monitor = Monitor::start(TAG, Arc::new(Collector::default().into_arc()));
        assert!(monitor.is_ok());
    }

    impl Collector {
        fn into_arc(self) -> Arc<Collector> {
            Arc::new(self)
        }
    }
}
