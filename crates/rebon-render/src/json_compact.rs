//! Compact JSON value formatting for tool-call headers/bodies.
//!
//! One formatter for tool params and results, so no surface ever dumps a raw
//! JSON blob where a one-line summary belongs.

use serde_json::Value;

/// Render a JSON value compactly for a one-line summary: strings lose their
/// embedded CR/LF; everything else is its compact `to_string`.
pub fn compact_json_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.replace(['\r', '\n'], " "),
        other => other.to_string(),
    }
}

/// Returns `true` for JSON values that are "default-looking" and should be
/// omitted from the compact tool-call header to reduce noise: `null`,
/// booleans, `0`, and empty strings.
pub fn is_default_json_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(_) => true,
        Value::Number(n) => n.as_f64().map_or(false, |f| f == 0.0),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}
