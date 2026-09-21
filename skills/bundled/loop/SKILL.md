---
name: loop
description: Schedule a prompt to repeat on an interval and run it once immediately.
when_to_use: Use when the user asks to keep doing something periodically, such as checking every few minutes or running a recurring watch loop.
argument-hint: "[interval] <prompt>"
user-invocable: true
---

# Loop

Schedule the user's request as a recurring cron task, then perform the request once immediately.

## Parse the interval

Default interval: `10m`.

Support both forms:
- Leading interval token: `5m check CI`
- Trailing phrase: `check CI every 5 minutes`

Recognize these units:
- minutes: `m`, `min`, `mins`, `minute`, `minutes`
- hours: `h`, `hr`, `hrs`, `hour`, `hours`
- days: `d`, `day`, `days`

If no interval is present, use `10m`. The remaining text is the prompt to run. If the remaining prompt is empty, ask the user what to run.

## Convert interval to cron

Use local time cron expressions:
- N minutes, where 1 <= N <= 59: `*/N * * * *`
- N hours: `0 */N * * *`
- N days: `0 9 */N * *`

If the interval cannot be represented cleanly as one of these cron expressions, ask the user for a cron expression or a simpler interval.

## Schedule it

Call `CronCreate` with:
- `cron`: the cron expression
- `prompt`: the parsed prompt
- `recurring`: `true`

Do not set `durable` unless the user explicitly asks for the loop to persist across restarts; the default is session-only.

If `CronCreate` is not loaded, first call `ToolSearch` with query `select:CronCreate`, then call `CronCreate`.

## Run once immediately

After `CronCreate` succeeds, immediately execute the parsed prompt once in this same turn. Do not wait for the first scheduled fire.

Tell the user the schedule and job id after scheduling, then continue with the immediate execution result.
