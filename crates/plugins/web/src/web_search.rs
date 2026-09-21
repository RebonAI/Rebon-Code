use std::sync::Arc;

use async_trait::async_trait;
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome};
use serde_json::{json, Value};

use crate::web_search_local::{run_local_web_search, LocalSearchOutcome};
use rebon_tool::{Tool, ToolContext};

pub use rebon_tool::web::WEB_SEARCH_TOOL_NAME;

#[derive(Clone, Default)]
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn id(&self) -> ToolId {
        ToolId::new(WEB_SEARCH_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["WebSearchTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Web
    }

    fn description(&self) -> &str {
        "Search the web for current information. Uses the provider's native server-side web_search when the active route supports it (ChatGPT Codex OAuth); every other provider falls back to Rebon's built-in local search (Brave Search API when BRAVE_API_KEY is set, otherwise DuckDuckGo/Bing). Use it for recent, time-sensitive, or web-only information."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The web search query to run."
                },
                "allowed_domains": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional domain allowlist for search results, e.g. [\"docs.rs\", \"github.com\"]."
                },
                "search_context_size": {
                    "type": "string",
                    "enum": ["low", "medium", "high"],
                    "description": "Optional amount of search context to retrieve. Defaults to the provider setting."
                },
                "user_location": {
                    "type": "object",
                    "description": "Optional approximate location for localized results. Only honored by provider-native search.",
                    "properties": {
                        "country": { "type": "string" },
                        "region": { "type": "string" },
                        "city": { "type": "string" },
                        "timezone": { "type": "string" }
                    },
                    "additionalProperties": false
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("internet current events recent sources citations lookup")
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if query.trim().is_empty() {
            return Ok(ValidationOutcome::invalid("query must not be empty", 400));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        // Kernel web seat: a configured plugin provider handles the
        // whole call; its failures are LOUD (arbitration semantics — a
        // configured-but-broken provider must never silently degrade).
        // None = no plugin route, the builtin path below runs unchanged.
        if let Some(router) = context.web_provider_router() {
            if let Some(output) = router.search(&input).await? {
                return Ok(output);
            }
        }
        if let Some(delegate) = context.web_search_delegate() {
            match Arc::clone(delegate).web_search(input.clone()).await {
                Ok(output) => return Ok(output),
                Err(err @ ToolError::InvalidInput { .. }) => return Err(err),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "provider-native web search failed; falling back to local search"
                    );
                }
            }
        }
        self.local_search(&input).await
    }
}

impl WebSearchTool {
    async fn local_search(&self, input: &Value) -> ToolResult<Value> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| ToolError::InvalidInput {
                tool: self.id(),
                reason: "query must not be empty".into(),
                error_code: Some(400),
            })?;
        let allowed_domains = input
            .get("allowed_domains")
            .and_then(Value::as_array)
            .map(|domains| {
                domains
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|domain| !domain.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|domains| !domains.is_empty());
        let max_results = match input.get("search_context_size").and_then(Value::as_str) {
            None | Some("medium") => 10,
            Some("low") => 5,
            Some("high") => 20,
            Some(other) => {
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!("invalid search_context_size `{other}`"),
                    error_code: Some(400),
                })
            }
        };

        let outcome = run_local_web_search(query, max_results, allowed_domains.as_deref())
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err,
            })?;
        Ok(local_web_search_output(query, &outcome))
    }
}

/// Mirrors the provider-native output shape (`query` / `answer` /
/// `results`) so transcript renderers work unchanged; `answer` carries
/// a plain-text digest because the local path has no model-synthesized
/// summary.
fn local_web_search_output(query: &str, outcome: &LocalSearchOutcome) -> Value {
    let results: Vec<Value> = outcome
        .results
        .iter()
        .map(|result| {
            json!({
                "title": result.title,
                "url": result.url,
                "snippet": result.snippet,
            })
        })
        .collect();
    json!({
        "query": query,
        "answer": format_local_answer(query, outcome),
        "results": results,
        "source": "local",
        "engine": outcome.engine.label(),
    })
}

fn format_local_answer(query: &str, outcome: &LocalSearchOutcome) -> String {
    if outcome.results.is_empty() {
        return format!(
            "No web results found for \"{query}\" (searched via {}).",
            outcome.engine.label()
        );
    }
    let mut answer = format!(
        "Top web results for \"{query}\" (via {}):\n",
        outcome.engine.label()
    );
    for (index, result) in outcome.results.iter().enumerate() {
        answer.push_str(&format!(
            "\n{}. {}\n   {}",
            index + 1,
            result.title,
            result.url
        ));
        if !result.snippet.is_empty() {
            answer.push_str(&format!("\n   {}", result.snippet));
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_search_local::{LocalSearchEngine, LocalSearchResult};

    fn outcome_with_results() -> LocalSearchOutcome {
        LocalSearchOutcome {
            engine: LocalSearchEngine::DuckDuckGo,
            results: vec![
                LocalSearchResult {
                    title: "Rust Programming Language".to_string(),
                    url: "https://www.rust-lang.org/".to_string(),
                    snippet: "A language empowering everyone.".to_string(),
                },
                LocalSearchResult {
                    title: "tokio".to_string(),
                    url: "https://docs.rs/tokio".to_string(),
                    snippet: String::new(),
                },
            ],
        }
    }

    #[test]
    fn local_output_keeps_provider_native_shape() {
        let output = local_web_search_output("rust async", &outcome_with_results());
        assert_eq!(output["query"], "rust async");
        assert_eq!(output["source"], "local");
        assert_eq!(output["engine"], "duckduckgo");
        assert_eq!(output["results"].as_array().unwrap().len(), 2);
        assert_eq!(output["results"][0]["url"], "https://www.rust-lang.org/");
        let answer = output["answer"].as_str().unwrap();
        assert!(answer.contains("1. Rust Programming Language"));
        assert!(answer.contains("https://docs.rs/tokio"));
    }

    #[test]
    fn empty_outcome_reports_no_results_in_answer() {
        let outcome = LocalSearchOutcome {
            engine: LocalSearchEngine::Bing,
            results: Vec::new(),
        };
        let output = local_web_search_output("obscure query", &outcome);
        assert_eq!(output["results"].as_array().unwrap().len(), 0);
        assert!(output["answer"]
            .as_str()
            .unwrap()
            .contains("No web results found"));
    }
}
