//! Stalled-animation reducer for spinner rows.
//!
//! The reducer tracks five values:
//!
//! * last token time — last time the response length grew.
//! * last response length — sentinel for detecting growth.
//! * mount time — time the spinner started (used when no tokens
//! have arrived yet).
//! * smoothed intensity — lerped toward the target intensity at 10%
//! per 50ms tick.
//! * last smooth time — last time the smoother ran.
//!
//! Lifecycle: the consumer owns a [`StalledAnimation`] struct and calls
//! [`StalledAnimation::tick`] once per frame with the absolute monotonic time.
//!
//! Rules pinned by the tests:
//!
//! * Stalled threshold: `> 3000ms` since last token.
//! * Fade duration: 2 seconds (`min((dt - 3000) / 2000, 1)`).
//! * Smoothing step: 10% per 50ms tick.
//! * Reset: any growth in `current_response_length` resets
//! `last_token_time`, `last_response_length`, and intensity.
//! * `has_active_tools` short-circuits stall detection and resets the
//! timer.
//! * `reduced_motion` skips smoothing — instant intensity.

/// Stall threshold in milliseconds. The spinner doesn't start fading
/// to red until `time_since_last_token > 3000`.
pub const STALLED_THRESHOLD_MS: u64 = 3000;

/// The fade-in duration. Once stalled, the intensity ramps from 0 to
/// 1 over this many milliseconds (the `/ 2000` in the formula).
pub const STALLED_FADE_DURATION_MS: u64 = 2000;

/// Smoothing tick interval in ms.
const SMOOTH_TICK_MS: u64 = 50;
/// Smoothing factor per tick.
const SMOOTH_FACTOR: f64 = 0.1;
/// Snap-to-target threshold. When `|target - current| < 0.01` we
/// snap to it.
const SNAP_EPSILON: f64 = 0.01;

/// The result of one [`StalledAnimation::tick`] — what the renderer
/// would consume.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StalledTick {
    /// Whether the spinner is currently stalled
    /// (`time_since_last_token > 3000`, with no active tools).
    pub is_stalled: bool,
    /// The smoothed intensity in `[0, 1]`. When `reduced_motion` is
    /// set this is the same as the raw target intensity.
    pub stalled_intensity: f64,
}

/// The retained state for the stalled-animation reducer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StalledAnimation {
    last_token_time: u64,
    last_response_length: u64,
    mount_time: u64,
    smoothed_intensity: f64,
    last_smooth_time: u64,
}

impl StalledAnimation {
    /// Create a new reducer at `time_ms` with `current_response_length`.
    /// The token timer, the length sentinel and the smoother all start
    /// from that instant.
    pub fn new(time_ms: u64, current_response_length: u64) -> Self {
        Self {
            last_token_time: time_ms,
            last_response_length: current_response_length,
            mount_time: time_ms,
            smoothed_intensity: 0.0,
            last_smooth_time: time_ms,
        }
    }

    /// Advance one frame.
    ///
    /// Order of operations:
    ///
    /// 1. If `current_response_length > last_response_length`, reset
    /// the timer / response length / intensity.
    /// 2. Compute `time_since_last_token`:
    /// * `has_active_tools` → `0` and reset `last_token_time` to now.
    /// * `current_response_length > 0` → `time - last_token_time`.
    /// * else → `time - mount_time`.
    /// 3. `is_stalled = time_since_last_token > 3000 && !has_active_tools`.
    /// 4. `intensity = is_stalled ? min((dt - 3000) / 2000, 1): 0`.
    /// 5. Smooth (10% per 50ms tick) unless `reduced_motion`.
    pub fn tick(
        &mut self,
        time_ms: u64,
        current_response_length: u64,
        has_active_tools: bool,
        reduced_motion: bool,
    ) -> StalledTick {
        // Step 1: detect new tokens.
        if current_response_length > self.last_response_length {
            self.last_token_time = time_ms;
            self.last_response_length = current_response_length;
            self.smoothed_intensity = 0.0;
            self.last_smooth_time = time_ms;
        }

        // Step 2: time since last token.
        let time_since_last_token: u64 = if has_active_tools {
            self.last_token_time = time_ms;
            0
        } else if current_response_length > 0 {
            time_ms.saturating_sub(self.last_token_time)
        } else {
            time_ms.saturating_sub(self.mount_time)
        };

        // Step 3: stalled flag.
        let is_stalled = time_since_last_token > STALLED_THRESHOLD_MS && !has_active_tools;

        // Step 4: target intensity.
        let intensity = if is_stalled {
            let over = time_since_last_token - STALLED_THRESHOLD_MS;
            ((over as f64) / (STALLED_FADE_DURATION_MS as f64)).min(1.0)
        } else {
            0.0
        };

        // Step 5: smooth (or skip when reduced motion).
        let effective = if reduced_motion {
            self.smoothed_intensity = intensity;
            self.last_smooth_time = time_ms;
            intensity
        } else if intensity > 0.0 || self.smoothed_intensity > 0.0 {
            let dt = time_ms.saturating_sub(self.last_smooth_time);
            if dt >= SMOOTH_TICK_MS {
                let steps = dt / SMOOTH_TICK_MS;
                let mut current = self.smoothed_intensity;
                for _ in 0..steps {
                    let diff = intensity - current;
                    if diff.abs() < SNAP_EPSILON {
                        current = intensity;
                        break;
                    }
                    current += diff * SMOOTH_FACTOR;
                }
                self.smoothed_intensity = current;
                self.last_smooth_time = time_ms;
            }
            self.smoothed_intensity
        } else {
            // intensity == 0 and smoothed == 0; nothing to do.
            self.smoothed_intensity = 0.0;
            self.last_smooth_time = time_ms;
            0.0
        };

        StalledTick {
            is_stalled,
            stalled_intensity: effective,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn fresh_state_not_stalled() {
        let mut s = StalledAnimation::new(0, 0);
        let t = s.tick(0, 0, false, false);
        assert!(!t.is_stalled);
        assert_eq!(t.stalled_intensity, 0.0);
    }

    #[test]
    fn before_threshold_not_stalled() {
        let mut s = StalledAnimation::new(0, 100);
        // 100 tokens, 2.9s elapsed.
        let t = s.tick(2900, 100, false, false);
        assert!(!t.is_stalled);
        assert_eq!(t.stalled_intensity, 0.0);
    }

    #[test]
    fn just_after_threshold_is_stalled() {
        let mut s = StalledAnimation::new(0, 100);
        let t = s.tick(3001, 100, false, false);
        assert!(t.is_stalled);
        // intensity = (1 / 2000) ~= 0.0005, smoothed → 0.00005.
        // First tick smooths from 0 toward target by 10%.
        assert!(t.stalled_intensity >= 0.0);
    }

    #[test]
    fn at_full_fade_intensity_caps_at_one() {
        // 5000 ms after last token, intensity target is min(2000/2000, 1) = 1.
        let mut s = StalledAnimation::new(0, 100);
        let mut last = 0.0;
        // Drive a few seconds of ticks.
        for ms in (0..6000).step_by(50) {
            last = s.tick(ms, 100, false, false).stalled_intensity;
        }
        // After 6 seconds (3s past threshold), intensity should be > 0.5.
        assert!(last > 0.5, "expected smoothed intensity > 0.5, got {last}");
    }

    #[test]
    fn token_growth_resets_stall() {
        let mut s = StalledAnimation::new(0, 100);
        let t1 = s.tick(4000, 100, false, false);
        assert!(t1.is_stalled);
        // New token arrives.
        let t2 = s.tick(4050, 200, false, false);
        assert!(!t2.is_stalled);
        assert_eq!(t2.stalled_intensity, 0.0);
    }

    #[test]
    fn active_tools_short_circuit() {
        let mut s = StalledAnimation::new(0, 100);
        // 5 seconds with active tools — never stalls.
        let t = s.tick(5000, 100, true, false);
        assert!(!t.is_stalled);
        assert_eq!(t.stalled_intensity, 0.0);
    }

    #[test]
    fn active_tools_resets_last_token_time() {
        let mut s = StalledAnimation::new(0, 100);
        // Tools active at t=4000 — would otherwise have stalled.
        let _ = s.tick(4000, 100, true, false);
        // Tools become inactive at t=4001 — only 1ms since "last
        // token" because we reset it during the active-tools tick.
        let t = s.tick(4001, 100, false, false);
        assert!(!t.is_stalled);
    }

    #[test]
    fn no_response_uses_mount_time() {
        // No tokens at all → time since last token is from mount.
        let mut s = StalledAnimation::new(1000, 0);
        // 4 seconds after mount, no tokens.
        let t = s.tick(5001, 0, false, false);
        assert!(t.is_stalled);
    }

    #[test]
    fn reduced_motion_skips_smoothing() {
        let mut s = StalledAnimation::new(0, 100);
        // First frame at t=4000: stalled, target intensity = 0.5.
        let t = s.tick(4000, 100, false, true);
        assert!(t.is_stalled);
        assert!(approx(t.stalled_intensity, 0.5));
    }

    #[test]
    fn smoothing_increments_each_tick() {
        let mut s = StalledAnimation::new(0, 100);
        // Drive 50ms ticks past threshold.
        let _ = s.tick(0, 100, false, false);
        let a = s.tick(3050, 100, false, false).stalled_intensity;
        let b = s.tick(3100, 100, false, false).stalled_intensity;
        let c = s.tick(3150, 100, false, false).stalled_intensity;
        // Each tick should be at least as large as the previous as we
        // approach the still-rising target.
        assert!(b >= a);
        assert!(c >= b);
    }

    #[test]
    fn smoothing_skips_when_dt_under_50ms() {
        let mut s = StalledAnimation::new(0, 100);
        let a = s.tick(3001, 100, false, false).stalled_intensity;
        // dt = 30ms, less than SMOOTH_TICK_MS — no change.
        let b = s.tick(3031, 100, false, false).stalled_intensity;
        assert_eq!(a, b);
    }

    #[test]
    fn snap_to_target_when_within_epsilon() {
        // Drive a long stable session — eventually intensity should
        // hit exactly 1.0 via the snap branch.
        let mut s = StalledAnimation::new(0, 100);
        let mut last = 0.0;
        for ms in (0..30_000).step_by(50) {
            last = s.tick(ms, 100, false, false).stalled_intensity;
        }
        assert!(last >= 0.99);
    }

    #[test]
    fn token_growth_clears_smoothed_state() {
        let mut s = StalledAnimation::new(0, 100);
        for ms in (0..6000).step_by(50) {
            let _ = s.tick(ms, 100, false, false);
        }
        // High intensity now.
        let t1 = s.tick(6000, 100, false, false);
        assert!(t1.stalled_intensity > 0.0);
        // Token arrives → smoothed intensity reset to 0.
        let t2 = s.tick(6050, 200, false, false);
        assert_eq!(t2.stalled_intensity, 0.0);
    }
}
