//! The projects this machine advertises.
//!
//! They come from two places: `rc.projects` in the config file (read by
//! the binary, which owns that file, and handed in as a
//! [`ProjectSource`]) and `--project` on the command line. With neither,
//! the directory `rebon rc serve` was started in is the one project.
//!
//! A project's identity on the wire is its path, compared byte for byte,
//! so every path is resolved once, here, to the spelling the session host
//! uses for the same directory (absolute, links resolved, no `\\?\`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use rebon_bridge::projects::{ProjectInfo, MAX_PROJECTS};

/// One entry of `rc.projects`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredProject {
    pub path: String,
    pub label: Option<String>,
}

/// Reads `rc.projects` from the config file, afresh on every call.
pub type ProjectSource = Arc<dyn Fn() -> anyhow::Result<Vec<ConfiguredProject>> + Send + Sync>;

/// The advertised list: configured entries, then command-line ones, then
/// the fallback directory when there are none. A path that does not name
/// a directory fails the whole list: serving a project that does not exist
/// would queue work that can never run.
pub fn resolve_projects(
    configured: Vec<ConfiguredProject>,
    flags: &[PathBuf],
    fallback: &Path,
) -> anyhow::Result<Vec<ProjectInfo>> {
    let mut entries: Vec<(PathBuf, Option<String>)> = configured
        .into_iter()
        .map(|entry| (PathBuf::from(entry.path), entry.label))
        .collect();
    entries.extend(flags.iter().map(|path| (path.clone(), None)));
    if entries.is_empty() {
        entries.push((fallback.to_path_buf(), None));
    }
    let mut projects: Vec<ProjectInfo> = Vec::new();
    for (path, label) in entries {
        let resolved = canonical_dir(&path)?;
        let spelled = resolved.to_string_lossy().into_owned();
        if projects.iter().any(|project| project.path == spelled) {
            continue;
        }
        let mut project = match label.filter(|label| !label.trim().is_empty()) {
            Some(label) => ProjectInfo::new(spelled, label.trim()),
            None => ProjectInfo::from_path(spelled),
        };
        project.branch = current_branch(&resolved);
        projects.push(project);
    }
    if projects.len() > MAX_PROJECTS {
        anyhow::bail!(
            "{} projects are configured; an environment advertises at most {MAX_PROJECTS}",
            projects.len()
        );
    }
    Ok(projects)
}

/// The paths of `projects`, as work items are checked against them.
pub fn project_paths(projects: &[ProjectInfo]) -> Vec<String> {
    projects
        .iter()
        .map(|project| project.path.clone())
        .collect()
}

fn canonical_dir(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = rebon_tools_core::strip_windows_verbatim_prefix(
        std::fs::canonicalize(path)
            .with_context(|| format!("project {} does not exist", path.display()))?,
    );
    if !canonical.is_dir() {
        anyhow::bail!("project {} is not a directory", canonical.display());
    }
    Ok(canonical)
}

/// The checked-out branch, read from `.git/HEAD` without running git. A
/// detached head, a worktree's `.git` file, or no repository at all is
/// simply no branch: it is display data.
fn current_branch(dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(dir.join(".git").join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .filter(|branch| !branch.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spelled(path: &Path) -> String {
        canonical_dir(path).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn with_nothing_configured_the_start_directory_is_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let projects = resolve_projects(Vec::new(), &[], dir.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, spelled(dir.path()));
        assert!(!projects[0].path.starts_with(r"\\?\"));
        assert_eq!(projects[0].branch, None);
    }

    #[test]
    fn configured_and_flagged_projects_are_merged_in_order() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("app");
        let docs = root.path().join("docs");
        std::fs::create_dir_all(app.join(".git")).unwrap();
        std::fs::write(app.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        let projects = resolve_projects(
            vec![ConfiguredProject {
                path: app.to_string_lossy().into_owned(),
                label: Some(" App ".into()),
            }],
            // The same directory spelled another way is one project.
            &[docs.clone(), app.join(".").join("..").join("app")],
            root.path(),
        )
        .unwrap();
        assert_eq!(
            project_paths(&projects),
            vec![spelled(&app), spelled(&docs)]
        );
        assert_eq!(projects[0].label, "App");
        assert_eq!(projects[0].branch.as_deref(), Some("main"));
        assert_eq!(projects[1].label, "docs");
    }

    #[test]
    fn a_missing_directory_fails_the_list() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        for bad in [root.path().join("absent"), file] {
            assert!(
                resolve_projects(Vec::new(), std::slice::from_ref(&bad), root.path()).is_err(),
                "{}",
                bad.display()
            );
        }
    }

    #[test]
    fn a_detached_head_has_no_branch() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git").join("HEAD"), "0123abcd\n").unwrap();
        assert_eq!(current_branch(root.path()), None);
    }
}
