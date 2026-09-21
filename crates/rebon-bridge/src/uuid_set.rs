//! Bounded FIFO UUID set — the echo-dedup ring buffer.
//!
//! Both sides of the bridge ingress pipeline use it:
//!
//! * **Echo dedup** — the UUIDs the bridge posted are remembered, so the
//!   server re-delivering them does not get them forwarded back a second
//!   time.
//! * **Re-delivery dedup** — a second safety net for when the SSE
//!   `lastTransportSequenceNum` carryover fails (the transport died before
//!   any frames arrived, and so on): UUIDs already forwarded locally are
//!   dropped.
//!
//! A circular buffer backs it, so memory is `O(capacity)`. Entries keep
//! chronological order, which is what makes the evicted entry always the
//! oldest one.

use std::collections::HashSet;

/// FIFO-bounded UUID set.
///
/// Keeps at most `capacity` entries; a fresh `add` evicts the oldest
/// entry when capacity is reached. Adding the same UUID twice is a
/// no-op.
pub struct BoundedUuidSet {
    capacity: usize,
    ring: Vec<Option<String>>,
    set: HashSet<String>,
    write_idx: usize,
}

impl BoundedUuidSet {
    /// Create a bounded set with the given capacity.
    ///
    /// Panics if `capacity` is zero — a zero-capacity set would
    /// silently fail to record anything, which almost certainly
    /// indicates a misconfigured caller.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "BoundedUuidSet capacity must be > 0");
        Self {
            capacity,
            ring: vec![None; capacity],
            set: HashSet::new(),
            write_idx: 0,
        }
    }

    /// Configured capacity of the ring.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current number of live entries.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// True when no entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Insert `uuid`. No-op if already present. If the set is at
    /// capacity, evicts the oldest entry first.
    pub fn add(&mut self, uuid: impl Into<String>) {
        let uuid = uuid.into();
        if self.set.contains(&uuid) {
            return;
        }
        // Evict entry at the current write position if any.
        if let Some(evicted) = self.ring[self.write_idx].take() {
            self.set.remove(&evicted);
        }
        self.ring[self.write_idx] = Some(uuid.clone());
        self.set.insert(uuid);
        self.write_idx = (self.write_idx + 1) % self.capacity;
    }

    /// True if `uuid` is tracked.
    pub fn has(&self, uuid: &str) -> bool {
        self.set.contains(uuid)
    }

    /// Drop all entries.
    pub fn clear(&mut self) {
        self.set.clear();
        for slot in &mut self.ring {
            *slot = None;
        }
        self.write_idx = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _ = BoundedUuidSet::new(0);
    }

    #[test]
    fn capacity_is_recorded() {
        let s = BoundedUuidSet::new(4);
        assert_eq!(s.capacity(), 4);
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
    }

    #[test]
    fn add_and_has_record_single_entry() {
        let mut s = BoundedUuidSet::new(4);
        s.add("a");
        assert!(s.has("a"));
        assert!(!s.has("b"));
        assert_eq!(s.len(), 1);
        assert!(!s.is_empty());
    }

    #[test]
    fn add_is_idempotent() {
        let mut s = BoundedUuidSet::new(4);
        s.add("a");
        s.add("a");
        s.add("a");
        assert_eq!(s.len(), 1);
        assert!(s.has("a"));
    }

    #[test]
    fn eviction_drops_oldest_entry_first() {
        let mut s = BoundedUuidSet::new(3);
        s.add("a");
        s.add("b");
        s.add("c");
        assert_eq!(s.len(), 3);
        // Filling past capacity evicts the oldest inserted.
        s.add("d");
        assert_eq!(s.len(), 3);
        assert!(!s.has("a"));
        assert!(s.has("b"));
        assert!(s.has("c"));
        assert!(s.has("d"));

        s.add("e");
        assert!(!s.has("b"));
        assert!(s.has("c"));
        assert!(s.has("d"));
        assert!(s.has("e"));
    }

    #[test]
    fn eviction_is_chronological_even_with_duplicates() {
        let mut s = BoundedUuidSet::new(3);
        s.add("a");
        s.add("b");
        s.add("a"); // no-op; does NOT bump "a" to the most-recent slot
        s.add("c");
        assert_eq!(s.len(), 3);
        s.add("d"); // evicts "a" (the oldest) because duplicates didn't refresh
        assert!(!s.has("a"));
        assert!(s.has("b"));
        assert!(s.has("c"));
        assert!(s.has("d"));
    }

    #[test]
    fn clear_drops_all_entries_and_resets_write_position() {
        let mut s = BoundedUuidSet::new(3);
        s.add("a");
        s.add("b");
        s.clear();
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert!(!s.has("a"));
        assert!(!s.has("b"));
        // After clear the next adds fill the ring from the start and
        // behave like a fresh set.
        s.add("x");
        s.add("y");
        s.add("z");
        s.add("w"); // evicts "x"
        assert!(!s.has("x"));
        assert!(s.has("y"));
        assert!(s.has("z"));
        assert!(s.has("w"));
    }

    #[test]
    fn capacity_one_keeps_only_the_latest_entry() {
        let mut s = BoundedUuidSet::new(1);
        s.add("a");
        assert!(s.has("a"));
        s.add("b");
        assert!(!s.has("a"));
        assert!(s.has("b"));
        s.add("b"); // idempotent
        assert_eq!(s.len(), 1);
        assert!(s.has("b"));
    }

    #[test]
    fn stress_wraparound_matches_set_semantics() {
        // Add more than 4x capacity worth of entries and verify the
        // set reports exactly the tail window.
        let mut s = BoundedUuidSet::new(8);
        for i in 0..64u32 {
            s.add(format!("u-{i}"));
        }
        assert_eq!(s.len(), 8);
        for i in 0..56u32 {
            assert!(!s.has(&format!("u-{i}")), "u-{i} should be evicted");
        }
        for i in 56..64u32 {
            assert!(s.has(&format!("u-{i}")), "u-{i} should still be present");
        }
    }
}
