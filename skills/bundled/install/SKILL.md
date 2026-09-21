---
name: install
description: Help find, review, and install external skills into Rebon.
when_to_use: Use when the user wants to discover, fetch, review, install, or migrate an external skill or slash-command into Rebon. Examples: "install this skill", "add a Claude skill", "find a skill for code review".
argument-hint: "[skill source or search terms]"
user-invocable: true
allowed-tools:
  - WebSearch
  - WebFetch
  - Bash
  - Read
  - Write
  - Edit
  - Glob
---

# Install External Skills

Help the user discover, review, and install external skills. Do not silently install anything.

## Default destinations

Use Rebon's native skill directories by default:
- User-wide: `~/.rebon/skills/<skill-name>/SKILL.md`
- Project-local: `<cwd>/.rebon/skills/<skill-name>/SKILL.md`

Rebon also reads compatibility skill directories, including `.claude/skills` and `.codex/skills` at the user and project levels. Prefer installing new skills into `~/.rebon/skills` unless the user explicitly asks for a compatibility directory.

## Steps

### 1. Identify the requested skill

If the user supplied a URL, repository path, local path, or search terms, use that as the source. If the source is ambiguous, ask the user what skill they want and where it should come from.

**Success criteria**: You know the candidate skill source or have asked the user for it.

### 2. Fetch or inspect the candidate

Use the least invasive available tool:
- For web discovery, use `WebSearch` if available and permitted.
- For a URL, use `WebFetch` if available and permitted.
- For local files or repositories, use `Glob` and `Read`.
- If a repository provides an install script, inspect it before running anything and ask the user before executing it.

**Success criteria**: You have reviewed the candidate `SKILL.md` content or know why it cannot be inspected.

### 3. Review safety and fit

Check the skill name, description, instructions, requested tools, and any bundled files. Explain notable permissions, network access, shell commands, or local writes. If the skill is not in `skill-name/SKILL.md` format, adapt it to that format instead of introducing a new parser or layout.

**Success criteria**: The user has enough information to decide whether to install it.

### 4. Confirm destination before writing

Recommend `~/.rebon/skills/<skill-name>/SKILL.md` for personal skills unless the user asks for project-local install or compatibility with `.claude/skills` or `.codex/skills`. Ask for confirmation before writing to any local skill directory.

**Success criteria**: The user has approved the target path and content.

### 5. Install and verify

Create the target directory if needed, write `SKILL.md`, and copy any required adjacent files only after approval. Then read back the installed file and report how to invoke it, such as `/skill-name`.

**Success criteria**: The skill exists at the approved path and the user knows how to invoke it.

## Rules

- Do not actually install remote content until the user approves the reviewed content and destination.
- Do not write to `~/.rebon`, `~/.claude`, or `~/.codex` without the normal tool permission flow and user approval.
- Do not modify unrelated files or settings.
- Do not add a new Rust tool or parser for installation.
