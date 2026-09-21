//! `MemoryFileInfo` struct + the `ExtendedMemoryFileInfo` overlay.
//!
//! ## Shape
//!
//! A memory file carries a path, type, content, and optional metadata
//! about the file that included it, path globs from frontmatter,
//! content that differs from disk, and raw on-disk content. The
//! selector overlay adds an `is_nested` flag and an `exists` flag.
//!
//! Missing-row stubs are for the files the discovery walk did not find:
//! a User or Project memory file that is absent still gets a row, with
//! an empty `content` and `exists: false`.
//!
//! ## What this module implements
//!
//! * The `MemoryFileInfo` struct (the in-memory shape — parsing is a
//! separate concern).
//! * The `ExtendedMemoryFileInfo` overlay used by the selector.
//! * Builder helpers `MemoryFileInfo::new` and
//! `ExtendedMemoryFileInfo::missing_user_stub` /
//! `missing_project_stub`.

use crate::memory_type::MemoryType;

/// In-memory shape of a memory file.
///
/// All fields are owned `String`s / owned `Vec<String>`s — nothing
/// here borrows from the parser that produced them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemoryFileInfo {
    /// Absolute path to the memory file. The selector matches rows
    /// on this value (so it must be globally unique).
    pub path: String,
    /// Type discriminant — the selector branches on this for the
    /// label / description / filter rules.
    pub r#type: MemoryType,
    /// Raw content of the memory file. The selector does not read
    /// this, but the callers that build the memory prompt do.
    pub content: String,
    /// Path of the file that `@`-included this one, if any. Used by
    /// the depth calculator that orders nested files.
    pub parent: Option<String>,
    /// Glob patterns for the file paths this rule applies to. Set
    /// when the file's frontmatter declares `paths:`. Not consumed
    /// by the selector but pinned so it round-trips.
    pub globs: Option<Vec<String>>,
    /// `true` when auto-injection transformed `content` such that
    /// it no longer matches the bytes on disk. Pinned so it
    /// round-trips; not consumed by the selector.
    pub content_differs_from_disk: bool,
    /// Raw on-disk content when [`Self::content_differs_from_disk`]
    /// is set. Not consumed by the selector.
    pub raw_content: Option<String>,
}

impl MemoryFileInfo {
    /// Construct a [`MemoryFileInfo`] with the minimum set of
    /// fields the selector reads. All optional fields default to
    /// `None`/`false`.
    pub fn new(path: impl Into<String>, r#type: MemoryType) -> Self {
        Self {
            path: path.into(),
            r#type,
            content: String::new(),
            parent: None,
            globs: None,
            content_differs_from_disk: false,
            raw_content: None,
        }
    }

    /// Builder: attach a `parent` (the path of the file that
    /// `@`-included this one).
    pub fn with_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// Builder: attach raw on-disk content + the
    /// `content_differs_from_disk` flag.
    pub fn with_raw_content(mut self, raw: impl Into<String>) -> Self {
        self.raw_content = Some(raw.into());
        self.content_differs_from_disk = true;
        self
    }

    /// Builder: attach a content blob.
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }

    /// Builder: attach a globs vector.
    pub fn with_globs(mut self, globs: Vec<String>) -> Self {
        self.globs = Some(globs);
        self
    }
}

/// Overlay struct: the inner [`MemoryFileInfo`] plus two flags the
/// selector needs.
///
/// * `is_nested` — `true` for files discovered by the
/// recursive rule / nested-directory walks. The label and
/// description branch on it.
/// * `exists` — `false` for the missing-row stubs the selector
/// injects when no User / Project memory file was found. When
/// `false`, the label gets a `" (new)"` suffix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtendedMemoryFileInfo {
    /// The inner memory file. Owned, not borrowed — the selector
    /// builds new structs out of the pre-resolved file list.
    pub inner: MemoryFileInfo,
    /// `true` for files discovered by the recursive memory walk.
    /// Defaults to `false`.
    pub is_nested: bool,
    /// `true` if the file exists on disk. `false` for the
    /// missing-row stubs.
    pub exists: bool,
}

impl ExtendedMemoryFileInfo {
    /// Wrap a file the discovery walk found: it exists on disk, so
    /// `exists` is `true` and `is_nested` starts `false`.
    pub fn existing(inner: MemoryFileInfo) -> Self {
        Self {
            inner,
            is_nested: false,
            exists: true,
        }
    }

    /// Build the stub row for a User memory file that was not found.
    pub fn missing_user_stub(user_memory_path: impl Into<String>) -> Self {
        Self {
            inner: MemoryFileInfo::new(user_memory_path, MemoryType::User),
            is_nested: false,
            exists: false,
        }
    }

    /// Build the stub row for a Project memory file that was not
    /// found.
    pub fn missing_project_stub(project_memory_path: impl Into<String>) -> Self {
        Self {
            inner: MemoryFileInfo::new(project_memory_path, MemoryType::Project),
            is_nested: false,
            exists: false,
        }
    }

    /// Mark the inner file as `is_nested = true`: it was found by the
    /// nested-directory walk rather than at the top level.
    pub fn nested(mut self) -> Self {
        self.is_nested = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_round_trips_minimum_fields() {
        let info = MemoryFileInfo::new("/tmp/REBON.md", MemoryType::Project);
        assert_eq!(info.path, "/tmp/REBON.md");
        assert_eq!(info.r#type, MemoryType::Project);
        assert!(info.content.is_empty());
        assert_eq!(info.parent, None);
        assert_eq!(info.globs, None);
        assert!(!info.content_differs_from_disk);
        assert_eq!(info.raw_content, None);
    }

    #[test]
    fn with_parent_sets_parent_field() {
        let info = MemoryFileInfo::new("/a", MemoryType::Local).with_parent("/b");
        assert_eq!(info.parent.as_deref(), Some("/b"));
    }

    #[test]
    fn with_raw_content_sets_diff_flag() {
        let info = MemoryFileInfo::new("/a", MemoryType::Local).with_raw_content("raw bytes");
        assert!(info.content_differs_from_disk);
        assert_eq!(info.raw_content.as_deref(), Some("raw bytes"));
    }

    #[test]
    fn with_content_sets_content_field() {
        let info = MemoryFileInfo::new("/a", MemoryType::Local).with_content("hello");
        assert_eq!(info.content, "hello");
    }

    #[test]
    fn with_globs_sets_globs_field() {
        let info = MemoryFileInfo::new("/a", MemoryType::Local)
            .with_globs(vec!["src/**/*.ts".to_string()]);
        assert_eq!(info.globs, Some(vec!["src/**/*.ts".to_string()]));
    }

    #[test]
    fn existing_marks_exists_true() {
        let inner = MemoryFileInfo::new("/a", MemoryType::Project);
        let ext = ExtendedMemoryFileInfo::existing(inner);
        assert!(ext.exists);
        assert!(!ext.is_nested);
    }

    #[test]
    fn missing_user_stub_has_expected_shape() {
        // The stub keeps the asked-for path and the User type, with
        // empty content and neither flag set.
        let stub = ExtendedMemoryFileInfo::missing_user_stub("/home/u/.rebon/REBON.md");
        assert_eq!(stub.inner.path, "/home/u/.rebon/REBON.md");
        assert_eq!(stub.inner.r#type, MemoryType::User);
        assert!(stub.inner.content.is_empty());
        assert!(!stub.exists);
        assert!(!stub.is_nested);
    }

    #[test]
    fn missing_project_stub_has_expected_shape() {
        // The stub keeps the asked-for path and the Project type, with
        // empty content and neither flag set.
        let stub = ExtendedMemoryFileInfo::missing_project_stub("/cwd/REBON.md");
        assert_eq!(stub.inner.path, "/cwd/REBON.md");
        assert_eq!(stub.inner.r#type, MemoryType::Project);
        assert!(stub.inner.content.is_empty());
        assert!(!stub.exists);
        assert!(!stub.is_nested);
    }

    #[test]
    fn nested_marker_flips_is_nested_true() {
        let inner = MemoryFileInfo::new("/a", MemoryType::Local);
        let ext = ExtendedMemoryFileInfo::existing(inner).nested();
        assert!(ext.is_nested);
        assert!(ext.exists);
    }

    #[test]
    fn missing_stubs_are_not_marked_nested() {
        // Missing stubs are built without the nested marker, so
        // `is_nested` stays false. Pin it.
        let user = ExtendedMemoryFileInfo::missing_user_stub("/u");
        let project = ExtendedMemoryFileInfo::missing_project_stub("/p");
        assert!(!user.is_nested);
        assert!(!project.is_nested);
    }
}
