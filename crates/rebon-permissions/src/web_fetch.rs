//! Hostname resolution for WebFetch permission rules.
//!
//! A WebFetch rule is scoped to a host, so the host the rule authorizes has
//! to be the host the request actually contacts. That one resolution step is
//! all this module owns; the option list and the allow/reject reducers for
//! the WebFetch prompt live with the prompt, not here.

/// Host a WebFetch rule is scoped to, resolved with the same parser the
/// outbound request uses.
///
/// Splitting the authority by hand diverges from WHATWG parsing on inputs
/// a hostile page controls — `https://evil.com\@docs.rs/` reads as
/// `docs.rs` to a naive `rsplit('@')` while reqwest connects to
/// `evil.com` — which turns a `WebFetch(domain:docs.rs)` grant into a
/// grant for anywhere. Deferring to `url` keeps the authorized host and
/// the contacted host the same string.
pub fn web_fetch_hostname(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_extracts_and_lowercases_domain() {
        assert_eq!(
            web_fetch_hostname("HTTPS://user:pass@Docs.Example.com:443/path?q=1"),
            Some("docs.example.com".to_string())
        );
        assert_eq!(
            web_fetch_hostname("https://[2001:db8::1]:443/index.html"),
            Some("[2001:db8::1]".to_string())
        );
    }

    #[test]
    fn hostname_does_not_credit_the_host_behind_a_backslash() {
        // WHATWG treats `\` as an authority terminator for special
        // schemes, so this URL connects to evil.com. A rule scoped to
        // docs.rs must not authorize it.
        assert_eq!(
            web_fetch_hostname("https://evil.com\\@docs.rs/"),
            Some("evil.com".to_string())
        );
    }
}
