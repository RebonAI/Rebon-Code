//! Option shapes for the select widgets.
//!
//! An option is a [`BaseOption`] — label, value, optional description,
//! the dim-description flag and the disabled flag — wrapped in an
//! [`OptionWithDescription`] that records whether it is a plain text
//! row ([`OptionType::Text`], the default) or an editable input row
//! ([`OptionType::Input`], carrying an [`InputOption`]).
//!
//! Two deliberate choices:
//!
//! * `label` is a plain `String`. Rich content belongs in a consumer
//!   render projection; this crate never inspects the label except for
//!   the `highlight_text` slicing in `select_layout`.
//! * An input option's change callback is *not* stored on the option.
//!   Callbacks are not pure data, so the reducer emits an
//!   `InputChanged` event and the consumer routes the side effect.

use std::fmt::Debug;
use std::hash::Hash;

/// Stable identifier for an option. Must be `Eq + Hash + Clone` to
/// live in the [`crate::option_map::OptionMap`].
pub trait OptionId: Eq + Hash + Clone + Debug {}

impl<T: Eq + Hash + Clone + Debug> OptionId for T {}

/// Discriminator for the option type: a text row or an input row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OptionType {
    /// Plain text option (the default).
    #[default]
    Text,
    /// Input-type option. Backed by an `InputOption` payload.
    Input,
}

/// Behaviour of an empty submit on an input-type option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum InputBehaviour {
    /// Default. Empty submits cancel.
    #[default]
    EmptyCancels,
    /// Empty submits go through as a change (empty is a valid value).
    EmptySubmits,
}

/// The base option shape shared by text and input options.
///
/// `label` is a plain `String` — see the module-level note.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseOption<T: OptionId> {
    /// Display label.
    pub label: String,
    /// Stable value identifier.
    pub value: T,
    /// Optional secondary description.
    pub description: Option<String>,
    /// Whether the description should be dimmed. Defaults to `true`;
    /// only an explicit `false` turns dimming off.
    pub dim_description: bool,
    /// Whether the option is disabled (cannot be focused-then-selected).
    pub disabled: bool,
}

impl<T: OptionId> BaseOption<T> {
    /// Convenience: build a non-disabled, non-described option.
    pub fn new(label: impl Into<String>, value: T) -> Self {
        Self {
            label: label.into(),
            value,
            description: None,
            dim_description: true,
            disabled: false,
        }
    }
}

/// Input-type option payload.
///
/// The change callback is *not* stored — the reducer emits an event
/// the consumer routes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InputOption {
    /// Optional placeholder shown when the input is empty.
    pub placeholder: Option<String>,
    /// Initial input value (defaults to empty).
    pub initial_value: Option<String>,
    /// Empty-submit behaviour.
    pub behaviour: InputBehaviour,
    /// Always show the label alongside the input value.
    pub show_label_with_value: bool,
    /// Custom separator between label and value when `show_label` is
    /// true. Defaults to `", "`.
    pub label_value_separator: Option<String>,
    /// Reset cursor to end of line on focus + value change.
    pub reset_cursor_on_update: bool,
}

impl Default for InputOption {
    fn default() -> Self {
        Self {
            placeholder: None,
            initial_value: None,
            behaviour: InputBehaviour::EmptyCancels,
            show_label_with_value: false,
            label_value_separator: None,
            reset_cursor_on_update: false,
        }
    }
}

/// The full option type: base fields, the option type, and the input
/// payload for input rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OptionWithDescription<T: OptionId> {
    /// The base option fields.
    pub base: BaseOption<T>,
    /// The option type.
    pub r#type: OptionType,
    /// Input-type metadata. `Some` only when `type == OptionType::Input`.
    pub input: Option<InputOption>,
}

impl<T: OptionId> OptionWithDescription<T> {
    /// Build a plain text option.
    pub fn text(label: impl Into<String>, value: T) -> Self {
        Self {
            base: BaseOption::new(label, value),
            r#type: OptionType::Text,
            input: None,
        }
    }

    /// Build an input-type option with default input behaviour.
    pub fn input(label: impl Into<String>, value: T) -> Self {
        Self {
            base: BaseOption::new(label, value),
            r#type: OptionType::Input,
            input: Some(InputOption::default()),
        }
    }

    /// Whether this is an input-type option.
    pub fn is_input(&self) -> bool {
        matches!(self.r#type, OptionType::Input)
    }

    /// Whether this option is disabled.
    pub fn is_disabled(&self) -> bool {
        self.base.disabled
    }

    /// Display label borrow.
    pub fn label(&self) -> &str {
        &self.base.label
    }

    /// Stable value borrow.
    pub fn value(&self) -> &T {
        &self.base.value
    }
}

/// Internal entry shape used by [`crate::option_map::OptionMap`].
/// Carries the stable index in the option list.
#[derive(Debug, Clone)]
pub struct OptionEntry<T: OptionId> {
    /// The option payload.
    pub option: OptionWithDescription<T>,
    /// Stable position in the input list.
    pub index: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_option_defaults() {
        let opt = OptionWithDescription::text("Hello", "h");
        assert_eq!(opt.label(), "Hello");
        assert_eq!(opt.value(), &"h");
        assert!(!opt.is_input());
        assert!(!opt.is_disabled());
        assert_eq!(opt.r#type, OptionType::Text);
        assert!(opt.input.is_none());
    }

    #[test]
    fn input_option_defaults() {
        let opt = OptionWithDescription::input("Type here", "t");
        assert!(opt.is_input());
        let input = opt.input.as_ref().unwrap();
        assert_eq!(input.behaviour, InputBehaviour::EmptyCancels);
        assert!(!input.show_label_with_value);
        assert!(!input.reset_cursor_on_update);
        assert!(input.placeholder.is_none());
        assert!(input.initial_value.is_none());
    }

    #[test]
    fn disabled_option_propagates() {
        let mut opt = OptionWithDescription::text("Disabled", "d");
        opt.base.disabled = true;
        assert!(opt.is_disabled());
    }

    #[test]
    fn dim_description_default_is_true() {
        // Descriptions dim unless explicitly turned off, so a fresh
        // option defaults to `true`.
        let opt = OptionWithDescription::text("L", "v");
        assert!(opt.base.dim_description);
    }

    #[test]
    fn option_id_works_for_string_and_int() {
        let s: OptionWithDescription<String> = OptionWithDescription::text("a", "x".to_string());
        let i: OptionWithDescription<i32> = OptionWithDescription::text("b", 42);
        assert_eq!(s.value(), "x");
        assert_eq!(*i.value(), 42);
    }

    #[test]
    fn input_behaviour_default_is_empty_cancels() {
        assert_eq!(InputBehaviour::default(), InputBehaviour::EmptyCancels);
    }

    #[test]
    fn option_type_default_is_text() {
        assert_eq!(OptionType::default(), OptionType::Text);
    }
}
