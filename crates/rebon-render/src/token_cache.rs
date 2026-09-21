//! Bounded LRU cache with explicit MRU promotion.
//!
//! ## The cache contract
//!
//! ```text
//!   capacity            500 entries (TOKEN_CACHE_MAX)
//!   on hit              the entry is promoted to most-recently-used
//!   on insert at cap    the oldest insertion-order entry is dropped first
//! ```
//!
//! Two load-bearing properties:
//!
//! 1. **MRU on hit.** A lookup moves the matched entry to the newest
//!    position in the order. Without this, an active session that
//!    scrolls back evicts the very rows you're reading.
//! 2. **FIFO eviction on insert.** When the cache is full, the
//!    oldest *insertion-order* entry is dropped. Combined with the
//!    MRU-on-hit promotion, this becomes a true LRU.
//!
//! ## Why a hand-rolled cache instead of `lru` crate
//!
//! `lru = "0.12"` would give this for free, but it adds an
//! external dep. The LRU semantics need only a few dozen lines here,
//! and hand-rolling them keeps the dependency floor low and lets the
//! tests assert the eviction order directly. The `lru` crate would
//! also push its own `NonZeroUsize` capacity type onto this API.

use std::collections::BTreeMap;
use std::hash::Hash;

/// Hard cap on how many entries the cache keeps.
pub const TOKEN_CACHE_MAX: usize = 500;

/// Bounded LRU cache. Stores `(key, value)` pairs with an explicit
/// insertion-order index used for FIFO eviction. On `get`, the
/// matched entry's index is moved to the new max so future eviction
/// passes treat it as freshly inserted — the MRU promotion that makes
/// scrolling back cheap.
///
/// The cache is `Send` if `K` and `V` are; it is **not** `Sync` and is
/// intended for single-threaded use from one render path.
///
/// `K: Eq + Hash + Clone` so the cache can both look up and re-key
/// on promotion. `V: Clone` is required because lookups return a
/// clone of the stored value, which is fine for `Vec` values whose
/// elements are themselves cheap to clone.
#[derive(Debug)]
pub struct TokenCache<K, V> {
    /// Order index → entry. The lowest key is the oldest, the
    /// highest is the newest. `BTreeMap` is used (rather than
    /// `Vec`) because eviction is `pop_first` (O(log n)) and
    /// promotion is `remove + insert` (also O(log n)).
    by_order: BTreeMap<u64, (K, V)>,
    /// Reverse index — `key → its current order key`. Used to
    /// locate the entry on `get` (without scanning) and to remove
    /// the old order entry before re-inserting at the new max.
    order_of: std::collections::HashMap<K, u64>,
    /// Monotonic counter — every insert / promotion takes a fresh
    /// value here, so the highest order key is always the most
    /// recently used.
    next_order: u64,
    /// Hard cap. Tested separately so the cap is auditable.
    capacity: usize,
}

impl<K, V> TokenCache<K, V>
where
    K: Eq + Hash + Clone + Ord,
    V: Clone,
{
    /// New empty cache with the supplied capacity. Use
    /// [`TokenCache::with_default_capacity`] for the default capacity of
    /// `TOKEN_CACHE_MAX` (500).
    pub fn new(capacity: usize) -> Self {
        Self {
            by_order: BTreeMap::new(),
            order_of: std::collections::HashMap::new(),
            next_order: 0,
            capacity,
        }
    }

    /// Cache sized to `TOKEN_CACHE_MAX` (500).
    pub fn with_default_capacity() -> Self {
        Self::new(TOKEN_CACHE_MAX)
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.by_order.len()
    }

    /// Whether the cache is empty. Provided to satisfy clippy and to let
    /// callers check the cache's size through the cache itself.
    pub fn is_empty(&self) -> bool {
        self.by_order.is_empty()
    }

    /// Cache lookup with **MRU promotion** on hit: the matched entry moves
    /// to the newest order slot. Returns `None` for a miss.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let old_order = *self.order_of.get(key)?;
        let (k, v) = self.by_order.remove(&old_order)?;
        let new_order = self.next_order;
        self.next_order += 1;
        self.order_of.insert(k.clone(), new_order);
        self.by_order.insert(new_order, (k, v.clone()));
        Some(v)
    }

    /// Insert (or update) a key. Triggers FIFO eviction of the
    /// oldest entry **before** insertion when the cache is at capacity,
    /// so the size never exceeds the capacity even transiently.
    ///
    /// If the key already exists, this updates the value AND promotes the
    /// entry to MRU.
    pub fn insert(&mut self, key: K, value: V) {
        // If the key is already present, just promote and update.
        if let Some(&old_order) = self.order_of.get(&key) {
            self.by_order.remove(&old_order);
            let new_order = self.next_order;
            self.next_order += 1;
            self.order_of.insert(key.clone(), new_order);
            self.by_order.insert(new_order, (key, value));
            return;
        }

        // Evict oldest if at capacity.
        if self.by_order.len() >= self.capacity {
            if let Some((_, (oldest_key, _))) = self.by_order.pop_first() {
                self.order_of.remove(&oldest_key);
            }
        }

        let new_order = self.next_order;
        self.next_order += 1;
        self.order_of.insert(key.clone(), new_order);
        self.by_order.insert(new_order, (key, value));
    }

    /// Iterator over `(key, value)` in **insertion-then-promotion
    /// order** — oldest first, newest last. Used by tests and by
    /// debugging callers.
    pub fn iter_oldest_to_newest(&self) -> impl Iterator<Item = (&K, &V)> {
        self.by_order.values().map(|(k, v)| (k, v))
    }
}

/// `djb2` hash over the input bytes.
///
/// ```text
/// hash = 0
/// for each byte b:
///     hash = ((hash << 5) - hash + b) as 32-bit int
/// ```
///
/// The walk is over **bytes**, which differs from a UTF-16 code-unit
/// walk for non-ASCII input. Both are deterministic within a process
/// and the cache is per-process — see the lib.rs "Hash-key choice"
/// section for the full rationale. The hash is returned as `u32` (the
/// same 32-bit truncation the arithmetic applies) so two callers in the
/// same process always get the same key for the same content.
pub fn djb2_key(content: &str) -> u32 {
    let mut hash: i32 = 0;
    for &byte in content.as_bytes() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(byte as i32);
    }
    hash as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Basic cache mechanics
    // -----------------------------------------------------------------

    #[test]
    fn empty_cache_returns_none() {
        let mut cache: TokenCache<String, Vec<i32>> = TokenCache::new(4);
        assert!(cache.is_empty());
        assert_eq!(cache.get(&"missing".into()), None);
    }

    #[test]
    fn insert_then_get_returns_value() {
        let mut cache = TokenCache::new(4);
        cache.insert("a".to_string(), vec![1, 2, 3]);
        assert_eq!(cache.get(&"a".to_string()), Some(vec![1, 2, 3]));
    }

    #[test]
    fn duplicate_insert_updates_value_and_promotes() {
        let mut cache = TokenCache::new(4);
        cache.insert("a".to_string(), vec![1]);
        cache.insert("b".to_string(), vec![2]);
        cache.insert("a".to_string(), vec![10]); // overwrite + promote
        let order: Vec<&String> = cache.iter_oldest_to_newest().map(|(k, _)| k).collect();
        // After re-inserting "a", "a" must be the newest entry.
        assert_eq!(order, vec![&"b".to_string(), &"a".to_string()]);
        assert_eq!(cache.get(&"a".to_string()), Some(vec![10]));
    }

    // -----------------------------------------------------------------
    // FIFO eviction at capacity
    // -----------------------------------------------------------------

    #[test]
    fn evicts_oldest_on_overflow() {
        let mut cache = TokenCache::new(3);
        cache.insert("a".to_string(), 1);
        cache.insert("b".to_string(), 2);
        cache.insert("c".to_string(), 3);
        cache.insert("d".to_string(), 4); // evicts "a"
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get(&"a".to_string()), None);
        assert_eq!(cache.get(&"d".to_string()), Some(4));
    }

    #[test]
    fn at_capacity_eviction_runs_before_insertion() {
        // Eviction runs before the insert, so with capacity 2 the size
        // is never observed at 3.
        let mut cache = TokenCache::new(2);
        cache.insert("a".to_string(), 1);
        cache.insert("b".to_string(), 2);
        cache.insert("c".to_string(), 3);
        assert_eq!(cache.len(), 2);
    }

    // -----------------------------------------------------------------
    // MRU promotion on hit — the load-bearing case this cache exists
    // for. Without promotion, the cache degrades to FIFO and
    // scrollback evicts the very item being read.
    // -----------------------------------------------------------------

    #[test]
    fn get_promotes_to_mru() {
        let mut cache = TokenCache::new(3);
        cache.insert("a".to_string(), 1);
        cache.insert("b".to_string(), 2);
        cache.insert("c".to_string(), 3);
        // Touch "a" — it should now be MRU.
        let _ = cache.get(&"a".to_string());
        // Insert "d" — eviction must drop "b" (now oldest), not "a".
        cache.insert("d".to_string(), 4);
        assert_eq!(cache.get(&"a".to_string()), Some(1)); // survives
        assert_eq!(cache.get(&"b".to_string()), None); // evicted
    }

    #[test]
    fn get_miss_does_not_change_order() {
        let mut cache = TokenCache::new(3);
        cache.insert("a".to_string(), 1);
        cache.insert("b".to_string(), 2);
        cache.insert("c".to_string(), 3);
        let _ = cache.get(&"missing".to_string()); // no-op
        cache.insert("d".to_string(), 4); // evicts "a"
        assert_eq!(cache.get(&"a".to_string()), None);
        assert_eq!(cache.get(&"b".to_string()), Some(2));
    }

    #[test]
    fn promotion_chain_preserves_correct_order() {
        // Touch a, then b, then a again. Eviction order must be
        // c, then b, then a.
        let mut cache = TokenCache::new(3);
        cache.insert("a".to_string(), 1);
        cache.insert("b".to_string(), 2);
        cache.insert("c".to_string(), 3);
        let _ = cache.get(&"a".to_string());
        let _ = cache.get(&"b".to_string());
        let _ = cache.get(&"a".to_string());

        // Now order (oldest → newest) should be: c, b, a.
        let order: Vec<&String> = cache.iter_oldest_to_newest().map(|(k, _)| k).collect();
        assert_eq!(
            order,
            vec![&"c".to_string(), &"b".to_string(), &"a".to_string()]
        );

        // Inserting d evicts c.
        cache.insert("d".to_string(), 4);
        assert_eq!(cache.get(&"c".to_string()), None);
        assert_eq!(cache.get(&"b".to_string()), Some(2));
        assert_eq!(cache.get(&"a".to_string()), Some(1));
    }

    // -----------------------------------------------------------------
    // Default capacity — pins the magic constant
    // -----------------------------------------------------------------

    #[test]
    fn default_capacity_is_500() {
        let cache: TokenCache<String, ()> = TokenCache::with_default_capacity();
        assert_eq!(cache.capacity, 500);
        assert_eq!(TOKEN_CACHE_MAX, 500);
    }

    #[test]
    fn at_default_capacity_size_never_exceeds_500() {
        let mut cache: TokenCache<u32, ()> = TokenCache::with_default_capacity();
        for i in 0..600 {
            cache.insert(i, ());
        }
        assert_eq!(cache.len(), 500);
        // The first 100 entries should have been evicted.
        assert_eq!(cache.get(&0), None);
        assert_eq!(cache.get(&99), None);
        assert_eq!(cache.get(&100), Some(()));
        assert_eq!(cache.get(&599), Some(()));
    }

    // -----------------------------------------------------------------
    // djb2_key determinism and known-value tests
    // -----------------------------------------------------------------

    #[test]
    fn djb2_key_is_deterministic() {
        assert_eq!(djb2_key("hello"), djb2_key("hello"));
    }

    #[test]
    fn djb2_key_distinguishes_different_inputs() {
        assert_ne!(djb2_key("hello"), djb2_key("world"));
        assert_ne!(djb2_key("a"), djb2_key("b"));
    }

    #[test]
    fn djb2_key_empty_string_is_zero() {
        // The hash starts at 0 and is never updated when there are no
        // bytes to fold in.
        assert_eq!(djb2_key(""), 0);
    }

    #[test]
    fn djb2_key_known_values() {
        // Sanity-check single-byte and two-byte hashes against the
        // canonical formulation, computed by hand:
        //
        //   djb2("a") = ((0 << 5) - 0 + 'a') | 0
        //             = 97
        //
        //   djb2("ab") = ((97 << 5) - 97 + 'b') | 0
        //              = 3104 - 97 + 98
        //              = 3105
        assert_eq!(djb2_key("a"), 97);
        assert_eq!(djb2_key("ab"), 3105);
    }

    #[test]
    fn djb2_key_hashes_byte_aware() {
        // Different bytes → different hashes (very high probability,
        // not guaranteed for all inputs but holds for these).
        assert_ne!(djb2_key("ab"), djb2_key("ba"));
    }
}
