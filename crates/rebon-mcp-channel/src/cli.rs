//! `rebon mcp …`: the clap half. The binary parses it and hands over what
//! only the binary knows — its store, its executable, its launch policy.

use std::path::PathBuf;

use anyhow::Context;
use rebon_session_host::BackgroundStore;

use crate::jobs::LaunchGate;
use crate::ledger::LedgerOwner;
use crate::server::{serve, ServeConfig};
use crate::watch::Cadence;

#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum McpCommand {
    /// Serve Rebon's background jobs to an MCP client over stdio.
    ///
    /// Offers exec_start / job_status / job_result / job_cancel / job_reply /
    /// job_permit, and pushes a job's outcome back to the client over the MCP
    /// channel extension (`notifications/claude/channel`) when it finishes or
    /// stops to wait for an answer. The jobs belong to Rebon's background
    /// supervisor, not to this process: closing the client leaves them running.
    ///
    /// Also offers sessions_list / session_read, which read — never change —
    /// the conversations other agents have had in the project: Rebon's
    /// sessions, and Claude Code's (under CLAUDE_CONFIG_DIR, else ~/.claude).
    ///
    /// Configure it in a client as the command `rebon mcp serve`, started in
    /// the project directory; jobs may run, and sessions be read, only in that
    /// directory or below it.
    Serve(ServeArgs),
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct ServeArgs {
    /// Do not declare the channel capability or push anything; the client
    /// checks `job_status` instead. For clients that cannot take pushes.
    #[arg(long = "no-channel")]
    pub no_channel: bool,
    /// Also offer `channel_probe`, a tool that pushes one message at once —
    /// the quickest way to find out whether pushes reach a session at all.
    #[arg(long)]
    pub probe: bool,
}

/// Run a `rebon mcp` subcommand.
///
/// `store` and `rebon_exe` are the binary's own (its config home, its
/// executable for starting the supervisor). `launch_gate` is checked before
/// every job is launched; stdout is the MCP connection, so nothing else may
/// write to it.
pub async fn run(
    command: McpCommand,
    store: BackgroundStore,
    rebon_exe: PathBuf,
    launch_gate: LaunchGate,
) -> anyhow::Result<()> {
    match command {
        McpCommand::Serve(args) => {
            let root = std::env::current_dir()
                .context("rebon mcp serve needs a working directory to confine jobs to")?;
            // The connection gets its own ends of stdin and stdout before
            // `exec_start` can launch anything, so no process this server
            // starts can inherit, block on or write into them. See
            // `rebon_proto::process_stdio`.
            let stdio = rebon_proto::process_stdio::take_process_stdio()
                .context("failed to take stdio for the MCP connection")?;
            serve(
                stdio.input,
                stdio.output,
                ServeConfig {
                    store,
                    projects_root: rebon_session::default_projects_root(),
                    claude_config_dir: crate::sessions::claude_config_dir_from_env(),
                    groups_root: rebon_group::default_root(
                        &rebon_session::default_config_home_dir(),
                    ),
                    caller: rebon_group::identity::detect(),
                    root,
                    rebon_exe,
                    launch_gate,
                    channel: !args.no_channel,
                    probe: args.probe,
                    owner: LedgerOwner::this_process(),
                    cadence: Cadence::default(),
                },
            )
            .await
        }
    }
}

/// `rebon group …`: agent groups (RFC-0009) from outside a session.
///
/// Besides `hook`, these are the group tools `rebon mcp serve` offers, as
/// commands: for an agent that has a shell but no MCP connection to Rebon
/// (the `rebon-groups` skill, `rebon group skill`, teaches them). Each one
/// runs the tool of the same name as the same member the MCP server would,
/// in the project directory it is run in, and prints the tool's JSON result.
#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum GroupCommand {
    /// Print what this session's agent group wrote to it, as hook context.
    ///
    /// Run by Claude Code or Codex as a command hook on UserPromptSubmit,
    /// SessionStart and PostToolUse, with the hook's JSON on stdin. It prints
    /// `{"hookSpecificOutput": {"additionalContext": …}}` when there is
    /// something new, and nothing otherwise; it never fails the hook.
    Hook(HookArgs),
    /// Show this session's group: its members, your alias, what you have not
    /// read. Outside a group, the groups in this project you could join.
    Info(WhoArgs),
    /// Join a group in this project, creating it unless --no-create.
    Join(JoinArgs),
    /// Leave this session's group.
    Leave(WhoArgs),
    /// Write to the group: a note, a request, or the reply to one.
    Send(SendArgs),
    /// Read what the group wrote to you, or to all, since you last read.
    Inbox(InboxArgs),
    /// Add one fact to the group's shared memory.
    Remember(RememberArgs),
    /// Read the group's shared memory.
    Recall(RecallArgs),
    /// Print the rebon-groups skill, which teaches an agent these commands,
    /// or write it into skill directories.
    Skill(SkillArgs),
}

/// Who is calling, when the environment does not say or says wrong.
///
/// Given, it wins over the environment — unlike `group_join`'s own `agent`
/// and `session_id`, which count only when the MCP server's environment
/// named nobody. An MCP server is started by its agent directly; a command
/// runs from a shell that may carry several agents' variables (pi inside a
/// Rebon session over ACP has both `REBON_SESSION_ID` and `PI_SESSION_ID`),
/// and the agent reading its own variable knows which one is it.
#[derive(Debug, Default, clap::Args, PartialEq, Eq)]
pub struct WhoArgs {
    /// Your agent program (pi, claude-code, codex, …), when the environment
    /// does not tell your session or tells the wrong one. Needs
    /// --session-id.
    #[arg(long, requires = "session_id")]
    pub agent: Option<String>,
    /// Your session id (pi: `$PI_SESSION_ID`), with --agent.
    #[arg(long, requires = "agent")]
    pub session_id: Option<String>,
}

impl WhoArgs {
    fn caller(&self) -> Option<rebon_group::Caller> {
        let agent = self.agent.as_deref()?.trim();
        let session_id = self.session_id.as_deref()?.trim();
        (!agent.is_empty() && !session_id.is_empty()).then(|| rebon_group::Caller {
            agent: agent.to_lowercase(),
            session_id: session_id.to_string(),
        })
    }
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct JoinArgs {
    /// The group's name (or id).
    pub group: String,
    /// What the others call you; by default made from your agent and
    /// session.
    #[arg(long)]
    pub alias: Option<String>,
    /// One line on what you do in the group.
    #[arg(long)]
    pub role: Option<String>,
    /// Join only a group that already exists.
    #[arg(long)]
    pub no_create: bool,
    #[command(flatten)]
    pub who: WhoArgs,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct SendArgs {
    /// A member's alias, `all`, or `user` (the person you work for, who
    /// reads the group in the desktop app).
    pub to: String,
    /// What to say. `-` reads it from stdin.
    pub text: String,
    /// note (the default) tells, request asks and returns a request id,
    /// reply answers the request named in --re.
    #[arg(long, value_parser = ["note", "request", "reply"])]
    pub kind: Option<String>,
    /// For a reply: the request id it answers (e.g. r12).
    #[arg(long)]
    pub re: Option<String>,
    #[command(flatten)]
    pub who: WhoArgs,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct InboxArgs {
    /// Whole entries rather than their first lines.
    #[arg(long)]
    pub full: bool,
    /// Leave them unread.
    #[arg(long)]
    pub peek: bool,
    #[command(flatten)]
    pub who: WhoArgs,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct RememberArgs {
    /// The fact, stated so it stands on its own.
    pub fact: String,
    /// The number (#) of the fact this one replaces.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub supersedes: Option<u64>,
    #[command(flatten)]
    pub who: WhoArgs,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct RecallArgs {
    /// Only the facts containing this text (case-insensitive).
    pub query: Option<String>,
    #[command(flatten)]
    pub who: WhoArgs,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct SkillArgs {
    /// Write the skill to `<SKILLS_DIR>/rebon-groups/SKILL.md` instead of
    /// printing it. Repeatable: `.agents/skills` is read by pi, Codex and
    /// OpenCode, `.claude/skills` by Claude Code. A SKILL.md there that this
    /// command did not write is left alone.
    #[arg(long = "write", value_name = "SKILLS_DIR")]
    pub write: Vec<PathBuf>,
    /// The program the skill tells the agent to run.
    #[arg(long, default_value = "rebon")]
    pub command: String,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct HookArgs {
    /// The agent running the hook: claude-code or codex.
    #[arg(long)]
    pub agent: String,
}

/// `rebon permission …`: an agent CLI's permission requests, asked in the
/// Rebon app that runs it.
#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum PermissionCommand {
    /// Hand a permission request to the Rebon app and print its answer.
    ///
    /// Run by Claude Code or Codex as their PermissionRequest hook, which the
    /// app adds when it starts one, with the request as JSON on stdin. It
    /// waits for the user's answer in the app and prints allow or deny; it
    /// prints nothing — and the CLI asks on its own screen — when the app is
    /// not there to ask, the user turns to the terminal, or anything fails.
    Hook(HookArgs),
}

/// Run a `rebon permission` subcommand.
pub fn run_permission(command: PermissionCommand) -> anyhow::Result<()> {
    match command {
        PermissionCommand::Hook(args) => {
            use std::io::Read;
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_err() {
                return Ok(());
            }
            let output = rebon_permission_relay::run_hook(
                &args.agent,
                &input,
                |name| std::env::var(name).ok(),
                // Unknown counts as there: the wait has a deadline anyway.
                |pid| rebon_session_host::process_is_running(pid) != Some(false),
            );
            if let Some(output) = output {
                println!("{output}");
            }
            Ok(())
        }
    }
}

/// Run a `rebon group` subcommand.
pub fn run_group(command: GroupCommand) -> anyhow::Result<()> {
    use rebon_group::tools;
    use serde_json::json;

    let (name, arguments, who) = match command {
        GroupCommand::Hook(args) => return run_hook(&args),
        GroupCommand::Skill(args) => return run_skill(&args),
        GroupCommand::Info(who) => (tools::GROUP_INFO, json!({}), who),
        GroupCommand::Leave(who) => (tools::GROUP_LEAVE, json!({}), who),
        GroupCommand::Join(args) => (
            tools::GROUP_JOIN,
            json!({
                "group": args.group,
                "alias": args.alias,
                "role": args.role,
                "create": !args.no_create,
            }),
            args.who,
        ),
        GroupCommand::Send(args) => {
            let text = if args.text == "-" {
                use std::io::Read;
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("failed to read the message from stdin")?;
                text
            } else {
                args.text
            };
            (
                tools::GROUP_SEND,
                json!({ "to": args.to, "text": text, "kind": args.kind, "re": args.re }),
                args.who,
            )
        }
        GroupCommand::Inbox(args) => (
            tools::GROUP_INBOX,
            json!({ "full": args.full, "peek": args.peek }),
            args.who,
        ),
        GroupCommand::Remember(args) => (
            tools::GROUP_REMEMBER,
            json!({ "fact": args.fact, "supersedes": args.supersedes }),
            args.who,
        ),
        GroupCommand::Recall(args) => (
            tools::GROUP_RECALL,
            json!({ "query": args.query }),
            args.who,
        ),
    };
    let store = rebon_group::GroupStore::new(rebon_group::default_root(
        &rebon_session::default_config_home_dir(),
    ));
    let cwd = std::env::current_dir().context("rebon group needs a working directory")?;
    let root = crate::jobs::canonical_dir(&cwd)?;
    let value = group_call(
        &store,
        &root,
        rebon_group::identity::detect(),
        &who,
        name,
        arguments,
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Runs the group tool `name` for the session `who` names, or else the one
/// the environment named (`detected`).
///
/// The gate is the MCP server's (`GroupDesk::offered`) less one rule: a
/// Rebon session is let through. Its own `groups` plugin is why the server
/// offers it nothing, but a Rebon session whose turns run on another agent
/// CLI (ACP) has no plugin tools in that agent's hands, and this command,
/// run from the agent's shell, is how it reaches its group as itself.
fn group_call(
    store: &rebon_group::GroupStore,
    root: &std::path::Path,
    detected: Option<rebon_group::Caller>,
    who: &WhoArgs,
    name: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let mut caller = who.caller().or(detected);
    if let Some(caller) = caller.as_ref() {
        if !store
            .agent_policy()
            .is_ok_and(|policy| policy.allows(&caller.agent))
        {
            anyhow::bail!(
                "{} is switched out of agent groups (Rebon desktop: Settings > Groups), or the switch file cannot be read",
                caller.agent
            );
        }
    }
    let root = root.to_string_lossy();
    rebon_group::tools::call(
        &mut rebon_group::tools::ToolContext {
            store,
            caller: &mut caller,
            root: &root,
        },
        name,
        arguments,
    )
}

fn run_hook(args: &HookArgs) -> anyhow::Result<()> {
    use std::io::Read;
    // A hook that errors shows up in the agent's session; one that has
    // nothing to say must say nothing, whatever went wrong.
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Ok(input) = serde_json::from_str::<serde_json::Value>(&input) else {
        return Ok(());
    };
    let store = rebon_group::GroupStore::new(rebon_group::default_root(
        &rebon_session::default_config_home_dir(),
    ));
    if let Some(output) = rebon_group::deliver::hook_output(&store, &args.agent, &input) {
        println!("{output}");
    }
    Ok(())
}

fn run_skill(args: &SkillArgs) -> anyhow::Result<()> {
    let text = rebon_group::skill::skill_md(&args.command);
    if args.write.is_empty() {
        print!("{text}");
        return Ok(());
    }
    for dir in &args.write {
        let path = write_skill(dir, &text)?;
        println!("{}", path.display());
    }
    Ok(())
}

/// Writes `text` as `<dir>/rebon-groups/SKILL.md`, replacing only a copy
/// this command wrote before.
fn write_skill(dir: &std::path::Path, text: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join(rebon_group::skill::NAME).join("SKILL.md");
    match std::fs::read_to_string(&path) {
        Ok(existing) if !rebon_group::skill::is_ours(&existing) => anyhow::bail!(
            "{} is someone else's skill; it was left alone",
            path.display()
        ),
        Ok(existing) if existing == text => return Ok(path),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    }
    let parent = path.parent().expect("the skill path has a directory");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod group_cli_tests {
    use super::*;
    use clap::Parser;
    use rebon_group::{Caller, GroupStore};
    use serde_json::json;

    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: GroupCommand,
    }

    fn parse(args: &[&str]) -> Result<GroupCommand, clap::Error> {
        Cli::try_parse_from(std::iter::once("group").chain(args.iter().copied()))
            .map(|cli| cli.command)
    }

    fn caller(agent: &str, session: &str) -> Caller {
        Caller {
            agent: agent.to_string(),
            session_id: session.to_string(),
        }
    }

    #[test]
    fn every_command_the_skill_shows_parses() {
        // The skill's table, with its placeholders filled in.
        for line in [
            "info",
            "join review --alias pi-1 --role \"reads the diff\" --no-create",
            "inbox --full --peek",
            "send all hello --kind request",
            "send pi-1 done --kind reply --re r12",
            "remember \"tabs, not spaces\" --supersedes 3",
            "recall tabs",
            "recall",
            "leave",
            "info --agent pi --session-id abc",
        ] {
            let words: Vec<String> = shell_words(line);
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            parse(&words).unwrap_or_else(|error| panic!("`{line}`: {error}"));
        }
    }

    #[test]
    fn an_identity_needs_both_halves_and_a_bad_kind_is_refused() {
        assert!(parse(&["info", "--agent", "pi"]).is_err());
        assert!(parse(&["info", "--session-id", "abc"]).is_err());
        assert!(parse(&["send", "all", "hi", "--kind", "shout"]).is_err());
        assert!(parse(&["remember", "x", "--supersedes", "0"]).is_err());
    }

    #[test]
    fn a_session_the_environment_does_not_name_joins_and_talks_as_the_one_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path().join("groups"));
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let pi = WhoArgs {
            agent: Some("Pi".into()),
            session_id: Some("s-1".into()),
        };

        let unknown = group_call(
            &store,
            &project,
            None,
            &WhoArgs::default(),
            rebon_group::tools::GROUP_SEND,
            json!({ "to": "all", "text": "hi" }),
        );
        assert!(unknown.unwrap_err().to_string().contains("cannot tell"));

        group_call(
            &store,
            &project,
            None,
            &pi,
            rebon_group::tools::GROUP_JOIN,
            json!({ "group": "review", "create": true }),
        )
        .unwrap();
        let claude = caller("claude-code", "c-1");
        group_call(
            &store,
            &project,
            Some(claude.clone()),
            &WhoArgs::default(),
            rebon_group::tools::GROUP_JOIN,
            json!({ "group": "review", "create": false }),
        )
        .unwrap();
        group_call(
            &store,
            &project,
            None,
            &pi,
            rebon_group::tools::GROUP_SEND,
            json!({ "to": "all", "text": "the diff is ready" }),
        )
        .unwrap();

        let inbox = group_call(
            &store,
            &project,
            Some(claude),
            &WhoArgs::default(),
            rebon_group::tools::GROUP_INBOX,
            json!({ "full": true, "peek": false }),
        )
        .unwrap();
        assert!(inbox.to_string().contains("the diff is ready"), "{inbox}");
    }

    #[test]
    fn the_flags_win_over_an_environment_that_names_an_outer_session() {
        // pi over ACP inside a Rebon session: the shell carries the Rebon
        // session's variable too, and detection picks it.
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path().join("groups"));
        let project = dir.path().to_path_buf();
        let rebon = Some(caller("rebon", "r-1"));
        let pi = WhoArgs {
            agent: Some("pi".into()),
            session_id: Some("p-1".into()),
        };
        group_call(
            &store,
            &project,
            rebon.clone(),
            &pi,
            rebon_group::tools::GROUP_JOIN,
            json!({ "group": "review" }),
        )
        .unwrap();

        let as_pi = group_call(
            &store,
            &project,
            rebon.clone(),
            &pi,
            rebon_group::tools::GROUP_INFO,
            json!({}),
        )
        .unwrap();
        let as_detected = group_call(
            &store,
            &project,
            rebon,
            &WhoArgs::default(),
            rebon_group::tools::GROUP_INFO,
            json!({}),
        )
        .unwrap();
        assert_ne!(as_pi, as_detected);
        assert!(store
            .group_of(&caller("pi", "p-1").key())
            .unwrap()
            .is_some());
        assert!(store
            .group_of(&caller("rebon", "r-1").key())
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_rebon_session_gets_through_and_a_switched_out_agent_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path().join("groups"));
        let project = dir.path().to_path_buf();
        group_call(
            &store,
            &project,
            Some(caller("rebon", "r-1")),
            &WhoArgs::default(),
            rebon_group::tools::GROUP_JOIN,
            json!({ "group": "review" }),
        )
        .unwrap();

        store.set_agent_allowed("pi", false).unwrap();
        let refused = group_call(
            &store,
            &project,
            Some(caller("pi", "p-1")),
            &WhoArgs::default(),
            rebon_group::tools::GROUP_INFO,
            json!({}),
        );
        assert!(refused.unwrap_err().to_string().contains("switched out"));
    }

    #[test]
    fn the_skill_is_written_once_updated_in_place_and_never_over_a_users_own() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".agents/skills");
        let path = write_skill(&skills, &rebon_group::skill::skill_md("rebon")).unwrap();
        assert_eq!(path, skills.join("rebon-groups/SKILL.md"));

        let newer = rebon_group::skill::skill_md("/opt/rebon/bin/rebon");
        write_skill(&skills, &newer).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);

        std::fs::write(&path, "---\nname: my-own\n---\nmine\n").unwrap();
        assert!(write_skill(&skills, &newer).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "---\nname: my-own\n---\nmine\n"
        );
    }

    /// Splits a line the way a shell would, for the few quoting forms the
    /// table above uses.
    fn shell_words(line: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quoted = false;
        for c in line.chars() {
            match c {
                '"' => quoted = !quoted,
                ' ' if !quoted => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                c => word.push(c),
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
        words
    }
}
