// The composition realm: one Cordis context, shared by every entry.
//
// This is the whole reason the composition exists rather than each plugin
// living alone. Cordis plugins reach each other through services on a context —
// `inject: ['tools']`, `ctx.llm.registerAdapter(...)` — and that only works if
// they are mounted on the same one. So the plane's "one plugin, one load" is
// the outside view, and inside the host they all land here.
//
// Two things live here that a flat list of plugins cannot express, and both are
// composition structure rather than plugin facts, which is why rebon describes
// them once when the composition is created rather than per load:
//
//   * **groups** — a container that is not a module. Unloading one is unloading
//     its members, which rebon does by naming them; nothing is loaded for the
//     group itself.
//   * **isolate** — Cordis's own realm narrowing. The agent loop needs its own
//     `systemPrompt` because the real dsh service and rebon's seat share that
//     name; without isolation one of them loses.
//
// Ordering note: this module imports Cordis by bare specifier, so it must not
// be imported before `installModuleResolution()` has run. Everything that
// reaches it does so through a dynamic import for exactly that reason.
import { Context } from 'cordis';
import { OWNER, RegistrationSink, SINK } from './registry.mjs';
import RebonLlmRuntime from './llm-runtime.mjs';
import RebonCredentials from './credentials-runtime.mjs';
import RebonToolsRuntime from './tools-runtime.mjs';
import RebonWebRuntime from './web-runtime.mjs';
import RebonSystemPromptRuntime from './systemprompt-runtime.mjs';

/** The built-in seats, in mount order: a dsh `inject` is satisfied at mount
 *  time, so a seat that arrives after the plugin needing it never arrives. */
const BUILTIN_SEATS = ['llm', 'credentials', 'tools', 'web', 'systemPrompt'];

let state;

class ComposeError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
    this.name = 'ComposeError';
  }
}

/// Applies one node's `isolate` declaration to its parent context.
///
/// Cordis semantics, unchanged: `true` mints an entry-local realm; a string
/// labels a realm SHARED by every node using the same label, so an in-group
/// provider serves in-group consumers while the outside world keeps its own
/// service under the same name.
function derive(parent, node, realms) {
  let derived = parent;
  if (node.isolate && typeof node.isolate === 'object') {
    for (const [service, label] of Object.entries(node.isolate)) {
      if (label === true) {
        derived = derived.isolate(service);
        continue;
      }
      const key = `${service} ${label}`;
      if (!realms.has(key)) realms.set(key, Symbol(`${service}#${label}`));
      derived = derived.isolate(service, realms.get(key));
    }
  }
  return derived;
}

/// Walks the declared structure into a placement per entry id.
///
/// The value is the context an entry is mounted *under*, not the entry's own —
/// that one is derived per load, because it carries the load's registration
/// sink and each load needs its own.
function place(nodes, parent, placements, groups, realms) {
  for (const node of nodes ?? []) {
    const id = node?.id;
    if (typeof id !== 'string' || id.length === 0) {
      throw new ComposeError('[BAD_STRUCTURE]', 'every entry in the composition structure needs an id');
    }
    if (placements.has(id) || groups.has(id)) {
      throw new ComposeError('[BAD_STRUCTURE]', `duplicate entry id ${JSON.stringify(id)}`);
    }
    const derived = derive(parent, node, realms);
    if (Array.isArray(node.group)) {
      groups.set(id, derived);
      place(node.group, derived, placements, groups, realms);
      continue;
    }
    placements.set(id, derived);
  }
}

/// Creates the realm. Called once, by the composition control plugin.
export async function createRealm({ web = null, entries = [] } = {}) {
  if (state !== undefined) {
    throw new ComposeError('[REALM_EXISTS]', 'the composition realm is already created');
  }
  const ctx = new Context();
  const placements = new Map();
  const groups = new Map();
  const realms = new Map();
  place(entries, ctx, placements, groups, realms);

  await ctx.plugin(RebonLlmRuntime);
  await ctx.plugin(RebonCredentials);
  await ctx.plugin(RebonToolsRuntime);
  await ctx.plugin(RebonWebRuntime, { web });
  await ctx.plugin(RebonSystemPromptRuntime);

  state = { ctx, placements, groups, realms, fibers: new Map(), sinks: new Map() };
  return state;
}

/// The realm, or a refusal that names the ordering mistake.
export function realm() {
  if (state === undefined) {
    throw new ComposeError(
      '[NO_REALM]',
      'the composition realm does not exist yet; rebon loads the composition control plugin before any entry',
    );
  }
  return state;
}

/// Tears the realm down. Only tests need this; a real host exits instead.
export async function destroyRealm() {
  if (state === undefined) return;
  for (const pluginId of [...state.fibers.keys()].reverse()) await disposeEntry(pluginId);
  await state.ctx.dispose?.();
  state = undefined;
}

export function builtinSeats() {
  return [...BUILTIN_SEATS];
}

/// The services a Cordis plugin cannot start without.
///
/// Cordis accepts both spellings: a bare array is all-required, an object
/// separates `required` from `optional`.
function requiredInjects(plugin) {
  const inject = plugin?.inject;
  if (Array.isArray(inject)) return inject;
  if (inject && typeof inject === 'object' && Array.isArray(inject.required)) return inject.required;
  return [];
}

/// Mounts one Cordis entry and hands back what it registered.
///
/// The sink rides on the entry's own derived context, which is what lets a seat
/// attribute a registration synchronously — the moment a plugin registers, the
/// context it registered through says which plugin it was.
export async function mountEntry(request, plugin) {
  const live = realm();
  const { pluginId } = request;
  if (live.groups.has(pluginId)) {
    throw new ComposeError(
      '[GROUP_NOT_LOADABLE]',
      `${pluginId} is a group in the composition structure, not a module; load its members`,
    );
  }
  if (live.fibers.has(pluginId)) {
    throw new ComposeError('[PLUGIN_ALREADY_LOADED]', `${pluginId} is already mounted`);
  }
  const parent = live.placements.get(pluginId) ?? live.ctx;
  const sink = new RegistrationSink(pluginId, request);
  const derived = parent.extend({ [OWNER]: pluginId, [SINK]: sink });
  // Refused before mounting rather than waited for. Cordis lets a plugin wait
  // for a service that has not arrived yet, and inside one composition that is
  // a feature — but a load is a call rebon is waiting on, and a plugin waiting
  // forever is a call that never answers. So a composition entry's required
  // services must be there when it loads, and rebon orders entries so they
  // are. Saying which one is missing turns a hang into a diagnosis.
  for (const service of requiredInjects(plugin)) {
    if (derived.get(service) === undefined) {
      throw new ComposeError(
        '[MISSING_INJECT]',
        `${pluginId} requires service ${JSON.stringify(service)}, which nothing provides where it is mounted`,
      );
    }
  }
  let fiber;
  try {
    fiber = derived.plugin(plugin, request.config ?? undefined);
    await fiber;
  } catch (cause) {
    // Whatever half-mounted has to go: a fiber left behind would keep serving
    // through registrations rebon was never told about.
    try {
      await fiber?.dispose();
    } catch {}
    if (cause?.code) throw cause;
    throw new ComposeError('[ACTIVATE_FAILED]', `mounting ${pluginId} failed: ${cause?.message ?? cause}`);
  }
  // Every catalog read has to finish before the report is sealed: a provider
  // reported without its models would have rebon resolving against a table
  // that is still being filled in.
  await live.ctx.llm.settle();
  sink.seal();
  live.fibers.set(pluginId, fiber);
  live.sinks.set(pluginId, sink);
  return sink;
}

/// Disposes one entry. Idempotent: the host retires a plugin once, but a realm
/// torn down underneath it must not turn that into a second failure.
export async function disposeEntry(pluginId) {
  if (state === undefined) return;
  const fiber = state.fibers.get(pluginId);
  state.fibers.delete(pluginId);
  state.sinks.delete(pluginId);
  if (fiber === undefined) return;
  await fiber.dispose();
}

/// What one entry reported beyond the protocol's own fields.
export function reportFor(pluginId) {
  const sink = realm().sinks.get(pluginId);
  return sink === undefined ? undefined : sink.report();
}

/// Every entry currently mounted, in load order.
export function mounted() {
  return [...realm().fibers.keys()];
}
