//! Which tools a session does its file work with.
//!
//! The shell-first set is the default: reading and searching go through
//! Bash, every change goes through Edit, and Read, Write, Glob, Grep and
//! MultiEdit wait behind ToolSearch (Read is still how images, PDFs and
//! notebooks are read). An A/B against the dedicated file tools passed the
//! same tasks with the same number of requests, on fewer prompt tokens.
//! `REBON_TOOLSET=files` restores the dedicated file tools, eager, with the
//! prompt sending file work to them.
//!
//! The toolset is part of the lean surface. Under `REBON_PROMPT_PROFILE=legacy`
//! it is always the file tools, because that profile restores the old surface
//! whole.
//!
//! Like the profile, the variable is read on every call: whoever launches the
//! process sets it once, so the prompt it shapes stays byte-stable for the
//! session.

/// The environment variable that selects the toolset.
pub const TOOLSET_ENV: &str = "REBON_TOOLSET";

/// A file-tool surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Toolset {
    /// Bash for reading and searching, Edit for changes.
    #[default]
    Shell,
    /// Dedicated file tools, eager.
    Files,
}

impl Toolset {
    /// The toolset a raw variable value names. Only `files` (trimmed, any
    /// case) selects the dedicated file tools.
    pub fn from_value(value: Option<&str>) -> Self {
        match value {
            Some(value) if value.trim().eq_ignore_ascii_case("files") => Self::Files,
            _ => Self::Shell,
        }
    }

    /// The toolset this process runs with: the variable's choice on the lean
    /// profile, the file tools on the legacy one.
    pub fn current() -> Self {
        if crate::legacy_prompt_profile() {
            Self::Files
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
    fn only_files_selects_the_dedicated_file_tools() {
        assert_eq!(Toolset::default(), Toolset::Shell);
        assert_eq!(Toolset::from_value(None), Toolset::Shell);
        assert_eq!(Toolset::from_value(Some("")), Toolset::Shell);
        assert_eq!(Toolset::from_value(Some("shell")), Toolset::Shell);
        assert_eq!(Toolset::from_value(Some("standard")), Toolset::Shell);
        for spelling in ["files", " Files ", "FILES"] {
            assert_eq!(
                Toolset::from_value(Some(spelling)),
                Toolset::Files,
                "{spelling:?}"
            );
        }
    }
}
