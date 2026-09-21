# Rebon composition runtime

The dsh/Cordis composition, running on the plugin plane. Where `runtimes/node/plugin-host` is the protocol and knows nothing about what runs on it, this package is what runs on it: one Cordis realm, the built-in seats a dsh plugin injects, and a loader that turns a Cordis plugin into a plugin the plane understands.

The supported runtime is Node `>=24.19.0 <25`, the same range `rebon-node-runtime` pins and `runtimes/node/plugin-host` declares; a Rust test reads all three back so they cannot drift.

## One load per entry

Every Cordis entry is its own `plugin/load`: its own declarations, its own drain, its own place in rebon's ledger. What makes them a composition rather than a pile is the realm they share — Cordis plugins reach each other through services on a context, and mounting them all on one is the whole point.

Two things a flat list cannot say are composition structure rather than plugin facts, so rebon states them once when the composition is created, naming only ids and placement:

- **groups** — a container that is not a module. Unloading one is unloading its members, which rebon does by naming them; loading the group itself is refused.
- **isolate** — Cordis's own realm narrowing. The agent loop needs its own `systemPrompt`, because the real dsh service and rebon's seat share that name.

The consequence worth stating: **`compose:control` is gone.** Single-plugin unload, reload and listing were a service the composition served itself; they are protocol operations now, and the composition no longer has an opinion about them.

## What moved, and what did not

The composition itself is unchanged. Adapters extend `LlmAdapter` and register with `ctx.llm.registerAdapter`; tools are `ctx.tools.register(defineTool({...}))`; prompt sections are `ctx.systemPrompt.section(...)`; web providers resolve at execution time by dsh's five-state rules. The vendored payload — Cordis, cosmokit, schemastery and the dsh package set — lives in this package, under `payload/`, and is loaded verbatim; `src/payload.mjs` is the one place that knows the location. It used to be kept by the deno_core host and pointed at from here; that host is gone.

What moved is everything that was transport:

- **The pumps are gone.** `op_llm_next`/`op_llm_emit` and `op_tool_serve_next`/`op_tool_serve_emit` parked a JS loop on a Rust channel. A model turn is an `llm/stream` request routed by provider, a tool is a declared tool called back through `tool/call`, and their pieces go out on the call with `ctx.emit`. There is no `op_compose_wait` either — the host's read loop keeps the process alive.
- **Registration is a declaration, not a call.** `callService('model-router', 'register', …)`, `('tool-registry', …)`, `('system-prompt', …)` and their compensating `unregister`s are gone. What an entry provides is reported by `plugin/load` and withdrawn by `plugin/unload` draining. This is not a preference: the unregisters lived in Cordis disposers, `dispose()` is synchronous, and a cross-process call is not. The facts the protocol has no field for — a route's model catalog, a prompt section, a web provider — are asked for afterwards with one `service/call` to the `compose` service, rather than widening a ready report every plugin shares.
- **Reaching rebon needs an identity.** A call is bound to the scope incarnation it was made for, so `src/bridge.mjs` carries the handler's context down through `AsyncLocalStorage`. Code running on a plugin's *own* schedule — an agent loop's turn, an adapter fetching a credential inside it — has no inbound call, and falls back to the session that plugin is attached to. Neither available is a named refusal (`[NO_CALL_CONTEXT]`), never a guess.
- **Module resolution is Node's.** `module.registerHooks()` replaces `RebonModuleLoader`'s import map, and the four `node:` shims under `js/node/` do not participate — real Node has `async_hooks`, `util/types`, `path` and `crypto`. `test/resolve.test.mjs` reads the Rust map and holds the two to the same specifier set.

## Attribution

Every call that reaches rebon says which plugin caused it, and the host injects that rather than the plugin reporting it. Two consequences a plugin author sees:

- a plugin whose tool reaches `ctx.web` causes rebon's own `WebSearch` to run, so it declares `WebSearch` in `invokableTools` — the seat's deployment default is rebon's tool, and the plugin is what caused it;
- the loop is what schedules a model-chosen tool, so the loop entry declares the tools rebon offers the model. rebon builds that list, and it is the same list it hands over as `toolCatalog`.

## Required services are checked, not waited for

Cordis lets a plugin wait for a service that has not arrived yet. Inside one composition that is a feature; here a load is a call rebon is waiting on, so a plugin waiting forever is a call that never answers. A composition entry's required `inject` must be satisfiable where it is mounted, rebon orders entries so it is, and a missing one is `[MISSING_INJECT]` naming the service.

## The one payload package that is not the published one

`js/vendor/eventsource-parser/stream.js` was rewritten for deno_core: the published `EventSourceParserStream` extends the WHATWG `TransformStream`, which bare deno_core does not have, so the vendored build is a "Mini streams" transform instead. On real Node that rewrite breaks — the dsh DeepSeek adapter does `body.pipeThrough(new EventSourceParserStream())` and gets `ERR_INVALID_ARG_TYPE` — so `src/eventsource-stream.mjs` restores the upstream shape over the same vendored `createParser`. Framing semantics stay byte-identical on both runtimes; only the plumbing differs.

## Using it

```
rebon-plugin-host --loader runtimes/node/compose-runtime/src/index.mjs
```

Then `plugin/load` the control plugin (`rebon:compose`, entry `src/plugin.mjs`) with the composition's shape as its config, and each entry after it:

```json
{ "payloadDir": "…/runtimes/node/compose-runtime/payload",
  "web": { "searchProvider": "rebon" },
  "entries": [
    { "id": "llm-deepseek" },
    { "id": "loop", "isolate": { "systemPrompt": "loop" },
      "group": [{ "id": "loop-sessions" }, { "id": "agent-loop" }] }
  ] }
```

The embedder half lives in `crates/rebon-harness/src/plugin_plane.rs`; the composition rebon assembles from `kernelPlugins` is `plugin_composition.rs`, and what rebon declares for the packages it ships is `payload-manifests.json` here.

## The scope a plane's own calls travel on

A scoped call needs an open scope, and a plane has work that belongs to no user session: asking an entry what it registered, running a composition tool, streaming a turn for a caller with no session. Every plugin gets one when it loads — `rebon:plane` by default. A plane that exists *for* one session names it after that session instead, which is what a per-session agent loop does: the kernel's event plane is process-wide, so two loops in one rebon process would otherwise be indistinguishable to a subscriber.

## One loop, one host

`@deepseek-ai/dsh-agent` declares a Cordis accessor on the context prototype — process-global, so a second copy fails whatever realm it is placed in — and `@deepseek-ai/dsh-agent-loop` registers itself as *the* agent factory on whichever `agents` registry it can see. `isolate` does not rescue either: isolating `systemPrompt` works, isolating `sessions` alongside it breaks the inject ACL. So a realm is not enough to keep two agent loops apart; a process is, which is the same trade the embedded runtime's isolate made.

## Scope

Ported: `ctx.llm`, `ctx.credentials`, `ctx.tools`, `ctx.web`, `ctx.systemPrompt`, and the loop assembly. What is left on the deno_core side is deletion work, gated on the compatibility matrix.

This package is not a sandbox. Composed plugins are trusted local code, per the plugin plane's trust model.
