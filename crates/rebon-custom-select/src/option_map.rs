//! Option map for value lookup and select navigation.
//!
//! The map is built once per option list and provides value-to-index
//! lookup plus sibling traversal for navigation.
//!
//! Stored data:
//!
//! * option payloads live in a `Vec`, preserving input order;
//! * value-to-index lookup lives in a `HashMap`;
//! * public items expose a stable value and zero-based index;
//! * first, last, previous, and next items are derived from indexes.
//!
//! Sibling links are computed (`index ± 1`) rather than stored, which
//! avoids a second linked structure that could drift out of sync with
//! the vector.

use std::collections::HashMap;

use crate::option::{OptionEntry, OptionId, OptionWithDescription};

/// Public view of one option entry with the state needed by the
/// navigation reducer.
#[derive(Debug, Clone)]
pub struct OptionMapItem<T: OptionId> {
    /// Stable value (key in the map).
    pub value: T,
    /// 0-based index in the input option list.
    pub index: usize,
}

/// Doubly-linked option lookup with first/last/previous/next.
#[derive(Debug, Clone)]
pub struct OptionMap<T: OptionId> {
    entries: Vec<OptionEntry<T>>,
    by_value: HashMap<T, usize>,
}

impl<T: OptionId> OptionMap<T> {
    /// Build a new option map from the given option list. Preserves
    /// order, builds the value-to-index lookup, and exposes first/last
    /// as optional item values.
    pub fn new(options: &[OptionWithDescription<T>]) -> Self {
        let mut entries = Vec::with_capacity(options.len());
        let mut by_value = HashMap::with_capacity(options.len());
        for (index, option) in options.iter().enumerate() {
            by_value.insert(option.value().clone(), index);
            entries.push(OptionEntry {
                option: option.clone(),
                index,
            });
        }
        Self { entries, by_value }
    }

    /// Number of entries.
    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// First entry, or `None` if empty.
    pub fn first(&self) -> Option<OptionMapItem<T>> {
        self.entries.first().map(|e| OptionMapItem {
            value: e.option.value().clone(),
            index: e.index,
        })
    }

    /// Last entry, or `None` if empty.
    pub fn last(&self) -> Option<OptionMapItem<T>> {
        self.entries.last().map(|e| OptionMapItem {
            value: e.option.value().clone(),
            index: e.index,
        })
    }

    /// Lookup by value.
    pub fn get(&self, value: &T) -> Option<OptionMapItem<T>> {
        self.by_value.get(value).map(|&index| OptionMapItem {
            value: self.entries[index].option.value().clone(),
            index,
        })
    }

    /// Whether a given value exists.
    pub fn has(&self, value: &T) -> bool {
        self.by_value.contains_key(value)
    }

    /// Sibling: the next entry after `value`. `None` if `value` is
    /// missing or is the last entry.
    pub fn next_of(&self, value: &T) -> Option<OptionMapItem<T>> {
        let index = *self.by_value.get(value)?;
        let next_index = index + 1;
        if next_index >= self.entries.len() {
            None
        } else {
            Some(OptionMapItem {
                value: self.entries[next_index].option.value().clone(),
                index: next_index,
            })
        }
    }

    /// Sibling: the previous entry before `value`. `None` if `value`
    /// is missing or is the first entry.
    pub fn previous_of(&self, value: &T) -> Option<OptionMapItem<T>> {
        let index = *self.by_value.get(value)?;
        if index == 0 {
            None
        } else {
            let prev_index = index - 1;
            Some(OptionMapItem {
                value: self.entries[prev_index].option.value().clone(),
                index: prev_index,
            })
        }
    }

    /// Read the entry at a specific index.
    pub fn at(&self, index: usize) -> Option<OptionMapItem<T>> {
        self.entries.get(index).map(|e| OptionMapItem {
            value: e.option.value().clone(),
            index: e.index,
        })
    }

    /// Read the underlying option payload at an index.
    pub fn option_at(&self, index: usize) -> Option<&OptionWithDescription<T>> {
        self.entries.get(index).map(|e| &e.option)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(values: &[&str]) -> Vec<OptionWithDescription<&'static str>> {
        // The map needs owned `'static` values; leaking the test
        // literals keeps the fixture tiny.
        values
            .iter()
            .map(|v| {
                let leaked: &'static str = Box::leak(v.to_string().into_boxed_str());
                OptionWithDescription::text(*v, leaked)
            })
            .collect()
    }

    #[test]
    fn empty_map_has_no_first_or_last() {
        let map: OptionMap<&'static str> = OptionMap::new(&[]);
        assert_eq!(map.size(), 0);
        assert!(map.is_empty());
        assert!(map.first().is_none());
        assert!(map.last().is_none());
    }

    #[test]
    fn single_item_first_equals_last() {
        let options = opts(&["a"]);
        let map = OptionMap::new(&options);
        assert_eq!(map.size(), 1);
        let f = map.first().unwrap();
        let l = map.last().unwrap();
        assert_eq!(f.value, "a");
        assert_eq!(l.value, "a");
        assert_eq!(f.index, 0);
        assert_eq!(l.index, 0);
        // First has no previous, no next.
        assert!(map.previous_of(&"a").is_none());
        assert!(map.next_of(&"a").is_none());
    }

    #[test]
    fn three_items_chain_correctly() {
        let options = opts(&["a", "b", "c"]);
        let map = OptionMap::new(&options);
        assert_eq!(map.size(), 3);
        assert_eq!(map.first().unwrap().value, "a");
        assert_eq!(map.last().unwrap().value, "c");
        // Next chain.
        assert_eq!(map.next_of(&"a").unwrap().value, "b");
        assert_eq!(map.next_of(&"b").unwrap().value, "c");
        assert!(map.next_of(&"c").is_none());
        // Previous chain.
        assert!(map.previous_of(&"a").is_none());
        assert_eq!(map.previous_of(&"b").unwrap().value, "a");
        assert_eq!(map.previous_of(&"c").unwrap().value, "b");
    }

    #[test]
    fn lookup_by_value_returns_correct_index() {
        let options = opts(&["a", "b", "c", "d"]);
        let map = OptionMap::new(&options);
        assert_eq!(map.get(&"a").unwrap().index, 0);
        assert_eq!(map.get(&"b").unwrap().index, 1);
        assert_eq!(map.get(&"c").unwrap().index, 2);
        assert_eq!(map.get(&"d").unwrap().index, 3);
    }

    #[test]
    fn lookup_missing_value_returns_none() {
        let options = opts(&["a", "b"]);
        let map = OptionMap::new(&options);
        assert!(map.get(&"missing").is_none());
        assert!(!map.has(&"missing"));
        assert!(map.has(&"a"));
    }

    #[test]
    fn next_and_previous_for_missing_value_are_none() {
        let options = opts(&["a", "b"]);
        let map = OptionMap::new(&options);
        assert!(map.next_of(&"missing").is_none());
        assert!(map.previous_of(&"missing").is_none());
    }

    #[test]
    fn at_returns_entry_for_valid_index() {
        let options = opts(&["a", "b", "c"]);
        let map = OptionMap::new(&options);
        assert_eq!(map.at(0).unwrap().value, "a");
        assert_eq!(map.at(2).unwrap().value, "c");
        assert!(map.at(3).is_none());
    }
}
