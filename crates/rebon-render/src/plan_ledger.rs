use serde_json::Value;

use crate::kind::ToolOutputVerbosity;

pub const PLAN_LEDGER_TOOL_NAME: &str = "PlanLedger";

pub fn plan_ledger_summary(
    operation: Option<&str>,
    input_items: Option<&Value>,
    result_requirements: Option<&Value>,
) -> String {
    let operation = operation.unwrap_or("unknown");
    let count_source = if matches!(operation, "set_requirements" | "add") {
        input_items
    } else {
        result_requirements.or(input_items)
    };
    let count = count_source
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    format!("{operation} · {count} items")
}

pub fn plan_ledger_requirement_lines(
    items: Option<&Value>,
    verbosity: ToolOutputVerbosity,
) -> Vec<String> {
    let requirements = items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let item = item.as_object()?;
            let id = item.get("id")?.as_str()?.trim();
            let title = item.get("title")?.as_str()?.trim();
            (!id.is_empty() && !title.is_empty()).then(|| format!("{id}  {title}"))
        })
        .collect::<Vec<_>>();
    let visible_count = if matches!(verbosity, ToolOutputVerbosity::Verbose) {
        requirements.len()
    } else {
        requirements.len().min(4)
    };
    let mut lines = requirements
        .iter()
        .take(visible_count)
        .cloned()
        .collect::<Vec<_>>();
    let hidden = requirements.len().saturating_sub(visible_count);
    if hidden > 0 {
        lines.push(format!("… +{hidden} requirements"));
    }
    lines
}

pub fn plan_ledger_result_from_text(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text.trim())
        .ok()
        .filter(Value::is_object)
}

pub fn plan_ledger_error_lines(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let mut lines = Vec::new();
        collect_plan_ledger_error_strings(&value, &mut lines);
        if lines.is_empty() {
            lines.push("PlanLedger failed".to_string());
        }
        return lines;
    }
    text.lines().map(str::to_string).collect()
}

fn collect_plan_ledger_error_strings(value: &Value, lines: &mut Vec<String>) {
    match value {
        Value::String(text) => lines.extend(text.lines().map(str::to_string)),
        Value::Array(values) => {
            for value in values {
                collect_plan_ledger_error_strings(value, lines);
            }
        }
        Value::Object(map) => {
            for key in ["error", "message", "reason", "detail", "details"] {
                if let Some(value) = map.get(key) {
                    collect_plan_ledger_error_strings(value, lines);
                }
            }
        }
        _ => {}
    }
}

pub fn plan_ledger_status_line(
    operation: Option<&str>,
    round: Option<&Value>,
    interview_revision: Option<&Value>,
    understanding_seal: Option<&Value>,
    has_result: bool,
) -> Option<String> {
    if !has_result {
        return None;
    }
    let action = match operation? {
        "set_requirements" | "add" => "updated",
        "list" => "listed",
        "seal_understanding" if understanding_seal.is_some_and(|seal| !seal.is_null()) => "sealed",
        "seal_understanding" => return None,
        _ => return None,
    };
    let mut parts = vec![action.to_string()];
    if let Some(round) = plan_ledger_number(round) {
        parts.push(format!("round {round}"));
    }
    if let Some(revision) = plan_ledger_number(interview_revision) {
        parts.push(format!("interview revision {revision}"));
    }
    Some(parts.join(" · "))
}

fn plan_ledger_number(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value
        .as_u64()
        .map(|number| number.to_string())
        .or_else(|| value.as_i64().map(|number| number.to_string()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn requirements() -> Value {
        json!([
            {"id": "R1", "title": "One", "source": "hidden"},
            {"id": "R2", "title": "Two"},
            {"id": "R3", "title": "Three"},
            {"id": "R4", "title": "Four"},
            {"id": "R5", "title": "Five"},
            {"id": "R6", "title": "Six"}
        ])
    }

    #[test]
    fn summaries_count_input_and_result_requirements() {
        let items = requirements();
        assert_eq!(
            plan_ledger_summary(Some("set_requirements"), Some(&items), None),
            "set_requirements · 6 items"
        );
        assert_eq!(
            plan_ledger_summary(Some("list"), None, Some(&items)),
            "list · 6 items"
        );
    }

    #[test]
    fn compact_and_normal_limit_requirements_while_verbose_is_complete() {
        let items = requirements();
        for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
            let lines = plan_ledger_requirement_lines(Some(&items), verbosity);
            assert_eq!(lines.len(), 5);
            assert_eq!(lines.last().map(String::as_str), Some("… +2 requirements"));
            assert!(lines.iter().all(|line| !line.contains("hidden")));
        }
        assert_eq!(
            plan_ledger_requirement_lines(Some(&items), ToolOutputVerbosity::Verbose).len(),
            6
        );
    }

    #[test]
    fn result_text_only_accepts_complete_json_objects() {
        assert_eq!(
            plan_ledger_result_from_text(r#"{"requirements":[]}"#)
                .and_then(|value| value.get("requirements").cloned()),
            Some(json!([]))
        );
        assert!(plan_ledger_result_from_text("completed").is_none());
        assert!(plan_ledger_result_from_text("[]").is_none());
    }

    #[test]
    fn json_errors_only_expose_readable_error_fields() {
        let lines = plan_ledger_error_lines(
            r#"{"error":{"message":"invalid requirement"},"requirements":[{"id":"R1"}],"manifest":"secret"}"#,
        );
        assert_eq!(lines, ["invalid requirement"]);
    }

    #[test]
    fn status_requires_a_result_and_a_real_seal() {
        assert_eq!(
            plan_ledger_status_line(
                Some("set_requirements"),
                Some(&json!(2)),
                Some(&json!(3)),
                None,
                true
            )
            .as_deref(),
            Some("updated · round 2 · interview revision 3")
        );
        assert_eq!(
            plan_ledger_status_line(
                Some("seal_understanding"),
                None,
                None,
                Some(&Value::Null),
                true
            ),
            None
        );
        assert_eq!(
            plan_ledger_status_line(
                Some("seal_understanding"),
                None,
                None,
                Some(&json!({"hash": "secret"})),
                true
            )
            .as_deref(),
            Some("sealed")
        );
    }
}
