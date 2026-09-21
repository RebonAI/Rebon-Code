//! Flat, append-only transcript store — holds the committed
//! [`Message`](crate::message::Message) list.
//!
//! The store's contract is:
//!
//! * Messages are append-only: new rows are pushed onto the end.
//!   Existing rows are **not** mutated in place mid-stream — streaming
//!   deltas live on a parallel overlay (see [`crate::streaming`]).
//! * Each message has a stable `uuid`. Re-renders must not shuffle
//!   uuids.
//! * Reads are synchronous (no async lens, no pending snapshot) — the
//!   renderer reads the rows directly on every paint.
//!
//! The store exposes exactly that contract: `push` appends, `rows`
//! returns an `&[Message]` slice, `get` looks up by uuid in O(1). An
//! [`upsert`](Self::upsert) entry point exists for the narrow case where
//! the same uuid is re-pushed. It is the safety net the cancel-commit
//! reducer uses to stay idempotent under Ctrl+C mash (a double commit
//! must not duplicate the row).
//!
//! ## Not in this module (explicitly)
//!
//! * **Derivation / pipeline** — the runtime applies 15+ passes
//!   (normalise the rows, drop the ones before a compact boundary,
//!   reorder them, group them, collapse read/search groups, …). None of
//!   those passes are in this crate yet; this store holds the raw
//!   `Message` list those passes consume.
//! * **Expand-key lookups** — the renderer builds its expand-state lookup
//!   from the normalised rows and the rows it is about to show. This
//!   module does not implement the lookup builder. The
//!   [`Message::tool_result_id`](crate::message::Message::tool_result_id) /
//!   [`Message::tool_use_ids`](crate::message::Message::tool_use_ids)
//!   helpers are deliberately minimal — just enough to correlate a
//!   single pair for tests.

use std::collections::HashMap;

use crate::message::Message;

/// Append-only message store.
#[derive(Debug, Default, Clone)]
pub struct TranscriptStore {
    rows: Vec<Message>,
    /// Parallel to `rows`. Stamped with the store-wide `revision` at
    /// the time each row was created or last mutated. The measurement
    /// cache uses this so an upsert only invalidates that one row,
    /// instead of forcing every other row to re-measure on the next
    /// frame.
    row_revisions: Vec<u64>,
    /// uuid → index. Rebuilt alongside `rows`. Rows without a uuid
    /// (the open-world `Message::Unknown` fallback) are not
    /// indexed — they can still appear in `rows` but can't be
    /// looked up.
    index: HashMap<String, usize>,
    revision: u64,
}

impl TranscriptStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_rows(rows: Vec<Message>) -> Self {
        let mut index = HashMap::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            if let Some(uuid) = row.uuid() {
                index.insert(uuid.to_string(), i);
            }
        }
        let row_revisions = vec![1u64; rows.len()];
        Self {
            rows,
            row_revisions,
            index,
            revision: 1,
        }
    }

    /// Append a new message. If `msg.uuid()` is already present, the
    /// call is routed through [`upsert`](Self::upsert) instead of
    /// duplicating — this keeps the cancel-commit reducer
    /// idempotent under double-Cancel.
    pub fn push(&mut self, msg: Message) {
        if let Some(uuid) = msg.uuid() {
            if self.index.contains_key(uuid) {
                self.upsert(msg);
                return;
            }
        }
        let idx = self.rows.len();
        if let Some(uuid) = msg.uuid() {
            self.index.insert(uuid.to_string(), idx);
        }
        self.rows.push(msg);
        self.revision = self.revision.saturating_add(1);
        self.row_revisions.push(self.revision);
    }

    /// Replace an existing message by uuid, or append if absent.
    pub fn upsert(&mut self, msg: Message) {
        if let Some(uuid) = msg.uuid() {
            if let Some(&idx) = self.index.get(uuid) {
                self.rows[idx] = msg;
                self.revision = self.revision.saturating_add(1);
                self.row_revisions[idx] = self.revision;
                return;
            }
            let idx = self.rows.len();
            self.index.insert(uuid.to_string(), idx);
            self.rows.push(msg);
            self.revision = self.revision.saturating_add(1);
            self.row_revisions.push(self.revision);
            return;
        }
        // Unknown / unidentified row — append without indexing.
        self.rows.push(msg);
        self.revision = self.revision.saturating_add(1);
        self.row_revisions.push(self.revision);
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.row_revisions.clear();
        self.index.clear();
        self.revision = self.revision.saturating_add(1);
    }

    /// Truncate the transcript to `len` rows, dropping only tail rows.
    ///
    /// Returns `true` when rows were removed. The uuid index is rebuilt
    /// for the remaining prefix and the revision advances exactly when
    /// the store changes.
    pub fn truncate(&mut self, len: usize) -> bool {
        if len >= self.rows.len() {
            return false;
        }
        self.rows.truncate(len);
        self.row_revisions.truncate(len);
        self.rebuild_index();
        self.revision = self.revision.saturating_add(1);
        true
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for (i, row) in self.rows.iter().enumerate() {
            if let Some(uuid) = row.uuid() {
                self.index.insert(uuid.to_string(), i);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> &[Message] {
        &self.rows
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Per-row revision stamp — bumped only when this specific row is
    /// upserted (or first inserted). Used by the measurement cache as
    /// part of its key so that appending a new row at the tail doesn't
    /// force every existing row to re-measure on the next frame.
    ///
    /// Returns 0 when `idx` is out of bounds — callers should treat that
    /// as "uncacheable" and re-measure.
    pub fn row_revision(&self, idx: usize) -> u64 {
        self.row_revisions.get(idx).copied().unwrap_or(0)
    }

    /// Borrow the parallel per-row revision slice. Lets the renderer
    /// look up revisions without re-querying the store per row.
    pub fn row_revisions(&self) -> &[u64] {
        &self.row_revisions
    }

    pub fn get(&self, uuid: &str) -> Option<&Message> {
        self.index.get(uuid).and_then(|&i| self.rows.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantTextBlock, UserMessage, UserMessageInner, UserRole, UserTextBlock,
    };
    use serde_json::json;

    fn user(uuid: &str, text: &str) -> Message {
        Message::User(UserMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: UserMessageInner {
                role: UserRole::User,
                content: vec![crate::message::UserContentBlock::Text(UserTextBlock {
                    text: text.into(),
                })],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    fn assistant(uuid: &str, text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::Text(AssistantTextBlock {
                    text: text.into(),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    #[test]
    fn push_preserves_insertion_order_and_indexes_by_uuid() {
        let mut store = TranscriptStore::new();
        store.push(user("u1", "hi"));
        store.push(assistant("a1", "hello"));
        store.push(user("u2", "more"));

        assert_eq!(store.len(), 3);
        assert_eq!(store.rows()[0].uuid(), Some("u1"));
        assert_eq!(store.rows()[1].uuid(), Some("a1"));
        assert_eq!(store.rows()[2].uuid(), Some("u2"));
        assert!(store.get("a1").is_some());
        assert!(store.get("missing").is_none());
    }

    #[test]
    fn push_of_existing_uuid_updates_in_place_not_duplicates() {
        // Cancel-commit idempotency: pushing the same uuid twice must
        // overwrite, not duplicate. Without this, a Ctrl+C double-
        // press would land two committed-partial rows.
        let mut store = TranscriptStore::new();
        store.push(assistant("a1", "first"));
        store.push(assistant("a1", "second"));
        assert_eq!(store.len(), 1);
        match store.get("a1").unwrap() {
            Message::Assistant(a) => match &a.message.content[0] {
                AssistantContentBlock::Text(t) => assert_eq!(t.text, "second"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn from_rows_reindexes() {
        let rows = vec![user("u1", "a"), assistant("a1", "b")];
        let store = TranscriptStore::from_rows(rows);
        assert_eq!(store.len(), 2);
        assert!(store.get("u1").is_some());
        assert!(store.get("a1").is_some());
    }

    #[test]
    fn revision_advances_on_push_upsert_and_clear() {
        let mut store = TranscriptStore::new();
        let r0 = store.revision();

        store.push(user("u1", "a"));
        let r1 = store.revision();
        assert!(r1 > r0);

        store.upsert(user("u1", "b"));
        let r2 = store.revision();
        assert!(r2 > r1);

        store.clear();
        assert!(store.revision() > r2);
    }

    #[test]
    fn from_rows_starts_with_non_zero_revision() {
        let store = TranscriptStore::from_rows(vec![user("u1", "a")]);
        assert!(store.revision() > 0);
    }

    #[test]
    fn unknown_rows_append_but_are_not_indexed() {
        let mut store = TranscriptStore::new();
        // A row whose top-level type isn't in the union falls through
        // to Message::Unknown on deserialize.
        let unknown: Message = serde_json::from_value(json!({
            "type": "progress",
            "uuid": "p-1",
            "content": "legacy"
        }))
        .unwrap();
        assert!(matches!(unknown, Message::Unknown));
        store.push(unknown);
        assert_eq!(store.len(), 1);
        // Can't be looked up by uuid — it's in rows but not in the
        // index, because unknown rows are skipped when the index is
        // built.
        assert!(store.get("p-1").is_none());
    }

    #[test]
    fn truncate_removes_only_tail_rows_and_rebuilds_index() {
        let mut store = TranscriptStore::new();
        store.push(user("u1", "a"));
        store.push(assistant("a1", "b"));
        store.push(user("u2", "c"));
        let before = store.revision();

        assert!(store.truncate(2));

        assert_eq!(store.len(), 2);
        assert!(store.revision() > before);
        assert_eq!(store.rows()[0].uuid(), Some("u1"));
        assert_eq!(store.rows()[1].uuid(), Some("a1"));
        assert!(store.get("u1").is_some());
        assert!(store.get("a1").is_some());
        assert!(store.get("u2").is_none());
    }

    #[test]
    fn truncate_past_end_is_noop() {
        let mut store = TranscriptStore::new();
        store.push(user("u1", "a"));
        let before = store.revision();

        assert!(!store.truncate(1));
        assert!(!store.truncate(2));

        assert_eq!(store.len(), 1);
        assert_eq!(store.revision(), before);
        assert!(store.get("u1").is_some());
    }

    #[test]
    fn row_revision_bumps_only_for_touched_row() {
        // Appending a new row must NOT change the per-row revision of
        // any existing row — that's the whole point of the per-row
        // stamp, so the measure cache can keep its old hits across an
        // append.
        let mut store = TranscriptStore::new();
        store.push(user("u1", "a"));
        store.push(assistant("a1", "b"));
        let rev_u1 = store.row_revision(0);
        let rev_a1 = store.row_revision(1);
        assert!(rev_u1 > 0);
        assert!(rev_a1 > rev_u1);

        store.push(user("u2", "c"));
        assert_eq!(store.row_revision(0), rev_u1, "u1 must be untouched");
        assert_eq!(store.row_revision(1), rev_a1, "a1 must be untouched");
        assert!(store.row_revision(2) > rev_a1);

        // Upserting u1 bumps u1's row revision and leaves a1/u2 alone.
        let rev_u2 = store.row_revision(2);
        store.upsert(user("u1", "a-updated"));
        assert!(store.row_revision(0) > rev_u1);
        assert_eq!(store.row_revision(1), rev_a1);
        assert_eq!(store.row_revision(2), rev_u2);

        // Out-of-bounds index returns 0 — caller should treat as
        // uncacheable.
        assert_eq!(store.row_revision(99), 0);
    }

    #[test]
    fn from_rows_stamps_every_row_with_a_non_zero_revision() {
        let store = TranscriptStore::from_rows(vec![user("u1", "a"), assistant("a1", "b")]);
        assert!(store.row_revision(0) > 0);
        assert!(store.row_revision(1) > 0);
    }

    #[test]
    fn truncate_drops_per_row_revisions_for_removed_tail() {
        let mut store = TranscriptStore::new();
        store.push(user("u1", "a"));
        store.push(assistant("a1", "b"));
        store.push(user("u2", "c"));
        let rev_u1 = store.row_revision(0);
        let rev_a1 = store.row_revision(1);

        assert!(store.truncate(2));
        assert_eq!(store.row_revision(0), rev_u1);
        assert_eq!(store.row_revision(1), rev_a1);
        assert_eq!(
            store.row_revision(2),
            0,
            "truncated tail must not retain a revision"
        );
    }

    #[test]
    fn clear_drops_everything_including_the_index() {
        let mut store = TranscriptStore::new();
        store.push(user("u1", "a"));
        store.push(user("u2", "b"));
        store.clear();
        assert_eq!(store.len(), 0);
        assert!(store.get("u1").is_none());
    }
}
