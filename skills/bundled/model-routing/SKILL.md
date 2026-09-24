---
name: model-routing
description: Set up or tune Rebon's automatic model routing (plugins.model-routing) and turn the user's providers and preferences into a routing policy the router can act on. Use when the user wants tasks routed to different models automatically, picks Jev or TypeSafe System One (directly or through Vercel AI Gateway) or another classifier model, mentions a router model, 分流, 自动路由 or 路由规则, or asks why routing was skipped.
when_to_use: Use when the user wants automatic model routing set up, changed or debugged, including choosing Jev / TypeSafe or a cheap router model and writing the routing policy.
argument-hint: "[what should go where, or the routing error you saw]"
user-invocable: true
allowed-tools:
  - Read
  - Edit
  - Write
  - Glob
  - Grep
  - Bash
  - PowerShell
  - AskUserQuestion
---

# Model routing setup

Rebon's router looks at the **first real prompt of a new session** and may move
the session to another provider, model and reasoning effort before any work
starts. Each new sub-agent that does not declare its own model is routed once
the same way. Routing never runs again in that session, a resumed session with
history is never routed, and `/model` or `/effort` afterwards always win.

The whole decision has 10 seconds. A failure never blocks the turn: the user
sees `Experimental model routing skipped: <reason>` and the turn runs on the
model it started on. A success shows
`Auto switched to <provider> / <model> [with <effort> effort]; continuing the task.`

Your job: find out what the user has, ask what they want, write a valid
`plugins.model-routing` block, and tell them how to check it. Work through the
steps below in order; skip questions the user already answered.

## 1. Read the current state

Never print, copy or ask for an API key. Check that a key exists, not what it is.

- **Config home**: `$REBON_CONFIG_DIR`, else `~/.rebon`.
- **Routing settings** live under `plugins.model-routing` in, from lowest to
  highest precedence: `<home>/settings.json`, `<cwd>/.rebon/settings.json`,
  `<cwd>/.rebon/settings.local.json`. A later file overrides an earlier one key
  by key. `/settings` writes the user file.
- **Providers** are `<home>/providers/<id>.json`, one file per provider; the id
  is the file name without `.json`. Without that directory, they are the
  `customProviders` array in `<home>/config.json`. The provider new sessions
  start on is `activeCustomProvider` in `config.json`. From each provider read
  only `format`, `baseUrl`, `models` and `modelProfiles` — never `apiKey`.
- **What the router may choose**: every configured provider, and for each one
  the ids in its `models` plus every model its `modelProfiles` point to. A model
  that is in neither cannot be picked, so add it to `models` if the user wants
  it routable. Profile names (`small`, `general`, `reasoning`, …) also resolve,
  within that provider.
- **Keys** must be real environment variables of the process that runs Rebon.
  Rebon does not read `.env` files, and key-like names in `settings.json` are
  ignored. Check presence only:
  - PowerShell: `[bool]$env:AI_GATEWAY_API_KEY`, and for a persisted user
    variable `[bool][Environment]::GetEnvironmentVariable('AI_GATEWAY_API_KEY','User')`
  - sh: `[ -n "$AI_GATEWAY_API_KEY" ] && echo set || echo missing`
  - The same for `TYPESAFE_API_KEY`, and the `REBON_`-prefixed variants, which
    take precedence.
- **What happened before**: search `<home>/projects/*/*.jsonl` for
  `Experimental model routing skipped` and `Auto switched to`. The reason after
  "skipped:" usually names the fix (see step 5).

## 2. Ask what the user wants

Use one AskUserQuestion with only the questions still open.

**Classifier backend**, the model that makes the decision:

| Choice | Settings | Key | Notes |
|---|---|---|---|
| Jev through Vercel AI Gateway | `backend: "typesafe"`, `classifierModel: "typesafe-ai/jev"`, `classifierEndpoint: "https://ai-gateway.vercel.sh/typesafe/v1/systemone"` | `AI_GATEWAY_API_KEY` | Sees each candidate's price, context window and accepted efforts. |
| TypeSafe System One direct | `backend: "typesafe"` (leave `classifierModel` and `classifierEndpoint` out for `jev-latest` on `https://api.typesafe.ai/v1/systemone`) | `TYPESAFE_API_KEY` | Same as above. |
| A cheap model of your own providers | `backend: "prompt"` (the default), `routerModel: "<model or profile>"` | none | Nothing leaves your providers. Gets no price data, so "cheapest" rests on the model's own knowledge. |

- `routerModel` must belong to the provider the session **starts** on, or
  routing is skipped. Prefer a profile name every provider defines (usually
  `small`) over a model id, so it resolves wherever the session starts.
- The TypeSafe backends send the first 32,000 characters of the first prompt,
  the working directory path, the current provider/model, every candidate
  provider/model and the policy to that endpoint. Say so before choosing one.
- A key only matters for its own endpoint: the Vercel endpoint reads
  `AI_GATEWAY_API_KEY`, any other endpoint reads `TYPESAFE_API_KEY`.

**Where work should go.** Ask which providers are allowed, which kinds of task
deserve a stronger or pricier model, and what costs matter: a subscription
quota (for example the ChatGPT Codex route) or pay-per-token. When the user has
no view, propose tiers from what they have: quick questions and routine edits
on the cheapest capable model, planning and hard debugging on the strongest
model they are willing to spend on.

## 3. Write the policy

`policy` is free text given to the classifier. It outranks the router's
built-in preference for the cheapest model that fits. Write it so each decision
it leads to is valid:

1. **Open with an allow-list**: `Only route to provider "a" or provider "b"; never pick c or d.`
   Every configured provider is a candidate otherwise, including relays the user
   never uses.
2. **Name targets exactly as the candidates spell them**: the provider id
   (the file name) and a model id from that provider's `models`, or one of its
   profile names.
3. **Use only efforts the model accepts**, from `low`, `medium`, `high`,
   `xhigh`, `max`. An effort the model does not take fails the whole decision.
   Common cases:
   - DeepSeek (`deepseek-flash`, `deepseek-v4-pro`): `low`, `high`, `max` only;
     no `medium`, no `xhigh`.
   - OpenAI GPT-6 / GPT-5.6: `low` through `max`. GPT-5.5 and GPT-5.4 mini:
     `low` through `xhigh`.
   - Claude Sonnet 5 / Opus 4.8: `low` through `max`.
   - A model with no row in Rebon's model table accepts any effort.
   When unsure, leave the effort out of that rule.
4. **Rules by the kind of task, as the first prompt shows it**: planning,
   hard debugging, routine edits, quick questions. The classifier sees only the
   first prompt, not the repository.
5. **Number the rules, end with a catch-all, and state precedence**:
   `Rule 1 outranks rule 2, …`.
6. **Give the cost facts that should steer it**, for example that one provider
   is a metered subscription, or that one model costs five times another.

Template:

```
Only route to provider "<cheap>" or provider "<strong>"; never pick <others>.
<strong> is a metered subscription, so keep it for work that needs it.
1. Hard work: multi-step debugging, subtle correctness, concurrency or security
   reasoning, cross-cutting design, or anything whose wrong first answer costs a
   rewrite: provider "<strong>", model "<model>", reasoningEffort "<effort>".
2. Planning: a plan, design or breakdown of work before code:
   provider "<strong>", model "<model>", reasoningEffort "<effort>".
3. Routine engineering: writing, editing or refactoring code, fixing a known bug,
   tests, docs, git chores: provider "<cheap>", model "<model>", reasoningEffort "<effort>".
4. Quick questions, explanations, lookups, one-line changes:
   provider "<cheap>", model "<model>", reasoningEffort "low".
Rule 1 outranks rule 2, rule 2 outranks rule 3, rule 3 outranks rule 4.
```

Write the policy as one JSON string (escape inner quotes as `\"`).

## 4. Apply it

Show the user the block you will write, then edit the chosen settings file —
the user file unless they want this for one project. Change only
`plugins.model-routing`, keep every other key, and confirm the file still
parses as JSON afterwards. Back it up first if it has content.

```json
{
  "plugins": {
    "model-routing": {
      "enabled": true,
      "backend": "typesafe",
      "classifierModel": "typesafe-ai/jev",
      "classifierEndpoint": "https://ai-gateway.vercel.sh/typesafe/v1/systemone",
      "routerModel": "small",
      "policy": "Only route to provider \"deepseek\" or provider \"openai\"; ..."
    }
  }
}
```

Checks `/settings` would make, which a direct edit must make itself:

- `enabled`: `true`. The plugin is off by default.
- `backend`: `"prompt"` or `"typesafe"` (`"jev"` is accepted as an alias).
  Any other value makes every routing attempt fail.
- `routerModel`: required by the prompt backend; a model or profile of the
  provider sessions start on. Keeping `"small"` here is harmless with TypeSafe
  and makes a later switch to the prompt backend work.
- `classifierModel`: a non-empty string, or leave the key out for `jev-latest`.
- `classifierEndpoint`: an `https://` URL, or leave the key out for TypeSafe's
  own endpoint.
- `policy`: non-empty text, or remove the key. Never an empty string.
- Never write an API key into any settings file.

**Keys**: the user sets them, not you, and never in the chat. Tell them the
command to run in their own terminal, then to open a new terminal so Rebon
inherits it:

- Windows: `setx AI_GATEWAY_API_KEY "<key>"` (or `TYPESAFE_API_KEY`)
- macOS / Linux: add `export AI_GATEWAY_API_KEY=<key>` to the shell profile

**When it takes effect**: setting changes apply to the next new session's first
prompt; nothing needs a restart. Turning the plugin on for the first time needs
`/kernel enable model-routing` or a restart. The current session has already
had its one routing attempt.

## 5. Check it

Have the user start a new session (`/new`) and send a first prompt of each tier,
one per session, and look for the notice.

| Notice after "skipped:" | Fix |
|---|---|
| `routerModel must belong to the current provider` | Use a profile name that exists in every provider (`small`), or switch to the TypeSafe backend. |
| `plugins.model-routing.routerModel must be a non-empty string` | Prompt backend without `routerModel`: set one, or use TypeSafe. |
| `set AI_GATEWAY_API_KEY (or REBON_AI_GATEWAY_API_KEY) …` / `set TYPESAFE_API_KEY (or REBON_TYPESAFE_API_KEY) …` | The key is not in Rebon's environment. Set it, then start Rebon from a new terminal. |
| `unknown routing provider` / `unknown routing model` | The policy names a provider or model that is not a candidate. Fix the spelling, or add the model to that provider's `models`. |
| `switching provider requires a model` | Every rule that moves provider must name a model. |
| `reasoning effort is not supported by selected model` / `unknown reasoning effort` | A rule gives an effort the model does not take (step 3). |
| `plugins.model-routing.backend is …` / `classifierEndpoint must be an HTTPS URL` / `classifierModel must be a non-empty string` | Fix that value (step 4). |
| `the catalogue offers N provider/model pairs, more than the 254 …` | TypeSafe offers at most 254 pairs: trim unused providers' `models`. |
| `model routing timed out` | The classifier took over 10 s. Retry in a new session; if it persists, use the other backend. |
| `provider cannot isolate a routing request` | The prompt backend cannot run on this provider; use TypeSafe. |

If nothing appears at all, the session was not eligible: it was resumed with
history, the plugin is not loaded (`/kernel enable model-routing`), or the first
message was empty or an image only.
