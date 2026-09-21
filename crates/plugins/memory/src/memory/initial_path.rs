//! `compute_initial_path` — the initial-focus rule for the memory
//! selector.
//!
//! ## What it does
//!
//! ```text
//! last_selected_path: Option<&str>
//!
//! if it is set, non-empty, and still in the option list:
//!     initial path = last_selected_path
//! else:
//!     initial path = first option's value, or "" when the list is empty
//! ```
//!
//! Three load-bearing details:
//!
//! 1. **The last-selected path is caller-owned state.** It
//! survives across re-opens of the selector; the crate does NOT
//! model that lifecycle — the consumer holds the current value
//! and threads it into [`compute_initial_path`].
//! 2. **The last-selected branch only fires when the value is
//! still in the option list.** If the user closed and re-opened
//! the selector after the option list changed (e.g. a memory
//! file was deleted), the lookup falls back to the first
//! option.
//! 3. **The fallback handles two empty cases.** If `options` is
//! empty there is no first option, so the result is an owned
//! `String::new()`. An empty `last_selected_path` also counts as
//! "nothing selected" and falls back the same way.

use crate::memory::option_list::MemoryOption;

/// The initial-focus rule, given the value carried over from the
/// previous open.
///
/// * `last_selected_path` is the consumer-owned value from that
/// previous open. Pass `None` when the selector has not been opened
/// before.
/// * `options` is the full option list (memory rows + folder rows
/// concatenated).
///
/// Returns the option value to focus initially. Empty string when
/// the option list is empty (there is no first option to fall back
/// to).
pub fn compute_initial_path(last_selected_path: Option<&str>, options: &[MemoryOption]) -> String {
    if let Some(last) = last_selected_path {
        if !last.is_empty() && options.iter().any(|o| o.value == last) {
            return last.to_string();
        }
    }
    options.first().map(|o| o.value.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(value: &str) -> MemoryOption {
        MemoryOption {
            label: value.to_string(),
            value: value.to_string(),
            description: String::new(),
        }
    }

    #[test]
    fn no_last_selected_returns_first_option() {
        let opts = vec![opt("/a"), opt("/b"), opt("/c")];
        let p = compute_initial_path(None, &opts);
        assert_eq!(p, "/a");
    }

    #[test]
    fn no_last_selected_with_empty_list_returns_empty() {
        let p = compute_initial_path(None, &[]);
        assert_eq!(p, "");
    }

    #[test]
    fn last_selected_in_list_returns_it() {
        let opts = vec![opt("/a"), opt("/b"), opt("/c")];
        let p = compute_initial_path(Some("/b"), &opts);
        assert_eq!(p, "/b");
    }

    #[test]
    fn last_selected_not_in_list_falls_back_to_first() {
        let opts = vec![opt("/a"), opt("/b"), opt("/c")];
        let p = compute_initial_path(Some("/missing"), &opts);
        assert_eq!(p, "/a");
    }

    #[test]
    fn last_selected_not_in_list_with_empty_list_returns_empty() {
        let p = compute_initial_path(Some("/anything"), &[]);
        assert_eq!(p, "");
    }

    #[test]
    fn empty_last_selected_falls_back_to_first() {
        // An empty `last_selected_path` counts as "nothing selected",
        // so the lookup falls back to the first option.
        let opts = vec![opt("/a"), opt("/b")];
        let p = compute_initial_path(Some(""), &opts);
        assert_eq!(p, "/a");
    }

    #[test]
    fn last_selected_at_first_position_returns_it() {
        let opts = vec![opt("/a"), opt("/b")];
        let p = compute_initial_path(Some("/a"), &opts);
        assert_eq!(p, "/a");
    }

    #[test]
    fn last_selected_at_last_position_returns_it() {
        let opts = vec![opt("/a"), opt("/b"), opt("/c")];
        let p = compute_initial_path(Some("/c"), &opts);
        assert_eq!(p, "/c");
    }

    #[test]
    fn lookup_uses_value_not_label() {
        // The lookup compares `option.value` against the carried-over
        // path, not the label. Pin it.
        let opts = vec![MemoryOption {
            label: "User memory".to_string(),
            value: "/home/u/.rebon/REBON.md".to_string(),
            description: String::new(),
        }];
        // Searching by label should NOT match.
        let p = compute_initial_path(Some("User memory"), &opts);
        assert_eq!(p, "/home/u/.rebon/REBON.md"); // falls back to first
    }

    #[test]
    fn initial_path_selection_table() {
        struct Case {
            last: Option<&'static str>,
            opts: Vec<&'static str>,
            expected: &'static str,
        }
        let cases = vec![
            Case {
                last: None,
                opts: vec!["/a", "/b"],
                expected: "/a",
            },
            Case {
                last: None,
                opts: vec![],
                expected: "",
            },
            Case {
                last: Some("/b"),
                opts: vec!["/a", "/b"],
                expected: "/b",
            },
            Case {
                last: Some("/missing"),
                opts: vec!["/a"],
                expected: "/a",
            },
            Case {
                last: Some(""),
                opts: vec!["/a"],
                expected: "/a",
            },
            Case {
                last: Some("/x"),
                opts: vec![],
                expected: "",
            },
        ];

        for c in cases {
            let opts: Vec<MemoryOption> = c.opts.iter().map(|v| opt(v)).collect();
            let actual = compute_initial_path(c.last, &opts);
            assert_eq!(actual, c.expected);
        }
    }
}
