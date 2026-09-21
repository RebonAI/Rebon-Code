---
name: stuck
description: "Investigate frozen/stuck/slow Rebon sessions on this machine and prepare a diagnostic report."
user-invocable: true
---

# /stuck — diagnose frozen/slow Rebon sessions

The user thinks another Rebon session on this machine is frozen, stuck, or very slow. Investigate and report the findings back to the user.

## What to look for

Scan for other Rebon processes (excluding the current one — PID is in `process.pid` but for shell commands just exclude the PID you see running this prompt). Process names may be `rebon`, `claude`, or local native dev binaries such as `cli`.

Signs of a stuck session:
- **High CPU (≥90%) sustained** — likely an infinite loop. Sample twice, 1-2s apart, to confirm it's not a transient spike.
- **Process state `D` (uninterruptible sleep)** — often an I/O hang. The `state` column in `ps` output; first character matters (ignore modifiers like `+`, `s`, `<`).
- **Process state `T` (stopped)** — user probably hit Ctrl+Z by accident.
- **Process state `Z` (zombie)** — parent isn't reaping.
- **Very high RSS (≥4GB)** — possible memory leak making the session sluggish.
- **Stuck child process** — a hung `git`, `node`, or shell subprocess can freeze the parent. Check `pgrep -lP <pid>` for each session.

## Investigation steps

1. **List all Rebon processes** (macOS/Linux):
   ```
   ps -axo pid=,pcpu=,rss=,etime=,state=,comm=,command= | grep -E '(rebon|claude|cli)' | grep -v grep
   ```
   Filter to rows that look like active Rebon sessions or local Rebon development binaries.

2. **For anything suspicious**, gather more context:
   - Child processes: `pgrep -lP <pid>`
   - If high CPU: sample again after 1-2s to confirm it's sustained
   - If a child looks hung (e.g., a git command), note its full command line with `ps -p <child_pid> -o command=`
   - Check relevant Rebon debug/session logs if you can infer their location from the process environment or working directory. Capture only the recent tail needed to diagnose the hang.

3. **Consider a stack dump** for a truly frozen process (advanced, optional):
   - macOS: `sample <pid> 3` gives a 3-second native stack sample
   - This is big — only grab it if the process is clearly hung and you want to know *why*

## Report

If every session looks healthy, tell the user that directly.

If you did find a stuck/slow session, format a concise diagnostic report for the user. Include:
- PID, CPU%, RSS, state, uptime, command line, child processes
- Your diagnosis of what's likely wrong
- Relevant debug log tail or `sample` output if you captured it
- Suggested next steps, but do not kill or signal any processes unless the user explicitly asks

## Notes
- Don't kill or signal any processes — this is diagnostic only.
- If the user gave an argument (e.g., a specific PID or symptom), focus there first.
