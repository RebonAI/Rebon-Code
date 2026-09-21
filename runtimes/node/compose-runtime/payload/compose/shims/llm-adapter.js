// dsh's abstract LLM adapter, on its own so that importing it costs nothing
// else.
//
// It used to live in `compose/llm-runtime.js`, which the `@deepseek-ai/dsh-llm`
// shim re-exported it from. That made the base class every provider plugin
// extends reachable only by evaluating a module that is bound to one host's
// transport — harmless under deno_core, fatal on real Node, where the same
// import chain reaches `globalThis.Deno.core` and every dsh package that
// touches `@deepseek-ai/dsh-llm` fails to load.
//
// The class is the contract; the runtime is one implementation of the seat
// that serves it. Keeping them in separate modules is what lets both hosts
// hand plugins the same base class.

/** dsh's abstract adapter: only `stream(options)` is required. */
export class LlmAdapter {
  providerInfo(provider) {
    return { id: provider, name: provider };
  }

  providerRetryPolicy(_provider) {
    return undefined;
  }

  listModels(_provider) {
    return Promise.resolve([]);
  }

  resolveModel(provider, model) {
    return Promise.resolve({ provider, id: model, name: model });
  }
}
