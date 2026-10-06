// One loaded mod: its hooks module, its chain, its `$`, and the service
// the plane drives it through.
//
// A mod is a plane plugin of one shape. It registers exactly one service,
// `mod`, and one scope handler. The service is how rebon reaches the chain
// — a classic hook event, a render ask, a press, a command, one of its own
// tools — and the scope handler is where `$` binds to a session: once the
// scope opens `session.start` fires, and from then on `$`'s seat calls have
// somewhere to go.
//
// The declarations a mod loads against come from the manifest rebon
// synthesised by scanning the hooks module (`$claudeMod` in the load
// request's config): the commands and tools it names there are registered
// with the host's registrar here, so the plane projects them like any
// plugin's, and a `$.command.register` / `$.tool.register` at run time
// refines a declared one rather than adding a name the ceiling lacks.
import { pathToFileURL } from 'node:url';
import path from 'node:path';
import { HookChain } from './chain.mjs';
import { buildEngine } from './engine.mjs';
import { dispatchClassic } from './classic.mjs';
import { HandlerRegistry, isElement } from './render.mjs';
import { ClientHost } from './client.mjs';
import { registerModRoot, unregisterModRoot } from './transpile.mjs';

/// The one service every mod registers.
export const MOD_SERVICE = 'mod';

class ModError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
    this.name = 'ModError';
  }
}

const SURFACES = new Set(['tui', 'desktop', 'acp', 'web', 'mobile']);

function diagnostic(level, message) {
  process.stderr.write(`${JSON.stringify({ level, message: String(message).replace(/[\r\n]+/g, ' ').slice(0, 512) })}\n`);
}

export class Mod {
  constructor({ pluginId, root, entry, marker }) {
    this.pluginId = pluginId;
    this.root = path.resolve(root);
    this.entry = entry;
    this.name = marker.name ?? pluginId;
    this.version = marker.version;
    this.options = Object.freeze({ ...(marker.options ?? {}) });
    this.declaredCommands = marker.commands ?? [];
    this.declaredTools = marker.tools ?? [];
    this.chain = new HookChain({ onFailure: (failure) => this.#hookFailed(failure) });
    this.timers = new Set();
    this.pendingCalls = new Map();
    this.handlers = new HandlerRegistry();
    this.clients = new ClientHost(this);
    this.scopeCtx = null;
    this.facts = { cwd: this.root, sessionId: undefined, model: undefined, surface: undefined, turns: 0 };
    this.registeredSpecs = { commands: new Map(), tools: new Map() };
    this.engine = buildEngine(this);
  }

  /// Imports the hooks module and runs its `register`.
  async load(importModule = (url) => import(url)) {
    registerModRoot(this.root);
    const file = path.resolve(this.root, this.entry);
    let module;
    try {
      module = await importModule(pathToFileURL(file).href);
    } catch (cause) {
      throw new ModError('[ENTRY_FAILED]', `hooks module ${this.entry} failed to load: ${cause?.message ?? cause}`);
    }
    const register = typeof module?.register === 'function' ? module.register : module?.default?.register;
    if (typeof register !== 'function') {
      throw new ModError('[NO_REGISTER]', `hooks module ${this.entry} exports no register function`);
    }
    const on = (pattern, matcherOrHook, maybeHook) => this.chain.on(pattern, matcherOrHook, maybeHook);
    try {
      await register(on, this.options);
    } catch (cause) {
      throw new ModError('[REGISTER_FAILED]', `register() of ${this.name} failed: ${cause?.message ?? cause}`);
    }
  }

  /// The registrar shape the host keeps for this plugin.
  sealed() {
    const serviceHandlers = new Map([[MOD_SERVICE, (request, ctx) => this.handleService(request, ctx)]]);
    const toolHandlers = new Map();
    const tools = [];
    for (const declared of this.declaredTools) {
      const name = declared.name;
      tools.push({ name, description: declared.description || `Tool ${name} of the mod ${this.name}`, inputSchema: declared.inputSchema ?? { type: 'object' } });
      toolHandlers.set(name, (request, ctx) => this.serveTool(name, request, ctx));
    }
    const commandHandlers = new Map();
    const commands = [];
    for (const declared of this.declaredCommands) {
      const name = declared.name;
      const definition = {
        name,
        description: declared.description || `/${name}, a command of the mod ${this.name}`,
        kind: { type: 'prompt' },
        surfaces: ['tui', 'desktop', 'acp', 'web', 'mobile'],
      };
      if (declared.argumentHint) definition.hint = declared.argumentHint;
      commands.push(Object.freeze(definition));
      commandHandlers.set(name, (request, ctx) => this.serveCommand(name, request, ctx));
    }
    return Object.freeze({
      services: Object.freeze([MOD_SERVICE]),
      eventTopics: Object.freeze([]),
      llmProviders: Object.freeze([]),
      llmAdapterInfo: Object.freeze({}),
      tools: Object.freeze(tools),
      commands: Object.freeze(commands),
      commandHandlers,
      serviceHandlers,
      topicHandlers: new Map(),
      llmAdapters: new Map(),
      toolHandlers,
      scopeHandlers: Object.freeze([(ctx) => this.onScopeOpen(ctx)]),
    });
  }

  dispose() {
    for (const timer of this.timers) { clearTimeout(timer); clearInterval(timer); }
    this.timers.clear();
    for (const pending of this.pendingCalls.values()) {
      clearTimeout(pending.expiry);
      pending.settle({ result: { error: 'the mod was unloaded' }, isError: true });
    }
    this.pendingCalls.clear();
    this.handlers.clear();
    this.clients.disposeAll();
    this.scopeCtx = null;
    unregisterModRoot(this.root);
  }

  // ---- the scope ------------------------------------------------------

  async onScopeOpen(ctx) {
    this.scopeCtx = ctx;
    if (ctx.workspaceRoot) this.facts.cwd = ctx.workspaceRoot;
    try {
      await this.chain.dispatch('session.start', { cwd: this.facts.cwd }, async () => ({ cwd: this.facts.cwd }), this.dispatchOptions());
    } catch (error) {
      this.report('session.start', error);
    }
    return () => { if (this.scopeCtx === ctx) this.scopeCtx = null; };
  }

  seat(method, params) {
    const ctx = this.scopeCtx;
    if (!ctx) return Promise.reject(new ModError('[NO_SESSION]', `$.${method} was called before the mod ${this.name} was bound to a session`));
    return ctx.seat('mods', method, { ...params });
  }

  sessionFact(name) {
    return this.facts[name];
  }

  dispatchOptions(extra = {}) {
    return { engine: this.engine, origin: { kind: 'engine' }, ...extra };
  }

  report(where, error) {
    const message = `${this.name}: ${where}: ${error?.message ?? error}`;
    diagnostic('warn', message);
    if (this.scopeCtx) void this.seat('ui.log', { text: message, to: 'debug' }).catch(() => {});
  }

  log(text, to = 'transcript') {
    if (this.scopeCtx) void this.seat('ui.log', { text, to }).catch(() => {});
    else diagnostic('info', `${this.name}: ${text}`);
  }

  #hookFailed({ event, pattern, error, inCatch }) {
    this.report(`${event} (hook on ${pattern}${inCatch ? ', its .catch' : ''})`, error);
  }

  // ---- registrations at run time ----------------------------------------

  ownToolName(name) {
    return name.startsWith('mcp__') ? name : `mcp__${this.name}__${name}`;
  }

  async registerTool(spec) {
    if (!spec || typeof spec.name !== 'string') throw new ModError('[WRONG_SHAPE]', '$.tool.register needs { name, description, inputSchema }');
    const full = this.ownToolName(spec.name);
    if (!this.declaredTools.some((tool) => tool.name === full)) {
      throw new ModError('[UNDECLARED_TOOL]', `tool ${spec.name} is not one the hooks module names in a $.tool.register({ name: "..." }) literal; spell it as one`);
    }
    this.registeredSpecs.tools.set(full, spec);
    return this.seat('tool.register', { name: full, description: spec.description, inputSchema: spec.inputSchema ?? spec.input_schema ?? { type: 'object' } });
  }

  async registerCommand(spec) {
    if (!spec || typeof spec.name !== 'string') throw new ModError('[WRONG_SHAPE]', '$.command.register needs { name, description }');
    if (!this.declaredCommands.some((command) => command.name === spec.name)) {
      throw new ModError('[UNDECLARED_COMMAND]', `command ${spec.name} is not one the hooks module names in a $.command.register({ name: "..." }) literal; spell it as one`);
    }
    this.registeredSpecs.commands.set(spec.name, spec);
    return this.seat('command.register', { name: spec.name, description: spec.description, argumentHint: spec.argumentHint ?? null });
  }

  /// `$.tool.call`: one of this mod's own tools runs through its chain;
  /// any other goes to rebon through the scope's `tool/invoke`.
  async callTool(input) {
    const tool = input?.tool;
    if (typeof tool !== 'string') throw new ModError('[WRONG_SHAPE]', '$.tool.call needs { tool, input }');
    if (this.declaredTools.some((declared) => declared.name === tool)) {
      const { result } = await this.chain.dispatch('tool.call', { tool, input: input.input ?? {}, origin: { kind: 'plugin', name: this.name } }, async () => ({ deny: `no hook of ${this.name} answers ${tool}` }), this.dispatchOptions());
      return result;
    }
    const ctx = this.scopeCtx;
    if (!ctx) throw new ModError('[NO_SESSION]', `$.tool.call(${tool}) was made before the mod was bound to a session`);
    try {
      const result = await ctx.invoke(tool, input.input ?? {});
      return { result, isError: false, ref: { tool } };
    } catch (error) {
      return { deny: error?.message ?? String(error) };
    }
  }

  // ---- what the plane asks ----------------------------------------------

  async serveTool(name, request, ctx) {
    this.scopeCtx ??= ctx;
    const e = { tool: name, input: request?.input ?? {}, tool_use_id: request?.toolUseId ?? null, origin: { kind: 'model' } };
    const { result } = await this.chain.dispatch('tool.call', e, async () => ({ deny: `no hook of ${this.name} answers the tool ${name}` }), this.dispatchOptions({ signal: ctx?.signal }));
    if (result !== null && typeof result === 'object' && typeof result.deny === 'string') {
      throw new ModError('[TOOL_DENIED]', result.deny);
    }
    return result?.result ?? result ?? null;
  }

  async serveCommand(name, request, ctx) {
    this.scopeCtx ??= ctx;
    const answer = await this.runCommand({ command: name, args: request?.rest ?? '', raw: request?.raw ?? `/${name}`, surface: request?.surface }, ctx);
    return typeof answer === 'string' ? answer : answer?.text ?? '';
  }

  async runCommand({ command, args, raw, surface }, ctx) {
    const e = {
      command,
      args: args ?? '',
      raw: raw ?? `/${command}${args ? ` ${args}` : ''}`,
      origin: { kind: 'user' },
      presentation: { surface: SURFACES.has(surface) ? surface : 'tui', isFullscreen: surface !== 'tui' },
    };
    const { result } = await this.chain.dispatch('command.run', e, async () => ({ text: '' }), this.dispatchOptions({ signal: ctx?.signal }));
    if (typeof result === 'string') return { text: result };
    if (result && typeof result === 'object') return { text: typeof result.text === 'string' ? result.text : '', context: result.context };
    return { text: '' };
  }

  async render({ component, surface, requestId, props, viewport }) {
    const e = { component, surface: surface ?? 'terminal', requestId, props: props ?? {}, viewport: viewport ?? undefined };
    const engineDraws = Symbol('engine');
    const { result } = await this.chain.dispatch('ui.render', e, async () => engineDraws, this.dispatchOptions());
    if (result === engineDraws || result === undefined || result === null) {
      this.clients.forget(requestId);
      return { engine: true };
    }
    if (isElement(result)) {
      const context = { component, surface: e.surface, viewport: e.viewport, requestId };
      await this.clients.prepare(requestId, result, context);
      const drawn = new Set();
      const tree = this.handlers.serialize(requestId, result, (node) => {
        drawn.add(String(node.props.key ?? ''));
        return this.clients.draw(requestId, node, context);
      });
      this.clients.sweep(requestId, drawn);
      return { tree };
    }
    if (typeof result === 'string') {
      this.clients.forget(requestId);
      return { tree: { type: 'Text', props: {}, children: [result] } };
    }
    return { error: `the ui.render hook of ${this.name} answered ${typeof result}, not a tree` };
  }

  async act(kind, request) {
    const { component, surface, requestId, element } = request;
    const base = { plugin: this.name, element, component, surface: surface ?? 'terminal', requestId };
    const handlerName = kind === 'press' ? 'onPress' : kind === 'select' ? 'onSelect' : request.inputKind === 'change' ? 'onInput' : 'onSubmit';
    const core = async (final) => {
      const handler = this.handlers.handler(requestId, element, handlerName) ?? (kind === 'press' ? this.handlers.handler(requestId, element, 'onLinkPress') : undefined);
      if (typeof handler === 'function') {
        try {
          await handler(kind === 'press' ? { element, href: final.href } : final.value, final);
        } catch (error) {
          this.report(`ui.${kind} (${handlerName} of ${element})`, error);
        }
      }
      return kind === 'press' ? { element } : { element, value: final.value };
    };
    const e = kind === 'press'
      ? { ...base, href: request.href }
      : kind === 'input'
        ? { ...base, kind: request.inputKind ?? 'submit', value: request.value ?? '' }
        : { ...base, value: request.value };
    const { result } = await this.chain.dispatch(`ui.${kind}`, e, core, this.dispatchOptions());
    return result ?? {};
  }

  /// The site's focus ring about to move onto `element` (absent: off the
  /// mod's elements), raised as `ui.focus`: `{ deny }` keeps the ring where
  /// it is, else `{ element }` is where it lands, which a hook may rewrite.
  async focus(request) {
    const e = {
      component: request.component,
      requestId: request.requestId,
      origin: request.origin?.kind === 'plugin' ? { kind: 'plugin', name: String(request.origin.name ?? this.name) } : { kind: 'person' },
    };
    if (typeof request.element === 'string') {
      e.plugin = this.name;
      e.element = request.element;
    }
    const { result } = await this.chain.dispatch('ui.focus', e, async (final) => ({ element: final.element ?? null }), this.dispatchOptions());
    if (result && typeof result.deny === 'string') return { deny: result.deny };
    const element = result && 'element' in result ? result.element : e.element ?? null;
    return { element: typeof element === 'string' ? element : null };
  }

  describe() {
    return {
      name: this.name,
      version: this.version ?? null,
      root: this.root,
      patterns: this.chain.patterns(),
      commands: this.declaredCommands.map((command) => command.name),
      tools: this.declaredTools.map((tool) => tool.name),
      pendingToolCalls: this.pendingCalls.size,
      timers: this.timers.size,
      clients: this.clients.size,
    };
  }

  async handleService(request, ctx) {
    if (request === null || typeof request !== 'object') throw new ModError('[WRONG_SHAPE]', 'a mod service request is an object with a kind');
    if (ctx && !this.scopeCtx) this.scopeCtx = ctx;
    switch (request.kind) {
      case 'classic': {
        if (request.input?.cwd) this.facts.cwd = request.input.cwd;
        if (request.input?.session_id) this.facts.sessionId = request.input.session_id;
        return dispatchClassic(this, request);
      }
      case 'facts':
        Object.assign(this.facts, request.facts ?? {});
        return {};
      case 'render': return this.render(request);
      case 'press': return this.act('press', request);
      case 'input': return this.act('input', request);
      case 'select': return this.act('select', request);
      case 'command': return this.runCommand(request, ctx);
      case 'describe': return this.describe();
      case 'forget':
        this.handlers.forget(request.requestId);
        this.clients.forget(request.requestId);
        return {};
      case 'focus': return this.focus(request);
      case 'clientKey': return this.clients.key(request.requestId, request.element, request.key ?? {});
      case 'clientPointer': return this.clients.pointer(request.requestId, request.element, request.pointer ?? {});
      default:
        throw new ModError('[UNKNOWN_KIND]', `mod service does not know the kind ${JSON.stringify(request.kind)}`);
    }
  }
}
