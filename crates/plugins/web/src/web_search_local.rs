//! Provider-independent web search backend for `WebSearchTool`.
//!
//! Used whenever the active model provider has no server-side
//! `web_search` capability (API-key routes, relays, non-OpenAI
//! providers) — the tool falls back to querying public SERP endpoints
//! directly over HTTP and parsing the results locally. Engine order:
//! Brave Search API when a key is configured (cleanest results, JSON),
//! then DuckDuckGo's static HTML endpoint, then Bing. The first engine
//! that yields at least one result wins; an engine that errors or is
//! bot-challenged is skipped rather than failing the whole search.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use scraper::{Html, Selector};
use serde_json::Value;

/// SERP endpoints serve degraded or challenge pages to unknown
/// clients; a mainstream desktop UA keeps the static-HTML paths on
/// their normal markup.
const DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSearchEngine {
    Brave,
    DuckDuckGo,
    Bing,
}

impl LocalSearchEngine {
    pub fn label(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::DuckDuckGo => "duckduckgo",
            Self::Bing => "bing",
        }
    }
}

#[derive(Debug)]
pub struct LocalSearchOutcome {
    pub engine: LocalSearchEngine,
    pub results: Vec<LocalSearchResult>,
}

pub(crate) fn http_client() -> anyhow::Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .user_agent(DESKTOP_USER_AGENT)
        .build()
        .context("failed to build the local web search HTTP client")?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

fn brave_api_key() -> Option<String> {
    ["REBON_BRAVE_API_KEY", "BRAVE_API_KEY"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

fn forced_engine() -> Option<LocalSearchEngine> {
    let value = std::env::var("REBON_WEB_SEARCH_ENGINE").ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "brave" => Some(LocalSearchEngine::Brave),
        "duckduckgo" | "ddg" => Some(LocalSearchEngine::DuckDuckGo),
        "bing" => Some(LocalSearchEngine::Bing),
        _ => None,
    }
}

fn engine_order() -> Vec<LocalSearchEngine> {
    if let Some(forced) = forced_engine() {
        return vec![forced];
    }
    let mut order = Vec::new();
    if brave_api_key().is_some() {
        order.push(LocalSearchEngine::Brave);
    }
    order.extend([LocalSearchEngine::DuckDuckGo, LocalSearchEngine::Bing]);
    order
}

pub async fn run_local_web_search(
    query: &str,
    max_results: usize,
    allowed_domains: Option<&[String]>,
) -> anyhow::Result<LocalSearchOutcome> {
    let client = http_client()?;
    let order = engine_order();
    let mut errors = Vec::new();
    let mut last_attempted = None;
    for engine in order.iter().copied() {
        last_attempted = Some(engine);
        let fetched = match engine {
            LocalSearchEngine::Brave => search_brave(&client, query, max_results).await,
            LocalSearchEngine::DuckDuckGo => search_duckduckgo(&client, query).await,
            LocalSearchEngine::Bing => search_bing(&client, query, max_results).await,
        };
        match fetched {
            Ok(mut results) => {
                if let Some(domains) = allowed_domains {
                    results.retain(|result| domain_allowed(&result.url, domains));
                }
                results.truncate(max_results);
                if !results.is_empty() {
                    return Ok(LocalSearchOutcome { engine, results });
                }
            }
            Err(err) => errors.push(format!("{}: {err:#}", engine.label())),
        }
    }
    if !errors.is_empty() && errors.len() == order.len() {
        bail!("all local search engines failed — {}", errors.join("; "));
    }
    Ok(LocalSearchOutcome {
        engine: last_attempted.unwrap_or(LocalSearchEngine::DuckDuckGo),
        results: Vec::new(),
    })
}

async fn search_brave(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<LocalSearchResult>> {
    let key = brave_api_key().ok_or_else(|| anyhow!("no Brave Search API key configured"))?;
    let count = max_results.clamp(1, 20).to_string();
    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", count.as_str())])
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .send()
        .await
        .context("request to the Brave Search API failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Brave Search API returned HTTP {status}");
    }
    let body: Value = response
        .json()
        .await
        .context("Brave Search API returned invalid JSON")?;
    Ok(parse_brave_json(&body))
}

fn parse_brave_json(body: &Value) -> Vec<LocalSearchResult> {
    let Some(items) = body.pointer("/web/results").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let url = item.get("url")?.as_str()?.trim().to_string();
            if !is_web_url(&url) {
                return None;
            }
            let title = strip_html_fragment(item.get("title")?.as_str()?);
            if title.is_empty() {
                return None;
            }
            let snippet = item
                .get("description")
                .and_then(Value::as_str)
                .map(strip_html_fragment)
                .unwrap_or_default();
            Some(LocalSearchResult {
                title,
                url,
                snippet,
            })
        })
        .collect()
}

async fn search_duckduckgo(
    client: &reqwest::Client,
    query: &str,
) -> anyhow::Result<Vec<LocalSearchResult>> {
    let response = client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .header("Accept-Language", ACCEPT_LANGUAGE)
        .send()
        .await
        .context("request to DuckDuckGo failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("DuckDuckGo returned HTTP {status}");
    }
    let body = response
        .text()
        .await
        .context("could not read the DuckDuckGo response body")?;
    let results = parse_duckduckgo_html(&body);
    if results.is_empty() && looks_like_bot_challenge(&body) {
        bail!("DuckDuckGo served a bot challenge instead of results");
    }
    Ok(results)
}

fn parse_duckduckgo_html(html: &str) -> Vec<LocalSearchResult> {
    let document = Html::parse_document(html);
    let result_selector = Selector::parse("div.result").expect("static selector");
    let title_selector = Selector::parse("a.result__a").expect("static selector");
    let snippet_selector = Selector::parse(".result__snippet").expect("static selector");
    let mut results = Vec::new();
    for element in document.select(&result_selector) {
        if element
            .value()
            .classes()
            .any(|class| class.starts_with("result--ad"))
        {
            continue;
        }
        let Some(anchor) = element.select(&title_selector).next() else {
            continue;
        };
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = normalize_duckduckgo_url(href) else {
            continue;
        };
        let title = collapse_whitespace(&anchor.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let snippet = element
            .select(&snippet_selector)
            .next()
            .map(|snippet| collapse_whitespace(&snippet.text().collect::<String>()))
            .unwrap_or_default();
        results.push(LocalSearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// DuckDuckGo result links are usually redirects of the form
/// `//duckduckgo.com/l/?uddg=<url-encoded target>&rut=…`; unwrap them
/// to the real destination so the agent gets a directly fetchable URL.
fn normalize_duckduckgo_url(href: &str) -> Option<String> {
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    let parsed = reqwest::Url::parse(&absolute).ok()?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let is_duckduckgo = host == "duckduckgo.com" || host.ends_with(".duckduckgo.com");
    if is_duckduckgo && parsed.path().starts_with("/l/") {
        let target = parsed
            .query_pairs()
            .find(|(key, _)| key == "uddg")
            .map(|(_, value)| value.into_owned())?;
        if !is_web_url(&target) {
            return None;
        }
        return Some(target);
    }
    if !is_web_url(&absolute) {
        return None;
    }
    Some(absolute)
}

async fn search_bing(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<LocalSearchResult>> {
    let count = max_results.clamp(10, 30).to_string();
    let response = client
        .get("https://www.bing.com/search")
        .query(&[("q", query), ("count", count.as_str())])
        .header("Accept-Language", ACCEPT_LANGUAGE)
        .send()
        .await
        .context("request to Bing failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Bing returned HTTP {status}");
    }
    let body = response
        .text()
        .await
        .context("could not read the Bing response body")?;
    Ok(parse_bing_html(&body))
}

fn parse_bing_html(html: &str) -> Vec<LocalSearchResult> {
    let document = Html::parse_document(html);
    let result_selector = Selector::parse("li.b_algo").expect("static selector");
    let title_selector = Selector::parse("h2 a").expect("static selector");
    let snippet_selector = Selector::parse(
        "div.b_caption p, p.b_lineclamp1, p.b_lineclamp2, p.b_lineclamp3, p.b_lineclamp4, p",
    )
    .expect("static selector");
    let mut results = Vec::new();
    for element in document.select(&result_selector) {
        let Some(anchor) = element.select(&title_selector).next() else {
            continue;
        };
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = normalize_bing_url(href) else {
            continue;
        };
        let title = collapse_whitespace(&anchor.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let snippet = element
            .select(&snippet_selector)
            .next()
            .map(|snippet| collapse_whitespace(&snippet.text().collect::<String>()))
            .unwrap_or_default();
        results.push(LocalSearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// Bing wraps result links in `bing.com/ck/a?…&u=a1<base64url>&…`
/// click-tracking redirects; decode the `u` parameter (a `a1`-prefixed
/// base64url payload) back to the destination URL.
fn normalize_bing_url(href: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(href).ok()?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let is_bing = host == "bing.com" || host.ends_with(".bing.com");
    if is_bing && parsed.path().starts_with("/ck/") {
        let raw = parsed
            .query_pairs()
            .find(|(key, _)| key == "u")
            .map(|(_, value)| value.into_owned())?;
        let encoded = raw.strip_prefix("a1")?;
        let decoded = decode_base64url(encoded)?;
        let target = String::from_utf8(decoded).ok()?;
        if !is_web_url(&target) {
            return None;
        }
        return Some(target);
    }
    if !is_web_url(href) {
        return None;
    }
    Some(href.to_string())
}

fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .ok()
}

fn is_web_url(value: &str) -> bool {
    reqwest::Url::parse(value)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn looks_like_bot_challenge(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("anomaly") || lower.contains("challenge-form") || lower.contains("captcha")
}

fn domain_allowed(url: &str, domains: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    domains.iter().any(|domain| {
        let domain = domain.trim().trim_matches('.').to_ascii_lowercase();
        !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
    })
}

/// Brave titles/descriptions embed highlight markup (`<strong>…`);
/// flatten any HTML fragment to plain text.
fn strip_html_fragment(fragment: &str) -> String {
    collapse_whitespace(
        &Html::parse_fragment(fragment)
            .root_element()
            .text()
            .collect::<String>(),
    )
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn duckduckgo_parser_skips_ads_and_unwraps_redirects() {
        let html = r##"
<html><body><div id="links" class="results">
  <div class="result results_links result--ad">
    <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fads.example.com%2F&rut=x">Sponsored hit</a></h2>
    <a class="result__snippet" href="#">Buy things now</a>
  </div>
  <div class="result results_links results_links_deep web-result">
    <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&rut=y">Rust <b>Programming</b> Language</a></h2>
    <a class="result__snippet" href="#">A language <b>empowering</b> everyone
      to build reliable software.</a>
  </div>
  <div class="result web-result">
    <h2 class="result__title"><a class="result__a" href="https://docs.rs/tokio">tokio - Rust</a></h2>
    <a class="result__snippet" href="#">Tokio documentation.</a>
  </div>
</div></body></html>"##;

        let results = parse_duckduckgo_html(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            results[0].snippet,
            "A language empowering everyone to build reliable software."
        );
        assert_eq!(results[1].url, "https://docs.rs/tokio");
    }

    #[test]
    fn bing_parser_decodes_click_tracking_redirects() {
        let encoded = URL_SAFE_NO_PAD.encode("https://www.rust-lang.org/");
        let html = format!(
            r##"
<html><body><ol id="b_results">
  <li class="b_algo">
    <h2><a href="https://www.bing.com/ck/a?!&&p=xyz&u=a1{encoded}&ntb=1">Rust Programming Language</a></h2>
    <div class="b_caption"><p>A language empowering everyone.</p></div>
  </li>
  <li class="b_algo">
    <h2><a href="https://docs.rs/tokio">tokio</a></h2>
    <div class="b_caption"><p>Tokio documentation.</p></div>
  </li>
</ol></body></html>"##
        );

        let results = parse_bing_html(&html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].snippet, "A language empowering everyone.");
        assert_eq!(results[1].url, "https://docs.rs/tokio");
    }

    #[test]
    fn brave_parser_reads_web_results_and_strips_markup() {
        let body = serde_json::json!({
            "web": {
                "results": [
                    {
                        "title": "Rust <strong>Programming</strong> Language",
                        "url": "https://www.rust-lang.org/",
                        "description": "A language <strong>empowering</strong> everyone."
                    },
                    { "title": "skipped", "url": "ftp://example.com/" }
                ]
            }
        });

        let results = parse_brave_json(&body);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].snippet, "A language empowering everyone.");
    }

    #[test]
    fn redirect_unwrapping_rejects_non_web_targets() {
        assert_eq!(
            normalize_duckduckgo_url("//duckduckgo.com/l/?uddg=javascript%3Aalert(1)"),
            None
        );
        assert_eq!(
            normalize_duckduckgo_url("https://example.com/page"),
            Some("https://example.com/page".to_string())
        );
        let encoded = URL_SAFE_NO_PAD.encode("javascript:alert(1)");
        assert_eq!(
            normalize_bing_url(&format!("https://www.bing.com/ck/a?u=a1{encoded}")),
            None
        );
    }

    // Network smoke test, excluded from CI. Run manually with:
    // `cargo test -p rebon-plugin-web --lib -- --ignored --nocapture live_local_search`
    #[tokio::test]
    #[ignore = "hits live search endpoints"]
    async fn live_local_search_returns_results() {
        let outcome = run_local_web_search("rust programming language", 5, None)
            .await
            .expect("local search failed");
        for result in &outcome.results {
            println!(
                "[{}] {} — {}",
                outcome.engine.label(),
                result.title,
                result.url
            );
        }
        assert!(!outcome.results.is_empty());
    }

    #[test]
    fn domain_allowlist_matches_hosts_and_subdomains() {
        let domains = vec!["rust-lang.org".to_string(), "Docs.rs".to_string()];
        assert!(domain_allowed("https://www.rust-lang.org/learn", &domains));
        assert!(domain_allowed("https://docs.rs/tokio", &domains));
        assert!(!domain_allowed("https://rust-lang.org.evil.com/", &domains));
        assert!(!domain_allowed("https://example.com/", &domains));
    }
}
