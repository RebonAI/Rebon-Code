//! The terminal's old path to the user-text classification.
//!
//! The classification itself moved to [`rebon_render::user_text_kind`]
//! over there: the fold that decides which rows group together now lives
//! in `rebon-render` so the app and the page reach the same verdict as
//! the terminal, and it reads this classification. The names are
//! re-exported here so every `rebon_tui::user_text::*` call site reads
//! as it did.

pub use rebon_render::tool_results::{INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE};
pub use rebon_render::user_text::NO_CONTENT_MESSAGE;
pub use rebon_render::user_text_kind::{detect_user_text_kind, extract_tag, UserTextKind};
