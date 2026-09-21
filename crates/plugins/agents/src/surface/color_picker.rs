//! Agent color-picker selector reducer.
//!
//! A small selector that:
//!
//! * starts on the index of the current color (0 if `automatic`),
//! * up/down navigate (with no wrap), and
//! * Enter confirms with `None` for `automatic` or the chosen
//!   color name otherwise.
//!
//! Produces a [`ColorPickerState`] reducer with an
//! event enum the consumer drives. The wrapped `Option<AgentColorName>`
//! returned by [`ColorPickerState::confirm`] is the confirm result.

/// One of the named agent colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentColorName {
    /// Red.
    Red,
    /// Blue.
    Blue,
    /// Green.
    Green,
    /// Yellow.
    Yellow,
    /// Purple.
    Purple,
    /// Orange.
    Orange,
    /// Pink.
    Pink,
    /// Cyan.
    Cyan,
}

impl AgentColorName {
    /// Lowercase string used in YAML frontmatter.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentColorName::Red => "red",
            AgentColorName::Blue => "blue",
            AgentColorName::Green => "green",
            AgentColorName::Yellow => "yellow",
            AgentColorName::Purple => "purple",
            AgentColorName::Orange => "orange",
            AgentColorName::Pink => "pink",
            AgentColorName::Cyan => "cyan",
        }
    }
}

/// The full ordered list — `AGENT_COLORS`.
pub const AGENT_COLORS: &[AgentColorName] = &[
    AgentColorName::Red,
    AgentColorName::Blue,
    AgentColorName::Green,
    AgentColorName::Yellow,
    AgentColorName::Purple,
    AgentColorName::Orange,
    AgentColorName::Pink,
    AgentColorName::Cyan,
];

/// One option in the picker — `automatic` or one of the named colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorOption {
    /// Theme decides — saved as `None` in the agent definition.
    Automatic,
    /// A specific color.
    Named(AgentColorName),
}

impl ColorOption {
    /// Lowercase label shown in the picker (and used by lookups).
    pub fn label(&self) -> &'static str {
        match self {
            ColorOption::Automatic => "automatic",
            ColorOption::Named(c) => c.as_str(),
        }
    }
}

/// The full picker option list: `automatic` followed by
/// [`AGENT_COLORS`] in order.
pub fn color_options() -> Vec<ColorOption> {
    let mut out = Vec::with_capacity(AGENT_COLORS.len() + 1);
    out.push(ColorOption::Automatic);
    for &c in AGENT_COLORS {
        out.push(ColorOption::Named(c));
    }
    out
}

/// Reducer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorPickerState {
    /// Currently-highlighted index into [`color_options()`].
    pub selected_index: usize,
}

impl ColorPickerState {
    /// Build a new state with the cursor on `current_color` (or
    /// `Automatic` if `None`).
    ///
    /// The cursor is placed at the position of `current_color` in
    /// [`color_options()`]; a color that is not in the list, and
    /// `None`, both start at index 0.
    pub fn new(current_color: Option<AgentColorName>) -> Self {
        let opts = color_options();
        let selected = match current_color {
            None => 0,
            Some(c) => opts
                .iter()
                .position(|o| matches!(o, ColorOption::Named(n) if *n == c))
                .unwrap_or(0),
        };
        ColorPickerState {
            selected_index: selected,
        }
    }

    /// The currently-selected option.
    pub fn current(&self) -> ColorOption {
        color_options()[self.selected_index]
    }

    /// Apply an event. Returns the new state — pure reducer.
    ///
    /// Up and down move the cursor one step; both ends saturate.
    pub fn handle_event(self, event: ColorPickerEvent) -> Self {
        let opts = color_options();
        let max = opts.len() - 1;
        match event {
            ColorPickerEvent::Up => ColorPickerState {
                selected_index: self.selected_index.saturating_sub(1),
            },
            ColorPickerEvent::Down => ColorPickerState {
                selected_index: (self.selected_index + 1).min(max),
            },
        }
    }

    /// Confirm the current selection. Returns `None` for `automatic`,
    /// `Some(color)` otherwise.
    ///
    /// Reads the current selection only — the caller advances the cursor
    /// with [`handle_event`](Self::handle_event) first.
    pub fn confirm(&self) -> Option<AgentColorName> {
        match self.current() {
            ColorOption::Automatic => None,
            ColorOption::Named(c) => Some(c),
        }
    }
}

/// Events that drive [`ColorPickerState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerEvent {
    /// Up arrow — move cursor up (saturates at 0).
    Up,
    /// Down arrow — move cursor down (saturates at end).
    Down,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_count_is_nine() {
        // 1 automatic + 8 colors.
        assert_eq!(color_options().len(), 9);
    }

    #[test]
    fn options_start_with_automatic() {
        assert_eq!(color_options()[0], ColorOption::Automatic);
    }

    #[test]
    fn new_with_none_starts_at_automatic() {
        let s = ColorPickerState::new(None);
        assert_eq!(s.selected_index, 0);
        assert_eq!(s.current(), ColorOption::Automatic);
    }

    #[test]
    fn new_with_blue_starts_at_blue() {
        let s = ColorPickerState::new(Some(AgentColorName::Blue));
        assert_eq!(s.current(), ColorOption::Named(AgentColorName::Blue));
    }

    #[test]
    fn down_advances() {
        let s = ColorPickerState::new(None);
        let s = s.handle_event(ColorPickerEvent::Down);
        assert_eq!(s.current(), ColorOption::Named(AgentColorName::Red));
    }

    #[test]
    fn up_at_top_saturates() {
        let s = ColorPickerState::new(None);
        let s = s.handle_event(ColorPickerEvent::Up);
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn down_at_bottom_saturates() {
        let mut s = ColorPickerState::new(Some(AgentColorName::Cyan));
        for _ in 0..5 {
            s = s.handle_event(ColorPickerEvent::Down);
        }
        assert_eq!(s.current(), ColorOption::Named(AgentColorName::Cyan));
    }

    #[test]
    fn confirm_automatic_returns_none() {
        let s = ColorPickerState::new(None);
        assert_eq!(s.confirm(), None);
    }

    #[test]
    fn confirm_named_returns_some() {
        let s = ColorPickerState::new(Some(AgentColorName::Pink));
        assert_eq!(s.confirm(), Some(AgentColorName::Pink));
    }

    #[test]
    fn agent_color_strings_are_lowercase() {
        for c in AGENT_COLORS {
            assert_eq!(c.as_str().to_lowercase(), c.as_str());
        }
    }

    #[test]
    fn cursor_navigates_through_all_colors() {
        let mut s = ColorPickerState::new(None);
        let expected = [
            ColorOption::Automatic,
            ColorOption::Named(AgentColorName::Red),
            ColorOption::Named(AgentColorName::Blue),
            ColorOption::Named(AgentColorName::Green),
            ColorOption::Named(AgentColorName::Yellow),
            ColorOption::Named(AgentColorName::Purple),
            ColorOption::Named(AgentColorName::Orange),
            ColorOption::Named(AgentColorName::Pink),
            ColorOption::Named(AgentColorName::Cyan),
        ];
        for &want in &expected {
            assert_eq!(s.current(), want);
            s = s.handle_event(ColorPickerEvent::Down);
        }
    }
}
