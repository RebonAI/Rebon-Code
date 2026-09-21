//! Which ACEs to write, with what rights, in what order.
//!
//! ## Order is the whole mechanism
//!
//! Windows walks a DACL from the top and stops at the first ACE that matches the
//! requested access. A deny that sorts after a grant is never reached. So "deny
//! before grant" is not a style rule, it is the difference between a confined
//! command and one that merely looks confined — [`is_correctly_ordered`] exists
//! so the sys layer can assert it right before it writes, and so the property is
//! tested on machines that have no DACLs at all.
//!
//! ## The rights are spelled out, not borrowed from `FILE_GENERIC_*`
//!
//! A write grant taken literally as `FILE_GENERIC_WRITE` produces a write root
//! the command can create files in but not read back, which fails the very first
//! build tool that reads a file it just wrote. A usable working directory is
//! read + write + execute + delete, and the read side costs nothing because deny
//! ACEs sort first: an explicit `--deny-read` inside a write root still wins.
//!
//! What a grant deliberately does **not** include is `WRITE_DAC` and
//! `WRITE_OWNER`. A sandbox that can rewrite a DACL can delete the deny ACE
//! confining it, and the fact that the ACE it would rewrite sits inside its own
//! write root does not make that acceptable. The deny side asks for the same two
//! rights explicitly, so a path under both rules is doubly covered.

use crate::core::argv::ExecRequest;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Individual access rights, so a mask can be read without decoding hex.
pub mod rights {
    pub const FILE_READ_DATA: u32 = 0x0000_0001;
    pub const FILE_WRITE_DATA: u32 = 0x0000_0002;
    pub const FILE_APPEND_DATA: u32 = 0x0000_0004;
    pub const FILE_READ_EA: u32 = 0x0000_0008;
    pub const FILE_WRITE_EA: u32 = 0x0000_0010;
    pub const FILE_EXECUTE: u32 = 0x0000_0020;
    pub const FILE_DELETE_CHILD: u32 = 0x0000_0040;
    pub const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    pub const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    pub const DELETE: u32 = 0x0001_0000;
    pub const READ_CONTROL: u32 = 0x0002_0000;
    pub const WRITE_DAC: u32 = 0x0004_0000;
    pub const WRITE_OWNER: u32 = 0x0008_0000;
    pub const SYNCHRONIZE: u32 = 0x0010_0000;
}

/// ACE inheritance flags.
pub mod inheritance {
    pub const OBJECT_INHERIT_ACE: u32 = 0x01;
    pub const CONTAINER_INHERIT_ACE: u32 = 0x02;
}

/// What an ACE does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AceKind {
    DenyRead,
    DenyWrite,
    AllowWrite,
    /// Deny running a file at all — used by `install` on the helper's own binary,
    /// never by [`plan_aces`].
    ///
    /// Not reachable from the `exec` grammar: [`AceOrigin`] has no variant for it,
    /// because no argument produces one. It exists because `install` gives the
    /// helper binary a deny-execute of its own, and that is a machine-lifetime ACE
    /// rather than a per-command one.
    DenyExecute,
}

impl AceKind {
    /// Whether this ACE denies.
    pub const fn is_deny(self) -> bool {
        matches!(
            self,
            AceKind::DenyRead | AceKind::DenyWrite | AceKind::DenyExecute
        )
    }

    /// The access mask to write.
    pub const fn access_mask(self) -> u32 {
        use rights::*;
        match self {
            // Deliberately narrow: contents and extended attributes. On a directory
            // `FILE_READ_DATA` is `FILE_LIST_DIRECTORY`, so the listing goes too.
            // Attributes and `READ_CONTROL` stay readable so that denying a leaf does not
            // make its parent undirectoriable — hiding a file's existence is not what a
            // deny-read is for, and trying costs traversal everywhere above it.
            AceKind::DenyRead => FILE_READ_DATA | FILE_READ_EA,

            // Everything that changes bytes, metadata or the security descriptor.
            // `WRITE_DAC` and `WRITE_OWNER` are the load-bearing pair: without them the
            // confined command can take ownership and drop the very ACE denying it.
            AceKind::DenyWrite => {
                FILE_WRITE_DATA
                    | FILE_APPEND_DATA
                    | FILE_WRITE_EA
                    | FILE_WRITE_ATTRIBUTES
                    | FILE_DELETE_CHILD
                    | DELETE
                    | WRITE_DAC
                    | WRITE_OWNER
            }

            // Read *and* execute. Denying only `FILE_EXECUTE` is not enough on its own and
            // denying only reads is an inference: loading an image needs both, but which
            // one the access check refuses first is not something to leave to the loader.
            // Both are named, so the answer does not depend on that.
            AceKind::DenyExecute => FILE_READ_DATA | FILE_READ_EA | FILE_EXECUTE,

            // A usable working directory — see the module docs for why this is wider than a
            // bare `FILE_GENERIC_WRITE`, and why it still excludes `WRITE_DAC`/`WRITE_OWNER`.
            AceKind::AllowWrite => {
                FILE_READ_DATA
                    | FILE_WRITE_DATA
                    | FILE_APPEND_DATA
                    | FILE_READ_EA
                    | FILE_WRITE_EA
                    | FILE_EXECUTE
                    | FILE_DELETE_CHILD
                    | FILE_READ_ATTRIBUTES
                    | FILE_WRITE_ATTRIBUTES
                    | DELETE
                    | READ_CONTROL
                    | SYNCHRONIZE
            }
        }
    }

    /// The `SANDBOX_WIN_DENIED` operation word for a denial under this rule.
    pub const fn denied_operation(self) -> &'static str {
        match self {
            AceKind::DenyRead | AceKind::DenyExecute => crate::core::markers::op::READ,
            AceKind::DenyWrite | AceKind::AllowWrite => crate::core::markers::op::WRITE,
        }
    }
}

/// Which argument put an ACE in the plan.
///
/// Separate from [`AceKind`] because a degraded mask and a plain `--deny-read`
/// produce the same ACE but not the same stderr: one has to tell the caller that
/// content substitution did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AceOrigin {
    DenyRead,
    DenyWrite,
    AllowWrite,
    /// A `--mask-file` that could not be a mask.
    MaskDegraded,
}

/// One ACE to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAce {
    pub path: PathBuf,
    pub kind: AceKind,
    pub origin: AceOrigin,
}

impl PlannedAce {
    /// The inheritance flags for this ACE against a target of the given kind.
    ///
    /// A directory rule has to reach the files inside it, which is what the two
    /// inherit flags do. A file has nothing below it, and setting container
    /// inheritance on one is how you get an ACE Windows reports back differently
    /// than you wrote it.
    pub const fn ace_flags(&self, is_directory: bool) -> u32 {
        if is_directory {
            inheritance::CONTAINER_INHERIT_ACE | inheritance::OBJECT_INHERIT_ACE
        } else {
            0
        }
    }
}

/// The ACEs for one `exec`, plus what had to be degraded to get there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcePlan {
    /// In DACL order: every deny, then every grant.
    pub aces: Vec<PlannedAce>,
    /// `--mask-file` real paths that became deny-read. One
    /// `SANDBOX_WIN_DENIED mask-degraded <path>` line each.
    pub degraded_masks: Vec<PathBuf>,
}

/// Turn a parsed request into the DACL changes it asks for.
///
/// Deduplicates on (path, kind): the same rule twice is one ACE, and writing it
/// twice would leave a second entry behind at revoke time.
pub fn plan_aces(request: &ExecRequest) -> AcePlan {
    let mut plan = AcePlan::default();

    // Denies first, and within them read before write — only the relative order of
    // deny-vs-grant is enforced by Windows, but a stable total order makes the
    // ledger and the tests reproducible.
    for path in &request.deny_read {
        push(&mut plan.aces, path, AceKind::DenyRead, AceOrigin::DenyRead);
    }
    for mask in &request.masks {
        // Without a filesystem filter driver the same path cannot answer two processes
        // differently, so the mask becomes a refusal. Recorded even when the path is
        // already denied, because the caller asked for substitution and has to hear that
        // it did not happen.
        push(
            &mut plan.aces,
            &mask.real,
            AceKind::DenyRead,
            AceOrigin::MaskDegraded,
        );
        if !plan.degraded_masks.contains(&mask.real) {
            plan.degraded_masks.push(mask.real.clone());
        }
    }
    for path in &request.deny_write {
        push(
            &mut plan.aces,
            path,
            AceKind::DenyWrite,
            AceOrigin::DenyWrite,
        );
    }
    for path in &request.allow_write {
        push(
            &mut plan.aces,
            path,
            AceKind::AllowWrite,
            AceOrigin::AllowWrite,
        );
    }

    plan
}

/// Add an ACE unless (path, kind) is already planned.
///
/// The duplicate matters at revoke time, not at write time: two ledger rows for
/// one ACE means the second revoke finds nothing and reports a mismatch.
fn push(aces: &mut Vec<PlannedAce>, path: &Path, kind: AceKind, origin: AceOrigin) {
    if aces.iter().any(|ace| ace.path == path && ace.kind == kind) {
        return;
    }
    aces.push(PlannedAce {
        path: path.to_path_buf(),
        kind,
        origin,
    });
}

/// Whether every deny precedes every grant.
///
/// The sys layer calls this immediately before it hands a DACL to
/// `SetNamedSecurityInfoW`. It is cheap, and the failure it catches is invisible
/// from the outside: a wrongly ordered DACL applies cleanly, reads back cleanly,
/// and enforces nothing.
pub fn is_correctly_ordered(aces: &[PlannedAce]) -> bool {
    let mut seen_grant = false;
    for ace in aces {
        if ace.kind.is_deny() {
            if seen_grant {
                return false;
            }
        } else {
            seen_grant = true;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::argv::MaskRule;

    fn request() -> ExecRequest {
        ExecRequest {
            command: vec!["cmd.exe".into()],
            ..Default::default()
        }
    }

    fn paths(plan: &AcePlan, kind: AceKind) -> Vec<String> {
        plan.aces
            .iter()
            .filter(|ace| ace.kind == kind)
            .map(|ace| ace.path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn deny_execute_is_never_planned_from_arguments() {
        // It is an `install`-time ACE on the helper's own binary and nothing else. If a
        // future flag started emitting it, the ledger would carry an entry `reap` revokes
        // on a dead session — and the binary would become runnable again halfway through
        // a command.
        let request = ExecRequest {
            deny_read: vec![PathBuf::from("a")],
            deny_write: vec![PathBuf::from("b")],
            allow_write: vec![PathBuf::from("c")],
            masks: vec![MaskRule {
                real: PathBuf::from("d"),
                fake: PathBuf::from("e"),
            }],
            ..ExecRequest::default()
        };
        for ace in plan_aces(&request).aces {
            assert_ne!(ace.kind, AceKind::DenyExecute, "{ace:?}");
        }
    }

    #[test]
    fn deny_execute_names_both_reading_and_executing() {
        // Loading an image needs both. Denying one and inferring the other makes the
        // outcome depend on which right the access check happens to test first.
        let mask = AceKind::DenyExecute.access_mask();
        assert_ne!(mask & rights::FILE_EXECUTE, 0, "execute not denied");
        assert_ne!(mask & rights::FILE_READ_DATA, 0, "read not denied");
    }

    #[test]
    fn deny_execute_denies_nothing_it_does_not_need_to() {
        // It goes on a file the *user* owns and keeps using. A stray write or delete bit
        // here would be this install quietly restricting a binary beyond what it said it
        // was doing.
        let mask = AceKind::DenyExecute.access_mask();
        for (name, right) in [
            ("FILE_WRITE_DATA", rights::FILE_WRITE_DATA),
            ("DELETE", rights::DELETE),
            ("WRITE_DAC", rights::WRITE_DAC),
            ("WRITE_OWNER", rights::WRITE_OWNER),
        ] {
            assert_eq!(mask & right, 0, "{name} should not be in a deny-execute");
        }
    }

    #[test]
    fn deny_execute_sorts_before_allows() {
        // Windows matches top-down and stops. A deny that landed after an allow would
        // never be reached.
        assert!(AceKind::DenyExecute.is_deny());
    }

    #[test]
    fn every_deny_precedes_every_grant() {
        let mut request = request();
        request.allow_write = vec![PathBuf::from(r"C:\work")];
        request.deny_write = vec![PathBuf::from(r"C:\work\vendor")];
        request.deny_read = vec![PathBuf::from(r"C:\Users\u\.ssh")];

        let plan = plan_aces(&request);

        assert!(is_correctly_ordered(&plan.aces));
        let first_grant = plan
            .aces
            .iter()
            .position(|ace| !ace.kind.is_deny())
            .unwrap();
        assert!(plan.aces[..first_grant]
            .iter()
            .all(|ace| ace.kind.is_deny()));
    }

    #[test]
    fn the_order_check_catches_a_grant_ahead_of_a_deny() {
        let wrong = vec![
            PlannedAce {
                path: PathBuf::from(r"C:\work"),
                kind: AceKind::AllowWrite,
                origin: AceOrigin::AllowWrite,
            },
            PlannedAce {
                path: PathBuf::from(r"C:\work\vendor"),
                kind: AceKind::DenyWrite,
                origin: AceOrigin::DenyWrite,
            },
        ];
        assert!(!is_correctly_ordered(&wrong));
    }

    #[test]
    fn an_empty_or_single_sided_plan_is_ordered() {
        assert!(is_correctly_ordered(&[]));
        assert!(is_correctly_ordered(&plan_aces(&request()).aces));
    }

    #[test]
    fn a_mask_becomes_a_deny_read_and_says_so() {
        let mut request = request();
        request.masks = vec![MaskRule {
            real: PathBuf::from(r"C:\Users\u\.npmrc"),
            fake: PathBuf::from(r"C:\tmp\fake"),
        }];

        let plan = plan_aces(&request);

        assert_eq!(paths(&plan, AceKind::DenyRead), vec![r"C:\Users\u\.npmrc"]);
        assert_eq!(
            plan.degraded_masks,
            vec![PathBuf::from(r"C:\Users\u\.npmrc")]
        );
        assert_eq!(plan.aces[0].origin, AceOrigin::MaskDegraded);
    }

    #[test]
    fn a_mask_over_an_already_denied_path_still_reports_the_degradation() {
        // Otherwise the caller is told the credential was substituted when what actually
        // happened is a permission error, and those get debugged in two entirely
        // different places.
        let mut request = request();
        request.deny_read = vec![PathBuf::from(r"C:\Users\u\.npmrc")];
        request.masks = vec![MaskRule {
            real: PathBuf::from(r"C:\Users\u\.npmrc"),
            fake: PathBuf::from(r"C:\tmp\fake"),
        }];

        let plan = plan_aces(&request);

        assert_eq!(plan.aces.len(), 1, "the ACE is not written twice");
        assert_eq!(
            plan.degraded_masks,
            vec![PathBuf::from(r"C:\Users\u\.npmrc")]
        );
    }

    #[test]
    fn a_repeated_rule_produces_one_ace() {
        let mut request = request();
        request.deny_write = vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\a")];
        assert_eq!(plan_aces(&request).aces.len(), 1);
    }

    #[test]
    fn the_same_path_denied_for_read_and_write_gets_both_aces() {
        let mut request = request();
        request.deny_read = vec![PathBuf::from(r"C:\a")];
        request.deny_write = vec![PathBuf::from(r"C:\a")];
        assert_eq!(plan_aces(&request).aces.len(), 2);
    }

    #[test]
    fn a_grant_never_carries_write_dac_or_write_owner() {
        // The invariant the whole non-elevated design rests on: inside its own write
        // root the sandbox may write files, and may not rewrite the DACL that confines
        // it.
        let mask = AceKind::AllowWrite.access_mask();
        assert_eq!(mask & rights::WRITE_DAC, 0);
        assert_eq!(mask & rights::WRITE_OWNER, 0);
    }

    #[test]
    fn a_write_denial_covers_taking_ownership() {
        let mask = AceKind::DenyWrite.access_mask();
        assert_ne!(mask & rights::WRITE_DAC, 0);
        assert_ne!(mask & rights::WRITE_OWNER, 0);
        assert_ne!(mask & rights::DELETE, 0);
        assert_ne!(mask & rights::FILE_DELETE_CHILD, 0);
    }

    #[test]
    fn a_read_denial_covers_contents_and_directory_listing() {
        let mask = AceKind::DenyRead.access_mask();
        // FILE_READ_DATA is FILE_LIST_DIRECTORY on a container.
        assert_ne!(mask & rights::FILE_READ_DATA, 0);
        assert_ne!(mask & rights::FILE_READ_EA, 0);
    }

    #[test]
    fn a_read_denial_leaves_traversal_and_attributes_alone() {
        // Denying these would break `dir` on every ancestor, which is a much larger
        // blast radius than the rule asked for.
        let mask = AceKind::DenyRead.access_mask();
        assert_eq!(mask & rights::FILE_READ_ATTRIBUTES, 0);
        assert_eq!(mask & rights::READ_CONTROL, 0);
        assert_eq!(mask & rights::SYNCHRONIZE, 0);
    }

    #[test]
    fn a_write_root_can_be_read_back() {
        let mask = AceKind::AllowWrite.access_mask();
        assert_ne!(mask & rights::FILE_READ_DATA, 0);
        assert_ne!(mask & rights::FILE_WRITE_DATA, 0);
        assert_ne!(mask & rights::DELETE, 0);
    }

    #[test]
    fn directories_inherit_and_files_do_not() {
        let ace = PlannedAce {
            path: PathBuf::from(r"C:\work"),
            kind: AceKind::AllowWrite,
            origin: AceOrigin::AllowWrite,
        };
        assert_eq!(
            ace.ace_flags(true),
            inheritance::CONTAINER_INHERIT_ACE | inheritance::OBJECT_INHERIT_ACE
        );
        assert_eq!(ace.ace_flags(false), 0);
    }

    #[test]
    fn deny_and_grant_classification_matches_the_kinds() {
        assert!(AceKind::DenyRead.is_deny());
        assert!(AceKind::DenyWrite.is_deny());
        assert!(!AceKind::AllowWrite.is_deny());
    }

    #[test]
    fn the_denied_operation_word_matches_the_rule() {
        assert_eq!(AceKind::DenyRead.denied_operation(), "read");
        assert_eq!(AceKind::DenyWrite.denied_operation(), "write");
    }

    #[test]
    fn the_full_caller_shape_plans_in_the_documented_order() {
        let mut request = request();
        request.deny_read = vec![PathBuf::from(r"C:\Users\u\.ssh")];
        request.masks = vec![MaskRule {
            real: PathBuf::from(r"C:\Users\u\.npmrc"),
            fake: PathBuf::from(r"C:\tmp\fake"),
        }];
        request.deny_write = vec![PathBuf::from(r"C:\work\vendor")];
        request.allow_write = vec![PathBuf::from(r"C:\work")];

        let kinds: Vec<AceKind> = plan_aces(&request).aces.iter().map(|a| a.kind).collect();

        assert_eq!(
            kinds,
            vec![
                AceKind::DenyRead,
                AceKind::DenyRead,
                AceKind::DenyWrite,
                AceKind::AllowWrite,
            ]
        );
    }
}
