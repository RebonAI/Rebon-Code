//! How a file path reads in a transcript row.
//!
//! Tools report absolute paths, and in a session rooted at
//! `C:\Users\me\AppData\Local\Temp\…\project` every Read and Update header
//! spent a full row on the part of the path that never changes. The surface
//! that owns the session names its working directory once with
//! [`set_display_root`]; after that, a path inside it is shown relative to it
//! (`src/lib.rs`), and anything outside it is shown as it came.
//!
//! This only changes the label. Hyperlinks and every other use of the path
//! keep the absolute form.

use std::borrow::Cow;
use std::sync::RwLock;

static DISPLAY_ROOT: RwLock<Option<String>> = RwLock::new(None);

/// Set (or with `None`, clear) the directory paths are shown relative to.
/// A blank root clears it.
pub fn set_display_root(root: Option<&str>) {
    let root = root
        .map(|root| root.trim().trim_end_matches(['/', '\\']).to_string())
        .filter(|root| !root.is_empty());
    if let Ok(mut slot) = DISPLAY_ROOT.write() {
        *slot = root;
    }
}

/// The label for `path`: relative to the display root when it lies inside
/// it, otherwise `path` unchanged.
pub fn display_path(path: &str) -> Cow<'_, str> {
    let Ok(slot) = DISPLAY_ROOT.read() else {
        return Cow::Borrowed(path);
    };
    match slot.as_deref() {
        Some(root) => relative_to(path, root, cfg!(windows)),
        None => Cow::Borrowed(path),
    }
}

/// `path` relative to `root`, comparing `/` and `\` as the same separator and,
/// when `case_insensitive`, ignoring ASCII case (Windows drive letters and
/// directory names). The root itself reads as `.`.
fn relative_to<'a>(path: &'a str, root: &str, case_insensitive: bool) -> Cow<'a, str> {
    let root = root.trim_end_matches(['/', '\\']);
    if root.is_empty() || path.len() < root.len() {
        return Cow::Borrowed(path);
    }
    let same = path.bytes().zip(root.bytes()).all(|(a, b)| match (a, b) {
        (b'/' | b'\\', b'/' | b'\\') => true,
        _ if case_insensitive => a.eq_ignore_ascii_case(&b),
        _ => a == b,
    });
    if !same {
        return Cow::Borrowed(path);
    }
    let rest = &path[root.len()..];
    if rest.is_empty() {
        return Cow::Borrowed(".");
    }
    match rest.strip_prefix(['/', '\\']) {
        Some("") => Cow::Borrowed("."),
        Some(relative) => Cow::Borrowed(relative),
        // `/repo-old` is not inside `/repo`.
        None => Cow::Borrowed(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_inside_the_root_reads_relative() {
        assert_eq!(
            relative_to("/repo/src/lib.rs", "/repo", false),
            "src/lib.rs"
        );
        assert_eq!(
            relative_to("/repo/src/lib.rs", "/repo/", false),
            "src/lib.rs"
        );
        assert_eq!(relative_to("/repo", "/repo", false), ".");
        assert_eq!(relative_to("/repo/", "/repo", false), ".");
    }

    #[test]
    fn windows_separators_and_case_are_interchangeable() {
        let root = r"C:\Users\me\project";
        assert_eq!(
            relative_to(r"C:\Users\me\project\src\lib.rs", root, true),
            r"src\lib.rs"
        );
        assert_eq!(
            relative_to("C:/Users/me/project/src/lib.rs", root, true),
            "src/lib.rs"
        );
        assert_eq!(
            relative_to(r"c:\users\ME\Project\README.md", root, true),
            "README.md"
        );
        // Case only folds where the filesystem does.
        assert_eq!(
            relative_to("/Repo/src/lib.rs", "/repo", false),
            "/Repo/src/lib.rs"
        );
    }

    #[test]
    fn paths_outside_the_root_are_left_alone() {
        assert_eq!(
            relative_to("/repo-old/lib.rs", "/repo", false),
            "/repo-old/lib.rs"
        );
        assert_eq!(
            relative_to("/other/lib.rs", "/repo", false),
            "/other/lib.rs"
        );
        assert_eq!(relative_to("/re", "/repo", false), "/re");
        assert_eq!(relative_to("src/lib.rs", "/repo", false), "src/lib.rs");
        assert_eq!(relative_to("/repo/lib.rs", "", false), "/repo/lib.rs");
    }

    #[test]
    fn non_ascii_paths_compare_byte_for_byte() {
        assert_eq!(relative_to("/项目/源码/a.rs", "/项目", false), "源码/a.rs");
        assert_eq!(relative_to("/项目二/a.rs", "/项目", false), "/项目二/a.rs");
    }

    #[test]
    fn the_process_wide_root_is_set_and_cleared() {
        // Serialised with every other test touching the slot.
        let _guard = crate::display_path::tests::LOCK.lock().unwrap();
        set_display_root(Some("/work/app/"));
        assert_eq!(display_path("/work/app/src/main.rs"), "src/main.rs");
        assert_eq!(display_path("/elsewhere/x"), "/elsewhere/x");
        set_display_root(Some("   "));
        assert_eq!(
            display_path("/work/app/src/main.rs"),
            "/work/app/src/main.rs"
        );
        set_display_root(None);
        assert_eq!(
            display_path("/work/app/src/main.rs"),
            "/work/app/src/main.rs"
        );
    }

    pub(crate) static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
