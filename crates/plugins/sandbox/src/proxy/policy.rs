//! Which hostnames a confined command may reach — sandbox RFC §3.1, §7.
//!
//! This is the whole of the domain decision, and it is a pure function so the
//! matrix can be asserted without a socket. Everything else in this crate is
//! plumbing that asks it a question and acts on the answer.
//!
//! ## The rules
//!
//! * **A deny always wins.** Listing a domain in both is a contradiction, and
//!   resolving it towards "allow" would mean a `deniedDomains` entry could be
//!   cancelled by a broader `allowedDomains` one.
//! * **A non-empty allow list is an allowlist.** Anything not on it is
//!   refused. That is what makes `allowedDomains` mean what it says.
//! * **An empty allow list allows everything the deny list has not refused.**
//!   Someone who only wants to block one host should not have to enumerate
//!   the internet.
//!
//! ## What a rule matches
//!
//! `example.com` matches `example.com` and any subdomain of it, on a label
//! boundary — so `api.example.com` matches and `notexample.com` does not.
//! `*.example.com` matches subdomains only, for the case where the apex
//! itself should not be included.
//!
//! Matching is case-insensitive and ignores one trailing dot, because
//! `Example.COM.` is the same name and a client is entitled to send it.
//!
//! ## IP literals are not hostnames
//!
//! `CONNECT 93.184.216.34:443` carries no name to match, so under an
//! allowlist it is refused. That is deliberate rather than incidental: an
//! allowlist that let raw addresses through would be bypassed by resolving
//! the name first, which any client can do.

/// Why a connection was refused, in words the model reads back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny { reason: String },
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Verdict::Allow)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Verdict::Allow => None,
            Verdict::Deny { reason } => Some(reason),
        }
    }
}

/// The session's domain rules, compiled once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainPolicy {
    allowed: Vec<Rule>,
    denied: Vec<Rule>,
}

impl DomainPolicy {
    pub fn new<A, D>(allowed: A, denied: D) -> Self
    where
        A: IntoIterator,
        A::Item: AsRef<str>,
        D: IntoIterator,
        D::Item: AsRef<str>,
    {
        Self {
            allowed: allowed.into_iter().filter_map(Rule::parse).collect(),
            denied: denied.into_iter().filter_map(Rule::parse).collect(),
        }
    }

    /// Whether any rule was configured. An unconfigured policy allows
    /// everything, and the sandbox does not start a proxy for it at all.
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.denied.is_empty()
    }

    /// Whether the allow list is closed, i.e. anything unlisted is refused.
    pub fn is_allowlist(&self) -> bool {
        !self.allowed.is_empty()
    }

    pub fn decide(&self, host: &str) -> Verdict {
        let name = normalise(host);
        if name.is_empty() {
            return Verdict::Deny {
                reason: "the request carried no host to check".into(),
            };
        }

        if let Some(rule) = self.denied.iter().find(|rule| rule.matches(&name)) {
            return Verdict::Deny {
                reason: format!("{host} is blocked by the sandbox rule `{}`", rule.source),
            };
        }

        if self.allowed.is_empty() {
            return Verdict::Allow;
        }
        if self.allowed.iter().any(|rule| rule.matches(&name)) {
            return Verdict::Allow;
        }

        Verdict::Deny {
            reason: format!(
                "{host} is not in the sandbox's allowed domains ({})",
                self.allowed
                    .iter()
                    .map(|rule| rule.source.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// As written, for the message. A refusal that quotes the rule the user
    /// typed is actionable; one that quotes a normalised form is a puzzle.
    source: String,
    /// Lower-cased, trailing dot removed, `*.` stripped.
    name: String,
    /// `*.example.com` — subdomains but not the apex.
    subdomains_only: bool,
}

impl Rule {
    fn parse(raw: impl AsRef<str>) -> Option<Self> {
        let source = raw.as_ref().trim().to_string();
        if source.is_empty() {
            return None;
        }
        let (body, subdomains_only) = match source.strip_prefix("*.") {
            Some(rest) => (rest, true),
            None => (source.as_str(), false),
        };
        let name = normalise(body);
        if name.is_empty() {
            return None;
        }
        Some(Self {
            source,
            name,
            subdomains_only,
        })
    }

    fn matches(&self, host: &str) -> bool {
        if !self.subdomains_only && host == self.name {
            return true;
        }
        // The label boundary is the whole point: without the leading dot,
        // `example.com` would match `notexample.com`.
        host.len() > self.name.len()
            && host.ends_with(&self.name)
            && host.as_bytes()[host.len() - self.name.len() - 1] == b'.'
    }
}

/// Lower-case, strip one trailing dot.
fn normalise(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &[&str], denied: &[&str]) -> DomainPolicy {
        DomainPolicy::new(allowed.iter().copied(), denied.iter().copied())
    }

    #[test]
    fn an_empty_policy_allows_everything() {
        let policy = policy(&[], &[]);
        assert!(policy.is_empty());
        assert!(policy.decide("example.com").is_allowed());
    }

    #[test]
    fn a_non_empty_allow_list_refuses_everything_else() {
        let policy = policy(&["api.example.com"], &[]);
        assert!(policy.is_allowlist());
        assert!(policy.decide("api.example.com").is_allowed());
        assert!(!policy.decide("evil.test").is_allowed());
    }

    #[test]
    fn a_deny_list_alone_allows_everything_it_does_not_name() {
        let policy = policy(&[], &["evil.test"]);
        assert!(policy.decide("example.com").is_allowed());
        assert!(!policy.decide("evil.test").is_allowed());
    }

    #[test]
    fn a_deny_beats_an_allow() {
        // Otherwise a broad allow entry silently cancels a specific deny,
        // and the more specific rule is the one the user meant.
        let policy = policy(&["example.com"], &["secrets.example.com"]);
        assert!(policy.decide("example.com").is_allowed());
        assert!(policy.decide("api.example.com").is_allowed());
        assert!(!policy.decide("secrets.example.com").is_allowed());
    }

    #[test]
    fn a_bare_rule_covers_the_apex_and_its_subdomains() {
        let policy = policy(&["example.com"], &[]);
        assert!(policy.decide("example.com").is_allowed());
        assert!(policy.decide("api.example.com").is_allowed());
        assert!(policy.decide("a.b.example.com").is_allowed());
    }

    #[test]
    fn a_rule_matches_only_on_a_label_boundary() {
        // The classic suffix bug: `notexample.com` ends with `example.com`
        // and is an entirely different domain that anybody can register.
        let policy = policy(&["example.com"], &[]);
        assert!(!policy.decide("notexample.com").is_allowed());
        assert!(!policy.decide("example.com.evil.test").is_allowed());
    }

    #[test]
    fn a_star_rule_covers_subdomains_but_not_the_apex() {
        let policy = policy(&["*.example.com"], &[]);
        assert!(policy.decide("api.example.com").is_allowed());
        assert!(!policy.decide("example.com").is_allowed());
    }

    #[test]
    fn matching_ignores_case_and_a_trailing_dot() {
        let policy = policy(&["Example.COM"], &[]);
        assert!(policy.decide("api.example.com").is_allowed());
        assert!(policy.decide("EXAMPLE.com.").is_allowed());
    }

    #[test]
    fn an_ip_literal_is_refused_under_an_allowlist() {
        // An allowlist that let raw addresses through would be bypassed by
        // resolving the name first, which every client can do.
        let policy = policy(&["example.com"], &[]);
        assert!(!policy.decide("93.184.216.34").is_allowed());
        assert!(!policy.decide("::1").is_allowed());
    }

    #[test]
    fn an_ip_literal_is_allowed_when_there_is_no_allowlist() {
        let policy = policy(&[], &["evil.test"]);
        assert!(policy.decide("93.184.216.34").is_allowed());
    }

    #[test]
    fn an_empty_host_is_refused_in_every_configuration() {
        for policy in [policy(&[], &[]), policy(&["example.com"], &[])] {
            assert!(!policy.decide("").is_allowed());
            assert!(!policy.decide("   ").is_allowed());
        }
    }

    #[test]
    fn a_refusal_names_the_rule_the_user_wrote() {
        // As typed, not normalised: a message quoting `*.EVIL.test` is
        // something the user can find in their settings file.
        let policy = policy(&[], &["*.EVIL.test"]);
        let verdict = policy.decide("api.evil.test");
        assert_eq!(
            verdict.reason(),
            Some("api.evil.test is blocked by the sandbox rule `*.EVIL.test`")
        );
    }

    #[test]
    fn a_refusal_under_an_allowlist_lists_what_is_allowed() {
        let policy = policy(&["api.example.com", "cdn.example.com"], &[]);
        let reason = policy.decide("evil.test").reason().unwrap().to_string();
        assert!(reason.contains("api.example.com"), "{reason}");
        assert!(reason.contains("cdn.example.com"), "{reason}");
    }

    #[test]
    fn blank_and_whitespace_rules_are_dropped_rather_than_matching_everything() {
        // A rule that normalised to the empty string would match every host
        // by suffix, turning a typo into an open allowlist.
        let policy = policy(&["", "   ", "*.", "."], &["example.com"]);
        assert!(!policy.is_allowlist(), "blank rules must not form a list");
        assert!(!policy.decide("example.com").is_allowed());
        assert!(policy.decide("anything.test").is_allowed());
    }

    #[test]
    fn rules_are_trimmed_before_use() {
        let policy = policy(&["  example.com  "], &[]);
        assert!(policy.decide("example.com").is_allowed());
    }
}
