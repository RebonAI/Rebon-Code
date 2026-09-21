//! Fullscreen layout-zone projection.
//!
//! [`layout_zones`] takes a [`LayoutInput`] — the zone-presence flags
//! (`has_scrollable`, `has_bottom`, `has_overlay`, `has_bottom_float`,
//! `has_modal`) plus the gating flags (`hide_pill`, `hide_sticky`,
//! `new_message_count`) — and decides:
//!
//! 1. **Sticky header** is shown iff `!hide_sticky`, the prompt is
//!    `StickyPrompt::Visible` (neither `None` nor `Clicked`), and
//!    `has_overlay` is false.
//! 2. **`pad_collapsed`** is true when the sticky prompt is present and
//!    there is no overlay. When true, the scroll region's padding top
//!    drops from 1 to 0 (the sticky header takes the row that the
//!    padding would have held).
//! 3. **Pill** is shown iff `!hide_pill && pill_visible && !has_overlay`
//!    (delegated to [`super::unseen_divider::pill_visible_gate`]).
//! 4. **`bottom_float`** is rendered iff `has_bottom_float` (no gating).
//! 5. **Modal pane** is shown iff `has_modal`. The modal context
//!    provides `rows - MODAL_TRANSCRIPT_PEEK - 1` and `columns - 4`.
//! 6. **Non-fullscreen mode** collapses everything into a sequential
//!    scrollable → bottom → overlay → modal stack.
//!
//! Hyperlink clicks route `file://` URLs to a path opener and
//! everything else to a browser opener. We model that as a
//! [`HyperlinkAction`] enum + [`dispatch_hyperlink`] function.
//!
//! This module takes a fully-resolved input bag and returns a
//! fully-resolved [`LayoutZones`] output. The consumer decides what
//! to render for each zone.

use super::unseen_divider::{pill_visible_gate, PillSuppression};
use super::MODAL_TRANSCRIPT_PEEK;

/// Sticky prompt state. Three shapes matter:
///
/// * `None` → no sticky prompt.
/// * `Clicked` → user clicked the pill/sticky → hide.
/// * `Visible { text }` → show the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StickyPrompt {
    /// No sticky prompt.
    None,
    /// User clicked — treat as absent but keep the flag so the pad
    /// math still collapses.
    Clicked,
    /// An actual text.
    Visible { text: String },
}

impl StickyPrompt {
    pub fn is_none(&self) -> bool {
        matches!(self, StickyPrompt::None)
    }
    pub fn is_clicked(&self) -> bool {
        matches!(self, StickyPrompt::Clicked)
    }
    pub fn is_visible(&self) -> bool {
        matches!(self, StickyPrompt::Visible { .. })
    }
}

/// Inputs to the layout projection.
#[derive(Debug, Clone)]
pub struct LayoutInput {
    /// Whether the fullscreen environment is enabled — true → fullscreen
    /// layout, false → the sequential scrollable/bottom/overlay/modal
    /// passthrough.
    pub fullscreen_enabled: bool,

    pub terminal_rows: u16,
    pub terminal_columns: u16,

    pub sticky: StickyPrompt,
    pub hide_sticky: bool,

    pub has_scrollable: bool,
    pub has_overlay: bool,
    pub has_bottom: bool,
    pub has_bottom_float: bool,
    pub has_modal: bool,

    pub hide_pill: bool,
    pub pill_visible: bool,
    pub new_message_count: u64,

    pub has_suggestions_overlay: bool,
    pub has_dialog_overlay: bool,
}

/// Output — a semantic projection of each zone's visibility and
/// per-zone numeric settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutZones {
    /// `true` when the consumer should render the sequential
    /// non-fullscreen passthrough. The other fields are ignored.
    pub passthrough: bool,

    /// Whether the sticky prompt header row should show.
    pub show_sticky_header: bool,
    /// The text to show in the sticky header (only present when
    /// `show_sticky_header == true`).
    pub sticky_header_text: Option<String>,
    /// Whether the main scroll region's top padding drops from 1 to 0.
    pub pad_collapsed: bool,
    /// Concrete top padding: `0` when `pad_collapsed`, `1` otherwise.
    pub scroll_padding_top: u16,

    /// Whether the "N new messages" pill should render.
    pub show_pill: bool,
    /// The count passed to the pill (0 → "Jump to bottom").
    pub pill_count: u64,

    /// Whether the bottom-float region should render.
    pub show_bottom_float: bool,

    /// Whether the modal pane should render.
    pub show_modal: bool,
    /// Rows available inside the modal pane (when `show_modal`).
    pub modal_rows: u16,
    /// Columns available inside the modal pane (when `show_modal`).
    pub modal_columns: u16,
    /// Max pixel height of the modal pane outer box.
    pub modal_max_height: u16,

    /// Whether the bottom bar area should render at all. Always `true`
    /// in fullscreen mode (it contains suggestions + dialog overlays
    /// even when `bottom` itself is empty).
    pub show_bottom_bar: bool,

    /// Whether the suggestions overlay should render inside the
    /// bottom bar.
    pub show_suggestions_overlay: bool,
    /// Whether the dialog overlay should render inside the bottom bar.
    pub show_dialog_overlay: bool,
}

/// Main projection.
pub fn layout_zones(input: &LayoutInput) -> LayoutZones {
    if !input.fullscreen_enabled {
        return LayoutZones {
            passthrough: true,
            show_sticky_header: false,
            sticky_header_text: None,
            pad_collapsed: false,
            scroll_padding_top: 0,
            show_pill: false,
            pill_count: 0,
            show_bottom_float: false,
            show_modal: false,
            modal_rows: 0,
            modal_columns: 0,
            modal_max_height: 0,
            show_bottom_bar: false,
            show_suggestions_overlay: false,
            show_dialog_overlay: false,
        };
    }

    // Sticky rules:
    //
    //   * `hide_sticky` forces the prompt to `StickyPrompt::None`.
    //   * the header row shows only for `StickyPrompt::Visible` with no
    //     overlay, so `Clicked` suppresses the row while still counting
    //     as present below.
    //   * `pad_collapsed` needs only that the prompt is present and
    //     there is no overlay.
    let sticky: &StickyPrompt = if input.hide_sticky {
        &StickyPrompt::None
    } else {
        &input.sticky
    };
    let sticky_is_present = !sticky.is_none();
    let show_sticky_header = sticky.is_visible() && !input.has_overlay;
    let sticky_header_text = if show_sticky_header {
        match sticky {
            StickyPrompt::Visible { text } => Some(text.clone()),
            _ => None,
        }
    } else {
        None
    };
    let pad_collapsed = sticky_is_present && !input.has_overlay;
    let scroll_padding_top = if pad_collapsed { 0 } else { 1 };

    let show_pill = pill_visible_gate(PillSuppression {
        hide_pill: input.hide_pill,
        has_overlay: input.has_overlay,
        pill_visible: input.pill_visible,
    });

    let show_bottom_float = input.has_bottom_float;

    let show_modal = input.has_modal;
    // rows - MODAL_TRANSCRIPT_PEEK - 1, saturating.
    let modal_rows = input
        .terminal_rows
        .saturating_sub(MODAL_TRANSCRIPT_PEEK)
        .saturating_sub(1);
    let modal_columns = input.terminal_columns.saturating_sub(4);
    let modal_max_height = input.terminal_rows.saturating_sub(MODAL_TRANSCRIPT_PEEK);

    LayoutZones {
        passthrough: false,
        show_sticky_header,
        sticky_header_text,
        pad_collapsed,
        scroll_padding_top,
        show_pill,
        pill_count: input.new_message_count,
        show_bottom_float,
        show_modal,
        modal_rows,
        modal_columns,
        modal_max_height,
        show_bottom_bar: true,
        show_suggestions_overlay: input.has_suggestions_overlay,
        show_dialog_overlay: input.has_dialog_overlay,
    }
}

/// A click on a hyperlink inside the rendered layout:
///
/// ```text
/// if url starts with "file:" → open the path locally
/// else → open the URL in the browser
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HyperlinkAction {
    /// Open a local file at the given path (without `file://` prefix).
    OpenPath(String),
    /// Open a URL in the user's browser.
    OpenBrowser(String),
}

/// Dispatch a clicked hyperlink. The consumer plugs in a
/// `file_url_to_path` function because we don't ship URL parsing.
pub fn dispatch_hyperlink(
    url: &str,
    file_url_to_path: impl Fn(&str) -> Option<String>,
) -> HyperlinkAction {
    if url.starts_with("file:") {
        if let Some(path) = file_url_to_path(url) {
            return HyperlinkAction::OpenPath(path);
        }
        // Fallback: a failed conversion is swallowed silently — treat
        // as no-op by dispatching a browser open (the consumer's
        // opener will no-op on bogus URLs).
    }
    HyperlinkAction::OpenBrowser(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> LayoutInput {
        LayoutInput {
            fullscreen_enabled: true,
            terminal_rows: 40,
            terminal_columns: 120,
            sticky: StickyPrompt::None,
            hide_sticky: false,
            has_scrollable: true,
            has_overlay: false,
            has_bottom: true,
            has_bottom_float: false,
            has_modal: false,
            hide_pill: false,
            pill_visible: false,
            new_message_count: 0,
            has_suggestions_overlay: false,
            has_dialog_overlay: false,
        }
    }

    // ---- passthrough ----

    #[test]
    fn passthrough_when_fullscreen_disabled() {
        let mut i = base();
        i.fullscreen_enabled = false;
        let z = layout_zones(&i);
        assert!(z.passthrough);
        // All other fields are defaults / ignored.
        assert!(!z.show_sticky_header);
        assert!(!z.show_pill);
    }

    // ---- sticky header gating ----

    #[test]
    fn sticky_none_hides_header() {
        let i = base();
        let z = layout_zones(&i);
        assert!(!z.show_sticky_header);
        assert_eq!(z.sticky_header_text, None);
        assert!(!z.pad_collapsed);
        assert_eq!(z.scroll_padding_top, 1);
    }

    #[test]
    fn sticky_visible_shows_header() {
        let mut i = base();
        i.sticky = StickyPrompt::Visible {
            text: "hello".into(),
        };
        let z = layout_zones(&i);
        assert!(z.show_sticky_header);
        assert_eq!(z.sticky_header_text, Some("hello".into()));
        assert!(z.pad_collapsed);
        assert_eq!(z.scroll_padding_top, 0);
    }

    #[test]
    fn sticky_clicked_hides_header_but_still_collapses_pad() {
        // The prompt is present but `StickyPrompt::Clicked`, so the
        // header row is suppressed while the padding still collapses.
        let mut i = base();
        i.sticky = StickyPrompt::Clicked;
        let z = layout_zones(&i);
        assert!(!z.show_sticky_header);
        assert!(z.pad_collapsed);
        assert_eq!(z.scroll_padding_top, 0);
    }

    #[test]
    fn hide_sticky_flag_kills_header_and_pad_collapse() {
        let mut i = base();
        i.sticky = StickyPrompt::Visible { text: "x".into() };
        i.hide_sticky = true;
        let z = layout_zones(&i);
        assert!(!z.show_sticky_header);
        assert!(!z.pad_collapsed);
        assert_eq!(z.scroll_padding_top, 1);
    }

    #[test]
    fn overlay_suppresses_sticky_header() {
        let mut i = base();
        i.sticky = StickyPrompt::Visible { text: "x".into() };
        i.has_overlay = true;
        let z = layout_zones(&i);
        assert!(!z.show_sticky_header);
        assert!(!z.pad_collapsed);
    }

    #[test]
    fn overlay_suppresses_clicked_collapse_too() {
        let mut i = base();
        i.sticky = StickyPrompt::Clicked;
        i.has_overlay = true;
        let z = layout_zones(&i);
        assert!(!z.pad_collapsed);
    }

    // ---- pill gating ----

    #[test]
    fn pill_hidden_when_not_visible() {
        let i = base();
        let z = layout_zones(&i);
        assert!(!z.show_pill);
    }

    #[test]
    fn pill_shown_when_visible_and_not_gated() {
        let mut i = base();
        i.pill_visible = true;
        i.new_message_count = 3;
        let z = layout_zones(&i);
        assert!(z.show_pill);
        assert_eq!(z.pill_count, 3);
    }

    #[test]
    fn pill_hidden_by_hide_pill_flag() {
        let mut i = base();
        i.pill_visible = true;
        i.hide_pill = true;
        let z = layout_zones(&i);
        assert!(!z.show_pill);
    }

    #[test]
    fn pill_hidden_by_overlay() {
        let mut i = base();
        i.pill_visible = true;
        i.has_overlay = true;
        let z = layout_zones(&i);
        assert!(!z.show_pill);
    }

    // ---- modal pane ----

    #[test]
    fn no_modal_when_has_modal_false() {
        let i = base();
        let z = layout_zones(&i);
        assert!(!z.show_modal);
    }

    #[test]
    fn modal_rows_and_columns_math() {
        let mut i = base();
        i.has_modal = true;
        i.terminal_rows = 40;
        i.terminal_columns = 120;
        let z = layout_zones(&i);
        assert!(z.show_modal);
        // rows - 2 - 1 = 37
        assert_eq!(z.modal_rows, 37);
        assert_eq!(z.modal_columns, 116);
        // max_height = rows - 2 = 38
        assert_eq!(z.modal_max_height, 38);
    }

    #[test]
    fn modal_rows_saturate_on_tiny_terminal() {
        let mut i = base();
        i.has_modal = true;
        i.terminal_rows = 2;
        i.terminal_columns = 3;
        let z = layout_zones(&i);
        assert_eq!(z.modal_rows, 0);
        assert_eq!(z.modal_columns, 0);
        assert_eq!(z.modal_max_height, 0);
    }

    #[test]
    fn modal_max_height_accounts_for_peek_only() {
        // max_height = rows - MODAL_TRANSCRIPT_PEEK (the `-1` is only
        // inside the modal context).
        let mut i = base();
        i.has_modal = true;
        i.terminal_rows = 10;
        let z = layout_zones(&i);
        assert_eq!(z.modal_max_height, 8);
        assert_eq!(z.modal_rows, 7);
    }

    // ---- bottom float ----

    #[test]
    fn bottom_float_shown_when_present() {
        let mut i = base();
        i.has_bottom_float = true;
        let z = layout_zones(&i);
        assert!(z.show_bottom_float);
    }

    #[test]
    fn bottom_float_absent_when_not_present() {
        let i = base();
        let z = layout_zones(&i);
        assert!(!z.show_bottom_float);
    }

    // ---- bottom bar ----

    #[test]
    fn bottom_bar_always_shown_in_fullscreen() {
        let i = base();
        let z = layout_zones(&i);
        assert!(z.show_bottom_bar);
    }

    #[test]
    fn bottom_bar_absent_in_passthrough() {
        let mut i = base();
        i.fullscreen_enabled = false;
        let z = layout_zones(&i);
        assert!(!z.show_bottom_bar);
    }

    #[test]
    fn suggestions_overlay_flag_threaded() {
        let mut i = base();
        i.has_suggestions_overlay = true;
        let z = layout_zones(&i);
        assert!(z.show_suggestions_overlay);
    }

    #[test]
    fn dialog_overlay_flag_threaded() {
        let mut i = base();
        i.has_dialog_overlay = true;
        let z = layout_zones(&i);
        assert!(z.show_dialog_overlay);
    }

    // ---- hyperlink dispatch ----

    #[test]
    fn dispatch_file_url_resolves_to_path() {
        let a = dispatch_hyperlink("file:///home/foo/bar.txt", |_| {
            Some("/home/foo/bar.txt".into())
        });
        assert_eq!(a, HyperlinkAction::OpenPath("/home/foo/bar.txt".into()));
    }

    #[test]
    fn dispatch_https_opens_browser() {
        let a = dispatch_hyperlink("https://example.com", |_| None);
        assert_eq!(
            a,
            HyperlinkAction::OpenBrowser("https://example.com".into())
        );
    }

    #[test]
    fn dispatch_file_url_fallback_when_conversion_fails() {
        // A conversion that fails is treated as "fall through to the
        // browser", because that can't do harm.
        let a = dispatch_hyperlink("file://bogus", |_| None);
        assert_eq!(a, HyperlinkAction::OpenBrowser("file://bogus".into()));
    }

    #[test]
    fn dispatch_http_opens_browser() {
        let a = dispatch_hyperlink("http://x.com", |_| None);
        assert_eq!(a, HyperlinkAction::OpenBrowser("http://x.com".into()));
    }

    #[test]
    fn dispatch_unknown_scheme_opens_browser() {
        let a = dispatch_hyperlink("slack://channel/123", |_| None);
        assert_eq!(
            a,
            HyperlinkAction::OpenBrowser("slack://channel/123".into())
        );
    }

    // ---- sticky prompt helpers ----

    #[test]
    fn sticky_prompt_is_none_checks() {
        assert!(StickyPrompt::None.is_none());
        assert!(!StickyPrompt::Clicked.is_none());
        assert!(!StickyPrompt::Visible { text: "x".into() }.is_none());
    }

    #[test]
    fn sticky_prompt_is_clicked_checks() {
        assert!(!StickyPrompt::None.is_clicked());
        assert!(StickyPrompt::Clicked.is_clicked());
        assert!(!StickyPrompt::Visible { text: "x".into() }.is_clicked());
    }

    #[test]
    fn sticky_prompt_is_visible_checks() {
        assert!(!StickyPrompt::None.is_visible());
        assert!(!StickyPrompt::Clicked.is_visible());
        assert!(StickyPrompt::Visible { text: "x".into() }.is_visible());
    }

    // ---- passthrough order integration ----

    #[test]
    fn modal_transcript_peek_pinned_at_2() {
        assert_eq!(MODAL_TRANSCRIPT_PEEK, 2);
    }
}
