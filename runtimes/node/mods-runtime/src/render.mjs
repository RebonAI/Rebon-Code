// What a render hook draws with, and what leaves the process.
//
// `$.ui.resolve(e)` answers the element table of the surface `e` is drawn
// on: a frozen object of constructors, the JSX tags a hook destructures.
// Each constructor builds plain data. Before a tree crosses to rebon it is
// serialised here: handler props are taken off and remembered by the
// element's `key` for the instance (`requestId`) that drew them, so a press
// reported by the surface finds its closure, and everything else is JSON.
//
// The tables mirror `Elements` in the Claude Code declarations: the
// terminal alone has `Raster` and `Image`, the mobile table has no `Input`,
// `Select` or `Client`, and `vscode` has no `Client`. A surface that cannot
// draw an element it is handed says so on its own side; the table here is
// what lets `e.surface` narrow the constructors a hook may reach for.
import { ELEMENT, normalizeChildren } from './claude-code.mjs';

const HANDLER_PROPS = Object.freeze(['onPress', 'onInput', 'onSubmit', 'onSelect', 'onLinkPress', 'onChange']);

const TABLES = Object.freeze({
  terminal: ['Box', 'Text', 'Button', 'Input', 'Select', 'Link', 'Code', 'Markdown', 'Client', 'Raster', 'Image'],
  desktop: ['Box', 'Text', 'Button', 'Input', 'Select', 'Svg', 'Link', 'Code', 'Markdown', 'Client'],
  mobile: ['Box', 'Text', 'Button', 'Svg', 'Link', 'Code', 'Markdown'],
  vscode: ['Box', 'Text', 'Button', 'Input', 'Select', 'Svg', 'Link', 'Code', 'Markdown'],
});

export const SURFACES = Object.freeze(Object.keys(TABLES));

function constructor(type) {
  const build = (props = {}) => {
    const { children, ...rest } = props;
    return { [ELEMENT]: true, type, props: rest, children: normalizeChildren(children) };
  };
  Object.defineProperty(build, 'name', { value: type });
  return build;
}

const tables = new Map();
const clientTables = new Map();

/// The frozen element table for `surface`; `terminal` when unknown.
export function elementTable(surface) {
  const name = TABLES[surface] ? surface : 'terminal';
  if (!tables.has(name)) {
    const table = {};
    for (const type of TABLES[name]) table[type] = constructor(type);
    tables.set(name, Object.freeze(table));
  }
  return tables.get(name);
}

/// The table a `Client`'s surface module draws with: its surface's,
/// without `Client` (a surface module draws no Client of its own).
export function clientElementTable(surface) {
  const name = TABLES[surface] ? surface : 'terminal';
  if (!clientTables.has(name)) {
    const { Client: _client, ...rest } = elementTable(name);
    clientTables.set(name, Object.freeze(rest));
  }
  return clientTables.get(name);
}

/// Remembers handler closures per drawn instance.
export class HandlerRegistry {
  #byInstance = new Map();

  /// Serialises `tree` for `requestId`, keeping its handlers here. A
  /// `Client` is handed to `drawClient`, and what that returns is drawn as
  /// its one child; without it a Client goes out undrawn.
  serialize(requestId, tree, drawClient) {
    const handlers = new Map();
    let autoKey = 0;
    const walk = (node) => {
      if (typeof node === 'string') return node;
      if (!node || !node[ELEMENT]) return String(node);
      if (node.type === 'Client') {
        const props = { key: String(node.props.key ?? ''), module: String(node.props.module ?? '') };
        for (const name of ['width', 'height', 'flexGrow']) {
          if (node.props[name] !== undefined) props[name] = node.props[name];
        }
        const drawn = typeof drawClient === 'function' ? drawClient(node) : undefined;
        return { type: 'Client', props, children: drawn === undefined ? [] : [walk(drawn)] };
      }
      const props = {};
      let key = node.props.key;
      const own = {};
      for (const [name, value] of Object.entries(node.props)) {
        if (HANDLER_PROPS.includes(name) && typeof value === 'function') { own[name] = value; continue; }
        if (value === undefined) continue;
        if (typeof value === 'function') continue;
        props[name] = value;
      }
      if (Object.keys(own).length > 0) {
        if (key === undefined || key === null || key === '') {
          key = `auto-${++autoKey}`;
          props.key = key;
        }
        handlers.set(String(key), own);
      }
      return { type: node.type, props, children: node.children.map(walk) };
    };
    const serialized = walk(tree);
    this.#byInstance.set(requestId, handlers);
    return serialized;
  }

  handler(requestId, key, name) {
    return this.#byInstance.get(requestId)?.get(String(key))?.[name];
  }

  forget(requestId) {
    this.#byInstance.delete(requestId);
  }

  clear() {
    this.#byInstance.clear();
  }
}

/// Whether `value` is an element a constructor built (a tree's root).
export function isElement(value) {
  return Boolean(value && typeof value === 'object' && value[ELEMENT]);
}
