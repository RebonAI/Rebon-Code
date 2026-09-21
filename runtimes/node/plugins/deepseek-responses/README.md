# deepseek-responses

DeepSeek Responses API model provider plugin for Rebon. It runs on Rebon's
plugin plane — one `llm/stream` adapter in the shared Node host, registered by
`activate` — and owns the whole DeepSeek dialect: request trimming,
`reasoning_text` streaming, custom tool calls, web search events, error and
usage translation, so Rebon core never grows provider-specific branches.

## Install

Unzip if you received this as an archive, then install the directory
(`rebon plugin install` takes a local directory path, not a zip):

```sh
rebon plugin install ./deepseek-responses
```

Restart Rebon afterwards — plugin capabilities materialize at startup.
No `node` of your own is needed: the plugin runs in the Node host Rebon
resolves for itself (`rebon node install` provides one).

Compatibility: this version requires a Rebon build whose plugin plane serves
model providers. On an older host the manifest's `plugin` transport is not
recognised; use the 0.3.x line of this plugin there.

## Configure

Add a provider entry named after the plugin provider id and select it. The
entry's `apiKey`, `baseUrl`, `options.headers`, and request body options are
forwarded to the plugin with each turn:

```json
{
  "activeCustomProvider": "deepseek",
  "customProviders": [
    {
      "name": "deepseek",
      "apiKey": "$DEEPSEEK_API_KEY",
      "model": "deepseek-v4-pro"
    }
  ]
}
```

Without a configured entry the plugin falls back to the `DEEPSEEK_API_KEY`
and `DEEPSEEK_BASE_URL` environment variables (default base URL
`https://api.deepseek.com`; the plugin appends `/responses`).

## Minimal mode

When this provider is selected with Rebon's **Minimal** capability mode, it
uses Anchored Minimal rather than staying permanently constrained.

The bootstrap request reproduces the **DeepSeek Harness Minimal preset's**
model-facing surface rather than a Rebon-shaped small catalog, because the
upstream measurement that motivates this mode
([`xiaobright/dsh-anchored-standard`](https://github.com/xiaobright/dsh-anchored-standard),
issue #11) found the tool *schema identity* to be the decisive first-request
variable: that exact pair anchored 5/5 runs at the adapter-default output
budget, while every standard-family schema (`pwsh`/`read`, `pwsh` only,
sandboxed `bash`/`read`) fell into standard-like behavior 11/11.

- The first model request sends the complete upstream Minimal persona
  ("You are a helpful software engineer assistant."), advertises exactly
  `bash` and `str_replace_editor` with the upstream descriptions and parameter
  schemas, and omits automatic workspace/memory/skill context. Its output
  budget is **not** capped by default.
- `bash` executes as Rebon's `Bash` (an alias), and `str_replace_editor`
  translates to Rebon's `Read`/`Write`/`Edit`, so permissions, the
  read-before-write file-state cache, and path scoping are unchanged.
- After the first persisted assistant response or tool call, the next request
  restores the normal runtime context and normal eager-tool projection while
  retaining the Minimal persona and the caller's original output budget.
- Deferred tools remain behind `ToolSearch` after promotion; the phase is
  derived from persisted conversation history, so reload, resume, and
  compacted histories stay promoted.

Two deliberate divergences from upstream: the promoted catalog is Rebon's
normal `ToolSearch`-gated projection rather than the upstream pair-plus-
discovery set, and object key order inside the advertised schemas follows
`serde_json` (alphabetical in the CLI build, declaration order in the desktop
build, which enables `preserve_order`). Everything the model reads — names,
descriptions, types, enums, and `required` — matches upstream.

`REBON_ANCHORED_BOOTSTRAP_MAX_TOKENS` is the opt-in equivalent of upstream's
`bootstrapMaxTokens`: set it to cap the bootstrap request's output budget (it
never raises a lower caller-supplied limit). Leave it unset unless you are
experimenting with a standard-family bootstrap schema — capping truncates the
first reply, and the pair anchors without it.

`minimalSystemPromptOverride` still wins over the anchored persona if you set
it.

The opt-in is declared by `anchoredMinimal` in `rebon-plugin.json`. The ready
report deliberately omits that newer field; Rebon takes the union of what the
manifest declares and what the adapter reports, so it reaches the same answer
either way.

The persona and both tool schemas are copied from DeepSeek Harness (MIT); see
`NOTICE` and `LICENSE-DEEPSEEK-HARNESS` next to this file.

## Seeded Standard experiment

The plugin can optionally prepend the Judy-style seeded conversation to every
interactive session request while removing the system instructions:

```text
user: how are you
reasoning: We need answer user greeting. Need just respond.
assistant: I’m doing well, thank you! How can I help you today?
```

Enable it on the provider entry with the plugin-only `rebonAnchoredPreset`
control:

```json
{
  "name": "deepseek",
  "apiKey": "$DEEPSEEK_API_KEY",
  "model": "deepseek-v4-pro",
  "options": {
    "extraBody": {
      "rebonAnchoredPreset": "seeded"
    }
  }
}
```

Restart Rebon and create a new session after changing the option. Use capability
mode **Minimal** to combine this prefix with Rebon's current Anchored Minimal
`Bash` / `Read` / `ToolSearch` bootstrap and subsequent promotion. The plugin
keeps the host-provided tool projection unchanged, so Normal mode instead uses
Normal tools from the first request.

The prefix is replayed on every stateless interactive request so resume/reload
retain it without plugin-local session state. Requests without a session prompt
cache key and compaction requests are left unchanged. The control field itself
is removed before the request is sent upstream. Set the value to `"off"` or
remove it to restore normal behavior.

This is an experimental wire-level compatibility test. The offline self-test
verifies the exact user/reasoning/assistant input shape, but acceptance of an
input `reasoning_text` item must still be confirmed against the selected
DeepSeek-compatible endpoint. An endpoint that rejects it will return an
`invalid_request_error`; disable the option rather than silently dropping the
reasoning seed.

## Models

Both hosted V4 models are exposed, each with a 1M context window and a 384K
output ceiling:

| id | display | notes |
|---|---|---|
| `deepseek-v4-pro` | DeepSeek V4 Pro | **default**; 1.6T MoE flagship, ~4× Flash's output price |
| `deepseek-v4-flash` | DeepSeek V4 Flash | cheap; also the `small` profile |

Pro is the default because it is the flagship and because the Anchored Minimal
measurements this plugin's bootstrap profile is built on were made on V4 Pro at
`reasoningEffort=max`. Pin Flash on the provider entry if you would rather not
pay Pro output rates by default:

```json
{ "name": "deepseek", "model": "deepseek-v4-flash" }
```

The `small` profile deliberately stays on Flash regardless of the default: it
is what context compaction uses, and compaction runs as a plain summarisation
request through this plugin (with `reasoning.effort: "none"` so the summary
skips thinking), not a server-side compact endpoint.

## Dialect decisions

Matched to the published DeepSeek Responses compatibility list (the API is
stateless and silently ignores unsupported parameters):

- Never sent: `previous_response_id`, `service_tier`, `stop_sequences`,
  `metadata`. No WebSocket transport, no `/responses/compact`.
- `prompt_cache_key` forwards the host's stable per-session key by default:
  the official endpoint silently ignores it, while OpenAI-compatible
  gateways with keyed prefix caches (e.g. opencode Go) route and retain by
  it. Opt out with `options.omitBodyFields: ["prompt_cache_key"]`.
- Image input is replaced with placeholder text (the same thing DeepSeek
  does server-side) instead of failing the turn; file/document input in
  tool results gets the same treatment.
- The stable system prompt is sent as `instructions`; per-turn transient
  context uses Rebon's canonical runtime-context user message at the input
  tail, so changing runtime state does not invalidate the reusable prefix.
- Reasoning effort passes through natively (`low`/`medium`/`high`/`xhigh`/
  `max` are all accepted upstream); an explicit thinking-disabled request
  maps to `reasoning.effort: "none"`.
- Replayed thinking is sent back as a `reasoning` item **on turns that carry a
  tool call**, and dropped on plain turns. This is not an optimisation choice:
  in thinking mode the API rejects a tool-call turn whose reasoning was not
  replayed (`The `reasoning_text` in the thinking mode must be passed back to
  the API`), and the error that actually surfaces is the misleading
  `No tool output found for tool call <id>`. On plain turns the server ignores
  the replayed reasoning, so it is dropped there to save tokens. The item id is
  derived from the turn's first tool-call id so a re-sent history keeps a
  byte-identical prefix for the cache.
- `incomplete_details.reason` maps to stop reasons: `max_output_tokens` →
  `max_tokens`, `content_filter` → `refusal`.
- Usage: DeepSeek's `input_tokens` *includes* cached tokens, so
  `cached_tokens` maps to the prompt-cache hit/miss convention (miss =
  input − cached) rather than the Anthropic-style exclusive
  `cacheReadInputTokens`, keeping billed-input accounting honest.
- `web_search_call` history items are replayed **as-is** — the server
  restores the search results from the call id — and Rebon's
  `web_search_result` blocks are therefore not re-sent.
- Context overflow returns HTTP 400 upstream (`truncation` unsupported);
  Rebon's pruning budget from the 1M `contextWindow` declaration is the
  first line of defence.

Handled events: `response.output_text.delta`,
`response.reasoning_text.delta` (→ thinking),
`response.function_call_arguments.delta`,
`response.custom_tool_call_input.delta` (buffered, emitted as one JSON
input), `response.output_item.*` for `web_search_call` (→ `server_tool_use`
block; the `response.web_search_call.*` status pings are ignored),
`response.completed` / `incomplete` / `failed`. Usage reports cached input
tokens (`input_tokens_details.cached_tokens`) and
`output_tokens_details.reasoning_tokens`.

Image input is not accepted by either V4 model, so image blocks are replaced
with placeholder text rather than failing the turn.

If the DeepSeek dialect drifts, `options.body` / `options.extraBody` /
`options.omitBodyFields` on the provider entry (also per-model under
`models.<id>.options`) patch the outgoing request without a plugin release.

## Gateways with keyed prefix caches

When `baseUrl` points at an OpenAI-compatible gateway instead of the
official endpoint (e.g. opencode Go), the forwarded per-session
`prompt_cache_key` keeps your growing conversation prefix in one cache
bucket, and pinning a longer retention stops the gateway's short default
TTL (~5 minutes on opencode Go) from evicting it between turns:

```json
{
  "name": "deepseek",
  "apiKey": "$GATEWAY_API_KEY",
  "baseUrl": "https://<gateway-host>",
  "model": "deepseek-v4-pro",
  "options": {
    "extraBody": { "prompt_cache_retention": "24h" }
  }
}
```

## Debugging the wire

Set `REBON_DEEPSEEK_DUMP` to a file path and every outgoing request body is
appended to it as JSONL. Off unless the variable is set, and a failure to write
never fails the turn.

Wire bugs in this dialect surface as opaque upstream 400s whose text names the
wrong cause, and reconstructing the body from the session transcript is *not*
equivalent — the host may send a different projection than it persisted. The
dumped body is the artifact that settles it. Note it contains the full prompt
and any file contents in the conversation.

The plugin reads it from the environment of the Node host it runs in, so set it
before starting Rebon rather than in the manifest — a plane transport declares a
module, not a process, and has no `env` of its own.

## Test

Offline (no key needed):

```sh
node runtimes/node/plugins/deepseek-responses/selftest.mjs
```

Against the real API (verifies your key and the live dialect in one shot;
answer streams to stdout, reasoning and usage to stderr):

```sh
DEEPSEEK_API_KEY=sk-... node runtimes/node/plugins/deepseek-responses/provider.mjs --smoke "Hello, introduce yourself"
```

Runs translation unit tests plus an end-to-end drive of the plugin contract
(`activate` → one registered adapter → a turn with `emit` and an abort signal)
against a local mock SSE server. The Rust side pins the emitted event shapes in
`rebon-api`'s `deepseek_plugin_frame_shapes_deserialize` test and validates
this manifest in `rebon-cli`'s `in_repo_deepseek_responses_plugin_materializes`.
