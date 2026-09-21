//! File-based skill naming and namespace resolution.
//!
//! These are pure string/path operations that work on
//! already-loaded directory and file path data.

// ---------------------------------------------------------------------------
// Skills path resolution
// ---------------------------------------------------------------------------

/// Returns the canonical skills (or commands) directory path for a
/// given source.
///
/// `managed_base` = the managed file path root.
/// `config_home`  = configuration home directory path.
pub fn get_skills_path(
    source: SkillPathSource,
    dir: SkillPathDir,
    managed_base: &str,
    config_home: &str,
) -> String {
    let dir_name = match dir {
        SkillPathDir::Skills => "skills",
        SkillPathDir::Commands => "commands",
    };
    match source {
        SkillPathSource::PolicySettings => {
            format!("{managed_base}/.rebon/{dir_name}")
        }
        SkillPathSource::UserSettings => {
            format!("{config_home}/{dir_name}")
        }
        SkillPathSource::ProjectSettings => {
            format!(".rebon/{dir_name}")
        }
        SkillPathSource::Plugin => "plugin".to_string(),
    }
}

/// Which source to resolve a skills path for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPathSource {
    /// Policy-managed.
    PolicySettings,
    /// User-level.
    UserSettings,
    /// Project-level.
    ProjectSettings,
    /// Plugin.
    Plugin,
}

/// Which directory type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPathDir {
    /// Modern skills directory.
    Skills,
    /// Legacy commands directory.
    Commands,
}

// ---------------------------------------------------------------------------
// Skill file detection
// ---------------------------------------------------------------------------

/// Check if a filename is a SKILL.md file (case-insensitive).
pub fn is_skill_file(filename: &str) -> bool {
    filename.eq_ignore_ascii_case("skill.md")
}

// ---------------------------------------------------------------------------
// Namespace building
// ---------------------------------------------------------------------------

/// Build a colon-separated namespace from the relative path
/// between `target_dir` and `base_dir`.
///
/// Both paths should use `/` as separator (pre-normalized).
pub fn build_namespace(target_dir: &str, base_dir: &str) -> String {
    let base = base_dir.trim_end_matches('/');
    if target_dir == base {
        return String::new();
    }
    let relative = target_dir
        .strip_prefix(base)
        .and_then(|r| r.strip_prefix('/'));
    match relative {
        Some(r) if !r.is_empty() => r.replace('/', ":"),
        _ => String::new(),
    }
}

/// Get the command name for a SKILL.md file.
///
/// The name comes from the parent directory of SKILL.md, prefixed
/// by the namespace from the base directory.
pub fn get_skill_command_name(skill_file_path: &str, base_dir: &str) -> String {
    let skill_directory = parent_path(skill_file_path);
    let parent_of_skill_dir = parent_path(skill_directory);
    let command_base_name = basename(skill_directory);

    let ns = build_namespace(parent_of_skill_dir, base_dir);
    if ns.is_empty() {
        command_base_name.to_string()
    } else {
        format!("{ns}:{command_base_name}")
    }
}

/// Get the command name for a regular `.md` file (not SKILL.md).
///
/// The name comes from the filename without `.md`, prefixed by
/// the namespace.
pub fn get_regular_command_name(file_path: &str, base_dir: &str) -> String {
    let file_name = basename(file_path);
    let file_directory = parent_path(file_path);
    let command_base_name = file_name.strip_suffix(".md").unwrap_or(file_name);

    let ns = build_namespace(file_directory, base_dir);
    if ns.is_empty() {
        command_base_name.to_string()
    } else {
        format!("{ns}:{command_base_name}")
    }
}

/// Get the command name for a file, dispatching between SKILL.md
/// and regular .md naming.
pub fn get_command_name(file_path: &str, base_dir: &str) -> String {
    if is_skill_file(basename(file_path)) {
        get_skill_command_name(file_path, base_dir)
    } else {
        get_regular_command_name(file_path, base_dir)
    }
}

// ---------------------------------------------------------------------------
// Legacy /commands/ skill file transform
// ---------------------------------------------------------------------------

/// Represents a loaded markdown file from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownFileEntry {
    /// Base directory the file was discovered from.
    pub base_dir: String,
    /// Full path to the file.
    pub file_path: String,
}

/// Group files by directory and, when a SKILL.md exists in a
/// directory, keep only that file (discarding siblings).
pub fn transform_skill_files(files: Vec<MarkdownFileEntry>) -> Vec<MarkdownFileEntry> {
    // Group by parent directory.
    let mut by_dir: Vec<(String, Vec<MarkdownFileEntry>)> = Vec::new();
    for file in files {
        let dir = parent_path(&file.file_path).to_string();
        if let Some(entry) = by_dir.iter_mut().find(|(d, _)| *d == dir) {
            entry.1.push(file);
        } else {
            by_dir.push((dir, vec![file]));
        }
    }

    let mut result = Vec::new();
    for (_dir, dir_files) in by_dir {
        let skill_files: Vec<&MarkdownFileEntry> = dir_files
            .iter()
            .filter(|f| is_skill_file(basename(&f.file_path)))
            .collect();
        if let Some(first) = skill_files.first() {
            // When SKILL.md exists, use only that.
            result.push((*first).clone());
        } else {
            result.extend(dir_files);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Path helpers (forward-slash based, no OS dependency)
// ---------------------------------------------------------------------------

/// Extract the parent path (everything before the last `/`).
fn parent_path(path: &str) -> &str {
    // Normalize backslashes for Windows paths.
    match path.rfind('/').or_else(|| path.rfind('\\')) {
        Some(pos) => &path[..pos],
        None => path,
    }
}

/// Extract the basename (everything after the last `/`).
fn basename(path: &str) -> &str {
    match path.rfind('/').or_else(|| path.rfind('\\')) {
        Some(pos) => &path[pos + 1..],
        None => path,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_skill_file_case_insensitive() {
        assert!(is_skill_file("SKILL.md"));
        assert!(is_skill_file("skill.md"));
        assert!(is_skill_file("Skill.MD"));
        assert!(!is_skill_file("README.md"));
        assert!(!is_skill_file("skill.txt"));
    }

    #[test]
    fn build_namespace_empty_at_base() {
        assert_eq!(
            build_namespace("/home/user/skills", "/home/user/skills"),
            ""
        );
    }

    #[test]
    fn build_namespace_one_level() {
        assert_eq!(
            build_namespace("/home/user/skills/sub", "/home/user/skills"),
            "sub",
        );
    }

    #[test]
    fn build_namespace_nested() {
        assert_eq!(
            build_namespace("/home/user/skills/a/b/c", "/home/user/skills"),
            "a:b:c",
        );
    }

    #[test]
    fn get_skill_command_name_simple() {
        assert_eq!(
            get_skill_command_name("/skills/my-skill/SKILL.md", "/skills"),
            "my-skill",
        );
    }

    #[test]
    fn get_skill_command_name_nested() {
        assert_eq!(
            get_skill_command_name("/skills/group/my-skill/SKILL.md", "/skills"),
            "group:my-skill",
        );
    }

    #[test]
    fn get_regular_command_name_simple() {
        assert_eq!(
            get_regular_command_name("/commands/review.md", "/commands"),
            "review",
        );
    }

    #[test]
    fn get_regular_command_name_nested() {
        assert_eq!(
            get_regular_command_name("/commands/ci/deploy.md", "/commands"),
            "ci:deploy",
        );
    }

    #[test]
    fn get_command_name_dispatches_correctly() {
        assert_eq!(get_command_name("/skills/foo/SKILL.md", "/skills"), "foo",);
        assert_eq!(get_command_name("/commands/bar.md", "/commands"), "bar",);
    }

    #[test]
    fn transform_skill_files_keeps_skill_md_only() {
        let files = vec![
            MarkdownFileEntry {
                base_dir: "/skills".into(),
                file_path: "/skills/my-skill/SKILL.md".into(),
            },
            MarkdownFileEntry {
                base_dir: "/skills".into(),
                file_path: "/skills/my-skill/README.md".into(),
            },
        ];
        let result = transform_skill_files(files);
        assert_eq!(result.len(), 1);
        assert!(result[0].file_path.ends_with("SKILL.md"));
    }

    #[test]
    fn transform_skill_files_keeps_all_when_no_skill_md() {
        let files = vec![
            MarkdownFileEntry {
                base_dir: "/cmds".into(),
                file_path: "/cmds/a.md".into(),
            },
            MarkdownFileEntry {
                base_dir: "/cmds".into(),
                file_path: "/cmds/b.md".into(),
            },
        ];
        let result = transform_skill_files(files);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn get_skills_path_variants() {
        assert_eq!(
            get_skills_path(
                SkillPathSource::PolicySettings,
                SkillPathDir::Skills,
                "/managed",
                "/home/.rebon",
            ),
            "/managed/.rebon/skills",
        );
        assert_eq!(
            get_skills_path(
                SkillPathSource::UserSettings,
                SkillPathDir::Skills,
                "/managed",
                "/home/.rebon",
            ),
            "/home/.rebon/skills",
        );
        assert_eq!(
            get_skills_path(
                SkillPathSource::ProjectSettings,
                SkillPathDir::Commands,
                "/managed",
                "/home/.rebon",
            ),
            ".rebon/commands",
        );
        assert_eq!(
            get_skills_path(
                SkillPathSource::Plugin,
                SkillPathDir::Skills,
                "/managed",
                "/home/.rebon",
            ),
            "plugin",
        );
    }

    #[test]
    fn build_namespace_trailing_slash_on_base() {
        assert_eq!(build_namespace("/skills/sub", "/skills/"), "sub",);
    }
}
