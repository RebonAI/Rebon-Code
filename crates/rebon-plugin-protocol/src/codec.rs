use crate::{WireEnvelope, PROTOCOL_VERSION};
use serde::Deserialize;
use thiserror::Error;

#[derive(Deserialize)]
struct ProtocolVersionProbe {
    protocol_version: u32,
}

/// NDJSON framing or protocol error.
///
/// Errors returned while decoding inbound data or finishing an inbound stream
/// are fatal and poison the decoder. Encoding errors leave codec state unchanged.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("NDJSON frame is empty")]
    EmptyFrame,
    #[error("NDJSON frame exceeds the configured {limit}-byte limit")]
    FrameTooLarge { limit: usize },
    #[error("NDJSON frame is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("NDJSON frame is not a valid protocol envelope: {0}")]
    InvalidEnvelope(#[from] serde_json::Error),
    #[error("unsupported protocol version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u32, expected: u32 },
    #[error("EOF arrived with an unterminated {bytes}-byte NDJSON frame")]
    IncompleteFrame { bytes: usize },
    #[error("codec cannot be reused after a fatal error")]
    Failed,
}

/// Strict, bounded, incremental NDJSON decoder and encoder.
///
/// Every non-empty inbound line must be exactly one valid protocol envelope.
/// Any inbound decode, framing, or incomplete-EOF error poisons the decoder
/// because continuing after stdout pollution or lost framing could misassociate
/// calls. An encode error does not change codec state and emits no bytes.
pub struct NdjsonCodec {
    max_frame_bytes: usize,
    buffer: Vec<u8>,
    failed: bool,
}

impl Default for NdjsonCodec {
    fn default() -> Self {
        Self::new(crate::DEFAULT_MAX_FRAME_BYTES)
    }
}

impl NdjsonCodec {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes,
            buffer: Vec::new(),
            failed: false,
        }
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Consumes an arbitrary byte chunk and returns all complete frames in it.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<WireEnvelope>, CodecError> {
        if self.failed {
            return Err(CodecError::Failed);
        }

        let mut frames = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let mut frame = std::mem::take(&mut self.buffer);
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                match self.decode_frame(&frame) {
                    Ok(envelope) => frames.push(envelope),
                    Err(error) => {
                        self.failed = true;
                        return Err(error);
                    }
                }
            } else {
                self.buffer.push(byte);
                // Permit max body bytes plus one CR while waiting for LF.
                if self.buffer.len() > self.max_frame_bytes
                    && !(self.buffer.len() == self.max_frame_bytes + 1
                        && self.buffer.last() == Some(&b'\r'))
                {
                    self.failed = true;
                    return Err(CodecError::FrameTooLarge {
                        limit: self.max_frame_bytes,
                    });
                }
            }
        }
        Ok(frames)
    }

    /// Signals clean EOF. A final JSON value without LF is an incomplete frame.
    pub fn finish(&mut self) -> Result<(), CodecError> {
        if self.failed {
            return Err(CodecError::Failed);
        }
        if self.buffer.is_empty() {
            Ok(())
        } else {
            self.failed = true;
            Err(CodecError::IncompleteFrame {
                bytes: self.buffer.len(),
            })
        }
    }

    /// Encodes exactly one JSON value followed by LF. On error, no bytes are
    /// returned and the codec remains usable for encoding and decoding.
    pub fn encode(&self, envelope: &WireEnvelope) -> Result<Vec<u8>, CodecError> {
        validate_version(envelope)?;
        let mut bytes = serde_json::to_vec(envelope)?;
        if bytes.len() > self.max_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn decode_frame(&self, frame: &[u8]) -> Result<WireEnvelope, CodecError> {
        if frame.is_empty() {
            return Err(CodecError::EmptyFrame);
        }
        if frame.len() > self.max_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        let text = std::str::from_utf8(frame)?;
        let version: ProtocolVersionProbe = serde_json::from_str(text)?;
        if version.protocol_version != PROTOCOL_VERSION {
            return Err(CodecError::UnsupportedVersion {
                actual: version.protocol_version,
                expected: PROTOCOL_VERSION,
            });
        }
        let envelope: WireEnvelope = serde_json::from_str(text)?;
        Ok(envelope)
    }
}

fn validate_version(envelope: &WireEnvelope) -> Result<(), CodecError> {
    if envelope.protocol_version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(CodecError::UnsupportedVersion {
            actual: envelope.protocol_version,
            expected: PROTOCOL_VERSION,
        })
    }
}
