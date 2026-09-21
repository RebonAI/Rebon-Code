//! Release channel — the `"stable"` / `"latest"` wire values.
//!
//! ## Behavior reference
//!
//! A [`ReleaseChannel`] is parsed from the `updates.channel`
//! setting: an unrecognized value parses to `None` and the caller
//! falls back to [`default_channel`] (`Latest`).
//!
//! Three load-bearing facts pinned by tests below:
//!
//! 1. The enum has exactly two members: `Stable` and
//!    `Latest`. Anything else parses to `None`
//!    (and the caller falls back to `Latest`).
//! 2. The default channel is `Latest` — used unconditionally when
//!    `updates.channel` is missing/null.
//! 3. The npm-tag mapping is identity: `stable → "stable"`,
//!    `latest → "latest"`. There's no third channel that maps to
//!    something else.
//!
//! ## Why a parser at all
//!
//! The settings file is JSON; the deserializer hands the updater a
//! `String`. [`parse_channel`] returns
//! `None` for unrecognized strings, and [`default_channel`] covers the
//! "no setting present" path. Callers compose the two:
//!
//! ```rust
//! use rebon_plugin_updater::channel::{default_channel, parse_channel, ReleaseChannel};
//!
//! fn from_settings(s: Option<&str>) -> ReleaseChannel {
//!     s.and_then(parse_channel).unwrap_or(default_channel())
//! }
//!
//! assert_eq!(from_settings(None), ReleaseChannel::Latest);
//! assert_eq!(from_settings(Some("stable")), ReleaseChannel::Stable);
//! assert_eq!(from_settings(Some("garbage")), ReleaseChannel::Latest);
//! ```
//!
//! That is the full resolution rule for the setting.

/// The two release channels supported by the updater.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseChannel {
    /// `stable` — the slower-moving channel that the user opts
    /// into via `updates.channel: "stable"` in settings. Mapped to
    /// the npm dist-tag `stable`.
    Stable,
    /// `latest` — the default channel. Mapped to the npm dist-tag
    /// `latest`.
    Latest,
}

/// Parse a channel string into a [`ReleaseChannel`]. Returns `None`
/// for any string that is not a release-channel wire value.
///
/// ## Case sensitivity
///
/// The comparison is exact string equality, so
/// it is also case-sensitive. `"Stable"`, `"STABLE"`,
/// `"Latest"` all return `None`. Tests pin this — see
/// [`tests::case_sensitivity`].
pub fn parse_channel(s: &str) -> Option<ReleaseChannel> {
    match s {
        "stable" => Some(ReleaseChannel::Stable),
        "latest" => Some(ReleaseChannel::Latest),
        _ => None,
    }
}

/// The default channel when `updates.channel` is missing/null in
/// settings.
pub const fn default_channel() -> ReleaseChannel {
    ReleaseChannel::Latest
}

/// Map a [`ReleaseChannel`] to the npm dist-tag string.
pub const fn npm_tag(channel: ReleaseChannel) -> &'static str {
    match channel {
        ReleaseChannel::Stable => "stable",
        ReleaseChannel::Latest => "latest",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stable() {
        assert_eq!(parse_channel("stable"), Some(ReleaseChannel::Stable));
    }

    #[test]
    fn parses_latest() {
        assert_eq!(parse_channel("latest"), Some(ReleaseChannel::Latest));
    }

    #[test]
    fn unknown_channel_returns_none() {
        assert_eq!(parse_channel("beta"), None);
        assert_eq!(parse_channel("nightly"), None);
        assert_eq!(parse_channel("canary"), None);
        assert_eq!(parse_channel("rc"), None);
    }

    #[test]
    fn empty_string_returns_none() {
        // The wire values are non-empty, so the empty
        // string falls through. The caller pairs `parse_channel`
        // with `default_channel` to handle this.
        assert_eq!(parse_channel(""), None);
    }

    #[test]
    fn whitespace_returns_none() {
        // No trimming is done, so leading
        // or trailing whitespace falls through to None.
        assert_eq!(parse_channel(" stable"), None);
        assert_eq!(parse_channel("stable "), None);
        assert_eq!(parse_channel("\tstable\n"), None);
    }

    #[test]
    fn case_sensitivity() {
        // Exact string equality — case-sensitive.
        assert_eq!(parse_channel("Stable"), None);
        assert_eq!(parse_channel("STABLE"), None);
        assert_eq!(parse_channel("Latest"), None);
        assert_eq!(parse_channel("LATEST"), None);
        assert_eq!(parse_channel("LaTeSt"), None);
    }

    #[test]
    fn default_is_latest() {
        // Pins the default-is-latest rule.
        assert_eq!(default_channel(), ReleaseChannel::Latest);
    }

    #[test]
    fn npm_tag_is_identity_mapping() {
        // Pins the npm dist-tag mapping
        // Updater fallback and branch behavior are documented by the Rust helpers.
        assert_eq!(npm_tag(ReleaseChannel::Stable), "stable");
        assert_eq!(npm_tag(ReleaseChannel::Latest), "latest");
    }

    #[test]
    fn settings_compose_pattern() {
        // The full resolution rule for the setting.
        fn from_settings(s: Option<&str>) -> ReleaseChannel {
            s.and_then(parse_channel).unwrap_or(default_channel())
        }
        assert_eq!(from_settings(None), ReleaseChannel::Latest);
        assert_eq!(from_settings(Some("latest")), ReleaseChannel::Latest);
        assert_eq!(from_settings(Some("stable")), ReleaseChannel::Stable);
        // Garbage / typos / future-channel attempt → silently fall
        // back to default. An invalid
        // value in settings is ignored, not an error.
        assert_eq!(from_settings(Some("garbage")), ReleaseChannel::Latest);
        assert_eq!(from_settings(Some("")), ReleaseChannel::Latest);
        assert_eq!(from_settings(Some("Stable")), ReleaseChannel::Latest);
    }

    /// Exhaustive table for [`parse_channel`].
    #[test]
    fn channel_parse_table() {
        let cases: &[(&str, Option<ReleaseChannel>)] = &[
            ("stable", Some(ReleaseChannel::Stable)),
            ("latest", Some(ReleaseChannel::Latest)),
            ("Stable", None),
            ("STABLE", None),
            ("Latest", None),
            ("LATEST", None),
            ("beta", None),
            ("nightly", None),
            ("canary", None),
            ("rc", None),
            ("dev", None),
            ("", None),
            (" stable", None),
            ("stable ", None),
            ("stable\n", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_channel(input),
                *expected,
                "channel parse table failed for input {input:?}",
            );
        }
    }
}
