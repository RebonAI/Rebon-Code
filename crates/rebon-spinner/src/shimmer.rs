//! Shimmer-frame computation for spinner messages.
//!
//! The sweep direction depends on the spinner mode:
//!
//! * requesting → forward sweep (`(pos % cycle) - 10`), speed 50ms.
//! * everything else → reverse sweep (`message_width + 10 - (pos % cycle)`),
//!   speed 200ms.
//!
//! When stalled, the index is forced to `-100` (off-screen) to disable the
//! shimmer.
//!
//! The brief spinner re-uses the reverse-sweep formula through
//! [`compute_glimmer_index`].

/// The spinner mode. Determines glimmer sweep direction, glimmer
/// speed, and whether the tool-use flash branch applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinnerMode {
    /// Sending a request to the API. Forward sweep at 50ms.
    Requesting,
    /// Receiving a response. Reverse sweep at 200ms.
    Responding,
    /// Calling a tool. Reverse sweep at 200ms + tool-use flash.
    ToolUse,
    /// Streaming tool input. Reverse sweep at 200ms.
    ToolInput,
    /// Thinking (extended-thinking blocks). Reverse sweep at 200ms.
    Thinking,
}

/// The forward-sweep speed used in requesting mode, in milliseconds.
pub const REQUESTING_GLIMMER_SPEED_MS: u64 = 50;

/// The reverse-sweep speed used for every other mode, in milliseconds.
pub const TOOL_USE_GLIMMER_SPEED_MS: u64 = 200;

/// The off-screen sentinel index used when stalled.
pub const STALLED_GLIMMER_INDEX: i64 = -100;

/// The sweep speed for `mode`: forward speed in requesting mode,
/// reverse speed otherwise.
pub fn glimmer_speed_for_mode(mode: SpinnerMode) -> u64 {
    match mode {
        SpinnerMode::Requesting => REQUESTING_GLIMMER_SPEED_MS,
        _ => TOOL_USE_GLIMMER_SPEED_MS,
    }
}

/// Reverse-sweep glimmer index.
///
/// Returns `message_width + 10 - (tick % cycle_length)` where
/// `cycle_length = message_width + 20`. The index decreases over time,
/// sweeping right-to-left across the message and then 10 cells past
/// the left edge before wrapping. A non-positive `cycle_length` short-circuits
/// to `message_width + 10` so the modulo cannot divide by zero.
///
/// The remainder is the signed `%`, deliberately not `rem_euclid`: the two
/// differ for a negative numerator. A negative `tick` is never passed in, but
/// the signed semantics are kept on purpose.
pub fn compute_glimmer_index(tick: i64, message_width: i64) -> i64 {
    let cycle_length = message_width + 20;
    if cycle_length <= 0 {
        return message_width + 10;
    }
    let pos = tick % cycle_length;
    message_width + 10 - pos
}

/// The glimmer index for `mode`, including the stalled case.
///
/// * `is_stalled` → returns [`STALLED_GLIMMER_INDEX`].
/// * requesting → forward sweep `(cycle_position % cycle_length) - 10`.
/// * everything else → reverse sweep (same as
/// [`compute_glimmer_index`]).
pub fn compute_glimmer_index_for_mode(
    mode: SpinnerMode,
    time_ms: u64,
    message_width: i64,
    is_stalled: bool,
) -> i64 {
    if is_stalled {
        return STALLED_GLIMMER_INDEX;
    }
    let speed = glimmer_speed_for_mode(mode) as i64;
    let cycle_length = message_width + 20;
    if cycle_length <= 0 || speed == 0 {
        return message_width + 10;
    }
    let cycle_position = (time_ms as i64) / speed;
    let pos = cycle_position % cycle_length;
    match mode {
        SpinnerMode::Requesting => pos - 10,
        _ => message_width + 10 - pos,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glimmer_speed_requesting_is_fifty() {
        assert_eq!(glimmer_speed_for_mode(SpinnerMode::Requesting), 50);
    }

    #[test]
    fn glimmer_speed_other_modes_are_two_hundred() {
        for m in [
            SpinnerMode::Responding,
            SpinnerMode::ToolUse,
            SpinnerMode::ToolInput,
            SpinnerMode::Thinking,
        ] {
            assert_eq!(glimmer_speed_for_mode(m), 200);
        }
    }

    #[test]
    fn compute_glimmer_index_tick_zero() {
        // tick=0, width=10 → cycle=30, pos=0, returns 20.
        assert_eq!(compute_glimmer_index(0, 10), 20);
    }

    #[test]
    fn compute_glimmer_index_decreases_over_time() {
        let a = compute_glimmer_index(0, 10);
        let b = compute_glimmer_index(5, 10);
        let c = compute_glimmer_index(10, 10);
        // Reverse sweep: each tick subtracts 1.
        assert_eq!(a, 20);
        assert_eq!(b, 15);
        assert_eq!(c, 10);
    }

    #[test]
    fn compute_glimmer_index_wraps_at_cycle() {
        // cycle = width + 20 = 30. After 30 ticks we're back at tick=0.
        assert_eq!(compute_glimmer_index(0, 10), compute_glimmer_index(30, 10));
        assert_eq!(compute_glimmer_index(1, 10), compute_glimmer_index(31, 10));
    }

    #[test]
    fn compute_glimmer_index_zero_width_does_not_panic() {
        // cycle = 20; reverse sweep starts at 10.
        assert_eq!(compute_glimmer_index(0, 0), 10);
    }

    #[test]
    fn compute_glimmer_index_for_mode_stalled_is_minus_hundred() {
        assert_eq!(
            compute_glimmer_index_for_mode(SpinnerMode::Responding, 12345, 10, true),
            STALLED_GLIMMER_INDEX
        );
    }

    #[test]
    fn compute_glimmer_index_for_mode_requesting_at_t_zero() {
        // time=0 → cyclePosition=0 → pos=0 → 0 - 10 = -10.
        assert_eq!(
            compute_glimmer_index_for_mode(SpinnerMode::Requesting, 0, 10, false),
            -10
        );
    }

    #[test]
    fn compute_glimmer_index_for_mode_requesting_advances() {
        // After 50ms (one tick), pos should be 1.
        let v = compute_glimmer_index_for_mode(SpinnerMode::Requesting, 50, 10, false);
        assert_eq!(v, -9);
    }

    #[test]
    fn compute_glimmer_index_for_mode_responding_at_t_zero() {
        // pos=0 → returns width + 10 = 20.
        assert_eq!(
            compute_glimmer_index_for_mode(SpinnerMode::Responding, 0, 10, false),
            20
        );
    }

    #[test]
    fn compute_glimmer_index_for_mode_responding_after_one_tick() {
        // 200ms → pos=1 → returns 19.
        assert_eq!(
            compute_glimmer_index_for_mode(SpinnerMode::Responding, 200, 10, false),
            19
        );
    }

    #[test]
    fn compute_glimmer_index_for_mode_responding_wraps_at_full_cycle() {
        // cycle = 30; 30 * 200ms = 6000ms. At t=6000 we should be back
        // at the start.
        let zero = compute_glimmer_index_for_mode(SpinnerMode::Responding, 0, 10, false);
        let wrap = compute_glimmer_index_for_mode(SpinnerMode::Responding, 6000, 10, false);
        assert_eq!(zero, wrap);
    }

    #[test]
    fn compute_glimmer_index_for_mode_tool_use_uses_reverse_sweep() {
        let r = compute_glimmer_index_for_mode(SpinnerMode::ToolUse, 200, 10, false);
        let resp = compute_glimmer_index_for_mode(SpinnerMode::Responding, 200, 10, false);
        assert_eq!(r, resp);
    }

    #[test]
    fn compute_glimmer_index_for_mode_zero_message_width() {
        // No message — formula still defined.
        let v = compute_glimmer_index_for_mode(SpinnerMode::Responding, 0, 0, false);
        assert_eq!(v, 10);
    }
}
