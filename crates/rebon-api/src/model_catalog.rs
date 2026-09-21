//! Ask an endpoint which models it serves.
//!
//! Every vendor lists models somewhere, but not in the same place or
//! shape: OpenAI's `GET /models` (adopted by DeepSeek, Kimi, MiniMax,
//! SiliconFlow, Gemini's compatibility surface, OpenCode Zen and — though
//! its docs never say so — Zhipu); Anthropic's paginated `GET /v1/models`,
//! which is the only one that states each model's limits; Ollama's native
//! `/api/tags` plus a `/api/show` per model for the context length;
//! DashScope's native `/api/v1/models`, which also carries limits. Volcengine
//! Ark has none an API key can reach. [`ProviderVendor::model_listing`]
//! picks the surface; this module speaks it.
//!
//! The result is already useful to a settings page: non-chat entries
//! (embeddings, speech, images, moderation) are dropped, a reseller's list
//! is narrowed to the wire the entry is configured for, and each model is
//! annotated with the limits the vendor documents where the list itself
//! carries none.

use std::time::Duration;

use serde_json::Value;

use crate::vendor::{KnownModel, ModelListing, ProviderVendor, WireFamily};

/// What to list.
#[derive(Debug, Clone)]
pub struct ModelDiscoveryRequest {
    pub vendor: ProviderVendor,
    pub wire: WireFamily,
    pub base_url: String,
    /// Already resolved: a literal key, not a `$VAR` reference.
    pub api_key: String,
    /// Extra headers the provider entry sends on every request.
    pub extra_headers: Vec<(String, String)>,
}

/// One model an endpoint serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    /// Where the limits came from: the list itself, the vendor's
    /// documented catalogue, or nowhere.
    pub limits_source: LimitsSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsSource {
    /// The list endpoint stated them.
    Endpoint,
    /// The vendor's documentation (see [`ProviderVendor::known_models`]).
    Catalogue,
    /// Unknown; the runtime falls back to name-based inference.
    None,
}

/// A successful listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDiscovery {
    pub models: Vec<DiscoveredModel>,
    /// The URL that was read, for the status line.
    pub endpoint: String,
    /// Entries dropped because they are not chat models.
    pub skipped_non_chat: usize,
    /// Entries dropped because this entry's wire cannot drive them
    /// (OpenCode Zen only).
    pub skipped_other_wire: usize,
    /// Whether the vendor documents the endpoint that was read.
    pub documented: bool,
}

/// Why a listing did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelDiscoveryError {
    /// The vendor has no list endpoint an API key can reach.
    #[error("{vendor} does not offer a model list endpoint; {hint}")]
    Unsupported {
        vendor: &'static str,
        hint: &'static str,
    },
    /// The request could not be made or did not come back.
    #[error("request to {url} failed: {message}")]
    Http { url: String, message: String },
    /// The endpoint answered with a non-success status.
    #[error("{url} answered {status}: {body}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },
    /// The body was not the shape the vendor documents.
    #[error("{url} returned something that is not a model list: {message}")]
    Parse { url: String, message: String },
}

const ANTHROPIC_VERSION: &str = "2023-06-01";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Ollama's `/api/show` is one request per model; a machine with a very
/// large library still gets its list, just without limits past this many.
const OLLAMA_SHOW_LIMIT: usize = 64;
const DASHSCOPE_PAGE_SIZE: u32 = 100;
const MAX_PAGES: u32 = 20;

/// List the models behind `request`, filtered to what this entry can run
/// and annotated with documented limits.
pub async fn discover_models(
    http: &reqwest::Client,
    request: &ModelDiscoveryRequest,
) -> Result<ModelDiscovery, ModelDiscoveryError> {
    let base_url = request.base_url.trim();
    if base_url.is_empty() {
        return Err(ModelDiscoveryError::Http {
            url: String::new(),
            message: "base URL is empty".into(),
        });
    }
    let (raw, endpoint, documented) = match request.vendor.model_listing(request.wire) {
        ModelListing::OpenAiCompatible { query, documented } => {
            let url = openai_models_url(base_url, query);
            (
                fetch_openai_list(http, request, &url).await?,
                url,
                documented,
            )
        }
        ModelListing::Anthropic => {
            let url = anthropic_models_url(base_url);
            (fetch_anthropic_list(http, request, &url).await?, url, true)
        }
        ModelListing::Ollama => {
            let root = ollama_root(base_url);
            let tags_url = format!("{root}/api/tags");
            match fetch_ollama_tags(http, request, &root).await {
                Ok(models) => (models, tags_url, true),
                // A gateway that mimics Ollama's port but not its native
                // API still has the OpenAI list.
                Err(ModelDiscoveryError::Status { .. })
                | Err(ModelDiscoveryError::Parse { .. }) => {
                    let url = openai_models_url(base_url, "");
                    (fetch_openai_list(http, request, &url).await?, url, true)
                }
                Err(other) => return Err(other),
            }
        }
        ModelListing::DashScope => {
            let url = dashscope_models_url(base_url);
            (fetch_dashscope_list(http, request, &url).await?, url, true)
        }
        ModelListing::Unsupported => {
            return Err(ModelDiscoveryError::Unsupported {
                vendor: request.vendor.display_name(),
                hint: "the documented catalogue is filled in instead",
            });
        }
    };
    Ok(finish(request, raw, endpoint, documented))
}

/// [`discover_models`] on a fresh single-threaded runtime, for callers
/// without one (the desktop settings window's background executor, the
/// TUI's command handler).
pub fn discover_models_blocking(
    request: &ModelDiscoveryRequest,
) -> Result<ModelDiscovery, ModelDiscoveryError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| ModelDiscoveryError::Http {
            url: request.base_url.clone(),
            message: format!("build runtime: {err}"),
        })?;
    runtime.block_on(async {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| ModelDiscoveryError::Http {
                url: request.base_url.clone(),
                message: format!("build HTTP client: {err}"),
            })?;
        discover_models(&http, request).await
    })
}

/// The vendor's documented catalogue in the shape a listing would have
/// produced — what a settings page falls back to when the endpoint has no
/// list (Volcengine) or the list could not be read.
pub fn catalogue_as_discovery(vendor: ProviderVendor, wire: WireFamily) -> ModelDiscovery {
    let raw = vendor
        .known_models()
        .iter()
        .map(|known| DiscoveredModel {
            id: known.id.to_string(),
            display_name: None,
            context_window: Some(known.context_window),
            max_output_tokens: known.max_output_tokens,
            limits_source: LimitsSource::Catalogue,
        })
        .collect();
    let request = ModelDiscoveryRequest {
        vendor,
        wire,
        base_url: String::new(),
        api_key: String::new(),
        extra_headers: Vec::new(),
    };
    finish(&request, raw, "documented catalogue".into(), true)
}

fn finish(
    request: &ModelDiscoveryRequest,
    raw: Vec<DiscoveredModel>,
    endpoint: String,
    documented: bool,
) -> ModelDiscovery {
    let mut skipped_non_chat = 0;
    let mut skipped_other_wire = 0;
    let mut models: Vec<DiscoveredModel> = Vec::with_capacity(raw.len());
    for mut model in raw {
        if !request.vendor.is_chat_model_id(&model.id) {
            skipped_non_chat += 1;
            continue;
        }
        if !request.vendor.model_speaks_wire(&model.id, request.wire) {
            skipped_other_wire += 1;
            continue;
        }
        if models.iter().any(|seen| seen.id == model.id) {
            continue;
        }
        if model.context_window.is_none() {
            if let Some(known) = request.vendor.known_model(&model.id) {
                model.context_window = Some(known.context_window);
                if model.max_output_tokens.is_none() {
                    model.max_output_tokens = known.max_output_tokens;
                }
                model.limits_source = LimitsSource::Catalogue;
            }
        }
        models.push(model);
    }
    sort_for_display(request.vendor, &mut models);
    ModelDiscovery {
        models,
        endpoint,
        skipped_non_chat,
        skipped_other_wire,
        documented,
    }
}

/// Documented models first, in the order the vendor's page lists them
/// (flagship first), then everything else alphabetically. A list of sixty
/// ids is only useful when the ones worth picking are at the top.
fn sort_for_display(vendor: ProviderVendor, models: &mut [DiscoveredModel]) {
    let catalogue: Vec<KnownModel> = match vendor {
        ProviderVendor::OpenCode | ProviderVendor::Unknown => ProviderVendor::ALL
            .iter()
            .flat_map(|v| v.known_models().iter().copied())
            .collect(),
        other => other.known_models().to_vec(),
    };
    let rank = |id: &str| -> (usize, String) {
        let lower = id.to_ascii_lowercase();
        let bare = lower.split('@').next().unwrap_or(&lower).to_string();
        let position = catalogue
            .iter()
            .position(|known| known.id.eq_ignore_ascii_case(&bare))
            .unwrap_or(catalogue.len());
        (position, lower)
    };
    models.sort_by_cached_key(|model| rank(&model.id));
}

// ── URL shapes ─────────────────────────────────────────────────

/// `GET /models` next to the chat endpoint. A base that already names a
/// version segment (`/v1`, `/api/v3`, `/api/paas/v4`, `/v1beta/openai`)
/// gets `/models` appended; a bare host gets `/v1/models`; a pasted
/// endpoint path (`/chat/completions`, `/responses`) is stripped first.
pub fn openai_models_url(base_url: &str, query: &str) -> String {
    let mut base = base_url.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/responses", "/completions"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.to_string();
            break;
        }
    }
    let has_path = base
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&base)
        .contains('/');
    let mut url = if has_path {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }
    url
}

/// `GET /v1/models`, placed the way `messages_endpoint_for_base` places
/// `/v1/messages`.
pub fn anthropic_models_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if let Some(root) = base.strip_suffix("/v1/messages") {
        return format!("{root}/v1/models");
    }
    if base.ends_with("/v1") {
        return format!("{base}/models");
    }
    format!("{base}/v1/models")
}

/// The server root of an Ollama base URL (`http://host:11434/v1` →
/// `http://host:11434`).
pub fn ollama_root(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    base.strip_suffix("/v1")
        .or_else(|| base.strip_suffix("/api"))
        .unwrap_or(base)
        .to_string()
}

/// DashScope's native list lives on the same host as compatible-mode:
/// `https://dashscope.aliyuncs.com/compatible-mode/v1` →
/// `https://dashscope.aliyuncs.com/api/v1/models`.
pub fn dashscope_models_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let lower = base.to_ascii_lowercase();
    let root = if let Some(at) = lower.find("/compatible-mode") {
        &base[..at]
    } else if let Some(at) = lower.find("/apps/anthropic") {
        &base[..at]
    } else if let Some(at) = lower.find("/api/v1") {
        &base[..at]
    } else {
        base
    };
    format!("{root}/api/v1/models")
}

// ── Fetchers ───────────────────────────────────────────────────

fn apply_extra_headers(
    mut builder: reqwest::RequestBuilder,
    request: &ModelDiscoveryRequest,
) -> reqwest::RequestBuilder {
    for (name, value) in &request.extra_headers {
        builder = builder.header(name, value);
    }
    builder
}

async fn get_json(
    builder: reqwest::RequestBuilder,
    url: &str,
) -> Result<Value, ModelDiscoveryError> {
    let response = builder
        .send()
        .await
        .map_err(|err| ModelDiscoveryError::Http {
            url: url.to_string(),
            message: err.to_string(),
        })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| ModelDiscoveryError::Http {
            url: url.to_string(),
            message: format!("read body: {err}"),
        })?;
    if !status.is_success() {
        return Err(ModelDiscoveryError::Status {
            url: url.to_string(),
            status: status.as_u16(),
            body: truncate_body(&body),
        });
    }
    serde_json::from_str(&body).map_err(|err| ModelDiscoveryError::Parse {
        url: url.to_string(),
        message: format!("{err} (body starts {:?})", truncate_body(&body)),
    })
}

fn truncate_body(body: &str) -> String {
    const LIMIT: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(LIMIT).collect();
    out.push('…');
    out
}

async fn fetch_openai_list(
    http: &reqwest::Client,
    request: &ModelDiscoveryRequest,
    url: &str,
) -> Result<Vec<DiscoveredModel>, ModelDiscoveryError> {
    let mut builder = http.get(url).header("Accept", "application/json");
    if !request.api_key.trim().is_empty() {
        builder = builder.header(
            "Authorization",
            format!("Bearer {}", request.api_key.trim()),
        );
    }
    let builder = apply_extra_headers(builder, request);
    let body = get_json(builder, url).await?;
    parse_openai_models_list(&body).ok_or_else(|| ModelDiscoveryError::Parse {
        url: url.to_string(),
        message: "no `data` array of models".into(),
    })
}

async fn fetch_anthropic_list(
    http: &reqwest::Client,
    request: &ModelDiscoveryRequest,
    url: &str,
) -> Result<Vec<DiscoveredModel>, ModelDiscoveryError> {
    let mut models = Vec::new();
    let mut after_id: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut page_url = format!("{url}?limit=1000");
        if let Some(after) = &after_id {
            page_url.push_str("&after_id=");
            page_url.push_str(after);
        }
        let builder = http
            .get(&page_url)
            .header("x-api-key", request.api_key.trim())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Accept", "application/json");
        let builder = apply_extra_headers(builder, request);
        let body = get_json(builder, &page_url).await?;
        let (page, next) =
            parse_anthropic_models_page(&body).ok_or_else(|| ModelDiscoveryError::Parse {
                url: page_url.clone(),
                message: "no `data` array of models".into(),
            })?;
        models.extend(page);
        match next {
            Some(next) if after_id.as_deref() != Some(next.as_str()) => after_id = Some(next),
            _ => break,
        }
    }
    Ok(models)
}

async fn fetch_ollama_tags(
    http: &reqwest::Client,
    request: &ModelDiscoveryRequest,
    root: &str,
) -> Result<Vec<DiscoveredModel>, ModelDiscoveryError> {
    let tags_url = format!("{root}/api/tags");
    let builder = apply_extra_headers(http.get(&tags_url), request);
    let body = get_json(builder, &tags_url).await?;
    let mut models = parse_ollama_tags(&body).ok_or_else(|| ModelDiscoveryError::Parse {
        url: tags_url.clone(),
        message: "no `models` array".into(),
    })?;
    let show_url = format!("{root}/api/show");
    for model in models.iter_mut().take(OLLAMA_SHOW_LIMIT) {
        let builder = apply_extra_headers(
            http.post(&show_url)
                .json(&serde_json::json!({ "model": model.id })),
            request,
        );
        // A missing context length is not a failed listing.
        if let Ok(info) = get_json(builder, &show_url).await {
            if let Some(window) = parse_ollama_show_context_length(&info) {
                model.context_window = Some(window);
                model.limits_source = LimitsSource::Endpoint;
            }
        }
    }
    Ok(models)
}

async fn fetch_dashscope_list(
    http: &reqwest::Client,
    request: &ModelDiscoveryRequest,
    url: &str,
) -> Result<Vec<DiscoveredModel>, ModelDiscoveryError> {
    let mut models = Vec::new();
    for page_no in 1..=MAX_PAGES {
        let page_url =
            format!("{url}?page_no={page_no}&page_size={DASHSCOPE_PAGE_SIZE}&supports=inference");
        let builder = http
            .get(&page_url)
            .header(
                "Authorization",
                format!("Bearer {}", request.api_key.trim()),
            )
            .header("Accept", "application/json");
        let builder = apply_extra_headers(builder, request);
        let body = get_json(builder, &page_url).await?;
        let (page, total) =
            parse_dashscope_models_page(&body).ok_or_else(|| ModelDiscoveryError::Parse {
                url: page_url.clone(),
                message: "no `output.models` array".into(),
            })?;
        let page_len = page.len();
        models.extend(page);
        let fetched = page_no * DASHSCOPE_PAGE_SIZE;
        if page_len == 0 || total.is_some_and(|total| fetched >= total) {
            break;
        }
    }
    Ok(models)
}

// ── Parsers (pure) ─────────────────────────────────────────────

/// OpenAI's `{"object":"list","data":[{"id":...}]}`. Kimi adds
/// `context_length`; OpenRouter-style lists carry `context_length` or a
/// `context_window`, and some put `max_output_tokens` / `max_completion_tokens`
/// beside it — read them when present. A Gemini-style `models/` prefix is
/// stripped so the id matches what the chat endpoint takes.
pub fn parse_openai_models_list(body: &Value) -> Option<Vec<DiscoveredModel>> {
    let data = body.get("data")?.as_array()?;
    let mut out = Vec::with_capacity(data.len());
    for entry in data {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.strip_prefix("models/").unwrap_or(id).trim();
        if id.is_empty() {
            continue;
        }
        let context_window = first_u32(
            entry,
            &[
                "context_length",
                "context_window",
                "max_context_length",
                "max_input_tokens",
            ],
        );
        let max_output_tokens = first_u32(
            entry,
            &["max_output_tokens", "max_completion_tokens", "max_tokens"],
        );
        let display_name = entry
            .get("display_name")
            .or_else(|| entry.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && *name != id)
            .map(str::to_string);
        out.push(DiscoveredModel {
            id: id.to_string(),
            display_name,
            limits_source: if context_window.is_some() {
                LimitsSource::Endpoint
            } else {
                LimitsSource::None
            },
            context_window,
            max_output_tokens,
        });
    }
    Some(out)
}

/// One page of Anthropic's `GET /v1/models`: the models and, when
/// `has_more`, the `last_id` to continue from.
pub fn parse_anthropic_models_page(body: &Value) -> Option<(Vec<DiscoveredModel>, Option<String>)> {
    let data = body.get("data")?.as_array()?;
    let mut out = Vec::with_capacity(data.len());
    for entry in data {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let context_window = first_u32(entry, &["max_input_tokens", "context_window"]);
        let max_output_tokens = first_u32(entry, &["max_tokens", "max_output_tokens"]);
        out.push(DiscoveredModel {
            id: id.trim().to_string(),
            display_name: entry
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            limits_source: if context_window.is_some() {
                LimitsSource::Endpoint
            } else {
                LimitsSource::None
            },
            context_window,
            max_output_tokens,
        });
    }
    let has_more = body
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next = has_more
        .then(|| {
            body.get("last_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten();
    Some((out, next))
}

/// Ollama's `GET /api/tags`: `{"models":[{"name":"llama3.3:70b", ...}]}`.
pub fn parse_ollama_tags(body: &Value) -> Option<Vec<DiscoveredModel>> {
    let models = body.get("models")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|entry| {
                let name = entry
                    .get("name")
                    .or_else(|| entry.get("model"))
                    .and_then(Value::as_str)?
                    .trim();
                if name.is_empty() {
                    return None;
                }
                Some(DiscoveredModel {
                    id: name.to_string(),
                    display_name: None,
                    context_window: None,
                    max_output_tokens: None,
                    limits_source: LimitsSource::None,
                })
            })
            .collect(),
    )
}

/// The context length in a `POST /api/show` answer: `model_info` carries
/// `"<architecture>.context_length"`, the architecture being
/// `general.architecture`.
pub fn parse_ollama_show_context_length(body: &Value) -> Option<u32> {
    let info = body.get("model_info")?.as_object()?;
    if let Some(arch) = info.get("general.architecture").and_then(Value::as_str) {
        if let Some(window) = info
            .get(&format!("{arch}.context_length"))
            .and_then(value_as_u32)
        {
            return Some(window);
        }
    }
    info.iter()
        .find(|(key, _)| key.ends_with(".context_length"))
        .and_then(|(_, value)| value_as_u32(value))
}

/// One page of DashScope's native `GET /api/v1/models`: the models
/// (text-generation ones only, when the entry says what it can do) and
/// the total count for pagination.
pub fn parse_dashscope_models_page(body: &Value) -> Option<(Vec<DiscoveredModel>, Option<u32>)> {
    let output = body.get("output")?;
    let models = output.get("models")?.as_array()?;
    let total = output.get("total").and_then(value_as_u32);
    let mut out = Vec::with_capacity(models.len());
    for entry in models {
        let Some(id) = entry.get("model").and_then(Value::as_str) else {
            continue;
        };
        if let Some(capabilities) = entry.get("capabilities").and_then(Value::as_array) {
            let text_generation = capabilities
                .iter()
                .filter_map(Value::as_str)
                .any(|cap| cap.eq_ignore_ascii_case("TG"));
            if !capabilities.is_empty() && !text_generation {
                continue;
            }
        }
        let info = entry.get("model_info");
        let context_window =
            info.and_then(|info| first_u32(info, &["context_window", "max_input_tokens"]));
        let max_output_tokens = info.and_then(|info| first_u32(info, &["max_output_tokens"]));
        out.push(DiscoveredModel {
            id: id.trim().to_string(),
            display_name: entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty() && *name != id)
                .map(str::to_string),
            limits_source: if context_window.is_some() {
                LimitsSource::Endpoint
            } else {
                LimitsSource::None
            },
            context_window,
            max_output_tokens,
        });
    }
    Some((out, total))
}

fn first_u32(entry: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| entry.get(*key).and_then(value_as_u32))
        .filter(|n| *n > 0)
}

fn value_as_u32(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f as u64))
            .and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(vendor: ProviderVendor, wire: WireFamily) -> ModelDiscoveryRequest {
        ModelDiscoveryRequest {
            vendor,
            wire,
            base_url: "https://example.invalid/v1".into(),
            api_key: "sk".into(),
            extra_headers: Vec::new(),
        }
    }

    fn model(id: &str) -> DiscoveredModel {
        DiscoveredModel {
            id: id.into(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            limits_source: LimitsSource::None,
        }
    }

    #[test]
    fn openai_models_url_lands_next_to_the_chat_endpoint() {
        assert_eq!(
            openai_models_url("https://api.openai.com/v1", ""),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            openai_models_url("https://api.deepseek.com", ""),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            openai_models_url("https://api.deepseek.com/", ""),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            openai_models_url("https://open.bigmodel.cn/api/paas/v4/", ""),
            "https://open.bigmodel.cn/api/paas/v4/models"
        );
        assert_eq!(
            openai_models_url(
                "https://generativelanguage.googleapis.com/v1beta/openai",
                ""
            ),
            "https://generativelanguage.googleapis.com/v1beta/openai/models"
        );
        assert_eq!(
            openai_models_url("https://api.siliconflow.cn/v1", "sub_type=chat"),
            "https://api.siliconflow.cn/v1/models?sub_type=chat"
        );
        // A pasted endpoint path is not a base.
        assert_eq!(
            openai_models_url("https://relay.example/v1/chat/completions", ""),
            "https://relay.example/v1/models"
        );
        assert_eq!(
            openai_models_url("https://opencode.ai/zen/v1/responses", ""),
            "https://opencode.ai/zen/v1/models"
        );
        assert_eq!(
            openai_models_url("http://localhost:11434/v1", ""),
            "http://localhost:11434/v1/models"
        );
    }

    #[test]
    fn anthropic_models_url_mirrors_the_messages_placement() {
        assert_eq!(
            anthropic_models_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            anthropic_models_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            anthropic_models_url("https://api.anthropic.com/v1/messages"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            anthropic_models_url("https://api.deepseek.com/anthropic/"),
            "https://api.deepseek.com/anthropic/v1/models"
        );
    }

    #[test]
    fn ollama_root_strips_the_compat_suffix() {
        assert_eq!(
            ollama_root("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_root("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_root("http://localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(ollama_root("http://box:11434/api"), "http://box:11434");
    }

    #[test]
    fn dashscope_models_url_moves_from_compat_to_native() {
        assert_eq!(
            dashscope_models_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "https://dashscope.aliyuncs.com/api/v1/models"
        );
        assert_eq!(
            dashscope_models_url("https://ws-1.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/"),
            "https://ws-1.cn-beijing.maas.aliyuncs.com/api/v1/models"
        );
        assert_eq!(
            dashscope_models_url("https://dashscope.aliyuncs.com/apps/anthropic"),
            "https://dashscope.aliyuncs.com/api/v1/models"
        );
        assert_eq!(
            dashscope_models_url("https://dashscope-intl.aliyuncs.com"),
            "https://dashscope-intl.aliyuncs.com/api/v1/models"
        );
    }

    #[test]
    fn openai_list_parses_ids_limits_and_gemini_prefixes() {
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "kimi-k3", "object": "model", "owned_by": "moonshot",
                 "context_length": 1048576, "supports_reasoning": true},
                {"id": "models/gemini-3.7-flash", "object": "model"},
                {"id": "  ", "object": "model"},
                {"object": "model"},
                {"id": "x", "context_length": "8192", "max_output_tokens": 0}
            ]
        });
        let models = parse_openai_models_list(&body).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "kimi-k3");
        assert_eq!(models[0].context_window, Some(1_048_576));
        assert_eq!(models[0].limits_source, LimitsSource::Endpoint);
        assert_eq!(models[1].id, "gemini-3.7-flash");
        assert_eq!(models[1].context_window, None);
        assert_eq!(models[2].context_window, Some(8192));
        assert_eq!(models[2].max_output_tokens, None, "zero is not a limit");
        assert!(parse_openai_models_list(&serde_json::json!({"models": []})).is_none());
    }

    #[test]
    fn anthropic_page_parses_limits_and_pagination() {
        let body = serde_json::json!({
            "data": [
                {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5",
                 "created_at": "2026-07-24T00:00:00Z",
                 "max_input_tokens": 1000000, "max_tokens": 128000},
                {"type": "model", "id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5",
                 "max_input_tokens": null, "max_tokens": null}
            ],
            "first_id": "claude-opus-5",
            "last_id": "claude-haiku-4-5-20251001",
            "has_more": true
        });
        let (models, next) = parse_anthropic_models_page(&body).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context_window, Some(1_000_000));
        assert_eq!(models[0].max_output_tokens, Some(128_000));
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Opus 5"));
        assert_eq!(models[1].context_window, None);
        assert_eq!(next.as_deref(), Some("claude-haiku-4-5-20251001"));

        let last = serde_json::json!({"data": [], "has_more": false, "last_id": null});
        assert_eq!(parse_anthropic_models_page(&last).unwrap().1, None);
    }

    #[test]
    fn ollama_tags_and_show_parse() {
        let tags = serde_json::json!({
            "models": [
                {"name": "llama3.3:70b", "model": "llama3.3:70b", "size": 1},
                {"model": "qwen3:8b"},
                {"name": ""}
            ]
        });
        let models = parse_ollama_tags(&tags).unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["llama3.3:70b", "qwen3:8b"]
        );

        let show = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 131072,
                "llama.embedding_length": 8192
            }
        });
        assert_eq!(parse_ollama_show_context_length(&show), Some(131_072));
        let unnamed_arch = serde_json::json!({
            "model_info": { "qwen3.context_length": 40960 }
        });
        assert_eq!(
            parse_ollama_show_context_length(&unnamed_arch),
            Some(40_960)
        );
        assert_eq!(
            parse_ollama_show_context_length(&serde_json::json!({"model_info": {}})),
            None
        );
    }

    #[test]
    fn dashscope_page_keeps_text_generation_models_with_their_limits() {
        let body = serde_json::json!({
            "output": {
                "total": 2,
                "page_no": 1,
                "page_size": 100,
                "models": [
                    {"model": "qwen3.7-plus", "name": "通义千问3.7-Plus", "capabilities": ["TG", "Reasoning"],
                     "model_info": {"context_window": 1000000, "max_input_tokens": 991808, "max_output_tokens": 131072}},
                    {"model": "text-embedding-v4", "name": "Embedding", "capabilities": ["TE"],
                     "model_info": {"context_window": 8192}},
                    {"model": "qwen-plus", "capabilities": [], "model_info": {}}
                ]
            },
            "request_id": "r"
        });
        let (models, total) = parse_dashscope_models_page(&body).unwrap();
        assert_eq!(total, Some(2));
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["qwen3.7-plus", "qwen-plus"]
        );
        assert_eq!(models[0].context_window, Some(1_000_000));
        assert_eq!(models[0].max_output_tokens, Some(131_072));
        assert_eq!(models[0].display_name.as_deref(), Some("通义千问3.7-Plus"));
        assert_eq!(models[1].context_window, None);
    }

    #[test]
    fn finish_filters_enriches_dedups_and_orders() {
        let raw = vec![
            model("text-embedding-3-large"),
            model("gpt-5.5"),
            model("gpt-5.6-sol"),
            model("gpt-5.5"),
            model("aardvark-experimental"),
        ];
        let request = request(ProviderVendor::OpenAi, WireFamily::OpenAiResponses);
        let discovery = finish(&request, raw, "u".into(), true);
        assert_eq!(discovery.skipped_non_chat, 1);
        assert_eq!(discovery.skipped_other_wire, 0);
        assert_eq!(
            discovery
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-5.6-sol", "gpt-5.5", "aardvark-experimental"],
            "catalogue order first, then the rest alphabetically"
        );
        assert_eq!(discovery.models[0].context_window, Some(1_050_000));
        assert_eq!(discovery.models[0].limits_source, LimitsSource::Catalogue);
        assert_eq!(discovery.models[2].limits_source, LimitsSource::None);
    }

    #[test]
    fn finish_keeps_endpoint_limits_over_the_catalogue() {
        let mut listed = model("claude-opus-5");
        listed.context_window = Some(999);
        listed.limits_source = LimitsSource::Endpoint;
        let request = request(ProviderVendor::Anthropic, WireFamily::Anthropic);
        let discovery = finish(&request, vec![listed], "u".into(), true);
        assert_eq!(discovery.models[0].context_window, Some(999));
        assert_eq!(discovery.models[0].limits_source, LimitsSource::Endpoint);
    }

    #[test]
    fn zen_listing_is_narrowed_to_the_configured_wire() {
        let raw = vec![
            model("gpt-5.6-sol"),
            model("claude-opus-5"),
            model("kimi-k3"),
            model("gemini-3.7-flash"),
        ];
        let request = request(ProviderVendor::OpenCode, WireFamily::Anthropic);
        let discovery = finish(&request, raw.clone(), "u".into(), true);
        assert_eq!(
            discovery
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-opus-5"]
        );
        assert_eq!(discovery.skipped_other_wire, 3);
        // And the reseller borrows the upstream limits.
        assert_eq!(discovery.models[0].context_window, Some(1_000_000));

        let chat = request_with(ProviderVendor::OpenCode, WireFamily::OpenAiChat);
        let discovery = finish(&chat, raw, "u".into(), true);
        assert_eq!(
            discovery
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["kimi-k3"]
        );
    }

    fn request_with(vendor: ProviderVendor, wire: WireFamily) -> ModelDiscoveryRequest {
        request(vendor, wire)
    }

    #[test]
    fn catalogue_fallback_has_the_documented_limits() {
        let discovery = catalogue_as_discovery(ProviderVendor::Volcengine, WireFamily::OpenAiChat);
        assert_eq!(discovery.models[0].id, "doubao-seed-evolving");
        assert_eq!(discovery.models[0].context_window, Some(1_024_000));
        assert_eq!(discovery.models[0].limits_source, LimitsSource::Catalogue);
        assert!(discovery.documented);
        assert!(
            catalogue_as_discovery(ProviderVendor::Ollama, WireFamily::OpenAiChat)
                .models
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unsupported_vendor_says_so_without_a_request() {
        let http = reqwest::Client::new();
        let request = request(ProviderVendor::Volcengine, WireFamily::OpenAiChat);
        let err = discover_models(&http, &request).await.unwrap_err();
        assert!(
            matches!(err, ModelDiscoveryError::Unsupported { .. }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn empty_base_url_is_rejected_before_any_request() {
        let http = reqwest::Client::new();
        let mut request = request(ProviderVendor::OpenAi, WireFamily::OpenAiChat);
        request.base_url = "   ".into();
        let err = discover_models(&http, &request).await.unwrap_err();
        assert!(matches!(err, ModelDiscoveryError::Http { .. }), "{err}");
    }

    #[test]
    fn value_as_u32_reads_numbers_and_numeric_strings() {
        assert_eq!(value_as_u32(&serde_json::json!(42)), Some(42));
        assert_eq!(value_as_u32(&serde_json::json!(42.0)), Some(42));
        assert_eq!(value_as_u32(&serde_json::json!("128000")), Some(128_000));
        assert_eq!(value_as_u32(&serde_json::json!("lots")), None);
        assert_eq!(value_as_u32(&serde_json::json!(null)), None);
    }
}
