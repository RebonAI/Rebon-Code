//! Canonical discovery of instruction markdown files auto-injected into prompts.
//!
//! This module is the single source of truth for REBON.md-style instruction files:
//! global user instructions, project instructions discovered while walking from the
//! filesystem root to the current working directory, private cwd-local instructions,
//! `.rebon/rules/**/*.md`, and markdown `@` includes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::frontmatter::{normalize_frontmatter_paths, parse_frontmatter};
use crate::memory_file::MemoryFileInfo;
use crate::memory_type::MemoryType;

const MAX_INCLUDE_DEPTH: usize = 10;

/// Discover the instruction files that should be auto-injected for `cwd`.
///
/// `MEMORY.md` auto-memory is intentionally not returned here: it is a different
/// feature with its own switch, its own truncation and its own cache semantics,
/// and it lives in the `memory` plugin. These files load whether that plugin is
/// on or off.
pub fn discover_instruction_files(cwd: impl AsRef<Path>) -> Vec<MemoryFileInfo> {
    discover_instruction_files_with_home(
        cwd,
        rebon_session::config_home_with_env(|name| std::env::var_os(name)),
    )
}

/// Discover instruction files with an explicit home directory override.
///
/// This is primarily useful for callers/tests that need deterministic home
/// resolution without mutating process-global environment variables.
pub fn discover_instruction_files_with_home(
    cwd: impl AsRef<Path>,
    home: Option<PathBuf>,
) -> Vec<MemoryFileInfo> {
    let cwd = cwd.as_ref();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(home) = home.as_ref() {
        let global = home.join("REBON.md");
        push_file_with_includes(
            &global,
            MemoryType::User,
            None,
            Some(home.as_path()),
            &mut out,
            &mut seen,
            0,
        );
    }

    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
    dirs.reverse();

    if let Some(home) = home.as_ref() {
        if cwd.starts_with(home) {
            dirs.retain(|dir| dir.starts_with(home) && dir != home);
        } else if let Some(home_parent) = home.parent().filter(|parent| cwd.starts_with(parent)) {
            dirs.retain(|dir| dir.starts_with(home_parent) && dir != home_parent);
        } else {
            dirs.retain(|dir| !dir.starts_with(home));
        }
    }

    for dir in dirs {
        push_file_with_includes(
            &dir.join("REBON.md"),
            MemoryType::Project,
            None,
            home.as_deref(),
            &mut out,
            &mut seen,
            0,
        );
        push_file_with_includes(
            &dir.join(".rebon").join("REBON.md"),
            MemoryType::Project,
            None,
            home.as_deref(),
            &mut out,
            &mut seen,
            0,
        );
        for rule in rule_files(&dir.join(".rebon").join("rules")) {
            if is_eager_rule_file(&rule) {
                push_file_with_includes(
                    &rule,
                    MemoryType::Project,
                    None,
                    home.as_deref(),
                    &mut out,
                    &mut seen,
                    0,
                );
            }
        }
    }

    push_file_with_includes(
        &cwd.join("REBON.local.md"),
        MemoryType::Local,
        None,
        home.as_deref(),
        &mut out,
        &mut seen,
        0,
    );

    out
}

fn push_file_with_includes(
    path: &Path,
    memory_type: MemoryType,
    parent: Option<&Path>,
    home: Option<&Path>,
    out: &mut Vec<MemoryFileInfo>,
    seen: &mut HashSet<PathBuf>,
    depth: usize,
) {
    if depth > MAX_INCLUDE_DEPTH || !is_markdown(path) {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let canonical = path.to_path_buf();
    let seen_key = normalized_path_key(path);
    if !seen.insert(seen_key) {
        return;
    }

    // Include policy: relative includes are resolved against the including file and
    // allowed for all instruction types. Absolute or home-relative includes are only
    // allowed from user/global instructions. Project/local discovery is synchronous
    // and cannot ask for permission, so external paths are deliberately rejected;
    // supporting them requires an approved-path decision supplied by the caller.
    for include in find_includes(&raw) {
        if let Some(include_path) =
            resolve_include(&include, path.parent().unwrap_or(Path::new("")), home)
        {
            let is_external = include.starts_with("~/")
                || include.starts_with("~\\")
                || include.starts_with('/')
                || looks_like_windows_absolute(&include);
            if memory_type == MemoryType::User || !is_external {
                push_file_with_includes(
                    &include_path,
                    memory_type.clone(),
                    Some(&canonical),
                    home,
                    out,
                    seen,
                    depth + 1,
                );
            }
        }
    }

    let parsed = parse_frontmatter(&raw);
    let without_frontmatter = parsed.body;
    let content = strip_html_comments_outside_fences(&without_frontmatter);
    let changed = content != raw;

    let mut info =
        MemoryFileInfo::new(canonical.to_string_lossy(), memory_type.clone()).with_content(content);
    info.parent = parent.map(|p| p.to_string_lossy().into_owned());
    let normalized_paths = normalize_frontmatter_paths(parsed.frontmatter.paths);
    if let Some(globs) = normalized_paths {
        info.globs = Some(globs);
    }
    info.content_differs_from_disk = changed;
    info.raw_content = Some(raw);
    out.push(info);
}

fn normalized_path_key(path: &Path) -> PathBuf {
    let normalized = std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize_path(path));
    normalize_path_case(normalized)
}

fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let can_pop = normalized
                    .components()
                    .next_back()
                    .map(|last| {
                        !matches!(
                            last,
                            std::path::Component::RootDir
                                | std::path::Component::Prefix(_)
                                | std::path::Component::ParentDir
                        )
                    })
                    .unwrap_or(false);
                if can_pop {
                    normalized.pop();
                } else {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(windows)]
fn normalize_path_case(path: PathBuf) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/").to_lowercase())
}

#[cfg(not(windows))]
fn normalize_path_case(path: PathBuf) -> PathBuf {
    path
}

fn is_eager_rule_file(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    normalize_frontmatter_paths(parse_frontmatter(&raw).frontmatter.paths).is_none()
}

fn rule_files(rules_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rule_files(rules_dir, &mut files);
    files.sort();
    files
}

fn collect_rule_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rule_files(&path, files);
        } else if is_markdown(&path) {
            files.push(path);
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
        .unwrap_or(false)
}

fn resolve_include(spec: &str, base_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let path = if let Some(rest) = spec.strip_prefix("~/").or_else(|| spec.strip_prefix("~\\")) {
        home?.join(rest)
    } else if spec.starts_with('/') || looks_like_windows_absolute(spec) {
        PathBuf::from(spec)
    } else {
        base_dir.join(spec)
    };
    is_markdown(&path).then_some(path)
}

fn looks_like_windows_absolute(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn find_includes(input: &str) -> Vec<String> {
    let mut includes = Vec::new();
    let mut in_fence = false;
    for line in input.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut in_inline_code = false;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                in_inline_code = !in_inline_code;
                i += 1;
                continue;
            }
            if !in_inline_code && bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace())
            {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                    end += 1;
                }
                let token = line[start..end]
                    .trim_end_matches(|c: char| matches!(c, ')' | ']' | '}' | ',' | ';' | '.'));
                if token.ends_with(".md") || token.ends_with(".markdown") {
                    includes.push(token.to_string());
                }
                i = end;
            } else {
                i += 1;
            }
        }
    }
    includes
}

fn strip_html_comments_outside_fences(input: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut in_comment = false;

    for segment in input.split_inclusive('\n') {
        let line_no_newline = segment.trim_end_matches('\n').trim_end_matches('\r');
        let trimmed = line_no_newline.trim_start();
        if !in_comment && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            in_fence = !in_fence;
            out.push_str(segment);
            continue;
        }
        if in_fence {
            out.push_str(segment);
            continue;
        }

        let mut rest = segment;
        loop {
            if in_comment {
                if let Some(end) = rest.find("-->") {
                    rest = &rest[end + 3..];
                    in_comment = false;
                } else {
                    break;
                }
            } else if let Some(start) = rest.find("<!--") {
                out.push_str(&rest[..start]);
                rest = &rest[start + 4..];
                in_comment = true;
            } else {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn discover(cwd: &Path, home: &Path) -> Vec<MemoryFileInfo> {
        discover_instruction_files_with_home(cwd, Some(home.to_path_buf()))
    }

    /// Cross-test mutex so HOME / USERPROFILE / REBON_CONFIG_DIR manipulations serialise.
    fn env_test_lock() -> &'static std::sync::Mutex<()> {
        crate::test_env::env_test_lock()
    }

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
        prev_rebon_config_dir: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new(home: &Path, config_home: Option<&Path>) -> Self {
            let _lock = env_test_lock().lock().unwrap_or_else(|p| p.into_inner());
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            let prev_rebon_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("HOME", home);
            std::env::set_var("USERPROFILE", home);
            match config_home {
                Some(config_home) => std::env::set_var("REBON_CONFIG_DIR", config_home),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
            Self {
                _lock,
                prev_home,
                prev_userprofile,
                prev_rebon_config_dir,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev_rebon_config_dir.take() {
                Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_userprofile.take() {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    #[test]
    fn discover_instruction_files_global_honors_rebon_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let config_home = tmp.path().join("config-home");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&config_home).unwrap();
        let cwd = config_home.join("repo");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(home.join(".rebon")).unwrap();
        fs::write(home.join(".rebon/REBON.md"), "home global").unwrap();
        fs::write(config_home.join("REBON.md"), "config global").unwrap();
        let _guard = EnvGuard::new(&home, Some(&config_home));

        let files = discover_instruction_files(&cwd);
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();

        assert_eq!(contents, vec!["config global"]);
        assert_eq!(PathBuf::from(&files[0].path), config_home.join("REBON.md"));
    }

    #[test]
    fn discover_instruction_files_global_falls_back_to_home_dot_rebon() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = home.join(".rebon").join("repo");
        fs::create_dir_all(home.join(".rebon")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(home.join(".rebon/REBON.md"), "home global").unwrap();
        let _guard = EnvGuard::new(&home, None);

        let files = discover_instruction_files(&cwd);
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();

        assert_eq!(contents, vec!["home global"]);
        assert_eq!(
            PathBuf::from(&files[0].path),
            home.join(".rebon").join("REBON.md")
        );
    }

    #[test]
    fn discovers_global_project_dot_rules_and_local_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = home.join("repo");
        let child = root.join("child");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(child.join(".rebon/rules/nested")).unwrap();
        fs::create_dir_all(root.join(".rebon")).unwrap();
        fs::write(home.join("REBON.md"), "global").unwrap();
        fs::write(root.join("REBON.md"), "root").unwrap();
        fs::write(root.join(".rebon/REBON.md"), "dot-root").unwrap();
        fs::write(child.join("REBON.md"), "child").unwrap();
        fs::write(child.join(".rebon/rules/nested/rule.md"), "rule").unwrap();
        fs::write(child.join("REBON.local.md"), "local").unwrap();

        let files = discover(&child, &home);
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert_eq!(
            contents,
            vec!["global", "root", "dot-root", "child", "rule", "local"]
        );
        assert_eq!(files[0].r#type, MemoryType::User);
        assert_eq!(files.last().unwrap().r#type, MemoryType::Local);
    }

    #[test]
    fn discovers_parent_rebon_from_child_without_child_instruction_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("repo");
        let child = root.join("src");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("REBON.md"), "root instructions").unwrap();

        let files = discover(&child, &home);
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert!(contents.contains(&"root instructions".to_string()));
        assert_eq!(
            contents
                .iter()
                .filter(|content| content.as_str() == "root instructions")
                .count(),
            1
        );
        assert!(files.iter().any(|file| {
            file.content.trim() == "root instructions"
                && file.path.to_ascii_lowercase().ends_with("rebon.md")
        }));
    }

    #[test]
    fn discovers_parent_dot_rebon_and_rules_from_child_without_child_instruction_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("repo");
        let child = root.join("src");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(root.join(".rebon/rules/nested")).unwrap();
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join(".rebon/REBON.md"), "dot parent").unwrap();
        fs::write(root.join(".rebon/rules/a.md"), "rule a").unwrap();
        fs::write(root.join(".rebon/rules/nested/b.md"), "rule b").unwrap();

        let files = discover(&child, &home);
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert!(contents.contains(&"dot parent".to_string()));
        assert!(contents.contains(&"rule a".to_string()));
        assert!(contents.contains(&"rule b".to_string()));
        let positions: Vec<_> = ["dot parent", "rule a", "rule b"]
            .iter()
            .map(|expected| {
                contents
                    .iter()
                    .position(|content| content == expected)
                    .unwrap()
            })
            .collect();
        assert!(positions.windows(2).all(|window| window[0] < window[1]));
    }

    #[test]
    fn normalized_include_keys_prevent_self_cycle_through_dot_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("p");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join("REBON.md"), "@./REBON.md\nparent").unwrap();

        let files = discover(&cwd, tmp.path());
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert_eq!(contents, vec!["@./REBON.md\nparent"]);
    }

    #[test]
    fn normalized_include_keys_dedupe_equivalent_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("p");
        fs::create_dir_all(cwd.join("sub")).unwrap();
        fs::write(cwd.join("shared.md"), "shared").unwrap();
        fs::write(
            cwd.join("REBON.md"),
            "@sub/../shared.md\n@shared.md\nparent",
        )
        .unwrap();

        let files = discover(&cwd, tmp.path());
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert_eq!(
            contents,
            vec!["shared", "@sub/../shared.md\n@shared.md\nparent"]
        );
        assert_eq!(
            files
                .iter()
                .filter(|f| f.content.trim() == "shared")
                .count(),
            1
        );
    }

    #[test]
    fn external_includes_are_allowed_for_user_but_rejected_for_project() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = home.join("projects").join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let user_abs = tmp.path().join("user_abs.md");
        let project_abs = tmp.path().join("project_abs.md");
        fs::write(&user_abs, "user absolute").unwrap();
        fs::write(home.join("user_home.md"), "user home").unwrap();
        fs::write(&project_abs, "project absolute").unwrap();
        fs::write(home.join("project_home.md"), "project home").unwrap();
        fs::write(
            home.join("REBON.md"),
            format!(
                "@{}\n@~/user_home.md\nglobal",
                user_abs.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        fs::write(
            cwd.join("REBON.md"),
            format!(
                "@{}\n@~/project_home.md\nproject",
                project_abs.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();

        let files = discover(&cwd, &home);
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert!(contents.contains(&"user absolute".to_string()));
        assert!(contents.contains(&"user home".to_string()));
        assert!(contents.iter().any(|content| content.contains("global")));
        assert!(contents.iter().any(|content| content.contains("project")));
        assert!(!contents.contains(&"project absolute".to_string()));
        assert!(!contents.contains(&"project home".to_string()));
    }

    #[test]
    fn parses_paths_frontmatter_and_strips_comments_for_non_rule_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("p");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(
            cwd.join("REBON.md"),
            "---\npaths:\n  - src/**/*.rs\n  - crates/**\n---\nkeep\n<!-- hidden\nblock -->\nvisible",
        )
        .unwrap();
        fs::write(
            cwd.join("REBON.local.md"),
            "---\npaths: [a/**, b/*.rs]\n---\nbody",
        )
        .unwrap();
        let files = discover(&cwd, tmp.path());
        let r = files.iter().find(|f| f.path.ends_with("REBON.md")).unwrap();
        assert_eq!(
            r.globs.as_ref().unwrap(),
            &vec!["src/**/*.rs".to_string(), "crates".to_string()]
        );
        assert_eq!(r.content.trim(), "keep\n\nvisible");
        assert!(r.content_differs_from_disk);
        assert!(r.raw_content.as_ref().unwrap().contains("hidden"));
        let inline = files
            .iter()
            .find(|f| f.path.ends_with("REBON.local.md"))
            .unwrap();
        assert_eq!(
            inline.globs.as_ref().unwrap(),
            &vec!["a".to_string(), "b/*.rs".to_string()]
        );
    }

    #[test]
    fn paths_match_all_becomes_unconditional() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("p");
        fs::create_dir_all(cwd.join(".rebon/rules")).unwrap();
        fs::write(
            cwd.join(".rebon/rules/star.md"),
            "---\npaths: **\n---\nstar",
        )
        .unwrap();
        fs::write(
            cwd.join(".rebon/rules/list.md"),
            "---\npaths: [**]\n---\nlist",
        )
        .unwrap();

        let files = discover(&cwd, tmp.path());
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert!(contents.contains(&"star".to_string()));
        assert!(contents.contains(&"list".to_string()));
        assert!(files.iter().all(|file| file.globs.is_none()));
    }

    #[test]
    fn eager_discovery_skips_conditional_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("p");
        fs::create_dir_all(cwd.join(".rebon/rules")).unwrap();
        fs::write(cwd.join(".rebon/rules/always.md"), "always").unwrap();
        fs::write(
            cwd.join(".rebon/rules/rust.md"),
            "---\npaths: src/**/*.rs\n---\nrust",
        )
        .unwrap();

        let files = discover(&cwd, tmp.path());
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert!(contents.contains(&"always".to_string()));
        assert!(!contents.contains(&"rust".to_string()));
    }

    #[test]
    fn eager_discovery_preserves_order_with_conditional_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = home.join("repo");
        let child = root.join("child");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(child.join(".rebon/rules")).unwrap();
        fs::create_dir_all(root.join(".rebon")).unwrap();
        fs::write(home.join("REBON.md"), "global").unwrap();
        fs::write(root.join("REBON.md"), "root").unwrap();
        fs::write(root.join(".rebon/REBON.md"), "dot-root").unwrap();
        fs::write(child.join("REBON.md"), "child").unwrap();
        fs::write(child.join(".rebon/rules/a.md"), "rule a").unwrap();
        fs::write(
            child.join(".rebon/rules/b.md"),
            "---\npaths: src/**/*.rs\n---\nrule b",
        )
        .unwrap();
        fs::write(child.join(".rebon/rules/c.md"), "rule c").unwrap();
        fs::write(child.join("REBON.local.md"), "local").unwrap();

        let files = discover(&child, &home);
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert_eq!(
            contents,
            vec!["global", "root", "dot-root", "child", "rule a", "rule c", "local"]
        );
    }

    #[test]
    fn conditional_rule_include_does_not_leak_into_eager_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("p");
        fs::create_dir_all(cwd.join(".rebon/rules")).unwrap();
        fs::write(cwd.join(".rebon/rules/always.md"), "always").unwrap();
        fs::write(cwd.join(".rebon/shared.md"), "shared").unwrap();
        fs::write(
            cwd.join(".rebon/rules/rust.md"),
            "---\npaths: src/**/*.rs\n---\n@../shared.md\nrust",
        )
        .unwrap();

        let files = discover(&cwd, tmp.path());
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert!(contents.contains(&"always".to_string()));
        assert!(!contents.contains(&"shared".to_string()));
        assert!(!contents.contains(&"@../shared.md\nrust".to_string()));
    }

    #[test]
    fn includes_are_inserted_before_parent_cycles_and_fences_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("p");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join("a.md"), "A @b.md").unwrap();
        fs::write(cwd.join("b.md"), "B @a.md").unwrap();
        fs::write(cwd.join("ignored.md"), "ignored").unwrap();
        fs::write(cwd.join("REBON.md"), "```\n@ignored.md\n```\n@a.md\nparent").unwrap();
        let files = discover(&cwd, tmp.path());
        let contents: Vec<_> = files.iter().map(|f| f.content.trim().to_string()).collect();
        assert_eq!(
            contents,
            vec!["B @a.md", "A @b.md", "```\n@ignored.md\n```\n@a.md\nparent"]
        );
        let a = files.iter().find(|f| f.path.ends_with("a.md")).unwrap();
        assert!(a
            .parent
            .as_ref()
            .unwrap()
            .to_ascii_lowercase()
            .ends_with("rebon.md"));
        assert!(!files.iter().any(|f| f.path.ends_with("ignored.md")));
    }

    #[test]
    fn strips_html_comments_but_not_inside_code_fences() {
        let input = "before <!-- gone --> after\n```\n<!-- keep -->\n```\n<!-- multi\nline -->done";
        let stripped = strip_html_comments_outside_fences(input);
        assert_eq!(stripped, "before  after\n```\n<!-- keep -->\n```\ndone");
    }

    #[test]
    fn resolves_windowsish_absolute_detection_without_normalizing_text() {
        assert!(looks_like_windows_absolute("C:/Users/me/file.md"));
        assert!(looks_like_windows_absolute("C:\\Users\\me\\file.md"));
        assert!(!looks_like_windows_absolute("folder/file.md"));
    }
}
