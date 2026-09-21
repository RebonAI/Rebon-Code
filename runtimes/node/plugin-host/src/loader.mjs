// Turning a `plugin/load` request into a loaded plugin.
//
// The registrar is the only thing a plugin's code is handed, and it enforces
// the same rule the Rust registry does from the other side: a plugin may
// register a service or a topic only if its manifest declared one. Checking it
// here as well is not redundancy for its own sake — it means the refusal names
// the plugin's own line rather than arriving as a rejected ready report after
// the module has already run and had its side effects.
//
// Registration closes when the entry module's `activate` returns. A plugin that
// registers later would be adding capabilities the host already reported as the
// complete set, and nothing downstream would know.
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { ProtocolError } from './protocol.mjs';
import { pluginCommandDefinition, pluginToolDefinition, validateName } from './methods.mjs';

/// Resolves an entry against its package root and refuses anything outside it.
///
/// The payload validator already rejects `..`, absolute entries, and
/// backslashes; this is the check that survives a symlink or a root that is
/// itself a relative-looking string, because it asks the filesystem's own
/// resolver rather than the text.
export function resolveEntry(root, entry) {
  const base = path.resolve(root);
  const resolved = path.resolve(base, entry);
  const inside = resolved === base || resolved.startsWith(base + path.sep);
  if (!inside) throw new ProtocolError('[PATH_ESCAPES]', `entry ${entry} resolves outside the package root`);
  return pathToFileURL(resolved).href;
}

class Registrar {
  #declaredServices; #declaredTopics; #declaredProviders; #declaredTools; #declaredCommands;
  #commands = new Map(); #commandHandlers = new Map();
  #services = new Map(); #topics = new Map(); #adapters = new Map();
  #adapterInfo = new Map();
  #tools = new Map(); #toolHandlers = new Map(); #scopes = []; #sealed = false;
  constructor(declaredServices, declaredTopics, declaredProviders, declaredTools, declaredCommands) {
    this.#declaredServices = new Set(declaredServices);
    this.#declaredTopics = new Set(declaredTopics);
    this.#declaredProviders = new Set(declaredProviders);
    this.#declaredTools = new Set(declaredTools);
    this.#declaredCommands = new Set(declaredCommands);
  }

  /// A command is a tool for a person: the manifest declares the name, and
  /// the registration says what it looks like in a menu. Only a `prompt`
  /// command carries a handler — an `explain` says its sentence at
  /// registration and a `panel` names a dialog, so neither has anything left
  /// to ask the plugin at invoke time.
  #admitCommand(definition, handler) {
    if (this.#sealed) throw new ProtocolError('[REGISTRATION_CLOSED]', 'a command was registered after activation finished');
    const command = pluginCommandDefinition(definition);
    if (!this.#declaredCommands.has(command.name)) throw new ProtocolError('[UNAUTHORIZED_REGISTER]', `command ${JSON.stringify(command.name)} is not declared by this plugin's manifest`);
    if (this.#commands.has(command.name)) throw new ProtocolError('[DUPLICATE_DECLARATION]', `command ${JSON.stringify(command.name)} is registered twice`);
    if (command.kind.type === 'prompt') {
      if (typeof handler !== 'function') throw new ProtocolError('[WRONG_SHAPE]', `prompt command ${JSON.stringify(command.name)} needs a function`);
      this.#commandHandlers.set(command.name, handler);
    } else if (handler !== undefined) {
      throw new ProtocolError('[WRONG_SHAPE]', `${command.kind.type} command ${JSON.stringify(command.name)} takes no handler`);
    }
    this.#commands.set(command.name, command);
  }

  /// A tool differs from a service in one way that matters: it has to be
  /// *offered* to a model, so it cannot be registered by name alone. The
  /// manifest still declares only the name — description and schema are how a
  /// tool describes itself, not what it is permitted to be.
  #admitTool(definition, handler) {
    if (this.#sealed) throw new ProtocolError('[REGISTRATION_CLOSED]', 'a tool was registered after activation finished');
    const tool = pluginToolDefinition(definition);
    if (typeof handler !== 'function') throw new ProtocolError('[WRONG_SHAPE]', `tool ${JSON.stringify(tool.name)} needs a function`);
    if (!this.#declaredTools.has(tool.name)) throw new ProtocolError('[UNAUTHORIZED_REGISTER]', `tool ${JSON.stringify(tool.name)} is not declared by this plugin's manifest`);
    if (this.#tools.has(tool.name)) throw new ProtocolError('[DUPLICATE_DECLARATION]', `tool ${JSON.stringify(tool.name)} is registered twice`);
    this.#tools.set(tool.name, tool);
    this.#toolHandlers.set(tool.name, handler);
  }

  #admit(kind, name, handler, declared, into) {
    if (this.#sealed) throw new ProtocolError('[REGISTRATION_CLOSED]', `${kind} ${JSON.stringify(name)} was registered after activation finished`);
    validateName(kind, name);
    if (typeof handler !== 'function') throw new ProtocolError('[WRONG_SHAPE]', `${kind} ${JSON.stringify(name)} needs a function`);
    if (!declared.has(name)) throw new ProtocolError('[UNAUTHORIZED_REGISTER]', `${kind} ${JSON.stringify(name)} is not declared by this plugin's manifest`);
    if (into.has(name)) throw new ProtocolError('[DUPLICATE_DECLARATION]', `${kind} ${JSON.stringify(name)} is registered twice`);
    into.set(name, handler);
  }

  /// The surface a plugin's `activate` receives. Frozen, and holding no
  /// reference back to the host: at activation a plugin registers, it does not
  /// reach.
  ///
  /// Reaching happens later and only from inside a handler, which is called as
  /// `handler(request, ctx)`. That `ctx` is bound to the scope incarnation the
  /// call arrived on and carries `invoke(tool, input)` — so a plugin can act on
  /// the session it was called for and has no way to name another.
  api() {
    return Object.freeze({
      service: (name, handler) => this.#admit('service', name, handler, this.#declaredServices, this.#services),
      topic: (name, handler) => this.#admit('topic', name, handler, this.#declaredTopics, this.#topics),
      // An llm adapter is a service under a different routing key: declared in
      // the manifest, registered here, and answered with chunks instead of one
      // value.
      //
      // The optional third argument is what the adapter says about itself —
      // which models, which default, what the provider can do. It is reported
      // once with the ready report rather than asked for per turn, because a
      // caller has to know whether a provider streams reasoning text before it
      // decides how to budget the turn it is about to send. Opaque to the
      // host: reading it is the model layer's business.
      llm: (provider, adapter, info) => {
        this.#admit('provider', provider, adapter, this.#declaredProviders, this.#adapters);
        if (info !== undefined) {
          if (info === null || typeof info !== 'object' || Array.isArray(info)) {
            throw new ProtocolError('[WRONG_SHAPE]', `adapter info for ${JSON.stringify(provider)} must be a plain object`);
          }
          this.#adapterInfo.set(provider, info);
        }
      },
      tool: (definition, handler) => this.#admitTool(definition, handler),
      command: (definition, handler) => this.#admitCommand(definition, handler),
      // Not a capability, so nothing to declare: everything a scope handle can
      // do is gated by the declarations above. What it is, is a lifetime — the
      // session, rather than one call inside it. A plugin that produces facts
      // on its own schedule (an agent loop, a watcher) has no inbound call to
      // hang them on, and requiring one would mean it could only speak when
      // spoken to.
      scope: (handler) => {
        if (this.#sealed) throw new ProtocolError('[REGISTRATION_CLOSED]', 'a scope handler was registered after activation finished');
        if (typeof handler !== 'function') throw new ProtocolError('[WRONG_SHAPE]', 'a scope handler must be a function');
        this.#scopes.push(handler);
      },
    });
  }

  seal() {
    this.#sealed = true;
    return Object.freeze({
      services: Object.freeze([...this.#services.keys()]),
      eventTopics: Object.freeze([...this.#topics.keys()]),
      llmProviders: Object.freeze([...this.#adapters.keys()]),
      llmAdapterInfo: Object.freeze(Object.fromEntries(this.#adapterInfo)),
      tools: Object.freeze([...this.#tools.values()]),
      commands: Object.freeze([...this.#commands.values()]),
      commandHandlers: this.#commandHandlers,
      serviceHandlers: this.#services,
      topicHandlers: this.#topics,
      llmAdapters: this.#adapters,
      toolHandlers: this.#toolHandlers,
      scopeHandlers: Object.freeze([...this.#scopes]),
    });
  }
}

/// Imports a plugin and collects what it registered.
///
/// `activate` is called as `activate(api, config)`: the package says what the
/// plugin can do, the load request's `config` says what this installation wants
/// it to do. The second argument is opaque here — reading it is the plugin's
/// business, and validating it is the plugin's job rather than the wire's.
///
/// `importModule` is injectable so the loader's rules can be tested without a
/// file on disk for every case.
export async function loadPlugin(request, importModule = (url) => import(url)) {
  const url = resolveEntry(request.root, request.entry);
  let module;
  try {
    module = await importModule(url);
  } catch (cause) {
    throw new ProtocolError('[ENTRY_FAILED]', `plugin entry ${request.entry} failed to load: ${cause?.message ?? cause}`);
  }
  const activate = module?.activate ?? module?.default;
  if (typeof activate !== 'function') {
    throw new ProtocolError('[NO_ACTIVATE]', `plugin entry ${request.entry} exports no activate function`);
  }
  const registrar = new Registrar(request.services, request.eventTopics, request.llmProviders, request.tools, request.commands);
  try {
    await activate(registrar.api(), request.config);
  } catch (cause) {
    if (cause instanceof ProtocolError) throw cause;
    throw new ProtocolError('[ACTIVATE_FAILED]', `plugin activation failed: ${cause?.message ?? cause}`);
  }
  return registrar.seal();
}
