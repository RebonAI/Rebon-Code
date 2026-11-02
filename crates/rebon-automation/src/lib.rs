//! Automation rules: prompts that launch a new background session on a
//! schedule — every N minutes, daily at a time, a cron expression — or when
//! another session's job fails or succeeds.
//!
//! Two files in the config home, each with one writer:
//!
//! - `automations.json` ([`AutomationRules`]) — the rules as the user wrote
//!   them. Written by whoever edits rules (the desktop app).
//! - `automation-state.json` ([`AutomationState`]) — what the rules have
//!   done: anchors, runs, counters. Written only by the background
//!   supervisor, which runs [`run_pass`] every few seconds and holds the
//!   supervisor lock while it does.
//!
//! A Run now is a request file the supervisor takes ([`request_run`]), so
//! runs are still written by one process. Launching a session and reading
//! jobs are the caller's: this crate names no session host, which keeps it
//! usable from the app and the CLI alike.

mod engine;
mod prompt;
mod requests;
mod rules;
pub mod schedule;
mod state;
pub mod threads;
mod trigger;

pub use engine::{run_pass, JobPhase, JobView, Launch, LaunchRequest};
pub use prompt::{render_prompt, PromptContext, TEMPLATE_VARS};
pub use requests::{
    has_run_requests, request_run, requests_dir, take_run_requests, RunRequest, REQUESTS_DIR_NAME,
};
pub use rules::{rules_path, AutomationRule, AutomationRules, RULES_FILE_NAME, RULES_VERSION};
pub use schedule::{local_stamp, LocalStamp};
pub use state::{
    state_path, AutomationRun, AutomationState, RuleState, RuleStats, MAX_RUNS_KEPT,
    STATE_FILE_NAME,
};
pub use trigger::{AutomationTrigger, TriggerKind};
