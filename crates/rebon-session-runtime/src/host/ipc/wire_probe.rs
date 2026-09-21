//! Which protocol did this connection open with?
//!
//! During the compatibility release a worker listens on one port and answers
//! two protocols: the legacy one-shot envelope every shipped client speaks,
//! and ACP JSON-RPC. They cannot be told apart per *message* — they are told
//! apart per *connection*, because their connection lifetimes differ. A legacy
//! client writes one compact JSON line, shuts down its write half, reads one
//! response and hangs up. An ACP client sends `initialize` and then keeps the
//! connection for as long as it has anything to say. So the decision has to be
//! made once, on the first frame, and it decides what the connection *is*.
//!
//! The rule, in the order it is applied:
//!
//! 1. **First non-whitespace byte is not `{`** — ACP with `Content-Length`
//!    header framing. Unambiguous: every legacy envelope is a JSON object, so
//!    nothing that starts with `C` was ever one of ours.
//! 2. **The first line decodes as an object carrying `jsonrpc`** — ACP over
//!    NDJSON. `jsonrpc` is the discriminator rather than `method`, because
//!    `method` is a name a future envelope field could plausibly take while
//!    `jsonrpc` is reserved by the JSON-RPC spec for exactly this.
//! 3. **Anything else** — legacy. Including a first line that is not valid
//!    JSON at all.
//!
//! Rule 3 is deliberately the fail-safe direction, and it is the same
//! direction the client half falls in: when the probe cannot tell, it decides
//! "not the new one". Getting it wrong that way costs a legacy-shaped error
//! response to a client that will reconnect; getting it wrong the other way
//! would hold a connection open speaking ACP at a peer that is waiting for one
//! line and will never send another.
//!
//! Nothing here consumes bytes it does not classify. Detecting rule 1 peeks
//! without consuming, so the ACP transport receives the header bytes it needs;
//! rules 2 and 3 hand the line they read back to the caller.

use std::io::{BufRead, Read};

use rebon_proto::framing::FramingMode;

/// What the first frame on a connection turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstFrame {
    /// Speak ACP on this connection and keep it open.
    Acp {
        /// Which of ACP's two wire framings the peer used.
        framing: FramingMode,
        /// The first message, already read off the stream, or empty when the
        /// framing is `ContentLength` and nothing was consumed.
        body: Vec<u8>,
    },
    /// Answer one legacy envelope and hang up, as every shipped client
    /// expects.
    Legacy {
        /// The line read, including whatever the caller must decode. Handed
        /// back rather than re-read, because it is already off the socket.
        line: String,
    },
    /// The peer connected and said nothing. Not an error: a health check that
    /// opens a socket to see whether anything is listening does exactly this.
    Closed,
}

/// The largest first line this will read before giving up.
///
/// A legacy `Reply` can carry base64 images, so the ceiling is generous — but
/// it is a ceiling, because without one a peer that opens a socket and writes
/// `{` forever holds a worker thread and the memory behind it. Sixty-four
/// mebibytes is far above the largest prompt anyone has sent and far below
/// anything that threatens a worker.
pub const MAX_FIRST_LINE_BYTES: u64 = 64 * 1024 * 1024;

/// Classify the first frame, consuming only what rules 2 and 3 read.
///
/// The reader keeps whatever was not consumed, so a `ContentLength` verdict
/// leaves the header where the ACP transport will look for it.
pub fn classify_first_frame(reader: &mut impl BufRead) -> std::io::Result<FirstFrame> {
    classify_first_frame_within(reader, MAX_FIRST_LINE_BYTES)
}

/// The same, with the ceiling given rather than assumed.
///
/// Exists so the bound can be tested without allocating the real one; every
/// caller outside this module's tests wants [`classify_first_frame`].
fn classify_first_frame_within(
    reader: &mut impl BufRead,
    ceiling: u64,
) -> std::io::Result<FirstFrame> {
    match peek_first_meaningful_byte(reader)? {
        None => Ok(FirstFrame::Closed),
        // Rule 1. Nothing that opens with a header was ever a legacy
        // envelope, so this needs no line read at all — and must not do one,
        // because the transport wants those bytes.
        Some(byte) if byte != b'{' => Ok(FirstFrame::Acp {
            framing: FramingMode::ContentLength,
            body: Vec::new(),
        }),
        Some(_) => {
            let mut line = String::new();
            // `&mut *reader` rather than `reader`: `take` consumes what it
            // wraps, and the caller still owns this stream.
            (&mut *reader).take(ceiling).read_line(&mut line)?;
            if line.trim().is_empty() {
                return Ok(FirstFrame::Closed);
            }
            // Rule 2, then rule 3. `get("jsonrpc")` and not a full decode:
            // classification must not depend on the message being *valid*
            // JSON-RPC, only on it claiming to be JSON-RPC. A malformed
            // `initialize` is a `-32700` for the ACP layer to answer, not a
            // reason to answer it as a legacy envelope.
            let claims_json_rpc = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|value| {
                    value
                        .as_object()
                        .map(|object| object.contains_key("jsonrpc"))
                })
                .unwrap_or(false);
            if claims_json_rpc {
                Ok(FirstFrame::Acp {
                    framing: FramingMode::Ndjson,
                    body: line.into_bytes(),
                })
            } else {
                Ok(FirstFrame::Legacy { line })
            }
        }
    }
}

/// The first byte that is not whitespace, without consuming it.
///
/// Leading whitespace is skipped rather than rejected because ACP's own
/// framing detector skips it, and a protocol that disagreed with the layer
/// underneath it about where a message starts would be a bug waiting for a
/// pretty-printer.
fn peek_first_meaningful_byte(reader: &mut impl BufRead) -> std::io::Result<Option<u8>> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        match available
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
        {
            Some(index) => {
                let found = available[index];
                reader.consume(index);
                return Ok(Some(found));
            }
            None => {
                let all = available.len();
                reader.consume(all);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(input: &str) -> FirstFrame {
        let mut reader = std::io::BufReader::new(input.as_bytes());
        classify_first_frame(&mut reader).expect("a slice reader cannot fail")
    }

    /// The shape every shipped client writes: one compact line, then a
    /// newline. It must still be read as legacy on the release that also
    /// speaks ACP, or the compatibility period does nothing.
    #[test]
    fn a_legacy_envelope_is_legacy() {
        let line = r#"{"protocolVersion":1,"jobId":"bg-1","token":"t","request":"ping"}"#;
        assert_eq!(
            classify(&format!("{line}\n")),
            FirstFrame::Legacy {
                line: format!("{line}\n")
            }
        );
    }

    /// A client that shut its write half without a trailing newline still gets
    /// classified: end of stream ends the line just as a newline does.
    #[test]
    fn a_legacy_envelope_without_a_trailing_newline_is_still_legacy() {
        let line = r#"{"protocolVersion":1,"jobId":"bg-1","token":"t","request":"ping"}"#;
        assert_eq!(
            classify(line),
            FirstFrame::Legacy {
                line: line.to_string()
            }
        );
    }

    #[test]
    fn a_json_rpc_line_is_acp_over_ndjson() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        assert_eq!(
            classify(&format!("{line}\n")),
            FirstFrame::Acp {
                framing: FramingMode::Ndjson,
                body: format!("{line}\n").into_bytes(),
            }
        );
    }

    /// `jsonrpc` decides, not `method`. An envelope that grew a field called
    /// `method` would otherwise start being answered as ACP, which is the
    /// failure this discriminator is chosen to avoid.
    #[test]
    fn a_method_field_alone_does_not_make_it_json_rpc() {
        let line = r#"{"protocolVersion":1,"token":"t","method":"whatever"}"#;
        assert_eq!(
            classify(&format!("{line}\n")),
            FirstFrame::Legacy {
                line: format!("{line}\n")
            }
        );
    }

    /// Header framing cannot be a legacy envelope, and the bytes must survive
    /// the verdict: the ACP transport reads the header itself.
    #[test]
    fn header_framing_is_acp_and_nothing_is_consumed() {
        let input = "Content-Length: 2\r\n\r\n{}";
        let mut reader = std::io::BufReader::new(input.as_bytes());
        assert_eq!(
            classify_first_frame(&mut reader).unwrap(),
            FirstFrame::Acp {
                framing: FramingMode::ContentLength,
                body: Vec::new(),
            }
        );
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut reader, &mut rest).unwrap();
        assert_eq!(rest, input, "the header bytes were eaten by the probe");
    }

    /// Leading whitespace is skipped, not counted as the first byte — but only
    /// the whitespace is consumed, so the verdict still leaves the message.
    #[test]
    fn leading_whitespace_does_not_decide_anything() {
        let input = "\r\n  Content-Length: 2\r\n\r\n{}";
        let mut reader = std::io::BufReader::new(input.as_bytes());
        assert_eq!(
            classify_first_frame(&mut reader).unwrap(),
            FirstFrame::Acp {
                framing: FramingMode::ContentLength,
                body: Vec::new(),
            }
        );
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut reader, &mut rest).unwrap();
        assert_eq!(rest, "Content-Length: 2\r\n\r\n{}");
    }

    /// A socket that opened and closed is not a client that got something
    /// wrong. Something checking whether anything is listening does this.
    #[test]
    fn an_empty_connection_is_closed_not_broken() {
        assert_eq!(classify(""), FirstFrame::Closed);
        assert_eq!(classify("\n\n"), FirstFrame::Closed);
    }

    /// When the probe cannot tell, it says legacy. Answering a legacy-shaped
    /// error costs a reconnect; holding the connection open speaking ACP at a
    /// peer waiting for one line costs the whole call.
    #[test]
    fn an_undecodable_first_line_falls_to_legacy() {
        let broken = "{not json at all\n";
        assert_eq!(
            classify(broken),
            FirstFrame::Legacy {
                line: broken.to_string()
            }
        );
    }

    /// The ceiling exists so a peer that writes `{` and never stops cannot
    /// hold a worker thread and unbounded memory. Past it the line simply
    /// ends, and an unterminated envelope is a legacy decode error — which is
    /// what the client that sent it will be told.
    #[test]
    fn an_endless_first_line_stops_at_the_ceiling() {
        struct Endless;
        impl std::io::Read for Endless {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                buffer.fill(b'{');
                Ok(buffer.len())
            }
        }
        let mut reader = std::io::BufReader::new(Endless);
        match classify_first_frame_within(&mut reader, 4_096).unwrap() {
            FirstFrame::Legacy { line } => assert_eq!(line.len(), 4_096),
            other => panic!("expected a bounded legacy verdict, got {other:?}"),
        }
    }

    /// The shipped ceiling, pinned so it is a decision rather than a number
    /// that drifted. A legacy `Reply` carries base64 images, so it has to be
    /// far above any real prompt and far below anything that threatens a
    /// worker.
    #[test]
    fn the_ceiling_is_sixty_four_mebibytes() {
        assert_eq!(MAX_FIRST_LINE_BYTES, 64 * 1024 * 1024);
    }
}
