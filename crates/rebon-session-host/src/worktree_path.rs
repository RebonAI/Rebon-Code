pub fn should_skip_background_worktree(cwd: &str) -> bool {
    cwd.replace('\\', "/").contains("/.rebon/worktrees/")
}

/// The same test, run against a *sanitized project-directory name* rather than
/// a working directory.
///
/// A background/queue worktree gets a `~/.rebon/projects/<sanitized>` dir of
/// its own the moment a session writes a transcript there. That dir is hidden
/// from the project list by [`should_skip_background_worktree`] — but only
/// while its real path can still be recovered, from a live job record or the
/// `.cwd` sidecar. Once the job is pruned and no sidecar was ever written, the
/// recovered "path" falls back to the sanitized name, which has no slashes
/// left to match, and the worktree resurfaces as a cryptic project the user
/// never opened.
///
/// The name still carries the signature: `sanitize_path` maps every non-alnum
/// character to `-`, so `/.rebon/worktrees/` is always spelled
/// `--rebon-worktrees-`.
pub fn is_background_worktree_dir_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("--rebon-worktrees-")
}

pub fn background_worktree_slug(job_id: &str) -> String {
    // Job ids usually already start with `bg-`; strip the conventional
    // prefix before re-applying it so slugs don't come out `bg-bg-…`.
    let job_id = job_id.strip_prefix("bg-").unwrap_or(job_id);
    let mut slug = String::from("bg-");
    for ch in job_id.chars().take(61) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            slug.push(ch);
        } else {
            slug.push('_');
        }
    }
    slug.truncate(64);
    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_worktree_slug_is_valid_and_bounded() {
        let slug = background_worktree_slug(
            "bg-invalid/path with spaces and a very very very very very very long suffix",
        );
        assert!(slug.starts_with("bg-invalid_path_with_spaces"));
        assert!(!slug.starts_with("bg-bg-"));
        assert!(slug.len() <= 64);
        assert!(slug
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
    }

    #[test]
    fn background_worktree_skip_detects_nested_worktree_path() {
        assert!(should_skip_background_worktree(
            "F:/repo/.rebon/worktrees/bg-job"
        ));
        assert!(should_skip_background_worktree(
            "F:\\repo\\.rebon\\worktrees\\bg-job"
        ));
        assert!(!should_skip_background_worktree("F:/repo/project"));
    }

    #[test]
    fn sanitized_worktree_dir_name_is_recognized_without_its_cwd() {
        // What `project_dir_component` writes for
        // `C:\repo\rebon\.rebon\worktrees\bg-19f5` on Windows and on Unix.
        assert!(is_background_worktree_dir_name(
            "c--repo-rebon--rebon-worktrees-bg-19f5"
        ));
        assert!(is_background_worktree_dir_name(
            "-home-u-rebon--rebon-worktrees-bg-19f5"
        ));
        // Dirs written before case normalization keep their original spelling.
        assert!(is_background_worktree_dir_name(
            "F--dev-rebon--rebon-worktrees-bg-bg-19e3"
        ));

        // The origin repo itself, and a project that merely mentions rebon.
        assert!(!is_background_worktree_dir_name("f--dev-rebon"));
        assert!(!is_background_worktree_dir_name(
            "f--dev-rebon-worktrees-demo"
        ));
        // Claude Code's own worktrees are not ours to hide.
        assert!(!is_background_worktree_dir_name(
            "f--dev-rebon--claude-worktrees-agent-a13"
        ));
    }
}
