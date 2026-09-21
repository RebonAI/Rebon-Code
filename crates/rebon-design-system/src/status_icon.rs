/// Tick glyph for the `success` status (`✔`).
///
/// Always the Unicode glyph, never an ASCII lookalike such as the `√`
/// some Windows terminals substitute: choosing a fallback is the
/// consumer's decision, not this crate's.
pub const FIGURE_TICK: &str = "✔";
/// Cross glyph for the `error` status (`✖`).
pub const FIGURE_CROSS: &str = "✖";
/// Warning glyph for the `warning` status (`⚠`).
pub const FIGURE_WARNING: &str = "⚠";
/// Info glyph for the `info` status (`ℹ`).
pub const FIGURE_INFO: &str = "ℹ";
/// Circle glyph for the `pending` status (`◯`), drawn without a color.
pub const FIGURE_CIRCLE: &str = "◯";
/// Ellipsis glyph for the `loading` status: `'…'` (U+2026), a single
/// character and not three ASCII dots.
pub const FIGURE_ELLIPSIS: &str = "…";

/// Which status icon to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusIconKind {
    /// Finished successfully.
    Success,
    /// Failed.
    Error,
    /// Completed with a caveat.
    Warning,
    /// Informational, not a result.
    Info,
    /// Not started yet; rendered dim.
    Pending,
    /// In progress; rendered dim.
    Loading,
}

/// Semantic color token for a status icon. Tokens name a role in the
/// palette; mapping one to a concrete color is the renderer's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusSemanticColor {
    /// The palette's success color.
    Success,
    /// The palette's error color.
    Error,
    /// The palette's warning color.
    Warning,
    /// The palette's suggestion color, which is also what `Info` uses.
    Suggestion,
}

/// Resolved glyph and color for one status icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusIconStyle {
    /// Glyph to draw.
    pub icon: &'static str,
    /// Semantic color token, or `None` when the icon carries no color of
    /// its own and should be drawn dim instead.
    pub color: Option<StatusSemanticColor>,
    /// Mirrors `color.is_none()`, so a renderer that already threads a
    /// `dim` flag through its pipeline does not have to re-derive it.
    pub dim: bool,
    /// True when a trailing space should follow the glyph.
    pub with_space: bool,
}

/// Resolve `kind` to its glyph and color token.
///
/// `with_space` asks for a trailing space after the glyph and is copied
/// straight into the result.
pub fn status_icon_style(kind: StatusIconKind, with_space: bool) -> StatusIconStyle {
    let (icon, color) = match kind {
        StatusIconKind::Success => (FIGURE_TICK, Some(StatusSemanticColor::Success)),
        StatusIconKind::Error => (FIGURE_CROSS, Some(StatusSemanticColor::Error)),
        StatusIconKind::Warning => (FIGURE_WARNING, Some(StatusSemanticColor::Warning)),
        StatusIconKind::Info => (FIGURE_INFO, Some(StatusSemanticColor::Suggestion)),
        StatusIconKind::Pending => (FIGURE_CIRCLE, None),
        StatusIconKind::Loading => (FIGURE_ELLIPSIS, None),
    };
    StatusIconStyle {
        icon,
        color,
        dim: color.is_none(),
        with_space,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_pinned() {
        assert_eq!(FIGURE_TICK, "✔");
        assert_eq!(FIGURE_CROSS, "✖");
        assert_eq!(FIGURE_WARNING, "⚠");
        assert_eq!(FIGURE_INFO, "ℹ");
        assert_eq!(FIGURE_CIRCLE, "◯");
        assert_eq!(FIGURE_ELLIPSIS, "…");
    }

    #[test]
    fn loading_ellipsis_is_single_char_not_three_dots() {
        // Uses '…' (U+2026), not '...'
        assert_eq!(FIGURE_ELLIPSIS.chars().count(), 1);
        assert_eq!(FIGURE_ELLIPSIS, "\u{2026}");
    }

    #[test]
    fn success_uses_success_token() {
        let s = status_icon_style(StatusIconKind::Success, false);
        assert_eq!(s.icon, FIGURE_TICK);
        assert_eq!(s.color, Some(StatusSemanticColor::Success));
        assert_eq!(s.dim, false);
    }

    #[test]
    fn error_uses_error_token() {
        let s = status_icon_style(StatusIconKind::Error, false);
        assert_eq!(s.icon, FIGURE_CROSS);
        assert_eq!(s.color, Some(StatusSemanticColor::Error));
    }

    #[test]
    fn warning_uses_warning_token() {
        let s = status_icon_style(StatusIconKind::Warning, false);
        assert_eq!(s.icon, FIGURE_WARNING);
        assert_eq!(s.color, Some(StatusSemanticColor::Warning));
    }

    #[test]
    fn info_uses_suggestion_token_not_info() {
        // Load-bearing: info routes to the 'suggestion' color, not a
        // separate 'info' token.
        let s = status_icon_style(StatusIconKind::Info, false);
        assert_eq!(s.icon, FIGURE_INFO);
        assert_eq!(s.color, Some(StatusSemanticColor::Suggestion));
    }

    #[test]
    fn pending_has_no_color_and_is_dim() {
        let s = status_icon_style(StatusIconKind::Pending, false);
        assert_eq!(s.icon, FIGURE_CIRCLE);
        assert_eq!(s.color, None);
        assert_eq!(s.dim, true);
    }

    #[test]
    fn loading_has_no_color_and_is_dim() {
        let s = status_icon_style(StatusIconKind::Loading, false);
        assert_eq!(s.icon, FIGURE_ELLIPSIS);
        assert_eq!(s.color, None);
        assert_eq!(s.dim, true);
    }

    #[test]
    fn with_space_propagates() {
        let a = status_icon_style(StatusIconKind::Success, true);
        let b = status_icon_style(StatusIconKind::Success, false);
        assert_eq!(a.with_space, true);
        assert_eq!(b.with_space, false);
    }

    #[test]
    fn dim_field_mirrors_color_is_none() {
        for kind in [
            StatusIconKind::Success,
            StatusIconKind::Error,
            StatusIconKind::Warning,
            StatusIconKind::Info,
            StatusIconKind::Pending,
            StatusIconKind::Loading,
        ] {
            let s = status_icon_style(kind, false);
            assert_eq!(s.dim, s.color.is_none(), "dim mismatch for {:?}", kind);
        }
    }

    #[test]
    fn status_icon_table_all_six_statuses() {
        // Covers all six statuses
        let cases = [
            (
                StatusIconKind::Success,
                FIGURE_TICK,
                Some(StatusSemanticColor::Success),
            ),
            (
                StatusIconKind::Error,
                FIGURE_CROSS,
                Some(StatusSemanticColor::Error),
            ),
            (
                StatusIconKind::Warning,
                FIGURE_WARNING,
                Some(StatusSemanticColor::Warning),
            ),
            (
                StatusIconKind::Info,
                FIGURE_INFO,
                Some(StatusSemanticColor::Suggestion),
            ),
            (StatusIconKind::Pending, FIGURE_CIRCLE, None),
            (StatusIconKind::Loading, FIGURE_ELLIPSIS, None),
        ];
        for (kind, icon, color) in cases {
            let s = status_icon_style(kind, false);
            assert_eq!(s.icon, icon, "icon mismatch for {:?}", kind);
            assert_eq!(s.color, color, "color mismatch for {:?}", kind);
        }
    }
}
