// The package's root export: the loader seam for Claude Code mods.
//
// A mod — a Claude Code plugin of function hooks, `.claude-plugin/plugin.json`
// beside a `hooks/hooks.json` naming one hooks module — is a plane plugin of
// a shape the host's own loader does not know. This decides it is one by
// reading the load request, not the module: rebon marks the request's
// `config.$claudeMod` when it synthesised the manifest from the plugin's
// folder, so what loads here is exactly what rebon said it was loading.
//
// `createLoader({ next })` is the host's contract: `next` is the loader a
// request this one does not recognise is handed to, untouched.
import { Mod, MOD_SERVICE } from './mod.mjs';

export { MOD_SERVICE };
export { transformJsx } from './jsx-transform.mjs';
export { transpileModSource } from './transpile.mjs';
export { HookChain, patternSelects, matcherSelects } from './chain.mjs';
export { elementTable, SURFACES } from './render.mjs';

const mods = new Map();

/// The marker rebon puts on a mod's load request.
export const MARKER = '$claudeMod';

/// Whether a load request is a mod's.
export function isModRequest(request) {
  const marker = request?.config?.[MARKER];
  return marker !== null && typeof marker === 'object' && !Array.isArray(marker);
}

export async function createLoader({ next }) {
  return {
    async load(request) {
      if (!isModRequest(request)) return next(request);
      const mod = new Mod({
        pluginId: request.pluginId,
        root: request.root,
        entry: request.entry,
        marker: request.config[MARKER],
      });
      await mod.load();
      mods.set(request.pluginId, mod);
      return mod.sealed();
    },
    async unload(pluginId) {
      const mod = mods.get(pluginId);
      if (!mod) return false;
      mods.delete(pluginId);
      mod.dispose();
      return true;
    },
  };
}

/// The mods this process has loaded, by plugin id.
export function loadedMods() {
  return new Map(mods);
}
