//! [`FsReadConfig`], [`FsWriteConfig`], and [`NetworkConfig`] — the
//! data shapes that flow through the Config tab.
//!
//! ## Behaviour notes
//!
//! The three shapes are:
//!
//! ```text
//! FsReadConfig  -> { deny_only: Vec<String>, allow_within_deny: Option<Vec<String>> }
//! FsWriteConfig -> { allow_only: Vec<String>, deny_within_allow: Vec<String> }
//! NetworkConfig -> { allowed_hosts: Option<Vec<String>>, denied_hosts: Option<Vec<String>> }
//! ```
//!
//! Each shape carries the `has_*` predicate helpers the read/write/
//! network rendering blocks consult to decide which rows to emit.
//!
//! ## Pinned rules
//!
//! 1. **`deny_only`, `allow_only`, `deny_within_allow` are ALWAYS
//! vectors.** They default to empty. The `Option` wrapper is reserved
//! for `allow_within_deny` (read) and the network host fields.
//! Pinned by [`FsReadConfig`] and [`FsWriteConfig`].
//! 2. **Network host fields are OPTIONAL.** `allowed_hosts` and
//! `denied_hosts` are `None` when the corresponding list is empty —
//! the same shape as an omitted field. The "either is non-empty"
//! predicate ([`NetworkConfig::has_any_restriction`]) is what gates
//! the network section.
//! 3. **`allow_within_deny` is OPTIONAL on the read shape.** `None`
//! means absent, which is distinct from `Some([])`. Modeled as
//! `Option<Vec<String>>`.

/// Read restrictions: the paths reads are denied under, plus any
/// paths re-allowed inside those denied regions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsReadConfig {
    /// Paths to deny reads under. Always present (default `[]`).
    pub deny_only: Vec<String>,
    /// Paths to re-allow within denied regions. `None` means the
    /// field is absent, which is distinct from `Some([])`; usually
    /// `Some([])`.
    pub allow_within_deny: Option<Vec<String>>,
}

impl FsReadConfig {
    /// True when at least one path is denied for reads.
    pub fn has_restrictions(&self) -> bool {
        !self.deny_only.is_empty()
    }

    /// True when `allow_within_deny` is present and non-empty.
    pub fn has_re_allowed_paths(&self) -> bool {
        self.allow_within_deny
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }
}

/// Write restrictions: the paths writes are allowed in, plus paths
/// denied inside those allowed regions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsWriteConfig {
    /// Allowed write paths. Always present.
    pub allow_only: Vec<String>,
    /// Denied paths within the allowed regions. Always present.
    pub deny_within_allow: Vec<String>,
}

impl FsWriteConfig {
    /// True when at least one write path is allowed.
    pub fn has_restrictions(&self) -> bool {
        !self.allow_only.is_empty()
    }

    /// True when at least one path is denied inside the allowed
    /// regions.
    pub fn has_excluded_paths(&self) -> bool {
        !self.deny_within_allow.is_empty()
    }
}

/// Network restrictions: the hosts that are explicitly allowed and
/// the hosts that are explicitly denied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkConfig {
    /// `allowed_hosts`. `None` when the list is empty — i.e. the
    /// field is absent.
    pub allowed_hosts: Option<Vec<String>>,
    /// `denied_hosts`. `None` when the list is empty — i.e. the
    /// field is absent.
    pub denied_hosts: Option<Vec<String>>,
}

impl NetworkConfig {
    /// True when `allowed_hosts` is present and non-empty.
    pub fn has_allowed(&self) -> bool {
        self.allowed_hosts
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// True when `denied_hosts` is present and non-empty.
    pub fn has_denied(&self) -> bool {
        self.denied_hosts
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// True when allowed or denied hosts are present — the gate that
    /// decides whether to render the network section at all.
    pub fn has_any_restriction(&self) -> bool {
        self.has_allowed() || self.has_denied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn read_default_has_no_restrictions() {
        let cfg = FsReadConfig::default();
        assert!(!cfg.has_restrictions());
        assert!(!cfg.has_re_allowed_paths());
    }

    #[test]
    fn read_with_deny_paths_has_restrictions() {
        let cfg = FsReadConfig {
            deny_only: s(&["/etc"]),
            allow_within_deny: None,
        };
        assert!(cfg.has_restrictions());
        assert!(!cfg.has_re_allowed_paths());
    }

    #[test]
    fn read_with_some_empty_allow_within_is_falsy() {
        let cfg = FsReadConfig {
            deny_only: s(&["/etc"]),
            allow_within_deny: Some(vec![]),
        };
        assert!(cfg.has_restrictions());
        assert!(!cfg.has_re_allowed_paths());
    }

    #[test]
    fn read_with_some_nonempty_allow_within_is_truthy() {
        let cfg = FsReadConfig {
            deny_only: s(&["/etc"]),
            allow_within_deny: Some(s(&["/etc/ssl"])),
        };
        assert!(cfg.has_re_allowed_paths());
    }

    #[test]
    fn read_with_none_allow_within_is_falsy() {
        let cfg = FsReadConfig {
            deny_only: s(&["/etc"]),
            allow_within_deny: None,
        };
        assert!(!cfg.has_re_allowed_paths());
    }

    #[test]
    fn write_default_has_no_restrictions() {
        let cfg = FsWriteConfig::default();
        assert!(!cfg.has_restrictions());
        assert!(!cfg.has_excluded_paths());
    }

    #[test]
    fn write_with_allow_paths() {
        let cfg = FsWriteConfig {
            allow_only: s(&["/tmp"]),
            deny_within_allow: vec![],
        };
        assert!(cfg.has_restrictions());
        assert!(!cfg.has_excluded_paths());
    }

    #[test]
    fn write_with_excluded_within_allow() {
        let cfg = FsWriteConfig {
            allow_only: s(&["/tmp"]),
            deny_within_allow: s(&["/tmp/secrets"]),
        };
        assert!(cfg.has_restrictions());
        assert!(cfg.has_excluded_paths());
    }

    #[test]
    fn network_default_has_no_restriction() {
        let cfg = NetworkConfig::default();
        assert!(!cfg.has_allowed());
        assert!(!cfg.has_denied());
        assert!(!cfg.has_any_restriction());
    }

    #[test]
    fn network_some_empty_allowed_is_falsy() {
        let cfg = NetworkConfig {
            allowed_hosts: Some(vec![]),
            denied_hosts: None,
        };
        assert!(!cfg.has_allowed());
        assert!(!cfg.has_any_restriction());
    }

    #[test]
    fn network_some_nonempty_allowed_is_truthy() {
        let cfg = NetworkConfig {
            allowed_hosts: Some(s(&["example.com"])),
            denied_hosts: None,
        };
        assert!(cfg.has_allowed());
        assert!(!cfg.has_denied());
        assert!(cfg.has_any_restriction());
    }

    #[test]
    fn network_some_nonempty_denied_is_truthy() {
        let cfg = NetworkConfig {
            allowed_hosts: None,
            denied_hosts: Some(s(&["evil.com"])),
        };
        assert!(!cfg.has_allowed());
        assert!(cfg.has_denied());
        assert!(cfg.has_any_restriction());
    }

    #[test]
    fn network_both_present() {
        let cfg = NetworkConfig {
            allowed_hosts: Some(s(&["api.example.com"])),
            denied_hosts: Some(s(&["evil.com"])),
        };
        assert!(cfg.has_allowed());
        assert!(cfg.has_denied());
        assert!(cfg.has_any_restriction());
    }

    #[test]
    fn network_restriction_gate_table() {
        // (allowed, denied, expected_any_restriction)
        let table: Vec<(Option<Vec<&str>>, Option<Vec<&str>>, bool)> = vec![
            (None, None, false),
            (Some(vec![]), None, false),
            (None, Some(vec![]), false),
            (Some(vec![]), Some(vec![]), false),
            (Some(vec!["a"]), None, true),
            (None, Some(vec!["a"]), true),
            (Some(vec!["a"]), Some(vec!["b"]), true),
            (Some(vec!["a"]), Some(vec![]), true),
            (Some(vec![]), Some(vec!["b"]), true),
        ];
        for (a, d, expected) in table {
            let cfg = NetworkConfig {
                allowed_hosts: a.map(|v| v.iter().map(|s| s.to_string()).collect()),
                denied_hosts: d.map(|v| v.iter().map(|s| s.to_string()).collect()),
            };
            assert_eq!(cfg.has_any_restriction(), expected, "{:?}", cfg);
        }
    }
}
