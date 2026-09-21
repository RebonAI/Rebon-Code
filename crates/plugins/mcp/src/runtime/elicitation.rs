//! MCP Elicitation dialog — schema, response builder, and validation
//! rules.
//!
//! This module focuses on the pure logic layer:
//!
//! 1. The parsed schema representation ([`ElicitationSchema`] /
//!    [`ElicitationField`] / [`ElicitationFieldKind`])
//! 2. Per-field validation rules ([`validate_field_value`])
//! 3. The response builder ([`build_elicitation_response`])
//!
//! **Out of scope:**
//! * Dialog lifecycle state management (accept/reject)
//! * Rendering (inputs, select boxes, etc.)
//! * The actual JSON-Schema parser (modeled as an input — the
//!   consumer pre-parses via its own schema library)
//! * The keyboard event wiring
//!
//! The current implementation supports six field kinds per the MCP elicitation
//! spec: `string`, `number`, `integer`, `boolean`, `enum` (select),
//! and `array`. Each carries its own validation rules.
//!
//! ## rules pinned
//!
//! 1. **Required fields must be present AND non-empty (for strings).**
//! 2. **String `minLength` / `maxLength` are strict inclusive bounds.**
//! 3. **Number/Integer `minimum` / `maximum` are inclusive.**
//! 4. **String `format: "email"` requires an `@` character.** (The
//!    current implementation uses a looser check than full RFC 5322.)
//! 5. **String `format: "uri"` requires `://` substring.**
//! 6. **Enum validation rejects values outside the provided list.**
//! 7. **Integer rejects non-integer floats.**
//! 8. **Boolean field accepts only `true` / `false`.**
//! 9. **Array field enforces `minItems` / `maxItems`.**
//! 10. **Response of kind `Accept` only includes declared fields;
//!     unknown fields are dropped.**

use std::collections::BTreeMap;

/// The kind-of-field discriminator. Matches the subset of JSON-Schema
/// types the MCP elicitation spec supports.
///
/// Does not derive `Eq` because the Number variant carries an `f64`
/// bound which is only `PartialEq`. Same applies transitively to
/// [`ElicitationField`] and [`ElicitationSchema`].
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationFieldKind {
    String {
        min_length: Option<usize>,
        max_length: Option<usize>,
        /// `format: "email"` / `format: "uri"` / `format: "date"` / etc.
        format: Option<String>,
        pattern: Option<String>,
    },
    Number {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Boolean,
    Enum {
        /// The ordered list of allowed values. Each entry is a display
        /// label paired with the wire value.
        options: Vec<EnumOption>,
    },
    Array {
        min_items: Option<usize>,
        max_items: Option<usize>,
        /// The kind of items in the array. Box to avoid infinite size.
        item_kind: Box<ElicitationFieldKind>,
    },
}

/// An enum option. Both fields can be the same string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumOption {
    pub label: String,
    pub value: String,
}

/// A single field in the elicitation schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationField {
    /// The field name (JSON key).
    pub name: String,
    /// The display title (falls back to `name`).
    pub title: Option<String>,
    /// The description / helper text.
    pub description: Option<String>,
    /// Whether the field is required.
    pub required: bool,
    /// The field kind with its constraints.
    pub kind: ElicitationFieldKind,
}

/// A parsed elicitation schema. Represents the JSON-Schema `properties`
/// + `required` block a parser walks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ElicitationSchema {
    /// The server-provided message describing why the elicitation
    /// is being requested.
    pub message: String,
    /// The ordered list of fields.
    pub fields: Vec<ElicitationField>,
}

/// A user-supplied value for a single field. The variant discrimin-
/// ates on the wire type; the validator normalizes.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    String(String),
    Number(f64),
    Integer(i64),
    Boolean(bool),
    Enum(String),
    Array(Vec<FieldValue>),
    /// Empty / not provided — used when the user left a field blank.
    None,
}

// NOTE: FieldValue does NOT implement `Eq` because of the `f64`
// payload in `Number`. Tests use `==` on the `PartialEq` impl.

/// A validation error for a single field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitationValidationError {
    pub field: String,
    pub message: String,
}

/// The response kind — accept with values, or decline / cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationResponseKind {
    /// User accepted; values map by field name.
    Accept,
    /// User explicitly declined — the MCP server should see "decline".
    Decline,
    /// User cancelled the dialog — the MCP server should see "cancel".
    Cancel,
}

/// The full response ready to send back to the MCP server.
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationResponse {
    pub kind: ElicitationResponseKind,
    /// Only populated when `kind == Accept`. Keyed by field name.
    pub values: BTreeMap<String, FieldValue>,
}

/// A parser seam: the consumer's JSON-Schema parser implements this
/// to feed schemas into the slice. The Rust implementation never touches
/// `serde_json` directly; the consumer calls the parser upstream
/// and hands us a built [`ElicitationSchema`]. This trait exists as
/// documentation for the seam shape.
pub trait ElicitationSchemaParser {
    type Error;
    /// Parse a raw JSON string into an [`ElicitationSchema`].
    fn parse(&self, raw: &str) -> Result<ElicitationSchema, Self::Error>;
}

/// Validate a single field value against its field definition.
///
/// Returns `Ok(())` on success. On failure returns a descriptive
/// error carrying the field name + message.
pub fn validate_field_value(
    field: &ElicitationField,
    value: &FieldValue,
) -> Result<(), ElicitationValidationError> {
    // Handle None / empty first.
    let is_empty =
        matches!(value, FieldValue::None) || matches!(value, FieldValue::String(s) if s.is_empty());

    if is_empty {
        if field.required {
            return Err(ElicitationValidationError {
                field: field.name.clone(),
                message: "This field is required".to_string(),
            });
        }
        return Ok(());
    }

    match (&field.kind, value) {
        // --- String ---
        (
            ElicitationFieldKind::String {
                min_length,
                max_length,
                format,
                pattern: _,
            },
            FieldValue::String(s),
        ) => {
            if let Some(min) = min_length {
                if s.chars().count() < *min {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must be at least {min} characters"),
                    });
                }
            }
            if let Some(max) = max_length {
                if s.chars().count() > *max {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must be at most {max} characters"),
                    });
                }
            }
            if let Some(fmt) = format {
                match fmt.as_str() {
                    "email" => {
                        if !s.contains('@') {
                            return Err(ElicitationValidationError {
                                field: field.name.clone(),
                                message: "Must be a valid email address".to_string(),
                            });
                        }
                    }
                    "uri" => {
                        if !s.contains("://") {
                            return Err(ElicitationValidationError {
                                field: field.name.clone(),
                                message: "Must be a valid URI".to_string(),
                            });
                        }
                    }
                    "date" => {
                        // YYYY-MM-DD shape check.
                        if s.len() != 10
                            || !s.chars().enumerate().all(|(i, c)| match i {
                                4 | 7 => c == '-',
                                _ => c.is_ascii_digit(),
                            })
                        {
                            return Err(ElicitationValidationError {
                                field: field.name.clone(),
                                message: "Must be a date in YYYY-MM-DD format".to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        // --- Number ---
        (ElicitationFieldKind::Number { minimum, maximum }, FieldValue::Number(n)) => {
            if let Some(min) = minimum {
                if n < min {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must be at least {min}"),
                    });
                }
            }
            if let Some(max) = maximum {
                if n > max {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must be at most {max}"),
                    });
                }
            }
            Ok(())
        }
        // --- Integer ---
        (ElicitationFieldKind::Integer { minimum, maximum }, FieldValue::Integer(n)) => {
            if let Some(min) = minimum {
                if n < min {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must be at least {min}"),
                    });
                }
            }
            if let Some(max) = maximum {
                if n > max {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must be at most {max}"),
                    });
                }
            }
            Ok(())
        }
        // Integer receiving a Number → reject if fractional.
        (ElicitationFieldKind::Integer { .. }, FieldValue::Number(n)) => {
            if n.fract() != 0.0 {
                return Err(ElicitationValidationError {
                    field: field.name.clone(),
                    message: "Must be an integer".to_string(),
                });
            }
            Ok(())
        }
        // --- Boolean ---
        (ElicitationFieldKind::Boolean, FieldValue::Boolean(_)) => Ok(()),
        // --- Enum ---
        (ElicitationFieldKind::Enum { options }, FieldValue::Enum(v)) => {
            if options.iter().any(|o| o.value == *v) {
                Ok(())
            } else {
                Err(ElicitationValidationError {
                    field: field.name.clone(),
                    message: format!("Must be one of: {}", enum_options_display(options)),
                })
            }
        }
        (ElicitationFieldKind::Enum { options }, FieldValue::String(v)) => {
            // A raw string is accepted as a substitute for the enum
            // value (the select widget writes a string).
            if options.iter().any(|o| o.value == *v) {
                Ok(())
            } else {
                Err(ElicitationValidationError {
                    field: field.name.clone(),
                    message: format!("Must be one of: {}", enum_options_display(options)),
                })
            }
        }
        // --- Array ---
        (
            ElicitationFieldKind::Array {
                min_items,
                max_items,
                item_kind,
            },
            FieldValue::Array(items),
        ) => {
            if let Some(min) = min_items {
                if items.len() < *min {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must have at least {min} items"),
                    });
                }
            }
            if let Some(max) = max_items {
                if items.len() > *max {
                    return Err(ElicitationValidationError {
                        field: field.name.clone(),
                        message: format!("Must have at most {max} items"),
                    });
                }
            }
            // Validate each item using a synthetic item-field.
            for (i, item) in items.iter().enumerate() {
                let item_field = ElicitationField {
                    name: format!("{}[{}]", field.name, i),
                    title: None,
                    description: None,
                    required: true, // array items are always required once present
                    kind: (**item_kind).clone(),
                };
                validate_field_value(&item_field, item)?;
            }
            Ok(())
        }
        // Type mismatch fallback.
        (_, _) => Err(ElicitationValidationError {
            field: field.name.clone(),
            message: "Value does not match field type".to_string(),
        }),
    }
}

fn enum_options_display(options: &[EnumOption]) -> String {
    options
        .iter()
        .map(|o| o.value.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validate an entire set of values against a schema. Returns all
/// errors (not just the first) so the UI can highlight every bad
/// field at once.
pub fn validate_schema(
    schema: &ElicitationSchema,
    values: &BTreeMap<String, FieldValue>,
) -> Vec<ElicitationValidationError> {
    let mut out = Vec::new();
    for field in &schema.fields {
        let value = values.get(&field.name).cloned().unwrap_or(FieldValue::None);
        if let Err(e) = validate_field_value(field, &value) {
            out.push(e);
        }
    }
    out
}

/// Build an accept-response from a validated set of values.
///
/// **Unknown fields are dropped** — only values for declared schema
/// fields are included in the response — `values` is filtered down to
/// the keys named by `schema.fields` (pinned by
/// `accept_response_drops_unknown_fields`).
///
/// If any field fails validation, returns `Err` with all the errors.
pub fn build_elicitation_response(
    schema: &ElicitationSchema,
    kind: ElicitationResponseKind,
    values: BTreeMap<String, FieldValue>,
) -> Result<ElicitationResponse, Vec<ElicitationValidationError>> {
    match kind {
        ElicitationResponseKind::Accept => {
            let errors = validate_schema(schema, &values);
            if !errors.is_empty() {
                return Err(errors);
            }
            // Drop unknown fields — only declared fields survive.
            let declared: std::collections::HashSet<&String> =
                schema.fields.iter().map(|f| &f.name).collect();
            let filtered: BTreeMap<String, FieldValue> = values
                .into_iter()
                .filter(|(k, _)| declared.contains(k))
                .collect();
            Ok(ElicitationResponse {
                kind: ElicitationResponseKind::Accept,
                values: filtered,
            })
        }
        ElicitationResponseKind::Decline => Ok(ElicitationResponse {
            kind: ElicitationResponseKind::Decline,
            values: BTreeMap::new(),
        }),
        ElicitationResponseKind::Cancel => Ok(ElicitationResponse {
            kind: ElicitationResponseKind::Cancel,
            values: BTreeMap::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_required_string(name: &str) -> ElicitationField {
        ElicitationField {
            name: name.to_string(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::String {
                min_length: None,
                max_length: None,
                format: None,
                pattern: None,
            },
        }
    }

    // --- Required / empty ---

    #[test]
    fn required_empty_string_rejected() {
        let f = mk_required_string("name");
        let err = validate_field_value(&f, &FieldValue::String("".into())).unwrap_err();
        assert_eq!(err.field, "name");
        assert_eq!(err.message, "This field is required");
    }

    #[test]
    fn required_none_rejected() {
        let f = mk_required_string("name");
        assert!(validate_field_value(&f, &FieldValue::None).is_err());
    }

    #[test]
    fn optional_empty_ok() {
        let mut f = mk_required_string("name");
        f.required = false;
        assert!(validate_field_value(&f, &FieldValue::None).is_ok());
        assert!(validate_field_value(&f, &FieldValue::String("".into())).is_ok());
    }

    // --- String min/max length ---

    #[test]
    fn string_min_length_enforced() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::String {
                min_length: Some(3),
                max_length: None,
                format: None,
                pattern: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::String("ab".into())).is_err());
        assert!(validate_field_value(&f, &FieldValue::String("abc".into())).is_ok());
    }

    #[test]
    fn string_max_length_enforced() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::String {
                min_length: None,
                max_length: Some(5),
                format: None,
                pattern: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::String("hello".into())).is_ok());
        assert!(validate_field_value(&f, &FieldValue::String("hello!".into())).is_err());
    }

    #[test]
    fn string_length_counts_characters_not_bytes() {
        // Unicode: a 2-char string of multibyte chars should still
        // pass min_length=2.
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::String {
                min_length: Some(2),
                max_length: Some(2),
                format: None,
                pattern: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::String("héllo".into())).is_err());
        // "日本" = 2 chars (6 bytes in UTF-8)
        assert!(validate_field_value(&f, &FieldValue::String("日本".into())).is_ok());
    }

    // --- Formats ---

    #[test]
    fn email_format_requires_at_sign() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::String {
                min_length: None,
                max_length: None,
                format: Some("email".into()),
                pattern: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::String("not-an-email".into())).is_err());
        assert!(validate_field_value(&f, &FieldValue::String("a@b".into())).is_ok());
    }

    #[test]
    fn uri_format_requires_scheme_separator() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::String {
                min_length: None,
                max_length: None,
                format: Some("uri".into()),
                pattern: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::String("example.com".into())).is_err());
        assert!(
            validate_field_value(&f, &FieldValue::String("https://example.com".into())).is_ok()
        );
    }

    #[test]
    fn date_format_requires_yyyy_mm_dd() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::String {
                min_length: None,
                max_length: None,
                format: Some("date".into()),
                pattern: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::String("2026-04-07".into())).is_ok());
        assert!(validate_field_value(&f, &FieldValue::String("2026/04/07".into())).is_err());
        assert!(validate_field_value(&f, &FieldValue::String("26-04-07".into())).is_err());
    }

    // --- Number / Integer ---

    #[test]
    fn number_min_max_inclusive() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Number {
                minimum: Some(0.0),
                maximum: Some(100.0),
            },
        };
        assert!(validate_field_value(&f, &FieldValue::Number(0.0)).is_ok());
        assert!(validate_field_value(&f, &FieldValue::Number(100.0)).is_ok());
        assert!(validate_field_value(&f, &FieldValue::Number(-0.1)).is_err());
        assert!(validate_field_value(&f, &FieldValue::Number(100.1)).is_err());
    }

    #[test]
    fn integer_rejects_fractional_number() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Integer {
                minimum: None,
                maximum: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::Number(3.5)).is_err());
        assert!(validate_field_value(&f, &FieldValue::Number(3.0)).is_ok());
        assert!(validate_field_value(&f, &FieldValue::Integer(3)).is_ok());
    }

    #[test]
    fn integer_min_max() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Integer {
                minimum: Some(1),
                maximum: Some(10),
            },
        };
        assert!(validate_field_value(&f, &FieldValue::Integer(0)).is_err());
        assert!(validate_field_value(&f, &FieldValue::Integer(1)).is_ok());
        assert!(validate_field_value(&f, &FieldValue::Integer(10)).is_ok());
        assert!(validate_field_value(&f, &FieldValue::Integer(11)).is_err());
    }

    // --- Boolean ---

    #[test]
    fn boolean_accepts_true_and_false() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Boolean,
        };
        assert!(validate_field_value(&f, &FieldValue::Boolean(true)).is_ok());
        assert!(validate_field_value(&f, &FieldValue::Boolean(false)).is_ok());
    }

    #[test]
    fn boolean_rejects_string() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Boolean,
        };
        assert!(validate_field_value(&f, &FieldValue::String("true".into())).is_err());
    }

    // --- Enum ---

    #[test]
    fn enum_accepts_listed_value() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Enum {
                options: vec![
                    EnumOption {
                        label: "Red".into(),
                        value: "red".into(),
                    },
                    EnumOption {
                        label: "Blue".into(),
                        value: "blue".into(),
                    },
                ],
            },
        };
        assert!(validate_field_value(&f, &FieldValue::Enum("red".into())).is_ok());
        assert!(validate_field_value(&f, &FieldValue::String("blue".into())).is_ok());
    }

    #[test]
    fn enum_rejects_unlisted_value() {
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Enum {
                options: vec![EnumOption {
                    label: "x".into(),
                    value: "x".into(),
                }],
            },
        };
        let err = validate_field_value(&f, &FieldValue::Enum("y".into())).unwrap_err();
        assert!(err.message.contains("Must be one of"));
    }

    // --- Array ---

    #[test]
    fn array_min_items_enforced() {
        let f = ElicitationField {
            name: "a".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Array {
                min_items: Some(2),
                max_items: None,
                item_kind: Box::new(ElicitationFieldKind::String {
                    min_length: None,
                    max_length: None,
                    format: None,
                    pattern: None,
                }),
            },
        };
        assert!(
            validate_field_value(&f, &FieldValue::Array(vec![FieldValue::String("x".into())]))
                .is_err()
        );
        assert!(validate_field_value(
            &f,
            &FieldValue::Array(vec![
                FieldValue::String("x".into()),
                FieldValue::String("y".into()),
            ])
        )
        .is_ok());
    }

    #[test]
    fn array_max_items_enforced() {
        let f = ElicitationField {
            name: "a".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Array {
                min_items: None,
                max_items: Some(2),
                item_kind: Box::new(ElicitationFieldKind::String {
                    min_length: None,
                    max_length: None,
                    format: None,
                    pattern: None,
                }),
            },
        };
        assert!(validate_field_value(
            &f,
            &FieldValue::Array(vec![
                FieldValue::String("x".into()),
                FieldValue::String("y".into()),
                FieldValue::String("z".into()),
            ])
        )
        .is_err());
    }

    #[test]
    fn array_validates_each_item() {
        let f = ElicitationField {
            name: "a".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Array {
                min_items: None,
                max_items: None,
                item_kind: Box::new(ElicitationFieldKind::Integer {
                    minimum: Some(0),
                    maximum: Some(100),
                }),
            },
        };
        assert!(validate_field_value(
            &f,
            &FieldValue::Array(vec![FieldValue::Integer(50), FieldValue::Integer(200)])
        )
        .is_err());
    }

    // --- validate_schema ---

    #[test]
    fn validate_schema_collects_all_errors() {
        let schema = ElicitationSchema {
            message: "Fill in".into(),
            fields: vec![
                mk_required_string("a"),
                mk_required_string("b"),
                mk_required_string("c"),
            ],
        };
        let mut values = BTreeMap::new();
        values.insert("a".into(), FieldValue::String("ok".into()));
        // b and c are missing.
        let errors = validate_schema(&schema, &values);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].field, "b");
        assert_eq!(errors[1].field, "c");
    }

    #[test]
    fn validate_schema_passes_when_all_ok() {
        let schema = ElicitationSchema {
            message: "x".into(),
            fields: vec![mk_required_string("a")],
        };
        let mut values = BTreeMap::new();
        values.insert("a".into(), FieldValue::String("ok".into()));
        assert!(validate_schema(&schema, &values).is_empty());
    }

    // --- build_elicitation_response ---

    #[test]
    fn accept_response_drops_unknown_fields() {
        let schema = ElicitationSchema {
            message: "x".into(),
            fields: vec![mk_required_string("declared")],
        };
        let mut values = BTreeMap::new();
        values.insert("declared".into(), FieldValue::String("ok".into()));
        values.insert("ghost".into(), FieldValue::String("unknown".into()));
        let resp =
            build_elicitation_response(&schema, ElicitationResponseKind::Accept, values).unwrap();
        assert_eq!(resp.values.len(), 1);
        assert!(resp.values.contains_key("declared"));
        assert!(!resp.values.contains_key("ghost"));
    }

    #[test]
    fn accept_response_validates() {
        let schema = ElicitationSchema {
            message: "x".into(),
            fields: vec![mk_required_string("a")],
        };
        let values = BTreeMap::new(); // missing "a"
        let result = build_elicitation_response(&schema, ElicitationResponseKind::Accept, values);
        assert!(result.is_err());
    }

    #[test]
    fn decline_response_has_no_values() {
        let schema = ElicitationSchema::default();
        let mut values = BTreeMap::new();
        values.insert("x".into(), FieldValue::String("y".into()));
        let resp =
            build_elicitation_response(&schema, ElicitationResponseKind::Decline, values).unwrap();
        assert_eq!(resp.kind, ElicitationResponseKind::Decline);
        assert!(resp.values.is_empty());
    }

    #[test]
    fn cancel_response_has_no_values() {
        let schema = ElicitationSchema::default();
        let resp =
            build_elicitation_response(&schema, ElicitationResponseKind::Cancel, BTreeMap::new())
                .unwrap();
        assert_eq!(resp.kind, ElicitationResponseKind::Cancel);
        assert!(resp.values.is_empty());
    }

    #[test]
    fn type_mismatch_is_error() {
        // Number field gets a String — reject.
        let f = ElicitationField {
            name: "n".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Number {
                minimum: None,
                maximum: None,
            },
        };
        assert!(validate_field_value(&f, &FieldValue::String("not a number".into())).is_err());
    }

    #[test]
    fn optional_field_passes_on_type_mismatch_when_empty() {
        let mut f = mk_required_string("n");
        f.required = false;
        // None is always accepted on optional fields.
        assert!(validate_field_value(&f, &FieldValue::None).is_ok());
    }

    #[test]
    fn accept_response_preserves_field_values() {
        let schema = ElicitationSchema {
            message: "x".into(),
            fields: vec![
                mk_required_string("name"),
                ElicitationField {
                    name: "age".into(),
                    title: None,
                    description: None,
                    required: true,
                    kind: ElicitationFieldKind::Integer {
                        minimum: Some(0),
                        maximum: Some(150),
                    },
                },
            ],
        };
        let mut values = BTreeMap::new();
        values.insert("name".into(), FieldValue::String("Alice".into()));
        values.insert("age".into(), FieldValue::Integer(30));
        let resp =
            build_elicitation_response(&schema, ElicitationResponseKind::Accept, values).unwrap();
        assert_eq!(resp.values.len(), 2);
        assert_eq!(
            resp.values.get("name"),
            Some(&FieldValue::String("Alice".into()))
        );
        assert_eq!(resp.values.get("age"), Some(&FieldValue::Integer(30)));
    }

    #[test]
    fn enum_options_display_joined_with_comma_space() {
        let opts = vec![
            EnumOption {
                label: "Red".into(),
                value: "red".into(),
            },
            EnumOption {
                label: "Blue".into(),
                value: "blue".into(),
            },
        ];
        assert_eq!(enum_options_display(&opts), "red, blue");
    }

    #[test]
    fn required_array_empty_is_error() {
        let f = ElicitationField {
            name: "a".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Array {
                min_items: None,
                max_items: None,
                item_kind: Box::new(ElicitationFieldKind::String {
                    min_length: None,
                    max_length: None,
                    format: None,
                    pattern: None,
                }),
            },
        };
        // None value for a required array.
        assert!(validate_field_value(&f, &FieldValue::None).is_err());
    }

    #[test]
    fn optional_array_empty_ok() {
        let f = ElicitationField {
            name: "a".into(),
            title: None,
            description: None,
            required: false,
            kind: ElicitationFieldKind::Array {
                min_items: None,
                max_items: None,
                item_kind: Box::new(ElicitationFieldKind::String {
                    min_length: None,
                    max_length: None,
                    format: None,
                    pattern: None,
                }),
            },
        };
        assert!(validate_field_value(&f, &FieldValue::None).is_ok());
    }

    #[test]
    fn empty_array_ok_when_no_min() {
        let f = ElicitationField {
            name: "a".into(),
            title: None,
            description: None,
            required: true,
            kind: ElicitationFieldKind::Array {
                min_items: None,
                max_items: None,
                item_kind: Box::new(ElicitationFieldKind::String {
                    min_length: None,
                    max_length: None,
                    format: None,
                    pattern: None,
                }),
            },
        };
        assert!(validate_field_value(&f, &FieldValue::Array(vec![])).is_ok());
    }
}
