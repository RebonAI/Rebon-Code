//! The one fact this crate exists to keep honest: which Node builds may host
//! the plugin plane.
//!
//! Only this module is compiled. The same range is also written down by hand in
//! the plugin host's package metadata, in its lockfile, in the release
//! workflow's Node gate and in the READMEs, so moving the range here means
//! moving it there too.

use std::fmt;

use thiserror::Error;

/// A Node runtime version. Node ships plain `major.minor.patch` triples for
/// every published build, so a suffix means the string did not come from a
/// runtime we recognise and is rejected rather than guessed at.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum VersionError {
    #[error("`{0}` is not a Node version (expected major.minor.patch)")]
    Malformed(String),
}

impl NodeVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `24.19.0` or `v24.19.0`.
    ///
    /// Leading zeros are rejected so that parsing and [`fmt::Display`] are exact
    /// inverses: a version that round-trips is the same string the runtime
    /// reported, which is what makes it safe to put in a receipt or a URL.
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        let malformed = || VersionError::Malformed(text.to_string());
        let body = text.strip_prefix('v').unwrap_or(text);
        let mut parts = body.split('.');
        let mut next = || -> Result<u32, VersionError> {
            let part = parts.next().ok_or_else(malformed)?;
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(malformed());
            }
            if part.len() > 1 && part.starts_with('0') {
                return Err(malformed());
            }
            part.parse().map_err(|_| malformed())
        };
        let major = next()?;
        let minor = next()?;
        let patch = next()?;
        if parts.next().is_some() {
            return Err(malformed());
        }
        Ok(Self::new(major, minor, patch))
    }

    /// The `vX.Y.Z` spelling nodejs.org uses for release directories.
    pub fn tag(&self) -> String {
        format!("v{self}")
    }
}

impl fmt::Display for NodeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A half-open version window: `>= min_inclusive` and `< max_exclusive`.
///
/// Half-open rather than "major N" because the lower bound carries a patch
/// level. The host relies on behaviour that landed inside a major, so
/// "Node 24" would admit builds that cannot run it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeVersionRange {
    pub min_inclusive: NodeVersion,
    pub max_exclusive: NodeVersion,
}

impl NodeVersionRange {
    pub const fn new(min_inclusive: NodeVersion, max_exclusive: NodeVersion) -> Self {
        Self {
            min_inclusive,
            max_exclusive,
        }
    }

    pub fn contains(&self, version: &NodeVersion) -> bool {
        *version >= self.min_inclusive && *version < self.max_exclusive
    }
}

impl fmt::Display for NodeVersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ">={} <{}", self.min_inclusive, self.max_exclusive)
    }
}

/// Node builds allowed to host the plugin plane.
pub const SUPPORTED_NODE_VERSIONS: NodeVersionRange =
    NodeVersionRange::new(NodeVersion::new(24, 19, 0), NodeVersion::new(25, 0, 0));

/// The exact build [`crate::ManagedRuntimeStore`] installs.
///
/// One version, not "the newest in range": the digests in
/// [`crate::pinned_archives`] are compiled into this binary, so the bytes a
/// managed install accepts are fixed at build time. Widening this to a live
/// lookup would mean trusting whatever a registry serves today.
pub const PINNED_NODE_VERSION: NodeVersion = NodeVersion::new(24, 19, 0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_spellings() {
        assert_eq!(
            NodeVersion::parse("24.19.0").unwrap(),
            NodeVersion::new(24, 19, 0)
        );
        assert_eq!(
            NodeVersion::parse("v24.19.0").unwrap(),
            NodeVersion::new(24, 19, 0)
        );
    }

    #[test]
    fn display_round_trips_every_accepted_string() {
        for text in ["0.0.0", "24.19.0", "25.0.0", "4294967295.1.2"] {
            let parsed = NodeVersion::parse(text).unwrap();
            assert_eq!(parsed.to_string(), text);
            assert_eq!(parsed.tag(), format!("v{text}"));
        }
    }

    #[test]
    fn rejects_everything_that_is_not_a_plain_triple() {
        for text in [
            "",
            "v",
            "24",
            "24.19",
            "24.19.0.1",
            "24.19.x",
            "24.19.0-nightly",
            "24.19.0+build",
            " 24.19.0",
            "24.19.0 ",
            "v24.19.0v",
            "-1.0.0",
            "4294967296.0.0",
        ] {
            assert!(
                NodeVersion::parse(text).is_err(),
                "expected `{text}` to be rejected"
            );
        }
    }

    /// A leading zero would parse to the same number but print differently, so
    /// a receipt written from it would not name the directory it describes.
    #[test]
    fn rejects_leading_zeros_but_keeps_plain_zero() {
        assert!(NodeVersion::parse("024.19.0").is_err());
        assert!(NodeVersion::parse("24.019.0").is_err());
        assert!(NodeVersion::parse("24.19.00").is_err());
        assert_eq!(
            NodeVersion::parse("24.19.0").unwrap(),
            NodeVersion::new(24, 19, 0)
        );
    }

    #[test]
    fn ordering_is_major_then_minor_then_patch() {
        let mut versions = [
            NodeVersion::new(24, 19, 1),
            NodeVersion::new(25, 0, 0),
            NodeVersion::new(24, 20, 0),
            NodeVersion::new(24, 19, 0),
        ];
        versions.sort();
        assert_eq!(
            versions,
            [
                NodeVersion::new(24, 19, 0),
                NodeVersion::new(24, 19, 1),
                NodeVersion::new(24, 20, 0),
                NodeVersion::new(25, 0, 0),
            ]
        );
    }

    #[test]
    fn range_is_half_open_at_both_ends() {
        let range = SUPPORTED_NODE_VERSIONS;
        assert!(!range.contains(&NodeVersion::new(24, 18, 9)));
        assert!(range.contains(&NodeVersion::new(24, 19, 0)));
        assert!(range.contains(&NodeVersion::new(24, 99, 99)));
        assert!(!range.contains(&NodeVersion::new(25, 0, 0)));
        assert!(!range.contains(&NodeVersion::new(22, 21, 1)));
    }

    #[test]
    fn pinned_version_is_installable_under_the_supported_range() {
        assert!(SUPPORTED_NODE_VERSIONS.contains(&PINNED_NODE_VERSION));
    }

    #[test]
    fn range_renders_the_way_the_error_messages_quote_it() {
        assert_eq!(SUPPORTED_NODE_VERSIONS.to_string(), ">=24.19.0 <25.0.0");
    }
}
