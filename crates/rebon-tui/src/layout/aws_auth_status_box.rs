//! Cloud-authentication status box: visibility + content projection.
//!
//! [`project_aws_auth_status`] turns an [`AwsAuthStatus`]
//! (`is_authenticating` / `error` / `output`) into a show/hide decision
//! plus content:
//!
//! 1. If auth is idle, there is no error, and `output` is empty → `None`.
//! 2. If auth is idle and there is no error → `None` (success state).
//! 3. Otherwise → show a round-bordered box with the `"Cloud
//!    Authentication"` header, the last 5 output lines (each line
//!    dim, with any `https?://...` URL split into before/link/after),
//!    and the error row if present.
//!
//! The URL rule is equivalent to the regex `/https?:\/\/\S+/` — one
//! match, the first URL on the line.

/// The literal header row text.
pub const HEADER_TEXT: &str = "Cloud Authentication";

/// How many output lines are shown from the tail.
pub const OUTPUT_TAIL_LENGTH: usize = 5;

/// Input state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AwsAuthStatus {
    pub is_authenticating: bool,
    pub error: Option<String>,
    /// Lines of authentication output. The box shows the
    /// last 5 only.
    pub output: Vec<String>,
}

/// A single rendered output line, with an optional URL split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    /// Text before the URL (or the whole line if no URL).
    pub before: String,
    /// The detected URL, if any. When `None`, `before` holds the whole
    /// line and `after` is empty.
    pub url: Option<String>,
    /// Text after the URL (empty if no URL).
    pub after: String,
}

impl OutputLine {
    /// Plain line, no URL.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            before: text.into(),
            url: None,
            after: String::new(),
        }
    }
}

/// The visible box display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsAuthBoxDisplay {
    pub header: &'static str,
    /// Tail-5 output lines. Each line is already split around any URL.
    /// Empty when `status.output` is empty.
    pub output_lines: Vec<OutputLine>,
    pub error: Option<String>,
}

/// Decision projection. Returns `None` when the box should be hidden.
///
/// Two guards apply:
///
/// * `!is_authenticating && !error && output.is_empty()` → None
/// * `!is_authenticating && !error` → None (success: output present
///   but auth finished without error means "don't linger")
pub fn project_aws_auth_status(status: &AwsAuthStatus) -> Option<AwsAuthBoxDisplay> {
    // Guard 1: nothing to show at all.
    if !status.is_authenticating && status.error.is_none() && status.output.is_empty() {
        return None;
    }
    // Guard 2: authentication finished successfully. Note this is
    // reachable only when `output` is non-empty (guard 1 skips empty)
    // — the box is still hidden in this branch.
    if !status.is_authenticating && status.error.is_none() {
        return None;
    }

    let output_lines = tail(&status.output, OUTPUT_TAIL_LENGTH)
        .iter()
        .map(|line| split_url(line))
        .collect();

    Some(AwsAuthBoxDisplay {
        header: HEADER_TEXT,
        output_lines,
        error: status.error.clone(),
    })
}

/// Takes the tail of `v` up to `n` elements — returns the whole array
/// when shorter than `n`.
pub fn tail<T: Clone>(v: &[T], n: usize) -> Vec<T> {
    if v.len() <= n {
        v.to_vec()
    } else {
        v[v.len() - n..].to_vec()
    }
}

/// Split a line around the FIRST `https?://\S+` URL. The URL is
/// defined as a protocol + zero-or-more non-whitespace chars.
pub fn split_url(line: &str) -> OutputLine {
    // Find `http://` or `https://` — first occurrence.
    let (proto_start, proto_len) = match (line.find("https://"), line.find("http://")) {
        (Some(s_pos), Some(p_pos)) => {
            if s_pos <= p_pos {
                (s_pos, "https://".len())
            } else {
                (p_pos, "http://".len())
            }
        }
        (Some(s_pos), None) => (s_pos, "https://".len()),
        (None, Some(p_pos)) => (p_pos, "http://".len()),
        (None, None) => return OutputLine::plain(line.to_string()),
    };
    // The URL extends until the next whitespace or end-of-line.
    let rest = &line[proto_start + proto_len..];
    let end_offset = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let url_end = proto_start + proto_len + end_offset;
    // Guard: URL must have at least 1 char past the protocol, matching
    // a `\S+` (one-or-more) semantics — a bare `http://` + whitespace
    // wouldn't match. If we have zero body, treat as no URL.
    if end_offset == 0 {
        return OutputLine::plain(line.to_string());
    }
    OutputLine {
        before: line[..proto_start].to_string(),
        url: Some(line[proto_start..url_end].to_string()),
        after: line[url_end..].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> AwsAuthStatus {
        AwsAuthStatus::default()
    }

    // ---- visibility guards ----

    #[test]
    fn hidden_when_everything_empty() {
        assert_eq!(project_aws_auth_status(&st()), None);
    }

    #[test]
    fn hidden_when_success_with_output() {
        let mut s = st();
        s.output.push("done".into());
        // authenticating = false, error = None, output non-empty
        // the box is hidden anyway (guard 2)
        assert_eq!(project_aws_auth_status(&s), None);
    }

    #[test]
    fn visible_when_authenticating_with_output() {
        let mut s = st();
        s.is_authenticating = true;
        s.output.push("authorizing".into());
        let d = project_aws_auth_status(&s).unwrap();
        assert_eq!(d.header, HEADER_TEXT);
        assert_eq!(d.output_lines.len(), 1);
        assert_eq!(d.output_lines[0].before, "authorizing");
        assert_eq!(d.error, None);
    }

    #[test]
    fn visible_when_error_no_output() {
        let mut s = st();
        s.error = Some("permission denied".into());
        let d = project_aws_auth_status(&s).unwrap();
        assert_eq!(d.error.as_deref(), Some("permission denied"));
        assert!(d.output_lines.is_empty());
    }

    #[test]
    fn visible_when_authenticating_no_output() {
        let mut s = st();
        s.is_authenticating = true;
        let d = project_aws_auth_status(&s).unwrap();
        assert!(d.output_lines.is_empty());
        assert_eq!(d.error, None);
    }

    #[test]
    fn visible_when_error_and_output() {
        let mut s = st();
        s.error = Some("err".into());
        s.output.push("line1".into());
        let d = project_aws_auth_status(&s).unwrap();
        assert_eq!(d.output_lines.len(), 1);
        assert_eq!(d.error.as_deref(), Some("err"));
    }

    // ---- tail slice ----

    #[test]
    fn tail_slices_last_5() {
        let v: Vec<String> = (1..=10).map(|i| i.to_string()).collect();
        let t = tail(&v, OUTPUT_TAIL_LENGTH);
        assert_eq!(t, vec!["6", "7", "8", "9", "10"]);
    }

    #[test]
    fn tail_keeps_all_when_shorter() {
        let v: Vec<String> = vec!["a".into(), "b".into()];
        let t = tail(&v, OUTPUT_TAIL_LENGTH);
        assert_eq!(t, vec!["a", "b"]);
    }

    #[test]
    fn tail_empty() {
        let v: Vec<String> = vec![];
        let t = tail(&v, OUTPUT_TAIL_LENGTH);
        assert!(t.is_empty());
    }

    #[test]
    fn output_lines_respect_tail() {
        let mut s = st();
        s.is_authenticating = true;
        for i in 0..10 {
            s.output.push(format!("line{i}"));
        }
        let d = project_aws_auth_status(&s).unwrap();
        assert_eq!(d.output_lines.len(), 5);
        assert_eq!(d.output_lines[0].before, "line5");
        assert_eq!(d.output_lines[4].before, "line9");
    }

    // ---- url split ----

    #[test]
    fn split_url_no_url_returns_plain() {
        let l = split_url("nothing here");
        assert_eq!(l.before, "nothing here");
        assert_eq!(l.url, None);
        assert_eq!(l.after, "");
    }

    #[test]
    fn split_url_https() {
        let l = split_url("visit https://example.com/auth to continue");
        assert_eq!(l.before, "visit ");
        assert_eq!(l.url, Some("https://example.com/auth".into()));
        assert_eq!(l.after, " to continue");
    }

    #[test]
    fn split_url_http() {
        let l = split_url("go http://foo.bar then");
        assert_eq!(l.url, Some("http://foo.bar".into()));
    }

    #[test]
    fn split_url_at_start() {
        let l = split_url("https://x.com done");
        assert_eq!(l.before, "");
        assert_eq!(l.url, Some("https://x.com".into()));
        assert_eq!(l.after, " done");
    }

    #[test]
    fn split_url_at_end() {
        let l = split_url("url: https://x.com");
        assert_eq!(l.before, "url: ");
        assert_eq!(l.url, Some("https://x.com".into()));
        assert_eq!(l.after, "");
    }

    #[test]
    fn split_url_bare_protocol_with_whitespace_is_plain() {
        // `/https?:\/\/\S+/` requires at least one \S — bare `http:// ` fails.
        let l = split_url("try http:// later");
        assert_eq!(l.url, None);
        assert_eq!(l.before, "try http:// later");
    }

    #[test]
    fn split_url_first_match_wins() {
        // First occurrence — doesn't matter which is first in the input.
        let l = split_url("a https://x.com b https://y.com c");
        assert_eq!(l.url, Some("https://x.com".into()));
        // `after` contains the rest including the second URL
        assert!(l.after.contains("https://y.com"));
    }

    #[test]
    fn split_url_stops_at_tab() {
        // Tab is whitespace → URL ends there.
        let l = split_url("x https://a.com\tmore");
        assert_eq!(l.url, Some("https://a.com".into()));
        assert_eq!(l.after, "\tmore");
    }

    #[test]
    fn split_url_http_before_https_picks_http() {
        let l = split_url("http://a.com and https://b.com");
        assert_eq!(l.url, Some("http://a.com".into()));
    }

    #[test]
    fn split_url_https_before_http_picks_https() {
        let l = split_url("https://a.com and http://b.com");
        assert_eq!(l.url, Some("https://a.com".into()));
    }

    // ---- header text pinned ----

    #[test]
    fn header_text_pinned() {
        assert_eq!(HEADER_TEXT, "Cloud Authentication");
    }
}
