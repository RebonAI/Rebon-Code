// What a plugin package *is*, when the composition is in the picture.
//
// The host knows one shape: a module exporting `activate`, which registers
// through the api it is handed. A Cordis plugin knows nothing about any of
// that — it exports `apply(ctx, config)` and registers by calling services on
// a context it shares with every other entry. Both are plugins on the plane;
// they differ only in what "loading" means, which is exactly the question the
// host delegates.
//
// The load request explicitly selects native, Cordis, or Claude Code mods.
// Export shape only selects a legal entry form *within* that ecosystem.
// The explicit legacy-1.9 profile alone retains the 1.9.x selection rules;
// package admission decides eligibility, and the next major removes it.
import { requireAdapter, resolveEntry, UnsupportedAdapter } from '../../plugin-host/src/loader.mjs';
import { pluginAdapter } from '../../plugin-host/src/methods.mjs';
import { createLoader as createModsLoader, isModRequest } from '../../mods-runtime/src/index.mjs';

const own = (value, key) => value != null && Object.prototype.hasOwnProperty.call(value, key);

class LoaderError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
    this.name = 'LoaderError';
  }
}

/// The Cordis plugin a module holds, or undefined if it does not hold one.
export function cordisPluginOf(module) {
  if (own(module, 'apply') && typeof module.apply === 'function') return module;
  const fallback = module?.default;
  if (fallback && typeof fallback === 'object' && typeof fallback.apply === 'function') return fallback;
  if (typeof fallback === 'function') return fallback;
  return undefined;
}

/// Builds the load/unload seams the host runs with.
///
/// `next` is the host's native loader: delegating keeps one set of rules for
/// that ecosystem. No unrecognised adapter is passed through to it.
export async function createLoader({ next }) {
  // Mod configuration still carries its scanned declarations, but only an
  // explicit adapter selects that loader outside the legacy profile.
  const mods = await createModsLoader();
  return {
    async load(request) {
      const adapter = pluginAdapter(request.adapter);
      switch (adapter.id) {
        case 'native': return next(request);
        case 'claude-mods': return mods.load(request);
        case 'cordis':
        case 'legacy-1.9': requireAdapter(adapter, adapter.id); break;
        default: throw new UnsupportedAdapter(adapter);
      }
      const legacy = adapter.id === 'legacy-1.9';
      if (legacy && isModRequest(request)) {
        return mods.load({ ...request, adapter: { id: 'claude-mods', revision: 1 } });
      }
      const url = resolveEntry(request.root, request.entry);
      let module;
      try {
        module = await import(url);
      } catch (cause) {
        throw new LoaderError('[ENTRY_FAILED]', `plugin entry ${request.entry} failed to load: ${cause?.message ?? cause}`);
      }
      const plugin = legacy && typeof module?.activate === 'function'
        ? undefined : cordisPluginOf(module);
      // Only the explicit legacy profile keeps shape-based dispatch. Importing
      // is idempotent; passing the module through avoids evaluating it twice.
      // The derived native request does not mutate the caller's descriptor.
      if (plugin === undefined) {
        if (legacy) return next({ ...request, adapter: { id: 'native', revision: 1 } }, async () => module);
        throw new LoaderError('[NOT_CORDIS]', `plugin entry ${request.entry} exports no Cordis plugin (no apply)`);
      }

      // Imported lazily: this module is loaded by the host at startup, long
      // before module resolution for the payload has been installed.
      const { mountEntry } = await import('./realm.mjs');
      const sink = await mountEntry(request, plugin);
      return sink.loaded();
    },

    async unload(pluginId) {
      if (await mods.unload(pluginId)) return;
      const { disposeEntry } = await import('./realm.mjs');
      await disposeEntry(pluginId);
    },
  };
}
