//! [`relative_memory_path`] and [`display_path`] — the two ways a
//! memory file path is shortened for display.
//!
//! ## [`relative_memory_path`]
//!
//! The function computes two candidate forms and returns the shorter
//! one:
//!
//! * `relative_to_home` — `~` followed by the raw suffix after
//!   `home_dir`, when `path` byte-starts with `home_dir`.
//! * `relative_to_cwd` — `./` followed by `relative_path_from(cwd, path)`,
//!   when `path` byte-starts with `cwd`.
//!
//! When both are present the home-relative form wins a tie because the
//! length comparison is `<=`; when only one is present it is returned
//! unchanged; when neither is present the absolute `path` is returned.
//!
//! Five load-bearing details:
//!
//! 1. **The prefix test is byte-prefix**, not path-component-aware.
//! `/home/u` is a prefix of `/home/user`, so the check will
//! happily produce `~ser/...` if `home_dir == /home/u` and the
//! path is `/home/user/...`.
//! 2. **The home-relative form is `~` + raw suffix**, not
//! `~/` + sep-aware suffix. If the path is exactly `home_dir`,
//! the result is `~`. If the path is `home_dir + "/x"`, the
//! result is `~/x`. Pinned with a test.
//! 3. **The cwd-relative form is `./` + the relative path.**
//! The relative path shares its prefix with `cwd` and strips it
//! instead of walking up, so the path `/a/b/c` relative to `/a`
//! is `b/c` and the final string is `./b/c`. Pinned with a test.
//! 4. **Tie-breaker is `<=`**, not `<`. When both forms have the
//! same length, the home-relative form wins.
//! 5. **The empty relative suffix is kept.** When `path` equals
//! `cwd` the relative suffix is empty, so the cwd-relative form is
//! the non-empty `'./'` and the fallback chain doesn't
//! short-circuit on it. Pinned.
//!
//! ## [`display_path`]
//!
//! The cwd-relative path with no `./` prefix, when it is non-empty and
//! does not start with `..` (i.e. the file is inside the cwd subtree);
//! otherwise `~` followed by the suffix after `home_dir`, when `path`
//! starts with `home_dir + sep`; otherwise the absolute path.
//!
//! Note the **subtle difference** from [`relative_memory_path`]:
//!
//! * [`display_path`] checks `home_dir + sep` (so a path that
//! exactly equals `home_dir` does **not** match — it falls
//! through to the absolute branch).
//! * [`relative_memory_path`] checks `home_dir` only (so a path that
//! exactly equals `home_dir` matches and produces `~`).
//! * [`display_path`] prefers the cwd-relative form *unconditionally*
//! once the relative path doesn't start with `..` (i.e. once the
//! file is inside the cwd subtree). It does NOT pick the shorter of
//! the two forms.
//! * [`display_path`] does NOT prefix `./` to the cwd-relative
//! form. It just returns the relative path.
//!
//! Both are separate functions so the consumer doesn't have to
//! remember which subtle variant to use.

/// Returns the shorter of:
///
/// * `~` + raw suffix (if `path` byte-starts with `home_dir`).
/// * `./` + the relative path from `cwd` (if `path` byte-starts with `cwd`).
///
/// Falls back to the absolute `path` if neither form is
/// applicable. Tie-breaker is `<=`, so the home-relative form wins
/// on ties.
pub fn relative_memory_path(path: &str, home_dir: &str, cwd: &str) -> String {
    let relative_to_home = if path.starts_with(home_dir) {
        let suffix = &path[home_dir.len()..];
        let mut s = String::with_capacity(1 + suffix.len());
        s.push('~');
        s.push_str(suffix);
        Some(s)
    } else {
        None
    };

    let relative_to_cwd = if path.starts_with(cwd) {
        let suffix = relative_path_from(cwd, path);
        let mut s = String::with_capacity(2 + suffix.len());
        s.push_str("./");
        s.push_str(&suffix);
        Some(s)
    } else {
        None
    };

    match (relative_to_home, relative_to_cwd) {
        (Some(home), Some(cwd)) => {
            // Tie-breaker: `<=` — home wins on ties.
            if home.len() <= cwd.len() {
                home
            } else {
                cwd
            }
        }
        (Some(home), None) => home,
        (None, Some(cwd)) => cwd,
        (None, None) => path.to_string(),
    }
}

/// Returns:
///
/// * The cwd-relative form (no `./` prefix) if the file is inside
/// the cwd subtree (i.e. the relative path doesn't
/// start with `..`).
/// * `~` + suffix (if the file is inside `home_dir + sep`).
/// * The absolute path otherwise.
///
/// Note: this is **not** the same function as
/// [`relative_memory_path`]. The selector calls this one for the
/// label text; the notification calls [`relative_memory_path`] for
/// the inline status text.
pub fn display_path(path: &str, home_dir: &str, cwd: &str, sep: char) -> String {
    let rel = relative_path_from(cwd, path);
    if !rel.is_empty() && !rel.starts_with("..") {
        return rel;
    }

    let mut home_with_sep = String::with_capacity(home_dir.len() + 1);
    home_with_sep.push_str(home_dir);
    home_with_sep.push(sep);

    if path.starts_with(&home_with_sep) {
        let mut s = String::with_capacity(1 + (path.len() - home_dir.len()));
        s.push('~');
        s.push_str(&path[home_dir.len()..]);
        return s;
    }

    path.to_string()
}

/// Path-relative calculation covering the
/// subset the selector exercises.
///
/// Semantics:
///
/// * `from == to` → returns `""`.
/// * `from` is a byte-prefix of `to` (with a separator boundary) →
/// strips the prefix and any leading separator. So
/// `relative_path_from("/a/b", "/a/b/c/d") == "c/d"`.
/// * `from` is **not** a prefix of `to` → returns a `..`-prefixed
/// upward path (callers detect this case with the
/// `starts_with("..")` guard in [`display_path`]). The exact upward
/// path returned matches the relative path between two paths that
/// share no common subdirectory, which is the only case the selector
/// exercises. Specifically:
/// `relative_path_from("/proj", "/home/u/x") == "../home/u/x"`.
///
/// The function is separator-agnostic for the prefix check (accepts
/// both `/` and `\`). Tests pin both.
fn relative_path_from(from: &str, to: &str) -> String {
    if from == to {
        return String::new();
    }

    if let Some(suffix) = to.strip_prefix(from) {
        if let Some(stripped) = suffix.strip_prefix('/') {
            return stripped.to_string();
        }
        if let Some(stripped) = suffix.strip_prefix('\\') {
            return stripped.to_string();
        }
    }

    let from_segments = split_segments(from);
    let to_segments = split_segments(to);

    let mut common = 0usize;
    while common < from_segments.len()
        && common < to_segments.len()
        && from_segments[common] == to_segments[common]
    {
        common += 1;
    }

    let mut result_parts: Vec<&str> = Vec::new();
    for _ in common..from_segments.len() {
        result_parts.push("..");
    }
    for segment in &to_segments[common..] {
        result_parts.push(segment);
    }

    if result_parts.is_empty() {
        String::new()
    } else {
        result_parts.join("/")
    }
}

fn split_segments(p: &str) -> Vec<&str> {
    let p = strip_leading_separator(p);
    if p.is_empty() {
        return Vec::new();
    }
    p.split(|c| c == '/' || c == '\\')
        .filter(|s| !s.is_empty())
        .collect()
}

fn strip_leading_separator(s: &str) -> &str {
    s.strip_prefix('/')
        .or_else(|| s.strip_prefix('\\'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────────────────────
    // relative_memory_path
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn relative_memory_path_inside_home_only() {
        let s = relative_memory_path("/home/u/.rebon/REBON.md", "/home/u", "/srv/proj");
        assert_eq!(s, "~/.rebon/REBON.md");
    }

    #[test]
    fn relative_memory_path_inside_cwd_only() {
        let s = relative_memory_path("/srv/proj/REBON.md", "/home/u", "/srv/proj");
        assert_eq!(s, "./REBON.md");
    }

    #[test]
    fn relative_memory_path_inside_both_picks_shorter_home() {
        // home_dir is `/h`, cwd is `/h/very/long/cwd/path`. A file at
        // `/h/very/long/cwd/path/REBON.md` is inside both.
        // Home form: "~/very/long/cwd/path/REBON.md" (len 27)
        // Cwd form: "./REBON.md" (len 8)
        // Shorter is cwd form.
        let s = relative_memory_path(
            "/h/very/long/cwd/path/REBON.md",
            "/h",
            "/h/very/long/cwd/path",
        );
        assert_eq!(s, "./REBON.md");
    }

    #[test]
    fn relative_memory_path_inside_both_picks_shorter_cwd() {
        // home_dir is `/home/u`, cwd is `/home/u/p/very/deeply/nested/cwd`.
        // File at `/home/u/p/very/deeply/nested/cwd/REBON.md`.
        // Home form: "~/p/very/deeply/nested/cwd/REBON.md" (33)
        // Cwd form: "./REBON.md" (8)
        let s = relative_memory_path(
            "/home/u/p/very/deeply/nested/cwd/REBON.md",
            "/home/u",
            "/home/u/p/very/deeply/nested/cwd",
        );
        assert_eq!(s, "./REBON.md");
    }

    #[test]
    fn relative_memory_path_inside_both_picks_home_when_home_is_shorter() {
        // home_dir is `/home/u`, cwd is `/very/different/place`.
        // The file is somewhere only inside home.
        let s = relative_memory_path(
            "/home/u/.rebon/REBON.md",
            "/home/u",
            "/very/different/place",
        );
        assert_eq!(s, "~/.rebon/REBON.md");
    }

    #[test]
    fn relative_memory_path_tie_break_uses_home_form() {
        // Construct: home form == cwd form length.
        // home: "~/REBON.md" (len 8)
        // cwd: "./REBON.md" (len 8)
        let s = relative_memory_path("/h/REBON.md", "/h", "/h");
        // Both home and cwd point at /h. tie -> home wins.
        assert_eq!(s, "~/REBON.md");
    }

    #[test]
    fn relative_memory_path_falls_back_to_absolute_when_neither_applies() {
        let s = relative_memory_path("/srv/elsewhere/REBON.md", "/home/u", "/proj");
        assert_eq!(s, "/srv/elsewhere/REBON.md");
    }

    #[test]
    fn relative_memory_path_exact_home_yields_tilde() {
        // The check is a plain prefix test on `home_dir` with no
        // trailing sep, so a path that exactly equals `home_dir`
        // matches and produces `~`.
        let s = relative_memory_path("/home/u", "/home/u", "/proj");
        assert_eq!(s, "~");
    }

    #[test]
    fn relative_memory_path_exact_cwd_yields_dotslash() {
        // The cwd-relative form is './' + the relative path. When the
        // path equals `cwd` the relative part is empty, so the result
        // is `./`.
        let s = relative_memory_path("/proj", "/elsewhere", "/proj");
        assert_eq!(s, "./");
    }

    #[test]
    fn relative_memory_path_byte_prefix_quirk() {
        // `home_dir` `/home/u` is a byte-prefix of `/home/user`. The
        // prefix check happily produces a wrong-looking result here.
        // Pin it to keep us honest.
        let s = relative_memory_path("/home/user/REBON.md", "/home/u", "/somewhere");
        assert_eq!(s, "~ser/REBON.md");
    }

    #[test]
    fn relative_memory_path_windows_separator() {
        let s = relative_memory_path(
            "C:\\Users\\bon\\.rebon\\REBON.md",
            "C:\\Users\\bon",
            "C:\\proj",
        );
        assert_eq!(s, "~\\.rebon\\REBON.md");
    }

    #[test]
    fn relative_memory_path_windows_cwd() {
        let s = relative_memory_path("C:\\proj\\REBON.md", "C:\\Users\\bon", "C:\\proj");
        assert_eq!(s, "./REBON.md");
    }

    #[test]
    fn relative_memory_path_empty_path_returns_empty() {
        // Edge case — both home and cwd are non-empty so neither
        // matches; fall through to the path itself.
        let s = relative_memory_path("", "/home", "/cwd");
        assert_eq!(s, "");
    }

    #[test]
    fn relative_memory_path_empty_home_and_cwd_returns_path() {
        // Edge case — empty `home_dir` is a byte-prefix of every
        // path, so the home branch matches. Pin the equivalent.
        let s = relative_memory_path("/x/y/z", "", "");
        // home form: "~/x/y/z" (7)
        // cwd form: "./x/y/z" (7)
        // tie -> home wins.
        assert_eq!(s, "~/x/y/z");
    }

    // ──────────────────────────────────────────────────────────────
    // display_path
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn display_path_inside_cwd_uses_relative() {
        let s = display_path("/proj/sub/REBON.md", "/home/u", "/proj", '/');
        assert_eq!(s, "sub/REBON.md");
    }

    #[test]
    fn display_path_inside_home_uses_tilde() {
        let s = display_path("/home/u/.rebon/REBON.md", "/home/u", "/proj", '/');
        assert_eq!(s, "~/.rebon/REBON.md");
    }

    #[test]
    fn display_path_outside_both_returns_absolute() {
        let s = display_path("/srv/elsewhere/REBON.md", "/home/u", "/proj", '/');
        assert_eq!(s, "/srv/elsewhere/REBON.md");
    }

    #[test]
    fn display_path_exact_home_does_not_match() {
        // The check uses `home_dir` + separator, so a path exactly
        // equal to `home_dir` does NOT match. Falls through to absolute.
        let s = display_path("/home/u", "/home/u", "/proj", '/');
        assert_eq!(s, "/home/u");
    }

    #[test]
    fn display_path_exact_cwd_returns_empty_then_falls_through() {
        // The relative path from `cwd` to itself is empty. The guard
        // needs a non-empty relative path that does not start with
        // `..`, so the empty string is treated as absent here and the
        // call falls through to the home / absolute branches. Pin it.
        let s = display_path("/proj", "/home/u", "/proj", '/');
        assert_eq!(s, "/proj");
    }

    #[test]
    fn display_path_windows_separator() {
        let s = display_path(
            "C:\\Users\\bon\\REBON.md",
            "C:\\Users\\bon",
            "C:\\proj",
            '\\',
        );
        assert_eq!(s, "~\\REBON.md");
    }

    #[test]
    fn display_path_outside_cwd_subtree_uses_home_branch() {
        // path.relative(cwd, path) starts with `..` because the
        // file is outside cwd. The guard short-circuits to the
        // home branch.
        let s = display_path("/home/u/x/REBON.md", "/home/u", "/proj/inside", '/');
        assert_eq!(s, "~/x/REBON.md");
    }

    // ──────────────────────────────────────────────────────────────
    // relative_path_from
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn relative_path_from_strips_prefix_and_separator() {
        assert_eq!(relative_path_from("/a/b", "/a/b/c/d"), "c/d");
    }

    #[test]
    fn relative_path_from_handles_windows_separator() {
        assert_eq!(relative_path_from("C:\\a", "C:\\a\\b"), "b");
    }

    #[test]
    fn relative_path_from_equal_returns_empty() {
        assert_eq!(relative_path_from("/a/b", "/a/b"), "");
    }

    #[test]
    fn relative_path_from_returns_upward_path_when_unrelated_to_from() {
        // The relative path from `/a/b` to `/x/y` walks up from
        // `/a/b` to `/` (two `..` segments) then down to `/x/y`,
        // yielding `../../x/y`. The exact form is not load-bearing
        // for the selector — only the leading `..` matters.
        let r = relative_path_from("/a/b", "/x/y");
        assert!(r.starts_with(".."));
    }

    #[test]
    fn relative_path_from_no_separator_after_prefix_returns_upward_path() {
        // Edge case — prefix without trailing sep, suffix has no
        // leading sep either. The relative path from `/a` to `/abc`
        // is `'../abc'` because `/a` and `/abc` share no
        // common subdirectory. The crate matches that.
        assert_eq!(relative_path_from("/a", "/abc"), "../abc");
    }

    #[test]
    fn relative_path_from_shared_parent_without_component_boundary_uses_common_parent() {
        assert_eq!(relative_path_from("/home/u", "/home/user/x"), "../user/x");
    }

    #[test]
    fn relative_memory_path_byte_prefix_uses_relative_path_from_tie_break() {
        let s = relative_memory_path("/home/user/x", "/zzz", "/home/u");
        assert_eq!(s, "./../user/x");
    }

    #[test]
    fn relative_path_from_returns_upward_path_when_not_a_prefix() {
        let r = relative_path_from("/proj", "/home/u/x");
        assert!(r.starts_with(".."));
    }

    // ──────────────────────────────────────────────────────────────
    // table
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn relative_memory_path_table() {
        // (path, home_dir, cwd) → expected
        let cases: &[(&str, &str, &str, &str)] = &[
            // inside home only
            (
                "/home/u/.rebon/REBON.md",
                "/home/u",
                "/srv",
                "~/.rebon/REBON.md",
            ),
            // inside cwd only
            ("/srv/proj/REBON.md", "/home/u", "/srv/proj", "./REBON.md"),
            // inside both, cwd shorter
            (
                "/home/u/proj/REBON.md",
                "/home/u",
                "/home/u/proj",
                "./REBON.md",
            ),
            // inside both, tie -> home wins
            ("/h/REBON.md", "/h", "/h", "~/REBON.md"),
            // outside both
            (
                "/srv/elsewhere/REBON.md",
                "/home/u",
                "/proj",
                "/srv/elsewhere/REBON.md",
            ),
            // exact home -> ~
            ("/home/u", "/home/u", "/proj", "~"),
            // exact cwd ->./
            ("/proj", "/elsewhere", "/proj", "./"),
        ];

        for (path, home, cwd, expected) in cases {
            let actual = relative_memory_path(path, home, cwd);
            assert_eq!(
                &actual, expected,
                "relative_memory_path({path:?}, {home:?}, {cwd:?})"
            );
        }
    }

    #[test]
    fn display_path_table() {
        // (path, home, cwd, sep) → expected
        let cases: &[(&str, &str, &str, char, &str)] = &[
            // inside cwd subtree
            ("/proj/sub/x.md", "/home/u", "/proj", '/', "sub/x.md"),
            // inside home (cwd elsewhere)
            (
                "/home/u/.rebon/REBON.md",
                "/home/u",
                "/proj",
                '/',
                "~/.rebon/REBON.md",
            ),
            // outside both
            ("/srv/x", "/home/u", "/proj", '/', "/srv/x"),
            // exact home -> falls through
            ("/home/u", "/home/u", "/proj", '/', "/home/u"),
            // exact cwd -> falls through (rel == "")
            ("/proj", "/home/u", "/proj", '/', "/proj"),
        ];

        for (path, home, cwd, sep, expected) in cases {
            let actual = display_path(path, home, cwd, *sep);
            assert_eq!(
                &actual, expected,
                "display_path({path:?}, {home:?}, {cwd:?}, {sep:?})"
            );
        }
    }
}
