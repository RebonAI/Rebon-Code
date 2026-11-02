//! When a rule fires.

use serde::{Deserialize, Serialize};

/// When a rule fires. Serialized with an explicit `type` tag so hand-editing
/// `automations.json` stays predictable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationTrigger {
    /// Every `minutes` minutes (armed from the last fire, not wall-aligned).
    Interval { minutes: u32 },
    /// Once per local day at `hour:minute`. If nothing was running at that
    /// moment, it catches up the first check later that day.
    Daily { hour: u8, minute: u8 },
    /// A 5-field cron expression (`min hour dom mon dow`, local time),
    /// evaluated with the parser the CLI's `CronCreate` scheduler uses. Fires
    /// when the next run after the last fire has passed.
    Cron { expr: String },
    /// Another session's job reached `Failed`.
    JobFailed,
    /// Another session's job reached `Succeeded`.
    JobSucceeded,
}

/// Flat discriminant of [`AutomationTrigger`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerKind {
    Interval,
    Daily,
    Cron,
    JobFailed,
    JobSucceeded,
}

impl TriggerKind {
    pub const ALL: [TriggerKind; 5] = [
        TriggerKind::Interval,
        TriggerKind::Daily,
        TriggerKind::Cron,
        TriggerKind::JobFailed,
        TriggerKind::JobSucceeded,
    ];
}

impl AutomationTrigger {
    pub fn kind(&self) -> TriggerKind {
        match self {
            Self::Interval { .. } => TriggerKind::Interval,
            Self::Daily { .. } => TriggerKind::Daily,
            Self::Cron { .. } => TriggerKind::Cron,
            Self::JobFailed => TriggerKind::JobFailed,
            Self::JobSucceeded => TriggerKind::JobSucceeded,
        }
    }

    /// Fires from another job finishing rather than from the clock.
    pub fn is_event(&self) -> bool {
        matches!(self, Self::JobFailed | Self::JobSucceeded)
    }

    /// The English description a run records as what fired it. The desktop
    /// app localizes stored descriptions it recognizes when it shows them.
    pub fn summary(&self) -> String {
        match self {
            Self::Interval { minutes } => {
                if *minutes >= 60 && minutes % 60 == 0 {
                    match minutes / 60 {
                        1 => "Every hour".to_string(),
                        hours => format!("Every {hours} h"),
                    }
                } else {
                    format!("Every {minutes} min")
                }
            }
            Self::Daily { hour, minute } => format!("Daily {hour:02}:{minute:02}"),
            Self::Cron { expr } => rebon_tool::cron::cron_to_human(expr),
            Self::JobFailed => "Session failed".to_string(),
            Self::JobSucceeded => "Session done".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_round_trip_through_their_tagged_json() {
        for trigger in [
            AutomationTrigger::Interval { minutes: 15 },
            AutomationTrigger::Daily { hour: 9, minute: 5 },
            AutomationTrigger::Cron {
                expr: "*/5 * * * *".into(),
            },
            AutomationTrigger::JobFailed,
            AutomationTrigger::JobSucceeded,
        ] {
            let json = serde_json::to_value(&trigger).unwrap();
            assert!(json["type"].is_string(), "{json}");
            assert_eq!(
                serde_json::from_value::<AutomationTrigger>(json).unwrap(),
                trigger
            );
        }
        assert_eq!(
            serde_json::to_value(AutomationTrigger::Daily { hour: 9, minute: 0 }).unwrap(),
            serde_json::json!({"type": "daily", "hour": 9, "minute": 0})
        );
    }

    #[test]
    fn summaries_read_as_the_schedule() {
        let summary = |trigger: AutomationTrigger| trigger.summary();
        assert_eq!(
            summary(AutomationTrigger::Interval { minutes: 5 }),
            "Every 5 min"
        );
        assert_eq!(
            summary(AutomationTrigger::Interval { minutes: 60 }),
            "Every hour"
        );
        assert_eq!(
            summary(AutomationTrigger::Interval { minutes: 180 }),
            "Every 3 h"
        );
        assert_eq!(
            summary(AutomationTrigger::Interval { minutes: 90 }),
            "Every 90 min"
        );
        assert_eq!(
            summary(AutomationTrigger::Daily { hour: 7, minute: 3 }),
            "Daily 07:03"
        );
        assert_eq!(summary(AutomationTrigger::JobFailed), "Session failed");
        assert_eq!(summary(AutomationTrigger::JobSucceeded), "Session done");
    }

    #[test]
    fn only_job_triggers_are_events() {
        for kind in TriggerKind::ALL {
            let trigger = match kind {
                TriggerKind::Interval => AutomationTrigger::Interval { minutes: 1 },
                TriggerKind::Daily => AutomationTrigger::Daily { hour: 0, minute: 0 },
                TriggerKind::Cron => AutomationTrigger::Cron {
                    expr: "* * * * *".into(),
                },
                TriggerKind::JobFailed => AutomationTrigger::JobFailed,
                TriggerKind::JobSucceeded => AutomationTrigger::JobSucceeded,
            };
            assert_eq!(trigger.kind(), kind);
            assert_eq!(
                trigger.is_event(),
                matches!(kind, TriggerKind::JobFailed | TriggerKind::JobSucceeded)
            );
        }
    }
}
