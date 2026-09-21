//! SSE line parser.
//!
//! All HTTP streaming providers read `text/event-stream` bodies.
//! The wire format is line-based: `event: <name>` + `data: <json>`
//! pairs separated by blank lines. Framing and byte-stream lifecycle
//! live here; provider-specific decoders only translate complete frames.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::{stream::unfold, Stream, StreamExt};

use crate::error::ModelResult;
use crate::events::{StreamEvent, StreamEventStream};

/// One parsed SSE frame. Only the `data` line is interesting to the
/// downstream decoder — `event:` is kept for tracing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    /// Optional `event:` line, empty string when absent.
    pub event: String,
    /// Joined `data:` lines.
    pub data: String,
}

impl SseFrame {
    /// Whether the frame has a non-empty data payload.
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }
}

/// Incremental SSE frame buffer. Feed raw bytes via [`Self::push`]
/// and pop completed frames via [`Self::pop_frame`].
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
    current: SseFrame,
    ready: Vec<SseFrame>,
}

impl SseParser {
    /// Create an empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a raw byte chunk into the parser. Invalid UTF-8 is
    /// replaced with `U+FFFD`.
    pub fn push(&mut self, chunk: &[u8]) {
        let text = String::from_utf8_lossy(chunk);
        self.buffer.push_str(&text);
        // Process any complete lines in the buffer.
        while let Some(nl) = self.buffer.find('\n') {
            let mut line = self.buffer[..nl].to_string();
            // Drop the newline and any trailing carriage return.
            self.buffer.drain(..=nl);
            if line.ends_with('\r') {
                line.pop();
            }
            self.handle_line(line);
        }
    }

    /// Pop the next completed frame, if any.
    pub fn pop_frame(&mut self) -> Option<SseFrame> {
        if self.ready.is_empty() {
            None
        } else {
            Some(self.ready.remove(0))
        }
    }

    /// Number of fully-parsed frames buffered for consumption.
    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }

    /// Flush at end-of-stream: parse any unterminated trailing line
    /// and promote the in-progress frame to the ready queue. Frames
    /// normally complete only on a blank line, so a gateway that
    /// closes the connection right after its final `data:` line
    /// (e.g. `data: [DONE]` with no trailing blank line) would
    /// otherwise leave that frame stuck in `current` forever and a
    /// completed stream would be misreported as truncated.
    pub fn flush_eof(&mut self) {
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.ends_with('\r') {
                line.pop();
            }
            self.handle_line(line);
        }
        if !self.current.event.is_empty() || !self.current.data.is_empty() {
            let frame = std::mem::take(&mut self.current);
            self.ready.push(frame);
        }
    }

    fn handle_line(&mut self, line: String) {
        if line.is_empty() {
            // End of frame: move `current` into the ready queue if
            // it carried any payload.
            if self.current.event.is_empty() && self.current.data.is_empty() {
                // Keep-alive / empty frame — drop silently.
                return;
            }
            let frame = std::mem::take(&mut self.current);
            self.ready.push(frame);
            return;
        }
        if let Some(value) = line.strip_prefix("event:") {
            self.current.event = value.trim_start().to_string();
            return;
        }
        if let Some(value) = line.strip_prefix("data:") {
            let value = value.trim_start();
            if !self.current.data.is_empty() {
                self.current.data.push('\n');
            }
            self.current.data.push_str(value);
            return;
        }
        // Ignore id:/retry:/comment/unknown lines.
    }
}

pub(crate) type ByteStream = Pin<Box<dyn Stream<Item = ModelResult<Bytes>> + Send>>;

/// Provider-specific half of an SSE stream.
///
/// The shared driver owns byte reads, incremental framing, EOF flushing, and
/// pending-event draining. Implementations only translate one complete frame
/// and decide whether their wire has reached a valid terminal event.
pub(crate) trait SseDecoder: Send + 'static {
    fn next_event(&mut self) -> Option<StreamEvent>;
    fn push_frame(&mut self, frame: SseFrame, at_eof: bool) -> ModelResult<()>;
    fn is_terminal(&self) -> bool;
    fn finish(&mut self) -> ModelResult<()>;
}

struct SseStreamState<D> {
    bytes: ByteStream,
    parser: SseParser,
    decoder: D,
    eof: bool,
    finished: bool,
}

/// Drive a provider decoder over an HTTP response byte stream.
///
/// This is the only HTTP/SSE read loop in the crate. In particular, EOF always
/// flushes a trailing unterminated frame before the decoder validates its
/// wire-specific terminal marker.
pub(crate) fn decode_sse_stream<D>(bytes: ByteStream, decoder: D) -> StreamEventStream
where
    D: SseDecoder,
{
    let state = SseStreamState {
        bytes,
        parser: SseParser::new(),
        decoder,
        eof: false,
        finished: false,
    };
    Box::pin(unfold(state, step_sse_stream::<D>))
}

async fn step_sse_stream<D>(
    mut state: SseStreamState<D>,
) -> Option<(ModelResult<StreamEvent>, SseStreamState<D>)>
where
    D: SseDecoder,
{
    loop {
        if let Some(event) = state.decoder.next_event() {
            return Some((Ok(event), state));
        }
        if state.finished {
            return None;
        }
        if let Some(frame) = state.parser.pop_frame() {
            if let Err(error) = state.decoder.push_frame(frame, state.eof) {
                state.finished = true;
                return Some((Err(error), state));
            }
            if state.decoder.is_terminal() {
                state.finished = true;
            }
            continue;
        }
        if state.decoder.is_terminal() {
            state.finished = true;
            continue;
        }
        if state.eof {
            state.finished = true;
            if let Err(error) = state.decoder.finish() {
                return Some((Err(error), state));
            }
            continue;
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => state.parser.push(&chunk),
            Some(Err(error)) => {
                state.finished = true;
                return Some((Err(error), state));
            }
            None => {
                state.eof = true;
                state.parser.flush_eof();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_single_frame() {
        let mut p = SseParser::new();
        p.push(b"event: message_start\ndata: {\"foo\":1}\n\n");
        let frame = p.pop_frame().unwrap();
        assert_eq!(frame.event, "message_start");
        assert_eq!(frame.data, "{\"foo\":1}");
        assert!(p.pop_frame().is_none());
    }

    #[test]
    fn parser_handles_crlf_newlines() {
        let mut p = SseParser::new();
        p.push(b"event: ping\r\ndata: hello\r\n\r\n");
        let frame = p.pop_frame().unwrap();
        assert_eq!(frame.event, "ping");
        assert_eq!(frame.data, "hello");
    }

    #[test]
    fn parser_joins_multiple_data_lines() {
        let mut p = SseParser::new();
        p.push(b"data: line1\ndata: line2\n\n");
        let frame = p.pop_frame().unwrap();
        assert_eq!(frame.data, "line1\nline2");
    }

    #[test]
    fn parser_handles_chunked_input() {
        let mut p = SseParser::new();
        p.push(b"event: partial\nda");
        assert!(p.pop_frame().is_none());
        p.push(b"ta: {\"a\":");
        assert!(p.pop_frame().is_none());
        p.push(b"1}\n\n");
        let frame = p.pop_frame().unwrap();
        assert_eq!(frame.event, "partial");
        assert_eq!(frame.data, "{\"a\":1}");
    }

    #[test]
    fn parser_drops_keepalive_frames() {
        let mut p = SseParser::new();
        p.push(b"\n\n");
        assert!(p.pop_frame().is_none());
    }

    #[test]
    fn parser_supports_data_without_event_line() {
        let mut p = SseParser::new();
        p.push(b"data: {\"type\":\"message_stop\"}\n\n");
        let frame = p.pop_frame().unwrap();
        assert_eq!(frame.event, "");
        assert_eq!(frame.data, "{\"type\":\"message_stop\"}");
    }

    #[test]
    fn parser_ignores_unknown_fields() {
        let mut p = SseParser::new();
        p.push(b"id: 7\nretry: 500\nevent: content_block_delta\ndata: {}\n\n");
        let frame = p.pop_frame().unwrap();
        assert_eq!(frame.event, "content_block_delta");
        assert_eq!(frame.data, "{}");
    }

    #[test]
    fn flush_eof_promotes_frame_missing_trailing_blank_line() {
        let mut p = SseParser::new();
        p.push(b"data: {\"x\":1}\n");
        assert!(p.pop_frame().is_none());
        p.flush_eof();
        assert_eq!(p.pop_frame().unwrap().data, "{\"x\":1}");
    }

    #[test]
    fn flush_eof_parses_trailing_line_without_newline() {
        let mut p = SseParser::new();
        p.push(b"data: [DONE]");
        assert!(p.pop_frame().is_none());
        p.flush_eof();
        assert_eq!(p.pop_frame().unwrap().data, "[DONE]");
    }

    #[test]
    fn flush_eof_is_noop_on_empty_parser() {
        let mut p = SseParser::new();
        p.flush_eof();
        assert!(p.pop_frame().is_none());
    }

    #[test]
    fn parser_buffers_multiple_frames() {
        let mut p = SseParser::new();
        p.push(b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n");
        assert_eq!(p.ready_count(), 2);
        assert_eq!(p.pop_frame().unwrap().data, "1");
        assert_eq!(p.pop_frame().unwrap().data, "2");
    }
}
