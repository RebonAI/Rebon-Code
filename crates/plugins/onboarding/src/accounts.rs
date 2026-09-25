//! Signing out and reporting status, the same way on every surface.
//!
//! `rebon logout`, the terminal's `/logout` and `rebon login --status` answer
//! the same questions — which login does this argument mean, which logins are
//! signed in, what does signing out leave behind — so the answers are
//! written once, here, and each surface only prints them.

use std::path::Path;

use rebon_config::account_login::{
    account_login_statuses_in, sign_out_account, AccountLoginSpec, AccountLoginStatus,
};

/// What a sign-out request names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoutTarget {
    /// Exactly one login.
    One(&'static AccountLoginSpec),
    /// No login named and more than one signed in: the user has to pick.
    Ambiguous(Vec<&'static AccountLoginSpec>),
    /// No login named and none signed in.
    NoneSignedIn,
}

/// The comma-separated ids a "which login?" message lists.
pub fn known_login_ids() -> String {
    rebon_config::account_logins()
        .iter()
        .map(|spec| spec.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Look a login up by name, or say which names exist.
pub fn find_login(name: &str) -> Result<&'static AccountLoginSpec, String> {
    rebon_config::account_login(name).ok_or_else(|| {
        format!(
            "no account login is called `{}` — known: {}",
            name.trim(),
            known_login_ids()
        )
    })
}

/// Which login a sign-out means: the one named, or, with no name, the only
/// one signed in.
pub fn logout_target(config_dir: &Path, name: Option<&str>) -> Result<LogoutTarget, String> {
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        return find_login(name).map(LogoutTarget::One);
    }
    let signed_in: Vec<&'static AccountLoginSpec> = account_login_statuses_in(config_dir)
        .map_err(|err| err.to_string())?
        .into_iter()
        .filter(|status| status.signed_in)
        .map(|status| status.spec)
        .collect();
    Ok(match signed_in.as_slice() {
        [] => LogoutTarget::NoneSignedIn,
        [one] => LogoutTarget::One(one),
        _ => LogoutTarget::Ambiguous(signed_in),
    })
}

/// What a sign-out did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutReport {
    /// One line for the user.
    pub message: String,
    /// The login whose tokens were removed, when any were. A surface that
    /// holds a live session uses it to say what that session still holds.
    pub signed_out: Option<&'static AccountLoginSpec>,
}

impl LogoutReport {
    fn nothing(message: String) -> Self {
        Self {
            message,
            signed_out: None,
        }
    }
}

/// Sign out of `spec` and say what happened, in one line.
pub fn sign_out(
    config_dir: &Path,
    spec: &'static AccountLoginSpec,
) -> Result<LogoutReport, String> {
    match sign_out_account(config_dir, spec) {
        Ok(true) => Ok(LogoutReport {
            message: format!(
                "Signed out of {}. The `{}` provider stays configured; sign in again with \
                 `rebon login {}` or /login.",
                spec.display_name, spec.provider.name, spec.id
            ),
            signed_out: Some(spec),
        }),
        Ok(false) => Ok(LogoutReport::nothing(format!(
            "Not signed in to {}.",
            spec.display_name
        ))),
        Err(err) => Err(format!(
            "could not sign out of {}: {err:#}",
            spec.display_name
        )),
    }
}

/// Resolve and run a sign-out: the whole of `/logout [account]`.
pub fn logout(config_dir: &Path, name: Option<&str>) -> Result<LogoutReport, String> {
    match logout_target(config_dir, name)? {
        LogoutTarget::One(spec) => sign_out(config_dir, spec),
        LogoutTarget::NoneSignedIn => Ok(LogoutReport::nothing(
            "No account is signed in.".to_string(),
        )),
        LogoutTarget::Ambiguous(specs) => Err(format!(
            "More than one account is signed in; name one: {}",
            specs
                .iter()
                .map(|spec| spec.id)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// One line per login, for `rebon login --status`.
pub fn status_lines(config_dir: &Path, now_ms: u64) -> Result<Vec<String>, String> {
    Ok(account_login_statuses_in(config_dir)
        .map_err(|err| err.to_string())?
        .iter()
        .map(|status| status_line(status, now_ms))
        .collect())
}

fn status_line(status: &AccountLoginStatus, now_ms: u64) -> String {
    let spec = status.spec;
    let state = if !status.signed_in {
        "not signed in".to_string()
    } else {
        match status.expires_at_ms {
            Some(expires_at) if expires_at <= now_ms => {
                "signed in (token expired; refreshes on next use)".to_string()
            }
            Some(expires_at) => format!(
                "signed in (token valid for {})",
                coarse_duration(expires_at - now_ms)
            ),
            None => "signed in".to_string(),
        }
    };
    let provider = if status.active {
        format!(" · provider `{}` (active)", spec.provider.name)
    } else if status.provider_configured {
        format!(" · provider `{}`", spec.provider.name)
    } else {
        String::new()
    };
    format!("{:<8} {} — {state}{provider}", spec.id, spec.display_name)
}

/// Hours or days, never minutes: the token refreshes by itself, so a
/// countdown would suggest a deadline the user does not have.
fn coarse_duration(ms: u64) -> String {
    let hours = ms / 3_600_000;
    if hours >= 48 {
        format!("{} days", hours / 24)
    } else if hours >= 1 {
        format!("{hours} h")
    } else {
        "under an hour".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_config::account_login::{write_account_tokens, AccountTokens};

    fn codex() -> &'static AccountLoginSpec {
        rebon_config::account_login::codex_login()
    }

    fn copilot() -> &'static AccountLoginSpec {
        rebon_config::account_login(rebon_config::COPILOT_LOGIN_ID).unwrap()
    }

    fn sign_in(dir: &Path, spec: &AccountLoginSpec, expires_at: Option<u64>) {
        write_account_tokens(dir, spec, &AccountTokens::new("t".into(), None, expires_at)).unwrap();
    }

    #[test]
    fn a_named_login_is_found_by_id_or_alias_and_an_unknown_one_lists_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            logout_target(dir.path(), Some("GitHub")).unwrap(),
            LogoutTarget::One(copilot())
        );
        assert_eq!(
            logout_target(dir.path(), Some("codex")).unwrap(),
            LogoutTarget::One(codex())
        );
        let err = logout_target(dir.path(), Some("gemini")).unwrap_err();
        assert!(err.contains("openai") && err.contains("copilot"), "{err}");
    }

    #[test]
    fn with_no_name_the_only_signed_in_login_is_the_target() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            logout_target(dir.path(), None).unwrap(),
            LogoutTarget::NoneSignedIn
        );
        sign_in(dir.path(), copilot(), None);
        assert_eq!(
            logout_target(dir.path(), Some("  ")).unwrap(),
            LogoutTarget::One(copilot())
        );
        sign_in(dir.path(), codex(), None);
        assert_eq!(
            logout_target(dir.path(), None).unwrap(),
            LogoutTarget::Ambiguous(vec![codex(), copilot()])
        );
    }

    #[test]
    fn logout_signs_out_one_login_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        sign_in(dir.path(), codex(), None);
        sign_in(dir.path(), copilot(), None);

        let err = logout(dir.path(), None).unwrap_err();
        assert!(err.contains("name one"), "{err}");

        let report = logout(dir.path(), Some("copilot")).unwrap();
        assert!(
            report.message.starts_with("Signed out of GitHub Copilot"),
            "{}",
            report.message
        );
        assert_eq!(report.signed_out, Some(copilot()));
        let statuses = account_login_statuses_in(dir.path()).unwrap();
        assert!(
            statuses
                .iter()
                .find(|s| s.spec.is_codex())
                .unwrap()
                .signed_in
        );
        assert!(
            !statuses
                .iter()
                .find(|s| !s.spec.is_codex())
                .unwrap()
                .signed_in
        );

        assert_eq!(
            logout(dir.path(), Some("copilot")).unwrap(),
            LogoutReport {
                message: "Not signed in to GitHub Copilot.".to_string(),
                signed_out: None,
            }
        );
        let report = logout(dir.path(), None).unwrap();
        assert!(
            report.message.starts_with("Signed out of ChatGPT"),
            "{}",
            report.message
        );
        assert_eq!(report.signed_out, Some(codex()));
        assert_eq!(
            logout(dir.path(), None).unwrap().message,
            "No account is signed in."
        );
    }

    #[test]
    fn status_lines_cover_every_login_and_never_count_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000_000_000;
        sign_in(dir.path(), codex(), Some(now + 5 * 3_600_000 + 60_000));
        let lines = status_lines(dir.path(), now).unwrap();
        assert_eq!(lines.len(), rebon_config::account_logins().len());
        assert!(
            lines[0].contains("signed in (token valid for 5 h)"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("not signed in"), "{}", lines[1]);

        sign_in(dir.path(), codex(), Some(now - 1));
        assert!(status_lines(dir.path(), now).unwrap()[0].contains("token expired"));
        sign_in(dir.path(), codex(), Some(now + 72 * 3_600_000));
        assert!(status_lines(dir.path(), now).unwrap()[0].contains("3 days"));
        sign_in(dir.path(), codex(), Some(now + 60_000));
        assert!(status_lines(dir.path(), now).unwrap()[0].contains("under an hour"));
    }

    #[test]
    fn a_status_line_names_the_provider_and_whether_it_is_active() {
        let dir = tempfile::tempdir().unwrap();
        sign_in(dir.path(), copilot(), None);
        rebon_config::account_login::upsert_account_provider_in(dir.path(), copilot(), None)
            .unwrap();
        let line = status_lines(dir.path(), 0).unwrap().remove(1);
        assert!(
            line.contains("signed in · provider `copilot` (active)"),
            "{line}"
        );
    }
}
