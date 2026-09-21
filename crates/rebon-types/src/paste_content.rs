//! [`PromptPasteContent`] — one row of the prompt's pasted-content store.
//!
//! A data shape, not a component: the background runtime carries these rows
//! across a detach/attach without ever drawing them, and the history and
//! submit-payload files persist them. Keeping it in the terminal half would
//! make every one of those reach through a UI crate for a six-field struct.

/// Matches the minimal prompt pasted-content shape needed by paste planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPasteContent {
    /// Stable paste id.
    pub id: u32,
    /// `"text"` or `"image"`.
    pub kind: String,
    /// Stored payload.
    pub content: String,
    /// Optional media type for images.
    pub media_type: Option<String>,
    /// Optional filename for images.
    pub filename: Option<String>,
    /// Optional current implementation path for images.
    pub source_path: Option<String>,
}
