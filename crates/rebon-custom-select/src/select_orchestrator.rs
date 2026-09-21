//! Orchestrator helpers shared by the select surfaces.
//!
//! These cover the input-value plumbing the reducers do not:
//!
//! 1. **Initial input values map** — build a value-to-text map seeded
//!    from each input option's `initial_value`.
//!
//! 2. **Initial-value sync** — when an input option's `initial_value`
//!    changes externally AND the user has not edited the field (the
//!    current value still matches the last seen initial), overwrite
//!    `input_values[value]` with the new initial. `last_initial_values`
//!    carries the last seen initial across calls so an in-progress
//!    edit is never clobbered.
//!
//! 3. **Disable-selection resolver** — an explicit disable wins,
//!    otherwise `hide_indexes` disables only the numeric jump. Exposed
//!    as [`NumericDisableSelection`] with a conversion to
//!    [`DisableSelection`].

use std::collections::HashMap;

use crate::option::{OptionId, OptionWithDescription};
use crate::select_input::DisableSelection;

/// Build a map of input-option value → initial text, seeded from each
/// input option's `initial_value` and skipping options with none.
pub fn initial_input_values<T: OptionId>(
    options: &[OptionWithDescription<T>],
) -> HashMap<T, String> {
    let mut map = HashMap::new();
    for option in options {
        if option.is_input() {
            if let Some(input) = option.input.as_ref() {
                if let Some(initial) = input.initial_value.as_ref() {
                    map.insert(option.value().clone(), initial.clone());
                }
            }
        }
    }
    map
}

/// Sync external `initial_value` updates into `input_values` when the
/// user hasn't edited the field.
///
/// `last_initial_values` is tracked across calls: this function updates
/// it in place and applies any in-place edits to `input_values`.
pub fn sync_initial_input_values<T: OptionId>(
    options: &[OptionWithDescription<T>],
    input_values: &mut HashMap<T, String>,
    last_initial_values: &mut HashMap<T, String>,
) {
    for option in options {
        if !option.is_input() {
            continue;
        }
        let Some(input) = option.input.as_ref() else {
            continue;
        };
        let Some(new_initial) = input.initial_value.as_ref() else {
            continue;
        };
        let last_initial = last_initial_values
            .get(option.value())
            .cloned()
            .unwrap_or_default();
        let current = input_values
            .get(option.value())
            .cloned()
            .unwrap_or_default();
        if new_initial != &last_initial && current == last_initial {
            input_values.insert(option.value().clone(), new_initial.clone());
        }
        last_initial_values.insert(option.value().clone(), new_initial.clone());
    }
}

/// `hide_indexes`-aware projection of the disable-selection rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericDisableSelection {
    /// Selection enabled, numeric jump enabled.
    Off,
    /// Both Enter and numeric jump disabled (`disable_selection: true`).
    All,
    /// Only numeric jump disabled (`hide_indexes: true`).
    Numeric,
}

/// Compute the resolved disable-selection mode.
pub fn resolve_disable_selection(
    disable_selection: bool,
    hide_indexes: bool,
) -> NumericDisableSelection {
    if disable_selection {
        NumericDisableSelection::All
    } else if hide_indexes {
        NumericDisableSelection::Numeric
    } else {
        NumericDisableSelection::Off
    }
}

impl From<NumericDisableSelection> for DisableSelection {
    fn from(value: NumericDisableSelection) -> Self {
        match value {
            NumericDisableSelection::Off => DisableSelection::Off,
            NumericDisableSelection::All => DisableSelection::All,
            NumericDisableSelection::Numeric => DisableSelection::Numeric,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option::InputOption;

    fn input_option(
        value: &'static str,
        initial: Option<&str>,
    ) -> OptionWithDescription<&'static str> {
        let mut o = OptionWithDescription::input("label", value);
        o.input = Some(InputOption {
            initial_value: initial.map(|s| s.to_string()),
            ..Default::default()
        });
        o
    }

    #[test]
    fn initial_input_values_includes_input_options_with_initial() {
        let options = vec![
            OptionWithDescription::text("text", "t"),
            input_option("i1", Some("hello")),
            input_option("i2", None),
        ];
        let map = initial_input_values(&options);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("i1"), Some(&"hello".to_string()));
    }

    #[test]
    fn initial_input_values_skips_text_options() {
        let options = vec![
            OptionWithDescription::text("text", "t"),
            OptionWithDescription::text("text2", "t2"),
        ];
        let map = initial_input_values(&options);
        assert!(map.is_empty());
    }

    #[test]
    fn sync_overwrites_when_user_unchanged() {
        let options = vec![input_option("i", Some("v2"))];
        let mut input_values = HashMap::new();
        input_values.insert("i", "v1".to_string());
        let mut last = HashMap::new();
        last.insert("i", "v1".to_string());
        sync_initial_input_values(&options, &mut input_values, &mut last);
        assert_eq!(input_values.get("i"), Some(&"v2".to_string()));
        assert_eq!(last.get("i"), Some(&"v2".to_string()));
    }

    #[test]
    fn sync_skips_when_user_edited() {
        let options = vec![input_option("i", Some("v2"))];
        let mut input_values = HashMap::new();
        input_values.insert("i", "edited".to_string());
        let mut last = HashMap::new();
        last.insert("i", "v1".to_string());
        sync_initial_input_values(&options, &mut input_values, &mut last);
        // Last gets updated, but the edited current value is preserved.
        assert_eq!(input_values.get("i"), Some(&"edited".to_string()));
        assert_eq!(last.get("i"), Some(&"v2".to_string()));
    }

    #[test]
    fn sync_no_op_when_initial_unchanged() {
        let options = vec![input_option("i", Some("v1"))];
        let mut input_values = HashMap::new();
        input_values.insert("i", "v1".to_string());
        let mut last = HashMap::new();
        last.insert("i", "v1".to_string());
        sync_initial_input_values(&options, &mut input_values, &mut last);
        assert_eq!(input_values.get("i"), Some(&"v1".to_string()));
    }

    #[test]
    fn sync_seeds_last_for_first_run() {
        let options = vec![input_option("i", Some("v1"))];
        let mut input_values = HashMap::new();
        let mut last = HashMap::new();
        sync_initial_input_values(&options, &mut input_values, &mut last);
        // current = "" == last = "" (default), so initial copies in.
        assert_eq!(input_values.get("i"), Some(&"v1".to_string()));
        assert_eq!(last.get("i"), Some(&"v1".to_string()));
    }

    #[test]
    fn resolve_disable_selection_default_off() {
        assert_eq!(
            resolve_disable_selection(false, false),
            NumericDisableSelection::Off
        );
    }

    #[test]
    fn resolve_disable_selection_disable_takes_priority() {
        assert_eq!(
            resolve_disable_selection(true, true),
            NumericDisableSelection::All
        );
    }

    #[test]
    fn resolve_disable_selection_hide_indexes_only() {
        assert_eq!(
            resolve_disable_selection(false, true),
            NumericDisableSelection::Numeric
        );
    }

    #[test]
    fn numeric_disable_into_disable_selection() {
        assert_eq!(
            DisableSelection::from(NumericDisableSelection::Off),
            DisableSelection::Off
        );
        assert_eq!(
            DisableSelection::from(NumericDisableSelection::All),
            DisableSelection::All
        );
        assert_eq!(
            DisableSelection::from(NumericDisableSelection::Numeric),
            DisableSelection::Numeric
        );
    }
}
