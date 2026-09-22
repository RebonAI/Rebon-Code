//! Tool-call header titles and one-line parameter summaries.
//!
//! One builder for tool-call titles and one-line summaries, shared by every
//! surface. Uses the centralized `rebon_tools_core::primary_display_params`
//! table so a new tool only needs an entry there. Pure (serde_json +
//! rebon-tools-core).

use std::collections::HashMap;

use serde_json::Value;

use crate::json_compact::{compact_json_value, is_default_json_value};
use crate::plan_ledger::{plan_ledger_summary, PLAN_LEDGER_TOOL_NAME};

/// Max char-length of a single param value before truncation.
const VALUE_MAX_CHARS: usize = 40;
/// Max total char-length of the `(key=val, key=val, …)` summary.
/// Large enough to show a full file path plus a couple of extra params.
pub const SUMMARY_MAX_CHARS: usize = 200;

/// Keys whose values are file paths and should never be truncated —
/// a truncated path loses its most important prefix.
fn is_path_key(key: &str) -> bool {
    matches!(key, "file_path" | "path")
}

pub fn format_sleep_duration_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    let millis = ms % 1000;
    if millis == 0 {
        return format!("{seconds}s");
    }

    let mut fraction = format!("{millis:03}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{seconds}.{fraction}s")
}

/// Truncate a param value, skipping truncation for path-type keys.
pub fn truncate_param_value(key: &str, raw: &str) -> String {
    if is_path_key(key) {
        raw.to_string()
    } else {
        truncate_tail(raw, VALUE_MAX_CHARS)
    }
}

pub fn compact_json_map(map: &HashMap<String, Value>) -> String {
    let mut entries: Vec<_> = map
        .iter()
        .filter(|(_, v)| !is_default_json_value(v))
        .collect();
    // Content-rich params first (longer values are more likely
    // to be user-provided). Ties broken alphabetically.
    entries.sort_by(|(ka, va), (kb, vb)| {
        let la = compact_json_value(va).chars().count();
        let lb = compact_json_value(vb).chars().count();
        lb.cmp(&la).then_with(|| ka.cmp(kb))
    });
    let mut parts: Vec<String> = Vec::new();
    let mut total: usize = 0;
    for (k, v) in entries {
        let raw = compact_json_value(v);
        let val = truncate_param_value(k, &raw);
        let entry = format!("{k}={val}");
        let entry_chars = entry.chars().count();
        let needed = if parts.is_empty() {
            entry_chars
        } else {
            entry_chars + 2 // ", "
        };
        if total + needed > SUMMARY_MAX_CHARS && !parts.is_empty() {
            parts.push("\u{2026}".into()); // …
            break;
        }
        total += needed;
        parts.push(entry);
    }
    parts.join(", ")
}

/// Build a concise streaming summary for a tool call, showing only
/// the primary parameter value(s) without key names.
///
/// Uses the centralized `primary_display_params` table from `rebon-tools-core`
/// so a new tool only needs to add an entry there.
/// Unknown tools fall back to `compact_json_map` (key=value pairs).
///
/// Primary values are shown in full — they are the main signal for the
/// tool call (bash command, grep pattern, agent description, …), and a
/// mid-string ellipsis destroys more meaning than it saves.
pub fn streaming_tool_summary(tool_name: &str, map: &HashMap<String, Value>) -> String {
    fn get_val(map: &HashMap<String, Value>, key: &str) -> Option<String> {
        map.get(key)
            .filter(|v| !is_default_json_value(v))
            .map(compact_json_value)
    }

    if tool_name == "InvokeDeferredTool" {
        let inner_tool = map.get("tool_name").and_then(Value::as_str).unwrap_or("");
        if let Some(inner_args) = map.get("arguments").and_then(Value::as_object) {
            let inner_map: HashMap<String, Value> = inner_args
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            return streaming_tool_summary(inner_tool, &inner_map);
        }
        return compact_json_map(map);
    }

    if tool_name == PLAN_LEDGER_TOOL_NAME {
        return plan_ledger_summary(
            map.get("operation").and_then(Value::as_str),
            map.get("items"),
            None,
        );
    }

    if tool_name == "WebSearch" {
        return map
            .get("query")
            .filter(|value| !is_default_json_value(value))
            .map(Value::to_string)
            .unwrap_or_default();
    }

    if tool_name == "Sleep" {
        if let Some(duration_ms) = map.get("duration_ms").and_then(Value::as_u64) {
            return format_sleep_duration_ms(duration_ms);
        }
    }

    if tool_name == "SendMessage" {
        let to = get_val(map, "to").unwrap_or_default();
        let preview = get_val(map, "summary")
            .or_else(|| get_val(map, "message").map(|m| truncate_tail(&m, VALUE_MAX_CHARS)))
            .unwrap_or_default();
        return if preview.is_empty() {
            format!("\u{2192} {to}")
        } else {
            format!("\u{2192} {to}: {preview}")
        };
    }

    if tool_name == "ResolveEscalation" {
        let agent = get_val(map, "agent_id").unwrap_or_default();
        let answer = get_val(map, "answer").unwrap_or_default();
        return if answer.is_empty() {
            format!("\u{2192} {agent}")
        } else {
            format!("\u{2192} {agent}: {answer}")
        };
    }

    match rebon_tools_core::primary_display_params(tool_name) {
        Some(keys) => {
            let parts: Vec<String> = keys.iter().filter_map(|k| get_val(map, k)).collect();
            parts.join(", ")
        }
        None => compact_json_map(map),
    }
}

/// Build verbose summary for `InvokeDeferredTool`, showing all inner
/// arguments as `Key: value` pairs separated by newlines.
pub fn deferred_tool_verbose_summary(map: &HashMap<String, Value>) -> Option<String> {
    let args = map.get("arguments")?.as_object()?;
    if args.is_empty() {
        return None;
    }
    let mut entries: Vec<_> = args
        .iter()
        .filter(|(_, v)| !is_default_json_value(v))
        .collect();
    entries.sort_by(|(ka, _), (kb, _)| ka.cmp(kb));
    let parts: Vec<String> = entries
        .into_iter()
        .map(|(k, v)| {
            let val = compact_json_value(v);
            let display_key = snake_to_title(k);
            format!("{display_key}: {val}")
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n"))
}

fn snake_to_title(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{upper}{}", chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one decision about what to call a tool in a transcript row, for every
/// caller that has the input as a map.
pub fn streaming_tool_display_name<'a>(
    tool_name: &'a str,
    raw_input: Option<&'a HashMap<String, Value>>,
) -> &'a str {
    display_name_for_tool(tool_name, |key| {
        raw_input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
    })
}

/// [`streaming_tool_display_name`] for callers holding the tool input as a
/// `serde_json::Value` — the same names for the same tools.
pub fn streaming_tool_display_name_for_value<'a>(
    tool_name: &'a str,
    raw_input: &'a Value,
) -> &'a str {
    display_name_for_tool(tool_name, |key| raw_input.get(key).and_then(Value::as_str))
}

fn display_name_for_tool<'a>(
    tool_name: &'a str,
    input_field: impl Fn(&str) -> Option<&'a str>,
) -> &'a str {
    match tool_name {
        crate::code_mode::RUN_CODE_TOOL_NAME => "Run sequence",
        "Agent" => input_field("subagent_type")
            .or_else(|| input_field("subagentType"))
            .map(str::trim)
            .filter(|agent_type| !agent_type.is_empty() && *agent_type != "general-purpose")
            .unwrap_or(tool_name),
        "InvokeDeferredTool" => input_field("tool_name")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(tool_name),
        _ => tool_name,
    }
}

/// Truncate `s` to at most `max` characters, keeping the tail
/// (more informative for paths/patterns) and prepending "…".
fn truncate_tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - (max - 1)..].iter().collect();
    format!("\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    #[test]
    fn run_code_display_label_does_not_change_canonical_name_or_summary() {
        let input = HashMap::from([
            ("description".into(), json!("Inspect tasks")),
            ("code".into(), json!("return 1;")),
        ]);
        assert_eq!(
            streaming_tool_display_name("run_code", None),
            "Run sequence"
        );
        assert_eq!(
            streaming_tool_display_name("run_code", Some(&input)),
            "Run sequence"
        );
        assert_eq!(streaming_tool_summary("run_code", &input), "Inspect tasks");
        assert_eq!(crate::code_mode::RUN_CODE_TOOL_NAME, "run_code");
        assert_eq!(streaming_tool_display_name("Bash", None), "Bash");
    }

    #[test]
    fn deferred_tool_display_name_extracts_inner_tool() {
        let mut input = HashMap::new();
        input.insert("tool_name".into(), json!("WebSearch"));
        input.insert(
            "arguments".into(),
            json!({"query": "rust async", "search_context_size": "high"}),
        );
        assert_eq!(
            streaming_tool_display_name("InvokeDeferredTool", Some(&input)),
            "WebSearch"
        );
    }

    #[test]
    fn deferred_web_search_summary_shows_quoted_query() {
        let mut map = HashMap::new();
        map.insert("tool_name".into(), json!("WebSearch"));
        map.insert(
            "arguments".into(),
            json!({"query": "rust async", "search_context_size": "high"}),
        );
        let summary = streaming_tool_summary("InvokeDeferredTool", &map);
        assert_eq!(summary, "\"rust async\"");
    }

    #[test]
    fn deferred_tool_verbose_shows_all_inner_args() {
        let mut map = HashMap::new();
        map.insert("tool_name".into(), json!("WebSearch"));
        map.insert(
            "arguments".into(),
            json!({"query": "rust async", "search_context_size": "high"}),
        );
        let verbose = deferred_tool_verbose_summary(&map).unwrap();
        assert!(verbose.contains("Query: rust async"));
        assert!(verbose.contains("Search Context Size: high"));
    }

    #[test]
    fn plan_ledger_summary_uses_operation_and_item_count() {
        let mut map = HashMap::new();
        map.insert("operation".into(), json!("set_requirements"));
        map.insert(
            "items".into(),
            json!([
                {"id": "R1", "title": "One"},
                {"id": "R2", "title": "Two"}
            ]),
        );
        assert_eq!(
            streaming_tool_summary(PLAN_LEDGER_TOOL_NAME, &map),
            "set_requirements · 2 items"
        );
    }

    #[test]
    fn sleep_summary_formats_milliseconds_as_seconds() {
        let mut map = HashMap::new();
        map.insert("duration_ms".into(), json!(3000));
        assert_eq!(streaming_tool_summary("Sleep", &map), "3s");

        map.insert("duration_ms".into(), json!(1500));
        assert_eq!(streaming_tool_summary("Sleep", &map), "1.5s");

        map.insert("duration_ms".into(), json!(250));
        assert_eq!(streaming_tool_summary("Sleep", &map), "0.25s");
    }

    #[test]
    fn send_message_summary_shows_arrow_recipient_and_preview() {
        let mut map = HashMap::new();
        map.insert("to".into(), json!("researcher"));
        map.insert("summary".into(), json!("assign task 1"));
        map.insert("message".into(), json!("Start on task #1 please"));
        let summary = streaming_tool_summary("SendMessage", &map);
        assert_eq!(summary, "\u{2192} researcher: assign task 1");
    }

    #[test]
    fn send_message_summary_falls_back_to_message_when_no_summary() {
        let mut map = HashMap::new();
        map.insert("to".into(), json!("worker"));
        map.insert("message".into(), json!("do the thing"));
        let summary = streaming_tool_summary("SendMessage", &map);
        assert_eq!(summary, "\u{2192} worker: do the thing");
    }

    #[test]
    fn resolve_escalation_summary_shows_arrow_agent_and_answer() {
        let mut map = HashMap::new();
        map.insert("escalation_id".into(), json!("esc-123"));
        map.insert("agent_id".into(), json!("agent-1"));
        map.insert("answer".into(), json!("Yes, proceed with option A"));
        map.insert("source".into(), json!("coordinator"));
        let summary = streaming_tool_summary("ResolveEscalation", &map);
        assert_eq!(summary, "\u{2192} agent-1: Yes, proceed with option A");
    }

    #[test]
    fn streaming_tool_summary_does_not_truncate_primary_bash_command() {
        // Longer than VALUE_MAX_CHARS, so a tail truncation would be visible.
        let long_cmd = "wc -l \"/repo/crates/example/src/render_pipeline.rs\"";
        let mut map = HashMap::new();
        map.insert("command".into(), json!(long_cmd));
        let summary = streaming_tool_summary("Bash", &map);
        assert_eq!(summary, long_cmd);
        assert!(
            !summary.starts_with('\u{2026}'),
            "primary value must not be tail-truncated: {summary:?}"
        );
    }

    #[test]
    fn streaming_tool_summary_uses_command_for_powershell() {
        let command = "Get-ChildItem Env:";
        let mut map = HashMap::new();
        map.insert("command".into(), json!(command));
        map.insert("timeout".into(), json!(60000));
        assert_eq!(streaming_tool_summary("PowerShell", &map), command);
    }
}
