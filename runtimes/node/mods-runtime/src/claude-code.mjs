// The `claude-code` module a mod imports at run time.
//
// The type imports (`import type { Register } from 'claude-code'`) are
// erased before this file is reached. What survives are the value imports:
// `h` and `Fragment`, which JSX compiles against (also installed as globals,
// since a hooks module may use JSX without importing anything), and the
// state library — `atom`, `derive`, `read`, `update`, `memberOf` — which the
// mods reference says comes from the same import.
//
// An element is plain data: `{ type, props, children }`, nothing a surface
// cannot read as JSON. Handler props (`onPress`, `onInput`, ...) are kept by
// the mod's own render registry and never leave the process; see
// `render.mjs`.

/// The marker the surface table's constructors leave on what they build.
export const ELEMENT = Symbol.for('rebon.mod.element');

export function Fragment(props) {
  return { [ELEMENT]: true, type: 'Fragment', props: {}, children: normalizeChildren(props?.children) };
}

/// JSX factory. `type` is a constructor from `$.ui.resolve(e)` (a function),
/// `Fragment`, or — refused later by validation — a string tag.
export function h(type, props, ...children) {
  const flat = normalizeChildren(children);
  const given = props === null || props === undefined ? {} : { ...props };
  if (typeof type === 'function') return type({ ...given, children: flat });
  return { [ELEMENT]: true, type: String(type), props: given, children: flat };
}

/// Children as a flat list of elements and strings; `null`, `undefined` and
/// booleans draw nothing, numbers draw as text.
export function normalizeChildren(children) {
  const out = [];
  const push = (child) => {
    if (child === null || child === undefined || typeof child === 'boolean') return;
    if (Array.isArray(child)) { child.forEach(push); return; }
    if (typeof child === 'number' || typeof child === 'bigint') { out.push(String(child)); return; }
    if (typeof child === 'string') { out.push(child); return; }
    if (typeof child === 'object' && child[ELEMENT]) {
      if (child.type === 'Fragment') { child.children.forEach(push); return; }
      out.push(child);
      return;
    }
    out.push(String(child));
  };
  push(children);
  return out;
}

if (typeof globalThis.h !== 'function') globalThis.h = h;
if (typeof globalThis.Fragment !== 'function') globalThis.Fragment = Fragment;

// ---- the state library -----------------------------------------------

const ATOM = Symbol.for('rebon.mod.atom');
const DERIVED = Symbol.for('rebon.mod.derived');

/// A named value in `$.state` with an initial value, by typed reference.
export function atom(ref, initial, options = {}) {
  return Object.freeze({ [ATOM]: true, ref: Object.freeze({ ...ref }), initial, options: Object.freeze({ ...options }) });
}

/// A value computed from other atoms while drawing.
export function derive(sources, compute) {
  return Object.freeze({ [DERIVED]: true, sources: Object.freeze([...sources]), compute });
}

/// A member of a state family, named by `e`'s instance.
export function memberOf(family, e) {
  const id = typeof e === 'string' ? e : e?.requestId ?? e?.id;
  return atom({ ...family.ref, id }, family.initial, family.options);
}

/// Reads an atom (or a derived value) through `$`, subscribing the drawing.
export async function read($, source) {
  if (source?.[DERIVED]) {
    const values = await Promise.all(source.sources.map((inner) => read($, inner)));
    return source.compute(...values);
  }
  if (!source?.[ATOM]) throw new TypeError('read() takes an atom or a derived value');
  const { value } = await $.state.get(source.ref);
  return value === undefined ? source.initial : value;
}

/// Reads, applies and writes with a version check, again on a miss, so two
/// updates before a redraw both land.
export async function update($, source, fn) {
  if (!source?.[ATOM]) throw new TypeError('update() takes an atom');
  for (let attempt = 0; attempt < 8; attempt += 1) {
    const current = await $.state.get(source.ref);
    const value = fn(current.value === undefined ? source.initial : current.value);
    const written = await $.state.set(source.ref, value, { ifVersion: current.version });
    if (written.isWritten !== false) return written.value;
  }
  throw new Error(`update(): ${source.ref.plugin}.${source.ref.key} kept changing underneath`);
}

export function isAtom(value) {
  return Boolean(value?.[ATOM]);
}
