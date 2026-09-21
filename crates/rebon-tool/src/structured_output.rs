//! Shared structured-result channel and validation used by workflow runtimes.
//! The model-facing tool itself lives with its plugin.

use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Structured-output state that [`crate::ToolContext`] carries in its extension bag.
///
/// Present only for workflow `agent(prompt, { schema })` workers. Storage
/// only — read and written through the unchanged
/// `structured_output_channel()` accessor.
#[derive(Clone, Default)]
pub struct StructuredOutputContext {
    pub channel: Option<Arc<StructuredOutputChannel>>,
}

pub const STRUCTURED_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

/// Per-sub-agent channel for the workflow `StructuredOutput` contract.
///
/// One channel is created per `agent(prompt, { schema })` call and
/// installed on the worker's [`crate::ToolContext`]. It carries the JSON
/// schema the worker must satisfy and records the validated value the
/// worker returns through the `StructuredOutput` tool. The spawner's
/// `WorkerDeliveryHook` reads [`Self::is_satisfied`] to decide whether
/// to coerce a worker that ended its turn without returning a result
/// into producing one.
#[derive(Debug, Default)]
pub struct StructuredOutputChannel {
    schema: Option<Value>,
    accepted: Mutex<Option<Value>>,
}

impl StructuredOutputChannel {
    pub fn new(schema: Option<Value>) -> Self {
        Self {
            schema,
            accepted: Mutex::new(None),
        }
    }

    /// The JSON schema the worker's structured output must satisfy.
    pub fn schema(&self) -> Option<&Value> {
        self.schema.as_ref()
    }

    /// Whether the worker has already produced a schema-valid result
    /// via the `StructuredOutput` tool this run.
    pub fn is_satisfied(&self) -> bool {
        self.accepted
            .lock()
            .expect("structured output slot poisoned")
            .is_some()
    }

    /// The validated value the worker returned, if any.
    pub fn accepted(&self) -> Option<Value> {
        self.accepted
            .lock()
            .expect("structured output slot poisoned")
            .clone()
    }

    /// Validate and record a worker result. Rejected values leave the slot intact.
    pub fn record(&self, value: Value) -> Result<(), String> {
        if let Some(schema) = self.schema() {
            validate_structured_output(schema, &value)?;
        }
        *self
            .accepted
            .lock()
            .expect("structured output slot poisoned") = Some(value);
        Ok(())
    }
}

/// Lightweight structural validation of a `StructuredOutput` value
/// against the per-agent JSON schema: enforces top-level `required`
/// keys and each declared property's `type`. Returns a human-readable
/// reason the worker can act on.
///
/// Shared by the tool's `call()` (which rejects bad shapes with `Err`
/// so the model retries) and the workflow runtime's post-hoc check, so
/// both agree on what "valid" means.
pub fn validate_structured_output(schema: &Value, value: &Value) -> Result<(), String> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };
    if let Some(expected) = schema_obj.get("type") {
        let matches_type = match expected {
            Value::String(kind) => json_type_matches(kind, value),
            Value::Array(kinds) => kinds
                .iter()
                .filter_map(Value::as_str)
                .any(|kind| json_type_matches(kind, value)),
            _ => true,
        };
        if !matches_type {
            return Err("value does not match the schema type".to_string());
        }
    }
    if let Some(required) = schema_obj.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if value.get(key).is_none() {
                return Err(format!("missing required property `{key}`"));
            }
        }
    }
    if let Some(properties) = schema_obj.get("properties").and_then(Value::as_object) {
        for (key, prop_schema) in properties {
            if let Some(actual) = value.get(key) {
                validate_json_type(key, prop_schema, actual)?;
            }
        }
    }
    Ok(())
}

fn validate_json_type(key: &str, schema: &Value, value: &Value) -> Result<(), String> {
    let Some(expected) = schema.get("type") else {
        return Ok(());
    };
    let matches_type = match expected {
        Value::String(kind) => json_type_matches(kind, value),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| json_type_matches(kind, value)),
        _ => true,
    };
    if matches_type {
        Ok(())
    } else {
        Err(format!("property `{key}` does not match the schema type"))
    }
}

fn json_type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "string" => value.is_string(),
        _ => true,
    }
}
