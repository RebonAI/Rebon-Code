//! What `sandbox-win.exe status` prints.
//!
//! The caller runs `status` and does three `String::contains` tests against
//! stdout:
//!
//! | probe | meaning |
//! |---|---|
//! | `user=ok` | the downgraded accounts exist |
//! | `credentials=ok` | their passwords decrypt, so a non-interactive logon works |
//! | `wfp=ok` | the WFP sublayer and filters are installed |
//!
//! Because they are substring tests, the *absent* spellings matter as much as
//! the present ones: any line that happened to contain `user=ok` would report a
//! provisioned account. [`render`] never emits such a line and a test below pins
//! that.
//!
//! Three properties follow from the caller doing this on every session start:
//! `status` must **exit zero**, **not elevate**, and **change nothing**. It is a
//! read, and a `status` that repaired something would make the sandbox's state
//! depend on how often it was inspected.
//!
//! ## The version line
//!
//! The caller ignores `version=` today; it exists because the three booleans
//! cannot tell "the helper is too old" from "the helper is not set up", and an
//! old helper is the dangerous one — it ignores flags it does not know while
//! reporting success. [`crate::core::argv`] refuses unknown flags so that
//! failure is loud, and this line is how the caller will one day see it coming.

/// Version of the `status` contract, not of the binary.
pub const STATUS_VERSION: u32 = 1;

/// The three probe strings, exactly as the caller looks for them.
pub const PROBE_USER: &str = "user=ok";
pub const PROBE_CREDENTIALS: &str = "credentials=ok";
pub const PROBE_WFP: &str = "wfp=ok";

/// What `status` found. Each is checked independently because each has a
/// different remedy, and "sandbox unavailable" with no reason is the least
/// actionable thing a user can be told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusFacts {
    pub user: bool,
    pub credentials: bool,
    pub wfp: bool,
}

impl StatusFacts {
    pub const fn is_ready(&self) -> bool {
        self.user && self.credentials && self.wfp
    }
}

/// Render the `status` stdout.
pub fn render(facts: &StatusFacts) -> String {
    format!(
        "version={STATUS_VERSION}\nuser={}\ncredentials={}\nwfp={}\n",
        state(facts.user),
        state(facts.credentials),
        state(facts.wfp),
    )
}

const fn state(present: bool) -> &'static str {
    if present {
        "ok"
    } else {
        "missing"
    }
}

/// What a `status` output says, for the tests here and for a future
/// `selftest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedStatus {
    pub version: Option<u32>,
    pub facts: StatusFacts,
}

/// Read a `status` output back.
///
/// Uses the same `contains` tests the caller does, rather than a stricter line
/// parser, so this cannot accidentally accept an output the caller would reject
/// or vice versa.
pub fn parse(text: &str) -> ParsedStatus {
    let version = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("version=")
            .and_then(|value| value.trim().parse().ok())
    });
    ParsedStatus {
        version,
        facts: StatusFacts {
            user: text.contains(PROBE_USER),
            credentials: text.contains(PROBE_CREDENTIALS),
            wfp: text.contains(PROBE_WFP),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> StatusFacts {
        StatusFacts {
            user: true,
            credentials: true,
            wfp: true,
        }
    }

    #[test]
    fn a_ready_helper_emits_all_three_probes() {
        let text = render(&all());
        assert!(text.contains(PROBE_USER));
        assert!(text.contains(PROBE_CREDENTIALS));
        assert!(text.contains(PROBE_WFP));
    }

    #[test]
    fn the_probe_strings_are_the_ones_the_caller_looks_for() {
        // Transcribed from the caller's probe. If these drift, the helper reports a
        // state the caller cannot read and every Windows session falls back to "not
        // available".
        assert_eq!(PROBE_USER, "user=ok");
        assert_eq!(PROBE_CREDENTIALS, "credentials=ok");
        assert_eq!(PROBE_WFP, "wfp=ok");
    }

    #[test]
    fn a_missing_piece_is_absent_from_the_output_not_negated_in_it() {
        // The caller uses `contains`, so `wfp=not ok` would still report an installed
        // filter set. `missing` is chosen because no probe string is a substring of it.
        let facts = StatusFacts {
            user: true,
            credentials: true,
            wfp: false,
        };
        let text = render(&facts);

        assert!(!text.contains(PROBE_WFP), "{text}");
        assert!(text.contains("wfp=missing"));
    }

    #[test]
    fn no_probe_string_is_a_substring_of_another_line() {
        // Belt and braces: every combination of facts, checked against every probe.
        // This is the failure mode substring probing invites — a new line like
        // `superuser=ok` would light up `user=ok`.
        for user in [false, true] {
            for credentials in [false, true] {
                for wfp in [false, true] {
                    let facts = StatusFacts {
                        user,
                        credentials,
                        wfp,
                    };
                    let text = render(&facts);
                    assert_eq!(text.contains(PROBE_USER), user, "{text}");
                    assert_eq!(text.contains(PROBE_CREDENTIALS), credentials, "{text}");
                    assert_eq!(text.contains(PROBE_WFP), wfp, "{text}");
                }
            }
        }
    }

    #[test]
    fn nothing_installed_prints_no_probe_at_all() {
        let text = render(&StatusFacts::default());
        for probe in [PROBE_USER, PROBE_CREDENTIALS, PROBE_WFP] {
            assert!(!text.contains(probe), "{text}");
        }
    }

    #[test]
    fn the_version_line_comes_first() {
        let text = render(&all());
        assert_eq!(text.lines().next(), Some("version=1"));
    }

    #[test]
    fn output_round_trips_through_the_parser() {
        for user in [false, true] {
            for credentials in [false, true] {
                for wfp in [false, true] {
                    let facts = StatusFacts {
                        user,
                        credentials,
                        wfp,
                    };
                    let parsed = parse(&render(&facts));
                    assert_eq!(parsed.facts, facts);
                    assert_eq!(parsed.version, Some(STATUS_VERSION));
                }
            }
        }
    }

    #[test]
    fn output_from_a_helper_with_no_version_line_still_parses() {
        // What a helper from before the version line prints. The caller has to be able
        // to tell "old helper" from "broken helper", and an absent version is that
        // signal.
        let parsed = parse("user=ok\ncredentials=ok\nwfp=ok\n");
        assert_eq!(parsed.version, None);
        assert!(parsed.facts.is_ready());
    }

    #[test]
    fn readiness_needs_every_piece() {
        assert!(all().is_ready());
        for missing in 0..3 {
            let mut facts = all();
            match missing {
                0 => facts.user = false,
                1 => facts.credentials = false,
                _ => facts.wfp = false,
            }
            assert!(!facts.is_ready(), "case {missing}");
        }
    }

    #[test]
    fn the_output_ends_with_a_newline() {
        // It is written to a terminal often enough that a missing one shows.
        assert!(render(&all()).ends_with('\n'));
    }
}
