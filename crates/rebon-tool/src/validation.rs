//! Centralised JSON Schema subset validator.
//!
//! Implements the narrow slice of JSON Schema that the 32 rebon tools
//! actually use in their `input_schema()` definitions:
//!
//! - `type: "object"` envelope check
//! - `required: [...]` — missing required keys
//! - `properties.*.type` — type validation for string, integer, number,
//!   boolean, array, object
//! - `additionalProperties: false` — unexpected keys
//! - `enum: [...]` — string enum membership
//! - nested `items: { type: "object", ... }` for array-of-objects
//!
//! This avoids pulling in the `jsonschema` crate (15+ transitive deps)
//! while still sorting every failure into the three categories the
//! harness reports: missing required params, unexpected params, and
//! type mismatches.

use rebon_tools_core::{InputValidationError, TypeMismatch};
use serde_json::Value;

/// Validate `input` against the tool's JSON Schema `schema`.
///
/// Returns `Ok(())` when the input conforms, or
/// `Err(InputValidationError)` carrying the problems split into missing
/// params, unexpected params, and type mismatches.
pub fn validate_schema(
    tool_name: &str,
    schema: &Value,
    input: &Value,
) -> Result<(), InputValidationError> {
    let mut err = InputValidationError::new(tool_name);

    // The schema must declare type: "object" for us to do anything.
    let schema_type = schema.get("type").and_then(Value::as_str);
    if schema_type != Some("object") {
        // Not an object schema — nothing to validate.
        return Ok(());
    }

    // Input must be an object.
    let input_obj = match input.as_object() {
        Some(obj) => obj,
        None => {
            err.type_mismatches.push(TypeMismatch {
                param: "(root)".into(),
                expected: "object".into(),
                received: json_type_name(input).into(),
            });
            return Err(err);
        }
    };

    let properties = schema.get("properties").and_then(Value::as_object);

    // Check required fields.
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for req in required {
            if let Some(name) = req.as_str() {
                if !input_obj.contains_key(name) {
                    err.missing_params.push(name.to_string());
                }
            }
        }
    }

    // Check additionalProperties: false.
    let deny_extra = schema.get("additionalProperties").and_then(Value::as_bool) == Some(false);

    if deny_extra {
        if let Some(props) = properties {
            for key in input_obj.keys() {
                if !props.contains_key(key) {
                    err.unexpected_params.push(key.clone());
                }
            }
        }
    }

    // Check property types and enum constraints.
    if let Some(props) = properties {
        for (key, prop_schema) in props {
            if let Some(value) = input_obj.get(key) {
                // Skip null for optional fields (not in required list).
                if value.is_null() {
                    continue;
                }
                // Type check.
                if let Some(expected_type) = prop_schema.get("type").and_then(Value::as_str) {
                    if !value_matches_type(value, expected_type) {
                        err.type_mismatches.push(TypeMismatch {
                            param: key.clone(),
                            expected: expected_type.into(),
                            received: json_type_name(value).into(),
                        });
                    }
                }
                // Enum check (string enums only).
                if let Some(enum_vals) = prop_schema.get("enum").and_then(Value::as_array) {
                    if let Some(s) = value.as_str() {
                        let matches = enum_vals.iter().any(|e| e.as_str() == Some(s));
                        if !matches {
                            let allowed: Vec<&str> =
                                enum_vals.iter().filter_map(Value::as_str).collect();
                            err.type_mismatches.push(TypeMismatch {
                                param: key.clone(),
                                expected: format!("one of [{}]", allowed.join(", ")),
                                received: format!("\"{s}\""),
                            });
                        }
                    }
                }
                // Nested array items validation.
                if let Some(items_schema) = prop_schema.get("items") {
                    if let Value::Array(items) = value {
                        validate_array_items(key, items, items_schema, &mut err);
                    }
                }
            }
        }
    }

    if err.is_empty() {
        Ok(())
    } else {
        Err(err)
    }
}

/// Validate each element in an array against `items_schema`.
fn validate_array_items(
    parent_key: &str,
    items: &[Value],
    items_schema: &Value,
    err: &mut InputValidationError,
) {
    let item_type = items_schema.get("type").and_then(Value::as_str);

    for (i, item) in items.iter().enumerate() {
        if let Some(expected) = item_type {
            if !value_matches_type(item, expected) {
                err.type_mismatches.push(TypeMismatch {
                    param: format!("{parent_key}[{i}]"),
                    expected: expected.into(),
                    received: json_type_name(item).into(),
                });
            }
        }
    }
}

/// Check whether a JSON value matches the declared JSON Schema type.
fn value_matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "integer" => {
            // JSON has no native integer type — accept any number that
            // is a whole number (including 5.0).
            value.as_i64().is_some() || value.as_u64().is_some()
        }
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true, // Unknown type — pass through.
    }
}

/// Return a human-readable type name for a JSON value.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_f64() && n.as_i64().is_none() && n.as_u64().is_none() {
                "number"
            } else {
                "integer"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            },
            "required": ["file_path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn valid_input_passes() {
        let schema = read_schema();
        let input = json!({ "file_path": "/foo/bar.rs" });
        assert!(validate_schema("Read", &schema, &input).is_ok());
    }

    #[test]
    fn valid_input_with_optional_fields_passes() {
        let schema = read_schema();
        let input = json!({ "file_path": "/foo/bar.rs", "offset": 10, "limit": 50 });
        assert!(validate_schema("Read", &schema, &input).is_ok());
    }

    #[test]
    fn missing_required_field_detected() {
        let schema = read_schema();
        let input = json!({ "offset": 10 });
        let err = validate_schema("Read", &schema, &input).unwrap_err();
        assert_eq!(err.missing_params, vec!["file_path"]);
        assert!(err
            .format()
            .contains("The required parameter `file_path` is missing"));
    }

    #[test]
    fn unexpected_field_detected() {
        let schema = read_schema();
        let input = json!({ "file_path": "/f", "unknown": true });
        let err = validate_schema("Read", &schema, &input).unwrap_err();
        assert_eq!(err.unexpected_params, vec!["unknown"]);
    }

    #[test]
    fn type_mismatch_detected() {
        let schema = read_schema();
        let input = json!({ "file_path": 42 });
        let err = validate_schema("Read", &schema, &input).unwrap_err();
        assert_eq!(err.type_mismatches.len(), 1);
        assert_eq!(err.type_mismatches[0].param, "file_path");
        assert_eq!(err.type_mismatches[0].expected, "string");
        assert_eq!(err.type_mismatches[0].received, "integer");
    }

    #[test]
    fn integer_accepts_whole_numbers() {
        let schema = read_schema();
        let input = json!({ "file_path": "/f", "offset": 10 });
        assert!(validate_schema("Read", &schema, &input).is_ok());
    }

    #[test]
    fn integer_rejects_strings() {
        let schema = read_schema();
        let input = json!({ "file_path": "/f", "offset": "ten" });
        let err = validate_schema("Read", &schema, &input).unwrap_err();
        assert_eq!(err.type_mismatches[0].param, "offset");
    }

    #[test]
    fn non_object_input_rejected() {
        let schema = read_schema();
        let input = json!("not an object");
        let err = validate_schema("Read", &schema, &input).unwrap_err();
        assert_eq!(err.type_mismatches[0].param, "(root)");
        assert_eq!(err.type_mismatches[0].expected, "object");
    }

    #[test]
    fn enum_validation_works() {
        let schema = json!({
            "type": "object",
            "properties": {
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"]
                }
            },
            "additionalProperties": false
        });
        // Valid enum value
        let input = json!({ "output_mode": "content" });
        assert!(validate_schema("Grep", &schema, &input).is_ok());

        // Invalid enum value
        let input = json!({ "output_mode": "invalid" });
        let err = validate_schema("Grep", &schema, &input).unwrap_err();
        assert_eq!(err.type_mismatches.len(), 1);
        assert!(err.type_mismatches[0].expected.contains("content"));
    }

    #[test]
    fn array_type_property_validated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tools": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "additionalProperties": false
        });
        // Valid array of strings
        let input = json!({ "tools": ["Read", "Grep"] });
        assert!(validate_schema("Agent", &schema, &input).is_ok());

        // Array type check
        let input = json!({ "tools": "Read" });
        let err = validate_schema("Agent", &schema, &input).unwrap_err();
        assert_eq!(err.type_mismatches[0].expected, "array");
    }

    #[test]
    fn array_items_type_checked() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tools": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "additionalProperties": false
        });
        let input = json!({ "tools": ["Read", 42] });
        let err = validate_schema("Agent", &schema, &input).unwrap_err();
        assert_eq!(err.type_mismatches[0].param, "tools[1]");
        assert_eq!(err.type_mismatches[0].expected, "string");
    }

    #[test]
    fn null_optional_field_passes() {
        let schema = read_schema();
        let input = json!({ "file_path": "/f", "offset": null });
        assert!(validate_schema("Read", &schema, &input).is_ok());
    }

    #[test]
    fn boolean_type_validated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "run_in_background": { "type": "boolean" }
            },
            "additionalProperties": false
        });
        let input = json!({ "run_in_background": "true" });
        let err = validate_schema("Agent", &schema, &input).unwrap_err();
        assert_eq!(err.type_mismatches[0].expected, "boolean");
        assert_eq!(err.type_mismatches[0].received, "string");
    }

    #[test]
    fn multiple_errors_accumulated() {
        let schema = read_schema();
        let input = json!({ "offset": "bad", "unknown": true });
        let err = validate_schema("Read", &schema, &input).unwrap_err();
        assert_eq!(err.missing_params.len(), 1); // file_path
        assert_eq!(err.unexpected_params.len(), 1); // unknown
        assert_eq!(err.type_mismatches.len(), 1); // offset
    }

    #[test]
    fn non_object_schema_passes_through() {
        let schema = json!({ "type": "string" });
        let input = json!("anything");
        assert!(validate_schema("Echo", &schema, &input).is_ok());
    }

    #[test]
    fn bash_schema_validates_correctly() {
        let schema = json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "minimum": 1 },
                "run_in_background": { "type": "boolean" },
                "dangerouslyDisableSandbox": { "type": "boolean" }
            },
            "required": ["command"],
            "additionalProperties": false
        });

        // Good input
        let input = json!({ "command": "ls", "timeout": 5000 });
        assert!(validate_schema("Bash", &schema, &input).is_ok());

        // Missing required + type mismatch
        let input = json!({ "timeout": "abc" });
        let err = validate_schema("Bash", &schema, &input).unwrap_err();
        assert!(err.missing_params.contains(&"command".to_string()));
        assert_eq!(err.type_mismatches[0].param, "timeout");
    }
}
