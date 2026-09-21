//! Slow-operation row of the developer bar on an internal build.
//!
//! The consumer re-reads its slow-operation list every 500ms when in a
//! dev or internal build and emits a single row of the form
//! `"[internal] slow sync: op1 (123ms) · op2 (456ms) · op3 (789ms)"`
//! showing only the most recent three operations.
//!
//! Pure logic covered here:
//!
//! 1. The 500ms tick interval ([`DEV_BAR_TICK_MS`]).
//! 2. The "last 3 operations" tail slice ([`recent_slow_ops`]).
//! 3. The `op (Nms)` formatter ([`format_slow_op`]).
//! 4. The hidden-when-empty / shown-when-non-empty branch
//!    ([`dev_bar_layout`]).
//!
//! The build-time gate is resolved by the consumer, which feeds the
//! resulting boolean in.

/// Tick interval between two slow-operation polls, in milliseconds.
pub const DEV_BAR_TICK_MS: u64 = 500;

/// How many slow operations are shown in the row at most: the newest
/// three.
pub const DEV_BAR_RECENT_LIMIT: usize = 3;

/// One slow-operation entry as the row builder reads it: an operation
/// name plus the wall time it took. A timestamp field, if the producer
/// carries one, is not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowOperation {
    /// Operation name.
    pub operation: String,
    /// How long the operation took.
    pub duration_ms: u64,
}

/// Display row emitted by the dev-bar: a single pre-formatted line the
/// consumer truncates at the end and paints with the warning color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevBarLayout {
    /// Pre-formatted text of the form
    /// `"[internal] slow sync: op (1ms) · op2 (2ms)"`.
    pub text: String,
}

/// Format a single slow-op entry as `"<operation> (<rounded>ms)"`.
/// The duration is rounded to a whole millisecond; since the `u64`
/// input is already integral, the rounding is a no-op here.
pub fn format_slow_op(op: &SlowOperation) -> String {
    format!("{} ({}ms)", op.operation, op.duration_ms)
}

/// Return at most the last [`DEV_BAR_RECENT_LIMIT`] operations,
/// preserving their order.
pub fn recent_slow_ops<'a>(ops: &'a [SlowOperation]) -> &'a [SlowOperation] {
    let len = ops.len();
    if len <= DEV_BAR_RECENT_LIMIT {
        ops
    } else {
        &ops[len - DEV_BAR_RECENT_LIMIT..]
    }
}

/// Build the dev-bar row. Returns `None` when the bar should be hidden
/// (gate disabled or no slow operations).
pub fn dev_bar_layout(should_show: bool, ops: &[SlowOperation]) -> Option<DevBarLayout> {
    if !should_show || ops.is_empty() {
        return None;
    }
    let recent = recent_slow_ops(ops);
    let text = recent
        .iter()
        .map(format_slow_op)
        .collect::<Vec<_>>()
        .join(" \u{00b7} ");
    Some(DevBarLayout {
        text: format!("[internal] slow sync: {text}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(name: &str, ms: u64) -> SlowOperation {
        SlowOperation {
            operation: name.into(),
            duration_ms: ms,
        }
    }

    #[test]
    fn tick_interval_is_500ms() {
        assert_eq!(DEV_BAR_TICK_MS, 500);
    }

    #[test]
    fn recent_limit_is_three() {
        assert_eq!(DEV_BAR_RECENT_LIMIT, 3);
    }

    #[test]
    fn format_slow_op_basic() {
        assert_eq!(format_slow_op(&op("read", 12)), "read (12ms)");
    }

    #[test]
    fn format_slow_op_zero_duration() {
        assert_eq!(format_slow_op(&op("noop", 0)), "noop (0ms)");
    }

    #[test]
    fn recent_slow_ops_keeps_all_when_under_limit() {
        let ops = vec![op("a", 1), op("b", 2)];
        assert_eq!(recent_slow_ops(&ops), &ops[..]);
    }

    #[test]
    fn recent_slow_ops_keeps_all_when_at_limit() {
        let ops = vec![op("a", 1), op("b", 2), op("c", 3)];
        assert_eq!(recent_slow_ops(&ops), &ops[..]);
    }

    #[test]
    fn recent_slow_ops_keeps_last_three_when_over_limit() {
        let ops = vec![op("a", 1), op("b", 2), op("c", 3), op("d", 4), op("e", 5)];
        let recent = recent_slow_ops(&ops);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0], op("c", 3));
        assert_eq!(recent[2], op("e", 5));
    }

    #[test]
    fn empty_input_yields_none() {
        assert_eq!(dev_bar_layout(true, &[]), None);
    }

    #[test]
    fn gate_disabled_yields_none_even_with_ops() {
        assert_eq!(dev_bar_layout(false, &[op("a", 1)]), None);
    }

    #[test]
    fn single_op_is_rendered() {
        let layout = dev_bar_layout(true, &[op("read", 50)]).expect("layout");
        assert_eq!(layout.text, "[internal] slow sync: read (50ms)");
    }

    #[test]
    fn multi_op_uses_middle_dot_separator() {
        let layout = dev_bar_layout(true, &[op("a", 1), op("b", 2), op("c", 3)]).expect("layout");
        assert_eq!(
            layout.text,
            "[internal] slow sync: a (1ms) \u{00b7} b (2ms) \u{00b7} c (3ms)"
        );
    }

    #[test]
    fn over_three_ops_only_shows_last_three() {
        let layout = dev_bar_layout(
            true,
            &[op("a", 1), op("b", 2), op("c", 3), op("d", 4), op("e", 5)],
        )
        .expect("layout");
        assert!(!layout.text.contains("a (1ms)"));
        assert!(!layout.text.contains("b (2ms)"));
        assert!(layout.text.contains("c (3ms)"));
        assert!(layout.text.contains("d (4ms)"));
        assert!(layout.text.contains("e (5ms)"));
    }

    #[test]
    fn internal_only_prefix_is_present() {
        let layout = dev_bar_layout(true, &[op("read", 1)]).expect("layout");
        assert!(layout.text.starts_with("[internal] slow sync: "));
    }
}
