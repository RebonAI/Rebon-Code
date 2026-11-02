//! `automations.json`: the rules the user wrote. Its one writer is whoever
//! edits rules (the desktop app); the supervisor only reads it.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::AutomationTrigger;

/// File name of the rules file in the config home.
pub const RULES_FILE_NAME: &str = "automations.json";

/// The rules-file version from which the supervisor schedules the rules.
///
/// Version 1 files were written by an app that fired the rules itself. The
/// supervisor leaves those to it — two schedulers would fire every rule
/// twice — and takes over once an app that no longer fires writes version 2.
pub const RULES_VERSION: u32 = 2;

/// `<config_home>/automations.json`.
pub fn rules_path(config_home: &Path) -> PathBuf {
    config_home.join(RULES_FILE_NAME)
}

fn default_permission_mode() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

/// One rule as the user wrote it. What it has done since — when it last
/// fired, its runs — is the supervisor's, in [`crate::AutomationState`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRule {
    pub id: String,
    pub name: String,
    /// Prompt template sent to the launched session; see
    /// [`crate::TEMPLATE_VARS`].
    pub prompt: String,
    pub trigger: AutomationTrigger,
    /// Project the launched session runs in. `None` ⇒ the source job's
    /// project for event triggers; a time trigger without one cannot launch.
    #[serde(default)]
    pub project_cwd: Option<String>,
    /// Permission wire mode for the launched worker (`default` ⇒ the worker's
    /// own default).
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// The whole rules file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRules {
    /// The format the file was written in; [`Self::save`] always writes
    /// [`RULES_VERSION`]. Absent in the oldest files.
    #[serde(default)]
    pub version: u32,
    /// Global switch: off, nothing fires on its own (a manual run still goes).
    #[serde(default = "default_true")]
    pub master_enabled: bool,
    /// The user has been told, once, that armed rules spend provider quota
    /// with nobody watching. Absent in older files, which means "not yet".
    #[serde(default)]
    pub provider_cost_acknowledged: bool,
    /// The user has been asked, once, whether the supervisor should be
    /// started by the system so rules run after a restart.
    #[serde(default)]
    pub autostart_asked: bool,
    /// Tell a session what happens to the pull requests it opened — checks
    /// failing, changes requested, merged or closed — so it can follow up.
    #[serde(default = "default_true")]
    pub pr_follow_up: bool,
    #[serde(default)]
    pub rules: Vec<AutomationRule>,
}

impl Default for AutomationRules {
    fn default() -> Self {
        Self {
            version: RULES_VERSION,
            master_enabled: true,
            provider_cost_acknowledged: false,
            autostart_asked: false,
            pr_follow_up: true,
            rules: Vec::new(),
        }
    }
}

impl AutomationRules {
    /// Read the rules file. A missing file is no rules. Fields this build
    /// does not know — the run history older builds kept beside each rule —
    /// are ignored here; [`crate::AutomationState::load_or_migrate`] reads
    /// them once.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a rules file", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Write the rules file atomically, as [`RULES_VERSION`].
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let current = Self {
            version: RULES_VERSION,
            ..self.clone()
        };
        let body = serde_json::to_vec_pretty(&current).context("serialize the rules file")?;
        rebon_session::write_file_atomically(path, &body)
            .with_context(|| format!("write {}", path.display()))
    }

    /// Whether the supervisor schedules these rules, rather than the app
    /// that wrote an older file; see [`RULES_VERSION`]. A missing file is
    /// the supervisor's: there is nothing for an older app to fire.
    pub fn scheduled_by_supervisor(&self) -> bool {
        self.version >= RULES_VERSION
    }

    pub fn rule(&self, id: &str) -> Option<&AutomationRule> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    pub fn rule_mut(&mut self, id: &str) -> Option<&mut AutomationRule> {
        self.rules.iter_mut().find(|rule| rule.id == id)
    }

    /// Rules that fire on the clock and are switched on: what keeps the
    /// supervisor running with no session to supervise.
    pub fn has_armed_time_rule(&self) -> bool {
        self.master_enabled
            && self
                .rules
                .iter()
                .any(|rule| rule.enabled && !rule.trigger.is_event())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, trigger: AutomationTrigger) -> AutomationRule {
        AutomationRule {
            id: id.into(),
            name: id.into(),
            prompt: "do it".into(),
            trigger,
            project_cwd: Some("/repo".into()),
            permission_mode: "default".into(),
            enabled: true,
        }
    }

    #[test]
    fn a_missing_rules_file_is_no_rules() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            AutomationRules::load(&rules_path(dir.path())).unwrap(),
            AutomationRules::default()
        );
    }

    #[test]
    fn rules_round_trip_with_a_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = rules_path(dir.path());
        let rules = AutomationRules {
            version: RULES_VERSION,
            master_enabled: false,
            provider_cost_acknowledged: true,
            autostart_asked: true,
            pr_follow_up: false,
            rules: vec![rule("a", AutomationTrigger::Interval { minutes: 5 })],
        };
        rules.save(&path).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"], 2);
        assert_eq!(AutomationRules::load(&path).unwrap(), rules);
    }

    #[test]
    fn an_older_file_with_run_history_still_reads_its_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = rules_path(dir.path());
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1,
                "master_enabled": true,
                "rules": [{
                    "id": "a", "name": "A", "prompt": "p",
                    "trigger": {"type": "interval", "minutes": 5},
                    "last_fired_ms": 10,
                    "runs": [{"at_ms": 10, "trigger": "Every 5 min", "ok": true, "detail": "launched"}],
                    "stats": {"total_fires": 3}
                }]
            })
            .to_string(),
        )
        .unwrap();
        let rules = AutomationRules::load(&path).unwrap();
        assert!(
            !rules.scheduled_by_supervisor(),
            "the app that wrote it fires it"
        );
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].permission_mode, "default");
        assert!(rules.rules[0].enabled);
        assert_eq!(rules.rules[0].project_cwd, None);
    }

    #[test]
    fn saving_hands_the_rules_to_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let path = rules_path(dir.path());
        let mut old = AutomationRules::default();
        old.version = 1;
        assert!(!old.scheduled_by_supervisor());
        old.save(&path).unwrap();
        assert!(AutomationRules::load(&path)
            .unwrap()
            .scheduled_by_supervisor());
        assert!(
            AutomationRules::default().scheduled_by_supervisor(),
            "no file: nobody else fires"
        );
        let unversioned: AutomationRules = serde_json::from_str("{}").unwrap();
        assert_eq!(unversioned.version, 0);
        assert!(!unversioned.scheduled_by_supervisor());
    }

    #[test]
    fn an_unreadable_rules_file_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = rules_path(dir.path());
        std::fs::write(&path, b"{nope").unwrap();
        assert!(AutomationRules::load(&path).is_err());
    }

    #[test]
    fn only_switched_on_clock_rules_keep_the_supervisor_up() {
        let mut rules = AutomationRules {
            rules: vec![rule("e", AutomationTrigger::JobFailed)],
            ..Default::default()
        };
        assert!(
            !rules.has_armed_time_rule(),
            "events need running jobs anyway"
        );
        rules
            .rules
            .push(rule("t", AutomationTrigger::Daily { hour: 9, minute: 0 }));
        assert!(rules.has_armed_time_rule());
        rules.rules[1].enabled = false;
        assert!(!rules.has_armed_time_rule());
        rules.rules[1].enabled = true;
        rules.master_enabled = false;
        assert!(!rules.has_armed_time_rule());
    }
}
