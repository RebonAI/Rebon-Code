//! The team status row in the footer.
//!
//! Shows the teammate count and an optional hint when the team dialog is active.

/// Minimal shape of a teammate entry used by the footer status row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamStatusTeammate {
    /// Teammate display name.
    pub name: String,
    /// True when the teammate is the team lead (should be excluded from count).
    pub is_team_lead: bool,
}

/// Output shape for the status row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamStatusDisplay {
    /// Text such as `3 teammates` or `1 teammate`.
    pub text: String,
    /// Optional hint (e.g., `Enter to view`).
    pub hint: Option<String>,
    /// Whether the status row should render with inverse highlight.
    pub is_selected: bool,
}

/// Build the team status row, or `None` when there is nothing to show.
pub fn render_team_status(
    teammates: &[TeamStatusTeammate],
    teams_selected: bool,
    show_hint: bool,
) -> Option<TeamStatusDisplay> {
    let total = teammates.iter().filter(|t| !t.is_team_lead).count();
    if total == 0 {
        return None;
    }

    let text = format!(
        "{} {}",
        total,
        if total == 1 { "teammate" } else { "teammates" }
    );
    let hint = if show_hint && teams_selected {
        Some("Enter to view".to_string())
    } else {
        None
    };

    Some(TeamStatusDisplay {
        text,
        hint,
        is_selected: teams_selected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_teammates_returns_none() {
        assert!(render_team_status(&[], false, false).is_none());
    }

    #[test]
    fn counts_teammates_excluding_team_lead() {
        let teammates = vec![
            TeamStatusTeammate {
                name: "lead".into(),
                is_team_lead: true,
            },
            TeamStatusTeammate {
                name: "user1".into(),
                is_team_lead: false,
            },
        ];
        let display = render_team_status(&teammates, true, true).unwrap();
        assert_eq!(display.text, "1 teammate");
        assert_eq!(display.hint.as_deref(), Some("Enter to view"));
    }

    #[test]
    fn pluralizes_teammates() {
        let teammates = vec![
            TeamStatusTeammate {
                name: "user1".into(),
                is_team_lead: false,
            },
            TeamStatusTeammate {
                name: "user2".into(),
                is_team_lead: false,
            },
        ];
        let display = render_team_status(&teammates, false, true).unwrap();
        assert_eq!(display.text, "2 teammates");
        assert!(display.hint.is_none());
    }
}
