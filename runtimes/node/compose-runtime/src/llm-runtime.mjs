// The composition's `ctx.llm` seat, on the plugin plane.
//
// Ported from `js/compose/llm-runtime.js`. The dsh-facing surface is
// unchanged — adapters extend `LlmAdapter`, register with
// `ctx.llm.registerAdapter(providers, adapter)`, and in-composition consumers
// (the agent loop) still go through `prepareCall` / `stream` — because that
// surface is dsh's contract and the transport underneath it is not dsh's
// business.
//
// What changed is everything that was transport:
//
//   * **The pump is gone.** `_pump` parked on `op_llm_next` and `_dispatch`
//     wrote chunks back with `op_llm_emit`. On the plugin plane a model turn
//     arrives as an `llm/stream` request routed by provider, and its chunks go
//     out on that call. What `registerAdapter` hands the registration sink is
//     the handler for that method; there is nothing left to pump.
//   * **Announcing is not a call.** `_announce` used to push each route into
//     the kernel's `model-router` with `callService('model-router',
//     'register', …)`. Registration is a protocol declaration now: the route
//     is reported by `plugin/load`, and its model catalog by the composition's
//     own `compose` service.
//   * **The disposer only forgets locally.** It used to issue the
//     compensating `callService('model-router', 'unregister', …)`. A Cordis
//     disposer is synchronous and a cross-process call is not, so that call
//     could not survive the move — and does not need to. Revocation is what
//     `plugin/unload` draining is for.
import { Service } from 'cordis';
import {
  LlmError,
  deepFreeze,
  callConfigEquals,
  errorChain,
  resolveRetryPolicy,
} from '@deepseek-ai/dsh-llm';
import { withCall } from './bridge.mjs';
import { sinkOf } from './registry.mjs';

// Re-exported from the payload so a plugin extending `LlmAdapter` from
// `@deepseek-ai/dsh-llm` and the runtime checking one share a single class.
export { LlmAdapter } from '@deepseek-ai/dsh-llm';

/**
 * Normalize an adapter throw into dsh's terminal finish chunk
 * (`adapterFailureChunk`, transcribed lean: LlmError keeps its facts,
 * anything else flattens under UNKNOWN — the same rule the loop itself
 * applies at its turn boundary).
 */
function failureChunk(error, signal) {
  const failure = error instanceof LlmError
    ? error.failure
    : { message: errorChain(error), code: 'UNKNOWN' };
  return {
    type: 'finish',
    reason: signal?.aborted || failure.code === 'ABORTED'
      ? { kind: 'aborted', failure }
      : { kind: 'error', failure },
  };
}

export default class RebonLlmRuntime extends Service {
  constructor(ctx) {
    super(ctx, 'llm');
    this.routes = new Map(); // provider -> adapter
    /** Catalog reads still in flight; `settle()` is what waits for them. */
    this.pending = new Set();
  }

  registerAdapter(providers, adapter) {
    if (!providers?.length) {
      throw new Error('an adapter must register at least one provider');
    }
    for (const provider of providers) {
      if (this.routes.has(provider)) {
        throw new Error(`an adapter for provider "${provider}" is already registered`);
      }
    }
    const runtime = this;
    // Read synchronously, before any await can interleave: which plugin is
    // registering is a fact about *now*, not about whenever this resolves.
    const sink = sinkOf(this.ctx);
    for (const provider of providers) {
      sink?.llm(provider, (request, callCtx) =>
        withCall(callCtx, () => runtime.serve(provider, request, callCtx)));
    }
    this.ctx.effect(() => {
      for (const provider of providers) {
        runtime.routes.set(provider, adapter);
        runtime._read(provider, adapter, sink);
      }
      return () => {
        // Local only. What rebon knows about these routes was reported at
        // load and is withdrawn by unload; a disposer cannot tell it anything
        // because a disposer cannot await an answer.
        for (const provider of providers) runtime.routes.delete(provider);
      };
    });
    // dsh handle shape: route replacement exists on it, but a static
    // composition never re-reads registration facts, so a call here is a
    // consumer bug — refuse loudly instead of silently diverging from dsh.
    return {
      providers: [...providers],
      replace() {
        throw new Error('adapter route replacement is not supported in a static composition');
      },
    };
  }

  /**
   * dsh surfaces these entries to its settings UI. The composition has no
   * such surface; record them so diagnostics can list what a composition
   * declared, and accept the call so unmodified dsh plugins load.
   */
  registerConfigurableProviders(entries) {
    this.configurableProviders ??= [];
    for (const entry of entries ?? []) {
      if (entry?.provider) {
        this.configurableProviders.push({
          provider: String(entry.provider),
          displayName: entry.displayName === undefined ? undefined : String(entry.displayName),
        });
      }
    }
  }

  // ---- JS consumer face ----
  //
  // These serve consumers INSIDE the composition (the agent loop); `serve()`
  // serves requests from rebon. Routing is the same in-JS adapter table.
  // v1 honesty boundary: no `llm/stream` middleware waterfall — dispatch goes
  // straight to the owning adapter.

  /** Resolve the owning adapter or refuse with dsh's NO_ADAPTER code. */
  _registration(provider) {
    const adapter = this.routes.get(provider);
    if (!adapter) {
      throw new LlmError(`no adapter registered for provider "${provider}"`, 'NO_ADAPTER');
    }
    return adapter;
  }

  /**
   * dsh `LlmRuntime.prepareCall`, transcribed: resolve the route's exact
   * model, materialize adapter defaults (recorded in `adapterDefaults` so
   * the loop's request header can re-resolve them later), and hand back a
   * one-shot stream bound to this registration.
   */
  async prepareCall(config, signal) {
    const adapter = this._registration(config.provider);
    const info = (await adapter.resolveModel(config.provider, config.model)) ?? {};
    signal?.throwIfAborted?.();
    const defaulted = config.maxTokens === undefined && info.defaultMaxTokens !== undefined
      ? { ...config, maxTokens: info.defaultMaxTokens }
      : config;
    const reasoning = info.reasoning;
    const requested = defaulted.reasoningEffort;
    let resolvedConfig = defaulted;
    if (reasoning === undefined) {
      if (requested !== undefined) {
        throw new LlmError(
          `provider "${config.provider}" model "${config.model}" does not support reasoning effort "${requested}"`,
          'UNSUPPORTED_REASONING_EFFORT',
        );
      }
    } else {
      const effective = requested ?? reasoning.defaultEffort;
      if (effective !== undefined) {
        if (!(reasoning.efforts ?? []).some((effort) => effort.id === effective)) {
          throw new LlmError(
            `provider "${config.provider}" model "${config.model}" does not support reasoning effort "${effective}"`,
            'UNSUPPORTED_REASONING_EFFORT',
          );
        }
        if (requested !== effective) resolvedConfig = { ...defaulted, reasoningEffort: effective };
      }
    }
    const frozen = deepFreeze(structuredClone(resolvedConfig));
    const context = info.context === undefined
      ? undefined
      : deepFreeze(structuredClone(info.context));
    const adapterDefaults = deepFreeze({
      ...(config.reasoningEffort === undefined && frozen.reasoningEffort !== undefined
        ? { reasoningEffort: true }
        : {}),
      ...(config.maxTokens === undefined && frozen.maxTokens !== undefined
        ? { maxTokens: true }
        : {}),
    });
    const runtime = this;
    let dispatched = false;
    return Object.freeze({
      config: frozen,
      // dsh contract: providerRetryPolicy returns an ALREADY-RESOLVED
      // policy (or undefined for the normal defaults) — never re-resolve.
      retryPolicy: adapter.providerRetryPolicy(config.provider)
        ?? resolveRetryPolicy(undefined, `llm: provider "${config.provider}" retryPolicy`),
      adapterDefaults,
      ...(context === undefined ? {} : { context }),
      stream(options) {
        if (dispatched) {
          throw new LlmError('a prepared LLM call can only be dispatched once', 'INVALID_PREPARED_CALL');
        }
        if (!callConfigEquals(options, frozen)) {
          throw new LlmError('prepared LLM call config changed before adapter dispatch', 'INVALID_PREPARED_CALL');
        }
        dispatched = true;
        return runtime._consumerStream(options);
      },
    });
  }

  /** Consumer fallback entry (the loop's NO_ADAPTER path): direct dispatch. */
  stream(options) {
    return this._consumerStream(options);
  }

  /**
   * Adapter boundary for in-composition consumers: selection, dispatch, and
   * iteration failures become one terminal failure chunk (dsh
   * `adapterStream` semantics) so the loop's assembler/request-error path
   * sees them instead of a raw throw.
   */
  async *_consumerStream(options) {
    let iterator;
    try {
      const adapter = this._registration(options.provider);
      iterator = adapter.stream(options)[Symbol.asyncIterator]();
    } catch (error) {
      yield failureChunk(error, options.signal);
      return;
    }
    while (true) {
      let item;
      try {
        item = await iterator.next();
      } catch (error) {
        yield failureChunk(error, options.signal);
        return;
      }
      if (item.done) return;
      yield item.value;
    }
  }

  // ---- what rebon is told, and what rebon asks for ----

  /**
   * Read one route's catalog for the load report.
   *
   * `listModels` is advisory in dsh and can take a network round trip, so a
   * failure is recorded rather than raised: a provider with no catalog is
   * still a working route whose callers name models explicitly. The route
   * table is the liveness witness — an adapter disposed while its catalog was
   * being read is no longer the live one, and its answer is dropped.
   */
  _read(provider, adapter, sink) {
    if (sink === undefined) return;
    const task = (async () => {
      let models = [];
      try {
        models = (await adapter.listModels(provider)) ?? [];
      } catch (error) {
        if (this.routes.get(provider) !== adapter) return;
        sink.catalog(provider, { models: [], defaultModel: 'default', catalogError: String(error?.message ?? error) });
        return;
      }
      if (this.routes.get(provider) !== adapter) return;
      // A key whose value is `undefined` is not JSON, and this report crosses
      // the wire: an unknown context window is an absent field, not a present
      // one holding nothing.
      const entries = models
        .filter((m) => m?.id)
        .map((m) => {
          const window = m.context?.window ?? m.contextWindow;
          return { id: m.id, ...(window === undefined ? {} : { contextWindow: window }) };
        });
      sink.catalog(provider, {
        models: entries,
        // The catalog is advisory (dsh semantics): with an empty one the route
        // still exists and callers name models explicitly; "default" is only
        // the resolve fallback for bare `{provider}` lookups.
        defaultModel: entries[0]?.id ?? 'default',
      });
    })();
    this.pending.add(task);
    void task.finally(() => this.pending.delete(task));
  }

  /** Waits for every catalog read in flight, so the load report is complete. */
  async settle() {
    while (this.pending.size > 0) await Promise.all([...this.pending]);
  }

  /**
   * Serves one `llm/stream` call: chunks out on the call, adapter throws left
   * to become its terminal.
   *
   * Deliberately the raw adapter rather than `_consumerStream`: an in-JS
   * consumer wants a failure flattened into a finish chunk it can assemble,
   * whereas rebon wants the call to end as an error carrying the adapter's own
   * machine code. Flattening here would report every failed turn as a
   * successful one whose last chunk happens to say otherwise.
   */
  async serve(provider, request, ctx) {
    const adapter = this._registration(provider);
    const options = {
      provider,
      model: request.model,
      messages: request.messages ?? [],
      system: request.system ?? undefined,
      tools: request.tools ?? undefined,
      maxTokens: request.maxTokens ?? undefined,
      temperature: request.temperature ?? undefined,
      stop: request.stop ?? undefined,
      reasoningEffort: request.reasoningEffort ?? undefined,
      sessionId: request.sessionId ?? undefined,
      purpose: request.purpose ?? undefined,
      signal: ctx.signal,
    };
    let count = 0;
    for await (const chunk of adapter.stream(options)) {
      await ctx.emit(chunk);
      count += 1;
    }
    return { chunks: count };
  }
}
