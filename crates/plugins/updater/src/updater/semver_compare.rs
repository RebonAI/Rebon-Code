//! Loose semver comparison.
//!
//! ## Behavior reference
//!
//! The public surface is four predicates plus the shared comparator.
//! [`gt`] and [`lt`] test [`loose_order`] against [`Ordering::Greater`]
//! and [`Ordering::Less`]; [`gte`] accepts `Greater | Equal` and
//! [`lte`] accepts `Less | Equal`. [`loose_order`] parses both operands
//! with `parse_loose` and then applies the major/minor/patch and
//! pre-release rules described below. There is no runtime backend
//! selection and no dependency: the whole comparison is local integer
//! and string work.
//!
//! The comparison is: compare major, minor, patch as integers; then
//! handle pre-release tags (`1.2.3-beta.1`); then **ignore build
//! metadata** (`1.2.3+sha`). Build metadata is how a continuous-deploy
//! version carries its commit, and per the SemVer spec it never takes
//! part in ordering.
//!
//! ## What loose mode means
//!
//! Loose mode accepts a few non-strict forms:
//!
//! * Optional leading `v` (`v1.2.3` == `1.2.3`).
//! * Missing minor or patch components default to 0
//!   (`1.2` == `1.2.0`, `1` == `1.0.0`). This is technically
//!   coercion, not loose mode, but the comparator accepts it the
//!   same way.
//! * Pre-release tags compare lexicographically (numeric segments
//!   numerically, alpha segments lexicographically; numeric < alpha
//!   per the semver spec).
//! * Build metadata is **ignored** in comparisons but preserved in
//!   strings for display.
//!
//! ## Comparison algorithm (per the semver 2.0.0 spec)
//!
//! 1. Compare `major`, then `minor`, then `patch` as integers.
//! 2. If all three are equal:
//!    * A version WITHOUT a pre-release tag has **higher** precedence
//!      than one WITH a pre-release tag (`1.0.0` > `1.0.0-rc1`).
//!    * If both have pre-release tags, compare each dot-separated
//!      identifier:
//!        - Numeric identifiers compare numerically.
//!        - Alphanumeric identifiers compare lexicographically.
//!        - Numeric identifiers always have **lower** precedence
//!          than alphanumeric.
//!        - A larger set of pre-release fields has higher precedence
//!          than a smaller set, if all preceding identifiers are
//!          equal.
//! 3. Build metadata (`+...`) is ignored.
//!
//! The Rust implementation implements steps 1-3 in [`loose_order`]. The
//! comparators [`gt`], [`gte`], [`lt`], [`lte`] are thin wrappers.
//!
//! ## Why a hand-rolled comparator (no `semver` crate)
//!
//! `semver` is a 30-line dependency on its own, plus transitive
//! `nom`/`pest` ones depending on the version. Pulling that in for
//! this lightweight pure-logic crate would raise the dependency floor.
//! The algorithm is
//! ~120 lines without dependencies and pinned by a comprehensive
//! test table.

use std::cmp::Ordering as StdOrdering;

/// Comparison result, with discriminants `-1 | 0 | 1`. Convertible to
/// `std::cmp::Ordering` if a
/// caller wants to feed it into `Vec::sort_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    /// `a < b`
    Less = -1,
    /// `a == b`
    Equal = 0,
    /// `a > b`
    Greater = 1,
}

impl From<StdOrdering> for Ordering {
    fn from(o: StdOrdering) -> Self {
        match o {
            StdOrdering::Less => Self::Less,
            StdOrdering::Equal => Self::Equal,
            StdOrdering::Greater => Self::Greater,
        }
    }
}

impl From<Ordering> for StdOrdering {
    fn from(o: Ordering) -> Self {
        match o {
            Ordering::Less => StdOrdering::Less,
            Ordering::Equal => StdOrdering::Equal,
            Ordering::Greater => StdOrdering::Greater,
        }
    }
}

/// `gt(a, b)` — `a > b`.
pub fn gt(a: &str, b: &str) -> bool {
    loose_order(a, b) == Ordering::Greater
}

/// `gte(a, b)` — `a >= b`.
pub fn gte(a: &str, b: &str) -> bool {
    matches!(loose_order(a, b), Ordering::Greater | Ordering::Equal)
}

/// `lt(a, b)` — `a < b`.
pub fn lt(a: &str, b: &str) -> bool {
    loose_order(a, b) == Ordering::Less
}

/// `lte(a, b)` — `a <= b`.
pub fn lte(a: &str, b: &str) -> bool {
    matches!(loose_order(a, b), Ordering::Less | Ordering::Equal)
}

/// Total order on two semver-loose strings. The core comparator
/// every other function in this module delegates to.
pub fn loose_order(a: &str, b: &str) -> Ordering {
    let pa = parse_loose(a);
    let pb = parse_loose(b);

    // Compare major, minor, patch as integers.
    match pa.major.cmp(&pb.major) {
        StdOrdering::Equal => {}
        other => return other.into(),
    }
    match pa.minor.cmp(&pb.minor) {
        StdOrdering::Equal => {}
        other => return other.into(),
    }
    match pa.patch.cmp(&pb.patch) {
        StdOrdering::Equal => {}
        other => return other.into(),
    }

    // Pre-release precedence per semver 2.0.0 §11:
    //   - Version with no pre-release > version with pre-release.
    //   - If both have pre-release, compare identifier-by-identifier.
    match (pa.pre.is_empty(), pb.pre.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater, // a has no pre-release, a > b
        (false, true) => Ordering::Less,    // b has no pre-release, a < b
        (false, false) => compare_pre_release(&pa.pre, &pb.pre),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LooseSemver {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<PreReleaseId>,
    // build metadata intentionally ignored — see module docs.
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreReleaseId {
    Numeric(u64),
    Alphanumeric(String),
}

/// Parse a semver-loose string into a `LooseSemver`. Tolerant of:
///
/// * Leading `v` / `V` prefix (stripped).
/// * Missing minor / patch (defaulted to 0).
/// * Pre-release segment (`-tag.1`), parsed into the `pre` vec.
/// * Build metadata (`+sha`), ignored.
/// * Garbage that won't parse (numeric components default to 0).
///
/// Garbage handling is pragmatic rather than exact: in practice the
/// updater only feeds registry versions into the comparator, and
/// those are always strict semver. The garbage path is exercised by
/// tests to ensure no panics.
fn parse_loose(input: &str) -> LooseSemver {
    let mut s = input.trim();
    if let Some(rest) = s.strip_prefix('v').or_else(|| s.strip_prefix('V')) {
        s = rest;
    }

    // Split off build metadata first — it never participates in
    // comparison and the spec says it's syntactically anywhere
    // after the patch (`1.2.3+sha-rc1` is `+sha-rc1`, not
    // `1.2.3` then a pre-release `sha-rc1`). The split is on the
    // first `+`.
    let (without_build, _build) = match s.find('+') {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };

    // Then split off pre-release on the first `-` AFTER the patch.
    // The patch component itself can't contain a `-`, so the first
    // `-` in `without_build` is the pre-release separator.
    let (version_part, pre_part) = match without_build.find('-') {
        Some(idx) => (&without_build[..idx], Some(&without_build[idx + 1..])),
        None => (without_build, None),
    };

    // Split version_part on `.` into up to 3 numeric components.
    let mut nums = version_part.split('.');
    let major = nums.next().and_then(parse_loose_u64).unwrap_or(0);
    let minor = nums.next().and_then(parse_loose_u64).unwrap_or(0);
    let patch = nums.next().and_then(parse_loose_u64).unwrap_or(0);

    let pre = match pre_part {
        Some(p) if !p.is_empty() => p.split('.').map(parse_pre_id).collect(),
        _ => Vec::new(),
    };

    LooseSemver {
        major,
        minor,
        patch,
        pre,
    }
}

/// Parse a single component as a u64. Strips trailing non-digits
/// (the `loose` mode silently allows trailing junk on the patch
/// segment, e.g. `1.2.3rc` → patch = 3 in some implementations).
/// We don't go that far — just parse the leading digits and bail
/// if there are none.
fn parse_loose_u64(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn parse_pre_id(id: &str) -> PreReleaseId {
    // A pre-release identifier is purely-numeric iff every char is
    // an ASCII digit AND it doesn't have a leading zero (the spec
    // says numeric identifiers MUST NOT have leading zeros, but
    // `loose` mode tolerates them — we still treat them as numeric).
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(n) = id.parse::<u64>() {
            return PreReleaseId::Numeric(n);
        }
    }
    PreReleaseId::Alphanumeric(id.to_string())
}

fn compare_pre_release(a: &[PreReleaseId], b: &[PreReleaseId]) -> Ordering {
    let max_len = a.len().max(b.len());
    for i in 0..max_len {
        match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) => match compare_pre_id(x, y) {
                Ordering::Equal => continue,
                other => return other,
            },
            // "A larger set of pre-release fields has higher
            // precedence than a smaller set, if all preceding
            // identifiers are equal."
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
    Ordering::Equal
}

fn compare_pre_id(a: &PreReleaseId, b: &PreReleaseId) -> Ordering {
    match (a, b) {
        (PreReleaseId::Numeric(x), PreReleaseId::Numeric(y)) => x.cmp(y).into(),
        (PreReleaseId::Alphanumeric(x), PreReleaseId::Alphanumeric(y)) => x.cmp(y).into(),
        // Numeric identifiers always have lower precedence than
        // alphanumeric per semver 2.0.0 §11.
        (PreReleaseId::Numeric(_), PreReleaseId::Alphanumeric(_)) => Ordering::Less,
        (PreReleaseId::Alphanumeric(_), PreReleaseId::Numeric(_)) => Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Numeric major.minor.patch comparison
    // -----------------------------------------------------------------

    #[test]
    fn equal_versions() {
        assert_eq!(loose_order("1.2.3", "1.2.3"), Ordering::Equal);
        assert!(gte("1.2.3", "1.2.3"));
        assert!(lte("1.2.3", "1.2.3"));
        assert!(!gt("1.2.3", "1.2.3"));
        assert!(!lt("1.2.3", "1.2.3"));
    }

    #[test]
    fn major_difference() {
        assert_eq!(loose_order("2.0.0", "1.99.99"), Ordering::Greater);
        assert!(gt("2.0.0", "1.99.99"));
        assert!(lt("1.99.99", "2.0.0"));
    }

    #[test]
    fn minor_difference() {
        assert_eq!(loose_order("1.3.0", "1.2.99"), Ordering::Greater);
        assert!(gt("1.3.0", "1.2.99"));
    }

    #[test]
    fn patch_difference() {
        assert_eq!(loose_order("1.2.4", "1.2.3"), Ordering::Greater);
        assert!(gt("1.2.4", "1.2.3"));
    }

    // -----------------------------------------------------------------
    // Build metadata is ignored — the load-bearing rule
    // -----------------------------------------------------------------

    #[test]
    fn build_metadata_ignored() {
        // The whole point of the build metadata format is that
        // SHA-only changes still produce equal-version comparisons.
        // The CD pipeline relies on this.
        assert_eq!(loose_order("1.2.3+abc123", "1.2.3+def456"), Ordering::Equal);
        assert_eq!(loose_order("1.2.3+abc", "1.2.3"), Ordering::Equal);
        assert!(gte("1.2.3+abc", "1.2.3"));
        assert!(lte("1.2.3+abc", "1.2.3"));
    }

    #[test]
    fn build_metadata_with_different_versions() {
        // Different versions still compare normally even when build
        // metadata is present.
        assert!(gt("1.2.4+abc", "1.2.3+abc"));
        assert!(gt("2.0.0+sha", "1.99.99+sha"));
    }

    // -----------------------------------------------------------------
    // Pre-release tag comparison — the trickiest part
    // -----------------------------------------------------------------

    #[test]
    fn pre_release_loses_to_release() {
        // semver 2.0.0 §11: a version with no pre-release tag has
        // higher precedence than one with a pre-release tag, when
        // major.minor.patch are equal.
        assert!(gt("1.0.0", "1.0.0-rc1"));
        assert!(lt("1.0.0-rc1", "1.0.0"));
        assert!(gt("1.0.0", "1.0.0-alpha"));
        assert!(gt("1.0.0", "1.0.0-beta.5"));
    }

    #[test]
    fn pre_release_numeric_compare() {
        // Numeric identifiers compare numerically.
        assert!(gt("1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(gt("1.0.0-alpha.10", "1.0.0-alpha.2"));
    }

    #[test]
    fn pre_release_alpha_compare() {
        // Alphanumeric identifiers compare lexicographically.
        assert!(gt("1.0.0-beta", "1.0.0-alpha"));
        assert!(gt("1.0.0-rc1", "1.0.0-beta"));
    }

    #[test]
    fn pre_release_numeric_lower_than_alpha() {
        // Per semver 2.0.0 §11: numeric identifiers always have
        // lower precedence than alphanumeric.
        assert!(lt("1.0.0-1", "1.0.0-alpha"));
        assert!(gt("1.0.0-alpha", "1.0.0-1"));
    }

    #[test]
    fn pre_release_more_fields_higher_precedence() {
        // "A larger set of pre-release fields has higher precedence
        // than a smaller set, if all preceding identifiers are
        // equal."
        assert!(gt("1.0.0-alpha.1", "1.0.0-alpha"));
        assert!(gt("1.0.0-alpha.beta.1", "1.0.0-alpha.beta"));
    }

    #[test]
    fn semver_spec_pre_release_chain() {
        // The example chain from the spec:
        //   1.0.0-alpha < 1.0.0-alpha.1 < 1.0.0-alpha.beta
        // < 1.0.0-beta < 1.0.0-beta.2 < 1.0.0-beta.11
        // < 1.0.0-rc.1 < 1.0.0
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for window in chain.windows(2) {
            let (a, b) = (window[0], window[1]);
            assert!(
                lt(a, b),
                "expected {a} < {b} per semver 2.0.0 §11 spec example",
            );
            assert!(gt(b, a), "expected {b} > {a}",);
        }
    }

    // -----------------------------------------------------------------
    // Loose mode coercions
    // -----------------------------------------------------------------

    #[test]
    fn leading_v_prefix_stripped() {
        assert_eq!(loose_order("v1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(loose_order("V1.2.3", "1.2.3"), Ordering::Equal);
        assert!(gt("v1.2.4", "1.2.3"));
        assert!(gt("1.2.4", "v1.2.3"));
    }

    #[test]
    fn missing_components_default_to_zero() {
        assert_eq!(loose_order("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(loose_order("1", "1.0.0"), Ordering::Equal);
        assert!(gt("1.2.1", "1.2"));
        assert!(gt("1.3", "1.2.99"));
    }

    #[test]
    fn whitespace_padding_tolerated() {
        // The comparators here are called from a few different code
        // paths and the inputs are sometimes trimmed strings from
        // npm output. We tolerate leading/trailing whitespace.
        assert_eq!(loose_order(" 1.2.3 ", "1.2.3"), Ordering::Equal);
        assert!(gt("  2.0.0", "1.0.0  "));
    }

    // -----------------------------------------------------------------
    // Combined: build metadata + pre-release
    // -----------------------------------------------------------------

    #[test]
    fn pre_release_and_build_metadata() {
        // Pre-release tags compare; build metadata is ignored.
        assert_eq!(
            loose_order("1.0.0-rc1+abc", "1.0.0-rc1+def"),
            Ordering::Equal
        );
        assert!(gt("1.0.0-rc2+abc", "1.0.0-rc1+def"));
        assert!(gt("1.0.0+abc", "1.0.0-rc1+xyz"));
    }

    // -----------------------------------------------------------------
    // Garbage and edge cases — must not panic
    // -----------------------------------------------------------------

    #[test]
    fn empty_string_does_not_panic() {
        // Empty string parses as 0.0.0 and compares accordingly.
        // no caller feeds the comparator an empty string in
        // production (the update-needed gate returns early without
        // both a current and a latest version), but the comparator
        // should never panic.
        let _ = loose_order("", "1.0.0");
        let _ = loose_order("1.0.0", "");
        let _ = loose_order("", "");
    }

    #[test]
    fn garbage_input_does_not_panic() {
        let _ = loose_order("not.a.version", "also.garbage");
        let _ = loose_order("1.2.3", "lol");
        let _ = loose_order("a.b.c", "1.2.3");
    }

    #[test]
    fn equal_with_extra_zeros() {
        // 1.2 == 1.2.0 already tested. Also: 1.0 == 1.0.0 == 1.
        assert_eq!(loose_order("1", "1.0"), Ordering::Equal);
        assert_eq!(loose_order("1", "1.0.0"), Ordering::Equal);
        assert_eq!(loose_order("1.0", "1.0.0"), Ordering::Equal);
    }

    // -----------------------------------------------------------------
    // gt/gte/lt/lte table — every cell of the truth table
    // -----------------------------------------------------------------

    /// Exhaustive comparator table. Each row is (a, b, expected_order);
    /// the comparators are derived from it.
    #[test]
    fn loose_order_comparator_table() {
        let cases: &[(&str, &str, Ordering)] = &[
            ("1.0.0", "1.0.0", Ordering::Equal),
            ("1.0.0", "2.0.0", Ordering::Less),
            ("2.0.0", "1.0.0", Ordering::Greater),
            ("1.2.3", "1.2.4", Ordering::Less),
            ("1.2.3", "1.3.0", Ordering::Less),
            ("1.0.0+abc", "1.0.0", Ordering::Equal),
            ("1.0.0+abc", "1.0.0+def", Ordering::Equal),
            ("1.0.0", "1.0.0-rc1", Ordering::Greater),
            ("1.0.0-rc1", "1.0.0", Ordering::Less),
            ("1.0.0-alpha", "1.0.0-beta", Ordering::Less),
            ("1.0.0-rc.2", "1.0.0-rc.1", Ordering::Greater),
            ("1.0.0-alpha", "1.0.0-alpha.1", Ordering::Less),
            ("1.0.0-1", "1.0.0-alpha", Ordering::Less), // numeric < alpha
            ("v1.0.0", "1.0.0", Ordering::Equal),
            ("1.0", "1.0.0", Ordering::Equal),
            ("1", "1.0.0", Ordering::Equal),
        ];
        for (a, b, expected) in cases {
            let actual = loose_order(a, b);
            assert_eq!(
                actual, *expected,
                "comparator table failed for ({a:?}, {b:?})",
            );
            // Derive comparators and cross-check.
            assert_eq!(gt(a, b), actual == Ordering::Greater);
            assert_eq!(lt(a, b), actual == Ordering::Less);
            assert_eq!(
                gte(a, b),
                matches!(actual, Ordering::Greater | Ordering::Equal)
            );
            assert_eq!(
                lte(a, b),
                matches!(actual, Ordering::Less | Ordering::Equal)
            );
        }
    }
}
