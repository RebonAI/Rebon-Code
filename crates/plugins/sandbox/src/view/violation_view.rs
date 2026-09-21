//! View-model for the expanded sandbox-violation view. Pure
//! projection of the violation total, the tail of the violation list,
//! the sandbox-enabled flag and the platform into either "render
//! nothing" or a render struct with the header line, the row lines,
//! and the footer line.
//!
//! ## Behaviour notes
//!
//! [`build_violation_view`] returns `None` in the first two cases and
//! a [`ViolationView`] in the third:
//!
//! ```text
//! sandboxing disabled OR platform is linux -> None
//! total_count == 0                          -> None
//! otherwise -> Some(ViolationView {
//! header: "⧈ Sandbox blocked {total_count} total {unit}",
//! rows:   last 10 violation rows, oldest first,
//! footer: "… showing last {min(10, rows)} of {total_count}",
//! })
//! ```
//!
//! ## Pinned rules
//!
//! 1. **The view renders nothing if sandboxing is disabled.** Pinned
//! by [`build_violation_view`] returning `None`.
//! 2. **The view renders nothing on Linux.** Even if sandboxing is
//! enabled and there are violations, the Linux platform short-
//! circuits to nothing. The check is Linux-specific — Windows and
//! unknown still render. SECURITY-RELEVANT: this is the macOS-only
//! viewer; Linux uses bubblewrap which surfaces violations through a
//! different channel.
//! 3. **The view renders nothing if `total_count` is 0.** This is
//! independent of how many entries the consumer supplied — even with
//! stale violations in the list, a zero total hides the block.
//! 4. **Only the LAST 10 violations are kept.** The truncation
//! happens in [`tail_last_10`], which preserves the input order.
//! Pinned by [`VIOLATION_TAIL_LIMIT`].
//! 5. **Header pluralization is singular only at a count of 1.**
//! ZERO is "operations", but zero never reaches
//! this branch because rule 3 short-circuits. We still pin the
//! pluralization in [`format_header`].
//! 6. **Header literal is `"⧈ Sandbox blocked N total <unit>"`.**
//! `⧈` (U+29C8) is the squared box operator. Pinned by
//! [`SANDBOX_BLOCKED_PREFIX`].
//! 7. **Footer literal is `"… showing last K of N"`.** `K` is
//! `min(10, rendered rows)`. Note `…` is U+2026 (horizontal
//! ellipsis), NOT three dots. Pinned by [`format_footer`].

use crate::view::platform::SandboxPlatform;
use crate::view::violation::{format_violation_row, SandboxViolationEvent};

/// Maximum number of recent violations the expanded view shows.
/// Pinned by the tail truncation and by the footer's `min`.
pub const VIOLATION_TAIL_LIMIT: usize = 10;

/// `"⧈ Sandbox blocked"` — the literal header prefix. The leading
/// char is U+29C8 SQUARED BOX OPERATOR. Pinned literally.
pub const SANDBOX_BLOCKED_PREFIX: &str = "⧈ Sandbox blocked";

/// `"operation"` — unit used when the total count is exactly 1.
pub const OPERATION_SINGULAR: &str = "operation";

/// `"operations"` — unit used for every other total count, zero
/// included.
pub const OPERATION_PLURAL: &str = "operations";

/// `"… showing last"` — the literal footer prefix. Leading char is
/// U+2026 HORIZONTAL ELLIPSIS.
pub const SHOWING_LAST_PREFIX: &str = "… showing last";

/// Inputs to the view-model. The consumer fills these in from the
/// real violation store + adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViolationViewInputs {
    /// Whether sandboxing is switched on.
    pub sandboxing_enabled: bool,
    /// Platform the sandbox runtime is on.
    pub platform: SandboxPlatform,
    /// Total number of blocked operations recorded so far.
    pub total_count: usize,
    /// The full violations list. The view-model truncates it to the
    /// last [`VIOLATION_TAIL_LIMIT`] entries internally.
    pub all_violations: Vec<SandboxViolationEvent>,
}

/// Render output. `None` when the block is suppressed; `Some`
/// carries the three text fragments the renderer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViolationView {
    /// Header text — `"⧈ Sandbox blocked N total <unit>"`.
    pub header: String,
    /// Per-violation row texts, oldest first. At most
    /// [`VIOLATION_TAIL_LIMIT`] entries.
    pub rows: Vec<String>,
    /// Footer text — `"… showing last K of N"`.
    pub footer: String,
}

/// Take the last [`VIOLATION_TAIL_LIMIT`] entries from `items`,
/// preserving order. Returns the input unchanged when it is already
/// short enough.
pub fn tail_last_10<T: Clone>(items: &[T]) -> Vec<T> {
    if items.len() <= VIOLATION_TAIL_LIMIT {
        items.to_vec()
    } else {
        items[items.len() - VIOLATION_TAIL_LIMIT..].to_vec()
    }
}

/// Build the header line: `"⧈ Sandbox blocked {total_count} total
/// {unit}"`, with the unit singular only at a count of 1.
pub fn format_header(total_count: usize) -> String {
    let unit = if total_count == 1 {
        OPERATION_SINGULAR
    } else {
        OPERATION_PLURAL
    };
    format!("{} {} total {}", SANDBOX_BLOCKED_PREFIX, total_count, unit)
}

/// Build the footer line: `"… showing last K of N"` with
/// `K = min(rendered_count, VIOLATION_TAIL_LIMIT)`.
pub fn format_footer(rendered_count: usize, total_count: usize) -> String {
    let k = rendered_count.min(VIOLATION_TAIL_LIMIT);
    format!("{} {} of {}", SHOWING_LAST_PREFIX, k, total_count)
}

/// Pure view-model. Returns `None` for the cases that suppress the block
/// (sandboxing disabled, Linux, total_count zero) and `Some(view)`
/// otherwise.
pub fn build_violation_view(inputs: &ViolationViewInputs) -> Option<ViolationView> {
    if !inputs.sandboxing_enabled || inputs.platform.is_linux() {
        return None;
    }
    if inputs.total_count == 0 {
        return None;
    }
    let tail = tail_last_10(&inputs.all_violations);
    let rows: Vec<String> = tail.iter().map(format_violation_row).collect();
    Some(ViolationView {
        header: format_header(inputs.total_count),
        rows,
        footer: format_footer(tail.len(), inputs.total_count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(hour: u8, minute: u8, second: u8, line: &str) -> SandboxViolationEvent {
        SandboxViolationEvent {
            hour,
            minute,
            second,
            command: None,
            line: line.to_string(),
        }
    }

    fn make_inputs(
        enabled: bool,
        platform: SandboxPlatform,
        total: usize,
        n: usize,
    ) -> ViolationViewInputs {
        let mut violations = Vec::new();
        for i in 0..n {
            violations.push(ev(13, 30, (i % 60) as u8, &format!("denied {}", i)));
        }
        ViolationViewInputs {
            sandboxing_enabled: enabled,
            platform,
            total_count: total,
            all_violations: violations,
        }
    }

    #[test]
    fn tail_last_10_short_input_returns_all() {
        let xs = vec![1, 2, 3];
        assert_eq!(tail_last_10(&xs), vec![1, 2, 3]);
    }

    #[test]
    fn tail_last_10_exactly_10_returns_all() {
        let xs: Vec<usize> = (0..10).collect();
        assert_eq!(tail_last_10(&xs), xs);
    }

    #[test]
    fn tail_last_10_more_than_10_returns_last_10() {
        let xs: Vec<usize> = (0..15).collect();
        assert_eq!(tail_last_10(&xs), (5..15).collect::<Vec<_>>());
    }

    #[test]
    fn tail_last_10_empty_returns_empty() {
        let xs: Vec<usize> = vec![];
        assert!(tail_last_10(&xs).is_empty());
    }

    #[test]
    fn header_singular_for_one() {
        assert_eq!(format_header(1), "⧈ Sandbox blocked 1 total operation");
    }

    #[test]
    fn header_plural_for_two() {
        assert_eq!(format_header(2), "⧈ Sandbox blocked 2 total operations");
    }

    #[test]
    fn header_plural_for_zero() {
        // Note: rule-3 short-circuits before this is reached. We still
        // pin the pluralization branch for safety.
        assert_eq!(format_header(0), "⧈ Sandbox blocked 0 total operations");
    }

    #[test]
    fn header_large_count() {
        assert_eq!(
            format_header(1234),
            "⧈ Sandbox blocked 1234 total operations"
        );
    }

    #[test]
    fn footer_clamps_k_to_10() {
        assert_eq!(format_footer(5, 100), "… showing last 5 of 100");
        assert_eq!(format_footer(10, 100), "… showing last 10 of 100");
        // The clamp caps K at 10; we mirror that even though
        // build_violation_view never feeds more than 10 rows.
        assert_eq!(format_footer(15, 100), "… showing last 10 of 100");
    }

    #[test]
    fn footer_zero_renders_zero() {
        assert_eq!(format_footer(0, 5), "… showing last 0 of 5");
    }

    #[test]
    fn build_returns_none_when_disabled() {
        let inputs = make_inputs(false, SandboxPlatform::Macos, 5, 5);
        assert!(build_violation_view(&inputs).is_none());
    }

    #[test]
    fn build_returns_none_on_linux() {
        let inputs = make_inputs(true, SandboxPlatform::Linux, 5, 5);
        assert!(build_violation_view(&inputs).is_none());
    }

    #[test]
    fn build_returns_none_when_total_count_zero() {
        let inputs = make_inputs(true, SandboxPlatform::Macos, 0, 0);
        assert!(build_violation_view(&inputs).is_none());
    }

    #[test]
    fn build_returns_none_when_total_zero_even_with_stale_violations() {
        // SECURITY-RELEVANT: total_count and the supplied list can
        // diverge if the store was cleared but a stale subscription
        // payload is in the local state. The view-model honours
        // total_count.
        let inputs = make_inputs(true, SandboxPlatform::Macos, 0, 5);
        assert!(build_violation_view(&inputs).is_none());
    }

    #[test]
    fn build_returns_none_when_disabled_even_on_mac() {
        let inputs = make_inputs(false, SandboxPlatform::Macos, 5, 5);
        assert!(build_violation_view(&inputs).is_none());
    }

    #[test]
    fn build_returns_none_when_disabled_and_linux() {
        let inputs = make_inputs(false, SandboxPlatform::Linux, 5, 5);
        assert!(build_violation_view(&inputs).is_none());
    }

    #[test]
    fn build_returns_some_on_mac_with_violations() {
        let inputs = make_inputs(true, SandboxPlatform::Macos, 1, 1);
        let v = build_violation_view(&inputs).expect("should render");
        assert_eq!(v.header, "⧈ Sandbox blocked 1 total operation");
        assert_eq!(v.rows.len(), 1);
        assert_eq!(v.footer, "… showing last 1 of 1");
    }

    #[test]
    fn build_truncates_to_last_10() {
        let inputs = make_inputs(true, SandboxPlatform::Macos, 25, 25);
        let v = build_violation_view(&inputs).expect("should render");
        assert_eq!(v.rows.len(), 10);
        // The first row should be the 16th violation (index 15), since
        // tail_last_10 keeps the trailing 10.
        assert!(v.rows[0].contains("denied 15"));
        assert!(v.rows[9].contains("denied 24"));
    }

    #[test]
    fn build_footer_uses_min_10_and_total_count() {
        let inputs = make_inputs(true, SandboxPlatform::Macos, 50, 25);
        let v = build_violation_view(&inputs).expect("should render");
        assert_eq!(v.footer, "… showing last 10 of 50");
    }

    #[test]
    fn build_works_on_unknown_platform() {
        // Only Linux short-circuits; an "unknown" platform with
        // sandboxing enabled still renders. We pin that.
        let inputs = make_inputs(true, SandboxPlatform::Unknown, 1, 1);
        assert!(build_violation_view(&inputs).is_some());
    }

    #[test]
    fn render_decision_table() {
        // (enabled, platform, total, n_violations, expected_render)
        let table = [
            (true, SandboxPlatform::Macos, 0, 0, false),
            (true, SandboxPlatform::Macos, 1, 1, true),
            (true, SandboxPlatform::Linux, 1, 1, false),
            (true, SandboxPlatform::Linux, 100, 100, false),
            (false, SandboxPlatform::Macos, 1, 1, false),
            (false, SandboxPlatform::Linux, 1, 1, false),
            (true, SandboxPlatform::Windows, 1, 1, true),
            (true, SandboxPlatform::Unknown, 1, 1, true),
            (true, SandboxPlatform::Macos, 5, 5, true),
            (true, SandboxPlatform::Macos, 0, 5, false), // total trumps stale
        ];
        for (enabled, platform, total, n, expected) in table {
            let inputs = make_inputs(enabled, platform, total, n);
            let result = build_violation_view(&inputs);
            assert_eq!(
                result.is_some(),
                expected,
                "enabled={enabled} platform={platform:?} total={total} n={n}"
            );
        }
    }

    #[test]
    fn header_constant_pinned() {
        assert_eq!(SANDBOX_BLOCKED_PREFIX, "⧈ Sandbox blocked");
    }

    #[test]
    fn footer_constant_pinned() {
        assert_eq!(SHOWING_LAST_PREFIX, "… showing last");
    }

    #[test]
    fn singular_plural_pinned() {
        assert_eq!(OPERATION_SINGULAR, "operation");
        assert_eq!(OPERATION_PLURAL, "operations");
    }

    #[test]
    fn tail_limit_pinned_to_10() {
        assert_eq!(VIOLATION_TAIL_LIMIT, 10);
    }
}
