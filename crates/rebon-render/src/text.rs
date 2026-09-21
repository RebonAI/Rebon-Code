//! Text normalization shared by tool-body renderers.
//!
//! Pinning tabs to spaces at the display boundary keeps the model-facing tool
//! result unchanged while giving every character a single deterministic column.

/// Replace tab characters with two spaces for display. Tools like Read prefix
/// every body line with `"{:>6}\t{line}"` (line number, tab, content); a raw
/// `\t` desyncs column counters in any renderer that advances columns
/// deterministically, so expand it here.
pub fn expand_tabs_for_tui(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    text.replace('\t', "  ")
}
