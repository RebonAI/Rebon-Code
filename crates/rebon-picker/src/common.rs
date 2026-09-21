//! Shapes shared across picker modules.
//!
//! A "shape namespace" rather than a utility bag: the picker modules are
//! otherwise independent, and only a primitive that recurs across them is
//! pinned here.

/// A label/value pair for one row of a select list. Each picker module
/// instantiates this with its own per-picker `value` enum or string. Used
/// by [`crate::theme_picker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption<V> {
    /// The label shown in the rendered list.
    pub label: String,
    /// The value passed to the state transition when this option is selected.
    pub value: V,
}

impl<V> SelectOption<V> {
    /// Create a new option from `(label, value)`.
    pub fn new(label: impl Into<String>, value: V) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_option_new() {
        let opt: SelectOption<&'static str> = SelectOption::new("Hello", "world");
        assert_eq!(opt.label, "Hello");
        assert_eq!(opt.value, "world");
    }
}
