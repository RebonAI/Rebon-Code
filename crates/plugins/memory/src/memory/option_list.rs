//! Orchestrator that ties [`crate::memory::label`] + [`crate::memory::description`]
//! together to build the final selector option list.
//!
//! ## What it does
//!
//! ```text
//! has_user_memory    = existing_memory_files contains user_memory_path
//! has_project_memory = existing_memory_files contains project_memory_path
//!
//! all_files = existing files that belong in the selector (exists = true)
//!             + user stub when there is no user memory file
//!             + project stub when there is no project memory file
//!
//! for each file, in order:
//! depth       = (parent's depth as seen so far) + 1, or 0 with no parent
//! label       = build_memory_label(...)
//! value       = file path
//! description = build_memory_description(...)
//! ```
//!
//! Six load-bearing details:
//!
//! 1. **Filter then map.** The filter excludes `AutoMem`
//! and `TeamMem` files; every survivor is wrapped with
//! `exists: true`.
//! 2. **Missing-row stubs come AFTER existing files.** The user
//! stub (if any) is appended after all existing files; the
//! project stub (if any) is appended after the user stub.
//! 3. **`has_user_memory` / `has_project_memory`** are checked
//! against the **unfiltered** `existing_memory_files` list: the
//! scan walks every entry, including AutoMem / TeamMem ones.
//! 4. **Depth map is in iteration order.** Each row's depth uses
//! the depth of its parent **as previously seen in the same
//! iteration**. If a child appears before its parent in the
//! list, its depth falls back to `0` (the parent is absent from
//! the map, so the default `0` applies and `+ 1` makes it `1`).
//! 5. **`value` is the file path** — the selector uses the file path
//! as the option value. Pinned.
//! 6. **The folder rows are pushed AFTER the memory rows**, not
//! interleaved. Handled by the consumer concatenating
//! `build_memory_options` + `build_folder_options`.

use crate::memory::description::{build_memory_description, DescriptionInputs};
use crate::memory::label::{build_memory_label, LabelInputs};
use crate::memory::relative_path::display_path;
use rebon_instructions::memory_file::{ExtendedMemoryFileInfo, MemoryFileInfo};
use std::collections::HashMap;

/// One row in the selector: the display label, the value reported
/// when the row is chosen, and the description shown on the right.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemoryOption {
    /// Display label for the row.
    pub label: String,
    /// Value reported to the consumer when the row is chosen. For
    /// memory files this is the absolute path. For "open folder"
    /// rows it's `OPEN_FOLDER_PREFIX + folder_path`.
    pub value: String,
    /// Description column on the right of the row.
    pub description: String,
}

/// Inputs for [`build_memory_options`]. The consumer computes these
/// once and then maps over the file list.
#[derive(Debug, Clone, Copy)]
pub struct MemoryOptionsInputs<'a> {
    /// The canonical user-memory path: `REBON.md` directly inside the
    /// config home directory (`REBON_CONFIG_DIR`).
    pub user_memory_path: &'a str,
    /// The canonical project-memory path: `REBON.md` at the root of the
    /// working directory.
    pub project_memory_path: &'a str,
    /// `true` if the cwd is inside a git working tree. Used by
    /// the description-builder for the
    /// `Checked in at ./REBON.md` / `Saved in ./REBON.md` branch.
    pub is_git: bool,
    /// User home directory. Forwarded to
    /// [`crate::memory::relative_path::display_path`].
    pub home_dir: &'a str,
    /// Current working directory. Forwarded to
    /// [`crate::memory::relative_path::display_path`].
    pub cwd: &'a str,
    /// Path separator (`/` on POSIX, `\\` on Windows). Forwarded
    /// to [`crate::memory::relative_path::display_path`].
    pub sep: char,
}

/// Build the selector option list.
///
/// **Inputs.** `existing_memory_files` is the pre-resolved list
/// handed in by the consumer. The crate does **not** resolve it
/// itself — that's the consumer's job. The
/// `MemoryFs` trait at [`crate::memory::memory_fs`] is for
/// downstream crates that re-implement the path-resolution layer.
///
/// **Output.** A `Vec<MemoryOption>` in the built order:
/// filtered existing files first, then the missing
/// user stub (if no existing User memory file was found), then
/// the missing project stub (if no existing Project memory file
/// was found).
///
/// **Folder rows are NOT included.** The consumer concatenates the
/// output of [`crate::memory::folder_options::build_folder_options`] after
/// this list.
pub fn build_memory_options(
    existing_memory_files: &[MemoryFileInfo],
    inputs: MemoryOptionsInputs<'_>,
) -> Vec<MemoryOption> {
    // Step 1: the has-user / has-project checks against the
    // *unfiltered* list.
    let has_user_memory = existing_memory_files
        .iter()
        .any(|f| f.path == inputs.user_memory_path);
    let has_project_memory = existing_memory_files
        .iter()
        .any(|f| f.path == inputs.project_memory_path);

    // Step 2: filter out AutoMem / TeamMem, wrap as
    // ExtendedMemoryFileInfo with exists=true.
    let mut all_files: Vec<ExtendedMemoryFileInfo> = existing_memory_files
        .iter()
        .filter(|f| f.r#type.keep_in_selector())
        .map(|f| ExtendedMemoryFileInfo::existing(f.clone()))
        .collect();

    // Step 3: append missing user stub (if any).
    if !has_user_memory {
        all_files.push(ExtendedMemoryFileInfo::missing_user_stub(
            inputs.user_memory_path,
        ));
    }

    // Step 4: append missing project stub (if any).
    if !has_project_memory {
        all_files.push(ExtendedMemoryFileInfo::missing_project_stub(
            inputs.project_memory_path,
        ));
    }

    // Step 5: walk the list, computing depth + label + description
    // for each row. The depth map carries forward in iteration
    // order — children that appear before their parent get depth
    // `0 + 1 = 1`.
    let mut depths: HashMap<String, usize> = HashMap::new();
    let mut options = Vec::with_capacity(all_files.len());

    for file in &all_files {
        let display = display_path(&file.inner.path, inputs.home_dir, inputs.cwd, inputs.sep);
        let depth = if let Some(parent) = &file.inner.parent {
            depths.get(parent).copied().unwrap_or(0) + 1
        } else {
            0
        };
        depths.insert(file.inner.path.clone(), depth);

        let label = build_memory_label(
            file,
            LabelInputs {
                display_path: &display,
                depth,
                user_memory_path: inputs.user_memory_path,
                project_memory_path: inputs.project_memory_path,
            },
        );
        let description = build_memory_description(
            file,
            DescriptionInputs {
                project_memory_path: inputs.project_memory_path,
                is_git: inputs.is_git,
            },
        );

        options.push(MemoryOption {
            label,
            value: file.inner.path.clone(),
            description,
        });
    }

    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_instructions::memory_type::MemoryType;

    fn standard_inputs<'a>() -> MemoryOptionsInputs<'a> {
        MemoryOptionsInputs {
            user_memory_path: "/home/u/.rebon/REBON.md",
            project_memory_path: "/proj/REBON.md",
            is_git: false,
            home_dir: "/home/u",
            cwd: "/proj",
            sep: '/',
        }
    }

    // ──────────────────────────────────────────────────────────────
    // missing-row injection
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn empty_existing_list_yields_user_and_project_stubs() {
        let opts = build_memory_options(&[], standard_inputs());
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "User memory");
        assert_eq!(opts[0].value, "/home/u/.rebon/REBON.md");
        assert_eq!(opts[1].label, "Project memory");
        assert_eq!(opts[1].value, "/proj/REBON.md");
    }

    #[test]
    fn missing_user_stub_described_as_saved_in() {
        let opts = build_memory_options(&[], standard_inputs());
        assert_eq!(opts[0].description, "Saved in ~/.rebon/REBON.md");
    }

    #[test]
    fn missing_project_stub_described_as_saved_in_when_not_git() {
        let opts = build_memory_options(&[], standard_inputs());
        assert_eq!(opts[1].description, "Saved in ./REBON.md");
    }

    #[test]
    fn missing_project_stub_described_as_checked_in_when_git() {
        let inputs = MemoryOptionsInputs {
            is_git: true,
            ..standard_inputs()
        };
        let opts = build_memory_options(&[], inputs);
        assert_eq!(opts[1].description, "Checked in at ./REBON.md");
    }

    #[test]
    fn existing_user_memory_suppresses_user_stub() {
        let existing = vec![MemoryFileInfo::new(
            "/home/u/.rebon/REBON.md",
            MemoryType::User,
        )];
        let opts = build_memory_options(&existing, standard_inputs());
        // 1 existing + 1 project stub = 2.
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "User memory");
        assert_eq!(opts[1].label, "Project memory");
    }

    #[test]
    fn existing_project_memory_suppresses_project_stub() {
        let existing = vec![MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project)];
        let opts = build_memory_options(&existing, standard_inputs());
        // 1 existing + 1 user stub = 2. The user stub is appended
        // BEFORE the project stub, but here only the project is
        // suppressed. Order is: existing project, then missing user
        // stub.
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Project memory");
        assert_eq!(opts[1].label, "User memory");
    }

    #[test]
    fn both_existing_yields_no_stubs() {
        let existing = vec![
            MemoryFileInfo::new("/home/u/.rebon/REBON.md", MemoryType::User),
            MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project),
        ];
        let opts = build_memory_options(&existing, standard_inputs());
        assert_eq!(opts.len(), 2);
    }

    // ──────────────────────────────────────────────────────────────
    // filter rule
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn auto_mem_files_are_filtered_out() {
        let existing = vec![
            MemoryFileInfo::new("/home/u/.rebon/auto/x.md", MemoryType::AutoMem),
            MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project),
        ];
        let opts = build_memory_options(&existing, standard_inputs());
        // AutoMem is filtered out and its path doesn't match
        // user_memory_path, so the user stub IS still injected.
        // Result: existing project, then user stub. Project stub is
        // suppressed because the existing project file matches the
        // canonical path.
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Project memory");
        assert_eq!(opts[1].label, "User memory");
    }

    #[test]
    fn team_mem_files_are_filtered_out() {
        let existing = vec![MemoryFileInfo::new(
            "/home/u/.rebon/team/x.md",
            MemoryType::TeamMem,
        )];
        let opts = build_memory_options(&existing, standard_inputs());
        // Team file filtered → only user + project stubs.
        assert_eq!(opts.len(), 2);
    }

    #[test]
    fn auto_mem_at_user_memory_path_still_counts_for_has_user_memory_check() {
        // The check scans the UNFILTERED file list for the
        // user_memory_path. So an
        // AutoMem file at the canonical user path suppresses the
        // user stub, even though the AutoMem file itself is
        // filtered out of the option list.
        let existing = vec![MemoryFileInfo::new(
            "/home/u/.rebon/REBON.md",
            MemoryType::AutoMem,
        )];
        let opts = build_memory_options(&existing, standard_inputs());
        // AutoMem filtered out + user stub suppressed → only the
        // project stub remains.
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].label, "Project memory");
    }

    // ──────────────────────────────────────────────────────────────
    // depth map
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn parent_then_child_yields_depth_1_for_child() {
        let existing = vec![
            MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project),
            MemoryFileInfo::new("/proj/child.md", MemoryType::Local).with_parent("/proj/REBON.md"),
        ];
        let opts = build_memory_options(&existing, standard_inputs());
        // Parent: "/proj/REBON.md" → "Project memory" (depth 0)
        // Child: "/proj/child.md" → depth 1, label uses `L `
        assert_eq!(opts[0].label, "Project memory");
        assert_eq!(opts[1].label, "L child.md");
        assert_eq!(opts[1].description, "@-imported");
    }

    #[test]
    fn grandchild_chains_depth() {
        let existing = vec![
            MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project),
            MemoryFileInfo::new("/proj/child.md", MemoryType::Local).with_parent("/proj/REBON.md"),
            MemoryFileInfo::new("/proj/grand.md", MemoryType::Local).with_parent("/proj/child.md"),
        ];
        let opts = build_memory_options(&existing, standard_inputs());
        // grand at depth 2 → indent " " + "L " + display
        assert_eq!(opts[2].label, "  L grand.md");
    }

    #[test]
    fn child_before_parent_falls_back_to_depth_1() {
        // Depth rule: `depths.get(file.parent)  defaulting to  0) + 1`. If the
        // parent hasn't been seen yet, default 0 → child depth 1.
        let existing = vec![
            MemoryFileInfo::new("/proj/child.md", MemoryType::Local).with_parent("/proj/REBON.md"),
            MemoryFileInfo::new("/proj/REBON.md", MemoryType::Project),
        ];
        let opts = build_memory_options(&existing, standard_inputs());
        // child appears first, parent unseen → depth 1
        assert_eq!(opts[0].label, "L child.md");
    }

    // ──────────────────────────────────────────────────────────────
    // value field
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn value_field_is_file_path() {
        let existing = vec![MemoryFileInfo::new("/proj/sub/x.md", MemoryType::Local)];
        let opts = build_memory_options(&existing, standard_inputs());
        // First option is the local file, then user stub, then
        // project stub.
        assert_eq!(opts[0].value, "/proj/sub/x.md");
    }

    #[test]
    fn value_field_for_missing_user_stub_is_canonical_user_path() {
        let opts = build_memory_options(&[], standard_inputs());
        assert_eq!(opts[0].value, "/home/u/.rebon/REBON.md");
    }

    // ──────────────────────────────────────────────────────────────
    // ordering
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn order_is_existing_then_user_stub_then_project_stub() {
        let existing = vec![MemoryFileInfo::new("/proj/sub/x.md", MemoryType::Local)];
        let opts = build_memory_options(&existing, standard_inputs());
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].value, "/proj/sub/x.md");
        assert_eq!(opts[1].value, "/home/u/.rebon/REBON.md");
        assert_eq!(opts[2].value, "/proj/REBON.md");
    }

    #[test]
    fn unrelated_files_keep_iteration_order() {
        let existing = vec![
            MemoryFileInfo::new("/a/1.md", MemoryType::Local),
            MemoryFileInfo::new("/a/2.md", MemoryType::Local),
            MemoryFileInfo::new("/a/3.md", MemoryType::Local),
        ];
        let opts = build_memory_options(&existing, standard_inputs());
        assert_eq!(opts[0].value, "/a/1.md");
        assert_eq!(opts[1].value, "/a/2.md");
        assert_eq!(opts[2].value, "/a/3.md");
    }

    // ──────────────────────────────────────────────────────────────
    // empty lists / boundary
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn label_uses_display_path_for_non_canonical_files() {
        let existing = vec![MemoryFileInfo::new("/proj/sub/x.md", MemoryType::Local)];
        let opts = build_memory_options(&existing, standard_inputs());
        // display_path("/proj/sub/x.md", "/home/u", "/proj", '/')
        // → "sub/x.md"
        assert_eq!(opts[0].label, "sub/x.md");
    }

    #[test]
    fn label_for_local_file_outside_cwd_uses_home_form() {
        let existing = vec![MemoryFileInfo::new("/home/u/foo/bar.md", MemoryType::Local)];
        let opts = build_memory_options(&existing, standard_inputs());
        // display_path → ~/foo/bar.md (it's inside home, outside cwd)
        assert_eq!(opts[0].label, "~/foo/bar.md");
    }
}
