//! Layout and chrome projection logic — the ratatui-free half of the
//! terminal's frame.
//!
//! This module was a standalone crate before the merge. It was its
//! own crate for the same reason every helper crate here was: the logic
//! is pure and testable on its own. But the terminal runner was its only
//! consumer, and every one of those consumers lives under its `src/tui/`,
//! so the crate boundary bought a Cargo entry and
//! nothing else. Nothing in this module names ratatui; keeping it that
//! way is what lets the app-side frame reuse the same rules later.
//!
//! This module covers the layout surface of the TUI:
//!
//! * The search-box input chrome that projects a query / placeholder /
//!   cursor into a display row.
//! * The AWS authentication status decision tree (visible / hidden,
//!   error / output / both).
//! * The header shown when viewing a teammate transcript (name + task
//!   + esc hint).
//! * The resume-picker tag-tab windowing reducer (truncation, overflow
//!   arrows, selected-centered window).
//! * The fullscreen layout wrapper for the REPL: the unseen-divider
//!   state machine, the new-messages-pill label projection, the
//!   layout-zone projection (header / scrollable / bottom / modal /
//!   overlay / bottom-float), the modal pane suppression rules, and
//!   the `MODAL_TRANSCRIPT_PEEK` content-height math.
//!
//! Every module pins its behaviour with tests.
//!
//! ## Context: the rebon-tui retrospective
//!
//! An earlier cut of this crate shipped a `layout.rs` that was
//! **deliberately removed** (see the crate-level docs in
//! `rebon-tui/src/lib.rs`). The retrospective flagged three rules as
//! unaudited:
//!
//! 1. **Divider-y race ordering** — the order of `on_scroll_away` and
//!    `on_repin`, a wheel event racing a divider-position update, and
//!    the deferred re-pin.
//! 2. **Modal pane suppression** — whether the divider / pill show
//!    while a modal pane is up.
//! 3. **Assistant-turn semantics** — what counts as a "turn" when the
//!    pill increments (raw assistant entries vs visible-text-only).
//!
//! And a fourth rule was flagged:
//!
//! 4. **"1 new" floor rule** — whether
//!    [`unseen_divider::compute_unseen_divider`] clamps
//!    the displayed count to `max(1, turns)`, and under what
//!    preconditions.
//!
//! **This module implements all four.** Each rule has a positive,
//! negative, and edge test. The [`unseen_divider`] module is the
//! centerpiece: state transitions are exposed as pure methods on an
//! [`unseen_divider::UnseenDividerState`] struct — the deferred-clear
//! ordering that avoids the state-update race is encoded explicitly as
//! `on_repin` + `settle`.
//!
//! ## Modules
//!
//! * [`search_box`] — [`search_box::SearchBoxDisplay`] projection with
//!   the four-way (focused/terminal-focused × with/without query)
//!   cursor-aware rendering. Defaults (`Search…` placeholder, `⌕`
//!   prefix, `borderless = false`) pinned.
//! * [`aws_auth_status_box`] — decision tree for the AWS auth box. Pure
//!   projection from `AwsAuthStatus` → `Option<AwsAuthBoxDisplay>`,
//!   plus the last-5 output tail and the URL split (`before` + `url`
//!   + `after`).
//! * [`teammate_view_header`] — [`teammate_view_header::TeammateViewHeaderDisplay`]
//!   — null vs visible gate, name color pass-through, esc hint text.
//! * [`tag_tabs`] — [`tag_tabs::TagTabsLayout`] windowing reducer:
//!   clamp selected index, compute per-tab widths, expand window
//!   around selected, compute `hidden_left` / `hidden_right`, emit
//!   visible-tab display rows with selected-highlight flags.
//! * [`unseen_divider`] — the four divider rules above. Exposes
//!   [`unseen_divider::UnseenDividerState`] (`on_scroll_away`,
//!   `on_repin`, `on_messages_changed` / `settle`, `shift_divider`),
//!   [`unseen_divider::count_unseen_assistant_turns`] (visible-text-
//!   only turn counter), [`unseen_divider::compute_unseen_divider`]
//!   (anchor-skip + "1 new" floor), and
//!   [`unseen_divider::pill_visible`] (the pure scroll snapshot check:
//!   `scroll_top + pending_delta + viewport_height < divider_y`).
//! * [`new_messages_pill`] — the label projection for the pill (`0 →
//!   "Jump to bottom"`, `1 → "1 new message"`, `N>1 → "N new messages"`).
//! * [`fullscreen_layout`] — the layout-zone projection:
//!   [`fullscreen_layout::LayoutZones`], [`fullscreen_layout::layout_zones`],
//!   the sticky-header gating (null when `hide_sticky` / when an
//!   overlay is present / when the prompt is `"clicked"`), the
//!   `pad_collapsed` flag, the `hide_pill` / `overlay` / `pill_visible`
//!   three-way gate on the pill, the modal pane visibility gate, and
//!   the modal context math (`rows - MODAL_TRANSCRIPT_PEEK - 1`,
//!   `columns - 4`).
//!
//! ## Outbound seam shapes
//!
//! External dependencies are modelled as follows:
//!
//! 1. **Scroll-box handle** → the module never imports a scroll handle
//!    directly. `UnseenDividerState` takes
//!    a [`unseen_divider::ScrollSnapshot`] struct with the four fields
//!    it reads (`scroll_top`, `pending_delta`, `viewport_height`,
//!    `scroll_height`). `jump_to_new` returns an
//!    [`unseen_divider::ScrollAction`] enum — the consumer dispatches
//!    the actual call.
//!
//! 2. **The message list + `StickyPrompt`** — the consumer feeds
//!    pre-computed [`fullscreen_layout::StickyPrompt`] values into
//!    `layout_zones`. We never touch message-list internals.
//!
//! 3. **Footer suggestions** — the consumer feeds pre-built footer
//!    suggestion data (not typed here — represented as an opaque
//!    `bool` "has suggestions" flag, since this module only decides
//!    whether to render the overlay, not what's inside it).
//!
//! 4. **App-state / lifecycle plumbing** (state subscriptions,
//!    settings, terminal size, keybindings) — not modelled. Each module
//!    exposes a small pure function or reducer struct the consumer
//!    drives.
//!
//! 5. **Upstream data sources and side effects** (fullscreen env
//!    check, classifying attachments that render nothing, pluralization,
//!    viewed-teammate lookup, color resolution, glyph tables,
//!    browser/path opening, modal context, prompt overlay) — not modelled.
//!    The module consumes plain owned input structs (or small trait
//!    callbacks for the message introspection in
//!    `count_unseen_assistant_turns`) and emits plain owned decision
//!    shapes.
//!
//! ## Out of scope
//!
//! * The alternate-screen wrapper, `DECSTBM` region management, and
//!   scroll-smear repair — terminal-renderer internals, out of scope
//!   for this module.
//! * The actual scroll-box and box render primitives — render targets
//!   are the consumer's responsibility.
//! * Terminal-size / animation-frame / external-store subscriptions —
//!   the consumer drives state transitions.
//! * Suggestions-overlay + dialog-overlay internal rendering — this
//!   module only decides "show / don't show", and the consumer threads
//!   the content through.
//! * Opening browsers / paths from hyperlink clicks — this is a
//!   side-effect outbound edge; the module emits a
//!   [`fullscreen_layout::HyperlinkAction`] enum.

pub mod aws_auth_status_box;
pub mod fullscreen_layout;
pub mod new_messages_pill;
pub mod search_box;
pub mod tag_tabs;
pub mod teammate_view_header;
pub mod unseen_divider;

/// Rows of transcript context kept visible above the modal pane's ▔
/// divider.
pub const MODAL_TRANSCRIPT_PEEK: u16 = 2;
