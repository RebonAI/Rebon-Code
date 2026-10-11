// The package's root export: the loader seam for Claude Code mods.
//
// A mod — a Claude Code plugin of function hooks, `.claude-plugin/plugin.json`
// beside a `hooks/hooks.json` naming one hooks module — is a plane plugin of
// a shape the host's own loader does not know. The explicit `claude-mods`
// adapter selects it; `config.$claudeMod` only carries the scanned manifest
// and options rebon synthesised from the plugin folder.
//
// `createLoader({ next })` follows the host loader contract, but this loader
// executes only mods and never falls back to another ecosystem.
import { Mod, MOD_SERVICE } from './mod.mjs';
import { requireAdapter } from '../../plugin-host/src/loader.mjs';
import { ProtocolError } from '../../plugin-host/src/protocol.mjs';

export { MOD_SERVICE };
export { transformJsx } from './jsx-transform.mjs';
export { transpileModSource } from './transpile.mjs';
export { HookChain, patternSelects, matcherSelects } from './chain.mjs';
export { elementTable, SURFACES } from './render.mjs';

const mods = new Map();

/// The marker rebon puts on a mod's load request.
export const MARKER = '$claudeMod';

/// Whether the request carries mod configuration. This selects an ecosystem
/// only for explicit legacy-1.9; modern mods merely validate their config here.
/// Remove that selection heuristic with legacy-1.9 in the next major.
export function isModRequest(request) {
  const marker = request?.config?.[MARKER];
  return marker !== null && typeof marker === 'object' && !Array.isArray(marker);
}

export async function createLoader() {
  return {
    async load(request) {
      requireAdapter(request.adapter, 'claude-mods');
      if (!isModRequest(request)) {
        throw new ProtocolError('[WRONG_SHAPE]', 'claude-mods requires config.$claudeMod');
      }
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
