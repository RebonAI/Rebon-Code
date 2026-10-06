// The `Client` element: a mod's surface module, run here, drawn there.
//
// In Claude Code a `Client` runs its surface module on the terminal's own
// drawing thread. Rebon's surfaces are not JavaScript, so the instance lives
// where the mod does, in this process: its function, its local state, its
// timers and its key listener. When the mod's render hook draws a tree
// holding a `<Client key module props width height />`, the instance under
// that key is called with its props and the region it was given, and what it
// returns travels to the surface as the Client's one child. A `setState`, a
// tick or a key asks the surface to draw the site again (`ui.invalidate`),
// which runs the render hook and the instance once more with its state kept.
//
// The surface hands back what the person does to the region: keys while the
// Client holds the site's focus ring (`clientKey`), clicks (`clientPointer`).
// `surface.post(data)` raises `ui.message` on the mod's chain; a hook that
// answers `{ props }` hands the instance its next props until the render
// hook draws different ones.
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { clientElementTable, isElement } from './render.mjs';
import { resolveModFile } from './transpile.mjs';

/// Renders in a row that each `setState` without a key, pointer, tick or
/// new props between: a render loop, and the instance unmounts.
const RENDER_LOOP_STREAK = 3;
/// The least interval `surface.every` runs at.
const MIN_TICK_MS = 16;
/// How much a post may hold, serialised.
const MAX_POST_CHARS = 100_000;

function instanceId(requestId, key) {
  return `${requestId}\u0000${key}`;
}

/// The cells a `width` / `height` prop gives out of `available`: a count, a
/// percentage of it, or all of it when the prop says neither.
export function regionCells(value, available) {
  const room = Math.max(0, Math.floor(available ?? 0));
  if (typeof value === 'number' && Number.isFinite(value)) return Math.max(0, Math.min(room, Math.floor(value)));
  if (typeof value === 'string' && value.trim().endsWith('%')) {
    const percent = Number.parseFloat(value);
    if (Number.isFinite(percent)) return Math.max(0, Math.floor((room * Math.min(100, percent)) / 100));
  }
  return room;
}

/// The function a surface module exports: its default, else its one
/// PascalCase export.
export function moduleFunction(namespace) {
  if (typeof namespace?.default === 'function') return namespace.default;
  const named = Object.entries(namespace ?? {}).filter(([name, value]) => /^[A-Z]/.test(name) && typeof value === 'function');
  return named.length === 1 ? named[0][1] : undefined;
}

export class ClientHost {
  #instances = new Map();
  #invalidateQueued = false;

  constructor(mod, { importModule = (url) => import(url) } = {}) {
    this.mod = mod;
    this.importModule = importModule;
  }

  /// Loads the surface module of every `Client` in `tree` that has no
  /// running instance yet, ahead of the drawing, which is synchronous.
  async prepare(requestId, tree, context) {
    for (const node of clientNodes(tree)) {
      const key = String(node.props.key ?? '');
      const module = String(node.props.module ?? '');
      if (!key || !module) continue;
      const id = instanceId(requestId, key);
      const held = this.#instances.get(id);
      if (held && held.module === module) continue;
      if (held) this.#unmount(held);
      const instance = this.#newInstance(requestId, key, module, context);
      this.#instances.set(id, instance);
      try {
        instance.fn = await this.#load(module);
      } catch (error) {
        instance.failed = error?.message ?? String(error);
        this.mod.report(`Client ${module}`, error);
      }
    }
  }

  /// What the instance under `node`'s key draws now, as an element.
  draw(requestId, node, context) {
    const key = String(node.props.key ?? '');
    const instance = this.#instances.get(instanceId(requestId, key));
    const { Text } = clientElementTable(context.surface);
    if (!instance) return Text({ dimColor: true, children: `Client ${key} is not loaded` });
    instance.context = context;
    instance.columns = regionCells(node.props.width, context.viewport?.columns);
    instance.rows = regionCells(node.props.height, context.viewport?.rows);
    const treeProps = JSON.stringify(node.props.props ?? null);
    if (treeProps !== instance.lastTreeProps) {
      instance.lastTreeProps = treeProps;
      instance.props = node.props.props;
      instance.propsOverride = undefined;
      instance.loopStreak = 0;
    }
    if (instance.failed !== undefined) {
      return Text({ dimColor: true, children: `${instance.module} stopped: ${instance.failed}` });
    }
    instance.setStateInDraw = false;
    instance.drawing = true;
    let drawn;
    try {
      drawn = instance.fn(instance.propsOverride !== undefined ? instance.propsOverride : instance.props, instance.surface);
    } catch (error) {
      this.#fail(instance, error);
      return Text({ dimColor: true, children: `${instance.module} stopped: ${instance.failed}` });
    } finally {
      instance.drawing = false;
    }
    instance.loopStreak = instance.setStateInDraw ? instance.loopStreak + 1 : 0;
    if (instance.loopStreak >= RENDER_LOOP_STREAK) {
      this.#fail(instance, new Error(`setState on ${RENDER_LOOP_STREAK} renders in a row with nothing between: a render loop`));
      return Text({ dimColor: true, children: `${instance.module} stopped: ${instance.failed}` });
    }
    if (typeof drawn === 'string') return Text({ children: drawn });
    if (!isElement(drawn)) return Text({ dimColor: true, children: '' });
    return drawn;
  }

  /// Unmounts the instances under `requestId` its latest drawing no longer
  /// holds.
  sweep(requestId, keys) {
    for (const instance of [...this.#instances.values()]) {
      if (instance.requestId === requestId && !keys.has(instance.key)) this.#unmount(instance);
    }
  }

  /// Unmounts every instance of one site (its pane closed).
  forget(requestId) {
    this.sweep(requestId, new Set());
  }

  disposeAll() {
    for (const instance of [...this.#instances.values()]) this.#unmount(instance);
  }

  /// A key the person pressed while the Client held the focus ring.
  key(requestId, element, event) {
    const instance = this.#instances.get(instanceId(requestId, String(element)));
    if (!instance || instance.failed !== undefined || typeof instance.keyFn !== 'function') return { heard: false };
    instance.loopStreak = 0;
    try {
      instance.keyFn(Object.freeze({ ...keyEvent(event) }));
    } catch (error) {
      this.#fail(instance, error);
    }
    return { heard: true };
  }

  /// A pointer event over the Client's region, region-relative.
  pointer(requestId, element, event) {
    const instance = this.#instances.get(instanceId(requestId, String(element)));
    if (!instance || instance.failed !== undefined || typeof instance.pointerFn !== 'function') return { heard: false };
    instance.loopStreak = 0;
    try {
      instance.pointerFn(Object.freeze({ ...event }));
    } catch (error) {
      this.#fail(instance, error);
    }
    return { heard: true };
  }

  /// How many instances run; for tests and `describe`.
  get size() {
    return this.#instances.size;
  }

  // ---- inside ----------------------------------------------------------

  async #load(module) {
    const base = path.resolve(this.mod.root, path.dirname(this.mod.entry));
    const wanted = path.resolve(base, module);
    const root = this.mod.root.endsWith(path.sep) ? this.mod.root : this.mod.root + path.sep;
    if (!wanted.startsWith(root)) throw new Error(`[PATH_ESCAPES] the Client module ${module} is outside the mod's folder`);
    const file = resolveModFile(wanted);
    if (file === null) throw new Error(`[NO_MODULE] the Client module ${module} names no file`);
    const namespace = await this.importModule(pathToFileURL(file).href);
    const fn = moduleFunction(namespace);
    if (typeof fn !== 'function') throw new Error(`[NO_COMPONENT] ${module} exports no default function and no one PascalCase function`);
    return fn;
  }

  #newInstance(requestId, key, module, context) {
    const host = this;
    const instance = {
      requestId, key, module, context,
      fn: undefined, failed: undefined,
      state: undefined, props: undefined, propsOverride: undefined, lastTreeProps: undefined,
      columns: 0, rows: 0,
      timers: new Set(), keyFn: undefined, pointerFn: undefined,
      drawing: false, setStateInDraw: false, loopStreak: 0,
      pendingPost: undefined, postQueued: false,
    };
    instance.surface = Object.freeze({
      get elements() { return clientElementTable(instance.context?.surface); },
      get state() { return instance.state; },
      get columns() { return instance.columns; },
      get rows() { return instance.rows; },
      setState(next) {
        instance.state = next;
        if (instance.drawing) instance.setStateInDraw = true;
        host.#invalidate();
      },
      every(ms, fn) {
        const timer = setInterval(() => {
          if (instance.failed !== undefined) return;
          instance.loopStreak = 0;
          try { fn(); } catch (error) { host.#fail(instance, error); }
        }, Math.max(MIN_TICK_MS, Number(ms) || 0));
        instance.timers.add(timer);
        return () => { clearInterval(timer); instance.timers.delete(timer); };
      },
      onKey(fn) {
        instance.keyFn = fn;
        return () => { if (instance.keyFn === fn) instance.keyFn = undefined; };
      },
      onPointer(fn) {
        instance.pointerFn = fn;
        return () => { if (instance.pointerFn === fn) instance.pointerFn = undefined; };
      },
      post(data) {
        let text;
        try { text = JSON.stringify(data); } catch { return; }
        if (text === undefined || text.length > MAX_POST_CHARS) return;
        instance.pendingPost = JSON.parse(text);
        if (instance.postQueued) return;
        instance.postQueued = true;
        setTimeout(() => host.#deliver(instance), 0);
      },
    });
    return instance;
  }

  async #deliver(instance) {
    instance.postQueued = false;
    if (instance.failed !== undefined || !this.#instances.has(instanceId(instance.requestId, instance.key))) return;
    const data = instance.pendingPost;
    instance.pendingPost = undefined;
    const e = {
      surface: instance.context?.surface ?? 'terminal',
      component: instance.context?.component ?? 'Pane',
      requestId: instance.requestId,
      element: instance.key,
      module: instance.module,
      data,
    };
    try {
      const { result } = await this.mod.chain.dispatch('ui.message', e, async () => ({}), this.mod.dispatchOptions());
      if (result && typeof result === 'object' && result.props !== undefined) {
        instance.propsOverride = result.props;
        instance.loopStreak = 0;
        this.#invalidate();
      }
    } catch (error) {
      this.mod.report(`ui.message from ${instance.module}`, error);
    }
  }

  #fail(instance, error) {
    instance.failed = error?.message ?? String(error);
    this.#stopTimers(instance);
    this.mod.report(`Client ${instance.module}`, error);
    this.#invalidate();
  }

  #stopTimers(instance) {
    for (const timer of instance.timers) clearInterval(timer);
    instance.timers.clear();
  }

  #unmount(instance) {
    this.#stopTimers(instance);
    instance.keyFn = undefined;
    instance.pointerFn = undefined;
    this.#instances.delete(instanceId(instance.requestId, instance.key));
  }

  /// One redraw of the mod's sites per turn of the event loop, however many
  /// instances asked.
  #invalidate() {
    if (this.#invalidateQueued) return;
    this.#invalidateQueued = true;
    setTimeout(() => {
      this.#invalidateQueued = false;
      if (this.mod.scopeCtx) void this.mod.seat('ui.invalidate', { event: 'ui.render' }).catch(() => {});
    }, 0);
  }
}

/// The `Client` elements in a tree a render hook returned, in order.
export function clientNodes(tree) {
  const out = [];
  const walk = (node) => {
    if (!isElement(node)) return;
    if (node.type === 'Client') { out.push(node); return; }
    for (const child of node.children ?? []) walk(child);
  };
  walk(tree);
  return out;
}

/// A key as `ClientKeyEvent` spells it: the name, and only the modifiers
/// held.
function keyEvent(event) {
  const out = { key: String(event?.key ?? '') };
  if (event?.ctrl) out.ctrl = true;
  if (event?.shift) out.shift = true;
  if (event?.meta) out.meta = true;
  return out;
}
