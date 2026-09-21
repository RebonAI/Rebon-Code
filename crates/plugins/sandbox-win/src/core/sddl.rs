//! Building SDDL strings.
//!
//! Two objects in the helper are created with a security descriptor rather than
//! having one applied afterwards: the credentials file, whose DACL must deny
//! the sandbox group, and the isolated desktop. SDDL is the only reasonable way
//! to express those inline.
//!
//! ## There is no escaping, so there is validation
//!
//! SDDL has no quoting or escape mechanism: `(`, `;` and `)` are structure
//! wherever they appear. A trustee or rights string carrying one of them does
//! not produce a mangled ACE, it produces a *different, valid* ACE. So this
//! module refuses anything that is not already a well-formed SID, alias or
//! rights token — [`SddlError`] rather than a best-effort escape. Every trustee
//! the helper uses is a SID it just looked up, so a refusal here means a bug
//! rather than a user typo.
//!
//! The deny-before-grant rule from [`crate::core::acl`] applies to a DACL
//! written as SDDL exactly as it does to one written through `SetEntriesInAclW`,
//! and [`dacl`] enforces it by construction rather than trusting the caller.

use std::fmt::Write as _;

/// Whether an ACE carries a grant or a deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SddlAceType {
    Allow,
    Deny,
}

impl SddlAceType {
    const fn token(self) -> &'static str {
        match self {
            SddlAceType::Allow => "A",
            SddlAceType::Deny => "D",
        }
    }
}

/// One ACE in SDDL form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SddlAce {
    pub kind: SddlAceType,
    /// ACE flags, e.g. `OICI`. Empty for a non-inheriting ACE.
    pub flags: String,
    /// Rights, e.g. `FA`, `GA`, or `0x1200a9`.
    pub rights: String,
    /// A SID string or a two-letter alias.
    pub trustee: String,
}

impl SddlAce {
    pub fn allow(rights: &str, trustee: &str) -> Self {
        Self {
            kind: SddlAceType::Allow,
            flags: String::new(),
            rights: rights.to_string(),
            trustee: trustee.to_string(),
        }
    }

    pub fn deny(rights: &str, trustee: &str) -> Self {
        Self {
            kind: SddlAceType::Deny,
            flags: String::new(),
            rights: rights.to_string(),
            trustee: trustee.to_string(),
        }
    }
}

/// Why a descriptor could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SddlError {
    #[error("`{0}` is not a SID or a known SDDL alias")]
    BadTrustee(String),
    #[error("`{0}` is not a valid SDDL rights string")]
    BadRights(String),
    #[error("`{0}` is not a valid SDDL ACE flag string")]
    BadFlags(String),
}

/// A DACL, with every deny placed ahead of every grant.
///
/// `protected` emits `P`, which stops inherited ACEs from being merged in. The
/// credentials file wants that: an inherited grant from a parent directory
/// would defeat the deny this DACL exists to carry.
pub fn dacl(protected: bool, aces: &[SddlAce]) -> Result<String, SddlError> {
    let mut rendered = String::from("D:");
    if protected {
        rendered.push('P');
    }
    // Sorted rather than validated-in-order: a caller that lists them the wrong
    // way round gets a working descriptor instead of a silent hole.
    for wanted in [SddlAceType::Deny, SddlAceType::Allow] {
        for ace in aces.iter().filter(|ace| ace.kind == wanted) {
            validate(ace)?;
            let _ = write!(
                rendered,
                "({};{};{};;;{})",
                ace.kind.token(),
                ace.flags,
                ace.rights,
                ace.trustee
            );
        }
    }
    Ok(rendered)
}

/// A full security descriptor.
pub fn descriptor(
    owner: Option<&str>,
    group: Option<&str>,
    dacl: Option<&str>,
) -> Result<String, SddlError> {
    let mut rendered = String::new();
    if let Some(owner) = owner {
        check_trustee(owner)?;
        let _ = write!(rendered, "O:{owner}");
    }
    if let Some(group) = group {
        check_trustee(group)?;
        let _ = write!(rendered, "G:{group}");
    }
    if let Some(dacl) = dacl {
        rendered.push_str(dacl);
    }
    Ok(rendered)
}

/// The DACL for `credentials.bin`.
///
/// The deny is not decoration. The file holds the passwords for both sandbox
/// accounts; a confined command that reads it can log on as the account with no
/// network filters, and `--block-network` stops meaning anything.
pub fn credentials_file_dacl(
    owner_sid: &str,
    sandbox_group_sid: &str,
) -> Result<String, SddlError> {
    dacl(
        true,
        &[
            SddlAce::deny("FA", sandbox_group_sid),
            SddlAce::allow("FA", owner_sid),
            SddlAce::allow("FA", "SY"),
        ],
    )
}

/// The DACL for the per-exec desktop.
///
/// Only the caller and the sandbox accounts may reach it. The point is to cut
/// the confined process off from the user's real desktop, where window messages
/// cross security boundaries.
pub fn sandbox_desktop_dacl(owner_sid: &str, sandbox_group_sid: &str) -> Result<String, SddlError> {
    dacl(
        true,
        &[
            SddlAce::allow("GA", owner_sid),
            SddlAce::allow("GA", sandbox_group_sid),
        ],
    )
}

fn validate(ace: &SddlAce) -> Result<(), SddlError> {
    check_trustee(&ace.trustee)?;
    if !ace.rights.is_empty() && !is_rights_token(&ace.rights) {
        return Err(SddlError::BadRights(ace.rights.clone()));
    }
    if !ace.flags.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(SddlError::BadFlags(ace.flags.clone()));
    }
    Ok(())
}

fn check_trustee(trustee: &str) -> Result<(), SddlError> {
    if is_trustee(trustee) {
        Ok(())
    } else {
        Err(SddlError::BadTrustee(trustee.to_string()))
    }
}

/// A SID string (`S-1-5-21-…`) or a two-letter SDDL alias (`BA`, `SY`, `WD`).
fn is_trustee(trustee: &str) -> bool {
    if trustee.len() == 2 && trustee.chars().all(|c| c.is_ascii_uppercase()) {
        return true;
    }
    is_sid(trustee)
}

/// `S-1-<authority>[-<sub authority>]…`, digits only.
pub fn is_sid(value: &str) -> bool {
    let mut parts = value.split('-');
    if parts.next() != Some("S") {
        return false;
    }
    if parts.next() != Some("1") {
        return false;
    }
    let mut count = 0usize;
    for part in parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        count += 1;
    }
    count >= 1
}

fn is_rights_token(value: &str) -> bool {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    value.chars().all(|c| c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUP: &str = "S-1-5-21-1111111111-2222222222-3333333333-1004";
    const OWNER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    #[test]
    fn a_dacl_puts_denies_first_even_when_listed_last() {
        let rendered = dacl(
            true,
            &[SddlAce::allow("FA", OWNER), SddlAce::deny("FA", GROUP)],
        )
        .unwrap();

        let deny = rendered.find("(D;").unwrap();
        let allow = rendered.find("(A;").unwrap();
        assert!(deny < allow, "{rendered}");
    }

    #[test]
    fn the_protected_flag_is_emitted_only_when_asked() {
        assert!(dacl(true, &[]).unwrap().starts_with("D:P"));
        assert_eq!(dacl(false, &[]).unwrap(), "D:");
    }

    #[test]
    fn the_credentials_dacl_denies_the_sandbox_group_first() {
        let rendered = credentials_file_dacl(OWNER, GROUP).unwrap();

        assert!(rendered.starts_with("D:P(D;;FA;;;"), "{rendered}");
        assert!(rendered.contains(GROUP));
        assert!(rendered.contains(OWNER));
        assert!(rendered.contains(";;;SY)"));
    }

    #[test]
    fn the_credentials_dacl_is_protected_against_inherited_grants() {
        // %LOCALAPPDATA% grants the user full control by inheritance; without `P` that
        // grant would flow in beside the deny.
        assert!(credentials_file_dacl(OWNER, GROUP)
            .unwrap()
            .starts_with("D:P"));
    }

    #[test]
    fn the_desktop_dacl_admits_only_the_caller_and_the_sandbox() {
        let rendered = sandbox_desktop_dacl(OWNER, GROUP).unwrap();
        assert_eq!(rendered.matches("(A;").count(), 2);
        assert_eq!(rendered.matches("(D;").count(), 0);
        assert!(rendered.contains(OWNER) && rendered.contains(GROUP));
    }

    #[test]
    fn a_trustee_that_could_change_the_ace_is_refused_not_escaped() {
        // SDDL has no escape for `)`, so this string would otherwise close the ACE
        // early and open a second one under our own punctuation.
        let injected = ")(A;;FA;;;WD";
        assert_eq!(
            dacl(false, &[SddlAce::deny("FA", injected)]).unwrap_err(),
            SddlError::BadTrustee(injected.to_string())
        );
    }

    #[test]
    fn a_rights_string_with_structure_in_it_is_refused() {
        assert_eq!(
            dacl(false, &[SddlAce::deny("FA;;;WD)(A;;FA;;;WD", GROUP)]).unwrap_err(),
            SddlError::BadRights("FA;;;WD)(A;;FA;;;WD".to_string())
        );
    }

    #[test]
    fn ace_flags_are_validated_too() {
        let ace = SddlAce {
            kind: SddlAceType::Allow,
            flags: "OI;CI".into(),
            rights: "FA".into(),
            trustee: GROUP.into(),
        };
        assert!(matches!(
            dacl(false, &[ace]).unwrap_err(),
            SddlError::BadFlags(_)
        ));
    }

    #[test]
    fn inheritance_flags_render_in_the_flags_slot() {
        let ace = SddlAce {
            kind: SddlAceType::Allow,
            flags: "OICI".into(),
            rights: "FA".into(),
            trustee: GROUP.into(),
        };
        assert!(dacl(false, &[ace]).unwrap().contains("(A;OICI;FA;;;"));
    }

    #[test]
    fn a_hex_rights_mask_is_accepted() {
        let ace = SddlAce::allow("0x1200a9", GROUP);
        assert!(dacl(false, &[ace]).unwrap().contains("0x1200a9"));
    }

    #[test]
    fn sid_recognition_accepts_real_sids_and_rejects_the_rest() {
        assert!(is_sid("S-1-5-18"));
        assert!(is_sid(GROUP));
        assert!(!is_sid("S-1"));
        assert!(!is_sid("S-2-5-18"));
        assert!(!is_sid("S-1-5-abc"));
        assert!(!is_sid("BA"));
        assert!(!is_sid(""));
        assert!(!is_sid("S-1-5-18-"));
    }

    #[test]
    fn two_letter_aliases_are_accepted_as_trustees() {
        assert!(dacl(false, &[SddlAce::allow("FA", "BA")]).is_ok());
        assert!(dacl(false, &[SddlAce::allow("FA", "Ba")]).is_err());
        assert!(dacl(false, &[SddlAce::allow("FA", "BAD")]).is_err());
    }

    #[test]
    fn a_descriptor_composes_owner_group_and_dacl_in_order() {
        let rendered = descriptor(
            Some("BA"),
            Some("SY"),
            Some(&dacl(true, &[SddlAce::allow("GA", "BA")]).unwrap()),
        )
        .unwrap();
        assert_eq!(rendered, "O:BAG:SYD:P(A;;GA;;;BA)");
    }

    #[test]
    fn a_descriptor_refuses_a_bad_owner() {
        assert!(descriptor(Some("not a sid"), None, None).is_err());
    }

    #[test]
    fn an_empty_descriptor_is_empty_rather_than_malformed() {
        assert_eq!(descriptor(None, None, None).unwrap(), "");
    }
}
