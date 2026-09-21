//! Runs of the same kind of activity, folded into one row.
//!
//! A turn that runs eight shell commands reads better as one "Run 8 shell
//! commands" row than as eight. The rule and the walk live here so that
//! folding runs stays a choice about what a surface shows rather than one
//! implementation per surface of when a run starts.
//!
//! ## Why this is generic and [`crate::fold_rows`] is not
//!
//! The fold answers *which rows group together* and hands back a plan over row
//! indices. This cannot: a run counts **tool calls**, and one assistant row
//! carries every call the model made that turn. Three `Bash` calls in one row
//! are one segment of that plan but three commands here, and a row of
//! `[Bash, Read, Bash]` breaks into two runs around the read — which no index
//! into that row can express.
//!
//! So the caller keeps its own items and its own granularity, and implements
//! [`ActivityItem`] to say what each one is. What is shared is the part that
//! actually drifts: which tools start a run, which ride along inside it, and
//! what the folded row says.

use serde_json::Value;

/// What a run of activity is a run *of*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityRunKind {
    /// Shell commands — `Bash`, `PowerShell`.
    Shell,
    /// Calls into one MCP server, named.
    Mcp(String),
}

impl ActivityRunKind {
    /// The one line a folded run of `count` shows.
    pub fn summary(&self, count: usize) -> String {
        match self {
            Self::Shell => format!("Run {count} shell commands"),
            Self::Mcp(server) => format!("Call {count} {server} MCPs"),
        }
    }
}

/// Borrowed run identity, probed for every item on every pass, so it does not
/// allocate; [`ActivityRunKind`] is minted only once a run actually starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityRunKey<'a> {
    /// See [`ActivityRunKind::Shell`].
    Shell,
    /// See [`ActivityRunKind::Mcp`].
    Mcp(&'a str),
}

impl ActivityRunKey<'_> {
    /// The owned key for a run that is starting.
    pub fn to_kind(self) -> ActivityRunKind {
        match self {
            Self::Shell => ActivityRunKind::Shell,
            Self::Mcp(server) => ActivityRunKind::Mcp(server.to_string()),
        }
    }

    /// Does this item belong to a run already keyed by `kind`?
    pub fn matches(self, kind: &ActivityRunKind) -> bool {
        match (kind, self) {
            (ActivityRunKind::Shell, Self::Shell) => true,
            (ActivityRunKind::Mcp(owned), Self::Mcp(server)) => owned == server,
            _ => false,
        }
    }
}

/// The tool name a run is keyed by, resolving a deferred call to the tool it
/// actually invokes.
pub fn activity_tool_name<'a>(name: &'a str, input: Option<&'a Value>) -> &'a str {
    if name != "InvokeDeferredTool" {
        return name;
    }
    input
        .and_then(|value| value.get("tool_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|inner| !inner.is_empty())
        .unwrap_or(name)
}

/// Which run, if any, a tool call belongs to.
pub fn activity_run_key<'a>(name: &'a str, input: Option<&'a Value>) -> Option<ActivityRunKey<'a>> {
    let name = activity_tool_name(name, input);
    if matches!(name, "Bash" | "PowerShell") {
        return Some(ActivityRunKey::Shell);
    }
    let server = name.strip_prefix("mcp__")?.split_once("__")?.0.trim();
    (!server.is_empty()).then_some(ActivityRunKey::Mcp(server))
}

/// The poll and stop rows a background shell produces between commands.
///
/// They are part of the shell's activity, not a new one: counting them as
/// ordinary rows is what splits every "Run N shell commands" group in a
/// background-shell turn, because `Bash → ShellOutput → … → Bash` never has
/// two adjacent `Bash` rows.
pub fn is_shell_management_tool(name: &str, input: Option<&Value>) -> bool {
    matches!(activity_tool_name(name, input), "ShellOutput" | "ShellStop")
}

/// The line a folded turn shows in place of everything it did.
pub fn worked_summary(elapsed_ms: Option<u64>) -> String {
    let Some(elapsed_ms) = elapsed_ms else {
        return "Worked".into();
    };
    let total_seconds = elapsed_ms / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("Worked {hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("Worked {minutes}m {seconds:02}s")
    } else {
        format!("Worked {seconds}s")
    }
}

/// What the walk needs to know about one of a caller's items.
pub trait ActivityItem {
    /// `(name, input)` when this item is a tool call, else `None`.
    fn tool(&self) -> Option<(&str, Option<&Value>)>;
    /// Whether this item is reasoning, which rides along inside a run rather
    /// than breaking it.
    fn is_thinking(&self) -> bool;
}

/// One entry of the compressed list: either an item the walk left alone, or a
/// run of two or more calls of the same kind with everything that rode along.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compressed<T> {
    /// Not part of any run.
    Item(T),
    /// A run of `count` calls. `members` holds every item in it, in order —
    /// the calls, the reasoning between them, and the shell's own poll rows.
    Run {
        /// What the run is a run of.
        kind: ActivityRunKind,
        /// How many calls it counts. Poll rows and reasoning are not calls.
        count: usize,
        /// Every item in the run.
        members: Vec<T>,
    },
}

/// Fold runs of two or more same-kind calls, leaving everything else in place.
///
/// A run absorbs, without counting: reasoning, and the poll or stop rows of
/// the shell it is polling. A single call is not a run and comes back as an
/// [`Compressed::Item`], so nothing is folded that would read worse folded.
pub fn compress_repeated_activity<T: ActivityItem>(items: Vec<T>) -> Vec<Compressed<T>> {
    struct Pending<T> {
        kind: ActivityRunKind,
        count: usize,
        members: Vec<T>,
    }

    fn flush<T>(pending: Option<Pending<T>>, out: &mut Vec<Compressed<T>>) {
        let Some(pending) = pending else {
            return;
        };
        if pending.count < 2 {
            out.extend(pending.members.into_iter().map(Compressed::Item));
            return;
        }
        out.push(Compressed::Run {
            kind: pending.kind,
            count: pending.count,
            members: pending.members,
        });
    }

    let mut out = Vec::with_capacity(items.len());
    let mut pending: Option<Pending<T>> = None;

    for item in items {
        let key = item.tool().and_then(|(name, input)| {
            if is_shell_management_tool(name, input) {
                None
            } else {
                activity_run_key(name, input).map(|key| key.to_kind())
            }
        });
        let rides_along = match item.tool() {
            // A poll belongs to the shell run it is polling.
            Some((name, input)) => {
                is_shell_management_tool(name, input)
                    && pending
                        .as_ref()
                        .is_some_and(|run| run.kind == ActivityRunKind::Shell)
            }
            None => item.is_thinking() && pending.is_some(),
        };
        if rides_along {
            pending.as_mut().expect("a run is open").members.push(item);
            continue;
        }
        let Some(kind) = key else {
            flush(pending.take(), &mut out);
            out.push(Compressed::Item(item));
            continue;
        };
        if pending.as_ref().is_some_and(|run| run.kind == kind) {
            let run = pending.as_mut().expect("a matching run is open");
            run.count += 1;
            run.members.push(item);
        } else {
            flush(pending.take(), &mut out);
            pending = Some(Pending {
                kind,
                count: 1,
                members: vec![item],
            });
        }
    }
    flush(pending, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, PartialEq, Eq)]
    enum Probe {
        Tool(String, Value),
        Thinking,
        Other,
    }

    impl Probe {
        fn tool(name: &str) -> Self {
            Self::Tool(name.into(), Value::Null)
        }
    }

    impl ActivityItem for Probe {
        fn tool(&self) -> Option<(&str, Option<&Value>)> {
            match self {
                Self::Tool(name, input) => Some((name.as_str(), Some(input))),
                _ => None,
            }
        }
        fn is_thinking(&self) -> bool {
            matches!(self, Self::Thinking)
        }
    }

    #[test]
    fn a_lone_call_is_not_a_run_and_two_are() {
        let out = compress_repeated_activity(vec![Probe::tool("Bash")]);
        assert!(matches!(out.as_slice(), [Compressed::Item(_)]));

        let out = compress_repeated_activity(vec![Probe::tool("Bash"), Probe::tool("PowerShell")]);
        let [Compressed::Run { kind, count, .. }] = out.as_slice() else {
            panic!("expected one run, got {out:?}");
        };
        assert_eq!(*kind, ActivityRunKind::Shell);
        assert_eq!(*count, 2);
        assert_eq!(kind.summary(*count), "Run 2 shell commands");
    }

    #[test]
    fn reasoning_and_poll_rows_ride_along_without_counting() {
        let out = compress_repeated_activity(vec![
            Probe::tool("Bash"),
            Probe::Thinking,
            Probe::tool("ShellOutput"),
            Probe::tool("Bash"),
        ]);
        let [Compressed::Run { count, members, .. }] = out.as_slice() else {
            panic!("expected one run, got {out:?}");
        };
        assert_eq!(*count, 2, "two commands, not four items");
        assert_eq!(members.len(), 4, "everything stays inside the run");
    }

    #[test]
    fn a_different_tool_between_two_calls_breaks_the_run() {
        let out = compress_repeated_activity(vec![
            Probe::tool("Bash"),
            Probe::tool("Read"),
            Probe::tool("Bash"),
        ]);
        assert_eq!(out.len(), 3, "three lone items, no run: {out:?}");
        assert!(out.iter().all(|entry| matches!(entry, Compressed::Item(_))));
    }

    #[test]
    fn one_server_is_one_run_and_two_servers_are_two() {
        let out = compress_repeated_activity(vec![
            Probe::tool("mcp__github__list_prs"),
            Probe::tool("mcp__github__get_pr"),
            Probe::tool("mcp__linear__list"),
            Probe::tool("mcp__linear__get"),
        ]);
        let [Compressed::Run {
            kind: first,
            count: two,
            ..
        }, Compressed::Run { kind: second, .. }] = out.as_slice()
        else {
            panic!("expected two runs, got {out:?}");
        };
        assert_eq!(*first, ActivityRunKind::Mcp("github".into()));
        assert_eq!(*second, ActivityRunKind::Mcp("linear".into()));
        assert_eq!(first.summary(*two), "Call 2 github MCPs");
    }

    #[test]
    fn a_deferred_call_is_keyed_by_the_tool_it_invokes() {
        let deferred = Probe::Tool(
            "InvokeDeferredTool".into(),
            json!({ "tool_name": " Bash " }),
        );
        let out = compress_repeated_activity(vec![deferred, Probe::tool("Bash")]);
        let [Compressed::Run { kind, count, .. }] = out.as_slice() else {
            panic!("expected one run, got {out:?}");
        };
        assert_eq!(*kind, ActivityRunKind::Shell);
        assert_eq!(*count, 2);
    }

    #[test]
    fn items_that_are_no_tool_at_all_pass_through() {
        let out = compress_repeated_activity(vec![Probe::Other, Probe::Thinking, Probe::Other]);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn worked_summary_reads_as_a_duration_or_says_nothing_about_one() {
        assert_eq!(worked_summary(None), "Worked");
        assert_eq!(worked_summary(Some(9_000)), "Worked 9s");
        assert_eq!(worked_summary(Some(125_000)), "Worked 2m 05s");
        assert_eq!(worked_summary(Some(3_725_000)), "Worked 1h 02m 05s");
    }
}
