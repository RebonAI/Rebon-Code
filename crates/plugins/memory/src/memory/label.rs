//! Pure label-builder for memory selector rows.
//!
//! ## What it produces
//!
//! ```text
//! canonical user path, not nested    -> "User memory"
//! canonical project path, not nested -> "Project memory"
//! depth > 0                          -> "<indent>L <display_path><exists_label>"
//! depth == 0                         -> "<display_path>"
//! ```
//!
//! Five load-bearing details:
//!
//! 1. **The "User memory" / "Project memory" labels are reserved
//! for the canonical paths only.** A `User`-typed file at a
//! different path falls through to the generic branch and gets
//! the display-path label.
//! 2. **`is_nested` blocks the canonical labels.** A nested
//! `User`-typed file (discovered by the recursive walk) does
//! NOT get the "User memory" label.
//! 3. **The indent is `"  ".repeat(depth - 1)`** — two spaces per
//! parent above the leaf. The depth-zero case has no indent.
//! 4. **The leaf marker is `L `** — capital L followed by a space.
//! Pinned with a test.
//! 5. **`exists_label` is `" (new)"` (with a leading space) when
//! `exists` is false.** Empty string otherwise.

use rebon_instructions::memory_file::ExtendedMemoryFileInfo;
use rebon_instructions::memory_type::MemoryType;

/// Inputs for [`build_memory_label`]. The selector pre-computes
/// `display_path`, `depth`, and the canonical paths once and reuses
/// them across all rows.
#[derive(Debug, Clone, Copy)]
pub struct LabelInputs<'a> {
    /// The pre-computed display path for the file, as produced by
    /// [`crate::memory::relative_path::display_path`].
    pub display_path: &'a str,
    /// Depth of the file in the `@`-include tree. `0` for top-level
    /// files; `1, 2, 3, …` for nested files.
    pub depth: usize,
    /// The canonical user-memory path. The label-builder branches
    /// on `file.path == user_memory_path` for the "User memory"
    /// label.
    pub user_memory_path: &'a str,
    /// The canonical project-memory path.
    pub project_memory_path: &'a str,
}

/// Build the row label for a memory file using the branches above.
pub fn build_memory_label(file: &ExtendedMemoryFileInfo, inputs: LabelInputs<'_>) -> String {
    let exists_label = if file.exists { "" } else { " (new)" };
    let indent = build_indent(inputs.depth);

    if file.inner.r#type == MemoryType::User
        && !file.is_nested
        && file.inner.path == inputs.user_memory_path
    {
        return "User memory".to_string();
    }

    if file.inner.r#type == MemoryType::Project
        && !file.is_nested
        && file.inner.path == inputs.project_memory_path
    {
        return "Project memory".to_string();
    }

    if inputs.depth > 0 {
        let mut s = String::with_capacity(
            indent.len() + 2 + inputs.display_path.len() + exists_label.len(),
        );
        s.push_str(&indent);
        s.push_str("L ");
        s.push_str(inputs.display_path);
        s.push_str(exists_label);
        s
    } else {
        inputs.display_path.to_string()
    }
}

/// Build the indent string for a given depth: `""` for `depth == 0`,
/// `"  ".repeat(depth - 1)` otherwise.
pub fn build_indent(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        "  ".repeat(depth - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_instructions::memory_file::MemoryFileInfo;

    fn user_inputs<'a>() -> LabelInputs<'a> {
        LabelInputs {
            display_path: "ignored",
            depth: 0,
            user_memory_path: "/home/u/.rebon/REBON.md",
            project_memory_path: "/proj/REBON.md",
        }
    }

    // ──────────────────────────────────────────────────────────────
    // build_indent
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn indent_depth_zero_is_empty() {
        assert_eq!(build_indent(0), "");
    }

    #[test]
    fn indent_depth_one_is_empty() {
        // depth - 1 = 0 → empty string. The `L ` marker handles
        // depth-1 visually.
        assert_eq!(build_indent(1), "");
    }

    #[test]
    fn indent_depth_two_is_two_spaces() {
        assert_eq!(build_indent(2), "  ");
    }

    #[test]
    fn indent_depth_three_is_four_spaces() {
        assert_eq!(build_indent(3), "    ");
    }

    #[test]
    fn indent_depth_four_is_six_spaces() {
        assert_eq!(build_indent(4), "      ");
    }

    // ──────────────────────────────────────────────────────────────
    // canonical "User memory" / "Project memory" labels
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn canonical_user_memory_path_yields_user_memory_label() {
        let inner = MemoryFileInfo::new("/home/u/.rebon/REBON.md", MemoryType::User);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let label = build_memory_label(&file, user_inputs());
        assert_eq!(label, "User memory");
    }

    #[test]
    fn canonical_project_memory_path_yields_project_memory_label() {
        let inner = MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let label = build_memory_label(&file, user_inputs());
        assert_eq!(label, "Project memory");
    }

    #[test]
    fn user_typed_at_different_path_falls_through_to_display_path() {
        // User-typed file but path != user_memory_path → generic.
        let inner = MemoryFileInfo::new("/elsewhere/REBON.md", MemoryType::User);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let inputs = LabelInputs {
            display_path: "elsewhere/REBON.md",
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "elsewhere/REBON.md");
    }

    #[test]
    fn nested_user_at_canonical_path_falls_through_to_display_path() {
        // A nested file blocks the canonical label.
        let inner = MemoryFileInfo::new("/home/u/.rebon/REBON.md", MemoryType::User);
        let file = ExtendedMemoryFileInfo::existing(inner).nested();
        let inputs = LabelInputs {
            display_path: "~/.rebon/REBON.md",
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        // Nested at depth 0 falls into the depth-0 branch.
        assert_eq!(label, "~/.rebon/REBON.md");
    }

    #[test]
    fn nested_project_at_canonical_path_falls_through_to_display_path() {
        let inner = MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project);
        let file = ExtendedMemoryFileInfo::existing(inner).nested();
        let inputs = LabelInputs {
            display_path: "REBON.md",
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "REBON.md");
    }

    #[test]
    fn local_typed_at_user_memory_path_falls_through_to_display_path() {
        // The canonical-label branch requires the file type to be `User`. A Local-typed
        // file at the user path does NOT get the "User memory" label.
        let inner = MemoryFileInfo::new("/home/u/.rebon/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let inputs = LabelInputs {
            display_path: "~/.rebon/REBON.md",
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "~/.rebon/REBON.md");
    }

    // ──────────────────────────────────────────────────────────────
    // depth-driven branch
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn depth_zero_label_is_just_display_path() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let inputs = LabelInputs {
            display_path: "sub/REBON.md",
            depth: 0,
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "sub/REBON.md");
    }

    #[test]
    fn depth_one_label_uses_l_marker_with_no_indent() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let inputs = LabelInputs {
            display_path: "sub/REBON.md",
            depth: 1,
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "L sub/REBON.md");
    }

    #[test]
    fn depth_two_label_indents_two_spaces() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let inputs = LabelInputs {
            display_path: "sub/REBON.md",
            depth: 2,
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "  L sub/REBON.md");
    }

    #[test]
    fn depth_three_label_indents_four_spaces() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let inputs = LabelInputs {
            display_path: "sub/REBON.md",
            depth: 3,
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "    L sub/REBON.md");
    }

    // ──────────────────────────────────────────────────────────────
    // exists / new suffix
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn missing_file_at_depth_one_appends_new_suffix() {
        let inner = MemoryFileInfo::new("/proj/new/REBON.md", MemoryType::Local);
        let mut file = ExtendedMemoryFileInfo::existing(inner);
        file.exists = false;
        let inputs = LabelInputs {
            display_path: "new/REBON.md",
            depth: 1,
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "L new/REBON.md (new)");
    }

    #[test]
    fn missing_file_at_depth_zero_does_not_append_new_suffix() {
        // The (new) suffix is only added in the depth > 0 branch, so a
        // missing file at depth 0 falls through to the bare
        // display path with no suffix.
        let inner = MemoryFileInfo::new("/proj/new/REBON.md", MemoryType::Local);
        let mut file = ExtendedMemoryFileInfo::existing(inner);
        file.exists = false;
        let inputs = LabelInputs {
            display_path: "new/REBON.md",
            depth: 0,
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "new/REBON.md");
    }

    #[test]
    fn existing_file_at_depth_one_omits_new_suffix() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let inputs = LabelInputs {
            display_path: "sub/REBON.md",
            depth: 1,
            ..user_inputs()
        };
        let label = build_memory_label(&file, inputs);
        assert_eq!(label, "L sub/REBON.md");
    }

    // ──────────────────────────────────────────────────────────────
    // missing user / project stub at canonical paths
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn missing_user_stub_at_canonical_path_yields_user_memory_label() {
        // The selector injects a stub with exists=false, type=User,
        // path=user_memory_path. It should still get "User memory".
        let stub = ExtendedMemoryFileInfo::missing_user_stub("/home/u/.rebon/REBON.md");
        let label = build_memory_label(&stub, user_inputs());
        // Note: even though exists is false, the canonical-label
        // branch matches first, so no " (new)" suffix.
        assert_eq!(label, "User memory");
    }

    #[test]
    fn missing_project_stub_at_canonical_path_yields_project_memory_label() {
        let stub = ExtendedMemoryFileInfo::missing_project_stub("/proj/REBON.md");
        let label = build_memory_label(&stub, user_inputs());
        assert_eq!(label, "Project memory");
    }

    // ──────────────────────────────────────────────────────────────
    // table
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn label_table() {
        // Each row pins one (type, path, is_nested, exists, depth,
        // display_path) tuple against the expected label.
        struct Case<'a> {
            ty: MemoryType,
            path: &'a str,
            is_nested: bool,
            exists: bool,
            depth: usize,
            display_path: &'a str,
            expected: &'a str,
        }

        let cases = [
            Case {
                ty: MemoryType::User,
                path: "/home/u/.rebon/REBON.md",
                is_nested: false,
                exists: true,
                depth: 0,
                display_path: "~/.rebon/REBON.md",
                expected: "User memory",
            },
            Case {
                ty: MemoryType::Project,
                path: "/proj/REBON.md",
                is_nested: false,
                exists: true,
                depth: 0,
                display_path: "REBON.md",
                expected: "Project memory",
            },
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                is_nested: false,
                exists: true,
                depth: 0,
                display_path: "sub/REBON.md",
                expected: "sub/REBON.md",
            },
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                is_nested: false,
                exists: true,
                depth: 1,
                display_path: "sub/REBON.md",
                expected: "L sub/REBON.md",
            },
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                is_nested: false,
                exists: false,
                depth: 1,
                display_path: "sub/REBON.md",
                expected: "L sub/REBON.md (new)",
            },
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                is_nested: false,
                exists: true,
                depth: 2,
                display_path: "sub/REBON.md",
                expected: "  L sub/REBON.md",
            },
            Case {
                ty: MemoryType::User,
                path: "/elsewhere/REBON.md",
                is_nested: false,
                exists: true,
                depth: 0,
                display_path: "elsewhere/REBON.md",
                expected: "elsewhere/REBON.md",
            },
        ];

        for case in cases {
            let mut info = MemoryFileInfo::new(case.path, case.ty);
            // Irrelevant to the label, but clear it to match the shape
            // the real walk produces
            info.content.clear();
            let mut file = ExtendedMemoryFileInfo::existing(info);
            file.is_nested = case.is_nested;
            file.exists = case.exists;
            let inputs = LabelInputs {
                display_path: case.display_path,
                depth: case.depth,
                user_memory_path: "/home/u/.rebon/REBON.md",
                project_memory_path: "/proj/REBON.md",
            };
            let actual = build_memory_label(&file, inputs);
            assert_eq!(
                actual, case.expected,
                "label mismatch for {:?} at {:?} (depth {})",
                case.ty, case.path, case.depth
            );
        }
    }
}
