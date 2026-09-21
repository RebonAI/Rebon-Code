//! What an environment serves: its projects, and the controller's view of
//! an environment.
//!
//! One environment is one **machine**, not one checkout. A machine
//! advertises the projects it is willing to run sessions in, and a
//! controller picks one of them when it queues work (see
//! [`crate::config::EnqueueWorkRequest`]). RC refuses work for a project
//! the environment did not advertise, so the list is also the boundary
//! of what a controller can make the machine do.
//!
//! | Route | Body | Credential |
//! |---|---|---|
//! | `POST /v1/environments` | [`crate::config::BridgeConfig`], `projects` optional | device access token |
//! | `PUT /v1/environments/{env}/projects` | [`ProjectList`] → [`ProjectList`] | environment secret |
//! | `GET /v1/environments` | → [`EnvironmentList`] | device access token |
//!
//! ## Identity of a project
//!
//! A project is identified by its `path`, exactly as the machine spells
//! it — `/home/me/src/app` or `C:\src\app`. RC compares paths as opaque
//! strings: it never normalises separators, case or trailing slashes,
//! because it cannot know the machine's filesystem rules. A controller
//! must therefore send back a `path` it read from the list, byte for
//! byte. `label`, `remote` and `branch` are display data.
//!
//! These types are defined once, here, and are pure data.

use serde::{Deserialize, Serialize};

/// Most projects one environment may advertise.
pub const MAX_PROJECTS: usize = 256;

/// One project an environment can run sessions in.
///
/// Every key is a single word, so the shape is the same inside the
/// camelCase [`crate::config::BridgeConfig`] and in the snake_case
/// response bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    /// Working directory, as the machine sees it. The project's identity.
    pub path: String,
    /// Human-readable name for a picker.
    pub label: String,
    /// Git remote URL, when the project is a git checkout with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Checked-out branch at the time the list was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl ProjectInfo {
    /// A project with no git metadata.
    pub fn new(path: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            label: label.into(),
            remote: None,
            branch: None,
        }
    }

    /// A project labelled after the last component of `path`.
    pub fn from_path(path: impl Into<String>) -> Self {
        let path = path.into();
        let label = default_label(&path);
        Self::new(path, label)
    }

    /// Attach a git remote.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = Some(remote.into());
        self
    }

    /// Attach the current branch.
    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = Some(branch.into());
        self
    }
}

/// The last non-empty component of `path`, whichever separator the
/// machine uses; the whole path when it has none.
fn default_label(path: &str) -> String {
    path.split(['/', '\\'])
        .rfind(|component| !component.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Body of `PUT /v1/environments/{env}/projects`, in both directions:
/// the environment sends the complete new list, and RC answers with the
/// list it stored.
///
/// The update **replaces** the list; there is no per-project add or
/// remove. An empty list is allowed and means the machine currently
/// serves nothing — queued work for any project is then refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectList {
    /// Every project the environment serves, in display order.
    pub projects: Vec<ProjectInfo>,
}

/// One row of `GET /v1/environments`. Never carries the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSummary {
    /// Backend-issued environment id.
    pub environment_id: String,
    /// `BridgeConfig.environment_id`, the client's idempotency key.
    pub client_environment_id: String,
    /// Device that registered the environment.
    pub device_id: String,
    /// `BridgeConfig.bridge_id`.
    pub bridge_id: String,
    /// `BridgeConfig.machine_name`.
    pub machine_name: String,
    /// `BridgeConfig.dir` as registered. Informational; `projects` is
    /// what work is checked against.
    pub dir: String,
    /// `BridgeConfig.branch`.
    pub branch: String,
    /// `BridgeConfig.git_repo_url`.
    pub git_repo_url: Option<String>,
    /// `BridgeConfig.worker_type`, opaque to RC.
    pub worker_type: String,
    /// `BridgeConfig.max_sessions`.
    pub max_sessions: i64,
    /// `BridgeConfig.spawn_mode` in its kebab-case wire form.
    pub spawn_mode: String,
    /// The projects the environment currently advertises. Absent from an
    /// older server's answer, which reads as empty.
    #[serde(default)]
    pub projects: Vec<ProjectInfo>,
    /// First registration (RFC 3339).
    pub created_at: String,
    /// Last authenticated request from the environment (RFC 3339).
    pub last_seen_at: String,
    /// Deregistration time, when deregistered (RFC 3339).
    pub deregistered_at: Option<String>,
}

/// Body of `GET /v1/environments`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentList {
    /// The account's environments, oldest registration first.
    pub environments: Vec<EnvironmentSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_project_without_git_metadata_omits_those_keys() {
        let project = ProjectInfo::new("/srv/app", "app");
        assert_eq!(
            serde_json::to_value(&project).expect("serialize"),
            json!({"path": "/srv/app", "label": "app"})
        );
        let full = project
            .clone()
            .with_remote("git@example.com:me/app.git")
            .with_branch("main");
        let value = serde_json::to_value(&full).expect("serialize");
        assert_eq!(
            value,
            json!({
                "path": "/srv/app",
                "label": "app",
                "remote": "git@example.com:me/app.git",
                "branch": "main"
            })
        );
        assert_eq!(
            serde_json::from_value::<ProjectInfo>(value).expect("parse"),
            full
        );
    }

    #[test]
    fn a_label_defaults_to_the_last_path_component() {
        assert_eq!(ProjectInfo::from_path("/home/me/src/app").label, "app");
        assert_eq!(ProjectInfo::from_path("/home/me/src/app/").label, "app");
        assert_eq!(ProjectInfo::from_path(r"C:\src\rebon").label, "rebon");
        assert_eq!(ProjectInfo::from_path("solo").label, "solo");
        assert_eq!(ProjectInfo::from_path("/").label, "/");
    }

    #[test]
    fn a_project_list_round_trips() {
        let list = ProjectList {
            projects: vec![
                ProjectInfo::from_path("/a"),
                ProjectInfo::from_path("/b").with_branch("dev"),
            ],
        };
        let text = serde_json::to_string(&list).expect("serialize");
        assert_eq!(
            text,
            r#"{"projects":[{"path":"/a","label":"a"},{"path":"/b","label":"b","branch":"dev"}]}"#
        );
        assert_eq!(
            serde_json::from_str::<ProjectList>(&text).expect("parse"),
            list
        );
    }

    fn summary_json() -> serde_json::Value {
        json!({
            "environment_id": "env_1",
            "client_environment_id": "client-1",
            "device_id": "dev_1",
            "bridge_id": "bridge-1",
            "machine_name": "workshop",
            "dir": "/srv/app",
            "branch": "main",
            "git_repo_url": null,
            "worker_type": "rebon",
            "max_sessions": 2,
            "spawn_mode": "worktree",
            "created_at": "2026-09-16T00:00:00.000Z",
            "last_seen_at": "2026-09-16T00:00:00.000Z",
            "deregistered_at": null
        })
    }

    #[test]
    fn an_environment_summary_without_projects_still_parses() {
        // What a server from before projects answered.
        let summary: EnvironmentSummary =
            serde_json::from_value(summary_json()).expect("old shape parses");
        assert!(summary.projects.is_empty());
    }

    #[test]
    fn an_environment_summary_round_trips_with_projects() {
        let mut value = summary_json();
        value["projects"] = json!([{"path": "/srv/app", "label": "app"}]);
        let summary: EnvironmentSummary = serde_json::from_value(value.clone()).expect("parse");
        assert_eq!(summary.projects, vec![ProjectInfo::new("/srv/app", "app")]);
        assert_eq!(serde_json::to_value(&summary).expect("serialize"), value);
    }
}
