---
name: loop
description: Repeat a prompt — on a fixed interval, or self-paced when no interval is given — and run it once immediately.
when_to_use: Use when the user asks to keep doing something periodically or until it is done, such as checking every few minutes, watching CI, or babysitting a long task.
argument-hint: "[interval] <prompt>"
user-invocable: true
---

# Loop

Run the user's request now, then keep running it: on a fixed schedule when the user gave an interval, or at a pace you choose each turn when they did not.

## Parse the interval

Support both forms:
- Leading interval token: `5m check CI`
- Trailing phrase: `check CI every 5 minutes`

Recognize these units:
- minutes: `m`, `min`, `mins`, `minute`, `minutes`
- hours: `h`, `hr`, `hrs`, `hour`, `hours`
- days: `d`, `day`, `days`

The remaining text is the prompt to run. If the remaining prompt is empty, ask the user what to run.

With an interval, follow **Fixed interval**. Without one, follow **Self-paced**.

## Fixed interval

Convert the interval to a local time cron expression:
- N minutes, where 1 <= N <= 59: `*/N * * * *`
- N hours: `0 */N * * *`
- N days: `0 9 */N * *`

If the interval cannot be represented cleanly as one of these cron expressions, ask the user for a cron expression or a simpler interval.

Call `CronCreate` with:
- `cron`: the cron expression
- `prompt`: the parsed prompt
- `recurring`: `true`

Do not set `durable` unless the user explicitly asks for the loop to persist across restarts; the default is session-only.

If `CronCreate` is not loaded, first call `ToolSearch` with query `select:CronCreate`, then call `CronCreate`.

After `CronCreate` succeeds, immediately execute the parsed prompt once in this same turn. Do not wait for the first scheduled fire. Tell the user the schedule and job id, then continue with the result.

## Self-paced

Immediately execute the parsed prompt once in this same turn.

At the end of every iteration, decide whether the loop goes on:
- **Done** — the task is finished, the thing being watched settled, or the user asked to stop: call `ScheduleWakeup` with `stop: true` and tell the user the loop ended and why.
- **Waiting on work you started** — a background agent or shell is still running: do not schedule a wakeup; its completion wakes you, and you continue the loop then.
- **Otherwise** — call `ScheduleWakeup` with:
  - `delaySeconds`: matched to what you are waiting on (a CI run of ~8 minutes deserves one check at ~480, not eight at 60). With nothing specific to watch, use 1200–1800.
  - `prompt`: `Continue the self-paced /loop: <the parsed prompt>` — the same text every iteration.
  - `reason`: one short sentence on why this delay, such as `watching the CI run`.

If `ScheduleWakeup` is not loaded, first call `ToolSearch` with query `select:ScheduleWakeup`.

A turn that starts with `Continue the self-paced /loop:` is the next iteration: run the prompt, then decide again as above. Keep each iteration's report short; say only what changed.
