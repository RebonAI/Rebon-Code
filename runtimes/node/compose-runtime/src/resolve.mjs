// Module resolution for the composition, on real Node.
//
// Node has no import map, but it has `module.registerHooks()` — synchronous,
// in-thread resolve/load hooks — and this is the composition's map written in
// that vocabulary. There used to be a second map, in the deno_core host's
// `build_import_map`, and a test here holding the two to the same specifier
// set; that host is gone, so this is the only map there is.
//
// Two entries it does not carry, both from the deleted one and both in the
// direction of deleting code:
//
//   * the four `node:` entries. `node:async_hooks`, `node:util/types`,
//     `node:path` and `node:crypto` resolved to hand-written shims because bare
//     deno_core has no standard library; real Node has all four.
//   * `eventsource-parser/stream` resolves to this package's module rather than
//     the vendored one. The vendored build is not the published package: it was
//     rewritten against the "Mini streams" protocol the old host installed,
//     because the published `EventSourceParserStream` extends the WHATWG
//     `TransformStream` and bare deno_core has none. On real Node that
//     rewrite breaks — `body.pipeThrough(new EventSourceParserStream())` throws
//     `ERR_INVALID_ARG_TYPE` — so the upstream shape comes back. See
//     `eventsource-stream.mjs`.
import { isBuiltin, registerHooks } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { payloadDir } from './payload.mjs';
import provided from './provided-modules.json' with { type: 'json' };

/// Payload-relative module map, keyed by bare specifier: what rebon owns and
/// what its vendored payload is built from, together.
export function payloadModules(dir) {
  return new Map([...ownedModules(dir), ...vendoredModules(dir)]);
}

/// The specifiers rebon answers for every importer.
export function ownedModules(dir) {
  const at = (relative) => pathToFileURL(path.join(dir, relative)).href;
  return new Map(Object.entries(provided.owned).map(([name, relative]) => [name, at(relative)]));
}

/// The vendored packages rebon's own payload imports. A package installed
/// from npm brings its own copy of these and gets it; only one that has none
/// falls back to the vendored build.
export function vendoredModules(dir) {
  const at = (relative) => pathToFileURL(path.join(dir, relative)).href;
  return new Map(Object.entries(provided.payload).map(([name, relative]) => [name, at(relative)]));
}

/// Specifiers this package answers itself rather than from the payload.
export function localModules() {
  const at = (relative) => new URL(relative, import.meta.url).href;
  return new Map(Object.entries(provided.runtime).map(([name, relative]) => [name, at(relative)]));
}

let installed;

/// Installs the composition's module resolution, once per process.
///
/// `extraModules` is the embedder's own map (the `modules` half of a
/// composition config: plugin name → absolute module file), which is how a
/// plugin outside the vendored payload gets a name.
///
/// Returns the resolved map so callers can report what a specifier became
/// without repeating the lookup rules.
export function installModuleResolution({ dir = payloadDir(), extraModules = {} } = {}) {
  const map = new Map([...ownedModules(dir), ...localModules()]);
  for (const [name, file] of Object.entries(extraModules)) {
    map.set(name, pathToFileURL(path.resolve(file)).href);
  }
  const vendored = vendoredModules(dir);
  if (installed) {
    // Hooks are process-wide and cannot be unregistered per call; re-running
    // with a different map would leave two resolvers disagreeing about the
    // same specifier, which is a debugging trap rather than a feature.
    for (const [name, url] of map) installed.map.set(name, url);
    return new Map([...installed.vendored, ...installed.map]);
  }

  const payloadPrefix = pathToFileURL(dir.endsWith(path.sep) ? dir : dir + path.sep).href;
  const runtimePrefix = new URL('./', import.meta.url).href;
  // Rebon's own code — the payload and this runtime — always gets the
  // vendored build it was written against.
  const ours = (parent) => parent === undefined || parent.startsWith(payloadPrefix) || parent.startsWith(runtimePrefix);
  const state = { map, vendored };
  registerHooks({
    resolve(specifier, context, next) {
      // The native Cordis, where the host defines it (cordis-rs on Verve's
      // in-process host). Elsewhere `cordis` is not a builtin and the
      // vendored bundle answers, as before.
      if ((specifier === 'cordis' || specifier === '@deepseek-ai/cordis') && isBuiltin('cordis')) {
        return { url: 'node:cordis', shortCircuit: true };
      }
      const mapped = state.map.get(specifier);
      if (mapped !== undefined) return { url: mapped, format: 'module', shortCircuit: true };
      const fallback = state.vendored.get(specifier);
      if (fallback === undefined) return next(specifier, context);
      if (ours(context?.parentURL)) return { url: fallback, format: 'module', shortCircuit: true };
      // A package from npm: its own copy if it installed one — dsh's current
      // packages need their own versions of these — else the vendored one.
      try {
        return next(specifier, context);
      } catch {
        return { url: fallback, format: 'module', shortCircuit: true };
      }
    },
    // The payload's ESM is `.js` under no `package.json`, so Node's own rules
    // read it as CommonJS and every `import` in it is a syntax error. The
    // deno_core loader had no such ambiguity — everything it loaded was a
    // module — so this is a fact about Node, not about the payload.
    load(url, context, next) {
      if (url.startsWith(payloadPrefix)) return next(url, { ...context, format: 'module' });
      return next(url, context);
    },
  });
  installed = state;
  return new Map([...vendored, ...map]);
}
