// The realm's entry table, as the dsh Loader vocabulary sees it.
//
// One view per mounted entry, shaped like a `cordis-plugin-loader` entry
// (`id`, `options.{id,name,config}`, `fiber`), so the services that answer
// for the loader (`loader-runtime.mjs`) hand upstream code the shape it
// reads. `realm.mjs` writes it as entries mount and leave; nothing else
// does. Its own module so both sides import it without importing each other.

/** Mounted entries by plugin id, in load order. */
export const entries = new Map();

let inflight = 0;
let waiters = [];

/// Records one mount for its whole duration, so `settled()` can wait it out.
export async function mounting(work) {
  inflight += 1;
  try {
    return await work();
  } finally {
    inflight -= 1;
    if (inflight === 0) {
      const ready = waiters;
      waiters = [];
      for (const resolve of ready) resolve();
    }
  }
}

/// Resolves once nothing is mounting.
export function settled() {
  if (inflight === 0) return Promise.resolve();
  return new Promise((resolve) => waiters.push(resolve));
}

/// The loader-shaped view of one mounted entry.
export function track(pluginId, request, fiber) {
  const view = {
    id: pluginId,
    options: { id: pluginId, name: request.name ?? pluginId, config: request.config ?? undefined },
    fiber,
    // Where the entry's module is, for what reads its package (its client
    // half): the load request's package root and module.
    root: request.root,
    module: request.entry,
  };
  entries.set(pluginId, view);
  return view;
}

export function untrack(pluginId) {
  entries.delete(pluginId);
}
