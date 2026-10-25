// The Host's half of DeepSeek Harness's client contract, for what this realm
// owns.
//
// A dsh client plugin (`lib/client.js`, React on a page) reaches the Host
// through `ctx.remote.<namespace>.<method>(...)`. dsh's gateway turns that
// into a call on a Host service with exact named arguments and answers
// `{ ok: true, value }` or `{ ok: false, error: { code, message, details } }`
// — never a thrown carrier fault. Rebon's plane is the carrier: the desktop
// app (or a test) sends `{ endpoint: 'settings/describe', args: {...} }` to
// the composition's `compose` service as `kind: 'remote'`, and this module
// answers the namespaces whose facts live in the realm:
//
//   * `settings` — upstream `@deepseek-ai/dsh-settings` over rebon's
//     configuration editor, read redacted and written with dsh's revision
//     fence, the way dsh's `settings-controller` does;
//   * `pluginInventory` — the entries mounted here, with their phases;
//   * `rebon/clientModules` — rebon's own: which mounted entries ship a
//     browser half (`package.json` `dsh.client`), for the client realm to
//     load. dsh injects this as `__DSH_BOOT__`; rebon asks for it.
//
// Namespaces this realm does not hold (`pluginManager`, `credentials`,
// sessions, …) are the plane's to answer or `gateway/method-unavailable`, as
// dsh's gateway says
// for an endpoint no live service claims. The events the client hears
// (`settings/document-updated`, …) are published by the control plugin on
// `compose:remote-event`, see `plugin.mjs`.
import fs from 'node:fs';
import path from 'node:path';
import { realm } from './realm.mjs';
import { entries } from './entries.mjs';

/** The forwarded events dsh's remotes allowlist names, that a realm emits. */
export const REMOTE_EVENTS = Object.freeze([
  'settings/document-updated',
  'credentials/record-updated',
  'credentials/reference-updated',
  'llm/adapters-updated',
  'commands/change',
]);

class RemoteError extends Error {
  constructor(code, message, details = {}) {
    super(message);
    this.code = code;
    this.details = details;
    this.name = 'RemoteError';
  }
}

/// One redacted descriptor as the wire carries it, field by field
/// (`settings-controller`'s `namespaceView`).
function namespaceView(descriptor) {
  return {
    ns: String(descriptor.ns),
    autoGenerate: descriptor.autoGenerate,
    schema: descriptor.schema,
    value: descriptor.value,
    ...(descriptor.base === undefined ? {} : { base: descriptor.base }),
    ...(descriptor.user === undefined ? {} : { user: descriptor.user }),
    applies: descriptor.applies,
    secrets: (descriptor.secrets ?? []).map((secret) => ({ path: [...secret.path], set: secret.set })),
    revision: descriptor.revision,
  };
}

function settingsService() {
  const settings = realm().ctx.get('settings');
  if (settings === undefined) {
    throw new RemoteError(
      'gateway/method-unavailable',
      'no settings service is mounted in this composition: install @deepseek-ai/dsh-settings with the packages it configures',
    );
  }
  return settings;
}

/// A refusal from the settings service, classified as dsh classifies it: a
/// stale revision is its own outcome, everything else is a rejection.
function settingsRefusal(ns, error) {
  if (error?.code === 'SETTINGS_CONFLICT' && typeof error.expected === 'number' && typeof error.actual === 'number') {
    return new RemoteError('settings/conflict', String(error.message), { ns, expected: error.expected, actual: error.actual });
  }
  return new RemoteError('settings/rejected', String(error?.message ?? error), { ns });
}

async function settingsWrite(mode, args) {
  const ns = args?.ns;
  if (typeof ns !== 'string' || ns.length === 0) {
    throw new RemoteError('gateway/bad-request', `invalid payload for settings.${mode}`, { field: 'ns' });
  }
  const settings = settingsService();
  const input = mode === 'mutate' ? args.ops : mode === 'replace' ? args.section : args.patch;
  try {
    await settings[mode](ns, input, args.expectedRevision ?? undefined);
  } catch (error) {
    throw settingsRefusal(ns, error);
  }
  const descriptor = settings.describe({ redactSecrets: true }).find((candidate) => candidate.ns === ns);
  if (descriptor === undefined) {
    throw new RemoteError('gateway/internal', `settings namespace "${ns}" was disposed after the ${mode}`);
  }
  return namespaceView(descriptor);
}

/// dsh's fiber phases, from a Cordis fiber state.
function phaseOf(fiber) {
  switch (fiber?.state) {
    case 0: return 'pending';
    case 1: return 'loading';
    case 2: return 'active';
    case 3: return 'failed';
    case 4: return 'unloading';
    default: return null;
  }
}

/// The name of the package an entry's module belongs to, else its id.
function moduleNameOf(entry) {
  const root = packageRoot(entry);
  if (root !== undefined) {
    try {
      const name = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8')).name;
      if (typeof name === 'string') return name;
    } catch {}
  }
  return entry.options.name;
}

function inventory() {
  return {
    // Which entries run is rebon's composition, changed where rebon's
    // plugins are configured; a page reads it.
    managementAvailable: false,
    entries: [...entries.values()].map((entry) => ({
      entryId: entry.id,
      moduleName: moduleNameOf(entry),
      enabled: true,
      fiberPhase: phaseOf(entry.fiber),
    })),
  };
}

/// The client half a package ships, as its `package.json` declares it.
function clientHalf(root) {
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
  } catch {
    return undefined;
  }
  const client = manifest?.dsh?.client;
  if (client === null || typeof client !== 'object' || client.platform !== 'web') return undefined;
  const exported = manifest.exports?.['./client'];
  const relative = typeof exported === 'string' ? exported : (exported?.default ?? exported?.import ?? 'lib/client.js');
  const file = path.resolve(root, relative);
  let stat;
  try {
    stat = fs.statSync(file);
  } catch {
    return undefined;
  }
  return {
    id: manifest.name,
    file,
    // dsh's `rev` changes whenever the bundle does; mtime and size say so.
    rev: `${Math.trunc(stat.mtimeMs)}-${stat.size}`,
    inject: Array.isArray(client.inject) ? [...client.inject] : [],
    immediately: client.immediately === true,
    external: Array.isArray(client.external) ? [...client.external] : [],
  };
}

/// The package an entry's module belongs to: the nearest directory above
/// it holding a `package.json`, the module's own package as Node reads it.
function packageRoot(entry) {
  if (typeof entry.root !== 'string' || typeof entry.module !== 'string') return undefined;
  let dir = path.dirname(path.resolve(entry.root, entry.module));
  const top = path.resolve(entry.root);
  for (;;) {
    if (fs.existsSync(path.join(dir, 'package.json'))) return dir;
    if (dir === top || path.dirname(dir) === dir) return undefined;
    dir = path.dirname(dir);
  }
}

function clientModules() {
  const rows = [];
  for (const entry of entries.values()) {
    const root = packageRoot(entry);
    if (root === undefined) continue;
    const half = clientHalf(root);
    if (half !== undefined && !rows.some((row) => row.id === half.id)) rows.push({ entryId: entry.id, ...half });
  }
  return { entries: rows };
}

const ENDPOINTS = {
  'settings/describe': () => {
    const settings = settingsService();
    return {
      writable: settings.writable,
      hasDocument: settings.documentPath !== undefined,
      namespaces: settings.describe({ redactSecrets: true }).map(namespaceView),
    };
  },
  'settings/update': (args) => settingsWrite('update', args),
  'settings/replace': (args) => settingsWrite('replace', args),
  'settings/mutate': (args) => settingsWrite('mutate', args),
  'pluginInventory/list': inventory,
  'rebon/clientModules': clientModules,
};

/// Answers one remote call in dsh's wire shape. Never throws: a refusal is a
/// result the client reads, as dsh's gateway answers.
export async function remote(endpoint, args = {}) {
  const handler = Object.hasOwn(ENDPOINTS, endpoint) ? ENDPOINTS[endpoint] : undefined;
  if (handler === undefined) {
    return {
      ok: false,
      error: { code: 'gateway/method-unavailable', message: `no live service answers ${JSON.stringify(String(endpoint))} in this composition`, details: {} },
    };
  }
  try {
    return { ok: true, value: (await handler(args ?? {})) ?? null };
  } catch (error) {
    const code = error instanceof RemoteError ? error.code : 'gateway/internal';
    return { ok: false, error: { code, message: String(error?.message ?? error), details: error?.details ?? {} } };
  }
}

/// Whether this realm answers `endpoint`, so the plane routes to it.
export function answers(endpoint) {
  return Object.hasOwn(ENDPOINTS, endpoint);
}
