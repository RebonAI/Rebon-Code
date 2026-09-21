//! ── "How long before the TUI answered my keystroke?" ─────────────
//!
//! Set `REBON_INPUT_PROBE_MS=<threshold>` and every keystroke that waits
//! longer than that for a frame gets a line in the TUI log
//! (`%TEMP%\rebon\logs\rebon.log`, or `$REBON_LOG_DIR`):
//!
//! ```text
//! INFO input probe: input waited for a frame waited_ms=1421 events=2947 burst=true echo=false
//! ```
//!
//! Three facts, because they separate the plausible causes:
//!
//! * `waited_ms` — what the user actually felt.
//! * `events` — how much input piled up behind the oldest unrendered
//!   one. A paste is thousands; a single slow frame is one. This alone
//!   says whether the TUI was buried in input or busy elsewhere.
//! * `burst` / `echo` — whether the paste burst detector or the paste
//!   echo suppressor was running for any of the held input, i.e. whether
//!   the stall belongs to the paste path at all.
//!
//! Off unless the variable is set, and when it is set the cost is two
//! `Instant::now()` calls per event, so it is fit to leave in place and
//! ask a user to switch on.

use std::time::{Duration, Instant};

/// Tracks the oldest input event that has not been shown to the user yet.
pub(super) struct InputLatencyProbe {
    threshold: Option<Duration>,
    /// When the oldest still-unrendered input event was read.
    pending_since: Option<Instant>,
    /// How many events have been read since — everything queued behind it.
    pending_events: usize,
    /// Whether any held event arrived while the paste path was running.
    burst_active: bool,
    echo_armed: bool,
}

impl InputLatencyProbe {
    pub(super) fn from_env() -> Self {
        let threshold = std::env::var("REBON_INPUT_PROBE_MS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .map(Duration::from_millis);
        if let Some(threshold) = threshold {
            tracing::info!(
                threshold_ms = threshold.as_millis() as u64,
                "input probe: reporting keystrokes that wait longer than this for a frame"
            );
        }
        Self {
            threshold,
            pending_since: None,
            pending_events: 0,
            burst_active: false,
            echo_armed: false,
        }
    }

    /// An input event was just read. Only the first one after a frame
    /// starts the clock — the rest are the backlog behind it.
    ///
    /// The paste flags accumulate across the whole backlog rather than
    /// describing only the oldest event, so the report answers "was the
    /// paste path involved in this stall at all", which is the question
    /// worth asking. Sticky is also the safer direction: a burst that
    /// starts one event into the backlog still gets attributed.
    pub(super) fn note_input(&mut self, burst_active: bool, echo_armed: bool) {
        if self.threshold.is_none() {
            return;
        }
        self.pending_events += 1;
        self.burst_active |= burst_active;
        self.echo_armed |= echo_armed;
        if self.pending_since.is_none() {
            self.pending_since = Some(Instant::now());
        }
    }

    /// A frame just went to the terminal: everything read before it has
    /// now been answered.
    ///
    /// `prompt_len` / `chips` ride along on a separate `frame_probe`
    /// target (`RUST_LOG=frame_probe=debug`, off by default): when the
    /// question is "did the screen stop updating, or was the state not
    /// there yet?", the only useful answer is a per-frame record of what
    /// the prompt held at the time.
    pub(super) fn note_frame(&mut self, prompt_len: usize, chips: usize) {
        tracing::debug!(target: "frame_probe", prompt_len, chips, "frame");
        let Some(threshold) = self.threshold else {
            return;
        };
        let Some(since) = self.pending_since.take() else {
            return;
        };
        let waited = since.elapsed();
        let events = std::mem::take(&mut self.pending_events);
        let burst = std::mem::take(&mut self.burst_active);
        let echo = std::mem::take(&mut self.echo_armed);
        if waited >= threshold {
            tracing::info!(
                waited_ms = waited.as_millis() as u64,
                events,
                burst,
                echo,
                "input probe: input waited for a frame"
            );
        }
    }
}

/// Counts what one pass of the paste drain actually did.
///
/// `RUST_LOG=drain_probe=debug` turns it on, and a pass is only reported
/// when it took at least [`DRAIN_REPORT_FLOOR`]:
///
/// ```text
/// DEBUG drain ms=16 chars=11300 refills=13 refilled=22593 reads=8 stashed=0
/// ```
///
/// It exists because the two things that made pasting slow were both
/// invisible from outside the drain. `reads` counts characters taken one
/// console round trip at a time, so `chars` far above `reads` means the
/// batched read is working and `reads` climbing with `chars` means it is
/// being refused. `stashed` counts what the pass handed back to the
/// outer loop, which is the other way a paste turns into thousands of
/// full loop iterations.
pub(super) struct DrainProbe {
    started: Instant,
    pub(super) chars: usize,
    pub(super) refills: usize,
    pub(super) refilled: usize,
    pub(super) reads: usize,
}

/// A drain faster than this is not the reason anything felt slow.
const DRAIN_REPORT_FLOOR: Duration = Duration::from_millis(5);

impl DrainProbe {
    pub(super) fn start() -> Self {
        Self {
            started: Instant::now(),
            chars: 0,
            refills: 0,
            refilled: 0,
            reads: 0,
        }
    }

    pub(super) fn finish(&self, stashed: usize) {
        let elapsed = self.started.elapsed();
        if elapsed < DRAIN_REPORT_FLOOR {
            return;
        }
        tracing::debug!(
            target: "drain_probe",
            ms = elapsed.as_millis() as u64,
            chars = self.chars,
            refills = self.refills,
            refilled = self.refilled,
            reads = self.reads,
            stashed,
            "drain"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(threshold_ms: u64) -> InputLatencyProbe {
        InputLatencyProbe {
            threshold: Some(Duration::from_millis(threshold_ms)),
            pending_since: None,
            pending_events: 0,
            burst_active: false,
            echo_armed: false,
        }
    }

    #[test]
    fn the_clock_starts_at_the_first_unrendered_input() {
        let mut probe = probe(0);
        probe.note_input(true, false);
        let first = probe.pending_since.expect("clock started");
        probe.note_input(false, true);
        assert_eq!(
            probe.pending_since,
            Some(first),
            "later input must not reset it"
        );
        assert_eq!(probe.pending_events, 2);
        // Paste state is sticky across the backlog, not just the oldest.
        assert!(probe.burst_active);
        assert!(probe.echo_armed);
    }

    #[test]
    fn a_frame_clears_the_backlog() {
        let mut probe = probe(0);
        probe.note_input(false, false);
        probe.note_input(false, false);
        probe.note_input(true, true);
        probe.note_frame(0, 0);
        assert!(probe.pending_since.is_none());
        assert_eq!(probe.pending_events, 0);
        // A frame also clears the sticky paste flags, so the next report
        // describes its own backlog and not an old one.
        assert!(!probe.burst_active);
        assert!(!probe.echo_armed);
        // A frame with nothing pending is a no-op, not a zero-length report.
        probe.note_frame(0, 0);
        assert!(probe.pending_since.is_none());
    }

    #[test]
    fn unset_env_makes_every_call_a_no_op() {
        let mut probe = InputLatencyProbe {
            threshold: None,
            pending_since: None,
            pending_events: 0,
            burst_active: false,
            echo_armed: false,
        };
        probe.note_input(true, true);
        assert!(probe.pending_since.is_none());
        assert_eq!(probe.pending_events, 0);
        probe.note_frame(0, 0);
    }
}
