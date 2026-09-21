//! Tolerant handling of `{"summary":"…"}` envelope messages.
//!
//! Side-channel model calls (session titles, agent-view row summaries)
//! instruct the model to answer with a single-field JSON object. A
//! transport race on the codex websocket backend used to let such a
//! response be consumed by the live conversation and persisted as a
//! regular assistant message, so existing transcripts (and streams
//! from not-yet-updated binaries) can contain assistant text that is
//! literally `{"summary":"fixed mobile chat refresh"}`. Rendering
//! layers use [`display_text_for_summary_envelope`] to show the
//! summary sentence instead of raw JSON.
//!
//! The read itself lives in `rebon-types`, beside the wire types it is
//! tolerating. This module is the name the rendering layers reach for; it
//! used to carry a byte-identical second copy, which meant the two could
//! drift on what counts as an envelope while both claimed to be strict.

pub use rebon_types::{display_text_for_summary_envelope, summary_envelope_text};
