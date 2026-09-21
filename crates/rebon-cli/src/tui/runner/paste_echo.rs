// ── Clipboard adoption + paste echo suppression ──────────────────
//
// A terminal delivers pastes as a byte stream over the pty/conpty
// pipe. For multi-megabyte pastes that stream takes anywhere from
// hundreds of milliseconds (bracketed, Unix) to many seconds
// (Windows conhost key floods) — and no amount of app-side
// optimization makes the *stream* arrive faster. The only way to
// make paste feel instant regardless of length is to stop treating
// the stream as the source of truth:
//
// 1. **Adopt**: on the first `Event::Paste` chunk, read the OS
//    clipboard directly. If the normalized clipboard *starts with*
//    the normalized chunk, the stream is (with overwhelming
//    likelihood) a replay of the clipboard — apply the full
//    clipboard text immediately (chip appears now), and
// 2. **Suppress**: arm a `PasteEchoSuppressor` that matches the
//    remaining stream against the adopted text and drops it instead
//    of re-accumulating it. Matching is cheap (memcmp against the
//    expected remainder), and the event loop keeps rendering between
//    chunks, so the UI stays live while the echo drains.
//
// Every failure path degrades to today's behaviour:
// * clipboard unreadable / mismatch → the legacy blocking coalesce
//   path runs unchanged;
// * the stream diverges mid-echo → the confirmed prefix stays, the
//   over-adopted suffix is repaired away
//   (`repair_overadopted_text_chip`), and the divergent tail flows
//   through the normal flush path where the 15 s chip-merge grace
//   attaches it back to the same chip;
// * the stream ends early (idle) → same repair, then any late
//   stragglers merge back via the chip-merge grace. Net result
//   converges to exactly what the legacy path would have produced.
//
// Over SSH the remote clipboard has nothing to do with what the user
// is pasting, so the prefix check fails and everything stays on the
// legacy path — correct, just not instant (physics: the content only
// exists in the stream).
//
// The suppressor is also armed (in a weaker, key-matching-disabled
// mode) after a direct Ctrl+V clipboard paste, so a terminal that
// both forwards the key *and* synthesizes a paste event does not
// paste the same content twice.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rebon_tui::promptinput::clamp_cursor_offset;
use rebon_tui::promptinput::paste_flow::{
    normalize_pasted_text, repair_overadopted_text_chip, OveradoptedChipRepairInput,
};

use crate::tui::app::AppState;

/// Minimum normalized-prefix length (bytes) required before a
/// clipboard adoption may fire on a *proper prefix* match. Split
/// bracketed pastes arrive in multi-kilobyte chunks, so a real first
/// chunk clears this easily; a short coincidental prefix (e.g. the
/// user pasting a small unrelated selection) does not.
pub(super) const MIN_ADOPTION_PREFIX_BYTES: usize = 256;

/// How long an armed stream-adoption suppressor waits for the next
/// echo chunk before concluding the stream ended. Early finalize is
/// harmless (see module docs: late stragglers converge via the
/// chip-merge grace), so this only needs to sit above typical conpty
/// inter-chunk jitter.
#[cfg(windows)]
const STREAM_ADOPTION_IDLE: Duration = Duration::from_millis(600);
#[cfg(not(windows))]
const STREAM_ADOPTION_IDLE: Duration = Duration::from_millis(250);

/// How long a direct-key (Ctrl+V) suppressor stays armed waiting for
/// a possible terminal echo. The echo, when a terminal produces one,
/// comes from the same keystroke and arrives essentially instantly.
const DIRECT_KEY_ECHO_IDLE: Duration = Duration::from_millis(300);

/// How far ahead to look for the echo when a replayed character does not
/// continue it.
///
/// The terminal's replay is not a faithful copy of what it pasted. On a
/// real 49,400-byte paste this console dropped 61 bytes — `</span>`
/// arrived as `</s>` — and every dropped byte puts the strict matcher
/// permanently out of step. Treating that as "the stream diverged" is
/// expensive twice over: the adopted text, which was read straight from
/// the clipboard and is *correct*, gets trimmed back to what the lossy
/// replay had confirmed, and the rest of the replay is then handed to the
/// burst detector, which buffers it with no frame drawn until it drains
/// (21 s in that measurement).
///
/// The clipboard is the authority on what was pasted; the replay only
/// says when it is over. So skip ahead over what the terminal dropped and
/// keep suppressing.
const ECHO_RESYNC_WINDOW: usize = 64;

/// How many times a single replay may be re-synced before it is treated
/// as genuinely different content rather than a lossy copy. A handful of
/// dropped runs is a lossy terminal; hundreds is a different string.
const ECHO_RESYNC_LIMIT: u32 = 64;

/// Kill switch: `REBON_PASTE_CLIPBOARD_ADOPTION=0` (or `false`/`off`)
/// disables clipboard adoption and echo suppression entirely,
/// reverting to the pure stream-collection paths.
pub fn paste_echo_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("REBON_PASTE_CLIPBOARD_ADOPTION")
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Outcome of feeding one normalized paste chunk to the suppressor.
#[derive(Debug, PartialEq, Eq)]
pub enum EchoConsume {
    /// Chunk matched (part of) the expected remainder — drop it.
    Swallowed,
    /// Chunk matched and completed the expected text — drop it and
    /// disarm.
    Done,
    /// Chunk diverged from the expected remainder after `matched`
    /// advanced past any common prefix. `tail_norm` is the divergent
    /// (already normalized) portion, which is genuinely-new input the
    /// caller must process as a fresh paste.
    Diverged { tail_norm: String },
}

/// Outcome of feeding one key-event char to the suppressor.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyEcho {
    /// Char matched the expected remainder — swallow the key event.
    Swallowed,
    /// Char matched and completed the expected text — swallow and
    /// disarm.
    Done,
    /// Char does not continue the echo (or key matching is disabled
    /// for this suppressor) — finalize and process the key normally.
    Mismatch,
}

/// Where optimistically adopted text must be repaired if the terminal
/// stream ends before confirming the full clipboard value.
#[derive(Debug, Clone)]
pub(super) enum AdoptionRepairTarget {
    TextChip {
        chip_id: u32,
        omitted_prefix_len: usize,
    },
    Inline(InlineAdoptionRepair),
}

#[derive(Debug, Clone)]
pub(super) struct InlineAdoptionRepair {
    input_after: String,
    start: usize,
    applied_text: String,
    omitted_prefix_len: usize,
}

/// Matches the terminal's replay of text that was already applied
/// from the OS clipboard, so it can be dropped instead of pasted a
/// second time. See the module docs for the full protocol.
#[derive(Debug, Clone)]
pub struct PasteEchoSuppressor {
    /// Normalized full adopted text.
    expected_norm: String,
    /// Byte offset into `expected_norm` the stream has confirmed.
    matched: usize,
    /// Applied text to repair when the echo ends short of full confirmation.
    repair_target: Option<AdoptionRepairTarget>,
    /// Whether plain key events may be matched as echo stragglers.
    /// True only for stream adoption: mid-stream, the user physically
    /// cannot interleave keystrokes (their input queues behind the
    /// paste bytes), so a plain char while armed is stream content.
    /// Direct-key mode must never match keys — the user may type
    /// immediately after Ctrl+V.
    match_keys: bool,
    /// Whether an early end-of-stream requires repairing the chip.
    repair_on_shortfall: bool,
    idle_timeout: Duration,
    last_activity: Instant,
    /// How many times the replay has been re-synced past characters the
    /// terminal dropped. See [`ECHO_RESYNC_WINDOW`].
    resyncs: u32,
}

impl PasteEchoSuppressor {
    /// Arm after adopting the clipboard for a split paste stream.
    /// `matched` is the prefix already delivered (the first chunk).
    pub(super) fn for_stream_adoption(
        expected_norm: String,
        matched: usize,
        repair_target: Option<AdoptionRepairTarget>,
        now: Instant,
    ) -> Self {
        Self {
            expected_norm,
            matched,
            repair_target,
            match_keys: true,
            repair_on_shortfall: true,
            idle_timeout: STREAM_ADOPTION_IDLE,
            last_activity: now,
            resyncs: 0,
        }
    }

    /// Arm after a direct Ctrl+V clipboard paste, purely to swallow a
    /// possible terminal echo of the same content. Never matches key
    /// events and never repairs — if no echo comes, it just expires.
    pub fn for_direct_key_paste(expected_norm: String, now: Instant) -> Self {
        Self {
            expected_norm,
            matched: 0,
            repair_target: None,
            match_keys: false,
            repair_on_shortfall: false,
            idle_timeout: DIRECT_KEY_ECHO_IDLE,
            last_activity: now,
            resyncs: 0,
        }
    }

    pub fn fully_matched(&self) -> bool {
        self.matched >= self.expected_norm.len()
    }

    pub fn matched_len(&self) -> usize {
        self.matched
    }

    pub fn expected_norm(&self) -> &str {
        &self.expected_norm
    }

    pub fn repair_on_shortfall(&self) -> bool {
        self.repair_on_shortfall
    }

    pub fn is_idle_expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_activity) >= self.idle_timeout
    }

    /// Feed a normalized `Event::Paste` chunk through the matcher.
    pub fn consume_paste_chunk(&mut self, chunk_norm: &str, now: Instant) -> EchoConsume {
        self.last_activity = now;
        let remaining = &self.expected_norm[self.matched..];
        if remaining.starts_with(chunk_norm) {
            self.matched += chunk_norm.len();
            if self.fully_matched() {
                EchoConsume::Done
            } else {
                EchoConsume::Swallowed
            }
        } else if let Some(tail) = chunk_norm.strip_prefix(remaining) {
            // Chunk overruns the adopted end: the remainder is fully
            // confirmed and the overhang is genuinely-new input
            // (e.g. a second rapid paste glued onto the echo).
            self.matched = self.expected_norm.len();
            EchoConsume::Diverged {
                tail_norm: tail.to_string(),
            }
        } else {
            let p = common_prefix_len(remaining, chunk_norm);
            self.matched += p;
            EchoConsume::Diverged {
                tail_norm: chunk_norm[p..].to_string(),
            }
        }
    }

    /// Feed a plain key-event char through the matcher (stream
    /// adoption only — conpty occasionally delivers paste-body chars
    /// as raw key events at chunk boundaries).
    pub fn consume_key_char(&mut self, ch: char, now: Instant) -> KeyEcho {
        if !self.match_keys {
            return KeyEcho::Mismatch;
        }
        let mut buf = [0u8; 4];
        let norm: &str = match ch {
            '\r' | '\n' => "\n",
            // Mirror normalize_pasted_text's tab expansion.
            '\t' => "    ",
            c => c.encode_utf8(&mut buf),
        };
        let remaining = &self.expected_norm[self.matched..];
        let skipped = if remaining.starts_with(norm) {
            Some(0)
        } else {
            // The terminal drops characters; look a little way ahead
            // rather than declaring the replay divergent.
            resync_offset(remaining, norm, ECHO_RESYNC_WINDOW)
                .filter(|_| self.resyncs < ECHO_RESYNC_LIMIT)
        };
        let Some(skipped) = skipped else {
            return KeyEcho::Mismatch;
        };
        if skipped > 0 {
            self.resyncs += 1;
        }
        self.last_activity = now;
        self.matched += skipped + norm.len();
        if self.fully_matched() {
            KeyEcho::Done
        } else {
            KeyEcho::Swallowed
        }
    }
}

/// Where `needle` picks the echo back up inside the first `window` bytes
/// of `remaining`, skipping over what the terminal dropped.
///
/// Returns the number of bytes to skip, or `None` when the needle is not
/// in reach — which is what a genuinely divergent stream looks like.
fn resync_offset(remaining: &str, needle: &str, window: usize) -> Option<usize> {
    if needle.is_empty() || remaining.is_empty() {
        return None;
    }
    let end = remaining.len().min(window);
    let mut haystack_end = end;
    while haystack_end > 0 && !remaining.is_char_boundary(haystack_end) {
        haystack_end -= 1;
    }
    let haystack = &remaining[..haystack_end];
    // Start at 1: offset 0 is the ordinary match the caller already tried.
    haystack
        .char_indices()
        .skip(1)
        .find(|(i, _)| haystack[*i..].starts_with(needle))
        .map(|(i, _)| i)
}

/// Longest common prefix of two strings, snapped back to a char
/// boundary (identical bytes up to the returned length, so the
/// boundary holds for both).
fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut n = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(x, y)| x == y)
        .count();
    while n > 0 && !a.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// Pure adoption decision for the first `Event::Paste` chunk.
#[derive(Debug, PartialEq, Eq)]
pub enum AdoptionPlan {
    /// The chunk already covers the whole clipboard — the paste is
    /// complete; skip the blocking coalesce grace entirely.
    Complete,
    /// The chunk is a proper prefix of the clipboard — apply the full
    /// clipboard now and arm a suppressor for the echo remainder.
    AdoptClipboard {
        clipboard_norm: String,
        matched: usize,
    },
    /// No adoption — run the legacy coalesce path.
    None,
}

pub fn plan_clipboard_adoption(chunk_norm: &str, clipboard_norm: String) -> AdoptionPlan {
    if chunk_norm.is_empty() || clipboard_norm.is_empty() {
        return AdoptionPlan::None;
    }
    if chunk_norm == clipboard_norm {
        return AdoptionPlan::Complete;
    }
    if chunk_norm.len() >= MIN_ADOPTION_PREFIX_BYTES
        && clipboard_norm.len() > chunk_norm.len()
        && clipboard_norm.starts_with(chunk_norm)
    {
        let matched = chunk_norm.len();
        return AdoptionPlan::AdoptClipboard {
            clipboard_norm,
            matched,
        };
    }
    AdoptionPlan::None
}

/// Clipboard-backed adoption decision for the first chunk of an
/// `Event::Paste`. Reads the OS clipboard (with a short retry — the
/// Windows clipboard can be transiently contended) and runs
/// [`plan_clipboard_adoption`].
pub enum ClipboardAdoption {
    Complete,
    Adopt {
        /// Raw clipboard text to apply (the flush path normalizes it
        /// to exactly `clipboard_norm`).
        clipboard_raw: String,
        clipboard_norm: String,
        matched: usize,
    },
    None,
}

pub fn try_adopt_clipboard_for_first_chunk(chunk: &str) -> ClipboardAdoption {
    try_adopt_clipboard_for_first_chunk_with(chunk, read_clipboard_text)
}

pub(super) fn try_adopt_clipboard_for_first_chunk_with(
    chunk: &str,
    read_clipboard: impl FnOnce() -> Option<String>,
) -> ClipboardAdoption {
    let chunk_norm = normalize_pasted_text(chunk);
    if chunk_norm.is_empty() {
        return ClipboardAdoption::None;
    }
    let Some(clipboard_raw) = read_clipboard() else {
        return ClipboardAdoption::None;
    };
    if crate::tui::clipboard_image::pasted_image_path_from_text(&clipboard_raw).is_some() {
        return ClipboardAdoption::None;
    }
    let clipboard_norm = normalize_pasted_text(&clipboard_raw);
    match plan_clipboard_adoption(&chunk_norm, clipboard_norm) {
        AdoptionPlan::Complete => ClipboardAdoption::Complete,
        AdoptionPlan::AdoptClipboard {
            clipboard_norm,
            matched,
        } => ClipboardAdoption::Adopt {
            clipboard_raw,
            clipboard_norm,
            matched,
        },
        AdoptionPlan::None => ClipboardAdoption::None,
    }
}

pub(super) fn clipboard_image_path_matches_prefix(prefix: &str) -> bool {
    clipboard_image_path_matches_prefix_with(prefix, read_clipboard_text)
}

pub(super) fn clipboard_image_path_matches_prefix_with(
    prefix: &str,
    read_clipboard: impl FnOnce() -> Option<String>,
) -> bool {
    let prefix_norm = normalize_pasted_text(prefix);
    if prefix_norm.is_empty() {
        return false;
    }
    let Some(clipboard_raw) = read_clipboard() else {
        return false;
    };
    if crate::tui::clipboard_image::pasted_image_path_from_text(&clipboard_raw).is_none() {
        return false;
    }
    normalize_pasted_text(&clipboard_raw).starts_with(&prefix_norm)
}

pub(super) fn is_main_prompt_paste_target(app: &AppState) -> bool {
    !app.has_modal_overlay()
        && !app
            .agent_view
            .as_ref()
            .is_some_and(|view| view.is_input_focused())
}

pub(super) fn snapshot_text_chips(app: &AppState) -> Vec<(u32, usize)> {
    app.pasted_contents
        .iter()
        .filter(|content| content.kind == "text")
        .map(|content| (content.id, content.content.len()))
        .collect()
}

pub(super) fn find_adopted_text_chip_id(
    app: &AppState,
    text_chips_before: &[(u32, usize)],
    clipboard_norm: &str,
) -> Option<u32> {
    app.pasted_contents
        .iter()
        .rev()
        .find(|content| {
            if content.kind != "text" || !content.content.ends_with(clipboard_norm) {
                return false;
            }
            match text_chips_before.iter().find(|(id, _)| *id == content.id) {
                Some((_, len)) => content.content.len() > *len,
                None => true,
            }
        })
        .map(|content| content.id)
}

pub(super) fn find_adopted_text_repair_target(
    app: &AppState,
    text_chips_before: &[(u32, usize)],
    input_before: &str,
    cursor_before: usize,
    clipboard_norm: &str,
) -> Option<AdoptionRepairTarget> {
    if let Some(inline) = inline_adoption_repair_target(
        input_before,
        cursor_before,
        &app.input,
        app.cursor_offset,
        clipboard_norm,
    ) {
        return Some(inline);
    }

    let omitted_prefixes = [
        Some(0usize),
        (input_before.is_empty() && clipboard_norm.starts_with('!')).then_some(1usize),
    ];
    for omitted_prefix_len in omitted_prefixes.into_iter().flatten() {
        let applied_text = clipboard_norm.get(omitted_prefix_len..)?;
        if let Some(chip_id) = find_adopted_text_chip_id(app, text_chips_before, applied_text) {
            return Some(AdoptionRepairTarget::TextChip {
                chip_id,
                omitted_prefix_len,
            });
        }
    }
    None
}

pub(super) fn inline_adoption_repair_target(
    input_before: &str,
    cursor_before: usize,
    input_after: &str,
    cursor_after: usize,
    clipboard_norm: &str,
) -> Option<AdoptionRepairTarget> {
    let start = clamp_cursor_offset(input_before, cursor_before);
    let omitted_prefixes = [
        Some(0usize),
        (input_before.is_empty() && clipboard_norm.starts_with('!')).then_some(1usize),
    ];

    for omitted_prefix_len in omitted_prefixes.into_iter().flatten() {
        let applied_text = clipboard_norm.get(omitted_prefix_len..)?;
        let end = start.checked_add(applied_text.len())?;
        if input_after.len() != input_before.len().checked_add(applied_text.len())?
            || cursor_after != end
            || input_after.get(..start) != input_before.get(..start)
            || input_after.get(start..end) != Some(applied_text)
            || input_after.get(end..) != input_before.get(start..)
        {
            continue;
        }
        return Some(AdoptionRepairTarget::Inline(InlineAdoptionRepair {
            input_after: input_after.to_string(),
            start,
            applied_text: applied_text.to_string(),
            omitted_prefix_len,
        }));
    }
    None
}

fn read_clipboard_text() -> Option<String> {
    // Headless environments (SSH without X forwarding, containers)
    // have no clipboard at all — latch the constructor failure so we
    // don't pay the retry loop on every subsequent paste.
    static CLIPBOARD_UNAVAILABLE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if CLIPBOARD_UNAVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let mut constructed = false;
    for attempt in 0..3 {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            constructed = true;
            if let Ok(text) = clipboard.get_text() {
                if text.is_empty() {
                    return None;
                }
                return Some(text);
            }
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if !constructed {
        CLIPBOARD_UNAVAILABLE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    None
}

#[derive(Debug, PartialEq, Eq)]
struct InlineAdoptionRepairResult {
    input: String,
    cursor_offset: usize,
    trimmed_bytes: usize,
}

fn repair_overadopted_inline_text(
    current_input: &str,
    cursor_offset: usize,
    adopted_norm: &str,
    confirmed_len: usize,
    repair: &InlineAdoptionRepair,
) -> Option<InlineAdoptionRepairResult> {
    if current_input != repair.input_after {
        return None;
    }
    let expected_applied = adopted_norm.get(repair.omitted_prefix_len..)?;
    if expected_applied != repair.applied_text {
        return None;
    }

    let mut confirmed = confirmed_len.min(adopted_norm.len());
    while confirmed > 0 && !adopted_norm.is_char_boundary(confirmed) {
        confirmed -= 1;
    }
    let confirmed_applied = if confirmed <= repair.omitted_prefix_len {
        ""
    } else {
        adopted_norm.get(repair.omitted_prefix_len..confirmed)?
    };
    let end = repair.start.checked_add(repair.applied_text.len())?;
    if current_input.get(repair.start..end) != Some(repair.applied_text.as_str()) {
        return None;
    }

    let mut input = current_input.to_string();
    input.replace_range(repair.start..end, confirmed_applied);
    let cursor = clamp_cursor_offset(current_input, cursor_offset);
    let cursor_offset = if cursor <= repair.start {
        cursor
    } else if cursor >= end {
        cursor - repair.applied_text.len() + confirmed_applied.len()
    } else {
        repair.start + (cursor - repair.start).min(confirmed_applied.len())
    };

    Some(InlineAdoptionRepairResult {
        input,
        cursor_offset,
        trimmed_bytes: repair.applied_text.len() - confirmed_applied.len(),
    })
}

/// Disarm the suppressor, repairing optimistically adopted text when
/// the stream fell short of full confirmation.
pub fn finalize_paste_echo(app: &mut AppState, reason: &str) {
    let Some(supp) = app.paste_echo.take() else {
        return;
    };
    if supp.fully_matched() || !supp.repair_on_shortfall() {
        tracing::debug!(
            reason,
            matched = supp.matched_len(),
            expected = supp.expected_norm().len(),
            "paste: echo suppressor disarmed"
        );
        return;
    }
    let Some(repair_target) = supp.repair_target.as_ref() else {
        tracing::warn!(
            reason,
            matched = supp.matched_len(),
            expected = supp.expected_norm().len(),
            "paste: adopted stream ended short but no repair target was recorded"
        );
        return;
    };

    match repair_target {
        AdoptionRepairTarget::TextChip {
            chip_id,
            omitted_prefix_len,
        } => {
            let Some(adopted_norm) = supp.expected_norm().get(*omitted_prefix_len..) else {
                return;
            };
            let confirmed_len = supp
                .matched_len()
                .saturating_sub(*omitted_prefix_len)
                .min(adopted_norm.len());
            match repair_overadopted_text_chip(OveradoptedChipRepairInput {
                current_input: &app.input,
                cursor_offset: app.cursor_offset,
                pasted_contents: &app.pasted_contents,
                chip_id: *chip_id,
                adopted_norm,
                confirmed_len,
            }) {
                Some(repair) => {
                    tracing::info!(
                        reason,
                        chip_id,
                        trimmed_bytes = repair.trimmed_bytes,
                        confirmed = supp.matched_len(),
                        adopted = supp.expected_norm().len(),
                        "paste: adopted stream ended short; trimmed over-adopted chip suffix"
                    );
                    app.input = repair.input;
                    app.cursor_offset = repair.cursor_offset;
                    if let Some(chip) = app.pasted_contents.iter_mut().find(|c| c.id == *chip_id) {
                        chip.content = repair.updated_content_text;
                    }
                }
                None => {
                    tracing::debug!(
                        reason,
                        chip_id,
                        "paste: echo shortfall repair found no chip suffix to trim"
                    );
                }
            }
        }
        AdoptionRepairTarget::Inline(repair_target) => {
            match repair_overadopted_inline_text(
                &app.input,
                app.cursor_offset,
                supp.expected_norm(),
                supp.matched_len(),
                repair_target,
            ) {
                Some(repair) => {
                    tracing::info!(
                        reason,
                        trimmed_bytes = repair.trimmed_bytes,
                        confirmed = supp.matched_len(),
                        adopted = supp.expected_norm().len(),
                        "paste: adopted stream ended short; trimmed over-adopted inline suffix"
                    );
                    app.input = repair.input;
                    app.cursor_offset = repair.cursor_offset;
                }
                None => {
                    tracing::debug!(
                        reason,
                        "paste: inline adoption changed before shortfall repair; leaving it untouched"
                    );
                }
            }
        }
    }
}

/// Result of routing a key event through the armed suppressor.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyEchoHook {
    /// Key was part of the echo stream — drop it.
    Swallow,
    /// Key is real user input — the suppressor has been finalized;
    /// process the key normally.
    PassThrough,
}

/// Route a key event through the armed suppressor. Plain text keys
/// (char / Enter / Tab) may continue the echo stream; anything else —
/// or any mismatch — finalizes the suppressor (repairing if needed)
/// and lets the key process normally.
pub fn paste_echo_key_hook(app: &mut AppState, key: &KeyEvent) -> KeyEchoHook {
    let ch = match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            Some(c)
        }
        KeyCode::Enter => Some('\r'),
        KeyCode::Tab if key.modifiers.is_empty() => Some('\t'),
        _ => None,
    };
    if let Some(c) = ch {
        if let Some(supp) = app.paste_echo.as_mut() {
            match supp.consume_key_char(c, Instant::now()) {
                KeyEcho::Swallowed => return KeyEchoHook::Swallow,
                KeyEcho::Done => {
                    tracing::debug!("paste: echo fully confirmed via key straggler");
                    app.paste_echo = None;
                    return KeyEchoHook::Swallow;
                }
                KeyEcho::Mismatch => {}
            }
        }
    }
    finalize_paste_echo(app, "key_mismatch");
    KeyEchoHook::PassThrough
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::runner::paste_burst::apply_paste_to_app;

    fn stream_supp(expected: &str, matched: usize) -> PasteEchoSuppressor {
        PasteEchoSuppressor::for_stream_adoption(
            expected.to_string(),
            matched,
            Some(AdoptionRepairTarget::TextChip {
                chip_id: 1,
                omitted_prefix_len: 0,
            }),
            Instant::now(),
        )
    }

    #[test]
    fn consume_paste_chunk_swallows_then_completes() {
        let mut supp = stream_supp("abcdef", 2);
        let now = Instant::now();
        assert_eq!(supp.consume_paste_chunk("cd", now), EchoConsume::Swallowed);
        assert_eq!(supp.consume_paste_chunk("ef", now), EchoConsume::Done);
        assert!(supp.fully_matched());
    }

    #[test]
    fn consume_paste_chunk_diverges_with_partial_prefix() {
        let mut supp = stream_supp("abcdef", 2);
        let now = Instant::now();
        match supp.consume_paste_chunk("cdXY", now) {
            EchoConsume::Diverged { tail_norm } => assert_eq!(tail_norm, "XY"),
            other => panic!("expected Diverged, got {other:?}"),
        }
        assert_eq!(supp.matched_len(), 4);
        assert!(!supp.fully_matched());
    }

    #[test]
    fn consume_paste_chunk_overrun_confirms_remainder() {
        let mut supp = stream_supp("abcd", 2);
        let now = Instant::now();
        match supp.consume_paste_chunk("cdEXTRA", now) {
            EchoConsume::Diverged { tail_norm } => assert_eq!(tail_norm, "EXTRA"),
            other => panic!("expected Diverged, got {other:?}"),
        }
        assert!(supp.fully_matched());
    }

    #[test]
    fn consume_paste_chunk_divergence_snaps_to_char_boundary() {
        // Expected and chunk share the first bytes of a multibyte char
        // but then diverge inside it — matched must stay on a boundary.
        let mut supp = stream_supp("你好", 0);
        let now = Instant::now();
        match supp.consume_paste_chunk("你妙", now) {
            EchoConsume::Diverged { tail_norm } => assert_eq!(tail_norm, "妙"),
            other => panic!("expected Diverged, got {other:?}"),
        }
        assert_eq!(supp.matched_len(), "你".len());
    }

    #[test]
    fn consume_key_char_matches_newline_tab_and_chars() {
        let mut supp = stream_supp("a\n    b", 0);
        let now = Instant::now();
        assert_eq!(supp.consume_key_char('a', now), KeyEcho::Swallowed);
        assert_eq!(supp.consume_key_char('\r', now), KeyEcho::Swallowed);
        assert_eq!(supp.consume_key_char('\t', now), KeyEcho::Swallowed);
        assert_eq!(supp.consume_key_char('b', now), KeyEcho::Done);
    }

    #[test]
    fn a_replay_that_dropped_characters_is_resynced_not_abandoned() {
        // What the console actually did: `</span>` came back as `</s>`.
        let mut supp = stream_supp("prefix</span> and a good deal more text after it", 6);
        let now = Instant::now();
        for ch in "</s".chars() {
            assert_eq!(supp.consume_key_char(ch, now), KeyEcho::Swallowed);
        }
        // `>` is not what comes next — `pan>` is — but it is a few bytes
        // ahead, so the replay picks back up instead of being declared
        // divergent and taking the adopted text down with it.
        assert_eq!(supp.consume_key_char('>', now), KeyEcho::Swallowed);
        for ch in " and a good".chars() {
            assert_eq!(
                supp.consume_key_char(ch, now),
                KeyEcho::Swallowed,
                "the rest of the replay keeps matching after the re-sync"
            );
        }
    }

    #[test]
    fn a_replay_of_different_content_still_diverges() {
        let mut supp = stream_supp("prefix and then a long stretch of expected text", 6);
        let now = Instant::now();
        // Nothing within reach looks like this.
        assert_eq!(supp.consume_key_char('中', now), KeyEcho::Mismatch);
    }

    #[test]
    fn resync_never_looks_past_its_window() {
        let far = format!("prefix{}X", "y".repeat(ECHO_RESYNC_WINDOW + 10));
        let mut supp = stream_supp(&far, 6);
        assert_eq!(
            supp.consume_key_char('X', Instant::now()),
            KeyEcho::Mismatch,
            "an X that far ahead is not a dropped run, it is different content"
        );
    }

    #[test]
    fn consume_key_char_mismatch_does_not_advance() {
        let mut supp = stream_supp("abc", 0);
        let now = Instant::now();
        assert_eq!(supp.consume_key_char('x', now), KeyEcho::Mismatch);
        assert_eq!(supp.matched_len(), 0);
    }

    #[test]
    fn direct_key_mode_never_matches_keys() {
        let mut supp = PasteEchoSuppressor::for_direct_key_paste("abc".to_string(), Instant::now());
        assert_eq!(
            supp.consume_key_char('a', Instant::now()),
            KeyEcho::Mismatch
        );
    }

    #[test]
    fn direct_key_mode_swallows_paste_echo() {
        let mut supp =
            PasteEchoSuppressor::for_direct_key_paste("hello\nworld".to_string(), Instant::now());
        assert_eq!(
            supp.consume_paste_chunk("hello\nworld", Instant::now()),
            EchoConsume::Done
        );
    }

    #[test]
    fn idle_expiry_uses_last_activity() {
        let start = Instant::now();
        let supp = stream_supp("abc", 0);
        assert!(!supp.is_idle_expired(start));
        assert!(supp.is_idle_expired(start + STREAM_ADOPTION_IDLE + Duration::from_millis(1)));
    }

    // ── plan_clipboard_adoption ───────────────────────────────────

    #[test]
    fn adoption_equal_chunk_is_complete() {
        assert_eq!(
            plan_clipboard_adoption("same text", "same text".to_string()),
            AdoptionPlan::Complete
        );
    }

    #[test]
    fn adoption_long_prefix_adopts_clipboard() {
        let chunk = "x".repeat(MIN_ADOPTION_PREFIX_BYTES);
        let clipboard = format!("{chunk}TAIL");
        match plan_clipboard_adoption(&chunk, clipboard.clone()) {
            AdoptionPlan::AdoptClipboard {
                clipboard_norm,
                matched,
            } => {
                assert_eq!(clipboard_norm, clipboard);
                assert_eq!(matched, chunk.len());
            }
            other => panic!("expected AdoptClipboard, got {other:?}"),
        }
    }

    #[test]
    fn adoption_short_prefix_declines() {
        let chunk = "x".repeat(MIN_ADOPTION_PREFIX_BYTES - 1);
        let clipboard = format!("{chunk}TAIL");
        assert_eq!(
            plan_clipboard_adoption(&chunk, clipboard),
            AdoptionPlan::None
        );
    }

    #[test]
    fn adoption_mismatch_declines() {
        let chunk = "y".repeat(MIN_ADOPTION_PREFIX_BYTES);
        assert_eq!(
            plan_clipboard_adoption(&chunk, "completely different".to_string()),
            AdoptionPlan::None
        );
    }

    #[test]
    fn adoption_empty_inputs_decline() {
        assert_eq!(
            plan_clipboard_adoption("", "clip".to_string()),
            AdoptionPlan::None
        );
        assert_eq!(
            plan_clipboard_adoption("chunk", String::new()),
            AdoptionPlan::None
        );
    }

    #[test]
    fn clipboard_adoption_with_equal_normalized_text_is_complete() {
        assert!(matches!(
            try_adopt_clipboard_for_first_chunk_with("same\n", || Some("same\r\n".to_string())),
            ClipboardAdoption::Complete
        ));
    }

    #[test]
    fn clipboard_adoption_with_long_prefix_returns_raw_and_normalized_clipboard() {
        let chunk = "x".repeat(MIN_ADOPTION_PREFIX_BYTES);
        let clipboard_raw = format!("{chunk}\r\nTAIL");
        let expected_norm = format!("{chunk}\nTAIL");

        match try_adopt_clipboard_for_first_chunk_with(&chunk, || Some(clipboard_raw.clone())) {
            ClipboardAdoption::Adopt {
                clipboard_raw: actual_raw,
                clipboard_norm,
                matched,
            } => {
                assert_eq!(actual_raw, clipboard_raw);
                assert_eq!(clipboard_norm, expected_norm);
                assert_eq!(matched, chunk.len());
            }
            _ => panic!("expected clipboard adoption"),
        }
    }

    #[test]
    fn clipboard_adoption_with_mismatched_clipboard_declines() {
        let chunk = "x".repeat(MIN_ADOPTION_PREFIX_BYTES);
        assert!(matches!(
            try_adopt_clipboard_for_first_chunk_with(&chunk, || Some("y".repeat(512))),
            ClipboardAdoption::None
        ));
    }

    #[test]
    fn clipboard_adoption_with_unavailable_clipboard_declines() {
        let chunk = "x".repeat(MIN_ADOPTION_PREFIX_BYTES);
        assert!(matches!(
            try_adopt_clipboard_for_first_chunk_with(&chunk, || None),
            ClipboardAdoption::None
        ));
    }

    #[test]
    fn image_path_clipboard_matches_observed_stream_prefix() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let image_path = temp.path().join("clip.png");
        std::fs::write(&image_path, b"image").expect("write image placeholder");
        let clipboard = image_path.to_string_lossy().into_owned();
        let prefix: String = clipboard.chars().take(2).collect();

        assert!(clipboard_image_path_matches_prefix_with(&prefix, || {
            Some(clipboard.clone())
        }));
        assert!(!clipboard_image_path_matches_prefix_with("zz", || {
            Some(clipboard.clone())
        }));
        assert!(!clipboard_image_path_matches_prefix_with("pl", || {
            Some("plain text".to_string())
        }));
    }

    #[test]
    fn text_adoption_declines_valid_image_path() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let image_path = temp.path().join("clip.png");
        std::fs::write(&image_path, b"image").expect("write image placeholder");
        let clipboard = image_path.to_string_lossy().into_owned();

        assert!(matches!(
            try_adopt_clipboard_for_first_chunk_with(&clipboard, || Some(clipboard.clone())),
            ClipboardAdoption::None
        ));
    }

    #[test]
    fn finalize_repairs_short_inline_adoption() {
        let input_before = "leftright";
        let cursor_before = 4;
        let adopted = "x".repeat(520);
        let confirmed_len = 440;
        let input_after = format!("left{adopted}right");
        let repair_target = inline_adoption_repair_target(
            input_before,
            cursor_before,
            &input_after,
            cursor_before + adopted.len(),
            &adopted,
        );
        let mut app = AppState::default();
        app.input = input_after;
        app.cursor_offset = cursor_before + adopted.len();
        app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
            adopted.clone(),
            confirmed_len,
            repair_target,
            Instant::now(),
        ));

        finalize_paste_echo(&mut app, "test");

        assert_eq!(app.input, format!("left{}right", &adopted[..confirmed_len]));
        assert_eq!(app.cursor_offset, cursor_before + confirmed_len);
    }

    #[test]
    fn finalize_repairs_inline_adoption_after_bash_marker_was_stripped() {
        let adopted = format!("!{}", "x".repeat(519));
        let applied = &adopted[1..];
        let confirmed_len = 400;
        let repair_target = inline_adoption_repair_target("", 0, applied, applied.len(), &adopted);
        let mut app = AppState::default();
        app.input = applied.to_string();
        app.cursor_offset = applied.len();
        app.mode = "bash".to_string();
        app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
            adopted.clone(),
            confirmed_len,
            repair_target,
            Instant::now(),
        ));

        finalize_paste_echo(&mut app, "test");

        assert_eq!(app.input, adopted[1..confirmed_len]);
        assert_eq!(app.cursor_offset, confirmed_len - 1);
        assert_eq!(app.mode, "bash");
    }

    #[test]
    fn finalize_repairs_new_bash_chip_when_old_unstripped_chip_matches() {
        let adopted = format!("!{}", "x".repeat(1_100));
        let applied = adopted[1..].to_string();
        let confirmed_len = 900;
        let mut app = AppState::default();
        app.pasted_contents.push(rebon_types::PromptPasteContent {
            id: 6,
            kind: "text".to_string(),
            content: adopted.clone(),
            media_type: None,
            filename: None,
            source_path: None,
        });
        app.next_paste_id = 7;
        let text_chips_before = snapshot_text_chips(&app);
        apply_paste_to_app(&mut app, adopted.clone(), 30);
        assert_eq!(app.input, "[Pasted text #7]");
        assert_eq!(app.mode, "bash");
        assert_eq!(app.pasted_contents[1].content, applied);
        let repair_target =
            find_adopted_text_repair_target(&app, &text_chips_before, "", 0, &adopted);
        assert!(matches!(
            repair_target,
            Some(AdoptionRepairTarget::TextChip {
                chip_id: 7,
                omitted_prefix_len: 1,
            })
        ));
        app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
            adopted,
            confirmed_len,
            repair_target,
            Instant::now(),
        ));

        finalize_paste_echo(&mut app, "test");

        assert_eq!(
            app.pasted_contents[0].content,
            format!("!{}", "x".repeat(1_100))
        );
        assert_eq!(app.pasted_contents[1].content, applied[..confirmed_len - 1]);
        assert_eq!(app.input, "[Pasted text #7]");
    }

    #[test]
    fn finalize_leaves_inline_adoption_untouched_after_prompt_change() {
        let adopted = "x".repeat(520);
        let repair_target = inline_adoption_repair_target("", 0, &adopted, adopted.len(), &adopted);
        let mut app = AppState::default();
        app.input = format!("{adopted}edited");
        app.cursor_offset = app.input.len();
        app.paste_echo = Some(PasteEchoSuppressor::for_stream_adoption(
            adopted.clone(),
            400,
            repair_target,
            Instant::now(),
        ));

        let unchanged = app.input.clone();
        finalize_paste_echo(&mut app, "test");

        assert_eq!(app.input, unchanged);
    }
}
