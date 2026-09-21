//! Dialog chrome projection for permission prompts.
//!
//! Implements:
//! * title/subtitle/worker badge header.
//! * permission dialog border defaults and header composition.
//! * `@name` badge projection.
//! * pending approval box copy and optional team metadata.

use crate::types::ThemeColor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionSubtitle {
    Plain(String),
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionSubtitleView {
    PlainTruncateStart { text: String },
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBadge {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBadgeView {
    pub text: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequestTitleView {
    pub title: String,
    pub color: ThemeColor,
    pub worker_suffix: Option<String>,
    pub subtitle: Option<PermissionSubtitleView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDialogView {
    pub border_color: ThemeColor,
    pub margin_top: u16,
    pub inner_padding_x: u16,
    pub hide_left_border: bool,
    pub hide_right_border: bool,
    pub hide_bottom_border: bool,
    pub title: PermissionRequestTitleView,
    pub has_title_right: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerPendingPermissionView {
    pub border_color: ThemeColor,
    pub heading: String,
    pub worker_badge: Option<WorkerBadgeView>,
    pub tool_line: String,
    pub action_line: String,
    pub team_notice: Option<String>,
}

pub fn worker_badge_view(worker_badge: &WorkerBadge) -> WorkerBadgeView {
    WorkerBadgeView {
        text: format!("● @{}", worker_badge.name),
        color: worker_badge.color.clone(),
    }
}

pub fn permission_request_title_view(
    title: &str,
    subtitle: Option<PermissionSubtitle>,
    color: Option<ThemeColor>,
    worker_badge: Option<&WorkerBadge>,
) -> PermissionRequestTitleView {
    PermissionRequestTitleView {
        title: title.to_string(),
        color: color.unwrap_or(ThemeColor::Permission),
        worker_suffix: worker_badge.map(|worker_badge| format!("· @{}", worker_badge.name)),
        subtitle: subtitle.map(|subtitle| match subtitle {
            PermissionSubtitle::Plain(text) => PermissionSubtitleView::PlainTruncateStart { text },
            PermissionSubtitle::Custom => PermissionSubtitleView::Custom,
        }),
    }
}

pub fn permission_dialog_view(
    title: &str,
    subtitle: Option<PermissionSubtitle>,
    border_color: Option<ThemeColor>,
    title_color: Option<ThemeColor>,
    inner_padding_x: Option<u16>,
    worker_badge: Option<&WorkerBadge>,
    has_title_right: bool,
) -> PermissionDialogView {
    PermissionDialogView {
        border_color: border_color.unwrap_or(ThemeColor::Permission),
        margin_top: 1,
        inner_padding_x: inner_padding_x.unwrap_or(1),
        hide_left_border: true,
        hide_right_border: true,
        hide_bottom_border: true,
        title: permission_request_title_view(title, subtitle, title_color, worker_badge),
        has_title_right,
    }
}

pub fn worker_pending_permission_view(
    tool_name: &str,
    description: &str,
    team_name: Option<&str>,
    agent_name: Option<&str>,
    agent_color: Option<&str>,
) -> WorkerPendingPermissionView {
    let worker_badge = match (agent_name, agent_color) {
        (Some(agent_name), Some(agent_color)) => Some(worker_badge_view(&WorkerBadge {
            name: agent_name.to_string(),
            color: agent_color.to_string(),
        })),
        _ => None,
    };

    WorkerPendingPermissionView {
        border_color: ThemeColor::Warning,
        heading: "Waiting for team lead approval".to_string(),
        worker_badge,
        tool_line: format!("Tool: {tool_name}"),
        action_line: format!("Action: {description}"),
        team_notice: team_name
            .map(|team_name| format!("Permission request sent to team \"{team_name}\" leader")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_badge_projects_circle_and_name() {
        let badge = worker_badge_view(&WorkerBadge {
            name: "alpha".to_string(),
            color: "blue".to_string(),
        });
        assert_eq!(badge.text, "● @alpha");
        assert_eq!(badge.color, "blue");
    }

    #[test]
    fn title_view_defaults_to_permission_color_and_truncates_plain_subtitle() {
        let title = permission_request_title_view(
            "Edit file",
            Some(PermissionSubtitle::Plain("/very/long/path".to_string())),
            None,
            Some(&WorkerBadge {
                name: "worker-1".to_string(),
                color: "green".to_string(),
            }),
        );
        assert_eq!(title.color, ThemeColor::Permission);
        assert_eq!(title.worker_suffix, Some("· @worker-1".to_string()));
        assert_eq!(
            title.subtitle,
            Some(PermissionSubtitleView::PlainTruncateStart {
                text: "/very/long/path".to_string()
            })
        );
    }

    #[test]
    fn dialog_view_uses_border_and_padding_defaults() {
        let dialog = permission_dialog_view(
            "Edit file",
            None,
            None,
            Some(ThemeColor::Warning),
            None,
            None,
            true,
        );
        assert_eq!(dialog.border_color, ThemeColor::Permission);
        assert_eq!(dialog.title.color, ThemeColor::Warning);
        assert_eq!(dialog.inner_padding_x, 1);
        assert!(dialog.has_title_right);
        assert!(dialog.hide_left_border);
        assert!(dialog.hide_right_border);
        assert!(dialog.hide_bottom_border);
    }

    #[test]
    fn worker_pending_view_keeps_optional_team_metadata() {
        let view = worker_pending_permission_view(
            "Bash",
            "npm test",
            Some("frontend"),
            Some("worker-a"),
            Some("magenta"),
        );
        assert_eq!(view.border_color, ThemeColor::Warning);
        assert_eq!(view.tool_line, "Tool: Bash");
        assert_eq!(view.action_line, "Action: npm test");
        assert_eq!(
            view.team_notice,
            Some("Permission request sent to team \"frontend\" leader".to_string())
        );
        assert_eq!(
            view.worker_badge.expect("badge").text,
            "● @worker-a".to_string()
        );
    }
}
