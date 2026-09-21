//! Serving a masked credential where the filesystem cannot be redirected.
//!
//! On Linux a `credentials.files` mask is a read-only bind of the fake file
//! over the real one, and the command reads the fake bytes without knowing
//! anything happened. macOS and Windows have no equivalent, so the mask
//! degrades to a plain denial: the command gets a permission error where it
//! expected a credential.
//!
//! That degradation is correct and stays. This module is a second, narrower
//! mechanism layered *on top of* it — never instead of it.
//!
//! ## Why not the interposer the RFC named
//!
//! RFC §14 called for a `DYLD_INSERT_LIBRARIES` interposer on macOS. It
//! cannot work, and the way it fails is the reason it must not be built:
//! `dyld` strips every `DYLD_*` variable when the program being launched is a
//! SIP-protected platform binary or is signed with library validation. The
//! sandbox reaches a tool through `/usr/bin/sandbox-exec` and then `/bin/sh`,
//! both platform binaries, so the variable is gone before the tool starts.
//! Homebrew binaries are often ad-hoc signed and *would* load it.
//!
//! So an interposer would apply to some commands and not others, invisibly,
//! with nothing in the output distinguishing the two. A credential mask that
//! silently does not apply is worse than no mask at all: the caller believes
//! a fake token was served and the real one was read.
//!
//! ## What this does instead
//!
//! Most things people mask are dotfiles whose location a tool will take from
//! an environment variable. Point the variable at the fake file and the tool
//! opens the fake one by its own choice — no interposition, no filesystem
//! trick, and it works the same on every platform.
//!
//! ## Why this cannot make things worse
//!
//! The deny ACE or the seatbelt `(deny file-read* ...)` on the real path is
//! applied either way. So the two outcomes are:
//!
//! * the tool honours the variable and reads the fake file — the mask worked;
//! * the tool ignores it and opens the real path — and is denied, exactly as
//!   it would have been without this.
//!
//! There is no third case where the real credential is read. That property is
//! what makes a best-effort table acceptable here; a redirect that *replaced*
//! the denial would need to be exhaustive, and could not be.

use crate::runtime::config::MaskedFileBind;
use std::path::Path;

/// One tool's environment variable and the file it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskRedirect {
    pub variable: String,
    pub value: String,
    /// The path the rule was written about, for the warning text.
    pub real: String,
}

/// The table.
///
/// Each entry is a file name (or trailing path) and the variable that names
/// it. **Only variables whose value is a path to the file itself** — several
/// tools take a *directory* instead (`DOCKER_CONFIG`, `CARGO_HOME`), and
/// pointing those at a synthesized directory means materialising a tree, not
/// naming a file. That is a different feature and is not this one.
///
/// Kept small and specific on purpose: every entry is a claim that this
/// variable, on the current version of that tool, names this file. A wrong
/// entry costs a mask that does not apply — which lands on the denial, and is
/// the same outcome as not having the entry.
const TABLE: &[(&str, &str)] = &[
    // npm ≥ 5. `NPM_CONFIG_USERCONFIG` is the per-user config path.
    (".npmrc", "NPM_CONFIG_USERCONFIG"),
    // git ≥ 2.32. Older git ignores it and falls through to the denial.
    (".gitconfig", "GIT_CONFIG_GLOBAL"),
    // The AWS SDKs and CLI, all languages.
    (".aws/credentials", "AWS_SHARED_CREDENTIALS_FILE"),
    (".aws/config", "AWS_CONFIG_FILE"),
    // kubectl and client-go. The variable is a path list; one entry is valid.
    (".kube/config", "KUBECONFIG"),
];

/// The redirect for one masked file, if this table knows the tool.
///
/// Matched on the trailing path components rather than the whole path: the
/// rule is about `~/.aws/credentials` wherever the home directory is, and a
/// user who masks a copy under another root means the same thing by it.
pub fn redirect_for(bind: &MaskedFileBind) -> Option<MaskRedirect> {
    let real = normalise(&bind.real);
    TABLE
        .iter()
        .find(|(suffix, _)| matches_suffix(&real, suffix))
        .map(|(_, variable)| MaskRedirect {
            variable: (*variable).to_string(),
            value: bind.fake.to_string_lossy().into_owned(),
            real: bind.real.to_string_lossy().into_owned(),
        })
}

/// Every redirect a set of masks produces.
///
/// Order follows the input so two rules naming the same variable resolve the
/// way the caller wrote them, and the caller's own `env_set` still wins —
/// [`crate::runtime::env::build_env_plan`] applies credential and caller rules after
/// everything else.
pub fn redirects_for(binds: &[MaskedFileBind]) -> Vec<MaskRedirect> {
    binds.iter().filter_map(redirect_for).collect()
}

/// Lower-cased with forward slashes, so one table serves every platform.
fn normalise(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Whether `path` ends with `suffix` **on a component boundary**.
///
/// A plain `ends_with` would match `/home/me/evil.npmrc` against `.npmrc` and
/// point npm at somebody's mask for a file it was never told about.
fn matches_suffix(path: &str, suffix: &str) -> bool {
    let Some(head) = path.strip_suffix(suffix) else {
        return false;
    };
    head.is_empty() || head.ends_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bind(real: &str, fake: &str) -> MaskedFileBind {
        MaskedFileBind {
            real: PathBuf::from(real),
            fake: PathBuf::from(fake),
        }
    }

    #[test]
    fn a_known_dotfile_gets_its_variable() {
        let redirect = redirect_for(&bind("/Users/me/.npmrc", "/tmp/fake-npmrc")).unwrap();
        assert_eq!(redirect.variable, "NPM_CONFIG_USERCONFIG");
        assert_eq!(redirect.value, "/tmp/fake-npmrc");
    }

    #[test]
    fn a_nested_credential_path_is_matched_whole() {
        let redirect = redirect_for(&bind("/Users/me/.aws/credentials", "/tmp/fake-aws")).unwrap();
        assert_eq!(redirect.variable, "AWS_SHARED_CREDENTIALS_FILE");
    }

    #[test]
    fn windows_paths_match_the_same_entries() {
        // One table for every platform; the mask degrades on Windows too.
        let redirect = redirect_for(&bind(r"C:\Users\me\.npmrc", r"C:\temp\fake")).unwrap();
        assert_eq!(redirect.variable, "NPM_CONFIG_USERCONFIG");
    }

    #[test]
    fn a_name_that_merely_ends_the_same_way_is_not_matched() {
        // The bug this guards: `evil.npmrc` is not `.npmrc`, and matching it
        // would point npm at a mask written for a different file entirely.
        assert!(redirect_for(&bind("/Users/me/evil.npmrc", "/tmp/fake")).is_none());
        assert!(redirect_for(&bind("/Users/me/notgitconfig", "/tmp/fake")).is_none());
    }

    #[test]
    fn a_partial_directory_match_is_not_enough() {
        // `.aws/credentials` must match the two components together, not
        // `credentials` under any directory.
        assert!(redirect_for(&bind("/Users/me/other/credentials", "/tmp/f")).is_none());
    }

    #[test]
    fn an_unknown_file_gets_nothing_and_that_is_fine() {
        // It stays denied, which is what it would have been anyway. The table
        // only ever upgrades a denial into a served fake.
        assert!(redirect_for(&bind("/Users/me/.pypirc", "/tmp/fake")).is_none());
        assert!(redirect_for(&bind("/etc/some-secret", "/tmp/fake")).is_none());
    }

    #[test]
    fn several_masks_produce_several_redirects_in_order() {
        let redirects = redirects_for(&[
            bind("/Users/me/.npmrc", "/tmp/a"),
            bind("/Users/me/.secret", "/tmp/b"),
            bind("/Users/me/.gitconfig", "/tmp/c"),
        ]);
        assert_eq!(redirects.len(), 2);
        assert_eq!(redirects[0].variable, "NPM_CONFIG_USERCONFIG");
        assert_eq!(redirects[1].variable, "GIT_CONFIG_GLOBAL");
    }

    #[test]
    fn no_entry_names_a_variable_that_wants_a_directory() {
        // `DOCKER_CONFIG` and `CARGO_HOME` name directories. Pointing one at
        // a file makes the tool fail to read its own config rather than read
        // a fake one — a worse outcome than the denial being upgraded.
        for (_, variable) in TABLE {
            assert!(
                !matches!(
                    *variable,
                    "DOCKER_CONFIG" | "CARGO_HOME" | "XDG_CONFIG_HOME"
                ),
                "{variable} names a directory"
            );
        }
    }

    #[test]
    fn the_table_has_no_duplicate_files() {
        // Two entries for one file means the second is unreachable, and
        // whichever tool it was for silently never gets its redirect.
        let mut names: Vec<&str> = TABLE.iter().map(|(name, _)| *name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn every_table_entry_is_lower_case() {
        // `normalise` lower-cases the path before matching, so an entry with
        // a capital in it could never match anything.
        for (name, _) in TABLE {
            assert_eq!(*name, name.to_lowercase(), "{name} would never match");
        }
    }
}
