//! The `status` command.
//!
//! Three rules, all of them contract:
//!
//! * **Exit zero.** The caller runs this on every session start and reads
//!   stdout. A non-zero exit on "nothing is installed" would turn the normal
//!   pre-install state into a spawn failure.
//! * **Do not elevate.** Prompting for UAC on a status check would make opening
//!   Rebon a security dialog.
//! * **Change nothing.** It is a read. A `status` that repaired something would
//!   make the machine's state depend on how often it was inspected.
//!
//! The caller tests stdout with three `String::contains` calls, so what is
//! *absent* matters as much as what is present. Diagnostics for humans are
//! allowed alongside — the caller ignores them — but none of them may contain a
//! probe string, which is what [`render`]'s tests check.

use crate::core::status::{render, StatusFacts};
use crate::sys::wfp::WfpProbe;
use crate::sys::{credentials, paths, principal, wfp};

/// Everything `status` found, plus why, for the human-readable half.
pub struct StatusReport {
    pub facts: StatusFacts,
    pub notes: Vec<String>,
}

/// Probe the machine. Never fails: every failure is a piece reported missing.
pub fn probe() -> StatusReport {
    let mut notes = Vec::new();

    let accounts = match principal::probe_accounts() {
        Ok(accounts) => accounts,
        Err(error) => {
            notes.push(format!("could not look up the sandbox accounts: {error}"));
            Default::default()
        }
    };
    if accounts.is_partial() {
        // Distinct from "not installed", and it needs a different sentence: an
        // interrupted install leaves a machine that is neither state, and "run install"
        // over the top of it is not obviously the right advice.
        notes.push(format!(
            "install did not finish — missing: {}",
            accounts.missing().join(", ")
        ));
    }

    let credentials_ok = match paths::credentials_path() {
        Ok(path) => {
            let usable = credentials::are_usable(&path);
            if !usable && path.exists() {
                // The file is there and will not decrypt: a different user, a restored profile,
                // a machine rebuild. Reporting only "missing" would send someone looking for a
                // file that is sitting right there.
                notes.push(format!(
                    "{} exists but could not be decrypted — it belongs to a different \
                     Windows user or profile; re-run install",
                    path.display()
                ));
            }
            usable
        }
        Err(error) => {
            notes.push(error.to_string());
            false
        }
    };

    let filters = wfp::probe();
    if let WfpProbe::Partial { filters } = &filters {
        notes.push(format!(
            "{filters} of the {} network filters are present — the machine is neither filtered nor clean; re-run install",
            crate::core::wfp::EXPECTED_FILTERS
        ));
    }
    if let WfpProbe::Unreadable { detail } = &filters {
        // The gap documented in `crate::sys::wfp`: opening the filter engine needs a
        // right ordinary accounts do not have, so a non-elevated status cannot see the
        // filters at all. Reported as missing, because a probe that could not look is
        // not evidence.
        notes.push(format!(
            "the network filters could not be read, so they are reported as missing \
             ({detail})"
        ));
    }

    StatusReport {
        facts: StatusFacts {
            user: accounts.is_complete(),
            credentials: credentials_ok,
            wfp: filters.is_installed(),
        },
        notes,
    }
}

/// The full stdout for `status`.
pub fn render_report(report: &StatusReport) -> String {
    let mut text = render(&report.facts);
    for note in &report.notes {
        // `#` so a human can tell a diagnostic from a probe line, and so no future note
        // can be mistaken for one.
        text.push_str(&format!("# {}\n", single_line(note)));
    }
    text
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::status::{PROBE_CREDENTIALS, PROBE_USER, PROBE_WFP};

    fn report(facts: StatusFacts, notes: &[&str]) -> StatusReport {
        StatusReport {
            facts,
            notes: notes.iter().map(|note| (*note).to_string()).collect(),
        }
    }

    #[test]
    fn a_note_can_never_be_mistaken_for_a_probe_line() {
        // The caller reads stdout with `contains`, so a diagnostic that happened to
        // spell `wfp=ok` would report installed filters on a machine that has none.
        // Every note is checked against every probe.
        let notes = [
            "the network filters could not be read, so they are reported as missing",
            "install did not finish — missing: rebon-sbx, rebon-sbx-grp",
            "C:\\x\\credentials.bin exists but could not be decrypted",
        ];
        let text = render_report(&report(StatusFacts::default(), &notes));

        for probe in [PROBE_USER, PROBE_CREDENTIALS, PROBE_WFP] {
            assert!(!text.contains(probe), "note leaked {probe}:\n{text}");
        }
    }

    #[test]
    fn the_probe_lines_still_reflect_the_facts_with_notes_present() {
        let text = render_report(&report(
            StatusFacts {
                user: true,
                credentials: false,
                wfp: false,
            },
            &["could not read the filter engine"],
        ));

        assert!(text.contains(PROBE_USER));
        assert!(!text.contains(PROBE_CREDENTIALS));
        assert!(!text.contains(PROBE_WFP));
    }

    #[test]
    fn notes_are_prefixed_and_never_span_two_lines() {
        // A note that wrapped onto a second line would produce an unprefixed line the
        // caller has no rule for.
        let text = render_report(&report(
            StatusFacts::default(),
            &["first line\nsecond line"],
        ));

        for line in text.lines().filter(|line| !line.contains('=')) {
            assert!(line.starts_with("# "), "unprefixed diagnostic line: {line}");
        }
    }

    #[test]
    fn a_report_with_no_notes_is_just_the_contract() {
        let text = render_report(&report(
            StatusFacts {
                user: true,
                credentials: true,
                wfp: true,
            },
            &[],
        ));
        assert_eq!(text, "version=1\nuser=ok\ncredentials=ok\nwfp=ok\n");
    }

    #[test]
    fn probing_never_panics_and_reports_missing_on_this_machine() {
        // Runs the real probe. On a machine where the helper has never been installed
        // every piece is missing, and getting that far without an error is the
        // assertion.
        let report = probe();
        let text = render_report(&report);

        assert!(text.starts_with("version="));
        if !report.facts.is_ready() {
            assert!(!text.contains(PROBE_USER) || !text.contains(PROBE_WFP));
        }
    }
}
