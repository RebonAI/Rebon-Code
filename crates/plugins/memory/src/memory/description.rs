//! Pure description-builder for memory selector rows.
//!
//! ## Behavior reference
//!
//! The builder chooses a description by walking a fixed precedence order:
//! user memory at the top level, canonical project memory at the top level,
//! imported child memory, recursively discovered memory, and finally an empty
//! fallback for every other non-nested file.
//!
//! Six load-bearing details for the current behavior:
//!
//! 1. **`User` + non-nested uses the literal "Saved in ~/.rebon/REBON.md".**
//!    The description is hard-coded and does not interpolate the user-memory
//!    path. A user-memory file at a non-default path still shows the same string.
//! 2. **`Project` + non-nested + canonical path branches on `is_git`.** The
//!    string switches between "Checked in at" in a git repo and "Saved in"
//!    otherwise. The path suffix is the literal `./REBON.md`, not the actual
//!    display path.
//! 3. **A `Project`-typed file at a non-canonical path falls through to the
//!    parent, nested, or empty-description branches.**
//! 4. **`parent` is checked before `is_nested`.** A file that is both imported
//!    and discovered by the recursive walk gets the imported-memory label rather
//!    than "dynamically loaded".
//! 5. **A nested file with no parent uses "dynamically loaded".**
//! 6. **A non-nested, non-parent, non-canonical user, non-canonical project file
//!    uses an empty description.**
use rebon_instructions::memory_file::ExtendedMemoryFileInfo;
use rebon_instructions::memory_type::MemoryType;

/// Inputs for [`build_memory_description`].
#[derive(Debug, Clone, Copy)]
pub struct DescriptionInputs<'a> {
    /// The canonical project-memory path. The `Project` branch only
    /// fires when `file.path == project_memory_path`.
    pub project_memory_path: &'a str,
    /// `true` if the working directory is inside a git working tree.
    /// The consumer owns the git probe.
    pub is_git: bool,
}

/// Description text constants — pinned here so the renderer doesn't
/// have to remember them.
pub mod text {
    /// Description for the user-memory row.
    pub const USER_MEMORY: &str = "Saved in ~/.rebon/REBON.md";
    /// Description for canonical project memory — git branch.
    pub const PROJECT_MEMORY_GIT: &str = "Checked in at ./REBON.md";
    /// Description for canonical project memory — non-git branch.
    pub const PROJECT_MEMORY_NO_GIT: &str = "Saved in ./REBON.md";
    /// Description for imported child memory.
    pub const AT_IMPORTED: &str = "@-imported";
    /// Description for recursively discovered memory.
    pub const DYNAMICALLY_LOADED: &str = "dynamically loaded";
    /// Empty fallback description.
    pub const EMPTY: &str = "";
}

/// Build the row description for a memory file, following the
/// precedence order documented in the module docs.
pub fn build_memory_description(
    file: &ExtendedMemoryFileInfo,
    inputs: DescriptionInputs<'_>,
) -> String {
    if file.inner.r#type == MemoryType::User && !file.is_nested {
        return text::USER_MEMORY.to_string();
    }

    if file.inner.r#type == MemoryType::Project
        && !file.is_nested
        && file.inner.path == inputs.project_memory_path
    {
        return if inputs.is_git {
            text::PROJECT_MEMORY_GIT.to_string()
        } else {
            text::PROJECT_MEMORY_NO_GIT.to_string()
        };
    }

    if file.inner.parent.is_some() {
        return text::AT_IMPORTED.to_string();
    }

    if file.is_nested {
        return text::DYNAMICALLY_LOADED.to_string();
    }

    text::EMPTY.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_instructions::memory_file::MemoryFileInfo;

    fn proj_inputs<'a>() -> DescriptionInputs<'a> {
        DescriptionInputs {
            project_memory_path: "/proj/REBON.md",
            is_git: false,
        }
    }

    fn proj_inputs_git<'a>() -> DescriptionInputs<'a> {
        DescriptionInputs {
            project_memory_path: "/proj/REBON.md",
            is_git: true,
        }
    }

    // ──────────────────────────────────────────────────────────────
    // text constants
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn user_memory_text_pinned() {
        assert_eq!(text::USER_MEMORY, "Saved in ~/.rebon/REBON.md");
    }

    #[test]
    fn project_memory_git_text_pinned() {
        assert_eq!(text::PROJECT_MEMORY_GIT, "Checked in at ./REBON.md");
    }

    #[test]
    fn project_memory_no_git_text_pinned() {
        assert_eq!(text::PROJECT_MEMORY_NO_GIT, "Saved in ./REBON.md");
    }

    #[test]
    fn at_imported_text_pinned() {
        assert_eq!(text::AT_IMPORTED, "@-imported");
    }

    #[test]
    fn dynamically_loaded_text_pinned() {
        assert_eq!(text::DYNAMICALLY_LOADED, "dynamically loaded");
    }

    // ──────────────────────────────────────────────────────────────
    // User branch
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn user_non_nested_yields_user_memory_string() {
        let inner = MemoryFileInfo::new("/home/u/.rebon/REBON.md", MemoryType::User);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "Saved in ~/.rebon/REBON.md");
    }

    #[test]
    fn user_non_nested_at_arbitrary_path_still_yields_same_string() {
        // The User branch doesn't check the path. Pin it.
        let inner = MemoryFileInfo::new("/anywhere/REBON.md", MemoryType::User);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "Saved in ~/.rebon/REBON.md");
    }

    #[test]
    fn user_nested_falls_through_to_other_branches() {
        let inner = MemoryFileInfo::new("/x/REBON.md", MemoryType::User);
        let file = ExtendedMemoryFileInfo::existing(inner).nested();
        let d = build_memory_description(&file, proj_inputs());
        // No parent, isNested true → "dynamically loaded".
        assert_eq!(d, "dynamically loaded");
    }

    // ──────────────────────────────────────────────────────────────
    // Project branch
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn project_non_nested_canonical_no_git_yields_saved_in() {
        let inner = MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "Saved in ./REBON.md");
    }

    #[test]
    fn project_non_nested_canonical_with_git_yields_checked_in_at() {
        let inner = MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs_git());
        assert_eq!(d, "Checked in at ./REBON.md");
    }

    #[test]
    fn project_at_non_canonical_path_falls_through_to_empty() {
        // Non-canonical project file with no parent and not nested
        // → empty description.
        let inner = MemoryFileInfo::new("/elsewhere/REBON.md", MemoryType::Project);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "");
    }

    #[test]
    fn project_at_non_canonical_path_with_parent_yields_at_imported() {
        let inner = MemoryFileInfo::new("/elsewhere/REBON.md", MemoryType::Project)
            .with_parent("/proj/REBON.md");
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "@-imported");
    }

    #[test]
    fn project_nested_at_canonical_path_falls_through() {
        let inner = MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project);
        let file = ExtendedMemoryFileInfo::existing(inner).nested();
        let d = build_memory_description(&file, proj_inputs());
        // isNested → "dynamically loaded".
        assert_eq!(d, "dynamically loaded");
    }

    // ──────────────────────────────────────────────────────────────
    // parent branch
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn local_with_parent_yields_at_imported() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local)
            .with_parent("/proj/REBON.md");
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "@-imported");
    }

    #[test]
    fn parent_takes_precedence_over_is_nested() {
        // The builder checks `parent` BEFORE `is_nested`, so parent
        // metadata wins even when the nested flag is also set.
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local)
            .with_parent("/proj/REBON.md");
        let file = ExtendedMemoryFileInfo::existing(inner).nested();
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "@-imported");
    }

    // ──────────────────────────────────────────────────────────────
    // nested branch
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn local_nested_no_parent_yields_dynamically_loaded() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner).nested();
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "dynamically loaded");
    }

    // ──────────────────────────────────────────────────────────────
    // empty branch
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn local_no_parent_no_nested_yields_empty() {
        let inner = MemoryFileInfo::new("/proj/sub/REBON.md", MemoryType::Local);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "");
    }

    // ──────────────────────────────────────────────────────────────
    // managed type
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn managed_no_parent_no_nested_yields_empty() {
        // Managed type doesn't get any special string; it falls
        // through to the empty case.
        let inner = MemoryFileInfo::new("/managed/x.md", MemoryType::Managed);
        let file = ExtendedMemoryFileInfo::existing(inner);
        let d = build_memory_description(&file, proj_inputs());
        assert_eq!(d, "");
    }

    // ──────────────────────────────────────────────────────────────
    // table
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn description_table() {
        struct Case<'a> {
            ty: MemoryType,
            path: &'a str,
            parent: Option<&'a str>,
            is_nested: bool,
            is_git: bool,
            expected: &'a str,
        }
        let cases = [
            // user, non-nested → literal saved string
            Case {
                ty: MemoryType::User,
                path: "/home/u/.rebon/REBON.md",
                parent: None,
                is_nested: false,
                is_git: false,
                expected: "Saved in ~/.rebon/REBON.md",
            },
            Case {
                ty: MemoryType::User,
                path: "/anywhere/REBON.md",
                parent: None,
                is_nested: false,
                is_git: false,
                expected: "Saved in ~/.rebon/REBON.md",
            },
            // project, non-nested, canonical, no-git → saved in
            Case {
                ty: MemoryType::Project,
                path: "/proj/REBON.md",
                parent: None,
                is_nested: false,
                is_git: false,
                expected: "Saved in ./REBON.md",
            },
            // project, non-nested, canonical, git → checked in at
            Case {
                ty: MemoryType::Project,
                path: "/proj/REBON.md",
                parent: None,
                is_nested: false,
                is_git: true,
                expected: "Checked in at ./REBON.md",
            },
            // project, non-nested, non-canonical → falls through
            Case {
                ty: MemoryType::Project,
                path: "/elsewhere/REBON.md",
                parent: None,
                is_nested: false,
                is_git: true,
                expected: "",
            },
            // Any type with a parent uses the imported-memory label.
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                parent: Some("/proj/REBON.md"),
                is_nested: false,
                is_git: false,
                expected: "@-imported",
            },
            // parent precedence over isNested
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                parent: Some("/proj/REBON.md"),
                is_nested: true,
                is_git: false,
                expected: "@-imported",
            },
            // nested, no parent → dynamically loaded
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                parent: None,
                is_nested: true,
                is_git: false,
                expected: "dynamically loaded",
            },
            // local, no parent, no nested → empty
            Case {
                ty: MemoryType::Local,
                path: "/proj/sub/REBON.md",
                parent: None,
                is_nested: false,
                is_git: false,
                expected: "",
            },
            // managed → empty (no Managed branch)
            Case {
                ty: MemoryType::Managed,
                path: "/managed/x.md",
                parent: None,
                is_nested: false,
                is_git: false,
                expected: "",
            },
            // user, nested → falls through to dynamically loaded
            Case {
                ty: MemoryType::User,
                path: "/home/u/.rebon/REBON.md",
                parent: None,
                is_nested: true,
                is_git: false,
                expected: "dynamically loaded",
            },
        ];

        for case in cases {
            let mut info = MemoryFileInfo::new(case.path, case.ty);
            if let Some(p) = case.parent {
                info = info.with_parent(p);
            }
            let mut file = ExtendedMemoryFileInfo::existing(info);
            file.is_nested = case.is_nested;
            let inputs = DescriptionInputs {
                project_memory_path: "/proj/REBON.md",
                is_git: case.is_git,
            };
            let actual = build_memory_description(&file, inputs);
            assert_eq!(
                actual, case.expected,
                "description mismatch for {:?} at {:?}",
                case.ty, case.path
            );
        }
    }
}
