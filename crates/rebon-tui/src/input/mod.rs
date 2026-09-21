//! Text-input widget decision logic.
//!
//! This module was a standalone crate before the merge: its only
//! consumers were this crate's `render_prompt_input` and the terminal
//! half, so the crate boundary bought a Cargo entry and
//! nothing else. Nothing here names ratatui, and nothing should start.
//!
//! This module implements the pure decision logic behind the
//! text-input widget trio:
//!
//! * the main text input (placeholder, mask, multiline, optional
//!   voice waveform cursor);
//! * the shared rendering shell consumed by both the plain and
//!   vim-mode inputs (placeholder routing, highlight viewport
//!   remapping, paste gating, argument-hint detection);
//! * the vim-mode variant that wraps the shell and keeps its mode in
//!   sync with an external `initial_mode` input.
//!
//! Every module pins its behaviour with a comprehensive test table.
//!
//! ## What is in this module
//!
//! * [`voice_cursor`] — the mini-waveform cursor computation: EMA
//!   smoothing, silence threshold, bar index selection, hue-to-RGB
//!   colour pick, and the "accessibility disables the cursor"
//!   short-circuit. The `BARS` constant, `SMOOTH`, `LEVEL_BOOST`,
//!   and `SILENCE_THRESHOLD` are pinned exactly. The HSL-to-RGB
//!   mapping is defined locally because it is only used here.
//! * [`placeholder`] — the placeholder-decision pure function:
//!   `show_placeholder` plus the `rendered_placeholder` variants.
//!   The actual ANSI dimming / inverse styling is modeled as a
//!   [`placeholder::PlaceholderStyle`] enum so consumers can render
//!   it however they like (ANSI escapes, ratatui, plain stdout).
//! * [`highlight_viewport`] — the cursor-aware highlight filter.
//!   Drops highlights the cursor is inside
//!   (unless `dim_color`), clips them to the visible viewport window,
//!   and remaps offsets into viewport-local coordinates.
//! * [`argument_hint`] — the slash-command argument-hint detector:
//!   whether to show an argument hint
//!   based on value starting with `/`, having no arguments yet, and a
//!   non-empty `argument_hint`.
//! * [`paste_gate`] — the paste-return gate: the "swallow Return
//!   while pasting" check that prevents a pasted newline from
//!   submitting.
//! * [`vim_mode_sync`] — the initial-mode sync predicate: the pure
//!   decision of "should the consumer call `set_mode(initial_mode)`
//!   given current mode".
//!
//! ## Outbound seam shapes
//!
//! Surrounding subsystems are modelled as data, not dependencies:
//!
//! 1. **Cursor/buffer state machine** (plain and vim-mode input
//!    state) → out of scope. This crate takes its output shape as a
//!    caller-owned [`BaseInputState`] struct: `rendered_value`,
//!    `cursor_line`, `cursor_column`, `viewport_char_offset`,
//!    `viewport_char_end`.
//!    Consumers feed these in from whichever state source they own.
//!
//! 2. **Paste handler** → modelled as the pure
//!    predicate in [`paste_gate`]. The consumer owns the
//!    `is_pasting: bool` slot; this crate just tells it whether to
//!    swallow a specific event.
//!
//! 3. **Keyboard subscription** → out of scope. This crate does
//!    not subscribe to keyboard events; the consumer routes events to
//!    whichever downstream reducer owns them.
//!
//! 4. **Voice / audio level subsystem** (voice state, audio level
//!    buffer, animation frames) → modelled as plain owned inputs
//!    to [`voice_cursor::compute_waveform_cursor`]. The consumer
//!    reads the voice state and passes a `VoiceCursorInput` struct.
//!
//! 5. **Rendering plumbing** (declared cursor, styled text,
//!    highlighted input, theme, terminal focus) → out of scope. The
//!    crate returns plain structs / enums that
//!    a renderer maps into its chosen primitives.
//!
//! ## Out of scope
//!
//! * **The cursor state machine** — every cursor mutation (insert
//!   char, delete, arrow keys, word-motion, kill ring, yank,
//!   multi-line navigation, viewport clamp) lives elsewhere. This
//!   crate only consumes the already-projected `BaseInputState`.
//! * **The full text-input state composition** — double-press
//!   handling, notifications, history, kill ring, yank pop, input
//!   filter, inline ghost text, and feature flags.
//! * **The vim-mode state machine** (normal / insert / replace
//!   modes, `.` repeat, `d`/`y`/`c` operator-pending state, count
//!   prefixes, visual mode). This crate only implements the tiny
//!   initial-mode sync decision that the vim input shell owns.
//! * **The full paste handler** — the bracketed-paste decoder, timer,
//!   image-paste recognition, and pasted-contents state. This crate
//!   only implements the single-line "swallow Return while pasting"
//!   gate that the input shell imposes.
//! * **Placeholder styling** — the *decision shape*
//!   (`show_placeholder`, `rendered_placeholder` variants) lives
//!   here; the ANSI styling is abstracted to a
//!   [`placeholder::PlaceholderStyle`] enum so the renderer can
//!   apply dim/inverse however it likes.
//! * **Rendering primitives and lifecycle plumbing** — the crate
//!   produces plain owned values, not widgets, and performs no
//!   effect / memoization bookkeeping.
//! * **ANSI colour rendering**. The voice cursor returns a
//!   `(u8, u8, u8)` RGB tuple + the chosen glyph `char`; the consumer
//!   colours it.
//! * **Reading the accessibility setting**. The
//!   `accessibility_enabled: bool` is a caller-resolved input.
//!
//! These are deliberately left out rather than carried as stubs:
//! *misaligned stubs have negative value* — they hint at the wrong
//! API and force downstream consumers to either preserve the mistake
//! or do a disruptive rename.
//!
//! ## Dependency policy
//!
//! This module names **no other module in this crate**, and its only
//! consumers are this crate's `render_prompt_input` and the terminal half:
//! every shape, every glyph
//! constant, every projection is owned right here. `rebon-customselect`
//! (which has an `OptionType::Input` row variant driven by its `select_input_option`
//! module) does not import from here either; the contract is
//! intentionally duplicated on both sides to keep that crate
//! dependency-free.

#![deny(missing_docs)]

pub mod argument_hint;
pub mod full_width_digit;
pub mod highlight_viewport;
pub mod paste_gate;
pub mod placeholder;
pub mod vim_mode_sync;
pub mod voice_cursor;

pub use argument_hint::{should_show_argument_hint, ArgumentHintInput};
pub use full_width_digit::normalize_full_width_digit;
pub use highlight_viewport::{filter_and_remap_highlights, HighlightViewportInput, TextHighlight};
pub use paste_gate::{should_swallow_event, PasteGateEvent};
pub use placeholder::{
    render_placeholder, PlaceholderDecision, PlaceholderInput, PlaceholderStyle,
};
pub use vim_mode_sync::{needs_mode_sync, VimMode};
pub use voice_cursor::{
    compute_waveform_cursor, hue_to_rgb, VoiceCursor, VoiceCursorInput, BARS,
    CURSOR_WAVEFORM_WIDTH, LEVEL_BOOST, SILENCE_THRESHOLD, SMOOTH,
};

/// The base text-input state projection that the input shell
/// consumes from the caller's cursor/buffer state source. Only the
/// fields the shell actually reads are surfaced: `rendered_value`,
/// `cursor_line`, `cursor_column`, `viewport_char_offset`,
/// `viewport_char_end`.
///
/// This crate is a pure projection — there is no input callback or
/// memoization plumbing here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseInputState {
    /// The already-styled (ANSI-dimmed mask / highlight / cursor
    /// overlay) string the renderer should draw. The caller produces
    /// this from its underlying cursor state.
    pub rendered_value: String,
    /// Zero-based display-line index of the cursor.
    pub cursor_line: usize,
    /// Zero-based display-column index of the cursor.
    pub cursor_column: usize,
    /// Character offset of the left edge of the visible viewport
    /// into `original_value`. When zero the whole value is visible;
    /// otherwise the renderer has horizontally scrolled.
    pub viewport_char_offset: usize,
    /// Character offset of the right edge of the visible viewport
    /// into `original_value`. Used to clip highlights.
    pub viewport_char_end: usize,
}

#[cfg(test)]
mod compatibility {
    /// The BaseInputState projection is plain data — no hidden
    /// callback slots, no RefCell, no Rc. A renderer can `Clone` it
    /// freely.
    #[test]
    fn base_input_state_is_plain_data() {
        let s = super::BaseInputState {
            rendered_value: String::from("hi"),
            cursor_line: 0,
            cursor_column: 2,
            viewport_char_offset: 0,
            viewport_char_end: 2,
        };
        let s2 = s.clone();
        assert_eq!(s, s2);
    }
}
