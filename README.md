<table>
  <tr>
    <td width="150" align="center" valign="middle">
      <img src="assets/app-icon/icon.png" width="128" alt="Rebon">
    </td>
    <td valign="middle">
      <h1>rebon</h1>
      <p>
        An agent CLI for coding and more — a terminal harness that drives an
        LLM through a real agent loop with tools, sessions, permissions, and a
        full TUI.
      </p>
      <p>
        <a href="https://www.npmjs.com/package/@rebon/cli"><img src="https://img.shields.io/npm/v/@rebon/cli?label=%40rebon%2Fcli" alt="npm"></a>
        <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="Apache-2.0"></a>
        ·
        <a href="docs/README.zh-CN.md">中文</a>
      </p>
    </td>
  </tr>
</table>

## Demo

The core loop — slash-command palette, `@`-file completion, a tool call with its
permission prompt, and a follow-up that keeps the earlier context:

![Rebon core loop](assets/demo/core-loop.webp)

Permission modes — `shift+tab` cycles default, plan, accept-edits and auto; the
task then runs unattended under the auto-mode classifier:

![Rebon permission modes](assets/demo/modes-auto.webp)

`/rewind` — pick a restore point and roll the code and the conversation back
together:

![Rebon rewind](assets/demo/rewind.webp)

## What is in this repository

The `rebon` command-line agent and everything it needs to build: the crates, the
plugins, the packaging chain and the release workflow.

Rebon's other surfaces ship on their own schedules and are **not** in this tree.
Comments here name them because the code they share was shaped by them:

| Surface | Where it lives |
| --- | --- |
| Desktop app | <https://reboncode.ai> |
| Mobile client | <https://reboncode.ai> |
| Web UI (`rebon serve`) | the `@rebon/rebon-web` npm package |

`assets/` holds the artefacts the CLI reads from those surfaces — the Windows
icon, the generated i18n catalogues, and a committed build of the web UI that
`build.rs` embeds so `cargo build` never needs Node.

### About the tags in this repository

This repository was previously where Rebon's releases were published, so its
refs carry a large number of `app-v*` tags from the desktop app. They are kept
deliberately: they are where those releases live, and deleting them would
break the links pointing at them.

They have nothing to do with the CLI. The CLI's own releases are the `v*`
tags, and those are what the release workflow builds and publishes from.

## Install

```bash
npm install -g @rebon/cli
```

Upgrade later:

```bash
npm install -g @rebon/cli@latest
```

`@rebon/cli` is a thin launcher that pulls the matching native binary as
an optional dependency. Supported platforms: `win32-x64`, `darwin-x64`,
`darwin-arm64`, `linux-x64`, `linux-arm64`.

> If install fails with "missing optional platform package", you likely passed
> `--omit=optional` or `--no-optional`. Reinstall without that flag.

## Quick start

```bash
# Local TUI in the current directory
rebon

# Resume a previous session
rebon --resume k7m2q-4xr9t-hb3wz-p8ncv

# Override the active provider / model
rebon --provider openrouter --model gpt-5.5

# Run as an ACP JSON-RPC server on stdio (for editor / IDE integrations)
rebon --acp

# Work on a project that lives on another machine
rebon remote add prod deploy@build.example --path /srv/app
rebon --remote prod
```

The first run drops you into onboarding: pick a provider, sign in (OAuth +
PKCE for Claude / OpenAI, or paste an API key), and you're in.

## CLI flags

| Flag                | Purpose                                                      |
| ------------------- | ------------------------------------------------------------ |
| `--acp`             | Run as an ACP JSON-RPC server on stdio.                      |
| `--provider <name>` | Override `activeCustomProvider` from `~/.rebon/config.json`. |
| `--model <id>`      | Override the resolved provider's default model.              |
| `--resume <id>`     | Load an on-disk transcript and replay it into the TUI.       |
| `--remote <name>`   | Run the session on a configured remote host over ssh.        |
| `--remote-path <p>` | Project directory on that remote. Requires `--remote`.       |

## Config & data

- `~/.rebon/config.json` — providers, models, credentials, defaults.
- `~/.rebon/sessions/` — saved transcripts.
- `~/.rebon/skills/`, `~/.rebon/agents/`, `~/.rebon/memory/` — user assets.
- `$REBON_LOG_DIR/rebon.log` — TUI log file (defaults to
  `%TEMP%/rebon/logs/rebon.log` on Windows, `$TMPDIR/rebon/logs/rebon.log`
  elsewhere). `--acp` mode logs to stderr instead.

Project-level overrides live under `.rebon/` and `.claude/` in your working
directory.

## What you get

- **Local TUI** — prompt input with history, paste, image paste, `@`-mention
  and slash-command pickers, queued submissions, mode cycling; streaming
  transcript with markdown, tool grouping, thinking blocks, plan approval,
  permission modal; full dialog stack for onboarding, settings, resume,
  rewind, quick-open, history search, global search, tasks, background
  tasks, teams, agents.
- **Agentic tool loop** — Bash / PowerShell, Read / Write / Edit, Glob /
  Grep, Sleep, TaskCreate / TaskUpdate / TaskList / TaskGet / TaskStop,
  Agent, SkillTool, AskUserQuestion, SendMessage, EnterPlanMode /
  ExitPlanMode, ToolSearch, Team{Create,Delete,Files,Mailbox,Manager},
  Worktree, plus MCP tools (stdio, Streamable HTTP, legacy SSE).
- **Providers** — Anthropic, OpenAI, and OpenAI-Responses, with streaming,
  compact / context-prune passes, and automatic session titles.
- **Permissions, hooks, sandbox** — fine-grained allow / deny for shell,
  filesystem, web-fetch, and skill invocations; user-defined hooks; sandbox
  config with violation reporting.
- **Skills, agents, memory** — bundled and user skills; spawnable
  worker agents and a coordinator for background tasks; a persistent memory
  layer with recall and surfacing.
- **Remote hosts** — run a session against a project on another
  machine over ssh. A server build is installed on the far end and the
  agent executes there — shell, filesystem, git — while this machine
  keeps the UI and the transcript. `rebon remote add`, then
  `rebon --remote <name>`.
- **ACP server** — drive Rebon from an editor over stdio JSON-RPC:
  `initialize`, `session/new`, `session/load`, `session/list`,
  `session/prompt`, `session/cancel`, reverse-RPC permission prompts,
  streamed tool output.

## Common tasks

```bash
# Start a fresh session in the current repo
rebon

# Resume the last session you worked on (use Tab in the TUI to browse)
rebon --resume <session-id>

# Plug Rebon into an editor over ACP (the editor manages stdio for you)
rebon --acp

# Override provider/model without editing config
rebon --provider anthropic --model claude-opus-4-7
```

Inside the TUI:

- `?` — keyboard help
- `/` — slash-command picker
- `@` — file / symbol mention
- `Tab` — switch panel / list
- `Esc` — cancel current step or dismiss a dialog

## Updating

`rebon` ships with a built-in updater that checks for new releases on launch.
You can also just rerun:

```bash
npm install -g @rebon/cli@latest
```

## Privacy

Rebon has no telemetry — no analytics, no crash reporting, no usage events,
no anonymous install ID. The only request it makes on its own is the update
check above: a plain `GET` for `@rebon/cli`'s public npm metadata, carrying
no identifiers and not even your current version. Everything else on the
wire is a request you made — a model turn, a `WebFetch`, an MCP call.

[PRIVACY.md](PRIVACY.md) lists every outbound request the binary can make,
with the file each one lives in, plus the commands to verify it yourself.

## Troubleshooting

- **"missing optional platform package"** — reinstall without
  `--omit=optional` / `--no-optional` / `--ignore-optional`.
- **"unsupported platform"** — your `process.platform`/`process.arch` is not
  one of the five supported targets.
- **TUI looks scrambled** — make sure your terminal supports truecolor and
  Unicode width tables (modern Windows Terminal, iTerm2, Alacritty, WezTerm,
  Kitty all work). The legacy `cmd.exe` and `conhost` are not supported.
- **Logs** — check `$REBON_LOG_DIR/rebon.log` (TUI) or stderr (`--acp`).

## Friends

[Linux.Do](https://linux.do) — A new ideal community

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Redistributions must carry the licence and the attribution notice in
[NOTICE](NOTICE), and modified files must say that they were changed.

Third-party components and their licenses are listed in
[THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt).
