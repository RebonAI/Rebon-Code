//! ── Batched Windows console input for pasted runs ────────────────
//!
//! On Windows there is no bracketed paste: conhost delivers a paste as
//! ordinary console input records, two per pasted character (key-down
//! and key-up). crossterm's Windows event source reads *one* record per
//! call — `WaitForSingleObject` + `GetNumberOfConsoleInputEvents` +
//! `ReadConsoleInputW` per record — and every one of those console APIs
//! is an IPC round trip to the console host process.
//!
//! Measured against a console stuffed with a synthetic paste (15 000
//! characters = 30 000 records):
//!
//! | read strategy                          | total    | per record |
//! |----------------------------------------|----------|------------|
//! | `event::poll(0)` + `event::read()`     | 1800 ms  | 60.0 us    |
//! | `ReadConsoleInputW` over the whole run |  7.7 ms  |  0.26 us   |
//!
//! And end to end, timing how long the real TUI takes to empty its
//! console queue after a paste lands in it — which is exactly the freeze
//! the user sees:
//!
//! | paste          | one record at a time | batched |
//! |----------------|----------------------|---------|
//! | 2 000 chars    |   249 ms             |  16 ms  |
//! | 12 000 chars   |  1448 ms             |  27 ms  |
//!
//! A few hundred pasted lines is the second row: over a second of frozen
//! prompt, scaling with the size of the paste, which is the "paste hangs for
//! a moment before it finishes" report. The buffer bookkeeping itself is
//! trivial —
//! essentially all of it is console round trips.
//!
//! So: consume the pasted run in one `ReadConsoleInputW`.
//!
//! ## Staying byte-identical to crossterm
//!
//! The risk in reading the queue ourselves is producing events that
//! differ from what crossterm would have produced. Two rules contain it:
//!
//! 1. **Peek before consuming.** `PeekConsoleInputW` is non-destructive,
//!    so we classify first and consume only the longest *prefix* of
//!    records we can translate with certainty. The first record we are
//!    not sure about ends the batch and stays in the queue for crossterm
//!    to read normally. Ordering is preserved because we always take a
//!    prefix and never leave anything of our own behind.
//! 2. **A deliberately narrow alphabet.** [`paste_run_event`] accepts
//!    only what a paste is made of — plain characters, Enter, Tab, with
//!    no Ctrl/Alt — and mirrors `crossterm::event::sys::windows::parse`
//!    for exactly those. Everything else (control codes needing a
//!    keyboard-layout lookup, surrogate halves, Alt codes, function and
//!    navigation keys, mouse, resize, focus) is left to crossterm.
//!
//! ## Where the batch goes
//!
//! Events land in the runner's existing `stashed_events` queue, which
//! every drain and the outer loop already drain before touching the
//! terminal. Nothing is buffered privately here, so a modal flow that
//! takes over the terminal sees the same picture it does today.
//!
//! One documented seam: crossterm clears its internal surrogate buffer
//! whenever it parses a valid key event, and records we consume never
//! reach it. A *lone, unpaired* high surrogate sitting in that buffer
//! from before a batch could therefore pair with the next surrogate
//! instead of being discarded. Conhost emits surrogates in pairs and we
//! stop the batch at every surrogate record, so this needs malformed
//! console input to bite.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::mem;
use std::sync::OnceLock;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crossterm_winapi::Handle;
use winapi::um::consoleapi::{GetNumberOfConsoleInputEvents, ReadConsoleInputW};
use winapi::um::wincon::{
    PeekConsoleInputW, INPUT_RECORD, KEY_EVENT, KEY_EVENT_RECORD, LEFT_ALT_PRESSED,
    LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED, SHIFT_PRESSED,
};
use winapi::um::winuser::{
    VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT,
    VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
};

/// Don't bother batching below this many queued records. Ordinary
/// typing never queues more than a couple of records (one key-down plus
/// its key-up), so this keeps every non-paste keystroke on crossterm's
/// well-worn path and confines the fast path to actual bursts. Eight
/// pasted characters is far below the burst detector's own threshold.
const MIN_BATCH_RECORDS: u32 = 16;

/// Upper bound on records consumed per call, so a huge paste is handed
/// over in slices instead of one enormous allocation. The drain loops
/// call back in as soon as the stash empties, so a bigger paste just
/// takes a few more batches.
const MAX_BATCH_RECORDS: usize = 8192;

/// Probes to skip after one comes back empty.
///
/// Asking whether a batch is available costs a console round trip of its
/// own — 16 us here, against 20 us for the single-record read it is
/// trying to replace. The drain loops ask once per event, so a stream
/// that never batches (conpty handing a split paste over a few records
/// at a time) would pay that probe on every event and come out *slower*
/// than before. Backing off after a miss amortises it to noise, and the
/// cost of backing off is only that a burst arriving right afterwards
/// waits a handful of events before batching.
const PROBES_SKIPPED_AFTER_MISS: u32 = 8;

thread_local! {
    /// `Handle::current_in_handle()` opens `CONIN$` with `CreateFileW`
    /// on every call; the event loop is single-threaded, so cache it.
    /// `None` means "opening it failed once" — see [`with_conin`].
    static CONIN: RefCell<Option<Option<Handle>>> = const { RefCell::new(None) };

    /// Remaining probes to skip after a miss. See
    /// [`PROBES_SKIPPED_AFTER_MISS`].
    static SKIP_PROBES: Cell<u32> = const { Cell::new(0) };
}

/// Returns true when this probe should be skipped outright, spending no
/// console call at all.
fn probe_backoff_active() -> bool {
    SKIP_PROBES.with(|skip| {
        let remaining = skip.get();
        if remaining == 0 {
            return false;
        }
        skip.set(remaining - 1);
        true
    })
}

fn note_probe_missed() {
    SKIP_PROBES.with(|skip| skip.set(PROBES_SKIPPED_AFTER_MISS));
}

fn note_probe_hit() {
    SKIP_PROBES.with(|skip| skip.set(0));
}

/// `REBON_PASTE_BATCH=0` puts every read back on crossterm's path.
///
/// Console behaviour varies more than the API suggests — conhost,
/// Windows Terminal, a conpty inside another editor — so keep one way to
/// take this out of the picture without a rebuild, both as an escape
/// hatch and to A/B the same binary.
fn batching_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED
        .get_or_init(|| std::env::var_os("REBON_PASTE_BATCH").is_some_and(|value| value == "0"))
}

/// Run `f` with the cached `CONIN$` handle. Returns `None` when the
/// process has no console input buffer to open (redirected stdin, a
/// detached process), in which case the caller falls back to crossterm —
/// which will fail the same way it does today, at its own call site.
fn with_conin<T>(f: impl FnOnce(&Handle) -> T) -> Option<T> {
    CONIN.with(|cell| {
        let mut slot = cell.borrow_mut();
        let handle = slot.get_or_insert_with(|| Handle::current_in_handle().ok());
        handle.as_ref().map(f)
    })
}

/// Number of records waiting in the console input queue.
fn queued_records(handle: &Handle) -> io::Result<u32> {
    let mut count: u32 = 0;
    let ok = unsafe { GetNumberOfConsoleInputEvents(**handle, &mut count) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(count)
}

/// Non-destructive look at the head of the queue.
fn peek_records(handle: &Handle, buffer: &mut [INPUT_RECORD]) -> io::Result<usize> {
    let mut read: u32 = 0;
    let ok = unsafe {
        PeekConsoleInputW(
            **handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut read,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(read as usize)
}

/// Consume exactly `count` records. Safe to call without blocking only
/// because a peek just confirmed at least that many are queued and
/// nothing else in the process reads `CONIN$`.
fn consume_records(handle: &Handle, buffer: &mut [INPUT_RECORD]) -> io::Result<usize> {
    let mut read: u32 = 0;
    let ok = unsafe {
        ReadConsoleInputW(
            **handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut read,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(read as usize)
}

/// Virtual key codes crossterm maps to something other than a plain
/// character. Any of them ends the batch: either the key is not paste
/// content at all, or reproducing crossterm's mapping would mean
/// duplicating its keyboard-layout lookup.
fn is_reserved_virtual_key(vk: i32) -> bool {
    matches!(vk, VK_SHIFT | VK_CONTROL | VK_MENU | VK_BACK | VK_ESCAPE)
        || matches!(
            vk,
            VK_LEFT | VK_UP | VK_RIGHT | VK_DOWN | VK_PRIOR | VK_NEXT | VK_HOME | VK_END
        )
        || matches!(vk, VK_DELETE | VK_INSERT)
        || (VK_F1..=VK_F24).contains(&vk)
}

/// What a batch should do with one console record.
#[derive(Debug, PartialEq)]
enum RecordOutcome {
    /// The event crossterm would have produced.
    Emit(Event),
    /// crossterm consumes this record and produces nothing. Consuming it
    /// silently is what keeps the batch going; stopping here instead is
    /// what made batching useless on real text (see [`paste_run_event`]).
    Skip,
    /// Not ours to translate — leave it, and everything after it, queued.
    Stop,
}

/// Translate one console record into the event crossterm would produce.
///
/// The accepted alphabet is exactly what a pasted run contains: plain
/// characters, Enter, and Tab, none of them carrying Ctrl or Alt. For
/// those, crossterm's parser reduces to: modifiers from the control-key
/// state (Shift only, once Ctrl/Alt are excluded), `KeyCode` from the
/// virtual key code, kind from the key-down flag, and `KeyEventState`
/// left at `NONE` — which is what this reproduces.
///
/// The one record that produces *nothing* and must still not end the
/// batch is the standalone Shift key. Pasting `:` or `{` or `(` makes
/// conhost synthesize a `VK_SHIFT` press around the character, so a rule
/// that stops at every untranslatable record stops at every shifted
/// character — on a page of JSON that is roughly a quarter of the text,
/// and it leaves runs averaging eight records where the batch needs
/// sixteen. Measured on a 1 019-line paste: stopping there covers 57% of
/// records in runs of mean length 8.5; skipping it covers 99.9% in runs
/// of mean length 4 226. crossterm maps `VK_SHIFT` (and `VK_CONTROL`,
/// which never reaches here because the control-key-state check above
/// already rejects it) to `None` — it consumes the record and emits no
/// event, which is exactly what `Skip` does.
fn paste_run_event(record: &KEY_EVENT_RECORD) -> RecordOutcome {
    let control_key_state = record.dwControlKeyState;

    // Alt is crossterm's Alt-code path — a key-up carrying the composed
    // character, reconstructed across several records — so every record
    // announcing Alt is left whole for crossterm.
    if control_key_state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
        return RecordOutcome::Stop;
    }

    let vk = record.wVirtualKeyCode as i32;

    // A modifier's own press/release. crossterm emits nothing for either,
    // whatever else is held, and the state they announce is already set
    // on every record that follows — so dropping them changes no event we
    // produce, while stopping at them would end the batch at every
    // shifted character.
    if vk == VK_SHIFT || vk == VK_CONTROL {
        return RecordOutcome::Skip;
    }

    let shift = control_key_state & SHIFT_PRESSED != 0;
    let ctrl = control_key_state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
    let mut modifiers = KeyModifiers::NONE;
    if shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }

    let code = if vk == VK_RETURN {
        // Ctrl is deliberately allowed here. conhost delivers a pasted
        // `\n` as Ctrl+Enter, so refusing Ctrl outright ended the batch
        // once per pasted line — a 1 019-line paste broke into 1 019
        // fragments, each too short to batch. crossterm maps `VK_RETURN`
        // to `Enter` unconditionally, modifiers and character ignored,
        // so reproducing it needs no layout lookup.
        KeyCode::Enter
    } else if vk == VK_TAB {
        // Shift+Tab is `BackTab` and is navigation, not paste content.
        if shift {
            return RecordOutcome::Stop;
        }
        KeyCode::Tab
    } else if is_reserved_virtual_key(vk) {
        return RecordOutcome::Stop;
    } else {
        // Ctrl with an ordinary key is a shortcut, and crossterm resolves
        // its character through a `ToUnicodeEx` keyboard-layout lookup.
        // Only the two virtual keys above are decided without one.
        if ctrl {
            return RecordOutcome::Stop;
        }
        // `u_char` is a UTF-16 code unit. Control codes (0x00..=0x1f)
        // send crossterm through a `ToUnicodeEx` keyboard-layout lookup,
        // and surrogate halves need its cross-record pairing buffer;
        // hand both back.
        let unit = unsafe { *record.uChar.UnicodeChar() };
        match unit {
            0x0020..=0xD7FF | 0xE000..=0xFFFF => match char::from_u32(u32::from(unit)) {
                Some(ch) => KeyCode::Char(ch),
                None => return RecordOutcome::Stop,
            },
            _ => return RecordOutcome::Stop,
        }
    };

    let kind = if record.bKeyDown != 0 {
        KeyEventKind::Press
    } else {
        KeyEventKind::Release
    };
    RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(code, modifiers, kind)))
}

/// Pull the queued pasted run into `out` in one console read.
///
/// Returns the number of events appended — `0` means "nothing batchable
/// was waiting", and the caller proceeds through crossterm exactly as
/// before. Only ever called with an empty `out`, so a batch can never
/// jump ahead of an already-stashed event.
pub(super) fn refill_stashed_events(out: &mut VecDeque<Event>) -> io::Result<usize> {
    if !out.is_empty() || batching_disabled() || probe_backoff_active() {
        return Ok(0);
    }

    let Some(result) = with_conin(|handle| -> io::Result<usize> {
        let queued = queued_records(handle)?;
        if queued < MIN_BATCH_RECORDS {
            note_probe_missed();
            return Ok(0);
        }

        let window = (queued as usize).min(MAX_BATCH_RECORDS);
        let mut records: Vec<INPUT_RECORD> = vec![unsafe { mem::zeroed() }; window];
        let peeked = peek_records(handle, &mut records)?;

        // Longest prefix we can translate with certainty. `scanned`
        // counts records while `events` counts what they produce; the
        // two differ because some records are consumed silently, so
        // remember which record each event came from for the short-read
        // fixup below.
        let mut events: Vec<Event> = Vec::with_capacity(peeked);
        let mut event_record_index: Vec<usize> = Vec::with_capacity(peeked);
        let mut scanned = 0usize;
        for (index, record) in records.iter().take(peeked).enumerate() {
            if record.EventType != KEY_EVENT {
                break;
            }
            let key = unsafe { record.Event.KeyEvent() };
            match paste_run_event(key) {
                RecordOutcome::Emit(event) => {
                    events.push(event);
                    event_record_index.push(index);
                    scanned = index + 1;
                }
                RecordOutcome::Skip => scanned = index + 1,
                RecordOutcome::Stop => break,
            }
        }

        // Too short to be worth a second console call — and, more to the
        // point, too short to be a paste. Leave the queue untouched.
        if scanned < MIN_BATCH_RECORDS as usize {
            note_probe_missed();
            return Ok(0);
        }
        note_probe_hit();

        let consumed = consume_records(handle, &mut records[..scanned])?;
        // `ReadConsoleInputW` returns as soon as it has records and can
        // stop short; only publish the events whose records it actually
        // took, and leave the rest queued.
        let kept = event_record_index.partition_point(|index| *index < consumed);
        events.truncate(kept);
        out.extend(events);
        Ok(kept)
    }) else {
        return Ok(0);
    };

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_record(ch: char, down: bool, vk: i32, control_key_state: u32) -> KEY_EVENT_RECORD {
        let mut record: KEY_EVENT_RECORD = unsafe { mem::zeroed() };
        record.bKeyDown = i32::from(down);
        record.wRepeatCount = 1;
        record.wVirtualKeyCode = vk as u16;
        record.dwControlKeyState = control_key_state;
        unsafe {
            *record.uChar.UnicodeChar_mut() = ch as u16;
        }
        record
    }

    fn plain(ch: char) -> KEY_EVENT_RECORD {
        key_record(ch, true, 0x41, 0)
    }

    #[test]
    fn plain_character_matches_crossterms_press_event() {
        assert_eq!(
            paste_run_event(&plain('a')),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )))
        );
    }

    #[test]
    fn key_up_record_becomes_a_release_event() {
        let record = key_record('a', false, 0x41, 0);
        assert_eq!(
            paste_run_event(&record),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )))
        );
    }

    #[test]
    fn shift_is_carried_but_uppercase_comes_from_the_record() {
        let record = key_record('A', true, 0x41, SHIFT_PRESSED);
        assert_eq!(
            paste_run_event(&record),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('A'),
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
            )))
        );
    }

    #[test]
    fn enter_and_tab_are_paste_content() {
        assert_eq!(
            paste_run_event(&key_record('\r', true, VK_RETURN, 0)),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )))
        );
        assert_eq!(
            paste_run_event(&key_record('\t', true, VK_TAB, 0)),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Tab,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )))
        );
    }

    #[test]
    fn control_and_alt_combinations_are_left_to_crossterm() {
        for state in [
            LEFT_CTRL_PRESSED,
            RIGHT_CTRL_PRESSED,
            LEFT_ALT_PRESSED,
            RIGHT_ALT_PRESSED,
        ] {
            assert_eq!(
                paste_run_event(&key_record('c', true, 0x43, state)),
                RecordOutcome::Stop
            );
        }
    }

    #[test]
    fn editing_and_navigation_keys_end_the_batch() {
        for vk in [
            VK_BACK, VK_ESCAPE, VK_LEFT, VK_UP, VK_RIGHT, VK_DOWN, VK_HOME, VK_END, VK_DELETE,
            VK_INSERT, VK_PRIOR, VK_NEXT, VK_F1, VK_F24, VK_MENU,
        ] {
            assert_eq!(
                paste_run_event(&key_record('\0', true, vk, 0)),
                RecordOutcome::Stop,
                "vk {vk:#x} must be left to crossterm"
            );
        }
        // Shift+Tab is BackTab, which crossterm maps differently.
        assert_eq!(
            paste_run_event(&key_record('\t', true, VK_TAB, SHIFT_PRESSED)),
            RecordOutcome::Stop
        );
    }

    #[test]
    fn a_pasted_newline_arrives_as_ctrl_enter_and_still_batches() {
        // conhost delivers every `\n` in a paste as Ctrl+Enter. Refusing
        // Ctrl outright ended the batch once per pasted line. crossterm
        // maps VK_RETURN to Enter whatever is held, so this is the same
        // event it would have produced.
        assert_eq!(
            paste_run_event(&key_record('\n', true, VK_RETURN, LEFT_CTRL_PRESSED)),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            )))
        );
        assert_eq!(
            paste_run_event(&key_record('\n', false, VK_RETURN, RIGHT_CTRL_PRESSED)),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::CONTROL,
                KeyEventKind::Release,
            )))
        );
        // Ctrl with any other key still needs crossterm's layout lookup.
        assert_eq!(
            paste_run_event(&key_record('\u{1}', true, 0x41, LEFT_CTRL_PRESSED)),
            RecordOutcome::Stop
        );
        // Alt wins over everything: that is the Alt-code path.
        assert_eq!(
            paste_run_event(&key_record('\n', true, VK_RETURN, LEFT_ALT_PRESSED)),
            RecordOutcome::Stop
        );
    }

    #[test]
    fn the_shift_key_itself_is_swallowed_rather_than_ending_the_batch() {
        // conhost brackets every shifted character in a paste with the
        // Shift key's own press and release. crossterm maps both to no
        // event at all, so consuming them silently keeps the run going
        // instead of chopping the paste into eight-record fragments.
        for down in [true, false] {
            assert_eq!(
                paste_run_event(&key_record('\0', down, VK_SHIFT, SHIFT_PRESSED)),
                RecordOutcome::Skip
            );
        }
        assert_eq!(
            paste_run_event(&key_record('\0', true, VK_SHIFT, 0)),
            RecordOutcome::Skip
        );
        // Same for the Ctrl key's own record: crossterm emits nothing.
        assert_eq!(
            paste_run_event(&key_record('\0', true, VK_CONTROL, LEFT_CTRL_PRESSED)),
            RecordOutcome::Skip
        );
        // Alt still wins: that record belongs to the Alt-code path.
        assert_eq!(
            paste_run_event(&key_record('\0', true, VK_SHIFT, LEFT_ALT_PRESSED)),
            RecordOutcome::Stop
        );
    }

    #[test]
    fn surrogate_halves_and_control_codes_are_left_to_crossterm() {
        let mut high = plain('a');
        unsafe {
            *high.uChar.UnicodeChar_mut() = 0xD83D;
        }
        assert_eq!(paste_run_event(&high), RecordOutcome::Stop);

        let mut control_code = plain('a');
        unsafe {
            *control_code.uChar.UnicodeChar_mut() = 0x01;
        }
        assert_eq!(paste_run_event(&control_code), RecordOutcome::Stop);
    }

    #[test]
    fn non_ascii_characters_still_batch() {
        // CJK arrives as a single BMP code unit and is ordinary paste
        // content; the burst drain stashes it for the IME path itself.
        assert_eq!(
            paste_run_event(&plain('中')),
            RecordOutcome::Emit(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('中'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )))
        );
    }

    #[test]
    fn a_miss_backs_the_probe_off_and_a_hit_clears_it() {
        note_probe_missed();
        // Exactly `PROBES_SKIPPED_AFTER_MISS` probes are skipped, then
        // probing resumes.
        for i in 0..PROBES_SKIPPED_AFTER_MISS {
            assert!(probe_backoff_active(), "probe {i} should be skipped");
        }
        assert!(!probe_backoff_active());
        assert!(!probe_backoff_active());

        note_probe_missed();
        assert!(probe_backoff_active());
        note_probe_hit();
        assert!(!probe_backoff_active());
    }

    #[test]
    fn a_non_empty_stash_is_never_reordered() {
        let mut out = VecDeque::from(vec![Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))]);
        assert_eq!(refill_stashed_events(&mut out).unwrap(), 0);
        assert_eq!(out.len(), 1);
    }
}
