//! The TypeSafe System One wire: typed questions in, typed answers out.
//!
//! Rebon's other classifier sends a chat request and parses JSON out of a text
//! reply. This one cannot: a System One model does not generate strings at all,
//! so both what is asked and what comes back are declared here.
//!
//! Reference: <https://docs.typesafe.ai/api>.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

/// Where a production call goes. The client takes its endpoint from the caller
/// so a test can point it at a loopback server.
pub const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
/// TypeSafe's alias for the model that serves `systemone`. The response names
/// the exact build that answered.
pub const DEFAULT_MODEL: &str = "jev-latest";

/// The type tag of every question this client asks.
const CHOICE: &str = "choice";

/// Each HTTP attempt is bounded independently so a retry is still finite.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(4);
/// The pause before the single retry the API asks for on 429/529.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
/// How much of an error body is worth repeating in a message.
const SNIPPET_CHARS: usize = 200;

/// One request body.
#[derive(Debug, Serialize)]
pub struct Request {
    /// What to judge. Structured rather than a bare string so the classifier
    /// can see the session it is deciding for.
    pub state: serde_json::Value,
    pub model: String,
    pub questions: BTreeMap<String, ChoiceQuestion>,
}

/// A question whose answer is one of a fixed set of options.
#[derive(Debug, Serialize)]
pub struct ChoiceQuestion {
    #[serde(rename = "type")]
    kind: &'static str,
    /// A string, or a list of strings when the user's policy is one of them.
    pub instructions: serde_json::Value,
    /// Option name to the description that separates it from its neighbours.
    /// `None` is allowed by the API and means the name says it all.
    pub criteria: BTreeMap<String, Option<String>>,
}

impl ChoiceQuestion {
    pub fn new(
        instructions: serde_json::Value,
        criteria: BTreeMap<String, Option<String>>,
    ) -> Self {
        Self {
            kind: CHOICE,
            instructions,
            criteria,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Response {
    /// The exact model that answered, for the log.
    pub model: String,
    /// One answer per question id, keyed the way the request named them.
    pub answers: BTreeMap<String, Answer>,
}

#[derive(Debug, Deserialize)]
pub struct Answer {
    #[serde(rename = "type")]
    kind: String,
    /// Present on a choice answer. Absent only if the API answered a different
    /// kind of question than the one asked.
    choice: Option<String>,
    /// How sure the model is, 0 to 1. Reported, never consulted by the wire client.
    confidence: Option<f64>,
}

impl Answer {
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The chosen option and its confidence, when this is a choice answer.
    pub fn chosen(&self) -> Option<(&str, Option<f64>)> {
        if self.kind != CHOICE {
            return None;
        }
        self.choice
            .as_deref()
            .map(|choice| (choice, self.confidence))
    }
}

/// The API key, from the environment.
///
/// `TYPESAFE_API_KEY` is what TypeSafe's own SDK reads. The `REBON_`-prefixed
/// name is checked first so a shell exporting several agents' keys can point
/// this one at a key of its own.
pub fn api_key_from(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["REBON_TYPESAFE_API_KEY", "TYPESAFE_API_KEY"]
        .iter()
        .filter_map(|name| lookup(name))
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

pub struct SystemOneClient {
    endpoint: String,
    api_key: String,
    http: reqwest::Client,
}

impl SystemOneClient {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::new(
            DEFAULT_ENDPOINT,
            api_key_from(|name| std::env::var(name).ok()),
            ATTEMPT_TIMEOUT,
        )
    }

    /// The key is `None` when the environment has none, which is the one
    /// failure a user has to be told how to fix.
    pub fn new(endpoint: &str, api_key: Option<String>, timeout: Duration) -> anyhow::Result<Self> {
        let api_key = api_key.context(
            "set TYPESAFE_API_KEY (or REBON_TYPESAFE_API_KEY) in the environment to use \
             TypeSafe",
        )?;
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("could not build the TypeSafe HTTP client")?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            api_key,
            http,
        })
    }

    /// Ask, retrying once on the two statuses the API asks to be retried on.
    ///
    /// 429 and 529 mean "later", so one backed-off retry is worth it; every
    /// other failure is about this request, and repeating it only repeats the
    /// error. The caller's own bound is what stops a pathological endpoint.
    pub async fn ask(&self, request: &Request) -> anyhow::Result<Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = self
                .http
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(request)
                .send()
                .await
                .context("could not reach TypeSafe")?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.is_success() {
                return serde_json::from_str(&text)
                    .context("TypeSafe answered something that is not a System One response");
            }
            if attempt == 1 && matches!(status.as_u16(), 429 | 529) {
                tokio::time::sleep(RETRY_BACKOFF).await;
                continue;
            }
            bail!("TypeSafe answered {status}: {}", snippet(&text));
        }
    }
}

/// Enough of a body to see what went wrong, on one line.
fn snippet(body: &str) -> String {
    let flat: String = body
        .trim()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(SNIPPET_CHARS)
        .collect();
    if flat.trim().is_empty() {
        "no body".to_string()
    } else {
        flat
    }
}
