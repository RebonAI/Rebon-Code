//! What the user typed after `rebon remote add <name>`.
//!
//! Deliberately permissive about the *host* and strict about
//! everything else. `host` is passed to `ssh` untouched, which means a
//! bare `prod-web` keeps working as an `~/.ssh/config` alias — the
//! whole point of not reimplementing ssh's own resolution here. What
//! this module does own is splitting user / host / port / path apart
//! without guessing wrong on the two ambiguous forms: `host:2222`
//! (port) versus `host:/srv/app` (path).

use std::fmt;

/// An ssh destination, plus an optional project path the user
/// appended to it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SshTarget {
    pub user: Option<String>,
    /// Hostname, IP, or `~/.ssh/config` alias — handed to ssh as-is.
    pub host: String,
    pub port: Option<u16>,
    /// Remote project directory, when the target string carried one.
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("empty ssh target")]
    Empty,
    #[error("ssh target `{0}` has no host")]
    MissingHost(String),
    #[error("ssh target `{target}` has an unusable port `{port}`")]
    BadPort { target: String, port: String },
    #[error("ssh target `{0}` has an unterminated `[` — IPv6 literals look like `[::1]:22`")]
    UnterminatedBracket(String),
}

impl SshTarget {
    /// Parse `user@host`, `host:port`, `host:/path`, `[::1]:22`, or a
    /// full `ssh://user@host:port/path` URL.
    pub fn parse(raw: &str) -> Result<Self, TargetError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(TargetError::Empty);
        }
        if let Some(rest) = raw.strip_prefix("ssh://") {
            return Self::parse_url(raw, rest);
        }
        Self::parse_scp_like(raw)
    }

    /// `ssh://user@host:port/path` — the path is unambiguous here
    /// because the authority ends at the first `/`.
    fn parse_url(raw: &str, rest: &str) -> Result<Self, TargetError> {
        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], Some(rest[idx..].to_string())),
            None => (rest, None),
        };
        let mut target = Self::parse_authority(raw, authority)?;
        target.path = path.filter(|p| p != "/").or(target.path);
        Ok(target)
    }

    /// `user@host`, `user@host:port`, `user@host:/path`.
    fn parse_scp_like(raw: &str) -> Result<Self, TargetError> {
        Self::parse_authority(raw, raw)
    }

    fn parse_authority(raw: &str, authority: &str) -> Result<Self, TargetError> {
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((user, host)) => {
                let user = user.trim();
                ((!user.is_empty()).then(|| user.to_string()), host)
            }
            None => (None, authority),
        };

        let (host, tail) = split_host(raw, hostport)?;
        if host.is_empty() {
            return Err(TargetError::MissingHost(raw.to_string()));
        }

        let mut target = Self {
            user,
            host: host.to_string(),
            port: None,
            path: None,
        };

        if let Some(tail) = tail.filter(|tail| !tail.is_empty()) {
            // The one genuinely ambiguous spot. `:2222` is a port,
            // `:/srv/app` is a path, and `:2222/srv/app` is both. A
            // leading run of digits ending at a `/` (or at the end) is
            // the port; anything else is a path, because a non-numeric
            // port is not a thing and rejecting it outright would
            // refuse `host:~/project`.
            let (head, rest) = match tail.split_once('/') {
                Some((head, rest)) => (head, Some(format!("/{rest}"))),
                None => (tail, None),
            };
            if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) {
                target.port = Some(head.parse::<u16>().map_err(|_| TargetError::BadPort {
                    target: raw.to_string(),
                    port: head.to_string(),
                })?);
                target.path = rest;
            } else {
                target.path = Some(tail.to_string());
            }
        }

        Ok(target)
    }

    /// How this destination is spelled on an `ssh` command line.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// A stable key for per-target scratch state (the multiplexing
    /// control socket). Includes the port because the same host on two
    /// ports is two servers.
    pub fn identity(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.destination()),
            None => self.destination(),
        }
    }
}

/// Split `host[:tail]`, honouring `[…]` around IPv6 literals.
fn split_host<'a>(raw: &str, hostport: &'a str) -> Result<(&'a str, Option<&'a str>), TargetError> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err(TargetError::UnterminatedBracket(raw.to_string()));
        };
        let host = &rest[..end];
        // `None` covers `[::1]` with nothing after it, and any trailing
        // junk, which ssh is left to complain about.
        let tail = rest[end + 1..].strip_prefix(':');
        return Ok((host, tail));
    }
    // A bare IPv6 literal has more than one colon and no brackets;
    // treating the last one as a port separator would mangle it, so
    // hand the whole thing to ssh instead.
    if hostport.matches(':').count() > 1 {
        return Ok((hostport, None));
    }
    match hostport.split_once(':') {
        Some((host, tail)) => Ok((host, Some(tail))),
        None => Ok((hostport, None)),
    }
}

impl fmt::Display for SshTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.destination())?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        if let Some(path) = &self.path {
            write!(f, "{}{path}", if path.starts_with('/') { "" } else { ":" })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> SshTarget {
        SshTarget::parse(raw).expect("target should parse")
    }

    #[test]
    fn bare_host_stays_a_bare_host() {
        // An `~/.ssh/config` alias must survive untouched — resolving
        // it here would shadow the user's own ssh config.
        let target = parse("prod-web");
        assert_eq!(target.host, "prod-web");
        assert_eq!(target.user, None);
        assert_eq!(target.port, None);
        assert_eq!(target.path, None);
    }

    #[test]
    fn user_and_host_split_on_the_last_at() {
        let target = parse("deploy@build.example.com");
        assert_eq!(target.user.as_deref(), Some("deploy"));
        assert_eq!(target.host, "build.example.com");
    }

    #[test]
    fn an_at_inside_the_user_is_kept() {
        // Kerberos-style principals contain an `@`; splitting on the
        // first one would send the wrong user.
        let target = parse("me@corp.example@bastion");
        assert_eq!(target.user.as_deref(), Some("me@corp.example"));
        assert_eq!(target.host, "bastion");
    }

    #[test]
    fn numeric_tail_is_a_port() {
        let target = parse("deploy@host:2222");
        assert_eq!(target.port, Some(2222));
        assert_eq!(target.path, None);
    }

    #[test]
    fn a_port_and_a_path_can_both_follow_the_colon() {
        let target = parse("deploy@host:2222/srv/app");
        assert_eq!(target.port, Some(2222));
        assert_eq!(target.path.as_deref(), Some("/srv/app"));
    }

    #[test]
    fn non_numeric_tail_is_a_path() {
        let target = parse("deploy@host:/srv/app");
        assert_eq!(target.port, None);
        assert_eq!(target.path.as_deref(), Some("/srv/app"));

        let tilde = parse("host:~/project");
        assert_eq!(tilde.path.as_deref(), Some("~/project"));
        assert_eq!(tilde.port, None);
    }

    #[test]
    fn out_of_range_port_is_rejected_not_truncated() {
        let err = SshTarget::parse("host:99999").unwrap_err();
        assert!(matches!(err, TargetError::BadPort { .. }), "got {err:?}");
    }

    #[test]
    fn bracketed_ipv6_splits_port_off() {
        let target = parse("root@[2001:db8::1]:2222");
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, Some(2222));
        assert_eq!(target.user.as_deref(), Some("root"));
    }

    #[test]
    fn bare_ipv6_is_not_mistaken_for_host_plus_port() {
        // `::1` has two colons; treating the last as a separator would
        // ask ssh to connect to host `:` on port 1.
        let target = parse("2001:db8::1");
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, None);
    }

    #[test]
    fn unterminated_bracket_is_an_error() {
        let err = SshTarget::parse("[2001:db8::1:2222").unwrap_err();
        assert!(matches!(err, TargetError::UnterminatedBracket(_)));
    }

    #[test]
    fn ssh_url_carries_user_port_and_path() {
        let target = parse("ssh://deploy@host.example:2222/srv/app");
        assert_eq!(target.user.as_deref(), Some("deploy"));
        assert_eq!(target.host, "host.example");
        assert_eq!(target.port, Some(2222));
        assert_eq!(target.path.as_deref(), Some("/srv/app"));
    }

    #[test]
    fn ssh_url_root_path_is_not_a_project_path() {
        // `ssh://host/` means "no path given", not "/".
        let target = parse("ssh://host/");
        assert_eq!(target.path, None);
    }

    #[test]
    fn empty_and_hostless_targets_are_rejected() {
        assert_eq!(SshTarget::parse("   ").unwrap_err(), TargetError::Empty);
        assert!(matches!(
            SshTarget::parse("deploy@").unwrap_err(),
            TargetError::MissingHost(_)
        ));
    }

    #[test]
    fn destination_and_identity_separate_host_from_port() {
        // ssh wants the port via `-p`, never glued to the destination;
        // the control socket key wants them together.
        let target = parse("deploy@host:2222");
        assert_eq!(target.destination(), "deploy@host");
        assert_eq!(target.identity(), "deploy@host:2222");
    }

    #[test]
    fn display_round_trips_the_common_forms() {
        assert_eq!(parse("deploy@host:2222").to_string(), "deploy@host:2222");
        assert_eq!(parse("host:/srv/app").to_string(), "host/srv/app");
        assert_eq!(parse("prod-web").to_string(), "prod-web");
    }
}
