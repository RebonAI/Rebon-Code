//! Per-row height cache used by the transcript renderer.
//!
//! The key bundles every input that can change a row's measured height
//! between frames so the renderer can hit cache during scroll, append,
//! and idle redraws — and miss exactly when something genuinely
//! changed.
//!
//! The cache is "invalidated" on width change. Rather than dropping
//! the whole map, width is part of the key — stale entries
//! silently miss instead of being returned with a wrong height.
//!
//! ## What's in the key
//!
//! * **uuid** — primary identity (`message.uuid`).
//! * **width** — wrapping changes height. Caching only by uuid would
//!   return a stale height on terminal resize (the exact bug width
//!   invalidation guards against).
//! * **row_revision** — bumped by `TranscriptStore` on every per-row
//!   mutation. Lets the cache survive appends to the tail and only
//!   miss the row that actually changed.
//! * **add_margin** — controls the leading blank row between segments.
//!   The first segment differs from subsequent ones; encoding the flag
//!   into the key keeps both variants live without one stomping the
//!   other.
//! * **last_thinking_block_id_hash** — Compact-mode rendering shows a
//!   thinking preview tied to the most recent thinking block, so
//!   assistant rows can change height when this id changes. We hash
//!   the Option<&str> at lookup time to avoid an extra String
//!   allocation in the hot path.
//! * **live_activity_signature** — non-zero only for committed async-Agent
//!   rows, so activity changes miss exactly those rows without flushing
//!   unrelated transcript measurements.
//!
//! Transcript-wide rendering modes are still handled by
//! `TranscriptMeasureCache::prepare`; live activity is encoded locally.

use std::collections::HashMap;

/// Cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MeasureKey {
    pub uuid: String,
    pub width: u16,
    pub row_revision: u64,
    pub add_margin: bool,
    pub last_thinking_block_id_hash: u64,
    pub live_activity_signature: u64,
}

impl MeasureKey {
    pub fn new(
        uuid: impl Into<String>,
        width: u16,
        row_revision: u64,
        add_margin: bool,
        last_thinking_block_id: Option<&str>,
        live_activity_signature: u64,
    ) -> Self {
        Self {
            uuid: uuid.into(),
            width,
            row_revision,
            add_margin,
            last_thinking_block_id_hash: hash_thinking_id(last_thinking_block_id),
            live_activity_signature,
        }
    }
}

/// Stable, deterministic hash of the optional thinking-block id used
/// in the cache key. `None` collapses to 0 so absence is distinct from
/// the (astronomically unlikely) hash value 0 of a real id.
///
/// FNV-1a 64-bit — fixed offset/prime, no per-process seeding, so
/// `insert` followed by `get` always agree. Collisions don't corrupt
/// correctness here because the cache value (height in rows) is
/// recomputed on miss anyway; a stray hit would have to collide on
/// uuid+width+row_revision+add_margin+thinking_id_hash simultaneously,
/// which is astronomically unlikely at our scale.
pub fn hash_thinking_id(id: Option<&str>) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    match id {
        None => 0,
        Some(s) => {
            let mut h = FNV_OFFSET;
            for b in s.as_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(FNV_PRIME);
            }
            // Avoid colliding with the explicit "None = 0" sentinel
            // on the empty string.
            if h == 0 {
                1
            } else {
                h
            }
        }
    }
}

/// Height cache. Maps `MeasureKey` → measured row height in terminal rows.
#[derive(Debug, Default, Clone)]
pub struct MeasureCache {
    map: HashMap<MeasureKey, u16>,
    /// Tracks the latest `(row_revision, live_activity_signature)` seen per
    /// uuid. Used to avoid a full-table `retain` on every `insert` — eviction
    /// only runs when either render revision actually changes.
    uuid_revision: HashMap<String, (u64, u64)>,
}

impl MeasureCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Look up a cached height. Returns `None` on miss — the caller is
    /// expected to re-measure and then call [`insert`](Self::insert).
    pub fn get(
        &self,
        uuid: &str,
        width: u16,
        row_revision: u64,
        add_margin: bool,
        last_thinking_block_id: Option<&str>,
        live_activity_signature: u64,
    ) -> Option<u16> {
        self.map
            .get(&MeasureKey {
                uuid: uuid.to_string(),
                width,
                row_revision,
                add_margin,
                last_thinking_block_id_hash: hash_thinking_id(last_thinking_block_id),
                live_activity_signature,
            })
            .copied()
    }

    /// Store a measured height.
    ///
    /// Evicts stale entries for the same uuid whose `row_revision` differs
    /// from the one being inserted — those will never hit again.
    pub fn insert(
        &mut self,
        uuid: impl Into<String>,
        width: u16,
        row_revision: u64,
        add_margin: bool,
        last_thinking_block_id: Option<&str>,
        live_activity_signature: u64,
        height: u16,
    ) {
        let uuid = uuid.into();
        let render_revision = (row_revision, live_activity_signature);
        let prev = self.uuid_revision.insert(uuid.clone(), render_revision);
        if prev.is_some_and(|revision| revision != render_revision) {
            self.map.retain(|key, _| {
                key.uuid != uuid
                    || (key.row_revision, key.live_activity_signature) == render_revision
            });
        }
        self.map.insert(
            MeasureKey {
                uuid,
                width,
                row_revision,
                add_margin,
                last_thinking_block_id_hash: hash_thinking_id(last_thinking_block_id),
                live_activity_signature,
            },
            height,
        );
    }

    /// Drop every entry for `uuid` across all keys.
    pub fn invalidate_uuid(&mut self, uuid: &str) {
        self.map.retain(|k, _| k.uuid != uuid);
        self.uuid_revision.remove(uuid);
    }

    /// Drop every entry whose `width` matches the given value.
    pub fn invalidate_width(&mut self, width: u16) {
        self.map.retain(|k, _| k.width != width);
    }

    /// Clear the entire cache.
    pub fn clear(&mut self) {
        self.map.clear();
        self.uuid_revision.clear();
    }

    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = (&MeasureKey, &u16)> {
        self.map.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get_round_trip() {
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        cache.insert("u1", 120, 7, false, None, 0, 2);
        cache.insert("u2", 80, 7, false, None, 0, 5);

        assert_eq!(cache.get("u1", 80, 7, false, None, 0), Some(3));
        assert_eq!(cache.get("u1", 120, 7, false, None, 0), Some(2));
        assert_eq!(cache.get("u2", 80, 7, false, None, 0), Some(5));
    }

    #[test]
    fn different_widths_for_same_uuid_are_independent() {
        // The whole point of the (uuid, width) key — a hit at width 80
        // must NOT return at width 120.
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        assert_eq!(
            cache.get("u1", 120, 7, false, None, 0),
            None,
            "width mismatch must miss"
        );
    }

    #[test]
    fn row_revision_bump_misses() {
        // A row's content was upserted — the new row_revision means the
        // old cached height is stale and must miss.
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        assert_eq!(cache.get("u1", 80, 8, false, None, 0), None);
        assert_eq!(cache.get("u1", 80, 7, false, None, 0), Some(3));
    }

    #[test]
    fn live_activity_signature_bump_misses_and_evicts_old_entry() {
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 11, 3);
        assert_eq!(cache.get("u1", 80, 7, false, None, 12), None);

        cache.insert("u1", 80, 7, false, None, 12, 4);
        assert_eq!(cache.get("u1", 80, 7, false, None, 11), None);
        assert_eq!(cache.get("u1", 80, 7, false, None, 12), Some(4));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_evicts_stale_row_revisions() {
        // When a new row_revision is inserted for a uuid, old entries
        // with a different row_revision are evicted to prevent unbounded
        // cache growth.
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        cache.insert("u1", 120, 7, false, None, 0, 2);
        cache.insert("u2", 80, 7, false, None, 0, 5);
        assert_eq!(cache.len(), 3);

        // Insert u1 at revision 8 — both old rev-7 entries for u1 are evicted.
        cache.insert("u1", 80, 8, false, None, 0, 10);
        assert_eq!(cache.get("u1", 80, 8, false, None, 0), Some(10));
        assert_eq!(cache.get("u1", 80, 7, false, None, 0), None);
        assert_eq!(cache.get("u1", 120, 7, false, None, 0), None);
        // u2 is unaffected.
        assert_eq!(cache.get("u2", 80, 7, false, None, 0), Some(5));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn add_margin_variants_coexist() {
        // The first segment renders with add_margin = false, every
        // other segment with add_margin = true. Both variants must
        // round-trip without stomping each other.
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        cache.insert("u1", 80, 7, true, None, 0, 4);
        assert_eq!(cache.get("u1", 80, 7, false, None, 0), Some(3));
        assert_eq!(cache.get("u1", 80, 7, true, None, 0), Some(4));
    }

    #[test]
    fn thinking_block_id_changes_invalidate_compact_assistant_rows() {
        // Compact-mode thinking previews shift based on the latest
        // visible thinking block id. Different ids must produce
        // different cache entries.
        let mut cache = MeasureCache::new();
        cache.insert("a1", 80, 7, true, Some("th-1"), 0, 5);
        assert_eq!(cache.get("a1", 80, 7, true, Some("th-1"), 0), Some(5));
        assert_eq!(cache.get("a1", 80, 7, true, Some("th-2"), 0), None);
        assert_eq!(cache.get("a1", 80, 7, true, None, 0), None);
    }

    #[test]
    fn miss_on_unknown_uuid() {
        let cache = MeasureCache::new();
        assert_eq!(cache.get("nope", 80, 7, false, None, 0), None);
    }

    #[test]
    fn invalidate_uuid_drops_every_key_for_that_uuid() {
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        cache.insert("u1", 120, 7, false, None, 0, 2);
        cache.insert("u2", 80, 7, false, None, 0, 5);

        cache.invalidate_uuid("u1");

        assert_eq!(cache.get("u1", 80, 7, false, None, 0), None);
        assert_eq!(cache.get("u1", 120, 7, false, None, 0), None);
        assert_eq!(cache.get("u2", 80, 7, false, None, 0), Some(5));
    }

    #[test]
    fn invalidate_width_drops_every_uuid_at_that_width() {
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        cache.insert("u2", 80, 7, false, None, 0, 5);
        cache.insert("u1", 120, 7, false, None, 0, 2);

        cache.invalidate_width(80);

        assert_eq!(cache.get("u1", 80, 7, false, None, 0), None);
        assert_eq!(cache.get("u2", 80, 7, false, None, 0), None);
        assert_eq!(cache.get("u1", 120, 7, false, None, 0), Some(2));
    }

    #[test]
    fn clear_drops_everything() {
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        cache.insert("u2", 80, 7, false, None, 0, 5);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn reinsert_overwrites_existing_height() {
        let mut cache = MeasureCache::new();
        cache.insert("u1", 80, 7, false, None, 0, 3);
        cache.insert("u1", 80, 7, false, None, 0, 9);
        assert_eq!(cache.get("u1", 80, 7, false, None, 0), Some(9));
        assert_eq!(cache.len(), 1);
    }
}
