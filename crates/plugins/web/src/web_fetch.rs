//! WebFetch — fetch a URL and return readable text.
//!
//! Fetches over plain HTTP with the shared local-web client, decodes
//! the body (honoring the `Content-Type` charset), and for HTML pages
//! extracts readable text with light Markdown structure: headings,
//! paragraphs, list items, code fences, and absolute links. There is
//! no model-side post-processing — the extracted text is returned
//! as-is, windowed by `offset`/`max_chars` so long pages can be read
//! in slices. JSON and other `text/*` bodies pass through unextracted.
//!
//! Pages that render their content with JavaScript come back nearly
//! empty; the result carries a `note` so the agent can switch to a
//! browser-based tool instead of retrying.

use async_trait::async_trait;
use rebon_permissions::web_fetch_hostname;
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use scraper::{Html, Node, Selector};
use serde_json::{json, Value};

use crate::web_search_local::http_client;
use rebon_tool::{Tool, ToolContext};

pub use rebon_tool::web::WEB_FETCH_TOOL_NAME;

/// Bound the downloaded body so a hostile endpoint cannot exhaust
/// memory; 8 MiB of HTML is far beyond any readable article.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_CHARS: u64 = 20_000;
const MIN_MAX_CHARS: u64 = 1_000;
const MAX_MAX_CHARS: u64 = 100_000;
const WEB_FETCH_MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Default)]
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn id(&self) -> ToolId {
        ToolId::new(WEB_FETCH_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["WebFetchTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Web
    }

    fn description(&self) -> &str {
        "Fetch an http(s) URL and return its readable text content. HTML pages are reduced to text with Markdown-style headings, lists, and absolute links; JSON and plain-text bodies are returned as-is. Long pages are windowed and each response is capped at 16 KiB: the result reports total_chars and next_offset, call again with offset to continue reading. Pages that need JavaScript rendering come back nearly empty."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The http(s) URL to fetch."
                },
                "max_chars": {
                    "type": "integer",
                    "minimum": MIN_MAX_CHARS,
                    "maximum": MAX_MAX_CHARS,
                    "default": DEFAULT_MAX_CHARS,
                    "description": "Maximum characters of extracted text to return; each response is also capped at 16 KiB and reports next_offset when more content remains."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Character offset into the extracted text, for paging long pages."
                },
                "raw": {
                    "type": "boolean",
                    "default": false,
                    "description": "Return the raw response body without HTML extraction."
                }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("fetch url page article docs website content download read web")
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    // Metadata only — permission gating happens in
    // `check_permissions`, not here. Kept false because a GET to an
    // arbitrary URL is an outbound request that can leak data via
    // query params.
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    // Ask per fetch; "allow always" persists a `WebFetch(domain:<host>)`
    // rule so approval is scoped to the host, never the whole tool
    // (the policy matcher understands the `domain:` form).
    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let url = input
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let host = web_fetch_hostname(url);
        let title = match &host {
            Some(host) => format!("Fetch from {host}"),
            None => "Fetch a URL".to_string(),
        };
        let message = if url.is_empty() {
            "WebFetch wants to fetch a URL".to_string()
        } else {
            format!("WebFetch wants to fetch: {url}")
        };
        Ok(PermissionDecision::ask(
            PermissionRequest::new(title, message).with_options([
                "allow_once",
                "allow_always",
                "reject_once",
            ]),
            Some(input.clone()),
        ))
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match parse_web_fetch_input(input) {
            Ok(_) => Ok(ValidationOutcome::valid()),
            Err(reason) => Ok(ValidationOutcome::invalid(reason, 400)),
        }
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        // Kernel web seat: same routing contract as WebSearch — a
        // configured plugin provider owns the call and fails loudly; None
        // runs the builtin retrieval below unchanged.
        if let Some(router) = context.web_provider_router() {
            if let Some(output) = router.fetch(&input).await? {
                return Ok(output);
            }
        }
        let request = parse_web_fetch_input(&input).map_err(|reason| ToolError::InvalidInput {
            tool: self.id(),
            reason,
            error_code: Some(400),
        })?;
        fetch_readable(&request)
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err,
            })
    }
}

struct WebFetchRequest {
    url: reqwest::Url,
    max_chars: usize,
    offset: usize,
    raw: bool,
}

fn parse_web_fetch_input(input: &Value) -> Result<WebFetchRequest, String> {
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "url must be a non-empty string".to_string())?;
    let url = reqwest::Url::parse(url).map_err(|err| format!("invalid url: {err}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "url must use http or https; `{}` is not supported",
            url.scheme()
        ));
    }
    let max_chars = match input.get("max_chars") {
        None | Some(Value::Null) => DEFAULT_MAX_CHARS,
        Some(value) => value
            .as_u64()
            .filter(|value| (MIN_MAX_CHARS..=MAX_MAX_CHARS).contains(value))
            .ok_or_else(|| {
                format!("max_chars must be an integer between {MIN_MAX_CHARS} and {MAX_MAX_CHARS}")
            })?,
    };
    let offset = match input.get("offset") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "offset must be a non-negative integer".to_string())?,
    };
    let raw = match input.get("raw") {
        None | Some(Value::Null) => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "raw must be a boolean".to_string())?,
    };
    Ok(WebFetchRequest {
        url,
        max_chars: max_chars as usize,
        offset: offset as usize,
        raw,
    })
}

async fn fetch_readable(request: &WebFetchRequest) -> anyhow::Result<Value> {
    let client = http_client()?;
    let mut response = client
        .get(request.url.clone())
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|err| anyhow::anyhow!("request failed: {err}"))?;

    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if is_binary_content_type(&content_type) {
        anyhow::bail!(
            "unsupported content type `{content_type}` — WebFetch only returns text content"
        );
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| anyhow::anyhow!("failed to read response body: {err}"))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            body.extend_from_slice(&chunk[..MAX_BODY_BYTES - body.len()]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let text = decode_body(&body, &content_type);

    let (title, extracted) = if request.raw || !looks_like_html(&content_type, &text) {
        (None, text)
    } else {
        let base = reqwest::Url::parse(&final_url).unwrap_or_else(|_| request.url.clone());
        let page = extract_readable_text(&text, &base);
        (page.title, page.text)
    };

    let total_chars = extracted.chars().count();
    let (window, end, byte_truncated) = window_text(&extracted, request.offset, request.max_chars);
    let truncated = request.offset > 0 || end < total_chars;

    let mut result = json!({
        "url": request.url.to_string(),
        "status": status,
        "content_type": content_type,
        "content": window,
        "offset": request.offset,
        "total_chars": total_chars,
        "truncated": truncated,
    });
    if final_url != request.url.as_str() {
        result["final_url"] = Value::String(final_url);
    }
    if let Some(title) = title {
        result["title"] = Value::String(title);
    }
    if end < total_chars {
        result["next_offset"] = Value::from(end);
    }
    if total_chars < 200 && !request.raw {
        result["note"] = Value::String(
            "Little extractable text; the page may require JavaScript rendering.".to_string(),
        );
    }
    if byte_truncated {
        result["truncation"] = Value::String(format!(
            "Output truncated: exceeded {} KiB byte budget. Returned characters {}-{} of {}. Call WebFetch again with offset={} to continue.",
            WEB_FETCH_MAX_OUTPUT_BYTES / 1024,
            request.offset,
            end,
            total_chars,
            end,
        ));
    }
    Ok(result)
}

fn window_text(text: &str, offset: usize, max_chars: usize) -> (String, usize, bool) {
    let mut window = String::new();
    let mut bytes = 0usize;
    let mut consumed = 0usize;
    let mut byte_truncated = false;

    for ch in text.chars().skip(offset).take(max_chars) {
        let next_bytes = bytes + ch.len_utf8();
        if next_bytes > WEB_FETCH_MAX_OUTPUT_BYTES {
            byte_truncated = true;
            break;
        }
        window.push(ch);
        bytes = next_bytes;
        consumed += 1;
    }

    (window, offset.saturating_add(consumed), byte_truncated)
}

fn is_binary_content_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if essence.is_empty() {
        return false;
    }
    let texty = essence.starts_with("text/")
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
        || matches!(
            essence.as_str(),
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-www-form-urlencoded"
        );
    !texty
}

fn decode_body(body: &[u8], content_type: &str) -> String {
    let encoding = charset_from_content_type(content_type)
        .or_else(|| sniff_meta_charset(body))
        .unwrap_or(encoding_rs::UTF_8);
    let (decoded, _, _) = encoding.decode(body);
    decoded.into_owned()
}

/// Read the `charset` parameter off a `Content-Type` header. Parameter
/// names are case-insensitive per RFC 9110, so `Charset=GBK` counts.
fn charset_from_content_type(content_type: &str) -> Option<&'static encoding_rs::Encoding> {
    content_type
        .split(';')
        .filter_map(|part| {
            let (name, value) = part.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches('"'))
        })
        .find_map(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
}

/// Bytes of the document scanned for a `<meta>` charset declaration.
/// HTML requires the declaration inside the first 1024 bytes, which is
/// also all a browser sniffs.
const MAX_META_SNIFF_BYTES: usize = 1024;

/// Recover the encoding from an in-document `<meta charset=…>` or
/// `<meta http-equiv="Content-Type" content="…; charset=…">`.
///
/// Plenty of older Chinese sites send GBK/GB2312 with no charset on the
/// header at all; decoding those as UTF-8 yields a page of U+FFFD with
/// no error to signal it. Only `<meta>` tags are considered so a
/// `charset` mentioned in inline script can't redirect the decode.
fn sniff_meta_charset(body: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = &body[..MAX_META_SNIFF_BYTES.min(body.len())];
    // The declaration is ASCII; lossy decoding only mangles bytes we
    // are not looking at.
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    head.split("<meta")
        .skip(1)
        .filter_map(|tag| tag.split('>').next())
        .find_map(charset_from_meta_tag)
}

fn charset_from_meta_tag(tag: &str) -> Option<&'static encoding_rs::Encoding> {
    let mut rest = tag;
    while let Some(index) = rest.find("charset") {
        rest = &rest[index + "charset".len()..];
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let label = value
            .trim_start()
            .trim_start_matches(['"', '\''])
            .split([' ', '\t', '\r', '\n', '"', '\'', ';', '/'])
            .next()
            .unwrap_or_default();
        if let Some(encoding) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            return Some(encoding);
        }
    }
    None
}

fn looks_like_html(content_type: &str, body: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if essence.contains("html") {
        return true;
    }
    if !essence.is_empty() && essence != "application/octet-stream" {
        return false;
    }
    let head = body.trim_start().get(..256.min(body.trim_start().len()));
    head.map(|head| {
        let lower = head.to_ascii_lowercase();
        lower.starts_with("<!doctype html") || lower.starts_with("<html")
    })
    .unwrap_or(false)
}

pub(crate) struct ReadablePage {
    pub(crate) title: Option<String>,
    pub(crate) text: String,
}

/// Reduce an HTML document to readable text. Prefers the `<main>` /
/// `<article>` landmark when present, skips chrome (nav, header,
/// footer, aside) and non-content elements, and keeps light Markdown
/// structure so the model sees headings, lists, code, and links.
pub(crate) fn extract_readable_text(html: &str, base: &reqwest::Url) -> ReadablePage {
    let document = Html::parse_document(html);
    let title = Selector::parse("title")
        .ok()
        .and_then(|selector| {
            document
                .select(&selector)
                .next()
                .map(|element| collapse_inline(&element.text().collect::<String>()))
        })
        .filter(|title| !title.is_empty());

    let mut out = String::new();
    let container = ["main", "article", "[role='main']", "body"]
        .iter()
        .filter_map(|candidate| Selector::parse(candidate).ok())
        .find_map(|selector| document.select(&selector).next());
    match container {
        Some(element) => {
            for child in element.children() {
                walk(child, base, &mut out, 0, 0);
            }
        }
        None => {
            for child in document.tree.root().children() {
                walk(child, base, &mut out, 0, 0);
            }
        }
    }

    ReadablePage {
        title,
        text: tidy(&out),
    }
}

const SKIPPED_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed",
    "form", "button", "input", "select", "textarea", "nav", "header", "footer", "aside", "head",
    "dialog", "video", "audio", "img", "picture", "source",
];

/// Cap on how deep the walker will recurse into the document tree.
///
/// Nesting is unbounded on the wire: twenty thousand nested `<div>`s
/// fit in 120 KB, far inside [`MAX_BODY_BYTES`], and each level costs a
/// stack frame on a 2 MiB tokio worker thread — deep enough to overflow
/// the stack and abort the whole process, not just the fetch. Content
/// below this depth is dropped instead; real documents nest an order of
/// magnitude shallower.
const MAX_DOM_DEPTH: usize = 512;

fn walk(
    node: ego_tree::NodeRef<'_, Node>,
    base: &reqwest::Url,
    out: &mut String,
    pre_depth: usize,
    depth: usize,
) {
    if depth > MAX_DOM_DEPTH {
        return;
    }
    let depth = depth + 1;
    match node.value() {
        Node::Text(text) => {
            if pre_depth > 0 {
                out.push_str(&text);
            } else {
                push_inline_text(out, &text);
            }
        }
        Node::Element(element) => {
            let name = element.name();
            if SKIPPED_ELEMENTS.contains(&name)
                || element.attr("aria-hidden") == Some("true")
                || element.attr("hidden").is_some()
            {
                return;
            }
            match name {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = name[1..].parse::<usize>().unwrap_or(1);
                    ensure_block_break(out);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                    for child in node.children() {
                        walk(child, base, out, pre_depth, depth);
                    }
                    ensure_block_break(out);
                }
                "p" | "blockquote" | "ul" | "ol" | "table" | "figcaption" => {
                    ensure_block_break(out);
                    for child in node.children() {
                        walk(child, base, out, pre_depth, depth);
                    }
                    ensure_block_break(out);
                }
                "pre" => {
                    ensure_block_break(out);
                    out.push_str("```\n");
                    for child in node.children() {
                        walk(child, base, out, pre_depth + 1, depth);
                    }
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("```");
                    ensure_block_break(out);
                }
                "li" => {
                    ensure_line_break(out);
                    out.push_str("- ");
                    for child in node.children() {
                        walk(child, base, out, pre_depth, depth);
                    }
                    ensure_line_break(out);
                }
                "tr" => {
                    ensure_line_break(out);
                    for child in node.children() {
                        walk(child, base, out, pre_depth, depth);
                    }
                    ensure_line_break(out);
                }
                "td" | "th" => {
                    for child in node.children() {
                        walk(child, base, out, pre_depth, depth);
                    }
                    out.push_str(" | ");
                }
                "br" => ensure_line_break(out),
                "a" => {
                    let mut inner = String::new();
                    for child in node.children() {
                        walk(child, base, &mut inner, pre_depth, depth);
                    }
                    let inner = collapse_inline(&inner);
                    if inner.is_empty() {
                        return;
                    }
                    let href = element
                        .attr("href")
                        .and_then(|href| base.join(href).ok())
                        .filter(|resolved| matches!(resolved.scheme(), "http" | "https"));
                    match href {
                        Some(resolved) if pre_depth == 0 => {
                            push_inline_text(out, &format!("[{inner}]({resolved})"));
                        }
                        _ => push_inline_text(out, &inner),
                    }
                }
                _ => {
                    let block = matches!(name, "div" | "section" | "article" | "main");
                    if block {
                        ensure_line_break(out);
                    }
                    for child in node.children() {
                        walk(child, base, out, pre_depth, depth);
                    }
                    if block {
                        ensure_line_break(out);
                    }
                }
            }
        }
        _ => {
            for child in node.children() {
                walk(child, base, out, pre_depth, depth);
            }
        }
    }
}

fn push_inline_text(out: &mut String, text: &str) {
    let mut first = true;
    for word in text.split_whitespace() {
        if first {
            let needs_space = !out.is_empty()
                && !out.ends_with([' ', '\n'])
                && !out.ends_with("- ")
                && !out.ends_with("# ");
            if needs_space {
                out.push(' ');
            }
            first = false;
        } else {
            out.push(' ');
        }
        out.push_str(word);
    }
}

fn ensure_block_break(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return;
    }
    while !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn ensure_line_break(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() || out.ends_with('\n') {
        return;
    }
    out.push('\n');
}

fn collapse_inline(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Trim line ends and collapse runs of blank lines so block-break
/// bookkeeping from the walker never surfaces as triple newlines.
fn tidy(text: &str) -> String {
    let mut lines = Vec::new();
    let mut blank_run = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        lines.push(line.to_string());
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> reqwest::Url {
        reqwest::Url::parse("https://example.com/docs/page").unwrap()
    }

    #[test]
    fn extraction_prefers_main_and_skips_chrome() {
        let html = r#"<!doctype html><html><head>
            <title>  Docs —  Guide </title>
            <script>var tracking = 1;</script>
            <style>body { color: red; }</style>
        </head><body>
            <nav><a href="/">Home</a><a href="/about">About</a></nav>
            <header>Site header junk</header>
            <main>
                <h1>Getting <em>Started</em></h1>
                <p>Install the CLI, then read the <a href="/docs/config">config guide</a>.</p>
                <ul><li>First step</li><li>Second step</li></ul>
                <pre><code>cargo install rebon</code></pre>
            </main>
            <footer>Copyright junk</footer>
        </body></html>"#;

        let page = extract_readable_text(html, &base());
        assert_eq!(page.title.as_deref(), Some("Docs — Guide"));
        let text = page.text;
        assert!(text.contains("# Getting Started"), "text was: {text}");
        assert!(text.contains("[config guide](https://example.com/docs/config)"));
        assert!(text.contains("- First step\n- Second step"));
        assert!(text.contains("```\ncargo install rebon\n```"));
        assert!(!text.contains("tracking"));
        assert!(!text.contains("Site header junk"));
        assert!(!text.contains("Copyright junk"));
        assert!(!text.contains("About"));
    }

    #[test]
    fn extraction_falls_back_to_body_without_landmarks() {
        let html = "<html><body><div><p>Just a paragraph.</p></div></body></html>";
        let page = extract_readable_text(html, &base());
        assert_eq!(page.text, "Just a paragraph.");
        assert_eq!(page.title, None);
    }

    #[test]
    fn extraction_survives_pathologically_deep_nesting() {
        // Twenty thousand nested divs are ~120 KB of HTML — well inside
        // MAX_BODY_BYTES, and one stack frame per level would overflow
        // the 2 MiB worker thread and abort the whole process rather
        // than failing the fetch. Everything below MAX_DOM_DEPTH is
        // dropped instead.
        let depth = 20_000;
        let mut html = String::from("<html><body><main><p>Shallow paragraph.</p>");
        html.push_str(&"<div>".repeat(depth));
        html.push_str("Buried text.");
        html.push_str(&"</div>".repeat(depth));
        html.push_str("</main></body></html>");

        let page = extract_readable_text(&html, &base());
        assert!(
            page.text.contains("Shallow paragraph."),
            "text was: {}",
            page.text
        );
        assert!(!page.text.contains("Buried text."));
    }

    #[test]
    fn input_parsing_enforces_scheme_and_ranges() {
        assert!(parse_web_fetch_input(&json!({"url": "https://example.com"})).is_ok());
        assert!(parse_web_fetch_input(&json!({"url": "ftp://example.com"})).is_err());
        assert!(parse_web_fetch_input(&json!({"url": "not a url"})).is_err());
        assert!(parse_web_fetch_input(&json!({"url": ""})).is_err());
        assert!(
            parse_web_fetch_input(&json!({"url": "https://example.com", "max_chars": 10})).is_err()
        );
        let parsed = parse_web_fetch_input(
            &json!({"url": "https://example.com", "max_chars": 5000, "offset": 100, "raw": true}),
        )
        .unwrap();
        assert_eq!(parsed.max_chars, 5000);
        assert_eq!(parsed.offset, 100);
        assert!(parsed.raw);
    }

    #[test]
    fn text_window_respects_byte_budget_and_utf8_boundaries() {
        let text = "中文".repeat(20_000);
        let (window, end, byte_truncated) = window_text(&text, 0, 100_000);

        assert!(byte_truncated);
        assert!(window.len() <= WEB_FETCH_MAX_OUTPUT_BYTES);
        assert_eq!(end, window.chars().count());
        assert_eq!(window, text[..window.len()].to_string());
    }

    #[test]
    fn text_window_preserves_character_paging() {
        let text = "0123456789".repeat(500);
        let (window, end, byte_truncated) = window_text(&text, 17, 25);

        assert!(!byte_truncated);
        assert_eq!(window, text.chars().skip(17).take(25).collect::<String>());
        assert_eq!(end, 42);
    }

    #[test]
    fn content_type_classification() {
        assert!(!is_binary_content_type("text/html; charset=utf-8"));
        assert!(!is_binary_content_type("application/json"));
        assert!(!is_binary_content_type("application/ld+json"));
        assert!(!is_binary_content_type(""));
        assert!(is_binary_content_type("image/png"));
        assert!(is_binary_content_type("application/pdf"));
        assert!(is_binary_content_type("application/octet-stream"));

        assert!(looks_like_html("text/html", "{}"));
        assert!(!looks_like_html("application/json", "{}"));
        assert!(looks_like_html("", "  <!DOCTYPE html><html>"));
    }

    #[test]
    fn body_decoding_honors_charset_header() {
        // "中文" in GBK.
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
        assert_eq!(decode_body(&gbk, "text/html; charset=gbk"), "中文");
        assert_eq!(decode_body("中文".as_bytes(), "text/html"), "中文");
        // Parameter names are case-insensitive, and the value may be quoted.
        assert_eq!(decode_body(&gbk, "text/html; Charset=GBK"), "中文");
        assert_eq!(decode_body(&gbk, "text/html;CHARSET=\"gbk\""), "中文");
    }

    #[test]
    fn body_decoding_sniffs_meta_charset_when_the_header_omits_it() {
        // Old Chinese sites routinely serve GBK with no charset on the
        // header; without the sniff the whole page decodes to U+FFFD.
        let (declared, _, _) = encoding_rs::GBK.encode(
            "<html><head><meta charset=\"gb2312\"><title>中文标题</title></head>\
             <body><main><p>正文内容</p></main></body></html>",
        );
        let decoded = decode_body(&declared, "text/html");
        assert!(decoded.contains("中文标题"), "{decoded}");
        assert!(decoded.contains("正文内容"), "{decoded}");

        let (http_equiv, _, _) = encoding_rs::GBK.encode(
            "<html><head><meta http-equiv=\"Content-Type\" \
             content=\"text/html; charset=GBK\"></head><body><p>正文内容</p></body></html>",
        );
        assert!(decode_body(&http_equiv, "").contains("正文内容"));

        // The header still wins when it declares a charset.
        let utf8 = "<meta charset=\"gbk\">正文内容".as_bytes();
        assert!(decode_body(utf8, "text/html; charset=utf-8").contains("正文内容"));

        // A `charset` outside a <meta> tag must not redirect the decode.
        let script = "<script>var charset = 'gbk';</script>正文内容".as_bytes();
        assert!(decode_body(script, "text/html").contains("正文内容"));
    }

    // Network smoke test, excluded from CI. Run manually with:
    // `cargo test -p rebon-plugin-web --lib -- --ignored --nocapture live_web_fetch`
    #[tokio::test]
    #[ignore = "hits a live endpoint"]
    async fn live_web_fetch_reads_example_domain() {
        let request = parse_web_fetch_input(&json!({"url": "https://example.com/"})).unwrap();
        let result = fetch_readable(&request).await.expect("fetch failed");
        println!("{result:#}");
        assert_eq!(result["status"], 200);
        assert!(result["content"]
            .as_str()
            .unwrap()
            .contains("Example Domain"));
    }
}
