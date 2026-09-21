//! The Images API endpoint of the session's provider, and the one HTTP
//! exchange the tool makes with it.
//!
//! Image generation is not a model turn: it is a plain `POST` to
//! `images/generations` or `images/edits` next to the provider's Responses
//! route, with the same bearer. So the provider resolution that already knows
//! the base URL and the credentials — `rebon-harness`, when it resolves a
//! first-party OpenAI route — builds an [`ImagesEndpoint`] and publishes it
//! through [`set_provider_endpoint`], and the tool reads it at call time. A
//! provider that is not first-party OpenAI publishes `None`, which is what
//! takes the tool off the model's list.

use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use rebon_api::TokenRefresher;
use serde::Deserialize;
use serde_json::Value;

/// How long one image request may take before it is abandoned.
///
/// Generation routinely takes one to two minutes and high-quality edits of
/// several references take longer; the bound is there so a request the
/// server never answers cannot pin the turn forever, not to hurry a slow one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How much of an error body is quoted back to the model. Enough for the
/// API's JSON error object; not a whole HTML error page.
const ERROR_BODY_LIMIT: usize = 2_000;

/// One published endpoint: where the Images API is, and how to authenticate.
pub struct ImagesEndpoint {
    base_url: String,
    access_token: Mutex<String>,
    refresher: Option<Arc<dyn TokenRefresher>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for ImagesEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagesEndpoint")
            .field("base_url", &self.base_url)
            .field("refresher", &self.refresher.is_some())
            .finish_non_exhaustive()
    }
}

impl ImagesEndpoint {
    /// `base_url` is the provider's configured base URL, as the Responses
    /// client receives it; `access_token` is its API key or current OAuth
    /// access token. An OAuth route passes the refresher its Responses client
    /// uses, so a token that expired hours into the session is rotated here
    /// the same way.
    pub fn new(
        base_url: impl Into<String>,
        access_token: impl Into<String>,
        refresher: Option<Arc<dyn TokenRefresher>>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            access_token: Mutex::new(access_token.into()),
            refresher,
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("a client with only a timeout set always builds"),
        }
    }

    pub(crate) fn url(&self, operation: Operation) -> String {
        images_url(&self.base_url, operation)
    }

    fn access_token(&self) -> String {
        self.access_token
            .lock()
            .expect("images access token poisoned")
            .clone()
    }

    /// Send `body` to `operation` and return the first image.
    ///
    /// A 401 or 403 with a refresher attached rotates the token and retries
    /// once; there is no other retry, because a generation that failed for
    /// any other reason is the model's to rephrase or the user's to hear
    /// about, and a blind resend is billed again.
    pub(crate) async fn send(
        &self,
        operation: Operation,
        body: &Value,
    ) -> Result<ImageOutput, String> {
        match self.send_once(operation, body, &self.access_token()).await {
            Err(SendError::Unauthorized(message)) => {
                let Some(refresher) = &self.refresher else {
                    return Err(message);
                };
                let token = refresher
                    .refresh()
                    .await
                    .map_err(|err| format!("{message}; token refresh failed: {err}"))?;
                *self
                    .access_token
                    .lock()
                    .expect("images access token poisoned") = token.clone();
                self.send_once(operation, body, &token)
                    .await
                    .map_err(SendError::into_message)
            }
            other => other.map_err(SendError::into_message),
        }
    }

    async fn send_once(
        &self,
        operation: Operation,
        body: &Value,
        token: &str,
    ) -> Result<ImageOutput, SendError> {
        let response = self
            .http
            .post(self.url(operation))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|err| {
                SendError::Other(format!("{} request failed: {err}", operation.label()))
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|err| {
            SendError::Other(format!(
                "{} response could not be read: {err}",
                operation.label()
            ))
        })?;
        if !status.is_success() {
            let message = format!(
                "{} returned HTTP {}: {}",
                operation.label(),
                status.as_u16(),
                truncate(&text, ERROR_BODY_LIMIT)
            );
            return Err(if matches!(status.as_u16(), 401 | 403) {
                SendError::Unauthorized(message)
            } else {
                SendError::Other(message)
            });
        }
        parse_response(&text).map_err(SendError::Other)
    }
}

enum SendError {
    Unauthorized(String),
    Other(String),
}

impl SendError {
    fn into_message(self) -> String {
        match self {
            Self::Unauthorized(message) | Self::Other(message) => message,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Generate,
    Edit,
}

impl Operation {
    fn path(self) -> &'static str {
        match self {
            Self::Generate => "images/generations",
            Self::Edit => "images/edits",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Generate => "image generation",
            Self::Edit => "image edit",
        }
    }
}

/// The Images API URL for `operation` under a provider base URL.
///
/// Provider entries carry the base the Responses client was given, and it
/// comes in three shapes: a bare origin (`https://api.openai.com`, which the
/// Responses client extends with `/v1`), a versioned base
/// (`https://api.openai.com/v1`), or the Codex OAuth route, stored with its
/// `/responses` suffix (`https://chatgpt.com/backend-api/codex/responses`).
/// The Images API sits beside `responses` in every case. An empty base is the
/// OpenAI default, the same reading `rebon_config::is_openai_api_endpoint`
/// gives it.
pub(crate) fn images_url(base_url: &str, operation: Operation) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    let base = trimmed.strip_suffix("/responses").unwrap_or(trimmed);
    let base = if base.is_empty() {
        "https://api.openai.com/v1".to_string()
    } else if is_bare_origin(base) {
        format!("{base}/v1")
    } else {
        base.to_string()
    };
    format!("{base}/{}", operation.path())
}

fn is_bare_origin(base: &str) -> bool {
    base.split_once("://")
        .is_some_and(|(_, rest)| !rest.contains('/'))
}

/// What one successful request produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageOutput {
    pub b64_json: String,
    /// What the server says the background came out as (`transparent` or
    /// `opaque`), when it says.
    pub background: Option<String>,
    pub revised_prompt: Option<String>,
}

#[derive(Deserialize)]
struct ImagesResponse {
    data: Vec<ImageDatum>,
    background: Option<String>,
}

#[derive(Deserialize)]
struct ImageDatum {
    b64_json: Option<String>,
    revised_prompt: Option<String>,
}

fn parse_response(text: &str) -> Result<ImageOutput, String> {
    let response: ImagesResponse = serde_json::from_str(text)
        .map_err(|err| format!("image response was not the expected JSON: {err}"))?;
    let datum = response
        .data
        .into_iter()
        .next()
        .ok_or_else(|| "image response carried no image".to_string())?;
    let b64_json = datum
        .b64_json
        .filter(|data| !data.trim().is_empty())
        .ok_or_else(|| "image response carried no base64 image data".to_string())?;
    Ok(ImageOutput {
        b64_json,
        background: response.background,
        revised_prompt: datum.revised_prompt,
    })
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn endpoint_cell() -> &'static RwLock<Option<Arc<ImagesEndpoint>>> {
    static CELL: OnceLock<RwLock<Option<Arc<ImagesEndpoint>>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// Publish the Images endpoint of the provider this process's session just
/// adopted, or `None` when that provider has none.
///
/// The one writer is the assembly step that resolved the provider
/// (`rebon_harness::RuntimeModel::publish_provider_capabilities`), on every
/// surface that adopts a runtime: session assembly, `/provider` and `/model`,
/// a background worker switching model. The tool is registered on the
/// process tool seat, which has no session of its own to ask, so writer and
/// reader meet at this cell — the arrangement `ComputerUse` uses for the
/// same reason, with the same limit: a process running sessions on two
/// providers at once sees the last one adopted.
///
/// The cell outlives the plugin's enable/disable cycle on purpose: turning
/// the plugin back on must find the endpoint the provider resolution left,
/// not an empty cell.
pub fn set_provider_endpoint(endpoint: Option<Arc<ImagesEndpoint>>) {
    *endpoint_cell()
        .write()
        .expect("images endpoint cell poisoned") = endpoint;
}

/// Held by every test that publishes into the process-wide cell, so two of
/// them running in parallel cannot see each other's endpoint.
#[cfg(test)]
pub(crate) fn endpoint_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The endpoint the tool calls, when the adopted provider has one.
pub(crate) fn provider_endpoint() -> Option<Arc<ImagesEndpoint>> {
    endpoint_cell()
        .read()
        .expect("images endpoint cell poisoned")
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_images_api_sits_beside_responses_for_every_base_shape() {
        for (base, expected) in [
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/images/generations",
            ),
            (
                "https://api.openai.com/v1/",
                "https://api.openai.com/v1/images/generations",
            ),
            (
                "https://api.openai.com",
                "https://api.openai.com/v1/images/generations",
            ),
            ("", "https://api.openai.com/v1/images/generations"),
            (
                "https://chatgpt.com/backend-api/codex/responses",
                "https://chatgpt.com/backend-api/codex/images/generations",
            ),
            (
                "https://chatgpt.com/backend-api/codex",
                "https://chatgpt.com/backend-api/codex/images/generations",
            ),
            (
                "https://api.openai.com/v1/responses/",
                "https://api.openai.com/v1/images/generations",
            ),
        ] {
            assert_eq!(images_url(base, Operation::Generate), expected, "{base:?}");
        }
        assert_eq!(
            images_url("https://api.openai.com/v1", Operation::Edit),
            "https://api.openai.com/v1/images/edits"
        );
    }

    #[test]
    fn a_response_yields_its_first_image_and_what_the_server_reported() {
        let output = parse_response(
            r#"{"created":1,"background":"transparent","data":[{"b64_json":"QUJD","revised_prompt":"a cat"},{"b64_json":"REVG"}]}"#,
        )
        .expect("well-formed");
        assert_eq!(
            output,
            ImageOutput {
                b64_json: "QUJD".into(),
                background: Some("transparent".into()),
                revised_prompt: Some("a cat".into()),
            }
        );
    }

    #[test]
    fn a_response_without_image_data_is_an_error() {
        assert!(parse_response(r#"{"data":[]}"#)
            .unwrap_err()
            .contains("no image"));
        assert!(parse_response(r#"{"data":[{"url":"https://x"}]}"#)
            .unwrap_err()
            .contains("no base64"));
        assert!(parse_response("<html>")
            .unwrap_err()
            .contains("expected JSON"));
    }

    #[test]
    fn long_error_bodies_are_cut_on_a_character_boundary() {
        let body = "é".repeat(ERROR_BODY_LIMIT);
        let cut = truncate(&body, ERROR_BODY_LIMIT);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= ERROR_BODY_LIMIT + '…'.len_utf8());
        assert_eq!(truncate("short", ERROR_BODY_LIMIT), "short");
    }

    #[test]
    fn the_debug_form_never_prints_the_token() {
        let endpoint = ImagesEndpoint::new("https://api.openai.com/v1", "sk-secret", None);
        assert!(!format!("{endpoint:?}").contains("sk-secret"));
    }
}
