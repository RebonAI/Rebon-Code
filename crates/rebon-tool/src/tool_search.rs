//! ToolSearchTool — on-demand discovery of deferred tools.
//!
//! Deferral contract. Tools marked with `should_defer() == true` are *not*
//! announced to the model upfront — only their names are listed in
//! the system prompt. The model calls ToolSearchTool to load their
//! full schemas before invoking them.
//!
//! ## Query forms
//!
//! | Form | Example | Behaviour |
//! |------|---------|-----------|
//! | `select:` | `select:Read,Edit,Grep` | Direct name lookup |
//! | keywords | `notebook jupyter` | Fuzzy scored search |
//! | `+required` | `+slack send` | Require "slack", rank by "send" |

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use rebon_tools_core::{ToolError, ToolId, ToolInputSchema};

use crate::{Tool, ToolContext, ToolFilter, ToolResult};
use std::collections::HashSet;
use std::sync::Mutex;

/// Deferred-tool discovery state that [`ToolContext`] carries in its
/// extension bag.
///
/// `discovered` is shared by every per-call clone in a query so a name
/// returned by ToolSearch stays discovered for the rest of the session.
/// Storage only — read and written through the unchanged
/// `tool_search_index()` / `record_discovered_deferred_tool()` accessors.
#[derive(Clone, Default)]
pub struct ToolSearchContext {
    pub index: Option<Arc<ToolSearchIndex>>,
    pub discovered: Option<Arc<Mutex<HashSet<String>>>>,
}

// ── Constants ────────────────────────────────────────────────────

pub const TOOL_SEARCH_TOOL_NAME: &str = "ToolSearch";

/// Check if tool search is enabled via `REBON_ENABLE_TOOL_SEARCH`.
///
/// | Value | Behaviour |
/// |-------|-----------|
/// | unset / `"true"` / `"1"` | enabled (default) |
/// | `"false"` / `"0"` | disabled — all tools sent inline |
pub fn is_tool_search_enabled() -> bool {
    // Enabled unless explicitly switched off.
    !rebon_types::env::env_defined_falsy("REBON_ENABLE_TOOL_SEARCH")
}

// ── ToolSearchIndex ──────────────────────────────────────────────

/// Pre-parsed metadata about a single deferred tool. Built once
/// when the engine starts; read on every ToolSearchTool call.
#[derive(Debug, Clone)]
pub struct ToolSearchEntry {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub search_hint: Option<String>,
    /// Tokenised name parts for keyword matching.
    /// `mcp__slack__post_message` → `["mcp", "slack", "post", "message"]`
    /// `TeamCreateTool` → `["team", "create", "tool"]`
    pub parsed_parts: Vec<String>,
    /// Lower-cased full name for substring fallback.
    pub full_lower: String,
}

/// Searchable index over deferred tools. Shared across the engine
/// via `Arc<ToolSearchIndex>` and injected into [`ToolContext`].
#[derive(Debug, Clone, Default)]
pub struct ToolSearchIndex {
    entries: Vec<ToolSearchEntry>,
}

impl ToolSearchIndex {
    /// Build an index from the given tools. Only tools where
    /// `should_defer() == true && is_enabled()` are indexed.
    pub fn build(tools: &[Arc<dyn Tool>]) -> Self {
        Self::build_filtered(tools, None)
    }

    /// Build an index from deferred enabled tools that also pass the supplied
    /// visibility filter. Used to ensure ToolSearch cannot rediscover tools
    /// hidden by a session filter or request-scoped policy.
    pub fn build_filtered(tools: &[Arc<dyn Tool>], filter: Option<&ToolFilter>) -> Self {
        Self::build_filtered_by(tools, filter, |tool| tool.should_defer())
    }

    /// Build an index from enabled tools accepted by the supplied deferred
    /// predicate and optional visibility filter.
    pub fn build_filtered_by<F>(
        tools: &[Arc<dyn Tool>],
        filter: Option<&ToolFilter>,
        is_deferred: F,
    ) -> Self
    where
        F: Fn(&dyn Tool) -> bool,
    {
        let entries = tools
            .iter()
            .filter(|t| t.is_enabled() && is_deferred(t.as_ref()))
            .filter(|t| {
                filter
                    .map(|f| f.allows(t.id().as_str(), t.aliases()))
                    .unwrap_or(true)
            })
            .map(|t| {
                Self::entry_from_parts(
                    t.id().as_str().to_string(),
                    // Use the provider-facing description (rich operational
                    // guidance for tools like Workflow) so deferred tools
                    // discovered via ToolSearch get the same text the eager
                    // tool-snapshot path serves. Falls back to description().
                    t.model_description().to_string(),
                    t.input_schema(),
                    t.search_hint().map(str::to_string),
                )
            })
            .collect();
        Self { entries }
    }

    pub fn with_entries(mut self, entries: impl IntoIterator<Item = ToolSearchEntry>) -> Self {
        self.entries.extend(entries);
        self
    }

    pub fn entry_from_parts(
        name: String,
        description: String,
        input_schema: Value,
        search_hint: Option<String>,
    ) -> ToolSearchEntry {
        let parsed_parts = parse_tool_name(&name);
        let full_lower = name.to_lowercase();
        ToolSearchEntry {
            name,
            description,
            input_schema,
            search_hint,
            parsed_parts,
            full_lower,
        }
    }

    /// Number of deferred tools in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return all deferred tool names, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Whether a tool with this name is in the index (same matching
    /// rules as [`Self::select`]: case-insensitive full name or exact).
    pub fn contains_name(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.entries
            .iter()
            .any(|e| e.full_lower == lower || e.name == name)
    }

    /// Direct selection: look up tools by exact name.
    pub fn select(&self, names: &[&str]) -> (Vec<&ToolSearchEntry>, Vec<String>) {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for &req in names {
            let lower = req.to_lowercase();
            if let Some(entry) = self
                .entries
                .iter()
                .find(|e| e.full_lower == lower || e.name == req)
            {
                found.push(entry);
            } else {
                missing.push(req.to_string());
            }
        }
        (found, missing)
    }

    /// Keyword search: score all entries against the query terms,
    /// return the top `max_results` matches.
    pub fn search(&self, query: &str, max_results: usize) -> Vec<&ToolSearchEntry> {
        let (required, optional) = parse_query(query);
        let all_terms: Vec<&str> = required.iter().chain(optional.iter()).copied().collect();

        if all_terms.is_empty() {
            return Vec::new();
        }

        // Pre-filter: if required terms exist, only keep tools
        // that match ALL required terms somewhere.
        let candidates: Vec<&ToolSearchEntry> = if required.is_empty() {
            self.entries.iter().collect()
        } else {
            self.entries
                .iter()
                .filter(|e| {
                    required.iter().all(|req| {
                        let req_l = req.to_lowercase();
                        e.parsed_parts.iter().any(|p| p.contains(&req_l))
                            || e.full_lower.contains(&req_l)
                            || e.description.to_lowercase().contains(&req_l)
                            || e.search_hint
                                .as_ref()
                                .map(|h| h.to_lowercase().contains(&req_l))
                                .unwrap_or(false)
                    })
                })
                .collect()
        };

        let mut scored: Vec<(&ToolSearchEntry, u32)> = candidates
            .into_iter()
            .map(|entry| {
                let score = score_entry(entry, &all_terms);
                (entry, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.truncate(max_results);
        scored.into_iter().map(|(e, _)| e).collect()
    }
}

// ── Scoring algorithm ────────────────────────────────────────────

fn score_entry(entry: &ToolSearchEntry, terms: &[&str]) -> u32 {
    let is_mcp = entry.name.starts_with("mcp__");
    let mut score: u32 = 0;

    for &term in terms {
        let term_l = term.to_lowercase();

        // 1. Exact part match (highest weight).
        if entry.parsed_parts.iter().any(|p| *p == term_l) {
            score += if is_mcp { 12 } else { 10 };
        }
        // 2. Substring part match.
        else if entry.parsed_parts.iter().any(|p| p.contains(&term_l)) {
            score += if is_mcp { 6 } else { 5 };
        }

        // 3. Full name fallback.
        if entry.full_lower.contains(&term_l) {
            score += 3;
        }

        // 4. `search_hint` match (word boundary).
        if let Some(ref hint) = entry.search_hint {
            if word_boundary_match(&hint.to_lowercase(), &term_l) {
                score += 4;
            }
        }

        // 5. Description match (word boundary).
        if word_boundary_match(&entry.description.to_lowercase(), &term_l) {
            score += 2;
        }
    }
    score
}

/// Check if `term` appears in `haystack` at a word boundary.
fn word_boundary_match(haystack: &str, term: &str) -> bool {
    if let Some(pos) = haystack.find(term) {
        let before_ok = pos == 0 || !haystack.as_bytes()[pos - 1].is_ascii_alphanumeric();
        let after_pos = pos + term.len();
        let after_ok =
            after_pos >= haystack.len() || !haystack.as_bytes()[after_pos].is_ascii_alphanumeric();
        before_ok && after_ok
    } else {
        false
    }
}

// ── Name / query parsing ─────────────────────────────────────────

/// Split a tool name into searchable parts.
///
/// - `mcp__slack__post_message` → `["mcp", "slack", "post", "message"]`
/// - `TeamCreateTool` → `["team", "create", "tool"]`
/// - `Read` → `["read"]`
fn parse_tool_name(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    // Split on `__` (MCP separator) first, then `_`, then
    // CamelCase boundaries.
    for segment in name.split("__") {
        for sub in segment.split('_') {
            parts.extend(split_camel_case(sub));
        }
    }
    parts
        .into_iter()
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split a CamelCase identifier into words.
/// `TeamCreate` → `["Team", "Create"]`
fn split_camel_case(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_uppercase() && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Parse a search query into (required_terms, optional_terms).
/// Terms prefixed with `+` are required.
fn parse_query(query: &str) -> (Vec<&str>, Vec<&str>) {
    let mut required = Vec::new();
    let mut optional = Vec::new();
    for token in query.split_whitespace() {
        if let Some(stripped) = token.strip_prefix('+') {
            if !stripped.is_empty() {
                required.push(stripped);
            }
        } else {
            optional.push(token);
        }
    }
    (required, optional)
}

// ── ToolSearchTool ───────────────────────────────────────────────

/// The ToolSearch tool itself. Never deferred — it's the gateway.
#[derive(Clone, Default)]
pub struct ToolSearchTool;

#[async_trait]
impl Tool for ToolSearchTool {
    fn id(&self) -> ToolId {
        ToolId::new(TOOL_SEARCH_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["ToolSearchTool"]
    }

    fn description(&self) -> &str {
        "Fetches full schema definitions for deferred tools so they can be called.\n\
         \n\
         Deferred tools appear by name in <system-reminder> messages. Until fetched, \
         only the name is known \u{2014} there is no parameter schema, so the tool \
         cannot be invoked safely. This tool takes a query, matches it against the deferred \
         tool list, and returns the matched tools' complete JSONSchema definitions \
         inside a <functions> block.\n\
         \n\
         Result format: each matched tool appears as one \
         <function>{\"description\": \"...\", \"name\": \"...\", \"parameters\": \
         {...}}</function> line inside the <functions> block. Once a tool's schema \
         appears in that result, call the tool directly by name exactly like any \
         provider-visible tool. The stable gateway `InvokeDeferredTool` \
         ({\"tool_name\": \"Name\", \"arguments\": {...}}) is also accepted.\n\
         \n\
         Query forms:\n\
         - \"select:Read,Edit,Grep\" \u{2014} fetch these exact tools by name\n\
         - \"notebook jupyter\" \u{2014} keyword search, up to max_results best matches\n\
         - \"+slack send\" \u{2014} require \"slack\" in the name, rank by remaining terms"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Query to find deferred tools. Use \"select:<tool_name>\" for direct selection, or keywords to search."
                },
                "max_results": {
                    "type": "number",
                    "default": 5,
                    "description": "Maximum number of results to return (default: 5)"
                }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let max_results = input
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        let index = context
            .tool_search_index()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(
                    "ToolSearchTool requires a ToolSearchIndex on the ToolContext"
                ),
            })?;

        // --- select: prefix → direct name lookup ---
        if let Some(names_csv) = query.strip_prefix("select:") {
            let names: Vec<&str> = names_csv
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let (found, missing) = index.select(&names);
            for entry in &found {
                context.record_discovered_deferred_tool(&entry.name);
            }
            return Ok(format_results(&found, &missing, index.len()));
        }

        // --- keyword search ---
        let matches = index.search(&query, max_results);
        let found_refs: Vec<&ToolSearchEntry> = matches;
        for entry in &found_refs {
            context.record_discovered_deferred_tool(&entry.name);
        }
        Ok(format_results(&found_refs, &[], index.len()))
    }
}

/// Format the tool search results as a structured response that
/// includes the full JSON schemas. Matched tools are emitted as
/// `<function>` blocks inside a `<functions>` section; the sections and
/// the surrounding guidance lines are joined with `\n` into the
/// `result` field. Each schema therefore travels as text the model can
/// read, because the OpenAI Responses API has no `tool_reference`
/// content type to carry it structurally.
fn format_results(found: &[&ToolSearchEntry], missing: &[String], total_deferred: usize) -> Value {
    if found.is_empty() && missing.is_empty() {
        return json!({
            "result": "No matching deferred tools found.",
            "total_deferred_tools": total_deferred
        });
    }

    let mut lines = Vec::new();
    lines.push(
        "Matched deferred tools can now be called directly by name, exactly like \
         provider-visible tools."
            .to_string(),
    );
    lines.push("Compatibility gateway call shape (also accepted):".to_string());
    lines.push(
        r#"InvokeDeferredTool({"tool_name":"<matched tool name>","arguments":{...}})"#.to_string(),
    );
    lines.push("<functions>".to_string());
    for entry in found {
        let func = json!({
            "description": entry.description,
            "name": entry.name,
            "parameters": entry.input_schema
        });
        lines.push(format!(
            "<function>{}</function>",
            serde_json::to_string(&func).unwrap_or_default()
        ));
    }
    lines.push("</functions>".to_string());

    let mut result = json!({
        "result": lines.join("\n"),
        "gateway_tool": "InvokeDeferredTool",
        "invocation_hint": "Call the matched tool directly by name with arguments matching its parameters schema. The InvokeDeferredTool gateway ({\"tool_name\": ..., \"arguments\": {...}}) is also accepted.",
        "matched_tools": found.iter().map(|e| &e.name).collect::<Vec<_>>(),
        "total_deferred_tools": total_deferred
    });

    if !missing.is_empty() {
        result["missing_tools"] = json!(missing);
    }

    result
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_name_camel_case() {
        assert_eq!(
            parse_tool_name("TeamCreateTool"),
            vec!["team", "create", "tool"]
        );
    }

    #[test]
    fn parse_tool_name_mcp() {
        assert_eq!(
            parse_tool_name("mcp__slack__post_message"),
            vec!["mcp", "slack", "post", "message"]
        );
    }

    #[test]
    fn parse_tool_name_simple() {
        assert_eq!(parse_tool_name("Read"), vec!["read"]);
    }

    #[test]
    fn parse_query_required_terms() {
        let (req, opt) = parse_query("+slack send message");
        assert_eq!(req, vec!["slack"]);
        assert_eq!(opt, vec!["send", "message"]);
    }

    #[test]
    fn word_boundary_match_basic() {
        assert!(word_boundary_match("wait pause delay", "pause"));
        assert!(!word_boundary_match("applause", "pause"));
    }

    #[test]
    fn select_exact_match() {
        let entries = vec![
            make_entry("Sleep", "Wait for a duration", None),
            make_entry("TeamCreate", "Create a team", None),
        ];
        let index = ToolSearchIndex { entries };
        let (found, missing) = index.select(&["Sleep", "Unknown"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Sleep");
        assert_eq!(missing, vec!["Unknown"]);
    }

    #[test]
    fn keyword_search_scores() {
        let entries = vec![
            make_entry(
                "Sleep",
                "Wait for a specified duration",
                Some("wait pause delay timer"),
            ),
            make_entry(
                "TeamCreate",
                "Create a multi-agent team",
                Some("parallel collaboration"),
            ),
        ];
        let index = ToolSearchIndex { entries };
        let results = index.search("wait timer", 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Sleep");
    }

    #[test]
    fn keyword_search_with_required() {
        let entries = vec![
            make_entry("Sleep", "Wait for a specified duration", Some("wait pause")),
            make_entry("TeamCreate", "Create a team", Some("parallel")),
            make_entry("TeamDelete", "Delete a team", None),
        ];
        let index = ToolSearchIndex { entries };
        let results = index.search("+team create", 5);
        assert_eq!(results.len(), 2); // TeamCreate and TeamDelete match "+team"
        assert_eq!(results[0].name, "TeamCreate"); // higher score due to "create"
    }

    #[test]
    fn build_filtered_hides_denied_deferred_tool() {
        struct DeferredTestTool(&'static str);

        #[async_trait]
        impl Tool for DeferredTestTool {
            fn id(&self) -> ToolId {
                ToolId::new(self.0)
            }

            fn description(&self) -> &str {
                "Deferred test tool"
            }

            fn input_schema(&self) -> ToolInputSchema {
                json!({"type": "object"})
            }

            fn should_defer(&self) -> bool {
                true
            }

            async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
                Ok(input)
            }
        }

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(DeferredTestTool("Read")),
            Arc::new(DeferredTestTool("Write")),
        ];
        let filter = ToolFilter::allow_only(["Read"]);

        let index = ToolSearchIndex::build_filtered(&tools, Some(&filter));

        assert_eq!(index.names(), vec!["Read"]);
        let (found, missing) = index.select(&["Write"]);
        assert!(found.is_empty());
        assert_eq!(missing, vec!["Write"]);
    }

    #[test]
    fn build_filtered_by_can_index_predicate_deferred_tool_without_changing_legacy_behavior() {
        struct NonDeferredTestTool(&'static str);

        #[async_trait]
        impl Tool for NonDeferredTestTool {
            fn id(&self) -> ToolId {
                ToolId::new(self.0)
            }

            fn description(&self) -> &str {
                "Non-deferred test tool"
            }

            fn input_schema(&self) -> ToolInputSchema {
                json!({"type": "object"})
            }

            async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
                Ok(input)
            }
        }

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(NonDeferredTestTool("PolicyDeferred"))];

        let legacy = ToolSearchIndex::build_filtered(&tools, None);
        assert!(legacy.names().is_empty());

        let policy = ToolSearchIndex::build_filtered_by(&tools, None, |tool| {
            tool.id().as_str() == "PolicyDeferred"
        });
        assert_eq!(policy.names(), vec!["PolicyDeferred"]);
    }

    #[test]
    fn format_results_includes_gateway_invocation_hint_and_schema() {
        let entry = make_entry("DeferredEcho", "Echo deferred input", None);
        let value = format_results(&[&entry], &[], 1);
        let result = value.get("result").and_then(Value::as_str).unwrap();

        assert_eq!(
            value.get("gateway_tool").and_then(Value::as_str),
            Some("InvokeDeferredTool")
        );
        assert!(result.contains("called directly by name"));
        assert!(result.contains("InvokeDeferredTool"));
        assert!(result.contains(r#""tool_name":"<matched tool name>""#));
        assert!(result.contains("DeferredEcho"));
        assert!(result.contains("parameters"));
        assert!(result.contains(r#""properties":{}"#));
        let hint = value
            .get("invocation_hint")
            .and_then(Value::as_str)
            .unwrap();
        assert!(hint.contains("directly by name"));
        assert!(hint.contains("InvokeDeferredTool"));
        assert!(hint.contains("arguments"));
    }

    #[test]
    fn select_agent_result_contains_gateway_guidance_and_schema_contract() {
        let entry = ToolSearchEntry {
            name: "Agent".to_string(),
            description: "Launch a specialized agent".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
            search_hint: None,
            parsed_parts: parse_tool_name("Agent"),
            full_lower: "agent".to_string(),
        };
        let index = ToolSearchIndex {
            entries: vec![entry],
        };
        let (found, missing) = index.select(&["Agent"]);
        let value = format_results(&found, &missing, index.len());
        let result = value.get("result").and_then(Value::as_str).unwrap();

        assert!(result.contains("<function>"));
        assert!(result.contains(r#""name":"Agent""#));
        assert!(result.contains(r#""parameters""#));
        assert!(result.contains(r#""prompt""#));
        assert!(result.contains(r#""required":["prompt"]"#));
        assert!(result.contains("called directly by name"));
        assert!(result.contains("InvokeDeferredTool"));
        assert!(result.contains(r#""tool_name":"<matched tool name>""#));
        assert_eq!(
            value.get("gateway_tool").and_then(Value::as_str),
            Some("InvokeDeferredTool")
        );
    }

    fn make_entry(name: &str, description: &str, search_hint: Option<&str>) -> ToolSearchEntry {
        ToolSearchEntry {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            search_hint: search_hint.map(str::to_string),
            parsed_parts: parse_tool_name(name),
            full_lower: name.to_lowercase(),
        }
    }
}
