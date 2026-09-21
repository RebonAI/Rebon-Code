//! The transcript row type, re-exported from where it now lives.
//!
//! The definitions moved to [`rebon_render::transcript_row`]. Nothing
//! in them knows what a terminal cell is — they are serde plus two string
//! sentinels — and `serve`, ACP and the desktop app all have to read the
//! same rows without growing a ratatui edge.
//!
//! This module stays as a re-export so every `rebon_tui::Message` /
//! `rebon_tui::message::…` use site resolves unchanged. New code outside
//! this crate should name `rebon_render::transcript_row` directly.

pub use rebon_render::transcript_row::*;
