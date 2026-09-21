// The composition's `ctx.web` seat, on the plugin plane.
//
// Ported from `js/compose/web-runtime.js`. The dsh-facing surface is
// unchanged and verbatim: providers register with
// `ctx.web.registerSearchProvider` / `registerFetchProvider` (duplicate ids
// throw WEB_DUPLICATE_PROVIDER, the exact disposer comes back), and
// `search`/`fetch` resolve their provider at EXECUTION time with dsh's
// five-state rules — a configured id wins or fails loudly, never falls back;
// otherwise exactly one usable provider auto-selects. `maxResults` truncation
// (capSources) is enforced on the way out.
//
// Deployment default: the `rebon` builtin provider, backed by rebon's own
// WebSearch/WebFetch tools through `tool/invoke`, is always registered, and the
// configured id DEFAULTS to `"rebon"`. So a plugin provider takes over only
// when configured, and dsh's AMBIGUOUS state is unreachable unless the default
// is explicitly cleared. **A plugin whose tool reaches `ctx.web` therefore
// causes a `tool/invoke` of rebon's WebSearch, attributed to that plugin** —
// which is why such a plugin declares `WebSearch`/`WebFetch` in
// `invokableTools`. The attribution is the point: rebon can always answer who
// caused a tool to run.
//
// What changed: `emit('web/provider-registered', …)` and the shared serve pump
// are gone. A registered provider is *reported* — it appears in the plugin's
// load report as a service named `web:<kind>:<id>`, which is also how rebon
// dispatches into it.
import { Service } from 'cordis';
import { WebError } from '@deepseek-ai/dsh-web';
import { invokeTool, withCall } from './bridge.mjs';
import { sinkOf } from './registry.mjs';

/** The service-name namespace a plugin web provider is served under. */
export const webTarget = (kind, id) => `web:${kind}:${id}`;

/** dsh WebRuntime.resolveProvider, transcribed. */
function resolveProvider(configuredId, providers) {
  if (configuredId !== undefined) {
    const provider = providers.get(configuredId);
    if (!provider) {
      throw new WebError(
        `configured web provider "${configuredId}" is not registered`,
        'WEB_PROVIDER_CONFIGURED_MISSING',
      );
    }
    if (!provider.available()) {
      throw new WebError(
        `configured web provider "${configuredId}" is registered but unavailable`,
        'WEB_PROVIDER_CONFIGURED_UNAVAILABLE',
      );
    }
    return provider;
  }
  const usable = [...providers.values()].filter((provider) => {
    try {
      return !!provider.available();
    } catch {
      return false;
    }
  });
  if (usable.length === 0) {
    throw new WebError('no usable web provider is registered', 'WEB_PROVIDER_UNAVAILABLE');
  }
  if (usable.length > 1) {
    const ids = usable.map((provider) => provider.id).join(', ');
    throw new WebError(
      `multiple usable web providers are registered (${ids}); configure one explicitly`,
      'WEB_PROVIDER_AMBIGUOUS',
    );
  }
  return usable[0];
}

/** dsh capSources: enforce `maxResults` on the way back. */
function capSources(result, maxResults) {
  if (maxResults === undefined || (result.sources?.length ?? 0) <= maxResults) return result;
  return { ...result, sources: result.sources.slice(0, maxResults), truncated: true };
}

/**
 * The rebon builtin search provider: reaches rebon's WebSearch tool and maps
 * the `{query, answer, results}` contract into dsh's result vocabulary.
 */
const rebonBuiltinSearch = {
  id: 'rebon',
  available: () => true,
  async search(request, _signal) {
    const out = await invokeTool('WebSearch', { query: String(request?.query ?? '') });
    const sources = (out?.results ?? [])
      .map((entry) => ({
        url: String(entry?.url ?? ''),
        ...(entry?.title ? { title: String(entry.title) } : {}),
        ...(entry?.snippet ? { snippet: String(entry.snippet) } : {}),
      }))
      .filter((source) => source.url.length > 0);
    return {
      ...(out?.answer ? { content: String(out.answer) } : {}),
      sources,
      truncated: false,
    };
  },
};

/**
 * The rebon builtin fetch provider over rebon's WebFetch tool. The tool
 * returns extracted text, not raw transport facts, so `statusCode` is the
 * 200 the extraction implies and the body is `text` — a documented v1
 * approximation, not a transport claim.
 */
const rebonBuiltinFetch = {
  id: 'rebon',
  available: () => true,
  async fetch(request, _signal) {
    const out = await invokeTool('WebFetch', { url: String(request?.url ?? '') });
    const content =
      typeof out === 'string' ? out : String(out?.content ?? out?.text ?? JSON.stringify(out));
    return {
      url: String(request?.url ?? ''),
      statusCode: 200,
      body: { kind: 'text', content },
      truncated: false,
    };
  },
};

export default class RebonWebRuntime extends Service {
  constructor(ctx, options) {
    super(ctx, 'web');
    const config = options?.web ?? {};
    const configured = (value) =>
      typeof value === 'string' && value.trim().length > 0 ? value.trim() : 'rebon';
    this.searchProviderId = configured(config?.searchProvider);
    this.fetchProviderId = configured(config?.fetchProvider);
    this.searchProviders = new Map([['rebon', rebonBuiltinSearch]]);
    this.fetchProviders = new Map([['rebon', rebonBuiltinFetch]]);
  }

  registerSearchProvider(provider) {
    return this._register(this.searchProviders, 'search', provider);
  }

  registerFetchProvider(provider) {
    return this._register(this.fetchProviders, 'fetch', provider);
  }

  _register(store, kind, provider) {
    if (typeof provider?.id !== 'string' || provider.id.length === 0) {
      throw new TypeError('a web provider must declare a non-empty string `id`');
    }
    if (store.has(provider.id)) {
      throw new WebError(
        `a web provider with id "${provider.id}" is already registered`,
        'WEB_DUPLICATE_PROVIDER',
      );
    }
    const runtime = this;
    let available = false;
    try {
      available = !!provider.available();
    } catch {}
    // `available` is a registration-time snapshot: a static composition's
    // configuration does not change under it. Documented v1 semantics,
    // carried over unchanged.
    const sink = sinkOf(this.ctx);
    sink?.service(webTarget(kind, provider.id), (input, callCtx) =>
      withCall(callCtx, () => runtime._serve(kind, provider.id, input, callCtx.signal)));
    sink?.webProvider(kind, provider.id, available);
    // Caller-fork RAII (the Service proxy binds this.ctx to the registrant):
    // plugin disposal removes the provider, and a call arriving afterwards is
    // refused rather than answered by a provider nobody can revoke.
    return this.ctx.effect(() => {
      store.set(provider.id, provider);
      return () => {
        store.delete(provider.id);
      };
    });
  }

  /** rebon-dispatched execution of one plugin provider (seat rules applied). */
  async _serve(kind, id, input, signal) {
    const store = kind === 'search' ? this.searchProviders : this.fetchProviders;
    const provider = store.get(id);
    if (provider === undefined) {
      throw new WebError(
        `web provider "${id}" is no longer registered in this composition`,
        'WEB_PROVIDER_UNAVAILABLE',
      );
    }
    if (kind === 'search') {
      const request = {
        query: String(input?.query ?? ''),
        ...(input?.maxResults !== undefined ? { maxResults: input.maxResults } : {}),
      };
      return capSources(await provider.search(request, signal), request.maxResults);
    }
    return provider.fetch({ url: String(input?.url ?? '') }, signal);
  }

  /** dsh consumer API: run one search through the selected provider. */
  async search(request, signal) {
    const provider = resolveProvider(this.searchProviderId, this.searchProviders);
    const result = await provider.search(request, signal);
    return capSources(result, request?.maxResults);
  }

  /** dsh consumer API: retrieve one URL through the selected provider. */
  async fetch(request, signal) {
    const provider = resolveProvider(this.fetchProviderId, this.fetchProviders);
    return provider.fetch(request, signal);
  }
}
