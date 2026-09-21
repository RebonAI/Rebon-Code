//! # rebon-design-system
//!
//! Pure logic for the terminal UI design system. Nothing in this crate does
//! IO and nothing in it renders: every function takes plain values and
//! returns a small style struct, a string, or an enum that a downstream
//! renderer can draw.
//!
//! ## What lives here
//!
//! * [`theme`] — the six palettes (69 keys each), [`theme::ThemeName`],
//!   [`theme::ThemeSetting`], the [`theme::get_theme`] resolver, and the
//!   process-wide active-theme slot.
//! * [`color`] — raw color literals versus theme keys, and the four raw
//!   prefixes (`rgb(`, `#`, `ansi256(`, `ansi:`).
//! * [`shortcut_hint`] — `"<shortcut> to <action>"` and
//!   `"(<shortcut> to <action>)"`, normalized for a platform.
//! * [`status_icon`] — six statuses, each resolving to a glyph plus a
//!   semantic color token, or to no color at all for the dim states.
//! * [`progress_bar`] — clamped ratio, whole/partial/empty segmentation,
//!   and the eight 1/8th sub-blocks.
//! * [`pane`], [`ratchet`], [`byline`], [`dialog`], [`divider`],
//!   [`themed_box`], [`themed_text`] — pure projections from component
//!   state to the style a renderer draws.
//! * [`loading_state`], [`list_item`], [`fuzzy_picker`], [`tabs`] —
//!   reducers and row projections.
//!
//! Every module pins its behaviour with a test table.
//!
//! ## Where the outside world comes in
//!
//! Everything the design system needs from its host arrives as data or as a
//! callback, never as a dependency:
//!
//! 1. **Colorization** is a `Fn(&str, &str, ColorType) -> String` handed to
//!    [`color::apply_color`]. Writing ANSI escape sequences belongs to the
//!    renderer.
//! 2. **Fuzzy matching** is a `Fn(&str, &str) -> Option<MatchScore>` handed
//!    to [`fuzzy_picker::apply_fuzzy_filter`].
//! 3. **Terminal and layout context** arrives as plain parameters — `rows`,
//!    `columns`, `inside_modal`, `is_visible`. The crate keeps no layout
//!    state of its own.
//!
//! ## What is deliberately absent
//!
//! Rendering primitives and layout measurement; lifecycle wiring (state,
//! focus, keybindings, modal context); animation and cursor blink; terminal
//! size queries; a glyph-library dependency; and any helper that bridges a
//! palette color into an ANSI sequence. Each surface here exposes a reducer
//! plus a display projection instead.
//!
//! That omission is on purpose. A half-matching stub is worse than nothing:
//! it advertises an API that was never meant to exist and leaves callers
//! either building on the mistake or paying for a disruptive rename.
//!
//! ## Dependencies
//!
//! None — not on other `rebon-*` crates and not on any external crate.
//! Palettes, glyphs and formatters are all owned here, so the crate compiles
//! on its own.

pub mod byline;
pub mod color;
pub mod dialog;
pub mod divider;
pub mod fuzzy_picker;
pub mod list_item;
pub mod loading_state;
pub mod pane;
pub mod progress_bar;
pub mod ratchet;
pub mod shortcut_hint;
pub mod status_icon;
pub mod tabs;
pub mod theme;
pub mod themed_box;
pub mod themed_text;

pub use byline::{byline_render, byline_separator_indices, BYLINE_SEPARATOR};
pub use color::{is_raw_color_value, ColorType, RawColorPrefix, RAW_COLOR_PREFIXES};
pub use dialog::{dialog_style, DialogPalette, DialogStyle};
pub use divider::{divider_style, DividerStyle, DIVIDER_GLYPH};
pub use fuzzy_picker::{
    apply_fuzzy_filter, FuzzyPickerEvent, FuzzyPickerState, MatchScore, ScoredItem,
};
pub use list_item::{list_item_row, ListItemRow, ListItemSelection};
pub use loading_state::{LoadingEvent, LoadingState};
pub use pane::{pane_style, PaneStyle};
pub use progress_bar::{progress_bar_segments, ProgressBarSegments, BLOCK_GLYPHS};
pub use ratchet::{ratchet_min_height, RatchetLock, RatchetState};
pub use shortcut_hint::{
    format_shortcut_for_current_platform, format_shortcut_for_platform, format_shortcut_hint,
    format_shortcut_hint_for_platform, format_shortcut_spaced_for_current_platform,
    format_shortcut_spaced_for_platform, ShortcutHint, ShortcutPlatform,
};
pub use status_icon::{status_icon_style, StatusIconKind, StatusIconStyle, StatusSemanticColor};
pub use tabs::{TabsEvent, TabsState};
pub use theme::{get_theme, Theme, ThemeName, ThemeSetting, THEME_NAMES, THEME_SETTINGS};
pub use themed_box::{themed_box_style, BorderStyle, ThemedBoxStyle};
pub use themed_text::{themed_text_style, ThemedTextStyle};
