//! Due-time arithmetic: when a clock rule is armed and when it is due.

use chrono::{Local, TimeZone, Timelike};

use crate::{AutomationTrigger, RuleState};

/// A resolved local instant: the day key for daily dedupe, minutes-of-day
/// for due checks, and the date / time strings prompt templates use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalStamp {
    /// `YYYY-MM-DD`
    pub day: String,
    pub minutes_of_day: u16,
    /// `HH:MM`
    pub time_label: String,
}

/// `now_ms` on this machine's local clock.
pub fn local_stamp(now_ms: u64) -> LocalStamp {
    let now = i64::try_from(now_ms)
        .ok()
        .and_then(|ms| Local.timestamp_millis_opt(ms).single())
        .unwrap_or_else(Local::now);
    LocalStamp {
        day: now.format("%Y-%m-%d").to_string(),
        minutes_of_day: (now.hour() * 60 + now.minute()) as u16,
        time_label: now.format("%H:%M").to_string(),
    }
}

fn daily_target(hour: u8, minute: u8) -> u16 {
    u16::from(hour) * 60 + u16::from(minute)
}

/// Arm `trigger`'s schedule at `now` so it does not fire on the spot:
/// interval and cron restart from `now`, and a daily rule whose time already
/// passed today waits for tomorrow.
pub fn arm(trigger: &AutomationTrigger, state: &mut RuleState, now_ms: u64, local: &LocalStamp) {
    match trigger {
        AutomationTrigger::Interval { .. } | AutomationTrigger::Cron { .. } => {
            state.last_fired_ms = Some(now_ms);
        }
        AutomationTrigger::Daily { hour, minute } => {
            if local.minutes_of_day >= daily_target(*hour, *minute) {
                state.last_fired_day = Some(local.day.clone());
            }
        }
        AutomationTrigger::JobFailed | AutomationTrigger::JobSucceeded => {}
    }
    state.armed_for = Some(trigger.clone());
}

/// Whether a clock trigger is due. Event triggers never are — they fire from
/// another job finishing — and neither is a schedule armed for a different
/// trigger than `trigger`, or not armed at all.
pub fn time_trigger_due(
    trigger: &AutomationTrigger,
    state: &RuleState,
    now_ms: u64,
    local: &LocalStamp,
) -> bool {
    if state.armed_for.as_ref() != Some(trigger) {
        return false;
    }
    match trigger {
        AutomationTrigger::Interval { minutes } => state.last_fired_ms.is_some_and(|last| {
            *minutes > 0 && now_ms.saturating_sub(last) >= u64::from(*minutes) * 60_000
        }),
        AutomationTrigger::Daily { hour, minute } => {
            local.minutes_of_day >= daily_target(*hour, *minute)
                && state.last_fired_day.as_deref() != Some(local.day.as_str())
        }
        AutomationTrigger::Cron { expr } => state.last_fired_ms.is_some_and(|last| {
            rebon_tool::cron::next_cron_run_ms(expr, last as i64)
                .is_some_and(|next| next <= now_ms as i64)
        }),
        AutomationTrigger::JobFailed | AutomationTrigger::JobSucceeded => false,
    }
}

/// Move a clock trigger's anchor to this fire (or this skipped fire), so the
/// next one is a period away rather than due again on the next check.
pub fn advance(
    trigger: &AutomationTrigger,
    state: &mut RuleState,
    now_ms: u64,
    local: &LocalStamp,
) {
    match trigger {
        AutomationTrigger::Interval { .. } | AutomationTrigger::Cron { .. } => {
            state.last_fired_ms = Some(now_ms);
        }
        AutomationTrigger::Daily { .. } => state.last_fired_day = Some(local.day.clone()),
        AutomationTrigger::JobFailed | AutomationTrigger::JobSucceeded => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(day: &str, minutes_of_day: u16) -> LocalStamp {
        LocalStamp {
            day: day.into(),
            minutes_of_day,
            time_label: format!("{:02}:{:02}", minutes_of_day / 60, minutes_of_day % 60),
        }
    }

    fn armed(trigger: &AutomationTrigger, now_ms: u64, local: &LocalStamp) -> RuleState {
        let mut state = RuleState::default();
        arm(trigger, &mut state, now_ms, local);
        state
    }

    #[test]
    fn an_interval_is_due_a_period_after_arming_and_not_before() {
        let trigger = AutomationTrigger::Interval { minutes: 5 };
        let local = stamp("2026-11-02", 600);
        let state = armed(&trigger, 1_000, &local);
        assert!(
            !time_trigger_due(&trigger, &state, 1_000, &local),
            "not on arming"
        );
        assert!(!time_trigger_due(&trigger, &state, 1_000 + 299_999, &local));
        assert!(time_trigger_due(&trigger, &state, 1_000 + 300_000, &local));
    }

    #[test]
    fn a_zero_interval_never_fires() {
        let trigger = AutomationTrigger::Interval { minutes: 0 };
        let local = stamp("2026-11-02", 600);
        let state = armed(&trigger, 0, &local);
        assert!(!time_trigger_due(&trigger, &state, u64::MAX, &local));
    }

    #[test]
    fn a_daily_rule_armed_after_its_time_waits_for_tomorrow() {
        let trigger = AutomationTrigger::Daily { hour: 9, minute: 0 };
        let state = armed(&trigger, 0, &stamp("2026-11-02", 10 * 60));
        assert!(!time_trigger_due(
            &trigger,
            &state,
            0,
            &stamp("2026-11-02", 23 * 60)
        ));
        assert!(!time_trigger_due(
            &trigger,
            &state,
            0,
            &stamp("2026-11-03", 8 * 60 + 59)
        ));
        assert!(time_trigger_due(
            &trigger,
            &state,
            0,
            &stamp("2026-11-03", 9 * 60)
        ));
    }

    #[test]
    fn a_daily_rule_armed_before_its_time_fires_that_day_and_once() {
        let trigger = AutomationTrigger::Daily { hour: 9, minute: 0 };
        let mut state = armed(&trigger, 0, &stamp("2026-11-02", 8 * 60));
        let at_nine = stamp("2026-11-02", 9 * 60);
        assert!(time_trigger_due(&trigger, &state, 0, &at_nine));
        advance(&trigger, &mut state, 0, &at_nine);
        assert!(!time_trigger_due(
            &trigger,
            &state,
            0,
            &stamp("2026-11-02", 15 * 60)
        ));
        // A late check the same day catches up; nothing ran at nine.
        let mut late = armed(&trigger, 0, &stamp("2026-11-02", 8 * 60));
        assert!(time_trigger_due(
            &trigger,
            &late,
            0,
            &stamp("2026-11-02", 22 * 60)
        ));
        advance(&trigger, &mut late, 0, &stamp("2026-11-02", 22 * 60));
        assert!(!time_trigger_due(
            &trigger,
            &late,
            0,
            &stamp("2026-11-02", 23 * 60)
        ));
    }

    #[test]
    fn a_cron_rule_is_due_once_its_next_run_after_the_anchor_passed() {
        let trigger = AutomationTrigger::Cron {
            expr: "*/5 * * * *".into(),
        };
        let local = stamp("2026-11-02", 600);
        let state = armed(&trigger, 1_700_000_000_000, &local);
        let next =
            rebon_tool::cron::next_cron_run_ms("*/5 * * * *", 1_700_000_000_000).unwrap() as u64;
        assert!(!time_trigger_due(&trigger, &state, next - 1, &local));
        assert!(time_trigger_due(&trigger, &state, next, &local));
    }

    #[test]
    fn nothing_is_due_unarmed_or_armed_for_another_trigger() {
        let five = AutomationTrigger::Interval { minutes: 5 };
        let ten = AutomationTrigger::Interval { minutes: 10 };
        let local = stamp("2026-11-02", 600);
        let unarmed = RuleState {
            last_fired_ms: Some(0),
            ..Default::default()
        };
        assert!(!time_trigger_due(&five, &unarmed, u64::MAX, &local));
        let state = armed(&five, 0, &local);
        assert!(
            !time_trigger_due(&ten, &state, u64::MAX, &local),
            "edited trigger"
        );
    }

    #[test]
    fn event_triggers_are_never_time_due() {
        for trigger in [
            AutomationTrigger::JobFailed,
            AutomationTrigger::JobSucceeded,
        ] {
            let local = stamp("2026-11-02", 600);
            let state = armed(&trigger, 0, &local);
            assert_eq!(state.armed_for.as_ref(), Some(&trigger));
            assert!(!time_trigger_due(&trigger, &state, u64::MAX, &local));
        }
    }

    #[test]
    fn local_stamps_have_the_template_shapes() {
        let stamp = local_stamp(1_700_000_000_000);
        assert_eq!(stamp.day.len(), 10);
        assert_eq!(&stamp.day[4..5], "-");
        assert_eq!(stamp.time_label.len(), 5);
        assert!(stamp.minutes_of_day < 24 * 60);
        let (h, m) = stamp.time_label.split_once(':').unwrap();
        assert_eq!(
            stamp.minutes_of_day,
            h.parse::<u16>().unwrap() * 60 + m.parse::<u16>().unwrap()
        );
    }
}
