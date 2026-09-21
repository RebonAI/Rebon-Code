// ── Non-bracketed paste burst detector ───────────────────────────
//
// Time-based state machine for detecting pasted input bursts.
// The earlier implementation used an `event::poll(Duration::from_millis(2))`
// queue peek to decide whether a text key was the first of a paste
// burst, but on Windows the scheduler timer granularity (~15.6ms)
// makes any sub-tick poll quantize unpredictably — the peek
// frequently reports "no more events" even when a multi-line paste
// is actively streaming, which caused each pasted `\n` to fall
// through as `KeyCode::Enter` → `KeyAction::Submit` → `submit_or_queue`
// and ended up as a fresh queued prompt per line. Users reported
// this exact failure mode ("the whole multi-line paste goes to the queue").
//
// Instead we track time between successive plain-char / Enter events
// across event-loop iterations. If four consecutive chars arrive
// within `BURST_CHAR_INTERVAL`, we assume a paste and start
// diverting subsequent chars into `buffer` instead of the prompt.
// Enter during an active burst appends `\n` to the buffer instead
// of submitting. When `BURST_IDLE_TIMEOUT` elapses without another
// event the buffer flushes through `apply_paste_to_app`, which
// handles the chip-vs-inline decision in rebon_tui::promptinput the
// same way bracketed paste does.
//
// Once active, plain chars and Enter stay latched in the
// buffer until the idle timeout. Any non-char non-Enter input
// force-flushes the buffer and then applies the original event,
// so cursor moves and editing keys always land on a consistent
// prompt state.
//
// Two separate intervals gate the state machine:
//
// * `BURST_CHAR_INTERVAL` — strict, used to *activate*. Fast typing
//   at ~60–80ms/char should NOT trip this; the threshold stays
//   tight so only an obviously paste-speed burst activates.
//
// * `BURST_CONTINUATION_INTERVAL` — lenient, used before the burst
//   has activated. Windows conhost can straggle the first chunk of a
//   paste or the first pasted newline by 60–120ms, so pre-activation
//   detection needs a wider window than the strict 30ms char burst.
//   Once we have committed to "this is a paste", the burst stays
//   latched until `BURST_IDLE_TIMEOUT`, so continuation chunks keep
//   joining the same buffered paste.
//
// * `BURST_IDLE_TIMEOUT` — how long silence must persist before
//   the pending burst flushes. This is the real "paste is over"
//   signal once a burst has activated.
//
// Windows needs larger continuation/idle thresholds than Unix
// because paste events traverse the console input queue with
// extra latency: the 120ms continuation / 150ms idle values sit
// safely above the 15.6ms timer tick and typical batching jitter.
//
// The strict activation interval, however, must stay BELOW human
// typing speed. Real Windows non-bracketed pastes deliver chars
// in tight bursts of 1–5ms (a single ReadConsoleInput batch fans
// out as separate KEY_EVENT records dispatched in microseconds),
// while a fast typist sustains >=40–60ms between consecutive
// keystrokes. With the runner's input fast path skipping the
// background pipeline whenever events are queued, observed inter-
// event gaps inside a paste sit at terminal-actual pace, so a 30ms
// Windows / 8ms Unix activation interval cleanly separates pastes
// from typing. (Without the fast path the prior loop inflated
// inter-event gaps to 15–30ms via drains/refreshes/render, which
// made any sub-30ms threshold misfire — see runner::event_loop.)
// Activation here is the strict signal; queue-depth detection in
// `runner::detect_paste_batch` is the secondary signal that catches
// pastes whose first char arrives at typing pace but whose tail is
// already queued in the crossterm buffer.

use std::time::{Duration, Instant};

use rebon_tui::promptinput::clamp_cursor_offset;
use rebon_tui::promptinput::paste_flow::{
    apply_text_paste, plan_image_paste, ApplyTextPasteState, ImagePasteInput,
};

use crate::tui::app::AppState;
use crate::tui::event::TextEdit;
use crate::tui::runner::layout_and_scroll::apply_prompt_input_repin;
use crate::tui::terminal::TerminalGuard;

/// Apply a decoded image paste to the prompt at the cursor and store
/// the base64 payload in `AppState::pasted_contents`.
pub(super) fn apply_image_paste_to_app(
    app: &mut AppState,
    image: crate::tui::clipboard_image::ClipboardImage,
) {
    apply_prompt_input_repin(app);
    let next_paste_id = app.next_paste_id;
    let plan = plan_image_paste(&ImagePasteInput {
        next_paste_id,
        image: image.data,
        media_type: Some(image.media_type),
        filename: image.filename,
        source_path: image.source_path,
        pending_space_after_pill: false,
    });

    let cursor = clamp_cursor_offset(&app.input, app.cursor_offset);
    let mut next = String::with_capacity(app.input.len() + plan.text_to_insert.len());
    next.push_str(&app.input[..cursor]);
    next.push_str(&plan.text_to_insert);
    next.push_str(&app.input[cursor..]);
    app.input = next;
    app.cursor_offset = cursor + plan.text_to_insert.len();

    app.pasted_contents.push(plan.new_content);
    app.next_paste_id = next_paste_id.saturating_add(1);
}

/// Handle `Alt+V` / `Option+V`: read the clipboard as bitmap or image
/// path only. Silent no-op when the clipboard has no image.
pub(super) fn apply_paste_image_from_clipboard(app: &mut AppState) {
    let Some(image) = crate::tui::clipboard_image::read_clipboard_image_or_path() else {
        return;
    };
    apply_image_paste_to_app(app, image);
}

/// Handle `Ctrl+V` / `Cmd+V`: read the clipboard and paste into the
/// prompt. Tries image first (bitmap or image path); if no image is
/// found, falls back to reading clipboard text and routing it through
/// the same `apply_paste_to_app` path that `Event::Paste` uses.
///
/// This gives instant paste on terminals that forward Ctrl+V as a key
/// event instead of synthesizing `Event::Paste` or feeding chars one
/// by one — the app reads the clipboard directly, bypassing the burst
/// detector entirely.
pub(super) fn apply_paste_from_clipboard(app: &mut AppState) {
    if let Some(image) = crate::tui::clipboard_image::read_clipboard_image_or_path() {
        apply_image_paste_to_app(app, image);
        return;
    }

    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return;
    };
    let Ok(text) = clipboard.get_text() else {
        return;
    };
    if text.is_empty() {
        return;
    }

    // Some terminals both forward Ctrl+V as a key AND synthesize a
    // paste for the same gesture. Arm the echo suppressor with the
    // text we are about to apply so that replay gets dropped instead
    // of pasting twice. Direct-key mode never matches key events, so
    // typing right after Ctrl+V is unaffected; if no echo arrives the
    // suppressor simply expires.
    let echo_expected = super::paste_echo::paste_echo_enabled()
        .then(|| rebon_tui::promptinput::paste_flow::normalize_pasted_text(&text));

    let terminal_rows = app.prev_frame_area.map(|r| r.height as i32).unwrap_or(24);
    apply_paste_to_app(app, text, terminal_rows);

    if let Some(expected) = echo_expected.filter(|e| !e.is_empty()) {
        app.paste_echo = Some(
            super::paste_echo::PasteEchoSuppressor::for_direct_key_paste(
                expected,
                std::time::Instant::now(),
            ),
        );
    }
}

/// Thin adapter around [`apply_text_paste`] — snapshots `app`'s
/// paste-relevant slots, hands them to the paste planner in
/// `rebon_tui::promptinput`, and writes the result back. Keeping the
/// state-mutation pattern here (rather than inline in the event
/// loop) lets the burst-detection fallback reuse the same adapter
/// with a synthetic paste string.
pub(super) fn apply_paste_to_app(app: &mut AppState, raw_text: String, terminal_rows: i32) {
    apply_prompt_input_repin(app);
    if let Some(image) = crate::tui::clipboard_image::read_image_file_from_pasted_text(&raw_text) {
        apply_image_paste_to_app(app, image);
        return;
    }

    let result = apply_text_paste(ApplyTextPasteState {
        raw_text,
        current_input: app.input.clone(),
        cursor_offset: app.cursor_offset,
        pasted_contents: std::mem::take(&mut app.pasted_contents),
        next_paste_id: app.next_paste_id,
        terminal_rows,
    });
    app.input = result.input;
    app.cursor_offset = result.cursor_offset;
    app.pasted_contents = result.pasted_contents;
    app.next_paste_id = result.next_paste_id;
    if let Some(next_mode) = result.next_mode {
        // `HistoryMode` is stored as its lowercase debug string on
        // the app side, matching `plan_input_change` in dispatch.rs.
        app.mode = format!("{next_mode:?}").to_lowercase();
    }
}

/// Walk back `retro_chars` UTF-8 char boundaries from the current
/// cursor, lift those chars out of `app.input`, and return them so the
/// caller can prepend them to the burst buffer. The cursor moves back
/// to the lift-out point; everything after the original cursor stays
/// in the prompt unchanged.
pub(super) fn retro_grab_at_cursor(app: &mut AppState, retro_chars: usize) -> String {
    let cursor = clamp_cursor_offset(&app.input, app.cursor_offset);
    let mut start = cursor;
    for _ in 0..retro_chars {
        if start == 0 {
            break;
        }
        start -= 1;
        while start > 0 && !app.input.is_char_boundary(start) {
            start -= 1;
        }
    }
    let grabbed = app.input[start..cursor].to_string();
    app.input.replace_range(start..cursor, "");
    app.cursor_offset = start;
    grabbed
}

pub(super) fn paste_candidate_prefix_at_cursor(
    app: &AppState,
    candidate_chars: usize,
    current_ch: char,
    queued_ch: char,
) -> String {
    let cursor = clamp_cursor_offset(&app.input, app.cursor_offset);
    let mut start = cursor;
    for _ in 0..candidate_chars.saturating_sub(1) {
        if start == 0 {
            break;
        }
        start -= 1;
        while start > 0 && !app.input.is_char_boundary(start) {
            start -= 1;
        }
    }
    let mut prefix =
        String::with_capacity(cursor - start + current_ch.len_utf8() + queued_ch.len_utf8());
    prefix.push_str(&app.input[start..cursor]);
    prefix.push(current_ch);
    prefix.push(queued_ch);
    prefix
}

// Strict activation interval. Must sit below human typing speed
// so a fast typist's continuous keystrokes never accumulate enough
// `consecutive_fast` ticks to trip activation. With the input
// fast-path in `runner::event_loop` skipping the background pipeline
// when events are queued, observed inter-key gaps inside a paste
// drop back to terminal-actual (~5ms on Windows, <2ms on Unix),
// matching codex-ref's measured envelope. 18ms / 8ms stay safely
// above the platform's batch noise but well below sustained typing.
#[cfg(windows)]
pub(super) const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(18);
#[cfg(not(windows))]
pub(super) const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);

/// Inter-event gap tolerated while the detector is still
/// collecting a pre-activation candidate stream. Much more
/// lenient than `BURST_CHAR_INTERVAL` because Windows can
/// straggle the first chunk of a paste or the first pasted
/// newline by tens of milliseconds before we have enough
/// signal to commit to burst mode.
#[cfg(windows)]
pub(super) const BURST_CONTINUATION_INTERVAL: Duration = Duration::from_millis(120);
#[cfg(not(windows))]
pub(super) const BURST_CONTINUATION_INTERVAL: Duration = Duration::from_millis(35);

#[cfg(windows)]
pub(super) const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(150);
#[cfg(not(windows))]
pub(super) const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(50);

/// Minimum strict-fast count before Enter may timing-activate a
/// burst. Kept intentionally higher than the queue-depth fallback:
/// a short queued typing burst after a render stall can collapse
/// into 1–2 ms processing gaps, and `abc<Enter>` must still submit
/// normally instead of flipping the prompt into a synthetic paste.
/// Short first-line multiline pastes are recovered by the runner's
/// `detect_paste_batch_after_enter` path, which is more reliable than
/// timing once a newline is involved.
pub(super) const BURST_ACTIVATION_COUNT: u16 = 5;
pub(super) const BURST_CHAR_ACTIVATION_COUNT: u16 = 4;

/// Minimum strict-fast chars before the runner may trust a single
/// queued follow-up as a plain-char paste signal. This prevents a
/// short queued typing burst from flipping into paste mode just
/// because one more key happened to be buffered behind a brief render
/// stall, while still rescuing real pastes whose tail is already
/// queued when timing on the current char was inconclusive.
pub(super) const BURST_BATCH_FAST_COUNT: u16 = 3;

/// Large-stream threshold used as a non-bracketed paste fallback.
/// Even if the first pasted chars arrive too slowly to satisfy the
/// strict activation timing, a sustained stream this large is still
/// overwhelmingly a paste rather than ordinary typing.
pub(super) const BURST_LENGTH_THRESHOLD: usize = 800;

pub(super) struct PasteBurst {
    /// Timestamp of the most recent plain char or Enter the detector
    /// observed. Used both for the "fast consecutive" classification
    /// and for the idle-flush timeout.
    last_event_at: Option<Instant>,
    /// Rolling count of consecutive fast events. Plain chars
    /// increment this in `on_char`; `on_enter` can also bump it
    /// while the burst is still inactive so short first-line
    /// pastes activate before newline submits.
    consecutive_fast: u16,
    /// Count of consecutive plain ASCII chars observed while the
    /// detector is still inactive. Uses the lenient continuation
    /// interval so large pastes can still promote into burst mode
    /// once they cross `BURST_LENGTH_THRESHOLD`.
    candidate_chars: usize,
    /// True once a non-bracketed paste has activated. Stays latched
    /// until `BURST_IDLE_TIMEOUT` expires so follow-up chunks keep
    /// joining the same paste without needing to re-win timing.
    active: bool,
    /// True when activation came from an authoritative paste signal
    /// (queue depth / pasted Enter / drain), so CJK body text should
    /// stay in the buffer instead of being treated as an IME commit.
    trusted_paste_stream: bool,
    /// Accumulated burst text — chars plus any `\n` absorbed from
    /// `on_enter`. Flushes whole through `apply_paste_to_app`.
    buffer: String,
    /// Whether clipboard adoption has already been attempted for the
    /// current buffered burst.
    adoption_attempted: bool,
    /// Whether the current pre-activation candidate stream has already
    /// probed the clipboard for an image-path match.
    image_path_probe_attempted: bool,
}

#[derive(Debug)]
#[must_use]
pub(super) enum CharOutcome {
    /// Apply the original `KeyAction::TextEdit` normally.
    PassThrough,
    /// Char was diverted into the burst buffer; skip the text edit.
    Buffered,
    /// Burst just activated: this char is in the buffer, AND the
    /// caller must retro-grab `retro_chars` characters from the end
    /// of the prompt (already-inserted prefix) and prepend them to
    /// the buffer so the eventual flush covers the full paste.
    ActivatedRetro { retro_chars: u16 },
    /// Burst ended: flush this accumulated text into the prompt
    /// first (as a paste), then apply the original text edit.
    FlushThenPassThrough(String),
}

#[derive(Debug)]
#[must_use]
pub(super) enum EnterOutcome {
    /// Submit the prompt as usual.
    Submit,
    /// Enter was absorbed as `\n` inside an active burst.
    Buffered,
    /// Enter just activated the burst: `\n` is in the buffer, AND
    /// the caller must retro-grab `retro_chars` characters from the
    /// prompt and prepend them to the buffer — same as
    /// `CharOutcome::ActivatedRetro`.
    ActivatedRetro { retro_chars: u16 },
}

impl PasteBurst {
    pub(super) fn new() -> Self {
        Self {
            last_event_at: None,
            consecutive_fast: 0,
            candidate_chars: 0,
            active: false,
            trusted_paste_stream: false,
            buffer: String::new(),
            adoption_attempted: false,
            image_path_probe_attempted: false,
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    pub(super) fn pending_text(&self) -> &str {
        &self.buffer
    }

    pub(super) fn pending_len(&self) -> usize {
        self.buffer.len()
    }

    pub(super) fn adoption_attempted(&self) -> bool {
        self.adoption_attempted
    }

    pub(super) fn mark_adoption_attempted(&mut self) {
        self.adoption_attempted = true;
    }

    pub(super) fn image_path_probe_attempted(&self) -> bool {
        self.image_path_probe_attempted
    }

    pub(super) fn mark_image_path_probe_attempted(&mut self) {
        self.image_path_probe_attempted = true;
    }

    /// Snapshot of the strict consecutive-fast counter at the moment of
    /// inspection. The runner uses this *before* calling `on_enter` so
    /// that, if the queue-depth signal then forces an Enter-driven
    /// activation, it knows how many trailing chars in the prompt were
    /// part of the paste stream and need to be retro-lifted.
    pub(super) fn consecutive_fast(&self) -> u16 {
        self.consecutive_fast
    }

    /// Snapshot of the lenient candidate stream length. Unlike the
    /// strict `consecutive_fast` counter, this survives Windows paste
    /// jitter up to `BURST_CONTINUATION_INTERVAL`, so Enter-driven
    /// activation can recover the full first pasted line instead of
    /// only the last fast character.
    pub(super) fn candidate_chars(&self) -> usize {
        self.candidate_chars
    }

    /// Feed a plain char through the detector.
    pub(super) fn on_char(&mut self, ch: char, now: Instant) -> CharOutcome {
        let fast_activation = self
            .last_event_at
            .is_some_and(|prev| now.duration_since(prev) <= BURST_CHAR_INTERVAL);
        let fast_continuation = self
            .last_event_at
            .is_some_and(|prev| now.duration_since(prev) <= BURST_CONTINUATION_INTERVAL);
        if !self.active && !fast_continuation {
            self.image_path_probe_attempted = false;
        }

        // IME commits (Chinese/Japanese/Korean input methods) deliver
        // CJK characters in rapid succession — e.g. typing a Pinyin
        // phrase and hitting space dispatches each committed CJK char
        // as a separate KeyCode::Char event within ~1ms. Without this
        // guard, the burst detector would classify them as a paste,
        // retro-grab the early chars from the prompt, and reinsert
        // them after `BURST_IDLE_TIMEOUT` — producing the "IME drifts"
        // visual drift users report (chars appear, vanish, reappear).
        //
        // Non-bracketed paste on legacy terminals — the scenario this
        // detector actually exists for — is overwhelmingly ASCII
        // (code, shell output, URLs), so exempting non-ASCII chars
        // preserves the paste fallback without fighting IMEs.
        if !ch.is_ascii() {
            if self.active {
                if self.trusted_paste_stream {
                    self.buffer.push(ch);
                    self.last_event_at = Some(now);
                    return CharOutcome::Buffered;
                }
                // A pending ASCII burst should flush into the prompt
                // before the IME char inserts, so the two streams
                // don't interleave into a scrambled buffer.
                let flushed = std::mem::take(&mut self.buffer);
                self.active = false;
                self.trusted_paste_stream = false;
                self.consecutive_fast = 0;
                self.candidate_chars = 0;
                self.adoption_attempted = false;
                self.image_path_probe_attempted = false;
                self.last_event_at = Some(now);
                return CharOutcome::FlushThenPassThrough(flushed);
            }
            // Reset the consecutive-fast counter so an ASCII char
            // that arrives right after an IME commit doesn't inherit
            // a stale count and immediately activate.
            self.consecutive_fast = 0;
            // Still track non-ASCII chars as paste candidates. They
            // must not activate by themselves, but if a queued pasted
            // newline arrives next, the runner needs this count to
            // retro-grab a Chinese/Japanese/Korean first line into the
            // paste buffer instead of leaving it as raw prompt text.
            self.candidate_chars = if fast_continuation {
                self.candidate_chars.saturating_add(1)
            } else {
                1
            };
            self.last_event_at = Some(now);
            return CharOutcome::PassThrough;
        }

        if self.active {
            // Once a paste has activated, keep it latched until the
            // idle timeout/flush decision. Follow-up chunks no longer
            // need to re-win any timing race to stay attached to the
            // same buffered paste.
            self.buffer.push(ch);
            self.last_event_at = Some(now);
            return CharOutcome::Buffered;
        }

        if fast_activation {
            self.consecutive_fast = self.consecutive_fast.saturating_add(1);
        } else {
            self.consecutive_fast = 1;
        }
        if fast_continuation {
            self.candidate_chars = self.candidate_chars.saturating_add(1);
        } else {
            self.candidate_chars = 1;
        }
        self.last_event_at = Some(now);

        if fast_activation && self.consecutive_fast >= BURST_CHAR_ACTIVATION_COUNT {
            self.active = true;
            self.trusted_paste_stream = false;
            self.candidate_chars = 0;
            self.buffer.push(ch);
            return CharOutcome::ActivatedRetro {
                retro_chars: self.consecutive_fast - 1,
            };
        }

        if self.candidate_chars >= BURST_LENGTH_THRESHOLD {
            self.active = true;
            self.trusted_paste_stream = true;
            self.consecutive_fast = BURST_CHAR_ACTIVATION_COUNT;
            self.buffer.push(ch);
            let retro_chars = (self.candidate_chars - 1) as u16;
            self.candidate_chars = 0;
            return CharOutcome::ActivatedRetro { retro_chars };
        }

        CharOutcome::PassThrough
    }

    /// Prepend retro-grabbed text to the front of the buffer.
    pub(super) fn prepend_retro(&mut self, grabbed: &str) {
        if grabbed.is_empty() {
            return;
        }
        let mut new = String::with_capacity(grabbed.len() + self.buffer.len());
        new.push_str(grabbed);
        new.push_str(&self.buffer);
        self.buffer = new;
    }

    /// Feed an Enter through the detector. Returns whether it was
    /// absorbed, needs a flush, or should submit normally.
    ///
    /// Activation rule: Enter only activates a burst if the *strict*
    /// `consecutive_fast` counter — the one `on_char` builds when chars
    /// arrive at <= `BURST_CHAR_INTERVAL` (paste pace) — already sits at
    /// the per-Enter activation threshold. Earlier revisions had Enter
    /// itself bump that counter using the lenient continuation window,
    /// which misclassified ordinary "type a sentence then press Enter"
    /// as a paste — every Enter trivially landed within 120 ms of the
    /// previous char. Combined with the active-latch (Enter stays
    /// buffered until the 150 ms idle timeout), the buffer would flush
    /// as a paste insertion instead of submitting the prompt — the user
    /// could no longer send messages.
    ///
    /// Timing is now intentionally conservative on Enter: short first-
    /// line multiline pastes are recovered by the runner's queue-depth
    /// `detect_paste_batch_after_enter` path, so a compressed typing
    /// burst like `abc<Enter>` can stay on the normal Submit path even
    /// if it was processed at paste-speed gaps after a render stall.
    pub(super) fn on_enter(&mut self, now: Instant) -> EnterOutcome {
        if self.active {
            // Once the burst is confirmed active, the authoritative
            // "paste is over" signal is `BURST_IDLE_TIMEOUT` firing in
            // `flush_if_idle`, not a per-Enter inter-event gap. The
            // earlier revision tried to distinguish pasted `\n` from
            // a user-pressed Enter by checking whether the gap from
            // the previous char was ≤ `BURST_CHAR_INTERVAL` (18 ms on
            // Windows), falling back to `FlushThenSubmit`. But Windows
            // conpty delivers pastes in batches separated by tens of
            // milliseconds of scheduler jitter, so every inter-batch
            // newline tripped that "slow gap" check and got submitted.
            // The runner then cleared the prompt via
            // `apply_submit → clear_input`, and the user saw only the
            // trailing line of a multi-line paste survive.
            //
            // Trade-off: if the user presses Enter within
            // `BURST_IDLE_TIMEOUT` of the last pasted char, the Enter
            // joins the buffer and the eventual idle flush produces a
            // paste chip with a trailing `\n`; the user then presses
            // Enter again to submit. This is a minor double-press
            // versus the previous content-loss bug.
            self.buffer.push('\n');
            self.last_event_at = Some(now);
            self.consecutive_fast = BURST_ACTIVATION_COUNT;
            self.candidate_chars = 0;
            self.trusted_paste_stream = true;
            return EnterOutcome::Buffered;
        }

        let fast_continuation = self
            .last_event_at
            .is_some_and(|prev| now.duration_since(prev) <= BURST_CONTINUATION_INTERVAL);

        // Roll the candidate-chars stream forward so the length
        // threshold path can still activate on long, straggly pastes.
        // Note: do NOT touch `consecutive_fast` here — Enter must rely
        // on what `on_char` already accumulated at the strict
        // `BURST_CHAR_INTERVAL` pace, otherwise normal typing trips the
        // Enter activation.
        let candidate_chars = if fast_continuation {
            self.candidate_chars.saturating_add(1)
        } else {
            1
        };
        self.candidate_chars = candidate_chars;

        // Activation criterion: Enter on the heels of an already-fast
        // char stream. `consecutive_fast` is at >= 2 only when at least
        // two prior chars came in within `BURST_CHAR_INTERVAL` of each
        // other — i.e. genuine paste pace. Normal typists rarely hit
        // 30 ms char-to-char, so this stays clear of the keyboard.
        let enter_activation_count = BURST_ACTIVATION_COUNT.saturating_sub(1).max(1);
        if fast_continuation && self.consecutive_fast >= enter_activation_count {
            let retro_chars = self.consecutive_fast;
            self.active = true;
            self.trusted_paste_stream = true;
            self.candidate_chars = 0;
            self.consecutive_fast = BURST_ACTIVATION_COUNT;
            self.last_event_at = Some(now);
            self.buffer.push('\n');
            return EnterOutcome::ActivatedRetro { retro_chars };
        }

        // Length-threshold fallback — same as `on_char`. A sustained
        // continuation stream that never tripped the strict char
        // window (Windows conhost straggling) can still activate when
        // it crosses the equivalent 800-char threshold.
        if candidate_chars >= BURST_LENGTH_THRESHOLD {
            self.active = true;
            self.trusted_paste_stream = true;
            self.consecutive_fast = BURST_ACTIVATION_COUNT;
            self.candidate_chars = 0;
            self.last_event_at = Some(now);
            self.buffer.push('\n');
            return EnterOutcome::ActivatedRetro {
                retro_chars: (candidate_chars - 1) as u16,
            };
        }

        // Normal Submit path. Reset detector state so the next
        // typed char starts a fresh detection cycle.
        self.consecutive_fast = 0;
        self.candidate_chars = 0;
        self.last_event_at = None;
        self.image_path_probe_attempted = false;
        EnterOutcome::Submit
    }

    /// Force-activate the burst from a queue-depth paste signal. Used
    /// when the runner's outer loop has observed another `Press` key
    /// event already queued behind the current char — on Windows that
    /// is a definitive paste signal (typing produces Press+Release
    /// pairs separated by human reaction-time gaps, while paste dumps
    /// the whole batch into the console input buffer at once, so
    /// consuming one Press + its Release reveals another Press).
    ///
    /// Unlike the timing-based activation path in `on_char`, this
    /// does not care about inter-event gaps — the outer loop's sync
    /// work (drain_ui_channels, drain_team_mailbox, dialog refreshes,
    /// runtime-state derivation, …) routinely stretches the gap
    /// between consecutive char events past `BURST_CHAR_INTERVAL`
    /// even when the terminal delivered them 1 ms apart, so the
    /// 3-fast-char heuristic never trips during real pastes. The
    /// queue-depth signal sidesteps that entirely.
    pub(super) fn batch_activate_with_char(&mut self, ch: char, now: Instant) {
        self.active = true;
        self.trusted_paste_stream = true;
        self.consecutive_fast = BURST_ACTIVATION_COUNT;
        self.candidate_chars = 0;
        self.last_event_at = Some(now);
        self.buffer.push(ch);
    }

    /// Force-activate the burst from a plain Enter that clearly has
    /// more queued input behind it. Used for non-bracketed multi-line
    /// paste on Windows/conhost where the first line may have arrived
    /// too slowly to trip the normal char-timing activation before the
    /// newline lands. `grabbed` should be the current line text that
    /// was already inserted into the prompt before the Enter.
    pub(super) fn force_activate_with_newline(&mut self, grabbed: &str, now: Instant) {
        self.active = true;
        self.trusted_paste_stream = true;
        self.consecutive_fast = BURST_ACTIVATION_COUNT;
        self.candidate_chars = 0;
        self.last_event_at = Some(now);
        self.adoption_attempted = false;
        self.image_path_probe_attempted = false;
        self.buffer.clear();
        self.prepend_retro(grabbed);
        self.buffer.push('\n');
    }

    /// Unconditionally append `\n` to the buffer of an active burst.
    /// Used by `drain_burst_queue` when an Enter event is encountered
    /// inside the tight inner drain — every event in that loop arrived
    /// as part of the same crossterm batch, so the time-based
    /// `on_enter` check (which would mis-classify a pasted `\n` arriving
    /// just past `BURST_CHAR_INTERVAL` as a real Submit and tear down
    /// the burst mid-paste) is the wrong gate. The drain itself is the
    /// authoritative "this is part of a paste" signal.
    pub(super) fn append_newline_to_active(&mut self, now: Instant) {
        if !self.active {
            self.active = true;
            self.consecutive_fast = BURST_ACTIVATION_COUNT;
        }
        self.trusted_paste_stream = true;
        self.candidate_chars = 0;
        self.last_event_at = Some(now);
        self.buffer.push('\n');
    }

    /// Unconditionally append `\t` to the buffer of an active burst.
    /// This is the Tab counterpart to `append_newline_to_active`: pasted
    /// content that contains literal tabs (markdown checklist markers,
    /// indented code) would otherwise break the drain because the
    /// outer loop treats Tab as a focus-next / prompt-nav key.
    /// `is_paste_batch_key` already lists Tab as a valid paste
    /// constituent, but the drain previously stashed it — this closes
    /// the loop so Tab keeps paste latched instead of tearing it down.
    pub(super) fn append_tab_to_active(&mut self, now: Instant) {
        if !self.active {
            self.active = true;
            self.consecutive_fast = BURST_ACTIVATION_COUNT;
        }
        self.trusted_paste_stream = true;
        self.candidate_chars = 0;
        self.last_event_at = Some(now);
        self.buffer.push('\t');
    }

    fn is_idle_expired(&self, now: Instant) -> bool {
        self.last_event_at
            .is_some_and(|prev| now.duration_since(prev) >= BURST_IDLE_TIMEOUT)
    }

    /// Check if the burst has been idle longer than
    /// `BURST_IDLE_TIMEOUT` and, if so, return the accumulated text.
    pub(super) fn flush_if_idle(&mut self, now: Instant) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        if self.is_idle_expired(now) {
            self.active = false;
            self.trusted_paste_stream = false;
            self.consecutive_fast = 0;
            self.candidate_chars = 0;
            self.last_event_at = None;
            self.adoption_attempted = false;
            self.image_path_probe_attempted = false;
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }

    /// Unconditionally append a queued character to the active burst.
    /// Used by `drain_burst_queue` after the outer loop has already
    /// established that the queue is a paste stream. This intentionally
    /// includes non-ASCII text: ordinary IME commits never enter the
    /// drain, but Chinese/Japanese/Korean paste bodies must remain in
    /// the same buffer as their surrounding newlines.
    pub(super) fn append_char_to_active(&mut self, ch: char, now: Instant) {
        if !self.active {
            self.active = true;
            self.consecutive_fast = BURST_ACTIVATION_COUNT;
        }
        self.trusted_paste_stream = true;
        self.candidate_chars = 0;
        self.last_event_at = Some(now);
        self.buffer.push(ch);
    }

    /// Buffer a non-ASCII character that arrived inside a drained paste
    /// run, or refuse it.
    ///
    /// `false` means this is not a trusted paste stream — an IME commit
    /// — and the caller must hand the character back to the outer loop
    /// so [`Self::on_char`] can flush the pending ASCII run and let the
    /// character pass through to the prompt. When it returns `true` the
    /// character has been buffered on exactly the terms `on_char` would
    /// have used, which is what lets the drain keep going instead of
    /// paying an outer-loop round trip per CJK character in a paste.
    pub(super) fn append_non_ascii_to_active(&mut self, ch: char, now: Instant) -> bool {
        if !self.active || !self.trusted_paste_stream {
            return false;
        }
        self.buffer.push(ch);
        self.last_event_at = Some(now);
        true
    }

    /// Force-flush the buffer regardless of timing. Called before
    /// any non-char / non-Enter input so cursor moves and edits
    /// never land on a half-buffered burst.
    pub(super) fn force_flush(&mut self) -> Option<String> {
        self.active = false;
        self.trusted_paste_stream = false;
        self.consecutive_fast = 0;
        self.candidate_chars = 0;
        self.last_event_at = None;
        self.adoption_attempted = false;
        self.image_path_probe_attempted = false;
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }
}

/// Extract a single plain char from a `TextEdit` produced by
/// `translate_key`. Returns `None` for non-char edits (backspace,
/// delete, shift+enter) so they take the "force flush then apply"
/// path instead of feeding into the burst detector.
pub(super) fn plain_char_from_edit(edit: &TextEdit) -> Option<char> {
    if edit.event_input.backspace || edit.event_input.delete || edit.event_input.return_key {
        return None;
    }
    let mut chars = edit.event_input.ch.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(first)
}

/// Apply a burst-buffer flush as a paste. Wraps `apply_paste_to_app`
/// with the `is_pasting` flag so the `rebon_tui::input::paste_gate`
/// swallows any stray Enter that happens to land during the flush.
pub(super) fn flush_burst_as_paste(app: &mut AppState, text: String, guard: &mut TerminalGuard) {
    let rows = guard
        .terminal()
        .size()
        .map(|s| s.height as i32)
        .unwrap_or(24);
    app.is_pasting = true;
    apply_paste_to_app(app, text, rows);
    app.is_pasting = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_ascii_is_buffered_only_inside_a_trusted_paste_stream() {
        let now = Instant::now();

        let mut idle = PasteBurst::new();
        assert!(
            !idle.append_non_ascii_to_active('中', now),
            "nothing is running, so this is an IME commit"
        );

        let mut trusted = PasteBurst::new();
        trusted.append_char_to_active('a', now);
        assert!(trusted.append_non_ascii_to_active('中', now));
        assert_eq!(trusted.pending_text(), "a中");
    }

    use tempfile::TempDir;

    #[test]
    fn paste_candidate_prefix_uses_pending_chars_before_cursor() {
        let mut app = AppState::default();
        app.input = "left你Ctail".into();
        app.cursor_offset = "left你C".len();

        assert_eq!(paste_candidate_prefix_at_cursor(&app, 2, ':', '\\'), "C:\\");
    }

    #[test]
    fn image_path_probe_resets_for_new_candidate_and_after_flush() {
        let mut burst = PasteBurst::new();
        let t0 = Instant::now();
        assert!(matches!(burst.on_char('C', t0), CharOutcome::PassThrough));
        burst.mark_image_path_probe_attempted();
        assert!(burst.image_path_probe_attempted());

        assert!(matches!(
            burst.on_char(
                ':',
                t0 + BURST_CONTINUATION_INTERVAL + Duration::from_millis(1)
            ),
            CharOutcome::PassThrough
        ));
        assert!(!burst.image_path_probe_attempted());

        burst.mark_image_path_probe_attempted();
        burst.batch_activate_with_char('x', t0);
        assert_eq!(burst.force_flush().as_deref(), Some("x"));
        assert!(!burst.image_path_probe_attempted());
    }

    /// When the cursor sits at the end of the prompt, retro-grab lifts
    /// the trailing N chars.
    #[test]
    fn retro_grab_at_cursor_with_cursor_at_end_lifts_trailing_chars() {
        let mut app = AppState::default();
        app.input = "abcde".into();
        app.cursor_offset = 5;
        let grabbed = retro_grab_at_cursor(&mut app, 3);
        assert_eq!(grabbed, "cde");
        assert_eq!(app.input, "ab");
        assert_eq!(app.cursor_offset, 2);
    }

    /// Regression: when the cursor is mid-prompt, the lift must come from
    /// `[cursor-N..cursor]`, not from the prompt tail.
    #[test]
    fn retro_grab_at_cursor_with_cursor_in_middle_preserves_tail() {
        let mut app = AppState::default();
        app.input = "abcXYZdef".into();
        app.cursor_offset = 6;
        let grabbed = retro_grab_at_cursor(&mut app, 3);
        assert_eq!(
            grabbed, "XYZ",
            "grabbed chars must come from before the cursor"
        );
        assert_eq!(
            app.input, "abcdef",
            "tail content after the cursor must be preserved"
        );
        assert_eq!(
            app.cursor_offset, 3,
            "cursor moves back to the lift-out point"
        );
    }

    /// Retro-grab must walk UTF-8 char boundaries.
    #[test]
    fn retro_grab_at_cursor_handles_multibyte_chars() {
        let mut app = AppState::default();
        app.input = "你好world".into();
        app.cursor_offset = 6;
        let grabbed = retro_grab_at_cursor(&mut app, 2);
        assert_eq!(grabbed, "你好");
        assert_eq!(app.input, "world");
        assert_eq!(app.cursor_offset, 0);
    }

    #[test]
    fn retro_grab_at_cursor_snaps_invalid_utf8_cursor() {
        let mut app = AppState::default();
        app.input = "你abc".into();
        app.cursor_offset = 2;

        let grabbed = retro_grab_at_cursor(&mut app, 1);

        assert_eq!(grabbed, "");
        assert_eq!(app.input, "你abc");
        assert_eq!(app.cursor_offset, 0);
    }

    /// Retro count larger than the chars before the cursor clamps to
    /// the cursor without underflowing into the tail.
    #[test]
    fn retro_grab_at_cursor_clamps_when_count_exceeds_prefix() {
        let mut app = AppState::default();
        app.input = "abXYZdef".into();
        app.cursor_offset = 5;
        let grabbed = retro_grab_at_cursor(&mut app, 10);
        assert_eq!(grabbed, "abXYZ");
        assert_eq!(app.input, "def");
        assert_eq!(app.cursor_offset, 0);
    }

    // ── apply_paste_to_app tests ──────────────────────────────────

    #[test]
    fn apply_paste_to_app_short_text_splices_inline() {
        let mut app = AppState::default();
        app.input = "hello ".into();
        app.cursor_offset = 6;
        apply_paste_to_app(&mut app, "world".into(), 30);
        assert_eq!(app.input, "hello world");
        assert_eq!(app.cursor_offset, 11);
        assert!(app.pasted_contents.is_empty());
        assert_eq!(app.next_paste_id, 1);
    }

    #[test]
    fn apply_paste_to_app_multiline_collapses_to_reference_chip() {
        let mut app = AppState::default();
        let task_list = "- [x] first\n- [x] second\n- [x] third\n- [x] fourth\n";
        apply_paste_to_app(&mut app, task_list.into(), 30);
        assert_eq!(app.input, "[Pasted text #1 +4 lines]");
        assert_eq!(app.cursor_offset, "[Pasted text #1 +4 lines]".len());
        assert_eq!(app.pasted_contents.len(), 1);
        assert_eq!(app.pasted_contents[0].id, 1);
        assert_eq!(app.pasted_contents[0].content, task_list);
        assert_eq!(app.next_paste_id, 2);
    }

    #[test]
    fn apply_paste_to_app_bang_prefix_switches_mode_when_empty() {
        let mut app = AppState::default();
        app.mode = "prompt".into();
        apply_paste_to_app(&mut app, "!echo hi".into(), 30);
        assert_eq!(app.input, "echo hi");
        assert_eq!(app.mode, "bash");
    }

    #[test]
    fn apply_paste_to_app_multiline_bang_prefix_stays_prompt_mode() {
        let mut app = AppState::default();
        app.mode = "prompt".into();
        apply_paste_to_app(&mut app, "!echo hi\nnext".into(), 30);
        assert_eq!(app.input, "!echo hi\nnext");
        assert_eq!(app.mode, "prompt");
        assert!(app.pasted_contents.is_empty());
    }

    #[test]
    fn apply_paste_to_app_appends_to_existing_pasted_contents_store() {
        let mut app = AppState::default();
        app.pasted_contents.push(rebon_types::PromptPasteContent {
            id: 5,
            kind: "text".into(),
            content: "earlier".into(),
            media_type: None,
            filename: None,
            source_path: None,
        });
        app.next_paste_id = 6;
        let task_list = "a\nb\nc\nd\ne\n";
        apply_paste_to_app(&mut app, task_list.into(), 30);
        assert_eq!(app.pasted_contents.len(), 2);
        assert_eq!(app.pasted_contents[0].id, 5);
        assert_eq!(app.pasted_contents[1].id, 6);
        assert_eq!(app.next_paste_id, 7);
        assert!(app.input.contains("[Pasted text #6 +5 lines]"));
    }

    #[test]
    fn apply_paste_to_app_image_path_creates_image_chip() {
        let temp = TempDir::new().expect("temp dir");
        let image_path = temp.path().join("clip.png");
        std::fs::write(
            &image_path,
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89",
        )
        .expect("write png");

        let mut app = AppState::default();
        apply_paste_to_app(&mut app, image_path.to_string_lossy().to_string(), 30);

        assert_eq!(app.input, "[Image #1]");
        assert_eq!(app.cursor_offset, "[Image #1]".len());
        assert_eq!(app.next_paste_id, 2);
        assert_eq!(app.pasted_contents.len(), 1);
        assert_eq!(app.pasted_contents[0].kind, "image");
        assert_eq!(
            app.pasted_contents[0].media_type.as_deref(),
            Some("image/png")
        );
        assert_eq!(
            app.pasted_contents[0].source_path.as_deref(),
            Some(image_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn apply_image_paste_snaps_cursor_to_utf8_boundary() {
        let mut app = AppState::default();
        app.input = "你abc".into();
        app.cursor_offset = 2;

        apply_image_paste_to_app(
            &mut app,
            crate::tui::clipboard_image::ClipboardImage {
                data: "image-data".into(),
                media_type: "image/png".into(),
                filename: Some("clip.png".into()),
                source_path: None,
            },
        );

        assert_eq!(app.input, "[Image #1]你abc");
        assert_eq!(app.cursor_offset, "[Image #1]".len());
    }

    // ── PasteBurst state machine ──────────────────────────────────

    #[test]
    fn paste_burst_single_char_passes_through() {
        let mut burst = PasteBurst::new();
        let now = Instant::now();
        assert!(matches!(burst.on_char('a', now), CharOutcome::PassThrough));
        assert!(!burst.has_pending());
    }

    #[test]
    fn paste_burst_activates_on_fourth_fast_char() {
        let mut burst = PasteBurst::new();
        let t0 = Instant::now();
        assert!(matches!(burst.on_char('a', t0), CharOutcome::PassThrough));
        let t1 = t0 + Duration::from_millis(1);
        assert!(matches!(burst.on_char('b', t1), CharOutcome::PassThrough));
        let t2 = t1 + Duration::from_millis(1);
        assert!(matches!(burst.on_char('c', t2), CharOutcome::PassThrough));
        let t3 = t2 + Duration::from_millis(1);
        assert!(matches!(
            burst.on_char('d', t3),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        ));
        assert!(burst.has_pending());
    }

    #[test]
    fn paste_burst_enter_absorbed_during_active_burst() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        ));
        let enter_t = t + Duration::from_millis(4);
        assert!(matches!(burst.on_enter(enter_t), EnterOutcome::Buffered));
    }

    #[test]
    fn paste_burst_enter_submits_when_not_active() {
        let mut burst = PasteBurst::new();
        let now = Instant::now();
        assert!(matches!(burst.on_enter(now), EnterOutcome::Submit));
    }

    #[test]
    fn paste_burst_flushes_on_idle() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        )); // activates
        assert!(matches!(
            burst.on_char('e', t + Duration::from_millis(4)),
            CharOutcome::Buffered
        )); // buffers
        assert!(burst.flush_if_idle(t + Duration::from_millis(10)).is_none());
        let idle_t = t + BURST_IDLE_TIMEOUT + Duration::from_millis(100);
        let flushed = burst.flush_if_idle(idle_t);
        assert!(flushed.is_some());
        assert_eq!(flushed.unwrap(), "de");
    }

    #[test]
    fn paste_burst_force_flush_returns_buffer() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('x', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('y', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('z', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('w', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        ));
        let flushed = burst.force_flush();
        assert_eq!(flushed.as_deref(), Some("w"));
        assert!(!burst.has_pending());
    }

    #[test]
    fn paste_burst_idle_flush_resets_adoption_attempt() {
        let mut burst = PasteBurst::new();
        let now = Instant::now();
        burst.batch_activate_with_char('x', now);
        burst.mark_adoption_attempted();

        assert!(burst.adoption_attempted());
        assert_eq!(
            burst.flush_if_idle(now + BURST_IDLE_TIMEOUT),
            Some("x".to_string())
        );
        assert!(!burst.adoption_attempted());
    }

    #[test]
    fn paste_burst_force_flush_resets_adoption_attempt() {
        let mut burst = PasteBurst::new();
        burst.batch_activate_with_char('x', Instant::now());
        burst.mark_adoption_attempted();

        assert_eq!(burst.force_flush(), Some("x".to_string()));
        assert!(!burst.adoption_attempted());
    }

    #[test]
    fn paste_burst_non_ascii_passthrough_resets_adoption_attempt() {
        let mut burst = PasteBurst::new();
        let now = Instant::now();
        let _ = burst.on_char('a', now);
        let _ = burst.on_char('b', now + Duration::from_millis(1));
        let _ = burst.on_char('c', now + Duration::from_millis(2));
        assert!(matches!(
            burst.on_char('d', now + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { .. }
        ));
        burst.mark_adoption_attempted();

        assert!(matches!(
            burst.on_char('你', now + Duration::from_millis(4)),
            CharOutcome::FlushThenPassThrough(text) if text == "d"
        ));
        assert!(!burst.adoption_attempted());
    }

    #[test]
    fn paste_burst_pending_text_includes_retro_prefix() {
        let mut burst = PasteBurst::new();
        burst.batch_activate_with_char('d', Instant::now());
        burst.prepend_retro("abc");

        assert_eq!(burst.pending_text(), "abcd");
        assert_eq!(burst.pending_len(), 4);
    }

    #[test]
    fn paste_burst_slow_char_before_idle_stays_buffered() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        let _ = burst.on_char('a', t);
        let _ = burst.on_char('b', t + Duration::from_millis(1));
        let _ = burst.on_char('c', t + Duration::from_millis(2));
        let _ = burst.on_char('d', t + Duration::from_millis(3)); // activates, d buffered
        let slow_gap = BURST_CONTINUATION_INTERVAL + Duration::from_millis(10);
        assert!(slow_gap < BURST_IDLE_TIMEOUT);
        let slow = t + Duration::from_millis(3) + slow_gap;
        assert!(matches!(burst.on_char('x', slow), CharOutcome::Buffered));
    }

    #[test]
    fn paste_burst_cjk_ime_commit_never_activates() {
        // Regression for "输入法飘掉": a Pinyin/IME commit delivers
        // several CJK chars within ~1ms of each other. The detector
        // must pass them straight through instead of classifying
        // them as a paste — otherwise the chars are retro-grabbed
        // out of the prompt and reinserted after the idle timeout,
        // which is exactly the visual drift users saw.
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        for (i, ch) in "你好世界".chars().enumerate() {
            let tick = t + Duration::from_millis(i as u64);
            assert!(
                matches!(burst.on_char(ch, tick), CharOutcome::PassThrough),
                "CJK char {ch} must pass through, not activate the burst"
            );
        }
        assert!(!burst.has_pending());
    }

    #[test]
    fn paste_burst_cjk_after_active_ascii_flushes_pending_then_passes_through() {
        // If an ASCII paste has already activated the burst, an
        // incoming IME char must flush the accumulated buffer into
        // the prompt first and then insert itself normally. Without
        // the flush the buffer would linger while CJK chars typed
        // into the prompt, scrambling ordering.
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        )); // activates, d buffered
        match burst.on_char('你', t + Duration::from_millis(4)) {
            CharOutcome::FlushThenPassThrough(text) => assert_eq!(text, "d"),
            other => panic!("expected FlushThenPassThrough, got {other:?}"),
        }
        assert!(!burst.has_pending());
    }

    #[test]
    fn paste_burst_stays_active_across_continuation_window_jitter() {
        // Regression for "中间部分 [pasted xxx] 然后别的又原样": a
        // Windows conhost scheduler pause mid-paste used to
        // deactivate the burst because the inter-event gap
        // exceeded BURST_CHAR_INTERVAL (30ms). With the separate
        // continuation interval, a gap just under
        // BURST_CONTINUATION_INTERVAL must keep the burst active so
        // the pasted stream stays in one buffer and flushes as a
        // single paste instead of fragmenting into chip+raw+chip.
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        )); // activates, 'd' buffered
            // Gap well beyond the strict activation interval but still
            // inside the continuation interval — simulating a Windows
            // conhost batching stall mid-paste.
        let jitter_offset = BURST_CHAR_INTERVAL + Duration::from_millis(20);
        assert!(jitter_offset < BURST_CONTINUATION_INTERVAL);
        let jitter = t + Duration::from_millis(3) + jitter_offset;
        match burst.on_char('e', jitter) {
            CharOutcome::Buffered => {}
            other => panic!(
                "char after jitter gap must stay buffered, got {other:?} \
                 (gap={jitter_offset:?}, char={BURST_CHAR_INTERVAL:?}, \
                 cont={BURST_CONTINUATION_INTERVAL:?})"
            ),
        }
        assert!(burst.has_pending());
    }

    #[test]
    fn paste_burst_enter_at_paste_pace_stays_buffered() {
        // Pasted `\n` arrives glued to surrounding chars at paste pace
        // (≤ BURST_CHAR_INTERVAL). The active-latch must keep it in
        // the buffer so a multi-line paste flushes as a single chip
        // rather than fragmenting around every embedded newline.
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        )); // activates
            // `\n` arriving within the strict char interval — same batch
            // as the pasted chars, no scheduler stall.
        let paste_gap = BURST_CHAR_INTERVAL.saturating_sub(Duration::from_millis(2));
        let pasted_newline_at = t + Duration::from_millis(3) + paste_gap;
        match burst.on_enter(pasted_newline_at) {
            EnterOutcome::Buffered => {}
            other => panic!("pasted `\\n` at paste pace must stay buffered, got {other:?}"),
        }
    }

    /// Regression for "粘贴只剩最后一行": on Windows, conpty delivers
    /// non-bracketed pastes as batches separated by tens of ms of
    /// scheduler jitter, so every inter-batch newline in a multi-line
    /// paste exceeded the old strict 30 ms `BURST_CHAR_INTERVAL` gate
    /// and landed in the `FlushThenSubmit` arm. The runner then fired
    /// `apply_submit → clear_input`, and the user saw only the
    /// trailing line of the paste survive.
    ///
    /// New contract: once the burst is active, *all* Enter events stay
    /// in the buffer — the authoritative "paste is over" signal is
    /// `BURST_IDLE_TIMEOUT` firing in `flush_if_idle`. A user Enter
    /// pressed within that window joins the buffer; the idle flush
    /// then produces a paste chip with a trailing `\n`, and the user
    /// presses Enter again to submit. Minor double-press, but no
    /// content loss.
    #[test]
    fn paste_burst_enter_during_active_burst_always_buffers() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        )); // activates, buffer="d"
            // Enter with a gap well past `BURST_CHAR_INTERVAL` but still
            // inside the idle timeout — the conpty inter-batch jitter
            // scenario that used to mis-fire as Submit.
        let typing_gap = BURST_CHAR_INTERVAL + Duration::from_millis(20);
        assert!(typing_gap < BURST_IDLE_TIMEOUT);
        let enter_at = t + Duration::from_millis(3) + typing_gap;
        match burst.on_enter(enter_at) {
            EnterOutcome::Buffered => {}
            other => panic!(
                "Enter during active burst must Buffered, got {other:?} — \
                 the 30ms-gap escape caused the \"only last line survives\" \
                 regression on Windows conpty"
            ),
        }
        assert!(burst.has_pending(), "buffer must still hold the paste");
    }

    #[test]
    fn paste_burst_gap_beyond_continuation_window_stays_buffered_before_idle() {
        // Once a non-bracketed paste has activated, later chunks must
        // stay latched to the same burst until the idle timeout fires,
        // even if the gap between chunks is larger than the
        // pre-activation continuation window.
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        )); // activates, 'd' buffered
        let slow_gap = BURST_CONTINUATION_INTERVAL + Duration::from_millis(10);
        assert!(slow_gap < BURST_IDLE_TIMEOUT);
        let beyond = t + Duration::from_millis(3) + slow_gap;
        assert!(matches!(burst.on_char('x', beyond), CharOutcome::Buffered));
    }

    #[test]
    fn paste_burst_idle_timeout_is_at_least_continuation_interval() {
        // Invariant: the idle flush must wait at least as long as
        // the continuation interval, otherwise the idle path would
        // flush mid-paste during a jitter pause that on_char would
        // otherwise tolerate.
        assert!(BURST_IDLE_TIMEOUT >= BURST_CONTINUATION_INTERVAL);
    }

    #[test]
    fn paste_burst_idle_not_flushed_during_continuation_window() {
        // Concrete companion to the invariant above: while chars
        // are spaced within the continuation window, an idle check
        // that runs between them must NOT flush. This is the
        // simulated-main-loop scenario where a render frame sits
        // between two paste events.
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('d', t + Duration::from_millis(3)),
            CharOutcome::ActivatedRetro { retro_chars: 3 }
        )); // activates
        let mid_gap =
            t + Duration::from_millis(3) + BURST_CONTINUATION_INTERVAL - Duration::from_millis(5);
        assert!(burst.flush_if_idle(mid_gap).is_none());
        // Next paste char arriving in the continuation window
        // continues buffering.
        let next = mid_gap + Duration::from_millis(2);
        assert!(matches!(burst.on_char('e', next), CharOutcome::Buffered));
    }

    #[test]
    fn paste_burst_length_threshold_activates_without_fast_chars() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        let gap = BURST_CHAR_INTERVAL + Duration::from_millis(1);
        assert!(gap <= BURST_CONTINUATION_INTERVAL);
        let gap_ms = gap.as_millis() as u64;

        for idx in 0..(BURST_LENGTH_THRESHOLD - 1) {
            let tick = t + Duration::from_millis(gap_ms * idx as u64);
            assert!(matches!(burst.on_char('x', tick), CharOutcome::PassThrough));
        }

        let activate_tick = t + Duration::from_millis(gap_ms * (BURST_LENGTH_THRESHOLD as u64 - 1));
        assert!(matches!(
            burst.on_char('x', activate_tick),
            CharOutcome::ActivatedRetro { retro_chars } if retro_chars as usize == BURST_LENGTH_THRESHOLD - 1
        ));
        assert!(burst.has_pending());
    }

    #[test]
    fn paste_burst_length_threshold_buffers_followup_chars() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        let gap = BURST_CHAR_INTERVAL + Duration::from_millis(1);
        assert!(gap <= BURST_CONTINUATION_INTERVAL);
        let gap_ms = gap.as_millis() as u64;

        for idx in 0..BURST_LENGTH_THRESHOLD {
            let tick = t + Duration::from_millis(gap_ms * idx as u64);
            let outcome = burst.on_char('x', tick);
            if idx + 1 == BURST_LENGTH_THRESHOLD {
                assert!(matches!(
                    outcome,
                    CharOutcome::ActivatedRetro { retro_chars } if retro_chars as usize == BURST_LENGTH_THRESHOLD - 1
                ));
            } else {
                assert!(matches!(outcome, CharOutcome::PassThrough));
            }
        }

        let followup_tick = t + Duration::from_millis(gap_ms * BURST_LENGTH_THRESHOLD as u64);
        assert!(matches!(
            burst.on_char('y', followup_tick),
            CharOutcome::Buffered
        ));
    }

    #[test]
    fn paste_burst_ascii_after_cjk_does_not_inherit_fast_count() {
        // After an IME commit, the next ASCII char should start a
        // fresh burst — not activate immediately from a stale
        // consecutive-fast counter. Otherwise two fast ASCII chars
        // following a CJK string would trigger the burst and
        // retro-grab CJK bytes from the prompt.
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        let _ = burst.on_char('你', t);
        let _ = burst.on_char('好', t + Duration::from_millis(1));
        // Two ASCII chars landing immediately after: must NOT
        // activate the burst on their own (count resets to 1).
        assert!(matches!(
            burst.on_char('a', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(3)),
            CharOutcome::PassThrough
        ));
        assert!(!burst.has_pending());
    }

    /// Regression for "Enter 都无法发送消息出去了": typing a sentence
    /// at normal pace and pressing Enter must Submit, not activate a
    /// paste burst. Earlier `on_enter` logic bumped the
    /// `consecutive_fast` counter on the lenient 120 ms continuation
    /// window — any Enter pressed within 120 ms of the previous char
    /// trivially satisfied that, so normal typing was misclassified
    /// as a paste, the buffer latched, and the prompt never submitted.
    #[test]
    fn paste_burst_normal_typing_then_enter_submits() {
        let mut burst = PasteBurst::new();
        let mut now = Instant::now();
        // Realistic typing pace ~80 ms/char — well above the strict
        // 30 ms paste interval, so consecutive_fast should never get
        // past 1 on Windows or non-Windows.
        let typing_gap = Duration::from_millis(80);
        for ch in "hello".chars() {
            assert!(
                matches!(burst.on_char(ch, now), CharOutcome::PassThrough),
                "char {ch:?} during normal typing must pass through"
            );
            now += typing_gap;
        }
        // Enter pressed shortly after the last char (50 ms) — comfortably
        // inside the 120 ms continuation window but the strict
        // consecutive_fast counter is still 1, so activation must NOT
        // fire and Enter must Submit.
        let enter_at = now - typing_gap + Duration::from_millis(50);
        assert!(
            matches!(burst.on_enter(enter_at), EnterOutcome::Submit),
            "Enter after normal typing must Submit, not activate the burst"
        );
        assert!(!burst.has_pending(), "no buffer should remain after Submit");
    }

    /// Regression for "现在 enter 回车后甚至会导致输入的内容延迟显示":
    /// after Submit, the user starts typing the next message. Even at a
    /// gap only slightly above the strict paste threshold, chars must pass
    /// straight through to the prompt — never get diverted into the burst
    /// buffer.
    #[test]
    fn paste_burst_fast_typing_after_submit_never_buffers() {
        let mut burst = PasteBurst::new();
        let mut now = Instant::now();
        let typing_gap = BURST_CHAR_INTERVAL + Duration::from_millis(5);
        for ch in "hello world this is a normal sentence".chars() {
            let outcome = burst.on_char(ch, now);
            assert!(
                matches!(outcome, CharOutcome::PassThrough),
                "char {ch:?} during fast typing must PassThrough, \
                 got {outcome:?} — this is the post-Submit delay regression"
            );
            now += typing_gap;
        }
        assert!(
            !burst.has_pending(),
            "no chars should be latched in the burst buffer after fast typing"
        );
    }

    /// Regression for burst can be processed at paste-speed gaps after a render
    /// stall. Three chars plus Enter must still stay on the normal Submit
    /// path; real short-first-line pastes are recovered by the runner's
    /// queue-depth Enter detection.
    #[test]
    fn paste_burst_three_fast_chars_then_enter_still_submits() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        let enter_at = t + Duration::from_millis(3);
        assert!(matches!(burst.on_enter(enter_at), EnterOutcome::Submit));
        assert!(!burst.has_pending());
    }

    /// Regression for "甚至在句子中间回车的话会把后边的内容全删掉":
    /// the visible symptom (Enter mid-sentence eats the rest of the
    /// prompt) was driven by the same `on_enter` over-activation —
    /// the runner sees `EnterOutcome::ActivatedRetro` and truncates
    /// the prompt's tail to retro-grab chars into the burst buffer.
    /// At normal typing pace, Enter must never return ActivatedRetro,
    /// so the runner's truncate path is never reached and the prompt
    /// stays intact.
    #[test]
    fn paste_burst_normal_typing_then_enter_does_not_retro_grab() {
        let mut burst = PasteBurst::new();
        let mut now = Instant::now();
        // Slightly faster typing — still above the strict paste
        // threshold, so this is typing, not paste.
        let typing_gap = Duration::from_millis(50);
        for ch in "hello world".chars() {
            let _ = burst.on_char(ch, now);
            now += typing_gap;
        }
        let enter_at = now - typing_gap + Duration::from_millis(40);
        match burst.on_enter(enter_at) {
            EnterOutcome::Submit => {}
            EnterOutcome::ActivatedRetro { retro_chars } => panic!(
                "Enter mid-sentence at typing pace must NOT retro-grab: \
                 got ActivatedRetro {{ retro_chars: {retro_chars} }} — the \
                 runner would truncate that many chars off the prompt tail"
            ),
            EnterOutcome::Buffered => panic!("burst was never activated, Enter cannot be Buffered"),
        }
        assert!(!burst.has_pending());
    }

    /// The queue-depth batch activation path: when the outer loop sees
    /// a Press queued right behind the current char Press (typing
    /// doesn't produce that, paste does), it must flip the burst into
    /// active state immediately — even with no prior timing signal,
    /// and even if the detector's `last_event_at` has never been set
    /// (this is the very first char of the paste).
    #[test]
    fn paste_burst_batch_activate_with_char_latches_active_immediately() {
        let mut burst = PasteBurst::new();
        let now = Instant::now();
        burst.batch_activate_with_char('A', now);

        assert!(
            burst.has_pending(),
            "batch_activate_with_char must put char into the buffer so the \
             runner skips the TextEdit pass-through"
        );

        // Subsequent chars (arriving with typing-pace gaps, after the
        // runner's drain loop has processed the paste batch) must
        // remain buffered because the active-latch is set — otherwise
        // the paste fragments back into individual inserts.
        let next = now + Duration::from_millis(200);
        let outcome = burst.on_char('B', next);
        assert!(
            matches!(outcome, CharOutcome::Buffered),
            "after batch_activate_with_char, on_char must keep buffering \
             regardless of timing gap — got {outcome:?}"
        );
    }

    /// `consecutive_fast()` is the snapshot the runner uses *before*
    /// calling `on_enter` — when the queue-depth signal then forces an
    /// Enter-driven activation it needs to know how many trailing chars
    /// in the prompt belong to the paste stream and must be retro-lifted.
    /// `on_enter` resets the counter on its slow (Submit) path, so any
    /// post-on_enter inspection would always see 0 — the snapshot is the
    /// only way to recover that information.
    #[test]
    fn paste_burst_consecutive_fast_snapshot_survives_until_on_enter() {
        let mut burst = PasteBurst::new();
        let t = Instant::now();
        assert!(matches!(burst.on_char('a', t), CharOutcome::PassThrough));
        // Two fast follow-ups → consecutive_fast climbs to 3.
        assert!(matches!(
            burst.on_char('b', t + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert!(matches!(
            burst.on_char('c', t + Duration::from_millis(2)),
            CharOutcome::PassThrough
        ));
        // Snapshot just before on_enter.
        let snapshot = burst.consecutive_fast();
        assert!(
            snapshot >= 2,
            "snapshot must reflect prior fast chars, got {snapshot}"
        );
    }

    /// Force-activate via Enter from a queue-depth signal. Used by the
    /// runner when `EnterOutcome::Submit` happens but
    /// `detect_paste_batch` reveals a pasted-newline scenario. The
    /// burst must come out latched, with the grabbed prefix and the
    /// newline both in the buffer, so the eventual idle flush emits
    /// the full line + `\n` as a single paste.
    #[test]
    fn paste_burst_force_activate_with_newline_buffers_prefix_and_newline() {
        let mut burst = PasteBurst::new();
        let now = Instant::now();
        burst.force_activate_with_newline("a", now);
        assert!(burst.has_pending(), "must latch active");

        // A follow-up char arriving at typing pace must still buffer
        // because the active-latch is set.
        let next = now + Duration::from_millis(150);
        match burst.on_char('b', next) {
            CharOutcome::Buffered => {}
            other => panic!(
                "follow-up char after force_activate_with_newline must Buffer, \
                 got {other:?}"
            ),
        }

        // Idle flush should yield the full prefix + newline + follow-up.
        let idle = next + BURST_IDLE_TIMEOUT + Duration::from_millis(10);
        let flushed = burst
            .flush_if_idle(idle)
            .expect("idle flush must yield buffer");
        assert_eq!(
            flushed, "a\nb",
            "buffer must contain the grabbed prefix, the newline, and the \
             post-activation char"
        );
    }

    /// Empty-prefix variant: when the runner detects a paste batch on
    /// Enter but `consecutive_fast` was 0 (e.g. the very first event
    /// in the session is an Enter that turns out to lead a multi-line
    /// paste), the runner passes an empty grabbed string. The burst
    /// must still latch and emit just the `\n` plus follow-ups.
    #[test]
    fn paste_burst_force_activate_with_empty_prefix_starts_with_newline() {
        let mut burst = PasteBurst::new();
        let now = Instant::now();
        burst.force_activate_with_newline("", now);
        let next = now + Duration::from_millis(50);
        let _ = burst.on_char('x', next);
        let idle = next + BURST_IDLE_TIMEOUT + Duration::from_millis(10);
        let flushed = burst.flush_if_idle(idle).expect("idle flush");
        assert_eq!(flushed, "\nx");
    }
}
