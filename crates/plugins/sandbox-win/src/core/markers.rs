//! The two stderr markers and the helper's own exit code.
//!
//! These strings are half of a frozen contract. The other half lives in the
//! sandbox plugin, which scans the helper's stderr line by line and takes the
//! first hit of:
//!
//! 1. a line beginning `sandbox-win: ` — everything after the prefix, or
//! 2. a line containing `SANDBOX_WIN_DENIED` — the whole line.
//!
//! They matter more than their size suggests. An ACL denial reaches the model
//! as whatever the confined program printed, which is usually a bare `Access is
//! denied` from deep inside a tool that has never heard of a sandbox. Without a
//! marker the model reads that as a permissions bug in the user's project and
//! starts trying to fix it.

/// The substring that marks a denial line for the caller.
pub const DENIED_MARKER: &str = "SANDBOX_WIN_DENIED";

/// The prefix that marks a helper-failure line for the caller.
pub const FAILURE_PREFIX: &str = "sandbox-win: ";

/// The helper's own failure exit code.
///
/// POSIX's "found but not executable". It has to stay distinguishable from the
/// child's exit code, which the helper relays verbatim, and 126 is the least
/// likely of the candidates to be a real one.
pub const EXIT_HELPER_FAILURE: i32 = 126;

/// The operations that can appear on a `SANDBOX_WIN_DENIED` line.
pub mod op {
    /// A read refused by the filesystem rules.
    pub const READ: &str = "read";
    /// A write refused by the filesystem rules.
    pub const WRITE: &str = "write";
    /// An outbound connection refused by the network filters.
    pub const NETWORK: &str = "network";
    /// A `--mask-file` that degraded to a plain deny.
    pub const MASK_DEGRADED: &str = "mask-degraded";
}

/// Builds a denial line: `SANDBOX_WIN_DENIED <op> <detail>`.
pub fn denied(operation: &str, detail: &str) -> String {
    format!(
        "{DENIED_MARKER} {} {}",
        single_line(operation),
        single_line(detail)
    )
}

/// Builds a helper-failure line: `sandbox-win: <reason>`.
pub fn failure(reason: &str) -> String {
    format!("{FAILURE_PREFIX}{}", single_line(reason))
}

/// Collapse anything that would split a single marker across two lines.
///
/// The caller scans line by line and takes the *first* match, so a reason
/// containing a newline would hand it half a sentence while the rest reads as
/// output from the confined command.
fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The caller's rules, reimplemented here rather than depended on.
    ///
    /// The sandbox plugin sits above this crate in the product and the helper ships
    /// separately, so depending on it would invert the layering for six lines of
    /// code. Both sides pin the same literals, and this test fails the moment they
    /// stop agreeing.
    fn extract_sandbox_error(stderr: &str) -> Option<String> {
        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("sandbox-win: ") {
                return Some(rest.to_string());
            }
            if trimmed.contains("SANDBOX_WIN_DENIED") {
                return Some(trimmed.to_string());
            }
        }
        None
    }

    #[test]
    fn a_denial_line_is_recognised_by_the_caller() {
        let line = denied(op::WRITE, r"C:\work\vendor\lock.json");
        assert_eq!(
            extract_sandbox_error(&line),
            Some(r"SANDBOX_WIN_DENIED write C:\work\vendor\lock.json".to_string())
        );
    }

    #[test]
    fn a_failure_line_is_recognised_by_the_caller() {
        let line = failure("the sandbox account is not provisioned");
        assert_eq!(
            extract_sandbox_error(&line),
            Some("the sandbox account is not provisioned".to_string())
        );
    }

    #[test]
    fn a_marker_still_lands_when_the_child_printed_first() {
        let stderr = format!("npm ERR! Access is denied\n{}\n", denied(op::READ, "C:/x"));
        assert!(extract_sandbox_error(&stderr)
            .expect("the marker must be found past unrelated output")
            .contains(DENIED_MARKER));
    }

    #[test]
    fn a_multiline_reason_is_flattened_into_one_marker() {
        let line = failure("could not open the ledger\nbecause the disk is full");
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("could not open the ledger because the disk is full"));
    }

    #[test]
    fn a_multiline_detail_cannot_smuggle_a_second_line_past_the_scanner() {
        let line = denied(op::READ, "C:/a\nSANDBOX_WIN_DENIED write C:/everything");
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn the_degraded_mask_op_is_the_one_the_rfc_names() {
        // This string is pinned: the caller's Windows warning and the macOS
        // `MASK_DOWNGRADED_TO_DENY` warning have to be recognisable as the same
        // degradation.
        assert_eq!(op::MASK_DEGRADED, "mask-degraded");
        assert!(denied(op::MASK_DEGRADED, r"C:\Users\u\.npmrc").contains("mask-degraded"));
    }

    #[test]
    fn the_failure_exit_code_stays_out_of_the_childs_way() {
        // 0, 1, 2 and the signal-ish 128+ range all belong to real programs.
        assert_eq!(EXIT_HELPER_FAILURE, 126);
    }

    #[test]
    fn unrelated_output_produces_no_marker() {
        assert_eq!(extract_sandbox_error("error: file not found"), None);
    }
}
