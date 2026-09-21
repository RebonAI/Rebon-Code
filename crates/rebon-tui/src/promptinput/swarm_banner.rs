//! The swarm banner shown above the prompt: which agent or teammate the
//! prompt is addressing, and in which color.
//!
//! The banner decision depends on AppState, teammate utilities, and agent-color tables.
//! This module models the decision tree as a pure function and injects
//! color-name -> theme-color resolution as a caller seam.

/// Minimal viewed-teammate shape used by the banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewedTeammate {
    /// Teammate agent name.
    pub agent_name: String,
    /// Optional teammate color name.
    pub color_name: Option<String>,
}

/// Leader-side team context fields used by the banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderTeamContext {
    /// Team name.
    pub team_name: String,
    /// Whether the leader actually has teammate entries.
    pub has_teammates: bool,
    /// The leader's own agent color name, if set.
    pub self_agent_color_name: Option<String>,
}

/// Active named-agent task banner data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveNamedAgent {
    /// Display name registered for the agent, if any.
    pub name: Option<String>,
    /// Fallback task description.
    pub description: String,
    /// Theme color already resolved from the task's agent type.
    pub theme_color: Option<String>,
}

/// Plain-data output of [`resolve_swarm_banner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmBannerInfo {
    /// Visible banner text.
    pub text: String,
    /// Background theme color.
    pub bg_color: String,
}

/// Pure inputs for the banner decision tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmBannerInput {
    /// This process runs as a teammate.
    pub is_teammate_process: bool,
    /// This teammate runs in-process with the leader.
    pub is_in_process_teammate: bool,
    /// This process's agent name, if any.
    pub agent_name: Option<String>,
    /// This process's team name, if any.
    pub current_team_name: Option<String>,
    /// This teammate's color name, if any.
    pub current_teammate_color_name: Option<String>,
    /// Current leader team context.
    pub leader_team_context: Option<LeaderTeamContext>,
    /// Viewed teammate when the leader is focused on one.
    pub viewed_teammate: Option<ViewedTeammate>,
    /// Whether the terminal runs inside tmux: `None` means still loading.
    pub inside_tmux: Option<bool>,
    /// Whether teammates run in-process.
    pub in_process_mode: bool,
    /// Whether the cached pane-backend detection found native panes; `false`
    /// when nothing is cached.
    pub native_panes: bool,
    /// tmux socket name the swarm uses.
    pub swarm_socket_name: Option<String>,
    /// Named-agent task the prompt input is currently routed to.
    pub active_named_agent: Option<ActiveNamedAgent>,
    /// Standalone agent name from `/rename`.
    pub standalone_name: Option<String>,
    /// Standalone agent color from `/color`.
    pub standalone_color_name: Option<String>,
    /// `--agent` CLI flag.
    pub agent_flag_name: Option<String>,
    /// Color name of the active agent definition matching `--agent`, if any.
    pub agent_flag_color_name: Option<String>,
}

/// Resolve the banner, first match wins: an out-of-process teammate's own
/// name, the leader's tmux hint or viewed teammate, the active named agent,
/// the standalone `/rename` / `/color` identity, then the `--agent` flag.
pub fn resolve_swarm_banner(
    input: &SwarmBannerInput,
    resolve_theme_color: impl Fn(&str) -> Option<String>,
) -> Option<SwarmBannerInfo> {
    if input.is_teammate_process && !input.is_in_process_teammate {
        if let (Some(agent_name), Some(_team_name)) = (
            input.agent_name.as_deref(),
            input.current_team_name.as_deref(),
        ) {
            let bg_color = input
                .leader_team_context
                .as_ref()
                .and_then(|ctx| ctx.self_agent_color_name.as_deref())
                .or(input.current_teammate_color_name.as_deref())
                .and_then(&resolve_theme_color)
                .unwrap_or_else(|| String::from("cyan_FOR_SUBAGENTS_ONLY"));
            return Some(SwarmBannerInfo {
                text: format!("@{agent_name}"),
                bg_color,
            });
        }
    }

    if input
        .leader_team_context
        .as_ref()
        .is_some_and(|ctx| ctx.has_teammates)
    {
        let viewed_color = input
            .viewed_teammate
            .as_ref()
            .and_then(|teammate| teammate.color_name.as_deref())
            .and_then(&resolve_theme_color)
            .unwrap_or_else(|| String::from("cyan_FOR_SUBAGENTS_ONLY"));

        if input.inside_tmux == Some(false) && !input.in_process_mode && !input.native_panes {
            return Some(SwarmBannerInfo {
                text: format!(
                    "View teammates: `tmux -L {} a`",
                    input.swarm_socket_name.as_deref().unwrap_or_default()
                ),
                bg_color: viewed_color,
            });
        }

        if (input.inside_tmux == Some(true) || input.in_process_mode || input.native_panes)
            && input.viewed_teammate.is_some()
        {
            let viewed_teammate = input.viewed_teammate.as_ref().unwrap();
            return Some(SwarmBannerInfo {
                text: format!("@{}", viewed_teammate.agent_name),
                bg_color: viewed_color,
            });
        }
    }

    if let Some(active) = &input.active_named_agent {
        return Some(SwarmBannerInfo {
            text: active
                .name
                .as_ref()
                .map(|name| format!("@{name}"))
                .unwrap_or_else(|| active.description.clone()),
            bg_color: active
                .theme_color
                .clone()
                .unwrap_or_else(|| String::from("cyan_FOR_SUBAGENTS_ONLY")),
        });
    }

    if input.standalone_name.is_some() || input.standalone_color_name.is_some() {
        return Some(SwarmBannerInfo {
            text: input.standalone_name.clone().unwrap_or_default(),
            bg_color: input
                .standalone_color_name
                .as_deref()
                .and_then(&resolve_theme_color)
                .unwrap_or_else(|| String::from("cyan_FOR_SUBAGENTS_ONLY")),
        });
    }

    if let Some(agent_flag_name) = &input.agent_flag_name {
        return Some(SwarmBannerInfo {
            text: agent_flag_name.clone(),
            bg_color: input
                .agent_flag_color_name
                .as_deref()
                .and_then(&resolve_theme_color)
                .unwrap_or_else(|| String::from("promptBorder")),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SwarmBannerInput {
        SwarmBannerInput {
            is_teammate_process: false,
            is_in_process_teammate: false,
            agent_name: None,
            current_team_name: None,
            current_teammate_color_name: None,
            leader_team_context: None,
            viewed_teammate: None,
            inside_tmux: None,
            in_process_mode: false,
            native_panes: false,
            swarm_socket_name: Some(String::from("team-123")),
            active_named_agent: None,
            standalone_name: None,
            standalone_color_name: None,
            agent_flag_name: None,
            agent_flag_color_name: None,
        }
    }

    fn resolve_theme_color(color_name: &str) -> Option<String> {
        match color_name {
            "cyan" => Some(String::from("cyan_FOR_SUBAGENTS_ONLY")),
            "green" => Some(String::from("success")),
            _ => None,
        }
    }

    #[test]
    fn teammate_process_uses_self_agent_or_teammate_color() {
        let mut input = base();
        input.is_teammate_process = true;
        input.agent_name = Some(String::from("alice"));
        input.current_team_name = Some(String::from("team"));
        input.current_teammate_color_name = Some(String::from("green"));
        let banner = resolve_swarm_banner(&input, resolve_theme_color).unwrap();
        assert_eq!(banner.text, "@alice");
        assert_eq!(banner.bg_color, "success");
    }

    #[test]
    fn leader_without_tmux_shows_attach_hint() {
        let mut input = base();
        input.leader_team_context = Some(LeaderTeamContext {
            team_name: String::from("team"),
            has_teammates: true,
            self_agent_color_name: None,
        });
        input.inside_tmux = Some(false);
        input.viewed_teammate = Some(ViewedTeammate {
            agent_name: String::from("bob"),
            color_name: Some(String::from("cyan")),
        });
        let banner = resolve_swarm_banner(&input, resolve_theme_color).unwrap();
        assert_eq!(banner.text, "View teammates: `tmux -L team-123 a`");
        assert_eq!(banner.bg_color, "cyan_FOR_SUBAGENTS_ONLY");
    }

    #[test]
    fn leader_in_tmux_shows_viewed_teammate_banner() {
        let mut input = base();
        input.leader_team_context = Some(LeaderTeamContext {
            team_name: String::from("team"),
            has_teammates: true,
            self_agent_color_name: None,
        });
        input.inside_tmux = Some(true);
        input.viewed_teammate = Some(ViewedTeammate {
            agent_name: String::from("bob"),
            color_name: Some(String::from("green")),
        });
        let banner = resolve_swarm_banner(&input, resolve_theme_color).unwrap();
        assert_eq!(banner.text, "@bob");
        assert_eq!(banner.bg_color, "success");
    }

    #[test]
    fn loading_tmux_state_falls_through_to_standalone_agent() {
        let mut input = base();
        input.leader_team_context = Some(LeaderTeamContext {
            team_name: String::from("team"),
            has_teammates: true,
            self_agent_color_name: None,
        });
        input.standalone_name = Some(String::from("solo"));
        let banner = resolve_swarm_banner(&input, resolve_theme_color).unwrap();
        assert_eq!(banner.text, "solo");
        assert_eq!(banner.bg_color, "cyan_FOR_SUBAGENTS_ONLY");
    }

    #[test]
    fn named_agent_and_agent_flag_paths_use_expected_fallbacks() {
        let mut named_agent = base();
        named_agent.active_named_agent = Some(ActiveNamedAgent {
            name: None,
            description: String::from("worker task"),
            theme_color: None,
        });
        let banner = resolve_swarm_banner(&named_agent, resolve_theme_color).unwrap();
        assert_eq!(banner.text, "worker task");
        assert_eq!(banner.bg_color, "cyan_FOR_SUBAGENTS_ONLY");

        let mut agent_flag = base();
        agent_flag.agent_flag_name = Some(String::from("reviewer"));
        let banner = resolve_swarm_banner(&agent_flag, resolve_theme_color).unwrap();
        assert_eq!(banner.text, "reviewer");
        assert_eq!(banner.bg_color, "promptBorder");
    }
}
