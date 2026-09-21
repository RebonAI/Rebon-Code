//! Frame decoder for the ACP stdio transport.
//!
//! ACP supports two wire framings over stdio and auto-detects which one the
//! peer is using:
//!
//! 1. **Content-Length header framing** (LSP / MCP style):
//!
//!    ```text
//!    Content-Length: <N>\r\n\r\n<N bytes of body>
//!    ```
//!
//! 2. **Newline-delimited JSON** (NDJSON): one JSON value per line.
//!
//! Detection looks at the first non-whitespace byte in the inbound stream.
//! `{` means NDJSON, anything else is treated as Content-Length.
//!
//! The decoder is pure and synchronous — feed it bytes with [`FrameDecoder::push`]
//! and pull complete message bodies out with [`FrameDecoder::next_message`].
//! It intentionally does *not* parse JSON; that is the job of the layer above.
//! Returning the raw body as bytes means callers can surface parse errors
//! themselves as `-32700 Parse error` responses.

use std::fmt;

/// Header block terminator for Content-Length framing.
const HEADER_DELIMITER: &[u8] = b"\r\n\r\n";

/// The header that precedes a body of `len` bytes under Content-Length
/// framing.
///
/// One definition of the format, because more than one writer emits it; a
/// frame header spelled two different ways would only show up as a bug
/// between peers of different vintages.
pub fn content_length_header(len: usize) -> String {
    format!("Content-Length: {len}\r\n\r\n")
}

/// Wire framing used on the stdio connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingMode {
    /// Framing not yet detected. The decoder will pick one as soon as the
    /// first non-whitespace byte arrives.
    Auto,
    /// Newline-delimited JSON (one value per line).
    Ndjson,
    /// `Content-Length: N\r\n\r\n<body>` framing.
    ContentLength,
}

impl fmt::Display for FramingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FramingMode::Auto => f.write_str("auto"),
            FramingMode::Ndjson => f.write_str("ndjson"),
            FramingMode::ContentLength => f.write_str("content-length"),
        }
    }
}

/// Outcome of a single [`FrameDecoder::next_message`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameDecodeStep {
    /// A complete message body is available. The bytes are the raw JSON-RPC
    /// payload — they have not yet been parsed.
    Message(Vec<u8>),
    /// More bytes are needed before another message can be produced.
    NeedMore,
}

/// Incremental frame decoder.
///
/// Feed it bytes with [`push`](FrameDecoder::push), then repeatedly call
/// [`next_message`](FrameDecoder::next_message) until it returns
/// [`FrameDecodeStep::NeedMore`].
pub struct FrameDecoder {
    buffer: Vec<u8>,
    framing: FramingMode,
}

impl FrameDecoder {
    /// Create a decoder that auto-detects its framing mode.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            framing: FramingMode::Auto,
        }
    }

    /// Create a decoder locked to a specific framing mode (skipping detection).
    pub fn with_framing(framing: FramingMode) -> Self {
        Self {
            buffer: Vec::new(),
            framing,
        }
    }

    /// The framing mode currently in effect. Returns [`FramingMode::Auto`]
    /// until enough bytes have arrived to detect.
    pub fn framing(&self) -> FramingMode {
        self.framing
    }

    /// Feed bytes read from the peer into the decoder.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// Try to pop the next complete message from the buffer.
    ///
    /// Safe to call repeatedly: keep calling until [`FrameDecodeStep::NeedMore`]
    /// is returned, then pull more bytes and call again.
    pub fn next_message(&mut self) -> FrameDecodeStep {
        if self.framing == FramingMode::Auto {
            match first_non_ws(&self.buffer) {
                None => return FrameDecodeStep::NeedMore,
                Some(b'{') => self.framing = FramingMode::Ndjson,
                Some(_) => self.framing = FramingMode::ContentLength,
            }
        }

        match self.framing {
            FramingMode::Ndjson => self.decode_ndjson(),
            FramingMode::ContentLength => self.decode_content_length(),
            // SAFETY: Auto is resolved above before we reach this match.
            FramingMode::Auto => unreachable!(),
        }
    }

    fn decode_ndjson(&mut self) -> FrameDecodeStep {
        loop {
            let Some(idx) = self.buffer.iter().position(|&b| b == b'\n') else {
                return FrameDecodeStep::NeedMore;
            };

            // Drain `[0..=idx]` (line + newline) from the buffer but only
            // collect the first `idx` bytes (everything before the '\n').
            // `Drain::drop` still removes the full range regardless of how
            // many items the iterator yielded, so the newline disappears.
            let line: Vec<u8> = self.buffer.drain(..=idx).take(idx).collect();
            let trimmed = line.trim_ascii();
            if !trimmed.is_empty() {
                return FrameDecodeStep::Message(trimmed.to_vec());
            }
            // Blank line — keep looping until we find content or run out.
        }
    }

    fn decode_content_length(&mut self) -> FrameDecodeStep {
        loop {
            let Some(delim_idx) = find_subslice(&self.buffer, HEADER_DELIMITER) else {
                return FrameDecodeStep::NeedMore;
            };

            let header_section = &self.buffer[..delim_idx];
            let Some(content_length) = parse_content_length(header_section) else {
                // Malformed header — skip past delimiter and retry so later
                // complete frames can still be decoded.
                self.buffer.drain(..delim_idx + HEADER_DELIMITER.len());
                continue;
            };

            let body_start = delim_idx + HEADER_DELIMITER.len();
            let total_needed = body_start + content_length;
            if self.buffer.len() < total_needed {
                return FrameDecodeStep::NeedMore;
            }

            let body = self.buffer[body_start..total_needed].to_vec();
            self.buffer.drain(..total_needed);
            return FrameDecodeStep::Message(body);
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn first_non_ws(buf: &[u8]) -> Option<u8> {
    buf.iter().copied().find(|b| !b.is_ascii_whitespace())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(header_section: &[u8]) -> Option<usize> {
    // HTTP-style headers are ASCII. If the bytes aren't valid UTF-8 we treat
    // the whole section as malformed and let the caller skip it.
    let s = std::str::from_utf8(header_section).ok()?;
    for line in s.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                return value.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_cl(body: &str) -> Vec<u8> {
        let mut v = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        v.extend_from_slice(body.as_bytes());
        v
    }

    fn take_message(d: &mut FrameDecoder) -> Vec<u8> {
        match d.next_message() {
            FrameDecodeStep::Message(b) => b,
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[test]
    fn auto_detects_ndjson_from_opening_brace() {
        let mut d = FrameDecoder::new();
        d.push(b"{\"jsonrpc\":\"2.0\"}\n");
        let msg = d.next_message();
        assert_eq!(d.framing(), FramingMode::Ndjson);
        assert_eq!(
            msg,
            FrameDecodeStep::Message(b"{\"jsonrpc\":\"2.0\"}".to_vec())
        );
    }

    #[test]
    fn auto_detects_content_length_from_header() {
        let mut d = FrameDecoder::new();
        let frame = frame_cl(r#"{"jsonrpc":"2.0"}"#);
        d.push(&frame);
        let msg = d.next_message();
        assert_eq!(d.framing(), FramingMode::ContentLength);
        assert_eq!(
            msg,
            FrameDecodeStep::Message(br#"{"jsonrpc":"2.0"}"#.to_vec())
        );
    }

    #[test]
    fn single_content_length_message() {
        let mut d = FrameDecoder::new();
        let frame = frame_cl(r#"{"a":1}"#);
        d.push(&frame);
        assert_eq!(take_message(&mut d), br#"{"a":1}"#.to_vec());
        assert_eq!(d.next_message(), FrameDecodeStep::NeedMore);
    }

    #[test]
    fn multiple_sequential_content_length_messages() {
        let mut d = FrameDecoder::new();
        let mut frames = frame_cl(r#"{"a":1}"#);
        frames.extend(frame_cl(r#"{"a":2}"#));
        frames.extend(frame_cl(r#"{"a":3}"#));
        d.push(&frames);

        assert_eq!(take_message(&mut d), br#"{"a":1}"#.to_vec());
        assert_eq!(take_message(&mut d), br#"{"a":2}"#.to_vec());
        assert_eq!(take_message(&mut d), br#"{"a":3}"#.to_vec());
        assert_eq!(d.next_message(), FrameDecodeStep::NeedMore);
    }

    #[test]
    fn content_length_waits_for_full_body() {
        let mut d = FrameDecoder::new();
        let frame = frame_cl(r#"{"big":"payload"}"#);
        // Feed header + partial body
        d.push(&frame[..frame.len() - 3]);
        assert_eq!(d.next_message(), FrameDecodeStep::NeedMore);
        // Now deliver the rest
        d.push(&frame[frame.len() - 3..]);
        assert_eq!(take_message(&mut d), br#"{"big":"payload"}"#.to_vec());
    }

    #[test]
    fn content_length_feeds_byte_by_byte() {
        let mut d = FrameDecoder::new();
        let frame = frame_cl(r#"{"a":1}"#);
        for byte in &frame[..frame.len() - 1] {
            d.push(&[*byte]);
            assert_eq!(d.next_message(), FrameDecodeStep::NeedMore);
        }
        d.push(&frame[frame.len() - 1..]);
        assert_eq!(take_message(&mut d), br#"{"a":1}"#.to_vec());
    }

    #[test]
    fn single_ndjson_message() {
        let mut d = FrameDecoder::new();
        d.push(b"{\"x\":1}\n");
        assert_eq!(take_message(&mut d), b"{\"x\":1}".to_vec());
        assert_eq!(d.next_message(), FrameDecodeStep::NeedMore);
    }

    #[test]
    fn multiple_sequential_ndjson_messages() {
        let mut d = FrameDecoder::new();
        d.push(b"{\"x\":1}\n{\"x\":2}\n{\"x\":3}\n");
        let mut got = Vec::new();
        while let FrameDecodeStep::Message(body) = d.next_message() {
            got.push(String::from_utf8(body).unwrap());
        }
        assert_eq!(
            got,
            vec![
                "{\"x\":1}".to_string(),
                "{\"x\":2}".to_string(),
                "{\"x\":3}".to_string(),
            ]
        );
        assert_eq!(d.next_message(), FrameDecodeStep::NeedMore);
    }

    #[test]
    fn ndjson_strips_crlf() {
        let mut d = FrameDecoder::with_framing(FramingMode::Ndjson);
        d.push(b"{\"x\":1}\r\n");
        assert_eq!(take_message(&mut d), b"{\"x\":1}".to_vec());
    }

    #[test]
    fn ndjson_skips_blank_lines() {
        // Auto-detection keys off the first non-ws byte, which is '{'.
        let mut d = FrameDecoder::new();
        d.push(b"   \n{\"x\":1}\n");
        assert_eq!(d.framing(), FramingMode::Auto);
        assert_eq!(take_message(&mut d), b"{\"x\":1}".to_vec());
        assert_eq!(d.framing(), FramingMode::Ndjson);
    }

    #[test]
    fn malformed_content_length_header_is_skipped() {
        let mut d = FrameDecoder::with_framing(FramingMode::ContentLength);
        // Header section without a Content-Length line, followed by a real
        // frame. The decoder should drop the bad block and surface the good
        // one without asking for more data.
        let mut buf = b"X-Garbage: 1\r\n\r\n".to_vec();
        buf.extend(frame_cl(r#"{"a":1}"#));
        d.push(&buf);
        assert_eq!(take_message(&mut d), br#"{"a":1}"#.to_vec());
        assert_eq!(d.next_message(), FrameDecodeStep::NeedMore);
    }

    #[test]
    fn content_length_preserves_malformed_json_body_bytes() {
        // The decoder is JSON-agnostic; it must still surface a non-JSON body
        // so the upper layer can emit a Parse error response.
        let mut d = FrameDecoder::new();
        let frame = frame_cl("not-json");
        d.push(&frame);
        assert_eq!(take_message(&mut d), b"not-json".to_vec());
    }

    #[test]
    fn ndjson_preserves_malformed_json_line_bytes() {
        let mut d = FrameDecoder::new();
        d.push(b"{oops\n");
        assert_eq!(take_message(&mut d), b"{oops".to_vec());
    }

    #[test]
    fn content_length_header_is_case_insensitive() {
        let mut d = FrameDecoder::with_framing(FramingMode::ContentLength);
        let body = r#"{"y":2}"#;
        let frame = format!("content-LENGTH:   {}\r\n\r\n{}", body.len(), body);
        d.push(frame.as_bytes());
        assert_eq!(take_message(&mut d), body.as_bytes().to_vec());
    }
}
