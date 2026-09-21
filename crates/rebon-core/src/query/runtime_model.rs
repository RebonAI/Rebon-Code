use super::*;

pub type SystemPromptSnapshot = Arc<RwLock<Option<String>>>;

#[derive(Clone)]
pub struct RuntimeModelConfig {
    pub provider_name: String,
    pub client: Arc<dyn ModelClient>,
    pub model: String,
    pub model_profiles: rebon_types::ModelProfileMap,
    pub title_model: String,
    pub model_marketing_name: Option<String>,
    pub knowledge_cutoff: Option<String>,
    pub prune_level: Option<PruneLevelHandle>,
    pub compact_provider: Option<Arc<dyn CompactProvider>>,
    pub compact_fallback_provider: Option<Arc<dyn CompactProvider>>,
    pub context_management: Option<rebon_api::ContextManagementConfig>,
    /// Reasoning mode for OpenAI Responses (gpt-5.6+ pro mode),
    /// resolved from the provider config's `reasoningMode`.
    pub reasoning_mode: Option<rebon_api::ReasoningMode>,
}

#[derive(Clone)]
pub struct SharedRuntimeModel {
    pub(super) routing: Arc<super::model_routing::SessionModelRouting>,
    inner: Arc<RwLock<RuntimeModelConfig>>,
    /// Session handle for whatever client `inner` currently holds.
    ///
    /// Switching provider or model (`/model`) replaces the client, and
    /// the new client's session state has nothing to do with the old
    /// one's — so the handle is rebuilt. Turns that keep the same
    /// client keep the same handle, which is what makes a session a
    /// session rather than a fresh one per turn.
    session: Arc<Mutex<Option<Arc<SessionHandle>>>>,
}

impl SharedRuntimeModel {
    pub fn new(config: RuntimeModelConfig) -> Self {
        Self {
            routing: Arc::new(super::model_routing::SessionModelRouting::default()),
            inner: Arc::new(RwLock::new(config)),
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// Share live provider/model configuration while starting an independent
    /// continuation session. Session runtimes use this so `/model` remains a
    /// process-wide hot swap, but `/new` cannot reset a detached old turn's
    /// response chain.
    pub fn fork_session(&self) -> Self {
        Self {
            routing: self.routing.clone(),
            inner: self.inner.clone(),
            session: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get(&self) -> RuntimeModelConfig {
        self.inner
            .read()
            .expect("runtime model config lock poisoned")
            .clone()
    }

    pub fn set_runtime_resolver(&self, resolver: crate::model_routing::ModelRuntimeResolver) {
        *self
            .routing
            .resolver
            .write()
            .expect("model runtime resolver lock poisoned") = Some(resolver);
    }

    pub fn set(&self, config: RuntimeModelConfig) {
        *self
            .inner
            .write()
            .expect("runtime model config lock poisoned") = config;
    }

    /// The live session handle for the current client.
    pub fn session(&self) -> Arc<SessionHandle> {
        let client = self
            .inner
            .read()
            .expect("runtime model config lock poisoned")
            .client
            .clone();
        self.session_for(&client)
    }

    /// Read the config and its session handle under one lock
    /// acquisition, so a concurrent [`Self::set`] cannot hand a caller
    /// one provider's config next to another provider's session.
    pub fn snapshot(&self) -> (RuntimeModelConfig, Arc<SessionHandle>) {
        let config = self
            .inner
            .read()
            .expect("runtime model config lock poisoned")
            .clone();
        let session = self.session_for(&config.client);
        (config, session)
    }

    fn session_for(&self, client: &Arc<dyn ModelClient>) -> Arc<SessionHandle> {
        let mut cached = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = cached.as_ref() {
            if Arc::ptr_eq(existing.client(), client) {
                return existing.clone();
            }
        }
        let handle = SessionHandle::new(client.clone());
        *cached = Some(handle.clone());
        handle
    }
}

#[derive(Clone)]
pub(super) struct CodexWebSearchDelegate {
    session: Arc<SessionHandle>,
    model: String,
}

impl CodexWebSearchDelegate {
    pub(super) fn new(session: Arc<SessionHandle>, model: String) -> Self {
        Self { session, model }
    }
}

#[async_trait]
impl WebSearchDelegate for CodexWebSearchDelegate {
    async fn web_search(&self, input: Value) -> ToolResult<Value> {
        if !self.session.client().supports_codex_oauth_web_search() {
            return Err(ToolError::Execution {
                tool: ToolId::new(WEB_SEARCH_TOOL_NAME),
                source: anyhow::anyhow!(
                    "WebSearch is only available for ChatGPT Codex OAuth providers"
                ),
            });
        }

        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| ToolError::InvalidInput {
                tool: ToolId::new(WEB_SEARCH_TOOL_NAME),
                reason: "query must not be empty".into(),
                error_code: Some(400),
            })?
            .to_string();
        let web_search = web_search_config_from_tool_input(&input)?;
        // A search runs as its own short-lived sub-session so its
        // request never joins the parent's continuation chain.
        let search_session = self.session.fork_for_sub_agent(None);
        let request = CreateMessageRequest {
            model: self.model.clone(),
            messages: vec![ApiMessage::user_text(format!(
                "Search the web for the following query and answer concisely with source URLs when available.\n\n{query}"
            ))],
            system: Some(
                "You are a web search tool. Use the native web_search tool for the user's query. Return the answer only, with source URLs when available."
                    .to_string(),
            ),
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: Some(web_search),
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };
        let message = search_session
            .client()
            .create_message(request)
            .await
            .map_err(|err| ToolError::Execution {
                tool: ToolId::new(WEB_SEARCH_TOOL_NAME),
                source: anyhow::anyhow!(err),
            })?;
        Ok(format_web_search_tool_output(&query, message))
    }
}

pub(super) fn web_search_config_from_tool_input(
    input: &Value,
) -> ToolResult<rebon_api::WebSearchToolConfig> {
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
    let search_context_size = input
        .get("search_context_size")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|size| !size.is_empty())
        .map(|size| match size {
            "low" | "medium" | "high" => Ok(size.to_string()),
            other => Err(ToolError::InvalidInput {
                tool: ToolId::new(WEB_SEARCH_TOOL_NAME),
                reason: format!("invalid search_context_size `{other}`"),
                error_code: Some(400),
            }),
        })
        .transpose()?;
    let user_location = input
        .get("user_location")
        .and_then(Value::as_object)
        .map(|loc| rebon_api::WebSearchUserLocation {
            country: loc
                .get("country")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            region: loc
                .get("region")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            city: loc
                .get("city")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            timezone: loc
                .get("timezone")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        });

    Ok(rebon_api::WebSearchToolConfig {
        allowed_domains,
        blocked_domains: None,
        max_uses: None,
        search_context_size,
        user_location,
    })
}

pub(super) fn format_web_search_tool_output(query: &str, message: AssistantMessage) -> Value {
    let mut server_tool_uses = Vec::new();
    let mut results = Vec::new();
    for block in &message.content {
        match block {
            ApiContentBlock::ServerToolUse(tool_use)
                if tool_use.name == "web_search" || tool_use.name == "web_search_20250305" =>
            {
                server_tool_uses.push(serde_json::json!({
                    "id": tool_use.id,
                    "name": tool_use.name,
                    "input": tool_use.input,
                }));
            }
            ApiContentBlock::WebSearchResult(result) => {
                for entry in &result.results {
                    results.push(serde_json::json!({
                        "title": entry.title,
                        "url": entry.url,
                        "snippet": entry.snippet,
                    }));
                }
            }
            _ => {}
        }
    }

    serde_json::json!({
        "query": query,
        "answer": message.text(),
        "serverToolUses": server_tool_uses,
        "results": results,
        "usage": message.usage,
        "content": message.content,
    })
}
