use crate::byline::byline_render;
use crate::pane::{pane_style, PaneStyle};
use crate::shortcut_hint::format_shortcut_hint;
use crate::theme::ThemeName;

/// Theme key a dialog falls back to when the caller passes no color:
/// `"permission"`.
pub const DEFAULT_DIALOG_COLOR: &str = "permission";

/// Whether the cancel keybinding is live by default.
pub const DEFAULT_IS_CANCEL_ACTIVE: bool = true;

/// Keybinding context dialogs register their keys under:
/// `"Confirmation"`.
pub const KEYBINDING_CONTEXT: &str = "Confirmation";

/// Action ID the cancel keybinding dispatches: `"confirm:no"`.
pub const CANCEL_ACTION_ID: &str = "confirm:no";

/// Cancel label shown when the keybinding subsystem reports no shortcut for
/// it: `"Esc"`.
pub const DEFAULT_CANCEL_SHORTCUT: &str = "Esc";

/// The small set of theme keys dialogs are drawn with.
///
/// A dialog's `color` may be any theme key, but in practice they come from
/// this fixed set; naming them gives callers a typed handle to switch on
/// instead of a bare string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialogPalette {
    /// `"permission"`, the default.
    Permission,
    /// `"autoAccept"`.
    AutoAccept,
    /// `"bashBorder"`.
    BashBorder,
    /// `"warning"`.
    Warning,
    /// `"error"`.
    Error,
    /// `"professionalBlue"`.
    ProfessionalBlue,
}

impl Default for DialogPalette {
    fn default() -> Self {
        DialogPalette::Permission
    }
}

impl DialogPalette {
    /// The theme key this palette entry resolves to.
    pub fn theme_key(self) -> &'static str {
        match self {
            DialogPalette::Permission => "permission",
            DialogPalette::AutoAccept => "autoAccept",
            DialogPalette::BashBorder => "bashBorder",
            DialogPalette::Warning => "warning",
            DialogPalette::Error => "error",
            DialogPalette::ProfessionalBlue => "professionalBlue",
        }
    }
}

/// Resolved style for one dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogStyle {
    /// The pane wrapping the dialog — a dialog is itself a pane.
    pub pane: PaneStyle,
    /// Theme key used for the title and the surrounding border.
    pub color_key: String,
    /// True when the dialog body should be drawn inside a border.
    pub show_border: bool,
    /// Footer text: either the `<confirm> · <cancel>` byline or
    /// `"Press {key} again to exit"` while an exit is pending. `None` when
    /// the input guide is hidden.
    pub footer: Option<String>,
}

/// Resolve the style for one dialog.
///
/// * `color` — theme key for the title color; [`DEFAULT_DIALOG_COLOR`] when
///   `None`.
/// * `inside_modal` — forwarded to the inner pane.
/// * `hide_input_guide` — drop the footer entirely.
/// * `hide_border` — drop the border, for a dialog already nested in a
///   bordered container.
/// * `is_cancel_active` — whether the cancel keybinding is live.
/// * `cancel_shortcut` — display text for the cancel key, typically
///   [`DEFAULT_CANCEL_SHORTCUT`].
/// * `pending_exit_key` — `Some(key)` while an exit is pending (Ctrl+C /
///   Ctrl+D), `None` otherwise.
///
/// A hidden input guide leaves the footer `None`; otherwise a pending exit
/// takes priority over the confirm/cancel byline.
#[allow(clippy::too_many_arguments)]
pub fn dialog_style(
    color: Option<&str>,
    inside_modal: bool,
    hide_input_guide: bool,
    hide_border: bool,
    is_cancel_active: bool,
    cancel_shortcut: &str,
    pending_exit_key: Option<&str>,
    theme: ThemeName,
) -> DialogStyle {
    let color_key = color.unwrap_or(DEFAULT_DIALOG_COLOR).to_string();
    let pane = pane_style(Some(&color_key), inside_modal, theme);

    let footer = if hide_input_guide {
        None
    } else if let Some(key_name) = pending_exit_key {
        Some(format!(
            "Press {} again to exit",
            crate::shortcut_hint::format_shortcut_for_current_platform(key_name)
        ))
    } else {
        // Default byline: Enter to confirm · {cancel_shortcut} to cancel
        let confirm = format_shortcut_hint("Enter", "confirm", false, false);
        let cancel_label = if is_cancel_active {
            format_shortcut_hint(cancel_shortcut, "cancel", false, false).plain_text
        } else {
            // Cancel is gated off — show the shortcut anyway since
            // the keybinding hint is purely informational.
            format_shortcut_hint(cancel_shortcut, "cancel", false, false).plain_text
        };
        Some(byline_render(&[&confirm.plain_text, &cancel_label]))
    };

    DialogStyle {
        pane,
        color_key,
        show_border: !hide_border,
        footer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pinned() {
        assert_eq!(DEFAULT_DIALOG_COLOR, "permission");
        assert_eq!(DEFAULT_IS_CANCEL_ACTIVE, true);
        assert_eq!(KEYBINDING_CONTEXT, "Confirmation");
        assert_eq!(CANCEL_ACTION_ID, "confirm:no");
        assert_eq!(DEFAULT_CANCEL_SHORTCUT, "Esc");
    }

    #[test]
    fn dialog_palette_default_is_permission() {
        assert_eq!(DialogPalette::default(), DialogPalette::Permission);
        assert_eq!(DialogPalette::default().theme_key(), "permission");
    }

    #[test]
    fn dialog_palette_theme_keys() {
        assert_eq!(DialogPalette::Permission.theme_key(), "permission");
        assert_eq!(DialogPalette::AutoAccept.theme_key(), "autoAccept");
        assert_eq!(DialogPalette::BashBorder.theme_key(), "bashBorder");
        assert_eq!(DialogPalette::Warning.theme_key(), "warning");
        assert_eq!(DialogPalette::Error.theme_key(), "error");
        assert_eq!(
            DialogPalette::ProfessionalBlue.theme_key(),
            "professionalBlue"
        );
    }

    #[test]
    fn default_color_is_permission_when_none() {
        let s = dialog_style(
            None,
            false,
            false,
            false,
            true,
            "Esc",
            None,
            ThemeName::Dark,
        );
        assert_eq!(s.color_key, "permission");
    }

    #[test]
    fn explicit_color_overrides_default() {
        let s = dialog_style(
            Some("autoAccept"),
            false,
            false,
            false,
            true,
            "Esc",
            None,
            ThemeName::Dark,
        );
        assert_eq!(s.color_key, "autoAccept");
    }

    #[test]
    fn footer_is_none_when_hide_input_guide() {
        let s = dialog_style(None, false, true, false, true, "Esc", None, ThemeName::Dark);
        assert_eq!(s.footer, None);
    }

    #[test]
    fn footer_is_byline_when_not_pending() {
        let s = dialog_style(
            None,
            false,
            false,
            false,
            true,
            "Esc",
            None,
            ThemeName::Dark,
        );
        assert_eq!(
            s.footer.as_deref(),
            Some("Enter to confirm · Esc to cancel")
        );
    }

    #[test]
    fn footer_is_pending_message_when_exit_pending() {
        let s = dialog_style(
            None,
            false,
            false,
            false,
            true,
            "Esc",
            Some("Ctrl+C"),
            ThemeName::Dark,
        );
        assert_eq!(s.footer.as_deref(), Some("Press Ctrl+C again to exit"));
    }

    #[test]
    fn pending_exit_overrides_byline_when_both_present() {
        // hide_input_guide is FALSE, exit is pending, so the
        // pending message wins.
        let s = dialog_style(
            None,
            false,
            false,
            false,
            true,
            "Esc",
            Some("Ctrl+D"),
            ThemeName::Dark,
        );
        assert!(s.footer.as_deref().unwrap().starts_with("Press Ctrl+D"));
    }

    #[test]
    fn hide_input_guide_takes_priority_over_pending_exit() {
        // hide_input_guide=true → footer is None even if exit is pending.
        let s = dialog_style(
            None,
            false,
            true,
            false,
            true,
            "Esc",
            Some("Ctrl+D"),
            ThemeName::Dark,
        );
        assert_eq!(s.footer, None);
    }

    #[test]
    fn show_border_inverse_of_hide_border() {
        let with = dialog_style(
            None,
            false,
            false,
            false,
            true,
            "Esc",
            None,
            ThemeName::Dark,
        );
        let without = dialog_style(None, false, false, true, true, "Esc", None, ThemeName::Dark);
        assert!(with.show_border);
        assert!(!without.show_border);
    }

    #[test]
    fn pane_propagates_inside_modal() {
        let m = dialog_style(None, true, false, false, true, "Esc", None, ThemeName::Dark);
        let n = dialog_style(
            None,
            false,
            false,
            false,
            true,
            "Esc",
            None,
            ThemeName::Dark,
        );
        assert_eq!(m.pane.padding_x, 1);
        assert_eq!(n.pane.padding_x, 2);
    }

    #[test]
    fn custom_cancel_shortcut_appears_in_footer() {
        let s = dialog_style(
            None,
            false,
            false,
            false,
            true,
            "ctrl+x",
            None,
            ThemeName::Dark,
        );
        assert!(s.footer.as_deref().unwrap().contains("Ctrl+X"));
    }
}
