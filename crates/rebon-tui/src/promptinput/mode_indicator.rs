//! Prompt-mode indicator resolution.
//!
//! This resolver depends on external agent-color tables, so it
//! keeps the prompt-mode decision local and injects theme-color resolution as a
//! caller-owned callback.

/// Caller-owned inputs needed to resolve the prompt mode indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptModeIndicatorInput {
    /// Current prompt mode (`prompt`, `bash`, ...).
    pub mode: String,
    /// Whether a turn is in flight; drives the dim flag.
    pub is_loading: bool,
    /// Whether we are rendering a viewed teammate prompt.
    pub viewing_agent_name: Option<String>,
    /// Optional teammate color name from the viewed teammate identity.
    pub viewing_agent_color_name: Option<String>,
    /// Whether agent swarms are enabled; gates the teammate color.
    pub agent_swarms_enabled: bool,
    /// Current teammate color when not explicitly viewing another teammate.
    pub teammate_color_name: Option<String>,
    /// Compile-time flag, `true` on the internal build variant.
    pub internal_build: bool,
}

/// Final color choice for the prompt indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptIndicatorColor {
    /// No explicit color.
    None,
    /// One of the theme keys.
    Theme(String),
}

/// Which indicator the prompt should render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptIndicatorKind {
    /// Standard prompt glyph.
    PromptChar,
    /// Bash mode `!`.
    BashBang,
}

/// Plain-data output of the prompt indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptModeIndicatorOutput {
    /// Which glyph family to draw.
    pub kind: PromptIndicatorKind,
    /// Final color.
    pub color: PromptIndicatorColor,
    /// Whether the glyph is dimmed (mirrors `is_loading`).
    pub dim: bool,
}

/// Resolve the prompt indicator kind, color, and dim flag.
pub fn resolve_prompt_mode_indicator(
    input: &PromptModeIndicatorInput,
    resolve_theme_color: impl Fn(&str) -> Option<String>,
) -> PromptModeIndicatorOutput {
    if input.viewing_agent_name.is_some() {
        return PromptModeIndicatorOutput {
            kind: PromptIndicatorKind::PromptChar,
            color: resolve_indicator_color(
                input.viewing_agent_color_name.as_deref(),
                input.internal_build,
                &resolve_theme_color,
            ),
            dim: input.is_loading,
        };
    }

    if input.mode == "bash" {
        return PromptModeIndicatorOutput {
            kind: PromptIndicatorKind::BashBang,
            color: PromptIndicatorColor::Theme(String::from("bashBorder")),
            dim: input.is_loading,
        };
    }

    let teammate_color_name = input
        .agent_swarms_enabled
        .then_some(input.teammate_color_name.as_deref())
        .flatten();
    PromptModeIndicatorOutput {
        kind: PromptIndicatorKind::PromptChar,
        color: resolve_indicator_color(
            teammate_color_name,
            input.internal_build,
            resolve_theme_color,
        ),
        dim: input.is_loading,
    }
}

fn resolve_indicator_color(
    color_name: Option<&str>,
    internal_build: bool,
    resolve_theme_color: impl Fn(&str) -> Option<String>,
) -> PromptIndicatorColor {
    if let Some(color_name) = color_name {
        if let Some(theme_color) = resolve_theme_color(color_name) {
            return PromptIndicatorColor::Theme(theme_color);
        }
    }

    if internal_build {
        PromptIndicatorColor::Theme(String::from("subtle"))
    } else {
        PromptIndicatorColor::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_theme_color(color_name: &str) -> Option<String> {
        match color_name {
            "cyan" => Some(String::from("cyan_FOR_SUBAGENTS_ONLY")),
            "green" => Some(String::from("success")),
            _ => None,
        }
    }

    #[test]
    fn bash_mode_takes_priority() {
        let output = resolve_prompt_mode_indicator(
            &PromptModeIndicatorInput {
                mode: String::from("bash"),
                is_loading: true,
                viewing_agent_name: None,
                viewing_agent_color_name: None,
                agent_swarms_enabled: true,
                teammate_color_name: Some(String::from("green")),
                internal_build: false,
            },
            resolve_theme_color,
        );
        assert_eq!(output.kind, PromptIndicatorKind::BashBang);
        assert_eq!(
            output.color,
            PromptIndicatorColor::Theme(String::from("bashBorder"))
        );
        assert!(output.dim);
    }

    #[test]
    fn viewing_agent_color_overrides_current_teammate_color() {
        let output = resolve_prompt_mode_indicator(
            &PromptModeIndicatorInput {
                mode: String::from("prompt"),
                is_loading: false,
                viewing_agent_name: Some(String::from("alice")),
                viewing_agent_color_name: Some(String::from("green")),
                agent_swarms_enabled: true,
                teammate_color_name: Some(String::from("cyan")),
                internal_build: false,
            },
            resolve_theme_color,
        );
        assert_eq!(output.kind, PromptIndicatorKind::PromptChar);
        assert_eq!(
            output.color,
            PromptIndicatorColor::Theme(String::from("success"))
        );
    }

    #[test]
    fn swarms_disabled_drops_teammate_color() {
        let output = resolve_prompt_mode_indicator(
            &PromptModeIndicatorInput {
                mode: String::from("prompt"),
                is_loading: false,
                viewing_agent_name: None,
                viewing_agent_color_name: None,
                agent_swarms_enabled: false,
                teammate_color_name: Some(String::from("cyan")),
                internal_build: false,
            },
            resolve_theme_color,
        );
        assert_eq!(output.color, PromptIndicatorColor::None);
    }

    #[test]
    fn internal_build_uses_subtle_fallback_when_no_color_exists() {
        let output = resolve_prompt_mode_indicator(
            &PromptModeIndicatorInput {
                mode: String::from("prompt"),
                is_loading: false,
                viewing_agent_name: None,
                viewing_agent_color_name: None,
                agent_swarms_enabled: true,
                teammate_color_name: Some(String::from("unknown")),
                internal_build: true,
            },
            resolve_theme_color,
        );
        assert_eq!(
            output.color,
            PromptIndicatorColor::Theme(String::from("subtle"))
        );
    }
}
