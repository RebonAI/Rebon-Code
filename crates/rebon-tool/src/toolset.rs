//! Which tools a session does its file work with.
//!
//! The standard toolset is the default: Read, Write, Glob and Grep are eager
//! and the prompt sends file work to them. `REBON_TOOLSET=shell` is the
//! shell-first set: reading and searching go through Bash, every change goes
//! through Edit, and Read, Write, Glob, Grep and MultiEdit wait behind
//! ToolSearch (Read is still how images, PDFs and notebooks are read).
//!
//! The toolset is part of the lean surface. Under `REBON_PROMPT_PROFILE=legacy`
//! it is always the standard one, because that profile restores the old
//! surface whole.
//!
//! Like the profile, the variable is read on every call: whoever launches the
//! process sets it once, so the prompt it shapes stays byte-stable for the
//! session.

/// The environment variable that selects the toolset.
pub const TOOLSET_ENV: &str = "REBON_TOOLSET";

/// A file-tool surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Toolset {
    /// Dedicated file tools, eager.
    #[default]
    Standard,
    /// Bash for reading and searching, Edit for changes.
    Shell,
}

impl Toolset {
    /// The toolset a raw variable value names. Only `shell` (trimmed, any
    /// case) selects the shell-first set.
    pub fn from_value(value: Option<&str>) -> Self {
        match value {
            Some(value) if value.trim().eq_ignore_ascii_case("shell") => Self::Shell,
            _ => Self::Standard,
        }
    }

    /// The toolset this process runs with: the variable's choice on the lean
    /// profile, the standard set on the legacy one.
    pub fn current() -> Self {
        if crate::legacy_prompt_profile() {
            Self::Standard
        } else {
            Self::from_value(std::env::var(TOOLSET_ENV).ok().as_deref())
        }
    }

    pub fn is_shell(self) -> bool {
        self == Self::Shell
    }
}

/// `true` when this process runs the shell-first toolset.
pub fn shell_toolset() -> bool {
    Toolset::current().is_shell()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_shell_selects_the_shell_first_set() {
        assert_eq!(Toolset::from_value(None), Toolset::Standard);
        assert_eq!(Toolset::from_value(Some("")), Toolset::Standard);
        assert_eq!(Toolset::from_value(Some("standard")), Toolset::Standard);
        assert_eq!(Toolset::from_value(Some("bash")), Toolset::Standard);
        for spelling in ["shell", " Shell ", "SHELL"] {
            assert_eq!(
                Toolset::from_value(Some(spelling)),
                Toolset::Shell,
                "{spelling:?}"
            );
        }
    }
}
