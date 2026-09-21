//! UnseenDivider state machine.
//!
//! This module provides:
//!
//! * [`UnseenDividerState`] — the scroll-away / repin / settle state
//!   machine behind the "N new messages" divider.
//! * [`count_unseen_assistant_turns`]
//! * [`compute_unseen_divider`]
//! * [`pill_visible`] — the pure visibility snapshot.
//!
//! ## The four rules the rebon-tui retrospective flagged
//!
//! The earlier rebon-tui `layout.rs` stub was removed because these
//! rules hadn't been audited. They are pinned here.
//!
//! ### Rule 1 — divider-position race ordering
//!
//! [`UnseenDividerState::on_scroll_away`] fires on **every** scroll
//! action, not just the initial break from sticky. The snapshot is
//! guarded by a null check on `divider_y`: after the first scroll-away,
//! `divider_y` is set and subsequent calls are no-ops.
//!
//! [`UnseenDividerState::on_repin`] only clears the state — it does
//! **not** clear `divider_y`. That field is cleared by
//! [`UnseenDividerState::settle`], which the consumer calls after the
//! render that committed the null `divider_index`. The reason is a wheel
//! event arriving in the same stdin batch as the submit that triggered
//! the repin: if `on_repin` cleared `divider_y` synchronously, the
//! racing wheel event would see it still unset and re-snapshot,
//! resurrecting a divider the user just dismissed.
//!
//! In Rust this is a two-step [`UnseenDividerState::on_repin`] and
//! [`UnseenDividerState::settle`] pair. `on_repin` sets
//! `divider_index = None`; `settle` (called after the consumer's
//! render / commit) then clears `divider_y` iff `divider_index` is
//! still `None`. Tests pin the invariant that a wheel event between
//! `on_repin` and `settle` does NOT re-arm the divider.
//!
//! ### Rule 2 — Modal pane suppression
//!
//! `has_overlay` is threaded through the pill visibility gate, alongside
//! `!hide_pill` and `pill_visible`. The pill is also suppressed when an
//! overlay is present — a modal takes over the bottom slot and the user
//! needs to read it, not chase new messages.
//!
//! The modal pane does NOT suppress the pill directly (modals render
//! in a different absolute-positioned layer). But it suppresses the
//! sticky header through the same `has_overlay` flag.
//!
//! In this module we expose [`PillSuppression`] and [`pill_visible_gate`]
//! which applies the three-way gate (`hide_pill`, `has_overlay`,
//! `pill_visible`).
//!
//! ### Rule 3 — Assistant-turn semantics
//!
//! A "turn" is what users think of as "a new message from Rebon".
//! A single API response yields multiple entries (`tool_use` blocks +
//! `text` blocks). [`count_unseen_assistant_turns`] counts
//! non-assistant → assistant transitions, BUT:
//!
//! * `progress` entries are skipped entirely (no effect on either
//!   counter or `prev_was_assistant`).
//! * `assistant` entries without visible text are also skipped — so
//!   a `tool_use`-only assistant entry doesn't tick the count. But
//!   because `prev_was_assistant` is NOT updated on the skip, a text
//!   block immediately following still counts as the same turn
//!   (`tool_use` + `text` = 1).
//!
//! [`MessageLite::assistant_has_visible_text`] carries the result of
//! walking the content blocks: true iff at least one `text` block has
//! non-whitespace trimmed content.
//!
//! This module erases the full `Message` type and takes the minimal
//! shape: [`MessageLite`] with a [`MessageKind`] and an "has visible
//! text" flag. Tests pin the priority: `progress` → skip entirely,
//! `assistant no-text` → skip but preserve the prev flag,
//! `assistant text` → increment if prev was not assistant.
//!
//! ### Rule 4 — "1 new" floor
//!
//! `compute_unseen_divider` returns the first unseen UUID together with a
//! count floored at 1 (`count.max(1)`) — the floor kicks in only when a
//! message at the resolved anchor index actually
//! exists (otherwise the whole function returns `None`). So:
//!
//! * no messages past divider → `None`
//! * any messages past divider (even tool-use only) → `Some({ count ≥ 1 })`
//! * multiple assistant text turns past divider → `Some({ count = N })`
//!
//! This means during a tool-call sequence the pill flips from "Jump
//! to bottom" to "1 new message" as soon as the first tool-use entry
//! lands, even before the assistant's text response. The floor is
//! **inclusive** — `max(1, count)`, not `count + 1`.
//!
//! ## Anchor-skip rule
//!
//! [`compute_unseen_divider`] also skips `progress` entries and
//! null-rendering-attachment entries when picking the anchor UUID,
//! filtering both out before the anchor search. The
//! `is_null_rendering_attachment` flag lets the consumer plug in the
//! attachment classifier without pulling in per-attachment types.

/// Simple enum for the message kinds this module cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    User,
    Assistant,
    Attachment,
    System,
    Progress,
}

/// Minimal shape used by the turn counter / divider computation. We
/// erase the full Anthropic content blocks and just carry the
/// predicates the rules need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLite {
    pub uuid: String,
    pub kind: MessageKind,
    /// True iff this is an `assistant` entry AND it has at least one
    /// `text` content block with non-whitespace content — the caller
    /// resolves this when it builds the entry.
    ///
    /// For non-assistant entries this is always false.
    pub assistant_has_visible_text: bool,
    /// True iff this is a null-rendering attachment (the anchor-skip
    /// rule). For non-attachment entries this is false.
    pub is_null_rendering_attachment: bool,
}

impl MessageLite {
    pub fn user(uuid: impl Into<String>) -> Self {
        Self {
            uuid: uuid.into(),
            kind: MessageKind::User,
            assistant_has_visible_text: false,
            is_null_rendering_attachment: false,
        }
    }
    pub fn assistant_text(uuid: impl Into<String>) -> Self {
        Self {
            uuid: uuid.into(),
            kind: MessageKind::Assistant,
            assistant_has_visible_text: true,
            is_null_rendering_attachment: false,
        }
    }
    pub fn assistant_tool_only(uuid: impl Into<String>) -> Self {
        Self {
            uuid: uuid.into(),
            kind: MessageKind::Assistant,
            assistant_has_visible_text: false,
            is_null_rendering_attachment: false,
        }
    }
    pub fn progress(uuid: impl Into<String>) -> Self {
        Self {
            uuid: uuid.into(),
            kind: MessageKind::Progress,
            assistant_has_visible_text: false,
            is_null_rendering_attachment: false,
        }
    }
    pub fn null_attachment(uuid: impl Into<String>) -> Self {
        Self {
            uuid: uuid.into(),
            kind: MessageKind::Attachment,
            assistant_has_visible_text: false,
            is_null_rendering_attachment: true,
        }
    }
    pub fn attachment(uuid: impl Into<String>) -> Self {
        Self {
            uuid: uuid.into(),
            kind: MessageKind::Attachment,
            assistant_has_visible_text: false,
            is_null_rendering_attachment: false,
        }
    }
}

/// Rule 3. Counts assistant turns in `messages[divider_index..]`.
///
/// A "turn" is a non-assistant → assistant transition where the
/// assistant entry carries visible text. `progress` and
/// `assistant`-without-text are both skipped without updating the
/// `prev_was_assistant` flag, so `tool_use` + `text` from one API
/// response counts as one turn.
pub fn count_unseen_assistant_turns(messages: &[MessageLite], divider_index: usize) -> u64 {
    let mut count = 0u64;
    let mut prev_was_assistant = false;
    if divider_index >= messages.len() {
        return 0;
    }
    for m in &messages[divider_index..] {
        if m.kind == MessageKind::Progress {
            continue;
        }
        if m.kind == MessageKind::Assistant && !m.assistant_has_visible_text {
            // Skip tool-use-only entries. prev_was_assistant stays so
            // the text block immediately after still counts as one.
            continue;
        }
        let is_assistant = m.kind == MessageKind::Assistant;
        if is_assistant && !prev_was_assistant {
            count += 1;
        }
        prev_was_assistant = is_assistant;
    }
    count
}

/// The "unseen divider" shape passed to the message list and the pill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnseenDivider {
    pub first_unseen_uuid: String,
    /// Floor-1 count (always >= 1 when present). Rule 4.
    pub count: u64,
}

/// Rule 4 + anchor-skip.
pub fn compute_unseen_divider(
    messages: &[MessageLite],
    divider_index: Option<usize>,
) -> Option<UnseenDivider> {
    let divider_index = divider_index?;
    // Anchor-skip: step past progress + null-rendering attachments.
    let mut anchor_idx = divider_index;
    while anchor_idx < messages.len() {
        let m = &messages[anchor_idx];
        if m.kind == MessageKind::Progress || m.is_null_rendering_attachment {
            anchor_idx += 1;
            continue;
        }
        break;
    }
    let uuid = messages.get(anchor_idx)?.uuid.clone();
    let count = count_unseen_assistant_turns(messages, divider_index);
    Some(UnseenDivider {
        first_unseen_uuid: uuid,
        count: count.max(1),
    })
}

/// A snapshot of the scroll state: the four values the consumer
/// resolves from its scroll handle before calling into this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollSnapshot {
    pub scroll_top: i64,
    pub pending_delta: i64,
    pub viewport_height: u64,
    pub scroll_height: u64,
}

/// Rule 1's state machine. `divider_index` holds the message index the
/// divider sits before; `divider_y` holds the scroll offset snapshotted
/// on the first scroll-away. The two-step repin flow is `on_repin` (sets
/// `divider_index = None`) + `settle` (clears `divider_y` iff the state
/// is still `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnseenDividerState {
    message_count: usize,
    divider_index: Option<usize>,
    divider_y: Option<u64>,
}

impl UnseenDividerState {
    pub fn new(initial_message_count: usize) -> Self {
        Self {
            message_count: initial_message_count,
            divider_index: None,
            divider_y: None,
        }
    }

    pub fn divider_index(&self) -> Option<usize> {
        self.divider_index
    }
    pub fn divider_y(&self) -> Option<u64> {
        self.divider_y
    }
    pub fn message_count(&self) -> usize {
        self.message_count
    }

    /// Called on every scroll action (wheel, page-up, scroll-to-bottom). The
    /// guard:
    ///
    /// * if viewport bottom (`scroll_top + pending_delta >= max`) is
    ///   at the bottom, nothing to do (click-to-select at bottom).
    /// * first scroll-away: snapshot `divider_y = scroll_height`, set
    ///   `divider_index = message_count`.
    /// * subsequent scroll-aways: no-op (`divider_y` is already set).
    pub fn on_scroll_away(&mut self, snap: ScrollSnapshot) {
        let max = snap.scroll_height.saturating_sub(snap.viewport_height) as i64;
        if snap.scroll_top + snap.pending_delta >= max {
            return;
        }
        if self.divider_y.is_none() {
            self.divider_y = Some(snap.scroll_height);
            self.divider_index = Some(self.message_count);
        }
    }

    /// Called on submit / explicit scroll-to-bottom. Sets
    /// `divider_index = None` but leaves `divider_y` alone —
    /// [`Self::settle`] clears it after the consumer's render.
    pub fn on_repin(&mut self) {
        self.divider_index = None;
    }

    /// Deferred commit step, called by the consumer after the render that
    /// observed the previous `on_*` call:
    ///
    /// * `divider_index` is `None` → clear `divider_y`.
    /// * `message_count < divider_index` → clear both (rewind / clear /
    ///   teammate swap).
    ///
    /// Call this AFTER the consumer has observed the state the
    /// previous `on_*` call produced.
    pub fn settle(&mut self) {
        match self.divider_index {
            None => {
                self.divider_y = None;
            }
            Some(idx) => {
                if self.message_count < idx {
                    self.divider_y = None;
                    self.divider_index = None;
                }
            }
        }
    }

    /// Update `message_count` to the consumer's current total.
    /// Call before an `on_scroll_away` so the snapshot captures the
    /// latest count.
    pub fn set_message_count(&mut self, count: usize) {
        self.message_count = count;
    }

    /// Returns `ScrollAction::ScrollToBottom`: the consumer scrolls to the
    /// bottom (which re-arms sticky) rather than to `divider_y`.
    pub fn jump_to_new(&self) -> ScrollAction {
        ScrollAction::ScrollToBottom
    }

    /// Used when infinite-scroll-back prepends messages.
    pub fn shift_divider(&mut self, index_delta: i64, height_delta: i64) {
        if let Some(idx) = self.divider_index {
            let new_idx = (idx as i64) + index_delta;
            if new_idx < 0 {
                // Guard: the arithmetic just adds without a floor,
                // but a negative index would be invalid. We clamp.
                self.divider_index = Some(0);
            } else {
                self.divider_index = Some(new_idx as usize);
            }
        }
        if let Some(y) = self.divider_y {
            let new_y = (y as i64) + height_delta;
            self.divider_y = Some(new_y.max(0) as u64);
        }
    }

    /// Whether the pill should be visible based on the raw scroll
    /// comparison (Rule 2 — before the `hide_pill`/`has_overlay` gate).
    pub fn pill_visible(&self, snap: ScrollSnapshot) -> bool {
        pill_visible(snap, self.divider_y)
    }
}

/// Consumer-facing scroll action enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    ScrollToBottom,
}

/// Pure-function pill visibility snapshot.
///
/// Returns `false` when `divider_y` is `None`; otherwise returns true
/// when the viewport bottom — `scroll_top + pending_delta +
/// viewport_height` — is strictly above `divider_y`.
pub fn pill_visible(snap: ScrollSnapshot, divider_y: Option<u64>) -> bool {
    let Some(dy) = divider_y else {
        return false;
    };
    let viewport_bottom = snap.scroll_top + snap.pending_delta + snap.viewport_height as i64;
    (viewport_bottom as i128) < dy as i128
}

/// Pill suppression input: the three flags that gate the pill's
/// visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PillSuppression {
    pub hide_pill: bool,
    pub has_overlay: bool,
    pub pill_visible: bool,
}

/// Rule 2. The full gate: `!hide_pill && pill_visible && !has_overlay`.
pub fn pill_visible_gate(s: PillSuppression) -> bool {
    !s.hide_pill && s.pill_visible && !s.has_overlay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(scroll_top: i64, pending: i64, viewport: u64, height: u64) -> ScrollSnapshot {
        ScrollSnapshot {
            scroll_top,
            pending_delta: pending,
            viewport_height: viewport,
            scroll_height: height,
        }
    }

    // ====================
    // Rule 3 — assistant-turn semantics
    // ====================

    #[test]
    fn turns_empty_messages_is_zero() {
        assert_eq!(count_unseen_assistant_turns(&[], 0), 0);
    }

    #[test]
    fn turns_divider_past_end_is_zero() {
        let msgs = vec![MessageLite::user("u1")];
        assert_eq!(count_unseen_assistant_turns(&msgs, 5), 0);
    }

    #[test]
    fn turns_single_user_message_zero() {
        let msgs = vec![MessageLite::user("u1")];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 0);
    }

    #[test]
    fn turns_single_assistant_text_is_one() {
        let msgs = vec![MessageLite::assistant_text("a1")];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_user_then_assistant_text_is_one() {
        let msgs = vec![MessageLite::user("u1"), MessageLite::assistant_text("a1")];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_two_assistant_texts_without_gap_counts_one() {
        // Consecutive assistant text entries are the same turn
        // (prev_was_assistant flag gates the increment).
        let msgs = vec![
            MessageLite::assistant_text("a1"),
            MessageLite::assistant_text("a2"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_assistant_user_assistant_counts_two() {
        let msgs = vec![
            MessageLite::assistant_text("a1"),
            MessageLite::user("u1"),
            MessageLite::assistant_text("a2"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 2);
    }

    #[test]
    fn turns_progress_is_skipped_no_effect_on_count() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::progress("p1"),
            MessageLite::assistant_text("a1"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_progress_preserves_prev_assistant_flag() {
        // assistant_text → progress → assistant_text → still 1 turn
        // because the progress skip preserves prev_was_assistant.
        let msgs = vec![
            MessageLite::assistant_text("a1"),
            MessageLite::progress("p1"),
            MessageLite::assistant_text("a2"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_tool_only_assistant_is_skipped_no_count() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::assistant_tool_only("a1"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 0);
    }

    #[test]
    fn turns_tool_only_then_text_is_one_turn() {
        // The critical same-turn rule: tool_use + text = 1.
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::assistant_tool_only("a1"),
            MessageLite::assistant_text("a2"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_text_then_tool_only_still_one_turn() {
        let msgs = vec![
            MessageLite::assistant_text("a1"),
            MessageLite::assistant_tool_only("a2"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_user_then_two_separate_assistant_turns_counts_two() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::assistant_text("a1"),
            MessageLite::user("u2"),
            MessageLite::assistant_text("a2"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 2);
    }

    #[test]
    fn turns_divider_in_middle_only_counts_past_it() {
        let msgs = vec![
            MessageLite::assistant_text("a1"),
            MessageLite::user("u1"),
            MessageLite::assistant_text("a2"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 1), 1);
    }

    #[test]
    fn turns_attachment_message_does_not_increment() {
        // Non-assistant → attachment → assistant_text = still 1 turn,
        // and the attachment itself doesn't count.
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::attachment("at1"),
            MessageLite::assistant_text("a1"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    #[test]
    fn turns_many_tool_onlys_before_text_one_turn() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::assistant_tool_only("a1"),
            MessageLite::assistant_tool_only("a2"),
            MessageLite::assistant_tool_only("a3"),
            MessageLite::assistant_text("a4"),
        ];
        assert_eq!(count_unseen_assistant_turns(&msgs, 0), 1);
    }

    // ====================
    // Rule 4 — "1 new" floor
    // ====================

    #[test]
    fn compute_none_when_divider_none() {
        let msgs = vec![MessageLite::assistant_text("a1")];
        assert_eq!(compute_unseen_divider(&msgs, None), None);
    }

    #[test]
    fn compute_none_when_no_messages_past_divider() {
        let msgs = vec![MessageLite::user("u1")];
        assert_eq!(compute_unseen_divider(&msgs, Some(1)), None);
    }

    #[test]
    fn compute_floor_one_even_if_only_tool_use() {
        // Tool-use-only past divider → count_turns returns 0 but
        // floor bumps to 1.
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::assistant_tool_only("a1"),
        ];
        let d = compute_unseen_divider(&msgs, Some(1)).unwrap();
        assert_eq!(d.count, 1);
        assert_eq!(d.first_unseen_uuid, "a1");
    }

    #[test]
    fn compute_floor_one_even_if_only_user_entry() {
        let msgs = vec![MessageLite::assistant_text("a1"), MessageLite::user("u1")];
        let d = compute_unseen_divider(&msgs, Some(1)).unwrap();
        assert_eq!(d.count, 1);
        assert_eq!(d.first_unseen_uuid, "u1");
    }

    #[test]
    fn compute_actual_count_when_multiple_turns() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::assistant_text("a1"),
            MessageLite::user("u2"),
            MessageLite::assistant_text("a2"),
        ];
        let d = compute_unseen_divider(&msgs, Some(0)).unwrap();
        assert_eq!(d.count, 2);
    }

    #[test]
    fn compute_anchor_skip_progress() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::progress("p1"),
            MessageLite::progress("p2"),
            MessageLite::assistant_text("a1"),
        ];
        let d = compute_unseen_divider(&msgs, Some(1)).unwrap();
        // Anchor skipped past p1 + p2 → lands on a1.
        assert_eq!(d.first_unseen_uuid, "a1");
    }

    #[test]
    fn compute_anchor_skip_null_attachment() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::null_attachment("na1"),
            MessageLite::assistant_text("a1"),
        ];
        let d = compute_unseen_divider(&msgs, Some(1)).unwrap();
        assert_eq!(d.first_unseen_uuid, "a1");
    }

    #[test]
    fn compute_anchor_does_not_skip_regular_attachment() {
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::attachment("at1"),
            MessageLite::assistant_text("a1"),
        ];
        let d = compute_unseen_divider(&msgs, Some(1)).unwrap();
        assert_eq!(d.first_unseen_uuid, "at1");
    }

    #[test]
    fn compute_none_when_divider_past_all_messages_even_floor() {
        let msgs = vec![MessageLite::assistant_text("a1")];
        assert_eq!(compute_unseen_divider(&msgs, Some(5)), None);
    }

    #[test]
    fn compute_floor_not_additive_one_plus() {
        // Floor is max(1, count), NOT count + 1.
        let msgs = vec![MessageLite::user("u1"), MessageLite::assistant_text("a1")];
        let d = compute_unseen_divider(&msgs, Some(0)).unwrap();
        assert_eq!(d.count, 1);
    }

    #[test]
    fn compute_progress_only_past_divider_is_none() {
        // After anchor-skip, nothing left → None.
        let msgs = vec![
            MessageLite::user("u1"),
            MessageLite::progress("p1"),
            MessageLite::progress("p2"),
        ];
        assert_eq!(compute_unseen_divider(&msgs, Some(1)), None);
    }

    // ====================
    // Rule 1 — divider-position race ordering
    // ====================

    #[test]
    fn new_state_is_clean() {
        let s = UnseenDividerState::new(5);
        assert_eq!(s.message_count(), 5);
        assert_eq!(s.divider_index(), None);
        assert_eq!(s.divider_y(), None);
    }

    #[test]
    fn first_scroll_away_snapshots_both() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        assert_eq!(s.divider_index(), Some(10));
        assert_eq!(s.divider_y(), Some(200));
    }

    #[test]
    fn scroll_away_at_bottom_is_noop() {
        let mut s = UnseenDividerState::new(10);
        // scroll_top = 170, viewport = 30, height = 200 → max = 170,
        // viewport bottom = 200 == scroll_height → at bottom.
        s.on_scroll_away(snap(170, 0, 30, 200));
        assert_eq!(s.divider_index(), None);
        assert_eq!(s.divider_y(), None);
    }

    #[test]
    fn scroll_away_at_bottom_via_pending_delta_is_noop() {
        // scroll_top=0, pending=170, viewport=30, height=200 → at max.
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(0, 170, 30, 200));
        assert_eq!(s.divider_index(), None);
    }

    #[test]
    fn second_scroll_away_is_noop_preserves_baseline() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        // Append more messages and fire another scroll-away. Baseline
        // must NOT reset.
        s.set_message_count(15);
        s.on_scroll_away(snap(40, 0, 30, 220));
        assert_eq!(s.divider_index(), Some(10)); // unchanged
        assert_eq!(s.divider_y(), Some(200)); // unchanged
    }

    #[test]
    fn on_repin_clears_index_but_not_y() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.on_repin();
        assert_eq!(s.divider_index(), None);
        // divider_y should still be set — the race window.
        assert_eq!(s.divider_y(), Some(200));
    }

    #[test]
    fn wheel_event_during_race_window_does_not_rearm() {
        // Rule 1 critical case: on_repin set index=None, but divider_y
        // is still set. A wheel event in the same batch must NOT
        // reset the divider.
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.on_repin();
        // Racing wheel event arrives — scroll up again.
        s.on_scroll_away(snap(30, 0, 30, 200));
        // Because divider_y is still Some (guard in on_scroll_away),
        // the second call is a no-op. Index stays None until settle.
        assert_eq!(s.divider_index(), None);
        assert_eq!(s.divider_y(), Some(200));
    }

    #[test]
    fn settle_clears_y_after_repin() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.on_repin();
        s.settle();
        assert_eq!(s.divider_index(), None);
        assert_eq!(s.divider_y(), None);
    }

    #[test]
    fn settle_noop_when_index_is_some() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.settle();
        assert_eq!(s.divider_index(), Some(10));
        assert_eq!(s.divider_y(), Some(200));
    }

    #[test]
    fn settle_clears_when_message_count_drops_below_index() {
        // /clear, rewind, or teammate swap reduces the count below
        // the divider — the `message_count < divider_index` branch of
        // `settle`.
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        // Messages go away (rewind).
        s.set_message_count(3);
        s.settle();
        assert_eq!(s.divider_index(), None);
        assert_eq!(s.divider_y(), None);
    }

    #[test]
    fn settle_noop_when_count_equals_index() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.set_message_count(10);
        s.settle();
        assert_eq!(s.divider_index(), Some(10));
    }

    #[test]
    fn settle_noop_when_count_above_index() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.set_message_count(20);
        s.settle();
        assert_eq!(s.divider_index(), Some(10));
    }

    #[test]
    fn shift_divider_moves_index_and_y() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.shift_divider(5, 40);
        assert_eq!(s.divider_index(), Some(15));
        assert_eq!(s.divider_y(), Some(240));
    }

    #[test]
    fn shift_divider_negative_index_clamps_to_zero() {
        let mut s = UnseenDividerState::new(3);
        s.on_scroll_away(snap(10, 0, 10, 100));
        s.shift_divider(-10, 0);
        assert_eq!(s.divider_index(), Some(0));
    }

    #[test]
    fn shift_divider_negative_y_clamps_to_zero() {
        let mut s = UnseenDividerState::new(3);
        s.on_scroll_away(snap(10, 0, 10, 50));
        s.shift_divider(0, -100);
        assert_eq!(s.divider_y(), Some(0));
    }

    #[test]
    fn shift_divider_noop_when_no_divider() {
        let mut s = UnseenDividerState::new(3);
        s.shift_divider(5, 100);
        assert_eq!(s.divider_index(), None);
        assert_eq!(s.divider_y(), None);
    }

    #[test]
    fn jump_to_new_returns_scroll_to_bottom() {
        let s = UnseenDividerState::new(5);
        assert_eq!(s.jump_to_new(), ScrollAction::ScrollToBottom);
    }

    #[test]
    fn repin_without_prior_scroll_away_is_safe() {
        let mut s = UnseenDividerState::new(5);
        s.on_repin();
        s.settle();
        assert_eq!(s.divider_index(), None);
        assert_eq!(s.divider_y(), None);
    }

    #[test]
    fn scroll_away_then_scroll_away_during_repin_race_window_is_noop() {
        // Full flow: scroll-away, repin, wheel (no-op because
        // divider_y set), settle (clears), wheel (snapshots fresh).
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(50, 0, 30, 200));
        s.on_repin();
        s.on_scroll_away(snap(40, 0, 30, 200));
        assert_eq!(s.divider_y(), Some(200));
        s.settle();
        s.on_scroll_away(snap(40, 0, 30, 210));
        assert_eq!(s.divider_y(), Some(210));
    }

    // ====================
    // pill_visible
    // ====================

    #[test]
    fn pill_visible_false_when_divider_y_none() {
        assert!(!pill_visible(snap(0, 0, 30, 200), None));
    }

    #[test]
    fn pill_visible_true_when_below_divider() {
        // viewport bottom = 80, divider_y = 150 → visible.
        assert!(pill_visible(snap(50, 0, 30, 200), Some(150)));
    }

    #[test]
    fn pill_visible_false_when_at_divider() {
        // viewport bottom = 150, divider_y = 150 → strict <, not visible.
        assert!(!pill_visible(snap(120, 0, 30, 200), Some(150)));
    }

    #[test]
    fn pill_visible_false_when_past_divider() {
        assert!(!pill_visible(snap(170, 0, 30, 200), Some(150)));
    }

    #[test]
    fn pill_visible_accounts_for_pending_delta() {
        // scroll_top = 50, pending = 50, viewport = 30 → bottom=130
        // < divider_y=150.
        assert!(pill_visible(snap(50, 50, 30, 200), Some(150)));
    }

    #[test]
    fn pill_visible_accounts_for_pending_delta_over_divider() {
        // scroll_top = 50, pending = 100, viewport = 30 → bottom=180
        // >= divider_y=150 → not visible.
        assert!(!pill_visible(snap(50, 100, 30, 200), Some(150)));
    }

    #[test]
    fn state_pill_visible_delegates() {
        let mut s = UnseenDividerState::new(10);
        s.on_scroll_away(snap(20, 0, 30, 200));
        assert!(s.pill_visible(snap(20, 0, 30, 200)));
    }

    // ====================
    // Rule 2 — modal pane / overlay suppression
    // ====================

    #[test]
    fn gate_all_conditions_true_returns_true() {
        assert!(pill_visible_gate(PillSuppression {
            hide_pill: false,
            has_overlay: false,
            pill_visible: true,
        }));
    }

    #[test]
    fn gate_hide_pill_hides() {
        assert!(!pill_visible_gate(PillSuppression {
            hide_pill: true,
            has_overlay: false,
            pill_visible: true,
        }));
    }

    #[test]
    fn gate_overlay_hides() {
        assert!(!pill_visible_gate(PillSuppression {
            hide_pill: false,
            has_overlay: true,
            pill_visible: true,
        }));
    }

    #[test]
    fn gate_not_visible_hides() {
        assert!(!pill_visible_gate(PillSuppression {
            hide_pill: false,
            has_overlay: false,
            pill_visible: false,
        }));
    }

    #[test]
    fn gate_overlay_and_hide_pill_both_set_hides() {
        assert!(!pill_visible_gate(PillSuppression {
            hide_pill: true,
            has_overlay: true,
            pill_visible: true,
        }));
    }
}
