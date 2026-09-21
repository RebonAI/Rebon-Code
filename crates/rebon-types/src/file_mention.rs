use std::fmt;
use std::path::{Path, PathBuf};

/// Maximum UTF-8 size accepted for an `@` file mention query.
pub const MAX_FILE_MENTION_QUERY_BYTES: usize = 1024;

/// A validated, normalized relative file mention query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMentionQuery {
    normalized: String,
    parent_depth: usize,
    relative_dir: String,
    display_dir: String,
    prefix: String,
}

/// Lexical paths derived from a validated file mention query and a cwd.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMentionLocation {
    /// The highest ancestor explicitly selected by leading `../` components.
    pub allowed_root: PathBuf,
    /// The directory whose direct children should be listed.
    pub target_dir: PathBuf,
}

/// Why a file mention query cannot be used for local completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileMentionQueryError {
    /// The query exceeds [`MAX_FILE_MENTION_QUERY_BYTES`].
    TooLong,
    /// The query contains whitespace or control characters.
    InvalidCharacter,
    /// The query is absolute, drive-relative, or uses a UNC/device prefix.
    AbsolutePath,
    /// The query contains `.`, an internal `..`, or an empty path component.
    InvalidComponent,
    /// Resolving the leading parent components would move above the filesystem root.
    AboveRoot,
    /// The supplied cwd is not absolute.
    RelativeCwd,
}

impl fmt::Display for FileMentionQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooLong => "file mention query is too long",
            Self::InvalidCharacter => "file mention query contains an invalid character",
            Self::AbsolutePath => "absolute file mention paths are not supported",
            Self::InvalidComponent => "file mention query contains an invalid path component",
            Self::AboveRoot => "file mention query moves above the filesystem root",
            Self::RelativeCwd => "file mention cwd must be absolute",
        })
    }
}

impl std::error::Error for FileMentionQueryError {}

impl FileMentionQuery {
    /// Parses a relative query, accepting only leading parent components.
    pub fn parse(query: &str) -> Result<Self, FileMentionQueryError> {
        if query.len() > MAX_FILE_MENTION_QUERY_BYTES {
            return Err(FileMentionQueryError::TooLong);
        }
        if query.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '\0'
        }) {
            return Err(FileMentionQueryError::InvalidCharacter);
        }

        let normalized = query.replace('\\', "/");
        if normalized.starts_with('/') || has_windows_drive_prefix(&normalized) {
            return Err(FileMentionQueryError::AbsolutePath);
        }

        let trailing_separator = normalized.ends_with('/');
        let mut components = if normalized.is_empty() {
            Vec::new()
        } else {
            normalized.split('/').collect::<Vec<_>>()
        };
        if trailing_separator {
            components.pop();
        }
        if components.iter().any(|component| component.is_empty()) {
            return Err(FileMentionQueryError::InvalidComponent);
        }

        let prefix = if trailing_separator || components.is_empty() {
            String::new()
        } else {
            components.pop().expect("non-empty components").to_string()
        };

        let mut parent_depth = 0;
        while components.first() == Some(&"..") {
            parent_depth += 1;
            components.remove(0);
        }
        if components
            .iter()
            .any(|component| *component == "." || *component == "..")
            || prefix == "."
            || (prefix == ".." && parent_depth > 0)
        {
            return Err(FileMentionQueryError::InvalidComponent);
        }

        let relative_dir = components.join("/");
        let mut display_components = vec![".."; parent_depth];
        display_components.extend(components.iter().copied());
        let display_dir = display_components.join("/");

        Ok(Self {
            normalized,
            parent_depth,
            relative_dir,
            display_dir,
            prefix,
        })
    }

    /// Returns the normalized query using `/` separators.
    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    /// Returns the number of explicit leading parent components.
    pub fn parent_depth(&self) -> usize {
        self.parent_depth
    }

    /// Returns the directory below the explicitly selected ancestor.
    pub fn relative_dir(&self) -> &str {
        &self.relative_dir
    }

    /// Returns the directory spelling that should prefix displayed candidates.
    pub fn display_dir(&self) -> &str {
        &self.display_dir
    }

    /// Returns the final filename prefix used to filter direct children.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Returns whether the query explicitly uses path separators or parent traversal.
    pub fn is_path_query(&self) -> bool {
        self.normalized.contains('/') || self.parent_depth > 0
    }

    /// Resolves lexical scan paths without following symlinks.
    pub fn resolve_from(&self, cwd: &Path) -> Result<FileMentionLocation, FileMentionQueryError> {
        if !cwd.is_absolute() {
            return Err(FileMentionQueryError::RelativeCwd);
        }

        let mut allowed_root = cwd.to_path_buf();
        for _ in 0..self.parent_depth {
            allowed_root = allowed_root
                .parent()
                .filter(|parent| *parent != allowed_root)
                .ok_or(FileMentionQueryError::AboveRoot)?
                .to_path_buf();
        }
        let target_dir = if self.relative_dir.is_empty() {
            allowed_root.clone()
        } else {
            allowed_root.join(Path::new(&self.relative_dir))
        };
        Ok(FileMentionLocation {
            allowed_root,
            target_dir,
        })
    }

    /// Builds a normalized candidate path for a direct child name.
    pub fn candidate_path(&self, name: &str) -> Option<String> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['/', '\\'])
            || name
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return None;
        }
        let candidate = if self.display_dir.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.display_dir)
        };
        if candidate.len() > MAX_FILE_MENTION_QUERY_BYTES || has_windows_drive_prefix(&candidate) {
            return None;
        }
        Some(candidate)
    }
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_leading_parent_components() {
        let query = FileMentionQuery::parse(r"..\..\shared\ma").unwrap();
        assert_eq!(query.normalized(), "../../shared/ma");
        assert_eq!(query.parent_depth(), 2);
        assert_eq!(query.relative_dir(), "shared");
        assert_eq!(query.display_dir(), "../../shared");
        assert_eq!(query.prefix(), "ma");
        assert!(query.is_path_query());
        assert_eq!(
            query.candidate_path("main.rs").as_deref(),
            Some("../../shared/main.rs")
        );
    }

    #[test]
    fn parses_directory_queries_and_plain_prefixes() {
        let parent = FileMentionQuery::parse("../").unwrap();
        assert_eq!(parent.parent_depth(), 1);
        assert_eq!(parent.display_dir(), "..");
        assert_eq!(parent.prefix(), "");

        let nested = FileMentionQuery::parse("src/").unwrap();
        assert_eq!(nested.relative_dir(), "src");
        assert_eq!(nested.display_dir(), "src");
        assert_eq!(nested.prefix(), "");

        let plain = FileMentionQuery::parse("main").unwrap();
        assert!(!plain.is_path_query());
        assert_eq!(plain.prefix(), "main");
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_queries() {
        for query in [
            "/etc",
            "C:/Users",
            "C:relative",
            r"\\server\share",
            r"\\?\C:\Users",
            "./foo",
            "foo/../bar",
            "foo//bar",
            "../..",
            "../foo bar",
        ] {
            assert!(
                FileMentionQuery::parse(query).is_err(),
                "accepted {query:?}"
            );
        }
    }

    #[test]
    fn resolves_requested_ancestor_and_target() {
        let cwd = if cfg!(windows) {
            Path::new("C:/work/project")
        } else {
            Path::new("/work/project")
        };
        let query = FileMentionQuery::parse("../sibling/src/").unwrap();
        let location = query.resolve_from(cwd).unwrap();
        let expected_root = cwd.parent().unwrap();
        assert_eq!(location.allowed_root, expected_root);
        assert_eq!(location.target_dir, expected_root.join("sibling/src"));
    }

    #[test]
    fn rejects_moving_above_root() {
        let root = if cfg!(windows) {
            Path::new("C:/")
        } else {
            Path::new("/")
        };
        assert_eq!(
            FileMentionQuery::parse("../").unwrap().resolve_from(root),
            Err(FileMentionQueryError::AboveRoot)
        );
    }

    #[test]
    fn filters_unrepresentable_candidate_names() {
        let query = FileMentionQuery::parse("../").unwrap();
        assert_eq!(query.candidate_path("hello world"), None);
        assert_eq!(query.candidate_path("nested/name"), None);
        assert_eq!(query.candidate_path("\n"), None);

        let plain = FileMentionQuery::parse("").unwrap();
        assert_eq!(plain.candidate_path("C:notes.txt"), None);

        let directory = format!("{}/", "a".repeat(MAX_FILE_MENTION_QUERY_BYTES - 1));
        let query = FileMentionQuery::parse(&directory).unwrap();
        assert_eq!(query.candidate_path("b"), None);
    }
}
