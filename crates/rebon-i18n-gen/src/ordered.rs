//! A JSON object read as an ordered list of pairs.
//!
//! `serde_json::Map` is a `BTreeMap` unless the whole build turns on
//! `preserve_order`, and cargo features are additive: switching that on for
//! this crate switches it on for every crate sharing the same `serde_json`,
//! which changes map ordering across the workspace to solve a problem only the
//! generator has. Twenty lines of visitor keep the blast radius here.
//!
//! Order matters because the generated files are read by people: the mobile
//! ARB's grouping is curated, and `flutter gen-l10n` emits its Dart getters in
//! exactly that order, so sorting the source would rewrite three committed
//! generated files to no purpose.

use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};

/// `{"key": "value", …}`, in the order the file wrote it.
#[derive(Debug, Default)]
pub struct OrderedStrings(Vec<(String, String)>);

impl OrderedStrings {
    pub fn iter(&self) -> impl Iterator<Item = (&str, &String)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(key, _)| key)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'de> Deserialize<'de> for OrderedStrings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Pairs;

        impl<'de> Visitor<'de> for Pairs {
            type Value = OrderedStrings;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object whose values are all strings")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut pairs = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    pairs.push((key, value));
                }
                Ok(OrderedStrings(pairs))
            }
        }

        deserializer.deserialize_map(Pairs)
    }
}
