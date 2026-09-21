// What a plugin package *is*, when the composition is in the picture.
//
// The host knows one shape: a module exporting `activate`, which registers
// through the api it is handed. A Cordis plugin knows nothing about any of
// that — it exports `apply(ctx, config)` and registers by calling services on
// a context it shares with every other entry. Both are plugins on the plane;
// they differ only in what "loading" means, which is exactly the question the
// host delegates.
//
// The rule, in order, and deliberately not a heuristic — a package's shape is
// something its author decides, so it should be decidable by reading it:
//
//   1. `activate` is a function                → the host's own loader
//   2. `apply` is the module's own function    → a Cordis plugin (the module)
//   3. `default` is an object with `apply`     → a Cordis plugin
//   4. `default` is a function                 → a Cordis functional plugin
//   5. anything else                           → the host's own loader, which
//                                                refuses it by name
//
// Step 1 comes first so a rebon-native plugin is never mistaken for a Cordis
// one, and step 4 is last so the ambiguous case (a bare default function) goes
// to the composition rather than being guessed at from argument counts.
import { resolveEntry } from '../../plugin-host/src/loader.mjs';

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
  if (typeof module?.activate === 'function') return undefined;
  if (own(module, 'apply') && typeof module.apply === 'function') return module;
  const fallback = module?.default;
  if (fallback && typeof fallback === 'object' && typeof fallback.apply === 'function') return fallback;
  if (typeof fallback === 'function') return fallback;
  return undefined;
}

/// Builds the load/unload seams the host runs with.
///
/// `next` is the host's own loader, handed back untouched for anything this
/// module does not recognise — an adapter that reimplemented it would be a
/// second set of rules for the same shape.
export async function createLoader({ next }) {
  return {
    async load(request) {
      const url = resolveEntry(request.root, request.entry);
      let module;
      try {
        module = await import(url);
      } catch (cause) {
        throw new LoaderError('[ENTRY_FAILED]', `plugin entry ${request.entry} failed to load: ${cause?.message ?? cause}`);
      }
      const plugin = cordisPluginOf(module);
      // Importing is idempotent, so handing the already-imported module back to
      // the host's loader costs nothing and keeps one import per entry — which
      // matters, because a module evaluated twice would register twice.
      if (plugin === undefined) return next(request, async () => module);

      // Imported lazily: this module is loaded by the host at startup, long
      // before module resolution for the payload has been installed.
      const { mountEntry } = await import('./realm.mjs');
      const sink = await mountEntry(request, plugin);
      return sink.loaded();
    },

    async unload(pluginId) {
      const { disposeEntry } = await import('./realm.mjs');
      await disposeEntry(pluginId);
    },
  };
}
