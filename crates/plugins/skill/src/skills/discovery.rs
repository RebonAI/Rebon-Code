//! Dynamic skill discovery, deduplication, and conditional
//! activation.
//!
//! All functions here are **I/O-free**: they operate on
//! already-loaded data and produce decisions. The actual
//! filesystem walks and stat calls live in the integration layer.

use super::skill_command::SkillCommandDef;

// ---------------------------------------------------------------------------
// SkillWithPath — loader output
// ---------------------------------------------------------------------------

/// A loaded skill paired with the file it was loaded from.
///
/// Used for deduplication (same file reached via different paths
/// should only produce one skill).
#[derive(Debug, Clone)]
pub struct SkillWithPath {
    /// The loaded skill command.
    pub skill: SkillCommandDef,
    /// Absolute path to the SKILL.md or .md file.
    pub file_path: String,
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

/// Result of a deduplication pass.
#[derive(Debug, Clone)]
pub struct DeduplicationResult {
    /// Skills that survived deduplication.
    pub skills: Vec<SkillCommandDef>,
    /// Number of duplicates removed.
    pub duplicates_removed: usize,
}

/// Deduplicate skills by their resolved file identity.
///
/// `file_identities` is a parallel array to `skills_with_paths`:
/// for each entry, it holds `Some(canonical_path)` from realpath
/// or `None` if the file couldn't be resolved. The I/O layer
/// computes these; this function performs the first-wins
/// deduplication.
pub fn deduplicate_by_file_identity(
    skills_with_paths: &[SkillWithPath],
    file_identities: &[Option<String>],
) -> DeduplicationResult {
    debug_assert_eq!(skills_with_paths.len(), file_identities.len());

    let mut seen: Vec<String> = Vec::new();
    let mut result = Vec::new();

    for (i, entry) in skills_with_paths.iter().enumerate() {
        let identity = file_identities.get(i).and_then(|id| id.as_ref());
        match identity {
            Some(id) => {
                if seen.iter().any(|s| s == id) {
                    // Duplicate — skip.
                    continue;
                }
                seen.push(id.clone());
                result.push(entry.skill.clone());
            }
            None => {
                // Couldn't resolve — keep it (no dedup possible).
                result.push(entry.skill.clone());
            }
        }
    }

    let duplicates_removed = skills_with_paths.len() - result.len();
    DeduplicationResult {
        skills: result,
        duplicates_removed,
    }
}

// ---------------------------------------------------------------------------
// Conditional / unconditional split
// ---------------------------------------------------------------------------

/// Split deduplicated skills into unconditional (always visible)
/// and conditional (activated when matching files are touched).
///
/// `already_activated` is the set of skill names that have been
/// previously activated in this session (survives cache clears).
pub fn split_conditional_skills(
    skills: Vec<SkillCommandDef>,
    already_activated: &[String],
) -> SplitResult {
    let mut unconditional = Vec::new();
    let mut conditional = Vec::new();

    for skill in skills {
        let has_paths = skill.paths.as_ref().map(|p| !p.is_empty()).unwrap_or(false);
        let was_activated = already_activated.contains(&skill.name);

        if has_paths && !was_activated {
            conditional.push(skill);
        } else {
            unconditional.push(skill);
        }
    }

    SplitResult {
        unconditional,
        conditional,
    }
}

/// Result of splitting skills into conditional/unconditional.
#[derive(Debug, Clone)]
pub struct SplitResult {
    /// Skills always visible to the model.
    pub unconditional: Vec<SkillCommandDef>,
    /// Skills stored for later activation.
    pub conditional: Vec<SkillCommandDef>,
}

// ---------------------------------------------------------------------------
// Conditional skill activation
// ---------------------------------------------------------------------------

/// Check whether a single skill's path patterns match any of the
/// given file paths.
///
/// Uses simple glob matching (not gitignore-style matching via the
/// `ignore` library). Patterns are matched against paths relative to cwd.
///
/// Returns `true` if any file path matches any pattern.
pub fn skill_matches_paths(skill_patterns: &[String], relative_file_paths: &[&str]) -> bool {
    for file_path in relative_file_paths {
        // Skip paths that escape the base directory.
        if file_path.is_empty() || file_path.starts_with("..") {
            continue;
        }

        for pattern in skill_patterns {
            if glob_match(pattern, file_path) {
                return true;
            }
        }
    }
    false
}

/// Activate conditional skills whose path patterns match the given
/// file paths.
///
/// Returns the names of newly activated skills.
///
/// `conditional_skills` is mutated: matched skills are removed.
/// The caller should move them to the dynamic skills map.
pub fn activate_conditional_skills(
    conditional_skills: &mut Vec<SkillCommandDef>,
    relative_file_paths: &[&str],
) -> Vec<SkillCommandDef> {
    let mut activated = Vec::new();
    let mut remaining = Vec::new();

    for skill in conditional_skills.drain(..) {
        let patterns = skill.paths.as_deref().unwrap_or(&[]);
        if !patterns.is_empty() && skill_matches_paths(patterns, relative_file_paths) {
            activated.push(skill);
        } else {
            remaining.push(skill);
        }
    }

    *conditional_skills = remaining;
    activated
}

// ---------------------------------------------------------------------------
// Discovery path helpers
// ---------------------------------------------------------------------------

/// Given a file path, walk up to (but not including) `cwd` and
/// collect potential `.rebon/skills/` directories to check.
///
/// Returns directory paths sorted deepest first (so skills closer
/// to the file take precedence).
///
/// This is only the *pure logic* portion of discovery.
/// The caller must then stat each directory.
///
/// Both `file_path` and `cwd` should use `/` separators.
pub fn candidate_skill_dirs(file_path: &str, cwd: &str) -> Vec<String> {
    let cwd = cwd.trim_end_matches('/');
    let cwd_prefix = format!("{cwd}/");
    let mut results = Vec::new();

    let mut current = parent_dir(file_path);

    // Walk up until we reach cwd (exclusive).
    while current.starts_with(&cwd_prefix) {
        let skill_dir = format!("{current}/.rebon/skills");
        results.push(skill_dir);
        let parent = parent_dir(current);
        if parent == current {
            break; // Root reached.
        }
        current = parent;
    }

    // Already deepest-first by construction (we started at file).
    results
}

/// Parent directory of a `/`-separated path (no trailing slash).
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(pos) => &path[..pos],
        None => path,
    }
}

// ---------------------------------------------------------------------------
// Simple glob matching
// ---------------------------------------------------------------------------

/// Minimal glob matcher supporting `*` (any non-`/` chars) and
/// `**` (any chars including `/`).
///
/// Gitignore semantics: a pattern without `/` or wildcards is
/// treated as a directory-prefix match (e.g. `src` matches
/// anything under `src/`). This covers the most common skill path
/// patterns. For full gitignore semantics the integration layer
/// should use the `ignore` crate.
fn glob_match(pattern: &str, path: &str) -> bool {
    // Exact match.
    if glob_match_recursive(pattern.as_bytes(), path.as_bytes()) {
        return true;
    }
    // Gitignore-style: a bare directory name matches anything
    // under it. If the pattern has no wildcard and the path starts
    // with `pattern/`, it's a match.
    if !pattern.contains('*') && !pattern.contains('?') {
        let prefix = format!("{pattern}/");
        if path.starts_with(&prefix) {
            return true;
        }
    }
    false
}

fn glob_match_recursive(pattern: &[u8], path: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star_pi, mut star_si) = (usize::MAX, usize::MAX);

    while si < path.len() {
        if pi < pattern.len() && pattern[pi] == b'*' {
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                // `**` — match any chars including `/`.
                // Try matching rest of pattern against rest of path.
                let rest_pattern = if pi + 2 < pattern.len() && pattern[pi + 2] == b'/' {
                    &pattern[pi + 3..]
                } else {
                    &pattern[pi + 2..]
                };
                // Try matching `**` at every position.
                for start in si..=path.len() {
                    if glob_match_recursive(rest_pattern, &path[start..]) {
                        return true;
                    }
                }
                return false;
            }
            // Single `*` — match any non-`/` chars.
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if pi < pattern.len() && (pattern[pi] == path[si] || pattern[pi] == b'?') {
            pi += 1;
            si += 1;
        } else if star_pi != usize::MAX {
            // Backtrack to last `*`.
            pi = star_pi + 1;
            star_si += 1;
            // `*` must not cross `/`.
            if path[star_si - 1] == b'/' {
                return false;
            }
            si = star_si;
        } else {
            return false;
        }
    }

    // Consume trailing `*` / `**`.
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::skill_command::{CommandSource, LoadedFrom, SkillCommandDef};

    fn dummy_skill(name: &str, paths: Option<Vec<String>>) -> SkillCommandDef {
        SkillCommandDef {
            name: name.into(),
            display_name: None,
            description: String::new(),
            has_user_specified_description: false,
            allowed_tools: Vec::new(),
            required_tools: Vec::new(),
            argument_hint: None,
            arg_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            context: None,
            agent: None,
            effort: None,
            paths,
            content_length: 0,
            is_hidden: false,
            source: CommandSource::ProjectSettings,
            loaded_from: LoadedFrom::Skills,
            skill_root: None,
            shell: None,
            hooks: None,
            disable_non_interactive: false,
            plugin_info: None,
            progress_message: "running".into(),
            markdown_content: String::new(),
        }
    }

    // -- deduplication --

    #[test]
    fn dedup_removes_same_identity() {
        let entries = vec![
            SkillWithPath {
                skill: dummy_skill("a", None),
                file_path: "/x/a/SKILL.md".into(),
            },
            SkillWithPath {
                skill: dummy_skill("a-dup", None),
                file_path: "/y/a/SKILL.md".into(),
            },
        ];
        let ids = vec![
            Some("/canonical/a/SKILL.md".into()),
            Some("/canonical/a/SKILL.md".into()), // same identity
        ];
        let result = deduplicate_by_file_identity(&entries, &ids);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.duplicates_removed, 1);
        assert_eq!(result.skills[0].name, "a");
    }

    #[test]
    fn dedup_keeps_unresolved() {
        let entries = vec![SkillWithPath {
            skill: dummy_skill("a", None),
            file_path: "/x/a/SKILL.md".into(),
        }];
        let ids = vec![None]; // couldn't resolve
        let result = deduplicate_by_file_identity(&entries, &ids);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.duplicates_removed, 0);
    }

    // -- conditional split --

    #[test]
    fn split_separates_conditional_and_unconditional() {
        let skills = vec![
            dummy_skill("always", None),
            dummy_skill("conditional", Some(vec!["src/**".into()])),
        ];
        let result = split_conditional_skills(skills, &[]);
        assert_eq!(result.unconditional.len(), 1);
        assert_eq!(result.unconditional[0].name, "always");
        assert_eq!(result.conditional.len(), 1);
        assert_eq!(result.conditional[0].name, "conditional");
    }

    #[test]
    fn split_treats_already_activated_as_unconditional() {
        let skills = vec![dummy_skill("cond", Some(vec!["src/**".into()]))];
        let result = split_conditional_skills(skills, &["cond".into()]);
        assert_eq!(result.unconditional.len(), 1);
        assert!(result.conditional.is_empty());
    }

    // -- glob matching --

    #[test]
    fn glob_match_literal() {
        assert!(glob_match("src/main.rs", "src/main.rs"));
        assert!(!glob_match("src/main.rs", "src/lib.rs"));
    }

    #[test]
    fn glob_match_star() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/sub/main.rs"));
    }

    #[test]
    fn glob_match_double_star() {
        assert!(glob_match("src/**", "src/main.rs"));
        assert!(glob_match("src/**", "src/sub/main.rs"));
        assert!(glob_match("src/**", "src/a/b/c.rs"));
        assert!(!glob_match("src/**", "tests/main.rs"));
    }

    #[test]
    fn glob_match_double_star_with_extension() {
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/sub/lib.rs"));
        assert!(!glob_match("**/*.rs", "src/main.ts"));
    }

    #[test]
    fn glob_match_question_mark() {
        assert!(glob_match("src/?.rs", "src/a.rs"));
        assert!(!glob_match("src/?.rs", "src/ab.rs"));
    }

    // -- conditional activation --

    #[test]
    fn activate_conditional_skills_matches() {
        let mut conditional = vec![
            dummy_skill("ts-skill", Some(vec!["src".into()])),
            dummy_skill("test-skill", Some(vec!["tests".into()])),
        ];
        let activated = activate_conditional_skills(&mut conditional, &["src/app.ts"]);
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0].name, "ts-skill");
        assert_eq!(conditional.len(), 1);
        assert_eq!(conditional[0].name, "test-skill");
    }

    // -- candidate skill dirs --

    #[test]
    fn candidate_skill_dirs_walks_up() {
        let dirs = candidate_skill_dirs("/project/src/components/App.tsx", "/project");
        assert_eq!(
            dirs,
            vec![
                "/project/src/components/.rebon/skills",
                "/project/src/.rebon/skills",
            ]
        );
    }

    #[test]
    fn candidate_skill_dirs_excludes_cwd_level() {
        let dirs = candidate_skill_dirs("/project/file.rs", "/project");
        // Only the file's parent is /project — which equals cwd, so empty.
        assert!(dirs.is_empty());
    }

    #[test]
    fn candidate_skill_dirs_no_false_prefix_match() {
        // /project-backup/file.rs should NOT match cwd=/project
        let dirs = candidate_skill_dirs("/project-backup/src/file.rs", "/project");
        assert!(dirs.is_empty());
    }
}
