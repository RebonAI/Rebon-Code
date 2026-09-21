//! The downgraded accounts and the group every ACL is written against.
//!
//! ## Why there are two accounts and one group
//!
//! The network filters are keyed on the sandbox user's SID, they are installed
//! once under administrator, and `exec` does not elevate — so `exec` has no way
//! to turn a SID-keyed filter on or off. Making the filter unconditional
//! instead would cut the network for a command that only ever asked for a file
//! rule.
//!
//! The resolution moves the choice from the filter to the account:
//! [`SANDBOX_USER_NO_NETWORK`] has block filters, [`SANDBOX_USER`] does not, and
//! `--block-network` selects between the two. Nothing at `exec` time then needs
//! a privilege it does not have.
//!
//! The ACL side does **not** double, because every grant and deny is written
//! against [`SANDBOX_GROUP`], which both accounts belong to. The filesystem
//! layer therefore sees one trustee while the network layer sees two accounts,
//! and neither has to know about the other's split.
//!
//! Two alternatives were rejected, and are recorded here because they will be
//! proposed again: a restricted token (`CreateRestrictedToken` with a
//! restricting SID) avoids the second account, but its double access check
//! fails ordinary programs in ways that are painful to diagnose, and the
//! evaluation of WFP's `ALE_USER_ID` against a restricted token is subtle
//! enough to produce a filter that looks installed and matches nothing; and a
//! single always-offline account, which is simplest but binds the file sandbox
//! to the network sandbox and needs a resident proxy.

/// The default sandbox account. No WFP filter is keyed to it.
pub const SANDBOX_USER: &str = "rebon-sbx";

/// The sandbox account used when `--block-network` is passed. The WFP block
/// and permit-loopback filters are keyed to this account's SID.
pub const SANDBOX_USER_NO_NETWORK: &str = "rebon-sbx-n";

/// The local group both accounts join. Every ACE the helper writes names this
/// group and never an account, which keeps the ACL layer to a single trustee.
pub const SANDBOX_GROUP: &str = "rebon-sbx-grp";

/// Every account and group name this crate creates.
pub const ALL_PRINCIPALS: &[&str] = &[SANDBOX_USER, SANDBOX_USER_NO_NETWORK, SANDBOX_GROUP];

/// The SAM account name limit. Windows refuses a longer name at creation, so a
/// name that outgrows this turns `install` into a failure that surfaces only on
/// a user's machine.
pub const MAX_SAM_NAME: usize = 20;

/// Which account runs this command.
///
/// This is all `--block-network` does at `exec` time: one bit selects a name.
/// Everything else was settled at `install`.
pub const fn account_for(block_network: bool) -> &'static str {
    if block_network {
        SANDBOX_USER_NO_NETWORK
    } else {
        SANDBOX_USER
    }
}

/// The characters Windows refuses in a SAM account name.
const FORBIDDEN_IN_SAM_NAME: &[char] = &[
    '"', '/', '\\', '[', ']', ':', ';', '|', '=', ',', '+', '*', '?', '<', '>', '@',
];

/// Whether `name` can be used as a local account or group name.
pub fn is_valid_sam_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SAM_NAME
        && !name.contains(FORBIDDEN_IN_SAM_NAME)
        && !name.ends_with('.')
        && !name.chars().all(|c| c == '.' || c == ' ')
        && name.chars().all(|c| !c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_bit_is_the_only_thing_that_picks_an_account() {
        assert_eq!(account_for(false), SANDBOX_USER);
        assert_eq!(account_for(true), SANDBOX_USER_NO_NETWORK);
    }

    #[test]
    fn the_two_accounts_are_distinct() {
        // If these ever collapse into one name, `--block-network` silently becomes
        // "always" or "never", depending on which filters exist.
        assert_ne!(SANDBOX_USER, SANDBOX_USER_NO_NETWORK);
    }

    #[test]
    fn the_group_is_neither_account() {
        assert_ne!(SANDBOX_GROUP, SANDBOX_USER);
        assert_ne!(SANDBOX_GROUP, SANDBOX_USER_NO_NETWORK);
    }

    #[test]
    fn every_principal_fits_the_sam_name_rules() {
        for name in ALL_PRINCIPALS {
            assert!(
                is_valid_sam_name(name),
                "{name} is not a usable local account name"
            );
        }
    }

    #[test]
    fn sam_name_validation_catches_the_ways_a_name_goes_wrong() {
        assert!(!is_valid_sam_name(""));
        assert!(!is_valid_sam_name(&"a".repeat(MAX_SAM_NAME + 1)));
        assert!(!is_valid_sam_name("rebon\\sbx"));
        assert!(!is_valid_sam_name("rebon sbx."));
        assert!(!is_valid_sam_name("rebon@sbx"));
        assert!(!is_valid_sam_name("..."));
        assert!(is_valid_sam_name("rebon sbx"));
    }

    #[test]
    fn principals_are_listed_once_each() {
        let mut sorted = ALL_PRINCIPALS.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "uninstall would try to delete twice");
    }
}
