//! Memory-update notification text formatter.
//!
//! ## Behavior notes
//!
//! ```text
//! display_path = relative_memory_path(memory_path, home_dir, cwd)
//! "Memory updated in {display_path} · /memory to edit"
//! ```
//!
//! Two load-bearing details:
//!
//! 1. **The path is interpolated through
//! [`crate::memory::relative_path::relative_memory_path`], not through a
//! display-path helper.** Pick the shorter of `~`-form and `./`-form,
//! fall back to absolute.
//! 2. **The literal text is `Memory updated in <path> · /memory to edit`.**
//! Pinned. Note the `·` U+00B7 separator.

use crate::memory::relative_path::relative_memory_path;

/// Format the memory-update notification line.
///
/// `home_dir` and `cwd` are forwarded to
/// [`crate::memory::relative_path::relative_memory_path`]. The crate does
/// not call `homedir` or `process.cwd` itself — the consumer
/// owns the env probe.
pub fn format_update_notification(memory_path: &str, home_dir: &str, cwd: &str) -> String {
    let display = relative_memory_path(memory_path, home_dir, cwd);
    format!("Memory updated in {display} \u{00B7} /memory to edit")
}

/// [`format_update_notification`] against the running process's home
/// directory, resolved the way every other `~` in Rebon is
/// ([`rebon_session::platform_home_dir`]). With no home directory the
/// `~`-form is simply never the shorter one.
pub fn format_update_notification_from_env(memory_path: &str, cwd: &str) -> String {
    let home = rebon_session::platform_home_dir()
        .map(|home| home.to_string_lossy().into_owned())
        .unwrap_or_default();
    format_update_notification(memory_path, &home, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_uses_home_relative_path_when_inside_home() {
        let s = format_update_notification("/home/u/.rebon/REBON.md", "/home/u", "/proj");
        assert_eq!(
            s,
            "Memory updated in ~/.rebon/REBON.md \u{00B7} /memory to edit"
        );
    }

    #[test]
    fn notification_uses_cwd_relative_path_when_inside_cwd() {
        let s = format_update_notification("/proj/REBON.md", "/home/u", "/proj");
        assert_eq!(s, "Memory updated in ./REBON.md \u{00B7} /memory to edit");
    }

    #[test]
    fn notification_picks_shorter_form_when_both_apply() {
        // home_dir is `/h`, cwd is `/h/deeply/nested`. File at
        // `/h/deeply/nested/REBON.md` is inside both. Cwd form
        // `./REBON.md` (8) is shorter than home form
        // `~/deeply/nested/REBON.md` (22).
        let s = format_update_notification("/h/deeply/nested/REBON.md", "/h", "/h/deeply/nested");
        assert_eq!(s, "Memory updated in ./REBON.md \u{00B7} /memory to edit");
    }

    #[test]
    fn notification_falls_back_to_absolute_when_outside_both() {
        let s = format_update_notification("/srv/elsewhere/REBON.md", "/home/u", "/proj");
        assert_eq!(
            s,
            "Memory updated in /srv/elsewhere/REBON.md \u{00B7} /memory to edit"
        );
    }

    #[test]
    fn notification_separator_is_middle_dot() {
        // U+00B7 MIDDLE DOT, not the regular ASCII dot.
        let s = format_update_notification("/a", "/h", "/c");
        assert!(s.contains('\u{00B7}'));
    }

    #[test]
    fn notification_text_template_pinned() {
        // Pin the entire literal sandwich.
        let s = format_update_notification("/", "/h", "/c");
        // Path "/" doesn't start with "/h" or "/c", so absolute.
        assert_eq!(s, "Memory updated in / \u{00B7} /memory to edit");
    }

    #[test]
    fn notification_with_empty_path_renders_empty_path() {
        let s = format_update_notification("", "/h", "/c");
        // Empty path doesn't match either prefix → absolute (empty).
        assert_eq!(s, "Memory updated in  \u{00B7} /memory to edit");
    }

    #[test]
    fn notification_tie_break_uses_home_form() {
        // home==cwd → tie → home wins.
        let s = format_update_notification("/h/REBON.md", "/h", "/h");
        assert_eq!(s, "Memory updated in ~/REBON.md \u{00B7} /memory to edit");
    }

    #[test]
    fn notification_windows_paths() {
        let s = format_update_notification(
            "C:\\Users\\bon\\.rebon\\REBON.md",
            "C:\\Users\\bon",
            "C:\\proj",
        );
        assert_eq!(
            s,
            "Memory updated in ~\\.rebon\\REBON.md \u{00B7} /memory to edit"
        );
    }
}
