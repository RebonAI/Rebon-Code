//! `MemoryType` discriminant — which of the six places a memory file
//! came from.
//!
//! The six variants, in declaration order:
//!
//! * `User` — top-level user file (`~/.rebon/REBON.md`)
//! * `Project` — top-level project file (`./REBON.md`)
//! * `Local` — workspace-local file
//! * `Managed` — managed (org-pushed) memory rule
//! * `AutoMem` — auto-memory entry (filtered out of the selector)
//! * `TeamMem` — team-memory entry (filtered out of the selector)
//!
//! The selector keeps every type except `AutoMem` and `TeamMem`, and
//! inserts a `User` or `Project` stub row when no file of that type was
//! found.
//!
//! ## What this module implements
//!
//! * The `MemoryType` enum.
//! * The `is_excluded_from_selector` filter for the two selector-excluded
//!   types, and its `keep_in_selector` inverse.
//! * `parse` / `as_str` round-trip so the consumer can read the
//! raw JSON / settings token.
//! * The `is_managed_or_user_owned` predicate (purely informational —
//! the description builder decides on `User` itself).

use core::fmt;

/// One member of [`MemoryType`].
///
/// The enum is not `#[non_exhaustive]` so a `match` against it is
/// guaranteed to cover every variant. Adding a variant is
/// a breaking change to this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MemoryType {
    /// Top-level user file. Lives at `~/.rebon/REBON.md`, under the
    /// resolved config home.
    User,
    /// Top-level project file. Lives at the session's original working
    /// directory, as `./REBON.md`.
    Project,
    /// Workspace-local file. The selector treats it like any other
    /// non-User non-Project entry — it gets the standard
    /// relative-path label.
    Local,
    /// Managed (org-pushed) memory rule. The selector treats it like
    /// `Local` for label/description purposes.
    Managed,
    /// Auto-memory entry. **Filtered out** of the selector — see
    /// [`Self::is_excluded_from_selector`].
    AutoMem,
    /// Team-memory entry. **Filtered out** of the selector — see
    /// [`Self::is_excluded_from_selector`].
    TeamMem,
}

impl MemoryType {
    /// The wire / settings string for this type — round-trip with
    /// [`Self::parse`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Project => "Project",
            Self::Local => "Local",
            Self::Managed => "Managed",
            Self::AutoMem => "AutoMem",
            Self::TeamMem => "TeamMem",
        }
    }

    /// Parse the wire / settings string back into a [`MemoryType`].
    /// Returns `None` for any unknown token, which is a programming
    /// error rather than a value with a default.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "User" => Some(Self::User),
            "Project" => Some(Self::Project),
            "Local" => Some(Self::Local),
            "Managed" => Some(Self::Managed),
            "AutoMem" => Some(Self::AutoMem),
            "TeamMem" => Some(Self::TeamMem),
            _ => None,
        }
    }

    /// The selector's filter: `AutoMem` and `TeamMem` are the two
    /// types it leaves out, everything else is kept.
    ///
    /// Returns `true` for the two types that the selector
    /// **excludes**, `false` otherwise, so that callers can read it as
    /// "is this entry excluded from the selector?".
    pub const fn is_excluded_from_selector(self) -> bool {
        matches!(self, Self::AutoMem | Self::TeamMem)
    }

    /// Inverse of [`Self::is_excluded_from_selector`] — the predicate
    /// a caller reads when it wants the types to keep.
    pub const fn keep_in_selector(self) -> bool {
        !self.is_excluded_from_selector()
    }

    /// `true` for `User` / `Managed` (the two "owned by the user
    /// or by the organisation" sources). The selector doesn't
    /// branch on this directly but the description-builder treats
    /// `User` specially.
    pub const fn is_managed_or_user_owned(self) -> bool {
        matches!(self, Self::User | Self::Managed)
    }

    /// Iterator over every variant in declaration order.
    pub fn all() -> impl Iterator<Item = Self> {
        [
            Self::User,
            Self::Project,
            Self::Local,
            Self::Managed,
            Self::AutoMem,
            Self::TeamMem,
        ]
        .into_iter()
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_user_is_expected() {
        assert_eq!(MemoryType::User.as_str(), "User");
    }

    #[test]
    fn as_str_project_is_expected() {
        assert_eq!(MemoryType::Project.as_str(), "Project");
    }

    #[test]
    fn as_str_local_is_expected() {
        assert_eq!(MemoryType::Local.as_str(), "Local");
    }

    #[test]
    fn as_str_managed_is_expected() {
        assert_eq!(MemoryType::Managed.as_str(), "Managed");
    }

    #[test]
    fn as_str_auto_mem_is_expected() {
        assert_eq!(MemoryType::AutoMem.as_str(), "AutoMem");
    }

    #[test]
    fn as_str_team_mem_is_expected() {
        assert_eq!(MemoryType::TeamMem.as_str(), "TeamMem");
    }

    #[test]
    fn parse_user_round_trips() {
        assert_eq!(MemoryType::parse("User"), Some(MemoryType::User));
    }

    #[test]
    fn parse_round_trips_for_every_variant() {
        for variant in MemoryType::all() {
            assert_eq!(MemoryType::parse(variant.as_str()), Some(variant));
        }
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert_eq!(MemoryType::parse("not_a_memory_type"), None);
    }

    #[test]
    fn parse_is_case_sensitive() {
        // The comparison is against the exact literal, so case matters.
        assert_eq!(MemoryType::parse("user"), None);
        assert_eq!(MemoryType::parse("USER"), None);
    }

    #[test]
    fn parse_empty_string_returns_none() {
        assert_eq!(MemoryType::parse(""), None);
    }

    #[test]
    fn is_excluded_auto_mem_is_true() {
        assert!(MemoryType::AutoMem.is_excluded_from_selector());
    }

    #[test]
    fn is_excluded_team_mem_is_true() {
        assert!(MemoryType::TeamMem.is_excluded_from_selector());
    }

    #[test]
    fn is_excluded_user_is_false() {
        assert!(!MemoryType::User.is_excluded_from_selector());
    }

    #[test]
    fn is_excluded_project_is_false() {
        assert!(!MemoryType::Project.is_excluded_from_selector());
    }

    #[test]
    fn is_excluded_local_is_false() {
        assert!(!MemoryType::Local.is_excluded_from_selector());
    }

    #[test]
    fn is_excluded_managed_is_false() {
        assert!(!MemoryType::Managed.is_excluded_from_selector());
    }

    #[test]
    fn keep_in_selector_is_inverse_of_excluded() {
        for variant in MemoryType::all() {
            assert_eq!(
                variant.keep_in_selector(),
                !variant.is_excluded_from_selector()
            );
        }
    }

    #[test]
    fn display_uses_wire_string() {
        assert_eq!(format!("{}", MemoryType::Project), "Project");
    }

    #[test]
    fn is_managed_or_user_owned_user_is_true() {
        assert!(MemoryType::User.is_managed_or_user_owned());
    }

    #[test]
    fn is_managed_or_user_owned_managed_is_true() {
        assert!(MemoryType::Managed.is_managed_or_user_owned());
    }

    #[test]
    fn is_managed_or_user_owned_project_is_false() {
        assert!(!MemoryType::Project.is_managed_or_user_owned());
    }

    #[test]
    fn is_excluded_from_selector_table() {
        // Each row pins one variant against the expected filter
        // outcome (`true` = excluded from selector, `false` = kept).
        let cases: &[(MemoryType, bool)] = &[
            (MemoryType::User, false),
            (MemoryType::Project, false),
            (MemoryType::Local, false),
            (MemoryType::Managed, false),
            (MemoryType::AutoMem, true),
            (MemoryType::TeamMem, true),
        ];
        for (variant, expected_excluded) in cases {
            assert_eq!(
                variant.is_excluded_from_selector(),
                *expected_excluded,
                "filter mismatch for {variant}"
            );
        }
    }

    #[test]
    fn all_iterator_yields_six_variants() {
        let collected: Vec<_> = MemoryType::all().collect();
        assert_eq!(collected.len(), 6);
    }

    #[test]
    fn all_iterator_in_declaration_order() {
        let collected: Vec<_> = MemoryType::all().collect();
        assert_eq!(
            collected,
            vec![
                MemoryType::User,
                MemoryType::Project,
                MemoryType::Local,
                MemoryType::Managed,
                MemoryType::AutoMem,
                MemoryType::TeamMem,
            ]
        );
    }
}
