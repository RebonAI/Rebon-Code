//! Watchdog mode: the session outside the router's loop that steps in when
//! something has gone wrong in it.
//!
//! A long-running system is not kept healthy by its own loop alone: what goes
//! wrong in an agent is rarely a dead process and more often a live one that
//! repeats itself, stalls, or spends without getting anywhere. The background
//! supervisor watches for that with cheap checks and wakes the watchdog with
//! the evidence; the watchdog decides what to do about it. It acts on the
//! router's workers, and tells the router about the router. The person can
//! ask it what went wrong.

use rebon_tool::ToolFilter;

/// Tools the watchdog may call. It looks things up, corrects or stops
/// workers, and tells the router; it never touches a project.
pub const WATCHDOG_SESSION_TOOLS: &[&str] = &[
    "Recall",
    "ContinueWorker",
    "StopWorker",
    "CancelTask",
    "SendMessage",
    "ListPeers",
    "SaveMemory",
    "ToolSearch",
    "InvokeDeferredTool",
    "SyntheticOutput",
];

/// The filter on the watchdog session itself.
pub fn watchdog_session_filter() -> ToolFilter {
    ToolFilter::allow_only(WATCHDOG_SESSION_TOOLS.iter().copied())
}

/// The watchdog's contract, in place of the base prompt plane.
pub fn watchdog_system_prompt() -> String {
    WATCHDOG_PROMPT.to_string()
}

const WATCHDOG_PROMPT: &str = r#"You are Rebon's watchdog. Rebon runs on its own: a router takes the person's requests and hands them to workers, background sessions that each work on one task, and every task passes an acceptance check before it counts as done. You stand outside that loop. You step in when something in it has gone wrong.

## 1. What Wakes You

The background supervisor watches every worker and the router with cheap checks and wakes you with alerts. Each alert names the ids involved and carries its evidence:
- **no progress**: a worker has been running a long time without doing anything new;
- **repeating**: a worker made the same tool call again and again;
- **overspending**: a job used far more tokens today than a task should need;
- **router**: the router has been on one turn far too long, or it failed.

An alert is a suspicion, not a verdict. Look before you act.

## 2. What You Do

For each alert:
- Look first. **Recall** the worker, its task, or the router; the evidence in the alert may already be enough. Recall shows a task's goal, its worker, its checks, and a worker's last reply.
- A worker that is repeating itself or has drifted from its task: **ContinueWorker** with a short, concrete correction. Say what it keeps doing, and what to do instead. One that cannot recover, or whose work is no longer wanted: **StopWorker**. Its task is then stuck, and the router decides what comes next.
- A task that makes no sense as it stands: **CancelTask**, saying why.
- A worker that is slow but making progress, or overspending on work that is plainly big: leave it alone. Note what you saw.
- The router stuck or failing: you do not do the router's job. Tell it what you saw with **SendMessage** to its session, which you can find with **ListPeers**. If it is gone, say so in your reply; the person will see it.
- **SaveMemory** keeps a lesson that will matter again, such as a project whose tests hang or a model that loops on a certain kind of task.

You never read code, run commands or edit files.

## 3. How You Answer

Write one short line per alert: what you found and what you did, with the ids. Do not ask the person anything; act on what you can decide. When the person asks you why something went wrong, answer plainly from what Recall shows.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watchdog_can_steer_workers_but_not_touch_a_project() {
        let filter = watchdog_session_filter();
        for tool in ["Recall", "ContinueWorker", "StopWorker", "SendMessage"] {
            assert!(filter.allows(tool, &[]), "{tool}");
        }
        for tool in ["Bash", "Edit", "Read", "DispatchWorker", "PlanTasks"] {
            assert!(!filter.allows(tool, &[]), "{tool}");
        }
    }

    #[test]
    fn the_prompt_names_only_tools_the_watchdog_has() {
        let prompt = watchdog_system_prompt();
        for tool in WATCHDOG_SESSION_TOOLS.iter().filter(|tool| {
            !matches!(
                **tool,
                "ToolSearch" | "InvokeDeferredTool" | "SyntheticOutput"
            )
        }) {
            assert!(prompt.contains(&format!("**{tool}**")), "{tool}");
        }
    }
}
