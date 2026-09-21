//! Async stdio transport built on top of [`crate::framing::FrameDecoder`].
//!
//! This wraps a pair of `AsyncRead`/`AsyncWrite` streams (typically
//! `tokio::io::stdin()` and `tokio::io::stdout()`) and exposes:
//!
//! - [`StdioReader::read_message`] — pulls the next complete raw message body
//!   from the peer, returning `Ok(None)` on clean EOF.
//! - [`StdioWriter::write_ndjson`] / [`StdioWriter::write_content_length`] —
//!   emit a message in the corresponding framing.
//!
//! There is intentionally no method dispatch or JSON-RPC request/response
//! plumbing at this layer: it moves bytes, and nothing more.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::framing::{FrameDecodeStep, FrameDecoder, FramingMode};

/// Asynchronous reader that consumes an `AsyncRead` and surfaces one message
/// body at a time via the internal [`FrameDecoder`].
pub struct StdioReader<R> {
    reader: R,
    decoder: FrameDecoder,
    chunk: Vec<u8>,
}

impl<R: AsyncRead + Unpin> StdioReader<R> {
    /// Wrap a reader. The decoder auto-detects its framing mode from the
    /// first bytes received.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            decoder: FrameDecoder::new(),
            chunk: vec![0u8; 4096],
        }
    }

    /// Wrap a reader and lock it to a specific framing mode.
    pub fn with_framing(reader: R, framing: FramingMode) -> Self {
        Self {
            reader,
            decoder: FrameDecoder::with_framing(framing),
            chunk: vec![0u8; 4096],
        }
    }

    /// Current framing mode of the underlying decoder. Returns
    /// [`FramingMode::Auto`] until auto-detection has seen real bytes.
    pub fn framing(&self) -> FramingMode {
        self.decoder.framing()
    }

    /// Pull the next complete message body from the stream.
    ///
    /// Returns `Ok(None)` on a clean EOF (reader returned `0`).
    pub async fn read_message(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            match self.decoder.next_message() {
                FrameDecodeStep::Message(body) => return Ok(Some(body)),
                FrameDecodeStep::NeedMore => {
                    let n = self.reader.read(&mut self.chunk).await?;
                    if n == 0 {
                        return Ok(None);
                    }
                    self.decoder.push(&self.chunk[..n]);
                }
            }
        }
    }
}

/// Asynchronous writer that emits JSON-RPC messages with a chosen framing.
pub struct StdioWriter<W> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> StdioWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Consume the writer and return the inner stream.
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Write a message body using newline-delimited JSON framing.
    ///
    /// `body` must be minified: NDJSON has no escape for an embedded newline,
    /// so a debug build asserts one is absent. `serde_json::to_vec` produces
    /// minified output by default.
    pub async fn write_ndjson(&mut self, body: &[u8]) -> std::io::Result<()> {
        debug_assert!(
            !body.contains(&b'\n'),
            "NDJSON body must not contain embedded newlines; use serde_json::to_vec"
        );
        self.writer.write_all(body).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Write a message body using Content-Length header framing.
    pub async fn write_content_length(&mut self, body: &[u8]) -> std::io::Result<()> {
        let header = crate::framing::content_length_header(body.len());
        self.writer.write_all(header.as_bytes()).await?;
        self.writer.write_all(body).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fill_and_read<const N: usize>(
        input: &[u8],
    ) -> (StdioReader<tokio::io::DuplexStream>, Vec<Vec<u8>>) {
        let (mut client, server) = tokio::io::duplex(N);
        client.write_all(input).await.unwrap();
        drop(client); // signal EOF
        let mut reader = StdioReader::new(server);
        let mut messages = Vec::new();
        while let Some(m) = reader.read_message().await.unwrap() {
            messages.push(m);
        }
        (reader, messages)
    }

    #[tokio::test]
    async fn reader_ndjson_single_message() {
        let (reader, msgs) = fill_and_read::<1024>(b"{\"x\":1}\n").await;
        assert_eq!(reader.framing(), FramingMode::Ndjson);
        assert_eq!(msgs, vec![b"{\"x\":1}".to_vec()]);
    }

    #[tokio::test]
    async fn reader_ndjson_multiple_messages() {
        let (_reader, msgs) = fill_and_read::<1024>(b"{\"x\":1}\n{\"x\":2}\n{\"x\":3}\n").await;
        assert_eq!(
            msgs,
            vec![
                b"{\"x\":1}".to_vec(),
                b"{\"x\":2}".to_vec(),
                b"{\"x\":3}".to_vec(),
            ]
        );
    }

    #[tokio::test]
    async fn reader_content_length_single_message() {
        let body = br#"{"a":1}"#;
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body);
        let (reader, msgs) = fill_and_read::<1024>(&frame).await;
        assert_eq!(reader.framing(), FramingMode::ContentLength);
        assert_eq!(msgs, vec![body.to_vec()]);
    }

    #[tokio::test]
    async fn reader_content_length_multiple_messages() {
        let bodies: [&[u8]; 3] = [br#"{"a":1}"#, br#"{"a":2}"#, br#"{"a":3}"#];
        let mut buf = Vec::new();
        for body in bodies {
            buf.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
            buf.extend_from_slice(body);
        }
        let (_reader, msgs) = fill_and_read::<1024>(&buf).await;
        assert_eq!(msgs, bodies.iter().map(|b| b.to_vec()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn reader_content_length_small_buffer_coalesces_chunks() {
        // Force the duplex buffer to be smaller than one frame so the reader
        // has to stitch chunks together across multiple reads.
        let body: String = "x".repeat(5000);
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());

        let (mut client, server) = tokio::io::duplex(256);
        let writer = tokio::spawn(async move {
            client.write_all(&frame).await.unwrap();
            drop(client);
        });
        let mut reader = StdioReader::new(server);
        let got = reader.read_message().await.unwrap().expect("one message");
        writer.await.unwrap();
        assert_eq!(got, body.as_bytes());
        assert_eq!(reader.read_message().await.unwrap(), None);
    }

    #[tokio::test]
    async fn reader_returns_none_on_empty_stream() {
        let mut reader = StdioReader::new(tokio::io::empty());
        assert_eq!(reader.read_message().await.unwrap(), None);
    }

    #[tokio::test]
    async fn writer_ndjson_roundtrips_through_reader() {
        let (client, server) = tokio::io::duplex(1024);
        let mut writer = StdioWriter::new(client);
        writer.write_ndjson(br#"{"id":1}"#).await.unwrap();
        writer.write_ndjson(br#"{"id":2}"#).await.unwrap();
        drop(writer.into_inner());

        let mut reader = StdioReader::new(server);
        assert_eq!(
            reader.read_message().await.unwrap().unwrap(),
            br#"{"id":1}"#.to_vec()
        );
        assert_eq!(
            reader.read_message().await.unwrap().unwrap(),
            br#"{"id":2}"#.to_vec()
        );
        assert_eq!(reader.read_message().await.unwrap(), None);
    }

    #[tokio::test]
    async fn writer_content_length_roundtrips_through_reader() {
        let (client, server) = tokio::io::duplex(1024);
        let mut writer = StdioWriter::new(client);
        writer.write_content_length(br#"{"id":1}"#).await.unwrap();
        writer.write_content_length(br#"{"id":2}"#).await.unwrap();
        drop(writer.into_inner());

        let mut reader = StdioReader::new(server);
        assert_eq!(
            reader.read_message().await.unwrap().unwrap(),
            br#"{"id":1}"#.to_vec()
        );
        assert_eq!(
            reader.read_message().await.unwrap().unwrap(),
            br#"{"id":2}"#.to_vec()
        );
        assert_eq!(reader.read_message().await.unwrap(), None);
    }
}
