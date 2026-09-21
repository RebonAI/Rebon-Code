//! Multi-select properties and their defaults.
//!
//! [`SelectMultiProps`] carries the option list and the view/behaviour
//! flags a multi-select surface needs. The defaults are:
//!
//! * **Default value** — empty (`[]`).
//! * **Visible option count** — `5`.
//! * **`is_disabled`** — `false`.
//! * **`hide_indexes`** — `false`.
//!
//! The consumer builds its [`crate::MultiSelectState`] from these
//! fields.

use crate::option::{OptionId, OptionWithDescription};

/// Properties for the multi-select surface.
#[derive(Debug, Clone)]
pub struct SelectMultiProps<T: OptionId> {
    /// Default selected values. Defaults to empty.
    pub default_value: Vec<T>,
    /// Number of items to display. Defaults to 5.
    pub visible_option_count: usize,
    /// Whether input is disabled.
    pub is_disabled: bool,
    /// `hide_indexes` flag.
    pub hide_indexes: bool,
    /// Whether the submit button is shown.
    pub has_submit_button: bool,
    /// Input option list.
    pub options: Vec<OptionWithDescription<T>>,
    /// Initially focus the LAST option (vs. the first, the default).
    pub initial_focus_last: bool,
}

impl<T: OptionId> SelectMultiProps<T> {
    /// Build with the defaults documented on each field.
    pub fn new(options: Vec<OptionWithDescription<T>>) -> Self {
        Self {
            default_value: Vec::new(),
            visible_option_count: 5,
            is_disabled: false,
            hide_indexes: false,
            has_submit_button: false,
            options,
            initial_focus_last: false,
        }
    }

    /// The configured default values (empty unless set).
    pub fn default_value_or_empty(&self) -> &[T] {
        &self.default_value
    }
}

/// Unwrap an optional default-value list, treating `None` as empty.
pub fn multi_default_value<T: OptionId>(default_value: Option<Vec<T>>) -> Vec<T> {
    default_value.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(values: &[&'static str]) -> Vec<OptionWithDescription<&'static str>> {
        values
            .iter()
            .map(|v| OptionWithDescription::text(*v, *v))
            .collect()
    }

    #[test]
    fn new_uses_defaults() {
        let p = SelectMultiProps::new(opts(&["a", "b"]));
        assert_eq!(p.default_value, Vec::<&'static str>::new());
        assert_eq!(p.visible_option_count, 5);
        assert!(!p.is_disabled);
        assert!(!p.hide_indexes);
        assert!(!p.has_submit_button);
        assert!(!p.initial_focus_last);
    }

    #[test]
    fn multi_default_value_helper_none_returns_empty() {
        let v: Vec<&'static str> = multi_default_value(None);
        assert!(v.is_empty());
    }

    #[test]
    fn multi_default_value_helper_some_returns_value() {
        let v = multi_default_value(Some(vec!["a", "b"]));
        assert_eq!(v, vec!["a", "b"]);
    }

    #[test]
    fn default_value_or_empty_returns_slice() {
        let p = SelectMultiProps::new(opts(&["a"]));
        assert_eq!(p.default_value_or_empty(), &[] as &[&'static str]);
    }
}
