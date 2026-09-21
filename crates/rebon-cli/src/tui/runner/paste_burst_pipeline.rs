//! Queue-detection layer over an [`EventSource`] that decides when a
//! flurry of inbound key/paste events should be treated as a single
//! paste batch. Sits on top of the existing [`super::paste_burst`]
//! buffer module: this submodule reads from the terminal queue and
//! shuttles characters into `PasteBurst`/`flush_burst_as_paste`, while
//! the buffer module owns the timing-based activation and chip
//! emission. Functions here are pure over `EventSource` so they unit-
//! test deterministically against the local `FakeEventSource` tests.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Context as _;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use rebon_tui::promptinput::paste_flow::{
    try_merge_into_prev_text_paste_diag, MergeRejectReason, TryMergePasteInput,
};

use crate::tui::app::AppState;
use crate::tui::terminal::TerminalGuard;

use super::paste_burst::{self, flush_burst_as_paste, BURST_CONTINUATION_INTERVAL};

/// Outcome of [`drain_burst_queue`] — how the drain terminated.
pub(super) enum DrainOutcome {
    /// Queue drained with no non-burst events encountered. Caller can
    /// return to the outer loop normally; the next iteration will land
    /// back at `event::poll(poll_timeout)`.
    Empty,
    /// A non-burst event (modifier key, Ctrl-combo, Mouse, Resize,
    /// bracketed Paste, non-ASCII commit, or a slow-gap Enter) was
    /// read during the drain. The event has been stashed into
    /// `stashed_events` so the outer loop picks it up on the next
    /// iteration.
    Stashed,
}

/// Tight inner drain that consumes queued key events directly into the
/// paste burst while the detector is active. Without this, a 1000-char
/// non-bracketed paste round-trips through the outer loop 1000 times —
/// and each iteration runs `drain_team_mailbox` (file IO),
/// `drain_ui_channels`, `sync_*`/`refresh_*` dialog passes, runtime-
/// state derivation, etc. That overhead is what made large pastes feel
/// seconds-slow on Windows even though the buffer logic itself is trivial.
///
/// This matches the scroll-event batch drain (see the `ScrollUp/Down`
/// handler below): a `while event::poll(Duration::ZERO)?` that pulls
/// every already-queued event in one go, handling only the ones that
/// are safe to process without re-entering the outer sync work.
///
/// Safety invariants:
/// - Called only when the burst is already active (`paste_burst.has_pending()`).
///   Modals cannot have become active mid-stream because `drain_ui_channels`
///   is skipped here — any pending engine event that might open a modal
///   waits until the next outer iteration.
/// - Non-ASCII chars (IME commits), chars with Ctrl/Alt modifiers, and
///   slow-gap Enters cause the drain to stash and return to the outer
///   loop so the full pipeline (dialog handlers, flush-then-passthrough,
///   flush-then-submit) runs correctly.
/// - Release events are silently skipped — they must not touch burst
///   state, matching the `KeyAction::Ignored` arm in the main match.
/// Abstraction over the crossterm event queue so the paste-detection
/// and drain logic is unit-testable without a real terminal. The runner
/// uses [`CrosstermSource`]; tests use a queue-backed fake that lets us
/// reproduce Windows ReadConsoleInput batching, multi-batch delivery,
/// and Press/Release pair patterns deterministically.
pub(super) trait EventSource {
    /// Returns true if at least one event is available within `timeout`.
    fn poll(&mut self, timeout: Duration) -> anyhow::Result<bool>;
    /// Reads the next available event. May block; only call after
    /// `poll` returned true (or with a timeout strategy that tolerates
    /// blocking — drain code always uses `Duration::ZERO`).
    fn read(&mut self) -> anyhow::Result<Event>;
    /// Bulk-transfer an already-queued pasted run into `out`, returning
    /// how many events were appended.
    ///
    /// This exists because reading a Windows paste one event at a time
    /// is dominated by console round trips — ~60 us per record, twice
    /// per pasted character, so a few hundred pasted lines freeze the
    /// prompt for a second or more. See
    /// [`super::console_input_batch`].
    ///
    /// Contract: only called with an empty `out` (so a batch can never
    /// overtake an already-stashed event), and returning `0` is always
    /// correct — the caller then falls back to `poll` + `read`. That is
    /// what the default does, and what every non-Windows build and every
    /// test fake gets, which keeps this a transport optimization rather
    /// than a second code path with its own semantics.
    fn refill_batch(&mut self, out: &mut VecDeque<Event>) -> anyhow::Result<usize> {
        let _ = out;
        Ok(0)
    }
}

/// Production wrapper that delegates straight to crossterm's global
/// event functions. Stateless; constructed at the top of `event_loop`
/// and passed by `&mut` everywhere a queue-aware function needs it.
pub(super) struct CrosstermSource;

pub(super) fn retry_interrupted_terminal_io<T>(
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    loop {
        match operation() {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn terminal_io<T>(
    phase: &'static str,
    operation: impl FnMut() -> io::Result<T>,
) -> anyhow::Result<T> {
    retry_interrupted_terminal_io(operation).context(phase)
}

pub(super) fn poll_crossterm_event(timeout: Duration) -> anyhow::Result<bool> {
    terminal_io("poll terminal input event", || event::poll(timeout))
}

pub(super) fn read_crossterm_event() -> anyhow::Result<Event> {
    terminal_io("read terminal input event", event::read)
}

impl EventSource for CrosstermSource {
    fn poll(&mut self, timeout: Duration) -> anyhow::Result<bool> {
        poll_crossterm_event(timeout)
    }
    fn read(&mut self) -> anyhow::Result<Event> {
        read_crossterm_event()
    }
    #[cfg(windows)]
    fn refill_batch(&mut self, out: &mut VecDeque<Event>) -> anyhow::Result<usize> {
        super::console_input_batch::refill_stashed_events(out)
            .context("batch-read queued console input")
    }
}

pub(super) fn drain_burst_queue<S: EventSource>(
    paste_burst: &mut paste_burst::PasteBurst,
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
) -> anyhow::Result<DrainOutcome> {
    drain_burst_queue_with_now(paste_burst, stashed_events, source, Instant::now)
}

fn drain_burst_queue_with_now<S: EventSource>(
    paste_burst: &mut paste_burst::PasteBurst,
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
    mut now: impl FnMut() -> Instant,
) -> anyhow::Result<DrainOutcome> {
    let mut probe = super::input_latency_probe::DrainProbe::start();
    // Every exit from the loop reports, so the counts always describe a
    // whole pass rather than whichever exit happened to be instrumented.
    macro_rules! finish {
        ($outcome:expr) => {{
            probe.finish(stashed_events.len());
            return Ok($outcome);
        }};
    }
    loop {
        if stashed_events.is_empty() {
            // Take the queued pasted run in one console read instead of
            // one round trip per record. Returns 0 for anything that
            // isn't an unambiguous paste, so the poll/read path below
            // still handles every case it handles today.
            probe.refills += 1;
            probe.refilled += source.refill_batch(stashed_events)?;
        }
        let evt = if let Some(stashed) = stashed_events.pop_front() {
            stashed
        } else {
            if !source.poll(Duration::ZERO)? {
                finish!(DrainOutcome::Empty);
            }
            probe.reads += 1;
            source.read()?
        };
        let Event::Key(key) = &evt else {
            stash_event_front(stashed_events, evt);
            finish!(DrainOutcome::Stashed);
        };
        // Release/Other — skip without disturbing burst timing.
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Char(ch) => {
                // Anything with Ctrl/Alt is not a paste char — bail out
                // and let the outer loop route it through translate_key
                // and the dialog handlers. Shift-only is fine (capital
                // letters in a paste are routine).
                let has_modifier = key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
                if has_modifier {
                    stash_event_front(stashed_events, evt);
                    finish!(DrainOutcome::Stashed);
                }
                // Non-ASCII splits two ways and only one of them can be
                // handled here. Inside a *trusted* paste stream a CJK
                // character is ordinary paste body and belongs in the
                // same buffer as the ASCII around it — which is exactly
                // what `on_char` does with it, so doing it here changes
                // nothing except that the drain keeps going. Everything
                // else is an IME commit: `on_char` flushes the pending
                // ASCII run via `FlushThenPassThrough`, and that flushed
                // text cannot be surfaced from `DrainOutcome::Stashed` —
                // it would be silently dropped, leaving only the most
                // recent CJK char in the prompt (the user-visible "只有
                // 最后一个字" regression). Hand those back untouched and
                // let the outer loop's TextEdit arm do the flush.
                //
                // The round trip is not free: a 1 019-line paste holding
                // ~1 100 CJK characters paid one outer-loop pass each,
                // and that was the last few hundred milliseconds of
                // frozen prompt once the console batching was fixed.
                if !ch.is_ascii() {
                    if !paste_burst.append_non_ascii_to_active(ch, now()) {
                        stash_event_front(stashed_events, evt);
                        finish!(DrainOutcome::Stashed);
                    }
                    probe.chars += 1;
                    continue;
                }
                probe.chars += 1;
                paste_burst.append_char_to_active(ch, now());
                continue;
            }
            KeyCode::Enter => {
                // Shift+Enter is a literal newline edit in the prompt,
                // not Submit, and the outer pipeline owns that decision
                // via translate_key. Stash so it routes correctly.
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    stash_event_front(stashed_events, evt);
                    finish!(DrainOutcome::Stashed);
                }
                // Inside an active drain, the Enter is unconditionally
                // a pasted newline — every event in this loop arrived
                // as part of the same crossterm batch. Bypass the time-
                // based on_enter check (which would mis-classify a
                // pasted `\n` arriving slightly outside the strict
                // BURST_CHAR_INTERVAL as a real Submit) and append the
                // newline directly to the buffer.
                probe.chars += 1;
                paste_burst.append_newline_to_active(now());
                continue;
            }
            KeyCode::Tab => {
                // Shift/Ctrl/Alt+Tab are navigation, not paste content.
                if key.modifiers.intersects(
                    KeyModifiers::SHIFT
                        | KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER,
                ) {
                    stash_event_front(stashed_events, evt);
                    finish!(DrainOutcome::Stashed);
                }
                // `is_paste_batch_key` already includes plain Tab as a
                // valid paste constituent (pasted markdown checklists,
                // indented code). Without this arm the drain would
                // immediately stash the Tab, force-flush the buffer,
                // and fire Tab as a focus/nav key into the prompt —
                // which tears a paste apart at the first tab. Append
                // `\t` straight into the buffer and keep draining.
                probe.chars += 1;
                paste_burst.append_tab_to_active(now());
                continue;
            }
            _ => {
                // Any other key (arrows, backspace, Esc, Home/End,
                // function keys) breaks the paste burst — force_flush
                // happens in the outer loop's generic `other` arm.
                stash_event_front(stashed_events, evt);
                finish!(DrainOutcome::Stashed);
            }
        }
    }
}

/// Swallow every already-queued key event that continues an armed paste
/// echo, in one pass.
///
/// After a clipboard adoption the chip is on screen immediately, but the
/// terminal still replays the whole paste — on Windows conhost that is a
/// flood of individual key events, two per pasted character. Matching one
/// per outer-loop iteration means the prompt stays unusable for the entire
/// replay even though the paste is visibly done, which is the "chip 出来了
/// 还要等剪贴板读完" freeze. Every event consumed here is already sitting
/// in the terminal queue, so the drain ends the moment the queue empties
/// and the outer loop resumes rendering.
///
/// Only the echo matcher decides what gets dropped
/// ([`PasteEchoSuppressor::consume_key_char`] swallows nothing that does
/// not continue the adopted text), so this is a batching change, not a
/// widening of what counts as echo. The first non-matching event is
/// stashed for the outer loop and ends the drain.
pub(super) fn drain_paste_echo_keys<S: EventSource>(
    app: &mut AppState,
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
) -> anyhow::Result<usize> {
    let mut swallowed = 0usize;
    while app.paste_echo.is_some() {
        if stashed_events.is_empty() {
            // Same reason as `drain_burst_queue`: the echo is the whole
            // paste replayed key by key, and reading it one console
            // record at a time is what keeps the prompt unusable after
            // the chip is already on screen.
            source.refill_batch(stashed_events)?;
        }
        let evt = if let Some(stashed) = stashed_events.pop_front() {
            stashed
        } else {
            if !source.poll(Duration::ZERO)? {
                break;
            }
            source.read()?
        };
        let Event::Key(key) = &evt else {
            stash_event_front(stashed_events, evt);
            break;
        };
        // Release/Repeat-less kinds carry no echo content; skipping them
        // mirrors the outer loop's own key routing.
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if super::paste_echo::paste_echo_key_hook(app, key)
            != super::paste_echo::KeyEchoHook::Swallow
        {
            // The hook already finalized the suppressor; the outer loop
            // processes this event as ordinary input.
            stash_event_front(stashed_events, evt);
            break;
        }
        swallowed += 1;
    }
    Ok(swallowed)
}

pub(super) fn is_paste_batch_key(key: &KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }
    match key.code {
        KeyCode::Char(_) => {
            let has_control_modifier = key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
            !has_control_modifier
        }
        KeyCode::Enter => {
            // Shift+Enter is a manual editing newline, not a paste event.
            !key.modifiers.contains(KeyModifiers::SHIFT)
        }
        KeyCode::Tab => {
            // Shift/Ctrl/Alt/Super+Tab are navigation, not paste content.
            !key.modifiers.intersects(
                KeyModifiers::SHIFT
                    | KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER,
            )
        }
        _ => false,
    }
}

pub(super) fn queued_paste_char(stashed_events: &VecDeque<Event>) -> Option<char> {
    let Event::Key(key) = stashed_events.front()? else {
        return None;
    };
    plain_key_as_paste_char(key)
}

/// Queue-depth peek: is the terminal actively streaming a non-bracketed
/// paste batch right now?
///
/// Rationale: the `PasteBurst` timing-based activation path needs three
/// consecutive char events within ~10 ms of each other on Windows. That
/// never happens in practice, because the outer event loop runs a pile
/// of per-iteration work between chars — `drain_ui_channels`,
/// `drain_team_mailbox` (file IO when a team is configured), dialog
/// `sync_*` / `refresh_*` passes, runtime-state derivation, etc. The
/// inter-char gap the detector measures is "terminal delivery gap +
/// outer-loop work", which typically lands at 15–20 ms — well beyond
/// the 10 ms activation window. As a result the burst never activates
/// during real pastes, every pasted char falls through as a plain
/// `TextEdit`, and the user sees the paste being "typed in" one char
/// at a time instead of converted to a `[pasted …]` chip.
///
/// Fix: use the crossterm input queue itself as the activation signal,
/// not wall-clock timing. On Windows, each keystroke produces a
/// `Press`+`Release` pair in the console input buffer. Typing has
/// human-reaction-time gaps between keystrokes, so after reading the
/// current `Press` and consuming its matching `Release`, the queue is
/// normally empty. A non-bracketed paste dumps the whole batch into
/// the console input buffer at once, so after consuming the current
/// Press+Release, the next Press is already queued.
///
/// Algorithm: caller has just read a `Press` char event. We peek the
/// next event: if it's a `Release`, consume it and re-check — another
/// queued event = paste. If the next event is already a `Press` (some
/// terminals don't emit Release), stash it for the next iteration and
/// report paste directly. Anything else (Mouse, Resize, bracketed
/// Paste, unrelated Key) — stash and report "not a batch".
///
/// Returns `true` when the queue-depth signal indicates a paste, in
/// which case the caller should call `PasteBurst::batch_activate_with_char`
/// and then `drain_burst_queue` instead of letting the char fall
/// through the normal path.
pub(super) fn detect_paste_batch<S: EventSource>(
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
) -> anyhow::Result<bool> {
    detect_paste_batch_with_timeout(stashed_events, source, Duration::ZERO, Duration::ZERO)
}

pub(super) const PASTE_BATCH_ENTER_GRACE: Duration = BURST_CONTINUATION_INTERVAL;

pub(super) fn detect_paste_batch_after_enter<S: EventSource>(
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
) -> anyhow::Result<bool> {
    // Enter-driven detection needs grace on BOTH polls: the first poll
    // may find the queue momentarily empty if the matching Release has
    // not yet crossed the conpty boundary, and the post-Release poll
    // needs to wait for the next batch's first Press. Without the
    // first-poll grace the Enter appeared "standalone" and the runner
    // submitted it — the failure mode for short-first-line pastes like
    // `"a\nbcdef\n…"` where the Enter arrives before the tail batch.
    detect_paste_batch_with_timeout(
        stashed_events,
        source,
        PASTE_BATCH_ENTER_GRACE,
        PASTE_BATCH_ENTER_GRACE,
    )
}

pub(super) const PASTE_BATCH_MODE_SHORTCUT_GRACE: Duration = BURST_CONTINUATION_INTERVAL;

pub(super) fn detect_paste_batch_after_mode_shortcut<S: EventSource>(
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
) -> anyhow::Result<bool> {
    detect_paste_batch_with_timeout(
        stashed_events,
        source,
        PASTE_BATCH_MODE_SHORTCUT_GRACE,
        PASTE_BATCH_MODE_SHORTCUT_GRACE,
    )
}

fn detect_paste_batch_with_timeout<S: EventSource>(
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
    first_timeout: Duration,
    after_release_timeout: Duration,
) -> anyhow::Result<bool> {
    if !stashed_events.is_empty() {
        return Ok(false);
    }
    if !source.poll(first_timeout)? {
        return Ok(false);
    }
    let next = source.read()?;
    match &next {
        Event::Key(k) if matches!(k.kind, KeyEventKind::Release) => {
            // Consumed the matching Release. For ordinary chars we
            // re-check immediately so typing stays latency-free. For
            // Enter, callers pass a small grace window because Windows
            // can deliver the next paste batch just after the Release;
            // missing that race submits the first pasted line.
            if !source.poll(after_release_timeout)? {
                return Ok(false);
            }
            let after_release = source.read()?;
            let is_batch = match &after_release {
                Event::Key(k) => is_paste_batch_key(k),
                _ => false,
            };
            stash_event_front(stashed_events, after_release);
            Ok(is_batch)
        }
        Event::Key(k) if is_paste_batch_key(k) => {
            // Another Press already queued with no Release in between —
            // this happens on terminals that don't emit Release events
            // (some Linux setups). Stronger paste signal: stash the
            // peeked event so the outer loop processes it next, and
            // report the batch.
            stash_event_front(stashed_events, next);
            Ok(true)
        }
        _ => {
            // Mouse, Resize, bracketed Paste, or non-key events are
            // not paste-batch signals — stash for the outer loop.
            stash_event_front(stashed_events, next);
            Ok(false)
        }
    }
}

/// Grace window used while coalescing adjacent `Event::Paste`
/// deliveries. Zero-poll would only catch chunks *already* sitting in
/// the queue; a small window catches chunks that conpty/Windows
/// Terminal deliver a few milliseconds apart because the console-input
/// pump hands them to crossterm in separate ticks. Stays well under
/// the BURST_IDLE_TIMEOUT so a user immediately typing after paste
/// isn't held up.
#[cfg(windows)]
const PASTE_COALESCE_GRACE: Duration = Duration::from_millis(80);
#[cfg(not(windows))]
const PASTE_COALESCE_GRACE: Duration = Duration::from_millis(10);

/// Grace window used for merging two consecutive paste flushes into a
/// single chip.
///
/// Non-bracketed paste on Windows can be split across two
/// `flush_burst_as_paste` calls when conpty batches the pasted events
/// with a gap larger than `BURST_IDLE_TIMEOUT` (150 ms) — the first
/// batch idle-flushes, the second one activates a fresh burst, and
/// the user sees one logical paste rendered as two adjacent chips
/// (`[Pasted text #1 +11 lines][Pasted text #2 +1 lines]`). Bracketed
/// paste has the same problem: conpty splits very large pastes into
/// multiple `Event::Paste(String)` deliveries, and when the transcript
/// is big enough that re-rendering between chunks exceeds this window
/// each chunk lands as its own chip (user reported 4+ chips from one
/// paste of ~160 lines).
///
/// `try_merge_into_prev_text_paste` already guards against merging a
/// deliberate second paste into the first: the merge only fires when
/// the prompt still ends with the previous chip's reference and the
/// cursor is at the very end — i.e. the user has not typed or moved
/// between flushes. With those guards the time gate can be generous.
/// 15 s covers slow-render cases (large transcripts, complex message
/// widgets, and terminal chunk delivery stalls) while still giving
/// the user a clear window to interact with the prompt between two
/// genuinely-separate paste operations.
const PASTE_CHIP_MERGE_GRACE: Duration = Duration::from_millis(15_000);

pub(super) fn summarize_paste_payload(text: &str) -> (usize, usize, String) {
    let len = text.len();
    let lines = text.lines().count();
    let tail: String = text
        .chars()
        .rev()
        .take(16)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let escaped_tail = tail.escape_debug().to_string();
    (len, lines, escaped_tail)
}

pub(super) fn summarize_prompt_tail(text: &str, max_chars: usize) -> String {
    text.chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>()
        .escape_debug()
        .to_string()
}

/// Flush the burst buffer, or merge it into the previous paste chip
/// if the prior flush landed within [`PASTE_CHIP_MERGE_GRACE`] and
/// the prompt still ends with that chip's reference.
///
/// The merge path exists to defeat conpty's tendency to split a
/// single pasted block across two batches whose delivery gap exceeds
/// [`BURST_IDLE_TIMEOUT`]. Without merging, each batch lands as its
/// own chip — see the rendering bug where a 12-line paste surfaced
/// as `[Pasted text #1 +11 lines][Pasted text #2 +1 lines]`. The
/// merge only fires when the user hasn't typed or cursor-moved
/// between the two flushes (enforced by
/// [`try_merge_into_prev_text_paste`] via the "prompt ends with the
/// chip ref" + "cursor at end" preconditions).
pub(super) fn flush_burst_with_merge(
    app: &mut AppState,
    text: String,
    guard: &mut TerminalGuard,
    last_flush_at: &mut Option<Instant>,
) {
    let now = Instant::now();
    let elapsed_since_prev = last_flush_at.map(|prev| now.duration_since(prev));
    let within_merge_grace = elapsed_since_prev
        .map(|d| d <= PASTE_CHIP_MERGE_GRACE)
        .unwrap_or(false);
    let (added_len, added_lines, added_tail) = summarize_paste_payload(&text);
    tracing::debug!(
        added_len,
        added_lines,
        added_tail = %added_tail,
        elapsed_ms = elapsed_since_prev.map(|d| d.as_millis()).unwrap_or(0),
        within_merge_grace,
        has_prior_flush = last_flush_at.is_some(),
        prompt_len = app.input.len(),
        prompt_tail = %summarize_prompt_tail(&app.input, 24),
        cursor_offset = app.cursor_offset,
        pasted_chip_count = app.pasted_contents.len(),
        "paste: flush_burst_with_merge start"
    );
    if crate::tui::clipboard_image::pasted_image_path_from_text(&text).is_some() {
        tracing::debug!(
            added_len,
            added_lines,
            added_tail = %added_tail,
            "paste: image path paste skips text-chip merge"
        );
        flush_burst_as_paste(app, text, guard);
        *last_flush_at = Some(now);
        return;
    }

    if within_merge_grace {
        match try_merge_into_prev_text_paste_diag(TryMergePasteInput {
            raw_text: &text,
            current_input: &app.input,
            cursor_offset: app.cursor_offset,
            pasted_contents: &app.pasted_contents,
        }) {
            Ok(merged) => {
                if merged.absorbed_stray_tail_bytes > 0 {
                    tracing::debug!(
                        chip_id = merged.updated_content_id,
                        new_content_len = merged.updated_content_text.len(),
                        added_len,
                        added_lines,
                        added_tail = %added_tail,
                        stray_tail_bytes = merged.absorbed_stray_tail_bytes,
                        elapsed_ms = elapsed_since_prev.map(|d| d.as_millis()).unwrap_or(0),
                        "paste: merge fallback absorbed non-ASCII stray tail into prev chip"
                    );
                } else {
                    tracing::debug!(
                        chip_id = merged.updated_content_id,
                        new_content_len = merged.updated_content_text.len(),
                        added_len,
                        added_lines,
                        added_tail = %added_tail,
                        elapsed_ms = elapsed_since_prev.map(|d| d.as_millis()).unwrap_or(0),
                        "paste: merged into prev chip (within chip-merge grace)"
                    );
                }
                app.is_pasting = true;
                app.input = merged.input;
                app.cursor_offset = merged.cursor_offset;
                if let Some(chip) = app
                    .pasted_contents
                    .iter_mut()
                    .find(|c| c.id == merged.updated_content_id)
                {
                    chip.content = merged.updated_content_text;
                }
                app.is_pasting = false;
                *last_flush_at = Some(now);
                return;
            }
            Err(reason) => match reason {
                MergeRejectReason::NoTextChip => {
                    tracing::debug!(
                        reason = reason.diagnostic_name(),
                        added_len,
                        added_lines,
                        added_tail = %added_tail,
                        elapsed_ms = elapsed_since_prev.map(|d| d.as_millis()).unwrap_or(0),
                        "paste: merge rejected"
                    );
                }
                MergeRejectReason::PromptTailMismatch {
                    ref expected_ref,
                    ref actual_tail,
                } => {
                    tracing::debug!(
                        reason = reason.diagnostic_name(),
                        added_len,
                        added_lines,
                        added_tail = %added_tail,
                        elapsed_ms = elapsed_since_prev.map(|d| d.as_millis()).unwrap_or(0),
                        expected_ref_len = expected_ref.len(),
                        expected_ref = %expected_ref.escape_debug(),
                        actual_tail_len = actual_tail.len(),
                        actual_tail = %actual_tail.escape_debug(),
                        input_len = app.input.len(),
                        cursor_at_end = app.cursor_offset == app.input.len(),
                        "paste: merge rejected"
                    );
                }
                MergeRejectReason::CursorNotAtEnd {
                    cursor_offset,
                    input_len,
                } => {
                    tracing::debug!(
                        reason = reason.diagnostic_name(),
                        added_len,
                        added_lines,
                        added_tail = %added_tail,
                        elapsed_ms = elapsed_since_prev.map(|d| d.as_millis()).unwrap_or(0),
                        cursor_offset,
                        input_len,
                        cursor_at_end = cursor_offset == input_len,
                        prompt_tail = %summarize_prompt_tail(&app.input, 24),
                        "paste: merge rejected"
                    );
                }
            },
        }
    } else {
        tracing::debug!(
            added_len,
            added_lines,
            added_tail = %added_tail,
            elapsed_ms = elapsed_since_prev.map(|d| d.as_millis()).unwrap_or(0),
            grace_ms = PASTE_CHIP_MERGE_GRACE.as_millis(),
            has_prior_flush = last_flush_at.is_some(),
            "paste: merge not attempted - outside chip-merge grace"
        );
    }
    tracing::debug!(
        added_len,
        added_lines,
        added_tail = %added_tail,
        "paste: flush falling back to new paste chip/application"
    );
    flush_burst_as_paste(app, text, guard);
    *last_flush_at = Some(now);
}

/// Coalesce adjacent `Event::Paste` payloads.
///
/// On Windows, conpty can split a single bracketed paste across
/// multiple `Event::Paste(String)` deliveries when the body is large
/// (the input pipeline chunks it as it pulls from the OS console
/// buffer). Each delivery is structurally a complete `\x1b[200~ … \x1b[201~`
/// payload from crossterm's perspective, but the user pressed paste
/// once and expects one chip.
///
/// Strategy: after consuming the first `Event::Paste`, drain any
/// queued events within `grace`. Concatenate further `Event::Paste`
/// bodies onto `accumulated`. Also absorb adjacent plain-character
/// `Event::Key(Char)` Press events — conpty's bracketed-paste framing
/// occasionally delivers individual paste-body characters as raw Key
/// events when the body contains non-ASCII (observed with em-dashes at
/// chunk boundaries), and treating them as paste continuation keeps a
/// single logical paste from splitting into multiple chips. Stash any
/// other event for the next outer-loop iteration so it isn't lost.
/// `grace == ZERO` collapses to the original "already-queued"
/// behaviour; a non-zero value lets coalescing wait a few milliseconds
/// for the next conpty chunk.
///
/// Returns the accumulated paste body. If a non-paste, non-Char event
/// appeared before any other paste, `accumulated` is unchanged and
/// that event is stashed.
pub(super) fn coalesce_adjacent_pastes<S: EventSource>(
    accumulated: &mut String,
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
) -> anyhow::Result<()> {
    coalesce_adjacent_pastes_with_grace(accumulated, stashed_events, source, PASTE_COALESCE_GRACE)
}

pub(super) fn coalesce_adjacent_pastes_with_grace<S: EventSource>(
    accumulated: &mut String,
    stashed_events: &mut VecDeque<Event>,
    source: &mut S,
    grace: Duration,
) -> anyhow::Result<()> {
    if !stashed_events.is_empty() {
        tracing::debug!(
            initial_len = accumulated.len(),
            initial_lines = accumulated.lines().count(),
            grace_ms = grace.as_millis(),
            "paste: coalesce skipped because a non-paste event is already stashed"
        );
        return Ok(());
    }
    let initial_len = accumulated.len();
    let initial_lines = accumulated.lines().count();
    let mut paste_chunks_absorbed = 0usize;
    let mut plain_key_chars_absorbed = 0usize;
    loop {
        if !source.poll(grace)? {
            tracing::debug!(
                initial_len,
                initial_lines,
                final_len = accumulated.len(),
                final_lines = accumulated.lines().count(),
                added_bytes = accumulated.len().saturating_sub(initial_len),
                paste_chunks_absorbed,
                plain_key_chars_absorbed,
                grace_ms = grace.as_millis(),
                "paste: coalesce finished after grace timeout"
            );
            return Ok(());
        }
        let next = source.read()?;
        match next {
            Event::Paste(more) => {
                let chunk_len = more.len();
                let chunk_lines = more.lines().count();
                paste_chunks_absorbed += 1;
                accumulated.push_str(&more);
                tracing::debug!(
                    chunk_len,
                    chunk_lines,
                    accumulated_len = accumulated.len(),
                    accumulated_lines = accumulated.lines().count(),
                    paste_chunks_absorbed,
                    grace_ms = grace.as_millis(),
                    "paste: coalesce absorbed adjacent Event::Paste chunk"
                );
                continue;
            }
            Event::Key(key) if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                tracing::debug!(
                    kind = ?key.kind,
                    grace_ms = grace.as_millis(),
                    "paste: coalesce skipped non-text Key event kind"
                );
                continue;
            }
            Event::Key(key) => {
                if let Some(c) = plain_key_as_paste_char(&key) {
                    if should_absorb_plain_key_during_paste_coalesce(accumulated, c, &key.modifiers)
                    {
                        plain_key_chars_absorbed += 1;
                        accumulated.push(c);
                        tracing::debug!(
                            absorbed_char = %c.escape_debug(),
                            accumulated_len = accumulated.len(),
                            accumulated_lines = accumulated.lines().count(),
                            plain_key_chars_absorbed,
                            grace_ms = grace.as_millis(),
                            "paste: coalesce absorbed plain Key event between paste chunks"
                        );
                        continue;
                    }

                    match probe_plain_key_run_until_next_paste(Event::Key(key), c, source)? {
                        PasteStragglerProbe::FollowedByPaste {
                            raw_text,
                            paste_text,
                        } => {
                            let raw_chars = raw_text.chars().count();
                            plain_key_chars_absorbed += raw_chars;
                            paste_chunks_absorbed += 1;
                            accumulated.push_str(&raw_text);
                            accumulated.push_str(&paste_text);
                            tracing::debug!(
                                raw_chars,
                                paste_len = paste_text.len(),
                                accumulated_len = accumulated.len(),
                                accumulated_lines = accumulated.lines().count(),
                                plain_key_chars_absorbed,
                                paste_chunks_absorbed,
                                straggler_grace_ms = PASTE_STRAGGLER_GRACE.as_millis(),
                                "paste: coalesce absorbed plain Key run before following paste chunk"
                            );
                            continue;
                        }
                        PasteStragglerProbe::NotPaste { replay } => {
                            tracing::debug!(
                                initial_len,
                                initial_lines,
                                final_len = accumulated.len(),
                                final_lines = accumulated.lines().count(),
                                added_bytes = accumulated.len().saturating_sub(initial_len),
                                paste_chunks_absorbed,
                                plain_key_chars_absorbed,
                                replay_events = replay.len(),
                                grace_ms = grace.as_millis(),
                                "paste: coalesce stopped and replaying plain Key run"
                            );
                            prepend_stashed_events(stashed_events, replay);
                            return Ok(());
                        }
                    }
                }

                tracing::debug!(
                    initial_len,
                    initial_lines,
                    final_len = accumulated.len(),
                    final_lines = accumulated.lines().count(),
                    added_bytes = accumulated.len().saturating_sub(initial_len),
                    paste_chunks_absorbed,
                    plain_key_chars_absorbed,
                    stashed_event = ?Event::Key(key),
                    grace_ms = grace.as_millis(),
                    "paste: coalesce stopped and stashed non-paste key event"
                );
                stash_event_front(stashed_events, Event::Key(key));
                return Ok(());
            }
            other => {
                tracing::debug!(
                    initial_len,
                    initial_lines,
                    final_len = accumulated.len(),
                    final_lines = accumulated.lines().count(),
                    added_bytes = accumulated.len().saturating_sub(initial_len),
                    paste_chunks_absorbed,
                    plain_key_chars_absorbed,
                    stashed_event = ?other,
                    grace_ms = grace.as_millis(),
                    "paste: coalesce stopped and stashed non-paste event"
                );
                stash_event_front(stashed_events, other);
                return Ok(());
            }
        }
    }
}

pub(super) const PASTE_STRAGGLER_GRACE: Duration = Duration::from_millis(25);
const MAX_PASTE_STRAGGLER_CHARS: usize = 128;

enum PasteStragglerProbe {
    FollowedByPaste {
        raw_text: String,
        paste_text: String,
    },
    NotPaste {
        replay: VecDeque<Event>,
    },
}

fn probe_plain_key_run_until_next_paste<S: EventSource>(
    first_event: Event,
    first_ch: char,
    source: &mut S,
) -> anyhow::Result<PasteStragglerProbe> {
    let mut raw_text = String::new();
    raw_text.push(first_ch);
    let mut replay = VecDeque::new();
    replay.push_back(first_event);
    let mut events_seen = 1usize;

    while raw_text.chars().count() < MAX_PASTE_STRAGGLER_CHARS
        && events_seen < MAX_PASTE_STRAGGLER_CHARS.saturating_mul(2)
    {
        if !source.poll(PASTE_STRAGGLER_GRACE)? {
            return Ok(PasteStragglerProbe::NotPaste { replay });
        }

        let next = source.read()?;
        events_seen += 1;
        match next {
            Event::Paste(paste_text) => {
                return Ok(PasteStragglerProbe::FollowedByPaste {
                    raw_text,
                    paste_text,
                });
            }
            Event::Key(key) if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                replay.push_back(Event::Key(key));
            }
            Event::Key(key) => {
                if let Some(ch) = plain_key_as_paste_char(&key) {
                    raw_text.push(ch);
                    replay.push_back(Event::Key(key));
                    continue;
                }
                replay.push_back(Event::Key(key));
                return Ok(PasteStragglerProbe::NotPaste { replay });
            }
            other => {
                replay.push_back(other);
                return Ok(PasteStragglerProbe::NotPaste { replay });
            }
        }
    }

    Ok(PasteStragglerProbe::NotPaste { replay })
}

fn plain_key_as_paste_char(key: &KeyEvent) -> Option<char> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

pub(super) fn stash_event_front(stashed_events: &mut VecDeque<Event>, event: Event) {
    stashed_events.push_front(event);
}

fn prepend_stashed_events(stashed_events: &mut VecDeque<Event>, mut events: VecDeque<Event>) {
    while let Some(event) = events.pop_back() {
        stashed_events.push_front(event);
    }
}

fn should_absorb_plain_key_during_paste_coalesce(
    accumulated: &str,
    c: char,
    modifiers: &KeyModifiers,
) -> bool {
    if !(modifiers.is_empty() || *modifiers == KeyModifiers::SHIFT) {
        return false;
    }
    if !c.is_ascii() {
        return true;
    }
    accumulated.contains('\n') || accumulated.contains('\r') || accumulated.len() > 1_000
}
#[cfg(test)]
mod tests {
    use super::super::paste_burst::{
        apply_paste_to_app, paste_candidate_prefix_at_cursor, CharOutcome, EnterOutcome,
        PasteBurst, BURST_BATCH_FAST_COUNT, BURST_CHAR_INTERVAL, BURST_CONTINUATION_INTERVAL,
        BURST_IDLE_TIMEOUT,
    };
    use super::super::paste_echo::{
        clipboard_image_path_matches_prefix_with, finalize_paste_echo,
        inline_adoption_repair_target, plan_clipboard_adoption, AdoptionPlan, KeyEcho,
        PasteEchoSuppressor, MIN_ADOPTION_PREFIX_BYTES,
    };
    use super::*;
    use rebon_tui::promptinput::paste_flow::normalize_pasted_text;
    use std::io;
    use std::time::{Duration, Instant};

    #[test]
    fn interrupted_terminal_io_retries_until_success() {
        let mut attempts = 0;
        let value = retry_interrupted_terminal_io(|| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::new(io::ErrorKind::Interrupted, "signal"))
            } else {
                Ok(42)
            }
        })
        .expect("interrupted operations should retry");

        assert_eq!(value, 42);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn terminal_io_does_not_retry_fatal_errors_and_preserves_phase() {
        for (kind, phase) in [
            (io::ErrorKind::BrokenPipe, "poll terminal input event"),
            (io::ErrorKind::PermissionDenied, "read terminal input event"),
        ] {
            let mut attempts = 0;
            let err = terminal_io::<()>(phase, || {
                attempts += 1;
                Err(io::Error::new(kind, "terminal unavailable"))
            })
            .expect_err("fatal terminal errors must be returned");

            assert_eq!(attempts, 1);
            assert!(format!("{err:#}").contains(phase));
            assert_eq!(
                err.downcast_ref::<io::Error>().map(io::Error::kind),
                Some(kind)
            );
        }
    }

    #[test]
    fn paste_simulation_delayed_continuation_before_idle_flush_does_not_submit_half_prompt() {
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, "intro line");
        let delayed_newline_at = (BURST_CONTINUATION_INTERVAL.as_millis() as u64).saturating_sub(1);
        push_paste_batch(
            &mut src,
            delayed_newline_at,
            "\ncontinuation line\nfinal line",
        );

        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "continuation arriving before the paste gate flushes must not submit a partial prompt; got {:?}",
            result.submits
        );
        assert_eq!(
            result.final_input,
            "intro line\ncontinuation line\nfinal line"
        );
    }

    #[test]
    fn paste_simulation_two_multiline_batches_flushes_one_paste_body_without_mid_submit() {
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, "alpha\nbeta");
        push_paste_batch(
            &mut src,
            PASTE_BATCH_ENTER_GRACE.as_millis() as u64 / 2,
            "\ngamma\ndelta",
        );

        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "multi-line paste split inside the enter grace must not submit a half prompt; got {:?}",
            result.submits
        );
        assert_eq!(result.final_input, "alpha\nbeta\ngamma\ndelta");
        assert_eq!(
            result.paste_flushes,
            vec!["alpha\nbeta\ngamma\ndelta".to_string()],
            "split paste should be represented as one flushed paste body"
        );
    }

    // ── Paste delivery simulation (EventSource fake) ─────────────────
    //
    // The crossterm event queue isn't directly testable, so the runner's
    // detect / drain code is parameterised over `EventSource`. These
    // tests use `FakeEventSource` to reproduce the exact event-streaming
    // patterns Windows ReadConsoleInput emits during real pastes. If the
    // simulated drain doesn't end with the full pasted text in the burst
    // buffer, the runner's behavior in production is wrong too.

    use ratatui::crossterm::event::{
        Event, KeyCode as CKeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        KeyModifiers as CKeyModifiers,
    };
    use std::collections::VecDeque;

    /// In-memory event source. Each entry is `(arrival_offset, event)`
    /// where the offset is wall-clock time relative to a `t0`. `poll`
    /// reports true once enough simulated time has elapsed for the
    /// front event to be available; `read` returns it. Calls to
    /// `advance_clock` step the virtual clock forward to mimic the
    /// outer loop's poll waits.
    struct FakeEventSource {
        events: VecDeque<(Duration, Event)>,
        clock: Duration,
        reads: usize,
    }

    impl FakeEventSource {
        fn new() -> Self {
            Self {
                events: VecDeque::new(),
                clock: Duration::ZERO,
                reads: 0,
            }
        }

        fn push(&mut self, offset_ms: u64, event: Event) {
            self.events
                .push_back((Duration::from_millis(offset_ms), event));
        }

        fn advance_clock(&mut self, by: Duration) {
            self.clock += by;
        }

        fn front_ready(&self) -> bool {
            self.events
                .front()
                .map(|(t, _)| *t <= self.clock)
                .unwrap_or(false)
        }
    }

    impl EventSource for FakeEventSource {
        fn poll(&mut self, timeout: Duration) -> anyhow::Result<bool> {
            if self.front_ready() {
                return Ok(true);
            }
            if let Some((t, _)) = self.events.front() {
                if *t <= self.clock + timeout {
                    self.clock = *t;
                    return Ok(true);
                }
            }
            self.clock += timeout;
            Ok(false)
        }
        fn read(&mut self) -> anyhow::Result<Event> {
            let (_, evt) = self
                .events
                .pop_front()
                .expect("EventSource::read called with empty queue");
            self.reads += 1;
            Ok(evt)
        }
    }

    fn key_event(code: CKeyCode, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: CKeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn coalesce_adjacent_pastes_combines_split_bracketed_chunks_and_non_ascii_key() {
        let mut accumulated = String::from("alpha");
        let mut stashed = VecDeque::new();
        let mut src = FakeEventSource::new();
        src.push(5, Event::Paste("beta".into()));
        src.push(
            10,
            Event::Key(KeyEvent {
                code: CKeyCode::Char('—'),
                modifiers: CKeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
        );
        src.push(15, Event::Paste("gamma".into()));
        src.push(20, press(CKeyCode::Esc));

        coalesce_adjacent_pastes_with_grace(
            &mut accumulated,
            &mut stashed,
            &mut src,
            Duration::from_millis(25),
        )
        .expect("coalesce succeeds");

        assert_eq!(accumulated, "alphabeta—gamma");
        assert!(matches!(
            stashed.front(),
            Some(Event::Key(KeyEvent {
                code: CKeyCode::Esc,
                ..
            }))
        ));
    }

    #[test]
    fn drain_paste_echo_keys_swallows_the_whole_queued_replay_in_one_pass() {
        // The adoption already put the chip on screen; conhost is still
        // replaying the paste as key events. One drain must consume the
        // whole queued replay instead of one event per outer-loop pass.
        let adopted = "alpha beta gamma";
        let mut app = AppState::default();
        app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
            adopted.to_string(),
            0,
            None,
            Instant::now(),
        ));
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, adopted);
        src.push(0, press(CKeyCode::Esc));

        let mut stashed = VecDeque::new();
        let swallowed =
            drain_paste_echo_keys(&mut app, &mut stashed, &mut src).expect("drain succeeds");

        assert_eq!(swallowed, adopted.chars().count());
        assert!(app.paste_echo.is_none(), "a full match disarms the echo");
        assert!(
            app.input.is_empty(),
            "echoed text never re-enters the prompt"
        );
        assert!(
            stashed.is_empty(),
            "the drain stops at the end of the echo, leaving later input queued"
        );
    }

    #[test]
    fn drain_paste_echo_keys_stops_at_real_input_and_stashes_it() {
        let mut app = AppState::default();
        app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
            "abcdef".to_string(),
            0,
            None,
            Instant::now(),
        ));
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, "abc");
        src.push(0, press(CKeyCode::Char('!')));

        let mut stashed = VecDeque::new();
        let swallowed =
            drain_paste_echo_keys(&mut app, &mut stashed, &mut src).expect("drain succeeds");

        assert_eq!(swallowed, 3);
        assert!(
            app.paste_echo.is_none(),
            "a mismatch finalizes the suppressor"
        );
        assert!(
            matches!(
                stashed.front(),
                Some(Event::Key(KeyEvent {
                    code: CKeyCode::Char('!'),
                    ..
                }))
            ),
            "the diverging key must reach the outer loop untouched: {stashed:?}"
        );
    }

    fn press(code: CKeyCode) -> Event {
        key_event(code, KeyEventKind::Press)
    }
    fn release(code: CKeyCode) -> Event {
        key_event(code, KeyEventKind::Release)
    }

    fn shifted_char(ch: char) -> Event {
        Event::Key(KeyEvent {
            code: CKeyCode::Char(ch),
            modifiers: CKeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    /// Push a Windows-style paste batch arriving atomically at
    /// `arrival_ms`. ReadConsoleInput delivers the entire batch into
    /// the console input buffer in one shot, so every event in the
    /// batch shares the same `not_before` time — `event::poll(0)`
    /// reports true for *all* of them as soon as the batch lands.
    /// (The earlier "1 ms per event" model was wrong — that's the
    /// rate at which our reader *consumes* events, not the rate at
    /// which they enter the queue.)
    fn push_paste_batch(src: &mut FakeEventSource, arrival_ms: u64, text: &str) -> u64 {
        for ch in text.chars() {
            let code = if ch == '\n' {
                CKeyCode::Enter
            } else {
                CKeyCode::Char(ch)
            };
            src.push(arrival_ms, press(code));
            src.push(arrival_ms, release(code));
        }
        arrival_ms
    }

    #[test]
    fn raw_image_path_batch_activates_before_prompt_text_and_becomes_chip() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let image_path = temp.path().join("clip.png");
        std::fs::write(
            &image_path,
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89",
        )
        .expect("write png");
        let clipboard = image_path.to_string_lossy().into_owned();
        assert!(
            clipboard.is_ascii(),
            "test path must exercise raw ASCII keys"
        );

        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, &clipboard);
        let Event::Key(first_key) = src.read().expect("first key") else {
            panic!("expected key event");
        };
        let CKeyCode::Char(first_ch) = first_key.code else {
            panic!("expected path char");
        };

        let mut app = AppState::default();
        let mut burst = PasteBurst::new();
        let mut stashed = VecDeque::new();
        let now = Instant::now();
        assert!(matches!(
            burst.on_char(first_ch, now),
            CharOutcome::PassThrough
        ));
        app.input.push(first_ch);
        app.cursor_offset = app.input.len();
        assert!(
            src.front_ready(),
            "queued release prevents an intermediate render"
        );
        assert!(matches!(
            src.read().expect("first release"),
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            })
        ));
        assert!(src.front_ready(), "second path key is already queued");
        let Event::Key(second_key) = src.read().expect("second key") else {
            panic!("expected second key event");
        };
        let CKeyCode::Char(second_ch) = second_key.code else {
            panic!("expected second path char");
        };
        assert!(matches!(
            burst.on_char(second_ch, now + Duration::from_millis(1)),
            CharOutcome::PassThrough
        ));
        assert_eq!(burst.candidate_chars(), 2);
        assert!(burst.consecutive_fast() < BURST_BATCH_FAST_COUNT);
        assert!(detect_paste_batch(&mut stashed, &mut src).expect("detect batch"));
        let queued_ch = queued_paste_char(&stashed).expect("queued path char");
        let prefix =
            paste_candidate_prefix_at_cursor(&app, burst.candidate_chars(), second_ch, queued_ch);
        burst.mark_image_path_probe_attempted();
        assert!(clipboard_image_path_matches_prefix_with(&prefix, || {
            Some(clipboard.clone())
        }));

        burst.batch_activate_with_char(second_ch, now);
        let grabbed = super::super::paste_burst::retro_grab_at_cursor(&mut app, 1);
        burst.prepend_retro(&grabbed);
        drain_burst_queue(&mut burst, &mut stashed, &mut src).expect("drain path");
        assert!(app.input.is_empty());
        assert_eq!(burst.pending_text(), clipboard);

        let flushed = burst.force_flush().expect("image path flush");
        apply_paste_to_app(&mut app, flushed, 30);
        assert_eq!(app.input, "[Image #1]");
        assert_eq!(app.pasted_contents.len(), 1);
        assert_eq!(app.pasted_contents[0].kind, "image");
    }

    #[test]
    fn a_cjk_body_inside_a_trusted_paste_run_stays_in_the_drain() {
        // Pasted Chinese used to be handed back one character at a time:
        // on a 1 019-line paste holding ~1 100 CJK characters that was
        // ~1 100 full outer-loop passes, and the last few hundred
        // milliseconds of frozen prompt.
        let mut burst = PasteBurst::new();
        let mut stashed = VecDeque::new();
        let mut src = FakeEventSource::new();
        burst.append_char_to_active('a', Instant::now());
        for ch in "中文内容".chars() {
            src.push(0, press(CKeyCode::Char(ch)));
        }
        src.push(0, press(CKeyCode::Esc));

        let outcome = drain_burst_queue(&mut burst, &mut stashed, &mut src).expect("drain");
        assert!(
            matches!(outcome, DrainOutcome::Stashed),
            "the Esc, not the CJK, is what ends the drain"
        );
        assert_eq!(burst.pending_text(), "a中文内容");
    }

    #[test]
    fn an_ime_commit_is_still_handed_back_to_the_outer_loop() {
        // No trusted paste stream behind it, so this is someone typing.
        // `on_char` has to flush any pending ASCII run before the
        // committed character lands in the prompt, and that flush cannot
        // be surfaced from `DrainOutcome::Stashed` — only the outer loop
        // can do it.
        let mut burst = PasteBurst::new();
        let mut stashed = VecDeque::new();
        let mut src = FakeEventSource::new();
        src.push(0, press(CKeyCode::Char('中')));

        let outcome = drain_burst_queue(&mut burst, &mut stashed, &mut src).expect("drain");
        assert!(matches!(outcome, DrainOutcome::Stashed));
        assert!(matches!(
            stashed.front(),
            Some(Event::Key(KeyEvent {
                code: CKeyCode::Char('中'),
                ..
            }))
        ));
        assert!(burst.pending_text().is_empty());
    }

    #[test]
    fn mode_shortcut_paste_detection_waits_for_delayed_tail_after_release() {
        let mut src = FakeEventSource::new();
        let mut stashed = VecDeque::new();
        src.push(0, release(CKeyCode::Char('!')));
        src.push(
            (PASTE_BATCH_MODE_SHORTCUT_GRACE.as_millis() as u64 / 2).max(1),
            press(CKeyCode::Char('e')),
        );

        assert!(detect_paste_batch_after_mode_shortcut(&mut stashed, &mut src).unwrap());
        assert!(matches!(
            stashed.front(),
            Some(Event::Key(KeyEvent {
                code: CKeyCode::Char('e'),
                kind: KeyEventKind::Press,
                ..
            }))
        ));
    }

    fn finalize_simulated_paste_echo(
        paste_echo: &mut Option<PasteEchoSuppressor>,
        active_adoption_flush: &mut Option<usize>,
        prompt_input: &mut String,
        paste_flushes: &mut [String],
        shortfall_repairs: &mut Vec<(usize, usize)>,
    ) {
        let Some(suppressor) = paste_echo.take() else {
            return;
        };
        let adoption_flush = active_adoption_flush.take();
        if suppressor.fully_matched() || !suppressor.repair_on_shortfall() {
            return;
        }

        let matched = suppressor.matched_len();
        let expected = suppressor.expected_norm().to_string();
        let expected_len = expected.len();
        let confirmed = expected[..matched].to_string();
        let mut app = AppState::default();
        app.input = std::mem::take(prompt_input);
        app.cursor_offset = app.input.len();
        let input_before_repair = app.input.clone();
        app.paste_echo = Some(suppressor);
        finalize_paste_echo(&mut app, "simulation");
        let repaired = app.input != input_before_repair;
        *prompt_input = app.input;

        if repaired {
            if let Some(index) = adoption_flush {
                paste_flushes[index] = confirmed;
            }
            shortfall_repairs.push((matched, expected_len));
        }
    }

    fn simulate_paste(src: &mut FakeEventSource) -> SimResult {
        simulate_paste_inner(src, None)
    }

    fn simulate_paste_with_clipboard(src: &mut FakeEventSource, clipboard: &str) -> SimResult {
        let clipboard = clipboard.to_string();
        let mut reader = || Some(clipboard.clone());
        simulate_paste_inner(src, Some(&mut reader))
    }

    fn simulate_paste_with_clipboard_reader(
        src: &mut FakeEventSource,
        reader: &mut dyn FnMut() -> Option<String>,
    ) -> SimResult {
        simulate_paste_inner(src, Some(reader))
    }

    /// Simulate the production runner's outer-loop pattern for a
    /// specific paste scenario. Returns the final flushed-paste text
    /// the user would see (after BURST_IDLE_TIMEOUT) plus a flag
    /// recording whether any mid-paste Submit fired. This matches what
    /// `event_loop` does in the relevant branches: read → on_char/on_enter
    /// → detect_paste_batch → batch_activate → drain → loop.
    fn simulate_paste_inner(
        src: &mut FakeEventSource,
        mut clipboard_reader: Option<&mut dyn FnMut() -> Option<String>>,
    ) -> SimResult {
        let mut burst = PasteBurst::new();
        let mut stashed = VecDeque::new();
        let mut submits: Vec<String> = Vec::new();
        let mut prompt_input = String::new();
        let mut paste_flushes: Vec<String> = Vec::new();
        let mut mode_shortcuts: Vec<char> = Vec::new();
        let mut paste_echo: Option<PasteEchoSuppressor> = None;
        let mut active_adoption_flush: Option<usize> = None;
        let mut clipboard_reads = 0usize;
        let mut adoption_read_counts = Vec::new();
        let mut shortfall_repairs = Vec::new();
        let started_at = Instant::now();
        // Cap iterations defensively so a regression doesn't infinite-loop
        // the test harness.
        for _ in 0..10_000 {
            let now = started_at + src.clock;
            // Idle flush: same as event_loop's top-of-loop.
            if let Some(flushed) = burst.flush_if_idle(now) {
                paste_flushes.push(flushed.clone());
                prompt_input.push_str(&flushed);
                continue;
            }
            if paste_echo
                .as_ref()
                .is_some_and(|suppressor| suppressor.is_idle_expired(now))
            {
                finalize_simulated_paste_echo(
                    &mut paste_echo,
                    &mut active_adoption_flush,
                    &mut prompt_input,
                    &mut paste_flushes,
                    &mut shortfall_repairs,
                );
            }

            if clipboard_reader.is_some()
                && burst.has_pending()
                && !burst.adoption_attempted()
                && burst.pending_len() >= MIN_ADOPTION_PREFIX_BYTES
                && paste_echo.is_none()
            {
                burst.mark_adoption_attempted();
                clipboard_reads += 1;
                let adoption = clipboard_reader
                    .as_deref_mut()
                    .and_then(|read_clipboard| read_clipboard())
                    .map(|clipboard_raw| {
                        let chunk_norm = normalize_pasted_text(burst.pending_text());
                        let clipboard_norm = normalize_pasted_text(&clipboard_raw);
                        plan_clipboard_adoption(&chunk_norm, clipboard_norm)
                    })
                    .unwrap_or(AdoptionPlan::None);
                match adoption {
                    AdoptionPlan::Complete => {
                        if stashed.is_empty() && !src.front_ready() {
                            if let Some(flushed) = burst.force_flush() {
                                adoption_read_counts.push(src.reads);
                                paste_flushes.push(flushed.clone());
                                prompt_input.push_str(&flushed);
                            }
                        }
                    }
                    AdoptionPlan::AdoptClipboard {
                        clipboard_norm,
                        matched,
                    } => {
                        let _ = burst.force_flush();
                        adoption_read_counts.push(src.reads);
                        let input_before = prompt_input.clone();
                        let cursor_before = prompt_input.len();
                        paste_flushes.push(clipboard_norm.clone());
                        prompt_input.push_str(&clipboard_norm);
                        let repair_target = inline_adoption_repair_target(
                            &input_before,
                            cursor_before,
                            &prompt_input,
                            prompt_input.len(),
                            &clipboard_norm,
                        );
                        active_adoption_flush = Some(paste_flushes.len() - 1);
                        paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
                            clipboard_norm,
                            matched,
                            repair_target,
                            now,
                        ));
                    }
                    AdoptionPlan::None => {}
                }
            }

            // Pull next event (either stashed or polled).
            let evt = if let Some(stashed_evt) = stashed.pop_front() {
                stashed_evt
            } else if src.poll(Duration::from_millis(50)).unwrap() {
                src.read().unwrap()
            } else if src.events.is_empty() && !burst.has_pending() && paste_echo.is_none() {
                break;
            } else {
                continue;
            };
            let Event::Key(key) = evt else { continue };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            let now = started_at + src.clock;

            if paste_echo.is_some() {
                let echo_char = match key.code {
                    CKeyCode::Char(ch)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                    {
                        Some(ch)
                    }
                    CKeyCode::Enter => Some('\r'),
                    CKeyCode::Tab if key.modifiers.is_empty() => Some('\t'),
                    _ => None,
                };
                if let Some(ch) = echo_char {
                    match paste_echo
                        .as_mut()
                        .expect("paste echo checked above")
                        .consume_key_char(ch, now)
                    {
                        KeyEcho::Swallowed => continue,
                        KeyEcho::Done => {
                            paste_echo = None;
                            active_adoption_flush = None;
                            continue;
                        }
                        KeyEcho::Mismatch => {
                            finalize_simulated_paste_echo(
                                &mut paste_echo,
                                &mut active_adoption_flush,
                                &mut prompt_input,
                                &mut paste_flushes,
                                &mut shortfall_repairs,
                            );
                        }
                    }
                } else {
                    finalize_simulated_paste_echo(
                        &mut paste_echo,
                        &mut active_adoption_flush,
                        &mut prompt_input,
                        &mut paste_flushes,
                        &mut shortfall_repairs,
                    );
                }
            }

            match key.code {
                CKeyCode::Char(ch) => {
                    if matches!(ch, '!' | '?')
                        && burst.has_pending()
                        && !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        )
                    {
                        burst.append_char_to_active(ch, now);
                        drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now).unwrap();
                        continue;
                    }
                    if matches!(ch, '!' | '?')
                        && prompt_input.is_empty()
                        && !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        )
                    {
                        if detect_paste_batch_after_mode_shortcut(&mut stashed, src).unwrap() {
                            burst.batch_activate_with_char(ch, now);
                            drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now)
                                .unwrap();
                        } else {
                            mode_shortcuts.push(ch);
                        }
                        continue;
                    }
                    match burst.on_char(ch, now) {
                        CharOutcome::PassThrough => {
                            let candidate_chars = burst.candidate_chars();
                            let consecutive_fast = burst.consecutive_fast();
                            if consecutive_fast >= BURST_BATCH_FAST_COUNT
                                && detect_paste_batch(&mut stashed, src).unwrap()
                            {
                                burst.batch_activate_with_char(ch, now);
                                let retro_chars = candidate_chars.saturating_sub(1);
                                if retro_chars > 0 {
                                    let lift_from =
                                        prompt_input.chars().count().saturating_sub(retro_chars);
                                    let grabbed: String =
                                        prompt_input.chars().skip(lift_from).collect();
                                    prompt_input = prompt_input.chars().take(lift_from).collect();
                                    burst.prepend_retro(&grabbed);
                                }
                                drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now)
                                    .unwrap();
                            } else {
                                prompt_input.push(ch);
                            }
                        }
                        CharOutcome::Buffered => {
                            drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now)
                                .unwrap();
                        }
                        CharOutcome::ActivatedRetro { retro_chars } => {
                            // Lift the trailing retro_chars from prompt_input.
                            let len = prompt_input.chars().count();
                            let lift_from = len.saturating_sub(retro_chars as usize);
                            let grabbed: String = prompt_input.chars().skip(lift_from).collect();
                            prompt_input = prompt_input.chars().take(lift_from).collect();
                            burst.prepend_retro(&grabbed);
                            drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now)
                                .unwrap();
                        }
                        CharOutcome::FlushThenPassThrough(flushed) => {
                            prompt_input.push_str(&flushed);
                            prompt_input.push(ch);
                        }
                    }
                }
                CKeyCode::Enter => {
                    let pre_enter_fast = burst.consecutive_fast();
                    let pre_enter_candidate = burst.candidate_chars();
                    match burst.on_enter(now) {
                        EnterOutcome::Submit => {
                            if detect_paste_batch_after_enter(&mut stashed, src).unwrap() {
                                let len = prompt_input.chars().count();
                                let retro_chars = pre_enter_candidate.max(pre_enter_fast as usize);
                                let lift_from = len.saturating_sub(retro_chars);
                                let grabbed: String =
                                    prompt_input.chars().skip(lift_from).collect();
                                prompt_input = prompt_input.chars().take(lift_from).collect();
                                burst.force_activate_with_newline(&grabbed, now);
                                drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now)
                                    .unwrap();
                            } else {
                                submits.push(std::mem::take(&mut prompt_input));
                            }
                        }
                        EnterOutcome::Buffered => {
                            drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now)
                                .unwrap();
                        }
                        EnterOutcome::ActivatedRetro { retro_chars } => {
                            let len = prompt_input.chars().count();
                            let lift_from = len.saturating_sub(retro_chars as usize);
                            let grabbed: String = prompt_input.chars().skip(lift_from).collect();
                            prompt_input = prompt_input.chars().take(lift_from).collect();
                            burst.prepend_retro(&grabbed);
                            drain_burst_queue_with_now(&mut burst, &mut stashed, src, || now)
                                .unwrap();
                        }
                    }
                }
                _ => {
                    if let Some(flushed) = burst.force_flush() {
                        prompt_input.push_str(&flushed);
                    }
                }
            }
        }
        // Final idle flush — at end of simulation, BURST_IDLE_TIMEOUT
        // would always have elapsed in real time.
        src.advance_clock(BURST_IDLE_TIMEOUT + Duration::from_millis(10));
        finalize_simulated_paste_echo(
            &mut paste_echo,
            &mut active_adoption_flush,
            &mut prompt_input,
            &mut paste_flushes,
            &mut shortfall_repairs,
        );
        if let Some(flushed) = burst.flush_if_idle(started_at + src.clock + BURST_IDLE_TIMEOUT) {
            paste_flushes.push(flushed.clone());
            prompt_input.push_str(&flushed);
        }
        SimResult {
            final_input: prompt_input,
            paste_flushes,
            submits,
            mode_shortcuts,
            clipboard_reads,
            adoption_read_counts,
            shortfall_repairs,
            source_reads: src.reads,
        }
    }

    #[derive(Debug)]
    struct SimResult {
        final_input: String,
        paste_flushes: Vec<String>,
        submits: Vec<String>,
        mode_shortcuts: Vec<char>,
        clipboard_reads: usize,
        adoption_read_counts: Vec<usize>,
        shortfall_repairs: Vec<(usize, usize)>,
        source_reads: usize,
    }

    #[test]
    fn raw_key_clipboard_adoption_flushes_before_tail_batch_is_consumed() {
        let prefix = "a".repeat(MIN_ADOPTION_PREFIX_BYTES + 64);
        let tail = format!("\n{}", "b".repeat(320));
        let clipboard = format!("{prefix}{tail}");
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, &prefix);
        push_paste_batch(&mut src, 80, &tail);

        let result = simulate_paste_with_clipboard(&mut src, &clipboard);

        assert_eq!(result.clipboard_reads, 1);
        assert_eq!(result.paste_flushes, vec![clipboard.clone()]);
        assert_eq!(result.final_input, clipboard);
        assert!(result.submits.is_empty());
        assert_eq!(result.adoption_read_counts.len(), 1);
        assert!(
            result.adoption_read_counts[0] < result.source_reads,
            "clipboard must be applied before the delayed tail is consumed"
        );
        assert!(result.shortfall_repairs.is_empty());
    }

    #[test]
    fn raw_key_clipboard_adoption_repairs_when_stream_ends_early() {
        let prefix = "a".repeat(MIN_ADOPTION_PREFIX_BYTES + 64);
        let tail = "b".repeat(120);
        let stream = format!("{prefix}{tail}");
        let clipboard = format!("{stream}{}", "c".repeat(80));
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, &prefix);
        push_paste_batch(&mut src, 80, &tail);

        let result = simulate_paste_with_clipboard(&mut src, &clipboard);

        assert_eq!(result.clipboard_reads, 1);
        assert_eq!(result.final_input, stream);
        assert_eq!(result.paste_flushes, vec![stream.clone()]);
        assert_eq!(
            result.shortfall_repairs,
            vec![(stream.len(), clipboard.len())]
        );
        assert!(result.submits.is_empty());
    }

    #[test]
    fn raw_key_clipboard_adoption_rebursts_divergent_tail() {
        let prefix = "a".repeat(MIN_ADOPTION_PREFIX_BYTES + 64);
        let expected_tail = "b".repeat(120);
        let divergent_tail = "z".repeat(120);
        let clipboard = format!("{prefix}{expected_tail}");
        let stream = format!("{prefix}{divergent_tail}");
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, &prefix);
        push_paste_batch(&mut src, 80, &divergent_tail);

        let result = simulate_paste_with_clipboard(&mut src, &clipboard);

        assert!(result.clipboard_reads <= 2);
        assert_eq!(result.final_input, stream);
        assert_eq!(
            result.paste_flushes,
            vec![prefix.clone(), divergent_tail.clone()]
        );
        assert_eq!(
            result.shortfall_repairs,
            vec![(prefix.len(), clipboard.len())]
        );
        assert!(result.submits.is_empty());
    }

    #[test]
    fn raw_key_clipboard_adoption_declines_retro_prefix_pollution() {
        let clipboard = "p".repeat(MIN_ADOPTION_PREFIX_BYTES + 64);
        let prefix_gap_ms = (BURST_CONTINUATION_INTERVAL / 2).as_millis() as u64;
        let mut src = FakeEventSource::new();
        src.push(0, press(CKeyCode::Char('x')));
        src.push(0, release(CKeyCode::Char('x')));
        src.push(prefix_gap_ms, press(CKeyCode::Char('y')));
        src.push(prefix_gap_ms, release(CKeyCode::Char('y')));
        push_paste_batch(&mut src, prefix_gap_ms + 1, &clipboard);

        let result = simulate_paste_with_clipboard(&mut src, &clipboard);
        let expected = format!("xy{clipboard}");

        assert_eq!(result.clipboard_reads, 1);
        assert_eq!(result.final_input, expected);
        assert_eq!(result.paste_flushes, vec![expected]);
        assert!(result.adoption_read_counts.is_empty());
        assert!(result.shortfall_repairs.is_empty());
        assert!(result.submits.is_empty());
    }

    #[test]
    fn raw_key_clipboard_adoption_does_not_read_clipboard_for_small_paste() {
        let text = "s".repeat(MIN_ADOPTION_PREFIX_BYTES - 1);
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, &text);
        let mut reader = || -> Option<String> {
            panic!("clipboard reader must not run below the adoption threshold")
        };

        let result = simulate_paste_with_clipboard_reader(&mut src, &mut reader);

        assert_eq!(result.clipboard_reads, 0);
        assert_eq!(result.final_input, text);
        assert_eq!(result.paste_flushes, vec![text]);
        assert!(result.submits.is_empty());
    }

    #[test]
    fn raw_key_complete_adoption_waits_when_an_event_is_stashed() {
        let clipboard = "a".repeat(MIN_ADOPTION_PREFIX_BYTES);
        let tail = "tail";
        let tail_offset_ms = (BURST_IDLE_TIMEOUT / 2).as_millis() as u64;
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, &clipboard);
        src.push(0, Event::FocusGained);
        push_paste_batch(&mut src, tail_offset_ms, tail);

        let result = simulate_paste_with_clipboard(&mut src, &clipboard);
        let expected = format!("{clipboard}{tail}");

        assert_eq!(result.clipboard_reads, 1);
        assert!(
            result.adoption_read_counts.is_empty(),
            "Complete must not flush while a looked-ahead event is stashed"
        );
        assert_eq!(result.final_input, expected);
        assert_eq!(result.paste_flushes, vec![expected]);
        assert!(result.submits.is_empty());
    }

    /// Single-batch paste: 200 chars across 5 lines all queued at t=0.
    /// Expectation: zero submits, full text in burst buffer (and thus
    /// in the final prompt input).
    #[test]
    fn paste_simulation_single_batch_keeps_all_lines_together() {
        let mut src = FakeEventSource::new();
        let lines = "line one of paste\nline two of paste\nline three of paste\nline four of paste\nline five of paste";
        push_paste_batch(&mut src, 0, lines);
        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "no Submit should fire mid-paste; got submits={:?}",
            result.submits
        );
        assert_eq!(
            result.final_input, lines,
            "all pasted text must end up in the prompt as one chunk"
        );
    }

    /// Multi-batch paste: first half at t=0, second half at t=80ms
    /// (well beyond BURST_CHAR_INTERVAL). This is the failure mode the
    /// user reports — each line printed separately. Expectation: still
    /// zero submits, both halves end up in the buffer because the
    /// queue-depth signal on the slow Enter / next char keeps the
    /// burst alive.
    #[test]
    fn paste_simulation_multi_batch_does_not_split_into_submits() {
        let mut src = FakeEventSource::new();
        let first = "first half line one\nfirst half line two";
        let second = "\nsecond half line three\nsecond half line four";
        let after_first = push_paste_batch(&mut src, 0, first);
        // Gap > BURST_CHAR_INTERVAL → on_enter / on_char would normally
        // tear down the burst. Queue-depth detection must save it.
        let second_start = after_first + 80;
        push_paste_batch(&mut src, second_start, second);
        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "multi-batch paste must not produce a mid-paste Submit; \
             got {:?}",
            result.submits
        );
        let combined = format!("{}{}", first, second);
        assert_eq!(
            result.final_input, combined,
            "all batches must coalesce into a single buffered paste"
        );
    }

    /// Three-batch paste with two large gaps — stress the queue-depth
    /// fallback by stacking multiple OS scheduler stalls in a row.
    #[test]
    fn paste_simulation_three_batches_with_large_gaps_stays_unified() {
        let mut src = FakeEventSource::new();
        let parts = ["alpha bravo\n", "charlie delta\n", "echo foxtrot golf"];
        let mut t = 0u64;
        for part in &parts {
            t = push_paste_batch(&mut src, t, part);
            t += 100; // 100ms gap between batches
        }
        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "three-batch paste must not fragment into submits; got {:?}",
            result.submits
        );
        let combined: String = parts.iter().copied().collect();
        assert_eq!(result.final_input, combined);
    }

    /// Short-first-line paste `"a\nbcdef\nghij"` — the original
    /// failure mode where the Enter immediately after a single char
    /// would land in `EnterOutcome::Submit` because consecutive_fast
    /// only reached 1. Queue-depth on Enter must catch this.
    #[test]
    fn paste_simulation_single_char_first_line_does_not_submit() {
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, "a\nbcdef\nghij");
        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "short-first-line paste must NOT submit the 'a'; got {:?}",
            result.submits
        );
        assert_eq!(result.final_input, "a\nbcdef\nghij");
    }

    /// Windows can deliver the next ReadConsoleInput batch tens of
    /// milliseconds after the Enter key's Release. A zero-duration
    /// queue peek misses that continuation and submits the first line
    /// (`"a"`) before the rest of the paste arrives. The Enter path
    /// must wait the same continuation window used by PasteBurst and
    /// fold the delayed tail into the same paste burst.
    #[test]
    fn paste_simulation_short_first_line_with_delayed_tail_does_not_submit() {
        let mut src = FakeEventSource::new();
        src.push(0, press(CKeyCode::Char('a')));
        src.push(0, release(CKeyCode::Char('a')));
        src.push(8, press(CKeyCode::Enter));
        src.push(8, release(CKeyCode::Enter));
        let delayed_tail_at = 8 + ((PASTE_BATCH_ENTER_GRACE.as_millis() as u64 * 2) / 3).max(1);
        push_paste_batch(&mut src, delayed_tail_at, "bcdef\nghij");

        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "delayed tail after pasted Enter must not submit first line; got {:?}",
            result.submits
        );
        assert_eq!(result.final_input, "a\nbcdef\nghij");
    }

    /// Regression for "paste loses content": when the first line's
    /// chars arrive slower than BURST_CHAR_INTERVAL but still within
    /// BURST_CONTINUATION_INTERVAL, the strict fast counter is only 1.
    /// Enter-driven activation must still retro-grab the whole
    /// candidate first line, otherwise the eventual paste chip stores
    /// only the suffix (for example `"c\nnext"` instead of
    /// `"abc\nnext"`).
    #[test]
    fn paste_simulation_slow_first_line_flushes_complete_paste_body() {
        let mut src = FakeEventSource::new();
        let slow_gap = BURST_CHAR_INTERVAL + Duration::from_millis(5);
        assert!(
            slow_gap <= BURST_CONTINUATION_INTERVAL,
            "test gap must reset consecutive_fast but preserve candidate_chars"
        );
        let gap_ms = slow_gap.as_millis() as u64;
        let mut t = 0u64;
        for ch in ['a', 'b', 'c'] {
            src.push(t, press(CKeyCode::Char(ch)));
            src.push(t, release(CKeyCode::Char(ch)));
            t += gap_ms;
        }
        src.push(t, press(CKeyCode::Enter));
        src.push(t, release(CKeyCode::Enter));
        let delayed_tail_at = t + ((PASTE_BATCH_ENTER_GRACE.as_millis() as u64 * 2) / 3).max(1);
        push_paste_batch(&mut src, delayed_tail_at, "next");

        let result = simulate_paste(&mut src);
        assert!(result.submits.is_empty());
        assert_eq!(result.final_input, "abc\nnext");
        assert_eq!(
            result.paste_flushes,
            vec!["abc\nnext".to_string()],
            "the paste body stored in the chip must include the whole first line"
        );
    }

    #[test]
    fn paste_simulation_middle_line_mode_shortcut_prefix_stays_paste_body() {
        let mut src = FakeEventSource::new();
        let text = "first line\n!not bash\n?not help\nlast line";
        push_paste_batch(&mut src, 0, text);

        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "mode shortcut prefixes in pasted middle lines must not submit; got {:?}",
            result.submits
        );
        assert!(
            result.mode_shortcuts.is_empty(),
            "mode shortcut prefixes in pasted middle lines must stay in the paste body; got {:?}",
            result.mode_shortcuts
        );
        assert_eq!(result.final_input, text);
        assert_eq!(result.paste_flushes, vec![text.to_string()]);
    }

    /// Sanity check for the simulation harness: a real human Submit
    /// (Enter pressed once with no events queued behind it) must still
    /// produce a submit, not absorb into a phantom paste.
    #[test]
    fn paste_simulation_real_enter_still_submits() {
        let mut src = FakeEventSource::new();
        // Type "hello" at typing pace, then Enter at typing pace, then
        // nothing.
        let typing_gap = 80u64;
        let mut t = 0u64;
        for ch in "hello".chars() {
            src.push(t, press(CKeyCode::Char(ch)));
            t += 1;
            src.push(t, release(CKeyCode::Char(ch)));
            t += typing_gap;
        }
        src.push(t, press(CKeyCode::Enter));
        t += 1;
        src.push(t, release(CKeyCode::Enter));
        let result = simulate_paste(&mut src);
        assert_eq!(
            result.submits,
            vec!["hello".to_string()],
            "typing 'hello' + Enter must produce exactly one Submit; got {:?}",
            result.submits
        );
        assert_eq!(result.final_input, "");
    }

    /// Regression: short single-line paste chunks can still be one
    /// logical bracketed paste when conpty surfaces a short ASCII run as
    /// raw key events between two Event::Paste deliveries. Absorb that
    /// run only because a following paste chunk proves it is paste body.
    #[test]
    fn coalesce_absorbs_ascii_run_between_paste_chunks_when_followed_by_paste() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("● 我先定位 ".into()));
        src.push(0, shifted_char('T'));
        src.push(0, shifted_char('U'));
        src.push(0, shifted_char('I'));
        src.push(0, Event::Paste(" 里\n后续内容".into()));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "● 我先定位 TUI 里\n后续内容");
        assert!(
            stashed.is_empty(),
            "raw key run should be consumed into paste"
        );
    }

    /// Regression: if the short ASCII key is not followed by another
    /// paste inside the straggler grace, replay it as ordinary input.
    #[test]
    fn coalesce_does_not_absorb_slow_ascii_key_before_later_paste() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("hello".into()));
        src.push(0, press(CKeyCode::Char('x')));
        src.push(
            (PASTE_STRAGGLER_GRACE.as_millis() as u64).saturating_add(1),
            Event::Paste("\nworld".into()),
        );
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "hello");
        assert!(matches!(
            stashed.front(),
            Some(Event::Key(KeyEvent {
                code: CKeyCode::Char('x'),
                ..
            }))
        ));
        assert!(matches!(
            src.events.front(),
            Some((_, Event::Paste(text))) if text == "\nworld"
        ));
    }

    /// Conpty splits very large bracketed pastes into multiple
    /// `Event::Paste(String)` deliveries. Without coalescing, each
    /// becomes its own `[Pasted text #N]` chip — the user reported
    /// seeing 9 chips for one paste. Verify the helper concatenates
    /// adjacent paste events and stops at the first non-paste event.
    #[test]
    fn coalesce_adjacent_pastes_concatenates_consecutive_paste_events() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("first chunk\n".into()));
        src.push(0, Event::Paste("second chunk\n".into()));
        src.push(0, Event::Paste("third chunk".into()));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::from("seed:");
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "seed:first chunk\nsecond chunk\nthird chunk");
        assert!(
            stashed.is_empty(),
            "queue drained, nothing should be stashed"
        );
    }

    /// Plain Char Press (no modifiers or only Shift) must be folded
    /// INTO the paste body. conpty-on-Windows splits bracketed pastes
    /// at non-ASCII byte boundaries and delivers the straddling char
    /// as a raw Key event — absorbing it keeps a single logical paste
    /// from turning into multiple chips.
    #[test]
    fn coalesce_adjacent_pastes_absorbs_plain_char_key_events() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("a".into()));
        src.push(0, press(CKeyCode::Char('—')));
        src.push(0, Event::Paste("b".into()));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(
            accumulated, "a—b",
            "em-dash key between chunks must be absorbed into the paste body"
        );
        assert!(stashed.is_empty(), "queue drained cleanly");
    }

    /// Regression: conpty can also surface plain ASCII body chars
    /// between paste chunks. Once the accumulated body is already
    /// multiline and will become a paste chip, keep ASCII stragglers
    /// attached to that same logical paste.
    #[test]
    fn coalesce_adjacent_pastes_absorbs_ascii_key_after_multiline_chunk() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("first line\n".into()));
        src.push(0, press(CKeyCode::Char('x')));
        src.push(0, Event::Paste("second line".into()));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "first line\nxsecond line");
        assert!(stashed.is_empty(), "queue drained cleanly");
    }

    /// Narrowing guard: after a short single-line paste, plain ASCII
    /// Char Press events still replay as normal input when no following
    /// paste chunk proves they are paste body.
    #[test]
    fn coalesce_does_not_absorb_ascii_char_without_following_paste() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("hello".into()));
        src.push(0, press(CKeyCode::Char('x')));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(
            accumulated, "hello",
            "ASCII key without a following paste inside grace must END coalescing"
        );
        match stashed.front() {
            Some(Event::Key(k)) if matches!(k.code, CKeyCode::Char('x')) => {}
            other => panic!("expected stashed plain 'x', got {:?}", other),
        }
    }

    /// Shift-only ASCII body characters should also be absorbed when a
    /// following Event::Paste proves they are part of a split paste.
    #[test]
    fn coalesce_absorbs_shift_ascii_run_between_paste_chunks() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("a".into()));
        src.push(0, shifted_char('A'));
        src.push(0, Event::Paste("b".into()));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "aAb");
        assert!(stashed.is_empty());
    }

    /// A modifier-carrying Char (Ctrl+C, Alt+x, …) or any non-Key
    /// non-Paste event must end coalescing and be stashed — those are
    /// real user input, not conpty stragglers.
    #[test]
    fn coalesce_adjacent_pastes_stops_at_ctrl_char_and_stashes_it() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("a".into()));
        src.push(
            0,
            Event::Key(KeyEvent {
                code: CKeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
        );
        src.push(0, Event::Paste("b".into()));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "a");
        match stashed.front() {
            Some(Event::Key(k))
                if matches!(k.code, CKeyCode::Char('c'))
                    && k.modifiers == KeyModifiers::CONTROL => {}
            other => panic!("expected stashed Ctrl+C, got {:?}", other),
        }
        assert!(
            !stashed.is_empty(),
            "modifier key should be stashed; following paste may remain queued"
        );
    }

    /// Non-Char key (arrow, escape, enter, …) must also stash — those
    /// are editing/navigation events, not paste stragglers.
    #[test]
    fn coalesce_adjacent_pastes_stops_at_non_char_key_and_stashes_it() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("a".into()));
        src.push(0, press(CKeyCode::Left));
        let mut stashed = VecDeque::new();
        let mut accumulated = String::new();
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "a");
        match stashed.front() {
            Some(Event::Key(k)) if matches!(k.code, CKeyCode::Left) => {}
            other => panic!("expected stashed Left, got {:?}", other),
        }
    }

    /// If a non-paste event is already stashed when coalescing starts,
    /// don't pull anything from the queue (the outer loop hasn't yet
    /// handled the stashed event — touching the queue would reorder
    /// events relative to the user's input timeline).
    #[test]
    fn coalesce_adjacent_pastes_skips_when_event_already_stashed() {
        let mut src = FakeEventSource::new();
        src.push(0, Event::Paste("would be lost".into()));
        let mut stashed = VecDeque::from([press(CKeyCode::Char('y'))]);
        let mut accumulated = String::from("untouched");
        coalesce_adjacent_pastes(&mut accumulated, &mut stashed, &mut src).unwrap();
        assert_eq!(accumulated, "untouched");
        assert!(matches!(stashed.front(), Some(Event::Key(_))));
        // Paste event remains queued for the next outer-loop iteration.
        assert!(src.front_ready());
    }

    /// Regression: is_paste_batch_key must accept Enter carrying a
    /// CONTROL/ALT/SUPER modifier, because Zed's built-in terminal on
    /// Windows delivers every pasted `\n` as Ctrl+Enter (Press+Release
    /// pair). If this helper rejects such events, the queue-depth peek
    /// in `detect_paste_batch_after_enter` misclassifies them as "not a
    /// paste" and each pasted line gets submitted individually.
    #[test]
    fn is_paste_batch_key_accepts_ctrl_enter() {
        let ctrl_enter_press = KeyEvent {
            code: CKeyCode::Enter,
            modifiers: CKeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(
            is_paste_batch_key(&ctrl_enter_press),
            "Ctrl+Enter (Press) must count as a paste-batch key for Zed compatibility"
        );
        let alt_enter_press = KeyEvent {
            code: CKeyCode::Enter,
            modifiers: CKeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(
            is_paste_batch_key(&alt_enter_press),
            "Alt+Enter (Press) must also be accepted (kitty/xterm deliver Alt+Enter similarly)"
        );
        // Ctrl+C stays rejected — modifier-bearing chars are never
        // paste content.
        let ctrl_c_press = KeyEvent {
            code: CKeyCode::Char('c'),
            modifiers: CKeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(
            !is_paste_batch_key(&ctrl_c_press),
            "Ctrl+Char must still be rejected; only Enter is whitelisted with modifiers"
        );
    }

    /// Push a paste batch that matches what Zed's built-in terminal on
    /// Windows delivers: every pasted `\n` arrives as a Ctrl+Enter
    /// Press+Release pair instead of a bare Enter. CC/codex-ref happen
    /// to work in the same terminal because they read raw byte streams;
    /// rebon sees the full crossterm KeyEvent and has to cope with the
    /// CONTROL modifier.
    fn push_paste_batch_zed(src: &mut FakeEventSource, arrival_ms: u64, text: &str) -> u64 {
        for ch in text.chars() {
            let (code, mods) = if ch == '\n' {
                (CKeyCode::Enter, CKeyModifiers::CONTROL)
            } else {
                (CKeyCode::Char(ch), CKeyModifiers::NONE)
            };
            let press = Event::Key(KeyEvent {
                code,
                modifiers: mods,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
            let release = Event::Key(KeyEvent {
                code,
                modifiers: mods,
                kind: KeyEventKind::Release,
                state: KeyEventState::NONE,
            });
            src.push(arrival_ms, press);
            src.push(arrival_ms, release);
        }
        arrival_ms
    }

    /// Zed Windows-terminal paste regression: a multi-line paste whose
    /// newlines arrive as Ctrl+Enter must NOT fragment into per-line
    /// submits. Before the is_paste_batch_key / TextEdit-return_key
    /// fixes, the first Ctrl+Enter force-flushed the burst and each
    /// subsequent line landed as its own chip — the user observed only
    /// the last line surviving in the prompt.
    #[test]
    fn paste_simulation_zed_ctrl_enter_keeps_all_lines_together() {
        let mut src = FakeEventSource::new();
        let lines = "alpha\nbravo\ncharlie\ndelta";
        push_paste_batch_zed(&mut src, 0, lines);
        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "Zed Ctrl+Enter paste must not produce mid-paste submits; got {:?}",
            result.submits
        );
        assert_eq!(
            result.final_input, lines,
            "all lines must land in the buffered paste, not get torn apart at Ctrl+Enter boundaries"
        );
    }

    /// Short-first-line Zed paste: `"a\n\nbc\nd"`. The double-newline
    /// case is the strongest regression signal because it produces two
    /// adjacent Ctrl+Enter events — the former is_paste_batch_key
    /// rejected Ctrl+Enter, so the queue-depth peek after the first
    /// newline saw "next is Ctrl+Enter → not a paste-key" and submitted
    /// the 'a' prematurely.
    #[test]
    fn paste_simulation_zed_ctrl_enter_handles_consecutive_newlines() {
        let mut src = FakeEventSource::new();
        push_paste_batch_zed(&mut src, 0, "a\n\nbc\nd");
        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "consecutive Ctrl+Enter newlines must not trigger a submit; got {:?}",
            result.submits
        );
        assert_eq!(result.final_input, "a\n\nbc\nd");
    }

    #[test]
    fn paste_simulation_windows_split_suffix_pollution_stays_single_logical_paste() {
        let mut src = FakeEventSource::new();
        let first = "line1\nline2\nline3\n";
        let second = "line4\nline5";
        push_paste_batch(&mut src, 0, first);
        push_paste_batch(
            &mut src,
            (BURST_IDLE_TIMEOUT + Duration::from_millis(20)).as_millis() as u64,
            second,
        );
        let result = simulate_paste(&mut src);
        assert!(result.submits.is_empty(), "{:?}", result.submits);
        assert_eq!(
            result.paste_flushes,
            vec![first.to_string(), second.to_string()]
        );
        assert_eq!(result.final_input, format!("{first}{second}"));
    }

    /// multi-char committed phrases as a rapid batch of individual
    /// `KeyCode::Char` events. The queue-depth paste detector fires on
    /// that batch, but each non-ASCII char must NOT destroy the burst
    /// mid-drain — otherwise every char except the last vanishes (the
    /// user-visible "只有最后一个字" regression).
    ///
    /// The guard lives in `drain_burst_queue`: when the drain sees a
    /// non-ASCII char, it must stash the event untouched and let the
    /// outer loop's TextEdit arm handle the `FlushThenPassThrough`
    /// transition so the buffered ASCII prefix (if any) lands in the
    /// prompt via `flush_burst_as_paste` instead of being silently
    /// dropped by the drain's `_` match arm.
    #[test]
    fn paste_simulation_ime_cjk_commit_does_not_lose_chars() {
        let mut src = FakeEventSource::new();
        // Four CJK chars delivered as one conpty batch — matches what a
        // Pinyin commit of "你好世界" emits on Windows.
        push_paste_batch(&mut src, 0, "你好世界");
        let result = simulate_paste(&mut src);
        assert!(
            result.submits.is_empty(),
            "CJK IME commit must not submit anything; got {:?}",
            result.submits
        );
        assert_eq!(
            result.final_input, "你好世界",
            "all CJK chars must survive the burst-detection detour; got {:?}",
            result.final_input
        );
    }

    /// Mixed ASCII→CJK commit: a pasted ASCII prefix followed by an IME
    /// commit in the same batch. The ASCII chars activate the burst;
    /// the first CJK char must terminate the drain cleanly and trigger
    /// a `FlushThenPassThrough` so the ASCII prefix lands as a paste
    /// chip before the CJK chars type in. Regression from the same
    /// drain bug as `paste_simulation_ime_cjk_commit_does_not_lose_chars`.
    #[test]
    fn paste_simulation_mixed_ascii_then_cjk_preserves_both_streams() {
        let mut src = FakeEventSource::new();
        push_paste_batch(&mut src, 0, "hello你好");
        let result = simulate_paste(&mut src);
        assert!(result.submits.is_empty(), "{:?}", result.submits);
        assert_eq!(
            result.final_input, "hello你好",
            "ASCII prefix must not be dropped when the burst transitions to CJK"
        );
    }
}
