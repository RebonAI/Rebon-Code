use super::*;

/// Visual gutter width used for the one-char glyph + single space.
pub(super) const GUTTER: u16 = 2;

/// A small bag of styles. [`RenderTheme::plain()`] returns a
/// no-style variant for deterministic golden tests.
#[derive(Debug, Clone, Copy)]
pub struct RenderTheme {
    pub user_prefix: Style,
    pub assistant_prefix: Style,
    pub system_info: Style,
    pub system_warning: Style,
    pub system_error: Style,
    pub thinking: Style,
    pub tool_header: Style,
    pub tool_result_error: Style,
    pub streaming: Style,
    pub unknown: Style,
    pub code_block: Style,
    pub code_fence: Style,
    /// Active design-system theme name. Carried alongside the styles
    /// so downstream renderers can re-derive the full palette without
    /// hardcoding `ThemeName::Dark`.
    pub name: ThemeName,
    /// Monotonic animation clock in milliseconds. Callers that want
    /// the in-progress tool-use gutter `●` to breathe (dim ↔ bright at
    /// 1 Hz) should
    /// set this to the CLI's `elapsed_ms` before each draw. Leaving it
    /// at `0` pins the dot to its bright phase — tests and static
    /// golden snapshots rely on this.
    pub frame_time_ms: u64,
    /// Whether OSC-8 hyperlinks should be embedded in path labels.
    /// Measurement/snapshot tests keep this disabled; the real TUI
    /// enables it when rendering to a hyperlink-capable terminal.
    pub supports_hyperlinks: bool,
    /// Formula parsing and terminal image enhancement policy.
    pub math_display: MathDisplayMode,
}

impl RenderTheme {
    pub const fn plain() -> Self {
        Self {
            user_prefix: Style::new(),
            assistant_prefix: Style::new(),
            system_info: Style::new(),
            system_warning: Style::new(),
            system_error: Style::new(),
            thinking: Style::new(),
            tool_header: Style::new(),
            tool_result_error: Style::new(),
            streaming: Style::new(),
            unknown: Style::new(),
            code_block: Style::new(),
            code_fence: Style::new(),
            name: ThemeName::Dark,
            frame_time_ms: 0,
            supports_hyperlinks: false,
            math_display: MathDisplayMode::Off,
        }
    }

    pub const fn without_math_graphics(mut self) -> Self {
        self.math_display = self.math_display.without_graphics();
        self
    }

    /// Build a `RenderTheme` from a `rebon-design-system` theme palette.
    /// Maps semantic theme color tokens to ratatui `Style`s.
    pub fn from_theme(t: &Theme) -> Self {
        Self::from_theme_with_name(t, ThemeName::Dark)
    }

    fn from_theme_with_name(t: &Theme, name: ThemeName) -> Self {
        Self {
            // The user pointer ❯ uses the `suggestion` token, bolded.
            user_prefix: Style::new()
                .fg(parse_theme_color(t.suggestion))
                .add_modifier(Modifier::BOLD),
            // The "⎿" prefix uses the inactive token (no bold).
            // Also used for the "●" gutter and tool-completed status.
            assistant_prefix: Style::new().fg(parse_theme_color(t.inactive)),
            system_info: Style::new().fg(parse_theme_color(t.inactive)),
            system_warning: Style::new().fg(parse_theme_color(t.warning)),
            system_error: Style::new()
                .fg(parse_theme_color(t.error))
                .add_modifier(Modifier::BOLD),
            // Thinking rows: the inactive token, italic.
            thinking: Style::new()
                .fg(parse_theme_color(t.inactive))
                .add_modifier(Modifier::ITALIC),
            // Tool name: the default text color, bold (no specific theme
            // color).
            tool_header: Style::new().add_modifier(Modifier::BOLD),
            tool_result_error: Style::new().fg(parse_theme_color(t.error)),
            // Streaming dot: the terminal's default color — no explicit
            // fg is set. An explicit `Rgb(255,255,255)` gets collapsed to
            // ANSI "white" on some terminals, which renders as gray.
            streaming: Style::new(),
            unknown: Style::new()
                .fg(parse_theme_color(t.error))
                .add_modifier(Modifier::DIM),
            code_block: Style::new(),
            code_fence: Style::new()
                .fg(parse_theme_color(t.subtle))
                .add_modifier(Modifier::DIM),
            name,
            frame_time_ms: 0,
            supports_hyperlinks: false,
            math_display: MathDisplayMode::Off,
        }
    }

    /// Build a `RenderTheme` from a named design-system theme.
    pub fn from_theme_name(name: ThemeName) -> Self {
        let t = rebon_design_system::theme::get_theme(name);
        Self::from_theme_with_name(&t, name)
    }

    /// Legacy hardcoded colors — kept for `plain()` tests only.
    pub fn default_styled() -> Self {
        Self::from_theme_name(ThemeName::Dark)
    }
}

impl Default for RenderTheme {
    fn default() -> Self {
        Self::default_styled()
    }
}

/// Parse a design-system color string (`"rgb(r,g,b)"`, `"#hex"`,
/// `"ansi256(n)"`, `"ansi:name"`) into a ratatui `Color`; the one
/// implementation lives in `rebon-message-tui`.
pub use rebon_message_tui::parse_theme_color;
